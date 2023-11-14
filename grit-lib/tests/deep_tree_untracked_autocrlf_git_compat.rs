//! Deep-chain untracked siblings and autocrlf staging (issue #959 review).

use std::fs;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::add::{stage, stage_for_commit, StageMode, StageOptions};
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;

fn init_autocrlf_repo(root: &std::path::Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "core.autocrlf", "true"]);
}

#[test]
fn status_and_add_see_interior_untracked_on_indexed_chain() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_autocrlf_repo(root);
    fs::create_dir_all(root.join("a/b/c")).expect("mkdirs");
    fs::write(root.join("a/b/c/tracked.txt"), b"tracked\n").expect("write tracked");
    git(root, &["add", "a/b/c/tracked.txt"]);
    git(root, &["commit", "-qm", "seed"]);
    fs::write(root.join("a/b/untracked.txt"), b"new\n").expect("write untracked");

    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open");
    let model = status(&repo, &StatusOptions::default(), &mut NullProgress).expect("status");
    assert!(
        model.untracked.iter().any(|p| p == "a/b/untracked.txt"),
        "status must list interior untracked sibling, got {:?}",
        model.untracked
    );

    let outcome = stage(
        &repo,
        &StageOptions {
            pathspecs: Vec::new(),
            pathspec_sources: Vec::new(),
            mode: StageMode::All,
            ..StageOptions::default()
        },
        &mut NullProgress,
    )
    .expect("add all");
    assert!(outcome.total() >= 1, "add must stage untracked sibling");

    let index = repo.index_handle().load().expect("index");
    assert!(
        index.get(b"a/b/untracked.txt", 0).is_some(),
        "index must contain staged untracked sibling"
    );

    if Command::new("git").arg("--version").output().is_ok() {
        let out = git(root, &["status", "--porcelain"]);
        assert!(
            out.contains("a/b/untracked.txt") || out.contains("?? a/b/untracked.txt"),
            "git should also see untracked before grit commit: {out}"
        );
    }
}

#[test]
fn full_tree_add_and_commit_store_lf_with_autocrlf() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_autocrlf_repo(root);
    fs::write(root.join("f.txt"), b"before\n").expect("write");
    git(root, &["add", "f.txt"]);
    git(root, &["commit", "-qm", "init"]);

    fs::write(root.join("f.txt"), b"after\r\n").expect("write crlf");

    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open");
    stage(
        &repo,
        &StageOptions {
            pathspecs: Vec::new(),
            pathspec_sources: Vec::new(),
            mode: StageMode::All,
            ..StageOptions::default()
        },
        &mut NullProgress,
    )
    .expect("grit add all");

    let grit_staged_oid = repo
        .index_handle()
        .load()
        .expect("idx")
        .get(b"f.txt", 0)
        .expect("f.txt staged")
        .oid;
    let grit_blob = repo.odb().read(&grit_staged_oid).expect("read blob");
    assert_eq!(
        grit_blob.data, b"after\n",
        "grit add must store LF-normalized bytes with autocrlf"
    );

    git(root, &["add", "f.txt"]);
    let git_blob_hex = git(root, &["rev-parse", ":f.txt"]);
    let git_oid = ObjectId::from_hex(git_blob_hex.trim()).expect("git blob oid");
    let git_blob = repo.odb().read(&git_oid).expect("git blob");
    assert_eq!(
        git_blob.data, grit_blob.data,
        "grit staged blob must match system git index for same worktree"
    );

    stage_for_commit(&repo, &mut NullProgress).expect("grit commit staging");
    let ident = "Test <t@example.com> 1 +0000".to_string();
    create_commit(
        &repo,
        &CommitRequest {
            message: "edit".into(),
            author: ident.clone(),
            committer: ident,
            allow_empty: false,
            sign_override: None,
            amend: false,
            assume_worktree_synced: false,
            skip_pre_commit_tree_match: false,
            staged_change_count: None,
            dry_run: false,
        },
        &mut NullProgress,
    )
    .expect("grit commit");
}
