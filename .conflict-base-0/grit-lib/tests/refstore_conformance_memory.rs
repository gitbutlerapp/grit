//! Conformance suite for [`grit_lib::refs::store::MemoryRefStore`].

use std::sync::Arc;

use grit_lib::refs::store::{MemoryRefStore, RefStore};

mod support;

#[test]
fn memory_refstore_passes_conformance_suite() {
    let factory = Arc::new(|| Arc::new(MemoryRefStore::new()) as Arc<dyn RefStore>);
    support::run_refstore_conformance(factory);
}
