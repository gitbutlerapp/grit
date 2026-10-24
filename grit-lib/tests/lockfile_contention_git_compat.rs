//! Ref lock backoff when another process briefly holds `refs/heads/*.lock`.

mod support;

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use grit_lib::error::Error;
use grit_lib::refs::store::RefStoreError;
use grit_lib::refs::{lock_path_for_ref, write_ref};

use support::{
    assert_fsck_when_git_interop, files_repo, git, git_empty_commit_oid, git_interop_available,
    Backend,
};

#[test]
fn lockfile_contention_git_compat_succeeds_within_default_budget() {
    if !git_interop_available(Backend::Files) {
        return;
    }
    let repo = files_repo();
    let git_dir = repo.git_dir();
    let base = git_empty_commit_oid(repo.worktree());
    write_ref(&git_dir, "refs/heads/main", &base).expect("seed main");

    let ref_path = git_dir.join("refs/heads/contended");
    let lock_path = lock_path_for_ref(&ref_path);
    if let Some(parent) = ref_path.parent() {
        std::fs::create_dir_all(parent).expect("ref parent");
    }

    let start = Barrier::new(2);
    let lock_path_thread = lock_path.clone();
    let starter = Arc::new(start);
    let starter_child = Arc::clone(&starter);
    let holder = thread::spawn(move || {
        std::fs::write(&lock_path_thread, b"").expect("foreign lock");
        starter_child.wait();
        thread::sleep(Duration::from_millis(50));
        std::fs::remove_file(&lock_path_thread).expect("release foreign lock");
    });

    starter.wait();
    let began = Instant::now();
    write_ref(&git_dir, "refs/heads/contended", &base).expect("write after brief contention");
    assert!(
        began.elapsed() < Duration::from_millis(500),
        "ref update should not block far beyond the default lock budget"
    );
    holder.join().expect("holder thread");

    assert!(ref_path.is_file());
    assert_fsck_when_git_interop(&repo);
}

#[test]
fn lockfile_contention_held_forever_preserves_foreign_lock() {
    if !git_interop_available(Backend::Files) {
        return;
    }
    let repo = files_repo();
    let git_dir = repo.git_dir();
    let base = git_empty_commit_oid(repo.worktree());
    write_ref(&git_dir, "refs/heads/main", &base).expect("seed main");

    let ref_path = git_dir.join("refs/heads/blocked");
    let lock_path = lock_path_for_ref(&ref_path);
    if let Some(parent) = ref_path.parent() {
        std::fs::create_dir_all(parent).expect("ref parent");
    }
    std::fs::write(&lock_path, b"foreign\n").expect("foreign lock");

    let err = write_ref(&git_dir, "refs/heads/blocked", &base).expect_err("budget exhausted");
    assert!(
        matches!(&err, Error::LockHeld { path, holder: None } if *path == lock_path)
            || matches!(&err, Error::RefStore(RefStoreError::LockHeld { name }) if name == "refs/heads/blocked"),
        "unexpected error: {err:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&lock_path).expect("foreign lock still present"),
        "foreign\n"
    );

    std::fs::remove_file(&lock_path).expect("clear foreign lock for git");
    git(
        repo.worktree(),
        &["update-ref", "refs/heads/blocked", &base.to_hex()],
    );
    assert_fsck_when_git_interop(&repo);
}
