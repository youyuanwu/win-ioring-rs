//! Safe and unsafe Rust bindings for the Windows IoRing API.
//!
//! # Platform requirement
//!
//! IoRing requires Windows 11 or Windows Server 2022 or later. The platform
//! entry points are statically imported, so a binary linking this crate will
//! fail to load on an older host rather than reporting a runtime error.
//!
//! # Compile-time guarantees
//!
//! Several of this crate's guarantees are enforced by the type system rather
//! than at run time. The doc-tests below assert them: each is a `compile_fail`
//! snippet that is otherwise valid, so it can only fail for the stated reason.
//! Run them with `cargo test --doc`.
//!
//! ## The driver and its handle are single-threaded
//!
//! [`runtime::Driver`] and [`runtime::Handle`] share state through `Rc` and
//! `RefCell` and must never cross a thread boundary. Requiring `Send` of a
//! `Driver` does not compile:
//!
//! ```compile_fail
//! use win_ioring::io_ring::IoRing;
//! use win_ioring::runtime::Driver;
//!
//! fn requires_send<T: Send>(_: &T) {}
//!
//! let ring = IoRing::builder().build().unwrap();
//! let driver = Driver::new(ring).unwrap();
//! requires_send(&driver);
//! ```
//!
//! Nor does a [`runtime::Handle`]:
//!
//! ```compile_fail
//! use win_ioring::io_ring::IoRing;
//! use win_ioring::runtime::Driver;
//!
//! fn requires_send<T: Send>(_: &T) {}
//!
//! let ring = IoRing::builder().build().unwrap();
//! let driver = Driver::new(ring).unwrap();
//! let handle = driver.handle();
//! requires_send(&handle);
//! ```
//!
//! Nor does an operation future, which holds a weak reference to the driver:
//!
//! ```compile_fail
//! use win_ioring::file::File;
//! use win_ioring::io_ring::IoRing;
//! use win_ioring::runtime::Driver;
//!
//! fn requires_send<T: Send>(_: &T) {}
//!
//! let ring = IoRing::builder().build().unwrap();
//! let driver = Driver::new(ring).unwrap();
//! let handle = driver.handle();
//! let file = File::open("Cargo.toml").unwrap();
//! let operation = handle.read(&file, vec![0_u8; 8], 8, 0);
//! requires_send(&operation);
//! ```
//!
//! The same code compiles without the `Send` bound, which is what makes the
//! failures above meaningful rather than incidental:
//!
//! ```
//! use win_ioring::io_ring::IoRing;
//! use win_ioring::runtime::Driver;
//!
//! fn accepts_anything<T>(_: &T) {}
//!
//! let ring = IoRing::builder().build().unwrap();
//! let driver = Driver::new(ring).unwrap();
//! let handle = driver.handle();
//! accepts_anything(&driver);
//! accepts_anything(&handle);
//! ```
//!
//! ## Two sequential operations cannot coexist
//!
//! [`file::File::read`] and [`file::File::write`] borrow the file exclusively
//! for as long as their future lives, so a second one cannot be started while
//! the first is alive:
//!
//! ```compile_fail
//! use win_ioring::file::File;
//! use win_ioring::io_ring::IoRing;
//! use win_ioring::runtime::Driver;
//!
//! let ring = IoRing::builder().build().unwrap();
//! let driver = Driver::new(ring).unwrap();
//! let handle = driver.handle();
//! let mut file = File::open("Cargo.toml").unwrap();
//!
//! let first = file.read(&handle, vec![0_u8; 8], 8);
//! let second = file.read(&handle, vec![0_u8; 8], 8);
//! // Using `first` here keeps its borrow live past the second call, so this
//! // fails on the conflicting borrow rather than because the borrow had
//! // already ended.
//! drop(first);
//! drop(second);
//! ```
//!
//! Positional operations take only shared access, so any number may coexist:
//!
//! ```
//! use win_ioring::file::File;
//! use win_ioring::io_ring::IoRing;
//! use win_ioring::runtime::Driver;
//!
//! let ring = IoRing::builder().build().unwrap();
//! let driver = Driver::new(ring).unwrap();
//! let handle = driver.handle();
//! let file = File::open("Cargo.toml").unwrap();
//!
//! let first = file.read_at(&handle, vec![0_u8; 8], 8, 0);
//! let second = file.read_at(&handle, vec![0_u8; 8], 8, 8);
//! drop(first);
//! drop(second);
//! ```
//!
//! ## The buffer contracts cannot be implemented safely
//!
//! [`IoBuf`] and [`IoBufMut`] carry obligations the compiler cannot check, so
//! they are `unsafe` traits. A safe `impl` does not compile:
//!
//! ```compile_fail
//! struct MyBuffer(Vec<u8>);
//!
//! impl win_ioring::IoBuf for MyBuffer {
//!     fn buf_ptr(&self) -> *const u8 {
//!         self.0.as_ptr()
//!     }
//!     fn buf_len(&self) -> usize {
//!         self.0.len()
//!     }
//! }
//! ```
//!
//! The identical `impl` written as `unsafe impl` compiles. Note what this pair
//! does and does not prove: it establishes only that the `unsafe` marker is
//! required, not that any particular implementation upholds the pointer
//! stability and initialization obligations, which no test can check.
//!
//! ```
//! struct MyBuffer(Vec<u8>);
//!
//! // SAFETY: the pointer and length come from the inner `Vec`, which is never
//! // reallocated here, and the buffer has no interior mutability.
//! unsafe impl win_ioring::IoBuf for MyBuffer {
//!     fn buf_ptr(&self) -> *const u8 {
//!         self.0.as_ptr()
//!     }
//!     fn buf_len(&self) -> usize {
//!         self.0.len()
//!     }
//! }
//! ```

pub mod buf;
pub mod error;
pub mod file;
pub mod io_ring;
pub mod sys;

pub mod runtime;

pub use buf::{BufResult, IoBuf, IoBufMut};
pub use error::{Error, Result};
