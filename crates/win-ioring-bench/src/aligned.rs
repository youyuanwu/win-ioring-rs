//! A buffer whose base address satisfies the unbuffered alignment requirement.
//!
//! A `Vec<u8>` will not do, and the reason is worth stating because it is easy
//! to believe otherwise: the global allocator's guarantee is
//! `align_of::<u8>() == 1`. A `Vec<u8>` sometimes *happens* to land on a sector
//! boundary, which is worse than never doing so — it means the mistake passes
//! on the machine where it was written and fails on the machine where it
//! matters. Taking a subslice destroys any luck involved: `&v[1..4097]` has
//! exactly the alignment its offset gives it.
//!
//! So the allocation is made through [`std::alloc::alloc_zeroed`] with an
//! explicit [`Layout`], which is the supported way to state an alignment.
//!
//! # Why it is zeroed
//!
//! Not for correctness — every byte a scenario reads was written by the read
//! that delivered it. It is so that a bug which reads *past* the delivered
//! length sees zeros rather than whatever the allocator last held there, which
//! makes such a bug reproducible instead of dependent on process history. The
//! cost is paid once when the pool is built, outside the measured region.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::io;

use crate::backend::Buffer;

/// An owned, sector-aligned allocation.
///
/// `len` is what a read delivered; `cap` is what was allocated. The two are
/// distinct because an unbuffered read must be issued for a whole number of
/// sectors even when the caller wants fewer bytes, so the capacity is rounded
/// up while the length stays honest about what arrived.
pub struct AlignedBuf {
    ptr: *mut u8,
    cap: usize,
    align: usize,
    len: usize,
}

// SAFETY: `AlignedBuf` owns its allocation exclusively — there is no shared
// ownership, no interior mutability, and no thread affinity in the allocation
// itself. Moving one to another thread moves the sole owner. This is required
// because the thread-pool backends hand buffers to `spawn_blocking`.
unsafe impl Send for AlignedBuf {}

impl AlignedBuf {
    /// Allocates `cap` bytes rounded up to `align`, aligned to `align`.
    ///
    /// # Errors
    ///
    /// If `align` is not a power of two, if rounding `cap` up would overflow,
    /// or if the allocation fails.
    pub fn new(cap: usize, align: usize) -> io::Result<Self> {
        if !align.is_power_of_two() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("alignment {align} is not a power of two"),
            ));
        }
        // Checked, not merely rounded. An unchecked `div_ceil(align) * align`
        // wraps to 0 for `cap` near `usize::MAX`, and `Layout::from_size_align`
        // accepts a zero size — so the wrap would carry a *safe* caller all the
        // way into `alloc_zeroed` with a zero-sized layout, which is undefined
        // behaviour. The invariant the SAFETY comment below relies on has to be
        // established here rather than assumed.
        let cap = cap
            .max(align)
            .checked_next_multiple_of(align)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("rounding a capacity of {cap} up to a multiple of {align} overflows"),
                )
            })?;
        let layout = Layout::from_size_align(cap, align)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

        // SAFETY: `layout` has non-zero size — `cap >= align >= 1` holds
        // because `align` is a non-zero power of two and the rounding above is
        // checked, so it cannot have wrapped.
        let ptr = unsafe { alloc_zeroed(layout) };
        if ptr.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                format!("could not allocate {cap} bytes aligned to {align}"),
            ));
        }
        Ok(Self {
            ptr,
            cap,
            align,
            len: 0,
        })
    }

    /// The allocation's capacity, which is a whole number of `align` units.
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// The alignment the allocation was made with.
    pub fn alignment(&self) -> usize {
        self.align
    }

    /// True if the base address really satisfies the requested alignment.
    ///
    /// The allocator is required to honour the layout, so this is not a doubt
    /// about the allocator — it is what lets a test assert the property that
    /// the unbuffered read actually depends on, rather than asserting that the
    /// code asked for it.
    pub fn is_aligned(&self) -> bool {
        (self.ptr as usize).is_multiple_of(self.align)
    }

    /// A raw pointer to the base of the allocation.
    pub fn as_ptr(&self) -> *const u8 {
        self.ptr
    }

    /// A raw mutable pointer to the base of the allocation.
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr
    }

    /// Records that a read delivered `len` bytes.
    ///
    /// # Panics
    ///
    /// If `len` exceeds the capacity — which would mean the buffer was handed
    /// to an operation larger than it, and every byte read past the end is
    /// memory this buffer does not own.
    pub fn set_len(&mut self, len: usize) {
        assert!(
            len <= self.cap,
            "delivered length {len} exceeds capacity {}",
            self.cap
        );
        self.len = len;
    }

    /// The bytes a read delivered.
    pub fn filled(&self) -> &[u8] {
        // SAFETY: `len <= cap` is maintained by `set_len`, and the allocation
        // is zeroed at construction, so every byte in range is initialised.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    /// The whole allocation, as a mutable slice.
    pub fn spare(&mut self) -> &mut [u8] {
        // SAFETY: the allocation is `cap` bytes and was zeroed, so every byte
        // in range is initialised and exclusively borrowed through `&mut self`.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.cap) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: `ptr` came from `alloc_zeroed` with exactly this layout, and
        // is deallocated once because `AlignedBuf` is not `Copy` or `Clone`.
        unsafe {
            dealloc(
                self.ptr,
                Layout::from_size_align_unchecked(self.cap, self.align),
            );
        }
    }
}

impl Buffer for AlignedBuf {
    fn bytes(&self) -> &[u8] {
        self.filled()
    }

    fn fill(&mut self, src: &[u8]) -> io::Result<()> {
        if src.len() > self.cap {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "source of {} bytes exceeds capacity {}",
                    src.len(),
                    self.cap
                ),
            ));
        }
        self.spare()[..src.len()].copy_from_slice(src);
        self.len = src.len();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The overflow path is unreachable through the benchmark's own call sites,
    /// which is exactly why it needs a test: `AlignedBuf::new` is a safe public
    /// function, so a caller that reaches it must get an error rather than
    /// undefined behaviour. Before this was checked, the rounding wrapped to
    /// zero and carried a safe caller into `alloc_zeroed` with a zero-sized
    /// layout.
    #[test]
    fn an_overflowing_capacity_is_an_error_not_a_wrap() {
        let err = AlignedBuf::new(usize::MAX, 4096)
            .err()
            .expect("a capacity that cannot be rounded up must be rejected");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn a_non_power_of_two_alignment_is_rejected() {
        assert!(AlignedBuf::new(4096, 0).is_err());
        assert!(AlignedBuf::new(4096, 3).is_err());
        assert!(AlignedBuf::new(4096, 4095).is_err());
    }

    #[test]
    fn capacity_is_rounded_up_and_never_down() {
        for (requested, align, expected) in [
            (1usize, 4096usize, 4096usize),
            (4096, 4096, 4096),
            (4097, 4096, 8192),
            (0, 512, 512),
        ] {
            let buf = AlignedBuf::new(requested, align).expect("allocation failed");
            assert_eq!(buf.capacity(), expected);
            assert!(buf.is_aligned());
        }
    }

    #[test]
    fn a_fresh_buffer_reports_no_delivered_bytes() {
        let buf = AlignedBuf::new(4096, 4096).expect("allocation failed");
        assert!(buf.filled().is_empty());
    }

    #[test]
    #[should_panic(expected = "delivered length")]
    fn a_length_past_the_capacity_panics_rather_than_aliasing() {
        let mut buf = AlignedBuf::new(4096, 4096).expect("allocation failed");
        buf.set_len(4097);
    }

    #[test]
    fn fill_rejects_a_source_larger_than_the_buffer() {
        let mut buf = AlignedBuf::new(4096, 4096).expect("allocation failed");
        assert!(buf.fill(&vec![0u8; 4097]).is_err());
        assert!(buf.fill(&vec![0u8; 4096]).is_ok());
    }

    #[test]
    fn fill_then_bytes_round_trips() {
        let mut buf = AlignedBuf::new(4096, 4096).expect("allocation failed");
        buf.fill(b"the quick brown fox").expect("fill failed");
        assert_eq!(buf.bytes(), b"the quick brown fox");
    }

    /// The allocation is zeroed so that a bug reading past the delivered length
    /// sees zeros rather than whatever the allocator last held, which makes
    /// such a bug reproducible instead of dependent on process history.
    #[test]
    fn the_allocation_starts_zeroed() {
        let mut buf = AlignedBuf::new(4096, 4096).expect("allocation failed");
        assert!(buf.spare().iter().all(|&b| b == 0));
    }
}
