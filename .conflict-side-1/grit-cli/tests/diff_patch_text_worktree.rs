//! `--patch-text` on the default worktree diff includes unified hunks.

use std::process::Command;

use tempfile::TempDir;

fn grit(args: &[&str], dir: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_grit"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn grit")
}

#[test]
fn diff_patch_text_worktree_includes_hunks() {
    let dir = TempDir::new().expect("tempdir");
    let p = dir.path();
    Command::new("git")
        .current_dir(p)
        .args(["init", "-q", "--initial-branch=main", "."])
        .status()
        .expect("git init");
    std::fs::write(p.join("file.txt"), b"base\n").unwrap();
    Command::new("git")
        .current_dir(p)
        .args(["add", "file.txt"])
        .status()
        .expect("git add");
    Command::new("git")
        .current_dir(p)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-qm",
            "init",
        ])
        .status()
        .expect("git commit");
    std::fs::write(p.join("file.txt"), b"base\nchanged\n").unwrap();
    let out = grit(&["diff", "--patch-text"], p);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let patch = String::from_utf8_lossy(&out.stdout);
    assert!(
        patch.contains("@@"),
        "expected hunk header in patch:\n{patch}"
    );
    assert!(patch.contains("+changed"), "expected added line in patch");
}
