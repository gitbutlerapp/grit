//! Conformance suite for [`grit_lib::refs::store::ReftableRefStore`].

use std::sync::Arc;

use grit_lib::refs::store::{RefStore, ReftableRefStore};
use grit_test_support::refstore_conformance::run_suite;

#[test]
fn reftable_refstore_passes_conformance_suite() {
    run_suite(|| {
        let store = ReftableRefStore::open_ephemeral().expect("open reftable store");
        Arc::new(store) as Arc<dyn RefStore>
    });
}
