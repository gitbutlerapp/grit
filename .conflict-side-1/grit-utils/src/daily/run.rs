//! `grit-bench daily` runner: direct-spawn AB timing with optional smoke and filters.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use time::OffsetDateTime;

use std::collections::HashSet;

use super::areas;
use super::coverage::all_registry_scenarios;
use super::helpers::{pin_daily_scratch_root, prepare_bench_argv};
use super::plan::{DailyHeadlinePlan, DailyPrepare, DailySetup};
use super::registry::{Area, ScenarioSpec};
use super::remote_fixture::refresh_grit_http_server;
use super::runner::{
    run_ab_bench, run_prepare, run_timed_via_helper, timing_stats_from_samples, AbBenchRequest,
    BenchArgv, RunLimits, RunPolicy,
};
use super::schema::{
    DailyBenchReport, DailyScenarioResult, DailyToolRun, DailyToolVersions, SCHEMA_TAG,
};
use crate::binary::{grit_source_commit, tool_version};
use crate::capped_run::TerminationKind;
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
    pub areas: Option<Vec<Area>>,
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

fn pending_row(spec: &ScenarioSpec) -> DailyScenarioResult {
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

fn filter_plans(
    mut plans: Vec<DailyHeadlinePlan>,
    opts: &DailyRunOptions,
) -> Vec<DailyHeadlinePlan> {
    if opts.smoke && opts.only.is_none() && opts.areas.is_none() {
        plans.retain(|p| p.spec.headline && !p.pending);
    }
    if let Some(areas) = &opts.areas {
        let set: HashSet<_> = areas.iter().copied().collect();
        plans.retain(|p| set.contains(&p.spec.area));
    }
    if let Some(only) = &opts.only {
        let set: HashSet<_> = only.iter().map(String::as_str).collect();
        plans.retain(|p| set.contains(p.spec.id));
    }
    plans
}

fn select_plans(opts: &DailyRunOptions) -> Vec<DailyHeadlinePlan> {
    filter_plans(areas::all_daily_plans(), opts)
}

/// Reject `--only` ids that are unknown, filtered out, or registered without an executable plan.
fn validate_only_ids(
    only: &[String],
    selected_ids: &HashSet<&str>,
    registry_ids: &HashSet<&str>,
    executable_ids: &HashSet<&str>,
) -> Result<()> {
    for id in only {
        if selected_ids.contains(id.as_str()) {
            continue;
        }
        if registry_ids.contains(id.as_str()) && !executable_ids.contains(id.as_str()) {
            anyhow::bail!(
                "daily scenario `{id}` is in the registry but has no executable plan yet"
            );
        }
        anyhow::bail!("daily scenario `{id}` is not available for this run");
    }
    Ok(())
}

fn validate_requested_scenarios(
    opts: &DailyRunOptions,
    selected: &[DailyHeadlinePlan],
) -> Result<()> {
    let Some(only) = &opts.only else {
        return Ok(());
    };
    let executable: HashSet<&str> = areas::all_daily_plans().iter().map(|p| p.spec.id).collect();
    let registry: HashSet<&str> = all_registry_scenarios().iter().map(|s| s.id).collect();
    let selected_ids: HashSet<&str> = selected.iter().map(|p| p.spec.id).collect();
    validate_only_ids(only, &selected_ids, &registry, &executable)
}

fn prepare_argv(prepare_bin: &Path, git: &Path, prepare: &DailyPrepare) -> Option<BenchArgv> {
    match prepare {
        DailyPrepare::None => None,
        DailyPrepare::Hook(kind) => Some(prepare_bench_argv(prepare_bin, git, *kind)),
        DailyPrepare::Shell(argv) => Some(argv.clone()),
        DailyPrepare::Network(sub) => {
            let bare = std::env::var("GRIT_BENCH_DAILY_NETWORK_BARE").ok()?;
            Some(BenchArgv::new(
                prepare_bin,
                [sub, "--git", &git.display().to_string(), "--bare", &bare],
            ))
        }
    }
}

fn run_baseline_samples(
    cwd: &Path,
    cmd: &BenchArgv,
    prepare: Option<&BenchArgv>,
    policy: &RunPolicy,
    limits: &RunLimits,
    helper: &Path,
) -> Result<DailyToolRun> {
    let mut samples = Vec::new();
    for _ in 0..policy.min_runs_per_tool.max(1) {
        run_prepare(cwd, prepare)?;
        let sample = run_timed_via_helper(cwd, cmd, limits, helper)?;
        if sample.termination != TerminationKind::Success {
            anyhow::bail!("baseline grit timed run failed: {:?}", sample.termination);
        }
        samples.push(sample);
    }
    let stats = timing_stats_from_samples(&samples);
    let run = tool_run(&stats, samples.len() as u32);
    if run.peak_rss_bytes == 0 {
        anyhow::bail!("baseline grit missing peak_rss_bytes");
    }
    Ok(run)
}

/// Execute headline scenarios and write a daily census JSON report.
pub fn run_daily(opts: DailyRunOptions, timestamp: OffsetDateTime) -> Result<DailyBenchReport> {
    pin_daily_scratch_root();
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

    let plans = select_plans(&opts);
    validate_requested_scenarios(&opts, &plans)?;

    let mut scenarios = Vec::new();
    for plan in plans {
        if plan.pending {
            scenarios.push(pending_row(plan.spec));
            continue;
        }
        let mut headline_ctx = (plan.setup)(&setup)
            .with_context(|| format!("setup daily scenario {}", plan.spec.id))?;
        let repo = headline_ctx.repo.clone();
        std::env::set_var("GRIT_BENCH_CAMPAIGN_CWD", &repo);
        if let Some(remote) = &headline_ctx.remote {
            std::env::set_var("GRIT_BENCH_DAILY_NETWORK_BARE", &remote.bare);
        }
        if let Some(check) = plan.correctness {
            check(&headline_ctx)
                .with_context(|| format!("correctness check before timing {}", plan.spec.id))?;
        }
        refresh_grit_http_server(&mut headline_ctx)
            .with_context(|| format!("refresh HTTP server for {}", plan.spec.id))?;
        let git_cmd = (plan.build_git)(&headline_ctx);
        let grit_cmd = (plan.build_grit)(&headline_ctx);
        let prepare_spec = (plan.prepare)(&headline_ctx);
        let prepare = prepare_argv(&opts.measure_helper, &opts.git, &prepare_spec);

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

        let git_run = tool_run(&outcome.git.stats, outcome.git.stats.runs_ms.len() as u32);
        let grit_run = tool_run(&outcome.grit.stats, outcome.grit.stats.runs_ms.len() as u32);
        let row_ratio = ratio(grit_run.median_wall_ms, git_run.median_wall_ms);

        let (baseline_grit, baseline_grit_ratio) =
            if let Some(base_bin) = opts.baseline_grit.as_ref().filter(|_| plan.spec.headline) {
                let base_cmd = baseline_grit_argv(&grit_cmd, &opts.grit, base_bin);
                let base_run = run_baseline_samples(
                    &repo,
                    &base_cmd,
                    prepare.as_ref(),
                    &policy,
                    &limits,
                    &opts.measure_helper,
                )?;
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

    scenarios.sort_by(|a, b| a.id.cmp(&b.id));

    let baseline_version = opts.baseline_grit.as_ref().map(|p| tool_version(p));
    let baseline_commit = opts.baseline_grit.as_ref().and_then(|base| {
        if base == &opts.grit {
            grit_source_commit()
        } else {
            None
        }
    });

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
            areas: None,
            output: None,
        };
        let plans = select_plans(&opts);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].spec.id, "status/clean@synth-10k");
    }

    #[test]
    fn smoke_with_areas_keeps_pending_gap_rows() {
        use crate::daily::helpers::{prepare_none, setup_small};
        use crate::daily::plan::{DailyHeadlineContext, DailyHeadlinePlan};
        use crate::daily::registry::ScenarioSpec;
        use crate::daily::runner::BenchArgv;

        static PENDING_GAP: ScenarioSpec = ScenarioSpec {
            id: "test/pending-gap@smoke-fixture",
            area: Area::Revwalk,
            headline: false,
            name: None,
            description: "Synthetic pending matrix gap row for daily plan filtering tests",
        };
        static EXECUTABLE: ScenarioSpec = ScenarioSpec {
            id: "test/executable@smoke-fixture",
            area: Area::Revwalk,
            headline: true,
            name: None,
            description: "Synthetic executable revwalk row for daily plan filtering tests",
        };

        fn noop_argv(_: &DailyHeadlineContext) -> BenchArgv {
            BenchArgv::new("true", Vec::<&str>::new())
        }

        let pending_plan = DailyHeadlinePlan {
            spec: &PENDING_GAP,
            pending: true,
            setup: setup_small,
            prepare: prepare_none,
            build_git: noop_argv,
            build_grit: noop_argv,
            correctness: None,
        };
        let executable_plan = DailyHeadlinePlan {
            spec: &EXECUTABLE,
            pending: false,
            setup: setup_small,
            prepare: prepare_none,
            build_git: noop_argv,
            build_grit: noop_argv,
            correctness: None,
        };

        let opts = DailyRunOptions {
            git: PathBuf::from("/usr/bin/git"),
            grit: PathBuf::from("/usr/bin/grit"),
            baseline_grit: None,
            measure_helper: PathBuf::from("/usr/bin/grit-bench"),
            smoke: true,
            offline: true,
            only: None,
            areas: Some(vec![Area::Revwalk]),
            output: None,
        };
        let plans = filter_plans(vec![pending_plan, executable_plan], &opts);
        assert!(
            plans
                .iter()
                .any(|p| p.spec.id == PENDING_GAP.id && p.pending),
            "pending gap scenario must remain when --area selects its area"
        );
        assert!(
            plans
                .iter()
                .any(|p| p.spec.id == EXECUTABLE.id && !p.pending),
            "executable scenario in the same area must also remain"
        );
    }

    #[test]
    fn only_rejects_registry_id_without_plan() {
        let only = vec!["future/scenario@small".into()];
        let selected = HashSet::new();
        let mut registry = HashSet::new();
        registry.insert("future/scenario@small");
        let executable = HashSet::new();
        let err = validate_only_ids(&only, &selected, &registry, &executable).unwrap_err();
        assert!(err.to_string().contains("no executable plan"), "{err}");
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
            areas: Some(vec![Area::Worktree]),
            output: None,
        };
        let plans = select_plans(&opts);
        assert!(plans.iter().all(|p| p.spec.area == Area::Worktree));
        assert!(plans.len() >= 5);
    }
}
