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

fn init_multi_file_repo(dir: &std::path::Path) {
    git_in(dir, &["init", "-b", "main"]);
    git_in(dir, &["config", "user.email", "t@example.com"]);
    git_in(dir, &["config", "user.name", "Test"]);
    fs::write(dir.join("a.txt"), "a\n").expect("write");
    fs::write(dir.join("b.txt"), "b\n").expect("write");
    fs::write(dir.join("bin.dat"), &[0u8, 1, 2]).expect("write");
    git_in(dir, &["add", "."]);
    git_in(dir, &["commit", "-m", "base"]);
    fs::write(dir.join("a.txt"), "A\n").expect("write");
    fs::write(dir.join("b.txt"), "B\n").expect("write");
    fs::write(dir.join("bin.dat"), &[0u8, 1, 2, 3]).expect("write");
    git_in(dir, &["add", "."]);
    git_in(dir, &["commit", "-m", "change all"]);
}

#[test]
fn show_log_patch_text_respects_pathspec() {
    let dir = TempDir::new().expect("tempdir");
    init_multi_file_repo(dir.path());
    for args in [
        &["show", "--patch-text", "HEAD", "--", "a.txt"][..],
        &["log", "-n1", "--patch-text", "--", "a.txt"][..],
    ] {
        let patch = grit_patch_stdout(dir.path(), args);
        let text = String::from_utf8_lossy(&patch);
        assert!(
            text.contains("a.txt") && text.contains("diff --git"),
            "expected a.txt patch in {args:?}, got:\n{text}"
        );
        assert!(
            !text.contains("b.txt") && !text.contains("bin.dat"),
            "pathspec should exclude other files for {args:?}, got:\n{text}"
        );
    }
}

#[test]
fn show_patch_text_merge_first_parent_git_apply_check() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    git_in(root, &["init", "-b", "main"]);
    git_in(root, &["config", "user.email", "t@example.com"]);
    git_in(root, &["config", "user.name", "Test"]);
    fs::write(root.join("base.txt"), "base\n").expect("write");
    git_in(root, &["add", "base.txt"]);
    git_in(root, &["commit", "-m", "main tip"]);
    git_in(root, &["switch", "-c", "side"]);
    fs::write(root.join("side.txt"), "side\n").expect("write");
    git_in(root, &["add", "side.txt"]);
    git_in(root, &["commit", "-m", "side tip"]);
    git_in(root, &["switch", "main"]);
    fs::write(root.join("main.txt"), "main\n").expect("write");
    git_in(root, &["add", "main.txt"]);
    git_in(root, &["commit", "-m", "main line"]);
    git_in(root, &["merge", "--no-edit", "side"]);
    git_in(root, &["rev-parse", "HEAD^2"]);

    let patch = grit_patch_stdout(
        root,
        &[
            "show",
            "--patch-text",
            "--diff-merges",
            "first-parent",
            "HEAD",
        ],
    );
    let text = String::from_utf8_lossy(&patch);
    assert!(
        text.contains("diff --git") && text.contains("side.txt") && !text.contains("main.txt"),
        "expected first-parent merge patch, got:\n{text}"
    );
    git_apply_check_reverse(dir.path(), &patch);
}

#[test]
fn show_log_patch_text_rename_matches_source_pathspec() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    git_in(root, &["init", "-b", "main"]);
    git_in(root, &["config", "user.email", "t@example.com"]);
    git_in(root, &["config", "user.name", "Test"]);
    fs::write(root.join("old.txt"), "content\n").expect("write");
    git_in(root, &["add", "old.txt"]);
    git_in(root, &["commit", "-m", "add old"]);
    git_in(root, &["mv", "old.txt", "new.txt"]);
    git_in(root, &["commit", "-m", "rename"]);

    for args in [
        &["show", "--patch-text", "HEAD", "--", "old.txt"][..],
        &["log", "-n1", "--patch-text", "--", "old.txt"][..],
    ] {
        let patch = grit_patch_stdout(root, args);
        let text = String::from_utf8_lossy(&patch);
        assert_eq!(
            text.matches("diff --git").count(),
            1,
            "expected one file patch for source pathspec {args:?}, got:\n{text}"
        );
        assert!(
            text.contains("old.txt") && text.contains("new.txt"),
            "expected rename in patch for {args:?}, got:\n{text}"
        );
        git_apply_check_reverse(root, &patch);
    }
}

#[test]
fn show_patch_text_binary_git_apply_check() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    git_in(root, &["init", "-b", "main"]);
    git_in(root, &["config", "user.email", "t@example.com"]);
    git_in(root, &["config", "user.name", "Test"]);
    fs::write(root.join("bin.dat"), &[0u8, 1, 2]).expect("write");
    git_in(root, &["add", "bin.dat"]);
    git_in(root, &["commit", "-m", "add binary"]);
    fs::write(root.join("bin.dat"), &[0u8, 1, 2, 3, 4]).expect("write");
    git_in(dir.path(), &["add", "bin.dat"]);
    git_in(dir.path(), &["commit", "-m", "change binary"]);
    let patch = grit_patch_stdout(dir.path(), &["show", "--patch-text", "HEAD"]);
    let text = String::from_utf8_lossy(&patch);
    assert!(
        text.contains("GIT binary patch") || text.contains("index "),
        "expected binary patch section, got:\n{text}"
    );
    git_apply_check_reverse(dir.path(), &patch);
}

fn grit_patch_redirected_stdout(dir: &std::path::Path, args: &[&str]) -> Vec<u8> {
    let file = tempfile::NamedTempFile::new().expect("output file");
    let out = Command::new(env!("CARGO_BIN_EXE_grit"))
        .current_dir(dir)
        .args(args)
        .stdout(file.reopen().expect("reopen output file"))
        .output()
        .expect("spawn grit");
    assert!(
        out.status.success(),
        "grit {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    fs::read(file.path()).expect("read redirected output")
}

fn line_index(text: &str, needle: &str) -> Option<usize> {
    text.lines().position(|line| line.contains(needle))
}

/// Non-TTY stdout (pipe capture) must keep ref decoration before the streamed patch.
#[test]
fn show_patch_text_redirected_branch_before_patch() {
    let dir = TempDir::new().expect("tempdir");
    init_two_commit_repo(dir.path());
    let args = &["show", "--patch-text", "main"];
    for bytes in [
        grit_patch_stdout(dir.path(), args),
        grit_patch_redirected_stdout(dir.path(), args),
    ] {
        let out = String::from_utf8_lossy(&bytes);
        let branch = line_index(&out, "branch main").expect("branch main line");
        let commit = line_index(&out, "commit ").expect("commit line");
        let diff = line_index(&out, "diff --git").expect("diff --git line");
        assert!(
            branch < commit && commit < diff,
            "expected branch, commit, then patch; got:\n{out}"
        );
    }
}

#[test]
fn show_patch_text_redirected_annotated_tag_before_patch() {
    let dir = TempDir::new().expect("tempdir");
    init_two_commit_repo(dir.path());
    git_in(dir.path(), &["tag", "-a", "v1", "-m", "release note"]);
    let args = &["show", "--patch-text", "v1"];
    for bytes in [
        grit_patch_stdout(dir.path(), args),
        grit_patch_redirected_stdout(dir.path(), args),
    ] {
        let out = String::from_utf8_lossy(&bytes);
        let tag = line_index(&out, "tag v1").expect("tag v1 line");
        let commit = line_index(&out, "commit ").expect("commit line");
        let diff = line_index(&out, "diff --git").expect("diff --git line");
        assert!(
            tag < commit && commit < diff,
            "expected tag, commit, then patch; got:\n{out}"
        );
    }
}

#[test]
fn log_patch_text_continuation_follows_patch_and_resumes() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    init_two_commit_repo(root);
    for i in 3..=22 {
        fs::write(root.join("a.txt"), format!("commit {i}\n")).expect("write");
        git_in(root, &["add", "a.txt"]);
        git_in(root, &["commit", "-m", &format!("commit {i}")]);
    }

    let mut args = vec![
        "log".to_owned(),
        "--patch-text".to_owned(),
        "--cursor".to_owned(),
    ];
    for expected in [10, 10, 2] {
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let bytes = grit_patch_stdout(root, &arg_refs);
        let out = String::from_utf8_lossy(&bytes);
        assert_eq!(
            out.lines()
                .filter(|line| line.starts_with("commit "))
                .count(),
            expected,
            "unexpected page size:\n{out}"
        );
        let continuation = out
            .lines()
            .find_map(|line| line.strip_prefix("→ more: grit log --cursor="));
        if expected == 2 {
            assert!(
                continuation.is_none(),
                "last page has no continuation:\n{out}"
            );
            break;
        }
        let cursor = continuation.expect("patch page continuation");
        assert!(
            cursor.starts_with("lw1.s."),
            "expected short cursor: {cursor}"
        );
        let last_diff = out
            .lines()
            .enumerate()
            .filter(|(_, line)| line.starts_with("diff --git"))
            .map(|(i, _)| i)
            .last()
            .expect("patch on page");
        assert!(
            line_index(&out, "→ more:").expect("continuation line") > last_diff,
            "continuation must follow all patches:\n{out}"
        );
        args = vec![
            "log".to_owned(),
            "--patch-text".to_owned(),
            format!("--cursor={cursor}"),
        ];
    }
}
