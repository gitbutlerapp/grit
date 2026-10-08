//! Compare two grit-bench JSON reports.

use std::collections::HashMap;

use anyhow::Result;

use crate::schema::BenchReport;

/// Outcome of comparing scenario ratios between two reports.
#[derive(Debug, Clone, PartialEq)]
pub struct CompareMismatch {
    pub scenario_id: String,
    pub ratio_a: f64,
    pub ratio_b: f64,
    pub delta: f64,
}

/// Compare per-scenario ratios; returns mismatches where `|ratio_a - ratio_b| > tolerance`.
pub fn compare_reports(a: &BenchReport, b: &BenchReport, tolerance: f64) -> Vec<CompareMismatch> {
    let map_b: HashMap<&str, f64> = b
        .scenarios
        .iter()
        .map(|s| (s.id.as_str(), s.ratio))
        .collect();

    let mut mismatches = Vec::new();
    for scenario in &a.scenarios {
        let Some(&ratio_b) = map_b.get(scenario.id.as_str()) else {
            mismatches.push(CompareMismatch {
                scenario_id: scenario.id.clone(),
                ratio_a: scenario.ratio,
                ratio_b: f64::NAN,
                delta: f64::INFINITY,
            });
            continue;
        };
        let delta = (scenario.ratio - ratio_b).abs();
        if delta > tolerance {
            mismatches.push(CompareMismatch {
                scenario_id: scenario.id.clone(),
                ratio_a: scenario.ratio,
                ratio_b,
                delta,
            });
        }
    }
    mismatches
}

/// Load two JSON files and compare; errors if schema is invalid.
pub fn compare_files(
    path_a: &std::path::Path,
    path_b: &std::path::Path,
    tolerance: f64,
) -> Result<Vec<CompareMismatch>> {
    let a = BenchReport::from_json_str(&std::fs::read_to_string(path_a)?)?;
    let b = BenchReport::from_json_str(&std::fs::read_to_string(path_b)?)?;
    Ok(compare_reports(&a, &b, tolerance))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{DriverKind, MachineInfo, ScenarioResult, TimingStats, ToolVersions};

    fn dummy_stats(median: f64) -> TimingStats {
        TimingStats {
            mean_ms: median,
            median_ms: median,
            stddev_ms: 0.0,
            min_ms: median,
            max_ms: median,
            runs_ms: vec![median],
            peak_rss_bytes: None,
        }
    }

    fn sample_report(id: &str, ratio: f64) -> BenchReport {
        let git_median = 100.0;
        let grit_median = ratio * git_median;
        BenchReport {
            schema_version: 1,
            timestamp: "2026-01-01T00:00:00Z".into(),
            machine: MachineInfo {
                cpu_model: "test".into(),
                physical_cores: 1,
                logical_cores: 1,
                ram_bytes: 1,
                os: "linux".into(),
                kernel: "test".into(),
                scratch_filesystem: "tmpfs".into(),
                rustc_version: "test".into(),
                cargo_profile: "release".into(),
            },
            tools: ToolVersions {
                git: "git".into(),
                grit: "grit".into(),
                grit_commit: None,
            },
            scenarios: vec![ScenarioResult {
                id: id.into(),
                group: "g".into(),
                fixture: "f".into(),
                description: "d".into(),
                driver: DriverKind::Cli,
                git: dummy_stats(git_median),
                grit: dummy_stats(grit_median),
                ratio,
            }],
        }
    }

    #[test]
    fn compare_within_tolerance() {
        let a = sample_report("status-100", 0.80);
        let b = sample_report("status-100", 0.85);
        assert!(compare_reports(&a, &b, 0.10).is_empty());
    }

    #[test]
    fn compare_outside_tolerance() {
        let a = sample_report("status-100", 0.80);
        let b = sample_report("status-100", 0.95);
        let mismatches = compare_reports(&a, &b, 0.10);
        assert_eq!(mismatches.len(), 1);
        assert!((mismatches[0].delta - 0.15).abs() < f64::EPSILON);
    }
}
