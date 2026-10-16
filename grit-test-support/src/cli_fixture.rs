//! Deterministic CLI integration fixtures and insta snapshot helpers.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output as ProcessOutput};

use serde::de::Deserialize;
use serde::Serialize;

use crate::git_fsck;

/// Fixed author/committer timestamp for deterministic commits in CLI fixtures.
pub const DETERMINISTIC_DATE: &str = "1700000000 +0000";

const PROGRESS_STDOUT_FORBIDDEN: &[&str] = &[
    "Receiving objects",
    "Cloning into '",
    "Enumerating objects",
    "Counting objects",
    "Compressing objects",
    "Writing objects",
    "Resolving deltas",
];

/// Expected process exit code for each CLI output mode in a snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotExitCodes {
    /// Exit code for default (human) output.
    pub human: i32,
    /// Exit code with `--json`.
    pub json: i32,
    /// Exit code with `--markdown`.
    pub markdown: i32,
}

impl SnapshotExitCodes {
    /// Exit code `0` for human, JSON, and Markdown.
    pub const SUCCESS: Self = Self {
        human: 0,
        json: 0,
        markdown: 0,
    };

    /// The same exit code for all three modes.
    #[must_use]
    pub const fn all(code: i32) -> Self {
        Self {
            human: code,
            json: code,
            markdown: code,
        }
    }
}

/// A temporary repository and isolated environment for CLI output snapshots.
pub struct CliFixture {
    repo: tempfile::TempDir,
    home: tempfile::TempDir,
    grit_bin: OsString,
}

/// [`CliFixture`] history published to a bare upstream with system `git`.
pub struct CliFixtureWithUpstream {
    fixture: CliFixture,
    upstream: tempfile::TempDir,
    workspace: tempfile::TempDir,
}

/// Options for network command snapshots.
#[derive(Clone, Copy, Default)]
pub struct NetworkSnapshotOptions {
    /// Run `git fsck --strict` on the bare upstream after the command.
    pub fsck_upstream: bool,
}

impl Default for CliFixture {
    fn default() -> Self {
        Self::new()
    }
}

impl CliFixture {
    /// Temporary directory and config home without a Git repository.
    pub fn workspace() -> Self {
        let grit_bin = grit_bin();
        let home = tempfile::tempdir().unwrap_or_else(|e| panic!("cli fixture home tempdir: {e}"));
        let repo = tempfile::tempdir().unwrap_or_else(|e| panic!("cli fixture repo tempdir: {e}"));
        Self {
            repo,
            home,
            grit_bin,
        }
    }

    /// Empty initialized repository with the same isolated environment as [`Self::new`].
    pub fn empty() -> Self {
        let fixture = Self::workspace();
        fixture.run_grit(&["init"]).expect_success("init");
        fixture
    }

    /// Build the standard snapshot repository: two commits on `main`, a `feature`
    /// branch, one modified, one staged, and one untracked file.
    pub fn new() -> Self {
        let fixture = Self::empty();
        fixture.bootstrap();
        fixture
    }

    /// Publish this fixture to a bare upstream using system `git`, then verify both
    /// repositories with `git fsck --strict`.
    #[must_use]
    pub fn with_upstream(self) -> CliFixtureWithUpstream {
        let upstream = tempfile::tempdir().unwrap_or_else(|e| panic!("upstream tempdir: {e}"));
        let workspace = tempfile::tempdir().unwrap_or_else(|e| panic!("workspace tempdir: {e}"));
        let upstream_path = upstream.path();
        crate::git(upstream_path, &["init", "--bare", "-b", "main"]);

        let seed = self.path();
        crate::git(seed, &["checkout", "main"]);
        let upstream_utf8 = upstream_path.to_str().unwrap_or_else(|| {
            panic!(
                "upstream path must be valid UTF-8: {}",
                upstream_path.display()
            )
        });
        crate::git(seed, &["remote", "add", "origin", upstream_utf8]);
        crate::git(seed, &["push", "-u", "origin", "main"]);
        crate::git(seed, &["push", "origin", "feature"]);
        crate::git(seed, &["tag", "-a", "v0.1", "-m", "snapshot-tag"]);
        crate::git(seed, &["push", "origin", "v0.1"]);

        let seed_fsck = git_fsck(seed, true);
        assert!(
            seed_fsck.ok,
            "git fsck seed after push: {:?}",
            seed_fsck.msg_ids
        );
        let upstream_fsck = git_fsck(upstream_path, true);
        assert!(
            upstream_fsck.ok,
            "git fsck upstream after push: {:?}",
            upstream_fsck.msg_ids
        );

        CliFixtureWithUpstream {
            fixture: self,
            upstream,
            workspace,
        }
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
    pub fn run(&self, args: &[&str]) -> crate::Output {
        self.run_grit(args)
    }

    /// Run `grit` and panic when the process exits non-zero.
    pub fn run_ok(&self, args: &[&str]) -> crate::Output {
        let label = args.join(" ");
        self.run(args).expect_success(&label)
    }

    /// Write a repository-relative file for scenario setup.
    pub fn write_repo_file(&self, name: &str, contents: &str) {
        write_file(self.path(), name, contents);
    }

    fn run_grit(&self, args: &[&str]) -> crate::Output {
        self.run_grit_in(self.path(), args)
    }

    fn run_grit_in(&self, cwd: &Path, args: &[&str]) -> crate::Output {
        let out = self.run_grit_process(cwd, args);
        crate::Output {
            status: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    fn run_grit_process(&self, cwd: &Path, args: &[&str]) -> ProcessOutput {
        let mut cmd = Command::new(&self.grit_bin);
        cmd.args(args).current_dir(cwd);
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
    /// Each mode runs against a fresh copy of the repository as it existed immediately
    /// before this call (after any setup the test performed on this fixture).
    ///
    /// `snapshot_dir` is the directory for `.snap` files (typically
    /// `grit-cli/tests/snapshots` from an integration test).
    pub fn snapshot_all_modes(&self, snapshot_name: &str, args: &[&str], snapshot_dir: &Path) {
        self.snapshot_all_modes_exits(
            snapshot_name,
            args,
            snapshot_dir,
            SnapshotExitCodes::SUCCESS,
        );
    }

    /// Like [`Self::snapshot_all_modes`], but require the given exit code in all three modes.
    pub fn snapshot_all_modes_expect(
        &self,
        snapshot_name: &str,
        args: &[&str],
        snapshot_dir: &Path,
        expected_exit: i32,
    ) {
        self.snapshot_all_modes_exits(
            snapshot_name,
            args,
            snapshot_dir,
            SnapshotExitCodes::all(expected_exit),
        );
    }

    /// Like [`Self::snapshot_all_modes`], with per-mode exit expectations.
    pub fn snapshot_all_modes_exits(
        &self,
        snapshot_name: &str,
        args: &[&str],
        snapshot_dir: &Path,
        expected: SnapshotExitCodes,
    ) {
        let baseline = self.save_repo_baseline();

        let human = self.capture_mode_from_baseline(&baseline, args, ModeKind::Human);
        let json = self.capture_mode_from_baseline(&baseline, args, ModeKind::Json);
        let markdown = self.capture_mode_from_baseline(&baseline, args, ModeKind::Markdown);

        assert_eq!(
            human.exit, expected.human,
            "human exit for {snapshot_name} {args:?}"
        );
        assert_eq!(
            json.exit, expected.json,
            "json exit for {snapshot_name} {args:?}"
        );
        assert_eq!(
            markdown.exit, expected.markdown,
            "markdown exit for {snapshot_name} {args:?}"
        );

        Self::assert_all_modes_snapshot(snapshot_name, snapshot_dir, self, human, json, markdown);
    }

    fn save_repo_baseline(&self) -> tempfile::TempDir {
        let backup = tempfile::tempdir().unwrap_or_else(|e| panic!("cli fixture baseline: {e}"));
        copy_dir_all(self.path(), backup.path())
            .unwrap_or_else(|e| panic!("copy repo baseline from {}: {e}", self.path().display()));
        backup
    }

    fn capture_mode_from_baseline(
        &self,
        baseline: &tempfile::TempDir,
        args: &[&str],
        kind: ModeKind,
    ) -> ModeCapture {
        clear_dir(self.path()).unwrap_or_else(|e| {
            panic!(
                "clear repo {} before snapshot mode: {e}",
                self.path().display()
            )
        });
        copy_dir_all(baseline.path(), self.path())
            .unwrap_or_else(|e| panic!("restore repo baseline to {}: {e}", self.path().display()));
        self.capture_mode(args, kind)
    }

    pub fn snapshot_fresh_all_modes(
        snapshot_name: &str,
        args: &[&str],
        snapshot_dir: &Path,
        fresh: impl Fn() -> Self,
    ) {
        let human_fixture = fresh();
        let human = human_fixture.capture_mode(args, ModeKind::Human);
        let json_fixture = fresh();
        let json = json_fixture.capture_mode(args, ModeKind::Json);
        let markdown_fixture = fresh();
        let markdown = markdown_fixture.capture_mode(args, ModeKind::Markdown);
        Self::assert_all_modes_snapshot_with_fixtures(
            snapshot_name,
            snapshot_dir,
            &[&human_fixture, &json_fixture, &markdown_fixture],
            human,
            json,
            markdown,
        );
    }

    /// Assert a three-mode snapshot (human / JSON / Markdown) with shared redactions.
    pub fn assert_all_modes_snapshot(
        snapshot_name: &str,
        snapshot_dir: &Path,
        filter_fixture: &Self,
        human: ModeCapture,
        json: ModeCapture,
        markdown: ModeCapture,
    ) {
        assert_single_json_value(&json.stdout);
        assert_no_progress_in_json_stdout(&json.stdout);
        assert_progress_not_on_stdout(&human);

        Self::assert_all_modes_snapshot_with_fixtures(
            snapshot_name,
            snapshot_dir,
            &[filter_fixture],
            human,
            json,
            markdown,
        );
    }

    fn assert_all_modes_snapshot_with_fixtures(
        snapshot_name: &str,
        snapshot_dir: &Path,
        filter_fixtures: &[&Self],
        human: ModeCapture,
        json: ModeCapture,
        markdown: ModeCapture,
    ) {
        let payload = AllModesSnapshot {
            human: &human,
            json: &json,
            markdown: &markdown,
        };
        let mut settings = insta::Settings::clone_current();
        register_cli_snapshot_filters_for(&mut settings, filter_fixtures);
        settings.set_snapshot_path(snapshot_dir);
        settings.bind(|| {
            insta::assert_json_snapshot!(snapshot_name, payload);
        });
    }

    /// Assert an insta snapshot of pre-rendered human, JSON, and Markdown output
    /// (for outcomes that cannot be exercised through the CLI in tests).
    pub fn snapshot_rendered_modes(
        snapshot_name: &str,
        snapshot_dir: &Path,
        human_stdout: &str,
        json_stdout: &str,
        markdown_stdout: &str,
    ) {
        let human = ModeCapture {
            exit: 0,
            stdout: human_stdout.to_owned(),
            stderr: String::new(),
        };
        let json = ModeCapture {
            exit: 0,
            stdout: json_stdout.to_owned(),
            stderr: String::new(),
        };
        let markdown = ModeCapture {
            exit: 0,
            stdout: markdown_stdout.to_owned(),
            stderr: String::new(),
        };
        let payload = AllModesSnapshot {
            human: &human,
            json: &json,
            markdown: &markdown,
        };
        let mut settings = insta::Settings::clone_current();
        settings.set_snapshot_path(snapshot_dir);
        settings.bind(|| {
            insta::assert_json_snapshot!(snapshot_name, payload);
        });
    }

    fn capture_mode(&self, args: &[&str], kind: ModeKind) -> ModeCapture {
        self.capture_mode_in(self.path(), args, kind)
    }

    fn capture_mode_in(&self, cwd: &Path, args: &[&str], kind: ModeKind) -> ModeCapture {
        let mut argv: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        match kind {
            ModeKind::Human => {}
            ModeKind::Json => argv.push("--json".into()),
            ModeKind::Markdown => argv.push("--markdown".into()),
        }
        let arg_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        let out = self.run_grit_in(cwd, &arg_refs);
        ModeCapture {
            exit: out.status.unwrap_or(-1),
            stdout: out.stdout,
            stderr: out.stderr,
        }
    }
}

impl CliFixtureWithUpstream {
    /// Bare upstream repository path (`git init --bare`).
    #[must_use]
    pub fn upstream_path(&self) -> &Path {
        self.upstream.path()
    }

    /// Scratch directory for clones and client repositories.
    #[must_use]
    pub fn workspace_path(&self) -> &Path {
        self.workspace.path()
    }

    /// Access the original grit-built seed repository.
    #[must_use]
    pub fn seed(&self) -> &CliFixture {
        &self.fixture
    }

    /// Clone the upstream into `workspace/<name>` with system `git` and return the path.
    #[must_use]
    pub fn git_clone_client(&self, name: &str) -> PathBuf {
        let dest = self.workspace.path().join(name);
        if dest.exists() {
            fs::remove_dir_all(&dest).unwrap_or_else(|e| panic!("remove {name}: {e}"));
        }
        let upstream_utf8 = self.upstream_path().to_str().unwrap_or_else(|| {
            panic!(
                "upstream path must be valid UTF-8: {}",
                self.upstream_path().display()
            )
        });
        crate::git(self.workspace.path(), &["clone", "-q", upstream_utf8, name]);
        dest
    }

    /// Snapshot a network command with fresh repository state for each output mode.
    ///
    /// `prepare` runs once per mode so human, `--json`, and `--markdown` each exercise
    /// the same scenario from an equivalent starting point (including a fresh upstream
    /// when the command mutates it).
    pub fn snapshot_network<F>(
        &self,
        snapshot_name: &str,
        snapshot_dir: &Path,
        options: NetworkSnapshotOptions,
        prepare: F,
    ) where
        F: Fn(SnapshotOutputMode) -> PreparedNetworkRun,
    {
        let human_run = prepare(SnapshotOutputMode::Human);
        let human = {
            let cap = self.capture_prepared(&human_run, ModeKind::Human);
            if options.fsck_upstream {
                fsck_upstream_strict(&human_run.upstream, "human");
            }
            cap
        };
        let json_run = prepare(SnapshotOutputMode::Json);
        let json = {
            let cap = self.capture_prepared(&json_run, ModeKind::Json);
            assert_single_json_value(&cap.stdout);
            assert_no_progress_in_json_stdout(&cap.stdout);
            if options.fsck_upstream {
                fsck_upstream_strict(&json_run.upstream, "json");
            }
            cap
        };
        let markdown_run = prepare(SnapshotOutputMode::Markdown);
        let markdown = {
            let cap = self.capture_prepared(&markdown_run, ModeKind::Markdown);
            if options.fsck_upstream {
                fsck_upstream_strict(&markdown_run.upstream, "markdown");
            }
            cap
        };

        assert_progress_not_on_stdout(&human);

        let payload = AllModesSnapshot {
            human: &human,
            json: &json,
            markdown: &markdown,
        };

        let mut settings = insta::Settings::clone_current();
        let redact_paths = collect_network_redact_paths([&human_run, &json_run, &markdown_run]);
        let upstream = human_run.upstream.as_path();
        register_network_snapshot_filters(&mut settings, &self.fixture, upstream, &redact_paths);
        settings.set_snapshot_path(snapshot_dir);
        settings.bind(|| {
            insta::assert_json_snapshot!(snapshot_name, payload);
        });
    }

    fn capture_prepared(&self, run: &PreparedNetworkRun, kind: ModeKind) -> ModeCapture {
        let arg_refs: Vec<&str> = run.args.iter().map(String::as_str).collect();
        self.fixture.capture_mode_in(&run.cwd, &arg_refs, kind)
    }
}

/// Output mode passed to network snapshot `prepare` callbacks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotOutputMode {
    /// Default human-readable output.
    Human,
    /// `--json` output.
    Json,
    /// `--markdown` output.
    Markdown,
}

impl SnapshotOutputMode {
    /// Stable suffix for per-mode temporary directory names.
    #[must_use]
    pub fn suffix(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Json => "json",
            Self::Markdown => "markdown",
        }
    }
}

/// One isolated network snapshot run (keeps tempdirs alive until the snapshot completes).
pub struct PreparedNetworkRun {
    #[allow(dead_code)]
    keepalive: Option<Box<CliFixtureWithUpstream>>,
    /// Working directory for `grit`.
    pub cwd: PathBuf,
    /// Arguments for `grit` (mode flags are added by the harness).
    pub args: Vec<String>,
    /// Bare upstream used for fsck and path redaction.
    pub upstream: PathBuf,
}

impl PreparedNetworkRun {
    /// Build a run that reuses an existing upstream fixture (fresh client/worktree only).
    #[must_use]
    pub fn from_shared_upstream(
        net: &CliFixtureWithUpstream,
        cwd: PathBuf,
        args: Vec<String>,
    ) -> Self {
        Self {
            keepalive: None,
            cwd,
            args,
            upstream: net.upstream_path().to_path_buf(),
        }
    }

    /// Build a run that owns a freshly created upstream fixture.
    #[must_use]
    pub fn from_owned_upstream(
        net: Box<CliFixtureWithUpstream>,
        cwd: PathBuf,
        args: Vec<String>,
    ) -> Self {
        let upstream = net.upstream_path().to_path_buf();
        Self {
            keepalive: Some(net),
            cwd,
            args,
            upstream,
        }
    }
}

fn fsck_upstream_strict(upstream: &Path, mode: &str) {
    let fsck = git_fsck(upstream, true);
    assert!(
        fsck.ok,
        "git fsck --strict upstream after push ({mode}): {:?}",
        fsck.msg_ids
    );
}

fn collect_network_redact_paths(runs: [&PreparedNetworkRun; 3]) -> Vec<&Path> {
    let mut paths: Vec<&Path> = Vec::new();
    for run in runs {
        paths.push(run.cwd.as_path());
        paths.push(run.upstream.as_path());
    }
    paths.sort_by_key(|p| p.as_os_str());
    paths.dedup_by_key(|p| p.as_os_str());
    paths
}

trait ExpectSuccess {
    fn expect_success(self, label: &str) -> crate::Output;
}

impl ExpectSuccess for crate::Output {
    fn expect_success(self, label: &str) -> Self {
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

/// One captured CLI invocation (stdout, stderr, exit code).
#[derive(Serialize)]
pub struct ModeCapture {
    pub exit: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Serialize)]
struct AllModesSnapshot<'a> {
    human: &'a ModeCapture,
    json: &'a ModeCapture,
    markdown: &'a ModeCapture,
}

/// Register shared insta redactions for CLI snapshots in `settings`.
pub fn register_cli_snapshot_filters(settings: &mut insta::Settings, fixture: &CliFixture) {
    register_cli_snapshot_filters_for(settings, &[fixture]);
}

/// Like [`register_cli_snapshot_filters`], but redact every fixture path (for
/// `snapshot_fresh_all_modes`, where human/json/markdown use separate repos).
pub fn register_cli_snapshot_filters_for(settings: &mut insta::Settings, fixtures: &[&CliFixture]) {
    settings.add_filter(
        r"(?i)\b\d+\s+(?:second|minute|hour|day|week|month|year)s?\s+ago\b",
        "[TIME_AGO]",
    );
    settings.add_filter(r"(?i)\bjust now\b", "[TIME_AGO]");
    settings.add_filter(r"\b\d+(?:\.\d+)?\s*(?:ms|s|m)\b", "[DURATION]");
    settings.add_filter(r#"\\"duration_ms\\": ?\d+"#, r#"\"duration_ms\": 0"#);
    settings.add_filter(r#"`\{\\"duration_ms\\": ?\d+"#, r#"`{\"duration_ms\": 0"#);
    settings.add_filter(r#"\{\\"duration_ms\\": ?\d+"#, r#"{\"duration_ms\": 0"#);
    settings.add_filter(r"pack-[0-9a-f]{20,}\.pack", "pack-[HASH].pack");
    settings.add_filter(r"grit \d+\.\d+\.\d+", "grit [VERSION]");
    settings.add_filter(r#""version": "\d+\.\d+\.\d+""#, r#""version": "[VERSION]""#);
    settings.add_filter(
        r#""version\\": \\"\\d+\\.\\d+\\.\\d+\\""#,
        r#""version\": \"[VERSION]\""#,
    );

    for fixture in fixtures {
        redact_path(settings, fixture.path(), "[REPO]");
        redact_path(settings, fixture.home_root(), "[HOME]");
    }
}

/// Insta redactions for network CLI snapshots (transfer stats and temp paths).
pub fn register_network_snapshot_filters(
    settings: &mut insta::Settings,
    fixture: &CliFixture,
    upstream: &Path,
    extra_paths: &[&Path],
) {
    register_cli_snapshot_filters(settings, fixture);
    redact_path(settings, upstream, "[UPSTREAM]");
    for path in extra_paths {
        redact_path(settings, path, "[WORKTREE]");
    }

    settings.add_filter(r#""bytes": \d+"#, r#""bytes": "[BYTES]""#);
    settings.add_filter(r" \([\d.]+ (?:KiB|MiB|B)\)", " ([SIZE])");
    settings.add_filter(r" in [\d.]+s\.", " in [ELAPSED].");
    settings.add_filter(r" \([\d.]+s\)", " ([ELAPSED])");
    // Clone summary: throughput, per-phase timings and the time bar vary run to run.
    settings.add_filter(r"[\d.]+ (?:B|KiB|MiB|GiB)/s", "[RATE]");
    settings.add_filter(r#"\\"seconds\\": [\d.e-]+"#, r#"\"seconds\": 0"#);
    settings.add_filter(r"\| [\d.]+ \|\\n", "| [SECONDS] |\\n");
    settings.add_filter(r"(  time {6})[^\\]*", "$1[TIME BAR]");
    settings.add_filter(r"(\\n {12})[^\\]*", "$1[TIMELINE]");
}

/// Run `callback` with [`register_cli_snapshot_filters`] applied.
pub fn with_cli_snapshot_filters<F: FnOnce()>(fixture: &CliFixture, callback: F) {
    let mut settings = insta::Settings::clone_current();
    register_cli_snapshot_filters(&mut settings, fixture);
    settings.bind(callback);
}

fn redact_path(settings: &mut insta::Settings, path: &Path, label: &str) {
    let escaped = regex::escape(path.to_string_lossy().as_ref());
    if !escaped.is_empty() {
        settings.add_filter(&escaped, label);
    }
}

fn assert_single_json_value(stdout: &str) {
    let trimmed = stdout.trim();
    assert!(
        !trimmed.is_empty(),
        "expected JSON stdout, got empty output"
    );
    let mut de = serde_json::Deserializer::from_str(trimmed);
    match serde_json::Value::deserialize(&mut de) {
        Ok(_value) => {}
        Err(err) => panic!("JSON stdout must parse: {err}"),
    }
    if let Err(err) = de.end() {
        panic!("JSON stdout must be exactly one value: {err}");
    }
}

fn assert_no_progress_in_json_stdout(stdout: &str) {
    for marker in PROGRESS_STDOUT_FORBIDDEN {
        assert!(
            !stdout.contains(marker),
            "progress marker {marker:?} must not appear in JSON stdout:\n{stdout}"
        );
    }
}

fn assert_progress_not_on_stdout(human: &ModeCapture) {
    for marker in PROGRESS_STDOUT_FORBIDDEN {
        assert!(
            !human.stdout.contains(marker),
            "progress marker {marker:?} must go to stderr, not stdout:\n{}",
            human.stdout
        );
    }
    if human.stderr.contains("Cloning into '") {
        assert!(
            !human.stdout.contains("Cloning into '"),
            "clone progress must stay on stderr"
        );
    }
}

fn write_file(dir: &Path, name: &str, contents: &str) {
    fs::write(dir.join(name), contents).unwrap_or_else(|e| panic!("write {name}: {e}"));
}

fn clear_dir(dir: &Path) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(path)?;
        } else {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn copy_dir_all(from: &Path, to: &Path) -> std::io::Result<()> {
    if !to.exists() {
        fs::create_dir_all(to)?;
    }
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let dest = to.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_all(&entry.path(), &dest)?;
        } else if file_type.is_symlink() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::symlink;
                let target = fs::read_link(entry.path())?;
                symlink(target, dest)?;
            }
            #[cfg(not(unix))]
            {
                fs::copy(entry.path(), dest)?;
            }
        } else {
            fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
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
