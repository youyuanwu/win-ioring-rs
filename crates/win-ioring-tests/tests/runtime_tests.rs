use win_ioring::io_ring::IoRing;
use win_ioring::runtime::Driver;
use win_ioring_tests::README_PATH;

/// The same read, driven by Tokio's local executor.
#[tokio::test(flavor = "current_thread")]
async fn tokio_read_round_trip() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let ring = IoRing::builder().build().unwrap();
            let driver = Driver::new(ring).unwrap();
            let handle = driver.handle();

            let driver_task = tokio::task::spawn_local(async move {
                driver.drive().await;
            });

            let file = win_ioring::file::File::open(README_PATH).unwrap();
            let (read, buffer) = handle
                .read(&file, vec![0_u8; 64], 20, 0)
                .await
                .expect_completed()
                .unwrap();

            assert_eq!(read, 20);
            assert_eq!(buffer.len(), 20, "initialized length tracks the transfer");

            handle.shutdown();
            driver_task.await.unwrap();
        })
        .await;
}

/// The same read, driven by the workspace's own executor. Producing identical
/// results under two unrelated executors is what "runtime agnostic" means.
#[test]
fn custom_runtime_read_round_trip() {
    let mut rt = win_ioring_tests::rt::Runtime::new();
    let spawner = rt.handle();
    rt.block_on(async move {
        let ring = IoRing::builder().build().unwrap();
        let driver = Driver::new(ring).unwrap();
        let handle = driver.handle();

        // The OS signals completions from a thread pool thread, so the waker
        // must be thread-safe even though the driver itself is not.
        let driver_task = spawner.spawn_thread_safe(async move {
            driver.drive().await;
        });

        let file = win_ioring::file::File::open(README_PATH).unwrap();
        let (read, buffer) = handle
            .read(&file, vec![0_u8; 64], 20, 0)
            .await
            .expect_completed()
            .unwrap();

        assert_eq!(read, 20);
        assert_eq!(buffer.len(), 20);

        handle.shutdown();
        driver_task.await.unwrap();
    });
}

/// Dropping a read before it completes must be safe: the buffer belongs to the
/// driver until the operation's own completion arrives.
#[tokio::test(flavor = "current_thread")]
async fn dropping_reads_in_flight_is_safe() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let ring = IoRing::builder().build().unwrap();
            let driver = Driver::new(ring).unwrap();
            let handle = driver.handle();

            let driver_task = tokio::task::spawn_local(async move {
                driver.drive().await;
            });

            let file = win_ioring::file::File::open(README_PATH).unwrap();
            for _ in 0..100 {
                let fut = handle.read(&file, vec![0_u8; 64], 20, 0);
                drop(fut);
                tokio::task::yield_now().await;
            }

            // The ring still works afterwards.
            let (read, _) = handle
                .read(&file, vec![0_u8; 64], 20, 0)
                .await
                .expect_completed()
                .unwrap();
            assert_eq!(read, 20);

            handle.shutdown();
            driver_task.await.unwrap();
        })
        .await;
}

/// A buffer must come back even when the operation is rejected before anything
/// is submitted.
#[tokio::test(flavor = "current_thread")]
async fn rejected_reads_return_the_buffer() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let ring = IoRing::builder().build().unwrap();
            let driver = Driver::new(ring).unwrap();
            let handle = driver.handle();

            let driver_task = tokio::task::spawn_local(async move {
                driver.drive().await;
            });

            let file = win_ioring::file::File::open(README_PATH).unwrap();

            // Asking for more than the buffer can hold is rejected locally.
            let outcome = handle.read(&file, vec![0_u8; 4], 64, 0).await;
            let result = outcome.expect_completed();
            assert!(!result.is_ok());
            let (err, buffer) = result.into_parts();
            assert!(matches!(
                err.unwrap_err(),
                win_ioring::Error::BufferTooSmall { .. }
            ));
            assert_eq!(buffer.capacity(), 4, "the caller's buffer came back");

            handle.shutdown();
            driver_task.await.unwrap();
        })
        .await;
}

/// Submitting after shutdown must report an error rather than panic.
#[tokio::test(flavor = "current_thread")]
async fn submitting_after_shutdown_errors() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let ring = IoRing::builder().build().unwrap();
            let driver = Driver::new(ring).unwrap();
            let handle = driver.handle();

            let driver_task = tokio::task::spawn_local(async move {
                driver.drive().await;
            });

            let file = win_ioring::file::File::open(README_PATH).unwrap();
            handle.shutdown();
            driver_task.await.unwrap();

            let outcome = handle.read(&file, vec![0_u8; 64], 20, 0).await;
            assert!(matches!(
                outcome.err(),
                Some(win_ioring::Error::ShuttingDown)
            ));
        })
        .await;
}

/// The file's handle must outlive the caller's own reference to it when an
/// operation is still using it.
#[tokio::test(flavor = "current_thread")]
async fn a_read_keeps_its_file_alive() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let ring = IoRing::builder().build().unwrap();
            let driver = Driver::new(ring).unwrap();
            let handle = driver.handle();

            let driver_task = tokio::task::spawn_local(async move {
                driver.drive().await;
            });

            let file = win_ioring::file::File::open(README_PATH).unwrap();
            let fut = handle.read(&file, vec![0_u8; 64], 20, 0);

            // The driver holds a reference for the duration of the operation.
            assert!(file.reference_count() >= 2);

            // Drop the caller's own reference while the read is in flight.
            drop(file);

            let (read, _) = fut.await.expect_completed().unwrap();
            assert_eq!(read, 20);

            handle.shutdown();
            driver_task.await.unwrap();
        })
        .await;
}
