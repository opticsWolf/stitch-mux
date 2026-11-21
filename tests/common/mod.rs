//! Shared helpers for repository integration tests.

use std::path::PathBuf;

/// Resolve a repository-only PTY fixture binary.
///
/// The fixtures live in the `stitch-mux-test-fixtures` package so they are not
/// product binary targets and never ship in the crates.io archive. CI builds
/// them explicitly; a local `cargo test --workspace` also builds the workspace
/// member before running these tests.
pub fn fixture_bin(name: &str) -> String {
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    let file = format!("{name}{suffix}");
    if let Ok(dir) = std::env::var("STITCH_MUX_FIXTURE_DIR") {
        let path = PathBuf::from(dir).join(&file);
        assert!(path.is_file(), "fixture not found: {}", path.display());
        return path.to_string_lossy().into_owned();
    }
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(profile)
        .join(&file);
    assert!(
        path.is_file(),
        "fixture {} not built; run `cargo build -p stitch-mux-test-fixtures`",
        path.display()
    );
    path.to_string_lossy().into_owned()
}
