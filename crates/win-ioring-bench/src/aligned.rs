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
    /// If `align` is not a power of two, or the allocation fails.
    pub fn new(cap: usize, align: usize) -> io::Result<Self> {
        if !align.is_power_of_two() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("alignment {align} is not a power of two"),
            ));
        }
        let cap = cap.max(align).div_ceil(align) * align;
        let layout = Layout::from_size_align(cap, align)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

        // SAFETY: `layout` has non-zero size, since `cap >= align >= 1`.
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
