//! Core CLI scenario matrix for one campaign fixture.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;

use crate::campaign_prep::{
    ensure_campaign_conflicts, ensure_campaign_hot_path, load_campaign_hot_path_meta,
    reset_campaign_worktree_clean, restore_campaign_worktree_to_main, switch_wide_meta,
    sync_grit_index_to_git, GIT_DOT_GIT_SWITCH_DETACH_TAG,
};
use crate::campaign_worktree::{blame_target_path, list_tracked_files};
use crate::conflicts_suite::campaign_conflicts_scenarios;
use crate::fixture::prepare_commit_iteration;
use crate::hot_path_fixture::HotPathMeta;
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

    reset_campaign_worktree_clean(git, &fixture.worktree)?;
    sync_grit_index_to_git(grit, &fixture.worktree)?;

    let mut out = Vec::new();
    let mut tracked = list_tracked_files(git, &fixture.worktree)?;
    tracked.sort_by(|a, b| a.as_os_str().cmp(b.as_os_str()));
    let modify_count = if smoke {
        5
    } else {
        48.min(tracked.len().max(1))
    };
    std::env::set_var("GRIT_BENCH_DIFF_DIRTY_COUNT", modify_count.to_string());

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
            prepare_kind: Some(PrepareKind::CampaignStatusDirty),
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
    sync_grit_index_to_git(grit, &fixture.worktree)?;
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

    sync_grit_index_to_git(grit, &fixture.worktree)?;
    // Large campaign fixtures can hit grit index edge cases after git hyperfine; record grit_failure and continue.
    let allow_grit_staging_failure = !smoke;
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
        allow_grit_staging_failure,
    )?);

    sync_grit_index_to_git(grit, &fixture.worktree)?;
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
        allow_grit_staging_failure,
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
    sync_grit_index_to_git(grit, &fixture.worktree)?;
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
    let log_allows_grit_failure = !smoke;
    out.push(run_matrix_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: sid(fixture.id, "log"),
            group: "log".into(),
            fixture: fixture.label.clone(),
            description: format!(
                "last {log_limit} commits one line each (paged grit log, {log_pages} pages)"
            ),
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

    if !smoke && fixture.id == "git-dot-git" {
        out.push(run_matrix_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: sid(fixture.id, "log-author-n50"),
                group: "log".into(),
                fixture: fixture.label.clone(),
                description: "log --author=. -n 50 (filtered author walk)".into(),
                grit_argv: vec!["log".into(), "--author=.".into(), "-n".into(), "50".into()],
                git_argv: vec![
                    "log".into(),
                    "--author=.".into(),
                    "-n".into(),
                    "50".into(),
                    "--format=%H".into(),
                ],
                driver: Driver::Cli,
                prepare_kind: None,
                grit_via_shell: false,
                git_via_shell: false,
            },
            &fixture.worktree,
            false,
        )?);
        out.push(run_matrix_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: sid(fixture.id, "log-pickaxe-S"),
                group: "log".into(),
                fixture: fixture.label.clone(),
                description: "log -S Copyright -n 30 (pickaxe occurrence walk)".into(),
                grit_argv: vec![
                    "log".into(),
                    "-S".into(),
                    "Copyright".into(),
                    "-n".into(),
                    "30".into(),
                ],
                git_argv: vec![
                    "log".into(),
                    "-S".into(),
                    "Copyright".into(),
                    "-n".into(),
                    "30".into(),
                    "--format=%H".into(),
                ],
                driver: Driver::Cli,
                prepare_kind: None,
                grit_via_shell: false,
                git_via_shell: false,
            },
            &fixture.worktree,
            false,
        )?);
    }

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
        let log_patch = Scenario {
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
        };
        if matches!(fixture.id, "git-dot-git" | "chromium-synth" | "linux") {
            let message = match fixture.id {
                "git-dot-git" => {
                    "grit log-patch on git.git exceeds factory campaign time budget; git side timed for ratio ledger"
                }
                "chromium-synth" => {
                    "grit log-patch on chromium-class synthetic exceeds factory campaign time budget; git side timed for ratio ledger"
                }
                _ => {
                    "grit log-patch on linux exceeds factory campaign time budget; git side timed for ratio ledger"
                }
            };
            out.push(run_scenario_git_only_with_grit_failure(
                hyperfine,
                git,
                grit,
                cfg,
                &log_patch,
                &fixture.worktree,
                BenchFailure {
                    kind: "runtime_cap".into(),
                    message: message.into(),
                },
            )?);
        } else {
            out.push(run_matrix_scenario(
                hyperfine,
                git,
                grit,
                cfg,
                &log_patch,
                &fixture.worktree,
                log_allows_grit_failure,
            )?);
        }
    }

    if !smoke && fixture.id == "git-dot-git" {
        append_git_dot_git_path_limited_rev_scenarios(
            hyperfine, git, grit, cfg, fixture, &mut out,
        )?;
    }

    if !smoke {
        let blame_path = blame_target_path(git, &fixture.worktree)?;
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
            !smoke,
        )?);
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
    let grit_args = [
        "merge-base",
        meta.main_tip.as_str(),
        meta.pick_commit.as_str(),
    ];
    let merge_base_allows_grit_failure = !smoke;
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
            merge_base_allows_grit_failure,
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
    append_conflicts_scenarios(hyperfine, git, grit, cfg, fixture, &mut out)?;
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

    if fixture.id == "git-dot-git" && !smoke {
        let (grit_detach, git_detach) =
            switch_detach_tag_argv(grit, git, GIT_DOT_GIT_SWITCH_DETACH_TAG, &meta.main_branch);
        out.push(run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: sid(fixture.id, "switch-detach-tag"),
                group: "switch".into(),
                fixture: fixture.label.clone(),
                description: "alternate switch --detach tag and main branch (issue #1158)".into(),
                grit_argv: grit_detach,
                git_argv: git_detach,
                driver: Driver::Cli,
                prepare_kind: Some(PrepareKind::SwitchDetachTagReset),
                grit_via_shell: true,
                git_via_shell: true,
            },
            &fixture.worktree,
        )?);
    }

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
                description: "switch-wide (~10% path delta including directory deletions)".into(),
                grit_argv: grit_wide,
                git_argv: git_wide,
                driver: Driver::Cli,
                prepare_kind: Some(PrepareKind::SwitchWideReset),
                grit_via_shell: true,
                git_via_shell: true,
            },
            &fixture.worktree,
        )?);
    }
    Ok(())
}

fn append_conflicts_scenarios(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    fixture: &CampaignFixture,
    out: &mut Vec<ScenarioResult>,
) -> Result<()> {
    let conflicts = ensure_campaign_conflicts(git, &fixture.worktree)?;
    for scenario in campaign_conflicts_scenarios(grit, git, &conflicts, &fixture.label, fixture.id)
    {
        out.push(run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &scenario,
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
    if let Ok(meta) = load_campaign_hot_path_meta(&fixture.worktree) {
        restore_campaign_worktree_to_main(git, &fixture.worktree, &meta)?;
    } else {
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
    }
    sync_grit_index_to_git(grit, &fixture.worktree)?;
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
    std::env::set_var("GRIT_BENCH_GC_SEED_COUNT", "8");
    if !smoke {
        crate::maintenance_suite::write_gc_base_marker(git, &fixture.worktree)?;
    }
    let gc_scenario = Scenario {
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
    };
    let grit_gc_failure = match fixture.id {
        "git-dot-git" => Some(BenchFailure {
            kind: "memory_cap".into(),
            message: "grit gc on git.git exceeds factory VM memory; git side timed for ratio ledger"
                .into(),
        }),
        "chromium-synth" => Some(BenchFailure {
            kind: "runtime_cap".into(),
            message: "grit gc on chromium-class synthetic exceeds factory campaign time budget; git side timed for ratio ledger"
                .into(),
        }),
        "linux" => Some(BenchFailure {
            kind: "runtime_cap".into(),
            message: "grit gc on linux kernel clone exceeds factory campaign time budget; git side timed for ratio ledger"
                .into(),
        }),
        _ => None,
    };
    if let Some(failure) = grit_gc_failure {
        out.push(run_scenario_git_only_with_grit_failure(
            hyperfine,
            git,
            grit,
            cfg,
            &gc_scenario,
            &fixture.worktree,
            failure,
        )?);
    } else {
        out.push(run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &gc_scenario,
            &fixture.worktree,
        )?);
    }

    let repack = Scenario {
        id: sid(fixture.id, "repack"),
        group: "maintenance".into(),
        fixture: fixture.label.clone(),
        description: "repack on bare remote (git: -adf; grit skipped on bare)".into(),
        grit_argv: vec!["repack".into()],
        git_argv: vec!["repack".into(), "-adf".into()],
        driver: Driver::Cli,
        prepare_kind: None,
        grit_via_shell: false,
        git_via_shell: false,
    };
    // Grit repack requires a worktree; invoking it on bare hangs instead of failing fast.
    out.push(run_scenario_git_only_with_grit_failure(
        hyperfine,
        git,
        grit,
        cfg,
        &repack,
        &fixture.bare,
        BenchFailure {
            kind: "unsupported".into(),
            message:
                "grit repack requires a worktree; bare remotes are measured with git repack only"
                    .into(),
        },
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

    let push_meta = ensure_campaign_bare_network_meta(git, &fixture.bare, smoke)?;
    let fetch_client = ensure_network_client_repo(git, &fixture.bare)?;
    let push_url = file_url(&fixture.bare)?;
    configure_push_remote(git, &fetch_client, &push_url, &push_meta.default_branch)?;
    let fetch_prepare =
        prepare_network_command(cfg, git, &fixture.bare, "prepare-network-fetch-incr");
    out.push(run_scenario_prepared(
        hyperfine,
        git,
        grit,
        cfg,
        &Scenario {
            id: sid(fixture.id, "fetch-incr-file"),
            group: "network".into(),
            fixture: fixture.label.clone(),
            description: format!(
                "incremental fetch of {} commits over file://",
                push_meta.incremental_fetch_commits
            ),
            grit_argv: vec!["fetch".into(), "origin".into()],
            git_argv: vec!["fetch".into(), "-q".into(), "origin".into()],
            driver: Driver::Cli,
            prepare_kind: None,
            grit_via_shell: false,
            git_via_shell: false,
        },
        &fetch_client,
        &fetch_prepare,
    )?);

    let push_client = fetch_client;
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

fn switch_detach_tag_argv(
    grit: &Path,
    git: &Path,
    tag: &str,
    main_branch: &str,
) -> (Vec<String>, Vec<String>) {
    let grit_cmd = chain_shell_public(
        grit,
        &[
            vec!["switch".into(), "--detach".into(), tag.to_owned()],
            vec!["switch".into(), main_branch.to_owned()],
        ],
    );
    let git_cmd = chain_shell_public(
        git,
        &[
            vec![
                "switch".into(),
                "-q".into(),
                "--detach".into(),
                tag.to_owned(),
            ],
            vec!["switch".into(), "-q".into(), main_branch.to_owned()],
        ],
    );
    (sh_script(grit_cmd), sh_script(git_cmd))
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

fn append_git_dot_git_path_limited_rev_scenarios(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    fixture: &CampaignFixture,
    out: &mut Vec<ScenarioResult>,
) -> Result<()> {
    let rows: [(&str, &str, Vec<&str>, Vec<&str>); 3] = [
        (
            "revs-path-builtin-add-c",
            "path-limited revs HEAD -- builtin/add.c (issue #1159)",
            vec!["revs", "HEAD", "--", "builtin/add.c"],
            vec!["log", "--format=%H", "--", "builtin/add.c"],
        ),
        (
            "log-path-doc-git-add-limit50",
            "path-limited log --limit 50 -- Documentation/git-add.txt (#1159)",
            vec![
                "log",
                "--limit",
                "50",
                "--json",
                "--",
                "Documentation/git-add.txt",
            ],
            vec![
                "log",
                "-n",
                "50",
                "--format=%H",
                "--",
                "Documentation/git-add.txt",
            ],
        ),
        (
            "revs-path-documentation",
            "path-limited revs HEAD -- Documentation (#1159)",
            vec!["revs", "HEAD", "--", "Documentation"],
            vec!["log", "--format=%H", "--", "Documentation"],
        ),
    ];
    for (suffix, description, grit_argv, git_argv) in rows {
        out.push(run_matrix_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: sid(fixture.id, suffix),
                group: "history".into(),
                fixture: fixture.label.clone(),
                description: description.into(),
                grit_argv: grit_argv.into_iter().map(String::from).collect(),
                git_argv: git_argv.into_iter().map(String::from).collect(),
                driver: Driver::Cli,
                prepare_kind: None,
                grit_via_shell: false,
                git_via_shell: false,
            },
            &fixture.worktree,
            false,
        )?);
    }
    Ok(())
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

    use crate::conflicts_fixture::ConflictsMeta;
    use crate::conflicts_suite::campaign_conflicts_scenarios;
    use crate::hot_path_fixture::HotPathMeta;
    use std::path::Path;

    const CAMPAIGN_CONFLICT_SUFFIXES: [&str; 5] = [
        "merge-no-ff",
        "merge-conflict-continue",
        "pick-range-20",
        "revert-single",
        "revert-mainline",
    ];

    fn sample_conflicts_meta() -> ConflictsMeta {
        ConflictsMeta {
            base: HotPathMeta {
                branch_a: "main".into(),
                branch_b: "topic".into(),
                main_branch: "main".into(),
                topic_branch: "topic".into(),
                base_commit: "base".into(),
                main_tip: "main-tip".into(),
                pick_commit: "pick".into(),
                pick_series_commits: (0..20).map(|i| format!("series-{i}")).collect(),
                wide: false,
                wide_branch_b: None,
            },
            merge_commit: "merge".into(),
            conflict_path_count: 10,
        }
    }

    #[test]
    fn campaign_conflicts_matrix_ids() {
        let meta = sample_conflicts_meta();
        for fixture in ["git-dot-git", "chromium-synth"] {
            let scenarios = campaign_conflicts_scenarios(
                Path::new("grit"),
                Path::new("git"),
                &meta,
                "label",
                fixture,
            );
            assert_eq!(scenarios.len(), CAMPAIGN_CONFLICT_SUFFIXES.len());
            for suffix in CAMPAIGN_CONFLICT_SUFFIXES {
                let id = format!("{fixture}/{suffix}");
                assert!(
                    scenarios.iter().any(|s| s.id == id),
                    "missing campaign conflict row {id}"
                );
            }
        }
    }

    #[test]
    fn production_log_limit_pages_match_git_thousand() {
        let log_limit = 1000usize;
        let pages = log_limit.div_ceil(LOG_BENCH_GRIT_PAGE_COUNT);
        assert_eq!(pages, 100);
    }
}
