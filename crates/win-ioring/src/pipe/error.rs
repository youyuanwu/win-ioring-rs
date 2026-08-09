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
    /// A driver-surface condition that this surface does not name.
    ///
    /// Registered I/O and shutdown produce conditions that no file or pipe
    /// operation can surface. They are carried here rather than flattened into
    /// [`Error::Other`] with an invented code, because inventing a code is the
    /// one thing the classification design forbids: a made-up code can be
    /// re-classified into a condition that never occurred.
    ///
    /// Boxed so this type stays small - it is returned from the measured read
    /// and write paths.
    Driver(Box<crate::runtime::error::Error>),
    /// A platform error this type does not name, carried verbatim.
    Other(windows::core::Error),
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
            Error::Ring(error) => error.os_error(),
            Error::Buf(error) => error.os_error(),
            Error::Driver(error) => error.os_error(),
            Error::Other(error) => Some(error),
            // The four named pipe conditions are classified from a code and
            // report `None`; the same code reaching a file error is demoted to
            // `Other` and reports `Some`.
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

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Busy => write!(f, "every pipe instance is already serving a client"),
            Error::Broken => write!(f, "the peer closed its end of the pipe"),
            Error::NoPeer => write!(f, "the pipe has no peer connected"),
            Error::Listening => write!(f, "the pipe instance is still waiting for a client"),
            Error::AcceptOutstanding => {
                write!(f, "an accept is already outstanding on this server")
            }
            Error::Ring(error) => write!(f, "{error}"),
            Error::Buf(error) => write!(f, "{error}"),
            Error::ShuttingDown => write!(f, "the runtime is shutting down"),
            Error::MissingField { field } => {
                write!(f, "required field `{field}` was not set")
            }
            Error::AbandonedAtShutdown => {
                write!(f, "the operation was abandoned when the runtime shut down")
            }
            Error::TooManyOperations => write!(
                f,
                "the driver is tracking as many operations as it can; complete \
                 outstanding operations rather than submitting more"
            ),
            Error::Driver(error) => write!(f, "{error}"),
            Error::Other(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Ring(error) => Some(error),
            Error::Buf(error) => Some(error),
            Error::Driver(error) => Some(error.as_ref()),
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
        Error::from(value.code())
    }
}

impl From<windows::core::HRESULT> for Error {
    fn from(value: windows::core::HRESULT) -> Self {
        // One of the crate's two classification tables, and the only type that
        // names a pipe condition. Anything the pipe table does not name delegates
        // to the ring surface exactly as `file::Error` does.
        match crate::error::classify_pipe(value) {
            Some(crate::error::PipeCondition::Busy) => Error::Busy,
            Some(crate::error::PipeCondition::Broken) => Error::Broken,
            Some(crate::error::PipeCondition::NoPeer) => Error::NoPeer,
            Some(crate::error::PipeCondition::Listening) => Error::Listening,
            None => match crate::io_ring::error::Error::from(value) {
                crate::io_ring::error::Error::Other(e) => Error::Other(e),
                named @ (crate::io_ring::error::Error::QueueFull
                | crate::io_ring::error::Error::UnsupportedOp { .. }
                | crate::io_ring::error::Error::RingClosed) => Error::Ring(named),
            },
        }
    }
}

impl From<crate::runtime::error::Error> for Error {
    // Two doors, both shut by the compiler rather than by review.
    //
    // A *new* `runtime::Error` variant is E0004 here, because every variant is
    // named and none is caught by a wildcard. Writing that wildcard is the
    // obvious way to make E0004 go away, and it would silently route a
    // condition this surface *can* produce into the driver-only sink -- so the
    // lint below makes the wildcard itself an error. A comment forbidding it
    // was the previous guard; a comment is not a guard.
    #[deny(clippy::wildcard_enum_match_arm)]
    /// Narrows a driver error onto the pipe surface.
    ///
    /// This is the direction the design exists for, and it is the **only** place
    /// a pipe condition is named. The driver demotes all four to
    /// [`R::Other`] carrying the code, because nothing it is reached through can
    /// know a handle is a pipe. This type does know, so the `R::Other` arm
    /// re-classifies the code and names it.
    ///
    /// Recovery is therefore not a special case bolted beside the conversion --
    /// it *is* the conversion. There is no `R::PipeBroken` arm to find, because
    /// there is no such variant.
    ///
    /// [`R::Other`]: crate::runtime::Error::Other
    fn from(value: crate::runtime::error::Error) -> Self {
        use crate::runtime::error::Error as R;

        match value {
            R::Ring(e) => Error::Ring(e),
            R::Buf(e) => Error::Buf(e),
            R::ShuttingDown => Error::ShuttingDown,
            R::AbandonedAtShutdown => Error::AbandonedAtShutdown,
            R::MissingField { field } => Error::MissingField { field },
            R::TooManyOperations => Error::TooManyOperations,
            // Every pipe condition arrives here, not in an arm of its own.
            R::Other(e) => Error::from(e.code()),
            // Driver-only, and unreachable from a pipe completion. Named
            // individually rather than caught by a wildcard: a wildcard would
            // silently box a *new* condition this surface *can* produce, which
            // is the one failure this design exists to prevent. Adding a
            // `runtime::Error` variant must fail to compile here.
            other @ (R::ShutdownStalled { .. }
            | R::InvalidRegisteredIndex { .. }
            | R::RegisteredRangeOutOfBounds { .. }
            | R::BufferCheckedOut { .. }
            | R::RegistrationSuperseded
            | R::RegistrationPending) => Error::Driver(Box::new(other)),
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
    fn display_is_non_empty_and_distinct_for_every_pipe_error_variant() {
        let variants: Vec<Error> = vec![
            Error::Busy,
            Error::Broken,
            Error::NoPeer,
            Error::Listening,
            Error::AcceptOutstanding,
            Error::Ring(crate::io_ring::error::Error::QueueFull),
            Error::Buf(crate::buf::error::Error::TooSmall {
                requested: 10,
                available: 4,
            }),
            Error::ShuttingDown,
            Error::MissingField { field: "handle" },
            Error::AbandonedAtShutdown,
            Error::TooManyOperations,
            Error::Driver(Box::new(crate::runtime::error::Error::RegistrationPending)),
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
    fn _every_pipe_error_variant_is_listed_above(e: &Error) {
        match e {
            Error::Busy
            | Error::Broken
            | Error::NoPeer
            | Error::Listening
            | Error::AcceptOutstanding
            | Error::Ring(_)
            | Error::Buf(_)
            | Error::ShuttingDown
            | Error::MissingField { .. }
            | Error::AbandonedAtShutdown
            | Error::TooManyOperations
            | Error::Driver(_)
            | Error::Other(_) => {}
        }
    }
}
