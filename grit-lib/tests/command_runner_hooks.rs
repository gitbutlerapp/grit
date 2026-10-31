//! RecordingRunner coverage for hooks and shell filters.

use grit_lib::command_runner::hooks::{
    run_commit_hook_checked, run_hook, run_hook_opts, CommitHookEnv, HookError, HookOutput,
    HookResult, HookStdio, RunHookOptions,
};
use grit_lib::command_runner::{
    CommandEnvironment, CommandExit, CommandRunner, CommandSpec, CommandStdin, CommandStdio,
    RecordedResponse, RecordingRunner, RunningCommand,
};
use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::error::{Error, FilterError};
use grit_lib::porcelain::commit::{commit_worktree, CommitRequest};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::worktree::crlf::run_filter;
use std::ffi::OsString;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

const LINT_HOOK: &str =
    "#!/bin/sh\necho \"lint failed: bad stuff\" >&2\necho \"stdout from hook\"\nexit 1\n";

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?}");
}

/// The issue's setup: one commit, a modified file, and a pre-commit hook that prints and fails.
fn repo_with_failing_pre_commit(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.name", "T"]);
    git(dir, &["config", "user.email", "t@e.x"]);
    std::fs::write(dir.join("a"), "a\n").expect("write");
    git(dir, &["add", "a"]);
    git(dir, &["commit", "-qm", "one"]);
    std::fs::write(dir.join("a"), "a\nb\n").expect("append");
    let hook = dir.join(".git/hooks/pre-commit");
    std::fs::create_dir_all(hook.parent().expect("hooks dir")).expect("mkdir");
    std::fs::write(&hook, LINT_HOOK).expect("write hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
}

fn open_repo(dir: &Path, opts: RepositoryOptions) -> Repository {
    Repository::open(&dir.join(".git"), &opts.with_work_tree(dir)).expect("open repo")
}

fn process_options(dir: &Path) -> RepositoryOptions {
    RepositoryOptions::with_environment(Environment::from_vars(
        std::env::vars_os(),
        dir.to_path_buf(),
    ))
}

fn commit_second(repo: &Repository) -> grit_lib::error::Result<grit_lib::objects::ObjectId> {
    let id = "T <t@e.x> 1700000000 +0000".to_string();
    let req = {
        let mut o = CommitRequest::default();
        o.message = "second".into();
        o.author = Some(id.parse().expect("signature"));
        o.committer = Some(id.parse().expect("signature"));
        o
    };
    commit_worktree(
        repo,
        &req,
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .map(|o| o.oid)
}

fn expect_hook_failure(result: grit_lib::error::Result<grit_lib::objects::ObjectId>) -> HookError {
    match result {
        Err(Error::Hook(hook @ HookError::Failed { .. })) => hook,
        other => panic!("expected a failed pre-commit, got {other:?}"),
    }
}

/// Spawns nothing; answers every hook with a canned exit status and output.
struct CannedRunner {
    specs: Mutex<Vec<CommandSpec>>,
    code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl CannedRunner {
    fn new(code: i32, stdout: &[u8], stderr: &[u8]) -> Arc<Self> {
        Arc::new(Self {
            specs: Mutex::default(),
            code,
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        })
    }

    fn specs(&self) -> Vec<CommandSpec> {
        self.specs.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl CommandRunner for CannedRunner {
    fn spawn(&self, spec: &CommandSpec) -> std::io::Result<RunningCommand> {
        self.specs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(spec.clone());
        Ok(RunningCommand::from_recording(
            CommandExit {
                code: Some(self.code),
                success: self.code == 0,
            },
            Some(self.stdout.clone()),
            self.stderr.clone(),
            false,
        ))
    }
}

#[cfg(unix)]
#[test]
fn failing_pre_commit_error_carries_hook_stdout_and_stderr() {
    let tmp = tempfile::tempdir().expect("tempdir");
    repo_with_failing_pre_commit(tmp.path());
    let repo = open_repo(tmp.path(), process_options(tmp.path()));

    let err = expect_hook_failure(commit_second(&repo));
    assert_eq!(err.to_string(), "hook pre-commit failed with exit status 1");
    let HookError::Failed {
        hook_name,
        status,
        output,
    } = err
    else {
        unreachable!()
    };
    assert_eq!(hook_name, "pre-commit");
    assert_eq!(status, 1);
    assert_eq!(output.stdout, b"stdout from hook\n");
    assert_eq!(output.stderr, b"lint failed: bad stuff\n");
}

#[cfg(unix)]
#[test]
fn captured_hook_output_never_reaches_the_process_stderr() {
    const CHILD: &str = "GRIT_TEST_HOOK_OUTPUT_CHILD";
    if let Some(dir) = std::env::var_os(CHILD) {
        let dir = Path::new(&dir);
        let repo = open_repo(dir, process_options(dir));
        expect_hook_failure(commit_second(&repo));
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    repo_with_failing_pre_commit(tmp.path());
    let out = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "captured_hook_output_never_reaches_the_process_stderr",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, tmp.path())
        .output()
        .expect("rerun test binary");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("1 passed"), "{stdout}");
    for line in ["lint failed: bad stuff", "stdout from hook"] {
        assert!(
            !stderr.contains(line),
            "library printed hook output: {stderr}"
        );
        assert!(
            !stdout.contains(line),
            "library printed hook output: {stdout}"
        );
    }
}

#[test]
fn injected_runner_output_goes_into_the_error_only() {
    let tmp = tempfile::tempdir().expect("tempdir");
    repo_with_failing_pre_commit(tmp.path());
    let runner = CannedRunner::new(1, b"runner stdout\n", b"runner stderr\n");
    let repo = open_repo(
        tmp.path(),
        RepositoryOptions::with_environment(Environment::empty())
            .with_command_runner(runner.clone()),
    );

    let err = expect_hook_failure(commit_second(&repo));
    let HookError::Failed { output, .. } = err else {
        unreachable!()
    };
    assert_eq!(
        output,
        HookOutput {
            stdout: b"runner stdout\n".to_vec(),
            stderr: b"runner stderr\n".to_vec(),
        }
    );
    let specs = runner.specs();
    assert_eq!(specs.len(), 1, "only pre-commit runs before the failure");
    #[cfg(unix)]
    assert_eq!(specs[0].program, Path::new(".git/hooks/pre-commit"));
    assert_eq!(specs[0].stdout, CommandStdio::Pipe);
    assert_eq!(specs[0].stderr, CommandStdio::Pipe);
}

#[test]
fn inherit_mode_lets_the_hook_write_to_process_stderr_and_returns_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    repo_with_failing_pre_commit(tmp.path());
    let runner = CannedRunner::new(1, b"ignored stdout\n", b"ignored stderr\n");
    let repo = open_repo(
        tmp.path(),
        RepositoryOptions::with_environment(Environment::empty())
            .with_command_runner(runner.clone())
            .with_hook_stdio(HookStdio::Inherit),
    );
    assert_eq!(repo.hook_stdio(), HookStdio::Inherit);

    let err = expect_hook_failure(commit_second(&repo));
    let HookError::Failed { status, output, .. } = err else {
        unreachable!()
    };
    assert_eq!(status, 1);
    assert!(
        output.is_empty(),
        "inherited output is not collected: {output:?}"
    );
    let spec = runner
        .specs()
        .into_iter()
        .next()
        .expect("pre-commit spawned");
    assert_eq!(spec.stdout, CommandStdio::ParentStderr);
    assert_eq!(spec.stderr, CommandStdio::Inherit);
}

#[test]
fn inherited_pre_push_keeps_stdout_separate() {
    let tmp = tempfile::tempdir().expect("tempdir");
    repo_with_failing_pre_commit(tmp.path());
    std::fs::copy(
        tmp.path().join(".git/hooks/pre-commit"),
        tmp.path().join(".git/hooks/pre-push"),
    )
    .expect("install pre-push");
    let runner = CannedRunner::new(0, b"", b"");
    let repo = open_repo(
        tmp.path(),
        RepositoryOptions::with_environment(Environment::empty())
            .with_command_runner(runner.clone())
            .with_hook_stdio(HookStdio::Inherit),
    );

    let result = run_hook(&repo, "pre-push", &["origin", "url"], None).expect("pre-push");
    assert!(result.is_ok());
    let spec = runner.specs().into_iter().next().expect("pre-push spawned");
    assert_eq!(spec.stdout, CommandStdio::Inherit);
    assert_eq!(spec.stderr, CommandStdio::Inherit);
}

#[cfg(unix)]
#[test]
fn successful_hook_output_is_returned_in_the_result() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q"]);
    let hook = tmp.path().join(".git/hooks/post-checkout");
    std::fs::create_dir_all(hook.parent().expect("hooks dir")).expect("mkdir");
    std::fs::write(&hook, "#!/bin/sh\necho \"args $*\"\necho warn >&2\n").expect("write hook");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let repo = open_repo(tmp.path(), process_options(tmp.path()));

    let result = run_hook(&repo, "post-checkout", &["a", "b", "1"], None).expect("hook");
    assert_eq!(
        result,
        HookResult::Success {
            output: HookOutput {
                stdout: b"args a b 1\n".to_vec(),
                stderr: b"warn\n".to_vec(),
            }
        }
    );
    assert_eq!(result.status(), Some(0));
    assert!(result.was_executed());
}

#[test]
fn configured_and_script_hooks_append_output_in_run_order() {
    let tmp = tempfile::tempdir().expect("tempdir");
    repo_with_failing_pre_commit(tmp.path());
    std::fs::write(
        tmp.path().join(".git/config"),
        "[core]\n\trepositoryformatversion = 0\n[hook \"first\"]\n\tcommand = true\n\tevent = pre-commit\n",
    )
    .expect("config");
    let runner = CannedRunner::new(0, b"o\n", b"e\n");
    let repo = open_repo(
        tmp.path(),
        RepositoryOptions::with_environment(Environment::empty())
            .with_command_runner(runner.clone()),
    );

    let result = run_hook(&repo, "pre-commit", &[], None).expect("hooks");
    assert_eq!(
        runner.specs().len(),
        2,
        "configured hook, then the hooks dir script"
    );
    assert_eq!(
        result.output(),
        Some(&HookOutput {
            stdout: b"o\no\n".to_vec(),
            stderr: b"e\ne\n".to_vec(),
        })
    );
}

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

    let repo =
        Repository::open(&git_dir, &&opts.clone().with_work_tree(tmp.path())).expect("open repo");

    let err = run_commit_hook_checked(&repo, "pre-commit", &[], None, &CommitHookEnv::default())
        .expect_err("hook should fail");
    match err {
        HookError::Failed {
            hook_name,
            status,
            output,
        } => {
            assert_eq!(hook_name, "pre-commit");
            assert_eq!(status, 42);
            assert!(output.is_empty(), "the recording produced no output");
        }
        other => panic!("unexpected error: {other:?}"),
    }

    let specs = runner.specs();
    assert_eq!(specs.len(), 1);
    let prog = &specs[0].program;
    assert!(
        prog.file_stem().is_some_and(|stem| stem == "sh"),
        "unexpected program: {}",
        prog.display()
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

    let repo = Repository::open(
        &tmp.path().join(".git"),
        &&opts.clone().with_work_tree(tmp.path()),
    )
    .expect("open");

    let config = repo.config().expect("config");
    run_hook_opts(
        Some(&repo),
        "pre-commit",
        &[],
        config.as_ref(),
        RunHookOptions::default(),
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
