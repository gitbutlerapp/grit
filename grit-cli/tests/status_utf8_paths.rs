//! UTF-8 path names in human status output should print literally (not Git octal escapes).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

#[test]
fn status_human_shows_utf8_paths_literally() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(Command::new(GRIT)
        .args(["init", dir.path().to_str().unwrap()])
        .status()
        .expect("grit init")
        .success());

    std::fs::write(dir.path().join("日本語-new.txt"), b"x").unwrap();
    std::fs::write(dir.path().join("café.txt"), b"y").unwrap();
    std::fs::write(dir.path().join("plain.txt"), b"z").unwrap();
    std::fs::File::create(dir.path().join("ctl\x01char.txt")).unwrap();

    let out = Command::new(GRIT)
        .args(["status"])
        .current_dir(dir.path())
        .output()
        .expect("grit status");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("日本語-new.txt"),
        "expected literal CJK filename in human status:\n{text}"
    );
    assert!(
        text.contains("café.txt"),
        "expected literal accented filename in human status:\n{text}"
    );
    assert!(
        !text.contains("\\346"),
        "unexpected octal UTF-8 escape in human status:\n{text}"
    );
    assert!(
        text.contains("\\001"),
        "control bytes should still be C-quoted:\n{text}"
    );

    let json = Command::new(GRIT)
        .args(["status", "--json"])
        .current_dir(dir.path())
        .output()
        .expect("grit status --json");
    assert!(json.status.success());
    let json_text = String::from_utf8_lossy(&json.stdout);
    assert!(json_text.contains("日本語-new.txt"));
    assert!(json_text.contains("café.txt"));
}
