#![allow(non_snake_case)]

pub mod args;

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
        args : args::ReadArg
    ) -> std::result::Result<(), Error> {
        let read_flags = IOSQE_FLAGS_NONE;

        unsafe {
            BuildIoRingReadFile(
                self.ring, args.handle_ref, args.data_ref, args.numofbytestoread, args.offset, args.userdata, read_flags,
            )
        }
    }

    pub fn BuildIoRingRegisterFileHandles(
        &mut self,
        raw_handle_vec: Vec<RawHandle>,
        userdata: usize
    )-> std::result::Result<(), Error>{
        let handle_vec : Vec<HANDLE> = raw_handle_vec.iter().map(
            |&h| HANDLE(h as isize)
        ).collect();
        unsafe{BuildIoRingRegisterFileHandles(self.ring, &handle_vec[..], userdata)}
    }

    pub fn BuildIoRingRegisterBuffers(
        &mut self,
        buffers: Vec<(*mut u8, usize)>,
        userdata: usize
    )-> std::result::Result<(), Error>{
        let buffer_infos: Vec<IORING_BUFFER_INFO> = buffers.iter().map(
            |buff| IORING_BUFFER_INFO{Address: buff.0 as *mut std::ffi::c_void, Length: buff.1 as u32}
        ).collect();

        unsafe{BuildIoRingRegisterBuffers(self.ring, &buffer_infos[..], userdata)}
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
