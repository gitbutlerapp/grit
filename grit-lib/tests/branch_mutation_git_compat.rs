//! Branch create, delete, upstream, and dry-run through `grit_lib::porcelain::branch`,
//! checked against the system `git` binary.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::branch::{
    create_branch, delete_branch, set_upstream, unset_upstream, BranchError, CreateBranchOptions,
    DeleteMode, StartPoint, TrackMode,
};
use grit_lib::repo::{InitOptions, Repository};
use grit_test_support::{git, git_cmd};

fn open(dir: &Path) -> Repository {
    Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("discover")
}

fn oid(dir: &Path, rev: &str) -> ObjectId {
    git(dir, &["rev-parse", rev]).trim().parse().expect("oid")
}

fn commit(dir: &Path, message: &str) {
    git(dir, &["commit", "-q", "--allow-empty", "-m", message]);
}

fn branch_config(dir: &Path) -> String {
    let out = git_cmd(&["config", "--get-regexp", r"^branch\."])
        .in_dir(dir)
        .exec();
    if out.ok() {
        out.stdout
    } else {
        String::new()
    }
}

fn fsck(dir: &Path) {
    git(dir, &["fsck", "--no-progress"]);
}

fn snapshot_bytes(dir: &Path) -> String {
    git(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads",
        ],
    ) + &branch_config(dir)
        + &git(dir, &["reflog", "show", "--all", "--format=%gd %gs"])
}

#[test]
fn create_at_start_point_reflog_matches_git() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    commit(dir, "c1");
    commit(dir, "c2");
    let start = oid(dir, "main~1");
    let repo = open(dir);
    create_branch(
        &repo,
        &CreateBranchOptions {
            name: "topic".into(),
            start: Some(StartPoint {
                oid: start,
                label: "main~1".into(),
            }),
            force: false,
            track: TrackMode::Never,
            dry_run: false,
        },
    )
    .unwrap();
    fsck(dir);
    let grit_log = git(dir, &["reflog", "show", "-1", "topic"]);
    let git_dir = tempfile::tempdir().unwrap();
    git(git_dir.path(), &["init", "-q", "-b", "main"]);
    commit(git_dir.path(), "c1");
    commit(git_dir.path(), "c2");
    git(git_dir.path(), &["branch", "topic", "main~1"]);
    let git_log = git(git_dir.path(), &["reflog", "show", "-1", "topic"]);
    assert_eq!(
        grit_log.lines().next(),
        git_log.lines().next(),
        "reflog first line"
    );
}

#[test]
fn force_overwrite_reflog_and_refuse_checked_out() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    commit(dir, "c1");
    git(dir, &["branch", "topic"]);
    commit(dir, "c2");
    let repo = open(dir);
    create_branch(
        &repo,
        &CreateBranchOptions {
            name: "topic".into(),
            start: Some(StartPoint {
                oid: oid(dir, "main"),
                label: "main".into(),
            }),
            force: true,
            track: TrackMode::Never,
            dry_run: false,
        },
    )
    .unwrap();
    assert_eq!(oid(dir, "topic"), oid(dir, "main"));
    let msg = git(dir, &["reflog", "show", "-1", "--format=%gs", "topic"]);
    assert!(msg.starts_with("branch: Reset to "));

    let wt = tmp.path().join("wt");
    git(
        dir,
        &["worktree", "add", "-q", "-b", "held", wt.to_str().unwrap()],
    );
    let err = create_branch(
        &repo,
        &CreateBranchOptions {
            name: "held".into(),
            start: None,
            force: true,
            track: TrackMode::Never,
            dry_run: false,
        },
    )
    .unwrap_err();
    assert!(matches!(
        err,
        Error::Branch(BranchError::CheckedOut { name, .. }) if name == "held"
    ));
    fsck(dir);
}

#[test]
fn auto_track_matches_git_branch_track() {
    let tmp = tempfile::tempdir().unwrap();
    let origin = tmp.path().join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q", "-b", "main"]);
    commit(&origin, "c1");
    git(&origin, &["branch", "upstream"]);
    commit(&origin, "on-upstream");

    let clone = tmp.path().join("clone");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    let repo = open(&clone);

    create_branch(
        &repo,
        &CreateBranchOptions {
            name: "upstream".into(),
            start: Some(StartPoint {
                oid: oid(&clone, "refs/remotes/origin/upstream"),
                label: "origin/upstream".into(),
            }),
            force: false,
            track: TrackMode::Auto,
            dry_run: false,
        },
    )
    .unwrap();
    fsck(&clone);
    let ours = branch_config(&clone);

    let copy = tmp.path().join("copy");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            copy.to_str().unwrap(),
        ],
    );
    git(
        &copy,
        &[
            "branch",
            "--track",
            "upstream",
            "refs/remotes/origin/upstream",
        ],
    );
    let theirs = branch_config(&copy);
    assert_eq!(ours, theirs);
}

#[test]
fn set_and_unset_upstream_match_git() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    commit(dir, "c1");
    git(dir, &["branch", "topic"]);
    git(dir, &["remote", "add", "origin", "/nonexistent"]);
    git(dir, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    let repo = open(dir);
    set_upstream(&repo, "topic", "origin/main").unwrap();
    fsck(dir);
    let ours = branch_config(dir);

    let copy = tempfile::tempdir().unwrap();
    git(copy.path(), &["init", "-q", "-b", "main"]);
    commit(copy.path(), "c1");
    git(copy.path(), &["branch", "topic"]);
    git(copy.path(), &["remote", "add", "origin", "/nonexistent"]);
    git(
        copy.path(),
        &["update-ref", "refs/remotes/origin/main", "HEAD"],
    );
    git(
        copy.path(),
        &["branch", "--set-upstream-to", "origin/main", "topic"],
    );
    assert_eq!(ours, branch_config(copy.path()));

    unset_upstream(&repo, "topic").unwrap();
    fsck(dir);
    git(copy.path(), &["branch", "--unset-upstream", "topic"]);
    assert_eq!(branch_config(dir), branch_config(copy.path()));
}

#[test]
fn safe_delete_unmerged_returns_not_fully_merged() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    commit(dir, "c1");
    git(dir, &["switch", "-q", "-c", "side"]);
    commit(dir, "c2");
    git(dir, &["switch", "-q", "main"]);
    let repo = open(dir);
    let err = delete_branch(&repo, "side", DeleteMode::Safe, false).unwrap_err();
    assert!(matches!(
        err,
        Error::Branch(BranchError::NotFullyMerged { name, .. }) if name == "side"
    ));
    fsck(dir);
}

#[test]
fn dry_run_leaves_refs_config_and_reflogs_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    commit(dir, "c1");
    git(dir, &["branch", "topic"]);
    git(dir, &["config", "branch.topic.remote", "origin"]);
    git(dir, &["config", "branch.topic.merge", "refs/heads/topic"]);
    let before = snapshot_bytes(dir);
    let repo = open(dir);

    create_branch(
        &repo,
        &CreateBranchOptions {
            name: "new".into(),
            start: None,
            force: false,
            track: TrackMode::Auto,
            dry_run: true,
        },
    )
    .unwrap();
    delete_branch(&repo, "topic", DeleteMode::Safe, true).unwrap();
    assert_eq!(before, snapshot_bytes(dir));
}

#[test]
fn public_branch_mutation_api_smoke() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::init(tmp.path(), &InitOptions::default()).unwrap();
    git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "c1"]);
    let opts = CreateBranchOptions::at_head("x");
    assert_eq!(opts.track, TrackMode::Auto);
    let _ = create_branch(&repo, &opts).unwrap();
    let _ = set_upstream(&repo, "x", "main").unwrap();
    let _ = unset_upstream(&repo, "x").unwrap();
    let _ = delete_branch(&repo, "x", DeleteMode::Force, false).unwrap();
}
