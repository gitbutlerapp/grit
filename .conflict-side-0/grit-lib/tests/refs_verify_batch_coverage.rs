//! `verify_ref_transaction_batch` and refname availability edge paths.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeSet, HashSet};
use std::fs;

use grit_lib::objects::ObjectId;
use grit_lib::refs::gc::{update_refs, RefTransactionItem};
use grit_lib::refs::resolve_ref;
use grit_lib::refs::write_ref;
use grit_lib::refs::{
    verify_ref_transaction_batch, verify_refname_available_for_create, RefBatchItem,
    RefnameUnavailable,
};
use grit_lib::repo::{InitOptions, Repository};
use tempfile::tempdir;

fn oid(byte: u8) -> ObjectId {
    ObjectId::from_bytes(&[byte; 20]).expect("oid")
}

#[test]
fn verify_batch_rejects_prefix_pair_in_same_transaction() {
    let dir = tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    let items = vec![
        RefBatchItem {
            name: "refs/heads/parent".into(),
            new_oid: Some(oid(1)),
        },
        RefBatchItem {
            name: "refs/heads/parent/child".into(),
            new_oid: Some(oid(2)),
        },
    ];
    let err = verify_ref_transaction_batch(&git_dir, &items).unwrap_err();
    assert!(matches!(err, RefnameUnavailable::SameBatch { .. }));
}

#[test]
fn verify_refname_blocks_loose_directory_and_packed_descendant() {
    let dir = tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    let extras = BTreeSet::new();
    let skip = HashSet::new();

    fs::create_dir_all(git_dir.join("refs/heads/blocker")).expect("dir");
    fs::write(
        git_dir.join("refs/heads/blocker/existing"),
        format!("{}\n", oid(3).to_hex()),
    )
    .expect("child file");

    let err = verify_refname_available_for_create(&git_dir, "refs/heads/blocker", &extras, &skip)
        .unwrap_err();
    assert!(matches!(err, RefnameUnavailable::DescendantExists { .. }));

    write_ref(&git_dir, "refs/heads/ok", &oid(5)).expect("write");
    assert!(
        verify_refname_available_for_create(&git_dir, "refs/heads/ok-new", &extras, &skip).is_ok()
    );
}

#[test]
fn verify_refname_blocks_packed_descendant_without_loose_children() {
    let dir = tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    let extras = BTreeSet::new();
    let skip = HashSet::new();

    fs::write(
        git_dir.join("packed-refs"),
        format!(
            "# pack-refs with: peeled tags\n{} refs/heads/packed-parent/child\n",
            oid(7).to_hex()
        ),
    )
    .expect("packed-refs");

    let err =
        verify_refname_available_for_create(&git_dir, "refs/heads/packed-parent", &extras, &skip)
            .unwrap_err();
    assert!(matches!(err, RefnameUnavailable::DescendantExists { .. }));
}

#[test]
fn update_refs_create_batch_rejects_later_conflict_without_partial_write() {
    let dir = tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    let git_dir = dir.path().join(".git");

    fs::create_dir_all(git_dir.join("refs/heads/existing")).expect("dir");
    fs::write(
        git_dir.join("refs/heads/existing/child"),
        format!("{}\n", oid(9).to_hex()),
    )
    .expect("child ref");

    let items = vec![
        RefTransactionItem {
            name: "refs/heads/innocent".into(),
            new_oid: Some(oid(10)),
            expected_old: None,
        },
        RefTransactionItem {
            name: "refs/heads/existing".into(),
            new_oid: Some(oid(11)),
            expected_old: None,
        },
    ];
    let err = update_refs(&git_dir, &items).expect_err("descendant conflict");
    let _ = err;
    assert!(
        resolve_ref(&git_dir, "refs/heads/innocent").is_err(),
        "batch must not leave earlier creates on disk when a later create is rejected"
    );
}

#[test]
fn update_refs_create_batch_rejects_remotes_conflict_without_partial_write() {
    let dir = tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    let git_dir = dir.path().join(".git");

    fs::create_dir_all(git_dir.join("refs/remotes/origin")).expect("dir");
    fs::write(
        git_dir.join("refs/remotes/origin/main"),
        format!("{}\n", oid(12).to_hex()),
    )
    .expect("remote ref");

    let items = vec![
        RefTransactionItem {
            name: "refs/heads/innocent".into(),
            new_oid: Some(oid(13)),
            expected_old: None,
        },
        RefTransactionItem {
            name: "refs/remotes/origin".into(),
            new_oid: Some(oid(14)),
            expected_old: None,
        },
    ];
    let err = update_refs(&git_dir, &items).expect_err("descendant conflict");
    let _ = err;
    assert!(
        resolve_ref(&git_dir, "refs/heads/innocent").is_err(),
        "batch must not leave earlier creates when a later create hits refs/remotes D/F"
    );
}

#[test]
fn update_refs_cas_sees_loose_tag_while_heads_tree_empty() {
    let dir = tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    let old_oid = oid(20);
    let new_oid = oid(21);
    write_ref(&git_dir, "refs/tags/v1", &old_oid).expect("write tag");

    update_refs(
        &git_dir,
        &[RefTransactionItem {
            name: "refs/tags/v1".into(),
            new_oid: Some(new_oid),
            expected_old: Some(old_oid),
        }],
    )
    .expect("CAS update tag");
    assert_eq!(resolve_ref(&git_dir, "refs/tags/v1").unwrap(), new_oid);
}
