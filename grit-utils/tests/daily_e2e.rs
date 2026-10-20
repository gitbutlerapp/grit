//! End-to-end tests for `grit-bench daily`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};

use grit_utils::daily::registry::HEADLINE_SCENARIOS;
use grit_utils::fixture_registry::FixtureKind;
use tempfile::TempDir;

static DAILY_E2E_LOCK: Mutex<()> = Mutex::new(());

fn lock_e2e() -> MutexGuard<'static, ()> {
    std::env::remove_var("GRIT_BENCH_DAILY_ROOT");
    DAILY_E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn bench_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_grit-bench"))
}

fn build_grit_executable() -> PathBuf {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "grit-cli", "--bin", "grit", "-q"])
        .current_dir(&workspace)
        .status()
        .expect("spawn cargo build grit-cli");
    assert!(status.success(), "cargo build grit-cli failed");
    let grit = workspace.join("target/debug/grit");
    grit.canonicalize().unwrap_or(grit)
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

fn run_daily_smoke(git: &Path, grit: &Path, output: &Path, extra: &[&str]) -> std::process::Output {
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

const LOCAL_HEADLINE_IDS: [&str; 13] = [
    "status/clean@synth-10k",
    "status/dirty@synth-10k",
    "add/modified@synth-10k",
    "commit/small@synth-10k",
    "diff/worktree@synth-10k",
    "log/oneline-100@git-git",
    "show/head@git-git",
    "blame/file@git-git",
    "switch/branch@synth-10k",
    "branch/list@many-refs",
    "merge/clean@synth-10k",
    "pick/one@synth-10k",
    "stash/push-pop@synth-10k",
];

#[test]
fn smoke_report_validates() {
    let _guard = lock_e2e();
    let git = resolve_git();
    let grit = build_grit_executable();
    let cache = TempDir::new().expect("cache");
    std::env::set_var("GRIT_BENCH_CACHE", cache.path());
    prepare_fixtures(
        &git,
        &[
            FixtureKind::Synth10kSmoke,
            FixtureKind::GitGitWt,
            FixtureKind::ManyRefsSmoke,
        ],
    );

    let out_path = cache.path().join("daily-smoke.json");
    let only = LOCAL_HEADLINE_IDS.join(",");
    let out = run_daily_smoke(&git, &grit, &out_path, &["--only", &only]);
    assert!(
        out.status.success(),
        "daily run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let report = grit_utils::validate_daily_report(&out_path).expect("validate smoke report");
    assert_eq!(report.scenarios.len(), LOCAL_HEADLINE_IDS.len());
    for id in LOCAL_HEADLINE_IDS {
        let row = report
            .scenarios
            .iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("missing headline {id}"));
        assert!(row.headline, "{id} must be headline");
        let spec = HEADLINE_SCENARIOS
            .iter()
            .find(|s| s.id == id)
            .expect("registry spec");
        assert_eq!(row.name.as_deref(), spec.name);
        assert!(row.git.peak_rss_bytes > 0, "{id} git peak rss");
        assert!(row.grit.peak_rss_bytes > 0, "{id} grit peak rss");
    }
}

#[test]
fn only_filter_selects_ids() {
    let _guard = lock_e2e();
    let git = resolve_git();
    let grit = build_grit_executable();
    let cache = TempDir::new().expect("cache");
    std::env::set_var("GRIT_BENCH_CACHE", cache.path());
    prepare_fixtures(&git, &[FixtureKind::Synth10kSmoke]);

    let out_path = cache.path().join("daily-only.json");
    let out = run_daily_smoke(
        &git,
        &grit,
        &out_path,
        &["--only", "status/clean@synth-10k"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report = grit_utils::validate_daily_report(&out_path).expect("validate");
    assert_eq!(report.scenarios.len(), 1);
    assert_eq!(report.scenarios[0].id, "status/clean@synth-10k");
}

#[test]
fn area_filter_selects_area() {
    let _guard = lock_e2e();
    let git = resolve_git();
    let grit = build_grit_executable();
    let cache = TempDir::new().expect("cache");
    std::env::set_var("GRIT_BENCH_CACHE", cache.path());
    prepare_fixtures(&git, &[FixtureKind::Synth10kSmoke]);

    let out_path = cache.path().join("daily-area.json");
    let out = run_daily_smoke(&git, &grit, &out_path, &["--area", "worktree,diff"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report = grit_utils::validate_daily_report(&out_path).expect("validate");
    assert!(report.scenarios.len() >= 5);
    assert!(report.scenarios.iter().all(|s| {
        matches!(
            s.area,
            grit_utils::daily::registry::Area::Worktree | grit_utils::daily::registry::Area::Diff
        )
    }));
}

/// Matrix ids for step 7 (worktree + history areas in smoke).
const MATRIX_SMOKE_IDS: [&str; 35] = [
    "status/clean@synth-100k",
    "status/clean@deep-tree",
    "status/clean@small",
    "status/dirty@synth-100k",
    "status/dirty@deep-tree",
    "status/dirty@small",
    "add/all@synth-100k",
    "add/modified@big-blobs",
    "commit/small@synth-100k",
    "commit/small@small",
    "restore/paths@synth-10k",
    "reset/hard@synth-10k",
    "reset/mixed@synth-10k",
    "rm/dir@synth-10k",
    "mv/file@synth-10k",
    "clean/untracked@synth-10k",
    "init/empty@small",
    "diff/worktree@synth-100k",
    "diff/worktree@deep-tree",
    "diff/worktree@big-blobs",
    "diff/staged@synth-100k",
    "diff/staged@deep-tree",
    "log/oneline-100@synth-100k",
    "log/all@git-git",
    "log/path@git-git",
    "show/head@synth-10k",
    "show/head@big-blobs",
    "blame/file@deep-tree",
    "revs/count@git-git",
    "merge-base/branches@git-git",
    "resolve/head-n@git-git",
    "cat/blob@git-git",
    "reflog/list@synth-10k",
    "shortlog/ahead@git-git",
    "patch-id/commit@git-git",
];

const MATRIX_GAP_IDS: [&str; 1] = ["log/author@git-git"];

#[test]
fn worktree_history_areas_smoke() {
    let _guard = lock_e2e();
    let git = resolve_git();
    let grit = build_grit_executable();
    let cache = TempDir::new().expect("cache");
    std::env::set_var("GRIT_BENCH_CACHE", cache.path());
    prepare_fixtures(
        &git,
        &[
            FixtureKind::Synth10kSmoke,
            FixtureKind::Synth100kSmoke,
            FixtureKind::SmallSmoke,
            FixtureKind::DeepTreeSmoke,
            FixtureKind::BigBlobsSmoke,
            FixtureKind::GitGitWt,
        ],
    );

    let out_path = cache.path().join("daily-matrix-smoke.json");
    let out = run_daily_smoke(
        &git,
        &grit,
        &out_path,
        &["--area", "worktree,diff,revwalk,blame,index"],
    );
    assert!(
        out.status.success(),
        "daily matrix smoke failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let report = grit_utils::validate_daily_report(&out_path).expect("validate matrix smoke");
    for id in MATRIX_SMOKE_IDS {
        let row = report
            .scenarios
            .iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("missing matrix scenario {id}"));
        assert!(!row.headline, "{id} is matrix not headline");
        assert!(row.git.peak_rss_bytes > 0, "{id} git peak rss");
        assert!(row.grit.peak_rss_bytes > 0, "{id} grit peak rss");
    }
    for id in MATRIX_GAP_IDS {
        let row = report
            .scenarios
            .iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("missing gap scenario {id}"));
        assert!(
            row.pending,
            "{id} must stay pending until grit log --author exists"
        );
    }
}

#[test]
fn baseline_grit_records_headlines() {
    let _guard = lock_e2e();
    let git = resolve_git();
    let grit = build_grit_executable();
    let cache = TempDir::new().expect("cache");
    std::env::set_var("GRIT_BENCH_CACHE", cache.path());
    prepare_fixtures(&git, &[FixtureKind::Synth10kSmoke]);

    let out_path = cache.path().join("daily-baseline.json");
    let mut cmd = Command::new(bench_exe());
    cmd.arg("daily")
        .arg("--smoke")
        .arg("--offline")
        .arg("--output")
        .arg(&out_path)
        .arg("--git")
        .arg(&git)
        .arg("--grit")
        .arg(&grit)
        .arg("--baseline-grit")
        .arg(&grit)
        .arg("--only")
        .arg("commit/small@synth-10k");
    let out = cmd.output().expect("spawn");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let report = grit_utils::validate_daily_report(&out_path).expect("validate");
    let row = &report.scenarios[0];
    let base = row.baseline_grit.as_ref().expect("baseline_grit row");
    assert!(base.peak_rss_bytes > 0);
    assert!(row.baseline_grit_ratio.is_some());
    assert!(report.tools.baseline_grit.is_some());
}
