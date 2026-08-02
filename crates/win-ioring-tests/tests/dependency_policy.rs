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

/// Collects the names of every dependency in `manifest` that reaches a
/// consumer.
///
/// `[dev-dependencies]` is deliberately excluded: those exist only for the
/// crate's own tests, which is why Tokio may appear there. Everything else
/// counts, including build and target-specific dependencies.
///
/// Both the key and any `package` rename are collected, because
/// `not_tokio = { package = "tokio" }` is a dependency on Tokio however it is
/// spelled at the use site.
fn dependency_names(manifest: &Table) -> Vec<String> {
    fn extend(names: &mut Vec<String>, table: Option<&Value>) {
        let Some(Value::Table(t)) = table else {
            return;
        };
        for (key, spec) in t {
            names.push(key.clone());
            if let Some(Value::String(renamed)) = spec.get("package") {
                names.push(renamed.clone());
            }
        }
    }

    let mut names = Vec::new();
    extend(&mut names, manifest.get("dependencies"));
    extend(&mut names, manifest.get("build-dependencies"));

    // `[target.<cfg>.dependencies]` and its build counterpart.
    if let Some(Value::Table(targets)) = manifest.get("target") {
        for spec in targets.values() {
            extend(&mut names, spec.get("dependencies"));
            extend(&mut names, spec.get("build-dependencies"));
        }
    }

    names.sort();
    names.dedup();
    names
}

/// Parses the manifest of the crate under test.
fn crate_manifest() -> Table {
    std::fs::read_to_string(MANIFEST)
        .expect("the crate under test must have a manifest")
        .parse()
        .expect("the manifest must be valid TOML")
}

/// SC-005: no async runtime appears among the crate's dependencies.
#[test]
fn the_crate_declares_no_async_runtime_dependency() {
    let declared = dependency_names(&crate_manifest());

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
        !dependency_names(&crate_manifest())
            .iter()
            .any(|d| d == "tokio"),
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

    assert_eq!(
        dependency_names(&manifest),
        vec![
            "as-its-own-table",
            "at-build-time",
            "plain",
            "renamed",
            "target-renamed",
            "target-specific",
            "the-real-name",
            "the-real-target-name",
        ],
        "the collector missed a dependency form, or picked up a dev-dependency"
    );
}
