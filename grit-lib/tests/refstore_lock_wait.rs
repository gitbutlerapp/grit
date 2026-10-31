//! Concurrent ref writers: memory/reftable single slot (issue #2181) and files per-ref locks (#2250).
//!
//! [`MemoryRefStore`] (like a database backend with a single write transaction) lets one
//! prepared transaction hold the whole store, so `prepare` for a disjoint ref fails fast with
//! [`RefStoreError::LockHeld`]. grit-lib's ref writes retry that for
//! `core.filesRefLockTimeout`, so concurrent updates to different refs all land, while a lock
//! that is never released still fails once the budget is spent.
//!
//! The default [`FilesRefStore`](grit_lib::refs::store::FilesRefStore) takes one lock file per
//! ref, so disjoint refs prepare and commit concurrently; only the same ref (or packed-refs)
//! contends.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Barrier};
use std::time::{Duration, Instant};

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::error::Error;
use grit_lib::ident::Signature;
use grit_lib::lockfile::LockWait;
use grit_lib::objects::odb::store::MemoryStore;
use grit_lib::objects::odb::OdbBuilder;
use grit_lib::objects::{HashAlgo, NewCommit, ObjectId, ObjectKind, TreeEntry};
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::branch::{create_branch, CreateBranchOptions, TrackMode};
use grit_lib::porcelain::commit::{commit_with_scope, CommitScope};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::refs::store::RefName;
use grit_lib::refs::store::{
    Expected, FilesRefStore, FilesRefStoreConfig, MemoryRefStore, RawRef, RefStore, RefStoreError,
    RefTransaction, RefUpdate, RefUpdateFlags,
};
use grit_lib::refs::{
    commit_ref_update_on_store, commit_ref_updates_on_store, prepare_with_lock_wait,
};
use grit_lib::repo::{InitOptions, Repository, RepositoryBuilder};
use grit_lib::worktree::{AddLinkedWorktreeOptions, WorktreeBranch};
use time::OffsetDateTime;

const IDENT_TS: &str = "T <t@e.x> 1700000000 +0000";

fn oid(n: u32) -> ObjectId {
    let mut bytes = [0u8; 20];
    bytes[16..].copy_from_slice(&n.to_be_bytes());
    bytes[0] = 1;
    ObjectId::from_bytes(&bytes).expect("oid")
}

fn set(name: &str, value: ObjectId, expected: Expected) -> RefUpdate {
    RefUpdate {
        name: name.to_owned(),
        new_value: Some(RawRef::Direct(value)),
        expected,
        reflog: None,
        flags: RefUpdateFlags::default(),
    }
}

fn txn(name: &str, value: ObjectId) -> RefTransaction {
    RefTransaction::new()
        .update(set(name, value, Expected::Any))
        .expect("txn")
}

fn is_lock_held(err: &Error, ref_name: &str) -> bool {
    matches!(err, Error::RefStore(RefStoreError::LockHeld { name }) if name == ref_name)
}

#[test]
fn concurrent_batches_on_disjoint_refs_all_commit() {
    const THREADS: u32 = 8;
    const UPDATES: u32 = 200;
    let store = MemoryRefStore::new();
    let start = Barrier::new(THREADS as usize);
    std::thread::scope(|s| {
        for t in 0..THREADS {
            let (store, start) = (&store, &start);
            s.spawn(move || {
                let name = format!("refs/heads/b{t}");
                start.wait();
                let mut previous = None;
                for k in 0..UPDATES {
                    let next = oid(t * UPDATES + k + 1);
                    let expected = previous.map_or(Expected::Missing, Expected::Oid);
                    commit_ref_updates_on_store(store, &[set(&name, next, expected)], &[])
                        .unwrap_or_else(|e| panic!("{name} update {k}: {e}"));
                    previous = Some(next);
                }
            });
        }
    });
    for t in 0..THREADS {
        assert_eq!(
            store.read_raw(&format!("refs/heads/b{t}")).expect("read"),
            Some(RawRef::Direct(oid(t * UPDATES + UPDATES)))
        );
    }
}

#[test]
fn concurrent_single_ref_updates_all_commit() {
    const THREADS: u32 = 8;
    const UPDATES: u32 = 200;
    let store = MemoryRefStore::new();
    std::thread::scope(|s| {
        for t in 0..THREADS {
            let store = &store;
            s.spawn(move || {
                let name = format!("refs/tags/t{t}");
                for k in 0..UPDATES {
                    commit_ref_update_on_store(store, set(&name, oid(k + 1), Expected::Any))
                        .unwrap_or_else(|e| panic!("{name} update {k}: {e}"));
                }
            });
        }
    });
    let mut count = 0;
    store
        .for_each_ref("refs/tags/", &mut |_| {
            count += 1;
            std::ops::ControlFlow::Continue(())
        })
        .expect("list");
    assert_eq!(count, THREADS);
}

#[test]
fn held_transaction_still_fails_after_the_budget() {
    let store = MemoryRefStore::new();
    let held = store
        .prepare(txn("refs/heads/holder", oid(1)))
        .expect("hold");

    let budget = Duration::from_millis(60);
    let started = Instant::now();
    let err = prepare_with_lock_wait(
        &store,
        txn("refs/heads/waiter", oid(2)),
        LockWait::Backoff { budget },
    )
    .map(|_| ())
    .expect_err("lock never released");
    let waited = started.elapsed();
    assert_eq!(
        err,
        RefStoreError::LockHeld {
            name: "refs/heads/waiter".into()
        }
    );
    assert!(waited >= budget, "gave up early after {waited:?}");
    assert!(waited < budget * 20, "wait not bounded: {waited:?}");

    let started = Instant::now();
    let err = prepare_with_lock_wait(
        &store,
        txn("refs/heads/waiter", oid(2)),
        LockWait::Immediately,
    )
    .map(|_| ())
    .expect_err("fail fast");
    assert!(matches!(err, RefStoreError::LockHeld { .. }));
    assert!(started.elapsed() < budget, "Immediately slept");

    // The default helpers wait core.filesRefLockTimeout's 100 ms default, then give up.
    let err = commit_ref_update_on_store(&store, set("refs/heads/waiter", oid(2), Expected::Any))
        .expect_err("still held");
    assert!(is_lock_held(&err, "refs/heads/waiter"), "{err:?}");

    held.commit().expect("commit holder");
    assert_eq!(store.read_raw("refs/heads/waiter").expect("read"), None);
}

#[test]
fn waiter_proceeds_once_the_holder_releases() {
    let store = MemoryRefStore::new();
    let (held_tx, held_rx) = mpsc::channel();
    std::thread::scope(|s| {
        let store = &store;
        s.spawn(move || {
            let held = store
                .prepare(txn("refs/heads/holder", oid(1)))
                .expect("hold");
            held_tx.send(()).expect("signal");
            std::thread::sleep(Duration::from_millis(30));
            held.commit().expect("commit holder");
        });
        held_rx.recv().expect("holder prepared");
        prepare_with_lock_wait(
            store,
            txn("refs/heads/waiter", oid(2)),
            LockWait::Backoff {
                budget: Duration::from_secs(10),
            },
        )
        .expect("prepare after release")
        .commit()
        .expect("commit waiter");
    });
    assert_eq!(
        store.read_raw("refs/heads/waiter").expect("read"),
        Some(RawRef::Direct(oid(2)))
    );
    assert_eq!(
        store.read_raw("refs/heads/holder").expect("read"),
        Some(RawRef::Direct(oid(1)))
    );
}

#[test]
fn other_errors_are_not_retried() {
    let store = MemoryRefStore::new();
    let started = Instant::now();
    let err = prepare_with_lock_wait(
        &store,
        RefTransaction::new()
            .update(set("refs/heads/x", oid(1), Expected::Oid(oid(2))))
            .expect("txn"),
        LockWait::Indefinitely,
    )
    .map(|_| ())
    .expect_err("mismatch");
    assert!(
        matches!(err, RefStoreError::ExpectedMismatch { .. }),
        "{err:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn lock_held_names_the_callers_ref() {
    let store = MemoryRefStore::new();
    let _held = store.prepare(txn("refs/heads/b1", oid(1))).expect("hold");
    let err = store
        .prepare(txn("refs/heads/b2", oid(2)))
        .map(|_| ())
        .expect_err("held");
    assert_eq!(
        err,
        RefStoreError::LockHeld {
            name: "refs/heads/b2".into()
        }
    );
    let err = store
        .set_ref("refs/heads/b3", RawRef::Direct(oid(3)))
        .expect_err("held");
    assert_eq!(
        err,
        RefStoreError::LockHeld {
            name: "refs/heads/b3".into()
        }
    );
}

fn options() -> RepositoryOptions {
    let mut env = Environment::empty();
    env.git_config_global = Some("/dev/null".into());
    env.git_config_system = Some("/dev/null".into());
    RepositoryOptions::with_environment(env)
}

fn stage_and_commit(
    repo: &Repository,
    work_tree: &Path,
    content: &str,
) -> grit_lib::error::Result<()> {
    fs::write(work_tree.join("f.txt"), content)?;
    stage(
        repo,
        &StageOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )?;
    let request = grit_test_support::commit_request(content, IDENT_TS);
    commit_with_scope(
        repo,
        &request,
        CommitScope::Staged,
        &mut NullProgress,
        &NeverCancel,
    )?;
    Ok(())
}

/// The issue's reproduction, scaled down: each thread commits to its own branch in its own
/// linked worktree, all over one shared `MemoryRefStore` and `MemoryStore`.
///
/// The generous `core.filesRefLockTimeout` keeps an overloaded test machine (a lock holder
/// descheduled for longer than the 100 ms default) from failing the run; without the retry
/// every overlap fails immediately whatever the budget.
#[test]
fn repository_injected_store_uses_core_files_ref_lock_timeout_from_git_dir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let git_dir = tmp.path().join("g");
    let work_tree = tmp.path().join("wt");
    fs::create_dir_all(&git_dir).expect("git dir");
    fs::create_dir_all(&work_tree).expect("work tree");
    fs::write(
        git_dir.join("config"),
        "[user]\n\tname = T\n\temail = t@e.x\n[core]\n\tfilesRefLockTimeout = 7500\n",
    )
    .expect("config");
    let repo = RepositoryBuilder::new(options())
        .odb_builder(
            OdbBuilder::files(git_dir.join("objects"))
                .primary(Arc::new(MemoryStore::new(HashAlgo::Sha1)))
                .alternates(false),
        )
        .ref_store(Arc::new(MemoryRefStore::new()))
        .initial_branch("main")
        .open(&git_dir, Some(&work_tree))
        .expect("open");
    match repo.refs().ref_lock_wait() {
        LockWait::Backoff { budget } => assert_eq!(budget, Duration::from_millis(7500)),
        other => panic!("expected Backoff from config, got {other:?}"),
    }
}

/// Dogfood repro (#2181): many threads each call [`create_branch`] on one shared injected store.
#[test]
fn concurrent_create_branch_on_injected_store_never_lock_held() {
    const THREADS: usize = 12;
    const CREATES: usize = 80;
    let tmp = tempfile::tempdir().expect("tempdir");
    let git_dir = tmp.path().join("g");
    let work_tree = tmp.path().join("wt");
    fs::create_dir_all(&git_dir).expect("git dir");
    fs::create_dir_all(&work_tree).expect("work tree");
    fs::write(
        git_dir.join("config"),
        "[user]\n\tname = T\n\temail = t@e.x\n[core]\n\tfilesRefLockTimeout = 30000\n",
    )
    .expect("config");
    let repo = Arc::new(
        RepositoryBuilder::new(options())
            .odb_builder(
                OdbBuilder::files(git_dir.join("objects"))
                    .primary(Arc::new(MemoryStore::new(HashAlgo::Sha1)))
                    .alternates(false),
            )
            .ref_store(Arc::new(MemoryRefStore::new()))
            .initial_branch("main")
            .open(&git_dir, Some(&work_tree))
            .expect("open"),
    );
    stage_and_commit(&repo, &work_tree, "base").expect("base commit");

    let start = Barrier::new(THREADS);
    let failures: Vec<String> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..THREADS)
            .map(|t| {
                let repo = Arc::clone(&repo);
                let start = &start;
                s.spawn(move || {
                    start.wait();
                    (0..CREATES)
                        .filter_map(|k| {
                            let name = format!("t{t}/b{k}");
                            let opts = CreateBranchOptions {
                                name,
                                start: None,
                                force: false,
                                track: TrackMode::Never,
                                dry_run: false,
                            };
                            create_branch(&repo, &opts)
                                .err()
                                .map(|e| format!("thread {t} create {k}: {e}"))
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().expect("worker"))
            .collect()
    });
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn concurrent_commits_in_linked_worktrees_over_shared_stores() {
    const THREADS: usize = 6;
    const COMMITS: usize = 25;
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let git_dir = root.join("g");
    let main_wt = root.join("wt");
    fs::create_dir_all(&git_dir).expect("git dir");
    fs::create_dir_all(&main_wt).expect("work tree");
    fs::write(
        git_dir.join("config"),
        "[user]\n\tname = T\n\temail = t@e.x\n[core]\n\tfilesRefLockTimeout = 30000\n",
    )
    .expect("config");
    let refs = Arc::new(MemoryRefStore::new());
    let main = RepositoryBuilder::new(options())
        .odb_builder(
            OdbBuilder::files(git_dir.join("objects"))
                .primary(Arc::new(MemoryStore::new(HashAlgo::Sha1)))
                .alternates(false),
        )
        .ref_store(refs.clone())
        .initial_branch("main")
        .open(&git_dir, Some(&main_wt))
        .expect("open over injected stores");
    stage_and_commit(&main, &main_wt, "base").expect("base commit");

    let handles: Vec<(Repository, PathBuf)> = (0..THREADS)
        .map(|i| {
            let path = root.join(format!("w{i}"));
            let entry = main
                .worktrees()
                .add(&AddLinkedWorktreeOptions {
                    path: path.clone(),
                    branch: Some(WorktreeBranch::Create(format!("b{i}"))),
                    start_point: None,
                    force: false,
                    lock: None,
                    no_checkout: false,
                    track: false,
                })
                .expect("add worktree");
            let repo = main
                .open_linked_worktree(&entry.admin_dir, &path)
                .expect("open linked worktree");
            (repo, path)
        })
        .collect();

    let start = Barrier::new(THREADS);
    let failures: Vec<String> = std::thread::scope(|s| {
        let workers: Vec<_> = handles
            .iter()
            .enumerate()
            .map(|(i, (repo, wt))| {
                let start = &start;
                s.spawn(move || {
                    start.wait();
                    (0..COMMITS)
                        .filter_map(|k| {
                            stage_and_commit(repo, wt, &format!("{i}-{k}"))
                                .err()
                                .map(|e| format!("b{i} commit {k}: {e}"))
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().expect("worker"))
            .collect()
    });
    assert!(failures.is_empty(), "{failures:#?}");

    for i in 0..THREADS {
        let mut depth = 0;
        let mut tip = Some(refs.resolve(&format!("refs/heads/b{i}")).expect("branch"));
        while let Some(commit) = tip {
            depth += 1;
            tip = main
                .odb()
                .read_commit(&commit)
                .expect("commit")
                .parents
                .first()
                .copied();
        }
        assert_eq!(depth, COMMITS + 1, "b{i} history");
    }
}

fn files_store(git_dir: &Path) -> FilesRefStore {
    let git_dir = fs::canonicalize(git_dir).unwrap_or_else(|_| git_dir.to_path_buf());
    FilesRefStore::open(FilesRefStoreConfig {
        git_dir: git_dir.clone(),
        common_dir: git_dir,
        namespace_prefix: None,
    })
}

#[test]
fn files_concurrent_disjoint_ref_updates_through_repository_never_lock_held() {
    const THREADS: usize = 8;
    const ITERS: usize = 500;
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = Arc::new(Repository::init(tmp.path(), &InitOptions::default()).expect("init"));
    let sig = Signature::new(
        "A",
        "a@example.com",
        OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("time"),
    )
    .expect("sig");
    let blob = repo
        .objects()
        .write(ObjectKind::Blob, b"x\n")
        .expect("blob");
    let tree = repo
        .objects()
        .write_tree([TreeEntry {
            mode: 0o100644,
            name: b"f".to_vec(),
            oid: blob,
        }])
        .expect("tree");
    let commit = repo
        .objects()
        .write_commit(&NewCommit::new(tree, sig.clone(), sig, "c"))
        .expect("commit");

    let failures: usize = std::thread::scope(|s| {
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let repo = Arc::clone(&repo);
                s.spawn(move || {
                    let name = RefName::try_from(format!("refs/heads/t{t}").as_str()).expect("ref");
                    let mut fails = 0usize;
                    for _ in 0..ITERS {
                        if repo.references().update(&name, &commit).is_err() {
                            fails += 1;
                        }
                    }
                    fails
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("join")).sum()
    });
    assert_eq!(
        failures, 0,
        "disjoint refs must not see LockHeld through References::update"
    );
    for t in 0..THREADS {
        let name = format!("refs/heads/t{t}");
        let resolved = repo
            .references()
            .resolve(&RefName::try_from(name.as_str()).expect("ref"))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(resolved, commit, "{name}");
    }
}

#[test]
fn files_prepare_disjoint_ref_while_another_transaction_is_open() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path();
    fs::create_dir_all(path.join("refs/heads")).expect("refs");
    fs::write(path.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");
    let store = files_store(path);

    let held = store
        .prepare(txn("refs/heads/a", oid(1)))
        .expect("prepare a");
    let waiter = store
        .prepare(txn("refs/heads/b", oid(2)))
        .expect("prepare b while a is open");
    waiter.commit().expect("commit b");
    held.commit().expect("commit a");
    assert_eq!(
        store.read_raw("refs/heads/a").expect("read"),
        Some(RawRef::Direct(oid(1)))
    );
    assert_eq!(
        store.read_raw("refs/heads/b").expect("read"),
        Some(RawRef::Direct(oid(2)))
    );
}

#[test]
fn files_create_over_stale_empty_directory_at_exact_ref_path() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path();
    fs::create_dir_all(path.join("refs/heads")).expect("refs");
    fs::write(path.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");
    let stale = path.join("refs/tags/empty");
    fs::create_dir_all(&stale).expect("stale empty directory at ref path");
    assert!(stale.is_dir());

    let store = files_store(path);
    let value = oid(42);
    commit_ref_update_on_store(&store, set("refs/tags/empty", value, Expected::Missing))
        .expect("empty directory at ref path must not block create");
    assert_eq!(
        store.read_raw("refs/tags/empty").expect("read"),
        Some(RawRef::Direct(value))
    );
    assert!(stale.is_file(), "ref file replaces the empty directory");
}

fn assert_a_vs_a_slash_b_create_race_once(path: &Path) {
    fs::create_dir_all(path.join("refs/heads")).expect("refs");
    fs::write(path.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");
    let store = Arc::new(files_store(path));
    let start = Arc::new(Barrier::new(2));
    let outcomes: Vec<grit_lib::error::Result<()>> = std::thread::scope(|s| {
        let a_store = Arc::clone(&store);
        let b_store = Arc::clone(&store);
        let a_start = Arc::clone(&start);
        let b_start = Arc::clone(&start);
        let a = s.spawn(move || {
            a_start.wait();
            commit_ref_update_on_store(
                a_store.as_ref(),
                set("refs/heads/a", oid(1), Expected::Missing),
            )
        });
        let b = s.spawn(move || {
            b_start.wait();
            commit_ref_update_on_store(
                b_store.as_ref(),
                set("refs/heads/a/b", oid(2), Expected::Missing),
            )
        });
        vec![a.join().expect("a"), b.join().expect("b")]
    });
    let successes = outcomes.iter().filter(|r| r.is_ok()).count();
    let name_unavailable = outcomes
        .iter()
        .filter(|r| {
            matches!(
                r,
                Err(Error::RefStore(RefStoreError::NameUnavailable { .. }))
            )
        })
        .count();
    assert_eq!(successes, 1, "{outcomes:?}");
    assert_eq!(name_unavailable, 1, "{outcomes:?}");
    for outcome in &outcomes {
        if let Err(err) = outcome {
            assert!(
                matches!(err, Error::RefStore(RefStoreError::NameUnavailable { .. })),
                "commit-time D/F races must return NameUnavailable, not {err:?}"
            );
        }
    }
    let a_exists = path.join("refs/heads/a").is_file();
    let b_exists = path.join("refs/heads/a/b").is_file();
    assert!(a_exists ^ b_exists, "exactly one ref file must exist");
    assert!(
        !path.join("refs/heads/a.lock").exists() && !path.join("refs/heads/a/b.lock").exists(),
        "locks released"
    );
}

#[test]
fn files_concurrent_create_race_a_vs_a_slash_b_leaves_one_ref() {
    let tmp = tempfile::tempdir().expect("tempdir");
    assert_a_vs_a_slash_b_create_race_once(tmp.path());
}

#[test]
fn files_concurrent_create_race_a_vs_a_slash_b_stress() {
    const ITERS: usize = 100;
    for i in 0..ITERS {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert_a_vs_a_slash_b_create_race_once(tmp.path());
        if i + 1 == ITERS {
            eprintln!("stress iterations={ITERS} ok");
        }
    }
}
