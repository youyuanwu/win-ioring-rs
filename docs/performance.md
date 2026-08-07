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
scenarios by point estimate, but only random read resolves — 0.57x owned and
0.59x registered. Sequential read at 0.87x and 0.91x, and write-then-read at
0.91x and 0.92x, are inside the null band, and no direction is claimed from them.
At eight and sixty-four operations in flight every point estimate is a loss,
ranging 1.08x to 1.44x with owned buffers and 1.03x to 1.60x with registered
ones — but only **five of those fourteen comparisons resolve**: random read at
both depths and both buffer modes (1.44x and 1.54x at depth 8, 1.40x and 1.60x at
depth 64), and sequential read at depth 64 with owned buffers (1.30x). The
direction is consistent across all fourteen; the evidence for it is much thinner
than fourteen cells.

Sequential read at depth 64 with registered buffers displays as **1.24x** and does
*not* resolve, because the band's edge is +24% and the ratio is 1.2395. A
threshold something meets exactly is not a threshold, and this cell is the
document's own illustration of it — it is reported unresolved rather than rounded
into a finding.

**All twenty of this crate's cells were re-measured on overlapped file handles**,
which is a change of default since the previous publication; see
["Handle mode"](#handle-mode-the-one-candidate-that-was-eliminated). The A/B
below establishes that the change does not account for the loss.

**And a fifth backend still says the loss is not about the ring.** `compio`,
which is completion-based on Windows but uses I/O completion ports rather than an
I/O ring, loses to `tokio::fs` in the same places: at depth 64 it is 1.57x on
random read against the ring's 1.40x with owned buffers, and 1.17x on sequential
read against the ring's 1.30x — though only the random-read pair resolves. Of the
twenty compio-against-ring comparisons in the matrix, **eighteen are unresolved,
and those eighteen include every one of the fourteen at depth 8 and depth 64** —
which is to say the two are indistinguishable everywhere the loss happens. That
finding is now in its second run. See
["A third backend"](#a-third-backend-completion-based-but-not-a-ring).

**Everything in this headline is warm page cache.** A separate, opt-in arm
measures unbuffered reads that reach the device, and there the ranking changes:
the crate beats `tokio::fs` at pool width 1 by 7.84x, and loses to `tokio::fs`
at pool 512 across 64 file handles by 1.22x. Neither figure is quotable without
the other, and neither is comparable to the numbers above. See
[Unbuffered](#unbuffered-reads-that-reach-the-device).

Four warnings attach to those paragraphs, and they are not decoration.

**One cell has now given three different answers in three runs.** Write-then-read
at depth 1 with registered buffers read 0.85x, then 1.64x, and now reads 0.92x on
an interval ([169.00, 201.19] µs per I/O) that overlaps `tokio::fs`'s
([182.79, 219.75]) and therefore resolves nothing. The 1.64x that the previous
revision published as "a real loss *in this run*" did not survive its next
measurement. Treat this cell as an instrument reading rather than a property of
the code, and read it beside the standing finding below that four of nine
`tokio-pool-1`-against-`tokio-pool-512` cells reversed between two runs of one
binary. Write-then-read is the widest-interval scenario in the matrix and none of
its twelve non-baseline cells resolve in this run.

**The sequential-read result at depth 1 is a change of *ranking* against what
this document used to publish, and it is not claimed as an improvement in this
crate.** The last revision published *before the harness changed* reported that
cell at 1.90x — a loss. (It is two revisions back now, not one: the intervening
revision already published the current ranking. The reference is kept because the
1.90x figure is the one the ranking changed against.) Nothing in
the crate changed between the two. What changed is the instrument, and the old
number for that one cell is independently known to be unreliable: re-running the
*unchanged* old harness twice, minutes apart, produced figures 35% and 75% higher
than the ones it published. See "What changed when the instrument changed".

**Absolute figures from this document and from any revision before it are not
comparable.** The measurement framework changed and, with it, how often the same
bytes are read. Everything below is per I/O and measured by Criterion; nothing
above 1.0x or below it should be read against a number from the old harness.

**The `tokio::fs` backends open synchronous handles, and this crate no longer
does.** Every `tokio::fs` figure in this document — the 1.00x baseline that every
`relative` is computed against — comes from a handle opened without
`FILE_FLAG_OVERLAPPED`, because `tokio::fs::File` wraps `std::fs::File` and
`std` does not set that flag. The `win-ioring` and `compio` cells are all
overlapped. **This is not an argument from how `std` opens files: the
handle-mode arm reads the mode back off the kernel for every backend it runs,
`tokio::fs` included, and fails the run if any handle is not what its
configuration declared.** The asymmetry is disclosed rather than corrected,
deliberately: changing the baseline in the same run that measures handle mode
would confound the variable the experiment below is built around.

The asymmetry is **not** symmetric in what it costs, and its likely size differs
by configuration rather than being uniformly unknown. At pool width 1 a
synchronous handle should cost `tokio::fs` very little, because a single blocking
thread issues one operation at a time regardless of what the file object permits
— there is nothing for the kernel's per-file-object serialisation to take away.
At pool 512 across a single handle it should cost a great deal, because 512
threads then contend for a lock the overlapped backends never take. That second
case is not hypothetical: the unbuffered arm already measured the shape of it as
a 1-handle against 64-handle result. So the honest reading is that the pool-1
column — which is the baseline for every `relative` in the table — is the column
least likely to be affected, and the pool-512 column is the one most likely to be
carrying a penalty that is an artefact of handle mode rather than of `tokio::fs`.
Recorded as a follow-up with its cost in
[`pending-work.md`](pending-work.md).

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

Four tests in `crates/win-ioring-bench/tests/fairness.rs` hold that to account.
Each takes a **real** backend, wraps it so that it either skips one read in four
or reports full transfers whose bytes never reach anywhere readable, and drives
it through `harness::measure_combination` — the same function a measured
benchmark calls, applying the weakening at the same call site the timed closure
uses. All four require the run to be **rejected**, and all four assert *which*
mismatch was reported, so a run falling over for an unrelated reason does not
count as a pass. Two of them weaken a thread-pool backend and two weaken
`compio-iocp`; no test weakens a ring backend, which is recorded in
`docs/pending-work.md`. A further test proves the weakenings change what was
delivered rather than what was issued, so the four above cannot be passing on
the issue-trace comparison alone.

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
| `compio (IOCP)` | the `compio` runtime, completion-based on Windows via I/O completion ports — **not** an I/O ring |

Four notes on fairness:

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
handle. Harmless to the measurement, since that makes all five alike, but the
printed string is wrong; it is tracked in [pending-work.md](pending-work.md).

**compio opens its files asynchronously, and that costs it about 29 µs per
open.** Every scenario opens inside the timed iteration, so this is a real
fairness item rather than a curiosity, and it is measured in-tree rather than
estimated. The run above recorded **std 15.1 µs (p90 23.8 µs) against compio
44.2 µs (p90 76.5 µs)**, medians over 200 opens each — compio costs **2.9x, or
29.1 µs more per open**. It is produced by `cargo bench -p win-ioring-bench` and
printed in the teardown section of `target/bench-data/fairness.md`. The probe can
be exercised without a full run by `cargo bench -p win-ioring-bench -- --list`,
which builds the whole working set and runs the preparation path
(see [pending-work.md](pending-work.md)) — note that it **overwrites the
account**, so do not run it against a run whose account you still need. The p90s
are worth reading beside the medians: the syscall's sits about 9 µs above its
median, the async open's at nearly twice its own — the cost is not merely larger,
it is *variable* in a way the syscall is not.

The previous publication recorded 14.0 µs against 45.9 µs, a 3.3x ratio and a
31.9 µs delta. Those are **historical** and are not the figures the shares below
are computed from; the probe is re-measured by every run and this run's numbers
are the ones used.

The quantity that matters is the **delta**, 29.1 µs, not compio's absolute
44.2 µs: the other four backends also open a file, and only the difference is a
fairness question. Multiplied by the opens per iteration — **one** for the three
read scenarios, **two** for write-then-read, which opens for the write and again
for the read — it is this share of a depth-64 iteration:

| scenario | opens | extra | iteration | share |
| --- | --- | --- | --- | --- |
| sequential read | 1 | 29.1 µs | 18.11 ms | **0.16%** |
| random read | 1 | 29.1 µs | 5.08 ms | **0.57%** |
| write then read | 2 | 58.2 µs | 44.08 ms | **0.13%** |
| bulk read | 1 | 29.1 µs | 18.48 ms | **0.16%** |

**The direction is asymmetric, and that is the whole reason the figure matters.**
The cost biases compio *slower*. So it is **conservative for any conclusion that
compio also loses** — the loss is real and would only shrink if the open were
free — and **anti-conservative for any conclusion that compio scales well**,
because a fixed per-iteration cost is amortised over more I/Os as depth rises and
can manufacture the appearance of improvement on its own. Both readings appear
below, and each is qualified accordingly rather than with a generic caveat.

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

All fifty cells of the matrix — every scenario, depth and backend — because a
selection invites the question of how it was selected. That is 45 rolling cells
(three scenarios by three depths by five backends) plus the five bulk-read cells,
which run at depth 64 alone.

**Every figure here comes from one run.** Adding a fifth backend invalidated every
stored number, so the whole matrix was re-measured rather than patched. The
bulk-read rows are therefore no longer a table stitched on beside this one, and
the shape comparison in
["The batched window"](#the-batched-window-and-what-it-settled) is now internal to
the same run as everything else. If this table has to be taken again it is taken
whole; nothing in it is ever patched from a second run.

Taken on an AMD Ryzen 7 PRO 6850U (8 cores, 16 logical processors, 16 MiB L3),
28436 MiB of memory, working files on an NVMe volume. `warm_up_time` 1 s,
`measurement_time` 2 s, 100 samples per estimate. The run took **280.2 seconds**
of wall clock, of which 279.3 s was measurement and 0.2 s preparation.

Operations per iteration: **256** for sequential read (256 I/Os), **512** for
random read (512 I/Os), **128** for write-then-read (**256** I/Os — it writes
each block and reads it back) and **256** for bulk read (256 I/Os). Every figure
in the `µs per I/O` column is **microseconds per single I/O** — an iteration's
measured time divided by the I/O count above, not the time for the iteration.
`relative` is against `tokio::fs (blocking pool 1)` within the same scenario and
depth. Bold marks where **this crate** is ahead; compio is a reference point
rather than a competitor and is not bolded, and its own comparisons are made
[below](#a-third-backend-completion-based-but-not-a-ring).

| scenario | depth | backend | µs per I/O [lower, estimate, upper] | relative |
| --- | --- | --- | --- | --- |
| sequential read (64 KiB) | 1 | tokio::fs (pool 1) | [85.91, 87.28, 88.72] | 1.00x |
| sequential read (64 KiB) | 1 | tokio::fs (pool 512) | [99.77, 101.81, 104.22] | 1.17x |
| sequential read (64 KiB) | 1 | win-ioring (owned) | **[75.42, 76.01, 76.64]** | 0.87x |
| sequential read (64 KiB) | 1 | win-ioring (registered) | **[79.05, 79.74, 80.48]** | 0.91x |
| sequential read (64 KiB) | 1 | compio (IOCP) | [71.20, 71.79, 72.43] | 0.82x |
| sequential read (64 KiB) | 8 | tokio::fs (pool 1) | [61.00, 61.35, 61.72] | 1.00x |
| sequential read (64 KiB) | 8 | tokio::fs (pool 512) | [63.14, 63.45, 63.79] | 1.03x |
| sequential read (64 KiB) | 8 | win-ioring (owned) | [69.86, 70.22, 70.62] | 1.14x |
| sequential read (64 KiB) | 8 | win-ioring (registered) | [73.56, 74.32, 75.14] | 1.21x |
| sequential read (64 KiB) | 8 | compio (IOCP) | [69.70, 70.17, 70.69] | 1.14x |
| sequential read (64 KiB) | 64 | tokio::fs (pool 1) | [59.96, 60.32, 60.71] | 1.00x |
| sequential read (64 KiB) | 64 | tokio::fs (pool 512) | [63.37, 63.68, 64.01] | 1.06x |
| sequential read (64 KiB) | 64 | win-ioring (owned) | [76.84, 78.29, 79.91] | 1.30x |
| sequential read (64 KiB) | 64 | win-ioring (registered) | [73.82, 74.77, 75.78] | 1.24x |
| sequential read (64 KiB) | 64 | compio (IOCP) | [70.18, 70.75, 71.35] | 1.17x |
| random read (4 KiB) | 1 | tokio::fs (pool 1) | [26.86, 27.33, 27.81] | 1.00x |
| random read (4 KiB) | 1 | tokio::fs (pool 512) | [26.90, 27.45, 28.00] | 1.00x |
| random read (4 KiB) | 1 | win-ioring (owned) | **[15.30, 15.52, 15.75]** | 0.57x |
| random read (4 KiB) | 1 | win-ioring (registered) | **[16.01, 16.20, 16.41]** | 0.59x |
| random read (4 KiB) | 1 | compio (IOCP) | [10.69, 10.90, 11.13] | 0.40x |
| random read (4 KiB) | 8 | tokio::fs (pool 1) | [7.05, 7.18, 7.31] | 1.00x |
| random read (4 KiB) | 8 | tokio::fs (pool 512) | [12.15, 12.38, 12.62] | 1.72x |
| random read (4 KiB) | 8 | win-ioring (owned) | [10.18, 10.35, 10.54] | 1.44x |
| random read (4 KiB) | 8 | win-ioring (registered) | [10.84, 11.02, 11.22] | 1.54x |
| random read (4 KiB) | 8 | compio (IOCP) | [9.30, 9.51, 9.73] | 1.32x |
| random read (4 KiB) | 64 | tokio::fs (pool 1) | [6.19, 6.31, 6.43] | 1.00x |
| random read (4 KiB) | 64 | tokio::fs (pool 512) | [13.67, 13.99, 14.32] | 2.22x |
| random read (4 KiB) | 64 | win-ioring (owned) | [8.66, 8.81, 8.97] | 1.40x |
| random read (4 KiB) | 64 | win-ioring (registered) | [9.90, 10.11, 10.33] | 1.60x |
| random read (4 KiB) | 64 | compio (IOCP) | [9.73, 9.92, 10.12] | 1.57x |
| write then read (64 KiB) | 1 | tokio::fs (pool 1) | [182.79, 195.88, 219.75] | 1.00x |
| write then read (64 KiB) | 1 | tokio::fs (pool 512) | [182.20, 189.47, 202.14] | 0.97x |
| write then read (64 KiB) | 1 | win-ioring (owned) | **[165.58, 177.99, 200.79]** | 0.91x |
| write then read (64 KiB) | 1 | win-ioring (registered) | **[169.00, 180.58, 201.19]** | 0.92x |
| write then read (64 KiB) | 1 | compio (IOCP) | [164.38, 175.30, 195.37] | 0.89x |
| write then read (64 KiB) | 8 | tokio::fs (pool 1) | [148.20, 158.36, 174.47] | 1.00x |
| write then read (64 KiB) | 8 | tokio::fs (pool 512) | [145.14, 147.13, 149.27] | 0.93x |
| write then read (64 KiB) | 8 | win-ioring (owned) | [160.16, 170.47, 189.58] | 1.08x |
| write then read (64 KiB) | 8 | win-ioring (registered) | [165.74, 177.93, 200.39] | 1.12x |
| write then read (64 KiB) | 8 | compio (IOCP) | [162.51, 174.52, 196.62] | 1.10x |
| write then read (64 KiB) | 64 | tokio::fs (pool 1) | [144.46, 156.41, 178.62] | 1.00x |
| write then read (64 KiB) | 64 | tokio::fs (pool 512) | [149.61, 160.81, 181.43] | 1.03x |
| write then read (64 KiB) | 64 | win-ioring (owned) | [162.07, 173.85, 194.62] | 1.11x |
| write then read (64 KiB) | 64 | win-ioring (registered) | [159.75, 161.70, 163.75] | 1.03x |
| write then read (64 KiB) | 64 | compio (IOCP) | [159.98, 172.17, 194.90] | 1.10x |
| bulk read (64 KiB) | 64 | tokio::fs (pool 1) | [61.36, 61.98, 62.61] | 1.00x |
| bulk read (64 KiB) | 64 | tokio::fs (pool 512) | [66.96, 67.50, 68.09] | 1.09x |
| bulk read (64 KiB) | 64 | win-ioring (owned) | [70.13, 70.80, 71.54] | 1.14x |
| bulk read (64 KiB) | 64 | win-ioring (registered) | [71.09, 71.60, 72.14] | 1.16x |
| bulk read (64 KiB) | 64 | compio (IOCP) | [71.35, 72.20, 73.11] | 1.16x |

**Do not compare µs per I/O across scenarios.** A 64 KiB read moves sixteen times
the bytes of a 4 KiB one, and write-then-read pays a file-system write path the
read scenarios never touch. Compare down a group of five rows, not across them.

**What a repeat run reproduced, and what it did not.** *This finding comes from an
earlier pair of runs of the four-backend matrix, not from the run in the table
above.* It is kept, and marked, because what it establishes — how much of a
relative survives a repeat — is a property of the instrument rather than of any
one table, and re-establishing it would cost a second whole run for no new
information. The same binary was run twice minutes apart, the second against the
first as a stored baseline, and it was the same binary in the checkable sense:
`sequential-read/tokio-pool-1/1` came back at 27.521 ms against 27.540 ms, a
reported change of −0.07%. Recomputing every relative from the second run's own
absolutes:

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
  random read at depth 1 and write-then-read at depth 8 — which is why the
  registration reading below is reported as a count of cells rather than as a
  winner. The fifty-benchmark run moved that count again, to nine cells behind
  and one ahead.

**This paragraph used to claim that every relative figure was reproduced**, with
"the largest disagreement being random read at depth 64 with owned buffers (1.75x
against 1.40x)", and advised treating one significant figure as solid. Four cells
reverse direction between the two runs, and 1.75x against 1.40x is itself a
disagreement in the first significant figure, so both halves were wrong.

**And the table above is a third data point for it.** That cell — random read at
depth 64, owned buffers — reads 1.40x in the fifty-benchmark run, against the
1.75x and 1.40x of the earlier pair. The 1.75x that the headline and `README.md`
used to quote as the top of the loss range is gone from both, because it was a
figure from one run out of three. Read a relative in this document for its
direction and its rough size. Not to a decimal, and on this evidence not to one
significant figure either.

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
backends agree to the digit in all **twenty** ring cells — the other thirty rows
of the matrix are `tokio::fs` and compio, neither of which has a ring, and both
report the figure as not applicable rather than as zero — and **three full runs
produced bit-identical
figures**, so this is not an average over noise. The fifty-benchmark run
published above is a **fifth** run of this table, and it reproduced every figure
to the digit again — this time with all twenty ring cells re-measured on
*overlapped* handles, which the previous four runs were not. That is the more
interesting robustness check anyway: submission batching is a property of the
ring's driver, and changing how the file object was opened did not perturb it by
a single entry.

An earlier revision of this sentence described this run as "a fourth, … with a
fifth backend in the matrix and nine of the ten combinations in a different
backend order". All three clauses were carried forward from the previous
publication and are wrong for this run: it is the fifth, the fifth backend was
already present in the fourth, and the rotation is deterministic over the same
five backends so the order is **identical** to the previous run's, not different.

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

From one run, in **microseconds per single I/O** — each iteration's measured time
divided by its 256 reads, not the time for the iteration:

| shape | backend | [lower, estimate, upper] | relative |
| --- | --- | --- | --- |
| rolling | tokio::fs (pool 1) | [59.96, 60.32, 60.71] | 1.00x |
| rolling | tokio::fs (pool 512) | [63.37, 63.68, 64.01] | 1.06x |
| rolling | win-ioring (owned) | [76.84, 78.29, 79.91] | 1.30x |
| rolling | win-ioring (registered) | [73.82, 74.77, 75.78] | 1.24x |
| rolling | compio (IOCP) | [70.18, 70.75, 71.35] | 1.17x |
| batched | tokio::fs (pool 1) | [61.36, 61.98, 62.61] | 1.00x |
| batched | tokio::fs (pool 512) | [66.96, 67.50, 68.09] | 1.09x |
| batched | win-ioring (owned) | [70.13, 70.80, 71.54] | 1.14x |
| batched | win-ioring (registered) | [71.09, 71.60, 72.14] | 1.16x |
| batched | compio (IOCP) | [71.35, 72.20, 73.11] | 1.16x |

Read by interval overlap within the run. A comparison is treated as **resolved**
only when the two intervals are disjoint **and** the ratio between the two
midpoints clears the −18%/+24% null band. The band is applied to **every** ratio
this document resolves — backend against backend, shape against shape, and depth
against depth within a single backend. It was measured on single-cell repeats
rather than on ratios, so using it this way is a **conservative proxy** that can
only under-claim, never over-claim. A comparison that does not clear both tests is
recorded as **unresolved** rather than reported as a direction.

An earlier revision of this document applied the band only to backend-against-
backend ratios and let disjointness alone settle within-backend ones. That is not
a defensible split, and it did not fall neutrally: it made this crate's shape
comparison "resolve" at −9.6% while compio's depth comparison, at −9.0% and
equally disjoint, was published as unresolved four sections later. The looser rule
landed on the favourable result and the stricter on the unfavourable one. The rule
above is the uniform one, and the bullets below are stated under it.

- **Unresolved, and it would have been reported as a finding under a looser rule:
  this crate is faster in the batched shape by point estimate, in both buffer
  modes.** Owned buffers [76.84, 79.91] against [70.13, 71.54], and registered
  [73.82, 75.78] against [71.09, 72.14]. Both pairs are disjoint, but the ratios
  are −9.6% and −4.2%, well inside the −18% band, so **neither resolves** and no
  direction is claimed from either. **The previous three runs all found the two
  shapes indistinguishable for this crate**, and under the uniform rule this run
  agrees with them.

  Two drafts of this bullet got this wrong in the same direction. The first
  reported the separation as resolved and explained it away as "the rolling/owned
  cell moving, not the batched shape improving" — a decomposition the log
  contradicts: rolling/owned rose 74.40 → 78.29 (+3.89) but batched/owned *fell*
  74.11 → 70.80 (−3.31), and for registered buffers rolling barely moved
  (75.16 → 74.77) while batched fell 74.77 → 71.60 (−3.17), so the batched cells
  are what moved. The second kept "resolves" and dropped the bad explanation. Both
  drafts left a favourable claim standing on disjointness alone, which is the
  looser of the two rules this document was applying. Under the uniform rule the
  claim does not stand at all, and the point estimates are reported here only as
  point estimates.

  What is worth noting without claiming: the batched movement is consistent in
  sign across both buffer modes, and both batched `tokio::fs` cells moved the
  other way (61.11 → 61.98 and 65.98 → 67.50). That is a pattern to watch in the
  next run, not a result from this one. The two shapes also differ in sustained
  depth as well as in shape, so any difference between them mixes the two.
- **Unresolved under the uniform rule, though the direction has now appeared four
  times: `tokio::fs` is slower in the batched shape by point estimate.**
  [59.96, 60.71] against [61.36, 62.61] — disjoint, and disjoint in all three
  earlier runs too ([61.66, 62.33] against [66.53, 68.24], and
  [66.34, 67.78] against [70.90, 72.94]). But the magnitude is **2.7%** per I/O
  this run, against about 8% in the first two and 1.7% in the third, and none of
  those clears the −18% band. **The direction has reproduced four times; no single
  run resolves it.** That is a weaker statement than the previous revision made and
  a more accurate one: four consistent point estimates are suggestive, and
  suggestive is what they are. The 512-thread pool is with it at 6.0%, also
  unresolved.
- **Unresolved, and it was never anything else: compio's shape difference.**
  [70.18, 71.35] against [71.35, 73.11], about 2.1% per I/O — nominally in the
  opposite direction to the previous run's 3.4%. The intervals are disjoint by
  **0.004 µs**, touching to four decimal places, which is why a disjointness-only
  rule was the wrong rule: it certified a difference of two-thousandths of a
  percent of separation as a finding. Under the band it is unresolved, as is the
  previous run's opposite result. Two runs disagreeing, both inside the band, is
  no direction at all. For the record, the async-open cost does **not** confound
  it — both shapes issue 256 I/Os per iteration behind one open, so the per-open
  cost is identical on both rows and cancels in the comparison.

  Taken together, these bullets say something about the instrument rather than
  about the code: **the −18%/+24% band is wide enough to swallow every shape
  effect in this table.** Rolling against batched is a single-digit-percent
  comparison across the board, and this document's noise band cannot see
  single-digit percentages. Anyone wanting to resolve shape effects needs a
  tighter instrument — the paired within-run design used for the handle-mode arm
  below is one, and it resolves about 10%.
- **Unresolved, still: whether the gap between this crate and `tokio::fs`
  narrows.** Within this run the ratio is smaller in the batched shape for both
  buffer modes — 1.30x against 1.14x owned, 1.24x against 1.16x registered — which
  is a larger apparent narrowing than any previous run showed. It is still not a
  result: across four runs bulk read at depth 64 has run 1.14x to 1.28x while
  rolling sequential read has run 0.92x to 1.61x — ranges that overlap heavily —
  and the shape-versus-depth confound applies here in full. The narrowing is what
  a run shows; it is not a result. And under the uniform rule none of the bullets
  above resolves either, so there is no interval evidence here for any shape
  effect in either direction. What the table does show without needing a rule:
  every ring cell in it is slower than every `tokio::fs` cell.

**These two shapes are not directly comparable to each other.** Rolling and
batched sustain different depths by construction — 56.1 against 32.5 — so a
difference between the two rows of one backend mixes shape with depth. The
comparison this table is arranged to support is **between backends within one
shape**, which is the only axis on which the application logic is identical.

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

**One of those three has since been narrowed, a fourth candidate has been ruled
out, and a fifth — handle mode — has now been eliminated by direct measurement.**
The fourth is what the next section is for; the fifth is
["Handle mode"](#handle-mode-the-one-candidate-that-was-eliminated), and it
matters here because it was the most concrete of the outstanding explanations and
it is now off the list. Three candidates remain, and the count of *tested* ones
has gone from zero to one.

## A third backend: completion-based, but not a ring

Everything above compares this crate against a thread pool. That comparison
confounds two variables. This crate is **completion-based** — it hands an
operation to the kernel and collects the result later — and it is **an I/O
ring**, a specific Windows interface with a submission queue, a completion queue
and a `SubmitIoRing` call. `tokio::fs` is neither. So every loss in the table
above could be charged to either property, and nothing in a two-way comparison
can say which.

`compio` is the third point that separates them. On Windows it is completion-based
via **I/O completion ports** — completion-based, no ring. If the loss travels with
the completion model it should appear in compio too; if it belongs to the ring, or
to this crate's implementation of it, compio should be clear of it.

It is not clear of it.

### What compio is here, as confirmed at run time

- **The driver is IOCP, confirmed by the run and not by the documentation.** The
  backend prints `compio_runtime::Runtime::driver_type()` rather than a written
  assumption, so a host or a version that produced something else would say so;
  this run printed `IOCP driver`. There is no mode to select — the driver type is
  a property of the platform build.
- **Completion processing is single-threaded.** The `iocp-global` feature is off,
  so the runtime drives its own thread's completions, and compio's `Driver` is
  neither `Send` nor `Sync`. This is deliberate: it matches this crate's
  single-threaded driver, which is the point of the comparison.
- **`open` is a blocking-pool operation plus a completion-port attach**, and this
  is why the benchmark's `Backend::open_read` and `open_write` are `async fn`.
  A file handle cannot be synthesised through `FromRawHandle` and used, because
  that route skips the attach and produces a file whose operations never
  complete — a defect that would present as a hang, not as an error. The cost of
  the async open is measured and published in
  ["The backends"](#the-backends) above.
- **`sync` is a blocking-pool operation too, and unlike `open` its cost is not
  measured.** compio declares `OpType::Blocking` for `Sync` and implements it as
  `FlushFileBuffers` (`compio-driver-0.12.4/src/sys/op/fs/iocp.rs:21-32`), so
  `file.sync_all().await` is a thread-pool hop rather than a completion. The
  write-then-read scenario commits once per iteration inside the timed region
  (`crates/win-ioring-bench/src/scenario.rs:295`) — deliberately, because a
  backend that skipped the commit would be doing less work than the others.
  **This is a second compio-slower bias in write-then-read, and it is disclosed
  rather than quantified.** It matters where it lands: write-then-read holds the
  widest intervals in the matrix and is the one scenario whose depth-scaling
  result resolves, so the unmeasured part of compio's cost is concentrated in
  exactly the cell doing the most argumentative work. Read the depth-scaling
  section below with that in mind.
- **There is no submission figure for compio, and that is not a gap.** The
  entries-per-submission table exists because a ring batches entries into one
  `SubmitIoRing` and the count is meaningful. IOCP has no such call, so there is
  nothing batched to count. The account reports the figure as *not applicable*
  rather than as zero, because zero would read as "compio does not batch", which
  is a claim this measurement does not make.
- **There is one compio row, not two, and the reason is a fact about Windows.**
  A reader who knows compio has a `buffer_pool` will reasonably ask why there is
  no registered-buffer compio row to set against this crate's registered
  backend. `compio_runtime::Runtime::buffer_pool()` resolves on Windows to
  `compio-driver-0.12.4/src/sys/buffer_pool/fallback.rs`, whose `BufControl` is a
  `VecDeque<u16>` of slot indices (`:7-8`) with a `release` that takes its driver
  argument and returns `Ok(())` (`:20-22`) — a **userspace free list**, not a
  kernel registration. That selection is not a guess: the sibling `mod.rs` picks
  its implementation with `cfg_select!`, and the fallback arm is the `_` default
  taken when neither `fusion` nor `io_uring` is configured. The `iour.rs` sibling,
  which does map to kernel buffer rings, imports `io_uring::types::BufRingEntry`
  and `rustix::mm` and is not compiled on Windows at all. A second compio
  configuration would have spent ten benchmarks of a hard-limited budget
  comparing a `Vec<u8>` pool against a `VecDeque<u16>` pool. The absence is a
  measured property of the platform, not an omission.

The compio backend ran all ten combinations, produced ten timed rows, and agreed
with the reference backend on issue trace and delivered-bytes digest in every one
of them — a disagreement would have ended the run rather than been reported.

**What it costs to have it.** compio arrives as **nine** crates, all of them
dependencies of `win-ioring-bench` alone — a `publish = false` crate — and none
of them reachable from the library: `cargo tree -p win-ioring -e normal` contains
no compio crate at all. They are `compio 0.19.1`, `compio-buf 0.8.3`,
`compio-driver 0.12.4`, `compio-executor 0.1.3`, `compio-fs 0.12.0`,
`compio-io 0.10.1`, `compio-log 0.2.0`, `compio-runtime 0.12.4` and
`compio-send-wrapper 0.7.2`. The dependency is declared with
`default-features = false` and the `fs` feature only, which drops the Linux-only
`io-uring` default and leaves `iocp-global` off. The list is given in full, and
is checkable with `cargo tree -p win-ioring-bench -e normal`, because a partial
list is the kind of thing that gets quoted as a total.

### What it measured

compio's per-I/O cost, and its ratio against `tokio::fs (pool 1)` and both ring
backends. A ratio is **resolved** only if it clears the −18%/+24% null band *and*
the two intervals are disjoint; otherwise it is **unresolved**, and no direction
is claimed from it — here or in the prose below.

| scenario | depth | compio µs per I/O | vs `tokio::fs` (pool 1) | vs win-ioring (owned) | vs win-ioring (registered) |
| --- | --- | --- | --- | --- | --- |
| sequential read | 1 | [71.20, 71.79, 72.43] | 0.82x unresolved | 0.94x unresolved | 0.90x unresolved |
| sequential read | 8 | [69.70, 70.17, 70.69] | 1.14x unresolved | 1.00x unresolved | 0.94x unresolved |
| sequential read | 64 | [70.18, 70.75, 71.35] | 1.17x unresolved | 0.90x unresolved | 0.95x unresolved |
| random read | 1 | [10.69, 10.90, 11.13] | 0.40x **resolved** | 0.70x **resolved** | 0.67x **resolved** |
| random read | 8 | [9.30, 9.51, 9.73] | 1.32x **resolved** | 0.92x unresolved | 0.86x unresolved |
| random read | 64 | [9.73, 9.92, 10.12] | 1.57x **resolved** | 1.13x unresolved | 0.98x unresolved |
| write then read | 1 | [164.38, 175.30, 195.37] | 0.89x unresolved | 0.98x unresolved | 0.97x unresolved |
| write then read | 8 | [162.51, 174.52, 196.62] | 1.10x unresolved | 1.02x unresolved | 0.98x unresolved |
| write then read | 64 | [159.98, 172.17, 194.90] | 1.10x unresolved | 0.99x unresolved | 1.06x unresolved |
| bulk read | 64 | [71.35, 72.20, 73.11] | 1.16x unresolved | 1.02x unresolved | 1.01x unresolved |

**Resolved: compio loses to the one-thread pool as depth rises, in the same
places this crate does.** Random read at depth 8 and 64, 1.32x and 1.57x. The
async-open cost biases compio slower, so this direction is **conservative** —
removing the open entirely would move random read at depth 64 by 0.64%, which
does not shift a 1.57x anywhere near the band. Sequential read at depth 64 moved
the other way this run, to 1.17x, and no longer clears the band; it is reported
unresolved. The load-bearing claim rests on random read, where it is unambiguous
at both depths.

**Resolved: compio beats the ring at depth 1 on random read** — 0.70x and 0.67x,
the only cells anywhere in the matrix where compio separates from both ring
backends. Write-then-read at depth 1 resolved this comparison in the previous
run, at 1.31x and 0.73x; it does not resolve here, at 0.98x and 0.97x. That cell
holds the widest intervals in the matrix and has now given three different
answers in three runs, so its previous appearance as a finding should be read as
the instrument rather than the code.

**Resolved, and stated because the data resolves it: compio beats `tokio::fs` at
depth 1 on random read** — 0.40x, the largest advantage any backend achieves over
`tokio::fs` anywhere in the matrix. Sequential read at depth 1 is 0.8225x, which
is inside the −18% band by a quarter of a percentage point, so it does not resolve
and no direction is claimed from it; the previous run had it at 0.81x and just
outside. Two runs on either side of a band edge is the clearest possible signal
that the cell is at the instrument's limit, not that it changed. It is reported
here rather than left to the table because enumerating only the losses would be a
one-sided reading of a result that cuts both ways.

**Unresolved, and this is the result: compio and the ring are indistinguishable
wherever the loss actually happens.** All fourteen compio-against-ring
comparisons at depth 8 and depth 64 are unresolved — sequential read 1.00x,
0.94x, 0.90x, 0.95x; random read 0.92x, 0.86x, 1.13x, 0.98x; write-then-read
1.02x, 0.98x, 0.99x, 1.06x; bulk read 1.02x, 1.01x. No direction is claimed for
any of them, and none is needed: two implementations that share no kernel
interface, no submission mechanism and no buffer-handling strategy land inside
each other's noise at every depth where this crate loses. **This is the second
independent run to find it**, and the two runs differ in exactly the way that
matters: in the previous run the two backends **differed** in handle mode (the
ring synchronous, compio overlapped), and in this one they **share** it. The
finding survives both configurations, which is a stronger statement than either
run could make alone — it is not an artefact of the handle-mode difference,
because it held when there was one, and it is not an artefact of the two having
been aligned, because it held when they were not.

### Does compio's per-I/O cost fall with depth?

The benchmark measures achieved depth at its own seam, so it cannot see a backend
serialising operations below its own interface — the standing caveat that applies
to every backend in this document applies to compio too. The one thing that would
give indirect evidence of real concurrency underneath is the per-I/O cost falling
as depth rises. It does so on one scenario of three:

| scenario | depth 1 | depth 8 | depth 64 | depth 64 vs depth 1 |
| --- | --- | --- | --- | --- |
| sequential read | 71.79 | 70.17 | 70.75 | −1.4%, **unresolved** |
| random read | 10.90 | 9.51 | 9.92 | −9.0%, **unresolved** |
| write then read | 175.30 | 174.52 | 172.17 | −1.8%, **unresolved** |

**None of the three clears the −18% band this run**, against one of three in the
previous one. Random read falls visibly and does not clear it — its intervals are
disjoint ([10.69, 11.13] against [9.73, 10.12]) but the fall is 9.0%, half the
band. This is the same test that the shape bullets above now fail, applied the
same way: disjoint but inside the band is unresolved, whoever it favours.
Write-then-read, which was the one scenario that resolved last time at
−23.7%, reads −1.8% here on overlapping intervals; its previous appearance rested
on the widest depth-1 interval in the matrix, the same cell flagged above as
having given three answers in three runs.

**So compio's in-kernel concurrency is unverified below its interface, and the
evidence for it is now weaker than it was.** Last run this section could point to
one scenario out of three; this run it can point to none. The reading is
unchanged in substance — the benchmark cannot see below compio's seam — but the
indirect evidence that used to soften it has not reproduced. This is also where
the async-open cost is **anti-conservative**: a fixed per-iteration cost is
amortised over the same 256 or 512 I/Os at every depth, so it does not manufacture
a fall on its own — but it inflates all three depths equally, which compresses the
*relative* fall it would otherwise show. It cannot rescue three unresolved rows,
and it is not offered as an excuse for them.

### What this bears on, and what it leaves alone

The three candidate causes left open above were single-threaded completion
processing against a 512-thread pool, per-completion dequeue cost, and cache
effects. This measurement touches them unevenly, and it is worth being exact
about which.

- **Per-completion dequeue cost — narrowed.** It is *not* the I/O ring's dequeue
  specifically. `PopIoRingCompletion` and `GetQueuedCompletionStatusEx` are
  different code paths in different subsystems, and they cost the same to within
  the resolution of this instrument at every depth where the loss appears. The
  candidate survives only in its model-level form: a per-completion cost that any
  completion-based design on this platform pays. In its implementation-specific
  form it is ruled out.
- **Single-threaded completion processing — untouched.** Both losing backends
  have it, so this matrix cannot discriminate it; there is no arm here with
  multi-threaded completion processing. It is worth noting that the backend that
  wins is `tokio::fs` at pool width **1** — a single blocking thread — so
  "against a 512-thread pool" was already the weaker half of the phrasing, and
  the pool-512 configuration loses to pool-1 in seven of the nine rolling cells.
  The live form of this candidate is about *where* completions are processed,
  not how many threads are available.
- **Cache effects — untouched.** Nothing here varies the cache path. Every figure
  in this document is warm-cache by design.

And a fourth candidate, never written down as one because a two-backend
comparison could not have tested it, is now ruled out: **the loss is not a defect
in this crate's implementation of the I/O ring, and it is not a property of the
I/O ring interface.** An independent runtime, sharing none of this crate's code,
reaching the kernel through a different mechanism, loses by the same margin in
the same cells. Whatever the cause is, it is above the interface or below both of
them — not in between.

That does not make this crate fast. It means the remaining explanation is one
that a completion-based design on this platform pays for reaching a warm page
cache, and that finding a cheaper ring will not recover it.


## Handle mode: the one candidate that was eliminated

This crate now opens files with `FILE_FLAG_OVERLAPPED`. It did not when the
previous matrix was published, and that change is why every `win-ioring` cell
above was re-measured rather than carried forward.

The change was made because of a real asymmetry. `compio` sets the flag
unconditionally, so the previous publication compared **overlapped-handle compio
against synchronous-handle win-ioring**, and the fairness account argued the
difference was irrelevant under a warm cache — nothing waits, so there is nothing
to overlap. That was a mechanism argument, not a measurement, and it was believed
on that basis through three features. It is now measured.

### The prediction, registered before any number existed

`FILE_SYNCHRONOUS_IO_NONALERT` makes the kernel serialise at the file object and
take a per-operation lock even when every read is served from cache and completes
inline. That is a per-operation cost that should grow with the number of
operations in flight — which is the shape of the unexplained loss above.

> Overlapped handles are expected to reduce the warm-cache per-operation cost at
> depths 8 and 64, narrowing or closing the gap to `tokio::fs` at pool width 1.
> The effect should be absent or negligible at depth 1.

Quantified before measuring, from the loss this document recorded as unexplained:
a **0.24** reduction in the gap for sequential read and **0.40** for random read,
both at depth 64, expressed as a difference of ratios against `tokio::fs`.

**The prediction is wrong.** Not unresolved — wrong, with an interval that
excludes an effect of the size predicted, in every one of the eight cells the
analysis was frozen over, and independently in two separate five-run sets.

### How it was measured

Not as a before-and-after. This document's own repeat-run analysis shows
between-run drift large enough to swamp the effect, so comparing the old matrix
against a new one would confound handle mode with drift. Instead **both handle
modes are present in the same run**, as separate backend configurations under the
harness's existing rotation and fairness checks — a paired comparison rather than
a historical one.

The decisive quantity is not the two arms against each other but **the gap to
`tokio::fs`, computed separately for each handle mode within each run**. If
handle mode explained the loss, the overlapped arm's gap would be materially
smaller than the synchronous arm's. The statistic is
`diff = (gap_sync - gap_overlapped)`, and the prediction is that it is positive
and large.

The arm is an opt-in target, not part of the matrix above:

```powershell
cargo bench -p win-ioring-bench --bench handle-mode
```

**Its numbers are not directly comparable to the matrix cells**, and that cost is
stated here rather than only in a methods note. The arm runs six configurations
over two scenarios and three depths against a 320-second budget; the matrix runs
five over four scenarios. Different budgets, different rotation, different
neighbours in the run. Compare within the arm, never between the arm and the
table above.

### Why the opens sit outside the timed region here

The warm-cache matrix times the open along with the I/O. This arm does not, and
the difference is not an oversight — inheriting the matrix's boundary would have
**destroyed the negative control**, which is worse than adding noise.

Opening a file is exactly where the two handle modes differ: one passes
`FILE_FLAG_OVERLAPPED` and the other does not, and the two calls do not cost the
same. Inside the timed region, that difference is charged to every iteration and
amortised over however many I/Os the iteration issues. At depth 64 it is spread
thin. **At depth 1 it is spread over almost nothing**, so the depth-1 cells —
the cells whose entire job is to show *no* effect where the mechanism predicts
none — would have shown a difference between the arms driven by open cost rather
than by per-operation serialisation.

That failure mode is worse than a plain confound because of what it would have
done to the analysis. The pre-registered reading treats a depth-1 flag as
evidence of run-level drift undermining the whole result. A systematic open-cost
difference would have produced exactly that flag, at exactly that cell, for a
reason that has nothing to do with drift — and the safeguard would have reported
it as drift. A clean, plausible, wrong result with the check silently disarmed.

So the boundaries differ **because the arms are asking different questions**, and
the rule is that the timed region excludes anything that varies with the
independent variable but is not the thing being measured. In the matrix, opens are
part of the workload and identical across backends within a cell. In this arm they
are contaminated by the variable under test. The unbuffered arm hoists its opens
for a third reason again — the configurations there hold different numbers of
handles by design. Three arms, three boundaries, one rule. The next arm added to
this suite should expect to make the choice rather than inherit it, and
`handle_mode::OPENS` in the bench crate carries the same reasoning next to the
constant that encodes it.

### The result: two independent five-run sets

Five whole runs in fresh processes per set. The pilot established the
within-run band; the confirmatory set was collected after the entire analysis was
frozen — cells, criterion, effect sizes, outlier rule and run count — and the
freeze is evidenced by a commit made before the data existed.

Gap to `tokio::fs` at pool 1, and the paired difference between handle modes,
confirmatory set:

| scenario | buffers | depth | gap overlapped | gap synchronous | diff | 95% CI | predicted |
| --- | --- | --- | --- | --- | --- | --- | --- |
| sequential read | owned | 1 | 0.80x | 0.84x | +0.03 | [-0.01, +0.08] | control |
| sequential read | owned | 8 | 1.21x | 1.22x | +0.00 | [-0.04, +0.05] | 0.24 |
| sequential read | owned | 64 | 1.24x | 1.21x | -0.03 | [-0.07, +0.02] | 0.24 |
| sequential read | registered | 1 | 0.83x | 0.83x | -0.00 | [-0.01, +0.01] | control |
| sequential read | registered | 8 | 1.26x | 1.24x | -0.02 | [-0.08, +0.04] | 0.24 |
| sequential read | registered | 64 | 1.28x | 1.27x | -0.02 | [-0.12, +0.08] | 0.24 |
| random read | owned | 1 | 0.57x | 0.67x | +0.09 | [-0.18, +0.37] | control |
| random read | owned | 8 | 1.36x | 1.41x | +0.05 | [-0.05, +0.15] | 0.40 |
| random read | owned | 64 | 1.52x | 1.50x | -0.02 | [-0.08, +0.04] | 0.40 |
| random read | registered | 1 | 0.68x | 0.58x | -0.11 | [-0.35, +0.14] | control |
| random read | registered | 8 | 1.45x | 1.48x | +0.02 | [-0.10, +0.15] | 0.40 |
| random read | registered | 64 | 1.59x | 1.57x | -0.02 | [-0.13, +0.09] | 0.40 |

**Eight of eight confirmatory cells exclude the predicted effect. None of the
four depth-1 controls flagged.**

The pilot set agrees. Seven of its eight confirmatory cells excluded the
predicted effect and the eighth was unresolved — random read, registered, depth 8,
whose interval [-0.71, +1.66] is the widest in either set and is discussed under
the transient below.

**Both sets are published regardless of agreement**, which was committed to
before the confirmatory set was collected. They agree.

### The negative control, and a pre-registered recurrence test

The pilot flagged one depth-1 control: sequential read, registered, at -0.04 with
interval [-0.06, -0.02], the synchronous arm faster. One flag in four controls is
about what a 95% interval produces by chance, but "about the chance rate" is a
judgement made after seeing the number, so it was turned into a test before the
second set: **if that cell flags again, two independent sets agreeing is no longer
a chance rate and must be reported as a real signal at the negative control** —
which would undercut the main conclusion.

It did not recur. The same cell in the confirmatory set reads -0.00 on [-0.01,
+0.01], the tightest null in either set. The chance-rate reading is supported by a
test rather than by a preference.

### What the instrument can and cannot resolve

This is the limit the conclusion rests on, and it belongs beside the conclusion
rather than in a footnote.

The paired statistic's 95% interval has a **median half-width of about 10% on the
ratio**, ranging from 0.9% in the best cell to 27.6% in the worst of the
confirmatory set (118.6% in the worst pilot cell). Per-cell within-run spread has
a median of 13.3% and a p90 of 24.5%.

The twelve confirmatory half-widths, in percent, are 0.95, 4.40, 4.50, 4.85,
5.90, 6.00, 9.90, 9.90, 10.65, 12.50, 24.05 and 27.60. They are listed because a
draft of this section published the median as "about 6%", which is the sixth of
twelve — the lower of the two middle values, and a convention used nowhere else in
this analysis. The mean of the two middle values is 7.95% and the convention the
analysis scripts use throughout gives 9.90%. **The error made the instrument look
sharper than it is, which is the flattering direction**, and it is recorded rather
than silently corrected.

So: **what is excluded is an effect of the predicted magnitude — 0.24 and 0.40 —
not any effect at all.** An effect of around 10% or less would not be reliably
visible here and is not ruled out. Every statement of this result should carry
that distinction, and the distinction is why the outcome is "the predicted
mechanism does not explain the loss" rather than "handle mode does nothing".

### The transient, and the rule that did not catch it

One measurement in pilot run 5 — random read, registered, synchronous, depth 8 —
came in at 14063 µs against 4653–5784 µs for the same cell in the other four
runs. The rest of run 5 is normal, so it is a transient on the host rather than a
property of the configuration.

The frozen outlier rule excludes a value at or above **3x its cell median**. This
one is **2.63x**, so the rule does not exclude it, and it was not excluded. The
bound was deliberately **not** lowered after seeing the value, because a threshold
chosen to catch the observation it was written for is not a threshold. That cell
is the one pilot cell reporting unresolved, and it reports unresolved with the
transient included.

Removing it post hoc gives +0.05 on [-0.10, +0.20] — which would exclude the
predicted effect and make the pilot eight for eight. That figure is recorded here
because suppressing it would be worse, and it is labelled post hoc and is not the
primary result.

### What this settles

**Outcome: the mechanism argument in this document is confirmed by measurement,
and a candidate is eliminated.**

Three things follow, and none of them is a consolation prize.

- **The previously published figures are retroactively fair.** The matrix that
  compared overlapped compio against synchronous win-ioring was believed to be a
  fair comparison on a mechanism argument. It now has a measurement behind it: at
  warm cache, on this workload, handle mode does not move the gap by anything
  near the size that would have mattered. A document that argued a mechanism, was
  believed on the argument for three features, and then had the argument
  confirmed by direct test is in better standing than one that never tested it.
- **A candidate is eliminated from a list of three**, and it was the most concrete
  of them. That is progress on the open question at
  ["What that leaves unexplained"](#what-that-leaves-unexplained), not an absence
  of progress.
- **The prediction was wrong, and can be said to be wrong with an interval that
  excludes the predicted effect.** That is the strongest form a negative result
  can take. It is not "we failed to find an effect"; it is "an effect of the size
  we predicted is not there".

The gap itself reproduces in the arm, on overlapped handles: 1.24x sequential and
1.52x random at depth 64 with owned buffers. Those are **not** the matrix's
figures for the same cells — the matrix reads 1.30x and 1.40x — and they are not
supposed to be, for the reasons given above about comparability. What reproduces
is the loss's existence, its direction and its rough size, measured on a handle
mode the previous matrix never used. The loss is real, it is not handle mode, and
at depth 1 the crate is still ahead — 0.57x to 0.83x across the four control
cells — so whatever the cause is, it appears as depth rises and is unchanged by
how the file was opened.

### Why the default changed anyway

The measurement says handle mode does not explain the warm-cache loss. It does
not say the flag is worthless, and the flag stays on for reasons the matrix above
cannot see: the unbuffered arm's configurations require overlapped handles, since
synchronous handles collapse under no-buffering, and a synchronous handle is a
correctness hazard for any caller who does reach a case where operations block.

`File::open` and `File::create` set the flag and there is no public route through
them to a handle that does not. `File::from_std` and `File::from_raw_handle`
adopt handles the caller opened, and those are deliberately left alone — a caller
doing depth-1 I/O, or adopting a handle for a file type that cannot be
overlapped, is not making a mistake, so neither constructor rejects or asserts.
This is a weaker guarantee than compio's, which holds because compio's `from_std`
is `pub(crate)`, and the difference is documented on both constructors rather
than papered over.

## What changed when the instrument changed

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
benchmarks matching the filter but still prepares, warms and verifies all fifty
of them, so the fairness check never narrows because somebody typed a
filter; the benchmarks it did not time are marked
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

**Does the band still apply after the handle-mode change?** It was measured on a
thirty-six-benchmark suite whose ring backends opened synchronous handles, and
both of those facts have changed. The question was asked deliberately rather than
assumed away, and the answer is **yes, and it now has independent support**.

The handle-mode arm measures the same quantity by a different route: five whole
runs of one configuration in fresh processes, which is a five-run version of the
two-run comparison above. Its per-cell spread across those runs has a **median of
13.3%, a mean of 17.1% and a p90 of 24.5%** — measured on a different suite, a
different budget, and on both handle modes. That lands close enough to
−18%/+24% that adjusting the band would be fitting it to noise. So the band is
kept as published, and it is no longer resting on a single pair of runs of a
suite that no longer exists.

Two limits on that. The arm's figures are not comparable to matrix cells in
absolute terms, for the reasons given in the handle-mode section; what transfers
is the *spread*, which is a property of the host. And the arm covers two read
scenarios, so it says nothing about write-then-read — which is where the widest
intervals in the matrix are, and where the band is most likely to be optimistic.

## The fairness account

Criterion has no place for facts that are not timings, so everything else the
comparison knows is written to a sidecar: line by line to **stderr** as the run
proceeds, and in full to `target/bench-data/fairness.md` at the end. It carries

- the configuration, the per-scenario shapes and the time budget actually used;
- host processor count, physical memory, and the volume the working files are on;
- the warm-cache premise, stated against both the **resident** working set that
  must stay cached (264 MiB) and the bytes each scenario **touches** per iteration
  (16 MiB, 2 MiB, 16 MiB, 16 MiB), which are no longer the same number;
- every backend's name, configuration and availability, with a reason for each
  one that was unavailable and each one that prepared and then failed;
- the run order actually used;
- per combination, the I/Os per iteration, the iteration count, the achieved
  concurrency, and whether it was **timed** or **verified but not timed**;
- the reference backend and the agreement verdict per (scenario, depth);
- the number of drivers built against the number of **ring** combinations
  measured — 20 of the 50, because the two thread-pool backends and compio
  build none;
- the write file's size after the run, and whether the first and last measured
  iteration issued and delivered the same work;
- preparation and measurement wall clock.

Achieved depth and the warm-cache premise live there for that reason and not
because they are unimportant. The standing caveat applies wherever they are read:
achieved depth is measured above the backend's own interface and cannot see a
backend serialising below it.

## Run order

Backends are measured in a **rotated** order: the five are rotated left by the
combination index, so each takes a turn going first. That is a decision, retained
deliberately, not an inherited habit.

Criterion's per-benchmark warm-up absorbs settling *within* a benchmark. It does
nothing about drift *across* a five-minute run, which is what rotation is for:
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
defaults of 3 s and 5 s, fifty benchmarks are 400 seconds of floor against a
six-minute budget — over it before a single I/O is issued. Reducing what an
iteration does cannot get below that floor; only the budget can.

**The budget is a number, not an aspiration.** It used to exist only in this
paragraph, which is no use as a constraint when somebody proposes a new scenario.
`RUN_BUDGET` is 360 seconds and `Budget::CHOSEN` holds the two timing values, and
a test computes the matrix's floor from the real depth lists and checks it. The
check is against **half** the budget, not all of it: three recorded runs cost
1.79x, 1.83x and 2.06x their floor once preparation, untimed warm-ups, analysis
and window overruns are counted, so a matrix whose floor merely fits the budget
will overrun it in practice. At fifty benchmarks the floor is 150 seconds against
a 180-second limit, which is room for about two more combinations. Past that,
something has to be traded away — which is how bulk read came to run at depth 64
alone.

**Why the budget moved from 300 seconds to 360.** Adding compio as a fifth
backend grew the matrix by 25%, from forty benchmarks to fifty, and took the
floor from 120 seconds to 150. Under the old budget the limit was also 150
seconds, so the matrix would have been affordable by exactly nothing — passing
the check with zero margin, which is indistinguishable on paper from a matrix
that is about to overrun.

The budget was raised rather than the matrix trimmed, and the distinction that
decided it is worth stating because it recurs. **`RUN_BUDGET` is a chosen
number: how long a run may take before people stop doing them.** It is not a
measured property of the host. The `RUN_BUDGET / 2` rule *is* measured — it comes
from the 1.79x, 1.83x and 2.06x above — and it **did not move**. Only the chosen
half changed.

The alternatives were considered and are recorded because they look cheaper than
they are. Dropping a depth from the matrix would have paid a real data point for
a saving that still left no margin. Shortening `measurement_time` further would
have widened every published interval in this document — degrading all fifty
results to protect a number nobody measured — and it would have damaged the
comparison compio was added to make in the first place, since the depth-scaling
question is exactly where tighter intervals matter most. Trading a *chosen*
constraint against a *measured* one is the wrong way round.

The cost is that a full run goes from about four minutes to about five.

**Measured: 280.2 seconds.** The fifty-benchmark run published above came in at
280.2 s of wall clock, measured at the shell; the account's own timers report
0.2 s of preparation and 279.3 s of measurement, and the difference is process
startup and teardown outside them. It is inside the 269–309 s that scaling the
forty-benchmark
runs by 25% projected, and 80 seconds under the 360-second budget. It is **not**
comparable to the "190 to 255 seconds" this document used to quote, which was a
*thirty-six*-benchmark matrix; reading fifty benchmarks against that band would
overstate the growth this change caused. The projection held, so it is left
standing rather than corrected.

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

**280.2 seconds** on the host above, with the working files already present and
warm; the first run on a fresh checkout also creates a 256 MiB file. That is a
single measurement of the fifty-benchmark matrix, and it replaces the "190 to 255
seconds" band this section used to give, which was a *thirty-six*-benchmark
measurement and was never re-published at forty or at fifty. The historical
figures are kept for the spread
they show: five timed runs of the thirty-six-benchmark matrix came in at 190, 210,
213, 225 and 254 seconds; three of the forty-benchmark matrix at 247, 219 and
215. **One run is not a band.** Budget for the slow end of that growth, not for
280 seconds exactly. The spread is the machine, not the suite: the same binary
measuring the same work varies by a minute depending on what else the host is
doing, which is the same fact the noise-floor finding above reports in a
different unit. The account prints preparation and measurement separately — 0.2 s
and 279.3 s in this run.

It writes about **12 GiB** to `target/bench-data/write.dat` — 8 MiB per
iteration, across roughly 1570 write-bearing iterations. The file itself stays
8 MiB, because each iteration truncates and rewrites it; the wear is in the
writing, not in the size. That is the price of statistics on a write-bearing
scenario, and it is stated here so nobody discovers it from a wear indicator.

## What to take from it

- **At depth 1 the crate is ahead on random reads**, by 0.57x/0.59x, and that
  result resolves. Its sequential-read leads (0.87x/0.91x) and its
  write-then-read leads (0.91x/0.92x) are inside the null band and resolve
  nothing, so read them as "no worse", not as wins. Depth 1 is where the wake-path
  work landed: with one
  operation in flight there is nothing to amortise the cost of parking over, so
  the driver paid it in full on every operation. Write-then-read with registered
  buffers has now read 0.85x, 1.64x and 0.92x in three runs — the single most
  unstable cell in the matrix, and not a result in any direction.
- **Concurrency still does not favour this crate on this workload, and the
  reason is not the one this document used to give.** The advantage
  completion-based I/O is supposed to earn — coalescing outstanding operations
  into one submission — is *not* missing: it was measured, and it runs at exactly
  the configured depth in every cell above. The crate loses anyway. It is not the
  ring's fault either: a completion-based runtime with no ring loses by the same
  margin, which is [the third backend's
  result](#a-third-backend-completion-based-but-not-a-ring). And it is not the
  file handle's fault: that candidate was measured directly and
  [eliminated](#handle-mode-the-one-candidate-that-was-eliminated).
- **A narrow blocking pool beats a wide one for small reads.** `tokio::fs` at one
  blocking thread ran 2.22x faster than at 512 on random reads at depth 64. That
  is contention, not I/O. (Previous runs put the same cell at 2.32x and 1.82x.
  The direction has held across every run; the size has not.) Note that this is
  also the comparison most exposed to the disclosed handle-mode asymmetry: both
  `tokio::fs` backends open synchronous handles, and 512 threads contending on
  one such handle is exactly where that costs most.
- **Registration still does not pay for itself here.** It is behind the
  owned-buffer path in eight of the ten (scenario, depth) cells in this run and
  ahead in two — sequential read and write-then-read at depth 64, by 4.5% and
  7.0% — having been behind in nine and ahead in one in the previous run, and
  behind in six of nine before that. Its own cost is excluded from these figures,
  so this is the per-operation comparison alone.
- **Random reads are where the crate loses hardest above depth 1**, at 1.40x to
  1.60x, and random read is the only scenario whose losses resolve at *both*
  depths and for *both* buffer modes. Sequential read runs 1.14x to 1.30x,
  write-then-read 1.03x to 1.12x and bulk read 1.14x to 1.16x, but of those ten
  comparisons only one resolves — sequential read at depth 64 with owned buffers.
  This bullet used to name sequential reads as the worst case at
  "1.10x to 1.25x", which was never what the table said: those are the smallest
  ratios above depth 1, not the largest. Sequential read is the most expensive
  scenario in *absolute* microseconds per I/O, because 64 KiB transfers are
  dominated by data movement rather than per-operation overhead — which is a
  statement about where there was least for this work to remove, not about where
  the crate falls furthest behind.

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
  A separate, opt-in arm now measures this; see
  [Unbuffered: reads that reach the device](#unbuffered-reads-that-reach-the-device).
  Its figures are **not comparable** to the table above and must not be read
  against it.
- Anything about workloads with **real per-operation latency**, where a
  thread-pool backend's threads block and a completion-based one's do not. Warm
  cache means nothing ever really waits, which is the case least favourable to
  this crate. Also addressed by the unbuffered arm — and the answer there is not
  the simple one this bullet implies.
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

## Unbuffered: reads that reach the device

Everything above this section is **warm page cache by design**. This section is
not. It is a separate, opt-in benchmark target measuring reads issued with
`FILE_FLAG_NO_BUFFERING`, which bypass the operating system page cache and pay
real device latency.

**Do not read these numbers against the table above.** They answer a different
question, they have their own noise band, and they are far more host-dependent
than anything else in this document. Nothing here supersedes or amends the
50-cell warm-cache matrix; that matrix was not re-run and its code path was not
touched. Per-operation microsecond figures from the two sections are not
comparable in either direction.

### Why it was measured

The crate loses to `tokio::fs` at blocking-pool width 1 as queue depth rises,
and two previous investigations removed the obvious explanations: submission
batching was already operating at the full configured depth, and `compio` — a
completion-based backend that is not a ring — loses in the same places
indistinguishably. The cause is still recorded as unknown.

The hypothesis this arm tests: under a warm cache a `ReadFile` never blocks, so
there is no waiting to overlap, and completion-based I/O pays per-operation
machinery for a benefit that cannot exist. Disable the cache and a single
blocking thread is structurally capped at **one outstanding device request**,
while a ring holds the full configured depth. That is the condition under which
the design should pay.

### The prediction, registered before measuring

Recorded in the feature's specification before any number existed, so that a
null or contrary result could not be quietly reframed afterwards:

> At depth 64 unbuffered, `win-ioring` is expected to beat `tokio::fs` at pool
> width 1 by a wide margin, because pool-1 is capped at one outstanding device
> request and the ring is not.

**It was confirmed, and it was also beside the point.** Both halves of that
sentence are the result, and neither is quotable without the other.

### The headline

Random read, 4 KiB, depth 64, µs per I/O:

| configuration | handles | blocking threads | µs/IO |
|---|---:|---:|---:|
| `tokio::fs` pool 512, **64 handles** | 64 | 512 | **11.33** |
| `win-ioring` registered | 1 | — | 13.70 |
| `win-ioring` owned | 1 | — | 13.86 |
| `compio` (IOCP) | 1 | — | 14.41 |
| `tokio::fs` pool 1 | 1 | 1 | 108.62 |
| `tokio::fs` pool 512, 1 handle | 1 | 512 | 116.41 |

- The ring beats `tokio::fs` at pool width 1 by **7.84x**. The prediction holds,
  and the margin is large.
- The ring **loses to `tokio::fs` at pool 512 with 64 handles by 1.22x**. That
  configuration is the honest competitor: with real device latency, 512 blocking
  threads across 64 file handles also achieve deep outstanding I/O.

A victory declared over pool-1 alone, while the multi-handle thread pool is
ahead, would be a misleading result. It is not presented as a win. The ring's
advantage here is **resource cost, not speed**: it reaches within 1.22x of the
thread pool's throughput with one handle, one thread and no thread per
outstanding operation. That is a real engineering property and it is a different
claim from being faster.

### Handle count was the variable, not pool width

This is the most useful thing the arm found, and it corrects a framing this
document has used since the thread-pool backends were first split by width.

Holding everything else fixed, per run, at random read depth 64:

| what changes | ratio, across 5 runs | resolved? |
|---|---|---|
| pool width 1 → 512, **one handle both** | 0.93, 1.02, 0.95, 0.95, 0.95 | no — straddles 1.0 |
| handles 1 → 64, **pool 512 both** | 10.27, 8.75, 9.48, 8.98, 10.16 | yes — 8.75x to 10.27x |

Adding 511 threads to a one-handle configuration does nothing measurable.
Adding 63 file handles at a fixed pool width moves the result by roughly nine to
ten times. The thread pool was never limited by its width; it was limited by
serialisation at the file object, and one handle is one queue however many
threads are pushing on it.

This also explains why pool 512 beats pool 1 here but *loses* to it in the
warm-cache table: those are different mechanisms. Warm-cache pool 512 loses to
pool 1 on scheduling contention with nothing to overlap, and that result stands
unchanged. Neither section overturns the other.

### Sequential read, 64 KiB

The weaker probe, and included because its absence would have been a choice
worth questioning: a drive's own readahead can serve sequential unbuffered reads
from its internal cache regardless of the OS page cache. Depth 64, µs/IO:

| configuration | µs/IO |
|---|---:|
| `tokio::fs` pool 512, 64 handles | 29.67 |
| `win-ioring` registered | 32.15 |
| `win-ioring` owned | 32.45 |
| `compio` | 32.56 |
| `tokio::fs` pool 1 | 193.82 |
| `tokio::fs` pool 512, 1 handle | 198.96 |

Same shape, smaller margins: ring over pool-1 is 5.97x, multi-handle pool over
ring is 1.09x.

### The noise band for this arm

**−5.9% to +14.0%**, measured for this arm specifically.

Five whole runs, each in a fresh process, on an otherwise idle machine — not
five samples within one run, which would not capture between-process or drive
state drift. Each cell's spread is taken against its own median across those
five runs; the figures above are from the first run.

This band is **not** the roughly −18% to +24% band recorded earlier for the
warm-cache arm, is not derived from it, and neither substitutes for the other.
It is also not the "±17%" figure this document previously retracted. Device I/O
has higher and differently shaped variance — SLC cache exhaustion, thermal
behaviour, background garbage collection, drive state drift — and the asymmetry
above is the visible sign of it: a run can be slowed by drive state, but nothing
makes the device faster than it is.

Both headline claims were tested by pairing runs rather than comparing ranges,
which is stricter. Across all five runs the ring beat pool-1 by 7.57x to 8.00x,
and the multi-handle thread pool beat the ring by 1.12x to 1.27x — the same
direction every time, with the ring never once ahead. Both are resolved. The
1.22x is the smaller claim and it is the one the band was measured for.

### What this arm does not tell you

- **Disabling the OS page cache does not disable the drive's cache**, nor its
  readahead. `FILE_FLAG_NO_BUFFERING` is an operating-system flag and has no
  authority over the device. The read file is 256 MiB, which a modern SSD may
  substantially hold in its own cache, so some fraction of these reads may not
  have reached NAND at all. Random access over the full extent is the better
  probe for this reason and is why it is the primary one; the residual
  uncertainty is not eliminated, only reduced, and it cannot be measured from
  the host.
- **These figures are one drive.** More so than anything else in this document.
  The host is a Micron `MTFDKBA1T0TFK` NVMe SSD on an NTFS volume. Alignment
  requirements are volume-dependent; this volume reported
  `AlignmentRequirement` 4 B, `LogicalBytesPerSector` 512 B,
  `PhysicalBytesPerSectorForAtomicity` 4096 B,
  `PhysicalBytesPerSectorForPerformance` 4096 B, and `GetDiskFreeSpace`
  `BytesPerSector` 512 B. The harness takes the **strictest** of these, 4096 B,
  as its granularity. Those four APIs disagreeing by a factor of a thousand is
  itself worth knowing: do not hardcode 4096, and do not trust any single one of
  them.
- **Nothing about µs/IO compared to the warm-cache table.** Stated twice
  deliberately.

### How to reproduce

```text
cargo bench -p win-ioring-bench --bench unbuffered
```

Opt-in: `bench = false` in the manifest keeps it out of a bare `cargo bench`, so
it cannot inflate the main run's budget or share its stored baselines. It has its
own wall-clock budget of 600 s; a full 36-cell run takes 192 s to 204 s here,
median 202 s.
Criterion group names are prefixed `unbuffered-` so the two arms cannot share a
`target/criterion` group directory.

The arm keeps its own working file, and that is load-bearing rather than tidy: a
single *buffered* read of a file collapses subsequent unbuffered reads of that
file by roughly an order of magnitude for the life of the process. Sharing the
warm-cache arm's data file — which is read buffered before every run — would
have produced a plausible null result rather than an obvious failure. See
[testing.md](testing.md) for that hazard and the type-level guard against it.

### One methodological difference from the main matrix

The opens sit **outside** the timed region in this arm. In the warm-cache arm
they sit inside it, which is the right choice there.

The reason is that the configurations here deliberately hold different numbers
of file handles — that is the variable under test — and an unbuffered open costs
about as much as a whole read. Charging opens per iteration would tax whichever
configuration holds the most handles, which is the multi-handle thread pool: the
one configuration whose result is inconvenient.

Measured rather than assumed. Opening 64 handles unbuffered costs about 555 µs
(median of seven), one handle about 9.3 µs; spread over the 256 operations in an
iteration that is 2.17 µs/IO for the multi-handle configuration and 0.036 µs/IO
for a single-handle one. Charged, the multi-handle figure moves 11.33 → 13.50
(+19%) and the ring's moves 13.86 → 13.90 (+0.3%), which compresses the
competitor's lead from **1.22x to 1.03x**. So the choice does not change who
wins — the thread pool is still ahead either way — but it would have shrunk a
genuine margin almost to nothing, in the crate's favour, on the strength of a
bookkeeping decision. The direction of that bias is the reason for the choice.

An earlier estimate of this effect, quoted from a single cold-start
measurement, was about three times too large and is withdrawn; the figures here
are medians of repeated runs.
