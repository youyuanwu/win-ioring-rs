//! SC-005: the crate under test must not depend on an async runtime.
//!
//! "Runtime agnostic" is a claim about the dependency graph as much as about
//! the API, and a dependency can be added without any test noticing. This
//! reads the manifest so that adding one breaks the build.

/// Runtimes the crate must not pull in.
///
/// Naming them explicitly is deliberate: a heuristic over every dependency name
/// would either miss a runtime or start failing on unrelated crates.
const ASYNC_RUNTIMES: &[&str] = &[
    "tokio",
    "async-std",
    "async_std",
    "smol",
    "compio",
    "monoio",
    "glommio",
    "actix-rt",
    "async-global-executor",
];

/// Returns the `[dependencies]` section of the crate's manifest.
///
/// A crude section split is enough here and avoids a TOML parser dependency:
/// the assertion is about `[dependencies]` alone, since `[dev-dependencies]`
/// legitimately contains Tokio for the crate's own tests.
fn runtime_dependencies_section() -> String {
    let manifest = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../win-ioring/Cargo.toml"
    ))
    .expect("the crate under test must have a manifest");

    let mut section = String::new();
    let mut in_dependencies = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_dependencies = trimmed == "[dependencies]";
            continue;
        }
        if in_dependencies {
            section.push_str(line);
            section.push('\n');
        }
    }
    section
}

/// SC-005: no async runtime appears under `[dependencies]`.
#[test]
fn the_crate_declares_no_async_runtime_dependency() {
    let section = runtime_dependencies_section();

    // Guard against the section parser silently returning nothing, which would
    // make every assertion below pass without checking anything.
    assert!(
        section.contains("windows"),
        "the dependencies section was not found; parsed:\n{section}"
    );

    for runtime in ASYNC_RUNTIMES {
        assert!(
            !section.contains(runtime),
            "`{runtime}` appears under [dependencies]; the crate must stay runtime agnostic:\n{section}"
        );
    }
}

/// The parser must actually distinguish the two sections, or the test above
/// would pass even with Tokio promoted to a real dependency.
#[test]
fn the_manifest_parser_excludes_dev_dependencies() {
    let manifest = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../win-ioring/Cargo.toml"
    ))
    .unwrap();

    assert!(
        manifest.contains("[dev-dependencies]") && manifest.contains("tokio"),
        "this test assumes the crate has Tokio as a dev-dependency"
    );
    assert!(
        !runtime_dependencies_section().contains("tokio"),
        "the parser leaked [dev-dependencies] into the [dependencies] section"
    );
}
