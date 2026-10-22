//! End-to-end test for `grit-bench merge-check`.

use std::process::Command;

use grit_utils::binary::require_hyperfine;

#[test]
fn merge_check_scenario_end_to_end() {
    if require_hyperfine().is_err() {
        eprintln!("skipping merge_check_scenario_end_to_end: hyperfine not on PATH");
        return;
    }

    let bench = std::path::Path::new(env!("CARGO_BIN_EXE_grit-bench"));

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
    assert_eq!(scenarios[0]["driver"], "lib");
}
