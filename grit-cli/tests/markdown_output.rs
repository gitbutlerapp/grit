//! Integration tests for `--markdown` output on key commands.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn init_repo(dir: &std::path::Path) {
    let status = Command::new(GRIT)
        .args(["init", dir.to_str().unwrap()])
        .status()
        .expect("grit init");
    assert!(status.success());
}

#[test]
fn log_markdown_uses_commit_bullets_not_json_blob() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("README.md"), "hi\n").unwrap();
    Command::new(GRIT)
        .args(["add", "README.md"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(Command::new(GRIT)
        .args(["commit", "-m", "first"])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());
    let out = Command::new(GRIT)
        .args(["log", "--markdown"])
        .current_dir(dir.path())
        .output()
        .expect("log");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("- `"),
        "expected commit bullet, got:\n{stdout}"
    );
    assert!(stdout.contains("first"));
    assert!(
        !stdout.contains("**commits**:"),
        "should not use generic JSON field bullets:\n{stdout}"
    );
}

#[test]
fn diff_markdown_uses_fenced_diff_blocks() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(
        dir.path().join("hello.txt"),
        "line one\nline two\nline three\n",
    )
    .unwrap();
    Command::new(GRIT)
        .args(["add", "hello.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    Command::new(GRIT)
        .args(["commit", "-m", "add hello"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    std::fs::write(
        dir.path().join("hello.txt"),
        "line one\nchanged\nline three\n",
    )
    .unwrap();
    let out = Command::new(GRIT)
        .args(["diff", "--markdown"])
        .current_dir(dir.path())
        .output()
        .expect("diff");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("```diff"), "stdout:\n{stdout}");
    assert!(!stdout.contains("**files**:"));

    let patch = stdout
        .split("```diff")
        .nth(1)
        .and_then(|rest| rest.split("```").next())
        .expect("diff fence body");
    assert!(
        patch.contains("\n-line two\n") || patch.contains("\n-line two"),
        "deletion should be its own line:\n{patch}"
    );
    assert!(
        patch.contains("\n+changed\n") || patch.contains("\n+changed"),
        "addition should be its own line:\n{patch}"
    );
    assert!(
        !patch.contains("-line two+changed"),
        "hunk lines must not be concatenated:\n{patch}"
    );
}

#[test]
fn branch_markdown_lists_branches_without_json_blob() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("f.txt"), "x\n").unwrap();
    Command::new(GRIT)
        .args(["add", "f.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    Command::new(GRIT)
        .args(["commit", "-m", "init"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(Command::new(GRIT)
        .args(["branch", "topic"])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());
    let out = Command::new(GRIT)
        .args(["branch", "--markdown"])
        .current_dir(dir.path())
        .output()
        .expect("branch");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("## Branches"));
    assert!(stdout.contains("**`main`** (current)"));
    assert!(stdout.contains("`topic`"));
    assert!(
        !stdout.contains("**branches**:"),
        "should not dump nested JSON:\n{stdout}"
    );
}

#[test]
fn status_markdown_has_staged_section() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("x.txt"), "x\n").unwrap();
    Command::new(GRIT)
        .args(["add", "x.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    let out = Command::new(GRIT)
        .args(["status", "--markdown"])
        .current_dir(dir.path())
        .output()
        .expect("status");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("## Staged"));
    assert!(stdout.contains("x.txt"));
}

#[test]
fn skill_markdown_prints_verbatim_skill_file() {
    let out = Command::new(GRIT)
        .args(["skill", "--markdown"])
        .output()
        .expect("skill");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("---\nname: grit\n"));
    assert!(!stdout.contains("**content**:"));
}

#[test]
fn completions_rejects_json_and_markdown() {
    for flag in ["--json", "--markdown"] {
        let out = Command::new(GRIT)
            .args(["completions", "zsh", flag])
            .output()
            .expect("completions");
        assert!(!out.status.success(), "expected failure with {flag}");
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(combined.contains("does not support"));
    }
}
