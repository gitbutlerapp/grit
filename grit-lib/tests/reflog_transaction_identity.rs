//! Public ref transactions must reject forged reflog identity strings (issue #1972).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;

use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::refs::commit_ref_update_on_store;
use grit_lib::refs::store::{
    Expected, MemoryRefStore, RawRef, RefStore, RefStoreError, RefTransaction, RefUpdate,
    RefUpdateFlags, ReflogUpdate,
};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::RefStorageFormat;
use support::require_reftable_git;
use tempfile::TempDir;

fn forged_log() -> ReflogUpdate {
    ReflogUpdate {
        identity: "Evil <a@b>\nforged".to_owned(),
        message: "test".to_owned(),
        time: time::OffsetDateTime::from_unix_timestamp(1).expect("time"),
        old_oid: Some(ObjectId::zero()),
        new_oid: Some(ObjectId::zero()),
    }
}

fn forged_head_update(tip: ObjectId) -> RefUpdate {
    RefUpdate {
        name: "HEAD".to_owned(),
        new_value: Some(RawRef::Direct(tip)),
        expected: Expected::Any,
        reflog: Some(forged_log()),
        flags: RefUpdateFlags {
            no_deref: true,
            log_only: true,
        },
    }
}

fn assert_invalid_reflog_signature(err: Error) {
    let is_invalid = matches!(err, Error::InvalidSignature(_))
        || matches!(err, Error::RefStore(RefStoreError::InvalidReflog(_)));
    assert!(is_invalid, "expected invalid reflog/signature, got {err:?}");
}

fn seed_main_commit(repo: &Repository, dir: &TempDir) -> ObjectId {
    use grit_lib::index::write_tree::{write_tree_update_index, WriteTreeFlags};
    use grit_lib::objects::{serialize_commit, CommitData, ObjectKind};
    use grit_lib::refs;

    std::fs::write(dir.path().join("f"), "x\n").expect("write");
    let mut index = repo.index_handle().load().expect("index");
    let tree = write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent())
        .expect("tree");
    let commit = CommitData {
        tree,
        parents: vec![],
        author: "T <t@t> 0 +0000".into(),
        committer: "T <t@t> 0 +0000".into(),
        author_raw: vec![],
        committer_raw: vec![],
        encoding: None,
        message: "c\n".into(),
        raw_message: None,
        extra_headers: vec![],
    };
    let oid = repo
        .odb()
        .write(ObjectKind::Commit, &serialize_commit(&commit))
        .expect("commit");
    refs::write_ref(repo.git_dir(), "refs/heads/main", &oid).expect("branch");
    refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main").expect("sym");
    oid
}

#[test]
fn memory_store_rejects_forged_reflog_identity() {
    let store = MemoryRefStore::new();
    let tip = ObjectId::from_hex("0000000000000000000000000000000000000001").expect("oid");
    store
        .set_ref("refs/heads/main", RawRef::Direct(tip))
        .expect("seed ref");

    let err = commit_ref_update_on_store(&store, forged_head_update(tip)).unwrap_err();
    assert_invalid_reflog_signature(err);
}

#[test]
fn memory_store_direct_prepare_rejects_forged_reflog_identity() {
    let store = MemoryRefStore::new();
    let tip = ObjectId::from_hex("0000000000000000000000000000000000000001").expect("oid");
    store
        .set_ref("refs/heads/main", RawRef::Direct(tip))
        .expect("seed ref");

    let txn = RefTransaction::new()
        .update(forged_head_update(tip))
        .expect("txn");
    let err = match store.prepare(txn) {
        Err(e) => e,
        Ok(_) => panic!("prepare must reject forged reflog identity"),
    };
    assert!(matches!(err, RefStoreError::InvalidReflog(_)));
}

#[test]
fn files_store_rejects_forged_reflog_identity_via_wrapper() {
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    let commit_oid = seed_main_commit(&repo, &dir);

    let err = commit_ref_update_on_store(repo.refs(), forged_head_update(commit_oid)).unwrap_err();
    assert_invalid_reflog_signature(err);

    let log_path = repo.git_dir().join("logs/HEAD");
    let raw = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        !raw.contains("Evil"),
        "forged identity must not be written: {raw:?}"
    );
}

#[test]
fn files_store_direct_prepare_rejects_forged_reflog_identity() {
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    let commit_oid = seed_main_commit(&repo, &dir);

    let txn = RefTransaction::new()
        .update(forged_head_update(commit_oid))
        .expect("txn");
    let err = match repo.refs().prepare(txn) {
        Err(e) => e,
        Ok(_) => panic!("prepare must reject forged reflog identity"),
    };
    assert!(matches!(err, RefStoreError::InvalidReflog(_)));

    let log_path = repo.git_dir().join("logs/HEAD");
    let raw = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        !raw.contains("Evil"),
        "forged identity must not be written: {raw:?}"
    );
}

#[test]
fn reftable_store_direct_prepare_rejects_forged_reflog_identity() {
    if !require_reftable_git() {
        return;
    }
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Reftable),
    )
    .expect("init");
    let commit_oid = seed_main_commit(&repo, &dir);

    let txn = RefTransaction::new()
        .update(forged_head_update(commit_oid))
        .expect("txn");
    let err = match repo.refs().prepare(txn) {
        Err(e) => e,
        Ok(_) => panic!("prepare must reject forged reflog identity"),
    };
    assert!(matches!(err, RefStoreError::InvalidReflog(_)));
}
