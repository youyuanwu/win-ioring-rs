//! The error produced by buffer contract checks.
//!
//! Two conditions, both about the buffer the *caller* supplied rather than
//! about the platform, which is why this type is the one error type in the
//! crate that is **not** a view over the classification table: no `HRESULT` can
//! produce either variant, so there is nothing for a view to map.

use std::fmt;

/// A buffer does not satisfy the contract for the operation it was passed to.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// A read was asked for more bytes than the buffer can hold.
    TooSmall {
        /// Bytes requested.
        requested: u64,
        /// Bytes the buffer can accept.
        available: u64,
    },
    /// A write was asked to send bytes the buffer has not initialized.
    ///
    /// Sending them would transmit whatever happened to be in memory, so this
    /// is refused rather than truncated.
    UninitializedWriteRange {
        /// Bytes requested.
        requested: u64,
        /// Bytes the buffer has initialized.
        initialized: u64,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::TooSmall {
                requested,
                available,
            } => write!(
                f,
                "buffer too small: {requested} bytes requested, {available} available"
            ),
            Error::UninitializedWriteRange {
                requested,
                initialized,
            } => write!(
                f,
                "write of {requested} bytes would send uninitialized memory: only \
                 {initialized} bytes are initialized"
            ),
        }
    }
}

impl std::error::Error for Error {}
