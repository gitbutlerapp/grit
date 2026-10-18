//! Compare two grit-bench JSON reports.

use std::collections::HashMap;

use anyhow::Result;

use crate::schema::{BenchReport, ScenarioResult};

/// Outcome of comparing scenario ratios between two reports.
#[derive(Debug, Clone, PartialEq)]
pub struct CompareMismatch {
    pub scenario_id: String,
    pub ratio_a: f64,
    pub ratio_b: f64,
    pub delta: f64,
}

/// Compare per-scenario ratios; returns mismatches where relative drift exceeds `tolerance`.
///
/// Drift is `|ratio_a - ratio_b| / max(|ratio_a|, |ratio_b|)` (so `0.10` means ±10%).
///
/// Scenarios with a grit-side hyperfine failure on either capture are skipped. Rows are skipped
/// when git or grit medians drift beyond `tolerance` (ratio is meaningless if either side moved).
/// Ratio drift uses whole percentage points: `floor(delta × 100) > floor(tolerance × 100)`.
pub fn compare_reports(a: &BenchReport, b: &BenchReport, tolerance: f64) -> Vec<CompareMismatch> {
    let map_b: HashMap<&str, &ScenarioResult> =
        b.scenarios.iter().map(|s| (s.id.as_str(), s)).collect();

    let mut mismatches = Vec::new();
    for scenario in &a.scenarios {
        if scenario.grit_failure.is_some() {
            continue;
        }
        let Some(scenario_b) = map_b.get(scenario.id.as_str()) else {
            mismatches.push(CompareMismatch {
                scenario_id: scenario.id.clone(),
                ratio_a: scenario.ratio,
                ratio_b: f64::NAN,
                delta: f64::INFINITY,
            });
            continue;
        };
        if scenario_b.grit_failure.is_some() {
            continue;
        }
        let git_delta = relative_ratio_drift(scenario.git.median_ms, scenario_b.git.median_ms);
        let grit_delta =
            relative_ratio_drift(scenario.grit.median_ms, scenario_b.grit.median_ms);
        if git_delta > tolerance || grit_delta > tolerance {
            continue;
        }
        let ratio_b = scenario_b.ratio;
        let delta = relative_ratio_drift(scenario.ratio, ratio_b);
        if ratio_drift_exceeds_tolerance(delta, tolerance) {
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

fn relative_ratio_drift(a: f64, b: f64) -> f64 {
    let base = a.abs().max(b.abs());
    if base <= f64::EPSILON {
        return 0.0;
    }
    (a - b).abs() / base
}

fn ratio_drift_exceeds_tolerance(delta: f64, tolerance: f64) -> bool {
    (delta * 100.0).floor() > (tolerance * 100.0).floor()
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
            fixtures: vec![],
            scenarios: vec![ScenarioResult {
                id: id.into(),
                group: "g".into(),
                fixture: "f".into(),
                description: "d".into(),
                driver: DriverKind::Cli,
                git: dummy_stats(git_median),
                grit: dummy_stats(grit_median),
                ratio,
                grit_failure: None,
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
        let mut a = sample_report("status-100", 1.0);
        let mut b = sample_report("status-100", 1.2);
        a.scenarios[0].git = dummy_stats(100.0);
        a.scenarios[0].grit = dummy_stats(100.0);
        b.scenarios[0].git = dummy_stats(100.0);
        b.scenarios[0].grit = dummy_stats(108.0);
        a.scenarios[0].ratio = 1.0;
        b.scenarios[0].ratio = 1.2;
        let mismatches = compare_reports(&a, &b, 0.10);
        assert_eq!(mismatches.len(), 1);
        assert!((mismatches[0].delta - (0.2 / 1.2)).abs() < 1e-9);
    }

    #[test]
    fn compare_ratio_tie_at_tenth_of_percent_point() {
        let a = sample_report("pick", 1.0);
        let b = sample_report("pick", 1.001);
        assert!(compare_reports(&a, &b, 0.10).is_empty());
    }

    #[test]
    fn compare_large_ratio_within_relative_tolerance() {
        let a = sample_report("rev-list", 523.82);
        let b = sample_report("rev-list", 530.26);
        assert!(compare_reports(&a, &b, 0.10).is_empty());
    }

    #[test]
    fn compare_skips_when_git_median_drifts() {
        let mut a = sample_report("cat", 12.0);
        let mut b = sample_report("cat", 14.0);
        a.scenarios[0].git = dummy_stats(1.4);
        b.scenarios[0].git = dummy_stats(1.6);
        assert!(compare_reports(&a, &b, 0.10).is_empty());
    }
}
