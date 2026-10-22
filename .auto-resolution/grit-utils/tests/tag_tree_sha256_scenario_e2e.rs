//! Smoke test for `grit-bench tag-tree-sha256` JSON schema.

use std::process::Command;

#[test]
fn tag_tree_sha256_smoke_report_schema() {
    let bench = env!("CARGO_BIN_EXE_grit-bench");
    let grit = std::env::var("CARGO_BIN_EXE_grit").unwrap_or_else(|_| {
        grit_utils::binary::resolve_binary("grit", None)
            .expect("grit binary")
            .display()
            .to_string()
    });
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
