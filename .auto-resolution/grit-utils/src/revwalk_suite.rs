//! `grit revs` / `grit merge-base` benchmark scenarios.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use time::OffsetDateTime;

use crate::bench_env::isolated_env_prefix;
use crate::binary::{grit_source_commit, tool_version};
use crate::fixture::{remove_dir_robust, scratch_dir};
use crate::hot_path_fixture::{setup_pick_merge_fixture, HotPathRepoSpec};
use crate::machine::{collect_machine_info, format_timestamp};
use crate::network_fixture::{ensure_fixture, NetworkFixtureKind, NetworkProfile};
use crate::odb_suite::{run_odb_scenario, OdbRunConfig};
use crate::scenarios::{run_scenario, Driver, RunConfig, Scenario};
use crate::schema::{BenchReport, ToolVersions, SCHEMA_VERSION};

fn git_env(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
}

fn lib_op_argv(sub: &str, repo: &Path, extra: &[String]) -> Vec<String> {
    let mut v = vec!["lib-op".into(), sub.into(), repo.display().to_string()];
    v.extend_from_slice(extra);
    v
}

fn odb_cfg_from_run(cfg: &RunConfig) -> OdbRunConfig {
    OdbRunConfig {
        warmup: cfg.warmup,
        min_runs: cfg.min_runs,
        exact_runs: cfg.exact_runs,
        prepare_bin: cfg.prepare_bin.clone(),
    }
}

fn clone_deep_worktree(git: &Path, bare: &Path) -> Result<PathBuf> {
    let dest = scratch_dir().join("deep-history-worktree");
    remove_dir_robust(&dest);
    let mut cmd = Command::new(git);
    let bare_str = bare
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("bare fixture path is not UTF-8"))?;
    let dest_str = dest
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("worktree path is not UTF-8"))?;
    cmd.args(["clone", bare_str, dest_str]);
    git_env(&mut cmd);
    let out = cmd.output().context("git clone deep-history")?;
    if !out.status.success() {
        anyhow::bail!("git clone failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(dest)
}

/// Rev-list count and merge-base scenarios vs system git.
pub fn run_revwalk_suite(
    hyperfine: &Path,
    bench_exe: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    smoke: bool,
    timestamp: OffsetDateTime,
) -> Result<BenchReport> {
    let odb_cfg = odb_cfg_from_run(cfg);
    let profile = if smoke {
        NetworkProfile::Smoke
    } else {
        NetworkProfile::Production
    };
    let bare = ensure_fixture(git, profile, NetworkFixtureKind::DeepHistory)?;
    let worktree = clone_deep_worktree(git, &bare)?;
    let mut scenarios = Vec::new();

    let env_prefix = cfg.isolated_config.then(isolated_env_prefix);
    for (id_suffix, prepare) in [
        ("no-graph", None),
        ("commit-graph", Some("commit-graph write")),
    ] {
        if let Some(prep) = prepare {
            let mut cmd = Command::new(git);
            cmd.args(["commit-graph", "write", "--reachable"])
                .current_dir(&worktree);
            git_env(&mut cmd);
            let out = cmd.output().context("commit-graph write")?;
            if !out.status.success() {
                anyhow::bail!(
                    "commit-graph write failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            let _ = prep;
        }
        scenarios.push(run_odb_scenario(
            hyperfine,
            bench_exe,
            git,
            &odb_cfg,
            &Scenario {
                id: format!("revs-count-author-{id_suffix}"),
                group: "revs".into(),
                fixture: bare.display().to_string(),
                description: format!(
                    "rev-list --count --author=. HEAD on deep-history ({id_suffix})"
                ),
                grit_argv: lib_op_argv("rev-list-count-author-head", &worktree, &[]),
                git_argv: vec![
                    "rev-list".into(),
                    "--count".into(),
                    "--author=.".into(),
                    "HEAD".into(),
                ],
                driver: Driver::Lib,
                prepare_kind: None,
                grit_via_shell: false,
                git_via_shell: false,
            },
            &worktree,
        )?);
        scenarios.push(run_odb_scenario(
            hyperfine,
            bench_exe,
            git,
            &odb_cfg,
            &Scenario {
                id: format!("revs-author-n50-{id_suffix}"),
                group: "revs".into(),
                fixture: bare.display().to_string(),
                description: format!(
                    "rev-list -n 50 --author=. HEAD on deep-history ({id_suffix})"
                ),
                grit_argv: lib_op_argv(
                    "rev-list-author-n-head",
                    &worktree,
                    &["--max-count".into(), "50".into()],
                ),
                git_argv: vec![
                    "rev-list".into(),
                    "-n".into(),
                    "50".into(),
                    "--author=.".into(),
                    "HEAD".into(),
                ],
                driver: Driver::Lib,
                prepare_kind: None,
                grit_via_shell: false,
                git_via_shell: false,
            },
            &worktree,
        )?);
        scenarios.push(run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: format!("revs-count-deep-{id_suffix}"),
                group: "revs".into(),
                fixture: bare.display().to_string(),
                description: format!("revs --count HEAD on deep-history fixture ({id_suffix})"),
                grit_argv: vec!["revs".into(), "--count".into(), "HEAD".into()],
                git_argv: vec!["rev-list".into(), "--count".into(), "HEAD".into()],
                driver: Driver::Cli,
                prepare_kind: None,
                grit_via_shell: false,
                git_via_shell: false,
            },
            &worktree,
        )?);
    }

    let spec = HotPathRepoSpec::for_size(if smoke { 80 } else { 10_000 });
    let (pick_repo, meta) = setup_pick_merge_fixture(git, spec, 4)?;
    scenarios.push(run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: "merge-base-pick-fixture".into(),
            group: "merge-base".into(),
            fixture: format!("synthetic-{}", spec.file_count),
            description: "merge-base main topic on pick/merge fixture".into(),
            grit_argv: vec![
                "merge-base".into(),
                meta.main_tip.clone(),
                meta.pick_commit.clone(),
            ],
            git_argv: vec![
                "merge-base".into(),
                meta.main_tip.clone(),
                meta.pick_commit.clone(),
            ],
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &pick_repo,
    )?);

    let _ = env_prefix;
    Ok(BenchReport {
        schema_version: SCHEMA_VERSION,
        timestamp: format_timestamp(timestamp),
        machine: collect_machine_info(&scratch_dir())?,
        tools: ToolVersions {
            git: tool_version(git),
            grit: tool_version(grit),
            grit_commit: grit_source_commit(),
        },
        fixtures: vec![],
        scenarios,
    })
}
