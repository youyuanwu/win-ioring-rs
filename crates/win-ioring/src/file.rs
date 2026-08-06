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
use windows::Win32::Storage::FileSystem::FILE_FLUSH_MODE;

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
    /// Adopts an already-open standard library file.
    ///
    /// Ownership transfers: the handle is closed when the last reference to it
    /// goes away, which includes any operation still in flight.
    pub fn from_std(file: std::fs::File) -> Self {
        Self {
            state: Rc::new(FileState {
                handle: OwnedHandle::from(file),
                cursor: Cell::new(0),
                sequential_outstanding: Cell::new(false),
            }),
        }
    }

    /// Opens a file for reading.
    ///
    /// # This produces a *synchronous* handle
    ///
    /// The handle comes from [`std::fs::File::open`], which does not pass
    /// `FILE_FLAG_OVERLAPPED`. Windows therefore creates it with
    /// `FILE_SYNCHRONOUS_IO_NONALERT`, and **the file object serialises I/O**:
    /// at most one operation is in flight against it at a time, no matter how
    /// many this crate submits to the ring.
    ///
    /// That is a real limitation of this constructor, not a detail. Submitting
    /// at depth 64 against such a handle yields a depth of one — a consequence
    /// of the serialisation the kernel performs at the file object, argued from
    /// the mechanism rather than measured here.
    ///
    /// The effect is invisible under a warm page cache, where a cached read
    /// returns synchronously after a memory copy and there is nothing to
    /// overlap. It becomes decisive as soon as reads reach the device. (The
    /// warm-cache half of that is a mechanism argument, not a measurement: no
    /// A/B on the flag under a warm cache has been run.)
    ///
    /// # Getting an overlapped handle
    ///
    /// Open the file yourself and adopt it with [`File::from_std`]:
    ///
    /// ```no_run
    /// use std::os::windows::fs::OpenOptionsExt;
    ///
    /// // FILE_FLAG_OVERLAPPED
    /// const OVERLAPPED: u32 = 0x4000_0000;
    ///
    /// let std_file = std::fs::OpenOptions::new()
    ///     .read(true)
    ///     .custom_flags(OVERLAPPED)
    ///     .open("data.bin")?;
    /// let file = win_ioring::file::File::from_std(std_file);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    ///
    /// Whether this function should set the flag itself is an open question,
    /// which `docs/pending-work.md` will record with its cost. It is not
    /// changed here because the twenty `win-ioring` cells of the fifty in the
    /// published matrix (see "Full result" in `docs/performance.md`) were all
    /// measured through handles from this function and from [`File::create`],
    /// and that matrix is a single-run artefact that is never patched from a
    /// second run — so re-measuring any part of it means re-running all fifty.
    pub fn open(path: impl AsRef<std::path::Path>) -> std::io::Result<Self> {
        Ok(Self::from_std(std::fs::File::open(path)?))
    }

    /// Creates or truncates a file for writing.
    ///
    /// Produces a **synchronous** handle, with the same consequences described
    /// on [`File::open`]: the file object serialises I/O regardless of the
    /// depth submitted. To obtain one that does not, open the file yourself
    /// with `FILE_FLAG_OVERLAPPED` alongside the write access this function
    /// implies — `.write(true).create(true).truncate(true)` — and adopt it with
    /// [`File::from_std`].
    pub fn create(path: impl AsRef<std::path::Path>) -> std::io::Result<Self> {
        Ok(Self::from_std(std::fs::File::create(path)?))
    }

    /// Adopts a raw OS handle.
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
    pub fn read<'a, B: IoBufMut>(
        &'a mut self,
        handle: &Handle,
        buffer: B,
        len: u32,
    ) -> SequentialRead<'a, B> {
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
    /// The same exclusivity and cursor rules apply as for [`File::read`].
    pub fn write<'a, B: IoBuf>(
        &'a mut self,
        handle: &Handle,
        buffer: B,
        len: u32,
    ) -> SequentialWrite<'a, B> {
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
