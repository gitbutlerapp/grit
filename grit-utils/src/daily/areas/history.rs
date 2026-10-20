//! Revwalk and blame headlines on git.git worktree.

use super::super::helpers::{
    blame_file_bench, ctx, headline_spec, log_oneline_bench, setup_git_git_worktree,
    show_head_bench,
};
use super::super::plan::{DailyHeadlinePlan, DailyPrepare, DailySetup};
use crate::campaign_worktree::blame_target_path;

fn setup_git_git(
    setup: &DailySetup<'_>,
) -> anyhow::Result<super::super::plan::DailyHeadlineContext> {
    let repo = setup_git_git_worktree(setup)?;
    let mut ctx = ctx(repo, setup);
    ctx.blame_path = Some(blame_target_path(setup.git, &ctx.repo)?);
    Ok(ctx)
}

pub fn plans() -> Vec<DailyHeadlinePlan> {
    vec![
        DailyHeadlinePlan {
            spec: headline_spec("log/oneline-100@git-git"),
            pending: false,
            setup: setup_git_git,
            prepare: DailyPrepare::None,
            build_git: |c| log_oneline_bench(c).1,
            build_grit: |c| log_oneline_bench(c).0,
        },
        DailyHeadlinePlan {
            spec: headline_spec("show/head@git-git"),
            pending: false,
            setup: setup_git_git,
            prepare: DailyPrepare::None,
            build_git: |c| show_head_bench(c).1,
            build_grit: |c| show_head_bench(c).0,
        },
        DailyHeadlinePlan {
            spec: headline_spec("blame/file@git-git"),
            pending: false,
            setup: setup_git_git,
            prepare: DailyPrepare::None,
            build_git: |c| {
                let path = c.blame_path.as_deref().unwrap_or("README.md");
                blame_file_bench(c, path).1
            },
            build_grit: |c| {
                let path = c.blame_path.as_deref().unwrap_or("README.md");
                blame_file_bench(c, path).0
            },
        },
    ]
}
