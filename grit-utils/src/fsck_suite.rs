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
use crate::schema::{BenchReport, ScenarioResult, ToolVersions, SCHEMA_VERSION};

fn git_env(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
}

pub(crate) fn clone_deep_worktree(git: &Path, bare: &Path) -> Result<PathBuf> {
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

/// Scenario row for fsck on the deep-history medium fixture.
pub fn fsck_medium_scenario(bare: &Path) -> Scenario {
    Scenario {
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
    }
}

/// Largest grit/git peak-RSS ratio the fsck suite accepts.
///
/// Pack verification streams objects and keeps only compact per-object facts, so grit's
/// peak should stay in git's range; a run above this ratio is a memory regression.
pub const FSCK_MAX_PEAK_RSS_RATIO: f64 = 2.0;

/// Extra peak RSS (bytes) grit may use over git regardless of the ratio: on small
/// fixtures both peaks are dominated by fixed per-process and per-thread overhead, and
/// git's own peak varies by tens of MiB between runs.
pub const FSCK_PEAK_RSS_SLACK_BYTES: u64 = 64 * 1024 * 1024;

/// Fail when grit's fsck peak RSS exceeds both [`FSCK_MAX_PEAK_RSS_RATIO`] times git's
/// and git's plus [`FSCK_PEAK_RSS_SLACK_BYTES`].
///
/// # Errors
///
/// Returns an error naming both peaks when grit's exceeds the limit. Missing
/// measurements are not an error.
pub fn check_fsck_peak_rss(scenario: &ScenarioResult) -> Result<()> {
    let (Some(grit), Some(git)) = (scenario.grit.peak_rss_bytes, scenario.git.peak_rss_bytes)
    else {
        return Ok(());
    };
    let by_ratio = (git as f64 * FSCK_MAX_PEAK_RSS_RATIO) as u64;
    let limit = by_ratio.max(git.saturating_add(FSCK_PEAK_RSS_SLACK_BYTES));
    if grit > limit {
        const MIB: u64 = 1024 * 1024;
        anyhow::bail!(
            "{}: grit fsck peak RSS {} MiB exceeds the {} MiB limit (git: {} MiB)",
            scenario.id,
            grit / MIB,
            limit / MIB,
            git / MIB,
        );
    }
    Ok(())
}

/// Fsck on a medium deep-history worktree vs `git fsck --no-dangling`.
///
/// # Errors
///
/// Fails on fixture or hyperfine errors, and when grit's peak RSS exceeds
/// [`FSCK_MAX_PEAK_RSS_RATIO`] times git's (see [`check_fsck_peak_rss`]).
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
    let scenario = fsck_medium_scenario(&bare);
    let scenario = run_scenario(hyperfine, git, grit, cfg, &scenario, &worktree)?;
    check_fsck_peak_rss(&scenario)?;

    let scratch = scratch_dir();
    Ok(BenchReport {
        schema_version: SCHEMA_VERSION,
        timestamp: format_timestamp(timestamp),
        machine: collect_machine_info(&scratch)?,
        tools: ToolVersions {
            git: tool_version(git),
            grit: tool_version(grit),
            grit_commit: grit_source_commit(),
            hyperfine: None,
        },
        fixtures: vec![],
        scenarios: vec![scenario],
    })
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::schema::DriverKind;
    use crate::stats::failed_timing_stats;

    const MIB: u64 = 1024 * 1024;

    fn scenario(git_rss: Option<u64>, grit_rss: Option<u64>) -> ScenarioResult {
        let mut git = failed_timing_stats();
        git.peak_rss_bytes = git_rss;
        let mut grit = failed_timing_stats();
        grit.peak_rss_bytes = grit_rss;
        ScenarioResult {
            id: "fsck-medium".into(),
            group: "maintenance".into(),
            fixture: "fixture".into(),
            description: "fsck".into(),
            driver: DriverKind::Cli,
            git,
            grit,
            ratio: 1.0,
            grit_failure: None,
        }
    }

    #[test]
    fn peak_rss_guard_accepts_git_range_and_rejects_regressions() {
        // cargo: git 232 MiB; this branch 277 MiB, before the fix 2392 MiB.
        assert!(check_fsck_peak_rss(&scenario(Some(232 * MIB), Some(277 * MIB))).is_ok());
        assert!(check_fsck_peak_rss(&scenario(Some(232 * MIB), Some(464 * MIB))).is_ok());
        let err = check_fsck_peak_rss(&scenario(Some(232 * MIB), Some(2392 * MIB)))
            .expect_err("10x git's peak must fail");
        assert!(err.to_string().contains("2392 MiB"), "{err}");
        assert!(err.to_string().contains("464 MiB limit"), "{err}");
        assert!(check_fsck_peak_rss(&scenario(Some(232 * MIB), Some(465 * MIB))).is_err());
    }

    #[test]
    fn peak_rss_guard_allows_fixed_overhead_on_small_fixtures() {
        // fsck-medium: git's peak varies between ~30 and ~44 MiB; grit sits near 75 MiB.
        assert!(check_fsck_peak_rss(&scenario(Some(30 * MIB), Some(75 * MIB))).is_ok());
        assert!(check_fsck_peak_rss(&scenario(Some(30 * MIB), Some(94 * MIB))).is_ok());
        assert!(check_fsck_peak_rss(&scenario(Some(30 * MIB), Some(95 * MIB))).is_err());
        assert!(check_fsck_peak_rss(&scenario(Some(0), Some(64 * MIB))).is_ok());
        assert!(check_fsck_peak_rss(&scenario(Some(0), Some(65 * MIB))).is_err());
    }

    #[test]
    fn peak_rss_guard_skips_missing_measurements() {
        assert!(check_fsck_peak_rss(&scenario(None, Some(4096 * MIB))).is_ok());
        assert!(check_fsck_peak_rss(&scenario(Some(100 * MIB), None)).is_ok());
    }
}
