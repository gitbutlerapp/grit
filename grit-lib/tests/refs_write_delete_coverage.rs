//! `write_ref_cas`, `delete_ref`, and physical listing coverage.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::objects::ObjectId;
use grit_lib::refs::{
    delete_ref, list_refs_physical, read_raw_ref, resolve_ref, write_ref, write_ref_cas,
    RawRefLookup,
};
use grit_lib::repo::init_repository;
use tempfile::tempdir;

fn oid(byte: u8) -> ObjectId {
    ObjectId::from_bytes(&[byte; 20]).expect("oid")
}

#[test]
fn write_ref_cas_and_delete_loose_ref() {
    let dir = tempdir().expect("tempdir");
    init_repository(dir.path(), false, "main", None, "files").expect("init");
    let git_dir = dir.path().join(".git");
    let name = "refs/heads/cas";
    write_ref(&git_dir, name, &oid(1)).expect("create");
    write_ref_cas(&git_dir, name, &oid(2), oid(1)).expect("cas");
    assert_eq!(resolve_ref(&git_dir, name).expect("resolve"), oid(2));
    delete_ref(&git_dir, name).expect("delete");
    assert!(matches!(
        read_raw_ref(&git_dir, name).expect("gone"),
        RawRefLookup::NotFound
    ));
}

#[test]
fn list_refs_physical_includes_loose_file() {
    let dir = tempdir().expect("tempdir");
    init_repository(dir.path(), false, "main", None, "files").expect("init");
    let git_dir = dir.path().join(".git");
    write_ref(&git_dir, "refs/heads/extra", &oid(9)).expect("write");
    let physical = list_refs_physical(&git_dir, "refs/").expect("list");
    assert!(physical.iter().any(|(n, _)| n == "refs/heads/extra"));
}
