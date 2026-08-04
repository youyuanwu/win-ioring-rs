//! The comparator, called from a measurement rather than beside one.
//!
//! Every backend measured at one (scenario, depth) must have done the same work,
//! or the fast one is fast for a reason that has nothing to do with its I/O
//! path. The [`Ledger`] remembers the first backend to run each combination and
//! compares every later one against it.
//!
//! # The ledger is owned by the caller, and that is load-bearing
//!
//! One `Ledger` is constructed per (scenario, depth) and **shared across that
//! combination's backends**. It is never constructed inside the measurement.
//! The alternative compiles and silently destroys the check: a ledger created
//! per call makes every backend the first observation for its own key, every
//! comparison a comparison with itself, and [`Ledger::observe`] can then never
//! return `Err` — with no build failure and no test failure to notice it.
//! `tests/comparison.rs`'s `a_backend_that_ran_a_different_job_is_rejected` is
//! the standing gate against exactly that.

use std::fmt;

use crate::concurrency::Depth;
use crate::scenario::Scenario;
use crate::verify::{Mismatch, Trace};

/// The first backend to run one (scenario, depth), and what it did.
struct Reference {
    scenario: Scenario,
    depth: Depth,
    backend: String,
    trace: Trace,
}

/// The reference traces observed so far, and the comparison against them.
#[derive(Default)]
pub struct Ledger {
    /// A list rather than a map: a ledger holds one entry per (scenario, depth)
    /// it has seen, which is one in normal use and never more than a handful.
    references: Vec<Reference>,
}

impl Ledger {
    /// An empty ledger.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records what `backend` did, or reports that it disagreed with the
    /// backend that ran this combination first.
    ///
    /// The first observation for a (scenario, depth) becomes the reference and
    /// always succeeds; there is nothing yet to disagree with.
    pub fn observe(
        &mut self,
        scenario: Scenario,
        depth: Depth,
        backend: &str,
        trace: &Trace,
    ) -> Result<(), FairnessFailure> {
        match self
            .references
            .iter()
            .find(|r| r.scenario == scenario && r.depth == depth)
        {
            Some(reference) => {
                reference
                    .trace
                    .agrees_with(trace)
                    .map_err(|mismatch| FairnessFailure {
                        scenario,
                        depth,
                        reference: reference.backend.clone(),
                        backend: backend.to_owned(),
                        mismatch,
                    })
            }
            None => {
                self.references.push(Reference {
                    scenario,
                    depth,
                    backend: backend.to_owned(),
                    trace: trace.clone(),
                });
                Ok(())
            }
        }
    }

    /// The backend that ran this combination first, if any has.
    ///
    /// Used by tests asserting that a combination which never produced a
    /// measurement left no reference behind.
    pub fn reference(&self, scenario: Scenario, depth: Depth) -> Option<&str> {
        self.references
            .iter()
            .find(|r| r.scenario == scenario && r.depth == depth)
            .map(|r| r.backend.as_str())
    }
}

/// Two backends did not do the same work.
///
/// Fatal to a run: a backend that did less is rejected, not reported.
#[derive(Debug)]
pub struct FairnessFailure {
    /// Which scenario they disagreed on.
    pub scenario: Scenario,
    /// At which depth.
    pub depth: Depth,
    /// The backend that ran the combination first.
    pub reference: String,
    /// The backend that disagreed with it.
    pub backend: String,
    /// The first difference between the two traces.
    pub mismatch: Mismatch,
}

impl fmt::Display for FairnessFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "at {} depth {}: {} did not do the same work as {}: {}",
            self.scenario.name(),
            self.depth,
            self.backend,
            self.reference,
            self.mismatch
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify::Phase;

    fn trace_of(operations: u64) -> Trace {
        let mut trace = Trace::new();
        for i in 0..operations {
            trace.issued(i * 64, 64);
        }
        for i in 0..operations {
            trace.delivered(Phase::Read, i * 64, 64, &[7_u8; 64]);
        }
        trace
    }

    #[test]
    fn the_first_observation_becomes_the_reference() {
        let mut ledger = Ledger::new();
        assert!(
            ledger
                .observe(Scenario::SequentialRead, 1, "first", &trace_of(4))
                .is_ok()
        );
        assert_eq!(ledger.reference(Scenario::SequentialRead, 1), Some("first"));
    }

    #[test]
    fn a_matching_trace_agrees() {
        let mut ledger = Ledger::new();
        ledger
            .observe(Scenario::SequentialRead, 1, "first", &trace_of(4))
            .unwrap();
        assert!(
            ledger
                .observe(Scenario::SequentialRead, 1, "second", &trace_of(4))
                .is_ok()
        );
    }

    #[test]
    fn a_differing_trace_is_rejected() {
        let mut ledger = Ledger::new();
        ledger
            .observe(Scenario::SequentialRead, 1, "first", &trace_of(4))
            .unwrap();
        let failure = ledger
            .observe(Scenario::SequentialRead, 1, "second", &trace_of(2))
            .expect_err("half the operations is not the same work");
        assert!(matches!(failure.mismatch, Mismatch::OperationCount { .. }));
        assert_eq!(failure.reference, "first");
        assert_eq!(failure.backend, "second");
    }

    #[test]
    fn two_combinations_do_not_contaminate_each_other() {
        let mut ledger = Ledger::new();
        ledger
            .observe(Scenario::SequentialRead, 1, "first", &trace_of(4))
            .unwrap();
        // A different depth, and a different scenario, are different keys: each
        // is its own first observation rather than a disagreement with the one
        // above.
        ledger
            .observe(Scenario::SequentialRead, 8, "first", &trace_of(2))
            .expect("a different depth is a different combination");
        ledger
            .observe(Scenario::RandomRead, 1, "first", &trace_of(9))
            .expect("a different scenario is a different combination");
        assert_eq!(ledger.reference(Scenario::RandomRead, 8), None);
    }
}
