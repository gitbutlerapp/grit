//! Hook spawning: interpreter fallback, typed spawn errors, environment and paths.
//!
//! Regression coverage for hooks that never ran on Windows (issue #1742): scripts must reach an
//! interpreter, the real exit status and output must come back, and a hook that cannot start
//! must be reported as [`HookError::Spawn`] rather than a made-up exit status.

use grit_lib::command_runner::hooks::{
    resolve_hooks_dir, run_commit_hook_checked, run_hook_opts, CommitHookEnv, HookError, HookName,
    HookResult, RunHookOptions,
};
use grit_lib::command_runner::{
    CommandExit, CommandRunner, CommandSpec, RunningCommand, ShellInvocation,
};
use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::diagnostics::{CollectingDiagnostics, Warning};
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::commit::{create_commit, CommitOutcome, CommitRequest};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::state::resolve_head;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

#[cfg(unix)]
const NOT_AN_EXECUTABLE: i32 = 8;
#[cfg(windows)]
const NOT_AN_EXECUTABLE: i32 = 193;

fn git_init(dir: &Path) {
    let status = Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir)
        .status()
        .expect("git init");
    assert!(status.success());
}

fn write_hook(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(".git").join("hooks").join(name);
    std::fs::create_dir_all(path.parent().expect("hooks dir")).expect("mkdir hooks");
    std::fs::write(&path, body).expect("write hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    path
}

fn open(dir: &Path, env: Environment, runner: Option<Arc<dyn CommandRunner>>) -> Repository {
    let mut opts = RepositoryOptions::with_environment(env).with_work_tree(dir);
    if let Some(runner) = runner {
        opts = opts.with_command_runner(runner);
    }
    Repository::open(&dir.join(".git"), &opts).expect("open repo")
}

fn process_environment(cwd: &Path) -> Environment {
    Environment::from_vars(std::env::vars_os(), cwd.to_path_buf())
}

fn run_captured(repo: &Repository, hook: &str) -> (Result<HookResult, HookError>, String) {
    let config = repo.config().expect("config");
    let mut captured = Vec::new();
    let result = run_hook_opts(
        Some(repo),
        hook,
        &[],
        config.as_ref(),
        RunHookOptions {
            stdout_to_stderr: true,
            ..RunHookOptions::default()
        },
        Some(&mut captured),
    );
    (result, String::from_utf8_lossy(&captured).into_owned())
}

/// Fails the first spawns with queued errors, then succeeds; records every spec.
#[derive(Default)]
struct FailingRunner {
    failures: Mutex<VecDeque<io::Error>>,
    specs: Mutex<Vec<CommandSpec>>,
}

impl FailingRunner {
    fn failing_with(errors: impl IntoIterator<Item = io::Error>) -> Arc<Self> {
        Arc::new(Self {
            failures: Mutex::new(errors.into_iter().collect()),
            specs: Mutex::default(),
        })
    }

    fn specs(&self) -> Vec<CommandSpec> {
        self.specs.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl CommandRunner for FailingRunner {
    fn spawn(&self, spec: &CommandSpec) -> io::Result<RunningCommand> {
        self.specs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(spec.clone());
        if let Some(err) = self
            .failures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
        {
            return Err(err);
        }
        Ok(RunningCommand::from_recording(
            CommandExit {
                code: Some(0),
                success: true,
            },
            None,
            Vec::new(),
            false,
        ))
    }
}

#[test]
fn script_hook_exit_status_and_output_are_reported() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git_init(tmp.path());
    write_hook(
        tmp.path(),
        "pre-commit",
        "#!/bin/sh\necho hello-from-hook >&2\nexit 3\n",
    );
    let repo = open(tmp.path(), process_environment(tmp.path()), None);

    let (result, output) = run_captured(&repo, "pre-commit");
    assert!(
        matches!(result, Ok(HookResult::Failed(3))),
        "{result:?}, output: {output}"
    );
    assert!(output.contains("hello-from-hook"), "output: {output}");
}

#[test]
fn script_hook_exiting_zero_succeeds_and_commit_hook_passes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git_init(tmp.path());
    write_hook(tmp.path(), "pre-commit", "#!/bin/sh\nexit 0\n");
    let repo = open(tmp.path(), process_environment(tmp.path()), None);

    let (result, output) = run_captured(&repo, "pre-commit");
    assert!(
        matches!(result, Ok(HookResult::Success)),
        "{result:?}, output: {output}"
    );
    run_commit_hook_checked(&repo, "pre-commit", &[], None, &CommitHookEnv::default())
        .expect("pre-commit exiting 0 lets the commit proceed");
}

#[test]
fn script_hook_without_shebang_runs_through_the_shell() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git_init(tmp.path());
    write_hook(tmp.path(), "commit-msg", "echo plain-script >&2\nexit 4\n");
    let repo = open(tmp.path(), process_environment(tmp.path()), None);

    let (result, output) = run_captured(&repo, "commit-msg");
    assert!(
        matches!(result, Ok(HookResult::Failed(4))),
        "{result:?}, output: {output}"
    );
    assert!(output.contains("plain-script"), "output: {output}");
}

#[test]
fn script_refused_by_the_os_is_retried_through_sh() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git_init(tmp.path());
    write_hook(tmp.path(), "pre-commit", "exit 0\n");
    let runner = FailingRunner::failing_with([io::Error::from_raw_os_error(NOT_AN_EXECUTABLE)]);
    let repo = open(tmp.path(), Environment::empty(), Some(runner.clone()));

    let (result, _) = run_captured(&repo, "pre-commit");
    assert!(matches!(result, Ok(HookResult::Success)), "{result:?}");

    let specs = runner.specs();
    assert_eq!(specs.len(), 2, "direct spawn, then the shell retry");
    assert!(specs[0].shell.is_none());
    assert_eq!(specs[0].program, Path::new(".git/hooks/pre-commit"));
    #[cfg(unix)]
    assert_eq!(specs[1].program, Path::new("/bin/sh"));
    assert!(
        specs[1].program.file_stem().is_some_and(|s| s == "sh"),
        "{}",
        specs[1].program.display()
    );
    match &specs[1].shell {
        Some(ShellInvocation::ScriptPath { script_path, .. }) => {
            assert_eq!(script_path, Path::new(".git/hooks/pre-commit"));
        }
        other => panic!("expected sh <script>, got {other:?}"),
    }
}

#[test]
fn hook_that_cannot_start_is_a_typed_spawn_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git_init(tmp.path());
    write_hook(tmp.path(), "pre-commit", "exit 0\n");
    let runner = FailingRunner::failing_with([io::Error::from(io::ErrorKind::NotFound)]);
    let repo = open(tmp.path(), Environment::empty(), Some(runner));

    let err = run_commit_hook_checked(&repo, "pre-commit", &[], None, &CommitHookEnv::default())
        .expect_err("spawn failure must not look like a hook exit status");
    match err {
        HookError::Spawn {
            hook_name,
            path,
            source,
        } => {
            assert_eq!(hook_name, "pre-commit");
            assert_eq!(path, Path::new(".git/hooks/pre-commit"));
            assert_eq!(source.kind(), io::ErrorKind::NotFound);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn missing_shell_for_a_script_is_a_typed_spawn_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git_init(tmp.path());
    write_hook(tmp.path(), "pre-commit", "exit 0\n");
    let runner = FailingRunner::failing_with([
        io::Error::from_raw_os_error(NOT_AN_EXECUTABLE),
        io::Error::from(io::ErrorKind::NotFound),
    ]);
    let repo = open(tmp.path(), Environment::empty(), Some(runner));

    let (result, _) = run_captured(&repo, "pre-commit");
    match result {
        Err(HookError::Spawn { path, source, .. }) => {
            assert!(path.file_stem().is_some_and(|s| s == "sh"), "{path:?}");
            assert_eq!(source.kind(), io::ErrorKind::NotFound);
        }
        other => panic!("unexpected result: {other:?}"),
    }
}

#[test]
fn configured_hook_that_cannot_start_is_a_typed_spawn_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git_init(tmp.path());
    std::fs::write(
        tmp.path().join(".git/config"),
        "[core]\n\trepositoryformatversion = 0\n[hook \"lint\"]\n\tcommand = true\n\tevent = pre-push\n",
    )
    .expect("config");
    let runner = FailingRunner::failing_with([io::Error::from(io::ErrorKind::PermissionDenied)]);
    let repo = open(tmp.path(), Environment::empty(), Some(runner));

    let (result, _) = run_captured(&repo, "pre-push");
    match result {
        Err(HookError::Spawn {
            hook_name, source, ..
        }) => {
            assert_eq!(hook_name, "pre-push");
            assert_eq!(source.kind(), io::ErrorKind::PermissionDenied);
        }
        other => panic!("unexpected result: {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn hook_sees_path_from_the_repository_environment() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git_init(tmp.path());
    let tools = tmp.path().join("tools");
    std::fs::create_dir_all(&tools).expect("mkdir tools");
    let tool = tools.join("grit-hook-test-tool");
    std::fs::write(&tool, "#!/bin/sh\nexit 7\n").expect("write tool");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    write_hook(tmp.path(), "pre-commit", "#!/bin/sh\ngrit-hook-test-tool\n");

    let mut path = OsString::from(&tools);
    path.push(":/usr/bin:/bin");
    let env = Environment::from_vars([(OsString::from("PATH"), path)], tmp.path().to_path_buf());
    let repo = open(tmp.path(), env, None);

    let (result, output) = run_captured(&repo, "pre-commit");
    assert!(
        matches!(result, Ok(HookResult::Failed(7))),
        "tool on the snapshot PATH must be found: {result:?}, output: {output}"
    );
}

#[test]
fn hooks_dir_has_no_verbatim_prefix() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git_init(tmp.path());
    let canonical = tmp.path().canonicalize().expect("canonicalize");
    let repo = open(&canonical, Environment::empty(), None);

    let hooks_dir = resolve_hooks_dir(&repo);
    let shown = hooks_dir.to_string_lossy();
    assert!(!shown.starts_with(r"\\?\"), "{shown}");
    assert!(hooks_dir.ends_with("hooks"), "{shown}");
    let listed = repo.hooks().list(None).expect("list hooks");
    for hook in listed.items {
        let shown = hook.path.to_string_lossy();
        assert!(!shown.starts_with(r"\\?\"), "{shown}");
    }
}

fn commit_one_file(repo: &Repository, dir: &Path) -> grit_lib::error::Result<CommitOutcome> {
    std::fs::write(dir.join("f.txt"), b"data\n").expect("write");
    stage(repo, &StageOptions::default(), &mut NullProgress).expect("stage");
    let ident = "T <t@t> 1 +0000".to_string();
    create_commit(
        repo,
        &CommitRequest {
            message: "msg".into(),
            author: Some(ident.parse().expect("signature")),
            committer: Some(ident.parse().expect("signature")),
            ..Default::default()
        },
        &mut NullProgress,
    )
}

#[test]
fn commit_with_unstartable_pre_commit_hook_fails_with_spawn_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git_init(tmp.path());
    write_hook(tmp.path(), "pre-commit", "exit 0\n");
    let runner = FailingRunner::failing_with([io::Error::from(io::ErrorKind::NotFound)]);
    let repo = open(tmp.path(), Environment::empty(), Some(runner));

    match commit_one_file(&repo, tmp.path()) {
        Err(grit_lib::error::Error::Hook(HookError::Spawn {
            hook_name, source, ..
        })) => {
            assert_eq!(hook_name, "pre-commit");
            assert_eq!(source.kind(), io::ErrorKind::NotFound);
        }
        other => panic!("expected a pre-commit spawn error, got {other:?}"),
    }
    assert!(
        resolve_head(repo.git_dir()).expect("head").oid().is_none(),
        "no commit is written when pre-commit cannot start"
    );
}

#[test]
fn commit_with_unstartable_post_commit_hook_succeeds_with_warning() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git_init(tmp.path());
    write_hook(tmp.path(), "post-commit", "exit 0\n");
    let runner = FailingRunner::failing_with([io::Error::from(io::ErrorKind::NotFound)]);
    let diagnostics = Arc::new(CollectingDiagnostics::new());
    let opts = RepositoryOptions::with_environment(Environment::empty())
        .with_work_tree(tmp.path())
        .with_command_runner(runner.clone())
        .with_diagnostics(diagnostics.clone());
    let repo = Repository::open(&tmp.path().join(".git"), &opts).expect("open repo");

    let outcome = commit_one_file(&repo, tmp.path())
        .expect("post-commit runs after the commit and must not fail it");
    assert!(!outcome.hooks_run.contains(&HookName::PostCommit));
    assert_eq!(runner.specs().len(), 1, "post-commit was attempted");
    assert_eq!(
        resolve_head(repo.git_dir()).expect("head").oid(),
        Some(&outcome.oid)
    );
    let warned = diagnostics.warnings().into_iter().any(|w| match w {
        Warning::HookNotRun { hook_name, detail } => {
            hook_name == "post-commit"
                && detail.contains(".git/hooks/post-commit")
                && detail.contains(&io::Error::from(io::ErrorKind::NotFound).to_string())
        }
        _ => false,
    });
    assert!(warned, "warnings: {:?}", diagnostics.warnings());
}
