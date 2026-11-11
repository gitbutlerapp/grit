//! Locked reflog rewrite paths must not drop concurrent appends (issue #2286 follow-ups).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::thread;

use crate::support::{append_repo_config, files_repo, git};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::lockfile::LockWait;
use grit_lib::objects::ObjectId;
use grit_lib::refs::prepare_with_lock_wait;
use grit_lib::refs::store::{RawRef, RefStore, RefTransaction, RefUpdate, ReflogUpdate};
use grit_lib::repo::Repository;
use grit_lib::test_support::refs::expire_reflog;

fn append_on_store(
    store: &dyn RefStore,
    refname: &str,
    oid: ObjectId,
    ts: i64,
    lock_wait: LockWait,
) -> grit_lib::error::Result<()> {
    let txn = RefTransaction::new().update(
        RefUpdate::new(refname.to_owned(), Some(RawRef::Direct(oid))).with_reflog(Some(
            ReflogUpdate {
                identity: format!("T <t@example.com> {ts} +0000"),
                message: "x".into(),
                time: time::OffsetDateTime::from_unix_timestamp(ts).expect("ts"),
                old_oid: None,
                new_oid: Some(oid),
            },
        )),
    )?;
    prepare_with_lock_wait(store, txn, lock_wait)?.commit()?;
    Ok(())
}

#[test]
fn expire_reflog_keeps_concurrent_append_files() {
    let test = files_repo();
    append_repo_config(test.worktree(), "[core]\n\tfilesRefLockTimeout = 30000\n");
    git(
        test.worktree(),
        &["commit", "--allow-empty", "-q", "-m", "init"],
    );
    let repo = Repository::discover(Some(test.worktree()), &RepositoryOptions::from_process())
        .expect("open");
    let git_dir = repo.git_dir();
    let store = repo.ref_store();
    let tip = repo.head_commit().expect("head");
    let wait = LockWait::files_ref_lock(repo.config().snapshot().ok().as_deref());
    append_on_store(store, "refs/heads/race", tip, 1_500_000_000, wait).expect("old");
    let b_dir = git_dir.to_path_buf();
    let b = thread::spawn(move || {
        for _ in 0..50 {
            expire_reflog(&b_dir, "refs/heads/race", Some(1_600_000_000)).expect("expire");
        }
    });
    let mut fresh = 0usize;
    while !b.is_finished() {
        let ts = time::OffsetDateTime::now_utc().unix_timestamp();
        append_on_store(store, "refs/heads/race", tip, ts, wait).expect("append");
        fresh += 1;
    }
    b.join().expect("join");
    let kept = grit_test_support::refs::at(git_dir)
        .references()
        .reflog(grit_lib::refs::store::RefNameRef::new("refs/heads/race").expect("valid ref name"))
        .expect("read")
        .len();
    assert!(kept >= fresh, "expire_reflog lost concurrent appends");
}
