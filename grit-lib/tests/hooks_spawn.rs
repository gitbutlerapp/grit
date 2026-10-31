//! Hook spawning: interpreter fallback, typed spawn errors, environment and paths.
//!
//! Regression coverage for hooks that never ran on Windows (issue #1742): scripts must reach an
//! interpreter, the real exit status and output must come back, and a hook that cannot start
//! must be reported as [`HookError::Spawn`] rather than a made-up exit status.

use grit_lib::command_runner::hooks::{
    resolve_hooks_dir, run_commit_hook_checked, run_hook_opts, run_post_checkout, run_post_merge,
    run_post_rewrite, CommitHookEnv, HookError, HookName, HookResult, RewriteKind, RunHookOptions,
};
use grit_lib::command_runner::{
    CommandExit, CommandRunner, CommandSpec, RunningCommand, ShellInvocation,
};
use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::diagnostics::{CollectingDiagnostics, Warning};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::commit::{create_commit, CommitOutcome, CommitRequest};
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
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
    let result = run_hook_opts(
        Some(repo),
        hook,
        &[],
        config.as_ref(),
        RunHookOptions {
            stdout_to_stderr: true,
            ..RunHookOptions::default()
        },
    );
    let output = match &result {
        Ok(r) => r.output().map_or_else(String::new, |o| {
            let mut all = String::from_utf8_lossy(&o.stdout).into_owned();
            all.push_str(&String::from_utf8_lossy(&o.stderr));
            all
        }),
        Err(_) => String::new(),
    };
    (result, output)
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
        matches!(result, Ok(HookResult::Failed { status: 3, .. })),
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
        matches!(result, Ok(HookResult::Success { .. })),
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
        matches!(result, Ok(HookResult::Failed { status: 4, .. })),
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
    assert!(
        matches!(result, Ok(HookResult::Success { .. })),
        "{result:?}"
    );

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
            output,
        } => {
            assert!(output.is_empty(), "no hook ran before the failed start");
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
        matches!(result, Ok(HookResult::Failed { status: 7, .. })),
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
    stage(
        repo,
        &StageOptions::default(),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("stage");
    let ident = "T <t@t> 1 +0000".to_string();
    create_commit(
        repo,
        &{
            let mut o = CommitRequest::default();
            o.message = "msg".into();
            o.author = Some(ident.parse().expect("signature"));
            o.committer = Some(ident.parse().expect("signature"));
            o
        },
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
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

#[cfg(unix)]
#[test]
fn spawn_error_keeps_output_of_hooks_that_already_ran() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git_init(tmp.path());
    std::fs::write(
        tmp.path().join(".git/config"),
        "[core]\n\trepositoryformatversion = 0\n[hook \"lint\"]\n\tcommand = \"echo configured-out; echo configured-err >&2\"\n\tevent = pre-commit\n",
    )
    .expect("config");
    write_hook(
        tmp.path(),
        "pre-commit",
        "#!/definitely/missing/interpreter\nexit 0\n",
    );
    let repo = open(tmp.path(), process_environment(tmp.path()), None);

    match commit_one_file(&repo, tmp.path()) {
        Err(grit_lib::error::Error::Hook(err @ HookError::Spawn { .. })) => {
            let output = err.output().expect("spawn errors carry output");
            assert_eq!(output.stdout, b"configured-out\n");
            assert_eq!(output.stderr, b"configured-err\n");
            let HookError::Spawn { path, source, .. } = err else {
                unreachable!()
            };
            assert_eq!(path, Path::new(".git/hooks/pre-commit"));
            assert_eq!(source.kind(), io::ErrorKind::NotFound);
        }
        other => panic!("expected the hooks-dir script to fail to start, got {other:?}"),
    }
    assert!(
        resolve_head(repo.git_dir()).expect("head").oid().is_none(),
        "no commit is written when pre-commit cannot start"
    );
}

fn commit_with_git(dir: &Path, path: &str, contents: &str) -> ObjectId {
    std::fs::write(dir.join(path), contents).expect("write");
    for args in [
        &["add", path][..],
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@t",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-qm",
            path,
        ][..],
    ] {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?}");
    }
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .expect("rev-parse");
    ObjectId::from_hex(String::from_utf8_lossy(&out.stdout).trim()).expect("oid")
}

#[test]
fn post_operation_hooks_receive_git_arguments() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git_init(tmp.path());
    let old = commit_with_git(tmp.path(), "a.txt", "a\n");
    let new = commit_with_git(tmp.path(), "b.txt", "b\n");
    for hook in ["post-checkout", "post-merge", "post-rewrite"] {
        write_hook(tmp.path(), hook, "exit 0\n");
    }
    let runner = FailingRunner::failing_with([]);
    let repo = open(tmp.path(), Environment::empty(), Some(runner.clone()));

    assert!(run_post_checkout(&repo, Some(old), new, true));
    assert!(run_post_checkout(&repo, None, new, false));
    assert!(run_post_merge(&repo, true));
    assert!(run_post_rewrite(&repo, RewriteKind::Amend, &[(old, new)]));
    assert!(
        !run_post_rewrite(&repo, RewriteKind::Rebase, &[]),
        "an empty rewrite list runs nothing"
    );

    let args: Vec<Vec<OsString>> = runner.specs().into_iter().map(|s| s.args).collect();
    let os = |items: &[&str]| items.iter().map(OsString::from).collect::<Vec<_>>();
    assert_eq!(
        args,
        vec![
            os(&[&old.to_hex(), &new.to_hex(), "1"]),
            os(&[&"0".repeat(40), &new.to_hex(), "0"]),
            os(&["1"]),
            os(&["amend"]),
        ]
    );
}

#[test]
fn switch_with_unstartable_post_checkout_hook_succeeds_with_warning() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git_init(tmp.path());
    let first = commit_with_git(tmp.path(), "a.txt", "a\n");
    commit_with_git(tmp.path(), "b.txt", "b\n");
    write_hook(tmp.path(), "post-checkout", "exit 0\n");
    let runner = FailingRunner::failing_with([io::Error::from(io::ErrorKind::NotFound)]);
    let diagnostics = Arc::new(CollectingDiagnostics::new());
    let opts = RepositoryOptions::with_environment(Environment::empty())
        .with_work_tree(tmp.path())
        .with_command_runner(runner.clone())
        .with_diagnostics(diagnostics.clone());
    let repo = Repository::open(&tmp.path().join(".git"), &opts).expect("open repo");

    let outcome = switch_repository(
        &repo,
        &SwitchOptions {
            mode: SwitchMode::Detached {
                commit: first,
                reflog_to_label: "HEAD~1".into(),
            },
            dry_run: false,
            hooks: grit_lib::PostOperationHooks::Run,
        },
        &mut grit_lib::progress::NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("post-checkout runs after the switch and must not fail it");
    assert!(outcome.hooks_run.is_empty());
    assert_eq!(runner.specs().len(), 1, "post-checkout was attempted");
    assert_eq!(
        resolve_head(repo.git_dir()).expect("head").oid(),
        Some(&first)
    );
    let warned = diagnostics.warnings().into_iter().any(
        |w| matches!(w, Warning::HookNotRun { hook_name, .. } if hook_name == "post-checkout"),
    );
    assert!(warned, "warnings: {:?}", diagnostics.warnings());
}

fn git_in(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "user.name=T", "-c", "user.email=t@t"])
        .args(args)
        .current_dir(dir)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?}");
}

/// Repository at `<tmp>/repo` with `a.txt` and `b.txt` committed by system git, opened through
/// its canonical path (verbatim `\\?\` on Windows, like the CLI's repository discovery).
fn repo_with_two_files(tmp: &Path) -> (PathBuf, Repository) {
    let dir = tmp.join("repo");
    std::fs::create_dir_all(&dir).expect("mkdir repo");
    git_init(&dir);
    std::fs::write(dir.join("a.txt"), b"a\n").expect("write a");
    std::fs::write(dir.join("b.txt"), b"b\n").expect("write b");
    git_in(&dir, &["add", "-A"]);
    git_in(&dir, &["commit", "-q", "-m", "base"]);
    let canonical = dir.canonicalize().expect("canonicalize");
    let repo = open(&canonical, process_environment(&canonical), None);
    (dir, repo)
}

fn commit_staged(
    repo: &Repository,
    dir: &Path,
    message: &str,
) -> grit_lib::error::Result<CommitOutcome> {
    std::fs::write(dir.join("b.txt"), b"b\nc\n").expect("modify b");
    stage(
        repo,
        &StageOptions::default(),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("stage");
    let ident = "T <t@t> 1 +0000".to_string();
    create_commit(
        repo,
        &{
            let mut o = CommitRequest::default();
            o.message = message.into();
            o.author = Some(ident.parse().expect("signature"));
            o.committer = Some(ident.parse().expect("signature"));
            o
        },
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
}

#[test]
fn pre_commit_hook_lists_only_staged_files_with_system_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (dir, repo) = repo_with_two_files(tmp.path());
    write_hook(
        &dir,
        "pre-commit",
        "#!/bin/sh\ngit diff --cached --name-only > ../hook-out.txt 2>&1\nenv > ../hook-env.txt 2>&1\nexit 0\n",
    );

    let outcome = commit_staged(&repo, &dir, "x").expect("commit");
    assert!(outcome.hooks_run.contains(&HookName::PreCommit));

    let listed = std::fs::read_to_string(tmp.path().join("hook-out.txt")).expect("hook output");
    assert_eq!(
        listed.lines().collect::<Vec<_>>(),
        ["b.txt"],
        "git inside the hook must read grit's index: {listed}"
    );
    let env = std::fs::read_to_string(tmp.path().join("hook-env.txt")).expect("hook env");
    let var = |name: &str| {
        env.lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
            .map(str::to_owned)
    };
    let index_file = var("GIT_INDEX_FILE").unwrap_or_else(|| panic!("GIT_INDEX_FILE in {env}"));
    assert!(!index_file.starts_with(r"\\?\"), "{index_file}");
    #[cfg(windows)]
    {
        let path = var("PATH").unwrap_or_else(|| panic!("PATH in {env}"));
        assert!(
            path.contains("/usr/bin"),
            "MSYS tools must be on PATH: {path}"
        );
    }
}

#[test]
fn commit_msg_hook_using_grep_rejects_the_commit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (dir, repo) = repo_with_two_files(tmp.path());
    write_hook(
        &dir,
        "commit-msg",
        "#!/bin/sh\nif grep -q WIP \"$1\"; then echo no WIP >&2; exit 1; fi\n",
    );
    let head_before = resolve_head(repo.git_dir()).expect("head").oid().copied();

    match commit_staged(&repo, &dir, "WIP thing") {
        Err(grit_lib::error::Error::Hook(HookError::Failed {
            hook_name, status, ..
        })) => {
            assert_eq!(hook_name, "commit-msg");
            assert_eq!(status, 1);
        }
        other => panic!("expected commit-msg to reject the commit, got {other:?}"),
    }
    assert_eq!(
        resolve_head(repo.git_dir()).expect("head").oid().copied(),
        head_before
    );
    commit_staged(&repo, &dir, "real change").expect("a message without WIP passes");
}
