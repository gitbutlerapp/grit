//! Reftable batch verify, CAS, and D/F regressions for stacked-table reads.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;

use grit_lib::error::Result;
use grit_lib::objects::ObjectId;
use grit_lib::refs::gc::{update_refs, RefTransactionItem};
use grit_lib::refs::reftable::{
    read_write_options, reftable_delete_ref_with_write_options,
    reftable_write_ref_with_write_options, RefValue, ReftableStack,
};
use grit_lib::refs::{verify_ref_transaction_batch, write_ref, RefBatchItem, RefnameUnavailable};
use grit_lib::repo::{InitOptions, Repository};
use tempfile::tempdir;

fn reftable_git_dir() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Reftable),
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    (dir, git_dir)
}

fn write_options_autocompact_off(
    git_dir: &std::path::Path,
) -> grit_lib::refs::reftable::WriteOptions {
    let mut opts = read_write_options(git_dir);
    opts.autocompaction_enabled = false;
    opts
}

fn write_with_autocompact_off(
    git_dir: &std::path::Path,
    refname: &str,
    oid: &ObjectId,
) -> Result<()> {
    let opts = write_options_autocompact_off(git_dir);
    reftable_write_ref_with_write_options(git_dir, refname, oid, None, None, &opts)
}

fn delete_with_autocompact_off(git_dir: &std::path::Path, refname: &str) -> Result<()> {
    let opts = write_options_autocompact_off(git_dir);
    reftable_delete_ref_with_write_options(git_dir, refname, &opts)
}

#[test]
fn reftable_verify_batch_rejects_existing_ancestor_with_unrelated_sibling() {
    let (_dir, git_dir) = reftable_git_dir();
    let oid = |n: u8| support::stored_commit(&git_dir, n);
    write_ref(&git_dir, "refs/heads/parent", &oid(1)).expect("parent");

    let items = vec![
        RefBatchItem {
            name: "refs/heads/parent/child".into(),
            new_oid: Some(oid(2)),
        },
        RefBatchItem {
            name: "refs/heads/unrelated".into(),
            new_oid: Some(oid(3)),
        },
    ];
    let err = verify_ref_transaction_batch(&git_dir, &items).unwrap_err();
    assert!(matches!(
        err,
        RefnameUnavailable::AncestorExists { new_ref, .. } if new_ref == "refs/heads/parent/child"
    ));
}

#[test]
fn reftable_cas_rejects_stale_oid_after_deletion_in_newer_table() {
    let (_dir, git_dir) = reftable_git_dir();
    let oid = |n: u8| support::stored_commit(&git_dir, n);
    let before = oid(0x11);
    let after = oid(0x22);
    write_with_autocompact_off(&git_dir, "refs/heads/cas-target", &before).expect("create");
    delete_with_autocompact_off(&git_dir, "refs/heads/cas-target").expect("delete");

    let stack = ReftableStack::open(&git_dir).expect("open");
    assert!(
        stack.table_names().len() >= 2,
        "need stacked tables for regression, got {}",
        stack.table_names().len()
    );
    assert!(stack
        .lookup_ref_oid("refs/heads/cas-target")
        .unwrap()
        .is_none());

    let err = update_refs(
        &git_dir,
        &[RefTransactionItem {
            name: "refs/heads/cas-target".into(),
            new_oid: Some(after),
            expected_old: Some(before),
        }],
    )
    .expect_err("stale CAS must fail");
    assert!(
        err.to_string().contains("expected"),
        "unexpected error: {err}"
    );
    assert!(stack
        .lookup_ref_oid("refs/heads/cas-target")
        .unwrap()
        .is_none());
}

#[test]
fn reftable_cas_rejects_stale_oid_when_newest_table_is_symref() {
    let (_dir, git_dir) = reftable_git_dir();
    let oid = |n: u8| support::stored_commit(&git_dir, n);
    let before = oid(0x41);
    write_with_autocompact_off(&git_dir, "refs/heads/sym-cas", &before).expect("create");
    write_with_autocompact_off(&git_dir, "refs/heads/stack-pad", &oid(0x99)).expect("pad table");
    let mut opts = read_write_options(&git_dir);
    opts.autocompaction_enabled = false;
    let mut stack = ReftableStack::open(&git_dir).expect("open");
    stack
        .write_ref(
            "refs/heads/sym-cas",
            RefValue::Symref("refs/heads/main".into()),
            None,
            &opts,
        )
        .expect("symref overlay");
    assert!(stack.table_names().len() >= 2);

    let err = update_refs(
        &git_dir,
        &[RefTransactionItem {
            name: "refs/heads/sym-cas".into(),
            new_oid: Some(oid(0x42)),
            expected_old: Some(before),
        }],
    )
    .expect_err("symref current is not the prior oid");
    assert!(
        err.to_string().contains("expected"),
        "unexpected error: {err}"
    );
    assert!(matches!(
        stack.lookup_ref("refs/heads/sym-cas").expect("lookup"),
        Some(rec) if matches!(rec.value, RefValue::Symref(_))
    ));
}

#[test]
fn reftable_verify_batch_allows_parent_after_deleted_descendant() {
    let (_dir, git_dir) = reftable_git_dir();
    let oid = |n: u8| support::stored_commit(&git_dir, n);
    write_with_autocompact_off(&git_dir, "refs/heads/topic/child", &oid(0x31)).expect("child");
    update_refs(
        &git_dir,
        &[RefTransactionItem {
            name: "refs/heads/topic/child".into(),
            new_oid: None,
            expected_old: None,
        }],
    )
    .expect("delete child");

    let items = vec![RefBatchItem {
        name: "refs/heads/topic".into(),
        new_oid: Some(oid(0x32)),
    }];
    verify_ref_transaction_batch(&git_dir, &items).expect("parent create allowed");

    update_refs(
        &git_dir,
        &[RefTransactionItem {
            name: "refs/heads/topic".into(),
            new_oid: Some(oid(0x32)),
            expected_old: None,
        }],
    )
    .expect("create parent");
    assert_eq!(
        grit_lib::refs::resolve_ref(&git_dir, "refs/heads/topic").expect("resolve"),
        oid(0x32)
    );
}
