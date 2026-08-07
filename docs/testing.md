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

Six `compile_fail` snippets in `crates/win-ioring/src/lib.rs` assert that
`Driver`, `Handle` and operation futures fail a `Send` bound; that a second
sequential operation cannot start while the first future is alive; that the
buffer contracts cannot be implemented safely; and that a registered buffer's
handle cannot be named while an operation holds it.

Each is paired with an otherwise-identical snippet that **does** compile, so a
snippet failing for an incidental reason would show up.

Each was verified to fail for its *intended* reason by temporarily removing the
marker and reading the compiler output: `E0277 cannot be sent between threads
safely`, `E0499 cannot borrow as mutable more than once` (with "first borrow
later used here", confirming NLL does not end the borrow early), `E0200 requires
an unsafe impl declaration`, and `E0382 borrow of moved value`. Repeat that check
if you change a snippet.

### Dependency policy

`tests/dependency_policy.rs` parses the crate manifest *and* the workspace
manifest and asserts no async runtime is reachable.

It resolves every form Cargo accepts — `[dependencies.tokio]`, target-scoped
dependencies, `package =` renames, and workspace inheritance — because the first
three versions of this test could each be bypassed by simply spelling the
dependency differently. A third test runs the collector over a synthetic manifest
containing every form, so the collector losing one is caught rather than silently
weakening the policy.

### Getting a genuinely long-running operation

A read from a small local file completes almost immediately, so any test
asserting that an operation is *still in flight* is asserting a timing accident.
That is why the older shutdown tests all accepted either outcome.

`Pipe` (in `crates/win-ioring/src/runtime/mod.rs`, test module) creates a
connected named-pipe pair opened for overlapped I/O. A read on the server end
stays in the kernel until the client writes, which makes "still in flight at
shutdown" a fact and lets the test choose the moment it reports. Use it for
anything about ordering with respect to shutdown.

Its own premise is tested by `a_read_on_an_empty_pipe_stays_in_flight`, so a
platform change that made pipe reads complete eagerly would show up as that test
failing rather than as several others quietly going vacuous.

### Test seams in the driver

All `#[cfg(test)]` fields on `DriverInner`. Each exists because the path it
reaches has no reproducible natural failure mode:

| Seam | Reaches |
|---|---|
| `fail_next_submits: u32` | submission retry, and the unsubmittable-residue carve-out |
| `fail_next_cancels: u32` | the cancellation retry path |
| `cancel_attempts: u32` | *observes* — how many cancellations were attempted |
| `withhold_reaps: u32` | an operation that stays outstanding for a known number of drain steps |
| `fail_next_registration: bool` | a registration that fails at completion time |
| `parks: u32` | *observes* — how many times the driver actually suspended |
| `passes: u32` | *observes* — how many times the driver loop ran a pass |

Plus one outside `DriverInner`, on the wait primitive itself:

| Seam | Reaches |
|---|---|
| `ArmedEvent::fail_next_arm()` | a `RegisterWaitForSingleObject` that fails, so a driver that cannot be woken reports rather than hangs |

`fail_next_arm` is a thread-local rather than a global, so tests running in
parallel cannot consume each other's injection. Verify by running the affected
tests both with `--test-threads=1` and at the default parallelism.

**`parks` and `passes` are both needed, and the distinction is the whole point.**
A driver whose park ignored the nudge entirely still suspends once per poll, so
`parks` alone cannot tell "the nudge was honoured" from "the wait re-armed and
suspended again". Only a pass counter separates them. See the vacuity traps
below — this one was caught by mutation, not by inspection.

**Counted, not boolean, and that matters.** The drain is unbounded, so a seam
that withheld reaping or refused cancellations *unconditionally* produces a test
that hangs forever rather than one that fails — a CI timeout instead of an
assertion. Give every such seam a budget the test outlives.

### Proving that two things did the same work

`crates/win-ioring-bench` compares I/O backends, and a comparison is only worth
quoting if the things compared did identical work. Two properties carry it, and
the split between them is the interesting part:

- the **issue trace** — every operation, in the order it was issued — is compared
  exactly. Issue order is the scenario's own and is deterministic.
- the **delivery digest** — a fold over what each operation put into
  application-visible memory — is folded *commutatively*, because completion
  order is legitimately nondeterministic above one operation in flight and must
  not enter the comparison.

Getting that split wrong in either direction breaks the benchmark: comparing
completion order makes it flaky, and not comparing delivered bytes lets a backend
report transfers whose data never reached anywhere readable.

Four tests in `crates/win-ioring-bench/tests/fairness.rs` deliberately weaken a
backend and require the run to be **rejected** rather than reported. Two of them
weaken a thread-pool backend and two weaken `compio-iocp`; none weakens a ring
backend, which is recorded in [pending-work.md](pending-work.md). Each takes a
*real* backend, wraps it so that it either skips one read in four or reports full
transfers whose bytes never reach anywhere readable, and drives it through
`harness::measure_combination`, which is the function a measured benchmark calls.
The weakening is applied by `Prepared::one`, the same call the timed closure
makes, so there is no second path a test could be passing on. Each asserts
*which* mismatch came back, so a run that fell over for an unrelated reason is
not counted as a pass, and a further test proves both weakenings change what was
delivered rather than what was issued — otherwise the two above could be passing
on the issue-trace comparison alone. A control case runs the matrix unweakened
and asserts it agrees and that it delivered a non-zero number of bytes, because
"everything agreed" is satisfied by everything doing nothing.

**Which backend is weakened, precisely.** Five of the weakening tests take fixed
positions from the available list, and the two `tokio::fs` backends are always
first, so on every host — with an I/O ring or without one — the backend those
five weaken is `tokio-pool-1` or `tokio-pool-512` and never a ring one. This
paragraph used to say each test took "every one the host can build, so a machine
without an I/O ring still runs them against two", which reads as though a ring
host runs them against every backend it has — five, today. It does not, and never
did. Two further tests,
added with the compio backend, select their backend *by identity* rather than by
position and so do weaken `compio-iocp` directly — that is what establishes the
machinery reaches a completion-based backend and not only the thread-pool ones.
Neither ring backend is weakened by any test. What the tests establish is that
`measure_combination` *rejects* a run that delivered less, and that function is
backend-agnostic — the weakening is injected above it, in a wrapper that knows
nothing about which backend it wraps. That the ring backends deliver what they
report is established by the control case, which does run every available
backend. Extending the weakening across the whole list is recorded in
[pending-work.md](pending-work.md).

**Two further tests fix *which run* is verified.** `measure_combination` runs an
untimed warm-up and then the timed iterations, and verifies the last timed
iteration rather than the warm-up. Every weakening above is uniform across runs,
so all of them are caught either way and none can tell the two apart — a
measurement that verified the warm-up would leave the whole suite green. So
`Weakness::HollowFromRun` weakens from a given run onward: from the first timed
iteration, leaving the warm-up honest, which is rejected only if a timed
iteration's trace reached the ledger; and from the last iteration alone, against
a ledger with nothing else in it, which nothing but the first-versus-last
comparison inside `measure_combination` can reject. Both were checked by
mutation, and each fails when its own line is removed.

**The first of those paragraphs used to describe tests that did not exist in that
form.** Until this was rewritten it said "Two tests deliberately weaken a backend
… and both must fail the run", and the tests it named built traces by hand and
compared them. They exercised the comparator; they never ran a backend and never touched
the measurement path, so a measurement that had stopped consulting the comparator
entirely would have left both passing. `docs/performance.md` carried the same
overstatement and is corrected the same way.

**This guarantee is held by mutation, not by inspection.** Two checks, both
recorded verbatim in the workflow artifacts for the Criterion migration:
`Ledger::observe` was deleted from its single call site, and `Weakness::SkipsWork`
was made a no-op. Neither alone is sufficient. The first establishes that the
comparator is consulted on the measured path; the second establishes that there
was a real difference for it to catch, which is what stops the first from being
satisfied by a test that would have failed anyway. Run both again if you move
where the ledger is consulted.

Two further mutations were checked when the run-order tests above were added, and
both were live defects in the coverage until then: replacing the verified outcome
with the warm-up's (`evidence.last.unwrap_or(warm)` → `warm`), and deleting the
first-versus-last trace comparison. Each is now caught by exactly one test and by
nothing else in the workspace.

**What this establishes is that the five backends did the same work — not that
the published timings are of that work.** The two are bound together by
`Timer::time`, whose production implementation is `CriterionTimer` in
`benches/comparison.rs`: it decides what Criterion measures, while the trace that
gets verified is populated by whatever the closure it was handed actually runs.
An implementation that ran that closure a few times and separately timed
something else would produce a fully green fairness account over timings of
nothing. The library's own tests cannot close this, because they all use
`Untimed`, which by design times nothing. Two things stand in for a test: the
binding is about ten reviewable lines in one file, and an unfiltered test-mode run
fails if any measured combination comes back **not timed**, which is what a timer
that stopped driving the closure would produce. Neither establishes that
Criterion timed *that* closure; that rests on reading those ten lines.

`cargo test --benches` runs the `comparison` Criterion target in **test mode** — one iteration
per benchmark, against `Config::small()` and a working directory of its own — so
`cargo test --workspace --all-targets` exercises preparation, warm-up,
verification and teardown end to end for **thirty-five** combinations.
`Config::small()` has depths `[1, 4]` where the benchmark configuration has
`[1, 8, 64]`, so the three rolling scenarios contribute 3 × 2 × 5 = 30, and bulk
read — which runs at the deepest configured depth alone — contributes 1 × 1 × 5 =
5. That is 35 against the 50 a benchmark run walks. (This paragraph said
thirty-six until it was checked against `config.rs`, then twenty-four until bulk
read was added, then twenty-eight; the path is the same one, but fifteen fewer
combinations travel it.) The bench target detects test mode by
Criterion's own rule rather than by testing for `--test` alone: a target that read
it wrongly would build a 256 MiB working file inside the test suite.

There are now **two** bench targets, and `--benches` does not reach both.
`--benches` and `--all-targets` select bench targets by the manifest's `bench`
flag, so they run `comparison` (which is `bench = true` by default) and skip
`unbuffered` (which is `bench = false`, being opt-in). The unbuffered target is
reached instead by its explicit `test = true`, which is what puts it in front of
`cargo test`. That flag is load-bearing rather than decorative: `bench = false`
on its own removes a target from *every* default command, including
`cargo check --all-targets`, so a planted type error in it goes undetected and
the target rots unnoticed while the build stays green. This was established by
experiment, not inference, and the reasoning is recorded in full beside the
stanza in `crates/win-ioring-bench/Cargo.toml`.

The depths `Config::small()` resolves to are the ones with teeth. They are what
every `cargo test` actually checks, so the closed-form depth predictions are
asserted at *those* values — a rolling mean of 3.90625 at 64 operations and depth
4, a batched mean of 2.5 — and not only at the default configuration's, which
nothing but a full benchmark run ever evaluates.

The bench crate also carries a driver-count observation seam, `drivers_built()`,
which is a plain counter and deliberately **not** `#[cfg(test)]`. What it observes
— how many drivers a run built, against how many ring combinations it measured —
is a property of a *benchmark run*, and a seam compiled out of the benchmark could
not observe it. Note what settles that property and what does not: the integration
test asserts a **lower** bound, because the counter is process-global and the test
binary is multi-threaded, so it excludes a driver being shared or skipped and
cannot see an excess. The excess — a driver per iteration — is caught by the
fairness account, which prints both numbers and marks them when they differ, and
by a full `cargo bench` run in a process that builds nothing else.

### Proving that memory is *not* freed

Several guarantees are about something not happening: a superseded registration
stays alive, and a buffer is *not* released before its operation reports. Nothing
observable occurs in those cases, so a test can otherwise only assert that no
call returned an error — which is exactly the kind of test that passes while
proving nothing.

`win_ioring_tests::counting::CountingBuf` is a buffer that increments a shared
counter when dropped. Use it whenever the property under test is an absence.
`win-ioring`'s own test module defines an equivalent locally, since it cannot
depend on the test crate.

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

### Vacuity traps caught in this suite

Every one of these was written, looked right, and **passed against a
deliberately broken variant** until it was tightened. They are recorded because
the pattern recurs, not because the specific tests matter.

- **The seam budget ran out before the interesting moment.** A test withheld
  reaping for exactly as many rounds as its own loop consumed, so by the time
  teardown started, reaping was no longer withheld and a teardown that gave up
  and leaked passed anyway. Set the budget *higher* than the loop consumes.
- **"Everything resolved" is satisfied by everything simply finishing.** A test
  meant to prove that refused cancellations are retried passed because the
  operations completed naturally. It needs a second assertion — that more
  cancellations were *attempted* than there are operations — and operations that
  cannot finish on their own, which is what `Pipe` is for.
- **The throttling bound was drawn against the wrong quantity.** A test asserted
  that stall reports numbered fewer than the rounds observed, and passed against
  a variant reporting on *every* round, because reporting only starts after a
  threshold. Draw the bound against the interval: `rounds / INTERVAL`.
- **After a read, `buf.len()` is the transferred count, not the original
  length.** Assert `capacity()` to prove the caller's own allocation came back;
  asserting `len()` proves only that a read happened.
- **A park counter cannot prove a wakeup was honoured.** A test asserted that a
  nudge raised against a parked driver increased the park count, and passed
  against a driver whose park ignored nudges completely — because such a driver
  still parks once per poll, so the count rises either way. It needed a separate
  *pass* counter. Found by mutating `take_nudge` to consume the flag without
  reporting it; two tests caught that mutation before the fix and four after.
- **A test that proves a check is consulted does not prove there was anything to
  catch.** Deleting `Ledger::observe` from the benchmark's measured path fails
  the weakened-backend tests, which looks conclusive — but it would fail equally
  for a weakening that did nothing, since a test asserting "this run is rejected"
  fails whenever the run is not rejected, for any reason. The second mutation is
  what closes it: making `Weakness::SkipsWork` a no-op must fail the test that
  depends on it and *pass* the one that does not, which is only possible if the
  weakening was real and specific. Both mutations are recorded verbatim in the
  workflow artifacts, and neither is worth running without the other.
- **A mutation you think you reverted can outlive the revert.** Restoring a file
  in a way that preserves its old modification time — `Move-Item` of a `.bak`
  copy will do it — leaves Cargo with no reason to rebuild, so the mutated
  artifact stays in `target/` and every later run measures it. This has produced
  two false findings in this repository: a matrix size read as 52 when it was 28
  at the time — it is 35 now — and an entries-per-submission figure read as 1.00
  when it is the configured
  depth. The second survived a code review and most of a day's investigation,
  because it was stable on every run and flipped whenever anything forced a
  rebuild — adding a package to `-p`, toggling an unrelated feature, inserting a
  `println!` — which reads exactly like codegen sensitivity and is nothing of the
  kind. After reverting a mutation, force the rebuild (touch the file, or restore
  by content rather than by moving a file over it) and re-take any measurement
  made since. If a result changes when you add an observer that cannot affect it,
  suspect a stale artifact before you believe the result.

### A passing negative test needs a twin that proves it can fail

This is the general form of the list above, and it earned a name of its own
because the unbuffered-arm work hit it **thirteen times in one feature**. Every
instance was a gate that reported success while guarding nothing. None of them
looked wrong.

The rule: **when a test's job is to prove something is absent, broken, refused
or prevented, a green result is ambiguous.** It means either "the guard held" or
"the guard never ran", and those are indistinguishable from the outside. Pair it
with a twin that establishes the test can fail at all — a mutation, a planted
error, or a positive control asserting the precondition the guard depends on.

The sub-species worth recognising, each from a real instance:

- **The assertion is a tautology.** `mode & K == K` is satisfied by *any* `K`,
  including zero; a counter incremented and then compared against itself proves
  only that the loop ran.
- **The test never constructs the thing it tests.** A test named for refusing
  writes across every configuration enumerated the configurations and never
  built a backend. It passed for the whole time it existed.
- **The test reconstructs the logic instead of calling it.** A replacement test
  for a flag-setting function inlined the same open rather than calling the
  function, so mutating the function's flags to zero left the suite green. This
  one was introduced *by the commit that fixed the previous instance*, while its
  own docstring claimed the opposite.
- **The precondition fires first.** A test for a free-list leak never reached the
  free list, because a capacity check rejected the call earlier for an unrelated
  reason. It asserted the right thing about a path it did not take.
- **The fixture is too weak to exercise the failure.** A reproduction run against
  a file created with `set_len` alone — never written, so never on the device —
  showed both variants surviving. The confound was in the *verification of a
  finding*, not in the code under test.
- **The test is compiled but never executed.** A `#[test]` inside a
  `harness = false` bench target is type-checked and then ignored:
  `cargo test --all-targets` reports success with an unconditionally panicking
  test in the file. Verified by planting both a type error (caught) and a
  panicking test (not caught) in `benches/unbuffered.rs`. Guards belong in the
  library, where they run. Note that `test = true` on such a target is still
  load-bearing — it gets the target compiled and its `main` smoke-run — but it
  does not run `#[test]` functions.
- **A `compile_fail` doctest that no longer names anything.** After a rename it
  fails to compile for the wrong reason and still reports `ok`.
- **The decision is in a place nothing observes.** The `handle-mode` arm's
  central premise — that file opens sit *outside* the region it times — lived as
  an `Opens::Hoisted` literal inside a `harness = false` bench target. Reversing
  it to `Opens::PerIteration` left every library test, every integration test and
  the target's own smoke run green, while folding per-open cost into the
  measured delta at the arm's negative control. The literal was covered by a
  test asserting that `Opens::Hoisted` *behaves* correctly, which proved the
  variant worked and said nothing about which variant was selected. Moving the
  decision into the library was necessary but not sufficient: an
  `assert_eq!(OPENS, Opens::Hoisted)` would only have restated the constant. The
  guard that works counts opens and checks the *property* — non-zero, and
  invariant under a fourfold change in iteration count.
- **The counter that a parallel harness makes unsound.** The fix for the item
  above initially used a process-global counter, copying an existing in-tree
  one. Tests run in parallel threads in a single process, so an unrelated test
  moved the counter between two readings. The positive assertion became a flake;
  the *negative* one — "this path performs zero of these" — became an assertion
  that fails for a reason unrelated to its claim, which is the shape that gets
  "repaired" into a lower bound and thereby deleted. Scope a counter to whatever
  actually performs the work: here the operation runs inline on the calling
  thread, so thread-local was the accurate scope and it permitted an exact zero.
- **A branch no real input can reach.** A `#[should_panic]` twin was written for
  a "could not determine the handle's mode" branch using an anonymous pipe as
  the vehicle. Pipes answer that query. Probing found that *every* obtainable
  handle answers it — file, pipe and socket alike — and only a value that is not
  a handle fails, which cannot be constructed without violating
  `File::from_raw_handle`'s contract in order to test a function whose own
  contract already requires a live handle. Splitting the judgement out from the
  query made the branch reachable directly. When a branch has no legitimate
  input, say so and test it at the level where it does; a twin that reaches it
  by breaking two contracts is not evidence about the branch.
- **A mutation that cannot mutate.** `Rng::new` does `seed | 1`, so mutating a
  seed constant by `SEED ^ 1` is a no-op and the "surviving mutation" says
  nothing. Check that the mutation changes behaviour before concluding anything
  from a test that survives it.

### A threshold something meets exactly is not a threshold

Named after two instances in one feature, one of them caught in the author's own
proposal and one in a document the reviewer had already approved.

A budget option was rejected for passing its own affordability check with **zero
margin** — a floor of exactly the permitted maximum. The replacement proposal
was then written with the same defect and caught only on re-derivation. The rule
adopted was that any such proposal must state its margin as a number; the chosen
budget carries 104 s of margin on a 216 s requirement, and says so.

The generalisation is the useful part, and it is not about budgets. **A
constraint that a proposal satisfies with no room is indistinguishable from a
constraint that was fitted to the proposal.** It provides no evidence, because
it could not have rejected anything. The same reasoning applies to a tolerance
chosen to admit a measurement, an outlier bound chosen to exclude a known
excursion, and a deadline met to the day.

The second instance is the more instructive one, because it did not look like a
number at all.

### An ordering that makes a blind analysis impossible

The `handle-mode` plan scheduled: (4) measure the within-run noise band,
(6) freeze the interpretation criterion *before* any numbers exist, (7) run the
experiment. Phase 6 stated its own justification — *"a threshold chosen after
seeing the data is not a threshold; this phase exists so that no such choice is
possible."*

The ordering could not deliver it. **The band is measured by repeating the whole
benchmark target, and that target contains both arms of the experiment.**
Measuring the instrument's spread necessarily measured the effect. There was no
order in which the freeze could have been blind, so the criterion was in fact
chosen with the estimates already visible.

What makes this worth a section is who missed it. The author wrote the ordering;
the reviewer read and approved it. In the same week, the author had rejected one
option for the zero-margin defect above and then nearly shipped a second with
it, and the reviewer had written that *"a threshold that a proposal meets
exactly is not a threshold, and that generalises beyond budgets."* Both parties
were attentive to this exact species, in these words, and both walked past an
instance of it sitting in the plan's phase ordering.

The lesson is not "check the ordering". It is that **attentiveness to a named
pattern does not transfer to an instance wearing different clothes** — here a
scheduling constraint rather than a numeric margin. If a document says a later
step will be performed without knowledge an earlier step produces, check that
the earlier step does not produce it.

The recovery is the standard one and is worth knowing: relabel the first set a
**pilot**, publish it as such, freeze the analysis in full — cells, criterion,
predicted effect sizes, outlier rule, run count — and collect a **confirmatory**
set against it. Freezing the threshold alone is not enough; every degree of
freedom that could otherwise be exercised after the fact has to be spent in
advance, and the outlier rule is the one that matters most once an excursion has
already been seen.

### Confounds that produce a believable number, not an obvious failure

The `mtime` trap above is one species of a broader hazard, and the unbuffered
work found another that behaves identically: **a confound whose output is a
plausible measurement rather than a crash.** Those are the dangerous ones,
because nothing prompts you to look.

- **Buffered access poisons a file for unbuffered I/O.** A single buffered read
  of a file collapses subsequent `FILE_FLAG_NO_BUFFERING` reads of that same
  file from roughly 11 µs to roughly 126 µs per I/O, for the life of the
  process. The benchmark's warm-cache arm reads its data file buffered before
  every run, so an unbuffered arm that shared that file would have measured
  about 115 µs/IO and concluded the ring gains nothing from bypassing the page
  cache. That null result would have been *believed*: it agrees with the two
  preceding features, both of which refuted their own premise. The arm therefore
  keeps its own directory and its own data file, and `UnbufferedPath` enforces
  the separation at the type level rather than by convention.

- **The guard's own test suite reproduced the confound in-tree.** While the
  poisoning guard was being written, two of its tests raced on a single file and
  one of them performed a buffered open on a file another was measuring
  unbuffered — which is precisely the failure the guard exists to prevent,
  committed to the repository by the guard's own tests.

- **A confound that also disarms the control that would have caught it.** The
  worst shape found so far. The `handle-mode` arm compares two handle modes and
  uses depth 1 as a negative control, where the prediction is no effect. Had the
  arm inherited the main matrix's boundary and opened files *inside* the timed
  region, per-open cost would have entered the measured delta — and an open is
  one of the places `FILE_FLAG_OVERLAPPED` itself costs something. So a
  difference would have appeared at depth 1, where the arm's own guard reads a
  difference as **run-level drift** rather than as a defect. The confound would
  not merely have added noise; it would have converted the one arm capable of
  saying "no effect here" into an arm showing an effect for the wrong reason,
  with the safeguard silently disarmed and the result publishing cleanly.

  Two consequences worth carrying forward. First, when adding an arm, ask not
  only "does this confound the measurement" but "does this confound the thing
  that would have caught it". Second, when a new arm's timing boundary differs
  from an existing one's, record **why** it differs and not merely that it does —
  the next person to add an arm faces the same choice with the same two
  plausible answers.

- **A second measurement inside the timed loop, wearing a doc comment that
  denied it.** In the same arm, `std::fs::metadata` was being called per timed
  iteration to size the file. On Windows that opens the path — so the arm whose
  whole premise was "opens are outside the timed region" was performing one per
  iteration, under a comment asserting the opposite. Dilution biases such an arm
  toward a null, which is the direction this project is documented as
  under-scrutinising.

### A standing bias hazard

Not an incident. Worth keeping in front of anyone adding to `docs/performance.md`.

This project has twice had a feature refute the premise that commissioned it,
and both refutations were published as the headline. That is the right
behaviour and it is the reason the numbers here are trustworthy. But it has a
side effect: **the project is now primed to accept unflattering results with
less scrutiny than flattering ones.** A result that says "the crate does not
win" matches the established pattern, reads as intellectual honesty, and invites
no further checking — which makes a *false* negative the cheapest error to
publish and the least likely to be caught.

The buffered-poisoning confound is exactly that shape: it would have produced a
null result, in a project that expects null results, from a measurement error.
Scrutinise a finding because it is surprising *or* because it is expected, not
only because it is convenient.


### Proving a wakeup cannot be lost

The wakeup guarantee is the easiest thing in this crate to get subtly wrong and
the hardest to catch, because a lost wakeup is a race that a passing test cannot
distinguish from a won one. Two techniques carry most of the weight:

- **Two wakers, and assert the old one was *not* used.** Poll the driver to a
  park under waker A, re-poll under waker B, then raise the signal. Asserting
  only that B fired lets a driver that wakes *both* pass; asserting that A's
  count did not move is what proves the waker was replaced rather than
  accumulated.
- **Mutate, do not inspect.** Every test in this group was run against a
  deliberately broken variant — `nudge()` returning `None`, `take_nudge()`
  never reporting, `Drop for Park` not clearing its waker — and required to fail.
  The counts above (two caught, then four) are the reason that is not optional.

### What cannot be tested here

Say so in the test module rather than writing something that passes vacuously:

- **Aborts.** They end the process. Enumerated and reviewed instead; see
  [pending-work.md](pending-work.md).
- **A completion racing its own cancellation.** No deterministic ordering has
  been found.

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
