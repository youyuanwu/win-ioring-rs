//! Keeping a bounded number of operations in flight, and observing how many
//! actually were.
//!
//! The achieved depth is measured here rather than asked of a backend. Only this
//! layer knows the figure in terms both backends share: one exposes an
//! outstanding count that includes work its kernel has not accepted, and the
//! other exposes nothing comparable at all.
//!
//! What this **cannot** see is a backend serialising operations below its own
//! interface — no available instrument can — so the report prints the depth
//! beside each backend's configuration rather than letting it stand alone.

use std::io;

use futures::stream::{FuturesUnordered, StreamExt};

use crate::backend::{Backend, Buffer};
use crate::verify::Trace;

/// How many operations may be outstanding at once.
pub type Depth = usize;

/// Why a run did not reach its configured depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shortfall {
    /// The configured depth was reached.
    None,
    /// Fewer operations exist than the depth allows, or the backend's pool is
    /// smaller than it. Expected, and no reflection on the backend.
    Expected,
    /// The backend did not sustain a depth it had the work and the buffers for.
    /// The one that casts doubt on a comparison.
    Unexpected,
}

/// What a run achieved, in terms of concurrency.
#[derive(Debug, Clone, Copy)]
pub struct Achieved {
    /// The greatest number outstanding at once.
    pub peak: usize,
    /// Mean depth, weighted by how long each depth was held rather than by how
    /// often it changed.
    pub mean: f64,
    /// How the run fell short, if it did.
    pub shortfall: Shortfall,
}

/// Drives a bounded set of operations, recording what completed.
pub struct Runner<'a, B: Backend> {
    backend: &'a B,
    configured: Depth,
    peak: usize,
    weighted: u64,
    samples: u64,
    starved: bool,
    /// Set when the pool could not supply a buffer, which bounds depth for a
    /// reason belonging to the configuration rather than to the backend.
    pool_bound: bool,
}

impl<'a, B: Backend> Runner<'a, B> {
    /// Starts a runner bounded at `depth`.
    pub fn new(backend: &'a B, depth: Depth) -> Self {
        Self {
            backend,
            configured: depth.max(1),
            peak: 0,
            weighted: 0,
            samples: 0,
            starved: false,
            pool_bound: false,
        }
    }

    /// Runs `count` operations, keeping the configured number outstanding.
    ///
    /// `make` produces the *n*th operation as a future yielding its file offset
    /// alongside its outcome, so a completion can be matched to what was asked
    /// for without depending on the order completions arrive in.
    ///
    /// A `WouldBlock` from `make` means the backend's pool is momentarily empty;
    /// the runner drains one operation and tries again, and records that depth
    /// was bounded by the pool.
    pub async fn run<F, Fut>(
        &mut self,
        count: usize,
        phase: crate::verify::Phase,
        trace: &mut Trace,
        mut make: F,
    ) -> io::Result<()>
    where
        F: FnMut(usize) -> io::Result<Fut>,
        Fut: Future<Output = (u64, io::Result<(u32, B::Buf)>)>,
    {
        let mut pending = FuturesUnordered::new();
        let mut issued = 0;
        loop {
            while issued < count && pending.len() < self.configured {
                match make(issued) {
                    Ok(fut) => {
                        pending.push(fut);
                        issued += 1;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock && !pending.is_empty() => {
                        self.pool_bound = true;
                        break;
                    }
                    Err(e) => return Err(e),
                }
            }
            if pending.is_empty() {
                break;
            }
            let outstanding = pending.len();
            self.peak = self.peak.max(outstanding);
            if issued >= count && outstanding < self.configured {
                self.starved = true;
            }
            let Some((offset, outcome)) = pending.next().await else {
                break;
            };
            self.weighted += outstanding as u64;
            self.samples += 1;
            let (transferred, buffer) = outcome?;
            trace.delivered(phase, offset, transferred, buffer.bytes());
            self.backend.put_buffer(buffer);
        }
        Ok(())
    }

    /// Returns what the run achieved.
    pub fn achieved(&self, operations: usize) -> Achieved {
        let shortfall = if self.peak >= self.configured {
            Shortfall::None
        } else if self.starved || self.pool_bound || operations < self.configured {
            Shortfall::Expected
        } else {
            Shortfall::Unexpected
        };
        Achieved {
            peak: self.peak,
            mean: if self.samples == 0 {
                0.0
            } else {
                self.weighted as f64 / self.samples as f64
            },
            shortfall,
        }
    }
}
