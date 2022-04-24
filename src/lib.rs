#![allow(non_snake_case)]

pub mod entry;

use windows::{core::*, Win32::Storage::FileSystem::*};

use std::io;

pub struct IoRing {
    pub ring: *mut HIORING__,
}

impl IoRing {
    // entries is size of the queue
    pub fn new(entries: u32) -> io::Result<IoRing> {
        let res: IORING_CAPABILITIES = unsafe { QueryIoRingCapabilities()? };
        let flags = IORING_CREATE_FLAGS::default();

        let innerring: *mut HIORING__ =
            unsafe { CreateIoRing(res.MaxVersion, flags, entries, entries)? };
        return Ok(IoRing { ring: innerring });
    }

    pub fn BuildIoRingReadFile(&mut self, entry : entry::Read) -> std::result::Result<(),Error> {

        let read_flags = IORING_SQE_FLAGS::default();

        unsafe {
            BuildIoRingReadFile(
                self.ring,
                entry.handle_ref,
                entry.data_ref,
                entry.len,
                entry.offset as u64,
                entry.userdata as usize,
                read_flags,
            )
        }
    }

    pub fn SubmitIoRing(&mut self, num_operation: usize, max_wait_time: usize) -> std::result::Result<u32,Error>{
        unsafe{SubmitIoRing(self.ring, num_operation as u32, max_wait_time as u32)}
    }

    pub fn CloseIoRing(&mut self) -> std::result::Result<(), windows::core::Error>
    {
        unsafe{CloseIoRing(self.ring)}
    }
}


//pub struct PushError;

// impl Display for PushError {
//     fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
//         f.write_str("submission queue is full")
//     }
// }

//impl std::Error for PushError {}