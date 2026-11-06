//! Issue #2521: assume-unchanged must survive reset, merge, and stash like Git.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::merge::{integrate_branch, IntegrateBranchOutcome};
use grit_lib::porcelain::reset::{reset_to_commit, ResetMode, ResetOptions, DEFAULT_ORPHAN_LIMIT};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::stash::{StashApplyOptions, StashPushOptions};
use grit_lib::state::resolve_head_for_repository;
use grit_test_support::git;

use crate::support::{
    assert_git_fsck_strict, git_ls_files_verbose_map, git_status_porcelain_v2_lines,
};

fn init_repo() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "a@b.c"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open");
    (dir, repo)
}

fn seed_assume_unchanged_repo() -> (tempfile::TempDir, Repository, grit_lib::objects::ObjectId) {
    let (dir, repo) = init_repo();
    let root = dir.path();
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(root.join(name), format!("{name}\n")).expect("write");
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);
    git(root, &["update-index", "--assume-unchanged", "b.txt"]);
    git(root, &["update-index", "--skip-worktree", "a.txt"]);
    std::fs::write(root.join("b.txt"), "b\nlocal\n").expect("local edit on b");
    std::fs::write(root.join("c.txt"), "c\nwork\n").expect("local edit on c");
    let head = *resolve_head_for_repository(&repo)
        .expect("head")
        .oid()
        .expect("commit");
    (dir, repo, head)
}

fn assert_b_assume_unchanged(root: &std::path::Path) {
    let tags = git_ls_files_verbose_map(root);
    assert_eq!(
        tags.get("b.txt").copied(),
        Some('h'),
        "b.txt must stay assume-unchanged"
    );
    assert_eq!(tags.get("a.txt").copied(), Some('S'));
    let status = git_status_porcelain_v2_lines(root);
    assert!(
        !status.iter().any(|l| l.contains("b.txt")),
        "git status must ignore local edit on assume-unchanged b.txt, got {status:?}"
    );
}

fn reset_opts(target: grit_lib::objects::ObjectId, mode: ResetMode) -> ResetOptions {
    ResetOptions {
        target,
        mode,
        identity: "T <a@b.c> 0 +0000".parse().expect("identity"),
        message: "reset".into(),
        dry_run: false,
        orphan_limit: DEFAULT_ORPHAN_LIMIT,
    }
}

fn stash_push_opts() -> StashPushOptions {
    StashPushOptions {
        message: None,
        include_untracked: false,
        identity: "T <a@b.c> 0 +0000".parse().expect("identity"),
        selection: None,
        pathspecs: Vec::new(),
        dry_run: false,
        mode: grit_lib::stash::StashMode::default(),
    }
}

#[test]
fn reset_mixed_and_hard_at_head_keep_assume_unchanged() {
    let (dir, repo, head) = seed_assume_unchanged_repo();
    let root = dir.path();
    reset_to_commit(
        &repo,
        &reset_opts(head, ResetMode::Mixed),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("mixed reset");
    assert_b_assume_unchanged(root);
    reset_to_commit(
        &repo,
        &reset_opts(head, ResetMode::Hard),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("hard reset at head");
    assert_b_assume_unchanged(root);
}

fn commit_c_txt_tip(repo: &Repository, root: &std::path::Path) -> grit_lib::objects::ObjectId {
    std::fs::write(root.join("c.txt"), "c\ncommit2\n").expect("second commit");
    git(root, &["add", "c.txt"]);
    git(root, &["commit", "-qm", "other"]);
    *resolve_head_for_repository(repo)
        .expect("tip")
        .oid()
        .expect("commit")
}

fn reapply_assume_unchanged_local_edits(root: &std::path::Path) {
    git(root, &["update-index", "--assume-unchanged", "b.txt"]);
    git(root, &["update-index", "--skip-worktree", "a.txt"]);
    std::fs::write(root.join("b.txt"), "b\nlocal\n").expect("local edit on b");
}

#[test]
fn reset_hard_parent_same_blob_keeps_assume_unchanged_edit() {
    let (dir, repo, init_oid) = seed_assume_unchanged_repo();
    let root = dir.path();
    let tip = commit_c_txt_tip(&repo, root);
    assert_ne!(init_oid, tip, "target tree must differ from reset target");

    reset_to_commit(
        &repo,
        &reset_opts(init_oid, ResetMode::Hard),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("hard reset to init");
    assert_b_assume_unchanged(root);
    assert_git_fsck_strict(root);

    git(root, &["reset", "--hard", &tip.to_hex()]);
    reapply_assume_unchanged_local_edits(root);
    reset_to_commit(
        &repo,
        &reset_opts(init_oid, ResetMode::Mixed),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("mixed reset to init");
    assert_b_assume_unchanged(root);
    assert_git_fsck_strict(root);
}

#[test]
fn reset_mixed_cross_blob_drops_assume_unchanged_keeps_skip_worktree() {
    let (dir, repo, head) = seed_assume_unchanged_repo();
    let root = dir.path();
    git(root, &["update-index", "--no-assume-unchanged", "b.txt"]);
    std::fs::write(root.join("b.txt"), "b\ncommitted-new\n").expect("tree bump");
    git(root, &["add", "b.txt"]);
    git(root, &["commit", "-qm", "bump b in tree"]);
    git(root, &["update-index", "--assume-unchanged", "b.txt"]);
    std::fs::write(root.join("b.txt"), "b\nlocal\n").expect("local");
    reset_to_commit(
        &repo,
        &reset_opts(head, ResetMode::Mixed),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("mixed reset to init");
    let tags = git_ls_files_verbose_map(root);
    assert_eq!(tags.get("b.txt").copied(), Some('H'));
    assert_eq!(tags.get("a.txt").copied(), Some('S'));
}

#[test]
fn fast_forward_merge_other_file_keeps_assume_unchanged() {
    let (dir, repo, _head) = seed_assume_unchanged_repo();
    let root = dir.path();
    git(root, &["switch", "-c", "other"]);
    std::fs::write(root.join("c.txt"), "c\nbranch\n").expect("other commit");
    git(root, &["commit", "-qam", "other"]);
    git(root, &["switch", "main"]);
    let head = *resolve_head_for_repository(&repo)
        .expect("head")
        .oid()
        .expect("commit");
    let other_oid =
        grit_lib::objects::ObjectId::from_hex(git(root, &["rev-parse", "other"]).trim())
            .expect("other oid");
    let outcome = integrate_branch(
        &repo,
        "main",
        head,
        other_oid,
        "other",
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("merge");
    assert!(matches!(
        outcome,
        IntegrateBranchOutcome::FastForward { .. }
    ));
    assert_b_assume_unchanged(root);
}

#[test]
fn stash_push_pop_unrelated_file_keeps_assume_unchanged() {
    let (dir, repo, _head) = seed_assume_unchanged_repo();
    let root = dir.path();
    repo.stash()
        .push(&stash_push_opts(), &mut NullProgress, &CancelToken::never())
        .expect("stash push")
        .expect("had stash");
    assert_b_assume_unchanged(root);
    repo.stash()
        .pop(
            0,
            &StashApplyOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("stash pop");
    assert_b_assume_unchanged(root);
}
