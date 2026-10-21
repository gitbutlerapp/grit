//! Conformance suite for [`grit_lib::refs::store::ReftableRefStore`].

use std::sync::Arc;

use grit_lib::refs::store::{RefStore, ReftableRefStore};

mod support;

#[test]
fn reftable_refstore_passes_conformance_suite() {
    let factory = Arc::new(|| {
        let store = ReftableRefStore::open_ephemeral().expect("open reftable store");
        Arc::new(store) as Arc<dyn RefStore>
    });
    support::run_refstore_conformance(factory);
}
