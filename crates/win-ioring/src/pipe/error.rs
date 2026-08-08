//! The error produced by the pipe surface.
//!
//! Twelve conditions. This is the type the whole design is for: a pipe's
//! failures are *named* here — busy, broken, no peer, listening — where a file
//! surface has no business naming them and a caller of a file surface has no
//! way to act on them.
//!
//! Two conditions the file surface names are absent, and both are structural
//! rather than a judgement:
//!
//! - `NotSeekable`, because the pipe surface offers only positional operations
//!   against a handle that is by construction not seekable;
//! - `OperationOutstanding`, because it is produced only after
//!   `SequentialGuard::claim`, and a pipe never passes the guard that precedes
//!   it. Its pipe-side counterpart is [`Error::AcceptOutstanding`].

use std::fmt;

use crate::error::ConditionView;

/// An error produced by an operation on a pipe.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// All pipe instances are busy.
    Busy,
    /// The peer closed its end of the pipe.
    Broken,
    /// The pipe has no peer connected.
    NoPeer,
    /// The pipe is listening and has not yet been connected to.
    ///
    /// Reached three ways that never pass through the classification table —
    /// the server produces it directly — as well as from a completion.
    Listening,
    /// An accept is already in flight on this server.
    AcceptOutstanding,
    /// The ring itself failed.
    Ring(crate::io_ring::error::Error),
    /// A buffer did not satisfy its contract.
    Buf(crate::buf::error::Error),
    /// The runtime is shutting down and will not accept new work.
    ShuttingDown,
    /// An operation was submitted without a field it requires.
    MissingField {
        /// The field's name.
        field: &'static str,
    },
    /// The operation was abandoned because the runtime shut down under it.
    AbandonedAtShutdown,
    /// The driver is already tracking as many operations as it can.
    ///
    /// Complete outstanding operations; submitting more cannot help.
    TooManyOperations,
    /// A platform error this type does not name, carried verbatim.
    Other(windows::core::Error),
}

impl ConditionView for Error {
    fn queue_full(_hr: windows::core::HRESULT) -> Self {
        Error::Ring(crate::io_ring::error::Error::QueueFull)
    }

    fn pipe_busy(_hr: windows::core::HRESULT) -> Self {
        Error::Busy
    }

    fn pipe_broken(_hr: windows::core::HRESULT) -> Self {
        Error::Broken
    }

    fn pipe_no_peer(_hr: windows::core::HRESULT) -> Self {
        Error::NoPeer
    }

    fn pipe_listening(_hr: windows::core::HRESULT) -> Self {
        Error::Listening
    }

    fn other(hr: windows::core::HRESULT) -> Self {
        Error::Other(hr.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Busy => write!(f, "all pipe instances are busy"),
            Error::Broken => write!(f, "the pipe's peer closed its end"),
            Error::NoPeer => write!(f, "the pipe has no peer connected"),
            Error::Listening => write!(f, "the pipe is listening and not yet connected"),
            Error::AcceptOutstanding => {
                write!(f, "an accept is already in flight on this server")
            }
            Error::Ring(error) => write!(f, "{error}"),
            Error::Buf(error) => write!(f, "{error}"),
            Error::ShuttingDown => write!(f, "the runtime is shutting down"),
            Error::MissingField { field } => {
                write!(f, "the operation is missing a required field: {field}")
            }
            Error::AbandonedAtShutdown => {
                write!(f, "the operation was abandoned when the runtime shut down")
            }
            Error::TooManyOperations => write!(
                f,
                "the driver is tracking as many operations as it can; complete \
                 outstanding operations rather than submitting more"
            ),
            Error::Other(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Ring(error) => Some(error),
            Error::Buf(error) => Some(error),
            Error::Other(error) => Some(error),
            Error::Busy
            | Error::Broken
            | Error::NoPeer
            | Error::Listening
            | Error::AcceptOutstanding
            | Error::ShuttingDown
            | Error::MissingField { .. }
            | Error::AbandonedAtShutdown
            | Error::TooManyOperations => None,
        }
    }
}
