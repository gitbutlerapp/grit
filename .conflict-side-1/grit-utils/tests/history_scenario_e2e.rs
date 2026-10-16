//! End-to-end smoke test for `grit-bench history --smoke`.

use std::path::PathBuf;
use std::process::Command;

use grit_utils::binary::require_hyperfine;
use grit_utils::history_compat::{
    verify_blame_porcelain, verify_cat_file_batch_head, verify_rev_list_count_head,
};
use grit_utils::history_fixture::{ensure_synth_history_repo, load_history_meta};
use grit_utils::synth::SynthTier;

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
fn history_lib_ops_match_git_on_synth_s() {
    let git = grit_utils::binary::resolve_binary("git", None).expect("git");
    let bench = PathBuf::from(env!("CARGO_BIN_EXE_grit-bench"));
    let repo = ensure_synth_history_repo(&git, SynthTier::S).expect("synth S");
    let meta = load_history_meta(&repo).expect("meta");
    verify_rev_list_count_head(&git, &bench, &repo).expect("rev-list count");
    verify_blame_porcelain(&git, &bench, &repo, &meta.blame_path).expect("blame");
    verify_cat_file_batch_head(&git, &bench, &repo).expect("cat-file batch");
}

#[test]
fn history_scenario_smoke_end_to_end() {
    require_hyperfine().expect(
        "hyperfine is required for grit-bench history smoke; install from https://github.com/sharkdp/hyperfine",
    );
    let grit = build_grit_binary(false);
    let bench = PathBuf::from(env!("CARGO_BIN_EXE_grit-bench"));

    let output = Command::new(&bench)
        .args([
            "history",
            "--smoke",
            "--warmup",
            "1",
            "--min-runs",
            "2",
            "--format",
            "json",
            "--grit",
            &grit.to_string_lossy(),
        ])
        .output()
        .expect("run grit-bench history --smoke");
    assert!(
        output.status.success(),
        "grit-bench history --smoke failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse JSON report");
    assert_eq!(report["schema_version"], 1);
    let scenarios = report["scenarios"].as_array().expect("scenarios");
    assert_eq!(scenarios.len(), 6, "expected six history scenarios");
    for scenario in scenarios {
        let driver = scenario["driver"].as_str().expect("driver");
        let ratio = scenario["ratio"].as_f64().expect("ratio");
        assert!(
            ratio.is_finite() && ratio > 0.0,
            "bad ratio for {}",
            scenario["id"]
        );
        if driver == "lib" {
            assert!(
                scenario["grit_failure"].is_null(),
                "lib scenario failed: {}",
                scenario["id"]
            );
        }
    }
}

#[test]
fn history_smoke_release_compare_within_tolerance() {
    require_hyperfine().expect(
        "hyperfine is required for grit-bench history smoke; install from https://github.com/sharkdp/hyperfine",
    );
    let workspace = workspace_root();
    let status = Command::new(env!("CARGO"))
        .args([
            "build",
            "-p",
            "grit-cli",
            "-p",
            "grit-utils",
            "--release",
            "-q",
        ])
        .current_dir(&workspace)
        .status()
        .expect("release build");
    assert!(status.success(), "release build failed");
    let grit = workspace.join("target/release/grit");
    let bench = workspace.join("target/release/grit-bench");
    let out_a = std::env::temp_dir().join("grit-history-smoke-compare-a.json");
    let out_b = std::env::temp_dir().join("grit-history-smoke-compare-b.json");
    for out in [&out_a, &out_b] {
        let output = Command::new(&bench)
            .args([
                "history",
                "--smoke",
                "--format",
                "json",
                "--output",
                &out.to_string_lossy(),
                "--grit",
                &grit.to_string_lossy(),
            ])
            .output()
            .expect("history --smoke");
        assert!(
            output.status.success(),
            "history smoke failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let compare = Command::new(&bench)
        .args([
            "compare",
            &out_a.to_string_lossy(),
            &out_b.to_string_lossy(),
            "--tolerance",
            "0.10",
        ])
        .output()
        .expect("compare");
    assert!(
        compare.status.success(),
        "compare failed: {}",
        String::from_utf8_lossy(&compare.stderr)
    );
}
