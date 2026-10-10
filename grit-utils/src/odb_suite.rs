//! ODB read scenarios: grit-bench drivers vs system `git`.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::binary::{grit_source_commit, tool_version};
use crate::machine::{collect_machine_info, format_timestamp};
use crate::odb_fixture::{
    ensure_git_git_bare, ensure_hot_path_repacked, ensure_sorted_oid_list, odb_isolated_env,
};
use crate::resource::bench_with_peak_rss;
use crate::scenarios::{Driver, Scenario};
use crate::schema::{BenchReport, ScenarioResult, ToolVersions, SCHEMA_VERSION};
use crate::shell::shell_command;
use crate::stats::median_ratio;

/// Hyperfine tuning for ODB scenarios.
#[derive(Debug, Clone)]
pub struct OdbRunConfig {
    pub warmup: u32,
    pub min_runs: u32,
    pub prepare_bin: PathBuf,
}

fn run_odb_scenario(
    hyperfine: &Path,
    bench_exe: &Path,
    git: &Path,
    cfg: &OdbRunConfig,
    scenario: &Scenario,
    repo: &Path,
) -> Result<ScenarioResult> {
    let env = Some(odb_isolated_env());
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

    let (mut git_stats, git_rss) = bench_with_peak_rss(
        hyperfine,
        bench_exe,
        &git_cmd,
        repo,
        cfg.warmup,
        cfg.min_runs,
        "git",
        env.as_deref(),
    )?;
    let (mut grit_stats, grit_rss) = bench_with_peak_rss(
        hyperfine,
        bench_exe,
        &grit_cmd,
        repo,
        cfg.warmup,
        cfg.min_runs,
        "grit",
        env.as_deref(),
    )?;
    git_stats.peak_rss_bytes = Some(git_rss);
    grit_stats.peak_rss_bytes = Some(grit_rss);
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

fn drive_argv(sub: &str, repo: &Path, extra: &[String]) -> Vec<String> {
    let mut v = vec!["drive".into(), sub.into(), repo.display().to_string()];
    v.extend_from_slice(extra);
    v
}

fn sh_redirect_stdin(cmd: &str, input: &Path) -> String {
    format!("{cmd} < {}", shell_quote_path(input))
}

fn shell_quote_path(path: &Path) -> String {
    shell_words::quote(path.to_string_lossy().as_ref()).into_owned()
}

fn cat_file_sorted_scenario(
    bench_exe: &Path,
    repo: &Path,
    oid_list: &Path,
    fixture: &str,
    id_suffix: &str,
) -> Scenario {
    let grit_inner = shell_command(bench_exe, &drive_argv("cat-file-batch", repo, &[]));
    let git_inner = shell_command(Path::new("git"), &["cat-file", "--batch"]);
    let grit_cmd = sh_redirect_stdin(&grit_inner, oid_list);
    let git_cmd = sh_redirect_stdin(&git_inner, oid_list);
    let scenario = Scenario {
        id: format!("cat-file-batch-sorted-{id_suffix}"),
        group: "object_reads".into(),
        fixture: fixture.into(),
        description: "cat-file --batch over all objects in sorted OID order".into(),
        grit_argv: vec![grit_cmd],
        git_argv: vec![git_cmd],
        driver: Driver::Lib,
        prepare_kind: None,
        grit_via_shell: true,
        git_via_shell: true,
    };
    scenario
}

/// Run all ODB read scenarios and return a JSON report.
pub fn run_odb_suite(
    hyperfine: &Path,
    git: &Path,
    grit_cli: &Path,
    bench_exe: &Path,
    cfg: &OdbRunConfig,
    timestamp: time::OffsetDateTime,
) -> Result<BenchReport> {
    let mut scenarios = Vec::new();

    let git_git = ensure_git_git_bare(git)?;
    let git_git_oids = ensure_sorted_oid_list(git, &git_git)?;

    scenarios.push(run_odb_scenario(
        hyperfine,
        bench_exe,
        git,
        cfg,
        &Scenario {
            id: "cat-file-batch-unordered-git.git".into(),
            group: "object_reads".into(),
            fixture: "git.git".into(),
            description: "cat-file --batch-all-objects --unordered (pack order)".into(),
            grit_argv: drive_argv("cat-file-batch-all-unordered", &git_git, &[]),
            git_argv: vec![
                "cat-file".into(),
                "--batch".into(),
                "--batch-all-objects".into(),
                "--unordered".into(),
            ],
            driver: Driver::Lib,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &git_git,
    )?);

    let sorted_git =
        cat_file_sorted_scenario(bench_exe, &git_git, &git_git_oids, "git.git", "git.git");
    scenarios.push(run_odb_scenario(
        hyperfine,
        bench_exe,
        git,
        cfg,
        &sorted_git,
        &git_git,
    )?);

    scenarios.push(run_odb_scenario(
        hyperfine,
        bench_exe,
        git,
        cfg,
        &Scenario {
            id: "rev-list-objects-git.git".into(),
            group: "object_reads".into(),
            fixture: "git.git".into(),
            description: "rev-list --objects --all".into(),
            grit_argv: drive_argv("rev-list-objects", &git_git, &[]),
            git_argv: vec!["rev-list".into(), "--objects".into(), "--all".into()],
            driver: Driver::Lib,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &git_git,
    )?);

    scenarios.push(run_odb_scenario(
        hyperfine,
        bench_exe,
        git,
        cfg,
        &Scenario {
            id: "log-patch-2000-git.git".into(),
            group: "object_reads".into(),
            fixture: "git.git".into(),
            description: "log -p over the last 2000 commits".into(),
            grit_argv: drive_argv("log-patch", &git_git, &["2000".into()]),
            git_argv: vec!["log".into(), "-p".into(), "-2000".into()],
            driver: Driver::Lib,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &git_git,
    )?);

    let hot = ensure_hot_path_repacked(git)?;
    let hot_oids = ensure_sorted_oid_list(git, &hot)?;

    scenarios.push(run_odb_scenario(
        hyperfine,
        bench_exe,
        git,
        cfg,
        &Scenario {
            id: "cat-file-batch-unordered-hot-path-100k".into(),
            group: "object_reads".into(),
            fixture: "hot-path-100k".into(),
            description: "cat-file --batch-all-objects --unordered on repacked 100k-file repo"
                .into(),
            grit_argv: drive_argv("cat-file-batch-all-unordered", &hot, &[]),
            git_argv: vec![
                "cat-file".into(),
                "--batch".into(),
                "--batch-all-objects".into(),
                "--unordered".into(),
            ],
            driver: Driver::Lib,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &hot,
    )?);

    let sorted_hot =
        cat_file_sorted_scenario(bench_exe, &hot, &hot_oids, "hot-path-100k", "hot-path-100k");
    scenarios.push(run_odb_scenario(
        hyperfine,
        bench_exe,
        git,
        cfg,
        &sorted_hot,
        &hot,
    )?);

    scenarios.push(run_odb_scenario(
        hyperfine,
        bench_exe,
        git,
        cfg,
        &Scenario {
            id: "rev-list-objects-hot-path-100k".into(),
            group: "object_reads".into(),
            fixture: "hot-path-100k".into(),
            description: "rev-list --objects --all on 100k-file repo".into(),
            grit_argv: drive_argv("rev-list-objects", &hot, &[]),
            git_argv: vec!["rev-list".into(), "--objects".into(), "--all".into()],
            driver: Driver::Lib,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &hot,
    )?);

    scenarios.push(run_odb_scenario(
        hyperfine,
        bench_exe,
        git,
        cfg,
        &Scenario {
            id: "log-patch-2000-hot-path-100k".into(),
            group: "object_reads".into(),
            fixture: "hot-path-100k".into(),
            description: "log -p -2000 on large synthetic repo".into(),
            grit_argv: drive_argv("log-patch", &hot, &["2000".into()]),
            git_argv: vec!["log".into(), "-p".into(), "-2000".into()],
            driver: Driver::Lib,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &hot,
    )?);

    Ok(BenchReport {
        schema_version: SCHEMA_VERSION,
        timestamp: format_timestamp(timestamp),
        machine: collect_machine_info(&crate::odb_fixture::odb_scratch_root())?,
        tools: ToolVersions {
            git: tool_version(git),
            grit: tool_version(grit_cli),
            grit_commit: grit_source_commit(),
        },
        scenarios,
    })
}

fn stdin_batch_scenario(
    bench_exe: &Path,
    repo: &Path,
    oid_list: &Path,
    id: &str,
    grit_sub: &str,
    git_args: &[&str],
    description: &str,
) -> Scenario {
    let grit_inner = shell_command(bench_exe, &drive_argv(grit_sub, repo, &[]));
    let git_inner = shell_command(Path::new("git"), git_args);
    Scenario {
        id: id.into(),
        group: "odb_backend".into(),
        fixture: "hot-path-100k".into(),
        description: description.into(),
        grit_argv: vec![sh_redirect_stdin(&grit_inner, oid_list)],
        git_argv: vec![sh_redirect_stdin(&git_inner, oid_list)],
        driver: Driver::Lib,
        prepare_kind: None,
        grit_via_shell: true,
        git_via_shell: true,
    }
}

/// Run ODB cat-file / rev-list scenarios on the repacked 100k synthetic repo.
pub fn run_odb_backend_suite(
    hyperfine: &Path,
    git: &Path,
    grit_cli: &Path,
    bench_exe: &Path,
    cfg: &OdbRunConfig,
    timestamp: time::OffsetDateTime,
) -> Result<BenchReport> {
    let hot = ensure_hot_path_repacked(git)?;
    let oids = ensure_sorted_oid_list(git, &hot)?;

    let scenarios = vec![
        run_odb_scenario(
            hyperfine,
            bench_exe,
            git,
            cfg,
            &stdin_batch_scenario(
                bench_exe,
                &hot,
                &oids,
                "cat-file-batch-hot-path-100k",
                "cat-file-batch",
                &["cat-file", "--batch"],
                "cat-file --batch over all packed objects (sorted OID stdin)",
            ),
            &hot,
        )?,
        run_odb_scenario(
            hyperfine,
            bench_exe,
            git,
            cfg,
            &stdin_batch_scenario(
                bench_exe,
                &hot,
                &oids,
                "cat-file-batch-check-hot-path-100k",
                "cat-file-batch-check",
                &["cat-file", "--batch-check"],
                "cat-file --batch-check over all packed objects (sorted OID stdin)",
            ),
            &hot,
        )?,
        run_odb_scenario(
            hyperfine,
            bench_exe,
            git,
            cfg,
            &Scenario {
                id: "rev-list-objects-odb-backend-hot-path-100k".into(),
                group: "odb_backend".into(),
                fixture: "hot-path-100k".into(),
                description: "rev-list --objects --all on repacked 100k-file repo".into(),
                grit_argv: drive_argv("rev-list-objects", &hot, &[]),
                git_argv: vec!["rev-list".into(), "--objects".into(), "--all".into()],
                driver: Driver::Lib,
                prepare_kind: None,
                grit_via_shell: false,
                git_via_shell: false,
            },
            &hot,
        )?,
    ];

    Ok(BenchReport {
        schema_version: SCHEMA_VERSION,
        timestamp: format_timestamp(timestamp),
        machine: collect_machine_info(&crate::odb_fixture::odb_scratch_root())?,
        tools: ToolVersions {
            git: tool_version(git),
            grit: tool_version(grit_cli),
            grit_commit: grit_source_commit(),
        },
        scenarios,
    })
}
