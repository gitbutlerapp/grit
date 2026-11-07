//! Issue #1401: commits that reference the empty tree must store object 4b825dc on disk.

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::porcelain::add::StageOptions;
use grit_lib::porcelain::commit::CommitRequest;
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
    {
        let mut req = CommitRequest::new(message.to_owned());
        req.author = Some(id.parse().expect("signature"));
        req.committer = Some(id.parse().expect("signature"));
        req.allow_empty = allow_empty;
        req
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
    let (repo, _) = Repository::init(root, &InitOptions::default()).expect("init");
    let git_dir = root.join(".git");

    let outcome = repo
        .commits()
        .create_staged(
            &commit_req("empty root", true),
            &mut NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
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
    let (repo, _) = Repository::init(root, &InitOptions::default()).expect("init");
    let git_dir = root.join(".git");

    fs::write(root.join("a"), b"a\n").expect("write a");
    repo.index()
        .stage(
            &StageOptions::default(),
            &mut NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("stage");
    repo.commits()
        .create_staged(
            &commit_req("one", false),
            &mut NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("first");

    remove_paths(
        &repo,
        &RemoveOptions {
            pathspecs: vec!["a".into()],
            ..RemoveOptions::default()
        },
        &grit_lib::progress::CancelToken::never(),
    )
    .expect("rm a");
    let outcome = repo
        .commits()
        .create_staged(
            &commit_req("remove everything", false),
            &mut NullProgress,
            &grit_lib::progress::CancelToken::never(),
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
