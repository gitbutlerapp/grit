//! A relative `core.hooksPath` resolves against the directory hooks run in (the work tree
//! top, or `$GIT_DIR` when bare), not against the directory the repository was opened from
//! (issue #1799).

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use grit_lib::command_runner::hooks::{
    resolve_hooks_dir, run_commit_hook, CommitHookEnv, HookResult,
};
use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::repo::Repository;
use grit_test_support::git;

fn process_env(cwd: &Path) -> Environment {
    Environment::from_vars(
        std::env::vars_os().filter(|(k, _)| !k.to_string_lossy().starts_with("GIT_")),
        cwd.to_path_buf(),
    )
}

/// `<root>/work` with `core.hooksPath=../hooks2` and a failing `pre-commit` in
/// `<root>/hooks2`. Returns the root and the hook path.
fn fixture(tmp: &tempfile::TempDir) -> (PathBuf, PathBuf) {
    let root = tmp.path().canonicalize().expect("canonical tempdir");
    let work = root.join("work");
    std::fs::create_dir_all(work.join("sub")).expect("mkdir work/sub");
    git(&work, &["init", "-q", "-b", "main", "."]);
    let hooks = root.join("hooks2");
    std::fs::create_dir_all(&hooks).expect("mkdir hooks2");
    let hook = hooks.join("pre-commit");
    std::fs::write(&hook, "#!/bin/sh\nexit 5\n").expect("write hook");
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    git(&work, &["config", "core.hooksPath", "../hooks2"]);
    (root, hook)
}

#[test]
fn relative_hooks_path_resolves_against_work_tree_from_subdirectory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (root, hook) = fixture(&tmp);
    let sub = root.join("work/sub");
    let repo = Repository::discover(
        Some(&sub),
        &RepositoryOptions::with_environment(process_env(&sub)),
    )
    .expect("discover from sub/");

    let dir = resolve_hooks_dir(&repo);
    assert_eq!(
        dir.canonicalize().expect("hooks dir exists"),
        root.join("hooks2")
    );

    let listed = repo.hooks().list(None).expect("list hooks").items;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "pre-commit");
    assert_eq!(listed[0].path.canonicalize().expect("hook path"), hook);

    let result =
        run_commit_hook(&repo, "pre-commit", &[], None, &CommitHookEnv::default()).expect("run");
    assert!(matches!(result, HookResult::Failed(5)), "{result:?}");
}

#[test]
fn relative_hooks_path_is_the_same_from_root_and_subdirectory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (root, _hook) = fixture(&tmp);
    let work = root.join("work");
    let dirs: Vec<PathBuf> = [work.clone(), work.join("sub")]
        .iter()
        .map(|cwd| {
            let repo = Repository::discover(
                Some(cwd),
                &RepositoryOptions::with_environment(process_env(cwd)),
            )
            .expect("discover");
            resolve_hooks_dir(&repo)
        })
        .collect();
    assert_eq!(dirs[0], dirs[1]);
}

#[test]
fn relative_hooks_path_in_bare_repository_resolves_against_git_dir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().canonicalize().expect("canonical").join("r.git");
    git(tmp.path(), &["init", "-q", "--bare", "r.git"]);
    git(&bare, &["config", "core.hooksPath", "custom"]);
    let repo = Repository::open(&bare, &RepositoryOptions::empty()).expect("open bare");
    assert_eq!(resolve_hooks_dir(&repo), bare.join("custom"));
}
