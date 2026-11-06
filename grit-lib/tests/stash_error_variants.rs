//! Typed [`StashError`] coverage (task 7108).

use grit_lib::ident::Signature;
use grit_lib::objects::{parse_commit, NewCommit};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::stash::StashApplyOptions;
use grit_lib::stash::{CorruptStashReason, StashError};
use grit_lib::state::{resolve_head, HeadState};
use grit_test_support::{git, git_cmd};
use std::fs;
use tempfile::tempdir;

#[test]
fn stash_error_variants_git_created_stash_round_trip() {
    let dir = tempdir().unwrap();
    git_cmd(&["init"]).in_dir(dir.path()).exec();
    git(dir.path(), &["config", "user.email", "a@b.c"]);
    git(dir.path(), &["config", "user.name", "Test"]);
    fs::write(dir.path().join("f.txt"), "one\n").unwrap();
    git(dir.path(), &["add", "f.txt"]);
    git(dir.path(), &["commit", "-m", "seed"]);
    fs::write(dir.path().join("f.txt"), "two\n").unwrap();
    git(dir.path(), &["stash", "push", "-m", "wip"]);

    let repo = Repository::discover(
        Some(dir.path()),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .unwrap();
    let entries = repo.stash().list().expect("read git stash");
    assert_eq!(entries.len(), 1);
}

#[test]
fn stash_not_found_when_index_out_of_range() {
    let dir = tempdir().unwrap();
    let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).unwrap();
    let err = repo
        .stash()
        .drop(5)
        .expect_err("stash@{5} missing on empty stack");
    assert!(matches!(err, StashError::NotFound { n: 5 }));
}

#[test]
fn stash_apply_refuses_local_overwrite() {
    let dir = tempdir().unwrap();
    git_cmd(&["init"]).in_dir(dir.path()).exec();
    git(dir.path(), &["config", "user.email", "a@b.c"]);
    git(dir.path(), &["config", "user.name", "Test"]);
    fs::write(dir.path().join("f.txt"), "base\n").unwrap();
    git(dir.path(), &["add", "f.txt"]);
    git(dir.path(), &["commit", "-m", "seed"]);
    fs::write(dir.path().join("f.txt"), "stashed\n").unwrap();
    git(dir.path(), &["stash", "push"]);
    fs::write(dir.path().join("f.txt"), "local edit\n").unwrap();

    let repo = Repository::discover(
        Some(dir.path()),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .unwrap();
    let err = repo
        .stash()
        .apply(
            0,
            &StashApplyOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .expect_err("overwrite blocked");
    assert!(matches!(err, StashError::WouldOverwriteLocalChanges { .. }));
}

#[test]
fn corrupt_stash_wrong_parent_count() {
    let dir = tempdir().unwrap();
    git_cmd(&["init"]).in_dir(dir.path()).exec();
    git(dir.path(), &["config", "user.email", "a@b.c"]);
    git(dir.path(), &["config", "user.name", "Test"]);
    fs::write(dir.path().join("f.txt"), "x\n").unwrap();
    git(dir.path(), &["add", "f.txt"]);
    git(dir.path(), &["commit", "-m", "seed"]);

    let repo = Repository::discover(
        Some(dir.path()),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .unwrap();
    let head = match resolve_head(repo.git_dir()).expect("head") {
        HeadState::Branch { oid: Some(oid), .. } | HeadState::Detached { oid } => oid,
        other => panic!("unexpected head {other:?}"),
    };
    let tree = parse_commit(&repo.objects().read(&head).expect("read").data)
        .expect("parse")
        .tree;
    let sig: Signature = "A <a@b.c> 0 +0000".parse().unwrap();
    let bad = repo
        .objects()
        .write_commit(&NewCommit::new(tree, sig.clone(), sig, "bad stash\n").with_parents([head]))
        .expect("write");
    let err = repo
        .stash()
        .apply_commit(
            &bad,
            &StashApplyOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .expect_err("single-parent commit");
    match err {
        StashError::CorruptStash { reason, .. } => {
            assert_eq!(reason, CorruptStashReason::InsufficientParents);
        }
        other => panic!("expected corrupt stash, got {other:?}"),
    }
}

#[test]
fn stash_error_legacy_kind_for_serialization() {
    let json = serde_json::to_value(StashError::NotFound { n: 2 }).expect("json");
    assert_eq!(json["kind"], "stash_not_found");
    let json = serde_json::to_value(StashError::CouldNotRestoreUntracked {
        path: "u.txt".into(),
    })
    .expect("json");
    assert_eq!(json["kind"], "stash_could_not_restore_untracked");
}
