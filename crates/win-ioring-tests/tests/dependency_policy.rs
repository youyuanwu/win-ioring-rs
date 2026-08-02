//! SC-005: the crate under test must not depend on an async runtime.
//!
//! "Runtime agnostic" is a claim about the dependency graph as much as about
//! the API, and a dependency can be added without any test noticing. This reads
//! the manifest so that adding one breaks the build.
//!
//! The manifest is parsed as TOML rather than scanned for text, because Cargo
//! accepts several forms a naive scan would miss: `[dependencies.tokio]` as its
//! own table, and `[target.'cfg(windows)'.dependencies]` for target-specific
//! dependencies. Each would violate the claim just as surely as an entry under
//! `[dependencies]`.

use toml::{Table, Value};

/// Runtimes the crate must not pull in.
///
/// Naming them explicitly is deliberate: a heuristic over every dependency name
/// would either miss a runtime or start failing on unrelated crates.
const ASYNC_RUNTIMES: &[&str] = &[
    "tokio",
    "async-std",
    "smol",
    "compio",
    "monoio",
    "glommio",
    "actix-rt",
    "async-global-executor",
];

/// The manifest of the crate under test.
const MANIFEST: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../win-ioring/Cargo.toml");

/// The workspace root manifest, which is where inherited dependencies are
/// actually declared.
const WORKSPACE_MANIFEST: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml");

/// Collects the names of every dependency in `manifest` that reaches a
/// consumer.
///
/// `[dev-dependencies]` is deliberately excluded: those exist only for the
/// crate's own tests, which is why Tokio may appear there. Everything else
/// counts, including build and target-specific dependencies.
///
/// Three spellings all have to resolve to the same crate:
///
/// - `tokio = "1"`, the plain form;
/// - `not_tokio = { package = "tokio" }`, renamed at the use site;
/// - `not_tokio.workspace = true` paired with a `package` rename in the
///   workspace manifest, which is the form this repository actually uses.
///
/// The last is why `workspace` is passed in: without it an inherited dependency
/// would be collected under its local alias alone, and the policy could be
/// evaded by renaming it once in the workspace manifest.
fn dependency_names(manifest: &Table, workspace: &Table) -> Vec<String> {
    /// The `[workspace.dependencies]` table, if there is one.
    fn workspace_dependencies(workspace: &Table) -> Option<&Value> {
        workspace.get("workspace")?.get("dependencies")
    }

    let inherited = workspace_dependencies(workspace);

    let mut names = Vec::new();
    let mut extend = |table: Option<&Value>| {
        let Some(Value::Table(t)) = table else {
            return;
        };
        for (key, spec) in t {
            names.push(key.clone());

            // A rename declared here.
            if let Some(Value::String(renamed)) = spec.get("package") {
                names.push(renamed.clone());
            }

            // A rename declared in the workspace and inherited here.
            if spec.get("workspace") == Some(&Value::Boolean(true))
                && let Some(Value::String(renamed)) = inherited
                    .and_then(|d| d.get(key))
                    .and_then(|d| d.get("package"))
            {
                names.push(renamed.clone());
            }
        }
    };

    extend(manifest.get("dependencies"));
    extend(manifest.get("build-dependencies"));

    // `[target.<cfg>.dependencies]` and its build counterpart.
    if let Some(Value::Table(targets)) = manifest.get("target") {
        for spec in targets.values() {
            extend(spec.get("dependencies"));
            extend(spec.get("build-dependencies"));
        }
    }

    names.sort();
    names.dedup();
    names
}

/// Parses a manifest.
fn parse_manifest(path: &str) -> Table {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("{path} must be readable: {e}"))
        .parse()
        .unwrap_or_else(|e| panic!("{path} must be valid TOML: {e}"))
}

/// The dependency names the crate under test actually takes on.
fn crate_dependency_names() -> Vec<String> {
    dependency_names(
        &parse_manifest(MANIFEST),
        &parse_manifest(WORKSPACE_MANIFEST),
    )
}

/// SC-005: no async runtime appears among the crate's dependencies.
#[test]
fn the_crate_declares_no_async_runtime_dependency() {
    let declared = crate_dependency_names();

    // Guard against the parse silently returning nothing, which would make the
    // assertion below pass without checking anything.
    assert!(
        declared.iter().any(|d| d == "windows"),
        "no dependencies were found; the manifest parse is wrong: {declared:?}"
    );

    for runtime in ASYNC_RUNTIMES {
        assert!(
            !declared.iter().any(|d| d == runtime),
            "`{runtime}` is a dependency of the crate, which must stay runtime \
             agnostic; declared: {declared:?}"
        );
    }
}

/// Dev-dependencies must be excluded, or the assertion above would fail on the
/// crate's own Tokio-based tests.
#[test]
fn dev_dependencies_are_excluded() {
    let manifest = std::fs::read_to_string(MANIFEST).unwrap();
    assert!(
        manifest.contains("[dev-dependencies]") && manifest.contains("tokio"),
        "this test assumes the crate has Tokio as a dev-dependency"
    );
    assert!(
        !crate_dependency_names().iter().any(|d| d == "tokio"),
        "dev-dependencies leaked into the collected set"
    );
}

/// The collector must see every form Cargo accepts.
///
/// Without this, the policy test could quietly stop working the day someone
/// writes a dependency as its own table, scopes it to a target, or renames it —
/// which is exactly how earlier versions of this test could have been bypassed.
#[test]
fn the_collector_sees_every_dependency_form() {
    let manifest: Table = r#"
[dependencies]
plain = "1"
renamed = { package = "the-real-name", version = "1" }
inherited.workspace = true
inherited-and-renamed.workspace = true

[dependencies.as-its-own-table]
version = "1"

[target.'cfg(windows)'.dependencies]
target-specific = "1"
target-renamed = { package = "the-real-target-name", version = "1" }

[build-dependencies]
at-build-time = "1"

[dev-dependencies]
only-for-tests = "1"
"#
    .parse()
    .unwrap();

    // The rename lives in the workspace manifest, not the crate's, which is the
    // form this repository uses and the one that evaded an earlier version of
    // this collector.
    let workspace: Table = r#"
[workspace.dependencies]
inherited = "1"
inherited-and-renamed = { package = "the-real-inherited-name", version = "1" }
"#
    .parse()
    .unwrap();

    assert_eq!(
        dependency_names(&manifest, &workspace),
        vec![
            "as-its-own-table",
            "at-build-time",
            "inherited",
            "inherited-and-renamed",
            "plain",
            "renamed",
            "target-renamed",
            "target-specific",
            "the-real-inherited-name",
            "the-real-name",
            "the-real-target-name",
        ],
        "the collector missed a dependency form, or picked up a dev-dependency"
    );
}
