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
                          │ waker signalled from an OS thread pool thread —
                          │ the crate's only thread boundary
                    ArmedEvent (completion event, one wait armed for life)
```

Queuing work does not appear in that diagram, because it does not cross
anything: `Handle` raises a flag on `DriverInner` and wakes the driver's own
waker, all on the caller's thread.

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

### Registration lends buffers, it does not take them

The platform has no unregister entry point, and gives no signal that it has
stopped referencing a superseded registration. So the *mapping* is permanent: a
registration lasts until the ring closes, and registering again supersedes the
previous set without releasing it.

What is **not** permanent is the application's access to the buffers. An earlier
design moved them into the driver and addressed them by a bare `u32`, with a
registered read resolving to a transfer count — so nothing could ever read what
arrived. That made the crate's principal optimisation unusable for real work, and
it was found by trying to benchmark it: the registered path would have measured
well precisely because it delivered less than what it was compared against.

Registration now follows the model `tokio-uring` established. A successful
registration yields a **collection**; the application checks a buffer out of it
and receives a handle that dereferences to the bytes, satisfies the buffer
contract so it flows through operations and comes back the same way, and returns
itself on drop. `register_files` is unchanged: it only needs its own reference to
each handle, so it borrows.

**INV-REG-ONE-HANDLE.** At most one handle to a buffer exists at a time, so an
operation in flight and the application can never reach the same bytes. Enforced
by ownership rather than documentation: an operation takes the handle by value,
so the application cannot name it until it comes back. A `compile_fail`
doc-test asserts it.

**INV-REG-NO-STALE-HANDLE.** A handle names a buffer by index, and the platform
resolves that index against whatever registration the ring currently holds — so a
handle outliving its own registration silently addresses different memory. Five
guards close every route there, and only all five together suffice:

1. re-registering is refused while any handle is checked out;
2. checkout is refused while a registration request is in flight, because a
   registration is adopted when it *completes*, not when it is requested;
3. a collection retained across a successful re-registration stops yielding;
4. checkout is refused once the driver is gone or shutting down;
5. a second registration request is refused while one is in flight.

Route 5 is the one that is easy to miss. Two requests may be in flight at once;
the first adopts, a handle is taken from it, and the second retires that
registration underneath the handle — and no per-registration flag can see it,
because the registration the handle came from did not exist when the second
request was made. That is why the lock-out lives on the driver, and why the
registry holds a `Weak` back to it and asks. Weak, because a strong reference
from a leaked handle would make the driver unreachable but alive, silently
disabling the abort that catches a ring left open.

The in-flight flag is scoped to *buffer* registrations on both set and clear.
`register_files` shares the payload field, the slab and the future, so a
variant-blind clear would let a file registration completing first reopen the
window.

**INV-REG-LIVES-LONGEST.** A registration's memory has three claimants — the
driver, an outstanding handle, and the platform — and outlives whichever ends
last. It is shared by `Rc`, so a handle held across ring closure still addresses
live memory, and a superseded set is retained until the ring closes because the
platform may still hold pointers into it.

**INV-REG-DRIVER-BOUND.** An operation checks by pointer identity that a handle
belongs to *this* driver's current registration. Nothing else would catch a
handle from another driver: registrations are per-driver, and an index alone
cannot distinguish them.

**INV-REG-LOCK-ORDER.** Checkout takes the driver borrow and then the registry's;
nothing takes them the other way. `Drop for RegisteredBuf` touches only the
registry, never the driver, because it can run while the driver is mutably
borrowed — during completion reaping, or teardown.

Registered buffers carry an **initialized prefix** alongside their extent. A read
may target the whole extent, but a write is bounded by the prefix, so
uninitialized memory is never sent to the kernel. A completed transfer extends
the prefix — but only if it started at or before it, because one landing further
on would vouch for a gap of genuinely uninitialized bytes in front. The count is
maintained by the driver on completion whether or not a future is waiting, and a
handle caches a read-through copy so the buffer-contract methods take no borrow.

Two behaviours are accepted rather than prevented, recorded so they are not later
filed as defects. A registration whose future is dropped before it completes is
still adopted, but its collection is discarded — leaving a registration nothing
can check out of. And a leaked handle blocks re-registration for the life of the
ring; there is no reclamation path and none is offered.

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

#### Re-entry, and two loops

Re-entry into teardown is guarded on `torn_down` — *finished* — and never on
"teardown has begun". A drain abandoned part-way, because the future driving it
was dropped or a caller's waker panicked, must **resume** rather than skip to
releasing. A guard on "started" would let a second call return as though
teardown were done, leaving the ring open and everything in it stranded. That
coupling is load-bearing enough that a `#[cfg(test)]` `teardown_started` flag
exists solely so a test can prove it abandoned a drain that had really begun.

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

The one genuine thread boundary is the completion event's thread-pool wait. It
is genuinely the only one. There used to appear to be a second — a `wake` event
the application signalled to tell the driver work was queued — but everything
that could signal it holds `Rc`s, so it only ever travelled between two points on
the driver's own thread, by way of a kernel object and an OS thread pool. That
cost more than the I/O it was announcing; see [performance.md](performance.md).

#### Waking the driver from its own thread

The nudge is now `DriverInner::nudged`, a flag, plus `driver_waker`, the waker of
whichever task is parked in `Driver::drive`.

The flag is durable state rather than an edge, and that is load-bearing. Within
one pass the driver releases its borrow and calls back into caller code twice —
waking futures and delivering reports — and either can queue work or request
shutdown, *after* the pass has already read whether it should keep going. A
signal that had to be caught in flight could be raised in that window and missed.
A flag raised there is still set when the park consults it.

The remaining window — between the park consulting the flag and the park being
suspended — is closed by construction. `Park::poll` consumes the nudge and arms
the wait in one `poll`, on one thread, with no `await`, no callback into caller
code and no re-entrant path between them. A nudge can only be raised by code
holding an `Rc` to the driver, which is to say by code on the driver's own
thread; while that `poll` runs, that thread is running that `poll`. So a nudge is
either raised before the check and seen by it, or raised after the poll returns
`Pending`, by which time the waker is visible to whoever raises it. There is no
third case.

`nudged` is deliberately *not* `pending_submit`, although the two are raised at
the same moments. `pending_submit` means "entries are queued and the kernel has
not taken them", is cleared by a successful `submit_pending` at the head of every
pass — before the park decision is reached — and is never set by
`Handle::escalate`. Reusing it would have left a shutdown request with no record
at all.

#### The wait is armed once, not once per park

`sys::ArmedEvent` owns an auto-reset event and one `RegisterWaitForSingleObject`
registration created in its constructor and torn down in `Drop`. The
registration omits `WT_EXECUTEONLYONCE`, so the OS re-arms it after every
callback and it serves every park for the driver's life.

That flag's absence is why the type owns its event rather than accepting one.
The platform's guidance is that an object which stays signalled — a manual-reset
event — must not be registered without it, or the callback "might be called too
many times before the event is reset". An auto-reset event is a requirement here,
not a preference, and constructing it inside makes handing the type the wrong
kind unrepresentable rather than merely documented.

The signal is **sticky**; the waker is not. A completion raised while nobody is
polling — between two parks, or while the driver is mid-pass — must still be seen
by the next poll, or it would be announced to nobody and the driver would park on
work the platform has already finished. The waker, by contrast, is replaced on
every poll, because the task driving may not be the one that parked last time.

The reference count handed to the OS belongs to the registration for its whole
life. The callback borrows the shared state without touching the count, and
`Drop` reclaims it **exactly once**, after a blocking
`UnregisterWaitEx(handle, INVALID_HANDLE_VALUE)` has proved that no callback is
running or can start. There is no `callback_ran` flag and no question of who
reclaims.

That blocking `UnregisterWaitEx` is load-bearing. Do not "optimise" it away. It
orders the unregister before the handle closes — the platform calls closing a
handle with a wait still pending undefined — and it is what makes the count safe
to reclaim. If it *fails*, `Drop` reclaims nothing and closes nothing: it leaks
both, deliberately, because the alternatives are undefined behaviour.

Taking that single blocking path is sound only because `Drop` can never run on a
callback thread. `ArmedEvent` is `pub(crate)` and carries a
`PhantomData<*const ()>` so it is `!Send` by construction, which makes that a
property the compiler checks rather than one a reviewer must remember. Making it
public would give the guarantee away and require the two-path teardown back —
non-blocking unregister when dropping from inside the callback, blocking
otherwise, with the reclaim deferred to callback exit.

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
- **Consuming the nudge anywhere other than the park.** `take_nudge` is the only
  place the flag may be cleared. Clearing it elsewhere lets the driver park on
  work it has been told about and will not be told about again.
- **Storing the driver's waker beyond the parked window.** `driver_waker` is
  documented as occupied only while parked. A waker left there keeps an
  abandoned driving task alive and makes "is the driver parked" unanswerable.
- **Asserting on park counts instead of pass counts.** A driver whose park
  ignored the nudge entirely still parks once per poll, so a park counter cannot
  tell "the nudge was honoured" from "the wait re-armed and suspended again".
  One of this crate's own wakeup tests passed against exactly that broken variant
  until it was changed to assert on passes.
