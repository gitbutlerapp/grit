//! End-to-end smoke test for `grit-bench parallel`.

use std::path::PathBuf;
use std::process::Command;

use grit_utils::binary::require_hyperfine;

fn build_grit_binary() -> PathBuf {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "grit-cli", "--bin", "grit", "-q"])
        .current_dir(&workspace)
        .status()
        .expect("spawn cargo build");
    assert!(status.success(), "cargo build grit failed");
    workspace.join("target/debug/grit")
}

#[test]
fn parallel_scenario_smoke_end_to_end() {
    require_hyperfine().expect(
        "hyperfine is required for grit-bench parallel smoke; install from https://github.com/sharkdp/hyperfine",
    );

    let grit = build_grit_binary();
    let bench = PathBuf::from(env!("CARGO_BIN_EXE_grit-bench"));
    let grit_flag = grit.to_string_lossy();

    let output = Command::new(&bench)
        .args([
            "parallel",
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
        .expect("run grit-bench parallel --smoke");
    assert!(
        output.status.success(),
        "grit-bench parallel --smoke failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse JSON report");
    assert_eq!(report["schema_version"], 1);

    let scenarios = report["scenarios"].as_array().expect("scenarios array");
    assert_eq!(
        scenarios.len(),
        16,
        "expected sixteen smoke scenarios (8 workloads × threads t1/tauto)"
    );
    for scenario in scenarios {
        let ratio = scenario["ratio"].as_f64().expect("ratio");
        assert!(
            ratio.is_finite() && ratio > 0.0,
            "invalid ratio for {}: {ratio}",
            scenario["id"]
        );
    }
}
