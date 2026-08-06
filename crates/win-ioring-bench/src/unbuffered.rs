//! The unbuffered arm's six configurations, and the backends that run them.
//!
//! # Why these are parallel types and not a flag
//!
//! [`crate::backend::Backend::open_read`] sits **inside the timed region** —
//! the trait says so at `crates/win-ioring-bench/src/backend.rs:53-56`. Adding
//! an `unbuffered: bool` to the existing backends and branching on it would
//! therefore insert a branch into the measured path of the *buffered* arms,
//! whose fifty published cells are a single-run artefact that is never patched
//! from a second run. Every type below is new; nothing in
//! `crates/win-ioring-bench/src/backends/` is touched, and an empty
//! `git diff --stat` over that directory is this module's acceptance test.
//!
//! # Where the timer starts, and why it is not where the buffered arms put it
//!
//! This arm opens **outside** the timed region, uniformly across all six
//! configurations. That is a deliberate departure from the buffered arms'
//! convention. It exists because keeping the convention would bias the
//! comparison in this crate's favour.
//!
//! Measured on this host (research probe 7, median of five repeats): an
//! unbuffered overlapped open costs about **9.5 µs** against about **8.9 µs**
//! for a plain buffered one, and sixty-four of them held open cost about
//! **571 µs**. A planned iteration is 256 operations. Charging opens to every
//! iteration would therefore add roughly **25%** to [`Config::TokioPool512Hn`],
//! the one configuration built to hold many handles, and about **0.3%** to
//! every single-handle configuration.
//!
//! On the probe's provisional figures that would move the multi-handle
//! configuration from 8.87 to about 11.10 µs/IO while the ring moved from 11.73
//! to about 11.77 — compressing the competitor's lead from about 1.32x to about
//! 1.06x. It would **not** have produced a ring victory, and an earlier version
//! of this comment claimed it would have ("a fake 1.35x"). That claim came from
//! a single cold run of probe 7 which reported 27.50 µs and 1779.90 µs for the
//! two open costs — roughly triple the medians above. Repeating the probe five
//! times did not reproduce it. The error was in the direction that made this
//! crate's methodology look more scrupulous than it was, which is the direction
//! this work has erred in before, so it is stated here rather than quietly
//! amended.
//!
//! The decision is unchanged, because the direction of the bias is unchanged
//! and only its size was overstated: charging set-up to every 256 reads
//! penalises the configuration that holds the most handles, which is the honest
//! competitor. A real program opens its handle set once and reads through it
//! for the process's life, so the per-iteration charge is an artifact of the
//! iteration boundary rather than a property of the design. The opens move out
//! for everyone, which is what keeps it fair — the ring gives up its 9.5 µs too.
//!
//! The cost is reported rather than discarded: [`open_cost`] is measured and
//! published beside the throughput, because sixty-four handles are not free and
//! a reader weighing the approach should see what it costs to establish.
//!
//! All figures in this section are probe measurements, not results. The
//! published numbers are whatever the harness produces.
//!
//! # What CI covers here, and what it deliberately does not
//!
//! Covered by `cargo test` at small sizes: the aligned allocation, the
//! unbuffered open, one real read per configuration end to end, the flags each
//! backend's `open_read` actually establishes, each backend's real `open_write`
//! refusal, buffer-pool exhaustion and over-capacity refusal in all four pools,
//! the published configuration strings, the handle-count arithmetic, and the
//! open-cost measurement.
//!
//! One disclosed deviation from R10.2, which otherwise forbids asserting on
//! durations: `open_cost_scales_with_the_handle_count` compares two
//! `Duration`s, asserting that opening 32 handles takes longer than opening 1.
//! Nothing else in this module asserts a duration, an ordering, or a ratio
//! against a wall clock, and nothing asserts throughput or a published figure.
//! The exemption is narrow on purpose: the comparison is within one process,
//! best-of-five, and the real margin is ~32x, so it is not a plausible flake —
//! and without it the round-3 fix to `open_cost` has no guard at all. A flaky
//! device-bound gate is worse than no gate, because it teaches people to ignore
//! failures; a 32x within-process margin is not that.
//!
//! Not covered, and stated rather than implied: `write_at`, which is
//! unreachable because every `open_write` refuses first. Reachable only from
//! the opt-in bench target.
//!
//! That split is deliberate rather than an omission. **A flaky device-bound
//! gate is worse than no gate, because it trains people to ignore failures** —
//! and a gate people have learned to ignore is indistinguishable from a gate
//! that does not exist, except that it looks like coverage. Timing on a shared
//! CI runner is not reproducible: the µs/IO figures, the depth-1-to-64 scaling
//! and this arm's ratios all depend on a specific drive in a specific state.
//! So CI proves the arm still *runs*, and only a manual
//! `cargo bench --bench unbuffered` on known hardware produces numbers.
//!
//! The next reader to notice the coverage gap should find this argument and be
//! able to disagree with it on the merits, rather than re-derive it or "fix" it
//! by adding a timing assertion that will fail on a loaded runner.

use std::io;
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use windows::Win32::Storage::FileSystem::{FILE_FLAG_NO_BUFFERING, FILE_FLAG_OVERLAPPED};

use crate::aligned::AlignedBuf;
use crate::backend::{Availability, Backend, OpResult};

/// The flags the *completion-based* read handles in this arm are opened with.
///
/// `FILE_FLAG_NO_BUFFERING` is the point of the arm. `FILE_FLAG_OVERLAPPED` is
/// not optional decoration: without it the kernel serialises operations at the
/// file object, so a backend submitting at depth 64 gets a depth of one. See
/// the rustdoc on `win_ioring::file::File::open`, which does *not* set it, and
/// `crates/win-ioring-bench/tests/open_mode.rs`, which pins that.
///
/// This is **not** what the thread-pool configurations use. See
/// [`POOL_READ_FLAGS`].
pub const READ_FLAGS: u32 = FILE_FLAG_NO_BUFFERING.0 | FILE_FLAG_OVERLAPPED.0;

/// The flags the *thread-pool* read handles in this arm are opened with.
///
/// Unbuffered, and deliberately **synchronous** — no `FILE_FLAG_OVERLAPPED`.
/// Two independent reasons, either of which alone would be sufficient:
///
/// 1. **It is the configuration under measurement.** Spec R3.3: the thread-pool
///    backends use synchronous handles as they do today; that is not an
///    oversight to be corrected. Handing them overlapped handles would quietly
///    remove the file-object serialisation that `TokioPool512H1` exists to
///    exhibit, and the finding that pool width is not the variable — handle
///    count is — depends on that control being intact.
/// 2. **`seek_read` is not usable on an overlapped handle under concurrency.**
///    `std`'s positional read requires the operation to complete synchronously
///    and calls `rtabort!` if the kernel returns `STATUS_PENDING`, because
///    otherwise the kernel could write into a buffer whose stack frame is gone.
///    On an overlapped handle a real device read *does* return `STATUS_PENDING`.
///
/// The second was measured, not reasoned about, because the first attempt to
/// measure it was wrong in a way worth recording. Probe 8 ran sixty-four
/// threads issuing `seek_read` against one handle:
///
/// | file | synchronous handle | overlapped handle |
/// |------|--------------------|-------------------|
/// | `set_len` only, no content written | survived | survived |
/// | real content written and flushed   | survived | **aborted the process** |
///
/// The first row is the trap. A file extended with `set_len` has a valid data
/// length of zero, so the filesystem serves reads from it as zeros without ever
/// reaching the device — and a read that never reaches the device always
/// completes synchronously. That experiment could not have failed, and it
/// reported that the overlapped handle was fine.
pub const POOL_READ_FLAGS: u32 = FILE_FLAG_NO_BUFFERING.0;

/// Opens a file for unbuffered, overlapped reading.
///
/// For the completion-based configurations. The thread-pool configurations use
/// [`open_unbuffered_synchronous`] instead.
///
/// # Errors
///
/// If the file cannot be opened with those flags.
pub fn open_unbuffered(path: &Path) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(READ_FLAGS)
        .open(path)
}

/// Opens a file for unbuffered, *synchronous* reading.
///
/// For the thread-pool configurations. See [`POOL_READ_FLAGS`] for why they do
/// not get an overlapped handle, and why using one would both change what is
/// being measured and abort the process.
///
/// # Errors
///
/// If the file cannot be opened with those flags.
pub fn open_unbuffered_synchronous(path: &Path) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(POOL_READ_FLAGS)
        .open(path)
}

/// What it costs to establish the handle set a configuration needs at `depth`.
///
/// Reported beside the throughput rather than folded into it. This arm opens
/// outside the timed region — see the module header for why keeping the
/// buffered arms' convention would have biased the comparison in this crate's
/// favour — but the cost is
/// real and belongs in the record: sixty-four handles are not free, and a
/// reader weighing the multi-handle approach against the ring's single handle
/// should be able to see what the approach costs to set up as well as what it
/// delivers per operation.
///
/// Scope, stated because the asymmetry would otherwise run in this crate's
/// favour: this measures **opens only**. It excludes `IoRing` and driver
/// construction and buffer registration, which only the ring configurations
/// pay, and it excludes the async runtime startup and buffer-pool allocation
/// that *every* configuration pays — `UnbufferedTokioFs::new` builds a
/// multi-threaded runtime and allocates its whole pool just as the ring does.
/// So the exclusion understates setup for all six, but disproportionately for
/// the ring, which has the most excluded machinery. The residual therefore runs
/// **for** this crate, not against it. Read the figure as a lower bound on
/// setup, not a total.
///
/// # Errors
///
/// If any handle cannot be opened.
pub fn open_cost(config: Config, path: &Path, depth: usize) -> io::Result<std::time::Duration> {
    let n = config.handles(depth);
    // Opened from `read_flags()` rather than from a second table of openers.
    // There used to be a third copy of this fact, and the test that guarded it
    // checked only the `FILE_FLAG_OVERLAPPED` bit — leaving the copy free to
    // drift on `FILE_FLAG_NO_BUFFERING`, the one bit this entire arm exists to
    // measure. A buffered open here would have poisoned the arm's working file
    // for the life of the process (see the module header), producing a
    // plausible, publishable, wrong number. Deriving it removes the copy
    // instead of testing it.
    let flags = config.read_flags();
    let open = |p: &Path| {
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(flags)
            .open(p)
    };
    let start = std::time::Instant::now();
    let handles = (0..n).map(|_| open(path)).collect::<io::Result<Vec<_>>>()?;
    let elapsed = start.elapsed();
    drop(handles);
    Ok(elapsed)
}

/// One cell's backend choice in the unbuffered arm.
///
/// Deliberately **not** `crate::harness::Which`. Sharing that enum would put
/// this arm's variants into `slug()` and into the position-balance invariant
/// that governs the published warm-cache matrix, and the Criterion baseline
/// keys under `target/criterion/` are derived from those slugs. A colliding
/// name would silently overwrite the primary published result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Config {
    /// The ring with caller-owned aligned buffers.
    IoRingPlain,
    /// The ring with registered aligned buffers.
    IoRingRegistered,
    /// IOCP, the control that separates "completion-based" from "the ring".
    Compio,
    /// A blocking pool of one, which is what the warm-cache matrix found hard
    /// to beat. Under real device latency it is capped at one outstanding
    /// request.
    TokioPool1,
    /// A wide blocking pool on a *single* handle: the control that isolates
    /// pool width from handle count.
    TokioPool512H1,
    /// A wide blocking pool with one handle per outstanding operation.
    ///
    /// **The honest competitor.** Reporting only the single-handle
    /// configurations would manufacture a ring win out of a handle-count
    /// decision.
    TokioPool512Hn,
}

impl Config {
    /// Every configuration, in report order.
    pub fn all() -> [Config; 6] {
        [
            Config::IoRingPlain,
            Config::IoRingRegistered,
            Config::Compio,
            Config::TokioPool1,
            Config::TokioPool512H1,
            Config::TokioPool512Hn,
        ]
    }

    /// The stable identifier used in this arm's report and baseline keys.
    ///
    /// Disjoint from `crate::harness::Which::slug` by construction — see
    /// `slugs_do_not_collide_with_the_warm_cache_arm` below, which asserts it
    /// rather than trusting it.
    pub fn slug(self) -> &'static str {
        match self {
            Config::IoRingPlain => "unbuffered-ioring-owned",
            Config::IoRingRegistered => "unbuffered-ioring-registered",
            Config::Compio => "unbuffered-compio",
            Config::TokioPool1 => "unbuffered-tokio-pool1-h1",
            Config::TokioPool512H1 => "unbuffered-tokio-pool512-h1",
            Config::TokioPool512Hn => "unbuffered-tokio-pool512-hn",
        }
    }

    /// The name for the report.
    pub fn name(self) -> &'static str {
        match self {
            Config::IoRingPlain => "win-ioring (owned buffers)",
            Config::IoRingRegistered => "win-ioring (registered)",
            Config::Compio => "compio (IOCP)",
            Config::TokioPool1 => "tokio::fs (pool 1, 1 handle)",
            Config::TokioPool512H1 => "tokio::fs (pool 512, 1 handle)",
            Config::TokioPool512Hn => "tokio::fs (pool 512, N handles)",
        }
    }

    /// How many file handles this configuration opens at `depth`.
    ///
    /// A first-class property, not a footnote: the whole finding of this arm is
    /// that handle count rather than pool width is the lever, so it reaches the
    /// published table as a column.
    pub fn handles(self, depth: usize) -> usize {
        match self {
            Config::TokioPool512Hn => depth.max(1),
            _ => 1,
        }
    }

    /// The blocking-pool width, for the configurations that have one.
    pub fn pool_width(self) -> Option<usize> {
        match self {
            Config::TokioPool1 => Some(1),
            Config::TokioPool512H1 | Config::TokioPool512Hn => Some(512),
            _ => None,
        }
    }

    /// The exact flags this configuration's read handles must be opened with.
    ///
    /// The completion-based configurations get [`READ_FLAGS`]; the thread-pool
    /// configurations get [`POOL_READ_FLAGS`], which omits
    /// `FILE_FLAG_OVERLAPPED`. See [`POOL_READ_FLAGS`] for the two reasons.
    ///
    /// This exists so the read-back test can bind each backend's *actual*
    /// handle to a per-configuration expectation. Before it did, a backend
    /// could be mutated to open plainly buffered and every test still passed.
    ///
    /// [`open_cost`] opens from this too, so there is one source of truth
    /// rather than a parallel table that has to be kept in step.
    pub fn read_flags(self) -> u32 {
        match self {
            Config::TokioPool1 | Config::TokioPool512H1 | Config::TokioPool512Hn => POOL_READ_FLAGS,
            _ => READ_FLAGS,
        }
    }

    /// True where two configurations are structurally the same cell at `depth`.
    ///
    /// At depth 1 [`Config::TokioPool512Hn`] opens one handle, so it *is*
    /// [`Config::TokioPool512H1`]. Both are still run: two nominally identical
    /// cells measured independently in the same run are a free within-run
    /// reproducibility check. The report must mark the pair so no reader counts
    /// it as two data points.
    ///
    /// Only thread-pool configurations can coincide. An earlier version
    /// compared `pool_width()` directly, and `None == None` made the plain
    /// ring, the registered ring and compio mutually duplicates at every depth
    /// — which would have merged the control into the thing it controls for.
    /// Two configurations with no pool width are not thereby the same cell;
    /// they are different designs.
    pub fn duplicates(self, other: Config, depth: usize) -> bool {
        let (Some(mine), Some(theirs)) = (self.pool_width(), other.pool_width()) else {
            return false;
        };
        self != other && mine == theirs && self.handles(depth) == other.handles(depth)
    }
}

// ------------------------------------------------------------------ ring, own

/// The ring with caller-owned aligned buffers, reading unbuffered.
pub struct UnbufferedIoRing {
    handle: win_ioring::runtime::Handle,
    driver: Option<win_ioring::runtime::Driver>,
    buffers: std::cell::RefCell<Vec<AlignedBuf>>,
}

impl UnbufferedIoRing {
    /// Builds the backend with a ring sized for `depth` operations in flight
    /// and `pool` buffers of `capacity` bytes aligned to `align`.
    ///
    /// # Errors
    ///
    /// If the ring cannot be built or a buffer cannot be allocated.
    pub fn new(depth: usize, pool: usize, capacity: usize, align: usize) -> io::Result<Self> {
        let queue = (depth.max(1) as u32).next_power_of_two();
        let ring = win_ioring::io_ring::IoRing::builder()
            .with_submission_queue_size(queue)
            .with_completion_queue_size(queue * 2)
            .build()
            .map_err(io::Error::other)?;
        let driver = win_ioring::runtime::Driver::new(ring).map_err(io::Error::other)?;
        let handle = driver.handle();
        let buffers = (0..pool)
            .map(|_| AlignedBuf::new(capacity, align))
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self {
            handle,
            driver: Some(driver),
            buffers: std::cell::RefCell::new(buffers),
        })
    }

    /// Returns the driver, for the caller to spawn alongside its work.
    pub fn take_driver(&mut self) -> Option<win_ioring::runtime::Driver> {
        self.driver.take()
    }

    /// Returns a handle, for shutting the driver down when the work is done.
    pub fn handle(&self) -> win_ioring::runtime::Handle {
        self.handle.clone()
    }
}

impl Backend for UnbufferedIoRing {
    type Buf = AlignedBuf;
    type File = win_ioring::file::File;

    fn name(&self) -> String {
        Config::IoRingPlain.name().to_owned()
    }

    fn configuration(&self) -> String {
        "single-threaded driver; caller-owned sector-aligned buffers; \
         unbuffered overlapped handle"
            .to_owned()
    }

    async fn open_read(&self, path: &Path) -> io::Result<Self::File> {
        // `File::from_std`, never `File::open` — the latter produces a
        // synchronous handle, which serialises the ring down to one
        // outstanding operation. That is documented on `File::open` and pinned
        // by `tests/open_mode.rs`; this is the additive route around it that
        // leaves the buffered arms bit-identical.
        Ok(win_ioring::file::File::from_std(open_unbuffered(path)?))
    }

    async fn open_write(&self, _path: &Path) -> io::Result<Self::File> {
        Err(unsupported_write())
    }

    fn take_buffer(&self, capacity: usize) -> io::Result<Self::Buf> {
        take_aligned(&self.buffers, capacity)
    }

    fn put_buffer(&self, buffer: Self::Buf) {
        self.buffers.borrow_mut().push(buffer);
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
            .read(file, buffer, len, offset)
            .await
            .into_parts();
        (result.map_err(io::Error::other), buffer)
    }

    async fn write_at(
        &self,
        _file: &Self::File,
        buffer: Self::Buf,
        _len: u32,
        _offset: u64,
    ) -> OpResult<u32, Self::Buf> {
        (Err(unsupported_write()), buffer)
    }

    async fn sync(&self, _file: &Self::File) -> io::Result<()> {
        Ok(())
    }
}

// ----------------------------------------------------------- ring, registered

/// The ring with registered aligned buffers, reading unbuffered.
///
/// `Handle::register_buffers` is generic over `IoBufMut` and registers the
/// caller's memory rather than allocating its own, which is what lets a
/// sector-aligned buffer be registered at all. The existing
/// `IoRingRegistered::register` allocates `Vec<Vec<u8>>` and must not be
/// edited, so this is a separate type rather than a parameter on that one.
pub struct UnbufferedIoRingRegistered {
    handle: win_ioring::runtime::Handle,
    driver: Option<win_ioring::runtime::Driver>,
    buffers: std::cell::RefCell<Option<win_ioring::runtime::RegisteredBuffers>>,
    free: std::cell::RefCell<Vec<u32>>,
    /// The size each registered buffer was established with, so `take_buffer`
    /// can refuse a read that would not fit rather than truncating it.
    buffer_len: std::cell::Cell<usize>,
}

impl UnbufferedIoRingRegistered {
    /// Builds the backend with a ring sized for `depth` operations in flight.
    ///
    /// # Errors
    ///
    /// If the ring cannot be built.
    pub fn new(depth: usize) -> io::Result<Self> {
        let queue = (depth.max(1) as u32).next_power_of_two();
        let ring = win_ioring::io_ring::IoRing::builder()
            .with_submission_queue_size(queue)
            .with_completion_queue_size(queue * 2)
            .build()
            .map_err(io::Error::other)?;
        let driver = win_ioring::runtime::Driver::new(ring).map_err(io::Error::other)?;
        let handle = driver.handle();
        Ok(Self {
            handle,
            driver: Some(driver),
            buffers: std::cell::RefCell::new(None),
            free: std::cell::RefCell::new(Vec::new()),
            buffer_len: std::cell::Cell::new(0),
        })
    }

    /// Returns the driver, for the caller to spawn alongside its work.
    pub fn take_driver(&mut self) -> Option<win_ioring::runtime::Driver> {
        self.driver.take()
    }

    /// Returns a handle, for shutting the driver down when the work is done.
    pub fn handle(&self) -> win_ioring::runtime::Handle {
        self.handle.clone()
    }

    /// Registers `count` sector-aligned buffers of `capacity` bytes each.
    ///
    /// # Errors
    ///
    /// If a buffer cannot be allocated or the registration is refused.
    pub async fn register(&self, count: usize, capacity: usize, align: usize) -> io::Result<()> {
        let buffers = (0..count)
            .map(|_| AlignedBuf::new(capacity, align))
            .collect::<io::Result<Vec<_>>>()?;
        match self.handle.register_buffers(buffers).await {
            win_ioring::runtime::Registered::Ok(collection) => {
                *self.free.borrow_mut() = (0..count as u32).rev().collect();
                *self.buffers.borrow_mut() = Some(collection);
                self.buffer_len.set(capacity);
                Ok(())
            }
            win_ioring::runtime::Registered::Failed(e, _) => Err(io::Error::other(e)),
        }
    }
}

impl Backend for UnbufferedIoRingRegistered {
    type Buf = win_ioring::runtime::RegisteredBuf;
    type File = win_ioring::file::File;

    fn name(&self) -> String {
        Config::IoRingRegistered.name().to_owned()
    }

    fn configuration(&self) -> String {
        "single-threaded driver; registered sector-aligned buffers; \
         unbuffered overlapped handle"
            .to_owned()
    }

    async fn open_read(&self, path: &Path) -> io::Result<Self::File> {
        Ok(win_ioring::file::File::from_std(open_unbuffered(path)?))
    }

    async fn open_write(&self, _path: &Path) -> io::Result<Self::File> {
        Err(unsupported_write())
    }

    fn take_buffer(&self, capacity: usize) -> io::Result<Self::Buf> {
        // `capacity` is validated here rather than ignored. The registered
        // buffers are fixed-size and established once by `register`, so an
        // over-large read would silently truncate at the registration size
        // instead of failing -- and a truncated read is faster than a complete
        // one, which is the wrong direction for a benchmark to be wrong in.
        if capacity > self.buffer_len.get() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "a {capacity}-byte read exceeds the {}-byte registered buffer size",
                    self.buffer_len.get()
                ),
            ));
        }
        let index = self.free.borrow_mut().pop().ok_or_else(|| {
            io::Error::new(io::ErrorKind::WouldBlock, "the pool holds no free buffer")
        })?;
        let borrowed = self.buffers.borrow();
        let collection = borrowed.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "buffers were taken before `register` established them",
            )
        })?;
        collection.check_out(index).map_err(io::Error::other)
    }

    fn put_buffer(&self, buffer: Self::Buf) {
        self.free.borrow_mut().push(buffer.index());
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
            .read_registered(
                win_ioring::runtime::FileTarget::Owned(file),
                buffer,
                // Buffer offset: reads target the whole registered extent from
                // its base, which is the address the alignment was chosen for.
                0,
                len,
                offset,
            )
            .await
            .into_parts();
        (result.map_err(io::Error::other), buffer)
    }

    async fn write_at(
        &self,
        _file: &Self::File,
        buffer: Self::Buf,
        _len: u32,
        _offset: u64,
    ) -> OpResult<u32, Self::Buf> {
        (Err(unsupported_write()), buffer)
    }

    async fn sync(&self, _file: &Self::File) -> io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------- compio

/// [`AlignedBuf`] wearing compio's buffer traits.
///
/// A newtype rather than impls on `AlignedBuf` itself: compio's `IoBuf` and
/// this crate's `IoBuf` are different traits with colliding names, and keeping
/// the compio side in a wrapper means neither module has to disambiguate at
/// every call site.
pub struct CompioAligned(pub AlignedBuf);

// SAFETY-adjacent note: compio's `IoBuf` is a safe trait, but its contract is
// the same in substance as `win_ioring::buf::IoBuf` — a stable pointer and
// extents for as long as the operation is in flight, which `AlignedBuf`
// provides because it never reallocates.
impl compio::buf::IoBuf for CompioAligned {
    fn as_init(&self) -> &[u8] {
        self.0.filled()
    }
}

impl compio::buf::SetLen for CompioAligned {
    unsafe fn set_len(&mut self, len: usize) {
        self.0.set_len(len.min(self.0.capacity()));
    }
}

impl compio::buf::IoBufMut for CompioAligned {
    fn as_uninit(&mut self) -> &mut [std::mem::MaybeUninit<u8>] {
        let cap = self.0.capacity();
        let ptr = self.0.as_mut_ptr().cast::<std::mem::MaybeUninit<u8>>();
        // SAFETY: the allocation is `cap` bytes in a single object, uniquely
        // borrowed through `&mut self`, and `MaybeUninit<u8>` has the same
        // layout as `u8` and no initialisation requirement.
        unsafe { std::slice::from_raw_parts_mut(ptr, cap) }
    }
}

impl crate::backend::Buffer for CompioAligned {
    fn bytes(&self) -> &[u8] {
        self.0.bytes()
    }

    fn fill(&mut self, src: &[u8]) -> io::Result<()> {
        self.0.fill(src)
    }
}

/// The IOCP backend reading unbuffered.
///
/// compio's `OpenOptions` ORs `FILE_FLAG_OVERLAPPED` itself, so only
/// `FILE_FLAG_NO_BUFFERING` is passed through `custom_flags` — which is why
/// this arm's compio handle is genuinely asynchronous while the ring's needs
/// `from_std` to become so.
pub struct UnbufferedCompio {
    runtime: compio::runtime::Runtime,
    align: usize,
    buffers: std::cell::RefCell<Vec<CompioAligned>>,
}

impl UnbufferedCompio {
    /// Builds the backend with `pool` aligned buffers of `capacity` bytes.
    ///
    /// # Errors
    ///
    /// If the runtime cannot be built or a buffer cannot be allocated.
    pub fn new(pool: usize, capacity: usize, align: usize) -> io::Result<Self> {
        let buffers = (0..pool)
            .map(|_| AlignedBuf::new(capacity, align).map(CompioAligned))
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self {
            runtime: compio::runtime::Runtime::new()?,
            align,
            buffers: std::cell::RefCell::new(buffers),
        })
    }

    /// Enters the backend's runtime for the duration of `f`.
    pub fn block_on<F: Future>(&self, f: F) -> F::Output {
        self.runtime.block_on(f)
    }

    /// The alignment this backend's buffers were allocated with.
    pub fn alignment(&self) -> usize {
        self.align
    }
}

impl Backend for UnbufferedCompio {
    type Buf = CompioAligned;
    type File = compio::fs::File;

    fn name(&self) -> String {
        Config::Compio.name().to_owned()
    }

    fn configuration(&self) -> String {
        format!(
            "{:?} driver; single-threaded completion processing; \
             sector-aligned owned buffers; unbuffered overlapped handle",
            self.runtime.driver_type()
        )
    }

    async fn open_read(&self, path: &Path) -> io::Result<Self::File> {
        // Only NO_BUFFERING: compio ORs OVERLAPPED into the flags itself.
        compio::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_NO_BUFFERING.0)
            .open(path)
            .await
    }

    async fn open_write(&self, _path: &Path) -> io::Result<Self::File> {
        Err(unsupported_write())
    }

    fn take_buffer(&self, capacity: usize) -> io::Result<Self::Buf> {
        let buffer = self.buffers.borrow_mut().pop().ok_or_else(|| {
            io::Error::new(io::ErrorKind::WouldBlock, "the pool holds no free buffer")
        })?;
        if buffer.0.capacity() < capacity {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "a {capacity}-byte read needs a buffer of at least that capacity, not {}",
                    buffer.0.capacity()
                ),
            ));
        }
        Ok(buffer)
    }

    fn put_buffer(&self, buffer: Self::Buf) {
        self.buffers.borrow_mut().push(buffer);
    }

    async fn read_at(
        &self,
        file: &Self::File,
        buffer: Self::Buf,
        len: u32,
        offset: u64,
    ) -> OpResult<u32, Self::Buf> {
        use compio::buf::{IntoInner, IoBuf};
        use compio::io::AsyncReadAt;

        // `.slice(..len)` is load-bearing for the same reason it is in the
        // buffered compio backend: compio's `read_at` takes no length and
        // fills to the buffer's capacity, which here is rounded up to a whole
        // number of sectors and so is routinely larger than the request.
        let compio::buf::BufResult(result, slice) =
            file.read_at(buffer.slice(..len as usize), offset).await;
        let buffer = slice.into_inner();
        match result {
            Ok(read) => (Ok(read as u32), buffer),
            Err(e) => (Err(e), buffer),
        }
    }

    async fn write_at(
        &self,
        _file: &Self::File,
        buffer: Self::Buf,
        _len: u32,
        _offset: u64,
    ) -> OpResult<u32, Self::Buf> {
        (Err(unsupported_write()), buffer)
    }

    async fn sync(&self, _file: &Self::File) -> io::Result<()> {
        Ok(())
    }
}

// ----------------------------------------------------------------- thread pool
/// A set of file handles shared with the blocking pool.
///
/// One handle for most configurations, `depth` of them for
/// [`Config::TokioPool512Hn`] — because a `HANDLE` opened without
/// `FILE_FLAG_OVERLAPPED` serialises at the file object, so a wide pool reading
/// through a single synchronous handle achieves an outstanding depth of one no
/// matter how many threads it has.
pub struct PoolFiles {
    handles: Vec<Arc<std::fs::File>>,
    /// Round-robin cursor.
    ///
    /// A relaxed `fetch_add` costs a few nanoseconds against a device read of
    /// tens of microseconds — under 0.05% — and it is charged to the
    /// configuration this arm is trying *not* to flatter away, so the residual
    /// bias runs against this crate rather than for it. That direction is the
    /// point: an error that favours the competitor cannot manufacture the
    /// result being tested for.
    next: AtomicUsize,
}

impl PoolFiles {
    /// The handles this file set holds.
    ///
    /// Exists so tests can read the mode back off the handles the backend
    /// really opened, rather than off a handle the test opened itself.
    pub fn handles(&self) -> &[Arc<std::fs::File>] {
        &self.handles
    }

    fn pick(&self) -> Arc<std::fs::File> {
        let i = self.next.fetch_add(1, Ordering::Relaxed);
        Arc::clone(&self.handles[i % self.handles.len()])
    }

    /// How many handles this file set holds.
    pub fn len(&self) -> usize {
        self.handles.len()
    }

    /// True if the set holds no handles, which cannot happen by construction.
    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }
}

/// The thread-pool backend reading unbuffered, parameterised by pool width and
/// handle count.
pub struct UnbufferedTokioFs {
    runtime: tokio::runtime::Runtime,
    blocking_threads: usize,
    handles: usize,
    align: usize,
    buffers: std::sync::Mutex<Vec<AlignedBuf>>,
}

impl UnbufferedTokioFs {
    /// Builds the backend.
    ///
    /// # Errors
    ///
    /// If the runtime cannot be built or a buffer cannot be allocated.
    pub fn new(
        blocking_threads: usize,
        handles: usize,
        pool: usize,
        capacity: usize,
        align: usize,
    ) -> io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(blocking_threads)
            .enable_all()
            .build()?;
        let buffers = (0..pool)
            .map(|_| AlignedBuf::new(capacity, align))
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self {
            runtime,
            blocking_threads,
            handles: handles.max(1),
            align,
            buffers: std::sync::Mutex::new(buffers),
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

impl Backend for UnbufferedTokioFs {
    type Buf = AlignedBuf;
    type File = PoolFiles;

    fn name(&self) -> String {
        format!(
            "tokio::fs (pool {}, {} handle{})",
            self.blocking_threads,
            self.handles,
            if self.handles == 1 { "" } else { "s" }
        )
    }

    fn configuration(&self) -> String {
        format!(
            "spawn_blocking + seek_read; max_blocking_threads = {}; \
             {} unbuffered synchronous handle(s), round-robin; \
             sector-aligned owned buffers",
            self.blocking_threads, self.handles
        )
    }

    async fn open_read(&self, path: &Path) -> io::Result<Self::File> {
        // `open_unbuffered_synchronous`, never `open_unbuffered`: no
        // `FILE_FLAG_OVERLAPPED`. This is the configuration under measurement
        // (R3.3), and `seek_read` aborts the process on an overlapped handle.
        // See `POOL_READ_FLAGS`.
        let handles = (0..self.handles)
            .map(|_| open_unbuffered_synchronous(path).map(Arc::new))
            .collect::<io::Result<Vec<_>>>()?;
        Ok(PoolFiles {
            handles,
            next: AtomicUsize::new(0),
        })
    }

    async fn open_write(&self, _path: &Path) -> io::Result<Self::File> {
        Err(unsupported_write())
    }

    fn take_buffer(&self, capacity: usize) -> io::Result<Self::Buf> {
        // Fail loudly on exhaustion, exactly as the ring and compio do. An
        // earlier version allocated a replacement here instead. That runs
        // inside the timed region, so it would have charged the honest
        // competitor a hidden per-operation `alloc_zeroed` that this crate's
        // configurations never pay -- a silent cost, in this crate's favour,
        // in the one configuration that beats it.
        let mut pool = self.buffers.lock().map_err(|_| poisoned())?;
        let buffer = pool.pop().ok_or_else(|| {
            io::Error::new(io::ErrorKind::WouldBlock, "the pool holds no free buffer")
        })?;
        if buffer.capacity() < capacity {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "a {capacity}-byte read needs a buffer of at least that capacity, not {}",
                    buffer.capacity()
                ),
            ));
        }
        Ok(buffer)
    }

    fn put_buffer(&self, buffer: Self::Buf) {
        if let Ok(mut pool) = self.buffers.lock() {
            pool.push(buffer);
        }
    }

    async fn read_at(
        &self,
        file: &Self::File,
        mut buffer: Self::Buf,
        len: u32,
        offset: u64,
    ) -> OpResult<u32, Self::Buf> {
        let handle = file.pick();
        let len = len as usize;
        if buffer.capacity() < len {
            return (
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "a {len}-byte read needs a buffer of at least that capacity, not {}",
                        buffer.capacity()
                    ),
                )),
                buffer,
            );
        }
        let joined = tokio::task::spawn_blocking(move || {
            let read = handle.seek_read(&mut buffer.spare()[..len], offset);
            (read, buffer)
        })
        .await;
        match joined {
            Ok((Ok(read), mut buffer)) => {
                buffer.set_len(read);
                (Ok(read as u32), buffer)
            }
            Ok((Err(e), buffer)) => (Err(e), buffer),
            // The buffer is gone with the panicked task, so a replacement of
            // the right alignment is allocated rather than returning an
            // unaligned stand-in that would fail the next read for a different
            // reason and disguise the panic.
            Err(e) => (
                Err(io::Error::other(e)),
                AlignedBuf::new(len, self.align).unwrap_or_else(|_| {
                    AlignedBuf::new(self.align, self.align).expect("a one-sector buffer")
                }),
            ),
        }
    }

    async fn write_at(
        &self,
        _file: &Self::File,
        buffer: Self::Buf,
        _len: u32,
        _offset: u64,
    ) -> OpResult<u32, Self::Buf> {
        (Err(unsupported_write()), buffer)
    }

    async fn sync(&self, _file: &Self::File) -> io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------- shared

fn unsupported_write() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "the unbuffered arm measures reads only; an unbuffered write would need \
         its own alignment and end-of-file treatment, and is recorded as a \
         phase candidate rather than half-built here",
    )
}

fn poisoned() -> io::Error {
    io::Error::other("the buffer pool's lock was poisoned by a panicking read")
}

fn take_aligned(
    pool: &std::cell::RefCell<Vec<AlignedBuf>>,
    capacity: usize,
) -> io::Result<AlignedBuf> {
    let buffer = pool.borrow_mut().pop().ok_or_else(|| {
        io::Error::new(io::ErrorKind::WouldBlock, "the pool holds no free buffer")
    })?;
    if buffer.capacity() < capacity {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "a {capacity}-byte read needs a buffer of at least that capacity, not {}",
                buffer.capacity()
            ),
        ));
    }
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Baseline keys under `target/criterion/` are derived from slugs, and the
    /// warm-cache matrix is the project's primary published result. A colliding
    /// name would overwrite it silently — no test would notice and no reader
    /// could tell from the output. So the disjointness is asserted rather than
    /// maintained by convention.
    ///
    /// No count of collisions is stated here. The unprefixed names this arm
    /// would otherwise have used collide on two of the five warm slugs, not
    /// three; an earlier version of this comment said three. The number is not
    /// load-bearing and the assertion below enumerates rather than counts.
    #[test]
    fn slugs_do_not_collide_with_the_warm_cache_arm() {
        let warm: Vec<&str> = crate::harness::Which::all()
            .iter()
            .map(|w| w.slug())
            .collect();
        for config in Config::all() {
            assert!(
                !warm.contains(&config.slug()),
                "{} collides with a warm-cache slug, which would overwrite \
                 that cell's Criterion baseline",
                config.slug()
            );
        }
    }

    #[test]
    fn slugs_are_unique_within_this_arm() {
        let mut seen = std::collections::HashSet::new();
        for config in Config::all() {
            assert!(
                seen.insert(config.slug()),
                "duplicate slug {}",
                config.slug()
            );
        }
    }

    /// The handle count is the finding, so it is pinned. Forcing
    /// `TokioPool512Hn` to one handle must fail here.
    #[test]
    fn only_the_multi_handle_configuration_scales_its_handles() {
        for depth in [1usize, 8, 64] {
            for config in Config::all() {
                let expected = if config == Config::TokioPool512Hn {
                    depth
                } else {
                    1
                };
                assert_eq!(
                    config.handles(depth),
                    expected,
                    "{} at depth {depth}",
                    config.slug()
                );
            }
        }
    }

    /// Pool width and handle count are independent axes, and the whole
    /// correction this arm publishes depends on their being measured
    /// separately.
    #[test]
    fn pool_width_and_handle_count_are_separable() {
        assert_eq!(
            Config::TokioPool512H1.pool_width(),
            Config::TokioPool512Hn.pool_width(),
            "the two wide-pool configurations must differ only in handle count"
        );
        assert_ne!(
            Config::TokioPool512H1.handles(64),
            Config::TokioPool512Hn.handles(64)
        );
        assert_eq!(
            Config::TokioPool1.handles(64),
            Config::TokioPool512H1.handles(64),
            "the pool-width control must hold handle count fixed"
        );
        assert_ne!(
            Config::TokioPool1.pool_width(),
            Config::TokioPool512H1.pool_width()
        );
    }

    /// At depth 1 the two wide-pool configurations are the same cell. That is
    /// deliberate and published as a within-run reproducibility check, so the
    /// report needs to be able to recognise the pair.
    #[test]
    fn the_wide_pool_configurations_coincide_at_depth_one_and_not_beyond() {
        assert!(Config::TokioPool512H1.duplicates(Config::TokioPool512Hn, 1));
        assert!(!Config::TokioPool512H1.duplicates(Config::TokioPool512Hn, 8));
        assert!(
            !Config::TokioPool1.duplicates(Config::TokioPool512H1, 1),
            "these differ in pool width and must never be treated as one cell"
        );
        assert!(
            !Config::TokioPool512Hn.duplicates(Config::TokioPool512Hn, 1),
            "a configuration is not a duplicate of itself"
        );
    }

    /// Enumerates every ordered pair rather than hand-picking four.
    ///
    /// The hand-picked version above stayed inside the thread-pool family, so
    /// it never evaluated a cross-family pair — and `duplicates` compared
    /// `pool_width()` with `None == None` true, which made the plain ring, the
    /// registered ring and compio mutually "the same cell" at every depth.
    /// compio is the control that separates "completion-based" from "the ring";
    /// collapsing it into the ring would have removed the one comparison that
    /// keeps this arm's headline honest, in the published table, silently.
    ///
    /// The only true pair is the wide-pool one at depth 1.
    #[test]
    fn the_only_configurations_that_coincide_are_the_wide_pool_pair_at_depth_one() {
        for &depth in &[1usize, 8, 64] {
            for a in Config::all() {
                for b in Config::all() {
                    let expected = depth == 1
                        && matches!(
                            (a, b),
                            (Config::TokioPool512H1, Config::TokioPool512Hn)
                                | (Config::TokioPool512Hn, Config::TokioPool512H1)
                        );
                    assert_eq!(
                        a.duplicates(b, depth),
                        expected,
                        "depth {depth}: {} vs {} — a false positive here merges two \
                         distinct configurations into one cell in the published \
                         table; the ring, the registered ring and compio must never \
                         be merged, compio being the control",
                        a.slug(),
                        b.slug()
                    );
                }
            }
        }
    }

    #[test]
    fn the_read_flags_carry_both_bits() {
        // Neither constant may be zero. Without this, `X & 0 == 0` satisfies
        // the mask assertions below and the test passes while proving nothing.
        assert_ne!(FILE_FLAG_NO_BUFFERING.0, 0);
        assert_ne!(FILE_FLAG_OVERLAPPED.0, 0);
        assert_eq!(
            READ_FLAGS & FILE_FLAG_NO_BUFFERING.0,
            FILE_FLAG_NO_BUFFERING.0
        );
        assert_eq!(READ_FLAGS & FILE_FLAG_OVERLAPPED.0, FILE_FLAG_OVERLAPPED.0);
        // The pool's flags are unbuffered but deliberately NOT overlapped.
        assert_eq!(
            POOL_READ_FLAGS & FILE_FLAG_NO_BUFFERING.0,
            FILE_FLAG_NO_BUFFERING.0
        );
        assert_eq!(POOL_READ_FLAGS & FILE_FLAG_OVERLAPPED.0, 0);
        assert_ne!(
            FILE_FLAG_NO_BUFFERING.0, FILE_FLAG_OVERLAPPED.0,
            "the two flags must be distinct bits, or the assertions above are \
             satisfied by either one alone"
        );
    }

    // ---------------------------------------------------------- end to end
    //
    // One real unbuffered read per configuration, at `Config::small()` scale.
    // No timing, no ordering, no ratios — see the module header for why a
    // device-bound gate on CI would be worse than none.

    use crate::align::Alignment;
    use crate::backend::Buffer;
    use crate::unbuffered_workload::UnbufferedFile;

    /// A private directory per test, so no two tests share a working file.
    ///
    /// Sharing one would not merely race: a buffered open performed by any
    /// test poisons the file for unbuffered reads for the rest of the
    /// process's life, so a neighbour's setup can silently change what this
    /// one measures. That confound was committed in-tree once already.
    fn dir(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("win-ioring-unbuffered-{name}"));
        std::fs::create_dir_all(&path).expect("a scratch directory");
        path
    }

    /// Drives a ring driver alongside the work, the way `session.rs` does.
    ///
    /// A local copy rather than a reach into `session::drive_while`, which is
    /// private and belongs to the buffered arms' machinery. Duplicating six
    /// lines is cheaper than widening that module's surface for a test.
    fn drive_while<T>(driver: &win_ioring::runtime::Driver, work: impl Future<Output = T>) -> T {
        use futures::future::Either;
        use std::pin::pin;
        futures::executor::block_on(async {
            let driving = pin!(driver.drive());
            let work = pin!(work);
            match futures::future::select(driving, work).await {
                Either::Left(_) => panic!("the driver shut down while the work was still running"),
                Either::Right((outcome, _)) => outcome,
            }
        })
    }

    /// The bytes at `offset` that a correct read must deliver.
    fn expect_pattern(file: &UnbufferedFile, offset: u64, len: usize) -> Vec<u8> {
        use std::os::windows::fs::FileExt;
        let handle = file
            .path()
            .open_unbuffered()
            .expect("an unbuffered handle for the expectation");
        let align = Alignment::query(file.path().as_raw_path())
            .expect("the volume alignment")
            .granularity();
        let mut buf = AlignedBuf::new(len, align).expect("an aligned buffer");
        let n = handle
            .seek_read(&mut buf.spare()[..len], offset)
            .expect("a reference read");
        buf.spare()[..n].to_vec()
    }

    /// Everything an end-to-end test needs, built once per test.
    struct Fixture {
        file: UnbufferedFile,
        align: usize,
        block: usize,
    }

    fn fixture(name: &str) -> Fixture {
        let dir = dir(name);
        let align = Alignment::query(&dir)
            .expect("the volume alignment")
            .granularity();
        // Small on purpose: this proves the path runs, not what it costs.
        let file = UnbufferedFile::create(
            &dir,
            (align as u64) * 64,
            &Alignment::query(&dir).expect("the volume alignment"),
        )
        .expect("an unbuffered working file");
        Fixture {
            file,
            align,
            block: align,
        }
    }

    #[test]
    fn the_ring_reads_unbuffered_end_to_end() {
        let fx = fixture("ring-owned");
        let offset = (fx.align * 3) as u64;
        let expected = expect_pattern(&fx.file, offset, fx.block);

        let mut backend = UnbufferedIoRing::new(4, 4, fx.block, fx.align).expect("a ring backend");
        let driver = backend.take_driver().expect("a driver");
        let handle = backend.handle();
        let (n, bytes) = drive_while(&driver, async {
            let file = backend
                .open_read(fx.file.path().as_raw_path())
                .await
                .expect("an unbuffered open");
            let buffer = backend.take_buffer(fx.block).expect("a buffer");
            let (result, buffer) = backend
                .read_at(&file, buffer, fx.block as u32, offset)
                .await;
            (result.expect("a read"), buffer.bytes().to_vec())
        });
        handle.shutdown();
        futures::executor::block_on(driver.drive());
        assert_eq!(n as usize, fx.block);
        assert_eq!(bytes, expected);
    }

    #[test]
    fn the_registered_ring_reads_unbuffered_end_to_end() {
        let fx = fixture("ring-registered");
        let offset = (fx.align * 9) as u64;
        let expected = expect_pattern(&fx.file, offset, fx.block);

        let mut backend = UnbufferedIoRingRegistered::new(4).expect("a ring backend");
        let driver = backend.take_driver().expect("a driver");
        let handle = backend.handle();
        let (n, bytes) = drive_while(&driver, async {
            backend
                .register(4, fx.block, fx.align)
                .await
                .expect("a registration of aligned buffers");
            let file = backend
                .open_read(fx.file.path().as_raw_path())
                .await
                .expect("an unbuffered open");
            let buffer = backend.take_buffer(fx.block).expect("a buffer");
            let (result, buffer) = backend
                .read_at(&file, buffer, fx.block as u32, offset)
                .await;
            (result.expect("a read"), buffer.bytes().to_vec())
        });
        handle.shutdown();
        futures::executor::block_on(driver.drive());
        assert_eq!(n as usize, fx.block);
        assert_eq!(bytes, expected);
    }

    #[test]
    fn compio_reads_unbuffered_end_to_end() {
        let fx = fixture("compio");
        let offset = (fx.align * 5) as u64;
        let expected = expect_pattern(&fx.file, offset, fx.block);

        let backend = UnbufferedCompio::new(4, fx.block, fx.align).expect("a compio backend");
        let (n, bytes) = backend.block_on(async {
            let file = backend
                .open_read(fx.file.path().as_raw_path())
                .await
                .expect("an unbuffered open");
            let buffer = backend.take_buffer(fx.block).expect("a buffer");
            let (result, buffer) = backend
                .read_at(&file, buffer, fx.block as u32, offset)
                .await;
            (result.expect("a read"), buffer.bytes().to_vec())
        });
        assert_eq!(n as usize, fx.block);
        assert_eq!(bytes, expected);
    }

    #[test]
    fn every_thread_pool_configuration_reads_unbuffered_end_to_end() {
        for config in [
            Config::TokioPool1,
            Config::TokioPool512H1,
            Config::TokioPool512Hn,
        ] {
            let depth = 4;
            let fx = fixture(&format!("pool-{}", config.slug()));
            let offset = (fx.align * 7) as u64;
            let expected = expect_pattern(&fx.file, offset, fx.block);

            let backend = UnbufferedTokioFs::new(
                config.pool_width().expect("a pool width"),
                config.handles(depth),
                depth,
                fx.block,
                fx.align,
            )
            .expect("a thread-pool backend");

            let (n, bytes, handles) = backend.block_on(async {
                let file = backend
                    .open_read(fx.file.path().as_raw_path())
                    .await
                    .expect("an unbuffered open");
                let handles = file.len();
                let buffer = backend.take_buffer(fx.block).expect("a buffer");
                let (result, buffer) = backend
                    .read_at(&file, buffer, fx.block as u32, offset)
                    .await;
                (result.expect("a read"), buffer.bytes().to_vec(), handles)
            });
            assert_eq!(n as usize, fx.block, "{}", config.slug());
            assert_eq!(bytes, expected, "{}", config.slug());
            assert_eq!(
                handles,
                config.handles(depth),
                "{} opened the wrong number of handles",
                config.slug()
            );
        }
    }

    /// The handle set really is `depth` handles for the multi-handle
    /// configuration, and really is one for the rest — asserted against the
    /// live open rather than against `handles()` alone, so a backend that
    /// ignores its configuration is caught.
    #[test]
    fn the_multi_handle_configuration_opens_more_than_one_handle() {
        let fx = fixture("handle-count");
        let backend = UnbufferedTokioFs::new(512, 8, 8, fx.block, fx.align).expect("a backend");
        let opened = backend.block_on(async {
            backend
                .open_read(fx.file.path().as_raw_path())
                .await
                .expect("an unbuffered open")
                .len()
        });
        assert_eq!(opened, 8);

        let single = UnbufferedTokioFs::new(512, 1, 8, fx.block, fx.align).expect("a backend");
        let opened = single.block_on(async {
            single
                .open_read(fx.file.path().as_raw_path())
                .await
                .expect("an unbuffered open")
                .len()
        });
        assert_eq!(
            opened, 1,
            "the single-handle control must not scale its handles, or it stops \
             isolating pool width from handle count"
        );
    }

    /// Every handle **each backend actually opens** must read back with exactly
    /// the flags its configuration promises.
    ///
    /// The point is the phrase "actually opens". An earlier version of this
    /// test called the free function `open_unbuffered` and checked *its*
    /// result, which proved only that the helper works — no backend's
    /// `open_read` was bound to anything. Under that version, mutating
    /// `UnbufferedIoRing::open_read` to a plainly buffered, synchronous
    /// `std::fs::File::open` left the entire suite green. A buffered ring
    /// handle reads from the page cache at warm-cache speed, so that mutation
    /// would have manufactured exactly the ring victory this arm was built to
    /// avoid manufacturing, and additionally poisoned the working file for
    /// every later cell.
    ///
    /// Expectations differ by configuration: the completion-based backends must
    /// be unbuffered *and* asynchronous, the thread-pool backends unbuffered
    /// *and* synchronous (R3.3). Both properties are read back from the live
    /// handle via `NtQueryInformationFile`, and the synchronous one is compared
    /// for equality against `Config::read_flags` rather than merely asserted
    /// true, so a backend that gets the mode backwards is caught in either
    /// direction.
    #[test]
    fn every_backend_opens_handles_with_exactly_its_configured_flags() {
        let fx = fixture("flag-readback");
        let path = fx.file.path();

        for config in Config::all() {
            let expect_sync = config.read_flags() & FILE_FLAG_OVERLAPPED.0 == 0;
            let seen = backend_handle_modes(config, path).expect("the backend's handles");

            assert!(
                !seen.is_empty(),
                "{}: no handle was inspected, so this test proved nothing",
                config.slug()
            );

            for (i, (unbuffered, synchronous)) in seen.iter().copied().enumerate() {
                assert!(
                    unbuffered,
                    "{} handle {i}: the backend opened a handle WITHOUT \
                     FILE_NO_INTERMEDIATE_BUFFERING, so it reads from the page \
                     cache and every figure it produces is a warm-cache figure \
                     wearing an unbuffered label",
                    config.slug()
                );
                assert_eq!(
                    synchronous,
                    expect_sync,
                    "{} handle {i}: expected synchronous={expect_sync}. \
                     A completion-based backend on a synchronous handle \
                     silently collapses to depth one; a thread-pool backend on \
                     an overlapped handle stops measuring the file-object \
                     serialisation it exists to exhibit, and aborts the process \
                     in `seek_read`",
                    config.slug()
                );
            }
        }
    }

    /// The open cost is measured, not assumed, and it really does scale with
    /// the handle count.
    ///
    /// `open_cost` is the entire honesty mechanism for this arm's decision to
    /// open outside the timed region, so a test of it that cannot fail would
    /// leave that decision unguarded. An earlier version asserted only
    /// `cost <= 60s` — unfalsifiable — and separately re-asserted
    /// `Config::handles`, which tests the enum and not `open_cost`. Forcing
    /// `open_cost`'s handle count to 1 left every target green.
    #[test]
    fn open_cost_scales_with_the_handle_count() {
        let fx = fixture("open-cost");
        let path = fx.file.path().as_raw_path();
        const DEPTH: usize = 32;

        // Take the best of a few repeats on each side. This is a ratio between
        // two measurements on the same host in the same process, and the claim
        // is 32x apart, so it does not need the arm's noise band -- but a
        // single sample of a syscall can be preempted arbitrarily.
        let best = |config: Config| {
            (0..5)
                .map(|_| open_cost(config, path, DEPTH).expect("an open"))
                .min()
                .expect("five samples")
        };

        let one = best(Config::IoRingPlain);
        let many = best(Config::TokioPool512Hn);

        assert_eq!(Config::IoRingPlain.handles(DEPTH), 1);
        assert_eq!(Config::TokioPool512Hn.handles(DEPTH), DEPTH);
        assert!(
            many > one,
            "opening {DEPTH} handles ({many:?}) must cost more than opening one \
             ({one:?}); if it does not, `open_cost` is not counting handles and \
             the separately-reported setup cost of the multi-handle \
             configuration is understated"
        );
    }

    /// Collects `(unbuffered, synchronous)` for every handle a backend's
    /// `open_read` actually established.
    ///
    /// The modes are read while the backend still owns its handles, so nothing
    /// here has to borrow a handle past the backend's life.
    fn backend_handle_modes(
        config: Config,
        path: &crate::unbuffered_workload::UnbufferedPath,
    ) -> io::Result<Vec<(bool, bool)>> {
        use crate::unbuffered_workload::{is_synchronous, is_unbuffered};
        use std::os::windows::io::{AsRawHandle, FromRawHandle};

        /// Reads the mode off a raw handle without taking ownership of it.
        fn modes(raw: std::os::windows::io::RawHandle) -> io::Result<(bool, bool)> {
            // SAFETY: `raw` is a live file handle owned by the backend. The
            // `File` is wrapped in `ManuallyDrop` and never dropped, so the
            // handle is not closed here and ownership stays with the backend.
            let view = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_handle(raw) });
            Ok((is_unbuffered(&view)?, is_synchronous(&view)?))
        }

        let raw = path.as_raw_path();
        Ok(match config {
            Config::IoRingPlain => {
                let mut backend = UnbufferedIoRing::new(4, 1, 4096, 4096)?;
                let driver = backend.take_driver().expect("a driver");
                let handle = backend.handle();
                let seen = drive_while(&driver, async {
                    let file = backend.open_read(raw).await?;
                    modes(file.as_raw_handle().0)
                })?;
                handle.shutdown();
                futures::executor::block_on(driver.drive());
                vec![seen]
            }
            Config::IoRingRegistered => {
                let mut backend = UnbufferedIoRingRegistered::new(4)?;
                let driver = backend.take_driver().expect("a driver");
                let handle = backend.handle();
                let seen = drive_while(&driver, async {
                    let file = backend.open_read(raw).await?;
                    modes(file.as_raw_handle().0)
                })?;
                handle.shutdown();
                futures::executor::block_on(driver.drive());
                vec![seen]
            }
            Config::Compio => {
                let backend = UnbufferedCompio::new(1, 4096, 4096)?;
                let seen = backend.block_on(async {
                    let file = backend.open_read(raw).await?;
                    modes(file.as_raw_handle())
                })?;
                vec![seen]
            }
            Config::TokioPool1 | Config::TokioPool512H1 | Config::TokioPool512Hn => {
                let backend = UnbufferedTokioFs::new(
                    config.pool_width().expect("a pool width"),
                    config.handles(4),
                    1,
                    4096,
                    4096,
                )?;
                backend.block_on(async {
                    let files = backend.open_read(raw).await?;
                    files
                        .handles()
                        .iter()
                        .map(|f| modes(f.as_raw_handle()))
                        .collect::<io::Result<Vec<_>>>()
                })?
            }
        })
    }

    /// Every backend's *own* `open_read` and `open_write` are exercised here.
    ///
    /// `write_refusals` drives each configuration's real `open_write`, rather
    /// than asserting on the shared `unsupported_write()` helper. An earlier
    /// version did the latter — it constructed no backend and named no
    /// `Config`, which is exactly the defect B2 fixed for `open_read`, left
    /// standing for `open_write` in the same file. Replacing
    /// `UnbufferedCompio::open_write` with a real, working open left the whole
    /// suite green.
    #[test]
    fn writes_are_refused_by_every_configuration() {
        let fx = fixture("write-refusal");
        let path = fx.file.path();

        // Collect a witness from each backend rather than counting loop
        // iterations. An earlier version incremented a counter once per
        // iteration and compared it to `Config::all().len()` — a tautology,
        // and its failure message named a hazard it could not detect.
        // Reintroducing the defect for three of the six configurations left it
        // green. The witness is the backend's own `name()`, which only exists
        // if a backend was actually constructed.
        let mut witnesses = Vec::new();
        for config in Config::all() {
            let (name, e) = backend_write_refusal(config, path);
            let e =
                e.expect_err("this arm reads only; a backend that opens for writing is a defect");
            assert_eq!(
                e.kind(),
                io::ErrorKind::Unsupported,
                "{}: `open_write` must refuse with `Unsupported`, not {:?}",
                config.slug(),
                e.kind()
            );
            witnesses.push(name);
        }

        // `Config::name()` is a template: the multi-handle configuration
        // carries a literal `N handles`, which the constructed backend
        // substitutes with its real count. Substituting here as well keeps the
        // comparison a pin on "this config produced a backend naming itself
        // per its own template" rather than weakening it to a length check.
        // `handles(4)` mirrors what the helper above passes, so the two stay in
        // step if the small-config size changes.
        let mut expected: Vec<String> = Config::all()
            .iter()
            .map(|c| {
                c.name()
                    .replace("N handles", &format!("{} handles", c.handles(4)))
            })
            .collect();
        expected.sort();
        witnesses.sort();
        assert_eq!(
            witnesses, expected,
            "every configuration must have had a backend constructed and its \
             own `open_write` driven; a missing or duplicated name means some \
             configuration was never exercised"
        );
    }

    /// Drives one configuration's real `open_write`, returning the backend's own
    /// name as a witness that a backend was constructed at all.
    fn backend_write_refusal(
        config: Config,
        path: &crate::unbuffered_workload::UnbufferedPath,
    ) -> (String, io::Result<()>) {
        let raw = path.as_raw_path();
        match config {
            Config::IoRingPlain => {
                let mut backend = UnbufferedIoRing::new(4, 1, 4096, 4096).expect("a backend");
                let driver = backend.take_driver().expect("a driver");
                let handle = backend.handle();
                let r = drive_while(&driver, async { backend.open_write(raw).await.map(|_| ()) });
                handle.shutdown();
                futures::executor::block_on(driver.drive());
                (backend.name(), r)
            }
            Config::IoRingRegistered => {
                let mut backend = UnbufferedIoRingRegistered::new(4).expect("a backend");
                let driver = backend.take_driver().expect("a driver");
                let handle = backend.handle();
                let r = drive_while(&driver, async { backend.open_write(raw).await.map(|_| ()) });
                handle.shutdown();
                futures::executor::block_on(driver.drive());
                (backend.name(), r)
            }
            Config::Compio => {
                let backend = UnbufferedCompio::new(1, 4096, 4096).expect("a backend");
                let r = backend.block_on(async { backend.open_write(raw).await.map(|_| ()) });
                (backend.name(), r)
            }
            Config::TokioPool1 | Config::TokioPool512H1 | Config::TokioPool512Hn => {
                let backend = UnbufferedTokioFs::new(
                    config.pool_width().expect("a pool width"),
                    config.handles(4),
                    1,
                    4096,
                    4096,
                )
                .expect("a backend");
                let r = backend.block_on(async { backend.open_write(raw).await.map(|_| ()) });
                (backend.name(), r)
            }
        }
    }

    /// All four buffer pools fail loudly on exhaustion rather than quietly
    /// allocating a replacement inside the timed region.
    ///
    /// This guards a correction that runs *against* this crate: the thread-pool
    /// backend used to `alloc_zeroed` a fresh buffer when its pool ran dry,
    /// charging the honest competitor a hidden per-operation cost the ring
    /// never pays. Without a test, restoring that behaviour left the suite
    /// green.
    ///
    /// An earlier version of this test said "the buffer pools" while exercising
    /// two of the four, leaving the ring's own `take_aligned` path — where a
    /// silent allocation would charge *this crate* — unguarded. All four are
    /// covered now: an untested pool is an untested pool whichever way its
    /// failure would bias the result.
    #[test]
    fn an_exhausted_buffer_pool_is_an_error_not_a_silent_allocation() {
        /// `expect_err` needs `Debug` on the success type; the buffers do not
        /// have it, and adding it to satisfy a test would be the tail wagging
        /// the dog.
        fn err_or_panic<T>(r: io::Result<T>, what: &str) -> io::Error {
            match r {
                Ok(_) => panic!("{what}"),
                Err(e) => e,
            }
        }

        let pool = UnbufferedTokioFs::new(1, 1, 1, 4096, 4096).expect("a backend");
        let first = pool.take_buffer(4096).expect("the pool's only buffer");
        let e = err_or_panic(
            pool.take_buffer(4096),
            "an exhausted thread-pool buffer pool must refuse, not allocate a \
             replacement inside the timed region",
        );
        assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
        drop(first);

        let compio = UnbufferedCompio::new(1, 4096, 4096).expect("a backend");
        let held = compio.take_buffer(4096).expect("the pool's only buffer");
        let e = err_or_panic(
            compio.take_buffer(4096),
            "an exhausted compio buffer pool must refuse, not allocate",
        );
        assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
        drop(held);

        // The ring's owned-buffer pool goes through `take_aligned`, a separate
        // path. A silent allocation here would charge *this crate*, which is
        // the safe direction, but an untested pool is an untested pool.
        let ring = UnbufferedIoRing::new(4, 1, 4096, 4096).expect("a backend");
        let held = ring.take_buffer(4096).expect("the pool's only buffer");
        let e = err_or_panic(
            ring.take_buffer(4096),
            "an exhausted owned-buffer pool must refuse, not allocate",
        );
        assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
        drop(held);

        // And the registered pool, whose free list is indices rather than
        // buffers.
        let mut reg = UnbufferedIoRingRegistered::new(4).expect("a backend");
        let driver = reg.take_driver().expect("a driver");
        let handle = reg.handle();
        let e = drive_while(&driver, async {
            reg.register(1, 4096, 4096).await.expect("registration");
            let held = reg.take_buffer(4096).expect("the pool's only buffer");
            let e = err_or_panic(
                reg.take_buffer(4096),
                "an exhausted registered pool must refuse, not allocate",
            );
            drop(held);
            e
        });
        handle.shutdown();
        futures::executor::block_on(driver.drive());
        assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
    }

    /// A read larger than the buffer it was given is refused, not truncated.
    ///
    /// A truncated read is *faster* than a complete one, so a benchmark that
    /// silently truncates reports a better number for doing less work. The
    /// registered-buffer backend used to ignore the requested capacity
    /// entirely.
    ///
    /// All four pools, not two. The earlier version covered only the ring's
    /// pools, leaving compio's and the thread pool's capacity checks
    /// unexercised — and those are the paths where a silent truncation would
    /// make the *honest competitor* do less work per operation, which is the
    /// direction that flatters this crate.
    #[test]
    fn a_read_larger_than_its_buffer_is_refused_rather_than_truncated() {
        fn err_or_panic<T>(r: io::Result<T>, what: &str) -> io::Error {
            match r {
                Ok(_) => panic!("{what}"),
                Err(e) => e,
            }
        }

        let mut backend = UnbufferedIoRingRegistered::new(4).expect("a backend");
        let driver = backend.take_driver().expect("a driver");
        let handle = backend.handle();
        let e = drive_while(&driver, async {
            backend.register(2, 4096, 4096).await.expect("registration");
            err_or_panic(
                backend.take_buffer(8192),
                "a read twice the registered buffer size must be refused, not \
                 silently truncated to the registered size",
            )
        });
        handle.shutdown();
        futures::executor::block_on(driver.drive());
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);

        let owned = UnbufferedIoRing::new(4, 1, 4096, 4096).expect("a backend");
        let e = err_or_panic(
            owned.take_buffer(8192),
            "an over-large read must be refused",
        );
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);

        let compio = UnbufferedCompio::new(1, 4096, 4096).expect("a backend");
        let e = err_or_panic(
            compio.take_buffer(8192),
            "an over-large read must be refused by the compio pool too",
        );
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);

        let pool = UnbufferedTokioFs::new(1, 1, 1, 4096, 4096).expect("a backend");
        let e = err_or_panic(
            pool.take_buffer(8192),
            "an over-large read must be refused by the thread pool too — a \
             truncation here would make the honest competitor do less work",
        );
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
    }

    /// Each backend's published self-description must match the handle mode its
    /// configuration declares.
    ///
    /// `configuration()` is printed into the report, so a wrong word here is a
    /// wrong statement in the published record rather than a private comment.
    /// The thread-pool backend described itself as opening "overlapped" handles
    /// for the whole of the phase in which it did — and kept doing so after the
    /// flags were split, because the split touched the opener and not the
    /// sentence about it.
    ///
    /// This compares the string against `Config::read_flags`, a declaration.
    /// The binding to a *live* handle is
    /// `every_backend_opens_handles_with_exactly_its_configured_flags`; the two
    /// together pin string → declaration → handle.
    #[test]
    fn each_backend_describes_the_handle_mode_it_declares() {
        /// Whole-word match. `"asynchronous".contains("synchronous")` is true,
        /// so a substring test would fail a backend that improved its string
        /// to "overlapped (asynchronous) handle" — punishing a correction.
        fn says(text: &str, word: &str) -> bool {
            text.split(|c: char| !c.is_ascii_alphanumeric())
                .any(|w| w.eq_ignore_ascii_case(word))
        }

        for config in Config::all() {
            let text = backend_configuration(config).expect("a backend");
            let expect_sync = config.read_flags() & FILE_FLAG_OVERLAPPED.0 == 0;
            let (wanted, forbidden) = if expect_sync {
                ("synchronous", "overlapped")
            } else {
                ("overlapped", "synchronous")
            };
            assert!(
                says(&text, wanted),
                "{}: the published configuration string must say \"{wanted}\": {text:?}",
                config.slug()
            );
            assert!(
                !says(&text, forbidden),
                "{}: the published configuration string says \"{forbidden}\", \
                 which is the opposite of the handles this configuration opens: {text:?}",
                config.slug()
            );
            assert!(
                says(&text, "unbuffered"),
                "{}: the published configuration string must say \"unbuffered\": {text:?}",
                config.slug()
            );
        }
    }

    /// One configuration's published self-description.
    fn backend_configuration(config: Config) -> io::Result<String> {
        Ok(match config {
            Config::IoRingPlain => UnbufferedIoRing::new(4, 1, 4096, 4096)?.configuration(),
            Config::IoRingRegistered => UnbufferedIoRingRegistered::new(4)?.configuration(),
            Config::Compio => UnbufferedCompio::new(1, 4096, 4096)?.configuration(),
            Config::TokioPool1 | Config::TokioPool512H1 | Config::TokioPool512Hn => {
                UnbufferedTokioFs::new(
                    config.pool_width().expect("a pool width"),
                    config.handles(4),
                    1,
                    4096,
                    4096,
                )?
                .configuration()
            }
        })
    }

    /// The handles `open_cost` actually opens carry both configured bits.
    ///
    /// `open_cost` used to consult a third copy of the flag decision — after
    /// `read_flags` and the backends themselves — and this test checked only
    /// the `FILE_FLAG_OVERLAPPED` bit of it. Pointing that copy at a plainly
    /// buffered `File::open` left all eighteen tests green, so the copy was
    /// free to drift on `FILE_FLAG_NO_BUFFERING` — the one bit the arm exists
    /// to measure, and the one whose loss silently poisons the working file.
    ///
    /// The copy is gone; `open_cost` derives from `read_flags`. This now
    /// verifies the derivation end to end, on a real handle, in **both**
    /// dimensions, so neither bit can be lost without a failure.
    #[test]
    fn the_handles_open_cost_opens_carry_both_configured_flags() {
        use crate::unbuffered_workload::{is_synchronous, is_unbuffered};

        let fx = fixture("opener-agreement");
        let path = fx.file.path().as_raw_path();
        for config in Config::all() {
            let flags = config.read_flags();
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(flags)
                .open(path)
                .expect("an open");
            assert!(
                is_unbuffered(&file).expect("a mode read-back"),
                "{}: `open_cost` would open a buffered handle, which poisons the \
                 arm's working file for the life of the process",
                config.slug()
            );
            assert_eq!(
                is_synchronous(&file).expect("a mode read-back"),
                flags & FILE_FLAG_OVERLAPPED.0 == 0,
                "{}: `open_cost` opens a handle mode this configuration never uses",
                config.slug()
            );
        }
    }
}
