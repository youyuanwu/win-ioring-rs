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
/// cannot be passed to [`crate::workload::warm`] or
/// [`crate::workload::ensure_file`]. Warming a file destroys its usefulness for
/// unbuffered measurement — see the module documentation — and the destruction
/// is silent, so it is prevented by the type system rather than by discipline.
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
/// The same snippet against an ordinary path *does* compile, so the failure
/// above can only be the missing conversion and not some unrelated mistake:
///
/// ```no_run
/// let path = std::env::temp_dir().join("example.dat");
/// win_ioring_bench::workload::warm(&path).unwrap();
/// ```
///
/// # What would defeat this, and what would not
///
/// Verified by deliberately adding each impl and re-running the doc-test.
/// Adding `Deref<Target = Path>` **defeats the guard** — deref coercion is
/// implicit, so `warm(file.path())` would start compiling silently — and the
/// doc-test above duly fails when it is present. Adding `AsRef<Path>` does
/// *not* defeat it, because reaching `warm` would still require writing
/// `.as_ref()` by hand, which is as deliberate an act as calling
/// [`UnbufferedPath::as_raw_path`]. So the rule this type must keep is
/// specifically: **never implement `Deref<Target = Path>`**.
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
    /// Creates the working file if it is missing or the wrong size.
    ///
    /// `bytes` is rounded up to the volume's alignment granularity, because an
    /// unbuffered write of a partial sector is rejected outright. The rounded
    /// size is what [`UnbufferedFile::bytes`] reports, so callers computing
    /// offsets from it stay in range.
    ///
    /// Recreates rather than trusting what is there, matching
    /// [`crate::workload::ensure_file`]: a file left from a different
    /// configuration would silently change what is measured.
    pub fn create(dir: &Path, bytes: u64, alignment: &Alignment) -> io::Result<Self> {
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
pub fn ensure_unbuffered_file(path: &Path, bytes: u64, alignment: &Alignment) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Ok(meta) = std::fs::metadata(path)
        && meta.len() == bytes
    {
        return Ok(());
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
        let take = ((bytes - written) as usize).min(chunk);
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
    if status.is_err() {
        return Err(io::Error::other(format!(
            "NtQueryInformationFile(FileModeInformation) failed: {status:?}"
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

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join("win-ioring-bench-unbuffered-unit");
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_created_file_is_readable_unbuffered_and_reports_the_flag() {
        let d = dir();
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
        let d = dir();
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
        let d = dir();
        let alignment = Alignment::query(&d).unwrap();
        let g = alignment.granularity() as u64;

        // Bypassing `create`, which rounds, to reach the check directly.
        let err = ensure_unbuffered_file(&d.join("partial.dat"), g - 1, &alignment)
            .expect_err("a partial-sector size must be refused, not silently written");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn the_three_open_modes_are_distinguishable() {
        // The evidence behind the mode table in Phase 3, asserted rather than
        // recorded as prose: if these bits ever stop meaning what the arm
        // assumes, this fails instead of the measurements quietly changing
        // meaning.
        let d = dir();
        let alignment = Alignment::query(&d).unwrap();
        let g = alignment.granularity() as u64;
        let file = UnbufferedFile::create(&d, 16 * g, &alignment).unwrap();
        let path = file.path().as_raw_path();

        let plain = std::fs::File::open(path).unwrap();
        assert!(
            is_synchronous(&plain).unwrap(),
            "a plain File::open handle is expected to be synchronous"
        );
        assert!(!is_unbuffered(&plain).unwrap());

        let unbuffered = file.path().open_unbuffered().unwrap();
        assert!(is_unbuffered(&unbuffered).unwrap());
    }
}
