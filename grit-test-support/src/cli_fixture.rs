//! Deterministic CLI integration fixtures and insta snapshot helpers.

use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::{Command, Output as ProcessOutput};

use serde::Serialize;

pub const DETERMINISTIC_DATE: &str = "1700000000 +0000";

/// A temporary repository and isolated environment for CLI output snapshots.
pub struct CliFixture {
    repo: tempfile::TempDir,
    home: tempfile::TempDir,
    grit_bin: OsString,
}

impl Default for CliFixture {
    fn default() -> Self {
        Self::new()
    }
}

impl CliFixture {
    /// Build the standard snapshot repository: two commits on `main`, a `feature`
    /// branch, one modified, one staged, and one untracked file.
    pub fn new() -> Self {
        let grit_bin = grit_bin();
        let home = tempfile::tempdir().unwrap_or_else(|e| panic!("cli fixture home tempdir: {e}"));
        let repo = tempfile::tempdir().unwrap_or_else(|e| panic!("cli fixture repo tempdir: {e}"));
        let fixture = Self {
            repo,
            home,
            grit_bin,
        };
        fixture.bootstrap();
        fixture
    }

    /// Repository working tree root.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.repo.path()
    }

    /// Root path of the isolated config home (for insta path filters).
    #[must_use]
    pub fn home_root(&self) -> &Path {
        self.home.path()
    }

    /// Run `grit` in this fixture with the standard snapshot environment.
    fn run_grit(&self, args: &[&str]) -> crate::Output {
        let out = self.run_grit_process(args);
        crate::Output {
            status: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    fn run_grit_process(&self, args: &[&str]) -> ProcessOutput {
        let mut cmd = Command::new(&self.grit_bin);
        cmd.args(args).current_dir(self.path());
        self.apply_fixture_env(&mut cmd);
        cmd.output()
            .unwrap_or_else(|e| panic!("spawn grit {args:?}: {e}"))
    }

    fn apply_fixture_env(&self, cmd: &mut Command) {
        crate::configure_git_env(cmd);
        cmd.env("TZ", "UTC")
            .env("NO_COLOR", "1")
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path());
    }

    fn bootstrap(&self) {
        self.run_grit(&["init"]).expect_success("init");
        write_file(self.path(), "tracked.txt", "tracked v1\n");
        self.run_grit(&["add", "tracked.txt"])
            .expect_success("add tracked");
        self.run_grit(&["commit", "-m", "first"])
            .expect_success("commit first");

        self.run_grit(&["branch", "feature"])
            .expect_success("branch feature");

        write_file(self.path(), "tracked.txt", "tracked v2\n");
        write_file(self.path(), "second.txt", "second file\n");
        self.run_grit(&["add", "tracked.txt", "second.txt"])
            .expect_success("add second");
        self.run_grit(&["commit", "-m", "second"])
            .expect_success("commit second");

        write_file(self.path(), "tracked.txt", "tracked modified\n");
        write_file(self.path(), "staged.txt", "staged contents\n");
        self.run_grit(&["add", "staged.txt"])
            .expect_success("stage staged.txt");
        write_file(self.path(), "untracked.txt", "untracked\n");
    }

    /// Run `grit` in human, `--json`, and `--markdown` modes and assert an insta
    /// snapshot of stdout, stderr, and exit status for each mode.
    ///
    /// `snapshot_dir` is the directory for `.snap` files (typically
    /// `grit-cli/tests/snapshots` from an integration test).
    pub fn snapshot_all_modes(&self, snapshot_name: &str, args: &[&str], snapshot_dir: &Path) {
        let human = self.capture_mode(args, ModeKind::Human);
        let json = self.capture_mode(args, ModeKind::Json);
        let markdown = self.capture_mode(args, ModeKind::Markdown);

        let payload = AllModesSnapshot {
            human: &human,
            json: &json,
            markdown: &markdown,
        };

        let mut settings = insta::Settings::clone_current();
        register_cli_snapshot_filters(&mut settings, self);
        settings.set_snapshot_path(snapshot_dir);
        settings.bind(|| {
            insta::assert_json_snapshot!(snapshot_name, payload);
        });
    }

    fn capture_mode(&self, args: &[&str], kind: ModeKind) -> ModeCapture {
        let mut argv: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        match kind {
            ModeKind::Human => {}
            ModeKind::Json => argv.push("--json".into()),
            ModeKind::Markdown => argv.push("--markdown".into()),
        }
        let arg_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        let out = self.run_grit(&arg_refs);
        ModeCapture {
            exit: out.status.unwrap_or(-1),
            stdout: out.stdout,
            stderr: out.stderr,
        }
    }
}

trait ExpectSuccess {
    fn expect_success(self, label: &str) -> crate::Output;
}

impl ExpectSuccess for crate::Output {
    fn expect_success(self, label: &str) -> crate::Output {
        assert!(
            self.ok(),
            "grit {label} failed: exit={:?}\nstdout:\n{}\nstderr:\n{}",
            self.status,
            self.stdout,
            self.stderr
        );
        self
    }
}

#[derive(Clone, Copy)]
enum ModeKind {
    Human,
    Json,
    Markdown,
}

#[derive(Serialize)]
struct ModeCapture {
    exit: i32,
    stdout: String,
    stderr: String,
}

#[derive(Serialize)]
struct AllModesSnapshot<'a> {
    human: &'a ModeCapture,
    json: &'a ModeCapture,
    markdown: &'a ModeCapture,
}

/// Register shared insta redactions for CLI snapshots in `settings`.
pub fn register_cli_snapshot_filters(settings: &mut insta::Settings, fixture: &CliFixture) {
    settings.add_filter(
        r"(?i)\b\d+\s+(?:second|minute|hour|day|week|month|year)s?\s+ago\b",
        "[TIME_AGO]",
    );
    settings.add_filter(r"(?i)\bjust now\b", "[TIME_AGO]");
    settings.add_filter(r"\b\d+(?:\.\d+)?(?:ms|s|m)\b", "[DURATION]");

    let repo = regex::escape(fixture.path().to_string_lossy().as_ref());
    settings.add_filter(&repo, "[REPO]");

    let home = regex::escape(fixture.home_root().to_string_lossy().as_ref());
    if home != repo {
        settings.add_filter(&home, "[HOME]");
    }
}

/// Run `callback` with [`register_cli_snapshot_filters`] applied.
pub fn with_cli_snapshot_filters<F: FnOnce()>(fixture: &CliFixture, callback: F) {
    let mut settings = insta::Settings::clone_current();
    register_cli_snapshot_filters(&mut settings, fixture);
    settings.bind(callback);
}

fn write_file(dir: &Path, name: &str, contents: &str) {
    fs::write(dir.join(name), contents).unwrap_or_else(|e| panic!("write {name}: {e}"));
}

fn grit_bin() -> OsString {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_grit") {
        return path;
    }
    if let Some(path) = std::env::var_os("GRIT_BIN") {
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
