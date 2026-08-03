//! One backend, prepared once and kept alive for the whole of one benchmark.
//!
//! # What this replaces, and why it is inside out
//!
//! The previous harness drove a ring backend through a `drive_local` that took
//! the [`Driver`] **by value**, shut it down as soon as the work finished, and
//! dropped it before returning. All three are wrong for a measurement framework
//! that calls the measured closure thousands of times:
//!
//! | Previously | Here |
//! |---|---|
//! | the driver was moved in, one per call | the driver is borrowed by every iteration and outlives them all |
//! | `shutdown_now` when the work ended | shutdown happens once, in [`Prepared::finish`] |
//! | dropped before returning | torn down after the benchmark, never between iterations |
//! | `join(drive, work)` | `select(drive, work)` |
//!
//! The last row is the one that is easy to get wrong. [`Driver::drive`] only
//! returns *after* shutdown, so joining it with work that does not shut the
//! driver down would never complete. Selecting returns as soon as the work does
//! and leaves the driver running for the next iteration. If `drive` wins the
//! select the driver shut down underneath a live iteration, which is a defect
//! rather than a slow result, so [`Prepared::block_on`] panics with that
//! message rather than reporting a time.

use std::io;
use std::pin::pin;

use futures::future::Either;
use win_ioring::runtime::{Driver, Handle};

use crate::backend::Backend;
use crate::backends::ioring;
use crate::backends::tokio_fs::TokioFs;
use crate::config::Config;
use crate::harness::{Job, Which};
use crate::scenario::{self, Outcome};
use crate::weaken::{Weakened, Weakness};

/// A backend the host could not provide, and the reason it could not.
///
/// Distinct from a measurement that failed: this one never ran at all.
pub struct Unavailable {
    /// The backend's name, which is known even though the backend is not.
    pub name: String,
    /// Why it could not be built here.
    pub reason: String,
}

/// A backend built and ready to be measured, with whatever it needs to run.
///
/// One of these per (backend, scenario, depth). Not shared across benchmarks:
/// the ring's submission queue is sized from the depth, so a shared ring would
/// be sized for the wrong one, and "one driver per measured combination" is only
/// a meaningful property if it is true by construction.
pub enum Prepared {
    /// The thread-pool backend, which brings its own runtime.
    Pool(TokioFs),
    /// The ring backend with caller-owned buffers.
    Plain {
        /// The backend the scenarios run against.
        backend: ioring::IoRingPlain,
        /// Pumped alongside every iteration, and torn down after the last.
        driver: Driver,
        /// Used once, by [`Prepared::finish`].
        handle: Handle,
    },
    /// The ring backend with registered buffers.
    Registered {
        /// The backend the scenarios run against.
        backend: ioring::IoRingRegistered,
        /// Pumped alongside every iteration, and torn down after the last.
        driver: Driver,
        /// Used once, by [`Prepared::finish`].
        handle: Handle,
    },
}

/// Builds one backend, performing every one-off cost outside any timed region.
///
/// The registered backend's registration happens here rather than in a measured
/// iteration, because a registration is a one-off whose cost belongs to the
/// decision to register rather than to any single transfer.
pub fn prepare(which: Which, config: &Config, job: &Job<'_>) -> Result<Prepared, Unavailable> {
    let depth = job.depth;
    // Every backend pre-allocates the same number of buffers of the same size,
    // so none is charged a per-operation allocation another avoids.
    let pool = depth.max(1);
    let capacity = job.block as usize;
    match which {
        Which::TokioOne | Which::TokioMany => {
            let width = if which == Which::TokioOne { 1 } else { 512 };
            TokioFs::new(width, pool, capacity)
                .map(Prepared::Pool)
                .map_err(|e| Unavailable {
                    name: format!("tokio::fs (blocking pool {width})"),
                    reason: e.to_string(),
                })
        }
        Which::RingPlain => {
            let mut backend =
                ioring::IoRingPlain::new(depth, pool, capacity).map_err(|e| Unavailable {
                    name: "win-ioring (owned buffers)".to_owned(),
                    reason: e.to_string(),
                })?;
            let driver = backend.take_driver().expect("taken once");
            let handle = backend.handle();
            Ok(Prepared::Plain {
                backend,
                driver,
                handle,
            })
        }
        Which::RingRegistered => {
            let mut backend = ioring::IoRingRegistered::new(depth).map_err(|e| Unavailable {
                name: "win-ioring (registered)".to_owned(),
                reason: e.to_string(),
            })?;
            let driver = backend.take_driver().expect("taken once");
            let handle = backend.handle();
            let registered = drive_while(
                &driver,
                backend.register(config.registered_buffers, capacity),
            );
            if let Err(e) = registered {
                // The driver has to be drained and the ring closed even on this
                // path, or a backend that failed to register would leave the
                // teardown to `Drop for Driver` in the middle of a preparation.
                handle.shutdown_now();
                futures::executor::block_on(driver.drive());
                return Err(Unavailable {
                    name: "win-ioring (registered)".to_owned(),
                    reason: e.to_string(),
                });
            }
            Ok(Prepared::Registered {
                backend,
                driver,
                handle,
            })
        }
    }
}

impl Prepared {
    /// The backend's name, for the report and the fairness account.
    pub fn name(&self) -> String {
        match self {
            Prepared::Pool(backend) => backend.name(),
            Prepared::Plain { backend, .. } => backend.name(),
            Prepared::Registered { backend, .. } => backend.name(),
        }
    }

    /// The backend's configuration, in enough detail to reproduce it.
    pub fn configuration(&self) -> String {
        match self {
            Prepared::Pool(backend) => backend.configuration(),
            Prepared::Plain { backend, .. } => backend.configuration(),
            Prepared::Registered { backend, .. } => backend.configuration(),
        }
    }

    /// Runs the job once — **the measured unit**.
    ///
    /// One `async fn` over three variants, so every call yields a future of one
    /// type, which is what a `FnMut() -> F` routine requires. The match costs
    /// one predictable branch per iteration and nothing per operation.
    ///
    /// `weakness` is [`Weakness::None`] for every real measurement. When it is
    /// not, the backend is wrapped in a [`Weakened`] before the scenario ever
    /// sees it, so a deliberately weakened run travels this identical path
    /// rather than a parallel implementation a test wrote for itself.
    pub async fn one(&self, job: &Job<'_>, weakness: Weakness) -> io::Result<Outcome> {
        match self {
            Prepared::Pool(backend) => run_job(backend, job, weakness).await,
            Prepared::Plain { backend, .. } => run_job(backend, job, weakness).await,
            Prepared::Registered { backend, .. } => run_job(backend, job, weakness).await,
        }
    }

    /// Runs `f` to completion on the thread this backend belongs to.
    ///
    /// For the ring backends that means selecting the work against
    /// `driver.drive()`, so the driver makes progress while the work awaits it
    /// and stays alive for the next iteration.
    pub fn block_on<T>(&self, f: impl Future<Output = T>) -> T {
        match self {
            Prepared::Pool(backend) => backend.block_on(f),
            Prepared::Plain { driver, .. } | Prepared::Registered { driver, .. } => {
                drive_while(driver, f)
            }
        }
    }

    /// Shuts the driver down, drains it cooperatively, and drops everything.
    ///
    /// Called once, after the last iteration. The drain is
    /// `futures::executor::block_on(driver.drive())` **by that name** and
    /// deliberately not [`Prepared::block_on`]: that method selects a
    /// `driver.drive()` of its own against the future it is given, so handing it
    /// `driver.drive()` would poll two drive futures against one driver
    /// concurrently, each evicting the other's registered waker — exactly what
    /// [`Driver::drive`]'s "poll exactly one of these per driver" forbids.
    pub fn finish(self) -> io::Result<()> {
        match self {
            Prepared::Pool(_) => Ok(()),
            Prepared::Plain { driver, handle, .. }
            | Prepared::Registered { driver, handle, .. } => {
                handle.shutdown_now();
                futures::executor::block_on(driver.drive());
                drop(driver);
                Ok(())
            }
        }
    }
}

/// Runs one job against one backend, through the one shared scenario entry
/// point.
///
/// The weakening is decided here, once per call, rather than inside the
/// scenario: nothing under this line knows a backend can be weakened.
async fn run_job<B: Backend>(
    backend: &B,
    job: &Job<'_>,
    weakness: Weakness,
) -> io::Result<Outcome> {
    match weakness {
        Weakness::None => run_scenario(backend, job).await,
        weakness => run_scenario(&Weakened::new(backend, weakness), job).await,
    }
}

/// Hands one backend to the scenario it was prepared for.
async fn run_scenario<B: Backend>(backend: &B, job: &Job<'_>) -> io::Result<Outcome> {
    scenario::run(
        backend,
        job.scenario,
        job.read_path,
        job.write_path,
        job.block,
        job.operations,
        job.depth,
    )
    .await
}

/// Runs `work` with `driver` pumped alongside it on this thread.
///
/// `select` rather than `join`: `drive()` only returns after shutdown, so a join
/// would require shutting the driver down per iteration — which is precisely
/// what must not happen between iterations.
fn drive_while<T>(driver: &Driver, work: impl Future<Output = T>) -> T {
    futures::executor::block_on(async {
        let driving = pin!(driver.drive());
        let work = pin!(work);
        match futures::future::select(driving, work).await {
            Either::Left(_) => {
                panic!("the driver shut down while an iteration was still running")
            }
            Either::Right((outcome, _)) => outcome,
        }
    })
}
