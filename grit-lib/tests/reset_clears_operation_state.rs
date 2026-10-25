//! Issue #1965: reset must end in-progress merge and cherry-pick state like Git.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::porcelain::merge::{integrate_branch, IntegrateBranchOutcome};
use grit_lib::porcelain::replay::ReplayDirection;
use grit_lib::porcelain::replay_sequence::{
    start_replay_sequence, ReplayEmptyPolicy, ReplayOptions, ReplaySequenceRequest, SequenceOutcome,
};
use grit_lib::porcelain::reset::{reset_to_commit, ResetMode, ResetOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::state::{primary_in_progress_operation, resolve_head_for_repository};

const IDENT: &str = "Test User <t@example.com> 1700000000 +0000";

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git spawn");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn init_repo(root: &Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
}

fn open_repo(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open")
}

fn commit_file(root: &Path, path: &str, contents: &str, msg: &str) -> ObjectId {
    fs::write(root.join(path), contents).expect("write");
    git(root, &["add", path]);
    git(root, &["commit", "-qm", msg]);
    ObjectId::from_hex(git(root, &["rev-parse", "HEAD"]).trim()).expect("oid")
}

fn commit_req(message: &str) -> CommitRequest {
    CommitRequest {
        message: message.to_owned(),
        author: Some(IDENT.parse().expect("author")),
        committer: Some(IDENT.parse().expect("committer")),
        allow_empty: false,
        ..Default::default()
    }
}

fn conflicted_merge_repo() -> (tempfile::TempDir, Repository, ObjectId) {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "f", "l1\nl2\n", "base");
    git(root.path(), &["switch", "-c", "o"]);
    commit_file(root.path(), "f", "O\n", "o");
    let other = ObjectId::from_hex(git(root.path(), &["rev-parse", "HEAD"]).trim()).expect("o");
    git(root.path(), &["switch", "main"]);
    commit_file(root.path(), "f", "M\n", "m");
    let main = ObjectId::from_hex(git(root.path(), &["rev-parse", "HEAD"]).trim()).expect("main");

    let repo = open_repo(root.path());
    let outcome = integrate_branch(&repo, "refs/heads/main", main, other, "o").expect("merge");
    assert!(matches!(outcome, IntegrateBranchOutcome::Conflicts { .. }));
    assert!(repo.git_dir().join("MERGE_HEAD").exists());
    (root, repo, main)
}

fn pick_req(commits: Vec<ObjectId>) -> ReplaySequenceRequest {
    ReplaySequenceRequest {
        commits,
        direction: ReplayDirection::Pick,
        committer: IDENT.to_owned(),
        options: ReplayOptions {
            empty: ReplayEmptyPolicy::Stop,
            ..ReplayOptions::default()
        },
    }
}

fn conflicted_pick_repo() -> (tempfile::TempDir, Repository, ObjectId) {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "f", "base\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    commit_file(root.path(), "f", "topic\n", "topic");
    let pick = ObjectId::from_hex(git(root.path(), &["rev-parse", "HEAD"]).trim()).expect("pick");
    git(root.path(), &["switch", "main"]);
    commit_file(root.path(), "f", "main\n", "main");
    let main = ObjectId::from_hex(git(root.path(), &["rev-parse", "HEAD"]).trim()).expect("main");

    let repo = open_repo(root.path());
    let out = start_replay_sequence(&repo, &pick_req(vec![pick])).expect("pick");
    assert!(matches!(out, SequenceOutcome::Conflicts { .. }));
    assert!(repo.git_dir().join("CHERRY_PICK_HEAD").exists());
    (root, repo, main)
}

fn assert_no_operation_state(repo: &Repository) {
    assert!(!repo.git_dir().join("MERGE_HEAD").exists());
    assert!(!repo.git_dir().join("CHERRY_PICK_HEAD").exists());
    let head = resolve_head_for_repository(repo).expect("head");
    let state = grit_lib::state::wt_status_get_state(repo.git_dir(), &head, false).expect("state");
    assert!(
        !state.merge_in_progress && !state.cherry_pick_in_progress,
        "expected no merge/pick in progress: {state:?}"
    );
    assert!(
        primary_in_progress_operation(&state).is_none(),
        "expected clean operation state"
    );
}

fn reset_head(repo: &Repository, head: ObjectId, mode: ResetMode) {
    reset_to_commit(
        repo,
        &ResetOptions::new(head, mode, IDENT, "reset"),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("reset");
}

#[test]
fn hard_reset_to_head_after_conflicted_merge_clears_state() {
    let (_root, repo, main) = conflicted_merge_repo();
    reset_head(&repo, main, ResetMode::Hard);
    assert_no_operation_state(&repo);

    let err = create_commit(&repo, &commit_req("oops"), &mut NullProgress).unwrap_err();
    assert!(
        matches!(err, Error::NothingToCommit),
        "commit after reset must not create a merge: {err}"
    );
}

#[test]
fn mixed_reset_to_head_after_conflicted_merge_clears_state() {
    let (_root, repo, main) = conflicted_merge_repo();
    reset_head(&repo, main, ResetMode::Mixed);
    assert_no_operation_state(&repo);
}

#[test]
fn soft_reset_to_head_after_conflicted_merge_clears_state() {
    let (_root, repo, main) = conflicted_merge_repo();
    reset_head(&repo, main, ResetMode::Soft);
    assert_no_operation_state(&repo);
}

#[test]
fn hard_reset_to_head_after_conflicted_pick_clears_state() {
    let (_root, repo, main) = conflicted_pick_repo();
    reset_head(&repo, main, ResetMode::Hard);
    assert_no_operation_state(&repo);

    let err = create_commit(&repo, &commit_req("oops"), &mut NullProgress).unwrap_err();
    assert!(matches!(err, Error::NothingToCommit), "{err}");
}

#[test]
fn grit_reset_matches_git_porcelain_after_conflicted_merge() {
    let grit_dir = tempfile::tempdir().expect("grit dir");
    let git_dir = tempfile::tempdir().expect("git dir");

    for root in [grit_dir.path(), git_dir.path()] {
        init_repo(root);
        commit_file(root, "f", "l1\nl2\n", "base");
        git(root, &["switch", "-c", "o"]);
        commit_file(root, "f", "O\n", "o");
        git(root, &["switch", "main"]);
        commit_file(root, "f", "M\n", "m");
    }

    let grit_repo = open_repo(grit_dir.path());
    let main =
        ObjectId::from_hex(git(grit_dir.path(), &["rev-parse", "main"]).trim()).expect("main");
    let other = ObjectId::from_hex(git(grit_dir.path(), &["rev-parse", "o"]).trim()).expect("o");
    let outcome =
        integrate_branch(&grit_repo, "refs/heads/main", main, other, "o").expect("grit merge");
    assert!(matches!(outcome, IntegrateBranchOutcome::Conflicts { .. }));

    let merge_out = Command::new("git")
        .current_dir(git_dir.path())
        .args(["merge", "o"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git merge");
    assert!(
        !merge_out.status.success(),
        "git merge should stop on conflict"
    );
    assert!(git_dir.path().join(".git/MERGE_HEAD").exists());

    reset_head(&grit_repo, main, ResetMode::Hard);
    git(git_dir.path(), &["reset", "--hard"]);

    let grit_porcelain = git(grit_dir.path(), &["status", "--porcelain"]);
    let git_porcelain = git(git_dir.path(), &["status", "--porcelain"]);
    assert_eq!(grit_porcelain.trim(), git_porcelain.trim());
    assert_no_operation_state(&grit_repo);

    let head_oid =
        ObjectId::from_hex(git(grit_dir.path(), &["rev-parse", "HEAD"]).trim()).expect("head oid");
    let grit_head =
        parse_commit(&grit_repo.objects().read(&head_oid).expect("read").data).expect("parse");
    assert_eq!(grit_head.parents.len(), 1, "HEAD must stay a normal commit");
}
