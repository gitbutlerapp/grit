//! `grit-bench daily` runner: direct-spawn AB timing with optional smoke and filters.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use time::OffsetDateTime;

use super::areas;
use super::helpers::prepare_bench_argv;
use super::plan::{DailyHeadlinePlan, DailyPrepare, DailySetup};
use super::registry::{Area, HEADLINE_SCENARIOS};
use super::runner::{
    run_ab_bench, run_timed_via_helper, timing_stats_from_samples, AbBenchRequest, BenchArgv,
    RunLimits, RunPolicy,
};
use super::schema::{
    DailyBenchReport, DailyScenarioResult, DailyToolRun, DailyToolVersions, SCHEMA_TAG,
};
use crate::binary::{grit_source_commit, tool_version};
use crate::machine::{collect_machine_info, format_timestamp};

/// Options for [`run_daily`].
#[derive(Debug, Clone)]
pub struct DailyRunOptions {
    pub git: PathBuf,
    pub grit: PathBuf,
    pub baseline_grit: Option<PathBuf>,
    pub measure_helper: PathBuf,
    pub smoke: bool,
    pub offline: bool,
    pub only: Option<Vec<String>>,
    pub area: Option<Area>,
    pub output: Option<PathBuf>,
}

fn smoke_policy() -> RunPolicy {
    RunPolicy {
        warmups_per_tool: 0,
        min_runs_per_tool: 1,
        max_runs_per_tool: 1,
        min_total_timed_duration: std::time::Duration::from_millis(0),
        scenario_time_budget: std::time::Duration::from_secs(600),
    }
}

fn default_policy() -> RunPolicy {
    RunPolicy::default()
}

fn tool_run(stats: &crate::schema::TimingStats, runs: u32) -> DailyToolRun {
    DailyToolRun {
        median_wall_ms: stats.median_ms,
        peak_rss_bytes: stats.peak_rss_bytes.unwrap_or(0),
        runs: Some(runs),
    }
}

fn ratio(grit_ms: f64, git_ms: f64) -> f64 {
    if git_ms <= 0.0 {
        0.0
    } else {
        grit_ms / git_ms
    }
}

fn pending_row(spec: &super::registry::HeadlineScenario) -> DailyScenarioResult {
    DailyScenarioResult {
        id: spec.id.to_string(),
        area: spec.area,
        headline: spec.headline,
        name: spec.name.map(str::to_string),
        description: spec.description.to_string(),
        git: DailyToolRun {
            median_wall_ms: 0.0,
            peak_rss_bytes: 0,
            runs: None,
        },
        grit: DailyToolRun {
            median_wall_ms: 0.0,
            peak_rss_bytes: 0,
            runs: None,
        },
        ratio: 0.0,
        baseline_grit: None,
        baseline_grit_ratio: None,
        pending: true,
    }
}

fn baseline_grit_argv(grit_cmd: &BenchArgv, grit: &Path, baseline: &Path) -> BenchArgv {
    if grit_cmd.program == grit {
        return BenchArgv {
            program: baseline.to_path_buf(),
            args: grit_cmd.args.clone(),
        };
    }
    let grit_q = grit.display().to_string();
    let base_q = baseline.display().to_string();
    let mut args = grit_cmd.args.clone();
    if let Some(script) = args.last_mut() {
        *script = script.replace(&grit_q, &base_q);
    }
    BenchArgv {
        program: grit_cmd.program.clone(),
        args,
    }
}

fn select_plans(opts: &DailyRunOptions) -> Vec<DailyHeadlinePlan> {
    let mut plans = areas::all_headline_plans();
    if opts.smoke {
        plans.retain(|p| !p.pending);
    }
    if let Some(area) = opts.area {
        plans.retain(|p| p.spec.area == area);
    }
    if let Some(only) = &opts.only {
        let set: std::collections::HashSet<_> = only.iter().map(String::as_str).collect();
        plans.retain(|p| set.contains(p.spec.id));
    }
    plans
}

fn prepare_argv(prepare_bin: &Path, git: &Path, prepare: &DailyPrepare) -> Option<BenchArgv> {
    match prepare {
        DailyPrepare::None => None,
        DailyPrepare::Hook(kind) => Some(prepare_bench_argv(prepare_bin, git, *kind)),
    }
}

fn run_baseline_samples(
    cwd: &Path,
    cmd: &BenchArgv,
    policy: &RunPolicy,
    limits: &RunLimits,
    helper: &Path,
) -> Result<DailyToolRun> {
    let mut samples = Vec::new();
    for _ in 0..policy.min_runs_per_tool.max(1) {
        let sample = run_timed_via_helper(cwd, cmd, limits, helper)?;
        samples.push(sample);
    }
    let stats = timing_stats_from_samples(&samples);
    Ok(tool_run(&stats, samples.len() as u32))
}

/// Execute headline scenarios and write a daily census JSON report.
pub fn run_daily(opts: DailyRunOptions, timestamp: OffsetDateTime) -> Result<DailyBenchReport> {
    let policy = if opts.smoke {
        smoke_policy()
    } else {
        default_policy()
    };
    let limits = RunLimits::default();
    let setup = DailySetup {
        git: &opts.git,
        grit: &opts.grit,
        smoke: opts.smoke,
        offline: opts.offline,
    };

    let mut scenarios = Vec::new();
    for plan in select_plans(&opts) {
        if plan.pending {
            scenarios.push(pending_row(plan.spec));
            continue;
        }
        let headline_ctx = (plan.setup)(&setup)
            .with_context(|| format!("setup daily scenario {}", plan.spec.id))?;
        let repo = headline_ctx.repo.clone();
        std::env::set_var("GRIT_BENCH_CAMPAIGN_CWD", &repo);
        let git_cmd = (plan.build_git)(&headline_ctx);
        let grit_cmd = (plan.build_grit)(&headline_ctx);
        let prepare = prepare_argv(&opts.measure_helper, &opts.git, &plan.prepare);

        let outcome = run_ab_bench(&AbBenchRequest {
            cwd: &repo,
            git: &git_cmd,
            grit: &grit_cmd,
            prepare: prepare.as_ref(),
            policy: &policy,
            limits: &limits,
            measure_helper: &opts.measure_helper,
            correctness: None,
        })
        .with_context(|| format!("benchmark {}", plan.spec.id))?;

        let git_run = tool_run(&outcome.git.stats, outcome.git.samples.len() as u32);
        let grit_run = tool_run(&outcome.grit.stats, outcome.grit.samples.len() as u32);
        let row_ratio = ratio(grit_run.median_wall_ms, git_run.median_wall_ms);

        let (baseline_grit, baseline_grit_ratio) =
            if let Some(base_bin) = opts.baseline_grit.as_ref().filter(|_| plan.spec.headline) {
                let base_cmd = baseline_grit_argv(&grit_cmd, &opts.grit, base_bin);
                let base_run =
                    run_baseline_samples(&repo, &base_cmd, &policy, &limits, &opts.measure_helper)?;
                let base_ratio = ratio(base_run.median_wall_ms, git_run.median_wall_ms);
                (Some(base_run), Some(base_ratio))
            } else {
                (None, None)
            };

        scenarios.push(DailyScenarioResult {
            id: plan.spec.id.to_string(),
            area: plan.spec.area,
            headline: plan.spec.headline,
            name: plan.spec.name.map(str::to_string),
            description: plan.spec.description.to_string(),
            git: git_run,
            grit: grit_run,
            ratio: row_ratio,
            baseline_grit,
            baseline_grit_ratio,
            pending: false,
        });
    }

    if !opts.smoke {
        for spec in HEADLINE_SCENARIOS.iter().filter(|s| {
            matches!(
                s.id,
                "fetch/incremental@git-git"
                    | "pull/fast-forward@git-git"
                    | "push/incremental@git-git"
                    | "clone/local@synth-10k"
            )
        }) {
            if scenarios.iter().any(|r| r.id == spec.id) {
                continue;
            }
            scenarios.push(pending_row(spec));
        }
    }

    scenarios.sort_by(|a, b| a.id.cmp(&b.id));

    let baseline_version = opts.baseline_grit.as_ref().map(|p| tool_version(p));
    let baseline_commit = opts
        .baseline_grit
        .as_ref()
        .and_then(|_| grit_source_commit());

    let report = DailyBenchReport {
        schema: SCHEMA_TAG.to_string(),
        generated_at: format_timestamp(timestamp),
        machine: collect_machine_info(&crate::fixture::scratch_dir())?,
        tools: DailyToolVersions {
            git: tool_version(&opts.git),
            grit: tool_version(&opts.grit),
            grit_commit: grit_source_commit(),
            baseline_grit: baseline_version,
            baseline_grit_commit: baseline_commit,
        },
        scenarios,
    };

    if let Some(path) = &opts.output {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
        }
        let json = serde_json::to_string_pretty(&report).context("serialize daily report")?;
        std::fs::write(path, json).with_context(|| format!("write {}", path.display()))?;
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_filter_narrows_plans() {
        let opts = DailyRunOptions {
            git: PathBuf::from("/usr/bin/git"),
            grit: PathBuf::from("/usr/bin/grit"),
            baseline_grit: None,
            measure_helper: PathBuf::from("/usr/bin/grit-bench"),
            smoke: true,
            offline: true,
            only: Some(vec!["status/clean@synth-10k".into()]),
            area: None,
            output: None,
        };
        let plans = select_plans(&opts);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].spec.id, "status/clean@synth-10k");
    }

    #[test]
    fn area_filter_selects_worktree() {
        let opts = DailyRunOptions {
            git: PathBuf::from("/usr/bin/git"),
            grit: PathBuf::from("/usr/bin/grit"),
            baseline_grit: None,
            measure_helper: PathBuf::from("/usr/bin/grit-bench"),
            smoke: true,
            offline: true,
            only: None,
            area: Some(Area::Worktree),
            output: None,
        };
        let plans = select_plans(&opts);
        assert!(plans.iter().all(|p| p.spec.area == Area::Worktree));
        assert!(plans.len() >= 5);
    }
}
