//! [`clone_into`](grit_lib::remote::clone::clone_into) clone reflogs with injected `RefStore` (#2393).
//!
//! Companion to `clone_into_custom_primary.rs`; runs in default CI (not `full-tests` only).

use std::fs;
use std::path::Path;
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::odb::store::MemoryStore;
use grit_lib::objects::HashAlgo;
use grit_lib::progress::CancelToken;
use grit_lib::refs::store::{MemoryRefStore, RawRef, RefNameRef, RefStore};
use grit_lib::remote::clone::{clone_into, CloneIntoOptions};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::repo::Repository;
use grit_test_support::{git, rev_parse};

fn git_upstream() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main", "."]);
    fs::write(dir.path().join("a.txt"), "one\n").expect("write");
    git(dir.path(), &["add", "a.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "c1"]);
    dir
}

fn empty_injected_repo(
    wt: &Path,
) -> (
    Repository,
    Arc<MemoryStore>,
    Arc<MemoryRefStore>,
    std::path::PathBuf,
) {
    let git_dir = wt.join(".git");
    fs::create_dir_all(&git_dir).expect("git dir");
    let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let ref_store = Arc::new(MemoryRefStore::new());
    let repo = Repository::open(
        &git_dir,
        &(RepositoryOptions::empty()
            .with_object_store(store.clone())
            .with_ref_store(ref_store.clone())
            .with_initial_branch("main")
            .with_work_tree(wt)),
    )
    .expect("open");
    (repo, store, ref_store, git_dir)
}

fn reflog_messages(store: &MemoryRefStore, refname: &str) -> Vec<String> {
    let mut msgs = Vec::new();
    store
        .for_each_reflog_entry(refname, false, &mut |e| {
            msgs.push(e.message.clone());
            std::ops::ControlFlow::Continue(())
        })
        .expect("reflog");
    msgs
}

#[test]
fn clone_into_injected_ref_store_keeps_clone_reflogs_in_store() {
    let upstream = git_upstream();
    let url = upstream.path().to_string_lossy().into_owned();
    let expected_msg = format!("clone: from {url}");
    let tip = rev_parse(upstream.path(), "main");

    let wt = tempfile::tempdir().expect("worktree");
    let (repo, store, ref_store, git_dir) = empty_injected_repo(wt.path());

    clone_into(
        repo,
        CloneIntoOptions::new(url),
        &mut NoProgress,
        &CancelToken::never(),
        None,
    )
    .expect("clone_into");

    for name in ["HEAD", "refs/heads/main", "refs/remotes/origin/HEAD"] {
        let msgs = reflog_messages(&ref_store, name);
        assert!(
            msgs.iter().any(|m| m == &expected_msg),
            "reflog for {name}: {msgs:?}"
        );
    }
    assert!(
        !git_dir.join("logs/HEAD").exists(),
        "HEAD reflog must not be written under git_dir/logs"
    );
    assert!(
        !git_dir.join("logs/refs/heads/main").exists(),
        "branch reflog must not be written under git_dir/logs"
    );
    assert_eq!(
        ref_store
            .read_raw(RefNameRef::from_trusted("HEAD"))
            .expect("HEAD"),
        Some(RawRef::Symbolic("refs/heads/main".to_owned()))
    );
    let repo = Repository::open(
        &git_dir,
        &(RepositoryOptions::empty()
            .with_object_store(store)
            .with_ref_store(ref_store.clone())
            .with_initial_branch("main")
            .with_work_tree(wt.path())),
    )
    .expect("reopen");
    assert_eq!(
        repo.revwalk()
            .resolve("HEAD@{0}", &grit_lib::revwalk::ResolveOptions::default())
            .expect("HEAD@{0}"),
        tip
    );
    assert_eq!(
        repo.revwalk()
            .resolve("main@{0}", &grit_lib::revwalk::ResolveOptions::default())
            .expect("main@{0}"),
        tip
    );
}
