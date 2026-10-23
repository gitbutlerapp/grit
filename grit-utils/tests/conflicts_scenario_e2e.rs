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

#[test]
fn revert_single_matches_git() {
    let git = grit_utils::binary::resolve_binary("git", None).expect("git");
    let grit = build_grit_executable();
    let spec = grit_utils::hot_path_fixture::HotPathRepoSpec {
        file_count: 200,
        dir_count: 2,
        history_commits: 5,
        fsmonitor: false,
    };
    let touch = grit_utils::conflicts_fixture::conflicts_touch_paths(spec.file_count);
    let grit_dir = tempfile::tempdir().expect("grit dir");
    let git_dir = tempfile::tempdir().expect("git dir");
    grit_utils::conflicts_fixture::setup_conflicts_clean_fixture_in(
        &git,
        grit_dir.path(),
        spec,
        touch,
    )
    .expect("setup grit");
    grit_utils::conflicts_fixture::setup_conflicts_clean_fixture_in(
        &git,
        git_dir.path(),
        spec,
        touch,
    )
    .expect("setup git");
    grit_utils::conflicts_fixture::prepare_conflicts_revert_single(&git, grit_dir.path())
        .expect("prep grit");
    grit_utils::conflicts_fixture::prepare_conflicts_revert_single(&git, git_dir.path())
        .expect("prep git");
    let grit_status = Command::new(&grit)
        .current_dir(grit_dir.path())
        .args(["revert", "HEAD"])
        .status()
        .expect("grit revert");
    let git_status = Command::new(&git)
        .current_dir(git_dir.path())
        .args(["revert", "--no-edit", "HEAD"])
        .status()
        .expect("git revert");
    assert!(grit_status.success(), "grit revert failed");
    assert!(git_status.success(), "git revert failed");
}

#[test]
fn merge_conflict_continue_script_exits_zero() {
    let git = grit_utils::binary::resolve_binary("git", None).expect("git");
    let grit = build_grit_executable();
    let spec = grit_utils::hot_path_fixture::HotPathRepoSpec {
        file_count: 200,
        dir_count: 2,
        history_commits: 5,
        fsmonitor: false,
    };
    let touch = grit_utils::conflicts_fixture::conflicts_touch_paths(spec.file_count);
    let overlap = grit_utils::conflicts_fixture::conflict_overlap_for_size(spec.file_count);
    let dir = tempfile::tempdir().expect("tempdir");
    grit_utils::conflicts_fixture::setup_conflicts_merge_fixture_in(
        &git,
        dir.path(),
        spec,
        touch,
        overlap,
    )
    .expect("setup");
    grit_utils::conflicts_fixture::prepare_conflicts_merge_conflict_continue(&git, dir.path())
        .expect("prepare");
    let topic = grit_utils::conflicts_fixture::load_conflicts_meta(dir.path())
        .expect("meta")
        .base
        .topic_branch;
    let grit_status = Command::new(&grit)
        .current_dir(dir.path())
        .args(["merge", &topic])
        .status()
        .expect("grit merge");
    assert!(!grit_status.success(), "merge should stop on conflicts");
    let finish = Command::new(&grit)
        .current_dir(dir.path())
        .args(["restore", "--theirs", "."])
        .status()
        .expect("restore");
    assert!(finish.success(), "restore --theirs failed");
    let add = Command::new(&grit)
        .current_dir(dir.path())
        .args(["add", "."])
        .status()
        .expect("add");
    assert!(add.success(), "add failed");
    let cont = Command::new(&grit)
        .current_dir(dir.path())
        .args(["merge", "--continue"])
        .status()
        .expect("continue");
    assert!(cont.success(), "merge --continue failed");
}
