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
//! what its interface promises. Measured with buffering disabled on an NVMe
//! SSD, random 4 KiB reads at a submitted depth of 64 ran at 117 µs/IO through
//! a `File::open` handle and 11.9 µs/IO through an overlapped one — a tenfold
//! difference produced entirely by the missing flag.
//!
//! The second is that **fixing it here would invalidate the published
//! results**. Every figure in `docs/performance.md` was measured through
//! handles from this function. Setting the flag would change the timed path of
//! all fifty warm-cache cells and force a full re-run and republication. That
//! is deliberately out of scope; the question is recorded in
//! `docs/pending-work.md` with its cost.
//!
//! So the behaviour is *pinned* rather than changed. If someone later adds the
//! flag — which is a reasonable thing to want, and the pending-work note invites
//! it — this test fails and says why, instead of the change silently
//! invalidating a published matrix that nothing else would notice.
//!
//! Under a warm page cache the flag makes no measurable difference, because a
//! cached read returns synchronously after a memory copy and there is nothing
//! to overlap. That is why fifty cells were measured on synchronous handles
//! without the defect showing up.

use std::os::windows::fs::OpenOptionsExt;

use win_ioring_bench::unbuffered_workload::{
    FILE_NO_INTERMEDIATE_BUFFERING, FILE_SYNCHRONOUS_IO_NONALERT, file_mode,
};

/// `FILE_FLAG_OVERLAPPED`.
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;

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
    // borrow. `ManuallyDrop` prevents the temporary `std::fs::File` from
    // closing it, so ownership is unaffected.
    let borrowed = unsafe { std::fs::File::from_raw_handle(raw.0) };
    let borrowed = std::mem::ManuallyDrop::new(borrowed);
    file_mode(&borrowed).unwrap()
}

#[test]
fn file_open_produces_a_synchronous_handle() {
    let path = scratch("open.dat");
    let file = win_ioring::file::File::open(&path).unwrap();

    assert_eq!(
        mode_of(&file) & FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_SYNCHRONOUS_IO_NONALERT,
        "win_ioring::file::File::open no longer produces a synchronous handle.\n\
         \n\
         If FILE_FLAG_OVERLAPPED was just added: that is a defensible change — \
         the flag's absence is a real limitation, documented on File::open and \
         recorded in docs/pending-work.md. But it changes the timed path of \
         every cell in the published 50-cell warm-cache matrix in \
         docs/performance.md, all of which were measured through handles from \
         this function. That matrix must be re-run and republished, not merely \
         re-read. Update this test as part of doing so."
    );
}

#[test]
fn file_create_produces_a_synchronous_handle() {
    let path = scratch("create.dat");
    let file = win_ioring::file::File::create(&path).unwrap();

    assert_eq!(
        mode_of(&file) & FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_SYNCHRONOUS_IO_NONALERT,
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
        .custom_flags(FILE_FLAG_OVERLAPPED)
        .open(&path)
        .unwrap();
    let file = win_ioring::file::File::from_std(std_file);

    assert_eq!(
        mode_of(&file) & FILE_SYNCHRONOUS_IO_NONALERT,
        0,
        "an overlapped handle adopted through from_std reports as synchronous, \
         so the read-back cannot distinguish the two modes and the pin tests \
         above prove nothing"
    );

    // Neither handle is unbuffered: this file was opened without
    // FILE_FLAG_NO_BUFFERING, so the two axes are independent and the pin above
    // is specifically about synchronicity.
    assert_eq!(mode_of(&file) & FILE_NO_INTERMEDIATE_BUFFERING, 0);
}
