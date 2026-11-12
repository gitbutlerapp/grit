//! After a cancelled grit commit, stash, merge or rebase, the system `git` sees the
//! repository exactly as before the call, and the uncancelled operation still round-trips
//! (issue #2142).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;

use crate::support::{assert_git_fsck_strict, git, open_grit_repo};

use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::commit::{CommitRequest, CommitScope};
use grit_lib::progress::{CancelToken, NullProgress, ProgressPhase, ProgressSink};
use grit_lib::revwalk::history::{
    ApplyOptions, RebaseOntoOptions, RebaseRequest, RefScope, RewriteOptions, StartRebaseOptions,
};
use grit_lib::stash::{StashMode, StashPushOptions};

const IDENT: &str = "Cancel Test <cancel@example.com> 1700000000 +0000";
const FILES: usize = 200;

/// Asks to stop once `limit` units of `phase` were reported.
struct CancelAfter {
    phase: ProgressPhase,
    limit: u64,
    active: Option<ProgressPhase>,
    seen: u64,
    flag: CancelToken,
}

impl CancelAfter {
    fn new(phase: ProgressPhase, limit: u64) -> Self {
        Self {
            phase,
            limit,
            active: None,
            seen: 0,
            flag: CancelToken::new(),
        }
    }
}

impl ProgressSink for CancelAfter {
    fn start(&mut self, phase: ProgressPhase, _total: Option<u64>) {
        self.active = Some(phase);
    }

    fn inc(&mut self, units: u64) {
        if self.active == Some(self.phase) {
            self.seen += units;
            if self.seen >= self.limit {
                self.flag.cancel();
            }
        }
    }
}

fn write_files(root: &Path, dir: &str, contents: &str) {
    fs::create_dir_all(root.join(dir)).expect("mkdir");
    for i in 0..FILES {
        fs::write(
            root.join(format!("{dir}/f{i:04}.txt")),
            format!("{contents} {i}\n"),
        )
        .expect("write");
    }
}

fn git_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main"]);
    git(tmp.path(), &["config", "user.name", "Cancel Test"]);
    git(tmp.path(), &["config", "user.email", "cancel@example.com"]);
    write_files(tmp.path(), "d", "base");
    git(tmp.path(), &["add", "."]);
    git(tmp.path(), &["commit", "-qm", "base"]);
    tmp
}

fn rev(root: &Path, spec: &str) -> String {
    git(root, &["rev-parse", spec]).trim().to_owned()
}

fn oid(root: &Path, spec: &str) -> ObjectId {
    ObjectId::from_hex(&rev(root, spec)).expect("oid")
}

fn status(root: &Path) -> String {
    git(root, &["status", "--porcelain=v1"])
}

fn commit_req(message: &str) -> CommitRequest {
    {
        let mut req = CommitRequest::new(message.to_owned());
        req.author = Some(IDENT.parse().expect("signature"));
        req.committer = Some(IDENT.parse().expect("signature"));
        req
    }
}

#[test]
fn cancelled_auto_stage_commit_leaves_git_view_unchanged() {
    let tmp = git_repo();
    let root = tmp.path();
    write_files(root, "d", "changed");
    let head = rev(root, "HEAD");
    let status_before = status(root);
    assert_eq!(status_before.lines().count(), FILES);

    let repo = open_grit_repo(root);
    let mut sink = CancelAfter::new(ProgressPhase::StagingFiles, 10);
    let cancel = sink.flag.clone();
    let err = repo
        .commits()
        .create(
            &grit_lib::commits::CommitOptions::new(commit_req("change"), CommitScope::AutoStage),
            &mut sink,
            &cancel,
        )
        .expect_err("cancelled");
    assert!(err.is_cancelled(), "got {err:?}");
    assert_eq!(rev(root, "HEAD"), head);
    assert_eq!(status(root), status_before, "nothing became staged");
    assert_git_fsck_strict(root);
    repo.commits()
        .create(
            &grit_lib::commits::CommitOptions::new(commit_req("change"), CommitScope::AutoStage),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("commit");
    assert_eq!(rev(root, "HEAD~1"), head);
    assert_eq!(status(root), "");
    assert_git_fsck_strict(root);
}

#[test]
fn cancelled_stash_push_leaves_no_entry_and_git_pops_a_finished_one() {
    let tmp = git_repo();
    let root = tmp.path();
    write_files(root, "d", "changed");
    let status_before = status(root);
    let repo = open_grit_repo(root);
    let opts = StashPushOptions {
        message: Some("wip".into()),
        include_untracked: false,
        identity: IDENT.parse().expect("signature"),
        selection: None,
        pathspecs: Vec::new(),
        dry_run: false,
        mode: StashMode::default(),
    };

    let mut sink = CancelAfter::new(ProgressPhase::StagingFiles, 10);
    let cancel = sink.flag.clone();
    let err = repo
        .stash()
        .push(&opts, &mut sink, &cancel)
        .expect_err("cancelled");
    assert!(
        matches!(err, grit_lib::stash::StashError::Cancelled),
        "got {err:?}"
    );
    assert_eq!(git(root, &["stash", "list"]), "");
    assert_eq!(status(root), status_before);
    assert_git_fsck_strict(root);

    repo.stash()
        .push(&opts, &mut NullProgress, &CancelToken::never())
        .expect("push")
        .expect("stashed");
    assert_eq!(git(root, &["stash", "list"]).lines().count(), 1);
    assert_eq!(status(root), "");
    git(root, &["stash", "pop", "-q"]);
    assert_eq!(status(root), status_before);
    assert_git_fsck_strict(root);
}

#[test]
fn cancelled_merge_and_rebase_keep_branches_where_git_left_them() {
    let tmp = git_repo();
    let root = tmp.path();
    let base = oid(root, "HEAD");
    git(root, &["switch", "-q", "-c", "side"]);
    write_files(root, "side", "side");
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "side"]);
    let side = oid(root, "HEAD");
    git(root, &["switch", "-q", "main"]);

    let repo = open_grit_repo(root);
    let mut sink = CancelAfter::new(ProgressPhase::CheckingOutFiles, 5);
    let cancel = sink.flag.clone();
    let err = integrate_branch(
        &repo,
        "refs/heads/main",
        base,
        side,
        "side",
        &mut sink,
        &cancel,
    )
    .expect_err("cancelled");
    assert!(matches!(err, Error::Cancelled), "got {err:?}");
    assert_eq!(oid(root, "main"), base);
    assert_git_fsck_strict(root);
    git(root, &["reset", "-q", "--hard"]);
    git(root, &["clean", "-qfd"]);

    git(root, &["switch", "-q", "-c", "topic"]);
    fs::write(root.join("topic.txt"), "topic\n").expect("write");
    git(root, &["add", "topic.txt"]);
    git(root, &["commit", "-qm", "topic"]);
    let topic = oid(root, "HEAD");
    let start = StartRebaseOptions::new(
        RebaseRequest::new(side, topic),
        RebaseOntoOptions::new(
            RewriteOptions::new(IDENT.parse().expect("signature"))
                .with_scope(RefScope::Head)
                .with_sign(false),
        )
        .with_skip_applied(false),
        ApplyOptions::new(IDENT.parse().expect("signature"), "rebase")
            .with_hooks(grit_lib::command_runner::hooks::PostOperationHooks::Skip),
    );
    let repo = open_grit_repo(root);
    let mut sink = CancelAfter::new(ProgressPhase::CheckingOutFiles, 5);
    let cancel = sink.flag.clone();
    let err = repo
        .history()
        .start_rebase(&start, repo.command_runner().as_ref(), &mut sink, &cancel)
        .expect_err("cancelled");
    assert!(err.is_cancelled(), "got {err:?}");
    assert_eq!(oid(root, "topic"), topic);
    assert_eq!(status(root), "", "rollback leaves a clean work tree");
    assert!(!root.join(".git/rebase-merge").exists());
    assert_git_fsck_strict(root);

    repo.history()
        .start_rebase(
            &start,
            repo.command_runner().as_ref(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("rebase");
    assert_eq!(rev(root, "topic~1"), side.to_hex());
    assert_eq!(status(root), "");
    assert_git_fsck_strict(root);
}
