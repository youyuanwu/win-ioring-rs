//! Test-only helpers and a custom single-threaded async runtime used to
//! exercise the `win-ioring` crate. Nothing here is io_ring specific.

pub mod rt;
pub mod temp;

#[allow(dead_code)]
mod sched;

/// Path to the workspace README, used as sample data by the tests.
pub const README_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../README.md");
