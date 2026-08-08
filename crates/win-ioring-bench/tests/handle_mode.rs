//! The handle-mode arm's measurement boundary, and the properties that make it
//! an experiment rather than a pair of numbers.
//!
//! The arm's grid, budget and naming are settled in the library's own tests.
//! What is settled here is the thing those cannot reach: that
//! [`win_ioring_bench::harness::Opens`] actually changes where the opens
//! happen, and that the arm's configurations really do differ in the one
//! respect the experiment claims and in no other.
//!
//! # Why this file exists at all
//!
//! `docs/testing.md`'s named pattern: a passing check needs a twin that proves
//! it can fail. The arm's most dangerous defect is not a wrong number, it is a
//! **clean null** — an A/B that silently compares two identical arms, passes
//! every fairness and shape check, and publishes "no effect" at every depth.
//! `docs/testing.md` also records that this project under-scrutinises
//! unflattering results, which makes exactly that error the cheapest one
//! available. So the properties that would have to hold for a null to be real
//! are asserted here rather than assumed.

use std::path::PathBuf;

use win_ioring_bench::backend::Availability;
use win_ioring_bench::backends::ioring::{self, HandleMode};
use win_ioring_bench::config::Config;
use win_ioring_bench::fairness::Ledger;
use win_ioring_bench::handle_mode::{CONFIGS, DEPTHS, OPENS, grid};
use win_ioring_bench::harness::{
    Job, Opens, Record, Untimed, Which, handle_mode_checks, measure_combination_with,
};
use win_ioring_bench::scenario::Scenario;
use win_ioring_bench::session::opens;
use win_ioring_bench::weaken::Weakness;
use win_ioring_bench::workload;

/// Prepares a small working set, in this file's own directory.
fn prepare(tag: &str, config: &Config) -> (PathBuf, PathBuf) {
    let dir = workload::data_dir().join(format!("test-handle-mode-{tag}"));
    std::fs::create_dir_all(&dir).unwrap();
    let read_path = dir.join("read.dat");
    let write_path = dir.join("write.dat");
    workload::ensure_file(&read_path, config.read_file_bytes).unwrap();
    workload::warm(&read_path).unwrap();
    (read_path, write_path)
}

fn ring_available() -> bool {
    matches!(ioring::availability(), Availability::Available)
}

/// [`Opens::Hoisted`] really hoists — and this can fail.
///
/// The twin for the arm's boundary. `Opens` is a two-variant enum matched in one
/// place, and a refactor that collapsed `Hoisted` into `PerIteration` would
/// compile, pass every existing test, produce plausible numbers, and quietly
/// fold per-open cost into the A/B delta. Worse, it would do so *at depth 1*,
/// which is the arm's negative control — the one cell whose job is to say "no
/// effect here". A per-open difference appearing there would have been read as
/// run-level drift, and the safeguard would have been disarmed without anything
/// reporting it.
///
/// The observable used is deliberately not a timing. The pre-opened path has no
/// form for [`Scenario::WriteThenRead`] — it opens for writing, and a
/// read-opened file cannot serve it — so it refuses that scenario with
/// `InvalidInput`. The per-iteration path runs it happily. That difference is
/// structural, instant, and cannot be flaky on a loaded test host.
#[test]
fn hoisting_really_moves_the_opens_out_of_the_timed_region() {
    if !ring_available() {
        eprintln!("no io_ring on this host; skipping");
        return;
    }
    let config = Config::small();
    let (read_path, write_path) = prepare("hoisting", &config);
    let (block, operations) = config.work(Scenario::WriteThenRead);
    let job = Job {
        scenario: Scenario::WriteThenRead,
        read_path: &read_path,
        write_path: &write_path,
        block,
        operations,
        depth: 1,
    };

    // Per-iteration: the scenario runs, because that path opens for writing.
    let mut ledger = Ledger::new();
    let record = measure_combination_with(
        Which::RingPlain,
        Weakness::None,
        Opens::PerIteration,
        &config,
        &job,
        &mut ledger,
        &mut Untimed { iterations: 1 },
    )
    .expect("the per-iteration path should run the write scenario");
    assert!(
        matches!(record, Record::Measured { .. }),
        "the per-iteration path did not measure the write scenario: {record:?}"
    );

    // Hoisted: refused, because the hoisted open is a *read* open and there is
    // no pre-opened form of a write. If this ever starts succeeding, the two
    // paths have converged and the hoisting is no longer happening.
    let mut ledger = Ledger::new();
    let record = measure_combination_with(
        Which::RingPlain,
        Weakness::None,
        Opens::Hoisted,
        &config,
        &job,
        &mut ledger,
        &mut Untimed { iterations: 1 },
    )
    .expect("a refusal is a record, not a fairness failure");
    match record {
        Record::Failed { error, .. } => assert_eq!(
            error.kind(),
            std::io::ErrorKind::InvalidInput,
            "the hoisted path failed for the wrong reason: {error}"
        ),
        other => panic!(
            "the hoisted path ran the write scenario, which it cannot do with a \
             read-opened file. `Opens::Hoisted` is behaving like \
             `Opens::PerIteration`, which silently folds per-open cost into the \
             A/B delta and disarms the depth-1 negative control. Got: {other:?}"
        ),
    }
}

/// The boundary **the arm actually uses** keeps opens out of the timed region.
///
/// The distinction from
/// [`hoisting_really_moves_the_opens_out_of_the_timed_region`] is the whole
/// point of this test. That one passes `Opens::Hoisted` as a literal, so it
/// proves the *variant* behaves correctly and says nothing about which variant
/// the arm selects. The selection used to be a literal in
/// `benches/handle-mode.rs`, and because a `harness = false` target compiles
/// `#[test]` without running it, reversing it to `Opens::PerIteration` left the
/// whole suite green — the arm's central premise guarded by a gate that could
/// not fail.
///
/// This drives [`OPENS`] itself, and checks the property rather than the value.
/// Asserting `OPENS == Opens::Hoisted` would only restate the constant; a
/// count of opens through [`opens`] observes what the constant causes.
///
/// Two readings are taken because one would not separate the two ways this can
/// break. A non-zero count rules out the reversal, since the per-iteration path
/// never reaches the hoisted open seam at all. Equality across a fourfold change
/// in iterations rules out an open that has crept back into the loop — the B1
/// defect, where a `std::fs::metadata` call (which opens the path on Windows)
/// was running per iteration inside the timed region.
#[test]
fn the_boundary_the_arm_uses_keeps_opens_out_of_the_timed_region() {
    if !ring_available() {
        eprintln!("no io_ring on this host; skipping");
        return;
    }
    let config = Config::small();
    let (read_path, write_path) = prepare("boundary", &config);
    let (block, operations) = config.work(Scenario::SequentialRead);

    let count = |iterations: usize| {
        let job = Job {
            scenario: Scenario::SequentialRead,
            read_path: &read_path,
            write_path: &write_path,
            block,
            operations,
            depth: 1,
        };
        let mut ledger = Ledger::new();
        let before = opens();
        let record = measure_combination_with(
            Which::RingPlain,
            Weakness::None,
            OPENS,
            &config,
            &job,
            &mut ledger,
            &mut Untimed { iterations },
        )
        .expect("the arm's own boundary should run the read scenario");
        assert!(
            matches!(record, Record::Measured { .. }),
            "the arm's boundary did not measure: {record:?}"
        );
        opens() - before
    };

    let one = count(1);
    let four = count(4);

    assert!(
        one > 0,
        "the arm's boundary performed no hoisted open at all, which is what \
         `Opens::PerIteration` does. Per-open cost is then inside the timed \
         region, where it lands on the depth-1 negative control and is read as \
         run-level drift rather than as the confound it is."
    );
    assert_eq!(
        one, four,
        "opens scaled with the iteration count ({one} for 1 iteration, {four} \
         for 4), so an open is inside the timed region. The arm attributes its \
         delta to per-operation cost; a per-open cost folded into it is a \
         different quantity wearing the same label."
    );
}

/// Hoisting refuses to be combined with a weakening.
/// Hoisting exists for a single-variable experiment. A weakened backend is a
/// second variable, and a second variable inside a controlled experiment is the
/// experiment's whole failure mode rather than a degraded version of it.
#[test]
#[should_panic(expected = "adds a second variable")]
fn hoisting_a_weakened_backend_is_refused() {
    let config = Config::small();
    let (read_path, write_path) = prepare("weakened", &config);
    let (block, operations) = config.work(Scenario::SequentialRead);
    let job = Job {
        scenario: Scenario::SequentialRead,
        read_path: &read_path,
        write_path: &write_path,
        block,
        operations,
        depth: 1,
    };
    let mut ledger = Ledger::new();
    let _ = measure_combination_with(
        Which::RingPlain,
        Weakness::SkipsWork,
        Opens::Hoisted,
        &config,
        &job,
        &mut ledger,
        &mut Untimed { iterations: 1 },
    );
}

/// Every configuration in the arm runs end to end, through the hoisted path,
/// with its handle mode read back off the kernel — and the read-back is
/// **reached**, once per configuration.
///
/// This is the arm's smoke test at the small configuration, and it is the check
/// that would fire if a synchronous configuration silently opened an overlapped
/// handle. That failure is the one worth spending a test on: it produces no
/// error, no warning and no anomaly — just an A/B comparing a handle against an
/// identical handle, reporting a clean null at every depth.
///
/// The counter assertion is the part that was missing at first, and its absence
/// is instructive. `verify_handle_mode` has `#[should_panic]` twins proving it
/// *can* fail — but nothing proved it was *called*, so deleting the one line
/// that calls it left all the library tests, all the integration tests and the
/// bench target's smoke run green. Twins for a function are not twins for its
/// call site.
#[test]
fn every_configuration_runs_the_hoisted_path_with_its_mode_confirmed() {
    if !ring_available() {
        eprintln!("no io_ring on this host; skipping");
        return;
    }
    let config = Config::small();
    let (read_path, write_path) = prepare("end-to-end", &config);
    let (block, operations) = config.work(Scenario::SequentialRead);

    // One ledger across all six, as the arm itself uses: it is what compares the
    // two handle modes' work against each other rather than each against itself.
    let mut ledger = Ledger::new();
    for which in CONFIGS {
        let job = Job {
            scenario: Scenario::SequentialRead,
            read_path: &read_path,
            write_path: &write_path,
            block,
            operations,
            depth: 1,
        };
        let before = handle_mode_checks();
        let record = measure_combination_with(
            which,
            Weakness::None,
            Opens::Hoisted,
            &config,
            &job,
            &mut ledger,
            &mut Untimed { iterations: 2 },
        )
        .unwrap_or_else(|failure| {
            panic!(
                "{which:?} disagreed with its peers about the work done. In this \
                 arm that is worse than in the matrix: the entire claim is that \
                 the only difference between the two handle modes is the handle \
                 mode. {failure}"
            )
        });
        match record {
            Record::Measured { .. } => {}
            other => panic!("{which:?} did not measure: {other:?}"),
        }
        assert_eq!(
            handle_mode_checks(),
            before + 1,
            "{which:?} ran the hoisted path without its handle mode being read \
             back. The safeguard against a clean null is not reached for this \
             configuration."
        );
    }
}

/// The published matrix pays nothing for this arm's safeguard.
///
/// The negative half of the counter assertion above, and a measurement rather
/// than the structural argument it replaces. The matrix runs
/// [`Opens::PerIteration`], so no handle is hoisted and no mode is queried; a
/// read-back that leaked onto that path would add a syscall per measured
/// combination to figures this repository publishes.
#[test]
fn the_per_iteration_path_performs_no_read_back() {
    if !ring_available() {
        eprintln!("no io_ring on this host; skipping");
        return;
    }
    let config = Config::small();
    let (read_path, write_path) = prepare("no-read-back", &config);
    let (block, operations) = config.work(Scenario::SequentialRead);
    let job = Job {
        scenario: Scenario::SequentialRead,
        read_path: &read_path,
        write_path: &write_path,
        block,
        operations,
        depth: 1,
    };

    let mut ledger = Ledger::new();
    let before = handle_mode_checks();
    let record = measure_combination_with(
        Which::RingPlain,
        Weakness::None,
        Opens::PerIteration,
        &config,
        &job,
        &mut ledger,
        &mut Untimed { iterations: 2 },
    )
    .expect("the matrix path should measure");
    assert!(matches!(record, Record::Measured { .. }));
    assert_eq!(
        handle_mode_checks(),
        before,
        "the matrix path performed a handle-mode read-back; the published \
         figures would be paying for this arm's safeguard"
    );
}

/// The two handle modes are assigned to different configurations, and the arm
/// contains both.
///
/// A grid that had lost one of the two modes would still be a grid, still be
/// affordable, still rotate, and still produce a full table of numbers with
/// nothing to compare against.
#[test]
fn the_arm_contains_both_handle_modes_at_every_depth() {
    for depth in DEPTHS {
        let at_depth: Vec<Which> = grid()
            .into_iter()
            .filter(|cell| cell.depth == depth && cell.scenario == Scenario::SequentialRead)
            .map(|cell| cell.config)
            .collect();
        let overlapped = at_depth
            .iter()
            .filter(|w| w.handle_mode() == HandleMode::Overlapped && w.builds_a_driver())
            .count();
        let synchronous = at_depth
            .iter()
            .filter(|w| w.handle_mode() == HandleMode::Synchronous)
            .count();
        assert_eq!(
            overlapped, 2,
            "depth {depth} does not carry both overlapped ring backends"
        );
        assert_eq!(
            synchronous, 2,
            "depth {depth} does not carry both synchronous ring backends; an A/B \
             missing one arm reports nothing"
        );
    }
}

/// Each synchronous configuration differs from its twin in the handle mode and
/// in nothing else the arm can observe.
///
/// The pairing is what the A/B delta is computed over. A `overlapped_twin` that
/// returned the identity, or that paired a plain backend with a registered one,
/// would produce a delta between two different backends and label it a handle
/// mode effect.
#[test]
fn each_synchronous_configuration_pairs_with_a_twin_that_differs_only_in_mode() {
    for which in CONFIGS {
        if which.handle_mode() != HandleMode::Synchronous {
            continue;
        }
        let twin = which.overlapped_twin();
        assert_ne!(
            twin, which,
            "{which:?} is its own twin; there is nothing to compare it against"
        );
        assert_eq!(
            twin.handle_mode(),
            HandleMode::Overlapped,
            "{which:?}'s twin is not overlapped"
        );
        assert!(
            CONFIGS.contains(&twin),
            "{which:?}'s twin {twin:?} is not in the arm, so the pair cannot be \
             measured in the same run — which is the entire reason this arm \
             exists rather than a before/after comparison"
        );
        // The slugs differ only by the mode's suffix, which is how the pairing
        // survives into the Criterion keys a reader compares.
        assert_eq!(
            format!("{}-sync", twin.slug()),
            which.slug(),
            "the slug pairing does not follow the twin pairing"
        );
    }
}
