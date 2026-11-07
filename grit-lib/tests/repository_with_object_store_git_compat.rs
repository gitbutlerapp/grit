//! Round-trips between a custom primary store ([`RepositoryOptions::with_object_store`]) and
//! the system `git`: history grit commits into the store passes `git fsck` once exported, and
//! a git-written history layered under a store through [`OdbBuilder`] reads back intact.

use std::fs;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::odb::store::{MemoryStore, ObjectStore};
use grit_lib::objects::odb::{Odb, OdbBuilder};
use grit_lib::objects::HashAlgo;
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::commit::{commit_with_scope, CommitRequest, CommitScope};
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::refs::store::MemoryRefStore;
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_list::{rev_list, RevListOptions};
use grit_test_support::{git, git_packed_history, rev_parse};

const IDENT: &str = "A U Thor <author@example.com> 1700000000 +0000";

fn commit_all(repo: &Repository, message: &str) -> grit_lib::objects::ObjectId {
    stage(
        repo,
        &StageOptions::default(),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("stage");
    let mut req = CommitRequest::new(message.to_owned());
    req.author = Some(IDENT.parse().expect("signature"));
    req.committer = Some(IDENT.parse().expect("signature"));
    commit_with_scope(
        repo,
        &req,
        CommitScope::Staged,
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("commit")
    .oid
}

fn export(store: &MemoryStore, objects_dir: &Path) {
    let disk = Odb::new(objects_dir);
    let mut oids = Vec::new();
    store
        .for_each_object(&mut |oid| {
            oids.push(*oid);
            ControlFlow::Continue(())
        })
        .expect("enumerate");
    for oid in oids {
        let obj = store.read(&oid).expect("read").expect("listed");
        assert_eq!(disk.write(obj.kind, &obj.data).expect("export"), oid);
    }
}

/// Grit commits into a memory store with a git-initialized repository's refs and index;
/// the exported objects make a history `git fsck --strict` accepts and `git log` walks.
#[test]
fn history_committed_into_a_custom_store_passes_git_fsck() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonical");
    git(&root, &["init", "-q", "-b", "main"]);
    let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty()
            .with_object_store(store.clone())
            .with_work_tree(&root),
    )
    .expect("open");

    fs::create_dir_all(root.join("src")).expect("mkdir");
    fs::write(root.join("README"), "hello\n").expect("write");
    fs::write(root.join("src/lib.rs"), "fn f() {}\n").expect("write");
    commit_all(&repo, "first");
    fs::write(root.join("README"), "hello again\n").expect("write");
    let tip = commit_all(&repo, "second");

    assert_eq!(
        files_under(&root.join(".git/objects")),
        0,
        "nothing written to objects/"
    );
    export(&store, &root.join(".git/objects"));
    git(&root, &["fsck", "--strict", "--full", "--no-dangling"]);
    assert_eq!(rev_parse(&root, "HEAD"), tip);
    assert_eq!(git(&root, &["rev-list", "--count", "HEAD"]).trim(), "2");
    assert_eq!(git(&root, &["show", "HEAD:README"]), "hello again\n");
    assert_eq!(git(&root, &["status", "--porcelain"]), "");
}

/// A git-written packed history read through `OdbBuilder::push_read_source` under an empty
/// memory primary walks exactly like git's own `rev-list`.
#[test]
fn git_packed_history_reads_through_a_read_source() {
    let dir = tempfile::tempdir().expect("tempdir");
    let history = git_packed_history(dir.path(), 5);
    let upstream =
        Repository::open(&history, &RepositoryOptions::empty()).expect("open git history");
    let tip = rev_parse(&history, "main");

    let layered = Repository::open(
        &dir.path().join("layered.git"),
        &RepositoryOptions::empty()
            .with_object_store(Arc::new(MemoryStore::new(HashAlgo::Sha1)))
            .with_odb_builder(OdbBuilder::new().push_read_source(upstream.odb().read_store()))
            .with_ref_store(Arc::new(MemoryRefStore::new())),
    )
    .expect("open layered");
    let walked =
        rev_list(&layered, &[tip.to_hex()], &[], &RevListOptions::default()).expect("rev-list");
    let expected: Vec<String> = git(&history, &["rev-list", "main"])
        .lines()
        .map(str::to_owned)
        .collect();
    let walked: Vec<String> = walked.commits.iter().map(|oid| oid.to_hex()).collect();
    assert_eq!(walked, expected);
}

fn files_under(dir: &Path) -> usize {
    fs::read_dir(dir).map_or(0, |entries| {
        entries
            .flatten()
            .map(|entry| {
                let path = entry.path();
                if path.is_dir() {
                    files_under(&path)
                } else {
                    1
                }
            })
            .sum()
    })
}
