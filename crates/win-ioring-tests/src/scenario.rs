//! A scenario exercised identically under two unrelated executors.
//!
//! The crate claims to be runtime agnostic. The only way to hold that claim to
//! account is to run the same work under two executors that share no code and
//! compare what came out, rather than merely observing that neither crashed.
//!
//! Every step appends a line to a transcript. Two executors driving the same
//! ring must produce the same transcript, line for line.

use std::fmt::Write as _;

use win_ioring::file::File;
use win_ioring::runtime::{FileTarget, Handle, Registered};

/// Runs the full scenario against `handle`, returning its transcript.
///
/// `dir_tag` distinguishes the temporary files of one run from another's, so
/// two runs never collide; it deliberately does not appear in the transcript.
pub async fn transcript(handle: &Handle, dir_tag: &str) -> String {
    let mut out = String::new();
    let source = crate::temp::TempFile::new(&format!("{dir_tag}-src"));
    let sink = crate::temp::TempFile::new(&format!("{dir_tag}-sink"));
    std::fs::write(source.path(), b"0123456789ABCDEF").unwrap();

    // Positional read.
    let input = File::open(source.path()).unwrap();
    let (read, buffer) = input
        .read_at(handle, vec![0_u8; 32], 16, 0)
        .await
        .expect_completed()
        .unwrap();
    writeln!(out, "read_at: {read} {:?}", &buffer[..read as usize]).unwrap();
    writeln!(out, "cursor after read_at: {}", input.cursor()).unwrap();

    // Short read: asking past the end succeeds with fewer bytes.
    let (read, _) = input
        .read_at(handle, vec![0_u8; 64], 40, 8)
        .await
        .expect_completed()
        .unwrap();
    writeln!(out, "short read_at: {read}").unwrap();

    // A read with nothing left to give fails rather than returning zero.
    let outcome = input.read_at(handle, vec![0_u8; 8], 8, 64).await;
    writeln!(out, "eof read_at: {:?}", outcome.err()).unwrap();

    // Zero-length read at a valid offset succeeds.
    let (read, _) = input
        .read_at(handle, vec![0_u8; 8], 0, 0)
        .await
        .expect_completed()
        .unwrap();
    writeln!(out, "empty read_at: {read}").unwrap();

    // Rejected before submission: the buffer cannot hold what was asked for.
    let outcome = input.read_at(handle, vec![0_u8; 4], 64, 0).await;
    writeln!(out, "oversized read_at: {:?}", outcome.err()).unwrap();

    // Sequential reads walk the file and move the cursor.
    let mut input = input;
    for _ in 0..2 {
        let (read, buffer) = input
            .read(handle, vec![0_u8; 4], 4)
            .await
            .expect_completed()
            .unwrap();
        writeln!(
            out,
            "read: {read} {:?} cursor {}",
            &buffer[..read as usize],
            input.cursor()
        )
        .unwrap();
    }
    drop(input);

    // Writes, positional and sequential, then read back through the file.
    let mut output = File::create(sink.path()).unwrap();
    let (written, _) = output
        .write_at(handle, b"positional".to_vec(), 10, 0)
        .await
        .expect_completed()
        .unwrap();
    writeln!(out, "write_at: {written} cursor {}", output.cursor()).unwrap();

    output.set_cursor(10);
    let (written, _) = output
        .write(handle, b"sequential".to_vec(), 10)
        .await
        .expect_completed()
        .unwrap();
    writeln!(out, "write: {written} cursor {}", output.cursor()).unwrap();

    writeln!(out, "flush: {:?}", output.flush(handle).await).unwrap();
    drop(output);
    writeln!(out, "on disk: {:?}", std::fs::read(sink.path()).unwrap()).unwrap();

    // Registered buffers, including the initialization watermark rule.
    let registered = File::open(source.path()).unwrap();
    let buffers: Vec<Vec<u8>> = vec![Vec::with_capacity(64)];
    writeln!(
        out,
        "register_buffers: {:?}",
        matches!(handle.register_buffers(buffers).await, Registered::Ok)
    )
    .unwrap();
    writeln!(
        out,
        "write_from_registered before init: {:?}",
        handle
            .write_from_registered(FileTarget::Owned(&registered), 0, 0, 4, 0)
            .await
            .err()
    )
    .unwrap();
    writeln!(
        out,
        "read_into_registered: {:?}",
        handle
            .read_into_registered(FileTarget::Owned(&registered), 0, 0, 8, 0)
            .await
    )
    .unwrap();

    // An out-of-range registered index is refused without reaching the kernel.
    writeln!(
        out,
        "bad registered index: {:?}",
        handle
            .read_into_registered(FileTarget::Owned(&registered), 9, 0, 4, 0)
            .await
            .err()
    )
    .unwrap();

    // A read whose identifier is then cancelled after it has already finished:
    // this must be harmless, and must leave the ring usable. Cancelling an
    // operation still in flight is deliberately not part of this scenario,
    // because whether the cancellation beats the completion is a race and would
    // make the transcript nondeterministic.
    let pending = registered.read_at(handle, vec![0_u8; 8], 8, 0);
    let id = pending
        .operation_id()
        .expect("the read must have been built");
    let done = pending.await.expect_completed();
    writeln!(out, "final read: {:?}", done.result).unwrap();
    handle.cancel(id);

    let (read, _) = registered
        .read_at(handle, vec![0_u8; 4], 4, 0)
        .await
        .expect_completed()
        .unwrap();
    writeln!(out, "read after stale cancel: {read}").unwrap();

    out
}
