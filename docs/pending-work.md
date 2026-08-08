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

### The ring op-code enumeration is not gated against a `windows` version bump

`docs/pipes-and-the-ring.md` designs around a closed set: `windows` 0.62.2 exposes
exactly seven `IORING_OP_*` constants and exactly six `BuildIoRing*` builders,
each hard-coding its own op code, with no generic submit path. That is why the
named-pipe accept runs as an overlapped Win32 call instead of a ring operation.

The enumeration was done by reading the pinned crate source, and **nothing
re-checks it.** Rust cannot assert that a dependency exposes no further items, so
a `windows` release adding `BuildIoRingConnectNamedPipe` would silently leave the
accept path taking the harder route for no reason, and the document asserting a
closure that no longer holds. `ALL_OPS` in `io_ring/tests.rs` is a hand-maintained
list of the codes this crate queries; it proves nothing about the crate it reads
from.

A partial gate is available and was judged not worth its cost: asserting the seven
numeric values are exactly 0–6 would catch a renumbering but not an addition,
which is the case that matters. **The practical mitigation is procedural — a
`windows` version bump is the moment to re-run the enumeration** — and it is
recorded here because a procedural obligation nobody has written down is one
nobody performs.

### A pipe flush outstanding at shutdown may never terminate

`Handle::shutdown` and the drain that follows it are unbounded by design: closing
the ring does not cancel in-flight operations, so the driver waits for every one
of them rather than abandoning memory the kernel may still be writing to. That is
the right trade for a file, where every operation completes on the storage
stack's schedule.

It is not bounded at all for a pipe. `FlushFileBuffers` on a named pipe waits for
the *peer* to read the buffered bytes, and the peer is under no obligation to do
so — it may be a different process, blocked, or simply uninterested. So the
drain's bound is set by a process this crate does not control, and there is no
bound at all if the peer never reads.

Measured, with a control:

| arm | verdict |
|---|---|
| peer never reads | the drain had not returned after 60 s |
| peer reads, one line different | the drain returned in 135.5 µs |

The control is what makes the first row a measurement: without it,
"did not terminate" is a verdict the probe might be structurally incapable of
contradicting.

**No fix is proposed here, and one is not obviously available.** Cancelling the
flush at shutdown would break the crate's central promise that shutdown never
abandons memory. Bounding the drain would break the same promise. Refusing
`flush` on a pipe would remove a legitimate operation — a flush on a pipe *does*
complete whenever the peer is reading, which is the normal case. The honest
position is the documented one: the rustdoc on the pipe types says the crate
cannot promise a bounded shutdown while a pipe flush is outstanding, rather than
implying a bound it cannot keep.

The reason this is recorded rather than fixed is that the alternatives are all
worse than the disclosure, and a reader who hits it deserves to find the
reasoning rather than a surprise.

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
batching. Its timings sit inside the rolling band: 1.14x-1.28x against
`tokio::fs`, where rolling sequential read at the same depth is 0.92x-1.61x.

**What this leaves open.** The published figures still run the wrong way — this
crate wins at depth 1 and loses by a widening margin as depth rises — and the
explanation this document previously offered for that, "paying close to one
submission per operation", is now known to be false. The crate's central claimed
advantage was already fully in effect in every figure ever published here, and it
loses anyway. Nothing currently establishes why. **One candidate has since been
measured and eliminated** — handle mode; see the closed item below. Candidates
worth measuring, none of them supported by evidence yet: single-threaded
completion processing becoming
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
- **Two wake-path tests fail intermittently under load.**
  `a_completion_signalled_mid_pass_is_not_lost` and
  `a_driver_polled_by_a_new_task_is_still_woken_by_a_completion`, both in
  `runtime/mod.rs`, fail occasionally when the full suite runs in parallel and
  pass 12 times out of 12 when run in isolation.

  **This predates the named-pipe work and is recorded rather than fixed for that
  reason**: a fix would land in the same diff as an unrelated feature and confound
  both. It was observed on an unmodified tree at `0f8971f` before any of that
  work began, which is what establishes it as pre-existing rather than
  introduced. It did not fire in roughly 125 full-suite runs during that work.

  The shape suggests a test-side timing assumption rather than a defect in the
  wake path — both tests drive the driver by hand and assume a completion lands
  within a bounded number of passes, which is exactly the assumption a loaded
  machine breaks. That is a hypothesis, not a finding; nobody has investigated it.
  Anyone who does should start by making the bound explicit rather than raising
  it, since a raised bound is a flake deferred rather than a flake understood.

- **The requirement-coverage audit cannot see a missing test.** The script at
  `.paw/work/named-pipes/probes/coverage-audit.ps1` checks that every requirement
  and criterion in a specification is *cited by a phase* of the implementation
  plan. It does not, and cannot, check that the phase then wrote the test. SC-001
  of the named-pipe work — a server accepting a client and exchanging bytes in
  both directions, through a registered buffer and through a registered file
  handle — was assigned to a phase, cited by it, reported covered, and never
  implemented. It was found in the final gate pass by noticing the integration
  test crate was untouched in the diffstat, not by any gate. The audit is still
  worth running; it costs seconds and it did catch omissions. But its result
  means "the plan forgot nothing", which is much weaker than "nothing was
  forgotten". Closing the gap needs a link from each criterion to the tests that
  gate it, which this repository has no convention for.

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

## Handle mode: `File::open` and `FILE_FLAG_OVERLAPPED` — **CLOSED**

Both items below are **done**. `File::open` and `File::create` now set
`FILE_FLAG_OVERLAPPED`, the warm-cache matrix has been re-run and republished on
overlapped handles, and the mechanism argument that stood in for a measurement
has been replaced by one. See
[performance.md](performance.md#handle-mode-the-one-candidate-that-was-eliminated).

**What was learned, and it was not what was predicted.**

The change was made expecting it to narrow the crate's unexplained warm-cache
loss against `tokio::fs` at pool width 1 — recorded, **at the time the prediction
was registered**, as 1.24x sequential and 1.40x random at depth 64, cause unknown.
(The re-run published in `performance.md` now reads that sequential cell at 1.30x
with owned buffers; the prediction was made against the figures then in print, and
those are the ones quoted here.) The prediction was registered before any number
existed, quantified as a 0.24 and 0.40 reduction in those ratios, and **it was
wrong**.

A paired A/B, with both handle modes present in the same run as separate backend
configurations, measured across two independent five-run sets — the second
collected after the entire analysis was frozen — found **no effect of the
predicted size in any of the eight cells**, and none of the four depth-1 negative
controls flagged. The gap to `tokio::fs` reproduces in the arm on overlapped
handles, at a similar size to the figures that motivated the prediction, though
the arm's numbers are not directly comparable to matrix cells.

Three things that buys, none of them a consolation:

- The **mechanism argument this document and `performance.md` had been making is
  confirmed by measurement.** Warm-cache figures measured on synchronous handles
  were fair, and are retroactively confirmed as fair rather than merely argued to
  be.
- **A candidate cause is eliminated from a list of three**, and it was the most
  concrete one. The count of *tested* candidates went from zero to one.
- The second item below — "should the figures carry a note?" — is resolved by
  being **overtaken**: there is no longer a discrepancy between the sections to
  explain, because the matrix and the unbuffered arm now both use overlapped
  handles, and the reason the arm always had to is stated in both places.

**What it does not buy.** The instrument resolves about 10% on the paired
difference of ratios at
the median, so what is excluded is an effect of the *predicted magnitude*, not
any effect at all. An effect of around 10% or less would not have been reliably
visible and is not ruled out. The open question at
[performance.md](performance.md#what-that-leaves-unexplained) remains open, with
three candidates left.

**What was kept.** The A/B arm is permanent, as `cargo bench -p win-ioring-bench
--bench handle-mode`. The published matrix carries overlapped handles only. The
reasoning for that split: under the sizing that made the experiment affordable
the marginal cost of keeping the arm is near zero, and a permanent arm means the
next person who wonders about handle mode reads a number instead of re-running
this.

## `tokio::fs` opens synchronous handles, and the matrix does not correct for it

Found while closing the item above, and **deliberately not fixed in the same
work**, because changing the baseline in the run that measures handle mode would
confound the variable the experiment was built around.

Every `tokio::fs` figure in the published matrix — including the pool-1 column
that is the 1.00x baseline for every `relative` in it — comes from a handle
opened without `FILE_FLAG_OVERLAPPED`, because `tokio::fs::File` wraps
`std::fs::File`. The `win-ioring` and `compio` cells are all overlapped. This is
disclosed in `performance.md` in the section where the comparison appears, and it
is confirmed at run time rather than argued: the handle-mode arm reads the mode
back off the kernel for every backend including `tokio::fs`, and fails the run if
a handle is not what its configuration declared.

**The asymmetry is not symmetric in what it costs, and its likely size differs by
configuration.** At pool width 1 a synchronous handle should cost `tokio::fs`
very little — a single blocking thread issues one operation at a time regardless
of what the file object permits, so there is nothing for the kernel's
serialisation to take away. At pool 512 across a single handle it should cost a
great deal, because 512 threads then contend for a lock the overlapped backends
never take. That second case is not hypothetical: the unbuffered arm already
measured the shape of it as its 1-handle against 64-handle result.

So the column most exposed is **pool 512**, and the column least exposed is
**pool 1** — which is the baseline, and therefore the one the published relatives
depend on. That is the fortunate direction, but it is a reasoned expectation and
not a measurement.

**Cost to resolve.** A variant `tokio::fs` backend opening through
`OpenOptions::custom_flags(FILE_FLAG_OVERLAPPED)`, run as an A/B against the
existing one. `tokio::fs` would not *use* the overlapped-ness — it issues
blocking positional reads on pool threads — so this measures the cost of the file
object's serialisation alone, which is exactly the quantity wanted. The obstacle
is that `std`'s `seek_read`/`seek_write` abort the process if the kernel returns
`STATUS_PENDING`, which is safe under a warm cache and not safe unbuffered, so
the variant would have to be restricted to the warm-cache arm and documented as
such. Budget: the arm already runs six configurations against 320 s; adding two
`tokio::fs` variants at the same scenarios and depths is roughly another 110 s,
which does not fit and would need the same sizing exercise the handle-mode work
did. Estimated a day, most of it measurement and write-up rather than code.
