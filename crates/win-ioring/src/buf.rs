//! Owned buffer contracts for IoRing operations.
//!
//! Completion-based I/O hands a raw pointer to the kernel and gets it back
//! later. The buffer must therefore stay alive, and stay at the same address,
//! for as long as the kernel might touch it — which can outlive the future the
//! caller is awaiting. This crate solves that by taking **ownership** of the
//! buffer for the duration of an operation and returning it to the caller
//! afterwards, rather than borrowing it.
//!
//! [`IoBuf`] describes a buffer an operation can read *from*; [`IoBufMut`]
//! describes one an operation can write *into*. Both are `unsafe` to implement
//! because the crate relies on their reported pointer and extents being
//! truthful and stable.
//!
//! # Who guarantees stability
//!
//! An implementor does **not** have to promise that its pointer survives being
//! moved — `[u8; N]` could never honour that, since its bytes move with the
//! value. Instead the crate places the buffer in a stable heap allocation
//! before taking its address, and does not move it again until the operation
//! reaches terminal completion. The implementor's obligation covers the window
//! between those two points: while the buffer is in flight, nothing it exposes
//! may invalidate the pointer or extents the driver observed, and no other
//! alias may touch the bytes the kernel is using.
//!
//! # Transfer counts
//!
//! Some buffer types track an initialized length separately from their capacity
//! (`Vec<u8>`), and some are wholly initialized by construction (`[u8; N]`,
//! `Box<[u8]>`). A read into the latter cannot shrink the buffer to the number
//! of bytes actually transferred, so the transfer count is always reported by
//! the operation's result. For length-tracking types the initialized length is
//! additionally updated to match.

use crate::error::{Error, Result};

/// A buffer an operation can read from.
///
/// # Safety
///
/// This trait is unsafe to implement because the driver hands the reported
/// pointer to the kernel and relies on it remaining correct for the whole time
/// the kernel may touch it. That window — from the moment the crate places the
/// buffer in its operation storage until the operation reaches terminal
/// completion — is called the *in-flight window* below.
///
/// An implementor cannot observe that window's exact boundaries, and does not
/// need to: the crate takes the buffer by value when an operation starts and
/// hands it back when the operation is over, so upholding these obligations
/// from the moment ownership is transferred until the moment it is returned is
/// always sufficient.
///
/// Implementors must guarantee all of the following.
///
/// **Validity.** [`IoBuf::buf_ptr`] returns a pointer that is non-null,
/// suitably aligned for `u8`, and valid for reads of [`IoBuf::buf_len`] bytes.
/// Those bytes are initialized and lie within a single allocated object, and
/// `buf_len` never exceeds `isize::MAX`. These requirements hold even when
/// `buf_len` is zero.
///
/// **Coherence.** This obligation applies at all times, not only in flight.
/// The type must not use interior mutability with respect to the buffer at
/// all: neither its storage, pointer, extents, **nor the buffer's bytes** may
/// be modified through a shared reference. Given any shared borrow,
/// [`IoBuf::buf_ptr`] and [`IoBuf::buf_len`] must describe the same buffer, so
/// that reading one and then the other cannot observe a torn view, and the
/// bytes must not change for as long as a slice obtained from
/// [`IoBuf::as_io_slice`] is alive. A `Cell`- or `Mutex`-backed byte store is
/// therefore not a valid implementation. Everything about the buffer may change
/// only through a `&mut` borrow, and only outside the in-flight window.
///
/// **Stability.** For the whole in-flight window the pointer and length must
/// not change. In particular, calling *any* method of this trait or of
/// [`IoBufMut`] — including [`IoBufMut::buf_mut_ptr`], which takes `&mut self`
/// — must not reallocate, shrink, or otherwise invalidate what a previous call
/// reported. The only method permitted to change the reported length is
/// [`IoBufMut::set_buf_init`], and the crate calls it only after terminal
/// completion.
///
/// **Exclusivity.** For the whole in-flight window the buffer's bytes must not
/// be read or written through any other alias. The kernel may be reading or
/// writing them concurrently, so a container that shares its storage — for
/// instance one that hands out a second view of the same allocation — must not
/// permit such access while an operation is outstanding.
///
/// The `'static` bound exists because a buffer outlives the future that
/// submitted it: the driver retains it until the kernel reports completion,
/// which may be after the future was dropped. The [`Unpin`] bound exists
/// because the crate moves the buffer into its own storage when an operation
/// starts and moves it back out when the operation ends; a buffer that cannot
/// be moved at all could never make that round trip. Note that this does not
/// weaken the stability guarantee: the crate keeps the buffer still for the
/// whole in-flight window, and only moves it at the boundaries.
pub unsafe trait IoBuf: 'static + Unpin {
    /// Returns a pointer to the first byte.
    fn buf_ptr(&self) -> *const u8;

    /// Returns the number of initialized bytes that may be read.
    fn buf_len(&self) -> usize;

    /// Returns the initialized bytes as a slice.
    fn as_io_slice(&self) -> &[u8] {
        let len = self.buf_len();
        if len == 0 {
            // Avoid constructing a slice from a pointer at all in the empty
            // case, so that a zero-length buffer can never trip the validity
            // requirements of `from_raw_parts`.
            return &[];
        }
        // SAFETY: the validity contract guarantees `buf_ptr` is non-null,
        // aligned, and valid for reads of `len` initialized bytes within a
        // single allocation, with `len <= isize::MAX`. The coherence contract
        // guarantees the length read above still describes the pointer read
        // here, and that the bytes cannot be mutated through a shared reference
        // while the returned slice is alive, since interior mutability of the
        // buffer is forbidden outright.
        unsafe { std::slice::from_raw_parts(self.buf_ptr(), len) }
    }
}

/// A buffer an operation can write into.
///
/// # Safety
///
/// In addition to every obligation of [`IoBuf`], including the stability and
/// exclusivity rules for the in-flight window, implementors must guarantee:
///
/// **Validity.** [`IoBufMut::buf_mut_ptr`] returns a pointer that is non-null,
/// suitably aligned for `u8`, and valid for writes of
/// [`IoBufMut::buf_capacity`] bytes within a single allocated object, with
/// `buf_capacity` no greater than `isize::MAX`.
///
/// **Consistency.** `buf_capacity` is always at least `buf_len`, and
/// `buf_mut_ptr` reports the same address as [`IoBuf::buf_ptr`].
pub unsafe trait IoBufMut: IoBuf {
    /// Returns a mutable pointer to the first byte.
    fn buf_mut_ptr(&mut self) -> *mut u8;

    /// Returns the total number of bytes that may be written.
    fn buf_capacity(&self) -> usize;

    /// Records that the first `len` bytes are now initialized.
    ///
    /// Types that cannot track an initialized length separately from their
    /// capacity may ignore this; the authoritative transfer count is always the
    /// one reported by the operation's result.
    ///
    /// # Safety
    ///
    /// `len` must not exceed [`IoBufMut::buf_capacity`], and the first `len`
    /// bytes must genuinely have been initialized.
    unsafe fn set_buf_init(&mut self, len: usize);
}

// SAFETY: `Vec<u8>` keeps its bytes in a single stable heap allocation whose
// address is non-null and aligned, and whose first `len` bytes are initialized.
// It has no interior mutability, so a shared borrow always sees a coherent
// pointer and length. None of the accessors below reallocate, so the pointer
// and extents stay put for as long as the crate holds the value still. A `Vec`
// owns its allocation exclusively, so no other alias can reach the bytes.
unsafe impl IoBuf for Vec<u8> {
    fn buf_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    fn buf_len(&self) -> usize {
        self.len()
    }
}

// SAFETY: `capacity` bytes are writable from `as_mut_ptr`, which reports the
// same address as `as_ptr` and never reallocates. Capacity is always at least
// `len`, and `set_len` is the intended way to record initialization.
unsafe impl IoBufMut for Vec<u8> {
    fn buf_mut_ptr(&mut self) -> *mut u8 {
        self.as_mut_ptr()
    }

    fn buf_capacity(&self) -> usize {
        self.capacity()
    }

    unsafe fn set_buf_init(&mut self, len: usize) {
        debug_assert!(len <= self.capacity());
        // SAFETY: the caller guarantees `len` bytes are initialized and that
        // `len` does not exceed the capacity.
        unsafe { self.set_len(len) };
    }
}

// SAFETY: a boxed slice owns a single stable heap allocation, non-null and
// aligned, whose bytes are all initialized. It has no interior mutability, and
// its length is fixed, so no accessor can change the extents and a shared
// borrow always sees a coherent view. Ownership is exclusive.
unsafe impl IoBuf for Box<[u8]> {
    fn buf_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    fn buf_len(&self) -> usize {
        self.len()
    }
}

// SAFETY: every byte of a boxed slice is writable, its length is fixed so
// capacity equals length, and `as_mut_ptr` reports the same address as
// `as_ptr`.
unsafe impl IoBufMut for Box<[u8]> {
    fn buf_mut_ptr(&mut self) -> *mut u8 {
        self.as_mut_ptr()
    }

    fn buf_capacity(&self) -> usize {
        self.len()
    }

    unsafe fn set_buf_init(&mut self, _len: usize) {
        // A boxed slice has a fixed length and is initialized throughout, so
        // there is no separate initialization watermark to record.
    }
}

// SAFETY: an array's bytes are initialized by construction, live in a single
// object, and are readable for its full length. It has no interior mutability
// and its length is a constant, so no accessor can change the extents and a
// shared borrow always sees a coherent view.
unsafe impl<const N: usize> IoBuf for [u8; N] {
    fn buf_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    fn buf_len(&self) -> usize {
        N
    }
}

// SAFETY: every byte of the array is writable, its length is a constant so
// capacity equals length, and `as_mut_ptr` reports the same address as
// `as_ptr`.
unsafe impl<const N: usize> IoBufMut for [u8; N] {
    fn buf_mut_ptr(&mut self) -> *mut u8 {
        self.as_mut_ptr()
    }

    fn buf_capacity(&self) -> usize {
        N
    }

    unsafe fn set_buf_init(&mut self, _len: usize) {
        // An array has a fixed length and is initialized throughout.
    }
}

/// The outcome of an operation together with the buffer it borrowed.
///
/// Completion-based I/O takes ownership of the caller's buffer, so it has to
/// give it back. `BufResult` pairs the operation's result with that buffer, and
/// does so on both success and failure.
#[derive(Debug)]
pub struct BufResult<T, B> {
    /// The operation's result.
    pub result: Result<T>,
    /// The caller's buffer, returned regardless of the result.
    pub buffer: B,
}

impl<T, B> BufResult<T, B> {
    /// Pairs a result with the buffer it used.
    pub fn new(result: Result<T>, buffer: B) -> Self {
        Self { result, buffer }
    }

    /// Splits into the result and the buffer.
    pub fn into_parts(self) -> (Result<T>, B) {
        (self.result, self.buffer)
    }

    /// Returns `true` if the operation succeeded.
    pub fn is_ok(&self) -> bool {
        self.result.is_ok()
    }

    /// Maps the success value, leaving the buffer untouched.
    pub fn map<U, F: FnOnce(T) -> U>(self, f: F) -> BufResult<U, B> {
        BufResult {
            result: self.result.map(f),
            buffer: self.buffer,
        }
    }

    /// Returns the success value and the buffer, panicking on failure.
    ///
    /// # Panics
    ///
    /// Panics if the operation failed.
    pub fn expect(self, msg: &str) -> (T, B) {
        match self.result {
            Ok(v) => (v, self.buffer),
            Err(e) => panic!("{msg}: {e}"),
        }
    }

    /// Returns the success value and the buffer, panicking on failure.
    ///
    /// # Panics
    ///
    /// Panics if the operation failed.
    pub fn unwrap(self) -> (T, B) {
        self.expect("operation failed")
    }
}

/// Rejects a read that would write past the buffer's capacity.
///
/// Exposed so that callers building operations against the raw layer can apply
/// the same rule the safe layer applies.
///
/// # Errors
///
/// Returns [`Error::BufferTooSmall`] if `requested` exceeds the capacity.
pub fn check_read_capacity<B: IoBufMut>(buffer: &B, requested: u64) -> Result<()> {
    let available = buffer.buf_capacity() as u64;
    if requested > available {
        Err(Error::BufferTooSmall {
            requested,
            available,
        })
    } else {
        Ok(())
    }
}

/// Rejects a write that would read past the buffer's initialized bytes.
///
/// Sending uninitialized memory to the kernel is never permitted, so the check
/// is against the initialized length rather than the capacity. Exposed so that
/// callers building operations against the raw layer can apply the same rule
/// the safe layer applies.
///
/// # Errors
///
/// Returns [`Error::UninitializedWriteRange`] if `requested` exceeds the
/// initialized length.
pub fn check_write_initialized<B: IoBuf>(buffer: &B, requested: u64) -> Result<()> {
    let initialized = buffer.buf_len() as u64;
    if requested > initialized {
        Err(Error::UninitializedWriteRange {
            requested,
            initialized,
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vec_reports_initialized_length_not_capacity() {
        let mut v: Vec<u8> = Vec::with_capacity(64);
        v.extend_from_slice(b"abc");
        assert_eq!(v.buf_len(), 3);
        assert!(v.buf_capacity() >= 64);
        assert_eq!(v.as_io_slice(), b"abc");
    }

    #[test]
    fn vec_records_transferred_length() {
        let mut v: Vec<u8> = Vec::with_capacity(16);
        // Simulate a completion writing 5 bytes into the spare capacity.
        unsafe {
            std::ptr::write_bytes(v.buf_mut_ptr(), b'x', 5);
            v.set_buf_init(5);
        }
        assert_eq!(v.buf_len(), 5);
        assert_eq!(v.as_io_slice(), b"xxxxx");
    }

    #[test]
    fn boxed_slice_is_fully_initialized() {
        let mut b: Box<[u8]> = vec![7_u8; 8].into_boxed_slice();
        assert_eq!(b.buf_len(), 8);
        assert_eq!(b.buf_capacity(), 8);
        // Fixed-length buffers cannot shrink to the transferred count; the
        // count is carried by the operation result instead.
        unsafe { b.set_buf_init(3) };
        assert_eq!(b.buf_len(), 8);
    }

    #[test]
    fn array_is_fully_initialized() {
        let mut a = [0_u8; 4];
        assert_eq!(a.buf_len(), 4);
        assert_eq!(a.buf_capacity(), 4);
        unsafe { a.set_buf_init(1) };
        assert_eq!(a.buf_len(), 4);
    }

    /// All three standard container types must work with no caller-written
    /// adapter, through both contracts where applicable.
    #[test]
    fn standard_containers_satisfy_the_contracts() {
        fn read_target<B: IoBufMut>(mut b: B) -> usize {
            let _ = b.buf_mut_ptr();
            b.buf_capacity()
        }
        fn write_source<B: IoBuf>(b: B) -> usize {
            let _ = b.buf_ptr();
            b.buf_len()
        }

        assert!(read_target(vec![0_u8; 4]) >= 4);
        assert_eq!(read_target(vec![0_u8; 4].into_boxed_slice()), 4);
        assert_eq!(read_target([0_u8; 4]), 4);

        assert_eq!(write_source(vec![1_u8, 2, 3]), 3);
        assert_eq!(write_source(vec![1_u8; 3].into_boxed_slice()), 3);
        assert_eq!(write_source([1_u8; 3]), 3);
    }

    #[test]
    fn read_over_capacity_is_rejected() {
        let v = vec![0_u8; 4];
        let err = check_read_capacity(&v, (v.capacity() + 1) as u64)
            .err()
            .unwrap();
        assert!(matches!(err, Error::BufferTooSmall { .. }));
        check_read_capacity(&v, 4).unwrap();
    }

    /// A write must never source bytes the caller has not initialized, even
    /// when those bytes are within the allocation's capacity.
    #[test]
    fn write_past_initialized_bytes_is_rejected() {
        let mut v: Vec<u8> = Vec::with_capacity(64);
        v.extend_from_slice(b"abc");
        let err = check_write_initialized(&v, 4).err().unwrap();
        match err {
            Error::UninitializedWriteRange {
                requested,
                initialized,
            } => {
                assert_eq!(requested, 4);
                assert_eq!(initialized, 3);
            }
            other => panic!("expected UninitializedWriteRange, got {other:?}"),
        }
        check_write_initialized(&v, 3).unwrap();
    }

    /// A zero-length buffer must produce an empty slice without forming one
    /// from a pointer, so the validity requirements of `from_raw_parts` can
    /// never be tripped by an empty container.
    #[test]
    fn zero_length_buffers_yield_an_empty_slice() {
        let v: Vec<u8> = Vec::new();
        assert_eq!(v.buf_len(), 0);
        assert!(v.as_io_slice().is_empty());

        let b: Box<[u8]> = Vec::new().into_boxed_slice();
        assert!(b.as_io_slice().is_empty());

        let a: [u8; 0] = [];
        assert!(a.as_io_slice().is_empty());
    }

    /// A write of nothing is legal regardless of how little is initialized.
    #[test]
    fn zero_length_transfers_pass_the_bounds_checks() {
        let v: Vec<u8> = Vec::new();
        check_write_initialized(&v, 0).unwrap();
        check_read_capacity(&v, 0).unwrap();
    }

    #[test]
    fn buf_result_returns_the_buffer_on_success_and_failure() {
        let ok: BufResult<usize, Vec<u8>> = BufResult::new(Ok(3), vec![1, 2, 3]);
        assert!(ok.is_ok());
        let (r, b) = ok.into_parts();
        assert_eq!(r.unwrap(), 3);
        assert_eq!(b, vec![1, 2, 3]);

        let err: BufResult<usize, Vec<u8>> = BufResult::new(Err(Error::QueueFull), vec![9]);
        assert!(!err.is_ok());
        let (r, b) = err.into_parts();
        assert!(matches!(r.unwrap_err(), Error::QueueFull));
        assert_eq!(b, vec![9], "buffer must come back even on failure");
    }

    #[test]
    fn buf_result_map_preserves_the_buffer() {
        let r: BufResult<usize, Vec<u8>> = BufResult::new(Ok(2), vec![4, 5]);
        let mapped = r.map(|n| n * 10);
        assert_eq!(mapped.result.unwrap(), 20);
        assert_eq!(mapped.buffer, vec![4, 5]);
    }
}
