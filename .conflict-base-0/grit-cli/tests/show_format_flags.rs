//! `grit show` patch-by-default, path limits, and shared diff format flags.

use std::process::Command;

use grit_test_support::cli_fixture::CliFixture;
use grit_test_support::{git, grit};

fn gs_json(repo: &std::path::Path, args: &[&str]) -> serde_json::Value {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_grit"));
    cmd.current_dir(repo).arg("--json");
    for a in args {
        cmd.arg(a);
    }
    let out = cmd.output().expect("grit");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("json")
}

#[test]
fn show_default_includes_patch() {
    let fx = CliFixture::new();
    let human = grit(fx.path(), &["show", "HEAD"]);
    assert!(human.ok(), "{}", human.dump("show human"));
    assert!(human.stdout.contains("commit "));
    assert!(
        human.stdout.contains("tracked.txt") || human.stdout.contains("second.txt"),
        "{}",
        human.dump("paths in patch")
    );
    assert!(
        human.stdout.contains('-') || human.stdout.contains('+'),
        "expected delta-style patch lines: {}",
        human.stdout
    );

    let md = grit(fx.path(), &["show", "HEAD", "--markdown"]);
    assert!(md.ok(), "{}", md.dump("show markdown"));
    assert!(md.stdout.contains("```diff") || md.stdout.contains("## Changes"));
}

#[test]
fn show_json_files_equal_diff_json_files() {
    let fx = CliFixture::new();
    for rev in ["HEAD", "HEAD~1"] {
        let show = gs_json(fx.path(), &["show", rev]);
        let diff = gs_json(fx.path(), &["diff", rev]);
        assert_eq!(show["files"], diff["files"], "files mismatch for {rev}");
    }
}

#[test]
fn show_paths_limit_matches_git_show() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init"]);
    std::fs::write(dir.path().join("a.txt"), b"one\n").expect("write a");
    std::fs::write(dir.path().join("b.txt"), b"two\n").expect("write b");
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-m", "init"]);
    std::fs::write(dir.path().join("a.txt"), b"one\nthree\n").expect("write a2");
    std::fs::write(dir.path().join("b.txt"), b"two\nfour\n").expect("write b2");
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-m", "second"]);

    let git_out = Command::new("git")
        .current_dir(dir.path())
        .args(["show", "--format=", "HEAD", "--", "a.txt"])
        .output()
        .expect("git show");
    assert!(git_out.status.success());

    let grit_init = grit(dir.path(), &["init"]);
    assert!(grit_init.ok(), "{}", grit_init.dump("grit init"));
    let grit_out = grit(dir.path(), &["show", "HEAD", "--", "a.txt"]);
    assert!(grit_out.ok(), "{}", grit_out.dump("grit show path"));
    let git_patch = String::from_utf8_lossy(&git_out.stdout);
    assert!(
        grit_out.stdout.contains("a.txt"),
        "{}",
        grit_out.dump("grit path limit")
    );
    assert!(
        !grit_out.stdout.contains("b.txt") || git_patch.contains("b.txt"),
        "grit should limit to a.txt like git"
    );
}

#[test]
fn show_stat_flag_matches_previous_default() {
    let fx = CliFixture::new();
    let stat = grit(fx.path(), &["show", "HEAD", "--stat"]);
    assert!(stat.ok(), "{}", stat.dump("show --stat"));
    assert!(stat.stdout.contains("file") && stat.stdout.contains("changed"));
    assert!(
        stat.stdout.contains('|'),
        "stat bars expected: {}",
        stat.stdout
    );
}

#[test]
fn show_name_status_matches_git() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init"]);
    std::fs::write(dir.path().join("f.txt"), b"x\n").expect("write");
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-m", "init"]);
    std::fs::write(dir.path().join("g.txt"), b"y\n").expect("write g");
    git(dir.path(), &["add", "g.txt"]);
    git(dir.path(), &["commit", "-m", "add g"]);

    let git_out = Command::new("git")
        .current_dir(dir.path())
        .args(["show", "--format=", "--name-status", "HEAD"])
        .output()
        .expect("git show");
    assert!(git_out.status.success());

    let grit_init = grit(dir.path(), &["init"]);
    assert!(grit_init.ok(), "{}", grit_init.dump("grit init"));
    let grit_out = grit(dir.path(), &["show", "HEAD", "--name-status"]);
    assert!(
        grit_out.ok(),
        "{}",
        grit_out.dump("grit show --name-status")
    );
    let git_stdout = String::from_utf8_lossy(&git_out.stdout);
    let git_lines: Vec<&str> = git_stdout.lines().filter(|l| !l.is_empty()).collect();
    let grit_lines: Vec<&str> = grit_out
        .stdout
        .lines()
        .filter(|l| {
            l.len() >= 2
                && l.as_bytes()[1] == b'\t'
                && matches!(
                    l.as_bytes()[0],
                    b'A' | b'M' | b'D' | b'R' | b'C' | b'T' | b'U' | b'X'
                )
        })
        .collect();
    assert_eq!(git_lines, grit_lines, "{}", grit_out.dump("name-status"));
}

#[test]
fn show_unified_and_w_flags() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    assert!(grit(&repo, &["init"]).ok(), "init");
    std::fs::write(repo.join("f.txt"), b"alpha\nbeta\n").expect("write");
    assert!(grit(&repo, &["commit", "-m", "init"]).ok(), "commit init");
    std::fs::write(repo.join("f.txt"), b"alpha\ngamma\n").expect("write2");
    assert!(
        grit(&repo, &["commit", "-m", "change"]).ok(),
        "commit change"
    );

    let u0 = grit(&repo, &["show", "HEAD", "-U0"]);
    assert!(u0.ok(), "{}", u0.dump("-U0"));
    let u5 = grit(&repo, &["show", "HEAD", "-U5"]);
    assert!(u5.ok(), "{}", u5.dump("-U5"));
    assert_ne!(u0.stdout, u5.stdout, "context width should change output");
}
