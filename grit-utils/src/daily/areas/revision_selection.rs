//! Daily matrix plans for revision-selection filter scenarios.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};

use super::super::helpers::{ctx, prepare_none, scenario_spec, setup_git_git_history, sh_devnull};
use super::super::plan::{DailyHeadlineContext, DailyHeadlinePlan, DailySetup};
use super::super::revision_selection_matrix_registry::REVISION_SELECTION_MATRIX_SCENARIOS;
use crate::fixture::{remove_dir_robust, scratch_dir};
use crate::network_fixture::{ensure_fixture, NetworkFixtureKind, NetworkProfile};

fn clone_deep_worktree(git: &std::path::Path) -> Result<PathBuf> {
    let bare = ensure_fixture(
        git,
        NetworkProfile::Production,
        NetworkFixtureKind::DeepHistory,
    )?;
    let dest = scratch_dir().join("daily-deep-history-worktree");
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

fn setup_deep_history(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    if setup.smoke {
        anyhow::bail!("deep-history revision-selection rows require a full (non-smoke) daily run");
    }
    let repo = clone_deep_worktree(setup.git)?;
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
                setup: setup_deep_history,
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
                setup: setup_deep_history,
                prepare: prepare_none,
                build_git: |c| {
                    sh_devnull(
                        &c.git,
                        &["rev-list", "--left-right", "--count", "HEAD~500...HEAD"],
                    )
                },
                build_grit: |c| {
                    sh_devnull(
                        &c.grit,
                        &["revs", "--left-right", "--count", "HEAD~500...HEAD"],
                    )
                },
                correctness: None,
            },
            "revs/count-author@deep-history" | "revs/count-author@deep-history-cg" => {
                DailyHeadlinePlan {
                    spec: scenario_spec(spec.id),
                    pending: false,
                    setup: setup_deep_history,
                    prepare: prepare_none,
                    build_git: |c| {
                        sh_devnull(&c.git, &["rev-list", "--count", "--author=.", "HEAD"])
                    },
                    build_grit: |c| sh_devnull(&c.grit, &["revs", "--author=.", "--count", "HEAD"]),
                    correctness: None,
                }
            }
            "revs/author-n50@deep-history" | "revs/author-n50@deep-history-cg" => {
                DailyHeadlinePlan {
                    spec: scenario_spec(spec.id),
                    pending: false,
                    setup: setup_deep_history,
                    prepare: prepare_none,
                    build_git: |c| {
                        sh_devnull(&c.git, &["rev-list", "-n", "50", "--author=.", "HEAD"])
                    },
                    build_grit: |c| {
                        sh_devnull(&c.grit, &["revs", "--author=.", "-n", "50", "HEAD"])
                    },
                    correctness: None,
                }
            }
            "log/author-n50@deep-history" | "log/author-n50@deep-history-cg" => DailyHeadlinePlan {
                spec: scenario_spec(spec.id),
                pending: false,
                setup: setup_deep_history,
                prepare: prepare_none,
                build_git: |c| {
                    sh_devnull(&c.git, &["log", "--author=.", "-n", "50", "--format=%H"])
                },
                build_grit: |c| sh_devnull(&c.grit, &["log", "--author=.", "-n", "50", "--json"]),
                correctness: None,
            },
            _ => continue,
        };
        out.push(plan);
    }
    out
}
