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

/// The two lints every view must arm.
const REQUIRED_LINTS: &[&str] = &[
    "clippy::wildcard_enum_match_arm",
    "clippy::match_wildcard_for_single_variants",
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
        file: "error.rs",
        line: "if err.code() == E_NOTIMPL {",
        sites: 1,
        why: "context-dependent: E_NOTIMPL denotes an unusable host only while \
              creating a ring. Classifying it in the shared table would \
              reclassify unrelated E_NOTIMPL results from every other call site.",
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
        line: "Some(code) => crate::Error::from_hresult(windows::core::HRESULT::from_win32(code as u32)),",
        sites: 1,
        why: "the opposite of a second table: it builds an HRESULT from a raw \
              OS error and hands it to the shared funnel. Flagged only because \
              it is a match arm naming HRESULT, and the detector is deliberately \
              broad. Note the neighbouring E_FAIL substituted when the OS error \
              is absent — that is a separate open question under FR-11, which \
              forbids fabricating an HRESULT, and it is not settled by this entry.",
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

/// The number of views over `Condition` the crate is expected to contain.
///
/// Asserted exactly, not as a lower bound. See
/// `every_match_on_a_condition_is_an_armed_view` for why.
const EXPECTED_VIEWS: usize = 1;

/// The `Condition` variant names, used to recognise a view by its arms.
const CONDITION_VARIANTS: [&str; 6] = [
    "QueueFull",
    "PipeBusy",
    "PipeBroken",
    "PipeNoPeer",
    "PipeListening",
    "Other",
];

/// Whether line `index` is a match arm over a `Condition` variant.
///
/// The qualifier is not required to be the literal text `Condition::`. It may be
/// `Self::` inside `impl Condition`, an alias established by `use ... as`, or
/// absent entirely under a glob import. What it may *not* be is some other
/// enum's path: `Error::PipeBusy` names a variant of the crate-wide error type,
/// which is not a view and must not be dragged into this check.
fn is_condition_arm(lines: &[&str], index: usize) -> bool {
    let trimmed = lines[index].trim();
    if !trimmed.contains("=>") {
        return false;
    }
    let pattern = trimmed.split("=>").next().unwrap_or("");

    // `Other` alone is too common a variant name to key on; a view is recognised
    // by one of the five distinctive conditions, and `Other` only counts once
    // the enclosing function is already known to be a view.
    let distinctive = &CONDITION_VARIANTS[..5];

    for variant in distinctive {
        let Some(at) = pattern.find(variant) else {
            continue;
        };
        // Reject a longer identifier that merely contains the variant name.
        let after = pattern[at + variant.len()..].chars().next();
        if after.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let before = pattern[..at].trim_end();
        if !before.ends_with("::") {
            // Unqualified: a glob import, or a bare pattern.
            return true;
        }
        let path = before.trim_end_matches("::");
        let last = path.rsplit("::").next().unwrap_or(path).trim();
        if last == "Condition" {
            return true;
        }
        if last == "Self" {
            // `Self::PipeBusy` is a view only inside `impl Condition`.
            return enclosing_item(lines, index)
                .is_some_and(|item| lines[item].contains("impl Condition"));
        }
        if condition_aliases(lines).contains(&last) {
            return true;
        }
    }
    false
}

/// Local aliases for `Condition` established by `use ... as ...` in this file.
fn condition_aliases<'a>(lines: &[&'a str]) -> Vec<&'a str> {
    lines
        .iter()
        .filter(|line| line.trim_start().starts_with("use "))
        .filter_map(|line| {
            let at = line.find("Condition as ")?;
            let rest = &line[at + "Condition as ".len()..];
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            Some(&rest[..end])
        })
        .collect()
}

/// The inner attributes (`#![...]`) at the top of an item's body.
fn inner_attributes_of(lines: &[&str], item: usize) -> String {
    let Some((start, end)) = body_range(lines, item) else {
        return String::new();
    };
    lines[start..end]
        .iter()
        .map(|line| line.trim())
        .take_while(|trimmed| {
            trimmed.starts_with("#![")
                || trimmed.starts_with("//")
                || trimmed.is_empty()
                || trimmed.ends_with('{')
        })
        .filter(|trimmed| trimmed.starts_with("#!["))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The `match` blocks within `start..end` whose arms name a `Condition`.
///
/// Returned as line ranges so the wildcard check can be confined to them. A
/// view's body may contain other matches — over an `i32`, a `str`, or any
/// `#[non_exhaustive]` foreign enum — which cannot be written exhaustively and
/// whose wildcards say nothing about the view's coverage of `Condition`.
fn condition_match_blocks(lines: &[&str], start: usize, end: usize) -> Vec<(usize, usize)> {
    let mut blocks: Vec<(usize, usize)> = Vec::new();

    for index in start..end {
        if !is_condition_arm(lines, index) {
            continue;
        }
        if blocks.iter().any(|&(s, e)| index >= s && index < e) {
            continue;
        }
        // Walk up to the `match` that owns this arm: the nearest line above at
        // a smaller indent that opens a match block.
        let arm_indent = indent(lines[index]);
        let Some(head) = (start..index)
            .rev()
            .find(|&candidate| {
                let trimmed = lines[candidate].trim();
                !trimmed.starts_with("//")
                    && trimmed.starts_with("match ")
                    && indent(lines[candidate]) < arm_indent
            })
            .or(Some(start))
        else {
            continue;
        };
        if let Some((block_start, block_end)) = body_range(lines, head) {
            blocks.push((block_start, block_end.min(end)));
        }
    }

    blocks
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

/// The extent of the function body beginning at or after `signature`.
///
/// Returns the half-open range of line indices covering the body, or `None` if
/// no opening brace follows. Seeding the depth from the signature line was a
/// defect: a signature wrapped across lines by rustfmt has no brace on it, the
/// depth started at zero, and the scan concluded the body had already ended.
/// That shape is not contrived — rustfmt produces it whenever the signature
/// exceeds the width limit.
fn body_range(lines: &[&str], signature: usize) -> Option<(usize, usize)> {
    let mut depth = 0usize;
    let mut opened = false;
    for (offset, line) in lines.iter().enumerate().skip(signature) {
        depth += line.matches('{').count();
        if depth > 0 {
            opened = true;
        }
        depth = depth.saturating_sub(line.matches('}').count());
        if opened && depth == 0 {
            return Some((signature, offset + 1));
        }
    }
    None
}

/// The function signature enclosing `target`, if any.
///
/// Doc comments are skipped: `trimmed.contains(" fn ")` is true of prose such as
/// "the fn that classifies", and resolving a view's owner to a doc line would
/// yield a nonsense body range and a false "does not deny".
fn enclosing_fn(lines: &[&str], target: usize) -> Option<usize> {
    let target_indent = indent(lines[target]);
    (0..target).rev().find(|&candidate| {
        let line = lines[candidate];
        let trimmed = line.trim();
        if trimmed.starts_with("//") {
            return false;
        }
        let is_fn = trimmed.starts_with("fn ")
            || trimmed.starts_with("pub fn ")
            || trimmed.starts_with("pub(crate) fn ")
            || trimmed.contains(" fn ");
        is_fn && indent(line) < target_indent
    })
}

/// The `impl` or `mod` header enclosing `target`, if any.
///
/// Used for two things: deciding whether `Self::PipeBusy` names a `Condition`,
/// and finding a `#[deny(...)]` written on the enclosing item rather than on the
/// function. The second is a *more* robust placement than the one this file
/// originally demanded, and rejecting it taught the next author to hollow out
/// the guard rather than obey it.
fn enclosing_item(lines: &[&str], target: usize) -> Option<usize> {
    let target_indent = indent(lines[target]);
    (0..target).rev().find(|&candidate| {
        let line = lines[candidate];
        let trimmed = line.trim();
        if trimmed.starts_with("//") {
            return false;
        }
        let is_item = trimmed.starts_with("impl ")
            || trimmed.starts_with("impl<")
            || trimmed.starts_with("mod ")
            || trimmed.starts_with("pub mod ")
            || trimmed.starts_with("pub(crate) mod ");
        is_item && indent(line) < target_indent
    })
}

/// The attribute text immediately above `signature`, joined into one string.
///
/// The walk stops at the first line that is neither an attribute nor a doc
/// comment. It used to treat the two interchangeably and only ask whether a
/// `deny` string appeared *somewhere* above, which let an `#[allow]` placed
/// below the `#[deny]` satisfy the check while overriding it.
///
/// Attributes are joined rather than examined line by line because an attribute
/// may span lines — `rustfmt` wraps `#[deny(a, b)]` automatically once it
/// exceeds the width limit, and a per-line parser rejected the result. A check
/// that fails on the formatting the project's own formatter produces will be
/// worked around rather than obeyed.
///
/// Doc comments are dropped, so prose that happens to quote an attribute cannot
/// be mistaken for one. This file's own documentation quotes both lints.
///
/// The walk refuses to *enter* continuation state from a comment, and only
/// treats a line as an attribute's closing tail if it looks like one. A previous
/// version decremented `depth` on `[` alone, so a single stray `]` inside a doc
/// comment — a typo in a code span, say — sent the walk climbing forever,
/// harvesting arbitrary prose as attribute text. Documentation quoting
/// `#[deny(...)]` then satisfied a check on a function that denied nothing. That
/// is not hypothetical: it was demonstrated against this file, and combined with
/// a binding catch-all it disarmed a view completely while clippy stayed silent.
fn attributes_above(lines: &[&str], signature: usize) -> String {
    let mut collected: Vec<&str> = Vec::new();
    let mut depth = 0usize;

    for line in lines[..signature].iter().rev() {
        let trimmed = line.trim();
        let closes = line.matches(']').count();
        let opens = line.matches('[').count();
        let is_comment = trimmed.starts_with("//");
        // Walking upwards, a wrapped attribute is met at its *last* line, which
        // is `)]` and starts with neither `#[` nor `//`. Recognising it by shape
        // is what lets the walk reach the `#[deny(` above it, without treating
        // every line that happens to contain a `]` as an attribute tail.
        let continues = depth > 0 && !is_comment;
        let is_attribute_tail = !continues
            && !is_comment
            && closes > opens
            && trimmed.trim_end_matches(']').trim_end_matches(',').trim() == ")";
        let is_attribute_start = trimmed.starts_with("#[");

        if !continues && !is_attribute_tail && !is_attribute_start && !is_comment {
            break;
        }
        if continues || is_attribute_tail || is_attribute_start {
            collected.push(trimmed);
        } else {
            // A comment reached at depth zero: skip it, and never let it put the
            // walk into continuation state.
            depth = 0;
            continue;
        }
        depth = (depth + closes).saturating_sub(opens);
    }

    collected.reverse();
    collected.join(" ")
}

/// Whether `lint` appears inside an attribute of the given kind in `text`.
///
/// Parses the lint names out of `deny(...)` or `allow(...)` with parenthesis
/// matching rather than comparing whole lines, so the combined form
/// `#[deny(a, b)]` counts for both lints and a wrapped one counts too.
/// Rejecting a spelling that is correct and equally effective would teach a
/// reader to work around this test rather than obey it.
fn attribute_names_lint(text: &str, kind: &str, lint: &str) -> bool {
    let bare = lint.trim_start_matches("clippy::");
    let mut rest = text;
    while let Some(start) = rest.find(kind) {
        let after = &rest[start + kind.len()..];
        let mut depth = 1usize;
        let mut end = after.len();
        for (offset, ch) in after.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = offset;
                        break;
                    }
                }
                _ => {}
            }
        }
        let named = after[..end]
            .split(',')
            .any(|name| name.trim() == lint || name.trim() == bare);
        if named {
            return true;
        }
        rest = &after[end.min(after.len())..];
    }
    false
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

/// FR-10, FR-11, SC-4, SC-6: every `match` on a `Condition` is an armed,
/// wildcard-free view.
///
/// Views are found by what they *do*, not by what they are called. Keying on
/// the name `from_condition` left `impl From<Condition> for X` — the first
/// thing a Rust author reaches for — invisible to this check while both lints
/// stayed allow-by-default and silent.
///
/// Detection is by *variant name*, not by the path written in front of it. An
/// inherent method on `Condition` spells its arms `Self::PipeBusy`, which is
/// what anyone writing `impl Condition` would produce, and a detector keyed on
/// the literal text `Condition::` never saw it. Glob imports and `use Condition
/// as C` are covered for the same reason.
///
/// The count is asserted, not merely required to be positive. Every defeat of
/// this guard so far worked by making a view *invisible* to it rather than by
/// making a visible view look correct, and an invisible view is indistinguishable
/// from no view at all unless the expected number is written down. This is the
/// same discipline the allowlist already applies with its `sites` counts, for
/// the same reason: text matching cannot tell absence from oversight.
#[test]
fn every_match_on_a_condition_is_an_armed_view() {
    let mut views = 0usize;
    let mut problems = Vec::new();

    for (rel, path) in source_files() {
        let text = std::fs::read_to_string(&path).expect("source file is readable");
        let lines: Vec<&str> = text.lines().collect();

        // Every line that matches on a Condition variant, mapped to the
        // function that contains it.
        let mut owners: Vec<usize> = Vec::new();
        for index in 0..lines.len() {
            if !is_condition_arm(&lines, index) {
                continue;
            }
            if let Some(owner) = enclosing_fn(&lines, index) {
                if !owners.contains(&owner) {
                    owners.push(owner);
                }
            } else {
                problems.push(format!(
                    "{rel}:{}: matches on a Condition outside any function",
                    index + 1
                ));
            }
        }

        for owner in owners {
            views += 1;
            // A `#[deny]` may sit on the function or on the `impl`/`mod` that
            // contains it. The second is the more robust placement — it covers
            // every view added to the block later — and rejecting it would
            // teach the next author to work around this test.
            let mut attributes = attributes_above(&lines, owner);
            if let Some(item) = enclosing_item(&lines, owner) {
                attributes.push(' ');
                attributes.push_str(&attributes_above(&lines, item));
                attributes.push(' ');
                attributes.push_str(&inner_attributes_of(&lines, item));
            }
            let (start, end) = body_range(&lines, owner).unwrap_or_else(|| {
                panic!("{rel}:{}: view has no body", owner + 1);
            });
            let body = &lines[start..end];

            for lint in REQUIRED_LINTS {
                if !attribute_names_lint(&attributes, "deny(", lint) {
                    problems.push(format!("{rel}:{}: does not deny {lint}", owner + 1));
                }
                // An allow anywhere in the preamble or the body overrides the
                // deny, so a view can be armed in the source and unarmed in
                // effect.
                let allowed_above = attribute_names_lint(&attributes, "allow(", lint);
                let allowed_within = body.iter().any(|line| {
                    let trimmed = line.trim();
                    trimmed.starts_with("#[") && attribute_names_lint(trimmed, "allow(", lint)
                });
                if allowed_above || allowed_within {
                    problems.push(format!(
                        "{rel}:{}: allows {lint}, which overrides the deny",
                        owner + 1
                    ));
                }
            }

            // The wildcard scan is bounded to the `match` blocks that actually
            // arm on a Condition, and within those, to arms of that match
            // itself. Scanning the whole function rejected a view whose body
            // also matched on an `i32` — which *cannot* be written exhaustively
            // — and reported it as a wildcard over Condition, which was simply
            // untrue. Nested matches sit at a deeper brace depth and are not
            // this match's arms.
            for (block_start, block_end) in condition_match_blocks(&lines, start, end) {
                let mut depth = 0isize;
                for (offset, line) in lines[block_start + 1..block_end].iter().enumerate() {
                    let trimmed = line.trim();
                    if depth == 0 && (trimmed.starts_with("_ =>") || trimmed.starts_with("_ if")) {
                        problems.push(format!(
                            "{rel}:{}: wildcard arm in a view over Condition",
                            block_start + offset + 2
                        ));
                    }
                    depth += line.matches('{').count() as isize;
                    depth -= line.matches('}').count() as isize;
                    depth = depth.max(0);
                }
            }
        }
    }

    assert_eq!(
        views, EXPECTED_VIEWS,
        "expected {EXPECTED_VIEWS} view(s) over the classification table, found \
         {views}. If you added a view, arm it and raise EXPECTED_VIEWS. If you \
         removed one, lower it. If you changed neither, this test has stopped \
         recognising a view that still exists — which is exactly how a view \
         gets to skip the checks below without anyone noticing."
    );
    assert!(
        problems.is_empty(),
        "views over the classification table are not properly armed:\n{}\n\n\
         The point of per-API error types is that each names what it can \
         actually produce. A wildcard lets a type claim conditions nobody \
         checked it against, and turns adding a Condition variant into a silent \
         change rather than a compile error.",
        problems.join("\n")
    );
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
        error_rs.contains("pub(crate) enum Condition {"),
        "Condition is no longer crate-private; the public variant count in the \
         design's costing assumed it contributed nothing"
    );
    assert!(
        error_rs.contains("pub(crate) fn classify("),
        "classify is no longer crate-private"
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
