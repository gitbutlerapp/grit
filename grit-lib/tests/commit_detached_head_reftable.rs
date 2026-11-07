//! Detached HEAD commits on grit reftable repositories (issue #1513 review).

use crate::support;

use grit_lib::porcelain::commit::{commit_worktree, CommitRequest, HookPolicy};
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::state::HeadState;
use support::{git, git_init_reftable_repo, require_reftable_git};

fn ident() -> String {
    "Reftable Detached <reftable-detached@example.com> 1700000000 +0000".to_owned()
}

fn commit_req(message: &str) -> CommitRequest {
    let id = ident();
    {
        let mut req = CommitRequest::new(message.to_owned());
        req.author = Some(id.parse().expect("signature"));
        req.committer = Some(id.parse().expect("signature"));
        req.allow_empty = false;
        req.sign_override = None;
        req.amend = false;
        req.assume_worktree_synced = false;
        req.hooks = HookPolicy::SkipVerify;
        req
    }
}

#[test]
fn grit_commit_on_detached_reftable_head_advances_head() {
    if !require_reftable_git() {
        return;
    }
    let root = git_init_reftable_repo("main");
    let wt = root.path();
    std::fs::write(wt.join("a.txt"), b"a\n").unwrap();
    let repo = Repository::open(
        &wt.join(".git"),
        &grit_lib::config::environment::RepositoryOptions::empty().with_work_tree(wt),
    )
    .expect("open");
    commit_worktree(
        &repo,
        &commit_req("one"),
        &mut NullProgress,
        &grit_lib::progress::CancelToken::never(),
    )
    .expect("first commit");
    let main_tip = repo.references().resolve("refs/heads/main").unwrap();

    let tip = match repo.references().head().expect("head before detach") {
        HeadState::Branch { oid: Some(oid), .. } => oid,
        other => panic!("expected branch tip, got {other:?}"),
    };
    switch_repository(
        &repo,
        &SwitchOptions {
            mode: SwitchMode::Detached {
                commit: tip,
                reflog_to_label: "HEAD".to_owned(),
            },
            dry_run: false,
            hooks: grit_lib::command_runner::hooks::PostOperationHooks::Run,
        },
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::CancelToken::never(),
    )
    .expect("detach");

    std::fs::write(wt.join("b.txt"), b"b\n").unwrap();
    let outcome = commit_worktree(
        &repo,
        &commit_req("detached"),
        &mut NullProgress,
        &grit_lib::progress::CancelToken::never(),
    )
    .expect("detached commit");
    assert_eq!(outcome.branch, None);
    assert_ne!(outcome.oid, main_tip);

    match repo.references().head().expect("head") {
        HeadState::Detached { oid } => assert_eq!(oid, outcome.oid),
        other => panic!("expected detached HEAD, got {other:?}"),
    }
    assert_eq!(
        repo.references().resolve("refs/heads/main").unwrap(),
        main_tip
    );

    git(wt, &["fsck", "--strict"]);
}

#[test]
fn grit_amend_on_detached_reftable_head() {
    if !require_reftable_git() {
        return;
    }
    let root = git_init_reftable_repo("main");
    let wt = root.path();
    std::fs::write(wt.join("a.txt"), b"a\n").unwrap();
    let repo = Repository::open(
        &wt.join(".git"),
        &grit_lib::config::environment::RepositoryOptions::empty().with_work_tree(wt),
    )
    .expect("open");
    commit_worktree(
        &repo,
        &commit_req("one"),
        &mut NullProgress,
        &grit_lib::progress::CancelToken::never(),
    )
    .expect("first");
    let tip = match repo.references().head().expect("head before detach") {
        HeadState::Branch { oid: Some(oid), .. } => oid,
        other => panic!("expected branch tip, got {other:?}"),
    };
    switch_repository(
        &repo,
        &SwitchOptions {
            mode: SwitchMode::Detached {
                commit: tip,
                reflog_to_label: "HEAD".to_owned(),
            },
            dry_run: false,
            hooks: grit_lib::command_runner::hooks::PostOperationHooks::Run,
        },
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::CancelToken::never(),
    )
    .expect("detach");

    let mut amend = commit_req("amended");
    amend.amend = true;
    let outcome = commit_worktree(
        &repo,
        &amend,
        &mut NullProgress,
        &grit_lib::progress::CancelToken::never(),
    )
    .expect("amend detached");
    assert_eq!(outcome.branch, None);
    match repo.references().head().expect("head") {
        HeadState::Detached { oid } => assert_eq!(oid, outcome.oid),
        other => panic!("expected detached HEAD, got {other:?}"),
    }
}
