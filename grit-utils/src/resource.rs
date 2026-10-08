//! Peak resident set size for benchmark child processes (Unix).

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result};

use crate::hyperfine::{run_hyperfine, HyperfineResultEntry, HyperfineRun};
use crate::schema::TimingStats;
use crate::stats::timing_from_hyperfine;

/// Environment variable carrying the shell command for [`run_measure_rss_cli`].
pub const MEASURE_CMD_ENV: &str = "GRIT_BENCH_MEASURE_CMD";

/// Hidden CLI entry: run one shell command and print peak RSS (bytes) on stdout.
///
/// Invoked in a fresh process so `getrusage(RUSAGE_CHILDREN)` reflects only this run.
pub fn run_measure_rss_cli(cwd: &Path) -> Result<()> {
    let command = std::env::var(MEASURE_CMD_ENV).context("GRIT_BENCH_MEASURE_CMD not set")?;
    let peak = peak_rss_single_child(&command, cwd)?;
    println!("{peak}");
    Ok(())
}

/// Peak RSS for `command`, measured in a fresh `grit-bench measure-rss` process.
pub fn peak_rss_for_command(command: &str, cwd: &Path, measure_helper: &Path) -> Result<u64> {
    let out = Command::new(measure_helper)
        .arg("measure-rss")
        .arg("--cwd")
        .arg(cwd)
        .env(MEASURE_CMD_ENV, command)
        .output()
        .context("spawn measure-rss helper")?;
    if !out.status.success() {
        anyhow::bail!(
            "measure-rss failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.trim()
        .parse::<u64>()
        .context("parse measure-rss stdout as u64")
}

#[cfg(unix)]
fn peak_rss_single_child(command: &str, cwd: &Path) -> Result<u64> {
    use nix::sys::resource::{getrusage, UsageWho};
    use std::process::Stdio;

    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let status = cmd.status().context("run benchmark command for RSS")?;
    if !status.success() {
        anyhow::bail!("benchmark command failed with {status}");
    }
    let usage = getrusage(UsageWho::RUSAGE_CHILDREN).context("getrusage(RUSAGE_CHILDREN)")?;
    Ok(rss_bytes_from_nix(usage.max_rss()))
}

#[cfg(not(unix))]
fn peak_rss_single_child(_command: &str, _cwd: &Path) -> Result<u64> {
    Ok(0)
}

/// Convert `ru_maxrss` from `getrusage` into bytes (OS-specific units).
#[cfg(unix)]
pub(crate) fn rss_bytes_from_nix(raw: i64) -> u64 {
    let raw = raw.max(0) as u64;
    #[cfg(target_os = "macos")]
    {
        raw
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Linux and BSD report KiB.
        raw.saturating_mul(1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rss_bytes_linux_kib_to_bytes() {
        if cfg!(target_os = "macos") {
            assert_eq!(rss_bytes_from_nix(4096), 4096);
        } else {
            assert_eq!(rss_bytes_from_nix(1024), 1024 * 1024);
            assert_eq!(rss_bytes_from_nix(1_126_400), 1_153_433_600);
        }
    }
}

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
