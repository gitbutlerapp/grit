//! Validate a daily census JSON report.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use super::registry::HEADLINE_SCENARIOS;
use super::schema::{DailyBenchReport, ScenarioId, SCHEMA_TAG};

const PENDING_REMOTE_IDS: [&str; 4] = [
    "fetch/incremental@git-git",
    "pull/fast-forward@git-git",
    "push/incremental@git-git",
    "clone/local@synth-10k",
];

/// Parse `path` and check schema tag, scenario ids, uniqueness, and peak RSS fields.
pub fn validate_daily_report(path: &Path) -> Result<DailyBenchReport> {
    let json = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let report = DailyBenchReport::from_json_str(&json)?;
    validate_report(&report)?;
    Ok(report)
}

fn validate_report(report: &DailyBenchReport) -> Result<()> {
    if report.schema != SCHEMA_TAG {
        anyhow::bail!(
            "unsupported schema tag {} (expected {SCHEMA_TAG})",
            report.schema
        );
    }
    let mut seen = HashSet::new();
    for scenario in &report.scenarios {
        let id = ScenarioId::parse(&scenario.id)
            .with_context(|| format!("invalid scenario id {}", scenario.id))?;
        if !seen.insert(id.0.clone()) {
            anyhow::bail!("duplicate scenario id {}", scenario.id);
        }
        if scenario.pending {
            if PENDING_REMOTE_IDS.contains(&scenario.id.as_str()) {
                eprintln!("pending remote headline: {}", scenario.id);
            }
            continue;
        }
        if scenario.git.peak_rss_bytes == 0 {
            anyhow::bail!("scenario {} missing git peak_rss_bytes", scenario.id);
        }
        if scenario.grit.peak_rss_bytes == 0 {
            anyhow::bail!("scenario {} missing grit peak_rss_bytes", scenario.id);
        }
        if let Some(base) = &scenario.baseline_grit {
            if base.peak_rss_bytes == 0 {
                anyhow::bail!(
                    "scenario {} missing baseline_grit peak_rss_bytes",
                    scenario.id
                );
            }
        }
    }
    for id in PENDING_REMOTE_IDS {
        if let Some(row) = report.scenarios.iter().find(|s| s.id == id) {
            if !row.pending {
                anyhow::bail!("remote headline {id} must be marked pending until step 6");
            }
        }
    }
    let headline_ids: std::collections::HashSet<_> =
        HEADLINE_SCENARIOS.iter().map(|s| s.id).collect();
    for scenario in &report.scenarios {
        if scenario.headline && headline_ids.contains(scenario.id.as_str()) {
            if let Some(spec) = HEADLINE_SCENARIOS.iter().find(|s| s.id == scenario.id) {
                if let Some(name) = &scenario.name {
                    if spec.name != Some(name.as_str()) {
                        anyhow::bail!(
                            "scenario {} name {name} does not match registry {:?}",
                            scenario.id,
                            spec.name
                        );
                    }
                } else {
                    anyhow::bail!("headline scenario {} missing name", scenario.id);
                }
            }
        }
    }
    Ok(())
}
