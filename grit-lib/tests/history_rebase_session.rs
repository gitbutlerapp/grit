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
use grit_lib::state::{detect_in_progress, HeadState, InProgressOperation};
use support::{
    assert_git_fsck_strict, build_commit_dag, git, git_merge_base_is_ancestor,
    git_rev_parse_verify, open_grit_repo, HistoryRepo,
};

const IDENT: &str = "Test User <t@example.com> 1700000000 +0000";

fn test_ident() -> grit_lib::ident::Signature {
    IDENT.parse().expect("IDENT")
}
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
            committer: COMMITTER.parse().expect("signature"),
            preserve_committer: false,
            sign: false,
        },
        skip_applied: false,
        flatten_leaf_merges: false,
        favor: grit_lib::diff::merge_file::MergeFavor::default(),
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
            identity: test_ident(),
            reflog_message: "rebase".into(),
            record_operation: None,
            hooks: grit_lib::PostOperationHooks::Run,
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
        RebaseOutcome::Done {
            applied,
            head_change,
            ..
        } => {
            let after = git_oid(wt, "HEAD");
            assert_ne!(tip, after);
            assert_eq!(head_change, Some((tip, after)));
            assert_eq!(applied.head, after);
        }
        other => panic!("expected done, got {other:?}"),
    }
    assert_eq!(
        git(wt, &["rev-parse", "topic^{tree}"]).trim(),
        git(git_wt.path(), &["rev-parse", "topic^{tree}"]).trim(),
        "topic tree should match git rebase"
    );
    let after = git_oid(wt, "HEAD");
    match start_rebase(&grit, &start_opts(onto, None, after)).expect("up to date") {
        RebaseOutcome::Done {
            applied,
            head_change,
            ..
        } => {
            assert_eq!(head_change, None);
            assert_eq!(applied.head, after);
        }
        other => panic!("expected up to date, got {other:?}"),
    }
    assert_eq!(git_oid(wt, "HEAD"), after);
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
    let before = git_oid(wt, "HEAD");
    let done = continue_rebase(&grit, &start_opts(upstream, Some(onto), tip)).expect("continue");
    match done {
        RebaseOutcome::Done {
            applied,
            head_change,
            plan,
        } => {
            let after = git_oid(wt, "HEAD");
            assert_eq!(before, onto);
            assert_ne!(before, after);
            assert_eq!(head_change, Some((before, after)));
            assert_eq!(applied.head, after);
            assert_eq!(plan.head.old_commit, Some(before));
            assert_eq!(plan.head.head_ref, None);
            assert_eq!(plan.head_change(), None);
        }
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
    abort_rebase(&grit2, &test_ident()).expect("abort");
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
    let before = git_oid(wt, "HEAD");
    let out = skip_rebase(&grit, &start_opts(upstream, Some(onto), tip)).expect("skip");
    match out {
        RebaseOutcome::Done {
            applied,
            head_change,
            ..
        } => {
            assert_eq!(head_change, None);
            assert_eq!(applied.head, before);
        }
        other => panic!("expected done after skip, got {other:?}"),
    }
    assert_eq!(git_oid(wt, "HEAD"), before);
    assert_eq!(git_oid(wt, "topic"), onto);
    assert_eq!(
        resolve_head(grit.git_dir()).unwrap().branch_name(),
        Some("topic")
    );
    assert_eq!(git(wt, &["show", "HEAD:f.txt"]), "main change\n");
    assert!(git(wt, &["status", "--porcelain"]).trim().is_empty());
    assert!(!grit.git_dir().join("rebase-merge").exists());
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
        detect_in_progress(grit.git_dir()).contains(&InProgressOperation::Rebase),
        "expected non-interactive rebase, got {:?}",
        detect_in_progress(grit.git_dir())
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
    let (repo_g, upstream_g, onto_g, _tip_g, _) = conflict_rebase_fixture();
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
fn conflict_stop_detaches_head_at_onto_not_upstream() {
    let (repo, upstream, onto, tip, _) = conflict_rebase_fixture();
    let wt = repo.worktree();
    let grit = open_grit(wt);
    git(wt, &["switch", "topic"]);
    let _ = start_rebase(&grit, &start_opts(upstream, Some(onto), tip)).expect("stop");
    let detached = match resolve_head(grit.git_dir()).expect("head") {
        HeadState::Detached { oid } => oid,
        other => panic!("expected detached HEAD during stop, got {other:?}"),
    };
    assert_eq!(
        detached, onto,
        "first replayed commit should stop with HEAD on onto, not upstream"
    );
    assert_ne!(detached, upstream);
    assert_git_fsck_strict(wt);
}

#[test]
fn continue_leaves_index_and_worktree_at_final_tip() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "f.txt", "base\n", "base");
    let upstream = git_oid(wt, "HEAD");
    git(wt, &["switch", "-c", "topic"]);
    commit_file(wt, "f.txt", "topic one\n", "topic commit one");
    let commit_one = git_oid(wt, "HEAD");
    commit_file(wt, "note.txt", "later note\n", "topic commit two");
    let tip = git_oid(wt, "HEAD");
    git(wt, &["switch", "main"]);
    commit_file(wt, "f.txt", "main line\n", "main conflict");
    let onto = git_oid(wt, "HEAD");

    let grit = open_grit(wt);
    git(wt, &["switch", "topic"]);
    let stop = match start_rebase(&grit, &start_opts(upstream, Some(onto), tip)).expect("start") {
        RebaseOutcome::Stopped(s) => s,
        other => panic!("expected stop on first replayed commit, got {other:?}"),
    };
    assert_eq!(stop.commit, commit_one);

    std::fs::write(wt.join("f.txt"), "resolved one\n").expect("resolve");
    git(wt, &["add", "f.txt"]);
    let grit = open_grit(wt);
    match continue_rebase(&grit, &start_opts(upstream, Some(onto), tip)).expect("continue") {
        RebaseOutcome::Done { .. } => {}
        other => panic!("expected done, got {other:?}"),
    }

    assert!(
        git(wt, &["status", "--porcelain"]).trim().is_empty(),
        "worktree and index should be clean after continue"
    );
    assert!(
        git(wt, &["diff", "HEAD"]).trim().is_empty(),
        "unstaged diff vs HEAD should be empty"
    );
    assert!(
        git(wt, &["diff", "--cached"]).trim().is_empty(),
        "staged diff should be empty"
    );
    assert_eq!(git(wt, &["show", "HEAD:f.txt"]), "resolved one\n");
    assert_eq!(git(wt, &["show", "HEAD:note.txt"]), "later note\n");
    assert_eq!(
        std::fs::read_to_string(wt.join("f.txt")).expect("f.txt"),
        "resolved one\n"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("note.txt")).expect("note.txt"),
        "later note\n"
    );
    assert_git_fsck_strict(wt);
}

#[test]
fn multi_step_conflict_continue_keeps_done_commits_in_history() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "f.txt", "base\n", "base");
    let upstream = git_oid(wt, "HEAD");
    git(wt, &["switch", "-c", "topic"]);
    commit_file(wt, "note.txt", "one\n", "commit one");
    let commit_one = git_oid(wt, "HEAD");
    commit_file(wt, "f.txt", "topic two\n", "commit two");
    let commit_two = git_oid(wt, "HEAD");
    commit_file(wt, "note.txt", "three\n", "commit three");
    let commit_three = git_oid(wt, "HEAD");
    let tip = commit_three;
    git(wt, &["switch", "main"]);
    commit_file(wt, "f.txt", "main line\n", "main conflict");
    let onto = git_oid(wt, "HEAD");

    let grit = open_grit(wt);
    git(wt, &["switch", "topic"]);
    let stop = match start_rebase(&grit, &start_opts(upstream, Some(onto), tip)).expect("stop") {
        RebaseOutcome::Stopped(s) => s,
        other => panic!("expected stop on commit two, got {other:?}"),
    };
    assert_eq!(stop.commit, commit_two);
    assert_eq!(stop.done, vec![commit_one]);
    assert!(stop.remaining.starts_with(&[commit_two, commit_three][..]));

    let replayed_one = match resolve_head(grit.git_dir()).expect("head") {
        HeadState::Detached { oid } => oid,
        other => panic!("expected detached HEAD at replayed commit one, got {other:?}"),
    };
    assert_ne!(replayed_one, onto);

    std::fs::write(wt.join("f.txt"), "resolved two\n").expect("resolve");
    git(wt, &["add", "f.txt"]);
    let grit = open_grit(wt);
    match continue_rebase(&grit, &start_opts(upstream, Some(onto), tip)).expect("continue") {
        RebaseOutcome::Done { .. } => {}
        other => panic!("expected done, got {other:?}"),
    }
    let head = git_oid(wt, "HEAD");
    assert!(
        git_merge_base_is_ancestor(wt, &replayed_one.to_hex(), &head.to_hex()),
        "done replay tip must stay an ancestor of HEAD after continue"
    );
    assert_ne!(head, onto);
    assert_git_fsck_strict(wt);
}

#[test]
fn refscope_head_leaves_other_branch_refs_on_continue() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "f.txt", "base\n", "base");
    let upstream = git_oid(wt, "HEAD");
    git(wt, &["switch", "-c", "topic"]);
    commit_file(wt, "f.txt", "topic change\n", "topic commit");
    let tip = git_oid(wt, "HEAD");
    git(wt, &["branch", "mirror", "topic"]);
    git(wt, &["switch", "main"]);
    commit_file(wt, "f.txt", "main change\n", "main commit");
    let onto = git_oid(wt, "HEAD");

    let grit = open_grit(wt);
    git(wt, &["switch", "topic"]);
    let mirror_before = resolve_ref(grit.git_dir(), "refs/heads/mirror").unwrap();
    let _ = start_rebase(&grit, &start_opts(upstream, Some(onto), tip)).expect("stop");
    std::fs::write(wt.join("f.txt"), "resolved\n").expect("resolve");
    git(wt, &["add", "f.txt"]);
    let grit = open_grit(wt);
    let _ = continue_rebase(&grit, &start_opts(upstream, Some(onto), tip)).expect("continue");
    assert_eq!(
        resolve_ref(grit.git_dir(), "refs/heads/mirror").unwrap(),
        mirror_before,
        "RefScope::Head must not move unrelated branch refs"
    );
    assert_ne!(
        resolve_ref(grit.git_dir(), "refs/heads/topic").unwrap(),
        tip
    );
    assert_git_fsck_strict(wt);
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
    git(
        wt,
        &[
            "switch",
            "--detach",
            &format!("{}^{{commit}}", oids["u"].to_hex()),
        ],
    );
    commit_file(wt, "shared.txt", "main-side\n", "onto shared");
    let onto = git_oid(wt, "HEAD");
    git(wt, &["switch", "main"]);
    commit_file(wt, "shared.txt", "topic-side\n", "topic shared");
    let branch_tip = git_oid(wt, "HEAD");

    let grit = open_grit(wt);
    git(wt, &["switch", "main"]);
    let mut opts = rebase_opts();
    opts.rewrite.scope = RefScope::Branches;
    let start = StartRebaseOptions {
        request: RebaseRequest {
            upstream: oids["u"],
            onto: Some(onto),
            branch_tip,
        },
        rebase: opts,
        apply: grit_lib::revwalk::history::ApplyOptions {
            identity: test_ident(),
            reflog_message: "rebase".into(),
            record_operation: None,
            hooks: grit_lib::PostOperationHooks::Run,
        },
    };
    let main_before = resolve_ref(grit.git_dir(), "refs/heads/main").unwrap();
    let stacked_before = resolve_ref(grit.git_dir(), "refs/heads/stacked").unwrap();
    match start_rebase(&grit, &start).expect("start") {
        RebaseOutcome::Stopped(stop) => {
            assert_eq!(stop.commit, branch_tip);
            std::fs::write(wt.join("shared.txt"), "resolved shared\n").expect("resolve");
            git(wt, &["add", "shared.txt"]);
            let grit = open_grit(wt);
            match continue_rebase(&grit, &start).expect("continue") {
                RebaseOutcome::Done { .. } => {}
                other => panic!("expected done after continue, got {other:?}"),
            }
        }
        other => panic!("expected conflict stop, got {other:?}"),
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
    abort_rebase(&grit, &test_ident()).expect("abort");
    assert_eq!(git_oid(wt, "topic"), orig_tip);
    assert_eq!(
        git(wt, &["rev-parse", "HEAD^{tree}"]),
        git(wt, &["rev-parse", &format!("{orig_tip}^{{tree}}")])
    );
    assert_git_fsck_strict(wt);
}

/// Run system `git` without asserting success (hermetic config, no editor).
fn git_unchecked(wt: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .current_dir(wt)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Git Author")
        .env("GIT_AUTHOR_EMAIL", "git@example.com")
        .env("GIT_COMMITTER_NAME", "Git Committer")
        .env("GIT_COMMITTER_EMAIL", "git@example.com")
        .env("GIT_EDITOR", ":")
        .env("EDITOR", ":")
        .output()
        .expect("spawn git")
}

/// `topic` and `main` both edit `f.txt`; `git rebase main` on `topic` stops on the conflict.
fn git_started_conflict_rebase() -> (HistoryRepo, ObjectId, ObjectId) {
    let (repo, _upstream, onto, _tip, orig_tip) = conflict_rebase_fixture();
    let wt = repo.worktree();
    git(wt, &["switch", "topic"]);
    let out = git_unchecked(wt, &["rebase", "main"]);
    assert!(!out.status.success(), "git rebase should stop on conflict");
    assert!(wt.join(".git/rebase-merge").is_dir());
    (repo, onto, orig_tip)
}

#[test]
fn switch_refuses_during_git_started_rebase_and_git_continue_still_moves_only_topic() {
    use grit_lib::error::Error;
    use grit_lib::porcelain::switch::{resolve_switch_target, switch_repository, SwitchOptions};

    let (repo, main_tip, orig_tip) = git_started_conflict_rebase();
    let wt = repo.worktree();
    let grit = open_grit(wt);
    let head_before = git_oid(wt, "HEAD");
    for dry_run in [true, false] {
        let mode = resolve_switch_target(&grit, "main").expect("resolve main");
        let err = switch_repository(
            &grit,
            &SwitchOptions {
                mode,
                dry_run,
                hooks: grit_lib::PostOperationHooks::Run,
            },
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::NeverCancel,
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                Error::OperationInProgress(InProgressOperation::RebaseInteractive)
            ),
            "dry_run={dry_run}: {err:?}"
        );
    }
    assert!(matches!(
        resolve_head(grit.git_dir()).unwrap(),
        HeadState::Detached { oid } if oid == head_before
    ));
    assert_eq!(git_oid(wt, "main"), main_tip);
    assert_eq!(git_oid(wt, "topic"), orig_tip);

    std::fs::write(wt.join("f.txt"), "resolved\n").expect("resolve");
    git(wt, &["add", "f.txt"]);
    let cont = git_unchecked(wt, &["rebase", "--continue"]);
    assert!(
        cont.status.success(),
        "git rebase --continue: {}",
        String::from_utf8_lossy(&cont.stderr)
    );
    assert_eq!(git_oid(wt, "main"), main_tip, "main must not move");
    assert_eq!(
        git_oid(wt, "topic^"),
        main_tip,
        "topic is replayed onto main"
    );
    assert_eq!(
        git(wt, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/topic"
    );
    assert_git_fsck_strict(wt);
}

#[test]
fn switch_refuses_during_merge_conflict() {
    use grit_lib::error::Error;
    use grit_lib::porcelain::switch::{resolve_switch_target, switch_repository, SwitchOptions};

    let (repo, _upstream, _onto, _tip, orig_tip) = conflict_rebase_fixture();
    let wt = repo.worktree();
    support::git_merge_expect_conflict(wt, "topic");
    let grit = open_grit(wt);
    let mode = resolve_switch_target(&grit, "topic").expect("resolve topic");
    let err = switch_repository(
        &grit,
        &SwitchOptions {
            mode,
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .unwrap_err();
    assert!(
        matches!(err, Error::OperationInProgress(InProgressOperation::Merge)),
        "{err:?}"
    );
    assert_eq!(git(wt, &["symbolic-ref", "HEAD"]).trim(), "refs/heads/main");
    assert_eq!(git_oid(wt, "topic"), orig_tip);
}

#[test]
fn status_reports_git_started_rebase_with_branch_and_git_continue_step() {
    use grit_lib::porcelain::status::{status, StatusOptions};
    use grit_lib::progress::{NeverCancel, NullProgress};
    use grit_lib::refs::branch_tracking::{BranchStanding, StandingOptions, StatusHeader};
    use grit_lib::state::NextStep;

    let (repo, main_tip, _) = git_started_conflict_rebase();
    let grit = open_grit(repo.worktree());
    let model = status(
        &grit,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");
    let standing = BranchStanding::compute(
        &grit,
        &model.head,
        StandingOptions {
            newest_ahead_limit: 1,
            counts: false,
        },
    )
    .expect("standing");
    let header =
        StatusHeader::resolve(&standing, &model.state, &detect_in_progress(grit.git_dir()));
    assert_eq!(
        header,
        StatusHeader::RebaseInProgress {
            branch: Some("topic".into()),
            onto: Some(main_tip.to_hex()[..7].to_owned()),
        }
    );

    let op = model.operation.expect("operation for a git-started rebase");
    assert_eq!(op.kind, InProgressOperation::RebaseInteractive);
    assert_eq!(
        op.next_steps,
        vec![
            NextStep::ResolveConflicts,
            NextStep::StageResolved,
            NextStep::ContinueWithGit(InProgressOperation::RebaseInteractive),
        ]
    );
    assert_eq!(
        op.next_steps.last().and_then(NextStep::command).as_deref(),
        Some("git rebase --continue")
    );
    assert_eq!(
        op.alternatives
            .iter()
            .filter_map(NextStep::command)
            .collect::<Vec<_>>(),
        vec!["grit rebase --abort".to_owned()]
    );
}

#[test]
fn continue_and_skip_refuse_git_started_rebase_without_touching_it() {
    let (repo, _main_tip, orig_tip) = git_started_conflict_rebase();
    let wt = repo.worktree();
    let grit = open_grit(wt);
    let head = git_oid(wt, "HEAD");
    let opts = start_opts(head, None, head);
    assert!(matches!(
        continue_rebase(&grit, &opts).unwrap_err(),
        RebaseSessionError::ForeignRebaseInProgress
    ));
    assert!(matches!(
        skip_rebase(&grit, &opts).unwrap_err(),
        RebaseSessionError::ForeignRebaseInProgress
    ));
    assert!(matches!(
        start_rebase(&grit, &opts).unwrap_err(),
        RebaseSessionError::RebaseAlreadyInProgress
    ));
    assert!(wt.join(".git/rebase-merge").is_dir());
    assert!(!wt.join(".git/rebase-merge/grit-plan").exists());
    assert_eq!(git_oid(wt, "HEAD"), head);
    assert_eq!(git_oid(wt, "topic"), orig_tip);
}

#[test]
fn abort_restores_git_started_rebase() {
    let (repo, main_tip, orig_tip) = git_started_conflict_rebase();
    let wt = repo.worktree();
    let grit = open_grit(wt);
    abort_rebase(&grit, &test_ident()).expect("abort git rebase");
    assert!(!wt.join(".git/rebase-merge").exists());
    assert_eq!(
        git(wt, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/topic"
    );
    assert_eq!(git_oid(wt, "topic"), orig_tip);
    assert_eq!(git_oid(wt, "main"), main_tip);
    assert!(
        git(wt, &["status", "--porcelain"]).trim().is_empty(),
        "index and worktree should match orig-head"
    );
    assert!(detect_in_progress(grit.git_dir()).is_empty());
    assert_git_fsck_strict(wt);
}

#[test]
fn abort_restores_detached_git_rebase() {
    let (repo, _upstream, _onto, _tip, orig_tip) = conflict_rebase_fixture();
    let wt = repo.worktree();
    git(wt, &["switch", "--detach", "topic"]);
    let out = git_unchecked(wt, &["rebase", "main"]);
    assert!(!out.status.success(), "git rebase should stop on conflict");
    let grit = open_grit(wt);
    abort_rebase(&grit, &test_ident()).expect("abort detached git rebase");
    assert!(!wt.join(".git/rebase-merge").exists());
    assert!(matches!(
        resolve_head(grit.git_dir()).unwrap(),
        HeadState::Detached { oid } if oid == orig_tip
    ));
    assert_eq!(git_oid(wt, "topic"), orig_tip);
    assert_git_fsck_strict(wt);
}

#[test]
fn abort_refuses_git_rebase_with_autostash() {
    let (repo, _main_tip, orig_tip) = git_started_conflict_rebase();
    let wt = repo.worktree();
    let stash = wt.join(".git/rebase-merge/autostash");
    std::fs::write(&stash, format!("{}\n", orig_tip.to_hex())).expect("autostash");
    let grit = open_grit(wt);
    assert!(matches!(
        abort_rebase(&grit, &test_ident()).unwrap_err(),
        RebaseSessionError::ForeignRebaseInProgress
    ));
    assert!(stash.is_file(), "refusal leaves git's state untouched");
}

#[test]
fn abort_without_rebase_reports_none_in_progress() {
    let (repo, ..) = conflict_rebase_fixture();
    let grit = open_grit(repo.worktree());
    assert!(matches!(
        abort_rebase(&grit, &test_ident()).unwrap_err(),
        RebaseSessionError::NoRebaseInProgress
    ));
}

#[test]
fn git_rebase_continue_finishes_grit_started_rebase() {
    let (repo, upstream, onto, tip, orig_tip) = conflict_rebase_fixture();
    let wt = repo.worktree();
    git(wt, &["switch", "topic"]);
    let author_before = git(
        wt,
        &[
            "show",
            "-s",
            "--format=%an <%ae> %ad",
            "--date=raw",
            "topic",
        ],
    );
    let grit = open_grit(wt);
    match start_rebase(&grit, &start_opts(upstream, Some(onto), tip)).expect("start") {
        RebaseOutcome::Stopped(_) => {}
        other => panic!("expected conflict stop, got {other:?}"),
    }
    let dir = wt.join(".git/rebase-merge");
    assert_eq!(
        std::fs::read_to_string(dir.join("stopped-sha"))
            .unwrap()
            .trim(),
        tip.to_hex()
    );
    assert!(std::fs::read_to_string(dir.join("done"))
        .unwrap()
        .starts_with(&format!("pick {} ", &tip.to_hex()[..7])));
    let field = |fmt: &str| {
        git(
            wt,
            &[
                "show",
                "-s",
                &format!("--format={fmt}"),
                "--date=raw",
                "topic",
            ],
        )
        .trim()
        .to_owned()
    };
    assert_eq!(
        std::fs::read_to_string(dir.join("author-script")).unwrap(),
        format!(
            "GIT_AUTHOR_NAME='{}'\nGIT_AUTHOR_EMAIL='{}'\nGIT_AUTHOR_DATE='@{}'\n",
            field("%an"),
            field("%ae"),
            field("%ad")
        )
    );

    std::fs::write(wt.join("f.txt"), "resolved\n").expect("resolve");
    git(wt, &["add", "f.txt"]);
    let status = git(wt, &["status"]);
    assert!(status.contains("rebasing branch 'topic'"), "{status}");
    let cont = git_unchecked(wt, &["rebase", "--continue"]);
    assert!(
        cont.status.success(),
        "git rebase --continue on a grit rebase: {}",
        String::from_utf8_lossy(&cont.stderr)
    );

    assert!(!dir.exists());
    assert_eq!(
        git(wt, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/topic"
    );
    assert_eq!(git_oid(wt, "main"), onto, "main must not move");
    assert_ne!(git_oid(wt, "topic"), orig_tip);
    assert_eq!(git_oid(wt, "topic^"), onto);
    assert_eq!(git(wt, &["show", "topic:f.txt"]), "resolved\n");
    assert_eq!(
        git(
            wt,
            &[
                "show",
                "-s",
                "--format=%an <%ae> %ad",
                "--date=raw",
                "topic"
            ]
        ),
        author_before,
        "git keeps the original author from author-script"
    );
    assert_eq!(
        git(wt, &["show", "-s", "--format=%s", "topic"]).trim(),
        "topic commit"
    );
    assert_git_fsck_strict(wt);
}

#[test]
fn git_rebase_skip_and_continue_finish_multi_step_grit_rebase() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "f.txt", "base\n", "base");
    let upstream = git_oid(wt, "HEAD");
    git(wt, &["switch", "-c", "topic"]);
    commit_file(wt, "f.txt", "topic one\n", "topic one");
    commit_file(wt, "note.txt", "note\n", "topic two");
    commit_file(wt, "f.txt", "topic three\n", "topic three");
    let tip = git_oid(wt, "HEAD");
    git(wt, &["switch", "main"]);
    commit_file(wt, "f.txt", "main\n", "main");
    let onto = git_oid(wt, "HEAD");
    git(wt, &["switch", "topic"]);

    let grit = open_grit(wt);
    assert!(matches!(
        start_rebase(&grit, &start_opts(upstream, Some(onto), tip)).expect("start"),
        RebaseOutcome::Stopped(_)
    ));
    assert!(
        !wt.join("note.txt").exists(),
        "a stop must not leave files from the original tip behind as untracked"
    );
    assert_eq!(git(wt, &["status", "--porcelain"]), "UU f.txt\n");
    let skip = git_unchecked(wt, &["rebase", "--skip"]);
    assert!(
        !skip.status.success(),
        "topic three conflicts after skipping topic one"
    );
    std::fs::write(wt.join("f.txt"), "resolved three\n").expect("resolve");
    git(wt, &["add", "f.txt"]);
    let cont = git_unchecked(wt, &["rebase", "--continue"]);
    assert!(
        cont.status.success(),
        "git rebase --continue: {}",
        String::from_utf8_lossy(&cont.stderr)
    );
    assert_eq!(
        git(wt, &["log", "--format=%s", "main..topic"]),
        "topic three\ntopic two\n"
    );
    assert_eq!(git_oid(wt, "main"), onto);
    assert_eq!(git(wt, &["show", "topic:f.txt"]), "resolved three\n");
    assert_git_fsck_strict(wt);
}

#[test]
fn abort_clears_rerere_state_of_git_started_rebase_like_git() {
    let rerere_rebase = || {
        let (repo, _upstream, _onto, _tip, orig_tip) = conflict_rebase_fixture();
        let wt = repo.worktree();
        git(wt, &["config", "rerere.enabled", "true"]);
        git(wt, &["switch", "topic"]);
        let out = git_unchecked(wt, &["rebase", "main"]);
        assert!(!out.status.success(), "git rebase should stop on conflict");
        assert_eq!(git(wt, &["rerere", "status"]), "f.txt\n");
        assert!(wt.join(".git/MERGE_RR").is_file());
        (repo, orig_tip)
    };

    let (control, _) = rerere_rebase();
    git(control.worktree(), &["rebase", "--abort"]);
    assert!(!control.worktree().join(".git/MERGE_RR").exists());
    assert_eq!(git(control.worktree(), &["rerere", "status"]), "");

    let (repo, orig_tip) = rerere_rebase();
    let wt = repo.worktree();
    abort_rebase(&open_grit(wt), &test_ident()).expect("abort git rebase");
    assert_eq!(git_oid(wt, "topic"), orig_tip);
    assert!(
        !wt.join(".git/MERGE_RR").exists(),
        "abort must drop the abandoned conflict from MERGE_RR"
    );
    assert_eq!(git(wt, &["rerere", "status"]), "");
    assert_eq!(git(wt, &["rerere", "remaining"]), "");
    assert_git_fsck_strict(wt);
}

fn assert_refused_untouched(wt: &std::path::Path, grit: &Repository, orig_tip: ObjectId) {
    assert_eq!(git_oid(wt, "topic"), orig_tip);
    assert_eq!(
        git(wt, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/topic"
    );
    assert!(!grit.git_dir().join("rebase-merge").exists());
    assert!(!std::fs::read_to_string(wt.join("f.txt"))
        .expect("f.txt")
        .contains("<<<<<<<"));
}

#[test]
fn conflicting_start_refuses_staged_and_unstaged_changes_instead_of_discarding_them() {
    let (repo, upstream, onto, tip, orig_tip) = conflict_rebase_fixture();
    let wt = repo.worktree();
    git(wt, &["switch", "topic"]);
    std::fs::write(wt.join("new.txt"), "staged work\n").expect("write");
    git(wt, &["add", "new.txt"]);
    std::fs::write(wt.join("new.txt"), "staged work\nplus unstaged\n").expect("write");
    let grit = open_grit(wt);

    let err = start_rebase(&grit, &start_opts(upstream, Some(onto), tip))
        .expect_err("a stop would discard local changes");
    match err {
        RebaseSessionError::Library(grit_lib::error::Error::DirtyWorktree { operation, paths }) => {
            assert_eq!(operation, grit_lib::error::WorktreeOperation::Rebase);
            assert_eq!(paths, vec!["new.txt".to_owned()]);
        }
        other => panic!("expected dirty worktree, got {other:?}"),
    }
    assert_refused_untouched(wt, &grit, orig_tip);
    assert_eq!(
        std::fs::read_to_string(wt.join("new.txt")).expect("new.txt"),
        "staged work\nplus unstaged\n"
    );
    assert_eq!(git(wt, &["show", ":new.txt"]), "staged work\n");
}

#[test]
fn conflicting_start_refuses_to_overwrite_untracked_file() {
    let (repo, upstream, _onto, tip, orig_tip) = conflict_rebase_fixture();
    let wt = repo.worktree();
    commit_file(wt, "added.txt", "upstream adds this\n", "main adds file");
    let onto = git_oid(wt, "HEAD");
    git(wt, &["switch", "topic"]);
    std::fs::write(wt.join("added.txt"), "precious untracked\n").expect("write");
    let grit = open_grit(wt);

    let err = start_rebase(&grit, &start_opts(upstream, Some(onto), tip))
        .expect_err("a stop would overwrite the untracked file");
    assert!(
        matches!(
            &err,
            RebaseSessionError::Library(grit_lib::error::Error::Message(msg)) if msg.contains("added.txt")
        ),
        "unexpected {err:?}"
    );
    assert_refused_untouched(wt, &grit, orig_tip);
    assert_eq!(
        std::fs::read_to_string(wt.join("added.txt")).expect("added.txt"),
        "precious untracked\n"
    );
}

#[test]
fn clean_replay_still_carries_local_changes() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "a.txt", "a\n", "base");
    let upstream = git_oid(wt, "HEAD");
    git(wt, &["switch", "-c", "topic"]);
    commit_file(wt, "t.txt", "t\n", "topic");
    let tip = git_oid(wt, "HEAD");
    git(wt, &["switch", "main"]);
    commit_file(wt, "m.txt", "m\n", "main");
    let onto = git_oid(wt, "HEAD");
    git(wt, &["switch", "topic"]);
    std::fs::write(wt.join("t.txt"), "t\nlocal edit\n").expect("write");

    let grit = open_grit(wt);
    let out = start_rebase(&grit, &start_opts(upstream, Some(onto), tip)).expect("start");
    assert!(matches!(out, RebaseOutcome::Done { .. }));
    assert_eq!(
        std::fs::read_to_string(wt.join("t.txt")).expect("t.txt"),
        "t\nlocal edit\n"
    );
    assert_git_fsck_strict(wt);
}

#[test]
fn multiple_conflict_stops_report_the_last_invocation_head_change() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "f.txt", "base\n", "base f");
    commit_file(wt, "g.txt", "base\n", "base g");
    let upstream = git_oid(wt, "HEAD");
    git(wt, &["switch", "-c", "topic"]);
    commit_file(wt, "f.txt", "topic f\n", "topic f");
    let first = git_oid(wt, "HEAD");
    commit_file(wt, "g.txt", "topic g\n", "topic g");
    let tip = git_oid(wt, "HEAD");
    git(wt, &["switch", "main"]);
    commit_file(wt, "f.txt", "main f\n", "main f");
    commit_file(wt, "g.txt", "main g\n", "main g");
    let onto = git_oid(wt, "HEAD");
    git(wt, &["switch", "topic"]);
    let grit = open_grit(wt);
    let opts = start_opts(upstream, Some(onto), tip);
    let RebaseOutcome::Stopped(stop) = start_rebase(&grit, &opts).expect("start") else {
        panic!("expected first conflict");
    };
    assert_eq!(stop.commit, first);
    assert_eq!(git_oid(wt, "HEAD"), onto);

    std::fs::write(wt.join("f.txt"), "resolved f\n").expect("resolve f");
    git(wt, &["add", "f.txt"]);
    let RebaseOutcome::Stopped(stop) = continue_rebase(&grit, &opts).expect("first continue")
    else {
        panic!("expected second conflict");
    };
    assert_eq!(stop.commit, tip);
    assert_eq!(stop.done, vec![first]);
    assert_eq!(git_oid(wt, "topic"), tip);
    let before = git_oid(wt, "HEAD");
    assert_ne!(before, onto);

    std::fs::write(wt.join("g.txt"), "resolved g\n").expect("resolve g");
    git(wt, &["add", "g.txt"]);
    let RebaseOutcome::Done {
        applied,
        head_change,
        plan,
    } = continue_rebase(&grit, &opts).expect("second continue")
    else {
        panic!("expected completed rebase");
    };
    let after = git_oid(wt, "HEAD");
    assert_ne!(before, after);
    assert_eq!(head_change, Some((before, after)));
    assert_eq!(applied.head, after);
    assert_eq!(plan.head.old_commit, Some(before));
    assert_eq!(plan.head.head_ref, None);
    assert_eq!(plan.head_change(), None);
    assert_eq!(git_oid(wt, "topic"), after);
    assert_eq!(git_oid(wt, "HEAD~1"), before);
    assert_eq!(git(wt, &["show", "HEAD:f.txt"]), "resolved f\n");
    assert_eq!(git(wt, &["show", "HEAD:g.txt"]), "resolved g\n");
    assert!(git(wt, &["status", "--porcelain"]).trim().is_empty());
    assert!(!grit.git_dir().join("rebase-merge").exists());
    assert_git_fsck_strict(wt);
}

#[test]
fn skip_with_remaining_replay_reports_head_change() {
    let (repo, upstream, onto, conflict, _) = conflict_rebase_fixture();
    let wt = repo.worktree();
    git(wt, &["switch", "topic"]);
    commit_file(wt, "note.txt", "later note\n", "later commit");
    let tip = git_oid(wt, "HEAD");
    let grit = open_grit(wt);
    let opts = start_opts(upstream, Some(onto), tip);
    assert!(matches!(
        start_rebase(&grit, &opts).expect("start"),
        RebaseOutcome::Stopped(_)
    ));
    let before = git_oid(wt, "HEAD");
    assert_eq!(before, onto);
    let RebaseOutcome::Done {
        applied,
        head_change,
        plan,
    } = skip_rebase(&grit, &opts).expect("skip")
    else {
        panic!("expected completed rebase");
    };
    let after = git_oid(wt, "HEAD");
    assert_ne!(before, after);
    assert_eq!(head_change, Some((before, after)));
    assert_eq!(applied.head, after);
    assert_eq!(plan.head.old_commit, Some(before));
    assert_eq!(plan.head_change(), None);
    assert_eq!(plan.commits[0].old, conflict);
    assert_eq!(plan.commits[0].new, None);
    assert_eq!(git_oid(wt, "topic"), after);
    assert_eq!(git(wt, &["show", "HEAD:f.txt"]), "main change\n");
    assert_eq!(git(wt, &["show", "HEAD:note.txt"]), "later note\n");
    assert!(git(wt, &["status", "--porcelain"]).trim().is_empty());
    assert_eq!(
        resolve_head(grit.git_dir()).unwrap().branch_name(),
        Some("topic")
    );
    assert!(!grit.git_dir().join("rebase-merge").exists());
    assert_git_fsck_strict(wt);
}
