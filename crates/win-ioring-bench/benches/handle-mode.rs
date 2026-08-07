//! The handle-mode arm — the same reads through an overlapped handle and
//! through a synchronous one, in one run.
//!
//! ```text
//! cargo bench -p win-ioring-bench --bench handle-mode
//! cargo bench -p win-ioring-bench --bench handle-mode -- --test
//! ```
//!
//! # What it is for
//!
//! Until this arm existed, `docs/performance.md`'s main matrix compared
//! `compio`, which opens overlapped handles as an invariant, against
//! `win-ioring`, which opened synchronous ones. The document argued that the
//! difference cannot matter under a warm page cache — nothing waits, so there is
//! nothing to overlap — and that argument is plausible. It is also, in the
//! crate's own words, a mechanism argument rather than a measurement.
//!
//! This arm measures it. Both ring backends run in both handle modes, in the
//! same process, under the same rotation and the same fairness ledger as the
//! matrix. That makes it a **paired** comparison rather than a historical one,
//! which matters because the document's own repeat-run analysis shows
//! between-run drift large enough to swamp the effect being looked for. A
//! before/after across two runs on different days would have confounded the
//! variable with the drift.
//!
//! # Its numbers are not matrix cells
//!
//! Stated here, and again wherever the numbers are published, because the
//! temptation to read one against the other is strong and the two are not
//! comparable:
//!
//! - This arm **hoists its opens** out of the timed region. The matrix does not.
//!   See [`Opens`] for why that difference is forced rather than chosen.
//! - It runs its own flat depth set and its own two scenarios.
//! - It carries two configurations — the synchronous ones — that the matrix
//!   deliberately does not contain, because nobody should choose them.
//!
//! An A/B delta *within* this arm is the result. A ratio between one of its
//! cells and a matrix cell is not a result at all.
//!
//! # Why it is a separate target
//!
//! Not preference. [`crate::harness`]'s rotation asserts
//! `combinations % backends == 0` so that every backend occupies every position
//! an equal number of times, and the matrix has ten combinations over five
//! backends. A sixth matrix backend makes that `10 % 6 == 4` and the assert
//! fails. The constraint was load-bearing rather than merely survived: being
//! pushed out of the matrix bought an A/B over **both** ring backends, which is
//! what the matrix design wanted and could not afford.
//!
//! # It is still covered by CI
//!
//! `test = true` in the manifest keeps this target compiled, linted and
//! smoke-run at the small configuration by `cargo test --workspace
//! --all-targets`. `bench = false` keeps it out of a bare `cargo bench`, and on
//! its own would remove the target from `--all-targets` entirely — the
//! unbuffered target's manifest block records that trap at length.
//!
//! As there, CI asserts nothing about timing. It proves the arm still runs.

use std::path::Path;
use std::time::{Duration, Instant};

use criterion::async_executor::AsyncExecutor;
use criterion::measurement::WallTime;
use criterion::{
    BenchmarkGroup, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main,
};

use win_ioring_bench::account::Budget;
use win_ioring_bench::config::Config;
use win_ioring_bench::fairness::Ledger;
use win_ioring_bench::handle_mode::{
    Cell, HANDLE_MODE_RUN_BUDGET, SCENARIOS, affordable, benchmarks, grid, group_name,
};
use win_ioring_bench::harness::{Job, Opens, Record, Timed, Timer, measure_combination_with};
use win_ioring_bench::session::Prepared;
use win_ioring_bench::weaken::Weakness;
use win_ioring_bench::workload;

/// Whether this process is a test run rather than a benchmark run.
///
/// Identical in reasoning to the other two targets: `cargo test --workspace
/// --all-targets` runs this binary with **neither** `--bench` nor `--test`, and
/// Criterion treats that as a test run. A target that read the absence of flags
/// as a benchmark run would walk the whole grid inside the test suite.
fn test_mode() -> bool {
    let mut bench = false;
    let mut test = false;
    for arg in std::env::args() {
        match arg.as_str() {
            "--bench" => bench = true,
            "--test" => test = true,
            _ => {}
        }
    }
    !bench || test
}

/// A [`Prepared`]'s `block_on`, in the shape Criterion asks for.
///
/// A copy of `comparison.rs`'s, and deliberately only this much is copied. The
/// Criterion adapter has to live in a target because the library never sees
/// Criterion; the **measurement path** — preparation, warm-up, the read-back,
/// the fairness ledger, the first/last agreement check, teardown — is shared
/// through [`measure_combination_with`] rather than copied. Duplicating that
/// would have put an implementation divergence inside a controlled experiment,
/// where a difference between two measurement paths could be published as a
/// difference between two handle modes.
#[derive(Clone, Copy)]
struct Exec<'a>(&'a Prepared);

impl AsyncExecutor for Exec<'_> {
    fn block_on<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        self.0.block_on(future)
    }
}

/// The [`Timer`] that hands one configuration to Criterion.
struct CriterionTimer<'a, 'b> {
    /// The open group this combination's benchmarks are added to.
    group: &'a mut BenchmarkGroup<'b, WallTime>,
}

impl Timer for CriterionTimer<'_, '_> {
    fn time<F, Fut>(&mut self, timed: &Timed, prepared: &Prepared, mut one: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        self.group
            .throughput(Throughput::Elements(timed.io_count as u64));
        self.group.bench_function(
            BenchmarkId::new(timed.which.slug(), timed.depth),
            |bencher| {
                bencher.to_async(Exec(prepared)).iter(&mut one);
            },
        );
    }
}

/// Measures one cell, through the shared path.
fn run_cell(
    group: &mut BenchmarkGroup<'_, WallTime>,
    cell: Cell,
    config: &Config,
    read_path: &Path,
    write_path: &Path,
    ledger: &mut Ledger,
) {
    let (block, operations) = config.work(cell.scenario);
    let job = Job {
        scenario: cell.scenario,
        read_path,
        write_path,
        block,
        operations,
        depth: cell.depth,
    };
    let mut timer = CriterionTimer { group };
    let record = measure_combination_with(
        cell.config,
        Weakness::None,
        // The one thing this arm does differently from the matrix, and the
        // reason it needs a `_with` at all.
        Opens::Hoisted,
        config,
        &job,
        ledger,
        &mut timer,
    );

    match record {
        Ok(Record::Measured {
            name,
            configuration,
            iterations,
            timed,
            ..
        }) => {
            if !timed {
                eprintln!(
                    "NOTE {name} ({configuration}) ran no timed iteration; the \
                     warm-up's trace was verified instead"
                );
            }
            let _ = iterations;
        }
        Ok(Record::Unavailable { name, reason }) => {
            // Loud rather than silent. A missing configuration is a missing arm
            // of the A/B, and an A/B with one arm reports nothing.
            eprintln!("UNAVAILABLE {name}: {reason}");
        }
        Ok(Record::Failed {
            name,
            configuration,
            error,
        }) => {
            panic!("FAILED {name} ({configuration}): {error}");
        }
        Err(failure) => {
            // The ledger caught two configurations doing different work. In this
            // arm that is worse than in the matrix: the whole claim is that the
            // only difference between the two handle modes is the handle mode.
            panic!("FAIRNESS FAILURE: {failure}");
        }
    }
}

fn handle_mode(c: &mut Criterion) {
    let test_mode = test_mode();
    let started = Instant::now();

    assert!(
        affordable(),
        "the handle-mode grid's floor of {:?} for {} benchmarks exceeds half \
         its budget of {HANDLE_MODE_RUN_BUDGET:?}",
        Budget::CHOSEN.floor(benchmarks()),
        benchmarks()
    );

    // Its own directory in both modes, for the reason the other targets give:
    // the two configurations want working files of very different sizes at the
    // same names, and sharing a directory would make each run rebuild the
    // other's file. Keeping a separate directory from the matrix even in a
    // benchmark run also means this arm cannot disturb the matrix's `read.dat`.
    let dir = if test_mode {
        workload::data_dir().join("handle-mode-test-run")
    } else {
        workload::data_dir().join("handle-mode")
    };
    if let Err(error) = std::fs::create_dir_all(&dir) {
        eprintln!("could not create {}: {error}", dir.display());
        return;
    }

    let config = if test_mode {
        eprintln!("running the small configuration (a test run, not a benchmark run)");
        Config::small()
    } else {
        Config::default()
    };

    let read_path = dir.join("read.dat");
    let write_path = dir.join("write.dat");

    if let Err(error) = workload::ensure_file(&read_path, config.read_file_bytes) {
        eprintln!("could not prepare {}: {error}", read_path.display());
        return;
    }
    eprintln!("warming the page cache...");
    if let Err(error) = workload::warm(&read_path) {
        eprintln!("could not warm {}: {error}", read_path.display());
        return;
    }

    for scenario in SCENARIOS {
        let mut group = c.benchmark_group(group_name(scenario));
        group
            .warm_up_time(Budget::CHOSEN.warm_up)
            .measurement_time(Budget::CHOSEN.measurement)
            .sample_size(Budget::CHOSEN.sample_size);

        let mut depths: Vec<usize> = grid()
            .into_iter()
            .filter(|cell| cell.scenario == scenario)
            .map(|cell| cell.depth)
            .collect();
        depths.dedup();

        for depth in depths {
            // One ledger per (scenario, depth), shared across that combination's
            // six configurations — including across the two handle modes, which
            // is the point. A ledger per configuration would make each its own
            // reference and no disagreement could ever be reported.
            let mut ledger = Ledger::new();
            for cell in grid() {
                if cell.scenario != scenario || cell.depth != depth {
                    continue;
                }
                run_cell(
                    &mut group,
                    cell,
                    &config,
                    &read_path,
                    &write_path,
                    &mut ledger,
                );
            }
        }

        group.finish();
    }

    let elapsed = started.elapsed();
    eprintln!("handle-mode arm finished in {elapsed:?}");
    if !test_mode && elapsed > HANDLE_MODE_RUN_BUDGET {
        eprintln!(
            "NOTE the run took {elapsed:?}, over its stated budget of \
             {HANDLE_MODE_RUN_BUDGET:?}. The budget is a planning figure, not a \
             gate; report it rather than discarding the run."
        );
    }
    let _: Duration = elapsed;
}

criterion_group! {
    // The group name must differ from the target name, for the reason both
    // other targets record: `criterion_group!` generates `pub fn <name>`, so
    // reusing `handle_mode` would collide with this module's own function.
    name = handle_mode_benches;
    config = Criterion::default();
    targets = handle_mode
}
criterion_main!(handle_mode_benches);
