//! [`custom_ref_store`] binary and [`CountingRefStore`] wrapper.

use std::process::Command;
use std::sync::Arc;

use grit_examples::counting_ref_store::CountingRefStore;
use grit_lib::objects::ObjectId;
use grit_lib::refs::list_refs_for_repository;
use grit_lib::refs::store::{
    Expected, MemoryRefStore, RawRef, RefStore, RefTransaction, RefUpdate,
};
use grit_lib::repo::Repository;
use grit_lib::rev_parse::resolve_revision;

#[test]
fn counting_ref_store_tracks_operations() -> grit_lib::error::Result<()> {
    let repo = Repository::discover(None)?;
    let oid: ObjectId = "0100000000000000000000000000000000000000".parse()?;

    let memory = Arc::new(MemoryRefStore::new());
    memory.set_ref("refs/heads/wrapped", RawRef::Direct(oid))?;
    let counting = Arc::new(CountingRefStore::new(memory));
    let repo = repo.with_ref_store(counting.clone());

    assert_eq!(resolve_revision(&repo, "refs/heads/wrapped")?, oid);
    assert!(counting.read_raw_calls() >= 1);

    let listed = list_refs_for_repository(&repo, "refs/heads/")?;
    assert!(listed.iter().any(|(n, _)| n == "refs/heads/wrapped"));
    assert!(counting.for_each_ref_calls() >= 1);

    let txn = RefTransaction::new().update(RefUpdate {
        name: "refs/heads/wrapped".to_owned(),
        new_value: Some(RawRef::Direct(oid)),
        expected: Expected::Oid(oid),
        reflog: None,
        flags: grit_lib::refs::store::RefUpdateFlags::default(),
    })?;
    counting.prepare(txn)?.commit()?;
    assert_eq!(counting.prepare_calls(), 1);
    Ok(())
}

#[test]
fn custom_ref_store_example_runs() {
    let output = Command::new(env!("CARGO_BIN_EXE_custom_ref_store"))
        .output()
        .expect("run custom_ref_store");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("read_raw_calls="));
    assert!(stdout.contains("prepare_calls="));
}
