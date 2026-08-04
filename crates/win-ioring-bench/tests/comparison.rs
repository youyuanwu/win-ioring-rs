//! Every backend, over every scenario, through the one seam that measures them.
//!
//! The point of these is not performance — a test host's timings are worthless —
//! but the properties the comparison rests on: that the same logic runs against
//! each backend, that they all do the same work, and that the facts a reader
//! needs beside a timing are the ones a real measurement produced. That a
//! backend which does *less* is rejected rather than reported is settled in
//! `tests/fairness.rs`, by a backend that really does less.

use std::io;
use std::path::PathBuf;

use win_ioring_bench::backend::Availability;
use win_ioring_bench::backends::ioring;
use win_ioring_bench::concurrency::{Shape, predicted_mean_depth};
use win_ioring_bench::config::Config;
use win_ioring_bench::fairness::Ledger;
use win_ioring_bench::harness::{Job, Record, Timed, Timer, Untimed, Which, measure_combination};
use win_ioring_bench::scenario::{Rng, Scenario};
use win_ioring_bench::session::Prepared;
use win_ioring_bench::verify::{Mismatch, Trace};
use win_ioring_bench::weaken::Weakness;
use win_ioring_bench::workload;

/// Prepares a small working set for the tests.
fn prepare(tag: &str, config: &Config) -> (PathBuf, PathBuf) {
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

/// FR-001: one shared piece of application logic runs against every backend,
/// with no scenario code naming an implementation.
///
/// Driven through `measure_combination`, the same function the benchmark target
/// calls, with a timer that runs iterations and times nothing.
#[test]
fn every_backend_runs_every_scenario() {
    let config = Config::small();
    let (read_path, write_path) = prepare("all", &config);

    for scenario in Scenario::all() {
        let (block, operations) = config.work(scenario);
        // One ledger for this (scenario, depth), shared across its backends.
        let mut ledger = Ledger::new();

        for which in Which::all() {
            if !ring_available() && which.builds_a_driver() {
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
            let mut timer = Untimed { iterations: 2 };
            let record = measure_combination(
                which,
                Weakness::None,
                &config,
                &job,
                &mut ledger,
                &mut timer,
            )
            .unwrap_or_else(|f| panic!("{which:?} was rejected on {}: {f}", scenario.name()));
            // What the deleted `samples.len() == config.repeats` assertion was
            // standing in for: `Untimed` collects no samples and `Config` no
            // longer carries a repeat count, so the property that survives is
            // that the combination produced a measurement whose trace issued
            // something.
            let Record::Measured { name, trace, .. } = &record else {
                panic!(
                    "{which:?} did not produce a measurement on {}: {record:?}",
                    scenario.name()
                );
            };
            assert!(
                trace.operations() > 0,
                "{name} issued nothing on {}",
                scenario.name()
            );
        }
    }
}

/// FR-002 and FR-003: the issue trace and the delivery digest are compared as
/// part of a run, so every backend issues the same operations and delivers the
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
        let (block, operations) = config.work(scenario);

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
            let mut timer = Untimed { iterations: 2 };
            let record = measure_combination(
                which,
                Weakness::None,
                &config,
                &job,
                &mut ledger,
                &mut timer,
            )
            .unwrap_or_else(|f| panic!("{which:?} disagreed on {}: {f}", scenario.name()));
            let Record::Measured { trace, .. } = record else {
                panic!("{which:?} did not run on {}", scenario.name());
            };
            last_trace = Some(trace);
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
/// `measure_combination` itself — would make each run its own reference, no
/// comparison could ever fail, and the cross-backend check would be gone with
/// nothing to notice it.
#[test]
fn a_backend_that_ran_a_different_job_is_rejected() {
    let config = Config::small();
    let (read_path, write_path) = prepare("rejected", &config);
    let scenario = Scenario::SequentialRead;
    let (block, operations) = config.work(scenario);
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
    let mut timer = Untimed { iterations: 2 };
    let first = measure_combination(
        which,
        Weakness::None,
        &config,
        &job,
        &mut ledger,
        &mut timer,
    )
    .expect("the first observation has nothing to disagree with");
    assert!(
        matches!(first, Record::Measured { .. }),
        "the reference run did not produce a measurement"
    );

    let halved = Job {
        operations: operations / 2,
        ..job.clone()
    };
    let mut timer = Untimed { iterations: 2 };
    let failure = measure_combination(
        which,
        Weakness::None,
        &config,
        &halved,
        &mut ledger,
        &mut timer,
    )
    .expect_err("half the operations is not the same work");
    assert!(
        matches!(failure.mismatch, Mismatch::OperationCount { .. }),
        "rejected for the wrong reason: {failure}"
    );
}

/// A fixed seed issues an identical sequence on two runs.
///
/// Without it the random scenario would compare different work on each run, and
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

/// SC-008: a backend the host cannot provide is reported as unavailable, not
/// measured and not silently absent.
///
/// **Vacuous on a host that has a ring**, and it says so rather than pretending
/// otherwise: there is no way to withdraw the platform's ring support from
/// inside a test. The half of SC-008 that is never vacuous —
/// prepared-then-failed — is the next test.
#[test]
fn an_unavailable_backend_is_reported_not_measured() {
    if ring_available() {
        eprintln!(
            "skipped: this host provides an I/O ring, so there is no unavailable backend to \
             observe. The prepared-then-failed half of SC-008 is covered by \
             a_backend_that_fails_after_preparing_is_reported_not_timed."
        );
        return;
    }
    let config = Config::small();
    let (read_path, write_path) = prepare("unavailable", &config);
    let scenario = Scenario::SequentialRead;
    let (block, operations) = config.work(scenario);

    let mut ledger = Ledger::new();
    let job = Job {
        scenario,
        read_path: &read_path,
        write_path: &write_path,
        block,
        operations,
        depth: 1,
    };
    let mut timer = Untimed { iterations: 1 };
    let record = measure_combination(
        Which::RingPlain,
        Weakness::None,
        &config,
        &job,
        &mut ledger,
        &mut timer,
    )
    .expect("an unavailable backend has no trace to disagree with");
    let Record::Unavailable { name, reason } = &record else {
        panic!("a host without a ring reported {record:?}");
    };
    assert!(!name.is_empty(), "an unavailable backend still has a name");
    assert!(!reason.is_empty(), "and the platform's reason for it");
}

/// SC-008: a backend that prepares and then fails is reported as failed, never
/// as a fast time.
///
/// The read path points at a file that does not exist. `prepare` still succeeds
/// — nothing in it opens a working file — and the failure arrives in the
/// **warm-up**, which is short-circuited to a `Record::Failed` before the timer
/// ever runs.
///
/// This covers the warm-up half of the post-`prepare` failure path and **only**
/// that half: because the missing file fails deterministically on the first
/// call, no timed iteration runs and the error slot is never written. That slot
/// is covered by `a_failure_inside_a_timed_iteration_fills_the_error_slot`.
#[test]
fn a_backend_that_fails_after_preparing_is_reported_not_timed() {
    let config = Config::small();
    let (_, write_path) = prepare("missing", &config);
    let scenario = Scenario::SequentialRead;
    let (block, operations) = config.work(scenario);
    let absent = workload::data_dir().join("test-missing").join("absent.dat");

    let mut ledger = Ledger::new();
    let job = Job {
        scenario,
        read_path: &absent,
        write_path: &write_path,
        block,
        operations,
        depth: 1,
    };
    let mut timer = Untimed { iterations: 3 };
    let record = measure_combination(
        Which::TokioOne,
        Weakness::None,
        &config,
        &job,
        &mut ledger,
        &mut timer,
    )
    .expect("a backend that never produced a trace cannot disagree with one");
    let Record::Failed { error, .. } = &record else {
        panic!("a missing read file produced {record:?}");
    };
    assert_eq!(
        error.kind(),
        io::ErrorKind::NotFound,
        "the failure carried the wrong reason: {error}"
    );
    // And the ledger holds nothing for this combination, so the next backend
    // becomes the reference rather than being compared against a run that never
    // happened.
    assert!(
        ledger.reference(scenario, 1).is_none(),
        "a failed run must not become the reference"
    );
}

/// The other half of the post-`prepare` failure path, and the only test that
/// exercises the error slot a timed iteration writes into.
///
/// The failure has to arrive *after* a successful warm-up, so it has to be
/// introduced between the warm-up and the iterations — and the only seam between
/// them is the [`Timer`], which is `pub` for exactly this reason. This timer
/// renames the read file aside, runs one iteration, and puts it back;
/// `Prepared::one`'s open then fails **inside the timed region**, where the
/// iteration closure has no way to return an error and records it into the slot
/// instead. Without this test that slot is dead weight whose removal no test
/// would notice.
#[test]
fn a_failure_inside_a_timed_iteration_fills_the_error_slot() {
    let config = Config::small();
    let (read_path, write_path) = prepare("vanishing", &config);
    let scenario = Scenario::SequentialRead;
    let (block, operations) = config.work(scenario);

    /// Runs one iteration with the read file moved out of the way.
    struct Vanishing {
        /// The file to move aside for the duration of the iteration.
        path: PathBuf,
        /// Where to move it.
        aside: PathBuf,
    }

    impl Timer for Vanishing {
        fn time<F, Fut>(&mut self, _timed: &Timed, prepared: &Prepared, mut one: F)
        where
            F: FnMut() -> Fut,
            Fut: Future<Output = ()>,
        {
            std::fs::rename(&self.path, &self.aside).expect("the read file exists to be moved");
            prepared.block_on(async {
                one().await;
            });
            std::fs::rename(&self.aside, &self.path).expect("and is put back");
        }
    }

    let mut ledger = Ledger::new();
    let job = Job {
        scenario,
        read_path: &read_path,
        write_path: &write_path,
        block,
        operations,
        depth: 1,
    };
    let mut timer = Vanishing {
        path: read_path.clone(),
        aside: read_path.with_extension("aside"),
    };
    let record = measure_combination(
        Which::TokioOne,
        Weakness::None,
        &config,
        &job,
        &mut ledger,
        &mut timer,
    )
    .expect("a run whose iteration failed has no trace to compare");
    let Record::Failed { error, .. } = &record else {
        panic!("a read file that vanished mid-run produced {record:?}");
    };
    assert_eq!(
        error.kind(),
        io::ErrorKind::NotFound,
        "the failure carried the wrong reason: {error}"
    );
    assert!(read_path.exists(), "the timer must put the file back");
}

/// SC-015 in miniature: repeated iterations do not grow the write file.
///
/// The write scenario truncates on open and writes one iteration's bytes from
/// offset zero, so five iterations must leave exactly what one wrote. A file
/// that grew would mean each iteration measured a larger job than the last.
/// Phase 5 checks the same property on a full run.
#[test]
fn the_write_file_does_not_grow_across_iterations() {
    let config = Config::small();
    let (read_path, write_path) = prepare("growth", &config);
    let scenario = Scenario::WriteThenRead;
    let (block, operations) = config.work(scenario);

    let mut ledger = Ledger::new();
    let job = Job {
        scenario,
        read_path: &read_path,
        write_path: &write_path,
        block,
        operations,
        depth: 1,
    };
    let mut timer = Untimed { iterations: 5 };
    let record = measure_combination(
        Which::TokioOne,
        Weakness::None,
        &config,
        &job,
        &mut ledger,
        &mut timer,
    )
    .expect("five identical iterations must agree with each other");
    assert!(
        matches!(record, Record::Measured { .. }),
        "the write scenario did not run: {record:?}"
    );
    assert_eq!(
        std::fs::metadata(&write_path).unwrap().len(),
        config.write_file_bytes(),
        "five iterations left more than one iteration's bytes"
    );
}

/// SC-014: a driver is built once per combination, not once per iteration.
///
/// **A lower bound, and one that excludes only the too-few case.**
/// `drivers_built()` is a process-global counter and this binary runs its tests
/// on several threads at once, so other tests here that build ring backends bump
/// it between this test's two reads. An exact delta would fail intermittently,
/// and a test that fails for reasons unrelated to its subject teaches a reader to
/// ignore it. The price is that this assertion cannot see an *excess*: under a
/// driver-per-iteration implementation `built` would be far larger than
/// `combinations` and `built >= combinations` would hold just as happily. What
/// it establishes is that a driver is not shared between combinations or skipped
/// altogether — the failure a per-combination design invites in the other
/// direction.
///
/// The too-many case is settled outside this test, deliberately: by the full
/// `cargo bench` run in Phase 5, in a process that builds nothing else, and by
/// the fairness account's own line, which prints drivers built against ring
/// combinations measured and marks the two when they differ.
#[test]
fn one_driver_is_built_per_combination() {
    if !ring_available() {
        eprintln!("skipped: this host provides no I/O ring, so no driver is ever built");
        return;
    }
    let config = Config::small();
    let (read_path, write_path) = prepare("drivers", &config);
    let scenario = Scenario::SequentialRead;
    let (block, operations) = config.work(scenario);

    let before = ioring::drivers_built();
    let mut combinations = 0_usize;
    for which in [Which::RingPlain, Which::RingRegistered] {
        let mut ledger = Ledger::new();
        let job = Job {
            scenario,
            read_path: &read_path,
            write_path: &write_path,
            block,
            operations,
            depth: 1,
        };
        let mut timer = Untimed { iterations: 8 };
        let record = measure_combination(
            which,
            Weakness::None,
            &config,
            &job,
            &mut ledger,
            &mut timer,
        )
        .expect("one backend against its own ledger cannot disagree");
        assert!(
            matches!(record, Record::Measured { .. }),
            "{which:?} did not run: {record:?}"
        );
        combinations += 1;
    }
    let built = ioring::drivers_built() - before;
    assert!(
        built >= combinations,
        "{built} drivers for {combinations} ring combinations — fewer than one each"
    );
}

/// FR-007: the shape a scenario declares is the shape it actually drives.
///
/// Runs bulk read and sequential read through the same seam, at the same depth,
/// over the same number of operations, and holds each to the closed-form mean
/// its shape predicts. The two predictions differ (2.5 against 3.90625 at depth
/// 4 over 64 operations), so a bulk read that quietly rolled — or a sequential
/// read that quietly batched — fails here rather than being reported as a
/// measurement of something it is not.
///
/// The expected shape is written out here rather than read from
/// [`Scenario::shape`]. Deriving it would move both sides of the comparison
/// together, and a scenario that declared the wrong shape *and* drove it would
/// pass: the test would confirm only that the runner is self-consistent, which
/// was never in doubt.
#[test]
fn each_scenario_drives_the_shape_it_declares() {
    let config = Config::small();
    let (read_path, write_path) = prepare("shape", &config);
    let depth = 4;

    let table = [
        (Scenario::SequentialRead, Shape::Rolling),
        (Scenario::RandomRead, Shape::Rolling),
        (Scenario::WriteThenRead, Shape::Rolling),
        (Scenario::BulkRead, Shape::Batched),
    ];
    assert_eq!(
        table.len(),
        Scenario::all().len(),
        "a scenario was added without a row here, so its declared shape is unchecked"
    );
    for scenario in Scenario::all() {
        assert!(
            table.iter().any(|&(s, _)| s == scenario),
            "{} has no row here, so its declared shape is unchecked",
            scenario.name()
        );
    }

    for (scenario, expected) in table {
        assert_eq!(
            scenario.shape(),
            expected,
            "{} declares {:?}, not the {expected:?} this test measures it against",
            scenario.name(),
            scenario.shape()
        );
        let (block, operations) = config.work(scenario);
        let predicted = predicted_mean_depth(expected, operations, depth);
        let mut ledger = Ledger::new();

        for which in Which::all() {
            if !ring_available() && which.builds_a_driver() {
                continue;
            }
            let job = Job {
                scenario,
                read_path: &read_path,
                write_path: &write_path,
                block,
                operations,
                depth,
            };
            let mut timer = Untimed { iterations: 2 };
            let record = measure_combination(
                which,
                Weakness::None,
                &config,
                &job,
                &mut ledger,
                &mut timer,
            )
            .unwrap_or_else(|f| panic!("{which:?} was rejected on {}: {f}", scenario.name()));
            let Record::Measured { achieved, .. } = record else {
                panic!("{which:?} did not measure {}: {record:?}", scenario.name());
            };
            assert_eq!(
                achieved.mean,
                predicted,
                "{which:?} on {} at depth {depth} achieved {} where {expected:?} predicts {predicted}",
                scenario.name(),
                achieved.mean,
            );
        }
    }
}

/// FR-004: the reported batching figure is a delta over one iteration, not a
/// running total for the session.
///
/// A cumulative reading would grow with the number of timed iterations and
/// would be diluted by the registered backend's buffer registration and by the
/// cancellations shutdown submits — a whole-session average answering a
/// question nobody asked. Running the same job at two iteration counts and
/// requiring the same figure is what distinguishes the two: a session total
/// cannot hold still while the session grows.
#[test]
fn the_batching_figure_is_per_iteration_not_per_session() {
    if !ring_available() {
        return;
    }
    let config = Config::small();
    let (read_path, write_path) = prepare("delta", &config);
    let scenario = Scenario::BulkRead;
    let (block, operations) = config.work(scenario);

    let mut figures = Vec::new();
    for iterations in [2, 16] {
        let mut ledger = Ledger::new();
        let job = Job {
            scenario,
            read_path: &read_path,
            write_path: &write_path,
            block,
            operations,
            depth: 4,
        };
        let mut timer = Untimed { iterations };
        let record = measure_combination(
            Which::RingPlain,
            Weakness::None,
            &config,
            &job,
            &mut ledger,
            &mut timer,
        )
        .expect("one backend against its own ledger cannot disagree");
        let Record::Measured { submitted, .. } = record else {
            panic!("the ring backend did not measure: {record:?}");
        };
        let counts = submitted.expect("a ring backend reports submission counts");
        assert!(
            counts.submissions > 0,
            "no submissions were counted over {iterations} iterations, so the figure is vacuous"
        );
        assert_eq!(
            counts.entries as usize, operations,
            "one iteration of {operations} operations covered {} entries",
            counts.entries
        );
        figures.push(counts);
    }

    assert_eq!(
        figures[0], figures[1],
        "the figure changed with the iteration count, so it is a session total rather than a \
         per-iteration delta"
    );
}

/// SC-001: at configured depth N, every ring backend records exactly N entries
/// per submission — in the rolling shape as much as the batched one.
///
/// This criterion was rewritten after measurement. It originally required the
/// batched window to record several times what a rolling window did, predicting
/// the rolling figure near 1 because a rolling refill was assumed to submit
/// about once per operation. It does not: the executor drains every ready
/// completion in one poll pass before the driver submits again, so against a
/// warm page cache a rolling refill rebuilds the whole window and one
/// submission covers all of it. Batching was already at full depth before the
/// batched shape existed.
///
/// The test covers **both** ring backends. The equivalence was first seen on the
/// plain one, and publishing it on that basis would generalise across the two
/// configurations this suite exists to distinguish.
///
/// It stays falsifiable in four distinguishable directions: a rolling figure
/// near 1 would restore the original premise; a bulk-read figure below N would
/// mean batches are not assembling whole; a figure above N would mean the delta
/// is capturing submissions from outside the measured iteration; and a figure
/// that moved between runs would contradict determinism.
#[test]
fn every_ring_backend_submits_exactly_its_depth_in_both_shapes() {
    if !ring_available() {
        return;
    }
    let config = Config::small();
    let (read_path, write_path) = prepare("equivalence", &config);
    let depth = 4;

    for scenario in Scenario::all() {
        // Write-then-read runs two phases against one trace, so entries per
        // submission is not a single window's figure and this identity does not
        // describe it.
        if scenario == Scenario::WriteThenRead {
            continue;
        }
        let (block, operations) = config.work(scenario);
        for which in [Which::RingPlain, Which::RingRegistered] {
            let mut ledger = Ledger::new();
            let job = Job {
                scenario,
                read_path: &read_path,
                write_path: &write_path,
                block,
                operations,
                depth,
            };
            let mut timer = Untimed { iterations: 2 };
            let record = measure_combination(
                which,
                Weakness::None,
                &config,
                &job,
                &mut ledger,
                &mut timer,
            )
            .expect("one backend against its own ledger cannot disagree");
            let Record::Measured { submitted, .. } = record else {
                panic!("{which:?} did not measure {}: {record:?}", scenario.name());
            };
            let counts = submitted.expect("a ring backend reports submission counts");
            assert_eq!(
                counts.entries as usize,
                operations,
                "{which:?} on {} covered {} entries for {operations} operations",
                scenario.name(),
                counts.entries
            );
            assert_eq!(
                counts.entries,
                counts.submissions * depth as u64,
                "{which:?} on {} ({:?}) recorded {} entries over {} submissions, which is {:.2} \
                 per submission rather than the configured {depth}",
                scenario.name(),
                scenario.shape(),
                counts.entries,
                counts.submissions,
                counts.entries as f64 / counts.submissions as f64,
            );
        }
    }
}
