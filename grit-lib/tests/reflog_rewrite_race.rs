//! Reflog expire vs concurrent ref updates must not drop entries (issue #2286).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::thread;

use crate::support::{
    append_repo_config, files_repo, git, git_init_reftable_repo, require_reftable_git,
};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::lockfile::LockWait;
use grit_lib::objects::ObjectId;
use grit_lib::refs::prepare_with_lock_wait;
use grit_lib::refs::store::RefStorageFormat;
use grit_lib::refs::store::{
    Expected, RawRef, RefStore, RefTransaction, RefUpdate, RefUpdateFlags, ReflogUpdate,
};
use grit_lib::repo::Repository;
use grit_lib::repo_handles::ExpireOptions;

fn long_ref_lock_wait(repo: &Repository) -> LockWait {
    LockWait::files_ref_lock(repo.config().ok().as_deref())
}

fn append(
    store: &dyn RefStore,
    oid: ObjectId,
    ts: i64,
    lock_wait: LockWait,
) -> grit_lib::error::Result<()> {
    let txn = RefTransaction::new().update(RefUpdate {
        name: "refs/heads/race".into(),
        new_value: Some(RawRef::Direct(oid)),
        expected: Expected::Any,
        reflog: Some(ReflogUpdate {
            identity: "T <t@example.com>".into(),
            message: "x".into(),
            time: time::OffsetDateTime::from_unix_timestamp(ts).expect("ts"),
            old_oid: None,
            new_oid: Some(oid),
        }),
        flags: RefUpdateFlags::default(),
    })?;
    prepare_with_lock_wait(store, txn, lock_wait)?.commit()?;
    Ok(())
}

fn expire(repo: &Repository) -> grit_lib::error::Result<usize> {
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    Ok(repo
        .reflogs()
        .expire(&ExpireOptions {
            now,
            expire: Some("1.year.ago".into()),
            expire_unreachable: Some("1.year.ago".into()),
            all: false,
            dry_run: false,
        })?
        .entries_pruned)
}

fn race_on_repo(repo: Arc<Repository>) {
    let lock_wait = long_ref_lock_wait(&repo);
    let tip = repo.head_commit().expect("head");
    let now = || time::OffsetDateTime::now_utc().unix_timestamp();
    append(repo.refs(), tip, now(), lock_wait).expect("seed");

    let b_repo = repo.clone();
    let b = thread::spawn(move || -> grit_lib::error::Result<()> {
        for _ in 0..100 {
            append(b_repo.refs(), tip, 1_500_000_000, lock_wait)?;
            expire(&b_repo)?;
        }
        Ok(())
    });

    let mut fresh = 1usize;
    while !b.is_finished() {
        append(repo.refs(), tip, now(), lock_wait).expect("append fresh");
        fresh += 1;
    }
    b.join().expect("join").expect("background");
    expire(&repo).expect("final expire");

    let mut kept = 0usize;
    repo.refs()
        .for_each_reflog_entry("refs/heads/race", false, &mut |_| {
            kept += 1;
            std::ops::ControlFlow::Continue(())
        })
        .expect("read log");
    assert_eq!(
        kept,
        fresh,
        "lost {} fresh reflog entries under concurrent expire",
        fresh.saturating_sub(kept)
    );
}

fn open_with_long_lock(worktree: &std::path::Path) -> Arc<Repository> {
    append_repo_config(worktree, "[core]\n\tfilesRefLockTimeout = 30000\n");
    Arc::new(
        Repository::discover(Some(worktree), &RepositoryOptions::from_process()).expect("open"),
    )
}

#[test]
fn reflog_expire_races_ref_updates_files_backend() {
    let test = files_repo();
    git(
        test.worktree(),
        &["commit", "--allow-empty", "-q", "-m", "init"],
    );
    let repo = open_with_long_lock(test.worktree());
    assert_eq!(repo.refs().format(), RefStorageFormat::Files);
    race_on_repo(repo);
}

#[test]
fn reflog_expire_races_ref_updates_reftable_backend() {
    if !require_reftable_git() {
        return;
    }
    let root = git_init_reftable_repo("main");
    git(
        root.path(),
        &["commit", "--allow-empty", "-q", "-m", "init"],
    );
    let repo = open_with_long_lock(root.path());
    assert_eq!(repo.refs().format(), RefStorageFormat::Reftable);
    race_on_repo(repo);
}
