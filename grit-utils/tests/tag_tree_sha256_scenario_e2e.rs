//! Smoke test for `grit-bench tag-tree-sha256` JSON schema.

use std::path::PathBuf;
use std::process::Command;

/// Build the workspace `grit` binary and return its path. `CARGO_BIN_EXE_grit` is never
/// set for this crate (the binary lives in `grit-cli`), and `grit` is not on PATH in CI.
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
fn tag_tree_sha256_smoke_report_schema() {
    let bench = env!("CARGO_BIN_EXE_grit-bench");
    let grit = build_grit_executable().display().to_string();
    let out = Command::new(bench)
        .args([
            "tag-tree-sha256",
            "--smoke",
            "--format",
            "json",
            "--grit",
            &grit,
        ])
        .output()
        .expect("run grit-bench tag-tree-sha256 --smoke");
    assert!(
        out.status.success(),
        "grit-bench failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: grit_utils::BenchReport =
        serde_json::from_slice(&out.stdout).expect("parse bench json");
    assert_eq!(report.schema_version, 1);
    assert!(
        report
            .scenarios
            .iter()
            .any(|s| s.id.contains("tag-verify-digest-no-signature")),
        "expected digest verify scenario"
    );
}
