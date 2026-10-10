//! Peak resident set size for benchmark child processes (Unix).

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::capped_run::{run_capped_shell_command, TerminationKind, EXIT_MEMORY_CAP, EXIT_TIMEOUT};
use crate::hyperfine::{run_hyperfine, HyperfineResultEntry, HyperfineRun};
use crate::schema::TimingStats;
use crate::stats::timing_from_hyperfine;

/// Environment variable carrying the shell command for [`run_measure_rss_cli`].
pub const MEASURE_CMD_ENV: &str = "GRIT_BENCH_MEASURE_CMD";

/// Optional wall-clock limit for [`run_measure_rss_cli`] / [`run_limited_cli`].
pub const MEASURE_TIMEOUT_SECS_ENV: &str = "GRIT_BENCH_MEASURE_TIMEOUT_SECS";

/// Optional peak-RSS cap (bytes) for [`run_measure_rss_cli`] / [`run_limited_cli`].
pub const MEASURE_MAX_RSS_ENV: &str = "GRIT_BENCH_MEASURE_MAX_RSS_BYTES";

/// Hidden CLI entry: run one shell command and print peak RSS (bytes) on stdout.
///
/// Invoked in a fresh process so `getrusage(RUSAGE_CHILDREN)` reflects only this run.
pub fn run_measure_rss_cli(cwd: &Path) -> Result<()> {
    let command = std::env::var(MEASURE_CMD_ENV).context("GRIT_BENCH_MEASURE_CMD not set")?;
    let caps = measure_limits_from_env()?;
    let outcome = run_capped_shell_command(&command, cwd, caps.timeout, caps.max_rss_bytes)?;
    match outcome.termination {
        TerminationKind::Success => {
            println!("{}", outcome.peak_rss_bytes);
            Ok(())
        }
        TerminationKind::Timeout => std::process::exit(EXIT_TIMEOUT),
        TerminationKind::MemoryCap { .. } => std::process::exit(EXIT_MEMORY_CAP),
        TerminationKind::CommandFailed { code } => std::process::exit(code),
    }
}

/// Hidden CLI entry: run one shell command under timeout/RSS caps (no stdout on success).
pub fn run_limited_cli(cwd: &Path) -> Result<()> {
    let command = std::env::var(MEASURE_CMD_ENV).context("GRIT_BENCH_MEASURE_CMD not set")?;
    let caps = measure_limits_from_env()?;
    let outcome = run_capped_shell_command(&command, cwd, caps.timeout, caps.max_rss_bytes)?;
    match outcome.termination {
        TerminationKind::Success => Ok(()),
        TerminationKind::Timeout => std::process::exit(EXIT_TIMEOUT),
        TerminationKind::MemoryCap { .. } => std::process::exit(EXIT_MEMORY_CAP),
        TerminationKind::CommandFailed { code } => std::process::exit(code),
    }
}

struct MeasureLimits {
    timeout: Option<Duration>,
    max_rss_bytes: Option<u64>,
}

fn measure_limits_from_env() -> Result<MeasureLimits> {
    let timeout = std::env::var(MEASURE_TIMEOUT_SECS_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|s| Duration::from_secs(s.max(1)));
    let max_rss_bytes = std::env::var(MEASURE_MAX_RSS_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok());
    Ok(MeasureLimits {
        timeout,
        max_rss_bytes,
    })
}

/// Peak RSS for `command`, measured in a fresh `grit-bench measure-rss` process.
pub fn peak_rss_for_command(command: &str, cwd: &Path, measure_helper: &Path) -> Result<u64> {
    peak_rss_for_command_with_limits(command, cwd, measure_helper, None, None)
}

/// Peak RSS with optional proactive timeout and RSS caps (see [`run_capped_shell_command`]).
pub fn peak_rss_for_command_with_limits(
    command: &str,
    cwd: &Path,
    measure_helper: &Path,
    timeout: Option<Duration>,
    max_rss_bytes: Option<u64>,
) -> Result<u64> {
    let mut cmd = Command::new(measure_helper);
    cmd.arg("measure-rss")
        .arg("--cwd")
        .arg(cwd)
        .env(MEASURE_CMD_ENV, command);
    if let Some(secs) = timeout.map(|d| d.as_secs().max(1)) {
        cmd.env(MEASURE_TIMEOUT_SECS_ENV, secs.to_string());
    }
    if let Some(cap) = max_rss_bytes {
        cmd.env(MEASURE_MAX_RSS_ENV, cap.to_string());
    }
    let out = cmd.output().context("spawn measure-rss helper")?;
    if !out.status.success() {
        let code = out.status.code().unwrap_or(-1);
        return Err(CappedCommandError { code }.into());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.trim()
        .parse::<u64>()
        .context("parse measure-rss stdout as u64")
}

/// Error from a capped helper process (`measure-rss` / `run-limited`).
#[derive(Debug, Clone, Copy)]
pub struct CappedCommandError {
    pub code: i32,
}

impl std::fmt::Display for CappedCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "capped command exited with code {}", self.code)
    }
}

impl std::error::Error for CappedCommandError {}

/// Run hyperfine for timing and sample peak RSS on the last timed run.
#[allow(clippy::too_many_arguments)]
pub fn bench_with_peak_rss(
    hyperfine: &std::path::Path,
    measure_helper: &std::path::Path,
    command: &str,
    cwd: &std::path::Path,
    warmup: u32,
    min_runs: u32,
    command_name: &str,
    env_prefix: Option<&str>,
) -> Result<(TimingStats, u64)> {
    let entry = run_hyperfine(
        hyperfine,
        &HyperfineRun {
            command: command.to_string(),
            working_directory: cwd.to_path_buf(),
            prepare: None,
            warmup,
            min_runs,
            command_name: Some(command_name.into()),
            env_prefix: env_prefix.map(str::to_string),
        },
    )?;
    let stats = timing_from_hyperfine(&entry);
    let peak = peak_rss_for_command(&wrap_env(command, env_prefix), cwd, measure_helper)?;
    Ok((stats, peak))
}

fn wrap_env(command: &str, env_prefix: Option<&str>) -> String {
    match env_prefix {
        Some(prefix) => format!("{prefix} {command}"),
        None => command.to_string(),
    }
}

/// Median wall time in milliseconds for a single command (no hyperfine).
pub fn median_wall_ms(
    command: &str,
    cwd: &std::path::Path,
    measure_helper: &std::path::Path,
    runs: u32,
) -> Result<f64> {
    let mut samples = Vec::new();
    for _ in 0..runs.max(1) {
        let start = Instant::now();
        peak_rss_for_command(command, cwd, measure_helper)?;
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Ok(samples[samples.len() / 2])
}

#[allow(dead_code)]
pub fn hyperfine_entry_mean_ms(entry: &HyperfineResultEntry) -> f64 {
    entry.mean * 1000.0
}

/// Limits for grit `serve-clone` benchmark runs (avoid OOM hangs).
#[derive(Debug, Clone, Copy)]
pub struct ServeCloneCaps {
    pub timeout: Duration,
    pub max_rss_bytes: u64,
}

/// Read serve-clone caps from the environment with factory VM defaults.
pub fn serve_clone_caps() -> ServeCloneCaps {
    let timeout_secs = std::env::var("GRIT_BENCH_SERVE_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    let max_rss_gib = std::env::var("GRIT_BENCH_SERVE_MAX_RSS_GIB")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(12.0);
    ServeCloneCaps {
        timeout: Duration::from_secs(timeout_secs),
        max_rss_bytes: (max_rss_gib * 1024.0 * 1024.0 * 1024.0) as u64,
    }
}

/// Wrap a benchmark command so hyperfine executes it via `grit-bench run-limited`.
pub fn wrap_command_with_serve_caps(
    measure_helper: &Path,
    cwd: &Path,
    inner_command: &str,
    caps: ServeCloneCaps,
) -> String {
    let helper = crate::shell::shell_quote(&measure_helper.to_string_lossy());
    let cwd_q = crate::shell::shell_quote(&cwd.to_string_lossy());
    let cmd_q = crate::shell::shell_quote(inner_command);
    format!(
        "GRIT_BENCH_MEASURE_CMD={cmd_q} GRIT_BENCH_MEASURE_TIMEOUT_SECS={} GRIT_BENCH_MEASURE_MAX_RSS_BYTES={} {helper} run-limited --cwd {cwd_q}",
        caps.timeout.as_secs().max(1),
        caps.max_rss_bytes,
    )
}
