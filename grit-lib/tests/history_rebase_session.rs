//! Stop/resume rebase session tests.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use grit_lib::objects::ObjectId;
use grit_lib::refs::resolve_ref;
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    abort_rebase, continue_rebase, skip_rebase, start_rebase, EmptyPolicy, RebaseOntoOptions,
    RebaseOutcome, RebaseRequest, RebaseSessionError, RefScope, RewriteOptions, StartRebaseOptions,
};
use grit_lib::state::resolve_head;
use grit_lib::state::{detect_in_progress, InProgressOperation};
use support::{
    assert_git_fsck_strict, build_commit_dag, git, git_rev_parse_verify, open_grit_repo,
    HistoryRepo,
};

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
    assert_eq!(git(wt, &["show", "HEAD:f.txt"]), "resolved\n");
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
fn skip_drops_conflicting_commit() {
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
fn continue_refuses_with_unmerged_paths() {
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

fn conflict_rebase_fixture() -> (HistoryRepo, ObjectId, ObjectId, ObjectId, ObjectId) {
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
    let orig_tip = git_oid(wt, "topic");
    (repo, upstream, onto, tip, orig_tip)
}

#[test]
fn stop_writes_markers_and_detaches_head() {
    let (repo, upstream, onto, tip, orig_tip) = conflict_rebase_fixture();
    let wt = repo.worktree();
    let grit = open_grit(wt);
    git(wt, &["switch", "topic"]);
    let out = start_rebase(&grit, &start_opts(upstream, Some(onto), tip)).expect("start");
    assert!(matches!(out, RebaseOutcome::Stopped(_)));
    let body = std::fs::read_to_string(wt.join("f.txt")).expect("f.txt");
    assert!(body.contains("<<<<<<<"));
    assert_eq!(
        resolve_ref(grit.git_dir(), "refs/heads/topic").unwrap(),
        orig_tip
    );
    assert!(matches!(
        resolve_head(grit.git_dir()).expect("head"),
        grit_lib::state::HeadState::Detached { .. }
    ));
    assert_git_fsck_strict(wt);
}

#[test]
fn branch_refs_unchanged_until_final_step() {
    let (repo, upstream, onto, tip, orig_tip) = conflict_rebase_fixture();
    let wt = repo.worktree();
    let grit = open_grit(wt);
    git(wt, &["switch", "topic"]);
    let _ = start_rebase(&grit, &start_opts(upstream, Some(onto), tip)).expect("stop");
    assert_eq!(
        resolve_ref(grit.git_dir(), "refs/heads/topic").unwrap(),
        orig_tip
    );
    std::fs::write(wt.join("f.txt"), "resolved\n").expect("resolve");
    git(wt, &["add", "f.txt"]);
    let _ = continue_rebase(&grit, &start_opts(upstream, Some(onto), tip)).expect("continue");
    assert_ne!(
        resolve_ref(grit.git_dir(), "refs/heads/topic").unwrap(),
        orig_tip
    );
    assert_git_fsck_strict(wt);
}

#[test]
fn git_status_reports_rebase_in_progress_after_stop() {
    let (repo, upstream, onto, tip, _) = conflict_rebase_fixture();
    let wt = repo.worktree();
    let grit = open_grit(wt);
    git(wt, &["switch", "topic"]);
    let _ = start_rebase(&grit, &start_opts(upstream, Some(onto), tip)).expect("stop");
    let status = git(wt, &["status"]);
    assert!(
        status.contains("rebase in progress"),
        "git status should report rebase: {status}"
    );
    assert!(
        detect_in_progress(grit.git_dir()).contains(&InProgressOperation::Rebase)
            || detect_in_progress(grit.git_dir()).contains(&InProgressOperation::RebaseInteractive)
    );
    assert_git_fsck_strict(wt);
}

#[test]
fn git_rebase_abort_restores_after_grit_stop() {
    let (repo, upstream, onto, tip, orig_tip) = conflict_rebase_fixture();
    let wt = repo.worktree();
    let grit = open_grit(wt);
    git(wt, &["switch", "topic"]);
    let _ = start_rebase(&grit, &start_opts(upstream, Some(onto), tip)).expect("stop");
    git(wt, &["rebase", "--abort"]);
    assert_eq!(git_oid(wt, "topic"), orig_tip);
    assert!(!grit.git_dir().join("rebase-merge").exists());
    assert_git_fsck_strict(wt);
}

fn finish_conflict_rebase_with_git(wt: &std::path::Path, upstream: ObjectId, onto: ObjectId) {
    let out = std::process::Command::new("git")
        .current_dir(wt)
        .args([
            "rebase",
            "--onto",
            &onto.to_hex(),
            &upstream.to_hex(),
            "topic",
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git rebase");
    assert!(!out.status.success(), "git rebase should stop on conflict");
    std::fs::write(wt.join("f.txt"), "resolved\n").expect("resolve");
    git(wt, &["add", "f.txt"]);
    let cont = std::process::Command::new("git")
        .current_dir(wt)
        .args(["rebase", "--continue"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_EDITOR", ":")
        .env("EDITOR", ":")
        .output()
        .expect("git rebase --continue");
    assert!(
        cont.status.success(),
        "git rebase --continue: {}",
        String::from_utf8_lossy(&cont.stderr)
    );
}

fn finish_conflict_rebase_with_grit(
    wt: &std::path::Path,
    grit: &Repository,
    upstream: ObjectId,
    onto: ObjectId,
    tip: ObjectId,
) {
    let _ = start_rebase(grit, &start_opts(upstream, Some(onto), tip)).expect("grit stop");
    std::fs::write(wt.join("f.txt"), "resolved\n").expect("resolve");
    git(wt, &["add", "f.txt"]);
    // Re-open so `continue_rebase` reads the index `git add` wrote (no stale handle).
    let grit = open_grit(wt);
    let _ = continue_rebase(&grit, &start_opts(upstream, Some(onto), tip)).expect("grit continue");
}

#[test]
fn continue_after_resolution_matches_git_rebase_continue() {
    let (repo_g, upstream_g, onto_g, tip_g, _) = conflict_rebase_fixture();
    let wt_g = repo_g.worktree();
    git(wt_g, &["switch", "topic"]);
    finish_conflict_rebase_with_git(wt_g, upstream_g, onto_g);

    let (repo_r, upstream_r, onto_r, tip_r, _) = conflict_rebase_fixture();
    let wt_r = repo_r.worktree();
    git(wt_r, &["switch", "topic"]);
    let grit = open_grit(wt_r);
    finish_conflict_rebase_with_grit(wt_r, &grit, upstream_r, onto_r, tip_r);

    assert_eq!(
        git(wt_r, &["show", "HEAD:f.txt"]),
        git(wt_g, &["show", "HEAD:f.txt"])
    );
    assert_eq!(
        git(wt_r, &["ls-tree", "-r", "HEAD"]),
        git(wt_g, &["ls-tree", "-r", "HEAD"]),
        "tree entries should match"
    );
    assert_eq!(
        git(wt_r, &["show", "-s", "--format=%P", "HEAD"]).trim(),
        onto_r.to_hex()
    );
    assert_eq!(
        git(wt_g, &["show", "-s", "--format=%P", "HEAD"]).trim(),
        onto_g.to_hex()
    );
    assert_eq!(
        git(wt_r, &["show", "-s", "--format=%s", "HEAD"]).trim(),
        git(wt_g, &["show", "-s", "--format=%s", "HEAD"]).trim()
    );
    assert_git_fsck_strict(wt_g);
    assert_git_fsck_strict(wt_r);
}

#[test]
fn stacked_branches_move_atomically_after_continue() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(
        wt,
        &[("u", "r"), ("a", "u"), ("b", "a"), ("c", "b"), ("d", "b")],
        &[("c", "refs/heads/main"), ("d", "refs/heads/stacked")],
    );
    let onto = {
        commit_file(wt, "base.txt", "base\n", "onto");
        git_oid(wt, "HEAD")
    };
    let grit = open_grit(wt);
    git(wt, &["switch", "main"]);
    let mut opts = rebase_opts();
    opts.rewrite.scope = RefScope::Branches;
    let start = StartRebaseOptions {
        request: RebaseRequest {
            upstream: oids["u"],
            onto: Some(onto),
            branch_tip: oids["c"],
        },
        rebase: opts,
        apply: grit_lib::revwalk::history::ApplyOptions {
            identity: IDENT.into(),
            reflog_message: "rebase".into(),
        },
    };
    let main_before = resolve_ref(grit.git_dir(), "refs/heads/main").unwrap();
    let stacked_before = resolve_ref(grit.git_dir(), "refs/heads/stacked").unwrap();
    match start_rebase(&grit, &start).expect("start") {
        RebaseOutcome::Done { .. } => {}
        other => panic!("expected clean rebase, got {other:?}"),
    }
    let main_after = resolve_ref(grit.git_dir(), "refs/heads/main").unwrap();
    let stacked_after = resolve_ref(grit.git_dir(), "refs/heads/stacked").unwrap();
    assert_ne!(main_before, main_after);
    assert_ne!(stacked_before, stacked_after);
    assert_git_fsck_strict(wt);
}

#[test]
fn abort_restores_branch_index_worktree_exactly() {
    let (repo, upstream, onto, tip, orig_tip) = conflict_rebase_fixture();
    let wt = repo.worktree();
    let grit = open_grit(wt);
    git(wt, &["switch", "topic"]);
    let _ = start_rebase(&grit, &start_opts(upstream, Some(onto), tip)).expect("stop");
    abort_rebase(&grit, IDENT).expect("abort");
    assert_eq!(git_oid(wt, "topic"), orig_tip);
    assert_eq!(
        git(wt, &["rev-parse", "HEAD^{tree}"]),
        git(wt, &["rev-parse", &format!("{orig_tip}^{{tree}}")])
    );
    assert_git_fsck_strict(wt);
}
