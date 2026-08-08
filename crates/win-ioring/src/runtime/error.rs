//! The error produced by the driver surface.
//!
//! This is the widest of the six types, and deliberately so: the driver is the
//! one surface that is *handle-agnostic*. [`Handle::read`](crate::runtime::Handle::read)
//! and [`Handle::write`](crate::runtime::Handle::write) take a
//! [`File`](crate::file::File), and both [`pipe::Client::file`](crate::pipe::Client::file)
//! and [`pipe::Server::file`](crate::pipe::Server::file) hand one out, so a
//! pipe's completions reach this surface. That is why the four pipe conditions
//! are **named** here rather than demoted to [`Error::Other`]: they are
//! genuinely reachable, and today's single error type already names them, so
//! demoting them would take away a caller's ability to learn that its pipe
//! broke.

use std::fmt;

use crate::error::ConditionView;

/// An error produced by the driver surface, including registered I/O.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The ring itself failed.
    Ring(crate::io_ring::error::Error),
    /// A buffer did not satisfy its contract.
    Buf(crate::buf::error::Error),
    /// The runtime is shutting down and will not accept new work.
    ShuttingDown,
    /// The operation was abandoned because the runtime shut down under it.
    AbandonedAtShutdown,
    /// Shutdown could not complete because operations remain outstanding.
    ShutdownStalled {
        /// How many operations were still outstanding.
        outstanding: usize,
    },
    /// An operation was submitted without a field it requires.
    MissingField {
        /// The field's name.
        field: &'static str,
    },
    /// The driver is already tracking as many operations as it can.
    ///
    /// The remedy is to **complete** outstanding operations. Submitting more
    /// cannot help, which is what distinguishes this from
    /// [`io_ring::Error::QueueFull`](crate::io_ring::error::Error::QueueFull) —
    /// whose remedy is the opposite. The two shared one variant until this
    /// design split them.
    TooManyOperations,
    /// No buffer is registered at the given index.
    InvalidRegisteredIndex {
        /// The index that was named.
        index: u32,
    },
    /// A range fell outside the registered buffer it named.
    RegisteredRangeOutOfBounds {
        /// The registered buffer's index.
        index: u32,
        /// Offset requested.
        offset: u64,
        /// Length requested.
        length: u64,
        /// The buffer's extent.
        extent: u64,
    },
    /// The registered buffer is checked out by an operation in flight.
    BufferCheckedOut {
        /// The registered buffer's index.
        index: u32,
    },
    /// The registration was replaced before this operation could use it.
    RegistrationSuperseded,
    /// The registration has not completed yet.
    RegistrationPending,
    /// All pipe instances are busy.
    PipeBusy,
    /// The pipe's peer closed its end.
    PipeBroken,
    /// The pipe has no peer connected.
    PipeNoPeer,
    /// The pipe is listening and has not yet been connected to.
    PipeListening,
    /// A platform error this type does not name, carried verbatim.
    Other(windows::core::Error),
}

impl ConditionView for Error {
    fn queue_full(_hr: windows::core::HRESULT) -> Self {
        Error::Ring(crate::io_ring::error::Error::QueueFull)
    }

    fn pipe_busy(_hr: windows::core::HRESULT) -> Self {
        Error::PipeBusy
    }

    fn pipe_broken(_hr: windows::core::HRESULT) -> Self {
        Error::PipeBroken
    }

    fn pipe_no_peer(_hr: windows::core::HRESULT) -> Self {
        Error::PipeNoPeer
    }

    fn pipe_listening(_hr: windows::core::HRESULT) -> Self {
        Error::PipeListening
    }

    fn other(hr: windows::core::HRESULT) -> Self {
        Error::Other(hr.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Ring(error) => write!(f, "{error}"),
            Error::Buf(error) => write!(f, "{error}"),
            Error::ShuttingDown => write!(f, "the runtime is shutting down"),
            Error::AbandonedAtShutdown => {
                write!(f, "the operation was abandoned when the runtime shut down")
            }
            Error::ShutdownStalled { outstanding } => write!(
                f,
                "shutdown stalled with {outstanding} operation(s) outstanding"
            ),
            Error::MissingField { field } => {
                write!(f, "the operation is missing a required field: {field}")
            }
            Error::TooManyOperations => write!(
                f,
                "the driver is tracking as many operations as it can; complete \
                 outstanding operations rather than submitting more"
            ),
            Error::InvalidRegisteredIndex { index } => {
                write!(f, "no buffer is registered at index {index}")
            }
            Error::RegisteredRangeOutOfBounds {
                index,
                offset,
                length,
                extent,
            } => write!(
                f,
                "range {offset}..{} is outside registered buffer {index}, whose \
                 extent is {extent}",
                offset.saturating_add(*length)
            ),
            Error::BufferCheckedOut { index } => write!(
                f,
                "registered buffer {index} is checked out by an operation in flight"
            ),
            Error::RegistrationSuperseded => {
                write!(f, "the registration was superseded before it could be used")
            }
            Error::RegistrationPending => write!(f, "the registration has not completed"),
            Error::PipeBusy => write!(f, "all pipe instances are busy"),
            Error::PipeBroken => write!(f, "the pipe's peer closed its end"),
            Error::PipeNoPeer => write!(f, "the pipe has no peer connected"),
            Error::PipeListening => write!(f, "the pipe is listening and not yet connected"),
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
            Error::ShuttingDown
            | Error::AbandonedAtShutdown
            | Error::ShutdownStalled { .. }
            | Error::MissingField { .. }
            | Error::TooManyOperations
            | Error::InvalidRegisteredIndex { .. }
            | Error::RegisteredRangeOutOfBounds { .. }
            | Error::BufferCheckedOut { .. }
            | Error::RegistrationSuperseded
            | Error::RegistrationPending
            | Error::PipeBusy
            | Error::PipeBroken
            | Error::PipeNoPeer
            | Error::PipeListening => None,
        }
    }
}
