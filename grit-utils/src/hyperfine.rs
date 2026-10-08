//! Run hyperfine and parse `--export-json` output.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::Deserialize;
use tempfile::NamedTempFile;

use crate::shell::shell_quote;

/// One entry from hyperfine JSON export (times in seconds).
#[derive(Debug, Clone, PartialEq)]
pub struct HyperfineResultEntry {
    pub command: String,
    pub mean: f64,
    pub stddev: Option<f64>,
    pub median: f64,
    pub min: f64,
    pub max: f64,
    pub times: Option<Vec<f64>>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct HyperfineExportV1 {
    pub results: Vec<HyperfineResultEntryV1>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct HyperfineResultEntryV1 {
    pub command: String,
    pub mean: f64,
    #[serde(default)]
    pub stddev: Option<f64>,
    pub median: f64,
    pub min: f64,
    pub max: f64,
    #[serde(default)]
    pub times: Option<Vec<f64>>,
}

#[derive(Debug, Clone, Deserialize)]
struct HyperfineExportV2 {
    pub results: Vec<HyperfineResultEntryV2>,
}

#[derive(Debug, Clone, Deserialize)]
struct HyperfineResultEntryV2 {
    pub command: String,
    pub measurements: Vec<HyperfineMeasurementV2>,
    pub summary: HyperfineSummaryV2,
}

#[derive(Debug, Clone, Deserialize)]
struct HyperfineMeasurementV2 {
    pub time_wall_clock: HyperfineMetricV2,
}

#[derive(Debug, Clone, Deserialize)]
struct HyperfineMetricV2 {
    pub value: f64,
}

#[derive(Debug, Clone, Deserialize)]
struct HyperfineSummaryV2 {
    pub time_wall_clock: HyperfineStatsV2,
}

#[derive(Debug, Clone, Deserialize)]
struct HyperfineStatsV2 {
    pub mean: f64,
    #[serde(default)]
    pub stddev: Option<f64>,
    pub median: f64,
    pub min: f64,
    pub max: f64,
}

/// Options passed to a single hyperfine invocation.
#[derive(Debug, Clone)]
pub struct HyperfineRun {
    pub command: String,
    pub working_directory: PathBuf,
    pub prepare: Option<String>,
    pub warmup: u32,
    pub min_runs: u32,
    pub command_name: Option<String>,
    /// Optional `env VAR=val …` prefix applied before `command` (inside the `cd` wrapper).
    pub env_prefix: Option<String>,
}

/// Run hyperfine and return the single benchmark result.
pub fn run_hyperfine(hyperfine: &Path, run: &HyperfineRun) -> Result<HyperfineResultEntry> {
    let export = NamedTempFile::new().context("create hyperfine export temp file")?;
    let export_path = export.path().to_path_buf();

    let inner = match &run.env_prefix {
        Some(prefix) => format!("{prefix} {}", run.command),
        None => run.command.clone(),
    };
    let command = wrap_in_dir(&run.working_directory, &inner);
    let prepare = run.prepare.as_ref().map(|p| {
        let prep_inner = match &run.env_prefix {
            Some(prefix) => format!("{prefix} {p}"),
            None => p.clone(),
        };
        wrap_in_dir(&run.working_directory, &prep_inner)
    });

    let mut cmd = Command::new(hyperfine);
    cmd.arg("--export-json")
        .arg(&export_path)
        .arg("--warmup")
        .arg(run.warmup.to_string())
        .arg("--min-runs")
        .arg(run.min_runs.to_string());

    if let Some(prepare) = prepare {
        cmd.arg("--prepare").arg(prepare);
    }
    if let Some(name) = &run.command_name {
        cmd.arg("--command-name").arg(name);
    }
    // hyperfine 2.x requires an explicit shell when the command uses `&&` (our `cd … && …` wrapper).
    cmd.arg("--shell=default");
    cmd.arg(command);

    let output = cmd
        .output()
        .with_context(|| format!("failed to execute {}", hyperfine.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("hyperfine failed: {}", stderr.trim());
    }

    let json = fs::read_to_string(&export_path).context("read hyperfine export")?;
    parse_hyperfine_json(&json)
}

fn wrap_in_dir(dir: &Path, inner: &str) -> String {
    format!("cd {} && {}", shell_quote(&dir.to_string_lossy()), inner)
}

fn parse_v2_entry(entry: HyperfineResultEntryV2) -> HyperfineResultEntry {
    let times: Vec<f64> = entry
        .measurements
        .iter()
        .map(|m| m.time_wall_clock.value)
        .collect();
    let wall = &entry.summary.time_wall_clock;
    HyperfineResultEntry {
        command: entry.command,
        mean: wall.mean,
        stddev: wall.stddev,
        median: wall.median,
        min: wall.min,
        max: wall.max,
        times: Some(times),
    }
}

/// Parse hyperfine JSON export; expects exactly one result entry.
pub fn parse_hyperfine_json(json: &str) -> Result<HyperfineResultEntry> {
    let value: serde_json::Value =
        serde_json::from_str(json).context("parse hyperfine JSON export")?;
    let schema_version = value
        .get("schema_version")
        .and_then(|v| v.as_u64())
        .unwrap_or(1);

    let mut results = if schema_version >= 2 {
        let export: HyperfineExportV2 =
            serde_json::from_value(value).context("parse hyperfine v2 export")?;
        export
            .results
            .into_iter()
            .map(parse_v2_entry)
            .collect::<Vec<_>>()
    } else {
        let export: HyperfineExportV1 =
            serde_json::from_str(json).context("parse hyperfine v1 export")?;
        export
            .results
            .into_iter()
            .map(|e| HyperfineResultEntry {
                command: e.command,
                mean: e.mean,
                stddev: e.stddev,
                median: e.median,
                min: e.min,
                max: e.max,
                times: e.times,
            })
            .collect()
    };

    if results.len() != 1 {
        anyhow::bail!(
            "expected exactly one hyperfine result, got {}",
            results.len()
        );
    }
    Ok(results.remove(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const SAMPLE_HYPERFINE_JSON: &str = include_str!("../tests/fixtures/hyperfine_sample.json");

    #[test]
    fn parse_sample_hyperfine_json() {
        let entry = parse_hyperfine_json(SAMPLE_HYPERFINE_JSON).unwrap();
        assert_eq!(entry.command, "sleep 0.1");
        assert!((entry.median - 0.1020).abs() < 1e-6);
        assert_eq!(entry.times.as_ref().map(|t| t.len()), Some(5));
    }

    #[test]
    fn parse_hyperfine_v2_export() {
        const V2: &str = r#"{
  "schema_version": 2,
  "results": [{
    "command": "sleep 0.01",
    "measurements": [
      {"time_wall_clock": {"value": 0.01, "unit": "second"}},
      {"time_wall_clock": {"value": 0.02, "unit": "second"}}
    ],
    "summary": {
      "time_wall_clock": {
        "unit": "second",
        "count": 2,
        "mean": 0.015,
        "stddev": 0.005,
        "median": 0.015,
        "min": 0.01,
        "max": 0.02
      }
    }
  }]
}"#;
        let entry = parse_hyperfine_json(V2).unwrap();
        assert!((entry.median - 0.015).abs() < 1e-9);
        assert_eq!(entry.times.as_deref(), Some([0.01, 0.02].as_slice()));
    }
}
