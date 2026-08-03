//! What the benchmark measures, and with what.
//!
//! Every figure here appears in the fairness account, because a result quoted
//! without its parameters is not a result.

use crate::concurrency::Depth;
use crate::scenario::Scenario;

/// Operation counts, stated per scenario rather than derived from a total.
///
/// Derived counts were how the previous harness sized an iteration: a file size
/// divided by a block size, which made an iteration as large as the working set.
/// Under a framework that runs an iteration a hundred times or more, the two
/// decisions — how much data the scenario works over, and how much of it one
/// iteration touches — have to come apart, and only the second one moves.
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
    ///
    /// Unchanged from the previous harness, so the random scenario's offset
    /// distribution is untouched and its shortened sequence is a prefix of the
    /// old one.
    pub read_file_bytes: u64,
    /// Block size for the sequential read scenario.
    pub sequential_block: u32,
    /// Block size for the random read scenario.
    pub random_block: u32,
    /// Block size for the write-then-read scenario.
    pub write_block: u32,
    /// How many buffers the registered backend registers.
    ///
    /// At least the highest depth, or that backend would be bounded by its pool
    /// rather than by the configuration.
    pub registered_buffers: usize,
    /// How much work one iteration does.
    pub operations_per_iteration: Operations,
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
            write_block: 64 * 1024,
            // Sized to the floor `shape` documents and no larger. An iteration
            // exists to be repeated a hundred times inside a measurement window,
            // so anything above the floor buys depth realism nobody reads and
            // spends the whole run's time budget.
            operations_per_iteration: Operations {
                sequential: 256,
                random: 512,
                write_then_read: 128,
            },
        }
    }
}

impl Config {
    /// A small configuration for tests and for the benchmark's own test mode, so
    /// neither needs a large file.
    pub fn small() -> Self {
        let depths = vec![1, 4];
        Self {
            registered_buffers: 4,
            depths,
            read_file_bytes: 256 * 1024,
            sequential_block: 4096,
            random_block: 512,
            write_block: 4096,
            // Equal to what the tests computed for themselves before there was
            // one shared rule, so a change to what the benchmark measures cannot
            // silently change the shapes the tests measure.
            operations_per_iteration: Operations {
                sequential: 64,
                random: 64,
                write_then_read: 16,
            },
        }
    }

    /// The block size and operation count for one scenario.
    ///
    /// The one place a scenario's shape is decided, shared by the benchmark and
    /// the tests — so a change to what is measured cannot reach one without
    /// reaching the other.
    ///
    /// # The floor these counts must clear
    ///
    /// `operations >= 4 × max(depths)` for the read scenarios, and
    /// `>= 2 × max(depths)` for write-then-read, which runs two phases of that
    /// many. Below it an iteration at the highest depth is one or two waves of
    /// issue-and-drain, and the ramp dominates the steady state the comparison
    /// is about. `the_operation_counts_clear_the_depth_floor` holds both
    /// configurations to it.
    pub fn shape(&self, scenario: Scenario) -> (u32, usize) {
        let block = match scenario {
            Scenario::SequentialRead => self.sequential_block,
            Scenario::RandomRead => self.random_block,
            Scenario::WriteThenRead => self.write_block,
        };
        let count = match scenario {
            Scenario::SequentialRead => self.operations_per_iteration.sequential,
            Scenario::RandomRead => self.operations_per_iteration.random,
            Scenario::WriteThenRead => self.operations_per_iteration.write_then_read,
        };
        (block, count)
    }

    /// How large the write-then-read scenario's file ends up.
    ///
    /// Derived rather than configured: the scenario writes `operations` blocks
    /// from offset zero and truncates on open, so one iteration's bytes *are*
    /// the file's size. A separately configured size could disagree with what
    /// the scenario actually writes, and SC-015 is checked against this figure.
    pub fn write_file_bytes(&self) -> u64 {
        let (block, operations) = self.shape(Scenario::WriteThenRead);
        block as u64 * operations as u64
    }

    /// How many bytes one iteration of `scenario` moves.
    ///
    /// Write-then-read counts twice, because it writes that many bytes and then
    /// reads the same bytes back — two I/Os per nominal operation.
    pub fn touched_bytes(&self, scenario: Scenario) -> u64 {
        let (block, operations) = self.shape(scenario);
        let bytes = block as u64 * operations as u64;
        match scenario {
            Scenario::WriteThenRead => bytes * 2,
            _ => bytes,
        }
    }

    /// The bytes that must stay resident in the page cache for the whole run.
    ///
    /// Distinct from [`Config::touched_bytes`], and both are reported: one
    /// iteration touches a fraction of the read file, but every byte of that
    /// file is a candidate for the next iteration's random offsets, so the
    /// premise is about the whole of it.
    pub fn resident_working_set(&self) -> u64 {
        self.read_file_bytes + self.write_file_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The counts are the one thing this migration changed about *what* is
    /// measured, and shrinking one below the depth floor would quietly turn a
    /// depth-64 benchmark into a measurement of the ramp.
    #[test]
    fn the_operation_counts_clear_the_depth_floor() {
        for config in [Config::default(), Config::small()] {
            let deepest = config.depths.iter().copied().max().expect("a depth");
            for scenario in Scenario::all() {
                let (_, operations) = config.shape(scenario);
                let floor = match scenario {
                    Scenario::WriteThenRead => 2 * deepest,
                    _ => 4 * deepest,
                };
                assert!(
                    operations >= floor,
                    "{} runs {operations} operations at depth {deepest}, below the floor of {floor}",
                    scenario.name()
                );
            }
        }
    }

    /// The write file's size is what the scenario writes, not a separate number
    /// that could disagree with it.
    #[test]
    fn the_write_file_is_one_iterations_worth() {
        let config = Config::default();
        let (block, operations) = config.shape(Scenario::WriteThenRead);
        assert_eq!(config.write_file_bytes(), block as u64 * operations as u64);
        assert_eq!(
            config.touched_bytes(Scenario::WriteThenRead),
            config.write_file_bytes() * 2,
            "write-then-read moves its bytes twice"
        );
    }
}
