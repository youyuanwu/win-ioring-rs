# Platform notes

How Windows IoRing actually behaves, established by probing it rather than by
reading about it. Every entry here contradicted a reasonable assumption, and
several of them caused real bugs before they were pinned down.

Findings were verified on a Windows 11 development host with the `windows` crate
at 0.62.2. Where a value is host-specific it says so.

## Availability is a load-time property, not a runtime one

`windows-link` uses `raw-dylib` with `+verbatim`, so
`api-ms-win-core-ioring-l1-1-0.dll` is a **static** import. A host without
IoRing fails to load the *process*.

There is therefore no such thing as a graceful "this host does not support
IoRing" error, and the crate does not pretend to offer one. `Error::Unsupported*`
covers only the shortfalls a host *with* IoRing can still have: a version below
what was asked for, a missing feature flag, or an unimplemented operation.

## Version is an open integer range

The development host reports `MaxVersion = 400`. The highest named constant in
`windows` 0.62.2 is `IORING_VERSION_3 = 300`.

So the version must be treated as an open integer and never matched
exhaustively against known constants. Code that assumes the reported version is
one it has a name for will break on a newer host.

## Queue sizes are rounded up to powers of two

Requesting 20/20 yields 32/64. Any code that needs the ring's real capacity —
detecting a full submission queue, for instance — must read it back from
`GetIoRingInfo` rather than trusting what it asked for.

## Read semantics are not what you would guess

| Request | Result |
|---|---|
| More bytes than remain in the file | **Short read that succeeds** |
| A read with nothing available (at or past EOF) | **Fails** with `ERROR_HANDLE_EOF` (`0x80070026`) |
| Zero-length read at a valid offset | Succeeds, transfers nothing |
| Zero-length buffer with no allocation at all | Submitted normally, not rejected |

The second row is the surprising one: end-of-file arrives as an *error*, not as a
zero-byte success, and it is distinct from a short read.

## There is no unregister

The platform exposes no `Unregister*` entry point at all. Registering an
**empty** set looks like the obvious substitute and is not one: the builder
accepts it, and it then fails at **completion** time with `E_INVALIDARG`.

A registration can therefore only be superseded by a different non-empty set, or
released by closing the ring. This is why registration in this crate is a
permanent transfer of ownership.

Note the asymmetry: a zero-**extent** buffer descriptor *is* accepted. Only a
zero-**count** registration fails.

## A closed ring handle faults rather than erroring

`PopIoRingCompletion` given a closed ring handle raises
`STATUS_ACCESS_VIOLATION`. `GetIoRingInfo` returns plausible-looking nonsense
with an `Ok` status.

The platform does not reliably validate a stale ring handle, so this crate
guards every method with an explicit closed check. That guard is load-bearing,
not defensive: without it the *safe* methods on `IoRing` would not be safe.

`IoRing` also has no `Drop` implementation, deliberately — closing a ring while
the kernel may still be working is a decision the raw layer refuses to make on
the caller's behalf.

## The submission-queue-full error has a dedicated family

A full submission queue reports `0x80460002`, which is
`IORING_E_SUBMISSION_QUEUE_FULL`. The bindings define a whole `IORING_E_*` family
at `windows-0.62.2` `Foundation/mod.rs:5704-5711`; this crate classifies that one
specifically and passes the rest through as OS errors.

## Write-through is refused for cached I/O

`FILE_WRITE_FLAGS_WRITE_THROUGH` fails with `0x800701FD` on a handle opened for
cached I/O — which is what `File::create` gives you.

## Feature flags on the development host

`FeatureFlags = 2`, i.e. `IORING_FEATURE_SET_COMPLETION_EVENT` — **not**
user-mode emulation, which is what the value initially looked like. All seven op
codes report as supported.

The driver depends on completion-event signalling, so the builder requires this
flag by default.

## Rust-side gotchas that cost time

**`std::fs::remove_file` does not prove a handle is closed.** Rust's standard
library opens files with `FILE_SHARE_DELETE`, so deletion succeeds while handles
are still open. To observe that a handle has been released, open the path with
`share_mode(0)` — that fails with a sharing violation for exactly as long as any
other handle exists. Probing the raw handle *value* is not a substitute: another
thread can be handed the same value moments after it closes.

**`File::create` opens write-only and truncates.** Reading back needs a separate
handle, and using one path as both source and destination silently empties the
source.

**A Tokio current-thread runtime cannot advance another task without an await
point.** This makes "the operation is still outstanding immediately after
dropping its future" a deterministic assertion rather than a racy one, which
several tests depend on.

**Doc-tests run with the crate directory as their working directory.** A
doc-test opening a fixture must name it relative to `crates/win-ioring`, not the
workspace root.
