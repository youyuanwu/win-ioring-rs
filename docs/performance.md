# Performance

What this crate costs against the alternative, measured rather than assumed.

`cargo run -p win-ioring-bench --release` produces the table. `--clean` removes
the working files it leaves under `target/bench-data`.

## The headline

**On this measurement, `tokio::fs` is faster than this crate almost everywhere.**
The IoRing backend is at parity at one operation in flight and 1.1× to 2.7×
slower as concurrency rises. Registration does not reliably help, and sometimes
hurts.

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
tokio::fs (blocking pool 1)          494393.5      1.00x
tokio::fs (blocking pool 512)        534522.4      1.08x
win-ioring (owned buffers)           537898.5      1.09x
win-ioring (registered)              455288.1      0.92x

## sequential read — depth 8, 4096 operations
tokio::fs (blocking pool 1)          274005.8      1.00x
tokio::fs (blocking pool 512)        315051.3      1.15x
win-ioring (owned buffers)           397888.7      1.45x
win-ioring (registered)              385506.7      1.41x

## random read — depth 8, 1024 operations
tokio::fs (blocking pool 1)            6711.0      1.00x
tokio::fs (blocking pool 512)         20877.3      3.11x
win-ioring (owned buffers)            15173.3      2.26x
win-ioring (registered)               15147.6      2.26x
```

## What to take from it

- **At depth 1 the backends are within noise of each other.** There is nothing
  for a submission ring to batch, so this is the expected shape.
- **Concurrency does not favour this crate on this workload.** The advantage
  completion-based I/O is supposed to earn — coalescing outstanding operations
  into one submission — does not show up against a warm cache, where each
  operation is cheap enough that the driver's own bookkeeping is a visible share
  of the cost.
- **A narrow blocking pool beats a wide one for small reads.** `tokio::fs` at
  one blocking thread is 3× faster than at 512 on random reads, which is
  contention, not I/O.
- **Registration is not a reliable win.** It helps sequential reads at depth 1
  and is a wash elsewhere. Its cost is a one-off per ring, so a long-lived
  registration amortises better than this harness's per-repeat one.

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
