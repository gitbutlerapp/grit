//! Hook execution utilities.
//!
//! Implements Git's multihook model: hooks from `hook.<name>.*` config plus the
//! traditional script in the hooks directory (`core.hooksPath` or `.git/hooks/`).

use crate::command_runner::{
    git_for_windows_tool_dirs, prepend_to_child_path, system_command_runner, CommandEnvironment,
    CommandRunner, CommandSpec, CommandStdin, CommandStdio, RunningCommand, ScriptInterpreter,
    ShellInvocation,
};
use crate::config::{parse_path, ConfigSet};
use crate::environment::Environment;
use crate::git_path::path_without_verbatim_prefix;
use crate::git_path::CanonicalizeExt;
use crate::objects::HashAlgo;
use crate::objects::ObjectId;
use crate::repo::Repository;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;

/// Whether `err` means the OS would not execute the file as a program, so it must be run
/// through a shell (`ENOEXEC` on Unix, `ERROR_BAD_EXE_FORMAT` on Windows).
fn is_not_an_executable(err: &std::io::Error) -> bool {
    #[cfg(unix)]
    const NOT_EXECUTABLE: i32 = 8;
    #[cfg(windows)]
    const NOT_EXECUTABLE: i32 = 193;
    #[cfg(not(any(unix, windows)))]
    const NOT_EXECUTABLE: i32 = -1;
    err.raw_os_error() == Some(NOT_EXECUTABLE)
}

/// Where hook subprocesses send their stdout and stderr.
///
/// The library never writes hook output itself: it either hands the bytes back to the caller
/// or lets the child write straight to this process's standard streams.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HookStdio {
    /// Pipe both streams and return them in [`HookResult`] and [`HookError::Failed`].
    ///
    /// The default, so an embedder (TUI, GUI, server) can show the output in its own way.
    #[default]
    Capture,
    /// The child writes directly to this process's stderr, as Git does: live output, nothing
    /// buffered. A hook's stdout goes to stderr too unless
    /// [`RunHookOptions::stdout_to_stderr`] is false. Returned [`HookOutput`]s are empty.
    Inherit,
}

/// Bytes the hooks of one event wrote while [`HookStdio::Capture`] was in effect.
///
/// When several hooks run for one event (configured `hook.<name>.*` entries, then the hooks
/// directory script), their output is appended in run order.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct HookOutput {
    /// Everything the hooks wrote to stdout.
    pub stdout: Vec<u8>,
    /// Everything the hooks wrote to stderr.
    pub stderr: Vec<u8>,
}

impl std::fmt::Debug for HookOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookOutput")
            .field("stdout", &String::from_utf8_lossy(&self.stdout))
            .field("stderr", &String::from_utf8_lossy(&self.stderr))
            .finish()
    }
}

impl HookOutput {
    /// Whether no output was captured on either stream.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.stdout.is_empty() && self.stderr.is_empty()
    }
}

/// Hook subprocess failure (non-zero exit or spawn error).
#[derive(Debug, Error)]
pub enum HookError {
    /// A hook ran and returned a non-zero exit status.
    ///
    /// The message is one line; the hook's own output is in `output`, never in `Display`.
    #[error("hook {hook_name} failed with exit status {status}")]
    Failed {
        /// Basename of the hook script that failed.
        hook_name: String,
        /// Process exit code from the hook subprocess.
        status: i32,
        /// What the hooks wrote; empty under [`HookStdio::Inherit`].
        output: HookOutput,
    },
    /// The hook (or the shell that runs it) could not be started.
    #[error("failed to run hook {hook_name} ({})", path.display())]
    Spawn {
        /// Hook event name (`pre-commit`, `commit-msg`, …).
        hook_name: String,
        /// Program that failed to start: the hook script, or the interpreter chosen for it.
        path: PathBuf,
        /// Error returned by the [`CommandRunner`] when spawning.
        #[source]
        source: std::io::Error,
        /// What earlier hooks for the same event wrote before this one could not start
        /// (configured `hook.<name>.*` entries run before the hooks directory script); empty
        /// under [`HookStdio::Inherit`].
        output: HookOutput,
    },
    /// Configuration or internal error while resolving hooks.
    #[error("{0}")]
    Internal(String),
}

impl HookError {
    /// Hook output captured before the failure, or `None` for [`HookError::Internal`].
    #[must_use]
    pub fn output(&self) -> Option<&HookOutput> {
        match self {
            HookError::Failed { output, .. } | HookError::Spawn { output, .. } => Some(output),
            HookError::Internal(_) => None,
        }
    }
}

impl From<String> for HookError {
    fn from(value: String) -> Self {
        Self::Internal(value)
    }
}

fn hook_command_env(git_dir: Option<&Path>, extra_env: &[(String, String)]) -> CommandEnvironment {
    let mut env = CommandEnvironment::inherit_process_only();
    if let Some(gd) = git_dir {
        env.set
            .push((OsString::from("GIT_DIR"), gd.as_os_str().to_os_string()));
    }
    for (k, v) in extra_env {
        env.set
            .push((OsString::from(k.as_str()), OsString::from(v.as_str())));
    }
    env
}

/// Environment for commit-style hooks (`GIT_INDEX_FILE`, `GIT_EDITOR`, `GIT_PREFIX`, and extra pairs).
#[derive(Debug, Clone, Default)]
pub struct CommitHookEnv<'a> {
    /// Absolute or cwd-relative index path passed as `GIT_INDEX_FILE`.
    pub index_file: Option<&'a Path>,
    /// When set, overrides `GIT_EDITOR` for the hook subprocess (e.g. `":"` when no editor is used).
    pub git_editor: Option<&'a str>,
    /// When set, used as `GIT_PREFIX`; when unset, derived from the current directory and work tree.
    pub git_prefix: Option<&'a str>,
    /// Additional `KEY=value` pairs for the hook subprocess.
    pub extra_env: &'a [(&'a str, &'a str)],
}

fn absolute_index_path(index_file: &Path, cwd: &Path) -> PathBuf {
    if index_file.is_absolute() {
        index_file.to_path_buf()
    } else {
        cwd.join(index_file)
    }
}

/// `GIT_PREFIX` for the invoking cwd relative to the work tree (Git sets this from the user's
/// `pwd`, not from the hook subprocess cwd, which is usually the work tree root).
fn git_prefix_for_invocation(repo: &Repository, invocation_cwd: &Path) -> String {
    let Some(wt) = repo.work_tree() else {
        return String::new();
    };
    if invocation_cwd == repo.git_dir() {
        return String::new();
    }
    let wt_canon = wt
        .canonicalize_simplified()
        .unwrap_or_else(|_| wt.to_path_buf());
    let wd_canon = invocation_cwd
        .canonicalize_simplified()
        .unwrap_or_else(|_| invocation_cwd.to_path_buf());
    let rel = wd_canon.strip_prefix(&wt_canon).ok();
    let Some(rel) = rel else {
        return String::new();
    };
    let Some(s) = rel.to_str() else {
        return String::new();
    };
    if s.is_empty() {
        return String::new();
    }
    let mut out = s.replace('\\', "/");
    if !out.ends_with('/') {
        out.push('/');
    }
    out
}

fn build_commit_hook_env(
    repo: &Repository,
    _work_dir: &Path,
    opts: &CommitHookEnv<'_>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    if let Some(p) = opts.index_file {
        env.push((
            "GIT_INDEX_FILE".to_string(),
            absolute_index_path(p, &repo.environment().cwd)
                .to_string_lossy()
                .into_owned(),
        ));
    }
    let invocation_cwd = repo.environment().discovery_cwd();
    let prefix = opts
        .git_prefix
        .map(|s| s.to_string())
        .unwrap_or_else(|| git_prefix_for_invocation(repo, &invocation_cwd));
    env.push(("GIT_PREFIX".to_string(), prefix));
    if let Some(ed) = opts.git_editor {
        env.push(("GIT_EDITOR".to_string(), ed.to_string()));
    }
    for (k, v) in opts.extra_env {
        env.push(((*k).to_string(), (*v).to_string()));
    }
    env
}

/// Git `git_parse_maybe_bool`: `Some(true/false)` or `None` if unrecognized.
fn parse_maybe_bool(value: &str) -> Option<bool> {
    let v = value.trim().to_ascii_lowercase();
    match v.as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// Split `hook.<subsection>.<var>` into `(subsection, var)`.
fn parse_hook_config_key(key: &str) -> Option<(&str, &str)> {
    let rest = key.strip_prefix("hook.")?;
    let (subsection, var) = rest.rsplit_once('.')?;
    if subsection.is_empty() || var.is_empty() {
        return None;
    }
    Some((subsection, var))
}

/// Parsed `hook.*` configuration in one pass (Git `hook_config_lookup_all` semantics).
#[derive(Debug, Default)]
struct HookConfigTables {
    /// Friendly name → last-seen command string.
    commands: HashMap<String, String>,
    /// Event name → ordered friendly names (last duplicate wins position).
    event_hooks: HashMap<String, VecDeque<String>>,
    disabled: HashSet<String>,
}

impl HookConfigTables {
    fn apply_entry(&mut self, key: &str, value: Option<&str>) {
        let Some((hook_name, subkey)) = parse_hook_config_key(key) else {
            return;
        };
        let Some(value) = value else {
            return;
        };
        let hook_name = hook_name.to_string();

        match subkey {
            "event" => {
                if value.is_empty() {
                    for hooks in self.event_hooks.values_mut() {
                        hooks.retain(|n| n != &hook_name);
                    }
                } else {
                    let event = value.to_string();
                    let hooks = self.event_hooks.entry(event).or_default();
                    hooks.retain(|n| n != &hook_name);
                    hooks.push_back(hook_name);
                }
            }
            "command" => {
                self.commands.insert(hook_name, value.to_string());
            }
            "enabled" => match parse_maybe_bool(value) {
                Some(false) => {
                    self.disabled.insert(hook_name);
                }
                Some(true) => {
                    self.disabled.remove(&hook_name);
                }
                None => {}
            },
            _ => {}
        }
    }

    fn from_config(config: &ConfigSet) -> Self {
        let mut t = Self::default();
        for e in config.entries() {
            t.apply_entry(&e.key, e.value.as_deref());
        }
        t
    }

    /// Configured hooks for `event`, in order, excluding disabled entries without a command.
    ///
    /// Returns `Err` if a non-disabled hook lacks `hook.<name>.command` (Git dies here).
    fn hooks_for_event(&self, event: &str) -> Result<Vec<(String, String)>, String> {
        let Some(names) = self.event_hooks.get(event) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for name in names {
            if self.disabled.contains(name) {
                continue;
            }
            let Some(cmd) = self.commands.get(name) else {
                return Err(format!(
                    "'hook.{name}.command' must be configured or 'hook.{name}.event' must be removed; aborting."
                ));
            };
            out.push((name.clone(), cmd.clone()));
        }
        Ok(out)
    }
}

/// One hook invocation resolved for an event.
#[derive(Debug)]
enum ResolvedHook {
    Configured { command: String },
    Traditional { path: PathBuf, argv0: PathBuf },
}

/// Resolve the hooks directory from config or fall back to `$GIT_DIR/hooks`.
///
/// A relative `core.hooksPath` is taken relative to the directory hooks run in: the top of
/// the work tree, or `$GIT_DIR` in a bare repository. It does not depend on the directory
/// the repository was opened from.
///
/// Never carries a Windows `\\?\` prefix, so the path can be shown to users and handed to
/// hook interpreters such as Git for Windows' `sh`.
pub fn resolve_hooks_dir(repo: &Repository) -> PathBuf {
    resolve_hooks_dir_for_config(repo, repo.config().ok().as_deref())
}

fn resolve_hooks_dir_for_config(repo: &Repository, config: Option<&ConfigSet>) -> PathBuf {
    let dir = match config.and_then(|cfg| cfg.get("core.hooksPath")) {
        Some(hooks_path) => {
            let p = PathBuf::from(parse_path(repo.environment(), &hooks_path));
            if p.is_absolute() {
                p
            } else {
                hook_run_dir(repo).join(p)
            }
        }
        None => crate::repo::common_git_dir_for_config(repo.git_dir()).join("hooks"),
    };
    path_without_verbatim_prefix(&dir).into_owned()
}

/// Directory hooks run in by default: the work tree top, or `$GIT_DIR` when bare.
fn hook_run_dir(repo: &Repository) -> &Path {
    repo.work_tree().unwrap_or(repo.git_dir())
}

fn hook_argv0(repo: &Repository, hooks_dir: &Path, hook_name: &str, cwd: &Path) -> PathBuf {
    let default_hooks_dir = repo.git_dir().join("hooks");
    if same_dir(hooks_dir, &default_hooks_dir) {
        if cwd == repo.git_dir() {
            return PathBuf::from("hooks").join(hook_name);
        }
        if let Some(work_tree) = repo.work_tree() {
            // Only relativise to `.git/hooks/<name>` when the git dir is literally
            // `<work_tree>/.git`. When `GIT_DIR` points elsewhere (e.g.
            // `GIT_DIR=clone1/.git` run from the parent dir, which makes the work
            // tree default to the process cwd), the relative path would resolve
            // against the wrong directory. In that case fall through to the
            // absolute hook path, mirroring upstream `hook.c`, which absolutises
            // the hook path whenever it runs the hook in a specific directory.
            if cwd == work_tree && same_dir(repo.git_dir(), &work_tree.join(".git")) {
                return PathBuf::from(".git").join("hooks").join(hook_name);
            }
        }
    }
    hooks_dir.join(hook_name)
}

/// Compare two paths for pointing at the same directory, tolerating symlinks and
/// non-canonical components by canonicalising both ends when possible.
fn same_dir(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize_simplified(), b.canonicalize_simplified()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

// `repo`/`meta` are only read on Unix (executable-bit advice); Windows has no
// POSIX permission bits, so the candidate is returned without that check.
#[cfg_attr(not(unix), allow(unused_variables))]
fn traditional_hook_candidate(
    repo: &Repository,
    hooks_dir: &Path,
    hook_name: &str,
) -> Option<PathBuf> {
    let path = hooks_dir.join(hook_name);
    if !path.exists() {
        return None;
    }
    let meta = fs::metadata(&path).ok()?;
    #[cfg(unix)]
    if meta.permissions().mode() & 0o111 == 0 {
        let config = repo.config().ok();
        let show_warning = config
            .as_deref()
            .and_then(|c| c.get("advice.ignoredHook"))
            .map(|v| !matches!(v.to_lowercase().as_str(), "false" | "no" | "off" | "0"))
            .unwrap_or(true);
        if show_warning {
            repo.warn(crate::diagnostics::Warning::NonExecutableHookIgnored {
                hook_name: hook_name.to_string(),
                hook_path: Some(path.clone()),
            });
        }
        return None;
    }
    Some(path)
}

/// Configured hooks only (for out-of-repo `git hook run`).
fn resolve_configured_hooks_only(
    hook_name: &str,
    config: &ConfigSet,
) -> Result<Vec<ResolvedHook>, String> {
    let tables = HookConfigTables::from_config(config);
    let mut seq = Vec::new();
    for (_friendly, command) in tables.hooks_for_event(hook_name)? {
        seq.push(ResolvedHook::Configured { command });
    }
    Ok(seq)
}

/// Build ordered hook list: configured hooks first, then the traditional hookdir script.
fn resolve_hook_sequence(
    repo: &Repository,
    hook_name: &str,
    config: &ConfigSet,
) -> Result<Vec<ResolvedHook>, String> {
    let tables = HookConfigTables::from_config(config);
    let mut seq = Vec::new();
    for (_friendly, command) in tables.hooks_for_event(hook_name)? {
        seq.push(ResolvedHook::Configured { command });
    }
    let hooks_dir = resolve_hooks_dir_for_config(repo, Some(config));
    if let Some(path) = traditional_hook_candidate(repo, &hooks_dir, hook_name) {
        let argv0 = hook_argv0(repo, &hooks_dir, hook_name, hook_run_dir(repo));
        seq.push(ResolvedHook::Traditional { path, argv0 });
    }
    Ok(seq)
}

/// Interpreter basename named by a script's `#!` line, or `None` without one.
///
/// `#!/bin/sh -e` gives `sh` and `#!/usr/bin/env bash` gives `bash`. Options after the
/// interpreter are dropped, as Git for Windows does when it runs scripts itself.
#[cfg_attr(not(windows), allow(dead_code))]
fn shebang_interpreter(head: &[u8]) -> Option<String> {
    let rest = head.strip_prefix(b"#!")?;
    let line_len = rest
        .iter()
        .position(|&b| b == b'\n' || b == b'\r')
        .unwrap_or(rest.len());
    let line = std::str::from_utf8(&rest[..line_len]).ok()?;
    let basename = |word: &str| word.rsplit(['/', '\\']).next().map(str::to_owned);
    let mut words = line.split_ascii_whitespace();
    let mut name = basename(words.next()?)?;
    if name == "env" {
        name = basename(words.find(|w| !w.starts_with('-'))?)?;
    }
    (!name.is_empty()).then_some(name)
}

/// Strip Windows `\\?\` prefixes from the path-valued `GIT_*` variables in `set`.
///
/// Repository paths are canonicalized, which makes them verbatim on Windows, but Git for
/// Windows does not open a verbatim `GIT_INDEX_FILE` (it reads an empty index instead) and
/// MSYS tools cannot open such paths at all.
fn strip_verbatim_prefix_from_path_vars(set: &mut [(OsString, OsString)]) {
    crate::git_path::strip_verbatim_prefix_from_subprocess_path_vars(set);
}

/// Shell for `hook.<name>.command` entries and for scripts the OS will not execute directly.
///
/// See [`crate::command_runner::shell_program`]: `/bin/sh` on Unix, `sh` resolved against the
/// hook's `PATH` on Windows.
fn hook_shell(env: &CommandEnvironment) -> ScriptInterpreter {
    crate::command_runner::shell_interpreter(env)
}

/// Interpreter from the `#!` line of `script` (Windows cannot execute scripts directly).
#[cfg(windows)]
fn script_shebang_interpreter(
    script: &Path,
    env: &CommandEnvironment,
) -> Option<ScriptInterpreter> {
    use std::io::Read;
    let mut head = [0u8; 256];
    let mut file = fs::File::open(script).ok()?;
    let n = file.read(&mut head).ok()?;
    let name = shebang_interpreter(&head[..n])?;
    Some(crate::command_runner::resolve_script_interpreter(
        &name,
        crate::command_runner::child_path_var(env).as_deref(),
    ))
}

/// Stdio and environment shared by every hook subprocess.
struct HookSpawnContext<'a> {
    runner: &'a dyn CommandRunner,
    cwd: &'a Path,
    env: &'a CommandEnvironment,
    stdin_piped: bool,
    stdout: CommandStdio,
    stderr: CommandStdio,
}

impl HookSpawnContext<'_> {
    fn spec(&self, program: PathBuf, shell: Option<ShellInvocation>, args: &[&str]) -> CommandSpec {
        CommandSpec {
            program,
            args: if shell.is_some() {
                Vec::new()
            } else {
                args.iter().map(OsString::from).collect()
            },
            shell,
            cwd: Some(path_without_verbatim_prefix(self.cwd).into_owned()),
            env: self.env.clone(),
            stdin: if self.stdin_piped {
                CommandStdin::Pipe(Vec::new())
            } else {
                CommandStdin::Inherit
            },
            stdout: self.stdout,
            stderr: self.stderr,
        }
    }

    /// Spec running `interpreter` with `invocation`; an interpreter from a Git for Windows
    /// tree also gets that tree's tool directories in front of `PATH`, as Git gives hooks.
    fn interpreter_spec(
        &self,
        interpreter: ScriptInterpreter,
        invocation: ShellInvocation,
        args: &[&str],
    ) -> CommandSpec {
        let mut spec = self.spec(interpreter.program, Some(invocation), args);
        if let Some(root) = &interpreter.git_root {
            prepend_to_child_path(&mut spec.env, &git_for_windows_tool_dirs(root));
        }
        spec
    }

    fn spawn(&self, spec: &CommandSpec, hook_name: &str) -> Result<RunningCommand, HookError> {
        self.runner.spawn(spec).map_err(|source| HookError::Spawn {
            hook_name: hook_name.to_owned(),
            path: spec.program.clone(),
            source,
            output: HookOutput::default(),
        })
    }

    /// Run `script` (spelled `argv0` for the child) directly, or through a shell when the OS
    /// will not execute it. On Windows a script with a `#!` line goes straight to that
    /// interpreter.
    #[cfg_attr(not(windows), allow(unused_variables))]
    fn spawn_traditional(
        &self,
        hook_name: &str,
        script: &Path,
        argv0: &Path,
        args: &[&str],
    ) -> Result<RunningCommand, HookError> {
        let argv0 = path_without_verbatim_prefix(argv0);
        let via_shell = |shell: ScriptInterpreter| {
            let invocation = ShellInvocation::ScriptPath {
                script_path: argv0.to_path_buf(),
                args: args.iter().map(OsString::from).collect(),
            };
            self.interpreter_spec(shell, invocation, args)
        };
        #[cfg(windows)]
        if let Some(interpreter) = script_shebang_interpreter(script, self.env) {
            return self.spawn(&via_shell(interpreter), hook_name);
        }
        let direct = self.spec(argv0.to_path_buf(), None, args);
        match self.runner.spawn(&direct) {
            Err(e) if is_not_an_executable(&e) => {
                self.spawn(&via_shell(hook_shell(self.env)), hook_name)
            }
            Err(source) => Err(HookError::Spawn {
                hook_name: hook_name.to_owned(),
                path: direct.program,
                source,
                output: HookOutput::default(),
            }),
            Ok(child) => Ok(child),
        }
    }

    /// Run a `hook.<name>.command` entry as `sh -c <command> hook <args>…`.
    fn spawn_configured(
        &self,
        hook_name: &str,
        command: &str,
        args: &[&str],
    ) -> Result<RunningCommand, HookError> {
        let invocation = ShellInvocation::DashC {
            script: command.to_owned(),
            argv0: Some("hook".to_owned()),
            args: args.iter().map(OsString::from).collect(),
        };
        self.spawn(
            &self.interpreter_spec(hook_shell(self.env), invocation, args),
            hook_name,
        )
    }
}

/// Well-known lifecycle hooks that porcelain operations run and report in their outcomes
/// (for example [`crate::porcelain::commit::CommitOutcome::hooks_run`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
#[cfg_attr(feature = "serde-models", serde(rename_all = "kebab-case"))]
pub enum HookName {
    /// `.git/hooks/pre-commit` or `hook.pre-commit.*`.
    PreCommit,
    /// `.git/hooks/pre-merge-commit`, run before a clean merge records its commit.
    PreMergeCommit,
    /// `.git/hooks/prepare-commit-msg`, run on the message file before `commit-msg`; not skipped
    /// by `--no-verify`.
    PrepareCommitMsg,
    /// `.git/hooks/commit-msg` or `hook.commit-msg.*`.
    CommitMsg,
    /// `.git/hooks/post-commit` or `hook.post-commit.*`.
    PostCommit,
    /// `.git/hooks/post-checkout`, run after `HEAD` moves to another commit or branch.
    PostCheckout,
    /// `.git/hooks/post-merge`, run after a merge or fast-forward updates the work tree.
    PostMerge,
    /// `.git/hooks/post-rewrite`, run after amend or a history rewrite replaces commits.
    PostRewrite,
    /// `.git/hooks/pre-rebase`, run before a rebase rewrites anything; a non-zero exit refuses
    /// the rebase.
    PreRebase,
}

impl HookName {
    /// Git hook basename (`pre-commit`, `commit-msg`, `post-commit`, `post-checkout`, …).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PreCommit => "pre-commit",
            Self::PreMergeCommit => "pre-merge-commit",
            Self::PrepareCommitMsg => "prepare-commit-msg",
            Self::PreRebase => "pre-rebase",
            Self::CommitMsg => "commit-msg",
            Self::PostCommit => "post-commit",
            Self::PostCheckout => "post-checkout",
            Self::PostMerge => "post-merge",
            Self::PostRewrite => "post-rewrite",
        }
    }
}

/// Whether an operation runs the hooks Git runs once it has completed (`post-checkout`,
/// `post-merge`, `post-rewrite`).
///
/// These hooks are advisory: their exit status never changes the operation's result. Callers
/// that compose several operations (or an embedder that does not want user scripts to run)
/// pass [`PostOperationHooks::Skip`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum PostOperationHooks {
    /// Run the installed post-operation hooks (default, matching Git).
    #[default]
    Run,
    /// Do not run post-operation hooks.
    Skip,
}

impl PostOperationHooks {
    /// Whether hooks should run.
    #[must_use]
    pub const fn enabled(self) -> bool {
        matches!(self, Self::Run)
    }
}

/// What kind of rewrite a `post-rewrite` hook is told about (its first argument).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewriteKind {
    /// `commit --amend` replaced the tip commit.
    Amend,
    /// A rebase or history edit replayed commits.
    Rebase,
}

impl RewriteKind {
    /// Hook argument (`amend` or `rebase`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Amend => "amend",
            Self::Rebase => "rebase",
        }
    }
}

/// Run an advisory hook whose outcome never fails the surrounding operation.
///
/// Returns whether a hook ran (a non-zero exit still counts as having run). A hook that cannot
/// be started, or invalid `hook.*` configuration, is reported as [`Warning::HookNotRun`] on the
/// repository's diagnostics sink and returns `false`.
///
/// [`Warning::HookNotRun`]: crate::diagnostics::Warning::HookNotRun
pub(crate) fn run_advisory_hook(
    repo: &Repository,
    hook: HookName,
    args: &[&str],
    stdin_data: Option<&[u8]>,
    commit_env: Option<&CommitHookEnv<'_>>,
) -> bool {
    let result = match commit_env {
        Some(env) => run_commit_hook(repo, hook.as_str(), args, stdin_data, env),
        None => run_hook(repo, hook.as_str(), args, stdin_data),
    };
    match result {
        Ok(result) => result.was_executed(),
        Err(err) => {
            let mut detail = err.to_string();
            let mut cause = std::error::Error::source(&err);
            while let Some(c) = cause {
                detail.push_str(": ");
                detail.push_str(&c.to_string());
                cause = c.source();
            }
            repo.warn(crate::diagnostics::Warning::HookNotRun {
                hook_name: hook.as_str().to_owned(),
                detail,
            });
            false
        }
    }
}

/// Run `post-checkout <old> <new> <flag>` after `HEAD` moved.
///
/// `old` is the commit `HEAD` pointed at before (`None` for an unborn branch, passed as the null
/// object id). `branch_checkout` is `true` when `HEAD` itself moved to another branch or commit
/// (switch, rebase) and `false` when only paths were checked out.
///
/// Returns whether a hook ran (a non-zero exit still counts). The hook's outcome is never an
/// error: a hook that cannot be started is reported as
/// [`Warning::HookNotRun`](crate::diagnostics::Warning::HookNotRun) and returns `false`.
pub fn run_post_checkout(
    repo: &Repository,
    old: Option<ObjectId>,
    new: ObjectId,
    branch_checkout: bool,
) -> bool {
    let old_hex = old
        .unwrap_or_else(|| ObjectId::null(repo.odb().hash_algo()))
        .to_hex();
    let new_hex = new.to_hex();
    let flag = if branch_checkout { "1" } else { "0" };
    run_advisory_hook(
        repo,
        HookName::PostCheckout,
        &[old_hex.as_str(), new_hex.as_str(), flag],
        None,
        None,
    )
}

/// Run `post-merge <flag>` after a merge or fast-forward updated the work tree; the flag is
/// `1` for a squash merge and `0` otherwise.
///
/// Returns whether a hook ran; failures never become errors (see [`run_post_checkout`]).
pub fn run_post_merge(repo: &Repository, squash: bool) -> bool {
    run_advisory_hook(
        repo,
        HookName::PostMerge,
        &[if squash { "1" } else { "0" }],
        None,
        None,
    )
}

/// Run `pre-rebase <upstream>` before a rebase of the checked-out branch rewrites anything.
///
/// `upstream` is the upstream exactly as the user named it (Git passes the argument, not the
/// resolved commit). `None` means the branch's configured upstream, which the hook receives as
/// its full ref name (`refs/remotes/origin/main`), as Git does. The hook gets no second
/// argument because the branch being rebased is the current one.
///
/// Returns whether a hook ran.
///
/// # Errors
///
/// [`HookError::Failed`] when the hook exits non-zero, which refuses the rebase;
/// [`HookError::Spawn`] / [`HookError::Internal`] as for [`run_hook`].
pub fn run_pre_rebase(repo: &Repository, upstream: Option<&str>) -> Result<bool, HookError> {
    let configured;
    let upstream_arg = match upstream {
        Some(arg) => arg,
        None => {
            configured = crate::state::resolve_head_for_repository(repo)
                .ok()
                .and_then(|head| {
                    head.branch_name().and_then(|branch| {
                        crate::branch_tracking::upstream_tracking_full_ref(repo, branch)
                    })
                })
                .unwrap_or_else(|| "@{upstream}".to_owned());
            configured.as_str()
        }
    };
    let name = HookName::PreRebase.as_str();
    run_hook(repo, name, &[upstream_arg], None)?.check(name)
}

/// Bytes fed to `post-rewrite` on stdin: one `<old> <new>` line per rewritten commit.
#[must_use]
pub fn post_rewrite_stdin(pairs: &[(ObjectId, ObjectId)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pairs.len() * 82);
    for (old, new) in pairs {
        out.extend_from_slice(old.to_hex().as_bytes());
        out.push(b' ');
        out.extend_from_slice(new.to_hex().as_bytes());
        out.push(b'\n');
    }
    out
}

/// Run `post-rewrite <amend|rebase>` with `pairs` (`old new`, oldest first) on stdin.
///
/// Nothing runs when `pairs` is empty. Returns whether a hook ran; failures never become errors
/// (see [`run_post_checkout`]).
pub fn run_post_rewrite(
    repo: &Repository,
    kind: RewriteKind,
    pairs: &[(ObjectId, ObjectId)],
) -> bool {
    if pairs.is_empty() {
        return false;
    }
    let stdin = post_rewrite_stdin(pairs);
    run_advisory_hook(
        repo,
        HookName::PostRewrite,
        &[kind.as_str()],
        Some(&stdin),
        None,
    )
}

/// Result of running the hooks for one event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookResult {
    /// Every hook ran and exited 0.
    Success {
        /// What the hooks wrote; empty under [`HookStdio::Inherit`].
        output: HookOutput,
    },
    /// No hook is installed for the event (or it is not executable) — treated as success.
    NotFound,
    /// A hook exited non-zero; later hooks for the event did not run.
    Failed {
        /// Exit code of the failing hook (1 when it had none, e.g. killed by a signal).
        status: i32,
        /// What the hooks wrote up to and including the failing one; empty under
        /// [`HookStdio::Inherit`].
        output: HookOutput,
    },
}

impl HookResult {
    /// Returns true if the hook was successful or not found.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        matches!(self, HookResult::Success { .. } | HookResult::NotFound)
    }

    /// Returns true if the hook existed and ran (regardless of exit code).
    #[must_use]
    pub fn was_executed(&self) -> bool {
        !matches!(self, HookResult::NotFound)
    }

    /// Exit code of the hook run: `Some(0)` on success, `None` when no hook ran.
    #[must_use]
    pub fn status(&self) -> Option<i32> {
        match self {
            HookResult::Success { .. } => Some(0),
            HookResult::NotFound => None,
            HookResult::Failed { status, .. } => Some(*status),
        }
    }

    /// Captured output, or `None` when no hook ran.
    #[must_use]
    pub fn output(&self) -> Option<&HookOutput> {
        match self {
            HookResult::Success { output } | HookResult::Failed { output, .. } => Some(output),
            HookResult::NotFound => None,
        }
    }

    /// Turn a failed run into [`HookError::Failed`] for `hook_name`; returns whether a hook ran.
    ///
    /// # Errors
    ///
    /// [`HookError::Failed`] carrying the status and captured output when a hook exited non-zero.
    pub fn check(self, hook_name: &str) -> Result<bool, HookError> {
        match self {
            HookResult::Success { .. } => Ok(true),
            HookResult::NotFound => Ok(false),
            HookResult::Failed { status, output } => Err(HookError::Failed {
                hook_name: hook_name.to_owned(),
                status,
                output,
            }),
        }
    }
}

/// Options for [`run_hook_opts`].
#[derive(Debug, Clone, Default)]
pub struct RunHookOptions<'a> {
    /// Capture hook output or let hooks write to this process's stderr (see [`HookStdio`]).
    ///
    /// The `run_*` helpers that take a [`Repository`] use [`Repository::hook_stdio`].
    pub stdio: HookStdio,
    /// Under [`HookStdio::Inherit`], send hook stdout to this process's stderr (Git does this
    /// for every hook except `pre-push`). Captured stdout is always returned separately.
    pub stdout_to_stderr: bool,
    /// File path to open and pipe to each hook's stdin (reopened per hook).
    pub path_to_stdin: Option<&'a Path>,
    /// In-memory stdin (used when `path_to_stdin` is None).
    pub stdin_data: Option<&'a [u8]>,
    /// Extra environment variables for each hook subprocess.
    pub env_vars: &'a [(&'a str, &'a str)],
    /// Override the hook process working directory.
    pub cwd: Option<&'a Path>,
    /// Commit-style env (`GIT_INDEX_FILE`, `GIT_PREFIX`, author exports, …) merged after `env_vars`.
    pub commit_env: Option<&'a CommitHookEnv<'a>>,
}

/// Run all hooks for `hook_name` in Git order; return first non-zero exit or success.
///
/// When `repo` is `None`, only configured hooks run (out-of-repo); cwd is the process cwd and
/// `GIT_DIR` is not set for those hooks.
///
/// Hook output is handled per [`RunHookOptions::stdio`]: captured into the returned
/// [`HookOutput`], or written by the child straight to this process's stderr. The library
/// never re-emits what a hook (or an injected [`CommandRunner`]) produced.
///
/// # Errors
///
/// [`HookError::Spawn`] when a hook (or the shell that runs it) cannot be started, carrying the
/// output earlier hooks for the event already wrote, and [`HookError::Internal`] for invalid
/// `hook.*` configuration. A hook that runs and exits non-zero is `Ok(HookResult::Failed { .. })`.
pub fn run_hook_opts(
    repo: Option<&Repository>,
    hook_name: &str,
    args: &[&str],
    config: &ConfigSet,
    opts: RunHookOptions<'_>,
) -> Result<HookResult, HookError> {
    let seq = match repo {
        Some(r) => resolve_hook_sequence(r, hook_name, config)?,
        None => resolve_configured_hooks_only(hook_name, config)?,
    };
    if seq.is_empty() {
        return Ok(HookResult::NotFound);
    }

    let work_dir: PathBuf = opts.cwd.map_or_else(
        || match repo {
            Some(r) => hook_run_dir(r).to_path_buf(),
            None => PathBuf::from("."),
        },
        Path::to_path_buf,
    );
    let work_dir = work_dir.as_path();
    let git_dir_for_configured = repo.map(|r| r.git_dir());
    let runner: Arc<dyn CommandRunner> = match repo {
        Some(r) => r.command_runner(),
        None => system_command_runner(),
    };

    let mut merged_env: Vec<(String, String)> = opts
        .env_vars
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    if let Some(r) = repo {
        if let Some(ce) = opts.commit_env {
            merged_env.extend(build_commit_hook_env(r, work_dir, ce));
        }
    }

    let mut hook_env = match repo {
        Some(r) => CommandEnvironment::from_repository_environment(r.environment(), &merged_env),
        None => hook_command_env(git_dir_for_configured, &merged_env),
    };
    if cfg!(windows) {
        strip_verbatim_prefix_from_path_vars(&mut hook_env.set);
    }

    let capture = opts.stdio == HookStdio::Capture;
    let (stdout, stderr) = match opts.stdio {
        HookStdio::Capture => (CommandStdio::Pipe, CommandStdio::Pipe),
        HookStdio::Inherit if opts.stdout_to_stderr => {
            (CommandStdio::ParentStderr, CommandStdio::Inherit)
        }
        HookStdio::Inherit => (CommandStdio::Inherit, CommandStdio::Inherit),
    };
    let mut output = HookOutput::default();

    for h in &seq {
        let (stdin_piped, stdin_file) = match opts.path_to_stdin {
            Some(p) => (true, Some(p.to_path_buf())),
            None => (opts.stdin_data.is_some(), None),
        };

        let ctx = HookSpawnContext {
            runner: runner.as_ref(),
            cwd: work_dir,
            env: &hook_env,
            stdin_piped,
            stdout,
            stderr,
        };
        let spawned = match h {
            ResolvedHook::Traditional { path, argv0 } => {
                let Some(r) = repo else {
                    continue;
                };
                let effective_argv0 = path
                    .parent()
                    .map(|hooks_dir| hook_argv0(r, hooks_dir, hook_name, work_dir))
                    .unwrap_or_else(|| argv0.clone());
                ctx.spawn_traditional(hook_name, path, &effective_argv0, args)
            }
            ResolvedHook::Configured { command } => ctx.spawn_configured(hook_name, command, args),
        };
        let mut child = match spawned {
            Ok(child) => child,
            Err(HookError::Spawn {
                hook_name,
                path,
                source,
                ..
            }) => {
                return Err(HookError::Spawn {
                    hook_name,
                    path,
                    source,
                    output,
                })
            }
            Err(other) => return Err(other),
        };

        if let Some(ref path) = stdin_file {
            let file = match fs::File::open(path) {
                Ok(f) => f,
                Err(_e) => {
                    return Ok(HookResult::Failed { status: 1, output });
                }
            };
            if let Some(stdin) = child.stdin_mut() {
                let mut file = file;
                let _ = std::io::copy(&mut file, stdin);
            }
        } else if let Some(data) = opts.stdin_data {
            if let Some(stdin) = child.stdin_mut() {
                let _ = stdin.write_all(data);
            }
        }
        if stdin_piped {
            child.close_stdin();
        }

        let waited = if capture {
            child.wait_with_output().map(|o| {
                output.stdout.extend_from_slice(&o.stdout);
                output.stderr.extend_from_slice(&o.stderr);
                o.status
            })
        } else {
            child.wait()
        };
        let exit = match waited {
            Ok(exit) => exit,
            Err(_) => return Ok(HookResult::Failed { status: 1, output }),
        };

        if !exit.success {
            if let Some(r) = repo {
                let _ = r.reload_config();
            }
            return Ok(HookResult::Failed {
                status: exit.code.unwrap_or(1),
                output,
            });
        }
    }

    if let Some(r) = repo {
        let _ = r.reload_config();
    }

    Ok(HookResult::Success { output })
}

/// Like [`run_commit_hook`] but a hook that exits non-zero is an error.
///
/// # Errors
///
/// [`HookError::Failed`] with the hook's exit status and captured output, or any error from
/// [`run_commit_hook`].
pub fn run_commit_hook_checked(
    repo: &Repository,
    hook_name: &str,
    args: &[&str],
    stdin_data: Option<&[u8]>,
    commit_env: &CommitHookEnv<'_>,
) -> Result<(), HookError> {
    run_commit_hook(repo, hook_name, args, stdin_data, commit_env)?
        .check(hook_name)
        .map(drop)
}

/// Run commit-style hooks with `GIT_INDEX_FILE`, `GIT_PREFIX`, and related env (Git `run_commit_hook`).
///
/// Output follows [`Repository::hook_stdio`].
///
/// # Errors
///
/// [`HookError::Spawn`] when a hook cannot be started, [`HookError::Internal`] when the
/// repository config cannot be loaded or `hook.*` entries are invalid.
pub fn run_commit_hook(
    repo: &Repository,
    hook_name: &str,
    args: &[&str],
    stdin_data: Option<&[u8]>,
    commit_env: &CommitHookEnv<'_>,
) -> Result<HookResult, HookError> {
    let config = repo
        .config()
        .map_err(|e| HookError::Internal(e.to_string()))?;
    run_hook_opts(
        Some(repo),
        hook_name,
        args,
        config.as_ref(),
        RunHookOptions {
            stdin_data,
            commit_env: Some(commit_env),
            ..repo_hook_options(repo, hook_name)
        },
    )
}

/// Default [`RunHookOptions`] for `hook_name` on `repo`: [`Repository::hook_stdio`], and stdout
/// sent to stderr for every hook except `pre-push`, whose stdout is part of its protocol.
fn repo_hook_options<'a>(repo: &Repository, hook_name: &str) -> RunHookOptions<'a> {
    RunHookOptions {
        stdio: repo.hook_stdio(),
        stdout_to_stderr: hook_name != "pre-push",
        ..RunHookOptions::default()
    }
}

/// Run a hook by name with the given arguments (Git-compatible multihooks).
///
/// Output follows [`Repository::hook_stdio`]; when inherited, `pre-push` keeps stdout
/// separate and other hooks send stdout to stderr.
///
/// # Errors
///
/// [`HookError::Spawn`] when a hook cannot be started, [`HookError::Internal`] when the
/// repository config cannot be loaded or `hook.*` entries are invalid.
pub fn run_hook(
    repo: &Repository,
    hook_name: &str,
    args: &[&str],
    stdin_data: Option<&[u8]>,
) -> Result<HookResult, HookError> {
    let config = repo
        .config()
        .map_err(|e| HookError::Internal(e.to_string()))?;
    run_hook_opts(
        Some(repo),
        hook_name,
        args,
        config.as_ref(),
        RunHookOptions {
            stdin_data,
            ..repo_hook_options(repo, hook_name)
        },
    )
}

/// Run a server-side hook (`pre-receive`, `update`, …) with cwd = `GIT_DIR`.
///
/// The hook sees `GIT_DIR=.`, as Git's receive-pack hooks do, so scripts that run
/// `git` commands address the repository they were started in.
///
/// Output is always captured, whatever [`Repository::hook_stdio`] says, because the server
/// relays it to the client over the sideband. A hook that cannot be resolved or started is
/// reported as `Failed { status: 1 }` so the push is declined.
pub fn run_hook_in_git_dir(
    repo: &Repository,
    hook_name: &str,
    args: &[&str],
    stdin_data: Option<&[u8]>,
    env_vars: &[(&str, &str)],
) -> HookResult {
    let declined = || HookResult::Failed {
        status: 1,
        output: HookOutput::default(),
    };
    let env_vars: Vec<(&str, &str)> = std::iter::once(("GIT_DIR", "."))
        .chain(env_vars.iter().copied())
        .collect();
    let Some(config) = repo.config().ok() else {
        return declined();
    };
    run_hook_opts(
        Some(repo),
        hook_name,
        args,
        config.as_ref(),
        RunHookOptions {
            stdio: HookStdio::Capture,
            stdout_to_stderr: true,
            stdin_data,
            env_vars: &env_vars,
            cwd: Some(repo.git_dir()),
            ..RunHookOptions::default()
        },
    )
    .unwrap_or_else(|_| declined())
}

/// One ref update a push is about to make, as reported to the `pre-push` hook.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrePushRef {
    /// Local source as named by the push (`refs/heads/main`, `HEAD`, …). `None` when the
    /// source was a bare object id, or for a deletion.
    pub local_ref: Option<String>,
    /// Object the remote ref will point at; `None` for a deletion.
    pub local_oid: Option<ObjectId>,
    /// Destination ref on the remote (`refs/heads/main`).
    pub remote_ref: String,
    /// Current value of [`Self::remote_ref`] on the remote; `None` when it does not exist.
    pub remote_oid: Option<ObjectId>,
}

impl PrePushRef {
    /// The hook's stdin line: `<local ref> SP <local oid> SP <remote ref> SP <remote oid> LF`.
    ///
    /// A missing object id is written as the all-zero id of width `algo`; a deletion's local
    /// ref is `(delete)` and a bare object id source repeats the id.
    fn stdin_line(&self, algo: crate::objects::HashAlgo) -> String {
        let zero = "0".repeat(algo.hex_len());
        let hex = |oid: Option<ObjectId>| oid.map_or_else(|| zero.clone(), |o| o.to_hex());
        let local_ref = match (&self.local_ref, self.local_oid) {
            (Some(name), _) => name.clone(),
            (None, Some(oid)) => oid.to_hex(),
            (None, None) => "(delete)".to_owned(),
        };
        format!(
            "{local_ref} {} {} {}\n",
            hex(self.local_oid),
            self.remote_ref,
            hex(self.remote_oid)
        )
    }
}

/// Run the client-side `pre-push` hook before a push sends anything.
///
/// The hook receives `remote` (the remote's name, or its URL for an unnamed remote) and `url`
/// as arguments, and one [`PrePushRef`] line per update on stdin. It runs even when `refs` is
/// empty. Output follows [`Repository::hook_stdio`]; under [`HookStdio::Inherit`] the hook's
/// stdout goes to stderr with its stderr, so a caller's own stdout (for example JSON output)
/// stays clean.
///
/// Returns the hook's captured output (empty under [`HookStdio::Inherit`] or when no hook is
/// installed).
///
/// # Errors
///
/// [`HookError::Failed`] when the hook exits non-zero (the push must not proceed),
/// [`HookError::Spawn`] when it cannot be started, and [`HookError::Internal`] when the
/// configuration cannot be loaded or `hook.*` entries are invalid.
pub fn run_pre_push_hook(
    repo: &Repository,
    remote: &str,
    url: &str,
    refs: &[PrePushRef],
) -> Result<HookOutput, HookError> {
    let config = repo
        .config()
        .map_err(|e| HookError::Internal(e.to_string()))?;
    let algo = repo.odb().hash_algo();
    let stdin: String = refs.iter().map(|r| r.stdin_line(algo)).collect();
    let result = run_hook_opts(
        Some(repo),
        "pre-push",
        &[remote, url],
        config.as_ref(),
        RunHookOptions {
            stdout_to_stderr: true,
            stdin_data: Some(stdin.as_bytes()),
            ..repo_hook_options(repo, "pre-push")
        },
    )?;
    let output = match &result {
        HookResult::Success { output } => output.clone(),
        HookResult::NotFound | HookResult::Failed { .. } => HookOutput::default(),
    };
    result.check("pre-push")?;
    Ok(output)
}

/// Phase argument for the [`reference-transaction`](https://git-scm.com/docs/githooks#_reference_transaction) hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReferenceTransactionPhase {
    /// Before any ref write in the transaction; non-zero exit aborts the batch.
    Prepared,
    /// After refs were written successfully.
    Committed,
    /// After the transaction was aborted without writing (or after `prepared` refused).
    Aborted,
}

impl ReferenceTransactionPhase {
    fn as_arg(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Committed => "committed",
            Self::Aborted => "aborted",
        }
    }
}

/// Cached hook execution context for ref-store transactions (no full [`Repository`] handle).
#[derive(Clone)]
pub struct RefHookContext {
    git_dir: PathBuf,
    work_tree: Option<PathBuf>,
    environment: Arc<Environment>,
    command_runner: Arc<dyn CommandRunner>,
    hook_stdio: HookStdio,
    #[cfg_attr(not(unix), allow(dead_code))]
    diagnostics: crate::diagnostics::DiagnosticsHandle,
    hash_algo: HashAlgo,
}

impl RefHookContext {
    /// Build hook context for a repository being opened.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        git_dir: PathBuf,
        work_tree: Option<PathBuf>,
        environment: Arc<Environment>,
        command_runner: Arc<dyn CommandRunner>,
        hook_stdio: HookStdio,
        diagnostics: crate::diagnostics::DiagnosticsHandle,
        hash_algo: HashAlgo,
    ) -> Self {
        Self {
            git_dir,
            work_tree,
            environment,
            command_runner,
            hook_stdio,
            diagnostics,
            hash_algo,
        }
    }

    /// Git directory this context was opened for (main or linked worktree admin dir).
    #[must_use]
    pub(crate) fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// Null object id for this repository's hash algorithm.
    #[must_use]
    pub(crate) fn null_oid(&self) -> ObjectId {
        ObjectId::null(self.hash_algo)
    }

    fn hook_run_dir(&self) -> &Path {
        self.work_tree.as_deref().unwrap_or(&self.git_dir)
    }

    fn load_config(&self) -> Result<Arc<crate::config::ConfigSet>, HookError> {
        crate::config::ConfigSet::load(&self.environment, Some(&self.git_dir), true)
            .map(Arc::new)
            .map_err(|e| HookError::Internal(e.to_string()))
    }

    /// Run the `reference-transaction` hook for one phase and stdin payload.
    ///
    /// A failure on [`ReferenceTransactionPhase::Prepared`] is returned as [`HookError::Failed`]
    /// or [`HookError::Spawn`]; `committed` and `aborted` ignore hook failures.
    pub(crate) fn run_reference_transaction(
        &self,
        phase: ReferenceTransactionPhase,
        stdin: &[u8],
    ) -> Result<(), HookError> {
        if stdin.is_empty() {
            return Ok(());
        }
        let config = self.load_config()?;
        let result = run_hook_opts_for_ref_context(
            self,
            "reference-transaction",
            &[phase.as_arg()],
            config.as_ref(),
            RunHookOptions {
                stdin_data: Some(stdin),
                stdout_to_stderr: true,
                stdio: self.hook_stdio,
                ..RunHookOptions::default()
            },
        )?;
        if phase == ReferenceTransactionPhase::Prepared {
            result.check("reference-transaction")?;
        }
        Ok(())
    }
}

fn resolve_hooks_dir_for_ref(ctx: &RefHookContext, config: &ConfigSet) -> PathBuf {
    let dir = match config.get("core.hooksPath") {
        Some(hooks_path) => {
            let p = PathBuf::from(parse_path(ctx.environment.as_ref(), &hooks_path));
            if p.is_absolute() {
                p
            } else {
                ctx.hook_run_dir().join(p)
            }
        }
        None => crate::repo::common_git_dir_for_config(&ctx.git_dir).join("hooks"),
    };
    path_without_verbatim_prefix(&dir).into_owned()
}

fn hook_argv0_for_ref(
    ctx: &RefHookContext,
    hooks_dir: &Path,
    hook_name: &str,
    cwd: &Path,
) -> PathBuf {
    let default_hooks_dir = ctx.git_dir.join("hooks");
    if same_dir(hooks_dir, &default_hooks_dir) {
        if cwd == ctx.git_dir.as_path() {
            return PathBuf::from("hooks").join(hook_name);
        }
        if let Some(work_tree) = &ctx.work_tree {
            if cwd == work_tree.as_path() && same_dir(&ctx.git_dir, &work_tree.join(".git")) {
                return PathBuf::from(".git").join("hooks").join(hook_name);
            }
        }
    }
    hooks_dir.join(hook_name)
}

fn resolve_hook_sequence_for_ref(
    ctx: &RefHookContext,
    hook_name: &str,
    config: &ConfigSet,
) -> Result<Vec<ResolvedHook>, String> {
    let tables = HookConfigTables::from_config(config);
    let mut seq = Vec::new();
    for (_friendly, command) in tables.hooks_for_event(hook_name)? {
        seq.push(ResolvedHook::Configured { command });
    }
    let hooks_dir = resolve_hooks_dir_for_ref(ctx, config);
    if let Some(path) = traditional_hook_candidate_for_ref(ctx, &hooks_dir, hook_name) {
        let argv0 = hook_argv0_for_ref(ctx, &hooks_dir, hook_name, ctx.hook_run_dir());
        seq.push(ResolvedHook::Traditional { path, argv0 });
    }
    Ok(seq)
}

#[cfg_attr(not(unix), allow(unused_variables))]
fn traditional_hook_candidate_for_ref(
    ctx: &RefHookContext,
    hooks_dir: &Path,
    hook_name: &str,
) -> Option<PathBuf> {
    let path = hooks_dir.join(hook_name);
    if !path.exists() {
        return None;
    }
    let meta = fs::metadata(&path).ok()?;
    #[cfg(unix)]
    if meta.permissions().mode() & 0o111 == 0 {
        let show_warning = ctx
            .load_config()
            .ok()
            .and_then(|c| c.get("advice.ignoredHook"))
            .map(|v| !matches!(v.to_lowercase().as_str(), "false" | "no" | "off" | "0"))
            .unwrap_or(true);
        if show_warning {
            ctx.diagnostics
                .warn(crate::diagnostics::Warning::NonExecutableHookIgnored {
                    hook_name: hook_name.to_string(),
                    hook_path: Some(path.clone()),
                });
        }
        return None;
    }
    Some(path)
}

fn run_hook_opts_for_ref_context(
    ctx: &RefHookContext,
    hook_name: &str,
    args: &[&str],
    config: &ConfigSet,
    opts: RunHookOptions<'_>,
) -> Result<HookResult, HookError> {
    let seq = resolve_hook_sequence_for_ref(ctx, hook_name, config)?;
    if seq.is_empty() {
        return Ok(HookResult::NotFound);
    }

    let work_dir = opts
        .cwd
        .map(Path::to_path_buf)
        .unwrap_or_else(|| ctx.hook_run_dir().to_path_buf());
    let work_dir = work_dir.as_path();
    let runner = ctx.command_runner.as_ref();

    let merged_env: Vec<(String, String)> = opts
        .env_vars
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();

    let mut hook_env =
        CommandEnvironment::from_repository_environment(ctx.environment.as_ref(), &merged_env);
    if cfg!(windows) {
        strip_verbatim_prefix_from_path_vars(&mut hook_env.set);
    }
    hook_env.set.push((
        OsString::from("GIT_DIR"),
        ctx.git_dir.as_os_str().to_os_string(),
    ));

    let capture = opts.stdio == HookStdio::Capture;
    let (stdout, stderr) = match opts.stdio {
        HookStdio::Capture => (CommandStdio::Pipe, CommandStdio::Pipe),
        HookStdio::Inherit if opts.stdout_to_stderr => {
            (CommandStdio::ParentStderr, CommandStdio::Inherit)
        }
        HookStdio::Inherit => (CommandStdio::Inherit, CommandStdio::Inherit),
    };
    let mut output = HookOutput::default();

    for h in &seq {
        let (stdin_piped, stdin_file) = match opts.path_to_stdin {
            Some(p) => (true, Some(p.to_path_buf())),
            None => (opts.stdin_data.is_some(), None),
        };

        let hook_ctx = HookSpawnContext {
            runner,
            cwd: work_dir,
            env: &hook_env,
            stdin_piped,
            stdout,
            stderr,
        };
        let spawned = match h {
            ResolvedHook::Traditional { path, argv0 } => {
                let effective_argv0 = path
                    .parent()
                    .map(|hooks_dir| hook_argv0_for_ref(ctx, hooks_dir, hook_name, work_dir))
                    .unwrap_or_else(|| argv0.clone());
                hook_ctx.spawn_traditional(hook_name, path, &effective_argv0, args)
            }
            ResolvedHook::Configured { command } => {
                hook_ctx.spawn_configured(hook_name, command, args)
            }
        };
        let mut child = match spawned {
            Ok(child) => child,
            Err(HookError::Spawn {
                hook_name,
                path,
                source,
                ..
            }) => {
                return Err(HookError::Spawn {
                    hook_name,
                    path,
                    source,
                    output,
                })
            }
            Err(other) => return Err(other),
        };

        if let Some(ref path) = stdin_file {
            let file = match fs::File::open(path) {
                Ok(f) => f,
                Err(_e) => {
                    return Ok(HookResult::Failed { status: 1, output });
                }
            };
            if let Some(stdin) = child.stdin_mut() {
                let mut file = file;
                let _ = std::io::copy(&mut file, stdin);
            }
        } else if let Some(data) = opts.stdin_data {
            if let Some(stdin) = child.stdin_mut() {
                let _ = stdin.write_all(data);
            }
        }
        if stdin_piped {
            child.close_stdin();
        }

        let waited = if capture {
            child.wait_with_output().map(|o| {
                output.stdout.extend_from_slice(&o.stdout);
                output.stderr.extend_from_slice(&o.stderr);
                o.status
            })
        } else {
            child.wait()
        };
        let exit = match waited {
            Ok(exit) => exit,
            Err(_) => return Ok(HookResult::Failed { status: 1, output }),
        };

        if !exit.success {
            return Ok(HookResult::Failed {
                status: exit.code.unwrap_or(1),
                output,
            });
        }
    }

    Ok(HookResult::Success { output })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, b"").expect("write");
    }

    fn exe(name: &str) -> String {
        format!("{name}{}", std::env::consts::EXE_SUFFIX)
    }

    #[test]
    fn hook_result_reports_status_output_and_checks() {
        let output = HookOutput {
            stdout: b"out".to_vec(),
            stderr: b"err".to_vec(),
        };
        assert!(!output.is_empty());
        assert!(HookOutput::default().is_empty());
        assert_eq!(HookError::Internal("bad".into()).output(), None);
        assert_eq!(
            format!("{output:?}"),
            r#"HookOutput { stdout: "out", stderr: "err" }"#
        );

        let success = HookResult::Success {
            output: output.clone(),
        };
        assert!(success.is_ok() && success.was_executed());
        assert_eq!(success.status(), Some(0));
        assert_eq!(success.output(), Some(&output));
        assert!(success.check("pre-commit").expect("success"));

        let missing = HookResult::NotFound;
        assert!(missing.is_ok() && !missing.was_executed());
        assert_eq!((missing.status(), missing.output()), (None, None));
        assert!(!missing.check("pre-commit").expect("not found"));

        let failed = HookResult::Failed {
            status: 3,
            output: output.clone(),
        };
        assert!(!failed.is_ok() && failed.was_executed());
        assert_eq!(failed.status(), Some(3));
        match failed.check("commit-msg") {
            Err(err @ HookError::Failed { .. }) => {
                assert_eq!(err.to_string(), "hook commit-msg failed with exit status 3");
                let HookError::Failed {
                    hook_name,
                    status,
                    output: got,
                } = err
                else {
                    unreachable!()
                };
                assert_eq!((hook_name.as_str(), status), ("commit-msg", 3));
                assert_eq!(got, output);
            }
            other => panic!("expected HookError::Failed, got {other:?}"),
        }
    }

    #[test]
    fn post_rewrite_stdin_is_one_old_new_line_per_pair() {
        let a = ObjectId::from_hex("1111111111111111111111111111111111111111").expect("a");
        let b = ObjectId::from_hex("2222222222222222222222222222222222222222").expect("b");
        assert_eq!(post_rewrite_stdin(&[]), b"");
        assert_eq!(
            post_rewrite_stdin(&[(a, b), (b, a)]),
            format!(
                "{} {}\n{} {}\n",
                a.to_hex(),
                b.to_hex(),
                b.to_hex(),
                a.to_hex()
            )
            .into_bytes()
        );
    }

    #[test]
    fn post_operation_names_and_flags() {
        assert_eq!(HookName::PostCheckout.as_str(), "post-checkout");
        assert_eq!(HookName::PostMerge.as_str(), "post-merge");
        assert_eq!(HookName::PostRewrite.as_str(), "post-rewrite");
        assert_eq!(HookName::PreMergeCommit.as_str(), "pre-merge-commit");
        assert_eq!(HookName::PrepareCommitMsg.as_str(), "prepare-commit-msg");
        assert_eq!(HookName::PreRebase.as_str(), "pre-rebase");
        assert_eq!(RewriteKind::Amend.as_str(), "amend");
        assert_eq!(RewriteKind::Rebase.as_str(), "rebase");
        assert!(PostOperationHooks::default().enabled());
        assert!(!PostOperationHooks::Skip.enabled());
    }

    #[test]
    fn shebang_interpreter_takes_the_interpreter_basename() {
        assert_eq!(
            shebang_interpreter(b"#!/bin/sh\nexit 0\n").as_deref(),
            Some("sh")
        );
        assert_eq!(
            shebang_interpreter(b"#!/bin/bash -e\n").as_deref(),
            Some("bash")
        );
        assert_eq!(shebang_interpreter(b"#! /bin/sh\n").as_deref(), Some("sh"));
        assert_eq!(
            shebang_interpreter(b"#!/bin/sh\r\nexit 0\r\n").as_deref(),
            Some("sh")
        );
        assert_eq!(shebang_interpreter(b"#!/bin/sh").as_deref(), Some("sh"));
        assert_eq!(
            shebang_interpreter(br"#!C:\tools\perl.exe").as_deref(),
            Some("perl.exe")
        );
    }

    #[test]
    fn shebang_interpreter_looks_through_env() {
        assert_eq!(
            shebang_interpreter(b"#!/usr/bin/env bash\n").as_deref(),
            Some("bash")
        );
        assert_eq!(
            shebang_interpreter(b"#!/usr/bin/env -S python3 -u\n").as_deref(),
            Some("python3")
        );
        assert_eq!(shebang_interpreter(b"#!/usr/bin/env\n"), None);
    }

    #[test]
    fn shebang_interpreter_rejects_non_scripts() {
        assert_eq!(shebang_interpreter(b"exit 0\n"), None);
        assert_eq!(shebang_interpreter(b""), None);
        assert_eq!(shebang_interpreter(b"#!\n"), None);
        assert_eq!(shebang_interpreter(b"#!/\n"), None);
        assert_eq!(shebang_interpreter(b"\x7fELF\x02\x01"), None);
    }

    #[cfg(unix)]
    #[test]
    fn hook_shell_is_bin_sh_on_unix() {
        let mut env = CommandEnvironment::default();
        env.set.push(("PATH".into(), "/nonexistent".into()));
        assert_eq!(hook_shell(&env), ScriptInterpreter::bare("/bin/sh"));
    }

    #[test]
    fn interpreter_from_git_tree_gets_its_tool_dirs_on_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("Git");
        touch(&root.join("cmd").join(exe("git")));
        touch(&root.join("mingw64").join("bin").join(exe("git")));
        touch(&root.join("usr").join("bin").join(exe("sh")));
        let mut env = CommandEnvironment::default();
        env.set
            .push(("PATH".into(), root.join("cmd").into_os_string()));
        let runner = crate::command_runner::RecordingRunner::always_success();
        let ctx = HookSpawnContext {
            runner: runner.as_ref(),
            cwd: tmp.path(),
            env: &env,
            stdin_piped: false,
            stdout: CommandStdio::Inherit,
            stderr: CommandStdio::Inherit,
        };
        let dash_c = || ShellInvocation::DashC {
            script: "true".into(),
            argv0: Some("hook".into()),
            args: Vec::new(),
        };
        let interpreter = crate::command_runner::resolve_script_interpreter(
            "sh",
            Some(root.join("cmd").as_os_str()),
        );
        let spec = ctx.interpreter_spec(interpreter, dash_c(), &[]);
        assert_eq!(spec.program, root.join("usr").join("bin").join(exe("sh")));
        let expected = std::env::join_paths([
            root.join("mingw64").join("bin"),
            root.join("usr").join("bin"),
            root.join("cmd"),
        ])
        .expect("join");
        assert_eq!(
            crate::command_runner::child_path_var(&spec.env),
            Some(expected)
        );

        let bare = ctx.interpreter_spec(ScriptInterpreter::bare("/bin/sh"), dash_c(), &[]);
        assert_eq!(
            crate::command_runner::child_path_var(&bare.env),
            Some(root.join("cmd").into_os_string()),
            "interpreters outside a Git tree keep PATH as is"
        );
    }

    #[test]
    fn verbatim_prefixes_are_stripped_from_path_vars() {
        let mut set: Vec<(OsString, OsString)> = vec![
            ("GIT_INDEX_FILE".into(), r"\\?\C:\repo\.git\index".into()),
            ("GIT_DIR".into(), r"\\?\UNC\server\share\repo\.git".into()),
            ("GIT_WORK_TREE".into(), r"C:\repo".into()),
            (
                "GIT_OBJECT_DIRECTORY".into(),
                r"\\?\C:\repo\.git\objects".into(),
            ),
            ("GIT_PREFIX".into(), r"\\?\kept".into()),
        ];
        strip_verbatim_prefix_from_path_vars(&mut set);
        let value = |key: &str| {
            set.iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
                .expect("present")
        };
        assert_eq!(
            value("GIT_INDEX_FILE"),
            OsString::from(r"C:\repo\.git\index")
        );
        assert_eq!(
            value("GIT_DIR"),
            OsString::from(r"\\server\share\repo\.git")
        );
        assert_eq!(value("GIT_WORK_TREE"), OsString::from(r"C:\repo"));
        assert_eq!(
            value("GIT_OBJECT_DIRECTORY"),
            OsString::from(r"C:\repo\.git\objects")
        );
        assert_eq!(value("GIT_PREFIX"), OsString::from(r"\\?\kept"));
    }

    #[test]
    fn not_an_executable_matches_only_the_platform_code() {
        #[cfg(unix)]
        assert!(is_not_an_executable(&std::io::Error::from_raw_os_error(8)));
        #[cfg(windows)]
        assert!(is_not_an_executable(&std::io::Error::from_raw_os_error(
            193
        )));
        assert!(!is_not_an_executable(&std::io::Error::from(
            std::io::ErrorKind::NotFound
        )));
    }
}
