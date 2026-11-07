//! `add_linked_worktree_with_cancel` rolls back registration and branch creation (issue #2489).

use std::fs;
use std::thread;
use std::time::Duration;

use grit_lib::porcelain::commit::{commit_with_scope, CommitScope};
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::worktree::{
    add_linked_worktree_with_cancel, list_worktrees, AddLinkedWorktreeOptions, WorktreeBranch,
};

const IDENT: &str = "T <t@t> 0 +0000";

fn init_wide_repo() -> (tempfile::TempDir, Repository) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (repo, _) = Repository::init(
        tmp.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    for i in 0..800 {
        std::fs::write(tmp.path().join(format!("f{i}.txt")), format!("{i}\n")).expect("write");
    }
    commit_with_scope(
        &repo,
        &grit_test_support::commit_request("wide", IDENT),
        CommitScope::AutoStage,
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("commit");
    (tmp, repo)
}

#[test]
fn cancel_during_worktree_add_leaves_no_registration_or_branch() {
    let (tmp, repo) = init_wide_repo();
    let wt_path = tmp.path().join("wt");
    let cancel = CancelToken::new();
    let flag = cancel.clone();
    let common = grit_lib::worktree::common_git_dir(repo.git_dir());
    let handle = thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            if common.join("worktrees").is_dir() {
                flag.cancel();
                return;
            }
            thread::sleep(Duration::from_micros(200));
        }
    });
    let err = add_linked_worktree_with_cancel(
        &repo,
        &AddLinkedWorktreeOptions {
            path: wt_path.clone(),
            branch: Some(WorktreeBranch::Create("wtb".into())),
            start_point: Some("HEAD".into()),
            force: false,
            lock: None,
            no_checkout: false,
            track: false,
        },
        &cancel,
    )
    .expect_err("cancelled add");
    handle.join().expect("join");
    assert!(err.is_cancelled(), "{err:?}");
    assert_eq!(list_worktrees(&repo).expect("list").len(), 1);
    assert!(
        repo.references().resolve("refs/heads/wtb").is_err(),
        "branch should be rolled back"
    );
    let common = grit_lib::worktree::common_git_dir(repo.git_dir());
    let linked = common.join("worktrees");
    let linked_count = linked
        .is_dir()
        .then(|| fs::read_dir(&linked).map(|d| d.count()).unwrap_or(0))
        .unwrap_or(0);
    assert_eq!(linked_count, 0, "linked admin dirs should be removed");
    assert!(!wt_path.join(".git").is_file());
}
