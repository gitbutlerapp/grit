//! `verify_ref_transaction_batch` and refname availability edge paths.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeSet, HashSet};
use std::fs;

use grit_lib::objects::ObjectId;
use grit_lib::refs::write_ref;
use grit_lib::refs::{
    verify_ref_transaction_batch, verify_refname_available_for_create, RefBatchItem,
    RefnameUnavailable,
};
use grit_lib::repo::init_repository;
use tempfile::tempdir;

fn oid(byte: u8) -> ObjectId {
    ObjectId::from_bytes(&[byte; 20]).expect("oid")
}

#[test]
fn verify_batch_rejects_prefix_pair_in_same_transaction() {
    let dir = tempdir().expect("tempdir");
    init_repository(dir.path(), false, "main", None, "files").expect("init");
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
    init_repository(dir.path(), false, "main", None, "files").expect("init");
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
    init_repository(dir.path(), false, "main", None, "files").expect("init");
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
