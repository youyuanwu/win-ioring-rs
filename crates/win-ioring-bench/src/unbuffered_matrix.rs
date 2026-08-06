//! The unbuffered arm's grid, its budget projection, and its measured region.
//!
//! Separate from [`crate::harness`] on purpose. `harness::measure_combination`
//! is typed on `harness::Which`, and generalising it would put this arm's
//! variants into `slug()` and into the position-balance invariant that governs
//! the warm-cache matrix — changing code inside the buffered arms' timed path,
//! which R8 forbids after those figures were published. So this is a parallel
//! runner rather than a generalisation, and the buffered arms are untouched.
//!
//! # Where the timer starts
//!
//! Handles are opened **outside** the timed region, uniformly across all six
//! configurations, and their cost is reported separately by
//! [`crate::unbuffered::open_cost`]. See the header of [`crate::unbuffered`]
//! for the full argument and for the retraction of the figures that were first
//! used to justify it.
//!
//! Buffers are also taken and returned outside the per-operation path where the
//! backend allows it, so no configuration pays an allocation the others do not.
//!
//! # What is covered by `cargo test`
//!
//! The grid arithmetic, the budget projection, the offset generator's alignment
//! and bounds, and the small-configuration end-to-end run. Not covered: the
//! wall-clock cost of a real run, which is what the opt-in target measures.

use std::time::Duration;

use crate::account::{Budget, UNBUFFERED_RUN_BUDGET};
use crate::scenario::Scenario;
use crate::unbuffered::Config;

/// The depths this arm measures.
///
/// Three, not the buffered arm's set. Depth 1 is the floor that shows what a
/// single outstanding request costs; 64 is where the hypothesis under test
/// predicts the ring should pay; 8 is between them so a reader can see whether
/// the trend is monotone rather than inferring a line from two points.
pub const DEPTHS: [usize; 3] = [1, 8, 64];

/// The scenarios this arm measures.
///
/// Random read is the primary probe. Sequential read is included because its
/// *absence* would be a choice worth questioning — but it is the weaker probe
/// here, since a drive's own readahead can serve sequential unbuffered reads
/// from its internal cache regardless of what the OS page cache is doing.
pub const SCENARIOS: [Scenario; 2] = [Scenario::SequentialRead, Scenario::RandomRead];

/// The Criterion group name for a scenario in this arm.
///
/// Prefixed, and the prefix is load-bearing rather than cosmetic. Criterion
/// keys its stored state on `target/criterion/<group>/<id>/`, and the warm-cache
/// target already owns the groups named by [`Scenario::slug`] alone. Sharing a
/// group directory would not overwrite the leaves — the benchmark ids differ —
/// but it would put device-bound and page-cache figures under one group report,
/// where Criterion's own group summary plots them against each other. Those two
/// sets of numbers answer different questions, and a chart implying otherwise is
/// exactly the confusion this arm was separated out to avoid.
///
/// This lives in the library rather than beside its caller in
/// `benches/unbuffered.rs` for a mechanical reason: that target is
/// `harness = false`, so a `#[test]` inside it is compiled but **never
/// executed** — `cargo test --all-targets` reports success with an
/// unconditionally panicking test in the file. Guards written there are
/// type-checked scenery. See `docs/testing.md`.
#[must_use]
pub fn group_name(scenario: Scenario) -> String {
    format!("unbuffered-{}", scenario.slug())
}

/// One cell of the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    /// Which scenario.
    pub scenario: Scenario,
    /// Outstanding operations.
    pub depth: usize,
    /// Which backend configuration.
    pub config: Config,
}

/// Every cell this arm runs, in a fixed order.
#[must_use]
pub fn grid() -> Vec<Cell> {
    let mut cells = Vec::new();
    for scenario in SCENARIOS {
        for depth in DEPTHS {
            for config in Config::all() {
                cells.push(Cell {
                    scenario,
                    depth,
                    config,
                });
            }
        }
    }
    cells
}

/// The shape of one run: file size, block sizes, and operations per iteration.
#[derive(Debug, Clone, Copy)]
pub struct UnbufferedConfig {
    /// The read file's size in bytes.
    pub read_file_bytes: u64,
    /// Block size for the random-read scenario.
    pub random_block: usize,
    /// Block size for the sequential-read scenario.
    pub sequential_block: usize,
    /// Operations per timed iteration.
    pub operations: usize,
    /// Which depths to run.
    pub depths: &'static [usize],
}

impl UnbufferedConfig {
    /// The configuration a real benchmark run uses.
    ///
    /// 256 operations per iteration is R9.2: medians were stable across 128,
    /// 256 and 512 (ring at depth 64 measured 12.25 / 11.43 / 10.89 µs/IO),
    /// and the spread was worse at 128.
    ///
    /// The 256 MiB file is inherited from the buffered arm so the two are
    /// comparable in shape, but see [`crate::unbuffered`] and the published
    /// section for the residual uncertainty: a modern drive can hold a
    /// meaningful fraction of 256 MiB in its own cache, which no OS-level flag
    /// can disable.
    #[must_use]
    pub const fn full() -> Self {
        Self {
            read_file_bytes: 256 * 1024 * 1024,
            random_block: 4 * 1024,
            sequential_block: 64 * 1024,
            operations: 256,
            depths: &DEPTHS,
        }
    }

    /// A small configuration for `cargo test`.
    ///
    /// Exists so every code path in this arm is exercised by the default test
    /// run rather than only by the opt-in bench target. An opt-in target that
    /// CI never runs will rot: a refactor that breaks the alignment query, the
    /// aligned allocation, the no-buffering open or the poisoning guard would
    /// otherwise fail for the next person to run the benchmark by hand rather
    /// than in the pull request that broke it.
    ///
    /// It asserts no timing, no ordering against a wall clock, and no ratio.
    /// That is deliberate: a flaky device-bound gate is worse than no gate,
    /// because it trains people to ignore failures, and a gate people have
    /// learned to ignore looks like coverage while providing none.
    #[must_use]
    pub const fn small() -> Self {
        Self {
            read_file_bytes: 1024 * 1024,
            random_block: 4 * 1024,
            sequential_block: 64 * 1024,
            operations: 8,
            depths: &[1, 8],
        }
    }

    /// The block size this configuration uses for `scenario`.
    #[must_use]
    pub fn block(&self, scenario: Scenario) -> usize {
        match scenario {
            Scenario::SequentialRead => self.sequential_block,
            _ => self.random_block,
        }
    }

    /// How many benchmarks a run of this configuration registers.
    #[must_use]
    pub fn benchmarks(&self) -> usize {
        SCENARIOS.len() * self.depths.len() * Config::all().len()
    }

    /// The least wall-clock time a run of this configuration can take.
    ///
    /// Criterion's `measurement_time` is a floor, not a cap, so this is a lower
    /// bound no amount of shrinking the per-iteration work can beat.
    #[must_use]
    pub fn floor(&self) -> Duration {
        Budget::CHOSEN.floor(self.benchmarks())
    }

    /// Whether the projected floor fits inside the arm's budget.
    ///
    /// The half-budget convention matches `tests/comparison.rs`: the floor is
    /// required to fit in half the budget, because measured floor-to-wall
    /// multipliers on the buffered arm were 1.79x to 2.06x. Those multipliers
    /// were measured on warm-cache runs and are an analogy here, not a
    /// measurement — which is part of why [`UNBUFFERED_RUN_BUDGET`] carries
    /// more headroom than that convention alone would require.
    #[must_use]
    pub fn affordable(&self) -> bool {
        self.floor() * 2 <= UNBUFFERED_RUN_BUDGET
    }
}

/// The byte offsets one iteration reads, in issue order.
///
/// Every offset is a multiple of `align` and every read of `block` bytes lies
/// wholly inside `file_bytes`. Both are hard requirements of
/// `FILE_FLAG_NO_BUFFERING`, not preferences: a misaligned offset or a read
/// running past the end returns `ERROR_INVALID_PARAMETER` rather than behaving
/// slowly but correctly.
///
/// The random sequence is a fixed-seed LCG rather than a system RNG so that two
/// runs on the same host issue the same offsets. A benchmark whose access
/// pattern changes between runs cannot be compared with itself.
#[must_use]
pub fn offsets(
    scenario: Scenario,
    operations: usize,
    block: usize,
    align: usize,
    file_bytes: u64,
) -> Vec<u64> {
    let align = align.max(1) as u64;
    let block = block as u64;
    // The last offset at which a whole block still fits, rounded down to an
    // aligned boundary. Saturating rather than wrapping: a file smaller than one
    // block yields a single offset of 0, which the caller's own bounds check
    // then rejects rather than reading past the end.
    let span = file_bytes.saturating_sub(block) / align;
    let slots = span + 1;

    match scenario {
        Scenario::SequentialRead => (0..operations as u64)
            .map(|i| (i % slots) * align)
            .collect(),
        _ => {
            let mut state: u64 = 0x2545_F491_4F6C_DD1D;
            (0..operations)
                .map(|_| {
                    state = state
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    (state >> 16) % slots * align
                })
                .collect()
        }
    }
}

/// Issues `offsets.len()` reads at `depth` outstanding, returning the total
/// bytes delivered.
///
/// The count is returned rather than discarded so a caller can assert that the
/// work actually happened. Returning `Result<()>` would make a runner that
/// issued nothing at all indistinguishable from one that issued everything —
/// and this arm's tests exist precisely to catch a measured region that stops
/// measuring.
///
/// The handles and buffers are the caller's: both are established before this
/// is called, uniformly for every configuration, so nothing inside the timed
/// region allocates or opens. That is the whole point of the split — see the
/// header of [`crate::unbuffered`] for why the harness's convention of charging
/// opens to the timed region is the wrong one for an arm whose honest
/// competitor holds sixty-four handles.
///
/// # Errors
///
/// If any read fails or returns short. A short read is an error rather than a
/// warning: an unbuffered read that returns fewer bytes than asked did less
/// work, and less work is faster, so tolerating it would let a broken
/// configuration post the best number in the table.
pub async fn issue_reads<B: crate::backend::Backend>(
    backend: &B,
    file: &B::File,
    offsets: &[u64],
    block: usize,
) -> std::io::Result<usize> {
    use futures::stream::{FuturesUnordered, StreamExt};

    let mut in_flight = FuturesUnordered::new();
    let mut next = 0usize;
    let mut delivered = 0usize;

    // The pool's size is what bounds concurrency: `take_buffer` refuses when the
    // pool is dry rather than allocating, so the loop naturally runs at the
    // configured depth without a separate semaphore. That refusal is load
    // bearing and is guarded by a test — an earlier version allocated instead,
    // which charged the honest competitor a hidden per-operation cost.
    loop {
        while next < offsets.len() {
            match backend.take_buffer(block) {
                Ok(buffer) => {
                    let offset = offsets[next];
                    next += 1;
                    in_flight.push(async move {
                        let (result, buffer) =
                            backend.read_at(file, buffer, block as u32, offset).await;
                        (result, buffer)
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }

        match in_flight.next().await {
            Some((result, buffer)) => {
                backend.put_buffer(buffer);
                let n = result?;
                if n as usize != block {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        format!(
                            "a {block}-byte read returned {n} bytes; a short \
                                 read did less work than the measurement claims"
                        ),
                    ));
                }
                delivered += n as usize;
            }
            None if next >= offsets.len() => return Ok(delivered),
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "no operation is outstanding and no buffer is available, so \
                     the run cannot make progress",
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_grid_is_thirty_six_cells() {
        let cells = grid();
        assert_eq!(
            cells.len(),
            2 * 3 * 6,
            "2 scenarios x 3 depths x 6 configurations"
        );
        assert_eq!(cells.len(), UnbufferedConfig::full().benchmarks());
    }

    #[test]
    fn every_cell_is_distinct() {
        let cells = grid();
        let mut seen = std::collections::HashSet::new();
        for cell in &cells {
            assert!(
                seen.insert((cell.scenario.name(), cell.depth, cell.config.slug())),
                "duplicate cell {cell:?} would overwrite another's Criterion baseline"
            );
        }
        assert_eq!(seen.len(), cells.len());
    }

    /// The projected floor must fit the arm's own budget, with the same
    /// half-budget margin `tests/comparison.rs` applies to the buffered arm.
    #[test]
    fn the_full_grid_is_affordable_within_its_own_budget() {
        let config = UnbufferedConfig::full();
        assert_eq!(config.benchmarks(), 36);
        assert_eq!(config.floor(), Duration::from_secs(108));
        assert!(
            config.affordable(),
            "floor {:?} must fit twice inside {:?}",
            config.floor(),
            UNBUFFERED_RUN_BUDGET
        );
    }

    /// The unbuffered budget must not have been achieved by raising the
    /// buffered one. The 360 s budget and the 50-cell matrix are the project's
    /// primary published result and this work does not touch them.
    #[test]
    fn the_buffered_budget_is_untouched() {
        assert_eq!(crate::account::RUN_BUDGET, Duration::from_secs(360));
        assert_ne!(
            crate::account::RUN_BUDGET,
            UNBUFFERED_RUN_BUDGET,
            "the two arms must have separate budgets"
        );
    }

    #[test]
    fn every_offset_is_aligned_and_in_bounds() {
        let file = 1024 * 1024u64;
        for scenario in SCENARIOS {
            for &(block, align) in &[(4096usize, 4096usize), (65536, 4096), (4096, 512)] {
                let got = offsets(scenario, 512, block, align, file);
                assert_eq!(got.len(), 512);
                for offset in got {
                    assert!(
                        offset.is_multiple_of(align as u64),
                        "{}: offset {offset} is not a multiple of {align}; an \
                         unaligned offset fails an unbuffered read outright",
                        scenario.name()
                    );
                    assert!(
                        offset + block as u64 <= file,
                        "{}: a {block}-byte read at {offset} runs past the end of \
                         a {file}-byte file",
                        scenario.name()
                    );
                }
            }
        }
    }

    /// The random sequence must actually be random-ish, and the sequential one
    /// must actually be sequential. Without this, a generator that returned
    /// zeros for both would satisfy the alignment and bounds test above.
    #[test]
    fn the_two_scenarios_produce_different_access_patterns() {
        let file = 16 * 1024 * 1024u64;
        let seq = offsets(Scenario::SequentialRead, 64, 4096, 4096, file);
        let rand = offsets(Scenario::RandomRead, 64, 4096, 4096, file);

        assert!(
            seq.windows(2).all(|w| w[1] > w[0]),
            "the sequential scenario must issue ascending offsets"
        );
        assert_ne!(
            seq, rand,
            "the two scenarios must not issue the same offsets"
        );
        let ascending = rand.windows(2).filter(|w| w[1] > w[0]).count();
        assert!(
            ascending > 8 && ascending < 56,
            "the random scenario issued {ascending}/63 ascending steps, which is \
             not a random pattern"
        );
    }

    /// Two runs must issue identical offsets, or a benchmark cannot be compared
    /// with itself across runs.
    #[test]
    fn the_random_sequence_is_reproducible() {
        let a = offsets(Scenario::RandomRead, 128, 4096, 4096, 8 * 1024 * 1024);
        let b = offsets(Scenario::RandomRead, 128, 4096, 4096, 8 * 1024 * 1024);
        assert_eq!(a, b);
    }

    /// A file smaller than one block must not produce an out-of-bounds offset.
    #[test]
    fn a_file_smaller_than_one_block_yields_only_offset_zero() {
        for scenario in SCENARIOS {
            let got = offsets(scenario, 4, 65536, 4096, 4096);
            assert!(got.iter().all(|&o| o == 0), "{}: {got:?}", scenario.name());
        }
    }

    #[test]
    fn the_small_configuration_is_small_enough_for_ci() {
        let small = UnbufferedConfig::small();
        assert!(small.read_file_bytes <= 1024 * 1024);
        assert!(small.operations <= 8);
        assert!(small.benchmarks() < UnbufferedConfig::full().benchmarks());
    }

    /// Drives the real measured region, through the real backends, at small
    /// sizes — the coverage that keeps this opt-in target from rotting.
    ///
    /// This is the test the whole `small()` configuration exists for. It runs
    /// `issue_reads` against every one of the six configurations on a real
    /// unbuffered file, so a refactor that breaks the alignment query, the
    /// aligned allocation, the no-buffering open, the depth-bounding buffer
    /// pool or the short-read check fails in the pull request that broke it
    /// rather than the next time someone runs the benchmark by hand.
    ///
    /// It asserts no duration, no ordering against a wall clock and no ratio.
    /// A flaky device-bound gate would be worse than no gate, because it trains
    /// people to ignore failures.
    #[test]
    fn every_configuration_runs_the_measured_region_end_to_end() {
        use crate::align::Alignment;
        use crate::backend::Backend;
        use crate::unbuffered::{
            UnbufferedCompio, UnbufferedIoRing, UnbufferedIoRingRegistered, UnbufferedTokioFs,
        };
        use crate::unbuffered_workload::UnbufferedFile;
        use std::future::Future;

        /// A local copy, for the reason `unbuffered.rs`'s tests give: the
        /// buffered arms' `session::drive_while` is private and duplicating six
        /// lines is cheaper than widening that module's surface for a test.
        fn drive_while<T>(
            driver: &win_ioring::runtime::Driver,
            work: impl Future<Output = T>,
        ) -> T {
            use futures::future::Either;
            use std::pin::pin;
            futures::executor::block_on(async {
                let driving = pin!(driver.drive());
                let work = pin!(work);
                match futures::future::select(driving, work).await {
                    Either::Left(_) => {
                        panic!("the driver shut down while the work was still running")
                    }
                    Either::Right((outcome, _)) => outcome,
                }
            })
        }

        let dir = std::env::temp_dir().join("win-ioring-unbuffered-matrix-e2e");
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let alignment = Alignment::query(&dir).expect("the volume alignment");
        let align = alignment.granularity();
        let block = align;
        let depth = 4usize;

        let small = UnbufferedConfig::small();
        let file = UnbufferedFile::create(&dir, (align as u64) * 64, &alignment)
            .expect("an unbuffered working file");
        let ops = offsets(
            Scenario::RandomRead,
            small.operations,
            block,
            align,
            file.bytes(),
        );
        let path = file.path().as_raw_path();

        // The assertion that gives this test the power to fail. Without it a
        // measured region that returned immediately, having issued nothing,
        // would still report `Ok` and this test would still pass green.
        let expected = ops.len() * block;
        let check = |config: Config, got: usize| {
            assert_eq!(
                got,
                expected,
                "{} delivered {got} bytes, expected {expected}",
                config.slug()
            );
        };

        for config in Config::all() {
            match config {
                Config::IoRingPlain => {
                    let mut b =
                        UnbufferedIoRing::new(depth, depth, block, align).expect("a backend");
                    let driver = b.take_driver().expect("a driver");
                    let handle = b.handle();
                    let r = drive_while(&driver, async {
                        let file = b.open_read(path).await.expect("an unbuffered open");
                        issue_reads(&b, &file, &ops, block).await
                    });
                    handle.shutdown();
                    futures::executor::block_on(driver.drive());
                    let got = r.unwrap_or_else(|e| panic!("{}: {e}", config.slug()));
                    check(config, got);
                }
                Config::IoRingRegistered => {
                    let mut b = UnbufferedIoRingRegistered::new(depth).expect("a backend");
                    let driver = b.take_driver().expect("a driver");
                    let handle = b.handle();
                    let r = drive_while(&driver, async {
                        b.register(depth, block, align).await.expect("registration");
                        let file = b.open_read(path).await.expect("an unbuffered open");
                        issue_reads(&b, &file, &ops, block).await
                    });
                    handle.shutdown();
                    futures::executor::block_on(driver.drive());
                    let got = r.unwrap_or_else(|e| panic!("{}: {e}", config.slug()));
                    check(config, got);
                }
                Config::Compio => {
                    let b = UnbufferedCompio::new(depth, block, align).expect("a backend");
                    b.block_on(async {
                        let file = b.open_read(path).await.expect("an unbuffered open");
                        issue_reads(&b, &file, &ops, block).await
                    })
                    .map(|got| check(config, got))
                    .unwrap_or_else(|e| panic!("{}: {e}", config.slug()));
                }
                Config::TokioPool1 | Config::TokioPool512H1 | Config::TokioPool512Hn => {
                    let b = UnbufferedTokioFs::new(
                        config.pool_width().expect("a pool width"),
                        config.handles(depth),
                        depth,
                        block,
                        align,
                    )
                    .expect("a backend");
                    b.block_on(async {
                        let file = b.open_read(path).await.expect("an unbuffered open");
                        issue_reads(&b, &file, &ops, block).await
                    })
                    .map(|got| check(config, got))
                    .unwrap_or_else(|e| panic!("{}: {e}", config.slug()));
                }
            }
        }
    }

    /// The arm's Criterion group names must not be the warm-cache arm's.
    #[test]
    fn group_names_do_not_collide_with_the_warm_cache_arm() {
        let warm: Vec<&str> = Scenario::all().iter().map(|s| s.slug()).collect();
        for scenario in SCENARIOS {
            let name = group_name(scenario);
            assert!(
                !warm.contains(&name.as_str()),
                "unbuffered group {name} collides with a warm-cache group name"
            );
        }
    }

    /// The twin that proves the test above can fail.
    ///
    /// Without it, `group_names_do_not_collide_with_the_warm_cache_arm` passes
    /// for two indistinguishable reasons: because the prefix is applied, or
    /// because these scenarios never shared a name with a warm-cache one in the
    /// first place. This asserts the *unprefixed* names would all have
    /// collided, so the guard above is known to be guarding something.
    #[test]
    fn the_unprefixed_names_would_all_have_collided() {
        let warm: Vec<&str> = Scenario::all().iter().map(|s| s.slug()).collect();
        let collisions = SCENARIOS
            .iter()
            .filter(|s| warm.contains(&s.slug()))
            .count();
        assert_eq!(
            collisions,
            SCENARIOS.len(),
            "the unprefixed unbuffered scenario names no longer all collide \
             with warm-cache ones, so the prefix guard may now be passing \
             vacuously"
        );
    }

    /// Every depth a configuration names must be a depth the grid emits.
    ///
    /// `grid()` only produces [`DEPTHS`], and the runner filters cells by
    /// `depths.contains(..)`. A depth listed here but absent from `DEPTHS`
    /// therefore matches nothing and is silently dropped -- which is exactly
    /// what happened: `small()` named depth 4, the grid has no depth 4, and the
    /// CI smoke-run covered depth 1 only. Every concurrency path in the bench
    /// runner went unexercised by the target whose whole purpose is to keep
    /// them from rotting, and `benchmarks()` over-reported by 2x because it
    /// counted depths that never ran.
    #[test]
    fn every_configured_depth_is_a_depth_the_grid_emits() {
        for config in [UnbufferedConfig::full(), UnbufferedConfig::small()] {
            for depth in config.depths {
                assert!(
                    DEPTHS.contains(depth),
                    "configured depth {depth} is not in DEPTHS {DEPTHS:?}, so it \
                     matches no cell and is silently dropped"
                );
            }
        }
    }

    /// `benchmarks()` must equal what the runner actually registers.
    ///
    /// The twin for the test above: it counts cells the way the bench runner
    /// filters them, so a depth that matches nothing shows up as a mismatch
    /// rather than as a quietly smaller run.
    #[test]
    fn the_benchmark_count_matches_the_cells_that_survive_filtering() {
        for config in [UnbufferedConfig::full(), UnbufferedConfig::small()] {
            let surviving = grid()
                .iter()
                .filter(|cell| config.depths.contains(&cell.depth))
                .count();
            assert_eq!(
                surviving,
                config.benchmarks(),
                "benchmarks() disagrees with the number of cells the runner \
                 would actually register"
            );
        }
    }

    /// A backend that performs no I/O and records what it was asked to read.
    ///
    /// Exists to bind [`issue_reads`] to the offset sequence it is given.
    /// Without it the two halves are tested only in isolation: `offsets()` is
    /// checked for alignment, bounds and reproducibility, and `issue_reads` is
    /// checked for the byte count it returns -- but nothing asserted that the
    /// measured region reads *those* offsets. Replacing `offsets[next]` with
    /// `offsets[0]` left the whole suite green.
    ///
    /// That mutation is not a harmless one. Random read is this arm's primary
    /// probe precisely because scattered access defeats the drive's own cache;
    /// collapsing all 256 reads onto a single block would turn every one of
    /// them into a cache hit and report a plausible, badly wrong number rather
    /// than failing. It is the "believable number, not an obvious failure"
    /// species this feature documented in `docs/testing.md`.
    struct Recorder {
        seen: std::cell::RefCell<Vec<u64>>,
        block: usize,
    }

    impl crate::backend::Backend for Recorder {
        type Buf = Vec<u8>;
        type File = ();

        fn name(&self) -> String {
            "recorder".into()
        }
        fn configuration(&self) -> String {
            "records offsets, performs no I/O".into()
        }
        async fn open_read(&self, _path: &std::path::Path) -> std::io::Result<()> {
            Ok(())
        }
        async fn open_write(&self, _path: &std::path::Path) -> std::io::Result<()> {
            Ok(())
        }
        fn take_buffer(&self, capacity: usize) -> std::io::Result<Vec<u8>> {
            Ok(vec![0u8; capacity])
        }
        fn put_buffer(&self, _buffer: Vec<u8>) {}
        async fn read_at(
            &self,
            _file: &(),
            buffer: Vec<u8>,
            len: u32,
            offset: u64,
        ) -> (std::io::Result<u32>, Vec<u8>) {
            self.seen.borrow_mut().push(offset);
            let _ = len;
            (Ok(self.block as u32), buffer)
        }
        async fn write_at(
            &self,
            _file: &(),
            buffer: Vec<u8>,
            _len: u32,
            _offset: u64,
        ) -> (std::io::Result<u32>, Vec<u8>) {
            (
                Err(std::io::Error::other("the recorder does not write")),
                buffer,
            )
        }
        async fn sync(&self, _file: &()) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The measured region must read the offsets it was handed, in full.
    #[test]
    fn the_measured_region_reads_every_offset_it_was_given() {
        let block = 4096usize;
        for scenario in SCENARIOS {
            let ops = offsets(scenario, 64, block, block, (block as u64) * 512);
            let backend = Recorder {
                seen: std::cell::RefCell::new(Vec::new()),
                block,
            };
            let delivered = futures::executor::block_on(issue_reads(&backend, &(), &ops, block))
                .expect("reads");

            assert_eq!(delivered, ops.len() * block);

            // Order is not asserted: the measured region completes reads out of
            // order on purpose, which is the whole point of running them
            // concurrently. The multiset is what must match.
            let mut seen = backend.seen.borrow().clone();
            let mut expected = ops.clone();
            seen.sort_unstable();
            expected.sort_unstable();
            assert_eq!(
                seen, expected,
                "{scenario:?}: the measured region did not read the offsets it \
                 was given"
            );
        }
    }
}
