//! Remote daily scenarios: file headlines and grit-http matrix cases.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};

use grit_utils::daily::registry::{HEADLINE_SCENARIOS, REMOTE_MATRIX_SCENARIOS};
use grit_utils::fixture_registry::FixtureKind;
use tempfile::TempDir;

static DAILY_REMOTE_E2E_LOCK: Mutex<()> = Mutex::new(());

fn lock_e2e() -> MutexGuard<'static, ()> {
    std::env::remove_var("GRIT_BENCH_DAILY_ROOT");
    DAILY_REMOTE_E2E_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn bench_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_grit-bench"))
}

fn build_binaries() -> (PathBuf, PathBuf, PathBuf) {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let status = Command::new(env!("CARGO"))
        .args([
            "build",
            "-p",
            "grit-cli",
            "-p",
            "grit-http-server",
            "-p",
            "grit-utils",
            "--bin",
            "grit",
            "--bin",
            "grit-http-server",
            "-q",
        ])
        .current_dir(&workspace)
        .status()
        .expect("spawn cargo build");
    assert!(status.success(), "cargo build failed");
    let grit = workspace.join("target/debug/grit");
    let http = workspace.join("target/debug/grit-http-server");
    (
        grit.canonicalize().unwrap_or(grit),
        http.canonicalize().unwrap_or(http),
        bench_exe(),
    )
}

fn resolve_git() -> PathBuf {
    grit_utils::binary::resolve_binary("git", None).expect("git")
}

fn prepare_fixtures(git: &Path, kinds: &[FixtureKind]) {
    for kind in kinds {
        grit_utils::fixture_registry::ensure_cached(git, *kind, false)
            .unwrap_or_else(|e| panic!("prepare fixture {kind:?}: {e}"));
    }
}

fn run_daily(git: &Path, grit: &Path, output: &Path, extra: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(bench_exe());
    cmd.arg("daily")
        .arg("--smoke")
        .arg("--offline")
        .arg("--output")
        .arg(output)
        .arg("--git")
        .arg(git)
        .arg("--grit")
        .arg(grit);
    for arg in extra {
        cmd.arg(arg);
    }
    cmd.output().expect("spawn grit-bench daily")
}

const REMOTE_HEADLINE_IDS: [&str; 5] = [
    "fetch/incremental@git-git",
    "pull/fast-forward@git-git",
    "pull/rebase@synth-10k",
    "push/incremental@git-git",
    "clone/local@synth-10k",
];

const HTTP_MATRIX_IDS: [&str; 3] = [
    "clone/http@synth-10k",
    "fetch/incremental-http@git-git",
    "push/incremental-http@git-git",
];

const FETCH_MATRIX_IDS: [&str; 3] = [
    "fetch/one-branch@git-git",
    "fetch/dry-run@git-git",
    "pull/check@git-git",
];

#[test]
fn network_headlines_smoke() {
    let _guard = lock_e2e();
    let git = resolve_git();
    let (grit, _http, _bench) = build_binaries();
    let cache = TempDir::new().expect("cache");
    std::env::set_var("GRIT_BENCH_CACHE", cache.path());
    prepare_fixtures(&git, &[FixtureKind::GitGit, FixtureKind::Synth10kSmoke]);

    let out_path = cache.path().join("daily-remote-headlines.json");
    let only = REMOTE_HEADLINE_IDS.join(",");
    let out = run_daily(&git, &grit, &out_path, &["--only", &only]);
    assert!(
        out.status.success(),
        "daily remote headlines failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let report = grit_utils::validate_daily_report(&out_path).expect("validate");
    assert_eq!(report.scenarios.len(), REMOTE_HEADLINE_IDS.len());
    for id in REMOTE_HEADLINE_IDS {
        let row = report
            .scenarios
            .iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("missing {id}"));
        assert!(row.headline, "{id} headline");
        let spec = HEADLINE_SCENARIOS
            .iter()
            .find(|s| s.id == id)
            .expect("registry");
        assert_eq!(row.name.as_deref(), spec.name);
        assert!(!row.pending, "{id} pending");
        assert!(row.git.peak_rss_bytes > 0, "{id} git rss");
        assert!(row.grit.peak_rss_bytes > 0, "{id} grit rss");
    }
}

#[test]
fn fetch_refspec_matrix_smoke() {
    let _guard = lock_e2e();
    let git = resolve_git();
    let (grit, _http, _bench) = build_binaries();
    let cache = TempDir::new().expect("cache");
    std::env::set_var("GRIT_BENCH_CACHE", cache.path());
    prepare_fixtures(&git, &[FixtureKind::GitGit]);

    for id in FETCH_MATRIX_IDS {
        let daily_root = cache.path().join(id.replace('/', "_"));
        std::env::set_var("GRIT_BENCH_DAILY_ROOT", &daily_root);
        let out_path = cache.path().join(format!("daily-fetch-matrix-{id}.json"));
        let out = run_daily(&git, &grit, &out_path, &["--only", id]);
        assert!(
            out.status.success(),
            "daily fetch matrix {id} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let report = grit_utils::validate_daily_report(&out_path).expect("validate");
        assert_eq!(report.scenarios.len(), 1);
        let row = &report.scenarios[0];
        assert_eq!(row.id, id);
        assert!(!row.headline, "{id} not headline");
        assert!(REMOTE_MATRIX_SCENARIOS.iter().any(|s| s.id == id));
        assert!(!row.pending, "{id} pending");
        assert!(row.git.peak_rss_bytes > 0, "{id} git rss");
        assert!(row.grit.peak_rss_bytes > 0, "{id} grit rss");
    }
}

#[test]
fn http_cases_smoke() {
    let _guard = lock_e2e();
    let git = resolve_git();
    let (grit, _http, _bench) = build_binaries();
    let cache = TempDir::new().expect("cache");
    std::env::set_var("GRIT_BENCH_CACHE", cache.path());
    prepare_fixtures(&git, &[FixtureKind::GitGit, FixtureKind::Synth10kSmoke]);

    for id in HTTP_MATRIX_IDS {
        let daily_root = cache.path().join(id.replace('/', "_"));
        std::env::set_var("GRIT_BENCH_DAILY_ROOT", &daily_root);
        let out_path = cache.path().join(format!("daily-remote-http-{id}.json"));
        let out = run_daily(&git, &grit, &out_path, &["--only", id]);
        assert!(
            out.status.success(),
            "daily http matrix {id} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let report = grit_utils::validate_daily_report(&out_path).expect("validate");
        assert_eq!(report.scenarios.len(), 1);
        let row = &report.scenarios[0];
        assert_eq!(row.id, id);
        assert!(!row.headline, "{id} not headline");
        assert!(REMOTE_MATRIX_SCENARIOS.iter().any(|s| s.id == id));
        assert!(!row.pending, "{id} pending");
        assert!(row.git.peak_rss_bytes > 0, "{id} git rss");
        assert!(row.grit.peak_rss_bytes > 0, "{id} grit rss");
    }
}
