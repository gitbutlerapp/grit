//! Ref scenarios: grit-bench refs drivers vs system `git`.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::binary::{grit_source_commit, tool_version};
use crate::hyperfine::{run_hyperfine, HyperfineRun};
use crate::machine::{collect_machine_info, format_timestamp};
use crate::refs_fixture::{
    ensure_refs_fixture, git_supports_reftable, refs_isolated_env, refs_scratch_root, work_dir_for,
    RefsFixtureKind, RefsFixtureMeta, RefsWorkMode, REF_COUNT,
};
use crate::scenarios::{Driver, Scenario};
use crate::schema::{BenchReport, ScenarioResult, ToolVersions, SCHEMA_VERSION};
use crate::shell::shell_command;
use crate::stats::timing_from_hyperfine;

/// Hyperfine tuning for ref scenarios.
#[derive(Debug, Clone)]
pub struct RefsRunConfig {
    pub warmup: u32,
    pub min_runs: u32,
    pub prepare_bin: PathBuf,
}

fn refs_drive_argv(sub: &str, repo: &Path, extra: &[String]) -> Vec<String> {
    let mut v = vec!["refs-driver".into(), sub.into(), repo.display().to_string()];
    v.extend_from_slice(extra);
    v
}

fn shell_quote_path(path: &Path) -> String {
    shell_words::quote(path.to_string_lossy().as_ref()).into_owned()
}

fn sh_redirect_stdin(cmd: &str, input: &Path) -> String {
    format!("{cmd} < {}", shell_quote_path(input))
}

fn prepare_refs_command(
    bench_exe: &Path,
    git: &Path,
    source: &Path,
    work: &Path,
    mode: RefsWorkMode,
) -> String {
    let mode_s = match mode {
        RefsWorkMode::Full => "full",
        RefsWorkMode::Empty => "empty",
    };
    let inner = shell_command(
        bench_exe,
        &[
            "prepare-refs".into(),
            "--git".into(),
            git.display().to_string(),
            "--source".into(),
            source.display().to_string(),
            "--work".into(),
            work.display().to_string(),
            "--mode".into(),
            mode_s.into(),
        ],
    );
    // Hyperfine wraps prepare in `cd <work> && …`; step out so restore can replace `work`.
    let parent = work
        .parent()
        .map(shell_quote_path)
        .unwrap_or_else(|| "'/'".to_string());
    format!("cd {parent} && {inner}")
}

fn run_refs_scenario(
    hyperfine: &Path,
    bench_exe: &Path,
    git: &Path,
    cfg: &RefsRunConfig,
    scenario: &Scenario,
    repo: &Path,
    prepare: Option<&str>,
) -> Result<ScenarioResult> {
    let env = Some(refs_isolated_env());
    let git_cmd = if scenario.git_via_shell {
        scenario.git_argv[0].clone()
    } else {
        shell_command(git, &scenario.git_argv)
    };
    let grit_cmd = if scenario.grit_via_shell {
        scenario.grit_argv[0].clone()
    } else {
        shell_command(bench_exe, &scenario.grit_argv)
    };

    let git_entry = run_hyperfine(
        hyperfine,
        &HyperfineRun {
            command: git_cmd,
            working_directory: repo.to_path_buf(),
            prepare: prepare.map(str::to_string),
            warmup: cfg.warmup,
            min_runs: cfg.min_runs,
            command_name: Some("git".into()),
            env_prefix: env.clone(),
        },
    )?;
    let grit_entry = run_hyperfine(
        hyperfine,
        &HyperfineRun {
            command: grit_cmd,
            working_directory: repo.to_path_buf(),
            prepare: prepare.map(str::to_string),
            warmup: cfg.warmup,
            min_runs: cfg.min_runs,
            command_name: Some("grit".into()),
            env_prefix: env.clone(),
        },
    )?;
    let git_stats = timing_from_hyperfine(&git_entry);
    let grit_stats = timing_from_hyperfine(&grit_entry);
    let ratio = crate::stats::median_ratio(git_stats.median_ms, grit_stats.median_ms);

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

fn fixture_label(kind: RefsFixtureKind) -> &'static str {
    match kind {
        RefsFixtureKind::Packed => "refs-100k-packed",
        RefsFixtureKind::Loose => "refs-100k-loose",
        RefsFixtureKind::Reftable => "refs-100k-reftable",
    }
}

fn add_fixture_scenarios(
    hyperfine: &Path,
    bench_exe: &Path,
    git: &Path,
    cfg: &RefsRunConfig,
    kind: RefsFixtureKind,
    out: &mut Vec<ScenarioResult>,
) -> Result<()> {
    let fixture_path = match ensure_refs_fixture(git, kind) {
        Ok(p) => p,
        Err(e) if kind == RefsFixtureKind::Reftable => {
            eprintln!("{e}");
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    let meta = RefsFixtureMeta::load(&fixture_path)?;
    let label = fixture_label(kind);
    let work_full = work_dir_for(kind, RefsWorkMode::Full);
    let work_empty = work_dir_for(kind, RefsWorkMode::Empty);
    crate::refs_fixture::restore_work_copy(&fixture_path, &work_full, RefsWorkMode::Full)?;
    crate::refs_fixture::restore_work_copy(&fixture_path, &work_empty, RefsWorkMode::Empty)?;

    let prepare_full = prepare_refs_command(
        bench_exe,
        git,
        &fixture_path,
        &work_full,
        RefsWorkMode::Full,
    );
    let prepare_empty = prepare_refs_command(
        bench_exe,
        git,
        &fixture_path,
        &work_empty,
        RefsWorkMode::Empty,
    );

    let create_stdin = fixture_path.join(&meta.create_stdin);
    let update_stdin = fixture_path.join(&meta.update_stdin);
    let delete_stdin = fixture_path.join(&meta.delete_stdin);

    out.push(run_refs_scenario(
        hyperfine,
        bench_exe,
        git,
        cfg,
        &Scenario {
            id: format!("for-each-ref-all-{label}"),
            group: "refs".into(),
            fixture: label.into(),
            description: format!("for-each-ref over {REF_COUNT} refs"),
            grit_argv: refs_drive_argv("for-each-ref", &work_full, &[]),
            git_argv: vec!["for-each-ref".into()],
            driver: Driver::Lib,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &work_full,
        Some(&prepare_full),
    )?);

    out.push(run_refs_scenario(
        hyperfine,
        bench_exe,
        git,
        cfg,
        &Scenario {
            id: format!("for-each-ref-prefix-b05-{label}"),
            group: "refs".into(),
            fixture: label.into(),
            description: "for-each-ref refs/heads/b05* (1k refs)".into(),
            grit_argv: refs_drive_argv("for-each-ref", &work_full, &["refs/heads/b05*".into()]),
            git_argv: vec!["for-each-ref".into(), "refs/heads/b05*".into()],
            driver: Driver::Lib,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &work_full,
        Some(&prepare_full),
    )?);

    let grit_create = sh_redirect_stdin(
        &shell_command(
            bench_exe,
            &refs_drive_argv("update-ref-stdin", &work_empty, &[]),
        ),
        &create_stdin,
    );
    let git_create = sh_redirect_stdin(
        &shell_command(git, &["update-ref", "--stdin"]),
        &create_stdin,
    );
    out.push(run_refs_scenario(
        hyperfine,
        bench_exe,
        git,
        cfg,
        &Scenario {
            id: format!("update-ref-create-100k-{label}"),
            group: "refs".into(),
            fixture: label.into(),
            description: format!("update-ref --stdin create {REF_COUNT} refs into empty copy"),
            grit_argv: vec![grit_create],
            git_argv: vec![git_create],
            driver: Driver::Lib,
            prepare_kind: None,
            grit_via_shell: true,
            git_via_shell: true,
        },
        &work_empty,
        Some(&prepare_empty),
    )?);

    let grit_update = sh_redirect_stdin(
        &shell_command(
            bench_exe,
            &refs_drive_argv("update-ref-stdin", &work_full, &[]),
        ),
        &update_stdin,
    );
    let git_update = sh_redirect_stdin(
        &shell_command(git, &["update-ref", "--stdin"]),
        &update_stdin,
    );
    out.push(run_refs_scenario(
        hyperfine,
        bench_exe,
        git,
        cfg,
        &Scenario {
            id: format!("update-ref-update-1k-{label}"),
            group: "refs".into(),
            fixture: label.into(),
            description: "update-ref --stdin update 1k existing refs".into(),
            grit_argv: vec![grit_update],
            git_argv: vec![git_update],
            driver: Driver::Lib,
            prepare_kind: None,
            grit_via_shell: true,
            git_via_shell: true,
        },
        &work_full,
        Some(&prepare_full),
    )?);

    let grit_delete = sh_redirect_stdin(
        &shell_command(
            bench_exe,
            &refs_drive_argv("update-ref-stdin", &work_full, &[]),
        ),
        &delete_stdin,
    );
    let git_delete = sh_redirect_stdin(
        &shell_command(git, &["update-ref", "--stdin"]),
        &delete_stdin,
    );
    out.push(run_refs_scenario(
        hyperfine,
        bench_exe,
        git,
        cfg,
        &Scenario {
            id: format!("update-ref-delete-1k-{label}"),
            group: "refs".into(),
            fixture: label.into(),
            description: "update-ref --stdin delete 1k refs".into(),
            grit_argv: vec![grit_delete],
            git_argv: vec![git_delete],
            driver: Driver::Lib,
            prepare_kind: None,
            grit_via_shell: true,
            git_via_shell: true,
        },
        &work_full,
        Some(&prepare_full),
    )?);

    Ok(())
}

/// Run ref scenarios on packed, loose, and (when supported) reftable fixtures.
pub fn run_refs_suite(
    hyperfine: &Path,
    git: &Path,
    grit_cli: &Path,
    bench_exe: &Path,
    cfg: &RefsRunConfig,
    timestamp: time::OffsetDateTime,
) -> Result<BenchReport> {
    let mut scenarios = Vec::new();
    let only = std::env::var("GRIT_BENCH_REFS_ONLY").ok();
    if only.as_deref() != Some("reftable") {
        add_fixture_scenarios(
            hyperfine,
            bench_exe,
            git,
            cfg,
            RefsFixtureKind::Packed,
            &mut scenarios,
        )?;
        add_fixture_scenarios(
            hyperfine,
            bench_exe,
            git,
            cfg,
            RefsFixtureKind::Loose,
            &mut scenarios,
        )?;
    }
    if git_supports_reftable(git) {
        add_fixture_scenarios(
            hyperfine,
            bench_exe,
            git,
            cfg,
            RefsFixtureKind::Reftable,
            &mut scenarios,
        )?;
    } else {
        eprintln!(
            "Skipping reftable fixture: system git is older than 2.45 (no --ref-format=reftable)"
        );
    }

    Ok(BenchReport {
        schema_version: SCHEMA_VERSION,
        timestamp: format_timestamp(timestamp),
        machine: collect_machine_info(&refs_scratch_root())?,
        tools: ToolVersions {
            git: tool_version(git),
            grit: tool_version(grit_cli),
            grit_commit: grit_source_commit(),
        },
        fixtures: vec![],
        scenarios,
    })
}
