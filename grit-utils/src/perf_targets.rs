//! Performance target ledger (`perf-targets.toml`) for the campaign scorecard.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::schema::{BenchReport, ScenarioResult};

/// Default grit/git ratio target when a scenario has no explicit entry.
pub const DEFAULT_TARGET_RATIO: f64 = 1.1;

/// Ledger row classification for the 1.1× scorecard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerStatus {
    Pass,
    Fail,
    Documented,
    OwnedByItem14,
}

impl LedgerStatus {
    /// Stable label for markdown tables.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Documented => "documented",
            Self::OwnedByItem14 => "owned-by-item-14",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ScenarioTarget {
    pub ratio: Option<f64>,
    pub documented: Option<String>,
    pub followup: Option<String>,
    pub owned_by_item_14: bool,
}

#[derive(Debug, Clone)]
pub struct PerfTargets {
    default_ratio: f64,
    scenarios: HashMap<String, ScenarioTarget>,
}

#[derive(Debug, Deserialize)]
struct RawPerfTargets {
    #[serde(default)]
    default: RawDefault,
    #[serde(default)]
    scenario: HashMap<String, RawScenarioEntry>,
}

#[derive(Debug, Default, Deserialize)]
struct RawDefault {
    #[serde(default = "default_ratio")]
    ratio: f64,
}

#[derive(Debug, Default, Deserialize)]
struct RawScenarioEntry {
    ratio: Option<f64>,
    documented: Option<String>,
    followup: Option<String>,
    owned_by_item_14: Option<bool>,
}

fn default_ratio() -> f64 {
    DEFAULT_TARGET_RATIO
}

/// Load committed perf targets from `path` (typically `grit-utils/perf-targets.toml`).
pub fn load_perf_targets(path: &Path) -> Result<PerfTargets> {
    let raw_text = std::fs::read_to_string(path)
        .with_context(|| format!("read perf targets {}", path.display()))?;
    let raw: RawPerfTargets = toml::from_str(&raw_text).context("parse perf-targets.toml")?;
    let mut scenarios = HashMap::new();
    for (id, entry) in raw.scenario {
        scenarios.insert(
            id,
            ScenarioTarget {
                ratio: entry.ratio,
                documented: entry.documented,
                followup: entry.followup,
                owned_by_item_14: entry.owned_by_item_14.unwrap_or(false),
            },
        );
    }
    Ok(PerfTargets {
        default_ratio: raw.default.ratio,
        scenarios,
    })
}

impl PerfTargets {
    /// Resolve the target ratio for `scenario_id`.
    pub fn target_ratio(&self, scenario_id: &str) -> f64 {
        self.scenarios
            .get(scenario_id)
            .and_then(|e| e.ratio)
            .unwrap_or(self.default_ratio)
    }

    /// Classify one measured scenario against targets.
    pub fn classify(&self, scenario: &ScenarioResult) -> LedgerStatus {
        if is_owned_by_item_14(scenario.id.as_str())
            || self
                .scenarios
                .get(&scenario.id)
                .is_some_and(|e| e.owned_by_item_14)
        {
            return LedgerStatus::OwnedByItem14;
        }
        if self
            .scenarios
            .get(&scenario.id)
            .and_then(|e| e.documented.as_ref())
            .is_some()
        {
            return LedgerStatus::Documented;
        }
        if scenario.grit_failure.is_some() {
            return LedgerStatus::Fail;
        }
        let target = self.target_ratio(&scenario.id);
        if scenario.ratio <= target {
            LedgerStatus::Pass
        } else {
            LedgerStatus::Fail
        }
    }

    /// Documented reason text when present.
    pub fn documented_reason(&self, scenario_id: &str) -> Option<&str> {
        self.scenarios
            .get(scenario_id)
            .and_then(|e| e.documented.as_deref())
    }

    /// Follow-up roadmap reference when present.
    pub fn followup(&self, scenario_id: &str) -> Option<&str> {
        self.scenarios
            .get(scenario_id)
            .and_then(|e| e.followup.as_deref())
    }
}

/// Scenario ids owned by roadmap item 14 (not scored in this campaign step).
pub fn is_owned_by_item_14(scenario_id: &str) -> bool {
    scenario_id.contains("rev-list-count")
        || scenario_id.contains("rev-list-count-objects")
        || scenario_id.contains("serve-clone")
        || scenario_id.contains("revs-count-deep")
}

/// Load a committed grit-bench JSON report (`schema_version` 1).
pub fn load_bench_report(path: &Path) -> Result<BenchReport> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read bench report {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse bench report {}", path.display()))
}

/// Scenario ids classified as `fail` against `targets`.
pub fn ledger_failures(report: &BenchReport, targets: &PerfTargets) -> Vec<String> {
    report
        .scenarios
        .iter()
        .filter(|s| targets.classify(s) == LedgerStatus::Fail)
        .map(|s| s.id.clone())
        .collect()
}

/// Render a markdown scorecard table for `report` using `targets`.
pub fn render_ledger_markdown(
    report: &crate::schema::BenchReport,
    targets: &PerfTargets,
) -> String {
    let mut out = String::from("| scenario | git ms | grit ms | ratio | status |\n");
    out.push_str("| --- | ---: | ---: | ---: | --- |\n");
    for scenario in &report.scenarios {
        let status = targets.classify(scenario);
        out.push_str(&format!(
            "| {} | {:.1} | {:.1} | {:.3} | {} |\n",
            scenario.id,
            scenario.git.median_ms,
            scenario.grit.median_ms,
            scenario.ratio,
            status.as_str()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{DriverKind, TimingStats};

    fn stats(median: f64) -> TimingStats {
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

    fn scenario(id: &str, ratio: f64) -> ScenarioResult {
        scenario_with_failure(id, ratio, None)
    }

    fn scenario_with_failure(
        id: &str,
        ratio: f64,
        grit_failure: Option<crate::schema::BenchFailure>,
    ) -> ScenarioResult {
        ScenarioResult {
            id: id.into(),
            group: "g".into(),
            fixture: "f".into(),
            description: "d".into(),
            driver: DriverKind::Cli,
            git: stats(100.0),
            grit: stats(ratio * 100.0),
            ratio,
            grit_failure,
        }
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn parse_perf_targets_and_classify() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("perf-targets.toml");
        std::fs::write(
            &path,
            r#"
[default]
ratio = 1.1

[scenario."slow-add"]
documented = "index stat cache not yet trusted"
followup = "ROADMAP item 2"

[scenario."rev-list-count-git.git"]
owned_by_item_14 = true
"#,
        )?;
        let targets = load_perf_targets(&path)?;
        assert!((targets.target_ratio("unknown") - 1.1).abs() < f64::EPSILON);
        assert_eq!(
            targets.classify(&scenario("slow-add", 2.0)),
            LedgerStatus::Documented
        );
        assert_eq!(
            targets.classify(&scenario("rev-list-count-git.git", 99.0)),
            LedgerStatus::OwnedByItem14
        );
        assert_eq!(
            targets.classify(&scenario("status-clean", 1.0)),
            LedgerStatus::Pass
        );
        assert_eq!(
            targets.classify(&scenario("status-clean", 1.2)),
            LedgerStatus::Fail
        );
        let mut failed = scenario("status-clean", 0.0);
        failed.grit_failure = Some(crate::schema::BenchFailure {
            kind: "command_failed".into(),
            message: "oom".into(),
        });
        assert_eq!(targets.classify(&failed), LedgerStatus::Fail);
        Ok(())
    }

    #[test]
    fn owned_by_item_14_pattern() {
        assert!(is_owned_by_item_14("git-dot-git/rev-list-count"));
        assert!(is_owned_by_item_14("serve-clone-git.git"));
        assert!(!is_owned_by_item_14("git-dot-git/status-clean"));
    }

    #[test]
    fn grit_failure_without_documentation_is_fail() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("perf-targets.toml");
        std::fs::write(
            &path,
            r#"
[default]
ratio = 1.1
"#,
        )?;
        let targets = load_perf_targets(&path)?;
        let failure = Some(crate::schema::BenchFailure {
            kind: "command_failed".into(),
            message: "hyperfine grit run failed".into(),
        });
        assert_eq!(
            targets.classify(&scenario_with_failure(
                "git-dot-git/log",
                0.0,
                failure.clone()
            )),
            LedgerStatus::Fail
        );
        Ok(())
    }

    #[test]
    fn grit_failure_with_documentation_stays_documented() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("perf-targets.toml");
        std::fs::write(
            &path,
            r#"
[default]
ratio = 1.1

[scenario."git-dot-git/fsck"]
documented = "known fsck gap"
"#,
        )?;
        let targets = load_perf_targets(&path)?;
        let failure = Some(crate::schema::BenchFailure {
            kind: "command_failed".into(),
            message: "fsck exited non-zero".into(),
        });
        assert_eq!(
            targets.classify(&scenario_with_failure("git-dot-git/fsck", 0.0, failure)),
            LedgerStatus::Documented
        );
        Ok(())
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn load_committed_campaign_before_report() -> Result<()> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("baselines/campaign-before.json");
        if !path.is_file() {
            return Ok(());
        }
        let report = load_bench_report(&path)?;
        assert!(!report.scenarios.is_empty());
        Ok(())
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn committed_campaign_after_closes_ledger() -> Result<()> {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let report_path = manifest.join("baselines/campaign-after.json");
        let targets_path = manifest.join("perf-targets.toml");
        if !report_path.is_file() {
            return Ok(());
        }
        let targets = load_perf_targets(&targets_path)?;
        let report = load_bench_report(&report_path)?;
        assert!(!report.scenarios.is_empty());
        let fails = ledger_failures(&report, &targets);
        assert!(
            fails.is_empty(),
            "campaign-after ledger has undocumented failures: {:?}",
            fails
        );
        for scenario in &report.scenarios {
            let status = targets.classify(scenario);
            assert_ne!(
                status,
                LedgerStatus::Fail,
                "scenario {} classified as fail",
                scenario.id
            );
            if status == LedgerStatus::Documented {
                let followup = targets
                    .followup(&scenario.id)
                    .expect("documented row must have followup");
                assert!(
                    followup.contains("ROADMAP item"),
                    "followup for {} must reference ROADMAP: {}",
                    scenario.id,
                    followup
                );
            }
        }
        Ok(())
    }
}
