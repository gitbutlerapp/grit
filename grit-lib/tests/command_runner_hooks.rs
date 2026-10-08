//! RecordingRunner coverage for hooks and shell filters.

use grit_lib::command_runner::{
    CommandEnvironment, CommandStdin, CommandStdio, RecordedResponse, RecordingRunner,
};
use grit_lib::crlf::run_filter;
use grit_lib::environment::{Environment, RepositoryOptions};
use grit_lib::error::FilterError;
use grit_lib::hooks::{
    run_commit_hook_checked, run_hook_opts, CommitHookEnv, HookError, RunHookOptions,
};
use grit_lib::repo::Repository;
use std::ffi::OsString;
use std::process::Command;

#[test]
fn recording_runner_pre_commit_hook_argv_and_failure() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    assert!(Command::new("git")
        .args(["init"])
        .current_dir(tmp.path())
        .status()
        .expect("git init")
        .success());

    let git_dir = tmp.path().join(".git");
    std::fs::write(
        git_dir.join("config"),
        "[core]\n\trepositoryformatversion = 0\n[hook \"hc\"]\n\tcommand = exit 42\n\tevent = pre-commit\n",
    )
    .expect("write config");

    let runner = RecordingRunner::always_success();
    runner.push_response(RecordedResponse::Exit(42));

    let mut env = Environment::empty();
    env.cwd = tmp.path().to_path_buf();
    let opts = RepositoryOptions::with_environment(env).with_command_runner(runner.clone());

    let repo = Repository::open_with(&opts, &git_dir, Some(tmp.path())).expect("open repo");

    let err = run_commit_hook_checked(&repo, "pre-commit", &[], None, &CommitHookEnv::default())
        .expect_err("hook should fail");
    match err {
        HookError::Failed { hook_name, status } => {
            assert_eq!(hook_name, "pre-commit");
            assert_eq!(status, 42);
        }
        other => panic!("unexpected error: {other:?}"),
    }

    let specs = runner.specs();
    assert_eq!(specs.len(), 1);
    let prog = specs[0].program.to_string_lossy();
    assert!(
        prog == "sh" || prog == "/bin/sh",
        "unexpected program: {prog}"
    );
}

#[test]
fn hook_environment_exports_repository_snapshot_not_process() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(tmp.path())
        .status()
        .expect("git init")
        .success());

    std::fs::write(
        tmp.path().join(".git/config"),
        "[core]\n\trepositoryformatversion = 0\n[hook \"hc\"]\n\tcommand = true\n\tevent = pre-commit\n",
    )
    .expect("config");

    let runner = RecordingRunner::always_success();
    let mut env = Environment::empty();
    env.cwd = tmp.path().to_path_buf();
    env.home = Some(OsString::from("/snapshot/home"));
    env.git_config_global = Some("/snapshot/home/global.cfg".to_owned());
    let opts = RepositoryOptions::with_environment(env).with_command_runner(runner.clone());

    let repo =
        Repository::open_with(&opts, &tmp.path().join(".git"), Some(tmp.path())).expect("open");

    let config = repo.config().expect("config");
    run_hook_opts(
        Some(&repo),
        "pre-commit",
        &[],
        config.as_ref(),
        RunHookOptions::default(),
        None,
    )
    .expect("hook");

    let spec = runner.specs().into_iter().next().expect("spawn");
    assert!(
        !spec.env.inherit_process,
        "hooks must not inherit the host process environment"
    );
    let home = spec
        .env
        .set
        .iter()
        .find(|(k, _)| k == "HOME")
        .map(|(_, v)| v.to_string_lossy().into_owned());
    assert_eq!(home.as_deref(), Some("/snapshot/home"));
    let global = spec
        .env
        .set
        .iter()
        .find(|(k, _)| k == "GIT_CONFIG_GLOBAL")
        .map(|(_, v)| v.to_string_lossy().into_owned());
    assert_eq!(global.as_deref(), Some("/snapshot/home/global.cfg"));
}

#[test]
fn from_repository_environment_merges_hook_overrides() {
    let mut env = Environment::empty();
    env.home = Some(OsString::from("/snapshot/home"));
    let cmd = CommandEnvironment::from_repository_environment(
        &env,
        &[("GIT_INDEX_FILE".to_owned(), "/tmp/index".to_owned())],
    );
    assert!(!cmd.inherit_process);
    assert!(cmd
        .set
        .iter()
        .any(|(k, v)| k == "HOME" && v == "/snapshot/home"));
    assert!(cmd
        .set
        .iter()
        .any(|(k, v)| k == "GIT_INDEX_FILE" && v == "/tmp/index"));
}

#[test]
fn recording_runner_clean_filter_failure_is_typed() {
    let runner = RecordingRunner::always_success();
    runner.push_response(RecordedResponse::Exit(9));

    let err = run_filter(runner.as_ref(), "false", b"payload", "file.txt")
        .expect_err("filter should fail");
    match err {
        FilterError::Failed { status } => assert_eq!(status, 9),
        other => panic!("unexpected: {other:?}"),
    }

    let spec = runner.specs().into_iter().next().expect("one spawn");
    match spec.stdin {
        CommandStdin::Pipe(ref b) => assert_eq!(b, b"payload"),
        _ => panic!("expected piped stdin"),
    }
    assert_eq!(spec.stdout, CommandStdio::Pipe);
}
