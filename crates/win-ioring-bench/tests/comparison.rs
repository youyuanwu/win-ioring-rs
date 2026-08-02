//! Every backend, over every scenario, through the one entry point.
//!
//! The point of these is not performance — a test host's timings are worthless —
//! but the properties the comparison rests on: that the same logic runs against
//! each backend, that they all do the same work, and that a backend which does
//! *not* is rejected rather than reported.

use win_ioring_bench::backend::Availability;
use win_ioring_bench::backends::ioring;
use win_ioring_bench::config::Config;
use win_ioring_bench::harness::{Job, Which, run_one};
use win_ioring_bench::scenario::{Rng, Scenario};
use win_ioring_bench::verify::{Phase, Trace};
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

/// The block size and operation count each scenario uses in these tests.
fn shape(config: &Config, scenario: Scenario) -> (u32, usize) {
    let (block, total) = match scenario {
        Scenario::SequentialRead => (config.sequential_block, config.read_file_bytes),
        Scenario::RandomRead => (config.random_block, config.read_file_bytes / 8),
        Scenario::WriteThenRead => (config.write_block, config.write_file_bytes),
    };
    (block, config.operations(total, block))
}

/// SC-014: every backend is instantiated from one common entry point and runs
/// every scenario, with no scenario code naming an implementation.
#[test]
fn every_backend_runs_every_scenario() {
    let config = Config::small();
    let (read_path, write_path) = prepare("all", &config);

    for scenario in Scenario::all() {
        let (block, operations) = shape(&config, scenario);

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
            let run = run_one(which, &config, &job);
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
#[test]
fn every_backend_does_the_same_work() {
    if !ring_available() {
        return;
    }
    let config = Config::small();
    let (read_path, write_path) = prepare("same", &config);

    for scenario in Scenario::all() {
        let (block, operations) = shape(&config, scenario);

        let mut reference: Option<(String, Trace)> = None;
        for which in Which::all() {
            let job = Job {
                scenario,
                read_path: &read_path,
                write_path: &write_path,
                block,
                operations,
                depth: 4,
            };
            let run = run_one(which, &config, &job);
            let measured = run
                .measured
                .unwrap_or_else(|e| panic!("{} failed on {}: {e}", run.name, scenario.name()));
            match &reference {
                None => reference = Some((run.name, measured.trace)),
                Some((ref_name, ref_trace)) => {
                    if let Err(mismatch) = ref_trace.agrees_with(&measured.trace) {
                        panic!(
                            "{} disagreed with {ref_name} on {}: {mismatch}",
                            run.name,
                            scenario.name()
                        );
                    }
                }
            }
        }

        let (_, trace) = reference.expect("at least one backend ran");
        assert!(
            trace.delivered_total() > 0,
            "{} delivered no bytes at all",
            scenario.name()
        );
    }
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
