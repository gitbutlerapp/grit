//! Run every grit-bench scenario group for one fixture tier (S / M / L).

use std::path::{Path, PathBuf};

use anyhow::Result;
use grit_lib::bench_fixture::synth::SynthTier;

use crate::bitmap_suite::{run_bitmaps_suite, BitmapRunConfig};
use crate::fixture::{DeepLeafDogfoodSpec, DenseLeafDogfoodSpec};
use crate::fixture_registry::{self, FixtureKind};
use crate::fsck_suite::run_fsck_suite;
use crate::history_suite::run_history_suite;
use crate::ignore_suite::run_ignore_suite;
use crate::local_clone_suite::run_local_clone_suite;
use crate::maintenance_suite::run_maintenance_suite;
use crate::network_fixture::NetworkProfile;
use crate::network_suite::run_network_suite;
use crate::odb_suite::{run_odb_backend_suite, OdbRunConfig};
use crate::parallel_fixture::ParallelProfile;
use crate::parallel_suite::run_parallel_suite;
use crate::refs_suite::{run_refs_suite, RefsRunConfig};
use crate::resolve_cat_suite::run_resolve_cat_suite;
use crate::revwalk_suite::run_revwalk_suite;
use crate::scenarios::{
    run_deep_leaf_worktree_suite, run_dense_leaf_worktree_suite, run_hot_path_suite, RunConfig,
};
use crate::schema::BenchReport;
use crate::worktree_suite::run_worktree_group;

/// Options for [`run_scenario_suite`].
#[derive(Debug, Clone, Copy)]
pub struct ScenarioSuiteOptions {
    pub tier: SynthTier,
    pub smoke: bool,
    pub offline: bool,
}

/// Merge multiple partial reports into one (metadata from the **first** group report).
pub fn merge_reports(reports: Vec<BenchReport>) -> BenchReport {
    let mut iter = reports.into_iter();
    let Some(mut merged) = iter.next() else {
        return BenchReport {
            schema_version: crate::schema::SCHEMA_VERSION,
            timestamp: String::new(),
            machine: crate::schema::MachineInfo {
                cpu_model: "unknown".into(),
                physical_cores: 0,
                logical_cores: 0,
                ram_bytes: 0,
                os: std::env::consts::OS.into(),
                kernel: "unknown".into(),
                scratch_filesystem: "unknown".into(),
                rustc_version: "unknown".into(),
                cargo_profile: crate::machine::grit_bench_cargo_profile(),
            },
            tools: crate::schema::ToolVersions {
                git: String::new(),
                grit: String::new(),
                grit_commit: None,
                hyperfine: None,
            },
            fixtures: Vec::new(),
            scenarios: Vec::new(),
        };
    };
    for report in iter {
        merged.scenarios.extend(report.scenarios);
        for fixture in report.fixtures {
            if !merged.fixtures.iter().any(|f| f.name == fixture.name) {
                merged.fixtures.push(fixture);
            }
        }
    }
    merged
}

/// Union fixture records from the registry prepare step and per-group reports.
pub fn merge_fixture_records(
    mut base: Vec<crate::schema::FixtureRecord>,
    from_reports: &[BenchReport],
) -> Vec<crate::schema::FixtureRecord> {
    for report in from_reports {
        for fixture in &report.fixtures {
            if !base.iter().any(|f| f.name == fixture.name) {
                base.push(fixture.clone());
            }
        }
    }
    base
}

fn hot_path_sizes(tier: SynthTier, smoke: bool) -> Vec<usize> {
    if smoke {
        return vec![100];
    }
    match tier {
        SynthTier::S => vec![1_000],
        SynthTier::M => vec![10_000],
        SynthTier::L => vec![10_000, 100_000],
    }
}

fn local_clone_files(tier: SynthTier, smoke: bool) -> usize {
    if smoke {
        200
    } else {
        match tier {
            SynthTier::S => 1_000,
            SynthTier::M => 5_000,
            SynthTier::L => 5_000,
        }
    }
}

/// Prepare registry fixtures required for this tier and mode.
pub fn prepare_suite_fixtures(
    git: &Path,
    options: ScenarioSuiteOptions,
) -> Result<Vec<crate::schema::FixtureRecord>> {
    let mut kinds = vec![FixtureKind::from_synth_tier(options.tier)];
    if !options.smoke {
        kinds.push(FixtureKind::GitGit);
    }
    fixture_registry::prepare_all(git, &kinds, options.offline)
}

/// Run all scenario groups and return one merged JSON report.
#[allow(clippy::too_many_arguments)]
pub fn run_scenario_suite(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    http_server: Option<&Path>,
    bench_exe: &Path,
    cfg: &RunConfig,
    options: ScenarioSuiteOptions,
    timestamp: time::OffsetDateTime,
) -> Result<BenchReport> {
    let fixture_records = prepare_suite_fixtures(git, options)?;
    let synth_kind = FixtureKind::from_synth_tier(options.tier);
    let mut reports = Vec::new();

    eprintln!(
        "Scenario suite: tier={:?} smoke={} offline={}",
        options.tier, options.smoke, options.offline
    );

    reports.push(run_worktree_group(
        hyperfine,
        git,
        grit,
        cfg,
        synth_kind,
        options.offline,
        timestamp,
    )?);

    let odb_cfg = OdbRunConfig {
        warmup: cfg.warmup,
        min_runs: cfg.min_runs,
        exact_runs: cfg.exact_runs,
        prepare_bin: cfg.prepare_bin.clone(),
    };
    reports.push(run_history_suite(
        hyperfine,
        git,
        grit,
        bench_exe,
        cfg,
        &odb_cfg,
        options.tier,
        options.smoke,
        timestamp,
    )?);

    let network_profile = if options.smoke {
        NetworkProfile::Smoke
    } else {
        NetworkProfile::Production
    };
    reports.push(run_network_suite(
        hyperfine,
        git,
        grit,
        http_server,
        cfg,
        network_profile,
        timestamp,
    )?);

    let parallel_profile = if options.smoke {
        ParallelProfile::Smoke
    } else {
        ParallelProfile::Production
    };
    reports.push(run_parallel_suite(
        hyperfine,
        git,
        grit,
        bench_exe,
        cfg,
        parallel_profile,
        timestamp,
    )?);

    reports.push(run_revwalk_suite(
        hyperfine,
        bench_exe,
        git,
        grit,
        cfg,
        options.smoke,
        timestamp,
    )?);
    reports.push(run_fsck_suite(
        hyperfine,
        git,
        grit,
        cfg,
        options.smoke,
        timestamp,
    )?);
    reports.push(run_maintenance_suite(
        hyperfine,
        git,
        grit,
        cfg,
        options.smoke,
        timestamp,
    )?);

    if options.smoke {
        reports.push(run_deep_leaf_worktree_suite(
            hyperfine,
            git,
            grit,
            cfg,
            DeepLeafDogfoodSpec::smoke(),
            timestamp,
        )?);
        reports.push(run_dense_leaf_worktree_suite(
            hyperfine,
            git,
            grit,
            cfg,
            DenseLeafDogfoodSpec::smoke(),
            timestamp,
        )?);
    }

    reports.push(run_resolve_cat_suite(hyperfine, git, grit, cfg, timestamp)?);
    reports.push(run_ignore_suite(hyperfine, git, grit, cfg, timestamp)?);
    reports.push(run_local_clone_suite(
        hyperfine,
        git,
        grit,
        bench_exe,
        cfg,
        local_clone_files(options.tier, options.smoke),
        timestamp,
    )?);
    reports.push(run_hot_path_suite(
        hyperfine,
        git,
        grit,
        cfg,
        &hot_path_sizes(options.tier, options.smoke),
        false,
        timestamp,
    )?);

    if !options.smoke {
        reports.push(run_odb_backend_suite(
            hyperfine, git, grit, bench_exe, &odb_cfg, timestamp,
        )?);
        let bitmap_cfg = BitmapRunConfig {
            warmup: cfg.warmup,
            min_runs: cfg.min_runs,
            exact_runs: cfg.exact_runs,
            prepare_bin: cfg.prepare_bin.clone(),
        };
        reports.push(run_bitmaps_suite(
            hyperfine,
            git,
            grit,
            bench_exe,
            &bitmap_cfg,
            None,
            timestamp,
        )?);
        let refs_cfg = RefsRunConfig {
            warmup: cfg.warmup,
            min_runs: cfg.min_runs,
            prepare_bin: cfg.prepare_bin.clone(),
        };
        reports.push(run_refs_suite(
            hyperfine, git, grit, bench_exe, &refs_cfg, timestamp,
        )?);
    }

    let fixtures = {
        let mut records = fixture_records;
        for report in &reports {
            records = merge_fixture_records(records, std::slice::from_ref(report));
        }
        records
    };
    let merged = merge_reports(reports);
    Ok(BenchReport { fixtures, ..merged })
}

/// Hostname for `scenarios-<host>.json` (short name when available).
pub fn bench_host_slug() -> String {
    std::env::var("GRIT_BENCH_HOST").unwrap_or_else(|_| {
        std::process::Command::new("hostname")
            .arg("-s")
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                } else {
                    None
                }
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "localhost".into())
    })
}

/// Default output path under `target/bench/`.
pub fn default_scenarios_output(repo_root: &Path) -> PathBuf {
    repo_root
        .join("target/bench")
        .join(format!("scenarios-{}.json", bench_host_slug()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        BenchReport, DriverKind, MachineInfo, ScenarioResult, TimingStats, ToolVersions,
    };

    fn stub_timing() -> TimingStats {
        TimingStats {
            mean_ms: 1.0,
            median_ms: 1.0,
            stddev_ms: 0.0,
            min_ms: 1.0,
            max_ms: 1.0,
            runs_ms: vec![1.0],
            peak_rss_bytes: None,
        }
    }

    fn stub_report(timestamp: &str, scenario_id: &str) -> BenchReport {
        BenchReport {
            schema_version: crate::schema::SCHEMA_VERSION,
            timestamp: timestamp.into(),
            machine: MachineInfo {
                cpu_model: "test-cpu".into(),
                physical_cores: 1,
                logical_cores: 1,
                ram_bytes: 0,
                os: "test".into(),
                kernel: "test".into(),
                scratch_filesystem: "test".into(),
                rustc_version: "test".into(),
                cargo_profile: "test".into(),
            },
            tools: ToolVersions {
                git: "git-test".into(),
                grit: "grit-test".into(),
                grit_commit: None,
                hyperfine: None,
            },
            fixtures: Vec::new(),
            scenarios: vec![ScenarioResult {
                id: scenario_id.into(),
                group: "g".into(),
                fixture: "f".into(),
                description: "d".into(),
                driver: DriverKind::Cli,
                git: stub_timing(),
                grit: stub_timing(),
                ratio: 1.0,
                grit_failure: None,
            }],
        }
    }

    #[test]
    fn merge_reports_keeps_first_metadata_and_unions_scenarios() {
        let first = stub_report("first-ts", "scenario-a");
        let second = stub_report("second-ts", "scenario-b");
        let merged = merge_reports(vec![first, second]);
        assert_eq!(merged.timestamp, "first-ts");
        assert_eq!(merged.machine.cpu_model, "test-cpu");
        assert_eq!(merged.tools.git, "git-test");
        assert_eq!(merged.scenarios.len(), 2);
        assert_eq!(merged.scenarios[0].id, "scenario-a");
        assert_eq!(merged.scenarios[1].id, "scenario-b");
    }
}
