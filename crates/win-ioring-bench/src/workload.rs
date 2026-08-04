//! Preparing the files and the cache the measurement depends on.
//!
//! All measurement here is against a **warm** operating-system page cache. The
//! backends differ in per-operation software overhead — syscalls, thread-pool
//! hops, submission batching — and device I/O would swamp that difference in
//! noise. That choice is stated in the report, because warm-cache figures read
//! as device throughput would be badly misleading.

use std::io;
use std::path::{Path, PathBuf};

/// Where the working files live.
///
/// Under `target/`, which the repository already ignores, so a 256 MiB file
/// never becomes a candidate for accidental commit. They persist between runs so
/// repeated invocations are cheap; deleting `target/bench-data` removes them.
///
/// There is deliberately no `clean` function here. The one entry point is
/// Criterion's, and Criterion owns its argument surface; a `--clean` this crate
/// could not offer through that surface would be a public function nothing
/// calls, which is the second path FR-014 exists to reject.
pub fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/bench-data")
        .canonicalize()
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/bench-data")
        })
}

/// Creates the working file if it is missing or the wrong size.
///
/// Recreating rather than trusting what is there, because a file left over from
/// a different configuration would silently change what is measured.
pub fn ensure_file(path: &Path, bytes: u64) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Ok(meta) = std::fs::metadata(path)
        && meta.len() == bytes
    {
        return Ok(());
    }
    let mut content = vec![0_u8; 1024 * 1024];
    for (i, b) in content.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    let file = std::fs::File::create(path)?;
    let mut written = 0_u64;
    {
        use std::io::Write;
        let mut w = std::io::BufWriter::new(&file);
        while written < bytes {
            let take = ((bytes - written) as usize).min(content.len());
            w.write_all(&content[..take])?;
            written += take as u64;
        }
        w.flush()?;
    }
    file.sync_all()
}

/// Reads a file through once, so the pages are resident before measuring.
pub fn warm(path: &Path) -> io::Result<u64> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0_u8; 1024 * 1024];
    let mut total = 0_u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
    }
    Ok(total)
}

/// How much physical memory the operating system reports as available.
pub fn available_memory_bytes() -> Option<u64> {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    let mut status = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: `status` is a correctly sized, correctly initialized structure
    // whose `dwLength` the call requires, and it outlives the call.
    unsafe { GlobalMemoryStatusEx(&mut status) }.ok()?;
    Some(status.ullTotalPhys)
}

/// Whether the working set plausibly fits the page cache.
///
/// The premise of every figure this harness produces. There is no direct way to
/// ask the operating system, so this compares the working set against a stated
/// fraction of total physical memory and says which it used.
pub fn cache_premise(working_set: u64) -> CachePremise {
    const FRACTION: u64 = 4; // a quarter of physical memory
    match available_memory_bytes() {
        Some(total) if working_set <= total / FRACTION => CachePremise::Holds { total },
        Some(total) => CachePremise::Doubtful { total },
        None => CachePremise::Unknown,
    }
}

/// Whether the warm-cache premise can be relied on.
pub enum CachePremise {
    /// The working set is within the stated fraction of physical memory.
    Holds { total: u64 },
    /// The working set is large enough that the premise may not hold.
    Doubtful { total: u64 },
    /// Physical memory could not be determined.
    Unknown,
}
