//! The completion-based, non-ring file backend.
//!
//! # Why this backend exists
//!
//! The other four backends differ along two axes at once. The thread-pool pair
//! is synchronous work handed to worker threads; the ring pair is asynchronous
//! completion delivered through an I/O ring. A difference between them is
//! therefore attributable to *either* the completion model *or* the ring, and
//! the comparison cannot say which.
//!
//! This one is completion-based and not a ring. It uses `IOCP`, the completion
//! mechanism Windows has had for decades, through the same application logic as
//! everything else. It is the third point that separates the two axes: where it
//! falls relative to the other two tells the reader which of the two properties
//! the numbers are actually about.
//!
//! # One configuration, not two
//!
//! There is no registered-buffer counterpart here, and the omission is a fact
//! about the platform rather than a gap. `Runtime::buffer_pool` exists, but on
//! Windows it resolves to the fallback implementation, whose `BufControl` is a
//! `VecDeque<u16>` of slot indices with a `release` that does nothing — a
//! userspace free list. It is not a kernel registration, unlike the io_uring
//! sibling that maps onto buffer rings. Measuring it against the pooled `Vec`
//! used here would compare one userspace free list with another and report the
//! difference as if it were a registration effect.

use std::io;
use std::path::Path;

use compio::buf::{BufResult, IntoInner, IoBuf};
use compio::fs::File;
use compio::io::{AsyncReadAt, AsyncWriteAt};

use crate::backend::{Availability, Backend, OpResult};
use crate::backends::tokio_fs::BufferPool;

/// The IOCP-backed backend.
pub struct Compio {
    runtime: ::compio::runtime::Runtime,
    /// Pre-allocated for the same reason as every other backend's: so a run's
    /// allocation count is fixed and not confounded with its operation count.
    buffers: BufferPool,
}

impl Compio {
    /// Builds the backend with a pool of `pool` buffers of `capacity` bytes.
    pub fn new(pool: usize, capacity: usize) -> io::Result<Self> {
        Ok(Self {
            runtime: ::compio::runtime::Runtime::new()?,
            buffers: BufferPool::new(pool, capacity),
        })
    }

    /// Enters the backend's runtime for the duration of `f`.
    ///
    /// Every operation must run inside this: compio submits through a
    /// thread-local runtime context, and an operation issued outside one has
    /// nowhere to submit to.
    pub fn block_on<F: Future>(&self, f: F) -> F::Output {
        self.runtime.block_on(f)
    }

    /// Reports whether this backend can run here.
    ///
    /// Building the runtime is the test, because that is what acquires the
    /// completion port. A host that refuses one says so here rather than
    /// failing every operation later.
    pub fn availability() -> Availability {
        match ::compio::runtime::Runtime::new() {
            Ok(_) => Availability::Available,
            Err(e) => Availability::Unavailable(format!("the platform refused a runtime: {e}")),
        }
    }
}

impl Backend for Compio {
    type Buf = Vec<u8>;
    type File = File;

    fn name(&self) -> String {
        "compio (IOCP)".to_owned()
    }

    fn configuration(&self) -> String {
        // The driver type is read from the runtime rather than written down,
        // so a host or a version that produces something other than IOCP
        // reports what it actually did instead of what this expected.
        format!(
            "{:?} driver; single-threaded completion processing \
             (`iocp-global` off, so the runtime drives its own thread's \
             completions); caller-owned buffers; unregistered handles",
            self.runtime.driver_type()
        )
    }

    async fn open_read(&self, path: &Path) -> io::Result<Self::File> {
        File::open(path).await
    }

    async fn open_write(&self, path: &Path) -> io::Result<Self::File> {
        File::create(path).await
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
        buffer: Self::Buf,
        len: u32,
        offset: u64,
    ) -> OpResult<u32, Self::Buf> {
        // `.slice(..len)` is load-bearing, not tidiness. compio's `read_at`
        // takes no length: it fills to the buffer's *capacity*, which for a
        // `Vec` is `capacity()`, not `len()`. Without the slice a 4096-byte
        // request against a buffer holding 8192 bytes of capacity transfers
        // 8192 — measured, not inferred. The pooled buffers happen to keep
        // `capacity() == len()`, so removing this breaks nothing that runs
        // today and would surface only as a wrong byte count in a later
        // change; `over_capacity_reads_are_bounded_by_the_request` below
        // constructs the mismatch directly so the guard has something that
        // fails when it goes.
        // `.slice(..len)` bounds the fill to `min(len, capacity)`, so a request
        // larger than the buffer's capacity would be quietly short where
        // `TokioFs::read_at` resizes up to `len` first
        // (`src/backends/tokio_fs.rs:169-171`). That cannot happen on the
        // harness path, but "cannot happen" is what this assertion is for:
        // `Backend::read_at`'s contract says it reads `len` bytes, and this
        // makes that structural rather than incidental.
        debug_assert!(
            len as usize <= buffer.capacity(),
            "read_at was asked for {len} bytes into a buffer holding {} of capacity",
            buffer.capacity()
        );
        let BufResult(result, slice) = file.read_at(buffer.slice(..len as usize), offset).await;
        // No `truncate` to the delivered count, unlike `TokioFs::read_at`. On
        // completion compio applies `SetLen::advance_to`
        // (compio-driver-0.12.4/src/sys/op/ext.rs:119), which is a *no-op when
        // the new length is not greater than the current one*
        // (compio-buf-0.8.3/src/io_buf.rs:759-767), so the recovered length is
        // `max(pre-read length, delivered)` rather than the delivered count.
        //
        // That divergence is unobservable here, and both halves of that matter.
        // Short reads are unreachable on the harness path: `session.rs:88`
        // sizes the pool at `job.block`, the same value passed as `len`, and
        // every offset the scenarios issue is in bounds. And even if one
        // occurred, `Trace::delivered` clamps with
        // `(transferred as usize).min(bytes.len())` (`src/verify.rs:78`), so
        // the digest and the delivered-byte count are taken over exactly the
        // transferred prefix regardless of what the buffer's length says.
        // Truncating here would therefore change nothing measured; it is
        // omitted because compio does not offer it, not because it was missed.
        let buffer = slice.into_inner();
        match result {
            Ok(read) => (Ok(read as u32), buffer),
            Err(e) => (Err(e), buffer),
        }
    }

    async fn write_at(
        &self,
        file: &Self::File,
        buffer: Self::Buf,
        len: u32,
        offset: u64,
    ) -> OpResult<u32, Self::Buf> {
        // compio's real `write_at` is `impl AsyncWriteAt for &File`, taking
        // `&mut self` — that is, `&mut &File`. This trait hands out `&File`,
        // and `src/scenario.rs` shares one `&File` across every concurrent
        // write of a batch, so a `&mut` must be manufactured here.
        //
        // It is manufactured by *copying the reference*: `&File` is `Copy`, so
        // each future gets its own binding and nothing mutable is shared.
        //
        // It must NOT be manufactured with interior mutability. A `RefCell` or
        // `Mutex` around the handle would serialise the write phase *below*
        // this trait, and that defect publishes cleanly: achieved depth is
        // measured at this seam, so the trace, the digest, the shape check and
        // `achieved.mean == predicted` would all still agree, and the run would
        // report a resolved, plausible, flattering result produced by a bug.
        // Nothing in the output would reveal it. Keep the binding below by
        // value.
        let mut target: &File = file;
        let BufResult(result, slice) = target.write_at(buffer.slice(..len as usize), offset).await;
        let buffer = slice.into_inner();
        match result {
            Ok(written) => (Ok(written as u32), buffer),
            Err(e) => (Err(e), buffer),
        }
    }

    async fn sync(&self, file: &Self::File) -> io::Result<()> {
        file.sync_all().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Buffer;

    /// The guard in `read_at` has to be tested against a buffer the pool cannot
    /// produce.
    ///
    /// `BufferPool::take` resizes to the requested capacity, so a pooled buffer
    /// always has `capacity() == len()` and the over-read cannot occur on the
    /// path the benchmark runs. A mutation deleting `.slice(..len)` would
    /// therefore pass every existing test — the structurally-guaranteed pass
    /// this crate treats as a defect. So the mismatch is built here directly.
    ///
    /// The source file must exceed the buffer's capacity, or an unguarded read
    /// is bounded by end-of-file rather than by the missing guard and the
    /// mutation cannot reproduce.
    #[test]
    fn over_capacity_reads_are_bounded_by_the_request() {
        let dir = std::env::temp_dir().join(format!(
            "win-ioring-bench-compio-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("a temporary directory");
        let path = dir.join("over-capacity.bin");
        // Larger than the buffer's capacity, so an unguarded read is stopped by
        // the guard rather than by the end of the file.
        std::fs::write(&path, vec![7_u8; 16384]).expect("a source file");

        let backend = Compio::new(1, 4096).expect("a compio runtime");
        // Cleanup runs before the assertions, so a failure does not leak the
        // 16 KiB source file. The results are carried out of the closure and
        // asserted on afterwards.
        let delivered = backend.block_on(async {
            let file = backend.open_read(&path).await.expect("an open file");
            let mut buffer = Vec::with_capacity(8192);
            buffer.resize(4096, 0);
            assert_eq!(
                buffer.capacity(),
                8192,
                "this test is only meaningful while capacity exceeds length; \
                 `Vec::with_capacity` is permitted to over-allocate, and if it \
                 ever returned exactly 4096 the guard would have nothing to bound"
            );
            let (result, buffer) = backend.read_at(&file, buffer, 4096, 0).await;
            (result.expect("a read"), buffer.bytes().len())
        });
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            delivered.0, 4096,
            "read_at transferred more than the request: the `.slice(..len)` \
             guard is what bounds it, because compio fills to capacity"
        );
        assert_eq!(
            delivered.1, 4096,
            "the recovered buffer must not have grown past the request. This is \
             not a claim that compio truncates to the delivered count — it does \
             not, because `advance_to` is a no-op downwards. It is the *upward* \
             half: without the guard, compio fills to capacity and `advance_to` \
             grows the buffer to 8192, so this dies alongside the byte count"
        );
    }
}
