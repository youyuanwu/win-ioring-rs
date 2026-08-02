use windows::Win32::Storage::FileSystem::*;

use windows::Win32::Foundation::*;

use crate::error::{Error, Result};

/// Capabilities reported by the host's IoRing implementation.
///
/// Obtained from [`IoRing::query_io_ring_capabilities`]. The version ceiling is
/// deliberately exposed as the platform's own newtype rather than matched
/// against named constants: hosts report values that have no corresponding
/// constant in the bindings this crate uses, so version handling must treat it
/// as an open range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// The highest ring version this host supports.
    pub max_version: IORING_VERSION,
    /// The largest submission queue size this host will accept.
    pub max_submission_queue_size: u32,
    /// The largest completion queue size this host will accept.
    pub max_completion_queue_size: u32,
    /// The feature flags this host reports.
    pub feature_flags: IORING_FEATURE_FLAGS,
}

impl Capabilities {
    /// Returns `true` if every bit in `required` is present in the reported
    /// feature flags.
    pub fn supports_features(&self, required: IORING_FEATURE_FLAGS) -> bool {
        self.feature_flags.0 & required.0 == required.0
    }
}

/// Information about a live ring, as reported by `GetIoRingInfo`.
///
/// The queue sizes reported here are the sizes the platform actually allocated,
/// rounded up to a power of two, and so are generally larger than the sizes
/// that were requested. Code that needs the real capacity — for instance to
/// detect a full submission queue — must use these values, not the requested
/// ones.
#[derive(Debug, Clone, Copy)]
pub struct RingInfo {
    /// The version this ring was created with.
    pub version: IORING_VERSION,
    /// The flags this ring was created with.
    pub flags: IORING_CREATE_FLAGS,
    /// The submission queue size the platform allocated.
    pub submission_queue_size: u32,
    /// The completion queue size the platform allocated.
    pub completion_queue_size: u32,
}

/// A raw, unsafe handle to a Windows IoRing.
///
/// This type is a thin wrapper over the platform API. It performs no lifetime
/// tracking of buffers or handles; every method that submits work to the kernel
/// is `unsafe` and documents what the caller must guarantee. The safe,
/// lifetime-tracking API lives in [`crate::runtime`].
///
/// # Closing
///
/// There is deliberately no `Drop` implementation. Closing a ring while the
/// kernel may still be working is exactly the decision this type refuses to
/// make on the caller's behalf, so [`IoRing::close`] must be called explicitly;
/// otherwise the ring handle is leaked. `close` is idempotent.
///
/// Every method checks first, so calling one on a closed ring returns
/// [`Error::RingClosed`] rather than reaching the platform. That check is not
/// belt-and-braces: passing a closed ring handle to `PopIoRingCompletion`
/// faults rather than returning an error, so without it the safe methods on
/// this type would not be safe.
pub struct IoRing {
    /// The underlying platform ring handle.
    pub ring: HIORING,
    closed: bool,
}

/// Builder for [`IoRing`].
///
/// By default the builder requires [`IORING_FEATURE_SET_COMPLETION_EVENT`],
/// because this crate's driver depends on completion-event signalling. Call
/// [`IoRingBuilder::with_required_features`] to override that.
pub struct IoRingBuilder {
    submission_queue_size: Option<u32>,
    completion_queue_size: Option<u32>,
    version: Option<IORING_VERSION>,
    required_features: IORING_FEATURE_FLAGS,
}

impl Default for IoRingBuilder {
    fn default() -> Self {
        Self {
            submission_queue_size: None,
            completion_queue_size: None,
            version: None,
            required_features: IORING_FEATURE_SET_COMPLETION_EVENT,
        }
    }
}

impl IoRingBuilder {
    /// Creates a builder with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the requested submission queue size.
    ///
    /// The platform rounds this up to a power of two; use [`IoRing::info`] to
    /// discover the size actually allocated.
    pub fn with_submission_queue_size(mut self, size: u32) -> Self {
        self.submission_queue_size = Some(size);
        self
    }

    /// Sets the requested completion queue size.
    ///
    /// The platform rounds this up to a power of two; use [`IoRing::info`] to
    /// discover the size actually allocated.
    pub fn with_completion_queue_size(mut self, size: u32) -> Self {
        self.completion_queue_size = Some(size);
        self
    }

    /// Sets the ring version to request.
    ///
    /// Defaults to the host's reported maximum.
    pub fn with_version(mut self, version: IORING_VERSION) -> Self {
        self.version = Some(version);
        self
    }

    /// Sets the feature flags the host must report.
    ///
    /// This replaces the default requirement of
    /// [`IORING_FEATURE_SET_COMPLETION_EVENT`]. Pass
    /// [`IORING_FEATURE_FLAGS_NONE`] to require nothing. Building fails with
    /// [`Error::UnsupportedFeature`] if any required bit is missing.
    pub fn with_required_features(mut self, features: IORING_FEATURE_FLAGS) -> Self {
        self.required_features = features;
        self
    }

    /// Creates the ring.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnsupportedVersion`] if the requested version exceeds
    /// what the host reports, [`Error::UnsupportedFeature`] if a required
    /// feature is missing, and [`Error::Unsupported`] if the host cannot
    /// provide a usable ring at all.
    pub fn build(self) -> Result<IoRing> {
        let submission_queue_size = self.submission_queue_size.unwrap_or(20);
        let completion_queue_size = self.completion_queue_size.unwrap_or(20);

        let caps = IoRing::query_io_ring_capabilities()?;

        if !caps.supports_features(self.required_features) {
            return Err(Error::UnsupportedFeature {
                required: self.required_features.0,
                available: caps.feature_flags.0,
            });
        }

        let version = match self.version {
            Some(v) => {
                if v.0 > caps.max_version.0 {
                    return Err(Error::unsupported_version(v, caps.max_version));
                }
                v
            }
            None => caps.max_version,
        };
        IoRing::create(version, submission_queue_size, completion_queue_size)
    }
}

/// Describes a registered buffer's address and length.
///
/// The representation is transparent because slices of this type are handed to
/// the platform as slices of the underlying structure.
#[repr(transparent)]
pub struct BufferInfo(IORING_BUFFER_INFO);

impl BufferInfo {
    /// Consumes this wrapper, yielding the raw platform structure.
    pub fn into_inner(self) -> IORING_BUFFER_INFO {
        self.0
    }

    /// Describes a buffer by raw address and length.
    ///
    /// # Safety
    ///
    /// `address` must point to at least `length` bytes that remain valid and at
    /// a stable address until the registration referencing them is released.
    pub unsafe fn from_raw_parts(address: *mut std::ffi::c_void, length: u32) -> Self {
        Self(IORING_BUFFER_INFO {
            Address: address,
            Length: length,
        })
    }

    /// Describes a slice's bytes.
    ///
    /// # Safety
    /// The buffer must be valid until the registration referencing it is released.
    pub unsafe fn raw_from_vec(buffer: &mut [u8]) -> Self {
        let len = buffer.len() as u32;
        // SAFETY: the caller guarantees the buffer outlives the operation, and
        // the pointer and length come from a live slice.
        unsafe { Self::from_raw_parts(buffer.as_mut_ptr() as *mut _, len) }
    }
}

impl IoRing {
    /// Returns a builder for creating a ring.
    pub fn builder() -> IoRingBuilder {
        IoRingBuilder::new()
    }

    /// Queries the host's IoRing capabilities.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] if the host cannot report capabilities.
    pub fn query_io_ring_capabilities() -> Result<Capabilities> {
        // SAFETY: the call takes no pointers and no ring; it reports the host's
        // capabilities into a struct it returns by value.
        let raw = unsafe { QueryIoRingCapabilities() }.map_err(Error::from_create_failure)?;
        Ok(Capabilities {
            max_version: raw.MaxVersion,
            max_submission_queue_size: raw.MaxSubmissionQueueSize,
            max_completion_queue_size: raw.MaxCompletionQueueSize,
            feature_flags: raw.FeatureFlags,
        })
    }

    /// Creates a ring with an explicit version and queue sizes.
    ///
    /// Prefer [`IoRing::builder`], which validates the version against the
    /// host's reported ceiling first.
    pub fn create(
        version: IORING_VERSION,
        submission_queue_size: u32,
        completion_queue_size: u32,
    ) -> Result<IoRing> {
        // currently win32 only has none flags
        let flags = IORING_CREATE_FLAGS {
            Required: IORING_CREATE_REQUIRED_FLAGS_NONE,
            Advisory: IORING_CREATE_ADVISORY_FLAGS_NONE,
        };

        let inner_ring =
            // SAFETY: every argument is a plain value. The handle the call produces is
            // owned by the `IoRing` built from it, whose `close` is idempotent, so it
            // cannot be closed twice.
            unsafe { CreateIoRing(version, flags, submission_queue_size, completion_queue_size) }
                .map_err(Error::from_create_failure)?;
        Ok(IoRing {
            ring: inner_ring,
            closed: false,
        })
    }

    /// Returns an error if this ring has been closed.
    ///
    /// The platform does not reliably reject a closed ring handle — passing one
    /// to `PopIoRingCompletion` faults rather than returning an error — so every
    /// method that touches the handle checks here first. That is what lets the
    /// safe methods on this type stay safe after [`IoRing::close`].
    fn ensure_open(&self) -> Result<()> {
        if self.closed {
            Err(Error::RingClosed)
        } else {
            Ok(())
        }
    }

    /// Returns information about this ring, including the queue sizes the
    /// platform actually allocated.
    pub fn info(&self) -> Result<RingInfo> {
        self.ensure_open()?;
        let mut info = IORING_INFO::default();
        // SAFETY: `self.ring` came from `CreateIoRing` and is never replaced. If
        // it has already been closed the platform rejects the stale handle with
        // an error rather than misbehaving. `info` is a local the call fills in.
        unsafe { GetIoRingInfo(self.ring, &mut info) }?;
        Ok(RingInfo {
            version: info.IoRingVersion,
            flags: info.Flags,
            submission_queue_size: info.SubmissionQueueSize,
            completion_queue_size: info.CompletionQueueSize,
        })
    }

    /// Reports whether this ring supports the given operation.
    ///
    /// The platform returns a plain boolean with no error channel, so a `false`
    /// result means "not supported" and cannot be distinguished from a failed
    /// query. A closed ring reports `false` for everything.
    pub fn is_op_supported(&self, op: IORING_OP_CODE) -> bool {
        if self.closed {
            return false;
        }
        // SAFETY: `self.ring` has not been closed, per the check above, and the
        // call only reads whether an op code is supported.
        unsafe { IsIoRingOpSupported(self.ring, op) }.as_bool()
    }

    /// Returns an error unless this ring supports the given operation.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnsupportedOp`] if the host does not support `op`.
    pub fn ensure_op_supported(&self, op: IORING_OP_CODE) -> Result<()> {
        if self.is_op_supported(op) {
            Ok(())
        } else {
            Err(Error::UnsupportedOp { op: op.0 })
        }
    }

    /// Sets the event the kernel signals when completions become available.
    ///
    /// # Safety
    ///
    /// `handle` must remain a valid, open event handle for as long as this ring
    /// might signal it, which is until the ring is closed or the completion
    /// event is replaced. Closing it earlier leaves the kernel signalling a
    /// handle that may since have been reused for something else.
    pub unsafe fn set_io_ring_completion_event(&mut self, handle: HANDLE) -> Result<()> {
        self.ensure_open()?;
        // SAFETY: `self.ring` has not been closed, per the guard above, and the caller guarantees `handle`
        // outlives the ring's use of it.
        unsafe { SetIoRingCompletionEvent(self.ring, handle) }.map_err(Error::from)
    }

    /// Builds a read operation into the submission queue.
    ///
    /// # Safety
    /// File ref and data ref must be valid until the operation is popped from the completion queue.
    pub unsafe fn build_read_file(&mut self, op: super::ops::ReadOp) -> Result<()> {
        self.ensure_open()?;
        // SAFETY: `self.ring` has not been closed, per the guard above, and the caller guarantees the file and data
        // references stay valid until this operation's completion is dequeued.
        unsafe {
            BuildIoRingReadFile(
                self.ring,
                op.handle_ref,
                op.data_ref,
                op.num_of_bytes_to_read,
                op.offset,
                op.userdata,
                op.sqe_flags,
            )
        }
        .map_err(Error::from)
    }

    /// Builds a write operation into the submission queue.
    ///
    /// # Safety
    /// File ref and data ref must be valid until the operation is popped from the completion queue.
    pub unsafe fn build_write_file(&mut self, op: super::ops::WriteOp) -> Result<()> {
        self.ensure_open()?;
        // SAFETY: `self.ring` has not been closed, per the guard above, and the caller guarantees the file and data
        // references stay valid until this operation's completion is dequeued.
        unsafe {
            BuildIoRingWriteFile(
                self.ring,
                op.handle_ref,
                op.data_ref,
                op.num_of_bytes_to_write,
                op.offset,
                op.write_flags,
                op.userdata,
                op.sqe_flags,
            )
        }
        .map_err(Error::from)
    }

    /// Builds a flush operation into the submission queue.
    ///
    /// # Safety
    /// File ref must be valid until the operation is popped from the completion queue.
    pub unsafe fn build_flush_file(&mut self, op: super::ops::FlushOp) -> Result<()> {
        self.ensure_open()?;
        // SAFETY: `self.ring` has not been closed, per the guard above, and the caller guarantees the file reference
        // stays valid until this operation's completion is dequeued.
        unsafe {
            BuildIoRingFlushFile(
                self.ring,
                op.handle_ref,
                op.flush_mode,
                op.userdata,
                op.sqe_flags,
            )
        }
        .map_err(Error::from)
    }

    /// Builds a cancellation request into the submission queue.
    ///
    /// The request targets the operation whose user data equals
    /// [`CancelOp::op_to_cancel`](super::ops::CancelOp). The cancellation
    /// carries its own user data and produces its own completion, distinct from
    /// the completion of the operation it targets.
    ///
    /// # Safety
    /// File ref must be valid until the cancellation is popped from the completion queue.
    pub unsafe fn build_cancel_request(&mut self, op: super::ops::CancelOp) -> Result<()> {
        self.ensure_open()?;
        // SAFETY: `self.ring` has not been closed, per the guard above, and the caller guarantees the file reference
        // stays valid until this cancellation's completion is dequeued.
        unsafe { BuildIoRingCancelRequest(self.ring, op.handle_ref, op.op_to_cancel, op.userdata) }
            .map_err(Error::from)
    }

    /// Builds a file handle registration into the submission queue.
    ///
    /// The platform exposes no unregister entry point, and an empty slice is
    /// not a substitute: it is accepted here but fails at completion time with
    /// E_INVALIDARG. A registration is released by registering a different
    /// non-empty set, or by closing the ring.
    ///
    /// # Safety
    /// The handles must remain valid until the registration is released.
    pub unsafe fn build_register_files(
        &mut self,
        op: super::ops::RegisterFilesOp<'_>,
    ) -> Result<()> {
        // SAFETY: forwards the caller's own obligation unchanged.
        unsafe { self.build_register_file_handles(op.handles, op.userdata) }
    }

    /// Builds a buffer registration into the submission queue.
    ///
    /// See [`IoRing::build_register_files`] for why registrations cannot be
    /// released directly.
    ///
    /// # Safety
    /// buffers must be valid until the registration referencing them is released.
    pub unsafe fn build_register_buffers_op(
        &mut self,
        op: super::ops::RegisterBuffersOp<'_>,
    ) -> Result<()> {
        // SAFETY: forwards the caller's own obligation unchanged.
        unsafe { self.build_register_buffers(op.buffers, op.userdata) }
    }

    /// Builds a file handle registration into the submission queue.
    ///
    /// The platform exposes no unregister entry point, and an empty slice is
    /// not a substitute: it is accepted here but fails at completion time with
    /// E_INVALIDARG. A registration is released by registering a different
    /// non-empty set, or by closing the ring.
    ///
    /// # Safety
    /// The handles must remain valid until the registration is released.
    pub unsafe fn build_register_file_handles(
        &mut self,
        handles: &[HANDLE],
        userdata: usize,
    ) -> Result<()> {
        self.ensure_open()?;
        // SAFETY: `self.ring` has not been closed, per the guard above, and the caller guarantees the handles stay
        // open for as long as the registration lives.
        unsafe { BuildIoRingRegisterFileHandles(self.ring, handles, userdata) }.map_err(Error::from)
    }

    /// Builds a buffer registration into the submission queue.
    ///
    /// The platform exposes no unregister entry point, and an empty slice is
    /// not a substitute: it is accepted here but fails at completion time with
    /// E_INVALIDARG. A registration is released by registering a different
    /// non-empty set, or by closing the ring.
    ///
    /// # Safety
    /// buffers must be valid until the registration referencing them is released.
    pub unsafe fn build_register_buffers(
        &mut self,
        buffers: &[BufferInfo],
        userdata: usize,
    ) -> Result<()> {
        self.ensure_open()?;
        // Convert the types.
        // SAFETY: `BufferInfo` is a transparent wrapper over `IORING_BUFFER_INFO`,
        // so the two have the same layout, and the slice is borrowed for the
        // duration of this call.
        let buffers = unsafe {
            std::slice::from_raw_parts(buffers.as_ptr() as *const IORING_BUFFER_INFO, buffers.len())
        };
        // SAFETY: `self.ring` has not been closed, per the guard above, and the caller guarantees the registered
        // buffers stay alive and unmoved for as long as the registration lives.
        unsafe { BuildIoRingRegisterBuffers(self.ring, buffers, userdata) }.map_err(Error::from)
    }

    /// Submits queued entries to the kernel.
    ///
    /// Returns the number of entries submitted. If this call fails for any
    /// reason other than a wait timeout, the platform leaves every entry in the
    /// submission queue, so the caller must assume those entries — and any
    /// memory they reference — are still live.
    pub fn submit(&mut self, wait_operations: usize, milliseconds: usize) -> Result<u32> {
        self.ensure_open()?;
        let mut submitted_entries = 0_u32;
        // SAFETY: `self.ring` has not been closed, per the guard above, and `submitted_entries` is a local the call
        // fills in.
        unsafe {
            SubmitIoRing(
                self.ring,
                wait_operations as u32,
                milliseconds as u32,
                Some(&mut submitted_entries),
            )
        }?;
        Ok(submitted_entries)
    }

    /// Pops one completion from the completion queue.
    ///
    /// Returns `Ok(None)` when the queue is empty.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Os`] if the platform reports anything other than a
    /// completion or an empty queue.
    pub fn pop_completion(&mut self) -> Result<Option<IORING_CQE>> {
        self.ensure_open()?;
        let mut out = IORING_CQE::default();
        // SAFETY: `self.ring` has not been closed, per the guard above, and `out` is a local the call fills in.
        let hr = unsafe { PopIoRingCompletion(self.ring, &mut out) };
        if hr == S_OK {
            Ok(Some(out))
        } else if hr == S_FALSE {
            Ok(None)
        } else {
            Err(Error::Os(windows::core::Error::from(hr)))
        }
    }

    /// Closes the ring.
    ///
    /// Closing an already-closed ring is a no-op, so this may be called
    /// defensively without risking a double close. If the platform reports a
    /// failure the ring is left open, so the call may be retried.
    pub fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        // SAFETY: `self.ring` has not been closed — the early return above covers
        // that case — and it is marked closed immediately afterwards, so it cannot
        // be closed twice.
        unsafe { CloseIoRing(self.ring) }.map_err(Error::from)?;
        self.closed = true;
        Ok(())
    }

    /// Returns `true` if [`IoRing::close`] has already been called.
    pub fn is_closed(&self) -> bool {
        self.closed
    }
}
