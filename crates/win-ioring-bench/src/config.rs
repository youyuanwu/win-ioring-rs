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
    /// The one place a scenario's work is decided, shared by the benchmark and
    /// the tests — so a change to what is measured cannot reach one without
    /// reaching the other. Named `work` rather than `shape` because
    /// [`Scenario::shape`] is a different concept a file away: this is how much
    /// I/O to do, that is the window shape to do it in.
    ///
    /// # The floor these counts must clear
    ///
    /// `operations >= 4 × max(depths)` for the read scenarios, and
    /// `>= 2 × max(depths)` for write-then-read, which runs two phases of that
    /// many. Below it an iteration at the highest depth is one or two waves of
    /// issue-and-drain, and the ramp dominates the steady state the comparison
    /// is about. `the_operation_counts_clear_the_depth_floor` holds both
    /// configurations to it.
    pub fn work(&self, scenario: Scenario) -> (u32, usize) {
        let block = match scenario {
            Scenario::SequentialRead | Scenario::BulkRead => self.sequential_block,
            Scenario::RandomRead => self.random_block,
            Scenario::WriteThenRead => self.write_block,
        };
        let count = match scenario {
            Scenario::SequentialRead | Scenario::BulkRead => {
                self.operations_per_iteration.sequential
            }
            Scenario::RandomRead => self.operations_per_iteration.random,
            Scenario::WriteThenRead => self.operations_per_iteration.write_then_read,
        };
        (block, count)
    }

    /// The depths one scenario is benchmarked at.
    ///
    /// Bulk read is benchmarked at the highest depth only. It was added to make
    /// submission batching happen; measurement showed batching was already
    /// happening at full depth in every rolling scenario, so the cells whose
    /// batching result is now exactly predictable do not earn their share of the
    /// run-time budget. What bulk read still measures uniquely is the
    /// drain-to-zero tail and the different depth profile that follows from it,
    /// and one depth demonstrates that.
    ///
    /// This is a statement about which cells the *matrix* measures, not an
    /// invariant of the runner: the batched shape is well defined at every depth
    /// and `tests/comparison.rs` legitimately drives every scenario at depth 1.
    pub fn depths_for(&self, scenario: Scenario) -> Vec<usize> {
        match scenario {
            Scenario::BulkRead => self.depths.iter().copied().max().into_iter().collect(),
            _ => self.depths.clone(),
        }
    }

    /// How large the write-then-read scenario's file ends up.
    ///
    /// Derived rather than configured: the scenario writes `operations` blocks
    /// from offset zero and truncates on open, so one iteration's bytes *are*
    /// the file's size. A separately configured size could disagree with what
    /// the scenario actually writes, and SC-015 is checked against this figure.
    pub fn write_file_bytes(&self) -> u64 {
        let (block, operations) = self.work(Scenario::WriteThenRead);
        block as u64 * operations as u64
    }

    /// How many bytes one iteration of `scenario` moves.
    ///
    /// Write-then-read counts twice, because it writes that many bytes and then
    /// reads the same bytes back — two I/Os per nominal operation.
    pub fn touched_bytes(&self, scenario: Scenario) -> u64 {
        let (block, operations) = self.work(scenario);
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
            for scenario in Scenario::all() {
                let deepest = config
                    .depths_for(scenario)
                    .into_iter()
                    .max()
                    .expect("a depth");
                let (_, operations) = config.work(scenario);
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

    /// Bulk read's depth list is what makes its predicted mean depth exactly
    /// `(N+1)/2`. A count that is not a whole multiple of every depth leaves a
    /// partial final batch, which lowers the mean and turns an exact check into
    /// an approximate one without anything announcing it.
    #[test]
    fn the_bulk_read_count_is_a_whole_multiple_of_every_depth_it_runs_at() {
        for (label, config) in [("default", Config::default()), ("small", Config::small())] {
            let (_, operations) = config.work(Scenario::BulkRead);
            for depth in config.depths_for(Scenario::BulkRead) {
                assert_eq!(
                    operations % depth,
                    0,
                    "{label}: bulk read's {operations} operations leave a partial batch at depth {depth}"
                );
            }
        }
    }

    /// Bulk read is benchmarked at one depth: the deepest the run configures.
    ///
    /// Not a free choice — the batching result at every other depth is now
    /// exactly predictable, so the cells would spend the run-time budget to
    /// confirm arithmetic. Depth 1 in particular would be a batched window one
    /// operation wide, which is a rolling window one operation wide.
    #[test]
    fn bulk_read_is_benchmarked_at_the_deepest_depth_only() {
        for (label, config) in [("default", Config::default()), ("small", Config::small())] {
            let depths = config.depths_for(Scenario::BulkRead);
            let deepest = config.depths.iter().copied().max().expect("a depth");
            assert_eq!(
                depths,
                vec![deepest],
                "{label}: bulk read's benchmark depths should be exactly the deepest"
            );
            for scenario in Scenario::all() {
                if scenario != Scenario::BulkRead {
                    assert_eq!(
                        config.depths_for(scenario),
                        config.depths,
                        "{label}: {} lost a depth",
                        scenario.name()
                    );
                }
            }
        }
    }

    /// Bulk read's block size and operation count come from the resolved
    /// configuration, like every other scenario's, rather than from constants
    /// baked into the scenario. A hard-coded count would ignore
    /// [`Config::small`] and make the benchmark's test mode measure the large
    /// configuration.
    #[test]
    fn bulk_read_draws_its_parameters_from_the_configuration() {
        let default = Config::default();
        let small = Config::small();
        assert_ne!(
            default.work(Scenario::BulkRead),
            small.work(Scenario::BulkRead),
            "bulk read reports the same work for two different configurations"
        );
        assert_eq!(
            default.work(Scenario::BulkRead),
            (
                default.sequential_block,
                default.operations_per_iteration.sequential
            ),
        );
        assert_eq!(
            small.work(Scenario::BulkRead),
            (
                small.sequential_block,
                small.operations_per_iteration.sequential
            ),
        );
    }

    /// The write file's size is what the scenario writes, not a separate number
    /// that could disagree with it.
    #[test]
    fn the_write_file_is_one_iterations_worth() {
        let config = Config::default();
        let (block, operations) = config.work(Scenario::WriteThenRead);
        assert_eq!(config.write_file_bytes(), block as u64 * operations as u64);
        assert_eq!(
            config.touched_bytes(Scenario::WriteThenRead),
            config.write_file_bytes() * 2,
            "write-then-read moves its bytes twice"
        );
    }
}
