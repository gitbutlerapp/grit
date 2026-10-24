//! Clone and fetch honor `GIT_SSH_COMMAND`, `GIT_SSH`, and `core.sshCommand`.

#![cfg(unix)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::config::{ConfigFile, ConfigScope};
use grit_lib::progress::NeverCancel;
use grit_lib::remote::clone::{clone, CloneOptions};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::transfer::{FetchOptions, TagMode};
use grit_lib::remote::{ListRefsOptions, Remote};
use grit_lib::repo::Repository;

struct EnvRestore {
    key: &'static str,
    prior: Option<std::ffi::OsString>,
}

impl EnvRestore {
    fn set(key: &'static str, value: &str) -> Self {
        let prior = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, prior }
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        match &self.prior {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

fn git(dir: Option<&Path>, args: &[&str]) {
    let mut cmd = Command::new("git");
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let out = cmd
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn bare_upstream(tmp: &Path) -> PathBuf {
    let work = tmp.join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(Some(&work), &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("a.txt"), "one\n").unwrap();
    git(Some(&work), &["add", "a.txt"]);
    git(Some(&work), &["commit", "-q", "-m", "c1"]);
    let bare = tmp.join("remote.git");
    git(
        Some(&work),
        &["clone", "-q", "--bare", ".", bare.to_str().unwrap()],
    );
    bare
}

fn write_logging_fake_ssh(dir: &Path, log: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let script = dir.join("fake-ssh.sh");
    let body = format!(
        r#"#!/bin/sh
echo "$*" >> {log}
cmd=
for cmd in "$@"; do :; done
case "$cmd" in
  "git-upload-pack "*) cmd="git upload-pack ${{cmd#git-upload-pack }}" ;;
  "git-receive-pack "*) cmd="git receive-pack ${{cmd#git-receive-pack }}" ;;
esac
eval "exec $cmd"
"#,
        log = log.display()
    );
    std::fs::write(&script, body).ok()?;
    let mut perms = std::fs::metadata(&script).ok()?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).ok()?;
    Some(script)
}

fn env_with_ssh_var(key: &str, value: &str) -> Environment {
    Environment::from_vars(
        [
            (OsString::from(key), OsString::from(value)),
            (
                OsString::from("GIT_CONFIG_GLOBAL"),
                OsString::from("/dev/null"),
            ),
            (
                OsString::from("GIT_CONFIG_SYSTEM"),
                OsString::from("/dev/null"),
            ),
        ],
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    )
}

#[test]
fn clone_uses_git_ssh_command() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = bare_upstream(tmp.path());
    let log = tmp.path().join("ssh-calls.log");
    let script = write_logging_fake_ssh(tmp.path(), &log).expect("fake ssh");
    let url = format!("git@localhost:{}", bare.display());

    let mut opts = CloneOptions::new(url, tmp.path().join("dest"));
    opts.environment = env_with_ssh_var("GIT_SSH_COMMAND", script.to_str().unwrap());

    clone(opts, &mut NoProgress, &NeverCancel, None).expect("clone");
    let calls = std::fs::read_to_string(&log).expect("log");
    assert!(
        !calls.trim().is_empty(),
        "GIT_SSH_COMMAND wrapper must be invoked during clone fetch"
    );
}

#[test]
fn clone_uses_git_ssh_program() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = bare_upstream(tmp.path());
    let log = tmp.path().join("ssh-calls.log");
    let script = write_logging_fake_ssh(tmp.path(), &log).expect("fake ssh");
    let url = format!("git@localhost:{}", bare.display());

    let mut opts = CloneOptions::new(url, tmp.path().join("dest"));
    opts.environment = env_with_ssh_var("GIT_SSH", script.to_str().unwrap());

    clone(opts, &mut NoProgress, &NeverCancel, None).expect("clone");
    let calls = std::fs::read_to_string(&log).expect("log");
    assert!(
        !calls.trim().is_empty(),
        "GIT_SSH wrapper must be invoked during clone fetch"
    );
}

#[test]
fn fetch_with_default_options_uses_repository_git_ssh_command() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = bare_upstream(tmp.path());
    let log = tmp.path().join("ssh-calls.log");
    let script = write_logging_fake_ssh(tmp.path(), &log).expect("fake ssh");

    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(Some(&local), &["init", "-q", "-b", "main"]);

    let config_path = local.join(".git/config");
    let content = std::fs::read_to_string(&config_path).unwrap();
    let mut config = ConfigFile::parse(&config_path, &content, ConfigScope::Local).unwrap();
    config
        .set(
            "remote.origin.url",
            &format!("git@localhost:{}", bare.display()),
        )
        .unwrap();
    config
        .set("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*")
        .unwrap();
    config.write().unwrap();

    let repo_env = env_with_ssh_var("GIT_SSH_COMMAND", script.to_str().unwrap());
    let repo = Repository::open(
        &local.join(".git"),
        &RepositoryOptions::with_environment(repo_env).with_work_tree(&local),
    )
    .unwrap();

    let remote = Remote::from_config(&repo.config().unwrap(), "origin").unwrap();
    remote
        .fetch(
            &repo,
            FetchOptions::default(),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("fetch");

    let calls = std::fs::read_to_string(&log).expect("log");
    assert!(
        !calls.trim().is_empty(),
        "repository GIT_SSH_COMMAND must apply when FetchOptions::default() leaves environment empty"
    );
}

#[test]
fn fetch_honors_core_ssh_command_when_env_unset() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = bare_upstream(tmp.path());
    let log = tmp.path().join("ssh-calls.log");
    let script = write_logging_fake_ssh(tmp.path(), &log).expect("fake ssh");

    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(Some(&local), &["init", "-q", "-b", "main"]);

    let config_path = local.join(".git/config");
    let content = std::fs::read_to_string(&config_path).unwrap();
    let mut config = ConfigFile::parse(&config_path, &content, ConfigScope::Local).unwrap();
    config
        .set(
            "remote.origin.url",
            &format!("git@localhost:{}", bare.display()),
        )
        .unwrap();
    config
        .set("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*")
        .unwrap();
    config
        .set("core.sshCommand", script.to_str().unwrap())
        .unwrap();
    config.write().unwrap();

    let repo = Repository::open(
        &local.join(".git"),
        &RepositoryOptions::empty().with_work_tree(&local),
    )
    .unwrap();

    let remote = Remote::from_config(&repo.config().unwrap(), "origin").unwrap();
    remote
        .fetch(
            &repo,
            FetchOptions {
                refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
                tags: TagMode::Following,
                environment: Environment::empty(),
                ..Default::default()
            },
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("fetch");

    let calls = std::fs::read_to_string(&log).expect("log");
    assert!(
        !calls.trim().is_empty(),
        "core.sshCommand wrapper must be invoked when GIT_SSH_* are unset"
    );
}

#[test]
fn list_refs_literal_ssh_url_uses_git_ssh_command_without_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = bare_upstream(tmp.path());
    let log = tmp.path().join("ssh-calls.log");
    let script = write_logging_fake_ssh(tmp.path(), &log).expect("fake ssh");
    let url = format!("git@localhost:{}", bare.display());

    let _ssh = EnvRestore::set("GIT_SSH_COMMAND", script.to_str().unwrap());
    let _global = EnvRestore::set("GIT_CONFIG_GLOBAL", "/dev/null");
    let _system = EnvRestore::set("GIT_CONFIG_SYSTEM", "/dev/null");

    let remote = Remote::from_url(&url).unwrap();
    let refs = remote
        .list_refs(None, &ListRefsOptions::default(), None)
        .expect("list_refs");
    assert!(!refs.is_empty());

    let calls = std::fs::read_to_string(&log).expect("log");
    assert!(
        !calls.trim().is_empty(),
        "GIT_SSH_COMMAND wrapper must be invoked for literal-URL list_refs without a repository"
    );
}
