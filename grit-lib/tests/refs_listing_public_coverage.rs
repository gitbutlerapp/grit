//! Coverage for [`grit_lib::refs::listing`] public API.

#![allow(clippy::unwrap_used)]

use grit_lib::objects::ObjectKind;
use grit_lib::refs::listing::query_listings_on_store;
use grit_lib::refs::listing::{
    compare_version_names, creator_date_from_bytes, epoch_from_identity,
    pattern_matches_short_name, query_listings_from_pairs, ref_kind_from_name,
    short_name_in_namespace, RefKind, RefNamespace, RefQuery, RefQueryOptions, RefSort, SortKey,
};
use grit_lib::refs::store::{MemoryRefStore, RawRef};

#[test]
fn listing_public_helpers_smoke() {
    assert_eq!(ref_kind_from_name("refs/heads/main"), RefKind::Head);
    assert_eq!(
        short_name_in_namespace("refs/tags/v1", &RefNamespace::Tags),
        "v1"
    );
    assert!(pattern_matches_short_name("v1.0-rc1", "*rc*", false));
    assert_eq!(
        compare_version_names("a2", "a10", &[]),
        std::cmp::Ordering::Less
    );
    assert_eq!(epoch_from_identity("T <t@e> 42 +0000"), 42);
    let commit = b"commit 123\0tree x\nauthor T <t@e> 1 +0000\ncommitter T <t@e> 99 +0000\n\n";
    assert_eq!(creator_date_from_bytes(ObjectKind::Commit, commit), 99);
}

#[test]
fn query_listings_from_pairs_and_store() {
    let pairs = vec![
        (
            "refs/tags/b".to_owned(),
            "0000000000000000000000000000000000000001".parse().unwrap(),
            None,
        ),
        (
            "refs/tags/a".to_owned(),
            "0000000000000000000000000000000000000002".parse().unwrap(),
            None,
        ),
    ];
    let query = RefQuery {
        namespaces: vec![RefNamespace::Tags],
        patterns: vec![],
        sort: RefSort {
            key: SortKey::Name,
            reverse: false,
        },
        limit: Some(1),
    };
    let out = query_listings_from_pairs(pairs, &query, &RefQueryOptions::default(), None).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].full_name, "refs/tags/a");

    let store = MemoryRefStore::new();
    store
        .set_ref(
            "refs/heads/x",
            RawRef::Direct("0000000000000000000000000000000000000003".parse().unwrap()),
        )
        .unwrap();
    let heads = query_listings_on_store(
        &store,
        &RefQuery {
            namespaces: vec![RefNamespace::Heads],
            ..RefQuery::default()
        },
        &RefQueryOptions::default(),
        None,
    )
    .unwrap();
    assert_eq!(heads.len(), 1);
}
