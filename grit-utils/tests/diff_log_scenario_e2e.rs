//! End-to-end tests for diff and log benchmark scenarios.

use std::path::PathBuf;
use std::process::Command;

use grit_utils::binary::require_hyperfine;

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

fn run_bench(subcmd: &str, id: &str) {
    if require_hyperfine().is_err() {
        eprintln!("skipping {subcmd} e2e: hyperfine not on PATH");
        return;
    }
    let grit = build_grit_executable();
    let bench = std::path::Path::new(env!("CARGO_BIN_EXE_grit-bench"));
    let output = Command::new(bench)
        .args([
            subcmd,
            "--sizes",
            "100",
            "--warmup",
            "1",
            "--min-runs",
            "2",
            "--format",
            "json",
            "--grit",
            &grit.to_string_lossy(),
        ])
        .output()
        .unwrap_or_else(|e| panic!("run grit-bench {subcmd}: {e}"));
    assert!(
        output.status.success(),
        "grit-bench {subcmd} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse JSON report");
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["scenarios"][0]["id"], id);
    assert!(report["scenarios"][0]["git"]["median_ms"].as_f64().unwrap() > 0.0);
    assert!(
        report["scenarios"][0]["grit"]["median_ms"]
            .as_f64()
            .unwrap()
            > 0.0
    );
}

#[test]
fn diff_and_log_scenarios_end_to_end() {
    // Scenarios share `/tmp/grit-bench-scratch`; run sequentially.
    run_bench("diff", "diff-100");
    run_bench("log", "log-100");
}
