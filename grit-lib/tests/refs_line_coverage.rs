//! Extra refs helpers for line coverage (alternates, globs, packed listing).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;

use grit_lib::objects::ObjectId;
use grit_lib::refs::{collect_alternate_ref_oids, list_refs_glob, ref_matches_glob, write_ref};
use grit_lib::repo::init_repository;
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
    init_repository(root, false, "main", None, "files").expect("init");
    let git_dir = root.join(".git");
    let oid = ObjectId::from_hex("1111111111111111111111111111111111111111").expect("oid");
    write_ref(&git_dir, "refs/heads/topic/one", &oid).expect("write");
    write_ref(&git_dir, "refs/heads/topic/two", &oid).expect("write");
    write_ref(&git_dir, "refs/heads/other", &oid).expect("write");

    let globbed = list_refs_glob(&git_dir, "refs/heads/topic/*").expect("glob");
    assert_eq!(globbed.len(), 2);
    let prefix = list_refs_glob(&git_dir, "refs/heads/o*").expect("prefix glob");
    assert_eq!(prefix.len(), 1);
}

#[test]
fn collect_alternate_ref_oids_reads_linked_object_store() {
    let dir = tempdir().expect("tempdir");
    let primary = dir.path().join("primary");
    let alt = dir.path().join("alternate");
    init_repository(&primary, false, "main", None, "files").expect("init primary");
    init_repository(&alt, false, "main", None, "files").expect("init alt");
    let alt_git = alt.join(".git");
    let oid = ObjectId::from_hex("2222222222222222222222222222222222222222").expect("oid");
    write_ref(&alt_git, "refs/heads/from-alt", &oid).expect("alt ref");

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
    init_repository(&primary, false, "main", None, "files").expect("init primary");
    init_repository(&alt, false, "main", None, "files").expect("init alt");
    let alt_git = alt.join(".git");
    let oid = ObjectId::from_hex("3333333333333333333333333333333333333333").expect("oid");
    write_ref(&alt_git, "refs/heads/picked", &oid).expect("alt ref");
    write_ref(&alt_git, "refs/tags/skipped", &oid).expect("tag");

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
