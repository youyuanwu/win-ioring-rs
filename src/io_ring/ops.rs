use windows::Win32::{
    Foundation::HANDLE,
    Storage::FileSystem::{
        IORING_BUFFER_REF, IORING_BUFFER_REF_0, IORING_HANDLE_REF, IORING_HANDLE_REF_0,
        IORING_REF_RAW, IORING_REF_REGISTERED, IORING_REGISTERED_BUFFER, IORING_SQE_FLAGS,
        IOSQE_FLAGS_DRAIN_PRECEDING_OPS, IOSQE_FLAGS_NONE,
    },
};

pub struct ReadOp {
    pub handle_ref: IORING_HANDLE_REF,
    pub data_ref: IORING_BUFFER_REF,
    pub num_of_bytes_to_read: u32,
    pub offset: u64,
    pub userdata: usize,
    pub sqe_flags: IORING_SQE_FLAGS,
}

impl ReadOp {
    pub fn builder() -> ReadOpBuilder {
        ReadOpBuilder::new()
    }
}

enum HandleRef {
    Raw { handle: HANDLE },
    Registered { index: u32 },
}

enum BufferRef {
    Raw { address: *mut std::ffi::c_void },
    Registered { index: u32, offset: u32 },
}

bitflags::bitflags! {
  pub struct SqeFlags: i32{
    const NONE = IOSQE_FLAGS_NONE.0;
    const DRAIN_PRECEDING_OPS = IOSQE_FLAGS_DRAIN_PRECEDING_OPS.0;
  }
}

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
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_raw_handle(mut self, handle: HANDLE) -> Self {
        self.handle_ref = Some(HandleRef::Raw { handle });
        self
    }

    pub fn with_registered_handle_index(mut self, index: u32) -> Self {
        self.handle_ref = Some(HandleRef::Registered { index });
        self
    }

    pub fn with_raw_data_address(mut self, address: *mut std::ffi::c_void) -> Self {
        self.data_ref = Some(BufferRef::Raw { address });
        self
    }

    pub fn with_registered_data_index_and_offset(mut self, index: u32, offset: u32) -> Self {
        self.data_ref = Some(BufferRef::Registered { index, offset });
        self
    }

    pub fn with_num_of_bytes_to_read(mut self, num_of_bytes_to_read: u32) -> Self {
        self.num_of_bytes_to_read = Some(num_of_bytes_to_read);
        self
    }

    pub fn with_offset(mut self, offset: u64) -> Self {
        self.offset = Some(offset);
        self
    }

    pub fn with_user_data(mut self, user_data: usize) -> Self {
        self.user_data = Some(user_data);
        self
    }

    pub fn with_sqe_flags(mut self, sqe_flags: SqeFlags) -> Self {
        self.sqe_flags = Some(sqe_flags);
        self
    }

    pub fn build(self) -> ReadOp {
        let handle_ref = match self.handle_ref.unwrap() {
            HandleRef::Raw { handle } => IORING_HANDLE_REF {
                Kind: IORING_REF_RAW,
                Handle: IORING_HANDLE_REF_0 { Handle: handle },
            },
            HandleRef::Registered { index } => IORING_HANDLE_REF {
                Kind: IORING_REF_REGISTERED,
                Handle: IORING_HANDLE_REF_0 { Index: index },
            },
        };
        let data_ref = match self.data_ref.unwrap() {
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
        };

        let sqe_flags = self.sqe_flags.unwrap_or(SqeFlags::NONE);

        ReadOp {
            handle_ref,
            data_ref,
            num_of_bytes_to_read: self.num_of_bytes_to_read.unwrap_or(0),
            offset: self.offset.unwrap_or(0),
            userdata: self.user_data.unwrap_or(0),
            sqe_flags: IORING_SQE_FLAGS(sqe_flags.bits()),
        }
    }
}
