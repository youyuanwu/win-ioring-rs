//! The crate's classification tables.
//!
//! This module owns the crate's only mappings from an `HRESULT` to a condition:
//! one table for the four pipe codes and one for the single ring code, with
//! **disjoint** code sets, so every code is still compared in exactly one place.
//! It exposes no error type of its own: each public surface has its own, naming
//! only the conditions that surface can produce, with everything else carried
//! verbatim in that type's `Other` variant.
//!
//! | Surface | Type |
//! |---|---|
//! | buffers | [`crate::buf::Error`] |
//! | ring construction | [`crate::io_ring::BuildError`] |
//! | ring operations | [`crate::io_ring::Error`] |
//! | driver | [`crate::runtime::Error`] |
//! | files | [`crate::file::Error`] |
//! | pipes | [`crate::pipe::Error`] |
//!
//! The point of the arrangement is that a given code is classified in exactly
//! **one** place. `pipe::Error` reads the pipe table; `io_ring::Error` reads the
//! ring table; `file::Error` and `runtime::Error` compare nothing at all and
//! delegate to the ring surface, wrapping what it recognised and demoting the
//! rest. Two surfaces cannot disagree about what `ERROR_PIPE_BUSY` means,
//! because only one of them decides.
//!
//! A condition one surface names and another does not is demoted to `Other`
//! **carrying the code it arrived with**. So a pipe condition that passes
//! through a file error survives as `Other(ERROR_BROKEN_PIPE)` and a pipe
//! surface handed the same error recovers `Broken`. Nothing is invented and
//! nothing is lost.
//!
//! Only [`crate::pipe::Error`] names a pipe condition. Everything else demotes
//! it, because nothing else can know a handle is a pipe: `Handle::read` takes a
//! `&File`, `read_registered` takes a registration index, and `Client::file()`
//! hands out a `&File`. A type that cannot know does not claim to.
//!
//! The operation builders in [`crate::io_ring::ops`] are the one surface that
//! does not consult the platform at all: their `build` methods return
//! [`MissingField`](crate::io_ring::ops::MissingField), whose failure set is
//! closed by construction. `docs/errors-and-the-funnel.md` records how this
//! design relates to the funnel argument that preceded it.
//!
//! # Recovering the platform error
//!
//! Every public error type has an inherent
//! `os_error(&self) -> Option<&windows::core::Error>`. It answers one question:
//! **does this value carry a platform error?** It does not answer "what code did
//! this come from", and the difference is the whole of its contract.
//!
//! `Some` means the value holds a `windows::core::Error` — an `Other` variant, or
//! a nested type whose own `os_error` says so. `None` means it does not, either
//! because the crate detected the condition itself or because classification
//! *named* the condition and dropped the code.
//!
//! ## The direction that is exact, and the one that is not
//!
//! `HRESULT` -> error -> `os_error()` round-trips exactly for anything carried in
//! an `Other`. The reverse does not exist, and deliberately: there is no
//! infallible `to_hresult()`, because supplying one would mean inventing codes
//! for conditions the platform never reported. That is the one thing this
//! design forbids — see [`crate::file::Error::Driver`] — since a made-up code
//! can be re-classified into a condition that never occurred. `os_error`
//! returns `None` instead of guessing.
//!
//! ## Naming a condition discards its code; demoting one preserves it
//!
//! This is the surprising part, and it follows from the demotion rule above
//! rather than from anything `os_error` does. A view that *names* a condition
//! stores no code, because the variant has nowhere to put one. A view that
//! cannot name it demotes to `Other`, which carries the code.
//!
//! So one `ERROR_PIPE_BUSY` reaches two surfaces and answers differently:
//!
//! ```text
//! pipe::Error::Busy          -> os_error() == None    // named, code dropped
//! file::Error::Other(0x..E7) -> os_error() == Some(..) // demoted, code kept
//! ```
//!
//! Neither is lossy in the way that matters: the pipe surface already told you
//! it was busy, and the file surface kept the code because it could not.
//!
//! ## The variants that were classified but report `None`
//!
//! Six named variants are reached from a platform code yet carry none, because
//! their view discards the `HRESULT` when it names the condition:
//!
//! - [`crate::io_ring::Error::QueueFull`] — also produced by slab exhaustion,
//!   which has no code at all, so it could not carry one consistently even in
//!   principle
//! - [`crate::pipe::Error::Busy`], [`Broken`](crate::pipe::Error::Broken),
//!   [`NoPeer`](crate::pipe::Error::NoPeer),
//!   [`Listening`](crate::pipe::Error::Listening)
//! - [`crate::io_ring::BuildError::Unsupported`], which is `E_NOTIMPL` named
//!
//! [`crate::buf::Error`] reports `None` for every variant: both of its
//! conditions are counted by the crate, and neither has ever seen an `HRESULT`.
//!
//! # Platform availability
//!
//! This crate binds the Windows IoRing API through statically imported symbols
//! from `api-ms-win-core-ioring-l1-1-0.dll`. A host that lacks that API set
//! entirely cannot report an error here, because the process fails to load
//! before any code in this crate runs. [`crate::io_ring::BuildError::Unsupported`]
//! and [`crate::io_ring::BuildError::UnsupportedFeature`] therefore describe
//! hosts where the API set loads but does not provide what this crate needs.

mod classification {
    //! The crate's classification tables.
    //!
    //! # Two tables, and why that is still one home per code
    //!
    //! A code is compared in exactly one place. [`pipe_table`] holds the four codes
    //! the pipe surface names; [`ring_table`] holds the one code the ring surface
    //! names. The sets are **disjoint**, and `the_two_tables_name_disjoint_codes`
    //! asserts it by reading the tables themselves rather than a restatement of
    //! them -- so a code cannot be added to a classifier without the test seeing it.
    //!
    //! That disjointness is load-bearing. Two tables are safe only while no code
    //! appears in both: if `ERROR_BROKEN_PIPE` were added to the ring table,
    //! `pipe::Error` would still name it (it consults its own table first) while
    //! `file::Error` would receive whatever the ring table said, and the two would
    //! disagree about one code. That is precisely the hazard `pipe/client.rs` warns
    //! about in the crate's own voice.
    //!
    //! Every other type *delegates*: `file::Error` and `runtime::Error` wrap
    //! whatever the ring table recognised and demote everything else;
    //! `io_ring::BuildError` names nothing from a code at all. So no surface
    //! outside these two functions compares a code, and
    //! `error_classification_has_one_home` enforces that against the source text.
    //!
    //! # Visibility of the condition types
    //!
    //! The condition types are `pub(crate)`, because the `From` implementations that
    //! consult the tables live in the modules that own each error type. They are not
    //! publicly re-exported, so they add no public variant slots -- but they *are*
    //! nameable crate-wide, and it is worth being exact about what that costs.
    //!
    //! It does not weaken the one-code-one-home property. That property concerns the
    //! mapping from a **code** to a condition, which happens only in [`pipe_table`]
    //! and [`ring_table`] and is policed by `error_classification_has_one_home`.
    //!
    //! It does mean a second mapping from a *condition* to some type's variants is
    //! representable. It always was: the `ConditionView` trait this replaced was
    //! itself `pub(crate)` and implementable by any type. So this is unchanged, and
    //! the earlier claim that a second match was "unrepresentable" was too strong
    //! even then -- a view method received the raw `HRESULT` and could always have
    //! compared it.
    //!
    //! This replaces a source-scanning test that tried to recognise every match on a
    //! condition and decide whether it was armed. That test was defeated nine times
    //! across three review rounds, always by a syntactic form it had not
    //! anticipated. Privacy does not have to anticipate anything.
    //!
    //! # What this replaced, and what that mechanism actually bought
    //!
    //! This was a `Condition` enum with a `ConditionView` trait -- one method per
    //! condition, implemented by five error types, dispatched by a single `view`
    //! function. Adding a condition was `E0004` at the dispatch and `E0046` at every
    //! view, under plain `cargo build`.
    //!
    //! It was adopted for good reason, defended twice against proposals to remove
    //! it, and retired when a prototype showed its load-bearing justification did
    //! not hold. The justification had been that four views must *recognise*
    //! `IORING_E_SUBMISSION_QUEUE_FULL`, so one table did work no `From`
    //! implementation could absorb. They do not recognise it -- they **wrap** what
    //! the ring surface recognised, which delegation absorbs exactly. Five impls of
    //! thirty methods were expressing five code comparisons and three wrapping
    //! rules, and two of the five impls (`file` and `runtime`) were textually
    //! identical while a third (`BuildError`) named nothing.
    //!
    //! The lesson worth keeping is narrower than the one first drawn, and getting it
    //! wrong is how the mechanism was over-credited: **the trait bought totality,
    //! not singularity.** It forced every view to *account for* every condition. It
    //! never prevented a second table -- a view method received the raw `HRESULT`
    //! and could always have compared it. Preventing a second table was, and
    //! remains, the job of `error_classification_has_one_home`. Retiring the trait
    //! surrendered `E0046`, and `E0046`'s benefit did not survive attack: a type
    //! that cannot name a condition must demote it, and demotion is what an
    //! unrecognised code already does.
    //!
    //! # Scope
    //!
    //! These tables cover *platform-derived* conditions only. Conditions the crate
    //! detects itself -- a shutting-down runtime, a builder field never set -- are
    //! produced at the call site that knows about them and never pass through here.
    //!
    //! `BuildError::Unsupported` is not in either table: it comes from
    //! `from_create_failure`, which reads `E_NOTIMPL` from ring-creation entry
    //! points only. Classifying it here would reclassify every unrelated
    //! `E_NOTIMPL` in the crate.

    /// A pipe condition this crate names.
    ///
    /// `pub(crate)`, in no public signature, contributing no public variant slots;
    /// `condition_is_not_part_of_the_public_api` checks that this stays true.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum PipeCondition {
        /// All instances of the named pipe are busy.
        Busy,
        /// The pipe's peer closed its end.
        Broken,
        /// The pipe has no peer connected.
        NoPeer,
        /// The pipe is listening and has not yet been connected to.
        Listening,
    }

    /// A ring condition this crate names.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum RingCondition {
        /// The ring's submission queue has no room for another entry.
        QueueFull,
    }

    /// The codes the pipe surface names, and what each one means.
    ///
    /// This array **is** the table. [`classify_pipe`] consults it and
    /// `the_two_tables_name_disjoint_codes` reads it, so a code added here is
    /// visible to both and a code added to neither cannot be classified. A
    /// hand-written `if` chain beside a hand-written test list is the shape this
    /// avoids: the list can stay at four while the chain grows to five, and the
    /// test then passes while proving nothing.
    ///
    /// Codes are matched **exactly**, never by facility or range. These classifiers
    /// see every ring completion in the crate, so a range match would reclassify
    /// errors from files and sockets that happen to fall inside it.
    ///
    /// `ERROR_PIPE_CONNECTED` is deliberately absent. It reports that a client
    /// arrived before the accept was issued, which is a **success** for the accept
    /// and is converted at that call site; classifying it here would turn the most
    /// easily lost connection in the API into an error.
    pub(crate) fn pipe_table() -> &'static [(windows::core::HRESULT, PipeCondition)] {
        use windows::Win32::Foundation::{
            ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_LISTENING,
        };
        const TABLE: &[(windows::core::HRESULT, PipeCondition)] = &[
            (ERROR_PIPE_BUSY.to_hresult(), PipeCondition::Busy),
            (ERROR_BROKEN_PIPE.to_hresult(), PipeCondition::Broken),
            (ERROR_NO_DATA.to_hresult(), PipeCondition::NoPeer),
            (ERROR_PIPE_LISTENING.to_hresult(), PipeCondition::Listening),
        ];
        TABLE
    }

    /// The codes the ring surface names.
    ///
    /// See [`pipe_table`] for why this is an array rather than a chain of
    /// comparisons.
    pub(crate) fn ring_table() -> &'static [(windows::core::HRESULT, RingCondition)] {
        use windows::Win32::Foundation::IORING_E_SUBMISSION_QUEUE_FULL;
        const TABLE: &[(windows::core::HRESULT, RingCondition)] =
            &[(IORING_E_SUBMISSION_QUEUE_FULL, RingCondition::QueueFull)];
        TABLE
    }

    /// Maps a code to the pipe condition it denotes, if any.
    ///
    /// `None` means "not a pipe condition", which every caller turns into a demotion
    /// carrying the original code.
    pub(crate) fn classify_pipe(hr: windows::core::HRESULT) -> Option<PipeCondition> {
        pipe_table()
            .iter()
            .find(|(code, _)| *code == hr)
            .map(|(_, condition)| *condition)
    }

    /// Maps a code to the ring condition it denotes, if any.
    pub(crate) fn classify_ring(hr: windows::core::HRESULT) -> Option<RingCondition> {
        ring_table()
            .iter()
            .find(|(code, _)| *code == hr)
            .map(|(_, condition)| *condition)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The two tables must never name the same code.
        ///
        /// This is what makes two classification tables as safe as one. Every code
        /// is compared in exactly one place *because the sets are disjoint*; if a
        /// code appeared in both, `pipe::Error` would name it from the pipe table
        /// while `file::Error` and `runtime::Error` -- which delegate to the ring
        /// surface -- would take the ring table's answer, and one code would mean
        /// two things. That is the divergence hazard `pipe/client.rs` warns about,
        /// reintroduced by the back door.
        ///
        /// # Why this reads the tables rather than a list of codes
        ///
        /// A guard that compared two hand-written lists would pass while proving
        /// nothing: add a code to a classifier, forget its list, and the assertion
        /// still compares the stale lists and still succeeds. This reads
        /// `pipe_table` and `ring_table` themselves -- the same arrays the
        /// classifiers consult -- so a code cannot exist in a classifier and be
        /// invisible here.
        ///
        /// It is a *value* check, not a text scan. There is no way to spell a code
        /// that makes set intersection miss it, which is the property the crate's
        /// nine defeated source-scanning guards lacked.
        #[test]
        fn the_two_tables_name_disjoint_codes() {
            let pipe: Vec<windows::core::HRESULT> =
                super::pipe_table().iter().map(|(hr, _)| *hr).collect();
            let ring: Vec<windows::core::HRESULT> =
                super::ring_table().iter().map(|(hr, _)| *hr).collect();

            let shared: Vec<_> = pipe.iter().filter(|hr| ring.contains(hr)).collect();
            assert!(
                shared.is_empty(),
                "these codes are named by both classification tables: {shared:?}. \
                 Two tables are safe only while they are disjoint: a shared code is \
                 named by `pipe::Error` from the pipe table and by `file::Error` \
                 through delegation to the ring table, and the two answers can \
                 differ. Put the code in exactly one table."
            );

            // A table that named the same code twice would also make one of the two
            // entries unreachable, since both classifiers stop at the first match.
            for (table, name) in [(&pipe, "pipe"), (&ring, "ring")] {
                let mut seen = Vec::new();
                for hr in table.iter() {
                    assert!(
                        !seen.contains(&hr),
                        "the {name} table names {hr:?} twice; the second entry is \
                         unreachable"
                    );
                    seen.push(hr);
                }
            }

            assert!(!pipe.is_empty() && !ring.is_empty(), "a table went empty");
        }

        /// A code the table does not name must arrive as `Other`, carrying that
        /// exact code.
        ///
        /// This is the property the `Other` variant exists for. Asserting the code
        /// survives, rather than merely that `Other` was produced, is what makes it
        /// a guarantee instead of a catch-all.
        #[test]
        fn an_unnamed_code_is_carried_verbatim() {
            let os = windows::core::Error::from(windows::Win32::Foundation::E_FAIL);
            assert_eq!(classify_pipe(os.code()), None);
            assert_eq!(classify_ring(os.code()), None);

            let err: crate::runtime::error::Error = os.clone().into();
            match err {
                crate::runtime::error::Error::Other(recovered) => {
                    assert_eq!(recovered.code(), os.code());
                }
                other => panic!("expected Other, got {other:?}"),
            }
        }

        /// Each pipe condition the table names must be a condition of its own.
        ///
        /// Callers distinguish these by pattern. Two conditions sharing one
        /// `Condition` would be indistinguishable without parsing a rendered
        /// string, and the pairs below are the ones most likely to be conflated by
        /// a well-meaning simplification: busy is transient and worth retrying
        /// while listening is not, and broken means the peer left while no-peer
        /// means there has not been one.
        ///
        /// Asserted at the table rather than at a view, because the table is now
        /// the single place the distinction is made -- every view inherits it.
        #[test]
        fn each_pipe_condition_is_its_own_condition() {
            use windows::Win32::Foundation::{
                ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_LISTENING,
            };

            let cases = [
                (ERROR_PIPE_BUSY, PipeCondition::Busy),
                (ERROR_BROKEN_PIPE, PipeCondition::Broken),
                (ERROR_NO_DATA, PipeCondition::NoPeer),
                (ERROR_PIPE_LISTENING, PipeCondition::Listening),
            ];

            let mut seen: Vec<String> = Vec::new();
            for (code, expected) in cases {
                let got = classify_pipe(code.to_hresult()).expect("the table names this code");
                assert_eq!(
                    std::mem::discriminant(&got),
                    std::mem::discriminant(&expected),
                    "{code:?} classified as {got:?}, expected {expected:?}"
                );
                // And the surface that names all four must render them apart.
                let rendered = crate::pipe::error::Error::from(code.to_hresult()).to_string();
                assert!(
                    !seen.contains(&rendered),
                    "two pipe conditions render identically: {rendered:?}"
                );
                seen.push(rendered);
            }
        }

        /// The classifier matches exact codes, and must leave everything else
        /// unnamed.
        ///
        /// The tables are what every surface reads, directly or by delegation, so
        /// widening one from exact codes to a facility or a range would silently
        /// reclassify file and socket errors that have nothing to do with pipes --
        /// and it would do
        /// so at *six* types at once. The two codes below are not hypothetical:
        /// they are what the existing `Other` assertions in `runtime_tests.rs`
        /// actually observe -- end-of-file on a read past the end, and the refusal
        /// of a write-through on cached I/O -- measured rather than assumed,
        /// because "the new variants cannot collide with anything" is exactly the
        /// comfortable claim that deserves evidence.
        #[test]
        fn codes_outside_the_pipe_set_are_still_reported_verbatim() {
            use windows::Win32::Foundation::{ERROR_HANDLE_EOF, WIN32_ERROR};

            // 509 is what the cached-I/O write-through refusal reports.
            for code in [ERROR_HANDLE_EOF, WIN32_ERROR(509)] {
                let hr = code.to_hresult();
                assert!(
                    classify_pipe(hr).is_none() && classify_ring(hr).is_none(),
                    "{code:?} must stay unnamed, got {:?}/{:?}",
                    classify_pipe(hr),
                    classify_ring(hr)
                );
            }

            // Adjacent to the pipe codes on both sides, to catch a range match that
            // happened to bracket them.
            for code in [230_u32, 233, 534, 537] {
                let hr = WIN32_ERROR(code).to_hresult();
                assert!(
                    classify_pipe(hr).is_none() && classify_ring(hr).is_none(),
                    "code {code} is not a condition this crate maps, got {:?}/{:?}",
                    classify_pipe(hr),
                    classify_ring(hr)
                );
            }
        }

        /// `ERROR_PIPE_CONNECTED` is a success for an accept, so the table must not
        /// claim it.
        ///
        /// If the table named it, the accept path could not tell it apart from a
        /// real failure without unwrapping the condition again -- and a client that
        /// connected between create and accept would be dropped. That is the single
        /// easiest connection in this API to lose, and the easiest bug to write a
        /// test that never exercises.
        #[test]
        fn a_client_that_connected_early_is_not_classified_as_a_failure() {
            use windows::Win32::Foundation::ERROR_PIPE_CONNECTED;

            let got = classify_pipe(ERROR_PIPE_CONNECTED.to_hresult());
            assert!(
                got.is_none(),
                "ERROR_PIPE_CONNECTED must not be named here; the accept call site \
                 converts it to success, and a named condition would hide it. \
                 Got {got:?}"
            );
        }

        /// A condition with no platform error beneath it must report no source.
        ///
        /// Checked on two types because `source` is now written once per type, so
        /// this is six opportunities to get it wrong rather than one.
        #[test]
        fn conditions_without_a_platform_error_have_no_source() {
            use std::error::Error as _;
            assert!(crate::io_ring::error::Error::QueueFull.source().is_none());
            assert!(
                crate::runtime::error::Error::TooManyOperations
                    .source()
                    .is_none()
            );
        }

        /// Every completion-derived pipe condition survives a trip through a type
        /// that has no name for it.
        ///
        /// A surface that cannot name a condition demotes it to `Other` carrying
        /// the code, so a surface that can name it recovers it. Since the driver
        /// no longer names any pipe condition, the demotion needs no inverse: the
        /// code the platform sent is the code that travels. SC-9.
        ///
        /// # What this actually proves
        ///
        /// The design's central claim is that classification can be *deferred to the
        /// boundary*: the driver classifies once, and a surface that cannot name a
        /// condition demotes it to `Other` **carrying the code**, so a surface that can
        /// name it recovers it. If that fails for even one condition, the claim is false
        /// and `docs/errors-and-the-funnel.md` was right after all.
        ///
        /// Two earlier arguments in this series asserted that failure, in opposite
        /// directions, and neither demonstrated it. So this is a demonstration.
        ///
        /// # Why the list is built by an exhaustive match
        ///
        /// A hand-written list of four conditions is a list that silently stays at four
        /// when a fifth is added. Matching on `Condition` makes adding a variant `E0004`
        /// here, so whoever adds it has to say whether it round-trips. That is the same
        /// reasoning the views themselves are built on.
        #[test]
        fn every_completion_derived_pipe_condition_survives_a_type_that_cannot_name_it() {
            use crate::pipe::Error as P;
            use crate::runtime::Error as R;
            use windows::Win32::Foundation::{
                ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_LISTENING,
            };

            /// Whether this condition can arrive on a *completion*, and if so what the
            /// pipe surface must call it.
            ///
            /// Returning `None` is a claim, so each one carries its justification.
            fn expected(c: PipeCondition) -> Option<(windows::core::HRESULT, &'static str)> {
                match c {
                    PipeCondition::Busy => Some((ERROR_PIPE_BUSY.to_hresult(), "Busy")),
                    PipeCondition::Broken => Some((ERROR_BROKEN_PIPE.to_hresult(), "Broken")),
                    PipeCondition::NoPeer => Some((ERROR_NO_DATA.to_hresult(), "NoPeer")),
                    PipeCondition::Listening => {
                        Some((ERROR_PIPE_LISTENING.to_hresult(), "Listening"))
                    }
                }
            }

            // Driven from the table rather than from a list beside it. The previous
            // version enumerated the conditions by hand, which is a list that stays
            // at four while the table grows to five; reading `pipe_table` means a new
            // entry is exercised the moment it is added, and `expected` is an
            // exhaustive match so it is also `E0004` until someone says what the new
            // condition round-trips to.
            let all: Vec<PipeCondition> = super::pipe_table().iter().map(|(_, c)| *c).collect();
            assert_eq!(
                all.len(),
                super::pipe_table().len(),
                "every table entry must be exercised"
            );

            let mut checked = 0;
            for condition in all {
                let Some((hr, name)) = expected(condition) else {
                    continue;
                };

                // The demotion. A surface that cannot name a pipe condition carries the
                // code instead, and `runtime::Error::Other` is what does the carrying.
                let demoted = R::Other(windows::core::Error::from_hresult(hr));

                // The recovery, at a surface that can name it.
                let recovered = P::from(demoted);

                let actual = match recovered {
                    P::Busy => "Busy",
                    P::Broken => "Broken",
                    P::NoPeer => "NoPeer",
                    P::Listening => "Listening",
                    ref other => panic!(
                        "{name} did not survive the round trip: {other:?}. The design's \
                         central claim is that a condition demoted to a code by one \
                         surface is recovered by another; this is that claim failing."
                    ),
                };
                assert_eq!(actual, name, "{name} round-tripped to the wrong condition");
                checked += 1;
            }

            assert_eq!(
                checked, 4,
                "expected four completion-derived pipe conditions, got {checked}. A \
                 lower number means `expected` started returning `None` for one of them \
                 and this test quietly stopped covering it."
            );
        }

        /// SC-9's companion: the demotion is real, not a no-op.
        ///
        /// Without this, the test above would pass just as well if `file::Error`
        /// happened to name the pipe conditions after all. It asserts that recovery
        /// works; this asserts that something was lost for recovery to recover.
        #[test]
        fn a_file_surface_genuinely_cannot_name_a_pipe_condition() {
            use crate::file::Error as F;

            use windows::Win32::Foundation::{
                ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_LISTENING,
            };

            for (hr, name) in [
                (ERROR_PIPE_BUSY.to_hresult(), "PipeBusy"),
                (ERROR_BROKEN_PIPE.to_hresult(), "PipeBroken"),
                (ERROR_NO_DATA.to_hresult(), "PipeNoPeer"),
                (ERROR_PIPE_LISTENING.to_hresult(), "PipeListening"),
            ] {
                let demoted = F::from(crate::runtime::Error::Other(
                    windows::core::Error::from_hresult(hr),
                ));
                match demoted {
                    F::Other(ref e) => assert_eq!(
                        e.code(),
                        hr,
                        "{name} demoted to `Other` but lost its code, which would make \
                         recovery impossible"
                    ),
                    other => panic!(
                        "{name} is named on the file surface as {other:?}; if that is \
                         intended, the round-trip test above is not testing recovery at \
                         all"
                    ),
                }
            }
        }

        /// SC-17: every view type carries an unrecognised code through Other
        /// without losing it.
        ///
        /// This is the property the whole design rests on. A type that names only
        /// the conditions its surface can produce is only honest if everything it
        /// does *not* name survives intact — otherwise the split would be trading
        /// precision at one surface for lost information at another. Asserting the
        /// code is recoverable, not merely that some Other was produced, is the
        /// difference between the two.
        #[test]
        fn every_view_carries_an_unrecognised_code_through_other() {
            // Not in the table, and not plausibly added to it.
            let hr = windows::core::HRESULT(0x8007_0525_u32 as i32);

            macro_rules! assert_carries {
                ($ty:ty, $pat:path) => {{
                    let e: $ty = <$ty>::from(hr);
                    match e {
                        $pat(inner) => assert_eq!(
                            inner.code(),
                            hr,
                            concat!(stringify!($ty), " lost the code it could not name")
                        ),
                        other => panic!(
                            concat!(stringify!($ty), " did not demote to Other: {:?}"),
                            other
                        ),
                    }
                }};
            }

            assert_carries!(
                crate::io_ring::error::BuildError,
                crate::io_ring::error::BuildError::Other
            );
            assert_carries!(
                crate::io_ring::error::Error,
                crate::io_ring::error::Error::Other
            );
            assert_carries!(
                crate::runtime::error::Error,
                crate::runtime::error::Error::Other
            );
            assert_carries!(crate::file::error::Error, crate::file::error::Error::Other);
            assert_carries!(crate::pipe::error::Error, crate::pipe::error::Error::Other);
        }

        /// The four pipe conditions demote on `file::Error` **with the code
        /// intact**, which is what makes the recovery path of §4.3 possible.
        ///
        /// `file::Error` deliberately does not name them. If it demoted them to a
        /// substituted code — `E_FAIL`, say — a pipe surface handed the same error
        /// could never recover the condition, and the split would be lossy in
        /// exactly the way the design claims it is not. Proven here rather than
        /// asserted, because it is a property of five hand-written methods.
        #[test]
        fn file_error_demotes_pipe_conditions_without_substituting_the_code() {
            use windows::Win32::Foundation::{
                ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_LISTENING,
            };

            for win32 in [
                ERROR_PIPE_BUSY,
                ERROR_BROKEN_PIPE,
                ERROR_NO_DATA,
                ERROR_PIPE_LISTENING,
            ] {
                let hr = win32.to_hresult();
                let demoted = crate::file::error::Error::from(hr);
                let carried = match demoted {
                    crate::file::error::Error::Other(ref e) => e.code(),
                    ref other => {
                        panic!("file::Error named a pipe condition it should not: {other:?}")
                    }
                };
                assert_eq!(carried, hr, "file::Error substituted a code for {win32:?}");

                // And the code it carried re-classifies to the condition the pipe
                // surface names, which is the round trip itself.
                let recovered = crate::pipe::error::Error::from(carried);
                assert!(
                    !matches!(recovered, crate::pipe::error::Error::Other(_)),
                    "re-classifying {win32:?} at the pipe surface did not recover a named condition"
                );
            }
        }
    }
}

pub(crate) use classification::{PipeCondition, RingCondition, classify_pipe, classify_ring};

#[cfg(test)]
mod conversion_tests {
    //! One test per conversion the design declares (Spec §3.4).
    //!
    //! Six types replaced one, and the seams between them are `From` impls.
    //! Those are the joints the whole split turns on: a caller who gets a
    //! `file::Error` from a buffer fault, or a `pipe::Error` from a builder, is
    //! reading something that crossed one of these.
    //!
    //! Every assertion here checks the **payload**, not merely the variant.
    //! Matching `Error::Buf(_)` would pass against a conversion that threw the
    //! numbers away and substituted zeroes; matching the numbers will not. A
    //! conversion that loses its payload still typechecks, still returns the
    //! right variant, and degrades the error to "something went wrong" — so
    //! variant-only assertions would pin the shape and miss the content.
    //!
    //! **These live in the crate rather than in `win-ioring-tests` because
    //! every error enum here is `#[non_exhaustive]`**, which makes its variants
    //! unconstructible from outside. That is deliberate and worth keeping, but
    //! it means a conversion test cannot be written as an integration test at
    //! all: there is no way to build the input. Discovered by trying.
    //!
    //! The rows fall into two families:
    //!
    //! - **Wrapping** (rows 1–3): `io_ring::Error`, `buf::Error` and
    //!   `ops::MissingField` are carried into the surface types whole. Nothing
    //!   can go stale, because adding a variant to the source needs no edit at
    //!   the destination.
    //! - **Viewing** (rows 4–6): `runtime::Error` into the two boundary types,
    //!   and a condition into every view. These *are* per-variant, and are the
    //!   ones a new variant can leave behind — which is why the mechanism makes
    //!   that a compile error rather than something a test must notice.

    use crate::buf::error::Error as BufError;
    use crate::file::error::Error as FileError;
    use crate::io_ring::error::Error as RingError;
    use crate::io_ring::ops::MissingField;
    use crate::pipe::error::Error as PipeError;
    use crate::runtime::error::Error as RuntimeError;

    /// A code no surface in this crate names, so every view must demote it.
    ///
    /// `E_UNEXPECTED` is not plausibly reachable from any I/O path, so a view
    /// that grows a name for it later has done something wrong.
    const UNNAMEABLE: windows::core::HRESULT = windows::core::HRESULT(0x8000_FFFFu32 as i32);

    /// A code the pipe surface names and the file surface does not.
    ///
    /// This asymmetry is what the design is built on, so it is worth a name.
    const PIPE_BROKEN: windows::core::HRESULT = windows::core::HRESULT(0x8007_006Du32 as i32);

    // -- Row 1: `io_ring::Error` into the three surfaces reporting ring faults.

    #[test]
    fn a_ring_fault_reaches_every_surface_with_its_own_identity_intact() {
        // `UnsupportedOp` rather than `QueueFull`: it carries data. A
        // conversion that substituted a default would be invisible against a
        // fieldless variant, and a lost payload is this row's only real risk.
        let source = || RingError::UnsupportedOp { op: 4242 };

        let into_runtime: RuntimeError = source().into();
        assert!(
            matches!(
                into_runtime,
                RuntimeError::Ring(RingError::UnsupportedOp { op: 4242 })
            ),
            "got {into_runtime:?}"
        );

        let into_file: FileError = source().into();
        assert!(
            matches!(
                into_file,
                FileError::Ring(RingError::UnsupportedOp { op: 4242 })
            ),
            "got {into_file:?}"
        );

        let into_pipe: PipeError = source().into();
        assert!(
            matches!(
                into_pipe,
                PipeError::Ring(RingError::UnsupportedOp { op: 4242 })
            ),
            "got {into_pipe:?}"
        );
    }

    // -- Row 2: `buf::Error` into the surfaces that can fault on a buffer.
    //
    // Spec §3.4 lists this row as reaching `file::Error` and `pipe::Error`. It
    // also reaches `runtime::Error` (`runtime/error.rs:203`), which the table
    // omits; the third conversion is exercised here so the omission cannot
    // quietly become a gap.

    #[test]
    fn a_buffer_fault_reaches_every_surface_carrying_the_counts_that_explain_it() {
        let source = || BufError::TooSmall {
            requested: 9_001,
            available: 17,
        };

        let into_file: FileError = source().into();
        assert!(
            matches!(
                into_file,
                FileError::Buf(BufError::TooSmall {
                    requested: 9_001,
                    available: 17
                })
            ),
            "got {into_file:?}"
        );

        let into_pipe: PipeError = source().into();
        assert!(
            matches!(
                into_pipe,
                PipeError::Buf(BufError::TooSmall {
                    requested: 9_001,
                    available: 17
                })
            ),
            "got {into_pipe:?}"
        );

        let into_runtime: RuntimeError = source().into();
        assert!(
            matches!(
                into_runtime,
                RuntimeError::Buf(BufError::TooSmall {
                    requested: 9_001,
                    available: 17
                })
            ),
            "got {into_runtime:?}"
        );

        // The other variant too: one variant passing says nothing about a
        // conversion that treats the two differently.
        let uninitialized: FileError = BufError::UninitializedWriteRange {
            requested: 64,
            initialized: 8,
        }
        .into();
        assert!(
            matches!(
                uninitialized,
                FileError::Buf(BufError::UninitializedWriteRange {
                    requested: 64,
                    initialized: 8
                })
            ),
            "got {uninitialized:?}"
        );
    }

    // -- Row 3: `ops::MissingField` into the three surfaces that build ops.

    #[test]
    fn a_builder_fault_reaches_every_surface_still_naming_the_field() {
        // The field name is the entire content of this error. Dropping it
        // leaves a caller with "a field was missing" and no way to learn which,
        // which is worse than useless in a builder with a dozen setters.
        let source = || MissingField {
            field: "raw_data_address",
        };

        let into_runtime: RuntimeError = source().into();
        assert!(
            matches!(
                into_runtime,
                RuntimeError::MissingField {
                    field: "raw_data_address"
                }
            ),
            "got {into_runtime:?}"
        );

        let into_file: FileError = source().into();
        assert!(
            matches!(
                into_file,
                FileError::MissingField {
                    field: "raw_data_address"
                }
            ),
            "got {into_file:?}"
        );

        let into_pipe: PipeError = source().into();
        assert!(
            matches!(
                into_pipe,
                PipeError::MissingField {
                    field: "raw_data_address"
                }
            ),
            "got {into_pipe:?}"
        );
    }

    // -- Rows 4 and 5: `runtime::Error` into the two boundary types.

    #[test]
    fn the_runtime_error_a_file_can_name_arrives_named_and_the_rest_are_carried() {
        let named: FileError = RuntimeError::TooManyOperations.into();
        assert!(
            matches!(named, FileError::TooManyOperations),
            "got {named:?}"
        );

        // A driver-only condition has nowhere to land on a file, and must not
        // be invented into an `HRESULT` it never had. It is carried whole
        // instead, which is what the `Driver` variant exists for.
        let driver_only: FileError = RuntimeError::RegistrationPending.into();
        assert!(
            matches!(&driver_only, FileError::Driver(inner)
                if matches!(**inner, RuntimeError::RegistrationPending)),
            "a driver-only condition must be carried, not flattened: got {driver_only:?}"
        );
    }

    #[test]
    fn the_runtime_error_a_pipe_can_name_arrives_named_and_the_rest_are_carried() {
        let named: PipeError = RuntimeError::TooManyOperations.into();
        assert!(
            matches!(named, PipeError::TooManyOperations),
            "got {named:?}"
        );

        let driver_only: PipeError = RuntimeError::RegistrationPending.into();
        assert!(
            matches!(&driver_only, PipeError::Driver(inner)
                if matches!(**inner, RuntimeError::RegistrationPending)),
            "a driver-only condition must be carried, not flattened: got {driver_only:?}"
        );
    }

    // -- Row 6: a condition into every view, reached publicly through
    //    `From<HRESULT>`, which classifies and then dispatches.

    #[test]
    fn one_code_reaches_every_view_and_each_answers_with_what_it_can_say() {
        let pipe_view: PipeError = PIPE_BROKEN.into();
        assert!(matches!(pipe_view, PipeError::Broken), "got {pipe_view:?}");

        // A file surface cannot name it, so it demotes — keeping the code,
        // which is the property the recovery path then relies on.
        let file_view: FileError = PIPE_BROKEN.into();
        match &file_view {
            FileError::Other(e) => assert_eq!(
                e.code(),
                PIPE_BROKEN,
                "a demoted condition must keep the code it came in with, or nothing \
                 downstream can recover it"
            ),
            other => panic!("a file cannot name a broken pipe, so it must demote: got {other:?}"),
        }

        // The runtime reaps the completion but does not name it: it is reached
        // through a `&File` or a registration index, neither of which can say
        // the handle is a pipe. It demotes, keeping the code, exactly as the
        // file surface does.
        let runtime_view: RuntimeError = PIPE_BROKEN.into();
        match &runtime_view {
            RuntimeError::Other(e) => assert_eq!(
                e.code(),
                PIPE_BROKEN,
                "the driver must carry the code it could not name, or the pipe                  surface has nothing to recover from"
            ),
            other => panic!(
                "the driver names a pipe condition again: got {other:?}. Only                  `pipe::Error` may name one — see `impl From<HRESULT>` in                  `runtime/error.rs` for why."
            ),
        }
    }

    #[test]
    fn a_code_no_view_names_demotes_everywhere_rather_than_being_forced_into_a_variant() {
        // `Other` is what makes the per-API split affordable: a surface lists
        // what it can produce and everything else falls through *carrying its
        // code*. If any view invented a name for an arbitrary code, the
        // fall-through would be lossy and the recovery path unsound.
        let from_file = match FileError::from(UNNAMEABLE) {
            FileError::Other(e) => e.code(),
            other => panic!("file must demote an unknown code: got {other:?}"),
        };
        let from_pipe = match PipeError::from(UNNAMEABLE) {
            PipeError::Other(e) => e.code(),
            other => panic!("pipe must demote an unknown code: got {other:?}"),
        };
        let from_runtime = match RuntimeError::from(UNNAMEABLE) {
            RuntimeError::Other(e) => e.code(),
            other => panic!("runtime must demote an unknown code: got {other:?}"),
        };

        for (label, code) in [
            ("file", from_file),
            ("pipe", from_pipe),
            ("runtime", from_runtime),
        ] {
            assert_eq!(
                code, UNNAMEABLE,
                "{label} dropped the code it could not name, leaving nothing to recover"
            );
        }
    }
}

#[cfg(test)]
mod os_error_tests {
    //! `os_error()` on each of the six public error types.
    //!
    //! These live in the crate for the same reason as `conversion_tests`: every
    //! error enum is `#[non_exhaustive]`, so its variants cannot be constructed
    //! from an integration test.
    //!
    //! The property under test is **what a value carries**, not what produced
    //! it. The two are deliberately different, and the test that matters most is
    //! `the_same_code_answers_differently_on_two_surfaces` \u2014 one `HRESULT`
    //! reaching two views, `Some` on the one that had to demote it and `None` on
    //! the one that could name it.
    //!
    //! Exhaustiveness is not tested here because it is not testable here, and
    //! does not need to be: a missing arm is `E0004` under plain `cargo build`,
    //! and the wildcard that would silence `E0004` is itself a hard error from
    //! `#[deny(clippy::wildcard_enum_match_arm)]` on each accessor. Two doors,
    //! both shut at compile time.
    //!
    //! Both were opened deliberately to confirm they shut. Adding a variant to
    //! `pipe::Error` produced `E0004` inside `os_error` specifically \u2014 not only
    //! in `Display` and `source` \u2014 and replacing that accessor's named arms
    //! with `_ => None` produced `error: wildcard matches known variants`. See
    //! `docs/testing.md` for why a guard of this shape is preferred here to one
    //! that reads the source.

    use crate::buf::error::Error as BufError;
    use crate::file::error::Error as FileError;
    use crate::io_ring::error::BuildError;
    use crate::io_ring::error::Error as RingError;
    use crate::pipe::error::Error as PipeError;
    use crate::runtime::error::Error as RuntimeError;

    /// A code no surface names, so every view demotes it into `Other`.
    const UNNAMEABLE: windows::core::HRESULT = windows::core::HRESULT(0x8000_FFFFu32 as i32);

    /// `ERROR_BROKEN_PIPE`: the pipe surface names it, the file surface cannot.
    const PIPE_BROKEN: windows::core::HRESULT = windows::core::HRESULT(0x8007_006Du32 as i32);

    fn carried(hr: windows::core::HRESULT) -> windows::core::Error {
        windows::core::Error::from_hresult(hr)
    }

    #[test]
    fn an_unnamed_code_is_recoverable_from_every_view() {
        assert_eq!(
            BuildError::from(UNNAMEABLE).os_error().map(|e| e.code()),
            Some(UNNAMEABLE)
        );
        assert_eq!(
            RingError::from(UNNAMEABLE).os_error().map(|e| e.code()),
            Some(UNNAMEABLE)
        );
        assert_eq!(
            FileError::from(UNNAMEABLE).os_error().map(|e| e.code()),
            Some(UNNAMEABLE)
        );
        assert_eq!(
            PipeError::from(UNNAMEABLE).os_error().map(|e| e.code()),
            Some(UNNAMEABLE)
        );
        assert_eq!(
            RuntimeError::from(UNNAMEABLE).os_error().map(|e| e.code()),
            Some(UNNAMEABLE)
        );
    }

    #[test]
    fn the_same_code_answers_differently_on_two_surfaces() {
        // The file surface cannot name a pipe condition, so it demotes to
        // `Other` and the code survives.
        let on_a_file = FileError::from(PIPE_BROKEN);
        assert!(matches!(on_a_file, FileError::Other(_)));
        assert_eq!(on_a_file.os_error().map(|e| e.code()), Some(PIPE_BROKEN));

        // The pipe surface names it, which is the more useful answer and the
        // one that drops the code.
        let on_a_pipe = PipeError::from(PIPE_BROKEN);
        assert!(matches!(on_a_pipe, PipeError::Broken));
        assert_eq!(on_a_pipe.os_error(), None);
    }

    #[test]
    fn a_named_condition_reports_none_on_every_surface_that_names_it() {
        assert_eq!(PipeError::from(PIPE_BROKEN).os_error(), None);
        // The driver demotes rather than names, so it reports `Some` here. That
        // is the whole difference the accessor exposes, and asserting it beside
        // the pipe surface is what keeps the two readings visibly distinct.
        assert_eq!(
            RuntimeError::from(PIPE_BROKEN).os_error().map(|e| e.code()),
            Some(PIPE_BROKEN)
        );
        // `QueueFull` is named by the ring view and wrapped by the rest.
        let queue_full =
            RingError::from(windows::Win32::Foundation::IORING_E_SUBMISSION_QUEUE_FULL);
        assert!(matches!(queue_full, RingError::QueueFull));
        assert_eq!(queue_full.os_error(), None);
    }

    #[test]
    fn nesting_is_traversed_to_the_error_the_inner_type_carries() {
        assert_eq!(
            FileError::Ring(RingError::Other(carried(UNNAMEABLE)))
                .os_error()
                .map(|e| e.code()),
            Some(UNNAMEABLE)
        );
        assert_eq!(
            PipeError::Ring(RingError::Other(carried(UNNAMEABLE)))
                .os_error()
                .map(|e| e.code()),
            Some(UNNAMEABLE)
        );
        assert_eq!(
            RuntimeError::Ring(RingError::Other(carried(UNNAMEABLE)))
                .os_error()
                .map(|e| e.code()),
            Some(UNNAMEABLE)
        );
        // Through the box, which is the nesting most likely to be got wrong.
        assert_eq!(
            FileError::Driver(Box::new(RuntimeError::Other(carried(UNNAMEABLE))))
                .os_error()
                .map(|e| e.code()),
            Some(UNNAMEABLE)
        );
        assert_eq!(
            PipeError::Driver(Box::new(RuntimeError::Other(carried(UNNAMEABLE))))
                .os_error()
                .map(|e| e.code()),
            Some(UNNAMEABLE)
        );
        // Nesting that carries nothing must still answer `None` rather than
        // stopping at the outer variant and guessing.
        assert_eq!(FileError::Ring(RingError::QueueFull).os_error(), None);
        assert_eq!(
            FileError::Driver(Box::new(RuntimeError::RegistrationPending)).os_error(),
            None
        );
    }

    #[test]
    fn a_condition_this_crate_detected_itself_carries_nothing() {
        assert_eq!(RuntimeError::RegistrationPending.os_error(), None);
        assert_eq!(
            RuntimeError::ShutdownStalled { outstanding: 3 }.os_error(),
            None
        );
        assert_eq!(FileError::NotSeekable { file_type: 3 }.os_error(), None);
        assert_eq!(RingError::RingClosed.os_error(), None);
        assert_eq!(
            BuildError::UnsupportedVersion {
                requested: 2,
                max_supported: 1
            }
            .os_error(),
            None
        );
    }

    #[test]
    fn a_buffer_error_never_carries_a_platform_error() {
        assert_eq!(
            BufError::TooSmall {
                requested: 8,
                available: 4
            }
            .os_error(),
            None
        );
        assert_eq!(
            BufError::UninitializedWriteRange {
                requested: 8,
                initialized: 4
            }
            .os_error(),
            None
        );
        // And through a surface that wraps it.
        assert_eq!(
            FileError::Buf(BufError::TooSmall {
                requested: 8,
                available: 4
            })
            .os_error(),
            None
        );
    }
}
