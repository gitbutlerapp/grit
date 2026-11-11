//! `write_ref_cas`, `delete_ref`, and physical listing coverage.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;

use grit_lib::refs::RawRefLookup;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::test_support::refs::{list_refs_physical, read_raw_ref};
use tempfile::tempdir;

#[test]
fn write_ref_cas_and_delete_loose_ref() {
    let dir = tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    let oid = |n: u8| support::stored_commit(&git_dir, n);
    let name = "refs/heads/cas";
    grit_test_support::refs::set_ref(&grit_test_support::refs::at(&git_dir), name, &oid(1))
        .expect("create");
    grit_test_support::refs::at(&git_dir)
        .references()
        .compare_and_swap(
            grit_lib::refs::store::RefNameRef::new(&name).expect("valid ref name"),
            &oid(1),
            &oid(2),
        )
        .expect("cas");
    assert_eq!(
        grit_test_support::refs::at(&git_dir)
            .references()
            .resolve(grit_lib::refs::store::RefNameRef::new(&name).expect("valid ref name"))
            .expect("resolve"),
        oid(2)
    );
    grit_test_support::refs::at(&git_dir)
        .references()
        .delete(grit_lib::refs::store::RefNameRef::new(&name).expect("valid ref name"))
        .expect("delete");
    assert!(matches!(
        read_raw_ref(&git_dir, name).expect("gone"),
        RawRefLookup::NotFound
    ));
}

#[test]
fn list_refs_physical_includes_loose_file() {
    let dir = tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    let oid = |n: u8| support::stored_commit(&git_dir, n);
    grit_test_support::refs::set_ref(
        &grit_test_support::refs::at(&git_dir),
        "refs/heads/extra",
        &oid(9),
    )
    .expect("write");
    let physical = list_refs_physical(&git_dir, "refs/").expect("list");
    assert!(physical.iter().any(|(n, _)| n == "refs/heads/extra"));
}
