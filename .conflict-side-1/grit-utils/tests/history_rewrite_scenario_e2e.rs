//! End-to-end smoke test for `grit-bench rewrite-history --smoke`.

use std::path::PathBuf;
use std::process::Command;

use grit_utils::binary::require_hyperfine;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn build_grit_binary(release: bool) -> PathBuf {
    let workspace = workspace_root();
    let mut args = vec!["build", "-p", "grit-cli", "--bin", "grit", "-q"];
    if release {
        args.push("--release");
    }
    let status = Command::new(env!("CARGO"))
        .args(&args)
        .current_dir(&workspace)
        .status()
        .expect("spawn cargo build");
    assert!(status.success(), "cargo build grit failed");
    if release {
        workspace.join("target/release/grit")
    } else {
        workspace.join("target/debug/grit")
    }
}

#[test]
fn rewrite_history_scenario_smoke_end_to_end() {
    require_hyperfine().expect(
        "hyperfine is required for grit-bench rewrite-history smoke; install from https://github.com/sharkdp/hyperfine",
    );
    let grit = build_grit_binary(false);
    let bench = PathBuf::from(env!("CARGO_BIN_EXE_grit-bench"));

    let output = Command::new(&bench)
        .args([
            "rewrite-history",
            "--smoke",
            "--warmup",
            "0",
            "--min-runs",
            "1",
            "--format",
            "json",
            "--grit",
            &grit.to_string_lossy(),
        ])
        .output()
        .expect("run grit-bench rewrite-history --smoke");
    assert!(
        output.status.success(),
        "grit-bench rewrite-history --smoke failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse JSON report");
    assert_eq!(report["schema_version"], 1);
    let scenarios = report["scenarios"].as_array().expect("scenarios");
    assert_eq!(scenarios.len(), 6, "expected six rewrite scenarios");
    for scenario in scenarios {
        let ratio = scenario["ratio"].as_f64().expect("ratio");
        assert!(
            ratio.is_finite() && ratio > 0.0,
            "bad ratio for {}",
            scenario["id"]
        );
    }
}
