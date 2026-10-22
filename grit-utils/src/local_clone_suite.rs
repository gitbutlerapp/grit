//! Fully packed local `file://` clone benchmark (synthetic repacked fixture).

use std::path::Path;

use anyhow::Result;
use time::OffsetDateTime;

use crate::binary::{grit_source_commit, tool_version};
use crate::fixture::scratch_dir;
use crate::hyperfine::{run_hyperfine, HyperfineRun};
use crate::machine::{collect_machine_info, format_timestamp};
use crate::parallel_env::{parallel_env_prefix, ThreadMode};
use crate::parallel_fixture::{ensure_fully_packed_local_clone_bare, file_url, parallel_clone_dir};
use crate::scenarios::{Driver, RunConfig};
use crate::schema::{BenchReport, ScenarioResult, ToolVersions, SCHEMA_VERSION};
use crate::shell::shell_command;
use crate::stats::{median_ratio, timing_from_hyperfine};

const LOCAL_CLONE_MIN_WARMUP: u32 = 7;
const LOCAL_CLONE_MIN_RUNS: u32 = 10;

/// Hyperfine grit vs git on `file://` clone into an empty directory (threads=1).
pub fn run_local_clone_suite(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    bench_exe: &Path,
    cfg: &RunConfig,
    file_count: usize,
    timestamp: OffsetDateTime,
) -> Result<BenchReport> {
    let bare = ensure_fully_packed_local_clone_bare(git, file_count)?;
    let url = file_url(&bare)?;
    let clone_dir = parallel_clone_dir();
    let prepare = shell_command(
        bench_exe,
        &[
            "prepare-parallel-clone".into(),
            "--git".into(),
            git.display().to_string(),
            "--url".into(),
            url.clone(),
            "--dest".into(),
            clone_dir.display().to_string(),
        ],
    );
    let mode = ThreadMode::One;
    let env = parallel_env_prefix(mode);
    let git_entry = run_hyperfine(
        hyperfine,
        &HyperfineRun {
            command: shell_command(
                git,
                &[
                    "clone".into(),
                    "-q".into(),
                    url.clone(),
                    clone_dir.display().to_string(),
                ],
            ),
            working_directory: clone_dir.parent().unwrap_or(Path::new(".")).to_path_buf(),
            prepare: Some(prepare.clone()),
            warmup: cfg.warmup.max(LOCAL_CLONE_MIN_WARMUP),
            min_runs: cfg.min_runs.max(LOCAL_CLONE_MIN_RUNS),
            exact_runs: cfg.exact_runs,
            command_name: Some("git".into()),
            env_prefix: Some(env.clone()),
        },
    )?;
    let git_stats = timing_from_hyperfine(&git_entry);
    let grit_entry = run_hyperfine(
        hyperfine,
        &HyperfineRun {
            command: shell_command(
                grit,
                &["clone".into(), url.clone(), clone_dir.display().to_string()],
            ),
            working_directory: clone_dir.parent().unwrap_or(Path::new(".")).to_path_buf(),
            prepare: Some(prepare),
            warmup: cfg.warmup.max(LOCAL_CLONE_MIN_WARMUP),
            min_runs: cfg.min_runs.max(LOCAL_CLONE_MIN_RUNS),
            exact_runs: cfg.exact_runs,
            command_name: Some("grit".into()),
            env_prefix: Some(env),
        },
    )?;
    let grit_stats = timing_from_hyperfine(&grit_entry);
    let ratio = median_ratio(git_stats.median_ms, grit_stats.median_ms);
    let scenario = ScenarioResult {
        id: format!("local-clone-file-{file_count}-packed-t1"),
        group: "local-clone".into(),
        fixture: format!("synthetic-{file_count}-repacked"),
        description: format!(
            "file:// clone into empty directory (fully packed source, {file_count} files) [threads=1]"
        ),
        driver: Driver::Cli.into(),
        git: git_stats,
        grit: grit_stats,
        ratio,
        grit_failure: None,
    };

    Ok(BenchReport {
        schema_version: SCHEMA_VERSION,
        timestamp: format_timestamp(timestamp),
        machine: collect_machine_info(&scratch_dir())?,
        tools: ToolVersions {
            git: tool_version(git),
            grit: tool_version(grit),
            grit_commit: grit_source_commit(),
            hyperfine: None,
        },
        fixtures: vec![],
        scenarios: vec![scenario],
    })
}
