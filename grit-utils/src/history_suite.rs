//! History and object-inspection scenarios on synthetic fixtures.

use std::path::Path;

use anyhow::{Context, Result};

use crate::binary::{grit_source_commit, tool_version};
use crate::history_compat::verify_history_lib_ops;
use crate::history_fixture::{ensure_head_oid_list, ensure_synth_history_repo, load_history_meta};
use crate::machine::{collect_machine_info, format_timestamp};
use crate::odb_suite::{run_odb_scenario, OdbRunConfig};
use crate::scenarios::{
    grit_log_pages_shell, run_scenario, Driver, RunConfig, Scenario, LOG_BENCH_GRIT_PAGE_COUNT,
};
use crate::schema::{BenchReport, ToolVersions, SCHEMA_VERSION};
use crate::shell::shell_command;
use crate::synth::SynthTier;

fn tier_for_smoke(smoke: bool) -> SynthTier {
    if smoke {
        SynthTier::S
    } else {
        SynthTier::L
    }
}

fn lib_op_argv(_bench_exe: &Path, sub: &str, repo: &Path, extra: &[String]) -> Vec<String> {
    let mut v = vec!["lib-op".into(), sub.into(), repo.display().to_string()];
    v.extend_from_slice(extra);
    v
}

fn sh_redirect_stdin(cmd: &str, input: &Path) -> String {
    format!(
        "{cmd} < {}",
        shell_words::quote(input.to_string_lossy().as_ref())
    )
}

fn sh_script(body: String) -> Vec<String> {
    vec!["-c".into(), body]
}

fn log_first_n_git(n: usize) -> Vec<String> {
    vec!["log".into(), format!("-{n}"), "--oneline".into()]
}

fn log_all_git() -> Vec<String> {
    vec!["log".into(), "--oneline".into()]
}

fn log_grit_shell(grit: &Path, commit_count: usize, limit: Option<usize>) -> String {
    let pages = match limit {
        Some(n) => n.div_ceil(LOG_BENCH_GRIT_PAGE_COUNT),
        None => commit_count.div_ceil(LOG_BENCH_GRIT_PAGE_COUNT),
    };
    grit_log_pages_shell(grit, pages)
}

/// Run history scenarios (log, show, rev-list count, blame, cat-file batch).
#[allow(clippy::too_many_arguments)]
pub fn run_history_suite(
    hyperfine: &Path,
    git: &Path,
    grit_cli: &Path,
    bench_exe: &Path,
    cfg: &RunConfig,
    odb_cfg: &OdbRunConfig,
    smoke: bool,
    timestamp: time::OffsetDateTime,
) -> Result<BenchReport> {
    let tier = tier_for_smoke(smoke);
    let spec = tier.spec();
    let repo = ensure_synth_history_repo(git, tier)?;
    let meta = load_history_meta(&repo).context("load history meta")?;
    let oids = ensure_head_oid_list(git, &repo)?;

    verify_history_lib_ops(git, bench_exe, &repo, &meta.blame_path)?;

    let fixture = format!("synth-{}", tier_label(tier));
    let mut scenarios = Vec::new();

    let log_first_n = if smoke { 20 } else { 100 };
    let grit_log_first = log_grit_shell(grit_cli, spec.commit_count, Some(log_first_n));
    scenarios.push(run_scenario(
        hyperfine,
        git,
        grit_cli,
        cfg,
        &Scenario {
            id: format!("history-log-first-{log_first_n}"),
            group: "history".into(),
            fixture: fixture.clone(),
            description: format!(
                "first {log_first_n} commits one line each (git log -{log_first_n} --oneline, grit log pages)"
            ),
            grit_argv: sh_script(grit_log_first),
            git_argv: log_first_n_git(log_first_n),
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: true,
            git_via_shell: false,
        },
        &repo,
    )?);

    let grit_log_all = log_grit_shell(grit_cli, spec.commit_count, None);
    scenarios.push(run_scenario(
        hyperfine,
        git,
        grit_cli,
        cfg,
        &Scenario {
            id: "history-log-all".into(),
            group: "history".into(),
            fixture: fixture.clone(),
            description: format!(
                "full main history ({}) one line per commit (git log --oneline, grit log pages)",
                spec.commit_count
            ),
            grit_argv: sh_script(grit_log_all),
            git_argv: log_all_git(),
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: true,
            git_via_shell: false,
        },
        &repo,
    )?);

    scenarios.push(run_odb_scenario(
        hyperfine,
        bench_exe,
        git,
        odb_cfg,
        &Scenario {
            id: "history-rev-list-count".into(),
            group: "history".into(),
            fixture: fixture.clone(),
            description: "commits reachable from HEAD (`git rev-list --count HEAD`)".into(),
            grit_argv: lib_op_argv(bench_exe, "rev-list-count-head", &repo, &[]),
            git_argv: vec!["rev-list".into(), "--count".into(), "HEAD".into()],
            driver: Driver::Lib,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &repo,
    )?);

    scenarios.push(run_odb_scenario(
        hyperfine,
        bench_exe,
        git,
        odb_cfg,
        &Scenario {
            id: "history-blame".into(),
            group: "history".into(),
            fixture: fixture.clone(),
            description: format!(
                "blame high-churn path `{}` (git blame --porcelain, grit-bench lib-op)",
                meta.blame_path
            ),
            grit_argv: lib_op_argv(
                bench_exe,
                "blame-porcelain",
                &repo,
                std::slice::from_ref(&meta.blame_path),
            ),
            git_argv: vec![
                "blame".into(),
                "--porcelain".into(),
                "HEAD".into(),
                "--".into(),
                meta.blame_path.clone(),
            ],
            driver: Driver::Lib,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &repo,
    )?);

    scenarios.push(run_scenario(
        hyperfine,
        git,
        grit_cli,
        cfg,
        &Scenario {
            id: "history-show".into(),
            group: "history".into(),
            fixture: fixture.clone(),
            description: "show one commit (`git show`, `grit show`)".into(),
            grit_argv: vec!["show".into(), meta.show_oid.clone()],
            git_argv: vec!["show".into(), meta.show_oid.clone()],
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &repo,
    )?);

    let grit_inner = shell_command(
        bench_exe,
        &lib_op_argv(bench_exe, "cat-file-batch", &repo, &[]),
    );
    let git_inner = shell_command(Path::new("git"), &["cat-file", "--batch"]);
    let grit_cmd = sh_redirect_stdin(&grit_inner, &oids);
    let git_cmd = sh_redirect_stdin(&git_inner, &oids);
    scenarios.push(run_odb_scenario(
        hyperfine,
        bench_exe,
        git,
        odb_cfg,
        &Scenario {
            id: "history-cat-file-batch".into(),
            group: "history".into(),
            fixture,
            description:
                "cat-file --batch over every object reachable from HEAD (sorted OID stdin)".into(),
            grit_argv: vec![grit_cmd],
            git_argv: vec![git_cmd],
            driver: Driver::Lib,
            prepare_kind: None,
            grit_via_shell: true,
            git_via_shell: true,
        },
        &repo,
    )?);

    Ok(BenchReport {
        schema_version: SCHEMA_VERSION,
        timestamp: format_timestamp(timestamp),
        machine: collect_machine_info(&crate::history_fixture::history_cache_root())?,
        tools: ToolVersions {
            git: tool_version(git),
            grit: tool_version(grit_cli),
            grit_commit: grit_source_commit(),
        },
        scenarios,
    })
}

fn tier_label(tier: SynthTier) -> &'static str {
    match tier {
        SynthTier::S => "S",
        SynthTier::M => "M",
        SynthTier::L => "L",
    }
}
