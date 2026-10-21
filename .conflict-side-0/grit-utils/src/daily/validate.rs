//! Validate a daily census JSON report.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use super::schema::{DailyBenchReport, ScenarioId, SCHEMA_TAG};

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
    Ok(())
}
