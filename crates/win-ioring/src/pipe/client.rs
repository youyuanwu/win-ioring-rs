//! The client end of a named pipe.

use crate::file::File;

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
/// # Ok::<(), win_ioring::Error>(())
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
    /// clients, this returns [`Error::PipeBusy`](crate::Error::PipeBusy)
    /// immediately. It does **not** block, and there is no equivalent of Win32's
    /// `WaitNamedPipe`.
    ///
    /// That is a deliberate omission rather than an oversight. This crate is
    /// runtime-agnostic and single-threaded; a blocking wait would stall the
    /// caller's executor, and a timed retry would need a timer this crate does
    /// not have and should not pick for the caller. Retrying on
    /// `Error::PipeBusy` — with whatever backoff and whatever timer the caller's
    /// runtime provides — is the intended pattern.
    ///
    /// If the server has not created the pipe at all, the error is
    /// [`Error::Os`](crate::Error::Os) carrying `ERROR_FILE_NOT_FOUND`, which is
    /// a different condition and deliberately not folded into `PipeBusy`: one
    /// says "come back shortly", the other says "nothing is listening here".
    pub fn open(&self, name: impl AsRef<str>) -> crate::Result<Client> {
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
/// Routes through [`Error::from_hresult`](crate::Error), which is the same
/// funnel every ring completion passes through, rather than repeating the code
/// comparisons here. That matters more than it looks: `ERROR_PIPE_BUSY` from a
/// failed open and `ERROR_PIPE_BUSY` from a completion must produce the same
/// variant, and two independent match arms are exactly how that stops being
/// true after someone edits one of them.
///
/// An `io::Error` with no OS code cannot come from `CreateFileW`, but the type
/// permits it, so it is reported verbatim rather than being mapped to a pipe
/// condition it is not.
fn classify_open_failure(e: &std::io::Error) -> crate::Error {
    match e.raw_os_error() {
        Some(code) => crate::Error::from_hresult(windows::core::HRESULT::from_win32(code as u32)),
        None => crate::Error::Os(windows::core::Error::from(
            windows::Win32::Foundation::E_FAIL,
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
    pub fn connect(name: impl AsRef<str>) -> crate::Result<Self> {
        ClientOptions::new().open(name)
    }

    /// The underlying file, for reads, writes and flushes through the ring.
    pub fn file(&self) -> &File {
        &self.file
    }

    /// Consumes the client and returns the file it wraps.
    ///
    /// The pipe stays open; only this wrapper goes away. Useful when the pipe's
    /// identity as a pipe stops mattering and it is just a byte stream.
    pub fn into_file(self) -> File {
        self.file
    }
}

impl std::ops::Deref for Client {
    type Target = File;

    fn deref(&self) -> &File {
        &self.file
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
            matches!(second, Err(crate::Error::PipeBusy)),
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
            !matches!(err, crate::Error::PipeBusy),
            "a missing pipe must not be reported as busy — a caller would retry \
             forever. Got {err:?}"
        );
        assert!(
            matches!(err, crate::Error::Os(_)),
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
