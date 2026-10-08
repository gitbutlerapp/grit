//! Peak resident set size for benchmark child processes (Unix).

use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result};

use crate::hyperfine::{run_hyperfine, HyperfineResultEntry, HyperfineRun};
use crate::schema::TimingStats;
use crate::stats::timing_from_hyperfine;

/// Peak RSS of child processes after running `command`, in bytes.
#[cfg(unix)]
pub fn peak_rss_for_command(command: &str, cwd: &std::path::Path) -> Result<u64> {
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
pub fn peak_rss_for_command(_command: &str, _cwd: &std::path::Path) -> Result<u64> {
    Ok(0)
}

#[cfg(unix)]
fn rss_bytes_from_nix(kib: i64) -> u64 {
    // Linux reports KiB; macOS reports bytes in some versions — treat large values as bytes.
    if kib > 1_000_000 {
        kib as u64
    } else {
        (kib as u64).saturating_mul(1024)
    }
}

/// Run hyperfine for timing and sample peak RSS on the last timed run.
pub fn bench_with_peak_rss(
    hyperfine: &std::path::Path,
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
    let peak = peak_rss_for_command(&wrap_env(command, env_prefix), cwd)?;
    Ok((stats, peak))
}

fn wrap_env(command: &str, env_prefix: Option<&str>) -> String {
    match env_prefix {
        Some(prefix) => format!("{prefix} {command}"),
        None => command.to_string(),
    }
}

/// Median wall time in milliseconds for a single command (no hyperfine).
pub fn median_wall_ms(command: &str, cwd: &std::path::Path, runs: u32) -> Result<f64> {
    let mut samples = Vec::new();
    for _ in 0..runs.max(1) {
        let start = Instant::now();
        peak_rss_for_command(command, cwd)?;
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Ok(samples[samples.len() / 2])
}

#[allow(dead_code)]
pub fn hyperfine_entry_mean_ms(entry: &HyperfineResultEntry) -> f64 {
    entry.mean * 1000.0
}
