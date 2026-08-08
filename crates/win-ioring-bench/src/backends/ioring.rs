//! This crate's backend, in both of its forms.
//!
//! `IoRingPlain` uses caller-owned buffers and unregistered handles — the
//! ordinary path any application gets without opting into anything.
//!
//! `IoRingRegistered` uses registered buffers and registered file handles, and
//! calls the operations that *name* the registration. That those are distinct
//! functions is what makes "the registered backend really used the registered
//! path" a property of the call site rather than something needing runtime
//! telemetry to confirm.
//!
//! Both are driven by the workspace's own single-threaded executor, because
//! everything here is `!Send` and the driver must live on the measuring thread.

use std::cell::RefCell;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use win_ioring::file::File;
use win_ioring::io_ring::IoRing;
use win_ioring::runtime::{Driver, FileTarget, Handle, RegisteredBuf, RegisteredBuffers};

use crate::backend::{Availability, Backend, Buffer, OpResult};

/// How many drivers this process has built.
///
/// An observation seam rather than a test fixture, which is why it is not
/// `#[cfg(test)]`: SC-014 asks a full release run to report the figure, and the
/// property it settles — one driver per measured combination, not one per
/// iteration — is only interesting about a real run.
static DRIVERS_BUILT: AtomicUsize = AtomicUsize::new(0);

/// How many drivers have been built since this process started.
///
/// Process-global and monotonic. A caller comparing two readings gets the number
/// built between them *by every thread*, which is why the test that reads it
/// asserts a lower bound rather than an exact delta.
pub fn drivers_built() -> usize {
    DRIVERS_BUILT.load(Ordering::Relaxed)
}

/// Records that a driver was built.
fn count_driver() {
    DRIVERS_BUILT.fetch_add(1, Ordering::Relaxed);
}

impl Buffer for RegisteredBuf {
    fn bytes(&self) -> &[u8] {
        self
    }

    fn fill(&mut self, src: &[u8]) -> io::Result<()> {
        RegisteredBuf::fill(self, src).map_err(io::Error::other)
    }
}

/// Probes whether this host can run the ring-backed backends at all.
pub fn availability() -> Availability {
    match IoRing::builder().build() {
        Ok(mut ring) => {
            let _ = ring.close();
            Availability::Available
        }
        Err(e) => Availability::Unavailable(format!("the platform refused a ring: {e}")),
    }
}

/// The submission queue size to request for a given in-flight depth.
///
/// Headroom above the depth because the same queue also carries cancellations,
/// and a floor because a very small ring is dominated by its own limits rather
/// than by the thing being measured.
fn queue_size(depth: usize) -> u32 {
    (depth.saturating_mul(4).max(128)).min(u32::MAX as usize) as u32
}

/// Which handle mode a ring backend opens its files in.
///
/// This exists for the `handle-mode` arm, which measures the same backends
/// twice — once through each mode — under identical conditions in one run. It
/// is the seam that makes that a paired comparison rather than a comparison
/// against a matrix published on a different day, which the repeat-run analysis
/// in `docs/performance.md` shows would be confounded by between-run drift.
///
/// [`HandleMode::Overlapped`] is what [`win_ioring::file::File::open`] does on
/// its own, so the overlapped side of the A/B exercises the real constructor
/// rather than a reimplementation of it. Only the synchronous side needs its
/// own open path, and it is written to differ in **exactly** the flag: same
/// access, same creation disposition, same everything else. A difference
/// anywhere else would be attributed to handle mode by the experiment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleMode {
    /// `FILE_FLAG_OVERLAPPED`. The crate's default since this arm was added.
    Overlapped,
    /// No `FILE_FLAG_OVERLAPPED`, so the file object serialises. The crate's
    /// default *before* this arm was added, and the configuration all twenty
    /// `win-ioring` cells of the previously published matrix were measured in.
    Synchronous,
}

impl HandleMode {
    /// Opens for reading in this mode.
    ///
    /// # Errors
    ///
    /// Propagates the underlying open failure.
    pub fn open_read(self, path: &Path) -> io::Result<File> {
        use std::os::windows::fs::OpenOptionsExt;

        match self {
            // The real constructor, not a copy of it. If `File::open` changes,
            // this side of the A/B changes with it.
            Self::Overlapped => File::open(path),
            Self::Synchronous => Ok(File::from_std(
                std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(0)
                    .open(path)?,
            )),
        }
    }

    /// Opens for writing in this mode, creating or truncating.
    ///
    /// # Errors
    ///
    /// Propagates the underlying open failure.
    pub fn open_write(self, path: &Path) -> io::Result<File> {
        use std::os::windows::fs::OpenOptionsExt;

        match self {
            Self::Overlapped => File::create(path),
            Self::Synchronous => Ok(File::from_std(
                std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .custom_flags(0)
                    .open(path)?,
            )),
        }
    }

    /// The suffix this mode contributes to a backend's reported name.
    #[must_use]
    pub fn suffix(self) -> &'static str {
        match self {
            Self::Overlapped => "overlapped handle",
            Self::Synchronous => "synchronous handle",
        }
    }
}

/// The ring-backed backend using caller-owned buffers.
pub struct IoRingPlain {
    /// Which handle mode [`Backend::open_read`] and `open_write` produce.
    handle_mode: HandleMode,
    handle: Handle,
    /// Held so the driver outlives every operation; dropped at teardown.
    driver: Option<Driver>,
    /// Pre-allocated so this backend is not charged a per-operation allocation
    /// the registered one avoids.
    buffers: crate::backends::tokio_fs::BufferPool,
}

impl IoRingPlain {
    /// Builds the backend with a ring sized for `depth` operations in flight.
    ///
    /// The submission queue must be able to hold everything outstanding, or the
    /// ring refuses entries and the backend is measured failing rather than
    /// working. Sized generously above the depth because the queue also carries
    /// cancellations and because the platform rounds the request up to a power
    /// of two anyway.
    pub fn new(depth: usize, pool: usize, capacity: usize) -> io::Result<Self> {
        let queue = queue_size(depth);
        let ring = IoRing::builder()
            .with_submission_queue_size(queue)
            .with_completion_queue_size(queue * 2)
            .build()
            .map_err(io::Error::other)?;
        let driver = Driver::new(ring).map_err(io::Error::other)?;
        count_driver();
        let handle = driver.handle();
        Ok(Self {
            handle_mode: HandleMode::Overlapped,
            handle,
            driver: Some(driver),
            buffers: crate::backends::tokio_fs::BufferPool::new(pool, capacity),
        })
    }

    /// Sets the handle mode this backend opens files in.
    ///
    /// Defaults to [`HandleMode::Overlapped`], which is what the published
    /// matrix and every non-experimental caller use. Only the `handle-mode` arm
    /// sets this.
    #[must_use]
    pub fn with_handle_mode(mut self, mode: HandleMode) -> Self {
        self.handle_mode = mode;
        self
    }

    /// Returns the driver, for the caller to spawn alongside its work.
    pub fn take_driver(&mut self) -> Option<Driver> {
        self.driver.take()
    }

    /// Returns a handle, for shutting the driver down when the work is done.
    pub fn handle(&self) -> Handle {
        self.handle.clone()
    }
}

impl Backend for IoRingPlain {
    type Buf = Vec<u8>;
    type File = File;

    fn name(&self) -> String {
        "win-ioring (owned buffers)".to_owned()
    }

    fn configuration(&self) -> String {
        format!(
            "single-threaded driver; caller-owned buffers; unregistered handles; {}",
            self.handle_mode.suffix()
        )
    }

    async fn open_read(&self, path: &Path) -> io::Result<Self::File> {
        self.handle_mode.open_read(path)
    }

    async fn open_write(&self, path: &Path) -> io::Result<Self::File> {
        self.handle_mode.open_write(path)
    }

    fn take_buffer(&self, capacity: usize) -> io::Result<Self::Buf> {
        self.buffers.take(capacity)
    }

    fn put_buffer(&self, buffer: Self::Buf) {
        self.buffers.put(buffer);
    }

    async fn read_at(
        &self,
        file: &Self::File,
        mut buffer: Self::Buf,
        len: u32,
        offset: u64,
    ) -> OpResult<u32, Self::Buf> {
        if buffer.len() < len as usize {
            buffer.resize(len as usize, 0);
        }
        let (result, buffer) = self
            .handle
            .read(file, buffer, len, offset)
            .await
            .into_parts();
        (result.map_err(io::Error::other), buffer)
    }

    async fn write_at(
        &self,
        file: &Self::File,
        buffer: Self::Buf,
        len: u32,
        offset: u64,
    ) -> OpResult<u32, Self::Buf> {
        let (result, buffer) = self
            .handle
            .write(file, buffer, len, offset)
            .await
            .into_parts();
        (result.map_err(io::Error::other), buffer)
    }

    async fn sync(&self, file: &Self::File) -> io::Result<()> {
        file.flush(&self.handle)
            .await
            .map(|_| ())
            .map_err(io::Error::other)
    }
}

/// The ring-backed backend using registered buffers and handles.
pub struct IoRingRegistered {
    /// Which handle mode [`Backend::open_read`] and `open_write` produce.
    handle_mode: HandleMode,
    handle: Handle,
    driver: Option<Driver>,
    /// The registration, once established.
    buffers: RefCell<Option<RegisteredBuffers>>,
    /// Indices not currently checked out, so the pool hands each buffer to one
    /// operation at a time.
    free: RefCell<Vec<u32>>,
    /// The registered file, if the caller registered one.
    registered_file: RefCell<bool>,
}

impl IoRingRegistered {
    /// Builds the backend with a ring sized for `depth` operations in flight.
    ///
    /// The registration is established separately, so its cost can be reported
    /// on its own rather than folded into the per-operation figures.
    pub fn new(depth: usize) -> io::Result<Self> {
        let queue = queue_size(depth);
        let ring = IoRing::builder()
            .with_submission_queue_size(queue)
            .with_completion_queue_size(queue * 2)
            .build()
            .map_err(io::Error::other)?;
        let driver = Driver::new(ring).map_err(io::Error::other)?;
        count_driver();
        let handle = driver.handle();
        Ok(Self {
            handle_mode: HandleMode::Overlapped,
            handle,
            driver: Some(driver),
            buffers: RefCell::new(None),
            free: RefCell::new(Vec::new()),
            registered_file: RefCell::new(false),
        })
    }

    /// Sets the handle mode this backend opens files in.
    ///
    /// Defaults to [`HandleMode::Overlapped`]. Only the `handle-mode` arm sets
    /// this.
    #[must_use]
    pub fn with_handle_mode(mut self, mode: HandleMode) -> Self {
        self.handle_mode = mode;
        self
    }

    /// Returns the driver, for the caller to spawn alongside its work.
    pub fn take_driver(&mut self) -> Option<Driver> {
        self.driver.take()
    }

    /// Returns a handle, for shutting the driver down when the work is done.
    pub fn handle(&self) -> Handle {
        self.handle.clone()
    }

    /// Registers `count` buffers of `capacity` bytes each.
    ///
    /// Timed separately from the operations, because registration is a one-off
    /// whose cost belongs to the caller's decision to register rather than to
    /// any single transfer.
    pub async fn register(&self, count: usize, capacity: usize) -> io::Result<()> {
        let buffers: Vec<Vec<u8>> = (0..count).map(|_| vec![0_u8; capacity]).collect();
        match self.handle.register_buffers(buffers).await {
            win_ioring::runtime::Registered::Ok(collection) => {
                *self.free.borrow_mut() = (0..count as u32).rev().collect();
                *self.buffers.borrow_mut() = Some(collection);
                Ok(())
            }
            win_ioring::runtime::Registered::Failed(e, _) => Err(io::Error::other(e)),
        }
    }

    /// Registers a file handle, so operations can name it by index.
    pub async fn register_file(&self, file: &File) -> io::Result<()> {
        self.handle
            .register_files(std::slice::from_ref(file))
            .await
            .map_err(io::Error::other)?;
        *self.registered_file.borrow_mut() = true;
        Ok(())
    }

    fn target<'a>(&self, file: &'a File) -> FileTarget<'a> {
        if *self.registered_file.borrow() {
            FileTarget::Registered { index: 0 }
        } else {
            FileTarget::Owned(file)
        }
    }
}

impl Backend for IoRingRegistered {
    type Buf = RegisteredBuf;
    type File = File;

    fn name(&self) -> String {
        "win-ioring (registered)".to_owned()
    }

    fn configuration(&self) -> String {
        format!(
            "single-threaded driver; registered buffers and file handle; \
             registration-naming operations; {}",
            self.handle_mode.suffix()
        )
    }

    async fn open_read(&self, path: &Path) -> io::Result<Self::File> {
        self.handle_mode.open_read(path)
    }

    async fn open_write(&self, path: &Path) -> io::Result<Self::File> {
        self.handle_mode.open_write(path)
    }

    fn take_buffer(&self, _capacity: usize) -> io::Result<Self::Buf> {
        let index = self.free.borrow_mut().pop().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "the registration holds no free buffer",
            )
        })?;
        let buffers = self.buffers.borrow();
        let collection = buffers
            .as_ref()
            .ok_or_else(|| io::Error::other("nothing registered"))?;
        collection.check_out(index).map_err(io::Error::other)
    }

    fn put_buffer(&self, buffer: Self::Buf) {
        let index = buffer.index();
        drop(buffer);
        self.free.borrow_mut().push(index);
    }

    async fn read_at(
        &self,
        file: &Self::File,
        buffer: Self::Buf,
        len: u32,
        offset: u64,
    ) -> OpResult<u32, Self::Buf> {
        let (result, buffer) = self
            .handle
            .read_registered(self.target(file), buffer, 0, len, offset)
            .await
            .into_parts();
        (result.map_err(io::Error::other), buffer)
    }

    async fn write_at(
        &self,
        file: &Self::File,
        buffer: Self::Buf,
        len: u32,
        offset: u64,
    ) -> OpResult<u32, Self::Buf> {
        let (result, buffer) = self
            .handle
            .write_registered(self.target(file), buffer, 0, len, offset)
            .await
            .into_parts();
        (result.map_err(io::Error::other), buffer)
    }

    async fn sync(&self, file: &Self::File) -> io::Result<()> {
        file.flush(&self.handle)
            .await
            .map(|_| ())
            .map_err(io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The read-back that keeps the A/B honest.
    ///
    /// This is the guard against the worst failure this experiment can have: a
    /// seam that silently opens both arms the same way. That produces a clean,
    /// plausible null — "handle mode makes no difference" — with the mechanism
    /// under test never actually varied. `docs/testing.md` records that this
    /// project under-scrutinises unflattering results, which makes a false
    /// negative the cheapest error to ship, and a broken A/B produces one by
    /// default. So the modes are read back from the kernel rather than trusted.
    ///
    /// Whole-value equality, not a mask: `mode & K == K` is satisfied by K = 0,
    /// which is how two earlier gates in this workspace came to be unable to
    /// fail.
    #[test]
    fn the_two_handle_modes_really_differ_on_a_real_handle() {
        use crate::unbuffered_workload::{FILE_SYNCHRONOUS_IO_NONALERT, file_mode};

        let dir = std::env::temp_dir().join("win-ioring-bench-handlemode");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("hm-{}.dat", std::process::id()));
        std::fs::write(&path, vec![0_u8; 4096]).unwrap();

        // Read back through a borrowed `std::fs::File` that never owns the
        // handle, so the `win_ioring::file::File` remains its sole owner.
        let mode_of = |file: &File| -> u32 {
            use std::os::windows::io::FromRawHandle;
            let raw = file.as_raw_handle();
            // SAFETY: `raw` is owned by `file`, which outlives this borrow, and
            // `ManuallyDrop` prevents the temporary from closing it.
            let borrowed =
                std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_handle(raw.0) });
            file_mode(&borrowed).unwrap()
        };

        let overlapped = HandleMode::Overlapped.open_read(&path).unwrap();
        let synchronous = HandleMode::Synchronous.open_read(&path).unwrap();

        assert_eq!(
            mode_of(&overlapped),
            0,
            "HandleMode::Overlapped did not produce an overlapped handle, so \
             the A/B's overlapped arm is not measuring what it claims"
        );
        assert_eq!(
            mode_of(&synchronous),
            FILE_SYNCHRONOUS_IO_NONALERT,
            "HandleMode::Synchronous did not produce a synchronous handle, so \
             the A/B is comparing two overlapped arms and will report a null \
             whatever the truth is"
        );
        // The assertion that survives both constants being wrong, including
        // both being zero: it compares two live measurements to each other.
        assert_ne!(
            mode_of(&overlapped),
            mode_of(&synchronous),
            "the two handle modes report the same mode, so the handle-mode arm \
             varies nothing"
        );

        // Writes travel the other open path, which has its own flags and could
        // drift independently of the read path.
        let w_over = HandleMode::Overlapped
            .open_write(&dir.join("hm-w-o.dat"))
            .unwrap();
        let w_sync = HandleMode::Synchronous
            .open_write(&dir.join("hm-w-s.dat"))
            .unwrap();
        assert_ne!(
            mode_of(&w_over),
            mode_of(&w_sync),
            "the two handle modes agree on the write path, so the write half of \
             the A/B varies nothing even if the read half does"
        );

        drop((overlapped, synchronous, w_over, w_sync));
        let _ = std::fs::remove_file(&path);
    }

    /// The two modes must differ in the flag and in **nothing else**.
    ///
    /// Access and creation disposition are what a careless synchronous open
    /// would get wrong, and a difference there would be attributed to handle
    /// mode by the experiment.
    ///
    /// The probe is `set_len`, which is orthogonal to handle mode: it reaches
    /// the file through `SetFileInformationByHandle`, which does not care
    /// whether the handle is overlapped. `std::io::Read` was tried first and is
    /// **not** usable here — it fails with `ERROR_INVALID_PARAMETER` on an
    /// overlapped handle, because a synchronous `ReadFile` against one requires
    /// an `OVERLAPPED` argument it does not pass. Measured, not assumed: a
    /// standalone probe returned os error 87 for the overlapped handle and read
    /// four bytes for the synchronous one, while `set_len` returned os error 5
    /// on the read handles of *both* modes and succeeded on the write handles
    /// of both. A probe that fails for one arm for a reason unrelated to what
    /// it is testing would have made this guard report a spurious difference.
    #[test]
    fn the_two_modes_differ_only_in_the_flag() {
        let dir = std::env::temp_dir().join("win-ioring-bench-handlemode-acc");
        std::fs::create_dir_all(&dir).unwrap();

        for mode in [HandleMode::Overlapped, HandleMode::Synchronous] {
            let path = dir.join(format!("acc-{mode:?}-{}.dat", std::process::id()));
            std::fs::write(&path, b"0123456789").unwrap();

            // Read handles carry read access and not write access, in both
            // modes.
            let f = mode.open_read(&path).unwrap();
            assert!(
                borrow(&f).set_len(5).is_err(),
                "{mode:?} read handle accepted set_len, so it was opened with \
                 write access and the two modes differ in access as well as in \
                 the flag"
            );
            drop(f);

            // Write handles carry write access and truncate, in both modes.
            let w = mode.open_write(&path).unwrap();
            assert!(
                borrow(&w).set_len(5).is_ok(),
                "{mode:?} write handle refused set_len, so it lacks write \
                 access and the two modes differ in access as well as in the \
                 flag"
            );
            drop(w);
            // Reopened rather than measured through the handle above, because
            // the `set_len(5)` just performed would mask a failure to truncate.
            let w2 = mode.open_write(&path).unwrap();
            drop(w2);
            assert_eq!(
                std::fs::metadata(&path).unwrap().len(),
                0,
                "{mode:?} write handle did not truncate, so the two modes \
                 differ in creation disposition as well as in the flag"
            );
        }
    }

    fn borrow(file: &File) -> std::mem::ManuallyDrop<std::fs::File> {
        use std::os::windows::io::FromRawHandle;
        let raw = file.as_raw_handle();
        // SAFETY: `raw` is owned by `file`, which outlives the borrow, and
        // `ManuallyDrop` prevents the temporary from closing it.
        std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_handle(raw.0) })
    }
}
