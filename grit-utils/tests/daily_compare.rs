//! Integration tests for `grit-bench daily compare` (library surface).

use grit_utils::daily::compare::{compare_daily_reports, DailyCompareMetric};
use grit_utils::daily::registry::Area;
use grit_utils::daily::schema::{
    DailyBenchReport, DailyScenarioResult, DailyToolRun, DailyToolVersions, SCHEMA_TAG,
};
use grit_utils::schema::MachineInfo;

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
        capture: None,
        scenarios: rows,
    }
}

#[test]
fn flags_ratio_drift_over_tolerance() {
    let a = sample_report(vec![sample_row(
        "status/clean@synth-10k",
        100.0,
        100.0,
        1_000_000,
        900_000,
    )]);
    let b = sample_report(vec![sample_row(
        "status/clean@synth-10k",
        100.0,
        125.0,
        1_000_000,
        900_000,
    )]);
    let outcome = compare_daily_reports(&a, &b, 0.10);
    assert!(!outcome.is_success());
    assert!(outcome
        .mismatches
        .iter()
        .any(|m| m.metric == DailyCompareMetric::Time));
}

#[test]
fn skips_time_ratio_when_either_git_median_sub_20ms() {
    let mut row_b = sample_row("status/clean@synth-10k", 30.0, 60.0, 1_000_000, 900_000);
    row_b.ratio = 3.0;
    let a = sample_report(vec![sample_row(
        "status/clean@synth-10k",
        19.0,
        19.0,
        1_000_000,
        900_000,
    )]);
    let b = sample_report(vec![row_b]);
    let outcome = compare_daily_reports(&a, &b, 0.10);
    assert!(outcome.is_success());
    assert_eq!(outcome.skipped_sub_20ms_time, 1);
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
    let a = sample_report(vec![sample_row(
        "fetch/incremental@git-git",
        50.0,
        50.0,
        git_rss_a,
        (mem_a * git_rss_a as f64) as u64,
    )]);
    let b = sample_report(vec![sample_row(
        "fetch/incremental@git-git",
        50.0,
        50.0,
        git_rss_b,
        (mem_b * git_rss_b as f64) as u64,
    )]);
    let outcome = compare_daily_reports(&a, &b, 0.10);
    assert!(!outcome.is_success());
}
