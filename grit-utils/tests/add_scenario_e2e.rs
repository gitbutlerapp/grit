//! End-to-end test for the add benchmark scenario via the `grit-bench` CLI.

use std::path::PathBuf;
use std::process::Command;

use grit_utils::binary::require_hyperfine;

/// Build `grit-cli` in the workspace and return the debug `grit` binary path.
///
/// Gate runs `cargo test --workspace` without prebuilding `target/release/grit`, so this
/// test must arrange its own grit executable and pass `--grit` explicitly.
fn build_grit_executable() -> PathBuf {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "grit-cli", "--bin", "grit", "-q"])
        .current_dir(&workspace)
        .status()
        .expect("spawn cargo build grit-cli");
    assert!(
        status.success(),
        "cargo build -p grit-cli --bin grit failed with status {status}"
    );
    let grit = workspace.join("target/debug/grit");
    assert!(grit.is_file(), "expected grit binary at {}", grit.display());
    grit.canonicalize().unwrap_or_else(|_| grit)
}

#[test]
fn add_scenario_cli_end_to_end() {
    if require_hyperfine().is_err() {
        eprintln!("skipping add_scenario_cli_end_to_end: hyperfine not on PATH");
        return;
    }

    let grit = build_grit_executable();
    let bench = std::path::Path::new(env!("CARGO_BIN_EXE_grit-bench"));
    let grit_flag = grit.to_string_lossy();

    let output = Command::new(bench)
        .args([
            "add",
            "--sizes",
            "100",
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
        .expect("run grit-bench add");
    assert!(
        output.status.success(),
        "grit-bench add failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse JSON report");
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["scenarios"][0]["id"], "add-100");
    assert!(report["scenarios"][0]["git"]["median_ms"].as_f64().unwrap() > 0.0);
    assert!(
        report["scenarios"][0]["grit"]["median_ms"]
            .as_f64()
            .unwrap()
            > 0.0
    );
}
