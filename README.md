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
ring itself never does.

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

## Cancellation, shutdown, and the leak policy

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

Await `Driver::drive` to know when shutdown has finished; `Handle::shutdown_complete`
does the same for code holding a handle but not the driver.

Registration follows the same logic. A registered buffer cannot be withdrawn —
the platform offers no unregister call — so registering is a permanent transfer
of ownership for the life of the ring, and registering again supersedes the old
set without returning it.

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
