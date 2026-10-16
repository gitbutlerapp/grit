//! Pathspec magic vs `git check-pathspec` / `git ls-files` (t6130–t6137 subset).

use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;
use grit_lib::worktree::pathspec::{
    matches_pathspec_list, path_allowed_by_pathspec_list, pathspec_matches,
};
use tempfile::TempDir;

fn git_ls_files(root: &std::path::Path, spec: &str) -> Vec<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(root).args(["ls-files"]);
    if !spec.is_empty() {
        cmd.arg(spec);
    }
    let out = cmd
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git ls-files");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

fn init_repo_with_files() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    Command::new("git")
        .current_dir(dir.path())
        .args(["init", "-q", "-b", "main", "."])
        .status()
        .unwrap();
    std::fs::write(dir.path().join("f[o][o]"), b"x").unwrap();
    std::fs::write(dir.path().join("tracked.txt"), b"t").unwrap();
    std::fs::create_dir_all(dir.path().join("sub2")).unwrap();
    std::fs::write(dir.path().join("sub2/keep.txt"), b"k").unwrap();
    Command::new("git")
        .current_dir(dir.path())
        .args(["add", "."])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(dir.path())
        .args(["commit", "-qm", "c"])
        .env("GIT_AUTHOR_EMAIL", "t@e.com")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.com")
        .env("GIT_COMMITTER_NAME", "T")
        .status()
        .unwrap();
    dir
}

#[test]
fn t6130_pathspec_glob_bracket_filename_matches_git() {
    let dir = init_repo_with_files();
    let spec = ":(glob)f[o][o]";
    let git_paths = git_ls_files(dir.path(), spec);
    assert_eq!(git_paths, vec!["f[o][o]".to_string()]);
    assert!(pathspec_matches(spec, "f[o][o]"));
}

#[test]
fn t6132_pathspec_exclude_matches_git() {
    let dir = init_repo_with_files();
    let specs = vec!["*".to_string(), ":/!sub2".to_string()];
    let git_all: Vec<_> = git_ls_files(dir.path(), "")
        .into_iter()
        .filter(|p| !p.starts_with("sub2/"))
        .collect();
    for path in &git_all {
        assert!(
            path_allowed_by_pathspec_list(&specs, path),
            "grit should include {path}"
        );
    }
    assert!(!path_allowed_by_pathspec_list(&specs, "sub2/keep.txt"));
    assert!(!matches_pathspec_list("sub2/keep.txt", &specs));
    let _repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
}
