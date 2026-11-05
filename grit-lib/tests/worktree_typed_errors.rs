//! Linked worktree refusals surface as typed `Error::Worktree` / `Error::Branch` variants,
//! and a refused operation leaves a registry that system Git still reads cleanly.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::porcelain::branch::BranchError;
use grit_lib::repo::Repository;
use grit_lib::repo_handles::LockWorktreeOptions;
use grit_lib::worktree::{
    add_linked_worktree, move_linked_worktree, repair_worktrees, resolve_move_destination,
    AddLinkedWorktreeOptions, MoveWorktreeOptions, WorktreeBranch, WorktreeError,
    WorktreeOperation,
};
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn seed_repo(main: &Path) -> Repository {
    std::fs::create_dir_all(main).unwrap();
    git(main, &["init", "-q", "-b", "main"]);
    std::fs::write(main.join("tracked.txt"), "1\n").unwrap();
    git(main, &["add", "tracked.txt"]);
    git(main, &["commit", "-qm", "seed"]);
    Repository::discover(Some(main), &RepositoryOptions::empty()).expect("discover")
}

fn add_opts(path: PathBuf, branch: Option<WorktreeBranch>) -> AddLinkedWorktreeOptions {
    AddLinkedWorktreeOptions {
        path,
        branch,
        start_point: None,
        force: false,
        lock: None,
        no_checkout: false,
        track: false,
    }
}

fn lock(repo: &Repository, path: &Path, reason: Option<&str>) -> grit_lib::error::Result<()> {
    repo.worktrees().lock(
        path,
        &LockWorktreeOptions {
            reason: reason.map(str::to_owned),
        },
    )
}

#[test]
fn lock_and_unlock_refusals_are_typed() {
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("main");
    let repo = seed_repo(&main);
    let linked = tmp.path().join("linked");
    add_linked_worktree(
        &repo,
        &add_opts(linked.clone(), Some(WorktreeBranch::Create("l".into()))),
    )
    .unwrap();

    assert!(matches!(
        lock(&repo, &main, None),
        Err(Error::Worktree(WorktreeError::MainWorktree {
            operation: WorktreeOperation::Lock
        }))
    ));
    assert!(matches!(
        repo.worktrees().unlock(&main),
        Err(Error::Worktree(WorktreeError::MainWorktree {
            operation: WorktreeOperation::Unlock
        }))
    ));
    assert!(matches!(
        lock(&repo, &tmp.path().join("nowhere"), None),
        Err(Error::Worktree(WorktreeError::NotRegistered { .. }))
    ));
    assert!(matches!(
        repo.worktrees().unlock(&linked),
        Err(Error::Worktree(WorktreeError::NotLocked))
    ));

    lock(&repo, &linked, Some("hold")).unwrap();
    assert!(git(&main, &["worktree", "list", "--porcelain"]).contains("locked hold"));
    match lock(&repo, &linked, None) {
        Err(Error::Worktree(WorktreeError::AlreadyLocked { reason })) => {
            assert_eq!(reason.as_deref(), Some("hold"));
        }
        other => panic!("expected AlreadyLocked, got {other:?}"),
    }
    repo.worktrees().unlock(&linked).unwrap();
    assert!(!git(&main, &["worktree", "list", "--porcelain"]).contains("locked"));
}

#[test]
fn move_refusals_are_typed_and_leave_git_registry_intact() {
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("main");
    let repo = seed_repo(&main);
    let linked = tmp.path().join("linked");
    add_linked_worktree(
        &repo,
        &add_opts(linked.clone(), Some(WorktreeBranch::Create("l".into()))),
    )
    .unwrap();

    let opts = MoveWorktreeOptions::default();
    assert!(matches!(
        move_linked_worktree(&repo, &main, &tmp.path().join("x"), &opts),
        Err(Error::Worktree(WorktreeError::MainWorktree {
            operation: WorktreeOperation::Move
        }))
    ));

    let occupied = tmp.path().join("occupied");
    std::fs::write(&occupied, b"x").unwrap();
    assert!(matches!(
        move_linked_worktree(&repo, &linked, &occupied, &opts),
        Err(Error::Worktree(WorktreeError::PathExists { .. }))
    ));
    assert!(matches!(
        resolve_move_destination(&linked, &occupied.join("child")),
        Err(Error::Worktree(WorktreeError::NotADirectory { path })) if path.ends_with("occupied")
    ));

    lock(&repo, &linked, None).unwrap();
    assert!(matches!(
        move_linked_worktree(&repo, &linked, &tmp.path().join("moved"), &opts),
        Err(Error::Worktree(WorktreeError::Locked { reason: None }))
    ));

    assert!(linked.join(".git").is_file());
    git(&main, &["worktree", "list", "--porcelain"]);
    git(&main, &["fsck", "--no-progress"]);
}

#[test]
fn repair_refusals_are_typed() {
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("main");
    let repo = seed_repo(&main);

    let missing = tmp.path().join("missing");
    assert!(matches!(
        repair_worktrees(&repo, &[missing.as_path()], true),
        Err(Error::Worktree(WorktreeError::PathNotFound { .. }))
    ));

    let file = tmp.path().join("file");
    std::fs::write(&file, b"x").unwrap();
    assert!(matches!(
        repair_worktrees(&repo, &[file.as_path()], true),
        Err(Error::Worktree(WorktreeError::NotADirectory { .. }))
    ));

    let broken = tmp.path().join("broken");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(broken.join(".git"), "gitdir: /no/such/admin\n").unwrap();
    assert!(matches!(
        repair_worktrees(&repo, &[broken.as_path()], true),
        Err(Error::Worktree(WorktreeError::BrokenGitFile { path })) if path.ends_with(".git")
    ));

    assert!(matches!(
        repair_worktrees(&repo, &[main.as_path()], true),
        Err(Error::Worktree(WorktreeError::DotGitIsDirectory { .. }))
    ));
}

#[test]
fn add_branch_refusals_use_branch_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("main");
    let repo = seed_repo(&main);
    git(&main, &["branch", "side"]);
    std::fs::write(main.join("tracked.txt"), "2\n").unwrap();
    git(&main, &["commit", "-qam", "second"]);

    assert!(matches!(
        add_linked_worktree(
            &repo,
            &add_opts(
                tmp.path().join("a"),
                Some(WorktreeBranch::Create("side".into()))
            )
        ),
        Err(Error::Branch(BranchError::AlreadyExists { name })) if name == "side"
    ));
    assert!(matches!(
        add_linked_worktree(
            &repo,
            &add_opts(
                tmp.path().join("b"),
                Some(WorktreeBranch::Existing("nope".into()))
            )
        ),
        Err(Error::Branch(BranchError::NotFound { name })) if name == "nope"
    ));
    assert!(matches!(
        add_linked_worktree(
            &repo,
            &add_opts(
                tmp.path().join("c"),
                Some(WorktreeBranch::Existing("bad..name".into()))
            )
        ),
        Err(Error::Branch(BranchError::InvalidName { .. }))
    ));

    let mut mismatch = add_opts(
        tmp.path().join("d"),
        Some(WorktreeBranch::Existing("side".into())),
    );
    mismatch.start_point = Some("main".into());
    assert!(matches!(
        add_linked_worktree(&repo, &mismatch),
        Err(Error::Worktree(WorktreeError::BranchTipMismatch { branch })) if branch == "side"
    ));

    assert!(matches!(
        add_linked_worktree(
            &repo,
            &add_opts(
                tmp.path().join("e"),
                Some(WorktreeBranch::Existing("main".into()))
            )
        ),
        Err(Error::BranchCheckedOutInWorktree { branch, .. }) if branch == "main"
    ));

    let tree = git(&main, &["rev-parse", "HEAD^{tree}"]);
    let mut not_commit = add_opts(tmp.path().join("f"), Some(WorktreeBranch::Detach));
    not_commit.start_point = Some(tree.trim().to_owned());
    assert!(matches!(
        add_linked_worktree(&repo, &not_commit),
        Err(Error::UnexpectedObjectKind { .. })
    ));

    let occupied = tmp.path().join("occupied");
    std::fs::create_dir_all(&occupied).unwrap();
    std::fs::write(occupied.join("f"), b"x").unwrap();
    assert!(matches!(
        add_linked_worktree(&repo, &add_opts(occupied, Some(WorktreeBranch::Detach))),
        Err(Error::Worktree(WorktreeError::PathExists { .. }))
    ));

    assert_eq!(
        git(&main, &["worktree", "list", "--porcelain"])
            .lines()
            .filter(|l| l.starts_with("worktree "))
            .count(),
        1,
        "refused adds must not register worktrees"
    );
    git(&main, &["fsck", "--no-progress"]);
}
