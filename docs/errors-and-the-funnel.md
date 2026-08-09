# Errors and the funnel

Why this crate classifies platform errors in exactly one place, why each public
API nonetheless reports its own error type, and which of the two is the
load-bearing constraint.

> **This document previously concluded the opposite.** It argued that this
> crate's errors could not partition by API, and that conclusion has been
> refuted — by execution, not by argument. The revision is kept visible rather
> than tidied away, because most of the reasoning survived intact and it is worth
> knowing exactly which part did not. See [What this document got
> wrong](#what-this-document-got-wrong).

## The question

The crate used to have one `crate::Error` with 25 variants, returned by every
fallible API. So `IoRing::builder().build()` could return `PipeBroken`, and
`RegisteredBuffers::check_out()` could return `NoFileOffset`. Neither can
actually happen. A caller reading the signature cannot tell.

## The answer: classification funnels, error *types* do not

The two are separable, and conflating them is what produced the earlier wrong
conclusion.

- **Classification** — turning an `HRESULT` into a named condition — happens in
  exactly one place and must keep happening in exactly one place. That is a hard
  constraint, and the reasoning for it below is unchanged.
- **Which type carries the condition** is decided at the public boundary, where
  the API is known. That is a different question, and it has a different answer.

The crate now has six error types:

| Type | Variants |
|---|---|
| `io_ring::BuildError` | 4 |
| `io_ring::Error` | 4 |
| `buf::Error` | 2 |
| `runtime::Error` | 17 |
| `file::Error` | 10 |
| `pipe::Error` | 13 |
| **Total public slots** | **50** |

Fifty slots against the previous twenty-five. That is the honest cost: the
duplication is real and roughly doubles the number of variants a reader can
encounter across the crate. What it buys is that no single signature exposes more
than it can produce — `File::read_at` returns 10 possibilities instead of 25, and
`Client::read_at` returns 13 that are actually a pipe's.

The mechanism that makes this affordable is a single `Other` variant on each
type. A surface names the conditions it can produce and everything else falls
through *carrying its `HRESULT`*, so no type has to enumerate the platform.

## The constraint that is real: one classifier

This is the part of the original argument that survived, and it is worth
restating because the new design is built to respect it rather than to escape it.

### The driver classifies before it knows the operation

In `DriverInner::reap_completions`:

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
else, and identifying the operation would not help: `OpPayload` records no field
naming the API that issued it.

**This is still true, and the design does not change it.** The driver still
classifies into one type. What changed is that the classified value is converted
at the public boundary, where the API *is* known — so the driver never needs an
API discriminator, and the completion path is untouched.

The original document read this constraint as fatal to per-API types. It is only
fatal to classifying *at the funnel*. Deferring the choice of type to the
boundary was never in tension with it.

### Why a second classifier is forbidden

The crate's own words, in `pipe::client`:

> `ERROR_PIPE_BUSY` from a failed open and `ERROR_PIPE_BUSY` from a completion
> must produce the same variant, and two independent match arms are exactly how
> that stops being true after someone edits one of them.

This is the sharpest objection to per-API error types, and it is correct. Per-API
*classifiers* would reintroduce exactly this hazard.

The design's answer is not to note the risk but to remove the possibility. There
is one classification table, private to `error.rs`, mapping an `HRESULT` to a
`Condition`. Each per-API type is a **view** over that table, expressed as a
trait with one method per condition and *no default bodies*, so a new condition
is `E0046` at every view that has not handled it. `Condition` and `classify` are
private to their module, so a second table cannot be written against them at all.

Two doors were found and shut during implementation:

- A wildcard arm in a boundary conversion would silently route a *new* condition
  into the driver-only sink. `#[deny(clippy::wildcard_enum_match_arm)]` now makes
  that arm a compile error. A comment forbidding it was the previous guard, and
  a comment is not a guard.
- A new condition can be routed to an *existing* view method, which E0046 cannot
  see because no method is missing. This is the residual hazard, and it is
  recorded rather than solved: rerouting an arm looks like ordinary maintenance,
  which is exactly why it deserves naming.

## What this document got wrong

The earlier conclusion rested on a producer/consumer map that grouped the 25
variants by **which surface can produce them**, found 15 reachable from two or
more surfaces, and treated that as fatal.

Multi-surface reachability is not fatal. It is duplication, and duplication is
affordable when each type lists only what it can produce and demotes the rest.
The map answered "who produces this?" when the design needed "what can this API
surface?" — a different question with a different shape, and the document did not
notice it had substituted one for the other.

Three specific errors:

1. **"A `pipe::Error` would have exactly one variant of its own:
   `AcceptOutstanding`."** It has 13. The estimate followed from the map's
   producer-side cut: the pipe conditions are *constructed* in `error.rs`, so
   they were counted as belonging to no module rather than as reachable from the
   pipe surface.

2. **"Pipes and files share the same futures... there is therefore no pipe I/O
   error channel to give a distinct type to."** True when written, and no longer,
   because the work changed it: `Client` and `Server` now have their own
   `read_at`/`write_at` returning `pipe::Error`, and `Deref<Target = File>` is
   gone. The premise was a fact about the code rather than a constraint on it,
   and the document did not distinguish those.

3. **`NoFileOffset` (now `file::Error::NotSeekable`) was listed as
   pipe-reachable**, via `Client::into_file()` followed by a sequential read.
   That was correct at the time. Removing `into_file` closes the route:
   `Client::file()` hands out a `&File`, and `File::read`/`File::write` take
   `&mut self`, so a borrow cannot reach the cursor-tracking methods. The
   condition is now genuinely file-only.

`PipeListening` deserves a line of its own. The earlier map recorded at `:98`
that it is *both* classified from `ERROR_PIPE_LISTENING` and produced directly in
`pipe/server.rs`, and a later section was then written as though it were only the
former. It has three direct producers — `Server::file()`, `Server::disconnect()`,
and a defensive arm in the accept future — as well as the classified route. A
condition with two origins needs both pinned separately; asserting one and
generalising is how the distinction gets lost, and it got lost inside this
document once already.

## What is genuinely shared, and what that costs

Some conditions really are reachable from several surfaces, and under the new
design they are named by several types. That is the duplication the table above
prices at 50 slots.

It is not *divergence*, because every one of them is a view over the same table
entry: the same `HRESULT` produces the same condition everywhere, and a test
walks every condition through every view to prove it rather than asserting it.

Ten conditions still partition cleanly — ring construction's three
(`Unsupported`, `UnsupportedVersion`, `UnsupportedFeature`), the runtime's six
registration and shutdown conditions, and the pipe's `AcceptOutstanding`.

The two exhaustion conditions deserve care because they look identical and are
not. `io_ring::Error::QueueFull` is the kernel's submission queue;
`runtime::Error::TooManyOperations` is this crate's slab of operation slots. Both
mean "too many in flight", both bind at 65,536, and only the order of the two
checks separates them. They call for different remedies, so they are pinned by a
test that reaches *both* producers rather than one that asserts a negative.

## What was carved by construction

`io_ring::ops::MissingField`. The operation builders check that the `Option`s a
caller filled cover the ones the platform requires; the module makes no Win32
call, so nothing in it can reach the classifier. Its failure set is closed **by
construction rather than by inspection**.

That remains the strongest form a narrow error type can take, and it is worth
distinguishing from the rest: the six types above are closed by *design and
enforcement*, which is weaker than closed by construction and needs its guards to
stay honest.

## The honest summary

The request that started this work was "each API should use its own error enum".
The answer is yes — at a cost of doubling the total variant count, and only
because classification stays in one place while the *type* is chosen at the
boundary.

The previous answer was no, and it was wrong for an instructive reason. It
established a real constraint on the completion path and then applied it to a
question the completion path does not decide. The constraint is still there; it
simply never reached as far as the conclusion drawn from it.
