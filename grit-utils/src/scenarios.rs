//! Scenario definitions and hyperfine execution.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::bench_env::isolated_env_prefix;
use crate::binary::{grit_source_commit, tool_version};
use crate::fixture::{
    create_repo, dirty_repo, prepare_add_iteration, prepare_commit_iteration, scratch_dir,
};
use crate::hot_path_fixture::{
    load_meta, prepare_merge, prepare_pick, prepare_pick_series, prepare_switch,
    setup_pick_merge_fixture, setup_pick_series_fixture, setup_switch_fixture,
    touch_paths_for_size, HotPathMeta, HotPathRepoSpec,
};
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
    /// When set, run argv via `/bin/sh` instead of invoking the tool binary directly.
    pub grit_via_shell: bool,
    pub git_via_shell: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum PrepareKind {
    AddIteration,
    SwitchReset,
    PickReset,
    MergeReset,
    PickSeriesReset,
    CommitIteration,
}

/// Hyperfine tuning for all scenarios in a run.
#[derive(Debug, Clone)]
pub struct RunConfig {
    pub warmup: u32,
    pub min_runs: u32,
    pub prepare_bin: PathBuf,
    /// When true, scenarios use `GIT_CONFIG_NOSYSTEM` and an empty global config file.
    pub isolated_config: bool,
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
    let env_prefix = cfg.isolated_config.then(isolated_env_prefix);
    let entry = run_hyperfine(
        hyperfine,
        &HyperfineRun {
            command: shell_command(program, args),
            working_directory: cwd.to_path_buf(),
            prepare: prepare.map(str::to_string),
            warmup: cfg.warmup,
            min_runs: cfg.min_runs,
            command_name: Some(command_name.into()),
            env_prefix,
        },
    )?;
    Ok(timing_from_hyperfine(&entry))
}

fn prepare_command(cfg: &RunConfig, git: &Path, kind: PrepareKind) -> String {
    let sub = match kind {
        PrepareKind::AddIteration => "prepare-add",
        PrepareKind::SwitchReset => "prepare-switch",
        PrepareKind::PickReset => "prepare-pick",
        PrepareKind::MergeReset => "prepare-merge",
        PrepareKind::PickSeriesReset => "prepare-pick-series",
        PrepareKind::CommitIteration => "prepare-commit",
    };
    shell_command(
        &cfg.prepare_bin,
        &[
            sub.to_string(),
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
    let prepare = scenario
        .prepare_kind
        .map(|kind| prepare_command(cfg, git, kind));

    let git_prog = if scenario.git_via_shell {
        PathBuf::from("/bin/sh")
    } else {
        git.to_path_buf()
    };
    let grit_prog = if scenario.grit_via_shell {
        PathBuf::from("/bin/sh")
    } else {
        grit.to_path_buf()
    };

    let git_stats = bench_tool(
        hyperfine,
        &git_prog,
        &scenario.git_argv,
        repo,
        cfg,
        prepare.as_deref(),
        "git",
    )?;
    let grit_stats = bench_tool(
        hyperfine,
        &grit_prog,
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
                grit_via_shell: false,
                git_via_shell: false,
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
                grit_via_shell: false,
                git_via_shell: false,
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
                grit_via_shell: false,
                git_via_shell: false,
            },
            &repo,
        )?);
    }
    Ok(build_report(git, grit, timestamp, scenarios))
}

/// `grit commit` (stage all + commit) vs `git add -A && git commit` at each file count.
pub fn run_commit_suite(
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
        prepare_commit_iteration(&repo, git)
            .with_context(|| format!("initial commit-bench setup for {size} files"))?;
        let git_cmd = chain_shell(
            git,
            &[
                vec!["add".into(), "-A".into()],
                vec!["commit".into(), "-q".into(), "-m".into(), "bench".into()],
            ],
        );
        scenarios.push(run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: format!("commit-{size}"),
                group: "commit".into(),
                fixture: format!("synthetic-{size}"),
                description:
                    "stage all + commit after modifying ~20% of files (git reset between runs)"
                        .into(),
                grit_argv: vec!["commit".into(), "bench".into()],
                git_argv: sh_script(git_cmd),
                driver: Driver::Cli,
                prepare_kind: Some(PrepareKind::CommitIteration),
                grit_via_shell: false,
                git_via_shell: true,
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

pub fn run_prepare_commit(git: &Path) -> Result<()> {
    prepare_commit_iteration(&scratch_dir(), git)
}

pub fn run_prepare_switch(git: &Path) -> Result<()> {
    prepare_switch(git, &scratch_dir())
}

pub fn run_prepare_pick(git: &Path) -> Result<()> {
    prepare_pick(git, &scratch_dir())
}

pub fn run_prepare_merge(git: &Path) -> Result<()> {
    prepare_merge(git, &scratch_dir())
}

pub fn run_prepare_pick_series(git: &Path) -> Result<()> {
    prepare_pick_series(git, &scratch_dir())
}

fn chain_shell(program: &Path, invocations: &[Vec<String>]) -> String {
    invocations
        .iter()
        .map(|args| shell_command(program, args))
        .collect::<Vec<_>>()
        .join(" && ")
}

fn hot_path_spec(file_count: usize, fsmonitor: bool) -> HotPathRepoSpec {
    let mut spec = HotPathRepoSpec::for_size(file_count);
    spec.fsmonitor = fsmonitor;
    spec
}

fn sh_script(script: String) -> Vec<String> {
    vec!["-c".into(), script]
}

fn switch_argv(grit: &Path, git: &Path, meta: &HotPathMeta) -> (Vec<String>, Vec<String>) {
    let grit_cmd = chain_shell(
        grit,
        &[
            vec!["switch".into(), meta.branch_b.clone()],
            vec!["switch".into(), meta.branch_a.clone()],
        ],
    );
    let git_cmd = chain_shell(
        git,
        &[
            vec!["switch".into(), "-q".into(), meta.branch_b.clone()],
            vec!["switch".into(), "-q".into(), meta.branch_a.clone()],
        ],
    );
    (sh_script(grit_cmd), sh_script(git_cmd))
}

fn pick_argv(meta: &HotPathMeta) -> (Vec<String>, Vec<String>) {
    (
        vec!["pick".into(), meta.pick_commit.clone()],
        vec![
            "cherry-pick".into(),
            "--no-edit".into(),
            meta.pick_commit.clone(),
        ],
    )
}

fn merge_argv(meta: &HotPathMeta) -> (Vec<String>, Vec<String>) {
    (
        vec!["merge".into(), meta.topic_branch.clone()],
        vec![
            "merge".into(),
            "-q".into(),
            "--no-edit".into(),
            meta.topic_branch.clone(),
        ],
    )
}

/// Switch, pick, merge, and pick-series scenarios at each file count.
pub fn run_hot_path_suite(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    sizes: &[usize],
    fsmonitor: bool,
    timestamp: time::OffsetDateTime,
) -> Result<BenchReport> {
    let mut scenarios = Vec::new();
    for &size in sizes {
        let spec = hot_path_spec(size, fsmonitor);
        let fixture = format!("synthetic-{size}");
        let fs_suffix = if fsmonitor { "-fsmn" } else { "" };

        let (_repo, meta) = setup_switch_fixture(git, spec, false)?;
        let (grit_sw, git_sw) = switch_argv(grit, git, &meta);
        let repo = scratch_dir();
        scenarios.push(run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: format!("switch-{size}{fs_suffix}"),
                group: "switch".into(),
                fixture: fixture.clone(),
                description: "switch between branches differing in ~50 paths (out and back)".into(),
                grit_argv: grit_sw,
                git_argv: git_sw,
                driver: Driver::Cli,
                prepare_kind: Some(PrepareKind::SwitchReset),
                grit_via_shell: true,
                git_via_shell: true,
            },
            &repo,
        )?);

        let meta_wide = {
            let spec_wide = spec;
            setup_switch_fixture(git, spec_wide, true)?;
            load_meta(&scratch_dir())?
        };
        let (grit_w, git_w) = switch_argv(grit, git, &meta_wide);
        scenarios.push(run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: format!("switch-wide-{size}{fs_suffix}"),
                group: "switch".into(),
                fixture: fixture.clone(),
                description:
                    "switch between branches differing in ~10% of paths including directory deletions"
                        .into(),
                grit_argv: grit_w,
                git_argv: git_w,
                driver: Driver::Cli,
                prepare_kind: Some(PrepareKind::SwitchReset),
                grit_via_shell: true,
                git_via_shell: true,
            },
            &scratch_dir(),
        )?);

        let touch = touch_paths_for_size(size);
        setup_pick_merge_fixture(git, spec, touch)?;
        let meta_pick = load_meta(&scratch_dir())?;
        let (grit_pick, git_pick) = pick_argv(&meta_pick);
        let repo_pick = scratch_dir();
        scenarios.push(run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: format!("pick-{size}{fs_suffix}"),
                group: "pick".into(),
                fixture: fixture.clone(),
                description: format!(
                    "cherry-pick one commit touching {touch} paths onto a sibling branch"
                ),
                grit_argv: grit_pick,
                git_argv: git_pick,
                driver: Driver::Cli,
                prepare_kind: Some(PrepareKind::PickReset),
                grit_via_shell: false,
                git_via_shell: false,
            },
            &repo_pick,
        )?);

        let (grit_merge, git_merge) = merge_argv(&meta_pick);
        scenarios.push(run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: format!("merge-{size}{fs_suffix}"),
                group: "merge".into(),
                fixture: fixture.clone(),
                description: format!("merge topic branch with one {touch}-path commit"),
                grit_argv: grit_merge,
                git_argv: git_merge,
                driver: Driver::Cli,
                prepare_kind: Some(PrepareKind::MergeReset),
                grit_via_shell: false,
                git_via_shell: false,
            },
            &repo_pick,
        )?);

        setup_pick_series_fixture(git, spec)?;
        let meta_series = load_meta(&scratch_dir())?;
        let repo_series = scratch_dir();
        let grit_picks: Vec<Vec<String>> = meta_series
            .pick_series_commits
            .iter()
            .map(|c| vec!["pick".into(), c.clone()])
            .collect();
        let grit_series_cmd = chain_shell(grit, &grit_picks);
        scenarios.push(run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: format!("pick-series-{size}{fs_suffix}"),
                group: "pick".into(),
                fixture: fixture.clone(),
                description: "20 sequential grit pick vs git cherry-pick base..topic (grit pays process startup)"
                    .into(),
                grit_argv: sh_script(grit_series_cmd),
                git_argv: vec![
                    "cherry-pick".into(),
                    "--no-edit".into(),
                    format!("{}..{}", meta_series.base_commit, meta_series.pick_commit),
                ],
                driver: Driver::Cli,
                prepare_kind: Some(PrepareKind::PickSeriesReset),
                grit_via_shell: true,
                git_via_shell: false,
            },
            &repo_series,
        )?);
    }
    Ok(build_report(git, grit, timestamp, scenarios))
}
