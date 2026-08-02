//! Test-only helpers and a custom single-threaded async runtime used to
//! exercise the `win-ioring` crate. Nothing here is io_ring specific.

pub mod rt;
pub mod scenario;
pub mod temp;

#[allow(dead_code)]
mod sched;

/// Path to the workspace README, used as sample data by the tests.
pub const README_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../README.md");

/// Yields until the driver has nothing outstanding.
///
/// Tests that abandon operations in a loop have to wait for the ring to drain
/// before assuming there is room for another. A fixed number of yields is not
/// enough: when the whole workspace's test binaries run at once the driver task
/// can be starved for long enough that the submission queue is still full, and
/// the next operation is then legitimately rejected with a queue-full error.
///
/// This uses the workspace executor's own yield rather than Tokio's, so it
/// works under either executor and keeps this crate's library free of an async
/// runtime dependency.
///
/// Panics rather than returning if the ring never settles, so a genuine leak
/// still fails the test instead of hanging it.
pub async fn settle(handle: &win_ioring::runtime::Handle) {
    for _ in 0..1_000_000 {
        if handle.outstanding() == 0 {
            return;
        }
        rt::yield_now().await;
    }
    panic!(
        "the ring never settled: {} operations still outstanding",
        handle.outstanding()
    );
}
