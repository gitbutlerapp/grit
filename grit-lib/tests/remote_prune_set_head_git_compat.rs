//! `RepositoryRemotes::prune` and `set_head` compared with `git remote prune` / `set-head`.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use crate::support;

use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;
use grit_lib::repo_handles::{SetHead, SetHeadOutcome};
use grit_test_support::{git, git_cmd};
use support::{git_for_each_ref, git_interop_available, Backend};

fn open(dir: &Path) -> Repository {
    Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("discover")
}

fn copy_dir(seed: &Path, label: &str) -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().expect("tempdir");
    let dest = root.path().join(label);
    let seed_abs = seed.canonicalize().unwrap_or_else(|_| seed.to_path_buf());
    let status = Command::new("cp")
        .arg("-a")
        .arg(&seed_abs)
        .arg(&dest)
        .status()
        .expect("cp");
    assert!(status.success(), "cp seed to {label}");
    (root, dest)
}

fn build_prune_seed() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().expect("tempdir");
    let upstream = root.path().join("upstream");
    git_cmd(&["init", "-q", "-b", "main", upstream.to_str().unwrap()])
        .in_dir(root.path())
        .suc();
    git(
        &upstream,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "c1",
        ],
    );
    git(&upstream, &["branch", "topic"]);
    let clone = root.path().join("clone");
    git_cmd(&[
        "clone",
        "-q",
        upstream.to_str().unwrap(),
        clone.to_str().unwrap(),
    ])
    .in_dir(root.path())
    .suc();
    git_cmd(&["fetch", "origin", "topic"]).in_dir(&clone).suc();
    (root, clone)
}

#[test]
fn prune_matches_git_remote_prune() {
    if !git_interop_available(Backend::Files) {
        return;
    }
    let (_root, seed) = build_prune_seed();
    let upstream = seed.parent().unwrap().join("upstream");
    git(&upstream, &["branch", "-D", "topic"]);

    let (_g, grit_dir) = copy_dir(&seed, "grit");
    let (_git, git_dir) = copy_dir(&seed, "git");

    git(&git_dir, &["remote", "prune", "origin"]);
    let git_refs = git_for_each_ref(&git_dir, "refs/remotes");

    let grit_repo = open(&grit_dir);
    let pruned = grit_repo
        .remotes()
        .prune("origin", false, None)
        .expect("prune");
    assert!(
        pruned.iter().any(|r| r.ends_with("/topic")),
        "expected topic tracking ref in prune list: {pruned:?}"
    );
    let grit_refs = git_for_each_ref(&grit_dir, "refs/remotes");
    assert_eq!(git_refs, grit_refs);
}

#[test]
fn prune_dry_run_noop() {
    if !git_interop_available(Backend::Files) {
        return;
    }
    let (_root, seed) = build_prune_seed();
    let upstream = seed.parent().unwrap().join("upstream");
    git(&upstream, &["branch", "-D", "topic"]);

    let repo = open(&seed);
    let before = git_for_each_ref(&seed, "refs/remotes");
    let would = repo
        .remotes()
        .prune("origin", true, None)
        .expect("dry-run prune");
    assert!(!would.is_empty());
    let after = git_for_each_ref(&seed, "refs/remotes");
    assert_eq!(before, after);
}

#[test]
fn set_head_auto_matches_git() {
    if !git_interop_available(Backend::Files) {
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let upstream = root.path().join("upstream");
    git_cmd(&["init", "-q", "-b", "main", upstream.to_str().unwrap()])
        .in_dir(root.path())
        .suc();
    git(
        &upstream,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "c1",
        ],
    );
    let clone = root.path().join("clone");
    git_cmd(&[
        "clone",
        "-q",
        upstream.to_str().unwrap(),
        clone.to_str().unwrap(),
    ])
    .in_dir(root.path())
    .suc();

    let (_g, grit_dir) = copy_dir(&clone, "grit");
    let (_git, git_dir) = copy_dir(&clone, "git");

    git(&git_dir, &["remote", "set-head", "origin", "--auto"]);
    let git_head = git(&git_dir, &["symbolic-ref", "refs/remotes/origin/HEAD"]);

    let grit_repo = open(&grit_dir);
    grit_repo
        .remotes()
        .set_head("origin", SetHead::Auto, false, None)
        .expect("set-head auto");
    let grit_head = git(&grit_dir, &["symbolic-ref", "refs/remotes/origin/HEAD"]);
    assert_eq!(git_head.trim(), grit_head.trim());
}

#[test]
fn set_head_delete() {
    if !git_interop_available(Backend::Files) {
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let upstream = root.path().join("upstream");
    git_cmd(&["init", "-q", "-b", "main", upstream.to_str().unwrap()])
        .in_dir(root.path())
        .suc();
    git(
        &upstream,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "c1",
        ],
    );
    let clone = root.path().join("clone");
    git_cmd(&[
        "clone",
        "-q",
        upstream.to_str().unwrap(),
        clone.to_str().unwrap(),
    ])
    .in_dir(root.path())
    .suc();
    git(
        &clone,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );

    let repo = open(&clone);
    let SetHeadOutcome {
        target,
        previous_target,
        dry_run,
    } = repo
        .remotes()
        .set_head("origin", SetHead::Delete, false, None)
        .expect("delete head");
    assert!(!dry_run);
    assert!(target.is_none());
    assert!(previous_target.is_some());
    let status = Command::new("git")
        .current_dir(&clone)
        .args(["symbolic-ref", "refs/remotes/origin/HEAD"])
        .status()
        .expect("git status");
    assert!(!status.success());
}

#[test]
fn prune_negative_refspec_preserves_excluded_ref() {
    if !git_interop_available(Backend::Files) {
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let upstream = root.path().join("upstream");
    git_cmd(&["init", "-q", "-b", "main", upstream.to_str().unwrap()])
        .in_dir(root.path())
        .suc();
    git(
        &upstream,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "c1",
        ],
    );
    git(&upstream, &["branch", "private"]);
    let clone = root.path().join("clone");
    git_cmd(&[
        "clone",
        "-q",
        upstream.to_str().unwrap(),
        clone.to_str().unwrap(),
    ])
    .in_dir(root.path())
    .suc();
    git(&clone, &["fetch", "origin", "private"]);
    git(
        &clone,
        &[
            "config",
            "--add",
            "remote.origin.fetch",
            "^refs/heads/private",
        ],
    );
    git(&upstream, &["branch", "-D", "private"]);

    let (_g, grit_dir) = copy_dir(&clone, "grit");
    let (_git, git_dir) = copy_dir(&clone, "git");

    git(&git_dir, &["remote", "prune", "origin"]);
    let git_refs = git_for_each_ref(&git_dir, "refs/remotes");

    let grit_repo = open(&grit_dir);
    let pruned = grit_repo
        .remotes()
        .prune("origin", false, None)
        .expect("prune");
    assert!(
        !pruned.iter().any(|r| r.ends_with("/private")),
        "excluded ref must not be pruned: {pruned:?}"
    );
    let grit_refs = git_for_each_ref(&grit_dir, "refs/remotes");
    assert_eq!(git_refs, grit_refs);
    assert!(
        grit_refs.iter().any(|(name, _)| name.ends_with("/private")),
        "private tracking ref should remain"
    );
}

#[test]
fn set_head_delete_broken_symref() {
    if !git_interop_available(Backend::Files) {
        return;
    }
    let root = tempfile::tempdir().expect("tempdir");
    let upstream = root.path().join("upstream");
    git_cmd(&["init", "-q", "-b", "main", upstream.to_str().unwrap()])
        .in_dir(root.path())
        .suc();
    git(
        &upstream,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "c1",
        ],
    );
    let clone = root.path().join("clone");
    git_cmd(&[
        "clone",
        "-q",
        upstream.to_str().unwrap(),
        clone.to_str().unwrap(),
    ])
    .in_dir(root.path())
    .suc();
    git(
        &clone,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/missing-branch",
        ],
    );

    let (_g, grit_dir) = copy_dir(&clone, "grit");
    let (_git, git_dir) = copy_dir(&clone, "git");

    git(&git_dir, &["remote", "set-head", "origin", "--delete"]);

    let grit_repo = open(&grit_dir);
    grit_repo
        .remotes()
        .set_head("origin", SetHead::Delete, false, None)
        .expect("delete broken head");

    for dir in [&git_dir, &grit_dir] {
        let status = Command::new("git")
            .current_dir(dir)
            .args(["symbolic-ref", "refs/remotes/origin/HEAD"])
            .status()
            .expect("git status");
        assert!(
            !status.success(),
            "HEAD symref should be gone in {}",
            dir.display()
        );
    }
}
