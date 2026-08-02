# Performance

What this crate costs against the alternative, measured rather than assumed.

`cargo run -p win-ioring-bench --release` produces the table. `--clean` removes
the working files it leaves under `target/bench-data`.

## The headline

**On this measurement, `tokio::fs` is faster than this crate almost everywhere.**
The IoRing backend is within a few percent at one operation in flight and 1.2× to
2.2× slower as concurrency rises. Registration does not pay for itself: it is a
consistent few percent behind even the owned-buffer path.

That is not the result the crate was built expecting, and it is stated first
because a benchmark that only gets quoted when it flatters its author is worth
nothing.

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

Taken on a 16-logical-processor Windows host, 28 GiB memory, working files on an
NVMe volume. Times in microseconds; `relative` is against the first row.

```
## sequential read — depth 1, 4096 operations
tokio::fs (blocking pool 1)          394051.4      1.00x
tokio::fs (blocking pool 512)        386620.5      0.98x
win-ioring (owned buffers)           404686.0      1.03x
win-ioring (registered)              433112.3      1.10x

## sequential read — depth 64, 4096 operations
tokio::fs (blocking pool 1)          256606.1      1.00x
tokio::fs (blocking pool 512)        266071.6      1.04x
win-ioring (owned buffers)           326977.3      1.27x
win-ioring (registered)              329994.8      1.29x

## random read — depth 64, 1024 operations
tokio::fs (blocking pool 1)            4927.6      1.00x
tokio::fs (blocking pool 512)         10447.8      2.12x
win-ioring (owned buffers)             9268.4      1.88x
win-ioring (registered)               10787.3      2.19x
```

## What to take from it

- **At depth 1 the backends are within a few percent of each other.** There is
  nothing for a submission ring to batch, so this is the expected shape.
- **Concurrency does not favour this crate on this workload.** The advantage
  completion-based I/O is supposed to earn — coalescing outstanding operations
  into one submission — does not show up against a warm cache, where each
  operation is cheap enough that the driver's own bookkeeping is a visible share
  of the cost.
- **A narrow blocking pool beats a wide one for small reads.** `tokio::fs` at one
  blocking thread is roughly twice as fast as at 512 on random reads at depth 64.
  That is contention, not I/O.
- **Registration does not pay for itself here.** It is a consistent few percent
  *behind* the owned-buffer path. Its own cost is excluded from these figures
  (see below), so this is the per-operation comparison alone.

### A correction worth recording

An earlier revision of this harness reported registration *winning* sequential
reads at depth 1 by ~16%. That was an artifact: the two owned-buffer backends
were allocating and zeroing a fresh buffer per operation while the registered one
reused pre-registered buffers, so the comparison included an `alloc_zeroed` on
one side and nothing on the other. Every backend now draws from a pool allocated
once at construction, and the apparent advantage disappeared.

It is recorded here because it is the exact failure mode this harness exists to
prevent — a difference in the benchmark being read as a difference in the
backend — and because it was caught by review rather than by any of the automated
checks.

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
  so these figures are per-operation cost only. A registration is a one-off whose
  cost belongs to the decision to register rather than to any single transfer;
  measuring it is separate work.

The honest summary is that this crate's design has costs that are visible and
benefits that this measurement does not exercise. Finding a workload where
completion-based I/O wins on Windows — and reporting it with the same rigour —
is the obvious next piece of work.

## Reproducing

```powershell
cargo run -p win-ioring-bench --release
cargo run -p win-ioring-bench --release -- --clean
```

Parameters live in `crates/win-ioring-bench/src/config.rs` and are printed with
every run, because a figure without its parameters is not a result.
