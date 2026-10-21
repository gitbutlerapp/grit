//! `grit diff --patch-text` behavior.

use std::fs;
use std::process::Command;

use tempfile::TempDir;

#[test]
fn diff_patch_text_rejected_with_json() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let out = Command::new(grit)
        .args(["diff", "--patch-text", "--json"])
        .output()
        .expect("spawn grit");
    assert!(!out.status.success(), "expected failure");
    let msg = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        msg.contains("--patch-text") && msg.contains("--json"),
        "expected clear error, got: {msg}"
    );
}

fn git_in(dir: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn diff_patch_text_unstaged_git_apply_check() {
    let dir = TempDir::new().expect("tempdir");
    git_in(dir.path(), &["init"]);
    git_in(dir.path(), &["config", "user.email", "t@example.com"]);
    git_in(dir.path(), &["config", "user.name", "Test"]);
    fs::write(dir.path().join("file.txt"), "one\n").expect("write");
    git_in(dir.path(), &["add", "file.txt"]);
    git_in(dir.path(), &["commit", "-m", "init"]);

    fs::write(dir.path().join("file.txt"), "two\n").expect("write");

    let grit = env!("CARGO_BIN_EXE_grit");
    let patch = Command::new(grit)
        .current_dir(dir.path())
        .args(["diff", "--patch-text", "--unstaged"])
        .output()
        .expect("spawn grit");
    assert!(
        patch.status.success(),
        "grit diff --patch-text: {}",
        String::from_utf8_lossy(&patch.stderr)
    );
    let patch_text = String::from_utf8_lossy(&patch.stdout);
    assert!(
        patch_text.contains("@@") && patch_text.contains("index "),
        "expected full patch body, got:\n{patch_text}"
    );

    git_in(
        dir.path(),
        &["restore", "--source=HEAD", "--worktree", "file.txt"],
    );

    let check = Command::new("git")
        .current_dir(dir.path())
        .args(["apply", "--check", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn git apply");
    use std::io::Write;
    let mut child = check;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(patch.stdout.as_slice()).expect("stdin");
    }
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "git apply --check failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn diff_patch_text_staged_git_apply_check() {
    let dir = TempDir::new().expect("tempdir");
    git_in(dir.path(), &["init"]);
    git_in(dir.path(), &["config", "user.email", "t@example.com"]);
    git_in(dir.path(), &["config", "user.name", "Test"]);
    fs::write(dir.path().join("file.txt"), "one\n").expect("write");
    git_in(dir.path(), &["add", "file.txt"]);
    git_in(dir.path(), &["commit", "-m", "init"]);

    fs::write(dir.path().join("file.txt"), "two\n").expect("write");
    git_in(dir.path(), &["add", "file.txt"]);

    let grit = env!("CARGO_BIN_EXE_grit");
    let patch = Command::new(grit)
        .current_dir(dir.path())
        .args(["diff", "--patch-text", "--staged"])
        .output()
        .expect("spawn grit");
    assert!(
        patch.status.success(),
        "grit diff --patch-text --staged: {}",
        String::from_utf8_lossy(&patch.stderr)
    );
    let patch_text = String::from_utf8_lossy(&patch.stdout);
    assert!(
        patch_text.contains("@@") && patch_text.contains("index "),
        "expected full patch body, got:\n{patch_text}"
    );

    git_in(
        dir.path(),
        &[
            "restore",
            "--source=HEAD",
            "--staged",
            "--worktree",
            "file.txt",
        ],
    );

    let check = Command::new("git")
        .current_dir(dir.path())
        .args(["apply", "--check", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn git apply");
    use std::io::Write;
    let mut child = check;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(patch.stdout.as_slice()).expect("stdin");
    }
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "git apply --check failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
