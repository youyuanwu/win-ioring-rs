use windows::{core::*, Win32::Foundation::*, Win32::Storage::FileSystem::*};

use std::fs::File;
use std::os::windows::io::*;

fn main() -> Result<()> {
    let res: IORING_CAPABILITIES;
    unsafe {
        res = QueryIoRingCapabilities()?;
    }
    println!("{:?}", res);

    let flags = IORING_CREATE_FLAGS::default();

    let ring: *mut HIORING__;
    unsafe {
        ring = CreateIoRing(res.MaxVersion, flags, 100, 100)?;
    }
    println!("ring created");

    // open file from std
    let file = File::open("README.md").expect("cannot open");
    let raw_handle = file.as_raw_handle();
    println!("file opened");

    let mut file_ref = IORING_HANDLE_REF::default();
    let mut file_handle_ref = IORING_HANDLE_REF_0::default();
    file_handle_ref.Handle = HANDLE(raw_handle as isize);
    file_ref.Handle = file_handle_ref;

    let read_flags = IORING_SQE_FLAGS::default();

    let mut dataref = IORING_BUFFER_REF::default();
    let mut data_ref_0 = IORING_BUFFER_REF_0::default();
    let mut buffer = vec![0; 255];
    data_ref_0.Address = buffer.as_mut_ptr() as *mut std::ffi::c_void;
    dataref.Buffer = data_ref_0;

    let numberofbytestoread = 20;
    let fileoffset = 0;
    let userdata = 1;
    // read file
    unsafe {
        BuildIoRingReadFile(
            ring,
            file_ref,
            dataref,
            numberofbytestoread,
            fileoffset,
            userdata,
            read_flags,
        )?;
    }

    println!("read built");

    let numentry: u32;
    unsafe {
        numentry = SubmitIoRing(ring, 1 /*waitOperations*/, 4294967295u32)?;
    }

    println!("Submitted {} entries", numentry);

    unsafe {
        CloseIoRing(ring)?;
    }

    println!("ring closed");

    println!("data read: [{}]", String::from_utf8_lossy(&buffer));
    // println!("data read raw: {:?}", buffer);
    Ok(())
}
