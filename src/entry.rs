use windows::{Win32::Foundation::*, Win32::Storage::FileSystem::*};

use std::io;
use std::os::windows::io::*;

pub struct Read {
    pub handle_ref: IORING_HANDLE_REF,
    pub data_ref: IORING_BUFFER_REF,
    pub len: u32,
    pub offset : u32,
    pub userdata: u32
}

impl Read {
    pub fn new(raw_handle: RawHandle, buf: & mut Vec<u8>, len: usize) -> io::Result<Read> {
        let file_ref = IORING_HANDLE_REF{
            Kind: IORING_REF_RAW,
            Handle : IORING_HANDLE_REF_0{
                Handle: HANDLE(raw_handle as isize)
            }
        };

        let dataref = IORING_BUFFER_REF{
            Kind : IORING_REF_RAW,
            Buffer : IORING_BUFFER_REF_0{
                Address : buf.as_mut_ptr() as *mut std::ffi::c_void
            }
        };
        return Ok(Read {
            handle_ref: file_ref,
            data_ref: dataref,
            len: len as u32,
            offset: 0,
            userdata: 0
        });
    }

    pub fn userdata(&mut self, data: u32)-> &mut Read{
        self.userdata = data;
        self
    }


}
