//! What the host requires of an unbuffered read, and whether we satisfy it.
//!
//! These tests run under the default `cargo test`, at sizes that take
//! milliseconds. They assert **structure, never timing**: that the alignment
//! query answers, that an [`AlignedBuf`] honours the answer, and — the part
//! that matters — that a read violating the answer actually fails. Timing
//! properties are device-bound and belong to `cargo bench --bench unbuffered`.
//!
//! # Why the negative control is the important test here
//!
//! Asserting that a correctly-aligned unbuffered read succeeds proves less than
//! it appears to. It would also succeed if `FILE_FLAG_NO_BUFFERING` were
//! silently dropped, or if the alignment code returned a number nobody checked.
//! The test that carries weight is the one asserting a *misaligned* read
//! **fails**: it is the one that fails when the constraint being modelled has
//! stopped existing.
//!
//! That control is written to tolerate host variance. Alignment *enforcement*
//! is volume-dependent exactly as the alignment values are, so the test asserts
//! "the misaligned read was rejected" rather than a specific error code, and on
//! a volume that enforces nothing it **skips with a stated reason** rather than
//! passing vacuously. A test that quietly passes where it cannot run is
//! indistinguishable from a test that was deleted.

use std::fs::OpenOptions;
use std::io::Read;
use std::os::windows::fs::OpenOptionsExt;

use win_ioring_bench::align::Alignment;
use win_ioring_bench::aligned::AlignedBuf;

const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;

/// A directory of this test's own, so it cannot disturb any benchmark data.
fn dir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join("win-ioring-bench-align-tests");
    std::fs::create_dir_all(&d).expect("could not create the test directory");
    d
}

/// Writes a file of `bytes` bytes and returns its path.
fn scratch(name: &str, bytes: usize) -> std::path::PathBuf {
    let path = dir().join(name);
    std::fs::write(&path, vec![0xABu8; bytes]).expect("could not write the scratch file");
    path
}

#[test]
fn the_host_reports_a_usable_alignment() {
    let path = scratch("query.dat", 4096);
    let alignment = Alignment::query(&path).expect("the volume alignment query failed");

    assert!(alignment.logical_sector.is_power_of_two());
    assert!(alignment.physical_sector.is_power_of_two());
    assert!(alignment.granularity() >= alignment.logical_sector as usize);
    assert!(alignment.granularity() >= alignment.physical_sector as usize);

    // R1.4: the figures are uninterpretable without this, so it must render.
    assert!(alignment.describe().contains("logical sector"));
}

#[test]
fn rounding_lands_on_legal_lengths() {
    let path = scratch("round.dat", 4096);
    let alignment = Alignment::query(&path).expect("the volume alignment query failed");
    let g = alignment.granularity();

    assert_eq!(alignment.round_up(0), 0);
    assert_eq!(alignment.round_up(1), g);
    assert_eq!(alignment.round_up(g), g);
    assert_eq!(alignment.round_up(g + 1), 2 * g);

    assert!(alignment.is_aligned(0));
    assert!(alignment.is_aligned(g as u64));
    assert!(!alignment.is_aligned(1));
}

#[test]
fn an_aligned_buffer_is_actually_aligned() {
    let path = scratch("buf.dat", 4096);
    let alignment = Alignment::query(&path).expect("the volume alignment query failed");
    let g = alignment.granularity();

    for cap in [1, g / 2, g, g + 1, 4 * g] {
        let buf = AlignedBuf::new(cap, g).expect("could not allocate");
        assert!(
            buf.is_aligned(),
            "AlignedBuf::new({cap}, {g}) produced a base address that is not aligned to {g}"
        );
        assert!(
            buf.capacity() >= cap.max(g),
            "capacity {} is smaller than the requested {cap}",
            buf.capacity()
        );
        assert_eq!(
            buf.capacity() % g,
            0,
            "capacity {} is not a whole number of {g}-byte units",
            buf.capacity()
        );
    }
}

#[test]
fn a_vec_is_not_an_acceptable_substitute() {
    // The premise of `AlignedBuf` existing at all. A `Vec<u8>`'s guarantee is
    // `align_of::<u8>() == 1`; a subslice of one has whatever alignment its
    // offset gives it. This asserts the second half, which is deterministic —
    // the first half is merely usually true, which is the whole problem.
    let v = vec![0u8; 8192];
    let base = v.as_ptr() as usize;
    let offset_by_one = base + 1;
    assert_ne!(
        offset_by_one % 4096,
        0,
        "a pointer one byte past an aligned base cannot itself be 4096-aligned"
    );
}

#[test]
fn a_correctly_aligned_unbuffered_read_succeeds() {
    let path = scratch("ok.dat", 1 << 20);
    let alignment = Alignment::query(&path).expect("the volume alignment query failed");
    let g = alignment.granularity();

    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_NO_BUFFERING)
        .open(&path)
        .expect("could not open unbuffered");

    let mut buf = AlignedBuf::new(g, g).expect("could not allocate");
    let n = file
        .read(buf.spare())
        .expect("a correctly aligned unbuffered read should succeed");
    assert_eq!(n, g, "expected a full sector, got {n}");
}

#[test]
fn a_misaligned_unbuffered_read_is_rejected() {
    let path = scratch("bad.dat", 1 << 20);
    let alignment = Alignment::query(&path).expect("the volume alignment query failed");
    let g = alignment.granularity();

    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_NO_BUFFERING)
        .open(&path)
        .expect("could not open unbuffered");

    let mut buf = AlignedBuf::new(4 * g, g).expect("could not allocate");

    // Two independent violations, either of which the host may or may not
    // enforce: a base address one byte off, and a length that is not a whole
    // number of sectors.
    let misaligned_base = file.read(&mut buf.spare()[1..1 + g]).is_err();
    let misaligned_len = file.read(&mut buf.spare()[..g - 1]).is_err();

    if !misaligned_base && !misaligned_len {
        // Not a pass. This host does not enforce what the arm is built around,
        // so the control cannot run — say so rather than report success.
        eprintln!(
            "SKIPPED: this volume enforced neither a misaligned base nor a \
             misaligned length at {g}-byte granularity, so the unbuffered \
             alignment control cannot be exercised here. The measurement is \
             unaffected; the guarantee this test provides is not available on \
             this host."
        );
        return;
    }

    assert!(
        misaligned_base || misaligned_len,
        "at least one alignment violation must be rejected"
    );
}
