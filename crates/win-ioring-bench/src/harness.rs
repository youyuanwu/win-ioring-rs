//! The one function that prepares a backend, measures it, and consults the
//! comparator.
//!
//! Everything a benchmark and a test both need lives here, and both call
//! [`measure_combination`]. Only *how the iterations are timed* differs, and
//! that is injected through [`Timer`] — which is what keeps the verification on
//! the timed path rather than beside it. There is exactly one call site for
//! [`Ledger::observe`] in the crate, so removing it breaks a test rather than
//! silently removing the check.

use std::cell::RefCell;
use std::io;
use std::path::Path;

use win_ioring::runtime::SubmissionCounts;

use crate::concurrency::{Achieved, Depth, ShapeCheck};
use crate::config::Config;
use crate::fairness::{FairnessFailure, Ledger};
use crate::scenario::{Outcome, Scenario};
use crate::session::{self, Prepared};
use crate::verify::Trace;
use crate::weaken::Weakness;

/// What a single run is asked to do.
///
/// Bundled because the same six values thread through the scenario, the harness
/// and the binary, and passing them individually made every signature a wall.
#[derive(Clone)]
pub struct Job<'a> {
    /// Which scenario to run.
    pub scenario: Scenario,
    /// The file the read scenarios work over.
    pub read_path: &'a Path,
    /// The file the write scenario creates.
    pub write_path: &'a Path,
    /// The transfer size of each operation.
    pub block: u32,
    /// How many operations the scenario performs.
    pub operations: usize,
    /// How many may be outstanding at once.
    pub depth: Depth,
}

/// Which backend to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    /// The thread-pool backend with a single blocking thread.
    TokioOne,
    /// The thread-pool backend at its default width.
    TokioMany,
    /// This crate, with caller-owned buffers.
    RingPlain,
    /// This crate, with registered buffers and handles.
    RingRegistered,
}

impl Which {
    /// Every backend, in a fixed order.
    pub fn all() -> [Which; 4] {
        [
            Which::TokioOne,
            Which::TokioMany,
            Which::RingPlain,
            Which::RingRegistered,
        ]
    }

    /// A short, stable, filesystem-safe identifier.
    ///
    /// Not the display name: `tokio::fs (blocking pool 1)` contains a colon,
    /// which is not a legal Windows path character, and a benchmark identifier
    /// becomes a directory name under `target/criterion`. These are also the
    /// keys a stored baseline is matched on, so they must not drift. The full
    /// name and configuration appear in the fairness account, where a reader
    /// wants them.
    pub fn slug(self) -> &'static str {
        match self {
            Which::TokioOne => "tokio-pool-1",
            Which::TokioMany => "tokio-pool-512",
            Which::RingPlain => "ioring-owned",
            Which::RingRegistered => "ioring-registered",
        }
    }

    /// Whether this backend builds a driver.
    ///
    /// The thread-pool backends do not, which is why the driver count of SC-014
    /// is read against the ring combinations rather than against all of them.
    pub fn builds_a_driver(self) -> bool {
        matches!(self, Which::RingPlain | Which::RingRegistered)
    }
}

/// The backend order for the `index`th combination.
///
/// Rotated so no backend is systematically advantaged by always running first on
/// a freshly settled machine. Deterministic, so two runs visit the same order
/// and can be compared.
pub fn rotated_order(index: usize) -> [Which; 4] {
    let mut order = Which::all();
    let count = order.len();
    order.rotate_left(index % count);
    order
}

/// What a timer needs in order to name and scale one benchmark.
pub struct Timed {
    /// The scenario being measured.
    pub scenario: Scenario,
    /// The depth it is measured at.
    pub depth: Depth,
    /// Which backend is being measured.
    pub which: Which,
    /// How many I/Os one iteration issues, taken from the warm-up's trace
    /// rather than from arithmetic, so the denominator cannot drift from what
    /// was actually issued.
    pub io_count: usize,
}

/// How a caller times the iterations of one combination.
///
/// Called once per combination, with a closure that runs one iteration. The
/// closure yields nothing: its outcome is recorded into an [`Evidence`] the
/// caller owns, because a timing framework drops whatever the measured closure
/// returns inside the timed region.
pub trait Timer {
    /// Runs `one` as many times as this timer wants to, timing it however it
    /// times things.
    fn time<F, Fut>(&mut self, timed: &Timed, prepared: &Prepared, one: F)
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = ()>;
}

/// What the measured iterations produced.
///
/// The error slot exists because the iteration closure's `Output = ()` has
/// nowhere to return an `io::Error`. Without it, a failure inside a measured
/// iteration could only be a panic in the timed region, which a timing framework
/// reports as a crash with no reason attached.
#[derive(Default)]
pub struct Evidence {
    /// The first measured iteration's outcome, kept for the FR-019 check.
    first: Option<Outcome>,
    /// The last measured iteration's outcome — the one the ledger observes.
    last: Option<Outcome>,
    /// How many iterations ran.
    iterations: usize,
    /// The first error any measured iteration produced.
    failure: Option<io::Error>,
}

impl Evidence {
    /// Records what one iteration did.
    pub fn record(&mut self, outcome: Outcome) {
        self.iterations += 1;
        if self.first.is_none() {
            self.first = Some(Outcome {
                trace: outcome.trace.clone(),
                achieved: outcome.achieved,
                shape: outcome.shape,
                submitted: outcome.submitted,
            });
        }
        self.last = Some(outcome);
    }

    /// Records that one iteration failed.
    ///
    /// Only the first error is kept, and later iterations are left to run: the
    /// timer is mid-sample and stopping it cleanly is not on offer.
    pub fn record_failure(&mut self, error: io::Error) {
        self.iterations += 1;
        if self.failure.is_none() {
            self.failure = Some(error);
        }
    }
}

/// What became of one (backend, scenario, depth).
///
/// Three outcomes rather than two, because "the backend cannot run here" and
/// "the backend ran and failed" are different facts and a reader needs both.
#[derive(Debug)]
pub enum Record {
    /// The backend prepared, ran and was verified.
    Measured {
        /// The backend's name.
        name: String,
        /// Its configuration.
        configuration: String,
        /// What the verified iteration achieved, in concurrency terms.
        achieved: Achieved,
        /// What it issued and delivered.
        trace: Trace,
        /// What the ring submitted during the reported iteration, or `None` for
        /// a backend with no ring.
        ///
        /// A delta over that one iteration, not a session total: see
        /// [`Prepared::one`]. Entries are not exactly operations — a
        /// registration and a cancellation occupy entries too — so entries per
        /// submission is a proxy for operations per submission, close but not
        /// identical.
        submitted: Option<SubmissionCounts>,
        /// Whether the reported iteration achieved the depth its scenario's
        /// shape predicts.
        shape: ShapeCheck,
        /// How many measured iterations ran.
        iterations: usize,
        /// Whether the verified trace came out of a timed iteration.
        ///
        /// False only when the timer ran no iterations at all — a filtered run,
        /// or a timer that declined this benchmark — in which case the warm-up
        /// trace was verified instead and the combination is
        /// **verified-but-not-timed**.
        timed: bool,
    },
    /// The backend could not be built here.
    Unavailable {
        /// The backend's name.
        name: String,
        /// Why the host cannot provide it.
        reason: String,
    },
    /// The backend prepared, then an iteration returned an error.
    ///
    /// Never reported as a fast time: a backend that gave up early would
    /// otherwise look like the winner.
    Failed {
        /// The backend's name.
        name: String,
        /// Its configuration.
        configuration: String,
        /// What went wrong.
        error: io::Error,
    },
}

/// Prepares one backend, warms it, times it, and puts its trace in front of the
/// comparator.
///
/// The trace handed to [`Ledger::observe`] is **the one the timed closure
/// produced** — the last measured iteration's — not the warm-up's. That is the
/// whole point: verifying the warm-up would leave the timed region unverified
/// while still passing a mutation check, which is the failure the specification's
/// edge case names. The single exception is a timer that ran no iterations at
/// all, where the warm-up trace is verified instead and the [`Record`] says so.
///
/// `ledger` is **the caller's**, shared across one (scenario, depth)'s backends.
/// Constructing one here would make every backend its own reference and no
/// disagreement could ever be reported.
///
/// `weakness` is [`Weakness::None`] for every real measurement. A test passes
/// something else to watch a backend that really does less be rejected by this
/// function rather than by a copy of its comparison — which is the only way
/// "a weakened backend fails a run" can be settled about the code a measurement
/// actually runs.
pub fn measure_combination(
    which: Which,
    weakness: Weakness,
    config: &Config,
    job: &Job<'_>,
    ledger: &mut Ledger,
    timer: &mut impl Timer,
) -> Result<Record, FairnessFailure> {
    let prepared = match session::prepare(which, config, job) {
        Ok(prepared) => prepared,
        Err(unavailable) => {
            return Ok(Record::Unavailable {
                name: unavailable.name,
                reason: unavailable.reason,
            });
        }
    };
    let name = prepared.name();
    let configuration = prepared.configuration();

    // One untimed warm-up: it pays for lazily created threads, first-touch page
    // faults, and anything else a backend defers until first use. A combination
    // whose warm-up could not run has nothing to time.
    let warm = match prepared.block_on(prepared.one(job, weakness)) {
        Ok(outcome) => outcome,
        Err(error) => {
            let teardown = prepared.finish();
            return Ok(Record::Failed {
                name,
                configuration,
                error: teardown.err().unwrap_or(error),
            });
        }
    };

    let benchmark = Timed {
        scenario: job.scenario,
        depth: job.depth,
        which,
        io_count: warm.trace.operations(),
    };
    let evidence = RefCell::new(Evidence::default());
    {
        // Shared references bound outside the closure, so each call returns a
        // future that borrows *them* rather than the closure: a future borrowing
        // the closure it came from could not satisfy `FnMut() -> Fut` for a
        // single `Fut`.
        let prepared = &prepared;
        let evidence = &evidence;
        timer.time(&benchmark, prepared, move || async move {
            match prepared.one(job, weakness).await {
                Ok(outcome) => evidence.borrow_mut().record(outcome),
                Err(error) => evidence.borrow_mut().record_failure(error),
            }
        });
    }
    let evidence = evidence.into_inner();

    // Teardown happens before anything below can return, including the
    // comparator's rejection. A rejection that returned first would leave the
    // driver to be torn down by its own `Drop` in the middle of a caller's error
    // handling, which is the one path this design exists to keep off.
    let teardown = prepared.finish();

    if let Some(error) = evidence.failure {
        return Ok(Record::Failed {
            name,
            configuration,
            error,
        });
    }
    if let Err(error) = teardown {
        return Ok(Record::Failed {
            name,
            configuration,
            error,
        });
    }

    // Repeated iterations must not accumulate state that changes what later ones
    // measure. Trivially satisfied when fewer than two ran.
    if let (Some(first), Some(last)) = (&evidence.first, &evidence.last)
        && let Err(mismatch) = first.trace.agrees_with(&last.trace)
    {
        return Err(FairnessFailure {
            scenario: job.scenario,
            depth: job.depth,
            reference: format!("{name} (first iteration)"),
            backend: format!("{name} (last iteration)"),
            mismatch,
        });
    }

    let timed = evidence.iterations > 0;
    let outcome = evidence.last.unwrap_or(warm);
    ledger.observe(job.scenario, job.depth, &name, &outcome.trace)?;

    Ok(Record::Measured {
        name,
        configuration,
        achieved: outcome.achieved,
        trace: outcome.trace,
        submitted: outcome.submitted,
        shape: outcome.shape,
        iterations: evidence.iterations,
        timed,
    })
}

/// A timer that runs a fixed number of iterations and times nothing.
///
/// For tests, and for anything else that wants the path — preparation, warm-up,
/// verification, teardown — without the statistics. It exists so a test can
/// exercise [`measure_combination`] without paying for repeats whose timings
/// nobody will read.
pub struct Untimed {
    /// How many iterations to run.
    pub iterations: usize,
}

impl Timer for Untimed {
    fn time<F, Fut>(&mut self, _timed: &Timed, prepared: &Prepared, mut one: F)
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = ()>,
    {
        let iterations = self.iterations;
        // One `block_on` around the whole loop, as any timer must: the ring
        // backends' driver is pumped by that call, and restarting it per
        // iteration would abandon a park between every pair of them.
        prepared.block_on(async move {
            for _ in 0..iterations {
                one().await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_consecutive_rotations_are_four_distinct_orders() {
        let orders: Vec<[Which; 4]> = (0..4).map(rotated_order).collect();
        for (i, a) in orders.iter().enumerate() {
            for b in &orders[i + 1..] {
                assert_ne!(a, b, "two rotations produced the same order");
            }
        }
    }

    #[test]
    fn every_rotation_contains_every_backend_once() {
        for index in 0..8 {
            let order = rotated_order(index);
            for which in Which::all() {
                assert_eq!(
                    order.iter().filter(|w| **w == which).count(),
                    1,
                    "rotation {index} did not contain {which:?} exactly once"
                );
            }
        }
    }
}
