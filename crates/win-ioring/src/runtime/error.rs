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
            Error::ShuttingDown => write!(f, "the driver is shutting down"),
            Error::AbandonedAtShutdown => {
                write!(
                    f,
                    "the operation was abandoned at shutdown before the platform ran it"
                )
            }
            Error::ShutdownStalled { outstanding } => write!(
                f,
                "shutdown is still draining, with {outstanding} operation(s) outstanding"
            ),
            Error::MissingField { field } => {
                write!(f, "required field `{field}` was not set")
            }
            Error::TooManyOperations => write!(
                f,
                "the driver is tracking as many operations as it can; complete \
                 outstanding operations rather than submitting more"
            ),
            Error::InvalidRegisteredIndex { index } => {
                write!(
                    f,
                    "registered index {index} does not refer to a registration"
                )
            }
            Error::RegisteredRangeOutOfBounds {
                index,
                offset,
                length,
                extent,
            } => write!(
                f,
                "registered buffer {index} range {offset}..{} exceeds its extent of {extent}",
                offset.saturating_add(*length)
            ),
            Error::BufferCheckedOut { index } => {
                write!(f, "registered buffer {index} is already checked out")
            }
            Error::RegistrationSuperseded => {
                write!(
                    f,
                    "the registration this collection came from has been superseded"
                )
            }
            Error::RegistrationPending => write!(
                f,
                "a registration request is in flight, so no buffer may be checked out"
            ),
            Error::PipeBusy => write!(f, "every pipe instance is already serving a client"),
            Error::PipeBroken => write!(f, "the peer closed its end of the pipe"),
            Error::PipeNoPeer => write!(f, "the pipe has no peer connected"),
            Error::PipeListening => write!(f, "the pipe instance is still waiting for a client"),
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

impl From<crate::io_ring::error::Error> for Error {
    fn from(value: crate::io_ring::error::Error) -> Self {
        Error::Ring(value)
    }
}

impl From<crate::buf::error::Error> for Error {
    fn from(value: crate::buf::error::Error) -> Self {
        Error::Buf(value)
    }
}

impl From<crate::io_ring::ops::MissingField> for Error {
    /// Widens a builder's missing-field report into this surface's error.
    ///
    /// The operation builders can fail in exactly one way, so they say so with
    /// their own single-condition type; this surface reports the same condition
    /// alongside everything else it can produce.
    fn from(value: crate::io_ring::ops::MissingField) -> Self {
        // Destructured rather than read field-by-field so that *widening* the
        // source is E0027 here. `value.field` would keep compiling if
        // `MissingField` grew a second field, and would silently carry half of
        // it -- a payload loss that typechecks, returns the right variant, and
        // passes any test that only asks which variant came out.
        let crate::io_ring::ops::MissingField { field } = value;
        Error::MissingField { field }
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
    fn display_is_non_empty_and_distinct_for_every_runtime_error_variant() {
        let variants: Vec<Error> = vec![
            Error::Ring(crate::io_ring::error::Error::QueueFull),
            Error::Buf(crate::buf::error::Error::TooSmall {
                requested: 10,
                available: 4,
            }),
            Error::ShuttingDown,
            Error::AbandonedAtShutdown,
            Error::ShutdownStalled { outstanding: 3 },
            Error::MissingField { field: "handle" },
            Error::TooManyOperations,
            Error::InvalidRegisteredIndex { index: 3 },
            Error::RegisteredRangeOutOfBounds {
                index: 0,
                offset: 8,
                length: 16,
                extent: 16,
            },
            Error::BufferCheckedOut { index: 2 },
            Error::RegistrationSuperseded,
            Error::RegistrationPending,
            Error::PipeBusy,
            Error::PipeBroken,
            Error::PipeNoPeer,
            Error::PipeListening,
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
    fn _every_runtime_error_variant_is_listed_above(e: &Error) {
        match e {
            Error::Ring(_)
            | Error::Buf(_)
            | Error::ShuttingDown
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
            | Error::PipeListening
            | Error::Other(_) => {}
        }
    }
}
