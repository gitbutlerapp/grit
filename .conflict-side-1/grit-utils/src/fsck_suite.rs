//! `grit fsck` benchmark scenarios vs system `git fsck`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use time::OffsetDateTime;

use crate::binary::{grit_source_commit, tool_version};
use crate::fixture::{remove_dir_robust, scratch_dir};
use crate::machine::{collect_machine_info, format_timestamp};
use crate::network_fixture::{ensure_fixture, NetworkFixtureKind, NetworkProfile};
use crate::scenarios::{run_scenario, Driver, RunConfig, Scenario};
use crate::schema::{BenchReport, ToolVersions, SCHEMA_VERSION};

fn git_env(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
}

fn clone_deep_worktree(git: &Path, bare: &Path) -> Result<PathBuf> {
    let dest = scratch_dir().join("fsck-medium-worktree");
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
    let out = cmd.output().context("git clone deep-history for fsck")?;
    if !out.status.success() {
        anyhow::bail!("git clone failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(dest)
}

/// Fsck on a medium deep-history worktree vs `git fsck --no-dangling`.
pub fn run_fsck_suite(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    smoke: bool,
    timestamp: OffsetDateTime,
) -> Result<BenchReport> {
    let profile = if smoke {
        NetworkProfile::Smoke
    } else {
        NetworkProfile::Production
    };
    let bare = ensure_fixture(git, profile, NetworkFixtureKind::DeepHistory)?;
    let worktree = clone_deep_worktree(git, &bare)?;
    let scenario = run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: "fsck-medium".into(),
            group: "maintenance".into(),
            fixture: bare.display().to_string(),
            description: "fsck on deep-history worktree (medium fixture)".into(),
            grit_argv: vec!["fsck".into(), "--no-dangling".into()],
            git_argv: vec!["fsck".into(), "--no-dangling".into()],
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &worktree,
    )?;

    let scratch = scratch_dir();
    Ok(BenchReport {
        schema_version: SCHEMA_VERSION,
        timestamp: format_timestamp(timestamp),
        machine: collect_machine_info(&scratch)?,
        tools: ToolVersions {
            git: tool_version(git),
            grit: tool_version(grit),
            grit_commit: grit_source_commit(),
        },
        fixtures: vec![],
        scenarios: vec![scenario],
    })
}
