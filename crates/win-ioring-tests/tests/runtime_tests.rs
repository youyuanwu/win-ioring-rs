use win_ioring::io_ring::IoRing;
use win_ioring::runtime::Driver;
use win_ioring_tests::SAMPLE_PATH;
use win_ioring_tests::temp::{TempFile, temp_path};

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

            let file = win_ioring::file::File::open(SAMPLE_PATH).unwrap();
            let (read, buffer) = handle.read(&file, vec![0_u8; 64], 20, 0).await.unwrap();

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

        let file = win_ioring::file::File::open(SAMPLE_PATH).unwrap();
        let (read, buffer) = handle.read(&file, vec![0_u8; 64], 20, 0).await.unwrap();

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

            let file = win_ioring::file::File::open(SAMPLE_PATH).unwrap();
            for _ in 0..100 {
                let fut = handle.read(&file, vec![0_u8; 64], 20, 0);
                drop(fut);
                tokio::task::yield_now().await;
            }

            // Wait for the ring to drain before asking for room: with the whole
            // workspace's tests running at once the driver can be starved long
            // enough that the submission queue is still full here.
            win_ioring_tests::settle(&handle).await;

            // The ring still works afterwards.
            let (read, _) = handle.read(&file, vec![0_u8; 64], 20, 0).await.unwrap();
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

            let file = win_ioring::file::File::open(SAMPLE_PATH).unwrap();

            // Asking for more than the buffer can hold is rejected locally.
            let outcome = handle.read(&file, vec![0_u8; 4], 64, 0).await;
            let result = outcome;
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

            let file = win_ioring::file::File::open(SAMPLE_PATH).unwrap();
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

            let file = win_ioring::file::File::open(SAMPLE_PATH).unwrap();
            let fut = handle.read(&file, vec![0_u8; 64], 20, 0);

            // The driver holds a reference for the duration of the operation.
            assert!(file.reference_count() >= 2);

            // Drop the caller's own reference while the read is in flight.
            drop(file);

            let (read, _) = fut.await.unwrap();
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
                let all = std::fs::read(SAMPLE_PATH).unwrap();
                all[..20].to_vec()
            };

            let file = win_ioring::file::File::open(SAMPLE_PATH).unwrap();
            let (read, buffer) = handle.read(&file, [0_u8; 64], 20, 0).await.unwrap();

            assert_eq!(read, 20);
            assert_eq!(
                &buffer[..20],
                expected.as_slice(),
                "inline buffer did not receive the file's bytes"
            );

            // A boxed slice is the third container shape.
            let boxed: Box<[u8]> = vec![0_u8; 64].into_boxed_slice();
            let (read, buffer) = handle.read(&file, boxed, 20, 0).await.unwrap();
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

            let file = win_ioring::file::File::open(SAMPLE_PATH).unwrap();

            for _ in 0..200 {
                let fut = handle.read(&file, vec![0_u8; 128], 64, 0);
                // Yield so the operation has a chance to reach the kernel,
                // mixing built-drop and submitted-drop paths under a real
                // executor. Which one a given iteration takes is genuinely
                // racy; the deterministic proof that each path behaves lives in
                // the crate's own unit tests, which can observe the lifecycle.
                tokio::task::yield_now().await;
                drop(fut);
            }

            // Let everything settle, then confirm the ring still works.
            win_ioring_tests::settle(&handle).await;
            let (read, _) = handle.read(&file, vec![0_u8; 64], 20, 0).await.unwrap();
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

            let file = win_ioring::file::File::open(SAMPLE_PATH).unwrap();
            let fut = handle.read(&file, vec![0_u8; 128], 64, 0);
            // Yield once so the operation has a chance to reach the kernel.
            // Whether it is still outstanding by the time we drop is genuinely
            // racy — a small local read can complete immediately — so this
            // asserts the property that holds either way: neither the built nor
            // the submitted drop path may wait on the kernel. The
            // crate's own unit tests cover each drop path deterministically,
            // since only they can observe the operation's lifecycle.
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

            let file = win_ioring::file::File::open(SAMPLE_PATH).unwrap();
            let fut = handle.read(&file, vec![0_u8; 128], 64, 0);
            let id = fut.operation_id().expect("read was submitted");

            // Cancelling repeatedly must be harmless.
            handle.cancel(id);
            handle.cancel(id);
            handle.cancel(id);

            // Whether the cancellation wins is a race; either outcome is fine.
            // What must hold is that the future resolves with the buffer back.
            let outcome = fut.await;
            let result = outcome;
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

            let file = win_ioring::file::File::open(SAMPLE_PATH).unwrap();
            let fut = handle.read(&file, vec![0_u8; 64], 20, 0);
            let id = fut.operation_id().expect("read was submitted");

            let (read, _) = fut.await.unwrap();
            assert_eq!(read, 20);

            // The operation is long gone; this must simply do nothing.
            handle.cancel(id);
            handle.cancel(id);

            // The ring is still usable.
            let (read, _) = handle.read(&file, vec![0_u8; 64], 20, 0).await.unwrap();
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

            let file = win_ioring::file::File::open(SAMPLE_PATH).unwrap();
            let futures: Vec<_> = (0..8)
                .map(|_| handle.read(&file, vec![0_u8; 128], 64, 0))
                .collect();

            handle.shutdown();
            driver_task.await.unwrap();

            // Every future must reach a terminal state, and every buffer must
            // come back. Teardown drains to quiescence, so there is no longer an
            // "abandoned" case to tolerate.
            let mut completed = 0;
            for fut in futures {
                let (_, buffer) = fut.await.into_parts();
                assert_eq!(buffer.capacity(), 128, "the buffer came back");
                completed += 1;
            }
            assert_eq!(completed, 8, "every future must resolve");

            // FR-032: nothing may be submitted once the driver is gone.
            let outcome = handle.read(&file, vec![0_u8; 64], 20, 0).await;
            assert!(matches!(
                outcome.err(),
                Some(win_ioring::Error::ShuttingDown)
            ));
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

            let file = win_ioring::file::File::open(SAMPLE_PATH).unwrap();
            let fut = handle.read(&file, vec![0_u8; 128], 64, 0);

            // No driver task was ever spawned, so nothing has been reaped. Drop
            // the driver with the operation still outstanding. Teardown now
            // drains to quiescence, so the future must resolve *and* the buffer
            // must come back — not merely reach some terminal state.
            drop(driver);

            let (_, buffer) = fut.await.into_parts();
            assert_eq!(buffer.capacity(), 128, "the buffer came back");

            // The handle still works well enough to report the state.
            assert!(handle.is_shutting_down());
        })
        .await;
}

/// Write, flush, then read back through the safe API.
#[tokio::test(flavor = "current_thread")]
async fn write_flush_read_round_trip() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let ring = IoRing::builder().build().unwrap();
            let driver = Driver::new(ring).unwrap();
            let handle = driver.handle();
            let driver_task = tokio::task::spawn_local(async move { driver.drive().await });

            let path = temp_path("roundtrip");
            let payload = b"safe layer round trip".to_vec();
            let expected = payload.clone();

            let out = win_ioring::file::File::create(&path).unwrap();
            let (written, buffer) = handle
                .write(&out, payload, expected.len() as u32, 0)
                .await
                .unwrap();
            assert_eq!(written as usize, expected.len());
            assert_eq!(buffer, expected, "the buffer came back unchanged");

            handle.flush(&out).await.unwrap();
            drop(out);

            let input = win_ioring::file::File::open(&path).unwrap();
            let (read, buffer) = handle
                .read(&input, vec![0_u8; 64], expected.len() as u32, 0)
                .await
                .unwrap();
            assert_eq!(read as usize, expected.len());
            assert_eq!(buffer, expected);

            handle.shutdown();
            driver_task.await.unwrap();
            drop(input);
            let _ = std::fs::remove_file(&path);
        })
        .await;
}

/// A write must never source bytes the caller has not initialized, even when
/// they are within the allocation's capacity.
#[tokio::test(flavor = "current_thread")]
async fn writes_past_initialized_bytes_are_rejected() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let ring = IoRing::builder().build().unwrap();
            let driver = Driver::new(ring).unwrap();
            let handle = driver.handle();
            let driver_task = tokio::task::spawn_local(async move { driver.drive().await });

            let path = temp_path("uninit");
            let out = win_ioring::file::File::create(&path).unwrap();

            // Plenty of capacity, but only three initialized bytes.
            let mut buffer: Vec<u8> = Vec::with_capacity(64);
            buffer.extend_from_slice(b"abc");

            let outcome = handle.write(&out, buffer, 32, 0).await;
            let result = outcome;
            let (err, buffer) = result.into_parts();
            match err.unwrap_err() {
                win_ioring::Error::UninitializedWriteRange {
                    requested,
                    initialized,
                } => {
                    assert_eq!(requested, 32);
                    assert_eq!(initialized, 3);
                }
                other => panic!("expected UninitializedWriteRange, got {other:?}"),
            }
            assert_eq!(buffer, b"abc", "the caller's buffer came back");

            handle.shutdown();
            driver_task.await.unwrap();
            drop(out);
            let _ = std::fs::remove_file(&path);
        })
        .await;
}

/// The platform's write flags must reach the kernel, and its verdict must come
/// back cleanly with the caller's buffer.
///
/// Write-through is rejected outright for a file opened for cached I/O, which
/// is what `File::create` gives you. That makes it a good test of the flag
/// actually being plumbed through: if the flag were dropped on the floor the
/// write would simply succeed.
#[tokio::test(flavor = "current_thread")]
async fn write_flags_reach_the_platform() {
    use windows::Win32::Storage::FileSystem::FILE_WRITE_FLAGS_WRITE_THROUGH;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let ring = IoRing::builder().build().unwrap();
            let driver = Driver::new(ring).unwrap();
            let handle = driver.handle();
            let driver_task = tokio::task::spawn_local(async move { driver.drive().await });

            let path = temp_path("writethrough");
            let out = win_ioring::file::File::create(&path).unwrap();
            let payload = b"durable".to_vec();

            // Same write, with and without the flag.
            let (written, payload) = handle.write(&out, payload, 7, 0).await.unwrap();
            assert_eq!(written, 7, "the unflagged write should succeed");

            let outcome = handle
                .write_with_options(
                    &out,
                    payload,
                    7,
                    0,
                    FILE_WRITE_FLAGS_WRITE_THROUGH,
                    win_ioring::io_ring::ops::SqeFlags::NONE,
                )
                .await;
            let result = outcome;
            let (err, buffer) = result.into_parts();
            let err = err.expect_err("write-through on cached I/O should be refused");
            assert!(
                matches!(err, win_ioring::Error::Os(_)),
                "expected the platform's own error, got {err:?}"
            );
            assert_eq!(
                buffer, b"durable",
                "the buffer must come back even when the operation fails"
            );

            handle.shutdown();
            driver_task.await.unwrap();
            drop(out);
            let _ = std::fs::remove_file(&path);
        })
        .await;
}

/// Every flush mode the platform defines must be selectable.
#[tokio::test(flavor = "current_thread")]
async fn flush_modes_are_selectable() {
    use windows::Win32::Storage::FileSystem::{
        FILE_FLUSH_DATA, FILE_FLUSH_DEFAULT, FILE_FLUSH_MIN_METADATA, FILE_FLUSH_NO_SYNC,
    };

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let ring = IoRing::builder().build().unwrap();
            let driver = Driver::new(ring).unwrap();
            let handle = driver.handle();
            let driver_task = tokio::task::spawn_local(async move { driver.drive().await });

            let path = temp_path("flushmodes");
            let out = win_ioring::file::File::create(&path).unwrap();
            let payload = b"flushed".to_vec();
            handle
                .write(&out, payload.clone(), payload.len() as u32, 0)
                .await
                .unwrap();

            for mode in [
                FILE_FLUSH_DEFAULT,
                FILE_FLUSH_DATA,
                FILE_FLUSH_MIN_METADATA,
                FILE_FLUSH_NO_SYNC,
            ] {
                handle
                    .flush_with_options(&out, mode, win_ioring::io_ring::ops::SqeFlags::NONE)
                    .await
                    .unwrap_or_else(|e| panic!("flush mode {mode:?} failed: {e}"));
            }

            handle.shutdown();
            driver_task.await.unwrap();
            drop(out);
            let _ = std::fs::remove_file(&path);
        })
        .await;
}

/// Write and flush must both refuse to start once the driver is shutting down,
/// and a write must still hand its buffer back when it does.
#[tokio::test(flavor = "current_thread")]
async fn write_and_flush_after_shutdown_error() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let ring = IoRing::builder().build().unwrap();
            let driver = Driver::new(ring).unwrap();
            let handle = driver.handle();
            let driver_task = tokio::task::spawn_local(async move { driver.drive().await });

            let path = temp_path("shutdown");
            let out = win_ioring::file::File::create(&path).unwrap();

            handle.shutdown();
            driver_task.await.unwrap();

            let outcome = handle.write(&out, b"data".to_vec(), 4, 0).await;
            let result = outcome;
            let (err, buffer) = result.into_parts();
            assert!(matches!(err.unwrap_err(), win_ioring::Error::ShuttingDown));
            assert_eq!(buffer, b"data", "the buffer came back");

            let err = handle.flush(&out).await.unwrap_err();
            assert!(matches!(err, win_ioring::Error::ShuttingDown));

            drop(out);
            let _ = std::fs::remove_file(&path);
        })
        .await;
}

/// Dropping a write in flight must be as safe as dropping a read.
#[tokio::test(flavor = "current_thread")]
async fn dropping_writes_in_flight_is_safe() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let ring = IoRing::builder().build().unwrap();
            let driver = Driver::new(ring).unwrap();
            let handle = driver.handle();
            let driver_task = tokio::task::spawn_local(async move { driver.drive().await });

            let path = temp_path("dropwrite");
            let out = win_ioring::file::File::create(&path).unwrap();

            for i in 0..100 {
                let fut = handle.write(&out, vec![b'x'; 64], 64, i * 64);
                tokio::task::yield_now().await;
                drop(fut);
            }

            win_ioring_tests::settle(&handle).await;

            // The ring still works.
            let (written, _) = handle.write(&out, b"final".to_vec(), 5, 0).await.unwrap();
            assert_eq!(written, 5);

            handle.shutdown();
            driver_task.await.unwrap();
            drop(out);
            let _ = std::fs::remove_file(&path);
        })
        .await;
}

/// SC-009: a completed read must report exactly the bytes transferred, and a
/// length-tracking buffer's initialized length must agree — for a partial read
/// and a zero-length read, not just a full one.
#[tokio::test(flavor = "current_thread")]
async fn read_transfer_accounting_covers_partial_and_empty() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let ring = IoRing::builder().build().unwrap();
            let driver = Driver::new(ring).unwrap();
            let handle = driver.handle();
            let driver_task = tokio::task::spawn_local(async move { driver.drive().await });

            let temp = TempFile::new("accounting");
            std::fs::write(temp.path(), b"0123456789").unwrap();
            let file = win_ioring::file::File::open(temp.path()).unwrap();

            // Full read.
            let (read, buffer) = handle.read(&file, vec![0_u8; 32], 10, 0).await.unwrap();
            assert_eq!(read, 10);
            assert_eq!(buffer.len(), 10);
            assert_eq!(&buffer[..], b"0123456789");

            // Asking for more than remains is a short read, not an error.
            let (read, buffer) = handle.read(&file, vec![0_u8; 32], 32, 6).await.unwrap();
            assert_eq!(read, 4, "only four bytes remain past offset six");
            assert_eq!(
                buffer.len(),
                4,
                "the initialized length must track the partial transfer"
            );
            assert_eq!(&buffer[..], b"6789");

            // Zero-length read at a valid offset: legal, transfers nothing.
            let (read, buffer) = handle.read(&file, vec![0_u8; 32], 0, 0).await.unwrap();
            assert_eq!(read, 0);
            assert_eq!(buffer.len(), 0);

            // A read with nothing at all available reports end-of-file as an
            // error rather than a zero-byte success. That is the platform's
            // behaviour, and differs from a short read.
            let outcome = handle.read(&file, vec![0_u8; 32], 8, 100).await;
            let (result, buffer) = outcome.into_parts();
            let err = result.expect_err("reading past the end should report EOF");
            assert!(
                matches!(err, win_ioring::Error::Os(_)),
                "expected the platform's EOF error, got {err:?}"
            );
            assert_eq!(buffer.capacity(), 32, "the buffer came back");

            handle.shutdown();
            driver_task.await.unwrap();
            drop(file);
        })
        .await;
}

/// FR-036: a genuinely zero-length buffer must be submitted normally and
/// complete with a zero transfer, not rejected locally.
///
/// This is distinct from asking for zero bytes into a buffer that has capacity:
/// here the buffer has no allocation at all, so `buf_mut_ptr` may hand the
/// kernel a dangling-but-aligned pointer, which the platform must accept.
#[tokio::test(flavor = "current_thread")]
async fn zero_length_buffers_are_submitted_not_rejected() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let ring = IoRing::builder().build().unwrap();
            let driver = Driver::new(ring).unwrap();
            let handle = driver.handle();
            let driver_task = tokio::task::spawn_local(async move { driver.drive().await });

            let temp = TempFile::new("zerolen");
            std::fs::write(temp.path(), b"0123456789").unwrap();
            let file = win_ioring::file::File::open(temp.path()).unwrap();

            // An empty `Vec` has no allocation whatsoever.
            let empty: Vec<u8> = Vec::new();
            assert_eq!(empty.capacity(), 0);

            let read = handle.read(&file, empty, 0, 0);
            assert!(
                read.operation_id().is_some(),
                "a zero-length read must reach the kernel, not be rejected locally"
            );
            let (transferred, buffer) = read.await.unwrap();
            assert_eq!(transferred, 0);
            assert_eq!(buffer.len(), 0);

            // The same on the write path, against a separate destination.
            let out_temp = TempFile::new("zerolen-out");
            let out = win_ioring::file::File::create(out_temp.path()).unwrap();
            let write = handle.write(&out, Vec::new(), 0, 0);
            assert!(
                write.operation_id().is_some(),
                "a zero-length write must reach the kernel too"
            );
            let (transferred, _) = write.await.unwrap();
            assert_eq!(transferred, 0);

            handle.shutdown();
            driver_task.await.unwrap();
            drop(file);
            drop(out);
        })
        .await;
}

/// A write sourced from an inline buffer guards the same pointer-before-boxing
/// mistake that was caught on the read path: a heap-backed buffer would survive
/// it, an array would not.
#[tokio::test(flavor = "current_thread")]
async fn inline_array_writes_reach_the_kernel_correctly() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let ring = IoRing::builder().build().unwrap();
            let driver = Driver::new(ring).unwrap();
            let handle = driver.handle();
            let driver_task = tokio::task::spawn_local(async move { driver.drive().await });

            let temp = TempFile::new("inlinewrite");
            let out = win_ioring::file::File::create(temp.path()).unwrap();

            let payload: [u8; 8] = *b"inlined!";
            let (written, returned) = handle.write(&out, payload, 8, 0).await.unwrap();
            assert_eq!(written, 8);
            assert_eq!(returned, payload);

            let boxed: Box<[u8]> = b"boxedsli".to_vec().into_boxed_slice();
            let (written, _) = handle.write(&out, boxed, 8, 8).await.unwrap();
            assert_eq!(written, 8);

            handle.shutdown();
            driver_task.await.unwrap();
            drop(out);

            assert_eq!(std::fs::read(temp.path()).unwrap(), b"inlined!boxedsli");
        })
        .await;
}

/// SQE flags must be selectable on every operation whose platform builder takes
/// them, which is read, write and flush.
#[tokio::test(flavor = "current_thread")]
async fn sqe_flags_are_selectable_on_read_write_and_flush() {
    use win_ioring::io_ring::ops::SqeFlags;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let ring = IoRing::builder().build().unwrap();
            let driver = Driver::new(ring).unwrap();
            let handle = driver.handle();
            let driver_task = tokio::task::spawn_local(async move { driver.drive().await });

            let temp = TempFile::new("sqeflags");
            let out = win_ioring::file::File::create(temp.path()).unwrap();

            let (written, _) = handle
                .write_with_options(
                    &out,
                    b"drained".to_vec(),
                    7,
                    0,
                    windows::Win32::Storage::FileSystem::FILE_WRITE_FLAGS_NONE,
                    SqeFlags::DRAIN_PRECEDING_OPS,
                )
                .await
                .unwrap();
            assert_eq!(written, 7);

            handle
                .flush_with_options(
                    &out,
                    windows::Win32::Storage::FileSystem::FILE_FLUSH_DEFAULT,
                    SqeFlags::DRAIN_PRECEDING_OPS,
                )
                .await
                .unwrap();

            drop(out);

            // `File::create` opens write-only, so reading needs its own handle.
            let input = win_ioring::file::File::open(temp.path()).unwrap();
            let (read, buffer) = handle
                .read_with_flags(&input, vec![0_u8; 16], 7, 0, SqeFlags::DRAIN_PRECEDING_OPS)
                .await
                .unwrap();
            assert_eq!(read, 7);
            assert_eq!(&buffer[..], b"drained");

            handle.shutdown();
            driver_task.await.unwrap();
            drop(input);
        })
        .await;
}

/// Registration takes ownership on success, and hands the resources straight
/// back on failure.
#[tokio::test(flavor = "current_thread")]
async fn registering_buffers_takes_ownership_and_returns_them_on_failure() {
    use win_ioring::runtime::Registered;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let ring = IoRing::builder().build().unwrap();
            let driver = Driver::new(ring).unwrap();
            let handle = driver.handle();
            let driver_task = tokio::task::spawn_local(async move { driver.drive().await });

            // The platform accepts an empty registration and then fails it at
            // completion, so the crate rejects it up front where the error is
            // actually useful.
            let empty: Vec<Vec<u8>> = Vec::new();
            match handle.register_buffers(empty).await {
                Registered::Failed(_, returned) => assert!(returned.is_empty()),
                Registered::Ok(_) => panic!("an empty registration should be refused"),
            }

            let buffers = vec![vec![0_u8; 128], vec![0_u8; 128]];
            assert!(handle.register_buffers(buffers).await.is_ok());

            handle.shutdown();
            driver_task.await.unwrap();
        })
        .await;
}

/// SC-017a: reads may fill a registered buffer's whole extent, but writes are
/// bounded by how much of it is initialized, and a completed read raises that
/// prefix.
#[tokio::test(flavor = "current_thread")]
async fn registered_writes_are_bounded_by_the_initialized_prefix() {
    use win_ioring::runtime::FileTarget;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let ring = IoRing::builder().build().unwrap();
            let driver = Driver::new(ring).unwrap();
            let handle = driver.handle();
            let driver_task = tokio::task::spawn_local(async move { driver.drive().await });

            let temp = TempFile::new("regwatermark");
            std::fs::write(temp.path(), b"0123456789").unwrap();
            let out_temp = TempFile::new("regwatermark-out");

            // Registered with capacity 64 but nothing initialized.
            let buffer: Vec<u8> = Vec::with_capacity(64);
            let collection = handle.register_buffers(vec![buffer]).await.unwrap();

            let input = win_ioring::file::File::open(temp.path()).unwrap();
            // A separate destination: `File::create` truncates, which would
            // empty the source if they were the same file.
            let out = win_ioring::file::File::create(out_temp.path()).unwrap();

            let handle_buf = collection.check_out(0).unwrap();

            // Writing before anything is initialized must be refused, and the
            // buffer must come straight back.
            let (result, handle_buf) = handle
                .write_registered(FileTarget::Owned(&out), handle_buf, 0, 10, 0)
                .await
                .into_parts();
            let err = result.expect_err("writing uninitialized registered bytes must be refused");
            assert!(
                matches!(err, win_ioring::Error::RegisteredRangeOutOfBounds { .. }),
                "got {err:?}"
            );

            // Reading may target the whole extent, and extends the prefix.
            let (result, handle_buf) = handle
                .read_registered(FileTarget::Owned(&input), handle_buf, 0, 10, 0)
                .await
                .into_parts();
            assert_eq!(result.unwrap(), 10);
            // The bytes are readable through the handle — the point of the
            // whole design.
            assert_eq!(&handle_buf[..], b"0123456789");

            // Now the same write is within the initialized prefix.
            let (result, handle_buf) = handle
                .write_registered(FileTarget::Owned(&out), handle_buf, 0, 10, 0)
                .await
                .into_parts();
            assert_eq!(result.unwrap(), 10);

            // Beyond the prefix is still refused.
            let (result, handle_buf) = handle
                .write_registered(FileTarget::Owned(&out), handle_buf, 0, 40, 0)
                .await
                .into_parts();
            assert!(matches!(
                result.expect_err("writing past the prefix must be refused"),
                win_ioring::Error::RegisteredRangeOutOfBounds { .. }
            ));

            drop(handle_buf);
            handle.shutdown();
            driver_task.await.unwrap();
            drop(input);
            drop(out);
        })
        .await;
}

/// SC-017: the failure path returns the caller's buffers.
///
/// Note that the platform is more permissive here than expected: a zero-extent
/// descriptor is accepted, unlike a zero-*count* registration, which is refused
/// at completion. Only the pre-build failures are reachable through the public
/// API, so those are what this asserts; the completion-time path returns
/// resources through the same channel and is covered by the driver's own tests.
#[tokio::test(flavor = "current_thread")]
async fn a_failed_registration_returns_the_buffers() {
    use win_ioring::runtime::Registered;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let ring = IoRing::builder().build().unwrap();
            let driver = Driver::new(ring).unwrap();
            let handle = driver.handle();
            let driver_task = tokio::task::spawn_local(async move { driver.drive().await });

            // A zero-extent buffer is registrable; the platform only objects to
            // a registration with no buffers at all.
            let buffers: Vec<Vec<u8>> = vec![Vec::with_capacity(0)];
            assert!(handle.register_buffers(buffers).await.is_ok());

            handle.shutdown();

            // Registering against a shut-down driver fails, and the buffers must
            // come back rather than being swallowed.
            let buffers = vec![vec![1_u8; 8], vec![2_u8; 8]];
            match handle.register_buffers(buffers).await {
                Registered::Failed(_, returned) => {
                    assert_eq!(returned.len(), 2, "both buffers must come back");
                    assert_eq!(returned[0], vec![1_u8; 8], "contents must be intact");
                    assert_eq!(returned[1], vec![2_u8; 8]);
                }
                Registered::Ok(_) => panic!("registering after shutdown should fail"),
            }

            driver_task.await.unwrap();
        })
        .await;
}

/// SC-017 and SC-007c: superseding a registration leaves the superseded buffers
/// alive until the ring is closed, rather than returning them to the caller.
///
/// The liveness half needs a drop counter. Asserting only that neither call
/// returned an error would pass whether the superseded buffer was still alive
/// or had been freed under the kernel.
///
/// Superseding *while an operation is in flight* is a separate matter: a handle
/// held by an in-flight operation now blocks re-registration outright, and that
/// refusal is tested where the guard lives.
#[tokio::test(flavor = "current_thread")]
async fn a_superseded_registration_stays_alive_until_the_ring_closes() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use win_ioring::runtime::FileTarget;
    use win_ioring_tests::counting::CountingBuf;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let ring = IoRing::builder().build().unwrap();
            let driver = Driver::new(ring).unwrap();
            let handle = driver.handle();
            let driver_task = tokio::task::spawn_local(async move { driver.drive().await });

            let temp = TempFile::new("regsupersede");
            std::fs::write(temp.path(), b"0123456789").unwrap();
            let input = win_ioring::file::File::open(temp.path()).unwrap();

            let drops = Arc::new(AtomicUsize::new(0));
            let first = vec![CountingBuf::new(64, &drops)];
            let first_collection = handle.register_buffers(first).await.unwrap();
            assert_eq!(drops.load(Ordering::SeqCst), 0);

            // Use the first registration, then return its handle so nothing is
            // checked out when the second registration is requested.
            let buffer = first_collection.check_out(0).unwrap();
            let (result, buffer) = handle
                .read_registered(FileTarget::Owned(&input), buffer, 0, 10, 0)
                .await
                .into_parts();
            assert_eq!(result.unwrap(), 10);
            drop(buffer);

            let second: Vec<Vec<u8>> = vec![Vec::with_capacity(128)];
            let second_collection = handle.register_buffers(second).await.unwrap();

            // The superseded buffer must still be alive: the platform offers no
            // way to withdraw a registration, so it may still hold pointers.
            assert_eq!(
                drops.load(Ordering::SeqCst),
                0,
                "a superseded registration must not be freed while the ring lives"
            );

            // The new registration is the one now in force: its larger extent is
            // addressable, which the superseded 64-byte buffer would refuse.
            let buffer = second_collection.check_out(0).unwrap();
            let (result, buffer) = handle
                .read_registered(FileTarget::Owned(&input), buffer, 100, 10, 0)
                .await
                .into_parts();
            assert!(result.is_ok(), "got {result:?}");
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            drop(buffer);

            handle.shutdown();
            driver_task.await.unwrap();

            // The collection is still held, and a collection keeps its buffers
            // alive on its own — that is what lets a handle outlive the driver.
            assert_eq!(
                drops.load(Ordering::SeqCst),
                0,
                "a held collection must keep its buffers alive past ring close"
            );

            // Releasing the last reference is what finally frees it.
            drop(first_collection);
            drop(second_collection);
            assert_eq!(
                drops.load(Ordering::SeqCst),
                1,
                "the superseded buffer must be released once nothing holds it"
            );
            drop(input);
        })
        .await;
}

/// SC-017a: the watermark tracks a contiguous initialized prefix.
///
/// A read landing past the watermark leaves genuinely uninitialized bytes
/// before it, so it must not raise the mark; and a short read raises the mark by
/// what was transferred, not by what was asked for.
#[tokio::test(flavor = "current_thread")]
async fn the_watermark_only_covers_a_contiguous_initialized_prefix() {
    use win_ioring::runtime::FileTarget;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let ring = IoRing::builder().build().unwrap();
            let driver = Driver::new(ring).unwrap();
            let handle = driver.handle();
            let driver_task = tokio::task::spawn_local(async move { driver.drive().await });

            let temp = TempFile::new("regprefix");
            std::fs::write(temp.path(), b"0123456789").unwrap();
            let out_temp = TempFile::new("regprefix-out");
            let input = win_ioring::file::File::open(temp.path()).unwrap();
            let out = win_ioring::file::File::create(out_temp.path()).unwrap();

            let buffers: Vec<Vec<u8>> = vec![Vec::with_capacity(64)];
            let collection = handle.register_buffers(buffers).await.unwrap();
            let buffer = collection.check_out(0).unwrap();

            // A read into offset 32 leaves bytes 0..32 uninitialized, so the
            // prefix must stay at zero and every write must still be refused.
            let (result, buffer) = handle
                .read_registered(FileTarget::Owned(&input), buffer, 32, 10, 0)
                .await
                .into_parts();
            assert_eq!(result.unwrap(), 10);
            let (result, buffer) = handle
                .write_registered(FileTarget::Owned(&out), buffer, 0, 1, 0)
                .await
                .into_parts();
            assert!(
                matches!(
                    result.expect_err("a gap before the read must keep the prefix at zero"),
                    win_ioring::Error::RegisteredRangeOutOfBounds { .. }
                ),
                "a gap before the read must keep the prefix at zero"
            );

            // A short read extends the prefix by what was transferred, not by
            // the 40 bytes requested: the file only holds 10.
            let (result, buffer) = handle
                .read_registered(FileTarget::Owned(&input), buffer, 0, 40, 0)
                .await
                .into_parts();
            assert_eq!(result.unwrap(), 10, "reading past the end is a short read");
            let (result, buffer) = handle
                .write_registered(FileTarget::Owned(&out), buffer, 0, 10, 0)
                .await
                .into_parts();
            assert!(result.is_ok());
            let (result, buffer) = handle
                .write_registered(FileTarget::Owned(&out), buffer, 0, 11, 0)
                .await
                .into_parts();
            assert!(
                matches!(
                    result.expect_err("the prefix must track the transfer, not the request"),
                    win_ioring::Error::RegisteredRangeOutOfBounds { .. }
                ),
                "the prefix must track the transfer, not the request"
            );

            drop(buffer);
            handle.shutdown();
            driver_task.await.unwrap();
            drop(input);
            drop(out);
        })
        .await;
}

/// An out-of-range registered index must be rejected before anything is
/// submitted.
///
/// Where the rejection happens moved with the redesign: naming a buffer that
/// does not exist is now caught when a handle is checked out, before an
/// operation exists at all. Naming a *range* outside one that does exist is
/// still caught by the operation.
#[tokio::test(flavor = "current_thread")]
async fn out_of_range_registered_indices_are_rejected() {
    use win_ioring::runtime::FileTarget;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let ring = IoRing::builder().build().unwrap();
            let driver = Driver::new(ring).unwrap();
            let handle = driver.handle();
            let driver_task = tokio::task::spawn_local(async move { driver.drive().await });

            let temp = TempFile::new("regindex");
            std::fs::write(temp.path(), b"data").unwrap();
            let file = win_ioring::file::File::open(temp.path()).unwrap();

            let collection = handle.register_buffers(vec![vec![0_u8; 32]]).await.unwrap();

            // Index past the end of the registration, refused at checkout.
            assert!(matches!(
                collection
                    .check_out(5)
                    .expect_err("index five does not exist"),
                win_ioring::Error::InvalidRegisteredIndex { index: 5 }
            ));

            // Offset plus length past the registered extent, refused by the
            // operation, with the handle handed straight back.
            let buffer = collection.check_out(0).unwrap();
            let (result, buffer) = handle
                .read_registered(FileTarget::Owned(&file), buffer, 30, 8, 0)
                .await
                .into_parts();
            assert!(matches!(
                result.expect_err("range exceeds the registered extent"),
                win_ioring::Error::RegisteredRangeOutOfBounds { .. }
            ));

            // A registered file index with no file registration.
            let (result, buffer) = handle
                .read_registered(FileTarget::Registered { index: 0 }, buffer, 0, 4, 0)
                .await
                .into_parts();
            assert!(matches!(
                result.expect_err("no file registration exists"),
                win_ioring::Error::InvalidRegisteredIndex { index: 0 }
            ));

            drop(buffer);
            handle.shutdown();
            driver_task.await.unwrap();
            drop(file);
        })
        .await;
}

/// Registered file handles must be usable as an operation's target.
#[tokio::test(flavor = "current_thread")]
async fn registered_file_handles_can_target_operations() {
    use win_ioring::runtime::FileTarget;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let ring = IoRing::builder().build().unwrap();
            let driver = Driver::new(ring).unwrap();
            let handle = driver.handle();
            let driver_task = tokio::task::spawn_local(async move { driver.drive().await });

            let temp = TempFile::new("regfile");
            std::fs::write(temp.path(), b"registered!").unwrap();

            let file = win_ioring::file::File::open(temp.path()).unwrap();
            assert!(
                handle
                    .register_files(std::slice::from_ref(&file))
                    .await
                    .is_ok()
            );
            let collection = handle.register_buffers(vec![vec![0_u8; 64]]).await.unwrap();

            // Drop the caller's own reference; the registration keeps it open.
            drop(file);

            let buffer = collection.check_out(0).unwrap();
            let (result, buffer) = handle
                .read_registered(FileTarget::Registered { index: 0 }, buffer, 0, 11, 0)
                .await
                .into_parts();
            assert_eq!(result.unwrap(), 11);
            // Both registrations combined, and the bytes came back readable.
            // The buffer was registered already fully initialized, so its
            // prefix spans the whole extent; the read filled its first 11 bytes.
            assert_eq!(&buffer[..11], b"registered!");

            drop(buffer);
            handle.shutdown();
            driver_task.await.unwrap();
        })
        .await;
}
