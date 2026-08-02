//! Running every backend over every scenario and depth.
//!
//! Kept out of the binary so the tests can drive the same code the command does.

use std::io;
use std::path::Path;
use std::time::Instant;

use crate::backend::Backend;
use crate::backends::{ioring, tokio_fs};
use crate::concurrency::Depth;
use crate::config::Config;
use crate::measure::Measured;
use crate::scenario::{self, Scenario};

/// What a single run is asked to do.
///
/// Bundled because the same six values thread through the scenario, the harness
/// and the binary, and passing them individually made every signature a wall.
#[derive(Clone)]
pub struct Job<'a> {
    /// Which scenario to run.
    pub scenario: Scenario,
    /// The file the read scenarios work over.
    pub read_path: &'a Path,
    /// The file the write scenario creates.
    pub write_path: &'a Path,
    /// The transfer size of each operation.
    pub block: u32,
    /// How many operations the scenario performs.
    pub operations: usize,
    /// How many may be outstanding at once.
    pub depth: Depth,
}

/// Runs a scenario `repeats` times against one backend, timing only the
/// measured region.
///
/// The warm-up repeat is discarded: it pays for lazily created threads,
/// first-touch page faults, and anything else a backend defers until first use.
/// Charging one backend for that and not another is the sort of unfairness that
/// is invisible in the result.
pub async fn repeats<B: Backend>(
    backend: &B,
    config: &Config,
    job: &Job<'_>,
) -> io::Result<Measured> {
    let _warm = scenario::run(
        backend,
        job.scenario,
        job.read_path,
        job.write_path,
        job.block,
        job.operations,
        job.depth,
    )
    .await?;

    let mut samples = Vec::with_capacity(config.repeats);
    let mut last = None;
    for _ in 0..config.repeats {
        let started = Instant::now();
        let outcome = scenario::run(
            backend,
            job.scenario,
            job.read_path,
            job.write_path,
            job.block,
            job.operations,
            job.depth,
        )
        .await?;
        samples.push(started.elapsed());
        last = Some(outcome);
    }

    let outcome = last.expect("at least one measured repeat");
    Ok(Measured {
        samples,
        achieved: outcome.achieved,
        trace: outcome.trace,
    })
}

/// Which backend to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    /// The thread-pool backend with a single blocking thread.
    TokioOne,
    /// The thread-pool backend at its default width.
    TokioMany,
    /// This crate, with caller-owned buffers.
    RingPlain,
    /// This crate, with registered buffers and handles.
    RingRegistered,
}

impl Which {
    /// Every backend, in a fixed order.
    pub fn all() -> [Which; 4] {
        [
            Which::TokioOne,
            Which::TokioMany,
            Which::RingPlain,
            Which::RingRegistered,
        ]
    }
}

/// What one backend's run produced, alongside how it was configured.
pub struct Run {
    /// The backend's name.
    pub name: String,
    /// Its configuration, for the report.
    pub configuration: String,
    /// The measurement, or why there is none.
    pub measured: io::Result<Measured>,
}

/// Runs one backend end to end, including its own setup and teardown.
///
/// Setup and teardown are outside every timed region, which is what lets a
/// backend that must build a ring be compared with one that need not.
pub fn run_one(which: Which, config: &Config, job: &Job<'_>) -> Run {
    let depth = job.depth;
    // Every backend pre-allocates the same number of buffers of the same size,
    // so none is charged a per-operation allocation another avoids.
    let pool = depth.max(1);
    let capacity = job.block as usize;
    match which {
        Which::TokioOne | Which::TokioMany => {
            let width = if which == Which::TokioOne { 1 } else { 512 };
            match tokio_fs::TokioFs::new(width, pool, capacity) {
                Ok(backend) => {
                    let measured = backend.block_on(repeats(&backend, config, job));
                    Run {
                        name: backend.name(),
                        configuration: backend.configuration(),
                        measured,
                    }
                }
                Err(e) => Run {
                    name: format!("tokio::fs (blocking pool {width})"),
                    configuration: "failed to build".to_owned(),
                    measured: Err(e),
                },
            }
        }
        Which::RingPlain => match ioring::IoRingPlain::new(depth, pool, capacity) {
            Ok(mut backend) => {
                let driver = backend.take_driver().expect("taken once");
                let handle = backend.handle();
                let name = backend.name();
                let configuration = backend.configuration();
                let measured = drive_local(driver, &handle, async {
                    repeats(&backend, config, job).await
                });
                Run {
                    name,
                    configuration,
                    measured,
                }
            }
            Err(e) => Run {
                name: "win-ioring (owned buffers)".to_owned(),
                configuration: "failed to build".to_owned(),
                measured: Err(e),
            },
        },
        Which::RingRegistered => match ioring::IoRingRegistered::new(depth) {
            Ok(mut backend) => {
                let driver = backend.take_driver().expect("taken once");
                let handle = backend.handle();
                let name = backend.name();
                let configuration = backend.configuration();
                let measured = drive_local(driver, &handle, async {
                    // Registration happens once, before any timed region, so
                    // none of the figures below contains its cost. That is a
                    // deliberate choice and the report says so: a registration
                    // is a one-off whose cost belongs to the decision to
                    // register, not to any single transfer.
                    backend
                        .register(config.registered_buffers, job.block as usize)
                        .await?;
                    repeats(&backend, config, job).await
                });
                Run {
                    name,
                    configuration,
                    measured,
                }
            }
            Err(e) => Run {
                name: "win-ioring (registered)".to_owned(),
                configuration: "failed to build".to_owned(),
                measured: Err(e),
            },
        },
    }
}

/// Runs `work` with the driver pumped on this thread.
///
/// The ring-backed backends are `!Send`, so their driver has to live on the
/// measuring thread. Joining the two is what keeps the driver making progress
/// while the work awaits it, and the shutdown afterwards is what lets `drive`
/// return so the join can complete.
///
/// A plain `block_on` of the joined pair rather than a spawning runtime: nothing
/// here needs a task scheduler, and a joined future avoids the `'static` bound
/// spawning would impose on borrowed state.
fn drive_local<T>(
    driver: win_ioring::runtime::Driver,
    handle: &win_ioring::runtime::Handle,
    work: impl Future<Output = io::Result<T>>,
) -> io::Result<T> {
    let result = {
        let driving = driver.drive();
        let work = async {
            let outcome = work.await;
            handle.shutdown_now();
            outcome
        };
        futures::executor::block_on(async {
            let (_, outcome) = futures::future::join(driving, work).await;
            outcome
        })
    };
    drop(driver);
    result
}
