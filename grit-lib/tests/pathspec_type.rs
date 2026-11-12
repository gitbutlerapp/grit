//! `Pathspec` type: magic, case, exclude, and attribute matrix vs `git ls-files`.

use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::status::{StatusOptions, UntrackedMode};
use grit_lib::progress::CancelToken;
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::worktree::pathspec::{Pathspec, PathspecGlobalFlags, PathspecOptions};

fn git_ls_files(root: &std::path::Path, spec: &str) -> Vec<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(root).args(["ls-files"]);
    if !spec.is_empty() {
        cmd.arg(spec);
    }
    clear_git_pathspec_env(&mut cmd);
    let out = cmd.output().expect("git ls-files");
    assert!(out.status.success(), "{:?}", out);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

fn clear_git_pathspec_env(cmd: &mut Command) {
    cmd.env_remove("GIT_LITERAL_PATHSPECS");
    cmd.env_remove("GIT_GLOB_PATHSPECS");
    cmd.env_remove("GIT_NOGLOB_PATHSPECS");
    cmd.env_remove("GIT_ICASE_PATHSPECS");
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
    cmd.env("GIT_CONFIG_SYSTEM", "/dev/null");
}

fn git_status_short_literal(root: &std::path::Path, spec: &str) -> Vec<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(root)
        .args(["status", "--short", "--", spec]);
    clear_git_pathspec_env(&mut cmd);
    cmd.env("GIT_LITERAL_PATHSPECS", "1");
    let out = cmd.output().expect("git status");
    assert!(out.status.success(), "{:?}", out);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let line = line.trim_end();
            if line.len() < 4 {
                return None;
            }
            Some(line[3..].trim().to_owned())
        })
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
fn status_literal_pathspec_matches_git_for_bracket_filename() {
    let dir = init_repo();
    std::fs::write(dir.path().join("foo"), b"f").unwrap();
    Command::new("git")
        .current_dir(dir.path())
        .args(["add", "foo"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(dir.path())
        .args(["commit", "-qm", "add foo"])
        .env("GIT_AUTHOR_EMAIL", "t@e.com")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.com")
        .env("GIT_COMMITTER_NAME", "T")
        .status()
        .unwrap();
    std::fs::write(dir.path().join("foo"), b"dirty").unwrap();
    std::fs::write(dir.path().join("f[o][o]"), b"dirty").unwrap();

    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let globals = PathspecGlobalFlags::default().with_literal_pathspecs(true);
    let options = PathspecOptions::default().with_globals(globals);
    let pathspec = repo.pathspec(["f[o][o]"], &options).expect("pathspec");
    assert!(!pathspec.is_entire_tree());
    assert_eq!(pathspec.specs(), &["f[o][o]"]);
    assert!(
        !pathspec.matches(&repo, "foo").expect("match foo"),
        "literal pathspec must not match foo"
    );
    assert!(
        pathspec
            .matches(&repo, "f[o][o]")
            .expect("match bracket file"),
        "literal pathspec must match f[o][o]"
    );
    assert!(
        !pathspec
            .matches_change(&repo, "foo", None)
            .expect("matches_change foo"),
        "literal pathspec must not match foo via matches_change"
    );
    let model = repo
        .status()
        .run(
            &StatusOptions::default()
                .with_pathspec(pathspec.clone())
                .with_untracked(UntrackedMode::No)
                .with_write_index(false),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("status");
    let grit_unstaged: Vec<String> = model.unstaged.iter().map(|e| e.path().to_owned()).collect();
    let git_unstaged = git_status_short_literal(dir.path(), "f[o][o]");
    assert_eq!(git_unstaged, vec!["f[o][o]".to_owned()]);
    assert_eq!(grit_unstaged, git_unstaged);
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
