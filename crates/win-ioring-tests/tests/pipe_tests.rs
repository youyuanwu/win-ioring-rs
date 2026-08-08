//! Integration tests for the named pipe types, exercised through the public API
//! exactly as a downstream consumer would reach them.
//!
//! These live outside the `win-ioring` crate deliberately. The unit tests in
//! `pipe::server` and `pipe::client` reach `pub(crate)` seams and test-only
//! constructors; nothing there establishes that the *public* surface is
//! sufficient to accept a client and move bytes. SC-001 is that criterion, and
//! it is the one gap this work's own coverage audit could not see — the audit
//! reads whether a plan phase cites a criterion, not whether a test exists.

use std::rc::Rc;

use win_ioring::io_ring::IoRing;
use win_ioring::pipe::{ClientOptions, ServerOptions};
use win_ioring::runtime::{Driver, FileTarget, Handle};

/// Runs `body` with a driver spawned on Tokio's local set, then shuts down.
///
/// Copied from `file_tests.rs` rather than shared, for the reason given there:
/// getting the shutdown wrong makes a failure look like a hang.
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

/// A pipe name unique to this test binary and this test.
fn unique(tag: &str) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    format!(
        "win-ioring-it-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

/// SC-001: a server accepts a client and bytes move **both** ways through the
/// ring, against a real pipe, in the default `cargo test` run.
///
/// The direction matters and is easy to fake. A test that only writes
/// server-to-client exercises one code path twice — the pipe is duplex, so the
/// two directions are different handles with different access rights, and an
/// options bug that denied one of them would pass a single-direction test.
///
/// The accept here is a *waiting* one: the server's connect is submitted before
/// the client exists. That is asserted rather than assumed, because a client
/// that connects first resolves the accept synchronously through the
/// `ERROR_PIPE_CONNECTED` path and never exercises the event wait.
#[tokio::test(flavor = "current_thread")]
async fn a_server_accepts_a_client_and_bytes_move_both_ways() {
    with_driver(|handle| async move {
        let name = unique("both-ways");
        let mut server = ServerOptions::new().create(&name).unwrap();

        // Submit the connect before the client exists, so the accept resolves
        // through the event wait rather than the synchronous path.
        //
        // The server cannot be inspected while the accept is alive — the future
        // borrows it exclusively, which is what makes two concurrent accepts
        // unrepresentable — so the ordering is established afterwards, by
        // `synchronous_accepts`. A client that connected first would make
        // `ConnectNamedPipe` return `ERROR_PIPE_CONNECTED` and increment that
        // counter; a zero means the connect went to the kernel and came back
        // through the event.
        let mut accept = Box::pin(server.accept());
        assert!(
            futures::poll!(accept.as_mut()).is_pending(),
            "the connect must not resolve before any client exists"
        );

        let client = ClientOptions::new().open(&name).unwrap();
        accept.await.unwrap();
        assert_eq!(
            server.synchronous_accepts(),
            0,
            "this accept waited on the event; a synchronous one would not have \
             tested the waiting path at all"
        );

        let server_file = server.file().unwrap();

        // Server -> client.
        let (written, _) = handle
            .write(server_file, b"from-the-server".to_vec(), 15, 0)
            .await
            .into_parts();
        assert_eq!(written.unwrap(), 15);

        let (read, buffer) = handle
            .read(client.file(), vec![0_u8; 15], 15, 0)
            .await
            .into_parts();
        assert_eq!(read.unwrap(), 15);
        assert_eq!(&buffer, b"from-the-server");

        // Client -> server. The other direction, over the same connection.
        let (written, _) = handle
            .write(client.file(), b"from-the-client".to_vec(), 15, 0)
            .await
            .into_parts();
        assert_eq!(written.unwrap(), 15);

        let (read, buffer) = handle
            .read(server_file, vec![0_u8; 15], 15, 0)
            .await
            .into_parts();
        assert_eq!(read.unwrap(), 15);
        assert_eq!(&buffer, b"from-the-client");
    })
    .await;
}

/// SC-001: a pipe read succeeds through a **registered buffer**.
///
/// Registration is the part of the ring most likely to reject a handle it was
/// not designed for, so "pipes are ordinary handles to the ring" is a claim that
/// has to be tested rather than asserted. The write side is deliberately an
/// ordinary one: this is a test about the read path's registration, and using a
/// registered buffer for both would leave a failure ambiguous.
#[tokio::test(flavor = "current_thread")]
async fn a_pipe_read_succeeds_through_a_registered_buffer() {
    with_driver(|handle| async move {
        let name = unique("reg-buffer");
        let mut server = ServerOptions::new().create(&name).unwrap();
        let client = ClientOptions::new().open(&name).unwrap();
        server.accept().await.unwrap();

        let (written, _) = handle
            .write(client.file(), b"registered".to_vec(), 10, 0)
            .await
            .into_parts();
        assert_eq!(written.unwrap(), 10);

        let buffers = handle.register_buffers(vec![vec![0_u8; 32]]).await.unwrap();
        let buffer = buffers.check_out(0).unwrap();

        let (read, buffer) = handle
            .read_registered(FileTarget::Owned(server.file().unwrap()), buffer, 0, 10, 0)
            .await
            .into_parts();
        assert_eq!(read.unwrap(), 10);
        assert_eq!(&buffer[..10], b"registered");
    })
    .await;
}

/// SC-001: a pipe read succeeds through a **registered file handle**.
///
/// The other half of the registration claim, and the one with a real chance of
/// failing: registering file *handles* hands the kernel the pipe handle itself
/// rather than a buffer, and the operation then names it by index.
#[tokio::test(flavor = "current_thread")]
async fn a_pipe_read_succeeds_through_a_registered_file_handle() {
    with_driver(|handle| async move {
        let name = unique("reg-file");
        let mut server = ServerOptions::new().create(&name).unwrap();
        let client = ClientOptions::new().open(&name).unwrap();
        server.accept().await.unwrap();

        let (written, _) = handle
            .write(client.file(), b"by-index".to_vec(), 8, 0)
            .await
            .into_parts();
        assert_eq!(written.unwrap(), 8);

        let server_file = server.file().unwrap().clone();
        handle
            .register_files(std::slice::from_ref(&server_file))
            .await
            .unwrap();
        let buffers = handle.register_buffers(vec![vec![0_u8; 32]]).await.unwrap();
        let buffer = buffers.check_out(0).unwrap();

        let (read, buffer) = handle
            .read_registered(FileTarget::Registered { index: 0 }, buffer, 0, 8, 0)
            .await
            .into_parts();
        assert_eq!(read.unwrap(), 8);
        assert_eq!(&buffer[..8], b"by-index");
    })
    .await;
}
