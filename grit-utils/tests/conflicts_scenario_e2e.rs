//! End-to-end smoke test for `grit-bench conflicts`.

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

const EXPECTED_IDS: &[&str] = &[
    "merge-no-ff-200",
    "merge-conflict-continue-200",
    "pick-range-20-200",
    "revert-single-200",
    "revert-mainline-200",
];

#[test]
fn conflicts_scenarios_smoke_end_to_end() {
    if require_hyperfine().is_err() {
        eprintln!("skipping conflicts_scenarios_smoke_end_to_end: hyperfine not on PATH");
        return;
    }

    let grit = build_grit_executable();
    let bench = std::path::Path::new(env!("CARGO_BIN_EXE_grit-bench"));
    let grit_flag = grit.to_string_lossy();

    let output = Command::new(bench)
        .args([
            "conflicts",
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
        .expect("run grit-bench conflicts");
    assert!(
        output.status.success(),
        "grit-bench conflicts failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse JSON report");
    assert_eq!(report["schema_version"], 1);
    let scenarios = report["scenarios"].as_array().expect("scenarios");
    assert_eq!(scenarios.len(), EXPECTED_IDS.len());
    for id in EXPECTED_IDS {
        let row = scenarios
            .iter()
            .find(|s| s["id"].as_str() == Some(id))
            .unwrap_or_else(|| panic!("missing scenario {id}"));
        assert!(row["git"]["median_ms"].as_f64().unwrap() > 0.0);
        assert!(row["grit"]["median_ms"].as_f64().unwrap() > 0.0);
        let ratio = row["ratio"].as_f64().expect("ratio");
        assert!(
            ratio.is_finite() && ratio > 0.0,
            "bad ratio for {id}: {ratio}"
        );
    }
}
