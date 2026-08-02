//! The abstract file interface every backend implements.
//!
//! One piece of application logic is written against this and run unmodified
//! against each backend, so a difference in the numbers is attributable to the
//! backend rather than to the benchmark. That is the whole basis of the
//! comparison, and it is why nothing below mentions a concrete implementation.
//!
//! # Shape, and why
//!
//! Operations take a buffer **by value** and return it. That is the natural
//! shape for a completion-based backend, which must own the buffer for the
//! kernel's benefit, and costs a thread-pool backend nothing: it must hand an
//! owned buffer to its worker anyway. An interface built around borrowed slices
//! would instead force the completion-based side into a copy, and the
//! comparison would be measuring the interface.
//!
//! Nothing here requires `Send`. One backend's futures are `!Send` by
//! construction, so a `Send` bound would exclude it outright.

use std::io;

/// What a backend gives back from an operation: the result, and the buffer.
pub type OpResult<T, B> = (io::Result<T>, B);

/// A file I/O backend.
///
/// Each implementation also owns whatever runtime setup it needs. That setup is
/// performed outside the measured region, so a backend that must build a ring or
/// spawn a pool is not charged for it while one that need not is idle.
pub trait Backend {
    /// The buffer type this backend's operations take and return.
    type Buf: Buffer;
    /// An open file.
    type File;

    /// A short name for the report, including the configuration that varies.
    fn name(&self) -> String;

    /// Describes the configuration in enough detail to reproduce it.
    ///
    /// Printed beside the results, because two backends can differ in ways the
    /// name does not carry — thread-pool width, for instance.
    fn configuration(&self) -> String;

    /// Opens a file for reading.
    fn open_read(&self, path: &std::path::Path) -> io::Result<Self::File>;

    /// Opens a file for writing, creating or truncating it.
    fn open_write(&self, path: &std::path::Path) -> io::Result<Self::File>;

    /// Takes a buffer of at least `capacity` bytes from the backend's pool.
    ///
    /// Buffers come from the backend so that one which registers them can hand
    /// out registered ones. Allocation happens when the pool is built, not here,
    /// so a run's allocation count is fixed and independent of its operation
    /// count.
    fn take_buffer(&self, capacity: usize) -> io::Result<Self::Buf>;

    /// Returns a buffer to the pool.
    fn put_buffer(&self, buffer: Self::Buf);

    /// Reads `len` bytes at `offset` into `buffer`.
    fn read_at(
        &self,
        file: &Self::File,
        buffer: Self::Buf,
        len: u32,
        offset: u64,
    ) -> impl Future<Output = OpResult<u32, Self::Buf>>;

    /// Writes `len` bytes from `buffer` at `offset`.
    fn write_at(
        &self,
        file: &Self::File,
        buffer: Self::Buf,
        len: u32,
        offset: u64,
    ) -> impl Future<Output = OpResult<u32, Self::Buf>>;

    /// Commits outstanding writes, so a subsequent read observes them.
    fn sync(&self, file: &Self::File) -> impl Future<Output = io::Result<()>>;
}

/// The application's view of a buffer.
///
/// Deliberately small: the scenarios need to fill a buffer before a write and
/// read what arrived after a read, and nothing else. Keeping it to that is what
/// lets both an owned `Vec` and a registered handle satisfy it without either
/// being bent out of shape.
pub trait Buffer {
    /// The bytes the application may read.
    fn bytes(&self) -> &[u8];

    /// Copies `src` in, from the buffer's first byte.
    fn fill(&mut self, src: &[u8]) -> io::Result<()>;
}

/// Whether a backend can run on this host, and why not if it cannot.
///
/// A host may lack the platform facility, or an operation a backend needs. Such
/// a backend is reported as unavailable and the rest are still measured, rather
/// than the whole run failing.
pub enum Availability {
    /// The backend can run.
    Available,
    /// The backend cannot run here.
    Unavailable(String),
}
