//! Issue #2155: dropping a path's conflict stages must not let the conflict markers be committed.
//!
//! `restore_paths` with `--staged` alone refuses unmerged paths after a merge, pick, revert,
//! rebase and stash pop, and the auto-staging commit refuses a file whose stages were already
//! dropped (by system `git restore --staged`) while its worktree copy still has markers.

use std::fs;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::commit::{commit_with_scope, CommitRequest, CommitScope};
use grit_lib::porcelain::merge::{integrate_branch, IntegrateBranchOutcome};
use grit_lib::porcelain::restore::{restore_paths, RestoreOptions, RestoreSource};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;
use grit_test_support::{git, git_cmd};

/// Operation files that must survive a refused restore or commit.
const STATE_FILES: [&str; 6] = [
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "rebase-merge",
    "sequencer",
    "ORIG_HEAD",
];

fn init(root: &Path) {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.test"]);
}

fn commit_file(root: &Path, contents: &str, message: &str) {
    fs::write(root.join("f"), contents).expect("write f");
    git(root, &["add", "f"]);
    git(root, &["commit", "-qm", message]);
}

/// `main` and `other` both change `f` from `one`; HEAD is on `main`.
fn diverged(root: &Path) {
    init(root);
    commit_file(root, "one\n", "base");
    git(root, &["switch", "-qc", "other"]);
    commit_file(root, "other\n", "other");
    git(root, &["switch", "-q", "main"]);
    commit_file(root, "main\n", "mainc");
}

fn git_expect_conflict(root: &Path, args: &[&str]) {
    let out = git_cmd(args).in_dir(root).exec();
    assert!(!out.ok(), "git {args:?} should stop with a conflict");
}

fn open(root: &Path) -> Repository {
    Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open")
}

fn oid(root: &Path, rev: &str) -> ObjectId {
    ObjectId::from_hex(git(root, &["rev-parse", rev]).trim()).expect("oid")
}

fn merge_conflict(root: &Path) -> Repository {
    diverged(root);
    let repo = open(root);
    let outcome = integrate_branch(
        &repo,
        "refs/heads/main",
        oid(root, "main"),
        oid(root, "other"),
        "other",
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("merge");
    assert!(matches!(outcome, IntegrateBranchOutcome::Conflicts { .. }));
    repo
}

fn pick_conflict(root: &Path) -> Repository {
    diverged(root);
    git_expect_conflict(root, &["cherry-pick", "other"]);
    open(root)
}

fn revert_conflict(root: &Path) -> Repository {
    init(root);
    commit_file(root, "one\n", "base");
    commit_file(root, "two\n", "two");
    commit_file(root, "three\n", "three");
    git_expect_conflict(root, &["revert", "--no-edit", "HEAD~1"]);
    open(root)
}

fn rebase_conflict(root: &Path) -> Repository {
    diverged(root);
    git(root, &["switch", "-q", "other"]);
    git_expect_conflict(root, &["rebase", "main"]);
    open(root)
}

fn stash_pop_conflict(root: &Path) -> Repository {
    init(root);
    commit_file(root, "one\n", "base");
    fs::write(root.join("f"), "stashed\n").expect("edit");
    git(root, &["stash", "-q"]);
    commit_file(root, "main\n", "mainc");
    git_expect_conflict(root, &["stash", "pop"]);
    open(root)
}

fn staged_only(pathspec: &str) -> RestoreOptions {
    RestoreOptions {
        pathspecs: vec![pathspec.into()],
        pathspec_sources: vec![pathspec.into()],
        source: RestoreSource::Index,
        staged: true,
        worktree: false,
        stage: None,
        remerge: None,
    }
}

fn commit_req() -> CommitRequest {
    let id = "T <t@e.test> 1700000000 +0000";
    {
        let mut o = CommitRequest::default();
        o.message = "after".to_owned();
        o.author = Some(id.parse().expect("signature"));
        o.committer = Some(id.parse().expect("signature"));
        o
    }
}

fn state_snapshot(root: &Path) -> Vec<(&'static str, bool)> {
    STATE_FILES
        .iter()
        .map(|name| (*name, root.join(".git").join(name).exists()))
        .collect()
}

/// `restore --staged` and then an auto-staging commit leave the conflict exactly as it was.
fn assert_conflict_survives(root: &Path, repo: &Repository) {
    let unmerged = git(root, &["ls-files", "-u"]);
    assert!(!unmerged.trim().is_empty(), "fixture must be conflicted");
    let worktree = fs::read(root.join("f")).expect("f");
    let head = git(root, &["rev-parse", "HEAD"]);
    let state = state_snapshot(root);

    for pathspec in ["f", "."] {
        let err = restore_paths(
            repo,
            &staged_only(pathspec),
            &mut NullProgress,
            &NeverCancel,
        )
        .unwrap_err();
        assert!(
            matches!(&err, Error::RestorePathUnmerged { paths } if paths == &["f"]),
            "{err:?}"
        );
    }
    assert_eq!(git(root, &["ls-files", "-u"]), unmerged);
    assert_eq!(fs::read(root.join("f")).expect("f"), worktree);
    assert_eq!(state_snapshot(root), state);

    let err = commit_with_scope(
        repo,
        &commit_req(),
        CommitScope::AutoStage,
        &mut NullProgress,
        &NeverCancel,
    )
    .unwrap_err();
    assert!(matches!(err, Error::IndexUnmerged), "{err:?}");
    assert_eq!(git(root, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(root, &["ls-files", "-u"]), unmerged);
    assert_eq!(state_snapshot(root), state);
}

#[test]
fn restore_staged_refuses_merge_conflict() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = merge_conflict(tmp.path());
    assert_conflict_survives(tmp.path(), &repo);
}

#[test]
fn restore_staged_refuses_pick_conflict() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = pick_conflict(tmp.path());
    assert_conflict_survives(tmp.path(), &repo);
}

#[test]
fn restore_staged_refuses_revert_conflict() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = revert_conflict(tmp.path());
    assert_conflict_survives(tmp.path(), &repo);
}

#[test]
fn restore_staged_refuses_rebase_conflict() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = rebase_conflict(tmp.path());
    assert_conflict_survives(tmp.path(), &repo);
}

#[test]
fn restore_staged_refuses_stash_pop_conflict() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = stash_pop_conflict(tmp.path());
    assert_conflict_survives(tmp.path(), &repo);
}

/// System git drops the stages (recording resolve-undo) and keeps the markers in the worktree.
fn git_collapse(root: &Path) {
    git(root, &["restore", "--staged", "f"]);
    assert!(git(root, &["ls-files", "-u"]).trim().is_empty());
    assert!(!git(root, &["ls-files", "--resolve-undo"]).trim().is_empty());
    assert!(fs::read_to_string(root.join("f"))
        .expect("f")
        .contains("<<<<<<<"));
}

fn assert_collapsed_markers_refused(root: &Path, repo: &Repository) {
    git_collapse(root);
    let head = git(root, &["rev-parse", "HEAD"]);
    let index = git(root, &["ls-files", "-s"]);
    let state = state_snapshot(root);

    let err = commit_with_scope(
        repo,
        &commit_req(),
        CommitScope::AutoStage,
        &mut NullProgress,
        &NeverCancel,
    )
    .unwrap_err();
    let Error::UnmergedConflictMarkers { paths } = err else {
        panic!("expected conflict markers refusal, got {err:?}");
    };
    assert_eq!(paths, ["f"]);
    assert_eq!(git(root, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(root, &["ls-files", "-s"]), index);
    assert_eq!(state_snapshot(root), state);

    let err = stage(
        repo,
        &StageOptions {
            pathspecs: vec!["f".into()],
            ..StageOptions::default()
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .unwrap_err();
    assert!(
        matches!(err, Error::UnmergedConflictMarkers { .. }),
        "{err:?}"
    );
    assert_eq!(git(root, &["ls-files", "-s"]), index);
}

#[test]
fn commit_refuses_markers_after_git_drops_merge_stages() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    diverged(root);
    git_expect_conflict(root, &["merge", "other"]);
    let repo = open(root);
    assert_collapsed_markers_refused(root, &repo);

    fs::write(root.join("f"), "resolved\n").expect("resolve");
    let outcome = commit_with_scope(
        &repo,
        &commit_req(),
        CommitScope::AutoStage,
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("commit resolved merge");
    assert_eq!(git(root, &["show", "HEAD:f"]), "resolved\n");
    assert_eq!(
        git(root, &["rev-list", "--parents", "-n1", "HEAD"])
            .split_whitespace()
            .count(),
        3,
        "{outcome:?}"
    );
    git(root, &["fsck", "--strict"]);
}

#[test]
fn commit_refuses_markers_after_git_drops_pick_stages() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = pick_conflict(tmp.path());
    assert_collapsed_markers_refused(tmp.path(), &repo);
}

#[test]
fn commit_refuses_markers_after_git_drops_stash_pop_stages() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = stash_pop_conflict(tmp.path());
    assert!(
        no_operation_files(tmp.path()),
        "stash pop leaves no operation file"
    );
    assert_collapsed_markers_refused(tmp.path(), &repo);
}

fn no_operation_files(root: &Path) -> bool {
    state_snapshot(root)
        .iter()
        .all(|(name, present)| *name == "ORIG_HEAD" || !present)
}

/// Resolve-undo records outlive the merge commit; editing around a marker block that is already
/// committed is not refused.
#[test]
fn markers_already_committed_do_not_block_commit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    diverged(root);
    git_expect_conflict(root, &["merge", "other"]);
    git(root, &["add", "f"]);
    git(root, &["commit", "-qm", "merged with markers"]);
    assert!(!git(root, &["ls-files", "--resolve-undo"]).trim().is_empty());

    let doc = format!("{}appended\n", git(root, &["show", "HEAD:f"]));
    fs::write(root.join("f"), &doc).expect("edit around the committed block");
    let repo = open(root);
    commit_with_scope(
        &repo,
        &commit_req(),
        CommitScope::AutoStage,
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("markers are already committed");
    assert_eq!(git(root, &["show", "HEAD:f"]), doc);
}

/// A committed marker example must not mask the block a later `stash pop` writes beside it.
#[test]
fn committed_marker_example_does_not_mask_new_stash_pop_conflict() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init(root);
    let example = "<<<<<<< ours\na\n=======\nb\n>>>>>>> theirs\n";
    commit_file(root, &format!("{example}value=base\n"), "base");
    fs::write(root.join("f"), format!("{example}value=stashed\n")).expect("edit");
    git(root, &["stash", "-q"]);
    commit_file(root, &format!("{example}value=main\n"), "mainc");
    git_expect_conflict(root, &["stash", "pop"]);
    let popped = fs::read_to_string(root.join("f")).expect("f");
    assert!(popped.contains(">>>>>>> Stashed changes"), "{popped}");
    let repo = open(root);
    assert_collapsed_markers_refused(root, &repo);
}

/// Without an operation sentinel, a collapsed path with markers and a clean `HEAD` blob is
/// still refused: that is exactly the state `git stash pop` + `git restore --staged` leaves.
#[test]
fn stale_resolve_undo_with_new_markers_is_refused() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    diverged(root);
    git_expect_conflict(root, &["merge", "other"]);
    fs::write(root.join("f"), "resolved\n").expect("resolve");
    git(root, &["add", "f"]);
    git(root, &["commit", "-qm", "merged"]);
    assert!(!git(root, &["ls-files", "--resolve-undo"]).trim().is_empty());
    let head = git(root, &["rev-parse", "HEAD"]);

    fs::write(root.join("f"), "<<<<<<< a\nx\n=======\ny\n>>>>>>> b\n").expect("markers");
    let repo = open(root);
    let err = commit_with_scope(
        &repo,
        &commit_req(),
        CommitScope::AutoStage,
        &mut NullProgress,
        &NeverCancel,
    )
    .unwrap_err();
    assert!(
        matches!(err, Error::UnmergedConflictMarkers { .. }),
        "{err:?}"
    );
    assert_eq!(git(root, &["rev-parse", "HEAD"]), head);
}
