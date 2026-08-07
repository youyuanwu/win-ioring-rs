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

use crate::backends::ioring::HandleMode;
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
    /// Completion-based, but not a ring.
    ///
    /// The other four vary two things at once — synchronous work on threads
    /// versus completion delivery through a ring — so a difference between
    /// them cannot say which of the two it is about. This one holds the
    /// completion model and drops the ring.
    Compio,
    /// [`Which::RingPlain`], but opening **synchronous** handles.
    ///
    /// See [`Which::RingPlainSync`] and [`Which::RingRegisteredSync`] below for
    /// why these are deliberately absent from [`Which::all`].
    RingPlainSync,
    /// [`Which::RingRegistered`], but opening **synchronous** handles.
    RingRegisteredSync,
}

impl Which {
    /// Every backend in the **published matrix**, in a fixed order.
    ///
    /// The two `*Sync` variants are deliberately excluded. Two reasons, and the
    /// first is structural rather than editorial:
    ///
    /// 1. `combinations % backends == 0` is asserted in this crate's tests, and
    ///    the matrix has 10 combinations. Five backends divide it; six or seven
    ///    do not, and the assertion's own message explains that relaxing it
    ///    would silently give up the position balance that
    ///    [`rotated_order`] exists to provide. Adding these to `all()` would
    ///    therefore break a real fairness guarantee, not merely a count.
    /// 2. The matrix answers "how does this crate compare to the alternatives",
    ///    and a backend deliberately configured to be slower is not an
    ///    alternative anyone would choose. It belongs to an experiment about
    ///    *this crate's* behaviour, which is what the `handle-mode` arm is.
    ///
    /// That constraint was load-bearing rather than merely survived. Being
    /// unable to widen the matrix forced the A/B into its own target, where the
    /// budget affords A/Bing **both** ring backends — something the main-matrix
    /// design wanted and could not afford.
    pub fn all() -> [Which; 5] {
        [
            Which::TokioOne,
            Which::TokioMany,
            Which::RingPlain,
            Which::RingRegistered,
            Which::Compio,
        ]
    }

    /// The handle mode this backend opens files in.
    ///
    /// Every non-ring backend reports [`HandleMode::Overlapped`] because that is
    /// what it in fact produces: `compio` ORs the flag unconditionally in its
    /// `OpenOptions`, and the thread-pool backends are discussed separately —
    /// see the disclosure in `docs/performance.md`, because `tokio::fs` opens
    /// through `std` and therefore does *not* get an overlapped handle. This
    /// method describes the **ring** backends, which are the ones the
    /// `handle-mode` arm varies; it is not a claim about `tokio::fs`.
    pub fn handle_mode(self) -> HandleMode {
        match self {
            Which::RingPlainSync | Which::RingRegisteredSync => HandleMode::Synchronous,
            _ => HandleMode::Overlapped,
        }
    }

    /// The handle mode the kernel should report for a file this backend opens.
    ///
    /// Distinct from [`Which::handle_mode`], which is an *input* to the ring
    /// backends and says nothing about the others. This is the *output*: what
    /// `NtQueryInformationFile` should say about the handle that comes back.
    /// Two of these answers are claims this repository makes in prose, and
    /// having them here turns each into something a run checks:
    ///
    /// - `compio` is [`HandleMode::Overlapped`] because `compio-fs`'s
    ///   `OpenOptions` ORs `FILE_FLAG_OVERLAPPED` in unconditionally, and its
    ///   `custom_flags` can only set flags, never clear them. That is the
    ///   invariant this crate's overlapped default was modelled on.
    /// - `tokio::fs` is [`HandleMode::Synchronous`] because it opens through
    ///   `std`, which sets no such flag. This is the asymmetry
    ///   `docs/performance.md` discloses rather than corrects — correcting it
    ///   inside the handle-mode work would have confounded the variable the
    ///   experiment is built around.
    ///
    /// A wrong answer here fails the run rather than skewing it, which is the
    /// intent: the alternative is an A/B that quietly compares two identical
    /// arms and reports a clean null.
    pub fn expected_handle_mode(self) -> HandleMode {
        match self {
            Which::RingPlain | Which::RingRegistered | Which::Compio => HandleMode::Overlapped,
            Which::RingPlainSync | Which::RingRegisteredSync => HandleMode::Synchronous,
            // Every `tokio::fs` width, however wide the pool: the pool's width
            // changes how many threads block on the handle, not how it was
            // opened.
            _ => HandleMode::Synchronous,
        }
    }

    /// The matrix backend this one shares everything but handle mode with.
    ///
    /// The pairing the A/B is computed over. Returns `self` for backends that
    /// have no synchronous twin.
    pub fn overlapped_twin(self) -> Which {
        match self {
            Which::RingPlainSync => Which::RingPlain,
            Which::RingRegisteredSync => Which::RingRegistered,
            other => other,
        }
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
            Which::Compio => "compio-iocp",
            Which::RingPlainSync => "ioring-owned-sync",
            Which::RingRegisteredSync => "ioring-registered-sync",
        }
    }

    /// Whether this backend builds a driver.
    ///
    /// The thread-pool and compio backends do not, which is why the driver count
    /// of SC-014 is read against the ring combinations rather than against all of
    /// them.
    pub fn builds_a_driver(self) -> bool {
        matches!(
            self,
            Which::RingPlain
                | Which::RingRegistered
                | Which::RingPlainSync
                | Which::RingRegisteredSync
        )
    }
}

/// The backend order for the `index`th combination.
///
/// Rotated so no backend is systematically advantaged by always running first on
/// a freshly settled machine. Deterministic, so two runs visit the same order
/// and can be compared.
pub fn rotated_order(index: usize) -> [Which; 5] {
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

/// Where the opens sit relative to the region the timer measures.
///
/// The published matrix opens **inside** it ([`Opens::PerIteration`]): every
/// iteration opens the file it reads, and the per-open cost is part of what the
/// matrix reports. That is a deliberate choice recorded on
/// [`crate::backend::Backend::open_read`], and it is fair there because every
/// backend in a (scenario, depth) pays the same open.
///
/// The `handle-mode` arm cannot inherit it. That arm's single variable is
/// whether the handle carries `FILE_FLAG_OVERLAPPED`, and an open is one of the
/// places that flag can itself cost something. With opens inside the timed
/// region the measured delta would be *serialisation plus per-open flag cost*
/// with no way to separate them — and worse, the depth-1 cell is that arm's
/// negative control, where serialisation has nothing to serialise and the
/// expected reading is "no difference". A per-open difference would show up
/// there as an effect, and the arm's own drift check would have read it as
/// run-level drift rather than as a confound. The control would have been
/// silently disarmed while still appearing to pass, which is the failure mode
/// this repository keeps finding: a clean, plausible, wrong result.
///
/// So that arm hoists its opens ([`Opens::Hoisted`]). The cost of hoisting is
/// stated where its numbers appear, not only where its method is described: an
/// arm that excludes opens is **not** comparable cell-for-cell with a matrix
/// that includes them.
///
/// The next arm to be added faces this same choice, and the answer is not
/// "copy whichever neighbour you read first". Hoist when the variable under
/// test could change what an open costs, or when the configurations being
/// compared hold different numbers of handles — the unbuffered arm hoists for
/// that second reason. Keep opens inside when the arm is reporting the cost of
/// the whole operation as a user would pay it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opens {
    /// Each iteration opens the file it reads. What the published matrix does.
    PerIteration,
    /// The file is opened once, before the warm-up, and shared by every
    /// iteration.
    Hoisted,
}

/// Prepares one backend, warms it, times it, and puts its trace in front of the
/// comparator.
///
/// Opens per iteration; see [`measure_combination_with`] for the general form
/// and [`Opens`] for what that choice means.
pub fn measure_combination(
    which: Which,
    weakness: Weakness,
    config: &Config,
    job: &Job<'_>,
    ledger: &mut Ledger,
    timer: &mut impl Timer,
) -> Result<Record, FairnessFailure> {
    measure_combination_with(
        which,
        weakness,
        Opens::PerIteration,
        config,
        job,
        ledger,
        timer,
    )
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
///
/// `opens` decides whether the opens are timed. Both arms run through *this*
/// function rather than through a copy of it: putting an implementation
/// divergence inside a controlled experiment is how a difference between two
/// measurement paths gets published as a difference between two handle modes.
///
/// # Panics
///
/// Panics if [`Opens::Hoisted`] is combined with a weakening. Hoisting exists
/// for a single-variable experiment, and a weakened backend is a second
/// variable; the combination has no legitimate caller, so it is a defect rather
/// than a slow result.
pub fn measure_combination_with(
    which: Which,
    weakness: Weakness,
    opens: Opens,
    config: &Config,
    job: &Job<'_>,
    ledger: &mut Ledger,
    timer: &mut impl Timer,
) -> Result<Record, FairnessFailure> {
    assert!(
        !(opens == Opens::Hoisted && weakness != Weakness::None),
        "hoisted opens exist for a single-variable experiment; weakening it \
         adds a second variable"
    );
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

    // Hoisted: the one open, before the warm-up and therefore before anything
    // timed. A failure here is the backend's failure to open, reported the same
    // way a failed warm-up would be.
    let hoisted = match opens {
        Opens::PerIteration => None,
        Opens::Hoisted => match prepared.block_on(prepared.open_read(job.read_path)) {
            Ok(file) => Some(file),
            Err(error) => {
                let teardown = prepared.finish();
                return Ok(Record::Failed {
                    name,
                    configuration,
                    error: teardown.err().unwrap_or(error),
                });
            }
        },
    };

    // The read-back. Everything above this line is the *intent* to open in a
    // particular handle mode; this asks the kernel what it actually got.
    //
    // It exists because of what the failure would otherwise look like. If the
    // synchronous configurations silently opened overlapped handles — a missing
    // flag, a builder call dropped in a refactor, a `Which` arm widened to
    // include the wrong variant — every A/B cell would compare a handle against
    // an identical handle and the arm would report **no effect**, cleanly, at
    // every depth, with every fairness and shape check passing. `docs/testing.md`
    // records that this project under-scrutinises unflattering results, which
    // makes a spurious null the cheapest error available here. So the null is
    // made unavailable: a handle that is not what it claims aborts the run.
    //
    // A panic rather than a `Record::Failed` for the reason `Prepared::block_on`
    // gives about its own: this is a defect, not a slow or failed measurement,
    // and it must not be capable of being read as one.
    //
    // The matrix pays nothing for this. It runs `Opens::PerIteration`, where
    // `hoisted` is `None` and neither the query nor the open it would inspect
    // happens at all.
    if let Some(file) = &hoisted {
        verify_handle_mode(file.raw_handle(), which.expected_handle_mode(), &name);
    }

    // One untimed warm-up: it pays for lazily created threads, first-touch page
    // faults, and anything else a backend defers until first use. A combination
    // whose warm-up could not run has nothing to time.
    let warm = match prepared.block_on(async {
        match &hoisted {
            Some(file) => prepared.one_on(file, job).await,
            None => prepared.one(job, weakness).await,
        }
    }) {
        Ok(outcome) => outcome,
        Err(error) => {
            drop(hoisted);
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
        let hoisted = &hoisted;
        timer.time(&benchmark, prepared, move || async move {
            let result = match hoisted {
                Some(file) => prepared.one_on(file, job).await,
                None => prepared.one(job, weakness).await,
            };
            match result {
                Ok(outcome) => evidence.borrow_mut().record(outcome),
                Err(error) => evidence.borrow_mut().record_failure(error),
            }
        });
    }
    let evidence = evidence.into_inner();

    // The hoisted file is closed before teardown. `win_ioring::file::File` holds
    // an `OwnedHandle`, so its close is a plain `CloseHandle` rather than a ring
    // operation and this is not a correctness requirement — but the ordering is
    // still what a reader should see, and it is what unwinding gives too, since
    // `prepared` is declared before `hoisted` and drops after it.
    drop(hoisted);

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

thread_local! {
/// How many handle-mode read-backs the calling thread has performed.
///
/// An observation seam rather than a test fixture, in the shape of/// [`crate::backends::ioring::drivers_built`] and for the same reason: it is not
/// `#[cfg(test)]` because the property it settles is about a **real** run.
///
/// The property is that the read-back is *reached*, and its absence is the
/// gap that makes the read-back's own two `#[should_panic]` twins insufficient.
/// Those prove the function can fail when called. Nothing proved it was called:
/// deleting the single line that calls it left every library test, every
/// integration test and the bench target's smoke run green, which is exactly the
/// "gate that never ran" species `docs/testing.md` names.
///
/// It settles the negative half too. The published matrix must pay **nothing**
/// for this arm's safeguard, and "the matrix runs `Opens::PerIteration`, where
/// `hoisted` is `None`" is a structural argument. A caller can now observe that
/// a per-iteration combination performs zero read-backs, which is a measurement.
///
/// # Why this is thread-local where `DRIVERS_BUILT` is process-global
///
/// The difference is not cosmetic and was not free: the process-global version
/// was written first, and both of its tests failed. The test harness runs tests
/// in parallel threads in one process, so between a `before` reading and the
/// assertion, an unrelated test's read-back moves a global counter. The
/// positive half degrades to a flake; the **negative** half — "this path
/// performs zero read-backs" — degrades to something worse, an assertion that
/// fails for a reason unrelated to what it claims to check, which would
/// eventually be "fixed" by relaxing it to a lower bound and thereby deleted.
///
/// `DRIVERS_BUILT` is global because a driver is built on whichever worker
/// thread needs it, so a per-thread count there would answer a question nobody
/// asked. The read-back is different: it runs inline on the thread that calls
/// [`measure_combination_with`], always. Thread-local is therefore not a
/// workaround for the parallel harness but the accurate scope, and it is
/// strictly stronger — it lets the negative half assert an exact zero instead of
/// an inequality.
static HANDLE_MODE_CHECKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many handle-mode read-backs have run **on the calling thread**.
///
/// Monotonic within a thread, so a caller compares two readings rather than
/// reading an absolute. The scope is the thread rather than the process because
/// the read-back runs inline on the thread that calls
/// [`measure_combination_with`]; a process-global counter was tried first and
/// made both of its own tests unsound under the parallel test harness.
#[must_use]
pub fn handle_mode_checks() -> usize {
    HANDLE_MODE_CHECKS.with(std::cell::Cell::get)
}

/// Asks the kernel what handle mode a live handle actually has, and aborts if
/// it is not `expected`.
///
/// See the call site for why this is a panic. The query itself is the same
/// `NtQueryInformationFile(FileModeInformation)` the unbuffered arm uses; a
/// query that cannot answer is treated as a failure rather than as agreement,
/// because "could not tell" is not evidence that the experiment is sound.
///
/// Takes a raw handle rather than the [`session::PreparedFile`] the caller
/// holds, so that a test can hand it handles it opened itself. A check that
/// could only be reached through a fully prepared backend would be a check
/// nothing could demonstrate failing.
///
/// # It is not redundant with the unit tests
///
/// This was checked rather than assumed. Replacing
/// `.with_handle_mode(which.handle_mode())` in [`session::prepare`] with a
/// hardcoded `HandleMode::Overlapped` — the shape a careless refactor of that
/// wiring would take — leaves **all 122 library unit tests passing**, including
/// the ones that assert the two modes really produce different handles and that
/// each `Which` maps to the right mode. Every one of those tests is about a
/// piece in isolation; none of them covers the wiring that connects the pieces,
/// and the result is an A/B whose two arms are the same handle.
///
/// Under that mutation this check fails, in the integration test and in the
/// bench target's own CI smoke run. It is the only thing that does.
fn verify_handle_mode(raw: std::os::windows::io::RawHandle, expected: HandleMode, name: &str) {
    use std::os::windows::io::FromRawHandle;

    // Counted before the query, so a handle whose mode cannot be read still
    // registers as an attempt. A counter incremented only on success would go
    // quiet in precisely the case the caller most needs to notice.
    HANDLE_MODE_CHECKS.with(|c| c.set(c.get() + 1));

    // SAFETY: the caller guarantees `raw` is a live file handle it owns. The
    // wrapper is in a `ManuallyDrop` and never dropped, so the handle is not
    // closed here and ownership stays with the caller.
    let view = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_handle(raw) });
    classify_handle_mode(crate::unbuffered_workload::file_mode(&view), expected, name);
}

/// The judgement half of [`verify_handle_mode`], split out from the query half.
///
/// # Why this is a separate function
///
/// So that the "could not determine" branch has a twin that can reach it. That
/// branch is the one that must not be quietly replaced by something like
/// `.unwrap_or(0)`, because an unreadable mode would then classify as
/// `Overlapped` — agreement with whichever arm the experiment most wants to be
/// true, arriving without a measurement.
///
/// Reaching it through a real handle turned out to be impossible, which was
/// established by probing rather than assumed. `NtQueryInformationFile` with
/// `FileModeInformation` **answers** for every handle kind that could be
/// produced:
///
/// | handle | result |
/// |---|---|
/// | file opened synchronously | `Ok(0x20)` — `FILE_SYNCHRONOUS_IO_NONALERT` |
/// | file opened overlapped | `Ok(0)` |
/// | anonymous pipe (`std::io::pipe`) | `Ok(0x20)` |
/// | TCP socket | `Ok(0)` |
/// | a value that is not a handle | `Err` — `STATUS_INVALID_HANDLE` |
///
/// Only the last fails, and constructing it means handing
/// `File::from_raw_handle` something that violates its documented contract, in
/// order to test a function whose own contract already requires a live handle.
/// A test that breaks two contracts to reach a branch is not evidence about the
/// branch. Splitting the function is the cheaper honesty: the query half is
/// covered end-to-end by the three handle-driven twins, and this half is covered
/// directly, including the case no handle produces.
///
/// The probe also recorded something worth keeping: a **socket** answers `Ok(0)`
/// and would therefore classify as `Overlapped`. Nothing in this arm hands it a
/// socket, but "mode 0 means overlapped" is a claim about file objects only, and
/// the table above is the record that it was checked rather than assumed.
fn classify_handle_mode(mode: std::io::Result<u32>, expected: HandleMode, name: &str) {
    let mode = mode.unwrap_or_else(|error| {
        panic!(
            "HANDLE MODE UNVERIFIABLE for {name}: could not read the handle's \
             mode ({error}). This arm's only variable is the handle mode, so a \
             mode that cannot be confirmed leaves the measurement unable to \
             mean anything; it is not evidence that the mode is correct."
        )
    });

    // There are two synchronous modes, and the classifier this shares with the
    // unbuffered arm tests only `FILE_SYNCHRONOUS_IO_NONALERT`. A handle with
    // `FILE_SYNCHRONOUS_IO_ALERT` would therefore be classified `Overlapped` —
    // the direction that manufactures a null. It is not reachable through
    // `CreateFileW`, which is why the classifier is left as it is rather than
    // widened underneath the unbuffered arm's published figures, but "not
    // reachable" is checked here instead of assumed.
    const FILE_SYNCHRONOUS_IO_ALERT: u32 = 0x0000_0010;
    assert!(
        mode & FILE_SYNCHRONOUS_IO_ALERT == 0,
        "HANDLE MODE UNCLASSIFIABLE for {name}: the handle carries \
         FILE_SYNCHRONOUS_IO_ALERT, which is synchronous but is not the bit \
         this classifier tests, so it would be reported as Overlapped. Mode: \
         {mode:#010x}"
    );

    let actual = if mode & crate::unbuffered_workload::FILE_SYNCHRONOUS_IO_NONALERT != 0 {
        HandleMode::Synchronous
    } else {
        HandleMode::Overlapped
    };
    assert!(
        actual == expected,
        "HANDLE MODE MISMATCH for {name}: expected {expected:?}, the kernel \
         reports {actual:?}. If this is one of the ring configurations, every \
         A/B cell would compare a handle against an identical handle and report \
         no effect — a clean null that is an artefact of this defect rather than \
         a measurement."
    );
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
    fn five_consecutive_rotations_are_five_distinct_orders() {
        let orders: Vec<[Which; 5]> = (0..5).map(rotated_order).collect();
        for (i, a) in orders.iter().enumerate() {
            for b in &orders[i + 1..] {
                assert_ne!(a, b, "two rotations produced the same order");
            }
        }
    }

    #[test]
    fn every_rotation_contains_every_backend_once() {
        // Ten, not eight. The bound is a multiple of the backend count so every
        // rotation offset is covered the same number of times. Eight did cover
        // all five offsets, but unevenly — and an arity-blind bound stops
        // covering all of them the moment the backend count exceeds it, which is
        // why this moved with the arity rather than being left alone.
        for index in 0..10 {
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

    /// Over the whole matrix, every backend occupies every position equally
    /// often — and the rotation turns the way it is supposed to.
    ///
    /// The balance clause and the direction clause are separate assertions
    /// because balance alone does not pin direction: `rotate_right` produces an
    /// exactly-as-balanced schedule as `rotate_left`, so a flipped rotation
    /// would satisfy every counting property here while visiting the
    /// combinations in a different order. Direction has to be asserted in its
    /// own right or the flip goes unnoticed.
    #[test]
    fn the_matrix_gives_every_backend_every_position_equally() {
        let config = crate::config::Config::default();
        let combinations: usize = crate::scenario::Scenario::all()
            .iter()
            .map(|scenario| config.depths_for(*scenario).len())
            .sum();
        let backends = Which::all().len();
        assert!(
            combinations > 0,
            "with no combinations both loops below iterate an empty range and \
             this test passes without checking anything"
        );
        assert_eq!(
            combinations % backends,
            0,
            "the schedule only balances when the combination count is a whole \
             number of cycles; at {combinations} combinations and {backends} \
             backends it is not. This is a real loss of the fairness property, \
             not a mis-specified test: at four backends the schedule genuinely \
             was unbalanced. If this fires, the matrix or the backend count has \
             to change — relaxing the check would silently give up position \
             balance rather than report that it was given up"
        );
        let expected = combinations / backends;

        // Balance: each backend runs in each position exactly `expected` times.
        for position in 0..backends {
            for which in Which::all() {
                let count = (0..combinations)
                    .filter(|index| rotated_order(*index)[position] == which)
                    .count();
                assert_eq!(
                    count, expected,
                    "{which:?} occupied position {position} {count} times over \
                     {combinations} combinations, not {expected}"
                );
            }
        }

        // Direction: rotation `n` starts with the backend `n` places along
        // `all()`, which is what `rotate_left` means. Under `rotate_right` this
        // fails while every count above still passes.
        let all = Which::all();
        for index in 0..combinations {
            assert_eq!(
                rotated_order(index)[0],
                all[index % backends],
                "rotation {index} did not begin with the {}th backend: the \
                 rotation is turning the wrong way",
                index % backends
            );
        }
    }
    /// Every slug is unique across **every** variant, not just `all()`.
    ///
    /// Slugs become directory names under `target/criterion` and are the keys a
    /// stored baseline is matched on. Iterating `Which::all()` here would be a
    /// gate that cannot fail for the variants this test most needs to cover:
    /// the two `*Sync` variants are deliberately absent from `all()`, so an
    /// `all()`-driven loop would never see them and a sync slug colliding with
    /// a published one would overwrite a published baseline unnoticed.
    #[test]
    fn every_slug_is_unique_including_the_variants_outside_the_matrix() {
        let every = every_variant();
        assert!(
            every.len() > Which::all().len(),
            "this test is iterating only the matrix backends, so it cannot see \
             the variants it exists to check"
        );
        for (i, a) in every.iter().enumerate() {
            for b in &every[i + 1..] {
                assert_ne!(
                    a.slug(),
                    b.slug(),
                    "{a:?} and {b:?} share the slug {:?}, so one would \
                     overwrite the other's stored baseline",
                    a.slug()
                );
            }
        }
    }

    /// The matrix must not contain the experimental variants.
    ///
    /// If one leaked into `all()`, the combination count would stop being a
    /// whole number of cycles and the balance assertion would fire — but it
    /// would fire reporting an arithmetic problem rather than the actual
    /// mistake, so this says the actual thing.
    #[test]
    fn the_published_matrix_excludes_the_synchronous_variants() {
        for which in Which::all() {
            assert_eq!(
                which.handle_mode(),
                HandleMode::Overlapped,
                "{which:?} is in the published matrix but opens synchronous \
                 handles; the matrix compares this crate against alternatives, \
                 and a deliberately-slowed configuration is not one"
            );
        }
        assert!(
            !Which::all().contains(&Which::RingPlainSync)
                && !Which::all().contains(&Which::RingRegisteredSync),
            "a *Sync variant reached the published matrix"
        );
    }

    /// Handle mode is assigned per variant, in both directions.
    ///
    /// The negative half matters: a `handle_mode` hardcoded to `Overlapped`
    /// satisfies every assertion about the matrix backends, and would silently
    /// turn the A/B into a comparison of two identical arms.
    #[test]
    fn handle_mode_is_assigned_in_both_directions() {
        assert_eq!(Which::RingPlain.handle_mode(), HandleMode::Overlapped);
        assert_eq!(Which::RingRegistered.handle_mode(), HandleMode::Overlapped);
        assert_eq!(Which::RingPlainSync.handle_mode(), HandleMode::Synchronous);
        assert_eq!(
            Which::RingRegisteredSync.handle_mode(),
            HandleMode::Synchronous
        );
    }

    /// Each synchronous variant pairs with the twin it differs from in exactly
    /// one respect, and the pairing is not the identity.
    #[test]
    fn each_synchronous_variant_pairs_with_its_overlapped_twin() {
        assert_eq!(Which::RingPlainSync.overlapped_twin(), Which::RingPlain);
        assert_eq!(
            Which::RingRegisteredSync.overlapped_twin(),
            Which::RingRegistered
        );
        for which in [Which::RingPlainSync, Which::RingRegisteredSync] {
            let twin = which.overlapped_twin();
            assert_ne!(
                twin, which,
                "{which:?} is its own twin, so the A/B would compare a \
                 measurement against itself and report no difference"
            );
            assert_eq!(
                twin.handle_mode(),
                HandleMode::Overlapped,
                "{which:?}'s twin is not overlapped, so the pair does not \
                 straddle the variable under test"
            );
            assert_eq!(
                twin.builds_a_driver(),
                which.builds_a_driver(),
                "{which:?} and its twin disagree about building a driver, so \
                 they differ in more than handle mode"
            );
        }
    }

    /// Both experimental variants build a driver.
    ///
    /// `builds_a_driver` gates the driver-count fairness check. A `*Sync`
    /// variant omitted from it would build a driver that the check never
    /// counted, which is a fairness hole rather than a cosmetic one.
    #[test]
    fn the_synchronous_variants_are_counted_as_driver_builders() {
        assert!(Which::RingPlainSync.builds_a_driver());
        assert!(Which::RingRegisteredSync.builds_a_driver());
        assert!(!Which::TokioOne.builds_a_driver());
    }

    /// Every variant this enum has, including those outside the matrix.
    ///
    /// Written out rather than derived, so adding a variant without deciding
    /// whether it belongs here is a compile error in `every_variant_is_listed`
    /// below rather than a silent gap in the uniqueness check.
    fn every_variant() -> Vec<Which> {
        vec![
            Which::TokioOne,
            Which::TokioMany,
            Which::RingPlain,
            Which::RingRegistered,
            Which::Compio,
            Which::RingPlainSync,
            Which::RingRegisteredSync,
        ]
    }

    /// `every_variant` really is every variant.
    ///
    /// The exhaustive `match` is the assertion: adding a variant to `Which`
    /// fails to compile here until it is listed above, which is what keeps the
    /// uniqueness test from quietly stopping short of the new one.
    #[test]
    fn every_variant_is_listed() {
        for which in every_variant() {
            match which {
                Which::TokioOne
                | Which::TokioMany
                | Which::RingPlain
                | Which::RingRegistered
                | Which::Compio
                | Which::RingPlainSync
                | Which::RingRegisteredSync => {}
            }
        }
        assert_eq!(
            every_variant().len(),
            7,
            "every_variant has changed size; update this count deliberately, \
             and check the uniqueness test still covers what it should"
        );
    }

    /// Opens a file in one handle mode and returns it plus its path.
    ///
    /// `std::fs` deliberately, not this crate's `File`: the point is to produce
    /// a handle whose mode is known independently of anything under test.
    fn open_in(mode: HandleMode, name: &str) -> (std::fs::File, std::path::PathBuf) {
        use std::os::windows::fs::OpenOptionsExt;

        let path = std::env::temp_dir().join(format!("win-ioring-handle-mode-{name}.tmp"));
        std::fs::write(&path, b"handle mode probe").expect("could not write the probe file");
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        if mode == HandleMode::Overlapped {
            // 0x4000_0000 is FILE_FLAG_OVERLAPPED. Spelled numerically here to
            // keep this test independent of the constant the crate under test
            // derives, so a wrong constant there cannot make this test agree
            // with it.
            options.custom_flags(0x4000_0000);
        }
        let file = options.open(&path).expect("could not open the probe file");
        (file, path)
    }

    /// The read-back agrees with the truth, in both directions.
    ///
    /// Both directions matter. A check that only ever confirmed `Overlapped`
    /// would pass against an implementation that always answered `Overlapped`,
    /// which is exactly the defect it exists to catch.
    #[test]
    fn the_read_back_agrees_with_a_handle_of_each_mode() {
        use std::os::windows::io::AsRawHandle;

        let (overlapped, a) = open_in(HandleMode::Overlapped, "agrees-overlapped");
        verify_handle_mode(overlapped.as_raw_handle(), HandleMode::Overlapped, "probe");
        let (synchronous, b) = open_in(HandleMode::Synchronous, "agrees-synchronous");
        verify_handle_mode(
            synchronous.as_raw_handle(),
            HandleMode::Synchronous,
            "probe",
        );

        drop((overlapped, synchronous));
        let _ = std::fs::remove_file(a);
        let _ = std::fs::remove_file(b);
    }

    /// The read-back can fail: an overlapped handle claimed synchronous aborts.
    ///
    /// The twin `docs/testing.md` asks for. Without it, the agreement test above
    /// would still pass against a `verify_handle_mode` whose assertion had been
    /// deleted — a gate that cannot fail, which is the failure this repository
    /// has now found in enough places to name.
    #[test]
    #[should_panic(expected = "HANDLE MODE MISMATCH")]
    fn the_read_back_rejects_an_overlapped_handle_claimed_synchronous() {
        use std::os::windows::io::AsRawHandle;

        let (file, _path) = open_in(HandleMode::Overlapped, "rejects-overlapped");
        verify_handle_mode(file.as_raw_handle(), HandleMode::Synchronous, "probe");
    }

    /// And in the other direction, for the same reason.
    #[test]
    #[should_panic(expected = "HANDLE MODE MISMATCH")]
    fn the_read_back_rejects_a_synchronous_handle_claimed_overlapped() {
        use std::os::windows::io::AsRawHandle;

        let (file, _path) = open_in(HandleMode::Synchronous, "rejects-synchronous");
        verify_handle_mode(file.as_raw_handle(), HandleMode::Overlapped, "probe");
    }

    /// A handle whose mode cannot be read is a failure, not an agreement.
    ///
    /// The twin for the branch that distinguishes "could not determine" from
    /// "not synchronous". Without it, that branch could be replaced by
    /// `.unwrap_or(0)` — which reads as harmless and would report every
    /// unreadable handle as overlapped, i.e. as agreeing with whichever arm the
    /// experiment most wants to be true.
    ///
    /// It drives [`classify_handle_mode`] rather than [`verify_handle_mode`],
    /// because no real handle reaches this branch. That was established by
    /// probing — a pipe answers `0x20`, a socket answers `0`, and only a value
    /// that is not a handle fails. See [`classify_handle_mode`] for the table
    /// and for why reaching it through a fabricated handle would be worse
    /// evidence than reaching it directly.
    #[test]
    #[should_panic(expected = "HANDLE MODE UNVERIFIABLE")]
    fn the_read_back_refuses_a_handle_it_cannot_classify() {
        classify_handle_mode(
            Err(std::io::Error::other("probe: query failed")),
            HandleMode::Overlapped,
            "probe",
        );
    }

    /// The split between query and judgement did not cost the end-to-end path.
    ///
    /// [`classify_handle_mode`] exists so one branch can be reached directly,
    /// and the hazard of splitting a checker is that the half left in place
    /// stops being exercised through a real handle. This pins that the three
    /// handle-driven twins still run against live handles by checking the one
    /// property the split could have broken: that `verify_handle_mode` passes
    /// the query's *result* through rather than a default.
    #[test]
    fn the_read_back_still_reads_a_real_handle() {
        use std::os::windows::io::AsRawHandle;

        for mode in [HandleMode::Overlapped, HandleMode::Synchronous] {
            let (file, path) = open_in(mode, "split");
            let queried = crate::unbuffered_workload::file_mode(&file)
                .expect("a real file handle answers the mode query");
            // The same input the read-back is given, judged the same way.
            classify_handle_mode(Ok(queried), mode, "probe");
            verify_handle_mode(file.as_raw_handle(), mode, "probe");
            drop(file);
            let _ = std::fs::remove_file(path);
        }
    }

    /// The read-back counter moves when the read-back runs, and only then.
    ///
    /// A counter that never moved would make every assertion built on it
    /// vacuous, so both halves are checked here before anything else relies on
    /// it.
    #[test]
    fn the_read_back_counter_counts_read_backs() {
        use std::os::windows::io::AsRawHandle;

        let before = handle_mode_checks();
        let (file, path) = open_in(HandleMode::Overlapped, "counter");
        assert_eq!(
            handle_mode_checks(),
            before,
            "opening a file moved the read-back counter"
        );
        verify_handle_mode(file.as_raw_handle(), HandleMode::Overlapped, "probe");
        assert_eq!(
            handle_mode_checks(),
            before + 1,
            "the read-back did not move its own counter"
        );

        drop(file);
        let _ = std::fs::remove_file(path);
    }

    /// What the kernel should report is assigned per variant, in both
    /// directions, and is **not** the same function as [`Which::handle_mode`].
    ///
    /// The `TokioOne` row is the one that would be got wrong by conflating them:
    /// `handle_mode` reports `Overlapped` for it because that method describes
    /// what the ring backends are told to do and says nothing about `tokio::fs`,
    /// while the handle `tokio::fs` actually produces is synchronous. A
    /// read-back driven by `handle_mode` would fail every `tokio::fs` cell of
    /// the arm for a reason that is not a defect.
    #[test]
    fn expected_handle_mode_is_about_the_handle_not_about_the_request() {
        assert_eq!(
            Which::RingPlain.expected_handle_mode(),
            HandleMode::Overlapped
        );
        assert_eq!(
            Which::RingRegistered.expected_handle_mode(),
            HandleMode::Overlapped
        );
        assert_eq!(Which::Compio.expected_handle_mode(), HandleMode::Overlapped);
        assert_eq!(
            Which::RingPlainSync.expected_handle_mode(),
            HandleMode::Synchronous
        );
        assert_eq!(
            Which::RingRegisteredSync.expected_handle_mode(),
            HandleMode::Synchronous
        );

        // The disclosed asymmetry, pinned. `tokio::fs` opens through `std` and
        // gets a synchronous handle at every pool width.
        assert_eq!(
            Which::TokioOne.expected_handle_mode(),
            HandleMode::Synchronous
        );
        assert_eq!(
            Which::TokioMany.expected_handle_mode(),
            HandleMode::Synchronous
        );

        // And the two methods really do disagree, so neither can be quietly
        // replaced by the other.
        assert_ne!(
            Which::TokioOne.handle_mode(),
            Which::TokioOne.expected_handle_mode(),
            "handle_mode and expected_handle_mode agree about tokio::fs; one of \
             them has been changed to the other and the distinction they exist \
             to draw has been lost"
        );
    }
}
