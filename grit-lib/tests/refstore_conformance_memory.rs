//! Conformance suite for [`grit_lib::refs::store::MemoryRefStore`].

use std::sync::Arc;

use grit_lib::refs::store::{MemoryRefStore, RefStore};
use grit_test_support::refstore_conformance::run_suite;

#[test]
fn memory_refstore_passes_conformance_suite() {
    run_suite(|| Arc::new(MemoryRefStore::new()) as Arc<dyn RefStore>);
}
