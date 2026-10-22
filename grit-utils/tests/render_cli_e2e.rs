//! CLI regression for `make bench-scenarios` markdown render argv.

use std::path::PathBuf;
use std::process::Command;

use grit_utils::schema::{
    BenchReport, DriverKind, MachineInfo, ScenarioResult, TimingStats, ToolVersions, SCHEMA_VERSION,
};

fn minimal_report() -> BenchReport {
    let timing = TimingStats {
        mean_ms: 1.0,
        median_ms: 1.0,
        stddev_ms: 0.0,
        min_ms: 1.0,
        max_ms: 1.0,
        runs_ms: vec![1.0],
        peak_rss_bytes: None,
    };
    BenchReport {
        schema_version: SCHEMA_VERSION,
        timestamp: "2026-01-01T00:00:00Z".into(),
        machine: MachineInfo {
            cpu_model: "test".into(),
            physical_cores: 1,
            logical_cores: 1,
            ram_bytes: 0,
            os: "test".into(),
            kernel: "test".into(),
            scratch_filesystem: "test".into(),
            rustc_version: "test".into(),
            cargo_profile: "test".into(),
        },
        tools: ToolVersions {
            git: "git".into(),
            grit: "grit".into(),
            grit_commit: None,
            hyperfine: None,
        },
        fixtures: Vec::new(),
        scenarios: vec![ScenarioResult {
            id: "sample".into(),
            group: "g".into(),
            fixture: "f".into(),
            description: "d".into(),
            driver: DriverKind::Cli,
            git: timing.clone(),
            grit: timing,
            ratio: 1.0,
            grit_failure: None,
        }],
    }
}

#[test]
fn render_accepts_makefile_input_flag() {
    let bench = PathBuf::from(env!("CARGO_BIN_EXE_grit-bench"));
    let json_path = std::env::temp_dir().join("grit-bench-render-e2e.json");
    let md_path = std::env::temp_dir().join("grit-bench-render-e2e.md");
    let json = serde_json::to_string_pretty(&minimal_report()).expect("serialize");
    std::fs::write(&json_path, json).expect("write json");

    let output = Command::new(&bench)
        .args([
            "render",
            "--format",
            "markdown",
            "--input",
            &json_path.to_string_lossy(),
            "--output",
            &md_path.to_string_lossy(),
        ])
        .output()
        .expect("spawn render");
    assert!(
        output.status.success(),
        "render --input must match make bench-scenarios (exit {}): {}",
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stderr)
    );
    let md = std::fs::read_to_string(&md_path).expect("read markdown");
    assert!(md.contains("sample"), "markdown should list scenario id");
}
