//! The thread-pool-backed file backend.
//!
//! # Why `spawn_blocking` and not `tokio::fs::File`
//!
//! `tokio::fs::File` is cursor-based: reads and writes advance a shared
//! position, so it cannot express several *positional* operations outstanding at
//! once, which is exactly what this comparison varies. Underneath, it does what
//! this does — hands the work to the blocking pool — so measuring
//! `spawn_blocking` with `seek_read`/`seek_write` measures the same mechanism
//! while permitting the concurrency the scenarios need.
//!
//! # What varies between the two configurations
//!
//! The **blocking-pool width**, not the runtime flavour. All the I/O happens on
//! the blocking pool regardless of flavour, and the application work here is
//! trivial, so two flavours would measure one thing under two labels. Width is
//! what actually determines how much of this backend's I/O can proceed at once,
//! so that is what the two configurations differ in, and it is reported.

use std::cell::RefCell;
use std::io;
use std::os::windows::fs::FileExt;
use std::path::Path;
use std::sync::Arc;

use crate::backend::{Availability, Backend, Buffer, OpResult};

impl Buffer for Vec<u8> {
    fn bytes(&self) -> &[u8] {
        self
    }

    fn fill(&mut self, src: &[u8]) -> io::Result<()> {
        if src.len() > self.capacity() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "source longer than the buffer",
            ));
        }
        self.clear();
        self.extend_from_slice(src);
        Ok(())
    }
}

/// A pool of reusable buffers.
///
/// Every backend allocates its buffers once, at construction, and hands the same
/// ones out for the life of the run. Without this the backends using owned
/// buffers would pay an `alloc_zeroed` and a `free` *per operation* while the
/// registered backend paid neither — a systematic, one-sided cost confounded
/// with the exact thing being compared.
pub struct BufferPool {
    free: RefCell<Vec<Vec<u8>>>,
}

impl BufferPool {
    /// Allocates `count` buffers of `capacity` bytes.
    pub fn new(count: usize, capacity: usize) -> Self {
        Self {
            free: RefCell::new((0..count).map(|_| vec![0_u8; capacity]).collect()),
        }
    }

    /// Takes a buffer, growing it if the caller needs more than it holds.
    ///
    /// Growth happens on the first operation of a run and not again, so it does
    /// not reintroduce per-operation allocation.
    pub fn take(&self, capacity: usize) -> io::Result<Vec<u8>> {
        let mut buffer = self.free.borrow_mut().pop().ok_or_else(|| {
            io::Error::new(io::ErrorKind::WouldBlock, "the pool holds no free buffer")
        })?;
        if buffer.capacity() < capacity {
            buffer.reserve(capacity - buffer.capacity());
        }
        buffer.resize(capacity, 0);
        Ok(buffer)
    }

    /// Returns a buffer to the pool.
    pub fn put(&self, buffer: Vec<u8>) {
        self.free.borrow_mut().push(buffer);
    }
}

/// A file shared with the blocking pool.
///
/// `Arc` because each operation moves a reference onto a pool thread.
pub struct PoolFile(Arc<std::fs::File>);

/// The thread-pool-backed backend, parameterised by blocking-pool width.
pub struct TokioFs {
    runtime: tokio::runtime::Runtime,
    blocking_threads: usize,
    buffers: BufferPool,
}

impl TokioFs {
    /// Builds the backend with a blocking pool of the given width.
    ///
    /// One thread is the like-for-like comparison against a backend that can use
    /// only one; the default width is this backend at its realistic best.
    ///
    /// `pool` is how many buffers to pre-allocate — at least the in-flight depth
    /// the caller intends to reach.
    pub fn new(blocking_threads: usize, pool: usize, capacity: usize) -> io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .max_blocking_threads(blocking_threads)
            .enable_all()
            .build()?;
        Ok(Self {
            runtime,
            blocking_threads,
            buffers: BufferPool::new(pool, capacity),
        })
    }

    /// Enters the backend's runtime for the duration of `f`.
    pub fn block_on<F: Future>(&self, f: F) -> F::Output {
        self.runtime.block_on(f)
    }

    /// Reports whether this backend can run here.
    pub fn availability() -> Availability {
        Availability::Available
    }
}

impl Backend for TokioFs {
    type Buf = Vec<u8>;
    type File = PoolFile;

    fn name(&self) -> String {
        format!("tokio::fs (blocking pool {})", self.blocking_threads)
    }

    fn configuration(&self) -> String {
        format!(
            "spawn_blocking + seek_read/seek_write; multi-thread runtime; \
             max_blocking_threads = {}",
            self.blocking_threads
        )
    }

    async fn open_read(&self, path: &Path) -> io::Result<Self::File> {
        Ok(PoolFile(Arc::new(
            std::fs::OpenOptions::new().read(true).open(path)?,
        )))
    }

    async fn open_write(&self, path: &Path) -> io::Result<Self::File> {
        Ok(PoolFile(Arc::new(
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(path)?,
        )))
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
        let handle = Arc::clone(&file.0);
        let len = len as usize;
        if buffer.len() < len {
            buffer.resize(len, 0);
        }
        let joined = tokio::task::spawn_blocking(move || {
            let read = handle.seek_read(&mut buffer[..len], offset);
            (read, buffer)
        })
        .await;
        match joined {
            Ok((Ok(read), mut buffer)) => {
                buffer.truncate(read);
                (Ok(read as u32), buffer)
            }
            Ok((Err(e), buffer)) => (Err(e), buffer),
            Err(e) => (Err(io::Error::other(e)), Vec::new()),
        }
    }

    async fn write_at(
        &self,
        file: &Self::File,
        buffer: Self::Buf,
        len: u32,
        offset: u64,
    ) -> OpResult<u32, Self::Buf> {
        let handle = Arc::clone(&file.0);
        let len = len as usize;
        let joined = tokio::task::spawn_blocking(move || {
            let written = handle.seek_write(&buffer[..len.min(buffer.len())], offset);
            (written, buffer)
        })
        .await;
        match joined {
            Ok((Ok(written), buffer)) => (Ok(written as u32), buffer),
            Ok((Err(e), buffer)) => (Err(e), buffer),
            Err(e) => (Err(io::Error::other(e)), Vec::new()),
        }
    }

    async fn sync(&self, file: &Self::File) -> io::Result<()> {
        let handle = Arc::clone(&file.0);
        tokio::task::spawn_blocking(move || handle.sync_all())
            .await
            .map_err(io::Error::other)?
    }
}
