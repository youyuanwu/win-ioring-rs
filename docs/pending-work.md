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
queue entries and submits them with one `io_uring_enter`.

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

## Test coverage gaps

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
