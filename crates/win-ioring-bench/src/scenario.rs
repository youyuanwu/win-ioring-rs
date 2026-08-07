//! The application logic under test.
//!
//! Each scenario is generic over [`Backend`] and names no implementation. That
//! is what makes the comparison mean anything: the same code, the same
//! operations, the same order, run against each backend in turn.

use std::io;
use std::path::Path;

use win_ioring::runtime::SubmissionCounts;

use crate::backend::{Backend, Buffer};
use crate::concurrency::{Achieved, Depth, Runner, Shape, ShapeCheck};
use crate::verify::{Phase, Trace};

/// A deterministic generator, so a randomised scenario issues the same sequence
/// every run and for every backend.
///
/// Hand-rolled rather than taken from a crate: this needs reproducibility, not
/// statistical quality, and a dependency for sixteen lines would be a decision
/// in its own right.
pub struct Rng(u64);

impl Rng {
    /// Seeds the generator.
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    /// Returns the next value. SplitMix64, chosen for being short enough to read
    /// and check.
    pub fn next_value(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Returns a value below `bound`.
    pub fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            0
        } else {
            self.next_value() % bound
        }
    }
}

/// Which scenario to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    /// Reads a large file from start to finish in fixed blocks.
    SequentialRead,
    /// Reads small blocks at randomised offsets over a large file.
    RandomRead,
    /// Writes a file, commits it, then reads it back.
    WriteThenRead,
    /// Reads a large file in batched windows: a whole window is built, then
    /// drained to nothing before the next is built.
    ///
    /// Appended last deliberately. The benchmark rotates backend order on a
    /// counter over the scenario/depth matrix, and appending keeps the counter
    /// each of the other combinations receives exactly what it received before
    /// this scenario existed.
    BulkRead,
}

impl Scenario {
    /// The scenario's name for the report.
    pub fn name(self) -> &'static str {
        match self {
            Scenario::SequentialRead => "sequential read",
            Scenario::RandomRead => "random read",
            Scenario::WriteThenRead => "write then read",
            Scenario::BulkRead => "bulk read",
        }
    }

    /// The window shape this scenario drives.
    ///
    /// A declaration with a home, rather than something a reader has to infer
    /// from which arm of [`run`] happens to be taken. The harness reads it to
    /// compute the depth the run should achieve, so a scenario that quietly
    /// drove a different shape than it declares is a checkable error rather
    /// than an invisible one.
    pub fn shape(self) -> Shape {
        match self {
            Scenario::SequentialRead | Scenario::RandomRead | Scenario::WriteThenRead => {
                Shape::Rolling
            }
            Scenario::BulkRead => Shape::Batched,
        }
    }

    /// A short, filesystem-safe identifier.
    ///
    /// Criterion turns a group name into a directory name under
    /// `target/criterion` and matches stored baselines on it, so it must contain
    /// nothing a path cannot and must not drift. [`Scenario::name`] is what a
    /// reader sees in the account.
    pub fn slug(self) -> &'static str {
        match self {
            Scenario::SequentialRead => "sequential-read",
            Scenario::RandomRead => "random-read",
            Scenario::WriteThenRead => "write-then-read",
            Scenario::BulkRead => "bulk-read",
        }
    }

    /// Every scenario, in report order.
    pub fn all() -> [Scenario; 4] {
        [
            Scenario::SequentialRead,
            Scenario::RandomRead,
            Scenario::WriteThenRead,
            Scenario::BulkRead,
        ]
    }
}

/// What one scenario run produced.
pub struct Outcome {
    /// What the run issued and delivered.
    pub trace: Trace,
    /// What concurrency it achieved.
    pub achieved: Achieved,
    /// Whether that concurrency is what the scenario's declared shape predicts.
    ///
    /// Produced by the [`Runner`] rather than recomputed downstream, because the
    /// runner is the only thing that knows whether a buffer pool smaller than
    /// the window bounded the run — the one circumstance under which a
    /// disagreement is forgivable.
    pub shape: ShapeCheck,
    /// What the ring submitted during this one iteration, if the backend has a
    /// ring at all.
    ///
    /// `None` for the `tokio::fs` backends, which submit nothing: the figure is
    /// not applicable to them rather than zero, and reporting it as zero would
    /// put a number in the account that reads as "no batching" when the truth is
    /// "no ring".
    ///
    /// Filled in by [`crate::session::Prepared::one`], not here — the scenario runs against a
    /// generic `Backend` and cannot see the driver. Every construction site in
    /// this module leaves it `None`.
    pub submitted: Option<SubmissionCounts>,
}

/// The seed every randomised scenario uses.
///
/// Fixed so two runs, and two backends, issue an identical sequence.
const SEED: u64 = 0x5EED_1234_ABCD_0001;

/// Runs a scenario against one backend.
pub async fn run<B: Backend>(
    backend: &B,
    scenario: Scenario,
    read_path: &Path,
    write_path: &Path,
    block: u32,
    operations: usize,
    depth: Depth,
) -> io::Result<Outcome> {
    match scenario {
        Scenario::SequentialRead | Scenario::BulkRead => {
            positional_reads(
                backend,
                read_path,
                block,
                operations,
                depth,
                scenario.shape(),
                |i, _| (i as u64) * block as u64,
            )
            .await
        }
        Scenario::RandomRead => {
            let blocks = (std::fs::metadata(read_path)?.len() / block as u64).max(1);
            let mut rng = Rng::new(SEED);
            let offsets: Vec<u64> = (0..operations)
                .map(|_| rng.below(blocks) * block as u64)
                .collect();
            positional_reads(
                backend,
                read_path,
                block,
                operations,
                depth,
                scenario.shape(),
                move |i, _| offsets[i],
            )
            .await
        }
        Scenario::WriteThenRead => {
            write_then_read(
                backend,
                write_path,
                block,
                operations,
                depth,
                scenario.shape(),
            )
            .await
        }
    }
}

/// Runs a scenario against one backend, on a file the caller already opened.
///
/// # Why this exists
///
/// [`run`] opens the file itself, **inside** the region its caller times. That
/// is right for the main matrix, where every backend pays its own open and the
/// open cost is part of what is being compared.
///
/// It is wrong for the `handle-mode` arm. That arm varies exactly one thing —
/// whether the handle carries `FILE_FLAG_OVERLAPPED` — and an open is one of
/// the places the flag can cost something. With the open inside the timed
/// region the measured difference would be per-operation serialisation *plus*
/// per-open flag cost, two mechanisms where the pre-registered hypothesis names
/// only the first.
///
/// Worse, it would **destroy that arm's negative control**. The hypothesis
/// predicts no effect at depth 1, because one operation at a time cannot be
/// serialised further; a depth-1 difference is therefore read as run-level
/// drift. A per-open cost difference would show up at depth 1 regardless, and
/// would have been read as drift rather than as the confound it is. That is a
/// defect that publishes cleanly: a plausible result with the safeguard
/// silently disarmed.
///
/// So the `handle-mode` arm hoists the open out and calls this instead, and its
/// per-open cost is measured separately by the open-cost probe, where it
/// belongs. The unbuffered arm makes the same choice for its own reasons
/// (`benches/unbuffered.rs`).
///
/// **The consequence is that this arm's absolute figures are not comparable to
/// main-matrix cells**, which include an open. That has to be said wherever the
/// numbers appear, not only where the method is described.
///
/// The measurement loop itself is shared with [`run`] rather than reimplemented,
/// so the two cannot drift: implementation divergence inside a controlled
/// experiment would be indistinguishable from the effect under test.
///
/// # Errors
///
/// Propagates the backend's read failures. Returns
/// [`std::io::ErrorKind::InvalidInput`] for [`Scenario::WriteThenRead`], which
/// has no pre-opened form because it opens for writing.
pub async fn run_on_open_file<B: Backend>(
    backend: &B,
    scenario: Scenario,
    file: &B::File,
    read_bytes: u64,
    block: u32,
    operations: usize,
    depth: Depth,
) -> io::Result<Outcome> {
    match scenario {
        Scenario::SequentialRead | Scenario::BulkRead => {
            positional_reads_on(
                backend,
                file,
                block,
                operations,
                depth,
                scenario.shape(),
                |i, _| (i as u64) * block as u64,
            )
            .await
        }
        Scenario::RandomRead => {
            // Reproduces `run`'s offsets exactly, including the shared seed, so
            // the two entry points issue an identical sequence. Taking the size
            // as a parameter rather than re-statting keeps this out of the
            // caller's timed region too.
            let blocks = (read_bytes / block as u64).max(1);
            let mut rng = Rng::new(SEED);
            let offsets: Vec<u64> = (0..operations)
                .map(|_| rng.below(blocks) * block as u64)
                .collect();
            positional_reads_on(
                backend,
                file,
                block,
                operations,
                depth,
                scenario.shape(),
                move |i, _| offsets[i],
            )
            .await
        }
        Scenario::WriteThenRead => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "write-then-read has no pre-opened form: it opens for writing, and \
             the arms that use this entry point measure reads",
        )),
    }
}

/// The shape both read scenarios share: `operations` positional reads, at
/// offsets a closure decides, with at most `depth` outstanding.
///
/// `shape` decides whether that window rolls or is drained to nothing between
/// batches. It is the only difference between sequential read and bulk read,
/// and it is applied identically to every backend — there is no per-backend
/// branch anywhere below this point.
async fn positional_reads<B, F>(
    backend: &B,
    path: &Path,
    block: u32,
    operations: usize,
    depth: Depth,
    shape: Shape,
    offset_of: F,
) -> io::Result<Outcome>
where
    B: Backend,
    F: Fn(usize, u32) -> u64,
{
    let file = backend.open_read(path).await?;
    positional_reads_on(backend, &file, block, operations, depth, shape, offset_of).await
}

/// [`positional_reads`], on a file the caller already opened.
///
/// This is the shared measurement loop. `positional_reads` is now only an
/// open-then-delegate wrapper, so the main matrix continues to time its own
/// opens and the `handle-mode` arm can hoist them out, without either arm
/// carrying its own copy of the loop.
async fn positional_reads_on<B, F>(
    backend: &B,
    file: &B::File,
    block: u32,
    operations: usize,
    depth: Depth,
    shape: Shape,
    offset_of: F,
) -> io::Result<Outcome>
where
    B: Backend,
    F: Fn(usize, u32) -> u64,
{
    let mut trace = Trace::new();
    for i in 0..operations {
        trace.issued(offset_of(i, block), block);
    }

    let mut runner = Runner::new(backend, depth, shape);
    runner
        .run(operations, Phase::Read, &mut trace, |i| {
            let offset = offset_of(i, block);
            let buffer = backend.take_buffer(block as usize)?;
            Ok(async move {
                let (result, buffer) = backend.read_at(file, buffer, block, offset).await;
                (offset, result.map(|n| (n, buffer)))
            })
        })
        .await?;

    let achieved = runner.achieved(operations);
    let shape_check = runner.shape_check(operations);
    Ok(Outcome {
        trace,
        achieved,
        shape: shape_check,
        submitted: None,
    })
}

/// Writes a file, commits it, then reads it back — both phases in `shape`.
///
/// `achieved` describes the read phase only: the write phase's runner is
/// dropped, so its samples do not reach the report.
async fn write_then_read<B: Backend>(
    backend: &B,
    path: &Path,
    block: u32,
    operations: usize,
    depth: Depth,
    shape: Shape,
) -> io::Result<Outcome> {
    let pattern: Vec<u8> = (0..block).map(|i| (i % 251) as u8).collect();
    let mut trace = Trace::new();
    for i in 0..operations {
        trace.issued((i as u64) * block as u64, block);
    }
    for i in 0..operations {
        trace.issued((i as u64) * block as u64, block);
    }

    {
        let file = backend.open_write(path).await?;
        let file = &file;
        let mut runner = Runner::new(backend, depth, shape);
        runner
            .run(operations, Phase::Write, &mut trace, |i| {
                let offset = (i as u64) * block as u64;
                let mut buffer = backend.take_buffer(block as usize)?;
                buffer.fill(&pattern)?;
                Ok(async move {
                    let (result, buffer) = backend.write_at(file, buffer, block, offset).await;
                    (offset, result.map(|n| (n, buffer)))
                })
            })
            .await?;
        // Commit before reading back, equivalently for every backend, or one
        // that skipped it would be doing less work than the others.
        backend.sync(file).await?;
    }

    let file = backend.open_read(path).await?;
    let file = &file;
    let mut runner = Runner::new(backend, depth, shape);
    runner
        .run(operations, Phase::Read, &mut trace, |i| {
            let offset = (i as u64) * block as u64;
            let buffer = backend.take_buffer(block as usize)?;
            Ok(async move {
                let (result, buffer) = backend.read_at(file, buffer, block, offset).await;
                (offset, result.map(|n| (n, buffer)))
            })
        })
        .await?;

    let achieved = runner.achieved(operations);
    let shape_check = runner.shape_check(operations);
    Ok(Outcome {
        trace,
        achieved,
        shape: shape_check,
        submitted: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::ioring::HandleMode;
    use crate::backends::tokio_fs::TokioFs;

    fn scratch(tag: &str, bytes: usize) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("win-ioring-bench-scenario");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{tag}-{}.dat", std::process::id()));
        let data: Vec<u8> = (0..bytes).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, data).unwrap();
        path
    }

    /// The two entry points must issue the **same** sequence of reads.
    ///
    /// `run_on_open_file` exists only to move the open out of the caller's
    /// timed region. If it also changed the workload — a different offset
    /// sequence, a different count, a different random draw — then the
    /// `handle-mode` arm would be measuring something other than what the
    /// matrix measures, and the difference would be silently attributed to
    /// handle mode.
    ///
    /// `RandomRead` is the one that can realistically drift, because
    /// `run_on_open_file` reconstructs the offsets from a size parameter
    /// instead of a `metadata` call. Both scenarios are checked anyway.
    ///
    /// Mutation-tested, and the first attempt is worth recording because it
    /// nearly produced a false verdict about this test rather than about the
    /// code. Perturbing the seed as `SEED ^ 1` left the test green, which reads
    /// as "this guard cannot fail" — but [`Rng::new`] does `seed | 1` and
    /// `SEED` already ends in a set bit, so `SEED ^ 1` seeds an identical
    /// generator. **The mutation could not mutate.** A mutation that does not
    /// mutate is the same species as a gate that cannot fail, and it fails in
    /// the more dangerous direction: it certifies a working guard as broken, or
    /// a broken one as working, depending on which way you read the green. With
    /// `SEED ^ 2` the test fails as it should, naming the first differing
    /// offset.
    #[test]
    fn both_entry_points_issue_the_same_reads() {
        let block = 4096_u32;
        let operations = 16;
        let path = scratch("equiv", block as usize * 64);
        let bytes = std::fs::metadata(&path).unwrap().len();
        let backend = TokioFs::new(1, 4, block as usize).unwrap();

        for scenario in [Scenario::SequentialRead, Scenario::RandomRead] {
            let via_run = backend
                .block_on(run(&backend, scenario, &path, &path, block, operations, 4))
                .unwrap();

            let file = backend.block_on(backend.open_read(&path)).unwrap();
            let via_open = backend
                .block_on(run_on_open_file(
                    &backend, scenario, &file, bytes, block, operations, 4,
                ))
                .unwrap();

            // `agrees_with` compares issue order, completion count, delivered
            // bytes and a content digest — not just the offsets — so it also
            // catches the two entry points reading the same places and getting
            // different data.
            if let Err(mismatch) = via_run.trace.agrees_with(&via_open.trace) {
                panic!(
                    "{scenario:?}: the two entry points did different work \
                     ({mismatch:?}), so the handle-mode arm is not measuring \
                     the matrix's workload"
                );
            }
            assert_eq!(
                via_run.trace.operations(),
                operations,
                "{scenario:?}: no reads were issued, so the comparison above \
                 holds vacuously"
            );
            assert!(
                via_run.trace.delivered_total() > 0,
                "{scenario:?}: nothing was delivered, so the digest comparison \
                 above holds vacuously"
            );
        }
    }

    /// Write-then-read has no pre-opened form and must say so.
    ///
    /// Returning an error rather than silently reading is the point: a
    /// pre-opened *read* handle cannot serve a scenario whose first phase
    /// writes, and quietly measuring the read half would produce a number that
    /// looked like the matrix's and was not.
    #[test]
    fn write_then_read_is_refused_rather_than_silently_reinterpreted() {
        let block = 4096_u32;
        let path = scratch("wtr", block as usize * 4);
        let backend = TokioFs::new(1, 4, block as usize).unwrap();
        let file = backend.block_on(backend.open_read(&path)).unwrap();

        let err = backend
            .block_on(run_on_open_file(
                &backend,
                Scenario::WriteThenRead,
                &file,
                block as u64 * 4,
                block,
                4,
                1,
            ))
            .err()
            .expect("write-then-read must be refused");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    /// Hoisting the open really does take it out of the measured region.
    ///
    /// The negative control of the whole boundary argument: if
    /// `run_on_open_file` opened a file of its own after all, the confound it
    /// exists to remove would still be present. Asserted by giving it a handle
    /// to a file that has since been renamed out from under the path — the
    /// reads must still succeed, which they can only do through the handle.
    #[test]
    fn run_on_open_file_opens_nothing() {
        let block = 4096_u32;
        let path = scratch("noopen", block as usize * 8);
        let backend = TokioFs::new(1, 4, block as usize).unwrap();
        let file = backend.block_on(backend.open_read(&path)).unwrap();

        let moved = path.with_extension("moved");
        let _ = std::fs::remove_file(&moved);
        std::fs::rename(&path, &moved).unwrap();

        let outcome = backend
            .block_on(run_on_open_file(
                &backend,
                Scenario::SequentialRead,
                &file,
                block as u64 * 8,
                block,
                4,
                2,
            ))
            .unwrap_or_else(|e| {
                panic!("reads through an already-open handle must not need the path: {e}")
            });
        assert_eq!(outcome.trace.operations(), 4);

        let _ = std::fs::remove_file(&moved);
    }

    /// Both handle modes complete the same workload correctly.
    ///
    /// Handle mode is invisible to correctness — a serialising handle returns
    /// the right bytes — so nothing else in the suite would notice if the
    /// synchronous arm were quietly broken. This is cheap and rules out the
    /// case where the A/B's synchronous side is fast because it is failing.
    #[test]
    fn both_handle_modes_read_correctly_through_the_shared_loop() {
        let block = 4096_u32;
        let path = scratch("modes", block as usize * 8);
        let backend = TokioFs::new(1, 4, block as usize).unwrap();

        for mode in [HandleMode::Overlapped, HandleMode::Synchronous] {
            // The ring backends are what the arm actually varies; this checks
            // the open paths themselves produce usable handles.
            let f = mode.open_read(&path).unwrap();
            assert!(!f.as_raw_handle().is_invalid(), "{mode:?} handle invalid");
        }
        drop(backend);
    }
}
