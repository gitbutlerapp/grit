//! Campaign scorecard: real fixtures × core CLI matrix + network scenarios.

use std::path::Path;

use anyhow::Result;
use time::OffsetDateTime;

use crate::bitmap_suite::{run_bitmaps_suite, BitmapRunConfig};
use crate::campaign_matrix::{run_fixture_cli_matrix, CampaignFixture};
use crate::chromium_synth::{ensure_chromium_synthetic, ChromiumSynthSpec};
use crate::network_fixture::NetworkProfile;
use crate::network_suite::run_network_suite;
use crate::real_repo_fixture::{
    ensure_git_git_bitmaps, ensure_linux_bare, ensure_worktree_clone, linux_fixture_enabled,
    scratch_root, DEFAULT_GIT_GIT_TAG, DEFAULT_LINUX_TAG,
};
use crate::scenarios::{build_report, RunConfig};
use crate::schema::{BenchReport, ScenarioResult, SCHEMA_VERSION};
use crate::serve_request::ensure_serve_clone_request;

/// Options for a campaign run.
#[derive(Debug, Clone)]
pub struct CampaignOptions {
    pub smoke: bool,
    pub with_linux: bool,
    pub include_network: bool,
}

fn pinned_git_git_tag() -> String {
    std::env::var("GRIT_BENCH_GIT_GIT_TAG").unwrap_or_else(|_| DEFAULT_GIT_GIT_TAG.to_owned())
}

/// Run the full campaign and return a merged schema-v1 report.
pub fn run_campaign_suite(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    bench_exe: &Path,
    cfg: &RunConfig,
    opts: &CampaignOptions,
    timestamp: OffsetDateTime,
) -> Result<BenchReport> {
    let mut scenarios: Vec<ScenarioResult> = Vec::new();
    let git_tag = pinned_git_git_tag();

    let git_bare = ensure_git_git_bitmaps(git)?;
    let git_worktree = ensure_worktree_clone(git, &git_bare, "git-dot-git-worktree", &git_tag)?;
    scenarios.extend(run_fixture_cli_matrix(
        hyperfine,
        git,
        grit,
        bench_exe,
        cfg,
        &CampaignFixture {
            id: "git-dot-git",
            label: "git-dot-git".into(),
            bare: git_bare.clone(),
            worktree: git_worktree,
        },
        opts.smoke,
    )?);
    scenarios.extend(prefix_bitmap_report(
        run_bitmaps_suite(
            hyperfine,
            git,
            grit,
            bench_exe,
            &BitmapRunConfig {
                warmup: cfg.warmup,
                min_runs: cfg.min_runs,
                prepare_bin: cfg.prepare_bin.clone(),
            },
            Some(&git_bare),
            timestamp,
        )?,
        "git-dot-git",
    ));

    let synth_spec = if opts.smoke {
        ChromiumSynthSpec::smoke()
    } else {
        ChromiumSynthSpec::production()
    };
    let synth_bare = ensure_chromium_synthetic(git, synth_spec)?;
    ensure_serve_clone_request(&synth_bare)?;
    let synth_worktree = ensure_worktree_clone(
        git,
        &synth_bare,
        &format!("chromium-synth-{}-worktree", synth_spec.cache_key()),
        &synth_spec.cache_key(),
    )?;
    scenarios.extend(run_fixture_cli_matrix(
        hyperfine,
        git,
        grit,
        bench_exe,
        cfg,
        &CampaignFixture {
            id: "chromium-synth",
            label: format!("chromium-synth-{}", synth_spec.cache_key()),
            bare: synth_bare.clone(),
            worktree: synth_worktree,
        },
        opts.smoke,
    )?);
    if linux_fixture_enabled(opts.with_linux) {
        let linux_tag =
            std::env::var("GRIT_BENCH_LINUX_TAG").unwrap_or_else(|_| DEFAULT_LINUX_TAG.to_owned());
        let linux_bare = ensure_linux_bare(git)?;
        let linux_worktree = ensure_worktree_clone(git, &linux_bare, "linux-worktree", &linux_tag)?;
        scenarios.extend(run_fixture_cli_matrix(
            hyperfine,
            git,
            grit,
            bench_exe,
            cfg,
            &CampaignFixture {
                id: "linux",
                label: "linux".into(),
                bare: linux_bare,
                worktree: linux_worktree,
            },
            opts.smoke,
        )?);
    }

    if opts.include_network {
        let profile = if opts.smoke {
            NetworkProfile::Smoke
        } else {
            NetworkProfile::Production
        };
        let network = run_network_suite(hyperfine, git, grit, None, cfg, profile, timestamp)?;
        scenarios.extend(network.scenarios);
    }

    let mut report = build_report(git, grit, timestamp, scenarios);
    report.machine = crate::machine::collect_machine_info(&scratch_root())?;
    report.schema_version = SCHEMA_VERSION;
    Ok(report)
}

fn prefix_bitmap_report(mut report: BenchReport, prefix: &str) -> Vec<ScenarioResult> {
    for scenario in &mut report.scenarios {
        scenario.id = format!("{prefix}/{}", strip_git_suffix(&scenario.id));
        scenario.fixture = prefix.into();
    }
    report.scenarios
}

fn strip_git_suffix(id: &str) -> &str {
    id.strip_suffix("-git.git").unwrap_or(id)
}
