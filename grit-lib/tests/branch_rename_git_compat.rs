//! Branch rename through [`grit_lib::porcelain::branch::rename_branch`], checked against
//! the system `git` on files (packed), reftable, and injected memory ref stores.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::branch::{list_branches, rename_branch, BranchError, RenameBranchOptions};
use grit_lib::refs::store::{
    Expected, MemoryRefStore, RawRef, RefStore, RefTransaction, RefUpdate, RefUpdateFlags,
    ReflogUpdate,
};
use grit_lib::repo::{InitOptions, Repository};
use grit_test_support::{git, git_cmd};

mod support;

use support::{git_init_reftable_repo, require_reftable_git};

fn oid(dir: &Path, rev: &str) -> ObjectId {
    git(dir, &["rev-parse", rev]).trim().parse().expect("oid")
}

fn commit(dir: &Path, message: &str) {
    git(dir, &["commit", "-q", "--allow-empty", "-m", message]);
}

fn open(dir: &Path) -> Repository {
    Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("discover")
}

fn git_fixture(root: &Path) -> PathBuf {
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    commit(&repo, "c1");
    git(&repo, &["branch", "topic"]);
    commit(&repo, "c2");
    git(&repo, &["switch", "-q", "topic"]);
    commit(&repo, "c3");
    git(&repo, &["switch", "-q", "main"]);
    git(&repo, &["remote", "add", "origin", "/nonexistent"]);
    git(&repo, &["config", "branch.topic.remote", "origin"]);
    git(&repo, &["config", "branch.topic.merge", "refs/heads/topic"]);
    git(&repo, &["pack-refs", "--all"]);
    let wt = root.join("wt");
    git(
        &repo,
        &["worktree", "add", "-q", "-b", "wt", wt.to_str().unwrap()],
    );
    repo
}

fn assert_git_rename_checks(dir: &Path, old: &str, new: &str) {
    assert!(
        !git_cmd(&["rev-parse", old]).in_dir(dir).exec().ok(),
        "old ref should be gone"
    );
    assert!(
        git_cmd(&["rev-parse", new]).in_dir(dir).exec().ok(),
        "new ref should exist"
    );
    let reflog = git(dir, &["reflog", "show", new]);
    assert!(
        reflog.contains("branch: Created") || reflog.contains("commit:"),
        "{reflog}"
    );
    assert!(
        reflog.contains(&format!(
            "Branch: renamed refs/heads/{old} to refs/heads/{new}"
        )),
        "{reflog}"
    );
    assert_eq!(git(dir, &["config", "branch.new.remote"]).trim(), "origin");
    assert_eq!(
        git(dir, &["config", "branch.new.merge"]).trim(),
        "refs/heads/topic"
    );
    git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn grit_rename_files_packed_git_compat() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = git_fixture(tmp.path());
    let topic_tip = oid(&dir, "topic");
    let repo = open(&dir);

    rename_branch(
        &repo,
        &RenameBranchOptions {
            old: "topic".into(),
            new: "new".into(),
            force: false,
            dry_run: false,
        },
    )
    .unwrap();

    assert_git_rename_checks(&dir, "topic", "new");
    assert_eq!(oid(&dir, "new"), topic_tip);
    let list = list_branches(&repo).unwrap();
    assert!(list.branches.iter().any(|b| b.name == "new"));
    assert!(!list.branches.iter().any(|b| b.name == "topic"));

    let wt_head = git(tmp.path().join("wt").as_path(), &["symbolic-ref", "HEAD"]);
    assert_eq!(wt_head.trim(), "refs/heads/wt");
    let main_head = git(&dir, &["symbolic-ref", "HEAD"]);
    assert_eq!(main_head.trim(), "refs/heads/main");
}

#[test]
fn git_rename_roundtrip_list_and_reflog() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = git_fixture(tmp.path());
    git(&dir, &["branch", "-m", "topic", "renamed"]);

    let repo = open(&dir);
    let names: Vec<String> = list_branches(&repo)
        .unwrap()
        .branches
        .into_iter()
        .map(|b| b.name)
        .collect();
    assert!(names.contains(&"renamed".to_owned()));
    let reflog = repo
        .references()
        .reflog(&grit_lib::refs::store::RefName::try_from("refs/heads/renamed").unwrap())
        .unwrap();
    assert!(reflog.iter().any(|e| e.message.contains("Branch: renamed")));
}

#[test]
fn rename_force_overwrite_and_dry_run() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = git_fixture(tmp.path());
    git(&dir, &["branch", "other", "main"]);
    let repo = open(&dir);

    let preview = rename_branch(
        &repo,
        &RenameBranchOptions {
            old: "topic".into(),
            new: "other".into(),
            force: true,
            dry_run: true,
        },
    )
    .unwrap();
    assert!(preview.dry_run);
    assert!(git_cmd(&["show-ref", "--verify", "refs/heads/topic"])
        .in_dir(&dir)
        .exec()
        .ok());

    rename_branch(
        &repo,
        &RenameBranchOptions {
            old: "topic".into(),
            new: "other".into(),
            force: true,
            dry_run: false,
        },
    )
    .unwrap();
    assert!(!git_cmd(&["show-ref", "--verify", "refs/heads/topic"])
        .in_dir(&dir)
        .exec()
        .ok());
    assert!(git_cmd(&["show-ref", "--verify", "refs/heads/other"])
        .in_dir(&dir)
        .exec()
        .ok());
}

#[test]
fn rename_refuses_without_force_when_target_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = git_fixture(tmp.path());
    git(&dir, &["branch", "other", "main"]);
    let repo = open(&dir);
    let err = rename_branch(
        &repo,
        &RenameBranchOptions {
            old: "topic".into(),
            new: "other".into(),
            force: false,
            dry_run: false,
        },
    )
    .unwrap_err();
    assert!(
        matches!(&err, Error::Branch(BranchError::AlreadyExists { name }) if name == "other"),
        "{err:?}"
    );
}

#[test]
fn rename_unborn_head_on_current_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("repo");
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    let repo = open(&dir);
    rename_branch(
        &repo,
        &RenameBranchOptions {
            old: "main".into(),
            new: "start".into(),
            force: false,
            dry_run: false,
        },
    )
    .unwrap();
    assert_eq!(
        git(&dir, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/start"
    );
    assert!(!git_cmd(&["rev-parse", "start"]).in_dir(&dir).exec().ok());
}

#[test]
fn rename_updates_linked_worktree_head_when_checked_out() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = git_fixture(tmp.path());
    let wt = tmp.path().join("wt");
    git(&wt, &["switch", "-q", "wt"]);
    let repo = open(&wt);
    rename_branch(
        &repo,
        &RenameBranchOptions {
            old: "wt".into(),
            new: "working".into(),
            force: false,
            dry_run: false,
        },
    )
    .unwrap();
    assert_eq!(
        git(&wt, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/working"
    );
    assert_eq!(
        git(&dir, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/main"
    );
}

#[test]
fn rename_force_refuses_when_target_checked_out_in_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = git_fixture(tmp.path());
    let repo = open(&dir);
    let err = rename_branch(
        &repo,
        &RenameBranchOptions {
            old: "topic".into(),
            new: "wt".into(),
            force: true,
            dry_run: false,
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, Error::BranchCheckedOutInWorktree { .. }),
        "{err:?}"
    );
}

#[test]
fn rename_on_reftable_backend() {
    if !require_reftable_git() {
        eprintln!("SKIP: reftable git unavailable");
        return;
    }
    let root = git_init_reftable_repo("main");
    let dir = root.path();
    commit(dir, "c1");
    git(dir, &["branch", "topic"]);
    commit(dir, "c2");
    git(dir, &["config", "branch.topic.remote", "origin"]);
    git(dir, &["remote", "add", "origin", "/dev/null"]);
    git(dir, &["config", "branch.topic.merge", "refs/heads/topic"]);
    let repo = open(dir);
    rename_branch(
        &repo,
        &RenameBranchOptions {
            old: "topic".into(),
            new: "new".into(),
            force: false,
            dry_run: false,
        },
    )
    .unwrap();
    assert_git_rename_checks(dir, "topic", "new");
}

#[test]
fn rename_on_injected_memory_ref_store() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("repo");
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    commit(&dir, "c1");
    let tip = oid(&dir, "HEAD");

    let store = Arc::new(MemoryRefStore::new());
    let direct = |name: &str, reflog: bool| RefUpdate {
        name: name.to_owned(),
        new_value: Some(RawRef::Direct(tip)),
        expected: Expected::Missing,
        reflog: reflog.then(|| ReflogUpdate {
            identity: "T <t@example.com>".to_owned(),
            message: "create".to_owned(),
            time: time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
            old_oid: None,
            new_oid: None,
        }),
        flags: RefUpdateFlags::default(),
    };
    store
        .prepare(
            RefTransaction::new()
                .update(direct("refs/heads/main", false))
                .unwrap()
                .update(direct("refs/heads/topic", true))
                .unwrap()
                .update(RefUpdate {
                    name: "HEAD".to_owned(),
                    new_value: Some(RawRef::Symbolic("refs/heads/main".to_owned())),
                    expected: Expected::Missing,
                    reflog: None,
                    flags: RefUpdateFlags {
                        no_deref: true,
                        ..RefUpdateFlags::default()
                    },
                })
                .unwrap(),
        )
        .unwrap()
        .commit()
        .unwrap();

    let repo = open(&dir).with_ref_store(store.clone());
    rename_branch(
        &repo,
        &RenameBranchOptions {
            old: "topic".into(),
            new: "renamed".into(),
            force: false,
            dry_run: false,
        },
    )
    .unwrap();
    assert_eq!(
        store.read_raw("refs/heads/renamed").unwrap(),
        Some(RawRef::Direct(tip))
    );
    assert_eq!(store.read_raw("refs/heads/topic").unwrap(), None);
    assert!(store.reflog_exists("refs/heads/renamed").unwrap());
    assert!(!store.reflog_exists("refs/heads/topic").unwrap());
}
