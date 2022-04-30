use windows::{Win32::Foundation::*, Win32::Storage::FileSystem::*};

// use std::io;
use std::os::windows::io::*;

pub struct ReadArg {
    pub handle_ref: IORING_HANDLE_REF,
    pub data_ref: IORING_BUFFER_REF,
    pub numofbytestoread: u32,
    pub offset : u64,
    pub userdata: usize
}

impl ReadArg {

    pub fn new() -> ReadArg {
        return ReadArg{
            handle_ref : IORING_HANDLE_REF::default(),
            data_ref : IORING_BUFFER_REF::default(),
            numofbytestoread: 0,
            offset: 0,
            userdata: 0
        }
    }

    pub fn with_file(mut self, raw_handle: RawHandle) -> ReadArg{
        let file_ref = IORING_HANDLE_REF {
            Kind: IORING_REF_RAW,
            Handle: IORING_HANDLE_REF_0 {
                Handle: HANDLE(raw_handle as isize),
            },
        };
        self.handle_ref = file_ref;
        self
    }

    pub fn with_regestered_file(mut self, index :u32) -> ReadArg{
        let file_ref = IORING_HANDLE_REF {
            Kind: IORING_REF_REGISTERED,
            Handle: IORING_HANDLE_REF_0 {
                Index: index
            },
        };
        self.handle_ref = file_ref;
        self
    }

    pub fn with_buffer(mut self, buff: *mut u8) -> ReadArg{
        let dataref = IORING_BUFFER_REF {
            Kind: IORING_REF_RAW,
            Buffer: IORING_BUFFER_REF_0 {
                Address: buff as *mut std::ffi::c_void,
            },
        };
        self.data_ref = dataref;
        self
    }

    // buffer index and the offset of the buffer
    pub fn with_registered_buffer(mut self, index: u32, offset: u32) -> ReadArg{
        let dataref = IORING_BUFFER_REF {
            Kind: IORING_REF_REGISTERED,
            Buffer: IORING_BUFFER_REF_0 {
                IndexAndOffset: IORING_REGISTERED_BUFFER{
                    BufferIndex: index,
                    Offset: offset
                }
            },
        };
        self.data_ref = dataref;
        self
    }

    pub fn with_numofbytestoread(mut self, numofbytestoread: u32) -> ReadArg
    {
        self.numofbytestoread = numofbytestoread;
        self
    }

    pub fn with_offset(mut self, offset: u64) -> ReadArg{
        self.offset = offset;
        self
    }
    
    pub fn with_userdata(mut self, userdata: usize) -> ReadArg{
        self.userdata = userdata;
        self
    }
}