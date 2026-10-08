//! Shared integration-test support for the Grit workspace.
//!
//! The crate centralizes deterministic command execution, output assertions,
//! temporary workspace helpers, and cached filesystem fixtures used by
//! integration tests in `grit` and `grit-lib`.

pub mod objects;

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

const AUTHOR_NAME: &str = "Test";
const AUTHOR_EMAIL: &str = "test@example.com";
const DETERMINISTIC_DATE: &str = "1700000000 +0000";

/// Assert `left == right`, printing a unified diff when the values differ.
///
/// Parameters are borrowed as strings. The optional trailing format arguments
/// are appended to the panic message to provide command or fixture context.
#[macro_export]
macro_rules! assert_eq_nice {
    ($left:expr, $right:expr $(,)?) => {
        $crate::assert_eq_nice!($left, $right,)
    };
    ($left:expr, $right:expr, $($arg:tt)+) => {{
        use std::borrow::Borrow;
        let left: &str = $left.borrow();
        let right: &str = $right.borrow();
        if left != right {
            let diff = ::similar::TextDiff::from_lines(right, left)
                .unified_diff()
                .context_radius(3)
                .to_string();
            panic!(
                "assertion failed: left != right\n\
                 --- right (expected)\n\
                 +++ left (actual)\n\
                 {diff}\n\
                 {}",
                format_args!($($arg)+),
            );
        }
    }};
}

/// A captured child-process result with UTF-8-lossy stdout and stderr.
#[derive(Clone)]
pub struct Output {
    /// The process exit status code, or `None` when the process was terminated
    /// by a signal on platforms that expose signal termination separately.
    pub status: Option<i32>,
    /// Captured standard output decoded with UTF-8 replacement.
    pub stdout: String,
    /// Captured standard error decoded with UTF-8 replacement.
    pub stderr: String,
}

impl Output {
    /// Return whether the process exited successfully.
    ///
    /// The return value is `true` only for status code `0`.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.status == Some(0)
    }

    /// Format stdout, stderr, and status for assertion failures.
    ///
    /// `label` names the command or assertion context. The returned string is
    /// intended for panic messages and test diagnostics.
    #[must_use]
    pub fn dump(&self, label: &str) -> String {
        format!(
            "{label}: exit={:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.status, self.stdout, self.stderr
        )
    }
}

impl fmt::Debug for Output {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Output")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
enum Program {
    Grit,
    Path(OsString),
}

impl Program {
    fn display(&self) -> String {
        match self {
            Self::Grit => grit_bin().to_string_lossy().into_owned(),
            Self::Path(path) => path.to_string_lossy().into_owned(),
        }
    }

    fn command(&self) -> Command {
        match self {
            Self::Grit => Command::new(grit_bin()),
            Self::Path(path) => Command::new(path),
        }
    }
}

/// A reusable command builder with deterministic Git author, committer, and
/// config environment.
#[derive(Clone, Debug)]
pub struct Cmd {
    program: Program,
    args: Vec<String>,
    dir: Option<PathBuf>,
    env: Vec<(OsString, Option<OsString>)>,
    stdin: Option<Vec<u8>>,
}

impl Cmd {
    /// Set the command working directory.
    ///
    /// `dir` is the repository or scratch directory where the command should
    /// run. The returned builder carries that directory into later execution.
    #[must_use]
    pub fn in_dir(mut self, dir: &Path) -> Self {
        self.dir = Some(dir.to_path_buf());
        self
    }

    /// Add or override an environment variable for this command.
    ///
    /// `key` and `value` are forwarded to `std::process::Command::env`. The
    /// returned builder preserves existing arguments and directory settings.
    #[must_use]
    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.env.push((
            key.as_ref().to_os_string(),
            Some(value.as_ref().to_os_string()),
        ));
        self
    }

    /// Remove an environment variable for this command.
    ///
    /// `key` is forwarded to `std::process::Command::env_remove`. The returned
    /// builder preserves existing arguments and directory settings.
    #[must_use]
    pub fn env_remove(mut self, key: impl AsRef<OsStr>) -> Self {
        self.env.push((key.as_ref().to_os_string(), None));
        self
    }

    /// Provide standard input bytes for this command.
    ///
    /// `stdin` is written to the child after it is spawned. The command's
    /// stdout and stderr are still captured in the returned [`Output`].
    #[must_use]
    pub fn stdin(mut self, stdin: impl AsRef<[u8]>) -> Self {
        self.stdin = Some(stdin.as_ref().to_vec());
        self
    }

    /// Run the command and return its captured output.
    ///
    /// Panics when no working directory was configured with [`Cmd::in_dir`],
    /// when the process cannot be spawned, or when writing configured stdin
    /// fails. Tests should use [`Cmd::suc`] or [`Cmd::with_status`] when the
    /// status code is part of the assertion.
    #[must_use]
    pub fn exec(&self) -> Output {
        let Some(dir) = self.dir.as_deref() else {
            panic!("Cmd: call .in_dir() before executing {:?}", self.args);
        };
        run_program(
            &self.program,
            &self.args,
            dir,
            self.stdin.as_deref(),
            &self.env,
        )
    }

    /// Run the command, assert exit status `0`, and return its output.
    ///
    /// The returned [`Output`] contains stdout and stderr for follow-up
    /// assertions. A non-zero exit status panics with a command dump.
    pub fn suc(&self) -> Output {
        let out = self.exec();
        assert!(
            out.ok(),
            "{}\n{}",
            self.program.display(),
            out.dump(&self.program.display())
        );
        out
    }

    /// Run the command, assert an exact exit status, and return its output.
    ///
    /// `status` is compared to the process status code. The method panics with
    /// stdout and stderr when the status differs.
    #[must_use]
    pub fn with_status(&self, status: i32) -> Output {
        let out = self.exec();
        assert_eq!(
            out.status,
            Some(status),
            "{}",
            out.dump(&self.program.display())
        );
        out
    }

    /// Cross-check this command against system `git`.
    ///
    /// The builder's arguments and directory are executed once with `grit` and
    /// once with `git`. The method asserts identical status and compatible
    /// stdout/stderr for success and failure cases. It panics on mismatches.
    pub fn check(&self) {
        let Some(dir) = self.dir.as_deref() else {
            panic!("Cmd: call .in_dir() before cross-checking {:?}", self.args);
        };

        let g = run_program(
            &Program::Grit,
            &self.args,
            dir,
            self.stdin.as_deref(),
            &self.env,
        );
        let r = run_program(
            &Program::Path(OsString::from("git")),
            &self.args,
            dir,
            self.stdin.as_deref(),
            &self.env,
        );

        let g_ok = g.ok();
        let r_ok = r.ok();

        assert_eq!(
            g.status, r.status,
            "exit status mismatch: grit={:?} git={:?}\n\
             args: {:?}  dir: {:?}\n\n\
             --- grit stdout ---\n{}\n\
             --- git  stdout ---\n{}\n",
            g.status, r.status, self.args, dir, g.stdout, r.stdout,
        );

        if g_ok && r_ok {
            assert_eq_nice!(
                g.stdout,
                r.stdout,
                "stdout mismatch (both exited 0)\n\
                 args: {:?}  dir: {:?}",
                self.args,
                dir,
            );
            if !g.stderr.is_empty() && !r.stderr.is_empty() {
                assert_eq_nice!(
                    g.stderr,
                    r.stderr,
                    "stderr mismatch (both exited 0)\n\
                     args: {:?}  dir: {:?}",
                    self.args,
                    dir,
                );
            }
        }

        if !g_ok && !r_ok {
            assert_eq_nice!(
                g.stderr,
                r.stderr,
                "stderr mismatch (both failed)\n\
                 args: {:?}  dir: {:?}",
                self.args,
                dir,
            );
        }
    }
}

/// Build a command that runs the workspace `grit` binary.
///
/// `args` are forwarded as command-line arguments. The returned builder must be
/// given a working directory before execution.
#[must_use]
pub fn grit_cmd(args: &[&str]) -> Cmd {
    Cmd {
        program: Program::Grit,
        args: args.iter().map(|s| (*s).to_owned()).collect(),
        dir: None,
        env: Vec::new(),
        stdin: None,
    }
}

/// Build a command that runs the system `git` binary.
///
/// `args` are forwarded as command-line arguments. The returned builder must be
/// given a working directory before execution.
#[must_use]
pub fn git_cmd(args: &[&str]) -> Cmd {
    Cmd {
        program: Program::Path(OsString::from("git")),
        args: args.iter().map(|s| (*s).to_owned()).collect(),
        dir: None,
        env: no_background_maintenance_env(),
        stdin: None,
    }
}

/// Config passed to every system `git` run by tests: no auto-gc and no background maintenance.
///
/// Newer Git starts detached maintenance after commits, which can repack and prune objects
/// while a test is still inspecting the repository (missing loose objects, MIDX entries naming
/// deleted packs). A test can still override these with its own `GIT_CONFIG_*` variables.
fn no_background_maintenance_env() -> Vec<(OsString, Option<OsString>)> {
    [
        ("GIT_CONFIG_COUNT", "2"),
        ("GIT_CONFIG_KEY_0", "gc.auto"),
        ("GIT_CONFIG_VALUE_0", "0"),
        ("GIT_CONFIG_KEY_1", "maintenance.auto"),
        ("GIT_CONFIG_VALUE_1", "false"),
    ]
    .into_iter()
    .map(|(k, v)| (OsString::from(k), Some(OsString::from(v))))
    .collect()
}

/// Run system `git`, assert success, and return stdout.
///
/// `dir` is the working directory and `args` are the command-line arguments.
/// The function panics on spawn failure, non-zero exit status, or invalid test
/// setup. Use [`git_cmd`] when stderr or non-zero statuses are relevant.
pub fn git(dir: &Path, args: &[&str]) -> String {
    git_cmd(args).in_dir(dir).suc().stdout
}

/// Run `grit` and return captured output without asserting success.
///
/// `dir` is the working directory and `args` are the command-line arguments.
/// The function panics on spawn failure or invalid test setup.
#[must_use]
pub fn grit(dir: &Path, args: &[&str]) -> Output {
    grit_cmd(args).in_dir(dir).exec()
}

/// Create a fresh, uniquely named temporary directory under the OS temp root.
///
/// `prefix` identifies the test family and `tag` identifies the case. The
/// returned path exists and any stale directory with the generated name has
/// already been removed.
#[must_use]
pub fn unique_tmp(prefix: &str, tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!("grit-{prefix}-{tag}-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).unwrap_or_else(|e| panic!("create temp dir {}: {e}", p.display()));
    p
}

/// Write a UTF-8 test file under a directory.
///
/// `dir` is the containing directory, `name` is the relative file name, and
/// `contents` are written as bytes. The function panics if the write fails.
pub fn write_file(dir: &Path, name: &str, contents: &str) {
    fs::write(dir.join(name), contents).unwrap_or_else(|e| panic!("write {name}: {e}"));
}

/// A cache for reusable filesystem fixtures.
///
/// The cache stores extracted or copied fixtures under a caller-owned root, then
/// materializes fresh tempdir copies for tests so individual tests can mutate
/// their repositories without changing the cached source.
#[derive(Clone, Debug)]
pub struct FixtureCache {
    root: PathBuf,
}

impl FixtureCache {
    /// Create a fixture cache rooted at `root`.
    ///
    /// The directory is created when missing. Returns an I/O error when the
    /// cache root cannot be created.
    pub fn new(root: impl AsRef<Path>) -> io::Result<Self> {
        fs::create_dir_all(root.as_ref())?;
        Ok(Self {
            root: root.as_ref().to_path_buf(),
        })
    }

    /// Create a fixture cache in a deterministic temp location.
    ///
    /// `prefix` is included in the cache directory name. Returns an I/O error
    /// when the cache root cannot be created.
    pub fn in_temp(prefix: &str) -> io::Result<Self> {
        let mut root = std::env::temp_dir();
        root.push(format!("grit-fixtures-{prefix}"));
        Self::new(root)
    }

    /// Return the cache root path.
    ///
    /// The returned path is the directory passed to [`FixtureCache::new`] or
    /// created by [`FixtureCache::in_temp`].
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Cache a fixture by recursively copying a source directory.
    ///
    /// `name` is the cache entry name and `source` is a directory to copy. The
    /// returned path points to the cached fixture. Existing cache entries are
    /// reused without recopying.
    pub fn cache_dir(&self, name: &str, source: impl AsRef<Path>) -> io::Result<PathBuf> {
        let cached = self.root.join(name);
        if cached.exists() {
            return Ok(cached);
        }

        let staging = self.root.join(format!("{name}.tmp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir_all(&staging)?;
        copy_dir_contents(source.as_ref(), &staging)?;
        fs::rename(&staging, &cached)?;
        Ok(cached)
    }

    /// Cache a fixture by extracting a tar archive with the system `tar`.
    ///
    /// `name` is the cache entry name and `archive` is the tar file to extract.
    /// The returned path points to the cached fixture. Existing cache entries
    /// are reused. Returns an I/O error when `tar` cannot run or exits non-zero.
    pub fn cache_tar(&self, name: &str, archive: impl AsRef<Path>) -> io::Result<PathBuf> {
        let cached = self.root.join(name);
        if cached.exists() {
            return Ok(cached);
        }

        let staging = self.root.join(format!("{name}.tmp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir_all(&staging)?;

        let out = Command::new("tar")
            .arg("-xf")
            .arg(archive.as_ref())
            .arg("-C")
            .arg(&staging)
            .output()?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(io::Error::other(format!(
                "extract {}: {stderr}",
                archive.as_ref().display()
            )));
        }

        fs::rename(&staging, &cached)?;
        Ok(cached)
    }

    /// Materialize a fresh tempdir copy of a cached fixture.
    ///
    /// `name` is the cache entry name. The returned tempdir owns an independent
    /// recursive copy of that fixture and is removed when dropped.
    pub fn materialize(&self, name: &str) -> io::Result<tempfile::TempDir> {
        let cached = self.root.join(name);
        let tempdir = tempfile::tempdir()?;
        copy_dir_contents(&cached, tempdir.path())?;
        Ok(tempdir)
    }
}

fn run_program(
    program: &Program,
    args: &[String],
    dir: &Path,
    stdin: Option<&[u8]>,
    env: &[(OsString, Option<OsString>)],
) -> Output {
    let mut cmd = program.command();
    cmd.args(args).current_dir(dir);
    configure_git_env(&mut cmd);
    for (key, value) in env {
        if let Some(value) = value {
            cmd.env(key, value);
        } else {
            cmd.env_remove(key);
        }
    }

    let out = if let Some(stdin) = stdin {
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {} {args:?}: {e}", program.display()));
        let Some(mut child_stdin) = child.stdin.take() else {
            panic!("open stdin for {} {args:?}", program.display());
        };
        child_stdin
            .write_all(stdin)
            .unwrap_or_else(|e| panic!("write stdin for {} {args:?}: {e}", program.display()));
        child
            .wait_with_output()
            .unwrap_or_else(|e| panic!("wait {} {args:?}: {e}", program.display()))
    } else {
        cmd.output()
            .unwrap_or_else(|e| panic!("spawn {} {args:?}: {e}", program.display()))
    };

    Output {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// Apply deterministic Git identity, date, and config isolation to a command.
///
/// `cmd` is the child command being prepared. The returned mutable reference is
/// the same command, allowing additional builder calls by the caller.
pub fn configure_git_env(cmd: &mut Command) -> &mut Command {
    cmd.env("GIT_AUTHOR_NAME", AUTHOR_NAME)
        .env("GIT_AUTHOR_EMAIL", AUTHOR_EMAIL)
        .env("GIT_COMMITTER_NAME", AUTHOR_NAME)
        .env("GIT_COMMITTER_EMAIL", AUTHOR_EMAIL)
        .env("GIT_AUTHOR_DATE", DETERMINISTIC_DATE)
        .env("GIT_COMMITTER_DATE", DETERMINISTIC_DATE)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
}

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn grit_bin() -> OsString {
    if let Some(path) = std::env::var_os("GRIT_BIN") {
        return path;
    }
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_grit") {
        return path;
    }
    let program = format!("grit{}", std::env::consts::EXE_SUFFIX);
    if let Ok(mut exe) = std::env::current_exe() {
        exe.pop();
        if exe.file_name().is_some_and(|name| name == "deps") {
            exe.pop();
        }
        let candidate = exe.join(&program);
        if candidate.is_file() {
            return candidate.into_os_string();
        }
    }
    OsString::from(format!("target/debug/{program}"))
}

fn copy_dir_contents(source: &Path, dest: &Path) -> io::Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path)?;

        if metadata.file_type().is_symlink() {
            copy_symlink(&source_path, &dest_path)?;
        } else if metadata.is_dir() {
            fs::create_dir_all(&dest_path)?;
            copy_dir_contents(&source_path, &dest_path)?;
        } else {
            fs::copy(&source_path, &dest_path)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn copy_symlink(source: &Path, dest: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(fs::read_link(source)?, dest)
}

#[cfg(windows)]
fn copy_symlink(source: &Path, dest: &Path) -> io::Result<()> {
    let target = fs::read_link(source)?;
    if source.is_dir() {
        std::os::windows::fs::symlink_dir(target, dest)
    } else {
        std::os::windows::fs::symlink_file(target, dest)
    }
}
