//! The error produced by the file surface.
//!
//! Nine conditions. What is *absent* is as deliberate as what is present: none
//! of the four pipe conditions are named here, because a regular file cannot
//! produce them. If one ever arrives — a caller having adopted a pipe handle as
//! a [`File`](crate::file::File) — it reaches [`Error::Other`] with its
//! `HRESULT` intact, and re-classifying that code at a surface that *does* name
//! it recovers the condition. Nothing is fabricated and nothing is lost.

use std::fmt;

use crate::error::ConditionView;

/// An error produced by an operation on a [`File`](crate::file::File).
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
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
    /// A sequential operation is already in flight on this file.
    ///
    /// Sequential reads and writes share a cursor, so a second one cannot start
    /// while the first is outstanding. Positional operations are unaffected.
    OperationOutstanding,
    /// The driver is already tracking as many operations as it can.
    ///
    /// Complete outstanding operations; submitting more cannot help.
    TooManyOperations,
    /// This handle has no file offset, so a positional operation is meaningless.
    ///
    /// Pipes and character devices are the cases that reach this. Renamed from
    /// the earlier `NoFileOffset`, which described the cause rather than the
    /// condition the caller has to act on.
    NotSeekable {
        /// The Win32 file type of the handle.
        file_type: u32,
    },
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

impl ConditionView for Error {
    fn queue_full(_hr: windows::core::HRESULT) -> Self {
        Error::Ring(crate::io_ring::error::Error::QueueFull)
    }

    // The four pipe conditions are not named by this type. They demote to
    // `Other` carrying the original code, which is what lets a surface that
    // does name them recover the condition by re-classifying. Fabricating a
    // stand-in code here would break that, which is why every method receives
    // the real `HRESULT`.
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

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Ring(error) => write!(f, "{error}"),
            Error::Buf(error) => write!(f, "{error}"),
            Error::ShuttingDown => write!(f, "the driver is shutting down"),
            Error::MissingField { field } => {
                write!(f, "required field `{field}` was not set")
            }
            Error::AbandonedAtShutdown => {
                write!(
                    f,
                    "the operation was abandoned at shutdown before the platform ran it"
                )
            }
            Error::OperationOutstanding => write!(
                f,
                "a sequential operation is already outstanding on this file"
            ),
            Error::TooManyOperations => write!(
                f,
                "the driver is tracking as many operations as it can; complete \
                 outstanding operations rather than submitting more"
            ),
            Error::NotSeekable { file_type } => write!(
                f,
                "this handle has no file offset (Win32 file type {file_type}), so \
                 a positional operation is meaningless"
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
            Error::ShuttingDown
            | Error::MissingField { .. }
            | Error::AbandonedAtShutdown
            | Error::OperationOutstanding
            | Error::TooManyOperations
            | Error::NotSeekable { .. } => None,
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
        Error::MissingField { field: value.field }
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

impl From<crate::runtime::error::Error> for Error {
    /// Narrows a driver error onto the file surface.
    ///
    /// The four pipe conditions have no name here, because a regular file
    /// cannot produce them. They are not dropped: each is re-viewed from its
    /// canonical code, so it arrives as [`Error::Other`] carrying that code, and
    /// a pipe surface handed the same error recovers the condition.
    fn from(value: crate::runtime::error::Error) -> Self {
        use crate::error::canonical;
        use crate::runtime::error::Error as R;

        match value {
            R::Ring(e) => Error::Ring(e),
            R::Buf(e) => Error::Buf(e),
            R::ShuttingDown => Error::ShuttingDown,
            R::AbandonedAtShutdown => Error::AbandonedAtShutdown,
            R::MissingField { field } => Error::MissingField { field },
            R::TooManyOperations => Error::TooManyOperations,
            R::Other(e) => crate::error::view::<Error>(e.code()),
            // Named on the driver, unnamed here: demote through the same table,
            // with the code attached so it can be recovered elsewhere.
            R::PipeBusy => crate::error::view::<Error>(canonical::pipe_busy()),
            R::PipeBroken => crate::error::view::<Error>(canonical::pipe_broken()),
            R::PipeNoPeer => crate::error::view::<Error>(canonical::pipe_no_peer()),
            R::PipeListening => crate::error::view::<Error>(canonical::pipe_listening()),
            // Driver-only, and unreachable from a file completion. Named
            // individually rather than caught by a wildcard: a wildcard would
            // silently box a *new* condition this surface *can* produce, which
            // is the one failure this design exists to prevent. Adding a
            // `runtime::Error` variant must fail to compile here.
            // Driver-only, and unreachable from a file completion. Named
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
    fn display_is_non_empty_and_distinct_for_every_file_error_variant() {
        let variants: Vec<Error> = vec![
            Error::Ring(crate::io_ring::error::Error::QueueFull),
            Error::Buf(crate::buf::error::Error::TooSmall {
                requested: 10,
                available: 4,
            }),
            Error::ShuttingDown,
            Error::MissingField { field: "handle" },
            Error::AbandonedAtShutdown,
            Error::OperationOutstanding,
            Error::TooManyOperations,
            Error::NotSeekable { file_type: 3 },
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
    fn _every_file_error_variant_is_listed_above(e: &Error) {
        match e {
            Error::Ring(_)
            | Error::Buf(_)
            | Error::ShuttingDown
            | Error::MissingField { .. }
            | Error::AbandonedAtShutdown
            | Error::OperationOutstanding
            | Error::TooManyOperations
            | Error::NotSeekable { .. }
            | Error::Driver(_)
            | Error::Other(_) => {}
        }
    }
}
