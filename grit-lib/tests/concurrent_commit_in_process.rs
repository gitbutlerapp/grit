//! In-process concurrent `create_commit` must not surface spurious permission errors.

use std::sync::Arc;
use std::thread;

use grit_lib::error::Error;
use grit_lib::ident::Signature;
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::refs::lock_path_for_ref;
use grit_lib::repo::{InitOptions, Repository};

struct Harness {
    _dir: tempfile::TempDir,
    repo: Arc<Repository>,
}

fn init_repo() -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Arc::new(Repository::init(dir.path(), &InitOptions::default()).expect("init"));
    let ident: Signature = "Test User <test@example.com> 0 +0000"
        .parse()
        .expect("ident");
    let mut root =
        grit_test_support::empty_commit_request("root", "Test User <test@example.com> 0 +0000");
    root.author = Some(ident.clone());
    root.committer = Some(ident);
    create_commit(&repo, &root, &mut NullProgress, &NeverCancel).expect("root commit");
    Harness { _dir: dir, repo }
}

#[test]
fn concurrent_allow_empty_commits_avoid_permission_errors() {
    let harness = init_repo();
    let repo = Arc::clone(&harness.repo);
    let ident: Signature = "Test User <test@example.com> 0 +0000"
        .parse()
        .expect("ident");
    let handles: Vec<_> = (0..4)
        .map(|t| {
            let repo = Arc::clone(&repo);
            let ident = ident.clone();
            thread::spawn(move || {
                let mut errors = Vec::new();
                for i in 0..30 {
                    let mut req = grit_test_support::empty_commit_request(
                        format!("t{t} i{i}"),
                        "Test User <test@example.com> 0 +0000",
                    );
                    req.author = Some(ident.clone());
                    req.committer = Some(ident.clone());
                    if let Err(e) = create_commit(&repo, &req, &mut NullProgress, &NeverCancel) {
                        errors.push(e);
                    }
                }
                errors
            })
        })
        .collect();

    let mut all_errors = Vec::new();
    for handle in handles {
        all_errors.extend(handle.join().expect("thread"));
    }
    drop(harness);

    for err in &all_errors {
        let text = err.to_string();
        assert!(
            !text.contains("insufficient permission"),
            "unexpected permission error: {text}"
        );
        match err {
            Error::RefStore(store) => {
                assert!(
                    matches!(
                        store,
                        grit_lib::refs::store::RefStoreError::ExpectedMismatch { .. }
                            | grit_lib::refs::store::RefStoreError::LockHeld { .. }
                    ),
                    "unexpected ref store error: {store:?}"
                );
            }
            Error::LockHeld { .. } => {}
            other => panic!("unexpected concurrent commit error: {other:?}"),
        }
    }
}

#[test]
fn lock_path_for_ref_uses_platform_parent() {
    let git_dir = tempfile::tempdir().expect("tempdir");
    let ref_path = git_dir.path().join("refs").join("heads").join("main");
    std::fs::create_dir_all(ref_path.parent().expect("parent")).expect("mkdir");
    let lock = lock_path_for_ref(&ref_path);
    assert_eq!(lock, ref_path.with_file_name("main.lock"));
    #[cfg(windows)]
    assert!(
        !lock.to_string_lossy().contains('/'),
        "lock path should not embed forward slashes: {}",
        lock.display()
    );
}

#[cfg(windows)]
#[test]
fn concurrent_commit_while_index_and_ref_handles_open() {
    use std::fs::File;

    let harness = init_repo();
    let repo = Arc::clone(&harness.repo);
    let git_dir = repo.git_dir().to_path_buf();
    let index_path = git_dir.join("index");
    let ref_path = git_dir.join("refs/heads/main");
    let index_hold = File::open(&index_path).expect("open index");
    let ref_hold = File::open(&ref_path).expect("open main ref");
    let ident: Signature = "Test User <test@example.com> 0 +0000"
        .parse()
        .expect("ident");

    let handles: Vec<_> = (0..4)
        .map(|t| {
            let repo = Arc::clone(&repo);
            let ident = ident.clone();
            thread::spawn(move || {
                let mut errors = Vec::new();
                for i in 0..30 {
                    let mut req = grit_test_support::empty_commit_request(
                        format!("held t{t} i{i}"),
                        "Test User <test@example.com> 0 +0000",
                    );
                    req.author = Some(ident.clone());
                    req.committer = Some(ident.clone());
                    if let Err(e) = create_commit(&repo, &req, &mut NullProgress, &NeverCancel) {
                        errors.push(e);
                    }
                }
                errors
            })
        })
        .collect();

    let mut all_errors = Vec::new();
    for handle in handles {
        all_errors.extend(handle.join().expect("thread"));
    }
    drop(index_hold);
    drop(ref_hold);
    drop(harness);

    for err in &all_errors {
        let text = err.to_string();
        assert!(
            !text.contains("insufficient permission"),
            "unexpected permission error: {text}"
        );
        match err {
            Error::RefStore(store) => assert!(
                matches!(
                    store,
                    grit_lib::refs::store::RefStoreError::ExpectedMismatch { .. }
                        | grit_lib::refs::store::RefStoreError::LockHeld { .. }
                ),
                "unexpected ref store error: {store:?}"
            ),
            Error::LockHeld { .. } | Error::GitDirPathIo { .. } if err.is_git_dir_file_in_use() => {
            }
            Error::GitDirPathIo { .. } => {
                panic!("unexpected git-dir I/O (not in-use): {err:?}")
            }
            other => panic!("unexpected concurrent commit error: {other:?}"),
        }
    }
}
