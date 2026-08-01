//! Error types for the crate.
//!
//! Every fallible operation in this crate reports failures through [`Error`], a
//! closed enum whose variants callers can match on directly rather than
//! inspecting `HRESULT` values. The underlying platform error is preserved in
//! [`Error::Os`] whenever one is available.
//!
//! # Platform availability
//!
//! This crate binds the Windows IoRing API through statically imported symbols
//! from `api-ms-win-core-ioring-l1-1-0.dll`. A host that lacks that API set
//! entirely cannot report an error here, because the process fails to load
//! before any code in this crate runs. [`Error::Unsupported`] and
//! [`Error::UnsupportedFeature`] therefore describe hosts where the API set
//! loads but does not provide what this crate needs.

use std::fmt;

use windows::Win32::Storage::FileSystem::IORING_VERSION;

/// The result type used throughout this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// An error produced by this crate.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Error {
    /// IoRing is present on this host but is not usable by this crate.
    ///
    /// See the [module documentation](self) for why a host with no IoRing
    /// support at all cannot produce this error.
    Unsupported,

    /// The requested ring version is not supported by this host.
    UnsupportedVersion {
        /// The version that was requested.
        requested: i32,
        /// The highest version the host reports supporting.
        max_supported: i32,
    },

    /// The host does not report a ring feature this crate requires.
    UnsupportedFeature {
        /// The feature flag bits that were required but not reported.
        required: i32,
        /// The feature flag bits the host actually reports.
        available: i32,
    },

    /// The host does not support the requested operation.
    UnsupportedOp {
        /// The operation code that is unsupported.
        op: i32,
    },

    /// The submission queue has no room for another entry.
    QueueFull,

    /// A registered buffer or handle index does not refer to a live
    /// registration.
    InvalidRegisteredIndex {
        /// The index that was supplied.
        index: u32,
    },

    /// The supplied buffer is too small for the requested transfer.
    BufferTooSmall {
        /// The number of bytes the operation requested.
        requested: u64,
        /// The number of bytes the buffer can accommodate.
        available: u64,
    },

    /// A write would have sourced bytes the caller has not initialized.
    ///
    /// Sending uninitialized memory to the kernel is never permitted, so the
    /// operation is rejected before submission.
    UninitializedWriteRange {
        /// The number of bytes the write requested.
        requested: u64,
        /// The number of initialized bytes available to read from.
        initialized: u64,
    },

    /// A registered buffer reference names a range outside its registration.
    RegisteredRangeOutOfBounds {
        /// The registered buffer index.
        index: u32,
        /// The offset within the registered buffer.
        offset: u64,
        /// The length requested from that offset.
        length: u64,
        /// The extent available for this operation.
        extent: u64,
    },

    /// The operation could not complete and its buffer could not be recovered.
    ///
    /// This is the one case in which a caller does not get its buffer back. It
    /// arises when the driver is torn down while a submission queue entry still
    /// references the buffer, so releasing it would risk the kernel writing into
    /// freed memory. The buffer is retained (leaked) instead.
    BufferRetained,

    /// The driver this operation belonged to no longer exists.
    DriverGone,

    /// The driver is shutting down and is not accepting new operations.
    ShuttingDown,

    /// A sequential operation is already outstanding on this file.
    ///
    /// Sequential file operations are serialized because they share a cursor. A
    /// previous sequential operation has not yet reached terminal completion,
    /// which can happen when its future was dropped while the operation was
    /// still in flight.
    OperationOutstanding,

    /// A required field was not supplied when building an operation.
    MissingField {
        /// The name of the field that was not set.
        field: &'static str,
    },

    /// An error reported by the operating system.
    Os(windows::core::Error),
}

impl Error {
    /// Returns the underlying platform error, if this error wraps one.
    pub fn as_os_error(&self) -> Option<&windows::core::Error> {
        match self {
            Error::Os(e) => Some(e),
            _ => None,
        }
    }

    /// Classifies a platform `HRESULT` into a crate error.
    ///
    /// The IoRing API set defines its own `HRESULT` facility, so most failure
    /// modes are distinguishable without guessing. Codes with no specific
    /// meaning to this crate are preserved verbatim in [`Error::Os`].
    pub(crate) fn from_hresult(hr: windows::core::HRESULT) -> Self {
        use windows::Win32::Foundation::{
            IORING_E_REQUIRED_FLAG_NOT_SUPPORTED, IORING_E_SUBMISSION_QUEUE_FULL,
            IORING_E_VERSION_NOT_SUPPORTED,
        };
        match hr {
            h if h == IORING_E_SUBMISSION_QUEUE_FULL => Error::QueueFull,
            h if h == IORING_E_VERSION_NOT_SUPPORTED => Error::UnsupportedVersion {
                requested: 0,
                max_supported: 0,
            },
            h if h == IORING_E_REQUIRED_FLAG_NOT_SUPPORTED => Error::UnsupportedFeature {
                required: 0,
                available: 0,
            },
            other => Error::Os(windows::core::Error::from(other)),
        }
    }

    /// Classifies a platform error produced while creating a ring.
    ///
    /// `E_NOTIMPL` is the signal that a host which can load this crate's
    /// imports nonetheless cannot provide a ring. Argument errors such as
    /// `E_INVALIDARG` are deliberately *not* folded into
    /// [`Error::Unsupported`], because they usually indicate a bad queue size
    /// rather than a platform limitation.
    pub(crate) fn from_create_failure(err: windows::core::Error) -> Self {
        use windows::Win32::Foundation::E_NOTIMPL;
        if err.code() == E_NOTIMPL {
            Error::Unsupported
        } else {
            Error::from_hresult(err.code())
        }
    }

    /// Builds an [`Error::UnsupportedVersion`] from platform version values.
    pub(crate) fn unsupported_version(requested: IORING_VERSION, max: IORING_VERSION) -> Self {
        Error::UnsupportedVersion {
            requested: requested.0,
            max_supported: max.0,
        }
    }
}

impl From<windows::core::HRESULT> for Error {
    fn from(value: windows::core::HRESULT) -> Self {
        Error::from_hresult(value)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unsupported => {
                write!(f, "IoRing is not usable on this host")
            }
            Error::UnsupportedVersion {
                requested,
                max_supported,
            } => write!(
                f,
                "IoRing version {requested} is not supported; this host supports up to {max_supported}"
            ),
            Error::UnsupportedFeature {
                required,
                available,
            } => write!(
                f,
                "IoRing feature flags {required:#x} are required but this host reports {available:#x}"
            ),
            Error::UnsupportedOp { op } => {
                write!(f, "IoRing operation {op} is not supported on this host")
            }
            Error::QueueFull => write!(f, "the submission queue is full"),
            Error::InvalidRegisteredIndex { index } => {
                write!(
                    f,
                    "registered index {index} does not refer to a registration"
                )
            }
            Error::BufferTooSmall {
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
            Error::BufferRetained => write!(
                f,
                "the operation could not complete and its buffer was retained because the kernel may still access it"
            ),
            Error::DriverGone => write!(f, "the driver no longer exists"),
            Error::ShuttingDown => write!(f, "the driver is shutting down"),
            Error::OperationOutstanding => {
                write!(
                    f,
                    "a sequential operation is already outstanding on this file"
                )
            }
            Error::MissingField { field } => {
                write!(f, "required field `{field}` was not set")
            }
            Error::Os(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Os(e) => Some(e),
            _ => None,
        }
    }
}

impl From<windows::core::Error> for Error {
    fn from(value: windows::core::Error) -> Self {
        Error::from_hresult(value.code())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_errors_round_trip() {
        let os = windows::core::Error::from(windows::Win32::Foundation::E_FAIL);
        let err = Error::from(os.clone());
        assert!(matches!(err, Error::Os(_)));
        assert_eq!(err.as_os_error().map(|e| e.code()), Some(os.code()));
    }

    #[test]
    fn non_os_errors_have_no_source() {
        use std::error::Error as _;
        assert!(Error::QueueFull.source().is_none());
        assert!(Error::QueueFull.as_os_error().is_none());
    }

    /// Callers must be able to distinguish these conditions by pattern, never
    /// by comparing rendered strings.
    #[test]
    fn unsupported_conditions_are_distinct_variants() {
        let host = Error::Unsupported;
        let version = Error::unsupported_version(IORING_VERSION(9999), IORING_VERSION(400));
        let op = Error::UnsupportedOp { op: 5 };
        let feature = Error::UnsupportedFeature {
            required: 2,
            available: 0,
        };

        assert!(matches!(host, Error::Unsupported));
        assert!(matches!(
            version,
            Error::UnsupportedVersion {
                requested: 9999,
                max_supported: 400
            }
        ));
        assert!(matches!(op, Error::UnsupportedOp { op: 5 }));
        assert!(matches!(
            feature,
            Error::UnsupportedFeature {
                required: 2,
                available: 0
            }
        ));
    }

    #[test]
    fn display_is_non_empty_for_every_variant() {
        let variants = [
            Error::Unsupported,
            Error::unsupported_version(IORING_VERSION(1), IORING_VERSION(400)),
            Error::UnsupportedFeature {
                required: 2,
                available: 0,
            },
            Error::UnsupportedOp { op: 6 },
            Error::QueueFull,
            Error::InvalidRegisteredIndex { index: 3 },
            Error::BufferTooSmall {
                requested: 10,
                available: 4,
            },
            Error::UninitializedWriteRange {
                requested: 10,
                initialized: 4,
            },
            Error::RegisteredRangeOutOfBounds {
                index: 0,
                offset: 8,
                length: 16,
                extent: 16,
            },
            Error::BufferRetained,
            Error::DriverGone,
            Error::ShuttingDown,
            Error::OperationOutstanding,
            Error::MissingField { field: "handle" },
            Error::Os(windows::core::Error::from(
                windows::Win32::Foundation::E_FAIL,
            )),
        ];
        for v in variants {
            assert!(!v.to_string().is_empty(), "empty Display for {v:?}");
        }
    }
}
