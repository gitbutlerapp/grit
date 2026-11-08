//! Issue #2466: user skip-worktree must survive restore, stash, and reset like Git.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::porcelain::reset::{ResetMode, ResetOptions, DEFAULT_ORPHAN_LIMIT};
use grit_lib::porcelain::restore::{RestoreOptions, RestoreSource};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::stash::{StashApplyOptions, StashPushOptions};
use grit_lib::worktree::WorktreeError;
use grit_test_support::git;

use crate::support::{git_ls_files_verbose_map, git_status_porcelain_v2_lines};
use grit_lib::progress::CancelToken;

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

fn seed_skip_worktree_repo() -> (tempfile::TempDir, Repository, grit_lib::objects::ObjectId) {
    let (dir, repo) = init_repo();
    let root = dir.path();
    std::fs::write(root.join("config.local"), "DB_URL=localhost\n").expect("write");
    std::fs::write(root.join("app.c"), "code\n").expect("write");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);
    git(root, &["update-index", "--skip-worktree", "config.local"]);
    std::fs::write(root.join("config.local"), "DB_URL=prod-password-123\n").expect("override");
    std::fs::write(root.join("app.c"), "code2\n").expect("modify app");
    let head = *repo
        .references()
        .head()
        .expect("head")
        .oid()
        .expect("commit");
    (dir, repo, head)
}

fn assert_config_local_skip_worktree(root: &std::path::Path) {
    let tags = git_ls_files_verbose_map(root);
    assert_eq!(
        tags.get("config.local").copied(),
        Some('S'),
        "config.local must stay skip-worktree"
    );
    let status = git_status_porcelain_v2_lines(root);
    assert!(
        !status.iter().any(|l| l.contains("config.local")),
        "git status must be clean for config.local, got {status:?}"
    );
}

fn restore_opts(pathspecs: &[&str]) -> RestoreOptions {
    RestoreOptions {
        pathspecs: pathspecs.iter().map(|s| (*s).to_string()).collect(),
        pathspec_sources: pathspecs.iter().map(|s| (*s).to_string()).collect(),
        source: RestoreSource::Index,
        staged: false,
        worktree: true,
        stage: None,
        remerge: None,
    }
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
fn restore_dot_leaves_skip_worktree_file_and_flag() {
    let (dir, repo, _head) = seed_skip_worktree_repo();
    let root = dir.path();
    repo.checkout()
        .restore(
            &restore_opts(&["."]),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("restore");
    assert_config_local_skip_worktree(root);
    assert_eq!(
        std::fs::read_to_string(root.join("config.local")).expect("read"),
        "DB_URL=prod-password-123\n"
    );
}

#[test]
fn stash_push_pop_leaves_skip_worktree_untouched() {
    let (dir, repo, _head) = seed_skip_worktree_repo();
    let root = dir.path();
    repo.stash()
        .push(&stash_push_opts(), &mut NullProgress, &CancelToken::never())
        .expect("stash push")
        .expect("had stash");
    assert_eq!(
        std::fs::read_to_string(root.join("config.local")).expect("read after push"),
        "DB_URL=prod-password-123\n"
    );
    assert_config_local_skip_worktree(root);
    repo.stash()
        .pop(
            0,
            &StashApplyOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("stash pop");
    assert_config_local_skip_worktree(root);
    assert_eq!(
        std::fs::read_to_string(root.join("config.local")).expect("read after pop"),
        "DB_URL=prod-password-123\n"
    );
}

#[test]
fn reset_mixed_and_hard_same_tree_keep_skip_worktree() {
    let (dir, repo, head) = seed_skip_worktree_repo();
    let root = dir.path();
    repo.checkout()
        .reset(
            &reset_opts(head, ResetMode::Mixed),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("mixed reset");
    assert_config_local_skip_worktree(root);
    repo.checkout()
        .reset(
            &reset_opts(head, ResetMode::Hard),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("hard reset at head");
    assert_config_local_skip_worktree(root);
}

#[test]
fn reset_hard_parent_same_blob_keeps_skip_worktree_edit() {
    let (dir, repo, head) = seed_skip_worktree_repo();
    let root = dir.path();
    std::fs::write(root.join("app.c"), "code3\n").expect("second commit file");
    git(root, &["add", "app.c"]);
    git(root, &["commit", "-qm", "second"]);
    let parent = *repo
        .references()
        .head()
        .expect("parent")
        .oid()
        .expect("commit");
    assert_ne!(parent, head);
    repo.checkout()
        .reset(
            &reset_opts(parent, ResetMode::Hard),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("hard reset parent");
    assert_config_local_skip_worktree(root);
    assert_eq!(
        std::fs::read_to_string(root.join("config.local")).expect("read"),
        "DB_URL=prod-password-123\n"
    );
}

#[test]
fn reset_mixed_cross_blob_keeps_skip_worktree_flag() {
    let (dir, repo, head) = seed_skip_worktree_repo();
    let root = dir.path();
    git(
        root,
        &["update-index", "--no-skip-worktree", "config.local"],
    );
    std::fs::write(root.join("config.local"), "DB_URL=committed-new\n").expect("tree bump");
    git(root, &["add", "config.local"]);
    git(root, &["commit", "-qm", "bump config in tree"]);
    git(root, &["update-index", "--skip-worktree", "config.local"]);
    std::fs::write(root.join("config.local"), "DB_URL=prod-password-123\n").expect("local");
    repo.checkout()
        .reset(
            &reset_opts(head, ResetMode::Mixed),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("mixed reset to init");
    assert_config_local_skip_worktree(root);
    assert_eq!(
        std::fs::read_to_string(root.join("config.local")).expect("read"),
        "DB_URL=prod-password-123\n"
    );
}

#[test]
fn reset_hard_cross_blob_clean_worktree_keeps_skip_worktree() {
    let (dir, repo, head) = seed_skip_worktree_repo();
    let root = dir.path();
    let init_blob = git(root, &["rev-parse", "HEAD:config.local"])
        .trim()
        .to_string();
    git(
        root,
        &["update-index", "--no-skip-worktree", "config.local"],
    );
    std::fs::write(root.join("config.local"), "DB_URL=committed-new\n").expect("tree bump");
    git(root, &["add", "config.local"]);
    git(root, &["commit", "-qm", "bump config in tree"]);
    std::fs::write(root.join("config.local"), "DB_URL=localhost\n").expect("match index blob");
    git(
        root,
        &[
            "update-index",
            "--cacheinfo",
            "100644",
            &init_blob,
            "config.local",
        ],
    );
    git(root, &["update-index", "--skip-worktree", "config.local"]);
    repo.checkout()
        .reset(
            &reset_opts(head, ResetMode::Hard),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("hard reset to init");
    assert_config_local_skip_worktree(root);
    assert_eq!(
        std::fs::read_to_string(root.join("config.local")).expect("read"),
        "DB_URL=localhost\n"
    );
}

#[test]
fn reset_hard_aborts_when_skip_worktree_path_removed_in_target() {
    let (dir, repo, head) = seed_skip_worktree_repo();
    let root = dir.path();
    std::fs::write(root.join("secret.local"), "private\n").expect("new secret");
    git(root, &["add", "secret.local"]);
    git(root, &["commit", "-qm", "add secret"]);
    git(root, &["update-index", "--skip-worktree", "secret.local"]);
    std::fs::write(root.join("secret.local"), "PRIVATE-EDIT\n").expect("local edit");
    let err = repo
        .checkout()
        .reset(
            &reset_opts(head, ResetMode::Hard),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .unwrap_err();
    assert!(
        matches!(err, Error::Worktree(WorktreeError::WouldOverwrite { .. })),
        "expected WouldOverwrite, got {err:?}"
    );
    assert!(
        root.join("secret.local").exists(),
        "skip-worktree file must survive refused reset"
    );
    let tags = git_ls_files_verbose_map(root);
    assert_eq!(
        tags.get("secret.local").copied(),
        Some('S'),
        "skip-worktree bit must remain after refused reset"
    );
}

#[test]
fn reset_hard_aborts_when_skip_worktree_blob_changes_and_worktree_dirty() {
    let (dir, repo, head) = seed_skip_worktree_repo();
    let root = dir.path();
    git(
        root,
        &["update-index", "--no-skip-worktree", "config.local"],
    );
    std::fs::write(root.join("config.local"), "DB_URL=committed-new\n").expect("tree bump");
    git(root, &["add", "config.local"]);
    git(root, &["commit", "-qm", "bump config in tree"]);
    git(root, &["update-index", "--skip-worktree", "config.local"]);
    std::fs::write(root.join("config.local"), "DB_URL=other-local\n").expect("local edit");
    let err = repo
        .checkout()
        .reset(
            &reset_opts(head, ResetMode::Hard),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .unwrap_err();
    assert!(
        matches!(err, Error::Worktree(WorktreeError::WouldOverwrite { .. })),
        "expected WouldOverwrite, got {err:?}"
    );
}
