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
fn diff_uncommitted_format_flags_show_staged_and_unstaged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let grit_init = grit(dir.path(), &["init"]);
    assert!(grit_init.ok(), "{}", grit_init.dump("grit init"));
    std::fs::write(dir.path().join("a.txt"), b"a1\n").expect("write");
    std::fs::write(dir.path().join("b.txt"), b"b1\n").expect("write");
    assert!(grit(dir.path(), &["add", "a.txt", "b.txt"]).ok(), "grit add");
    assert!(
        grit(dir.path(), &["commit", "-m", "init"]).ok(),
        "grit commit"
    );

    // Modify and stage a.txt
    std::fs::write(dir.path().join("a.txt"), b"a2\n").expect("write");
    assert!(grit(dir.path(), &["add", "a.txt"]).ok(), "grit add a.txt");

    // Modify b.txt without staging
    std::fs::write(dir.path().join("b.txt"), b"b2\n").expect("write");

    // --name-only lists both staged and unstaged files
    let name_only = grit(dir.path(), &["diff", "--name-only"]);
    assert!(name_only.ok(), "{}", name_only.dump("diff --name-only"));
    let mut names: Vec<_> = name_only.stdout.lines().collect();
    names.sort();
    assert_eq!(names, ["a.txt", "b.txt"]);

    // --name-status lists both staged and unstaged files with M status
    let name_status = grit(dir.path(), &["diff", "--name-status"]);
    assert!(name_status.ok(), "{}", name_status.dump("diff --name-status"));
    let mut statuses: Vec<_> = name_status.stdout.lines().collect();
    statuses.sort();
    assert_eq!(statuses, ["M\ta.txt", "M\tb.txt"]);

    // --numstat lists both files
    let numstat = grit(dir.path(), &["diff", "--numstat"]);
    assert!(numstat.ok(), "{}", numstat.dump("diff --numstat"));
    let mut numstat_lines: Vec<_> = numstat.stdout.lines().collect();
    numstat_lines.sort();
    assert_eq!(numstat_lines, ["1\t1\ta.txt", "1\t1\tb.txt"]);
}

/// Git sometimes prints `old => new` for rename numstat lines; grit uses the new path only.
fn normalize_numstat_for_git_compat(s: &str) -> String {
    let mut out = String::new();
    for line in s.lines() {
        if !out.is_empty() {
            out.push('\n');
        }
        let mut parts = line.splitn(3, '\t');
        let ins = parts.next().unwrap_or("");
        let del = parts.next().unwrap_or("");
        let path = parts.next().unwrap_or("");
        let path = path.rsplit(" => ").next().unwrap_or(path);
        out.push_str(&format!("{ins}\t{del}\t{path}"));
    }
    if s.ends_with('\n') {
        out.push('\n');
    }
    out
}

#[test]
fn diff_numstat_rename_matches_git_bytes() {
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
    let git_out = git_cmd(&["diff", "--numstat"]).in_dir(dir.path()).exec();
    let grit_out = grit(dir.path(), &["diff", "--numstat"]);
    assert_eq!(
        normalize_numstat_for_git_compat(&git_out.stdout),
        normalize_numstat_for_git_compat(&grit_out.stdout),
        "{}",
        grit_out.dump("grit diff --numstat rename")
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
