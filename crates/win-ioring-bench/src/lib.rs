//! Comparing this crate's IoRing backend against the thread-pool-backed file
//! API, on identical application logic.
//!
//! The obvious way to benchmark two I/O layers is to write two benchmarks, and
//! the obvious problem is that they are never quite doing the same work. This is
//! arranged the other way round: one piece of application logic, written against
//! [`backend::Backend`], executed unmodified against each implementation. Any
//! difference in the numbers is therefore attributable to the backend rather
//! than to the benchmark.
//!
//! Fairness is checked rather than asserted. [`verify::Trace`] records every
//! operation issued and folds what each delivered into a digest, and a run whose
//! trace disagrees with the others is **rejected instead of reported** — so a
//! backend cannot look fast by issuing fewer operations or by putting the bytes
//! somewhere the application cannot read them.
//!
//! See `docs/performance.md` for what the measurements do and do not tell you.

pub mod backend;
pub mod backends;
pub mod concurrency;
pub mod config;
pub mod harness;
pub mod measure;
pub mod report;
pub mod scenario;
pub mod verify;
pub mod workload;
