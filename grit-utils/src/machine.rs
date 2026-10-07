//! Collect machine and toolchain metadata for benchmark reports.

use std::collections::{HashMap, HashSet};
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
        cargo_profile: grit_bench_cargo_profile(),
    })
}

/// Profile this `grit-bench` binary was built with (`dev` or `release`).
pub fn grit_bench_cargo_profile() -> String {
    if cfg!(debug_assertions) {
        "dev".into()
    } else {
        "release".into()
    }
}

fn read_cpu_model() -> String {
    fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.trim().to_string())
        })
        .unwrap_or_else(|| "unknown".into())
}

fn read_logical_cores_from(content: &str) -> u32 {
    let count = content
        .lines()
        .filter(|l| l.starts_with("processor"))
        .count() as u32;
    if count > 0 {
        count
    } else {
        1
    }
}

fn read_logical_cores() -> u32 {
    fs::read_to_string("/proc/cpuinfo")
        .map(|c| read_logical_cores_from(&c))
        .unwrap_or(1)
}

/// Count unique `(physical id, core id)` tuples per `/proc/cpuinfo` block.
pub fn physical_cores_from_cpuinfo(content: &str) -> Option<u32> {
    let mut socket_cores: HashMap<String, HashSet<String>> = HashMap::new();
    let mut physical_id: Option<String> = None;
    let mut core_id: Option<String> = None;

    let flush = |socket_cores: &mut HashMap<String, HashSet<String>>,
                 physical_id: &mut Option<String>,
                 core_id: &mut Option<String>| {
        if let (Some(p), Some(c)) = (physical_id.take(), core_id.take()) {
            socket_cores.entry(p).or_default().insert(c);
        } else {
            physical_id.take();
            core_id.take();
        }
    };

    for line in content.lines() {
        if line.trim().is_empty() {
            flush(&mut socket_cores, &mut physical_id, &mut core_id);
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "physical id" => physical_id = Some(value.trim().to_string()),
            "core id" => core_id = Some(value.trim().to_string()),
            _ => {}
        }
    }
    flush(&mut socket_cores, &mut physical_id, &mut core_id);

    let total: usize = socket_cores.values().map(HashSet::len).sum();
    if total > 0 {
        return Some(total as u32);
    }

    // Fallback: `cpu cores` per socket × socket count from unique physical ids.
    let mut cores_per_socket: Option<u32> = None;
    let mut physical_ids: HashSet<String> = HashSet::new();
    for line in content.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "cpu cores" if cores_per_socket.is_none() => {
                cores_per_socket = value.trim().parse().ok();
            }
            "physical id" => {
                physical_ids.insert(value.trim().to_string());
            }
            _ => {}
        }
    }
    match (cores_per_socket, physical_ids.len()) {
        (Some(cps), 0) => Some(cps),
        (Some(cps), sockets) => Some(cps * sockets as u32),
        _ => None,
    }
}

fn read_physical_cores() -> u32 {
    fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|c| physical_cores_from_cpuinfo(&c))
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

    const FOUR_CORE_CPUINFO: &str = include_str!("../tests/fixtures/cpuinfo_four_logical.txt");

    #[test]
    fn format_timestamp_rfc3339() {
        let ts = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let s = format_timestamp(ts);
        assert!(s.contains('T'));
    }

    #[test]
    fn physical_cores_from_fixture_counts_core_tuples() {
        assert_eq!(physical_cores_from_cpuinfo(FOUR_CORE_CPUINFO), Some(4));
        assert_eq!(read_logical_cores_from(FOUR_CORE_CPUINFO), 4);
    }

    #[test]
    fn cargo_profile_matches_build() {
        let profile = grit_bench_cargo_profile();
        if cfg!(debug_assertions) {
            assert_eq!(profile, "dev");
        } else {
            assert_eq!(profile, "release");
        }
    }
}
