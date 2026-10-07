//! Hook execution utilities.
//!
//! Implements Git's multihook model: hooks from `hook.<name>.*` config plus the
//! traditional script in the hooks directory (`core.hooksPath` or `.git/hooks/`).

use crate::config::{parse_path, ConfigSet};
use crate::objects::ObjectId;
use crate::repo::Repository;
use crate::state::HeadState;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[cfg(unix)]
const ENOEXEC: i32 = 8;

#[cfg(unix)]
fn is_enoexec(err: &std::io::Error) -> bool {
    err.raw_os_error() == Some(ENOEXEC)
}

fn stdio_piped(piped: bool) -> Stdio {
    if piped {
        Stdio::piped()
    } else {
        Stdio::inherit()
    }
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

fn absolute_index_path(index_file: &Path) -> PathBuf {
    if index_file.is_absolute() {
        index_file.to_path_buf()
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(index_file)
    } else {
        index_file.to_path_buf()
    }
}

/// `GIT_PREFIX` for the invoking cwd relative to the work tree (Git sets this from the user's
/// `pwd`, not from the hook subprocess cwd, which is usually the work tree root).
fn git_prefix_for_invocation(repo: &Repository, invocation_cwd: &Path) -> String {
    let Some(wt) = repo.work_tree.as_deref() else {
        return String::new();
    };
    if invocation_cwd == repo.git_dir.as_path() {
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
    work_dir: &Path,
    opts: &CommitHookEnv<'_>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    if let Some(p) = opts.index_file {
        env.push((
            "GIT_INDEX_FILE".to_string(),
            absolute_index_path(p).to_string_lossy().into_owned(),
        ));
    }
    let invocation_cwd = std::env::current_dir().unwrap_or_else(|_| work_dir.to_path_buf());
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
pub fn resolve_hooks_dir(repo: &Repository) -> PathBuf {
    resolve_hooks_dir_for_config(Some(&repo.git_dir), repo.config().ok().as_deref())
}

fn resolve_hooks_dir_for_config(git_dir: Option<&Path>, config: Option<&ConfigSet>) -> PathBuf {
    if let Some(cfg) = config {
        if let Some(hooks_path) = cfg.get("core.hooksPath") {
            let expanded = parse_path(&hooks_path);
            let p = PathBuf::from(expanded);
            if p.is_absolute() {
                return p;
            }
            if let Ok(cwd) = std::env::current_dir() {
                return cwd.join(p);
            }
        }
    }
    git_dir
        .map(|gd| crate::repo::common_git_dir_for_config(gd).join("hooks"))
        .unwrap_or_else(|| PathBuf::from("hooks"))
}

fn hook_argv0(repo: &Repository, hooks_dir: &Path, hook_name: &str, cwd: &Path) -> PathBuf {
    let default_hooks_dir = repo.git_dir.join("hooks");
    if hooks_dir == default_hooks_dir.as_path() {
        if cwd == repo.git_dir.as_path() {
            return PathBuf::from("hooks").join(hook_name);
        }
        if let Some(work_tree) = repo.work_tree.as_deref() {
            // Only relativise to `.git/hooks/<name>` when the git dir is literally
            // `<work_tree>/.git`. When `GIT_DIR` points elsewhere (e.g.
            // `GIT_DIR=clone1/.git` run from the parent dir, which makes the work
            // tree default to the process cwd), the relative path would resolve
            // against the wrong directory. In that case fall through to the
            // absolute hook path, mirroring upstream `hook.c`, which absolutises
            // the hook path whenever it runs the hook in a specific directory.
            if cwd == work_tree && same_dir(repo.git_dir.as_path(), &work_tree.join(".git")) {
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
            eprintln!(
                "hint: The '{hook_name}' hook was ignored because it's not set as executable."
            );
            eprintln!(
                "hint: You can disable this warning with `git config set advice.ignoredHook false`."
            );
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
    let hooks_dir = resolve_hooks_dir_for_config(Some(&repo.git_dir), Some(config));
    if let Some(path) = traditional_hook_candidate(repo, &hooks_dir, hook_name) {
        let work_dir = repo.work_tree.as_deref().unwrap_or(&repo.git_dir);
        let argv0 = hook_argv0(repo, &hooks_dir, hook_name, work_dir);
        seq.push(ResolvedHook::Traditional { path, argv0 });
    }
    Ok(seq)
}

/// List hook display lines for `git hook list` (configured friendly names, then `hook from hookdir`).
pub fn list_hooks_display_lines(
    repo: Option<&Repository>,
    hook_name: &str,
    config: &ConfigSet,
) -> Result<Vec<String>, String> {
    let git_dir = repo.map(|r| r.git_dir.as_path());
    let tables = HookConfigTables::from_config(config);
    let mut lines = Vec::new();
    for (friendly, _) in tables.hooks_for_event(hook_name)? {
        lines.push(friendly);
    }
    if let Some(r) = repo {
        let hooks_dir = resolve_hooks_dir_for_config(git_dir, Some(config));
        if traditional_hook_candidate(r, &hooks_dir, hook_name).is_some() {
            lines.push("hook from hookdir".to_owned());
        }
    }
    Ok(lines)
}
#[expect(clippy::too_many_arguments)]
/// Spawn a traditional hook executable. On ENOEXEC, retry with `/bin/sh`.
fn spawn_traditional_hook(
    argv0: &Path,
    hook_args: &[&str],
    cwd: &Path,
    git_dir: &Path,
    extra_env: &[(String, String)],
    stdin_piped: bool,
    stdout_piped: bool,
    stderr_piped: bool,
    use_shell: bool,
) -> std::io::Result<std::process::Child> {
    let mut cmd = if use_shell {
        let mut sh = Command::new("/bin/sh");
        sh.arg(argv0);
        sh
    } else {
        Command::new(argv0)
    };
    cmd.args(hook_args)
        .current_dir(cwd)
        .env("GIT_DIR", git_dir)
        .stdin(stdio_piped(stdin_piped))
        .stdout(stdio_piped(stdout_piped))
        .stderr(stdio_piped(stderr_piped));
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    match cmd.spawn() {
        Ok(c) => Ok(c),
        Err(e) => {
            #[cfg(unix)]
            {
                if !use_shell && is_enoexec(&e) {
                    return spawn_traditional_hook(
                        argv0,
                        hook_args,
                        cwd,
                        git_dir,
                        extra_env,
                        stdin_piped,
                        stdout_piped,
                        stderr_piped,
                        true,
                    );
                }
            }
            Err(e)
        }
    }
}
#[expect(clippy::too_many_arguments)]
/// Spawn a configured hook (`/bin/sh -c <command>`) with optional extra args as `$1`, `$2`, …
fn spawn_configured_hook(
    command: &str,
    hook_args: &[&str],
    cwd: &Path,
    git_dir: Option<&Path>,
    extra_env: &[(String, String)],
    stdin_piped: bool,
    stdout_piped: bool,
    stderr_piped: bool,
) -> std::io::Result<std::process::Child> {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(command)
        .arg("hook")
        .args(hook_args)
        .current_dir(cwd)
        .stdin(stdio_piped(stdin_piped))
        .stdout(stdio_piped(stdout_piped))
        .stderr(stdio_piped(stderr_piped));
    if let Some(gd) = git_dir {
        cmd.env("GIT_DIR", gd);
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.spawn()
}

fn report_spawn_error(path: &Path, err: &std::io::Error) {
    let msg = format!("{err}");
    let p = path.display();
    eprintln!("error: cannot exec '{p}': {msg}");
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
pub fn run_hook_opts(
    repo: Option<&Repository>,
    hook_name: &str,
    args: &[&str],
    config: &ConfigSet,
    opts: RunHookOptions<'_>,
    mut capture_output: Option<&mut Vec<u8>>,
) -> Result<HookResult, String> {
    let seq = match repo {
        Some(r) => resolve_hook_sequence(r, hook_name, config)?,
        None => resolve_configured_hooks_only(hook_name, config)?,
    };
    if seq.is_empty() {
        return Ok(HookResult::NotFound);
    }

    let work_dir: PathBuf = opts.cwd.map_or_else(
        || match repo {
            Some(r) => r.work_tree.clone().unwrap_or_else(|| r.git_dir.clone()),
            None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        },
        Path::to_path_buf,
    );
    let work_dir = work_dir.as_path();
    let git_dir_for_configured = repo.map(|r| r.git_dir.as_path());

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

        let mut child = match h {
            ResolvedHook::Traditional { path, argv0 } => {
                let Some(r) = repo else {
                    continue;
                };
                let gd = r.git_dir.as_path();
                let effective_argv0 = path
                    .parent()
                    .map(|hooks_dir| hook_argv0(r, hooks_dir, hook_name, work_dir))
                    .unwrap_or_else(|| argv0.clone());
                match spawn_traditional_hook(
                    &effective_argv0,
                    args,
                    work_dir,
                    gd,
                    &merged_env,
                    stdin_piped,
                    stdout_piped,
                    stderr_piped,
                    false,
                ) {
                    Ok(c) => c,
                    Err(e) => {
                        report_spawn_error(path, &e);
                        return Ok(HookResult::Failed(1));
                    }
                }
            }
            ResolvedHook::Configured { command } => {
                match spawn_configured_hook(
                    command,
                    args,
                    work_dir,
                    git_dir_for_configured,
                    &merged_env,
                    stdin_piped,
                    stdout_piped,
                    stderr_piped,
                ) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("error: failed to run configured hook: {e}");
                        return Ok(HookResult::Failed(1));
                    }
                }
            }
        };

        if let Some(ref path) = stdin_file {
            let file = match fs::File::open(path) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("error: failed to open stdin file {}: {e}", path.display());
                    return Ok(HookResult::Failed(1));
                }
            };
            if let Some(ref mut stdin) = child.stdin {
                let mut file = file;
                let _ = std::io::copy(&mut file, stdin);
            }
            drop(child.stdin.take());
        } else if let Some(data) = opts.stdin_data {
            if let Some(ref mut stdin) = child.stdin {
                let _ = stdin.write_all(data);
            }
            drop(child.stdin.take());
        }

        let status = if capture_mode {
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

        if !status.success() {
            return Ok(HookResult::Failed(status.code().unwrap_or(1)));
        }
    }

    Ok(HookResult::Success)
}

/// Run commit-style hooks with `GIT_INDEX_FILE`, `GIT_PREFIX`, and related env (Git `run_commit_hook`).
pub fn run_commit_hook(
    repo: &Repository,
    hook_name: &str,
    args: &[&str],
    stdin_data: Option<&[u8]>,
    commit_env: &CommitHookEnv<'_>,
) -> Result<HookResult, String> {
    let config = repo.config().map_err(|e| format!("{e}"))?.as_ref().clone();
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
pub fn run_hook(
    repo: &Repository,
    hook_name: &str,
    args: &[&str],
    stdin_data: Option<&[u8]>,
) -> HookResult {
    let Some(config) = repo.config().ok() else {
        return HookResult::Failed(1);
    };
    let stdout_to_stderr = hook_name != "pre-push";
    match run_hook_opts(
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
    ) {
        Ok(r) => r,
        Err(msg) => {
            eprintln!("fatal: {msg}");
            HookResult::Failed(1)
        }
    }
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
            cwd: Some(repo.git_dir.as_path()),
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
#[must_use]
pub fn run_reference_transaction_committed_for_head_update(
    repo: &Repository,
    head: &HeadState,
    old_head_commit: Option<ObjectId>,
    new_oid: ObjectId,
) -> HookResult {
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
