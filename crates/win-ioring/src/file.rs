//! File handles usable with an IoRing.
//!
//! An operation can outlive both the future awaiting it and the caller's own
//! reference to the file, because the kernel keeps working until the operation
//! completes. The handle must therefore stay open for at least that long.
//!
//! [`File`] holds its state behind an [`Rc`], and submitting an operation clones
//! that handle into the driver's storage. The underlying OS handle is closed
//! only once the caller's `File`, and every operation naming it, are gone.
//!
//! This is also why the safe API never accepts a bare `HANDLE`: no safe
//! signature could promise that a raw handle outlives an operation whose future
//! was dropped. Raw handles remain available through [`crate::io_ring`], which
//! is unsafe and documents the obligation.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::rc::Rc;

use windows::Win32::Foundation::HANDLE;

/// State shared between a [`File`] and any operations naming it.
///
/// Phase 5 extends this with the sequential cursor and its outstanding-operation
/// flag; for now it owns the handle.
#[derive(Debug)]
pub struct FileState {
    handle: OwnedHandle,
}

impl FileState {
    /// Returns the raw OS handle.
    ///
    /// The handle is valid for as long as this state is alive, which the
    /// reference count guarantees.
    pub fn raw_handle(&self) -> HANDLE {
        HANDLE(self.handle.as_raw_handle())
    }
}

/// A file that IoRing operations can target.
///
/// Cloning is cheap and shares the same underlying handle.
#[derive(Debug, Clone)]
pub struct File {
    state: Rc<FileState>,
}

impl File {
    /// Adopts an already-open standard library file.
    ///
    /// Ownership transfers: the handle is closed when the last reference to it
    /// goes away, which includes any operation still in flight.
    pub fn from_std(file: std::fs::File) -> Self {
        Self {
            state: Rc::new(FileState {
                handle: OwnedHandle::from(file),
            }),
        }
    }

    /// Opens a file for reading.
    pub fn open(path: impl AsRef<std::path::Path>) -> std::io::Result<Self> {
        Ok(Self::from_std(std::fs::File::open(path)?))
    }

    /// Creates or truncates a file for writing.
    pub fn create(path: impl AsRef<std::path::Path>) -> std::io::Result<Self> {
        Ok(Self::from_std(std::fs::File::create(path)?))
    }

    /// Adopts a raw OS handle.
    ///
    /// # Safety
    ///
    /// `handle` must be a valid, open handle that this `File` may take sole
    /// ownership of, and which is not owned by anything else.
    pub unsafe fn from_raw_handle(handle: HANDLE) -> Self {
        Self {
            state: Rc::new(FileState {
                handle: unsafe { OwnedHandle::from_raw_handle(handle.0) },
            }),
        }
    }

    /// Returns the shared state, for the driver to retain alongside an
    /// operation.
    pub(crate) fn state(&self) -> Rc<FileState> {
        Rc::clone(&self.state)
    }

    /// Returns the raw OS handle.
    ///
    /// Prefer passing the `File` itself to an operation. This is exposed for
    /// building operations against the raw layer, where the caller takes on the
    /// obligation of keeping the file alive.
    pub fn as_raw_handle(&self) -> HANDLE {
        self.state.raw_handle()
    }

    /// Returns the number of live references to this file's handle.
    ///
    /// Counts the caller's own `File` values and any operations the driver is
    /// still tracking.
    pub fn reference_count(&self) -> usize {
        Rc::strong_count(&self.state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(tag: &str) -> (std::path::PathBuf, std::fs::File) {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "win-ioring-file-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let file = std::fs::File::create(&path).unwrap();
        (path, file)
    }

    #[test]
    fn adopting_a_std_file_takes_ownership() {
        let (path, std_file) = temp_file("adopt");
        let file = File::from_std(std_file);
        assert_eq!(file.reference_count(), 1);
        assert!(!file.as_raw_handle().is_invalid());
        drop(file);
        let _ = std::fs::remove_file(&path);
    }

    /// Cloning shares one handle, so an operation holding a clone keeps the
    /// handle open after the caller drops theirs.
    #[test]
    fn clones_share_one_handle() {
        let (path, std_file) = temp_file("clone");
        let file = File::from_std(std_file);
        let retained = file.state();

        assert_eq!(file.reference_count(), 2);
        let raw = file.as_raw_handle();

        // The caller drops their reference while the "operation" still holds one.
        drop(file);
        assert_eq!(Rc::strong_count(&retained), 1);
        assert_eq!(retained.raw_handle().0, raw.0, "handle changed identity");

        drop(retained);
        let _ = std::fs::remove_file(&path);
    }

    /// The handle must be released exactly once, when the last reference goes.
    #[test]
    fn handle_is_closed_once_the_last_reference_goes() {
        use windows::Win32::Foundation::GetHandleInformation;

        let (path, std_file) = temp_file("close");
        let file = File::from_std(std_file);
        let raw = file.as_raw_handle();
        let retained = file.state();

        // Still open while a reference remains.
        let mut flags = 0_u32;
        drop(file);
        assert!(
            unsafe { GetHandleInformation(HANDLE(raw.0), &mut flags) }.is_ok(),
            "handle closed while an operation still referenced it"
        );

        drop(retained);
        assert!(
            unsafe { GetHandleInformation(HANDLE(raw.0), &mut flags) }.is_err(),
            "handle was not closed after the last reference went away"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn open_and_create_round_trip() {
        let mut path = std::env::temp_dir();
        path.push(format!("win-ioring-file-open-{}", std::process::id()));
        std::fs::write(&path, b"hello").unwrap();

        let file = File::open(&path).unwrap();
        assert!(!file.as_raw_handle().is_invalid());
        drop(file);

        let created = File::create(&path).unwrap();
        assert!(!created.as_raw_handle().is_invalid());
        drop(created);

        let _ = std::fs::remove_file(&path);
    }
}
