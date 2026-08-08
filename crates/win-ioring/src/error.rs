//! Error types for the crate.
//!
//! Almost every fallible operation in this crate reports failures through
//! [`Error`], a closed enum whose variants callers can match on directly rather
//! than inspecting `HRESULT` values. The underlying platform error is preserved
//! in [`Error::Os`] whenever one is available.
//!
//! The exception is the operation builders in [`crate::io_ring::ops`], whose
//! `build` methods return [`MissingField`](crate::io_ring::ops::MissingField)
//! because their failure set is closed without consulting the platform. That is
//! the only surface in the crate narrow enough to carve;
//! `docs/errors-and-the-funnel.md` records why the rest do not partition by API.
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

/// A platform error condition, named once so that every error type agrees.
///
/// This is the crate's **single classification table**. [`classify`] is the only
/// function in the crate that maps an `HRESULT` to a condition, and every public
/// error type is a *view* over this enum rather than a second table.
///
/// # Why one table
///
/// `pipe::Client`'s open path and the ring completion funnel both observe
/// `ERROR_PIPE_BUSY`, and both must produce the same condition. Two independent
/// match arms are exactly how that stops being true after someone edits one of
/// them, which is the hazard `pipe/client.rs` warns about in the crate's own
/// voice. A view cannot diverge from the table it reads, so the hazard is
/// removed structurally rather than discouraged in a comment.
///
/// Each view is hand-written and carries `#[deny(clippy::wildcard_enum_match_arm,
/// clippy::match_wildcard_for_single_variants)]`, so adding a variant here fails
/// to compile every view that has not been updated. Both lints are required and
/// the views are not generated; `view_convention` records why, and neither fact
/// is incidental.
///
/// # Scope
///
/// This enum covers *platform-derived* conditions only — those a caller learns
/// about by receiving an `HRESULT`. Conditions the crate detects itself, such as
/// a shutting-down runtime or a builder field that was never set, are produced
/// directly at the call site that knows about them and never pass through here.
///
/// It is deliberately `pub(crate)`. It appears in no public signature and is not
/// re-exported, so it contributes **no** public variant slots to the crate's API
/// surface; see `condition_is_not_part_of_the_public_api` for the check that
/// keeps that true.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Condition {
    /// The ring's submission queue has no room for another entry.
    QueueFull,
    /// All instances of the named pipe are busy.
    PipeBusy,
    /// The pipe's peer closed its end.
    PipeBroken,
    /// The pipe has no peer connected.
    PipeNoPeer,
    /// The pipe is listening and has not yet been connected to.
    PipeListening,
    /// A platform error this crate does not name.
    ///
    /// The `HRESULT` is carried so that a view handed only a `Condition` can
    /// still construct a platform error without a second condition-to-code
    /// table of its own.
    ///
    /// It is **not** what lets a pipe's error survive a trip through a type that
    /// has no name for it. `classify` is deterministic, so `Other(hr)`
    /// reclassifies to `Other(hr)` forever and no named condition can be
    /// recovered from this payload. That recovery runs through the *error
    /// type's* `Other`, which carries the `HRESULT` the classifier was given —
    /// see `view_convention`.
    Other(windows::core::HRESULT),
}

/// Maps a platform `HRESULT` to the condition it denotes.
///
/// This is the only function in the crate that matches on an `HRESULT` to
/// produce an error condition. `error_classification_has_one_home` enforces
/// that by inspecting the crate's own source text, and carries the allowlist of
/// the sites which compare codes for other reasons.
///
/// Codes are matched **exactly**, never by facility or range. This classifier
/// sees every ring completion in the crate, so a range match would reclassify
/// errors from files and sockets that happen to fall inside it — a much larger
/// blast radius than the pipe surface that motivated the pipe codes.
///
/// `ERROR_PIPE_CONNECTED` is deliberately absent. It reports that a client
/// arrived before the accept was issued, which is a **success** for the accept
/// and is converted at that call site; classifying it here would turn the most
/// easily lost connection in the API into an error at the one place with no
/// context to recognise it.
pub(crate) fn classify(hr: windows::core::HRESULT) -> Condition {
    use windows::Win32::Foundation::{
        ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_LISTENING,
        IORING_E_SUBMISSION_QUEUE_FULL,
    };
    if hr == IORING_E_SUBMISSION_QUEUE_FULL {
        Condition::QueueFull
    } else if hr == ERROR_PIPE_BUSY.to_hresult() {
        Condition::PipeBusy
    } else if hr == ERROR_BROKEN_PIPE.to_hresult() {
        Condition::PipeBroken
    } else if hr == ERROR_NO_DATA.to_hresult() {
        Condition::PipeNoPeer
    } else if hr == ERROR_PIPE_LISTENING.to_hresult() {
        Condition::PipeListening
    } else {
        Condition::Other(hr)
    }
}

/// How views over [`Condition`] are written, and why they are not generated.
///
/// Views are **hand-written**, and this is a deliberate reversal of the obvious
/// design. Two mechanisms that look sufficient are not, and both were found by
/// mutating a view and watching for a complaint that never came.
///
/// **A macro cannot carry the lint.** A macro that emitted each view would
/// guarantee the attribute was present, and this crate had one.
/// `clippy::wildcard_enum_match_arm` does not fire inside a macro expansion —
/// Clippy skips lints whose span comes from expansion — so the attribute was
/// present in the source and inert in effect. A generated view could absorb
/// every condition behind a `_` arm and Clippy would report nothing. The
/// identical match, written by hand, is rejected.
///
/// **One lint is not enough.** `clippy::wildcard_enum_match_arm` does not fire
/// when the wildcard covers exactly *one* remaining variant; that case belongs
/// to `clippy::match_wildcard_for_single_variants`, a different lint in a
/// different group. The single-variant case is the likely one here: a view that
/// names every condition but the one its author forgot is exactly the mistake
/// these types exist to prevent, and it is the case the obvious lint misses.
/// Both are denied.
///
/// Each view is therefore written out, carries both attributes itself, and is
/// checked by `error_classification_policy.rs`:
///
/// - every view must arm both lints;
/// - no view may use a wildcard arm at all.
///
/// Between them, `rustc`'s own exhaustiveness rule does the rest: with no
/// wildcard, adding a [`Condition`] variant fails to compile every view that has
/// not named it.
mod view_convention {}

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

    /// A registered buffer is already checked out.
    ///
    /// Only one handle to a given registered buffer may exist at a time, so
    /// that an operation in flight and the application can never reach the same
    /// bytes at once. A buffer held by an operation returns when that operation
    /// reports, which may be later than the point its future was dropped.
    BufferCheckedOut {
        /// The index that is already checked out.
        index: u32,
    },

    /// The registration this collection came from has been superseded.
    ///
    /// Its buffers remain valid for as long as any handle holds them, but the
    /// collection no longer yields new ones: the indices it names belong to a
    /// registration the platform is no longer resolving against.
    RegistrationSuperseded,

    /// A registration request is in flight, so no handle may be checked out.
    ///
    /// A registration is adopted when it completes rather than when it is
    /// requested. A handle taken in between would name a set about to be
    /// superseded, so checkout is refused for the duration.
    RegistrationPending,

    /// The driver is shutting down and is not accepting new operations.
    ShuttingDown,

    /// The operation was ended by shutdown before the platform ever ran it.
    ///
    /// Distinct from a natural I/O failure and from a cancellation: nothing was
    /// attempted. It arises when teardown resolves an operation itself, rather
    /// than waiting for a completion that is never coming — because the queue
    /// entry was never accepted by the platform despite repeated attempts, and
    /// closing the ring discards it. The caller's buffer is returned.
    AbandonedAtShutdown,

    /// Shutdown is taking an unusual amount of time to drain.
    ///
    /// Reported, throttled, while operations remain outstanding. Draining is
    /// unbounded by design — it never abandons memory — so a shutdown blocked on
    /// an operation that neither completes nor responds to cancellation would
    /// otherwise be indistinguishable from a hang. Informational: the drain is
    /// still making attempts.
    ShutdownStalled {
        /// How many operations are still outstanding.
        outstanding: usize,
    },

    /// A sequential operation is already outstanding on this file.
    ///
    /// Sequential file operations are serialized because they share a cursor. A
    /// previous sequential operation has not yet reached terminal completion,
    /// which can happen when its future was dropped while the operation was
    /// still in flight.
    OperationOutstanding,

    /// The sequential API was used on a handle that has no file offset.
    ///
    /// [`File::read`](crate::file::File::read) and
    /// [`File::write`](crate::file::File::write) supply the file offset
    /// themselves, from a cursor they advance. That contract is meaningless on a
    /// pipe or a character device: the platform ignores the offset and consumes
    /// from the head of the stream, so every operation after the first *would*
    /// return success paired with bytes that did not come from where the cursor
    /// says. This error is what prevents that, and it is the reason the crate
    /// refuses rather than describing the hazard in a doc comment.
    ///
    /// The positional API is unaffected:
    /// [`Handle::read`](crate::runtime::Handle::read) and
    /// [`Handle::write`](crate::runtime::Handle::write) take an explicit offset
    /// and continue to work on these handles. The platform ignores that offset
    /// too, but the caller supplied it knowingly, and the crate's own pipe types
    /// depend on that path to move bytes.
    ///
    /// The refusal is *fail-open*: a handle whose type the platform reports as
    /// unknown is permitted, on the grounds that a kind nobody anticipated should
    /// be left exactly where it is today rather than newly broken.
    NoFileOffset {
        /// The platform's handle-type code, as reported by `GetFileType`.
        file_type: u32,
    },

    /// The ring has been closed and can no longer be used.
    ///
    /// The platform does not reliably reject a closed ring handle — passing one
    /// to `PopIoRingCompletion` faults rather than returning an error — so this
    /// crate refuses the call itself.
    RingClosed,

    /// A required field was not supplied when building an operation.
    MissingField {
        /// The name of the field that was not set.
        field: &'static str,
    },

    /// An error reported by the operating system.
    Os(windows::core::Error),

    /// No pipe instance was available to connect to.
    ///
    /// Every instance the server created is already serving a client. The
    /// condition is transient by nature: a client that retries after a peer
    /// disconnects may succeed against the same server.
    PipeBusy,

    /// The peer closed its end of the pipe.
    ///
    /// Distinct from a zero-byte read. A pipe reports the peer's departure as
    /// an error rather than as end-of-file, so treating this as "no more data"
    /// would silently conflate a completed exchange with a truncated one.
    PipeBroken,

    /// The pipe is connected in the caller's own view but has no peer.
    ///
    /// Reported when a server writes to an instance the client has disconnected
    /// from, or reads from one that was never connected. Separate from
    /// [`Error::PipeBroken`] because the platform separates them, and folding
    /// them together would lose the distinction between "the peer left" and
    /// "there has not been one".
    PipeNoPeer,

    /// The pipe instance is still waiting for a client.
    ///
    /// A read or a write issued against a listening instance fails with this
    /// rather than blocking. It means an accept has not completed, not that the
    /// pipe is broken, so the remedy is to accept first rather than to reopen.
    PipeListening,

    /// An accept is already outstanding on this server.
    ///
    /// One instance can host one pending accept. This is refused rather than
    /// queued because the alternative — two futures both waiting on the same
    /// `OVERLAPPED` — has no sound completion story.
    AcceptOutstanding,
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
    /// This is [`classify`] followed by this type's view over the result, kept
    /// as one call because it is what most call sites want. The classification
    /// rules — exact codes, no ranges, and why `ERROR_PIPE_CONNECTED` is absent
    /// — live on [`classify`], which is their single home.
    ///
    /// Only codes whose meaning is fully determined by the code itself are
    /// reclassified. Version and feature failures are deliberately *not*
    /// mapped here: their variants carry context — what was requested versus
    /// what the host offers — that a bare `HRESULT` cannot supply, and
    /// fabricating zeroed context would be worse than reporting the platform
    /// error verbatim. Those variants are produced at the call sites that know
    /// the context, such as [`IoRingBuilder::build`](crate::io_ring::IoRingBuilder::build).
    pub(crate) fn from_hresult(hr: windows::core::HRESULT) -> Self {
        Self::from_condition(classify(hr))
    }

    /// The crate-wide error's view over [`Condition`].
    ///
    /// This is the first of the exhaustive views described on [`Condition`]. It
    /// names every condition the table can report, so its `Other` arm is
    /// reached only by codes the table itself did not classify.
    ///
    /// Both attributes are written here rather than generated, and there are two
    /// of them for a reason; see [`view_convention`](crate::error::view_convention).
    #[deny(
        clippy::wildcard_enum_match_arm,
        clippy::match_wildcard_for_single_variants
    )]
    pub(crate) fn from_condition(condition: Condition) -> Self {
        match condition {
            Condition::QueueFull => Error::QueueFull,
            Condition::PipeBusy => Error::PipeBusy,
            Condition::PipeBroken => Error::PipeBroken,
            Condition::PipeNoPeer => Error::PipeNoPeer,
            Condition::PipeListening => Error::PipeListening,
            Condition::Other(hr) => Error::Os(windows::core::Error::from(hr)),
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
            Error::BufferCheckedOut { index } => {
                write!(f, "registered buffer {index} is already checked out")
            }
            Error::RegistrationSuperseded => write!(
                f,
                "the registration this collection came from has been superseded"
            ),
            Error::RegistrationPending => write!(
                f,
                "a registration request is in flight, so no buffer may be checked out"
            ),
            Error::ShuttingDown => write!(f, "the driver is shutting down"),
            Error::AbandonedAtShutdown => write!(
                f,
                "the operation was abandoned at shutdown before the platform ran it"
            ),
            Error::ShutdownStalled { outstanding } => write!(
                f,
                "shutdown is still draining, with {outstanding} operation(s) outstanding"
            ),
            Error::OperationOutstanding => {
                write!(
                    f,
                    "a sequential operation is already outstanding on this file"
                )
            }
            Error::NoFileOffset { file_type } => write!(
                f,
                "the sequential API needs a file offset, and this handle has \
                 none (GetFileType reported {file_type}); use the positional \
                 read/write instead"
            ),
            Error::RingClosed => write!(f, "the ring has been closed"),
            Error::MissingField { field } => {
                write!(f, "required field `{field}` was not set")
            }
            Error::Os(e) => write!(f, "{e}"),
            Error::PipeBusy => write!(f, "every pipe instance is already serving a client"),
            Error::PipeBroken => write!(f, "the peer closed its end of the pipe"),
            Error::PipeNoPeer => write!(f, "the pipe has no peer connected"),
            Error::PipeListening => {
                write!(f, "the pipe instance is still waiting for a client")
            }
            Error::AcceptOutstanding => {
                write!(f, "an accept is already outstanding on this server")
            }
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

impl From<crate::io_ring::ops::MissingField> for Error {
    /// Widens a builder's missing-field report into the crate-wide error.
    ///
    /// The same condition lives in both types on purpose. The operation
    /// builders can fail in exactly one way, so they say so
    /// ([`MissingField`](crate::io_ring::ops::MissingField)); the driver's
    /// registration entry points report a missing field too but sit on the data
    /// path, where every other variant is also reachable. This conversion keeps
    /// `?` working across that seam rather than forcing the narrow type to widen
    /// at each call site.
    fn from(value: crate::io_ring::ops::MissingField) -> Self {
        Error::MissingField { field: value.field }
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

    /// Each pipe condition FR-010 names must be reachable as its own variant.
    ///
    /// Callers distinguish these by pattern. Two conditions sharing a variant
    /// would be indistinguishable without parsing a rendered string, and the
    /// pairs below are the ones most likely to be conflated by a well-meaning
    /// simplification: busy is transient and worth retrying while listening is
    /// not, and broken means the peer left while no-peer means there has not
    /// been one.
    #[test]
    fn each_pipe_condition_maps_to_its_own_variant() {
        use windows::Win32::Foundation::{
            ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_LISTENING,
        };

        let cases = [
            (ERROR_PIPE_BUSY, Error::PipeBusy),
            (ERROR_BROKEN_PIPE, Error::PipeBroken),
            (ERROR_NO_DATA, Error::PipeNoPeer),
            (ERROR_PIPE_LISTENING, Error::PipeListening),
        ];

        let mut seen: Vec<String> = Vec::new();
        for (code, expected) in cases {
            let got = Error::from_hresult(code.to_hresult());
            assert_eq!(
                std::mem::discriminant(&got),
                std::mem::discriminant(&expected),
                "{code:?} classified as {got:?}, expected {expected:?}"
            );
            let rendered = got.to_string();
            assert!(
                !seen.contains(&rendered),
                "two pipe conditions render identically: {rendered:?}"
            );
            seen.push(rendered);
        }
    }

    /// The classifier matches exact codes, and must leave everything else to
    /// `Error::Os`.
    ///
    /// `from_hresult` is the single funnel every ring completion passes
    /// through, so widening it from exact codes to a facility or a range would
    /// silently reclassify file and socket errors that have nothing to do with
    /// pipes. The two codes below are not hypothetical: they are what the
    /// existing `Error::Os(_)` assertions in `runtime_tests.rs` actually
    /// observe — end-of-file on a read past the end, and the refusal of a
    /// write-through on cached I/O — measured rather than assumed, because
    /// "the new variants cannot collide with anything" is exactly the
    /// comfortable claim that deserves evidence.
    #[test]
    fn codes_outside_the_pipe_set_are_still_reported_verbatim() {
        use windows::Win32::Foundation::{ERROR_HANDLE_EOF, WIN32_ERROR};

        // 509 is what the cached-I/O write-through refusal reports.
        for code in [ERROR_HANDLE_EOF, WIN32_ERROR(509)] {
            let got = Error::from_hresult(code.to_hresult());
            assert!(
                matches!(got, Error::Os(_)),
                "{code:?} must stay an Os error, got {got:?}"
            );
        }

        // Adjacent to the pipe codes on both sides, to catch a range match that
        // happened to bracket them.
        for code in [230_u32, 233, 534, 537] {
            let got = Error::from_hresult(WIN32_ERROR(code).to_hresult());
            assert!(
                matches!(got, Error::Os(_)),
                "code {code} is not a pipe condition this crate maps, got {got:?}"
            );
        }
    }

    /// `ERROR_PIPE_CONNECTED` is a success for an accept, so the classifier
    /// must not claim it.
    ///
    /// If this funnel turned it into a typed error, the accept path could not
    /// tell it apart from a real failure without unwrapping the variant again —
    /// and a client that connected between create and accept would be dropped.
    /// That is the single easiest connection in this API to lose, and the
    /// easiest bug to write a test that never exercises.
    #[test]
    fn a_client_that_connected_early_is_not_classified_as_a_failure() {
        use windows::Win32::Foundation::ERROR_PIPE_CONNECTED;

        let got = Error::from_hresult(ERROR_PIPE_CONNECTED.to_hresult());
        assert!(
            matches!(got, Error::Os(_)),
            "ERROR_PIPE_CONNECTED must not be reclassified here; the accept \
             call site converts it to success, and a typed error would hide it. \
             Got {got:?}"
        );
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
            Error::BufferCheckedOut { index: 2 },
            Error::RegistrationSuperseded,
            Error::RegistrationPending,
            Error::ShuttingDown,
            Error::AbandonedAtShutdown,
            Error::ShutdownStalled { outstanding: 3 },
            Error::OperationOutstanding,
            Error::NoFileOffset { file_type: 3 },
            Error::RingClosed,
            Error::MissingField { field: "handle" },
            Error::Os(windows::core::Error::from(
                windows::Win32::Foundation::E_FAIL,
            )),
            Error::PipeBusy,
            Error::PipeBroken,
            Error::PipeNoPeer,
            Error::PipeListening,
            Error::AcceptOutstanding,
        ];
        for v in variants {
            assert!(!v.to_string().is_empty(), "empty Display for {v:?}");
        }
    }

    /// Fails to compile when a variant is added, so the list above cannot
    /// silently fall behind.
    ///
    /// The `Display` test builds its variants by hand, and nothing else would
    /// notice a new one being missed.
    fn _every_variant_is_listed_above(e: &Error) {
        match e {
            Error::Unsupported
            | Error::UnsupportedVersion { .. }
            | Error::UnsupportedFeature { .. }
            | Error::UnsupportedOp { .. }
            | Error::QueueFull
            | Error::InvalidRegisteredIndex { .. }
            | Error::BufferTooSmall { .. }
            | Error::UninitializedWriteRange { .. }
            | Error::RegisteredRangeOutOfBounds { .. }
            | Error::BufferCheckedOut { .. }
            | Error::RegistrationSuperseded
            | Error::RegistrationPending
            | Error::ShuttingDown
            | Error::AbandonedAtShutdown
            | Error::ShutdownStalled { .. }
            | Error::OperationOutstanding
            | Error::NoFileOffset { .. }
            | Error::RingClosed
            | Error::MissingField { .. }
            | Error::Os(_)
            | Error::PipeBusy
            | Error::PipeBroken
            | Error::PipeNoPeer
            | Error::PipeListening
            | Error::AcceptOutstanding => {}
        }
    }
}
