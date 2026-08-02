//! SC-006: the same work, driven by two unrelated executors, must produce the
//! same result.
//!
//! Running a scenario under each executor and observing that neither panicked
//! would prove very little. These tests compare the two transcripts directly,
//! so any behavioural difference between executors shows up as a diff.

use win_ioring::io_ring::IoRing;
use win_ioring::runtime::Driver;
use win_ioring_tests::scenario::transcript;

/// Runs the scenario on Tokio's local set.
fn under_tokio(tag: &str) -> String {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    rt.block_on(local.run_until(async move {
        let ring = IoRing::builder().build().unwrap();
        let driver = Driver::new(ring).unwrap();
        let handle = driver.handle();
        let driver_task = tokio::task::spawn_local(async move { driver.drive().await });

        let out = transcript(&handle, tag).await;

        handle.shutdown();
        driver_task.await.unwrap();
        out
    }))
}

/// Runs the scenario on the workspace's own executor, which shares no code with
/// Tokio.
fn under_custom_runtime(tag: &str) -> String {
    let mut rt = win_ioring_tests::rt::Runtime::new();
    let spawner = rt.handle();
    let tag = tag.to_string();
    rt.block_on(async move {
        let ring = IoRing::builder().build().unwrap();
        let driver = Driver::new(ring).unwrap();
        let handle = driver.handle();

        // Completions are signalled from an OS thread pool thread, so the
        // driver's waker must be thread-safe even though the driver is not.
        let driver_task = spawner.spawn_thread_safe(async move { driver.drive().await });

        let out = transcript(&handle, &tag).await;

        handle.shutdown();
        driver_task.await.unwrap();
        out
    })
}

/// SC-006: the two executors' transcripts must match exactly.
#[test]
fn both_executors_produce_identical_transcripts() {
    let tokio_out = under_tokio("equiv-tokio");
    let custom_out = under_custom_runtime("equiv-custom");

    assert_eq!(
        tokio_out, custom_out,
        "the two executors disagreed:\n--- tokio ---\n{tokio_out}\n--- custom ---\n{custom_out}"
    );

    // A transcript that came out empty, or that recorded no successful
    // transfer, would compare equal while proving nothing.
    assert!(
        tokio_out.lines().count() > 10,
        "the transcript is too short to be meaningful:\n{tokio_out}"
    );
    assert!(
        tokio_out.contains("read_at: 16"),
        "the scenario did not perform its first read:\n{tokio_out}"
    );
}

/// The scenario must be deterministic under one executor before comparing two
/// is meaningful.
#[test]
fn the_scenario_is_deterministic() {
    let first = under_tokio("determinism-1");
    let second = under_tokio("determinism-2");
    assert_eq!(first, second, "the scenario is not deterministic");
}
