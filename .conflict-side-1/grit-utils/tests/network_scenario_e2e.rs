//! End-to-end smoke test for `grit-bench network`.

use std::path::PathBuf;
use std::process::Command;

use grit_utils::binary::{require_hyperfine, resolve_binary};

fn build_network_binaries() -> (PathBuf, PathBuf) {
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
            "--bin",
            "grit-http-server",
            "-q",
        ])
        .current_dir(&workspace)
        .status()
        .expect("spawn cargo build");
    assert!(
        status.success(),
        "cargo build grit / grit-http-server failed; fix compilation before running this test"
    );
    (
        workspace.join("target/debug/grit"),
        workspace.join("target/debug/grit-http-server"),
    )
}

#[test]
fn network_scenario_smoke_end_to_end() {
    let Ok(_hyperfine) = require_hyperfine() else {
        eprintln!(
            "SKIP: hyperfine is required for grit-bench network smoke; install from https://github.com/sharkdp/hyperfine"
        );
        return;
    };
    if resolve_binary("git", None).is_err() {
        eprintln!("SKIP: system git not available");
        return;
    }
    let (grit, http_server) = build_network_binaries();
    let bench = PathBuf::from(env!("CARGO_BIN_EXE_grit-bench"));
    let grit_flag = grit.to_string_lossy();
    let http_flag = http_server.to_string_lossy();

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
            "--http-server",
            &http_flag,
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
    assert_eq!(
        scenarios.len(),
        15,
        "expected fifteen smoke scenarios (clone×3, fetch incr/noop×3, push×2, ls-remote×3, server compare)"
    );
    for scenario in scenarios {
        let ratio = scenario["ratio"].as_f64().expect("ratio");
        assert!(
            ratio.is_finite() && ratio > 0.0,
            "bad ratio for {}",
            scenario["id"]
        );
    }
}
