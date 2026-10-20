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
    let git_out = std::process::Command::new("git")
        .current_dir(dir.path())
        .args(["diff", "--numstat"])
        .output()
        .expect("git diff");
    let grit_out = grit(dir.path(), &["diff", "--numstat"]);
    assert_eq!(
        String::from_utf8_lossy(&git_out.stdout),
        grit_out.stdout,
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
fn diff_name_status_two_commit_path_limit() {
    let fx = CliFixture::new();
    std::fs::write(fx.path().join("a.txt"), b"a\n").expect("write");
    std::fs::write(fx.path().join("b.txt"), b"b\n").expect("write");
    git(fx.path(), &["add", "a.txt", "b.txt"]);
    git(fx.path(), &["commit", "-m", "initial"]);
    std::fs::write(fx.path().join("a.txt"), b"aa\n").expect("write");
    std::fs::write(fx.path().join("b.txt"), b"bb\n").expect("write");
    git(fx.path(), &["add", "a.txt", "b.txt"]);
    git(fx.path(), &["commit", "-m", "second"]);
    let git_out = std::process::Command::new("git")
        .current_dir(fx.path())
        .args(["diff", "--name-status", "HEAD~1", "HEAD", "--", "a.txt"])
        .output()
        .expect("git diff");
    let grit_out = grit(
        fx.path(),
        &["diff", "--name-status", "HEAD~1", "HEAD", "--", "a.txt"],
    );
    assert!(grit_out.ok(), "{}", grit_out.dump("name-status path"));
    assert_eq!(
        String::from_utf8_lossy(&git_out.stdout),
        grit_out.stdout,
        "{}",
        grit_out.dump("grit name-status path limit")
    );
}

#[test]
fn diff_stat_no_leading_blank_line() {
    let fx = CliFixture::new();
    std::fs::write(fx.path().join("f.txt"), b"a\n").expect("write");
    git(fx.path(), &["add", "f.txt"]);
    git(fx.path(), &["commit", "-m", "init"]);
    std::fs::write(fx.path().join("f.txt"), b"b\n").expect("write");
    let out = grit(fx.path(), &["diff", "--stat", "HEAD~1", "HEAD"]);
    assert!(out.ok(), "{}", out.dump("stat"));
    assert!(
        !out.stdout.starts_with('\n'),
        "stat must not begin with a blank line: {:?}",
        out.stdout
    );
}

#[test]
fn diff_markdown_shortstat_empty_is_zero_bytes() {
    let fx = CliFixture::empty();
    let out = grit(fx.path(), &["diff", "--markdown", "--shortstat"]);
    assert!(out.ok(), "{}", out.dump("markdown shortstat empty"));
    assert!(out.stdout.is_empty(), "stdout={:?}", out.stdout);
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
