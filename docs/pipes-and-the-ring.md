# Pipes and the ring

Named pipe support is the first feature in this crate where a required operation
**cannot** be expressed as a ring operation. Reads and writes go through the ring
unchanged; the server-side accept cannot go through it at all.

That asymmetry is a property of the platform, not of this crate's design, and it
is written down here rather than smoothed over because a reader who assumes the
ring covers everything will draw wrong conclusions about shutdown, about
cancellation, and about what the driver is waiting for.

## What the ring can build

`windows` 0.62.2 exposes exactly seven `IORING_OP_*` constants and exactly six
`BuildIoRing*` functions, each of which hard-codes its own op code:

| operation | builder |
|---|---|
| `IORING_OP_NOP` | *none* — the op code exists, the builder does not |
| `IORING_OP_READ` | `BuildIoRingReadFile` |
| `IORING_OP_WRITE` | `BuildIoRingWriteFile` |
| `IORING_OP_FLUSH` | `BuildIoRingFlushFile` |
| `IORING_OP_CANCEL` | `BuildIoRingCancelRequest` |
| `IORING_OP_REGISTER_FILES` | `BuildIoRingRegisterFileHandles` |
| `IORING_OP_REGISTER_BUFFERS` | `BuildIoRingRegisterBuffers` |

The only function that accepts an `IORING_OP_CODE` as a parameter is
`IsIoRingOpSupported`, which is a query. **There is no generic submit path.** A
caller cannot describe an operation the API has no builder for, however well the
kernel might support it.

This was established by enumerating the entire op surface of `windows` 0.62.2 —
the version `Cargo.lock` pins — rather than by sampling it. The distinction
matters: "I looked and didn't find one" and "the set is closed and here it is"
support very different conclusions, and only the second justifies designing
around the absence.

**That enumeration is not gated, and cannot easily be.** Rust has no way to
assert that a dependency exposes no further constants, so a future `windows` that
added `BuildIoRingConnectNamedPipe` would leave this document quietly wrong
rather than failing a test. `ALL_OPS` in `io_ring/tests.rs` lists the seven codes
this crate queries, but it is hand-maintained and proves nothing about what the
crate it reads from contains. Recorded as an obligation in
[pending-work.md](pending-work.md), because an ungated obligation is the one that
gets forgotten: **a `windows` version bump is the moment to re-run this
enumeration.**

There is no `ConnectNamedPipe` in that table and no way to construct one.

## What that costs, concretely

Everything below is measured on real handles rather than inferred from the
documentation.

**Reads and writes are unaffected.** A pipe handle opened with
`FILE_FLAG_OVERLAPPED` is an ordinary handle to `ReadFile` and `WriteFile`, which
is what the ring issues. Registered buffers and registered file handles both work
against a pipe.

**The file offset is ignored.** A pipe has no file offset, and the platform does
not error on one — it silently consumes from the head of the stream. Two
successive 8-byte reads issued at offsets 4 and 99 against a pipe carrying
`0123456789ABCDEF` returned `"01234567"` and then `"89ABCDEF"`: consecutive
bytes from the head, with the offsets disregarded entirely. A write at offset 512
appended. This is why `File::read` and `File::write`, which supply an offset
from a cursor they maintain, refuse a pipe outright: they would otherwise report
success with bytes from somewhere the caller never asked for. The positional
`Handle::read` and `Handle::write` are unguarded, because there the caller
supplied the ignored offset knowingly.

**A read on a listening instance fails** with `ERROR_PIPE_LISTENING`, so the
accept cannot be faked by simply reading and waiting.

**A flush waits for the peer.** `FlushFileBuffers` on a pipe with unread bytes
does not complete until the peer reads them. Measured at over 180 seconds
directly, and a ring flush on an undrained pipe did not complete in 500 driver
passes. The consequence for shutdown is in
[pending-work.md](pending-work.md#a-pipe-flush-outstanding-at-shutdown-may-never-terminate).

## How accept works instead

`ConnectNamedPipe` is issued as an overlapped Win32 call. It returns
`ERROR_IO_PENDING` and completes by signalling the event in its `OVERLAPPED`,
which the crate turns into a waker wake through the same
`RegisterWaitForSingleObject` machinery the driver already uses for its own
completion signal.

Three consequences follow, and each of them is a promise this crate makes
elsewhere that has to be restated for pipes.

**The accept occupies no slab entry**, so the driver does not know it exists. It
is not counted in `awaiting_kernel`, it is not reaped, and it is not part of what
the drain waits for. A test asserts this, with a real ring operation outstanding
alongside so the count being compared is not zero against zero.

**Shutdown does not cancel it.** The driver's drain covers ring operations, and
an accept is not one. A `Server` dropped with an accept outstanding cancels it
itself, with `CancelIoEx`, and the discipline for what happens next is the
subject of the next section.

**The `OVERLAPPED` is the first this crate has ever constructed.** Every ring
submission passes an explicit offset scalar; nothing before this needed a
structure the kernel writes into asynchronously. The lifetime obligations are
exactly those [buffer-ownership.md](buffer-ownership.md) sets out for buffers —
the memory must stay at a stable address and must outlive the operation — applied
to a structure rather than a byte array.

## Teardown, and why it leaks on purpose

Dropping a `Server` with an accept outstanding has to answer one question: **is
the kernel finished with the `OVERLAPPED`?** Only if the answer is a definite yes
may the allocation be freed.

The answer comes from three Win32 calls that can each fail independently, so the
teardown decides before it acts — three pure functions, one per call, choosing
between five outcomes of which four leak. `teardown_action` reads the cancel,
`release_permits_collect` reads the unregister, and `collect_finished_it` reads
the status word afterwards rather than trusting that the blocking collect
honoured its own contract.

| outcome | what is known | action |
|---|---|---|
| `CancelIoEx` failed for any reason but `ERROR_NOT_FOUND` | **nothing** — what the cancel did is unknown | leak |
| `ERROR_NOT_FOUND` **and** the operation never completed | no I/O for this structure exists in the kernel | leak |
| `ERROR_NOT_FOUND` but the operation *had* completed | a terminal status exists | collect, then free |
| the cancel located the operation | a terminal status is coming | collect, then free |
| the blocking unregister failed | the thread pool may still consume the event | leak |
| the collect returned but the status is still pending | the kernel still owns it | leak |

The second and third rows are the pair worth dwelling on, because collapsing
them is the bug. `ERROR_NOT_FOUND` on its own does **not** mean the kernel never
had the operation — it equally means the operation already finished, and
measurement confirms both readings occur. Only `ERROR_NOT_FOUND` *together with*
"never completed" establishes that no terminal status will ever be written.
Treating the code alone as proof deadlocks the other way: the collect then waits,
with `bWait=TRUE`, on a status word nothing will update. That deadlock was found
by a mutation aimed at something else, and the rule is now held by a pure
function tested exhaustively over all four inputs plus mutation rows that fail it
in both directions.

Leaking means what it says: the `OVERLAPPED` allocation is forgotten, the event
handle is left open, and the `File` reference is forgotten so the pipe handle
stays valid. A handful of bytes and one handle are lost, per server, on a path
that should never be reached. The alternative is freeing memory the kernel may
write into afterwards, which is a use-after-free.

The teardown also releases the wait registration **before** it collects, rather
than relying on the fused release-and-close that `ArmedEvent`'s own `Drop` does.
A blocking collect and a callback that may still fire are two consumers of a
single auto-reset signal. Measurement found that configuration hanging 8 times in
200 on one run and 1 in 200 on another — and only when *both* the early release
and the cancel were removed. Each alone is sufficient to prevent the hang and
neither is necessary given the other, which is why both are kept and why the
mutation rows record that single-sided removals do not fire. The rate is
host-sensitive by an order of magnitude, so no threshold on it is ever the
criterion.

## Does this fit the ring model?

No, and it is worth saying so plainly, because the comfortable conclusion here is
that it does.

Half of a named pipe server maps onto the ring perfectly and half of it cannot
touch the ring at all. The half that cannot brings its own thread boundary, its
own cancellation path, its own teardown discipline, and its own exception to the
crate's shutdown guarantee. None of that is visible in the public API, which is
the point of the design — but it is all there, and a future maintainer who
assumes "operations go through the ring" will be wrong about this one.

The design that follows from accepting the misfit is better than the design that
would follow from denying it. Pre-created instance pools, a blocking-thread hop,
and a synchronous accept on a dedicated handle were all considered; each hides
the asymmetry somewhere less visible rather than removing it. The overlapped
route at least puts the exception where a reader can find it.
