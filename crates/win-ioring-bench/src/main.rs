//! Runs the comparison and prints the table.
//!
//! ```text
//! cargo run -p win-ioring-bench --release
//! cargo run -p win-ioring-bench --release -- --clean
//! ```

use std::io;
use std::time::Instant;

use win_ioring_bench::backend::Availability;
use win_ioring_bench::backends::ioring;
use win_ioring_bench::config::Config;
use win_ioring_bench::fairness::Ledger;
use win_ioring_bench::harness::{Job, Which, rotated_order, run_one};
use win_ioring_bench::measure::Cell;
use win_ioring_bench::report::{Report, Row};
use win_ioring_bench::scenario::Scenario;
use win_ioring_bench::workload;

fn main() -> io::Result<()> {
    if std::env::args().any(|a| a == "--clean") {
        workload::clean()?;
        println!("working files removed");
        return Ok(());
    }

    let config = Config::default();
    let started = Instant::now();

    let dir = workload::data_dir();
    let read_path = dir.join("read.dat");
    let write_path = dir.join("write.dat");

    eprintln!("preparing working files under {}...", dir.display());
    workload::ensure_file(&read_path, config.read_file_bytes)?;
    eprintln!("warming the page cache...");
    workload::warm(&read_path)?;

    let mut unavailable = Vec::new();
    let ring_available = match ioring::availability() {
        Availability::Available => true,
        Availability::Unavailable(reason) => {
            unavailable.push(("win-ioring".to_owned(), reason));
            false
        }
    };

    let mut rows = Vec::new();
    let mut configurations: Vec<(String, String)> = Vec::new();
    let mut rotation = 0_usize;

    for scenario in Scenario::all() {
        let (block, operations) = config.shape(scenario);

        for &depth in &config.depths {
            eprintln!("{} at depth {depth}...", scenario.name());

            let order = rotated_order(rotation);
            rotation += 1;

            let mut cells = Vec::new();
            // One ledger per (scenario, depth), shared across that
            // combination's backends. A ledger per backend would make every
            // backend its own reference and no disagreement could be reported.
            let mut ledger = Ledger::new();

            for which in order {
                if !ring_available && matches!(which, Which::RingPlain | Which::RingRegistered) {
                    continue;
                }
                let job = Job {
                    scenario,
                    read_path: &read_path,
                    write_path: &write_path,
                    block,
                    operations,
                    depth,
                };
                let run = run_one(which, &config, &job, &mut ledger);
                if !configurations.iter().any(|(n, _)| n == &run.name) {
                    configurations.push((run.name.clone(), run.configuration.clone()));
                }

                // A backend that did different work is rejected, not reported.
                // This is the check that stops one looking fast by delivering
                // less.
                if let Err(failure) = run.fairness {
                    eprintln!("FAIRNESS FAILURE {failure}");
                    std::process::exit(1);
                }

                match run.measured {
                    Ok(measured) => cells.push(Cell {
                        backend: run.name,
                        samples: measured.samples,
                        achieved: measured.achieved,
                        failure: None,
                    }),
                    Err(e) => cells.push(Cell::failed(run.name, e.to_string())),
                }
            }

            // Stable presentation regardless of the order they ran in.
            cells.sort_by(|a, b| a.backend.cmp(&b.backend));
            rows.push(Row {
                scenario: scenario.name(),
                depth,
                operations,
                cells,
            });
        }
    }

    let working_set = config.read_file_bytes + config.write_file_bytes;
    let report = Report {
        cache: workload::cache_premise(working_set),
        volume: dir.to_string_lossy().into_owned(),
        run_order: "rotated per scenario and depth".to_owned(),
        configurations,
        unavailable,
        rows,
        config,
        elapsed: started.elapsed(),
    };
    println!("{}", report.render());
    Ok(())
}
