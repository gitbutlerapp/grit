//! Injectable subprocess execution for hooks, filters, credentials, and helpers.
//!
//! All [`std::process::Command`] usage in non-test library code lives in
//! [`SystemCommandRunner`]. Embedders and tests use [`RecordingRunner`] or custom
//! [`CommandRunner`] implementations.
//!
//! Library guide: [command_runner](https://grit-scm.com/docs/library/embedding)
//!
//! ```rust
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let _repo = Repository::init(dir.path(), &InitOptions::default())?;
//! # Ok(())
//! # }
//! ```

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::io::{self, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use thiserror::Error;

use crate::environment::Environment;

/// Returned when an operation requires a live OS child but the handle is synthetic.
#[derive(Debug, Error)]
#[error("command handle is not backed by a live OS process")]
pub struct CommandHandleError;

/// How a child process should inherit or receive environment variables.
#[derive(Debug, Clone, Default)]
pub struct CommandEnvironment {
    /// When `true`, the child starts from the current process environment (system runner only).
    pub inherit_process: bool,
    /// Variables to set or override (after inherit).
    pub set: Vec<(OsString, OsString)>,
    /// Variable names to remove (after inherit and `set`).
    pub remove: Vec<OsString>,
}

impl CommandEnvironment {
    /// Inherit the process environment with no changes.
    #[must_use]
    pub fn inherit_process_only() -> Self {
        Self {
            inherit_process: true,
            set: Vec::new(),
            remove: Vec::new(),
        }
    }

    /// Build env for a subprocess from an explicit [`Environment`] snapshot plus overrides.
    ///
    /// The child does **not** inherit the host process environment; only variables present
    /// on `env` and `extra` are exported (`extra` wins on duplicate keys).
    #[must_use]
    pub fn from_repository_environment(env: &Environment, extra: &[(String, String)]) -> Self {
        let mut set = env.subprocess_environment();
        for (k, v) in extra {
            let key = OsString::from(k.as_str());
            let val = OsString::from(v.as_str());
            if let Some(slot) = set.iter_mut().find(|(ek, _)| ek == &key) {
                slot.1 = val;
            } else {
                set.push((key, val));
            }
        }
        Self {
            inherit_process: false,
            set,
            remove: Vec::new(),
        }
    }
}

/// Stdio disposition for a child stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CommandStdio {
    #[default]
    /// Returned when inherit.
    Inherit,
    /// Returned when null.
    Null,
    /// Returned when pipe.
    Pipe,
}

/// Stdin source for the child.
#[derive(Debug, Clone, Default)]
pub enum CommandStdin {
    #[default]
    /// Returned when inherit.
    Inherit,
    /// Returned when null.
    Null,
    /// Returned when pipe.
    Pipe(Vec<u8>),
}

/// Shell invocation mode (Git `sh -c` hooks and filters).
#[derive(Debug, Clone)]
pub enum ShellInvocation {
    /// `program` is `sh`: `-c script`, optional `argv0` positional (`hook`), then `args`.
    DashC {
        /// Script for this error or warning.
        script: String,
        /// Argv0 for this error or warning.
        argv0: Option<String>,
        /// Args for this error or warning.
        args: Vec<OsString>,
    },
    /// Run `sh script_path` with `args` (traditional hook ENOEXEC retry).
    ScriptPath {
        /// Script path for this error or warning.
        script_path: PathBuf,
        /// Args for this error or warning.
        args: Vec<OsString>,
    },
}

/// Typed description of a subprocess to spawn.
#[derive(Debug, Clone)]
pub struct CommandSpec {
    /// Executable when not using [`Self::shell`], or `sh` for shell modes.
    ///
    /// [`SystemCommandRunner`] runs a bare `sh` the way Git does: `/bin/sh` on Unix, and on
    /// Windows the `sh` of the Git for Windows install that owns `git` on the child's `PATH`
    /// (then `sh` elsewhere on that `PATH`), so filters and helpers work when only Git's
    /// `cmd` directory is on `PATH`.
    pub program: PathBuf,
    /// argv tail when not using shell `-c`.
    pub args: Vec<OsString>,
    /// When set, `program` is invoked as a shell wrapper per [`ShellInvocation`].
    pub shell: Option<ShellInvocation>,
    /// Process working directory used during repository discovery.
    pub cwd: Option<PathBuf>,
    /// Env for this item.
    pub env: CommandEnvironment,
    /// Stdin for this item.
    pub stdin: CommandStdin,
    /// Stdout for this item.
    pub stdout: CommandStdio,
    /// Stderr for this item.
    pub stderr: CommandStdio,
}

impl CommandSpec {
    /// `sh -c expanded` with piped stdin/stdout (one-shot clean/smudge filter).
    #[must_use]
    pub fn sh_dash_c_filter(script: impl Into<String>) -> Self {
        Self {
            program: PathBuf::from("sh"),
            args: Vec::new(),
            shell: Some(ShellInvocation::DashC {
                script: script.into(),
                argv0: None,
                args: Vec::new(),
            }),
            cwd: None,
            env: CommandEnvironment::inherit_process_only(),
            stdin: CommandStdin::Pipe(Vec::new()),
            stdout: CommandStdio::Pipe,
            stderr: CommandStdio::Inherit,
        }
    }
}

/// Exit status of a completed child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandExit {
    /// Process exit code when available.
    pub code: Option<i32>,
    /// Whether the process exited successfully.
    pub success: bool,
}

impl From<ExitStatus> for CommandExit {
    fn from(status: ExitStatus) -> Self {
        Self {
            code: status.code(),
            success: status.success(),
        }
    }
}

/// Output from [`RunningCommand::wait_with_output`].
#[derive(Debug, Clone)]
pub struct CommandOutput {
    /// Subprocess exit status from the filter command.
    pub status: CommandExit,
    /// Stdout for this item.
    pub stdout: Vec<u8>,
    /// Stderr for this item.
    pub stderr: Vec<u8>,
}

enum RunningCommandInner {
    System {
        child: Child,
        stdin: Option<ChildStdin>,
        stdout: Option<ChildStdout>,
        stderr: Option<ChildStderr>,
    },
    Recording {
        state: Arc<Mutex<RecordingState>>,
        stdin: Option<RecordingStdinBuffer>,
    },
}

struct RecordingStdinBuffer(Arc<Mutex<Vec<u8>>>);

impl Write for RecordingStdinBuffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).flush()
    }
}

struct RecordingState {
    stdout: Option<Cursor<Vec<u8>>>,
    stderr: Vec<u8>,
    exit: CommandExit,
}

/// Live child process with optional piped stdio.
pub struct RunningCommand {
    inner: RunningCommandInner,
}

impl RunningCommand {
    /// Build a handle from a spawned OS child and taken stdio pipes.
    #[must_use]
    pub fn from_system(
        child: Child,
        stdin: Option<ChildStdin>,
        stdout: Option<ChildStdout>,
        stderr: Option<ChildStderr>,
    ) -> Self {
        Self {
            inner: RunningCommandInner::System {
                child,
                stdin,
                stdout,
                stderr,
            },
        }
    }

    /// Build a synthetic handle for tests and custom [`CommandRunner`] implementations.
    #[must_use]
    pub fn from_recording(
        exit: CommandExit,
        stdout: Option<Vec<u8>>,
        stderr: Vec<u8>,
        piped_stdin: bool,
    ) -> Self {
        Self {
            inner: RunningCommandInner::Recording {
                state: Arc::new(Mutex::new(RecordingState {
                    stdout: stdout.map(Cursor::new),
                    stderr,
                    exit,
                })),
                stdin: piped_stdin.then(|| RecordingStdinBuffer(Arc::new(Mutex::new(Vec::new())))),
            },
        }
    }

    /// Writable stdin when the spec requested a pipe.
    pub fn stdin_mut(&mut self) -> Option<&mut dyn Write> {
        match &mut self.inner {
            RunningCommandInner::System { stdin, .. } => {
                stdin.as_mut().map(|s| s as &mut dyn Write)
            }
            RunningCommandInner::Recording { stdin, .. } => {
                stdin.as_mut().map(|s| s as &mut dyn Write)
            }
        }
    }

    /// Close the stdin pipe so the child sees EOF (required after writing hook/filter stdin).
    pub fn close_stdin(&mut self) {
        match &mut self.inner {
            RunningCommandInner::System { stdin, .. } => *stdin = None,
            RunningCommandInner::Recording { stdin, .. } => *stdin = None,
        }
    }

    /// Readable stdout when the spec requested a pipe.
    pub fn stdout_mut(&mut self) -> Option<&mut dyn Read> {
        match &mut self.inner {
            RunningCommandInner::System { stdout, .. } => {
                stdout.as_mut().map(|s| s as &mut dyn Read)
            }
            RunningCommandInner::Recording { .. } => None,
        }
    }

    /// Wait for the child and return its exit status.
    ///
    /// # Errors
    ///
    /// Returns I/O errors from [`Child::wait`].
    pub fn wait(self) -> io::Result<CommandExit> {
        match self.inner {
            RunningCommandInner::System { mut child, .. } => child.wait().map(Into::into),
            RunningCommandInner::Recording { state, .. } => {
                let guard = state.lock().unwrap_or_else(|e| e.into_inner());
                Ok(guard.exit)
            }
        }
    }

    /// Wait and collect piped stdout/stderr.
    ///
    /// # Errors
    ///
    /// Returns I/O errors from [`Child::wait_with_output`].
    pub fn wait_with_output(self) -> io::Result<CommandOutput> {
        match self.inner {
            RunningCommandInner::System {
                mut child,
                stdout,
                stderr,
                ..
            } => {
                let (stdout_bytes, stderr_bytes) = drain_child_pipes(stdout, stderr)?;
                let status = child.wait()?.into();
                Ok(CommandOutput {
                    status,
                    stdout: stdout_bytes,
                    stderr: stderr_bytes,
                })
            }
            RunningCommandInner::Recording { state, .. } => {
                let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
                let mut stdout_bytes = Vec::new();
                if let Some(cursor) = &mut guard.stdout {
                    let _ = cursor.read_to_end(&mut stdout_bytes);
                }
                Ok(CommandOutput {
                    status: guard.exit,
                    stdout: stdout_bytes,
                    stderr: guard.stderr.clone(),
                })
            }
        }
    }

    /// Underlying OS child for long-running filters (system runner only).
    ///
    /// # Errors
    ///
    /// Returns [`CommandHandleError`] when the handle is not backed by a live process.
    pub fn into_child(self) -> Result<Child, CommandHandleError> {
        match self.inner {
            RunningCommandInner::System { child, .. } => Ok(child),
            RunningCommandInner::Recording { .. } => Err(CommandHandleError),
        }
    }

    /// Split into OS child and taken stdio (system runner only).
    ///
    /// # Errors
    ///
    /// Returns [`CommandHandleError`] when the handle is not backed by a live process.
    #[expect(clippy::type_complexity)]
    pub fn into_system_parts(
        self,
    ) -> Result<
        (
            Child,
            Option<ChildStdin>,
            Option<ChildStdout>,
            Option<ChildStderr>,
        ),
        CommandHandleError,
    > {
        match self.inner {
            RunningCommandInner::System {
                child,
                stdin,
                stdout,
                stderr,
            } => Ok((child, stdin, stdout, stderr)),
            RunningCommandInner::Recording { .. } => Err(CommandHandleError),
        }
    }
}

fn drain_child_pipes(
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
) -> io::Result<(Vec<u8>, Vec<u8>)> {
    std::thread::scope(|scope| {
        let out_task = stdout.map(|mut out| {
            scope.spawn(move || {
                let mut buf = Vec::new();
                let _ = out.read_to_end(&mut buf);
                buf
            })
        });
        let err_task = stderr.map(|mut err| {
            scope.spawn(move || {
                let mut buf = Vec::new();
                let _ = err.read_to_end(&mut buf);
                buf
            })
        });
        let stdout_bytes = out_task
            .map(|h| h.join().unwrap_or_default())
            .unwrap_or_default();
        let stderr_bytes = err_task
            .map(|h| h.join().unwrap_or_default())
            .unwrap_or_default();
        Ok((stdout_bytes, stderr_bytes))
    })
}

/// Spawns subprocesses; the only production implementation uses [`std::process::Command`].
pub trait CommandRunner: Send + Sync {
    /// Start `spec` and return a handle with stdio configured per the spec.
    ///
    /// # Errors
    ///
    /// Returns I/O errors from spawn or missing piped stdio when requested.
    fn spawn(&self, spec: &CommandSpec) -> io::Result<RunningCommand>;
}

/// Runs commands with the real OS process API.
#[derive(Debug, Clone, Default)]
pub struct SystemCommandRunner;

impl SystemCommandRunner {
    // hygiene: grit-lib subprocess execution is centralized here.
    fn build_command(spec: &CommandSpec) -> Command {
        let program = if spec.program.as_os_str() == SHELL_NAME {
            shell_program(&spec.env)
        } else {
            spec.program.clone()
        };
        let mut cmd = match &spec.shell {
            None => {
                let mut c = Command::new(&program); // hygiene: centralized subprocess spawn
                c.args(&spec.args);
                c
            }
            Some(ShellInvocation::DashC {
                script,
                argv0,
                args,
            }) => {
                let mut c = Command::new(&program); // hygiene: centralized subprocess spawn
                c.arg("-c").arg(script);
                if let Some(name) = argv0 {
                    c.arg(name);
                }
                c.args(args);
                c
            }
            Some(ShellInvocation::ScriptPath { script_path, args }) => {
                let mut c = Command::new(&program); // hygiene: centralized subprocess spawn
                c.arg(script_path);
                c.args(args);
                c
            }
        };

        if let Some(cwd) = &spec.cwd {
            cmd.current_dir(cwd);
        }

        apply_env(&mut cmd, &spec.env);
        apply_stdio(&mut cmd, spec);
        cmd
    }
}

fn apply_env(cmd: &mut Command, env: &CommandEnvironment) {
    if !env.inherit_process {
        cmd.env_clear();
    }
    for key in &env.remove {
        cmd.env_remove(key);
    }
    for (k, v) in &env.set {
        cmd.env(k, v);
    }
}

fn apply_stdio(cmd: &mut Command, spec: &CommandSpec) {
    cmd.stdin(match spec.stdin {
        CommandStdin::Inherit => Stdio::inherit(),
        CommandStdin::Null => Stdio::null(),
        CommandStdin::Pipe(_) => Stdio::piped(),
    });
    cmd.stdout(match spec.stdout {
        CommandStdio::Inherit => Stdio::inherit(),
        CommandStdio::Null => Stdio::null(),
        CommandStdio::Pipe => Stdio::piped(),
    });
    cmd.stderr(match spec.stderr {
        CommandStdio::Inherit => Stdio::inherit(),
        CommandStdio::Null => Stdio::null(),
        CommandStdio::Pipe => Stdio::piped(),
    });
}

/// [`CommandSpec::program`] value that stands for the platform shell.
const SHELL_NAME: &str = "sh";

/// `PATH` as a child spawned with `env` will see it: the last explicit `PATH` in
/// [`CommandEnvironment::set`], else the process `PATH` when the child inherits it.
pub(crate) fn child_path_var(env: &CommandEnvironment) -> Option<OsString> {
    let is_path = |key: &OsString| {
        if cfg!(windows) {
            key.eq_ignore_ascii_case("PATH")
        } else {
            key == "PATH"
        }
    };
    env.set
        .iter()
        .rev()
        .find(|(k, _)| is_path(k))
        .map(|(_, v)| v.clone())
        .or_else(|| {
            env.inherit_process
                .then(|| std::env::var_os("PATH"))
                .flatten()
        })
}

/// Shell for `sh -c` commands (filters, hooks, helpers) spawned with `env`.
///
/// `/bin/sh` on Unix, as Git uses. Windows has no `/bin/sh`, so `sh` is resolved against
/// the child's `PATH` with [`resolve_script_interpreter`].
pub(crate) fn shell_program(env: &CommandEnvironment) -> PathBuf {
    if cfg!(windows) {
        resolve_script_interpreter(SHELL_NAME, child_path_var(env).as_deref())
    } else {
        PathBuf::from("/bin/sh")
    }
}

/// First `dir/name` (or `dir/name.exe` on Windows) that is a file, for `dir` in `dirs`.
fn find_program_in_dirs(name: &str, dirs: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    let suffix = std::env::consts::EXE_SUFFIX;
    dirs.into_iter()
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

/// [`find_program_in_dirs`] over the entries of a `PATH` value.
fn find_program_in_path(name: &str, path_var: &OsStr) -> Option<PathBuf> {
    find_program_in_dirs(name, std::env::split_paths(path_var))
}

/// Whether `dir` holds Windows' WSL launchers (`System32\bash.exe`, `WindowsApps\bash.exe`),
/// which would run a script inside a Linux distribution instead of against this checkout.
fn is_wsl_launcher_dir(dir: &Path) -> bool {
    dir.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
        n.eq_ignore_ascii_case("System32") || n.eq_ignore_ascii_case("WindowsApps")
    })
}

/// Resolve the interpreter `name` (`sh`, `bash`, …) for running a script or shell command.
///
/// Looks first in the Git for Windows tree that owns a `git` found on `path_var`
/// (`<root>/usr/bin`, `<root>/bin`), as Git for Windows puts those ahead of `PATH` for the
/// commands it runs, then on `path_var` itself, skipping the WSL launcher directories. Falls
/// back to the bare name so the OS search applies.
pub(crate) fn resolve_script_interpreter(name: &str, path_var: Option<&OsStr>) -> PathBuf {
    let Some(path_var) = path_var else {
        return PathBuf::from(name);
    };
    let in_git_tree = || {
        let git = find_program_in_path("git", path_var)?;
        let bin_dir = git.parent()?.to_path_buf();
        let roots: Vec<PathBuf> = bin_dir
            .ancestors()
            .skip(1)
            .take(2)
            .map(Path::to_path_buf)
            .collect();
        find_program_in_dirs(
            name,
            roots
                .iter()
                .flat_map(|root| [root.join("usr").join("bin"), root.join("bin")]),
        )
    };
    in_git_tree()
        .or_else(|| {
            find_program_in_dirs(
                name,
                std::env::split_paths(path_var).filter(|dir| !is_wsl_launcher_dir(dir)),
            )
        })
        .unwrap_or_else(|| PathBuf::from(name))
}

impl CommandRunner for SystemCommandRunner {
    fn spawn(&self, spec: &CommandSpec) -> io::Result<RunningCommand> {
        let mut cmd = Self::build_command(spec);
        let mut child = cmd.spawn()?;

        let mut stdin = child.stdin.take();
        if let CommandStdin::Pipe(ref bytes) = spec.stdin {
            if let Some(ref mut stdin_handle) = stdin {
                if !bytes.is_empty() {
                    let _ = stdin_handle.write_all(bytes);
                    // Close stdin so one-shot helpers (credentials, filters) see EOF.
                    stdin = None;
                }
            }
        }

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        Ok(RunningCommand::from_system(child, stdin, stdout, stderr))
    }
}

/// Default shared system runner.
#[must_use]
pub fn system_command_runner() -> Arc<dyn CommandRunner> {
    Arc::new(SystemCommandRunner)
}

/// Scripted responses for [`RecordingRunner`].
#[derive(Debug, Clone)]
pub enum RecordedResponse {
    /// Returned when success.
    Success,
    /// Returned when exit.
    Exit(i32),
    /// Returned when output.
    Output {
        /// Stdout disposition for the child process.
        stdout: Vec<u8>,
        /// Code.
        code: i32,
    },
}

/// Records [`CommandSpec`] invocations and returns configurable exit statuses.
#[derive(Debug, Default)]
pub struct RecordingRunner {
    /// Recorded for this item.
    pub recorded: Mutex<Vec<CommandSpec>>,
    stdin_payloads: Mutex<Vec<Arc<Mutex<Vec<u8>>>>>,
    responses: Mutex<VecDeque<RecordedResponse>>,
}

impl RecordingRunner {
    /// Create a runner that succeeds for every spawn.
    #[must_use]
    pub fn always_success() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Queue responses consumed in FIFO order (defaults to success when empty).
    pub fn push_response(&self, response: RecordedResponse) {
        self.responses
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(response);
    }

    /// Snapshot recorded specs.
    #[must_use]
    pub fn specs(&self) -> Vec<CommandSpec> {
        self.recorded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Stdin bytes written to each spawned hook/filter, in spawn order.
    #[must_use]
    pub fn stdin_payloads(&self) -> Vec<Vec<u8>> {
        self.stdin_payloads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|buf| buf.lock().unwrap_or_else(|e| e.into_inner()).clone())
            .collect()
    }
}

impl CommandRunner for RecordingRunner {
    fn spawn(&self, spec: &CommandSpec) -> io::Result<RunningCommand> {
        self.recorded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(spec.clone());

        let response = self
            .responses
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
            .unwrap_or(RecordedResponse::Success);

        let piped_stdin = matches!(spec.stdin, CommandStdin::Pipe(_));

        let (stdout_bytes, exit) = match response {
            RecordedResponse::Success => (
                None,
                CommandExit {
                    code: Some(0),
                    success: true,
                },
            ),
            RecordedResponse::Exit(code) => (
                None,
                CommandExit {
                    code: Some(code),
                    success: code == 0,
                },
            ),
            RecordedResponse::Output { stdout, code } => (
                Some(stdout),
                CommandExit {
                    code: Some(code),
                    success: code == 0,
                },
            ),
        };

        let stdout_cursor = if spec.stdout == CommandStdio::Pipe {
            Some(Cursor::new(stdout_bytes.unwrap_or_default()))
        } else {
            None
        };

        Ok(RunningCommand {
            inner: RunningCommandInner::Recording {
                state: Arc::new(Mutex::new(RecordingState {
                    stdout: stdout_cursor,
                    stderr: Vec::new(),
                    exit,
                })),
                stdin: piped_stdin.then(|| {
                    let buf = Arc::new(Mutex::new(Vec::new()));
                    self.stdin_payloads
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(buf.clone());
                    RecordingStdinBuffer(buf)
                }),
            },
        })
    }
}

/// Run `sh -c script` and collect stdout (common helper for textconv/trailers).
pub fn run_sh_dash_c(
    runner: &dyn CommandRunner,
    script: &str,
    cwd: Option<&Path>,
    stdin: CommandStdin,
    stderr: CommandStdio,
) -> io::Result<CommandOutput> {
    let mut spec = CommandSpec::sh_dash_c_filter(script);
    spec.cwd = cwd.map(Path::to_path_buf);
    spec.stdin = stdin;
    spec.stderr = stderr;
    runner.spawn(&spec)?.wait_with_output()
}

/// Run a program with argv and collect stdout.
pub fn run_program(
    runner: &dyn CommandRunner,
    program: &Path,
    args: &[OsString],
    cwd: Option<&Path>,
    stdin: CommandStdin,
    stderr: CommandStdio,
) -> io::Result<CommandOutput> {
    let spec = CommandSpec {
        program: program.to_path_buf(),
        args: args.to_vec(),
        shell: None,
        cwd: cwd.map(Path::to_path_buf),
        env: CommandEnvironment::inherit_process_only(),
        stdin,
        stdout: CommandStdio::Pipe,
        stderr,
    };
    runner.spawn(&spec)?.wait_with_output()
}

/// Git hook execution helpers.
pub mod hooks {
    pub use crate::hooks::*;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, b"").expect("write");
    }

    fn exe(name: &str) -> String {
        format!("{name}{}", std::env::consts::EXE_SUFFIX)
    }

    #[test]
    fn find_program_in_path_searches_entries_in_order() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        std::fs::create_dir_all(&first).expect("mkdir");
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
    fn resolve_script_interpreter_prefers_git_tree_over_earlier_path_entries() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let other = tmp.path().join("other-bin");
        touch(&other.join(exe("bash")));
        let root = tmp.path().join("Git");
        touch(&root.join("cmd").join(exe("git")));
        touch(&root.join("usr").join("bin").join(exe("bash")));
        let path_var = std::env::join_paths([other, root.join("cmd")]).expect("join");
        assert_eq!(
            resolve_script_interpreter("bash", Some(&path_var)),
            root.join("usr").join("bin").join(exe("bash"))
        );
    }

    #[test]
    fn resolve_script_interpreter_skips_wsl_launchers() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let system32 = tmp.path().join("Windows").join("System32");
        let apps = tmp.path().join("WindowsApps");
        let msys = tmp.path().join("msys64").join("usr").join("bin");
        touch(&system32.join(exe("bash")));
        touch(&apps.join(exe("bash")));
        touch(&msys.join(exe("bash")));
        let path_var =
            std::env::join_paths([system32.clone(), apps.clone(), msys.clone()]).expect("join");
        assert_eq!(
            resolve_script_interpreter("bash", Some(&path_var)),
            msys.join(exe("bash"))
        );
        let only_wsl = std::env::join_paths([system32, apps]).expect("join");
        assert_eq!(
            resolve_script_interpreter("bash", Some(&only_wsl)),
            PathBuf::from("bash")
        );
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
    fn child_path_var_uses_last_explicit_path() {
        let mut env = CommandEnvironment::default();
        assert_eq!(child_path_var(&env), None);
        env.set.push(("PATH".into(), "/first".into()));
        env.set.push(("HOME".into(), "/home/u".into()));
        env.set.push(("PATH".into(), "/second".into()));
        assert_eq!(child_path_var(&env), Some(OsString::from("/second")));
    }

    #[cfg(windows)]
    #[test]
    fn child_path_var_ignores_case_on_windows() {
        let mut env = CommandEnvironment::default();
        env.set.push(("Path".into(), r"C:\Git\cmd".into()));
        assert_eq!(child_path_var(&env), Some(OsString::from(r"C:\Git\cmd")));
    }

    #[test]
    fn child_path_var_falls_back_to_process_path_only_when_inheriting() {
        let inherit = CommandEnvironment::inherit_process_only();
        assert_eq!(child_path_var(&inherit), std::env::var_os("PATH"));
        assert_eq!(child_path_var(&CommandEnvironment::default()), None);
    }

    #[cfg(unix)]
    #[test]
    fn shell_program_is_bin_sh_on_unix() {
        let mut env = CommandEnvironment::default();
        env.set.push(("PATH".into(), "/nonexistent".into()));
        assert_eq!(shell_program(&env), PathBuf::from("/bin/sh"));
    }

    #[cfg(windows)]
    #[test]
    fn shell_program_finds_git_for_windows_shell_from_cmd_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("Git");
        touch(&root.join("cmd").join(exe("git")));
        touch(&root.join("usr").join("bin").join(exe("sh")));
        let mut env = CommandEnvironment::default();
        env.set
            .push(("Path".into(), root.join("cmd").into_os_string()));
        assert_eq!(
            shell_program(&env),
            root.join("usr").join("bin").join(exe("sh"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn system_runner_runs_bare_sh_without_sh_on_path() {
        let mut spec = CommandSpec::sh_dash_c_filter("printf ok");
        spec.env = CommandEnvironment {
            inherit_process: false,
            set: vec![("PATH".into(), "/nonexistent".into())],
            remove: Vec::new(),
        };
        let out = SystemCommandRunner
            .spawn(&spec)
            .expect("spawn")
            .wait_with_output()
            .expect("wait");
        assert!(out.status.success);
        assert_eq!(out.stdout, b"ok");
    }

    #[cfg(unix)]
    #[test]
    fn system_runner_runs_sh_dash_c_script_with_argument() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let helper = tmp.path().join("h.sh");
        std::fs::write(
            &helper,
            "#!/bin/sh\nif [ \"$1\" = get ]; then echo username=alice; fi\n",
        )
        .expect("write");
        let mut perms = std::fs::metadata(&helper).expect("meta").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&helper, perms).expect("chmod");

        let script = format!("{} get", helper.display());
        let spec = CommandSpec {
            program: "sh".into(),
            args: Vec::new(),
            shell: Some(ShellInvocation::DashC {
                script,
                argv0: None,
                args: Vec::new(),
            }),
            cwd: None,
            env: CommandEnvironment::inherit_process_only(),
            stdin: CommandStdin::Null,
            stdout: CommandStdio::Pipe,
            stderr: CommandStdio::Inherit,
        };
        let out = SystemCommandRunner
            .spawn(&spec)
            .expect("spawn")
            .wait_with_output()
            .expect("wait");
        assert!(out.status.success);
        assert!(
            out.stdout.starts_with(b"username=alice"),
            "runner stdout={:?} stderr={:?} status={:?}",
            out.stdout,
            out.stderr,
            out.status
        );

        let direct = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{} get", helper.display()))
            .output()
            .expect("direct");
        assert!(
            direct.stdout.starts_with(b"username=alice"),
            "direct stdout={:?}",
            direct.stdout
        );
    }

    #[test]
    fn wait_with_output_drains_stderr_and_stdout_concurrently() {
        let script = "dd if=/dev/zero bs=1024 count=1024 >&2; printf ok";
        let spec = CommandSpec {
            program: "sh".into(),
            args: Vec::new(),
            shell: Some(ShellInvocation::DashC {
                script: script.into(),
                argv0: None,
                args: Vec::new(),
            }),
            cwd: None,
            env: CommandEnvironment::inherit_process_only(),
            stdin: CommandStdin::Null,
            stdout: CommandStdio::Pipe,
            stderr: CommandStdio::Pipe,
        };
        let out = SystemCommandRunner
            .spawn(&spec)
            .expect("spawn")
            .wait_with_output()
            .expect("wait");
        assert!(out.status.success);
        assert_eq!(out.stdout, b"ok");
        assert!(out.stderr.len() >= 1024);
    }

    #[test]
    fn recording_runner_captures_spec_and_exit() {
        let runner = RecordingRunner::always_success();
        runner.push_response(RecordedResponse::Output {
            stdout: b"ok".to_vec(),
            code: 0,
        });
        let mut spec = CommandSpec::sh_dash_c_filter("echo hi");
        spec.stdin = CommandStdin::Pipe(b"data".to_vec());
        let handle = runner.spawn(&spec).expect("spawn");
        let out = handle.wait_with_output().expect("wait");
        assert!(out.status.success);
        assert_eq!(out.stdout, b"ok");
        let recorded = runner.specs();
        assert_eq!(recorded.len(), 1);
        match &recorded[0].stdin {
            CommandStdin::Pipe(b) => assert_eq!(b, b"data"),
            _ => panic!("expected pipe stdin"),
        }
    }
}
