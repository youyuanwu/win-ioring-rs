//! The application logic under test.
//!
//! Each scenario is generic over [`Backend`] and names no implementation. That
//! is what makes the comparison mean anything: the same code, the same
//! operations, the same order, run against each backend in turn.

use std::io;
use std::path::Path;

use crate::backend::{Backend, Buffer};
use crate::concurrency::{Achieved, Depth, Runner};
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
}

impl Scenario {
    /// The scenario's name for the report.
    pub fn name(self) -> &'static str {
        match self {
            Scenario::SequentialRead => "sequential read",
            Scenario::RandomRead => "random read",
            Scenario::WriteThenRead => "write then read",
        }
    }

    /// Every scenario, in report order.
    pub fn all() -> [Scenario; 3] {
        [
            Scenario::SequentialRead,
            Scenario::RandomRead,
            Scenario::WriteThenRead,
        ]
    }
}

/// What one scenario run produced.
pub struct Outcome {
    /// What the run issued and delivered.
    pub trace: Trace,
    /// What concurrency it achieved.
    pub achieved: Achieved,
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
        Scenario::SequentialRead => {
            positional_reads(backend, read_path, block, operations, depth, |i, _| {
                (i as u64) * block as u64
            })
            .await
        }
        Scenario::RandomRead => {
            let blocks = (std::fs::metadata(read_path)?.len() / block as u64).max(1);
            let mut rng = Rng::new(SEED);
            let offsets: Vec<u64> = (0..operations)
                .map(|_| rng.below(blocks) * block as u64)
                .collect();
            positional_reads(backend, read_path, block, operations, depth, move |i, _| {
                offsets[i]
            })
            .await
        }
        Scenario::WriteThenRead => {
            write_then_read(backend, write_path, block, operations, depth).await
        }
    }
}

/// The shape both read scenarios share: `operations` positional reads, at
/// offsets a closure decides, with at most `depth` outstanding.
async fn positional_reads<B, F>(
    backend: &B,
    path: &Path,
    block: u32,
    operations: usize,
    depth: Depth,
    offset_of: F,
) -> io::Result<Outcome>
where
    B: Backend,
    F: Fn(usize, u32) -> u64,
{
    let file = backend.open_read(path)?;
    let file = &file;
    let mut trace = Trace::new();
    for i in 0..operations {
        trace.issued(offset_of(i, block), block);
    }

    let mut runner = Runner::new(backend, depth);
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
    Ok(Outcome { trace, achieved })
}

async fn write_then_read<B: Backend>(
    backend: &B,
    path: &Path,
    block: u32,
    operations: usize,
    depth: Depth,
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
        let file = backend.open_write(path)?;
        let file = &file;
        let mut runner = Runner::new(backend, depth);
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

    let file = backend.open_read(path)?;
    let file = &file;
    let mut runner = Runner::new(backend, depth);
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
    Ok(Outcome { trace, achieved })
}
