//! Worktree command scenarios on registry fixtures (status, add, commit, diff).

use std::path::Path;

use anyhow::Result;

use crate::fixture_registry::FixtureKind;
use crate::scenarios::{
    build_report_with_fixtures, run_scenario, sh_script_internal, Driver, PrepareKind, RunConfig,
    Scenario,
};
use crate::schema::BenchReport;
use crate::worktree_fixture::{
    ensure_worktree_bench_repo, set_worktree_bench_cwd, shell_git_add_modified_set,
    shell_grit_add_modified_set, shell_redirect_devnull, MODIFY_FRACTION_DENOM,
};

struct WorktreeCwdGuard;

impl Drop for WorktreeCwdGuard {
    fn drop(&mut self) {
        std::env::remove_var("GRIT_BENCH_CAMPAIGN_CWD");
    }
}

fn fixture_label(kind: FixtureKind) -> String {
    kind.cache_name().to_string()
}

/// Run all worktree scenarios for one cached fixture kind.
pub fn run_worktree_group(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    kind: FixtureKind,
    offline: bool,
    timestamp: time::OffsetDateTime,
) -> Result<BenchReport> {
    let (repo, fixture_record) = ensure_worktree_bench_repo(git, kind, offline)?;
    set_worktree_bench_cwd(&repo)?;
    let _guard = WorktreeCwdGuard;
    let fixture = fixture_label(kind);

    let git_status_clean = sh_script_internal(shell_redirect_devnull(git, &["status", "-s"]));
    let grit_status_clean = sh_script_internal(shell_redirect_devnull(grit, &["status"]));
    let git_status_dirty = sh_script_internal(shell_redirect_devnull(git, &["status", "-s"]));
    let grit_status_dirty = sh_script_internal(shell_redirect_devnull(grit, &["status"]));

    let git_add = sh_script_internal(shell_git_add_modified_set(git));
    let grit_add = sh_script_internal(shell_grit_add_modified_set(grit));

    let git_commit = sh_script_internal(shell_redirect_devnull(
        git,
        &["commit", "-q", "-m", "bench"],
    ));
    let grit_commit = sh_script_internal(shell_redirect_devnull(grit, &["commit", "bench"]));

    let git_diff = sh_script_internal(shell_redirect_devnull(git, &["diff"]));
    let grit_diff = sh_script_internal(shell_redirect_devnull(grit, &["diff"]));

    let scenarios = vec![
        run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: format!("worktree-status-clean-{fixture}"),
                group: "worktree".into(),
                fixture: fixture.clone(),
                description: "status on a clean worktree (output discarded)".into(),
                grit_argv: grit_status_clean,
                git_argv: git_status_clean,
                driver: Driver::Cli,
                prepare_kind: Some(PrepareKind::WorktreeStatusClean),
                grit_via_shell: true,
                git_via_shell: true,
            },
            &repo,
        )?,
        run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: format!("worktree-status-dirty-{fixture}"),
                group: "worktree".into(),
                fixture: fixture.clone(),
                description: format!(
                    "status with ~1/{MODIFY_FRACTION_DENOM} modified tracked files and ~1/{MODIFY_FRACTION_DENOM} untracked (output discarded)"
                ),
                grit_argv: grit_status_dirty,
                git_argv: git_status_dirty,
                driver: Driver::Cli,
                prepare_kind: Some(PrepareKind::WorktreeStatusDirty),
                grit_via_shell: true,
                git_via_shell: true,
            },
            &repo,
        )?,
        run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: format!("worktree-add-{fixture}"),
                group: "worktree".into(),
                fixture: fixture.clone(),
                description: format!(
                    "stage ~1/{MODIFY_FRACTION_DENOM} modified paths (git add paths, grit add; output discarded)"
                ),
                grit_argv: grit_add,
                git_argv: git_add,
                driver: Driver::Cli,
                prepare_kind: Some(PrepareKind::WorktreeAdd),
                grit_via_shell: true,
                git_via_shell: true,
            },
            &repo,
        )?,
        run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: format!("worktree-commit-{fixture}"),
                group: "worktree".into(),
                fixture: fixture.clone(),
                description: format!(
                    "commit after staging ~1/{MODIFY_FRACTION_DENOM} modified paths (output discarded)"
                ),
                grit_argv: grit_commit,
                git_argv: git_commit,
                driver: Driver::Cli,
                prepare_kind: Some(PrepareKind::WorktreeCommit),
                grit_via_shell: true,
                git_via_shell: true,
            },
            &repo,
        )?,
        run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &Scenario {
                id: format!("worktree-diff-{fixture}"),
                group: "worktree".into(),
                fixture,
                description: format!(
                    "worktree vs index diff with ~1/{MODIFY_FRACTION_DENOM} unstaged modifications (output discarded)"
                ),
                grit_argv: grit_diff,
                git_argv: git_diff,
                driver: Driver::Cli,
                prepare_kind: Some(PrepareKind::WorktreeDiff),
                grit_via_shell: true,
                git_via_shell: true,
            },
            &repo,
        )?,
    ];

    Ok(build_report_with_fixtures(
        git,
        grit,
        timestamp,
        scenarios,
        vec![fixture_record],
    ))
}
