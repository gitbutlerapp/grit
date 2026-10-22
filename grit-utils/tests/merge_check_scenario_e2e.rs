//! End-to-end test for `grit-bench merge-check`.

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

#[test]
fn merge_check_scenario_end_to_end() {
    if require_hyperfine().is_err() {
        eprintln!("skipping merge_check_scenario_end_to_end: hyperfine not on PATH");
        return;
    }

    let grit = build_grit_executable();
    let bench = std::path::Path::new(env!("CARGO_BIN_EXE_grit-bench"));
    let grit_flag = grit.to_string_lossy();

    let output = Command::new(bench)
        .args([
            "merge-check",
            "--sizes",
            "200",
            "--warmup",
            "1",
            "--min-runs",
            "2",
            "--format",
            "json",
            "--grit",
            &grit_flag,
        ])
        .output()
        .expect("run grit-bench merge-check");
    assert!(
        output.status.success(),
        "grit-bench merge-check failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse JSON report");
    let scenarios = report["scenarios"].as_array().expect("scenarios");
    assert_eq!(scenarios.len(), 1);
    assert_eq!(scenarios[0]["id"], "merge-check-200");
    assert!(scenarios[0]["git"]["median_ms"].as_f64().unwrap() > 0.0);
    assert!(scenarios[0]["grit"]["median_ms"].as_f64().unwrap() > 0.0);
    let ratio = scenarios[0]["ratio"].as_f64().expect("ratio");
    assert!(
        ratio <= 1.0,
        "merge-check should be at most 1.0× git merge-tree, got {ratio}"
    );
}
