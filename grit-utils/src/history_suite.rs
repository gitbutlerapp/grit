//! History and object-inspection scenarios on synthetic fixtures.

use std::path::Path;

use anyhow::{Context, Result};

use crate::binary::{grit_source_commit, tool_version};
use crate::fixture::create_blame_large_tail_churn_repo;
use crate::history_compat::{
    verify_blame_porcelain, verify_blame_porcelain_line_range, verify_history_lib_ops,
};
use crate::history_fixture::{ensure_head_oid_list, ensure_synth_history_repo, load_history_meta};
use crate::machine::{collect_machine_info, format_timestamp};
use crate::odb_suite::{run_odb_scenario, OdbRunConfig};
use crate::scenarios::{run_scenario, Driver, RunConfig, Scenario, LOG_BENCH_GRIT_PAGE_COUNT};
use crate::schema::{BenchReport, ToolVersions, SCHEMA_VERSION};
use crate::shell::{shell_command, shell_quote};
use crate::synth::SynthTier;

/// Target grit `log` page passes per hyperfine sample (~512 keeps ratios stable without extreme runtime).
const TARGET_LOG_PAGE_PASSES: usize = 512;
pub const HISTORY_SHOW_SAMPLE_BATCH: usize = 128;
const HISTORY_BLAME_LARGE_LINES: usize = 5_000;
const HISTORY_BLAME_LARGE_COMMITS: usize = 150;
const HISTORY_BLAME_LARGE_PATH: &str = "large.txt";

/// Batched repetitions for a log workload with `pages` grit log invocations per repetition.
#[must_use]
pub fn log_sample_batch(pages: usize) -> usize {
    (TARGET_LOG_PAGE_PASSES / pages.max(1)).clamp(8, 256)
}

fn tier_for_run(requested: SynthTier, smoke: bool) -> SynthTier {
    if smoke && requested != SynthTier::S {
        SynthTier::S
    } else {
        requested
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

fn log_first_n_git_shell(git: &Path, n: usize, batch: usize) -> String {
    let git_q = shell_quote(&git.to_string_lossy());
    format!(
        "set -eu; for _ in $(seq 1 {batch}); do {git_q} log -{n} --oneline >/dev/null || exit $?; done"
    )
}

fn log_all_git_shell(git: &Path, batch: usize) -> String {
    let git_q = shell_quote(&git.to_string_lossy());
    format!(
        "set -eu; for _ in $(seq 1 {batch}); do {git_q} log --oneline >/dev/null || exit $?; done"
    )
}

fn show_shell(tool: &Path, oid: &str, batch: usize) -> String {
    let tool_q = shell_quote(&tool.to_string_lossy());
    let oid_q = shell_quote(oid);
    format!(
        "set -eu; for _ in $(seq 1 {batch}); do {tool_q} show {oid_q} >/dev/null || exit $?; done"
    )
}

fn grit_log_pages_oneline(grit: &Path, pages: usize) -> String {
    let grit_q = shell_quote(&grit.to_string_lossy());
    format!(
        "before=\"\"; for _ in $(seq 1 {pages}); do \
         if [ -z \"$before\" ]; then page=$({grit_q} log) || exit $?; \
         else page=$({grit_q} log --before=\"$before\") || exit $?; fi; \
         printf '%s\\n' \"$page\" | awk '!/^→/ && !/^$/ {{print}}' >/dev/null; \
         before=$(printf '%s\\n' \"$page\" | sed -n 's/^→ more: grit log --before=//p'); \
         if [ -z \"$before\" ]; then break; fi; done"
    )
}

fn log_grit_shell_batched(grit: &Path, commit_count: usize, limit: Option<usize>) -> String {
    let pages = match limit {
        Some(n) => n.div_ceil(LOG_BENCH_GRIT_PAGE_COUNT),
        None => commit_count.div_ceil(LOG_BENCH_GRIT_PAGE_COUNT),
    };
    let batch = log_sample_batch(pages);
    let inner = grit_log_pages_oneline(grit, pages);
    format!("set -eu; for _ in $(seq 1 {batch}); do {inner}; done")
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
    tier: SynthTier,
    smoke: bool,
    timestamp: time::OffsetDateTime,
) -> Result<BenchReport> {
    let tier = tier_for_run(tier, smoke);
    let spec = tier.spec();
    let repo = ensure_synth_history_repo(git, tier)?;
    let meta = load_history_meta(&repo).context("load history meta")?;
    let oids = ensure_head_oid_list(git, &repo)?;

    verify_history_lib_ops(git, bench_exe, &repo, &meta.blame_path)?;
    if !smoke {
        let large = create_blame_large_tail_churn_repo(
            git,
            HISTORY_BLAME_LARGE_LINES,
            HISTORY_BLAME_LARGE_COMMITS,
        )
        .context("large tail-churn blame fixture")?;
        verify_blame_porcelain(git, bench_exe, &large, HISTORY_BLAME_LARGE_PATH)?;
        verify_blame_porcelain_line_range(
            git,
            bench_exe,
            &large,
            HISTORY_BLAME_LARGE_PATH,
            100,
            120,
        )?;
    }

    let fixture = format!("synth-{}", tier_label(tier));
    let mut scenarios = Vec::new();

    let log_first_n: usize = if smoke { 20 } else { 100 };
    let log_first_pages = log_first_n.div_ceil(LOG_BENCH_GRIT_PAGE_COUNT);
    let log_first_batch = log_sample_batch(log_first_pages);
    let grit_log_first = log_grit_shell_batched(grit_cli, spec.commit_count, Some(log_first_n));
    let git_log_first = log_first_n_git_shell(git, log_first_n, log_first_batch);
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
                "first {log_first_n} commits ({log_first_batch}× batched: git log -{log_first_n} --oneline, grit log pages)"
            ),
            grit_argv: sh_script(grit_log_first),
            git_argv: sh_script(git_log_first),
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: true,
            git_via_shell: true,
        },
        &repo,
    )?);

    let log_all_pages = spec.commit_count.div_ceil(LOG_BENCH_GRIT_PAGE_COUNT);
    let log_all_batch = log_sample_batch(log_all_pages);
    let grit_log_all = log_grit_shell_batched(grit_cli, spec.commit_count, None);
    let git_log_all = log_all_git_shell(git, log_all_batch);
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
                "full main history ({log_all_batch}× batched, {} commits: git log --oneline, grit log pages)",
                spec.commit_count
            ),
            grit_argv: sh_script(grit_log_all),
            git_argv: sh_script(git_log_all),
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: true,
            git_via_shell: true,
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

    if !smoke {
        let large = create_blame_large_tail_churn_repo(
            git,
            HISTORY_BLAME_LARGE_LINES,
            HISTORY_BLAME_LARGE_COMMITS,
        )
        .context("large tail-churn blame fixture")?;
        let large_fixture =
            format!("blame-large-{HISTORY_BLAME_LARGE_LINES}x{HISTORY_BLAME_LARGE_COMMITS}");
        scenarios.push(run_scenario(
            hyperfine,
            git,
            grit_cli,
            cfg,
            &Scenario {
                id: "history-blame-large-full".into(),
                group: "history".into(),
                fixture: large_fixture.clone(),
                description: format!(
                    "full-file blame on {HISTORY_BLAME_LARGE_LINES}-line tail-churn file"
                ),
                grit_argv: vec!["blame".into(), HISTORY_BLAME_LARGE_PATH.into()],
                git_argv: vec![
                    "blame".into(),
                    "--porcelain".into(),
                    HISTORY_BLAME_LARGE_PATH.into(),
                ],
                driver: Driver::Cli,
                prepare_kind: None,
                grit_via_shell: false,
                git_via_shell: false,
            },
            &large,
        )?);
        scenarios.push(run_scenario(
            hyperfine,
            git,
            grit_cli,
            cfg,
            &Scenario {
                id: "history-blame-large-range".into(),
                group: "history".into(),
                fixture: large_fixture,
                description: format!(
                    "blame -L 100,120 on {HISTORY_BLAME_LARGE_LINES}-line tail-churn file"
                ),
                grit_argv: vec![
                    "blame".into(),
                    "-L".into(),
                    "100,120".into(),
                    HISTORY_BLAME_LARGE_PATH.into(),
                ],
                git_argv: vec![
                    "blame".into(),
                    "-L100,120".into(),
                    "--porcelain".into(),
                    HISTORY_BLAME_LARGE_PATH.into(),
                ],
                driver: Driver::Cli,
                prepare_kind: None,
                grit_via_shell: false,
                git_via_shell: false,
            },
            &large,
        )?);
    }

    if !smoke {
        let packed = tempfile::tempdir().context("packed blame directory")?;
        grit_test_support::blame::packed_history(packed.path(), 1800, 64 * 1024)
            .context("packed deep-history blame fixture")?;
        verify_blame_porcelain_line_range(git, bench_exe, packed.path(), "target.txt", 1, 3)?;
        scenarios.push(run_scenario(
            hyperfine, git, grit_cli, cfg,
            &Scenario {
                id: "history-blame-packed-range".into(),
                group: "history".into(),
                fixture: "packed-blame-1800x1000".into(),
                description: "three-line blame through wide changing trees, a rename and a merge in a large pack".into(),
                grit_argv: vec!["blame".into(), "--json".into(), "-L".into(), "1,3".into(), "target.txt".into()],
                git_argv: vec!["blame".into(), "--line-porcelain".into(), "-L".into(), "1,3".into(), "HEAD".into(), "--".into(), "target.txt".into()],
                driver: Driver::Cli,
                prepare_kind: None,
                grit_via_shell: false,
                git_via_shell: false,
            }, packed.path(),
        )?);
    }

    let show_batch = HISTORY_SHOW_SAMPLE_BATCH;
    scenarios.push(run_scenario(
        hyperfine,
        git,
        grit_cli,
        cfg,
        &Scenario {
            id: "history-show".into(),
            group: "history".into(),
            fixture: fixture.clone(),
            description: format!(
                "show one commit ({show_batch}× batched: `git show`, `grit show`)"
            ),
            grit_argv: sh_script(show_shell(grit_cli, &meta.show_oid, show_batch)),
            git_argv: sh_script(show_shell(git, &meta.show_oid, show_batch)),
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: true,
            git_via_shell: true,
        },
        &repo,
    )?);

    let grit_inner = shell_command(
        bench_exe,
        &lib_op_argv(bench_exe, "cat-file-batch", &repo, &[]),
    );
    let git_inner = shell_command(git, &["cat-file", "--batch"]);
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
            hyperfine: None,
        },
        fixtures: vec![],
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn log_sample_batch_scales_with_page_count() {
        assert_eq!(log_sample_batch(2), 256);
        assert_eq!(log_sample_batch(50), 10);
    }

    #[test]
    fn log_first_n_shell_batches_git_invocations() {
        let git = Path::new("/usr/bin/git");
        let batch = log_sample_batch(2);
        let script = log_first_n_git_shell(git, 20, batch);
        assert!(script.contains(&format!("seq 1 {batch}")));
        assert!(script.contains("log -20 --oneline"));
        assert!(!script.contains('\n'));
    }

    #[test]
    fn grit_log_shell_batches_page_loop() {
        let grit = Path::new("/opt/grit/grit");
        let script = log_grit_shell_batched(grit, 500, Some(20));
        assert!(script.contains("seq 1 256"));
        assert!(script.contains("seq 1 2"));
        assert!(!script.contains('\n'));
    }
}
