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
use win_ioring::runtime::{Driver, Handle, SubmissionCounts};

use crate::backend::Backend;
use crate::backends::compio::Compio;
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
    /// The completion-based, non-ring backend.
    ///
    /// A single field, like `Pool`: it brings its own runtime and has no driver
    /// to pump alongside the work, because the runtime pumps its own completion
    /// port from inside `block_on`.
    Compio(Compio),
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
        Which::RingPlain | Which::RingPlainSync => {
            let mut backend = ioring::IoRingPlain::new(depth, pool, capacity)
                .map(|b| b.with_handle_mode(which.handle_mode()))
                .map_err(|e| Unavailable {
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
        Which::RingRegistered | Which::RingRegisteredSync => {
            let mut backend = ioring::IoRingRegistered::new(depth)
                .map(|b| b.with_handle_mode(which.handle_mode()))
                .map_err(|e| Unavailable {
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
        Which::Compio => Compio::new(pool, capacity)
            .map(Prepared::Compio)
            .map_err(|e| Unavailable {
                name: "compio (IOCP)".to_owned(),
                reason: e.to_string(),
            }),
    }
}

/// A file opened by a [`Prepared`] backend, held open across timed iterations.///
/// Exists so the `handle-mode` arm can open **once**, outside the region it
/// times, and still run through the shared measurement path. See
/// [`crate::scenario::run_on_open_file`] for why that arm must not time its
/// opens: briefly, an open is one of the places `FILE_FLAG_OVERLAPPED` can cost
/// something, so timing it would fold per-open cost into the A/B delta and
/// destroy the depth-1 negative control.
///
/// The variants mirror [`Prepared`]'s because each backend has its own file
/// type. Pairing is checked at use rather than in the type system, which is the
/// cost of not making `Prepared` generic; the mismatch arm is unreachable for
/// files obtained from [`Prepared::open_read`].
pub enum PreparedFile {
    /// A file belonging to the thread-pool backend.
    Pool(<TokioFs as Backend>::File),
    /// A file belonging to the ring backend with caller-owned buffers.
    Plain(<ioring::IoRingPlain as Backend>::File),
    /// A file belonging to the ring backend with registered buffers.
    Registered(<ioring::IoRingRegistered as Backend>::File),
    /// A file belonging to the compio backend.
    Compio(<Compio as Backend>::File),
}

impl PreparedFile {
    /// The raw handle underneath, for a caller that wants to interrogate the
    /// file without taking ownership of it.
    ///
    /// The returned handle is borrowed: it stays owned by this `PreparedFile`
    /// and must not be closed.
    #[must_use]
    pub fn raw_handle(&self) -> std::os::windows::io::RawHandle {
        use std::os::windows::io::AsRawHandle;
        match self {
            PreparedFile::Pool(file) => file.as_raw_handle(),
            // `win_ioring::file::File::as_raw_handle` returns the `windows`
            // crate's `HANDLE` rather than `std`'s `RawHandle`, which is the
            // same pointer under a different name.
            PreparedFile::Plain(file) | PreparedFile::Registered(file) => file.as_raw_handle().0,
            PreparedFile::Compio(file) => file.as_raw_handle(),
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
            Prepared::Compio(backend) => backend.name(),
        }
    }

    /// The backend's configuration, in enough detail to reproduce it.
    pub fn configuration(&self) -> String {
        match self {
            Prepared::Pool(backend) => backend.configuration(),
            Prepared::Plain { backend, .. } => backend.configuration(),
            Prepared::Registered { backend, .. } => backend.configuration(),
            Prepared::Compio(backend) => backend.configuration(),
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
    ///
    /// # Why the submission counts are differenced here
    ///
    /// The counter on the driver is cumulative over the session, and a session
    /// spans preparation — which submits the registered backend's buffer
    /// registration — an untimed warm-up, every timed iteration, and a shutdown
    /// that submits cancellations. A single reading taken at the end would be a
    /// session total diluted by all of that, and would answer a question nobody
    /// asked.
    ///
    /// Bracketing the run here makes the figure a delta over **exactly the
    /// iteration whose [`Outcome`] carries it** — the same iteration whose
    /// achieved depth and trace are reported — and gives the warm-up its own
    /// delta for free, which matters because a combination that ran no timed
    /// iteration reports the warm-up's outcome.
    pub async fn one(&self, job: &Job<'_>, weakness: Weakness) -> io::Result<Outcome> {
        let before = self.submission_counts();
        let mut outcome = match self {
            Prepared::Pool(backend) => run_job(backend, job, weakness).await,
            Prepared::Plain { backend, .. } => run_job(backend, job, weakness).await,
            Prepared::Registered { backend, .. } => run_job(backend, job, weakness).await,
            Prepared::Compio(backend) => run_job(backend, job, weakness).await,
        }?;
        outcome.submitted = match (before, self.submission_counts()) {
            (Some(before), Some(after)) => Some(SubmissionCounts {
                submissions: after.submissions - before.submissions,
                entries: after.entries - before.entries,
            }),
            _ => None,
        };
        Ok(outcome)
    }

    /// Opens `path` for reading, once, outside any timed region.
    ///
    /// # Errors
    ///
    /// Propagates the backend's open failure.
    pub async fn open_read(&self, path: &std::path::Path) -> io::Result<PreparedFile> {
        Ok(match self {
            Prepared::Pool(backend) => PreparedFile::Pool(backend.open_read(path).await?),
            Prepared::Plain { backend, .. } => PreparedFile::Plain(backend.open_read(path).await?),
            Prepared::Registered { backend, .. } => {
                PreparedFile::Registered(backend.open_read(path).await?)
            }
            Prepared::Compio(backend) => PreparedFile::Compio(backend.open_read(path).await?),
        })
    }

    /// Runs one iteration against an already-open file.
    ///
    /// The pre-opened counterpart of [`Prepared::one`], carrying the same
    /// submission-count bracketing so the reported figure is a delta over
    /// exactly this iteration.
    ///
    /// Weakening is deliberately not accepted here. The `handle-mode` arm is a
    /// controlled experiment with one variable, and a weakened backend would be
    /// a second one.
    ///
    /// # Errors
    ///
    /// Propagates the scenario's failure. Returns
    /// [`std::io::ErrorKind::InvalidInput`] if `file` did not come from this
    /// backend, which cannot happen for a file from [`Prepared::open_read`].
    pub async fn one_on(&self, file: &PreparedFile, job: &Job<'_>) -> io::Result<Outcome> {
        let before = self.submission_counts();
        let read_bytes = self.read_bytes(job)?;
        let mut outcome = match (self, file) {
            (Prepared::Pool(backend), PreparedFile::Pool(file)) => {
                run_scenario_on(backend, file, read_bytes, job).await
            }
            (Prepared::Plain { backend, .. }, PreparedFile::Plain(file)) => {
                run_scenario_on(backend, file, read_bytes, job).await
            }
            (Prepared::Registered { backend, .. }, PreparedFile::Registered(file)) => {
                run_scenario_on(backend, file, read_bytes, job).await
            }
            (Prepared::Compio(backend), PreparedFile::Compio(file)) => {
                run_scenario_on(backend, file, read_bytes, job).await
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the file was opened by a different backend than the one \
                 running it",
            )),
        }?;
        outcome.submitted = match (before, self.submission_counts()) {
            (Some(before), Some(after)) => Some(SubmissionCounts {
                submissions: after.submissions - before.submissions,
                entries: after.entries - before.entries,
            }),
            _ => None,
        };
        Ok(outcome)
    }

    /// The read file's size, for the offset reconstruction in
    /// [`crate::scenario::run_on_open_file`].
    ///
    /// Read here rather than inside the scenario so the `metadata` call stays
    /// out of the caller's timed region, which is the entire point of the
    /// pre-opened path.
    fn read_bytes(&self, job: &Job<'_>) -> io::Result<u64> {
        Ok(std::fs::metadata(job.read_path)?.len())
    }

    /// What this backend's ring has submitted so far, cumulatively, or `None`
    /// for a backend that has no ring.
    fn submission_counts(&self) -> Option<SubmissionCounts> {
        match self {
            // Not applicable, and deliberately not zero. Neither of these has a
            // ring, so "how many submissions did the ring make" has no answer
            // rather than the answer none — and a zero would render in the
            // account as a measured figure instead of an absent one.
            Prepared::Pool(_) | Prepared::Compio(_) => None,
            Prepared::Plain { handle, .. } | Prepared::Registered { handle, .. } => {
                Some(handle.submission_counts())
            }
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
            Prepared::Compio(backend) => backend.block_on(f),
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
            Prepared::Pool(_) | Prepared::Compio(_) => Ok(()),
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

/// Runs one job against one backend on an already-open file.
///
/// The pre-opened counterpart of [`run_scenario`], and deliberately a thin
/// forwarder for the same reason that one is: nothing under this line knows
/// which arm it is serving.
async fn run_scenario_on<B: Backend>(
    backend: &B,
    file: &B::File,
    read_bytes: u64,
    job: &Job<'_>,
) -> io::Result<Outcome> {
    scenario::run_on_open_file(
        backend,
        job.scenario,
        file,
        read_bytes,
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
