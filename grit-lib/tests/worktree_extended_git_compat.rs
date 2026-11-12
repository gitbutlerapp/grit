//! Additional linked worktree scenarios for admin layout and branch rules.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;
use grit_lib::worktree::{AddLinkedWorktreeOptions, WorktreeBranch};
use grit_test_support::git;
use std::fs;
use std::path::Path;

fn git_cmd(root: &Path, args: &[&str]) -> String {
    git(root, args)
}

fn init_repo(root: &Path) {
    git_cmd(root, &["init", "-q", "-b", "main", "."]);
    git_cmd(root, &["config", "user.email", "t@example.com"]);
    git_cmd(root, &["config", "user.name", "Test"]);
}

fn discover(main: &Path) -> Repository {
    Repository::discover(Some(main), &RepositoryOptions::empty()).expect("discover")
}

#[test]
fn add_detach_linked_worktree_and_list_locked_state() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let main = tmp.path().join("main");
    fs::create_dir_all(&main).expect("mkdir");
    init_repo(&main);
    fs::write(main.join("f"), "1\n").unwrap();
    git_cmd(&main, &["add", "f"]);
    git_cmd(&main, &["commit", "-qm", "init"]);

    let linked = tmp.path().join("detached");
    let repo = discover(&main);
    repo.worktrees()
        .add(
            &AddLinkedWorktreeOptions::new(linked.clone())
                .with_branch(Some(WorktreeBranch::Detach))
                .with_start_point(Some("HEAD".into())),
        )
        .expect("detach add");

    let entry = repo
        .worktrees()
        .list()
        .expect("list")
        .into_iter()
        .find(|e| e.path == linked.canonicalize().unwrap_or(linked.clone()))
        .expect("listed");
    assert!(entry.head.is_detached());

    let admin = entry.admin_dir;
    fs::write(admin.join("locked"), "reason\n").expect("lock");
    let listed = repo.worktrees().list().expect("list");
    let locked = listed.iter().find(|e| e.path == entry.path).expect("find");
    assert!(locked.is_locked);
    assert_eq!(locked.lock_reason.as_deref(), Some("reason"));

    repo.worktrees()
        .remove(&linked, true)
        .expect("force remove locked");
}

#[test]
fn add_worktree_on_existing_branch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let main = tmp.path().join("main");
    fs::create_dir_all(&main).expect("mkdir");
    init_repo(&main);
    fs::write(main.join("f"), "1\n").unwrap();
    git_cmd(&main, &["add", "f"]);
    git_cmd(&main, &["commit", "-qm", "init"]);
    git_cmd(&main, &["branch", "feature"]);

    let linked = tmp.path().join("feature-wt");
    discover(&main)
        .worktrees()
        .add(
            &AddLinkedWorktreeOptions::new(linked.clone())
                .with_branch(Some(WorktreeBranch::Existing("feature".into()))),
        )
        .expect("existing branch checkout");
    assert!(linked.join("f").is_file());
}

#[test]
fn remove_dirty_worktree_requires_force() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let main = tmp.path().join("main");
    fs::create_dir_all(&main).expect("mkdir");
    init_repo(&main);
    fs::write(main.join("f"), "1\n").unwrap();
    git_cmd(&main, &["add", "f"]);
    git_cmd(&main, &["commit", "-qm", "init"]);

    let linked = tmp.path().join("dirty");
    let repo = discover(&main);
    repo.worktrees()
        .add(
            &AddLinkedWorktreeOptions::new(linked.clone())
                .with_branch(Some(WorktreeBranch::Create("dirty".into())))
                .with_start_point(Some("HEAD".into())),
        )
        .expect("add");
    fs::write(linked.join("f"), "dirty change\n").unwrap();
    assert!(repo.worktrees().remove(&linked, false).is_err());
    repo.worktrees()
        .remove(&linked, true)
        .expect("force remove");
}

#[test]
fn second_worktree_same_branch_is_rejected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let main = tmp.path().join("main");
    fs::create_dir_all(&main).expect("mkdir");
    init_repo(&main);
    fs::write(main.join("f"), "1\n").unwrap();
    git_cmd(&main, &["add", "f"]);
    git_cmd(&main, &["commit", "-qm", "init"]);

    let first = tmp.path().join("first");
    let repo = discover(&main);
    repo.worktrees()
        .add(
            &AddLinkedWorktreeOptions::new(first.clone())
                .with_branch(Some(WorktreeBranch::Create("shared".into())))
                .with_start_point(Some("HEAD".into())),
        )
        .expect("first");

    let second = tmp.path().join("second");
    assert!(repo
        .worktrees()
        .add(
            &AddLinkedWorktreeOptions::new(second)
                .with_branch(Some(WorktreeBranch::Existing("shared".into()))),
        )
        .is_err());
    let found = repo
        .worktrees()
        .find_with_branch_checked_out("shared")
        .expect("find");
    assert!(found.is_some());
    assert!(repo
        .worktrees()
        .ensure_branch_not_checked_out_elsewhere("shared")
        .is_err());
}

#[test]
fn bare_repo_head_does_not_block_worktree_add_on_default_branch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src");
    fs::create_dir_all(&src).expect("mkdir");
    init_repo(&src);
    fs::write(src.join("a"), "a\n").unwrap();
    git_cmd(&src, &["add", "a"]);
    git_cmd(&src, &["commit", "-qm", "a"]);

    let bare = tmp.path().join("bare.git");
    git_cmd(
        tmp.path(),
        &[
            "clone",
            "-q",
            "--bare",
            src.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    assert_eq!(
        git_cmd(&bare, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/main"
    );

    let repo = discover(&bare);
    assert!(
        repo.worktrees()
            .find_with_branch_checked_out("main")
            .expect("find")
            .is_none(),
        "bare HEAD must not count as checking out main"
    );
    assert!(repo
        .worktrees()
        .ensure_branch_not_checked_out_in_any_worktree("main")
        .is_ok());

    let wt = tmp.path().join("wt");
    repo.worktrees()
        .add(
            &AddLinkedWorktreeOptions::new(wt.clone())
                .with_branch(Some(WorktreeBranch::Existing("main".into()))),
        )
        .expect("add main from bare repo");

    git_cmd(&wt, &["status"]);
    git_cmd(&wt, &["fsck"]);
    assert!(wt.join("a").is_file());

    let second = tmp.path().join("wt2");
    assert!(
        repo.worktrees()
            .add(
                &AddLinkedWorktreeOptions::new(second)
                    .with_branch(Some(WorktreeBranch::Existing("main".into()))),
            )
            .is_err(),
        "second checkout of main must still fail"
    );

    repo.worktrees().remove(&wt, true).expect("cleanup");
}
