use windows::core::*;

use std::fs::File;
use std::os::windows::io::*;

use win_ioring_rs::*;


fn main() -> Result<()> {
    let mut ring: IoRing = IoRing::new(20).unwrap();

    println!("ring created");

    // open file from std
    let file = File::open("README.md").expect("cannot open");
    let raw_handle = file.as_raw_handle(); // TODO: fix ownership
    println!("file opened");

    let mut buffer = vec![0; 255];

    let args = args::ReadArg::new()
        .with_buffer(buffer.as_mut_ptr())
        .with_file(raw_handle)
        .with_numofbytestoread(20) // buffer needs to be bigger
        .with_offset(0)
        .with_userdata(11);

    ring.BuildIoRingReadFile(args)?;

    println!("read built");

    let numentry: u32 = ring.SubmitIoRing(1, 4294967295).unwrap();

    println!("Submitted {} entries", numentry);

    ring.CloseIoRing()?;

    println!("ring closed");

    println!("data read: [{}]", String::from_utf8_lossy(&buffer));
    // println!("data read raw: {:?}", buffer);
    Ok(())
}
