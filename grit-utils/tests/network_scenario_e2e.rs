//! End-to-end smoke test for `grit-bench network`.

use std::path::PathBuf;
use std::process::Command;

use grit_utils::binary::require_hyperfine;

fn build_grit_executable() -> PathBuf {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let status = Command::new(env!("CARGO"))
        .args([
            "build",
            "-p",
            "grit-cli",
            "-p",
            "grit-http-server",
            "--bin",
            "grit",
            "-q",
        ])
        .current_dir(&workspace)
        .status()
        .expect("spawn cargo build");
    assert!(status.success(), "cargo build grit binaries failed");
    workspace.join("target/debug/grit")
}

#[test]
fn network_scenario_smoke_end_to_end() {
    if require_hyperfine().is_err() {
        eprintln!("skipping network_scenario_smoke_end_to_end: hyperfine not on PATH");
        return;
    }

    let grit = build_grit_executable();
    let bench = PathBuf::from(env!("CARGO_BIN_EXE_grit-bench"));
    let grit_flag = grit.to_string_lossy();

    let output = Command::new(&bench)
        .args([
            "network",
            "--smoke",
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
        .expect("run grit-bench network --smoke");
    assert!(
        output.status.success(),
        "grit-bench network --smoke failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse JSON report");
    assert_eq!(report["schema_version"], 1);

    let scenarios = report["scenarios"].as_array().expect("scenarios array");
    assert_eq!(scenarios.len(), 6, "expected six smoke network scenarios");
    for scenario in scenarios {
        let ratio = scenario["ratio"].as_f64().expect("ratio");
        assert!(
            ratio.is_finite() && ratio > 0.0,
            "bad ratio for {}",
            scenario["id"]
        );
    }
}
