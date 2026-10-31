//! Regression for GitHub issue #2198: creating a ref whose name is a directory of other refs
//! (or of their reflogs) is a typed name conflict naming the blocking ref, whether that ref is
//! loose or packed, for `branch`, `switch -c` and `fetch` alike. System `git` refuses the same
//! creates.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::porcelain::branch::{create_branch, CreateBranchOptions};
use grit_lib::porcelain::switch::{resolve_create_branch, switch_repository, SwitchOptions};
use grit_lib::refs::store::RefStoreError;
use grit_lib::refs::{resolve_ref, RefnameUnavailable};
use grit_lib::remote::transfer::{fetch_local, FetchOptions, TagMode, UpdateMode};
use grit_lib::repo::Repository;
use grit_test_support::{git, git_cmd};

/// How `refs/heads/rel/1.0` is stored when `rel` is created.
#[derive(Clone, Copy, Debug)]
enum Shape {
    Loose,
    /// Packed by `git pack-refs --all`, which keeps `logs/refs/heads/rel/1.0`.
    Packed,
}

fn repo_with_rel_child(shape: Shape) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["commit", "-q", "--allow-empty", "-m", "c"]);
    git(dir, &["branch", "rel/1.0"]);
    if let Shape::Packed = shape {
        git(dir, &["pack-refs", "--all"]);
        assert!(!dir.join(".git/refs/heads/rel/1.0").exists());
    }
    assert!(dir.join(".git/logs/refs/heads/rel/1.0").is_file());
    tmp
}

fn open(dir: &Path) -> Repository {
    Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("discover")
}

fn assert_names_blocking_ref(err: &Error, blocking_ref: &str, new: &str) {
    assert!(
        matches!(
            err,
            Error::RefStore(RefStoreError::NameUnavailable {
                reason: RefnameUnavailable::DescendantExists { blocking, new_ref }
            }) if blocking == blocking_ref && new_ref == new
        ),
        "expected '{blocking_ref}' to block '{new}': {err:?}"
    );
    assert_eq!(
        err.to_string(),
        format!("cannot lock ref '{new}': '{blocking_ref}' exists; cannot create '{new}'")
    );
}

fn assert_git_refuses_branch(dir: &Path) {
    let out = git_cmd(&["branch", "rel"]).in_dir(dir).exec();
    assert!(!out.ok(), "git should refuse too");
    assert!(
        out.stderr.contains("'refs/heads/rel/1.0' exists"),
        "{}",
        out.stderr
    );
}

#[test]
fn create_branch_over_a_ref_directory_names_the_ref() {
    for shape in [Shape::Loose, Shape::Packed] {
        let tmp = repo_with_rel_child(shape);
        let dir = tmp.path();
        let err = create_branch(&open(dir), &CreateBranchOptions::at_head("rel")).unwrap_err();
        assert_names_blocking_ref(&err, "refs/heads/rel/1.0", "refs/heads/rel");
        assert!(resolve_ref(&dir.join(".git"), "refs/heads/rel").is_err());
        assert_git_refuses_branch(dir);
        git(dir, &["fsck", "--no-progress"]);
    }
}

#[test]
fn switch_create_over_a_ref_directory_names_the_ref() {
    for shape in [Shape::Loose, Shape::Packed] {
        let tmp = repo_with_rel_child(shape);
        let dir = tmp.path();
        let repo = open(dir);
        let mode = resolve_create_branch(&repo, "rel", None).expect("resolve");
        let err = switch_repository(
            &repo,
            &SwitchOptions {
                mode,
                dry_run: false,
                hooks: grit_lib::PostOperationHooks::Run,
            },
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::NeverCancel,
        )
        .unwrap_err();
        assert_names_blocking_ref(&err, "refs/heads/rel/1.0", "refs/heads/rel");
        assert_eq!(
            git(dir, &["symbolic-ref", "HEAD"]).trim(),
            "refs/heads/main",
            "{shape:?}: HEAD must stay put"
        );
        assert_git_refuses_branch(dir);
    }
}

#[test]
fn create_branch_over_a_stray_reflog_directory_names_the_log() {
    let tmp = repo_with_rel_child(Shape::Loose);
    let dir = tmp.path();
    // Delete the ref behind git's back so only its log directory remains.
    fs::remove_file(dir.join(".git/refs/heads/rel/1.0")).unwrap();
    let err = create_branch(&open(dir), &CreateBranchOptions::at_head("rel")).unwrap_err();
    assert_names_blocking_ref(&err, "refs/heads/rel/1.0", "refs/heads/rel");
    assert!(resolve_ref(&dir.join(".git"), "refs/heads/rel").is_err());
    let out = git_cmd(&["branch", "rel"]).in_dir(dir).exec();
    assert!(!out.ok(), "git refuses while logs remain: {}", out.stdout);
}

fn commit(dir: &Path, message: &str) {
    git(dir, &["commit", "-q", "--allow-empty", "-m", message]);
}

/// A clone that fetched `rel/1.0` and `rel/2.0` into `refs/remotes/origin/rel/`, with their
/// reflogs, from a bare remote that has since replaced them with `rel` and added `other`.
fn fetched_rel_children(tmp: &Path) -> (PathBuf, PathBuf, FetchOptions) {
    let bare = tmp.join("remote.git");
    git(tmp, &["init", "-q", "--bare", bare.to_str().unwrap()]);
    let seed = tmp.join("seed");
    git(tmp, &["init", "-q", "-b", "main", seed.to_str().unwrap()]);
    commit(&seed, "c");
    git(
        &seed,
        &[
            "push",
            "-q",
            bare.to_str().unwrap(),
            "HEAD:refs/heads/main",
            "HEAD:refs/heads/rel/1.0",
            "HEAD:refs/heads/rel/2.0",
        ],
    );
    let clone = tmp.join("work");
    git(tmp, &["init", "-q", "-b", "main", clone.to_str().unwrap()]);
    git(&clone, &["remote", "add", "origin", bare.to_str().unwrap()]);
    let clone_git = clone.join(".git");
    let opts = {
        let mut o = FetchOptions::default();
        o.refspecs = vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()];
        o.remote_name = Some("origin".to_owned());
        o.tags = TagMode::None;
        o
    };
    // System git writes the tracking refs with reflogs, so `logs/…/rel/` is a directory too.
    git(&clone, &["fetch", "-q", "origin"]);
    assert!(clone_git.join("logs/refs/remotes/origin/rel/2.0").is_file());

    for gone in ["refs/heads/rel/1.0", "refs/heads/rel/2.0"] {
        git(&bare, &["update-ref", "-d", gone]);
    }
    git(&bare, &["update-ref", "refs/heads/rel", "main"]);
    git(&bare, &["update-ref", "refs/heads/other", "main"]);
    (bare, clone, opts)
}

fn assert_fetch_rejects_rel_and_creates_other(clone: &Path, bare: &Path, opts: &FetchOptions) {
    let clone_git = clone.join(".git");
    let outcome = fetch_local(&clone_git, bare, opts).expect("fetch without prune");
    assert!(resolve_ref(&clone_git, "refs/remotes/origin/other").is_ok());
    assert!(resolve_ref(&clone_git, "refs/remotes/origin/rel").is_err());
    let rel = outcome
        .updates
        .iter()
        .find(|u| u.local_ref.as_deref() == Some("refs/remotes/origin/rel"))
        .expect("rel update");
    assert_eq!(
        rel.mode,
        UpdateMode::RefLockRejected,
        "{:?}",
        outcome.updates
    );
    let note = rel.note.as_deref().unwrap_or_default();
    assert!(
        note.contains("refs/remotes/origin/rel/1.0") && note.contains("prune"),
        "the rejection names the blocking ref and suggests prune: {note}"
    );
    git(clone, &["fsck", "--no-progress"]);

    let pruned = {
        let mut o = opts.clone();
        o.prune = true;
        o
    };
    let outcome = fetch_local(&clone_git, bare, &pruned).expect("fetch with prune");
    assert!(!outcome.has_rejections(), "{:?}", outcome.updates);
    assert!(resolve_ref(&clone_git, "refs/remotes/origin/rel").is_ok());
    git(clone, &["fsck", "--no-progress"]);
}

#[test]
fn fetch_rejects_only_the_ref_blocked_by_loose_tracking_refs() {
    let tmp = tempfile::tempdir().unwrap();
    let (bare, clone, opts) = fetched_rel_children(tmp.path());
    assert!(clone.join(".git/refs/remotes/origin/rel/1.0").is_file());
    assert_fetch_rejects_rel_and_creates_other(&clone, &bare, &opts);
}

#[test]
fn fetch_rejects_only_the_ref_blocked_by_packed_tracking_refs_with_log_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    let (bare, clone, opts) = fetched_rel_children(tmp.path());
    git(&clone, &["pack-refs", "--all"]);
    assert!(!clone.join(".git/refs/remotes/origin/rel/1.0").exists());
    assert!(clone
        .join(".git/logs/refs/remotes/origin/rel/1.0")
        .is_file());
    assert_fetch_rejects_rel_and_creates_other(&clone, &bare, &opts);
}

#[test]
fn system_git_fetch_also_rejects_the_blocked_ref() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, clone, _) = fetched_rel_children(tmp.path());
    let out = git_cmd(&["fetch", "origin"]).in_dir(&clone).exec();
    assert!(!out.ok(), "git fails the fetch: {}", out.stderr);
    assert!(
        resolve_ref(&clone.join(".git"), "refs/remotes/origin/other").is_ok(),
        "git still creates the unblocked ref"
    );
    assert!(resolve_ref(&clone.join(".git"), "refs/remotes/origin/rel").is_err());
}
