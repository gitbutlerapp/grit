//! Hook execution utilities.
//!
//! Implements Git's multihook model: hooks from `hook.<name>.*` config plus the
//! traditional script in the hooks directory (`core.hooksPath` or `.git/hooks/`).

use crate::command_runner::{
    system_command_runner, CommandEnvironment, CommandRunner, CommandSpec, CommandStdin,
    CommandStdio, RunningCommand, ShellInvocation,
};
use crate::config::{parse_path, ConfigSet};
use crate::git_path::path_without_verbatim_prefix;
use crate::objects::ObjectId;
use crate::repo::Repository;
use crate::state::HeadState;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::io::Write;
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

/// Hook subprocess failure (non-zero exit or spawn error).
#[derive(Debug, Error)]
pub enum HookError {
    /// A hook ran and returned a non-zero exit status.
    #[error("hook {hook_name} failed with exit status {status}")]
    Failed {
        /// Basename of the hook script that failed.
        hook_name: String,
        /// Process exit code from the hook subprocess.
        status: i32,
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
    },
    /// Configuration or internal error while resolving hooks.
    #[error("{0}")]
    Internal(String),
}

impl From<String> for HookError {
    fn from(value: String) -> Self {
        Self::Internal(value)
    }
}

fn stdio_mode(piped: bool) -> CommandStdio {
    if piped {
        CommandStdio::Pipe
    } else {
        CommandStdio::Inherit
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
    let wt_canon = wt.canonicalize().unwrap_or_else(|_| wt.to_path_buf());
    let wd_canon = invocation_cwd
        .canonicalize()
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
/// Never carries a Windows `\\?\` prefix, so the path can be shown to users and handed to
/// hook interpreters such as Git for Windows' `sh`.
pub fn resolve_hooks_dir(repo: &Repository) -> PathBuf {
    resolve_hooks_dir_for_config(
        repo.environment(),
        Some(repo.git_dir()),
        repo.config().ok().as_deref(),
    )
}

fn resolve_hooks_dir_for_config(
    env: &crate::environment::Environment,
    git_dir: Option<&Path>,
    config: Option<&ConfigSet>,
) -> PathBuf {
    let dir = match config.and_then(|cfg| cfg.get("core.hooksPath")) {
        Some(hooks_path) => {
            let p = PathBuf::from(parse_path(env, &hooks_path));
            if p.is_absolute() {
                p
            } else {
                env.cwd.join(p)
            }
        }
        None => git_dir
            .map(|gd| crate::repo::common_git_dir_for_config(gd).join("hooks"))
            .unwrap_or_else(|| PathBuf::from("hooks")),
    };
    path_without_verbatim_prefix(&dir).into_owned()
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
    match (a.canonicalize(), b.canonicalize()) {
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
    let hooks_dir =
        resolve_hooks_dir_for_config(repo.environment(), Some(repo.git_dir()), Some(config));
    if let Some(path) = traditional_hook_candidate(repo, &hooks_dir, hook_name) {
        let work_dir = repo.work_tree().unwrap_or(repo.git_dir());
        let argv0 = hook_argv0(repo, &hooks_dir, hook_name, work_dir);
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

/// First `dir/name` (or `dir/name.exe` on Windows) that is a file, for `dir` in `path_var`.
fn find_program_in_path(name: &str, path_var: &std::ffi::OsStr) -> Option<PathBuf> {
    let suffix = std::env::consts::EXE_SUFFIX;
    std::env::split_paths(path_var)
        .filter(|dir| !dir.as_os_str().is_empty())
        .find_map(|dir| {
            let with_suffix = (!suffix.is_empty() && !name.ends_with(suffix))
                .then(|| dir.join(format!("{name}{suffix}")));
            with_suffix
                .into_iter()
                .chain(std::iter::once(dir.join(name)))
                .find(|candidate| candidate.is_file())
        })
}

/// Resolve the interpreter `name` (`sh`, `bash`, …) for running a hook script.
///
/// Looks on `path_var` first, then in the Git for Windows tree that owns a `git` found on
/// `path_var` (`<root>/usr/bin`, `<root>/bin`), where `sh` lives even when only `git` is on
/// `PATH`. Falls back to the bare name so the OS search applies.
fn resolve_script_interpreter(name: &str, path_var: Option<&std::ffi::OsStr>) -> PathBuf {
    let Some(path_var) = path_var else {
        return PathBuf::from(name);
    };
    if let Some(found) = find_program_in_path(name, path_var) {
        return found;
    }
    find_program_in_path("git", path_var)
        .and_then(|git| {
            let bin_dir = git.parent()?.to_path_buf();
            bin_dir
                .ancestors()
                .skip(1)
                .take(2)
                .flat_map(|root| [root.join("usr").join("bin"), root.join("bin")])
                .find_map(|dir| find_program_in_path(name, dir.as_os_str()))
        })
        .unwrap_or_else(|| PathBuf::from(name))
}

/// `PATH` as the hook subprocess will see it, when it is set explicitly in `env`.
fn command_env_path(env: &CommandEnvironment) -> Option<&std::ffi::OsStr> {
    env.set
        .iter()
        .rev()
        .find(|(k, _)| {
            if cfg!(windows) {
                k.eq_ignore_ascii_case("PATH")
            } else {
                k == "PATH"
            }
        })
        .map(|(_, v)| v.as_os_str())
}

/// Shell for `hook.<name>.command` entries and for scripts the OS will not execute directly.
///
/// `/bin/sh` on Unix. On Windows there is no `/bin/sh`, so `sh` is resolved against the hook's
/// `PATH` (see [`resolve_script_interpreter`]).
fn hook_shell(env: &CommandEnvironment) -> PathBuf {
    if cfg!(windows) {
        resolve_script_interpreter("sh", command_env_path(env))
    } else {
        PathBuf::from("/bin/sh")
    }
}

/// Interpreter from the `#!` line of `script` (Windows cannot execute scripts directly).
#[cfg(windows)]
fn script_shebang_interpreter(script: &Path, env: &CommandEnvironment) -> Option<PathBuf> {
    use std::io::Read;
    let mut head = [0u8; 256];
    let mut file = fs::File::open(script).ok()?;
    let n = file.read(&mut head).ok()?;
    let name = shebang_interpreter(&head[..n])?;
    Some(resolve_script_interpreter(&name, command_env_path(env)))
}

/// Stdio and environment shared by every hook subprocess.
struct HookSpawnContext<'a> {
    runner: &'a dyn CommandRunner,
    cwd: &'a Path,
    env: &'a CommandEnvironment,
    stdin_piped: bool,
    stdout_piped: bool,
    stderr_piped: bool,
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
            stdout: stdio_mode(self.stdout_piped),
            stderr: stdio_mode(self.stderr_piped),
        }
    }

    fn spawn(&self, spec: &CommandSpec, hook_name: &str) -> Result<RunningCommand, HookError> {
        self.runner.spawn(spec).map_err(|source| HookError::Spawn {
            hook_name: hook_name.to_owned(),
            path: spec.program.clone(),
            source,
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
        let via_shell = |shell: PathBuf| {
            let invocation = ShellInvocation::ScriptPath {
                script_path: argv0.to_path_buf(),
                args: args.iter().map(OsString::from).collect(),
            };
            self.spec(shell, Some(invocation), args)
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
            &self.spec(hook_shell(self.env), Some(invocation), args),
            hook_name,
        )
    }
}

/// Well-known commit lifecycle hooks tracked in [`crate::porcelain::commit::CommitOutcome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookName {
    /// `.git/hooks/pre-commit` or `hook.pre-commit.*`.
    PreCommit,
    /// `.git/hooks/commit-msg` or `hook.commit-msg.*`.
    CommitMsg,
    /// `.git/hooks/post-commit` or `hook.post-commit.*`.
    PostCommit,
}

impl HookName {
    /// Git hook basename (`pre-commit`, `commit-msg`, `post-commit`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PreCommit => "pre-commit",
            Self::CommitMsg => "commit-msg",
            Self::PostCommit => "post-commit",
        }
    }
}

/// Result of running a hook.
#[derive(Debug)]
pub enum HookResult {
    /// Hook ran successfully (exit code 0).
    Success,
    /// Hook does not exist or is not executable — treated as success.
    NotFound,
    /// Hook ran but returned a non-zero exit code.
    Failed(i32),
}

impl HookResult {
    /// Returns true if the hook was successful or not found.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        matches!(self, HookResult::Success | HookResult::NotFound)
    }

    /// Returns true if the hook existed and ran (regardless of exit code).
    #[must_use]
    pub fn was_executed(&self) -> bool {
        matches!(self, HookResult::Success | HookResult::Failed(_))
    }
}

/// Options for [`run_hook_opts`].
#[derive(Debug, Clone, Default)]
pub struct RunHookOptions<'a> {
    /// When true, hook stdout is merged to stderr (Git default except `pre-push`).
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
/// When `capture_output` is `Some`, each hook's stdout and stderr are appended there (receive-pack /
/// simulated remote) instead of being written to the process stderr.
///
/// # Errors
///
/// [`HookError::Spawn`] when a hook (or the shell that runs it) cannot be started, and
/// [`HookError::Internal`] for invalid `hook.*` configuration. A hook that runs and exits
/// non-zero is `Ok(HookResult::Failed(status))`.
pub fn run_hook_opts(
    repo: Option<&Repository>,
    hook_name: &str,
    args: &[&str],
    config: &ConfigSet,
    opts: RunHookOptions<'_>,
    mut capture_output: Option<&mut Vec<u8>>,
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
            Some(r) => r
                .work_tree()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| r.git_dir().to_path_buf()),
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

    let hook_env = match repo {
        Some(r) => CommandEnvironment::from_repository_environment(r.environment(), &merged_env),
        None => hook_command_env(git_dir_for_configured, &merged_env),
    };

    for h in &seq {
        let (stdin_piped, stdin_file) = match opts.path_to_stdin {
            Some(p) => (true, Some(p.to_path_buf())),
            None => (opts.stdin_data.is_some(), None),
        };

        let capture_mode = capture_output.is_some();
        let pipe_stderr_to_capture = capture_mode || opts.stdout_to_stderr;
        let (stdout_piped, stderr_piped) = if pipe_stderr_to_capture {
            (true, true)
        } else {
            (false, false)
        };

        let ctx = HookSpawnContext {
            runner: runner.as_ref(),
            cwd: work_dir,
            env: &hook_env,
            stdin_piped,
            stdout_piped,
            stderr_piped,
        };
        let mut child = match h {
            ResolvedHook::Traditional { path, argv0 } => {
                let Some(r) = repo else {
                    continue;
                };
                let effective_argv0 = path
                    .parent()
                    .map(|hooks_dir| hook_argv0(r, hooks_dir, hook_name, work_dir))
                    .unwrap_or_else(|| argv0.clone());
                ctx.spawn_traditional(hook_name, path, &effective_argv0, args)?
            }
            ResolvedHook::Configured { command } => {
                ctx.spawn_configured(hook_name, command, args)?
            }
        };

        if let Some(ref path) = stdin_file {
            let file = match fs::File::open(path) {
                Ok(f) => f,
                Err(_e) => {
                    return Ok(HookResult::Failed(1));
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

        let exit = if capture_mode {
            let output = match child.wait_with_output() {
                Ok(o) => o,
                Err(_) => return Ok(HookResult::Failed(1)),
            };
            if let Some(buf) = capture_output.as_mut() {
                buf.extend_from_slice(&output.stdout);
                buf.extend_from_slice(&output.stderr);
            }
            output.status
        } else if opts.stdout_to_stderr {
            let output = match child.wait_with_output() {
                Ok(o) => o,
                Err(_) => return Ok(HookResult::Failed(1)),
            };
            let mut stderr = std::io::stderr().lock();
            let _ = stderr.write_all(&output.stdout);
            let _ = stderr.write_all(&output.stderr);
            output.status
        } else {
            match child.wait() {
                Ok(s) => s,
                Err(_) => return Ok(HookResult::Failed(1)),
            }
        };

        if !exit.success {
            if let Some(r) = repo {
                let _ = r.reload_config();
            }
            return Ok(HookResult::Failed(exit.code.unwrap_or(1)));
        }
    }

    if let Some(r) = repo {
        let _ = r.reload_config();
    }

    Ok(HookResult::Success)
}

/// Like [`run_commit_hook`] but a hook that exits non-zero is an error.
///
/// # Errors
///
/// [`HookError::Failed`] with the hook's exit status, or any error from [`run_commit_hook`].
pub fn run_commit_hook_checked(
    repo: &Repository,
    hook_name: &str,
    args: &[&str],
    stdin_data: Option<&[u8]>,
    commit_env: &CommitHookEnv<'_>,
) -> Result<(), HookError> {
    match run_commit_hook(repo, hook_name, args, stdin_data, commit_env)? {
        HookResult::Success | HookResult::NotFound => Ok(()),
        HookResult::Failed(status) => Err(HookError::Failed {
            hook_name: hook_name.to_owned(),
            status,
        }),
    }
}

/// Run commit-style hooks with `GIT_INDEX_FILE`, `GIT_PREFIX`, and related env (Git `run_commit_hook`).
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
        .map_err(|e| HookError::Internal(e.to_string()))?
        .as_ref()
        .clone();
    let stdout_to_stderr = hook_name != "pre-push";
    run_hook_opts(
        Some(repo),
        hook_name,
        args,
        &config,
        RunHookOptions {
            stdout_to_stderr,
            path_to_stdin: None,
            stdin_data,
            env_vars: &[],
            cwd: None,
            commit_env: Some(commit_env),
        },
        None,
    )
}

/// Run a hook by name with the given arguments (Git-compatible multihooks).
///
/// `pre-push` keeps stdout separate; other hooks merge stdout to stderr.
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
    let stdout_to_stderr = hook_name != "pre-push";
    run_hook_opts(
        Some(repo),
        hook_name,
        args,
        config.as_ref(),
        RunHookOptions {
            stdout_to_stderr,
            path_to_stdin: None,
            stdin_data,
            env_vars: &[],
            cwd: None,
            commit_env: None,
        },
        None,
    )
}

/// Run a hook with extra env vars, cwd = `GIT_DIR` (receive-pack and similar).
pub fn run_hook_in_git_dir(
    repo: &Repository,
    hook_name: &str,
    args: &[&str],
    stdin_data: Option<&[u8]>,
    env_vars: &[(&str, &str)],
) -> (HookResult, Vec<u8>) {
    let Some(config) = repo.config().ok() else {
        return (HookResult::Failed(1), Vec::new());
    };
    let mut captured = Vec::new();
    match run_hook_opts(
        Some(repo),
        hook_name,
        args,
        config.as_ref(),
        RunHookOptions {
            stdout_to_stderr: true,
            path_to_stdin: None,
            stdin_data,
            env_vars,
            cwd: Some(repo.git_dir()),
            commit_env: None,
        },
        Some(&mut captured),
    ) {
        Ok(r) => (r, captured),
        Err(_) => (HookResult::Failed(1), captured),
    }
}

/// Like `run_hook` but with extra environment variables and captures output.
pub fn run_hook_with_env(
    repo: &Repository,
    hook_name: &str,
    args: &[&str],
    stdin_data: Option<&[u8]>,
    env_vars: &[(&str, &str)],
) -> (HookResult, Vec<u8>) {
    let Some(config) = repo.config().ok() else {
        return (HookResult::Failed(1), Vec::new());
    };
    let mut captured = Vec::new();
    match run_hook_opts(
        Some(repo),
        hook_name,
        args,
        config.as_ref(),
        RunHookOptions {
            stdout_to_stderr: true,
            path_to_stdin: None,
            stdin_data,
            env_vars,
            cwd: None,
            commit_env: None,
        },
        Some(&mut captured),
    ) {
        Ok(r) => (r, captured),
        Err(_) => (HookResult::Failed(1), captured),
    }
}

/// `run_hook_capture` function.
pub fn run_hook_capture(
    repo: &Repository,
    hook_name: &str,
    args: &[&str],
    stdin_data: Option<&[u8]>,
) -> (HookResult, Vec<u8>) {
    run_hook_with_env(repo, hook_name, args, stdin_data, &[])
}

/// `reference-transaction` hook with phase `committed` after updating `HEAD` and (on a branch)
/// the branch ref to `new_oid`.
///
/// `old_head_commit` is the commit OID `HEAD` pointed at before the update, or `None` for an
/// unborn branch (null old OID in hook stdin).
///
/// # Errors
///
/// Same as [`run_hook`].
pub fn run_reference_transaction_committed_for_head_update(
    repo: &Repository,
    head: &HeadState,
    old_head_commit: Option<ObjectId>,
    new_oid: ObjectId,
) -> Result<HookResult, HookError> {
    let zero = ObjectId::zero();
    let old_oid = old_head_commit.unwrap_or(zero);
    let old_hex = if old_oid == zero {
        "0000000000000000000000000000000000000000".to_owned()
    } else {
        old_oid.to_hex()
    };
    let new_hex = new_oid.to_hex();
    let mut stdin = String::new();
    match head {
        HeadState::Branch { refname, .. } => {
            // Git sorts ref updates lexicographically; `HEAD` precedes `refs/...`.
            stdin.push_str(&format!("{old_hex} {new_hex} HEAD\n"));
            stdin.push_str(&format!("{old_hex} {new_hex} {refname}\n"));
        }
        _ => {
            stdin.push_str(&format!("{old_hex} {new_hex} HEAD\n"));
        }
    }
    run_hook(
        repo,
        "reference-transaction",
        &["committed"],
        Some(stdin.as_bytes()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, b"").expect("write");
    }

    fn exe(name: &str) -> String {
        format!("{name}{}", std::env::consts::EXE_SUFFIX)
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

    #[test]
    fn find_program_in_path_searches_entries_in_order() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        fs::create_dir_all(&first).expect("mkdir");
        touch(&second.join(exe("sh")));
        touch(&tmp.path().join("third").join(exe("sh")));
        let path_var = std::env::join_paths([
            PathBuf::new(),
            first,
            second.clone(),
            tmp.path().join("third"),
        ])
        .expect("join");

        assert_eq!(
            find_program_in_path("sh", &path_var),
            Some(second.join(exe("sh")))
        );
        assert_eq!(find_program_in_path("bash", &path_var), None);
        assert_eq!(find_program_in_path("sh", OsStr::new("")), None);
    }

    #[test]
    fn resolve_script_interpreter_prefers_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bin = tmp.path().join("bin");
        touch(&bin.join(exe("bash")));
        let path_var = bin.clone().into_os_string();
        assert_eq!(
            resolve_script_interpreter("bash", Some(&path_var)),
            bin.join(exe("bash"))
        );
    }

    #[test]
    fn resolve_script_interpreter_finds_git_for_windows_shell() {
        for git_dir in [&["cmd"][..], &["mingw64", "bin"][..]] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let root = tmp.path().join("Git");
            let git_bin = git_dir.iter().fold(root.clone(), |p, c| p.join(c));
            touch(&git_bin.join(exe("git")));
            touch(&root.join("usr").join("bin").join(exe("sh")));
            let path_var = git_bin.into_os_string();
            assert_eq!(
                resolve_script_interpreter("sh", Some(&path_var)),
                root.join("usr").join("bin").join(exe("sh")),
                "git under {git_dir:?}"
            );
        }
    }

    #[test]
    fn resolve_script_interpreter_falls_back_to_bare_name() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path_var = tmp.path().as_os_str().to_os_string();
        assert_eq!(
            resolve_script_interpreter("sh", Some(&path_var)),
            PathBuf::from("sh")
        );
        assert_eq!(resolve_script_interpreter("sh", None), PathBuf::from("sh"));
    }

    #[test]
    fn command_env_path_uses_last_explicit_path() {
        let mut env = CommandEnvironment::default();
        assert_eq!(command_env_path(&env), None);
        env.set.push(("PATH".into(), "/first".into()));
        env.set.push(("HOME".into(), "/home/u".into()));
        env.set.push(("PATH".into(), "/second".into()));
        assert_eq!(command_env_path(&env), Some(OsStr::new("/second")));
    }

    #[cfg(windows)]
    #[test]
    fn command_env_path_ignores_case_on_windows() {
        let mut env = CommandEnvironment::default();
        env.set.push(("Path".into(), r"C:\Git\cmd".into()));
        assert_eq!(command_env_path(&env), Some(OsStr::new(r"C:\Git\cmd")));
    }

    #[cfg(unix)]
    #[test]
    fn hook_shell_is_bin_sh_on_unix() {
        let mut env = CommandEnvironment::default();
        env.set.push(("PATH".into(), "/nonexistent".into()));
        assert_eq!(hook_shell(&env), PathBuf::from("/bin/sh"));
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
