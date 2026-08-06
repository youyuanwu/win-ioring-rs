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
//! alongside the granularity the host reported, so [`Alignment::describe`]
//! renders it for the run's report and `docs/performance.md` publishes it.
//!
//! # The three sources disagree, and that is not a bug
//!
//! Windows exposes at least three answers, and on the development host they
//! were:
//!
//! | source | field | value |
//! |---|---|---|
//! | `FileAlignmentInfo` | `AlignmentRequirement` | 4 bytes |
//! | `FILE_STORAGE_INFO` | `LogicalBytesPerSector` | 512 |
//! | `FILE_STORAGE_INFO` | `PhysicalBytesPerSectorForPerformance` | 4096 |
//! | `GetDiskFreeSpaceW` | `BytesPerSector` | 512 |
//!
//! They disagree because they answer different questions: the *buffer* must
//! meet the device's addressing requirement (4 bytes there), while *length and
//! offset* must be multiples of the sector size (512 there). Deliberate
//! violation confirmed exactly that split — a buffer at `base + 64` succeeded
//! and `base + 1` failed; lengths of 512 and 4096 succeeded and 4095 failed.
//!
//! This module takes the **strictest** of them for every purpose. Over-aligning
//! is always legal, costs a bounded amount of address space, and removes a
//! class of host-specific failure that would otherwise appear only on somebody
//! else's volume. Being fast on the development machine and broken elsewhere is
//! not a trade this crate makes.

use std::io;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    FILE_STORAGE_INFO, FileStorageInfo, GetFileInformationByHandleEx,
};

/// What one volume requires of an unbuffered read.
///
/// Obtained by [`Alignment::query`]. Every field is what the host reported;
/// nothing here is a default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Alignment {
    /// `LogicalBytesPerSector` — the addressable sector size.
    pub logical_sector: u32,
    /// `PhysicalBytesPerSectorForPerformance` — the device's preferred unit.
    pub physical_sector: u32,
}

impl Alignment {
    /// Asks the volume holding `path` what it requires.
    ///
    /// `path` need only identify the volume; the file it names is opened for
    /// metadata alone, so this is safe to call before the benchmark's data file
    /// exists as long as *something* on the volume does.
    pub fn query(path: &Path) -> io::Result<Self> {
        let file = std::fs::File::open(path)?;
        let handle = HANDLE(file.as_raw_handle() as _);

        let mut info = FILE_STORAGE_INFO::default();
        // SAFETY: `handle` is open for the duration of the call, and `info` is a
        // correctly-sized, correctly-typed buffer for `FileStorageInfo`.
        unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileStorageInfo,
                (&raw mut info).cast(),
                size_of::<FILE_STORAGE_INFO>() as u32,
            )
        }?;

        let alignment = Self {
            logical_sector: info.LogicalBytesPerSector,
            physical_sector: info.PhysicalBytesPerSectorForPerformance,
        };
        alignment.validate()?;
        Ok(alignment)
    }

    /// Rejects a report this module cannot safely build on.
    ///
    /// A zero or non-power-of-two sector size would make [`Self::round_up`]
    /// meaningless and `std::alloc::Layout` reject the alignment, and it would
    /// do so far from here. Failing at the query is the legible place to fail.
    fn validate(&self) -> io::Result<()> {
        for (name, value) in [
            ("LogicalBytesPerSector", self.logical_sector),
            ("PhysicalBytesPerSectorForPerformance", self.physical_sector),
        ] {
            if value == 0 || !value.is_power_of_two() {
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
    pub fn granularity(&self) -> usize {
        self.logical_sector.max(self.physical_sector) as usize
    }

    /// Rounds `n` up to the next multiple of [`Self::granularity`].
    pub fn round_up(&self, n: usize) -> usize {
        let g = self.granularity();
        n.div_ceil(g) * g
    }

    /// True if `n` is a legal length or offset for an unbuffered read.
    pub fn is_aligned(&self, n: u64) -> bool {
        n.is_multiple_of(self.granularity() as u64)
    }

    /// What the host said, for the run's report and for `docs/performance.md`.
    ///
    /// R1.4: a reader on a different volume cannot interpret these timings
    /// without knowing the granularity they were measured under.
    pub fn describe(&self) -> String {
        format!(
            "logical sector {} B, physical sector {} B, using {} B",
            self.logical_sector,
            self.physical_sector,
            self.granularity()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`Alignment::validate`] only fires on a host that reports something
    /// unusable, which cannot be provoked by querying this machine. Constructing
    /// the struct directly is the only way to reach it — and without this test
    /// the check is unverified, which was established by deleting it and
    /// watching every other test still pass.
    #[test]
    fn a_nonsensical_report_is_rejected() {
        for (logical, physical) in [(0, 4096), (4096, 0), (513, 4096), (512, 4095)] {
            let alignment = Alignment {
                logical_sector: logical,
                physical_sector: physical,
            };
            assert!(
                alignment.validate().is_err(),
                "logical {logical}, physical {physical} should be rejected: \
                 a zero or non-power-of-two sector size makes round_up meaningless \
                 and Layout::from_size_align reject the alignment, far from here"
            );
        }
    }

    #[test]
    fn a_sane_report_is_accepted() {
        let alignment = Alignment {
            logical_sector: 512,
            physical_sector: 4096,
        };
        assert!(alignment.validate().is_ok());
        assert_eq!(alignment.granularity(), 4096, "the strictest value is used");
    }

    /// The strictest-wins rule, in both directions, so a reversed `max` is
    /// caught rather than being masked by this host's ordering.
    #[test]
    fn granularity_takes_the_strictest_value() {
        assert_eq!(
            Alignment {
                logical_sector: 4096,
                physical_sector: 512
            }
            .granularity(),
            4096
        );
        assert_eq!(
            Alignment {
                logical_sector: 512,
                physical_sector: 4096
            }
            .granularity(),
            4096
        );
    }
}
