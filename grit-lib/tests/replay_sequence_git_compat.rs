//! Git on-disk compatibility tests for [`grit_lib::porcelain::replay_sequence`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::change_id::change_id;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::porcelain::replay::ReplayDirection;
use grit_lib::porcelain::replay_sequence::{
    abort_replay_sequence, continue_replay_sequence, skip_replay_step, start_replay_sequence,
    ReplayEmptyPolicy, ReplayOptions, ReplaySequenceRequest, SequenceOutcome,
};
use grit_lib::repo::Repository;
const IDENT: &str = "Test User <t@example.com> 1700000000 +0000";

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
        committer: IDENT.to_owned(),
        options: opts,
    }
}

fn revert_req(commits: Vec<ObjectId>, opts: ReplayOptions) -> ReplaySequenceRequest {
    ReplaySequenceRequest {
        commits,
        direction: ReplayDirection::Revert,
        committer: IDENT.to_owned(),
        options: opts,
    }
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
    let out = start_replay_sequence(&repo, &pick_req(vec![c1, c2], ReplayOptions::default()))
        .expect("pick");
    let created = match out {
        SequenceOutcome::Completed { created, .. } => created,
        other => panic!("expected completed, got {other:?}"),
    };
    assert_eq!(created.len(), 2);
    assert_eq!(created[0].source, c1);
    assert_eq!(created[1].source, c2);
    assert_ne!(created[0].new_oid, c1);
    assert_ne!(created[1].new_oid, c2);
    assert!(created[0].change_id.is_some());
    assert!(created[1].change_id.is_some());
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
    let out = start_replay_sequence(&repo, &pick_req(vec![pick], ReplayOptions::default()))
        .expect("pick");
    assert!(matches!(out, SequenceOutcome::Conflicts { .. }));
    assert!(root.path().join(".git/CHERRY_PICK_HEAD").is_file());
    assert!(root.path().join(".git/MERGE_MSG").is_file());
    let wt = fs::read_to_string(root.path().join("f")).unwrap();
    assert!(wt.contains("<<<<<<<"));
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
    start_replay_sequence(&repo, &pick_req(vec![pick], ReplayOptions::default())).expect("pick");
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
    start_replay_sequence(&repo, &pick_req(vec![pick], ReplayOptions::default())).expect("pick");
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
    let out = continue_replay_sequence(&repo, IDENT).expect("continue");
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
    let out = start_replay_sequence(&repo, &revert_req(vec![revert], ReplayOptions::default()))
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
    start_replay_sequence(&repo, &revert_req(vec![rev], ReplayOptions::default()))
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
    start_replay_sequence(&repo, &pick_req(vec![c1, c2], ReplayOptions::default())).expect("pick");
    let out = skip_replay_step(&repo, IDENT).expect("skip");
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
    start_replay_sequence(&repo, &pick_req(vec![pick], ReplayOptions::default())).expect("pick");
    abort_replay_sequence(&repo).expect("abort");
    assert_eq!(oid_from_rev(root.path(), "HEAD"), before);
    assert_eq!(fs::read_to_string(root.path().join("f")).unwrap(), "main\n");
    git_fsck_strict(root.path());
}

#[test]
fn abort_refuses_when_head_moved() {
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
    start_replay_sequence(&repo, &pick_req(vec![pick], ReplayOptions::default())).expect("pick");
    git(root.path(), &["update-ref", "refs/heads/main", "HEAD~1"]);
    let err = abort_replay_sequence(&repo).unwrap_err();
    assert!(matches!(err, grit_lib::error::Error::ReplayAbortHeadMoved));
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
    start_replay_sequence(&repo, &pick_req(vec![pick], ReplayOptions::default())).expect("pick");
    let err = continue_replay_sequence(&repo, IDENT).unwrap_err();
    assert!(matches!(
        err,
        grit_lib::error::Error::ReplayContinueUnmerged
    ));
}

#[test]
fn revert_mainline_matches_git_revert_m1_tree_and_message() {
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
    let out = start_replay_sequence(&repo, &revert_req(vec![merge], opts)).expect("revert");
    assert!(matches!(out, SequenceOutcome::Completed { .. }));
    assert_eq!(head_tree_hex(root.path()), head_tree_hex(git_wt.path()));
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
    let err =
        start_replay_sequence(&repo, &pick_req(vec![merge], ReplayOptions::default())).unwrap_err();
    assert!(matches!(err, grit_lib::error::Error::MergeCommit { .. }));
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
    let out = start_replay_sequence(&repo, &pick_req(vec![empty], stop_opts)).expect("stop");
    assert!(matches!(out, SequenceOutcome::Empty { .. }));
    abort_replay_sequence(&repo).expect("abort");

    let mut drop_opts = ReplayOptions::default();
    drop_opts.empty = ReplayEmptyPolicy::Drop;
    let out = start_replay_sequence(&repo, &pick_req(vec![empty], drop_opts)).expect("drop");
    assert!(matches!(out, SequenceOutcome::Completed { .. }));

    git(root.path(), &["reset", "--hard", "HEAD"]);
    let mut keep_opts = ReplayOptions::default();
    keep_opts.empty = ReplayEmptyPolicy::Keep;
    let out = start_replay_sequence(&repo, &pick_req(vec![empty], keep_opts)).expect("keep");
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
    start_replay_sequence(&repo, &pick_req(vec![pick], opts)).expect("pick");
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
    commit_file(root.path(), "extra.txt", "extra\n", "t-extra");
    fs::write(root.path().join("shared.txt"), "topic\n").unwrap();
    git(root.path(), &["add", "shared.txt"]);
    git(root.path(), &["commit", "-qm", "t2"]);
    let c1 = oid_from_rev(root.path(), "topic~2");
    let c2 = oid_from_rev(root.path(), "topic~1");
    let c3 = oid_from_rev(root.path(), "topic");
    git(root.path(), &["switch", "main"]);
    fs::write(root.path().join("shared.txt"), "main\n").unwrap();
    git(root.path(), &["add", "shared.txt"]);
    git(root.path(), &["commit", "-qm", "main"]);
    let start = oid_from_rev(root.path(), "HEAD");
    let start_tree = head_tree_hex(root.path());
    fs::write(root.path().join("shared.txt"), "main\n").unwrap();

    let repo = open_repo(root.path());
    let out = start_replay_sequence(&repo, &pick_req(vec![c1, c2, c3], ReplayOptions::default()))
        .expect("pick");
    assert!(matches!(out, SequenceOutcome::Conflicts { .. }));
    assert!(root.path().join(".git/sequencer/todo").is_file());
    assert!(root.path().join("only-topic.txt").exists());
    abort_replay_sequence(&repo).expect("abort");
    assert_eq!(oid_from_rev(root.path(), "HEAD"), start);
    assert_eq!(head_tree_hex(root.path()), start_tree);
    assert!(!root.path().join("only-topic.txt").exists());
    assert!(!root.path().join(".git/CHERRY_PICK_HEAD").exists());
    let shared = fs::read_to_string(root.path().join("shared.txt")).unwrap();
    assert_eq!(shared, "main\n");
    git_fsck_strict(root.path());
}

#[test]
fn no_commit_empty_stop_compares_accumulated_index() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "f.txt", "base\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    commit_file(root.path(), "a.txt", "first\n", "a");
    fs::write(root.path().join("f.txt"), "first\n").unwrap();
    git(root.path(), &["add", "f.txt"]);
    git(root.path(), &["commit", "-qm", "b"]);
    let c1 = oid_from_rev(root.path(), "topic~1");
    let c2 = oid_from_rev(root.path(), "topic");
    git(root.path(), &["switch", "main"]);
    let start = oid_from_rev(root.path(), "HEAD");

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
    git(git_wt.path(), &["cherry-pick", "-n", &c1.to_hex()]);
    git(git_wt.path(), &["cherry-pick", "-n", &c2.to_hex()]);

    let repo = open_repo(root.path());
    let mut opts = ReplayOptions::default();
    opts.no_commit = true;
    opts.empty = ReplayEmptyPolicy::Stop;
    let out = start_replay_sequence(&repo, &pick_req(vec![c1, c2], opts)).expect("pick");
    assert!(matches!(out, SequenceOutcome::Completed { .. }));
    assert_eq!(oid_from_rev(root.path(), "HEAD"), start);
    assert_eq!(
        git(root.path(), &["show", ":a.txt"]),
        git(git_wt.path(), &["show", ":a.txt"])
    );
    assert_eq!(
        git(root.path(), &["show", ":f.txt"]),
        git(git_wt.path(), &["show", ":f.txt"])
    );
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
    let out = start_replay_sequence(&repo, &pick_req(vec![c1, c2], opts)).expect("pick");
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
    start_replay_sequence(&repo, &pick_req(vec![pick], opts)).expect("pick");
    fs::write(root.path().join("f"), "resolved\n").unwrap();
    git(root.path(), &["add", "f"]);
    git(root.path(), &["cherry-pick", "--continue", "--no-gpg-sign"]);
    let body = git(root.path(), &["log", "-1", "--format=%B"]);
    assert!(body.contains("(cherry picked from commit"));
    assert_eq!(body.matches("(cherry picked from commit").count(), 1);
    git_fsck_strict(root.path());
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
    start_replay_sequence(&repo, &pick_req(vec![pick], ReplayOptions::default())).expect("pick");
    fs::write(root.path().join("f"), "merged\n").unwrap();
    git(root.path(), &["add", "f"]);
    let out = continue_replay_sequence(&repo, IDENT).expect("continue");
    let created = match out {
        SequenceOutcome::Completed { created, .. } => created,
        _ => panic!("expected completed"),
    };
    assert_eq!(created.len(), 1);
    let record = &created[0];
    assert_eq!(record.source, pick);
    let obj = repo.odb().read(&record.new_oid).expect("read");
    let commit = parse_commit(&obj.data).expect("parse");
    let src_obj = repo.odb().read(&pick).expect("src");
    let src = parse_commit(&src_obj.data).expect("src parse");
    assert_ne!(change_id(&commit), change_id(&src));
    assert_eq!(record.change_id, change_id(&commit));
    git_fsck_strict(root.path());
}

#[test]
fn completed_created_records_after_continue() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "f", "base\n", "base");
    git(root.path(), &["switch", "-c", "topic"]);
    fs::write(root.path().join("f"), "topic\n").unwrap();
    git(root.path(), &["commit", "-qam", "topic-f"]);
    let c1 = oid_from_rev(root.path(), "HEAD");
    git(root.path(), &["switch", "main"]);
    fs::write(root.path().join("f"), "main\n").unwrap();
    git(root.path(), &["commit", "-qam", "main"]);

    let repo = open_repo(root.path());
    let stop =
        start_replay_sequence(&repo, &pick_req(vec![c1], ReplayOptions::default())).expect("pick");
    let (current, mut created_so_far) = match stop {
        SequenceOutcome::Conflicts {
            current,
            created_so_far,
            ..
        } => (current, created_so_far),
        other => panic!("expected conflicts, got {other:?}"),
    };
    assert_eq!(current, c1);
    assert!(created_so_far.is_empty());

    fs::write(root.path().join("f"), "resolved\n").unwrap();
    git(root.path(), &["add", "f"]);
    let out = continue_replay_sequence(&repo, IDENT).expect("continue");
    let created = match out {
        SequenceOutcome::Completed { created, .. } => created,
        other => panic!("expected completed, got {other:?}"),
    };
    assert_eq!(created.len(), 1);
    let record = &created[0];
    assert_eq!(record.source, c1);
    assert_ne!(record.new_oid, c1);
    assert!(record.change_id.is_some());
    created_so_far.extend(created);
    assert_eq!(created_so_far.len(), 1);
    git_fsck_strict(root.path());
}
