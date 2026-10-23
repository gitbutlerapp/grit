//! Execute all five campaign conflict matrix rows on a synthetic worktree (grit + git).

use std::path::{Path, PathBuf};
use std::process::Command;

use grit_utils::binary::require_hyperfine;
use grit_utils::campaign_prep::ensure_campaign_conflicts;
use grit_utils::conflicts_fixture::{
    prepare_conflicts_merge_conflict_continue, prepare_conflicts_merge_no_ff,
    prepare_conflicts_pick_range, prepare_conflicts_revert_mainline,
    prepare_conflicts_revert_single,
};
use grit_utils::conflicts_suite::campaign_conflicts_scenarios;
use grit_utils::scenarios::PrepareKind;

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

fn init_repo_with_files(git: &Path, dir: &Path, n: usize) {
    std::fs::create_dir_all(dir).expect("mkdir repo");
    Command::new(git)
        .args(["init", "-q", "-b", "campaign-main"])
        .current_dir(dir)
        .status()
        .expect("git init");
    for i in 0..n {
        std::fs::write(dir.join(format!("f{i}.txt")), format!("v{i}\n")).expect("write file");
    }
    Command::new(git)
        .args(["add", "-A"])
        .current_dir(dir)
        .status()
        .expect("git add");
    Command::new(git)
        .args(["commit", "-qm", "initial"])
        .current_dir(dir)
        .status()
        .expect("git commit");
}

fn run_cli(repo: &Path, bin: &Path, argv: &[String]) -> bool {
    Command::new(bin)
        .args(argv)
        .current_dir(repo)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn run_shell(repo: &Path, script_argv: &[String]) -> bool {
    assert_eq!(script_argv.first().map(String::as_str), Some("-c"));
    let script = script_argv.get(1).expect("shell script");
    Command::new("sh")
        .args(["-c", script])
        .current_dir(repo)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn campaign_conflicts_matrix_rows_execute() {
    if require_hyperfine().is_err() {
        eprintln!("skipping campaign_conflicts_matrix_rows_execute: hyperfine not on PATH");
        return;
    }

    let git = grit_utils::binary::resolve_binary("git", None).expect("git");
    let grit = build_grit_executable();
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo_with_files(&git, dir.path(), 120);
    let meta = ensure_campaign_conflicts(&git, dir.path()).expect("ensure campaign conflicts");
    let scenarios =
        campaign_conflicts_scenarios(&grit, &git, &meta, "smoke-fixture", "campaign-smoke");
    assert_eq!(scenarios.len(), 5);

    for scenario in &scenarios {
        ensure_campaign_conflicts(&git, dir.path()).expect("restore campaign conflicts baseline");
        let prep = scenario.prepare_kind.expect("prepare kind");
        match prep {
            PrepareKind::ConflictsMergeNoFfReset => {
                prepare_conflicts_merge_no_ff(&git, dir.path()).expect("prep merge-no-ff");
            }
            PrepareKind::ConflictsMergeConflictContinueReset => {
                prepare_conflicts_merge_conflict_continue(&git, dir.path())
                    .expect("prep merge-conflict-continue");
            }
            PrepareKind::ConflictsPickRangeReset => {
                prepare_conflicts_pick_range(&git, dir.path()).expect("prep pick-range");
            }
            PrepareKind::ConflictsRevertSingleReset => {
                prepare_conflicts_revert_single(&git, dir.path()).expect("prep revert-single");
            }
            PrepareKind::ConflictsRevertMainlineReset => {
                prepare_conflicts_revert_mainline(&git, dir.path()).expect("prep revert-mainline");
            }
            other => panic!("unexpected prepare kind for campaign conflicts: {other:?}"),
        }

        let grit_ok = if scenario.grit_via_shell {
            run_shell(dir.path(), &scenario.grit_argv)
        } else {
            run_cli(dir.path(), &grit, &scenario.grit_argv)
        };
        assert!(grit_ok, "grit failed scenario {}", scenario.id);

        match prep {
            PrepareKind::ConflictsMergeNoFfReset => {
                prepare_conflicts_merge_no_ff(&git, dir.path()).expect("re-prep for git");
            }
            PrepareKind::ConflictsMergeConflictContinueReset => {
                prepare_conflicts_merge_conflict_continue(&git, dir.path())
                    .expect("re-prep for git");
            }
            PrepareKind::ConflictsPickRangeReset => {
                prepare_conflicts_pick_range(&git, dir.path()).expect("re-prep for git");
            }
            PrepareKind::ConflictsRevertSingleReset => {
                prepare_conflicts_revert_single(&git, dir.path()).expect("re-prep for git");
            }
            PrepareKind::ConflictsRevertMainlineReset => {
                prepare_conflicts_revert_mainline(&git, dir.path()).expect("re-prep for git");
            }
            other => panic!("unexpected prepare kind: {other:?}"),
        }

        let git_ok = if scenario.git_via_shell {
            run_shell(dir.path(), &scenario.git_argv)
        } else {
            run_cli(dir.path(), &git, &scenario.git_argv)
        };
        assert!(git_ok, "git failed scenario {}", scenario.id);
    }
}
