//! Merge, pick, and revert conflict-operation benchmarks vs system git.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::conflicts_fixture::{
    conflict_overlap_for_size, conflicts_meta_path, conflicts_spec, conflicts_touch_paths,
    setup_conflicts_clean_fixture_in, setup_conflicts_merge_fixture_in,
    setup_conflicts_pick_range_fixture_in, setup_conflicts_revert_mainline_fixture_in,
    ConflictsMeta,
};
use crate::fixture::remove_dir_robust;
use crate::scenarios::{
    build_report, run_scenario, sh_script_internal, Driver, PrepareKind, RunConfig, Scenario,
};
use crate::schema::BenchReport;
use crate::shell::shell_command;

fn scenario_id(name: &str, size: usize) -> String {
    if size == 10_000 {
        name.to_string()
    } else {
        format!("{name}-{size}")
    }
}

fn merge_no_ff_argv(meta: &ConflictsMeta) -> (Vec<String>, Vec<String>) {
    (
        vec![
            "merge".into(),
            "--no-ff".into(),
            meta.base.topic_branch.clone(),
        ],
        vec![
            "merge".into(),
            "--no-ff".into(),
            "-q".into(),
            "--no-edit".into(),
            meta.base.topic_branch.clone(),
        ],
    )
}

fn merge_conflict_continue_argv(
    grit: &Path,
    git: &Path,
    meta: &ConflictsMeta,
) -> (Vec<String>, Vec<String>) {
    let topic = meta.base.topic_branch.clone();
    let grit_script = format!(
        "{}; {} && {} && {}",
        shell_command(grit, &["merge", &topic]),
        shell_command(grit, &["restore", "--theirs", "."]),
        shell_command(grit, &["add", "."]),
        shell_command(grit, &["merge", "--continue"])
    );
    let git_script = format!(
        "{}; {} && {} && {}",
        shell_command(git, &["merge", &topic]),
        shell_command(git, &["restore", "--theirs", "."]),
        shell_command(git, &["add", "."]),
        shell_command(git, &["-c", "core.editor=true", "merge", "--continue"],)
    );
    (
        sh_script_internal(grit_script),
        sh_script_internal(git_script),
    )
}

fn pick_range_argv(meta: &ConflictsMeta) -> (Vec<String>, Vec<String>) {
    let mut grit_pick = vec!["pick".into()];
    grit_pick.extend(meta.base.pick_series_commits.iter().cloned());
    (
        grit_pick,
        vec![
            "cherry-pick".into(),
            "--no-edit".into(),
            format!("{}..{}", meta.base.base_commit, meta.base.pick_commit),
        ],
    )
}

fn revert_single_argv(_meta: &ConflictsMeta) -> (Vec<String>, Vec<String>) {
    (
        vec!["revert".into(), "HEAD".into()],
        vec!["revert".into(), "--no-edit".into(), "HEAD".into()],
    )
}

fn revert_mainline_argv(meta: &ConflictsMeta) -> (Vec<String>, Vec<String>) {
    (
        vec![
            "revert".into(),
            "--mainline".into(),
            "1".into(),
            meta.merge_commit.clone(),
        ],
        vec![
            "revert".into(),
            "--no-edit".into(),
            "-m".into(),
            "1".into(),
            meta.merge_commit.clone(),
        ],
    )
}

fn conflicts_base_dir() -> PathBuf {
    PathBuf::from("/tmp/grit-bench-scratch")
}

fn scenario_repo(name: &str, size: usize) -> PathBuf {
    conflicts_base_dir().join(format!("{name}-{size}"))
}

fn with_scenario_repo<F>(name: &str, size: usize, setup: F) -> Result<(PathBuf, ConflictsMeta)>
where
    F: FnOnce(&Path) -> Result<(PathBuf, ConflictsMeta)>,
{
    let repo = scenario_repo(name, size);
    if repo.exists() {
        remove_dir_robust(&repo);
    }
    std::env::remove_var("GRIT_BENCH_SCRATCH");
    std::env::remove_var("GRIT_BENCH_CAMPAIGN_CWD");
    std::env::set_var("GRIT_BENCH_SCRATCH", &repo);
    std::env::set_var("GRIT_BENCH_CAMPAIGN_CWD", &repo);
    let out = setup(&repo)?;
    anyhow::ensure!(
        conflicts_meta_path(&repo).is_file(),
        "missing conflicts meta for {name} (repo={})",
        repo.display()
    );
    Ok(out)
}

/// Run conflict-operation scenarios at each file count.
pub fn run_conflicts_suite(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    sizes: &[usize],
    timestamp: time::OffsetDateTime,
) -> Result<BenchReport> {
    let mut scenarios = Vec::new();
    for &size in sizes {
        let spec = conflicts_spec(size);
        let fixture = format!("synthetic-{size}");
        let touch = conflicts_touch_paths(size);
        let overlap = conflict_overlap_for_size(size);

        let (repo, meta_clean) = with_scenario_repo("merge-no-ff", size, |dir| {
            setup_conflicts_clean_fixture_in(git, dir, spec, touch)
        })?;
        let (grit_m, git_m) = merge_no_ff_argv(&meta_clean);
        scenarios.push(run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: scenario_id("merge-no-ff", size),
                group: "conflicts".into(),
                fixture: fixture.clone(),
                description: "three-way merge with --no-ff (clean sibling branches)".into(),
                grit_argv: grit_m,
                git_argv: git_m,
                driver: Driver::Cli,
                prepare_kind: Some(PrepareKind::ConflictsMergeNoFfReset),
                grit_via_shell: false,
                git_via_shell: false,
            },
            &repo,
        )?);

        let (repo, meta_conflict) = with_scenario_repo("merge-conflict-continue", size, |dir| {
            setup_conflicts_merge_fixture_in(git, dir, spec, touch, overlap)
        })?;
        let (grit_c, git_c) = merge_conflict_continue_argv(grit, git, &meta_conflict);
        scenarios.push(run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: scenario_id("merge-conflict-continue", size),
                group: "conflicts".into(),
                fixture: fixture.clone(),
                description: format!(
                    "merge with {overlap} overlapping paths, restore --theirs, add, --continue"
                ),
                grit_argv: grit_c,
                git_argv: git_c,
                driver: Driver::Cli,
                prepare_kind: Some(PrepareKind::ConflictsMergeConflictContinueReset),
                grit_via_shell: true,
                git_via_shell: true,
            },
            &repo,
        )?);

        let (repo, meta_pick) = with_scenario_repo("pick-range-20", size, |dir| {
            setup_conflicts_pick_range_fixture_in(git, dir, spec)
        })?;
        let (grit_p, git_p) = pick_range_argv(&meta_pick);
        scenarios.push(run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: scenario_id("pick-range-20", size),
                group: "conflicts".into(),
                fixture: fixture.clone(),
                description:
                    "pick 20 topic commits in one invocation vs git cherry-pick base..topic".into(),
                grit_argv: grit_p,
                git_argv: git_p,
                driver: Driver::Cli,
                prepare_kind: Some(PrepareKind::ConflictsPickRangeReset),
                grit_via_shell: false,
                git_via_shell: false,
            },
            &repo,
        )?);

        let (repo, meta_revert) = with_scenario_repo("revert-single", size, |dir| {
            setup_conflicts_clean_fixture_in(git, dir, spec, touch)
        })?;
        let (grit_r, git_r) = revert_single_argv(&meta_revert);
        scenarios.push(run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: scenario_id("revert-single", size),
                group: "conflicts".into(),
                fixture: fixture.clone(),
                description: format!("revert one {touch}-path topic commit on main"),
                grit_argv: grit_r,
                git_argv: git_r,
                driver: Driver::Cli,
                prepare_kind: Some(PrepareKind::ConflictsRevertSingleReset),
                grit_via_shell: false,
                git_via_shell: false,
            },
            &repo,
        )?);

        let (repo, meta_mainline) = with_scenario_repo("revert-mainline", size, |dir| {
            setup_conflicts_revert_mainline_fixture_in(git, dir, spec, touch)
        })?;
        let (grit_rm, git_rm) = revert_mainline_argv(&meta_mainline);
        scenarios.push(run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: scenario_id("revert-mainline", size),
                group: "conflicts".into(),
                fixture: fixture.clone(),
                description: "revert a --no-ff merge commit with --mainline 1".into(),
                grit_argv: grit_rm,
                git_argv: git_rm,
                driver: Driver::Cli,
                prepare_kind: Some(PrepareKind::ConflictsRevertMainlineReset),
                grit_via_shell: false,
                git_via_shell: false,
            },
            &repo,
        )?);
    }
    std::env::remove_var("GRIT_BENCH_SCRATCH");
    std::env::remove_var("GRIT_BENCH_CAMPAIGN_CWD");
    Ok(build_report(git, grit, timestamp, scenarios))
}

/// Campaign matrix rows for conflict scenarios (single fixture size per call).
pub fn campaign_conflicts_scenarios(
    grit: &Path,
    git: &Path,
    meta: &ConflictsMeta,
    fixture_label: &str,
    id_prefix: &str,
) -> Vec<Scenario> {
    let touch = meta.base.pick_series_commits.len();
    let overlap = meta.conflict_path_count;
    let (grit_m, git_m) = merge_no_ff_argv(meta);
    let (grit_c, git_c) = merge_conflict_continue_argv(grit, git, meta);
    let (grit_p, git_p) = pick_range_argv(meta);
    let (grit_r, git_r) = revert_single_argv(meta);
    let (grit_rm, git_rm) = revert_mainline_argv(meta);

    vec![
        Scenario {
            id: format!("{id_prefix}/merge-no-ff"),
            group: "conflicts".into(),
            fixture: fixture_label.into(),
            description: "three-way merge with --no-ff".into(),
            grit_argv: grit_m,
            git_argv: git_m,
            driver: Driver::Cli,
            prepare_kind: Some(PrepareKind::ConflictsMergeNoFfReset),
            grit_via_shell: false,
            git_via_shell: false,
        },
        Scenario {
            id: format!("{id_prefix}/merge-conflict-continue"),
            group: "conflicts".into(),
            fixture: fixture_label.into(),
            description: format!("merge conflict continue ({overlap} overlapping paths)"),
            grit_argv: grit_c,
            git_argv: git_c,
            driver: Driver::Cli,
            prepare_kind: Some(PrepareKind::ConflictsMergeConflictContinueReset),
            grit_via_shell: true,
            git_via_shell: true,
        },
        Scenario {
            id: format!("{id_prefix}/pick-range-20"),
            group: "conflicts".into(),
            fixture: fixture_label.into(),
            description: if touch == 20 {
                "pick 20 commits vs cherry-pick range".into()
            } else {
                "pick series vs cherry-pick range".into()
            },
            grit_argv: grit_p,
            git_argv: git_p,
            driver: Driver::Cli,
            prepare_kind: Some(PrepareKind::ConflictsPickRangeReset),
            grit_via_shell: false,
            git_via_shell: false,
        },
        Scenario {
            id: format!("{id_prefix}/revert-single"),
            group: "conflicts".into(),
            fixture: fixture_label.into(),
            description: "revert single topic commit".into(),
            grit_argv: grit_r,
            git_argv: git_r,
            driver: Driver::Cli,
            prepare_kind: Some(PrepareKind::ConflictsRevertSingleReset),
            grit_via_shell: false,
            git_via_shell: false,
        },
        Scenario {
            id: format!("{id_prefix}/revert-mainline"),
            group: "conflicts".into(),
            fixture: fixture_label.into(),
            description: "revert merge commit (--mainline 1)".into(),
            grit_argv: grit_rm,
            git_argv: git_rm,
            driver: Driver::Cli,
            prepare_kind: Some(PrepareKind::ConflictsRevertMainlineReset),
            grit_via_shell: false,
            git_via_shell: false,
        },
    ]
}
