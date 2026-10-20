//! CLI tests for `grit diff --stat`, `--numstat`, `--name-only`, `--name-status`.

use grit_test_support::cli_fixture::CliFixture;
use grit_test_support::{git, grit};

#[test]
fn diff_detail_flags_conflict() {
    let fx = CliFixture::new();
    let out = grit(fx.path(), &["diff", "--stat", "--name-only"]);
    assert!(!out.ok());
    assert!(
        out.stderr.contains("cannot be used with") || out.stderr.contains("conflict"),
        "{}",
        out.dump("flags conflict")
    );
}

#[test]
fn diff_name_only_matches_git_bytes() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("f.txt"), b"a\n").expect("write");
    git(dir.path(), &["init"]);
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-m", "init"]);
    std::fs::write(dir.path().join("f.txt"), b"b\n").expect("write");
    let git_out = std::process::Command::new("git")
        .current_dir(dir.path())
        .args(["diff", "--name-only"])
        .output()
        .expect("git diff");
    let grit_init = grit(dir.path(), &["init"]);
    assert!(grit_init.ok(), "{}", grit_init.dump("grit init"));
    let grit_out = grit(dir.path(), &["diff", "--name-only"]);
    assert_eq!(
        String::from_utf8_lossy(&git_out.stdout),
        grit_out.stdout,
        "{}",
        grit_out.dump("grit diff --name-only")
    );
}

#[test]
fn diff_numstat_unstaged_rename_matches_git_bytes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let grit_init = grit(dir.path(), &["init"]);
    assert!(grit_init.ok(), "{}", grit_init.dump("grit init"));
    std::fs::write(dir.path().join("f.txt"), b"line\n").expect("write");
    assert!(grit(dir.path(), &["add", "."]).ok(), "grit add");
    assert!(
        grit(dir.path(), &["commit", "-m", "init"]).ok(),
        "grit commit"
    );
    git(dir.path(), &["mv", "f.txt", "g.txt"]);
    std::fs::write(dir.path().join("g.txt"), b"line\nextra\n").expect("write");
    let git_out = std::process::Command::new("git")
        .current_dir(dir.path())
        .args(["diff", "--numstat"])
        .output()
        .expect("git diff");
    let grit_out = grit(dir.path(), &["diff", "--unstaged", "--numstat"]);
    assert_eq!(
        String::from_utf8_lossy(&git_out.stdout),
        grit_out.stdout,
        "{}",
        grit_out.dump("grit diff --unstaged --numstat rename")
    );
}

#[test]
fn diff_numstat_default_lists_same_paths_as_default_patch() {
    let dir = tempfile::tempdir().expect("tempdir");
    let grit_init = grit(dir.path(), &["init"]);
    assert!(grit_init.ok(), "{}", grit_init.dump("grit init"));
    std::fs::write(dir.path().join("f.txt"), b"line\n").expect("write");
    assert!(grit(dir.path(), &["add", "."]).ok(), "grit add");
    assert!(
        grit(dir.path(), &["commit", "-m", "init"]).ok(),
        "grit commit"
    );
    git(dir.path(), &["mv", "f.txt", "g.txt"]);
    std::fs::write(dir.path().join("g.txt"), b"line\nextra\n").expect("write");

    let patch_json = grit(dir.path(), &["--json", "diff"]);
    assert!(patch_json.ok(), "{}", patch_json.dump("patch json"));
    let stat_json = grit(dir.path(), &["--json", "diff", "--stat"]);
    assert!(stat_json.ok(), "{}", stat_json.dump("stat json"));
    let patch: serde_json::Value = serde_json::from_str(&patch_json.stdout).expect("patch json");
    let stat: serde_json::Value = serde_json::from_str(&stat_json.stdout).expect("stat json");
    let patch_paths: Vec<String> = patch["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|f| f["path"].as_str().expect("path").to_owned())
        .collect();
    let stat_paths: Vec<String> = stat["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|f| f["path"].as_str().expect("path").to_owned())
        .collect();
    assert_eq!(patch_paths, stat_paths, "default patch vs --stat file sets");
}

#[test]
fn diff_stat_empty_is_zero_bytes() {
    let fx = CliFixture::empty();
    for args in [
        &["diff", "--numstat"][..],
        &["diff", "--name-only"][..],
        &["diff", "--name-status"][..],
        &["diff", "--shortstat"][..],
        &["diff", "--stat"][..],
    ] {
        let out = grit(fx.path(), args);
        assert!(out.ok(), "{}: {}", args.join(" "), out.dump("empty diff"));
        assert!(
            out.stdout.is_empty(),
            "{} should emit no stdout when clean: {:?}",
            args.join(" "),
            out.stdout
        );
    }
}

#[test]
fn diff_head_stat_matches_git_worktree() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init"]);
    std::fs::write(dir.path().join("f.txt"), b"v1\n").expect("write");
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-m", "c1"]);
    std::fs::write(dir.path().join("f.txt"), b"v2\n").expect("write");
    git(dir.path(), &["add", "f.txt"]);
    std::fs::write(dir.path().join("f.txt"), b"v3\n").expect("write");
    let git_out = std::process::Command::new("git")
        .current_dir(dir.path())
        .args(["diff", "--numstat", "HEAD"])
        .output()
        .expect("git diff");
    let grit_out = grit(dir.path(), &["diff", "--numstat", "HEAD"]);
    assert_eq!(
        String::from_utf8_lossy(&git_out.stdout),
        grit_out.stdout,
        "{}",
        grit_out.dump("grit diff --numstat HEAD")
    );
}

#[test]
fn diff_against_stat_honors_ref() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init"]);
    std::fs::write(dir.path().join("f.txt"), b"base\n").expect("write");
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-m", "base"]);
    std::fs::write(dir.path().join("f.txt"), b"tip\n").expect("write");
    git(dir.path(), &["commit", "-am", "tip"]);
    std::fs::write(dir.path().join("f.txt"), b"work\n").expect("write");
    let git_out = std::process::Command::new("git")
        .current_dir(dir.path())
        .args(["diff", "--numstat", "HEAD~1"])
        .output()
        .expect("git");
    assert!(git_out.status.success());
    let grit_out = grit(dir.path(), &["diff", "--numstat", "--against", "HEAD~1"]);
    assert_eq!(
        String::from_utf8_lossy(&git_out.stdout),
        grit_out.stdout,
        "{}",
        grit_out.dump("grit diff --against")
    );
}

#[test]
fn diff_path_only_stays_unstaged_with_stat() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init"]);
    std::fs::write(dir.path().join("a.txt"), b"1\n").expect("write");
    std::fs::write(dir.path().join("b.txt"), b"2\n").expect("write");
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-m", "init"]);
    std::fs::write(dir.path().join("a.txt"), b"1\nstaged\n").expect("write");
    git(dir.path(), &["add", "a.txt"]);
    std::fs::write(dir.path().join("a.txt"), b"1\nstaged\nunstaged\n").expect("write");
    std::fs::write(dir.path().join("b.txt"), b"2\nchanged\n").expect("write");
    let git_out = std::process::Command::new("git")
        .current_dir(dir.path())
        .args(["diff", "--numstat", "--", "a.txt"])
        .output()
        .expect("git");
    let grit_out = grit(dir.path(), &["diff", "--numstat", "--", "a.txt"]);
    assert_eq!(
        String::from_utf8_lossy(&git_out.stdout),
        grit_out.stdout,
        "{}",
        grit_out.dump("path-only unstaged numstat")
    );
}

#[test]
fn diff_json_worktree_rename_keeps_similarity() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(grit(dir.path(), &["init"]).ok(), "init");
    std::fs::write(dir.path().join("old.txt"), b"line one\nline two\n").expect("write");
    assert!(grit(dir.path(), &["add", "."]).ok(), "add");
    assert!(grit(dir.path(), &["commit", "-m", "init"]).ok(), "commit");
    git(dir.path(), &["mv", "old.txt", "new.txt"]);
    std::fs::write(dir.path().join("new.txt"), b"line one\nline TWO\n").expect("write");
    let out = grit(dir.path(), &["--json", "diff"]);
    assert!(out.ok(), "{}", out.dump("json diff"));
    let v: serde_json::Value = serde_json::from_str(&out.stdout).expect("json");
    let files = v["files"].as_array().expect("files");
    assert_eq!(files.len(), 1, "{v}");
    assert_eq!(files[0]["status"], "renamed");
    assert!(
        files[0]["similarity"]
            .as_u64()
            .is_some_and(|s| s > 0 && s <= 100),
        "rename similarity missing in JSON: {}",
        files[0]
    );
}

#[test]
fn diff_name_only_path_limit() {
    let fx = CliFixture::new();
    std::fs::write(fx.path().join("only.txt"), b"x\n").expect("write");
    std::fs::write(fx.path().join("other.txt"), b"y\n").expect("write");
    git(fx.path(), &["add", "only.txt", "other.txt"]);
    git(fx.path(), &["commit", "-m", "add only and other"]);
    std::fs::write(fx.path().join("only.txt"), b"xx\n").expect("write");
    std::fs::write(fx.path().join("other.txt"), b"yy\n").expect("write");
    let out = grit(fx.path(), &["diff", "only.txt", "--name-only"]);
    assert!(out.ok(), "{}", out.dump("path limit"));
    let lines: Vec<_> = out.stdout.lines().collect();
    assert_eq!(lines, ["only.txt"], "stdout={:?}", out.stdout);
}
