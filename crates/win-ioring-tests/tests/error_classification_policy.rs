//! FR-1, FR-10, FR-11, FR-16: error classification has exactly one home.
//!
//! The crate's own voice, in `pipe/client.rs`, warns that `ERROR_PIPE_BUSY`
//! from a failed open and `ERROR_PIPE_BUSY` from a completion must produce the
//! same condition, and that "two independent match arms are exactly how that
//! stops being true after someone edits one of them". Per-API error types make
//! that hazard sharper, not softer: five error types that each classify codes
//! independently would diverge the first time one of them was edited.
//!
//! The design's answer is that `error::classify` is the only function mapping
//! an `HRESULT` to a condition, and every error type is an exhaustive view over
//! its result. Those are claims about source text, so they are checked here
//! against source text. `dependency_policy.rs` is the nearest precedent for a
//! test that reads the repository rather than running it — but it parses
//! manifests, and this reads `.rs` files, so the machinery is new.
//!
//! # Three guards, deliberately independent
//!
//! An earlier version of this file had one guard per claim, and a review broke
//! four of five with code that compiles and is idiomatic in this crate: a
//! second table written with `HRESULT::from_win32` rather than `to_hresult()`;
//! a view spelled `impl From<Condition>` rather than `fn from_condition`; a
//! wildcard hidden behind an `#[allow]` that sat *below* the `#[deny]`. Each
//! bypass was narrow, and each defeated the whole file, because the guards
//! shared assumptions.
//!
//! So there are now three, and they fail independently:
//!
//! 1. [`the_table_constants_have_one_home`] — the five Win32 constants that
//!    *are* the table may appear only in `classify`. A second table has to name
//!    them, whatever syntax it uses to compare them.
//! 2. [`error_classification_has_one_home`] — no line outside `classify` may
//!    decide anything from a platform error code, with the exceptions
//!    enumerated and justified here.
//! 3. [`every_match_on_a_condition_is_an_armed_view`] — every `match` on a
//!    `Condition`, wherever it lives and whatever its function is called, must
//!    arm both wildcard lints and must not use a wildcard.
//!
//! # What this cannot catch
//!
//! Stated rather than glossed, because a policy test that implies more coverage
//! than it has is worse than none. A second table built from bare numeric
//! literals — `&[(231u32, Error::PipeBusy), …]` — names no constant and makes
//! no comparison this file recognises. Guard 3 still forces any *view* to be
//! exhaustive and armed, so such a table could not silently disagree with a
//! view; but it could disagree with `classify`. That residue is why the
//! allowlist below carries reasons rather than just names: the reasons are what
//! a reader checks when this file says a change is fine.

use std::path::{Path, PathBuf};

/// The source root of the crate under test.
const SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../win-ioring/src");

/// The Win32 constants that constitute the classification table.
///
/// These five names are the table's content. Any production mention outside
/// `classify` is a second table by definition, regardless of how it compares
/// them — which is what makes this the sharpest of the three guards.
const TABLE_CONSTANTS: &[&str] = &[
    "ERROR_PIPE_BUSY",
    "ERROR_BROKEN_PIPE",
    "ERROR_NO_DATA",
    "ERROR_PIPE_LISTENING",
    "IORING_E_SUBMISSION_QUEUE_FULL",
];

/// A production site that mentions a platform error code without classifying
/// one.
struct Allowed {
    /// Path relative to the crate source root.
    file: &'static str,
    /// The line, as it appears in the source with whitespace trimmed.
    ///
    /// Matching on the text rather than on a line number is deliberate. Line
    /// numbers drift with unrelated edits, and a drifted allowlist silently
    /// permits whatever now occupies the line. If the line itself is rewritten,
    /// its justification deserves re-reading, so this test failing is correct.
    line: &'static str,
    /// How many times this exact line appears in the policed region of the file.
    ///
    /// Text matching alone cannot tell two identical lines apart, and this
    /// crate has two: the early-connect check appears in both the blocking and
    /// the polled accept paths. Without a count, a *third* copy would match an
    /// existing entry and join the exception silently — which is precisely the
    /// failure an allowlist exists to prevent.
    sites: usize,
    /// Why this site is not classification.
    #[allow(dead_code, reason = "read by a human when this test fails")]
    why: &'static str,
}

/// Every production line that decides something from a platform error code
/// outside `classify`.
///
/// Two kinds appear here, and they are not the same kind of exception.
///
/// The first is *context-dependent* classification, which genuinely maps a code
/// to a condition but only along one call path. `E_NOTIMPL` means "this host
/// cannot provide a ring" while a ring is being created and means nothing in
/// particular anywhere else, so folding it into the shared table would
/// reclassify every unrelated `E_NOTIMPL` in the crate. It stays out of the
/// table for the same reason the table matches exact codes rather than ranges.
///
/// The second is *status checks*: comparisons that drive a synchronous state
/// machine and never produce an error variant. `ERROR_IO_PENDING` means the
/// overlapped operation started normally, and `ERROR_PIPE_CONNECTED` means a
/// client arrived early — a success. These do not classify anything, and a
/// classifier is the wrong shape for them.
const ALLOWED: &[Allowed] = &[
    Allowed {
        file: "io_ring/error.rs",
        line: "if err.code() == E_NOTIMPL {",
        sites: 1,
        why: "context-dependent: E_NOTIMPL denotes an unusable host only while \
              creating a ring. Classifying it in the shared table would \
              reclassify unrelated E_NOTIMPL results from every other call site. \
              It moved here from `error.rs` when ring construction got its own \
              `BuildError`, which is where the condition had always belonged: \
              it is reachable only while building.",
    },
    Allowed {
        file: "pipe/server.rs",
        line: "Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {",
        sites: 1,
        why: "status check: the overlapped connect started normally. Not a failure.",
    },
    Allowed {
        file: "pipe/server.rs",
        line: "Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => {",
        sites: 2,
        why: "status check: a client connected before the accept was issued, \
              which is a success for the accept. The shared table deliberately \
              refuses to name this code for the same reason.",
    },
    Allowed {
        file: "pipe/server.rs",
        line: "Err(e) if e.code() == ERROR_IO_INCOMPLETE.to_hresult() => Ok(false),",
        sites: 1,
        why: "status check: the overlapped connect has not finished yet.",
    },
    Allowed {
        file: "pipe/server.rs",
        line: "Err(e) if e.code() == ERROR_NOT_FOUND.to_hresult() => CancelOutcome::NotFound,",
        sites: 1,
        why: "status check: there was nothing to cancel, which the caller \
              distinguishes from a cancellation that failed.",
    },
    Allowed {
        file: "pipe/server.rs",
        line: "internal != STATUS_PENDING_INTERNAL",
        sites: 1,
        why: "not an HRESULT at all: the NT status word written into an \
              OVERLAPPED by the kernel, read to decide whether the operation \
              has completed.",
    },
    Allowed {
        file: "pipe/server.rs",
        line: "const STATUS_PENDING_INTERNAL: usize = 0x0000_0103;",
        sites: 1,
        why: "the declaration of the NT status word above. Flagged because \
              declaring a constant is one of the ways a table gets written \
              without a comparison, which is a rule worth keeping even when it \
              costs an entry for a constant that is not an HRESULT.",
    },
    Allowed {
        file: "pipe/client.rs",
        line: "Some(code) => Error::from(windows::core::HRESULT::from_win32(code as u32)),",
        sites: 1,
        why: "the opposite of a second table: it builds an HRESULT from a raw \
              OS error and hands it to `From`, which calls `view`, which calls \
              `classify`. Flagged only because it is a match arm naming HRESULT, \
              and the detector is deliberately broad. The neighbouring E_FAIL, \
              substituted when the OS error is absent, remains the crate's one \
              fabricated code; it is justified at the call site and is not \
              settled by this entry.",
    },
];

/// Every `.rs` file under the crate's source root, paired with its relative
/// path.
fn all_files() -> Vec<(String, PathBuf)> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, PathBuf)>) {
        for entry in std::fs::read_dir(dir).expect("source root is readable") {
            let path = entry.expect("directory entry is readable").path();
            if path.is_dir() {
                walk(&path, root, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let rel = path
                    .strip_prefix(root)
                    .expect("walked path is under the root")
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((rel, path));
            }
        }
    }

    let root = PathBuf::from(SRC);
    let mut out = Vec::new();
    walk(&root, &root, &mut out);
    assert!(!out.is_empty(), "no source files found under {SRC}");
    out.sort();
    out
}

/// Reads every non-test `.rs` file under the crate's source root.
///
/// Test modules are excluded: their comparisons exist to probe the classifier
/// rather than to classify, and a rule that forbade them would forbid checking
/// the very property this file protects.
///
/// A test module declared as `#[cfg(test)] mod tests;` lives in its own file,
/// found by reading the declaration rather than by guessing from the file name.
/// Rust resolves such a declaration to `name.rs` beside a `mod.rs`/`lib.rs`, but
/// to `<declaring-stem>/name.rs` beside any other file, and both are excluded —
/// getting this wrong in the second direction would scan a test file and
/// exclude a production one.
fn source_files() -> Vec<(String, PathBuf)> {
    let all = all_files();

    let mut test_files = Vec::new();
    for (rel, path) in &all {
        let text = std::fs::read_to_string(path).expect("source file is readable");
        let (dir, stem) = match rel.rsplit_once('/') {
            Some((dir, file)) => (format!("{dir}/"), file.trim_end_matches(".rs").to_string()),
            None => (String::new(), rel.trim_end_matches(".rs").to_string()),
        };
        let mut pending = false;
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed == "#[cfg(test)]" {
                pending = true;
                continue;
            }
            if pending {
                pending = false;
                if let Some(name) = trimmed
                    .strip_prefix("mod ")
                    .and_then(|rest| rest.strip_suffix(';'))
                {
                    if stem == "mod" || stem == "lib" {
                        test_files.push(format!("{dir}{name}.rs"));
                    } else {
                        test_files.push(format!("{dir}{stem}/{name}.rs"));
                    }
                }
            }
        }
    }

    all.into_iter()
        .filter(|(rel, _)| !test_files.contains(rel))
        .collect()
}

/// The indentation width of a line, in leading spaces.
fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Returns the lines of `text` that the policy applies to: production lines,
/// outside `#[cfg(test)]` modules and outside the body of `classify`.
///
/// Both exclusions are anchored on indentation rather than on a brace at column
/// zero. The earlier column-zero rule was correct only while `classify` was a
/// free function; if it became a method it would have swallowed the rest of the
/// enclosing `impl` block silently, because an over-long skip looks exactly like
/// a short one.
fn policed_lines(text: &str) -> Vec<(usize, &str)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut index = 0usize;

    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim();

        let starts_classify =
            trimmed.starts_with("pub(crate) fn classify(") || trimmed.starts_with("fn classify(");
        // The table's inverse. It has to name the same five constants -- that
        // is what makes it an inverse -- so it cannot be policed by a rule that
        // counts mentions. What keeps it honest is
        // `the_canonical_codes_round_trip`, which classifies every canonical
        // code and asserts it lands on the condition it names. Divergence is
        // caught by evidence rather than forbidden by spelling.
        let starts_canonical = trimmed.starts_with("pub(crate) mod canonical {")
            || trimmed.starts_with("mod canonical {");
        let starts_test_mod = trimmed == "#[cfg(test)]"
            && lines
                .get(index + 1)
                .is_some_and(|next| next.trim().starts_with("mod ") && !next.trim().ends_with(';'));

        if starts_classify || starts_canonical || starts_test_mod {
            let open_indent = indent(if starts_classify || starts_canonical {
                line
            } else {
                lines[index + 1]
            });
            index += 1;
            // Skip to the line that closes the item, identified by a brace at
            // the same indentation as the line that opened it.
            while index < lines.len() {
                let candidate = lines[index];
                index += 1;
                if candidate.trim() == "}" && indent(candidate) == open_indent {
                    break;
                }
            }
            continue;
        }

        out.push((index + 1, line));
        index += 1;
    }

    out
}

/// Whether a line renames a platform error *code* through an import.
///
/// Type names (`HRESULT`, `WIN32_ERROR`) are excluded: aliasing a type launders
/// nothing, while aliasing `ERROR_PIPE_BUSY` makes every later use of it
/// invisible to both text guards.
fn renames_a_platform_code(line: &str) -> bool {
    line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|word| {
            word.starts_with("ERROR_")
                || word.starts_with("IORING_E_")
                || word.starts_with("STATUS_")
                || (word.starts_with("E_") && word.len() > 2)
        })
}

/// Whether a line decides something from a platform error code.
///
/// Deliberately broader than "contains `==`". The forms that matter are a
/// comparison, a match arm, and a constant declaration typed as a platform
/// error — the last because declaring a constant is how a match on error codes
/// is written without any comparison operator at all, which is how the first
/// version of this detector was defeated.
fn decides_from_an_error_code(line: &str) -> bool {
    /// Identifiers that denote a platform error code.
    fn names_a_code(line: &str) -> bool {
        line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .any(|word| {
                word.starts_with("ERROR_")
                    || word.starts_with("IORING_E_")
                    || word.starts_with("STATUS_")
                    || word == "HRESULT"
                    || word == "WIN32_ERROR"
                    || (word.starts_with("E_") && word.len() > 2)
                    // std::io::ErrorKind is a classification of platform error
                    // codes wearing different clothes, and it names no platform
                    // identifier at all — which is exactly how a reviewer got a
                    // second table past the identifier check above. It also
                    // genuinely diverges: std maps ERROR_NO_DATA to BrokenPipe,
                    // where classify maps it to PipeNoPeer. Two callers asking
                    // the same question would get two answers.
                    || word == "ErrorKind"
            })
            || line.contains("0x8007")
            || line.contains("0x8000")
    }

    if !names_a_code(line) {
        return false;
    }

    let trimmed = line.trim();
    let compares = line.contains("==") || line.contains("!=");
    let match_arm = line.contains("=>");
    let declares_const = trimmed.starts_with("const ") || trimmed.starts_with("static ");

    compares || match_arm || declares_const
}

/// FR-1: the constants that make up the table appear only in `classify`.
///
/// This is the sharpest of the three guards, because it does not depend on how
/// a second table is written. Whatever syntax it uses — `to_hresult()`,
/// `HRESULT::from_win32`, a `const` pattern — a table that disagrees with
/// `classify` about `ERROR_PIPE_BUSY` has to name `ERROR_PIPE_BUSY`.
///
/// It therefore reads `use` lines too, which it once skipped. An import may
/// rename a constant (`use ... ERROR_PIPE_BUSY as PIPE_BUSY_CODE;`) and every
/// later mention is then a name this guard does not know, on a line naming no
/// platform identifier at all — a second table assembled entirely out of lines
/// both guards had agreed not to read. Reading them costs nothing: the only
/// `use` naming a table constant is inside `classify`, which `policed_lines`
/// already skips.
#[test]
fn the_table_constants_have_one_home() {
    let mut offenders = Vec::new();

    for (rel, path) in source_files() {
        let text = std::fs::read_to_string(&path).expect("source file is readable");
        for (number, line) in policed_lines(&text) {
            let trimmed = line.trim();
            if trimmed.starts_with("//") {
                continue;
            }
            for constant in TABLE_CONSTANTS {
                if line.contains(constant) {
                    offenders.push(format!("{rel}:{number}: {trimmed}"));
                    break;
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "these production sites name a classification-table constant outside \
         `error::classify`:\n{}\n\nThese five constants are the table. A second \
         place that names one is a second table, and two tables are how \
         ERROR_PIPE_BUSY from an open and from a completion stop meaning the \
         same thing. Route the code through `classify` instead.",
        offenders.join("\n")
    );
}

/// FR-1, SC-23: nothing outside `classify` decides from a platform error code,
/// except the sites enumerated and justified in `ALLOWED`.
#[test]
fn error_classification_has_one_home() {
    let mut unexplained = Vec::new();

    for (rel, path) in source_files() {
        let text = std::fs::read_to_string(&path).expect("source file is readable");
        for (number, line) in policed_lines(&text) {
            let trimmed = line.trim();
            if trimmed.starts_with("//") {
                continue;
            }
            if trimmed.starts_with("use ") {
                // An import is not a decision, but a *renaming* import launders
                // a platform identifier into a name no detector recognises, and
                // every use of it afterwards reads as ordinary code. The rename
                // is the last point at which the connection is visible.
                if trimmed.contains(" as ") && renames_a_platform_code(trimmed) {
                    unexplained.push(format!("{rel}:{number}: {trimmed}"));
                }
                continue;
            }
            if !decides_from_an_error_code(line) {
                continue;
            }
            let allowed = ALLOWED.iter().any(|a| a.file == rel && a.line == trimmed);
            if !allowed {
                unexplained.push(format!("{rel}:{number}: {trimmed}"));
            }
        }
    }

    assert!(
        unexplained.is_empty(),
        "these sites decide something from a platform error code outside \
         `error::classify`:\n{}\n\n`classify` is the crate's single \
         classification table, and a second one is how two surfaces stop \
         agreeing about the same code. If the site genuinely does not classify \
         — a status check that drives a state machine, or a mapping confined to \
         one call path — add it to ALLOWED in this file with the reason. Do not \
         narrow the detector.",
        unexplained.join("\n")
    );
}

/// Every allowlisted site must still exist, and exactly as often as recorded.
#[test]
fn every_allowlisted_site_still_exists_exactly_as_often_as_recorded() {
    let files = source_files();
    for entry in ALLOWED {
        let (_, path) = files
            .iter()
            .find(|(rel, _)| rel == entry.file)
            .unwrap_or_else(|| panic!("allowlist names a file that is gone: {}", entry.file));
        let text = std::fs::read_to_string(path).expect("source file is readable");
        // Counted over the policed region, so that a test which happens to
        // reproduce one of these lines does not fail this check. The two tests
        // must agree on scope or they will contradict each other.
        let found = policed_lines(&text)
            .into_iter()
            .filter(|(_, line)| line.trim() == entry.line)
            .count();
        assert_eq!(
            found, entry.sites,
            "{} contains {found} copies of this line in production code, but \
             the allowlist records {}:\n  {}\n\nA new copy is not covered by an \
             existing justification just because it is spelled the same way. \
             Read the reason, decide whether it applies to the new site, and \
             update the count deliberately.",
            entry.file, entry.sites, entry.line
        );
    }
}

/// The derivation behind the design's headline cost: `Condition` adds no public
/// variant slots.
///
/// The per-API split was costed at 46 to 48 public variants across six types.
/// That figure counts only types callers can name. `Condition` is `pub(crate)`,
/// appears in no public signature, and is not re-exported, so it contributes
/// zero. Checked rather than asserted, because it is a claim about the design's
/// cost and an unchecked number in this work has a poor record.
#[test]
fn condition_is_not_part_of_the_public_api() {
    let error_rs =
        std::fs::read_to_string(PathBuf::from(SRC).join("error.rs")).expect("error.rs is readable");
    assert!(
        error_rs.contains("\nmod classification {"),
        "the classification module is no longer private. Its privacy is what \
         makes a second match on a condition unrepresentable rather than merely \
         discouraged: no other module can name the type."
    );
    assert!(
        error_rs.contains("\n    enum Condition {"),
        "Condition is no longer private to the classification module; the public \
         variant count in the design's costing assumed it contributed nothing, \
         and a nameable Condition can be matched a second time"
    );
    assert!(
        error_rs.contains("\n    fn classify("),
        "classify is no longer private to the classification module"
    );
    assert!(
        !error_rs.contains("pub use classification"),
        "the classification module's contents are re-exported, which undoes the \
         privacy the guard above depends on"
    );

    for (rel, path) in all_files() {
        let text = std::fs::read_to_string(&path).expect("source file is readable");
        for (index, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if !trimmed.contains("Condition") && !trimmed.contains("classify") {
                continue;
            }
            // `pub` followed by any function form: `pub fn`, `pub const fn`,
            // `pub unsafe fn`, `pub async fn`. Checking only `pub fn` left the
            // others to rustc's private-interface lint, which is warn-by-default
            // and so not the guarantee this test reads as.
            let public_fn = trimmed.starts_with("pub ") && trimmed.contains("fn ");
            let exported = trimmed.starts_with("pub use");
            assert!(
                !(public_fn || exported),
                "{rel}:{}: Condition or classify reached the public API: {trimmed}",
                index + 1
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The mechanism guard: E0046 and its two residual hazards.
//
// Adding a variant to a source enum is caught by the compiler in two steps —
// E0004 on the dispatch, then E0046 on every view that has not implemented the
// new method. Both are hard `rustc` errors and neither can be silenced by an
// attribute, so nothing here needs to re-check them; `compile_fail` twins in
// `error_view_exhaustiveness.rs` pin that they fire.
//
// What the compiler cannot see are the two edits that stop it firing at all.
// This is where those are pinned.
// ---------------------------------------------------------------------------

/// How many view traits the crate is known to have.
///
/// # Why one, when the spec anticipated four
///
/// Spec §3.4 lists six conversion rows and estimated a trait for each family.
/// Implemented, only one row needs a trait, and the derivation is worth keeping
/// because the number looks too low:
///
/// - **Rows 1–3** (`io_ring::Error`, `buf::Error`, `ops::MissingField` into the
///   surface types) carry the source value *whole* — `Error::Ring(value)` and
///   friends. Adding a variant to the source needs no edit at any destination,
///   so there is no per-variant mapping that can go stale and nothing for a
///   trait to guard. A trait here would be machinery protecting nothing.
/// - **Rows 4–5** (`runtime::Error` into the two boundary types) *are*
///   per-variant, and are guarded by two compiler errors rather than a trait: a
///   new variant is E0004 because every variant is named, and the wildcard that
///   would silence E0004 is itself an error under
///   `#[deny(clippy::wildcard_enum_match_arm)]` at both impls. Those are the
///   same two doors E0046 shuts, for one attribute instead of a seventeen-method
///   trait implemented twice.
/// - **Row 6** (a condition into every view) is the one that needs
///   `ConditionView`, because it is the only row where several destinations map
///   the *same* source variant independently — which is the divergence the whole
///   design exists to prevent.
///
/// So the count is one because only one row has independent per-variant mapping
/// across multiple destinations. If a second such row ever appears, this
/// constant must rise with it.
///
/// Row 3 has a narrower hazard of its own — `MissingField` is a struct, so
/// *widening* it would silently drop the new field at three destinations. That
/// is pinned by destructuring rather than by a trait: the conversions bind
/// `let MissingField { field } = value`, which is E0027 the moment a field is
/// added. Verified by adding one.
///
/// Discovered traits are counted against this rather than being looked up by
/// name, so a fifth trait cannot arrive un-covered: it either fails this
/// assertion or is deliberately recorded here. A hand-written list of trait
/// names would have the opposite property — the guard would keep passing while
/// covering less than the mechanism it exists to protect.
const VIEW_TRAITS: usize = 1;

/// How many dispatch functions the crate is known to have — one per view trait.
const DISPATCH_FUNCTIONS: usize = 1;

/// `QueueFull` -> `queue_full`.
fn snake_case(variant: &str) -> String {
    let mut out = String::new();
    for (position, character) in variant.char_indices() {
        if character.is_uppercase() {
            if position != 0 {
                out.push('_');
            }
            out.extend(character.to_lowercase());
        } else {
            out.push(character);
        }
    }
    out
}

/// The lines of the body of the item declared at `start`, by brace depth.
fn item_body<'a>(lines: &[&'a str], start: usize) -> Vec<&'a str> {
    let mut depth = 0usize;
    let mut out = Vec::new();
    for line in &lines[start..] {
        let opened = line.matches('{').count();
        let closed = line.matches('}').count();
        if depth > 0 {
            out.push(*line);
        }
        depth = depth + opened - closed.min(depth + opened);
        if depth == 0 && (opened > 0 || !out.is_empty()) {
            break;
        }
    }
    out.pop();
    out
}

/// No method of any view trait may have a default body.
///
/// A default body is the first of two ways to disarm `E0046`. It makes the
/// method optional, so a view that has never heard of a condition compiles and
/// silently answers for it. Nothing else in the mechanism notices: the trait
/// still exists, the dispatch is still exhaustive, and every lint is still
/// armed.
#[test]
fn no_view_trait_method_has_a_default_body() {
    let mut traits_found = 0usize;
    let mut offenders = Vec::new();

    for (rel, path) in source_files() {
        let text = std::fs::read_to_string(&path).expect("source file is readable");
        let lines: Vec<&str> = text.lines().collect();

        for (index, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            let Some(rest) = trimmed.strip_prefix("pub(crate) trait ") else {
                continue;
            };
            let name = rest
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .next()
                .unwrap_or("");
            if !name.ends_with("View") {
                continue;
            }
            traits_found += 1;

            for (offset, method) in item_body(&lines, index).iter().enumerate() {
                let method = method.trim();
                if !method.starts_with("fn ") {
                    continue;
                }
                // A signature may wrap across lines, so read forward to
                // whichever of `;` or `{` terminates it. Judging the first line
                // alone would report every wrapped signature as a default body,
                // and the fix for that false positive would be to relax the
                // check — which is how a guard stops guarding.
                let body = item_body(&lines, index);
                let mut declaration = String::new();
                for line in &body[offset..] {
                    declaration.push_str(line.trim());
                    if line.trim_end().ends_with(';') || line.contains('{') {
                        break;
                    }
                    declaration.push(' ');
                }
                if !declaration.ends_with(';') {
                    offenders.push(format!("{rel}: trait {name}: {declaration}"));
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "a view trait method has a default body, which makes it optional and \
         lets a stale view compile — the exact silent demotion E0046 exists to \
         prevent:\n  {}",
        offenders.join("\n  ")
    );

    assert_eq!(
        traits_found, VIEW_TRAITS,
        "expected {VIEW_TRAITS} view trait(s), found {traits_found}. If you \
         added one, raise VIEW_TRAITS; if you removed one, lower it. If you \
         changed neither, this guard has stopped recognising a trait that still \
         exists, and is no longer covering the whole mechanism."
    );
}

/// Every dispatch arm must call the method named for the variant it matches,
/// and every trait method must be called by exactly one arm.
///
/// This is the second way to disarm `E0046`, and it was found by mutating this
/// design after adopting it. Writing
///
/// ```ignore
/// Condition::NewlyAdded => V::other(hr),
/// ```
///
/// satisfies E0004 without adding a trait method, so E0046 never fires and
/// every view in the crate silently demotes the new condition. Clippy reports
/// nothing, because the arm names its variant and no wildcard is involved.
///
/// The bijection is what catches it: `other` would then be called twice. Name
/// correspondence alone would also catch it, but the bijection additionally
/// catches a method that no arm reaches, which is the same fault seen from the
/// other end.
#[test]
fn every_dispatch_arm_calls_the_method_named_for_its_variant() {
    let mut dispatches = 0usize;

    for (rel, path) in source_files() {
        let text = std::fs::read_to_string(&path).expect("source file is readable");
        let lines: Vec<&str> = text.lines().collect();

        for (index, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            if !trimmed.contains("fn ") || !trimmed.contains("View>") {
                continue;
            }
            let Some(bound) = trimmed.split_once("<V: ") else {
                continue;
            };
            let trait_name = bound.1.split('>').next().unwrap_or("");
            if !trait_name.ends_with("View") {
                continue;
            }
            dispatches += 1;

            let mut called = Vec::new();
            for arm in item_body(&lines, index) {
                let arm = arm.trim();
                let Some((pattern, action)) = arm.split_once("=>") else {
                    continue;
                };
                let Some((_, variant)) = pattern.trim().rsplit_once("::") else {
                    continue;
                };
                let variant = variant
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .next()
                    .unwrap_or("");
                let method = action
                    .trim()
                    .strip_prefix("V::")
                    .and_then(|rest| rest.split('(').next())
                    .unwrap_or("");

                assert_eq!(
                    method,
                    snake_case(variant),
                    "in {rel}, the dispatch for {trait_name} routes {variant} to \
                     `{method}`, which is not the method named for it. Routing a \
                     variant to another variant's method satisfies E0004 without \
                     adding a trait method, so E0046 never fires and every view \
                     silently demotes it."
                );
                called.push(method.to_string());
            }

            let mut unique = called.clone();
            unique.sort();
            unique.dedup();
            assert_eq!(
                called.len(),
                unique.len(),
                "in {rel}, the dispatch for {trait_name} calls a method from more \
                 than one arm, so two conditions are indistinguishable to every \
                 view"
            );

            let declaration = lines
                .iter()
                .position(|l| {
                    l.trim_start()
                        .starts_with(&format!("pub(crate) trait {trait_name}"))
                })
                .expect("the dispatch's trait is declared in the same file");
            let methods = item_body(&lines, declaration)
                .iter()
                .filter(|l| l.trim().starts_with("fn "))
                .count();
            assert_eq!(
                called.len(),
                methods,
                "in {rel}, {trait_name} has {methods} method(s) but its dispatch \
                 reaches {} of them; a method no arm calls is a condition no \
                 view will ever be asked about",
                called.len()
            );
        }
    }

    assert_eq!(
        dispatches, DISPATCH_FUNCTIONS,
        "expected {DISPATCH_FUNCTIONS} dispatch function(s), found {dispatches}"
    );
}

// ---------------------------------------------------------------------------
// E0046 itself.
//
// The two tests above pin the crate-specific preconditions — no default bodies,
// and a dispatch that reaches each method exactly once. With those held, the
// remaining claim is a property of `rustc`: a trait method with no default body
// is required, and an impl that omits it does not compile.
//
// That claim cannot be pinned against the real trait. `ConditionView` is private
// to `error::classification`, so no doctest and no external test crate can name
// it, and making it public to test it would hand the crate a public trait with
// one method per condition — which is precisely the public cost the design
// exists to avoid. Testing it in a replica is the honest alternative, and the
// scope of what that covers is stated here rather than implied.
// ---------------------------------------------------------------------------

/// The shape of a view trait, reduced to what `E0046` acts on.
const REPLICA: &str = r#"
trait ConditionView: Sized {
    fn pipe_broken(hr: u32) -> Self;
    fn other(hr: u32) -> Self;
    // ADDED: a new condition has just been named.
    fn pipe_listening(hr: u32) -> Self;
}

enum FileError { Broken(u32), Other(u32), Listening(u32) }

impl ConditionView for FileError {
    fn pipe_broken(hr: u32) -> Self { FileError::Broken(hr) }
    fn other(hr: u32) -> Self { FileError::Other(hr) }
"#;

/// The view has **not** been updated for the new method.
const STALE: &str = "}\n";

/// The view **has** been updated. Identical in every other respect.
const UPDATED: &str = "    fn pipe_listening(hr: u32) -> Self { FileError::Listening(hr) }\n}\n";

/// Adding a method to a view trait fails to compile every view that has not
/// implemented it, and the failure is `E0046`.
///
/// The two halves share `REPLICA` verbatim and differ only in whether the impl
/// provides the new method. That is what makes the failing half non-vacuous: if
/// the shared setup ever stopped compiling for an unrelated reason — a rename,
/// a syntax error, an edition change — the *passing* half would fail too, and
/// this test would report that rather than quietly reporting success. A
/// `compile_fail` doctest without such a sibling reports `ok` when it fails for
/// the wrong reason, and this crate has shipped one that did exactly that.
///
/// The error code is asserted for the same reason: "it did not compile" is not
/// evidence that the mechanism fired.
#[test]
fn omitting_a_view_trait_method_is_e0046() {
    let dir = std::env::temp_dir().join(format!("win-ioring-e0046-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch directory is creatable");

    let compile = |name: &str, tail: &str| {
        let source = dir.join(format!("{name}.rs"));
        std::fs::write(&source, format!("{REPLICA}{tail}")).expect("snippet is writable");
        std::process::Command::new("rustc")
            .args(["--crate-type", "lib", "--edition", "2021", "--out-dir"])
            .arg(&dir)
            .arg(&source)
            .output()
            .expect("rustc runs")
    };

    let updated = compile("updated", UPDATED);
    assert!(
        updated.status.success(),
        "the sibling that implements the new method must compile, or the failing \
         half below proves nothing:\n{}",
        String::from_utf8_lossy(&updated.stderr)
    );

    let stale = compile("stale", STALE);
    assert!(
        !stale.status.success(),
        "a view that has not implemented the new method compiled, so E0046 is \
         not protecting the mechanism"
    );
    let stderr = String::from_utf8_lossy(&stale.stderr);
    assert!(
        stderr.contains("E0046"),
        "the stale view failed to compile, but not with E0046 — so this test is \
         passing for the wrong reason, which is the failure mode it exists to \
         rule out:\n{stderr}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
