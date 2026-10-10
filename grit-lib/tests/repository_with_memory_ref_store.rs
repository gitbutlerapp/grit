//! [`Repository::with_ref_store`] injects a [`MemoryRefStore`] for rev-parse and listing.

use std::sync::Arc;

use grit_lib::objects::ObjectId;
use grit_lib::refs::list_refs_for_repository;
use grit_lib::refs::store::{
    Expected, MemoryRefStore, RawRef, RefStore, RefTransaction, RefUpdate,
};
use grit_lib::repo::Repository;
use grit_lib::rev_parse::resolve_revision;

#[test]
fn repository_with_memory_ref_store() {
    let repo = Repository::discover(None).expect("discover");

    let oid: ObjectId = "0100000000000000000000000000000000000000"
        .parse()
        .expect("oid");
    let store = Arc::new(MemoryRefStore::new());
    store
        .prepare(
            RefTransaction::new()
                .update(RefUpdate {
                    name: "refs/heads/injected".to_owned(),
                    new_value: Some(RawRef::Direct(oid)),
                    expected: Expected::Any,
                    reflog: None,
                    flags: grit_lib::refs::store::RefUpdateFlags::default(),
                })
                .expect("txn"),
        )
        .expect("prepare")
        .commit()
        .expect("commit");

    let repo = repo.with_ref_store(store);
    assert_eq!(
        repo.refs().format(),
        grit_lib::refs::store::RefStorageFormat::Memory
    );

    let listed = list_refs_for_repository(&repo, "refs/heads/").expect("list");
    assert!(
        listed
            .iter()
            .any(|(name, id)| name == "refs/heads/injected" && *id == oid),
        "list_refs_for_repository should list injected ref"
    );

    let resolved = resolve_revision(&repo, "refs/heads/injected").expect("rev-parse");
    assert_eq!(resolved, oid);
}
