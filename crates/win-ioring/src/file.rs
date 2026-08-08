//! File handles usable with an IoRing.
//!
//! An operation can outlive both the future awaiting it and the caller's own
//! reference to the file, because the kernel keeps working until the operation
//! completes. The handle must therefore stay open for at least that long.
//!
//! [`File`] holds its state behind an [`Rc`], and submitting an operation clones
//! that handle into the driver's storage. The underlying OS handle is closed
//! only once the caller's `File`, and every operation naming it, are gone.
//!
//! This is also why the safe API never accepts a bare `HANDLE`: no safe
//! signature could promise that a raw handle outlives an operation whose future
//! was dropped. Raw handles remain available through [`crate::io_ring`], which
//! is unsafe and documents the obligation.

use std::cell::Cell;
use std::future::Future;
use std::marker::PhantomData;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    FILE_FLUSH_MODE, FILE_TYPE_CHAR, FILE_TYPE_PIPE, GetFileType,
};

/// `FILE_FLAG_OVERLAPPED`, as a plain `u32` for `OpenOptionsExt::custom_flags`.
///
/// Named from the `windows` constant rather than written as a literal, so a
/// change in the crate's value cannot silently desynchronise this from it.
const FILE_FLAG_OVERLAPPED: u32 = windows::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED.0;

use crate::buf::{BufResult, IoBuf, IoBufMut};
use crate::error::Error;
use crate::io_ring::ops::SqeFlags;
use crate::runtime::{FlushFuture, Handle, OperationId, ReadFuture, WriteFuture};

/// State shared between a [`File`] and any operations naming it.
///
/// The cursor and the outstanding-operation flag live here rather than in
/// [`File`] because the driver must be able to clear the flag on terminal
/// completion, which can happen long after the caller dropped both the future
/// and its `File`.
#[derive(Debug)]
pub struct FileState {
    handle: OwnedHandle,
    /// Where the next sequential operation starts.
    cursor: Cell<u64>,
    /// Whether a sequential operation is outstanding in the kernel.
    ///
    /// Exclusive access to the `File` is not enough on its own: dropping a
    /// sequential operation's future releases that access while the kernel is
    /// still working, so a second operation could otherwise start against a
    /// cursor position the first is about to consume.
    sequential_outstanding: Cell<bool>,
    /// What `GetFileType` said about this handle, cached after the first
    /// sequential use.
    ///
    /// Populated lazily rather than at construction, and that is a deliberate
    /// trade rather than an accident of implementation. Opening a file sits
    /// inside the timed region of the warm-cache arm of the published benchmark
    /// matrix, so a syscall in the constructor would disturb fifty cells for the
    /// benefit of a path the benchmark never takes. Caching on first sequential
    /// use costs the same query once, on a call that is already making a
    /// syscall, and only for callers who use the sequential API at all.
    ///
    /// Caching at all is sound because a handle names one kernel object for its
    /// whole life and that object's type cannot change underneath it.
    file_type: Cell<Option<u32>>,
}

impl FileState {
    /// Returns the raw OS handle.
    ///
    /// The handle is valid for as long as this state is alive, which the
    /// reference count guarantees.
    pub fn raw_handle(&self) -> HANDLE {
        HANDLE(self.handle.as_raw_handle())
    }

    /// Marks a sequential operation as no longer outstanding.
    pub(crate) fn clear_sequential(&self) {
        self.sequential_outstanding.set(false);
    }

    /// Refuses the sequential API on a handle that has no file offset.
    ///
    /// Returns `Ok(())` when the sequential contract is meaningful for this
    /// handle, and the typed error when it is not.
    ///
    /// The guard rejects two of the platform's four handle kinds and permits the
    /// other two, and the shape of that choice matters more than the list. It is
    /// **fail-open**: only `FILE_TYPE_PIPE` and `FILE_TYPE_CHAR` are refused, and
    /// anything else — including `FILE_TYPE_UNKNOWN`, which is also what the
    /// platform returns on a failed query — is permitted. A guard that failed
    /// closed would newly break handle kinds nobody anticipated, which is a worse
    /// trade than leaving them exactly where they are today.
    ///
    /// `FILE_TYPE_CHAR` is refused even though no pipe work requires it, because
    /// the fail-open rationale does not cover it: a character device is a named
    /// member of the very enumeration this reads, it is reachable through
    /// [`File::open`], and the crate's offset contract is as meaningless for it
    /// as for a pipe. That is a real cost to a downstream consumer doing
    /// sequential I/O on a console handle, and it is paid to avoid a silent wrong
    /// answer on the same handle kind.
    fn check_has_file_offset(&self) -> std::result::Result<(), Error> {
        let file_type = match self.file_type.get() {
            Some(cached) => cached,
            None => {
                let queried = query_file_type(self.raw_handle());
                self.file_type.set(Some(queried));
                queried
            }
        };

        if file_type == FILE_TYPE_PIPE.0 || file_type == FILE_TYPE_CHAR.0 {
            Err(Error::NoFileOffset { file_type })
        } else {
            Ok(())
        }
    }
}

// Test seam: the handle type the next query reports, instead of the platform's.
//
// `FILE_TYPE_UNKNOWN` cannot be produced on demand from a real handle, and the
// crate opens no character device of its own — `NUL` is reachable but returns
// end-of-file to every read, so it could not distinguish a working guard from a
// broken one. Both of those cases are requirements, so both need a way to be
// asserted, and injection is the only one available. This follows the form
// `FAIL_NEXT_ARM` established in `sys/event.rs` for the same reason.
#[cfg(test)]
thread_local! {
    static FORCE_FILE_TYPE: Cell<Option<u32>> = const { Cell::new(None) };
}

/// Sets the type the next handle-type query will report.
#[cfg(test)]
pub(crate) fn force_next_file_type(file_type: u32) {
    FORCE_FILE_TYPE.with(|c| c.set(Some(file_type)));
}

/// Asks the platform what kind of handle this is.
///
/// `GetFileType` returns `FILE_TYPE_UNKNOWN` both for a handle it cannot
/// classify and for a call that failed, and the two are told apart by
/// `GetLastError`. This does not tell them apart, deliberately: both mean the
/// same thing to the caller — that nothing is known — and the guard fails open
/// on that, so distinguishing them would change no decision.
fn query_file_type(handle: HANDLE) -> u32 {
    #[cfg(test)]
    if let Some(forced) = FORCE_FILE_TYPE.with(|c| c.take()) {
        return forced;
    }
    // SAFETY: `handle` comes from this state's `OwnedHandle`, which keeps it open
    // for the duration of this call. `GetFileType` reads the handle's type and
    // writes nothing, so it needs no further guarantee about ownership.
    unsafe { GetFileType(handle).0 }
}

/// Clears a file's outstanding-sequential flag when the operation ends.
///
/// The driver keeps this in the operation's payload, so the flag clears on
/// terminal completion whether or not the future survived to see it. Teardown
/// drains rather than abandoning payloads, so the flag is cleared there too —
/// by which point the kernel is provably finished with the operation, which is
/// the condition that made holding the flag necessary in the first place.
#[derive(Debug)]
pub(crate) struct SequentialGuard(Rc<FileState>);

impl SequentialGuard {
    /// Claims the sequential slot, or reports that it is already taken.
    pub(crate) fn claim(state: &Rc<FileState>) -> Option<Self> {
        if state.sequential_outstanding.replace(true) {
            None
        } else {
            Some(Self(Rc::clone(state)))
        }
    }
}

impl Drop for SequentialGuard {
    fn drop(&mut self) {
        self.0.clear_sequential();
    }
}

/// A file that IoRing operations can target.
///
/// Cloning is cheap and shares the same underlying handle.
#[derive(Debug, Clone)]
pub struct File {
    state: Rc<FileState>,
}

impl File {
    /// Reads back whether this handle serialises I/O at the file object.
    ///
    /// Available only with the `handle-mode-query` feature, which is off by
    /// default because reaching `NtQueryInformationFile` costs three additional
    /// `windows` namespaces in every downstream consumer's compile. See the
    /// feature's comment in this crate's `Cargo.toml`.
    ///
    /// Handles from [`File::open`] and [`File::create`] are always overlapped,
    /// so this is only informative for handles adopted through
    /// [`File::from_std`] or [`File::from_raw_handle`].
    ///
    /// # Errors
    ///
    /// Returns the underlying error if the mode could not be read. **A failure
    /// is not "the handle is overlapped".** The distinction matters: a caller
    /// checking whether it is about to serialise its I/O must be able to tell
    /// "no" from "could not tell", so this deliberately does not return a bare
    /// `bool`.
    #[cfg(feature = "handle-mode-query")]
    pub fn is_synchronous(&self) -> std::io::Result<bool> {
        use windows::Wdk::Storage::FileSystem::{FileModeInformation, NtQueryInformationFile};
        use windows::Win32::System::IO::IO_STATUS_BLOCK;

        /// `FILE_SYNCHRONOUS_IO_NONALERT` — set when the file object serialises
        /// operations and maintains a kernel file pointer.
        const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x0000_0020;

        let mut mode: u32 = 0;
        let mut iosb = IO_STATUS_BLOCK::default();
        // SAFETY: `mode` outlives the call and its size is passed correctly;
        // the handle is owned by `self` and open for the duration.
        let status = unsafe {
            NtQueryInformationFile(
                windows::Win32::Foundation::HANDLE(self.state.handle.as_raw_handle()),
                &mut iosb,
                std::ptr::from_mut(&mut mode).cast(),
                u32::try_from(size_of::<u32>()).expect("4 fits in u32"),
                FileModeInformation,
            )
        };
        if status.is_err() {
            return Err(std::io::Error::other(format!(
                "NtQueryInformationFile(FileModeInformation) failed: {status:?}"
            )));
        }
        Ok(mode & FILE_SYNCHRONOUS_IO_NONALERT != 0)
    }

    /// Adopts an already-open standard library file.
    ///
    /// Ownership transfers: the handle is closed when the last reference to it
    /// goes away, which includes any operation still in flight.
    ///
    /// # This accepts a synchronous handle
    ///
    /// Unlike [`File::open`], this cannot guarantee the handle is overlapped —
    /// adopting a caller-provided handle is the whole point. A synchronous
    /// handle serialises at the file object regardless of the depth submitted.
    /// With the `handle-mode-query` feature, `File::is_synchronous` reports
    /// which kind this is.
    pub fn from_std(file: std::fs::File) -> Self {
        Self {
            state: Rc::new(FileState {
                handle: OwnedHandle::from(file),
                cursor: Cell::new(0),
                sequential_outstanding: Cell::new(false),
                file_type: Cell::new(None),
            }),
        }
    }

    /// Opens a file for reading.
    ///
    /// # This produces an *overlapped* handle
    ///
    /// The handle is opened with `FILE_FLAG_OVERLAPPED`. This is not a default
    /// that a caller can talk this function out of: there is no parameter and
    /// no opt-out, following the same reasoning as `compio`, whose
    /// `OpenOptions` OR-s the flag in unconditionally and keeps `from_std`
    /// private so no public route to a non-overlapped file exists.
    ///
    /// # What the flag buys
    ///
    /// Without it, Windows creates the handle with
    /// `FILE_SYNCHRONOUS_IO_NONALERT` and **the file object serialises I/O**:
    /// at most one operation is in flight against it at a time, no matter how
    /// many this crate submits to the ring. Submitting at depth 64 against such
    /// a handle yields a depth of one.
    ///
    /// The effect is invisible under a warm page cache, where a cached read
    /// returns synchronously after a memory copy and there is nothing to
    /// overlap. **That is now a measurement rather than an argument**: a
    /// pre-registered A/B running both handle modes in the same benchmark run,
    /// over two independent five-run sets, found no effect of the predicted
    /// size in any of the eight cells it was frozen over (`docs/performance.md`,
    /// "Handle mode"). What is excluded is an effect of the predicted
    /// magnitude, not any effect at all.
    ///
    /// It becomes decisive as soon as reads reach the device — the
    /// unbuffered arm measures the same mechanism at 8.75x to 10.27x, by
    /// handle count rather than handle mode (see `docs/performance.md`).
    ///
    /// # This handle has no kernel file pointer
    ///
    /// Overlapped handles do not maintain one. This crate does not need it —
    /// it tracks its own cursor and passes an explicit offset on every
    /// operation — but [`File::as_raw_handle`] is public, so a caller who takes
    /// the raw handle elsewhere and issues a *pointer-relative* `ReadFile`
    /// against it will not get the behaviour a synchronous handle would have
    /// given.
    ///
    /// Two concrete consequences for such a caller, both measured rather than
    /// inferred:
    ///
    /// - A synchronous `ReadFile`/`WriteFile` with a null `OVERLAPPED` fails
    ///   with `ERROR_INVALID_PARAMETER` (87). This is what `std::io::Read` and
    ///   `std::io::Write` issue, so wrapping this handle back into a
    ///   [`std::fs::File`] and reading from it will fail. It is not a silent
    ///   wrong answer — it is a clean error — but it is an error where a
    ///   synchronous handle succeeded.
    /// - `std`'s positional `seek_read`/`seek_write` are unsafe to use
    ///   concurrently against an overlapped handle: they require the operation
    ///   to complete inline and abort the process if the kernel returns
    ///   `STATUS_PENDING`, which a real device read does.
    ///
    /// Operations issued through this crate are unaffected: they always carry an
    /// explicit offset and are completed through the ring.
    ///
    /// # Getting a synchronous handle
    ///
    /// If you deliberately want one, open the file yourself and adopt it with
    /// [`File::from_std`]:
    ///
    /// ```no_run
    /// // `std::fs::File::open` does not pass FILE_FLAG_OVERLAPPED, so the
    /// // handle it returns is synchronous.
    /// let std_file = std::fs::File::open("data.bin")?;
    /// let file = win_ioring::file::File::from_std(std_file);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn open(path: impl AsRef<std::path::Path>) -> std::io::Result<Self> {
        use std::os::windows::fs::OpenOptionsExt;

        Ok(Self::from_std(
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(FILE_FLAG_OVERLAPPED)
                .open(path)?,
        ))
    }

    /// Creates or truncates a file for writing.
    ///
    /// Produces an **overlapped** handle, with the same guarantee and the same
    /// consequences described on [`File::open`].
    pub fn create(path: impl AsRef<std::path::Path>) -> std::io::Result<Self> {
        use std::os::windows::fs::OpenOptionsExt;

        Ok(Self::from_std(
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .custom_flags(FILE_FLAG_OVERLAPPED)
                .open(path)?,
        ))
    }

    /// Adopts a raw OS handle.
    ///
    /// # This accepts a synchronous handle
    ///
    /// As with [`File::from_std`], and for the same reason: adopting a
    /// caller-provided handle means accepting whatever mode it was opened in.
    /// If `handle` lacks `FILE_FLAG_OVERLAPPED` the file object serialises, so
    /// operations submitted at depth 64 complete one at a time however many the
    /// ring accepts. Open with `FILE_FLAG_OVERLAPPED` to avoid this, or use
    /// [`File::open`], which sets it. With the `handle-mode-query` feature,
    /// `File::is_synchronous` reports which kind this is.
    ///
    /// # Safety
    ///
    /// `handle` must be a valid, open handle that this `File` may take sole
    /// ownership of, and which is not owned by anything else.
    pub unsafe fn from_raw_handle(handle: HANDLE) -> Self {
        Self {
            state: Rc::new(FileState {
                // SAFETY: the caller guarantees `handle` is open and owned by
                // nothing else, so this `OwnedHandle` becomes its sole owner.
                handle: unsafe { OwnedHandle::from_raw_handle(handle.0) },
                cursor: Cell::new(0),
                sequential_outstanding: Cell::new(false),
                file_type: Cell::new(None),
            }),
        }
    }

    /// Returns the shared state, for the driver to retain alongside an
    /// operation.
    pub(crate) fn state(&self) -> Rc<FileState> {
        Rc::clone(&self.state)
    }

    /// Returns the raw OS handle.
    ///
    /// Prefer passing the `File` itself to an operation. This is exposed for
    /// building operations against the raw layer, where the caller takes on the
    /// obligation of keeping the file alive.
    pub fn as_raw_handle(&self) -> HANDLE {
        self.state.raw_handle()
    }

    /// Returns the number of live references to this file's handle.
    ///
    /// Counts the caller's own `File` values and any operations the driver is
    /// still tracking.
    pub fn reference_count(&self) -> usize {
        Rc::strong_count(&self.state)
    }

    /// Returns where the next sequential operation will start.
    pub fn cursor(&self) -> u64 {
        self.state.cursor.get()
    }

    /// Moves the cursor.
    ///
    /// This takes exclusive access, so it cannot race a sequential operation's
    /// own future. It can still be called while an operation dropped mid-flight
    /// is outstanding; that operation will not advance the cursor when it
    /// completes, so the position set here is the one that stands.
    pub fn set_cursor(&mut self, position: u64) {
        self.state.cursor.set(position);
    }

    /// Reports whether a sequential operation is still outstanding.
    ///
    /// This can be true while no future is alive, if a sequential operation's
    /// future was dropped before its operation completed. Sequential operations
    /// are refused until it clears.
    pub fn sequential_outstanding(&self) -> bool {
        self.state.sequential_outstanding.get()
    }

    /// Reads `len` bytes at `offset` into `buffer`, without touching the cursor.
    ///
    /// Positional operations need only shared access, so any number of them may
    /// be in flight against the same file at once.
    pub fn read_at<B: IoBufMut>(
        &self,
        handle: &Handle,
        buffer: B,
        len: u32,
        offset: u64,
    ) -> ReadFuture<B> {
        handle.read(self, buffer, len, offset)
    }

    /// Writes `len` bytes from `buffer` at `offset`, without touching the
    /// cursor.
    pub fn write_at<B: IoBuf>(
        &self,
        handle: &Handle,
        buffer: B,
        len: u32,
        offset: u64,
    ) -> WriteFuture<B> {
        handle.write(self, buffer, len, offset)
    }

    /// Reads `len` bytes from the cursor into `buffer`, advancing the cursor.
    ///
    /// Exclusive access stops two sequential futures existing at once. It does
    /// not stop a *dropped* future's operation from still being outstanding, so
    /// this also fails with [`Error::OperationOutstanding`] until that one
    /// completes.
    ///
    /// The cursor advances by exactly the number of bytes transferred, and only
    /// when this future observes the completion: dropping it leaves the cursor
    /// where it was.
    ///
    /// # Handles with no file offset
    ///
    /// This fails with [`Error::NoFileOffset`] on a pipe or a character device.
    /// Those handles have no file offset, so the cursor this method maintains
    /// describes nothing: the platform ignores the offset and consumes from the
    /// head of the stream. Were the operation permitted, every read after the
    /// first *would* return `Ok` paired with bytes that did not come from where
    /// the cursor says — success, with the wrong bytes, on a caller doing nothing
    /// unusual. The refusal exists so that never happens.
    ///
    /// Use [`Handle::read`] with an explicit offset instead. The platform ignores
    /// that offset too, but the caller supplied it knowingly rather than having
    /// the crate supply a meaningless one on their behalf.
    pub fn read<'a, B: IoBufMut>(
        &'a mut self,
        handle: &Handle,
        buffer: B,
        len: u32,
    ) -> SequentialRead<'a, B> {
        if let Err(error) = self.state.check_has_file_offset() {
            return SequentialRead {
                inner: ReadFuture::failed(error, buffer),
                state: Rc::clone(&self.state),
                _file: PhantomData,
            };
        }
        let offset = self.state.cursor.get();
        let guard = match SequentialGuard::claim(&self.state) {
            Some(guard) => guard,
            None => {
                return SequentialRead {
                    inner: ReadFuture::failed(Error::OperationOutstanding, buffer),
                    state: Rc::clone(&self.state),
                    _file: PhantomData,
                };
            }
        };
        SequentialRead {
            inner: handle.read_sequential(self, buffer, len, offset, guard),
            state: Rc::clone(&self.state),
            _file: PhantomData,
        }
    }

    /// Writes `len` bytes from `buffer` at the cursor, advancing the cursor.
    ///
    /// The same exclusivity and cursor rules apply as for [`File::read`], and so
    /// does its refusal: this fails with [`Error::NoFileOffset`] on a pipe or a
    /// character device, because the offset it would supply describes nothing
    /// there. A permitted write would append regardless of the cursor and report
    /// success, leaving the cursor claiming a position the stream does not have.
    /// Use [`Handle::write`] with an explicit offset instead.
    pub fn write<'a, B: IoBuf>(
        &'a mut self,
        handle: &Handle,
        buffer: B,
        len: u32,
    ) -> SequentialWrite<'a, B> {
        if let Err(error) = self.state.check_has_file_offset() {
            return SequentialWrite {
                inner: WriteFuture::failed(error, buffer),
                state: Rc::clone(&self.state),
                _file: PhantomData,
            };
        }
        let offset = self.state.cursor.get();
        let guard = match SequentialGuard::claim(&self.state) {
            Some(guard) => guard,
            None => {
                return SequentialWrite {
                    inner: WriteFuture::failed(Error::OperationOutstanding, buffer),
                    state: Rc::clone(&self.state),
                    _file: PhantomData,
                };
            }
        };
        SequentialWrite {
            inner: handle.write_sequential(self, buffer, len, offset, guard),
            state: Rc::clone(&self.state),
            _file: PhantomData,
        }
    }

    /// Flushes the file, using the platform's default flush mode.
    pub fn flush(&self, handle: &Handle) -> FlushFuture {
        handle.flush(self)
    }

    /// Flushes the file with an explicit flush mode.
    pub fn flush_with_mode(&self, handle: &Handle, mode: FILE_FLUSH_MODE) -> FlushFuture {
        handle.flush_with_options(self, mode, SqeFlags::NONE)
    }
}

/// A sequential read in progress.
///
/// Borrows the file exclusively, so a second sequential operation cannot be
/// started while this exists.
pub struct SequentialRead<'a, B: IoBufMut> {
    inner: ReadFuture<B>,
    state: Rc<FileState>,
    _file: PhantomData<&'a mut File>,
}

impl<B: IoBufMut> SequentialRead<'_, B> {
    /// Returns this operation's identifier, for cancellation.
    ///
    /// Absent if the operation was rejected before reaching the kernel.
    pub fn operation_id(&self) -> Option<OperationId> {
        self.inner.operation_id()
    }
}

impl<B: IoBufMut> Future for SequentialRead<'_, B> {
    type Output = BufResult<u32, B>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let outcome = std::task::ready!(Pin::new(&mut self.inner).poll(cx));
        advance_on_success(&self.state, &outcome);
        Poll::Ready(outcome)
    }
}

/// A sequential write in progress.
///
/// Borrows the file exclusively, as [`SequentialRead`] does.
pub struct SequentialWrite<'a, B: IoBuf> {
    inner: WriteFuture<B>,
    state: Rc<FileState>,
    _file: PhantomData<&'a mut File>,
}

impl<B: IoBuf> SequentialWrite<'_, B> {
    /// Returns this operation's identifier, for cancellation.
    ///
    /// Absent if the operation was rejected before reaching the kernel.
    pub fn operation_id(&self) -> Option<OperationId> {
        self.inner.operation_id()
    }
}

impl<B: IoBuf> Future for SequentialWrite<'_, B> {
    type Output = BufResult<u32, B>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let outcome = std::task::ready!(Pin::new(&mut self.inner).poll(cx));
        advance_on_success(&self.state, &outcome);
        Poll::Ready(outcome)
    }
}

/// Advances the cursor by the bytes a completed sequential operation moved.
///
/// A failure transfers nothing, so it leaves the cursor alone. So does a
/// zero-byte transfer, trivially.
fn advance_on_success<B>(state: &FileState, outcome: &BufResult<u32, B>) {
    if let Ok(transferred) = &outcome.result {
        state
            .cursor
            .set(state.cursor.get().saturating_add(u64::from(*transferred)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(tag: &str) -> (std::path::PathBuf, std::fs::File) {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "win-ioring-file-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let file = std::fs::File::create(&path).unwrap();
        (path, file)
    }

    /// `is_synchronous` must answer correctly in **both** directions.
    ///
    /// A one-directional test here would be the gate that cannot fail: a body
    /// hardcoded to `Ok(false)` satisfies any number of assertions that
    /// `File::open` is not synchronous. The adopted handle is the twin that
    /// makes the negative attributable, and it is deliberately opened through
    /// plain `std::fs::File::open` so the two differ in exactly one respect.
    #[cfg(feature = "handle-mode-query")]
    #[test]
    fn is_synchronous_distinguishes_both_handle_modes() {
        let (path, _std_file) = temp_file("issync");

        let overlapped = File::open(&path).unwrap();
        assert!(
            !overlapped.is_synchronous().unwrap(),
            "File::open produced a synchronous handle, or is_synchronous \
             misreports overlapped handles"
        );

        let adopted = File::from_std(std::fs::File::open(&path).unwrap());
        assert!(
            adopted.is_synchronous().unwrap(),
            "a handle adopted from plain std::fs::File::open did not report as \
             synchronous, so is_synchronous cannot distinguish the two modes \
             and the assertion above proves nothing"
        );

        drop((overlapped, adopted));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn adopting_a_std_file_takes_ownership() {
        let (path, std_file) = temp_file("adopt");
        let file = File::from_std(std_file);
        assert_eq!(file.reference_count(), 1);
        assert!(!file.as_raw_handle().is_invalid());
        drop(file);
        let _ = std::fs::remove_file(&path);
    }

    /// Cloning shares one handle, so an operation holding a clone keeps the
    /// handle open after the caller drops theirs.
    #[test]
    fn clones_share_one_handle() {
        let (path, std_file) = temp_file("clone");
        let file = File::from_std(std_file);
        let retained = file.state();

        assert_eq!(file.reference_count(), 2);
        let raw = file.as_raw_handle();

        // The caller drops their reference while the "operation" still holds one.
        drop(file);
        assert_eq!(Rc::strong_count(&retained), 1);
        assert_eq!(retained.raw_handle().0, raw.0, "handle changed identity");

        drop(retained);
        let _ = std::fs::remove_file(&path);
    }

    /// The handle must stay open while any operation still references it, and
    /// its state must be released exactly when the last reference goes.
    ///
    /// The release itself is `OwnedHandle`'s job; what this crate is
    /// responsible for is the reference counting that decides *when*. Probing
    /// the OS for whether a specific handle value is closed would be racy in a
    /// multi-threaded test process, because another test can be handed the same
    /// value moments later.
    #[test]
    fn handle_lives_exactly_as_long_as_its_references() {
        use std::rc::Weak;

        let (path, std_file) = temp_file("close");
        let file = File::from_std(std_file);
        let retained = file.state();
        let observer: Weak<FileState> = Rc::downgrade(&retained);

        assert_eq!(file.reference_count(), 2);

        // The caller drops theirs; the operation's reference keeps it alive.
        drop(file);
        assert!(
            observer.upgrade().is_some(),
            "state released while an operation still referenced it"
        );

        drop(retained);
        assert!(
            observer.upgrade().is_none(),
            "state outlived its last reference"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn open_and_create_round_trip() {
        let mut path = std::env::temp_dir();
        path.push(format!("win-ioring-file-open-{}", std::process::id()));
        std::fs::write(&path, b"hello").unwrap();

        let file = File::open(&path).unwrap();
        assert!(!file.as_raw_handle().is_invalid());
        drop(file);

        let created = File::create(&path).unwrap();
        assert!(!created.as_raw_handle().is_invalid());
        drop(created);

        let _ = std::fs::remove_file(&path);
    }
}
