# Design

## The constraint everything follows from

IoRing is **completion-based**. Once the kernel has accepted an operation it runs
to completion whether or not anyone is still waiting for it, and there is no way
to withdraw it. Cancellation is a *request*, not a revocation.

That single fact drives nearly every decision below. If you are changing
something here and it seems unnecessarily careful, check it against this first —
the alternative is usually a use-after-free rather than merely a worse API.

## Layers

```
caller ──> Handle ──┐                    ┌──> IoRing (unsafe layer) ──> kernel
                    │                    │
                    ├──> DriverInner ────┤
                    │    (Rc<RefCell>)   │
Driver::drive() ────┘                    └──> OpSlab (operation storage)
                          ▲
                          │ waker signalled from an OS thread pool thread
                    AsyncEvent (completion event)
```

- `io_ring` — an unsafe, one-for-one wrapper over all fourteen platform entry
  points. No lifetime tracking. Every method that hands the kernel a resource is
  `unsafe` and documents its contract.
- `runtime` — `Driver` owns the ring and the operation slab; `Handle` is a cheap
  clone used to submit work.
- `file` — an ergonomic `File` over a reference-counted handle.
- `buf` — the `unsafe` buffer contracts.

The only integration point with an executor is spawning `Driver::drive()`. There
is no ambient state and no runtime dependency.

## Decisions

### Operation state lives in the driver, not the future

Each operation's payload — the caller's buffer, a reference to the file, the
result slot, any sequential guard — sits in a driver-owned slab, boxed so its
address is stable. The future holds only a token, a shared result slot, and a
**weak** reference to the driver.

This is what makes dropping a future safe. The payload stays put, and the
kernel's pointers stay valid, until that operation's own completion is dequeued.
A future that borrowed its buffer would free memory the kernel is about to write
into.

**Corollary worth knowing:** always `Box::new(buffer)` *first*, then take the
pointer from inside the box. Taking the pointer from a local before boxing hands
the kernel a stale address for any buffer stored inline, such as `[u8; N]`. A
`Vec` masks this bug because its heap allocation does not move; an array does
not. Coercing `Box<B>` to `Box<dyn Any>` only changes pointer metadata, so the
address survives.

### Tokens carry a generation

A bare slot index would let a stale completion resolve a recycled slot. A token
packs a kind bit (operation vs cancellation), a 16-bit slot index, and a
generation, laid out identically on 32- and 64-bit targets.

A slot whose generation is exhausted is **retired** rather than wrapped, so a
token can never be minted twice. The kind bit is also what keeps operation
completions and cancellation completions in disjoint halves of the user-data
space — the platform gives a cancellation its *own* user data rather than
echoing its target's.

Generations start at **1**, not 0, and that is a safety property rather than
bookkeeping taste (**INV-TOKEN-NONZERO**). A cancellation names its target by
user data, and the platform reads a target of `0` as *"everything on this
handle"* — see [platform-notes.md](platform-notes.md). Zero is reachable only as
an operation (kind bit clear) in slot 0 at generation 0, so starting a generation
higher removes it. `issue_cancel` re-checks the value before use, because the
consequence of the invariant lapsing is silent damage to code outside this crate
rather than a failure inside it.

### Cancellation is a correlated state, not a counter

A cancellation is its own submission with its own completion, and may complete
before, after, or instead of the operation it targets. A slot therefore tracks
`CancelState` (`NeverRequested` / `Pending` / `Completed`).

A counter cannot express "the operation completed while a cancellation is still
outstanding" — which is exactly the state that requires holding the slot as a
tombstone until the cancellation reports. Releasing it earlier would let the
cancellation's completion land on a recycled slot.

### Slot state has two orthogonal dimensions

`Lifecycle` (Described → Built → Submitted) tracks how far an operation has got.
`Observer` (Live / Detached) tracks whether a future is still waiting.

Keeping them separate is what lets the driver tell a detached-but-unsubmitted
operation — which can simply be released — from a detached-and-submitted one,
which must be cancelled and waited for.

### A submission failure is not an operation failure

`SubmitIoRing` failing leaves the built entries queued, so "built but not yet
submitted" is a real, persistent state rather than a transient one.

Consequently a submission failure is reported to the driver's *error observer*,
not to any one operation: it affects every queued entry, so it cannot be any
single operation's result. The affected futures stay pending, and the driver
wakes itself to retry.

The self-wake matters. `futures::pending!()` returns `Pending` **without**
waking, so the driver would park forever on the retry path — no completion can
arrive to prompt the next poll when the problem is that nothing was submitted.
The crate uses a self-waking `YieldNow` instead.

### Submission is batched implicitly

`submit_pending` issues a single `SubmitIoRing` covering every entry built since
the last call, and promotes all of them at once. So operations started before the
driver task next runs — the usual case for concurrent work on a single-threaded
executor — cost one submission between them, not one each.

This is the same bargain `tokio-uring` makes: it exposes no batch API either, and
its driver accumulates submission queue entries and submits them with one
`io_uring_enter`. Batching across *await points* is not possible in either crate,
because an operation that has not been issued yet cannot be batched with one that
has.

### Registration is a permanent transfer of ownership

The platform has no unregister entry point, and gives no signal that it has
stopped referencing a superseded registration. There is therefore no moment at
which handing the resources back is provably safe.

So a successful registration takes ownership for the life of the ring.
Registering again supersedes the previous set without returning it; superseded
sets are retained until the ring closes. `register_files` is the exception to the
*taking* part: it only needs its own reference to each handle, so it borrows.

Registered buffers carry an **initialization watermark** alongside their extent.
A read may target the whole extent, but a write is bounded by the watermark, so
uninitialized memory is never sent to the kernel. Completing a read raises the
watermark — but only if the read started at or before it, because a read landing
past the watermark would otherwise vouch for a gap of genuinely uninitialized
bytes in front of it.

### Shutdown drains to quiescence

Closing the ring neither cancels in-flight work nor waits for it, and the kernel
may keep writing into buffers afterwards
([platform-notes.md](platform-notes.md)). Linux's `io_uring` blocks in-kernel
when its descriptor closes, which is why the strategy other crates use — just
close it — is unsound here. Nothing else releases registrations either: there is
no unregister entry point at all.

So the driver **drains**, and the drain is unbounded. It ends when the kernel
holds nothing, however long that takes. A shutdown can therefore hang on an
operation that never completes and cannot be cancelled; that is a deliberate
trade against the alternative, which is freeing memory the kernel is still
writing into.

Four invariants carry it:

- **INV-DRAIN-TERMINATES** — `outstanding()` counts slots for which no completion
  can ever arrive on their own. Each step must therefore resolve `Described`
  slots and submit `Built` ones *before* waiting, or the drain waits forever for
  a report nothing will send.
- **INV-BUILT-NEVER-DISCARDED** — a `Built` entry is queued, still references the
  caller's buffer, and cannot be withdrawn. It must be submitted, never resolved
  locally — even under an immediate shutdown. The one carve-out is an entry the
  kernel has *repeatedly refused*: it was never accepted, so closing the ring
  discards it, and only then may it be abandoned.
- **INV-NO-WAIT-WITH-BUILT** — the drain never issues its waiting submission
  while any `Built` slot remains. `SubmitIoRing` submits *and* waits in one call,
  and the wrapper discards the submitted-entry count on error, so a waiting call
  that times out can hand entries to the kernel while leaving them still marked
  unsubmitted. Residue detection would then conclude the kernel held nothing and
  release buffers it is actively writing into.
- **INV-RING-CLOSED-FIRST** — nothing the kernel can reach is released until the
  ring is closed, and the ring is closed before any `Driver` field is dropped.

#### Two flags, and two loops

Teardown tracks `teardown_started` and `torn_down` separately. Re-entry is
guarded on `torn_down` — *finished* — because a drain abandoned part-way must
resume rather than skip to releasing. Guarding on "started" would let a second
call return as though teardown were done, leaving the ring open and everything in
it stranded.

There are two drain loops, not one: a cooperative one in `drive` that yields
between steps, and a synchronous one in `Drop for Driver`. Sharing a single
blocking loop would make escalation impossible. Everything here is `!Send`, so
`Handle::shutdown_now` can only be called from the driver's own thread; a graceful
drain that blocked that thread outright could never be escalated, and a graceful
drain never cancels, so escalation is the only way out of a stalled one. The mode
is re-read every step.

Their unwind handling is deliberately **asymmetric**. `drive`'s loop needs no
guard: a panic unwinds out, the `Driver` drops, and `Drop for Driver` re-enters
and finishes — which works *only* because re-entry is guarded on `torn_down`.
`Drop`'s loop does need `AbortOnUnwind`, because a panic there means `Drop for
Driver` will not run again while the remaining fields still drop in declaration
order, including the completion event, whose handle must not close while the ring
can still signal it.

#### Where it aborts, and why

Two categories, both reached only where every alternative is worse:

- **Ring failure** — the ring cannot be closed after `CLOSE_ATTEMPTS`. It can
  neither be drained nor safely released from.
- **Memory safety** — a panic escapes teardown, or `DriverInner` is destroyed,
  with the ring still open. Both would otherwise release memory the kernel can
  still reach.

A **wait timeout is not a failure** and never feeds either. `submit` reports one
as an error, which is a trap: counting it would make an ordinary slow shutdown
look like a stuck queue, and the drain would eventually abandon live entries.
Only the non-waiting `submit(0, 0)` feeds the submission-failure count.

#### What callers see

Every future resolves, with its buffer, either with its own outcome or with
`Error::AbandonedAtShutdown`. A drain that is not converging reports
`Error::ShutdownStalled { outstanding }` to the error observer, throttled, so a
stalled shutdown is distinguishable from a hang.

Each individual wait blocks its thread for up to `DRAIN_TIMEOUT_MS`, which
callers sharing that thread see for the whole of a prolonged shutdown.

### Buffers are owned, and the traits are `unsafe`

Operations take an `IoBuf`/`IoBufMut` by value and return it in a `BufResult`, on
success and on failure alike. The traits are `unsafe` to implement because they
promise a stable address and an honest initialized length that the compiler
cannot check.

They also forbid interior mutability of the buffer contents outright: a
`Cell<u8>`-backed buffer could mutate under a live `&[u8]` already handed to the
kernel.

`Outcome<T, B>` no longer exists. Operations resolve to `BufResult<T, B>`
unconditionally, because there is no longer a case in which the buffer does not
come back: the drain waits until the kernel is finished with it. An operation the
driver ended itself reports `Error::AbandonedAtShutdown` — a named teardown
error, not a fabricated I/O failure — and still returns the buffer alongside it.

Why ownership rather than a borrowed slice, and what it would take to accept one,
is worked through in [buffer-ownership.md](buffer-ownership.md).

### Single-threaded by construction

`Driver` and `Handle` share state through `Rc` and `RefCell` and are `!Send`. The
compiler enforces it and a `compile_fail` doc-test asserts it.

The one genuine thread boundary is `sys::AsyncEvent`. The completion event is
waited on via `RegisterWaitForSingleObject`, whose callback runs on an OS thread
pool thread. The shared state handed to that callback is an `Arc` whose reference
count is deliberately given to the OS and reclaimed **exactly once** — either by
the callback, or by a blocking `UnregisterWaitEx(handle, INVALID_HANDLE_VALUE)`
that proves the callback will never run. The `callback_ran` flag is checked
*first* to avoid deadlocking against the callback the caller is inside.

That blocking `UnregisterWaitEx` is load-bearing. Do not "optimise" it away.

### Recurring hazards

Bugs of these shapes were found repeatedly during development. They are worth
checking for in any change:

- **Taking a buffer pointer before boxing it.** See above.
- **Waking a future, or invoking the error observer, while `DriverInner` is
  `RefCell`-borrowed.** If the executor polls inline this panics. Both now
  collect into a `Vec` and act after the borrow drops; the observer lives on
  `Driver` rather than `DriverInner` so it is structurally impossible to call
  under the borrow.
- **Tests that pass without testing anything.** Dropping a `Pin<&mut F>` does not
  drop the future. `Handle::outstanding()` counts `Built` slots, so it cannot
  prove the *submitted* drop path ran. Asserting an operation is still
  outstanding after a drop is racy for a small local read unless nothing has
  awaited in between.
