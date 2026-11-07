//! Extra refs helpers for line coverage (alternates, globs, packed listing).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;

use std::fs;

use grit_lib::refs::check_ref_format::{RefName, RefNameError, RefNamePolicy, RefNameRef};
use grit_lib::refs::ref_matches_glob;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::test_support::refs::collect_alternate_ref_oids;
use tempfile::tempdir;

#[test]
fn ref_matches_glob_wildcards_and_exact() {
    assert!(ref_matches_glob("refs/heads/main", "refs/heads/main"));
    assert!(ref_matches_glob(
        "refs/heads/feature/x",
        "refs/heads/feature/*"
    ));
    assert!(ref_matches_glob("refs/heads/a", "refs/heads/?"));
    assert!(ref_matches_glob("refs/heads/main", "main"));
    assert!(!ref_matches_glob("refs/heads/main", "refs/tags/*"));
}

#[test]
fn list_refs_glob_prefix_and_pattern() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path();
    Repository::init(
        root,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )
    .expect("init");
    let git_dir = root.join(".git");
    let oid = support::stored_commit(&git_dir, 1);
    grit_test_support::refs::set_ref(
        &grit_test_support::refs::at(&git_dir),
        "refs/heads/topic/one",
        &oid,
    )
    .expect("write");
    grit_test_support::refs::set_ref(
        &grit_test_support::refs::at(&git_dir),
        "refs/heads/topic/two",
        &oid,
    )
    .expect("write");
    grit_test_support::refs::set_ref(
        &grit_test_support::refs::at(&git_dir),
        "refs/heads/other",
        &oid,
    )
    .expect("write");

    let globbed = grit_test_support::refs::at(&git_dir)
        .references()
        .list_glob("refs/heads/topic/*")
        .map(|refs| {
            refs.into_iter()
                .map(|(n, o)| (String::from(n), o))
                .collect::<Vec<_>>()
        })
        .expect("glob");
    assert_eq!(globbed.len(), 2);
    let prefix = grit_test_support::refs::at(&git_dir)
        .references()
        .list_glob("refs/heads/o*")
        .map(|refs| {
            refs.into_iter()
                .map(|(n, o)| (String::from(n), o))
                .collect::<Vec<_>>()
        })
        .expect("prefix glob");
    assert_eq!(prefix.len(), 1);
}

#[test]
fn collect_alternate_ref_oids_reads_linked_object_store() {
    let dir = tempdir().expect("tempdir");
    let primary = dir.path().join("primary");
    let alt = dir.path().join("alternate");
    Repository::init(
        &primary,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )
    .expect("init primary");
    Repository::init(
        &alt,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )
    .expect("init alt");
    let alt_git = alt.join(".git");
    let oid = support::stored_commit(&alt_git, 2);
    grit_test_support::refs::set_ref(
        &grit_test_support::refs::at(&alt_git),
        "refs/heads/from-alt",
        &oid,
    )
    .expect("alt ref");

    let primary_git = primary.join(".git");
    let objects = primary_git.join("objects");
    fs::write(
        objects.join("info/alternates"),
        format!("{}\n", alt_git.join("objects").display()),
    )
    .expect("alternates");

    let oids = collect_alternate_ref_oids(&primary_git).expect("collect");
    assert!(oids.contains(&oid));
}

#[test]
fn collect_alternate_ref_oids_honors_prefix_config() {
    let dir = tempdir().expect("tempdir");
    let primary = dir.path().join("primary");
    let alt = dir.path().join("alternate");
    Repository::init(
        &primary,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )
    .expect("init primary");
    Repository::init(
        &alt,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )
    .expect("init alt");
    let alt_git = alt.join(".git");
    let oid = support::stored_commit(&alt_git, 3);
    grit_test_support::refs::set_ref(
        &grit_test_support::refs::at(&alt_git),
        "refs/heads/picked",
        &oid,
    )
    .expect("alt ref");
    grit_test_support::refs::set_ref(
        &grit_test_support::refs::at(&alt_git),
        "refs/tags/skipped",
        &oid,
    )
    .expect("tag");

    let primary_git = primary.join(".git");
    fs::write(
        primary_git.join("config"),
        "[core]\n\talternateRefsPrefixes = refs/heads/\n",
    )
    .expect("config");
    let objects = primary_git.join("objects");
    fs::write(
        objects.join("info/alternates"),
        format!("{}\n", alt_git.join("objects").display()),
    )
    .expect("alternates");

    let oids = collect_alternate_ref_oids(&primary_git).expect("collect");
    assert!(oids.contains(&oid));
    assert_eq!(oids.len(), 1);
}

#[test]
fn ref_name_policy_edge_cases_for_coverage() {
    let format = RefNamePolicy::default();
    let storable = RefNamePolicy::default().storable();
    let safe = RefNamePolicy::default().safe_for_fs();
    let fetch_ad = RefNamePolicy::default().advertised_fetch();
    let receive = RefNamePolicy::default().receive_pack(false);
    let symref = RefNamePolicy::new().allow_onelevel();

    assert!(matches!(
        RefName::parse("", &format),
        Err(RefNameError::Empty)
    ));
    assert!(RefName::parse("refs/heads/../escape", &safe).is_err());
    assert!(RefName::parse("not_upper", &safe).is_err());
    assert!(RefName::parse("refs/heads/main", &receive).is_ok());
    assert!(RefName::parse("heads/main", &receive).is_err());
    assert!(matches!(RefName::branch(""), Err(RefNameError::Empty)));
    assert!(matches!(
        RefName::branch("-bad"),
        Err(RefNameError::BranchStartsWithDash)
    ));
    assert!(matches!(RefName::tag(""), Err(RefNameError::Empty)));
    assert!(RefName::tag("v1.0").is_ok());
    assert!(RefName::parse("refs/heads/main", &fetch_ad).is_ok());
    assert!(RefName::parse("HEAD", &fetch_ad).is_err());
    assert!(RefNameRef::parse("HEAD", &symref).is_ok());
    assert!(RefNameRef::parse("@", &symref).is_err());
    assert_eq!(
        RefName::parse("refs//heads///x", &RefNamePolicy::default().normalize())
            .expect("normalize")
            .as_str(),
        "refs/heads/x"
    );
    assert!(matches!(
        RefName::parse("@", &format),
        Err(RefNameError::LoneAt)
    ));
    assert!(matches!(
        RefName::branch("HEAD"),
        Err(RefNameError::ReservedBranchName)
    ));
    assert!(RefName::parse("HEAD", &storable).is_ok());
}
