//! What the harness measures, and with what.
//!
//! Every figure here appears in the report, because a result quoted without its
//! parameters is not a result.

use crate::concurrency::Depth;

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
        }
    }

    /// How many operations a scenario performs, given its block size.
    pub fn operations(&self, total: u64, block: u32) -> usize {
        (total / block as u64) as usize
    }
}
