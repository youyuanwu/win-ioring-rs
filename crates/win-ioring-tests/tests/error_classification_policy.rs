//! FR-1, FR-10, FR-11, FR-16: error classification has exactly one home per code.
//!
//! The crate's own voice, in `pipe/client.rs`, warns that `ERROR_PIPE_BUSY`
//! from a failed open and `ERROR_PIPE_BUSY` from a completion must produce the
//! same condition, and that "two independent match arms are exactly how that
//! stops being true after someone edits one of them". Per-API error types make
//! that hazard sharper, not softer: six error types that each classify codes
//! independently would diverge the first time one of them was edited.
//!
//! The design's answer is that `error::pipe_table` and `error::ring_table` are
//! the only places a code is compared, that their code sets are **disjoint**, and
//! that every other type reaches a condition by delegating rather than by
//! comparing. Those are claims about source text, so they are checked here
//! against source text. `dependency_policy.rs` is the nearest precedent for a
//! test that reads the repository rather than running it — but it parses
//! manifests, and this reads `.rs` files, so the machinery is new.
//!
//! Disjointness is not checked here. It is a property of *values*, not of source
//! text, so it is asserted in `error.rs` by
//! `the_two_tables_name_disjoint_codes`, which reads the two tables themselves.
//! That split is deliberate: a value check cannot be evaded by choosing different
//! syntax, and belongs where it can read the data.
//!
//! # Guards, deliberately independent
//!
//! An earlier version of this file had one guard per claim, and a review broke
//! four of five with code that compiles and is idiomatic in this crate: a
//! second table written with `HRESULT::from_win32` rather than `to_hresult()`;
//! a view spelled `impl From<Condition>` rather than `fn from_condition`; a
//! wildcard hidden behind an `#[allow]` that sat *below* the `#[deny]`. Each
//! bypass was narrow, and each defeated the whole file, because the guards
//! shared assumptions.
//!
//! So they fail independently:
//!
//! 1. [`the_table_constants_have_one_home`] — the five Win32 constants that
//!    *are* the tables may appear only in `pipe_table` and `ring_table`. A third
//!    table has to name them, whatever syntax it uses to compare them.
//! 2. [`error_classification_has_one_home`] — no line outside those two
//!    functions may decide anything from a platform error code, with the
//!    exceptions enumerated and justified here.
//! 3. [`condition_is_not_part_of_the_public_api`] — the condition types and the
//!    tables stay `pub(crate)`, and the two tables keep the exact names guard 1
//!    and 2 exempt.
//!
//! The exemption in guards 1 and 2 is keyed on the names `pipe_table` and
//! `ring_table`, never on `fn from(`. The tables are consulted from `From`
//! implementations, and exempting that name would unpolice most of the crate's
//! conversion surface; holding the tables in two distinctly named functions is
//! what keeps the exemption narrow enough to be worth having.
//!
//! # What this cannot catch
//!
//! Stated rather than glossed, because a policy test that implies more coverage
//! than it has is worse than none. A third table built from bare numeric
//! literals — `&[(231u32, Error::PipeBusy), …]` — names no constant and makes
//! no comparison this file recognises. It would also be invisible to the
//! disjointness check, which reads the two real tables and cannot know about a
//! third. That residue is why the allowlist below carries reasons rather than
//! just names: the reasons are what a reader checks when this file says a change
//! is fine.
//!
//! A guard that formerly stood here policed the `ConditionView` trait. It went
//! with the trait, and the note at the foot of this file records why — including
//! the correction that the trait bought *totality*, not *singularity*, and that
//! these guards were always what prevented a second table.
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

        // The two classification tables, exempted **by their own names**.
        //
        // Not by `fn from(`. The tables are consulted from `From` implementations,
        // and exempting that name would unpolice every `From` in the crate -- which
        // is most of the conversion surface. Holding the tables in two distinctly
        // named functions is what keeps the exemption this narrow.
        let starts_classify = ["pipe_table", "ring_table"].iter().any(|name| {
            trimmed.starts_with(&format!("pub(crate) fn {name}("))
                || trimmed.starts_with(&format!("fn {name}("))
        });
        let starts_test_mod = trimmed == "#[cfg(test)]"
            && lines
                .get(index + 1)
                .is_some_and(|next| next.trim().starts_with("mod ") && !next.trim().ends_with(';'));

        if starts_classify || starts_test_mod {
            let open_indent = indent(if starts_classify {
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
         `error::pipe_table` and `error::ring_table`:\n{}\n\nThese five \
         constants are the tables. A third place that names one is a third \
         table, and multiple tables are how ERROR_PIPE_BUSY from an open and \
         from a completion stop meaning the same thing. Add the code to one of \
         the two tables instead, and note that `the_two_tables_name_disjoint_codes` \
         requires it to appear in exactly one.",
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

/// The derivation behind the design's headline cost: the condition types add no
/// public variant slots.
///
/// The per-API split ships 46 public variants across six types. That figure
/// counts only types callers can name. `PipeCondition` and `RingCondition` are
/// `pub(crate)`, appear in no public signature, and are not re-exported, so they
/// contribute zero. Checked rather than asserted, because it is a claim about the
/// design's cost and an unchecked number in this work has a poor record.
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
    for item in [
        "\n    pub(crate) enum PipeCondition {",
        "\n    pub(crate) enum RingCondition {",
    ] {
        assert!(
            error_rs.contains(item),
            "{item:?} is missing or is no longer `pub(crate)`; the public variant \
             count in the design's costing assumes the condition types contribute \
             nothing"
        );
    }
    for table in [
        "\n    pub(crate) fn pipe_table(",
        "\n    pub(crate) fn ring_table(",
    ] {
        assert!(
            error_rs.contains(table),
            "{table:?} is missing. The two tables are exempted from \
             `error_classification_has_one_home` by these exact names, so renaming \
             one silently un-exempts it -- or, worse, leaves the exemption \
             matching nothing while the table moves somewhere unpoliced."
        );
    }
    assert!(
        !error_rs.contains("pub use classification"),
        "the classification module's contents are re-exported, which undoes the \
         privacy the guard above depends on"
    );

    for (rel, path) in all_files() {
        let text = std::fs::read_to_string(&path).expect("source file is readable");
        for (index, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if !trimmed.contains("Condition")
                && !trimmed.contains("classify")
                && !trimmed.contains("pipe_table")
                && !trimmed.contains("ring_table")
            {
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
                "{rel}:{}: a condition type or classifier reached the public API: \
                 {trimmed}",
                index + 1
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Retired: the view-trait mechanism guards.
//
// This section policed a `ConditionView` trait -- one method per condition,
// implemented by five error types and dispatched by a single `view` function --
// against the two edits that would stop `E0046` firing: a default method body,
// and a dispatch arm calling the wrong method. Both guards, their `E0046`
// compile-twin, and `error_view_exhaustiveness.rs` went when the trait did.
//
// The trait was replaced by two tables consulted from `From` implementations.
// The reason is recorded in the `classification` module docs, and the part worth
// repeating here is the part this file got wrong: **the trait bought totality,
// not singularity.** It forced every view to account for every condition. It
// never prevented a second table -- a view method received the raw `HRESULT` and
// could always have compared it -- so the guards above, not the trait, were
// always what stopped a second table. They still are, which is why they stayed
// and these did not.
// ---------------------------------------------------------------------------
