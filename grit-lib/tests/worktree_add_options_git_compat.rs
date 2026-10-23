//! `worktree add -B`, `--lock`, `--no-checkout`, `--track`, and `prune --dry-run` vs system `git`.

use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::repo::Repository;
use grit_lib::worktree::{
    add_linked_worktree, list_worktrees, prune_linked_worktrees, AddLinkedWorktreeOptions,
    LockWorktreeOptions, PruneWorktreesOptions, WorktreeBranch,
};
use grit_lib::config::environment::RepositoryOptions;
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn init_repo(tmp: &TempDir) -> PathBuf {
    let main = tmp.path().join("main");
    std::fs::create_dir_all(&main).unwrap();
    git(&main, &["init", "-q", "-b", "main"]);
    std::fs::write(main.join("f"), b"1\n").unwrap();
    git(&main, &["add", "f"]);
    git(&main, &["commit", "-qm", "one"]);
    git(&main, &["branch", "topic"]);
    git(&main, &["commit", "--allow-empty", "-qm", "two"]);
    main
}

#[test]
fn worktree_add_force_branch_resets_existing_branch() {
    let tmp = TempDir::new().unwrap();
    let main = init_repo(&tmp);
    let repo = Repository::discover(Some(&main), &RepositoryOptions::empty()).unwrap();
    let old_tip = git(&main, &["rev-parse", "topic"]).trim().to_owned();

    let wt = tmp.path().join("wt-b");
    add_linked_worktree(
        &repo,
        &AddLinkedWorktreeOptions {
            path: wt.clone(),
            branch: Some(WorktreeBranch::CreateOrReset("topic".into())),
            start_point: Some("main".into()),
            force: false,
            lock: None,
            no_checkout: false,
            track: false,
        },
    )
    .expect("add -B");

    let new_tip = git(&main, &["rev-parse", "topic"]);
    let main_tip = git(&main, &["rev-parse", "main"]);
    assert_eq!(new_tip.trim(), main_tip.trim());
    assert_ne!(new_tip.trim(), old_tip.trim());
    git(&main, &["fsck"]);
    git(&wt, &["fsck"]);
}

#[test]
fn worktree_add_lock_writes_reason_git_reads_it() {
    let tmp = TempDir::new().unwrap();
    let main = init_repo(&tmp);
    let repo = Repository::discover(Some(&main), &RepositoryOptions::empty()).unwrap();
    let wt = tmp.path().join("locked-wt");
    add_linked_worktree(
        &repo,
        &AddLinkedWorktreeOptions {
            path: wt.clone(),
            branch: Some(WorktreeBranch::Create("side".into())),
            start_point: Some("HEAD".into()),
            force: false,
            lock: Some(LockWorktreeOptions {
                reason: Some("hold".into()),
            }),
            no_checkout: false,
            track: false,
        },
    )
    .expect("add --lock");

    let porcelain = git(&main, &["worktree", "list", "--porcelain"]);
    assert!(
        porcelain.contains("locked hold"),
        "git must see grit lock:\n{porcelain}"
    );
    git(&main, &["fsck"]);
}

#[test]
fn worktree_add_no_checkout_leaves_tree_empty_git_status_agrees() {
    let tmp = TempDir::new().unwrap();
    let main = init_repo(&tmp);
    let repo = Repository::discover(Some(&main), &RepositoryOptions::empty()).unwrap();
    let wt = tmp.path().join("empty-wt");
    add_linked_worktree(
        &repo,
        &AddLinkedWorktreeOptions {
            path: wt.clone(),
            branch: Some(WorktreeBranch::Create("empty".into())),
            start_point: Some("HEAD".into()),
            force: false,
            lock: None,
            no_checkout: true,
            track: false,
        },
    )
    .expect("add --no-checkout");

    assert!(wt.join(".git").is_file());
    assert!(!wt.join("f").exists());
    let status = git(&wt, &["status", "--porcelain"]);
    assert_eq!(
        status.trim(),
        "D  f",
        "git status must match `git worktree add --no-checkout` (index populated, tree empty)"
    );
    git(&main, &["fsck"]);
}

#[test]
fn worktree_add_track_sets_upstream_git_agrees() {
    let tmp = TempDir::new().unwrap();
    let main = init_repo(&tmp);
    git(
        &main,
        &[
            "remote",
            "add",
            "origin",
            &format!("file://{}", main.display()),
        ],
    );
    git(&main, &["fetch", "origin"]);
    let repo = Repository::discover(Some(&main), &RepositoryOptions::empty()).unwrap();
    let wt = tmp.path().join("track-wt");
    add_linked_worktree(
        &repo,
        &AddLinkedWorktreeOptions {
            path: wt.clone(),
            branch: Some(WorktreeBranch::Create("local-main".into())),
            start_point: Some("origin/main".into()),
            force: false,
            lock: None,
            no_checkout: false,
            track: true,
        },
    )
    .expect("add --track");

    let upstream = git(&wt, &["rev-parse", "@{u}"]);
    let origin_main = git(&main, &["rev-parse", "origin/main"]);
    assert_eq!(upstream.trim(), origin_main.trim());
    git(&main, &["fsck"]);
    git(&wt, &["fsck"]);
}

#[test]
fn worktree_prune_dry_run_deletes_nothing() {
    let tmp = TempDir::new().unwrap();
    let main = init_repo(&tmp);
    let repo = Repository::discover(Some(&main), &RepositoryOptions::empty()).unwrap();
    let wt = tmp.path().join("gone");
    add_linked_worktree(
        &repo,
        &AddLinkedWorktreeOptions {
            path: wt.clone(),
            branch: Some(WorktreeBranch::Create("gone-branch".into())),
            start_point: Some("HEAD".into()),
            force: false,
            lock: None,
            no_checkout: false,
            track: false,
        },
    )
    .unwrap();
    std::fs::remove_dir_all(&wt).unwrap();
    assert_eq!(list_worktrees(&repo).unwrap().len(), 2);

    let would = prune_linked_worktrees(
        &repo,
        &PruneWorktreesOptions { dry_run: true },
    )
    .expect("dry-run prune");
    assert_eq!(would.len(), 1);
    assert_eq!(list_worktrees(&repo).unwrap().len(), 2);

    let pruned =
        prune_linked_worktrees(&repo, &PruneWorktreesOptions::default()).expect("prune");
    assert_eq!(pruned.len(), 1);
    assert_eq!(list_worktrees(&repo).unwrap().len(), 1);
}
