//! What the host requires of an unbuffered read, asked rather than assumed.
//!
//! `FILE_FLAG_NO_BUFFERING` takes the operating system's page cache out of the
//! path, and in exchange the caller must satisfy three alignment constraints:
//! the buffer's base address, the read length, and the file offset. Violating
//! any of them fails the read with `ERROR_INVALID_PARAMETER` rather than
//! degrading to something slow but correct, so these are not values to guess at.
//!
//! They are also **volume-dependent**, which is the reason this module exists
//! instead of a constant. A figure measured here is only interpretable
//! alongside the granularity the host reported, so [`Alignment`] records
//! *every* answer the host gave — not merely the one it acts on — and
//! [`Alignment::describe`] renders them for the run's report and for
//! `docs/performance.md`. That is the point: what the host said is **data
//! collected at run time**, not a table someone typed after looking at their
//! own machine.
//!
//! # The sources disagree, and that is not a bug
//!
//! Windows exposes at least three answers, through `FileAlignmentInfo`, the
//! `FILE_STORAGE_INFO` sector sizes, and `GetDiskFreeSpaceW`. They disagree
//! because they answer different questions: the *buffer* must meet the device's
//! addressing requirement, while *length and offset* must be multiples of the
//! sector size. Deliberate violation on the development host confirmed exactly
//! that split — a buffer at `base + 64` succeeded where `base + 1` failed, and
//! lengths of 512 and 4096 succeeded where 4095 failed.
//!
//! This module takes the **strictest** of them for every purpose. Over-aligning
//! is always legal, costs a bounded amount of address space, and removes a
//! class of host-specific failure that would otherwise appear only on somebody
//! else's volume. Being fast on the development machine and broken elsewhere is
//! not a trade this crate makes.

use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    FILE_ALIGNMENT_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_STORAGE_INFO, FileAlignmentInfo,
    FileStorageInfo, GetDiskFreeSpaceW, GetFileInformationByHandleEx,
};
use windows::core::HSTRING;

/// Every answer the host gave about one volume's alignment requirements.
///
/// Obtained by [`Alignment::query`]. Each field is what a specific Windows API
/// reported; nothing here is a default or a fallback. The struct deliberately
/// carries values it does not act on, because a reader on a different volume
/// needs to see what *this* volume said in order to judge whether a published
/// figure transfers to theirs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Alignment {
    /// `FileAlignmentInfo`'s `AlignmentRequirement`, as a byte count.
    ///
    /// Reported as a mask (`0` means 1 byte, `1` means 2, `3` means 4, …) and
    /// converted here, because a mask sitting beside three byte counts invites
    /// exactly the misreading it looks like.
    pub alignment_requirement: u32,
    /// `FILE_STORAGE_INFO::LogicalBytesPerSector` — the addressable sector size.
    pub logical_sector: u32,
    /// `FILE_STORAGE_INFO::PhysicalBytesPerSectorForAtomicity`.
    pub physical_atomicity: u32,
    /// `FILE_STORAGE_INFO::PhysicalBytesPerSectorForPerformance`.
    pub physical_performance: u32,
    /// `FILE_STORAGE_INFO::ByteOffsetForSectorAlignment`.
    ///
    /// `u32::MAX` means the partition is not aligned to the physical sector
    /// size, which Windows reports in-band rather than as an error.
    pub byte_offset_for_sector_alignment: u32,
    /// `GetDiskFreeSpaceW`'s `lpBytesPerSector` — the oldest of the three
    /// answers, and the one most often quoted in documentation.
    pub disk_bytes_per_sector: u32,
}

impl Alignment {
    /// Asks the volume holding `path` what it requires.
    ///
    /// `path` need only identify the volume; it is opened for metadata alone,
    /// never for data, so this is safe to call before the benchmark's data file
    /// exists. In particular it does **not** poison an unbuffered working file:
    /// no data is read through the handle. `path` may be a directory — passing
    /// the working directory is the usual case, since the alignment must be
    /// known before a correctly sized file can be created.
    pub fn query(path: &Path) -> io::Result<Self> {
        // `FILE_FLAG_BACKUP_SEMANTICS` is what makes a directory openable at
        // all; without it `std::fs::File::open` on a directory fails with
        // ERROR_ACCESS_DENIED. Since the alignment granularity is needed to
        // decide how large the working file may be, the query must work before
        // that file exists, which means it must accept a directory.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
            .open(path)?;
        // Borrowed for the duration of these calls only. `file` is alive across
        // all of them and closes the handle on drop, so no raw handle outlives
        // its owner.
        let handle = HANDLE(file.as_raw_handle() as _);

        let mut storage = FILE_STORAGE_INFO::default();
        // SAFETY: `handle` is open for the call, and `storage` is a
        // correctly-sized, correctly-typed buffer for `FileStorageInfo`.
        unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileStorageInfo,
                (&raw mut storage).cast(),
                size_of::<FILE_STORAGE_INFO>() as u32,
            )
        }?;

        let mut align_info = FILE_ALIGNMENT_INFO::default();
        // SAFETY: as above, for `FileAlignmentInfo`.
        unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileAlignmentInfo,
                (&raw mut align_info).cast(),
                size_of::<FILE_ALIGNMENT_INFO>() as u32,
            )
        }?;

        let alignment = Self {
            // The API reports a mask: 0 => 1 byte, 1 => 2 bytes, 3 => 4 bytes.
            alignment_requirement: align_info.AlignmentRequirement.saturating_add(1),
            logical_sector: storage.LogicalBytesPerSector,
            physical_atomicity: storage.PhysicalBytesPerSectorForAtomicity,
            physical_performance: storage.PhysicalBytesPerSectorForPerformance,
            byte_offset_for_sector_alignment: storage.ByteOffsetForSectorAlignment,
            disk_bytes_per_sector: disk_bytes_per_sector(path).unwrap_or(0),
        };
        alignment.validate()?;
        Ok(alignment)
    }

    /// Rejects a report this module cannot safely build on.
    ///
    /// Only [`Self::logical_sector`] is required to be present: it is the one
    /// answer every volume owes. The rest are recorded for the report and may
    /// legitimately be absent — a volume that declines `GetDiskFreeSpaceW`, or
    /// reports no performance hint, is unusual but not unusable, and turning
    /// the whole suite red for it would be an asymmetry against the host
    /// tolerance the rest of this arm is built with. What is *not* tolerated is
    /// a value that is present and nonsensical, because
    /// [`Self::round_up`] and `Layout::from_size_align` would then fail far
    /// from here.
    fn validate(&self) -> io::Result<()> {
        if self.logical_sector == 0 || !self.logical_sector.is_power_of_two() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "host reported LogicalBytesPerSector = {}, which is not a power of two",
                    self.logical_sector
                ),
            ));
        }
        for (name, value) in [
            (
                "PhysicalBytesPerSectorForAtomicity",
                self.physical_atomicity,
            ),
            (
                "PhysicalBytesPerSectorForPerformance",
                self.physical_performance,
            ),
            ("AlignmentRequirement", self.alignment_requirement),
            ("BytesPerSector", self.disk_bytes_per_sector),
        ] {
            if value != 0 && !value.is_power_of_two() {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("host reported {name} = {value}, which is not a power of two"),
                ));
            }
        }
        Ok(())
    }

    /// The alignment used for buffers, lengths and offsets alike.
    ///
    /// The strictest of the reported values, for the reason given in the module
    /// documentation: over-aligning is legal everywhere, and a single number
    /// for all three constraints removes the chance of applying the wrong one.
    ///
    /// A source that reported nothing contributes nothing — an absent answer
    /// cannot drag the result *down*, which is what makes tolerating one safe.
    pub fn granularity(&self) -> usize {
        self.logical_sector
            .max(self.physical_atomicity)
            .max(self.physical_performance)
            .max(self.alignment_requirement)
            .max(self.disk_bytes_per_sector) as usize
    }

    /// Rounds `n` up to the next multiple of [`Self::granularity`].
    ///
    /// # Errors
    ///
    /// If the rounded value would overflow `usize`. Wrapping here would produce
    /// a zero or truncated length, and so a silently empty read rather than a
    /// reported failure.
    pub fn round_up(&self, n: usize) -> io::Result<usize> {
        let g = self.granularity();
        n.checked_next_multiple_of(g).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("rounding {n} up to a multiple of {g} overflows"),
            )
        })
    }

    /// True if `n` is a legal length or offset for an unbuffered read.
    pub fn is_aligned(&self, n: u64) -> bool {
        n.is_multiple_of(self.granularity() as u64)
    }

    /// What the host said, for the run's report and for `docs/performance.md`.
    ///
    /// A reader on a different volume cannot interpret these timings without
    /// knowing what this one required, so every queried value appears —
    /// including the ones [`Self::granularity`] did not end up using.
    pub fn describe(&self) -> String {
        let offset = if self.byte_offset_for_sector_alignment == u32::MAX {
            "not aligned to the physical sector size".to_string()
        } else {
            format!("{} B", self.byte_offset_for_sector_alignment)
        };
        format!(
            "AlignmentRequirement {} B, LogicalBytesPerSector {} B, \
             PhysicalBytesPerSectorForAtomicity {} B, \
             PhysicalBytesPerSectorForPerformance {} B, \
             ByteOffsetForSectorAlignment {}, GetDiskFreeSpace BytesPerSector {} B; using {} B",
            self.alignment_requirement,
            self.logical_sector,
            self.physical_atomicity,
            self.physical_performance,
            offset,
            self.disk_bytes_per_sector,
            self.granularity()
        )
    }
}

/// `GetDiskFreeSpaceW`'s sector size for the volume holding `path`.
///
/// Returns `None` rather than an error: this is the one source nothing is built
/// on. It is recorded because it is the figure most documentation quotes, and a
/// reader comparing volumes will look for it.
fn disk_bytes_per_sector(path: &Path) -> Option<u32> {
    let root = path.components().next()?;
    let root = HSTRING::from(format!("{}\\", root.as_os_str().to_string_lossy()));

    let mut sectors_per_cluster = 0u32;
    let mut bytes_per_sector = 0u32;
    let mut free_clusters = 0u32;
    let mut total_clusters = 0u32;

    // SAFETY: `root` outlives the call, and each out-parameter is a live,
    // correctly-typed local.
    unsafe {
        GetDiskFreeSpaceW(
            &root,
            Some(&raw mut sectors_per_cluster),
            Some(&raw mut bytes_per_sector),
            Some(&raw mut free_clusters),
            Some(&raw mut total_clusters),
        )
    }
    .ok()?;

    Some(bytes_per_sector)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sane() -> Alignment {
        Alignment {
            alignment_requirement: 4,
            logical_sector: 512,
            physical_atomicity: 4096,
            physical_performance: 4096,
            byte_offset_for_sector_alignment: 0,
            disk_bytes_per_sector: 512,
        }
    }

    /// [`Alignment::validate`] only fires on a host that reports something
    /// unusable, which cannot be provoked by querying this machine.
    /// Constructing the struct directly is the only way to reach it — and
    /// without this test the check is unverified, which was established by
    /// deleting it and watching every other test still pass.
    #[test]
    fn a_nonsensical_report_is_rejected() {
        let cases = [
            (
                "zero logical sector",
                Alignment {
                    logical_sector: 0,
                    ..sane()
                },
            ),
            (
                "odd logical sector",
                Alignment {
                    logical_sector: 513,
                    ..sane()
                },
            ),
            (
                "odd atomicity",
                Alignment {
                    physical_atomicity: 4095,
                    ..sane()
                },
            ),
            (
                "odd performance",
                Alignment {
                    physical_performance: 4095,
                    ..sane()
                },
            ),
            (
                "odd alignment requirement",
                Alignment {
                    alignment_requirement: 7,
                    ..sane()
                },
            ),
            (
                "odd disk sector",
                Alignment {
                    disk_bytes_per_sector: 999,
                    ..sane()
                },
            ),
        ];
        for (why, alignment) in cases {
            assert!(
                alignment.validate().is_err(),
                "{why} should be rejected: a zero or non-power-of-two value makes \
                 round_up meaningless and Layout::from_size_align reject the \
                 alignment, far from here"
            );
        }
    }

    /// The counterpart to the above: an absent optional value is *tolerated*,
    /// because a volume that declines to answer is unusual, not unusable.
    #[test]
    fn an_absent_optional_value_is_tolerated() {
        let alignment = Alignment {
            physical_atomicity: 0,
            physical_performance: 0,
            disk_bytes_per_sector: 0,
            ..sane()
        };
        assert!(alignment.validate().is_ok());
        assert_eq!(
            alignment.granularity(),
            512,
            "an absent source must not drag the granularity below what is reported"
        );
    }

    /// The strictest-wins rule, exercised through each source in turn, so a
    /// dropped term is caught rather than masked by this host's ordering.
    #[test]
    fn granularity_takes_the_strictest_value_from_any_source() {
        let base = Alignment {
            alignment_requirement: 1,
            logical_sector: 512,
            physical_atomicity: 512,
            physical_performance: 512,
            byte_offset_for_sector_alignment: 0,
            disk_bytes_per_sector: 512,
        };
        assert_eq!(base.granularity(), 512);

        for (why, alignment) in [
            (
                "logical",
                Alignment {
                    logical_sector: 8192,
                    ..base
                },
            ),
            (
                "atomicity",
                Alignment {
                    physical_atomicity: 8192,
                    ..base
                },
            ),
            (
                "performance",
                Alignment {
                    physical_performance: 8192,
                    ..base
                },
            ),
            (
                "requirement",
                Alignment {
                    alignment_requirement: 8192,
                    ..base
                },
            ),
            (
                "disk",
                Alignment {
                    disk_bytes_per_sector: 8192,
                    ..base
                },
            ),
        ] {
            assert_eq!(
                alignment.granularity(),
                8192,
                "the {why} source must be able to raise the granularity"
            );
        }
    }

    #[test]
    fn rounding_refuses_to_wrap() {
        let alignment = sane();
        assert!(
            alignment.round_up(usize::MAX).is_err(),
            "rounding up must report overflow rather than wrapping to a short read"
        );
        assert_eq!(alignment.round_up(0).unwrap(), 0);
        assert_eq!(alignment.round_up(1).unwrap(), 4096);
        assert_eq!(alignment.round_up(4096).unwrap(), 4096);
        assert_eq!(alignment.round_up(4097).unwrap(), 8192);
    }

    #[test]
    fn describe_names_every_source() {
        let d = sane().describe();
        for field in [
            "AlignmentRequirement",
            "LogicalBytesPerSector",
            "PhysicalBytesPerSectorForAtomicity",
            "PhysicalBytesPerSectorForPerformance",
            "ByteOffsetForSectorAlignment",
            "GetDiskFreeSpace",
            "using",
        ] {
            assert!(d.contains(field), "describe() omitted {field}: {d}");
        }
    }

    #[test]
    fn an_unaligned_partition_is_reported_in_words() {
        let alignment = Alignment {
            byte_offset_for_sector_alignment: u32::MAX,
            ..sane()
        };
        assert!(
            alignment.describe().contains("not aligned"),
            "u32::MAX is an in-band signal, not a byte count, and must not be printed as one"
        );
    }
}
