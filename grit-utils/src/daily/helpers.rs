//! Shared setup and argv helpers for daily headline scenarios.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::plan::{DailyHeadlineContext, DailySetup};
use super::runner::BenchArgv;
use crate::fixture::scratch_dir;
use crate::fixture_registry::{self, FixtureKind};
use crate::hot_path_fixture::{
    setup_pick_merge_fixture, setup_switch_fixture, touch_paths_for_size, HotPathRepoSpec,
};
use crate::scenarios::{merge_argv, pick_argv, switch_argv};
use crate::worktree_fixture::{
    ensure_worktree_bench_repo, set_worktree_bench_cwd, shell_git_add_modified_set,
    shell_git_add_modified_set_and_commit, shell_grit_add_modified_set, shell_redirect_devnull,
};

pub fn synth10k_kind(smoke: bool) -> FixtureKind {
    if smoke {
        FixtureKind::Synth10kSmoke
    } else {
        FixtureKind::Synth10k
    }
}

pub fn many_refs_kind(smoke: bool) -> FixtureKind {
    if smoke {
        FixtureKind::ManyRefsSmoke
    } else {
        FixtureKind::ManyRefs
    }
}

pub fn hot_path_spec(smoke: bool) -> HotPathRepoSpec {
    if smoke {
        HotPathRepoSpec::for_size(1_000)
    } else {
        HotPathRepoSpec::large()
    }
}

fn with_scratch<T, F>(label: &str, f: F) -> Result<T>
where
    F: FnOnce(&Path) -> Result<T>,
{
    let scratch = scratch_dir().join(format!("daily-{label}"));
    std::env::set_var("GRIT_BENCH_SCRATCH", &scratch);
    f(&scratch)
}

pub fn setup_registry_worktree(setup: &DailySetup<'_>, kind: FixtureKind) -> Result<PathBuf> {
    with_scratch(kind.cache_name(), |scratch| {
        std::env::set_var("GRIT_BENCH_SCRATCH", scratch);
        let (repo, _record) =
            ensure_worktree_bench_repo(setup.git, kind, setup.offline).context("worktree repo")?;
        set_worktree_bench_cwd(&repo)?;
        Ok(repo)
    })
}

pub fn setup_git_git_worktree(setup: &DailySetup<'_>) -> Result<PathBuf> {
    setup_registry_worktree(setup, FixtureKind::GitGitWt)
}

pub fn setup_hot_path_switch(
    setup: &DailySetup<'_>,
) -> Result<super::plan::DailyHeadlineContext> {
    with_scratch("switch", |scratch| {
        std::env::set_var("GRIT_BENCH_SCRATCH", scratch);
        let spec = hot_path_spec(setup.smoke);
        let (repo, meta) = setup_switch_fixture(setup.git, spec, false)?;
        let mut ctx = ctx(repo, setup);
        ctx.hot_path_meta = Some(meta);
        Ok(ctx)
    })
}

pub fn setup_hot_path_pick_merge(
    setup: &DailySetup<'_>,
) -> Result<super::plan::DailyHeadlineContext> {
    with_scratch("pick-merge", |scratch| {
        std::env::set_var("GRIT_BENCH_SCRATCH", scratch);
        let spec = hot_path_spec(setup.smoke);
        let touch = touch_paths_for_size(spec.file_count);
        let (_repo, meta) = setup_pick_merge_fixture(setup.git, spec, touch)?;
        let mut ctx = ctx(scratch_dir(), setup);
        ctx.hot_path_meta = Some(meta);
        Ok(ctx)
    })
}

pub fn setup_many_refs_worktree(setup: &DailySetup<'_>) -> Result<PathBuf> {
    let kind = many_refs_kind(setup.smoke);
    let (cached, _record) = fixture_registry::ensure_cached(setup.git, kind, setup.offline)?;
    with_scratch(kind.cache_name(), |scratch| {
        let dest = scratch.join("branch-list");
        if dest.exists() {
            crate::fixture::remove_dir_robust(&dest);
        }
        fixture_registry::copy_for_use(setup.git, &cached, &dest)?;
        Ok(dest)
    })
}

pub fn setup_stash_repo(setup: &DailySetup<'_>) -> Result<PathBuf> {
    with_scratch("stash", |scratch| {
        std::env::set_var("GRIT_BENCH_SCRATCH", scratch);
        let size = if setup.smoke { 1_000 } else { 10_000 };
        let repo = crate::fixture::create_repo(setup.git, size).context("stash repo")?;
        crate::fixture::prepare_commit_iteration(&repo, setup.git)
            .context("stash bench initial dirty")?;
        Ok(repo)
    })
}

pub fn sh_devnull(program: &Path, args: &[&str]) -> BenchArgv {
    BenchArgv::new("/bin/sh", ["-c", &shell_redirect_devnull(program, args)])
}

pub fn sh_script(script: String) -> BenchArgv {
    BenchArgv::new("/bin/sh", ["-c", script.as_str()])
}

pub fn ctx(repo: PathBuf, setup: &DailySetup<'_>) -> DailyHeadlineContext {
    DailyHeadlineContext::new(repo, setup.git, setup.grit)
}

fn noop_bench() -> (BenchArgv, BenchArgv) {
    let cmd = BenchArgv::new("true", Vec::<&str>::new());
    (cmd.clone(), cmd)
}

pub fn switch_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let Some(meta) = ctx.hot_path_meta.as_ref() else {
        return noop_bench();
    };
    let (grit_argv, git_argv) = switch_argv(&ctx.grit, &ctx.git, meta);
    (
        BenchArgv::new("/bin/sh", grit_argv),
        BenchArgv::new("/bin/sh", git_argv),
    )
}

pub fn pick_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let Some(meta) = ctx.hot_path_meta.as_ref() else {
        return noop_bench();
    };
    let (grit_argv, git_argv) = pick_argv(meta);
    (
        BenchArgv::new(&ctx.grit, grit_argv),
        BenchArgv::new(&ctx.git, git_argv),
    )
}

pub fn merge_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let Some(meta) = ctx.hot_path_meta.as_ref() else {
        return noop_bench();
    };
    let (grit_argv, git_argv) = merge_argv(meta);
    (
        BenchArgv::new(&ctx.grit, grit_argv),
        BenchArgv::new(&ctx.git, git_argv),
    )
}

pub fn stash_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    use crate::scenarios::chain_shell;
    let grit_cmd = chain_shell(
        &ctx.grit,
        &[vec!["stash".into()], vec!["stash".into(), "pop".into()]],
    );
    let git_cmd = chain_shell(
        &ctx.git,
        &[
            vec!["stash".into(), "push".into(), "-q".into()],
            vec!["stash".into(), "pop".into(), "-q".into()],
        ],
    );
    (sh_script(grit_cmd), sh_script(git_cmd))
}

pub fn branch_list_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["branch"]),
        sh_devnull(&ctx.git, &["branch"]),
    )
}

pub fn status_clean_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["status"]),
        sh_devnull(&ctx.git, &["status", "-s"]),
    )
}

pub fn status_dirty_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["status"]),
        sh_devnull(&ctx.git, &["status", "-s"]),
    )
}

pub fn add_modified_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_script(shell_grit_add_modified_set(&ctx.grit)),
        sh_script(shell_git_add_modified_set(&ctx.git)),
    )
}

pub fn commit_small_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["commit", "bench"]),
        sh_script(shell_git_add_modified_set_and_commit(&ctx.git)),
    )
}

pub fn diff_worktree_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["diff"]),
        sh_devnull(&ctx.git, &["diff"]),
    )
}

pub fn log_oneline_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["log", "--limit", "100"]),
        sh_devnull(&ctx.git, &["log", "--oneline", "-100"]),
    )
}

pub fn show_head_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["show"]),
        sh_devnull(&ctx.git, &["show"]),
    )
}

pub fn blame_file_bench(ctx: &DailyHeadlineContext, path: &str) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["blame", path]),
        sh_devnull(&ctx.git, &["blame", "--porcelain", path]),
    )
}

pub fn prepare_bench_argv(
    prepare_bin: &Path,
    git: &Path,
    kind: crate::scenarios::PrepareKind,
) -> BenchArgv {
    use crate::scenarios::PrepareKind;
    let sub = match kind {
        PrepareKind::AddIteration => "prepare-add",
        PrepareKind::SwitchReset => "prepare-switch",
        PrepareKind::PickReset => "prepare-pick",
        PrepareKind::MergeReset => "prepare-merge",
        PrepareKind::CommitIteration => "prepare-commit",
        PrepareKind::StashReset => "prepare-stash",
        PrepareKind::WorktreeStatusClean => "prepare-worktree-status-clean",
        PrepareKind::WorktreeStatusDirty => "prepare-worktree-status-dirty",
        PrepareKind::WorktreeAdd => "prepare-worktree-add",
        PrepareKind::WorktreeCommit => "prepare-worktree-commit",
        PrepareKind::WorktreeDiff => "prepare-worktree-diff",
        _ => "prepare-worktree-status-clean",
    };
    BenchArgv::new(prepare_bin, [sub, "--git", &git.display().to_string()])
}

pub fn headline_spec(id: &str) -> &'static super::registry::HeadlineScenario {
    super::registry::HEADLINE_SCENARIOS
        .iter()
        .find(|s| s.id == id)
        .unwrap_or_else(|| panic!("unknown headline id {id}"))
}
