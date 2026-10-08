//! JSON report schema v1 for grit-bench.

use serde::{Deserialize, Serialize};

/// Current report schema version.
pub const SCHEMA_VERSION: u32 = 1;

/// Timing statistics for one tool on one scenario (milliseconds).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TimingStats {
    pub mean_ms: f64,
    pub median_ms: f64,
    pub stddev_ms: f64,
    pub min_ms: f64,
    pub max_ms: f64,
    /// Individual run times in milliseconds.
    pub runs_ms: Vec<f64>,
    /// Peak resident set size of child processes (bytes), from `getrusage(RUSAGE_CHILDREN)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_rss_bytes: Option<u64>,
}

/// Host and toolchain description captured at benchmark time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MachineInfo {
    pub cpu_model: String,
    pub physical_cores: u32,
    pub logical_cores: u32,
    pub ram_bytes: u64,
    pub os: String,
    pub kernel: String,
    pub scratch_filesystem: String,
    pub rustc_version: String,
    pub cargo_profile: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolVersions {
    pub git: String,
    pub grit: String,
    pub grit_commit: Option<String>,
}

/// How the scenario was measured.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DriverKind {
    Cli,
    Lib,
}

/// One benchmark scenario (grit vs git).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScenarioResult {
    pub id: String,
    pub group: String,
    pub fixture: String,
    pub description: String,
    pub driver: DriverKind,
    pub git: TimingStats,
    pub grit: TimingStats,
    /// `grit_median_ms / git_median_ms` (values below 1.0 mean grit is faster).
    pub ratio: f64,
}

/// Full grit-bench report (schema v1).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BenchReport {
    pub schema_version: u32,
    pub timestamp: String,
    pub machine: MachineInfo,
    pub tools: ToolVersions,
    pub scenarios: Vec<ScenarioResult>,
}

impl BenchReport {
    /// Parse and validate schema version.
    pub fn from_json_str(json: &str) -> anyhow::Result<Self> {
        let report: Self = serde_json::from_str(json)?;
        if report.schema_version != SCHEMA_VERSION {
            anyhow::bail!(
                "unsupported schema_version {} (expected {SCHEMA_VERSION})",
                report.schema_version
            );
        }
        Ok(report)
    }
}
