//! Loose-ref lock collision and broken-ref update errors.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;

use grit_lib::objects::ObjectId;
use grit_lib::refs::{
    delete_ref, delete_ref_cas, lock_path_for_ref, read_raw_ref,
    verify_refname_available_for_create, write_ref, RawRefLookup,
};
use grit_lib::repo::init_repository;
use std::collections::{BTreeSet, HashSet};
use tempfile::tempdir;

fn oid(byte: u8) -> ObjectId {
    ObjectId::from_bytes(&[byte; 20]).expect("oid")
}

#[test]
fn read_raw_ref_reports_directory_at_ref_path() {
    let dir = tempdir().expect("tempdir");
    init_repository(
        dir.path(),
        false,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    fs::create_dir_all(git_dir.join("refs/heads/dir-as-ref")).expect("dir ref");
    assert!(matches!(
        read_raw_ref(&git_dir, "refs/heads/dir-as-ref").expect("read"),
        RawRefLookup::IsDirectory
    ));
    assert!(matches!(
        read_raw_ref(&git_dir, "refs/heads/no-such-ref").expect("read"),
        RawRefLookup::NotFound
    ));
}

#[test]
fn write_ref_fails_when_lock_file_exists() {
    let dir = tempdir().expect("tempdir");
    init_repository(
        dir.path(),
        false,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    let ref_path = git_dir.join("refs/heads/locked");
    fs::create_dir_all(ref_path.parent().unwrap()).expect("parent");
    fs::write(&ref_path, format!("{}\n", oid(1).to_hex())).expect("seed");
    fs::write(lock_path_for_ref(&ref_path), b"held").expect("lock");
    assert!(write_ref(&git_dir, "refs/heads/locked", &oid(2)).is_err());
}

#[test]
fn write_ref_rejects_broken_existing_ref_file() {
    let dir = tempdir().expect("tempdir");
    init_repository(
        dir.path(),
        false,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    fs::write(git_dir.join("refs/heads/broken"), b"not-a-valid-oid\n").expect("broken");
    assert!(write_ref(&git_dir, "refs/heads/broken", &oid(3)).is_err());
}

#[test]
fn delete_ref_cas_and_directory_in_the_way() {
    let dir = tempdir().expect("tempdir");
    init_repository(
        dir.path(),
        false,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    write_ref(&git_dir, "refs/heads/del", &oid(4)).expect("seed");
    delete_ref_cas(&git_dir, "refs/heads/del", oid(4)).expect("delete");
    write_ref(&git_dir, "refs/heads/parent/leaf", &oid(5)).expect("child");
    assert!(write_ref(&git_dir, "refs/heads/parent", &oid(6)).is_err());
    delete_ref(&git_dir, "refs/heads/parent/leaf").expect("cleanup");
    let extras = BTreeSet::new();
    let skip = HashSet::new();
    assert!(
        verify_refname_available_for_create(&git_dir, "refs/heads/new", &extras, &skip).is_ok()
    );
}
