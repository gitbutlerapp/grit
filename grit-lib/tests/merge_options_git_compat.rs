//! Merge option compatibility with the system `git` binary.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::{parse_commit, ObjectId, ObjectKind};
use grit_lib::porcelain::commit::pending_commit_message;
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::porcelain::merge::{
    abort_branch_merge, integrate_branch_with, FastForwardPolicy, IntegrateBranchOutcome,
    MergeCommitMode, MergeOptions,
};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;
use std::process::Command;

fn grit_repo(tmp: &tempfile::TempDir) -> Repository {
    Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("open grit repo")
}

fn setup_identities(path: &std::path::Path) {
    git(path, &["config", "user.email", "t@example.com"]);
    git(path, &["config", "user.name", "Test"]);
}

fn diverged_repo() -> (tempfile::TempDir, ObjectId, ObjectId) {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main"]);
    setup_identities(tmp.path());
    std::fs::write(tmp.path().join("shared.txt"), "base\n").expect("write");
    git(tmp.path(), &["add", "shared.txt"]);
    git(tmp.path(), &["commit", "-m", "base"]);
    git(tmp.path(), &["switch", "-c", "topic"]);
    std::fs::write(tmp.path().join("shared.txt"), "topic\n").expect("write");
    git(tmp.path(), &["add", "shared.txt"]);
    git(tmp.path(), &["commit", "-m", "topic"]);
    let topic: ObjectId = git(tmp.path(), &["rev-parse", "topic"])
        .trim()
        .parse()
        .expect("topic");
    git(tmp.path(), &["checkout", "main"]);
    std::fs::write(tmp.path().join("main-only.txt"), "main\n").expect("write");
    git(tmp.path(), &["add", "main-only.txt"]);
    git(tmp.path(), &["commit", "-m", "main"]);
    let main: ObjectId = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("main");
    (tmp, main, topic)
}

fn commit_ident() -> String {
    "Merge Test <merge@example.com> 1700000000 +0000".to_owned()
}

fn empty_commit_req() -> CommitRequest {
    let id = commit_ident();
    CommitRequest {
        message: String::new(),
        author: id.clone(),
        committer: id,
        ..Default::default()
    }
}

fn conflict_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main"]);
    setup_identities(tmp.path());
    std::fs::write(tmp.path().join("f"), "base\n").expect("write");
    git(tmp.path(), &["add", "f"]);
    git(tmp.path(), &["commit", "-m", "base"]);
    git(tmp.path(), &["switch", "-c", "topic"]);
    std::fs::write(tmp.path().join("f"), "theirs\n").expect("write");
    git(tmp.path(), &["add", "f"]);
    git(tmp.path(), &["commit", "-m", "theirs"]);
    git(tmp.path(), &["switch", "main"]);
    std::fs::write(tmp.path().join("f"), "ours\n").expect("write");
    git(tmp.path(), &["add", "f"]);
    git(tmp.path(), &["commit", "-m", "ours"]);
    tmp
}

fn linear_ff_repo() -> (tempfile::TempDir, ObjectId, ObjectId) {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main"]);
    setup_identities(tmp.path());
    std::fs::write(tmp.path().join("f.txt"), "a\n").expect("write");
    git(tmp.path(), &["add", "f.txt"]);
    git(tmp.path(), &["commit", "-m", "base"]);
    let main: ObjectId = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("main");
    git(tmp.path(), &["switch", "-c", "topic"]);
    std::fs::write(tmp.path().join("f.txt"), "b\n").expect("write");
    git(tmp.path(), &["add", "f.txt"]);
    git(tmp.path(), &["commit", "-m", "topic"]);
    let topic: ObjectId = git(tmp.path(), &["rev-parse", "topic"])
        .trim()
        .parse()
        .expect("topic");
    git(tmp.path(), &["switch", "main"]);
    (tmp, main, topic)
}

#[test]
fn ff_only_refuses_divergent_and_changes_nothing() {
    let (tmp, main, topic) = diverged_repo();
    let head_before = git(tmp.path(), &["rev-parse", "HEAD"]).trim().to_owned();
    let index_before = std::fs::read(tmp.path().join(".git/index")).expect("index");

    let repo = grit_repo(&tmp);
    let err = integrate_branch_with(
        &repo,
        "refs/heads/main",
        main,
        topic,
        "topic",
        &MergeOptions {
            fast_forward: FastForwardPolicy::Only,
            ..MergeOptions::default()
        },
    )
    .expect_err("ff-only must fail");
    assert!(matches!(err, Error::NotFastForward));
    assert_eq!(git(tmp.path(), &["rev-parse", "HEAD"]).trim(), head_before);
    assert_eq!(
        std::fs::read(tmp.path().join(".git/index")).expect("index"),
        index_before
    );
    git(tmp.path(), &["fsck", "--strict"]);
}

#[test]
fn no_ff_creates_two_parent_commit_git_fsck_clean() {
    let (tmp, main, topic) = linear_ff_repo();
    let repo = grit_repo(&tmp);
    let outcome = integrate_branch_with(
        &repo,
        "refs/heads/main",
        main,
        topic,
        "topic",
        &MergeOptions {
            fast_forward: FastForwardPolicy::Never,
            ..MergeOptions::default()
        },
    )
    .expect("no-ff merge");
    let IntegrateBranchOutcome::Merged { oid, .. } = outcome else {
        panic!("expected merged, got {outcome:?}");
    };
    let obj = repo.odb().read(&oid).expect("read merge commit");
    assert_eq!(obj.kind, ObjectKind::Commit);
    let commit = parse_commit(&obj.data).expect("parse");
    assert_eq!(commit.parents.len(), 2);
    assert_eq!(commit.parents[0], main);
    assert_eq!(commit.parents[1], topic);
    git(tmp.path(), &["fsck", "--strict"]);
}

#[test]
fn squash_writes_squash_msg_no_merge_head() {
    let (tmp, main, topic) = linear_ff_repo();
    let repo = grit_repo(&tmp);
    let outcome = integrate_branch_with(
        &repo,
        "refs/heads/main",
        main,
        topic,
        "topic",
        &MergeOptions {
            commit: MergeCommitMode::Squash,
            ..MergeOptions::default()
        },
    )
    .expect("squash merge");
    assert_eq!(outcome, IntegrateBranchOutcome::Squashed);
    assert!(!tmp.path().join(".git/MERGE_HEAD").exists());
    assert!(tmp.path().join(".git/SQUASH_MSG").exists());
    assert_eq!(
        git(tmp.path(), &["rev-parse", "HEAD"]).trim(),
        main.to_hex()
    );
    git(tmp.path(), &["commit", "--no-edit"]);
    let parent_count = git(tmp.path(), &["rev-list", "--count", "-n", "1", "HEAD"])
        .trim()
        .to_owned();
    assert_eq!(parent_count, "1", "squash commit must be single-parent");
    git(tmp.path(), &["fsck", "--strict"]);
}

#[test]
fn no_commit_state_readable_by_git() {
    let (tmp, main, topic) = linear_ff_repo();
    let repo = grit_repo(&tmp);
    let outcome = integrate_branch_with(
        &repo,
        "refs/heads/main",
        main,
        topic,
        "topic",
        &MergeOptions {
            commit: MergeCommitMode::NoCommit,
            ..MergeOptions::default()
        },
    )
    .expect("no-commit merge");
    assert_eq!(outcome, IntegrateBranchOutcome::Stopped);
    assert!(tmp.path().join(".git/MERGE_HEAD").exists());
    let status = Command::new("git")
        .current_dir(tmp.path())
        .args(["status"])
        .output()
        .expect("git status");
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        stdout.contains("merging") || stdout.contains("Merging"),
        "git status should report merge in progress: {stdout}"
    );
    git(tmp.path(), &["commit", "--no-edit"]);
    git(tmp.path(), &["fsck", "--strict"]);
}

#[test]
fn message_override_used() {
    let (tmp, main, topic) = diverged_repo();
    let repo = grit_repo(&tmp);
    let custom = "Custom merge title\n";
    let outcome = integrate_branch_with(
        &repo,
        "refs/heads/main",
        main,
        topic,
        "topic",
        &MergeOptions {
            message: Some(custom.to_owned()),
            ..MergeOptions::default()
        },
    )
    .expect("merge with message");
    let IntegrateBranchOutcome::Merged { oid, .. } = outcome else {
        panic!("expected merged, got {outcome:?}");
    };
    let obj = repo.odb().read(&oid).expect("read commit");
    let commit = parse_commit(&obj.data).expect("parse");
    assert_eq!(commit.message, custom);
    git(tmp.path(), &["fsck", "--strict"]);
}

#[test]
fn no_commit_merge_mode_bytes_match_git() {
    let (tmp, main, topic) = diverged_repo();
    let repo = grit_repo(&tmp);
    integrate_branch_with(
        &repo,
        "refs/heads/main",
        main,
        topic,
        "topic",
        &MergeOptions {
            commit: MergeCommitMode::NoCommit,
            ..MergeOptions::default()
        },
    )
    .expect("no-commit");
    let mode_path = tmp.path().join(".git/MERGE_MODE");
    assert!(mode_path.is_file(), "MERGE_MODE must exist");
    assert_eq!(std::fs::read(&mode_path).expect("read mode"), b"");

    git(tmp.path(), &["reset", "--hard", main.to_hex().as_str()]);
    integrate_branch_with(
        &repo,
        "refs/heads/main",
        main,
        topic,
        "topic",
        &MergeOptions {
            fast_forward: FastForwardPolicy::Never,
            commit: MergeCommitMode::NoCommit,
            ..MergeOptions::default()
        },
    )
    .expect("no-ff no-commit");
    assert_eq!(
        std::fs::read(tmp.path().join(".git/MERGE_MODE")).expect("read mode"),
        b"no-ff"
    );
}

#[test]
fn squash_conflict_writes_merge_and_squash_msg_no_merge_head() {
    let tmp = conflict_repo();
    let main: ObjectId = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("main");
    let topic: ObjectId = git(tmp.path(), &["rev-parse", "topic"])
        .trim()
        .parse()
        .expect("topic");
    let repo = grit_repo(&tmp);
    let outcome = integrate_branch_with(
        &repo,
        "refs/heads/main",
        main,
        topic,
        "topic",
        &MergeOptions {
            commit: MergeCommitMode::Squash,
            ..MergeOptions::default()
        },
    )
    .expect("squash with conflicts");
    assert!(matches!(outcome, IntegrateBranchOutcome::Conflicts { .. }));
    assert!(!tmp.path().join(".git/MERGE_HEAD").exists());
    assert!(tmp.path().join(".git/SQUASH_MSG").exists());
    assert!(tmp.path().join(".git/ORIG_HEAD").exists());
    assert_eq!(
        std::fs::read(tmp.path().join(".git/MERGE_MSG")).expect("merge msg"),
        b"\n# Conflicts:\n#\tf\n"
    );
}

#[test]
fn squash_msg_date_line_matches_git() {
    let (tmp, main, topic) = linear_ff_repo();
    git(tmp.path(), &["merge", "--squash", "topic"]);
    let git_squash =
        std::fs::read_to_string(tmp.path().join(".git/SQUASH_MSG")).expect("git squash");
    let git_date = git_squash
        .lines()
        .find(|l| l.starts_with("Date:"))
        .expect("git date line");

    git(tmp.path(), &["reset", "--hard", main.to_hex().as_str()]);
    let repo = grit_repo(&tmp);
    integrate_branch_with(
        &repo,
        "refs/heads/main",
        main,
        topic,
        "topic",
        &MergeOptions {
            commit: MergeCommitMode::Squash,
            ..MergeOptions::default()
        },
    )
    .expect("grit squash");
    let grit_squash =
        std::fs::read_to_string(tmp.path().join(".git/SQUASH_MSG")).expect("grit squash");
    let grit_date = grit_squash
        .lines()
        .find(|l| l.starts_with("Date:"))
        .expect("grit date line");
    assert_eq!(grit_date, git_date, "SQUASH_MSG Date line must match git");
}

#[test]
fn squash_conflict_abort_restores_pre_merge_state() {
    let tmp = conflict_repo();
    let main: ObjectId = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("main");
    let topic: ObjectId = git(tmp.path(), &["rev-parse", "topic"])
        .trim()
        .parse()
        .expect("topic");
    let repo = grit_repo(&tmp);
    integrate_branch_with(
        &repo,
        "refs/heads/main",
        main,
        topic,
        "topic",
        &MergeOptions {
            commit: MergeCommitMode::Squash,
            ..MergeOptions::default()
        },
    )
    .expect("squash conflicts");
    abort_branch_merge(&repo).expect("abort squash conflict");
    assert_eq!(
        git(tmp.path(), &["rev-parse", "HEAD"]).trim(),
        main.to_hex()
    );
    assert!(!tmp.path().join(".git/SQUASH_MSG").exists());
    assert!(!tmp.path().join(".git/MERGE_MSG").exists());
    git(tmp.path(), &["fsck", "--strict"]);
}

#[test]
fn squash_conflict_pending_message_prefers_merge_msg() {
    let tmp = conflict_repo();
    let main: ObjectId = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("main");
    let topic: ObjectId = git(tmp.path(), &["rev-parse", "topic"])
        .trim()
        .parse()
        .expect("topic");
    let custom = "Squash title from -m\n";
    let repo = grit_repo(&tmp);
    integrate_branch_with(
        &repo,
        "refs/heads/main",
        main,
        topic,
        "topic",
        &MergeOptions {
            commit: MergeCommitMode::Squash,
            message: Some(custom.to_owned()),
            ..MergeOptions::default()
        },
    )
    .expect("squash conflicts");
    let pending = pending_commit_message(&repo)
        .expect("pending")
        .expect("message");
    assert!(
        pending.starts_with(custom.trim()),
        "pending message must include custom -m text, got: {pending:?}"
    );
    assert!(
        !pending.starts_with("Squashed commit of the following"),
        "must not use SQUASH_MSG template while MERGE_MSG holds the edit message"
    );
}

#[test]
fn squash_commit_clears_squash_msg() {
    let (tmp, main, topic) = linear_ff_repo();
    let repo = grit_repo(&tmp);
    integrate_branch_with(
        &repo,
        "refs/heads/main",
        main,
        topic,
        "topic",
        &MergeOptions {
            commit: MergeCommitMode::Squash,
            ..MergeOptions::default()
        },
    )
    .expect("squash");
    assert!(tmp.path().join(".git/SQUASH_MSG").exists());
    create_commit(&repo, &empty_commit_req(), &mut NullProgress).expect("squash commit");
    assert!(!tmp.path().join(".git/SQUASH_MSG").exists());
    git(tmp.path(), &["fsck", "--strict"]);
}

#[test]
fn squash_commit_does_not_reuse_squash_msg_on_following_commit() {
    let (tmp, main, topic) = linear_ff_repo();
    let repo = grit_repo(&tmp);
    integrate_branch_with(
        &repo,
        "refs/heads/main",
        main,
        topic,
        "topic",
        &MergeOptions {
            commit: MergeCommitMode::Squash,
            ..MergeOptions::default()
        },
    )
    .expect("squash");
    let squash_msg =
        std::fs::read_to_string(tmp.path().join(".git/SQUASH_MSG")).expect("squash msg");
    create_commit(&repo, &empty_commit_req(), &mut NullProgress).expect("squash commit");
    assert!(!tmp.path().join(".git/SQUASH_MSG").exists());

    std::fs::write(tmp.path().join("follow.txt"), "next\n").expect("write");
    git(tmp.path(), &["add", "follow.txt"]);
    let err = create_commit(&repo, &empty_commit_req(), &mut NullProgress).expect_err("no msg");
    assert!(
        matches!(err, Error::EmptyCommitMessage),
        "unexpected: {err:?}"
    );
    assert_ne!(squash_msg.trim(), "");
}

#[test]
fn squash_conflict_resolves_with_git_commit_no_edit() {
    let tmp = conflict_repo();
    let main: ObjectId = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("main");
    let topic: ObjectId = git(tmp.path(), &["rev-parse", "topic"])
        .trim()
        .parse()
        .expect("topic");
    let repo = grit_repo(&tmp);
    integrate_branch_with(
        &repo,
        "refs/heads/main",
        main,
        topic,
        "topic",
        &MergeOptions {
            commit: MergeCommitMode::Squash,
            ..MergeOptions::default()
        },
    )
    .expect("squash with conflicts");
    assert!(!tmp.path().join(".git/MERGE_HEAD").exists());
    assert!(tmp.path().join(".git/SQUASH_MSG").exists());

    std::fs::write(tmp.path().join("f"), "resolved\n").expect("resolve");
    git(tmp.path(), &["add", "f"]);
    let status = git(tmp.path(), &["status", "--porcelain"]);
    assert!(
        !status.contains("UU"),
        "conflicts should be cleared: {status}"
    );

    git(tmp.path(), &["commit", "--no-edit"]);
    assert!(!tmp.path().join(".git/SQUASH_MSG").exists());
    let head: ObjectId = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("head");
    let obj = repo.odb().read(&head).expect("read head");
    let commit = parse_commit(&obj.data).expect("parse head");
    assert_eq!(
        commit.parents.len(),
        1,
        "squash finish must be a single-parent commit, not a merge commit"
    );
    git(tmp.path(), &["fsck", "--strict"]);
}
