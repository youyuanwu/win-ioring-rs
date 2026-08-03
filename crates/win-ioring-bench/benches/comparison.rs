//! The one way to run these benchmarks.
//!
//! ```text
//! cargo bench -p win-ioring-bench
//! cargo bench -p win-ioring-bench -- --test
//! cargo bench -p win-ioring-bench -- random
//! ```
//!
//! There is no measurement logic here. Everything load-bearing — preparing a
//! backend, warming it, verifying that it did the same work as its peers, and
//! tearing it down — is in the library, behind
//! [`win_ioring_bench::harness::measure_combination`]. This file wires Criterion
//! to that seam and nothing else, so the library never depends on Criterion and
//! a reader looking for what is measured does not have to read a benchmark
//! harness to find it.
//!
//! # A filtered run still verifies everything
//!
//! Criterion's positional filter selects which benchmarks are *timed*. It does
//! not, and must not, select which combinations are prepared, warmed and
//! verified: the cross-backend fairness check is only meaningful when every
//! backend of a (scenario, depth) has put a trace in front of the same ledger,
//! and a check that silently narrowed when someone typed a filter would be worth
//! less than no check. So this target walks all thirty-six combinations whatever
//! the filter says. What a filter removes is the timing — Criterion never
//! invokes the routine closure for a filtered-out benchmark, so no timed
//! iteration runs, and the warm-up's trace is verified instead. Those
//! combinations are marked **verified but not timed** in the account, which is
//! the only case in which a verified trace did not come out of a timed region.

use std::time::{Duration, Instant};

use criterion::async_executor::AsyncExecutor;
use criterion::measurement::WallTime;
use criterion::{
    BenchmarkGroup, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main,
};

use win_ioring_bench::account::{Account, Budget, Combination, Entry};
use win_ioring_bench::backend::Availability;
use win_ioring_bench::backends::ioring;
use win_ioring_bench::config::Config;
use win_ioring_bench::fairness::Ledger;
use win_ioring_bench::harness::{Job, Record, Timed, Timer, measure_combination, rotated_order};
use win_ioring_bench::scenario::Scenario;
use win_ioring_bench::session::Prepared;
use win_ioring_bench::weaken::Weakness;
use win_ioring_bench::workload;

/// How long each benchmark is warmed up for before Criterion starts sampling.
///
/// Criterion's own default is three seconds, which thirty-six benchmarks
/// multiply well past the five-minute budget of SC-001. One second is enough
/// here because [`measure_combination`] has already run an untimed warm-up of
/// its own — outside anything Criterion sees — that pays for lazily created
/// threads and first-touch page faults.
const WARM_UP: Duration = Duration::from_secs(1);

/// How long each benchmark is sampled over.
///
/// Thirty-six benchmarks share five minutes, which leaves each about eight
/// seconds all-in; three of sampling on top of one of warm-up leaves room for
/// preparation, for the untimed warm-ups and for teardown.
const MEASUREMENT: Duration = Duration::from_secs(3);

/// How many samples Criterion gathers per estimate.
///
/// Left at Criterion's default and reported rather than set, so the account
/// says what the intervals rest on.
const SAMPLE_SIZE: usize = 100;

/// A [`Prepared`]'s `block_on`, in the shape Criterion asks for.
///
/// Criterion's [`AsyncExecutor`] is taken **by value** by
/// [`criterion::bencher::Bencher::to_async`], once per sample, and there is no
/// blanket implementation for references — so the executor has to be something
/// cheap to hand over repeatedly. A `Copy` newtype around a borrow is that, and
/// it lives here rather than in the library so the library never sees Criterion.
///
/// The delegation is not a formality: the ring backends' completions are only
/// delivered while [`Prepared::block_on`] is pumping the driver, so a
/// general-purpose executor here would hang rather than merely be slower.
#[derive(Clone, Copy)]
struct Exec<'a>(&'a Prepared);

impl AsyncExecutor for Exec<'_> {
    fn block_on<T>(&self, future: impl Future<Output = T>) -> T {
        self.0.block_on(future)
    }
}

/// The [`Timer`] that hands one combination to Criterion.
///
/// Borrows the group rather than owning it: a group borrows the `Criterion`
/// mutably for as long as it lives, so only one may be open at a time and the
/// caller keeps it across all four of a combination's backends.
struct CriterionTimer<'a, 'b> {
    /// The open group this combination's benchmark is added to.
    group: &'a mut BenchmarkGroup<'b, WallTime>,
}

impl Timer for CriterionTimer<'_, '_> {
    fn time<F, Fut>(&mut self, timed: &Timed, prepared: &Prepared, mut one: F)
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = ()>,
    {
        // Elements rather than Bytes: what this comparison is about is
        // per-operation software overhead, and a byte rate invites being quoted
        // as device throughput, which against a warm page cache it is not.
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

/// Everything that would be `main` if `criterion_main!` did not write one.
///
/// The generated `main` builds the `Criterion` from the command line, calls
/// this, and prints the final summary. There is nowhere else for a program's
/// worth of work to go, which is why this function reads like one.
fn comparison(c: &mut Criterion) {
    if let Err(code) = run(c) {
        // Criterion's generated `main` returns `()`, so an unrecoverable failure
        // leaves by this door rather than by a `?`.
        std::process::exit(code);
    }
}

/// Whether this process is running the benchmarks or merely testing that they
/// run.
///
/// Read from the process arguments rather than from the `Criterion`: the flags
/// are parsed and consumed by `configure_from_args` inside the generated `main`,
/// and `Criterion` exposes no getter for the mode it settled on.
///
/// The rule is Criterion's own, reproduced rather than approximated. Testing for
/// `--test` alone would be wrong in the case that matters most: `cargo test
/// --workspace --all-targets` runs this binary with **neither** flag, and
/// Criterion treats that as a test run. A target that read it as a benchmark run
/// would build a 256 MiB working file and walk the full matrix inside the test
/// suite.
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
    // `cargo bench` passes `--bench`; `cargo bench -- --test` passes both and
    // means a test run; anything else — `cargo test --benches` among them — is a
    // test run.
    !bench || test
}

/// The run proper, with a failure path that can return.
fn run(c: &mut Criterion) -> Result<(), i32> {
    let test_mode = test_mode();
    let config = if test_mode {
        eprintln!("running the small configuration (a test run, not a benchmark run)");
        Config::small()
    } else {
        Config::default()
    };

    let started = Instant::now();
    // A test run gets its own directory. The two configurations want working
    // files of very different sizes at the same names, and `ensure_file`
    // recreates a file whose size does not match — so sharing one directory
    // would make every `cargo test` shrink the 256 MiB read file and every
    // `cargo bench` write it out again.
    let dir = if test_mode {
        workload::data_dir().join("test-run")
    } else {
        workload::data_dir()
    };
    let read_path = dir.join("read.dat");
    let write_path = dir.join("write.dat");

    eprintln!("preparing working files under {}...", dir.display());
    if let Err(error) = workload::ensure_file(&read_path, config.read_file_bytes) {
        eprintln!("could not prepare {}: {error}", read_path.display());
        return Err(1);
    }
    eprintln!("warming the page cache...");
    if let Err(error) = workload::warm(&read_path) {
        eprintln!("could not warm {}: {error}", read_path.display());
        return Err(1);
    }
    let preparation = started.elapsed();

    // Probed once and reported, but the ring backends are still driven through
    // `measure_combination` on a host without a ring: `prepare` fails there and
    // yields `Record::Unavailable`, which is the path SC-008 is about, and
    // skipping them here would leave that path untested by the benchmark.
    if let Availability::Unavailable(reason) = ioring::availability() {
        eprintln!("no I/O ring on this host: {reason}");
    }

    let mut account = Account::new(
        config.clone(),
        Budget {
            warm_up: WARM_UP,
            measurement: MEASUREMENT,
            sample_size: SAMPLE_SIZE,
        },
        test_mode,
        dir.to_string_lossy().into_owned(),
    );
    let measurement_started = Instant::now();
    let drivers_before = ioring::drivers_built();
    let mut ring_combinations = 0_usize;
    let mut rotation = 0_usize;

    for scenario in Scenario::all() {
        let (block, operations) = config.shape(scenario);
        let mut group = c.benchmark_group(scenario.slug());
        group.warm_up_time(WARM_UP).measurement_time(MEASUREMENT);

        for &depth in &config.depths {
            let order = rotated_order(rotation);
            rotation += 1;

            // One ledger per (scenario, depth), shared across that combination's
            // backends. A ledger per backend would make every backend its own
            // reference and no disagreement could ever be reported.
            let mut ledger = Ledger::new();
            let mut entries = Vec::new();
            let mut reference = None;

            for which in order {
                let job = Job {
                    scenario,
                    read_path: &read_path,
                    write_path: &write_path,
                    block,
                    operations,
                    depth,
                };
                let mut timer = CriterionTimer { group: &mut group };
                let record = match measure_combination(
                    which,
                    Weakness::None,
                    &config,
                    &job,
                    &mut ledger,
                    &mut timer,
                ) {
                    Ok(record) => record,
                    Err(failure) => {
                        // A backend that did different work is rejected, not
                        // reported. This is the check that stops one looking
                        // fast by delivering less.
                        eprintln!("FAIRNESS FAILURE {failure}");
                        return Err(1);
                    }
                };
                if let Record::Measured { name, .. } = &record {
                    if which.builds_a_driver() {
                        ring_combinations += 1;
                    }
                    if reference.is_none() {
                        reference = Some(name.clone());
                    }
                }
                entries.push(Entry::from_record(&record));
            }

            account.record(Combination {
                scenario,
                depth,
                order: order.iter().map(|which| which.slug()).collect(),
                reference,
                entries,
            });
        }

        // Explicit rather than left to `Drop`: the next iteration opens another
        // group, and a group borrows the `Criterion` mutably for as long as it
        // lives.
        group.finish();
    }

    // The closing work sits here, after the last group has been finished,
    // because a group holds the `Criterion` borrowed and anything written inside
    // the loop would be skipped entirely by an early return from a later
    // combination.
    account.drivers_built = ioring::drivers_built() - drivers_before;
    account.ring_combinations = ring_combinations;
    account.write_file_bytes = std::fs::metadata(&write_path).ok().map(|file| file.len());
    account.measurement = measurement_started.elapsed();
    account.preparation = preparation;

    let failures = account.failures();
    for failure in &failures {
        eprintln!("BACKEND FAILURE {failure}");
    }

    match account.write(&dir) {
        Ok(path) => eprintln!("fairness account written to {}", path.display()),
        Err(error) => {
            eprintln!("could not write the fairness account: {error}");
            return Err(1);
        }
    }

    // A backend that could not complete its work is not a backend that was slow.
    if !failures.is_empty() {
        return Err(1);
    }
    Ok(())
}

criterion_group! {
    // The group's name is what `criterion_main!` calls; the target is this
    // file's `comparison`. They must differ, or the function the macro generates
    // collides with the one it is meant to call.
    name = benches;
    config = Criterion::default()
        .warm_up_time(WARM_UP)
        .measurement_time(MEASUREMENT);
    targets = comparison
}
criterion_main!(benches);
