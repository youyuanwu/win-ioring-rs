# Performance

What this crate costs against the alternative, measured rather than assumed.

```powershell
cargo bench -p win-ioring-bench
```

produces the figures below. Working files live under `target/bench-data`; remove
them by deleting that directory. There is no `--clean` flag and no separate
binary — the benchmark is a Criterion target and `cargo bench` is the only way to
run it.

## The headline

**`tokio::fs` is still faster than this crate on most of what is measured here,
but not at one operation in flight.** At depth 1 this crate is ahead in all three
scenarios — random reads at 0.55x, write-then-read at 0.86x, sequential reads at
0.81x. At eight and sixty-four operations in flight it loses everywhere, by 1.10x
to 1.75x with owned buffers and by 1.10x to 1.76x with registered ones.

Two warnings attach to that paragraph, and they are not decoration.

**The sequential-read result at depth 1 is a change of *ranking* against what
this document used to publish, and it is not claimed as an improvement in this
crate.** The previous revision reported that cell at 1.90x — a loss. Nothing in
the crate changed between the two. What changed is the instrument, and the old
number for that one cell is independently known to be unreliable: re-running the
*unchanged* old harness twice, minutes apart, produced figures 35% and 75% higher
than the ones it published. See "What changed when the instrument changed".

**Absolute figures from this document and from any revision before it are not
comparable.** The measurement framework changed and, with it, how often the same
bytes are read. Everything below is per I/O and measured by Criterion; nothing
above 1.0x or below it should be read against a number from the old harness.

## What is actually being measured

Every figure is **per-operation software overhead against a warm page cache**.
The working file is read through once before measuring, and the working set is
checked against physical memory so the premise can be reported as holding or not.

These are **not** device throughput numbers and must not be quoted as such. The
backends differ in syscall counts, thread-pool hops and submission batching;
device I/O would swamp all of that in noise, which is why it is deliberately kept
out.

## How it is arranged

The obvious way to compare two I/O layers is to write two benchmarks, and the
obvious problem is that they are never quite doing the same work — a difference
in buffer reuse, operation ordering, or how many operations are outstanding moves
the numbers more than the I/O layer does.

So there is **one** piece of application logic, written against a trait, executed
unmodified against each backend. Each run records:

- every operation it issued, in **issue** order, compared exactly; and
- a digest of what each operation delivered into application-visible memory,
  folded commutatively so **completion** order — legitimately nondeterministic
  above one operation in flight — cannot enter it.

A run whose trace disagrees with the others is **rejected, not reported**.

Two tests in `crates/win-ioring-bench/tests/fairness.rs` hold that to account.
Each takes a **real** backend, wraps it so that it either skips one read in four
or reports full transfers whose bytes never reach anywhere readable, and drives
it through `harness::measure_combination` — the same function a measured
benchmark calls, applying the weakening at the same call site the timed closure
uses. Both require the run to be **rejected**, and both assert *which* mismatch
was reported, so a run falling over for an unrelated reason does not count as a
pass. A third test proves the weakenings change what was delivered rather than
what was issued, so the two above cannot be passing on the issue-trace comparison
alone.

**That paragraph used to say something stronger than was true.** Until this
document was rewritten it read "Two tests deliberately weaken a backend … and
both must fail the run", and the tests it described did no such thing: they built
traces by hand and compared them, exercising the comparator but never a backend
and never the measurement path. A measurement that had stopped consulting the
comparator entirely would have left both of them passing. The claim is now true
of the code, and the overstatement is recorded here rather than quietly deleted,
because a document that corrects itself in silence teaches nobody what to be
suspicious of.

That second weakening is not hypothetical. Before the registered-buffer redesign,
a registered read resolved to a transfer count with no way to read the buffer it
filled — so the registered path would have benchmarked well precisely because it
delivered less than everything it was being compared against. The comparison
could not be built honestly until that was fixed.

## The backends

| Backend | What it is |
|---|---|
| `tokio::fs (blocking pool 1)` | `spawn_blocking` + `seek_read`/`seek_write`, one blocking thread |
| `tokio::fs (blocking pool 512)` | the same, at the default pool width |
| `win-ioring (owned buffers)` | this crate, caller-owned buffers, unregistered handles |
| `win-ioring (registered)` | this crate, **registered buffers**, operations naming a buffer by index |

Three notes on fairness:

**The thread-pool backend uses `spawn_blocking`, not `tokio::fs::File`.** That
type is cursor-based and cannot express several *positional* operations
outstanding at once, which is exactly what this varies. Underneath, it does what
this does — hands the work to the blocking pool — so the mechanism is the same.

**What varies between the two thread-pool configurations is pool width, not
runtime flavour.** All the I/O lands on the blocking pool regardless of flavour,
and the application work is trivial, so two flavours would measure one thing
under two labels.

**The registered backend registers buffers only.** The account prints its
configuration as "registered buffers and file handle", which overstates: the code
never registers a file, so every backend in the comparison passes an owned
handle. Harmless to the measurement, since that makes all four alike, but the
printed string is wrong; it is tracked in [pending-work.md](pending-work.md).

## Reading the numbers

Each figure is a **per-I/O cost with a confidence interval**: Criterion's
`[lower, estimate, upper]` for one iteration, divided by the number of I/Os that
iteration issued. The divisor is not arithmetic — it is `trace.operations()` from
the run's own issue trace, printed per combination in the fairness account, so it
cannot drift from what was actually issued. Write-then-read issues two I/Os per
nominal operation and the divisor already accounts for it.

Three things follow that the old five-repeat table could not offer:

- **The interval is computed, not observed.** It is a bootstrap confidence
  interval over 100 samples at Criterion's default 95% confidence, not a min and
  a max. Two intervals that do not overlap are a real difference *on this host,
  during this run*; two that do overlap are not a difference at all.
- **Outliers are classified rather than hidden.** A run prints, per benchmark,
  how many of its 100 measurements were mild or severe outliers on either side. A
  benchmark with 9% severe high outliers was measured on a machine doing
  something else, and says so.
- **A stored baseline turns a comparison of two absolute numbers into a reported
  verdict.** See "Reproduction and regression tracking".

Standing caveat, unchanged in force: **a confidence interval describes
repeatability on one host, not portability.** It is narrow because 100 samples
were gathered over two seconds on one machine. It says nothing whatever about
another machine, another Windows build, or another volume.

Achieved depth is measured by this crate, because one backend's outstanding count
includes work its kernel has not accepted and the other exposes nothing
comparable. It therefore **cannot see a backend serialising operations below its
own interface** — read it beside that backend's configuration, not alone. It is
reported in the fairness account rather than here, because Criterion has nowhere
to put it.

**Achieved depth used to be decoration.** A shortfall against the configured
depth was annotated in the account and failed nothing, at any depth or shape, so
a run that never reached the concurrency it claimed would publish figures without
complaint. It now carries a check: each scenario declares its shape, the expected
mean depth follows in closed form from the shape, the operation count and the
depth, and a run whose measured mean disagrees is a **failure**, not a note. The
shortfall annotation remains, and remains only an annotation — it is the shape
check that bites.

## Full result

All thirty-six cells of the rolling matrix — every rolling scenario, depth and
backend — because a selection invites the question of how it was selected.

The bulk-read scenario was added after this run and is not in this table.
Republishing forty cells would have meant re-running everything, and a table
stitched from two runs is worse than two tables. It has its own section,
[below](#the-batched-window-and-what-it-settled), reported against the rolling
depth-64 cells **from the same run as itself** so the shape comparison is
internal to one run rather than across the ±24% floor.

Taken on an AMD Ryzen 7 PRO 6850U (8 cores, 16 logical processors, 16 MiB L3),
28436 MiB of memory, working files on an NVMe volume. `warm_up_time` 1 s,
`measurement_time` 2 s, 100 samples per estimate.

Operations per iteration: **256** for sequential read (256 I/Os), **512** for
random read (512 I/Os), **128** for write-then-read (**256** I/Os — it writes
each block and reads it back). `relative` is against
`tokio::fs (blocking pool 1)` within the same scenario and depth. Bold marks
where this crate is ahead.

| scenario | depth | backend | µs per I/O [lower, estimate, upper] | relative |
| --- | --- | --- | --- | --- |
| sequential read (64 KiB) | 1 | tokio::fs (pool 1) | [105.62, 107.58, 109.54] | 1.00x |
| sequential read (64 KiB) | 1 | tokio::fs (pool 512) | [111.87, 114.57, 117.34] | 1.06x |
| sequential read (64 KiB) | 1 | win-ioring (owned) | **[85.46, 87.47, 89.70]** | 0.81x |
| sequential read (64 KiB) | 1 | win-ioring (registered) | **[82.95, 85.31, 87.94]** | 0.79x |
| sequential read (64 KiB) | 8 | tokio::fs (pool 1) | [72.81, 74.70, 76.71] | 1.00x |
| sequential read (64 KiB) | 8 | tokio::fs (pool 512) | [69.98, 71.28, 72.64] | 0.95x |
| sequential read (64 KiB) | 8 | win-ioring (owned) | [81.09, 82.42, 83.86] | 1.10x |
| sequential read (64 KiB) | 8 | win-ioring (registered) | [84.91, 86.72, 88.64] | 1.16x |
| sequential read (64 KiB) | 64 | tokio::fs (pool 1) | [67.84, 69.09, 70.41] | 1.00x |
| sequential read (64 KiB) | 64 | tokio::fs (pool 512) | [71.82, 73.10, 74.46] | 1.06x |
| sequential read (64 KiB) | 64 | win-ioring (owned) | [82.69, 84.59, 86.68] | 1.22x |
| sequential read (64 KiB) | 64 | win-ioring (registered) | [84.49, 86.10, 87.82] | 1.25x |
| random read (4 KiB) | 1 | tokio::fs (pool 1) | [24.23, 24.88, 25.59] | 1.00x |
| random read (4 KiB) | 1 | tokio::fs (pool 512) | [24.15, 24.73, 25.33] | 0.99x |
| random read (4 KiB) | 1 | win-ioring (owned) | **[13.25, 13.59, 13.96]** | 0.55x |
| random read (4 KiB) | 1 | win-ioring (registered) | **[14.03, 14.39, 14.78]** | 0.58x |
| random read (4 KiB) | 8 | tokio::fs (pool 1) | [6.92, 7.15, 7.39] | 1.00x |
| random read (4 KiB) | 8 | tokio::fs (pool 512) | [11.87, 12.28, 12.74] | 1.72x |
| random read (4 KiB) | 8 | win-ioring (owned) | [10.18, 10.75, 11.42] | 1.50x |
| random read (4 KiB) | 8 | win-ioring (registered) | [11.24, 11.69, 12.18] | 1.64x |
| random read (4 KiB) | 64 | tokio::fs (pool 1) | [5.65, 5.78, 5.92] | 1.00x |
| random read (4 KiB) | 64 | tokio::fs (pool 512) | [12.97, 13.38, 13.84] | 2.32x |
| random read (4 KiB) | 64 | win-ioring (owned) | [9.77, 10.11, 10.47] | 1.75x |
| random read (4 KiB) | 64 | win-ioring (registered) | [9.95, 10.17, 10.42] | 1.76x |
| write then read (64 KiB) | 1 | tokio::fs (pool 1) | [186.62, 192.39, 200.67] | 1.00x |
| write then read (64 KiB) | 1 | tokio::fs (pool 512) | [193.34, 196.84, 200.86] | 1.02x |
| write then read (64 KiB) | 1 | win-ioring (owned) | **[163.04, 165.57, 168.37]** | 0.86x |
| write then read (64 KiB) | 1 | win-ioring (registered) | **[158.40, 162.85, 167.66]** | 0.85x |
| write then read (64 KiB) | 8 | tokio::fs (pool 1) | [149.90, 152.09, 154.43] | 1.00x |
| write then read (64 KiB) | 8 | tokio::fs (pool 512) | [153.46, 155.70, 158.08] | 1.02x |
| write then read (64 KiB) | 8 | win-ioring (owned) | [170.24, 175.98, 185.02] | 1.16x |
| write then read (64 KiB) | 8 | win-ioring (registered) | [164.33, 166.88, 169.55] | 1.10x |
| write then read (64 KiB) | 64 | tokio::fs (pool 1) | [149.21, 152.77, 157.54] | 1.00x |
| write then read (64 KiB) | 64 | tokio::fs (pool 512) | [154.32, 159.17, 166.44] | 1.04x |
| write then read (64 KiB) | 64 | win-ioring (owned) | [167.17, 170.98, 175.62] | 1.12x |
| write then read (64 KiB) | 64 | win-ioring (registered) | [172.29, 175.71, 179.30] | 1.15x |

**Do not compare µs per I/O across scenarios.** A 64 KiB read moves sixteen times
the bytes of a 4 KiB one, and write-then-read pays a file-system write path the
read scenarios never touch. Compare down a group of four rows, not across them.

**What a second full run reproduced, and what it did not.** The same binary was
run again minutes later against this run as a stored baseline — the same binary
in the checkable sense: `sequential-read/tokio-pool-1/1` came back at 27.521 ms
against the 27.540 ms above, a reported change of −0.07%. Recomputing every
relative from that run's own absolutes:

- **The direction of each ring backend against `tokio::fs (pool 1)` reproduced in
  all eighteen ring cells.** Ahead at depth 1 in all three scenarios, behind at
  depths 8 and 64 in all three. That is the comparison this document is about, and
  it is the one that held.
- **The magnitudes moved, by up to 0.36 in the relative.** Random read at depth 64
  read 2.32x for `tokio-pool-512` against 1.96x, and 1.75x for owned buffers
  against 1.40x. A relative from this table is good to its direction and its rough
  size, not to a decimal — and not, on this evidence, even to one significant
  figure, since 1.75x and 1.40x do not share one.
- **The `tokio-pool-1`-against-`tokio-pool-512` comparison did not reproduce.** It
  reversed which of the two was faster in four of its nine cells: sequential read
  at depths 1 and 8, and write-then-read at depths 1 and 8, all of them cells
  where the two are within a few percent of each other. Read those cells as "no
  difference measured", not as a ranking.
- **The two ring backends' order against each other flipped in two cells** —
  random read at depth 1 and write-then-read at depth 8 — which is the same fact
  the "registration is behind in six of nine and ahead in three" reading below
  rests on, and the reason that reading is given as "a few percent either way"
  rather than as a winner.

**This paragraph used to claim that every relative figure was reproduced**, with
"the largest disagreement being random read at depth 64 with owned buffers (1.75x
against 1.40x)", and advised treating one significant figure as solid. Four cells
reverse direction between the two runs, and 1.75x against 1.40x is itself a
disagreement in the first significant figure, so both halves were wrong. The
1.75x that the headline and `README.md` quote as the top of the loss range is a
figure from one run; the second put that cell at 1.40x.

## The batched window, and what it settled

Everything in the table above is a **rolling window**: fill to the configured
depth, await one completion, refill one. That shape was suspected of never
engaging this crate's central mechanism — the single `SubmitIoRing` that covers
every entry built since the last one. A **batched window** was added to settle
it: issue N reads with no await between them, await all N, repeat, so every
operation in a batch is built before the driver's next pass.

The suspicion was wrong, and finding that out was worth more than confirming it
would have been.

### Batching was already at full depth

`Handle::submission_counts` reports how many submissions a run made and how many
entries they covered. Across the whole default matrix, entries per submission:

| scenario | depth 1 | depth 8 | depth 64 |
|---|---|---|---|
| sequential read (256 ops) | 1.0 | 8.0 | 64.0 |
| random read (512 ops) | 1.0 | 8.0 | 64.0 |
| write then read (257 entries) | 1.0 | 7.8 | 51.4 |
| bulk read (256 ops) | — | — | 64.0 |

The rolling window batches at **exactly the configured depth** — 256 entries over
4 submissions at depth 64. Where a figure is not the depth it is arithmetic:
write-then-read's 257 entries are four submissions of 64 and one of 1. Both ring
backends agree to the digit in all **twenty** ring cells — the other twenty rows
of the matrix are `tokio::fs`, which has no ring and reports the figure as not
applicable rather than as zero — and **three full runs produced bit-identical
figures**, so this is not an average over noise.

The mechanism: the executor drains every ready completion in one pass before the
driver submits again, so against a warm cache a rolling refill rebuilds the whole
window before the next `submit_pending`. **That is a property of the measurement
conditions, not of the shapes.** Every figure in this document is warm-cache by
design. A cold cache would stagger completions, and whether the two shapes still
coincide there is unmeasured.

### The two shapes cost this crate the same, and `tokio::fs` more

Sequential read and bulk read do identical work — 256 reads of 64 KiB from the
same file at depth 64 — and differ only in shape. Both batch 64.0 entries per
submission. They differ in sustained depth: the rolling window holds a mean of
56.1 operations outstanding, the batched one 32.5, because it drains to zero at
every batch boundary.

From one run, µs per I/O:

| shape | backend | [lower, estimate, upper] | relative |
| --- | --- | --- | --- |
| rolling | tokio::fs (pool 1) | [61.66, 61.98, 62.33] | 1.00x |
| rolling | tokio::fs (pool 512) | [65.28, 66.02, 66.83] | 1.07x |
| rolling | win-ioring (owned) | [77.64, 78.35, 79.10] | 1.26x |
| rolling | win-ioring (registered) | [78.60, 79.35, 80.15] | 1.28x |
| batched | tokio::fs (pool 1) | [66.53, 67.36, 68.23] | 1.00x |
| batched | tokio::fs (pool 512) | [67.10, 67.66, 68.23] | 1.00x |
| batched | win-ioring (owned) | [78.28, 79.33, 80.44] | 1.18x |
| batched | win-ioring (registered) | [78.95, 79.81, 80.72] | 1.18x |

Read by interval overlap within the run:

- **This crate is indistinguishable between the two shapes.** Owned buffers
  [77.64, 79.10] against [78.28, 80.44]; registered [78.60, 80.15] against
  [78.95, 80.72]. Both pairs overlap. Draining the ring to zero at every batch
  tail cost it nothing measurable, and neither did filling it.
- **`tokio::fs` is slower in the batched shape.** [61.66, 62.33] against
  [66.53, 68.23] — disjoint, and disjoint again in a second run ([66.34, 67.78]
  against [70.90, 72.94]). Handed 64 tasks at once rather than a steady trickle,
  the one-thread pool pays about 8% more per I/O.
- **So the gap narrows and does not close**: 1.26x to 1.18x. That is the whole of
  the batching advantage visible here, and it does not reach parity.

Across three runs bulk read at depth 64 ran **1.15x to 1.28x** against
`tokio::fs (pool 1)`, inside the 0.92x-1.61x band the rolling sequential read
occupied in the same three runs.

### What that leaves unexplained

This crate's central claimed advantage was **already fully in effect in every
figure ever published here**, at exactly the configured depth, and the crate
still loses as depth rises. So the loss is not a batching shortfall, and this
document previously implied otherwise.

**Why it loses is currently unknown.** Nothing measured here establishes a cause,
and the honest position is to leave the gap open rather than fill it with the
next plausible guess — single-threaded completion processing against a
512-thread pool, per-completion dequeue cost, cache effects. Each is testable and
none is tested.


The migration to Criterion shrank what one iteration does — the read scenarios
from a whole 256 MiB sweep to 16 MiB, write-then-read from 64 MiB to 8 MiB — and
the specification required the new per-I/O intervals to be checked against the
old harness's. They do not agree. **Thirty-five of the thirty-six cells are
disjoint**, and thirty-four of those are *faster* per I/O, by 0.39x to 0.94x,
uniformly across all four backends.

The suspect was the reduced operation count, so it was tested rather than
assumed: random read was re-measured at 1024 operations per iteration, the old
harness's own count, touching exactly the bytes the old harness touched. That
moved per-I/O cost by **1.01x to 1.25x**, against a gap of 0.47x to 0.64x. The
operation count is not the cause and the specification's assumption — that
shrinking it does not change per-operation economics — survives.

The most likely cause is **repetition**, and it is a hypothesis, not a finding.
The old harness ran each scenario six times: one discarded warm-up and five
repeats. Criterion runs it as many times as a hundred samples take, which on this
matrix is 131 to 1311 iterations of the *same deterministic offset sequence*. The
bytes an iteration touches are therefore revisited one to two orders of magnitude
more often. What that revisiting buys was **not** measured: no cache counter was
read, and no configuration was run that varies data residency while holding
repetition fixed. Two of this document's own numbers sit badly with a
cache-residency story in particular — the old harness's random read already
touched only 4 MiB per repeat, comfortably inside this host's 16 MiB L3, yet
random read shows the *largest* shift (0.39x–0.60x); and sequential read's
touched set shrank sixteenfold, from far outside L3 to its boundary, yet moved by
the same factor (0.50x–0.57x). Sustained load is at least as plausible a
contributor: a three-second continuously loaded sampling window versus six short
bursts differs in boost residency, core parking and thread-pool settling, which
would also explain why `tokio-pool-1`, the most thread-hop-bound backend, moved
as much as the ring ones. Write-then-read moved least (0.75x–0.94x), and its
write half is the part no read-side effect of any kind accelerates.

**The conclusion does not rest on the mechanism.** What rules out a regression in
this crate is that all four backends moved by roughly the same factor —
including the two `tokio::fs` backends, which nothing in this repository can make
faster — and that the *relative* column is stable across the change. For owned
buffers against `tokio::fs (pool 1)`, in the eight cells whose pre-change side is
trustworthy: seq d8 1.18→1.10, seq d64 1.17→1.22, rand d1 0.63→0.55, rand d8
1.89→1.50, rand d64 1.65→1.75, wtr d1 0.93→0.86, wtr d8 1.19→1.16, wtr d64
1.17→1.12. A regression in this crate would move the ring backends *relative to*
the pool backends. Nothing does, except the one cell already recorded below as
inconclusive — which is excluded from that list precisely because its pre-change
`tokio-pool-1` figure is the untrustworthy one.

**This section used to assert the mechanism as established** — "The cause is
**repetition** … a 2 MiB or 16 MiB touched set revisited that often is resident in
cache … The effect tracks that explanation across scenarios". The diagnostic that
was run refuted the *rival* explanation, the operation count; it established
nothing about residency. The correction is recorded here rather than made
silently, because the strong argument was buried under the weak one.

**One cell is unresolved and is recorded as such**: sequential read at depth 1
with the narrow blocking pool reads as 1.20x *slower*, the only cell in the wrong
direction. Its pre-change figure is the one the migration's own Phase 2 check
caught disagreeing with two later runs of the very code it describes — 83.8–95.1
µs/IO published, against 128.6–166.6 µs/IO measured twice afterwards from an
unchanged binary. Measured against those instead, the new figure is faster, in
line with its three siblings. No mechanism has been proposed by which this change
could slow one backend at one depth in one scenario while leaving the same
backend at 0.53x–0.57x at the other two depths. The pre-change side has no
trustworthy value for this cell, so there is no verdict — and this is the cell
whose *ranking* the headline declines to claim as an improvement.

## Reproduction and regression tracking

```powershell
cargo bench -p win-ioring-bench
cargo bench -p win-ioring-bench -- --save-baseline pre
cargo bench -p win-ioring-bench -- --baseline pre
cargo bench -p win-ioring-bench -- random-read
```

The first stores its results as the default baseline and compares against
whatever was there before. The second names a baseline; the third compares
against that name and prints, per benchmark, a change interval and a verdict.
Baselines live under `target/criterion/<group>/<backend>/<depth>/<name>`, survive
rebuilds, and are lost with the `target` directory. A filtered run times only the
benchmarks matching the filter but still prepares, warms and verifies all ten
combinations, so the fairness check never narrows because somebody typed a
filter; the combinations it did not time are marked
**verified but not timed** in the account.

**Read the verdicts against this host's noise floor, not against zero.** Running
the suite twice with no change at all between the runs produced a "significant"
verdict for **17 of 36** benchmarks, in both directions. Two quantities, because
they differ and the difference matters: the **point estimates** ran from −15.2%
to +13.7%, while the **change intervals** those estimates sit in reached from
−17.6% (sequential read, depth 8, `tokio-pool-1`) to +23.7% (write then read,
depth 8, `ioring-registered`), with a mean interval width of 8 points.
Criterion's default noise threshold is 1%, so on a machine that drifts this far
over three minutes it will report changes that are real drift and not real
regressions. The threshold is deliberately not raised: a statistic tuned until
the answer is comfortable is not a measurement. What the number is good for is a
bound — on this host, a reported change whose interval lies inside roughly
−18% to +24% is indistinguishable from an unchanged tree, and one of 40% is not.

**That bound used to be published as "±17%"**, which is neither of the two
quantities above: the point estimates never reached 17% and the intervals went
past 24%. The nearest 17 in the data is `parse.py`'s "widest change-interval
**width**: 17.43 points", a different quantity from a bound on a change, and the
two look to have been conflated. `docs/pending-work.md` leaned on the ±17%
figure as an envelope and is corrected with it.

## The fairness account

Criterion has no place for facts that are not timings, so everything else the
comparison knows is written to a sidecar: line by line to **stderr** as the run
proceeds, and in full to `target/bench-data/fairness.md` at the end. It carries

- the configuration, the per-scenario shapes and the time budget actually used;
- host processor count, physical memory, and the volume the working files are on;
- the warm-cache premise, stated against both the **resident** working set that
  must stay cached (264 MiB) and the bytes each scenario **touches** per iteration
  (16 MiB, 2 MiB, 16 MiB), which are no longer the same number;
- every backend's name, configuration and availability, with a reason for each
  one that was unavailable and each one that prepared and then failed;
- the run order actually used;
- per combination, the I/Os per iteration, the iteration count, the achieved
  concurrency, and whether it was **timed** or **verified but not timed**;
- the reference backend and the agreement verdict per (scenario, depth);
- the number of drivers built against the number of **ring** combinations
  measured — 18 of the 36, because the two thread-pool backends build none;
- the write file's size after the run, and whether the first and last measured
  iteration issued and delivered the same work;
- preparation and measurement wall clock.

Achieved depth and the warm-cache premise live there for that reason and not
because they are unimportant. The standing caveat applies wherever they are read:
achieved depth is measured above the backend's own interface and cannot see a
backend serialising below it.

## Run order

Backends are measured in a **rotated** order: the four are rotated left by the
combination index, so each takes a turn going first. That is a decision, retained
deliberately, not an inherited habit.

Criterion's per-benchmark warm-up absorbs settling *within* a benchmark. It does
nothing about drift *across* a three-minute run, which is what rotation is for:
without it, whichever backend is measured first is systematically measured on a
cooler machine, in every group and in every run — a bias that repeats rather than
averages out. It costs one line, it is deterministic so two runs compared through
`--baseline` saw the same order, and the figures this document compares against
were taken with it.

The cost, recorded: a filtered run visits a different order than a full one, so
figures from a filtered run are not strictly comparable with unfiltered ones. The
account prints the order it used.

## The time budget, and why it is not Criterion's default

`warm_up_time` is 1 second, `measurement_time` is 2, and `sample_size` is 100 —
Criterion's own default. Every *statistical* parameter is left alone: confidence
level, resampling count and noise threshold are all Criterion's.

The two that moved had to. A benchmark costs at least its warm-up plus its
measurement window no matter how small an iteration is, and at Criterion's
defaults of 3 s and 5 s, forty benchmarks are 320 seconds of floor against a
five-minute budget — over it before a single I/O is issued. Reducing what an
iteration does cannot get below that floor; only the budget can.

**The budget is a number, not an aspiration.** It used to exist only in this
paragraph, which is no use as a constraint when somebody proposes a new scenario.
`RUN_BUDGET` is 300 seconds and `Budget::CHOSEN` holds the two timing values, and
a test computes the matrix's floor from the real depth lists and checks it. The
check is against **half** the budget, not all of it: three recorded runs cost
1.79x, 1.83x and 2.06x their floor once preparation, untimed warm-ups, analysis
and window overruns are counted, so a matrix whose floor merely fits the budget
will overrun it in practice. At forty benchmarks the floor is 120 seconds against
a 150-second limit, which is room for about two more combinations. Past that,
something has to be traded away — which is how bulk read came to run at depth 64
alone.

`measurement_time` is a floor, not a cap, and this is worth knowing before
reading an interval. A benchmark whose hundred samples fit inside the window is
padded back up to fill it with extra iterations per sample. A benchmark whose
hundred samples do not fit overruns the window, prints "Unable to complete 100
samples", and takes as long as a hundred samples take. Between half and two
thirds of this matrix is in the second regime on any given run, and the warning
is expected, not a defect. The sample count is 100 either way, which is what the
intervals rest on.

A reader comparing intervals should know that each was gathered over two seconds.
Estimates gathered over longer would be tighter, and the whole matrix would not
then fit the budget.

## What a full run costs

Roughly **190 to 255 seconds** on the host above, with the working files already
present and warm; the first run on a fresh checkout also creates a 256 MiB file.
Five timed runs of the thirty-six-benchmark matrix came in at 190, 210, 213, 225
and 254 seconds; three of the forty-benchmark matrix at 247, 219 and 215. The
spread is the machine, not the suite: the same binary measuring the
same work varies by a minute depending on what else the host is doing, which is
the same fact the noise-floor finding above reports in a different unit. Budget
for the slow end, not the fast one. The account prints preparation and
measurement separately.

It writes about **12 GiB** to `target/bench-data/write.dat` — 8 MiB per
iteration, across roughly 1570 write-bearing iterations. The file itself stays
8 MiB, because each iteration truncates and rewrites it; the wear is in the
writing, not in the size. That is the price of statistics on a write-bearing
scenario, and it is stated here so nobody discovers it from a wear indicator.

## What to take from it

- **At depth 1 the crate is ahead in all three scenarios.** This is where the
  wake-path work landed: with one operation in flight there is nothing to
  amortise the cost of parking over, so the driver paid it in full on every
  operation.
- **Concurrency still does not favour this crate on this workload, and the
  reason is not the one this document used to give.** The advantage
  completion-based I/O is supposed to earn — coalescing outstanding operations
  into one submission — is *not* missing: it was measured, and it runs at exactly
  the configured depth in every cell above. The crate loses anyway. What accounts
  for that is currently unknown; see [the batched
  window](#the-batched-window-and-what-it-settled).
- **A narrow blocking pool beats a wide one for small reads.** `tokio::fs` at one
  blocking thread is more than twice as fast as at 512 on random reads at depth
  64. That is contention, not I/O.
- **Registration still does not pay for itself here.** It is behind the
  owned-buffer path in six of the nine (scenario, depth) cells and ahead in
  three, by a few percent either way — where it used to be consistently behind.
  Its own cost is excluded from these figures, so this is the per-operation
  comparison alone.
- **Sequential reads remain the worst case above depth 1**, at 1.10x to 1.25x.
  64 KiB transfers are dominated by data movement rather than per-operation
  overhead, so there was less for this work to remove.

## Where the improvement came from

The driver used to wait on two operating-system events at once and arm a
thread-pool wait for both on every pass, tearing the loser down with a blocking
`UnregisterWaitEx`. One of those two events carried nothing but "the application
queued work for you" — a signal raised only ever from the driver's own thread,
routed through a kernel object and an OS thread pool to travel no distance.

Measured with temporary scaffolding, per-operation park overhead above a
synchronous ring floor was **13.99 µs at one operation in flight**, against a
ring that cost 10.69 µs to drive synchronously — so the machinery cost more than
the I/O. After the rewrite it is **2.46 µs**, an 82% reduction. At eight and
sixty-four operations in flight, where a single park already served many
completions, per-operation cost fell slightly as well.

**Those decomposition figures are not reproducible from this repository.** They
were produced by a probe that was deleted once it had done its job, on the
grounds that a benchmark nobody runs is a benchmark nobody maintains. The figures
in the table above *are* reproducible — they come from the committed benchmark.
The raw probe output, with every repeat and its provenance, is preserved outside
the repository in the workflow artifacts for that change.

What a stored baseline now closes, and what it does not, is stated in
[pending-work.md](pending-work.md): a regression in the park path large enough to
move the depth-1 figures would be reported, but nothing here can attribute it to
the park path, and nothing runs this in CI.

### A correction worth recording

An earlier revision of this document reported sequential read at depth 1 as
1.03x. Re-running the *unchanged* crate on the current host gave **2.04x** for
that same cell. The gap was not caused by any change since — it was present in
the code those figures describe.

So the old absolute numbers reflected a machine, or a machine state, that no
longer exists, and the document presented them with more confidence than a single
run supports. Treat every absolute figure here as one host on one day; the
relative ordering is the part that travels, and even that only within the
scenario it was measured in. That same cell is the one this revision cannot
adjudicate at all, which is not a coincidence: it has been the least stable
measurement in this document since the document existed.

### An earlier correction, still worth recording

An earlier revision of the *harness* reported registration winning sequential
reads at depth 1 by ~16%. That was an artifact: the two owned-buffer backends
were allocating and zeroing a fresh buffer per operation while the registered one
reused pre-registered buffers, so the comparison included an `alloc_zeroed` on one
side and nothing on the other. Every backend now draws from a pool allocated once
at construction, and the apparent advantage disappeared.

It is recorded because it is the exact failure mode this comparison exists to
prevent — a difference in the benchmark being read as a difference in the
backend — and because it was caught by review rather than by any automated check.

## What this does not tell you

- Anything about **cold-cache or device-bound** workloads, where the ranking
  could differ entirely and where the device, not the software, decides.
- Anything about workloads with **real per-operation latency**, where a
  thread-pool backend's threads block and a completion-based one's do not. Warm
  cache means nothing ever really waits, which is the case least favourable to
  this crate.
- Anything about **scaling past one application thread**. This crate is `!Send`
  by construction and uses one; the comparison holds that constant rather than
  rewarding the alternative for using more.
- **The cost of registering.** Setup — ring, runtime, registration and buffer
  pool — is built once per scenario and depth and is outside every timed region,
  so these figures are per-operation cost only.
- **Anything about the `sys` layer's own async surface.** `sys::AsyncEvent` no
  longer offers asynchronous waiting; the persistent-wait primitive that replaced
  it is crate-internal, for reasons given in [design.md](design.md). A caller
  driving a ring by hand uses `wait_sync` or supplies its own waiting.
- **Why the comparison reports more per operation than a direct probe measured
  for the same shape of work.** Unexplained; recorded in
  [pending-work.md](pending-work.md).

The honest summary is that this crate's remaining costs are visible and its
benefits are still largely unexercised by this measurement. Finding a workload
where completion-based I/O wins on Windows for reasons other than avoided
overhead — and reporting it with the same rigour — is the obvious next piece of
work.

## Parameters

They live in `crates/win-ioring-bench/src/config.rs` and
`crates/win-ioring-bench/benches/comparison.rs`, and every one of them is printed
with every run, because a figure without its parameters is not a result.
