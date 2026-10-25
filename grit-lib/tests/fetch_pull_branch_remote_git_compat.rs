//! Fetch and pull with no remote named use `branch.<name>.remote` (issue #1856),
//! checked against system `git fetch` / `git pull` on the same layout.

use std::fs;
use std::path::{Path, PathBuf};

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::porcelain::pull::{pull, PullOptions, PullResult};
use grit_lib::progress::NeverCancel;
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::fetch_request::{resolve_fetch_invocation, resolve_remote_argument};
use grit_lib::remote::transfer::FetchOptions;
use grit_lib::repo::Repository;
use grit_lib::state::resolve_head_for_repository;
use grit_test_support::{git, git_fsck, unique_tmp, write_file};

fn commit(dir: &Path, file: &str, message: &str) {
    write_file(dir, file, message);
    git(dir, &["add", file]);
    git(
        dir,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            message,
        ],
    );
}

fn open(dir: &Path) -> Repository {
    let mut env = Environment::empty();
    env.cwd = dir.to_path_buf();
    Repository::discover(Some(dir), &RepositoryOptions::with_environment(env)).expect("open")
}

fn rev(dir: &Path, spec: &str) -> String {
    git(dir, &["rev-parse", spec]).trim().to_owned()
}

/// Two clients (`grit`, `git`) with remotes `origin` (a.git) and `backup` (b.git), `main`
/// tracking `backup/main`. Returns the clients and the new tip pushed to b.git only.
fn setup(tag: &str) -> (PathBuf, PathBuf, String) {
    let root = unique_tmp("fetch-pull-branch-remote", tag);
    fs::create_dir_all(&root).expect("root");
    git(&root, &["init", "-q", "--bare", "-b", "main", "a.git"]);
    git(&root, &["init", "-q", "--bare", "-b", "main", "b.git"]);
    let seed = root.join("seed");
    fs::create_dir_all(&seed).expect("seed");
    git(&seed, &["init", "-q", "-b", "main"]);
    commit(&seed, "f", "one");
    git(&seed, &["push", "-q", "../a.git", "main"]);
    git(&seed, &["push", "-q", "../b.git", "main"]);

    let mut clients = Vec::new();
    for name in ["grit", "git"] {
        git(&root, &["clone", "-q", "-b", "main", "a.git", name]);
        let dir = root.join(name);
        git(&dir, &["remote", "add", "backup", "../b.git"]);
        git(&dir, &["fetch", "-q", "backup"]);
        git(&dir, &["branch", "-q", "-u", "backup/main"]);
        git(&dir, &["config", "user.name", "T"]);
        git(&dir, &["config", "user.email", "t@example.com"]);
        clients.push(dir);
    }

    commit(&seed, "f", "three");
    git(&seed, &["push", "-q", "../b.git", "main"]);
    let tip = rev(&seed, "HEAD");
    let git_dir = clients.pop().expect("git client");
    let grit_dir = clients.pop().expect("grit client");
    (grit_dir, git_dir, tip)
}

#[test]
fn pull_default_options_fast_forward_from_branch_remote_like_git() {
    let (grit_dir, git_dir, tip) = setup("pull");
    let repo = open(&grit_dir);
    let options =
        PullOptions::for_working_branch(&repo, Environment::empty(), None).expect("options");
    assert_eq!(options.fetch.remote_name.as_deref(), Some("backup"));
    let outcome = pull(&repo, &options, &mut NoProgress, &NeverCancel, None).expect("pull");
    assert_eq!(outcome.result, PullResult::FastForward);
    assert_eq!(outcome.label, "backup/main");
    assert_eq!(rev(&grit_dir, "HEAD"), tip);
    assert_eq!(rev(&grit_dir, "refs/remotes/backup/main"), tip);

    git(&git_dir, &["pull", "-q", "--ff-only"]);
    assert_eq!(rev(&git_dir, "HEAD"), tip);
    assert!(
        git_fsck(&grit_dir, false).ok,
        "git fsck on grit pull result"
    );
}

#[test]
fn fetch_without_remote_updates_branch_remote_tracking_ref_like_git() {
    let (grit_dir, git_dir, tip) = setup("fetch");
    let origin_before = rev(&grit_dir, "refs/remotes/origin/main");
    let repo = open(&grit_dir);
    let config = repo.config().expect("config");
    let head = resolve_head_for_repository(&repo).expect("head");
    let invocation = resolve_fetch_invocation(config.as_ref(), None, &[], head.branch_name());
    assert_eq!(invocation.remote, "backup");

    let remote = resolve_remote_argument(config.as_ref(), &invocation.remote).expect("remote");
    remote
        .fetch(
            &repo,
            FetchOptions {
                refspecs: invocation.user_refspecs,
                environment: Environment::empty(),
                ..Default::default()
            },
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("fetch");
    assert_eq!(rev(&grit_dir, "refs/remotes/backup/main"), tip);
    assert_eq!(rev(&grit_dir, "refs/remotes/origin/main"), origin_before);

    git(&git_dir, &["fetch", "-q"]);
    assert_eq!(rev(&git_dir, "refs/remotes/backup/main"), tip);
    assert_eq!(
        rev(&git_dir, "refs/remotes/origin/main"),
        origin_before,
        "system git fetches the same remote"
    );
    assert!(
        git_fsck(&grit_dir, false).ok,
        "git fsck on grit fetch result"
    );
}
