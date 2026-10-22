//! git.git and many-refs daily headline scenarios via hyperfine (valid wall times + peak RSS).

use std::path::Path;

use anyhow::Result;
use time::OffsetDateTime;

use crate::fixture_registry::FixtureKind;
use crate::scenarios::{
    build_report_with_fixtures, run_scenario, sh_script_internal, Driver, RunConfig, Scenario,
};
use crate::schema::BenchReport;
use crate::worktree_fixture::{
    ensure_worktree_bench_repo, set_worktree_bench_cwd, shell_redirect_devnull,
};

struct WorktreeCwdGuard;

impl Drop for WorktreeCwdGuard {
    fn drop(&mut self) {
        std::env::remove_var("GRIT_BENCH_CAMPAIGN_CWD");
    }
}

fn headline_scenario(
    id: &str,
    fixture: &str,
    description: &str,
    grit_argv: Vec<String>,
    git_argv: Vec<String>,
) -> Scenario {
    Scenario {
        id: id.into(),
        group: "git-git-headlines".into(),
        fixture: fixture.into(),
        description: description.into(),
        grit_argv,
        git_argv,
        driver: Driver::Cli,
        prepare_kind: None,
        grit_via_shell: true,
        git_via_shell: true,
    }
}

/// Run git.git / many-refs headline rows with hyperfine timing and peak RSS sampling.
pub fn run_headlines_suite(
    hyperfine: &Path,
    git: &Path,
    grit: &Path,
    cfg: &RunConfig,
    offline: bool,
    timestamp: OffsetDateTime,
) -> Result<BenchReport> {
    let (git_git_repo, git_git_record) =
        ensure_worktree_bench_repo(git, FixtureKind::GitGit, offline)?;
    set_worktree_bench_cwd(&git_git_repo)?;
    let _guard = WorktreeCwdGuard;

    let grit_log = sh_script_internal(shell_redirect_devnull(grit, &["log", "--limit", "100"]));
    let git_log = sh_script_internal(shell_redirect_devnull(git, &["log", "--oneline", "-100"]));
    let grit_show = sh_script_internal(shell_redirect_devnull(grit, &["show"]));
    let git_show = sh_script_internal(shell_redirect_devnull(git, &["show"]));
    let grit_resolve = sh_script_internal(shell_redirect_devnull(grit, &["resolve", "HEAD"]));
    let git_resolve = sh_script_internal(shell_redirect_devnull(git, &["rev-parse", "HEAD"]));

    let mut scenarios = vec![
        run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &headline_scenario(
                "log/oneline-100@git-git",
                "git-git",
                "Last 100 commits one line each vs git log --oneline",
                grit_log,
                git_log,
            ),
            &git_git_repo,
        )?,
        run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &headline_scenario(
                "show/head@git-git",
                "git-git",
                "Show HEAD commit vs git show",
                grit_show,
                git_show,
            ),
            &git_git_repo,
        )?,
        run_scenario(
            hyperfine,
            git,
            grit,
            cfg,
            &headline_scenario(
                "resolve/head-n@git-git",
                "git-git",
                "Resolve HEAD: grit resolve HEAD vs git rev-parse HEAD",
                grit_resolve,
                git_resolve,
            ),
            &git_git_repo,
        )?,
    ];

    let (many_refs_repo, many_refs_record) =
        ensure_worktree_bench_repo(git, FixtureKind::ManyRefs, offline)?;
    set_worktree_bench_cwd(&many_refs_repo)?;
    let grit_branch = sh_script_internal(shell_redirect_devnull(grit, &["branch"]));
    let git_branch = sh_script_internal(shell_redirect_devnull(git, &["branch"]));
    scenarios.push(run_scenario(
        hyperfine,
        git,
        grit,
        cfg,
        &headline_scenario(
            "branch/list@many-refs",
            "many-refs",
            "List branches vs git branch",
            grit_branch,
            git_branch,
        ),
        &many_refs_repo,
    )?);

    Ok(build_report_with_fixtures(
        git,
        grit,
        timestamp,
        scenarios,
        vec![git_git_record, many_refs_record],
    ))
}
