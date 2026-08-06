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
//! That last claim is settled rather than asserted: [`weaken::Weakened`] is a
//! backend that really does less, and `tests/fairness.rs` drives it through
//! [`harness::measure_combination`] — the same function a measurement calls —
//! and watches the run fail.
//!
//! # One entry point
//!
//! This crate has no binary. Its measurements run under Criterion, through
//! `benches/comparison.rs`, invoked as `cargo bench -p win-ioring-bench`. That
//! is deliberate: a second entry point of our own would be a second timing
//! implementation to keep honest, and the reason to adopt a measurement
//! framework at all is to stop maintaining one. Criterion supplies the
//! statistics — warm-up, sampling, outlier classification, intervals, and
//! comparison against a stored baseline — and this crate supplies what a timing
//! cannot carry: [`account::Account`] records the achieved concurrency, the
//! cache premise, the run order, the backend availability and the fairness
//! verdict beside every figure, and writes them to `target/bench-data/`.
//!
//! See `docs/performance.md` for what the measurements do and do not tell you.

pub mod account;
pub mod align;
pub mod aligned;
pub mod backend;
pub mod backends;
pub mod concurrency;
pub mod config;
pub mod fairness;
pub mod harness;
pub mod scenario;
pub mod session;
// Deliberately carries no `///` comment here. The module's own `//!` header
// documents it at length, and an outer doc comment on the declaration makes
// that header's intra-doc links resolve in *this* module's scope, where the
// types it names are absent — which fails `cargo doc -D warnings` in CI.
pub mod unbuffered;
pub mod unbuffered_workload;
pub mod verify;
pub mod weaken;
pub mod workload;
