//! Pins the handle mode that `win_ioring::file::File::open` produces.
//!
//! # Why this test exists
//!
//! [`win_ioring::file::File::open`] delegates to [`std::fs::File::open`], which
//! does not pass `FILE_FLAG_OVERLAPPED`. The resulting handle carries
//! `FILE_SYNCHRONOUS_IO_NONALERT`, so **the file object serialises I/O**: at
//! most one operation is outstanding against it at a time, however many the
//! crate submits to the ring.
//!
//! Two things follow, and they pull in opposite directions.
//!
//! The first is that this is a genuine defect in the crate's own API. An async
//! I/O crate whose file handles serialise at the file object is not delivering
//! what its interface promises.
//!
//! The second is that **fixing it here would invalidate the published
//! results**. The twenty `win-ioring` cells of the matrix in
//! `docs/performance.md` were all measured through handles from
//! `win_ioring::file::File::open` and `File::create`, and that matrix is a
//! single-run artefact that is never patched from a second run
//! (`docs/performance.md:236-242`), so setting the flag would force a full
//! re-run and republication of the whole table. That is deliberately out of
//! scope; the question is recorded in `docs/pending-work.md` with its cost.
//!
//! So the behaviour is *pinned* rather than changed. If someone later adds the
//! flag — which is a reasonable thing to want, and the pending-work note invites
//! it — this test fails and says why, instead of the change silently
//! invalidating a published matrix that nothing else would notice.
//!
//! Under a warm page cache the flag is expected to make no measurable
//! difference, because a cached read returns synchronously after a memory copy
//! and there is nothing to overlap. That is a mechanism argument and not a
//! measurement: no warm-cache A/B on the flag has been run, and none is
//! required for the pin, which asserts only what the handle *is*.
//!
//! # Scope of these tests
//!
//! They assert handle mode, never timing. The size of the effect the flag has
//! on throughput is the unbuffered arm's business, measured through the real
//! harness and reported in `docs/performance.md` once that arm has run.
//! Nothing here depends on those figures.

use std::os::windows::fs::OpenOptionsExt;

use win_ioring_bench::unbuffered_workload::{FILE_NO_INTERMEDIATE_BUFFERING, file_mode};

/// `FILE_FLAG_OVERLAPPED`, from the same source the rest of the crate uses.
use windows::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;

/// The exact mode a synchronous, buffered handle reports.
///
/// Asserted as a whole value rather than only as a masked bit. Masking against
/// `FILE_SYNCHRONOUS_IO_NONALERT` and comparing to that same constant is
/// satisfied by *any* value of the constant, including zero — `mode & 0 == 0`
/// holds for every handle in existence. That is the fourth instance in this
/// work of a check that cannot distinguish "the guard held" from "the guard
/// never ran", so these tests pin the literal values the host actually reports
/// and the constant is left to document rather than to decide.
const MODE_SYNCHRONOUS: u32 = 0x0000_0020;

/// The exact mode an overlapped, buffered handle reports.
const MODE_OVERLAPPED: u32 = 0x0000_0000;

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("win-ioring-bench-openmode");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, vec![0_u8; 4096]).unwrap();
    path
}

/// Reads the mode of a handle owned by a [`win_ioring::file::File`].
///
/// Borrows the raw handle for the duration of the query only, and wraps it in a
/// `ManuallyDrop` so the borrowed handle is never closed by this function — the
/// `File` remains its sole owner.
fn mode_of(file: &win_ioring::file::File) -> u32 {
    use std::os::windows::io::FromRawHandle;

    let raw = file.as_raw_handle();
    // SAFETY: `raw` is a live handle owned by `file`, which outlives this
    // borrow. `ManuallyDrop` wraps the temporary in the same expression that
    // creates it, so there is no point at which an unwrapped `std::fs::File`
    // could be dropped and close a handle it does not own.
    let borrowed = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_handle(raw.0) });
    file_mode(&borrowed).unwrap()
}

#[test]
fn file_open_produces_a_synchronous_handle() {
    let path = scratch("open.dat");
    let file = win_ioring::file::File::open(&path).unwrap();

    assert_eq!(
        mode_of(&file),
        MODE_SYNCHRONOUS,
        "win_ioring::file::File::open no longer produces a synchronous handle.\n\
         \n\
         If FILE_FLAG_OVERLAPPED was just added: that is a defensible change — \
         the flag's absence is a real limitation, documented on File::open and \
         recorded in docs/pending-work.md. But the twenty win-ioring cells of \
         the published matrix in docs/performance.md were all measured through \
         handles from this function, and that matrix is a single-run artefact \
         that is never patched from a second run, so the whole table must be \
         re-run and republished rather than merely re-read. Update this test \
         as part of doing so."
    );
}

#[test]
fn file_create_produces_a_synchronous_handle() {
    let path = scratch("create.dat");
    let file = win_ioring::file::File::create(&path).unwrap();

    assert_eq!(
        mode_of(&file),
        MODE_SYNCHRONOUS,
        "win_ioring::file::File::create no longer produces a synchronous \
         handle; see the note on the File::open pin test — the published write \
         cells were measured through handles from this function."
    );
}

#[test]
fn an_adopted_overlapped_handle_is_not_synchronous() {
    // The negative control, and the reason the pin above is attributable.
    //
    // Without this, `file_open_produces_a_synchronous_handle` would also pass
    // if `file_mode` had broken in a way that returned the synchronous bit for
    // everything, or if the read-back had stopped working entirely and happened
    // to return a value with that bit set. This asserts the same mechanism
    // reports a *different* answer for a handle known to differ in exactly the
    // one respect under test.
    //
    // It also demonstrates the documented remedy: `from_std` is the additive
    // route to an overlapped handle, and it needs no change to `File::open`.
    let path = scratch("overlapped.dat");
    let std_file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OVERLAPPED.0)
        .open(&path)
        .unwrap();
    let file = win_ioring::file::File::from_std(std_file);

    assert_eq!(
        mode_of(&file),
        MODE_OVERLAPPED,
        "an overlapped handle adopted through from_std does not report the \
         expected mode, so the read-back cannot distinguish the two modes and \
         the pin tests above prove nothing"
    );

    // The two modes must differ. This is the assertion that survives any error
    // in the named constants above, including all of them being zero: it
    // compares two live measurements against each other rather than against a
    // literal, so it fails if the read-back has collapsed to a constant.
    let synchronous = win_ioring::file::File::open(scratch("compare.dat")).unwrap();
    assert_ne!(
        mode_of(&synchronous),
        mode_of(&file),
        "File::open and an overlapped from_std handle report the same mode, so \
         the read-back is not measuring what these tests assume"
    );

    // Neither handle is unbuffered: this file was opened without
    // FILE_FLAG_NO_BUFFERING, so the two axes are independent and the pin above
    // is specifically about synchronicity.
    assert_eq!(mode_of(&file) & FILE_NO_INTERMEDIATE_BUFFERING, 0);
}
