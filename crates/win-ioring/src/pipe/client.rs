//! The client end of a named pipe.

use super::io::{PipeRead, PipeWrite};
use crate::buf::{IoBuf, IoBufMut};
use crate::file::File;
use crate::pipe::error::Error;
use crate::runtime::Handle;

/// Options for connecting to a named pipe.
///
/// Separate from [`Client`] so that the access mode can be chosen before the
/// handle exists — a pipe's direction is fixed at open time and cannot be
/// widened afterwards.
///
/// ```no_run
/// use win_ioring::pipe::ClientOptions;
///
/// let client = ClientOptions::new().read(true).write(true).open("demo")?;
/// # Ok::<(), win_ioring::pipe::Error>(())
/// ```
#[derive(Debug, Clone)]
pub struct ClientOptions {
    read: bool,
    write: bool,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientOptions {
    /// Read and write, which is what a duplex pipe's client usually wants.
    pub fn new() -> Self {
        Self {
            read: true,
            write: true,
        }
    }

    /// Requests read access. Defaults to `true`.
    pub fn read(mut self, read: bool) -> Self {
        self.read = read;
        self
    }

    /// Requests write access. Defaults to `true`.
    pub fn write(mut self, write: bool) -> Self {
        self.write = write;
        self
    }

    /// Connects to an existing pipe instance.
    ///
    /// `name` may be a bare name such as `"demo"`, which is qualified to
    /// `\\.\pipe\demo`, or an already-qualified path. A path naming another
    /// host is used as given.
    ///
    /// The handle is **overlapped**, like [`File::open`]'s. A pipe client that
    /// opened synchronously would serialise its operations through the ring
    /// rather than failing, which is the quietest possible way to lose
    /// concurrency, so this is not left to the caller.
    ///
    /// # This does not wait for an instance
    ///
    /// If the server has created instances but all of them are already serving
    /// clients, this returns [`Error::Busy`](crate::pipe::error::Error::Busy)
    /// immediately. It does **not** block, and there is no equivalent of Win32's
    /// `WaitNamedPipe`.
    ///
    /// That is a deliberate omission rather than an oversight. This crate is
    /// runtime-agnostic and single-threaded; a blocking wait would stall the
    /// caller's executor, and a timed retry would need a timer this crate does
    /// not have and should not pick for the caller. Retrying on
    /// `Error::Busy` — with whatever backoff and whatever timer the caller's
    /// runtime provides — is the intended pattern.
    ///
    /// If the server has not created the pipe at all, the error is
    /// [`Error::Other`](crate::pipe::error::Error::Other) carrying
    /// `ERROR_FILE_NOT_FOUND`, which is
    /// a different condition and deliberately not folded into `PipeBusy`: one
    /// says "come back shortly", the other says "nothing is listening here".
    pub fn open(&self, name: impl AsRef<str>) -> Result<Client, Error> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;

        let path = super::qualify(name.as_ref());
        let opened = std::fs::OpenOptions::new()
            .read(self.read)
            .write(self.write)
            .custom_flags(FILE_FLAG_OVERLAPPED.0)
            .open(&path);

        match opened {
            Ok(file) => Ok(Client {
                file: File::from_std(file),
            }),
            Err(e) => Err(classify_open_failure(&e)),
        }
    }
}

/// Maps an open failure onto the crate's error type.
///
/// Routes through the crate's single classification table, which is the same
/// funnel every ring completion passes through, rather than repeating the code
/// comparisons here. That matters more than it looks: `ERROR_PIPE_BUSY` from a
/// failed open and `ERROR_PIPE_BUSY` from a completion must produce the same
/// variant, and two independent match arms are exactly how that stops being
/// true after someone edits one of them.
///
/// Under the per-API split this property is now structural rather than merely
/// observed: there is one table, and `pipe::Error` is a *view* over it, so
/// there is no second set of arms available to edit.
///
/// # The one substituted code in the crate, and why it stays
///
/// An `io::Error` with no OS code cannot come from `CreateFileW` — the failure
/// path builds it from `GetLastError` — but the type permits it. FR-11 forbids
/// fabricating an `HRESULT`, and this branch does substitute one, so it is
/// called out rather than hidden:
///
/// - No *condition* is fabricated. `E_FAIL` is not in the classification table,
///   so it can only ever produce `Other`, never a named pipe condition. The
///   hazard FR-11 exists to prevent — a made-up code being re-classified into a
///   condition that never occurred — cannot happen here.
/// - Nothing is lost. The original `io::Error`'s message is carried on the
///   substituted code, so a caller still sees what actually failed.
/// - The alternative costs a public slot on `pipe::Error` for a variant that is
///   unreachable, which is a worse trade than one documented placeholder.
fn classify_open_failure(e: &std::io::Error) -> Error {
    match e.raw_os_error() {
        Some(code) => Error::from(windows::core::HRESULT::from_win32(code as u32)),
        None => Error::Other(windows::core::Error::new(
            windows::Win32::Foundation::E_FAIL,
            format!("pipe open failed without an OS error code: {e}"),
        )),
    }
}

/// A connected client end of a named pipe.
///
/// Owns the handle. Reads and writes go through the ring by way of the
/// [`File`] this derefs into, so there is no pipe-specific I/O API to learn.
///
/// # Sequential reads and writes are refused
///
/// [`File::read`] and [`File::write`] track a cursor and pass it to the ring as
/// an offset. A pipe has no meaningful file position, and the platform
/// **ignores** the offset rather than rejecting it — so a sequential read at a
/// non-zero cursor would return the bytes at the front of the pipe while
/// reporting the cursor advanced past them. This crate refuses those two calls
/// on a pipe rather than letting them return wrong data successfully. Use
/// [`File::read_at`] and [`File::write_at`], whose offsets a pipe also ignores
/// but which do not imply a position the caller can rely on.
#[derive(Debug)]
pub struct Client {
    file: File,
}

impl Client {
    /// Connects with the default options: read and write.
    ///
    /// Equivalent to `ClientOptions::new().open(name)`. See
    /// [`ClientOptions::open`] for what happens when every instance is busy —
    /// this does not wait either.
    pub fn connect(name: impl AsRef<str>) -> Result<Self, Error> {
        ClientOptions::new().open(name)
    }

    /// The underlying file, for reads, writes and flushes through the ring.
    ///
    /// # What this replaces
    ///
    /// This type used to `Deref` to [`File`] and to offer `into_file`. Both are
    /// gone, because a pipe's reads and writes are not a file's: they fail in
    /// ways a file cannot ([`Error::Broken`], [`Error::NoPeer`]) and they now
    /// report [`Error`] rather than [`file::Error`](crate::file::Error).
    ///
    /// - For pipe I/O, use [`Client::read_at`] and [`Client::write_at`], which
    ///   return this module's error type. Under `Deref` these calls silently
    ///   resolved to [`File`]'s and handed back a file's error for a pipe's
    ///   failure.
    /// - For the deliberate case where a pipe is *wanted* as a byte stream --
    ///   passing it to code that takes a [`File`] and does not care what is
    ///   behind it -- this method still gives you one. What it gives you is a
    ///   `&File`, and that is the difference that matters: [`File::read`] and
    ///   [`File::write`] take `&mut self`, so a borrow cannot reach the
    ///   *sequential* methods. Those track a cursor, a pipe has no seekable
    ///   position, and the result was
    ///   [`file::Error::NotSeekable`](crate::file::Error::NotSeekable) -- a file
    ///   condition with no pipe counterpart, reached only because `into_file`
    ///   handed out ownership. Removing it is what closes that route.
    ///
    /// A caller who genuinely needs an owned [`File`] should open the path as a
    /// file rather than open it as a pipe and discard the distinction.
    pub fn file(&self) -> &File {
        &self.file
    }

    /// Reads up to `len` bytes into `buffer`, reporting failures as [`Error`].
    ///
    /// # The offset is ignored
    ///
    /// A pipe has no position, and the platform discards this argument. It is
    /// present because this is the same entry point the file surface uses and
    /// splitting the signature would gain nothing; pass `0` unless you have a
    /// reason not to.
    ///
    /// There is deliberately **no** sequential (`read`/`write`) pipe method. The
    /// cursor those maintain would be a fiction on a pipe: the platform ignores
    /// the offset, so a sequential read would report the cursor advancing past
    /// bytes it never positioned for. Refusing to offer it is the same choice
    /// this crate already makes by refusing `File::read` on a pipe.
    pub fn read_at<B: IoBufMut>(
        &self,
        handle: &Handle,
        buffer: B,
        len: u32,
        offset: u64,
    ) -> PipeRead<B> {
        PipeRead::issue(handle, &self.file, buffer, len, offset)
    }

    /// Writes `len` bytes from `buffer`, reporting failures as [`Error`].
    ///
    /// The offset is ignored, and there is no sequential counterpart. See
    /// [`read_at`](Self::read_at) for both.
    pub fn write_at<B: IoBuf>(
        &self,
        handle: &Handle,
        buffer: B,
        len: u32,
        offset: u64,
    ) -> PipeWrite<B> {
        PipeWrite::issue(handle, &self.file, buffer, len, offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX};
    use windows::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    use windows::core::PCWSTR;

    /// A server instance created by hand, so the client tests do not depend on
    /// `Server` existing yet.
    struct RawInstance {
        handle: HANDLE,
    }

    impl RawInstance {
        fn create(name: &str, max_instances: u32) -> Self {
            let path = crate::pipe::qualify(name);
            let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
            // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the
            // call, and the flag combination is the byte-mode duplex form this
            // crate creates everywhere. The returned handle is owned by this
            // value and closed in `Drop`.
            let handle = unsafe {
                CreateNamedPipeW(
                    PCWSTR(wide.as_ptr()),
                    PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    max_instances,
                    4096,
                    4096,
                    0,
                    None,
                )
            };
            assert!(
                !handle.is_invalid(),
                "the pipe instance should have been created"
            );
            Self { handle }
        }
    }

    impl Drop for RawInstance {
        fn drop(&mut self) {
            // SAFETY: `self.handle` came from a successful `CreateNamedPipeW`
            // and is closed exactly once, here.
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(self.handle);
            }
        }
    }

    /// A name unique to this test run, so tests can run in parallel.
    use crate::pipe::unique_name as unique;

    #[test]
    fn a_client_connects_to_a_waiting_instance() {
        let name = unique("connect");
        let _instance = RawInstance::create(&name, 1);

        let client = Client::connect(&name).expect("connecting to a waiting instance should work");
        assert!(!client.file().as_raw_handle().is_invalid());
    }

    /// Busy is its own condition, and it must be reachable.
    ///
    /// One instance is created and the first client takes it, so the second has
    /// nowhere to go.
    ///
    /// **The cap is not what makes this work, and an earlier version of this
    /// comment said it was.** `nMaxInstances` limits how many instances may be
    /// created; it does not create them. Raising it to four leaves exactly one
    /// instance in existence and the second client still refused — which a
    /// mutation confirmed, by surviving. The twin below is what carries the
    /// weight: the first client must succeed against this same instance, or the
    /// refusal proves nothing except that the name was wrong.
    #[test]
    fn a_second_client_is_refused_as_busy_when_every_instance_is_taken() {
        let name = unique("busy");
        let _instance = RawInstance::create(&name, 1);

        let first = Client::connect(&name);
        assert!(
            first.is_ok(),
            "the twin: the first client must succeed, or the refusal below \
             proves nothing about instance availability"
        );

        let second = Client::connect(&name);
        assert!(
            matches!(second, Err(Error::Busy)),
            "a second client with no free instance must be refused as busy, \
             got {second:?}"
        );
    }

    /// A second *created* instance is what frees the second client.
    ///
    /// The twin to the busy test above, and the one that proves the refusal
    /// tracks instance availability rather than something incidental about the
    /// name or the handle. Two instances, two clients, both connect.
    #[test]
    fn a_second_instance_admits_a_second_client() {
        let name = unique("two-instances");
        let _a = RawInstance::create(&name, 2);
        let _b = RawInstance::create(&name, 2);

        let first = Client::connect(&name).expect("the first client should connect");
        let second = Client::connect(&name).expect(
            "with two instances created, the second client must connect too — \
             if this fails, the busy test above is not measuring availability",
        );
        assert!(!first.file().as_raw_handle().is_invalid());
        assert!(!second.file().as_raw_handle().is_invalid());
    }

    /// A missing pipe is not a busy pipe.
    ///
    /// These two are the failures a caller most needs to tell apart, because
    /// one says "retry shortly" and the other says "nothing is listening". If
    /// `ERROR_FILE_NOT_FOUND` were folded into `PipeBusy`, a client would retry
    /// forever against a server that was never started.
    #[test]
    fn connecting_to_a_pipe_that_does_not_exist_is_not_reported_as_busy() {
        let err = Client::connect(unique("absent"))
            .expect_err("connecting to a pipe nobody created should fail");
        assert!(
            !matches!(err, Error::Busy),
            "a missing pipe must not be reported as busy — a caller would retry \
             forever. Got {err:?}"
        );
        assert!(
            matches!(err, Error::Other(_)),
            "expected the platform's own error for a missing pipe, got {err:?}"
        );
    }

    /// The client's handle must be overlapped.
    ///
    /// A synchronous handle does not fail; it serialises, which is the quietest
    /// way to lose concurrency. `file.rs` documents that explicitly, so a
    /// behavioural test that submits work and watches for a stall could not
    /// distinguish the two — the mode is read back directly instead.
    #[cfg(feature = "handle-mode-query")]
    #[test]
    fn the_clients_handle_is_overlapped_not_synchronous() {
        let name = unique("overlapped");
        let _instance = RawInstance::create(&name, 1);
        let client = Client::connect(&name).unwrap();

        assert!(
            !client.file().is_synchronous().unwrap(),
            "the client must open with FILE_FLAG_OVERLAPPED; a synchronous \
             handle serialises through the ring rather than failing, so nothing \
             else would report this"
        );
    }
}
