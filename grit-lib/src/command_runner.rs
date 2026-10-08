//! Injectable subprocess execution for hooks, filters, credentials, and helpers.
//!
//! All [`std::process::Command`] usage in non-test library code lives in
//! [`SystemCommandRunner`]. Embedders and tests use [`RecordingRunner`] or custom
//! [`CommandRunner`] implementations.

use std::collections::VecDeque;
use std::ffi::OsString;
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
    Inherit,
    Null,
    Pipe,
}

/// Stdin source for the child.
#[derive(Debug, Clone, Default)]
pub enum CommandStdin {
    #[default]
    Inherit,
    Null,
    Pipe(Vec<u8>),
}

/// Shell invocation mode (Git `sh -c` hooks and filters).
#[derive(Debug, Clone)]
pub enum ShellInvocation {
    /// `program` is `sh`: `-c script`, optional `argv0` positional (`hook`), then `args`.
    DashC {
        script: String,
        argv0: Option<String>,
        args: Vec<OsString>,
    },
    /// Run `sh script_path` with `args` (traditional hook ENOEXEC retry).
    ScriptPath {
        script_path: PathBuf,
        args: Vec<OsString>,
    },
}

/// Typed description of a subprocess to spawn.
#[derive(Debug, Clone)]
pub struct CommandSpec {
    /// Executable when not using [`Self::shell`], or `sh` for shell modes.
    pub program: PathBuf,
    /// argv tail when not using shell `-c`.
    pub args: Vec<OsString>,
    /// When set, `program` is invoked as a shell wrapper per [`ShellInvocation`].
    pub shell: Option<ShellInvocation>,
    pub cwd: Option<PathBuf>,
    pub env: CommandEnvironment,
    pub stdin: CommandStdin,
    pub stdout: CommandStdio,
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
    pub status: CommandExit,
    pub stdout: Vec<u8>,
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

struct RecordingStdinBuffer(Mutex<Vec<u8>>);

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
                stdin: piped_stdin.then(|| RecordingStdinBuffer(Mutex::new(Vec::new()))),
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
        let mut cmd = match &spec.shell {
            None => {
                let mut c = Command::new(&spec.program); // hygiene: centralized subprocess spawn
                c.args(&spec.args);
                c
            }
            Some(ShellInvocation::DashC {
                script,
                argv0,
                args,
            }) => {
                let mut c = Command::new(&spec.program); // hygiene: centralized subprocess spawn
                c.arg("-c").arg(script);
                if let Some(name) = argv0 {
                    c.arg(name);
                }
                c.args(args);
                c
            }
            Some(ShellInvocation::ScriptPath { script_path, args }) => {
                let mut c = Command::new(&spec.program); // hygiene: centralized subprocess spawn
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
    Success,
    Exit(i32),
    Output { stdout: Vec<u8>, code: i32 },
}

/// Records [`CommandSpec`] invocations and returns configurable exit statuses.
#[derive(Debug, Default)]
pub struct RecordingRunner {
    pub recorded: Mutex<Vec<CommandSpec>>,
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
                stdin: piped_stdin.then(|| RecordingStdinBuffer(Mutex::new(Vec::new()))),
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

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

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
