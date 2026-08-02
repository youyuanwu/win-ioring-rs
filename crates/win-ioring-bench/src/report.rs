//! Printing the results, with the conditions that make them mean something.

use std::fmt::Write as _;
use std::time::Duration;

use crate::concurrency::Shortfall;
use crate::config::Config;
use crate::measure::Cell;
use crate::workload::CachePremise;

/// One scenario at one depth, across every backend.
pub struct Row {
    /// The scenario's name.
    pub scenario: &'static str,
    /// The configured in-flight depth.
    pub depth: usize,
    /// How many operations each backend performed.
    pub operations: usize,
    /// One cell per backend, in the order they were run.
    pub cells: Vec<Cell>,
}

/// Everything the run produced.
pub struct Report {
    /// The parameters used.
    pub config: Config,
    /// The results.
    pub rows: Vec<Row>,
    /// Each backend's configuration, in report order.
    pub configurations: Vec<(String, String)>,
    /// Backends that could not run here, and why.
    pub unavailable: Vec<(String, String)>,
    /// Whether the warm-cache premise held.
    pub cache: CachePremise,
    /// The order backends were run in, per repeat.
    pub run_order: String,
    /// How long the whole run took.
    pub elapsed: Duration,
    /// The volume the working files sit on.
    pub volume: String,
}

fn micros(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

impl Report {
    /// Renders the report.
    ///
    /// The reference backend is the first one, and every other figure is also
    /// given relative to it, so a reader is not left computing ratios.
    pub fn render(&self) -> String {
        let mut out = String::new();

        writeln!(out, "# File I/O comparison").unwrap();
        writeln!(out).unwrap();
        self.render_conditions(&mut out);
        writeln!(out).unwrap();

        for row in &self.rows {
            writeln!(
                out,
                "## {} — depth {}, {} operations",
                row.scenario, row.depth, row.operations
            )
            .unwrap();
            writeln!(out).unwrap();
            writeln!(
                out,
                "{:<32} {:>12} {:>12} {:>12} {:>10} {:>18}",
                "backend", "median µs", "min µs", "max µs", "relative", "achieved depth"
            )
            .unwrap();

            let reference = row.cells.first().and_then(|c| c.median());
            for cell in &row.cells {
                if let Some(reason) = &cell.failure {
                    writeln!(out, "{:<32} {:>12}  {reason}", cell.backend, "FAILED").unwrap();
                    continue;
                }
                let median = cell.median().unwrap_or_default();
                let (min, max) = cell.spread().unwrap_or_default();
                let relative = match reference {
                    Some(r) if r.as_secs_f64() > 0.0 => {
                        format!("{:.2}x", median.as_secs_f64() / r.as_secs_f64())
                    }
                    _ => "-".to_owned(),
                };
                let depth = match cell.achieved.shortfall {
                    Shortfall::None => format!("{:.1}", cell.achieved.mean),
                    Shortfall::Expected => {
                        format!("{:.1} (short, expected)", cell.achieved.mean)
                    }
                    Shortfall::Unexpected => {
                        format!("{:.1} (SHORT)", cell.achieved.mean)
                    }
                };
                writeln!(
                    out,
                    "{:<32} {:>12.1} {:>12.1} {:>12.1} {:>10} {:>18}",
                    cell.backend,
                    micros(median),
                    micros(min),
                    micros(max),
                    relative,
                    depth
                )
                .unwrap();
            }
            writeln!(out).unwrap();
        }

        out
    }

    fn render_conditions(&self, out: &mut String) {
        writeln!(out, "## Conditions").unwrap();
        writeln!(out).unwrap();
        writeln!(
            out,
            "These figures measure **per-operation software overhead against a warm page"
        )
        .unwrap();
        writeln!(
            out,
            "cache**. They are not device throughput, and must not be quoted as such."
        )
        .unwrap();
        match &self.cache {
            CachePremise::Holds { total } => writeln!(
                out,
                "- warm cache: working set is within a quarter of {} MiB physical memory",
                total / (1024 * 1024)
            )
            .unwrap(),
            CachePremise::Doubtful { total } => writeln!(
                out,
                "- warm cache: **PREMISE DOUBTFUL** — working set is large relative to {} MiB \
                 physical memory, so these figures may include device I/O",
                total / (1024 * 1024)
            )
            .unwrap(),
            CachePremise::Unknown => {
                writeln!(out, "- warm cache: physical memory could not be determined").unwrap()
            }
        }
        writeln!(
            out,
            "- read file: {} MiB, sequential in {} KiB blocks, random in {} KiB blocks",
            self.config.read_file_bytes / (1024 * 1024),
            self.config.sequential_block / 1024,
            self.config.random_block / 1024
        )
        .unwrap();
        writeln!(
            out,
            "- write-then-read: {} MiB in {} KiB blocks, committed before read-back",
            self.config.write_file_bytes / (1024 * 1024),
            self.config.write_block / 1024
        )
        .unwrap();
        writeln!(
            out,
            "- repeats: {} measured, after one discarded warm-up",
            self.config.repeats
        )
        .unwrap();
        writeln!(
            out,
            "- setup — ring, runtime, registration and buffer pool — is built once per scenario \
             and depth, and is outside every timed region. Files are reopened per repeat. \
             Registration cost is therefore **not** included in any figure below."
        )
        .unwrap();
        writeln!(out, "- working files on: {}", self.volume).unwrap();
        writeln!(out, "- run order: {}", self.run_order).unwrap();
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
            "- total harness duration: {:.1}s",
            self.elapsed.as_secs_f64()
        )
        .unwrap();
        writeln!(out).unwrap();
        writeln!(out, "### Backends").unwrap();
        writeln!(out).unwrap();
        for (name, configuration) in &self.configurations {
            writeln!(out, "- **{name}** — {configuration}").unwrap();
        }
        if !self.unavailable.is_empty() {
            writeln!(out).unwrap();
            writeln!(out, "### Unavailable on this host").unwrap();
            writeln!(out).unwrap();
            for (name, reason) in &self.unavailable {
                writeln!(out, "- **{name}** — {reason}").unwrap();
            }
        }
        writeln!(out).unwrap();
        writeln!(
            out,
            "Achieved depth is measured by the harness, so it cannot see a backend serialising"
        )
        .unwrap();
        writeln!(
            out,
            "operations below its own interface. Read it beside the backend's configuration."
        )
        .unwrap();
    }
}
