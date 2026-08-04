//! Keeping a bounded number of operations in flight, and observing how many
//! actually were.
//!
//! The achieved depth is measured here rather than asked of a backend. Only this
//! layer knows the figure in terms both backends share: one exposes an
//! outstanding count that includes work its kernel has not accepted, and the
//! other exposes nothing comparable at all.
//!
//! What this **cannot** see is a backend serialising operations below its own
//! interface — no available instrument can — so the report prints the depth
//! beside each backend's configuration rather than letting it stand alone.

use std::io;

use futures::stream::{FuturesUnordered, StreamExt};

use crate::backend::{Backend, Buffer};
use crate::verify::Trace;

/// How many operations may be outstanding at once.
pub type Depth = usize;

/// How a run spreads its operations over time.
///
/// Both shapes keep at most the configured number outstanding; they differ in
/// when they refill. The distinction matters because it decides how much work a
/// single submission to the kernel can carry, which is the quantity the
/// completion-based design's central claim is about.
///
/// Neither shape is better than the other. A rolling window keeps the ring full
/// and is the natural shape for a server with continuous arrivals; a batched
/// window drains to zero at each boundary, leaving the ring idle at the tail,
/// and is the natural shape for an application that has a known set of reads to
/// do at once. Both are real application patterns, which is why both are
/// measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// Fill to the configured depth, await one completion, refill one.
    ///
    /// Depth stays at the configured value for almost the whole run, so
    /// operations are issued a few at a time and a submission covers far fewer
    /// entries than the depth suggests.
    Rolling,
    /// Issue a whole batch, drain it to zero, then issue the next.
    ///
    /// Every operation in a batch is built before any of them is awaited, so
    /// one submission can cover the entire batch.
    Batched,
}

impl Shape {
    /// Whether the refill loop may run, given whether anything is outstanding.
    ///
    /// This is a gate on **entering** the refill, deliberately separate from the
    /// refill's own bound. Folding it into that bound instead — as a further
    /// conjunct on the `while` — would let the batched shape push exactly one
    /// future, observe that something was now outstanding, and stop: a serial
    /// depth-one run wearing a batch's name, which would publish a scenario that
    /// measured nothing.
    fn may_refill(self, nothing_outstanding: bool) -> bool {
        match self {
            Shape::Rolling => true,
            Shape::Batched => nothing_outstanding,
        }
    }
}

/// The mean depth a run of this shape must sustain, given its size.
///
/// The runner samples the outstanding count once before each await, so the
/// sequence of samples — and therefore its mean — is fully determined by the
/// shape, the operation count and the configured depth. That makes the expected
/// value a closed form rather than an empirical observation, and lets a wrong
/// shape be detected rather than merely described.
///
/// Every value this returns for the configurations the suite runs is a dyadic
/// rational, so the division is exact and the comparison against a measured mean
/// needs no meaningful tolerance.
pub fn predicted_mean_depth(shape: Shape, count: usize, configured: Depth) -> f64 {
    let n = configured.max(1) as u64;
    let count = count as u64;
    if count == 0 {
        return 0.0;
    }
    // The sum of a full descending drain from `d` down to one.
    let drain = |d: u64| d * (d + 1) / 2;
    let weighted = match shape {
        // Rising to the depth is folded into the steady state: the run holds
        // exactly `n` outstanding for every sample until the last `n`, which
        // descend to one as the tail drains.
        Shape::Rolling if count >= n => (count - n) * n + drain(n),
        // Too few operations to reach the configured depth at all, so the whole
        // run is the descending tail.
        Shape::Rolling => drain(count),
        // Whole batches each contribute a full drain; a final partial batch
        // contributes a shorter one.
        Shape::Batched => {
            let whole = count / n;
            let tail = count % n;
            whole * drain(n) + drain(tail)
        }
    };
    weighted as f64 / count as f64
}

/// Whether a run's measured depth agreed with the shape it declared.
///
/// Separate from [`Shortfall`], which stays an annotation on the report. This is
/// the verdict that can fail a run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ShapeCheck {
    /// The measured mean matched the prediction for the declared shape.
    Matched {
        /// What the shape predicted.
        predicted: f64,
        /// What the run achieved.
        measured: f64,
    },
    /// The measured mean disagreed, but the buffer pool could not supply a whole
    /// batch — a bound belonging to the configuration, not to the runner's
    /// shape, and so not the runner's fault.
    ///
    /// This is the only permitted excuse for a disagreement, and it is
    /// deliberately narrow: it is consulted **after** the comparison, so it can
    /// forgive a disagreement but can never manufacture agreement that was not
    /// there.
    PoolBound {
        /// What the shape predicted.
        predicted: f64,
        /// What the run achieved.
        measured: f64,
    },
    /// The measured mean disagreed and the pool was not at fault. The run did
    /// not drive the shape it said it would.
    Mismatched {
        /// What the shape predicted.
        predicted: f64,
        /// What the run achieved.
        measured: f64,
    },
}

impl ShapeCheck {
    /// Whether this verdict should fail the run.
    pub fn is_failure(self) -> bool {
        matches!(self, ShapeCheck::Mismatched { .. })
    }

    /// What the declared shape predicted.
    pub fn predicted(self) -> f64 {
        match self {
            ShapeCheck::Matched { predicted, .. }
            | ShapeCheck::PoolBound { predicted, .. }
            | ShapeCheck::Mismatched { predicted, .. } => predicted,
        }
    }

    /// What the run achieved.
    pub fn measured(self) -> f64 {
        match self {
            ShapeCheck::Matched { measured, .. }
            | ShapeCheck::PoolBound { measured, .. }
            | ShapeCheck::Mismatched { measured, .. } => measured,
        }
    }
}

/// How far a measured mean may sit from an exactly representable prediction.
///
/// Guards the division that produces the measured mean, nothing more: every
/// prediction the suite compares against is a dyadic rational and therefore
/// exact, so a genuine shape disagreement is never within this of the target.
const DEPTH_TOLERANCE: f64 = 1e-9;

/// Why a run did not reach its configured depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shortfall {
    /// The configured depth was reached.
    None,
    /// Fewer operations exist than the depth allows, or the backend's pool is
    /// smaller than it. Expected, and no reflection on the backend.
    Expected,
    /// The backend did not sustain a depth it had the work and the buffers for.
    /// The one that casts doubt on a comparison.
    Unexpected,
}

/// What a run achieved, in terms of concurrency.
#[derive(Debug, Clone, Copy)]
pub struct Achieved {
    /// The greatest number outstanding at once.
    pub peak: usize,
    /// Mean of the outstanding count, sampled once immediately before each
    /// await.
    ///
    /// A mean over completions rather than over time: each of the run's
    /// completions contributes one sample, taken just before that completion was
    /// awaited. Nothing here measures duration, so this is not a time-weighted
    /// figure.
    pub mean: f64,
    /// How the run fell short, if it did.
    pub shortfall: Shortfall,
}

/// Drives a bounded set of operations, recording what completed.
pub struct Runner<'a, B: Backend> {
    backend: &'a B,
    configured: Depth,
    shape: Shape,
    peak: usize,
    weighted: u64,
    samples: u64,
    starved: bool,
    /// Set when the pool could not supply a buffer, which bounds depth for a
    /// reason belonging to the configuration rather than to the backend.
    pool_bound: bool,
}

impl<'a, B: Backend> Runner<'a, B> {
    /// Starts a runner bounded at `depth`, driving `shape`.
    pub fn new(backend: &'a B, depth: Depth, shape: Shape) -> Self {
        Self {
            backend,
            configured: depth.max(1),
            shape,
            peak: 0,
            weighted: 0,
            samples: 0,
            starved: false,
            pool_bound: false,
        }
    }

    /// Runs `count` operations, keeping the configured number outstanding.
    ///
    /// `make` produces the *n*th operation as a future yielding its file offset
    /// alongside its outcome, so a completion can be matched to what was asked
    /// for without depending on the order completions arrive in.
    ///
    /// A `WouldBlock` from `make` means the backend's pool is momentarily empty;
    /// the runner drains one operation and tries again, and records that depth
    /// was bounded by the pool.
    pub async fn run<F, Fut>(
        &mut self,
        count: usize,
        phase: crate::verify::Phase,
        trace: &mut Trace,
        mut make: F,
    ) -> io::Result<()>
    where
        F: FnMut(usize) -> io::Result<Fut>,
        Fut: Future<Output = (u64, io::Result<(u32, B::Buf)>)>,
    {
        let mut pending = FuturesUnordered::new();
        let mut issued = 0;
        loop {
            // The gate is on entering the refill, not on the refill's bound. See
            // `Shape::may_refill`: making it a further conjunct below would
            // collapse the batched shape into a serial run of depth one.
            if self.shape.may_refill(pending.is_empty()) {
                while issued < count && pending.len() < self.configured {
                    match make(issued) {
                        Ok(fut) => {
                            pending.push(fut);
                            issued += 1;
                        }
                        // A pool that cannot supply the *rest* of a batch bounds
                        // depth by the configuration rather than by the shape,
                        // and the run continues: draining what is outstanding
                        // returns buffers. The guard is what makes that true. A
                        // pool that cannot supply even the *first* buffer while
                        // nothing is outstanding has nothing to drain and no way
                        // to make progress, so that case is a genuine error and
                        // falls through — this is why the batched shape, whose
                        // refill always begins empty, does not need the guard
                        // widened.
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock && !pending.is_empty() => {
                            self.pool_bound = true;
                            break;
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
            if pending.is_empty() {
                break;
            }
            let outstanding = pending.len();
            self.peak = self.peak.max(outstanding);
            if issued >= count && outstanding < self.configured {
                self.starved = true;
            }
            let Some((offset, outcome)) = pending.next().await else {
                break;
            };
            self.weighted += outstanding as u64;
            self.samples += 1;
            let (transferred, buffer) = outcome?;
            trace.delivered(phase, offset, transferred, buffer.bytes());
            self.backend.put_buffer(buffer);
        }
        Ok(())
    }

    /// Returns what the run achieved.
    pub fn achieved(&self, operations: usize) -> Achieved {
        let shortfall = if self.peak >= self.configured {
            Shortfall::None
        } else if self.starved || self.pool_bound || operations < self.configured {
            Shortfall::Expected
        } else {
            Shortfall::Unexpected
        };
        Achieved {
            peak: self.peak,
            mean: self.mean(),
            shortfall,
        }
    }

    /// Checks the measured mean depth against what the declared shape predicts.
    ///
    /// The verdict [`Shortfall`] cannot give. A shortfall says only that the
    /// peak fell below the configured depth, is reached by several unrelated
    /// routes, and reaches nothing that can fail a run. This compares the run
    /// against an exact expectation derived from the shape it said it would
    /// drive, so a run that quietly drove a different shape is detectable.
    pub fn shape_check(&self, count: usize) -> ShapeCheck {
        let predicted = predicted_mean_depth(self.shape, count, self.configured);
        let measured = self.mean();
        if (measured - predicted).abs() <= DEPTH_TOLERANCE {
            ShapeCheck::Matched {
                predicted,
                measured,
            }
        } else if self.pool_bound {
            // Consulted only after the comparison has already disagreed, so this
            // can forgive a disagreement but can never assert an agreement.
            ShapeCheck::PoolBound {
                predicted,
                measured,
            }
        } else {
            ShapeCheck::Mismatched {
                predicted,
                measured,
            }
        }
    }

    /// The shape this runner was told to drive.
    pub fn shape(&self) -> Shape {
        self.shape
    }

    /// Mean of the outstanding count over the run's completions.
    fn mean(&self) -> f64 {
        if self.samples == 0 {
            0.0
        } else {
            self.weighted as f64 / self.samples as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{Buffer, OpResult};
    use crate::verify::{Phase, Trace};

    /// A backend that owns no files and performs no I/O.
    ///
    /// `Runner` reaches its backend for exactly one thing — returning a buffer
    /// to the pool — so a fake that counts those returns is enough to drive the
    /// whole runner. The operations themselves are supplied by the caller's
    /// `make` closure, which is what lets a test decide precisely when the pool
    /// runs dry.
    struct FakeBackend {
        returned: std::cell::Cell<usize>,
    }

    impl FakeBackend {
        fn new() -> Self {
            Self {
                returned: std::cell::Cell::new(0),
            }
        }
    }

    #[derive(Default)]
    struct FakeBuf(Vec<u8>);

    impl Buffer for FakeBuf {
        fn bytes(&self) -> &[u8] {
            &self.0
        }
        fn fill(&mut self, src: &[u8]) -> io::Result<()> {
            self.0.clear();
            self.0.extend_from_slice(src);
            Ok(())
        }
    }

    impl Backend for FakeBackend {
        type Buf = FakeBuf;
        type File = ();

        fn name(&self) -> String {
            "fake".into()
        }
        fn configuration(&self) -> String {
            "fake".into()
        }
        fn open_read(&self, _path: &std::path::Path) -> io::Result<Self::File> {
            Ok(())
        }
        fn open_write(&self, _path: &std::path::Path) -> io::Result<Self::File> {
            Ok(())
        }
        fn take_buffer(&self, _capacity: usize) -> io::Result<Self::Buf> {
            Ok(FakeBuf::default())
        }
        fn put_buffer(&self, _buffer: Self::Buf) {
            self.returned.set(self.returned.get() + 1);
        }
        fn read_at(
            &self,
            _file: &Self::File,
            buffer: Self::Buf,
            len: u32,
            _offset: u64,
        ) -> impl Future<Output = OpResult<u32, Self::Buf>> {
            std::future::ready((Ok(len), buffer))
        }
        fn write_at(
            &self,
            _file: &Self::File,
            buffer: Self::Buf,
            len: u32,
            _offset: u64,
        ) -> impl Future<Output = OpResult<u32, Self::Buf>> {
            std::future::ready((Ok(len), buffer))
        }
        fn sync(&self, _file: &Self::File) -> impl Future<Output = io::Result<()>> {
            std::future::ready(Ok(()))
        }
    }

    /// Drives `count` trivially-ready operations and returns what was achieved.
    ///
    /// `supply` decides how many buffers the pool can hand out before it starts
    /// refusing, which is how the pool-bound path is reached at all.
    fn drive(
        shape: Shape,
        count: usize,
        depth: Depth,
        supply: usize,
    ) -> (Achieved, ShapeCheck, usize) {
        let backend = FakeBackend::new();
        let mut runner = Runner::new(&backend, depth, shape);
        let mut trace = Trace::new();
        // How many operations had been *built* at the moment the first future
        // body ran. In a batched shape that is the whole batch, because nothing
        // is awaited until the batch is complete.
        let built_when_first_polled = std::cell::Cell::new(None::<usize>);
        let handed_out = std::cell::Cell::new(0_usize);
        let built = std::cell::Cell::new(0_usize);

        let result = futures::executor::block_on(runner.run(
            count,
            Phase::Read,
            &mut trace,
            |i| {
                // Buffers are returned to the pool as operations complete, so
                // "supply" bounds how many may be outstanding at once, not how
                // many the run may issue in total.
                if handed_out.get() >= supply {
                    return Err(io::Error::from(io::ErrorKind::WouldBlock));
                }
                handed_out.set(handed_out.get() + 1);
                built.set(built.get() + 1);
                let handed_out = &handed_out;
                let built = &built;
                let first = &built_when_first_polled;
                Ok(async move {
                    if first.get().is_none() {
                        first.set(Some(built.get()));
                    }
                    handed_out.set(handed_out.get() - 1);
                    (i as u64, Ok((0_u32, FakeBuf::default())))
                })
            },
        ));
        result.expect("the fake backend never fails an operation");
        (
            runner.achieved(count),
            runner.shape_check(count),
            built_when_first_polled.get().unwrap_or(0),
        )
    }

    /// Every value the suite's configurations predict, checked exactly.
    ///
    /// The default-configuration values are the published ones and are checked
    /// by hand once per measurement run; the small-configuration values are the
    /// ones every `cargo test` actually produces, so they are the ones with
    /// teeth. Both sets are here because a check that only covers the figures
    /// nobody runs is not a check.
    #[test]
    fn predicted_depths_are_exact_for_every_configuration() {
        // Default configuration: depths 1, 8, 64 with 256/512/128 operations.
        let cases = [
            // (shape, count, depth, expected)
            (Shape::Rolling, 256, 1, 1.0),
            (Shape::Rolling, 256, 8, 7.890625),
            (Shape::Rolling, 256, 64, 56.125),
            (Shape::Rolling, 512, 1, 1.0),
            (Shape::Rolling, 512, 8, 7.9453125),
            (Shape::Rolling, 512, 64, 60.0625),
            (Shape::Rolling, 128, 1, 1.0),
            (Shape::Rolling, 128, 8, 7.78125),
            (Shape::Rolling, 128, 64, 48.25),
            (Shape::Batched, 256, 8, 4.5),
            (Shape::Batched, 256, 64, 32.5),
            // Small configuration: depths 1, 4 with 64/64/16 operations. These
            // are what `cargo test` drives, via the fairness tests at depth 4
            // and the comparison test at depth 1.
            (Shape::Rolling, 64, 1, 1.0),
            (Shape::Rolling, 64, 4, 3.90625),
            (Shape::Rolling, 16, 1, 1.0),
            (Shape::Rolling, 16, 4, 3.625),
            (Shape::Batched, 64, 1, 1.0),
            (Shape::Batched, 64, 4, 2.5),
            (Shape::Batched, 16, 4, 2.5),
        ];
        for (shape, count, depth, expected) in cases {
            let got = predicted_mean_depth(shape, count, depth);
            assert_eq!(
                got, expected,
                "{shape:?} with {count} operations at depth {depth} \
                 must predict exactly {expected}, got {got}"
            );
        }
    }

    /// Fewer operations than the depth allows: the whole run is a drain.
    #[test]
    fn a_run_shorter_than_its_depth_is_all_tail() {
        // Samples descend 3, 2, 1 whichever shape is asked for, because the
        // batch and the run are the same thing.
        assert_eq!(predicted_mean_depth(Shape::Rolling, 3, 8), 2.0);
        assert_eq!(predicted_mean_depth(Shape::Batched, 3, 8), 2.0);
    }

    /// A count that is not a whole multiple of the depth leaves a short batch.
    #[test]
    fn a_partial_final_batch_lowers_the_predicted_mean() {
        // 10 operations at depth 4: batches of 4, 4, 2.
        // (10 + 10 + 3) / 10 = 2.3
        assert_eq!(predicted_mean_depth(Shape::Batched, 10, 4), 2.3);
        // A whole multiple has no tail, so it sits exactly at (N + 1) / 2.
        assert_eq!(predicted_mean_depth(Shape::Batched, 12, 4), 2.5);
    }

    /// Each shape achieves exactly what it predicts, when actually run.
    ///
    /// The prediction and the runner are separate pieces of code; this is what
    /// stops them drifting apart. A prediction nothing is measured against is
    /// arithmetic, not a check.
    #[test]
    fn a_run_achieves_the_mean_its_shape_predicts() {
        for (shape, count, depth) in [
            (Shape::Rolling, 64, 4),
            (Shape::Rolling, 64, 8),
            (Shape::Rolling, 16, 4),
            (Shape::Rolling, 64, 1),
            (Shape::Batched, 64, 4),
            (Shape::Batched, 64, 8),
            (Shape::Batched, 64, 1),
            (Shape::Batched, 10, 4),
        ] {
            let (achieved, check, _) = drive(shape, count, depth, usize::MAX);
            let expected = predicted_mean_depth(shape, count, depth);
            assert_eq!(
                achieved.mean, expected,
                "{shape:?} {count}@{depth} measured {} but predicts {expected}",
                achieved.mean
            );
            assert!(
                matches!(check, ShapeCheck::Matched { .. }),
                "{shape:?} {count}@{depth} should match, got {check:?}"
            );
            assert!(!check.is_failure());
        }
    }

    /// The batched shape builds a whole batch before awaiting any of it.
    ///
    /// This is the property the crate's batching claim rests on: every entry in
    /// a batch is built before the driver gets a chance to run, so one
    /// submission can cover them all. It is asserted rather than assumed —
    /// batch *completeness* is what matters here, not the order within a batch.
    #[test]
    fn a_batch_is_fully_built_before_anything_is_awaited() {
        let (_, _, built_before_first_await) = drive(Shape::Batched, 64, 8, usize::MAX);
        assert_eq!(
            built_before_first_await, 8,
            "all eight of the batch must be built before the first completion"
        );

        // The rolling shape is the contrast: it awaits with the window full,
        // then tops it up one at a time.
        let (_, _, rolling) = drive(Shape::Rolling, 64, 8, usize::MAX);
        assert_eq!(rolling, 8, "the rolling window also fills before its first await");
    }

    /// A pool too small for a whole batch bounds depth without failing the run.
    ///
    /// This is the one permitted excuse for a depth disagreement, and it is the
    /// only route to that code path: at the suite's real sizing the pool always
    /// holds a whole batch. Without this test the exemption would be an
    /// untested branch guarding the only check that can fail a run.
    #[test]
    fn a_pool_smaller_than_the_batch_is_bounded_not_failed() {
        // Depth 8, but the pool can only ever supply 3 buffers at once.
        let (achieved, check, _) = drive(Shape::Batched, 64, 8, 3);
        assert!(
            matches!(check, ShapeCheck::PoolBound { .. }),
            "a pool-bound run must be reported as bounded, got {check:?}"
        );
        assert!(
            !check.is_failure(),
            "a pool-bound run must not fail: the configuration bounded it, not the shape"
        );
        assert_eq!(
            achieved.shortfall,
            Shortfall::Expected,
            "and its shortfall must be the expected kind"
        );
        assert!(achieved.peak <= 3, "depth cannot exceed what the pool supplied");
    }

    /// A run whose measured depth disagrees with its declared shape is caught.
    ///
    /// The check must bite, not merely describe. Here the disagreement is
    /// produced by a run that issued a different number of operations from the
    /// one its scenario declared — the prediction is a function of that count,
    /// so the two no longer line up, and the verdict must say so.
    #[test]
    fn a_measured_depth_that_contradicts_the_declared_shape_is_a_mismatch() {
        let backend = FakeBackend::new();
        let mut runner = Runner::new(&backend, 8, Shape::Batched);
        let mut trace = Trace::new();
        futures::executor::block_on(runner.run(64, Phase::Read, &mut trace, |i| {
            Ok(std::future::ready((
                i as u64,
                Ok((0_u32, FakeBuf::default())),
            )))
        }))
        .expect("the fake backend never fails");

        // Judged against the count it actually ran, it agrees.
        let honest = runner.shape_check(64);
        assert!(
            matches!(honest, ShapeCheck::Matched { .. }),
            "64 operations at depth 8 batched should match, got {honest:?}"
        );

        // Judged against a different count, the prediction moves and the
        // disagreement must be reported as a failure.
        let mismatched = runner.shape_check(100);
        assert!(
            matches!(mismatched, ShapeCheck::Mismatched { .. }),
            "a measured mean that contradicts the prediction must be a mismatch, \
             got {mismatched:?}"
        );
        assert!(
            mismatched.is_failure(),
            "and a mismatch must be able to fail the run"
        );
        assert_eq!(mismatched.measured(), 4.5);
        assert_ne!(mismatched.predicted(), 4.5);
    }

    /// The rolling scenarios' shortfall verdicts are exactly what they were.
    ///
    /// FR-009: adding a second shape must not disturb the first. These are the
    /// six (operation count, depth) pairs the existing three scenarios produce
    /// under `Config::small()`, which is the configuration every `cargo test`
    /// run uses. A gate that collapsed the rolling window would change these.
    #[test]
    fn the_existing_rolling_combinations_keep_their_verdicts() {
        // Three scenarios (64, 64 and 16 operations) at the two small depths.
        for count in [64_usize, 64, 16] {
            for depth in [1_usize, 4] {
                let (achieved, check, _) = drive(Shape::Rolling, count, depth, usize::MAX);
                assert_eq!(
                    achieved.shortfall,
                    Shortfall::None,
                    "{count} rolling operations at depth {depth} reached their depth before \
                     this change and must still"
                );
                assert_eq!(achieved.peak, depth, "and must still peak at the depth");
                assert!(
                    matches!(check, ShapeCheck::Matched { .. }),
                    "{count}@{depth} should match its rolling prediction, got {check:?}"
                );
            }
        }
    }
}
