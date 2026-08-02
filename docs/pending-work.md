# Pending work

Known limitations, deferred work, and observations from review that were judged
non-blocking. Nothing here is a known soundness defect; where safety is relevant
it says so explicitly.

Roughly ordered by value.

## Deferred features

### Owned buffers cannot be combined with registered handles

A registered file handle is reachable only from `read_into_registered` /
`write_from_registered`, which *force* a registered buffer. So the two
registration kinds cannot be mixed: a caller with registered handles but
caller-owned buffers has to pass a raw `File`. `flush` cannot name a registered
handle at all.

Fixing this means threading a `FileTarget` through `Handle::read`,
`Handle::write` and `Handle::flush`, changing three public signatures. It is the
most substantial gap in the API surface.

### No `NOP` operation

The host reports `IORING_OP_NOP` as supported, but `windows` 0.62.2 exposes no
`BuildIoRingNop`, so it is unreachable without hand-written bindings.

### No batching API

Each operation is submitted individually. An API that submits several built
operations under one `SubmitIoRing` call would cut syscalls materially.

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

If a supersession is adopted in between — a real interleaving, since
registrations are adopted in completion order and both can be in one submission
batch — the kernel resolves the index against the **new** registration.

Memory safety is not at risk: superseded buffers are retained, and the kernel
bounds-checks against its own registered extents. The watermark update is
generation-guarded, so bookkeeping stays coherent. What can differ is where the
data lands, and an index valid at build time can be rejected at completion time.

**Guidance for now:** do not supersede a registration while operations naming it
are outstanding. A stronger fix would refuse `register_*` while any registered
operation is in flight, at the cost of an API restriction.

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
completion.

But the operation then runs uncancelled *and* the slot is never marked detached,
so `detached_submitted_uncancelled()` will not pick it up later either. The
borrow is held exactly while the driver is inside `reap_completions`/`teardown`,
which is when a waker may be invoked and a task may drop a future inline — so the
case is reachable rather than theoretical.

Consider recording the token outside the `RefCell` for the driver to process on
its next pass.

## API consistency

### `WriteFuture` alone has no poll-after-completion state

`ReadFuture`, `RegisteredOpFuture` and `FlushFuture` each carry a `Done` state
and panic on re-poll. `WriteFuture` delegates straight to
`OpFuture::poll_resolution`, so re-polling a completed one returns `Pending`
**forever** — a hang rather than a panic. Poll-after-`Ready` is caller misuse
either way, but the four future types should behave alike.

### SQE flags are not available on every path

`read_with_flags` / `write_with_options` / `flush_with_options` expose
`SqeFlags`, but `read_into_registered`, `write_from_registered`, `File::read_at`
and `File::write_at` do not. A caller needing `DRAIN_PRECEDING_OPS` cannot use the
registered path at all.

### `Driver::drop` can block for up to ~1 second

`teardown` runs up to `DRAIN_ROUNDS` (4) iterations of
`submit(waiting, DRAIN_TIMEOUT_MS)` (250 ms), each of which blocks the calling
thread. A `Drop` that can stall a single-threaded executor for a second is worth
stating in `Driver`'s rustdoc, and the budget is a reasonable thing to make
configurable.

### `Error::DriverGone` may be unreachable on the intended path

`Handle` holds a strong `Rc`, and `Driver::drop` always resolves or marks-retained
every live slot, so the `Weak` upgrade failure looks dead. The one route found to
it is misuse: re-polling a `WriteFuture` that was rejected before submission.
Either the variant should go, or its rustdoc should stop implying it is a normal
outcome, or there should be a test for the case that reaches it.

## Test coverage gaps

- Post-shutdown rejection is asserted for `read`, `write`, `flush` and
  `register_buffers`, but not for `register_files`, `read_into_registered` or
  `write_from_registered` — all three implement the check.
- The headline soundness property — no buffer released before its operation
  reaches terminal completion — is proved by construction and by review, not by
  test. `shutdown_with_work_in_flight_settles` asserts settlement but carries no
  drop instrumentation. `CountingBuf` exists precisely for this; wiring it in
  would close the gap.

## Minor

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
