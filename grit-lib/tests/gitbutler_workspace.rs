//! [`GitButlerWorkspace::detect`] on repositories set up the way GitButler leaves them.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::gitbutler::{
    GitButlerWorkspace, WorkspaceHead, WorkspaceStanding, WORKSPACE_COMMIT_MARKER,
};
use grit_lib::objects::ObjectId;
use grit_lib::refs::branch_tracking::{BranchStanding, StandingOptions, TrackedComparison};
use grit_lib::repo::Repository;
use grit_lib::state::resolve_head_for_repository;
use tempfile::TempDir;

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "A")
        .env("GIT_AUTHOR_EMAIL", "a@example.com")
        .env("GIT_COMMITTER_NAME", "A")
        .env("GIT_COMMITTER_EMAIL", "a@example.com")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8")
}

fn rev(root: &Path, spec: &str) -> ObjectId {
    git(root, &["rev-parse", spec]).trim().parse().expect("oid")
}

fn open(root: &Path) -> Repository {
    Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open")
}

/// `main` plus a commit each on `one` and `two` (and `two-alias` at the same tip).
fn two_branches() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["commit", "-q", "--allow-empty", "-m", "base"]);
    for name in ["one", "two"] {
        git(root, &["switch", "-q", "-c", name, "main"]);
        std::fs::write(root.join(format!("{name}.txt")), name).unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-q", "-m", name]);
    }
    git(root, &["branch", "two-alias", "two"]);
    dir
}

/// Check out `gitbutler/workspace` at a merge of `parents` with `message`.
fn workspace_commit(root: &Path, parents: &[&str], message: &str) -> ObjectId {
    let tree = git(root, &["rev-parse", &format!("{}^{{tree}}", parents[0])]);
    let mut args = vec!["commit-tree", tree.trim()];
    for parent in parents {
        args.extend(["-p", parent]);
    }
    args.extend(["-m", message]);
    let commit = git(root, &args).trim().to_owned();
    git(root, &["branch", "-f", "gitbutler/workspace", &commit]);
    git(
        root,
        &["symbolic-ref", "HEAD", "refs/heads/gitbutler/workspace"],
    );
    commit.parse().expect("oid")
}

#[test]
fn detects_workspace_and_names_each_merged_head() {
    let dir = two_branches();
    let root = dir.path();
    let message = format!("{WORKSPACE_COMMIT_MARKER}\n\nHere be dragons");
    let commit = workspace_commit(root, &["one", "two"], &message);
    git(root, &["branch", "gitbutler/edit", "one"]);

    let workspace = GitButlerWorkspace::detect(&open(root))
        .expect("detect")
        .expect("a workspace");
    assert_eq!(workspace.commit, commit);
    assert_eq!(
        workspace.heads,
        [
            WorkspaceHead {
                commit: rev(root, "one"),
                branches: vec!["one".to_owned()],
            },
            WorkspaceHead {
                commit: rev(root, "two"),
                branches: vec!["two".to_owned(), "two-alias".to_owned()],
            },
        ],
        "parent order, sorted names, no gitbutler/* branches"
    );
    assert_eq!(
        workspace.head_commits(),
        [rev(root, "one"), rev(root, "two")]
    );
}

/// `HEAD`'s comparison with `main`, the way `grit status` computes it.
fn target_comparison(repo: &Repository, counts: bool) -> TrackedComparison {
    BranchStanding::compute(
        repo,
        &resolve_head_for_repository(repo).expect("HEAD"),
        StandingOptions {
            newest_ahead_limit: 10,
            counts,
        },
    )
    .expect("standing")
    .target
    .expect("main is the target")
}

#[test]
fn standing_leaves_out_the_workspace_commit_and_compares_each_head() {
    let dir = two_branches();
    let root = dir.path();
    git(root, &["switch", "-q", "one"]);
    std::fs::write(root.join("more.txt"), "more").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "one more"]);
    let commit = workspace_commit(root, &["one", "two", "main"], WORKSPACE_COMMIT_MARKER);
    let repo = open(root);
    let workspace = GitButlerWorkspace::detect(&repo)
        .expect("detect")
        .expect("a workspace");
    let target = target_comparison(&repo, true);
    assert_eq!(
        target.ahead,
        Some(4),
        "three branch commits and the workspace commit"
    );
    assert_eq!(target.newest_ahead.first(), Some(&commit));

    let standing = workspace.standing(&repo, &target).expect("standing");
    assert_eq!(standing.ahead, Some(3));
    assert!(!standing.newest_ahead.contains(&commit));
    assert_eq!(standing.newest_ahead.len(), 3);
    assert_eq!(standing.heads_ahead, [Some(2), Some(1), Some(0)]);
}

#[test]
fn standing_without_counts_compares_nothing() {
    let dir = two_branches();
    let root = dir.path();
    let commit = workspace_commit(root, &["one", "two"], WORKSPACE_COMMIT_MARKER);
    let repo = open(root);
    let workspace = GitButlerWorkspace::detect(&repo)
        .expect("detect")
        .expect("a workspace");
    let target = target_comparison(&repo, false);
    assert_eq!(target.ahead, None);

    let standing = workspace.standing(&repo, &target).expect("standing");
    assert_eq!(
        standing,
        WorkspaceStanding {
            ahead: None,
            newest_ahead: Vec::new(),
            heads_ahead: vec![None, None],
        }
    );
    assert_eq!(workspace.commit, commit);
}

#[test]
fn head_with_no_branch_has_no_names() {
    let dir = two_branches();
    let root = dir.path();
    workspace_commit(root, &["one", "main"], WORKSPACE_COMMIT_MARKER);
    git(root, &["branch", "-D", "one"]);
    let workspace = GitButlerWorkspace::detect(&open(root))
        .expect("detect")
        .expect("a workspace");
    assert!(workspace.heads[0].branches.is_empty());
    assert_eq!(workspace.heads[1].branches, ["main"]);
}

#[test]
fn ordinary_branch_is_not_a_workspace() {
    let dir = two_branches();
    let root = dir.path();
    git(root, &["switch", "-q", "main"]);
    assert_eq!(
        GitButlerWorkspace::detect(&open(root)).expect("detect"),
        None
    );
}

#[test]
fn workspace_branch_without_the_workspace_commit_is_not_a_workspace() {
    let dir = two_branches();
    let root = dir.path();
    workspace_commit(root, &["one", "two"], "an ordinary merge");
    assert_eq!(
        GitButlerWorkspace::detect(&open(root)).expect("detect"),
        None
    );
}

#[test]
fn workspace_commit_on_another_branch_is_not_a_workspace() {
    let dir = two_branches();
    let root = dir.path();
    workspace_commit(root, &["one", "two"], WORKSPACE_COMMIT_MARKER);
    git(root, &["switch", "-q", "-c", "copy"]);
    assert_eq!(
        GitButlerWorkspace::detect(&open(root)).expect("detect"),
        None
    );
}

#[test]
fn detached_head_is_not_a_workspace() {
    let dir = two_branches();
    let root = dir.path();
    let commit = workspace_commit(root, &["one", "two"], WORKSPACE_COMMIT_MARKER);
    git(root, &["switch", "-q", "--detach", &commit.to_hex()]);
    assert_eq!(
        GitButlerWorkspace::detect(&open(root)).expect("detect"),
        None
    );
}
