# Errors and the funnel

Why this crate has one error type for almost everything, why that is not
laziness, and what it would cost to change.

This document exists because "each API should report its own error type" is a
reasonable-sounding idea that someone will have again. It was tried, costed, and
mostly declined — and the reasons are structural rather than aesthetic, so they
will still hold next time unless the architecture changes.

## The question

`crate::Error` has 25 variants and every fallible API in the crate returns it.
So `IoRing::builder().build()` can return `PipeBroken`, and
`RegisteredBuffers::check_out()` can return `NoFileOffset`. Neither can actually
happen. A caller reading the signature cannot tell.

The obvious fix is one error type per module: `io_ring::Error`, `runtime::Error`,
`file::Error`, `pipe::Error`. The obvious fix does not work.

## The answer: the errors do not partition by API

Grouping all 25 variants by which public surface can produce them gives **10
reachable from exactly one surface and 15 reachable from two or more**. The
shared 15 include every variant a caller is most likely to match on.

Three structural reasons, each checkable.

### 1. The pipe error variants have no producer in `pipe/`

`PipeBusy`, `PipeBroken` and `PipeNoPeer` are constructed in exactly one place in
the entire crate: `Error::from_hresult` in `error.rs`. Nothing under `pipe/`
constructs them.

They are not pipe-module errors. They are values that `runtime::ReadFuture` can
yield, and a file read can yield them just as a pipe read can, because the
classifier matches on the platform code alone and a code is all it has.

### 2. Pipes and files share the same futures

Not "similar futures" — the same ones. `Server::file()` hands out a `&File`, and
`Client` implements `Deref<Target = File>`, so a `Client` *is* a `File` for every
purpose including method resolution. Neither type exposes a read, write or flush
of its own; pipe I/O is `Handle::read` and `Handle::write`, returning
`ReadFuture` and `WriteFuture`. See `docs/pipes-and-the-ring.md`, which reaches
the same conclusion from the other direction.

There is therefore no pipe I/O error channel to give a distinct type to. A
`pipe::Error` would have exactly one variant of its own: `AcceptOutstanding`.

The converse trap is sharper, and it is where a first pass at this went wrong.
`NoFileOffset` lives in `file.rs` and looks like a file error. It exists **for
pipes**: `File::read` and `File::write` track a cursor, a pipe has no meaningful
file position, and the platform ignores the offset rather than rejecting it — so
the crate refuses those two calls on a pipe rather than returning wrong data
successfully. It is documented as pipe behaviour on `pipe::Client`. Grouping it
under "file" because of where it is constructed gets it exactly backwards.

### 3. The driver classifies the error before it knows the operation

In `Driver::reap_completions`:

```rust
let result = match cqe.ResultCode.ok() {
    Ok(()) => Ok(cqe.Information as Transferred),
    Err(e) => Err(Error::from(e)),          // classified here
};

let Some(payload) = self.slab.complete(token) else {   // identified five lines later
    continue;
};
```

At the moment of classification the driver holds an `IORING_CQE` and nothing
else. And identifying the operation would not help: `OpPayload` has seven fields
— buffer, file, slot, registered buffer, registered file, pending registration,
and a sequential-I/O drop guard — and **none records which API issued the
operation**.

So a per-API error type on the completion path would require the driver to carry
an API discriminator it does not have and has no reason to want. This is the
constraint that settles the design: **on the completion path there is only one
error type by design**, and the single funnel is the expression of that, not an
accident.

## What is irreducibly shared

| Variant | Why it cannot belong to one module |
|---|---|
| `Os` | the `from_hresult` fallback, reachable from every surface |
| `QueueFull` | slab exhaustion *and* a platform code |
| `RingClosed` | `ensure_open` guards every ring touch |
| `UnsupportedOp` | `ensure_op_supported` is public *and* called by every submit path |
| `ShuttingDown`, `AbandonedAtShutdown` | teardown resolves any operation |
| `BufferTooSmall`, `UninitializedWriteRange` | `buf`'s check functions are public *and* called by `try_read`/`try_write` |
| `MissingField` | driver registration reports it too |
| `PipeBusy`, `PipeBroken`, `PipeNoPeer`, `PipeListening` | one classifier, reachable from all four surfaces |
| `OperationOutstanding`, `NoFileOffset` | produced in `file.rs`, reachable on a pipe through `Deref` |

Ten variants do partition: `Unsupported`, `UnsupportedVersion` and
`UnsupportedFeature` (ring construction only); `InvalidRegisteredIndex`,
`RegisteredRangeOutOfBounds`, `BufferCheckedOut`, `RegistrationSuperseded`,
`RegistrationPending` and `ShutdownStalled` (runtime only); `AcceptOutstanding`
(pipe only).

A single method, `Handle::read`, can return **twelve** of the twenty-five.

## Why a second classifier is forbidden

Any design that gives the pipe *setup* path its own error type has to decide what
`ERROR_PIPE_BUSY` means at the point a client fails to open. The crate already
answers this, in `pipe::client`:

> Routes through `Error::from_hresult`, which is the same funnel every ring
> completion passes through, rather than repeating the code comparisons here.
> That matters more than it looks: `ERROR_PIPE_BUSY` from a failed open and
> `ERROR_PIPE_BUSY` from a completion must produce the same variant, and two
> independent match arms are exactly how that stops being true after someone
> edits one of them.

A `pipe::SetupError` must therefore either re-derive the pipe codes in a second
match — the thing this forbids — or convert from the shared classifier anyway, in
which case it is not closed and buys no precision.

Worth recording plainly: **the first draft of the design work behind this change
proposed `pipe::SetupError`**, and cited lines seven away from the rationale
against it. It was the shape that made the original request look achievable,
which is exactly why it needed the most scrutiny and got the least. Review caught
it.

## What was carved, and why that one

`io_ring::ops::MissingField`. The operation builders' `build()` methods do
nothing but check that the `Option`s the caller filled cover the ones the
platform requires. The module imports Win32 *types*, but contains no `unsafe`
block and makes no Win32 call, so nothing in it can reach the classifier — every
failure is an `Option::ok_or`. The failure set is closed **by construction rather
than by inspection**. Four methods went from 25 reachable variants to 1.

That is the test a carve has to pass here: not "does this module feel distinct"
but "is this surface's outcome set closed without consulting the platform".
Almost nothing in this crate passes it, which is the whole finding.

## What was declined

`io_ring::BuildError` and `runtime::RegistryError` were fully costed and not
taken; see `docs/pending-work.md` for the numbers and the reasoning, so that
proposing them again starts from the evidence rather than from scratch.

The short version: carving them would have left the retained type at 22 of 25
variants while duplicating 8 conditions across four types. The surfaces they
narrow are touched once at startup; the surface callers touch in a loop would not
have been narrowed at all. A refactor whose gain is documentary is not worth a
breaking change.

## The honest summary

The request that started this work was "each API should use its own error enum".
The answer is that this crate cannot do that, for reasons that are properties of
the ring model rather than of this implementation: one completion queue, one
classifier, and a deliberate decision that a pipe is a file once it is connected.

One surface could be carved and was. The rest is not a backlog item — it is a
finding.
