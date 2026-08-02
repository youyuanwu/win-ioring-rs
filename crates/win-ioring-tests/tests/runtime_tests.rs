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

/// A buffer stored inline, rather than behind a heap pointer, is the case that
/// catches a driver taking the buffer's address before moving it into its own
/// storage. A `Vec` would survive that mistake because its heap allocation does
/// not move; `[u8; N]` would not.
#[tokio::test(flavor = "current_thread")]
async fn inline_array_buffers_reach_the_kernel_correctly() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let ring = IoRing::builder().build().unwrap();
            let driver = Driver::new(ring).unwrap();
            let handle = driver.handle();

            let driver_task = tokio::task::spawn_local(async move {
                driver.drive().await;
            });

            let expected = {
                let all = std::fs::read(README_PATH).unwrap();
                all[..20].to_vec()
            };

            let file = win_ioring::file::File::open(README_PATH).unwrap();
            let (read, buffer) = handle
                .read(&file, [0_u8; 64], 20, 0)
                .await
                .expect_completed()
                .unwrap();

            assert_eq!(read, 20);
            assert_eq!(
                &buffer[..20],
                expected.as_slice(),
                "inline buffer did not receive the file's bytes"
            );

            // A boxed slice is the third container shape.
            let boxed: Box<[u8]> = vec![0_u8; 64].into_boxed_slice();
            let (read, buffer) = handle
                .read(&file, boxed, 20, 0)
                .await
                .expect_completed()
                .unwrap();
            assert_eq!(read, 20);
            assert_eq!(&buffer[..20], expected.as_slice());

            handle.shutdown();
            driver_task.await.unwrap();
        })
        .await;
}

/// SC-003: dropping an in-flight read must be safe, repeatedly. This is the
/// property the whole slab and cancellation design exists to guarantee.
#[tokio::test(flavor = "current_thread")]
async fn dropping_in_flight_reads_repeatedly_is_safe() {
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

            for _ in 0..200 {
                let fut = handle.read(&file, vec![0_u8; 128], 64, 0);
                // Give the operation a chance to actually reach the kernel, so
                // the drop exercises the submitted-and-cancellable path rather
                // than only the trivial one.
                tokio::task::yield_now().await;
                drop(fut);
            }

            // Let everything settle, then confirm the ring still works.
            for _ in 0..50 {
                tokio::task::yield_now().await;
            }
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

/// SC-004: dropping a future must return without waiting on the kernel. A drop
/// that blocked would show up as a wall-clock stall here.
#[tokio::test(flavor = "current_thread")]
async fn dropping_an_in_flight_read_does_not_block() {
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
            let fut = handle.read(&file, vec![0_u8; 128], 64, 0);
            tokio::task::yield_now().await;

            let start = std::time::Instant::now();
            drop(fut);
            let elapsed = start.elapsed();

            assert!(
                elapsed < std::time::Duration::from_millis(50),
                "drop took {elapsed:?}, which suggests it waited on the kernel"
            );

            handle.shutdown();
            driver_task.await.unwrap();
        })
        .await;
}

/// Explicit cancellation must not prevent the caller observing the operation's
/// own terminal result, and cancelling twice must be a no-op.
#[tokio::test(flavor = "current_thread")]
async fn explicit_cancellation_still_yields_a_terminal_result() {
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
            let fut = handle.read(&file, vec![0_u8; 128], 64, 0);
            let id = fut.operation_id().expect("read was submitted");

            // Cancelling repeatedly must be harmless.
            handle.cancel(id);
            handle.cancel(id);
            handle.cancel(id);

            // Whether the cancellation wins is a race; either outcome is fine.
            // What must hold is that the future resolves with the buffer back.
            let outcome = fut.await;
            let result = outcome.expect_completed();
            let (_, buffer) = result.into_parts();
            assert_eq!(buffer.capacity(), 128, "the buffer came back");

            handle.shutdown();
            driver_task.await.unwrap();
        })
        .await;
}

/// Cancelling something that has already finished, or that never existed, must
/// be a no-op rather than corrupting the driver.
#[tokio::test(flavor = "current_thread")]
async fn cancelling_a_finished_operation_is_harmless() {
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
            let id = fut.operation_id().expect("read was submitted");

            let (read, _) = fut.await.expect_completed().unwrap();
            assert_eq!(read, 20);

            // The operation is long gone; this must simply do nothing.
            handle.cancel(id);
            handle.cancel(id);

            // The ring is still usable.
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

/// SC-013: shutting down with work in flight must not hang or panic, and must
/// resolve whatever futures are still waiting rather than stranding them.
#[tokio::test(flavor = "current_thread")]
async fn shutdown_with_work_in_flight_settles() {
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
            let futures: Vec<_> = (0..8)
                .map(|_| handle.read(&file, vec![0_u8; 128], 64, 0))
                .collect();

            handle.shutdown();
            driver_task.await.unwrap();

            // Every future must resolve one way or another; none may hang.
            for fut in futures {
                let outcome = fut.await;
                // Either it completed normally, or its buffer was retained
                // because the kernel could still reach it. Both are terminal.
                assert!(
                    outcome.is_ok() || outcome.err().is_some(),
                    "a future neither completed nor reported"
                );
            }
        })
        .await;
}

/// FR-042: dropping the driver outright must resolve surviving futures rather
/// than leaving them pending forever.
#[tokio::test(flavor = "current_thread")]
async fn dropping_the_driver_resolves_surviving_futures() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let ring = IoRing::builder().build().unwrap();
            let driver = Driver::new(ring).unwrap();
            let handle = driver.handle();

            let file = win_ioring::file::File::open(README_PATH).unwrap();
            let fut = handle.read(&file, vec![0_u8; 128], 64, 0);

            // No driver task was ever spawned, so nothing has been reaped. Drop
            // the driver with the operation still outstanding. Teardown drains
            // with a bounded wait, so the read may well complete normally; what
            // matters is that the future reaches a terminal state either way
            // rather than hanging.
            drop(driver);

            let outcome = fut.await;
            match outcome {
                win_ioring::runtime::Outcome::Completed(result) => {
                    let (_, buffer) = result.into_parts();
                    assert_eq!(buffer.capacity(), 128, "the buffer came back");
                }
                win_ioring::runtime::Outcome::Retained(_) => {
                    // The kernel could still reach the buffer, so it was
                    // abandoned. Also terminal, and also not a hang.
                }
            }

            // The handle still works well enough to report the state.
            assert!(handle.is_shutting_down());
        })
        .await;
}
