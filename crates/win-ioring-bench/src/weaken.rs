//! Backends that deliberately do less, so a test can prove the comparison
//! notices.
//!
//! The fairness layer's whole claim is that a backend which did not do the same
//! work is **rejected rather than reported**. A test can only settle that by
//! running a backend that really does less, through the same function a real
//! measurement calls. That is what this module supplies: [`Weakened`] wraps any
//! [`Backend`] and is handed to [`crate::scenario::run`] by
//! [`crate::session::Prepared::one`], so a weakened run travels the identical
//! path — the same scenario, the same runner, the same trace, the same
//! [`crate::fairness::Ledger`].
//!
//! Nothing in the benchmark ever constructs one. It is ordinary `pub` code
//! rather than `#[cfg(test)]` because an integration test cannot see a library's
//! test-only items, and it is not behind a cargo feature because `cargo check
//! --workspace --all-targets` runs without `--all-features` and would then
//! compile a test file whose imports had vanished.
//!
//! # Two constraints, both load-bearing
//!
//! **[`Weakness::SkipsWork`] intercepts reads only.** Write-then-read writes
//! `operations` blocks and reads the same blocks back. Skipping a write would
//! leave the file short, and the read-back would then run at or past the end of
//! it — which a backend may report as an `io::Error` that propagates out before
//! the comparator is ever consulted. The test would fail for entirely the wrong
//! reason, and the weakening would be untested. Skipping only reads leaves every
//! scenario's file exactly the length an honest run leaves it, so the rejection
//! is a delivery difference in every case. Every scenario has a read phase, so
//! the weakening is non-vacuous everywhere.
//!
//! **[`Weakness::HollowDelivery`] fills with a single repeated byte.**
//! `workload::ensure_file` fills the read file with `(i % 251)` and
//! `scenario::write_then_read` builds its write pattern the same way, so
//! reaching for the scenario's own pattern would produce a "weakening" whose
//! bytes *agree* with the honest run's — a weakening that does not weaken, and a
//! test that passes for no reason. A mod-251 ramp is strictly increasing over
//! any block longer than one byte, so no block either file can produce is ever a
//! constant run: a constant fill differs from the honest bytes for every
//! operation.
//!
//! An implementer who removes either constraint produces a weakening that fails
//! for the wrong reason or does not weaken at all.
//!
//! # One weakening varies between runs, and has to
//!
//! [`Weakness::HollowFromRun`] is the only weakening that behaves differently on
//! different runs of the same combination, and it exists because two properties
//! of [`crate::harness::measure_combination`] cannot be observed by a weakening
//! that does not: that the trace handed to the ledger comes from the last
//! **timed** iteration rather than from the untimed warm-up, and that a backend
//! drifting *between* iterations is rejected. A uniform weakening makes the
//! warm-up and every iteration alike, so a measurement that verified the wrong
//! one of them looks identical. Its counter is a thread-local rather than state
//! on the wrapper, because the wrapper is rebuilt for every run; see
//! [`reset_runs`].

use std::cell::Cell;
use std::io;
use std::path::Path;

use crate::backend::{Backend, Buffer, OpResult};

thread_local! {
    /// How many runs weakened by [`Weakness::HollowFromRun`] this thread has
    /// begun since the last [`reset_runs`].
    ///
    /// Per thread rather than global: one combination's runs — the warm-up and
    /// every iteration — all happen on the thread that called `block_on`, and a
    /// process-global counter would be raced by the test binary's other threads.
    static RUNS: Cell<usize> = const { Cell::new(0) };
}

/// Forgets how many runs this thread has begun.
///
/// Call before each combination measured with [`Weakness::HollowFromRun`], so
/// the run index that weakening counts from is that combination's own.
pub fn reset_runs() {
    RUNS.with(|runs| runs.set(0));
}

/// Returns the index of the run starting now and counts it.
fn next_run() -> usize {
    RUNS.with(|runs| {
        let index = runs.get();
        runs.set(index + 1);
        index
    })
}

/// How a backend has been weakened.
///
/// [`Weakness::None`] is what every real measurement passes; the other two exist
/// only so a test can watch the comparator reject them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Weakness {
    /// Not weakened at all. The backend does exactly what it does.
    None,
    /// One read in every [`SKIP_EVERY`] is not performed at all: no I/O reaches
    /// the platform, the buffer is handed straight back untouched, and a
    /// transfer of zero bytes is reported.
    ///
    /// Less work is done and less arrives in application-readable memory, so the
    /// trace records a smaller delivered total than an honest run's.
    SkipsWork,
    /// Every operation is performed and reports its full transfer count, but the
    /// buffer handed back is overwritten with [`HOLLOW`] to the same length, so
    /// nothing the application reads came from the file.
    ///
    /// The exact shape of the registered-read defect that motivated the digest:
    /// a transfer count with nothing readable behind it.
    HollowDelivery,
    /// [`Weakness::HollowDelivery`], but only from the given run of the job
    /// onward — the untimed warm-up being run 0, the first timed iteration run 1
    /// and so on, counted per thread from the last [`reset_runs`].
    ///
    /// The only weakening that is not the same on every run, and the only one
    /// that can say *which* run's trace a measurement verified. With `1` the
    /// warm-up is honest and every timed iteration is hollow, so a measurement
    /// that verified the warm-up would report a weakened run as clean. With `2`
    /// and two timed iterations the first is honest and the last is not, which
    /// nothing but the first-versus-last comparison can catch.
    HollowFromRun(usize),
}

/// The byte a hollow delivery fills with.
///
/// Any single repeated byte would do; what matters is that it is *constant*.
/// See this module's documentation for why the scenario's own pattern would not.
pub const HOLLOW: u8 = 0xA5;

/// One read in this many is skipped by [`Weakness::SkipsWork`].
///
/// Counted from the **first** read rather than the fourth, so the weakening
/// still bites on a scenario with fewer than four of them and no configuration
/// can make the tests vacuous by shrinking.
pub const SKIP_EVERY: usize = 4;

/// A backend that does less than the one it wraps.
///
/// Borrows rather than owns, because the backend it weakens is the one
/// [`crate::session::Prepared`] already built and holds for the life of the
/// benchmark.
pub struct Weakened<'a, B: Backend> {
    inner: &'a B,
    weakness: Weakness,
    /// Whether *this* run delivers hollow buffers.
    ///
    /// Decided once, when the wrapper is built, because
    /// [`Weakness::HollowFromRun`] is a property of the run rather than of the
    /// operation and consulting the counter per operation would advance it.
    hollow: bool,
    /// How many reads have been issued, so the skip is deterministic: the
    /// scenario calls `read_at` in issue order even though completions arrive in
    /// any order.
    reads: Cell<usize>,
}

impl<'a, B: Backend> Weakened<'a, B> {
    /// Wraps `inner`, weakened as `weakness` says.
    pub fn new(inner: &'a B, weakness: Weakness) -> Self {
        let hollow = match weakness {
            Weakness::HollowDelivery => true,
            Weakness::HollowFromRun(from) => next_run() >= from,
            Weakness::None | Weakness::SkipsWork => false,
        };
        Self {
            inner,
            weakness,
            hollow,
            reads: Cell::new(0),
        }
    }

    /// Whether the `index`th read of this run is one of the skipped ones.
    fn skips(&self, index: usize) -> bool {
        self.weakness == Weakness::SkipsWork && index.is_multiple_of(SKIP_EVERY)
    }

    /// Overwrites what an operation delivered, if this run is a hollow one.
    ///
    /// Applied *after* the operation completed, so a write still puts the honest
    /// bytes on disk and only the trace's view of what came back is hollowed.
    fn hollow(&self, transferred: u32, buffer: &mut B::Buf) -> io::Result<()> {
        if !self.hollow {
            return Ok(());
        }
        buffer.fill(&vec![HOLLOW; transferred as usize])
    }
}

impl<B: Backend> Backend for Weakened<'_, B> {
    type Buf = B::Buf;
    type File = B::File;

    fn name(&self) -> String {
        self.inner.name()
    }

    fn configuration(&self) -> String {
        format!(
            "{} — weakened: {:?}",
            self.inner.configuration(),
            self.weakness
        )
    }

    async fn open_read(&self, path: &Path) -> io::Result<Self::File> {
        self.inner.open_read(path).await
    }

    async fn open_write(&self, path: &Path) -> io::Result<Self::File> {
        self.inner.open_write(path).await
    }

    fn take_buffer(&self, capacity: usize) -> io::Result<Self::Buf> {
        self.inner.take_buffer(capacity)
    }

    fn put_buffer(&self, buffer: Self::Buf) {
        self.inner.put_buffer(buffer);
    }

    async fn read_at(
        &self,
        file: &Self::File,
        buffer: Self::Buf,
        len: u32,
        offset: u64,
    ) -> OpResult<u32, Self::Buf> {
        let index = self.reads.get();
        self.reads.set(index + 1);
        if self.skips(index) {
            // No I/O at all, and the buffer goes back exactly as it arrived: the
            // application sees nothing from this offset.
            return (Ok(0), buffer);
        }
        let (result, mut buffer) = self.inner.read_at(file, buffer, len, offset).await;
        if let Ok(transferred) = result
            && let Err(e) = self.hollow(transferred, &mut buffer)
        {
            return (Err(e), buffer);
        }
        (result, buffer)
    }

    async fn write_at(
        &self,
        file: &Self::File,
        buffer: Self::Buf,
        len: u32,
        offset: u64,
    ) -> OpResult<u32, Self::Buf> {
        // Never skipped. See this module's documentation: a short file turns the
        // read-back into an `io::Error` rather than the mismatch under test.
        let (result, mut buffer) = self.inner.write_at(file, buffer, len, offset).await;
        if let Ok(transferred) = result
            && let Err(e) = self.hollow(transferred, &mut buffer)
        {
            return (Err(e), buffer);
        }
        (result, buffer)
    }

    async fn sync(&self, file: &Self::File) -> io::Result<()> {
        self.inner.sync(file).await
    }
}
