//! Issue #2145: status, merge, pick and rebase describe one conflict with one model.
//!
//! `main` deletes `f` and `feat` modifies it. Every operation that stops on that conflict,
//! and `status` afterwards, must report the same [`Conflict`], matching the `DU` that
//! `git status --porcelain=v2` shows for the index grit wrote.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::operation::{Conflict, ConflictKind};
use grit_lib::porcelain::merge::{integrate_branch, IntegrateBranchOutcome};
use grit_lib::porcelain::replay::ReplayDirection;
use grit_lib::porcelain::replay_sequence::{
    start_replay_sequence, ReplayOptions, ReplaySequenceRequest, SequenceOutcome,
};
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    start_rebase, ApplyOptions, EmptyPolicy, RebaseOntoOptions, RebaseOutcome, RebaseRequest,
    RefScope, RewriteOptions, StartRebaseOptions,
};
use grit_test_support::git;

const IDENT: &str = "Test User <t@example.com> 1700000000 +0000";

struct Fixture {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    main: ObjectId,
    feat: ObjectId,
}

fn rev(root: &Path, spec: &str) -> ObjectId {
    ObjectId::from_hex(git(root, &["rev-parse", spec]).trim()).expect("oid")
}

/// `main` deletes `f`; `feat` modifies it. `HEAD` is `checkout`.
fn modify_delete_repo(checkout: &str) -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.name", "T"]);
    git(&root, &["config", "user.email", "t@e.com"]);
    git(&root, &["config", "grit.changeIds", "false"]);
    fs::write(root.join("f"), "1\n").expect("write f");
    git(&root, &["add", "f"]);
    git(&root, &["commit", "-qm", "base"]);
    git(&root, &["switch", "-qc", "feat"]);
    fs::write(root.join("f"), "1\n2\n").expect("modify f");
    git(&root, &["commit", "-qam", "feat"]);
    git(&root, &["switch", "-q", "main"]);
    git(&root, &["rm", "-q", "f"]);
    git(&root, &["commit", "-qm", "delete f"]);
    git(&root, &["switch", "-q", checkout]);
    Fixture {
        main: rev(&root, "main"),
        feat: rev(&root, "feat"),
        _dir: dir,
        root,
    }
}

fn open(root: &Path) -> Repository {
    Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open")
}

fn status_conflicts(repo: &Repository) -> Vec<Conflict> {
    status(
        repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status")
    .conflict_details
}

fn deleted_by_us() -> Vec<Conflict> {
    vec![Conflict {
        path: "f".into(),
        kind: ConflictKind::DeletedByUs,
    }]
}

/// The operation's conflicts, `status`, and system git all agree on the index grit wrote.
fn assert_one_model(root: &Path, from_operation: &[Conflict]) {
    assert_eq!(from_operation, deleted_by_us());
    assert_eq!(status_conflicts(&open(root)), from_operation);
    let porcelain = git(root, &["status", "--porcelain=v2", "--", "f"]);
    assert!(porcelain.starts_with("u DU "), "{porcelain}");
}

#[test]
fn rebase_stop_and_status_report_the_same_conflict() {
    let fx = modify_delete_repo("feat");
    let repo = open(&fx.root);
    let opts = StartRebaseOptions {
        request: RebaseRequest {
            upstream: fx.main,
            onto: None,
            branch_tip: fx.feat,
        },
        rebase: RebaseOntoOptions {
            rewrite: RewriteOptions {
                scope: RefScope::Head,
                empty: EmptyPolicy::Drop,
                committer: IDENT.parse().expect("committer"),
                preserve_committer: false,
                sign: false,
            },
            skip_applied: false,
            flatten_leaf_merges: false,
            favor: grit_lib::diff::merge_file::MergeFavor::default(),
            drop: Vec::new(),
        },
        apply: ApplyOptions {
            identity: IDENT.parse().expect("identity"),
            reflog_message: "rebase".into(),
            record_operation: None,
            hooks: grit_lib::PostOperationHooks::Run,
        },
        pre_rebase: grit_lib::revwalk::history::PreRebase::Skip,
    };
    let RebaseOutcome::Stopped(stop) =
        start_rebase(&repo, &opts, &mut NullProgress, &NeverCancel).expect("rebase")
    else {
        panic!("expected the rebase to stop on f");
    };
    assert_one_model(&fx.root, &stop.conflicts);
}

#[test]
fn merge_conflicts_and_status_report_the_same_conflict() {
    let fx = modify_delete_repo("main");
    let repo = open(&fx.root);
    let IntegrateBranchOutcome::Conflicts { details, .. } = integrate_branch(
        &repo,
        "refs/heads/main",
        fx.main,
        fx.feat,
        "feat",
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("merge") else {
        panic!("expected merge conflicts");
    };
    assert_one_model(&fx.root, &details);
}

#[test]
fn pick_conflicts_and_status_report_the_same_conflict() {
    let fx = modify_delete_repo("main");
    let repo = open(&fx.root);
    let req = ReplaySequenceRequest {
        commits: vec![fx.feat],
        direction: ReplayDirection::Pick,
        committer: IDENT.parse().expect("committer"),
        options: ReplayOptions::default(),
    };
    let SequenceOutcome::Conflicts { conflicts, .. } =
        start_replay_sequence(&repo, &req, &mut NullProgress, &NeverCancel).expect("pick")
    else {
        panic!("expected the pick to stop on f");
    };
    assert_one_model(&fx.root, &conflicts);
}

#[test]
fn status_reads_a_git_rebase_stop_the_same_way() {
    let fx = modify_delete_repo("feat");
    let out = grit_test_support::git_cmd(&["rebase", "main"])
        .in_dir(&fx.root)
        .exec();
    assert_ne!(out.status, Some(0), "git rebase should stop on f");
    assert_eq!(status_conflicts(&open(&fx.root)), deleted_by_us());
}
