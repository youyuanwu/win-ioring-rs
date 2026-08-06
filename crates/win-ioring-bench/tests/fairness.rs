//! Weakened backends, driven through the function a real measurement calls.
//!
//! The comparator's own unit tests establish that [`Trace::agrees_with`] rejects
//! a trace that disagrees. They say nothing about whether a *measurement*
//! consults it — a measurement path that stopped verifying entirely would leave
//! every one of them passing. These tests close that: each one builds a real
//! backend, runs a real scenario against it through
//! [`measure_combination`], and weakens the backend so that the run the
//! benchmark would report has to be rejected instead.
//!
//! The weakening is applied by `Prepared::one`, which is the same call the
//! benchmark's timed closure makes, so there is no second path a test could be
//! passing on.

use win_ioring_bench::backend::Availability;
use win_ioring_bench::backends::{compio, ioring};
use win_ioring_bench::concurrency::Depth;
use win_ioring_bench::config::Config;
use win_ioring_bench::fairness::{FairnessFailure, Ledger};
use win_ioring_bench::harness::{Job, Record, Untimed, Which, measure_combination};
use win_ioring_bench::scenario::Scenario;
use win_ioring_bench::verify::{Mismatch, Trace};
use win_ioring_bench::weaken::{self, Weakness};
use win_ioring_bench::workload;

/// The depth these run at.
///
/// Above one deliberately: at depth one a scenario is a sequence of single
/// operations, and the runner's bounded-concurrency path — where completions
/// arrive in an order nobody chose, which is the reason the digest is
/// commutative in the first place — is never exercised.
const DEPTH: Depth = 4;

/// How many measured iterations each combination runs.
///
/// Two rather than one, so the first-versus-last trace check has something to
/// compare and a weakening that varied between iterations would be caught here
/// rather than mistaken for the mismatch under test.
const ITERATIONS: usize = 2;

/// Prepares a small working set, in a directory of this test's own.
fn prepare(tag: &str, config: &Config) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = workload::data_dir().join(format!("fairness-{tag}"));
    std::fs::create_dir_all(&dir).unwrap();
    let read_path = dir.join("read.dat");
    let write_path = dir.join("write.dat");
    workload::ensure_file(&read_path, config.read_file_bytes).unwrap();
    workload::warm(&read_path).unwrap();
    (read_path, write_path)
}

/// The backends this host can actually run.
///
/// The two thread-pool backends are always available and are ordered first, so a
/// host without a ring still runs every test below against real backends rather
/// than skipping them — a weakened-backend test that quietly did nothing would be
/// worse than no test at all.
///
/// **Only [`an_honest_matrix_agrees`] walks the whole list.** Five of the
/// weakening tests take fixed positions from it, so on every host — ring or not
/// — those five weaken `tokio-pool-1` or `tokio-pool-512` and never a ring
/// backend. The weakening is applied inside `harness::measure_combination`,
/// which is backend-agnostic, so this establishes the rejection rather than any
/// one backend's honesty; that the ring backends are honest is what the control
/// case above establishes. The two compio tests at the end of this file are the
/// exception: they select their backend by identity, so they do weaken compio
/// itself. The ring backends remain unweakened, and the gap is recorded in
/// `docs/pending-work.md`.
fn available() -> Vec<Which> {
    let mut backends = vec![Which::TokioOne, Which::TokioMany];
    if matches!(ioring::availability(), Availability::Available) {
        backends.push(Which::RingPlain);
        backends.push(Which::RingRegistered);
    }
    // Appended last, and the position is load-bearing. The weakening tests below
    // take fixed indices from this list — `backends[0]`, `backends[1]`,
    // `available()[0]` — so inserting compio anywhere but the end would silently
    // re-target them at a different backend and change what they establish
    // without changing a line of their own source.
    if matches!(compio::availability(), Availability::Available) {
        backends.push(Which::Compio);
    }
    backends
}

/// The compio backend, taken from [`available`] by identity.
///
/// By identity and never by index, for two reasons. Indices are what pins the
/// tests above to the thread-pool backends, and compio's index is
/// host-dependent — it sits at 2 on a host without an I/O ring and at 4 on one
/// with, since `available()` only pushes the ring backends when the host has a
/// ring. And sourcing it from `available()` rather than naming
/// `Which::Compio` directly is what makes "remove compio from `available()`"
/// a mutation the compio tests can fail: a test that named the variant itself
/// would be decoupled from the list and would pass with compio absent from it,
/// which is the reach this phase exists to establish.
///
/// This *panics* on a host without compio rather than skipping, which is the
/// opposite of what `tests/comparison.rs` does for the same condition. Both are
/// deliberate and the difference is the two files' standing policies, not an
/// oversight: this file fails rather than skips (see the module doc above),
/// because a fairness property that silently stops being checked is the failure
/// it exists to prevent, while the comparison suite skips whatever the host
/// cannot build so that a ring-less host still gets a green run.
fn compio_backend() -> Which {
    available()
        .into_iter()
        .find(|which| matches!(which, Which::Compio))
        .expect("compio is available on this host")
}

/// Runs one combination through the measured path.
fn run(
    which: Which,
    weakness: Weakness,
    config: &Config,
    job: &Job<'_>,
    ledger: &mut Ledger,
) -> Result<Record, FairnessFailure> {
    let mut timer = Untimed {
        iterations: ITERATIONS,
    };
    measure_combination(which, weakness, config, job, ledger, &mut timer)
}

fn job<'a>(
    scenario: Scenario,
    read_path: &'a std::path::Path,
    write_path: &'a std::path::Path,
    config: &Config,
) -> Job<'a> {
    let (block, operations) = config.work(scenario);
    Job {
        scenario,
        read_path,
        write_path,
        block,
        operations,
        depth: DEPTH,
    }
}

/// The control case: nothing weakened, so the ledger must agree throughout.
///
/// Without it the seven tests below would be satisfied by a comparator that
/// rejected everything.
#[test]
fn an_honest_matrix_agrees() {
    let config = Config::small();
    let (read_path, write_path) = prepare("honest", &config);

    for scenario in Scenario::all() {
        let job = job(scenario, &read_path, &write_path, &config);
        // One ledger for this (scenario, depth), shared across its backends.
        let mut ledger = Ledger::new();
        let mut delivered = 0;

        for which in available() {
            let record = run(which, Weakness::None, &config, &job, &mut ledger)
                .unwrap_or_else(|f| panic!("an honest backend was rejected: {f}"));
            match record {
                Record::Measured { trace, .. } => {
                    delivered = delivered.max(trace.delivered_total())
                }
                other => panic!("{scenario:?} did not measure: {other:?}"),
            }
        }

        assert!(
            delivered > 0,
            "{} delivered no bytes at all, so agreement is vacuous",
            scenario.name()
        );
    }
}

/// SC-003: a backend weakened to do less work fails a run, through the same
/// function a real measurement calls.
///
/// The weakening skips one read in four, so fewer bytes reach
/// application-readable memory than the honest backend put there. The mismatch
/// is asserted by variant: a test that accepted any failure would pass just as
/// happily if the weakened run had fallen over for an unrelated reason.
#[test]
fn a_backend_that_skips_work_fails_a_run() {
    let config = Config::small();
    let (read_path, write_path) = prepare("skips", &config);

    for scenario in Scenario::all() {
        let job = job(scenario, &read_path, &write_path, &config);
        let backends = available();
        let mut ledger = Ledger::new();

        run(backends[0], Weakness::None, &config, &job, &mut ledger)
            .unwrap_or_else(|f| panic!("the reference backend was rejected: {f}"));

        let failure = run(backends[1], Weakness::SkipsWork, &config, &job, &mut ledger)
            .expect_err("a backend that skipped a quarter of its reads must be rejected");
        assert!(
            matches!(failure.mismatch, Mismatch::DeliveredBytes { .. }),
            "{} was rejected for the wrong reason: {failure}",
            scenario.name()
        );
    }
}

/// SC-004: a backend that reports full transfers with nothing readable behind
/// them fails a run.
///
/// The subtler hazard, and the one that motivated the digest: the counts all
/// agree and only the bytes differ, so nothing short of comparing what reached
/// application-visible memory would catch it.
#[test]
fn a_backend_that_delivers_nothing_readable_fails_a_run() {
    let config = Config::small();
    let (read_path, write_path) = prepare("hollow", &config);

    for scenario in Scenario::all() {
        let job = job(scenario, &read_path, &write_path, &config);
        let backends = available();
        let mut ledger = Ledger::new();

        run(backends[0], Weakness::None, &config, &job, &mut ledger)
            .unwrap_or_else(|f| panic!("the reference backend was rejected: {f}"));

        let failure = run(
            backends[1],
            Weakness::HollowDelivery,
            &config,
            &job,
            &mut ledger,
        )
        .expect_err("a backend delivering nothing from the file must be rejected");
        assert!(
            matches!(failure.mismatch, Mismatch::Delivered { .. }),
            "{} was rejected for the wrong reason: {failure}",
            scenario.name()
        );
    }
}

/// The guard against a vacuous version of the two tests above.
///
/// A weakening that also changed *which* operations were issued would be
/// rejected on the issue trace, and the two tests above would pass without ever
/// establishing that the delivery comparison works. This runs each weakening
/// against its own ledger — so the run is recorded rather than rejected — and
/// then compares the traces by hand: the operation counts must match, and the
/// difference must be a delivery one. `agrees_with` checks operation count and
/// issue order before anything else, so a delivery variant coming back is proof
/// the issue trace agreed.
#[test]
fn a_weakened_backend_still_issues_the_same_operations() {
    let config = Config::small();
    let (read_path, write_path) = prepare("issued", &config);
    let which = available()[0];

    for scenario in Scenario::all() {
        let job = job(scenario, &read_path, &write_path, &config);

        let honest = trace_of(which, Weakness::None, &config, &job);
        for weakness in [Weakness::SkipsWork, Weakness::HollowDelivery] {
            let weakened = trace_of(which, weakness, &config, &job);
            assert_eq!(
                honest.operations(),
                weakened.operations(),
                "{weakness:?} changed what {} issued, not what it delivered",
                scenario.name()
            );
            let mismatch = honest
                .agrees_with(&weakened)
                .expect_err("a weakened run must not agree with an honest one");
            assert!(
                matches!(
                    mismatch,
                    Mismatch::DeliveredBytes { .. } | Mismatch::Delivered { .. }
                ),
                "{weakness:?} on {} differed in the issue trace, not in delivery: {mismatch}",
                scenario.name()
            );
        }
    }
}

/// The trace that reaches the ledger comes from a **timed** iteration, not from
/// the untimed warm-up.
///
/// [`measure_combination`] runs an untimed warm-up before handing the closure to
/// the timer, and verifies the last timed iteration rather than that warm-up.
/// Every other weakening here is uniform across runs, so it is caught either
/// way and none of them can tell the two apart: a measurement that verified the
/// warm-up instead would leave the whole suite green. This one weakens from the
/// first timed iteration onward, leaving the warm-up honest, so it is rejected
/// only if what the ledger saw came from a timed iteration.
#[test]
fn the_warm_up_is_not_what_gets_verified() {
    let config = Config::small();
    let (read_path, write_path) = prepare("warm-up", &config);

    for scenario in Scenario::all() {
        let job = job(scenario, &read_path, &write_path, &config);
        let backends = available();
        let mut ledger = Ledger::new();

        run(backends[0], Weakness::None, &config, &job, &mut ledger)
            .unwrap_or_else(|f| panic!("the reference backend was rejected: {f}"));

        weaken::reset_runs();
        let failure = run(
            backends[1],
            Weakness::HollowFromRun(1),
            &config,
            &job,
            &mut ledger,
        )
        .expect_err("a backend hollow in every timed iteration must be rejected");
        assert!(
            matches!(failure.mismatch, Mismatch::Delivered { .. }),
            "{} was rejected for the wrong reason: {failure}",
            scenario.name()
        );
    }
}

/// A backend that does not do the same work on every iteration fails a run.
///
/// SC-005 asks that removing a verification step break a test; this is the test
/// for the first-versus-last comparison, which the two recorded mutations did
/// not reach — deleting it left the whole suite green.
///
/// Weakened from the *last* iteration onward, so the warm-up and the first timed
/// iteration are honest and only the last is not. Its own ledger, with no other
/// backend in front of it, so the cross-backend comparison cannot be what
/// rejects it: the first-versus-last comparison inside
/// [`measure_combination`] is the only thing left that can.
#[test]
fn a_backend_that_drifts_between_iterations_fails_a_run() {
    let config = Config::small();
    let (read_path, write_path) = prepare("drifts", &config);

    for scenario in Scenario::all() {
        let job = job(scenario, &read_path, &write_path, &config);
        let mut ledger = Ledger::new();

        weaken::reset_runs();
        let failure = run(
            available()[0],
            // Run 0 is the warm-up and run 1 the first timed iteration, so with
            // ITERATIONS = 2 this hollows the last iteration and nothing else.
            Weakness::HollowFromRun(ITERATIONS),
            &config,
            &job,
            &mut ledger,
        )
        .expect_err("a backend that changed between iterations must be rejected");
        assert!(
            failure.reference.contains("first iteration")
                && failure.backend.contains("last iteration"),
            "{} was rejected by something other than the first-versus-last check: {failure}",
            scenario.name()
        );
        assert!(
            matches!(failure.mismatch, Mismatch::Delivered { .. }),
            "{} was rejected for the wrong reason: {failure}",
            scenario.name()
        );
    }
}

/// Runs one combination against a ledger of its own, so the trace comes back
/// rather than being rejected, and returns what it did.
fn trace_of(which: Which, weakness: Weakness, config: &Config, job: &Job<'_>) -> Trace {
    let mut ledger = Ledger::new();
    match run(which, weakness, config, job, &mut ledger) {
        Ok(Record::Measured { trace, .. }) => trace,
        other => panic!("{weakness:?} did not produce a measurement: {other:?}"),
    }
}

// The two tests below are the fairness machinery's reach over compio. Every
// weakening test above this point takes a fixed index and so only ever weakens a
// thread-pool backend; these two weaken compio itself, by identity.
//
// They are two tests and not one because the requirement names two checks — the
// trace and the digest — and one weakening reaches only one of them. Which arm a
// weakening lands on is decided by the comparison order in
// `Trace::agrees_with`, where `delivered_bytes` is compared before the digest.
//
// # The three arms no backend-level weakening can reach
//
// `Mismatch::OperationCount`, `Mismatch::IssueOrder` and
// `Mismatch::CompletionCount` are structurally unreachable from here, and that
// is recorded rather than left as an apparent gap — an unexplained hole is what
// gets closed later by weakening a check.
//
// The first two are unreachable because `Trace::issued` is pushed by the
// *scenario*, with the offset and length the scenario chose, and nothing the
// backend does can perturb it. (The scenario may open the file first —
// `read_only` calls `open_read` at `scenario.rs:228` before its issue loop at
// `:231` — but an open does not touch the trace, and the issue loop's shape is
// fixed by `operations` and `offset_of`.) A weakened backend is handed the same
// operations in the same order by construction, so neither the count nor the
// order of issues can diverge.
//
// The third is unreachable because a skipped read still completes.
// `Runner::run` awaits an outcome and then calls `trace.delivered(..)`
// (`concurrency.rs:303-304`); the only path that skips the call is the `?` on
// the line above, and an `Err` there propagates out and aborts the whole run
// rather than shortening the trace. `SkipsWork` returns `Ok(0)`, never `Err`,
// so the completion count is the same whether the backend did the work or
// returned `Ok(0)` without doing it. That is also why the two existing
// `SkipsWork` assertions no longer hedge with `CompletionCount` — that branch
// was dead, and a hedge that cannot fire makes an assertion weaker than it
// looks. `CompletionCount` is compared *before* `DeliveredBytes`
// (`verify.rs:118` and `:127`), so had it been reachable it would have fired
// first and the hedge would have been the branch that ran.

/// A weakened compio backend fails a run on the delivered-byte count.
///
/// `SkipsWork` returns `Ok(0)` without reading for one index in four, starting
/// at index 0 — so every scenario's read phase diverges, not just those long
/// enough to reach a later multiple. Fewer bytes reach application-readable
/// memory than the honest reference put there, and `delivered_bytes` is compared
/// before the digest, so this lands on `DeliveredBytes` rather than `Delivered`.
#[test]
fn a_weakened_compio_backend_fails_on_the_delivered_byte_count() {
    let config = Config::small();
    let (read_path, write_path) = prepare("compio-skips", &config);

    for scenario in Scenario::all() {
        let job = job(scenario, &read_path, &write_path, &config);
        let mut ledger = Ledger::new();

        run(available()[0], Weakness::None, &config, &job, &mut ledger)
            .unwrap_or_else(|f| panic!("the reference backend was rejected: {f}"));

        let failure = run(
            compio_backend(),
            Weakness::SkipsWork,
            &config,
            &job,
            &mut ledger,
        )
        .expect_err("a compio backend that skipped a quarter of its reads must be rejected");
        assert!(
            matches!(failure.mismatch, Mismatch::DeliveredBytes { .. }),
            "{} was rejected for the wrong reason: {failure}",
            scenario.name()
        );
    }
}

/// A weakened compio backend fails a run on the digest.
///
/// The subtler half, and the one the digest exists for: `HollowDelivery` reports
/// full transfers and overwrites the buffer, so every count agrees and only the
/// bytes differ. Nothing short of comparing what reached application-visible
/// memory catches it — and this establishes that the comparison reaches compio's
/// buffers and not only the thread-pool backends'.
#[test]
fn a_weakened_compio_backend_fails_on_the_digest() {
    let config = Config::small();
    let (read_path, write_path) = prepare("compio-hollow", &config);

    for scenario in Scenario::all() {
        let job = job(scenario, &read_path, &write_path, &config);
        let mut ledger = Ledger::new();

        run(available()[0], Weakness::None, &config, &job, &mut ledger)
            .unwrap_or_else(|f| panic!("the reference backend was rejected: {f}"));

        let failure = run(
            compio_backend(),
            Weakness::HollowDelivery,
            &config,
            &job,
            &mut ledger,
        )
        .expect_err("a compio backend delivering nothing from the file must be rejected");
        assert!(
            matches!(failure.mismatch, Mismatch::Delivered { .. }),
            "{} was rejected for the wrong reason: {failure}",
            scenario.name()
        );
    }
}
