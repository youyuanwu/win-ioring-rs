//! Tests for the async [`File`] abstraction: positional and sequential
//! operations, cursor accounting, and handle lifetime.

use std::rc::Rc;

use win_ioring::file::File;
use win_ioring::io_ring::IoRing;
use win_ioring::runtime::{Driver, Handle};
use win_ioring_tests::temp::TempFile;

/// Runs `body` with a driver spawned on Tokio's local set, then shuts down.
///
/// Every test here needs the same scaffolding, and getting the shutdown wrong
/// makes a failure look like a hang rather than an assertion.
async fn with_driver<F, Fut>(body: F)
where
    F: FnOnce(Rc<Handle>) -> Fut,
    Fut: Future<Output = ()>,
{
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let ring = IoRing::builder().build().unwrap();
            let driver = Driver::new(ring).unwrap();
            let handle = Rc::new(driver.handle());
            let driver_task = tokio::task::spawn_local(async move { driver.drive().await });

            body(Rc::clone(&handle)).await;

            handle.shutdown();
            driver_task.await.unwrap();
        })
        .await;
}

/// SC-012: a write-then-read round trip returns exactly the bytes written, in
/// both the positional and the sequential form.
#[tokio::test(flavor = "current_thread")]
async fn round_trip_in_both_forms() {
    with_driver(|handle| async move {
        let temp = TempFile::new("file-roundtrip");

        // Positional.
        let out = File::create(temp.path()).unwrap();
        let written = out
            .write_at(&handle, b"positional".to_vec(), 10, 0)
            .await
            .unwrap()
            .0;
        assert_eq!(written, 10);
        assert_eq!(
            out.cursor(),
            0,
            "a positional write must not move the cursor"
        );
        drop(out);

        let input = File::open(temp.path()).unwrap();
        let (read, buffer) = input.read_at(&handle, vec![0_u8; 32], 10, 0).await.unwrap();
        assert_eq!(read, 10);
        assert_eq!(&buffer[..10], b"positional");
        assert_eq!(input.cursor(), 0);
        drop(input);

        // Sequential.
        let mut out = File::create(temp.path()).unwrap();
        let written = out
            .write(&handle, b"sequential".to_vec(), 10)
            .await
            .unwrap()
            .0;
        assert_eq!(written, 10);
        assert_eq!(out.cursor(), 10, "a sequential write must move the cursor");
        drop(out);

        let mut input = File::open(temp.path()).unwrap();
        let (read, buffer) = input.read(&handle, vec![0_u8; 32], 10).await.unwrap();
        assert_eq!(read, 10);
        assert_eq!(&buffer[..10], b"sequential");
        assert_eq!(input.cursor(), 10);
    })
    .await;
}

/// Successive sequential reads walk the file, each starting where the last
/// stopped.
#[tokio::test(flavor = "current_thread")]
async fn sequential_reads_walk_the_file() {
    with_driver(|handle| async move {
        let temp = TempFile::new("file-walk");
        std::fs::write(temp.path(), b"abcdefghij").unwrap();

        let mut file = File::open(temp.path()).unwrap();
        let mut seen = Vec::new();
        for _ in 0..5 {
            let (read, buffer) = file.read(&handle, vec![0_u8; 2], 2).await.unwrap();
            assert_eq!(read, 2);
            seen.extend_from_slice(&buffer[..2]);
        }
        assert_eq!(seen, b"abcdefghij");
        assert_eq!(file.cursor(), 10);
    })
    .await;
}

/// FR-027b: the cursor advances by bytes actually transferred, and stays put
/// when nothing is transferred or the operation fails.
#[tokio::test(flavor = "current_thread")]
async fn cursor_accounting_covers_partial_empty_and_failed_transfers() {
    with_driver(|handle| async move {
        let temp = TempFile::new("file-cursor");
        std::fs::write(temp.path(), b"0123456789").unwrap();
        let mut file = File::open(temp.path()).unwrap();

        // A zero-length read transfers nothing and must not move the cursor.
        let (read, _) = file.read(&handle, vec![0_u8; 8], 0).await.unwrap();
        assert_eq!(read, 0);
        assert_eq!(file.cursor(), 0);

        // A short read advances by what arrived, not by what was asked for.
        let (read, _) = file.read(&handle, vec![0_u8; 64], 40).await.unwrap();
        assert_eq!(read, 10, "asking past the end is a short read");
        assert_eq!(file.cursor(), 10);

        // A read at the end fails, and a failure transfers nothing.
        let outcome = file.read(&handle, vec![0_u8; 8], 8).await;
        assert!(outcome.err().is_some(), "reading at EOF must fail");
        assert_eq!(file.cursor(), 10, "a failed read must not move the cursor");
    })
    .await;
}

/// FR-027a and FR-027f: positional operations need only shared access and may
/// run concurrently (SC-028).
#[tokio::test(flavor = "current_thread")]
async fn concurrent_positional_operations_share_one_file() {
    with_driver(|handle| async move {
        let temp = TempFile::new("file-concurrent");
        std::fs::write(temp.path(), b"0123456789").unwrap();
        let file = File::open(temp.path()).unwrap();

        // Three reads at once against the same `&File`, which only compiles
        // because positional operations take shared access.
        let (a, b, c) = tokio::join!(
            file.read_at(&handle, vec![0_u8; 4], 4, 0),
            file.read_at(&handle, vec![0_u8; 4], 4, 3),
            file.read_at(&handle, vec![0_u8; 4], 4, 6),
        );

        let (n, buf) = a.unwrap();
        assert_eq!((n, &buf[..4]), (4, b"0123".as_slice()));
        let (n, buf) = b.unwrap();
        assert_eq!((n, &buf[..4]), (4, b"3456".as_slice()));
        let (n, buf) = c.unwrap();
        assert_eq!((n, &buf[..4]), (4, b"6789".as_slice()));

        assert_eq!(file.cursor(), 0, "positional reads must leave the cursor");
    })
    .await;
}

/// FR-027d and FR-027e (SC-019 runtime half): dropping a sequential future
/// releases the exclusive borrow while the operation is still outstanding, so
/// the file must refuse the next sequential operation until it completes — and
/// must then become usable again, with the cursor untouched.
#[tokio::test(flavor = "current_thread")]
async fn a_dropped_sequential_future_blocks_then_releases_the_file() {
    with_driver(|handle| async move {
        let temp = TempFile::new("file-dropseq");
        std::fs::write(temp.path(), b"0123456789").unwrap();
        let mut file = File::open(temp.path()).unwrap();

        // Start a sequential read and drop its future before it completes.
        {
            let fut = file.read(&handle, vec![0_u8; 8], 8);
            assert!(
                fut.operation_id().is_some(),
                "the read must have been built"
            );
            drop(fut);
        }

        // Nothing has awaited since the drop, and a current-thread runtime only
        // switches tasks at an await point, so the driver cannot have reaped
        // the completion yet: the flag is still set, and the file must refuse.
        assert!(
            file.sequential_outstanding(),
            "the operation must still be outstanding at this point"
        );
        let outcome = file.read(&handle, vec![0_u8; 8], 8).await;
        let err = outcome.err().expect("a second sequential read must fail");
        assert!(
            matches!(err, win_ioring::Error::OperationOutstanding),
            "got {err:?}"
        );

        // Once the dropped operation reaches terminal completion the file is
        // usable again. The driver clears the flag, so this needs the executor
        // to run, not just a spin.
        for _ in 0..1000 {
            if !file.sequential_outstanding() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            !file.sequential_outstanding(),
            "the flag must clear at terminal completion"
        );

        assert_eq!(
            file.cursor(),
            0,
            "a dropped sequential operation must not move the cursor"
        );

        // And the file works again, starting from the untouched cursor.
        let (read, buffer) = file.read(&handle, vec![0_u8; 4], 4).await.unwrap();
        assert_eq!(read, 4);
        assert_eq!(&buffer[..4], b"0123");
        assert_eq!(file.cursor(), 4);
    })
    .await;
}

/// SC-022: dropping both the file and the future before completion must still
/// be safe, because the operation holds its own reference to the handle.
#[tokio::test(flavor = "current_thread")]
async fn dropping_the_file_and_future_before_completion_is_safe() {
    with_driver(|handle| async move {
        let temp = TempFile::new("file-dropboth");
        std::fs::write(temp.path(), b"0123456789").unwrap();

        let file = File::open(temp.path()).unwrap();
        let fut = file.read_at(&handle, vec![0_u8; 8], 8, 0);
        assert!(fut.operation_id().is_some());

        // The caller walks away from everything while the kernel is still
        // working. The handle must outlive both.
        drop(fut);
        drop(file);

        // Let the driver settle; nothing here may panic.
        for _ in 0..1000 {
            tokio::task::yield_now().await;
        }

        // The ring is still usable afterwards.
        let file = File::open(temp.path()).unwrap();
        let (read, _) = file.read_at(&handle, vec![0_u8; 8], 8, 0).await.unwrap();
        assert_eq!(read, 8);
    })
    .await;
}

/// SC-026: a file adopted from `std::fs::File` works, and its handle is owned
/// by exactly one `File`, released when the last reference goes.
#[tokio::test(flavor = "current_thread")]
async fn adopting_a_std_file_works_and_owns_the_handle() {
    with_driver(|handle| async move {
        let temp = TempFile::new("file-adopt");
        std::fs::write(temp.path(), b"adopted!!!").unwrap();

        let std_file = std::fs::File::open(temp.path()).unwrap();
        let file = File::from_std(std_file);
        assert_eq!(file.reference_count(), 1);

        let (read, buffer) = file.read_at(&handle, vec![0_u8; 16], 10, 0).await.unwrap();
        assert_eq!(read, 10);
        assert_eq!(&buffer[..10], b"adopted!!!");

        // With the operation finished, the caller holds the only reference.
        assert_eq!(
            file.reference_count(),
            1,
            "the driver must have released its reference"
        );
    })
    .await;
}

/// `set_cursor` repositions sequential operations, and positional operations
/// still ignore the cursor entirely.
#[tokio::test(flavor = "current_thread")]
async fn setting_the_cursor_repositions_sequential_operations() {
    with_driver(|handle| async move {
        let temp = TempFile::new("file-setcursor");
        std::fs::write(temp.path(), b"0123456789").unwrap();
        let mut file = File::open(temp.path()).unwrap();

        file.set_cursor(6);
        let (read, buffer) = file.read(&handle, vec![0_u8; 4], 4).await.unwrap();
        assert_eq!(read, 4);
        assert_eq!(&buffer[..4], b"6789");
        assert_eq!(file.cursor(), 10);

        // A positional read still addresses the file directly.
        let (read, buffer) = file.read_at(&handle, vec![0_u8; 4], 4, 0).await.unwrap();
        assert_eq!(read, 4);
        assert_eq!(&buffer[..4], b"0123");
        assert_eq!(file.cursor(), 10, "a positional read must not move it");
    })
    .await;
}

/// FR-003: flush is reachable from the file, in both its default and its
/// explicit-mode form.
#[tokio::test(flavor = "current_thread")]
async fn flush_is_reachable_from_the_file() {
    use windows::Win32::Storage::FileSystem::FILE_FLUSH_DEFAULT;

    with_driver(|handle| async move {
        let temp = TempFile::new("file-flush");
        let mut file = File::create(temp.path()).unwrap();

        let written = file
            .write(&handle, b"flush me".to_vec(), 8)
            .await
            .unwrap()
            .0;
        assert_eq!(written, 8);

        file.flush(&handle).await.unwrap();
        file.flush_with_mode(&handle, FILE_FLUSH_DEFAULT)
            .await
            .unwrap();

        drop(file);
        assert_eq!(std::fs::read(temp.path()).unwrap(), b"flush me");
    })
    .await;
}
