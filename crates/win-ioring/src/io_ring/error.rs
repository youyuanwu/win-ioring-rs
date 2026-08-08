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
            BuildError::Unsupported => write!(f, "this host does not provide an IoRing"),
            BuildError::UnsupportedVersion {
                requested,
                max_supported,
            } => write!(
                f,
                "IoRing version {requested} requested, but this host supports at \
                 most {max_supported}"
            ),
            BuildError::UnsupportedFeature {
                required,
                available,
            } => write!(
                f,
                "IoRing features {required:#x} required, but this host provides \
                 {available:#x}"
            ),
            BuildError::Other(error) => write!(f, "{error}"),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::QueueFull => {
                write!(f, "the ring's submission queue is full; submit to drain it")
            }
            Error::UnsupportedOp { op } => {
                write!(f, "this host does not support IoRing operation {op}")
            }
            Error::RingClosed => write!(f, "the ring is closed"),
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
