//! Scenario definitions and hyperfine execution.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::bench_env::isolated_env_prefix;
use crate::binary::{report_grit_commit, tool_version};
use crate::fixture::{
    bench_work_dir, create_blame_deep_history_repo, create_blame_large_mid_churn_repo,
    create_blame_large_tail_churn_repo, create_deep_leaf_dogfood_repo,
    create_dense_leaf_dogfood_repo, create_repo, create_repo_with_history,
    create_untracked_monorepo_repo, dirty_repo, modify_files_for_diff, modify_rs_files_for_diff,
    modify_txt_files_for_diff, prepare_add_iteration, prepare_commit_iteration,
    prepare_dense_leaf_add_iteration, prepare_initial_monorepo_add, prepare_restore_iteration,
    scratch_dir, DeepLeafDogfoodSpec, DenseLeafDogfoodSpec,
};
use crate::hot_path_fixture::{
    load_meta, prepare_merge, prepare_pick, prepare_pick_series, prepare_revert, prepare_switch,
    prepare_switch_wide, setup_pick_merge_fixture, setup_pick_series_fixture, setup_switch_fixture,
    touch_paths_for_size, HotPathMeta, HotPathRepoSpec,
};
use crate::machine::{collect_machine_info, format_timestamp};
use crate::real_repo_fixture::run_git;
use crate::resource::bench_with_peak_rss;
use crate::schema::{
    BenchFailure, BenchReport, DriverKind, ScenarioResult, ToolVersions, SCHEMA_VERSION,
};
use crate::shell::{shell_command, shell_quote};
use crate::stats::{failed_timing_stats, median_ratio};

/// Driver selection for a scenario (CLI today; library hooks reserved).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Driver {
    Cli,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrepareKind {
    AddIteration,
    InitialMonorepoAdd,
    DenseLeafAddIteration,
    SwitchReset,
    SwitchWideReset,
    SwitchDetachTagReset,
    PickReset,
    MergeReset,
    PickSeriesReset,
    RevertReset,
    CommitIteration,
    RestoreReset,
    StashReset,
    MaintenanceReset,
    DiffDirtyReset,
    DiffSyntheticReset,
    WorktreeStatusClean,
    WorktreeStatusDirty,
    WorktreeAdd,
    WorktreeAddPatchFile,
    WorktreeAddHunks,
    WorktreeAddLines,
    WorktreeCommit,
    WorktreeCommitStaged,
    WorktreeDiff,
    WorktreeStagedDiff,
    WorktreeRestorePaths,
    WorktreeRmDir,
    WorktreeMvFile,
    WorktreeCleanUntracked,
    WorktreeInitEmpty,
    HistoryRewriteReset,
    CampaignStatusDirty,
    TagTreeSha256Digest,
    TagTreeSha256Signed,
    ConflictsMergeNoFfReset,
    ConflictsMergeConflictContinueReset,
    ConflictsPickRangeReset,
    ConflictsRevertSingleReset,
    ConflictsRevertMainlineReset,
}

/// Hyperfine tuning for all scenarios in a run.
#[derive(Debug, Clone)]
pub struct RunConfig {
    pub warmup: u32,
    pub min_runs: u32,
    /// When set, each hyperfine invocation uses `--runs N` (exact sample count).
    pub exact_runs: Option<u32>,
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
    let command = shell_command(program, args);
    let (mut stats, peak_rss) = bench_with_peak_rss(
        hyperfine,
        &cfg.prepare_bin,
        &command,
        cwd,
        cfg.warmup,
        cfg.min_runs,
        cfg.exact_runs,
        command_name,
        env_prefix.as_deref(),
        prepare,
    )?;
    stats.peak_rss_bytes = Some(peak_rss);
    Ok(stats)
}

fn prepare_command(
    cfg: &RunConfig,
    git: &Path,
    grit: &Path,
    kind: PrepareKind,
    repo: &Path,
) -> String {
    let sub = match kind {
        PrepareKind::AddIteration => "prepare-add",
        PrepareKind::InitialMonorepoAdd => "prepare-initial-monorepo-add",
        PrepareKind::DenseLeafAddIteration => "prepare-dense-leaf-add",
        PrepareKind::SwitchReset => "prepare-switch",
        PrepareKind::SwitchWideReset => "prepare-switch-wide",
        PrepareKind::SwitchDetachTagReset => "prepare-switch-detach-tag",
        PrepareKind::PickReset => "prepare-pick",
        PrepareKind::MergeReset => "prepare-merge",
        PrepareKind::PickSeriesReset => "prepare-pick-series",
        PrepareKind::RevertReset => "prepare-revert",
        PrepareKind::CommitIteration => "prepare-commit",
        PrepareKind::RestoreReset => "prepare-restore",
        PrepareKind::StashReset => "prepare-stash",
        PrepareKind::MaintenanceReset => "prepare-maintenance",
        PrepareKind::DiffDirtyReset => "prepare-diff-dirty",
        PrepareKind::DiffSyntheticReset => "prepare-diff-synthetic",
        PrepareKind::WorktreeStatusClean => "prepare-worktree-status-clean",
        PrepareKind::WorktreeStatusDirty => "prepare-worktree-status-dirty",
        PrepareKind::WorktreeAdd => "prepare-worktree-add",
        PrepareKind::WorktreeAddPatchFile => "prepare-worktree-add-patch-file",
        PrepareKind::WorktreeAddHunks => "prepare-worktree-add-hunks",
        PrepareKind::WorktreeAddLines => "prepare-worktree-add-lines",
        PrepareKind::WorktreeCommit => "prepare-worktree-commit",
        PrepareKind::WorktreeCommitStaged => "prepare-worktree-commit-staged",
        PrepareKind::WorktreeDiff => "prepare-worktree-diff",
        PrepareKind::WorktreeStagedDiff => "prepare-worktree-staged-diff",
        PrepareKind::WorktreeRestorePaths => "prepare-worktree-restore-paths",
        PrepareKind::WorktreeRmDir => "prepare-worktree-rm-dir",
        PrepareKind::WorktreeMvFile => "prepare-worktree-mv-file",
        PrepareKind::WorktreeCleanUntracked => "prepare-worktree-clean-untracked",
        PrepareKind::WorktreeInitEmpty => "prepare-worktree-init-empty",
        PrepareKind::HistoryRewriteReset => "prepare-history-rewrite",
        PrepareKind::CampaignStatusDirty => "prepare-campaign-status-dirty",
        PrepareKind::TagTreeSha256Digest => "prepare-tag-tree-sha256-digest",
        PrepareKind::TagTreeSha256Signed => "prepare-tag-tree-sha256-signed",
        PrepareKind::ConflictsMergeNoFfReset => "prepare-conflicts-merge-no-ff",
        PrepareKind::ConflictsMergeConflictContinueReset => {
            "prepare-conflicts-merge-conflict-continue"
        }
        PrepareKind::ConflictsPickRangeReset => "prepare-conflicts-pick-range",
        PrepareKind::ConflictsRevertSingleReset => "prepare-conflicts-revert-single",
        PrepareKind::ConflictsRevertMainlineReset => "prepare-conflicts-revert-mainline",
    };
    let mut args = vec![
        sub.to_string(),
        "--git".to_string(),
        git.display().to_string(),
    ];
    if matches!(
        kind,
        PrepareKind::TagTreeSha256Digest | PrepareKind::TagTreeSha256Signed
    ) {
        args.push("--grit".to_string());
        args.push(grit.display().to_string());
    }
    if kind == PrepareKind::MaintenanceReset {
        args.push("--worktree".to_string());
        args.push(repo.display().to_string());
    }
    let inner = shell_command(&cfg.prepare_bin, &args);
    if kind == PrepareKind::MaintenanceReset {
        if let Ok(bare) = std::env::var("GRIT_BENCH_MAINTENANCE_BARE") {
            return format!(
                "GRIT_BENCH_MAINTENANCE_BARE='{}' {inner}",
                bare.replace('\'', "'\\''")
            );
        }
    }
    inner
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
        .map(|kind| prepare_command(cfg, git, grit, kind, repo));

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

    // Measure grit before git so a preceding `git` hyperfine run cannot leave the
    // worktree/index in a state grit's next prepare+command pair cannot handle
    // (campaign git.git commit rows hit this when git commits first).
    let grit_stats = bench_tool(
        hyperfine,
        &grit_prog,
        &scenario.grit_argv,
        repo,
        cfg,
        prepare.as_deref(),
        "grit",
    )?;
    let git_stats = bench_tool(
        hyperfine,
        &git_prog,
        &scenario.git_argv,
        repo,
        cfg,
        prepare.as_deref(),
        "git",
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
        grit_failure: None,
    })
}

/// Run git hyperfine only; grit stats come from `grit_failure` (avoids repeated OOM during hyperfine warmups).
pub fn run_scenario_git_only_with_grit_failure(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    scenario: &Scenario,
    repo: &Path,
    grit_failure: BenchFailure,
) -> Result<ScenarioResult> {
    let prepare = scenario
        .prepare_kind
        .map(|kind| prepare_command(cfg, git, grit, kind, repo));

    let git_prog = if scenario.git_via_shell {
        PathBuf::from("/bin/sh")
    } else {
        git.to_path_buf()
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
    let _ = grit;

    Ok(ScenarioResult {
        id: scenario.id.clone(),
        group: scenario.group.clone(),
        fixture: scenario.fixture.clone(),
        description: scenario.description.clone(),
        driver: scenario.driver.into(),
        git: git_stats,
        grit: failed_timing_stats(),
        ratio: 0.0,
        grit_failure: Some(grit_failure),
    })
}

/// Like [`run_scenario`], but records a grit-side hyperfine failure instead of aborting the campaign.
pub fn run_scenario_allow_grit_failure(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    scenario: &Scenario,
    repo: &Path,
) -> Result<ScenarioResult> {
    let prepare = scenario
        .prepare_kind
        .map(|kind| prepare_command(cfg, git, grit, kind, repo));

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

    let grit_outcome = bench_tool(
        hyperfine,
        &grit_prog,
        &scenario.grit_argv,
        repo,
        cfg,
        prepare.as_deref(),
        "grit",
    );

    let git_stats = bench_tool(
        hyperfine,
        &git_prog,
        &scenario.git_argv,
        repo,
        cfg,
        prepare.as_deref(),
        "git",
    )?;

    let (grit_stats, ratio, grit_failure) = match grit_outcome {
        Ok(stats) => {
            let ratio = median_ratio(git_stats.median_ms, stats.median_ms);
            (stats, ratio, None)
        }
        Err(err) => (
            failed_timing_stats(),
            0.0,
            Some(BenchFailure {
                kind: "command_failed".into(),
                message: err.to_string(),
            }),
        ),
    };

    Ok(ScenarioResult {
        id: scenario.id.clone(),
        group: scenario.group.clone(),
        fixture: scenario.fixture.clone(),
        description: scenario.description.clone(),
        driver: scenario.driver.into(),
        git: git_stats,
        grit: grit_stats,
        ratio,
        grit_failure,
    })
}

/// Like [`run_scenario`] but uses an explicit hyperfine prepare command (network hooks).
pub fn run_scenario_prepared(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    scenario: &Scenario,
    repo: &Path,
    prepare: &str,
) -> Result<ScenarioResult> {
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
        Some(prepare),
        "git",
    )?;
    let grit_stats = bench_tool(
        hyperfine,
        &grit_prog,
        &scenario.grit_argv,
        repo,
        cfg,
        Some(prepare),
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
        grit_failure: None,
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

/// Restore-all benchmark for each file count (`git restore .` vs `grit restore .`).
pub fn run_restore_suite(
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
        prepare_restore_iteration(&repo)?;
        scenarios.push(run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: format!("restore-{size}"),
                group: "restore".into(),
                fixture: format!("synthetic-{size}"),
                description: "restore worktree from index after modifying ~20% of files".into(),
                grit_argv: vec!["restore".into(), ".".into()],
                git_argv: vec!["restore".into(), ".".into()],
                driver: Driver::Cli,
                prepare_kind: Some(PrepareKind::RestoreReset),
                grit_via_shell: false,
                git_via_shell: false,
            },
            &repo,
        )?);
    }
    Ok(build_report(git, grit, timestamp, scenarios))
}

/// Deep-leaf CRLF worktree mirroring dogfood issue #1017 (clean status + dirty diff).
pub fn run_deep_leaf_worktree_suite(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    spec: DeepLeafDogfoodSpec,
    timestamp: time::OffsetDateTime,
) -> Result<BenchReport> {
    const MODIFIED_RS: usize = 100;
    let repo = create_deep_leaf_dogfood_repo(git, spec)?;
    let fixture = format!(
        "deep-leaf-{}",
        spec.leaf_dirs * spec.files_per_leaf + spec.rs_file_count + 1
    );
    let mut scenarios = vec![run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: "status-clean-deep-leaf".into(),
            group: "status".into(),
            fixture: fixture.clone(),
            description: "status on clean deep-leaf CRLF worktree (git -s, grit default)".into(),
            grit_argv: vec!["status".into()],
            git_argv: vec!["status".into(), "-s".into()],
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &repo,
    )?];

    modify_rs_files_for_diff(&repo, MODIFIED_RS.min(spec.rs_file_count))?;
    scenarios.push(run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: "status-dirty-deep-leaf".into(),
            group: "status".into(),
            fixture: fixture.clone(),
            description: format!(
                "status after modifying {} .rs files on deep-leaf worktree",
                MODIFIED_RS.min(spec.rs_file_count)
            ),
            grit_argv: vec!["status".into()],
            git_argv: vec!["status".into(), "-s".into()],
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &repo,
    )?);
    scenarios.push(run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: "diff-deep-leaf".into(),
            group: "diff".into(),
            fixture,
            description: format!(
                "worktree diff after modifying {} .rs files (git diff, grit diff)",
                MODIFIED_RS.min(spec.rs_file_count)
            ),
            grit_argv: vec!["diff".into()],
            git_argv: vec!["diff".into()],
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &repo,
    )?);
    Ok(build_report(git, grit, timestamp, scenarios))
}

/// Dense leaf dirs with many small files per directory (dogfood issue #1041).
pub fn run_dense_leaf_worktree_suite(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    spec: DenseLeafDogfoodSpec,
    timestamp: time::OffsetDateTime,
) -> Result<BenchReport> {
    const MODIFIED_TXT: usize = 100;
    let repo = create_dense_leaf_dogfood_repo(git, spec)?;
    let tracked =
        spec.leaf_dirs * spec.files_per_leaf + spec.deep_chains * spec.deep_files_per_chain;
    let fixture = format!("dense-leaf-{tracked}");
    let mut scenarios = Vec::new();
    scenarios.push(run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: "status-clean-dense-leaf".into(),
            group: "status".into(),
            fixture: fixture.clone(),
            description: "status on clean dense-leaf worktree (#1041 layout)".into(),
            grit_argv: vec!["status".into()],
            git_argv: vec!["status".into(), "-s".into()],
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &repo,
    )?);
    scenarios.push(run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: "add-clean-dense-leaf".into(),
            group: "add".into(),
            fixture: fixture.clone(),
            description: "no-op add on clean dense-leaf worktree (#1518 / #1041 layout)".into(),
            grit_argv: vec!["add".into()],
            git_argv: vec!["add".into(), "-A".into()],
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &repo,
    )?);
    modify_txt_files_for_diff(&repo, MODIFIED_TXT)?;
    scenarios.push(run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: "status-dirty-dense-leaf".into(),
            group: "status".into(),
            fixture: fixture.clone(),
            description: format!(
                "status after modifying {MODIFIED_TXT} .txt files on dense-leaf worktree"
            ),
            grit_argv: vec!["status".into()],
            git_argv: vec!["status".into(), "-s".into()],
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &repo,
    )?);
    scenarios.push(run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: "diff-dense-leaf".into(),
            group: "diff".into(),
            fixture: fixture.clone(),
            description: format!(
                "worktree diff after modifying {MODIFIED_TXT} .txt files on dense-leaf worktree"
            ),
            grit_argv: vec!["diff".into()],
            git_argv: vec!["diff".into()],
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &repo,
    )?);
    scenarios.push(run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: "add-dense-leaf".into(),
            group: "add".into(),
            fixture,
            description: format!("stage {MODIFIED_TXT} modified .txt files on dense-leaf worktree"),
            grit_argv: vec!["add".into()],
            git_argv: vec!["add".into(), "-A".into()],
            driver: Driver::Cli,
            prepare_kind: Some(PrepareKind::DenseLeafAddIteration),
            grit_via_shell: false,
            git_via_shell: false,
        },
        &repo,
    )?);
    Ok(build_report(git, grit, timestamp, scenarios))
}

/// Number of tracked `.txt` files modified for the synthetic `diff-{size}` dashboard scenario.
pub const SYNTHETIC_DIFF_MODIFIED: usize = 48;

/// Worktree diff with ~48 modified files (`git diff` vs `grit diff`).
pub fn run_diff_suite(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    sizes: &[usize],
    timestamp: time::OffsetDateTime,
) -> Result<BenchReport> {
    let modified = SYNTHETIC_DIFF_MODIFIED;
    let mut scenarios = Vec::new();
    for &size in sizes {
        let repo = create_repo(git, size)?;
        modify_files_for_diff(&repo, modified)?;
        scenarios.push(run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: format!("diff-{size}"),
                group: "diff".into(),
                fixture: format!("synthetic-{size}"),
                description: format!(
                    "worktree diff with {modified} modified tracked files (git diff, grit diff)"
                ),
                grit_argv: vec!["diff".into()],
                git_argv: vec!["diff".into()],
                driver: Driver::Cli,
                prepare_kind: Some(PrepareKind::DiffSyntheticReset),
                grit_via_shell: false,
                git_via_shell: false,
            },
            &repo,
        )?);
    }
    Ok(build_report(git, grit, timestamp, scenarios))
}

/// Commits listed by the log benchmark fixture (`create_repo_with_history`).
pub const LOG_BENCH_COMMIT_COUNT: usize = 100;

/// `grit log` pages in the log benchmark (10 commits per page).
pub const LOG_BENCH_GRIT_PAGE_COUNT: usize = 10;

/// Grit subprocesses in one full log benchmark run (one `grit log` per page).
#[must_use]
pub fn log_bench_expected_grit_invocations() -> usize {
    LOG_BENCH_GRIT_PAGE_COUNT
}

/// Regression guard: shell must page without an extra timed probe `grit log`.
pub fn validate_log_bench_grit_shell(script: &str, grit: &Path) -> Result<(), String> {
    if script.contains("/dev/null") {
        return Err("log benchmark must not run a timed probe grit log".into());
    }
    let grit_q = shell_quote(&grit.to_string_lossy());
    if !script.contains(&format!("seq 1 {LOG_BENCH_GRIT_PAGE_COUNT}")) {
        return Err("log benchmark shell missing page loop bound".into());
    }
    if !script.contains(&format!("page=$({grit_q} log)")) {
        return Err("log benchmark shell missing first-page grit log".into());
    }
    if !script.contains(&format!("page=$({grit_q} log --before=\"$before\")")) {
        return Err("log benchmark shell missing paged grit log".into());
    }
    Ok(())
}

/// Shell script hyperfine runs for the grit side of the log benchmark.
pub fn log_bench_grit_shell(grit: &Path) -> String {
    grit_log_pages_shell(grit, LOG_BENCH_GRIT_PAGE_COUNT)
}

/// Grit argv for the dashboard `log-{size}` scenario (one process, capped history).
#[must_use]
pub fn log_bench_grit_argv() -> Vec<String> {
    vec![
        "log".into(),
        "--limit".into(),
        LOG_BENCH_COMMIT_COUNT.to_string(),
    ]
}

/// Last 100 commits, one line each (`git log -100 --oneline` vs `grit log --limit 100`).
pub fn run_log_suite(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    sizes: &[usize],
    timestamp: time::OffsetDateTime,
) -> Result<BenchReport> {
    let mut scenarios = Vec::new();
    for &size in sizes {
        let repo = create_repo_with_history(git, size, LOG_BENCH_COMMIT_COUNT)?;
        scenarios.push(run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: format!("log-{size}"),
                group: "log".into(),
                fixture: format!("synthetic-{size}"),
                description: format!(
                    "last {LOG_BENCH_COMMIT_COUNT} commits one line each (git log -100 --oneline, grit log --limit {LOG_BENCH_COMMIT_COUNT})"
                ),
                grit_argv: log_bench_grit_argv(),
                git_argv: vec!["log".into(), "-100".into(), "--oneline".into()],
                driver: Driver::Cli,
                prepare_kind: None,
                grit_via_shell: false,
                git_via_shell: false,
            },
            &repo,
        )?);
    }
    Ok(build_report(git, grit, timestamp, scenarios))
}

pub(crate) fn grit_log_pages_shell(grit: &Path, pages: usize) -> String {
    let grit_q = shell_quote(&grit.to_string_lossy());
    format!(
        r#"set -eu
before=""
for _ in $(seq 1 {pages}); do
  if [ -z "$before" ]; then
    page=$({grit_q} log) || exit $?
  else
    page=$({grit_q} log --before="$before") || exit $?
  fi
  printf '%s\n' "$page" | awk '!/^→/ && !/^$/ {{print}}'
  before=$(printf '%s\n' "$page" | sed -n 's|^→ more: grit log --before=||p')
  if [ -z "$before" ]; then break; fi
done"#
    )
}

#[cfg(test)]
mod log_bench_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn log_bench_shell_has_no_probe_and_ten_grit_invocations() {
        let grit = Path::new("/opt/grit/grit");
        let script = log_bench_grit_shell(grit);
        validate_log_bench_grit_shell(&script, grit).expect("shell shape");
        assert_eq!(
            log_bench_expected_grit_invocations(),
            LOG_BENCH_GRIT_PAGE_COUNT
        );
    }

    #[test]
    fn log_dashboard_grit_argv_is_single_limit_invocation() {
        assert_eq!(
            log_bench_grit_argv(),
            vec![
                "log".to_string(),
                "--limit".to_string(),
                LOG_BENCH_COMMIT_COUNT.to_string(),
            ]
        );
    }

    #[test]
    fn campaign_production_log_uses_hundred_grit_pages() {
        let grit = Path::new("/opt/grit/grit");
        let log_limit = 1000usize;
        let pages = log_limit.div_ceil(LOG_BENCH_GRIT_PAGE_COUNT);
        assert_eq!(pages, 100);
        let script = grit_log_pages_shell(grit, pages);
        assert!(script.contains("seq 1 100"));
        let grit_q = shell_quote(&grit.to_string_lossy());
        assert!(script.contains(&format!("page=$({grit_q} log)")));
        assert!(
            script.contains("if [ -z \"$before\" ]; then break; fi"),
            "must not use `[ -z ] && break` under set -eu"
        );
        assert!(script.contains("sed -n 's|^→ more: grit log --before=||p'"));
    }
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

/// Initial stage-all on a fresh monorepo-shaped tree (#1674 dogfood layout).
pub fn run_initial_monorepo_add_suite(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    file_count: usize,
    timestamp: time::OffsetDateTime,
) -> Result<BenchReport> {
    let repo = create_untracked_monorepo_repo(git, file_count)?;
    prepare_initial_monorepo_add(&repo)?;
    let scenario = run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: format!("add-initial-monorepo-{file_count}"),
            group: "add".into(),
            fixture: format!("monorepo-untracked-{file_count}"),
            description: format!(
                "initial stage-all on {file_count} untracked files (20×20×10 monorepo layout, #1674)"
            ),
            grit_argv: vec!["add".into()],
            git_argv: vec!["add".into(), "-A".into()],
            driver: Driver::Cli,
            prepare_kind: Some(PrepareKind::InitialMonorepoAdd),
            grit_via_shell: false,
            git_via_shell: false,
        },
        &repo,
    )?;
    Ok(build_report(git, grit, timestamp, vec![scenario]))
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

const BLAME_LARGE_LINES: usize = 5_000;
const BLAME_LARGE_COMMITS: usize = 150;
const BLAME_LARGE_PATH: &str = "large.txt";
const BLAME_MID_CHURN_LINES: usize = 3_500;
const BLAME_MID_CHURN_COMMITS: usize = 30;

/// `grit blame` vs `git blame --porcelain` on deep linear and large tail-churn fixtures.
pub fn run_blame_suite(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    commit_count: usize,
    timestamp: time::OffsetDateTime,
) -> Result<BenchReport> {
    let mut scenarios = Vec::new();
    let repo = create_blame_deep_history_repo(git, commit_count)?;
    scenarios.push(run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: format!("blame-deep-{commit_count}"),
            group: "blame".into(),
            fixture: format!("blame-linear-{commit_count}"),
            description: "blame one file with deep history (git --porcelain vs grit blame)".into(),
            grit_argv: vec!["blame".into(), "blame.txt".into()],
            git_argv: vec!["blame".into(), "--porcelain".into(), "blame.txt".into()],
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &repo,
    )?);

    let large = create_blame_large_tail_churn_repo(git, BLAME_LARGE_LINES, BLAME_LARGE_COMMITS)?;
    scenarios.push(run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: "blame-large-tail-full".into(),
            group: "blame".into(),
            fixture: format!("blame-large-{BLAME_LARGE_LINES}x{BLAME_LARGE_COMMITS}"),
            description: format!(
                "full-file blame on {BLAME_LARGE_LINES}-line file with {BLAME_LARGE_COMMITS} tail-only commits"
            ),
            grit_argv: vec!["blame".into(), BLAME_LARGE_PATH.into()],
            git_argv: vec![
                "blame".into(),
                "--porcelain".into(),
                BLAME_LARGE_PATH.into(),
            ],
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &large,
    )?);
    let mid =
        create_blame_large_mid_churn_repo(git, BLAME_MID_CHURN_LINES, BLAME_MID_CHURN_COMMITS)?;
    scenarios.push(run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: "blame-large-mid-churn-full".into(),
            group: "blame".into(),
            fixture: format!("blame-mid-{BLAME_MID_CHURN_LINES}x{BLAME_MID_CHURN_COMMITS}"),
            description: format!(
                "full-file blame on {BLAME_MID_CHURN_LINES}-line file with {BLAME_MID_CHURN_COMMITS} mid-file hunk commits (#1505)"
            ),
            grit_argv: vec!["blame".into(), BLAME_LARGE_PATH.into()],
            git_argv: vec![
                "blame".into(),
                "--porcelain".into(),
                BLAME_LARGE_PATH.into(),
            ],
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &mid,
    )?);

    scenarios.push(run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: "blame-large-tail-range".into(),
            group: "blame".into(),
            fixture: format!("blame-large-{BLAME_LARGE_LINES}x{BLAME_LARGE_COMMITS}"),
            description: format!(
                "blame -L 100,120 on {BLAME_LARGE_LINES}-line file with tail-only commits"
            ),
            grit_argv: vec![
                "blame".into(),
                "-L".into(),
                "100,120".into(),
                BLAME_LARGE_PATH.into(),
            ],
            git_argv: vec![
                "blame".into(),
                "-L100,120".into(),
                "--porcelain".into(),
                BLAME_LARGE_PATH.into(),
            ],
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &large,
    )?);

    Ok(build_report(git, grit, timestamp, scenarios))
}

/// `grit stash` + `grit stash pop` vs `git stash push` + `git stash pop`.
pub fn run_stash_suite(
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
            .with_context(|| format!("initial stash-bench setup for {size} files"))?;
        let grit_cmd = chain_shell(
            grit,
            &[vec!["stash".into()], vec!["stash".into(), "pop".into()]],
        );
        let git_cmd = chain_shell(
            git,
            &[
                vec!["stash".into(), "push".into(), "-q".into()],
                vec!["stash".into(), "pop".into(), "-q".into()],
            ],
        );
        scenarios.push(run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: format!("stash-push-pop-{size}"),
                group: "stash".into(),
                fixture: format!("synthetic-{size}"),
                description:
                    "stash push then pop after modifying ~20% of files (git reset between runs)"
                        .into(),
                grit_argv: sh_script(grit_cmd),
                git_argv: sh_script(git_cmd),
                driver: Driver::Cli,
                prepare_kind: Some(PrepareKind::StashReset),
                grit_via_shell: true,
                git_via_shell: true,
            },
            &repo,
        )?);
    }
    Ok(build_report(git, grit, timestamp, scenarios))
}

pub(crate) fn build_report(
    git: &Path,
    grit: &Path,
    timestamp: time::OffsetDateTime,
    scenarios: Vec<ScenarioResult>,
) -> BenchReport {
    build_report_with_fixtures(git, grit, timestamp, scenarios, Vec::new())
}

/// Bench report for merge-check with hyperfine version recorded in `tools`.
pub(crate) fn build_merge_check_report(
    git: &Path,
    bench_exe: &Path,
    hyperfine: &Path,
    timestamp: time::OffsetDateTime,
    scenarios: Vec<ScenarioResult>,
) -> BenchReport {
    use crate::binary::tool_version;
    use crate::schema::ToolVersions;

    let mut report = build_report_with_fixtures(git, bench_exe, timestamp, scenarios, Vec::new());
    report.tools = ToolVersions {
        git: tool_version(git),
        grit: tool_version(bench_exe),
        grit_commit: report_grit_commit(),
        hyperfine: Some(tool_version(hyperfine)),
    };
    report
}

pub(crate) fn build_report_with_fixtures(
    git: &Path,
    grit: &Path,
    timestamp: time::OffsetDateTime,
    scenarios: Vec<ScenarioResult>,
    fixtures: Vec<crate::schema::FixtureRecord>,
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
        tools: ToolVersions::without_hyperfine(
            tool_version(git),
            tool_version(grit),
            report_grit_commit(),
        ),
        fixtures,
        scenarios,
    }
}

/// Shared prepare hook for `add` scenarios (invoked by hyperfine `--prepare`).
pub fn run_prepare_add(git: &Path) -> Result<()> {
    prepare_add_iteration(&bench_work_dir(), git)
}

pub fn run_prepare_initial_monorepo_add(_git: &Path) -> Result<()> {
    prepare_initial_monorepo_add(&bench_work_dir())
}

/// Hyperfine `--prepare` hook for dense-leaf add (reset index, re-dirty 100 `.txt` files).
pub fn run_prepare_dense_leaf_add(git: &Path) -> Result<()> {
    const MODIFIED_TXT: usize = 100;
    prepare_dense_leaf_add_iteration(&scratch_dir(), git, MODIFIED_TXT)
}

pub fn run_prepare_commit(git: &Path) -> Result<()> {
    prepare_commit_iteration(&bench_work_dir(), git)
}

pub fn run_prepare_restore(_git: &Path) -> Result<()> {
    prepare_restore_iteration(&bench_work_dir())
}

pub fn run_prepare_stash(git: &Path) -> Result<()> {
    let dir = bench_work_dir();
    if let Ok(meta) = crate::campaign_prep::load_campaign_hot_path_meta(&dir) {
        crate::campaign_prep::restore_campaign_worktree_to_main(git, &dir, &meta)?;
    }
    run_git(git, &dir, &["reset", "--hard", "HEAD"])?;
    run_git(git, &dir, &["clean", "-fd", "-q"])?;
    let mut cmd = Command::new(git);
    cmd.args(["stash", "clear"]).current_dir(&dir);
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            crate::bench_env::empty_global_config_path(),
        )
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    let _ = cmd.output();
    prepare_commit_iteration(&dir, git)
}

pub fn run_prepare_diff_dirty(git: &Path) -> Result<()> {
    crate::fixture::prepare_diff_dirty_iteration(&bench_work_dir(), git)
}

/// Hyperfine prepare hook for [`run_diff_suite`] (restores the synthetic diff fixture).
pub fn run_prepare_diff_synthetic(git: &Path) -> Result<()> {
    crate::fixture::prepare_synthetic_diff_iteration(
        &bench_work_dir(),
        git,
        SYNTHETIC_DIFF_MODIFIED,
    )
}

pub fn run_prepare_switch(git: &Path) -> Result<()> {
    prepare_switch(git, &bench_work_dir())
}

pub fn run_prepare_switch_wide(git: &Path) -> Result<()> {
    prepare_switch_wide(git, &bench_work_dir())
}

pub fn run_prepare_switch_detach_tag(git: &Path) -> Result<()> {
    crate::campaign_prep::prepare_switch_detach_tag(git, &bench_work_dir())
}

pub fn run_prepare_pick(git: &Path) -> Result<()> {
    prepare_pick(git, &bench_work_dir())
}

pub fn run_prepare_merge(git: &Path) -> Result<()> {
    prepare_merge(git, &bench_work_dir())
}

pub fn run_prepare_pick_series(git: &Path) -> Result<()> {
    prepare_pick_series(git, &bench_work_dir())
}

pub fn run_prepare_revert(git: &Path) -> Result<()> {
    prepare_revert(git, &bench_work_dir())
}

/// Join multiple CLI invocations for hyperfine shell benchmarks.
pub fn chain_shell_public(program: &Path, invocations: &[Vec<String>]) -> String {
    chain_shell(program, invocations)
}

pub(crate) fn chain_shell(program: &Path, invocations: &[Vec<String>]) -> String {
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

pub(crate) fn sh_script_internal(script: String) -> Vec<String> {
    sh_script(script)
}

pub fn run_prepare_conflicts_merge_no_ff(git: &Path) -> Result<()> {
    crate::conflicts_fixture::prepare_conflicts_merge_no_ff(git, &bench_work_dir())
}

pub fn run_prepare_conflicts_merge_conflict_continue(git: &Path) -> Result<()> {
    crate::conflicts_fixture::prepare_conflicts_merge_conflict_continue(git, &bench_work_dir())
}

pub fn run_prepare_conflicts_pick_range(git: &Path) -> Result<()> {
    crate::conflicts_fixture::prepare_conflicts_pick_range(git, &bench_work_dir())
}

pub fn run_prepare_conflicts_revert_single(git: &Path) -> Result<()> {
    crate::conflicts_fixture::prepare_conflicts_revert_single(git, &bench_work_dir())
}

pub fn run_prepare_conflicts_revert_mainline(git: &Path) -> Result<()> {
    crate::conflicts_fixture::prepare_conflicts_revert_mainline(git, &bench_work_dir())
}

fn sh_script(script: String) -> Vec<String> {
    vec!["-c".into(), script]
}

pub(crate) fn switch_argv(
    grit: &Path,
    git: &Path,
    meta: &HotPathMeta,
) -> (Vec<String>, Vec<String>) {
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

pub(crate) fn pick_argv(meta: &HotPathMeta) -> (Vec<String>, Vec<String>) {
    (
        vec!["pick".into(), meta.pick_commit.clone()],
        vec![
            "cherry-pick".into(),
            "--no-edit".into(),
            meta.pick_commit.clone(),
        ],
    )
}

pub(crate) fn revert_argv(meta: &HotPathMeta) -> (Vec<String>, Vec<String>) {
    (
        vec!["revert".into(), meta.pick_commit.clone()],
        vec![
            "revert".into(),
            "--no-edit".into(),
            meta.pick_commit.clone(),
        ],
    )
}

pub(crate) fn merge_argv(meta: &HotPathMeta) -> (Vec<String>, Vec<String>) {
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

pub(crate) fn merge_check_git_argv(meta: &HotPathMeta) -> Vec<String> {
    vec![
        "merge-tree".into(),
        "--write-tree".into(),
        format!("--merge-base={}", meta.base_commit),
        meta.main_tip.clone(),
        meta.pick_commit.clone(),
    ]
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

        let (grit_revert, git_revert) = revert_argv(&meta_pick);
        scenarios.push(run_scenario_allow_grit_failure(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: format!("revert-{size}{fs_suffix}"),
                group: "pick".into(),
                fixture: fixture.clone(),
                description: format!(
                    "revert one commit touching {touch} paths (topic tip vs git revert --no-edit)"
                ),
                grit_argv: grit_revert,
                git_argv: git_revert,
                driver: Driver::Cli,
                prepare_kind: Some(PrepareKind::RevertReset),
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

        if Some(size) == sizes.first().copied() {
            scenarios.push(run_scenario(
                hyperfine,
                git,
                grit,
                cfg,
                &Scenario {
                    id: "pick-range-20".into(),
                    group: "pick".into(),
                    fixture: fixture.clone(),
                    description:
                        "grit pick base..topic vs git cherry-pick on 20-commit topic branch".into(),
                    grit_argv: vec![
                        "pick".into(),
                        format!("{}..{}", meta_series.base_commit, meta_series.pick_commit),
                    ],
                    git_argv: vec![
                        "cherry-pick".into(),
                        "--no-edit".into(),
                        format!("{}..{}", meta_series.base_commit, meta_series.pick_commit),
                    ],
                    driver: Driver::Cli,
                    prepare_kind: Some(PrepareKind::PickSeriesReset),
                    grit_via_shell: false,
                    git_via_shell: false,
                },
                &repo_series,
            )?);
        }
    }
    Ok(build_report(git, grit, timestamp, scenarios))
}

/// In-memory merge prediction vs `git merge-tree --write-tree` (medium synthetic fixture).
pub fn run_merge_check_suite(
    hyperfine: &Path,
    git: &Path,
    bench_exe: &Path,
    cfg: &RunConfig,
    sizes: &[usize],
    timestamp: time::OffsetDateTime,
) -> Result<BenchReport> {
    let mut scenarios = Vec::new();
    for &size in sizes {
        let spec = hot_path_spec(size, false);
        let fixture = format!("synthetic-{size}");
        let touch = touch_paths_for_size(size);
        setup_pick_merge_fixture(git, spec, touch)?;
        let meta = load_meta(&scratch_dir())?;
        let git_argv = merge_check_git_argv(&meta);
        let repo = scratch_dir();
        scenarios.push(run_scenario(
            hyperfine,
            git,
            bench_exe,
            cfg,
            &Scenario {
                id: if size == 10_000 {
                    "merge-check".into()
                } else {
                    format!("merge-check-{size}")
                },
                group: "merge".into(),
                fixture: fixture.clone(),
                description: format!(
                    "predict_merge (grit-bench drive) vs git merge-tree --write-tree ({touch}-path topic commit)"
                ),
                grit_argv: vec![
                    "drive".into(),
                    "merge-predict".into(),
                    ".".into(),
                ],
                git_argv,
                driver: Driver::Lib,
                prepare_kind: Some(PrepareKind::MergeReset),
                grit_via_shell: false,
                git_via_shell: false,
            },
            &repo,
        )?);
    }
    Ok(build_merge_check_report(
        git, bench_exe, hyperfine, timestamp, scenarios,
    ))
}

/// Merge-only hot-path scenario (isolated fixture; no prior pick/revert in the same run).
pub fn run_merge_only_suite(
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
        let touch = touch_paths_for_size(size);
        setup_pick_merge_fixture(git, spec, touch)?;
        let meta = load_meta(&scratch_dir())?;
        let (grit_merge, git_merge) = merge_argv(&meta);
        let repo = scratch_dir();
        scenarios.push(run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: format!("merge-{size}"),
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
            &repo,
        )?);
    }
    Ok(build_report(git, grit, timestamp, scenarios))
}
