//! Git on-disk compatibility tests for [`grit_lib::porcelain::replay_sequence`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::change_id::change_id;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::porcelain::commit::{commit_with_scope, CommitRequest, CommitScope};
use grit_lib::porcelain::replay::ReplayDirection;
use grit_lib::porcelain::replay_sequence::{
    abort_replay_sequence, commits_to_replay, continue_replay_sequence, predict_replay_sequence,
    skip_replay_step, start_replay_sequence, ReplayAbortOutcome, ReplayCreated, ReplayEmptyPolicy,
    ReplayOptions, ReplaySequenceRequest, SequenceOutcome, SequencePrediction,
};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;
const IDENT: &str = "Test User <t@example.com> 1700000000 +0000";

fn replay_ident() -> grit_lib::ident::Signature {
    IDENT.parse().expect("IDENT")
}

fn git(root: &Path, args: &[&str]) -> String {
    let home = root.join(".git-home");
    let _ = fs::create_dir_all(&home);
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("HOME", &home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
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

fn assert_no_replay_state(root: &Path) {
    let git_dir = root.join(".git");
    assert!(
        !git_dir.join("sequencer").exists(),
        "sequencer directory must not remain after a failed start with no progress"
    );
    for name in ["CHERRY_PICK_HEAD", "REVERT_HEAD", "MERGE_MSG"] {
        assert!(
            !git_dir.join(name).exists(),
            "{name} must not remain after a failed start with no progress"
        );
    }
}

fn init_repo(root: &Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "gc.auto", "0"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    git(root, &["config", "gpg.program", "/bin/true"]);
}

fn open_repo(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open")
}

fn commit_file(root: &Path, path: &str, contents: &str, msg: &str) -> ObjectId {
    let full = root.join(path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(&full, contents).expect("write");
    git(root, &["add", path]);
    git(root, &["commit", "-qm", msg]);
    oid_from_rev(root, "HEAD")
}

fn oid_from_rev(root: &Path, rev: &str) -> ObjectId {
    let hex = git(root, &["rev-parse", rev]).trim().to_owned();
    ObjectId::from_hex(&hex).expect("oid")
}

fn git_fsck_strict(root: &Path) {
    let out = Command::new("git")
        .current_dir(root)
        .args(["fsck", "--strict"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fsck");
    assert!(
        out.status.success(),
        "git fsck --strict failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn head_tree_hex(root: &Path) -> String {
    git(root, &["rev-parse", "HEAD^{tree}"]).trim().to_owned()
}

fn pick_req(commits: Vec<ObjectId>, opts: ReplayOptions) -> ReplaySequenceRequest {
    ReplaySequenceRequest {
        commits,
        direction: ReplayDirection::Pick,
        committer: IDENT.parse().expect("IDENT"),
        options: opts,
    }
}

fn revert_req(commits: Vec<ObjectId>, opts: ReplayOptions) -> ReplaySequenceRequest {
    ReplaySequenceRequest {
        commits,
        direction: ReplayDirection::Revert,
        committer: IDENT.parse().expect("IDENT"),
        options: opts,
    }
}

#[test]
fn commits_to_replay_expands_range_oldest_first() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "a.txt", "base\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    commit_file(root.path(), "b.txt", "one\n", "one");
    commit_file(root.path(), "c.txt", "two\n", "two");
    let c1 = oid_from_rev(root.path(), "topic~1");
    let c2 = oid_from_rev(root.path(), "topic");
    git(root.path(), &["switch", "main"]);
    commit_file(root.path(), "d.txt", "main\n", "main");
    let base = oid_from_rev(root.path(), "main");

    let repo = open_repo(root.path());
    let expanded =
        commits_to_replay(&repo, &[format!("{}..{}", base.to_hex(), c2.to_hex())]).expect("expand");
    assert_eq!(expanded, vec![c1, c2]);
}

#[test]
fn pick_range_clean_matches_git_cherry_pick_trees() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "a.txt", "base\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    commit_file(root.path(), "b.txt", "one\n", "one");
    commit_file(root.path(), "c.txt", "two\n", "two");
    let c1 = oid_from_rev(root.path(), "topic~1");
    let c2 = oid_from_rev(root.path(), "topic");
    git(root.path(), &["switch", "main"]);
    commit_file(root.path(), "d.txt", "main\n", "main");

    let git_wt = tempfile::tempdir().expect("git copy");
    fs::create_dir_all(git_wt.path()).unwrap();
    Command::new("cp")
        .args([
            "-a",
            &format!("{}/.", root.path().display()),
            &git_wt.path().to_string_lossy(),
        ])
        .status()
        .unwrap();
    git(git_wt.path(), &["cherry-pick", &c1.to_hex(), &c2.to_hex()]);

    let repo = open_repo(root.path());
    let out = start_replay_sequence(
        &repo,
        &pick_req(vec![c1, c2], ReplayOptions::default()),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("pick");
    assert!(matches!(out, SequenceOutcome::Completed { .. }));
    assert_eq!(head_tree_hex(root.path()), head_tree_hex(git_wt.path()));
    git_fsck_strict(root.path());
}

#[test]
fn pick_stops_with_markers_and_cherry_pick_head() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "f", "base\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    fs::write(root.path().join("f"), "topic\n").unwrap();
    git(root.path(), &["commit", "-qam", "topic-f"]);
    let pick = oid_from_rev(root.path(), "HEAD");
    git(root.path(), &["switch", "main"]);
    fs::write(root.path().join("f"), "main\n").unwrap();
    git(root.path(), &["commit", "-qam", "main"]);

    let repo = open_repo(root.path());
    let out = start_replay_sequence(
        &repo,
        &pick_req(vec![pick], ReplayOptions::default()),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("pick");
    assert!(matches!(out, SequenceOutcome::Conflicts { .. }));
    assert!(root.path().join(".git/CHERRY_PICK_HEAD").is_file());
    assert!(root.path().join(".git/MERGE_MSG").is_file());
    let wt = fs::read_to_string(root.path().join("f")).unwrap();
    assert!(wt.contains("<<<<<<<"));
    git_fsck_strict(root.path());
}

#[test]
fn commit_after_pick_conflict_finishes_sequence_issue_2262() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "f.txt", "a\nb\nc\n", "base");
    git(root.path(), &["switch", "-c", "side"]);
    fs::write(root.path().join("f.txt"), "a\nSIDE\nc\n").unwrap();
    git(
        root.path(),
        &["commit", "-qam", "side", "--author=Other <o@example.com>"],
    );
    let side = oid_from_rev(root.path(), "HEAD");
    git(root.path(), &["switch", "main"]);
    fs::write(root.path().join("f.txt"), "a\nMAIN\nc\n").unwrap();
    git(root.path(), &["commit", "-qam", "mainchg"]);

    let repo = open_repo(root.path());
    start_replay_sequence(
        &repo,
        &pick_req(vec![side], ReplayOptions::default()),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("pick");
    fs::write(root.path().join("f.txt"), "a\nRES\nc\n").unwrap();

    let req = {
        let mut o = CommitRequest::default();
        o.message = "resolved pick".to_owned();
        o
    };
    let mut progress = NullProgress;
    commit_with_scope(
        &repo,
        &req,
        CommitScope::AutoStage,
        &mut progress,
        &NeverCancel,
    )
    .expect("commit finishes pick");

    assert_eq!(
        git(root.path(), &["log", "-1", "--format=%an"]).trim(),
        "Other"
    );
    assert_no_replay_state(root.path());
    abort_replay_sequence(&repo).expect_err("no replay after commit");
    git_fsck_strict(root.path());
}

#[test]
fn git_status_recognizes_grit_stop() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "f", "base\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    fs::write(root.path().join("f"), "topic\n").unwrap();
    git(root.path(), &["commit", "-qam", "topic-f"]);
    let pick = oid_from_rev(root.path(), "HEAD");
    git(root.path(), &["switch", "main"]);
    fs::write(root.path().join("f"), "main\n").unwrap();
    git(root.path(), &["commit", "-qam", "main"]);

    let repo = open_repo(root.path());
    start_replay_sequence(
        &repo,
        &pick_req(vec![pick], ReplayOptions::default()),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("pick");
    let st = git(root.path(), &["status", "--porcelain"]);
    assert!(st.contains("UU f") || st.contains("U f"));
    git_fsck_strict(root.path());
}

#[test]
fn grit_stop_then_git_cherry_pick_continue() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "f", "base\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    fs::write(root.path().join("f"), "topic\n").unwrap();
    git(root.path(), &["commit", "-qam", "topic-f"]);
    let pick = oid_from_rev(root.path(), "HEAD");
    git(root.path(), &["switch", "main"]);
    fs::write(root.path().join("f"), "main\n").unwrap();
    git(root.path(), &["commit", "-qam", "main"]);

    let repo = open_repo(root.path());
    start_replay_sequence(
        &repo,
        &pick_req(vec![pick], ReplayOptions::default()),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("pick");
    fs::write(root.path().join("f"), "resolved\n").unwrap();
    git(root.path(), &["add", "f"]);
    git(root.path(), &["cherry-pick", "--continue", "--no-gpg-sign"]);
    assert_eq!(
        git(root.path(), &["log", "-1", "--format=%s"]).trim(),
        "topic-f"
    );
    git_fsck_strict(root.path());
}

#[test]
fn git_cherry_pick_stop_then_grit_continue() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "f", "base\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    fs::write(root.path().join("f"), "topic\n").unwrap();
    git(root.path(), &["commit", "-qam", "topic-f"]);
    let pick = oid_from_rev(root.path(), "HEAD");
    git(root.path(), &["switch", "main"]);
    fs::write(root.path().join("f"), "main\n").unwrap();
    git(root.path(), &["commit", "-qam", "main"]);
    let cp = Command::new("git")
        .current_dir(root.path())
        .args(["cherry-pick", &pick.to_hex()])
        .output()
        .expect("cherry-pick");
    assert!(!cp.status.success(), "expected conflict");

    fs::write(root.path().join("f"), "resolved\n").unwrap();
    git(root.path(), &["add", "f"]);
    let repo = open_repo(root.path());
    let out = continue_replay_sequence(&repo, &replay_ident(), &mut NullProgress, &NeverCancel)
        .expect("continue");
    assert!(matches!(out, SequenceOutcome::Completed { .. }));
    git_fsck_strict(root.path());
}

#[test]
fn revert_stops_with_markers_and_revert_head() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "f", "base\n", "base");
    git(root.path(), &["switch", "-c", "side"]);
    fs::write(root.path().join("f"), "side\n").unwrap();
    git(root.path(), &["commit", "-qam", "side"]);
    let revert = oid_from_rev(root.path(), "HEAD");
    git(root.path(), &["switch", "main"]);
    fs::write(root.path().join("f"), "main\n").unwrap();
    git(root.path(), &["commit", "-qam", "main"]);

    let repo = open_repo(root.path());
    let out = start_replay_sequence(
        &repo,
        &revert_req(vec![revert], ReplayOptions::default()),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("revert");
    assert!(matches!(out, SequenceOutcome::Conflicts { .. }));
    assert!(root.path().join(".git/REVERT_HEAD").is_file());
    git_fsck_strict(root.path());
}

#[test]
fn revert_grit_stop_then_git_revert_continue() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "f", "base\n", "base");
    git(root.path(), &["switch", "-c", "side"]);
    fs::write(root.path().join("f"), "side\n").unwrap();
    git(root.path(), &["commit", "-qam", "side"]);
    let rev = oid_from_rev(root.path(), "HEAD");
    git(root.path(), &["switch", "main"]);
    fs::write(root.path().join("f"), "main\n").unwrap();
    git(root.path(), &["commit", "-qam", "main"]);

    let repo = open_repo(root.path());
    start_replay_sequence(
        &repo,
        &revert_req(vec![rev], ReplayOptions::default()),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("conflict");
    fs::write(root.path().join("f"), "base\n").unwrap();
    git(root.path(), &["add", "f"]);
    git(root.path(), &["revert", "--continue", "--no-gpg-sign"]);
    git_fsck_strict(root.path());
}

#[test]
fn skip_moves_to_next() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "f", "base\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    fs::write(root.path().join("f"), "topic\n").unwrap();
    git(root.path(), &["commit", "-qam", "topic-f"]);
    let c1 = oid_from_rev(root.path(), "HEAD");
    commit_file(root.path(), "a.txt", "a\n", "a");
    let c2 = oid_from_rev(root.path(), "HEAD");
    git(root.path(), &["switch", "main"]);
    fs::write(root.path().join("f"), "main\n").unwrap();
    git(root.path(), &["commit", "-qam", "main"]);

    let repo = open_repo(root.path());
    start_replay_sequence(
        &repo,
        &pick_req(vec![c1, c2], ReplayOptions::default()),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("pick");
    let out =
        skip_replay_step(&repo, &replay_ident(), &mut NullProgress, &NeverCancel).expect("skip");
    assert!(matches!(out, SequenceOutcome::Completed { .. }));
    assert!(root.path().join("a.txt").is_file());
    git_fsck_strict(root.path());
}

#[test]
fn abort_restores_branch_index_worktree() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "f", "base\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    fs::write(root.path().join("f"), "topic\n").unwrap();
    git(root.path(), &["commit", "-qam", "topic-f"]);
    let pick = oid_from_rev(root.path(), "HEAD");
    git(root.path(), &["switch", "main"]);
    fs::write(root.path().join("f"), "main\n").unwrap();
    git(root.path(), &["commit", "-qam", "main"]);
    let before = oid_from_rev(root.path(), "HEAD");

    let repo = open_repo(root.path());
    start_replay_sequence(
        &repo,
        &pick_req(vec![pick], ReplayOptions::default()),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("pick");
    abort_replay_sequence(&repo).expect("abort");
    assert_eq!(oid_from_rev(root.path(), "HEAD"), before);
    assert_eq!(fs::read_to_string(root.path().join("f")).unwrap(), "main\n");
    git_fsck_strict(root.path());
}

#[test]
fn abort_clears_state_when_head_moved() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "f", "base\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    fs::write(root.path().join("f"), "topic\n").unwrap();
    git(root.path(), &["commit", "-qam", "topic-f"]);
    let pick = oid_from_rev(root.path(), "HEAD");
    git(root.path(), &["switch", "main"]);
    fs::write(root.path().join("f"), "main\n").unwrap();
    git(root.path(), &["commit", "-qam", "main"]);

    let repo = open_repo(root.path());
    start_replay_sequence(
        &repo,
        &pick_req(vec![pick], ReplayOptions::default()),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("pick");
    git(root.path(), &["update-ref", "refs/heads/main", "HEAD~1"]);
    let outcome = abort_replay_sequence(&repo).expect("abort clears without rewind");
    assert_eq!(outcome, ReplayAbortOutcome::ClearedInPlace);
    assert_no_replay_state(root.path());
}

#[test]
fn continue_refuses_unmerged() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "f", "base\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    fs::write(root.path().join("f"), "topic\n").unwrap();
    git(root.path(), &["commit", "-qam", "topic-f"]);
    let pick = oid_from_rev(root.path(), "HEAD");
    git(root.path(), &["switch", "main"]);
    fs::write(root.path().join("f"), "main\n").unwrap();
    git(root.path(), &["commit", "-qam", "main"]);

    let repo = open_repo(root.path());
    start_replay_sequence(
        &repo,
        &pick_req(vec![pick], ReplayOptions::default()),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("pick");
    let err = continue_replay_sequence(&repo, &replay_ident(), &mut NullProgress, &NeverCancel)
        .unwrap_err();
    assert!(matches!(
        err,
        grit_lib::error::Error::ReplayContinueUnmerged
    ));
}

#[test]
fn revert_mainline_matches_git_revert_m1_tree() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "f", "a\n", "c1");
    git(root.path(), &["switch", "-c", "topic"]);
    fs::write(root.path().join("f"), "t\n").unwrap();
    git(root.path(), &["commit", "-qam", "topic"]);
    git(root.path(), &["switch", "main"]);
    commit_file(root.path(), "g", "g\n", "prep");
    git(
        root.path(),
        &["merge", "--no-ff", "-qm", "Merge topic", "topic"],
    );
    let merge = oid_from_rev(root.path(), "HEAD");

    let git_wt = tempfile::tempdir().expect("git copy");
    Command::new("cp")
        .args([
            "-a",
            &format!("{}/.", root.path().display()),
            &git_wt.path().to_string_lossy(),
        ])
        .status()
        .unwrap();
    git(
        git_wt.path(),
        &["revert", "-m", "1", "--no-edit", &merge.to_hex()],
    );

    let repo = open_repo(root.path());
    let mut opts = ReplayOptions::default();
    opts.mainline = std::num::NonZeroU32::new(1);
    let out = start_replay_sequence(
        &repo,
        &revert_req(vec![merge], opts),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("revert");
    assert!(matches!(out, SequenceOutcome::Completed { .. }));
    assert_eq!(head_tree_hex(root.path()), head_tree_hex(git_wt.path()));
    let msg = git(root.path(), &["log", "-1", "--format=%B"]);
    assert!(msg.contains("This reverts commit"));
    assert!(msg.contains("Mainline parent"));
    assert!(
        !msg.contains("reversing\nchanges made to"),
        "must not use Git sequencer.c revert body wording"
    );
    git_fsck_strict(root.path());
}

#[test]
fn merge_source_without_mainline_errors() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "f", "a\n", "c1");
    git(root.path(), &["switch", "-c", "topic"]);
    fs::write(root.path().join("f"), "t\n").unwrap();
    git(root.path(), &["commit", "-qam", "topic"]);
    git(root.path(), &["switch", "main"]);
    commit_file(root.path(), "g", "g\n", "prep");
    git(
        root.path(),
        &["merge", "--no-ff", "-qm", "Merge topic", "topic"],
    );
    let merge = oid_from_rev(root.path(), "HEAD");
    commit_file(root.path(), "after.txt", "after\n", "after");

    let repo = open_repo(root.path());
    let err = start_replay_sequence(
        &repo,
        &pick_req(vec![merge], ReplayOptions::default()),
        &mut NullProgress,
        &NeverCancel,
    )
    .unwrap_err();
    assert!(matches!(err, grit_lib::error::Error::MergeCommit { .. }));
    assert_no_replay_state(root.path());
}

#[test]
fn empty_stop_drop_keep() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "f", "a\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    git(root.path(), &["commit", "--allow-empty", "-qm", "empty"]);
    let empty = oid_from_rev(root.path(), "HEAD");
    git(root.path(), &["switch", "main"]);

    let repo = open_repo(root.path());
    let mut stop_opts = ReplayOptions::default();
    stop_opts.empty = ReplayEmptyPolicy::Stop;
    let out = start_replay_sequence(
        &repo,
        &pick_req(vec![empty], stop_opts),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("stop");
    assert!(matches!(out, SequenceOutcome::Empty { .. }));
    abort_replay_sequence(&repo).expect("abort");

    let mut drop_opts = ReplayOptions::default();
    drop_opts.empty = ReplayEmptyPolicy::Drop;
    let out = start_replay_sequence(
        &repo,
        &pick_req(vec![empty], drop_opts),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("drop");
    assert!(matches!(out, SequenceOutcome::Completed { .. }));

    git(root.path(), &["reset", "--hard", "HEAD"]);
    let mut keep_opts = ReplayOptions::default();
    keep_opts.empty = ReplayEmptyPolicy::Keep;
    let out = start_replay_sequence(
        &repo,
        &pick_req(vec![empty], keep_opts),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("keep");
    assert!(matches!(out, SequenceOutcome::Completed { .. }));
    git_fsck_strict(root.path());
}

#[test]
fn record_origin_appends_cherry_picked_from_line() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "base.txt", "base\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    commit_file(root.path(), "a.txt", "a\n", "subject line");
    let pick = oid_from_rev(root.path(), "HEAD");
    git(root.path(), &["switch", "main"]);
    commit_file(root.path(), "b.txt", "b\n", "main");

    let repo = open_repo(root.path());
    let mut opts = ReplayOptions::default();
    opts.record_origin = true;
    start_replay_sequence(
        &repo,
        &pick_req(vec![pick], opts),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("pick");
    let log_msg = git(root.path(), &["log", "-1", "--format=%B"]);
    assert!(log_msg.contains("(cherry picked from commit"));
    git_fsck_strict(root.path());
}

#[test]
fn abort_after_clean_step_then_conflict_restores_original_head() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "shared.txt", "base\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    commit_file(root.path(), "only-topic.txt", "one\n", "t1");
    fs::write(root.path().join("shared.txt"), "topic\n").unwrap();
    git(root.path(), &["add", "shared.txt"]);
    git(root.path(), &["commit", "-qm", "t2"]);
    let c1 = oid_from_rev(root.path(), "topic~1");
    let c2 = oid_from_rev(root.path(), "topic");
    git(root.path(), &["switch", "main"]);
    fs::write(root.path().join("shared.txt"), "main\n").unwrap();
    git(root.path(), &["add", "shared.txt"]);
    git(root.path(), &["commit", "-qm", "main"]);
    let start = oid_from_rev(root.path(), "HEAD");

    let repo = open_repo(root.path());
    let out = start_replay_sequence(
        &repo,
        &pick_req(vec![c1, c2], ReplayOptions::default()),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("pick");
    assert!(matches!(out, SequenceOutcome::Conflicts { .. }));
    assert!(root.path().join(".git/sequencer/todo").is_file());
    abort_replay_sequence(&repo).expect("abort");
    assert_eq!(oid_from_rev(root.path(), "HEAD"), start);
    assert!(!root.path().join(".git/CHERRY_PICK_HEAD").exists());
    git_fsck_strict(root.path());
}

#[test]
fn no_commit_range_accumulates_staged_changes() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "base.txt", "base\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    commit_file(root.path(), "a.txt", "a\n", "a");
    commit_file(root.path(), "b.txt", "b\n", "b");
    let c1 = oid_from_rev(root.path(), "topic~1");
    let c2 = oid_from_rev(root.path(), "topic");
    git(root.path(), &["switch", "main"]);
    let start = oid_from_rev(root.path(), "HEAD");

    let repo = open_repo(root.path());
    let mut opts = ReplayOptions::default();
    opts.no_commit = true;
    let out = start_replay_sequence(
        &repo,
        &pick_req(vec![c1, c2], opts),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("pick");
    assert!(matches!(out, SequenceOutcome::Completed { .. }));
    assert_eq!(oid_from_rev(root.path(), "HEAD"), start);
    let staged = git(root.path(), &["show", ":a.txt"]);
    assert_eq!(staged, "a\n");
    let staged_b = git(root.path(), &["show", ":b.txt"]);
    assert_eq!(staged_b, "b\n");
    git_fsck_strict(root.path());
}

#[test]
fn grit_stop_record_origin_git_cherry_pick_continue() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "f", "base\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    fs::write(root.path().join("f"), "topic\n").unwrap();
    git(root.path(), &["commit", "-qam", "topic-f"]);
    let pick = oid_from_rev(root.path(), "HEAD");
    git(root.path(), &["switch", "main"]);
    fs::write(root.path().join("f"), "main\n").unwrap();
    git(root.path(), &["commit", "-qam", "main"]);

    let repo = open_repo(root.path());
    let mut opts = ReplayOptions::default();
    opts.record_origin = true;
    start_replay_sequence(
        &repo,
        &pick_req(vec![pick], opts),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("pick");
    fs::write(root.path().join("f"), "resolved\n").unwrap();
    git(root.path(), &["add", "f"]);
    git(root.path(), &["cherry-pick", "--continue", "--no-gpg-sign"]);
    let body = git(root.path(), &["log", "-1", "--format=%B"]);
    assert!(body.contains("(cherry picked from commit"));
    assert_eq!(body.matches("(cherry picked from commit").count(), 1);
    git_fsck_strict(root.path());
}

#[test]
fn abort_after_clean_step_then_merge_error_restores_start() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "shared.txt", "base\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    commit_file(root.path(), "picked.txt", "picked\n", "topic-only");
    git(root.path(), &["switch", "main"]);
    commit_file(root.path(), "mainline.txt", "main\n", "main-prep");
    git(root.path(), &["switch", "topic"]);
    git(
        root.path(),
        &["merge", "--no-ff", "-qm", "Merge main into topic", "main"],
    );
    let merge = oid_from_rev(root.path(), "HEAD");
    let clean = oid_from_rev(root.path(), "topic~1");
    git(root.path(), &["switch", "main"]);
    commit_file(root.path(), "after.txt", "after\n", "after");
    let start = oid_from_rev(root.path(), "HEAD");

    let repo = open_repo(root.path());
    let err = start_replay_sequence(
        &repo,
        &pick_req(vec![clean, merge], ReplayOptions::default()),
        &mut NullProgress,
        &NeverCancel,
    )
    .unwrap_err();
    assert!(matches!(err, grit_lib::error::Error::MergeCommit { .. }));
    assert!(root.path().join(".git/sequencer/todo").is_file());
    abort_replay_sequence(&repo).expect("abort after partial sequence");
    assert_eq!(oid_from_rev(root.path(), "HEAD"), start);
    git_fsck_strict(root.path());
}

#[test]
fn clean_pick_preserves_preexisting_orig_head() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "a.txt", "base\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    commit_file(root.path(), "side.txt", "side\n", "side");
    let pick = oid_from_rev(root.path(), "HEAD");
    git(root.path(), &["switch", "main"]);
    commit_file(root.path(), "main.txt", "main\n", "main");
    let sentinel = oid_from_rev(root.path(), "main~0");
    fs::write(
        root.path().join(".git/ORIG_HEAD"),
        format!("{}\n", sentinel.to_hex()),
    )
    .expect("write ORIG_HEAD");

    let repo = open_repo(root.path());
    start_replay_sequence(
        &repo,
        &pick_req(vec![pick], ReplayOptions::default()),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("pick");
    let kept = fs::read_to_string(root.path().join(".git/ORIG_HEAD")).expect("read");
    assert_eq!(kept.trim(), sentinel.to_hex());
    git_fsck_strict(root.path());
}

#[test]
fn continue_after_no_commit_conflict_leaves_head() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "f", "base\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    fs::write(root.path().join("f"), "topic\n").unwrap();
    git(root.path(), &["commit", "-qam", "topic-f"]);
    let pick = oid_from_rev(root.path(), "HEAD");
    git(root.path(), &["switch", "main"]);
    fs::write(root.path().join("f"), "main\n").unwrap();
    git(root.path(), &["commit", "-qam", "main"]);
    let start = oid_from_rev(root.path(), "HEAD");

    let repo = open_repo(root.path());
    let mut opts = ReplayOptions::default();
    opts.no_commit = true;
    let out = start_replay_sequence(
        &repo,
        &pick_req(vec![pick], opts),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("pick");
    assert!(matches!(out, SequenceOutcome::Conflicts { .. }));
    fs::write(root.path().join("f"), "resolved\n").unwrap();
    git(root.path(), &["add", "f"]);
    let done = continue_replay_sequence(&repo, &replay_ident(), &mut NullProgress, &NeverCancel)
        .expect("continue");
    assert!(matches!(done, SequenceOutcome::Completed { .. }));
    assert_eq!(oid_from_rev(root.path(), "HEAD"), start);
    git_fsck_strict(root.path());
}

#[test]
fn commits_to_replay_skips_empty_range() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    let tip = commit_file(root.path(), "a.txt", "a\n", "only");
    let repo = open_repo(root.path());
    let expanded =
        commits_to_replay(&repo, &[format!("{}..{}", tip.to_hex(), tip.to_hex())]).expect("expand");
    assert!(expanded.is_empty());
}

#[test]
fn continue_after_range_conflict_pairs_all_created_sources() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "shared.txt", "base\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    fs::write(root.path().join("shared.txt"), "topic\n").unwrap();
    git(root.path(), &["add", "shared.txt"]);
    git(root.path(), &["commit", "-qm", "t1 conflict"]);
    commit_file(root.path(), "clean.txt", "clean\n", "t2 clean");
    let c1 = oid_from_rev(root.path(), "topic~1");
    let c2 = oid_from_rev(root.path(), "topic");
    git(root.path(), &["switch", "main"]);
    fs::write(root.path().join("shared.txt"), "main\n").unwrap();
    git(root.path(), &["add", "shared.txt"]);
    git(root.path(), &["commit", "-qm", "main"]);

    let repo = open_repo(root.path());
    let out = start_replay_sequence(
        &repo,
        &pick_req(vec![c1, c2], ReplayOptions::default()),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("pick");
    assert!(matches!(out, SequenceOutcome::Conflicts { .. }));
    fs::write(root.path().join("shared.txt"), "resolved\n").unwrap();
    git(root.path(), &["add", "shared.txt"]);
    let done = continue_replay_sequence(&repo, &replay_ident(), &mut NullProgress, &NeverCancel)
        .expect("continue");
    let created = match done {
        SequenceOutcome::Completed { created, .. } => created,
        _ => panic!("expected completed"),
    };
    assert_eq!(created.len(), 2);
    assert_eq!(
        created,
        vec![
            ReplayCreated {
                source: c1,
                new_oid: created[0].new_oid,
                change_id: created[0].change_id.clone(),
                subject: "t1 conflict".into(),
            },
            ReplayCreated {
                source: c2,
                new_oid: created[1].new_oid,
                change_id: created[1].change_id.clone(),
                subject: "t2 clean".into(),
            },
        ]
    );
    assert_ne!(created[0].new_oid, created[1].new_oid);
    git_fsck_strict(root.path());
}

#[test]
fn continue_omits_conflict_comment_lines_from_merge_msg() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "f", "base\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    fs::write(root.path().join("f"), "topic\n").unwrap();
    git(root.path(), &["commit", "-qam", "topic-f"]);
    let pick = oid_from_rev(root.path(), "HEAD");
    git(root.path(), &["switch", "main"]);
    fs::write(root.path().join("f"), "main\n").unwrap();
    git(root.path(), &["commit", "-qam", "main"]);

    let repo = open_repo(root.path());
    start_replay_sequence(
        &repo,
        &pick_req(vec![pick], ReplayOptions::default()),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("pick");
    fs::write(root.path().join("f"), "resolved\n").unwrap();
    git(root.path(), &["add", "f"]);
    fs::write(
        root.path().join(".git/MERGE_MSG"),
        "topic-f\n\n# Conflicts:\n# f\n",
    )
    .unwrap();
    continue_replay_sequence(&repo, &replay_ident(), &mut NullProgress, &NeverCancel)
        .expect("continue");
    let msg = git(root.path(), &["log", "-1", "--format=%B"]);
    assert!(!msg.contains("# Conflicts:"));
    assert!(msg.contains("topic-f"));
    git_fsck_strict(root.path());
}

#[test]
fn skip_after_partial_range_returns_earlier_created_records() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "shared.txt", "base\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    commit_file(root.path(), "clean.txt", "clean\n", "t1 clean");
    let c1 = oid_from_rev(root.path(), "HEAD");
    fs::write(root.path().join("shared.txt"), "topic\n").unwrap();
    git(root.path(), &["add", "shared.txt"]);
    git(root.path(), &["commit", "-qm", "t2 conflict"]);
    let c2 = oid_from_rev(root.path(), "HEAD");
    git(root.path(), &["switch", "main"]);
    fs::write(root.path().join("shared.txt"), "main\n").unwrap();
    git(root.path(), &["add", "shared.txt"]);
    git(root.path(), &["commit", "-qm", "main"]);

    let repo = open_repo(root.path());
    start_replay_sequence(
        &repo,
        &pick_req(vec![c1, c2], ReplayOptions::default()),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("pick");
    let out =
        skip_replay_step(&repo, &replay_ident(), &mut NullProgress, &NeverCancel).expect("skip");
    match out {
        SequenceOutcome::Completed { created, .. } => {
            assert_eq!(created.len(), 1);
            assert_eq!(created[0].source, c1);
        }
        _ => panic!("expected completed after skip"),
    }
    git_fsck_strict(root.path());
}

#[test]
fn continue_honours_persisted_no_commit() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "f", "base\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    fs::write(root.path().join("f"), "topic\n").unwrap();
    git(root.path(), &["commit", "-qam", "topic-f"]);
    let pick = oid_from_rev(root.path(), "HEAD");
    git(root.path(), &["switch", "main"]);
    fs::write(root.path().join("f"), "main\n").unwrap();
    git(root.path(), &["commit", "-qam", "main"]);
    let start = oid_from_rev(root.path(), "HEAD");

    let repo = open_repo(root.path());
    let mut opts = ReplayOptions::default();
    opts.no_commit = true;
    let out = start_replay_sequence(
        &repo,
        &pick_req(vec![pick], opts),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("pick");
    assert!(matches!(out, SequenceOutcome::Conflicts { .. }));
    fs::write(root.path().join("f"), "resolved\n").unwrap();
    git(root.path(), &["add", "f"]);
    let done = continue_replay_sequence(&repo, &replay_ident(), &mut NullProgress, &NeverCancel)
        .expect("continue");
    assert!(matches!(done, SequenceOutcome::Completed { .. }));
    assert_eq!(oid_from_rev(root.path(), "HEAD"), start);
    git_fsck_strict(root.path());
}

#[test]
fn commits_to_replay_empty_range_does_not_resolve_tip_alone() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    let tip = commit_file(root.path(), "a.txt", "a\n", "only");
    let repo = open_repo(root.path());
    let expanded =
        commits_to_replay(&repo, &[format!("{}..{}", tip.to_hex(), tip.to_hex())]).expect("expand");
    assert!(expanded.is_empty());
    let req = pick_req(vec![], ReplayOptions::default());
    let mut req_with_empty = req.clone();
    req_with_empty.commits = expanded;
    let out = start_replay_sequence(&repo, &req_with_empty, &mut NullProgress, &NeverCancel)
        .expect("start");
    assert!(matches!(
        out,
        SequenceOutcome::Completed {
            created,
            dropped
        } if created.is_empty() && dropped.is_empty()
    ));
}

#[test]
fn continued_commit_gets_new_change_id() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "f", "base\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    fs::write(root.path().join("f"), "topic\n").unwrap();
    git(root.path(), &["commit", "-qam", "topic-f"]);
    let pick = oid_from_rev(root.path(), "HEAD");
    git(root.path(), &["switch", "main"]);
    fs::write(root.path().join("f"), "main\n").unwrap();
    git(root.path(), &["commit", "-qam", "main"]);

    let repo = open_repo(root.path());
    start_replay_sequence(
        &repo,
        &pick_req(vec![pick], ReplayOptions::default()),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("pick");
    fs::write(root.path().join("f"), "merged\n").unwrap();
    git(root.path(), &["add", "f"]);
    let out = continue_replay_sequence(&repo, &replay_ident(), &mut NullProgress, &NeverCancel)
        .expect("continue");
    let created = match out {
        SequenceOutcome::Completed { created, .. } => created,
        _ => panic!("expected completed"),
    };
    let obj = repo.odb().read(&created[0].new_oid).expect("read");
    let commit = parse_commit(&obj.data).expect("parse");
    let src_obj = repo.odb().read(&pick).expect("src");
    let src = parse_commit(&src_obj.data).expect("src parse");
    assert_ne!(change_id(&commit), change_id(&src));
    git_fsck_strict(root.path());
}

fn commit_nested_tree(root: &Path) {
    for dir in ["src/a", "src/b", "docs", "tests/deep/er"] {
        for i in 0..3 {
            let path = root.join(dir).join(format!("f{i}.txt"));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, format!("{dir} {i}\n")).unwrap();
        }
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "nested base"]);
}

/// The replay keeps the cache-tree: subtrees it did not touch stay valid and agree with `HEAD`.
fn assert_untouched_cache_tree_kept(root: &Path, untouched_dir: &str) {
    let index = grit_lib::index::Index::load(&root.join(".git/index")).expect("load index");
    let cache_tree = index
        .cache_tree
        .as_ref()
        .expect("index written by the replay must keep the cache-tree extension");
    let mut node = cache_tree;
    for component in untouched_dir.split('/') {
        node = node
            .children
            .iter()
            .find(|child| child.name == component.as_bytes())
            .unwrap_or_else(|| panic!("cache-tree lost the node for {untouched_dir}"));
    }
    let expected = oid_from_rev(root, &format!("HEAD:{untouched_dir}"));
    assert_eq!(node.oid, Some(expected), "{untouched_dir} must stay valid");
    assert!(node.entry_count > 0);
    let head_tree = oid_from_rev(root, "HEAD^{tree}");
    assert_eq!(git(root, &["write-tree"]).trim(), head_tree.to_hex());
    assert_eq!(git(root, &["status", "--porcelain"]), "");
    git_fsck_strict(root);
}

#[test]
fn pick_and_revert_keep_valid_cache_tree() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_nested_tree(root.path());
    git(root.path(), &["switch", "-c", "topic"]);
    fs::write(root.path().join("src/a/f1.txt"), "topic change\n").unwrap();
    fs::write(root.path().join("tests/deep/er/new.txt"), "added\n").unwrap();
    git(root.path(), &["add", "-A"]);
    git(root.path(), &["commit", "-qm", "topic"]);
    let pick = oid_from_rev(root.path(), "HEAD");
    git(root.path(), &["switch", "main"]);
    commit_file(root.path(), "docs/f0.txt", "main change\n", "main");
    let main_tree = oid_from_rev(root.path(), "HEAD^{tree}");
    let index = grit_lib::index::Index::load(&root.path().join(".git/index")).expect("index");
    assert_eq!(
        index.cache_tree_root,
        Some(main_tree),
        "git seeds a cache-tree"
    );

    let repo = open_repo(root.path());
    let out = start_replay_sequence(
        &repo,
        &pick_req(vec![pick], ReplayOptions::default()),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("pick");
    let picked = match out {
        SequenceOutcome::Completed { created, .. } => created[0].new_oid,
        _ => panic!("expected completed pick"),
    };
    assert_untouched_cache_tree_kept(root.path(), "src/b");

    let out = start_replay_sequence(
        &repo,
        &revert_req(vec![picked], ReplayOptions::default()),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("revert");
    assert!(matches!(out, SequenceOutcome::Completed { .. }));
    assert_eq!(oid_from_rev(root.path(), "HEAD^{tree}"), main_tree);
    assert_untouched_cache_tree_kept(root.path(), "docs");
}

#[test]
fn pick_refuses_staged_or_unstaged_changes_without_state() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_nested_tree(root.path());
    git(root.path(), &["switch", "-c", "topic"]);
    commit_file(root.path(), "src/b/f2.txt", "topic\n", "topic");
    let pick = oid_from_rev(root.path(), "HEAD");
    git(root.path(), &["switch", "main"]);
    commit_file(root.path(), "docs/f1.txt", "main\n", "main");
    let head = oid_from_rev(root.path(), "HEAD");
    let repo = open_repo(root.path());

    fs::write(root.path().join("src/a/f0.txt"), "staged\n").unwrap();
    git(root.path(), &["add", "src/a/f0.txt"]);
    start_replay_sequence(
        &repo,
        &pick_req(vec![pick], ReplayOptions::default()),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect_err("staged change must refuse the pick");
    assert_no_replay_state(root.path());
    assert_eq!(oid_from_rev(root.path(), "HEAD"), head);
    git(root.path(), &["reset", "-q", "--hard"]);

    fs::write(root.path().join("tests/deep/er/f2.txt"), "unstaged\n").unwrap();
    start_replay_sequence(
        &repo,
        &pick_req(vec![pick], ReplayOptions::default()),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect_err("unstaged change must refuse the pick");
    assert_no_replay_state(root.path());
    assert_eq!(oid_from_rev(root.path(), "HEAD"), head);
    assert_eq!(
        fs::read_to_string(root.path().join("tests/deep/er/f2.txt")).unwrap(),
        "unstaged\n"
    );
}

fn stored_change_id(repo: &Repository, oid: ObjectId) -> Option<grit_lib::change_id::ChangeId> {
    let obj = repo.odb().read(&oid).expect("read");
    change_id(&parse_commit(&obj.data).expect("parse"))
}

#[test]
fn revert_continue_reports_subjects_and_change_ids_of_earlier_steps() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "shared.txt", "base\n", "base");
    let add_clean = commit_file(root.path(), "clean.txt", "clean\n", "add clean");
    let edit_shared = commit_file(root.path(), "shared.txt", "b\n", "edit shared");
    commit_file(root.path(), "shared.txt", "c\n", "edit shared again");

    let repo = open_repo(root.path());
    let out = start_replay_sequence(
        &repo,
        &revert_req(vec![add_clean, edit_shared], ReplayOptions::default()),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("revert");
    let SequenceOutcome::Conflicts {
        current,
        current_subject,
        created_so_far,
        ..
    } = out
    else {
        panic!("expected conflicts on the second revert, got {out:?}");
    };
    assert_eq!(current, edit_shared);
    assert_eq!(current_subject, "edit shared");
    assert_eq!(created_so_far.len(), 1);
    assert_eq!(created_so_far[0].subject, "Revert \"add clean\"");

    fs::write(root.path().join("shared.txt"), "resolved\n").unwrap();
    git(root.path(), &["add", "shared.txt"]);
    let SequenceOutcome::Completed { created, .. } =
        continue_replay_sequence(&repo, &replay_ident(), &mut NullProgress, &NeverCancel)
            .expect("continue")
    else {
        panic!("expected completed");
    };
    assert_eq!(created.len(), 2);
    assert_eq!(created[0].source, add_clean);
    assert_eq!(created[0].subject, "Revert \"add clean\"");
    assert_eq!(created[1].source, edit_shared);
    assert_eq!(created[1].subject, "Revert \"edit shared\"");
    for record in &created {
        let stored = stored_change_id(&repo, record.new_oid);
        assert!(stored.is_some(), "revert commits carry a change-id");
        assert_eq!(record.change_id, stored);
    }
    git_fsck_strict(root.path());
}

#[test]
fn empty_stop_and_prediction_report_current_subject() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "f", "a\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    git(
        root.path(),
        &["commit", "--allow-empty", "-qm", "nothing here"],
    );
    let empty = oid_from_rev(root.path(), "HEAD");
    git(root.path(), &["switch", "main"]);

    let repo = open_repo(root.path());
    let prediction =
        predict_replay_sequence(&repo, &pick_req(vec![empty], ReplayOptions::default()))
            .expect("predict");
    assert_eq!(
        prediction,
        SequencePrediction::Empty {
            commit: empty,
            subject: "nothing here".into(),
        }
    );

    let out = start_replay_sequence(
        &repo,
        &pick_req(vec![empty], ReplayOptions::default()),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("pick");
    let SequenceOutcome::Empty {
        current,
        current_subject,
        created_so_far,
        ..
    } = out
    else {
        panic!("expected an empty stop, got {out:?}");
    };
    assert_eq!(current, empty);
    assert_eq!(current_subject, "nothing here");
    assert!(created_so_far.is_empty());
    abort_replay_sequence(&repo).expect("abort");
}

#[test]
fn subjects_skip_leading_blank_message_lines_like_git() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "f", "base\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    fs::write(root.path().join("f"), "topic\n").unwrap();
    git(root.path(), &["add", "f"]);
    let tree = git(root.path(), &["write-tree"]).trim().to_owned();
    let mut commit_tree = Command::new("git")
        .current_dir(root.path())
        .args(["commit-tree", &tree, "-p", "HEAD"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("git commit-tree");
    use std::io::Write as _;
    commit_tree
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"\n\n  blank-led subject\n\nbody\n")
        .expect("write message");
    let out = commit_tree.wait_with_output().expect("commit-tree output");
    assert!(out.status.success());
    let pick = ObjectId::from_hex(String::from_utf8_lossy(&out.stdout).trim()).expect("oid");
    git(root.path(), &["reset", "-q", "--hard", &pick.to_hex()]);
    assert_eq!(
        git(root.path(), &["log", "-1", "--format=%s", &pick.to_hex()]).trim(),
        "blank-led subject"
    );
    git(root.path(), &["switch", "main"]);
    fs::write(root.path().join("f"), "main\n").unwrap();
    git(root.path(), &["commit", "-qam", "main"]);

    let repo = open_repo(root.path());
    let req = pick_req(vec![pick], ReplayOptions::default());
    match predict_replay_sequence(&repo, &req).expect("predict") {
        SequencePrediction::Conflicts { subject, .. } => assert_eq!(subject, "blank-led subject"),
        other => panic!("expected predicted conflicts, got {other:?}"),
    }
    match start_replay_sequence(&repo, &req, &mut NullProgress, &NeverCancel).expect("pick") {
        SequenceOutcome::Conflicts {
            current_subject, ..
        } => assert_eq!(current_subject, "blank-led subject"),
        other => panic!("expected conflicts, got {other:?}"),
    }
    let todo = fs::read_to_string(root.path().join(".git/sequencer/todo")).expect("todo");
    assert!(
        todo.trim_end().ends_with("blank-led subject"),
        "todo names the first non-blank line, like git: {todo:?}"
    );

    fs::write(root.path().join("f"), "resolved\n").unwrap();
    git(root.path(), &["add", "f"]);
    let SequenceOutcome::Completed { created, .. } =
        continue_replay_sequence(&repo, &replay_ident(), &mut NullProgress, &NeverCancel)
            .expect("continue")
    else {
        panic!("expected completed");
    };
    assert_eq!(created.len(), 1);
    assert_eq!(
        created[0].subject,
        git(root.path(), &["log", "-1", "--format=%s"]).trim()
    );
    assert_eq!(created[0].subject, "blank-led subject");
    git_fsck_strict(root.path());
}
