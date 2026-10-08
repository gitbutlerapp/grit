//! Branch/tag ref names must match Git `check-ref-format` rules (#927).

use std::path::Path;
use std::process::Command;

use grit_lib::check_ref_format::{validate_branch_short_name, validate_tag_short_name};
use grit_lib::objects::ObjectId;
use grit_lib::refs::write_ref;
use grit_lib::repo::init_repository;
use tempfile::tempdir;

fn git_check_ref_format_branch(name: &str) -> bool {
    Command::new("git")
        .args(["check-ref-format", "--branch", name])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn git_check_ref_format_tag(name: &str) -> bool {
    let full = format!("refs/tags/{name}");
    Command::new("git")
        .args(["check-ref-format", &full])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn git_fsck_strict(git_dir: &Path) -> bool {
    Command::new("git")
        .args([
            "-C",
            git_dir.to_str().expect("utf-8 path"),
            "fsck",
            "--strict",
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn sample_oid() -> ObjectId {
    "67bf698f3ab735e92fb011a99cff3497c44d30c1".parse().unwrap()
}

fn seed_empty_commit(worktree: &Path) -> ObjectId {
    let status = Command::new("git")
        .current_dir(worktree)
        .args(["commit", "--allow-empty", "-m", "seed"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .status()
        .expect("git commit");
    assert!(status.success(), "git commit --allow-empty failed");
    let out = Command::new("git")
        .current_dir(worktree)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("rev-parse");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("HEAD oid")
}

#[test]
fn branch_short_names_match_git_check_ref_format() {
    let samples = [
        "main",
        "feature/foo",
        "@",
        "-",
        "-topic",
        "bad name",
        "x..y",
        "a~b",
        "a.lock",
        "HEAD",
        "a:b",
        "a*b",
    ];
    for name in samples {
        let grit_ok = validate_branch_short_name(name).is_ok();
        let git_ok = git_check_ref_format_branch(name);
        assert_eq!(
            grit_ok, git_ok,
            "branch name {name:?}: grit={grit_ok} git={git_ok}"
        );
    }
}

#[test]
fn tag_short_names_match_git_check_ref_format() {
    let samples = [
        "v1",
        "release/1.0",
        "bad name",
        "a..b",
        "x.lock",
        "@",
        "HEAD",
    ];
    for name in samples {
        let grit_ok = validate_tag_short_name(name).is_ok();
        let git_ok = git_check_ref_format_tag(name);
        assert_eq!(
            grit_ok, git_ok,
            "tag name {name:?}: grit={grit_ok} git={git_ok}"
        );
    }
}

#[test]
fn write_ref_rejects_invalid_branch_names_without_loose_files() {
    let dir = tempdir().unwrap();
    let repo = init_repository(dir.path(), false, "main", None, "files").expect("init");
    let git_dir = repo.git_dir.clone();
    let invalid = ["bad name", "x..y", "a~b", "a.lock"];
    for name in invalid {
        let refname = format!("refs/heads/{name}");
        assert!(
            write_ref(&git_dir, &refname, &sample_oid()).is_err(),
            "write_ref must reject {refname}"
        );
        let loose = git_dir.join("refs/heads").join(name);
        assert!(
            !loose.exists(),
            "must not leave loose ref at {}",
            loose.display()
        );
    }
    assert!(git_fsck_strict(dir.path()), "git fsck --strict must pass");
}

#[test]
fn write_ref_accepts_refs_heads_head_like_git() {
    let dir = tempdir().unwrap();
    let worktree = dir.path();
    let repo = init_repository(worktree, false, "main", None, "files").expect("init");
    let tip = seed_empty_commit(worktree);
    write_ref(&repo.git_dir, "refs/heads/HEAD", &tip)
        .expect("plumbing may write refs/heads/HEAD");
    assert!(
        git_fsck_strict(worktree),
        "git fsck --strict must pass on refs/heads/HEAD"
    );
}
