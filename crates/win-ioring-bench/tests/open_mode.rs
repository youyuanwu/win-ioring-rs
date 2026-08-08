//! Pins the handle mode that `win_ioring::file::File::open` produces.
//!
//! # Why this test exists
//!
//! [`win_ioring::file::File::open`] and `File::create` open with
//! `FILE_FLAG_OVERLAPPED`, so the file object does **not** serialise I/O and
//! operations submitted at depth stay at depth.
//!
//! This file previously pinned the opposite. It asserted that both
//! constructors produced *synchronous* handles, and explained that fixing the
//! defect would invalidate the twenty `win-ioring` cells of the published
//! matrix — which it did. The flag was added, the matrix was re-run, and the
//! pin was inverted as part of doing so. The failure message that pin carried
//! did its job: it told the implementer what the change would cost before they
//! paid it.
//!
//! The pin remains valuable in the new direction. Silently *losing* the flag —
//! by routing either constructor back through `std::fs::File::open`, say —
//! would restore serialisation and invalidate the matrix again, and nothing
//! else in the workspace would notice. Handle mode is invisible to every
//! functional test: a serialising handle returns correct bytes.
//!
//! # Scope of these tests
//!
//! They assert handle mode, never timing. The size of the effect the flag has
//! on throughput is the `handle-mode` arm's business, measured through the real
//! harness and reported in `docs/performance.md`. Nothing here depends on those
//! figures.

use std::os::windows::fs::OpenOptionsExt;

use win_ioring_bench::unbuffered_workload::{FILE_NO_INTERMEDIATE_BUFFERING, file_mode};

/// `FILE_FLAG_OVERLAPPED`, from the same source the rest of the crate uses.
use windows::Win32::Storage::FileSystem::{FILE_FLAG_NO_BUFFERING, FILE_FLAG_OVERLAPPED};

/// The exact mode a synchronous, buffered handle reports.
///
/// Asserted as a whole value rather than only as a masked bit. Masking against
/// `FILE_SYNCHRONOUS_IO_NONALERT` and comparing to that same constant is
/// satisfied whenever the constant's bits are a subset of the mode, and
/// unconditionally when it is zero — `mode & 0 == 0` holds for every handle in
/// existence. That is the fourth instance in this work of a check that cannot
/// distinguish "the guard held" from "the guard never ran", so these tests pin
/// the literal values the host actually reports and the constant is left to
/// document rather than to decide.
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
fn file_open_produces_an_overlapped_handle() {
    let path = scratch("open.dat");
    let file = win_ioring::file::File::open(&path).unwrap();

    assert_eq!(
        mode_of(&file),
        MODE_OVERLAPPED,
        "win_ioring::file::File::open no longer produces an overlapped \
         handle.\n\
         \n\
         If FILE_FLAG_OVERLAPPED was just removed, or the constructor was \
         routed back through std::fs::File::open: the file object now \
         serialises I/O, so operations submitted at depth 64 run at depth 1. \
         Nothing else in this workspace would notice — a serialising handle \
         returns correct bytes — but the twenty win-ioring cells of the fifty \
         in the published matrix in docs/performance.md were measured through \
         overlapped handles, and that matrix is a single-run artefact that is \
         never patched from a second run. Losing the flag means re-running and \
         republishing the whole table."
    );
}

#[test]
fn file_create_produces_an_overlapped_handle() {
    let path = scratch("create.dat");
    let file = win_ioring::file::File::create(&path).unwrap();

    assert_eq!(
        mode_of(&file),
        MODE_OVERLAPPED,
        "win_ioring::file::File::create no longer produces an overlapped \
         handle; see the note on the File::open pin test — the published write \
         cells were measured through handles from this function."
    );
}

#[test]
fn an_adopted_synchronous_handle_differs_from_the_default() {
    // The negative control, and the reason the pins above are attributable.
    //
    // Without this, the pins would also pass if `file_mode` had broken in a way
    // that returned MODE_OVERLAPPED (zero) for everything — which is the easiest
    // way for the read-back to fail, since zero is what an errored-out or
    // never-written buffer holds. This asserts the same mechanism reports a
    // *different* answer for a handle known to differ in exactly the one
    // respect under test.
    //
    // NOTE ON DIRECTION. This control was inverted when the default changed.
    // It used to adopt an *overlapped* handle and contrast it with File::open's
    // synchronous one. Once File::open became overlapped, that comparison put
    // two handles of the same mode side by side, and its `assert_ne!` could no
    // longer fail — the gate-that-cannot-fail species this file's header names.
    // The contrasting handle must therefore be the synchronous one now.
    //
    // It also demonstrates the documented remedy: `from_std` remains the route
    // to a synchronous handle for a caller who deliberately wants one.
    let path = scratch("synchronous.dat");
    let std_file = std::fs::File::open(&path).unwrap();
    let file = win_ioring::file::File::from_std(std_file);

    assert_eq!(
        mode_of(&file),
        MODE_SYNCHRONOUS,
        "a synchronous handle adopted through from_std does not report the \
         expected mode, so the read-back cannot distinguish the two modes and \
         the pin tests above prove nothing"
    );

    // The two modes must differ. This is the assertion that survives any error
    // in the named constants above, including all of them being zero: it
    // compares two live measurements against each other rather than against a
    // literal, so it fails if the read-back has collapsed to a constant.
    let overlapped = win_ioring::file::File::open(scratch("compare.dat")).unwrap();
    assert_ne!(
        mode_of(&overlapped),
        mode_of(&file),
        "File::open and a synchronous from_std handle report the same mode, so \
         the read-back is not measuring what these tests assume"
    );

    // Neither handle is unbuffered: these files were opened without
    // FILE_FLAG_NO_BUFFERING. The bare `mode & K == 0` below proves nothing on
    // its own — the overlapped mode is already pinned to exactly zero, so it
    // holds for every K — which is what made it instance six of the gate that
    // cannot fail, in the file whose own header names the species: setting
    // FILE_NO_INTERMEDIATE_BUFFERING to 0xFFFF_FFFF left all three tests
    // passing. It is kept only behind the positive read-back below, which
    // asserts the *whole* mode of a genuinely unbuffered handle rather than
    // masking it. The masked form was instance seven: `mode & K == K` is
    // satisfied by K = 0, so zeroing the constant left this file green again.
    // Whole-value equality has no such hole, because the modes involved are
    // themselves non-zero.
    let unbuffered = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_NO_BUFFERING.0 | FILE_FLAG_OVERLAPPED.0)
        .open(scratch("compare.dat"))
        .unwrap();
    let unbuffered = win_ioring::file::File::from_std(unbuffered);
    assert_eq!(
        mode_of(&unbuffered),
        FILE_NO_INTERMEDIATE_BUFFERING,
        "an unbuffered overlapped handle no longer reports exactly \
         FILE_NO_INTERMEDIATE_BUFFERING, so asserting that bit's absence \
         elsewhere in this file proves nothing"
    );
    assert_eq!(mode_of(&overlapped) & FILE_NO_INTERMEDIATE_BUFFERING, 0);

    // The doctest literal pin that used to live here has been REMOVED, not
    // updated. File::open's example no longer hardcodes FILE_FLAG_OVERLAPPED:
    // the constructor sets the flag itself, so the example demonstrates the
    // opposite case and opens with plain `std::fs::File::open`. The crate's own
    // flag constant is now derived from `windows` rather than written as a
    // literal, so there is no literal left to drift. A pin kept here would name
    // nothing and pass unconditionally — which is precisely the species this
    // file exists to avoid, and the reason it is deleted rather than left in
    // place looking vigilant.
}
