# Performance

What this crate costs against the alternative, measured rather than assumed.

`cargo run -p win-ioring-bench --release` produces the table. `--clean` removes
the working files it leaves under `target/bench-data`.

## The headline

**`tokio::fs` is still faster than this crate on most of what is measured here,
but no longer on all of it.** At one operation in flight this crate now wins two
of the three scenarios — random reads at 0.64x and write-then-read at 0.93x —
having lost both before the driver's wake path was rewritten. Everywhere else it
loses, by 1.18x to 1.90x.

That is a change of ranking, not just of numbers, and it is stated first because
the previous revision of this document led with an unqualified loss and would
have been quietly wrong to keep doing so.

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

A run whose trace disagrees with the others is **rejected, not reported**. Two
tests deliberately weaken a backend, one by issuing fewer operations and one by
reporting transfers whose bytes never reach anywhere readable, and both must fail
the run.

That second one is not hypothetical. Before the registered-buffer redesign, a
registered read resolved to a transfer count with no way to read the buffer it
filled — so the registered path would have benchmarked well precisely because it
delivered less than everything it was being compared against. The comparison
could not be built honestly until that was fixed.

## The backends

| Backend | What it is |
|---|---|
| `tokio::fs (blocking pool 1)` | `spawn_blocking` + `seek_read`/`seek_write`, one blocking thread |
| `tokio::fs (blocking pool 512)` | the same, at the default pool width |
| `win-ioring (owned buffers)` | this crate, caller-owned buffers, unregistered handles |
| `win-ioring (registered)` | this crate, registered buffers and file handle, registration-naming operations |

Two notes on fairness:

**The thread-pool backend uses `spawn_blocking`, not `tokio::fs::File`.** That
type is cursor-based and cannot express several *positional* operations
outstanding at once, which is exactly what this varies. Underneath, it does what
this does — hands the work to the blocking pool — so the mechanism is the same.

**What varies between the two thread-pool configurations is pool width, not
runtime flavour.** All the I/O lands on the blocking pool regardless of flavour,
and the application work is trivial, so two flavours would measure one thing
under two labels.

## Reading the numbers

Each cell reports median, min and max across five repeats after a discarded
warm-up, and a figure relative to the first backend. **Look at the spread before
believing a difference**: several of the gaps here are within run-to-run
variation.

Achieved depth is measured by the harness, because one backend's outstanding
count includes work its kernel has not accepted and the other exposes nothing
comparable. It therefore **cannot see a backend serialising operations below its
own interface** — read it beside that backend's configuration, not alone.

## Representative result

Taken on a 16-logical-processor Windows host, working files on an NVMe volume.
Times in microseconds; `relative` is against the first row.

```
## sequential read — depth 1, 4096 operations
tokio::fs (blocking pool 1)          371299.4      1.00x
tokio::fs (blocking pool 512)        782989.9      2.11x
win-ioring (owned buffers)           704884.9      1.90x
win-ioring (registered)              714631.1      1.92x

## random read — depth 1, 1024 operations
tokio::fs (blocking pool 1)           54711.4      1.00x
tokio::fs (blocking pool 512)         55040.4      1.01x
win-ioring (owned buffers)            35221.9      0.64x
win-ioring (registered)               37774.5      0.69x

## random read — depth 64, 1024 operations
tokio::fs (blocking pool 1)           11769.4      1.00x
tokio::fs (blocking pool 512)         25973.0      2.21x
win-ioring (owned buffers)            22245.9      1.89x
win-ioring (registered)               24012.8      2.04x

## write then read — depth 1, 1024 operations
tokio::fs (blocking pool 1)          467846.2      1.00x
tokio::fs (blocking pool 512)        478464.5      1.02x
win-ioring (owned buffers)           434025.0      0.93x
win-ioring (registered)              437991.5      0.94x
```

## What to take from it

- **At depth 1 the crate is now competitive, and sometimes ahead.** This is
  where the wake-path work landed: with one operation in flight there is nothing
  to amortise the cost of parking over, so the driver paid it in full on every
  operation. Removing it moved random reads at depth 1 from 1.02x to 0.64x.
- **Concurrency still does not favour this crate on this workload.** The
  advantage completion-based I/O is supposed to earn — coalescing outstanding
  operations into one submission — does not show up against a warm cache, where
  each operation is cheap enough that the driver's own bookkeeping is a visible
  share of the cost.
- **A narrow blocking pool beats a wide one for small reads.** `tokio::fs` at one
  blocking thread is roughly twice as fast as at 512 on random reads at depth 64.
  That is contention, not I/O.
- **Registration still does not pay for itself here.** It is a consistent few
  percent *behind* the owned-buffer path. Its own cost is excluded from these
  figures, so this is the per-operation comparison alone.
- **Sequential reads at depth 1 remain the worst case**, at 1.90x. 64 KiB
  transfers are dominated by data movement rather than per-operation overhead, so
  there was less for this work to remove.

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
in the tables above *are* reproducible — they come from the committed harness.
The raw probe output, with every repeat and its provenance, is preserved outside
the repository in the workflow artifacts for this change.

The consequence, accepted knowingly: **nothing committed would catch a
regression in the park path.** The comparison harness would notice a large one,
but it cannot separate park cost from ring cost, so a small regression would
disappear into the difference between backends.

### A correction worth recording

An earlier revision of this document reported sequential read at depth 1 as
1.03x. Re-running the *unchanged* crate on the current host gives **2.04x** for
that same cell. The gap is not caused by any change since — it is present in the
code those figures describe.

So the old absolute numbers reflected a machine, or a machine state, that no
longer exists, and the document presented them with more confidence than a single
run supports. Treat every absolute figure here as one host on one day; the
relative ordering is the part that travels, and even that only within the
scenario it was measured in.

### An earlier correction, still worth recording

An earlier revision of the *harness* reported registration winning sequential
reads at depth 1 by ~16%. That was an artifact: the two owned-buffer backends
were allocating and zeroing a fresh buffer per operation while the registered one
reused pre-registered buffers, so the comparison included an `alloc_zeroed` on one
side and nothing on the other. Every backend now draws from a pool allocated once
at construction, and the apparent advantage disappeared.

It is recorded because it is the exact failure mode this harness exists to
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
- **Why the harness reports roughly 4 µs per operation more than a direct probe
  measured for the same shape of work at depth 64.** Unexplained; recorded in
  [pending-work.md](pending-work.md).

The honest summary is that this crate's remaining costs are visible and its
benefits are still largely unexercised by this measurement. Finding a workload
where completion-based I/O wins on Windows for reasons other than avoided
overhead — and reporting it with the same rigour — is the obvious next piece of
work.

## Reproducing

```powershell
cargo run -p win-ioring-bench --release
cargo run -p win-ioring-bench --release -- --clean
```

Parameters live in `crates/win-ioring-bench/src/config.rs` and are printed with
every run, because a figure without its parameters is not a result.
