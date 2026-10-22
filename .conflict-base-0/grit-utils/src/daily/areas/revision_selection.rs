//! Daily matrix plans for revision-selection filter scenarios.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};

use super::super::helpers::{ctx, prepare_none, scenario_spec, setup_git_git_history, sh_devnull};
use super::super::matrix_helpers::{
    ensure_commit_graph_absent, verify_git_commit_graph_in_repo, write_git_commit_graph_reachable,
};
use super::super::plan::{DailyHeadlineContext, DailyHeadlinePlan, DailySetup};
use super::super::revision_selection_matrix_registry::REVISION_SELECTION_MATRIX_SCENARIOS;
use super::super::runner::BenchArgv;
use crate::fixture::{remove_dir_robust, scratch_dir};
use crate::network_fixture::{ensure_fixture, NetworkFixtureKind, NetworkProfile};

fn clone_deep_worktree(git: &std::path::Path, smoke: bool, dest_name: &str) -> Result<PathBuf> {
    let profile = if smoke {
        NetworkProfile::Smoke
    } else {
        NetworkProfile::Production
    };
    let bare = ensure_fixture(git, profile, NetworkFixtureKind::DeepHistory)?;
    let dest = scratch_dir().join(dest_name);
    remove_dir_robust(&dest);
    let mut cmd = Command::new(git);
    cmd.args([
        "clone",
        bare.to_str().context("bare path utf-8")?,
        dest.to_str().context("dest utf-8")?,
    ]);
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    let out = cmd.output().context("git clone deep-history")?;
    if !out.status.success() {
        anyhow::bail!("git clone failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(dest)
}

fn setup_deep_history_no_graph(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    let repo = clone_deep_worktree(setup.git, setup.smoke, "daily-deep-history-no-graph")?;
    ensure_commit_graph_absent(&repo)?;
    Ok(ctx(repo, setup))
}

fn setup_deep_history_commit_graph(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    let repo = clone_deep_worktree(setup.git, setup.smoke, "daily-deep-history-commit-graph")?;
    write_git_commit_graph_reachable(setup.git, &repo)?;
    verify_git_commit_graph_in_repo(setup.git, &repo)?;
    Ok(ctx(repo, setup))
}

pub fn plans() -> Vec<DailyHeadlinePlan> {
    let mut out = Vec::new();
    for spec in REVISION_SELECTION_MATRIX_SCENARIOS {
        let plan = match spec.id {
            "log/author@git-git" => DailyHeadlinePlan {
                spec: scenario_spec(spec.id),
                pending: false,
                setup: setup_git_git_history,
                prepare: prepare_none,
                build_git: |c| {
                    sh_devnull(&c.git, &["log", "--author=.", "-n", "50", "--format=%H"])
                },
                build_grit: |c| sh_devnull(&c.grit, &["log", "--author=.", "-n", "50", "--json"]),
                correctness: None,
            },
            "revs/count-since@deep-history" => DailyHeadlinePlan {
                spec: scenario_spec(spec.id),
                pending: false,
                setup: setup_deep_history_no_graph,
                prepare: prepare_none,
                build_git: |c| {
                    sh_devnull(
                        &c.git,
                        &[
                            "rev-list",
                            "--count",
                            "--since=1970-01-01 13:53:00 +0000",
                            "-n",
                            "20",
                            "HEAD",
                        ],
                    )
                },
                build_grit: |c| {
                    sh_devnull(
                        &c.grit,
                        &[
                            "revs",
                            "--count",
                            "--since=1970-01-01 13:53:00 +0000",
                            "-n",
                            "20",
                            "HEAD",
                        ],
                    )
                },
                correctness: None,
            },
            "revs/left-right-count@deep-history" => DailyHeadlinePlan {
                spec: scenario_spec(spec.id),
                pending: false,
                setup: setup_deep_history_no_graph,
                prepare: prepare_none,
                build_git: |c| {
                    let range = if c.smoke {
                        "HEAD~20...HEAD"
                    } else {
                        "HEAD~500...HEAD"
                    };
                    sh_devnull(&c.git, &["rev-list", "--left-right", "--count", range])
                },
                build_grit: |c| {
                    let range = if c.smoke {
                        "HEAD~20...HEAD"
                    } else {
                        "HEAD~500...HEAD"
                    };
                    sh_devnull(&c.grit, &["revs", "--left-right", "--count", range])
                },
                correctness: None,
            },
            "revs/count-author@deep-history"
            | "revs/author-n50@deep-history"
            | "log/author-n50@deep-history" => DailyHeadlinePlan {
                spec: scenario_spec(spec.id),
                pending: false,
                setup: setup_deep_history_no_graph,
                prepare: prepare_none,
                build_git: deep_history_git_bench(spec.id),
                build_grit: deep_history_grit_bench(spec.id),
                correctness: None,
            },
            "revs/count-author@deep-history-cg"
            | "revs/author-n50@deep-history-cg"
            | "log/author-n50@deep-history-cg" => DailyHeadlinePlan {
                spec: scenario_spec(spec.id),
                pending: false,
                setup: setup_deep_history_commit_graph,
                prepare: prepare_none,
                build_git: deep_history_git_bench(spec.id),
                build_grit: deep_history_grit_bench(spec.id),
                correctness: None,
            },
            _ => continue,
        };
        out.push(plan);
    }
    out
}

fn deep_history_git_bench(id: &str) -> fn(&DailyHeadlineContext) -> BenchArgv {
    match id {
        "revs/count-author@deep-history" | "revs/count-author@deep-history-cg" => {
            |c| sh_devnull(&c.git, &["rev-list", "--count", "--author=.", "HEAD"])
        }
        "revs/author-n50@deep-history" | "revs/author-n50@deep-history-cg" => {
            |c| sh_devnull(&c.git, &["rev-list", "-n", "50", "--author=.", "HEAD"])
        }
        "log/author-n50@deep-history" | "log/author-n50@deep-history-cg" => {
            |c| sh_devnull(&c.git, &["log", "--author=.", "-n", "50", "--format=%H"])
        }
        _ => unreachable!("deep_history_git_bench: {id}"),
    }
}

fn deep_history_grit_bench(id: &str) -> fn(&DailyHeadlineContext) -> BenchArgv {
    match id {
        "revs/count-author@deep-history" | "revs/count-author@deep-history-cg" => {
            |c| sh_devnull(&c.grit, &["revs", "--author=.", "--count", "HEAD"])
        }
        "revs/author-n50@deep-history" | "revs/author-n50@deep-history-cg" => {
            |c| sh_devnull(&c.grit, &["revs", "--author=.", "-n", "50", "HEAD"])
        }
        "log/author-n50@deep-history" | "log/author-n50@deep-history-cg" => {
            |c| sh_devnull(&c.grit, &["log", "--author=.", "-n", "50", "--json"])
        }
        _ => unreachable!("deep_history_grit_bench: {id}"),
    }
}
