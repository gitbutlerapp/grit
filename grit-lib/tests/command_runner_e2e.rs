//! End-to-end CommandRunner coverage: hooks, filters, and commit abort.

#![cfg(unix)]

use grit_lib::command_runner::{RecordedResponse, RecordingRunner};
use grit_lib::crlf::run_filter;
use grit_lib::environment::{Environment, RepositoryOptions};
use grit_lib::error::FilterError;
use grit_lib::hooks::{run_hook_opts, HookError, RunHookOptions};
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use std::fs;
use std::path::Path;
use std::process::Command;

fn init_repo(root: &Path) -> Repository {
    assert!(Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(root)
        .status()
        .expect("git init")
        .success());
    let git_dir = root.join(".git");
    fs::create_dir_all(git_dir.join("hooks")).expect("hooks dir");
    let mut env = Environment::empty();
    env.cwd = root.to_path_buf();
    Repository::open_with(
        &RepositoryOptions::with_environment(env),
        &git_dir,
        Some(root),
    )
    .expect("open")
}

#[test]
fn system_hook_receives_stdin_on_pre_push() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let _repo = init_repo(tmp.path());
    let hook = tmp.path().join(".git/hooks/pre-push");
    fs::write(&hook, "#!/bin/sh\ncat >.git/hook-stdin.txt\n").expect("write hook");
    let mut perms = fs::metadata(&hook).expect("meta").permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    fs::set_permissions(&hook, perms).expect("chmod");

    let config = _repo.config().expect("config");
    let stdin_data = b"expected hook stdin\n";
    let result = run_hook_opts(
        Some(&_repo),
        "pre-push",
        &[],
        config.as_ref(),
        RunHookOptions {
            stdin_data: Some(stdin_data),
            ..RunHookOptions::default()
        },
        None,
    )
    .expect("run hook");
    assert!(result.is_ok());

    let captured = fs::read(tmp.path().join(".git/hook-stdin.txt")).expect("read capture");
    assert_eq!(captured, stdin_data);
}

#[test]
fn create_commit_aborts_on_pre_commit_hook_failure() {
    let tmp = tempfile::tempdir().expect("tempdir");
    assert!(Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(tmp.path())
        .status()
        .expect("git init")
        .success());
    fs::write(
        tmp.path().join(".git/config"),
        "[core]\n\trepositoryformatversion = 0\n[hook \"hc\"]\n\tcommand = exit 7\n\tevent = pre-commit\n",
    )
    .expect("config");

    let runner = RecordingRunner::always_success();
    runner.push_response(RecordedResponse::Exit(7));

    let mut env = Environment::empty();
    env.cwd = tmp.path().to_path_buf();
    let opts = RepositoryOptions::with_environment(env).with_command_runner(runner);
    let repo =
        Repository::open_with(&opts, &tmp.path().join(".git"), Some(tmp.path())).expect("open");

    fs::write(tmp.path().join("f.txt"), b"data\n").expect("write");
    stage(&repo, &StageOptions::default(), &mut NullProgress).expect("stage");

    let ident = "T <t@t> 1 +0000".to_string();
    let err = create_commit(
        &repo,
        &CommitRequest {
            message: "msg".into(),
            author: ident.clone(),
            committer: ident,
            allow_empty: false,
            sign_override: None,
            amend: false,
        },
        &mut NullProgress,
    )
    .expect_err("commit should fail");
    assert!(matches!(
        err,
        grit_lib::error::Error::Hook(HookError::Failed { .. })
    ));
}

#[test]
fn commit_msg_hook_receives_editmsg_path_argument() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(tmp.path());
    let hook = tmp.path().join(".git/hooks/commit-msg");
    fs::write(
        &hook,
        "#!/bin/sh\nif [ \"$1\" != .git/COMMIT_EDITMSG ]; then exit 23; fi\n",
    )
    .expect("hook");
    let mut perms = fs::metadata(&hook).expect("meta").permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    fs::set_permissions(&hook, perms).expect("chmod");
    fs::write(tmp.path().join("a.txt"), b"x\n").expect("write");
    stage(&repo, &StageOptions::default(), &mut NullProgress).expect("stage");

    let ident = "T <t@t> 1 +0000".to_string();
    create_commit(
        &repo,
        &CommitRequest {
            message: "hello commit".into(),
            author: ident.clone(),
            committer: ident,
            allow_empty: false,
            sign_override: None,
            amend: false,
        },
        &mut NullProgress,
    )
    .expect("commit");

    let msg = fs::read_to_string(tmp.path().join(".git/COMMIT_EDITMSG")).expect("editmsg");
    assert!(msg.contains("hello commit"));
}

#[test]
fn recording_runner_clean_filter_failure_is_still_typed() {
    let runner = RecordingRunner::always_success();
    runner.push_response(RecordedResponse::Exit(9));
    let err = run_filter(runner.as_ref(), "false", b"payload", "file.txt").expect_err("fail");
    assert!(matches!(err, FilterError::Failed { status: 9 }));
}
