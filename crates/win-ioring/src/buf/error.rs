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

impl Error {
    /// The platform error this value carries, or `None` if it carries none.
    ///
    /// `None` does not mean "no platform error was involved" — a condition that
    /// was named during classification reports `None` because the variant holds
    /// no code. See [the module docs](crate::error#recovering-the-platform-error)
    /// for the contract and the six variants this affects.
    #[deny(clippy::wildcard_enum_match_arm)]
    pub fn os_error(&self) -> Option<&windows::core::Error> {
        match self {
            // Both conditions are counted by this crate; neither has an
            // `HRESULT` behind it. Named exhaustively anyway so that a variant
            // added later must decide rather than inherit `None`.
            Error::TooSmall { .. } | Error::UninitializedWriteRange { .. } => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::TooSmall {
                requested,
                available,
            } => write!(
                f,
                "buffer too small: {requested} bytes requested but only {available} available"
            ),
            Error::UninitializedWriteRange {
                requested,
                initialized,
            } => write!(
                f,
                "write of {requested} bytes would read past {initialized} initialized bytes"
            ),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod display_tests {
    use super::*;

    /// Every variant renders as something, and none renders identically to
    /// another.
    ///
    /// Distinctness matters as much as non-emptiness: a caller who cannot tell
    /// two conditions apart by pattern will reach for the rendered string, and
    /// two variants sharing one message make that silently wrong.
    #[test]
    fn display_is_non_empty_and_distinct_for_every_buf_error_variant() {
        let variants: Vec<Error> = vec![
            Error::TooSmall {
                requested: 10,
                available: 4,
            },
            Error::UninitializedWriteRange {
                requested: 10,
                initialized: 4,
            },
        ];
        let mut seen: Vec<String> = Vec::new();
        for v in variants {
            let rendered = v.to_string();
            assert!(!rendered.is_empty(), "empty Display for {v:?}");
            assert!(
                !seen.contains(&rendered),
                "two variants of Error render identically: {rendered:?}"
            );
            seen.push(rendered);
        }
    }

    /// Fails to compile when a variant is added, so the list above cannot
    /// silently fall behind.
    ///
    /// The list is written by hand and nothing else would notice an omission.
    /// This lives beside the type rather than in a central suite so the error
    /// lands in front of whoever adds the variant.
    fn _every_buf_error_variant_is_listed_above(e: &Error) {
        match e {
            Error::TooSmall { .. } | Error::UninitializedWriteRange { .. } => {}
        }
    }
}
