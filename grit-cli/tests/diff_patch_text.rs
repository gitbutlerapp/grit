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

#[test]
fn diff_patch_text_quiet_emits_no_stdout() {
    let dir = TempDir::new().expect("tempdir");
    git_in(dir.path(), &["init"]);
    git_in(dir.path(), &["config", "user.email", "t@example.com"]);
    git_in(dir.path(), &["config", "user.name", "Test"]);
    fs::write(dir.path().join("file.txt"), "one\n").expect("write");
    git_in(dir.path(), &["add", "file.txt"]);
    git_in(dir.path(), &["commit", "-m", "init"]);

    fs::write(dir.path().join("file.txt"), "two\n").expect("write");

    let grit = env!("CARGO_BIN_EXE_grit");
    let out = Command::new(grit)
        .current_dir(dir.path())
        .args(["diff", "--patch-text", "--quiet"])
        .output()
        .expect("spawn grit");
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected exit 1 when worktree differs: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "expected no stdout with --quiet, got {} bytes",
        out.stdout.len()
    );
}

fn git_stdout(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn grit_patch_text(dir: &std::path::Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new(env!("CARGO_BIN_EXE_grit"))
        .current_dir(dir)
        .arg("diff")
        .arg("--patch-text")
        .args(args)
        .output()
        .expect("spawn grit");
    assert!(
        out.status.success(),
        "grit diff --patch-text {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// Initialize a repository at `dir` with deterministic identity settings.
fn init_repo(dir: &std::path::Path) {
    git_in(dir, &["init", "-q", "-b", "main"]);
    git_in(dir, &["config", "user.email", "t@example.com"]);
    git_in(dir, &["config", "user.name", "Test"]);
}

/// Apply `patch` with `git apply --index` in a fresh clone of `src` at `base` (or an empty
/// repository when `base` is `None`) and return the tree `git write-tree` records.
fn git_apply_index_tree(src: &std::path::Path, base: Option<&str>, patch: &[u8]) -> String {
    let scratch = TempDir::new().expect("tempdir");
    let dst = scratch.path().join("dst");
    match base {
        Some(rev) => {
            git_in(
                scratch.path(),
                &["clone", "-q", src.to_str().expect("utf8"), "dst"],
            );
            git_in(&dst, &["checkout", "-q", "--detach", rev]);
        }
        None => {
            fs::create_dir(&dst).expect("mkdir");
            init_repo(&dst);
        }
    }
    let patch_path = scratch.path().join("change.patch");
    fs::write(&patch_path, patch).expect("write patch");
    git_in(
        &dst,
        &["apply", "--index", patch_path.to_str().expect("utf8")],
    );
    git_stdout(&dst, &["write-tree"])
}

fn assert_binary_literals_nonzero(patch: &[u8], path: &str) {
    let text = String::from_utf8_lossy(patch);
    if !text.contains(&format!("diff --git a/{path} b/{path}")) {
        return;
    }
    let in_file = text
        .split(&format!("diff --git a/{path} b/{path}"))
        .nth(1)
        .unwrap_or(&text);
    for line in in_file
        .lines()
        .take_while(|l| !l.starts_with("diff --git "))
    {
        if let Some(rest) = line.strip_prefix("literal ") {
            let size: usize = rest.parse().expect("literal size");
            assert!(size > 0, "literal 0 in binary patch for {path}:\n{text}");
        }
    }
}

fn assert_binary_patch(patch: &[u8], path: &str) {
    let text = String::from_utf8_lossy(patch);
    assert!(
        text.contains(&format!("diff --git a/{path} b/{path}"))
            && text.contains("GIT binary patch"),
        "expected a GIT binary patch for {path}, got:\n{text}"
    );
    let index_line = text
        .lines()
        .skip_while(|l| *l != format!("diff --git a/{path} b/{path}"))
        .find(|l| l.starts_with("index "))
        .unwrap_or_else(|| panic!("missing index line for {path}:\n{text}"));
    let ids = index_line["index ".len()..]
        .split(' ')
        .next()
        .expect("index range");
    let (old, new) = ids.split_once("..").expect("index range");
    assert!(
        old.len() == 40 && new.len() == 40,
        "binary patch needs full ids, got `{index_line}`"
    );
}

#[test]
fn diff_patch_text_staged_new_binary_applies_with_git() {
    let dir = TempDir::new().expect("tempdir");
    init_repo(dir.path());
    fs::write(dir.path().join("b.bin"), [0u8, 1, 2, 3]).expect("write");
    git_in(dir.path(), &["add", "."]);

    let patch = grit_patch_text(dir.path(), &["--staged"]);
    assert_binary_patch(&patch, "b.bin");
    let want = git_stdout(dir.path(), &["write-tree"]);
    assert_eq!(git_apply_index_tree(dir.path(), None, &patch), want);
}

#[test]
fn diff_patch_text_staged_binary_and_text_apply_with_git() {
    let dir = TempDir::new().expect("tempdir");
    init_repo(dir.path());
    fs::write(dir.path().join("b.bin"), [0u8, 1, 2, 3]).expect("write");
    fs::write(dir.path().join("gone.bin"), [0u8, 9, 9]).expect("write");
    fs::write(dir.path().join("t.txt"), "one\n").expect("write");
    git_in(dir.path(), &["add", "."]);
    git_in(dir.path(), &["commit", "-q", "-m", "init"]);

    fs::write(dir.path().join("b.bin"), [0u8, 1, 2, 4, 5, 6]).expect("write");
    fs::write(dir.path().join("t.txt"), "one\ntwo\n").expect("write");
    fs::remove_file(dir.path().join("gone.bin")).expect("rm");
    git_in(dir.path(), &["add", "-A"]);

    let patch = grit_patch_text(dir.path(), &["--staged"]);
    assert_binary_patch(&patch, "b.bin");
    assert_binary_patch(&patch, "gone.bin");
    let want = git_stdout(dir.path(), &["write-tree"]);
    assert_eq!(git_apply_index_tree(dir.path(), Some("HEAD"), &patch), want);
}

#[test]
fn diff_patch_text_worktree_binary_applies_with_git() {
    let dir = TempDir::new().expect("tempdir");
    init_repo(dir.path());
    fs::write(dir.path().join("b.bin"), [0u8, 1, 2, 3]).expect("write");
    fs::write(dir.path().join("gone.bin"), [0u8, 9, 9]).expect("write");
    fs::write(dir.path().join("t.txt"), "one\n").expect("write");
    git_in(dir.path(), &["add", "."]);
    git_in(dir.path(), &["commit", "-q", "-m", "init"]);

    // Size changes take the status fast path, which records no worktree blob id.
    fs::write(dir.path().join("b.bin"), [0u8, 1, 2, 4, 5, 6, 7]).expect("write");
    fs::write(dir.path().join("t.txt"), "one\ntwo\n").expect("write");
    fs::remove_file(dir.path().join("gone.bin")).expect("rm");

    for args in [&["--unstaged"][..], &[][..]] {
        let patch = grit_patch_text(dir.path(), args);
        assert_binary_patch(&patch, "b.bin");
        assert_binary_patch(&patch, "gone.bin");
        let got = git_apply_index_tree(dir.path(), Some("HEAD"), &patch);
        let index = dir.path().join(".git").join("scratch-index");
        let want = {
            let out = Command::new("git")
                .current_dir(dir.path())
                .env("GIT_INDEX_FILE", &index)
                .args(["read-tree", "HEAD"])
                .status()
                .expect("read-tree");
            assert!(out.success());
            let out = Command::new("git")
                .current_dir(dir.path())
                .env("GIT_INDEX_FILE", &index)
                .args(["add", "-A"])
                .status()
                .expect("add");
            assert!(out.success());
            let out = Command::new("git")
                .current_dir(dir.path())
                .env("GIT_INDEX_FILE", &index)
                .arg("write-tree")
                .output()
                .expect("write-tree");
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        fs::remove_file(&index).expect("rm scratch index");
        assert_eq!(
            got, want,
            "git apply --index of grit diff --patch-text {args:?}"
        );
    }
}

#[test]
fn diff_patch_text_large_binary_commit_range_applies_from_preimage_only() {
    let dir = TempDir::new().expect("tempdir");
    init_repo(dir.path());
    let mut v1 = vec![0u8; 100_000];
    for (i, b) in v1.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    v1[1000] = 0;
    fs::write(dir.path().join("x.bin"), &v1).expect("write");
    git_in(dir.path(), &["add", "x.bin"]);
    git_in(dir.path(), &["commit", "-q", "-m", "one"]);
    let mut v2 = v1.clone();
    v2[5000] = v2[5000].wrapping_add(1);
    fs::write(dir.path().join("x.bin"), &v2).expect("write");
    git_in(dir.path(), &["add", "x.bin"]);
    git_in(dir.path(), &["commit", "-q", "-m", "two"]);

    let patch = grit_patch_text(dir.path(), &["HEAD~1..HEAD"]);
    assert_binary_patch(&patch, "x.bin");
    assert_binary_literals_nonzero(&patch, "x.bin");
    let want = git_stdout(dir.path(), &["rev-parse", "HEAD^{tree}"]);
    assert_eq!(
        git_apply_index_tree(dir.path(), Some("HEAD~1"), &patch),
        want
    );
}

#[test]
fn diff_patch_text_commit_range_binary_applies_with_git() {
    let dir = TempDir::new().expect("tempdir");
    init_repo(dir.path());
    fs::write(dir.path().join("b.bin"), [0u8, 1, 2, 3]).expect("write");
    git_in(dir.path(), &["add", "."]);
    git_in(dir.path(), &["commit", "-q", "-m", "init"]);
    fs::write(dir.path().join("b.bin"), [0u8, 1, 2, 4, 5]).expect("write");
    fs::write(dir.path().join("t.txt"), "text\n").expect("write");
    git_in(dir.path(), &["add", "."]);
    git_in(dir.path(), &["commit", "-q", "-m", "edit"]);

    let patch = grit_patch_text(dir.path(), &["HEAD~1..HEAD"]);
    assert_binary_patch(&patch, "b.bin");
    let want = git_stdout(dir.path(), &["rev-parse", "HEAD^{tree}"]);
    assert_eq!(
        git_apply_index_tree(dir.path(), Some("HEAD~1"), &patch),
        want
    );
}

#[test]
fn diff_patch_text_no_index_binary_emits_git_binary_patch() {
    let dir = TempDir::new().expect("tempdir");
    fs::create_dir(dir.path().join("a")).expect("mkdir");
    fs::create_dir(dir.path().join("b")).expect("mkdir");
    fs::write(dir.path().join("a").join("b.bin"), [0u8, 1, 2, 3]).expect("write");
    fs::write(dir.path().join("b").join("b.bin"), [0u8, 1, 2, 4, 5]).expect("write");

    let out = Command::new(env!("CARGO_BIN_EXE_grit"))
        .current_dir(dir.path())
        .args(["diff", "--no-index", "--patch-text", "a", "b"])
        .output()
        .expect("spawn grit");
    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("diff --git a/a/b.bin b/b/b.bin") && text.contains("GIT binary patch"),
        "expected a GIT binary patch, got:\n{text}"
    );
    assert!(
        text.contains(
            "index eaf36c1daccfdf325514461cd1a2ffbc139b5464..\
             cf408f3b73c016dff811028f082e3e2277146661 100644"
        ),
        "expected full index ids, got:\n{text}"
    );
}

#[test]
fn diff_patch_text_no_index_emits_patch() {
    let dir = TempDir::new().expect("tempdir");
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    fs::write(&a, "left\n").expect("write a");
    fs::write(&b, "right\n").expect("write b");

    let grit = env!("CARGO_BIN_EXE_grit");
    let out = Command::new(grit)
        .args([
            "diff",
            "--no-index",
            "--patch-text",
            a.to_str().expect("utf8"),
            b.to_str().expect("utf8"),
        ])
        .output()
        .expect("spawn grit");
    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let patch = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.stdout.is_empty() && patch.contains("@@"),
        "expected patch bytes on stdout, got:\n{patch}"
    );
}

#[test]
fn patch_id_json_matches_git_and_normal_diff_on_ten_line_file() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = TempDir::new().unwrap();
    git_in(dir.path(), &["init", "-q", "-b", "main"]);
    fs::write(
        dir.path().join("file"),
        (1..=10).map(|i| format!("{i}\n")).collect::<String>(),
    )
    .unwrap();
    git_in(dir.path(), &["add", "file"]);
    git_in(dir.path(), &["commit", "-qm", "base"]);
    fs::write(
        dir.path().join("file"),
        (1..=9).map(|i| format!("{i}\n")).collect::<String>() + "changed\n",
    )
    .unwrap();
    git_in(dir.path(), &["commit", "-qam", "change"]);

    let grit = env!("CARGO_BIN_EXE_grit");
    let patch = Command::new(grit)
        .current_dir(dir.path())
        .args(["diff", "--patch-text", "HEAD"])
        .output()
        .unwrap();
    assert!(
        patch.status.success(),
        "{}",
        String::from_utf8_lossy(&patch.stderr)
    );
    let show = Command::new("git")
        .current_dir(dir.path())
        .args(["show", "--format=", "--no-ext-diff", "-U3", "HEAD"])
        .output()
        .unwrap();
    assert!(show.status.success());
    for text in [&show.stdout, &patch.stdout] {
        let mut child = Command::new("git")
            .current_dir(dir.path())
            .args(["patch-id", "--stable"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(text).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8(out.stdout)
                .unwrap()
                .split_whitespace()
                .next(),
            Some("5ab484ff6e8f56d89403fae49e2063b0712ed58e")
        );
    }
    let output = Command::new(grit)
        .current_dir(dir.path())
        .args(["patch-id", "HEAD", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["entries"].as_array().unwrap().len(), 1);
    assert_eq!(
        json["entries"][0]["patch_id"],
        "5ab484ff6e8f56d89403fae49e2063b0712ed58e"
    );
    git_in(dir.path(), &["fsck", "--strict"]);
}
