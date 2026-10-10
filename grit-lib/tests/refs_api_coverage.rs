//! Additional refs API paths: DWIM, packed-refs, reflog policy, and CAS errors.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;

use grit_lib::objects::ObjectId;
use grit_lib::refs::{
    append_reflog, delete_ref_cas, effective_log_refs_config, is_valid_fetch_advertised_ref,
    is_valid_ls_refs_advertised_name, is_valid_storable_ref_name, list_refs, list_refs_glob,
    lock_path_for_ref, pack_remote_tracking_refs_for_clone, packed_refs_entry_exists, read_head,
    read_log_refs_config, read_raw_ref, resolve_at_n_branch, resolve_ref_cached, resolve_ref_dwim,
    should_autocreate_reflog, write_ref, write_ref_cached, write_ref_cas, write_symbolic_ref,
    PackedRefs, RawRefLookup,
};
use grit_lib::repo::init_repository;
use tempfile::tempdir;

fn oid(byte: u8) -> ObjectId {
    ObjectId::from_bytes(&[byte; 20]).expect("oid")
}

#[test]
fn ref_name_validation_and_dwim_helpers() {
    assert!(is_valid_storable_ref_name("refs/heads/main"));
    assert!(!is_valid_storable_ref_name("bad name"));
    assert!(is_valid_fetch_advertised_ref("refs/heads/x"));
    assert!(is_valid_ls_refs_advertised_name("HEAD"));
}

#[test]
fn packed_refs_load_and_namespace_conflict() {
    let dir = tempdir().expect("tempdir");
    init_repository(
        dir.path(),
        false,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    write_ref(&git_dir, "refs/heads/packed", &oid(1)).expect("write");
    fs::write(
        git_dir.join("packed-refs"),
        format!(
            "# pack-refs with: peeled tags\n{} refs/heads/packed\n",
            oid(1).to_hex()
        ),
    )
    .expect("packed");
    let packed = PackedRefs::load(&git_dir).expect("load");
    assert!(packed_refs_entry_exists(&git_dir, "refs/heads/packed").expect("exists"));
    assert!(packed.has_namespace_conflict("refs/heads/packed/extra"));
    write_ref_cached(&git_dir, "refs/heads/cached-new", &oid(8), &packed).expect("cached write");
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
    init_repository(
        dir.path(),
        false,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    write_ref(&git_dir, "refs/heads/cas", &oid(1)).expect("seed");
    assert!(write_ref_cas(&git_dir, "refs/heads/cas", &oid(2), oid(9)).is_err());
    write_ref_cas(&git_dir, "refs/heads/cas", &oid(2), oid(1)).expect("cas ok");
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
    delete_ref_cas(&git_dir, "refs/heads/cas", oid(2)).expect("delete cas");
}

#[test]
fn reflog_policy_and_append() {
    let dir = tempdir().expect("tempdir");
    init_repository(
        dir.path(),
        false,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    write_ref(&git_dir, "refs/heads/logged", &oid(2)).expect("write");
    let mode = read_log_refs_config(&git_dir);
    let effective = effective_log_refs_config(&git_dir);
    assert!(should_autocreate_reflog(&git_dir, "refs/heads/logged"));
    let _ = effective;
    let _ = mode;
    append_reflog(
        &git_dir,
        "refs/heads/logged",
        &oid(1),
        &oid(2),
        "Tester <t@e.com> 1700000000 +0000",
        "coverage",
        true,
    )
    .expect("append");
}

#[test]
fn list_refs_resolve_dwim_and_symbolic() {
    let dir = tempdir().expect("tempdir");
    init_repository(
        dir.path(),
        false,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    write_ref(&git_dir, "refs/heads/topic", &oid(3)).expect("write");
    write_symbolic_ref(&git_dir, "refs/heads/sym", "refs/heads/topic").expect("sym");
    let listed = list_refs(&git_dir, "refs/heads/").expect("list");
    assert!(listed.iter().any(|(n, _)| n.contains("topic")));
    let (_n, oid_opt) = resolve_ref_dwim(&git_dir, "topic");
    assert!(oid_opt.is_some());
    let head = read_head(&git_dir).expect("head");
    assert!(head.as_deref().is_some_and(|h| h.contains("main")));
    let _ = resolve_at_n_branch(&git_dir, "HEAD");
    let _ = list_refs_glob(&git_dir, "refs/heads/t*").expect("glob");
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
    init_repository(
        dir.path(),
        false,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    let mut cfg = fs::read_to_string(git_dir.join("config")).expect("cfg");
    cfg.push_str("[remote \"origin\"]\n\turl = https://example.com/r.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n");
    fs::write(git_dir.join("config"), cfg).expect("write cfg");
    write_ref(&git_dir, "refs/remotes/origin/main", &oid(4)).expect("remote tracking");
    pack_remote_tracking_refs_for_clone(&git_dir, "origin").expect("pack remote");
}
