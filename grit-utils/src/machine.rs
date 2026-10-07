//! Collect machine and toolchain metadata for benchmark reports.

use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::Result;

use crate::schema::MachineInfo;

/// Build a [`MachineInfo`] snapshot for the given scratch directory path.
pub fn collect_machine_info(scratch_dir: &Path) -> Result<MachineInfo> {
    Ok(MachineInfo {
        cpu_model: read_cpu_model(),
        physical_cores: read_physical_cores(),
        logical_cores: read_logical_cores(),
        ram_bytes: read_ram_bytes(),
        os: env::consts::OS.to_string(),
        kernel: read_kernel(),
        scratch_filesystem: filesystem_type(scratch_dir),
        rustc_version: read_rustc_version(),
        cargo_profile: env::var("CARGO_PROFILE").unwrap_or_else(|_| "release".into()),
    })
}

fn read_cpu_model() -> String {
    fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|content| {
            content
                .lines()
                .find(|l| l.starts_with("model name"))
                .map(|l| l.split_once(':').map(|(_, v)| v.trim()).unwrap_or(l))
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown".into())
}

fn read_logical_cores() -> u32 {
    fs::read_to_string("/proc/cpuinfo")
        .ok()
        .map(|c| c.lines().filter(|l| l.starts_with("processor")).count() as u32)
        .filter(|&n| n > 0)
        .unwrap_or(1)
}

fn read_physical_cores() -> u32 {
    fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|content| {
            let cores: std::collections::HashSet<_> = content
                .lines()
                .filter_map(|l| {
                    l.strip_prefix("core id")
                        .or_else(|| l.strip_prefix("cpu cores"))
                })
                .filter_map(|l| l.split_once(':').map(|(_, v)| v.trim()))
                .collect();
            if cores.is_empty() {
                None
            } else {
                Some(cores.len() as u32)
            }
        })
        .unwrap_or_else(read_logical_cores)
}

fn read_ram_bytes() -> u64 {
    fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|content| {
            content.lines().find_map(|l| {
                l.strip_prefix("MemTotal:")
                    .map(|rest| rest.trim().trim_end_matches(" kB"))
                    .and_then(|kb| kb.parse::<u64>().ok())
                    .map(|kb| kb * 1024)
            })
        })
        .unwrap_or(0)
}

fn read_kernel() -> String {
    Command::new("uname")
        .args(["-sr"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn read_rustc_version() -> String {
    Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn filesystem_type(path: &Path) -> String {
    let path_str = path.to_str().unwrap_or("/tmp");
    let output = Command::new("df")
        .args(["-T", path_str])
        .output()
        .ok()
        .filter(|o| o.status.success());
    let Some(output) = output else {
        return "unknown".into();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .nth(1)
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("unknown")
        .to_string()
}

/// RFC3339 timestamp in UTC using the `time` crate (no shell `date`).
pub fn format_timestamp(now: time::OffsetDateTime) -> String {
    match now.format(&time::format_description::well_known::Rfc3339) {
        Ok(s) => s,
        Err(_) => now.unix_timestamp().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_timestamp_rfc3339() {
        let ts = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let s = format_timestamp(ts);
        assert!(s.contains('T'));
    }
}
