//! `worktree add --orphan` vs system `git`.

use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;
use grit_lib::worktree::{AddLinkedWorktreeOptions, WorktreeBranch};
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

fn init_repo(tmp: &TempDir, with_commit: bool) -> PathBuf {
    let main = tmp.path().join("main");
    std::fs::create_dir_all(&main).unwrap();
    git(&main, &["init", "-q", "-b", "main"]);
    if with_commit {
        std::fs::write(main.join("f"), b"1\n").unwrap();
        git(&main, &["add", "f"]);
        git(&main, &["commit", "-qm", "one"]);
    }
    main
}

fn add_orphan(main: &Path, wt: &Path, branch: &str) -> grit_lib::error::Result<()> {
    let repo = Repository::discover(Some(main), &RepositoryOptions::empty()).unwrap();
    repo.worktrees()
        .add(&AddLinkedWorktreeOptions {
            path: wt.to_owned(),
            branch: Some(WorktreeBranch::Orphan(branch.to_owned())),
            start_point: None,
            force: false,
            lock: None,
            no_checkout: false,
            track: false,
        })
        .map(|_| ())
}

/// Files in `dir`, sorted.
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Compares an orphan worktree added by grit with one added by `git`.
fn assert_matches_git(with_commit: bool) {
    let tmp = TempDir::new().unwrap();
    let main = init_repo(&tmp, with_commit);
    let grit_wt = tmp.path().join("grit-wt");
    let git_wt = tmp.path().join("git-wt");
    add_orphan(&main, &grit_wt, "grit-orphan").unwrap();
    git(
        &main,
        &[
            "worktree",
            "add",
            "--orphan",
            "-b",
            "git-orphan",
            git_wt.to_str().unwrap(),
        ],
    );

    // HEAD is the unborn branch, which isn't created.
    let head = |wt: &Path| git(wt, &["symbolic-ref", "HEAD"]);
    assert_eq!(head(&grit_wt), "refs/heads/grit-orphan\n");
    assert_eq!(head(&git_wt), "refs/heads/git-orphan\n");
    let branches = git(&main, &["branch", "--list", "*-orphan"]);
    assert_eq!(branches, "");

    // Nothing is checked out.
    assert_eq!(entries(&grit_wt), entries(&git_wt));
    assert_eq!(entries(&grit_wt), [".git"]);
    assert_eq!(git(&grit_wt, &["status", "--porcelain"]), "");
    assert_eq!(git(&grit_wt, &["ls-files"]), "");

    // Git sees both worktrees.
    let list = git(&main, &["worktree", "list", "--porcelain"]);
    assert!(list.contains("branch refs/heads/grit-orphan"), "{list}");
    assert!(list.contains("branch refs/heads/git-orphan"), "{list}");
}

#[test]
fn worktree_add_orphan_matches_git() {
    assert_matches_git(true);
}

#[test]
fn worktree_add_orphan_without_commits_matches_git() {
    assert_matches_git(false);
}

#[test]
fn worktree_add_orphan_rejects_existing_branch() {
    let tmp = TempDir::new().unwrap();
    let main = init_repo(&tmp, true);
    git(&main, &["branch", "taken"]);
    let wt = tmp.path().join("wt");
    assert!(add_orphan(&main, &wt, "taken").is_err());
    assert!(!wt.join(".git").exists());
}
