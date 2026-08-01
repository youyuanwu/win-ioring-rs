use win_ioring::io_ring::IoRing;
use win_ioring_tests::README_PATH;

#[tokio::test(flavor = "current_thread")]
async fn tokio_readme_test() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let io_ring = IoRing::builder().build().unwrap();
            let driver = win_ioring::runtime::Driver::new(io_ring);
            let mut h_ring = driver.handle();

            let driver = tokio::task::spawn_local(async move {
                // Drive the IO operations
                driver.drive().await;
            });

            let file = win_ioring::file::File::from_std(
                std::fs::File::open(README_PATH).expect("cannot open"),
            );

            let (hr, buffer) = h_ring
                .build_read_file(&file, vec![0_u8; 20], 20, 0)
                .unwrap()
                .await;
            hr.unwrap();
            println!("Read: {:?}", String::from_utf8_lossy(&buffer));

            h_ring.shutdown();
            driver.await.unwrap();
        })
        .await;
}

// Custom runtime test.
#[test]
fn rt_readme_test() {
    let mut rt = win_ioring_tests::rt::Runtime::new();
    let handle = rt.handle();
    rt.block_on(async move {
        let io_ring = IoRing::builder().build().unwrap();
        let driver = win_ioring::runtime::Driver::new(io_ring);
        let mut h_ring = driver.handle();

        // OS needs to call the waker from the other thread.
        // So this must be thread-safe.
        let driver = handle.spawn_thread_safe(async move {
            // Drive the IO operations
            driver.drive().await;
        });

        let file = win_ioring::file::File::from_std(
            std::fs::File::open(README_PATH).expect("cannot open"),
        );

        let (hr, buffer) = h_ring
            .build_read_file(&file, vec![0_u8; 20], 20, 0)
            .unwrap()
            .await;
        hr.unwrap();
        println!("Read: {:?}", String::from_utf8_lossy(&buffer));

        h_ring.shutdown();
        driver.await.unwrap();
    });
}
