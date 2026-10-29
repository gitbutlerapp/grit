//! `Repository::init` keeps a supplied [`Environment`] on the returned handle (issue #2294).

use std::fs;
use std::process::Command;

use grit_lib::config::environment::{ConfigOverride, Environment};
use grit_lib::config::ident_resolve::{IdentRole, IdentityError};
use grit_lib::error::Error;
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::commit::{commit_worktree, CommitRequest};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::{InitOptions, Repository};
use tempfile::TempDir;

fn git_fsck_ok(git_dir: &std::path::Path) {
    let output = Command::new("git")
        .args(["--git-dir", &git_dir.to_string_lossy(), "fsck"])
        .output()
        .expect("spawn git fsck");
    assert!(
        output.status.success(),
        "git fsck failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_global_config(dir: &TempDir, contents: &str) {
    let path = dir.path().join("gitconfig");
    fs::write(&path, contents).expect("write gitconfig");
}

fn env_with_global(global_config: &std::path::Path) -> Environment {
    let mut env = Environment::empty();
    env.git_config_global = Some(global_config.to_string_lossy().into_owned());
    env
}

fn write_user_global_config(dir: &TempDir) {
    write_global_config(
        dir,
        "[user]\n\tname = Init User\n\temail = init@example.com\n",
    );
}

fn stage_all_and_commit_first(repo: &Repository, wt: &std::path::Path) {
    fs::write(wt.join("a.txt"), "hello\n").expect("write");
    stage(
        repo,
        &StageOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("stage");
    commit_worktree(
        repo,
        &CommitRequest {
            message: "first".into(),
            ..CommitRequest::default()
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("commit");
}

#[test]
fn init_with_environment_keeps_identity_for_first_commit() {
    let cfg_home = TempDir::new().expect("tempdir");
    write_user_global_config(&cfg_home);
    let env = env_with_global(&cfg_home.path().join("gitconfig"));
    let wt = TempDir::new().expect("tempdir");
    let repo =
        Repository::init(wt.path(), &InitOptions::new().with_environment(env)).expect("init");
    let committer = repo
        .identity(IdentRole::Committer, repo.identity_now())
        .expect("committer from init environment");
    assert_eq!(committer.email, "init@example.com");
    stage_all_and_commit_first(&repo, wt.path());
    git_fsck_ok(repo.git_dir());
}

#[test]
fn init_default_hermetic_commit_reports_missing_identity() {
    let wt = TempDir::new().expect("tempdir");
    let repo = Repository::init(wt.path(), &InitOptions::default()).expect("init");
    fs::write(wt.path().join("a.txt"), "hello\n").expect("write");
    stage(
        &repo,
        &StageOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("stage");
    let err = commit_worktree(
        &repo,
        &CommitRequest {
            message: "first".into(),
            ..CommitRequest::default()
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .unwrap_err();
    assert!(matches!(
        err,
        Error::Identity(IdentityError::MissingConfiguredEmail {
            role: IdentRole::Committer
        })
    ));
}

#[test]
fn init_bare_with_environment_resolves_identity() {
    let cfg_home = TempDir::new().expect("tempdir");
    write_user_global_config(&cfg_home);
    let env = env_with_global(&cfg_home.path().join("gitconfig"));
    let bare = TempDir::new().expect("tempdir");
    let repo = Repository::init(
        bare.path(),
        &InitOptions::new().with_bare(true).with_environment(env),
    )
    .expect("init bare");
    assert!(repo.work_tree().is_none());
    let committer = repo
        .identity(IdentRole::Committer, repo.identity_now())
        .expect("committer");
    assert_eq!(committer.email, "init@example.com");
}

#[test]
fn init_separate_git_dir_with_environment_commits() {
    let cfg_home = TempDir::new().expect("tempdir");
    write_user_global_config(&cfg_home);
    let env = env_with_global(&cfg_home.path().join("gitconfig"));
    let wt = TempDir::new().expect("tempdir");
    let separate = TempDir::new().expect("tempdir");
    let repo = Repository::init(
        wt.path(),
        &InitOptions::new()
            .with_environment(env)
            .with_separate_git_dir(Some(separate.path().to_path_buf())),
    )
    .expect("init separate");
    assert_eq!(
        repo.work_tree().map(|p| p.to_path_buf()),
        Some(wt.path().to_path_buf())
    );
    stage_all_and_commit_first(&repo, wt.path());
    git_fsck_ok(repo.git_dir());
}

#[test]
fn init_honors_git_config_parameters_for_filesystem_config() {
    let wt = TempDir::new().expect("tempdir");
    let mut env = Environment::empty();
    env.push_config_override(
        &"core.filemode=false"
            .parse::<ConfigOverride>()
            .expect("override"),
    );
    let repo =
        Repository::init(wt.path(), &InitOptions::new().with_environment(env)).expect("init");
    let config = fs::read_to_string(repo.git_dir().join("config")).expect("read config");
    assert!(
        config.contains("filemode = false"),
        "expected core.filemode from init environment, got:\n{config}"
    );
}
