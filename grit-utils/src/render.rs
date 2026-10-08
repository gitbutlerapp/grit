//! Text, markdown, and JSON renderers for [`BenchReport`].

use std::fmt::Write as _;

use crate::schema::BenchReport;

/// Human-readable text report.
pub fn render_text(report: &BenchReport) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "grit-bench schema v{} — {} vs {}",
        report.schema_version, report.tools.grit, report.tools.git
    );
    let _ = writeln!(out, "{}\n", report.timestamp);
    let _ = writeln!(
        out,
        "Machine: {} ({} logical cores), RAM {} MiB, fs {}",
        report.machine.cpu_model,
        report.machine.logical_cores,
        report.machine.ram_bytes / (1024 * 1024),
        report.machine.scratch_filesystem
    );
    let _ = writeln!(out);

    for scenario in &report.scenarios {
        let _ = writeln!(out, "── {} ({}) ──", scenario.id, scenario.group);
        let _ = writeln!(out, "{}\n", scenario.description);
        let _ = writeln!(
            out,
            "{:>12}  {:>14}  {:>14}  {:>8}",
            "metric", "git (ms)", "grit (ms)", "ratio"
        );
        let _ = writeln!(out, "{}", "─".repeat(56));
        let _ = writeln!(
            out,
            "{:>12}  {:>14.1}  {:>14.1}  {:>8.3}",
            "median", scenario.git.median_ms, scenario.grit.median_ms, scenario.ratio
        );
        let _ = writeln!(
            out,
            "{:>12}  {:>14.1}  {:>14.1}",
            "mean", scenario.git.mean_ms, scenario.grit.mean_ms
        );
        out.push('\n');
    }
    out
}

/// Markdown table: scenario | git | grit | ratio.
pub fn render_markdown(report: &BenchReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# grit-bench results\n");
    let _ = writeln!(out, "- **timestamp:** {}", report.timestamp);
    let _ = writeln!(
        out,
        "- **tools:** grit `{}` vs git `{}`",
        report.tools.grit, report.tools.git
    );
    if let Some(commit) = &report.tools.grit_commit {
        let _ = writeln!(out, "- **grit commit:** `{commit}`");
    }
    let _ = writeln!(out);

    let _ = writeln!(
        out,
        "| scenario | git median (ms) | grit median (ms) | ratio |"
    );
    let _ = writeln!(out, "| --- | ---: | ---: | ---: |");
    for scenario in &report.scenarios {
        let _ = writeln!(
            out,
            "| {} | {:.1} | {:.1} | {:.3} |",
            scenario.id, scenario.git.median_ms, scenario.grit.median_ms, scenario.ratio
        );
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{DriverKind, MachineInfo, ScenarioResult, TimingStats, ToolVersions};

    fn sample_report() -> BenchReport {
        BenchReport {
            schema_version: 1,
            timestamp: "2026-01-01T00:00:00Z".into(),
            machine: MachineInfo {
                cpu_model: "Test CPU".into(),
                physical_cores: 4,
                logical_cores: 8,
                ram_bytes: 16_000_000_000,
                os: "linux".into(),
                kernel: "Linux 6.0".into(),
                scratch_filesystem: "ext4".into(),
                rustc_version: "rustc 1.85".into(),
                cargo_profile: "release".into(),
            },
            tools: ToolVersions {
                git: "git version 2.43".into(),
                grit: "grit 0.5.0".into(),
                grit_commit: Some("abc123".into()),
            },
            scenarios: vec![ScenarioResult {
                id: "status-dirty-100".into(),
                group: "status".into(),
                fixture: "synthetic-100".into(),
                description: "status on dirty tree".into(),
                driver: DriverKind::Cli,
                git: TimingStats {
                    mean_ms: 10.0,
                    median_ms: 9.5,
                    stddev_ms: 0.5,
                    min_ms: 9.0,
                    max_ms: 11.0,
                    runs_ms: vec![9.0, 9.5, 10.0],
                    peak_rss_bytes: None,
                },
                grit: TimingStats {
                    mean_ms: 5.0,
                    median_ms: 4.8,
                    stddev_ms: 0.2,
                    min_ms: 4.5,
                    max_ms: 5.5,
                    runs_ms: vec![4.5, 4.8, 5.5],
                    peak_rss_bytes: None,
                },
                ratio: 0.505,
            }],
        }
    }

    #[test]
    fn markdown_table_golden() {
        let md = render_markdown(&sample_report());
        insta_golden(&md);
        assert!(md.contains("| status-dirty-100 |"));
        assert!(md.contains("| 9.5 |"));
        assert!(md.contains("| 4.8 |"));
        assert!(md.contains("| 0.505 |"));
    }

    fn insta_golden(md: &str) {
        let expected = r#"# grit-bench results

- **timestamp:** 2026-01-01T00:00:00Z
- **tools:** grit `grit 0.5.0` vs git `git version 2.43`
- **grit commit:** `abc123`

| scenario | git median (ms) | grit median (ms) | ratio |
| --- | ---: | ---: | ---: |
| status-dirty-100 | 9.5 | 4.8 | 0.505 |

"#;
        assert_eq!(md, expected);
    }
}
