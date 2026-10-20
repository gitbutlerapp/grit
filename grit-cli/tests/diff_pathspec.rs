//! Path limits on commit, range, and staged diffs (`grit diff A B -- path`).

use std::process::Command;

fn grit(dir: &std::path::Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_grit"))
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .args(args)
        .output()
        .expect("run grit");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn init_repo_with_two_files(dir: &std::path::Path) {
    assert_eq!(grit(dir, &["init"]).0, 0);
    assert_eq!(grit(dir, &["config", "user.name", "Test"]).0, 0);
    assert_eq!(grit(dir, &["config", "user.email", "t@example.com"]).0, 0);
    std::fs::write(dir.join("a.txt"), "base a\n").unwrap();
    std::fs::write(dir.join("b.txt"), "base b\n").unwrap();
    assert_eq!(grit(dir, &["add", "a.txt", "b.txt"]).0, 0);
    assert_eq!(grit(dir, &["commit", "-m", "initial"]).0, 0);
    assert_eq!(grit(dir, &["switch", "-c", "feature/v1.2/x"]).0, 0);
    std::fs::write(dir.join("a.txt"), "feature a\n").unwrap();
    std::fs::write(dir.join("b.txt"), "feature b\n").unwrap();
    assert_eq!(grit(dir, &["add", "a.txt", "b.txt"]).0, 0);
    assert_eq!(grit(dir, &["commit", "-m", "feature"]).0, 0);
}

#[test]
fn diff_two_refs_with_double_dash_path_limit() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo_with_two_files(dir.path());

    let (code, stdout, stderr) = grit(
        dir.path(),
        &["diff", "main", "feature/v1.2/x", "--", "a.txt"],
    );
    assert_eq!(code, 0, "stderr={stderr}");
    assert!(stdout.contains("a.txt"), "stdout={stdout}");
    assert!(!stdout.contains("b.txt"), "stdout={stdout}");
}

#[test]
fn diff_range_with_trailing_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo_with_two_files(dir.path());

    let (code, stdout, stderr) = grit(dir.path(), &["diff", "main..feature/v1.2/x", "a.txt"]);
    assert_eq!(code, 0, "stderr={stderr}");
    assert!(stdout.contains("a.txt"), "stdout={stdout}");
    assert!(!stdout.contains("b.txt"), "stdout={stdout}");
}

#[test]
fn diff_staged_with_path_limit() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo_with_two_files(dir.path());
    assert_eq!(grit(dir.path(), &["switch", "main"]).0, 0);
    std::fs::write(dir.path().join("a.txt"), "staged a\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "staged b\n").unwrap();
    assert_eq!(grit(dir.path(), &["add", "a.txt", "b.txt"]).0, 0);

    let (code, stdout, stderr) = grit(dir.path(), &["diff", "--staged", "--", "a.txt"]);
    assert_eq!(code, 0, "stderr={stderr}");
    assert!(stdout.contains("a.txt"), "stdout={stdout}");
    assert!(!stdout.contains("b.txt"), "stdout={stdout}");
}

#[test]
fn diff_two_refs_json_path_limit() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo_with_two_files(dir.path());

    let (code, stdout, stderr) = grit(
        dir.path(),
        &["diff", "--json", "main", "feature/v1.2/x", "--", "a.txt"],
    );
    assert_eq!(code, 0, "stderr={stderr}");
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    let paths: Vec<_> = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["path"].as_str())
        .collect();
    assert_eq!(paths, ["a.txt"]);
}

fn git(dir: &std::path::Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(args)
        .output()
        .expect("run git");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn init_repo_with_deletion_on_feature(dir: &std::path::Path) {
    assert_eq!(grit(dir, &["init"]).0, 0);
    assert_eq!(grit(dir, &["config", "user.name", "Test"]).0, 0);
    assert_eq!(grit(dir, &["config", "user.email", "t@example.com"]).0, 0);
    std::fs::write(dir.join("old.txt"), "gone soon\n").unwrap();
    std::fs::write(dir.join("keep.txt"), "stay\n").unwrap();
    assert_eq!(grit(dir, &["add", "old.txt", "keep.txt"]).0, 0);
    assert_eq!(grit(dir, &["commit", "-m", "main"]).0, 0);
    assert_eq!(grit(dir, &["switch", "-c", "feature"]).0, 0);
    std::fs::remove_file(dir.join("old.txt")).unwrap();
    std::fs::write(dir.join("keep.txt"), "changed\n").unwrap();
    assert_eq!(git(dir, &["add", "-A"]).0, 0);
    assert_eq!(grit(dir, &["commit", "-m", "delete old"]).0, 0);
}

#[test]
fn diff_two_refs_deleted_path_not_in_current_tree() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo_with_deletion_on_feature(dir.path());

    let (g_code, g_out, g_err) = git(dir.path(), &["diff", "main", "feature", "--", "old.txt"]);
    assert_eq!(g_code, 0, "git stderr={g_err}");
    assert!(g_out.contains("old.txt"), "git stdout={g_out}");

    let (code, stdout, stderr) = grit(dir.path(), &["diff", "main", "feature", "--", "old.txt"]);
    assert_eq!(code, 0, "stderr={stderr}");
    assert!(stdout.contains("old.txt"), "stdout={stdout}");
}

#[test]
fn diff_two_refs_pathspec_exclude_magic() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo_with_two_files(dir.path());

    let (g_code, _, g_err) = git(
        dir.path(),
        &[
            "diff",
            "main",
            "feature/v1.2/x",
            "--",
            ".",
            ":(exclude)a.txt",
        ],
    );
    assert_eq!(g_code, 0, "git stderr={g_err}");

    let (code, stdout, stderr) = grit(
        dir.path(),
        &[
            "diff",
            "main",
            "feature/v1.2/x",
            "--",
            ".",
            ":(exclude)a.txt",
        ],
    );
    assert_eq!(code, 0, "stderr={stderr}");
    assert!(stdout.contains("b.txt"), "stdout={stdout}");
    assert!(!stdout.contains("a.txt"), "stdout={stdout}");
}

#[test]
fn diff_two_refs_bare_repository() {
    let src = tempfile::tempdir().expect("tempdir");
    init_repo_with_deletion_on_feature(src.path());

    let bare = tempfile::tempdir().expect("bare tempdir");
    assert_eq!(
        git(
            src.path(),
            &[
                "clone",
                "--bare",
                src.path().to_str().unwrap(),
                bare.path().join("repo.git").to_str().unwrap(),
            ],
        )
        .0,
        0
    );
    let bare_root = bare.path().join("repo.git");

    let (g_code, g_out, g_err) = git(&bare_root, &["diff", "main", "feature", "--", "old.txt"]);
    assert_eq!(g_code, 0, "git stderr={g_err}");
    assert!(g_out.contains("old.txt"), "git stdout={g_out}");

    let (code, stdout, stderr) = grit(&bare_root, &["diff", "main", "feature", "--", "old.txt"]);
    assert_eq!(code, 0, "stderr={stderr}");
    assert!(stdout.contains("old.txt"), "stdout={stdout}");
}

#[test]
fn diff_revision_then_bare_path_without_double_dash() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo_with_two_files(dir.path());

    let git = |args: &[&str]| {
        Command::new("git")
            .current_dir(dir.path())
            .args(args)
            .output()
            .expect("git")
    };
    let git_out = git(&["diff", "feature/v1.2/x", "a.txt"]);
    assert!(git_out.status.success(), "git stderr={}", String::from_utf8_lossy(&git_out.stderr));

    let (code, stdout, stderr) = grit(dir.path(), &["diff", "feature/v1.2/x", "a.txt"]);
    assert_eq!(code, 0, "stderr={stderr}");
    assert_eq!(
        stdout,
        String::from_utf8_lossy(&git_out.stdout),
        "grit should match git for rev + path without `--`"
    );
}
