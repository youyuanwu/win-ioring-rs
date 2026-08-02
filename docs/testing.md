# Testing

## Commands

The full set CI runs:

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets --all-features
cargo test --workspace --all-features --doc
$env:RUSTDOCFLAGS='-D warnings'; cargo doc --workspace --no-deps
```

**The doc-test step is not optional.** `--all-targets` excludes doc-tests, and
this crate's compile-time guarantees are asserted entirely by `compile_fail`
doc-tests — skipping the step silently skips those assertions.

## Approaches worth knowing about

### Executor equivalence

"Runtime agnostic" is a claim that can only be held to account by running the
same work under two unrelated executors and comparing the results.

`crates/win-ioring-tests/src/scenario.rs` records a transcript of about twenty
operations — reads, writes, cursor movement, errors, registration, flush, a stale
cancellation. `tests/executor_equivalence.rs` runs it under Tokio's `LocalSet`
and under the workspace's own executor, and compares the transcripts line for
line.

A separate test runs the scenario twice under one executor first. Comparing two
nondeterministic transcripts would prove nothing, so determinism has to be
established before equivalence means anything.

### Compile-fail doc-tests

Five `compile_fail` snippets in `crates/win-ioring/src/lib.rs` assert that
`Driver`, `Handle` and operation futures fail a `Send` bound; that a second
sequential operation cannot start while the first future is alive; and that the
buffer contracts cannot be implemented safely.

Each is paired with an otherwise-identical snippet that **does** compile, so a
snippet failing for an incidental reason would show up.

Each was verified to fail for its *intended* reason by temporarily removing the
marker and reading the compiler output: `E0277 cannot be sent between threads
safely`, `E0499 cannot borrow as mutable more than once` (with "first borrow
later used here", confirming NLL does not end the borrow early), and `E0200
requires an unsafe impl declaration`. Repeat that check if you change a snippet.

### Dependency policy

`tests/dependency_policy.rs` parses the crate manifest *and* the workspace
manifest and asserts no async runtime is reachable.

It resolves every form Cargo accepts — `[dependencies.tokio]`, target-scoped
dependencies, `package =` renames, and workspace inheritance — because the first
three versions of this test could each be bypassed by simply spelling the
dependency differently. A third test runs the collector over a synthetic manifest
containing every form, so the collector losing one is caught rather than silently
weakening the policy.

### Proving that memory is *not* freed

Several guarantees are about something not happening: a superseded registration
stays alive, an unquiet teardown abandons rather than releases. Nothing
observable occurs in those cases, so a test can otherwise only assert that no
call returned an error — which is exactly the kind of test that passes while
proving nothing.

`win_ioring_tests::counting::CountingBuf` is a buffer that increments a shared
counter when dropped. Use it whenever the property under test is an absence.

### Observing handle release

Use `opens_exclusively` (in `tests/file_tests.rs`): opening the path with
`share_mode(0)` fails for exactly as long as any other handle to the file exists.
See [platform-notes.md](platform-notes.md) for why deletion does not work as a
probe.

## Writing tests that actually test something

This came up often enough to be worth stating:

- Dropping a `Pin<&mut F>` does not drop the future.
- `Handle::outstanding()` counts `Built` slots, so it cannot prove the
  *submitted* drop path ran.
- Asserting an operation is still outstanding after a drop is racy for a small
  local read — unless nothing has awaited in between, since a current-thread
  runtime cannot advance the driver without an await point.
- A conditional assertion (`if still_outstanding { assert!(...) }`) may never
  execute. Prefer proving the precondition holds.
- **Verify a new test fails against the old behaviour.** Several tests in this
  suite were confirmed load-bearing by temporarily reverting the fix; the
  closed-ring guard test dies with `STATUS_ACCESS_VIOLATION` without it.

## Fixtures

Read tests use `testdata/sample.txt`, a purpose-built fixture. They previously
used `README.md`, which coupled every read test to the project's documentation:
a rewrite would silently change what the tests read, and a shorter README would
break the ones reading a fixed number of bytes. Do not shorten the fixture below
512 bytes, and prefer appending to editing so existing offsets keep pointing at
the same bytes.

## A note on the build cache

This repository's cargo incremental cache has repeatedly served **stale test
binaries** after a source file was restored from a backup copy, producing results
that look impossible — a test failing with a fix that is demonstrably present in
the file. If you hit that, touch the file and re-run before believing the result.
