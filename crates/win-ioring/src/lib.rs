#![deny(missing_docs)]
// Every unsafe block in shipped code must say why it is sound. Test code is
// exempt: its unsafe blocks exist to set up or probe the very invariants under
// test, and are read alongside the assertion they serve.
#![cfg_attr(not(test), deny(clippy::undocumented_unsafe_blocks))]
// The README is the crate's front page. Including it here means its example is
// compiled as a doc-test, so the first code a reader sees cannot rot.
#![doc = include_str!("../../../README.md")]
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
//! ## A registered buffer in flight is unreachable
//!
//! A registered operation takes the buffer's handle **by value**, so while the
//! kernel may be writing into it the application has no way to name it. This is
//! what makes registration safe without a runtime check: the exclusion is
//! ordinary move semantics.
//!
//! ```compile_fail
//! use win_ioring::file::File;
//! use win_ioring::io_ring::IoRing;
//! use win_ioring::runtime::{Driver, FileTarget};
//!
//! # async fn demo() {
//! let ring = IoRing::builder().build().unwrap();
//! let driver = Driver::new(ring).unwrap();
//! let handle = driver.handle();
//! let file = File::open("Cargo.toml").unwrap();
//!
//! let buffers = handle.register_buffers(vec![vec![0_u8; 64]]).await.unwrap();
//! let buffer = buffers.check_out(0).unwrap();
//!
//! let op = handle.read_registered(FileTarget::Owned(&file), buffer, 0, 8, 0);
//! // The handle moved into the operation, so reading it here is a use after
//! // move — `E0382`, which is exactly the race this design prevents.
//! let _peek = buffer[0];
//! let _ = op.await;
//! # }
//! ```
//!
//! Reading it *after* the operation returns it is how the bytes are meant to be
//! reached, and that compiles:
//!
//! ```
//! use win_ioring::file::File;
//! use win_ioring::io_ring::IoRing;
//! use win_ioring::runtime::{Driver, FileTarget};
//!
//! # async fn demo() {
//! let ring = IoRing::builder().build().unwrap();
//! let driver = Driver::new(ring).unwrap();
//! let handle = driver.handle();
//! let file = File::open("Cargo.toml").unwrap();
//!
//! let buffers = handle.register_buffers(vec![vec![0_u8; 64]]).await.unwrap();
//! let buffer = buffers.check_out(0).unwrap();
//!
//! let (result, buffer) = handle
//!     .read_registered(FileTarget::Owned(&file), buffer, 0, 8, 0)
//!     .await
//!     .into_parts();
//! let _read = result.unwrap();
//! let _peek = buffer[0];
//! # }
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

/// Thin wrappers over the Windows primitives the crate needs.
///
/// Exposed because the raw layer in [`io_ring`] needs a completion event, and a
/// caller driving the ring by hand needs to be able to make one.
pub mod sys;

pub mod runtime;

pub use buf::{BufResult, IoBuf, IoBufMut};
pub use error::{Error, Result};
pub use runtime::ShutdownMode;
