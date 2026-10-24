//! A repository whose object store and ref store are both injected needs no git directory
//! layout on disk: no `HEAD`, `objects/` or `refs/`.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::odb::store::MemoryStore;
use grit_lib::objects::odb::OdbBuilder;
use grit_lib::objects::{HashAlgo, ObjectId, ObjectKind};
use grit_lib::refs::store::{
    Expected, MemoryRefStore, RawRef, RefStore, RefTransaction, RefUpdate, RefUpdateFlags,
};
use grit_lib::repo::{Repository, RepositoryBuilder};
use grit_lib::revwalk::rev_list::{rev_list, RevListOptions};
use grit_lib::revwalk::rev_parse::resolve_revision;

fn open_injected(git_dir: &Path) -> grit_lib::error::Result<(Repository, Arc<MemoryRefStore>)> {
    let refs = Arc::new(MemoryRefStore::new());
    let odb = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let repo = RepositoryBuilder::new(RepositoryOptions::empty())
        .odb_builder(
            OdbBuilder::files(git_dir.join("objects"))
                .primary(odb)
                .alternates(false),
        )
        .ref_store(refs.clone())
        .initial_branch("main")
        .open(git_dir, None)?;
    Ok((repo, refs))
}

fn commit(repo: &Repository, parent: Option<ObjectId>, message: &str) -> ObjectId {
    let blob = repo
        .odb()
        .write(ObjectKind::Blob, message.as_bytes())
        .expect("blob");
    let mut tree = b"100644 file\0".to_vec();
    tree.extend_from_slice(blob.as_bytes());
    let tree = repo.odb().write(ObjectKind::Tree, &tree).expect("tree");
    let parent = parent.map(|p| format!("parent {p}\n")).unwrap_or_default();
    let body = format!(
        "tree {tree}\n{parent}author A U Thor <a@example.com> 1700000000 +0000\n\
         committer A U Thor <a@example.com> 1700000000 +0000\n\n{message}\n"
    );
    repo.odb()
        .write(ObjectKind::Commit, body.as_bytes())
        .expect("commit")
}

fn advance_head(refs: &dyn RefStore, old: Option<ObjectId>, new: ObjectId) {
    let txn = RefTransaction::new()
        .update(RefUpdate {
            name: "HEAD".to_owned(),
            new_value: Some(RawRef::Direct(new)),
            expected: old.map_or(Expected::Any, Expected::Oid),
            reflog: None,
            flags: RefUpdateFlags::default(),
        })
        .expect("txn");
    refs.prepare(txn)
        .expect("prepare")
        .commit()
        .expect("commit");
}

fn exercise(repo: &Repository, refs: &MemoryRefStore) {
    assert_eq!(
        refs.read_raw("HEAD").expect("read HEAD"),
        Some(RawRef::Symbolic("refs/heads/main".to_owned())),
        "initial_branch seeds HEAD in the injected store"
    );
    let first = commit(repo, None, "first");
    advance_head(refs, None, first);
    let second = commit(repo, Some(first), "second");
    advance_head(refs, Some(first), second);

    assert_eq!(resolve_revision(repo, "HEAD").expect("HEAD"), second);
    assert_eq!(resolve_revision(repo, "main~1").expect("main~1"), first);
    let walked =
        rev_list(repo, &["HEAD".to_owned()], &[], &RevListOptions::default()).expect("rev-list");
    assert_eq!(walked.commits, vec![second, first]);
}

#[test]
fn opens_without_a_git_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    let git_dir = dir.path().join("nonexistent.git");
    let (repo, refs) = open_injected(&git_dir).expect("open with both backends injected");
    exercise(&repo, &refs);
    assert!(!git_dir.exists(), "nothing is written to disk");
}

#[test]
fn opens_an_empty_git_directory_and_writes_nothing_there() {
    let dir = tempfile::tempdir().expect("tempdir");
    let git_dir = dir.path().join("empty.git");
    fs::create_dir(&git_dir).expect("mkdir");
    let (repo, refs) = open_injected(&git_dir).expect("open empty dir");
    exercise(&repo, &refs);
    assert_eq!(fs::read_dir(&git_dir).expect("read_dir").count(), 0);
}

#[test]
fn reads_repository_config_when_present() {
    let dir = tempfile::tempdir().expect("tempdir");
    let git_dir = dir.path().join("config-only.git");
    fs::create_dir(&git_dir).expect("mkdir");
    fs::write(git_dir.join("config"), "[user]\n\tname = Injected\n").expect("config");
    let (repo, _refs) = open_injected(&git_dir).expect("open");
    let config = repo.config().expect("config");
    assert_eq!(config.get("user.name").as_deref(), Some("Injected"));
}

#[test]
fn initial_branch_keeps_an_existing_head() {
    let dir = tempfile::tempdir().expect("tempdir");
    let git_dir = dir.path().join("seeded.git");
    let refs = Arc::new(MemoryRefStore::new());
    refs.set_ref("HEAD", RawRef::Symbolic("refs/heads/trunk".to_owned()))
        .expect("seed");
    let repo = RepositoryBuilder::new(RepositoryOptions::empty())
        .odb_builder(
            OdbBuilder::files(git_dir.join("objects"))
                .primary(Arc::new(MemoryStore::new(HashAlgo::Sha1)))
                .alternates(false),
        )
        .ref_store(refs.clone())
        .initial_branch("main")
        .open(&git_dir, None)
        .expect("open");
    assert_eq!(
        repo.refs().read_raw("HEAD").expect("HEAD"),
        Some(RawRef::Symbolic("refs/heads/trunk".to_owned()))
    );
}

#[test]
fn initial_branch_rejects_invalid_names() {
    let dir = tempfile::tempdir().expect("tempdir");
    let git_dir = dir.path().join("bad.git");
    let result = RepositoryBuilder::new(RepositoryOptions::empty())
        .odb_builder(
            OdbBuilder::files(git_dir.join("objects"))
                .primary(Arc::new(MemoryStore::new(HashAlgo::Sha1)))
                .alternates(false),
        )
        .ref_store(Arc::new(MemoryRefStore::new()))
        .initial_branch("bad..name")
        .open(&git_dir, None);
    assert!(matches!(result, Err(Error::InvalidRef(_))), "{result:?}");
}

#[test]
fn a_files_object_store_still_needs_its_objects_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    let git_dir = dir.path().join("refs-only.git");
    fs::create_dir(&git_dir).expect("mkdir");
    let result = RepositoryBuilder::new(RepositoryOptions::empty())
        .odb_builder(OdbBuilder::files(git_dir.join("objects")))
        .ref_store(Arc::new(MemoryRefStore::new()))
        .open(&git_dir, None);
    assert!(
        matches!(result, Err(Error::NotARepository(_))),
        "{result:?}"
    );
}
