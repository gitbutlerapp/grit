//! Staging a tracked path whose worktree type flipped between a file and a directory (issue #2170).
//!
//! `git add -A` / `git commit -a` record such a flip as a deletion of the old path plus additions
//! under the new one. Every test round-trips the result through the system `git`.

use std::fs;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::add::{StageMode, StageOptions, StageOutcome};
use grit_lib::porcelain::commit::{CommitRequest, HookPolicy};
use grit_lib::porcelain::stage_tracked::IndexStagingThresholds;
use grit_lib::progress::CancelToken;
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::test_support::knobs::stage_options_staging_thresholds;
use grit_test_support::git;

fn init_repo(root: &Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
}

fn commit_all(root: &Path) {
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
}

fn open_repo(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open")
}

fn stage_all(root: &Path, thresholds: Option<IndexStagingThresholds>) -> StageOutcome {
    let base = StageOptions {
        mode: StageMode::All,
        ..StageOptions::default()
    };
    let opts = match thresholds {
        Some(t) => stage_options_staging_thresholds(base, t),
        None => base,
    };
    &open_repo(root)
        .index()
        .stage(&opts, &mut NullProgress, &CancelToken::never())
        .expect("stage")
}

fn cached_name_status(root: &Path) -> String {
    git(root, &["diff", "--cached", "--name-status", "--no-renames"])
}

fn assert_git_fsck_clean(root: &Path) {
    git(root, &["fsck", "--strict", "--no-dangling"]);
}

/// Replaces tracked file `f` (with an unchanged later sibling `g`) by a directory holding `f/i`.
fn file_became_dir_repo(root: &Path, with_sibling: bool) {
    init_repo(root);
    fs::write(root.join("f"), "f\n").expect("write f");
    if with_sibling {
        fs::write(root.join("g"), "g\n").expect("write g");
    }
    commit_all(root);
    fs::remove_file(root.join("f")).expect("rm f");
    fs::create_dir(root.join("f")).expect("mkdir f");
    fs::write(root.join("f/i"), "i\n").expect("write f/i");
}

/// Replaces tracked directory `d/x` (with an unchanged later sibling `g`) by a file `d`.
fn dir_became_file_repo(root: &Path) {
    init_repo(root);
    fs::create_dir(root.join("d")).expect("mkdir d");
    fs::write(root.join("d/x"), "1\n").expect("write d/x");
    fs::write(root.join("g"), "g\n").expect("write g");
    commit_all(root);
    fs::remove_dir_all(root.join("d")).expect("rm d");
    fs::write(root.join("d"), "n\n").expect("write d");
}

fn commit_request() -> CommitRequest {
    let ident = "Test <t@example.com> 1 +0000".to_string();
    {
        let mut req = CommitRequest::new("flip");
        req.author = Some(ident.parse().expect("signature"));
        req.committer = Some(ident.parse().expect("signature"));
        req.hooks = HookPolicy::SkipVerify;
        req
    }
}

fn commit_and_check(root: &Path, expected: &str) {
    &open_repo(root)
        .commits()
        .create(
            &grit_lib::commits::CommitOptions::new(
                commit_request(),
                grit_lib::porcelain::commit::CommitScope::AutoStage,
            ),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("commit");
    assert_eq!(
        git(
            root,
            &["diff", "--name-status", "--no-renames", "HEAD~1", "HEAD"]
        ),
        expected
    );
    assert_eq!(git(root, &["status", "--porcelain"]), "");
    assert_git_fsck_clean(root);
}

#[test]
fn stage_file_replaced_by_directory_with_later_sibling() {
    let tmp = tempfile::tempdir().expect("tempdir");
    file_became_dir_repo(tmp.path(), true);

    let outcome = stage_all(tmp.path(), None);
    assert_eq!(outcome.removed, ["f"]);
    assert_eq!(outcome.added, ["f/i"]);
    assert!(outcome.modified.is_empty(), "{outcome:?}");
    assert_eq!(cached_name_status(tmp.path()), "D\tf\nA\tf/i\n");
    assert_git_fsck_clean(tmp.path());
}

#[test]
fn stage_file_replaced_by_directory_without_sibling() {
    let tmp = tempfile::tempdir().expect("tempdir");
    file_became_dir_repo(tmp.path(), false);

    let outcome = stage_all(tmp.path(), None);
    assert_eq!(outcome.removed, ["f"]);
    assert_eq!(outcome.added, ["f/i"]);
    assert_eq!(cached_name_status(tmp.path()), "D\tf\nA\tf/i\n");
}

#[test]
fn stage_file_replaced_by_directory_on_bulk_restage_path() {
    let tmp = tempfile::tempdir().expect("tempdir");
    file_became_dir_repo(tmp.path(), true);
    // Warm the untracked cache so the bulk path's deferred invalidation must see `f` removed.
    git(tmp.path(), &["config", "core.untrackedCache", "true"]);
    git(tmp.path(), &["update-index", "--untracked-cache"]);
    git(tmp.path(), &["status", "--porcelain"]);

    let outcome = stage_all(
        tmp.path(),
        Some(IndexStagingThresholds {
            large_index_restage: 1,
            parallel_tracked_restage: 1,
        }),
    );
    assert_eq!(outcome.removed, ["f"]);
    assert_eq!(outcome.added, ["f/i"]);
    assert_eq!(cached_name_status(tmp.path()), "D\tf\nA\tf/i\n");
    assert_git_fsck_clean(tmp.path());
}

#[test]
fn stage_intent_to_add_file_replaced_by_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    fs::write(root.join("g"), "g\n").expect("write g");
    commit_all(root);
    fs::write(root.join("f"), "f\n").expect("write f");
    git(root, &["add", "-N", "f"]);
    fs::remove_file(root.join("f")).expect("rm f");
    fs::create_dir(root.join("f")).expect("mkdir f");
    fs::write(root.join("f/i"), "i\n").expect("write f/i");

    let outcome = stage_all(root, None);
    assert_eq!(outcome.removed, ["f"]);
    assert_eq!(outcome.added, ["f/i"]);
    assert_eq!(cached_name_status(root), "A\tf/i\n");
}

#[test]
fn stage_directory_replaced_by_file_with_later_sibling() {
    let tmp = tempfile::tempdir().expect("tempdir");
    dir_became_file_repo(tmp.path());

    let outcome = stage_all(tmp.path(), None);
    assert_eq!(outcome.removed, ["d/x"]);
    assert_eq!(outcome.added, ["d"]);
    assert_eq!(cached_name_status(tmp.path()), "A\td\nD\td/x\n");
    assert_git_fsck_clean(tmp.path());
}

#[test]
fn stage_for_commit_records_both_flips() {
    let tmp = tempfile::tempdir().expect("tempdir");
    file_became_dir_repo(tmp.path(), true);
    let outcome = &open_repo(tmp.path())
        .index()
        .stage_for_commit(&mut NullProgress, &CancelToken::never())
        .expect("stage");
    assert_eq!(outcome.removed, ["f"]);
    assert_eq!(outcome.added, ["f/i"]);

    let tmp = tempfile::tempdir().expect("tempdir");
    dir_became_file_repo(tmp.path());
    let outcome = &open_repo(tmp.path())
        .index()
        .stage_for_commit(&mut NullProgress, &CancelToken::never())
        .expect("stage");
    assert_eq!(outcome.removed, ["d/x"]);
    assert_eq!(outcome.added, ["d"]);
}

#[test]
fn commit_file_replaced_by_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    file_became_dir_repo(tmp.path(), true);
    commit_and_check(tmp.path(), "D\tf\nA\tf/i\n");
}

#[test]
fn commit_directory_replaced_by_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    dir_became_file_repo(tmp.path());
    commit_and_check(tmp.path(), "A\td\nD\td/x\n");
}

#[cfg(unix)]
#[test]
fn commit_symlink_replaced_by_directory_with_later_sibling() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    std::os::unix::fs::symlink("g", root.join("l")).expect("symlink l");
    fs::write(root.join("m"), "m\n").expect("write m");
    commit_all(root);
    fs::remove_file(root.join("l")).expect("rm l");
    fs::create_dir(root.join("l")).expect("mkdir l");
    fs::write(root.join("l/i"), "i\n").expect("write l/i");

    commit_and_check(root, "D\tl\nA\tl/i\n");
}
