//! The facts a timing does not carry, gathered beside it.
//!
//! Criterion reports how long an iteration took, with an interval. It has no
//! slot for what makes that number mean anything: what concurrency the run
//! actually achieved, whether the page cache could hold the working set, which
//! backends the host could provide, what order they ran in, and — above all —
//! whether every backend did the same work. Three homes for those were
//! considered:
//!
//! - encoding them into the benchmark identifier, rejected because the
//!   identifier is the key a stored baseline is matched on, and one that moved
//!   when achieved depth varied would silently orphan every baseline;
//! - `iter_custom` with our own measurement, rejected because that is
//!   hand-rolled timing under a different name;
//! - a sidecar account, which is this.
//!
//! It is emitted twice: a line per combination to **stderr** as the run
//! proceeds, because Criterion owns stdout and a reader redirecting results to a
//! file still needs to see a fairness failure; and in full to
//! `target/bench-data/fairness.md` at the end.

use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use win_ioring::runtime::SubmissionCounts;

use crate::concurrency::{Achieved, Depth, ShapeCheck, Shortfall};
use crate::config::Config;
use crate::harness::Record;
use crate::scenario::Scenario;
use crate::workload::CachePremise;

/// The time budget the run was given.
///
/// Reported because two of these three moved from Criterion's defaults to fit
/// the run budget, and a reader comparing intervals needs to know how long each
/// estimate was gathered over.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    /// How long each benchmark is warmed up for.
    pub warm_up: Duration,
    /// How long each benchmark is measured over.
    pub measurement: Duration,
    /// How many samples each estimate rests on.
    pub sample_size: usize,
}

/// How long a full `cargo bench` run is allowed to take.
///
/// This existed only as prose until the matrix grew, at which point it needed
/// to be a number something could check. It is the constraint that fixed
/// [`Budget::warm_up`] and [`Budget::measurement`] below Criterion's defaults:
/// a benchmark costs at least its warm-up plus its measurement window however
/// small an iteration is, so the floor is `benchmarks * (warm_up +
/// measurement)` and shrinking the per-iteration work cannot get under it.
///
/// Six minutes is not a property of the machine. It is how long a run may take
/// before people stop doing them, which is the only budget that matters — and
/// being a chosen number rather than a measured one is exactly why it was the
/// thing that moved.
///
/// It was five minutes until a fifth backend was added, growing the matrix 25%.
/// The alternative was to cut the matrix or shorten the windows. Shortening the
/// windows would have widened every published confidence interval, degrading
/// every result in the report to protect a number nobody measured — and it
/// would have done the most damage to the deep-queue comparisons, which is the
/// exact question the fifth backend was added to answer. Dropping a scenario or
/// a depth would have paid a real data point for a saving that still did not
/// close the gap. So the chosen number moved, once, deliberately, and says here
/// that it did.
///
/// What did **not** move is the empirically grounded half. `RUN_BUDGET / 2` as
/// the affordability limit (`tests/comparison.rs`) is derived from measured
/// floor-to-wall multipliers of 1.79x, 1.83x and 2.06x, and [`Budget::CHOSEN`]
/// below is unchanged. Raising a chosen number is a decision; changing a
/// measured one would be a claim.
pub const RUN_BUDGET: Duration = Duration::from_secs(360);

/// The wall-clock budget for the **unbuffered** arm, which is a separate,
/// opt-in target (`cargo bench --bench unbuffered`).
///
/// Deliberately not [`RUN_BUDGET`]. The two arms measure different things at
/// different costs: the warm-cache matrix never touches the device, while every
/// iteration here is device-bound. Sharing a budget would force one of them to
/// be wrong — either this arm is squeezed below the point where it measures
/// anything, or the 360 s that governs the project's primary published result
/// gets raised to accommodate an arm that is not part of it.
///
/// The value is derived, not chosen for comfort. The grid is 2 scenarios × 3
/// depths × 6 configurations = 36 benchmarks; at [`Budget::CHOSEN`] the
/// Criterion floor is 36 × 3 s = 108 s, and at the floor-to-wall multipliers
/// measured for the buffered arm (1.79x, 1.83x, 2.06x — see [`RUN_BUDGET`]) the
/// projected real cost is 190–225 s.
///
/// 600 s leaves roughly 2.7x headroom over that projection, which is
/// intentional rather than slack. Device-bound timings vary far more across
/// host states — SLC cache exhaustion, thermal behaviour, background garbage
/// collection — than warm-cache ones do, and the multipliers above were
/// measured on the warm-cache arm, so they are an analogy here rather than a
/// measurement. A budget that a slower drive silently blows is a budget that
/// gets raised under pressure instead of respected.
pub const UNBUFFERED_RUN_BUDGET: Duration = Duration::from_secs(600);

impl Budget {
    /// The budget this suite actually runs at.
    ///
    /// Every *statistical* parameter is Criterion's own — confidence level,
    /// resampling count, noise threshold. The two timing parameters moved
    /// because they had to: at Criterion's defaults of 3 s and 5 s this matrix
    /// is over [`RUN_BUDGET`] before a single I/O is issued.
    ///
    /// It lives here rather than in the benchmark so that a test can check the
    /// matrix still fits, which a constant in the bench target could not be
    /// reached to do.
    pub const CHOSEN: Self = Self {
        warm_up: Duration::from_secs(1),
        measurement: Duration::from_secs(2),
        sample_size: 100,
    };

    /// The least time a matrix of `benchmarks` can take under this budget.
    ///
    /// Criterion's `measurement_time` is a floor, not a cap: a benchmark whose
    /// samples fit inside the window is padded back up with extra iterations to
    /// fill it, and one whose samples do not fit overruns it. So this is a lower
    /// bound that no amount of shrinking the workload can beat, not an estimate
    /// — real runs land well above it. Preparation, file creation and Criterion's
    /// own analysis are all on top.
    #[must_use]
    pub fn floor(&self, benchmarks: usize) -> Duration {
        (self.warm_up + self.measurement) * u32::try_from(benchmarks).unwrap_or(u32::MAX)
    }
}

/// What one backend did in one combination.
#[derive(Debug)]
pub enum Standing {
    /// It ran, was timed, and agreed with the reference.
    Timed {
        /// What concurrency the verified iteration achieved.
        achieved: Achieved,
        /// How many I/Os one iteration issued.
        io_count: usize,
        /// How many measured iterations ran.
        iterations: usize,
        /// What the ring submitted during that iteration, or `None` for a
        /// backend with no ring.
        submitted: Option<SubmissionCounts>,
        /// Whether the achieved depth is what the declared shape predicts.
        shape: ShapeCheck,
    },
    /// It was prepared, warmed and verified, but the timer ran no iterations.
    ///
    /// What a filtered run produces: the fairness check never narrows, but the
    /// verified trace came from the warm-up rather than from a timed region.
    /// Every combination of an unfiltered run should be `Timed`.
    VerifiedNotTimed {
        /// What concurrency the warm-up achieved.
        achieved: Achieved,
        /// How many I/Os one iteration issued.
        io_count: usize,
        /// What the ring submitted during the warm-up.
        ///
        /// A real datum rather than a hole: the warm-up is bracketed by the same
        /// mechanism a timed iteration is, so a combination the timer declined
        /// still reports a figure belonging to an iteration that really ran.
        submitted: Option<SubmissionCounts>,
        /// Whether the achieved depth is what the declared shape predicts.
        shape: ShapeCheck,
    },
    /// The host cannot provide this backend.
    Unavailable {
        /// Why not.
        reason: String,
    },
    /// It prepared, then failed.
    Failed {
        /// What went wrong.
        error: String,
    },
}

/// One backend's line in the account.
#[derive(Debug)]
pub struct Entry {
    /// The backend's display name.
    pub backend: String,
    /// Its configuration, in enough detail to reproduce it.
    pub configuration: String,
    /// What became of it.
    pub standing: Standing,
}

impl Entry {
    /// Builds an entry from what the measurement produced.
    pub fn from_record(record: &Record) -> Self {
        match record {
            Record::Measured {
                name,
                configuration,
                achieved,
                trace,
                iterations,
                timed,
                submitted,
                shape,
            } => Entry {
                backend: name.clone(),
                configuration: configuration.clone(),
                standing: if *timed {
                    Standing::Timed {
                        achieved: *achieved,
                        io_count: trace.operations(),
                        iterations: *iterations,
                        submitted: *submitted,
                        shape: *shape,
                    }
                } else {
                    Standing::VerifiedNotTimed {
                        achieved: *achieved,
                        io_count: trace.operations(),
                        submitted: *submitted,
                        shape: *shape,
                    }
                },
            },
            Record::Unavailable { name, reason } => Entry {
                backend: name.clone(),
                configuration: "not built".to_owned(),
                standing: Standing::Unavailable {
                    reason: reason.clone(),
                },
            },
            Record::Failed {
                name,
                configuration,
                error,
            } => Entry {
                backend: name.clone(),
                configuration: configuration.clone(),
                standing: Standing::Failed {
                    error: error.to_string(),
                },
            },
        }
    }
}

/// One (scenario, depth), across every backend that was asked to run it.
#[derive(Debug)]
pub struct Combination {
    /// The scenario.
    pub scenario: Scenario,
    /// The configured in-flight depth.
    pub depth: Depth,
    /// The order the backends actually ran in, which rotates per combination.
    pub order: Vec<&'static str>,
    /// The backend whose trace every other one was compared against.
    pub reference: Option<String>,
    /// One entry per backend, in run order.
    pub entries: Vec<Entry>,
}

/// What one file open costs on each runtime, measured on the host that ran.
///
/// Reported because the comparison is not perfectly fair and this is the size of
/// the unfairness. Every scenario opens its file **inside** the timed iteration,
/// so compio pays its runtime's open cost where `std::fs::File::open` pays a
/// syscall — once for the read scenarios, twice for write-then-read.
///
/// **The direction matters more than the magnitude, and it is not symmetric.**
/// compio's open is the slower of the two, so the bias inflates every compio
/// timing by a fixed amount that does not scale with depth. That makes this
/// measurement **conservative** for any conclusion of the form "compio is also
/// slower here" — the real gap is smaller than published — and
/// **anti-conservative** for any conclusion of the form "compio scales well",
/// because a fixed per-iteration cost is proportionally largest at depth 1,
/// where iterations are shortest, and shrinks as depth rises. A small share of
/// an iteration is not licence to ignore it in the second case. That asymmetry
/// is the reason this is measured rather than waved at.
///
/// **These are medians, not means, and that was forced by measurement.** An
/// arithmetic mean over 200 opens has no defence against one stalled open: a
/// single 3 ms hiccup moves it by 15 µs, which is the whole `std` figure. Fifteen
/// consecutive runs of the probe produced fourteen agreeing results and one where
/// `std`'s mean doubled to 28.9 µs against a 14.3–15.4 µs floor — enough to fail
/// the probe's own ratio check on an honest tree. The median of the same samples
/// is unmoved by a handful of outliers, which is what makes the check safe to run
/// in front of a six-minute benchmark.
///
/// [`OpenCost::std_p90`] and [`OpenCost::compio_p90`] are carried alongside so a
/// reader can see the spread rather than only a point estimate. The two differ
/// substantially for compio and barely at all for `std`, which is itself the
/// finding: the async open's cost is variable in a way the syscall's is not.
#[derive(Debug, Clone, Copy)]
pub struct OpenCost {
    /// Median `std::fs::File::open` over [`OpenCost::SAMPLES`] opens.
    pub std_median: Duration,
    /// Median `compio::fs::File::open` over [`OpenCost::SAMPLES`] opens.
    pub compio_median: Duration,
    /// The 90th percentile of the same `std` samples.
    pub std_p90: Duration,
    /// The 90th percentile of the same compio samples.
    pub compio_p90: Duration,
}

impl OpenCost {
    /// How many opens each figure is taken over.
    ///
    /// 200, matching the out-of-tree probe that produced the expected band, so
    /// the in-tree figure is comparable with it rather than merely similar.
    pub const SAMPLES: usize = 200;

    /// Summarises one set of per-open samples.
    ///
    /// Sorts in place. The 90th percentile is taken at index `len * 9 / 10`,
    /// clamped to the last element, which for 200 samples is index 180 — the
    /// 181st value — a
    /// nearest-rank percentile, not an interpolated one, because interpolating
    /// between two adjacent measured durations invents a value that was not
    /// observed.
    ///
    /// Returns `None` for an empty slice rather than picking a value out of the
    /// air, which is what indexing an empty slice would otherwise panic over.
    #[must_use]
    pub fn summarise(samples: &mut [Duration]) -> Option<(Duration, Duration)> {
        if samples.is_empty() {
            return None;
        }
        samples.sort_unstable();
        let median = samples[samples.len() / 2];
        let p90 = samples[(samples.len() * 9 / 10).min(samples.len() - 1)];
        Some((median, p90))
    }

    /// What compio's open costs over `std`'s, per open, at the median.
    ///
    /// Saturating rather than wrapping: if compio ever came out faster this
    /// returns zero instead of an enormous number, and the ratio printed
    /// alongside it is what would show the reversal.
    #[must_use]
    pub fn extra(&self) -> Duration {
        self.compio_median.saturating_sub(self.std_median)
    }

    /// How many times `std`'s open compio's costs, at the median.
    ///
    /// Zero if `std`'s median is zero, which cannot happen for a real syscall but
    /// is not worth a panic in a reporting path.
    #[must_use]
    pub fn ratio(&self) -> f64 {
        let std = self.std_median.as_secs_f64();
        if std == 0.0 {
            0.0
        } else {
            self.compio_median.as_secs_f64() / std
        }
    }
}

/// Everything the run established that is not a timing.
pub struct Account {
    /// The parameters the run used.
    pub config: Config,
    /// The time budget it was given.
    pub budget: Budget,
    /// Whether the configuration was the small one.
    pub test_mode: bool,
    /// The volume the working files sit on.
    pub volume: String,
    /// Whether the warm-cache premise holds for the resident working set.
    pub cache: CachePremise,
    /// One per (scenario, depth), in the order they ran.
    pub combinations: Vec<Combination>,
    /// How many drivers the process built.
    pub drivers_built: usize,
    /// How many of the measured combinations were ring ones, which is what the
    /// driver count is read against.
    pub ring_combinations: usize,
    /// The write scenario's file size after the run, against what one iteration
    /// requires.
    pub write_file_bytes: Option<u64>,
    /// How long preparing and warming the working files took.
    pub preparation: Duration,
    /// What opening a file costs on each runtime, measured on this host.
    ///
    /// `None` for four distinct reasons, which the renderer distinguishes rather
    /// than printing all of them as absence: a test run, where the dev profile
    /// makes an absolute timing meaningless; a `compio` runtime that would not
    /// build; a failed `std` open; a failed compio open. The last three are
    /// reported as **NOT MEASURED**, because a benchmark account that quietly
    /// omits a line it usually carries is indistinguishable from one where the
    /// measurement was never attempted. See [`OpenCost`].
    pub open_cost: Option<OpenCost>,
    /// How long everything after that took.
    pub measurement: Duration,
}

impl Account {
    /// An empty account for a run about to start.
    pub fn new(config: Config, budget: Budget, test_mode: bool, volume: String) -> Self {
        let cache = crate::workload::cache_premise(config.resident_working_set());
        Self {
            config,
            budget,
            test_mode,
            volume,
            cache,
            combinations: Vec::new(),
            drivers_built: 0,
            ring_combinations: 0,
            write_file_bytes: None,
            preparation: Duration::ZERO,
            open_cost: None,
            measurement: Duration::ZERO,
        }
    }

    /// Records one combination, and says so on stderr as it goes.
    ///
    /// Reported as the run proceeds rather than only at the end, because a run
    /// that exits on a fairness failure still owes the reader everything it had
    /// established up to that point.
    pub fn record(&mut self, combination: Combination) {
        eprintln!("{}", one_line(&combination));
        self.combinations.push(combination);
    }

    /// Renders the whole account.
    pub fn render(&self) -> String {
        let mut out = String::new();
        writeln!(out, "# Fairness account").unwrap();
        writeln!(out).unwrap();
        self.render_conditions(&mut out);
        self.render_combinations(&mut out);
        self.render_teardown(&mut out);
        out
    }

    /// Writes the account beside the working files.
    pub fn write(&self, dir: &Path) -> io::Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("fairness.md");
        std::fs::write(&path, self.render())?;
        Ok(path)
    }

    /// Whether anything in the run should stop it being reported as a success.
    ///
    /// A backend that could not complete its work is not a backend that was
    /// slow, so a failure is fatal rather than a footnote. An unavailable
    /// backend is neither: the host simply cannot provide it.
    pub fn failures(&self) -> Vec<String> {
        let mut failures = Vec::new();
        for combination in &self.combinations {
            for entry in &combination.entries {
                let at = format!(
                    "{} at {} depth {}",
                    entry.backend,
                    combination.scenario.name(),
                    combination.depth
                );
                match &entry.standing {
                    Standing::Failed { error } => failures.push(format!("{at}: {error}")),
                    Standing::Timed { shape, .. } | Standing::VerifiedNotTimed { shape, .. } => {
                        if shape.is_failure() {
                            failures.push(format!(
                                "{at}: achieved a mean depth of {} where its shape predicts {} — \
                                 the run did not drive the shape it declared, so its timing \
                                 measures something other than what it is labelled",
                                shape.measured(),
                                shape.predicted()
                            ));
                        }
                    }
                    Standing::Unavailable { .. } => {}
                }
            }
        }
        failures
    }

    fn render_conditions(&self, out: &mut String) {
        writeln!(out, "## Conditions").unwrap();
        writeln!(out).unwrap();
        writeln!(
            out,
            "These figures measure **per-operation software overhead against a warm page \
             cache**. They are not device throughput, and must not be quoted as such."
        )
        .unwrap();
        writeln!(out).unwrap();
        writeln!(
            out,
            "- configuration: {}",
            if self.test_mode {
                "small (test mode)"
            } else {
                "default"
            }
        )
        .unwrap();
        writeln!(
            out,
            "- time budget: warm-up {:?}, measurement {:?}, {} samples per estimate",
            self.budget.warm_up, self.budget.measurement, self.budget.sample_size
        )
        .unwrap();
        writeln!(
            out,
            "- read file: {}, sequential in {} blocks, random in {} blocks",
            bytes(self.config.read_file_bytes),
            bytes(self.config.sequential_block as u64),
            bytes(self.config.random_block as u64)
        )
        .unwrap();
        for scenario in Scenario::all() {
            let (block, operations) = self.config.work(scenario);
            writeln!(
                out,
                "- {}: {operations} operations of {} per iteration, touching {}",
                scenario.name(),
                bytes(block as u64),
                bytes(self.config.touched_bytes(scenario))
            )
            .unwrap();
        }
        // Both figures, because they are no longer the same number: an iteration
        // touches a fraction of the read file, but every byte of it is a
        // candidate for the next iteration's random offsets.
        writeln!(
            out,
            "- resident working set: {} (read file plus write file); the largest single \
             iteration touches {}",
            bytes(self.config.resident_working_set()),
            bytes(
                Scenario::all()
                    .into_iter()
                    .map(|scenario| self.config.touched_bytes(scenario))
                    .max()
                    .unwrap_or(0)
            )
        )
        .unwrap();
        match &self.cache {
            CachePremise::Holds { total } => writeln!(
                out,
                "- warm cache: the resident working set is within a quarter of {} physical memory",
                bytes(*total)
            )
            .unwrap(),
            CachePremise::Doubtful { total } => writeln!(
                out,
                "- warm cache: **PREMISE DOUBTFUL** — the resident working set is large relative \
                 to {} physical memory, so these figures may include device I/O",
                bytes(*total)
            )
            .unwrap(),
            CachePremise::Unknown => {
                writeln!(out, "- warm cache: physical memory could not be determined").unwrap()
            }
        }
        writeln!(out, "- working files on: {}", self.volume).unwrap();
        writeln!(
            out,
            "- host: {} logical processors",
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(0)
        )
        .unwrap();
        writeln!(
            out,
            "- setup — ring, runtime, registration and buffer pool — is built once per scenario \
             and depth and is outside every timed region. Files are reopened per iteration."
        )
        .unwrap();
        writeln!(out).unwrap();
    }

    fn render_combinations(&self, out: &mut String) {
        for combination in &self.combinations {
            writeln!(
                out,
                "## {} — depth {}",
                combination.scenario.name(),
                combination.depth
            )
            .unwrap();
            writeln!(out).unwrap();
            writeln!(out, "- run order: {}", combination.order.join(" → ")).unwrap();
            match &combination.reference {
                Some(reference) => writeln!(
                    out,
                    "- every backend below agreed with the reference, **{reference}**"
                )
                .unwrap(),
                None => writeln!(
                    out,
                    "- no backend produced a trace, so there was nothing to compare"
                )
                .unwrap(),
            }
            writeln!(out).unwrap();
            for entry in &combination.entries {
                match &entry.standing {
                    Standing::Timed {
                        achieved,
                        io_count,
                        iterations,
                        submitted,
                        shape,
                    } => writeln!(
                        out,
                        "- **{}** — timed, {io_count} I/Os per iteration, {iterations} \
                         iteration{}, achieved depth {}\n  - {}\n  - {}",
                        entry.backend,
                        if *iterations == 1 { "" } else { "s" },
                        depth_of(achieved),
                        entry.configuration,
                        batching_of(submitted, shape)
                    )
                    .unwrap(),
                    Standing::VerifiedNotTimed {
                        achieved,
                        io_count,
                        submitted,
                        shape,
                    } => writeln!(
                        out,
                        "- **{}** — **verified but not timed** (filtered out), {io_count} I/Os \
                         per iteration, achieved depth {}\n  - {}\n  - {}",
                        entry.backend,
                        depth_of(achieved),
                        entry.configuration,
                        batching_of(submitted, shape)
                    )
                    .unwrap(),
                    Standing::Unavailable { reason } => {
                        writeln!(out, "- **{}** — unavailable: {reason}", entry.backend).unwrap()
                    }
                    Standing::Failed { error } => writeln!(
                        out,
                        "- **{}** — **FAILED**: {error}\n  - {}",
                        entry.backend, entry.configuration
                    )
                    .unwrap(),
                }
            }
            writeln!(out).unwrap();
        }
    }

    fn render_teardown(&self, out: &mut String) {
        writeln!(out, "## Teardown and provenance").unwrap();
        writeln!(out).unwrap();
        // Against the ring combinations, not against all of them: the two
        // thread-pool backends and compio build no driver, so 50 measured
        // combinations build 20 drivers and reading SC-014 against 50 would
        // fail a correct run.
        //
        // The narrative clause is conditional on the two numbers, because the
        // reassuring version of this line is the one a reader would most want to
        // be able to trust: printing "one per combination, not one per
        // iteration" beside 2000 and 20 would be the exact false comfort the
        // line exists to prevent. Same shape as the write-file line below.
        writeln!(
            out,
            "- drivers built: {} against {} ring combinations measured — {} (the thread-pool \
             and compio backends build none)",
            self.drivers_built,
            self.ring_combinations,
            if self.drivers_built == self.ring_combinations {
                "one per combination, not one per iteration"
            } else {
                "**NOT ONE PER COMBINATION**"
            }
        )
        .unwrap();
        match self.write_file_bytes {
            Some(actual) => {
                let expected = self.config.write_file_bytes();
                writeln!(
                    out,
                    "- write file after the run: {} against {} for one iteration{}",
                    bytes(actual),
                    bytes(expected),
                    if actual <= expected {
                        ""
                    } else {
                        " — **GREW ACROSS ITERATIONS**"
                    }
                )
                .unwrap();
            }
            None => writeln!(out, "- write file after the run: not measured").unwrap(),
        }
        writeln!(
            out,
            "- the first and last measured iteration of every combination issued and delivered \
             the same work; a disagreement would have ended the run"
        )
        .unwrap();
        writeln!(
            out,
            "- preparation: {:.1}s; measurement: {:.1}s",
            self.preparation.as_secs_f64(),
            self.measurement.as_secs_f64()
        )
        .unwrap();
        match self.open_cost {
            Some(open) => writeln!(
                out,
                "- one file open: std {:.1}us (p90 {:.1}us), compio {:.1}us (p90 {:.1}us), \
                 medians over {} opens each — compio costs {:.1}x, {:.1}us more per open. \
                 Every scenario opens inside the timed iteration, so this inflates compio: \
                 conservative where compio is reported slower, anti-conservative where it is \
                 reported to scale well",
                open.std_median.as_secs_f64() * 1e6,
                open.std_p90.as_secs_f64() * 1e6,
                open.compio_median.as_secs_f64() * 1e6,
                open.compio_p90.as_secs_f64() * 1e6,
                OpenCost::SAMPLES,
                open.ratio(),
                open.extra().as_secs_f64() * 1e6
            )
            .unwrap(),
            // Said rather than left blank. A run whose probe failed and a run
            // that never attempted one are different facts, and absence renders
            // them identically — so a reader of a benchmark account would have
            // no way to tell a measurement that was skipped from one that broke.
            None if self.test_mode => writeln!(
                out,
                "- one file open: not measured — a test run, where the dev profile makes an \
                 absolute timing meaningless"
            )
            .unwrap(),
            None => writeln!(
                out,
                "- one file open: **NOT MEASURED** — the probe failed on this host, so the \
                 async-open fairness caveat is unquantified for this run"
            )
            .unwrap(),
        }
        writeln!(out).unwrap();
        writeln!(
            out,
            "Achieved depth is measured by this crate, so it cannot see a backend serialising \
             operations below its own interface. Read it beside the backend's configuration."
        )
        .unwrap();
    }
}

/// The stderr line one combination gets while the run is in progress.
fn one_line(combination: &Combination) -> String {
    let mut line = format!(
        "  {} depth {}: {}",
        combination.scenario.name(),
        combination.depth,
        combination.order.join(" → ")
    );
    for entry in &combination.entries {
        match &entry.standing {
            Standing::Timed { achieved, .. } => {
                write!(line, "; {} depth {}", entry.backend, depth_of(achieved)).unwrap()
            }
            Standing::VerifiedNotTimed { .. } => {
                write!(line, "; {} verified, not timed", entry.backend).unwrap()
            }
            Standing::Unavailable { .. } => {
                write!(line, "; {} unavailable", entry.backend).unwrap()
            }
            Standing::Failed { error } => {
                write!(line, "; {} FAILED: {error}", entry.backend).unwrap()
            }
        }
    }
    line
}

/// The achieved depth, with the shortfall that qualifies it.
fn depth_of(achieved: &Achieved) -> String {
    match achieved.shortfall {
        Shortfall::None => format!("{:.1} (peak {})", achieved.mean, achieved.peak),
        Shortfall::Expected => format!(
            "{:.1} (peak {}, short — expected)",
            achieved.mean, achieved.peak
        ),
        Shortfall::Unexpected => {
            format!("{:.1} (peak {}, **SHORT**)", achieved.mean, achieved.peak)
        }
    }
}

/// What one iteration's submissions say about batching, and whether the run
/// drove the shape it declared.
///
/// The two belong on one line because neither is worth much alone: a batching
/// figure from a run that drove the wrong shape describes the wrong thing, and a
/// shape verdict without the figure does not say what batching actually
/// happened.
///
/// Reported as **not applicable** rather than zero for the backends without a
/// ring — the two `tokio::fs` configurations and compio. They submit nothing
/// because they have no ring, and a zero here would read as "this backend
/// batches nothing", which is a claim about a mechanism it does not have.
///
/// Entries are not exactly operations: a buffer registration and a shutdown
/// cancellation occupy entries too. The delta is taken around one iteration, so
/// neither of those falls inside it, but the figure remains entries per
/// submission used as a proxy for operations per submission.
fn batching_of(submitted: &Option<SubmissionCounts>, shape: &ShapeCheck) -> String {
    let batching = match submitted {
        None => "batching: not applicable (no ring)".to_owned(),
        Some(counts) if counts.submissions == 0 => {
            "batching: no submissions in the measured iteration".to_owned()
        }
        Some(counts) => format!(
            "batching: {:.1} entries per submission ({} entries over {} submissions)",
            counts.entries as f64 / counts.submissions as f64,
            counts.entries,
            counts.submissions
        ),
    };
    let verdict = match shape {
        ShapeCheck::Matched { predicted, .. } => {
            format!("shape: as predicted ({predicted:.2})")
        }
        ShapeCheck::PoolBound {
            predicted,
            measured,
        } => format!(
            "shape: {measured:.2} against a predicted {predicted:.2}, bounded by the buffer pool"
        ),
        ShapeCheck::Mismatched {
            predicted,
            measured,
        } => format!("shape: **MISMATCHED** — {measured:.2} against a predicted {predicted:.2}"),
    };
    format!("{batching}; {verdict}")
}

/// A byte count a reader can scan.
fn bytes(count: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    const KIB: u64 = 1024;
    // Down to bytes, because the small configuration's random block is 512 and
    // a KiB-only formatter reported it as "0 KiB". Truncating rather than
    // rounding: every figure here is either exact or an order-of-magnitude
    // sanity check, and neither wants a decimal point.
    if count >= MIB {
        format!("{} MiB", count / MIB)
    } else if count >= KIB {
        format!("{} KiB", count / KIB)
    } else {
        format!("{count} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::concurrency::Shortfall;
    use crate::harness::Which;

    fn achieved() -> Achieved {
        Achieved {
            peak: 4,
            mean: 3.9,
            shortfall: Shortfall::None,
        }
    }

    /// A shape verdict that agrees, for the cases not about shape checking.
    fn matched() -> ShapeCheck {
        ShapeCheck::Matched {
            predicted: 3.9,
            measured: 3.9,
        }
    }

    /// A shape verdict that disagrees and is not forgiven — what a run that
    /// drove a different shape than it declared produces.
    fn mismatched() -> ShapeCheck {
        ShapeCheck::Mismatched {
            predicted: 32.5,
            measured: 56.125,
        }
    }

    fn account() -> Account {
        Account::new(
            Config::small(),
            Budget {
                warm_up: Duration::from_secs(1),
                measurement: Duration::from_secs(3),
                sample_size: 100,
            },
            true,
            "D:\\".to_owned(),
        )
    }

    /// The account is what a reader gets instead of the report, so the facts
    /// Criterion cannot carry must all be in it.
    #[test]
    fn the_account_carries_what_a_timing_does_not() {
        let mut account = account();
        account.ring_combinations = 2;
        account.drivers_built = 2;
        account.write_file_bytes = Some(account.config.write_file_bytes());
        account.record(Combination {
            scenario: Scenario::RandomRead,
            depth: 4,
            order: vec![Which::TokioOne.slug(), Which::RingPlain.slug()],
            reference: Some("tokio::fs (blocking pool 1)".to_owned()),
            entries: vec![
                Entry {
                    backend: "tokio::fs (blocking pool 1)".to_owned(),
                    configuration: "a pool of one".to_owned(),
                    standing: Standing::Timed {
                        achieved: achieved(),
                        io_count: 64,
                        iterations: 100,
                        submitted: None,
                        shape: matched(),
                    },
                },
                Entry {
                    backend: "win-ioring (registered)".to_owned(),
                    configuration: "a ring".to_owned(),
                    standing: Standing::Unavailable {
                        reason: "the platform refused a ring".to_owned(),
                    },
                },
            ],
        });

        let rendered = account.render();
        for expected in [
            "random read",
            "achieved depth 3.9 (peak 4)",
            "unavailable: the platform refused a ring",
            "warm cache",
            "resident working set",
            "drivers built: 2 against 2 ring combinations",
            "write file after the run",
            "run order: tokio-pool-1 → ioring-owned",
        ] {
            assert!(
                rendered.contains(expected),
                "the account did not report {expected:?}:\n{rendered}"
            );
        }
    }

    /// The reassuring half of the driver line is a claim, so it must be
    /// conditional on the numbers beside it.
    ///
    /// A run that built a driver per iteration is exactly the run whose reader
    /// needs to be told so, and it is the run in which an unconditional "one per
    /// combination, not one per iteration" would be most convincing and most
    /// wrong.
    #[test]
    fn a_driver_count_that_does_not_match_is_not_reported_as_if_it_did() {
        let mut account = account();
        account.ring_combinations = 2;
        account.drivers_built = 1800;

        let rendered = account.render();
        assert!(
            rendered.contains(
                "drivers built: 1800 against 2 ring combinations measured — **NOT \
                 ONE PER COMBINATION**"
            ),
            "a mismatched driver count was not marked:\n{rendered}"
        );
        assert!(
            !rendered.contains("one per combination, not one per iteration"),
            "a mismatched driver count still claimed one driver per combination:\n{rendered}"
        );
    }

    /// A backend that failed must not be reportable as a run that succeeded.
    #[test]
    fn a_failed_backend_is_a_failure_and_an_unavailable_one_is_not() {
        let mut account = account();
        account.record(Combination {
            scenario: Scenario::SequentialRead,
            depth: 1,
            order: vec![Which::TokioOne.slug()],
            reference: None,
            entries: vec![
                Entry {
                    backend: "absent".to_owned(),
                    configuration: "not built".to_owned(),
                    standing: Standing::Unavailable {
                        reason: "no ring here".to_owned(),
                    },
                },
                Entry {
                    backend: "broken".to_owned(),
                    configuration: "a pool".to_owned(),
                    standing: Standing::Failed {
                        error: "the file vanished".to_owned(),
                    },
                },
            ],
        });

        let failures = account.failures();
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("broken"), "{failures:?}");
        assert!(failures[0].contains("the file vanished"), "{failures:?}");
    }

    /// A run that did not drive the shape it declared fails the suite, through
    /// the same path a backend error takes — which is the path that writes the
    /// account before returning non-zero. A mismatch that only annotated a line
    /// would leave a wrongly-shaped run to be published as a measurement of the
    /// shape it claimed.
    #[test]
    fn a_run_that_drove_the_wrong_shape_is_a_failure() {
        let mut account = account();
        account.record(Combination {
            scenario: Scenario::BulkRead,
            depth: 64,
            order: vec![Which::RingPlain.slug()],
            reference: Some("win-ioring (plain)".to_owned()),
            entries: vec![Entry {
                backend: "win-ioring (plain)".to_owned(),
                configuration: "a ring".to_owned(),
                standing: Standing::Timed {
                    achieved: achieved(),
                    io_count: 256,
                    iterations: 100,
                    submitted: Some(SubmissionCounts {
                        submissions: 256,
                        entries: 256,
                    }),
                    shape: mismatched(),
                },
            }],
        });

        let failures = account.failures();
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("bulk read"), "{failures:?}");
        assert!(failures[0].contains("56.125"), "{failures:?}");
        assert!(failures[0].contains("32.5"), "{failures:?}");
    }

    /// A shape verdict that agrees is not a failure, or every run would fail and
    /// the check would carry no information.
    #[test]
    fn a_run_that_drove_its_declared_shape_is_not_a_failure() {
        let mut account = account();
        account.record(Combination {
            scenario: Scenario::BulkRead,
            depth: 64,
            order: vec![Which::RingPlain.slug()],
            reference: Some("win-ioring (plain)".to_owned()),
            entries: vec![Entry {
                backend: "win-ioring (plain)".to_owned(),
                configuration: "a ring".to_owned(),
                standing: Standing::Timed {
                    achieved: achieved(),
                    io_count: 256,
                    iterations: 100,
                    submitted: None,
                    shape: matched(),
                },
            }],
        });

        assert!(account.failures().is_empty(), "{:?}", account.failures());
    }

    /// FR-005: the batching figure is reported for backends that have a ring and
    /// marked not applicable for those that do not.
    ///
    /// A `tokio::fs` or compio backend submits nothing because it has no ring.
    /// Rendering that as `0.0 entries per submission` would read as a claim that
    /// it batches nothing, which is a statement about a mechanism it does not
    /// have.
    #[test]
    fn the_batching_figure_is_rendered_for_rings_and_marked_absent_otherwise() {
        let mut account = account();
        account.record(Combination {
            scenario: Scenario::BulkRead,
            depth: 64,
            order: vec![Which::TokioOne.slug(), Which::RingPlain.slug()],
            reference: Some("tokio::fs (blocking pool 1)".to_owned()),
            entries: vec![
                Entry {
                    backend: "tokio::fs (blocking pool 1)".to_owned(),
                    configuration: "a pool of one".to_owned(),
                    standing: Standing::Timed {
                        achieved: achieved(),
                        io_count: 256,
                        iterations: 100,
                        submitted: None,
                        shape: matched(),
                    },
                },
                Entry {
                    backend: "win-ioring (plain)".to_owned(),
                    configuration: "a ring".to_owned(),
                    standing: Standing::Timed {
                        achieved: achieved(),
                        io_count: 256,
                        iterations: 100,
                        submitted: Some(SubmissionCounts {
                            submissions: 4,
                            entries: 256,
                        }),
                        shape: matched(),
                    },
                },
            ],
        });

        let rendered = account.render();
        assert!(
            rendered.contains("batching: not applicable (no ring)"),
            "{rendered}"
        );
        assert!(
            rendered
                .contains("batching: 64.0 entries per submission (256 entries over 4 submissions)"),
            "{rendered}"
        );
    }

    /// A median is not a mean, and the difference is the point of using one.
    ///
    /// The sample set here is the shape the probe actually met: a tight cluster
    /// with two stalls an order of magnitude out. Its mean is 128us and its
    /// median 15us, and it was a stall of exactly this kind that aborted an
    /// honest run when the probe divided a whole-loop elapsed. Asserting on the
    /// mean too, so this test fails if someone "simplifies" the estimator back.
    #[test]
    fn one_stalled_open_moves_the_mean_and_not_the_median() {
        let mut samples: Vec<Duration> = (0..18).map(|_| Duration::from_micros(15)).collect();
        samples.push(Duration::from_micros(1_150));
        samples.push(Duration::from_micros(1_150));

        let mean = samples.iter().sum::<Duration>() / u32::try_from(samples.len()).unwrap();
        let (median, p90) = OpenCost::summarise(&mut samples).expect("a non-empty sample set");

        assert_eq!(
            median,
            Duration::from_micros(15),
            "the median ignores the stalls"
        );
        assert_eq!(
            p90,
            Duration::from_micros(1_150),
            "the p90 of twenty samples is the nineteenth, which is a stall — the spread is \
             reported rather than smoothed away"
        );
        assert!(
            mean > median * 7,
            "the mean of this set is {mean:?} against a median of {median:?}; if these were \
             close the sample set would no longer be testing what it was built to test"
        );
    }

    /// An empty sample set has no median, and inventing one would be worse.
    #[test]
    fn an_empty_sample_set_has_no_summary() {
        assert!(OpenCost::summarise(&mut []).is_none());
    }

    /// The reported figures are the ones the run measured, in the right slots.
    ///
    /// This is the test the swap mutation motivated. The probe's own assertion
    /// runs only on the benchmark path, so nothing under `cargo test` would
    /// otherwise notice the two figures being rendered into each other's places
    /// — which is the same defect one layer below the one the assertion caught.
    #[test]
    fn the_open_cost_is_rendered_with_each_figure_in_its_own_place() {
        let mut account = account();
        account.test_mode = false;
        account.open_cost = Some(OpenCost {
            std_median: Duration::from_micros(15),
            compio_median: Duration::from_micros(53),
            std_p90: Duration::from_micros(17),
            compio_p90: Duration::from_micros(84),
        });

        let rendered = account.render();

        assert!(
            rendered.contains("std 15.0us (p90 17.0us), compio 53.0us (p90 84.0us)"),
            "each figure must render in its own slot; got:\n{rendered}"
        );
        assert!(
            rendered.contains("3.5x, 38.0us more per open"),
            "the ratio and the extra are derived from the medians; got:\n{rendered}"
        );
        assert!(
            rendered.contains("anti-conservative where it is reported to scale well"),
            "the direction of the bias travels with the number, not only with the docs"
        );
    }

    /// A probe that failed and a probe that was never run are different facts.
    ///
    /// Both were rendered as absence once — no line at all — which made a broken
    /// measurement on the benchmark path indistinguishable from a test run in
    /// the published artifact.
    #[test]
    fn a_failed_probe_is_not_rendered_the_same_as_one_that_was_never_attempted() {
        let mut skipped = account();
        skipped.test_mode = true;
        let skipped = skipped.render();

        let mut failed = account();
        failed.test_mode = false;
        let failed = failed.render();

        assert!(
            skipped.contains("- one file open: not measured — a test run"),
            "a test run says why it has no figure; got:\n{skipped}"
        );
        assert!(
            failed.contains("- one file open: **NOT MEASURED**"),
            "a failed probe is marked, not omitted; got:\n{failed}"
        );
        assert_ne!(
            skipped.contains("**NOT MEASURED**"),
            failed.contains("**NOT MEASURED**"),
            "the two cases must be distinguishable in the artifact"
        );
    }

    /// A reversal would be a finding, so it must render as one rather than wrap.
    ///
    /// Unreachable through the probe, whose assertion panics first. Tested here
    /// because a saturating subtraction that is never exercised is a claim about
    /// behaviour rather than a demonstration of it.
    #[test]
    fn a_faster_compio_open_saturates_rather_than_wrapping() {
        let reversed = OpenCost {
            std_median: Duration::from_micros(50),
            compio_median: Duration::from_micros(15),
            std_p90: Duration::from_micros(60),
            compio_p90: Duration::from_micros(20),
        };

        assert_eq!(
            reversed.extra(),
            Duration::ZERO,
            "no wrap to a vast duration"
        );
        assert!(
            reversed.ratio() < 1.0,
            "the ratio is what shows the reversal the extra cannot: {}",
            reversed.ratio()
        );
    }

    /// A zero denominator is a reporting path, not a place to panic.
    #[test]
    fn a_zero_std_median_yields_a_zero_ratio_rather_than_an_infinity() {
        let impossible = OpenCost {
            std_median: Duration::ZERO,
            compio_median: Duration::from_micros(15),
            std_p90: Duration::ZERO,
            compio_p90: Duration::from_micros(15),
        };

        assert_eq!(impossible.ratio(), 0.0);
        assert!(impossible.ratio().is_finite());
    }
}
