//! Summarize Criterion `estimates.json` trees into grit-bench-compatible JSON.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use time::OffsetDateTime;

use crate::machine::{collect_machine_info, format_timestamp, grit_bench_cargo_profile};
use crate::schema::{MachineInfo, TimingStats, ToolVersions};

/// Schema version for [`CriterionSummaryReport`].
pub const CRITERION_SUMMARY_SCHEMA_VERSION: u32 = 1;

/// One Criterion benchmark (nanoseconds in source; milliseconds in output).
#[derive(Debug, Clone, Deserialize)]
struct EstimateBlock {
    point_estimate: f64,
}

#[derive(Debug, Clone, Deserialize)]
struct CriterionEstimates {
    mean: EstimateBlock,
    median: EstimateBlock,
    std_dev: EstimateBlock,
}

/// Full summary written to `target/bench/criterion-summary.json`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct CriterionSummaryReport {
    pub schema_version: u32,
    pub timestamp: String,
    pub machine: MachineInfo,
    pub tools: ToolVersions,
    pub scenarios: Vec<CriterionScenarioResult>,
}

/// One row in the criterion summary (matches site baseline shape for `driver: "criterion"`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct CriterionScenarioResult {
    pub id: String,
    pub group: String,
    pub fixture: String,
    pub description: String,
    pub driver: String,
    pub grit: TimingStats,
}

/// Walk `root` for `estimates.json` under Criterion's `new/` directories.
pub fn collect_estimate_paths(root: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    if !root.is_dir() {
        return Ok(paths);
    }
    walk_estimates(root, &mut paths)?;
    paths.sort();
    Ok(paths)
}

fn walk_estimates(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("read dir {}", dir.display()))? {
        let entry = entry.context("dir entry")?;
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "new") {
                let estimates = path.join("estimates.json");
                if estimates.is_file() {
                    out.push(estimates);
                }
            } else {
                walk_estimates(&path, out)?;
            }
        }
    }
    Ok(())
}

fn ns_to_ms(ns: f64) -> f64 {
    ns / 1_000_000.0
}

fn parse_estimates(path: &Path) -> Result<CriterionEstimates> {
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parse estimates {}", path.display()))
}

fn load_sample_runs_ms(estimates_path: &Path) -> Option<Vec<f64>> {
    let sample_path = estimates_path.with_file_name("sample.json");
    let raw = fs::read_to_string(sample_path).ok()?;
    #[derive(Deserialize)]
    struct Sample {
        times: Vec<f64>,
    }
    let sample: Sample = serde_json::from_str(&raw).ok()?;
    if sample.times.is_empty() {
        return None;
    }
    Some(sample.times.iter().map(|t| ns_to_ms(*t)).collect())
}

/// Derive `(group, id_suffix)` from a path like `target/criterion/hash_sha1/4KiB/new/estimates.json`.
fn scenario_id_from_path(criterion_root: &Path, estimates_path: &Path) -> (String, String) {
    let rel = estimates_path
        .strip_prefix(criterion_root)
        .unwrap_or(estimates_path);
    let parts: Vec<_> = rel
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    // [bench_group, param?, "new", "estimates.json"]
    let group = parts.first().unwrap_or(&"criterion").to_string();
    let mut id_parts: Vec<String> = parts
        .iter()
        .take(parts.len().saturating_sub(2))
        .map(|s| s.replace('/', "_"))
        .collect();
    if id_parts.is_empty() {
        id_parts.push("bench".into());
    }
    (group.clone(), id_parts.join("/"))
}

fn timing_from_estimates(estimates: &CriterionEstimates, runs_ms: Option<Vec<f64>>) -> TimingStats {
    let mean_ms = ns_to_ms(estimates.mean.point_estimate);
    let median_ms = ns_to_ms(estimates.median.point_estimate);
    let stddev_ms = ns_to_ms(estimates.std_dev.point_estimate);
    let runs = runs_ms.unwrap_or_else(|| vec![median_ms]);
    let min_ms = runs.iter().copied().fold(f64::INFINITY, f64::min);
    let max_ms = runs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    TimingStats {
        mean_ms,
        median_ms,
        stddev_ms,
        min_ms: if min_ms.is_finite() {
            min_ms
        } else {
            median_ms
        },
        max_ms: if max_ms.is_finite() {
            max_ms
        } else {
            median_ms
        },
        runs_ms: runs,
        peak_rss_bytes: None,
    }
}

/// Build a [`CriterionSummaryReport`] from a Criterion output directory.
pub fn summarize_criterion_tree(
    criterion_root: &Path,
    timestamp: OffsetDateTime,
) -> Result<CriterionSummaryReport> {
    let paths = collect_estimate_paths(criterion_root)?;
    let mut scenarios = Vec::with_capacity(paths.len());
    for path in paths {
        let estimates = parse_estimates(&path)?;
        let (group, id_suffix) = scenario_id_from_path(criterion_root, &path);
        let runs_ms = load_sample_runs_ms(&path);
        let id = format!("criterion-{group}-{id_suffix}").replace('/', "-");
        scenarios.push(CriterionScenarioResult {
            id: id.clone(),
            group: group.clone(),
            fixture: "criterion-fixture".into(),
            description: format!("Criterion {group}/{id_suffix} (from estimates.json)"),
            driver: "criterion".into(),
            grit: timing_from_estimates(&estimates, runs_ms),
        });
    }
    scenarios.sort_by(|a, b| a.id.cmp(&b.id));

    let machine = collect_machine_info(criterion_root).unwrap_or_else(|_| MachineInfo {
        cpu_model: "unknown".into(),
        physical_cores: 0,
        logical_cores: 0,
        ram_bytes: 0,
        os: std::env::consts::OS.into(),
        kernel: "unknown".into(),
        scratch_filesystem: "unknown".into(),
        rustc_version: "unknown".into(),
        cargo_profile: grit_bench_cargo_profile(),
    });

    Ok(CriterionSummaryReport {
        schema_version: CRITERION_SUMMARY_SCHEMA_VERSION,
        timestamp: format_timestamp(timestamp),
        machine,
        tools: ToolVersions {
            git: "n/a (criterion micro-benchmarks)".into(),
            grit: format!("grit-lib {}", env!("CARGO_PKG_VERSION")),
            grit_commit: crate::binary::grit_source_commit(),
        },
        scenarios,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenario_id_from_criterion_path() {
        let root = Path::new("/tmp/criterion");
        let path = Path::new("/tmp/criterion/hash_sha1/4KiB/new/estimates.json");
        let (group, suffix) = scenario_id_from_path(root, path);
        assert_eq!(group, "hash_sha1");
        assert_eq!(suffix, "hash_sha1/4KiB");
    }

    #[test]
    fn parse_real_estimates_fixture() {
        let estimates = include_str!("../tests/fixtures/criterion_estimates_sample.json");
        let parsed: CriterionEstimates = serde_json::from_str(estimates).expect("parse");
        let stats = timing_from_estimates(&parsed, None);
        assert!(stats.median_ms > 0.0);
    }
}
