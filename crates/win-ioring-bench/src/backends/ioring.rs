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

/// The ring-backed backend using caller-owned buffers.
pub struct IoRingPlain {
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
            handle,
            driver: Some(driver),
            buffers: crate::backends::tokio_fs::BufferPool::new(pool, capacity),
        })
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
        "single-threaded driver; caller-owned buffers; unregistered handles".to_owned()
    }

    fn open_read(&self, path: &Path) -> io::Result<Self::File> {
        File::open(path)
    }

    fn open_write(&self, path: &Path) -> io::Result<Self::File> {
        File::create(path)
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
            handle,
            driver: Some(driver),
            buffers: RefCell::new(None),
            free: RefCell::new(Vec::new()),
            registered_file: RefCell::new(false),
        })
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
        "single-threaded driver; registered buffers and file handle; \
         registration-naming operations"
            .to_owned()
    }

    fn open_read(&self, path: &Path) -> io::Result<Self::File> {
        File::open(path)
    }

    fn open_write(&self, path: &Path) -> io::Result<Self::File> {
        File::create(path)
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
