use std::os::windows::io::AsRawHandle;

pub struct File {
    fd: std::fs::File,
}

impl File {
    pub fn from_std(file: std::fs::File) -> Self {
        Self { fd: file }
    }

    pub fn as_raw_handle(&self) -> windows::Win32::Foundation::HANDLE {
        windows::Win32::Foundation::HANDLE(self.fd.as_raw_handle())
    }
    // pub fn read(&self, )
}
