//! Operation descriptors for the raw IoRing layer.
//!
//! Each descriptor is built with a fluent builder and then handed to the
//! matching `IoRing::build_*` method. Builders validate that required fields
//! were supplied and report omissions as [`Error::MissingField`].
//!
//! Only read, write and flush accept submission queue entry flags; the
//! platform's cancellation and registration builders take no such parameter.

use windows::Win32::{
    Foundation::HANDLE,
    Storage::FileSystem::{
        FILE_FLUSH_MODE, FILE_WRITE_FLAGS, FILE_WRITE_FLAGS_NONE, IORING_BUFFER_REF,
        IORING_BUFFER_REF_0, IORING_HANDLE_REF, IORING_HANDLE_REF_0, IORING_REF_RAW,
        IORING_REF_REGISTERED, IORING_REGISTERED_BUFFER, IORING_SQE_FLAGS,
        IOSQE_FLAGS_DRAIN_PRECEDING_OPS, IOSQE_FLAGS_NONE,
    },
};

use crate::error::{Error, Result};

/// Names the file an operation targets, either directly or by registration
/// index.
#[derive(Clone, Copy)]
pub enum HandleRef {
    /// A raw OS handle.
    Raw {
        /// The handle.
        handle: HANDLE,
    },
    /// An index into the ring's registered file handles.
    Registered {
        /// The registration index.
        index: u32,
    },
}

impl HandleRef {
    fn to_platform(self) -> IORING_HANDLE_REF {
        match self {
            HandleRef::Raw { handle } => IORING_HANDLE_REF {
                Kind: IORING_REF_RAW,
                Handle: IORING_HANDLE_REF_0 { Handle: handle },
            },
            HandleRef::Registered { index } => IORING_HANDLE_REF {
                Kind: IORING_REF_REGISTERED,
                Handle: IORING_HANDLE_REF_0 { Index: index },
            },
        }
    }
}

/// Names the memory an operation transfers, either directly or by registration
/// index and offset.
#[derive(Clone, Copy)]
pub enum BufferRef {
    /// A raw address.
    Raw {
        /// The address of the first byte.
        address: *mut std::ffi::c_void,
    },
    /// An offset into one of the ring's registered buffers.
    Registered {
        /// The registration index.
        index: u32,
        /// The offset within that registered buffer.
        offset: u32,
    },
}

impl BufferRef {
    fn to_platform(self) -> IORING_BUFFER_REF {
        match self {
            BufferRef::Raw { address } => IORING_BUFFER_REF {
                Kind: IORING_REF_RAW,
                Buffer: IORING_BUFFER_REF_0 { Address: address },
            },
            BufferRef::Registered { index, offset } => IORING_BUFFER_REF {
                Kind: IORING_REF_REGISTERED,
                Buffer: IORING_BUFFER_REF_0 {
                    IndexAndOffset: IORING_REGISTERED_BUFFER {
                        BufferIndex: index,
                        Offset: offset,
                    },
                },
            },
        }
    }
}

bitflags::bitflags! {
  /// Submission queue entry flags.
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub struct SqeFlags: i32{
    /// No flags.
    const NONE = IOSQE_FLAGS_NONE.0;
    /// Drain all preceding operations before starting this one.
    const DRAIN_PRECEDING_OPS = IOSQE_FLAGS_DRAIN_PRECEDING_OPS.0;
  }
}

impl SqeFlags {
    fn to_platform(self) -> IORING_SQE_FLAGS {
        IORING_SQE_FLAGS(self.bits())
    }
}

/// A read operation.
pub struct ReadOp {
    pub(crate) handle_ref: IORING_HANDLE_REF,
    pub(crate) data_ref: IORING_BUFFER_REF,
    pub(crate) num_of_bytes_to_read: u32,
    pub(crate) offset: u64,
    pub(crate) userdata: usize,
    pub(crate) sqe_flags: IORING_SQE_FLAGS,
}

impl ReadOp {
    /// Returns a builder for a read operation.
    pub fn builder() -> ReadOpBuilder {
        ReadOpBuilder::new()
    }
}

/// Builder for [`ReadOp`].
#[derive(Default)]
pub struct ReadOpBuilder {
    handle_ref: Option<HandleRef>,
    data_ref: Option<BufferRef>,
    num_of_bytes_to_read: Option<u32>,
    offset: Option<u64>,
    user_data: Option<usize>,
    sqe_flags: Option<SqeFlags>,
}

impl ReadOpBuilder {
    /// Creates an empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Targets a raw OS handle.
    pub fn with_raw_handle(mut self, handle: HANDLE) -> Self {
        self.handle_ref = Some(HandleRef::Raw { handle });
        self
    }

    /// Targets a registered file handle by index.
    pub fn with_registered_handle_index(mut self, index: u32) -> Self {
        self.handle_ref = Some(HandleRef::Registered { index });
        self
    }

    /// Reads into a raw address.
    pub fn with_raw_data_address(mut self, address: *mut std::ffi::c_void) -> Self {
        self.data_ref = Some(BufferRef::Raw { address });
        self
    }

    /// Reads into a registered buffer at the given index and offset.
    pub fn with_registered_data_index_and_offset(mut self, index: u32, offset: u32) -> Self {
        self.data_ref = Some(BufferRef::Registered { index, offset });
        self
    }

    /// Sets the number of bytes to read.
    pub fn with_num_of_bytes_to_read(mut self, num_of_bytes_to_read: u32) -> Self {
        self.num_of_bytes_to_read = Some(num_of_bytes_to_read);
        self
    }

    /// Sets the file offset to read from.
    pub fn with_offset(mut self, offset: u64) -> Self {
        self.offset = Some(offset);
        self
    }

    /// Sets the user data reported with this operation's completion.
    pub fn with_user_data(mut self, user_data: usize) -> Self {
        self.user_data = Some(user_data);
        self
    }

    /// Sets the submission queue entry flags.
    pub fn with_sqe_flags(mut self, sqe_flags: SqeFlags) -> Self {
        self.sqe_flags = Some(sqe_flags);
        self
    }

    /// Builds the operation.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MissingField`] if the handle or data reference was not
    /// supplied.
    pub fn build(self) -> Result<ReadOp> {
        let handle_ref = self
            .handle_ref
            .ok_or(Error::MissingField { field: "handle" })?;
        let data_ref = self.data_ref.ok_or(Error::MissingField { field: "data" })?;

        Ok(ReadOp {
            handle_ref: handle_ref.to_platform(),
            data_ref: data_ref.to_platform(),
            num_of_bytes_to_read: self.num_of_bytes_to_read.unwrap_or(0),
            offset: self.offset.unwrap_or(0),
            userdata: self.user_data.unwrap_or(0),
            sqe_flags: self.sqe_flags.unwrap_or(SqeFlags::NONE).to_platform(),
        })
    }
}

/// A write operation.
pub struct WriteOp {
    pub(crate) handle_ref: IORING_HANDLE_REF,
    pub(crate) data_ref: IORING_BUFFER_REF,
    pub(crate) num_of_bytes_to_write: u32,
    pub(crate) offset: u64,
    pub(crate) write_flags: FILE_WRITE_FLAGS,
    pub(crate) userdata: usize,
    pub(crate) sqe_flags: IORING_SQE_FLAGS,
}

impl WriteOp {
    /// Returns a builder for a write operation.
    pub fn builder() -> WriteOpBuilder {
        WriteOpBuilder::new()
    }
}

/// Builder for [`WriteOp`].
#[derive(Default)]
pub struct WriteOpBuilder {
    handle_ref: Option<HandleRef>,
    data_ref: Option<BufferRef>,
    num_of_bytes_to_write: Option<u32>,
    offset: Option<u64>,
    write_flags: Option<FILE_WRITE_FLAGS>,
    user_data: Option<usize>,
    sqe_flags: Option<SqeFlags>,
}

impl WriteOpBuilder {
    /// Creates an empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Targets a raw OS handle.
    pub fn with_raw_handle(mut self, handle: HANDLE) -> Self {
        self.handle_ref = Some(HandleRef::Raw { handle });
        self
    }

    /// Targets a registered file handle by index.
    pub fn with_registered_handle_index(mut self, index: u32) -> Self {
        self.handle_ref = Some(HandleRef::Registered { index });
        self
    }

    /// Writes from a raw address.
    pub fn with_raw_data_address(mut self, address: *mut std::ffi::c_void) -> Self {
        self.data_ref = Some(BufferRef::Raw { address });
        self
    }

    /// Writes from a registered buffer at the given index and offset.
    pub fn with_registered_data_index_and_offset(mut self, index: u32, offset: u32) -> Self {
        self.data_ref = Some(BufferRef::Registered { index, offset });
        self
    }

    /// Sets the number of bytes to write.
    pub fn with_num_of_bytes_to_write(mut self, num_of_bytes_to_write: u32) -> Self {
        self.num_of_bytes_to_write = Some(num_of_bytes_to_write);
        self
    }

    /// Sets the file offset to write at.
    pub fn with_offset(mut self, offset: u64) -> Self {
        self.offset = Some(offset);
        self
    }

    /// Sets the platform write flags, such as write-through.
    pub fn with_write_flags(mut self, write_flags: FILE_WRITE_FLAGS) -> Self {
        self.write_flags = Some(write_flags);
        self
    }

    /// Sets the user data reported with this operation's completion.
    pub fn with_user_data(mut self, user_data: usize) -> Self {
        self.user_data = Some(user_data);
        self
    }

    /// Sets the submission queue entry flags.
    pub fn with_sqe_flags(mut self, sqe_flags: SqeFlags) -> Self {
        self.sqe_flags = Some(sqe_flags);
        self
    }

    /// Builds the operation.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MissingField`] if the handle or data reference was not
    /// supplied.
    pub fn build(self) -> Result<WriteOp> {
        let handle_ref = self
            .handle_ref
            .ok_or(Error::MissingField { field: "handle" })?;
        let data_ref = self.data_ref.ok_or(Error::MissingField { field: "data" })?;

        Ok(WriteOp {
            handle_ref: handle_ref.to_platform(),
            data_ref: data_ref.to_platform(),
            num_of_bytes_to_write: self.num_of_bytes_to_write.unwrap_or(0),
            offset: self.offset.unwrap_or(0),
            write_flags: self.write_flags.unwrap_or(FILE_WRITE_FLAGS_NONE),
            userdata: self.user_data.unwrap_or(0),
            sqe_flags: self.sqe_flags.unwrap_or(SqeFlags::NONE).to_platform(),
        })
    }
}

/// A flush operation.
pub struct FlushOp {
    pub(crate) handle_ref: IORING_HANDLE_REF,
    pub(crate) flush_mode: FILE_FLUSH_MODE,
    pub(crate) userdata: usize,
    pub(crate) sqe_flags: IORING_SQE_FLAGS,
}

impl FlushOp {
    /// Returns a builder for a flush operation.
    pub fn builder() -> FlushOpBuilder {
        FlushOpBuilder::new()
    }
}

/// Builder for [`FlushOp`].
#[derive(Default)]
pub struct FlushOpBuilder {
    handle_ref: Option<HandleRef>,
    flush_mode: Option<FILE_FLUSH_MODE>,
    user_data: Option<usize>,
    sqe_flags: Option<SqeFlags>,
}

impl FlushOpBuilder {
    /// Creates an empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Targets a raw OS handle.
    pub fn with_raw_handle(mut self, handle: HANDLE) -> Self {
        self.handle_ref = Some(HandleRef::Raw { handle });
        self
    }

    /// Targets a registered file handle by index.
    pub fn with_registered_handle_index(mut self, index: u32) -> Self {
        self.handle_ref = Some(HandleRef::Registered { index });
        self
    }

    /// Sets the flush mode.
    pub fn with_flush_mode(mut self, flush_mode: FILE_FLUSH_MODE) -> Self {
        self.flush_mode = Some(flush_mode);
        self
    }

    /// Sets the user data reported with this operation's completion.
    pub fn with_user_data(mut self, user_data: usize) -> Self {
        self.user_data = Some(user_data);
        self
    }

    /// Sets the submission queue entry flags.
    pub fn with_sqe_flags(mut self, sqe_flags: SqeFlags) -> Self {
        self.sqe_flags = Some(sqe_flags);
        self
    }

    /// Builds the operation.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MissingField`] if the handle reference was not
    /// supplied.
    pub fn build(self) -> Result<FlushOp> {
        let handle_ref = self
            .handle_ref
            .ok_or(Error::MissingField { field: "handle" })?;

        Ok(FlushOp {
            handle_ref: handle_ref.to_platform(),
            flush_mode: self.flush_mode.unwrap_or_default(),
            userdata: self.user_data.unwrap_or(0),
            sqe_flags: self.sqe_flags.unwrap_or(SqeFlags::NONE).to_platform(),
        })
    }
}

/// A cancellation request.
///
/// Cancellation names its target by that operation's user data. The request
/// carries its own user data and produces its own completion, so a driver must
/// be able to tell the two apart. The platform's cancellation builder accepts
/// no submission queue entry flags.
pub struct CancelOp {
    pub(crate) handle_ref: IORING_HANDLE_REF,
    pub(crate) op_to_cancel: usize,
    pub(crate) userdata: usize,
}

impl CancelOp {
    /// Returns a builder for a cancellation request.
    pub fn builder() -> CancelOpBuilder {
        CancelOpBuilder::new()
    }
}

/// Builder for [`CancelOp`].
#[derive(Default)]
pub struct CancelOpBuilder {
    handle_ref: Option<HandleRef>,
    op_to_cancel: Option<usize>,
    user_data: Option<usize>,
}

impl CancelOpBuilder {
    /// Creates an empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Targets a raw OS handle. This must be the handle the cancelled
    /// operation named.
    pub fn with_raw_handle(mut self, handle: HANDLE) -> Self {
        self.handle_ref = Some(HandleRef::Raw { handle });
        self
    }

    /// Targets a registered file handle by index. This must be the handle the
    /// cancelled operation named.
    pub fn with_registered_handle_index(mut self, index: u32) -> Self {
        self.handle_ref = Some(HandleRef::Registered { index });
        self
    }

    /// Sets the user data of the operation to cancel.
    pub fn with_op_to_cancel(mut self, op_to_cancel: usize) -> Self {
        self.op_to_cancel = Some(op_to_cancel);
        self
    }

    /// Sets the user data reported with the cancellation's own completion.
    pub fn with_user_data(mut self, user_data: usize) -> Self {
        self.user_data = Some(user_data);
        self
    }

    /// Builds the request.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MissingField`] if the handle reference or the target
    /// operation was not supplied.
    pub fn build(self) -> Result<CancelOp> {
        let handle_ref = self
            .handle_ref
            .ok_or(Error::MissingField { field: "handle" })?;
        let op_to_cancel = self.op_to_cancel.ok_or(Error::MissingField {
            field: "op_to_cancel",
        })?;

        Ok(CancelOp {
            handle_ref: handle_ref.to_platform(),
            op_to_cancel,
            userdata: self.user_data.unwrap_or(0),
        })
    }
}

/// A buffer registration.
///
/// Registration replaces any previous buffer registration on the ring. The
/// platform provides no unregister entry point, and a zero-length registration
/// fails at completion time, so a registration is released only by replacing it
/// or by closing the ring.
pub struct RegisterBuffersOp<'a> {
    pub(crate) buffers: &'a [crate::io_ring::BufferInfo],
    pub(crate) userdata: usize,
}

impl<'a> RegisterBuffersOp<'a> {
    /// Describes a registration of the given buffers.
    pub fn new(buffers: &'a [crate::io_ring::BufferInfo], userdata: usize) -> Self {
        Self { buffers, userdata }
    }
}

/// A file handle registration.
///
/// Registration replaces any previous handle registration on the ring. See
/// [`RegisterBuffersOp`] for why registrations cannot be released directly.
pub struct RegisterFilesOp<'a> {
    pub(crate) handles: &'a [HANDLE],
    pub(crate) userdata: usize,
}

impl<'a> RegisterFilesOp<'a> {
    /// Describes a registration of the given handles.
    pub fn new(handles: &'a [HANDLE], userdata: usize) -> Self {
        Self { handles, userdata }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_builder_requires_handle_and_data() {
        let err = ReadOp::builder()
            .with_raw_data_address(std::ptr::null_mut())
            .build()
            .err()
            .unwrap();
        assert!(matches!(err, Error::MissingField { field: "handle" }));

        let err = ReadOp::builder()
            .with_registered_handle_index(0)
            .build()
            .err()
            .unwrap();
        assert!(matches!(err, Error::MissingField { field: "data" }));
    }

    #[test]
    fn write_builder_requires_handle_and_data() {
        let err = WriteOp::builder().build().err().unwrap();
        assert!(matches!(err, Error::MissingField { field: "handle" }));

        let err = WriteOp::builder()
            .with_registered_handle_index(0)
            .build()
            .err()
            .unwrap();
        assert!(matches!(err, Error::MissingField { field: "data" }));
    }

    #[test]
    fn flush_builder_requires_handle() {
        let err = FlushOp::builder().build().err().unwrap();
        assert!(matches!(err, Error::MissingField { field: "handle" }));
    }

    #[test]
    fn cancel_builder_requires_handle_and_target() {
        let err = CancelOp::builder().build().err().unwrap();
        assert!(matches!(err, Error::MissingField { field: "handle" }));

        let err = CancelOp::builder()
            .with_registered_handle_index(0)
            .build()
            .err()
            .unwrap();
        assert!(matches!(
            err,
            Error::MissingField {
                field: "op_to_cancel"
            }
        ));
    }

    #[test]
    fn sqe_flags_round_trip() {
        assert_eq!(SqeFlags::NONE.to_platform().0, IOSQE_FLAGS_NONE.0);
        assert_eq!(
            SqeFlags::DRAIN_PRECEDING_OPS.to_platform().0,
            IOSQE_FLAGS_DRAIN_PRECEDING_OPS.0
        );
    }
}
