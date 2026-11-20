//! The agent skill installs idempotently into portable local skill layouts.

use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_root() -> PathBuf {
    let unique = format!(
        "skill-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    );
    std::env::temp_dir().join(unique)
}

#[test]
fn skill_installer_is_idempotent() {
    let root = temp_root();
    for _ in 0..2 {
        let output = Command::new(env!("CARGO_BIN_EXE_stitch-mux"))
            .args(["skill-install", "--root"])
            .arg(&root)
            .args(["--targets", "pi,claude"])
            .output()
            .expect("skill-install runs");
        assert!(
            output.status.success(),
            "installer failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    for relative in [
        ".agents/skills/stitch-mux/SKILL.md",
        ".agents/skills/stitch-mux/references/commands.md",
        ".claude/skills/stitch-mux/SKILL.md",
        ".claude/skills/stitch-mux/references/commands.md",
    ] {
        assert!(root.join(relative).is_file(), "missing {relative}");
    }
    let skill = std::fs::read_to_string(root.join(".agents/skills/stitch-mux/SKILL.md"))
        .expect("skill text");
    assert!(skill.starts_with("---\nname: stitch-mux\n"));
    assert!(skill.contains("stitch-mux ping"));
    assert!(skill.contains("no MCP server"));

    std::fs::remove_dir_all(&root).expect("cleanup");
}
