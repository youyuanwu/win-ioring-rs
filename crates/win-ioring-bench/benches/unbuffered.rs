//! The unbuffered arm — reads that reach the device.
//!
//! ```text
//! cargo bench -p win-ioring-bench --bench unbuffered
//! cargo bench -p win-ioring-bench --bench unbuffered -- --test
//! ```
//!
//! This target is **opt-in**: `bench = false` in `Cargo.toml` keeps it out of a
//! bare `cargo bench`. That is structural rather than advisory, and the reasons
//! are worth restating where they will be read.
//!
//! Every figure in `docs/performance.md`'s main matrix is warm-page-cache by
//! design, because the harness was built to measure per-operation *software*
//! overhead and device I/O would swamp it. This target measures the opposite
//! thing: reads issued with `FILE_FLAG_NO_BUFFERING`, which bypass the
//! operating system's page cache and pay real device latency. Its numbers are
//! therefore **not comparable** to that matrix — different question, different
//! variance, different sensitivity to the host. They get their own budget, their
//! own noise band, and their own section of the document.
//!
//! # It is still covered by CI
//!
//! An opt-in target that CI never runs would rot. `test = true` in the manifest
//! keeps this one compiled, linted and smoke-run by
//! `cargo test --workspace --all-targets` at the small configuration, so a
//! refactor that breaks the alignment query, the aligned allocation, the
//! no-buffering open or the buffered-poisoning guard fails in CI rather than the
//! next time somebody runs the benchmark by hand.
//!
//! What CI deliberately does **not** assert is anything about timing. Those
//! properties are device-bound and host-specific, and a flaky gate is worse than
//! no gate because it trains people to ignore failures. CI proves the arm still
//! runs; only a manual run on known hardware produces numbers.

use std::time::Instant;

use criterion::measurement::WallTime;
use criterion::{
    BenchmarkGroup, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main,
};

use win_ioring_bench::account::{Budget, UNBUFFERED_RUN_BUDGET};
use win_ioring_bench::align::Alignment;
use win_ioring_bench::aligned::AlignedBuf;
use win_ioring_bench::backend::Backend;
use win_ioring_bench::unbuffered::{
    Config, UnbufferedCompio, UnbufferedIoRing, UnbufferedIoRingRegistered, UnbufferedTokioFs,
};
use win_ioring_bench::unbuffered_matrix::{
    Cell, SCENARIOS, UnbufferedConfig, grid, group_name, issue_reads, offsets,
};
use win_ioring_bench::unbuffered_workload::UnbufferedFile;
use win_ioring_bench::workload;

/// Whether this process is a test run rather than a benchmark run.
///
/// Identical in reasoning to `benches/comparison.rs`: `cargo test
/// --workspace --all-targets` runs this binary with **neither** `--bench` nor
/// `--test`, and Criterion treats that as a test run. A target that read the
/// absence of flags as a benchmark run would build a 256 MiB working file and
/// walk the whole device-bound matrix inside the test suite.
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

/// Establishes what this host requires, and proves the buffer satisfies it.
///
/// Runs in both modes. In a test run this is the whole of the target's work,
/// and it is the part that must not be allowed to rot: if the alignment query
/// stops returning a usable answer, or `AlignedBuf` stops honouring it, every
/// unbuffered read in this arm fails with `ERROR_INVALID_PARAMETER` rather than
/// degrading to something slow but correct.
fn check_alignment(dir: &std::path::Path) -> Alignment {
    std::fs::create_dir_all(dir).expect("could not create the working directory");

    // The query takes the directory itself. It opens it with
    // `FILE_FLAG_BACKUP_SEMANTICS` for metadata only and reads no data, so it
    // cannot poison anything — which an earlier version, that wrote and deleted
    // a probe file in this very directory, could not have said.
    let alignment = Alignment::query(dir).expect("could not query the volume alignment");

    eprintln!("volume alignment: {}", alignment.describe());

    let buf = AlignedBuf::new(alignment.granularity(), alignment.granularity())
        .expect("could not allocate an aligned buffer");
    assert!(
        buf.is_aligned(),
        "an AlignedBuf built at the host's granularity is not aligned to it"
    );
    assert!(
        alignment.is_aligned(buf.capacity() as u64),
        "an AlignedBuf's capacity is not a legal unbuffered length"
    );

    alignment
}

/// Drives a ring driver alongside the work.
///
/// A local copy, for the reason the unbuffered module's tests give: the
/// buffered arms' `session::drive_while` is private and belongs to that
/// machinery.
fn drive_while<T>(
    driver: &win_ioring::runtime::Driver,
    work: impl std::future::Future<Output = T>,
) -> T {
    use futures::future::Either;
    use std::pin::pin;
    futures::executor::block_on(async {
        let driving = pin!(driver.drive());
        let work = pin!(work);
        match futures::future::select(driving, work).await {
            Either::Left(_) => panic!("the driver shut down while the work was still running"),
            Either::Right((outcome, _)) => outcome,
        }
    })
}

/// Measures one cell.
///
/// The opens happen **before** `bench_function` and are deliberately outside
/// the timed region — unlike the buffered arm, where [`Backend::open_read`]'s
/// documentation notes they sit inside it. An unbuffered open costs about the
/// same as a whole read, and the configurations here hold deliberately
/// different numbers of handles (that is the variable under test), so charging
/// opens per iteration would tax whichever configuration holds the most
/// handles. That is the multi-handle thread pool: the honest competitor. The
/// unbuffered module header records the measurements behind this, including the
/// retraction of an earlier, overstated estimate of its size.
fn run_cell(
    group: &mut BenchmarkGroup<'_, WallTime>,
    cell: Cell,
    file: &UnbufferedFile,
    alignment: &Alignment,
    config: UnbufferedConfig,
) {
    let block = config.block(cell.scenario);
    let align = alignment.granularity();
    let ops = offsets(cell.scenario, config.operations, block, align, file.bytes());
    let path = file.path().as_raw_path();
    let depth = cell.depth;
    let id = BenchmarkId::new(cell.config.slug(), depth);

    group.throughput(Throughput::Elements(ops.len() as u64));

    match cell.config {
        Config::IoRingPlain => {
            let mut b = UnbufferedIoRing::new(depth, depth, block, align).expect("a ring backend");
            let driver = b.take_driver().expect("a driver");
            let handle = b.handle();
            let file = drive_while(&driver, b.open_read(path)).expect("an unbuffered open");
            group.bench_function(id, |bencher| {
                bencher.iter_custom(|iters| {
                    drive_while(&driver, async {
                        let start = Instant::now();
                        for _ in 0..iters {
                            issue_reads(&b, &file, &ops, block).await.expect("a read");
                        }
                        start.elapsed()
                    })
                });
            });
            drop(file);
            handle.shutdown();
            futures::executor::block_on(driver.drive());
        }
        Config::IoRingRegistered => {
            let mut b = UnbufferedIoRingRegistered::new(depth).expect("a ring backend");
            let driver = b.take_driver().expect("a driver");
            let handle = b.handle();
            let file = drive_while(&driver, async {
                b.register(depth, block, align).await.expect("registration");
                b.open_read(path).await.expect("an unbuffered open")
            });
            group.bench_function(id, |bencher| {
                bencher.iter_custom(|iters| {
                    drive_while(&driver, async {
                        let start = Instant::now();
                        for _ in 0..iters {
                            issue_reads(&b, &file, &ops, block).await.expect("a read");
                        }
                        start.elapsed()
                    })
                });
            });
            drop(file);
            handle.shutdown();
            futures::executor::block_on(driver.drive());
        }
        Config::Compio => {
            let b = UnbufferedCompio::new(depth, block, align).expect("a compio backend");
            let file = b.block_on(b.open_read(path)).expect("an unbuffered open");
            group.bench_function(id, |bencher| {
                bencher.iter_custom(|iters| {
                    b.block_on(async {
                        let start = Instant::now();
                        for _ in 0..iters {
                            issue_reads(&b, &file, &ops, block).await.expect("a read");
                        }
                        start.elapsed()
                    })
                });
            });
        }
        Config::TokioPool1 | Config::TokioPool512H1 | Config::TokioPool512Hn => {
            let b = UnbufferedTokioFs::new(
                cell.config.pool_width().expect("a pool width"),
                cell.config.handles(depth),
                depth,
                block,
                align,
            )
            .expect("a thread-pool backend");
            let file = b.block_on(b.open_read(path)).expect("an unbuffered open");
            group.bench_function(id, |bencher| {
                bencher.iter_custom(|iters| {
                    b.block_on(async {
                        let start = Instant::now();
                        for _ in 0..iters {
                            issue_reads(&b, &file, &ops, block).await.expect("a read");
                        }
                        start.elapsed()
                    })
                });
            });
        }
    }
}

fn unbuffered(c: &mut Criterion) {
    let test_mode = test_mode();

    // A test run gets its own directory, for the reason `comparison.rs` gives:
    // the two configurations want working files of very different sizes at the
    // same names, and sharing a directory would make each run rebuild the
    // other's file. This arm additionally keeps its own directory even in a
    // benchmark run — its data file must never be the warm-cache arm's
    // `read.dat`, which is read buffered before every run and is therefore
    // poisoned for unbuffered use.
    let dir = if test_mode {
        workload::data_dir().join("unbuffered-test-run")
    } else {
        workload::data_dir().join("unbuffered")
    };

    let started = Instant::now();
    let alignment = check_alignment(&dir);

    if test_mode {
        eprintln!("running the small configuration (a test run, not a benchmark run)");
    }

    // The measured grid.
    let config = if test_mode {
        UnbufferedConfig::small()
    } else {
        UnbufferedConfig::full()
    };

    assert!(
        config.affordable(),
        "the unbuffered grid's floor of {:?} exceeds its budget of {UNBUFFERED_RUN_BUDGET:?}",
        config.floor()
    );

    let file = UnbufferedFile::create(&dir, config.read_file_bytes, &alignment)
        .expect("could not create the unbuffered working file");

    eprintln!(
        "unbuffered working file: {} ({} MiB)",
        file.path().as_raw_path().display(),
        file.bytes() / (1024 * 1024)
    );

    for scenario in SCENARIOS {
        let mut group = c.benchmark_group(group_name(scenario));
        group
            .warm_up_time(Budget::CHOSEN.warm_up)
            .measurement_time(Budget::CHOSEN.measurement)
            .sample_size(Budget::CHOSEN.sample_size);

        for cell in grid() {
            if cell.scenario != scenario || !config.depths.contains(&cell.depth) {
                continue;
            }
            run_cell(&mut group, cell, &file, &alignment, config);
        }

        group.finish();
    }

    eprintln!("unbuffered arm finished in {:?}", started.elapsed());
}

criterion_group! {
    // The group name must differ from the target name: `criterion_group!`
    // generates `pub fn <name>`, so naming the group `unbuffered` would collide
    // with the target's own module path. `comparison.rs` documents the same trap.
    name = unbuffered_benches;
    config = Criterion::default();
    targets = unbuffered
}
criterion_main!(unbuffered_benches);
