//! Inject a counting in-memory ref store through [`Repository::with_ref_store`].
//!
//! Demonstrates embedder-owned ref storage without touching on-disk refs.

use std::sync::Arc;

use grit_examples::counting_ref_store::CountingRefStore;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;

use grit_lib::refs::store::{
    Expected, MemoryRefStore, RawRef, RefName, RefStore, RefTransaction, RefUpdate,
};
use grit_lib::repo::Repository;
use grit_lib::revwalk::ResolveOptions;

fn main() -> grit_lib::error::Result<()> {
    let repo = Repository::discover(None, &RepositoryOptions::from_process())?;
    let oid: ObjectId = "0100000000000000000000000000000000000000".parse()?;

    let memory = Arc::new(MemoryRefStore::new());
    memory.set_ref("refs/heads/custom-demo", RawRef::Direct(oid))?;
    let counting = Arc::new(CountingRefStore::new(memory));
    let repo = repo.with_ref_store(counting.clone());

    let resolved = repo
        .revwalk()
        .resolve("refs/heads/custom-demo", &ResolveOptions::default())?;
    let listed = repo.references().list("refs/heads/").map(|refs| {
        refs.into_iter()
            .map(|(n, o)| (String::from(n), o))
            .collect::<Vec<_>>()
    })?;

    println!("resolved={resolved}");
    println!("listed={}", listed.len());
    println!("read_raw_calls={}", counting.read_raw_calls());
    println!("for_each_ref_calls={}", counting.for_each_ref_calls());

    let txn = RefTransaction::new().update(
        RefUpdate::new(
            RefName::new("refs/heads/custom-demo")?,
            Some(RawRef::Direct(oid)),
        )
        .with_expected(Expected::Oid(oid)),
    )?;
    counting.prepare(txn)?.commit()?;
    println!("prepare_calls={}", counting.prepare_calls());

    Ok(())
}
