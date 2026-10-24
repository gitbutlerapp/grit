//! CLI tests for `grit diff --stat`, `--numstat`, `--name-only`, `--name-status`.

use grit_test_support::cli_fixture::CliFixture;
use grit_test_support::{git, git_cmd, grit};

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
        .args(["diff", "HEAD", "--name-only"])
        .output()
        .expect("git diff HEAD");
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
fn diff_numstat_default_matches_against_head_after_rename() {
    let dir = tempfile::tempdir().expect("tempdir");
    let grit_init = grit(dir.path(), &["init"]);
    assert!(grit_init.ok(), "{}", grit_init.dump("grit init"));
    std::fs::write(dir.path().join("f.txt"), b"line\n").expect("write");
    assert!(grit(dir.path(), &["add", "."]).ok(), "grit add");
    assert!(
        grit(dir.path(), &["commit", "-m", "init"]).ok(),
        "grit commit"
    );
    std::fs::rename(dir.path().join("f.txt"), dir.path().join("g.txt")).expect("rename");
    std::fs::write(dir.path().join("g.txt"), b"line\nextra\n").expect("write");
    let default = grit(dir.path(), &["diff", "--numstat"]);
    assert!(default.ok(), "{}", default.dump("default numstat"));
    let against = grit(dir.path(), &["diff", "--against", "HEAD", "--numstat"]);
    assert!(against.ok(), "{}", against.dump("against HEAD numstat"));
    assert_eq!(
        default.stdout, against.stdout,
        "default numstat should match --against HEAD"
    );
    assert!(
        default.stdout.contains("g.txt"),
        "rename target should appear: {:?}",
        default.stdout
    );
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
fn diff_stat_same_files_as_default_diff() {
    let fx = CliFixture::new();
    std::fs::write(fx.path().join("staged.txt"), b"a\n").expect("write");
    std::fs::write(fx.path().join("other.txt"), b"o\n").expect("write");
    git(fx.path(), &["add", "staged.txt", "other.txt"]);
    git(fx.path(), &["commit", "-m", "init"]);
    std::fs::write(fx.path().join("staged.txt"), b"b\n").expect("write");
    std::fs::write(fx.path().join("other.txt"), b"p\n").expect("write");
    git(fx.path(), &["add", "staged.txt"]);

    let default_json = grit(fx.path(), &["diff", "--json"]);
    assert!(default_json.ok(), "{}", default_json.dump("diff json"));
    let stat_json = grit(fx.path(), &["diff", "--stat", "--json"]);
    assert!(stat_json.ok(), "{}", stat_json.dump("diff --stat json"));

    let mut default_paths = paths_from_diff_json(&default_json.stdout);
    let mut stat_paths = paths_from_diff_json(&stat_json.stdout);
    default_paths.sort();
    stat_paths.sort();
    assert_eq!(
        default_paths, stat_paths,
        "default diff paths {default_paths:?} vs stat {stat_paths:?}"
    );
}

fn paths_from_diff_json(stdout: &str) -> Vec<String> {
    let v: serde_json::Value = serde_json::from_str(stdout).expect("json");
    v["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|f| f["path"].as_str().expect("path").to_string())
        .collect()
}

#[test]
fn diff_staged_stat_same_files_as_staged_diff() {
    let fx = CliFixture::new();
    std::fs::write(fx.path().join("staged.txt"), b"a\n").expect("write");
    std::fs::write(fx.path().join("unstaged.txt"), b"u\n").expect("write");
    git(fx.path(), &["add", "."]);
    git(fx.path(), &["commit", "-m", "init"]);
    std::fs::write(fx.path().join("staged.txt"), b"b\n").expect("write");
    std::fs::write(fx.path().join("unstaged.txt"), b"v\n").expect("write");
    git(fx.path(), &["add", "staged.txt"]);

    let staged_json = grit(fx.path(), &["diff", "--staged", "--json"]);
    assert!(staged_json.ok(), "{}", staged_json.dump("staged diff"));
    let stat_json = grit(fx.path(), &["diff", "--staged", "--stat", "--json"]);
    assert!(stat_json.ok(), "{}", stat_json.dump("staged stat"));

    let mut staged_paths = paths_from_diff_json(&staged_json.stdout);
    let mut stat_paths = paths_from_diff_json(&stat_json.stdout);
    staged_paths.sort();
    stat_paths.sort();
    assert_eq!(staged_paths, ["staged.txt"]);
    assert_eq!(staged_paths, stat_paths);
}

#[test]
fn diff_commit_stat_same_files_as_commit_diff() {
    let dir = tempfile::tempdir().expect("tempdir");
    let grit_init = grit(dir.path(), &["init"]);
    assert!(grit_init.ok(), "{}", grit_init.dump("init"));
    std::fs::write(dir.path().join("f.txt"), b"1\n").expect("write");
    assert!(grit(dir.path(), &["add", "f.txt"]).ok(), "add");
    assert!(
        grit(dir.path(), &["commit", "-m", "one"]).ok(),
        "commit one"
    );
    std::fs::write(dir.path().join("f.txt"), b"2\n").expect("write");
    assert!(grit(dir.path(), &["add", "f.txt"]).ok(), "add");
    assert!(
        grit(dir.path(), &["commit", "-m", "two"]).ok(),
        "commit two"
    );

    let patch_json = grit(dir.path(), &["diff", "HEAD~1", "--json"]);
    assert!(patch_json.ok(), "{}", patch_json.dump("commit diff"));
    let stat_json = grit(dir.path(), &["diff", "HEAD~1", "--stat", "--json"]);
    assert!(stat_json.ok(), "{}", stat_json.dump("commit stat"));

    let mut patch_paths = paths_from_diff_json(&patch_json.stdout);
    let mut stat_paths = paths_from_diff_json(&stat_json.stdout);
    patch_paths.sort();
    stat_paths.sort();
    assert_eq!(patch_paths, stat_paths);
    assert_eq!(patch_paths, ["f.txt"]);
}

/// Regression for issue #1655: default list formats must include staged and unstaged paths.
#[test]
fn diff_name_status_default_includes_staged_and_unstaged() {
    let fx = CliFixture::new();
    std::fs::write(fx.path().join("a.txt"), b"1\n").expect("write a");
    std::fs::write(fx.path().join("b.txt"), b"1\n").expect("write b");
    git(fx.path(), &["add", "."]);
    git(fx.path(), &["commit", "-m", "init"]);
    std::fs::write(fx.path().join("a.txt"), b"2\n").expect("edit a");
    git(fx.path(), &["add", "a.txt"]);
    std::fs::write(fx.path().join("b.txt"), b"2\n").expect("edit b unstaged");

    let out = grit(fx.path(), &["diff", "--name-status"]);
    assert!(out.ok(), "{}", out.dump("name-status"));
    let mut paths: Vec<_> = out
        .stdout
        .lines()
        .filter_map(|line| line.split('\t').nth(1))
        .map(str::to_string)
        .collect();
    paths.sort();
    assert_eq!(paths, ["a.txt", "b.txt"], "stdout={:?}", out.stdout);

    let against = grit(fx.path(), &["diff", "--against", "HEAD", "--name-status"]);
    assert!(against.ok(), "{}", against.dump("against HEAD"));
    assert_eq!(
        out.stdout, against.stdout,
        "default should match --against HEAD"
    );
}

/// Regression for issue #1703: path limits must not drop staged-only changes from default diff.
#[test]
fn diff_default_path_limit_includes_staged_only_change() {
    let fx = CliFixture::new();
    std::fs::write(fx.path().join("a.txt"), b"1\n").expect("write a");
    std::fs::write(fx.path().join("b.txt"), b"1\n").expect("write b");
    git(fx.path(), &["add", "."]);
    git(fx.path(), &["commit", "-m", "init"]);
    std::fs::write(fx.path().join("a.txt"), b"2\n").expect("edit a staged");
    git(fx.path(), &["add", "a.txt"]);
    std::fs::write(fx.path().join("b.txt"), b"2\n").expect("edit b unstaged");

    let full = grit(fx.path(), &["diff", "--json"]);
    assert!(full.ok(), "{}", full.dump("full diff"));
    let v: serde_json::Value = serde_json::from_str(&full.stdout).expect("json");
    assert!(v["changed"].as_bool().unwrap());
    let full_paths: Vec<_> = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["path"].as_str())
        .collect();
    assert_eq!(full_paths, ["a.txt", "b.txt"]);

    let a_only = grit(fx.path(), &["diff", "--json", "--", "a.txt"]);
    assert!(a_only.ok(), "{}", a_only.dump("a.txt json"));
    let a_v: serde_json::Value = serde_json::from_str(&a_only.stdout).expect("json a");
    assert!(
        a_v["changed"].as_bool().unwrap(),
        "staged-only a.txt: {a_v}"
    );
    assert_eq!(a_v["files"].as_array().unwrap().len(), 1);
    assert_eq!(a_v["files"][0]["path"], "a.txt");

    let a_exit = grit(fx.path(), &["diff", "--exit-code", "--", "a.txt"]);
    assert_eq!(a_exit.status, Some(1), "{}", a_exit.dump("a.txt exit-code"));

    let b_only = grit(fx.path(), &["diff", "--json", "--", "b.txt"]);
    assert!(b_only.ok(), "{}", b_only.dump("b.txt json"));
    let b_v: serde_json::Value = serde_json::from_str(&b_only.stdout).expect("json b");
    assert!(b_v["changed"].as_bool().unwrap());
    assert_eq!(b_v["files"][0]["path"], "b.txt");

    let a_name = grit(fx.path(), &["diff", "--name-status", "--", "a.txt"]);
    assert!(a_name.ok(), "{}", a_name.dump("a.txt name-status"));
    assert!(a_name.stdout.contains("a.txt"));
    assert!(!a_name.stdout.contains("b.txt"));
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
