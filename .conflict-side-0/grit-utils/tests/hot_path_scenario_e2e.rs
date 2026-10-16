//! End-to-end test for switch and pick hot-path scenarios via `grit-bench`.

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
fn hot_path_switch_and_pick_end_to_end() {
    if require_hyperfine().is_err() {
        eprintln!("skipping hot_path_switch_and_pick_end_to_end: hyperfine not on PATH");
        return;
    }

    let grit = build_grit_executable();
    let bench = std::path::Path::new(env!("CARGO_BIN_EXE_grit-bench"));
    let grit_flag = grit.to_string_lossy();

    let output = Command::new(bench)
        .args([
            "hot-paths",
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
        .expect("run grit-bench hot-paths");
    assert!(
        output.status.success(),
        "grit-bench hot-paths failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse JSON report");
    assert_eq!(report["schema_version"], 1);

    let scenarios = report["scenarios"].as_array().expect("scenarios array");
    assert!(!scenarios.is_empty());

    for scenario in scenarios {
        let id = scenario["id"].as_str().unwrap_or("");
        if id.starts_with("switch-200") || id.starts_with("pick-200") {
            let ratio = scenario["ratio"].as_f64().expect("ratio");
            assert!(
                ratio.is_finite() && ratio > 0.0,
                "scenario {id} has bad ratio {ratio}"
            );
            assert!(scenario["git"]["median_ms"].as_f64().unwrap() > 0.0);
            assert!(scenario["grit"]["median_ms"].as_f64().unwrap() > 0.0);
        }
    }

    let has_switch = scenarios.iter().any(|s| {
        s["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("switch-200"))
    });
    let has_pick = scenarios.iter().any(|s| {
        s["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("pick-200"))
    });
    assert!(has_switch, "expected switch-200 scenario in report");
    assert!(has_pick, "expected pick-200 scenario in report");
}
