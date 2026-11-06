//! [`RepositoryReflogs`](grit_lib::repo_handles::RepositoryReflogs) reads and rewrites the
//! reflog through the repository's ref store (issue #2131): an injected store lists, deletes
//! and expires its own reflog.

use std::fs;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Arc;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::error::Error;
use grit_lib::objects::odb::{store::MemoryStore, OdbBuilder};
use grit_lib::objects::{HashAlgo, ObjectId};
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::commit::{commit_with_scope, CommitScope};
use grit_lib::progress::NullProgress;
use grit_lib::refs::store::{MemoryRefStore, RefName, RefStore};
use grit_lib::repo::{Repository, RepositoryBuilder};
use grit_lib::repo_handles::{ExpireOptions, ReflogQuery};

const IDENT_TS: &str = "T <t@e.x> 1700000000 +0000";

fn environment() -> Environment {
    let mut env = Environment::empty();
    env.git_config_global = Some("/dev/null".into());
    env.git_config_system = Some("/dev/null".into());
    env
}

fn open_injected(root: &Path) -> (Repository, Arc<MemoryRefStore>) {
    let (git_dir, work_tree) = (root.join("g.git"), root.join("wt"));
    fs::create_dir_all(&git_dir).expect("git dir");
    fs::create_dir_all(&work_tree).expect("work tree");
    fs::write(
        git_dir.join("config"),
        "[user]\n\tname = T\n\temail = t@e.x\n",
    )
    .expect("config");
    let store = Arc::new(MemoryRefStore::new());
    let repo = RepositoryBuilder::new(RepositoryOptions::with_environment(environment()))
        .odb_builder(
            OdbBuilder::files(git_dir.join("objects"))
                .primary(Arc::new(MemoryStore::new(HashAlgo::Sha1)))
                .alternates(false),
        )
        .ref_store(store.clone())
        .initial_branch("main")
        .open(&git_dir, Some(&work_tree))
        .expect("open with injected stores");
    (repo, store)
}

fn commit_n(repo: &Repository, work_tree: &Path, n: usize) -> Vec<ObjectId> {
    (0..n)
        .map(|i| {
            fs::write(work_tree.join("a"), format!("{i}\n")).expect("write");
            stage(
                repo,
                &StageOptions::default(),
                &mut NullProgress,
                &grit_lib::progress::NeverCancel,
            )
            .expect("stage");
            commit_with_scope(
                repo,
                &grit_test_support::commit_request(format!("c{i}"), IDENT_TS),
                CommitScope::Staged,
                &mut NullProgress,
                &grit_lib::progress::NeverCancel,
            )
            .expect("commit")
            .oid
        })
        .collect()
}

fn store_count(store: &dyn RefStore, name: &str) -> usize {
    let mut n = 0;
    store
        .for_each_reflog_entry(name, false, &mut |_| {
            n += 1;
            ControlFlow::Continue(())
        })
        .expect("read store reflog");
    n
}

fn head() -> RefName {
    RefName::try_from("HEAD").expect("HEAD")
}

fn expire_now(all: bool, dry_run: bool) -> ExpireOptions {
    ExpireOptions {
        now: 1_800_000_000,
        expire: Some("now".into()),
        expire_unreachable: Some("now".into()),
        all,
        dry_run,
    }
}

#[test]
fn injected_store_reflog_is_listed_and_read() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (repo, store) = open_injected(tmp.path());
    let tips = commit_n(&repo, &tmp.path().join("wt"), 3);
    assert_eq!(store_count(store.as_ref(), "HEAD"), 3);

    let names: Vec<String> = repo
        .reflogs()
        .refs()
        .expect("refs")
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(names, ["HEAD", "refs/heads/main"]);

    let entries = repo.reflogs().entries(&head()).expect("entries");
    let newest_first: Vec<ObjectId> = entries.iter().map(|e| e.entry.new_oid).collect();
    assert_eq!(newest_first, tips.iter().rev().copied().collect::<Vec<_>>());
    assert_eq!(
        entries.iter().map(|e| e.index).collect::<Vec<_>>(),
        [0, 1, 2]
    );

    // A partial name resolves to the branch log through the store.
    let main = RefName::try_from("heads/main").expect("heads/main");
    assert_eq!(repo.reflogs().entries(&main).expect("main").len(), 3);
}

#[test]
fn injected_store_query_limit_reports_truncation() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (repo, _store) = open_injected(tmp.path());
    commit_n(&repo, &tmp.path().join("wt"), 3);
    let page = |limit| {
        repo.reflogs()
            .query(
                &head(),
                &ReflogQuery {
                    limit: Some(limit),
                    ..ReflogQuery::default()
                },
            )
            .expect("query")
    };
    let two = page(2);
    assert_eq!(two.entries.len(), 2);
    assert!(two.truncated);
    let three = page(3);
    assert_eq!(three.entries.len(), 3);
    assert!(
        !three.truncated,
        "nothing older remains after the last entry"
    );
}

#[test]
fn injected_store_unknown_ref_is_a_typed_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (repo, _store) = open_injected(tmp.path());
    commit_n(&repo, &tmp.path().join("wt"), 1);
    let missing = RefName::try_from("refs/heads/missing").expect("name");
    match repo.reflogs().entries(&missing) {
        Err(Error::ReflogUnknownRef { refname }) => assert_eq!(refname, "refs/heads/missing"),
        other => panic!("expected ReflogUnknownRef, got {other:?}"),
    }
}

#[test]
fn injected_store_delete_rewrites_the_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (repo, store) = open_injected(tmp.path());
    let tips = commit_n(&repo, &tmp.path().join("wt"), 3);

    repo.reflogs().delete(&head(), &[1], false).expect("delete");
    let left = repo.reflogs().entries(&head()).expect("entries");
    assert_eq!(
        left.iter().map(|e| e.entry.new_oid).collect::<Vec<_>>(),
        [tips[2], tips[0]]
    );
    assert_eq!(left[0].entry.old_oid, tips[1], "no rechain without rewrite");
    assert_eq!(store_count(store.as_ref(), "HEAD"), 2);

    repo.reflogs()
        .delete(&head(), &[1], true)
        .expect("delete rewrite");
    let left = repo.reflogs().entries(&head()).expect("entries");
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].entry.new_oid, tips[2]);
    assert!(
        left[0].entry.old_oid.is_zero(),
        "oldest survivor is rechained"
    );

    match repo.reflogs().delete(&head(), &[5], false) {
        Err(Error::ReflogIndexOutOfRange { refname, index }) => {
            assert_eq!((refname.as_str(), index), ("HEAD", 5));
        }
        other => panic!("expected ReflogIndexOutOfRange, got {other:?}"),
    }
}

#[test]
fn injected_store_expire_prunes_through_the_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (repo, store) = open_injected(tmp.path());
    commit_n(&repo, &tmp.path().join("wt"), 3);

    let dry = repo
        .reflogs()
        .expire(&expire_now(false, true))
        .expect("dry run");
    assert_eq!(dry.entries_pruned, 6);
    assert_eq!(
        store_count(store.as_ref(), "HEAD"),
        3,
        "dry run writes nothing"
    );

    let out = repo
        .reflogs()
        .expire(&expire_now(false, false))
        .expect("expire");
    assert_eq!(out.entries_pruned, 6);
    let rows: Vec<(&str, usize)> = out
        .refs
        .iter()
        .map(|r| (r.ref_name.as_str(), r.pruned))
        .collect();
    assert_eq!(rows, [("HEAD", 3), ("refs/heads/main", 3)]);
    assert_eq!(store_count(store.as_ref(), "HEAD"), 0);
    assert_eq!(store_count(store.as_ref(), "refs/heads/main"), 0);
    assert!(repo.reflogs().entries(&head()).expect("empty").is_empty());
}

#[test]
fn injected_store_expire_all_lists_refs_from_the_store() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (repo, store) = open_injected(tmp.path());
    commit_n(&repo, &tmp.path().join("wt"), 2);
    let out = repo
        .reflogs()
        .expire(&expire_now(true, false))
        .expect("expire --all");
    let names: Vec<&str> = out.refs.iter().map(|r| r.ref_name.as_str()).collect();
    assert_eq!(names, ["HEAD", "refs/heads/main"]);
    assert_eq!(out.entries_pruned, 4);
    assert_eq!(store_count(store.as_ref(), "HEAD"), 0);
}

#[test]
fn injected_store_unreachable_expiry_keeps_reachable_entries() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (repo, store) = open_injected(tmp.path());
    commit_n(&repo, &tmp.path().join("wt"), 3);
    // Every entry is reachable from the tips in the store, so only unreachable expiry
    // (which needs those tips) applies and nothing is pruned.
    let out = repo
        .reflogs()
        .expire(&ExpireOptions {
            now: 1_800_000_000,
            expire: Some("never".into()),
            expire_unreachable: Some("now".into()),
            all: false,
            dry_run: false,
        })
        .expect("expire unreachable");
    assert_eq!(out.entries_pruned, 0);
    assert_eq!(store_count(store.as_ref(), "HEAD"), 3);
}
