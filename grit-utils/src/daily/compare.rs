//! Compare two `grit-daily-bench/1` reports for consecutive-run stability.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;

use super::schema::{DailyBenchReport, DailyScenarioResult};

/// Minimum git median wall time (ms) on **both** captures before time-ratio drift is enforced.
pub const TIME_RATIO_MIN_GIT_MS: f64 = 20.0;

/// One ratio drift beyond tolerance.
#[derive(Debug, Clone, PartialEq)]
pub struct DailyCompareMismatch {
    pub scenario_id: String,
    pub metric: DailyCompareMetric,
    pub ratio_a: f64,
    pub ratio_b: f64,
    pub delta: f64,
}

/// Which ratio drifted (`grit/git` for time or peak RSS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DailyCompareMetric {
    Time,
    Memory,
}

/// Full compare result including audit counters.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DailyCompareOutcome {
    pub mismatches: Vec<DailyCompareMismatch>,
    pub compared: usize,
    pub skipped_pending: usize,
    pub skipped_sub_20ms_time: usize,
    pub missing_in_candidate: usize,
}

impl DailyCompareOutcome {
    /// Whether time and memory ratio checks passed within tolerance.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.mismatches.is_empty()
    }
}

fn relative_ratio_drift(a: f64, b: f64) -> f64 {
    let base = a.abs().max(b.abs());
    if base <= f64::EPSILON {
        return 0.0;
    }
    (a - b).abs() / base
}

fn ratio_drift_exceeds_tolerance(delta: f64, tolerance: f64) -> bool {
    delta > tolerance
}

fn memory_ratio(scenario: &DailyScenarioResult) -> f64 {
    let git = scenario.git.peak_rss_bytes as f64;
    if git <= f64::EPSILON {
        0.0
    } else {
        scenario.grit.peak_rss_bytes as f64 / git
    }
}

fn git_median_ms(scenario: &DailyScenarioResult) -> f64 {
    scenario.git.median_wall_ms
}

/// Compare per-scenario time and memory **ratios** between two daily reports.
///
/// Time-ratio drift applies only when git median wall time on **both** captures is above
/// [`TIME_RATIO_MIN_GIT_MS`]. Raw git/grit median movement between runs does not skip rows;
/// only stored ratio drift is checked.
pub fn compare_daily_reports(
    a: &DailyBenchReport,
    b: &DailyBenchReport,
    tolerance: f64,
) -> DailyCompareOutcome {
    let map_b: HashMap<&str, &DailyScenarioResult> =
        b.scenarios.iter().map(|s| (s.id.as_str(), s)).collect();

    let mut outcome = DailyCompareOutcome::default();
    for scenario in &a.scenarios {
        if scenario.pending {
            outcome.skipped_pending += 1;
            continue;
        }
        let Some(scenario_b) = map_b.get(scenario.id.as_str()) else {
            outcome.missing_in_candidate += 1;
            outcome.mismatches.push(DailyCompareMismatch {
                scenario_id: scenario.id.clone(),
                metric: DailyCompareMetric::Time,
                ratio_a: scenario.ratio,
                ratio_b: f64::NAN,
                delta: f64::INFINITY,
            });
            continue;
        };
        if scenario_b.pending {
            outcome.skipped_pending += 1;
            continue;
        }
        outcome.compared += 1;

        let git_ms_a = git_median_ms(scenario);
        let git_ms_b = git_median_ms(scenario_b);
        if git_ms_a > TIME_RATIO_MIN_GIT_MS && git_ms_b > TIME_RATIO_MIN_GIT_MS {
            let ratio_b = scenario_b.ratio;
            let delta = relative_ratio_drift(scenario.ratio, ratio_b);
            if ratio_drift_exceeds_tolerance(delta, tolerance) {
                outcome.mismatches.push(DailyCompareMismatch {
                    scenario_id: scenario.id.clone(),
                    metric: DailyCompareMetric::Time,
                    ratio_a: scenario.ratio,
                    ratio_b,
                    delta,
                });
            }
        } else {
            outcome.skipped_sub_20ms_time += 1;
        }

        let mem_a = memory_ratio(scenario);
        let mem_b = memory_ratio(scenario_b);
        let mem_delta = relative_ratio_drift(mem_a, mem_b);
        if ratio_drift_exceeds_tolerance(mem_delta, tolerance) {
            outcome.mismatches.push(DailyCompareMismatch {
                scenario_id: scenario.id.clone(),
                metric: DailyCompareMetric::Memory,
                ratio_a: mem_a,
                ratio_b: mem_b,
                delta: mem_delta,
            });
        }
    }
    outcome
}

/// Load two daily JSON files and compare.
pub fn compare_daily_files(
    path_a: &Path,
    path_b: &Path,
    tolerance: f64,
) -> Result<DailyCompareOutcome> {
    let a = DailyBenchReport::from_json_str(&std::fs::read_to_string(path_a)?)?;
    let b = DailyBenchReport::from_json_str(&std::fs::read_to_string(path_b)?)?;
    Ok(compare_daily_reports(&a, &b, tolerance))
}

/// Markdown table summarizing ratio drift (for CI transcripts and docs).
#[must_use]
pub fn render_daily_compare_markdown(
    path_a: &Path,
    path_b: &Path,
    tolerance: f64,
    outcome: &DailyCompareOutcome,
) -> String {
    let mut lines = vec![
        format!(
            "## Daily compare (`{tolerance}` tolerance)",
            tolerance = tolerance
        ),
        String::new(),
        format!("- baseline: `{}`", path_a.display()),
        format!("- candidate: `{}`", path_b.display()),
        format!(
            "- compared rows: **{}** (skipped pending {}, sub-20ms time {}, missing {})",
            outcome.compared,
            outcome.skipped_pending,
            outcome.skipped_sub_20ms_time,
            outcome.missing_in_candidate
        ),
        String::new(),
        "| scenario | metric | ratio A | ratio B | drift | status |".to_string(),
        "| --- | --- | ---: | ---: | ---: | --- |".to_string(),
    ];
    if outcome.mismatches.is_empty() {
        lines.push("| *(none)* | | | | | ok |".to_string());
    } else {
        for m in &outcome.mismatches {
            let metric = match m.metric {
                DailyCompareMetric::Time => "time",
                DailyCompareMetric::Memory => "memory",
            };
            lines.push(format!(
                "| `{}` | {metric} | {:.4} | {:.4} | {:.4} | **fail** |",
                m.scenario_id, m.ratio_a, m.ratio_b, m.delta
            ));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daily::registry::Area;
    use crate::daily::schema::{DailyToolRun, DailyToolVersions, SCHEMA_TAG};
    use crate::schema::MachineInfo;

    fn tool_run(median_ms: f64, peak_rss: u64) -> DailyToolRun {
        DailyToolRun {
            median_wall_ms: median_ms,
            peak_rss_bytes: peak_rss,
            runs: Some(3),
        }
    }

    fn sample_row(
        id: &str,
        git_ms: f64,
        grit_ms: f64,
        git_rss: u64,
        grit_rss: u64,
    ) -> DailyScenarioResult {
        DailyScenarioResult {
            id: id.into(),
            area: Area::Worktree,
            headline: false,
            name: None,
            description: "test".into(),
            git: tool_run(git_ms, git_rss),
            grit: tool_run(grit_ms, grit_rss),
            ratio: if git_ms > 0.0 { grit_ms / git_ms } else { 0.0 },
            baseline_grit: None,
            baseline_grit_ratio: None,
            pending: false,
        }
    }

    fn sample_report(rows: Vec<DailyScenarioResult>) -> DailyBenchReport {
        DailyBenchReport {
            schema: SCHEMA_TAG.into(),
            generated_at: "2026-01-01T00:00:00Z".into(),
            machine: MachineInfo {
                cpu_model: "test".into(),
                physical_cores: 8,
                logical_cores: 8,
                ram_bytes: 1,
                os: "linux".into(),
                kernel: "test".into(),
                scratch_filesystem: "tmpfs".into(),
                rustc_version: "test".into(),
                cargo_profile: "release".into(),
            },
            tools: DailyToolVersions {
                git: "git".into(),
                grit: "grit".into(),
                grit_commit: None,
                baseline_grit: None,
                baseline_grit_commit: None,
            },
            scenarios: rows,
            capture: None,
        }
    }

    #[test]
    fn flags_ratio_drift_over_tolerance() {
        let mut row_b = sample_row("status/clean@synth-10k", 100.0, 100.0, 1_000_000, 900_000);
        row_b.ratio = 1.25;
        let a = sample_report(vec![sample_row(
            "status/clean@synth-10k",
            100.0,
            100.0,
            1_000_000,
            900_000,
        )]);
        let b = sample_report(vec![row_b]);
        let outcome = compare_daily_reports(&a, &b, 0.10);
        assert_eq!(outcome.skipped_sub_20ms_time, 0);
        assert!(!outcome.is_success());
        assert!(outcome
            .mismatches
            .iter()
            .any(|m| m.metric == DailyCompareMetric::Time));
    }

    #[test]
    fn ignores_sub_20ms_time_drift() {
        let mut row_b = sample_row("status/clean@small", 10.0, 10.0, 500_000, 500_000);
        row_b.ratio = 2.0;
        let a = sample_report(vec![sample_row(
            "status/clean@small",
            10.0,
            10.0,
            500_000,
            500_000,
        )]);
        let b = sample_report(vec![row_b]);
        let outcome = compare_daily_reports(&a, &b, 0.10);
        assert!(outcome.is_success());
        assert_eq!(outcome.skipped_sub_20ms_time, 1);
    }

    #[test]
    fn compares_memory_ratios() {
        let a = sample_report(vec![sample_row(
            "log/oneline@synth-10k",
            50.0,
            50.0,
            1_000_000,
            800_000,
        )]);
        let b = sample_report(vec![sample_row(
            "log/oneline@synth-10k",
            50.0,
            50.0,
            1_000_000,
            1_200_000,
        )]);
        let outcome = compare_daily_reports(&a, &b, 0.10);
        assert!(!outcome.is_success());
        assert!(outcome
            .mismatches
            .iter()
            .any(|m| m.metric == DailyCompareMetric::Memory));
    }

    #[test]
    fn rejects_memory_drift_just_over_tenth_boundary() {
        let mem_a = 2.365_442_860_3;
        let mem_b = 2.111_474_971_6;
        let git_rss_a = 55_447_552_u64;
        let git_rss_b = 61_288_448_u64;
        let grit_rss_a = (mem_a * git_rss_a as f64) as u64;
        let grit_rss_b = (mem_b * git_rss_b as f64) as u64;
        let a = sample_report(vec![sample_row(
            "fetch/incremental@git-git",
            50.0,
            50.0,
            git_rss_a,
            grit_rss_a,
        )]);
        let b = sample_report(vec![sample_row(
            "fetch/incremental@git-git",
            50.0,
            50.0,
            git_rss_b,
            grit_rss_b,
        )]);
        let delta = relative_ratio_drift(mem_a, mem_b);
        assert!(delta > 0.10);
        assert!(delta < 0.11);
        let outcome = compare_daily_reports(&a, &b, 0.10);
        assert!(!outcome.is_success());
    }
}
