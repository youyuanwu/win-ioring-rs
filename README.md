# win-ioring

Safe, runtime-agnostic Rust bindings for the Windows
[IoRing](https://learn.microsoft.com/en-us/windows/win32/api/ioringapi/) API.

IoRing is Windows' completion-based I/O interface, comparable to Linux's
io_uring: you build operations into a submission queue, hand them to the kernel,
and collect results from a completion queue. This crate wraps it twice — an
unsafe layer that mirrors the platform one-for-one, and a safe layer that tracks
buffer and handle lifetimes for you.

## Platform requirement

**IoRing requires Windows 11 or Windows Server 2022 or later.**

This is a load-time requirement, not a runtime one. The platform entry points
are statically imported, so a binary linking this crate will fail to load at all
on an older host — there is no opportunity to report a friendly error. If you
need to support older Windows, load a separate module dynamically.

Because of that, this crate has no error meaning "this host has no IoRing". It
reports only the shortfalls a host *with* IoRing can still have, each as its own
variant you can match on: `Error::UnsupportedVersion` for a ring version below
what you asked for, `Error::UnsupportedFeature` for a missing feature flag, and
`Error::UnsupportedOp` for an operation the host does not implement.

## Runtime agnostic

The crate depends on no async runtime, and takes no ambient state. You create a
ring, wrap it in a `Driver`, and spawn that driver on whatever executor you
already have. Everything else goes through the `Handle` the driver gives you.

The driver and its handle are single-threaded by design: they share state
through `Rc` and are `!Send`, which the compiler enforces. Completions are
signalled from an OS thread pool thread, so the *waker* crosses threads, but the
ring itself never does — and that is the only thread boundary in the crate.
Queuing work does not cross one: it raises a flag on the driver and wakes the
driver's own waker, on your thread.

```rust,no_run
use win_ioring::file::File;
use win_ioring::io_ring::IoRing;
use win_ioring::runtime::Driver;

async fn example() -> Result<(), Box<dyn std::error::Error>> {
    let ring = IoRing::builder().build()?;
    let driver = Driver::new(ring)?;
    let handle = driver.handle();

    // Spawn the driver on your executor. This is the only integration point.
    let driving = tokio::task::spawn_local(async move { driver.drive().await });

    let file = File::open("data.bin")?;
    let (bytes_read, buffer) = file
        .read_at(&handle, vec![0_u8; 4096], 4096, 0)
        .await
        .unwrap();
    println!("read {bytes_read} bytes: {:?}", &buffer[..bytes_read as usize]);

    handle.shutdown();
    driving.await?;
    Ok(())
}
```

The same code runs unchanged on Tokio's `LocalSet` and on a hand-written
executor. The test suite asserts that by running one scenario under both and
comparing the recorded output line for line.

One caveat worth knowing before you scale it up: `File::open` and `File::create`
produce **synchronous** handles, because `std` does not pass
`FILE_FLAG_OVERLAPPED`. Windows then serialises I/O at the file object, so
submitting many operations against one such handle yields a depth of one however
many the ring holds. This is invisible against a warm cache and decisive once
reads reach the device. To avoid it, open the file yourself with that flag and
adopt it with `File::from_std` — the rustdoc on `File::open` shows how.

## Buffers are owned, not borrowed

An operation can outlive the future awaiting it, because the kernel keeps
working whether or not anyone is still listening. A borrowed buffer would
therefore be unsound: dropping the future would free memory the kernel is about
to write into.

So operations *take* their buffer and give it back on completion, in a
`BufResult` that carries the buffer whether the operation succeeded or failed.
Any type implementing `IoBuf`/`IoBufMut` works; `Vec<u8>`, `Box<[u8]>` and
`[u8; N]` are implemented for you. Those traits are `unsafe` to implement,
because they promise the kernel a stable address and an honest initialized
length that the compiler cannot check for you.

## Cancellation and shutdown

**Dropping a future is always safe and never blocks.** It detaches and, where the
platform can act on one, requests cancellation. The buffer and the file handle
stay owned by the driver until the kernel reports the operation finished, and
are released then. You will not get the buffer back, because nobody is waiting
for it.

Cancellation is a *request*, and a best-effort one. An operation may well
complete normally after being cancelled, and an operation the kernel has not yet
seen is cancelled when it reaches the kernel rather than at drop time. The crate
does not pretend otherwise.

`Handle::shutdown` asks the driver to stop accepting new work and to let
operations already in flight finish. `Handle::shutdown_now` asks it to cancel
them instead. Either way the driver drains until every operation has reported,
and only then closes the ring and releases what it was holding. **Shutdown never
abandons memory**: every buffer comes back, every handle is closed, every
registration is freed.

Draining is unbounded, because the alternative is worse. Closing the ring does
not cancel in-flight operations and does not wait for them — the platform
documents that memory may still be written afterwards — so giving up early would
mean freeing memory the kernel is still using. The trade is that a shutdown
blocked on an operation that neither completes nor responds to cancellation will
not finish. That case is reported, throttled, through the error observer, and a
graceful shutdown can be escalated with `Handle::shutdown_now` while it drains.

**To know when shutdown has finished, await `Driver::drive`.** That is the
recommended path and the one to reach for by default. `Handle::shutdown_complete`
exists for code that holds a handle but not the driver; it resolves on the same
event but cannot itself make progress, so awaiting it on a thread where nothing
is driving the ring waits forever. Having two ways to await the same thing with
different failure modes is the sharpest edge in this API — prefer the first
unless you cannot reach the driver.

Registration lends buffers rather than taking them. The *mapping* is permanent —
the platform offers no unregister call, so it lasts for the life of the ring, and
registering again supersedes the old set without releasing it. But the buffers
stay reachable: a successful registration yields a collection you check buffers
out of, and the handle you get back dereferences to the bytes, goes into an
operation by value, and comes back with the result. Exactly one handle to a
buffer exists at a time, so an operation in flight and your code can never touch
the same bytes — enforced by the compiler, not by documentation.

```rust,ignore
let buffers = handle.register_buffers(vec![vec![0_u8; 64 * 1024]]).await.unwrap();
let buffer = buffers.check_out(0)?;

let (result, buffer) = handle
    .read_registered(FileTarget::Owned(&file), buffer, 0, 4096, 0)
    .await
    .into_parts();

println!("{:?}", &buffer[..result?  as usize]);
```

## Is it faster?

Measured, in [docs/performance.md](docs/performance.md), and the short answer is
**it depends on whether the reads reach the device** — which turned out to matter
far more than concurrency, buffer registration, or anything else measured here.

**Against a warm page cache, only at low concurrency.** At one operation in
flight this crate
is ahead of `tokio::fs` on random reads at 0.57x — the one depth-1 result that
resolves; its sequential-read and write-then-read leads (0.87x, 0.91x) sit inside
the noise band and claim nothing. At
eight and sixty-four operations in flight `tokio::fs` is ahead on every point
estimate, by 1.08x to
1.44x with owned buffers and 1.03x to 1.60x with registered ones, though only five
of those fourteen comparisons resolve statistically. That document is careful
about which is which.

**One candidate explanation has been measured and eliminated.** This crate now
opens files with `FILE_FLAG_OVERLAPPED`, following `compio`. The change was
predicted to narrow the warm-cache gap above, and a pre-registered A/B across two
independent five-run sets found **no effect of the predicted size** — the
mechanism argument the performance document had been making on reasoning alone is
now confirmed by measurement, and a candidate cause is off the list.

**Unbuffered, the ranking changes and the design pays.** A cached `ReadFile`
never blocks, so there is nothing for a completion-based interface to overlap and
its per-operation machinery buys nothing. Reads issued with
`FILE_FLAG_NO_BUFFERING` pay real device latency, and there a separate opt-in arm
measures this crate at **7.84x faster than `tokio::fs`** at blocking-pool width 1
on random reads with sixty-four operations in flight — the depth at which the
warm-cache table has it losing.

**That is not a claim to be the fastest, and the second number is not optional.**
Against the honest competitor — `tokio::fs` at pool width 512 spread across
sixty-four file handles, which also achieves deep outstanding I/O once latency is
real — this crate **loses by 1.22x**. The advantage it does have is *resource
cost*: it comes within 1.22x of the thread pool using one file handle, one
thread, and no thread per outstanding operation. Neither figure means anything
without the other, and neither is comparable to the warm-cache numbers above.

That arm also corrected a framing this document had used for some time: for the
thread pool, **file handle count was the variable, not pool width**. Adding 511
threads to a one-handle configuration does nothing measurable; adding 63 handles
at a fixed pool width moves the result nine to ten times. One handle is one
queue, however many threads push on it.

**And the loss is not the I/O ring's fault.** The comparison now runs five
backends, the fifth being [`compio`](https://github.com/compio-rs/compio) — which
is completion-based on Windows but reaches the kernel through I/O completion
ports, not through a ring. It loses to `tokio::fs` in the same cells, where the
comparison resolves, by margins matching this crate's own, and of the twenty
comparisons between it and this crate, all fourteen at
depth 8 and depth 64 are statistically unresolved. It also wins where this crate
wins — it beats `tokio::fs` at depth 1 on both read scenarios, and is the
fastest backend in the matrix at depth 1 on random read. Two implementations sharing no
code and no kernel interface land inside each other's noise wherever the loss
happens, so whatever causes it is not this crate's ring code and not the ring
API. That document says what it does and does not narrow. The unbuffered result
above is consistent with this: the loss belongs to the *measurement condition* —
a workload with no waiting to overlap — rather than to any of the three
implementations.

Run it yourself with `cargo bench -p win-ioring-bench` — about five minutes; one
piece of application logic runs against every backend and any run that did not do
identical work is rejected rather than reported. The unbuffered arm is opt-in and
kept out of that run, because its figures need their own noise band and are far
more host-dependent:

```text
cargo bench -p win-ioring-bench --bench unbuffered
```

Every figure comes with a
confidence interval, and `-- --save-baseline` and `-- --baseline` compare two
runs. Treat the absolute figures as one host on one day — that document explains
why, at some length, and explains which of the numbers above it declines to claim
as an improvement.

## The unsafe layer

`win_ioring::io_ring` mirrors all fourteen platform entry points directly. It
does no lifetime tracking; every method that hands work to the kernel is
`unsafe` and documents what you must guarantee. Reach for it when you want the
platform's semantics exactly, and take on the obligations yourself.

## Contributing

[`docs/`](docs/) covers the architecture and the reasoning behind it, the
platform behaviours this crate was built around — several of which are
undocumented and unintuitive — the verification approach, and the list of known
limitations and deferred work.

## License

This project is licensed under the MIT license.
