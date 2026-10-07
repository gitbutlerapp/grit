//! Scenario definitions and hyperfine execution.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::binary::{grit_source_commit, tool_version};
use crate::fixture::{create_repo, dirty_repo, prepare_add_iteration, scratch_dir};
use crate::hyperfine::{run_hyperfine, HyperfineRun};
use crate::machine::{collect_machine_info, format_timestamp};
use crate::schema::{BenchReport, DriverKind, ScenarioResult, ToolVersions, SCHEMA_VERSION};
use crate::shell::shell_command;
use crate::stats::{median_ratio, timing_from_hyperfine};

/// Driver selection for a scenario (CLI today; library hooks reserved).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Driver {
    Cli,
    #[allow(dead_code)]
    Lib,
}

impl From<Driver> for DriverKind {
    fn from(value: Driver) -> Self {
        match value {
            Driver::Cli => DriverKind::Cli,
            Driver::Lib => DriverKind::Lib,
        }
    }
}

/// One benchmark scenario specification.
#[derive(Debug, Clone)]
pub struct Scenario {
    pub id: String,
    pub group: String,
    pub fixture: String,
    pub description: String,
    pub grit_argv: Vec<String>,
    pub git_argv: Vec<String>,
    pub driver: Driver,
    pub prepare_kind: Option<PrepareKind>,
}

#[derive(Debug, Clone, Copy)]
pub enum PrepareKind {
    AddIteration,
}

/// Hyperfine tuning for all scenarios in a run.
#[derive(Debug, Clone)]
pub struct RunConfig {
    pub warmup: u32,
    pub min_runs: u32,
    pub prepare_bin: PathBuf,
}

fn bench_tool(
    hyperfine: &Path,
    program: &Path,
    args: &[String],
    cwd: &Path,
    cfg: &RunConfig,
    prepare: Option<&str>,
    command_name: &str,
) -> Result<crate::schema::TimingStats> {
    let entry = run_hyperfine(
        hyperfine,
        &HyperfineRun {
            command: shell_command(program, args),
            working_directory: cwd.to_path_buf(),
            prepare: prepare.map(str::to_string),
            warmup: cfg.warmup,
            min_runs: cfg.min_runs,
            command_name: Some(command_name.into()),
        },
    )?;
    Ok(timing_from_hyperfine(&entry))
}

fn prepare_command(cfg: &RunConfig, git: &Path) -> String {
    shell_command(
        &cfg.prepare_bin,
        &[
            "prepare-add".to_string(),
            "--git".to_string(),
            git.display().to_string(),
        ],
    )
}

/// Run one scenario at the given repo path.
pub fn run_scenario(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    scenario: &Scenario,
    repo: &Path,
) -> Result<ScenarioResult> {
    let prepare = scenario.prepare_kind.map(|_| prepare_command(cfg, git));

    let git_stats = bench_tool(
        hyperfine,
        git,
        &scenario.git_argv,
        repo,
        cfg,
        prepare.as_deref(),
        "git",
    )?;
    let grit_stats = bench_tool(
        hyperfine,
        grit,
        &scenario.grit_argv,
        repo,
        cfg,
        prepare.as_deref(),
        "grit",
    )?;

    let ratio = median_ratio(git_stats.median_ms, grit_stats.median_ms);

    Ok(ScenarioResult {
        id: scenario.id.clone(),
        group: scenario.group.clone(),
        fixture: scenario.fixture.clone(),
        description: scenario.description.clone(),
        driver: scenario.driver.into(),
        git: git_stats,
        grit: grit_stats,
        ratio,
    })
}

/// Status benchmarks (dirty + clean) for each file count.
pub fn run_status_suite(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    sizes: &[usize],
    timestamp: time::OffsetDateTime,
) -> Result<BenchReport> {
    let mut scenarios = Vec::new();
    for &size in sizes {
        let repo = create_repo(git, size)?;
        dirty_repo(&repo, size)?;
        let fixture = format!("synthetic-{size}");
        scenarios.push(run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: format!("status-dirty-{size}"),
                group: "status".into(),
                fixture: fixture.clone(),
                description:
                    "status on a dirty worktree (~10% modified, ~5% untracked; git -s, grit default)"
                        .into(),
                grit_argv: vec!["status".into()],
                git_argv: vec!["status".into(), "-s".into()],
                driver: Driver::Cli,
                prepare_kind: None,
            },
            &repo,
        )?);

        let repo_clean = create_repo(git, size)?;
        scenarios.push(run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: format!("status-clean-{size}"),
                group: "status".into(),
                fixture: fixture.clone(),
                description: "status on a clean worktree (git -s, grit default)".into(),
                grit_argv: vec!["status".into()],
                git_argv: vec!["status".into(), "-s".into()],
                driver: Driver::Cli,
                prepare_kind: None,
            },
            &repo_clean,
        )?);
    }

    Ok(build_report(git, grit, timestamp, scenarios))
}

/// Stage-all benchmark for each file count (`git add -A` vs `grit add`).
pub fn run_add_suite(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    sizes: &[usize],
    timestamp: time::OffsetDateTime,
) -> Result<BenchReport> {
    let mut scenarios = Vec::new();
    for &size in sizes {
        let repo = create_repo(git, size)?;
        prepare_add_iteration(&repo, git)
            .with_context(|| format!("initial add-bench setup for {size} files"))?;
        scenarios.push(run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: format!("add-{size}"),
                group: "add".into(),
                fixture: format!("synthetic-{size}"),
                description:
                    "stage all changes after modifying ~20% of files (git reset between runs)"
                        .into(),
                grit_argv: vec!["add".into()],
                git_argv: vec!["add".into(), "-A".into()],
                driver: Driver::Cli,
                prepare_kind: Some(PrepareKind::AddIteration),
            },
            &repo,
        )?);
    }
    Ok(build_report(git, grit, timestamp, scenarios))
}

fn build_report(
    git: &Path,
    grit: &Path,
    timestamp: time::OffsetDateTime,
    scenarios: Vec<ScenarioResult>,
) -> BenchReport {
    BenchReport {
        schema_version: SCHEMA_VERSION,
        timestamp: format_timestamp(timestamp),
        machine: collect_machine_info(&scratch_dir()).unwrap_or_else(|_| {
            collect_machine_info(Path::new("/tmp")).unwrap_or_else(|_| {
                use crate::schema::MachineInfo;
                MachineInfo {
                    cpu_model: "unknown".into(),
                    physical_cores: 0,
                    logical_cores: 0,
                    ram_bytes: 0,
                    os: std::env::consts::OS.into(),
                    kernel: "unknown".into(),
                    scratch_filesystem: "unknown".into(),
                    rustc_version: "unknown".into(),
                    cargo_profile: crate::machine::grit_bench_cargo_profile(),
                }
            })
        }),
        tools: ToolVersions {
            git: tool_version(git),
            grit: tool_version(grit),
            grit_commit: grit_source_commit(),
        },
        scenarios,
    }
}

/// Shared prepare hook for `add` scenarios (invoked by hyperfine `--prepare`).
pub fn run_prepare_add(git: &Path) -> Result<()> {
    prepare_add_iteration(&scratch_dir(), git)
}
