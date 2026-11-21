//! Shared helpers for repository integration tests.

use std::path::PathBuf;

/// Resolve a repository-only PTY fixture binary.
///
/// The fixtures live in the `stitch-mux-test-fixtures` package so they are not
/// product binary targets. Run `cargo build -p stitch-mux-test-fixtures`
/// before `cargo test --workspace`, using the same profile for both commands.
/// Set `STITCH_MUX_FIXTURE_DIR` when building into a custom target directory.
pub fn fixture_bin(name: &str) -> String {
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    let file = format!("{name}{suffix}");
    if let Ok(dir) = std::env::var("STITCH_MUX_FIXTURE_DIR") {
        let path = PathBuf::from(dir).join(&file);
        assert!(path.is_file(), "fixture not found: {}", path.display());
        return path.to_string_lossy().into_owned();
    }
    let (profile, release_flag) = if cfg!(debug_assertions) {
        ("debug", "")
    } else {
        ("release", " --release")
    };
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(profile)
        .join(&file);
    assert!(
        path.is_file(),
        "fixture {} not built; run `cargo build{release_flag} -p stitch-mux-test-fixtures` or set STITCH_MUX_FIXTURE_DIR",
        path.display()
    );
    path.to_string_lossy().into_owned()
}
