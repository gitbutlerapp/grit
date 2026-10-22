//! Wide clean picks through [`grit_lib::porcelain::replay::replay_commit`], checked with system git.
//!
//! A pick that rewrites many files exercises the sparse three-way tree merge, the merge checkout
//! and the single index write that follows it. Git must read the result as a clean repository
//! whose index carries fresh stat data and a valid cache-tree.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::Index;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::replay::{replay_commit, ReplayDirection, ReplayOutcome, ReplayRequest};
use grit_lib::repo::Repository;
use grit_test_support::git;

const IDENT: &str = "Test User <t@example.com> 1700000000 +0000";
const DIRS: usize = 6;
const FILES_PER_DIR: usize = 12;

fn init_repo(root: &Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "gc.auto", "0"]);
}

fn rev(root: &Path, spec: &str) -> String {
    git(root, &["rev-parse", spec]).trim().to_owned()
}

fn rel(dir: usize, file: usize) -> String {
    format!("d{dir}/f{file:02}.txt")
}

/// Base commit with `DIRS × FILES_PER_DIR` files, a `topic` commit rewriting the first half of
/// the directories, and a diverging `main` commit touching one file outside that half.
/// Returns the topic commit id.
fn wide_fixture(root: &Path) -> String {
    init_repo(root);
    for dir in 0..DIRS {
        fs::create_dir_all(root.join(format!("d{dir}"))).expect("mkdir");
        for file in 0..FILES_PER_DIR {
            fs::write(root.join(rel(dir, file)), format!("base {dir}/{file}\n")).expect("write");
        }
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["checkout", "-q", "-b", "topic"]);
    for dir in 0..DIRS / 2 {
        for file in 0..FILES_PER_DIR {
            fs::write(root.join(rel(dir, file)), format!("topic {dir}/{file}\n")).expect("write");
        }
    }
    git(root, &["commit", "-qam", "topic rewrite"]);
    let topic = rev(root, "HEAD");
    git(root, &["checkout", "-q", "main"]);
    fs::write(root.join(rel(DIRS - 1, FILES_PER_DIR - 1)), "main diverge\n").expect("write");
    git(root, &["commit", "-qam", "main diverge"]);
    topic
}

fn open_repo(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open")
}

fn pick(repo: &Repository, commit: &str) -> ReplayOutcome {
    replay_commit(
        repo,
        &ReplayRequest {
            commit: ObjectId::from_hex(commit).expect("oid"),
            direction: ReplayDirection::Pick,
            committer: IDENT.to_owned(),
            message_override: None,
        },
    )
    .expect("pick")
}

#[test]
fn wide_pick_tree_matches_git_merge_tree_and_git_sees_clean_repo() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let topic = wide_fixture(root);
    let main_before = rev(root, "main");
    let expected_tree = git(
        root,
        &[
            "merge-tree",
            "--write-tree",
            "--merge-base",
            &format!("{topic}^"),
            &main_before,
            &topic,
        ],
    )
    .lines()
    .next()
    .expect("merge-tree output")
    .to_owned();

    let repo = open_repo(root);
    let ReplayOutcome::Committed { tree, .. } = pick(&repo, &topic) else {
        panic!("expected a clean pick");
    };
    assert_eq!(tree.to_hex(), expected_tree);
    assert_eq!(rev(root, "HEAD^{tree}"), expected_tree);
    assert_eq!(rev(root, "HEAD^"), main_before);

    assert_eq!(git(root, &["status", "--porcelain"]), "");
    git(root, &["diff", "--cached", "--quiet"]);
    git(root, &["diff", "--quiet"]);
    // `write-tree` trusts valid cache-tree nodes, so a stale TREE extension shows up here.
    assert_eq!(git(root, &["write-tree"]).trim(), expected_tree);
    git(root, &["fsck", "--strict", "--no-dangling"]);
}

#[test]
fn wide_pick_index_rows_carry_worktree_stat_and_index_is_newer_than_entries() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let topic = wide_fixture(root);
    let repo = open_repo(root);
    assert!(matches!(
        pick(&repo, &topic),
        ReplayOutcome::Committed { .. }
    ));

    let index_path = root.join(".git/index");
    let index = Index::load(&index_path).expect("load index");
    assert_eq!(index.entries().len(), DIRS * FILES_PER_DIR);
    let index_mtime = fs::metadata(&index_path)
        .expect("index meta")
        .modified()
        .expect("index mtime");
    for entry in index.entries() {
        let path = String::from_utf8(entry.path.clone()).expect("utf8 path");
        let meta = fs::symlink_metadata(root.join(&path)).expect("worktree meta");
        assert_eq!(u64::from(entry.size), meta.len(), "{path}: size");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(i64::from(entry.mtime_sec), meta.mtime(), "{path}: mtime");
            assert_eq!(u64::from(entry.ino), meta.ino() & 0xFFFF_FFFF, "{path}: ino");
        }
        assert!(
            meta.modified().expect("mtime") < index_mtime,
            "{path}: entry is racily clean against the new index"
        );
    }
}

#[cfg(unix)]
#[test]
fn wide_pick_runs_post_index_change_once_with_workdir_flag() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let topic = wide_fixture(root);
    let log = root.join("hook.log");
    let hook = root.join(".git/hooks/post-index-change");
    fs::create_dir_all(hook.parent().expect("hooks dir")).expect("mkdir hooks");
    fs::write(
        &hook,
        format!("#!/bin/sh\necho \"$1 $2\" >> '{}'\n", log.display()),
    )
    .expect("write hook");
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("chmod hook");

    let repo = open_repo(root);
    assert!(matches!(
        pick(&repo, &topic),
        ReplayOutcome::Committed { .. }
    ));
    assert_eq!(fs::read_to_string(&log).expect("hook log"), "1 0\n");
}
