//! What the harness measures, and with what.
//!
//! Every figure here appears in the report, because a result quoted without its
//! parameters is not a result.

use crate::concurrency::Depth;
use crate::scenario::Scenario;

/// Operation counts stated per scenario rather than derived from a total.
///
/// A configuration that carries these decides its shapes outright. The default
/// configuration does not: it derives them from the file sizes and block sizes
/// below, which is the rule the published figures were taken with.
#[derive(Debug, Clone, Copy)]
pub struct Operations {
    /// Operations per iteration of the sequential read scenario.
    pub sequential: usize,
    /// Operations per iteration of the random read scenario.
    pub random: usize,
    /// Operations per *direction* of the write-then-read scenario, which issues
    /// two I/Os per operation.
    pub write_then_read: usize,
}

/// The measurement parameters.
#[derive(Debug, Clone)]
pub struct Config {
    /// In-flight depths to measure at.
    pub depths: Vec<Depth>,
    /// Size of the file the read scenarios work over.
    pub read_file_bytes: u64,
    /// Block size for the sequential read scenario.
    pub sequential_block: u32,
    /// Block size for the random read scenario.
    pub random_block: u32,
    /// Total bytes the write-then-read scenario moves, in each direction.
    pub write_file_bytes: u64,
    /// Block size for the write-then-read scenario.
    pub write_block: u32,
    /// Measured repeats, after the discarded warm-up.
    pub repeats: usize,
    /// How many buffers the registered backend registers.
    ///
    /// At least the highest depth, or that backend would be bounded by its pool
    /// rather than by the configuration.
    pub registered_buffers: usize,
    /// Per-scenario operation counts, when this configuration states them
    /// explicitly rather than deriving them.
    ///
    /// `None` for [`Config::default`], which keeps the derivation the published
    /// figures were taken with.
    pub operations_per_iteration: Option<Operations>,
}

impl Default for Config {
    fn default() -> Self {
        let depths = vec![1, 8, 64];
        Self {
            registered_buffers: depths.iter().copied().max().unwrap_or(1),
            depths,
            read_file_bytes: 256 * 1024 * 1024,
            sequential_block: 64 * 1024,
            random_block: 4 * 1024,
            write_file_bytes: 64 * 1024 * 1024,
            write_block: 64 * 1024,
            repeats: 5,
            operations_per_iteration: None,
        }
    }
}

impl Config {
    /// A small configuration for tests, so they need no large files.
    pub fn small() -> Self {
        let depths = vec![1, 4];
        Self {
            registered_buffers: 4,
            depths,
            read_file_bytes: 256 * 1024,
            sequential_block: 4096,
            random_block: 512,
            write_file_bytes: 64 * 1024,
            write_block: 4096,
            repeats: 1,
            // Stated outright rather than derived, and equal to what the tests
            // computed for themselves before there was one shared rule: the
            // production rule takes the random scenario over a sixty-fourth of
            // the read file, and applying it to a 256 KiB test file would take
            // these tests from 64 random operations to 8 — few enough that a
            // depth-4 run is barely two waves. The divergence is a stated fact
            // here rather than an accident there.
            operations_per_iteration: Some(Operations {
                sequential: 64,
                random: 64,
                write_then_read: 16,
            }),
        }
    }

    /// How many operations a scenario performs, given its block size.
    pub fn operations(&self, total: u64, block: u32) -> usize {
        (total / block as u64) as usize
    }

    /// The block size and operation count for one scenario.
    ///
    /// The one place a scenario's shape is decided, shared by the measurement
    /// path and the tests — so a change to what is measured cannot reach one
    /// without reaching the other.
    pub fn shape(&self, scenario: Scenario) -> (u32, usize) {
        let block = match scenario {
            Scenario::SequentialRead => self.sequential_block,
            Scenario::RandomRead => self.random_block,
            Scenario::WriteThenRead => self.write_block,
        };
        if let Some(operations) = self.operations_per_iteration {
            let count = match scenario {
                Scenario::SequentialRead => operations.sequential,
                Scenario::RandomRead => operations.random,
                Scenario::WriteThenRead => operations.write_then_read,
            };
            return (block, count);
        }
        let total = match scenario {
            Scenario::SequentialRead => self.read_file_bytes,
            // A slice of the file: many small reads over the whole of a 256 MiB
            // file at 4 KiB each would be 65k operations per iteration.
            Scenario::RandomRead => self.read_file_bytes / 64,
            Scenario::WriteThenRead => self.write_file_bytes,
        };
        (block, self.operations(total, block))
    }
}
