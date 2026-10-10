//! Inject a counting in-memory ref store through [`Repository::with_ref_store`].
//!
//! Demonstrates embedder-owned ref storage without touching on-disk refs.

use std::sync::Arc;

use grit_examples::counting_ref_store::CountingRefStore;
use grit_lib::objects::ObjectId;
use grit_lib::refs::list_refs_for_repository;
use grit_lib::refs::store::{
    Expected, MemoryRefStore, RawRef, RefStore, RefTransaction, RefUpdate,
};
use grit_lib::repo::Repository;
use grit_lib::rev_parse::resolve_revision;

fn main() -> grit_lib::error::Result<()> {
    let repo = Repository::discover(None)?;
    let oid: ObjectId = "0100000000000000000000000000000000000000".parse()?;

    let memory = Arc::new(MemoryRefStore::new());
    memory.set_ref("refs/heads/custom-demo", RawRef::Direct(oid))?;
    let counting = Arc::new(CountingRefStore::new(memory));
    let repo = repo.with_ref_store(counting.clone());

    let resolved = resolve_revision(&repo, "refs/heads/custom-demo")?;
    let listed = list_refs_for_repository(&repo, "refs/heads/")?;

    println!("resolved={resolved}");
    println!("listed={}", listed.len());
    println!("read_raw_calls={}", counting.read_raw_calls());
    println!("for_each_ref_calls={}", counting.for_each_ref_calls());

    let txn = RefTransaction::new().update(RefUpdate {
        name: "refs/heads/custom-demo".to_owned(),
        new_value: Some(RawRef::Direct(oid)),
        expected: Expected::Oid(oid),
        reflog: None,
        flags: grit_lib::refs::store::RefUpdateFlags::default(),
    })?;
    counting.prepare(txn)?.commit()?;
    println!("prepare_calls={}", counting.prepare_calls());

    Ok(())
}
