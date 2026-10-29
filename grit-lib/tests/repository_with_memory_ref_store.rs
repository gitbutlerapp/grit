//! [`Repository::with_ref_store`] injects a [`MemoryRefStore`] for rev-parse and listing.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::odb::store::MemoryStore;
use grit_lib::objects::odb::OdbBuilder;
use grit_lib::objects::{HashAlgo, ObjectId, ObjectKind};
use grit_lib::porcelain::merge::{resolve_merge_source, MergeSourceKind};
use grit_lib::refs::list_refs_for_repository;
use grit_lib::refs::store::{
    Expected, MemoryRefStore, RawRef, RefName, RefStore, RefTransaction, RefUpdate, ReflogUpdate,
};
use grit_lib::repo::{Repository, RepositoryBuilder};
use grit_lib::revwalk::rev_parse::resolve_revision;
use time::OffsetDateTime;

fn open_memory_ref_repo(git_dir: &Path) -> Repository {
    fs::create_dir_all(git_dir).expect("git dir");
    RepositoryBuilder::new(RepositoryOptions::empty())
        .odb_builder(
            OdbBuilder::files(git_dir.join("objects"))
                .primary(Arc::new(MemoryStore::new(HashAlgo::Sha1)))
                .alternates(false),
        )
        .ref_store(Arc::new(MemoryRefStore::new()))
        .initial_branch("main")
        .open(git_dir, None)
        .expect("open")
}

fn store_commit(repo: &Repository, message: &str) -> ObjectId {
    let blob = repo
        .odb()
        .write(ObjectKind::Blob, message.as_bytes())
        .expect("blob");
    let mut tree = b"100644 file\0".to_vec();
    tree.extend_from_slice(blob.as_bytes());
    let tree = repo.odb().write(ObjectKind::Tree, &tree).expect("tree");
    let body = format!(
        "tree {tree}\nauthor T <t@example.com> 1700000000 +0000\n\
         committer T <t@example.com> 1700000000 +0000\n\n{message}\n"
    );
    repo.odb()
        .write(ObjectKind::Commit, body.as_bytes())
        .expect("commit")
}

#[test]
fn repository_with_memory_ref_store() {
    let repo = Repository::discover(None, &RepositoryOptions::empty()).expect("discover");

    let oid: ObjectId = "0100000000000000000000000000000000000000"
        .parse()
        .expect("oid");
    let store = Arc::new(MemoryRefStore::new());
    store
        .prepare(
            RefTransaction::new()
                .update(RefUpdate {
                    name: "refs/heads/injected".to_owned(),
                    new_value: Some(RawRef::Direct(oid)),
                    expected: Expected::Missing,
                    reflog: Some(ReflogUpdate {
                        identity: "Test User <test@example.com>".to_owned(),
                        message: "create injected".to_owned(),
                        time: OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("time"),
                        old_oid: None,
                        new_oid: None,
                    }),
                    flags: grit_lib::refs::store::RefUpdateFlags::default(),
                })
                .expect("txn"),
        )
        .expect("prepare")
        .commit()
        .expect("commit");

    let repo = repo.with_ref_store(store);
    assert_eq!(
        repo.refs().format(),
        grit_lib::refs::store::RefStorageFormat::Memory
    );

    let listed = list_refs_for_repository(&repo, "refs/heads/").expect("list");
    assert!(
        listed
            .iter()
            .any(|(name, id)| name == "refs/heads/injected" && *id == oid),
        "list_refs_for_repository should list injected ref"
    );

    let resolved = resolve_revision(&repo, "refs/heads/injected").expect("rev-parse");
    assert_eq!(resolved, oid);

    let at_zero = resolve_revision(&repo, "refs/heads/injected@{0}").expect("reflog @{0}");
    assert_eq!(at_zero, oid);
}

#[test]
fn repository_injected_head_reflog_uses_ref_store() {
    let repo = Repository::discover(None, &RepositoryOptions::empty()).expect("discover");

    let oid: ObjectId = "0200000000000000000000000000000000000000"
        .parse()
        .expect("oid");
    let store = Arc::new(MemoryRefStore::new());
    store
        .prepare(
            RefTransaction::new()
                .update(RefUpdate {
                    name: "refs/heads/injected".to_owned(),
                    new_value: Some(RawRef::Direct(oid)),
                    expected: Expected::Missing,
                    reflog: Some(ReflogUpdate {
                        identity: "Test User <test@example.com>".to_owned(),
                        message: "branch create".to_owned(),
                        time: OffsetDateTime::from_unix_timestamp(1_700_000_001).expect("time"),
                        old_oid: None,
                        new_oid: None,
                    }),
                    flags: grit_lib::refs::store::RefUpdateFlags::default(),
                })
                .expect("txn"),
        )
        .expect("prepare")
        .commit()
        .expect("commit");
    store
        .prepare(
            RefTransaction::new()
                .update(RefUpdate {
                    name: "HEAD".to_owned(),
                    new_value: Some(RawRef::Symbolic("refs/heads/injected".to_owned())),
                    expected: Expected::Any,
                    reflog: None,
                    flags: grit_lib::refs::store::RefUpdateFlags::default(),
                })
                .expect("txn"),
        )
        .expect("prepare")
        .commit()
        .expect("commit");

    let repo = repo.with_ref_store(store);
    use grit_lib::refs::store::RefName;
    assert_eq!(
        repo.references()
            .resolve(&RefName::try_from("HEAD").expect("ref name"))
            .expect("HEAD"),
        oid
    );
    let head_at_zero = resolve_revision(&repo, "HEAD@{0}").expect("HEAD reflog");
    assert_eq!(head_at_zero, oid);
}

#[test]
fn injected_ref_updates_never_write_head_or_refs_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    let git_dir = dir.path().join("in-memory.git");
    let repo = open_memory_ref_repo(&git_dir);
    let tip = store_commit(&repo, "tip");
    let main = RefName::try_from("refs/heads/main").expect("ref name");
    repo.references().update(&main, &tip).expect("update main");

    assert!(
        !git_dir.join("HEAD").exists(),
        "injected ref update must not create HEAD on disk"
    );
    assert!(
        !git_dir.join("refs").exists(),
        "injected ref update must not create refs/ on disk"
    );
    assert_eq!(
        repo.refs()
            .read_raw("HEAD")
            .expect("HEAD")
            .and_then(|raw| match raw {
                RawRef::Symbolic(target) => Some(target),
                RawRef::Direct(_) => None,
            }),
        Some("refs/heads/main".to_owned())
    );
    assert_eq!(repo.references().resolve(&main).expect("main"), tip);
}

#[test]
fn resolve_merge_source_uses_injected_branch_refs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let git_dir = dir.path().join("merge-memory.git");
    let repo = open_memory_ref_repo(&git_dir);
    let tip = store_commit(&repo, "topic tip");
    let store = repo.refs();
    store
        .prepare(
            RefTransaction::new()
                .update(RefUpdate {
                    name: "refs/heads/topic".to_owned(),
                    new_value: Some(RawRef::Direct(tip)),
                    expected: Expected::Missing,
                    reflog: None,
                    flags: grit_lib::refs::store::RefUpdateFlags::default(),
                })
                .expect("txn"),
        )
        .expect("prepare")
        .commit()
        .expect("commit");

    assert!(!git_dir.join("refs").exists());

    let source = resolve_merge_source(&repo, "topic").expect("merge source");
    assert_eq!(source.oid, tip);
    assert_eq!(source.kind, MergeSourceKind::LocalBranch);
}
