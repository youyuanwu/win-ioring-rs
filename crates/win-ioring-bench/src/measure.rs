//! Measuring, and assembling the results.
//!
//! This module is the hand-rolled timing layer: a fixed number of repeats, a
//! median and a min/max. It is deliberately visible as such, because it is the
//! part an established statistics implementation replaces.

use std::time::{Duration, Instant};

use crate::concurrency::{Achieved, Shortfall};
use crate::harness::{Timed, Timer};
use crate::session::Prepared;

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

/// Times a fixed number of iterations with [`Instant`], collecting a sample per
/// iteration.
///
/// The warm-up is not here: it belongs to `measure_combination`, so every timer
/// gets one and none has to remember to discard a repeat of its own.
pub struct Repeats {
    repeats: usize,
    samples: Vec<Duration>,
}

impl Repeats {
    /// A timer that will run `repeats` measured iterations.
    pub fn new(repeats: usize) -> Self {
        Self {
            repeats,
            samples: Vec::with_capacity(repeats),
        }
    }

    /// The timings collected, in issue order.
    pub fn into_samples(self) -> Vec<Duration> {
        self.samples
    }
}

impl Timer for Repeats {
    fn time<F, Fut>(&mut self, _timed: &Timed, prepared: &Prepared, mut one: F)
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = ()>,
    {
        let repeats = self.repeats;
        let samples = &mut self.samples;
        // One `block_on` around the whole loop rather than one per iteration:
        // the ring backends' driver is pumped by that call, and restarting it
        // per iteration would abandon a park between every pair of them.
        prepared.block_on(async move {
            for _ in 0..repeats {
                let started = Instant::now();
                one().await;
                samples.push(started.elapsed());
            }
        });
    }
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
