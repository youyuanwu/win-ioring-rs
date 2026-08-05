# Pending work

Known limitations, deferred work, and observations from review that were judged
non-blocking. Nothing here is a known soundness defect; where safety is relevant
it says so explicitly.

Roughly ordered by value.

## Deferred features

### Owned buffers cannot be combined with registered handles

A registered file handle is reachable only from `read_registered` /
`write_registered`, which *force* a registered buffer. So the two registration
kinds cannot be mixed: a caller with registered handles but caller-owned buffers
has to pass a raw `File`. `flush` cannot name a registered handle at all.

Fixing this means threading a `FileTarget` through `Handle::read`,
`Handle::write` and `Handle::flush`, changing three public signatures. It is the
most substantial gap in the API surface.

### No `'static` reference buffers

`IoBuf` and `IoBufMut` are implemented only for `Vec<u8>`, `Box<[u8]>` and
`[u8; N]`. Adding `&'static [u8]` and `&'static mut [u8]` is sound under the
existing `'static + Unpin` bound and costs nothing, and it closes a gap that
both tokio-uring and compio have already closed. See
[buffer-ownership.md](buffer-ownership.md) for the proposed impls and the
reasoning behind the bound.

### No `NOP` operation

The host reports `IORING_OP_NOP` as supported, but `windows` 0.62.2 exposes no
`BuildIoRingNop`, so it is unreachable without hand-written bindings.

### Explicit batching control

Operations already coalesce: `submit_pending` issues one `SubmitIoRing` covering
every entry built since the last call, so N operations started before the driver
task next runs cost a single submission. This matches how `tokio-uring` behaves —
it has no user-facing batch API either, and its driver accumulates submission
queue entries and submits them with one `io_uring_enter`. That coalescing is now
measured rather than asserted, and in the benchmark it reaches the full
configured concurrency — see the entry on batching under "Benchmark blind spots"
below, which lowers the priority of this one further.

What is *not* possible is batching across await points: a caller who awaits each
operation in turn gets one submission each, because there is nothing to batch
with an operation that has not been issued yet. That is inherent, not a gap.

An explicit API would only add the ability to *withhold* the wake signal so that
operations accumulate deliberately — trading latency for fewer submissions. A
niche throughput knob rather than a missing capability, which is why it is low
priority.

### Submission retry busy-loops

When `SubmitIoRing` fails persistently the driver re-polls as fast as the
executor schedules it. A Win32 waitable timer would give runtime-independent
backoff, reusing the `RegisterWaitForSingleObject` machinery already in
`sys::event`.

## Correctness observations

### A registered index is resolved at execution time, not build time

`check_registered_buffer` / `check_registered_file` validate against whatever
registration is in force when the operation is *described*. The submission-queue
entry carries only the bare index, so the kernel resolves it when the operation
*executes*.

**Resolved for buffers.** A supersession can no longer be adopted while an
operation names the old registration: holding a handle refuses re-registration
outright, and four further guards close the remaining routes — see
`INV-REG-NO-STALE-HANDLE` in [design.md](design.md). The interleaving this entry
described is therefore unreachable for buffers.

**Still open for files.** `register_files` has no equivalent guard, because
nothing borrows a registered *file* the way a handle borrows a registered buffer.
An operation naming file index 0 can still have a new file registration adopted
under it, and the kernel will resolve the index against the new set. Memory
safety is not at risk — superseded registrations are retained until the ring
closes — but the operation may target a different file than the caller meant.

**Guidance:** do not supersede a *file* registration while operations naming it
are outstanding. Closing this properly would mean giving file registrations the
same checked-out shape buffers now have.

### The sequential cursor advances on poll; the guard clears on completion

`SequentialGuard` is dropped when the driver drops the payload, at terminal
completion. The cursor advances later, when the future is polled. Between those
two moments the flag is clear but the cursor is stale.

`&mut self` prevents a second sequential operation on the same `File` *value*,
but `File` is `Clone` and clones share one `FileState`, so a second task holding
a clone could claim the guard in that window and read from a stale cursor. Not
unsound — just a silently wrong offset.

Options: have the driver advance the cursor (it already holds the transfer count
and the `Rc<FileState>`), or document that concurrent sequential operations
across `File` clones are unsupported.

### `File: Clone` softens the compile-time exclusivity guarantee

Sequential exclusivity is meant to be enforced at compile time by `&mut self`,
and a `compile_fail` doc-test asserts it. Two clones give two independent `&mut`
paths to one `FileState`. The runtime guard catches it with
`Error::OperationOutstanding`, so it degrades gracefully — the compile-time check
is per-value, the runtime check is per-file. Worth a sentence in the `File::clone`
rustdoc.

### A dropped future's cancellation can be silently abandoned

`OpFuture::drop` uses `try_borrow_mut` and returns if the driver is already
borrowed — the right call, since panicking in `Drop` would be far worse. It is
not a soundness hole: the buffer is still released by the operation's own
completion, and now also by the drain, which no longer abandons anything.

The hazard is smaller than it was. Wakers and error reports are now delivered
*outside* the driver's borrow everywhere, so the window in which a task can drop
a future while the driver is borrowed is much narrower than when this was first
written. The operation then runs uncancelled, which costs efficiency rather than
correctness.

Consider recording the token outside the `RefCell` for the driver to process on
its next pass.

## API consistency

### The comparison's registered backend reports a registration it never made

`IoRingRegistered::configuration()` prints "single-threaded driver; registered
buffers and file handle; registration-naming operations", and the fairness
account and `docs/performance.md` both quote it. The middle clause is false:
`IoRingRegistered::register_file` exists and is never called, so
`registered_file` stays `false`, `target()` always yields `FileTarget::Owned`,
and the backend runs registered buffers against an owned handle.

Harmless to the measurement — every backend in the comparison passes an owned
handle, so the five are alike in exactly the way a comparison needs — but the
printed configuration overstates what was measured, which is the one thing a
fairness account exists not to do. Either call `register_file` during
preparation, and accept that the registered backend then differs from its peers
in two ways rather than one, or correct the string. It is listed here rather than
fixed inside the Criterion migration because it changes what is measured, and
that migration's whole premise is that what is measured did not change.

### SQE flags are not available on every path

`read_with_flags` / `write_with_options` / `flush_with_options` expose
`SqeFlags`, but `read_registered`, `write_registered`, `File::read_at`
and `File::write_at` do not. A caller needing `DRAIN_PRECEDING_OPS` cannot use the
registered path at all.

### `Driver::drop` can block indefinitely

The drain is unbounded by design: it ends when the kernel holds nothing. A
`Drop` that can stall a single-threaded executor for an unbounded time is worth
stating in `Driver`'s rustdoc.

Two specific hazards follow, both accepted rather than solved:

- **An operation that never completes hangs the shutdown.** Cancellation is
  best-effort, so an operation the platform will not abandon has no exit. The
  alternative — giving up and freeing memory the kernel may still write into —
  is a use-after-free, so hanging is the lesser failure. `Error::ShutdownStalled`
  exists so the caller can at least see it happening.
- **Registrations cannot be cancelled at all.** A cancellation must name the file
  its target named, and a registration names none, so one in flight can only be
  waited for.

A bounded-with-escalation policy, or a caller-supplied deadline after which the
process aborts rather than hangs, would both be defensible; neither is
implemented.

### The drain busy-spins while submission is blocked

While any `Built` entry remains, the drain must not issue its waiting submission
(**INV-NO-WAIT-WITH-BUILT** in [design.md](design.md)), so it retries without
blocking. In `drive` that is a cooperative yield, but in `Drop` it is a tight
loop until the queue accepts the entries. Bounded in practice — the entries are
accepted as soon as there is room — but it is a busy-wait, and the same waitable
timer that would fix the submission retry above would fix this.

## Wake path

### The guard on park cost is indirect, and is not wired to CI

The driver's park machinery was measured at **13.99 µs per operation above the
synchronous ring floor** at one operation in flight, and rewritten down to
**2.46 µs**. Both figures came from a probe that was deleted afterwards, by
decision: a benchmark nobody runs is a benchmark nobody maintains.

What now exists, which did not before: the comparison benchmark stores and
compares against a **baseline**, so `cargo bench -p win-ioring-bench --
--save-baseline pre` followed by the same command with `-- --baseline pre`
reports, per benchmark, a change interval and a verdict. That is a committed
guard on **end-to-end per-operation cost**, and a regression in the park path
large enough to move the depth-1 figures would show up in it — the wake-path work
moved random read at depth 1 by 38%, against a null run of this host whose change
intervals reached −18% to +24% on an unchanged tree. So 38% is larger than
anything the noise produced, but by roughly a factor of 1.6, not by an order of
magnitude: this guard catches a regression of that size and would not reliably
catch one half of it. (This sentence previously compared 38% against "the ±17%
this host's own run-to-run noise produces". That figure is not what the null run
measured — see [performance.md](performance.md) — and it was doing load-bearing
work here, which is why it is corrected rather than dropped.)

Three things it still does not do, and all three matter:

- It **cannot attribute** a change to the park path. It measures the whole
  per-operation cost, so a park regression and a ring regression look identical.
- It is **not wired to CI**. `cargo test --benches` runs the target in test mode,
  one iteration per benchmark against a small configuration, which proves the
  measurement path works but times nothing. Nobody's build fails on a
  performance change.
- Its **baselines are local**. They live under `target/criterion` and are lost
  with the `target` directory, so the comparison is between two runs somebody
  chose to make on one machine, not against a recorded history.

Restoring the probe as a test with a threshold was considered and rejected as too
brittle to be worth its maintenance; a lower-variance measurement of the same
quantity would be the thing to build if this becomes a recurring worry.

### Two faster dispatch mechanisms were measured and not taken

Getting a signalled event to the driver's thread costs about 4.3 µs through the
OS thread pool, which is now the single largest remaining item in a park. (That
figure and the one below come from the original investigation run, not from the
retained measurement artifacts, and are approximate.)

- **A dedicated waiter thread** measured 1.66 µs — roughly 2.6x faster. Rejected
  because it costs one OS thread per driver, which sits badly with a crate whose
  whole shape is "no hidden machinery".
- **`WT_EXECUTEINWAITTHREAD`** would avoid queueing the callback to a worker
  thread entirely. Rejected because it runs arbitrary executor wake code on a
  shared wait thread, and the platform reference warns of deadlock when another
  thread calls `UnregisterWaitEx` while the callback contends for a lock — which
  is exactly the shape of this crate's teardown.

### The always-armed registration has a standing cost

The completion wait is armed for the driver's whole life, so the OS dispatches a
callback for every completion signal even on passes where the driver is not
parked and will find the work itself. That callback is paid on a pool thread and
costs the driver one uncontended mutex acquisition. Measured harmless at eight
and sixty-four operations in flight — per-operation cost fell at both — but it is
a real cost that a workload with very high completion rates and a rarely-parked
driver would pay.

### The comparison and a direct probe disagree by about 4 µs per operation

At sixty-four operations in flight on random reads, the comparison benchmark
reported roughly 9.3 µs per operation for this crate where a direct probe over
the same shape of work measured roughly 5.1 µs. Both were measured on the same
host, by the harness that has since been replaced.

The difference belongs to something in the measurement — trace recording, the
verification digest, the work loop's own bookkeeping — and has not been chased
down. It does not affect the *comparisons*, since every backend pays it, but it
means the absolute figures are not a measurement of the crate alone.

The move to Criterion did not resolve this and was not expected to: the same
recording and digesting happen inside the timed closure, by design, because a
comparison that verified outside the timed region would not be verifying what it
timed. The current figure for that cell is 10.1 µs per I/O, which is the same
quantity under a different instrument and not a new datum on this question.

## Benchmark blind spots

### Why this crate loses as depth rises is unknown; it is not a batching shortfall

**This entry previously claimed that no benchmark engaged submission batching.
That claim was wrong, on both of its factual counts, and the correction is worth
more than the original suspicion was.**

What it said: that `Runner::run`'s rolling window issues a full batch only on the
first poll of its `FuturesUnordered`, so "a submission covers far fewer entries
than the depth suggests", and that "how many is not deterministic".

What was measured. A counter on the driver — `Handle::submission_counts`, added
for this — reports how many `SubmitIoRing` calls a run made and how many entries
they covered. Across the default matrix, every ring cell:

| scenario | depth 1 | depth 8 | depth 64 |
|---|---|---|---|
| sequential read (256 ops) | 1.0 | 8.0 | 64.0 |
| random read (512 ops) | 1.0 | 8.0 | 64.0 |
| write then read (257 entries) | 1.0 | 7.8 | 51.4 |
| bulk read (256 ops) | — | — | 64.0 |

Entries per submission. The rolling window batches at **exactly the configured
depth**: 256 entries over 4 submissions at depth 64, not "far fewer". Where the
figure is not the depth it is arithmetic, not shortfall — write-then-read's 257
entries are four submissions of 64 and one of 1. And it is not indeterminate:
three full runs produced **bit-identical** figures in all twenty ring cells, for
both ring backends.

Why the original reasoning failed: it stopped at the first poll. The executor
drains every ready completion in one pass before the driver submits again, so
against a warm page cache — which is the documented condition these benchmarks
run under — a rolling refill rebuilds the whole window before the next
`submit_pending`, and one submission covers all of it. That is a property of the
measurement conditions, not of the code; a cold cache would stagger completions
and could well produce the ragged batches this entry assumed.

The bulk-read scenario was added anyway, and earns its place by settling the
question rather than by changing the answer. At depth 64 it batches 64.0 entries
per submission — the same 256 entries over 4 submissions as the rolling
sequential read, digit for digit — while reaching a mean depth of 32.5 against
the rolling 56.1, so it is demonstrably a different shape that produces identical
batching. Its timings sit inside the rolling band: 1.15x-1.28x against
`tokio::fs`, where rolling sequential read at the same depth is 0.92x-1.61x.

**What this leaves open.** The published figures still run the wrong way — this
crate wins at depth 1 and loses by a widening margin as depth rises — and the
explanation this document previously offered for that, "paying close to one
submission per operation", is now known to be false. The crate's central claimed
advantage was already fully in effect in every figure ever published here, and it
loses anyway. Nothing currently establishes why. Candidates worth measuring, none
of them supported by evidence yet: single-threaded completion processing becoming
the bottleneck where a 512-thread pool gains real parallelism; per-completion
dequeue cost; the cache effects raised in the wake-path entries above. Picking
one of these to write down without measuring it would repeat the mistake this
correction exists to fix.

The remaining measurement gap is a **cold-cache run**. Every figure here is
warm-cache by design, and the batching equivalence above is explicitly a
warm-cache result. Whether the shapes still coincide when completions arrive
staggered is unmeasured, and it is the one condition under which the original
suspicion could still turn out to be right.

A correction to a second claim in the original entry: it said `Runner`'s
`starved` flag "would report a shortfall on every run" in a batched window. It
would not, because it could not report anything. Achieved depth was decoration —
`Shortfall` annotates the account and fails nothing, at any shape. The check that
now bites is `ShapeCheck`, which compares measured mean depth against a
closed-form prediction for the declared shape and routes a mismatch through
`Account::failures`.

## Test coverage gaps

- **The weakening tests never weaken a ring backend.** Five of the tests in
  `crates/win-ioring-bench/tests/fairness.rs` that require a weakened run to be
  rejected take fixed positions from the available-backend list, and the two
  `tokio::fs` backends are always first, so `tokio-pool-1` and `tokio-pool-512`
  are the only backends those five ever weaken — on a host with an I/O ring as
  much as on one without. The rejection they establish is a property of
  `harness::measure_combination`, which is backend-agnostic, and the honest
  control case does run every available backend; but `Weakness::HollowDelivery`
  exists because the *registered* backend once reported transfer counts with
  nothing readable behind them, and that is the backend the weakening never
  reaches. Looping the weakening over every available backend, weakening backend
  *i* against an honest reference, would close it, at the cost of the extra
  combinations' run time in every `cargo test`.

  **This gap survives the addition of the compio backend.** Two tests added with
  that backend do weaken it directly — `a_weakened_compio_backend_fails_on_the_delivered_byte_count`
  and `a_weakened_compio_backend_fails_on_the_digest`, which select compio by
  identity rather than by index — so the machinery is now known to reach a
  completion-based backend rather than only the thread-pool ones. That is a
  narrower thing than closing the gap. **Neither ring backend is weakened by any
  test, and the specific backend whose historical defect motivated
  `HollowDelivery` is still the one never exercised.** The compio tests were
  scoped that way deliberately: the run-time cost of looping every backend, given
  as the reason above for leaving this open, should not be paid by the work that
  adds a backend. For scale, those two tests alone add 48 measured combinations
  to every `cargo test` (2 tests × 4 scenarios × 2 backends × 3 runs), and
  enrolling compio in the control case adds 12 more — 60 in total. Looping the
  weakening across all five backends would multiply the first figure again.
- Post-shutdown rejection is asserted for `read`, `write`, `flush` and
  `register_buffers`, but not for `register_files`, `read_registered` or
  `write_registered` — all three implement the check.
- **Aborts cannot be provoked in-process.** The two memory-safety aborts — a
  panic escaping teardown, and `DriverInner` destroyed with the ring open — end
  the process, so a test cannot observe them without a subprocess harness. They
  are discharged by enumeration and review instead. A `#[test]` that spawns the
  test binary with a filter and asserts the exit code would close this.
- **A completion racing its own cancellation** is not tested. The criterion asks
  that the natural result win when it gets there first, and no way to make that
  ordering deterministic has been found. The correlation mechanics either side of
  it are tested.

## Minor

- **`cargo bench -p win-ioring-bench -- --list` builds the full working set.**
  `--list` sets `bench=true, test=false`, so `test_mode()` correctly reports a
  benchmark run and `Config::default()` is selected; Criterion's list mode then
  never invokes a routine. The result is that merely asking which benchmarks
  exist creates the 256 MiB read file, walks all fifty combinations through
  preparation, warm-up, verification and teardown, and reports every one of them
  as verified but not timed — minutes of I/O for a list of names. Detecting
  `--list` alongside `--test` and using `Config::small()`, or returning before
  preparation, would fix it. Not fixed inside the Criterion migration because
  every additional argument `test_mode()` reads is another way for a benchmark
  run to be misclassified as a test run, which is the failure that would silently
  publish figures from the small configuration.
- `cancel_holds` is a `Vec` cleared with a linear `retain` on every cancel
  completion. O(n) per completion; a `HashMap` would be O(1). Only matters at
  high cancel volume.
- `Driver::drive` builds a fresh wait registration for both events on every loop
  iteration, so a steady stream of completions costs four thread-pool syscalls
  per batch, one of which blocks. Hoisting the registrations out of the loop and
  re-arming after each firing would make the steady state free. Note that the
  blocking `UnregisterWaitEx` is what makes the `Arc` handoff sound — do not
  simply remove it.
- `cqe.Information as Transferred` is an unchecked `usize` → `u32` truncation.
  Safe today because every submission path bounds the length by a `u32` argument,
  but a `debug_assert!` would document and enforce that invariant for free.
- `AsyncEvent::wait_sync` maps `WAIT_TIMEOUT` to `Error::from_thread()`, which
  reports whatever unrelated error happens to be in thread-local storage.
  `WAIT_TIMEOUT` is a return value, not a last-error condition. Both match arms
  are also identical, so the explicit `WAIT_TIMEOUT` arm looks like it
  distinguishes the case and does not.
- `start_register_buffers` rejects an empty `Vec` of buffers but not a
  zero-capacity buffer *within* it. The crate rejects the analogous
  empty-registration case eagerly "where the error is useful"; the same argument
  applies here.
- `deferred_cancels` is drained only by `cancel_abandoned`, which runs only after
  a *successful* `submit_pending`. If submission never succeeds before shutdown,
  tokens accumulate until teardown. Harmless — bounded by `MAX_SLOTS`.
