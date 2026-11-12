//! `worktree prune` removes worktrees whose `.git` file is gone, like system `git`, even
//! when the worktree directory itself is still there.

use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;
use grit_lib::worktree::PruneWorktreesOptions;
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

/// A repository with two linked worktrees whose `.git` files were deleted, one with
/// its directory left in place and one with its directory removed, and one intact.
fn setup(tmp: &TempDir) -> std::path::PathBuf {
    let main = tmp.path().join("main");
    std::fs::create_dir_all(&main).unwrap();
    git(&main, &["init", "-q", "-b", "main"]);
    git(&main, &["commit", "-q", "--allow-empty", "-m", "one"]);
    for name in ["kept-dir", "gone-dir", "intact"] {
        let wt = tmp.path().join(name);
        git(
            &main,
            &["worktree", "add", "-q", "--detach", wt.to_str().unwrap()],
        );
    }
    std::fs::remove_file(tmp.path().join("kept-dir/.git")).unwrap();
    std::fs::remove_dir_all(tmp.path().join("gone-dir")).unwrap();
    main
}

fn admin_dirs(main: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(main.join(".git/worktrees"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn prune_removes_worktrees_without_git_file_like_git() {
    let grit_tmp = TempDir::new().unwrap();
    let grit_main = setup(&grit_tmp);
    let repo = Repository::discover(Some(&grit_main), &RepositoryOptions::empty()).unwrap();
    repo.worktrees()
        .prune(&PruneWorktreesOptions::default())
        .unwrap();

    let git_tmp = TempDir::new().unwrap();
    let git_main = setup(&git_tmp);
    git(&git_main, &["worktree", "prune"]);

    assert_eq!(admin_dirs(&git_main), ["intact"]);
    assert_eq!(admin_dirs(&grit_main), admin_dirs(&git_main));
}
