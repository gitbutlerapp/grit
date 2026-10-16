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
