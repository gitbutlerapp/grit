//! Rename with unrelated packed refs, including loose overrides and peeled tags.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::branch::{rename_branch, RenameBranchOptions};
use grit_lib::repo::Repository;
use grit_test_support::{git, git_cmd};

fn open(dir: &Path) -> Repository {
    Repository::discover(Some(dir), &RepositoryOptions::empty()).unwrap()
}

fn rename(repo: &Repository, old: &str, new: &str) {
    rename_branch(
        repo,
        &RenameBranchOptions {
            old: old.into(),
            new: new.into(),
            force: false,
            dry_run: false,
        },
    )
    .unwrap();
}

fn fixture() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["commit", "-q", "--allow-empty", "-m", "initial"]);
    let tip = git(dir, &["rev-parse", "HEAD"]);
    let mut input = String::new();
    for i in 0..2_000 {
        writeln!(input, "create refs/heads/b{i:04} {}", tip.trim()).unwrap();
        writeln!(input, "create refs/tags/t{i:04} {}", tip.trim()).unwrap();
    }
    git_cmd(&["update-ref", "--stdin"])
        .in_dir(dir)
        .stdin(input)
        .suc();
    git(dir, &["tag", "-a", "annotated", "-m", "tag"]);
    git(dir, &["pack-refs", "--all"]);
    tmp
}

#[test]
fn loose_rename_preserves_unrelated_packed_refs_and_roundtrips_through_git() {
    let tmp = fixture();
    let dir = tmp.path();
    git(dir, &["branch", "topic"]);
    let tip: ObjectId = git(dir, &["rev-parse", "topic"]).trim().parse().unwrap();
    let packed = dir.join(".git/packed-refs");
    let before = fs::read(&packed).unwrap();
    let log = fs::read(dir.join(".git/logs/refs/heads/topic")).unwrap();
    let repo = open(dir);

    rename(&repo, "topic", "renamed");

    assert_eq!(fs::read(&packed).unwrap(), before);
    assert!(!dir.join(".git/refs/heads/topic").exists());
    assert!(fs::read(dir.join(".git/logs/refs/heads/topic"))
        .unwrap()
        .is_empty());
    assert!(fs::read(dir.join(".git/logs/refs/heads/renamed"))
        .unwrap()
        .starts_with(&log));
    assert_eq!(git(dir, &["rev-parse", "renamed"]).trim(), tip.to_string());
    assert_eq!(
        git(dir, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/main"
    );
    git(dir, &["fsck", "--strict", "--no-progress"]);

    git(dir, &["branch", "-m", "renamed", "returned"]);
    assert_eq!(repo.refs().resolve("refs/heads/returned").unwrap(), tip);
    assert!(repo
        .refs()
        .read_raw("refs/heads/renamed")
        .unwrap()
        .is_none());
    assert_eq!(fs::read(&packed).unwrap(), before);
}

#[test]
fn rename_removes_shadowed_packed_source_and_keeps_peeled_tags() {
    let tmp = fixture();
    let dir = tmp.path();
    let packed = dir.join(".git/packed-refs");
    let before = fs::read_to_string(&packed).unwrap();
    git(dir, &["commit", "-q", "--allow-empty", "-m", "next"]);
    git(dir, &["branch", "-f", "b1000", "HEAD"]);
    let tip: ObjectId = git(dir, &["rev-parse", "b1000"]).trim().parse().unwrap();
    let repo = open(dir);

    rename(&repo, "b1000", "renamed");

    let after = fs::read_to_string(&packed).unwrap();
    let expected: Vec<_> = before
        .lines()
        .filter(|line| !line.starts_with('#') && !line.ends_with(" refs/heads/b1000"))
        .collect();
    assert_eq!(
        after
            .lines()
            .filter(|line| !line.starts_with('#'))
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(git(dir, &["rev-parse", "renamed"]).trim(), tip.to_string());
    assert!(repo.refs().read_raw("refs/heads/b1000").unwrap().is_none());
    assert_eq!(
        git(dir, &["rev-parse", "annotated^{}"]),
        git(dir, &["rev-parse", "HEAD^"])
    );
    git(dir, &["fsck", "--strict", "--no-progress"]);
    git(dir, &["pack-refs", "--all"]);
    assert_eq!(repo.refs().resolve("refs/heads/renamed").unwrap(), tip);
}
