//! Direct-spawn benchmark runner with per-run wall time and peak RSS.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::bench_env::apply_isolated_env;
use crate::capped_run::{
    run_capped_argv_command_with, TerminationKind, EXIT_MEMORY_CAP, EXIT_TIMEOUT,
};
use crate::schema::TimingStats;

/// One benchmark invocation (program + arguments, no shell).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchArgv {
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl BenchArgv {
    /// Build argv for a command on `PATH`.
    pub fn new(
        program: impl Into<PathBuf>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}

/// When to stop alternating git/grit timed runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunPolicy {
    /// Untimed iterations per tool before recording (each tool, in order git then grit).
    pub warmups_per_tool: u32,
    /// Minimum timed runs per tool (git and grit each).
    pub min_runs_per_tool: u32,
    /// Hard cap on timed runs per tool.
    pub max_runs_per_tool: u32,
    /// Keep alternating until the sum of timed wall times reaches this (both tools).
    pub min_total_timed_duration: Duration,
    /// Stop the whole scenario when elapsed wall time exceeds this budget.
    pub scenario_time_budget: Duration,
}

impl Default for RunPolicy {
    fn default() -> Self {
        Self {
            warmups_per_tool: 1,
            min_runs_per_tool: 3,
            max_runs_per_tool: 20,
            min_total_timed_duration: Duration::from_secs(3),
            scenario_time_budget: Duration::from_secs(120),
        }
    }
}

/// Optional caps on each timed iteration (enforced in the measure helper process).
#[derive(Debug, Clone, Copy, Default)]
pub struct RunLimits {
    pub timeout: Option<Duration>,
    pub max_rss_bytes: Option<u64>,
}

/// Outcome of one timed iteration.
#[derive(Debug, Clone, PartialEq)]
pub struct TimedRunSample {
    pub wall_ms: f64,
    pub peak_rss_bytes: u64,
    pub termination: TerminationKind,
}

/// Aggregated samples for one tool on a scenario.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSeries {
    pub stats: TimingStats,
    pub samples: Vec<TimedRunSample>,
}

/// Result of an ABAB git/grit benchmark session.
#[derive(Debug, Clone, PartialEq)]
pub struct AbBenchOutcome {
    pub git: ToolSeries,
    pub grit: ToolSeries,
}

/// Inputs for [`run_ab_bench`].
#[allow(clippy::type_complexity)]
pub struct AbBenchRequest<'a> {
    pub cwd: &'a Path,
    pub git: &'a BenchArgv,
    pub grit: &'a BenchArgv,
    pub prepare: Option<&'a BenchArgv>,
    pub policy: &'a RunPolicy,
    pub limits: &'a RunLimits,
    pub measure_helper: &'a Path,
    pub correctness: Option<&'a (dyn Fn(&Path) -> Result<()> + 'a)>,
}

/// Compute how many timed runs each tool should execute under `policy`.
#[must_use]
pub fn planned_timed_runs_per_tool(policy: &RunPolicy, mean_run_ms: f64) -> u32 {
    let mean_ms = mean_run_ms.max(0.001);
    let min_total_ms = policy.min_total_timed_duration.as_secs_f64() * 1000.0;
    let runs_for_time =
        ((min_total_ms / (2.0 * mean_ms)).ceil() as u32).max(policy.min_runs_per_tool);
    runs_for_time.min(policy.max_runs_per_tool)
}

/// Decide whether another timed pair (git + grit) is needed.
#[must_use]
pub fn needs_more_timed_runs(
    policy: &RunPolicy,
    git_runs: u32,
    grit_runs: u32,
    total_timed_ms: f64,
    scenario_elapsed: Duration,
) -> bool {
    if scenario_elapsed >= policy.scenario_time_budget {
        return false;
    }
    let min_runs = policy.min_runs_per_tool;
    let min_total_ms = policy.min_total_timed_duration.as_secs_f64() * 1000.0;
    if git_runs < min_runs && git_runs < policy.max_runs_per_tool {
        return true;
    }
    if grit_runs < min_runs && grit_runs < policy.max_runs_per_tool {
        return true;
    }
    if git_runs != grit_runs {
        if git_runs < grit_runs && git_runs < policy.max_runs_per_tool {
            return true;
        }
        if grit_runs < git_runs && grit_runs < policy.max_runs_per_tool {
            return true;
        }
    }
    if total_timed_ms < min_total_ms {
        return git_runs < policy.max_runs_per_tool || grit_runs < policy.max_runs_per_tool;
    }
    false
}

fn sample_succeeded(sample: &TimedRunSample) -> bool {
    sample.termination == TerminationKind::Success
}

/// Build [`TimingStats`] from successful timed samples only (peak RSS is max over all runs).
#[must_use]
pub fn timing_stats_from_samples(samples: &[TimedRunSample]) -> TimingStats {
    let runs_ms: Vec<f64> = samples
        .iter()
        .filter(|s| sample_succeeded(s))
        .map(|s| s.wall_ms)
        .collect();
    let mut sorted_ms = runs_ms.clone();
    sorted_ms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = sorted_ms.len();
    let mean_ms = if n == 0 {
        0.0
    } else {
        runs_ms.iter().sum::<f64>() / n as f64
    };
    let median_ms = if n == 0 {
        0.0
    } else if n % 2 == 1 {
        sorted_ms[n / 2]
    } else {
        (sorted_ms[n / 2 - 1] + sorted_ms[n / 2]) / 2.0
    };
    let variance = if n <= 1 {
        0.0
    } else {
        runs_ms
            .iter()
            .map(|x| {
                let d = x - mean_ms;
                d * d
            })
            .sum::<f64>()
            / ((n - 1) as f64)
    };
    let stddev_ms = variance.sqrt();
    let min_ms = sorted_ms.first().copied().unwrap_or(0.0);
    let max_ms = sorted_ms.last().copied().unwrap_or(0.0);
    let peak_rss_bytes = samples.iter().map(|s| s.peak_rss_bytes).max();
    TimingStats {
        mean_ms,
        median_ms,
        stddev_ms,
        min_ms,
        max_ms,
        runs_ms,
        peak_rss_bytes,
    }
}

/// Run warmups, optional correctness check, then ABAB timed git/grit runs.
pub fn run_ab_bench(request: &AbBenchRequest<'_>) -> Result<AbBenchOutcome> {
    let AbBenchRequest {
        cwd,
        git,
        grit,
        prepare,
        policy,
        limits,
        measure_helper,
        correctness,
    } = request;

    if let Some(check) = correctness {
        check(cwd).context("correctness hook before timing")?;
    }

    for _ in 0..policy.warmups_per_tool {
        run_prepare(cwd, *prepare)?;
        run_timed_via_helper(cwd, git, limits, measure_helper)?;
        run_prepare(cwd, *prepare)?;
        run_timed_via_helper(cwd, grit, limits, measure_helper)?;
    }

    let mut git_samples = Vec::new();
    let mut grit_samples = Vec::new();
    let scenario_start = Instant::now();
    let mut total_timed_ms = 0.0;
    let mut git_count = 0u32;
    let mut grit_count = 0u32;
    let mut next_git = true;

    while needs_more_timed_runs(
        policy,
        git_count,
        grit_count,
        total_timed_ms,
        scenario_start.elapsed(),
    ) {
        run_prepare(cwd, *prepare)?;
        if next_git {
            let sample = run_timed_via_helper(cwd, git, limits, measure_helper)?;
            if sample_succeeded(&sample) {
                total_timed_ms += sample.wall_ms;
            }
            git_samples.push(sample);
            git_count += 1;
        } else {
            let sample = run_timed_via_helper(cwd, grit, limits, measure_helper)?;
            if sample_succeeded(&sample) {
                total_timed_ms += sample.wall_ms;
            }
            grit_samples.push(sample);
            grit_count += 1;
        }
        next_git = !next_git;
    }

    Ok(AbBenchOutcome {
        git: ToolSeries {
            stats: timing_stats_from_samples(&git_samples),
            samples: git_samples,
        },
        grit: ToolSeries {
            stats: timing_stats_from_samples(&grit_samples),
            samples: grit_samples,
        },
    })
}

fn run_prepare(cwd: &Path, prepare: Option<&BenchArgv>) -> Result<()> {
    let Some(prep) = prepare else {
        return Ok(());
    };
    let outcome = run_direct_once(cwd, prep, &RunLimits::default())?;
    match outcome.termination {
        TerminationKind::Success => Ok(()),
        TerminationKind::CommandFailed { code } => {
            anyhow::bail!("prepare command failed with exit code {code}");
        }
        TerminationKind::Timeout => anyhow::bail!("prepare command timed out"),
        TerminationKind::MemoryCap { .. } => anyhow::bail!("prepare command exceeded memory cap"),
    }
}

/// Run one timed iteration in a fresh helper process (direct argv, isolated env).
pub fn run_timed_via_helper(
    cwd: &Path,
    cmd: &BenchArgv,
    limits: &RunLimits,
    measure_helper: &Path,
) -> Result<TimedRunSample> {
    let start = Instant::now();
    let mut command = Command::new(measure_helper);
    command
        .arg("measure-direct-run")
        .arg("--cwd")
        .arg(cwd)
        .arg("--")
        .arg(&cmd.program)
        .args(&cmd.args);
    if let Some(secs) = limits.timeout.map(|d| d.as_secs().max(1)) {
        command.env(crate::resource::MEASURE_TIMEOUT_SECS_ENV, secs.to_string());
    }
    if let Some(cap) = limits.max_rss_bytes {
        command.env(crate::resource::MEASURE_MAX_RSS_ENV, cap.to_string());
    }
    let out = command
        .output()
        .context("spawn measure-direct-run helper")?;
    let wall_ms = start.elapsed().as_secs_f64() * 1000.0;
    if !out.status.success() {
        let code = out.status.code().unwrap_or(-1);
        let termination = TerminationKind::from_exit_code(code);
        return Ok(TimedRunSample {
            wall_ms,
            peak_rss_bytes: 0,
            termination,
        });
    }
    let line = String::from_utf8_lossy(&out.stdout);
    let mut parts = line.split_whitespace();
    let peak_rss_bytes: u64 = parts
        .next()
        .context("measure-direct-run peak rss")?
        .parse()
        .context("parse peak rss")?;
    let inner_ms: f64 = parts
        .next()
        .map(|s| s.parse())
        .transpose()
        .context("parse inner wall ms")?
        .unwrap_or(wall_ms);
    Ok(TimedRunSample {
        wall_ms: inner_ms,
        peak_rss_bytes,
        termination: TerminationKind::Success,
    })
}

/// Hidden CLI: run argv directly and print `peak_rss_bytes inner_wall_ms` on stdout.
pub fn run_measure_direct_cli(cwd: &Path, argv: &[String]) -> Result<()> {
    anyhow::ensure!(!argv.is_empty(), "measure-direct-run requires a program");
    let program = Path::new(&argv[0]);
    let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
    let limits = measure_limits_from_env()?;
    let start = Instant::now();
    let outcome = run_direct_once_with_limits(cwd, program, &args, &limits)?;
    let wall_ms = start.elapsed().as_secs_f64() * 1000.0;
    match outcome.termination {
        TerminationKind::Success => {
            println!("{} {wall_ms}", outcome.peak_rss_bytes);
            Ok(())
        }
        TerminationKind::Timeout => std::process::exit(EXIT_TIMEOUT),
        TerminationKind::MemoryCap { .. } => std::process::exit(EXIT_MEMORY_CAP),
        TerminationKind::CommandFailed { code } => std::process::exit(code),
    }
}

struct DirectOutcome {
    termination: TerminationKind,
    peak_rss_bytes: u64,
}

fn run_direct_once(cwd: &Path, cmd: &BenchArgv, limits: &RunLimits) -> Result<DirectOutcome> {
    let args: Vec<&str> = cmd.args.iter().map(String::as_str).collect();
    run_direct_once_with_limits(cwd, &cmd.program, &args, limits)
}

fn run_direct_once_with_limits(
    cwd: &Path,
    program: &Path,
    args: &[&str],
    limits: &RunLimits,
) -> Result<DirectOutcome> {
    let outcome = run_capped_argv_command_with(
        program,
        args,
        cwd,
        limits.timeout,
        limits.max_rss_bytes,
        apply_isolated_env,
    )?;
    Ok(DirectOutcome {
        termination: outcome.termination,
        peak_rss_bytes: outcome.peak_rss_bytes,
    })
}

fn measure_limits_from_env() -> Result<RunLimits> {
    let timeout = std::env::var(crate::resource::MEASURE_TIMEOUT_SECS_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|s| Duration::from_secs(s.max(1)));
    let max_rss_bytes = std::env::var(crate::resource::MEASURE_MAX_RSS_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok());
    Ok(RunLimits {
        timeout,
        max_rss_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timing_stats_median_averages_even_count() {
        let samples = vec![
            TimedRunSample {
                wall_ms: 10.0,
                peak_rss_bytes: 0,
                termination: TerminationKind::Success,
            },
            TimedRunSample {
                wall_ms: 20.0,
                peak_rss_bytes: 0,
                termination: TerminationKind::Success,
            },
        ];
        let stats = timing_stats_from_samples(&samples);
        assert!((stats.median_ms - 15.0).abs() < f64::EPSILON);
    }

    #[test]
    fn timing_stats_excludes_failed_runs() {
        let samples = vec![
            TimedRunSample {
                wall_ms: 10.0,
                peak_rss_bytes: 100,
                termination: TerminationKind::Success,
            },
            TimedRunSample {
                wall_ms: 60_000.0,
                peak_rss_bytes: 0,
                termination: TerminationKind::Timeout,
            },
            TimedRunSample {
                wall_ms: 20.0,
                peak_rss_bytes: 100,
                termination: TerminationKind::Success,
            },
        ];
        let stats = timing_stats_from_samples(&samples);
        assert_eq!(stats.runs_ms, vec![10.0, 20.0]);
        assert!((stats.median_ms - 15.0).abs() < f64::EPSILON);
        assert!((stats.mean_ms - 15.0).abs() < f64::EPSILON);
    }

    #[test]
    fn needs_more_balances_unequal_counts_after_min_runs() {
        let policy = RunPolicy {
            min_runs_per_tool: 2,
            max_runs_per_tool: 10,
            min_total_timed_duration: Duration::from_millis(100),
            scenario_time_budget: Duration::from_secs(60),
            ..Default::default()
        };
        assert!(needs_more_timed_runs(
            &policy,
            3,
            2,
            500.0,
            Duration::from_millis(1)
        ));
        assert!(!needs_more_timed_runs(
            &policy,
            3,
            3,
            500.0,
            Duration::from_millis(1)
        ));
    }

    #[test]
    fn timing_stats_median_mean_stddev() {
        let samples = vec![
            TimedRunSample {
                wall_ms: 10.0,
                peak_rss_bytes: 100,
                termination: TerminationKind::Success,
            },
            TimedRunSample {
                wall_ms: 20.0,
                peak_rss_bytes: 200,
                termination: TerminationKind::Success,
            },
            TimedRunSample {
                wall_ms: 30.0,
                peak_rss_bytes: 150,
                termination: TerminationKind::Success,
            },
        ];
        let stats = timing_stats_from_samples(&samples);
        assert!((stats.mean_ms - 20.0).abs() < f64::EPSILON);
        assert!((stats.median_ms - 20.0).abs() < f64::EPSILON);
        assert!((stats.min_ms - 10.0).abs() < f64::EPSILON);
        assert!((stats.max_ms - 30.0).abs() < f64::EPSILON);
        assert_eq!(stats.peak_rss_bytes, Some(200));
        assert!((stats.stddev_ms - 10.0).abs() < 1e-9);
    }

    #[test]
    fn planned_runs_respects_min_and_max() {
        let policy = RunPolicy {
            min_runs_per_tool: 5,
            max_runs_per_tool: 8,
            min_total_timed_duration: Duration::from_secs(10),
            ..Default::default()
        };
        let runs = planned_timed_runs_per_tool(&policy, 100.0);
        assert_eq!(runs, 8);
        let runs_short = planned_timed_runs_per_tool(&policy, 10_000.0);
        assert_eq!(runs_short, 5);
    }

    #[test]
    fn needs_more_until_min_runs_and_duration() {
        let policy = RunPolicy {
            min_runs_per_tool: 2,
            max_runs_per_tool: 10,
            min_total_timed_duration: Duration::from_millis(500),
            scenario_time_budget: Duration::from_secs(60),
            ..Default::default()
        };
        assert!(needs_more_timed_runs(
            &policy,
            0,
            0,
            0.0,
            Duration::from_millis(1)
        ));
        assert!(needs_more_timed_runs(
            &policy,
            2,
            1,
            100.0,
            Duration::from_millis(1)
        ));
        assert!(!needs_more_timed_runs(
            &policy,
            2,
            2,
            600.0,
            Duration::from_millis(1)
        ));
    }
}
