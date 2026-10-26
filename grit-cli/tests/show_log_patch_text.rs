//! `grit show` / `grit log --patch-text` git-apply compatibility (issue #2005).

use std::fs;
use std::process::Command;

use tempfile::TempDir;

#[test]
fn show_log_patch_text_rejected_with_json() {
    let grit = env!("CARGO_BIN_EXE_grit");
    for args in [
        &["show", "--patch-text", "--json"][..],
        &["log", "-n1", "--patch-text", "--json"][..],
    ] {
        let out = Command::new(grit).args(args).output().expect("spawn grit");
        assert!(!out.status.success(), "expected failure for {args:?}");
        let msg = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            msg.contains("--patch-text") && msg.contains("--json"),
            "expected clear error for {args:?}, got: {msg}"
        );
    }
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

fn init_two_commit_repo(dir: &std::path::Path) {
    git_in(dir, &["init", "-b", "main"]);
    git_in(dir, &["config", "user.email", "t@example.com"]);
    git_in(dir, &["config", "user.name", "Test"]);
    fs::write(dir.join("a.txt"), "a\nb\n").expect("write");
    git_in(dir, &["add", "a.txt"]);
    git_in(dir, &["commit", "-m", "one"]);
    fs::write(dir.join("a.txt"), "a\nB\n").expect("write");
    git_in(dir, &["add", "a.txt"]);
    git_in(dir, &["commit", "-m", "two"]);
}

fn grit_patch_stdout(dir: &std::path::Path, args: &[&str]) -> Vec<u8> {
    let grit = env!("CARGO_BIN_EXE_grit");
    let out = Command::new(grit)
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn grit");
    assert!(
        out.status.success(),
        "grit {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn git_apply_check_reverse(dir: &std::path::Path, patch: &[u8]) {
    let check = Command::new("git")
        .current_dir(dir)
        .args(["apply", "--check", "-R"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn git apply");
    use std::io::Write;
    let mut child = check;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(patch).expect("stdin");
    }
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "git apply --check -R failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn show_patch_text_git_apply_check() {
    let dir = TempDir::new().expect("tempdir");
    init_two_commit_repo(dir.path());
    let patch = grit_patch_stdout(dir.path(), &["show", "--patch-text", "HEAD"]);
    let text = String::from_utf8_lossy(&patch);
    assert!(
        text.contains("diff --git") && text.contains("index "),
        "expected git-apply patch, got:\n{text}"
    );
    git_apply_check_reverse(dir.path(), &patch);
}

#[test]
fn log_patch_text_git_apply_check() {
    let dir = TempDir::new().expect("tempdir");
    init_two_commit_repo(dir.path());
    let patch = grit_patch_stdout(dir.path(), &["log", "-n1", "--patch-text"]);
    let text = String::from_utf8_lossy(&patch);
    assert!(
        text.contains("diff --git") && text.contains("index "),
        "expected git-apply patch, got:\n{text}"
    );
    git_apply_check_reverse(dir.path(), &patch);
}

#[test]
fn diff_patch_text_still_git_apply_check() {
    let dir = TempDir::new().expect("tempdir");
    init_two_commit_repo(dir.path());
    let patch = grit_patch_stdout(dir.path(), &["diff", "HEAD~1..HEAD", "--patch-text"]);
    git_apply_check_reverse(dir.path(), &patch);
}

fn line_index(text: &str, needle: &str) -> Option<usize> {
    text.lines().position(|line| line.contains(needle))
}

/// Non-TTY stdout (pipe capture) must keep ref decoration before the streamed patch.
#[test]
fn show_patch_text_redirected_branch_before_patch() {
    let dir = TempDir::new().expect("tempdir");
    init_two_commit_repo(dir.path());
    let bytes = grit_patch_stdout(dir.path(), &["show", "--patch-text", "main"]);
    let out = String::from_utf8_lossy(&bytes);
    let branch = line_index(&out, "branch main").expect("branch main line");
    let commit = line_index(&out, "commit ").expect("commit line");
    let diff = line_index(&out, "diff --git").expect("diff --git line");
    assert!(
        branch < commit && commit < diff,
        "expected branch, commit, then patch; got:\n{out}"
    );
}

#[test]
fn show_patch_text_redirected_annotated_tag_before_patch() {
    let dir = TempDir::new().expect("tempdir");
    init_two_commit_repo(dir.path());
    git_in(dir.path(), &["tag", "-a", "v1", "-m", "release note"]);
    let bytes = grit_patch_stdout(dir.path(), &["show", "--patch-text", "v1"]);
    let out = String::from_utf8_lossy(&bytes);
    let tag = line_index(&out, "tag v1").expect("tag v1 line");
    let commit = line_index(&out, "commit ").expect("commit line");
    let diff = line_index(&out, "diff --git").expect("diff --git line");
    assert!(
        tag < commit && commit < diff,
        "expected tag, commit, then patch; got:\n{out}"
    );
}
