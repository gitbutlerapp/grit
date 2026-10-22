//! Stop/resume rebase session tests.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::refs::resolve_ref;
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    abort_rebase, continue_rebase, skip_rebase, start_rebase, EmptyPolicy, RebaseOntoOptions,
    RebaseOutcome, RebaseRequest, RebaseSessionError, RefScope, RewriteOptions, StartRebaseOptions,
};
use grit_lib::state::resolve_head;
use support::{assert_git_fsck_strict, git, git_rev_parse_verify, open_grit_repo, HistoryRepo};

const IDENT: &str = "Test User <t@example.com> 1700000000 +0000";
const COMMITTER: &str = "Refs Harness Author <refs-harness@example.com> 1700001000 +0000";

fn open_grit(wt: &std::path::Path) -> Repository {
    open_grit_repo(wt)
}

fn git_oid(wt: &std::path::Path, spec: &str) -> ObjectId {
    git_rev_parse_verify(wt, spec).expect(spec)
}

fn rebase_opts() -> RebaseOntoOptions {
    RebaseOntoOptions {
        rewrite: RewriteOptions {
            scope: RefScope::Head,
            empty: EmptyPolicy::Drop,
            committer: COMMITTER.into(),
            preserve_committer: false,
            sign: false,
        },
        skip_applied: false,
        flatten_leaf_merges: false,
    }
}

fn start_opts(upstream: ObjectId, onto: Option<ObjectId>, tip: ObjectId) -> StartRebaseOptions {
    StartRebaseOptions {
        request: RebaseRequest {
            upstream,
            onto,
            branch_tip: tip,
        },
        rebase: rebase_opts(),
        apply: grit_lib::revwalk::history::ApplyOptions {
            identity: IDENT.into(),
            reflog_message: "rebase".into(),
        },
    }
}

fn commit_file(wt: &std::path::Path, path: &str, contents: &str, msg: &str) {
    std::fs::write(wt.join(path), contents).expect("write");
    git(wt, &["add", path]);
    git(wt, &["commit", "-qm", msg]);
}

#[test]
fn rebase_session_clean_completes_and_matches_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "base.txt", "base\n", "base");
    let upstream = git_oid(wt, "HEAD");
    git(wt, &["switch", "-c", "topic"]);
    commit_file(wt, "topic.txt", "topic\n", "topic");
    let tip = git_oid(wt, "HEAD");
    git(wt, &["switch", "main"]);
    commit_file(wt, "other.txt", "other\n", "other");
    let onto = git_oid(wt, "HEAD");

    let git_wt = tempfile::tempdir().expect("git copy");
    std::process::Command::new("cp")
        .args([
            "-a",
            &format!("{}/.", wt.display()),
            &git_wt.path().to_string_lossy(),
        ])
        .status()
        .unwrap();
    git(
        git_wt.path(),
        &[
            "rebase",
            "--onto",
            &onto.to_hex(),
            &upstream.to_hex(),
            "topic",
        ],
    );

    let grit = open_grit(wt);
    git(wt, &["switch", "topic"]);
    let out = start_rebase(&grit, &start_opts(upstream, Some(onto), tip)).expect("start");
    match out {
        RebaseOutcome::Done { .. } => {}
        other => panic!("expected done, got {other:?}"),
    }
    assert_eq!(
        git(wt, &["rev-parse", "topic^{tree}"]).trim(),
        git(git_wt.path(), &["rev-parse", "topic^{tree}"]).trim(),
        "topic tree should match git rebase"
    );
    assert_git_fsck_strict(wt);
}

#[test]
fn rebase_session_conflict_stop_continue_and_abort() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "f.txt", "base\n", "base");
    let upstream = git_oid(wt, "HEAD");
    git(wt, &["switch", "-c", "topic"]);
    commit_file(wt, "f.txt", "topic change\n", "topic commit");
    let tip = git_oid(wt, "HEAD");
    git(wt, &["switch", "main"]);
    commit_file(wt, "f.txt", "main change\n", "main commit");
    let onto = git_oid(wt, "HEAD");

    let grit = open_grit(wt);
    git(wt, &["switch", "topic"]);
    let orig_tip = git_oid(wt, "topic");
    let out = start_rebase(&grit, &start_opts(upstream, Some(onto), tip)).expect("start conflict");
    let stop = match out {
        RebaseOutcome::Stopped(s) => s,
        other => panic!("expected stopped, got {other:?}"),
    };
    assert_eq!(stop.remaining, vec![tip]);
    assert!(stop.conflicts.iter().any(|c| c.path == "f.txt"));
    assert_eq!(
        resolve_ref(grit.git_dir(), "refs/heads/topic").unwrap(),
        orig_tip
    );
    assert!(grit.git_dir().join("rebase-merge/grit-plan").is_file());
    assert!(grit
        .git_dir()
        .join("rebase-merge/git-rebase-todo")
        .is_file());

    std::fs::write(wt.join("f.txt"), "resolved\n").expect("resolve");
    git(wt, &["add", "f.txt"]);
    let done = continue_rebase(&grit, &start_opts(upstream, Some(onto), tip)).expect("continue");
    match done {
        RebaseOutcome::Done { .. } => {}
        other => panic!("expected done after continue, got {other:?}"),
    }
    assert!(!grit.git_dir().join("rebase-merge").exists());
    assert_ne!(git_oid(wt, "topic"), orig_tip);
    assert_git_fsck_strict(wt);

    // Abort path on a fresh conflict stop.
    let repo2 = HistoryRepo::init();
    let wt2 = repo2.worktree();
    commit_file(wt2, "f.txt", "base\n", "base");
    let upstream2 = git_oid(wt2, "HEAD");
    git(wt2, &["switch", "-c", "topic"]);
    commit_file(wt2, "f.txt", "topic change\n", "topic commit");
    let tip2 = git_oid(wt2, "HEAD");
    git(wt2, &["switch", "main"]);
    commit_file(wt2, "f.txt", "main change\n", "main commit");
    let onto2 = git_oid(wt2, "HEAD");
    let grit2 = open_grit(wt2);
    git(wt2, &["switch", "topic"]);
    let orig2 = git_oid(wt2, "topic");
    let _ = start_rebase(&grit2, &start_opts(upstream2, Some(onto2), tip2)).expect("stop");
    abort_rebase(&grit2, IDENT).expect("abort");
    assert_eq!(git_oid(wt2, "topic"), orig2);
    assert!(resolve_head(grit2.git_dir()).unwrap().branch_name() == Some("topic"));
    assert_git_fsck_strict(wt2);
}

#[test]
fn rebase_session_skip_drops_current_commit() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "f.txt", "base\n", "base");
    let upstream = git_oid(wt, "HEAD");
    git(wt, &["switch", "-c", "topic"]);
    commit_file(wt, "f.txt", "topic change\n", "topic commit");
    let tip = git_oid(wt, "HEAD");
    git(wt, &["switch", "main"]);
    commit_file(wt, "f.txt", "main change\n", "main commit");
    let onto = git_oid(wt, "HEAD");

    let grit = open_grit(wt);
    git(wt, &["switch", "topic"]);
    let _ = start_rebase(&grit, &start_opts(upstream, Some(onto), tip)).expect("stop");
    let out = skip_rebase(&grit, &start_opts(upstream, Some(onto), tip)).expect("skip");
    match out {
        RebaseOutcome::Done { .. } => {}
        other => panic!("expected done after skip, got {other:?}"),
    }
    assert_git_fsck_strict(wt);
}

#[test]
fn continue_rebase_refuses_unmerged_index() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "f.txt", "base\n", "base");
    let upstream = git_oid(wt, "HEAD");
    git(wt, &["switch", "-c", "topic"]);
    commit_file(wt, "f.txt", "topic change\n", "topic commit");
    let tip = git_oid(wt, "HEAD");
    git(wt, &["switch", "main"]);
    commit_file(wt, "f.txt", "main change\n", "main commit");
    let onto = git_oid(wt, "HEAD");

    let grit = open_grit(wt);
    git(wt, &["switch", "topic"]);
    let _ = start_rebase(&grit, &start_opts(upstream, Some(onto), tip)).expect("stop");
    let err = continue_rebase(&grit, &start_opts(upstream, Some(onto), tip)).unwrap_err();
    assert!(matches!(err, RebaseSessionError::UnmergedPaths));
}

#[test]
fn start_rebase_refuses_when_session_active() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "f.txt", "base\n", "base");
    let upstream = git_oid(wt, "HEAD");
    git(wt, &["switch", "-c", "topic"]);
    commit_file(wt, "f.txt", "topic change\n", "topic commit");
    let tip = git_oid(wt, "HEAD");
    git(wt, &["switch", "main"]);
    commit_file(wt, "f.txt", "main change\n", "main commit");
    let onto = git_oid(wt, "HEAD");

    let grit = open_grit(wt);
    git(wt, &["switch", "topic"]);
    let opts = start_opts(upstream, Some(onto), tip);
    let _ = start_rebase(&grit, &opts).expect("first start");
    let err = start_rebase(&grit, &opts).unwrap_err();
    assert!(matches!(err, RebaseSessionError::RebaseAlreadyInProgress));
}
