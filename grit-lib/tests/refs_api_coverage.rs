//! Additional refs API paths: DWIM, packed-refs, reflog policy, and CAS errors.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;

use std::fs;

use grit_lib::refs::check_ref_format::{RefName, RefNamePolicy, RefNameRef};
use grit_lib::refs::{lock_path_for_ref, PackedRefs, RawRefLookup};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::test_support::refs::{
    effective_log_refs_config, pack_remote_tracking_refs_for_clone, packed_refs_entry_exists,
    read_log_refs_config, read_raw_ref, resolve_at_n_branch, resolve_ref_cached,
    should_autocreate_reflog, write_ref_cached,
};
use tempfile::tempdir;

#[test]
fn ref_name_validation_and_dwim_helpers() {
    assert!(RefName::parse("refs/heads/main", &RefNamePolicy::default().storable()).is_ok());
    assert!(RefName::parse("bad name", &RefNamePolicy::default().storable()).is_err());
    assert!(RefName::parse("refs/heads/x", &RefNamePolicy::default().advertised_fetch()).is_ok());
    assert!(RefNameRef::parse("HEAD", &RefNamePolicy::default().storable()).is_ok());
}

#[test]
fn packed_refs_load_and_namespace_conflict() {
    let dir = tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    let oid = |n: u8| support::stored_commit(&git_dir, n);
    grit_test_support::refs::set_ref(
        &grit_test_support::refs::at(&git_dir),
        "refs/heads/packed",
        &oid(1),
    )
    .expect("write");
    fs::write(
        git_dir.join("packed-refs"),
        format!(
            "# pack-refs with: peeled tags\n{} refs/heads/packed\n",
            oid(1).to_hex()
        ),
    )
    .expect("packed");
    let mut packed = PackedRefs::load(&git_dir).expect("load");
    assert!(packed_refs_entry_exists(&git_dir, "refs/heads/packed").expect("exists"));
    assert!(packed.has_namespace_conflict(
        grit_lib::refs::store::RefNameRef::new("refs/heads/packed/extra").expect("valid ref name")
    ));
    write_ref_cached(&git_dir, "refs/heads/cached-new", &oid(8), &mut packed)
        .expect("cached write");
    assert_eq!(
        resolve_ref_cached(&git_dir, "refs/heads/cached-new", &packed)
            .expect("cached resolve")
            .expect("oid"),
        oid(8)
    );
}

#[test]
fn write_ref_cas_and_delete_cas_error_paths() {
    let dir = tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    let oid = |n: u8| support::stored_commit(&git_dir, n);
    grit_test_support::refs::set_ref(
        &grit_test_support::refs::at(&git_dir),
        "refs/heads/cas",
        &oid(1),
    )
    .expect("seed");
    assert!(grit_test_support::refs::at(&git_dir)
        .references()
        .compare_and_swap(
            grit_lib::refs::store::RefNameRef::new("refs/heads/cas").expect("valid ref name"),
            &oid(9),
            &oid(2)
        )
        .is_err());
    grit_test_support::refs::at(&git_dir)
        .references()
        .compare_and_swap(
            grit_lib::refs::store::RefNameRef::new("refs/heads/cas").expect("valid ref name"),
            &oid(1),
            &oid(2),
        )
        .expect("cas ok");
    assert_eq!(
        resolve_ref_cached(
            &git_dir,
            "refs/heads/cas",
            &PackedRefs::load(&git_dir).expect("load")
        )
        .expect("resolve")
        .expect("oid"),
        oid(2)
    );
    grit_test_support::refs::at(&git_dir)
        .references()
        .compare_and_delete(
            grit_lib::refs::store::RefNameRef::new("refs/heads/cas").expect("valid ref name"),
            &oid(2),
        )
        .expect("delete cas");
}

#[test]
fn reflog_policy_and_append() {
    let dir = tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    let oid = |n: u8| support::stored_commit(&git_dir, n);
    grit_test_support::refs::set_ref(
        &grit_test_support::refs::at(&git_dir),
        "refs/heads/logged",
        &oid(2),
    )
    .expect("write");
    let mode = read_log_refs_config(&git_dir);
    let effective = effective_log_refs_config(&git_dir);
    assert!(should_autocreate_reflog(&git_dir, "refs/heads/logged"));
    let _ = effective;
    let _ = mode;
    grit_test_support::refs::at(&git_dir)
        .reflogs()
        .append(
            grit_lib::refs::store::RefNameRef::new("refs/heads/logged").expect("valid ref name"),
            &grit_lib::refs::reflog::ReflogEntry {
                old_oid: oid(1),
                new_oid: oid(2),
                identity: ("Tester <t@e.com> 1700000000 +0000").to_string(),
                message: ("coverage").to_string(),
            },
            grit_lib::repo_handles::ReflogCreate::Always,
        )
        .expect("append");
}

#[test]
fn list_refs_resolve_dwim_and_symbolic() {
    let dir = tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    let oid = |n: u8| support::stored_commit(&git_dir, n);
    grit_test_support::refs::set_ref(
        &grit_test_support::refs::at(&git_dir),
        "refs/heads/topic",
        &oid(3),
    )
    .expect("write");
    grit_test_support::refs::at(&git_dir)
        .references()
        .update_symbolic(
            grit_lib::refs::store::RefNameRef::new("refs/heads/sym").expect("valid ref name"),
            grit_lib::refs::store::RefNameRef::new("refs/heads/topic").expect("valid ref name"),
        )
        .expect("sym");
    let listed = grit_test_support::refs::at(&git_dir)
        .references()
        .list("refs/heads/")
        .map(|refs| {
            refs.into_iter()
                .map(|(n, o)| (String::from(n), o))
                .collect::<Vec<_>>()
        })
        .expect("list");
    assert!(listed.iter().any(|(n, _)| n.contains("topic")));
    let dwim = grit_test_support::refs::at(&git_dir)
        .references()
        .dwim("topic")
        .expect("dwim")
        .expect("topic matches");
    assert!(dwim.name.as_str().ends_with("/topic"));
    let head = grit_test_support::refs::at(&git_dir)
        .references()
        .symbolic(grit_lib::refs::store::RefNameRef::new("HEAD").expect("valid ref name"))
        .map(|t| t.map(String::from))
        .expect("head");
    assert!(head.as_deref().is_some_and(|h| h.contains("main")));
    let _ = resolve_at_n_branch(&git_dir, "HEAD");
    let _ = grit_test_support::refs::at(&git_dir)
        .references()
        .list_glob("refs/heads/t*")
        .map(|refs| {
            refs.into_iter()
                .map(|(n, o)| (String::from(n), o))
                .collect::<Vec<_>>()
        })
        .expect("glob");
    let path = git_dir.join("refs/heads/topic");
    let _ = lock_path_for_ref(&path);
    assert!(matches!(
        read_raw_ref(&git_dir, "refs/heads/topic").expect("raw"),
        RawRefLookup::Exists
    ));
}

#[test]
fn pack_remote_tracking_refs_for_clone_smoke() {
    let dir = tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    let oid = |n: u8| support::stored_commit(&git_dir, n);
    let mut cfg = fs::read_to_string(git_dir.join("config")).expect("cfg");
    cfg.push_str("[remote \"origin\"]\n\turl = https://example.com/r.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n");
    fs::write(git_dir.join("config"), cfg).expect("write cfg");
    grit_test_support::refs::set_ref(
        &grit_test_support::refs::at(&git_dir),
        "refs/remotes/origin/main",
        &oid(4),
    )
    .expect("remote tracking");
    pack_remote_tracking_refs_for_clone(&git_dir, "origin").expect("pack remote");
}

#[test]
fn ref_store_as_files_names_only_the_files_backend() {
    use grit_lib::refs::store::{FilesRefStore, MemoryRefStore, RefStore, ReftableRefStore};

    let dir = tempdir().expect("tempdir");
    let files = FilesRefStore::from_git_dir(dir.path()).expect("files store");
    let dyn_files: &dyn RefStore = &files;
    let backend = dyn_files.as_files().expect("files backend");
    assert!(std::ptr::eq(backend, &files));

    assert!(MemoryRefStore::new().as_files().is_none());
    let reftable = ReftableRefStore::open_ephemeral().expect("reftable store");
    assert!(reftable.as_files().is_none());
}
