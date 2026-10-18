//! Core CLI scenario matrix for one campaign fixture.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;

use crate::campaign_prep::{
    ensure_campaign_hot_path, load_campaign_hot_path_meta, restore_campaign_worktree_to_main,
    switch_wide_meta,
};
use crate::campaign_worktree::{blame_target_path, dirty_tracked_worktree, list_tracked_files};
use crate::fixture::prepare_commit_iteration;
use crate::hot_path_fixture::HotPathMeta;
use crate::maintenance_suite::seed_loose_objects;
use crate::network_fixture::{ensure_campaign_bare_network_meta, file_url};
use crate::network_suite::{ensure_network_client_repo, prepare_network_command};
use crate::real_repo_fixture::git_env;
use crate::real_repo_fixture::run_git;
use crate::real_repo_fixture::scratch_root;
use crate::scenarios::{
    chain_shell_public, grit_log_pages_shell, run_scenario, run_scenario_allow_grit_failure,
    run_scenario_git_only_with_grit_failure, run_scenario_prepared, Driver, PrepareKind, RunConfig,
    Scenario, LOG_BENCH_GRIT_PAGE_COUNT,
};
use crate::schema::{BenchFailure, ScenarioResult};
use crate::shell::{shell_command, shell_quote};

/// One campaign fixture (bare remote + local worktree).
#[derive(Debug, Clone)]
pub struct CampaignFixture {
    pub id: &'static str,
    pub label: String,
    pub bare: PathBuf,
    pub worktree: PathBuf,
}

fn sid(fixture_id: &str, name: &str) -> String {
    format!("{fixture_id}/{name}")
}

fn sh_script(script: String) -> Vec<String> {
    vec!["-c".into(), script]
}

fn probe_grit_cli(grit: &Path, repo: &Path, args: &[&str]) -> Option<BenchFailure> {
    let mut cmd = Command::new(grit);
    cmd.args(args).current_dir(repo);
    git_env(&mut cmd);
    let out = cmd.output().ok()?;
    if out.status.success() {
        return None;
    }
    let message = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let message = if message.is_empty() {
        format!("grit exited with {}", out.status)
    } else {
        message
    };
    Some(BenchFailure {
        kind: "command_failed".into(),
        message,
    })
}

fn run_matrix_scenario(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    scenario: &Scenario,
    repo: &Path,
    allow_grit_failure: bool,
) -> Result<ScenarioResult> {
    if allow_grit_failure {
        run_scenario_allow_grit_failure(hyperfine, git, grit, cfg, scenario, repo)
    } else {
        run_scenario(hyperfine, git, grit, cfg, scenario, repo)
    }
}

/// Run the core CLI matrix for `fixture`.
#[allow(clippy::too_many_arguments)]
pub fn run_fixture_cli_matrix(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    bench_exe: &Path,
    cfg: &RunConfig,
    fixture: &CampaignFixture,
    smoke: bool,
    include_file_transport: bool,
) -> Result<Vec<ScenarioResult>> {
    std::env::set_var(
        "GRIT_BENCH_CAMPAIGN_CWD",
        fixture.worktree.display().to_string(),
    );
    let _guard = CampaignCwdGuard;

    let mut out = Vec::new();
    let mut tracked = list_tracked_files(git, &fixture.worktree)?;
    tracked.sort_by(|a, b| a.as_os_str().cmp(b.as_os_str()));
    let modify_count = if smoke {
        5
    } else {
        48.min(tracked.len().max(1))
    };
    std::env::set_var("GRIT_BENCH_DIFF_DIRTY_COUNT", modify_count.to_string());

    dirty_tracked_worktree(git, &fixture.worktree)?;
    out.push(run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: sid(fixture.id, "status-dirty"),
            group: "status".into(),
            fixture: fixture.label.clone(),
            description: "status on dirty worktree (git -s vs grit status)".into(),
            grit_argv: vec!["status".into()],
            git_argv: vec!["status".into(), "-s".into()],
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &fixture.worktree,
    )?);

    run_git(git, &fixture.worktree, &["reset", "--hard", "HEAD"])?;
    run_git(
        git,
        &fixture.worktree,
        &[
            "clean",
            "-fd",
            "-q",
            "-e",
            ".grit-bench-worktree-ready",
            "-e",
            ".grit-bench-hot-path.json",
        ],
    )?;
    out.push(run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: sid(fixture.id, "status-clean"),
            group: "status".into(),
            fixture: fixture.label.clone(),
            description: "status on clean worktree".into(),
            grit_argv: vec!["status".into()],
            git_argv: vec!["status".into(), "-s".into()],
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &fixture.worktree,
    )?);

    let stage_allows_grit_failure = !smoke;
    out.push(run_matrix_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: sid(fixture.id, "add"),
            group: "add".into(),
            fixture: fixture.label.clone(),
            description: "stage all changes (git add -A vs grit add)".into(),
            grit_argv: vec!["add".into()],
            git_argv: vec!["add".into(), "-A".into()],
            driver: Driver::Cli,
            prepare_kind: Some(PrepareKind::AddIteration),
            grit_via_shell: false,
            git_via_shell: false,
        },
        &fixture.worktree,
        stage_allows_grit_failure,
    )?);

    let git_commit = chain_shell_public(
        git,
        &[
            vec!["add".into(), "-A".into()],
            vec!["commit".into(), "-q".into(), "-m".into(), "bench".into()],
        ],
    );
    out.push(run_matrix_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: sid(fixture.id, "commit"),
            group: "commit".into(),
            fixture: fixture.label.clone(),
            description: "stage all + commit after modifying ~20% of files".into(),
            grit_argv: vec!["commit".into(), "bench".into()],
            git_argv: sh_script(git_commit),
            driver: Driver::Cli,
            prepare_kind: Some(PrepareKind::CommitIteration),
            grit_via_shell: false,
            git_via_shell: true,
        },
        &fixture.worktree,
        stage_allows_grit_failure,
    )?);

    out.push(run_matrix_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: sid(fixture.id, "diff-dirty"),
            group: "diff".into(),
            fixture: fixture.label.clone(),
            description: format!("worktree diff with {modify_count} modified files"),
            grit_argv: vec!["diff".into()],
            git_argv: vec!["diff".into()],
            driver: Driver::Cli,
            prepare_kind: Some(PrepareKind::DiffDirtyReset),
            grit_via_shell: false,
            git_via_shell: false,
        },
        &fixture.worktree,
        false,
    )?);
    run_git(git, &fixture.worktree, &["reset", "--hard", "HEAD"])?;
    out.push(run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: sid(fixture.id, "diff-clean"),
            group: "diff".into(),
            fixture: fixture.label.clone(),
            description: "worktree diff on clean tree".into(),
            grit_argv: vec!["diff".into()],
            git_argv: vec!["diff".into()],
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &fixture.worktree,
    )?);

    let log_limit: usize = if smoke { 20 } else { 1000 };
    let page_size = LOG_BENCH_GRIT_PAGE_COUNT.max(1);
    let log_pages = log_limit.div_ceil(page_size);
    let (grit_log, grit_via_shell) = if smoke {
        (vec!["log".into()], false)
    } else {
        (sh_script(grit_log_pages_shell(grit, log_pages)), true)
    };
    let log_allows_grit_failure = !smoke && fixture.id == "git-dot-git";
    out.push(run_matrix_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: sid(fixture.id, "log"),
            group: "log".into(),
            fixture: fixture.label.clone(),
            description: format!("last {log_limit} commits one line each"),
            grit_argv: grit_log,
            git_argv: vec!["log".into(), format!("-{log_limit}"), "--oneline".into()],
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell,
            git_via_shell: false,
        },
        &fixture.worktree,
        log_allows_grit_failure,
    )?);

    if !smoke {
        let patch_limit = 200;
        let grit_patch = shell_command(
            bench_exe,
            &[
                "drive".into(),
                "log-patch".into(),
                fixture.worktree.display().to_string(),
                patch_limit.to_string(),
            ],
        );
        out.push(run_matrix_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: sid(fixture.id, "log-patch"),
                group: "log".into(),
                fixture: fixture.label.clone(),
                description: format!("log -p over {patch_limit} commits"),
                grit_argv: sh_script(grit_patch),
                git_argv: vec!["log".into(), "-p".into(), format!("-{patch_limit}")],
                driver: Driver::Lib,
                prepare_kind: None,
                grit_via_shell: true,
                git_via_shell: false,
            },
            &fixture.worktree,
            log_allows_grit_failure,
        )?);
    }

    if !smoke {
        let blame_path = blame_target_path(git, &fixture.worktree)?;
        let blame_allows_grit_failure = fixture.id == "git-dot-git";
        out.push(run_matrix_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: sid(fixture.id, "blame"),
                group: "blame".into(),
                fixture: fixture.label.clone(),
                description: format!("blame {blame_path}"),
                grit_argv: vec!["blame".into(), blame_path.clone()],
                git_argv: vec!["blame".into(), "--porcelain".into(), blame_path],
                driver: Driver::Cli,
                prepare_kind: None,
                grit_via_shell: false,
                git_via_shell: false,
            },
            &fixture.worktree,
            blame_allows_grit_failure,
        )?);
    }

    if !smoke && fixture.id == "git-dot-git" {
        let git_dot_git_blame: [(&str, &str, Option<&str>); 4] = [
            ("blame-makefile", "Makefile", None),
            ("blame-commit-c", "builtin/commit.c", None),
            ("blame-makefile-range", "Makefile", Some("1,80")),
            ("blame-commit-c-range", "builtin/commit.c", Some("1,80")),
        ];
        for (name, path, line_range) in git_dot_git_blame {
            let mut grit_argv = vec!["blame".into()];
            let mut git_argv = vec!["blame".into(), "--porcelain".into()];
            if let Some(spec) = line_range {
                grit_argv.push("-L".into());
                grit_argv.push(spec.into());
                git_argv.push("-L".into());
                git_argv.push(spec.into());
            }
            grit_argv.push(path.into());
            git_argv.push(path.into());
            out.push(run_matrix_scenario(
                hyperfine,
                git,
                grit,
                cfg,
                &Scenario {
                    id: sid(fixture.id, name),
                    group: "blame".into(),
                    fixture: fixture.label.clone(),
                    description: format!(
                        "blame {path}{}",
                        line_range.map_or(String::new(), |r| format!(" lines {r}"))
                    ),
                    grit_argv,
                    git_argv,
                    driver: Driver::Cli,
                    prepare_kind: None,
                    grit_via_shell: false,
                    git_via_shell: false,
                },
                &fixture.worktree,
                true,
            )?);
        }
    }

    let meta = ensure_campaign_hot_path(git, &fixture.worktree)?;
    let merge_base = Scenario {
        id: sid(fixture.id, "merge-base"),
        group: "merge-base".into(),
        fixture: fixture.label.clone(),
        description: "merge-base between main tip and topic commit".into(),
        grit_argv: vec![
            "merge-base".into(),
            meta.main_tip.clone(),
            meta.pick_commit.clone(),
        ],
        git_argv: vec![
            "merge-base".into(),
            meta.main_tip.clone(),
            meta.pick_commit.clone(),
        ],
        driver: Driver::Cli,
        prepare_kind: None,
        grit_via_shell: false,
        git_via_shell: false,
    };
    if fixture.id == "git-dot-git" {
        let grit_args = [
            "merge-base",
            meta.main_tip.as_str(),
            meta.pick_commit.as_str(),
        ];
        if let Some(failure) = probe_grit_cli(grit, &fixture.worktree, &grit_args) {
            out.push(run_scenario_git_only_with_grit_failure(
                hyperfine,
                git,
                grit,
                cfg,
                &merge_base,
                &fixture.worktree,
                failure,
            )?);
        } else {
            out.push(run_matrix_scenario(
                hyperfine,
                git,
                grit,
                cfg,
                &merge_base,
                &fixture.worktree,
                false,
            )?);
        }
    } else {
        // Chromium-class synth can OOM grit during timed merge-base; record grit_failure and continue.
        let merge_allows_grit_failure = fixture.id == "chromium-synth";
        out.push(run_matrix_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &merge_base,
            &fixture.worktree,
            merge_allows_grit_failure,
        )?);
    }

    out.push(run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: sid(fixture.id, "resolve"),
            group: "resolve".into(),
            fixture: fixture.label.clone(),
            description: "resolve HEAD".into(),
            grit_argv: vec!["resolve".into(), "HEAD".into()],
            git_argv: vec!["rev-parse".into(), "HEAD".into()],
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &fixture.worktree,
    )?);

    out.push(run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: sid(fixture.id, "cat"),
            group: "cat".into(),
            fixture: fixture.label.clone(),
            description: "cat HEAD tree object".into(),
            grit_argv: vec!["cat".into(), "HEAD^{tree}".into()],
            git_argv: vec!["cat-file".into(), "-p".into(), "HEAD^{tree}".into()],
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &fixture.worktree,
    )?);

    append_hot_path_scenarios(hyperfine, git, grit, cfg, fixture, &meta, smoke, &mut out)?;
    append_stash_scenario(hyperfine, git, grit, cfg, fixture, &mut out)?;
    append_fsck_gc_repack(hyperfine, git, grit, cfg, fixture, smoke, &mut out)?;
    if include_file_transport {
        append_file_transport(hyperfine, git, grit, cfg, fixture, smoke, &mut out)?;
    }

    Ok(out)
}

/// File:// clone/fetch/push rows (run after bitmap scenarios on tag-pinned git.git).
pub fn run_fixture_file_transport_matrix(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    fixture: &CampaignFixture,
    smoke: bool,
) -> Result<Vec<ScenarioResult>> {
    let mut out = Vec::new();
    append_file_transport(hyperfine, git, grit, cfg, fixture, smoke, &mut out)?;
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn append_hot_path_scenarios(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    fixture: &CampaignFixture,
    meta: &HotPathMeta,
    smoke: bool,
    out: &mut Vec<ScenarioResult>,
) -> Result<()> {
    let (grit_switch, git_switch) = switch_argv(grit, git, meta);
    out.push(run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: sid(fixture.id, "switch"),
            group: "switch".into(),
            fixture: fixture.label.clone(),
            description: "switch between campaign-a and campaign-b".into(),
            grit_argv: grit_switch,
            git_argv: git_switch,
            driver: Driver::Cli,
            prepare_kind: Some(PrepareKind::SwitchReset),
            grit_via_shell: true,
            git_via_shell: true,
        },
        &fixture.worktree,
    )?);

    let (grit_pick, git_pick) = pick_argv(meta);
    out.push(run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: sid(fixture.id, "pick"),
            group: "pick".into(),
            fixture: fixture.label.clone(),
            description: "pick topic commit".into(),
            grit_argv: grit_pick,
            git_argv: git_pick,
            driver: Driver::Cli,
            prepare_kind: Some(PrepareKind::PickReset),
            grit_via_shell: false,
            git_via_shell: false,
        },
        &fixture.worktree,
    )?);

    let (grit_merge, git_merge) = merge_argv(meta);
    out.push(run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: sid(fixture.id, "merge"),
            group: "merge".into(),
            fixture: fixture.label.clone(),
            description: "merge campaign-topic".into(),
            grit_argv: grit_merge,
            git_argv: git_merge,
            driver: Driver::Cli,
            prepare_kind: Some(PrepareKind::MergeReset),
            grit_via_shell: false,
            git_via_shell: false,
        },
        &fixture.worktree,
    )?);

    let (grit_revert, git_revert) = revert_argv(meta);
    out.push(run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: sid(fixture.id, "revert"),
            group: "revert".into(),
            fixture: fixture.label.clone(),
            description: "revert topic commit".into(),
            grit_argv: grit_revert,
            git_argv: git_revert,
            driver: Driver::Cli,
            prepare_kind: Some(PrepareKind::RevertReset),
            grit_via_shell: false,
            git_via_shell: false,
        },
        &fixture.worktree,
    )?);

    if !smoke {
        let wide_meta = switch_wide_meta(meta)?;
        let (grit_wide, git_wide) = switch_argv(grit, git, &wide_meta);
        out.push(run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: sid(fixture.id, "switch-wide"),
                group: "switch".into(),
                fixture: fixture.label.clone(),
                description: "switch-wide (campaign-b with extra dirty files)".into(),
                grit_argv: grit_wide,
                git_argv: git_wide,
                driver: Driver::Cli,
                prepare_kind: Some(PrepareKind::SwitchReset),
                grit_via_shell: true,
                git_via_shell: true,
            },
            &fixture.worktree,
        )?);
    }
    Ok(())
}

fn append_stash_scenario(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    fixture: &CampaignFixture,
    out: &mut Vec<ScenarioResult>,
) -> Result<()> {
    if let Ok(meta) = load_campaign_hot_path_meta(&fixture.worktree) {
        restore_campaign_worktree_to_main(git, &fixture.worktree, &meta)?;
    } else {
        run_git(git, &fixture.worktree, &["reset", "--hard", "HEAD"])?;
        run_git(git, &fixture.worktree, &["clean", "-fd", "-q"])?;
    }
    prepare_commit_iteration(&fixture.worktree, git)?;
    let grit_cmd = chain_shell_public(
        grit,
        &[vec!["stash".into()], vec!["stash".into(), "pop".into()]],
    );
    let git_cmd = chain_shell_public(
        git,
        &[
            vec!["stash".into(), "push".into(), "-q".into()],
            vec!["stash".into(), "pop".into(), "-q".into()],
        ],
    );
    out.push(run_matrix_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: sid(fixture.id, "stash-push-pop"),
            group: "stash".into(),
            fixture: fixture.label.clone(),
            description: "stash push then pop".into(),
            grit_argv: sh_script(grit_cmd),
            git_argv: sh_script(git_cmd),
            driver: Driver::Cli,
            prepare_kind: Some(PrepareKind::StashReset),
            grit_via_shell: true,
            git_via_shell: true,
        },
        &fixture.worktree,
        fixture.id == "git-dot-git",
    )?);
    Ok(())
}

fn append_fsck_gc_repack(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    fixture: &CampaignFixture,
    smoke: bool,
    out: &mut Vec<ScenarioResult>,
) -> Result<()> {
    let fsck_allows_grit_failure = fixture.id == "git-dot-git";
    out.push(run_matrix_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: sid(fixture.id, "fsck"),
            group: "maintenance".into(),
            fixture: fixture.label.clone(),
            description: "fsck --no-dangling on worktree".into(),
            grit_argv: vec!["fsck".into(), "--no-dangling".into()],
            git_argv: vec!["fsck".into(), "--no-dangling".into()],
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &fixture.worktree,
        fsck_allows_grit_failure,
    )?);

    if smoke {
        return Ok(());
    }
    std::env::set_var(
        "GRIT_BENCH_MAINTENANCE_BARE",
        fixture.bare.display().to_string(),
    );
    if !smoke {
        seed_loose_objects(git, &fixture.worktree)?;
    }
    out.push(run_matrix_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: sid(fixture.id, "gc"),
            group: "maintenance".into(),
            fixture: fixture.label.clone(),
            description: "grit gc vs git gc".into(),
            grit_argv: vec!["gc".into()],
            git_argv: vec!["gc".into()],
            driver: Driver::Cli,
            prepare_kind: Some(PrepareKind::MaintenanceReset),
            grit_via_shell: false,
            git_via_shell: false,
        },
        &fixture.worktree,
        fsck_allows_grit_failure,
    )?);

    // Grit repack currently requires a worktree; still measure git on the bare remote.
    out.push(run_matrix_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: sid(fixture.id, "repack"),
            group: "maintenance".into(),
            fixture: fixture.label.clone(),
            description: "repack on bare remote (git: -adf; grit: modern repack)".into(),
            grit_argv: vec!["repack".into()],
            git_argv: vec!["repack".into(), "-adf".into()],
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &fixture.bare,
        true,
    )?);
    Ok(())
}

fn append_file_transport(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    fixture: &CampaignFixture,
    smoke: bool,
    out: &mut Vec<ScenarioResult>,
) -> Result<()> {
    let url = file_url(&fixture.bare)?;
    let clone_dest = scratch_root().join(format!("{}-campaign-clone", fixture.id));
    let grit_clone = shell_command(
        grit,
        &[
            "clone".into(),
            url.clone(),
            clone_dest.display().to_string(),
        ],
    );
    let git_clone = shell_command(
        git,
        &[
            "clone".into(),
            "-q".into(),
            url.clone(),
            clone_dest.display().to_string(),
        ],
    );
    out.push(run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: sid(fixture.id, "clone-file"),
            group: "network".into(),
            fixture: fixture.label.clone(),
            description: "clone over file://".into(),
            grit_argv: sh_script(format!(
                "rm -rf {} && {grit_clone}",
                shell_quote(&clone_dest.to_string_lossy())
            )),
            git_argv: sh_script(format!(
                "rm -rf {} && {git_clone}",
                shell_quote(&clone_dest.to_string_lossy())
            )),
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: true,
            git_via_shell: true,
        },
        &fixture.bare,
    )?);

    if smoke {
        return Ok(());
    }

    let fetch_dest = fixture.worktree.clone();
    let grit_fetch = shell_command(grit, &["fetch".to_string(), "origin".to_string()]);
    let git_fetch = shell_command(
        git,
        &[
            "fetch".to_string(),
            "-q".to_string(),
            "origin".to_string(),
            "HEAD".to_string(),
        ],
    );
    out.push(run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: sid(fixture.id, "fetch-incr-file"),
            group: "network".into(),
            fixture: fixture.label.clone(),
            description: "incremental fetch over file://".into(),
            grit_argv: sh_script(grit_fetch),
            git_argv: sh_script(git_fetch),
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: true,
            git_via_shell: true,
        },
        &fetch_dest,
    )?);

    let push_meta = ensure_campaign_bare_network_meta(git, &fixture.bare, smoke)?;
    let push_client = ensure_network_client_repo(git, &fixture.bare)?;
    let push_url = file_url(&fixture.bare)?;
    configure_push_remote(git, &push_client, &push_url, &push_meta.default_branch)?;
    let prepare = prepare_network_command(cfg, git, &fixture.bare, "prepare-network-push");
    out.push(run_scenario_prepared(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: sid(fixture.id, "push-file"),
            group: "network".into(),
            fixture: fixture.label.clone(),
            description: format!("push {} commits over file://", push_meta.push_commits),
            grit_argv: vec!["push".into()],
            git_argv: vec![
                "push".into(),
                "-q".into(),
                "origin".into(),
                push_meta.default_branch.clone(),
            ],
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &push_client,
        &prepare,
    )?);
    Ok(())
}

fn configure_push_remote(git: &Path, client: &Path, url: &str, default_branch: &str) -> Result<()> {
    if client.join(".git").is_dir() {
        run_git(git, client, &["remote", "remove", "origin"]).ok();
    }
    run_git(git, client, &["remote", "add", "origin", url])?;
    run_git(
        git,
        client,
        &[
            "config",
            &format!("branch.{default_branch}.remote"),
            "origin",
        ],
    )?;
    run_git(
        git,
        client,
        &[
            "config",
            &format!("branch.{default_branch}.merge"),
            &format!("refs/heads/{default_branch}"),
        ],
    )?;
    Ok(())
}

fn switch_argv(grit: &Path, git: &Path, meta: &HotPathMeta) -> (Vec<String>, Vec<String>) {
    let grit_cmd = chain_shell_public(
        grit,
        &[
            vec!["switch".into(), meta.branch_b.clone()],
            vec!["switch".into(), meta.branch_a.clone()],
        ],
    );
    let git_cmd = chain_shell_public(
        git,
        &[
            vec!["switch".into(), "-q".into(), meta.branch_b.clone()],
            vec!["switch".into(), "-q".into(), meta.branch_a.clone()],
        ],
    );
    (sh_script(grit_cmd), sh_script(git_cmd))
}

fn pick_argv(meta: &HotPathMeta) -> (Vec<String>, Vec<String>) {
    (
        vec!["pick".into(), meta.pick_commit.clone()],
        vec![
            "cherry-pick".into(),
            "--no-edit".into(),
            meta.pick_commit.clone(),
        ],
    )
}

fn revert_argv(meta: &HotPathMeta) -> (Vec<String>, Vec<String>) {
    (
        vec!["revert".into(), meta.pick_commit.clone()],
        vec![
            "revert".into(),
            "--no-edit".into(),
            meta.pick_commit.clone(),
        ],
    )
}

fn merge_argv(meta: &HotPathMeta) -> (Vec<String>, Vec<String>) {
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

struct CampaignCwdGuard;

impl Drop for CampaignCwdGuard {
    fn drop(&mut self) {
        std::env::remove_var("GRIT_BENCH_CAMPAIGN_CWD");
    }
}

#[cfg(test)]
mod tests {
    use super::LOG_BENCH_GRIT_PAGE_COUNT;

    #[test]
    fn production_log_limit_pages_match_git_thousand() {
        let log_limit = 1000usize;
        let pages = log_limit.div_ceil(LOG_BENCH_GRIT_PAGE_COUNT);
        assert_eq!(pages, 100);
    }
}
