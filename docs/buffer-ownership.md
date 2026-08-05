# Buffer ownership

Why the buffer traits take ownership, what the other completion-based Rust
runtimes do, and what it would take to accept a borrowed slice instead.

This exists because "why can't I just pass `&mut [u8]`?" is the first question
anyone asks about this API, and the answer is not the obvious one.

## The rule

**Every completion-based Rust runtime requires `'static` buffers** — with one
instructive exception, [kimojio](#the-exception-kimojio), which takes borrowed
slices and pays for them with a blocking drop. Setting that aside, none of the
mainstream runtimes accepts a borrowed `&'a [u8]`. Some accept `&'static [u8]`,
which is a reference in the type system but is equivalent to ownership for our
purposes: there is nothing behind it that can ever be freed.

## What the ecosystem actually does

Verified by reading the sources, not by reading the docs.

| Crate | Trait bound | Borrowed forms accepted |
|---|---|---|
| [tokio-uring](https://github.com/tokio-rs/tokio-uring) | `unsafe trait IoBuf: Unpin + 'static` | `&'static [u8]` for `IoBuf`. **Nothing** for `IoBufMut` — reads always need an owned buffer. |
| [compio](https://github.com/compio-rs/compio) | `trait IoBuf: 'static` | blanket impls for `&'static B` and `&'static mut B`, plus unsized impls for `[u8]`, `[u8; N]`, `str`. `&'static mut [u8]` therefore works for reads. |
| win-ioring (this crate) | `unsafe trait IoBuf: 'static + Unpin` | none — `Vec<u8>`, `Box<[u8]>`, `[u8; N]` only |
| [kimojio](https://github.com/Azure/kimojio-rs) | none — no buffer trait at all | **borrowed `&'a [u8]` / `&'a mut [u8]`**, the only public buffer API. See below. |

Sources: `tokio-uring/src/buf/io_buf.rs`, `io_buf_mut.rs`;
`compio/compio-buf/src/io_buf.rs`; our `crates/win-ioring/src/buf.rs`.

Two details worth carrying forward:

- compio's unsized `impl IoBuf for [u8]` is what makes everything else fall out
  — `Box<[u8]>`, `Rc<[u8]>`, `&'static [u8]` and `&'static mut [u8]` are all
  reached through blanket impls over it, rather than being enumerated
  one at a time the way we and tokio-uring do it.
- compio's `IoBuf` is a **safe**, dyn-compatible trait whose accessor returns
  `&[u8]`; ours and tokio-uring's are `unsafe` and hand back a raw pointer.
  compio keeps the `unsafe` on the narrower `SetLen`/`set_len` obligation
  instead. That is a genuinely different factoring of the same contract, and
  worth considering if the trait is ever revisited.

## Why ownership, and not lifetimes

The instinct is that this is a lifetime problem the borrow checker ought to be
able to solve. It is not. It is a **drop** problem.

1. The kernel may read or write the buffer at any point between submission and
   completion. That window is not bounded by anything the caller controls —
   see `design.md`, "the constraint everything follows from".
2. A future can be dropped at any await point. Dropping it does **not** withdraw
   the operation; cancellation is a request, not a revocation.
3. `Drop` is synchronous. It cannot await the cancellation it just requested.
   Async drop is not stable.
4. `mem::forget` is safe. So the region a borrow covers can end *without `Drop`
   ever running at all* — this is the "leakpocalypse" that removed scoped
   threads from Rust 1.0 and is why `std::thread::scope` blocks on exit.

Given (1)–(4), a runtime that accepted `&'a mut [u8]` would have no sound move
available at drop time. With an **owned** buffer it does: keep it, and release it
when the completion is finally dequeued. With a `'static` **reference** it also
does, trivially — there is no allocation to release.

### The contrast that makes this click

Readiness-based APIs — epoll, kqueue, `WSAPoll` — take `&mut [u8]` happily. Not
because they are better designed, but because the actual `read()` there is a
*synchronous* syscall. The buffer is borrowed across the syscall and never
across an await point. Completion-based I/O inverts exactly that property, and
the buffer API inverts with it.

This is also why a readiness-shaped facade over a completion-based OS (what
several crates do on Windows) ends up copying: it has to own a buffer
internally to keep the syscall's borrow window short.

## Is a borrowed buffer possible in principle?

Yes — but only by reintroducing a point where the caller cannot proceed until
the kernel is provably done. Every viable design is some flavour of that
barrier.

### Block on drop (cancel, then wait)

`Drop` issues the cancel and then blocks until the completion arrives. Sound,
and it is exactly the trick `std::thread::scope` uses. **This is not
hypothetical — it is what kimojio ships**; see the next section for how it
works in practice and what it costs.

Costs: the block is unbounded — IoRing cancellation is not guaranteed prompt,
and an operation against a dead endpoint may not complete for a long time.
Worse for us specifically: our driver is single-threaded, so a blocking drop
would have to **pump the ring from inside `drop`** or it would deadlock against
the very driver that would deliver the completion. That is a large amount of
re-entrancy in the most safety-critical path in the crate.

### A stack-pinned, unforgettable operation handle

Never hand the caller an owned value — only a `Pin<&mut _>` from something like
`pin!`. `Pin`'s drop guarantee then says the memory cannot be invalidated
without `Drop` running first, which closes the `mem::forget` hole from (4) and
makes a blocking cancel-on-drop actually reliable.

Costs: the operation can no longer be moved, boxed, spawned, or stored in a
collection. It stops composing with essentially every combinator, which
defeats the point of being runtime-agnostic.

### Hand ownership to the runtime instead of the caller

The buffer belongs to the runtime permanently and the caller borrows it for the
duration of one operation. io_uring calls these provided/fixed buffers;
tokio-uring exposes them as `FixedBuf`; **we already have this as
`Registered<T>`**.

This is the one that is already solved. It is not really an exception to the
rule — ownership simply moved rather than disappeared.

### Copy through an internal owned buffer

Always sound, costs one `memcpy` per operation, and gives a completely ordinary
`&mut [u8]` signature. Worth remembering as the pragmatic option if a
convenience layer is ever wanted; it is what readiness-emulation layers on
Windows already do.

### Async `Drop`

The actual fix: let `Drop` await the cancellation. Not stable, and borrowed
buffers in completion-based I/O are one of its standing motivations. Nothing to
do here but watch it.

## The exception: kimojio

[Azure/kimojio-rs](https://github.com/Azure/kimojio-rs) is a thread-per-core
Linux io_uring runtime from Microsoft. It is worth studying closely because it
is architecturally the closest thing to this crate — single-threaded,
cooperatively scheduled, no locks or atomics — and it made the *opposite*
buffer choice and made it work.

Line references are against `main` as of this writing.

### It really is a borrowed slice

```rust
// kimojio/src/operations.rs:949
pub fn read<'a>(fd: &impl AsFd, buf: &'a mut [u8]) -> UsizeFuture<'a>

// kimojio/src/operations.rs:690
pub fn write<'a>(fd: &impl AsFd, buf: &'a [u8]) -> ErrnoOrFuture<UsizeFuture<'a>>
```

No buffer trait, no `'static`, no `BufResult` — an ordinary borrowed slice and
an ordinary `usize` back. This is the API everyone reaches for first.

### How the borrow is anchored

```rust
// kimojio/src/ring_future.rs:58
pub struct RingFuture<'a, T: Unpin, C: MakeResult<T>> {
    handle: Option<Rc<Completion>>,
    _marker: std::marker::PhantomData<(&'a (), T, C)>,
}
```

The lifetime is a pure `PhantomData` marker. It carries no data; its only job
is to make the borrow checker refuse to let the buffer die before the future.
The crate asserts this with a `compile_fail` doc-test at the top of
`operations.rs` that constructs a write, drops the buffer, and then awaits —
the same technique we use for our `!Send` assertions.

That handles the *lexical* case. It does nothing about early drop, which is the
actual problem.

### The blocking drop

```rust
// kimojio/src/ring_future.rs:287
impl<'a, T: Unpin, C: MakeResult<T>> Drop for RingFuture<'a, T, C> {
```

The drop path:

1. Cancel the completion.
2. Decide whether it is safe to return. Safe if the state is `Idle` (never
   submitted — submission is lazy, on first poll), or `Completed`/`Terminated`.
   Also safe if the completion owns its resources, because those live as long as
   the `Rc<Completion>`.
3. Otherwise — `Submitted` with `CompletionResources::None`, meaning **borrowed**
   memory is in the kernel's hands — loop on `submit_and_complete_io` until the
   completion leaves that state.

Step 3 is a blocking pump of the io_uring from inside `Drop`. The comment says
plainly that the alternative is the kernel reading or writing memory that is no
longer valid, and there is a `TODO` to revisit it when `AsyncDrop` stabilises —
the same conclusion this document reaches independently.

### It kept the owned path too

```rust
enum CompletionResources { None, Timespec(..), Box(Box<dyn Any>), Rc(Rc<dyn Any>), InlineBuffer([u8; 8]) }
```

Anything other than `None` is owned by the `Rc<Completion>`, and drop returns
immediately. kimojio uses this for runtime-internal structures — the boxed
`Timespec` behind a timeout at `operations.rs:1279`, socket addresses, and an
inline 8-byte buffer for tiny payloads — but does **not** expose it as a public
owned-buffer API for reads and writes.

So kimojio is really a hybrid, and the split is the interesting part: *owned
where the runtime controls the buffer, borrowed where the caller does, with the
blocking drop as the price of the second*.

### `io_scope`: amortising the stall

A blocking drop per abandoned operation is expensive, and `select!` /
timeout patterns abandon operations constantly. `io_scope`
(`operations.rs:1899`) is the answer: I/O started inside the scope registers
into a scope-level list rather than blocking on its own drop, and the scope exit
does one batched cancel-everything-then-wait
(`io_scope_cancel_and_wait_internal`, `operations.rs:1858`).

One stall instead of N. But still a stall, and kimojio's own documentation is
candid about it:

> Note that this is a blocking call. Other tasks in this uringruntime thread
> will not make progress until this call returns. […] If there are I/O that do
> not respond quickly to cancellation then that could cause stalls.

That is the real cost of borrowed buffers, and it is a *runtime-wide* cost, not
a per-task one. On a thread-per-core runtime, one uncancellable operation stalls
an entire core.

### One gap worth noting

`RingFuture` is `Unpin` — it holds only an `Rc` and a `PhantomData`. So
`Pin::new(&mut fut)`, poll once to submit, then `mem::forget(fut)` appears to
skip the blocking drop entirely while ending the borrow region, which is the
classic leak-hole described above. I derived this from reading, and have not
tested it or checked whether something upstream prevents it — treat it as a
question to ask rather than a defect to report. It is, however, exactly the
hazard that owned buffers remove by construction, and the reason the `Pin`-based
"unforgettable handle" variant exists.

### Why we could not simply copy this

The design is sound and proven, but three things make it materially harder here
than on io_uring:

- Step 3 requires pumping the ring from inside `Drop`. Our completion path
  borrows `DriverInner` through a `RefCell`, and our design notes already
  record that waking futures or invoking the error observer under that borrow
  panics. A blocking drop would be re-entrant into exactly that path.
- kimojio's futures reach the ring through `TaskState::get()`, ambient
  per-thread runtime state. We deliberately have no ambient state — a future
  holds a *weak* driver reference, and the driver may already be gone.
- IoRing cancellation is a request with no promptness guarantee, and
  `PopIoRingCompletion` on a closed ring faults outright (see
  `platform-notes.md`). The "wait until the kernel acknowledges" loop is a much
  less comfortable proposition when the ring may be mid-teardown.

None of that is fatal. It is a real option, and this is the reference
implementation to read if we ever take it. But it is a driver-architecture
change, not a buffer-trait change.

## What this means for this crate

Our bound is already `'static + Unpin`, the same shape as tokio-uring's, but we
only implement the traits for `Vec<u8>`, `Box<[u8]>` and `[u8; N]`.

**Proposed (not yet implemented):** add

```rust
unsafe impl IoBuf for &'static [u8]
unsafe impl IoBuf for &'static mut [u8]
unsafe impl IoBufMut for &'static mut [u8]
```

This is sound under the existing contract without weakening anything — a
`'static` reference satisfies stability and validity by construction, and
exclusivity follows from `&mut` being unique. It costs nothing at runtime, it
closes the gap with both tokio-uring (which has the shared case) and compio
(which has both), and it lets callers use leaked or statically allocated
buffers without a wrapper type.

Two notes for whoever implements it:

- `set_buf_init` has nothing to record for a slice, since capacity and length
  are the same. That is explicitly permitted by the `IoBufMut` docs — the
  authoritative transfer count is the operation's result.
- The `Box::new`-before-taking-the-pointer rule in `design.md` is about buffers
  stored *inline*, like `[u8; N]`. A reference is a pointer already, so boxing
  it does not move the bytes and the hazard does not apply. Do not let that
  lull you into skipping the box — the payload boxing is structural.

Whether to go further and adopt compio's unsized-plus-blanket factoring (which
would give `Rc<[u8]>`, `Arc<[u8]>` and friends for free) is a separate and
larger question, since it changes the trait's shape rather than adding impls.

## A measured note on compio's fill length

Everything above about compio's `IoBuf`/`IoBufMut` is read from its source. One
thing this repository has now *measured*, while building the compio comparison
backend, is worth recording beside it because it is not visible in the trait
definitions.

**compio's `read_at` takes no length. It fills to the buffer's capacity.** For a
`Vec` that is `capacity()`, not `len()` — a 4096-byte request against a buffer
holding 8192 bytes of capacity transfers **8192**. The bound is `Slice`:
`buffer.slice(..len)` limits the fill to `min(len, capacity)`. This is measured,
not inferred, and it is pinned by the
`over_capacity_reads_are_bounded_by_the_request` test in
`crates/win-ioring-bench/src/backends/compio.rs`, which fails on both the
transferred count and the recovered length if the slice is removed.

The recovered length is the second half of it. On completion compio applies
`SetLen::advance_to`, which sets the length **only if the new length is greater
than the current one**
(`compio-buf-0.8.3/src/io_buf.rs:759-767`) — so the length after a read is
`max(pre-read length, delivered)` rather than the delivered count. A caller that
reads the buffer's length to find out how much arrived will be wrong whenever the
read was short. The transfer count in the operation's result is the authoritative
figure, which is the same rule this document states for `set_buf_init` above, met
from the other direction.

## Sources

- `tokio-uring/src/buf/io_buf.rs`, `src/buf/io_buf_mut.rs`,
  `src/buf/fixed/handle.rs`
- `compio/compio-buf/src/io_buf.rs`, `compio-buf/src/lib.rs`
- `kimojio/src/operations.rs`, `kimojio/src/ring_future.rs`
  (`Azure/kimojio-rs`, MIT)
- Rust RFC 1084 / "leakpocalypse", and the removal of `std::thread::scoped`
