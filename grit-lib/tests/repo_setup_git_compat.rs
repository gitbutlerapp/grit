//! Repository discovery vs Git for `GIT_DIR`, `GIT_WORK_TREE`, and ceiling dirs (t1510 core).

use std::path::Path;
use std::process::Command;

use grit_lib::environment::{Environment, RepositoryOptions};
use grit_lib::repo::Repository;
use tempfile::TempDir;

fn git_rev_parse(worktree: &Path, env_pairs: &[(&str, &str)], spec: &str) -> String {
    let mut cmd = Command::new("git");
    cmd.current_dir(worktree)
        .args(["rev-parse", "--git-dir", spec])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    for (k, v) in env_pairs {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("git");
    assert!(
        out.status.success(),
        "git rev-parse failed: {:?}",
        out.stderr
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn grit_open(start: &Path, env: Environment) -> Repository {
    let options = RepositoryOptions {
        environment: env,
        ..RepositoryOptions::empty()
    };
    Repository::discover_with(&options, Some(start)).expect("discover")
}

fn init_nested_repo() -> (TempDir, TempDir) {
    let outer = TempDir::new().expect("outer");
    let inner = TempDir::new().expect("inner");
    for dir in [outer.path(), inner.path()] {
        assert!(Command::new("git")
            .current_dir(dir)
            .args(["init", "-q", "-b", "main", "."])
            .status()
            .expect("init")
            .success());
    }
    (outer, inner)
}

#[test]
fn t1510_repo_setup_matches_git() {
    let (outer, inner) = init_nested_repo();
    let git_dir = inner.path().join(".git");
    let git_dir_s = git_dir.to_string_lossy().into_owned();

    let git_show = git_rev_parse(outer.path(), &[("GIT_DIR", &git_dir_s)], "--show-toplevel");
    let mut env = Environment::empty();
    env.cwd = outer.path().to_path_buf();
    env.git_dir = Some(git_dir_s.clone());
    let repo = grit_open(outer.path(), env);
    let wt = repo.work_tree.as_ref().expect("worktree");
    assert_eq!(
        wt.canonicalize().unwrap_or_else(|_| wt.clone()),
        outer
            .path()
            .canonicalize()
            .unwrap_or_else(|_| outer.path().to_path_buf()),
        "GIT_DIR without GIT_WORK_TREE uses cwd as worktree (git toplevel {git_show:?})"
    );
    assert_eq!(
        repo.git_dir.canonicalize().unwrap_or(repo.git_dir.clone()),
        git_dir.canonicalize().unwrap_or(git_dir)
    );
}

#[test]
fn t1510_repo_setup_git_work_tree_without_git_dir() {
    let dir = TempDir::new().expect("dir");
    assert!(Command::new("git")
        .current_dir(dir.path())
        .args(["init", "-q", "-b", "main", "."])
        .status()
        .unwrap()
        .success());
    let wt = dir.path().to_string_lossy().into_owned();
    let mut env = Environment::empty();
    env.cwd = dir.path().to_path_buf();
    env.git_work_tree = Some(wt.clone());
    let repo = grit_open(dir.path(), env);
    assert_eq!(
        repo.work_tree.as_ref().map(|p| p.as_path()),
        Some(dir.path())
    );
}

#[test]
fn t1504_ceiling_directories_blocks_walk_above_ceiling() {
    let root = TempDir::new().expect("root");
    let ceiling = root.path().join("ceiling");
    let below = ceiling.join("below");
    std::fs::create_dir_all(&below).expect("mkdir");
    let ceiling_s = ceiling.to_string_lossy().into_owned();
    let mut env = Environment::empty();
    env.cwd = below.clone();
    env.git_ceiling_directories = Some(ceiling_s);
    let err = Repository::discover_with(
        &RepositoryOptions {
            environment: env,
            ..RepositoryOptions::empty()
        },
        Some(&below),
    )
    .expect_err("no repository under ceiling-only tree");
    let msg = format!("{err}");
    assert!(
        msg.contains("Not a git repository") || msg.contains("repository"),
        "expected not-a-repo, got {msg}"
    );
}
