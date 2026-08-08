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
            Error::ShuttingDown => write!(f, "the runtime is shutting down"),
            Error::MissingField { field } => {
                write!(f, "the operation is missing a required field: {field}")
            }
            Error::AbandonedAtShutdown => {
                write!(f, "the operation was abandoned when the runtime shut down")
            }
            Error::OperationOutstanding => write!(
                f,
                "a sequential operation is already in flight on this file"
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
            | Error::MissingField { .. }
            | Error::AbandonedAtShutdown
            | Error::OperationOutstanding
            | Error::TooManyOperations
            | Error::NotSeekable { .. } => None,
        }
    }
}
