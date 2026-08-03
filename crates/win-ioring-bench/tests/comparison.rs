//! Every backend, over every scenario, through the one entry point.
//!
//! The point of these is not performance — a test host's timings are worthless —
//! but the properties the comparison rests on: that the same logic runs against
//! each backend, that they all do the same work, and that a backend which does
//! *not* is rejected rather than reported.

use win_ioring_bench::backend::Availability;
use win_ioring_bench::backends::ioring;
use win_ioring_bench::config::Config;
use win_ioring_bench::fairness::Ledger;
use win_ioring_bench::harness::{Job, Record, Which, measure_combination, run_one};
use win_ioring_bench::measure::Repeats;
use win_ioring_bench::scenario::{Rng, Scenario};
use win_ioring_bench::verify::{Mismatch, Phase, Trace};
use win_ioring_bench::workload;

/// Prepares a small working set for the tests.
fn prepare(tag: &str, config: &Config) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = workload::data_dir().join(format!("test-{tag}"));
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

/// SC-014: every backend is instantiated from one common entry point and runs
/// every scenario, with no scenario code naming an implementation.
#[test]
fn every_backend_runs_every_scenario() {
    let config = Config::small();
    let (read_path, write_path) = prepare("all", &config);

    for scenario in Scenario::all() {
        let (block, operations) = config.shape(scenario);
        // One ledger for this (scenario, depth), shared across its backends.
        let mut ledger = Ledger::new();

        for which in Which::all() {
            if !ring_available() && matches!(which, Which::RingPlain | Which::RingRegistered) {
                continue;
            }
            let job = Job {
                scenario,
                read_path: &read_path,
                write_path: &write_path,
                block,
                operations,
                depth: 1,
            };
            let run = run_one(which, &config, &job, &mut ledger);
            run.fairness.unwrap_or_else(|f| {
                panic!("{} was rejected on {}: {f}", run.name, scenario.name())
            });
            let measured = run
                .measured
                .unwrap_or_else(|e| panic!("{} failed on {}: {e}", run.name, scenario.name()));
            assert_eq!(
                measured.samples.len(),
                config.repeats,
                "{} produced the wrong number of repeats",
                run.name
            );
            assert!(
                measured.trace.operations() > 0,
                "{} issued nothing on {}",
                run.name,
                scenario.name()
            );
        }
    }
}

/// SC-016 and SC-022: every backend issues the same operations and delivers the
/// same bytes.
///
/// The property the whole comparison rests on. It is also what catches a backend
/// that reports a transfer without putting the data anywhere the application can
/// read — the exact hazard the registered path used to have.
///
/// The agreement is asserted through the ledger the measurement itself consults,
/// rather than through a copy of that comparison written here.
#[test]
fn every_backend_does_the_same_work() {
    if !ring_available() {
        return;
    }
    let config = Config::small();
    let (read_path, write_path) = prepare("same", &config);

    for scenario in Scenario::all() {
        let (block, operations) = config.shape(scenario);

        let mut ledger = Ledger::new();
        let mut last_trace: Option<Trace> = None;
        for which in Which::all() {
            let job = Job {
                scenario,
                read_path: &read_path,
                write_path: &write_path,
                block,
                operations,
                depth: 4,
            };
            let run = run_one(which, &config, &job, &mut ledger);
            run.fairness
                .unwrap_or_else(|f| panic!("{} disagreed on {}: {f}", run.name, scenario.name()));
            let measured = run
                .measured
                .unwrap_or_else(|e| panic!("{} failed on {}: {e}", run.name, scenario.name()));
            last_trace = Some(measured.trace);
        }

        let trace = last_trace.expect("at least one backend ran");
        assert!(
            trace.delivered_total() > 0,
            "{} delivered no bytes at all",
            scenario.name()
        );
    }
}

/// The gate on the ledger being shared across a combination's backends rather
/// than constructed per call.
///
/// Two real runs of the same backend through `measure_combination`, the second
/// asked to do half the work, against one ledger: the rejection has to come out
/// of the measured path. A ledger constructed per call — inside
/// `measure_combination` or inside `run_one` — would make each run its own
/// reference, no comparison could ever fail, and the cross-backend check would
/// be gone with nothing to notice it.
#[test]
fn a_backend_that_ran_a_different_job_is_rejected() {
    let config = Config::small();
    let (read_path, write_path) = prepare("rejected", &config);
    let scenario = Scenario::SequentialRead;
    let (block, operations) = config.shape(scenario);
    let depth = 1;
    // The thread-pool backend, so this gate holds on a host without a ring.
    let which = Which::TokioOne;

    let mut ledger = Ledger::new();

    let job = Job {
        scenario,
        read_path: &read_path,
        write_path: &write_path,
        block,
        operations,
        depth,
    };
    let mut timer = Repeats::new(config.repeats);
    let first = measure_combination(which, &config, &job, &mut ledger, &mut timer)
        .expect("the first observation has nothing to disagree with");
    assert!(
        matches!(first, Record::Measured { .. }),
        "the reference run did not produce a measurement"
    );

    let halved = Job {
        operations: operations / 2,
        ..job.clone()
    };
    let mut timer = Repeats::new(config.repeats);
    let failure = measure_combination(which, &config, &halved, &mut ledger, &mut timer)
        .expect_err("half the operations is not the same work");
    assert!(
        matches!(failure.mismatch, Mismatch::OperationCount { .. }),
        "rejected for the wrong reason: {failure}"
    );
}

/// SC-021: a randomised scenario issues an identical sequence on two runs.
///
/// Without this the random scenario would compare different work each time, and
/// every other check would be meaningless for it.
#[test]
fn the_random_scenario_is_reproducible() {
    let mut first = Rng::new(0x5EED_1234_ABCD_0001);
    let mut second = Rng::new(0x5EED_1234_ABCD_0001);
    let a: Vec<u64> = (0..64).map(|_| first.below(1000)).collect();
    let b: Vec<u64> = (0..64).map(|_| second.below(1000)).collect();
    assert_eq!(a, b, "the same seed must produce the same sequence");
    assert!(
        a.iter().collect::<std::collections::HashSet<_>>().len() > 1,
        "a sequence of one repeated value would make this vacuous"
    );
}

/// SC-017: a backend deliberately made to do less work is rejected, not
/// reported. The check has to bite, or none of the others mean anything.
#[test]
fn a_backend_that_does_less_work_is_rejected() {
    let mut honest = Trace::new();
    let mut lazy = Trace::new();

    for i in 0..8_u64 {
        honest.issued(i * 64, 64);
        lazy.issued(i * 64, 64);
    }
    for i in 0..8_u64 {
        honest.delivered(Phase::Read, i * 64, 64, &[7_u8; 64]);
        // Skips the last two: fewer completions, less delivered.
        if i < 6 {
            lazy.delivered(Phase::Read, i * 64, 64, &[7_u8; 64]);
        }
    }

    assert!(
        honest.agrees_with(&lazy).is_err(),
        "a backend completing fewer operations must be rejected"
    );
}

/// The subtler form: the same operations, the same counts, but the bytes never
/// reached anywhere the application could read them.
#[test]
fn a_backend_that_delivers_nothing_readable_is_rejected() {
    let mut honest = Trace::new();
    let mut hollow = Trace::new();

    for i in 0..4_u64 {
        honest.issued(i * 64, 64);
        hollow.issued(i * 64, 64);
        honest.delivered(Phase::Read, i * 64, 64, &[3_u8; 64]);
        // Reports the same transfer count, but the application sees nothing.
        hollow.delivered(Phase::Read, i * 64, 64, &[]);
    }

    assert!(
        honest.agrees_with(&hollow).is_err(),
        "a backend that delivers no readable bytes must be rejected"
    );
}
