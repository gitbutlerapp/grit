//! Detached HEAD commits on grit reftable repositories (issue #1513 review).

mod support;

use grit_lib::porcelain::commit::{commit_worktree, CommitRequest, HookPolicy};
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::state::{resolve_head, HeadState};
use support::{git, git_init_reftable_repo, require_reftable_git};

fn ident() -> String {
    "Reftable Detached <reftable-detached@example.com> 1700000000 +0000".to_owned()
}

fn commit_req(message: &str) -> CommitRequest {
    let id = ident();
    CommitRequest {
        message: message.to_owned(),
        author: id.clone(),
        committer: id,
        allow_empty: false,
        sign_override: None,
        amend: false,
        assume_worktree_synced: false,
        skip_pre_commit_tree_match: false,
        staged_change_count: None,
        hooks: HookPolicy::SkipVerify,
        ..Default::default()
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
    commit_worktree(&repo, &commit_req("one"), &mut NullProgress).expect("first commit");
    let main_tip = grit_lib::refs::resolve_ref(repo.git_dir(), "refs/heads/main").unwrap();

    let tip = match resolve_head(repo.git_dir()).expect("head before detach") {
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
        },
    )
    .expect("detach");

    std::fs::write(wt.join("b.txt"), b"b\n").unwrap();
    let outcome = commit_worktree(&repo, &commit_req("detached"), &mut NullProgress)
        .expect("detached commit");
    assert_eq!(outcome.branch, "detached HEAD");
    assert_ne!(outcome.oid, main_tip);

    match resolve_head(repo.git_dir()).expect("head") {
        HeadState::Detached { oid } => assert_eq!(oid, outcome.oid),
        other => panic!("expected detached HEAD, got {other:?}"),
    }
    assert_eq!(
        grit_lib::refs::resolve_ref(repo.git_dir(), "refs/heads/main").unwrap(),
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
    commit_worktree(&repo, &commit_req("one"), &mut NullProgress).expect("first");
    let tip = match resolve_head(repo.git_dir()).expect("head before detach") {
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
        },
    )
    .expect("detach");

    let mut amend = commit_req("amended");
    amend.amend = true;
    let outcome = commit_worktree(&repo, &amend, &mut NullProgress).expect("amend detached");
    assert_eq!(outcome.branch, "detached HEAD");
    match resolve_head(repo.git_dir()).expect("head") {
        HeadState::Detached { oid } => assert_eq!(oid, outcome.oid),
        other => panic!("expected detached HEAD, got {other:?}"),
    }
}
