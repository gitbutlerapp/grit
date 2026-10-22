//! Serde types for the factory daily benchmark report (`grit-daily-bench/1`).

use std::fmt;
use std::str::FromStr;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::schema::MachineInfo;

fn is_false(value: &bool) -> bool {
    !*value
}

/// Schema tag every daily report must carry (`grit-daily-bench/1`).
pub const SCHEMA_TAG: &str = "grit-daily-bench/1";

/// Stable scenario identifier: `<command>/<case>@<repo>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ScenarioId(pub String);

impl ScenarioId {
    /// Parse and validate the `<command>/<case>@<repo>` format.
    pub fn parse(id: &str) -> Result<Self> {
        let (left, repo) = id
            .split_once('@')
            .with_context(|| format!("scenario id missing '@repo' suffix: {id}"))?;
        if repo.is_empty() {
            anyhow::bail!("scenario id has empty repo segment: {id}");
        }
        if repo.contains('@') {
            anyhow::bail!("scenario id has extra '@' in repo segment: {id}");
        }
        let (command, case) = left
            .split_once('/')
            .with_context(|| format!("scenario id missing '/case' before '@': {id}"))?;
        if command.is_empty() || case.is_empty() {
            anyhow::bail!("scenario id has empty command or case segment: {id}");
        }
        if command.contains('/') || case.contains('/') {
            anyhow::bail!("scenario id must have exactly one '/' before '@': {id}");
        }
        Ok(Self(id.to_string()))
    }

    /// Command segment (before `/`).
    pub fn command(&self) -> &str {
        self.0.split_once('/').map_or("", |(command, _)| command)
    }

    /// Case segment (between `/` and `@`).
    pub fn case(&self) -> &str {
        let Some((left, _repo)) = self.0.split_once('@') else {
            return "";
        };
        left.split_once('/').map_or("", |(_command, case)| case)
    }

    /// Repo shape segment (after `@`).
    pub fn repo(&self) -> &str {
        self.0.split_once('@').map_or("", |(_left, repo)| repo)
    }
}

impl fmt::Display for ScenarioId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ScenarioId {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

/// Git and grit versions recorded in the report header.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DailyToolVersions {
    pub git: String,
    pub grit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grit_commit: Option<String>,
    /// Optional older grit release timed on headline scenarios (`--baseline-grit`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_grit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_grit_commit: Option<String>,
}

/// One tool's timing and memory on a scenario (median wall over runs, max peak RSS).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DailyToolRun {
    pub median_wall_ms: f64,
    pub peak_rss_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runs: Option<u32>,
}

/// One row in the daily matrix.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DailyScenarioResult {
    pub id: String,
    pub area: super::registry::Area,
    #[serde(default, skip_serializing_if = "is_false")]
    pub headline: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub description: String,
    pub git: DailyToolRun,
    pub grit: DailyToolRun,
    /// `grit.median_wall_ms / git.median_wall_ms` (values below 1.0 mean grit is faster).
    pub ratio: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_grit: Option<DailyToolRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_grit_ratio: Option<f64>,
    /// Remote or not-yet-implemented scenario (no timing fields required).
    #[serde(default, skip_serializing_if = "is_false")]
    pub pending: bool,
}

/// Full daily census report (`grit-daily-bench/1`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DailyBenchReport {
    pub schema: String,
    pub generated_at: String,
    pub machine: MachineInfo,
    pub tools: DailyToolVersions,
    pub scenarios: Vec<DailyScenarioResult>,
}

impl DailyBenchReport {
    /// Parse JSON and verify the schema tag.
    pub fn from_json_str(json: &str) -> Result<Self> {
        let report: Self = serde_json::from_str(json)?;
        if report.schema != SCHEMA_TAG {
            anyhow::bail!(
                "unsupported schema tag {} (expected {SCHEMA_TAG})",
                report.schema
            );
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daily::registry::{Area, HEADLINE_SCENARIOS};
    use crate::schema::MachineInfo;

    fn example_report() -> DailyBenchReport {
        DailyBenchReport {
            schema: SCHEMA_TAG.to_string(),
            generated_at: "2026-10-20T00:00:00Z".into(),
            machine: MachineInfo {
                cpu_model: "test".into(),
                physical_cores: 4,
                logical_cores: 4,
                ram_bytes: 16_000_000_000,
                os: "linux".into(),
                kernel: "test".into(),
                scratch_filesystem: "ext4".into(),
                rustc_version: "rustc 1.85.0".into(),
                cargo_profile: "release".into(),
            },
            tools: DailyToolVersions {
                git: "git version 2.43.0".into(),
                grit: "grit 0.5.14".into(),
                grit_commit: Some("abc123".into()),
                baseline_grit: None,
                baseline_grit_commit: None,
            },
            scenarios: vec![DailyScenarioResult {
                id: HEADLINE_SCENARIOS[0].id.to_string(),
                area: Area::Worktree,
                headline: true,
                name: Some(HEADLINE_SCENARIOS[0].name.unwrap().into()),
                description: HEADLINE_SCENARIOS[0].description.into(),
                git: DailyToolRun {
                    median_wall_ms: 8.0,
                    peak_rss_bytes: 12_000_000,
                    runs: Some(5),
                },
                grit: DailyToolRun {
                    median_wall_ms: 13.0,
                    peak_rss_bytes: 14_000_000,
                    runs: Some(5),
                },
                ratio: 13.0 / 8.0,
                baseline_grit: None,
                baseline_grit_ratio: None,
                pending: false,
            }],
        }
    }

    #[test]
    fn roundtrip_example_report() {
        let report = example_report();
        let json = serde_json::to_string_pretty(&report).expect("serialize");
        let back = DailyBenchReport::from_json_str(&json).expect("parse");
        assert_eq!(report, back);
    }

    #[test]
    fn rejects_wrong_schema_tag() {
        let mut report = example_report();
        report.schema = "grit-daily-bench/0".into();
        let json = serde_json::to_string(&report).expect("serialize");
        let err = DailyBenchReport::from_json_str(&json).unwrap_err();
        assert!(err.to_string().contains("unsupported schema"));
    }

    #[test]
    fn scenario_id_parse_display_roundtrip() {
        let id = ScenarioId::parse("status/clean@synth-10k").expect("parse");
        assert_eq!(id.command(), "status");
        assert_eq!(id.case(), "clean");
        assert_eq!(id.repo(), "synth-10k");
        assert_eq!(id.to_string(), "status/clean@synth-10k");
        assert_eq!(
            ScenarioId::from_str("log/oneline-100@git-git").expect("from_str"),
            ScenarioId("log/oneline-100@git-git".into())
        );
    }

    #[test]
    fn rejects_malformed_ids() {
        for bad in [
            "noseparator",
            "status/clean",
            "status/clean@",
            "@synth-10k",
            "/case@repo",
            "cmd/@repo",
            "a/b/c@repo",
            "cmd/case@a@b",
        ] {
            assert!(
                ScenarioId::parse(bad).is_err(),
                "expected parse failure for {bad}"
            );
        }
    }

    #[test]
    fn matrix_ids_are_unique_and_non_headline() {
        use super::super::matrix_registry::MATRIX_SCENARIOS;
        let mut seen = std::collections::HashSet::new();
        for spec in MATRIX_SCENARIOS {
            assert!(!spec.headline);
            assert!(seen.insert(spec.id), "duplicate matrix id {}", spec.id);
        }
    }

    #[test]
    fn headline_ids_match_roadmap() {
        let expected: [(&str, &str); 20] = [
            ("status/clean@synth-10k", "status-clean"),
            ("status/dirty@synth-10k", "status-dirty"),
            ("add/modified@synth-10k", "add"),
            ("commit/small@synth-10k", "commit"),
            ("diff/worktree@synth-10k", "diff"),
            ("log/oneline-100@git-git", "log"),
            ("log/json-100@git-git", "log"),
            ("show/head@git-git", "show"),
            ("blame/file@git-git", "blame"),
            ("switch/branch@synth-10k", "switch"),
            ("branch/list@many-refs", "branch"),
            ("merge/clean@synth-10k", "merge"),
            ("pick/one@synth-10k", "pick"),
            ("stash/push-pop@synth-10k", "stash"),
            ("fetch/incremental@git-git", "fetch"),
            ("pull/fast-forward@git-git", "pull"),
            ("pull/rebase@synth-10k", "pull"),
            ("push/incremental@git-git", "push"),
            ("push/force-with-lease@git-git", "push"),
            ("clone/local@synth-10k", "clone"),
        ];
        assert_eq!(HEADLINE_SCENARIOS.len(), 20);
        for (spec, (exp_id, exp_name)) in HEADLINE_SCENARIOS.iter().zip(expected.iter()) {
            assert_eq!(spec.id, *exp_id);
            assert_eq!(spec.name, Some(*exp_name));
            assert!(spec.headline);
        }
    }
}
