//! Recording what a run did, so two runs can be compared.
//!
//! A benchmark is only worth quoting if the things being compared did the same
//! work. Two properties carry that here:
//!
//! - the **issue trace**: every operation, in the order it was issued. Issue
//!   order is the scenario's own and is deterministic; *completion* order is
//!   legitimately nondeterministic above one operation in flight, so it is
//!   deliberately not compared.
//! - the **delivery digest**: a fold over what each operation actually put into
//!   application-visible memory. Folded commutatively — the per-operation hashes
//!   are combined with exclusive-or — so completion order cannot enter it.
//!
//! A backend that issues fewer operations, or delivers different bytes, differs
//! in one of these and the run is rejected rather than reported.

use std::fmt;

/// One issued operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Issued {
    offset: u64,
    len: u32,
}

/// What a run issued and delivered.
#[derive(Debug, Default, Clone)]
pub struct Trace {
    issued: Vec<Issued>,
    digest: u64,
    delivered_bytes: u64,
    completions: usize,
}

impl Trace {
    /// Starts an empty trace.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that an operation was issued.
    pub fn issued(&mut self, offset: u64, len: u32) {
        self.issued.push(Issued { offset, len });
    }

    /// Records what an operation delivered.
    ///
    /// `bytes` is what the application can actually read — not what the backend
    /// claims it transferred — so a backend that reports a count without putting
    /// the data anywhere reachable produces a different digest.
    pub fn delivered(&mut self, offset: u64, transferred: u32, bytes: &[u8]) {
        let mut h = fnv1a(0xcbf2_9ce4_8422_2325);
        h = h.feed_u64(offset).feed_u64(transferred as u64);
        // Only the bytes this operation actually transferred: a backend whose
        // buffer carries stale trailing content must not be charged for it.
        let take = (transferred as usize).min(bytes.len());
        h = h.feed(&bytes[..take]);
        // Exclusive-or, so the order completions arrive in cannot change the
        // result.
        self.digest ^= h.0;
        self.delivered_bytes += take as u64;
        self.completions += 1;
    }

    /// How many operations were issued.
    pub fn operations(&self) -> usize {
        self.issued.len()
    }

    /// How many bytes reached application-visible memory.
    pub fn delivered_total(&self) -> u64 {
        self.delivered_bytes
    }

    /// Compares this trace with another, describing the first difference.
    ///
    /// Returns `Ok(())` when the two runs did the same work.
    pub fn agrees_with(&self, other: &Trace) -> Result<(), Mismatch> {
        if self.issued.len() != other.issued.len() {
            return Err(Mismatch::OperationCount {
                left: self.issued.len(),
                right: other.issued.len(),
            });
        }
        for (i, (a, b)) in self.issued.iter().zip(&other.issued).enumerate() {
            if a != b {
                return Err(Mismatch::IssueOrder {
                    index: i,
                    left: format!("{}+{}", a.offset, a.len),
                    right: format!("{}+{}", b.offset, b.len),
                });
            }
        }
        if self.completions != other.completions {
            return Err(Mismatch::CompletionCount {
                left: self.completions,
                right: other.completions,
            });
        }
        if self.digest != other.digest {
            return Err(Mismatch::Delivered {
                left: self.digest,
                right: other.digest,
            });
        }
        Ok(())
    }
}

/// How two runs differed.
#[derive(Debug, Clone)]
pub enum Mismatch {
    /// One run issued a different number of operations.
    OperationCount { left: usize, right: usize },
    /// The two runs issued different operations, or in a different order.
    IssueOrder {
        index: usize,
        left: String,
        right: String,
    },
    /// One run completed a different number of operations.
    CompletionCount { left: usize, right: usize },
    /// The two runs delivered different bytes.
    Delivered { left: u64, right: u64 },
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Mismatch::OperationCount { left, right } => {
                write!(f, "issued {left} operations against {right}")
            }
            Mismatch::IssueOrder { index, left, right } => {
                write!(f, "operation {index} was {left} against {right}")
            }
            Mismatch::CompletionCount { left, right } => {
                write!(f, "completed {left} operations against {right}")
            }
            Mismatch::Delivered { left, right } => {
                write!(f, "delivered digest {left:#x} against {right:#x}")
            }
        }
    }
}

/// A small non-cryptographic hash, enough to notice different bytes.
#[derive(Clone, Copy)]
struct Fnv(u64);

fn fnv1a(seed: u64) -> Fnv {
    Fnv(seed)
}

impl Fnv {
    fn feed(mut self, bytes: &[u8]) -> Self {
        for &b in bytes {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01B3);
        }
        self
    }

    fn feed_u64(self, value: u64) -> Self {
        self.feed(&value.to_le_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_runs_agree() {
        let mut a = Trace::new();
        let mut b = Trace::new();
        for t in [&mut a, &mut b] {
            t.issued(0, 4);
            t.issued(4, 4);
        }
        a.delivered(0, 4, b"abcd");
        a.delivered(4, 4, b"efgh");
        // Delivered in the opposite order: the digest must not care.
        b.delivered(4, 4, b"efgh");
        b.delivered(0, 4, b"abcd");
        assert!(a.agrees_with(&b).is_ok());
    }

    #[test]
    fn a_run_that_issued_fewer_operations_is_caught() {
        let mut a = Trace::new();
        let mut b = Trace::new();
        a.issued(0, 4);
        a.issued(4, 4);
        b.issued(0, 4);
        assert!(matches!(
            a.agrees_with(&b),
            Err(Mismatch::OperationCount { left: 2, right: 1 })
        ));
    }

    #[test]
    fn a_run_that_issued_a_different_order_is_caught() {
        let mut a = Trace::new();
        let mut b = Trace::new();
        a.issued(0, 4);
        a.issued(4, 4);
        b.issued(4, 4);
        b.issued(0, 4);
        assert!(matches!(
            a.agrees_with(&b),
            Err(Mismatch::IssueOrder { index: 0, .. })
        ));
    }

    #[test]
    fn a_run_that_delivered_different_bytes_is_caught() {
        let mut a = Trace::new();
        let mut b = Trace::new();
        a.issued(0, 4);
        b.issued(0, 4);
        a.delivered(0, 4, b"abcd");
        b.delivered(0, 4, b"abcX");
        assert!(matches!(a.agrees_with(&b), Err(Mismatch::Delivered { .. })));
    }

    /// The hazard the whole design exists to catch: a backend that reports a
    /// transfer without putting the bytes anywhere the application can read.
    #[test]
    fn a_run_that_delivered_nothing_readable_is_caught() {
        let mut a = Trace::new();
        let mut b = Trace::new();
        a.issued(0, 4);
        b.issued(0, 4);
        a.delivered(0, 4, b"abcd");
        b.delivered(0, 4, b"");
        assert!(a.agrees_with(&b).is_err());
    }
}
