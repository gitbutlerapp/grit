//! Stop/resume rebase session tests.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;

use grit_lib::objects::ObjectId;
use grit_lib::refs::resolve_ref;
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    abort_rebase, continue_rebase, read_history_last, skip_rebase, start_rebase, undo_last_rewrite,
    EmptyPolicy, RebaseOntoOptions, RebaseOutcome, RebaseRequest, RebaseSessionError, RefScope,
    RewriteOptions, StartRebaseOptions,
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
        drop: Vec::new(),
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
        pre_rebase: grit_lib::revwalk::history::PreRebase::Skip,
    }
}

/// Assert the finished rebase is the newest `HEAD` reflog entry, moving from `orig_tip` to the
/// current `HEAD`, and that every `HEAD` log entry starts where the previous one ended.
fn assert_head_reflog_records_rebase_finish(wt: &std::path::Path, orig_tip: ObjectId) {
    let head = git_oid(wt, "HEAD");
    assert_eq!(git_oid(wt, "HEAD@{0}"), head, "HEAD@{{0}} must be HEAD");
    assert_eq!(
        git_oid(wt, "HEAD@{1}"),
        orig_tip,
        "HEAD@{{1}} must be the pre-rebase tip"
    );
    let log = std::fs::read_to_string(wt.join(".git/logs/HEAD")).expect("HEAD log");
    let entries: Vec<(&str, &str, &str)> = log
        .lines()
        .map(|line| {
            let (header, message) = line.split_once('\t').expect("reflog message");
            let mut oids = header.split(' ');
            (
                oids.next().expect("old"),
                oids.next().expect("new"),
                message,
            )
        })
        .collect();
    let &(old, new, message) = entries.last().expect("entries");
    assert_eq!(old, orig_tip.to_hex());
    assert_eq!(new, head.to_hex());
    assert_eq!(message, "rebase");
    for pair in entries.windows(2) {
        assert_eq!(pair[1].0, pair[0].1, "HEAD reflog chain broken: {log}");
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
    let out = start_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("start");
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
    match start_rebase(
        &grit,
        &start_opts(onto, None, after),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("up to date")
    {
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
    let out = start_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("start conflict");
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
    let done = continue_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("continue");
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
    assert_head_reflog_records_rebase_finish(wt, orig_tip);
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
    let _ = start_rebase(
        &grit2,
        &start_opts(upstream2, Some(onto2), tip2),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("stop");
    abort_rebase(&grit2, &test_ident()).expect("abort");
    assert_eq!(git_oid(wt2, "topic"), orig2);
    assert!(resolve_head(grit2.git_dir()).unwrap().branch_name() == Some("topic"));
    assert_git_fsck_strict(wt2);
}

#[test]
fn continue_after_commit_during_rebase_conflict() {
    use grit_lib::change_id::change_id;
    use grit_lib::porcelain::commit::{commit_with_scope, CommitRequest, CommitScope};

    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "f", "1\n2\n3\n", "base");
    let upstream = git_oid(wt, "HEAD");
    git(wt, &["switch", "-c", "o"]);
    commit_file(wt, "f", "1\nO\n3\n", "o1");
    let o1 = git_oid(wt, "HEAD");
    let o1_author = git(wt, &["log", "-1", "--format=%an <%ae>", &o1.to_hex()]);
    let o1_change_id = change_id(&open_grit(wt).odb().read_commit(&o1).expect("o1 commit"));
    std::fs::write(wt.join("g"), "y\n").expect("g");
    git(wt, &["add", "g"]);
    git(wt, &["commit", "-qm", "o2"]);
    let tip = git_oid(wt, "HEAD");
    git(wt, &["switch", "main"]);
    commit_file(wt, "f", "1\nM\n3\n", "m");
    let onto = git_oid(wt, "HEAD");

    let grit = open_grit(wt);
    git(wt, &["switch", "o"]);
    let orig_tip = git_oid(wt, "o");
    let out = start_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("start conflict");
    assert!(matches!(out, RebaseOutcome::Stopped(_)));

    std::fs::write(wt.join("f"), "1\nR\n3\n").expect("resolve");
    commit_with_scope(
        &grit,
        &CommitRequest {
            message: "resolved o1\n".into(),
            committer: Some(IDENT.parse().expect("IDENT")),
            ..Default::default()
        },
        CommitScope::AutoStage,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("commit resolution");

    let done = continue_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("continue after commit");
    assert!(matches!(done, RebaseOutcome::Done { .. }));
    assert!(!grit.git_dir().join("rebase-merge").exists());
    assert_ne!(git_oid(wt, "o"), orig_tip);
    assert_eq!(git(wt, &["show", "HEAD:f"]), "1\nR\n3\n");
    assert_eq!(git(wt, &["show", "HEAD:g"]), "y\n");
    assert_eq!(
        git(wt, &["log", "-1", "--format=%s", "HEAD~1"]).trim(),
        "resolved o1"
    );
    assert_eq!(
        git(wt, &["log", "-1", "--format=%an <%ae>", "HEAD~1"]).trim(),
        o1_author.trim()
    );
    if let Some(expected_id) = o1_change_id {
        let resolved = git_oid(wt, "HEAD~1");
        let resolved_id = change_id(&grit.odb().read_commit(&resolved).expect("resolved commit"));
        assert_eq!(resolved_id, Some(expected_id));
    }
    assert_git_fsck_strict(wt);
}

#[test]
fn continue_nothing_to_commit_when_index_matches_stop_head() {
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
    let _ = start_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("stop");
    git(wt, &["reset", "--mixed", "HEAD"]);
    let err = continue_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect_err("empty resolution continue");
    assert!(matches!(err, RebaseSessionError::NothingToCommit));
    assert!(grit.git_dir().join("rebase-merge/grit-plan").is_file());
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
    let _ = start_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("stop");
    let before = git_oid(wt, "HEAD");
    let out = skip_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("skip");
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
    assert_head_reflog_records_rebase_finish(wt, tip);
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
    let _ = start_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("stop");
    let err = continue_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .unwrap_err();
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
    let _ = start_rebase(
        &grit,
        &opts,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("first start");
    let err = start_rebase(
        &grit,
        &opts,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .unwrap_err();
    assert!(matches!(err, RebaseSessionError::RebaseAlreadyInProgress));
}

#[test]
fn pull_and_pick_refuse_during_grit_rebase_conflict_stop() {
    use grit_lib::error::Error;
    use grit_lib::porcelain::pull::{pull, PullOptions};
    use grit_lib::porcelain::replay::ReplayDirection;
    use grit_lib::porcelain::replay_sequence::{
        start_replay_sequence, ReplayOptions, ReplaySequenceRequest,
    };
    use grit_lib::progress::{NeverCancel, NullProgress};
    use grit_lib::remote::fetch::NoProgress;

    let (repo, upstream, onto, tip, orig_tip) = conflict_rebase_fixture();
    let wt = repo.worktree();
    let grit = open_grit(wt);
    git(wt, &["switch", "topic"]);
    let _ = start_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("stop");
    let head_before = git_oid(wt, "HEAD");
    let branch_before = resolve_ref(grit.git_dir(), "refs/heads/topic").unwrap();

    let mut pull_progress = NoProgress;
    let pull_err = pull(
        &grit,
        &PullOptions::default(),
        &mut pull_progress,
        &NeverCancel,
        None,
    )
    .unwrap_err();
    assert!(
        matches!(
            pull_err,
            grit_lib::porcelain::pull::PullError::Repository(Error::OperationInProgress(
                InProgressOperation::Rebase
            ))
        ),
        "pull: {pull_err:?}"
    );

    let pick_req = ReplaySequenceRequest {
        commits: vec![onto],
        direction: ReplayDirection::Pick,
        committer: COMMITTER.parse().expect("signature"),
        options: ReplayOptions::default(),
    };
    let pick_err = start_replay_sequence(&grit, &pick_req).unwrap_err();
    assert!(
        matches!(
            pick_err,
            Error::OperationInProgress(InProgressOperation::Rebase)
        ),
        "pick: {pick_err:?}"
    );

    let err = start_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut NullProgress,
        &NeverCancel,
    )
    .unwrap_err();
    assert!(matches!(err, RebaseSessionError::RebaseAlreadyInProgress));

    assert_eq!(git_oid(wt, "HEAD"), head_before);
    assert_eq!(
        resolve_ref(grit.git_dir(), "refs/heads/topic").unwrap(),
        branch_before
    );
    assert_eq!(branch_before, orig_tip);
    assert!(wt.join(".git/rebase-merge").is_dir());
    assert_git_fsck_strict(wt);
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
    let out = start_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("start");
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
    let _ = start_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("stop");
    assert_eq!(
        resolve_ref(grit.git_dir(), "refs/heads/topic").unwrap(),
        orig_tip
    );
    std::fs::write(wt.join("f.txt"), "resolved\n").expect("resolve");
    git(wt, &["add", "f.txt"]);
    let _ = continue_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("continue");
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
    let _ = start_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("stop");
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
    let _ = start_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("stop");
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
    let _ = start_rebase(
        grit,
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("grit stop");
    std::fs::write(wt.join("f.txt"), "resolved\n").expect("resolve");
    git(wt, &["add", "f.txt"]);
    // Re-open so `continue_rebase` reads the index `git add` wrote (no stale handle).
    let grit = open_grit(wt);
    let _ = continue_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("grit continue");
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

/// Run one `git rebase` step in `wt`; returns whether it finished without stopping.
fn git_rebase_step(wt: &std::path::Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .current_dir(wt)
        .arg("rebase")
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_EDITOR", ":")
        .output()
        .expect("git rebase")
        .status
        .success()
}

#[test]
fn conflict_markers_name_each_replayed_commit_like_git_rebase() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "f.txt", "base\n", "base f");
    commit_file(wt, "g.txt", "base\n", "base g");
    let upstream = git_oid(wt, "HEAD");
    git(wt, &["switch", "-c", "topic"]);
    commit_file(wt, "f.txt", "topic f\n", "first replayed");
    let first = git_oid(wt, "HEAD");
    commit_file(wt, "g.txt", "topic g\n", "second replayed");
    let tip = git_oid(wt, "HEAD");
    git(wt, &["switch", "main"]);
    commit_file(wt, "f.txt", "main f\n", "main f");
    commit_file(wt, "g.txt", "main g\n", "main g");
    let onto = git_oid(wt, "HEAD");
    git(wt, &["switch", "topic"]);

    let git_copy = tempfile::tempdir().expect("git copy");
    let git_wt = git_copy.path();
    let copied = std::process::Command::new("cp")
        .args([
            "-a",
            &format!("{}/.", wt.display()),
            &git_wt.to_string_lossy(),
        ])
        .status()
        .expect("cp");
    assert!(copied.success());

    let incoming_marker = |body: &str| {
        body.lines()
            .find(|line| line.starts_with(">>>>>>>"))
            .unwrap_or_else(|| panic!("no incoming marker in {body:?}"))
            .to_owned()
    };

    let opts = start_opts(upstream, Some(onto), tip);
    let RebaseOutcome::Stopped(stop) = start_rebase(
        &open_grit(wt),
        &opts,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("start") else {
        panic!("expected the first replayed commit to stop");
    };
    assert_eq!(stop.commit, first);
    assert!(!git_rebase_step(git_wt, &["main"]), "git should stop too");
    let grit_f = std::fs::read_to_string(wt.join("f.txt")).expect("f.txt");
    assert_eq!(
        grit_f,
        std::fs::read_to_string(git_wt.join("f.txt")).expect("git f.txt")
    );
    assert_eq!(
        incoming_marker(&grit_f),
        format!(">>>>>>> {} (first replayed)", &first.to_hex()[..7])
    );

    for dir in [wt, git_wt] {
        std::fs::write(dir.join("f.txt"), "resolved f\n").expect("resolve f");
        git(dir, &["add", "f.txt"]);
    }
    let RebaseOutcome::Stopped(stop) = continue_rebase(
        &open_grit(wt),
        &opts,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("continue") else {
        panic!("expected the second replayed commit to stop");
    };
    assert_eq!(stop.commit, tip);
    assert!(
        !git_rebase_step(git_wt, &["--continue"]),
        "git should stop on the second commit"
    );
    let grit_g = std::fs::read_to_string(wt.join("g.txt")).expect("g.txt");
    assert_eq!(
        grit_g,
        std::fs::read_to_string(git_wt.join("g.txt")).expect("git g.txt")
    );
    assert_eq!(
        incoming_marker(&grit_g),
        format!(">>>>>>> {} (second replayed)", &tip.to_hex()[..7])
    );
    assert_git_fsck_strict(wt);
}

#[test]
fn conflict_stop_detaches_head_at_onto_not_upstream() {
    let (repo, upstream, onto, tip, _) = conflict_rebase_fixture();
    let wt = repo.worktree();
    let grit = open_grit(wt);
    git(wt, &["switch", "topic"]);
    let _ = start_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("stop");
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
    let stop = match start_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("start")
    {
        RebaseOutcome::Stopped(s) => s,
        other => panic!("expected stop on first replayed commit, got {other:?}"),
    };
    assert_eq!(stop.commit, commit_one);

    std::fs::write(wt.join("f.txt"), "resolved one\n").expect("resolve");
    git(wt, &["add", "f.txt"]);
    let grit = open_grit(wt);
    match continue_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("continue")
    {
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
    let stop = match start_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("stop")
    {
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
    match continue_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("continue")
    {
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
fn continue_after_stop_chains_past_a_commit_already_upstream() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "f.txt", "base\n", "base");
    let upstream = git_oid(wt, "HEAD");
    git(wt, &["switch", "-c", "topic"]);
    git(wt, &["switch", "main"]);
    commit_file(wt, "m1.txt", "m1\n", "m1");
    let m1 = git_oid(wt, "HEAD");
    commit_file(wt, "f.txt", "main\n", "main conflict");
    let onto = git_oid(wt, "HEAD");
    git(wt, &["switch", "topic"]);
    commit_file(wt, "f.txt", "topic\n", "topic conflict");
    let conflicting = git_oid(wt, "HEAD");
    git(wt, &["cherry-pick", &m1.to_hex()]);
    commit_file(wt, "y.txt", "y\n", "y1");
    let tip = git_oid(wt, "HEAD");

    let mut opts = start_opts(upstream, Some(onto), tip);
    opts.rebase.skip_applied = true;
    let grit = open_grit(wt);
    let stop = match start_rebase(
        &grit,
        &opts,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("start")
    {
        RebaseOutcome::Stopped(stop) => stop,
        other => panic!("expected a stop on the conflicting commit, got {other:?}"),
    };
    assert_eq!(stop.commit, conflicting);

    std::fs::write(wt.join("f.txt"), "resolved\n").expect("resolve");
    git(wt, &["add", "f.txt"]);
    let grit = open_grit(wt);
    match continue_rebase(
        &grit,
        &opts,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("continue")
    {
        RebaseOutcome::Done { .. } => {}
        other => panic!("expected done, got {other:?}"),
    }
    assert_eq!(
        git(wt, &["log", "--reverse", "--format=%s", "main..topic"]),
        "topic conflict\ny1\n",
        "the skipped commit is dropped and y1 follows the resolved commit"
    );
    assert_eq!(git_oid(wt, "topic~2"), onto);
    assert_eq!(git(wt, &["show", "topic:f.txt"]), "resolved\n");
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
    let _ = start_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("stop");
    std::fs::write(wt.join("f.txt"), "resolved\n").expect("resolve");
    git(wt, &["add", "f.txt"]);
    let grit = open_grit(wt);
    let _ = continue_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("continue");
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
        pre_rebase: grit_lib::revwalk::history::PreRebase::Skip,
    };
    let main_before = resolve_ref(grit.git_dir(), "refs/heads/main").unwrap();
    let stacked_before = resolve_ref(grit.git_dir(), "refs/heads/stacked").unwrap();
    match start_rebase(
        &grit,
        &start,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("start")
    {
        RebaseOutcome::Stopped(stop) => {
            assert_eq!(stop.commit, branch_tip);
            std::fs::write(wt.join("shared.txt"), "resolved shared\n").expect("resolve");
            git(wt, &["add", "shared.txt"]);
            let grit = open_grit(wt);
            match continue_rebase(
                &grit,
                &start,
                &mut grit_lib::progress::NullProgress,
                &grit_lib::progress::NeverCancel,
            )
            .expect("continue")
            {
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

/// Issue #2255: Git's `rebase --continue` must not replay stacked-branch commits grit appended
/// to the full replay order when `--update-refs=branches` is active.
#[test]
fn git_rebase_continue_leaves_stacked_branch_untouched_on_primary_conflict() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "base.txt", "base\n", "base");
    let upstream = git_oid(wt, "HEAD");
    git(wt, &["switch", "-c", "a"]);
    commit_file(wt, "shared.txt", "a1\n", "a: shared");
    commit_file(wt, "a2.txt", "a2\n", "a: two");
    let a_tip = git_oid(wt, "HEAD");
    git(wt, &["switch", "-c", "b"]);
    commit_file(wt, "b1.txt", "b1\n", "b: one");
    let b_before = git_oid(wt, "HEAD");
    git(wt, &["switch", "main"]);
    commit_file(wt, "shared.txt", "m\n", "main: shared");
    let onto = git_oid(wt, "HEAD");
    git(wt, &["switch", "a"]);

    let grit = open_grit(wt);
    let mut opts = rebase_opts();
    opts.rewrite.scope = RefScope::Branches;
    let start = StartRebaseOptions {
        request: RebaseRequest {
            upstream,
            onto: Some(onto),
            branch_tip: a_tip,
        },
        rebase: opts.clone(),
        apply: grit_lib::revwalk::history::ApplyOptions {
            identity: test_ident(),
            reflog_message: "rebase".into(),
            record_operation: None,
            hooks: grit_lib::PostOperationHooks::Run,
        },
        pre_rebase: grit_lib::revwalk::history::PreRebase::Skip,
    };

    match start_rebase(
        &grit,
        &start,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("start")
    {
        RebaseOutcome::Stopped(_) => {}
        other => panic!("expected conflict stop, got {other:?}"),
    }

    let todo = std::fs::read_to_string(wt.join(".git/rebase-merge/git-rebase-todo")).unwrap();
    assert!(
        todo.contains("a: two"),
        "git todo must include the remaining primary commit: {todo}"
    );
    assert!(
        !todo.contains("b: one"),
        "git todo must not include stacked branch commits: {todo}"
    );

    std::fs::write(wt.join("shared.txt"), "resolved\n").expect("resolve");
    git(wt, &["add", "shared.txt"]);
    let cont = git_unchecked(wt, &["rebase", "--continue"]);
    assert!(
        cont.status.success(),
        "git rebase --continue: {}",
        String::from_utf8_lossy(&cont.stderr)
    );

    assert_eq!(
        git(wt, &["log", "--format=%s", "main..a"]),
        "a: two\na: shared\n",
        "branch a should end on its own commits only"
    );
    assert_eq!(
        git_oid(wt, "b"),
        b_before,
        "branch b must stay on old history"
    );
    assert_git_fsck_strict(wt);
}

#[test]
fn git_rebase_continue_todo_excludes_descendant_merge_commit() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "base.txt", "base\n", "base");
    let upstream = git_oid(wt, "HEAD");
    git(wt, &["switch", "-c", "a"]);
    commit_file(wt, "shared.txt", "a1\n", "a: shared");
    commit_file(wt, "a2.txt", "a2\n", "a: two");
    let a_tip = git_oid(wt, "HEAD");
    git(wt, &["switch", "-c", "side"]);
    commit_file(wt, "side.txt", "side\n", "side: one");
    git(wt, &["switch", "a"]);
    git(wt, &["merge", "--no-ff", "-m", "Merge side into a", "side"]);
    git(wt, &["switch", "-c", "b"]);
    commit_file(wt, "b1.txt", "b1\n", "b: one");
    git(wt, &["switch", "main"]);
    commit_file(wt, "shared.txt", "m\n", "main: shared");
    let onto = git_oid(wt, "HEAD");
    git(wt, &["switch", "a"]);

    let grit = open_grit(wt);
    let mut opts = rebase_opts();
    opts.rewrite.scope = RefScope::Branches;
    let start = StartRebaseOptions {
        request: RebaseRequest {
            upstream,
            onto: Some(onto),
            branch_tip: a_tip,
        },
        rebase: opts,
        apply: grit_lib::revwalk::history::ApplyOptions {
            identity: test_ident(),
            reflog_message: "rebase".into(),
            record_operation: None,
            hooks: grit_lib::PostOperationHooks::Run,
        },
        pre_rebase: grit_lib::revwalk::history::PreRebase::Skip,
    };

    match start_rebase(
        &grit,
        &start,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("start")
    {
        RebaseOutcome::Stopped(_) => {}
        other => panic!("expected conflict stop, got {other:?}"),
    }

    let todo = std::fs::read_to_string(wt.join(".git/rebase-merge/git-rebase-todo")).unwrap();
    assert!(
        !todo.contains("Merge side into a"),
        "git todo must not replay stacked merge commits: {todo}"
    );
    assert!(
        todo.contains("a: two"),
        "git todo must still list remaining primary commits: {todo}"
    );
}

#[test]
fn abort_restores_branch_index_worktree_exactly() {
    let (repo, upstream, onto, tip, orig_tip) = conflict_rebase_fixture();
    let wt = repo.worktree();
    let grit = open_grit(wt);
    git(wt, &["switch", "topic"]);
    let _ = start_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("stop");
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
        continue_rebase(
            &grit,
            &opts,
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::NeverCancel
        )
        .unwrap_err(),
        RebaseSessionError::ForeignRebaseInProgress
    ));
    assert!(matches!(
        skip_rebase(
            &grit,
            &opts,
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::NeverCancel
        )
        .unwrap_err(),
        RebaseSessionError::ForeignRebaseInProgress
    ));
    assert!(matches!(
        start_rebase(
            &grit,
            &opts,
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::NeverCancel
        )
        .unwrap_err(),
        RebaseSessionError::ForeignRebaseInProgress
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
    match start_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("start")
    {
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
        start_rebase(
            &grit,
            &start_opts(upstream, Some(onto), tip),
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::NeverCancel
        )
        .expect("start"),
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

    let err = start_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
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

    let err = start_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect_err("a stop would overwrite the untracked file");
    assert!(
        matches!(
            &err,
            RebaseSessionError::Library(grit_lib::error::Error::WouldOverwrite {
                reason: grit_lib::error::OverwriteReason::Untracked,
                paths,
                ..
            }) if paths == &["added.txt".to_owned()]
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
    let out = start_rebase(
        &grit,
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("start");
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
    let RebaseOutcome::Stopped(stop) = start_rebase(
        &grit,
        &opts,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("start") else {
        panic!("expected first conflict");
    };
    assert_eq!(stop.commit, first);
    assert_eq!(git_oid(wt, "HEAD"), onto);

    std::fs::write(wt.join("f.txt"), "resolved f\n").expect("resolve f");
    git(wt, &["add", "f.txt"]);
    let RebaseOutcome::Stopped(stop) = continue_rebase(
        &grit,
        &opts,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("first continue") else {
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
    } = continue_rebase(
        &grit,
        &opts,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("second continue")
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
    assert_head_reflog_records_rebase_finish(wt, tip);
    assert_git_fsck_strict(wt);
}

#[test]
fn rewrite_after_continue_extends_the_head_reflog_chain() {
    let (repo, upstream, onto, tip, orig_tip) = conflict_rebase_fixture();
    let wt = repo.worktree();
    git(wt, &["switch", "topic"]);
    let grit = open_grit(wt);
    let opts = start_opts(upstream, Some(onto), tip);
    assert!(matches!(
        start_rebase(
            &grit,
            &opts,
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::NeverCancel,
        )
        .expect("start"),
        RebaseOutcome::Stopped(_)
    ));
    std::fs::write(wt.join("f.txt"), "resolved\n").expect("resolve");
    git(wt, &["add", "f.txt"]);
    continue_rebase(
        &grit,
        &opts,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("continue");
    let rebased = git_oid(wt, "HEAD");

    let plan = grit_lib::revwalk::history::reword(
        &grit,
        rebased,
        "reworded\n".into(),
        &rebase_opts().rewrite,
    )
    .expect("plan reword");
    grit_lib::revwalk::history::apply_rewrite(
        &grit,
        &plan,
        &opts.apply,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("apply reword");

    let reworded = git_oid(wt, "HEAD");
    assert_ne!(reworded, rebased);
    assert_eq!(git_oid(wt, "HEAD@{1}"), rebased);
    assert_eq!(git_oid(wt, "HEAD@{2}"), orig_tip);
    let log = std::fs::read_to_string(wt.join(".git/logs/HEAD")).expect("HEAD log");
    let last = log.lines().last().expect("entry");
    assert!(last.starts_with(&format!("{} {} ", rebased.to_hex(), reworded.to_hex())));
    assert_git_fsck_strict(wt);
}

#[test]
fn failed_head_reflog_append_keeps_rebase_detached_and_abortable() {
    let (repo, upstream, onto, tip, orig_tip) = conflict_rebase_fixture();
    let wt = repo.worktree();
    git(wt, &["switch", "topic"]);
    let grit = open_grit(wt);
    let opts = start_opts(upstream, Some(onto), tip);
    assert!(matches!(
        start_rebase(
            &grit,
            &opts,
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::NeverCancel,
        )
        .expect("start"),
        RebaseOutcome::Stopped(_)
    ));
    std::fs::write(wt.join("f.txt"), "resolved\n").expect("resolve");
    git(wt, &["add", "f.txt"]);

    let head_log = wt.join(".git/logs/HEAD");
    let saved_log = std::fs::read(&head_log).expect("HEAD log");
    std::fs::remove_file(&head_log).expect("remove HEAD log");
    // An empty directory would be cleared like Git does; one holding a file blocks the log.
    std::fs::create_dir(&head_log).expect("block HEAD log");
    std::fs::write(head_log.join("stray"), "").expect("keep HEAD log blocked");
    continue_rebase(
        &grit,
        &opts,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect_err("HEAD reflog append must fail");

    assert!(
        matches!(
            resolve_head(grit.git_dir()).unwrap(),
            HeadState::Detached { .. }
        ),
        "HEAD must stay detached until the finish is logged"
    );
    assert!(grit.git_dir().join("rebase-merge").is_dir());

    std::fs::remove_dir_all(&head_log).expect("unblock HEAD log");
    std::fs::write(&head_log, saved_log).expect("restore HEAD log");
    abort_rebase(&grit, &test_ident()).expect("abort");
    assert_eq!(git_oid(wt, "topic"), orig_tip);
    assert_eq!(
        resolve_head(grit.git_dir()).unwrap().branch_name(),
        Some("topic")
    );
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
        start_rebase(
            &grit,
            &opts,
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::NeverCancel
        )
        .expect("start"),
        RebaseOutcome::Stopped(_)
    ));
    let before = git_oid(wt, "HEAD");
    assert_eq!(before, onto);
    let RebaseOutcome::Done {
        applied,
        head_change,
        plan,
    } = skip_rebase(
        &grit,
        &opts,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("skip")
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
    assert_head_reflog_records_rebase_finish(wt, tip);
    assert_git_fsck_strict(wt);
}

fn exec_fixture() -> (
    HistoryRepo,
    StartRebaseOptions,
    grit_lib::revwalk::history::RewritePlan,
) {
    use grit_lib::revwalk::history::{plan_rebase, RebasePlan};
    let fixture = HistoryRepo::init();
    let wt = fixture.worktree();
    commit_file(wt, "f", "base\n", "base");
    let upstream = git_oid(wt, "HEAD");
    git(wt, &["switch", "-c", "topic"]);
    commit_file(wt, "one", "one\n", "one");
    commit_file(wt, "two", "two\n", "two");
    let tip = git_oid(wt, "HEAD");
    git(wt, &["switch", "main"]);
    commit_file(wt, "extra", "upstream\n", "main moves");
    let onto = git_oid(wt, "HEAD");
    git(wt, &["switch", "topic"]);
    let opts = start_opts(upstream, Some(onto), tip);
    let RebasePlan::Plan(plan) = plan_rebase(&open_grit(wt), &opts.request, &opts.rebase).unwrap()
    else {
        panic!("replay plan")
    };
    (fixture, opts, plan)
}

fn run_exec(
    wt: &std::path::Path,
    plan: &mut grit_lib::revwalk::history::RewritePlan,
    commands: &[&str],
) -> Result<grit_lib::revwalk::history::ExecReport, grit_lib::revwalk::history::HistoryError> {
    use grit_lib::revwalk::history::{verify_rebase_with_exec, ExecSpec};
    verify_rebase_with_exec(
        &open_grit(wt),
        plan,
        &ExecSpec {
            commands: commands.iter().map(|c| (*c).to_owned()).collect(),
        },
        &rebase_opts().rewrite,
        grit_lib::command_runner::system_command_runner().as_ref(),
    )
}

fn run_dirty_exec(
    wt: &std::path::Path,
    plan: &grit_lib::revwalk::history::RewritePlan,
    command: &str,
) -> grit_lib::revwalk::history::HistoryError {
    run_exec(wt, &mut plan.clone(), &[command, "echo later >> exec-log"]).unwrap_err()
}

#[test]
fn exec_stop_keeps_dirty_index_and_resume_requires_explicit_recovery() {
    use grit_lib::revwalk::history::{HistoryError, RebaseSessionError};
    let (fixture, opts, plan) = exec_fixture();
    let wt = fixture.worktree();
    let original_index = std::fs::read(wt.join(".git/index")).unwrap();
    let first = plan.commits.iter().find_map(|r| r.new).unwrap();
    let err = run_dirty_exec(
        wt,
        &plan,
        "echo staged >> f; git add f; echo unstaged >> f; echo captured",
    );
    match err {
        HistoryError::ExecDirty {
            commit,
            paths,
            runs,
            ..
        } => {
            assert_eq!(commit, first);
            assert_eq!(paths, ["f"]);
            assert_eq!(runs.len(), 1);
            assert_eq!(runs[0].stdout, b"captured\n");
        }
        other => panic!("expected typed exec stop, got {other:?}"),
    }
    assert_eq!(git_oid(wt, "topic"), opts.request.branch_tip);
    assert_eq!(git_oid(wt, "HEAD"), first);
    assert_eq!(git(wt, &["show", ":f"]), "base\nstaged\n");
    let dirty_index = std::fs::read(wt.join(".git/index")).unwrap();
    for skip in [false, true] {
        let repo = open_grit(wt);
        let result = if skip {
            skip_rebase(
                &repo,
                &opts,
                &mut grit_lib::progress::NullProgress,
                &grit_lib::progress::NeverCancel,
            )
        } else {
            continue_rebase(
                &repo,
                &opts,
                &mut grit_lib::progress::NullProgress,
                &grit_lib::progress::NeverCancel,
            )
        };
        assert!(matches!(
            result,
            Err(RebaseSessionError::History(HistoryError::ExecDirty { .. }))
        ));
        assert_eq!(std::fs::read(wt.join(".git/index")).unwrap(), dirty_index);
        assert_eq!(
            std::fs::read(wt.join("f")).unwrap(),
            b"base\nstaged\nunstaged\n"
        );
    }
    assert!(!wt.join("exec-log").exists());
    // A dirty stop is a grit-owned rebase; status must offer grit recovery.
    let state = grit_lib::state::wt_status_get_state(
        &wt.join(".git"),
        &HeadState::Detached { oid: first },
        false,
    )
    .unwrap();
    assert!(state.grit_rebase_in_progress);
    assert_git_fsck_strict(wt);
    abort_rebase(&open_grit(wt), &test_ident()).unwrap();
    assert_eq!(git_oid(wt, "HEAD"), opts.request.branch_tip);
    assert_eq!(
        std::fs::read(wt.join(".git/index")).unwrap(),
        original_index
    );
    assert_eq!(std::fs::read(wt.join("f")).unwrap(), b"base\n");
    assert!(!wt.join(".git/rebase-merge").exists());
    assert_git_fsck_strict(wt);
}

#[test]
fn exec_continue_resumes_after_dirty_command_and_keeps_saved_edits() {
    let (fixture, opts, plan) = exec_fixture();
    let wt = fixture.worktree();
    // Only the first replay commit dirties the worktree; the second is clean.
    run_dirty_exec(wt, &plan, "if ! test -f two; then echo hook-edit >> f; fi");
    git(wt, &["stash", "push", "-m", "exec edits"]);
    let outcome = continue_rebase(
        &open_grit(wt),
        &opts,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .unwrap();
    let RebaseOutcome::ExecDone {
        applied,
        plan: resumed_plan,
        ..
    } = outcome
    else {
        panic!("completed")
    };
    assert_eq!(
        *resumed_plan, plan,
        "recovery retains the complete atomic plan and commit metadata"
    );
    assert_eq!(
        applied.head,
        plan.mapped_tip(opts.request.branch_tip).unwrap()
    );
    assert_eq!(git_oid(wt, "topic"), applied.head);
    assert_eq!(
        git(wt, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/topic"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("exec-log")).unwrap(),
        "later\nlater\n"
    );
    assert!(!wt.join(".git/rebase-merge").exists());
    git(wt, &["stash", "pop"]);
    assert_eq!(std::fs::read(wt.join("f")).unwrap(), b"base\nhook-edit\n");
    assert_git_fsck_strict(wt);
}

#[test]
fn exec_skip_skips_remaining_commands_without_dropping_replayed_commit() {
    let (fixture, opts, plan) = exec_fixture();
    let wt = fixture.worktree();
    run_dirty_exec(wt, &plan, "if ! test -f two; then echo hook-edit >> f; fi");
    git(wt, &["stash", "push"]);
    let RebaseOutcome::ExecDone { applied, .. } = skip_rebase(
        &open_grit(wt),
        &opts,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .unwrap() else {
        panic!("done")
    };
    assert_eq!(
        applied.head,
        plan.mapped_tip(opts.request.branch_tip).unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("exec-log")).unwrap(),
        "later\n"
    );
    assert_eq!(git(wt, &["show", "HEAD:one"]), "one\n");
    git(wt, &["stash", "pop"]);
    assert_eq!(std::fs::read(wt.join("f")).unwrap(), b"base\nhook-edit\n");
    assert_git_fsck_strict(wt);
}

#[test]
fn dirty_success_preservation_matches_system_git() {
    for command in [
        "echo hook-edit >> f",
        "echo hook-edit >> f; git add f",
        "echo staged >> f; git add f; echo unstaged >> f",
        "rm f",
        "git rm -q f; echo new > added; git add added",
    ] {
        let (grit_fixture, opts, plan) = exec_fixture();
        let wt = grit_fixture.worktree();
        run_dirty_exec(wt, &plan, command);
        let (git_fixture, git_opts, _) = exec_fixture();
        let other = git_fixture.worktree();
        let output = std::process::Command::new("git")
            .args([
                "rebase",
                "--onto",
                &git_opts.request.onto.unwrap().to_hex(),
                &git_opts.request.upstream.to_hex(),
                "--exec",
                command,
            ])
            .current_dir(other)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            std::fs::read(wt.join("f")).ok(),
            std::fs::read(other.join("f")).ok()
        );
        assert_eq!(
            git(wt, &["diff", "--binary"]),
            git(other, &["diff", "--binary"])
        );
        assert_eq!(
            git(wt, &["diff", "--cached", "--binary"]),
            git(other, &["diff", "--cached", "--binary"])
        );
        assert_eq!(git_oid(wt, "topic"), opts.request.branch_tip);
        assert_eq!(git_oid(other, "topic"), git_opts.request.branch_tip);
        assert_git_fsck_strict(wt);
        assert_git_fsck_strict(other);
    }
}

#[test]
fn exec_continue_rejects_moved_head_and_abort_restores_original_branch() {
    let (fixture, opts, plan) = exec_fixture();
    let wt = fixture.worktree();
    run_dirty_exec(wt, &plan, "echo hook-edit >> f");
    git(wt, &["add", "f"]);
    git(wt, &["commit", "-qm", "save exec edit"]);
    let saved = git_oid(wt, "HEAD");
    git(wt, &["branch", "saved-exec", &saved.to_hex()]);
    let err = continue_rebase(
        &open_grit(wt),
        &opts,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .unwrap_err();
    assert!(matches!(err, RebaseSessionError::ExecHeadChanged { .. }));
    assert_eq!(git_oid(wt, "HEAD"), saved);
    assert_eq!(git(wt, &["show", "HEAD:f"]), "base\nhook-edit\n");
    abort_rebase(&open_grit(wt), &test_ident()).unwrap();
    assert_eq!(git_oid(wt, "topic"), opts.request.branch_tip);
    assert_eq!(git(wt, &["show", "saved-exec:f"]), "base\nhook-edit\n");
    assert_git_fsck_strict(wt);
}

/// Move `refs/heads/topic` the way #2197 did (`git commit-tree` + `git update-ref`) and return
/// the new tip.
fn move_topic_behind_the_rebase(wt: &std::path::Path) -> ObjectId {
    let moved = git(
        wt,
        &["commit-tree", "-p", "topic", "-m", "moved", "topic^{tree}"],
    );
    git(wt, &["update-ref", "refs/heads/topic", moved.trim()]);
    git_oid(wt, "topic")
}

/// `continue_rebase` and `skip_rebase` both refuse with a typed stale-plan error naming the
/// moved branch, and change nothing.
fn assert_resume_refuses_stale_plan(
    wt: &std::path::Path,
    opts: &StartRebaseOptions,
    moved: ObjectId,
) {
    let head = git_oid(wt, "HEAD");
    let index = std::fs::read(wt.join(".git/index")).unwrap();
    let status = git(wt, &["status", "--porcelain"]);
    for skip in [false, true] {
        let repo = open_grit(wt);
        let result = if skip {
            skip_rebase(
                &repo,
                opts,
                &mut grit_lib::progress::NullProgress,
                &grit_lib::progress::NeverCancel,
            )
        } else {
            continue_rebase(
                &repo,
                opts,
                &mut grit_lib::progress::NullProgress,
                &grit_lib::progress::NeverCancel,
            )
        };
        match result {
            Err(RebaseSessionError::StaleExecPlan {
                name,
                expected,
                actual,
            }) => {
                assert_eq!(name, "refs/heads/topic");
                assert_eq!(expected, opts.request.branch_tip);
                assert_eq!(actual, Some(moved));
            }
            other => panic!("expected a stale exec plan (skip={skip}), got {other:?}"),
        }
        assert_eq!(git_oid(wt, "topic"), moved);
        assert_eq!(git_oid(wt, "HEAD"), head);
        assert_eq!(std::fs::read(wt.join(".git/index")).unwrap(), index);
        assert_eq!(git(wt, &["status", "--porcelain"]), status);
        assert!(wt.join(".git/rebase-merge/grit-exec").is_file());
    }
}

/// Abort resets the moved branch to its pre-rebase tip, reports what it overwrote, logs it,
/// and leaves the exact pre-rebase index and a clean worktree.
fn assert_abort_restores_moved_branch(
    wt: &std::path::Path,
    opts: &StartRebaseOptions,
    moved: ObjectId,
    original_index: &[u8],
) {
    use grit_lib::revwalk::history::PlannedRefUpdate;
    let abort = abort_rebase(&open_grit(wt), &test_ident()).expect("abort");
    assert_eq!(
        abort.overwritten,
        [PlannedRefUpdate {
            name: "refs/heads/topic".into(),
            old: moved,
            new: opts.request.branch_tip,
        }]
    );
    assert_eq!(git_oid(wt, "topic"), opts.request.branch_tip);
    assert_eq!(
        git(wt, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/topic"
    );
    assert_eq!(git_oid(wt, "topic@{1}"), moved, "moved tip stays in reflog");
    assert_eq!(
        git(wt, &["reflog", "-1", "--format=%gs", "refs/heads/topic"]).trim(),
        "rebase (abort)"
    );
    assert_eq!(
        std::fs::read(wt.join(".git/index")).unwrap(),
        original_index
    );
    assert_eq!(git(wt, &["status", "--porcelain"]), "");
    assert!(!wt.join(".git/rebase-merge").exists());
    assert_git_fsck_strict(wt);
}

#[test]
fn exec_dirty_stop_with_moved_branch_refuses_resume_and_abort_restores_it() {
    let (fixture, opts, plan) = exec_fixture();
    let wt = fixture.worktree();
    let original_index = std::fs::read(wt.join(".git/index")).unwrap();
    run_dirty_exec(wt, &plan, "echo hook-edit >> f");
    let moved = move_topic_behind_the_rebase(wt);
    assert_resume_refuses_stale_plan(wt, &opts, moved);
    assert_eq!(std::fs::read(wt.join("f")).unwrap(), b"base\nhook-edit\n");
    // Saving the edits does not make the stale plan resumable either.
    git(wt, &["stash", "push", "-m", "exec edits"]);
    assert_resume_refuses_stale_plan(wt, &opts, moved);
    assert_abort_restores_moved_branch(wt, &opts, moved, &original_index);
}

#[test]
fn exec_conflict_stop_with_moved_branch_refuses_resume_and_abort_restores_it() {
    use grit_lib::revwalk::history::HistoryError;
    let (fixture, opts, mut plan) = exec_fixture();
    let wt = fixture.worktree();
    let original_index = std::fs::read(wt.join(".git/index")).unwrap();
    let err = run_exec(wt, &mut plan, &[&conflicting_amend()]).unwrap_err();
    assert!(matches!(err, HistoryError::ExecConflict { .. }), "{err:?}");
    let moved = move_topic_behind_the_rebase(wt);
    assert_resume_refuses_stale_plan(wt, &opts, moved);
    assert_abort_restores_moved_branch(wt, &opts, moved, &original_index);
}

#[test]
fn exec_abort_with_unmoved_branch_reports_nothing_overwritten() {
    let (fixture, opts, plan) = exec_fixture();
    let wt = fixture.worktree();
    run_dirty_exec(wt, &plan, "echo hook-edit >> f");
    let abort = abort_rebase(&open_grit(wt), &test_ident()).expect("abort");
    assert!(abort.overwritten.is_empty(), "{abort:?}");
    assert_eq!(git_oid(wt, "topic"), opts.request.branch_tip);
}

#[test]
fn conflict_stop_abort_reports_a_branch_moved_during_the_stop() {
    use grit_lib::revwalk::history::PlannedRefUpdate;
    let (repo, upstream, onto, tip, orig_tip) = conflict_rebase_fixture();
    let wt = repo.worktree();
    git(wt, &["switch", "topic"]);
    let RebaseOutcome::Stopped(_) = start_rebase(
        &open_grit(wt),
        &start_opts(upstream, Some(onto), tip),
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("stop") else {
        panic!("expected a conflict stop")
    };
    let moved = move_topic_behind_the_rebase(wt);
    let abort = abort_rebase(&open_grit(wt), &test_ident()).expect("abort");
    assert_eq!(
        abort.overwritten,
        [PlannedRefUpdate {
            name: "refs/heads/topic".into(),
            old: moved,
            new: orig_tip,
        }]
    );
    assert_eq!(git_oid(wt, "topic"), orig_tip);
    assert!(!wt.join(".git/rebase-merge").exists());
    assert_git_fsck_strict(wt);
}

#[test]
fn exec_abort_removes_staged_addition_and_restores_exact_index() {
    let (fixture, opts, plan) = exec_fixture();
    let wt = fixture.worktree();
    let index = std::fs::read(wt.join(".git/index")).unwrap();
    run_dirty_exec(wt, &plan, "git rm -q f; echo new > added; git add added");
    assert_eq!(std::fs::read(wt.join("added")).unwrap(), b"new\n");
    abort_rebase(&open_grit(wt), &test_ident()).unwrap();
    assert_eq!(git_oid(wt, "HEAD"), opts.request.branch_tip);
    assert_eq!(std::fs::read(wt.join(".git/index")).unwrap(), index);
    assert_eq!(std::fs::read(wt.join("f")).unwrap(), b"base\n");
    assert!(!wt.join("added").exists());
    assert_eq!(git(wt, &["status", "--short"]), "");
    assert_git_fsck_strict(wt);
}

#[test]
fn exec_recovery_can_stop_again_at_next_command_and_final_commit() {
    use grit_lib::revwalk::history::HistoryError;
    let (fixture, opts, plan) = exec_fixture();
    let wt = fixture.worktree();
    let commands = ["echo first >> f", "echo second >> f"];
    let mut result = run_exec(wt, &mut plan.clone(), &commands)
        .map(|_| ())
        .map_err(RebaseSessionError::from);
    let commits: Vec<_> = plan.commits.iter().filter_map(|r| r.new).collect();
    for (step, expected_commit) in [commits[0], commits[0], commits[1], commits[1]]
        .into_iter()
        .enumerate()
    {
        let Err(RebaseSessionError::History(HistoryError::ExecDirty {
            commit,
            command,
            runs,
            ..
        })) = result
        else {
            panic!("expected dirty exec stop at step {step}")
        };
        assert_eq!(commit, expected_commit);
        assert_eq!(command, commands[step % 2]);
        assert_eq!(runs.len(), 1);
        assert_eq!(git_oid(wt, "topic"), opts.request.branch_tip);
        git(wt, &["stash", "push", "-m", &format!("exec step {step}")]);
        result = continue_rebase(
            &open_grit(wt),
            &opts,
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::NeverCancel,
        )
        .map(|outcome| {
            if step == 3 {
                assert!(matches!(outcome, RebaseOutcome::ExecDone { .. }));
            }
        });
    }
    result.unwrap();
    assert_eq!(
        git_oid(wt, "topic"),
        plan.mapped_tip(opts.request.branch_tip).unwrap()
    );
    assert_eq!(git(wt, &["stash", "list"]).lines().count(), 4);
    assert!(!wt.join(".git/rebase-merge").exists());
    assert_git_fsck_strict(wt);
}

#[test]
fn exec_failure_during_recovery_rolls_back_and_clears_exec_stop() {
    use grit_lib::revwalk::history::HistoryError;
    let (fixture, opts, plan) = exec_fixture();
    let wt = fixture.worktree();
    let index = std::fs::read(wt.join(".git/index")).unwrap();
    let error = run_exec(
        wt,
        &mut plan.clone(),
        &["echo edit >> f", "echo captured; echo failed >&2; exit 7"],
    )
    .unwrap_err();
    assert!(matches!(error, HistoryError::ExecDirty { .. }));
    git(wt, &["stash", "push"]);
    let error = continue_rebase(
        &open_grit(wt),
        &opts,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .unwrap_err();
    let RebaseSessionError::History(HistoryError::ExecFailed {
        exit_code,
        stdout,
        stderr,
        ..
    }) = error
    else {
        panic!("typed exec failure")
    };
    assert_eq!(exit_code, 7);
    assert_eq!(stdout, b"captured\n");
    assert_eq!(stderr, b"failed\n");
    assert_eq!(git_oid(wt, "HEAD"), opts.request.branch_tip);
    assert_eq!(
        git(wt, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/topic"
    );
    assert_eq!(std::fs::read(wt.join(".git/index")).unwrap(), index);
    assert!(!wt.join(".git/rebase-merge").exists());
    git(wt, &["stash", "pop"]);
    assert_eq!(std::fs::read(wt.join("f")).unwrap(), b"base\nedit\n");
    assert_git_fsck_strict(wt);
}

#[test]
fn flattened_session_drops_merge_and_chains_commits_after_continue() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "f.txt", "base\n", "base");
    git(wt, &["switch", "-c", "topic"]);
    commit_file(wt, "f.txt", "topic change\n", "topic commit");
    git(wt, &["switch", "main"]);
    commit_file(wt, "b.txt", "2\n", "upstream two");
    git(wt, &["switch", "topic"]);
    git(wt, &["merge", "-q", "--no-ff", "--no-edit", "main"]);
    commit_file(wt, "d.txt", "later\n", "after merge");
    let tip = git_oid(wt, "HEAD");
    git(wt, &["switch", "main"]);
    commit_file(wt, "f.txt", "main change\n", "upstream three");
    let upstream = git_oid(wt, "HEAD");
    git(wt, &["switch", "topic"]);

    let git_wt = tempfile::tempdir().expect("git copy");
    std::process::Command::new("cp")
        .args([
            "-a",
            &format!("{}/.", wt.display()),
            &git_wt.path().to_string_lossy(),
        ])
        .status()
        .unwrap();

    let mut opts = start_opts(upstream, None, tip);
    opts.rebase.flatten_leaf_merges = true;
    let grit = open_grit(wt);
    let stop = match start_rebase(
        &grit,
        &opts,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("start")
    {
        RebaseOutcome::Stopped(stop) => stop,
        other => panic!("expected a conflict stop, got {other:?}"),
    };
    assert!(stop.conflicts.iter().any(|c| c.path == "f.txt"));
    std::fs::write(wt.join("f.txt"), "resolved\n").expect("resolve");
    git(wt, &["add", "f.txt"]);
    let grit = open_grit(wt);
    match continue_rebase(
        &grit,
        &opts,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("continue")
    {
        RebaseOutcome::Done { .. } => {}
        other => panic!("expected done after continue, got {other:?}"),
    }

    let out = std::process::Command::new("git")
        .current_dir(git_wt.path())
        .args(["rebase", &upstream.to_hex()])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git rebase");
    assert!(!out.status.success(), "git rebase should stop on conflict");
    std::fs::write(git_wt.path().join("f.txt"), "resolved\n").expect("resolve");
    git(git_wt.path(), &["add", "f.txt"]);
    let cont = std::process::Command::new("git")
        .current_dir(git_wt.path())
        .args(["rebase", "--continue"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_EDITOR", ":")
        .env("EDITOR", ":")
        .output()
        .expect("git rebase --continue");
    assert!(cont.status.success(), "git rebase --continue failed");

    for dir in [wt, git_wt.path()] {
        assert_eq!(
            git(dir, &["log", "--format=%s", "topic"]),
            "after merge\ntopic commit\nupstream three\nupstream two\nbase\n",
            "the merge is dropped and the line ends on upstream in {}",
            dir.display()
        );
    }
    assert_eq!(
        git(wt, &["rev-parse", "topic^{tree}"]),
        git(git_wt.path(), &["rev-parse", "topic^{tree}"]),
        "topic tree matches git rebase"
    );
    assert_eq!(git_oid(wt, "topic~2"), upstream);
    assert_git_fsck_strict(wt);
}

/// `git` with a fixed identity and no user config, for commands run by `--exec`.
const EXEC_GIT: &str = "env GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null \
    GIT_AUTHOR_NAME=T GIT_AUTHOR_EMAIL=t@example.com GIT_COMMITTER_NAME=T \
    GIT_COMMITTER_EMAIL=t@example.com GIT_AUTHOR_DATE='1700000500 +0000' \
    GIT_COMMITTER_DATE='1700000500 +0000' git";

fn amend_command(edit: &str) -> String {
    format!("{edit} && {EXEC_GIT} commit -qa --amend --no-edit")
}

/// Run `git rebase --exec` on a fresh [`exec_fixture`]; returns the fixture and the exit code.
fn git_exec_twin(commands: &[&str]) -> (HistoryRepo, Option<i32>) {
    let (fixture, opts, _) = exec_fixture();
    let onto = opts.request.onto.unwrap().to_hex();
    let upstream = opts.request.upstream.to_hex();
    let mut args = vec!["rebase", "--onto", onto.as_str(), upstream.as_str()];
    for command in commands {
        args.extend(["--exec", command]);
    }
    let code = git_hermetic(fixture.worktree(), &args).status.code();
    (fixture, code)
}

fn git_hermetic(wt: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args(args)
        .current_dir(wt)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_EDITOR", ":")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap()
}

fn apply_exec_plan(
    wt: &std::path::Path,
    opts: &StartRebaseOptions,
    plan: &grit_lib::revwalk::history::RewritePlan,
) {
    grit_lib::revwalk::history::apply_rewrite(
        &open_grit(wt),
        plan,
        &opts.apply,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .unwrap();
}

fn subjects(wt: &std::path::Path, rev: &str) -> String {
    git(wt, &["log", "--format=%s", rev])
}

#[test]
fn exec_amend_is_adopted_and_later_commits_replay_onto_it() {
    let (fixture, opts, mut plan) = exec_fixture();
    let wt = fixture.worktree();
    let planned = plan.clone();
    let command = amend_command("echo fmt >> one");
    let report = run_exec(wt, &mut plan, &[&command]).expect("amending exec completes");
    assert_eq!(report.runs.len(), 2);
    assert_eq!(
        report.runs[0].commit,
        planned.commits[0].new.unwrap(),
        "the first command runs at the planned replay"
    );
    assert_ne!(
        plan, planned,
        "the amended commits replace the planned ones"
    );
    assert_eq!(
        plan.commits[0].change_id, planned.commits[0].change_id,
        "git commit --amend keeps the change-id header"
    );
    assert_eq!(plan.commits[1].change_id, planned.commits[1].change_id);
    assert_eq!(
        git_oid(wt, &format!("{}^", report.runs[1].commit)),
        plan.commits[0].new.unwrap(),
        "the second commit is replayed onto the amended first commit before its command"
    );
    assert_eq!(git_oid(wt, "topic"), opts.request.branch_tip);
    apply_exec_plan(wt, &opts, &plan);

    let tip = plan.mapped_tip(opts.request.branch_tip).unwrap();
    assert_eq!(git_oid(wt, "topic"), tip);
    assert_eq!(
        git(wt, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/topic"
    );
    assert_eq!(git_oid(wt, "topic~1"), plan.commits[0].new.unwrap());
    assert_eq!(git_oid(wt, "topic~2"), opts.request.onto.unwrap());
    assert_eq!(git(wt, &["show", "topic~1:one"]), "one\nfmt\n");
    assert_eq!(git(wt, &["show", "topic:one"]), "one\nfmt\nfmt\n");
    assert_eq!(git(wt, &["show", "topic:two"]), "two\n");
    assert_eq!(subjects(wt, "topic"), "two\none\nmain moves\nbase\n");
    assert_eq!(git(wt, &["status", "--porcelain"]), "");
    assert_git_fsck_strict(wt);

    let (twin, code) = git_exec_twin(&[&command]);
    assert_eq!(code, Some(0));
    for rev in ["topic^{tree}", "topic~1^{tree}"] {
        assert_eq!(git_oid(wt, rev), git_oid(twin.worktree(), rev), "{rev}");
    }
    assert_eq!(subjects(twin.worktree(), "topic"), subjects(wt, "topic"));
}

#[test]
fn exec_message_only_amend_rewords_the_replayed_commit() {
    let (fixture, opts, mut plan) = exec_fixture();
    let wt = fixture.worktree();
    let planned_tip_tree = git_oid(
        wt,
        &format!(
            "{}^{{tree}}",
            plan.mapped_tip(opts.request.branch_tip).unwrap()
        ),
    );
    let command = format!("if ! test -f two; then {EXEC_GIT} commit -q --amend -m reworded; fi");
    run_exec(wt, &mut plan, &[&command]).expect("exec");
    assert_eq!(plan.commits[0].subject.as_deref(), Some("reworded"));
    apply_exec_plan(wt, &opts, &plan);
    assert_eq!(subjects(wt, "topic"), "two\nreworded\nmain moves\nbase\n");
    assert_eq!(git_oid(wt, "topic^{tree}"), planned_tip_tree);
    assert_git_fsck_strict(wt);

    let (twin, code) = git_exec_twin(&[&command]);
    assert_eq!(code, Some(0));
    assert_eq!(subjects(twin.worktree(), "topic"), subjects(wt, "topic"));
}

#[test]
fn exec_later_command_sees_the_amended_tree() {
    let (fixture, opts, mut plan) = exec_fixture();
    let wt = fixture.worktree();
    let amend = amend_command("echo fmt >> one");
    let report = run_exec(wt, &mut plan, &[&amend, "grep -q fmt one"])
        .expect("the second command sees the amended file");
    assert_eq!(report.runs.len(), 4);
    assert_eq!(report.runs[1].commit, plan.commits[0].new.unwrap());
    assert_eq!(report.runs[3].commit, plan.commits[1].new.unwrap());
    apply_exec_plan(wt, &opts, &plan);
    assert_eq!(git(wt, &["show", "topic:one"]), "one\nfmt\nfmt\n");
    assert_git_fsck_strict(wt);
}

#[test]
fn exec_commits_added_on_top_are_kept_below_later_commits() {
    let (fixture, opts, mut plan) = exec_fixture();
    let wt = fixture.worktree();
    let command = format!(
        "if ! test -f two; then echo lock > lock && {EXEC_GIT} add lock && {EXEC_GIT} commit -qm lockfile; fi"
    );
    run_exec(wt, &mut plan, &[&command]).expect("exec");
    apply_exec_plan(wt, &opts, &plan);
    assert_eq!(
        subjects(wt, "topic"),
        "two\nlockfile\none\nmain moves\nbase\n"
    );
    assert_eq!(git_oid(wt, "topic~2"), plan.commits[0].new.unwrap());
    assert_eq!(git(wt, &["show", "topic:lock"]), "lock\n");
    assert_git_fsck_strict(wt);

    let (twin, code) = git_exec_twin(&[&command]);
    assert_eq!(code, Some(0));
    assert_eq!(subjects(twin.worktree(), "topic"), subjects(wt, "topic"));
    assert_eq!(
        git_oid(wt, "topic^{tree}"),
        git_oid(twin.worktree(), "topic^{tree}")
    );
}

#[test]
fn exec_amend_with_leftover_edits_stops_at_the_amended_commit() {
    use grit_lib::revwalk::history::HistoryError;
    let (fixture, opts, mut plan) = exec_fixture();
    let wt = fixture.worktree();
    let command = format!(
        "if ! test -f two; then {} && echo dirt >> f; fi",
        amend_command("echo fmt >> one")
    );
    let err = run_exec(wt, &mut plan, &[&command]).unwrap_err();
    let HistoryError::ExecDirty { commit, paths, .. } = err else {
        panic!("expected a dirty stop, got {err:?}")
    };
    assert_eq!(paths, ["f"], "the amended file is not reported as dirty");
    assert_eq!(git_oid(wt, "HEAD"), commit);
    assert_eq!(git(wt, &["show", "HEAD:one"]), "one\nfmt\n");
    git(wt, &["stash", "push", "-m", "dirt"]);
    let RebaseOutcome::ExecDone { plan: done, .. } = continue_rebase(
        &open_grit(wt),
        &opts,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("continue after saving the edits") else {
        panic!("done")
    };
    assert_eq!(done.commits[0].new, Some(commit));
    assert_eq!(git_oid(wt, "topic~1"), commit);
    assert_eq!(git(wt, &["show", "topic:one"]), "one\nfmt\n");
    assert_eq!(git(wt, &["show", "topic:two"]), "two\n");
    assert!(!wt.join(".git/rebase-merge").exists());
    assert_eq!(
        done.mapped_tip(opts.request.branch_tip),
        Some(git_oid(wt, "topic"))
    );
    assert_git_fsck_strict(wt);
}

#[test]
fn exec_moving_head_elsewhere_rolls_back() {
    use grit_lib::revwalk::history::HistoryError;
    let (fixture, opts, mut plan) = exec_fixture();
    let wt = fixture.worktree();
    let index = std::fs::read(wt.join(".git/index")).unwrap();
    let err = run_exec(wt, &mut plan, &["git checkout -q --detach HEAD~1"]).unwrap_err();
    assert!(
        matches!(err, HistoryError::ExecHeadMoved { head: Some(_), .. }),
        "{err:?}"
    );
    assert_eq!(git_oid(wt, "HEAD"), opts.request.branch_tip);
    assert_eq!(
        git(wt, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/topic"
    );
    assert_eq!(std::fs::read(wt.join(".git/index")).unwrap(), index);
    assert_eq!(git(wt, &["status", "--porcelain"]), "");
    assert!(!wt.join(".git/rebase-merge").exists());
}

/// An amend at the first commit that adds `two` conflicts with the second commit adding it.
fn conflicting_amend() -> String {
    format!(
        "if ! test -f two; then echo amended > two && {EXEC_GIT} add two && {EXEC_GIT} commit -q --amend --no-edit; fi"
    )
}

#[test]
fn exec_amend_conflicting_with_next_commit_stops_resumably_and_matches_git() {
    use grit_lib::revwalk::history::HistoryError;
    let (fixture, opts, mut plan) = exec_fixture();
    let wt = fixture.worktree();
    let command = conflicting_amend();
    let err = run_exec(wt, &mut plan, &[&command, "true"]).unwrap_err();
    let HistoryError::ExecConflict {
        commit,
        conflicts,
        runs,
    } = err
    else {
        panic!("expected an exec conflict stop, got {err:?}")
    };
    let second_original = git_oid(wt, "topic");
    assert_eq!(commit, second_original);
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].path, "two");
    assert_eq!(runs.len(), 2);
    let amended = git_oid(wt, "HEAD");
    assert_eq!(git(wt, &["show", "HEAD:two"]), "amended\n");
    assert_eq!(git_oid(wt, "HEAD~1"), opts.request.onto.unwrap());
    let two = std::fs::read_to_string(wt.join("two")).unwrap();
    assert!(two.contains("<<<<<<<"));
    assert!(
        two.contains(&format!(
            "\n>>>>>>> {} (two)\n",
            &second_original.to_hex()[..7]
        )),
        "{two}"
    );
    assert_eq!(git_oid(wt, "topic"), opts.request.branch_tip);

    let repo = open_grit(wt);
    assert!(matches!(
        continue_rebase(
            &repo,
            &opts,
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::NeverCancel
        ),
        Err(RebaseSessionError::UnmergedPaths)
    ));
    std::fs::write(wt.join("two"), "resolved\n").unwrap();
    git(wt, &["add", "two"]);
    let RebaseOutcome::ExecDone { exec, plan, .. } = continue_rebase(
        &open_grit(wt),
        &opts,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("continue") else {
        panic!("done")
    };
    assert_eq!(exec.runs.len(), 2, "commands run at the resolved commit");
    assert_eq!(plan.commits[0].new, Some(amended));
    assert_eq!(git_oid(wt, "topic~1"), amended);
    assert_eq!(git(wt, &["show", "topic:two"]), "resolved\n");
    assert_eq!(subjects(wt, "topic"), "two\none\nmain moves\nbase\n");
    assert_eq!(
        git(wt, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/topic"
    );
    assert_eq!(git(wt, &["status", "--porcelain"]), "");
    assert!(!wt.join(".git/rebase-merge").exists());
    assert_git_fsck_strict(wt);

    let (twin, code) = git_exec_twin(&[&command]);
    assert_eq!(code, Some(1), "git stops on the same conflict");
    let other = twin.worktree();
    std::fs::write(other.join("two"), "resolved\n").unwrap();
    git(other, &["add", "two"]);
    let cont = git_hermetic(other, &["rebase", "--continue"]);
    assert!(cont.status.success(), "{cont:?}");
    for rev in ["topic^{tree}", "topic~1^{tree}"] {
        assert_eq!(git_oid(wt, rev), git_oid(other, rev), "{rev}");
    }
    assert_eq!(subjects(other, "topic"), subjects(wt, "topic"));
}

#[test]
fn exec_conflict_stop_skip_drops_the_commit_and_abort_restores() {
    use grit_lib::revwalk::history::HistoryError;
    for skip in [true, false] {
        let (fixture, opts, mut plan) = exec_fixture();
        let wt = fixture.worktree();
        let index = std::fs::read(wt.join(".git/index")).unwrap();
        let err = run_exec(wt, &mut plan, &[&conflicting_amend()]).unwrap_err();
        assert!(matches!(err, HistoryError::ExecConflict { .. }), "{err:?}");
        let amended = git_oid(wt, "HEAD");
        if skip {
            let RebaseOutcome::ExecDone { plan, .. } = skip_rebase(
                &open_grit(wt),
                &opts,
                &mut grit_lib::progress::NullProgress,
                &grit_lib::progress::NeverCancel,
            )
            .expect("skip") else {
                panic!("done")
            };
            assert_eq!(plan.commits[1].new, None);
            assert_eq!(git_oid(wt, "topic"), amended);
            assert_eq!(subjects(wt, "topic"), "one\nmain moves\nbase\n");
        } else {
            abort_rebase(&open_grit(wt), &test_ident()).unwrap();
            assert_eq!(git_oid(wt, "topic"), opts.request.branch_tip);
            assert_eq!(git_oid(wt, "HEAD"), opts.request.branch_tip);
            assert_eq!(std::fs::read(wt.join(".git/index")).unwrap(), index);
            assert_eq!(std::fs::read(wt.join("two")).unwrap(), b"two\n");
        }
        assert_eq!(git(wt, &["status", "--porcelain"]), "");
        assert!(!wt.join(".git/rebase-merge").exists());
        assert_git_fsck_strict(wt);
    }
}

#[test]
fn exec_resume_keeps_the_rewrite_options_the_rebase_started_with() {
    use grit_lib::revwalk::history::{
        plan_rebase, verify_rebase_with_exec, ExecSpec, HistoryError, RebasePlan,
    };
    let fixture = HistoryRepo::init();
    let wt = fixture.worktree();
    commit_file(wt, "f", "base\n", "base");
    let upstream = git_oid(wt, "HEAD");
    git(wt, &["switch", "-c", "topic"]);
    commit_file(wt, "one", "one\n", "one");
    commit_file(wt, "two", "B\n", "two");
    commit_file(wt, "two", "C\n", "three");
    let tip = git_oid(wt, "HEAD");
    git(wt, &["switch", "main"]);
    commit_file(wt, "up", "up\n", "up");
    let onto = git_oid(wt, "HEAD");
    git(wt, &["switch", "topic"]);

    let resume_opts = start_opts(upstream, Some(onto), tip);
    let mut keep = resume_opts.clone();
    keep.rebase.rewrite.empty = EmptyPolicy::Keep;
    let RebasePlan::Plan(mut plan) =
        plan_rebase(&open_grit(wt), &keep.request, &keep.rebase).unwrap()
    else {
        panic!("replay plan")
    };
    let err = verify_rebase_with_exec(
        &open_grit(wt),
        &mut plan,
        &ExecSpec {
            commands: vec![conflicting_amend()],
        },
        &keep.rebase.rewrite,
        grit_lib::command_runner::system_command_runner().as_ref(),
    )
    .unwrap_err();
    assert!(matches!(err, HistoryError::ExecConflict { .. }), "{err:?}");
    std::fs::write(wt.join("two"), "C\n").unwrap();
    git(wt, &["add", "two"]);
    assert_eq!(resume_opts.rebase.rewrite.empty, EmptyPolicy::Drop);
    let RebaseOutcome::ExecDone { .. } = continue_rebase(
        &open_grit(wt),
        &resume_opts,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("continue") else {
        panic!("done")
    };
    assert_eq!(
        subjects(wt, "topic"),
        "three\ntwo\none\nup\nbase\n",
        "the now-empty third commit is kept, as the rebase was started with --empty keep"
    );
    assert_eq!(git(wt, &["show", "topic:two"]), "C\n");
    assert_git_fsck_strict(wt);
}

#[test]
fn exec_conflict_skip_runs_the_pending_command_like_git() {
    use grit_lib::revwalk::history::HistoryError;
    let logs = tempfile::tempdir().unwrap();
    let command_logging_to =
        |log: &std::path::Path| format!("echo run >> '{}'; {}", log.display(), conflicting_amend());
    let count = |log: &std::path::Path| {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .count()
    };

    let grit_log = logs.path().join("grit");
    let (fixture, opts, mut plan) = exec_fixture();
    let wt = fixture.worktree();
    let err = run_exec(wt, &mut plan, &[&command_logging_to(&grit_log)]).unwrap_err();
    assert!(matches!(err, HistoryError::ExecConflict { .. }), "{err:?}");
    assert_eq!(count(&grit_log), 1);
    let amended = git_oid(wt, "HEAD");
    let RebaseOutcome::ExecDone { exec, plan, .. } = skip_rebase(
        &open_grit(wt),
        &opts,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("skip") else {
        panic!("done")
    };
    assert_eq!(count(&grit_log), 2, "the skipped pick's command still runs");
    assert_eq!(exec.runs.len(), 1);
    assert_eq!(exec.runs[0].commit, amended);
    assert_eq!(plan.commits[1].new, None);
    assert_eq!(git_oid(wt, "topic"), amended);

    let git_log = logs.path().join("git");
    let (twin, code) = git_exec_twin(&[&command_logging_to(&git_log)]);
    assert_eq!(code, Some(1));
    assert_eq!(count(&git_log), 1);
    let skip = git_hermetic(twin.worktree(), &["rebase", "--skip"]);
    assert!(skip.status.success(), "{skip:?}");
    assert_eq!(count(&git_log), 2);
    assert_eq!(subjects(twin.worktree(), "topic"), subjects(wt, "topic"));
    assert_eq!(
        git_oid(wt, "topic^{tree}"),
        git_oid(twin.worktree(), "topic^{tree}")
    );
    assert_git_fsck_strict(wt);
}

#[test]
fn exec_conflict_recovery_write_failure_rolls_back() {
    let (fixture, opts, mut plan) = exec_fixture();
    let wt = fixture.worktree();
    let index = std::fs::read(wt.join(".git/index")).unwrap();
    let command = format!(
        "if ! test -f two; then echo amended > two && {EXEC_GIT} add two && \
         {EXEC_GIT} commit -q --amend --no-edit && mkdir -p .git/rebase-merge && \
         touch .git/rebase-merge/grit-exec.lock; fi"
    );
    let err = run_exec(wt, &mut plan, &[&command]).unwrap_err();
    assert!(err.to_string().contains("grit-exec.lock"), "{err}");
    assert_eq!(
        git(wt, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/topic"
    );
    assert_eq!(git_oid(wt, "topic"), opts.request.branch_tip);
    assert_eq!(std::fs::read(wt.join(".git/index")).unwrap(), index);
    assert_eq!(git(wt, &["ls-files", "-u"]), "");
    assert_eq!(std::fs::read(wt.join("two")).unwrap(), b"two\n");
    assert_eq!(git(wt, &["status", "--porcelain"]), "");
    for name in ["grit-exec", "orig-head", "head-name"] {
        assert!(!wt.join(".git/rebase-merge").join(name).exists(), "{name}");
    }
    assert_git_fsck_strict(wt);
}

#[test]
fn rebase_merge_replay_one_conflict_keeps_merge_parents_issue_2245() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    git(wt, &["config", "grit.changeIds", "false"]);
    commit_file(wt, "base.txt", "base\n", "base");
    git(wt, &["switch", "-qc", "side"]);
    commit_file(wt, "side.txt", "s\n", "side: a");
    commit_file(wt, "side.txt", "s\ns2\n", "side: b");
    git(wt, &["switch", "-qc", "feat", "main"]);
    commit_file(wt, "shared.txt", "mine\n", "feat: a (shared)");
    commit_file(wt, "c.txt", "c\n", "feat: c");
    git(wt, &["merge", "-q", "--no-ff", "-m", "Merge side", "side"]);
    commit_file(wt, "feat.txt", "f2\n", "feat: b");
    git(wt, &["switch", "-q", "main"]);
    commit_file(wt, "shared.txt", "theirs\n", "main: shared");
    git(wt, &["switch", "-q", "feat"]);
    let fork = git_oid(wt, "main~1");
    let onto = git_oid(wt, "main");
    let tip = git_oid(wt, "feat");
    let opts = start_opts(fork, Some(onto), tip);

    let RebaseOutcome::Stopped(first) = start_rebase(
        &open_grit(wt),
        &opts,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("start") else {
        panic!("expected conflict on feat: a");
    };
    assert_eq!(first.conflicts.len(), 1);
    assert_eq!(first.conflicts[0].path, "shared.txt");

    std::fs::write(wt.join("shared.txt"), "resolved1\n").expect("resolve");
    git(wt, &["add", "shared.txt"]);
    let outcome = continue_rebase(
        &open_grit(wt),
        &opts,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("continue");
    assert!(
        matches!(
            outcome,
            RebaseOutcome::Done { .. } | RebaseOutcome::ExecDone { .. }
        ),
        "expected rebase to finish after one conflict, got {outcome:?}"
    );

    let merge_oid = git_oid(wt, "feat~1");
    let merge =
        grit_lib::objects::parse_commit(&open_grit(wt).odb().read(&merge_oid).expect("read").data)
            .expect("parse merge");
    assert_eq!(merge.parents.len(), 2, "Merge side keeps two parents");
    let tree_listing = git(wt, &["ls-tree", "-r", "--name-only", "HEAD"]);
    let files: Vec<_> = tree_listing
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    assert!(
        files.contains(&"side.txt"),
        "side.txt must remain: {files:?}"
    );
    assert!(
        !git_merge_base_is_ancestor(wt, "feat", "side"),
        "side ref must not absorb feat commits"
    );
    assert_git_fsck_strict(wt);
}

fn rebase_apply_with_record() -> grit_lib::revwalk::history::ApplyOptions {
    grit_lib::revwalk::history::ApplyOptions {
        identity: test_ident(),
        reflog_message: "rebase".into(),
        record_operation: Some("rebase".into()),
        hooks: grit_lib::PostOperationHooks::Run,
    }
}

fn undo_opts() -> grit_lib::revwalk::history::ApplyOptions {
    grit_lib::revwalk::history::ApplyOptions {
        identity: test_ident(),
        reflog_message: "history undo".into(),
        record_operation: None,
        hooks: grit_lib::PostOperationHooks::Run,
    }
}

fn finish_conflict_rebase_with_record(
    wt: &std::path::Path,
    start: &StartRebaseOptions,
) -> (ObjectId, ObjectId) {
    let orig_tip = git_oid(wt, "HEAD");
    match start_rebase(
        &open_grit(wt),
        start,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("start")
    {
        RebaseOutcome::Stopped(_) => {}
        other => panic!("expected conflict stop, got {other:?}"),
    }
    std::fs::write(wt.join("f.txt"), "resolved\n").expect("resolve");
    git(wt, &["add", "f.txt"]);
    let grit = open_grit(wt);
    match continue_rebase(
        &grit,
        start,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("continue")
    {
        RebaseOutcome::Done { .. } => {}
        other => panic!("expected done, got {other:?}"),
    }
    (orig_tip, git_oid(wt, "HEAD"))
}

/// Issue #2274: history-last after `--continue` must describe the reattached branch, not the
/// detached conflict stop, so `history undo` can invert the rebase.
#[test]
fn continue_after_conflict_records_history_last_for_undo_issue_2274() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "f.txt", "base\n", "base");
    let upstream = git_oid(wt, "HEAD");
    git(wt, &["switch", "-c", "topic"]);
    commit_file(wt, "f.txt", "topic change\n", "topic commit");
    let tip = git_oid(wt, "HEAD");
    let orig_tip = tip;
    git(wt, &["switch", "main"]);
    commit_file(wt, "f.txt", "main change\n", "main commit");
    let onto = git_oid(wt, "HEAD");
    git(wt, &["switch", "topic"]);

    let start = StartRebaseOptions {
        request: RebaseRequest {
            upstream,
            onto: Some(onto),
            branch_tip: tip,
        },
        rebase: rebase_opts(),
        apply: rebase_apply_with_record(),
        pre_rebase: grit_lib::revwalk::history::PreRebase::Skip,
    };
    let (before, after) = finish_conflict_rebase_with_record(wt, &start);
    assert_eq!(before, orig_tip);
    assert_ne!(before, after);

    let grit = open_grit(wt);
    let record = read_history_last(&grit).expect("read").expect("record");
    assert_eq!(record.operation, "rebase");
    assert_eq!(record.head_ref.as_deref(), Some("refs/heads/topic"));
    assert_eq!(record.head_old_commit, Some(orig_tip));
    assert_eq!(record.head_new_commit, Some(after));
    assert_ne!(record.head_old_commit, record.head_new_commit);

    undo_last_rewrite(&grit, &undo_opts(), false).expect("undo");
    assert_eq!(git_oid(wt, "topic"), orig_tip);
    assert_eq!(
        git(wt, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/topic"
    );
    undo_last_rewrite(&grit, &undo_opts(), false).expect("redo");
    assert_eq!(git_oid(wt, "topic"), after);
    assert_git_fsck_strict(wt);
}

#[test]
fn skip_after_conflict_records_history_last_for_undo_issue_2274() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "f.txt", "base\n", "base");
    let upstream = git_oid(wt, "HEAD");
    git(wt, &["switch", "-c", "topic"]);
    commit_file(wt, "f.txt", "topic change\n", "topic commit");
    commit_file(wt, "f2.txt", "second\n", "topic second");
    let tip = git_oid(wt, "HEAD");
    let orig_tip = git_oid(wt, "topic");
    git(wt, &["switch", "main"]);
    commit_file(wt, "f.txt", "main change\n", "main commit");
    let onto = git_oid(wt, "HEAD");
    git(wt, &["switch", "topic"]);

    let start = StartRebaseOptions {
        request: RebaseRequest {
            upstream,
            onto: Some(onto),
            branch_tip: tip,
        },
        rebase: rebase_opts(),
        apply: rebase_apply_with_record(),
        pre_rebase: grit_lib::revwalk::history::PreRebase::Skip,
    };
    match start_rebase(
        &open_grit(wt),
        &start,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("start")
    {
        RebaseOutcome::Stopped(_) => {}
        other => panic!("expected stop, got {other:?}"),
    }
    let stop_head = git_oid(wt, "HEAD");
    let grit = open_grit(wt);
    match skip_rebase(
        &grit,
        &start,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("skip")
    {
        RebaseOutcome::Done { applied, .. } => {
            let final_tip = git_oid(wt, "topic");
            assert_ne!(final_tip, stop_head);
            assert_ne!(final_tip, orig_tip);
            assert_eq!(applied.head, final_tip);
        }
        other => panic!("expected done, got {other:?}"),
    }

    let grit = open_grit(wt);
    let record = read_history_last(&grit).expect("read").expect("record");
    assert_eq!(record.head_ref.as_deref(), Some("refs/heads/topic"));
    assert_eq!(record.head_old_commit, Some(orig_tip));
    assert_eq!(record.head_new_commit, Some(git_oid(wt, "topic")));

    undo_last_rewrite(&grit, &undo_opts(), false).expect("undo");
    assert_eq!(git_oid(wt, "topic"), orig_tip);
    assert_git_fsck_strict(wt);
}

#[test]
fn stacked_continue_after_conflict_undo_restores_all_branch_tips_issue_2274() {
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
    git(wt, &["switch", "stacked"]);

    let grit = open_grit(wt);
    let mut opts = rebase_opts();
    opts.rewrite.scope = RefScope::Branches;
    let start = StartRebaseOptions {
        request: RebaseRequest {
            upstream: oids["u"],
            onto: Some(onto),
            branch_tip,
        },
        rebase: opts,
        apply: rebase_apply_with_record(),
        pre_rebase: grit_lib::revwalk::history::PreRebase::Skip,
    };
    let main_before = resolve_ref(grit.git_dir(), "refs/heads/main").unwrap();
    let stacked_before = resolve_ref(grit.git_dir(), "refs/heads/stacked").unwrap();

    match start_rebase(
        &grit,
        &start,
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("start")
    {
        RebaseOutcome::Stopped(_) => {
            std::fs::write(wt.join("shared.txt"), "resolved shared\n").expect("resolve");
            git(wt, &["add", "shared.txt"]);
            let grit = open_grit(wt);
            match continue_rebase(
                &grit,
                &start,
                &mut grit_lib::progress::NullProgress,
                &grit_lib::progress::NeverCancel,
            )
            .expect("continue")
            {
                RebaseOutcome::Done { .. } => {}
                other => panic!("expected done, got {other:?}"),
            }
        }
        other => panic!("expected conflict stop, got {other:?}"),
    }

    assert_ne!(
        resolve_ref(grit.git_dir(), "refs/heads/main").unwrap(),
        main_before
    );
    assert_ne!(
        resolve_ref(grit.git_dir(), "refs/heads/stacked").unwrap(),
        stacked_before
    );

    let grit = open_grit(wt);
    let record = read_history_last(&grit).expect("read").expect("record");
    assert_eq!(record.head_ref.as_deref(), Some("refs/heads/stacked"));
    assert_eq!(record.head_old_commit, Some(stacked_before));

    undo_last_rewrite(&grit, &undo_opts(), false).expect("undo");
    assert_eq!(
        resolve_ref(grit.git_dir(), "refs/heads/main").unwrap(),
        main_before
    );
    assert_eq!(
        resolve_ref(grit.git_dir(), "refs/heads/stacked").unwrap(),
        stacked_before
    );
    assert_eq!(
        git(wt, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/stacked"
    );
    assert_git_fsck_strict(wt);
}
