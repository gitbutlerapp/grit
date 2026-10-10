//! Reachability bitmap scenarios on a repacked `git.git` clone.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::binary::{grit_source_commit, tool_version};
use crate::bitmap_fixture::{bitmap_isolated_env, ensure_bitmaps_git_repo};
use crate::capped_run::{TerminationKind, EXIT_MEMORY_CAP, EXIT_TIMEOUT};
use crate::machine::{collect_machine_info, format_timestamp};
use crate::odb_fixture::odb_scratch_root;
use crate::resource::{
    bench_with_peak_rss, peak_rss_for_command_with_limits, serve_clone_caps,
    wrap_command_with_serve_caps, CappedCommandError,
};
use crate::scenarios::{Driver, Scenario};
use crate::schema::{BenchFailure, BenchReport, ScenarioResult, ToolVersions, SCHEMA_VERSION};
use crate::serve_request::SERVE_CLONE_REQUEST_FILE;
use crate::shell::shell_command;
use crate::stats::median_ratio;

/// Hyperfine tuning for bitmap scenarios.
#[derive(Debug, Clone)]
pub struct BitmapRunConfig {
    pub warmup: u32,
    pub min_runs: u32,
    pub prepare_bin: PathBuf,
}

fn drive_argv(sub: &str, repo: &Path) -> Vec<String> {
    vec!["drive".into(), sub.into(), repo.display().to_string()]
}

fn sh_redirect_stdin(cmd: &str, input: &Path) -> String {
    format!(
        "{cmd} < {}",
        shell_words::quote(input.to_string_lossy().as_ref())
    )
}

fn bench_failure_from_termination(kind: TerminationKind) -> BenchFailure {
    match kind {
        TerminationKind::Timeout => BenchFailure {
            kind: "timeout".into(),
            message: format!("serve-clone exceeded wall-clock cap (exit {EXIT_TIMEOUT})"),
        },
        TerminationKind::MemoryCap {
            peak_rss_bytes,
            cap_bytes,
        } => BenchFailure {
            kind: "memory_cap".into(),
            message: format!(
                "RSS monitor stopped child at {peak_rss_bytes} bytes (cap {cap_bytes})"
            ),
        },
        TerminationKind::CommandFailed { code } => BenchFailure {
            kind: "command_failed".into(),
            message: format!("serve-clone command failed with exit code {code}"),
        },
        TerminationKind::Success => BenchFailure {
            kind: "command_failed".into(),
            message: "expected capped serve-clone failure but command succeeded".into(),
        },
    }
}

fn bench_failure_from_capped_err(err: &CappedCommandError) -> BenchFailure {
    bench_failure_from_termination(TerminationKind::from_exit_code(err.code))
}

fn run_scenario(
    hyperfine: &Path,
    bench_exe: &Path,
    git: &Path,
    cfg: &BitmapRunConfig,
    scenario: &Scenario,
    repo: &Path,
    grit_failure: Option<BenchFailure>,
) -> Result<ScenarioResult> {
    let env = Some(bitmap_isolated_env());
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
    git_stats.peak_rss_bytes = Some(git_rss);

    let caps = serve_clone_caps();
    let serve_clone = scenario.id.contains("serve-clone");
    let grit_hyperfine_cmd = if serve_clone {
        wrap_command_with_serve_caps(bench_exe, repo, &grit_cmd, caps)
    } else {
        grit_cmd.clone()
    };

    let (grit_stats, ratio, failure) = if let Some(fail) = grit_failure {
        (failed_timing_stats(), 0.0, Some(fail))
    } else {
        let grit_outcome = (|| -> Result<crate::schema::TimingStats> {
            let (mut grit_stats, grit_rss) = bench_with_peak_rss(
                hyperfine,
                bench_exe,
                &grit_hyperfine_cmd,
                repo,
                cfg.warmup,
                cfg.min_runs,
                "grit",
                env.as_deref(),
            )?;
            grit_stats.peak_rss_bytes = Some(grit_rss);
            Ok(grit_stats)
        })();
        match grit_outcome {
            Ok(stats) => {
                let ratio = median_ratio(git_stats.median_ms, stats.median_ms);
                (stats, ratio, None)
            }
            Err(e) => {
                let fail = e
                    .downcast_ref::<CappedCommandError>()
                    .map(bench_failure_from_capped_err)
                    .unwrap_or_else(|| BenchFailure {
                        kind: "command_failed".into(),
                        message: e.to_string(),
                    });
                (failed_timing_stats(), 0.0, Some(fail))
            }
        }
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
        grit_failure: failure,
    })
}

fn failed_timing_stats() -> crate::schema::TimingStats {
    crate::schema::TimingStats {
        mean_ms: 0.0,
        median_ms: 0.0,
        stddev_ms: 0.0,
        min_ms: 0.0,
        max_ms: 0.0,
        runs_ms: Vec::new(),
        peak_rss_bytes: None,
    }
}

/// Run bitmap reachability scenarios and return a JSON report.
pub fn run_bitmaps_suite(
    hyperfine: &Path,
    git: &Path,
    grit_cli: &Path,
    bench_exe: &Path,
    cfg: &BitmapRunConfig,
    repo_override: Option<&Path>,
    timestamp: time::OffsetDateTime,
) -> Result<BenchReport> {
    let repo = match repo_override {
        Some(p) => {
            crate::bitmap_fixture::assert_tag_pinned_fixture_refs(p).context(
                "bitmap --repo path is not a single-branch tag fixture; delete the cache or omit --repo",
            )?;
            if !p.join(SERVE_CLONE_REQUEST_FILE).is_file() {
                crate::serve_request::ensure_serve_clone_request(p)?;
            }
            p.to_path_buf()
        }
        None => ensure_bitmaps_git_repo(git)?,
    };

    let req = repo.join(SERVE_CLONE_REQUEST_FILE);
    let grit_serve_inner = shell_command(bench_exe, &drive_argv("serve-clone", &repo));
    let git_serve_inner = shell_command(git, &["upload-pack", "--stateless-rpc", "."]);
    let serve_scenario = Scenario {
        id: "serve-clone-git.git".into(),
        group: "bitmaps".into(),
        fixture: "git.git".into(),
        description: "upload-pack --stateless-rpc full ref closure (side-band thin-pack ofs-delta)"
            .into(),
        grit_argv: vec![sh_redirect_stdin(&grit_serve_inner, &req)],
        git_argv: vec![sh_redirect_stdin(&git_serve_inner, &req)],
        driver: Driver::Lib,
        prepare_kind: None,
        grit_via_shell: true,
        git_via_shell: true,
    };

    let caps = serve_clone_caps();
    let grit_failure = probe_serve_clone_failure(bench_exe, &repo, &serve_scenario, caps)?;

    let scenarios = vec![
        run_scenario(
            hyperfine,
            bench_exe,
            git,
            cfg,
            &Scenario {
                id: "rev-list-count-git.git".into(),
                group: "bitmaps".into(),
                fixture: "git.git".into(),
                description: "rev-list --count --all".into(),
                grit_argv: drive_argv("rev-list-count", &repo),
                git_argv: vec!["rev-list".into(), "--count".into(), "--all".into()],
                driver: Driver::Lib,
                prepare_kind: None,
                grit_via_shell: false,
                git_via_shell: false,
            },
            &repo,
            None,
        )?,
        run_scenario(
            hyperfine,
            bench_exe,
            git,
            cfg,
            &Scenario {
                id: "rev-list-count-objects-git.git".into(),
                group: "bitmaps".into(),
                fixture: "git.git".into(),
                description: "rev-list --count --objects --all --use-bitmap-index".into(),
                grit_argv: drive_argv("rev-list-count-objects", &repo),
                git_argv: vec![
                    "rev-list".into(),
                    "--count".into(),
                    "--objects".into(),
                    "--all".into(),
                    "--use-bitmap-index".into(),
                ],
                driver: Driver::Lib,
                prepare_kind: None,
                grit_via_shell: false,
                git_via_shell: false,
            },
            &repo,
            None,
        )?,
        run_scenario(
            hyperfine,
            bench_exe,
            git,
            cfg,
            &serve_scenario,
            &repo,
            grit_failure,
        )?,
    ];

    Ok(BenchReport {
        schema_version: SCHEMA_VERSION,
        timestamp: format_timestamp(timestamp),
        machine: collect_machine_info(&odb_scratch_root())?,
        tools: ToolVersions {
            git: tool_version(git),
            grit: tool_version(grit_cli),
            grit_commit: grit_source_commit(),
        },
        scenarios,
    })
}

fn probe_serve_clone_failure(
    bench_exe: &Path,
    repo: &Path,
    scenario: &Scenario,
    caps: crate::resource::ServeCloneCaps,
) -> Result<Option<BenchFailure>> {
    let grit_cmd = if scenario.grit_via_shell {
        scenario.grit_argv[0].clone()
    } else {
        shell_command(bench_exe, &scenario.grit_argv)
    };
    let env = bitmap_isolated_env();
    let full = format!("{env} {grit_cmd}");
    match peak_rss_for_command_with_limits(
        &full,
        repo,
        bench_exe,
        Some(caps.timeout),
        Some(caps.max_rss_bytes),
    ) {
        Ok(_) => Ok(None),
        Err(e) => {
            if let Some(cap_err) = e.downcast_ref::<CappedCommandError>() {
                if cap_err.code == EXIT_MEMORY_CAP {
                    return Ok(Some(BenchFailure {
                        kind: "memory_cap".into(),
                        message: format!(
                            "RSS monitor enforced cap {} bytes (exit {EXIT_MEMORY_CAP})",
                            caps.max_rss_bytes
                        ),
                    }));
                }
                if cap_err.code == EXIT_TIMEOUT {
                    return Ok(Some(bench_failure_from_termination(
                        TerminationKind::Timeout,
                    )));
                }
                return Ok(Some(bench_failure_from_termination(
                    TerminationKind::CommandFailed { code: cap_err.code },
                )));
            }
            Ok(Some(BenchFailure {
                kind: "command_failed".into(),
                message: e.to_string(),
            }))
        }
    }
}
