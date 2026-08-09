//! The crate's one classification table, and the views over it.
//!
//! This module owns the single mapping from an `HRESULT` to a condition. It
//! exposes no error type of its own: each public surface has its own, naming
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
//! The point of the arrangement is that the classification happens **once**,
//! here, and each type is a *view* over the result rather than an independent
//! classifier. Two surfaces cannot disagree about what `ERROR_PIPE_BUSY` means,
//! because neither of them decides.
//!
//! A condition one surface names and another does not is demoted through the
//! module's canonical inverse, which attaches the code the table would have classified. So a
//! pipe condition that passes through a file error survives as
//! `Other(ERROR_BROKEN_PIPE)` and a pipe surface handed the same error recovers
//! `Broken`. Nothing is invented and nothing is lost.
//!
//! The operation builders in [`crate::io_ring::ops`] are the one surface that
//! does not consult the platform at all: their `build` methods return
//! [`MissingField`](crate::io_ring::ops::MissingField), whose failure set is
//! closed by construction. `docs/errors-and-the-funnel.md` records how this
//! design relates to the funnel argument that preceded it.
//!
//! # Platform availability
//!
//! This crate binds the Windows IoRing API through statically imported symbols
//! from `api-ms-win-core-ioring-l1-1-0.dll`. A host that lacks that API set
//! entirely cannot report an error here, because the process fails to load
//! before any code in this crate runs. [`crate::io_ring::BuildError::Unsupported`]
//! and [`crate::io_ring::BuildError::UnsupportedFeature`] therefore describe
//! hosts where the API set loads but does not provide what this crate needs.

/// The classification table and the views over it.
///
/// [Condition] and [classify] are private to this module and are not
/// re-exported. That is the point: a second match on a condition is not
/// forbidden elsewhere in the crate, it is **unrepresentable**, because no other
/// module can name the type. The only way out is [`view`], which classifies and
/// dispatches in one step.
///
/// This replaces a source-scanning test that tried to recognise every match on a
/// condition and decide whether it was armed. That test was defeated nine times
/// across three review rounds, always by a syntactic form it had not
/// anticipated. Privacy does not have to anticipate anything.
mod classification {
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
    /// Each view is a trait implementation with one method per variant and no
    /// default bodies ([`ConditionView`]), so adding a variant here fails to compile
    /// every view that has not been updated — with `E0046`, under plain
    /// `cargo build`, and not by way of any lint or test.
    /// [`view_convention`] records why the views are traits rather than matches, and
    /// that is not incidental: the match-and-lint design it replaced was defeated
    /// nine times.
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
    enum Condition {
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
    fn classify(hr: windows::core::HRESULT) -> Condition {
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

    /// One canonical code per condition the table names.
    ///
    /// [`classify`] is many-to-one only in its `Other` arm; each named condition
    /// has exactly one code today. These constants are that mapping read
    /// backwards, and `the_canonical_codes_round_trip` pins the property that
    /// makes them safe to use: viewing a canonical code routes to the method
    /// named for its condition.
    ///
    /// They exist so a conversion between two surface types can demote a
    /// condition the destination does not name **with a real code attached**,
    /// which is what lets a third surface recover it. Without them a conversion
    /// would have to either invent a code or drop the condition, and both defeat
    /// the design.
    pub(crate) mod canonical {
        use windows::Win32::Foundation::{
            ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_LISTENING,
            IORING_E_SUBMISSION_QUEUE_FULL,
        };
        use windows::core::HRESULT;

        /// The canonical code for a full submission queue.
        ///
        /// Unused by todays conversions -- no surface demotes `QueueFull`,
        /// because every surface that can see it names it. Kept so the inverse
        /// is complete: a partial inverse is the kind of thing someone later
        /// completes wrongly.
        #[allow(dead_code)]
        pub(crate) const QUEUE_FULL: HRESULT = IORING_E_SUBMISSION_QUEUE_FULL;

        /// The canonical code for "all pipe instances are busy".
        pub(crate) fn pipe_busy() -> HRESULT {
            ERROR_PIPE_BUSY.to_hresult()
        }

        /// The canonical code for "the peer closed its end".
        pub(crate) fn pipe_broken() -> HRESULT {
            ERROR_BROKEN_PIPE.to_hresult()
        }

        /// The canonical code for "no peer is connected".
        pub(crate) fn pipe_no_peer() -> HRESULT {
            ERROR_NO_DATA.to_hresult()
        }

        /// The canonical code for "listening, not yet connected".
        pub(crate) fn pipe_listening() -> HRESULT {
            ERROR_PIPE_LISTENING.to_hresult()
        }
    }

    /// How views over [`Condition`] are written, and why they are traits.
    ///
    /// A view is a trait with **one method per condition and no default bodies**,
    /// dispatched by [`view`], which holds the crate's only `match` on
    /// [`Condition`]. Adding a condition is caught in two steps, both hard `rustc`
    /// errors that no attribute can silence:
    ///
    /// 1. `E0004` — the dispatch `match` is no longer exhaustive. There is no way
    ///    to satisfy it without deciding what the new condition means.
    /// 2. `E0046` — once the trait gains the corresponding method, every view that
    ///    has not implemented it fails to compile.
    ///
    /// Neither step depends on Clippy running, on a lint being armed, or on a test.
    /// A view contains no `match`, so a wildcard is not *forbidden* in a view — it
    /// is unrepresentable.
    ///
    /// # Why not a hand-written match with a lint
    ///
    /// That was the previous design, and it was defeated **nine times across three
    /// review rounds** by code that compiled, was idiomatic in this crate, and left
    /// both Clippy and the policing test silent. The defeats were not nine bugs;
    /// they were nine samples from one unbounded class — *get a `match` past a text
    /// scanner*:
    ///
    /// - `let mapped = match condition { … }` rather than a line beginning `match `;
    /// - `#![allow(…)]` as the first line of the function body, which overrides an
    ///   outer `#[deny]` and which a scanner looking for `#[` does not see;
    /// - a second classification table keyed on `std::io::ErrorKind`, naming no
    ///   platform identifier at all — and diverging for real, since std maps
    ///   `ERROR_NO_DATA` to `BrokenPipe` where [`classify`] maps it to `PipeNoPeer`.
    ///
    /// Two facts about Clippy made that design weaker than it looked, and both were
    /// found by mutating a view and watching for a complaint that never came.
    /// Neither wildcard lint fires inside a `macro_rules!` expansion, so generating
    /// the views — the obvious way to guarantee the attribute was present —
    /// guaranteed only that it was present, not that it did anything. And
    /// `wildcard_enum_match_arm` alone does not fire when the wildcard covers
    /// exactly *one* remaining variant; that belongs to
    /// `match_wildcard_for_single_variants`, a different lint in a different group,
    /// and the single-variant case is the realistic mistake — a view naming every
    /// condition but the one its author forgot.
    ///
    /// Both lints are still denied, on the dispatch, where a wildcard is still
    /// expressible. They are no longer the guarantee; they are a second line.
    ///
    /// # The two residual hazards
    ///
    /// Both are bounded, both live at a named site, and both are pinned by
    /// `error_classification_policy.rs` with a mutation twin.
    ///
    /// **A default body** on a trait method restores exactly the silent demotion
    /// this design removes: the method stops being required by `E0046`, and a stale
    /// view compiles.
    ///
    /// **A dispatch arm routing a new condition to an existing method** — writing
    /// `Condition::New => V::other(hr)` — satisfies `E0004` without ever adding a
    /// method, so `E0046` never fires and every view silently demotes the new
    /// condition. This was found by mutation, against this design, after it was
    /// adopted; Clippy reports nothing, because the arm names its variant and no
    /// wildcard is involved. The guard is name-correspondence: arm *i* must call
    /// the method whose name is the snake_case of the variant it matches, which is
    /// total over the arms and fails on exactly this edit.
    mod view_convention {}

    /// A view over [`Condition`], as one method per condition.
    ///
    /// Implementing this is how an error type says which conditions it names. There
    /// are **no default bodies**: a type that has not accounted for every condition
    /// does not compile, with `E0046`, under plain `cargo build`. See
    /// [`view_convention`] for why this is a trait and not a `match`.
    ///
    /// Every method receives the originating `HRESULT` alongside its condition, so a
    /// view that cannot name a condition can still carry the code forward rather
    /// than fabricating a stand-in. [`view`] classifies and dispatches in one step,
    /// which is what guarantees the code and the condition it is paired with always
    /// describe the same failure.
    pub(crate) trait ConditionView: Sized {
        /// The submission or completion queue had no room.
        fn queue_full(hr: windows::core::HRESULT) -> Self;
        /// All pipe instances are busy.
        fn pipe_busy(hr: windows::core::HRESULT) -> Self;
        /// The pipe was broken by the peer.
        fn pipe_broken(hr: windows::core::HRESULT) -> Self;
        /// The pipe has no peer connected.
        fn pipe_no_peer(hr: windows::core::HRESULT) -> Self;
        /// The pipe is listening and not yet connected.
        fn pipe_listening(hr: windows::core::HRESULT) -> Self;
        /// A code the table does not classify, or a condition this view does not
        /// name. The `HRESULT` is passed through unchanged so it can be
        /// re-classified at a boundary that does name it.
        ///
        /// Named for its variant rather than for its meaning — `other`, not
        /// `unnamed` — so that every dispatch arm's method name is exactly the
        /// snake_case of the variant it matches, with no exceptions. The
        /// correspondence guard in `error_classification_policy.rs` needs no
        /// allowlist as a result, and an allowlist is a place for a future
        /// mis-routing to hide.
        fn other(hr: windows::core::HRESULT) -> Self;
    }

    /// Classifies `hr` and dispatches it to `V`'s view.
    ///
    /// This function holds the crate's only `match` on [`Condition`]. Because it
    /// classifies and dispatches together, no caller can pair a condition with an
    /// `HRESULT` that did not produce it.
    #[deny(
        clippy::wildcard_enum_match_arm,
        clippy::match_wildcard_for_single_variants
    )]
    pub(crate) fn view<V: ConditionView>(hr: windows::core::HRESULT) -> V {
        match classify(hr) {
            Condition::QueueFull => V::queue_full(hr),
            Condition::PipeBusy => V::pipe_busy(hr),
            Condition::PipeBroken => V::pipe_broken(hr),
            Condition::PipeNoPeer => V::pipe_no_peer(hr),
            Condition::PipeListening => V::pipe_listening(hr),
            Condition::Other(_) => V::other(hr),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A code the table does not name must arrive as `Other`, carrying that
        /// exact code.
        ///
        /// This is the property the `Other` variant exists for. Asserting the code
        /// survives, rather than merely that `Other` was produced, is what makes it
        /// a guarantee instead of a catch-all.
        #[test]
        fn an_unnamed_code_is_carried_verbatim() {
            let os = windows::core::Error::from(windows::Win32::Foundation::E_FAIL);
            let condition = classify(os.code());
            assert!(matches!(condition, Condition::Other(_)));

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
                (ERROR_PIPE_BUSY, Condition::PipeBusy),
                (ERROR_BROKEN_PIPE, Condition::PipeBroken),
                (ERROR_NO_DATA, Condition::PipeNoPeer),
                (ERROR_PIPE_LISTENING, Condition::PipeListening),
            ];

            let mut seen: Vec<String> = Vec::new();
            for (code, expected) in cases {
                let got = classify(code.to_hresult());
                assert_eq!(
                    std::mem::discriminant(&got),
                    std::mem::discriminant(&expected),
                    "{code:?} classified as {got:?}, expected {expected:?}"
                );
                // And the surface that names all four must render them apart.
                let rendered = view::<crate::pipe::error::Error>(code.to_hresult()).to_string();
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
        /// `classify` is the single table every surface reads, so widening it from
        /// exact codes to a facility or a range would silently reclassify file and
        /// socket errors that have nothing to do with pipes -- and now it would do
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
                let got = classify(code.to_hresult());
                assert!(
                    matches!(got, Condition::Other(_)),
                    "{code:?} must stay unnamed, got {got:?}"
                );
            }

            // Adjacent to the pipe codes on both sides, to catch a range match that
            // happened to bracket them.
            for code in [230_u32, 233, 534, 537] {
                let got = classify(WIN32_ERROR(code).to_hresult());
                assert!(
                    matches!(got, Condition::Other(_)),
                    "code {code} is not a pipe condition this crate maps, got {got:?}"
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

            let got = classify(ERROR_PIPE_CONNECTED.to_hresult());
            assert!(
                matches!(got, Condition::Other(_)),
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

        /// The canonical inverse must land back on the condition it names.
        ///
        /// `canonical` exists so a surface can demote a condition it cannot name
        /// while attaching a real code, letting a third surface recover it. That
        /// only works if the inverse is exact: a canonical code that classified as
        /// anything else would turn a demotion into a silent reclassification, and
        /// the recovery path would return the wrong condition rather than fail.
        /// SC-9: every completion-derived pipe condition survives a trip through a
        /// type that has no name for it.
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

            /// Whether this condition can arrive on a *completion*, and if so what the
            /// pipe surface must call it.
            ///
            /// Returning `None` is a claim, so each one carries its justification.
            fn expected(c: Condition) -> Option<(windows::core::HRESULT, &'static str)> {
                match c {
                    // Produced by the ring's submission path, not by a completion.
                    // Every surface that can see it names it, so it never demotes.
                    Condition::QueueFull => None,
                    Condition::PipeBusy => Some((canonical::pipe_busy(), "Busy")),
                    Condition::PipeBroken => Some((canonical::pipe_broken(), "Broken")),
                    Condition::PipeNoPeer => Some((canonical::pipe_no_peer(), "NoPeer")),
                    Condition::PipeListening => Some((canonical::pipe_listening(), "Listening")),
                    // Not a condition but the absence of one. It has no canonical code
                    // by construction, and `classify` is deterministic, so it
                    // reclassifies to itself forever.
                    Condition::Other(_) => None,
                }
            }

            let all = [
                Condition::QueueFull,
                Condition::PipeBusy,
                Condition::PipeBroken,
                Condition::PipeNoPeer,
                Condition::PipeListening,
                Condition::Other(windows::core::HRESULT(0x1234)),
            ];

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

            for (hr, name) in [
                (canonical::pipe_busy(), "PipeBusy"),
                (canonical::pipe_broken(), "PipeBroken"),
                (canonical::pipe_no_peer(), "PipeNoPeer"),
                (canonical::pipe_listening(), "PipeListening"),
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

        #[test]
        fn the_canonical_codes_round_trip() {
            let cases: [(windows::core::HRESULT, Condition); 5] = [
                (canonical::QUEUE_FULL, Condition::QueueFull),
                (canonical::pipe_busy(), Condition::PipeBusy),
                (canonical::pipe_broken(), Condition::PipeBroken),
                (canonical::pipe_no_peer(), Condition::PipeNoPeer),
                (canonical::pipe_listening(), Condition::PipeListening),
            ];

            for (code, expected) in cases {
                let got = classify(code);
                assert_eq!(
                    std::mem::discriminant(&got),
                    std::mem::discriminant(&expected),
                    "canonical code {code:?} classifies as {got:?}, not {expected:?}; \
                     the inverse and the table have diverged"
                );
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
                    let e: $ty = view::<$ty>(hr);
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
                let demoted = view::<crate::file::error::Error>(hr);
                let carried = match demoted {
                    crate::file::error::Error::Other(ref e) => e.code(),
                    ref other => {
                        panic!("file::Error named a pipe condition it should not: {other:?}")
                    }
                };
                assert_eq!(carried, hr, "file::Error substituted a code for {win32:?}");

                // And the code it carried re-classifies to the condition the pipe
                // surface names, which is the round trip itself.
                let recovered = view::<crate::pipe::error::Error>(carried);
                assert!(
                    !matches!(recovered, crate::pipe::error::Error::Other(_)),
                    "re-classifying {win32:?} at the pipe surface did not recover a named condition"
                );
            }
        }
    }
}

pub(crate) use classification::{ConditionView, canonical, view};
