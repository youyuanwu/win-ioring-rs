//! Errors produced by the raw ring surface.
//!
//! Two types, split by *when* they can occur, because the two have disjoint
//! remedies and a caller handling one has nothing to say about the other.
//! [`BuildError`] can only arrive while a ring is being constructed, and every
//! one of its named variants means "this host cannot give you the ring you
//! asked for". [`Error`] can only arrive once a ring exists.
//!
//! Both are views over the crate's single classification table: the mapping
//! from a Windows error code to a condition lives in exactly one place, and
//! each error type states which conditions it names rather than repeating the
//! mapping. That is why the same code cannot mean one thing here and another
//! on the file surface. (The mechanism enforcing it is crate-private, so it is
//! not linked from here; see the `view_convention` notes in `src/error.rs`.)

use std::fmt;

use crate::error::ConditionView;

/// An error produced while constructing an [`IoRing`](crate::io_ring::IoRing).
///
/// Every named variant here is a statement about the *host*, not about the
/// request: the platform has no IoRing at all, or has one too old, or lacks a
/// feature this crate requires. That is why none of the classification table's
/// conditions appear — a queue cannot be full before a queue exists, and a pipe
/// condition cannot arrive on a ring that has not been created.
#[derive(Debug)]
#[non_exhaustive]
pub enum BuildError {
    /// The host cannot provide an IoRing.
    ///
    /// Distinct from a missing import: a host that could not load the entry
    /// points would not reach this point at all.
    Unsupported,
    /// The host's maximum IoRing version is below the one requested.
    UnsupportedVersion {
        /// The version this crate asked for.
        requested: i32,
        /// The highest version the host supports.
        max_supported: i32,
    },
    /// The host does not provide a required feature flag.
    UnsupportedFeature {
        /// The flags this crate requires.
        required: i32,
        /// The flags the host reports.
        available: i32,
    },
    /// A platform error this type does not name, carried verbatim.
    Other(windows::core::Error),
}

impl BuildError {
    /// The platform error this value carries, or `None` if it carries none.
    ///
    /// `None` does not mean "no platform error was involved" — a condition that
    /// was named during classification reports `None` because the variant holds
    /// no code. See [the module docs](crate::error#recovering-the-platform-error)
    /// for the contract and the ten variants this affects.
    #[deny(clippy::wildcard_enum_match_arm)]
    pub fn os_error(&self) -> Option<&windows::core::Error> {
        match self {
            BuildError::Other(error) => Some(error),
            // `Unsupported` is `E_NOTIMPL` named, so it is platform-derived and
            // still reports `None`: naming a condition discards its code.
            BuildError::Unsupported
            | BuildError::UnsupportedVersion { .. }
            | BuildError::UnsupportedFeature { .. } => None,
        }
    }
}

/// An error produced by a ring that already exists — submission or completion.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The ring's submission queue has no room.
    ///
    /// The remedy is to **submit**: draining the queue clears the condition.
    /// This is deliberately *not* the same condition as
    /// [`runtime::Error::TooManyOperations`](crate::runtime::error::Error::TooManyOperations),
    /// whose remedy is the opposite — complete outstanding operations, and do
    /// not submit more. The two shared one variant until this design split
    /// them, and a caller who retried on the wrong one would spin.
    QueueFull,
    /// The host does not support the requested operation code.
    UnsupportedOp {
        /// The operation code that was rejected.
        op: i32,
    },
    /// The ring has been closed.
    RingClosed,
    /// A platform error this type does not name, carried verbatim.
    Other(windows::core::Error),
}

impl Error {
    /// The platform error this value carries, or `None` if it carries none.
    ///
    /// `None` does not mean "no platform error was involved" — a condition that
    /// was named during classification reports `None` because the variant holds
    /// no code. See [the module docs](crate::error#recovering-the-platform-error)
    /// for the contract and the ten variants this affects.
    #[deny(clippy::wildcard_enum_match_arm)]
    pub fn os_error(&self) -> Option<&windows::core::Error> {
        match self {
            Error::Other(error) => Some(error),
            // `QueueFull` is reached both from `IORING_E_SUBMISSION_QUEUE_FULL`
            // and from slab exhaustion, which has no code at all. It could not
            // carry one consistently, so it carries none.
            Error::QueueFull | Error::UnsupportedOp { .. } | Error::RingClosed => None,
        }
    }
}

impl BuildError {
    /// Classifies a failure from a ring-creation entry point.
    ///
    /// `E_NOTIMPL` from these calls means the host has no IoRing at all, which
    /// is a statement about the host rather than a platform error worth
    /// carrying verbatim. Everything else goes through the crate's single
    /// classification table like any other code.
    pub(crate) fn from_create_failure(err: windows::core::Error) -> Self {
        use windows::Win32::Foundation::E_NOTIMPL;
        if err.code() == E_NOTIMPL {
            BuildError::Unsupported
        } else {
            crate::error::view::<BuildError>(err.code())
        }
    }

    /// Builds an [`BuildError::UnsupportedVersion`] from platform version values.
    pub(crate) fn unsupported_version(
        requested: windows::Win32::Storage::FileSystem::IORING_VERSION,
        max: windows::Win32::Storage::FileSystem::IORING_VERSION,
    ) -> Self {
        BuildError::UnsupportedVersion {
            requested: requested.0,
            max_supported: max.0,
        }
    }
}

impl From<windows::core::Error> for BuildError {
    fn from(value: windows::core::Error) -> Self {
        crate::error::view::<BuildError>(value.code())
    }
}

impl From<windows::core::HRESULT> for BuildError {
    fn from(value: windows::core::HRESULT) -> Self {
        crate::error::view::<BuildError>(value)
    }
}

impl From<windows::core::Error> for Error {
    fn from(value: windows::core::Error) -> Self {
        crate::error::view::<Error>(value.code())
    }
}

impl From<windows::core::HRESULT> for Error {
    fn from(value: windows::core::HRESULT) -> Self {
        crate::error::view::<Error>(value)
    }
}

impl ConditionView for BuildError {
    fn queue_full(hr: windows::core::HRESULT) -> Self {
        BuildError::Other(hr.into())
    }

    fn pipe_busy(hr: windows::core::HRESULT) -> Self {
        BuildError::Other(hr.into())
    }

    fn pipe_broken(hr: windows::core::HRESULT) -> Self {
        BuildError::Other(hr.into())
    }

    fn pipe_no_peer(hr: windows::core::HRESULT) -> Self {
        BuildError::Other(hr.into())
    }

    fn pipe_listening(hr: windows::core::HRESULT) -> Self {
        BuildError::Other(hr.into())
    }

    fn other(hr: windows::core::HRESULT) -> Self {
        BuildError::Other(hr.into())
    }
}

impl ConditionView for Error {
    fn queue_full(_hr: windows::core::HRESULT) -> Self {
        Error::QueueFull
    }

    fn pipe_busy(hr: windows::core::HRESULT) -> Self {
        Error::Other(hr.into())
    }

    fn pipe_broken(hr: windows::core::HRESULT) -> Self {
        Error::Other(hr.into())
    }

    fn pipe_no_peer(hr: windows::core::HRESULT) -> Self {
        Error::Other(hr.into())
    }

    fn pipe_listening(hr: windows::core::HRESULT) -> Self {
        Error::Other(hr.into())
    }

    fn other(hr: windows::core::HRESULT) -> Self {
        Error::Other(hr.into())
    }
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::Unsupported => write!(f, "IoRing is not usable on this host"),
            BuildError::UnsupportedVersion {
                requested,
                max_supported,
            } => write!(
                f,
                "IoRing version {requested} is not supported; this host supports up to {max_supported}"
            ),
            BuildError::UnsupportedFeature {
                required,
                available,
            } => write!(
                f,
                "IoRing feature flags {required:#x} are required but this host reports {available:#x}"
            ),
            BuildError::Other(error) => write!(f, "{error}"),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::QueueFull => {
                write!(f, "the submission queue is full")
            }
            Error::UnsupportedOp { op } => {
                write!(f, "IoRing operation {op} is not supported on this host")
            }
            Error::RingClosed => write!(f, "the ring has been closed"),
            Error::Other(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for BuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // Exhaustive rather than `_ => None`. A wildcard here would silently
        // answer "no source" for a variant added later that has one, which is
        // the same shape of silent demotion the classification views exist to
        // rule out — and it is not policed by anything, since these are not
        // views.
        match self {
            BuildError::Other(error) => Some(error),
            BuildError::Unsupported
            | BuildError::UnsupportedVersion { .. }
            | BuildError::UnsupportedFeature { .. } => None,
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Other(error) => Some(error),
            Error::QueueFull | Error::UnsupportedOp { .. } | Error::RingClosed => None,
        }
    }
}

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
    fn display_is_non_empty_and_distinct_for_every_io_ring_build_error_variant() {
        let variants: Vec<BuildError> = vec![
            BuildError::Unsupported,
            BuildError::UnsupportedVersion {
                requested: 9999,
                max_supported: 400,
            },
            BuildError::UnsupportedFeature {
                required: 2,
                available: 0,
            },
            BuildError::Other(windows::core::Error::from(
                windows::Win32::Foundation::E_FAIL,
            )),
        ];
        let mut seen: Vec<String> = Vec::new();
        for v in variants {
            let rendered = v.to_string();
            assert!(!rendered.is_empty(), "empty Display for {v:?}");
            assert!(
                !seen.contains(&rendered),
                "two variants of BuildError render identically: {rendered:?}"
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
    fn _every_io_ring_build_error_variant_is_listed_above(e: &BuildError) {
        match e {
            BuildError::Unsupported
            | BuildError::UnsupportedVersion { .. }
            | BuildError::UnsupportedFeature { .. }
            | BuildError::Other(_) => {}
        }
    }

    /// Every variant renders as something, and none renders identically to
    /// another.
    ///
    /// Distinctness matters as much as non-emptiness: a caller who cannot tell
    /// two conditions apart by pattern will reach for the rendered string, and
    /// two variants sharing one message make that silently wrong.
    #[test]
    fn display_is_non_empty_and_distinct_for_every_io_ring_error_variant() {
        let variants: Vec<Error> = vec![
            Error::QueueFull,
            Error::UnsupportedOp { op: 6 },
            Error::RingClosed,
            Error::Other(windows::core::Error::from(
                windows::Win32::Foundation::E_FAIL,
            )),
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
    fn _every_io_ring_error_variant_is_listed_above(e: &Error) {
        match e {
            Error::QueueFull
            | Error::UnsupportedOp { .. }
            | Error::RingClosed
            | Error::Other(_) => {}
        }
    }
}
