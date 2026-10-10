//! Reachability bitmap scenarios on a repacked `git.git` clone.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;

use crate::binary::{grit_source_commit, tool_version};
use crate::bitmap_fixture::{bitmap_isolated_env, ensure_bitmaps_git_repo};
use crate::machine::{collect_machine_info, format_timestamp};
use crate::odb_fixture::odb_scratch_root;
use crate::resource::{bench_with_peak_rss, serve_clone_caps, wrap_command_with_timeout};
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
    let grit_command = if scenario.id.contains("serve-clone") {
        wrap_command_with_timeout(&grit_cmd, caps.timeout)
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
                &grit_command,
                repo,
                cfg.warmup,
                cfg.min_runs,
                "grit",
                env.as_deref(),
            )?;
            if scenario.id.contains("serve-clone") && grit_rss > caps.max_rss_bytes {
                anyhow::bail!("peak RSS {grit_rss} exceeds cap {}", caps.max_rss_bytes);
            }
            grit_stats.peak_rss_bytes = Some(grit_rss);
            Ok(grit_stats)
        })();
        match grit_outcome {
            Ok(stats) => {
                let ratio = median_ratio(git_stats.median_ms, stats.median_ms);
                (stats, ratio, None)
            }
            Err(e) => {
                let fail = bench_failure_from_error(&e);
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

fn bench_failure_from_error(err: &anyhow::Error) -> BenchFailure {
    let msg = err.to_string();
    if msg.contains("exit status: 137") || msg.contains("signal 9") || msg.contains("Killed") {
        BenchFailure {
            kind: "memory_cap".into(),
            message: msg,
        }
    } else if msg.contains("exit status: 124") || msg.contains("timed out") {
        BenchFailure {
            kind: "timeout".into(),
            message: msg,
        }
    } else {
        BenchFailure {
            kind: "command_failed".into(),
            message: msg,
        }
    }
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
    let wrapped = wrap_command_with_timeout(&grit_cmd, caps.timeout);
    let env = bitmap_isolated_env();
    let full = format!("{env} {wrapped}");
    let start = std::time::Instant::now();
    match crate::resource::peak_rss_for_command(&full, repo, bench_exe) {
        Ok(rss) if rss <= caps.max_rss_bytes => Ok(None),
        Ok(rss) => Ok(Some(BenchFailure {
            kind: "memory_cap".into(),
            message: format!("peak RSS {rss} exceeded cap {}", caps.max_rss_bytes),
        })),
        Err(e) => {
            let elapsed = start.elapsed();
            if elapsed >= caps.timeout.saturating_sub(Duration::from_secs(1)) {
                Ok(Some(BenchFailure {
                    kind: "timeout".into(),
                    message: format!("exceeded {}s cap: {e}", caps.timeout.as_secs()),
                }))
            } else {
                Ok(Some(bench_failure_from_error(&e)))
            }
        }
    }
}
