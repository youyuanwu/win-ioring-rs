#![allow(non_snake_case)]

pub mod entry;

use windows::{core::*, Win32::Storage::FileSystem::*};

use std::io;
use std::os::windows::io::*;
use windows::Win32::Foundation::*;

pub struct IoRing {
    pub ring: *mut HIORING__,
}

impl IoRing {
    // entries is size of the queue
    pub fn new(entries: u32) -> io::Result<IoRing> {
        let res: IORING_CAPABILITIES = unsafe { QueryIoRingCapabilities()? };
        // currently win32 only has none flags
        let flags = IORING_CREATE_FLAGS {
            Required: IORING_CREATE_REQUIRED_FLAGS_NONE,
            Advisory: IORING_CREATE_ADVISORY_FLAGS_NONE,
        };

        let innerring: *mut HIORING__ =
            unsafe { CreateIoRing(res.MaxVersion, flags, entries, entries)? };
        return Ok(IoRing { ring: innerring });
    }

    pub fn BuildIoRingReadFile(
        &mut self,
        raw_handle: RawHandle,
        buff: *mut u8,
        len: usize,
        offset: u64,
        userdata: usize,
    ) -> std::result::Result<(), Error> {
        let read_flags = IOSQE_FLAGS_NONE;

        let file_ref = IORING_HANDLE_REF {
            Kind: IORING_REF_RAW,
            Handle: IORING_HANDLE_REF_0 {
                Handle: HANDLE(raw_handle as isize),
            },
        };

        let dataref = IORING_BUFFER_REF {
            Kind: IORING_REF_RAW,
            Buffer: IORING_BUFFER_REF_0 {
                Address: buff as *mut std::ffi::c_void,
            },
        };

        unsafe {
            BuildIoRingReadFile(
                self.ring, file_ref, dataref, len as u32, offset, userdata, read_flags,
            )
        }
    }

    pub fn SubmitIoRing(
        &mut self,
        num_operation: usize,
        max_wait_time: usize,
    ) -> std::result::Result<u32, Error> {
        unsafe { SubmitIoRing(self.ring, num_operation as u32, max_wait_time as u32) }
    }

    pub fn CloseIoRing(&mut self) -> std::result::Result<(), windows::core::Error> {
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
