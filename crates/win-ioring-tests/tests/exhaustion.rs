//! Which producer reports exhaustion, and under which name.
//!
//! Two independent things in this crate can run out while an operation is being
//! started, and they are not the same thing:
//!
//! - the **ring's** submission queue, reported as [`io_ring::Error::QueueFull`]
//!   and reaching a caller as `file::Error::Ring(..)`;
//! - the **runtime's** slab of operation slots, reported as
//!   `runtime::Error::TooManyOperations` and reaching a caller under that name.
//!
//! They call for different remedies -- the first is a ring built too small, the
//! second is a program with too much genuinely in flight -- so collapsing them
//! into one variant would cost a caller the ability to tell which. That is
//! cheap to do by accident, because both mean "too many", and it is exactly the
//! kind of merge that a test asserting "some exhaustion error" would wave
//! through. So this test names the variant it expects and states the one it
//! must not be.
//!
//! **The two limits are numerically identical at full size**, which is what
//! makes the pin delicate rather than obvious: the slab holds `MAX_SLOTS` =
//! 65,536 entries (`runtime/slab.rs:84,93`) and the platform's largest
//! submission queue is also 65,536. Only the *order of the two checks*
//! separates them -- the slab insert at `runtime/mod.rs:1902` runs before the
//! ring build at `:1942`, and a failed ring build hands its slot straight back
//! via `recover_buffer` -- so at full size the slab reports first, by exactly
//! one operation.
//!
//! Rather than assert a negative, this test reaches **both** producers and
//! shows they answer to different names: a deliberately small ring runs out
//! first and reports `Ring(QueueFull)` at its own capacity, while a
//! maximum-size ring lets the slab bind and reports `TooManyOperations`. A
//! merge of the two variants breaks the second case; a reversal of the two
//! checks breaks the first. Neither can be satisfied by a test that merely
//! observed *some* failure, because each names the capacity it must fail at.
//!
//! **Cost, stated rather than buried.** Exhausting the slab means 65,536
//! genuinely outstanding kernel operations, and tearing those down takes about
//! 40 seconds in a debug build (1.7 in release -- the drain is ~23x cheaper
//! optimised, so this is unoptimised-build overhead rather than a shutdown
//! defect). That is most of this file's runtime. It is paid deliberately: the
//! alternative is asserting the mapping without ever reaching the producer,
//! which would pin the arm and not the behaviour, and this work has already
//! found one criterion that was reported covered while the thing it named was
//! never exercised.

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use win_ioring::file::{self, File};
use win_ioring::io_ring::IoRing;
use win_ioring::runtime::Driver;

/// Starts operations against a ring of `queue_size` until one is refused.
///
/// Returns how many started before the refusal, and the error that refused it.
/// The driver is spawned but never gets to run during the loop: the loop is
/// synchronous, so a driver that would otherwise reap completions and free slab
/// slots is starved for as long as it takes to fill them.
async fn exhaust(queue_size: u32, path: &std::path::Path) -> (usize, file::Error) {
    let ring = IoRing::builder()
        .with_submission_queue_size(queue_size)
        .with_completion_queue_size(queue_size)
        .build()
        .expect("the ring size this test asks for must be one the platform accepts");

    let driver = Driver::new(ring).unwrap();
    let handle = Rc::new(driver.handle());
    let driver_task = tokio::task::spawn_local(async move { driver.drive().await });

    let file = File::open(path).unwrap();
    // A no-op waker, because the loop must poll each future once to see
    // whether starting it failed while never letting the driver run: a driver
    // that gets to reap completions frees slab slots as fast as the loop
    // consumes them, and the slab would never fill. The loop being synchronous
    // is what starves it.
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);

    let mut held = Vec::new();
    let mut failure = None;

    // One past the larger of the two limits, so the loop ends by running into
    // one of them rather than by running out of iterations.
    for _ in 0..=65_536u32 {
        let mut op = file.read_at(&handle, vec![0u8; 1], 1, 0);
        match Pin::new(&mut op).poll(&mut cx) {
            Poll::Pending => held.push(op),
            Poll::Ready(done) => {
                failure = Some(done.result);
                break;
            }
        }
    }

    let started = held.len();
    let failure = failure.expect(
        "some limit should have bound within 65,536 operations; if every one started, \
         neither limit is where this test believes it is",
    );
    let err = failure.expect_err("the operation past a limit must fail");

    drop(held);
    drop(file);
    handle.shutdown();
    driver_task.await.unwrap();

    (started, err)
}

fn scratch_file(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("win_ioring_exhaustion");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{tag}.bin"));
    std::fs::write(&path, vec![7u8; 4096]).unwrap();
    path
}

#[test]
fn a_full_ring_and_a_full_slab_are_reported_as_different_things() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                // A ring far smaller than the slab, so the *ring* is what binds.
                let small = scratch_file("small_ring");
                let (started, err) = exhaust(1_024, &small).await;

                assert_eq!(
                    started, 1_024,
                    "a ring of 1,024 must bind at 1,024; binding elsewhere means this \
                     case is not exercising the producer it names"
                );
                assert!(
                    matches!(err, file::Error::Ring(_)),
                    "a full submission queue must be reported as the ring's own error, \
                     got {err:?}"
                );
                assert!(
                    !matches!(err, file::Error::TooManyOperations),
                    "the kernel's queue filling is not the runtime running out of slots; \
                     reporting one as the other sends a caller after the wrong remedy"
                );

                // A ring as large as the platform allows, so the *slab* binds.
                // The two capacities are equal here, and only the order of the
                // checks decides which reports.
                let large = scratch_file("large_ring");
                let (started, err) = exhaust(65_536, &large).await;

                assert_eq!(
                    started, 65_536,
                    "with a maximum-size ring the slab must bind at its own capacity; \
                     binding earlier means the error under test is not the one named"
                );
                assert!(
                    matches!(err, file::Error::TooManyOperations),
                    "the runtime's slab running out must be reported under its own name, \
                     got {err:?}"
                );
                assert!(
                    !matches!(err, file::Error::Ring(_)),
                    "the runtime's slab and the kernel's queue are different limits with \
                     different remedies; reporting one as the other loses that"
                );
            })
            .await;
    });
}
