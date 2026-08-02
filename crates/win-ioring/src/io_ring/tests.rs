use std::{cell::RefCell, fs::File, rc::Rc};

use crate::io_ring::{BufferInfo, IoRing, ops::ReadOp};
use crate::sys::AsyncEvent;

const SAMPLE_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/sample.txt");

#[test]
fn readme_test() {
    let event = AsyncEvent::new().unwrap();
    let mut ring = IoRing::builder().build().unwrap();
    // SAFETY: `event` is declared before `ring`, so it is dropped after it
    // and outlives the ring's use of it.
    unsafe { ring.set_io_ring_completion_event(event.handle()) }.unwrap();

    println!("ring created");

    // open file from std
    let file = crate::file::File::from_std(File::open(SAMPLE_PATH).expect("cannot open"));
    let raw_handle = file.as_raw_handle(); // TODO: fix ownership
    println!("file opened");

    let mut buffer = vec![0; 255];

    let args = ReadOp::builder()
        .with_raw_handle(raw_handle)
        .with_raw_data_address(buffer.as_mut_ptr() as *mut _)
        .with_num_of_bytes_to_read(20) // buffer needs to be bigger
        .with_offset(0)
        .with_user_data(11)
        .build()
        .unwrap();

    unsafe { ring.build_read_file(args).unwrap() };

    println!("read built");

    let num_entry = ring.submit(0, 0).unwrap();

    println!("Submitted {num_entry} entries");

    event.wait_sync_infinite().unwrap();
    event.reset().unwrap();
    while let Some(cp) = ring.pop_completion().unwrap() {
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
    // SAFETY: `event` is declared before `ring`, so it is dropped after it
    // and outlives the ring's use of it.
    unsafe { ring.set_io_ring_completion_event(event.handle()) }.unwrap();
    println!("ring created");

    // open file from std
    let file = crate::file::File::from_std(File::open(SAMPLE_PATH).expect("cannot open"));
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
        .build()
        .unwrap();

    unsafe { ring.build_read_file(op).unwrap() };

    println!("read built");

    let num_entry = ring.submit(0, 0).unwrap();
    println!("Submitted {num_entry} entries");

    // Wait for completion using the event
    event.wait_sync_infinite().unwrap();
    event.reset().unwrap();

    while let Some(cp) = ring.pop_completion().unwrap() {
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
    // SAFETY: `event` is declared before `ring`, so it is dropped after it and
    // outlives the ring's use of it.
    unsafe {
        ring.borrow_mut()
            .set_io_ring_completion_event(event.handle())
            .unwrap();
    }

    println!("ring created");

    // open file from std
    let file = crate::file::File::from_std(File::open(SAMPLE_PATH).expect("cannot open"));
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
            let (tx, rx) = futures::channel::oneshot::channel::<()>();
            let t1 = tokio::task::spawn_local(async move {
                let ctx = Box::into_raw(Box::new(tx));
                let op = ReadOp::builder()
                    .with_registered_handle_index(0)
                    .with_registered_data_index_and_offset(0, 0)
                    .with_num_of_bytes_to_read(20) // buffer needs to be bigger
                    .with_offset(0)
                    .with_user_data(ctx as usize)
                    .build()
                    .unwrap();
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

                while let Some(cp) = ring_cp2.borrow_mut().pop_completion().unwrap() {
                    cp.ResultCode.unwrap();
                    let ctx = cp.UserData;
                    if ctx == 0 {
                        continue; // skip if no user data for registering handles.
                    }
                    let ctx = cp.UserData as *mut futures::channel::oneshot::Sender<()>;
                    let tx = unsafe { Box::from_raw(ctx) };
                    tx.send(()).unwrap();
                    println!("Completion received");
                }
            });
            let (r1, r2) = futures::join!(t1, t2);
            r1.unwrap();
            r2.unwrap();
        })
        .await;

    ring.borrow_mut().close().unwrap();
    println!("ring closed");

    println!("data read: [{}]", String::from_utf8_lossy(&buffer));
}

// ---------------------------------------------------------------------------
// Phase 1: capabilities, introspection, and the operations added to the raw
// layer (write, flush, cancel).
// ---------------------------------------------------------------------------

use crate::error::Error;
use crate::io_ring::ops::{CancelOp, FlushOp, WriteOp};
use windows::Win32::Storage::FileSystem::{
    FILE_FLUSH_DEFAULT, FILE_WRITE_FLAGS_NONE, IORING_FEATURE_SET_COMPLETION_EVENT,
    IORING_OP_CANCEL, IORING_OP_FLUSH, IORING_OP_NOP, IORING_OP_READ, IORING_OP_REGISTER_BUFFERS,
    IORING_OP_REGISTER_FILES, IORING_OP_WRITE, IORING_VERSION,
};

/// Every operation code this crate cares about, for support queries.
const ALL_OPS: &[(&str, windows::Win32::Storage::FileSystem::IORING_OP_CODE)] = &[
    ("NOP", IORING_OP_NOP),
    ("READ", IORING_OP_READ),
    ("REGISTER_FILES", IORING_OP_REGISTER_FILES),
    ("REGISTER_BUFFERS", IORING_OP_REGISTER_BUFFERS),
    ("CANCEL", IORING_OP_CANCEL),
    ("WRITE", IORING_OP_WRITE),
    ("FLUSH", IORING_OP_FLUSH),
];

fn temp_path(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "win-ioring-test-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    p
}

#[test]
fn capabilities_are_reported() {
    let caps = IoRing::query_io_ring_capabilities().unwrap();
    assert!(caps.max_version.0 > 0, "no usable version reported");
    assert!(caps.max_submission_queue_size > 0);
    assert!(caps.max_completion_queue_size > 0);
}

/// A ring must be creatable at the exact version the host advertises as its
/// ceiling. This also guards against treating any named constant as the
/// maximum: hosts report versions with no corresponding constant.
#[test]
fn ring_can_be_created_at_reported_max_version() {
    let caps = IoRing::query_io_ring_capabilities().unwrap();
    let mut ring = IoRing::builder()
        .with_version(caps.max_version)
        .build()
        .unwrap();
    assert_eq!(ring.info().unwrap().version.0, caps.max_version.0);
    ring.close().unwrap();
}

#[test]
fn requesting_a_version_above_the_ceiling_is_a_distinct_error() {
    let caps = IoRing::query_io_ring_capabilities().unwrap();
    let err = IoRing::builder()
        .with_version(IORING_VERSION(caps.max_version.0 + 1))
        .build()
        .err()
        .unwrap();
    match err {
        Error::UnsupportedVersion {
            requested,
            max_supported,
        } => {
            assert_eq!(requested, caps.max_version.0 + 1);
            assert_eq!(max_supported, caps.max_version.0);
        }
        other => panic!("expected UnsupportedVersion, got {other:?}"),
    }
}

#[test]
fn requiring_an_absent_feature_is_a_distinct_error() {
    // Bit 30 is not a feature any host reports, so this must always fail.
    let bogus = windows::Win32::Storage::FileSystem::IORING_FEATURE_FLAGS(1 << 30);
    let err = IoRing::builder()
        .with_required_features(bogus)
        .build()
        .err()
        .unwrap();
    assert!(matches!(err, Error::UnsupportedFeature { .. }));
}

/// The driver depends on completion-event signalling, so requiring it must
/// succeed on any host this crate can actually run on.
#[test]
fn required_completion_event_feature_is_available() {
    let mut ring = IoRing::builder()
        .with_required_features(IORING_FEATURE_SET_COMPLETION_EVENT)
        .build()
        .unwrap();
    ring.close().unwrap();
}

#[test]
fn op_support_can_be_queried_without_submitting() {
    let mut ring = IoRing::builder().build().unwrap();

    // Every operation this crate builds on must be supported, otherwise the
    // safe layer cannot function on this host at all.
    for (name, op) in ALL_OPS {
        let supported = ring.is_op_supported(*op);
        if *op == IORING_OP_NOP {
            // NOP has no Build* entry point in the bindings, so the crate never
            // issues it; its support is informational only.
            println!("op {name} supported = {supported} (informational)");
            continue;
        }
        assert!(
            supported,
            "op {name} is required by this crate but is unsupported on this host"
        );
        ring.ensure_op_supported(*op)
            .unwrap_or_else(|e| panic!("op {name} failed the support check: {e}"));
    }

    ring.close().unwrap();
}

/// The platform rounds queue sizes up to a power of two, so the allocated size
/// is generally larger than the requested one. Anything that needs the real
/// capacity must read it from `GetIoRingInfo`.
#[test]
fn ring_info_reports_allocated_queue_sizes() {
    let mut ring = IoRing::builder()
        .with_submission_queue_size(20)
        .with_completion_queue_size(20)
        .build()
        .unwrap();
    let info = ring.info().unwrap();
    assert!(
        info.submission_queue_size >= 20,
        "allocated submission queue {} smaller than requested",
        info.submission_queue_size
    );
    assert!(info.completion_queue_size >= 20);
    ring.close().unwrap();
}

#[test]
fn close_is_idempotent() {
    let mut ring = IoRing::builder().build().unwrap();
    assert!(!ring.is_closed());
    ring.close().unwrap();
    assert!(ring.is_closed());
    // A second close must not attempt to close the underlying ring again.
    ring.close().unwrap();
}

/// Writes the given bytes through the ring, then flushes, then reads them back
/// through the ring. Exercises the write and flush entry points end to end.
#[test]
fn write_then_flush_then_read_round_trip() {
    let path = temp_path("rw");
    let payload = b"io_ring round trip payload";

    let event = AsyncEvent::new().unwrap();
    let mut ring = IoRing::builder().build().unwrap();
    // SAFETY: `event` is declared before `ring`, so it is dropped after it
    // and outlives the ring's use of it.
    unsafe { ring.set_io_ring_completion_event(event.handle()) }.unwrap();

    let write_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&path)
        .unwrap();
    let write_handle = crate::file::File::from_std(write_file);

    let mut source = payload.to_vec();
    let op = WriteOp::builder()
        .with_raw_handle(write_handle.as_raw_handle())
        .with_raw_data_address(source.as_mut_ptr() as *mut _)
        .with_num_of_bytes_to_write(source.len() as u32)
        .with_offset(0)
        .with_write_flags(FILE_WRITE_FLAGS_NONE)
        .with_user_data(1)
        .build()
        .unwrap();
    unsafe { ring.build_write_file(op).unwrap() };
    ring.submit(1, 5000).unwrap();
    event.wait_sync_infinite().unwrap();
    event.reset().unwrap();
    let cqe = ring.pop_completion().unwrap().expect("write completion");
    cqe.ResultCode.unwrap();
    assert_eq!(cqe.UserData, 1);
    assert_eq!(cqe.Information, source.len());

    let flush = FlushOp::builder()
        .with_raw_handle(write_handle.as_raw_handle())
        .with_flush_mode(FILE_FLUSH_DEFAULT)
        .with_user_data(2)
        .build()
        .unwrap();
    unsafe { ring.build_flush_file(flush).unwrap() };
    ring.submit(1, 5000).unwrap();
    event.wait_sync_infinite().unwrap();
    event.reset().unwrap();
    let cqe = ring.pop_completion().unwrap().expect("flush completion");
    cqe.ResultCode.unwrap();
    assert_eq!(cqe.UserData, 2);

    drop(write_handle);

    let read_file = crate::file::File::from_std(File::open(&path).unwrap());
    let mut dest = vec![0_u8; payload.len()];
    let op = ReadOp::builder()
        .with_raw_handle(read_file.as_raw_handle())
        .with_raw_data_address(dest.as_mut_ptr() as *mut _)
        .with_num_of_bytes_to_read(dest.len() as u32)
        .with_offset(0)
        .with_user_data(3)
        .build()
        .unwrap();
    unsafe { ring.build_read_file(op).unwrap() };
    ring.submit(1, 5000).unwrap();
    event.wait_sync_infinite().unwrap();
    event.reset().unwrap();
    let cqe = ring.pop_completion().unwrap().expect("read completion");
    cqe.ResultCode.unwrap();
    assert_eq!(cqe.UserData, 3);

    assert_eq!(&dest, payload);

    ring.close().unwrap();
    drop(read_file);
    let _ = std::fs::remove_file(&path);
}

/// A cancellation request produces its own completion, carrying its own user
/// data rather than the target's. Whether the cancellation actually beats the
/// operation is a race, so this asserts only on the correlation mechanics.
#[test]
fn cancel_request_completes_with_its_own_user_data() {
    let path = temp_path("cancel");
    std::fs::write(&path, b"some bytes to read").unwrap();

    let event = AsyncEvent::new().unwrap();
    let mut ring = IoRing::builder().build().unwrap();
    // SAFETY: `event` is declared before `ring`, so it is dropped after it
    // and outlives the ring's use of it.
    unsafe { ring.set_io_ring_completion_event(event.handle()) }.unwrap();

    let file = crate::file::File::from_std(File::open(&path).unwrap());
    let mut dest = vec![0_u8; 8];

    const TARGET_USER_DATA: usize = 0xAAAA;
    const CANCEL_USER_DATA: usize = 0xBBBB;

    let read = ReadOp::builder()
        .with_raw_handle(file.as_raw_handle())
        .with_raw_data_address(dest.as_mut_ptr() as *mut _)
        .with_num_of_bytes_to_read(dest.len() as u32)
        .with_offset(0)
        .with_user_data(TARGET_USER_DATA)
        .build()
        .unwrap();
    unsafe { ring.build_read_file(read).unwrap() };

    let cancel = CancelOp::builder()
        .with_raw_handle(file.as_raw_handle())
        .with_op_to_cancel(TARGET_USER_DATA)
        .with_user_data(CANCEL_USER_DATA)
        .build()
        .unwrap();
    unsafe { ring.build_cancel_request(cancel).unwrap() };

    ring.submit(2, 5000).unwrap();

    let mut seen = Vec::new();
    while seen.len() < 2 {
        event.wait_sync_infinite().unwrap();
        event.reset().unwrap();
        while let Some(cqe) = ring.pop_completion().unwrap() {
            seen.push((cqe.UserData, cqe.ResultCode));
        }
    }

    let target = seen
        .iter()
        .find(|(ud, _)| *ud == TARGET_USER_DATA)
        .expect("target completion missing");
    let cancel_cqe = seen
        .iter()
        .find(|(ud, _)| *ud == CANCEL_USER_DATA)
        .expect("cancel completion missing");

    // Whether the cancellation wins the race is not deterministic, so the
    // target may have succeeded or been aborted. Both are acceptable; what must
    // hold is that the two completions are distinguishable and that neither
    // reports an unexpected failure mode.
    let target_ok = target.1.is_ok();
    let target_aborted = target.1 == windows::Win32::Foundation::E_ABORT;
    assert!(
        target_ok || target_aborted,
        "target completed with an unexpected code: {:?}",
        target.1
    );

    // The cancellation request itself either found its target or reported that
    // there was nothing left to cancel; it must not fail in any other way.
    let cancel_ok = cancel_cqe.1.is_ok();
    let not_found =
        cancel_cqe.1 == windows::core::HRESULT::from(windows::Win32::Foundation::ERROR_NOT_FOUND);
    assert!(
        cancel_ok || not_found,
        "cancel completed with an unexpected code: {:?}",
        cancel_cqe.1
    );

    ring.close().unwrap();
    drop(file);
    let _ = std::fs::remove_file(&path);
}

/// The platform has no unregister entry point, and it also rejects a\r
/// zero-length registration, so a registration cannot be released except by\r
/// replacing it or closing the ring.
#[test]
fn empty_registration_is_rejected_by_the_platform() {
    let event = AsyncEvent::new().unwrap();
    let mut ring = IoRing::builder().build().unwrap();
    // SAFETY: `event` is declared before `ring`, so it is dropped after it
    // and outlives the ring's use of it.
    unsafe { ring.set_io_ring_completion_event(event.handle()) }.unwrap();

    let mut buffer = vec![0_u8; 64];
    unsafe {
        ring.build_register_buffers(&[BufferInfo::raw_from_vec(&mut buffer)], 1)
            .unwrap()
    };
    ring.submit(1, 5000).unwrap();
    event.wait_sync_infinite().unwrap();
    event.reset().unwrap();
    ring.pop_completion()
        .unwrap()
        .expect("register completion")
        .ResultCode
        .unwrap();

    // A zero-length registration is accepted by the builder but fails at
    // completion time, so there is no "unregister by empty set" mechanism. A
    // registration is released by replacing it or by closing the ring.
    unsafe { ring.build_register_buffers(&[], 2).unwrap() };
    ring.submit(1, 5000).unwrap();
    event.wait_sync_infinite().unwrap();
    event.reset().unwrap();
    let cqe = ring
        .pop_completion()
        .unwrap()
        .expect("empty registration completion");
    assert_eq!(cqe.UserData, 2);
    assert_eq!(
        cqe.ResultCode,
        windows::Win32::Foundation::E_INVALIDARG,
        "expected an empty registration to fail at completion"
    );

    // Replacing a registration with a different non-empty set does work.
    let mut replacement = vec![0_u8; 32];
    unsafe {
        ring.build_register_buffers(&[BufferInfo::raw_from_vec(&mut replacement)], 3)
            .unwrap()
    };
    ring.submit(1, 5000).unwrap();
    event.wait_sync_infinite().unwrap();
    event.reset().unwrap();
    let cqe = ring
        .pop_completion()
        .unwrap()
        .expect("re-register completion");
    cqe.ResultCode.unwrap();
    assert_eq!(cqe.UserData, 3);

    ring.close().unwrap();
}

/// SC-002: every IoRing entry point the platform bindings expose has a wrapper
/// in this layer. This is a compile-time enumeration: if a wrapper is removed
/// or renamed, this fails to build.
#[test]
fn all_fourteen_entry_points_have_wrappers() {
    #[allow(unused_imports)]
    use crate::io_ring::ops::{
        CancelOp, FlushOp, ReadOp, RegisterBuffersOp, RegisterFilesOp, WriteOp,
    };

    // Associated-function pointers, one per platform entry point.
    let _create: fn(IORING_VERSION, u32, u32) -> crate::Result<IoRing> = IoRing::create;
    let _close: fn(&mut IoRing) -> crate::Result<()> = IoRing::close;
    let _submit: fn(&mut IoRing, usize, usize) -> crate::Result<u32> = IoRing::submit;
    let _pop: fn(
        &mut IoRing,
    ) -> crate::Result<Option<windows::Win32::Storage::FileSystem::IORING_CQE>> =
        IoRing::pop_completion;
    let _caps: fn() -> crate::Result<crate::io_ring::Capabilities> =
        IoRing::query_io_ring_capabilities;
    let _info: fn(&IoRing) -> crate::Result<crate::io_ring::RingInfo> = IoRing::info;
    let _is_supported: fn(&IoRing, windows::Win32::Storage::FileSystem::IORING_OP_CODE) -> bool =
        IoRing::is_op_supported;
    let _set_event: unsafe fn(
        &mut IoRing,
        windows::Win32::Foundation::HANDLE,
    ) -> crate::Result<()> = IoRing::set_io_ring_completion_event;
    let _read: unsafe fn(&mut IoRing, ReadOp) -> crate::Result<()> = IoRing::build_read_file;
    let _write: unsafe fn(&mut IoRing, WriteOp) -> crate::Result<()> = IoRing::build_write_file;
    let _flush: unsafe fn(&mut IoRing, FlushOp) -> crate::Result<()> = IoRing::build_flush_file;
    let _cancel: unsafe fn(&mut IoRing, CancelOp) -> crate::Result<()> =
        IoRing::build_cancel_request;
    let _reg_bufs: unsafe fn(&mut IoRing, &[BufferInfo], usize) -> crate::Result<()> =
        IoRing::build_register_buffers;
    let _reg_files: unsafe fn(
        &mut IoRing,
        &[windows::Win32::Foundation::HANDLE],
        usize,
    ) -> crate::Result<()> = IoRing::build_register_file_handles;
}

/// A full submission queue must surface as the dedicated `QueueFull` variant,
/// not as an opaque platform error. The real capacity comes from
/// `GetIoRingInfo`, because the platform rounds the requested size up.
#[test]
fn full_submission_queue_reports_queue_full() {
    let event = AsyncEvent::new().unwrap();
    let mut ring = IoRing::builder()
        .with_submission_queue_size(2)
        .with_completion_queue_size(2)
        .build()
        .unwrap();
    // SAFETY: `event` is declared before `ring`, so it is dropped after it
    // and outlives the ring's use of it.
    unsafe { ring.set_io_ring_completion_event(event.handle()) }.unwrap();
    let capacity = ring.info().unwrap().submission_queue_size;

    let file = crate::file::File::from_std(File::open(SAMPLE_PATH).unwrap());
    let mut buffer = vec![0_u8; 16];

    let mut queue_full = None;
    let mut queued = 0_u32;
    for i in 0..(capacity + 4) {
        let op = ReadOp::builder()
            .with_raw_handle(file.as_raw_handle())
            .with_raw_data_address(buffer.as_mut_ptr() as *mut _)
            .with_num_of_bytes_to_read(8)
            .with_offset(0)
            .with_user_data(i as usize)
            .build()
            .unwrap();
        match unsafe { ring.build_read_file(op) } {
            Ok(()) => queued += 1,
            Err(e) => {
                queue_full = Some((i, e));
                break;
            }
        }
    }

    let (filled_at, err) = queue_full.expect("submission queue never reported full");
    assert!(
        matches!(err, Error::QueueFull),
        "expected QueueFull, got {err:?}"
    );
    assert_eq!(
        filled_at, capacity,
        "queue reported full at a different depth than GetIoRingInfo advertised"
    );

    // Every queued entry references `buffer` and `file`, and closing the ring
    // does not withdraw them. Drain all completions before anything is dropped.
    let mut completed = 0_u32;
    while completed < queued {
        ring.submit(0, 0).unwrap();
        event.wait_sync_infinite().unwrap();
        event.reset().unwrap();
        while let Some(cqe) = ring.pop_completion().unwrap() {
            let _ = cqe.ResultCode;
            completed += 1;
        }
    }

    ring.close().unwrap();
    drop(buffer);
    drop(file);
}

/// An operation the host does not support must produce the dedicated
/// `UnsupportedOp` variant rather than a raw platform error.
#[test]
fn unsupported_op_is_a_distinct_error() {
    let mut ring = IoRing::builder().build().unwrap();

    // A code the platform does not define. `ensure_op_supported` must classify
    // it rather than surfacing an opaque failure.
    let bogus = windows::Win32::Storage::FileSystem::IORING_OP_CODE(0x7FFF_FFFF);
    let err = ring.ensure_op_supported(bogus).err().unwrap();
    assert!(matches!(err, Error::UnsupportedOp { op: 0x7FFF_FFFF }));

    // A supported op must pass the same check.
    ring.ensure_op_supported(IORING_OP_READ).unwrap();

    ring.close().unwrap();
}

/// Ring creation with an absurd queue size is an argument error, not a claim
/// that the host lacks IoRing support.
#[test]
fn bad_queue_size_is_not_reported_as_unsupported() {
    let caps = IoRing::query_io_ring_capabilities().unwrap();
    let err = IoRing::create(caps.max_version, u32::MAX, u32::MAX)
        .err()
        .expect("an absurd queue size should fail");
    assert!(
        !matches!(err, Error::Unsupported),
        "argument failure must not be reported as an unsupported host: {err:?}"
    );
}
