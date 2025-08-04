use std::{cell::RefCell, fs::File, rc::Rc};

use crate::event::AsyncEvent;
use crate::io_ring::{ops::ReadOp, BufferInfo, IoRing};

#[test]
fn readme_test() {
    let event = AsyncEvent::new().unwrap();
    let mut ring = IoRing::builder().build().unwrap();
    ring.set_io_ring_completion_event(event.handle()).unwrap();

    println!("ring created");

    // open file from std
    let file = crate::file::File::from_std(File::open("README.md").expect("cannot open"));
    let raw_handle = file.as_raw_handle(); // TODO: fix ownership
    println!("file opened");

    let mut buffer = vec![0; 255];

    let args = ReadOp::builder()
        .with_raw_handle(raw_handle)
        .with_raw_data_address(buffer.as_mut_ptr() as *mut _)
        .with_num_of_bytes_to_read(20) // buffer needs to be bigger
        .with_offset(0)
        .with_user_data(11)
        .build();

    unsafe { ring.build_read_file(args).unwrap() };

    println!("read built");

    let num_entry = ring.submit(0, 0).unwrap();

    println!("Submitted {num_entry} entries");

    event.wait_sync_infinite().unwrap();
    event.reset().unwrap();
    while let Some(cp) = ring.pop_completion() {
        cp.ResultCode.unwrap();
        assert_eq!(cp.UserData, 11);
    }

    ring.close().unwrap();

    println!("ring closed");

    println!("data read: [{}]", String::from_utf8_lossy(&buffer));
}

#[test]
fn readme_register_test() {
    let event = AsyncEvent::new().unwrap();
    let mut ring = IoRing::builder().build().unwrap();
    ring.set_io_ring_completion_event(event.handle()).unwrap();
    println!("ring created");

    // open file from std
    let file = crate::file::File::from_std(File::open("README.md").expect("cannot open"));
    let raw_handle = file.as_raw_handle(); // TODO: fix ownership
    println!("file opened");

    let mut buffer = vec![0; 255];

    unsafe {
        ring.build_register_buffers(&[BufferInfo::raw_from_vec(&mut buffer)], 10)
            .unwrap()
    };

    unsafe { ring.build_register_file_handles(&[raw_handle], 11).unwrap() };

    let op = ReadOp::builder()
        .with_registered_handle_index(0)
        .with_registered_data_index_and_offset(0, 0)
        .with_num_of_bytes_to_read(20) // buffer needs to be bigger
        .with_offset(0)
        .with_user_data(11)
        .build();

    unsafe { ring.build_read_file(op).unwrap() };

    println!("read built");

    let num_entry = ring.submit(0, 0).unwrap();
    println!("Submitted {num_entry} entries");

    // Wait for completion using the event
    event.wait_sync_infinite().unwrap();
    event.reset().unwrap();

    while let Some(cp) = ring.pop_completion() {
        cp.ResultCode.unwrap();
    }
    ring.close().unwrap();

    println!("ring closed");

    println!("data read: [{}]", String::from_utf8_lossy(&buffer));
}

#[tokio::test(flavor = "current_thread")]
async fn readme_test_async() {
    let event = AsyncEvent::new().unwrap();
    let ring = Rc::new(RefCell::new(IoRing::builder().build().unwrap()));
    ring.borrow_mut()
        .set_io_ring_completion_event(event.handle())
        .unwrap();

    println!("ring created");

    // open file from std
    let file = crate::file::File::from_std(File::open("README.md").expect("cannot open"));
    let raw_handle = file.as_raw_handle(); // TODO: fix ownership
    println!("file opened");

    let mut buffer = vec![0_u8; 255];
    unsafe {
        ring.borrow_mut()
            .build_register_buffers(&[BufferInfo::raw_from_vec(&mut buffer)], 0)
            .unwrap()
    };

    unsafe {
        ring.borrow_mut()
            .build_register_file_handles(&[raw_handle], 0)
            .unwrap()
    };

    // drain ops first.
    let num_entry = ring.borrow_mut().submit(2, 5000).unwrap();
    assert_eq!(num_entry, 2);

    let local = tokio::task::LocalSet::new();

    let ring_cp = ring.clone();
    local
        .run_until(async move {
            let ring_cp2 = ring_cp.clone();
            // spawn read task.
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            let t1 = tokio::task::spawn_local(async move {
                let ctx = Box::into_raw(Box::new(tx));
                let op = ReadOp::builder()
                    .with_registered_handle_index(0)
                    .with_registered_data_index_and_offset(0, 0)
                    .with_num_of_bytes_to_read(20) // buffer needs to be bigger
                    .with_offset(0)
                    .with_user_data(ctx as usize)
                    .build();
                unsafe { ring_cp.borrow_mut().build_read_file(op).unwrap() };
                println!("read built");
                rx.await.unwrap();
            });
            let t2 = tokio::task::spawn_local(async move {
                loop {
                    let num_entry = ring_cp2.borrow_mut().submit(0, 0).unwrap();
                    println!("Submitted {num_entry} entries");
                    if num_entry > 0 {
                        break;
                    }
                }

                // Wait for completion using the event
                event.wait().await.unwrap();
                event.reset().unwrap();

                while let Some(cp) = ring_cp2.borrow_mut().pop_completion() {
                    cp.ResultCode.unwrap();
                    let ctx = cp.UserData;
                    if ctx == 0 {
                        continue; // skip if no user data for registering handles.
                    }
                    let ctx = cp.UserData as *mut tokio::sync::oneshot::Sender<()>;
                    let tx = unsafe { Box::from_raw(ctx) };
                    tx.send(()).unwrap();
                    println!("Completion received");
                }
            });
            let (r1, r2) = tokio::join!(t1, t2);
            r1.unwrap();
            r2.unwrap();
        })
        .await;

    ring.borrow_mut().close().unwrap();
    println!("ring closed");

    println!("data read: [{}]", String::from_utf8_lossy(&buffer));
}
