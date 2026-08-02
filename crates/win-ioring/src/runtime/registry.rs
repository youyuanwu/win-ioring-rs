//! Registered buffers, and the handles an application checks out of them.
//!
//! The platform can map a set of buffers once and then let operations name one
//! by index, avoiding the per-operation cost of describing it. What it cannot do
//! is give the memory back: there is no unregister entry point, so a
//! registration lasts until the ring closes.
//!
//! That makes the obvious design — move the buffers into the driver and address
//! them by index — a dead end for the application, which can then never read
//! what arrived. This module takes the approach `tokio-uring` established
//! instead. The registration is a *collection* the application checks buffers
//! out of, receiving a [`RegisteredBuf`] handle that:
//!
//! - dereferences to the bytes, so a read's result is directly readable;
//! - satisfies the crate's buffer contract, so it flows through operations and
//!   comes back the same way an ordinary owned buffer does;
//! - returns to the collection when dropped.
//!
//! Exactly one handle to a given buffer exists at a time. That is what keeps an
//! operation in flight and the application from reaching the same bytes at once,
//! and it is enforced by ownership rather than by documentation: an operation
//! takes the handle by value, so the application cannot name it until it comes
//! back.
//!
//! # Lifetime
//!
//! A registration's memory has three potential claimants — the driver, an
//! outstanding handle, and the platform — and must outlive whichever ends last.
//! [`RegistryInner`] is therefore shared by `Rc`: the driver holds one, each
//! handle holds one, and the allocation goes away only when the last is dropped.
//! A handle held across ring closure still addresses live memory.

use std::cell::RefCell;
use std::ops::{Deref, DerefMut};
use std::rc::{Rc, Weak};

use crate::error::{Error, Result};

/// The per-buffer bookkeeping a registration keeps.
///
/// Split out from the buffers themselves because it is the only part behind the
/// [`RefCell`]. See the coherence note on [`RegistryInner`].
#[derive(Debug)]
struct Slot {
    /// The registered extent, in bytes. Fixed for the registration's life.
    extent: usize,
    /// How many leading bytes are initialized.
    ///
    /// A single contiguous prefix, never a set of ranges: a transfer landing
    /// past this mark does not extend it, because doing so would vouch for the
    /// uninitialized gap in front of it.
    initialized: usize,
    /// Whether a handle to this buffer currently exists.
    checked_out: bool,
}

/// A registration's storage and bookkeeping.
///
/// # Coherence
///
/// The buffer *allocations* are held as opaque `Box<dyn Any>` and are **never
/// reborrowed here** after registration — nothing in this module reads or writes
/// their bytes. Only the bookkeeping sits behind the [`RefCell`]. So the bytes
/// are not reachable through the cell, and a [`RegisteredBuf`]'s `DerefMut` is
/// the sole `&mut` path to them, which is what lets `RegisteredBuf` satisfy the
/// buffer contract's coherence and exclusivity clauses.
pub struct RegistryInner {
    /// The caller's buffers, boxed so their addresses are stable, retained for
    /// as long as anything can still reach them. Never reborrowed.
    _buffers: Vec<Box<dyn std::any::Any>>,
    /// The base address of each buffer, as recorded in the descriptor handed to
    /// the platform.
    ///
    /// Taken from the descriptor rather than re-derived from `_buffers`:
    /// re-deriving would require a `&mut` borrow of a box the platform already
    /// holds a pointer into.
    pointers: Vec<*mut u8>,
    /// Per-buffer bookkeeping.
    slots: RefCell<Vec<Slot>>,
    /// The driver this registration belongs to.
    ///
    /// Weak on purpose: a strong reference from a leaked handle would make the
    /// driver unreachable but alive, silently disabling the abort in
    /// `Drop for DriverInner` that catches a ring left open.
    driver: Weak<RefCell<super::DriverInner>>,
    /// The descriptor array handed to the platform, kept at a stable address for
    /// as long as the registration can be reached.
    _descriptors: Box<[crate::io_ring::BufferInfo]>,
}

impl std::fmt::Debug for RegistryInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryInner")
            .field("len", &self.pointers.len())
            .field("slots", &self.slots)
            .finish_non_exhaustive()
    }
}

impl RegistryInner {
    /// Builds a registration from its parts.
    ///
    /// `pointers` must be the addresses recorded in the descriptors handed to
    /// the platform, and `extents` their registered lengths.
    pub(crate) fn new(
        buffers: Vec<Box<dyn std::any::Any>>,
        pointers: Vec<*mut u8>,
        extents: Vec<usize>,
        initialized: Vec<usize>,
        descriptors: Box<[crate::io_ring::BufferInfo]>,
        driver: Weak<RefCell<super::DriverInner>>,
    ) -> Rc<Self> {
        debug_assert_eq!(buffers.len(), pointers.len());
        debug_assert_eq!(buffers.len(), extents.len());
        debug_assert_eq!(buffers.len(), initialized.len());
        let slots = extents
            .into_iter()
            .zip(initialized)
            .map(|(extent, initialized)| Slot {
                extent,
                initialized: initialized.min(extent),
                checked_out: false,
            })
            .collect();
        Rc::new(Self {
            _buffers: buffers,
            pointers,
            slots: RefCell::new(slots),
            driver,
            _descriptors: descriptors,
        })
    }

    /// Returns whether any buffer is currently checked out, and which.
    ///
    /// The driver consults this before accepting a new registration: a handle
    /// outstanding when one is adopted would be left naming a set the platform
    /// no longer resolves against.
    pub(crate) fn any_checked_out(&self) -> Option<u32> {
        self.slots
            .borrow()
            .iter()
            .position(|s| s.checked_out)
            .map(|i| i as u32)
    }

    /// Returns how many buffers the registration holds.
    pub(crate) fn len(&self) -> usize {
        self.pointers.len()
    }

    /// Returns a buffer's registered extent, if the index names one.
    pub(crate) fn extent(&self, index: u32) -> Option<usize> {
        self.slots.borrow().get(index as usize).map(|s| s.extent)
    }

    /// Returns how many of a buffer's leading bytes are initialized.
    pub(crate) fn initialized(&self, index: u32) -> Option<usize> {
        self.slots
            .borrow()
            .get(index as usize)
            .map(|s| s.initialized)
    }

    /// Raises a buffer's initialized prefix to cover a completed transfer.
    ///
    /// `start` is where the transfer began within the buffer. The mark is raised
    /// only when the transfer began at or before it: a transfer landing further
    /// on leaves a gap of genuinely uninitialized bytes in front, and raising the
    /// mark over that gap would falsely vouch for it.
    ///
    /// Called by the driver when an operation reports, whether or not a future
    /// is waiting for it.
    pub(crate) fn raise_initialized(&self, index: u32, start: usize, transferred: usize) {
        let mut slots = self.slots.borrow_mut();
        let Some(slot) = slots.get_mut(index as usize) else {
            return;
        };
        if start > slot.initialized {
            return;
        }
        let filled = start.saturating_add(transferred).min(slot.extent);
        slot.initialized = slot.initialized.max(filled);
    }

    /// Sets a buffer's initialized prefix outright.
    ///
    /// Unlike [`RegistryInner::raise_initialized`] this may lower the count,
    /// which only narrows what a later write may source and is therefore always
    /// safe. It is the path an ordinary operation's `set_buf_init` takes.
    fn set_initialized(&self, index: u32, len: usize) {
        let mut slots = self.slots.borrow_mut();
        if let Some(slot) = slots.get_mut(index as usize) {
            slot.initialized = len.min(slot.extent);
        }
    }

    /// Claims a buffer, refusing if the index is unknown or already claimed.
    ///
    /// The registry-local half of checkout. The driver-consulting guards —
    /// shutdown, a registration in flight, and supersession — wrap this in
    /// [`RegisteredBuffers::check_out`].
    fn claim(self: &Rc<Self>, index: u32) -> Result<RegisteredBuf> {
        let mut slots = self.slots.borrow_mut();
        let slot = slots
            .get_mut(index as usize)
            .ok_or(Error::InvalidRegisteredIndex { index })?;
        if slot.checked_out {
            return Err(Error::BufferCheckedOut { index });
        }
        slot.checked_out = true;
        let (extent, initialized) = (slot.extent, slot.initialized);
        drop(slots);

        Ok(RegisteredBuf {
            registry: Rc::clone(self),
            index,
            ptr: self.pointers[index as usize],
            extent,
            initialized,
        })
    }

    /// Returns a buffer to the collection.
    fn release(&self, index: u32) {
        if let Some(slot) = self.slots.borrow_mut().get_mut(index as usize) {
            slot.checked_out = false;
        }
    }
}

/// A set of buffers registered with the platform.
///
/// Obtained from a successful registration. Cloning produces another reference
/// to the same collection, not another registration.
///
/// Buffers are taken out one at a time with [`RegisteredBuffers::check_out`] and
/// return when their handle is dropped.
#[derive(Clone, Debug)]
pub struct RegisteredBuffers {
    inner: Rc<RegistryInner>,
}

impl RegisteredBuffers {
    /// Wraps a registration.
    pub(crate) fn from_inner(inner: Rc<RegistryInner>) -> Self {
        Self { inner }
    }

    /// Returns the shared registration, for this module's own tests.
    #[cfg(test)]
    pub(crate) fn inner(&self) -> &Rc<RegistryInner> {
        &self.inner
    }

    /// Returns how many buffers the registration holds.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns `true` if the registration holds no buffers.
    ///
    /// Always `false` in practice: an empty registration is refused before it
    /// reaches the platform.
    pub fn is_empty(&self) -> bool {
        self.inner.len() == 0
    }

    /// Takes the buffer at `index`, if it is not already taken.
    ///
    /// # Errors
    ///
    /// Refusals are checked in this order, because more than one can hold at
    /// once and each names a different situation:
    ///
    /// - [`Error::ShuttingDown`] if the driver is gone or shutting down.
    /// - [`Error::RegistrationPending`] if a registration request is in flight.
    ///   One is adopted when it completes rather than when it is requested, so a
    ///   handle taken in that window would name a set about to be superseded.
    /// - [`Error::RegistrationSuperseded`] if this collection's registration is
    ///   no longer the driver's current one. Its buffers stay valid for any
    ///   handle still holding them, but it yields no more.
    /// - [`Error::InvalidRegisteredIndex`] if the registration has no such
    ///   buffer.
    /// - [`Error::BufferCheckedOut`] if a handle to it already exists. A buffer
    ///   held by an operation returns when that operation reports, which may be
    ///   later than the point its future was dropped.
    pub fn check_out(&self, index: u32) -> Result<RegisteredBuf> {
        let driver = self.inner.driver.upgrade().ok_or(Error::ShuttingDown)?;
        {
            let driver = driver.borrow();
            if driver.torn_down || driver.shutdown != super::ShutdownMode::Running {
                return Err(Error::ShuttingDown);
            }
            if driver.registration_in_flight {
                return Err(Error::RegistrationPending);
            }
            match driver.buffer_registration.as_ref() {
                Some(current) if Rc::ptr_eq(current, &self.inner) => {}
                _ => return Err(Error::RegistrationSuperseded),
            }
        }
        self.inner.claim(index)
    }
}

/// Exclusive access to one registered buffer.
///
/// Dereferences to the buffer's initialized bytes, so an operation's result can
/// be read directly. Satisfies the crate's buffer contract, so it can be handed
/// to an operation and comes back the same way an owned buffer does. Returns to
/// its collection when dropped.
///
/// The bytes stay valid for as long as this handle exists, including after the
/// driver that registered them is gone.
#[derive(Debug)]
pub struct RegisteredBuf {
    registry: Rc<RegistryInner>,
    index: u32,
    /// The buffer's base address, taken from the descriptor at registration.
    ptr: *mut u8,
    /// The registered extent. Fixed for the registration's life.
    extent: usize,
    /// A read-through copy of the registry's initialized count.
    ///
    /// Cached so the buffer-contract methods take no borrow of the registry's
    /// cell, which also discharges the contract's stability clause: the driver
    /// raising the registry's count cannot change what an in-flight handle
    /// reports.
    initialized: usize,
}

impl RegisteredBuf {
    /// Returns the index this buffer occupies in its registration.
    pub fn index(&self) -> u32 {
        self.index
    }

    /// Returns the registered extent, which is the most this buffer can hold.
    pub fn capacity(&self) -> usize {
        self.extent
    }

    /// Returns the registration this buffer belongs to.
    pub(crate) fn registry(&self) -> &Rc<RegistryInner> {
        &self.registry
    }

    /// Re-reads the initialized count from the registration.
    ///
    /// Used when an operation hands the handle back, since the driver updated
    /// the registration while the handle was in flight.
    pub(crate) fn refresh(&mut self) {
        if let Some(initialized) = self.registry.initialized(self.index) {
            self.initialized = initialized;
        }
    }

    /// Copies `src` into the buffer, starting at its first byte.
    ///
    /// Always from byte zero: filling from anywhere else would leave a gap of
    /// uninitialized bytes in front, which the initialized prefix cannot
    /// describe.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BufferTooSmall`] if `src` is longer than the registered
    /// extent.
    pub fn fill(&mut self, src: &[u8]) -> Result<()> {
        if src.len() > self.extent {
            return Err(Error::BufferTooSmall {
                requested: src.len() as u64,
                available: self.extent as u64,
            });
        }
        if !src.is_empty() {
            // SAFETY: `ptr` is the registered base address, valid for writes of
            // `extent` bytes, and `src.len() <= extent` was just checked. `src`
            // is a distinct allocation from the registration's buffers, which
            // nothing else in this module reads or writes.
            unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), self.ptr, src.len()) };
        }
        self.initialized = src.len();
        self.registry.set_initialized(self.index, src.len());
        Ok(())
    }
}

impl Drop for RegisteredBuf {
    /// Returns the buffer to its collection.
    ///
    /// Touches only the registration, never the driver. This can run while the
    /// driver is mutably borrowed — when a payload is dropped during completion
    /// reaping, or during teardown — so reaching the driver from here would
    /// panic. Checkout is the only registry operation that consults the driver.
    fn drop(&mut self) {
        self.registry.release(self.index);
    }
}

impl Deref for RegisteredBuf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        if self.initialized == 0 {
            return &[];
        }
        // SAFETY: `ptr` is the registered base address, valid for reads of
        // `extent` bytes, and `initialized <= extent`. Exclusivity holds because
        // this is the only handle to this buffer, and the registration never
        // reborrows the underlying box.
        unsafe { std::slice::from_raw_parts(self.ptr, self.initialized) }
    }
}

impl DerefMut for RegisteredBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        if self.initialized == 0 {
            return &mut [];
        }
        // SAFETY: as for `deref`, and `&mut self` proves no shared borrow of
        // these bytes is outstanding.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.initialized) }
    }
}

// SAFETY:
//
// **Validity.** `ptr` is the address recorded in the descriptor handed to the
// platform at registration, so it is non-null, suitably aligned, and valid for
// `extent` bytes within one allocation whose length was clamped to `u32::MAX`
// and so cannot exceed `isize::MAX`. The allocation is kept alive by the `Rc` to
// the registration this handle holds, independently of the driver.
//
// **Coherence.** The registration never reborrows the boxed buffers, so the
// bytes are unreachable through its `RefCell`; the cell holds only bookkeeping.
// `buf_ptr` is a plain field read and `buf_len` returns the cached count, so a
// shared borrow always sees a coherent pair, and neither can change through a
// shared reference.
//
// **Stability.** `ptr` and `extent` are fixed for the registration's life, and
// the cached count changes only through `&mut self` — `fill`, `set_buf_init`, or
// `refresh`. The driver raising the registration's count does not alter what an
// in-flight handle reports.
//
// **Exclusivity.** Exactly one handle to a buffer exists at a time, and an
// operation takes it by value, so the application cannot reach the bytes while
// the kernel may be using them.
unsafe impl crate::buf::IoBuf for RegisteredBuf {
    fn buf_ptr(&self) -> *const u8 {
        self.ptr
    }

    fn buf_len(&self) -> usize {
        self.initialized
    }
}

// SAFETY: as for `IoBuf`. `buf_mut_ptr` reports the same address as `buf_ptr`
// and never reallocates, and `buf_capacity` is the registered extent, which is
// always at least the initialized count because both `fill` and `set_buf_init`
// clamp to it.
unsafe impl crate::buf::IoBufMut for RegisteredBuf {
    fn buf_mut_ptr(&mut self) -> *mut u8 {
        self.ptr
    }

    fn buf_capacity(&self) -> usize {
        self.extent
    }

    unsafe fn set_buf_init(&mut self, len: usize) {
        let len = len.min(self.extent);
        self.initialized = len;
        self.registry.set_initialized(self.index, len);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buf::{IoBuf, IoBufMut};

    /// Builds a standalone registration for tests, bypassing the driver.
    ///
    /// Its driver reference is dangling, so only the registry-local half of
    /// checkout — index range and the checked-out flag — is exercised here.
    /// The guards that consult a driver are tested against a real one.
    fn registry(sizes: &[usize]) -> RegisteredBuffers {
        let mut buffers: Vec<Box<dyn std::any::Any>> = Vec::new();
        let mut pointers = Vec::new();
        let mut extents = Vec::new();
        let mut initialized = Vec::new();
        for &size in sizes {
            let mut boxed: Box<Vec<u8>> = Box::new(vec![0_u8; size]);
            pointers.push(boxed.as_mut_ptr());
            extents.push(size);
            initialized.push(0);
            buffers.push(boxed);
        }
        RegisteredBuffers::from_inner(RegistryInner::new(
            buffers,
            pointers,
            extents,
            initialized,
            Vec::new().into_boxed_slice(),
            Weak::new(),
        ))
    }

    /// Claims a buffer without the driver-consulting guards.
    fn claim(buffers: &RegisteredBuffers, index: u32) -> Result<RegisteredBuf> {
        buffers.inner().claim(index)
    }

    #[test]
    fn a_buffer_can_be_checked_out_and_reports_its_extent() {
        let buffers = registry(&[64, 128]);
        assert_eq!(buffers.len(), 2);

        let buf = claim(&buffers, 1).expect("index 1 exists");
        assert_eq!(buf.index(), 1);
        assert_eq!(buf.capacity(), 128);
        assert_eq!(buf.buf_capacity(), 128);
    }

    /// FR-005: two handles to one buffer would let an operation in flight and
    /// the application reach the same bytes at once.
    #[test]
    fn a_buffer_cannot_be_checked_out_twice() {
        let buffers = registry(&[64]);
        let _first = claim(&buffers, 0).expect("index 0 exists");

        match claim(&buffers, 0) {
            Err(Error::BufferCheckedOut { index }) => assert_eq!(index, 0),
            other => panic!("expected BufferCheckedOut, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_index_is_refused() {
        let buffers = registry(&[64]);
        match claim(&buffers, 7) {
            Err(Error::InvalidRegisteredIndex { index }) => assert_eq!(index, 7),
            other => panic!("expected InvalidRegisteredIndex, got {other:?}"),
        }
    }

    /// FR-006: dropping a handle returns its buffer to the collection.
    #[test]
    fn dropping_a_handle_returns_its_buffer() {
        let buffers = registry(&[64]);
        let first = claim(&buffers, 0).expect("index 0 exists");
        assert!(claim(&buffers, 0).is_err());
        drop(first);

        claim(&buffers, 0).expect("the buffer must be available once its handle is dropped");
    }

    /// FR-002 and FR-003: the application can write bytes into a registered
    /// buffer and read them back, which is the whole point of the redesign.
    #[test]
    fn fill_then_read_round_trips_through_the_handle() {
        let buffers = registry(&[64]);
        let mut buf = claim(&buffers, 0).expect("index 0 exists");

        assert!(buf.is_empty(), "a fresh buffer holds nothing");
        assert_eq!(buf.buf_len(), 0);

        buf.fill(b"hello").expect("64 bytes is room enough");
        assert_eq!(&*buf, b"hello");
        assert_eq!(
            buf.buf_len(),
            5,
            "the buffer contract must agree with Deref"
        );
    }

    #[test]
    fn filling_past_the_extent_is_refused() {
        let buffers = registry(&[4]);
        let mut buf = claim(&buffers, 0).expect("index 0 exists");

        match buf.fill(b"too long") {
            Err(Error::BufferTooSmall {
                requested,
                available,
            }) => {
                assert_eq!(requested, 8);
                assert_eq!(available, 4);
            }
            other => panic!("expected BufferTooSmall, got {other:?}"),
        }
        assert!(
            buf.is_empty(),
            "a refused fill must not claim initialization"
        );
    }

    /// FR-008: the count travels with the handle and survives a round trip
    /// through the collection.
    #[test]
    fn the_initialized_count_survives_check_in_and_check_out() {
        let buffers = registry(&[64]);
        let mut buf = claim(&buffers, 0).expect("index 0 exists");
        buf.fill(b"kept").expect("room enough");
        drop(buf);

        let buf = claim(&buffers, 0).expect("index 0 exists");
        assert_eq!(&*buf, b"kept", "the bytes and the count must both survive");
    }

    /// D3: `set_buf_init` assigns rather than raising, matching `Vec<u8>`.
    /// Lowering only narrows what a later write may source, so it is safe.
    #[test]
    fn set_buf_init_assigns_in_both_directions() {
        let buffers = registry(&[64]);
        let mut buf = claim(&buffers, 0).expect("index 0 exists");
        buf.fill(b"0123456789").expect("room enough");
        assert_eq!(buf.buf_len(), 10);

        // SAFETY: the first 4 bytes were initialized by the fill above.
        unsafe { buf.set_buf_init(4) };
        assert_eq!(buf.buf_len(), 4, "a shorter count must be honoured");
        assert_eq!(&*buf, b"0123");

        // SAFETY: the first 8 bytes were initialized by the fill above.
        unsafe { buf.set_buf_init(8) };
        assert_eq!(buf.buf_len(), 8);
    }

    #[test]
    fn set_buf_init_is_clamped_to_the_extent() {
        let buffers = registry(&[8]);
        let mut buf = claim(&buffers, 0).expect("index 0 exists");

        // SAFETY: the clamp is what keeps this within the extent; the bytes
        // below it are zero-initialized by the constructor above.
        unsafe { buf.set_buf_init(999) };
        assert_eq!(buf.buf_len(), 8);
    }

    /// The rule that stops a transfer landing past the mark from vouching for
    /// the uninitialized gap in front of it.
    #[test]
    fn a_transfer_past_the_mark_does_not_extend_it() {
        let buffers = registry(&[64]);
        let inner = buffers.inner();

        inner.raise_initialized(0, 0, 8);
        assert_eq!(inner.initialized(0), Some(8));

        // Starts beyond the mark: bytes 16..24 arrived, but 8..16 did not.
        inner.raise_initialized(0, 16, 8);
        assert_eq!(
            inner.initialized(0),
            Some(8),
            "raising over the gap would vouch for bytes nothing wrote"
        );

        // Starts at the mark: contiguous, so it extends.
        inner.raise_initialized(0, 8, 8);
        assert_eq!(inner.initialized(0), Some(16));
    }

    #[test]
    fn raising_is_clamped_to_the_extent() {
        let buffers = registry(&[8]);
        let inner = buffers.inner();
        inner.raise_initialized(0, 0, 999);
        assert_eq!(inner.initialized(0), Some(8));
    }

    /// FR-009: the bytes outlive the collection reference the application
    /// started from, because every handle holds its own share of the
    /// registration.
    #[test]
    fn a_handle_keeps_its_buffer_alive_on_its_own() {
        let buffers = registry(&[16]);
        let mut buf = claim(&buffers, 0).expect("index 0 exists");
        buf.fill(b"alive").expect("room enough");

        drop(buffers);

        assert_eq!(&*buf, b"alive", "the bytes must survive the collection");
    }
}
