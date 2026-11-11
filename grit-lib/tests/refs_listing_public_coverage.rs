//! Coverage for [`grit_lib::refs::listing`] public API.

#![allow(clippy::unwrap_used)]

use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::porcelain::tag::CreateTagRequest;
use grit_lib::progress::NullProgress;
use grit_lib::refs::listing::query_listings_on_store;
use grit_lib::refs::listing::{
    compare_version_names, creator_date_from_bytes, epoch_from_identity,
    pattern_matches_short_name, query_listings_from_pairs, ref_kind_from_name,
    short_name_in_namespace, RefKind, RefNamespace, RefQuery, RefQueryOptions, RefSort, SortKey,
};
use grit_lib::refs::store::{MemoryRefStore, RawRef};
use grit_lib::repo::{InitOptions, Repository};

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
    let query = RefQuery::default()
        .with_namespaces(vec![RefNamespace::Tags])
        .with_sort(RefSort::default())
        .with_limit(Some(1));
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
        &RefQuery::default().with_namespaces(vec![RefNamespace::Heads]),
        &RefQueryOptions::default(),
        None,
    )
    .unwrap();
    assert_eq!(heads.len(), 1);
}

#[test]
fn references_query_peels_annotated_tags() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).unwrap();
    let ident = "T <t@example.com> 1700000000 +0000".to_owned();
    let commit = repo
        .commits()
        .create_staged(
            &grit_test_support::empty_commit_request("c", &ident),
            &mut NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .unwrap()
        .oid;
    repo.tags()
        .create(
            &CreateTagRequest::new("v1", commit)
                .with_message(Some("note".into()))
                .with_tagger(Some("T <t@example.com> 1700000000 +0000".into())),
        )
        .unwrap();

    let query = RefQuery::default().with_namespaces(vec![RefNamespace::Tags]);
    let rows = repo
        .references()
        .query(&query, &RefQueryOptions::default())
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].full_name, "refs/tags/v1");
    assert_eq!(rows[0].short_name, "v1");
    assert_eq!(rows[0].kind, RefKind::Tag);
    assert_ne!(rows[0].oid, commit);
    assert_eq!(rows[0].peeled, Some(commit));
}

#[test]
fn references_query_peels_annotated_tag_to_non_commit() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).unwrap();
    let blob: ObjectId = repo.odb().write(ObjectKind::Blob, b"payload").unwrap();
    repo.tags()
        .create(
            &CreateTagRequest::new("of-blob", blob)
                .with_message(Some("annotated".into()))
                .with_tagger(Some("T <t@example.com> 1700000000 +0000".into())),
        )
        .unwrap();

    let rows = repo
        .references()
        .query(
            &RefQuery::default().with_namespaces(vec![RefNamespace::Tags]),
            &RefQueryOptions::default(),
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_ne!(rows[0].oid, blob);
    assert_eq!(rows[0].peeled, Some(blob));
}

#[test]
fn references_query_lightweight_commit_tag_has_no_peel() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).unwrap();
    let ident = "T <t@example.com> 1700000000 +0000".to_owned();
    let commit = repo
        .commits()
        .create_staged(
            &grit_test_support::empty_commit_request("c", &ident),
            &mut NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .unwrap()
        .oid;
    repo.tags()
        .create(&CreateTagRequest::new("light", commit))
        .unwrap();

    let rows = repo
        .references()
        .query(
            &RefQuery::default().with_namespaces(vec![RefNamespace::Tags]),
            &RefQueryOptions::default(),
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].oid, commit);
    assert_eq!(rows[0].peeled, None);
}

#[test]
fn overlapping_namespaces_deduplicate_by_full_name() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).unwrap();
    let ident = "T <t@example.com> 1700000000 +0000".to_owned();
    let commit = repo
        .commits()
        .create_staged(
            &grit_test_support::empty_commit_request("c", &ident),
            &mut NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .unwrap()
        .oid;
    repo.tags()
        .create(&CreateTagRequest::new("t", commit))
        .unwrap();

    let query = RefQuery::default().with_namespaces(vec![
        RefNamespace::Tags,
        RefNamespace::Custom("refs/tags/".to_owned()),
    ]);
    let rows = repo
        .references()
        .query(&query, &RefQueryOptions::default())
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].full_name, "refs/tags/t");
}

#[test]
fn symbolic_ref_listing_does_not_leak_resolved_peel() {
    let commit: ObjectId = "00000000000000000000000000000000000000ab".parse().unwrap();
    let store = MemoryRefStore::new();
    store
        .set_ref("refs/heads/main", RawRef::Direct(commit))
        .unwrap();
    store
        .set_ref(
            "refs/remotes/origin/HEAD",
            RawRef::Symbolic("refs/heads/main".to_owned()),
        )
        .unwrap();

    let rows = query_listings_on_store(
        &store,
        &RefQuery::default().with_namespaces(vec![RefNamespace::Remotes]),
        &RefQueryOptions::default(),
        None,
    )
    .unwrap();
    let sym = rows
        .iter()
        .find(|r| r.full_name == "refs/remotes/origin/HEAD")
        .expect("symbolic ref");
    assert_eq!(sym.oid, commit);
    assert_eq!(sym.peeled, None);
}

#[test]
fn date_sort_keeps_refs_with_missing_objects() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).unwrap();
    let ident = "T <t@example.com> 1700000000 +0000".to_owned();
    let commit = repo
        .commits()
        .create_staged(
            &grit_test_support::empty_commit_request("c", &ident),
            &mut NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .unwrap()
        .oid;
    let store = MemoryRefStore::new();
    store
        .set_ref("refs/tags/good", RawRef::Direct(commit))
        .unwrap();
    let missing: ObjectId = "00000000000000000000000000000000000000ff".parse().unwrap();
    store
        .set_ref("refs/tags/bad", RawRef::Direct(missing))
        .unwrap();

    let rows = query_listings_on_store(
        &store,
        &RefQuery::default()
            .with_namespaces(vec![RefNamespace::Tags])
            .with_sort(RefSort::default().with_key(SortKey::Date)),
        &RefQueryOptions::default(),
        Some(repo.odb()),
    )
    .unwrap();
    assert_eq!(rows.len(), 2);
    let bad = rows
        .iter()
        .find(|r| r.full_name == "refs/tags/bad")
        .expect("bad ref listed");
    assert_eq!(bad.oid, missing);
}

#[test]
fn pairs_query_short_name_uses_longest_namespace_prefix() {
    let pairs = vec![(
        "refs/tags/foo/bar".to_owned(),
        "0000000000000000000000000000000000000001".parse().unwrap(),
        None,
    )];
    let query = RefQuery::default().with_namespaces(vec![
        RefNamespace::Custom("refs/".to_owned()),
        RefNamespace::Tags,
    ]);
    let rows = query_listings_from_pairs(pairs, &query, &RefQueryOptions::default(), None).unwrap();
    assert_eq!(rows[0].short_name, "foo/bar");
}
