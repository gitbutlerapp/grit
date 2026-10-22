//! Revision-selection benchmark scenarios (revwalk filters and author log).

use std::path::Path;

use anyhow::{Context, Result};
use time::OffsetDateTime;

use crate::daily::matrix_helpers::{
    ensure_commit_graph_absent, verify_git_commit_graph_in_repo, write_git_commit_graph_reachable,
};
use crate::network_fixture::{ensure_fixture, NetworkFixtureKind, NetworkProfile};
use crate::revwalk_suite::run_revwalk_suite;
use crate::scenarios::{run_scenario, Driver, RunConfig, Scenario};
use crate::schema::BenchReport;

fn clone_deep_worktree(git: &Path, bare: &Path, dest_name: &str) -> Result<std::path::PathBuf> {
    use crate::fixture::{remove_dir_robust, scratch_dir};
    let dest = scratch_dir().join(dest_name);
    remove_dir_robust(&dest);
    let mut cmd = std::process::Command::new(git);
    cmd.args([
        "clone",
        bare.to_str()
            .ok_or_else(|| anyhow::anyhow!("bare fixture path is not UTF-8"))?,
        dest.to_str()
            .ok_or_else(|| anyhow::anyhow!("worktree path is not UTF-8"))?,
    ]);
    git_env(&mut cmd);
    let out = cmd
        .output()
        .with_context(|| format!("git clone {} into {}", bare.display(), dest.display()))?;
    if !out.status.success() {
        anyhow::bail!("git clone failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(dest)
}

fn git_env(cmd: &mut std::process::Command) {
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
}

fn push_log_author_n50(
    report: &mut BenchReport,
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    id_suffix: &str,
    worktree: &Path,
) -> Result<()> {
    report.scenarios.push(run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: format!("log-author-n50-{id_suffix}"),
            group: "revision-selection".into(),
            fixture: worktree.display().to_string(),
            description: format!("log --author=. -n 50 on deep-history fixture ({id_suffix})"),
            grit_argv: vec![
                "log".into(),
                "--author=.".into(),
                "-n".into(),
                "50".into(),
                "--json".into(),
            ],
            git_argv: vec![
                "log".into(),
                "--author=.".into(),
                "-n".into(),
                "50".into(),
                "--format=%H".into(),
            ],
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        worktree,
    )?);
    Ok(())
}

/// Hyperfine scenarios for revision selection (revwalk suite plus author log rows).
pub fn run_revision_selection_suite(
    hyperfine: &Path,
    bench_exe: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    smoke: bool,
    timestamp: OffsetDateTime,
) -> Result<BenchReport> {
    let mut report = run_revwalk_suite(hyperfine, bench_exe, git, grit, cfg, smoke, timestamp)?;
    let profile = if smoke {
        NetworkProfile::Smoke
    } else {
        NetworkProfile::Production
    };
    let bare = ensure_fixture(git, profile, NetworkFixtureKind::DeepHistory)?;

    let worktree_no_graph =
        clone_deep_worktree(git, &bare, "revision-selection-worktree-no-graph")?;
    ensure_commit_graph_absent(&worktree_no_graph)?;
    push_log_author_n50(
        &mut report,
        hyperfine,
        git,
        grit,
        cfg,
        "no-graph",
        &worktree_no_graph,
    )?;

    let worktree_commit_graph =
        clone_deep_worktree(git, &bare, "revision-selection-worktree-commit-graph")?;
    write_git_commit_graph_reachable(git, &worktree_commit_graph)?;
    verify_git_commit_graph_in_repo(git, &worktree_commit_graph)?;
    push_log_author_n50(
        &mut report,
        hyperfine,
        git,
        grit,
        cfg,
        "commit-graph",
        &worktree_commit_graph,
    )?;

    Ok(report)
}
