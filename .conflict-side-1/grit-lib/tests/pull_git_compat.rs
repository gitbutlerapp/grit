//! `porcelain::pull` integration tests against system `git pull`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::pull::{
    pull, resolve_pull_mode, PullError, PullMode, PullOptions, PullResult,
};
use grit_lib::refs::resolve_ref;
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::transfer::{FetchOptions, TagMode};
use grit_lib::repo::Repository;
use grit_lib::state::resolve_head;
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

fn rev(dir: &Path, spec: &str) -> ObjectId {
    git(dir, &["rev-parse", spec]).trim().parse().expect("oid")
}

fn tree_oid(dir: &Path, commit: &str) -> ObjectId {
    rev(dir, &format!("{commit}^{{tree}}"))
}

fn grit_identity(dir: &Path) {
    git(dir, &["config", "user.name", "T"]);
    git(dir, &["config", "user.email", "t@example.com"]);
}

fn git_allow_fail(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("spawn git");
    assert!(!status.success(), "expected git to fail: {args:?}");
}

fn setup_tracked_pair(tag: &str) -> (PathBuf, PathBuf) {
    let root = unique_tmp("pull-git", tag);
    let origin = root.join("origin");
    fs::create_dir_all(&origin).expect("origin");
    git(&origin, &["init", "-q", "-b", "main"]);
    commit(&origin, "f", "seed");
    git(&root, &["clone", "-q", "origin", "grit"]);
    git(&root, &["clone", "-q", "origin", "git"]);
    let grit_dir = root.join("grit");
    let git_dir = root.join("git");
    grit_identity(&grit_dir);
    grit_identity(&git_dir);
    (grit_dir, git_dir)
}

fn fetch_opts(env: &Environment) -> FetchOptions {
    FetchOptions {
        tags: TagMode::Following,
        environment: env.clone(),
        ..Default::default()
    }
}

fn pull_grit(repo: &Repository, options: &PullOptions) -> grit_lib::porcelain::pull::PullOutcome {
    let mut progress = NoProgress;
    pull(repo, options, &mut progress, None).expect("grit pull")
}

fn try_pull_grit(
    repo: &Repository,
    options: &PullOptions,
) -> Result<grit_lib::porcelain::pull::PullOutcome, PullError> {
    let mut progress = NoProgress;
    pull(repo, options, &mut progress, None)
}

fn assert_fsck(dir: &Path) {
    let outcome = git_fsck(dir, false);
    assert!(outcome.ok, "git fsck failed: {:?}", outcome.msg_ids);
}

fn assert_repo_matches_git(grit_dir: &Path, git_dir: &Path) {
    assert_eq!(
        tree_oid(grit_dir, "HEAD"),
        tree_oid(git_dir, "HEAD"),
        "merged tree must match `git pull`"
    );
    assert_fsck(grit_dir);
    assert_fsck(git_dir);
}

fn git_pull(dir: &Path) {
    git(dir, &["pull", "-q", "--no-rebase"]);
}

fn parent_count(dir: &Path, rev: &str) -> usize {
    git(dir, &["rev-list", "--parents", "-n", "1", rev])
        .split_whitespace()
        .count()
        .saturating_sub(1)
}

#[test]
fn pull_up_to_date_matches_git() {
    let (grit_dir, git_dir) = setup_tracked_pair("uptodate");
    let repo = open(&grit_dir);
    let env = Environment::empty();
    let outcome = pull_grit(
        &repo,
        &PullOptions {
            fetch: fetch_opts(&env),
            ..PullOptions::default()
        },
    );
    assert_eq!(outcome.result, PullResult::UpToDate);
    git_pull(&git_dir);
    assert_repo_matches_git(&grit_dir, &git_dir);
}

#[test]
fn pull_ff_false_merge_commit_when_only_upstream_moved() {
    let (grit_dir, git_dir) = setup_tracked_pair("ff-false");
    let origin = grit_dir.parent().unwrap().join("origin");
    commit(&origin, "f", "remote-only");
    git(&grit_dir, &["fetch", "-q"]);
    git(&git_dir, &["fetch", "-q"]);
    git(&grit_dir, &["config", "pull.ff", "false"]);
    git(&git_dir, &["config", "pull.ff", "false"]);

    let repo = open(&grit_dir);
    let outcome = pull_grit(
        &repo,
        &PullOptions {
            fetch: fetch_opts(&Environment::empty()),
            ..PullOptions::default()
        },
    );
    assert_eq!(outcome.result, PullResult::Merged);
    assert_eq!(parent_count(&grit_dir, "HEAD"), 2, "grit merge commit");

    git_pull(&git_dir);
    assert_eq!(parent_count(&git_dir, "HEAD"), 2, "git merge commit");
    assert_repo_matches_git(&grit_dir, &git_dir);
}

#[test]
fn pull_implicit_upstream_tracking_ref_without_merge_config() {
    let root = unique_tmp("pull-git", "implicit-upstream");
    let origin = root.join("origin");
    fs::create_dir_all(&origin).expect("origin");
    git(&origin, &["init", "-q", "-b", "main"]);
    commit(&origin, "f", "seed");

    let grit_dir = root.join("grit");
    fs::create_dir_all(&grit_dir).expect("grit");
    git(&grit_dir, &["init", "-q", "-b", "main"]);
    git(
        &grit_dir,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&grit_dir, &["config", "branch.main.remote", "origin"]);
    git(
        &grit_dir,
        &["fetch", "-q", "origin", "main:refs/remotes/origin/main"],
    );
    grit_identity(&grit_dir);

    let repo = open(&grit_dir);
    let outcome = pull_grit(
        &repo,
        &PullOptions {
            fetch: fetch_opts(&Environment::empty()),
            ..PullOptions::default()
        },
    );
    assert_eq!(outcome.result, PullResult::SetUpstream);
    assert_eq!(rev(&grit_dir, "HEAD"), rev(&origin, "main"));
}

#[test]
fn pull_refuses_when_configured_merge_ref_did_not_fetch() {
    let (grit_dir, git_dir) = setup_tracked_pair("missing-merge-upstream");
    for dir in [&grit_dir, &git_dir] {
        git(dir, &["config", "branch.main.merge", "refs/heads/missing"]);
        git(dir, &["config", "branch.main.remote", "origin"]);
    }
    let origin = grit_dir.parent().unwrap().join("origin");
    commit(&origin, "f", "advance-main");
    git(&grit_dir, &["fetch", "-q"]);
    git(&git_dir, &["fetch", "-q"]);

    let head_before = rev(&grit_dir, "HEAD");
    assert!(
        resolve_ref(grit_dir.join(".git").as_path(), "refs/remotes/origin/main").is_ok(),
        "origin/main exists but configured upstream is missing"
    );
    assert!(resolve_ref(
        grit_dir.join(".git").as_path(),
        "refs/remotes/origin/missing"
    )
    .is_err());

    let repo = open(&grit_dir);
    let err = try_pull_grit(
        &repo,
        &PullOptions {
            fetch: fetch_opts(&Environment::empty()),
            ..PullOptions::default()
        },
    )
    .expect_err("must not fall back to origin/main");
    assert!(
        matches!(err, PullError::NoUpstream { ref branch } if branch == "main"),
        "unexpected error: {err:?}"
    );
    assert_eq!(rev(&grit_dir, "HEAD"), head_before, "branch unchanged");

    git_allow_fail(&git_dir, &["-c", "pull.rebase=false", "pull", "-q"]);
    assert_eq!(rev(&git_dir, "HEAD"), head_before, "git also refuses pull");
}

#[test]
fn pull_fast_forward_matches_git() {
    let (grit_dir, git_dir) = setup_tracked_pair("ff");
    let origin = grit_dir.parent().unwrap().join("origin");
    commit(&origin, "f", "remote");
    git(&grit_dir, &["fetch", "-q"]);
    git(&git_dir, &["fetch", "-q"]);

    let repo = open(&grit_dir);
    let outcome = pull_grit(
        &repo,
        &PullOptions {
            fetch: fetch_opts(&Environment::empty()),
            ..PullOptions::default()
        },
    );
    assert_eq!(outcome.result, PullResult::FastForward);
    git_pull(&git_dir);
    assert_repo_matches_git(&grit_dir, &git_dir);
}

#[test]
fn pull_merge_matches_git() {
    let (grit_dir, git_dir) = setup_tracked_pair("merge");
    let origin = grit_dir.parent().unwrap().join("origin");
    commit(&grit_dir, "local", "local");
    commit(&git_dir, "local", "local");
    commit(&origin, "remote", "remote");
    git(&grit_dir, &["fetch", "-q"]);
    git(&git_dir, &["fetch", "-q"]);

    let repo = open(&grit_dir);
    let outcome = pull_grit(
        &repo,
        &PullOptions {
            fetch: fetch_opts(&Environment::empty()),
            ..PullOptions::default()
        },
    );
    assert_eq!(outcome.result, PullResult::Merged);
    git_pull(&git_dir);
    assert_repo_matches_git(&grit_dir, &git_dir);
}

#[test]
fn pull_ff_only_refused_leaves_repo_unchanged() {
    let (grit_dir, git_dir) = setup_tracked_pair("ff-only-refused");
    let origin = grit_dir.parent().unwrap().join("origin");
    commit(&grit_dir, "local", "local");
    commit(&git_dir, "local", "local");
    commit(&origin, "remote", "remote");
    git(&grit_dir, &["fetch", "-q"]);
    git(&git_dir, &["fetch", "-q"]);

    let before_head = rev(&grit_dir, "HEAD");
    let before_tree = tree_oid(&grit_dir, "HEAD");
    write_file(&grit_dir, "dirty", "worktree\n");

    let repo = open(&grit_dir);
    let outcome = pull_grit(
        &repo,
        &PullOptions {
            mode: Some(PullMode::FfOnly),
            fetch: fetch_opts(&Environment::empty()),
            ..PullOptions::default()
        },
    );
    assert_eq!(outcome.result, PullResult::RefusedNotFf);
    assert_eq!(rev(&grit_dir, "HEAD"), before_head);
    assert_eq!(tree_oid(&grit_dir, "HEAD"), before_tree);
    assert_eq!(
        fs::read_to_string(grit_dir.join("dirty")).expect("dirty file"),
        "worktree\n"
    );

    let git_out = git(
        &git_dir,
        &["rev-list", "--left-right", "--count", "HEAD...origin/main"],
    );
    let mut parts = git_out.split_whitespace();
    let git_ahead: usize = parts.next().unwrap().parse().unwrap();
    let git_behind: usize = parts.next().unwrap().parse().unwrap();
    assert_eq!(outcome.ahead, Some(git_ahead));
    assert_eq!(outcome.behind, Some(git_behind));

    let git_before = rev(&git_dir, "HEAD");
    git_allow_fail(&git_dir, &["pull", "-q", "--ff-only"]);
    assert_eq!(
        rev(&git_dir, "HEAD"),
        git_before,
        "git ff-only should refuse"
    );
}

#[test]
fn pull_ff_only_from_config_matches_git() {
    let (grit_dir, git_dir) = setup_tracked_pair("config-ff-only");
    let origin = grit_dir.parent().unwrap().join("origin");
    commit(&grit_dir, "local", "local");
    commit(&git_dir, "local", "local");
    commit(&origin, "remote", "remote");
    git(&grit_dir, &["fetch", "-q"]);
    git(&git_dir, &["fetch", "-q"]);
    git(&grit_dir, &["config", "pull.ff", "only"]);
    git(&git_dir, &["config", "pull.ff", "only"]);

    let repo = open(&grit_dir);
    assert_eq!(
        resolve_pull_mode(&repo.config().expect("config"), None),
        PullMode::FfOnly
    );
    let outcome = pull_grit(
        &repo,
        &PullOptions {
            fetch: fetch_opts(&Environment::empty()),
            ..PullOptions::default()
        },
    );
    assert_eq!(outcome.result, PullResult::RefusedNotFf);
    git_allow_fail(&git_dir, &["pull", "-q", "--ff-only"]);
    assert_eq!(rev(&grit_dir, "HEAD"), rev(&git_dir, "HEAD"));
}

#[test]
fn pull_no_rebase_overrides_rebase_config_and_merges() {
    let (grit_dir, git_dir) = setup_tracked_pair("no-rebase");
    let origin = grit_dir.parent().unwrap().join("origin");
    commit(&grit_dir, "local", "local");
    commit(&git_dir, "local", "local");
    commit(&origin, "remote", "remote");
    git(&grit_dir, &["fetch", "-q"]);
    git(&git_dir, &["fetch", "-q"]);
    git(&grit_dir, &["config", "pull.rebase", "true"]);
    git(&git_dir, &["config", "pull.rebase", "true"]);

    let repo = open(&grit_dir);
    let outcome = pull_grit(
        &repo,
        &PullOptions {
            mode: Some(PullMode::Merge),
            fetch: fetch_opts(&Environment::empty()),
            ..PullOptions::default()
        },
    );
    assert_eq!(outcome.result, PullResult::Merged);
    git(&git_dir, &["pull", "-q", "--no-rebase"]);
    assert_repo_matches_git(&grit_dir, &git_dir);
}

#[test]
fn pull_unborn_adoption_matches_git() {
    let root = unique_tmp("pull-git", "unborn");
    let origin = root.join("origin");
    fs::create_dir_all(&origin).expect("origin");
    git(&origin, &["init", "-q", "-b", "main"]);
    commit(&origin, "f", "seed");

    let grit_dir = root.join("grit");
    fs::create_dir_all(&grit_dir).expect("grit dir");
    git(&grit_dir, &["init", "-q", "-b", "main"]);
    git(
        &grit_dir,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&grit_dir, &["config", "branch.main.remote", "origin"]);
    git(
        &grit_dir,
        &["config", "branch.main.merge", "refs/heads/main"],
    );

    let git_dir = root.join("git");
    fs::create_dir_all(&git_dir).expect("git dir");
    git(&git_dir, &["init", "-q", "-b", "main"]);
    git(
        &git_dir,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&git_dir, &["config", "branch.main.remote", "origin"]);
    git(
        &git_dir,
        &["config", "branch.main.merge", "refs/heads/main"],
    );
    grit_identity(&grit_dir);
    grit_identity(&git_dir);

    let repo = open(&grit_dir);
    let outcome = pull_grit(
        &repo,
        &PullOptions {
            fetch: fetch_opts(&Environment::empty()),
            ..PullOptions::default()
        },
    );
    assert_eq!(outcome.result, PullResult::SetUpstream);
    assert!(resolve_head(repo.git_dir()).expect("head").oid().is_some());
    git_pull(&git_dir);
    assert_repo_matches_git(&grit_dir, &git_dir);
    assert_eq!(
        resolve_ref(repo.git_dir(), "refs/heads/main").expect("main"),
        rev(&git_dir, "HEAD")
    );
}
