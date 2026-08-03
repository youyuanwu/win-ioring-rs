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
//! # The seam
//!
//! [`harness::measure_combination`] is the one function that prepares a backend,
//! warms it, measures it and consults the comparator, and everything that
//! measures anything calls it. A [`session::Prepared`] holds the backend and,
//! for the ring backends, the driver that must outlive every iteration; a
//! [`harness::Timer`] decides only *how* the iterations are timed; and a
//! [`fairness::Ledger`], owned by the caller and shared across one (scenario,
//! depth)'s backends, is what every trace is put in front of. Because there is
//! exactly one call site for the comparator, removing it breaks a test rather
//! than quietly removing the check.
//!
//! See `docs/performance.md` for what the measurements do and do not tell you.

pub mod backend;
pub mod backends;
pub mod concurrency;
pub mod config;
pub mod fairness;
pub mod harness;
pub mod measure;
pub mod report;
pub mod scenario;
pub mod session;
pub mod verify;
pub mod workload;
