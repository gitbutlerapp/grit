//! Smoke test for `grit-bench run --group worktree --fixture synth-s`.

use std::path::PathBuf;
use std::process::Command;

use grit_utils::binary::require_hyperfine;
use grit_utils::fixture_registry::FixtureKind;
use tempfile::TempDir;

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

#[test]
fn worktree_group_smoke_on_synth_s() {
    if require_hyperfine().is_err() {
        eprintln!("skipping worktree_group_smoke_on_synth_s: hyperfine not on PATH");
        return;
    }

    let cache = TempDir::new().expect("tempdir");
    let scratch = TempDir::new().expect("tempdir");
    std::env::set_var("GRIT_BENCH_CACHE", cache.path());
    std::env::set_var("GRIT_BENCH_SCRATCH", scratch.path());

    let git = grit_utils::binary::resolve_binary("git", None).expect("git");
    grit_utils::fixture_registry::ensure_cached(&git, FixtureKind::SynthS, false)
        .expect("prepare synth-s cache");

    let grit = build_grit_executable();
    let bench = PathBuf::from(env!("CARGO_BIN_EXE_grit-bench"));
    let grit_flag = grit.to_string_lossy();

    let output = Command::new(&bench)
        .args([
            "run",
            "--group",
            "worktree",
            "--fixture",
            "synth-s",
            "--smoke",
            "--offline",
            "--format",
            "json",
            "--grit",
            &grit_flag,
        ])
        .env("GRIT_BENCH_CACHE", cache.path())
        .env("GRIT_BENCH_SCRATCH", scratch.path())
        .output()
        .expect("run grit-bench run");

    assert!(
        output.status.success(),
        "grit-bench run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse JSON report");
    assert_eq!(report["schema_version"], 1);
    let scenarios = report["scenarios"].as_array().expect("scenarios array");
    assert_eq!(scenarios.len(), 5, "expected five worktree scenarios");
    for s in scenarios {
        assert_eq!(s["group"], "worktree");
        assert!(s["ratio"].as_f64().unwrap() > 0.0);
        let git_runs = s["git"]["runs_ms"].as_array().expect("git runs_ms");
        let grit_runs = s["grit"]["runs_ms"].as_array().expect("grit runs_ms");
        assert_eq!(
            git_runs.len(),
            1,
            "smoke must use hyperfine --runs 1 for git ({})",
            s["id"]
        );
        assert_eq!(
            grit_runs.len(),
            1,
            "smoke must use hyperfine --runs 1 for grit ({})",
            s["id"]
        );
        assert!(s["git"]["median_ms"].as_f64().unwrap() > 0.0);
        assert!(s["grit"]["median_ms"].as_f64().unwrap() > 0.0);
    }
}
