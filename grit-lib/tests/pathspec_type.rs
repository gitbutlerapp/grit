//! `Pathspec` type: magic, case, exclude, and attribute matrix vs `git ls-files`.

use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;
use grit_lib::worktree::pathspec::{Pathspec, PathspecGlobalFlags, PathspecOptions};

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
    assert!(out.status.success(), "{:?}", out);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    Command::new("git")
        .current_dir(dir.path())
        .args(["init", "-q", "-b", "main", "."])
        .status()
        .unwrap();
    std::fs::write(dir.path().join("tracked.txt"), b"t").unwrap();
    std::fs::write(dir.path().join("f[o][o]"), b"x").unwrap();
    std::fs::create_dir_all(dir.path().join("sub2")).unwrap();
    std::fs::write(dir.path().join("sub2/keep.txt"), b"k").unwrap();
    std::fs::write(dir.path().join(".gitattributes"), b"*.txt export-subst\n").unwrap();
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

fn grit_paths(repo: &Repository, spec: &str) -> Vec<String> {
    let options = PathspecOptions::default();
    let ps = repo.pathspec([spec], &options).expect("pathspec");
    let all = git_ls_files(repo.work_tree().expect("work tree"), "");
    all.into_iter()
        .filter(|p| ps.matches(repo, p).expect("match"))
        .collect()
}

#[test]
fn pathspec_glob_bracket_matches_git_ls_files() {
    let dir = init_repo();
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let spec = ":(glob)f[o][o]";
    assert_eq!(git_ls_files(dir.path(), spec), vec!["f[o][o]".to_string()]);
    assert_eq!(grit_paths(&repo, spec), vec!["f[o][o]".to_string()]);
}

#[test]
fn pathspec_exclude_matches_git_ls_files() {
    let dir = init_repo();
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let git_all: Vec<_> = git_ls_files(dir.path(), "")
        .into_iter()
        .filter(|p| !p.starts_with("sub2/"))
        .collect();
    let options = PathspecOptions::default();
    let ps = repo.pathspec(["*", ":/!sub2"], &options).expect("pathspec");
    for path in &git_all {
        assert!(ps.matches(&repo, path).expect("match"), "path {path}");
    }
    assert!(!ps.matches(&repo, "sub2/keep.txt").expect("match"));
}

#[test]
fn pathspec_icase_magic_matches_git() {
    let dir = init_repo();
    std::fs::write(dir.path().join("Case.TXT"), b"c").unwrap();
    Command::new("git")
        .current_dir(dir.path())
        .args(["add", "Case.TXT"])
        .status()
        .unwrap();
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let spec = ":(icase)case.txt";
    let git = git_ls_files(dir.path(), spec);
    let grit = grit_paths(&repo, spec);
    assert_eq!(grit, git);
}

#[test]
fn pathspec_entire_tree_matches_all() {
    let dir = init_repo();
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let ps = Pathspec::entire_tree(PathspecGlobalFlags::default());
    assert!(ps.is_entire_tree());
    for path in git_ls_files(dir.path(), "") {
        assert!(ps.matches(&repo, &path).expect("match"));
    }
}
