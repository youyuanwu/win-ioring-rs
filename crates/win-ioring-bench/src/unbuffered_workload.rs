//! The working file for the unbuffered arm, and the guard that keeps it clean.
//!
//! # Why this module is separate from [`crate::workload`]
//!
//! [`crate::workload`] prepares files for the **warm-cache** matrix, and its
//! whole purpose is to put pages in the operating system's cache before
//! measuring: [`crate::workload::warm`] reads the file through once for exactly
//! that reason. This arm needs the opposite, and the two requirements are not
//! merely different — they are mutually destructive.
//!
//! # The confound
//!
//! A single buffered access **poisons a file for unbuffered I/O**, per file,
//! for the life of the process, with no recovery short of reopening it in a
//! fresh process. Measured on this host: depth-64 random 4 KiB unbuffered reads
//! run at about 11 µs/IO against a file created and only ever touched through
//! unbuffered handles, and at about 126 µs/IO against the same file after one
//! ordinary buffered read of it — even a 2 MiB one. Creating the file with a
//! `BufWriter`, as [`crate::workload::ensure_file`] does, poisons it the same
//! way.
//!
//! What makes this dangerous is not that it is a large effect. It is that the
//! poisoned number is **plausible**. About 126 µs/IO at depth 64 is
//! indistinguishable from a well-behaved measurement showing that queue depth
//! buys the ring nothing — which is precisely the conclusion this project's two
//! previous investigations reached, so it would have been believed. It would
//! have been published as a third refutation, and it would have been wrong.
//!
//! # Why the guard is type-level and not a comment
//!
//! A comment saying "do not warm this file" is worth nothing against a
//! confound that produces no error, no warning, and a believable number. So
//! [`UnbufferedFile`] does not hand out a [`std::path::Path`]. It hands out an
//! [`UnbufferedPath`], which the buffered helpers cannot accept, because they
//! take `&Path` and `UnbufferedPath` implements neither `AsRef<Path>` nor
//! `Deref<Target = Path>`. Reaching the buffered helpers requires calling
//! [`UnbufferedPath::as_raw_path`] by name, which cannot be done by accident.
//!
//! The assertion that this holds is a `compile_fail` doc-test on
//! [`UnbufferedPath`], following the convention this workspace already uses for
//! compile-time guarantees (see the crate documentation of `win_ioring`). CI
//! runs `cargo test --doc`, so the guard is checked on every push rather than
//! resting on review.

use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::FILE_FLAG_NO_BUFFERING;

use crate::align::Alignment;
use crate::aligned::AlignedBuf;

/// The mode bit meaning "no intermediate buffering", as reported by
/// `NtQueryInformationFile`/`FileModeInformation`.
///
/// This is `FILE_NO_INTERMEDIATE_BUFFERING`. It is the read-back counterpart of
/// the `FILE_FLAG_NO_BUFFERING` passed at open time, and the two have different
/// values because they belong to different layers: the flag is a Win32 creation
/// flag, this is an NT file-mode bit.
pub const FILE_NO_INTERMEDIATE_BUFFERING: u32 = 0x0000_0008;

/// The mode bit meaning the handle serialises I/O at the file object.
///
/// This is `FILE_SYNCHRONOUS_IO_NONALERT`. A handle carrying it cannot have
/// more than one operation outstanding regardless of how many the caller
/// submits, which is the mechanism behind the finding recorded in Phase 3.
pub const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x0000_0020;

/// A path that the buffered helpers deliberately cannot accept.
///
/// # The guarantee
///
/// This type implements neither `AsRef<Path>` nor `Deref<Target = Path>`, so it
/// cannot be passed to [`crate::workload::warm`], to
/// [`crate::workload::ensure_file`], or to any of the many standard-library
/// entry points that are generic over `AsRef<Path>`. Buffered access destroys a
/// file's usefulness for unbuffered measurement — see the module documentation —
/// and the destruction is silent, so it is prevented by the type system rather
/// than by discipline.
///
/// Passing one to `warm` does not compile:
///
/// ```compile_fail
/// use win_ioring_bench::unbuffered_workload::UnbufferedFile;
/// use win_ioring_bench::align::Alignment;
///
/// let dir = std::env::temp_dir();
/// let alignment = Alignment::query(&dir).unwrap();
/// let file = UnbufferedFile::create(&dir, 1 << 20, &alignment).unwrap();
/// // `warm` takes `&Path`; `UnbufferedPath` is not one.
/// win_ioring_bench::workload::warm(file.path()).unwrap();
/// ```
///
/// The twin below is the **same snippet**, differing only in the one offending
/// expression, and it compiles. That is what makes the failure above
/// attributable: without a twin sharing the setup, a typo in `create` or
/// `query` would also produce a `compile_fail` "pass" and the guard would
/// silently become a no-op that keeps showing green.
///
/// ```no_run
/// use win_ioring_bench::unbuffered_workload::UnbufferedFile;
/// use win_ioring_bench::align::Alignment;
///
/// let dir = std::env::temp_dir();
/// let alignment = Alignment::query(&dir).unwrap();
/// let file = UnbufferedFile::create(&dir, 1 << 20, &alignment).unwrap();
/// // The only change: the escape hatch, named explicitly.
/// win_ioring_bench::workload::warm(file.path().as_raw_path()).unwrap();
/// ```
///
/// # What would defeat this
///
/// Established by adding each impl and re-running the doc-test, **not** by
/// reasoning about it — an earlier revision of this comment reasoned about it
/// and got the answer wrong.
///
/// - `Deref<Target = Path>` defeats it. Deref coercion is implicit, so
///   `warm(file.path())` would start compiling with nothing written at the call
///   site.
/// - `AsRef<Path>` defeats it too, and this is the one that was misjudged. It
///   is true that `warm` itself would still need an explicit `.as_ref()`,
///   because `warm` takes a concrete `&Path`. But `warm` is not the only
///   poisoning route and not even the most likely one:
///   [`std::fs::File::open`], [`std::fs::read`], [`std::fs::write`] and
///   [`std::fs::OpenOptions::open`] are all generic over `P: AsRef<Path>`, so
///   `File::open(p)` would compile for `p: &UnbufferedPath` with no conversion
///   written anywhere. Every one of those is a buffered open.
///
/// So the rule is: **implement neither `Deref<Target = Path>` nor
/// `AsRef<Path>`** — nor `Borrow<Path>`, for the same reason.
///
/// That rule is asserted, not just stated. A buffered `File::open` on one does
/// not compile either:
///
/// ```compile_fail
/// use win_ioring_bench::unbuffered_workload::UnbufferedFile;
/// use win_ioring_bench::align::Alignment;
///
/// let dir = std::env::temp_dir();
/// let alignment = Alignment::query(&dir).unwrap();
/// let file = UnbufferedFile::create(&dir, 1 << 20, &alignment).unwrap();
/// // `File::open` is generic over `AsRef<Path>`; `UnbufferedPath` is not.
/// let _f = std::fs::File::open(file.path()).unwrap();
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnbufferedPath(PathBuf);

impl UnbufferedPath {
    /// Opens this path with `FILE_FLAG_NO_BUFFERING`, which is the only way the
    /// arm is meant to reach it.
    pub fn open_unbuffered(&self) -> io::Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_NO_BUFFERING.0)
            .open(&self.0)
    }

    /// The underlying path, for the few callers that legitimately need one.
    ///
    /// # This is the escape hatch, and it is named so it cannot be taken by
    /// accident
    ///
    /// Opening this path through any buffered API — including a plain
    /// [`std::fs::File::open`], a `BufWriter`, or
    /// [`crate::workload::warm`] — poisons the file for unbuffered I/O for the
    /// rest of the process's life and inflates every subsequent measurement
    /// against it by roughly an order of magnitude, without error. Legitimate
    /// uses are metadata queries, alignment queries and reporting, none of
    /// which open the file for data access.
    pub fn as_raw_path(&self) -> &Path {
        &self.0
    }
}

/// A working file that has only ever been touched through unbuffered handles.
///
/// Its only constructor is [`UnbufferedFile::create`], which writes the file
/// through a `FILE_FLAG_NO_BUFFERING` handle, and its only path accessor hands
/// out an [`UnbufferedPath`]. Between them there is no route by which the file
/// becomes buffered without someone naming
/// [`UnbufferedPath::as_raw_path`] explicitly.
#[derive(Debug, Clone)]
pub struct UnbufferedFile {
    path: UnbufferedPath,
    bytes: u64,
}

impl UnbufferedFile {
    /// Creates the working file, unconditionally rewriting it.
    ///
    /// `bytes` is rounded up to the volume's alignment granularity, because an
    /// unbuffered write of a partial sector is rejected outright. The rounded
    /// size is what [`UnbufferedFile::bytes`] reports, so callers computing
    /// offsets from it stay in range.
    ///
    /// Always recreates rather than trusting what is there: a file left from a
    /// different configuration would silently change what is measured, and a
    /// file whose bytes arrived through a buffered write is poisoned in a way no
    /// later inspection can detect. See [`ensure_unbuffered_file`].
    ///
    /// # Errors
    ///
    /// If `bytes` is zero. A zero-length working file would make every read in
    /// the arm return zero bytes immediately, which measures nothing but would
    /// still produce timings.
    pub fn create(dir: &Path, bytes: u64, alignment: &Alignment) -> io::Result<Self> {
        if bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a zero-length working file measures nothing, but would still \
                 produce timings",
            ));
        }
        let bytes = alignment.round_up(usize::try_from(bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("working-file size {bytes} does not fit in usize"),
            )
        })?)? as u64;
        let path = dir.join("unbuffered-read.dat");
        ensure_unbuffered_file(&path, bytes, alignment)?;
        Ok(Self {
            path: UnbufferedPath(path),
            bytes,
        })
    }

    /// The path, wrapped so the buffered helpers cannot take it.
    pub fn path(&self) -> &UnbufferedPath {
        &self.path
    }

    /// The file's size in bytes, already a multiple of the alignment
    /// granularity.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

/// Writes `bytes` of content to `path` through an unbuffered handle.
///
/// Mirrors the content pattern of [`crate::workload::ensure_file`] — byte `i`
/// is `(i % 251)` — so a read can assert it received the bytes it asked for
/// rather than merely the right *number* of bytes.
///
/// # Why not a `BufWriter`
///
/// [`crate::workload::ensure_file`] writes through a `BufWriter`, which is
/// correct and fast for the warm-cache arm and fatal here: buffered writes
/// during creation poison the file exactly as buffered reads do. See the module
/// documentation.
///
/// # Why this always rewrites
///
/// [`crate::workload::ensure_file`] skips the work when a file of the right
/// size is already present, and an earlier revision of this function copied
/// that. It was wrong twice over. A size match says nothing about *how* the
/// bytes got there, so a file written buffered by anything at all would be
/// accepted and measured; and because the skip fires on every run after the
/// first, the unbuffered self-check below — the only mechanical guard on the
/// creation path — would never execute again. A check that quietly stops
/// running is indistinguishable from one that was deleted.
///
/// Rewriting costs a few seconds of sequential unbuffered writing per run, once,
/// entirely outside the measured region. That is the cheapest part of this arm.
pub fn ensure_unbuffered_file(path: &Path, bytes: u64, alignment: &Alignment) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let g = alignment.granularity();
    if !alignment.is_aligned(bytes) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("size {bytes} is not a multiple of the {g}-byte alignment granularity"),
        ));
    }

    // A whole number of sectors, large enough that the write is not dominated
    // by per-call overhead.
    let chunk = g.max(1 << 20).next_multiple_of(g);
    let mut buf = AlignedBuf::new(chunk, g)?;
    {
        let dst = buf.spare();
        for (i, b) in dst.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
    }

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(FILE_FLAG_NO_BUFFERING.0)
        .open(path)?;

    // The file is poisoned by buffered *writes* during creation exactly as it
    // is by buffered reads, and the damage is invisible afterwards: the file
    // reads back correctly, with the right bytes, and only the timings are
    // wrong. No structural test downstream can detect it, so the property is
    // asserted here, at the one point where it is still checkable.
    if !is_unbuffered(&file)? {
        return Err(io::Error::other(
            "the creation handle is buffered, which poisons the working file \
             for unbuffered I/O for the life of the process and inflates every \
             subsequent measurement against it by roughly an order of magnitude \
             — silently, since the file still reads back correctly",
        ));
    }

    let mut written = 0_u64;
    while written < bytes {
        use std::io::Write;
        let take = (bytes - written).min(chunk as u64) as usize;
        // `take` is a multiple of `g` because both `bytes` and `chunk` are, so
        // this never presents a partial sector to an unbuffered handle.
        file.write_all(&buf.spare()[..take])?;
        written += take as u64;
    }
    file.sync_all()
}

/// Reads back the NT file-mode bits for an open handle.
///
/// # Why not `GetFileInformationByHandleEx`
///
/// It cannot report either bit. There is no documented Win32 call that answers
/// "is this handle unbuffered?", so this uses `NtQueryInformationFile` with
/// `FileModeInformation`, which answers it directly. Verified against handles
/// opened three ways: a plain [`std::fs::File::open`] reports `0x20`
/// (synchronous), one opened with `FILE_FLAG_OVERLAPPED` reports `0x00`, and
/// one opened with `FILE_FLAG_NO_BUFFERING | FILE_FLAG_OVERLAPPED` reports
/// `0x08`.
///
/// # Why this is not `#[cfg(test)]`
///
/// It reads as test-only, but `#[cfg(test)]` items are visible only to unit
/// tests compiled into the same crate. The tests that need this live in
/// `tests/unbuffered.rs` (a separate crate) and in the `unbuffered` bench
/// target's test mode, and **neither could see it**. A guard that cannot be
/// called from where the tests live is not a guard, so it is an ordinary
/// public function kept out of the measured region by never being called from
/// timed code — not by being compiled out.
pub fn file_mode(file: &std::fs::File) -> io::Result<u32> {
    use windows::Wdk::Storage::FileSystem::{FileModeInformation, NtQueryInformationFile};
    use windows::Win32::System::IO::IO_STATUS_BLOCK;

    let handle = HANDLE(file.as_raw_handle());
    let mut mode: u32 = 0;
    let mut iosb = IO_STATUS_BLOCK::default();

    // SAFETY: `handle` is a live file handle borrowed for the duration of the
    // call; `iosb` and `mode` are correctly sized, initialized locals that
    // outlive it; and the length passed matches `mode`'s size exactly, which is
    // what `FileModeInformation` expects.
    let status = unsafe {
        NtQueryInformationFile(
            handle,
            &mut iosb,
            (&raw mut mode).cast(),
            size_of::<u32>() as u32,
            FileModeInformation,
        )
    };
    // Anything other than STATUS_SUCCESS is treated as failure, deliberately
    // rather than using `status.is_err()`. `is_err()` is `status.0 < 0`, so it
    // is *false* for STATUS_PENDING (0x103) — and on a pending completion the
    // kernel writes into `iosb` and `mode` after this function returns, which
    // would be a use-after-scope, while the caller would meanwhile read
    // `mode == 0` as "neither unbuffered nor synchronous". That is a wrong
    // answer of exactly the shape this module exists to prevent. Phase 3 and 4
    // call this on overlapped handles by design, so the case is reachable even
    // though `FileModeInformation` does not pend in practice.
    if status.0 != 0 {
        return Err(io::Error::other(format!(
            "NtQueryInformationFile(FileModeInformation) returned {status:?}"
        )));
    }
    Ok(mode)
}

/// Whether a handle really was opened without intermediate buffering.
///
/// The arm's central premise. If this is false, every figure it produces is a
/// warm-cache figure wearing an unbuffered label.
pub fn is_unbuffered(file: &std::fs::File) -> io::Result<bool> {
    Ok(file_mode(file)? & FILE_NO_INTERMEDIATE_BUFFERING != 0)
}

/// Whether a handle serialises I/O at the file object.
///
/// A synchronous handle caps outstanding operations at one no matter what the
/// caller submits, so a depth sweep against one measures nothing about depth.
pub fn is_synchronous(file: &std::fs::File) -> io::Result<bool> {
    Ok(file_mode(file)? & FILE_SYNCHRONOUS_IO_NONALERT != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory unique to the calling test.
    ///
    /// Tests run concurrently in one process and [`UnbufferedFile::create`]
    /// always uses the same file name, so a shared directory means they race on
    /// one file: one test truncating to `2 * g` while another reads `16 * g`
    /// produced an intermittent failure at roughly 1 in 100 runs. A flaky gate
    /// in CI is worse than no gate, because it teaches people to re-run
    /// failures instead of reading them.
    ///
    /// It also matters here for a second reason. These tests deliberately open
    /// files buffered — that is the confound under test — and a buffered open
    /// poisons whatever file it touches for every other test in the process.
    /// Isolation keeps that contained.
    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join("win-ioring-bench-unbuffered-unit")
            .join(name);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_created_file_is_readable_unbuffered_and_reports_the_flag() {
        let d = dir("readable");
        let alignment = Alignment::query(&d).unwrap();
        let g = alignment.granularity();
        let file = UnbufferedFile::create(&d, (16 * g) as u64, &alignment).unwrap();

        let handle = file.path().open_unbuffered().unwrap();
        assert!(
            is_unbuffered(&handle).unwrap(),
            "the arm's read handle is not actually unbuffered, so every figure \
             it produces would be a warm-cache figure with an unbuffered label"
        );

        // Correctness, not timing: the bytes delivered must be the bytes
        // written. A byte count alone would also pass if the read landed
        // somewhere else entirely.
        use std::io::Read;
        let mut buf = AlignedBuf::new(g, g).unwrap();
        let mut handle = handle;
        let n = handle.read(buf.spare()).unwrap();
        assert_eq!(n, g);
        assert!(
            buf.spare()[..n]
                .iter()
                .enumerate()
                .all(|(i, &b)| b == (i % 251) as u8),
            "content pattern does not match what was written"
        );
    }

    #[test]
    fn the_size_is_rounded_up_to_a_whole_number_of_sectors() {
        let d = dir("rounding");
        let alignment = Alignment::query(&d).unwrap();
        let g = alignment.granularity() as u64;

        let file = UnbufferedFile::create(&d, g + 1, &alignment).unwrap();
        assert_eq!(
            file.bytes(),
            2 * g,
            "an unbuffered write of a partial sector is rejected outright, so \
             the size must be rounded up rather than truncated"
        );
        assert!(alignment.is_aligned(file.bytes()));
    }

    #[test]
    fn a_size_that_is_not_a_whole_number_of_sectors_is_refused() {
        let d = dir("partial");
        let alignment = Alignment::query(&d).unwrap();
        let g = alignment.granularity() as u64;

        // Bypassing `create`, which rounds, to reach the check directly.
        let err = ensure_unbuffered_file(&d.join("partial.dat"), g - 1, &alignment)
            .expect_err("a partial-sector size must be refused, not silently written");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn the_three_open_modes_are_distinguishable() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;

        // The evidence behind the mode table in Phase 3, asserted rather than
        // recorded as prose: if these bits ever stop meaning what the arm
        // assumes, this fails instead of the measurements quietly changing
        // meaning.
        //
        // This test opens a file *buffered* on purpose, which poisons it for
        // unbuffered I/O for the life of the process. It therefore uses its own
        // throwaway file, never the one any other test measures against.
        let d = dir("modes");
        let alignment = Alignment::query(&d).unwrap();
        let g = alignment.granularity() as u64;
        let path = d.join("throwaway-poisoned.dat");
        ensure_unbuffered_file(&path, 16 * g, &alignment).unwrap();

        // Synchronous: caps outstanding operations at one regardless of what
        // the caller submits. This is the defect Phase 3 documents.
        let plain = std::fs::File::open(&path).unwrap();
        assert_eq!(
            file_mode(&plain).unwrap() & FILE_SYNCHRONOUS_IO_NONALERT,
            FILE_SYNCHRONOUS_IO_NONALERT,
            "a plain File::open handle is expected to be synchronous"
        );
        assert!(!is_unbuffered(&plain).unwrap());

        // Overlapped, still buffered. Phase 4's "the ring's handle is really
        // overlapped" assertion rests on this row, so it is pinned here rather
        // than assumed.
        let overlapped = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OVERLAPPED.0)
            .open(&path)
            .unwrap();
        assert!(!is_synchronous(&overlapped).unwrap());
        assert!(!is_unbuffered(&overlapped).unwrap());

        // Unbuffered and overlapped: what the arm actually measures on.
        let unbuffered = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_NO_BUFFERING.0 | FILE_FLAG_OVERLAPPED.0)
            .open(&path)
            .unwrap();
        assert!(is_unbuffered(&unbuffered).unwrap());
        assert!(!is_synchronous(&unbuffered).unwrap());

        // The three modes must be mutually distinguishable, not merely each
        // individually plausible: if any two collapsed to the same bits, the
        // read-back could not tell the arm what it needs to know.
        let modes = [
            file_mode(&plain).unwrap(),
            file_mode(&overlapped).unwrap(),
            file_mode(&unbuffered).unwrap(),
        ];
        assert_ne!(modes[0], modes[1]);
        assert_ne!(modes[1], modes[2]);
        assert_ne!(modes[0], modes[2]);
    }

    #[test]
    fn a_zero_length_working_file_is_refused() {
        let d = dir("zero");
        let alignment = Alignment::query(&d).unwrap();
        let err = UnbufferedFile::create(&d, 0, &alignment)
            .expect_err("a zero-length working file measures nothing");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn an_existing_file_of_the_right_size_is_still_rewritten() {
        // The guard this protects: a size match says nothing about how the
        // bytes got there. If creation were skipped for an existing file, a
        // buffered-written file would be accepted and measured, and the
        // unbuffered self-check would never run again after the first run.
        let d = dir("rewrite");
        let alignment = Alignment::query(&d).unwrap();
        let g = alignment.granularity() as u64;
        let path = d.join("preexisting.dat");

        std::fs::write(&path, vec![0xEE_u8; (4 * g) as usize]).unwrap();
        ensure_unbuffered_file(&path, 4 * g, &alignment).unwrap();

        let content = std::fs::read(&path).unwrap();
        assert_ne!(
            content[0], 0xEE,
            "the pre-existing buffered-written content survived, so creation \
             was skipped and the unbuffered self-check did not run"
        );
        assert_eq!(content[0], 0);
        assert_eq!(content[1], 1);
    }
}
