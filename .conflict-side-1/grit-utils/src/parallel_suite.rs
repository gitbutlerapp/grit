//! Parallel scaling scenarios (threads=1 vs auto) for grit vs git.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use time::OffsetDateTime;

use crate::bench_env::empty_global_config_path;
use crate::binary::{grit_source_commit, tool_version};
use crate::fixture::{remove_dir_robust, scratch_dir};
use crate::hyperfine::{run_hyperfine, HyperfineRun};
use crate::machine::{collect_machine_info, format_timestamp};
use crate::parallel_env::{
    grit_supports_threads_flag, grit_thread_argv, parallel_env_prefix, ThreadMode,
};
use crate::parallel_fixture::{
    assert_clone_no_checkout_worktree, ensure_bare_remote, ensure_rename_repo, ensure_repack_pack,
    ensure_switch_wide_repo, file_url, index_pack_staging_dir, parallel_clone_dir,
    parallel_clone_warm_dir, prepare_status_clean_repo, prepare_status_dirty_repo, ParallelProfile,
};
use crate::scenarios::{Driver, RunConfig, Scenario};
use crate::schema::{BenchReport, ScenarioResult, ToolVersions, SCHEMA_VERSION};
use crate::shell::{shell_command, shell_quote};
use crate::stats::{median_ratio, timing_from_hyperfine};

const PARALLEL_MIN_WARMUP: u32 = 7;
const PARALLEL_MIN_RUNS: u32 = 10;

#[derive(Debug, Clone)]
struct ParallelScenario {
    pub base: Scenario,
    pub thread_mode: ThreadMode,
    pub git_env: String,
    pub grit_env: String,
    pub grit_extra_argv: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
fn bench_with_env(
    hyperfine: &Path,
    program: &Path,
    args: &[String],
    cwd: &Path,
    cfg: &RunConfig,
    prepare: Option<&str>,
    command_name: &str,
    env_prefix: &str,
) -> Result<crate::schema::TimingStats> {
    let entry = run_hyperfine(
        hyperfine,
        &HyperfineRun {
            command: shell_command(program, args),
            working_directory: cwd.to_path_buf(),
            prepare: prepare.map(str::to_string),
            warmup: cfg.warmup,
            min_runs: cfg.min_runs,
            command_name: Some(command_name.into()),
            env_prefix: Some(env_prefix.to_string()),
        },
    )?;
    Ok(timing_from_hyperfine(&entry))
}

fn run_parallel_scenario(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    scenario: &ParallelScenario,
    repo: &Path,
    prepare: Option<&str>,
) -> Result<ScenarioResult> {
    let git_prog = if scenario.base.git_via_shell {
        PathBuf::from("/bin/sh")
    } else {
        git.to_path_buf()
    };
    let grit_prog = if scenario.base.grit_via_shell {
        PathBuf::from("/bin/sh")
    } else {
        grit.to_path_buf()
    };

    let mut grit_argv = scenario.base.grit_argv.clone();
    if !scenario.grit_extra_argv.is_empty() {
        if scenario.base.grit_via_shell {
            if grit_argv.len() >= 2 && grit_argv[0] == "-c" {
                grit_argv[1] =
                    inject_grit_thread_flags(&grit_argv[1], grit, &scenario.grit_extra_argv);
            }
        } else {
            grit_argv = scenario
                .grit_extra_argv
                .iter()
                .chain(grit_argv.iter())
                .cloned()
                .collect();
        }
    }

    let git_stats = bench_with_env(
        hyperfine,
        &git_prog,
        &scenario.base.git_argv,
        repo,
        cfg,
        prepare,
        "git",
        &scenario.git_env,
    )?;
    let grit_stats = bench_with_env(
        hyperfine,
        &grit_prog,
        &grit_argv,
        repo,
        cfg,
        prepare,
        "grit",
        &scenario.grit_env,
    )?;
    let ratio = median_ratio(git_stats.median_ms, grit_stats.median_ms);
    Ok(ScenarioResult {
        id: scenario.base.id.clone(),
        group: scenario.base.group.clone(),
        fixture: scenario.base.fixture.clone(),
        description: format!(
            "{} [threads={}]",
            scenario.base.description,
            scenario.thread_mode.label()
        ),
        driver: scenario.base.driver.into(),
        git: git_stats,
        grit: grit_stats,
        ratio,
        grit_failure: None,
    })
}

fn scenario_id(base: &str, mode: ThreadMode) -> String {
    format!("{base}-{}", mode.id_suffix())
}

fn git_env_cmd(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", empty_global_config_path())
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
}

fn git_pack_thread_arg(mode: ThreadMode) -> String {
    match mode {
        ThreadMode::One => "1".into(),
        ThreadMode::Auto => "0".into(),
    }
}

/// Run every parallel scaling scenario for the given profile.
pub fn run_parallel_suite(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    bench_exe: &Path,
    cfg: &RunConfig,
    profile: ParallelProfile,
    timestamp: OffsetDateTime,
) -> Result<BenchReport> {
    let cfg = &RunConfig {
        warmup: cfg.warmup.max(PARALLEL_MIN_WARMUP),
        min_runs: cfg.min_runs.max(PARALLEL_MIN_RUNS),
        ..cfg.clone()
    };
    let grit_threads_flag = grit_supports_threads_flag(grit);
    let fixture_label = format!("synthetic-{}", profile.suffix());
    let mut scenarios = Vec::new();

    for mode in [ThreadMode::One, ThreadMode::Auto] {
        let env = parallel_env_prefix(mode);
        let grit_extra = grit_thread_argv(mode, grit_threads_flag);

        let dirty = prepare_status_dirty_repo(git, profile)?;
        let status_prepare = status_warm_prepare(git, grit, &grit_extra);
        scenarios.push(run_parallel_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &ParallelScenario {
                base: Scenario {
                    id: scenario_id(&format!("parallel-status-dirty-{}", profile.suffix()), mode),
                    group: "parallel".into(),
                    fixture: fixture_label.clone(),
                    description: "status on dirty 100k-class worktree (git -s)".into(),
                    grit_argv: vec!["status".into()],
                    git_argv: vec!["status".into(), "-s".into()],
                    driver: Driver::Cli,
                    prepare_kind: None,
                    grit_via_shell: false,
                    git_via_shell: false,
                },
                thread_mode: mode,
                git_env: env.clone(),
                grit_env: env.clone(),
                grit_extra_argv: grit_extra.clone(),
            },
            &dirty,
            Some(&status_prepare),
        )?);

        let clean = prepare_status_clean_repo(git, profile)?;
        scenarios.push(run_parallel_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &ParallelScenario {
                base: Scenario {
                    id: scenario_id(&format!("parallel-status-clean-{}", profile.suffix()), mode),
                    group: "parallel".into(),
                    fixture: fixture_label.clone(),
                    description: "status on clean 100k-class worktree (git -s)".into(),
                    grit_argv: vec!["status".into()],
                    git_argv: vec!["status".into(), "-s".into()],
                    driver: Driver::Cli,
                    prepare_kind: None,
                    grit_via_shell: false,
                    git_via_shell: false,
                },
                thread_mode: mode,
                git_env: env.clone(),
                grit_env: env.clone(),
                grit_extra_argv: grit_extra.clone(),
            },
            &clean,
            Some(&status_prepare),
        )?);
    }

    let bare = ensure_bare_remote(git, profile)?;
    let url = file_url(&bare)?;
    for mode in [ThreadMode::One, ThreadMode::Auto] {
        let env = parallel_env_prefix(mode);
        let grit_extra = grit_thread_argv(mode, grit_threads_flag);
        let clone_dir = parallel_clone_dir();
        let warm_dir = parallel_clone_warm_dir();
        let prepare_warm = shell_command(
            &cfg.prepare_bin,
            &[
                "prepare-parallel-checkout".into(),
                "--git".into(),
                git.display().to_string(),
                "--url".into(),
                url.clone(),
                "--dest".into(),
                warm_dir.display().to_string(),
            ],
        );
        let warm_git_checkout = shell_command(
            git,
            &[
                "-C",
                &warm_dir.display().to_string(),
                "checkout",
                "-f",
                "HEAD",
            ],
        );
        let warm_grit_checkout = shell_command(
            bench_exe,
            &[
                "drive".into(),
                "checkout-branch".into(),
                warm_dir.display().to_string(),
                "HEAD".into(),
            ],
        );
        let prepare_timed = shell_command(
            &cfg.prepare_bin,
            &[
                "prepare-parallel-checkout".into(),
                "--git".into(),
                git.display().to_string(),
                "--url".into(),
                url.clone(),
                "--dest".into(),
                clone_dir.display().to_string(),
            ],
        );
        let prepare_checkout = format!(
            "{prepare_warm} && {warm_git_checkout} && {warm_grit_checkout} && {prepare_timed}"
        );

        scenarios.push(run_parallel_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &ParallelScenario {
                base: Scenario {
                    id: scenario_id(
                        &format!("parallel-checkout-fresh-{}", profile.suffix()),
                        mode,
                    ),
                    group: "parallel".into(),
                    fixture: fixture_label.clone(),
                    description:
                        "materialize worktree after clone --no-checkout (git checkout / grit switch)"
                            .into(),
                    grit_argv: sh_script(shell_command(
                        bench_exe,
                        &[
                            "drive".into(),
                            "checkout-branch".into(),
                            clone_dir.display().to_string(),
                            "HEAD".into(),
                        ],
                    )),
                    git_argv: vec![
                        "-C".into(),
                        clone_dir.display().to_string(),
                        "checkout".into(),
                        "-f".into(),
                        "HEAD".into(),
                    ],
                    driver: Driver::Lib,
                    prepare_kind: None,
                    grit_via_shell: true,
                    git_via_shell: false,
                },
                thread_mode: mode,
                git_env: env.clone(),
                grit_env: env.clone(),
                grit_extra_argv: Vec::new(),
            },
            clone_dir.parent().unwrap_or(Path::new(".")),
            Some(&prepare_checkout),
        )?);

        let prepare_clone = shell_command(
            &cfg.prepare_bin,
            &[
                "prepare-parallel-clone".into(),
                "--git".into(),
                git.display().to_string(),
                "--url".into(),
                url.clone(),
                "--dest".into(),
                clone_dir.display().to_string(),
            ],
        );
        scenarios.push(run_parallel_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &ParallelScenario {
                base: Scenario {
                    id: scenario_id(&format!("parallel-clone-file-{}", profile.suffix()), mode),
                    group: "parallel".into(),
                    fixture: fixture_label.clone(),
                    description: "file:// clone into empty directory".into(),
                    grit_argv: vec!["clone".into(), url.clone(), clone_dir.display().to_string()],
                    git_argv: vec![
                        "clone".into(),
                        "-q".into(),
                        url.clone(),
                        clone_dir.display().to_string(),
                    ],
                    driver: Driver::Cli,
                    prepare_kind: None,
                    grit_via_shell: false,
                    git_via_shell: false,
                },
                thread_mode: mode,
                git_env: env.clone(),
                grit_env: env.clone(),
                grit_extra_argv: grit_extra.clone(),
            },
            clone_dir.parent().unwrap_or(Path::new(".")),
            Some(&prepare_clone),
        )?);
    }

    let (wide_repo, meta) = ensure_switch_wide_repo(git, profile)?;
    for mode in [ThreadMode::One, ThreadMode::Auto] {
        let env = parallel_env_prefix(mode);
        let grit_extra = grit_thread_argv(mode, grit_threads_flag);
        // Thread flags are injected into the shell script in `run_parallel_scenario`.
        let grit_cmd = chain_switch(grit, &meta.branch_b, &meta.branch_a, &[]);
        let git_cmd = chain_switch_git(git, &meta.branch_b, &meta.branch_a);
        let prepare = shell_command(
            git,
            &[
                "-C",
                &wide_repo.to_string_lossy(),
                "checkout",
                "-q",
                &meta.branch_a,
            ],
        );
        scenarios.push(run_parallel_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &ParallelScenario {
                base: Scenario {
                    id: scenario_id(&format!("parallel-switch-wide-{}", profile.suffix()), mode),
                    group: "parallel".into(),
                    fixture: fixture_label.clone(),
                    description: "switch wide ~10% path delta (out and back)".into(),
                    grit_argv: sh_script(grit_cmd),
                    git_argv: sh_script(git_cmd),
                    driver: Driver::Cli,
                    prepare_kind: None,
                    grit_via_shell: true,
                    git_via_shell: true,
                },
                thread_mode: mode,
                git_env: env.clone(),
                grit_env: env.clone(),
                grit_extra_argv: grit_extra.clone(),
            },
            &wide_repo,
            Some(&prepare),
        )?);
    }

    let (pack_repo, pack_path) = ensure_repack_pack(git, profile)?;
    for mode in [ThreadMode::One, ThreadMode::Auto] {
        let env = parallel_env_prefix(mode);
        let threads = git_pack_thread_arg(mode);
        let staging = index_pack_staging_dir();
        let prepare = shell_command(
            &cfg.prepare_bin,
            &[
                "prepare-parallel-index-pack".into(),
                "--pack".into(),
                pack_path.display().to_string(),
                "--dest".into(),
                staging.display().to_string(),
            ],
        );
        let pack_in_staging = staging.join("bench.pack");
        let git_cmd = shell_command(
            git,
            &[
                "index-pack",
                &format!("--threads={threads}"),
                &pack_in_staging.to_string_lossy(),
            ],
        );
        let grit_cmd = shell_command(
            bench_exe,
            &[
                "drive".into(),
                "index-pack".into(),
                pack_in_staging.display().to_string(),
                "--repo".into(),
                pack_repo.display().to_string(),
                "--threads".into(),
                threads.clone(),
            ],
        );
        scenarios.push(run_parallel_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &ParallelScenario {
                base: Scenario {
                    id: scenario_id(&format!("parallel-index-pack-{}", profile.suffix()), mode),
                    group: "parallel".into(),
                    fixture: fixture_label.clone(),
                    description: "index-pack on repacked large-repo .pack".into(),
                    grit_argv: sh_script(grit_cmd),
                    git_argv: sh_script(git_cmd),
                    driver: Driver::Lib,
                    prepare_kind: None,
                    grit_via_shell: true,
                    git_via_shell: true,
                },
                thread_mode: mode,
                git_env: env.clone(),
                grit_env: env.clone(),
                grit_extra_argv: Vec::new(),
            },
            staging.parent().unwrap_or(Path::new(".")),
            Some(&prepare),
        )?);
    }

    for mode in [ThreadMode::One, ThreadMode::Auto] {
        let env = parallel_env_prefix(mode);
        let delta = git_pack_thread_arg(mode);
        let grit_cmd = shell_command(
            bench_exe,
            &[
                "drive".into(),
                "pack-build-all".into(),
                pack_repo.display().to_string(),
                "--delta-threads".into(),
                delta.clone(),
            ],
        );
        let git_cmd = format!(
            "{} > /dev/null",
            shell_command(git, &["pack-objects", "--all", "--stdout"])
        );
        let pack_prepare = format!("{git_cmd} && {grit_cmd}");
        scenarios.push(run_parallel_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &ParallelScenario {
                base: Scenario {
                    id: scenario_id(&format!("parallel-pack-write-{}", profile.suffix()), mode),
                    group: "parallel".into(),
                    fixture: fixture_label.clone(),
                    description: "pack-objects --all --stdout".into(),
                    grit_argv: sh_script(grit_cmd.clone()),
                    git_argv: sh_script(git_cmd.clone()),
                    driver: Driver::Lib,
                    prepare_kind: None,
                    grit_via_shell: true,
                    git_via_shell: true,
                },
                thread_mode: mode,
                git_env: env.clone(),
                grit_env: env.clone(),
                grit_extra_argv: Vec::new(),
            },
            &pack_repo,
            Some(&pack_prepare),
        )?);
    }

    let (rename_repo, old_rev, new_rev) = ensure_rename_repo(git, profile)?;
    for mode in [ThreadMode::One, ThreadMode::Auto] {
        let env = parallel_env_prefix(mode);
        let grit_cmd = shell_command(
            bench_exe,
            &[
                "drive".into(),
                "diff-rename-m".into(),
                rename_repo.display().to_string(),
                "--old".into(),
                old_rev.clone(),
                "--new".into(),
                new_rev.clone(),
            ],
        );
        let git_cmd = rename_git_diff_null_cmd(git, &old_rev, &new_rev);
        let diff_prepare = format!("{git_cmd} && {grit_cmd} && {git_cmd} && {grit_cmd}");
        scenarios.push(run_parallel_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &ParallelScenario {
                base: Scenario {
                    id: scenario_id(&format!("parallel-diff-rename-{}", profile.suffix()), mode),
                    group: "parallel".into(),
                    fixture: fixture_label.clone(),
                    description: "rename-heavy tree diff (-M, ~2000 renames)".into(),
                    grit_argv: sh_script(grit_cmd.clone()),
                    git_argv: sh_script(git_cmd.clone()),
                    driver: Driver::Lib,
                    prepare_kind: None,
                    grit_via_shell: true,
                    git_via_shell: true,
                },
                thread_mode: mode,
                git_env: env,
                grit_env: parallel_env_prefix(mode),
                grit_extra_argv: Vec::new(),
            },
            &rename_repo,
            Some(&diff_prepare),
        )?);
    }

    Ok(BenchReport {
        schema_version: SCHEMA_VERSION,
        timestamp: format_timestamp(timestamp),
        machine: collect_machine_info(&scratch_dir())?,
        tools: ToolVersions {
            git: tool_version(git),
            grit: tool_version(grit),
            grit_commit: grit_source_commit(),
        },
        fixtures: vec![],
        scenarios,
    })
}

fn sh_script(script: String) -> Vec<String> {
    vec!["-c".into(), script]
}

fn status_warm_prepare(git: &Path, grit: &Path, grit_extra: &[String]) -> String {
    let mut grit_args = grit_extra.to_vec();
    grit_args.push("status".into());
    let grit_args_ref: Vec<&str> = grit_args.iter().map(String::as_str).collect();
    format!(
        "{} && {}",
        shell_command(git, &["status", "-s"]),
        shell_command(grit, &grit_args_ref)
    )
}

fn inject_grit_thread_flags(script: &str, grit: &Path, extra: &[String]) -> String {
    if extra.is_empty() {
        return script.to_string();
    }
    let tool = shell_quote(&grit.to_string_lossy());
    let flags = extra
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    let needle = format!("{tool} ");
    let replacement = format!("{tool} {flags} ");
    script.replace(&needle, &replacement)
}

fn chain_switch(tool: &Path, first: &str, second: &str, grit_extra: &[String]) -> String {
    let mut args_a = grit_extra.to_vec();
    args_a.push("switch".into());
    args_a.push(first.into());
    let mut args_b = grit_extra.to_vec();
    args_b.push("switch".into());
    args_b.push(second.into());
    format!(
        "{} && {}",
        shell_command(tool, &args_a.iter().map(String::as_str).collect::<Vec<_>>()),
        shell_command(tool, &args_b.iter().map(String::as_str).collect::<Vec<_>>())
    )
}

fn chain_switch_git(git: &Path, first: &str, second: &str) -> String {
    format!(
        "{} && {}",
        shell_command(git, &["switch", "-q", first]),
        shell_command(git, &["switch", "-q", second])
    )
}

/// Hyperfine prepare: clone without checkout.
pub fn prepare_parallel_checkout(git: &Path, url: &str, dest: &Path) -> Result<()> {
    if dest.exists() {
        remove_dir_robust(dest);
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut cmd = Command::new(git);
    cmd.args([
        "clone",
        "-q",
        "--no-checkout",
        url,
        dest.to_str().context("dest utf8")?,
    ]);
    git_env_cmd(&mut cmd);
    let out = cmd.output().context("clone --no-checkout")?;
    if !out.status.success() {
        anyhow::bail!(
            "clone --no-checkout failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    assert_clone_no_checkout_worktree(dest)?;
    Ok(())
}

/// Hyperfine prepare: remove destination before a full clone.
pub fn prepare_parallel_clone(_git: &Path, _url: &str, dest: &Path) -> Result<()> {
    if dest.exists() {
        remove_dir_robust(dest);
    }
    Ok(())
}

/// Copy pack into staging and remove any existing `.idx`.
pub fn prepare_parallel_index_pack(pack: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        remove_dir_robust(dest);
    }
    fs::create_dir_all(dest)?;
    fs::copy(pack, dest.join("bench.pack"))?;
    let idx = dest.join("bench.idx");
    if idx.exists() {
        fs::remove_file(idx)?;
    }
    Ok(())
}

/// Shell snippet: resolved `git diff -M` between two commits, discarding output.
fn rename_git_diff_null_cmd(git: &Path, old_rev: &str, new_rev: &str) -> String {
    format!(
        "{} > /dev/null",
        shell_command(
            git,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "-M",
                old_rev,
                new_rev,
            ],
        )
    )
}

#[cfg(test)]
mod rename_git_path_tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use crate::binary::resolve_binary;
    use crate::parallel_fixture::{ensure_rename_repo, ParallelProfile};

    #[test]
    fn rename_diff_prepare_uses_resolved_git_not_path() {
        let real_git = resolve_binary("git", None).expect("system git");
        let (repo, old_rev, new_rev) =
            ensure_rename_repo(&real_git, ParallelProfile::Smoke).expect("rename smoke fixture");
        let git_cmd = rename_git_diff_null_cmd(&real_git, &old_rev, &new_rev);

        let tmp = tempfile::tempdir().expect("tempdir");
        let trap = tmp.path().join("git");
        fs::write(&trap, "#!/bin/sh\nexit 42\n").expect("write trap git");
        let mut perms = fs::metadata(&trap).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&trap, perms).expect("chmod trap git");

        let path = std::env::var("PATH").unwrap_or_default();
        let path_with_trap = format!("{}:{path}", tmp.path().display());

        let ok = Command::new("sh")
            .arg("-c")
            .arg(&git_cmd)
            .current_dir(&repo)
            .env("PATH", &path_with_trap)
            .status()
            .expect("run resolved git diff");
        assert!(
            ok.success(),
            "rename warm-up must call the resolved git binary, not PATH (got {:?})",
            ok.code()
        );

        let bad_cmd = format!(
            "git diff --no-ext-diff --no-textconv -M {} {} > /dev/null",
            shell_quote(&old_rev),
            shell_quote(&new_rev)
        );
        let bad = Command::new("sh")
            .arg("-c")
            .arg(&bad_cmd)
            .current_dir(&repo)
            .env("PATH", &path_with_trap)
            .status()
            .expect("run PATH git diff");
        assert_eq!(
            bad.code(),
            Some(42),
            "sanity: unqualified git must hit the PATH trap"
        );
    }
}
