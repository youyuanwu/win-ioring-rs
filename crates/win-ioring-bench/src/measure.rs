//! Measuring, and assembling the results.

use std::time::Duration;

use crate::concurrency::{Achieved, Shortfall};

/// What one backend achieved on one scenario at one depth.
pub struct Cell {
    /// The backend's name.
    pub backend: String,
    /// Timings from the measured repeats, in issue order.
    pub samples: Vec<Duration>,
    /// What concurrency the last repeat achieved.
    pub achieved: Achieved,
    /// Why no timing was produced, if none was.
    pub failure: Option<String>,
}

impl Cell {
    /// A cell for a backend that failed partway.
    ///
    /// A failure is reported as a failure, never as a fast time: a backend that
    /// gave up early would otherwise look like the winner.
    pub fn failed(backend: String, reason: String) -> Self {
        Self {
            backend,
            samples: Vec::new(),
            achieved: Achieved {
                peak: 0,
                mean: 0.0,
                shortfall: Shortfall::None,
            },
            failure: Some(reason),
        }
    }

    /// The median of the measured repeats.
    pub fn median(&self) -> Option<Duration> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        Some(sorted[sorted.len() / 2])
    }

    /// The fastest and slowest repeats, which is the dispersion a reader needs
    /// to judge whether two backends actually differ.
    pub fn spread(&self) -> Option<(Duration, Duration)> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        Some((sorted[0], sorted[sorted.len() - 1]))
    }
}

/// Times one closure, discarding a warm-up repeat first.
///
/// Only the measured region is timed: preparation, warm-up and verification all
/// happen outside it.
pub fn measure<F>(repeats: usize, mut once: F) -> std::io::Result<Vec<Duration>>
where
    F: FnMut() -> std::io::Result<Duration>,
{
    // Discarded: it pays for lazily created threads, first-touch page faults,
    // and anything else a backend defers until first use.
    once()?;
    let mut samples = Vec::with_capacity(repeats);
    for _ in 0..repeats {
        samples.push(once()?);
    }
    Ok(samples)
}

/// What a set of measured repeats produced.
pub struct Measured {
    /// One timing per measured repeat.
    pub samples: Vec<Duration>,
    /// What the last repeat achieved, in concurrency terms.
    pub achieved: crate::concurrency::Achieved,
    /// What the last repeat issued and delivered.
    pub trace: crate::verify::Trace,
}
