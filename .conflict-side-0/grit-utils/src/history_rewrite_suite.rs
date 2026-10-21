//! `grit history` rewrite scenarios vs system `git history` and scripted `git rebase`.

use std::path::Path;

use anyhow::{Context, Result};
use time::OffsetDateTime;

use crate::binary::{grit_source_commit, tool_version};
use crate::history_rewrite_compat::{
    drop_git_rebase_scenario, drop_scenario, fixup_git_rebase_scenario, fixup_scenario,
    git_drop_primary_is_history, git_fixup_primary_is_history, git_reword_primary_is_history,
    git_squash_primary_is_history, reword_git_rebase_scenario, reword_scenario,
    squash_git_rebase_scenario, squash_scenario,
};
use crate::history_rewrite_fixture::{
    ensure_history_rewrite_work, history_rewrite_fixture_label, load_history_rewrite_meta,
};
use crate::history_rewrite_prepare::{install_history_rewrite_env, MODE_ENV};
use crate::machine::{collect_machine_info, format_timestamp};
use crate::scenarios::{run_scenario, Driver, PrepareKind, RunConfig, Scenario};
use crate::schema::{BenchReport, ToolVersions, SCHEMA_VERSION};

fn prepare_kind(mode: &str) -> PrepareKind {
    std::env::set_var(MODE_ENV, mode);
    PrepareKind::HistoryRewriteReset
}

#[allow(clippy::too_many_arguments)]
fn push_cli_scenario(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    repo: &Path,
    fixture: &str,
    id: &str,
    description: &str,
    grit_argv: Vec<String>,
    git_argv: Vec<String>,
    prepare: PrepareKind,
    scenarios: &mut Vec<crate::schema::ScenarioResult>,
) -> Result<()> {
    scenarios.push(run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: id.into(),
            group: "history".into(),
            fixture: fixture.into(),
            description: description.into(),
            grit_argv,
            git_argv,
            driver: Driver::Cli,
            prepare_kind: Some(prepare),
            grit_via_shell: true,
            git_via_shell: true,
        },
        repo,
    )?);
    Ok(())
}

/// Run history rewrite benchmarks (reword, drop, fixup, squash).
pub fn run_history_rewrite_suite(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    smoke: bool,
    timestamp: OffsetDateTime,
) -> Result<BenchReport> {
    let (_bare, _work_env) = install_history_rewrite_env(git, smoke)?;
    let repo = ensure_history_rewrite_work(git, smoke)?;
    let meta = load_history_rewrite_meta(
        &std::env::var(crate::history_rewrite_prepare::BARE_ENV)
            .map(std::path::PathBuf::from)
            .context("bare env")?,
    )?;
    let fixture = history_rewrite_fixture_label(smoke);
    let git_reword_history = git_reword_primary_is_history(git);
    let mut scenarios = Vec::new();

    let (grit_argv, git_argv) = reword_scenario(grit, git, &meta, false);
    push_cli_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &repo,
        &fixture,
        if smoke {
            "history-reword-smoke"
        } else {
            "history-reword-1000"
        },
        if git_reword_history {
            "reword deep commit vs git history reword"
        } else {
            "reword deep commit vs git rebase -i (system git < 2.54, no git history)"
        },
        grit_argv,
        if git_reword_history {
            git_argv
        } else {
            reword_git_rebase_scenario(&repo)
        },
        prepare_kind("default"),
        &mut scenarios,
    )?;

    let grit_rebase_argv = reword_git_rebase_scenario(&repo);
    push_cli_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &repo,
        &fixture,
        if smoke {
            "history-reword-smoke-rebase"
        } else {
            "history-reword-1000-rebase"
        },
        "reword deep commit vs scripted git rebase -i (rebase baseline)",
        reword_scenario(grit, git, &meta, false).0,
        grit_rebase_argv,
        prepare_kind("default"),
        &mut scenarios,
    )?;

    let (grit_argv, git_argv) = reword_scenario(grit, git, &meta, true);
    push_cli_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &repo,
        &fixture,
        if smoke {
            "history-reword-head-smoke"
        } else {
            "history-reword-update-refs-head"
        },
        "reword with --update-refs=head vs git history",
        grit_argv,
        if git_reword_history {
            git_argv
        } else {
            reword_git_rebase_scenario(&repo)
        },
        prepare_kind("default"),
        &mut scenarios,
    )?;

    let (grit_drop, git_drop) = drop_scenario(grit, git, &meta);
    push_cli_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &repo,
        &fixture,
        if smoke {
            "history-drop-smoke"
        } else {
            "history-drop-100"
        },
        if git_drop_primary_is_history(git) {
            "drop commit vs git history drop"
        } else {
            "drop commit vs git rebase -i drop"
        },
        grit_drop,
        if git_drop_primary_is_history(git) {
            git_drop
        } else {
            drop_git_rebase_scenario(&repo)
        },
        prepare_kind("default"),
        &mut scenarios,
    )?;

    let (grit_fixup, git_fixup) = fixup_scenario(grit, git, &meta);
    push_cli_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &repo,
        &fixture,
        if smoke {
            "history-fixup-smoke"
        } else {
            "history-fixup-500"
        },
        if git_fixup_primary_is_history(git) {
            "fixup with one staged file vs git history fixup"
        } else {
            "fixup with one staged file vs git rebase fixup"
        },
        grit_fixup,
        if git_fixup_primary_is_history(git) {
            git_fixup
        } else {
            fixup_git_rebase_scenario(&repo)
        },
        prepare_kind("fixup"),
        &mut scenarios,
    )?;

    let (grit_squash, git_squash) = squash_scenario(grit, git, &meta);
    push_cli_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &repo,
        &fixture,
        if smoke {
            "history-squash-smoke"
        } else {
            "history-squash-10"
        },
        if git_squash_primary_is_history(git) {
            "squash newest commits vs git history squash"
        } else {
            "squash newest commits vs git reset --soft + commit"
        },
        grit_squash,
        if git_squash_primary_is_history(git) {
            git_squash
        } else {
            squash_git_rebase_scenario(&repo)
        },
        prepare_kind("default"),
        &mut scenarios,
    )?;

    Ok(BenchReport {
        schema_version: SCHEMA_VERSION,
        timestamp: format_timestamp(timestamp),
        machine: collect_machine_info(&crate::history_fixture::history_cache_root())?,
        tools: ToolVersions {
            git: tool_version(git),
            grit: tool_version(grit),
            grit_commit: grit_source_commit(),
        },
        fixtures: vec![],
        scenarios,
    })
}
