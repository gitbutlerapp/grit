//! Issue #1401: commits that reference the empty tree must store object 4b825dc on disk.

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::porcelain::paths::{remove_paths, RemoveOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::{InitOptions, Repository};
use grit_test_support::git;

const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

fn ident() -> String {
    "Compat Test <compat@example.com> 1700000000 +0000".to_owned()
}

fn commit_req(message: &str, allow_empty: bool) -> CommitRequest {
    let id = ident();
    CommitRequest {
        message: message.to_owned(),
        author: id.clone(),
        committer: id,
        allow_empty,
        sign_override: None,
        amend: false,
        assume_worktree_synced: false,
        skip_pre_commit_tree_match: false,
        staged_change_count: None,
        ..Default::default()
    }
}

fn git_fsck_strict(dir: &Path) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["fsck", "--strict"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fsck");
    assert!(
        out.status.success(),
        "git fsck --strict: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn empty_tree_loose_path(git_dir: &Path) -> std::path::PathBuf {
    git_dir.join(format!("objects/{}/{}", &EMPTY_TREE[..2], &EMPTY_TREE[2..]))
}

fn assert_empty_tree_materialized(git_dir: &Path) {
    assert!(
        empty_tree_loose_path(git_dir).is_file(),
        "expected loose empty tree at {}",
        empty_tree_loose_path(git_dir).display()
    );
}

fn git_clone_fsck(source: &Path) {
    let parent = tempfile::tempdir().expect("parent tempdir");
    let dest = parent.path().join("clone");
    git(
        parent.path(),
        &["clone", "-q", source.to_str().expect("utf8 path"), "clone"],
    );
    git_fsck_strict(&dest);
}

#[test]
fn grit_allow_empty_root_commit_git_fsck_clean() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = Repository::init(root, &InitOptions::default()).expect("init");
    let git_dir = root.join(".git");

    let outcome = create_commit(&repo, &commit_req("empty root", true), &mut NullProgress)
        .expect("allow-empty root commit");

    assert_empty_tree_materialized(&git_dir);
    git_fsck_strict(root);

    let head_obj = repo.odb().read(&outcome.oid).expect("commit");
    let parsed = parse_commit(&head_obj.data).expect("parse");
    assert_eq!(
        parsed.tree,
        ObjectId::from_hex(EMPTY_TREE).expect("empty tree oid")
    );

    git_clone_fsck(root);
}

#[test]
fn grit_remove_everything_commit_git_fsck_clean() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = Repository::init(root, &InitOptions::default()).expect("init");
    let git_dir = root.join(".git");

    fs::write(root.join("a"), b"a\n").expect("write a");
    stage(&repo, &StageOptions::default(), &mut NullProgress).expect("stage");
    create_commit(&repo, &commit_req("one", false), &mut NullProgress).expect("first");

    remove_paths(
        &repo,
        &RemoveOptions {
            pathspecs: vec!["a".into()],
            ..RemoveOptions::default()
        },
    )
    .expect("rm a");
    let outcome = create_commit(
        &repo,
        &commit_req("remove everything", false),
        &mut NullProgress,
    )
    .expect("empty tree commit");

    assert_empty_tree_materialized(&git_dir);
    git_fsck_strict(root);

    let head_obj = repo.odb().read(&outcome.oid).expect("commit");
    let parsed = parse_commit(&head_obj.data).expect("parse");
    assert_eq!(
        parsed.tree,
        ObjectId::from_hex(EMPTY_TREE).expect("empty tree oid")
    );

    git_clone_fsck(root);
}
