use windows::{core::*, Win32::Storage::FileSystem::*};

use windows::Win32::Foundation::*;

pub struct IoRing {
    pub ring: HIORING, // It has auto free?
}

#[derive(Default)]
pub struct IoRingBuilder {
    submission_queue_size: Option<u32>,
    completion_queue_size: Option<u32>,
    version: Option<IORING_VERSION>,
}

impl IoRingBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_submission_queue_size(mut self, size: u32) -> Self {
        self.submission_queue_size = Some(size);
        self
    }

    pub fn with_completion_queue_size(mut self, size: u32) -> Self {
        self.completion_queue_size = Some(size);
        self
    }

    pub fn with_version(mut self, version: IORING_VERSION) -> Self {
        self.version = Some(version);
        self
    }

    pub fn build(self) -> windows::core::Result<IoRing> {
        let submission_queue_size = self.submission_queue_size.unwrap_or(20);
        let completion_queue_size = self.completion_queue_size.unwrap_or(20);

        let version = match self.version {
            Some(v) => v,
            None => IoRing::query_io_ring_capabilities()?.MaxVersion,
        };
        IoRing::create(version, submission_queue_size, completion_queue_size)
    }
}

pub struct BufferInfo(IORING_BUFFER_INFO);

impl BufferInfo {
    pub fn into_inner(self) -> IORING_BUFFER_INFO {
        self.0
    }

    /// # Safety
    /// The buffer must be valid until the operation is popped from the completion queue.
    pub unsafe fn raw_from_vec(buffer: &mut Vec<u8>) -> Self {
        let info = IORING_BUFFER_INFO {
            Address: buffer.as_mut_ptr() as *mut _,
            Length: buffer.len() as u32,
        };

        Self(info)
    }
}

impl IoRing {
    pub fn builder() -> IoRingBuilder {
        IoRingBuilder::new()
    }

    pub fn query_io_ring_capabilities() -> windows::core::Result<IORING_CAPABILITIES> {
        unsafe { QueryIoRingCapabilities() }
    }

    pub fn create(
        version: IORING_VERSION,
        submission_queue_size: u32,
        completion_queue_size: u32,
    ) -> windows::core::Result<IoRing> {
        // currently win32 only has none flags
        let flags = IORING_CREATE_FLAGS {
            Required: IORING_CREATE_REQUIRED_FLAGS_NONE,
            Advisory: IORING_CREATE_ADVISORY_FLAGS_NONE,
        };

        let inner_ring =
            unsafe { CreateIoRing(version, flags, submission_queue_size, completion_queue_size)? };
        Ok(IoRing { ring: inner_ring })
    }

    pub fn set_io_ring_completion_event(&mut self, handle: HANDLE) -> windows::core::Result<()> {
        unsafe { SetIoRingCompletionEvent(self.ring, handle) }
    }

    /// # Safety
    /// File ref and data ref must be valid until the operation is popped from the completion queue.
    pub unsafe fn build_read_file(
        &mut self,
        op: super::ops::ReadOp,
    ) -> std::result::Result<(), Error> {
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
    }

    /// # Safety
    /// user data must be valid until the operation is popped from the completion queue.
    pub unsafe fn build_register_file_handles(
        &mut self,
        handles: &[HANDLE],
        userdata: usize,
    ) -> std::result::Result<(), Error> {
        unsafe { BuildIoRingRegisterFileHandles(self.ring, handles, userdata) }
    }

    /// # Safety
    /// buffers must be valid until the operation is popped from the completion queue.
    pub unsafe fn build_register_buffers(
        &mut self,
        buffers: &[BufferInfo],
        userdata: usize,
    ) -> std::result::Result<(), Error> {
        // Convert the types.
        let buffers = std::slice::from_raw_parts(
            buffers.as_ptr() as *const IORING_BUFFER_INFO,
            buffers.len(),
        );
        unsafe { BuildIoRingRegisterBuffers(self.ring, buffers, userdata) }
    }

    pub fn submit(
        &mut self,
        wait_operations: usize,
        milliseconds: usize,
    ) -> windows::core::Result<u32> {
        let mut submitted_entries = 0_u32;
        unsafe {
            SubmitIoRing(
                self.ring,
                wait_operations as u32,
                milliseconds as u32,
                Some(&mut submitted_entries),
            )?;
        }
        Ok(submitted_entries)
    }

    pub fn pop_completion(&mut self) -> Option<IORING_CQE> {
        let mut out = IORING_CQE::default();
        let hr = unsafe { PopIoRingCompletion(self.ring, &mut out) };
        if hr == S_OK {
            Some(out)
        } else {
            assert_eq!(hr, S_FALSE);
            None
        }
    }

    pub fn close(&mut self) -> windows::core::Result<()> {
        unsafe { CloseIoRing(self.ring) }
    }
}

//pub struct PushError;

// impl Display for PushError {
//     fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
//         f.write_str("submission queue is full")
//     }
// }

//impl std::Error for PushError {}
