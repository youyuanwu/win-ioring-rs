//! A buffer that reports when it is dropped.
//!
//! Some of this crate's guarantees are about memory *not* being freed — a
//! superseded registration stays alive, an unquiet teardown abandons rather
//! than releases. Nothing observable happens in those cases, so without a way
//! to count drops a test can only assert that no call returned an error, which
//! is exactly the kind of test that passes while proving nothing.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use win_ioring::{IoBuf, IoBufMut};

/// A `Vec<u8>` that increments a shared counter when dropped.
#[derive(Debug)]
pub struct CountingBuf {
    bytes: Vec<u8>,
    drops: Arc<AtomicUsize>,
}

impl CountingBuf {
    /// Creates a buffer of `capacity` bytes that reports drops to `drops`.
    pub fn new(capacity: usize, drops: &Arc<AtomicUsize>) -> Self {
        Self {
            bytes: Vec::with_capacity(capacity),
            drops: Arc::clone(drops),
        }
    }
}

impl Drop for CountingBuf {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

// SAFETY: the pointer and lengths come from the inner `Vec`, which nothing here
// grows, so its allocation never moves; and the buffer has no interior
// mutability, so the bytes cannot change under a slice handed to the kernel.
unsafe impl IoBuf for CountingBuf {
    fn buf_ptr(&self) -> *const u8 {
        self.bytes.as_ptr()
    }
    fn buf_len(&self) -> usize {
        self.bytes.len()
    }
}

// SAFETY: as above. `set_buf_init` only ever reports bytes the kernel wrote
// into the `Vec`'s existing capacity.
unsafe impl IoBufMut for CountingBuf {
    fn buf_mut_ptr(&mut self) -> *mut u8 {
        self.bytes.as_mut_ptr()
    }
    fn buf_capacity(&self) -> usize {
        self.bytes.capacity()
    }
    unsafe fn set_buf_init(&mut self, len: usize) {
        // SAFETY: the caller guarantees `len` bytes are initialized.
        unsafe { self.bytes.set_len(len) }
    }
}
