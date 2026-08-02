//! Temporary files that clean themselves up.
//!
//! Shared by the integration test binaries, which are separate crates and so
//! cannot see each other's helpers.

/// Builds a temporary path unique to this process and thread.
///
/// Tests run in parallel threads within one process, so both are needed to keep
/// two tests from colliding on the same file.
pub fn temp_path(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "win-ioring-rt-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    p
}

/// A temporary path that is removed when the guard drops.
pub struct TempFile(std::path::PathBuf);

impl TempFile {
    /// Reserves a temporary path tagged for the calling test.
    pub fn new(tag: &str) -> Self {
        Self(temp_path(tag))
    }

    /// Returns the reserved path.
    pub fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
