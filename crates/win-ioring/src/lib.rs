//! Safe and unsafe Rust bindings for the Windows IoRing API.
//!
//! # Platform requirement
//!
//! IoRing requires Windows 11 or Windows Server 2022 or later. The platform
//! entry points are statically imported, so a binary linking this crate will
//! fail to load on an older host rather than reporting a runtime error.

pub mod buf;
pub mod error;
pub mod file;
pub mod io_ring;
pub mod sys;

pub mod runtime;

pub use buf::{BufResult, IoBuf, IoBufMut};
pub use error::{Error, Result};
