//! Verification scaffolding for the driver wake path — **temporary**.
//!
//! Not part of the comparison harness, and deliberately not a permanent
//! fixture. It exists to produce the before-and-after decomposition that
//! adjudicates the wake-path work, and it is deleted once those figures are
//! recorded. Its source stays recoverable from the commit that introduced it,
//! which the measurement artifacts cite by hash.
//!
//! It decomposes one 4 KiB warm-cache read into the layers that carry it:
//!
//! * `blocking seek_read` — the operating system read alone, no ring.
//! * `ring, no events` — the ring driven synchronously: build, submit-and-wait,
//!   pop. This is the floor the async path cannot beat, and the thing the
//!   asynchronous figure is stated relative to.
//! * `ring via Driver` — the crate's actual async path.
//!
//! Plus the standalone wait micro-measurements. Those touch only raw Win32, so
//! they are identical either side of the change and serve as a check that the
//! host behaved the same in both runs.
//!
//! Every repeat is printed rather than only the best, because the criteria are
//! decided on medians and spreads, and a summary that hides the spread cannot
//! settle whether a run was conclusive.

use std::ffi::c_void;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use win_ioring::file::File;
use win_ioring::io_ring::IoRing;
use win_ioring::io_ring::ops::ReadOp;
use win_ioring::runtime::Driver;
use windows::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::System::Threading::{
    CreateEventW, INFINITE, RegisterWaitForSingleObject, ResetEvent, SetEvent, UnregisterWaitEx,
    WT_EXECUTEONLYONCE,
};

const BLOCK: u32 = 4096;
const FILE_BYTES: u64 = 64 * 1024 * 1024;
const OPS: usize = 2048;
const CYCLES: usize = 2048;
const REPEATS: usize = 5;
const DEPTHS: [usize; 3] = [1, 8, 64];

/// A run's per-operation figures, plus what it actually delivered.
struct Sample {
    /// Microseconds per operation, one entry per repeat.
    per_op: Vec<f64>,
    /// Bytes the platform reported transferred, from the last repeat.
    bytes: u64,
    /// Order-insensitive digest of what reached application-visible memory.
    ///
    /// Folded commutatively across operations, so completion order — which is
    /// legitimately nondeterministic above one operation in flight — cannot
    /// enter it, while content and placement still do.
    digest: u64,
}

impl Sample {
    fn median(&self) -> f64 {
        let mut v = self.per_op.clone();
        v.sort_by(|a, b| a.partial_cmp(b).expect("no NaN in a timing"));
        v[v.len() / 2]
    }

    fn min(&self) -> f64 {
        self.per_op.iter().copied().fold(f64::INFINITY, f64::min)
    }

    fn max(&self) -> f64 {
        self.per_op
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max)
    }

    fn spread(&self) -> f64 {
        self.max() - self.min()
    }

    /// The inconclusiveness rule: a run whose spread exceeds a quarter of its
    /// own median is too noisy to adjudicate and must be repeated.
    fn inconclusive(&self) -> bool {
        self.spread() > self.median() / 4.0
    }
}

fn main() -> std::io::Result<()> {
    let path = data_file()?;
    warm(&path)?;

    provenance();

    let blocking = measure(REPEATS, || blocking_reads(&path, OPS), OPS)?;
    println!("## the operating system read alone");
    report("blocking seek_read", &blocking);
    println!();

    let mut inconclusive = blocking.inconclusive();
    for depth in DEPTHS {
        let sync = measure(REPEATS, || ring_sync(&path, depth, OPS), OPS)?;
        let driver = measure(REPEATS, || ring_driver(&path, depth, OPS), OPS)?;

        println!("## depth {depth} — {OPS} operations of {BLOCK} bytes");
        report("ring, no events", &sync);
        report("ring via Driver", &driver);
        println!(
            "{:<24} {:>8.3} us/op   <- median(async) - median(sync); the quantity SC-001 compares",
            "park overhead",
            driver.median() - sync.median()
        );
        println!(
            "{:<24} sync {} / async {}",
            "conclusive?",
            verdict(&sync),
            verdict(&driver)
        );
        println!(
            "{:<24} sync bytes {} digest {:#018x} / async bytes {} digest {:#018x}",
            "delivered", sync.bytes, sync.digest, driver.bytes, driver.digest
        );
        inconclusive |= sync.inconclusive() || driver.inconclusive();
        println!();
    }

    println!("## park machinery, measured alone (raw Win32; identical either side of the change)");
    micro("RegisterWait+signal+Unreg", || wait_signalled(CYCLES))?;
    micro("RegisterWait+Unreg(block)", || wait_cancelled(CYCLES))?;
    micro("persistent: signal->cb", || wait_persistent(CYCLES))?;
    micro("same thread: signal->wait", || wait_same_thread(CYCLES))?;
    micro("dedicated thread: ->wake", || wait_dedicated_thread(CYCLES))?;
    println!();

    if inconclusive {
        println!(
            "RUN IS INCONCLUSIVE: at least one configuration's spread exceeded a quarter of its \
             median. Repeat on a quieter machine; this run must not be used to adjudicate."
        );
    } else {
        println!(
            "Run is conclusive: every configuration's spread is within a quarter of its median."
        );
    }

    Ok(())
}

fn verdict(s: &Sample) -> &'static str {
    if s.inconclusive() { "NO" } else { "yes" }
}

fn provenance() {
    println!("# wake path probe");
    println!();
    println!("## parameters and host");
    println!("- block: {BLOCK} bytes");
    println!("- file: {} MiB", FILE_BYTES / (1024 * 1024));
    println!("- operations per repeat: {OPS}");
    println!("- repeats: {REPEATS}");
    println!("- depths: {DEPTHS:?}");
    println!("- micro-measurement cycles: {CYCLES}");
    println!(
        "- logical processors: {}",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0)
    );
    match win_ioring_bench::workload::available_memory_bytes() {
        Some(total) => println!("- physical memory: {} MiB", total / (1024 * 1024)),
        None => println!("- physical memory: could not be determined"),
    }
    println!("- offsets: xorshift64 over the file, same sequence every run");
    println!("- profile: release");
    println!();
}

fn report(name: &str, s: &Sample) {
    let repeats: Vec<String> = s.per_op.iter().map(|v| format!("{v:.3}")).collect();
    println!(
        "{name:<26} median {:>8.3}  min {:>8.3}  max {:>8.3}  spread {:>7.3} ({:>5.1}% of median)",
        s.median(),
        s.min(),
        s.max(),
        s.spread(),
        100.0 * s.spread() / s.median()
    );
    println!("{:<26}   repeats: [{}]", "", repeats.join(", "));
}

/// Runs a measurement `repeats` times, keeping every figure.
fn measure(
    repeats: usize,
    mut f: impl FnMut() -> std::io::Result<(u64, u64)>,
    ops: usize,
) -> std::io::Result<Sample> {
    let mut per_op = Vec::with_capacity(repeats);
    let mut bytes = 0;
    let mut digest = 0;
    for _ in 0..repeats {
        let started = Instant::now();
        let (b, d) = f()?;
        per_op.push(started.elapsed().as_secs_f64() * 1e6 / ops as f64);
        bytes = b;
        digest = d;
    }
    Ok(Sample {
        per_op,
        bytes,
        digest,
    })
}

fn micro(name: &str, mut f: impl FnMut() -> std::io::Result<()>) -> std::io::Result<()> {
    let mut per_op = Vec::with_capacity(REPEATS);
    for _ in 0..REPEATS {
        let started = Instant::now();
        f()?;
        per_op.push(started.elapsed().as_secs_f64() * 1e6 / CYCLES as f64);
    }
    report(
        name,
        &Sample {
            per_op,
            bytes: 0,
            digest: 0,
        },
    );
    Ok(())
}

fn data_file() -> std::io::Result<PathBuf> {
    let dir = PathBuf::from("target/probe-data");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("probe.bin");
    let ok = std::fs::metadata(&path)
        .map(|m| m.len() == FILE_BYTES)
        .unwrap_or(false);
    if !ok {
        let mut f = std::fs::File::create(&path)?;
        // Content varies with position, so a digest over what was read notices a
        // backend reading the wrong place, not merely the wrong amount.
        let mut chunk = vec![0u8; 1024 * 1024];
        let mut written = 0u64;
        while written < FILE_BYTES {
            let mib = (written / (1024 * 1024)) as u8;
            for (i, b) in chunk.iter_mut().enumerate() {
                *b = mib.wrapping_mul(31).wrapping_add(i as u8);
            }
            let take = ((FILE_BYTES - written) as usize).min(chunk.len());
            f.write_all(&chunk[..take])?;
            written += take as u64;
        }
        f.sync_all()?;
    }
    Ok(path)
}

/// Reads the file through once so every measurement sees a warm page cache.
fn warm(path: &Path) -> std::io::Result<()> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 1024 * 1024];
    while f.read(&mut buf)? > 0 {}
    Ok(())
}

/// Offsets in the same shape the random-read scenario uses.
fn offsets(n: usize) -> Vec<u64> {
    let blocks = FILE_BYTES / BLOCK as u64;
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    (0..n)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % blocks) * BLOCK as u64
        })
        .collect()
}

/// One operation's contribution to the digest.
///
/// Mixes the offset in, so reading the right number of bytes from the wrong
/// place is still caught, and is combined into the total with `wrapping_add` so
/// completion order cannot affect the result.
fn contribution(offset: u64, bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64 ^ offset.wrapping_mul(0x0000_0100_0000_01b3);
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The operating system read on its own, with no ring in the picture.
fn blocking_reads(path: &Path, ops: usize) -> std::io::Result<(u64, u64)> {
    use std::os::windows::fs::FileExt;
    let f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; BLOCK as usize];
    let mut total = 0u64;
    let mut digest = 0u64;
    for offset in offsets(ops) {
        let n = f.seek_read(&mut buf, offset)?;
        total += n as u64;
        digest = digest.wrapping_add(contribution(offset, &buf[..n]));
    }
    Ok((total, digest))
}

/// The ring driven synchronously: no events, no futures, no wakers.
///
/// `SubmitIoRing` submits and waits in one call, so a thread with nothing else
/// to do can drive the ring with no park machinery at all. This is the floor:
/// whatever the async path costs above this is what the async path costs.
fn ring_sync(path: &Path, depth: usize, ops: usize) -> std::io::Result<(u64, u64)> {
    let file = File::open(path)?;
    let handle = file.as_raw_handle();
    let queue = (depth * 4).max(128) as u32;
    let mut ring = IoRing::builder()
        .with_submission_queue_size(queue)
        .with_completion_queue_size(queue * 2)
        .build()
        .map_err(std::io::Error::other)?;

    let mut buffers: Vec<Vec<u8>> = (0..depth).map(|_| vec![0u8; BLOCK as usize]).collect();
    let all = offsets(ops);
    let mut total = 0u64;
    let mut digest = 0u64;

    for batch in all.chunks(depth) {
        for (i, offset) in batch.iter().enumerate() {
            let address = buffers[i].as_mut_ptr() as *mut c_void;
            let op = ReadOp::builder()
                .with_raw_handle(handle)
                .with_raw_data_address(address)
                .with_num_of_bytes_to_read(BLOCK)
                .with_offset(*offset)
                .with_user_data(i)
                .build()
                .map_err(std::io::Error::other)?;
            // SAFETY: `file` and `buffers` outlive this loop, and every entry
            // built here is popped before either is touched again.
            unsafe { ring.build_read_file(op) }.map_err(std::io::Error::other)?;
        }

        let mut remaining = batch.len();
        while remaining > 0 {
            ring.submit(remaining, 10_000)
                .map_err(std::io::Error::other)?;
            while let Some(cqe) = ring.pop_completion().map_err(std::io::Error::other)? {
                let i = cqe.UserData;
                let n = cqe.Information;
                total += n as u64;
                digest = digest.wrapping_add(contribution(batch[i], &buffers[i][..n]));
                remaining -= 1;
            }
        }
    }

    ring.close().map_err(std::io::Error::other)?;
    Ok((total, digest))
}

/// The crate's async path, driven as the comparison harness drives it.
fn ring_driver(path: &Path, depth: usize, ops: usize) -> std::io::Result<(u64, u64)> {
    use futures::stream::{FuturesUnordered, StreamExt};

    let queue = (depth * 4).max(128) as u32;
    let ring = IoRing::builder()
        .with_submission_queue_size(queue)
        .with_completion_queue_size(queue * 2)
        .build()
        .map_err(std::io::Error::other)?;
    let driver = Driver::new(ring).map_err(std::io::Error::other)?;
    let handle = driver.handle();
    let file = File::open(path)?;
    let all = offsets(ops);

    let work = {
        let handle = handle.clone();
        let file = &file;
        async move {
            let mut total = 0u64;
            let mut digest = 0u64;
            let mut inflight = FuturesUnordered::new();
            let mut free: Vec<Vec<u8>> = (0..depth).map(|_| vec![0u8; BLOCK as usize]).collect();
            let mut next = 0usize;

            loop {
                while inflight.len() < depth && next < all.len() {
                    let buffer = free.pop().expect("a buffer per outstanding operation");
                    let offset = all[next];
                    next += 1;
                    let h = handle.clone();
                    inflight.push(async move {
                        let out = h.read(file, buffer, BLOCK, offset).await;
                        (offset, out)
                    });
                }
                match inflight.next().await {
                    Some((offset, result)) => {
                        let (result, buffer) = result.into_parts();
                        let n = result.map_err(std::io::Error::other)? as usize;
                        total += n as u64;
                        digest = digest.wrapping_add(contribution(offset, &buffer[..n]));
                        free.push(buffer);
                    }
                    None => break,
                }
            }
            Ok::<(u64, u64), std::io::Error>((total, digest))
        }
    };

    let outcome = {
        let driving = driver.drive();
        let work = async {
            let outcome = work.await;
            handle.shutdown_now();
            outcome
        };
        futures::executor::block_on(async {
            let (_, outcome) = futures::future::join(driving, work).await;
            outcome
        })?
    };
    drop(driver);
    Ok(outcome)
}

struct Flag(AtomicBool);

/// `UnregisterWaitEx` reports `ERROR_IO_PENDING` when it has not waited out the
/// callbacks, which is success for the non-blocking form rather than a failure.
fn ignore_io_pending(r: windows::core::Result<()>) -> std::io::Result<()> {
    const ERROR_IO_PENDING: i32 = -2_147_023_899;
    match r {
        Ok(()) => Ok(()),
        Err(e) if e.code().0 == ERROR_IO_PENDING => Ok(()),
        Err(e) => Err(std::io::Error::other(e)),
    }
}

/// The winning branch of the driver's select: a wait registered, fired, then
/// unregistered by the non-blocking path.
fn wait_signalled(cycles: usize) -> std::io::Result<()> {
    // SAFETY: a manual-reset, initially unsignalled event; closed below.
    let event = unsafe { CreateEventW(None, true, false, None) }?;
    for _ in 0..cycles {
        let flag = Arc::new(Flag(AtomicBool::new(false)));
        let raw = Arc::into_raw(Arc::clone(&flag));
        let mut wait = HANDLE::default();
        // SAFETY: `wait` is a local the call fills in, `event` outlives this
        // iteration, and `raw` is a reference count the callback reclaims.
        unsafe {
            RegisterWaitForSingleObject(
                &mut wait,
                event,
                Some(callback),
                Some(raw as *const c_void),
                INFINITE,
                WT_EXECUTEONLYONCE,
            )
        }?;
        // SAFETY: `event` is open for the whole loop.
        unsafe { SetEvent(event) }?;
        while !flag.0.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }
        // SAFETY: the registration above has not been unregistered yet.
        ignore_io_pending(unsafe { UnregisterWaitEx(wait, None) })?;
        // SAFETY: `event` is open for the whole loop.
        unsafe { ResetEvent(event) }?;
    }
    // SAFETY: `event` is open and owned here, and closed exactly once.
    unsafe { windows::Win32::Foundation::CloseHandle(event) }?;
    Ok(())
}

/// The losing branch: registered, never fires, and must be unregistered by the
/// blocking path that waits out any in-flight callback.
fn wait_cancelled(cycles: usize) -> std::io::Result<()> {
    // SAFETY: a manual-reset, initially unsignalled event; closed below.
    let event = unsafe { CreateEventW(None, true, false, None) }?;
    for _ in 0..cycles {
        let flag = Arc::new(Flag(AtomicBool::new(false)));
        let raw = Arc::into_raw(Arc::clone(&flag));
        let mut wait = HANDLE::default();
        // SAFETY: as above.
        unsafe {
            RegisterWaitForSingleObject(
                &mut wait,
                event,
                Some(callback),
                Some(raw as *const c_void),
                INFINITE,
                WT_EXECUTEONLYONCE,
            )
        }?;
        // SAFETY: the registration above has not been unregistered yet, and
        // `INVALID_HANDLE_VALUE` asks to wait out any running callback.
        unsafe { UnregisterWaitEx(wait, Some(INVALID_HANDLE_VALUE)) }?;
        // The callback never ran, so its reference count comes back here.
        // SAFETY: the blocking unregister above guarantees it never will.
        unsafe { drop(Arc::from_raw(raw)) };
        drop(flag);
    }
    // SAFETY: `event` is open and owned here, and closed exactly once.
    unsafe { windows::Win32::Foundation::CloseHandle(event) }?;
    Ok(())
}

/// What the driver could cost instead: one wait registered once and left armed,
/// so a completion costs a signal and a callback and no registration at all.
fn wait_persistent(cycles: usize) -> std::io::Result<()> {
    // SAFETY: an auto-reset, initially unsignalled event; closed below.
    let event = unsafe { CreateEventW(None, false, false, None) }?;
    let flag = Arc::new(Flag(AtomicBool::new(false)));
    let raw = Arc::into_raw(Arc::clone(&flag));
    let mut wait = HANDLE::default();
    // SAFETY: `wait` is a local the call fills in; `event` and the leaked count
    // both outlive the registration, which is unregistered below before either
    // is released.
    unsafe {
        RegisterWaitForSingleObject(
            &mut wait,
            event,
            Some(callback_borrowing),
            Some(raw as *const c_void),
            INFINITE,
            windows::Win32::System::Threading::WORKER_THREAD_FLAGS(0),
        )
    }?;

    for _ in 0..cycles {
        flag.0.store(false, Ordering::Release);
        // SAFETY: `event` is open until after the unregister below.
        unsafe { SetEvent(event) }?;
        while !flag.0.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }
    }

    // SAFETY: the registration is live and this waits out every callback, after
    // which the leaked reference count is ours to reclaim.
    unsafe { UnregisterWaitEx(wait, Some(INVALID_HANDLE_VALUE)) }?;
    // SAFETY: no callback can still be running, per the blocking unregister.
    unsafe { drop(Arc::from_raw(raw)) };
    // SAFETY: `event` is open and owned here, and closed exactly once.
    unsafe { windows::Win32::Foundation::CloseHandle(event) }?;
    Ok(())
}

/// The floor for reacting to an event at all: signal it and wait for it on the
/// same thread, with no thread pool and no hand-off anywhere.
fn wait_same_thread(cycles: usize) -> std::io::Result<()> {
    use windows::Win32::System::Threading::WaitForSingleObject;
    // SAFETY: an auto-reset, initially unsignalled event; closed below.
    let event = unsafe { CreateEventW(None, false, false, None) }?;
    for _ in 0..cycles {
        // SAFETY: `event` is open for the whole loop.
        unsafe { SetEvent(event) }?;
        // SAFETY: as above; already signalled, so this returns at once.
        unsafe { WaitForSingleObject(event, INFINITE) };
    }
    // SAFETY: `event` is open and owned here, and closed exactly once.
    unsafe { windows::Win32::Foundation::CloseHandle(event) }?;
    Ok(())
}

/// The other way to turn a signalled event into a wake: one thread of our own
/// blocked on it, rather than the operating system thread pool's dispatcher.
fn wait_dedicated_thread(cycles: usize) -> std::io::Result<()> {
    use windows::Win32::System::Threading::WaitForSingleObject;

    // SAFETY: an auto-reset, initially unsignalled event; closed below.
    let event = unsafe { CreateEventW(None, false, false, None) }?;
    // SAFETY: an auto-reset, initially unsignalled event; closed below.
    let done = unsafe { CreateEventW(None, false, false, None) }?;
    let flag = Arc::new(Flag(AtomicBool::new(false)));

    let raw_event = event.0 as usize;
    let raw_done = done.0 as usize;
    let waiter_flag = Arc::clone(&flag);
    let waiter = std::thread::spawn(move || {
        let event = HANDLE(raw_event as *mut c_void);
        let done = HANDLE(raw_done as *mut c_void);
        loop {
            // SAFETY: both handles stay open until this thread is joined.
            unsafe { WaitForSingleObject(event, INFINITE) };
            // SAFETY: as above. A signalled `done` is the shutdown request.
            if unsafe { WaitForSingleObject(done, 0) } == windows::Win32::Foundation::WAIT_OBJECT_0
            {
                return;
            }
            waiter_flag.0.store(true, Ordering::Release);
        }
    });

    for _ in 0..cycles {
        flag.0.store(false, Ordering::Release);
        // SAFETY: `event` stays open until the waiter is joined.
        unsafe { SetEvent(event) }?;
        while !flag.0.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }
    }

    // SAFETY: both handles are open; this asks the waiter to return.
    unsafe { SetEvent(done) }?;
    // SAFETY: as above.
    unsafe { SetEvent(event) }?;
    waiter.join().expect("the waiter thread does not panic");
    // SAFETY: both handles are open, owned here, and closed exactly once.
    unsafe {
        windows::Win32::Foundation::CloseHandle(event)?;
        windows::Win32::Foundation::CloseHandle(done)?;
    }
    Ok(())
}

/// # Safety
///
/// `context` must be the pointer produced by `Arc::into_raw` for a `Flag`, and
/// this must be called at most once for it.
unsafe extern "system" fn callback(context: *mut c_void, _fired: bool) {
    // SAFETY: the caller guarantees `context` is that pointer, called once.
    let flag = unsafe { Arc::from_raw(context as *const Flag) };
    flag.0.store(true, Ordering::Release);
}

/// # Safety
///
/// `context` must point to a live `Flag` that outlives the registration. Unlike
/// [`callback`] this borrows rather than consuming, so it may run many times.
unsafe extern "system" fn callback_borrowing(context: *mut c_void, _fired: bool) {
    // SAFETY: the caller guarantees `context` points to a live `Flag`.
    let flag = unsafe { &*(context as *const Flag) };
    flag.0.store(true, Ordering::Release);
}
