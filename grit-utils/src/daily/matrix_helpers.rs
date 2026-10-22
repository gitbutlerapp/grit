//! Setup and argv helpers for daily matrix scenarios (branching, refs, maintenance, misc).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use super::helpers::{
    ctx, many_refs_kind, setup_hot_path_pick_merge, setup_registry_worktree, sh_devnull, sh_script,
    synth10k_kind, with_scratch,
};
use super::plan::{DailyHeadlineContext, DailyPrepare, DailySetup};
use super::runner::BenchArgv;
use crate::fixture_registry::FixtureKind;
use crate::history_rewrite_compat::{
    git_reword_primary_is_history, reword_git_rebase_scenario, reword_scenario,
};
use crate::history_rewrite_fixture::{ensure_history_rewrite_work, load_history_rewrite_meta};
use crate::history_rewrite_prepare::{install_history_rewrite_env, BARE_ENV, WORK_ENV};
use crate::hot_path_fixture::{setup_pick_merge_fixture, setup_switch_fixture, HotPathRepoSpec};
use crate::maintenance_suite::{git_env, seed_loose_objects};
use crate::real_repo_fixture::run_git;
use crate::scenarios::{merge_argv, pick_argv, revert_argv, switch_argv};
use crate::shell::shell_command;
use crate::worktree_fixture::{
    ensure_worktree_bench_repo, reset_worktree_clean, set_worktree_bench_cwd,
};

pub const BRANCH_BENCH_NAME: &str = "daily-bench-branch";
pub const BRANCH_RENAME_TARGET: &str = "daily-bench-branch-renamed";
pub const TAG_BENCH_NAME: &str = "daily-bench-tag";
pub const BUNDLE_FILE: &str = "daily-bench.bundle";
pub const LINKED_WT_DIR: &str = "daily-linked-wt";
pub const CONFIG_BENCH_KEY: &str = "daily.bench.key";
pub const CONFIG_BENCH_VALUE: &str = "daily-bench-value";
pub const NOTES_BENCH_MSG: &str = "daily bench note";
pub const PACK_REFS_BENCH_TAG: &str = "daily-bench-pack-tag";

fn default_linked_worktree_path(ctx: &DailyHeadlineContext) -> PathBuf {
    ctx.linked_worktree
        .clone()
        .unwrap_or_else(|| ctx.repo.parent().unwrap_or(&ctx.repo).join(LINKED_WT_DIR))
}

fn hot_path_spec_100k(smoke: bool) -> HotPathRepoSpec {
    if smoke {
        HotPathRepoSpec::for_size(5_000)
    } else {
        HotPathRepoSpec::heavy()
    }
}

pub fn setup_many_refs(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    let kind = many_refs_kind(setup.smoke);
    let (repo, _record) = ensure_worktree_bench_repo(setup.git, kind, setup.offline)?;
    set_worktree_bench_cwd(&repo)?;
    Ok(ctx(repo, setup))
}

pub fn setup_many_refs_loose(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    let kind = if setup.smoke {
        FixtureKind::ManyRefsLooseSmoke
    } else {
        FixtureKind::ManyRefsLoose
    };
    let (repo, _record) = ensure_worktree_bench_repo(setup.git, kind, setup.offline)?;
    set_worktree_bench_cwd(&repo)?;
    Ok(ctx(repo, setup))
}

pub fn setup_synth10k(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    let repo = setup_registry_worktree(setup, synth10k_kind(setup.smoke))?;
    set_worktree_bench_cwd(&repo)?;
    Ok(ctx(repo, setup))
}

pub fn setup_small(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    let kind = if setup.smoke {
        FixtureKind::SmallSmoke
    } else {
        FixtureKind::Small
    };
    let repo = setup_registry_worktree(setup, kind)?;
    set_worktree_bench_cwd(&repo)?;
    Ok(ctx(repo, setup))
}

pub fn setup_git_git(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    let repo = setup_registry_worktree(setup, FixtureKind::GitGitWt)?;
    set_worktree_bench_cwd(&repo)?;
    Ok(ctx(repo, setup))
}

/// Git.git worktree for production; synth-10k smoke substitute for heavy repack scenarios.
pub fn setup_git_git_repack_smoke(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    if setup.smoke {
        setup_synth10k(setup)
    } else {
        setup_git_git(setup)
    }
}

pub fn setup_big_blobs(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    let kind = if setup.smoke {
        FixtureKind::BigBlobsSmoke
    } else {
        FixtureKind::BigBlobs
    };
    let repo = setup_registry_worktree(setup, kind)?;
    set_worktree_bench_cwd(&repo)?;
    Ok(ctx(repo, setup))
}

pub fn setup_hot_path_switch_100k(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    with_scratch("switch-100k", |_scratch| {
        let spec = hot_path_spec_100k(setup.smoke);
        let (repo, meta) = setup_switch_fixture(setup.git, spec, false)?;
        let mut headline = ctx(repo, setup);
        headline.hot_path_meta = Some(meta);
        Ok(headline)
    })
}

pub fn setup_hot_path_pick_merge_100k(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    with_scratch("pick-merge-100k", |_scratch| {
        let spec = hot_path_spec_100k(setup.smoke);
        let touch = crate::hot_path_fixture::touch_paths_for_size(spec.file_count);
        let (_repo, meta) = setup_pick_merge_fixture(setup.git, spec, touch)?;
        let mut headline = ctx(crate::fixture::scratch_dir(), setup);
        headline.hot_path_meta = Some(meta);
        Ok(headline)
    })
}

pub fn setup_hot_path_revert(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    setup_hot_path_pick_merge(setup)
}

pub fn setup_history_reword(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    with_scratch("history-reword", |_scratch| {
        let (bare, work) = install_history_rewrite_env(setup.git, setup.smoke)?;
        let repo = ensure_history_rewrite_work(setup.git, setup.smoke)?;
        set_worktree_bench_cwd(&repo)?;
        let meta = load_history_rewrite_meta(&bare)?;
        let mut ctx = ctx(repo, setup);
        ctx.history_meta = Some(meta);
        std::env::set_var(BARE_ENV, bare.display().to_string());
        std::env::set_var(WORK_ENV, work.display().to_string());
        Ok(ctx)
    })
}

pub fn setup_stash_with_entry(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    setup_synth10k(setup)
}

pub fn setup_synth10k_gc(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    let ctx = setup_synth10k(setup)?;
    seed_loose_objects(setup.git, &ctx.repo)?;
    run_git(
        setup.git,
        &ctx.repo,
        &["tag", "-f", PACK_REFS_BENCH_TAG, "HEAD"],
    )?;
    Ok(ctx)
}

pub fn setup_synth10k_bundle(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    let mut ctx = setup_synth10k(setup)?;
    ctx.bundle_path = Some(ctx.repo.join(BUNDLE_FILE));
    Ok(ctx)
}

pub fn setup_synth10k_linked_worktree(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    let mut ctx = setup_synth10k(setup)?;
    ctx.linked_worktree = Some(ctx.repo.parent().unwrap_or(&ctx.repo).join(LINKED_WT_DIR));
    Ok(ctx)
}

pub fn setup_branch_create(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    let mut ctx = setup_many_refs(setup)?;
    ctx.branch_bench_name = Some(BRANCH_BENCH_NAME.into());
    Ok(ctx)
}

pub fn setup_branch_delete(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    let mut ctx = setup_many_refs(setup)?;
    ctx.branch_bench_name = Some(BRANCH_BENCH_NAME.into());
    run_git(setup.git, &ctx.repo, &["branch", BRANCH_BENCH_NAME, "HEAD"])?;
    Ok(ctx)
}

pub fn setup_branch_rename(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    setup_branch_delete(setup)
}

pub fn setup_tag_create(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    let mut ctx = setup_many_refs(setup)?;
    ctx.tag_bench_name = Some(TAG_BENCH_NAME.into());
    Ok(ctx)
}

fn shell_quote_path(path: &Path) -> String {
    shell_words::quote(path.to_string_lossy().as_ref()).into_owned()
}

pub fn prepare_daily_reset_script(git: &Path) -> String {
    format!(
        "set -eu\n{}",
        shell_command(
            &std::env::current_exe().unwrap_or_else(|_| PathBuf::from("grit-bench")),
            &["prepare-daily-reset", "--git", &git.display().to_string(),],
        )
    )
}

pub fn prepare_reset_shell(git: &Path) -> BenchArgv {
    BenchArgv::new("/bin/sh", ["-c", prepare_daily_reset_script(git).as_str()])
}

pub fn prepare_reset_delete_branch(git: &Path, branch: &str) -> BenchArgv {
    let reset = prepare_daily_reset_script(git);
    let script = format!(
        "{reset}\n{} 2>/dev/null || true",
        shell_command(git, &["branch", "-D", branch])
    );
    BenchArgv::new("/bin/sh", ["-c", script.as_str()])
}

pub fn prepare_reset_delete_tag(git: &Path, tag: &str) -> BenchArgv {
    let reset = prepare_daily_reset_script(git);
    let script = format!(
        "{reset}\n{} 2>/dev/null || true",
        shell_command(git, &["tag", "-d", tag])
    );
    BenchArgv::new("/bin/sh", ["-c", script.as_str()])
}

pub fn prepare_reset_recreate_branch(git: &Path, branch: &str) -> BenchArgv {
    let reset = prepare_daily_reset_script(git);
    let script = format!(
        "{reset}\n{} 2>/dev/null || true\n{}",
        shell_command(git, &["branch", "-D", branch]),
        shell_command(git, &["branch", branch, "HEAD"])
    );
    BenchArgv::new("/bin/sh", ["-c", script.as_str()])
}

pub fn prepare_reset_unset_config(git: &Path, key: &str) -> BenchArgv {
    let reset = prepare_daily_reset_script(git);
    let script = format!(
        "{reset}\n{} 2>/dev/null || true",
        shell_command(git, &["config", "--unset", key])
    );
    BenchArgv::new("/bin/sh", ["-c", script.as_str()])
}

pub fn prepare_reset_seed_gc(git: &Path) -> BenchArgv {
    let reset = prepare_daily_reset_script(git);
    let seed = shell_command(
        &std::env::current_exe().unwrap_or_else(|_| PathBuf::from("grit-bench")),
        &["prepare-daily-gc-seed", "--git", &git.display().to_string()],
    );
    BenchArgv::new("/bin/sh", ["-c", format!("{reset}\n{seed}").as_str()])
}

pub fn prepare_reset_linked_worktree(git: &Path, linked: &Path) -> BenchArgv {
    let reset = prepare_daily_reset_script(git);
    let repo = crate::worktree_fixture::active_worktree_repo();
    let remove = shell_command(
        git,
        &[
            "worktree",
            "remove",
            "--force",
            &linked.display().to_string(),
        ],
    );
    let script = format!(
        "{reset}\ncd {} && {} 2>/dev/null || true\nrm -rf {}",
        shell_quote_path(&repo),
        remove,
        shell_quote_path(linked)
    );
    BenchArgv::new("/bin/sh", ["-c", script.as_str()])
}

pub fn prepare_reset_bundle(git: &Path, bundle: &Path) -> BenchArgv {
    let reset = prepare_daily_reset_script(git);
    let script = format!("{reset}\nrm -f {}", shell_quote_path(bundle));
    BenchArgv::new("/bin/sh", ["-c", script.as_str()])
}

pub fn prepare_reset_notes(git: &Path) -> BenchArgv {
    let reset = prepare_daily_reset_script(git);
    let script = format!(
        "{reset}\n{} 2>/dev/null || true",
        shell_command(git, &["notes", "--ref", "refs/notes/commits", "drop"],)
    );
    BenchArgv::new("/bin/sh", ["-c", script.as_str()])
}

pub fn prepare_reset_create_bundle(git: &Path, bundle: &Path) -> BenchArgv {
    let repo = crate::worktree_fixture::active_worktree_repo();
    let create = shell_command(
        git,
        &["bundle", "create", &bundle.display().to_string(), "HEAD"],
    );
    let inner = format!(
        "{}\ncd {} && {}",
        prepare_daily_reset_script(git),
        shell_quote_path(&repo),
        create
    );
    BenchArgv::new("/bin/sh", ["-c", inner.as_str()])
}

pub fn run_prepare_daily_reset(git: &Path) -> Result<()> {
    let repo = crate::worktree_fixture::active_worktree_repo();
    reset_worktree_clean(git, &repo)
}

pub fn run_prepare_daily_gc_seed(git: &Path) -> Result<()> {
    let repo = crate::worktree_fixture::active_worktree_repo();
    reset_worktree_clean(git, &repo)?;
    seed_loose_objects(git, &repo)?;
    run_git(git, &repo, &["tag", "-f", PACK_REFS_BENCH_TAG, "HEAD"])?;
    Ok(())
}

pub fn switch_100k_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    switch_bench_from_meta(ctx)
}

pub fn merge_100k_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    merge_bench_from_meta(ctx)
}

pub fn pick_100k_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    pick_bench_from_meta(ctx)
}

fn switch_bench_from_meta(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let Some(meta) = ctx.hot_path_meta.as_ref() else {
        return noop_bench();
    };
    let (grit_argv, git_argv) = switch_argv(&ctx.grit, &ctx.git, meta);
    (
        BenchArgv::new("/bin/sh", grit_argv),
        BenchArgv::new("/bin/sh", git_argv),
    )
}

fn merge_bench_from_meta(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let Some(meta) = ctx.hot_path_meta.as_ref() else {
        return noop_bench();
    };
    let (grit_argv, git_argv) = merge_argv(meta);
    (
        BenchArgv::new(&ctx.grit, grit_argv),
        BenchArgv::new(&ctx.git, git_argv),
    )
}

fn pick_bench_from_meta(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let Some(meta) = ctx.hot_path_meta.as_ref() else {
        return noop_bench();
    };
    let (grit_argv, git_argv) = pick_argv(meta);
    (
        BenchArgv::new(&ctx.grit, grit_argv),
        BenchArgv::new(&ctx.git, git_argv),
    )
}

fn noop_bench() -> (BenchArgv, BenchArgv) {
    let cmd = BenchArgv::new("true", Vec::<String>::new());
    (cmd.clone(), cmd)
}

pub fn revert_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let Some(meta) = ctx.hot_path_meta.as_ref() else {
        return noop_bench();
    };
    let (grit_argv, git_argv) = revert_argv(meta);
    (
        BenchArgv::new(&ctx.grit, grit_argv),
        BenchArgv::new(&ctx.git, git_argv),
    )
}

pub fn branch_list_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["branch"]),
        sh_devnull(&ctx.git, &["branch"]),
    )
}

pub fn branch_create_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let name = ctx
        .branch_bench_name
        .as_deref()
        .unwrap_or(BRANCH_BENCH_NAME);
    (
        sh_devnull(&ctx.grit, &["branch", name]),
        sh_devnull(&ctx.git, &["branch", name]),
    )
}

pub fn branch_delete_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let name = ctx
        .branch_bench_name
        .as_deref()
        .unwrap_or(BRANCH_BENCH_NAME);
    (
        sh_devnull(&ctx.grit, &["branch", "-D", name]),
        sh_devnull(&ctx.git, &["branch", "-D", name]),
    )
}

pub fn branch_rename_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let old = ctx
        .branch_bench_name
        .as_deref()
        .unwrap_or(BRANCH_BENCH_NAME);
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("grit-bench"));
    let repo = ctx.repo.to_string_lossy().into_owned();
    (
        BenchArgv::new(
            &exe,
            [
                "rename-branch-lib",
                "--repo",
                repo.as_str(),
                "--old",
                old,
                "--new",
                BRANCH_RENAME_TARGET,
            ],
        ),
        sh_devnull(&ctx.git, &["branch", "-m", old, BRANCH_RENAME_TARGET]),
    )
}

pub fn tag_list_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["tag"]),
        sh_devnull(&ctx.git, &["tag", "-l"]),
    )
}

pub fn tag_create_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let name = ctx.tag_bench_name.as_deref().unwrap_or(TAG_BENCH_NAME);
    (
        sh_devnull(&ctx.grit, &["tag", name]),
        sh_devnull(&ctx.git, &["tag", name]),
    )
}

pub fn history_reword_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let Some(meta) = ctx.history_meta.as_ref() else {
        return noop_bench();
    };
    let git_history = git_reword_primary_is_history(&ctx.git);
    let (grit_argv, git_argv) = reword_scenario(&ctx.grit, &ctx.git, meta, false);
    let git_side = if git_history {
        git_argv
    } else {
        reword_git_rebase_scenario(&ctx.repo)
    };
    (
        BenchArgv::new("/bin/sh", grit_argv),
        BenchArgv::new("/bin/sh", git_side),
    )
}

pub fn stash_list_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["stash", "list"]),
        sh_devnull(&ctx.git, &["stash", "list"]),
    )
}

pub fn gc_pack_refs_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["gc", "pack-refs"]),
        sh_devnull(&ctx.git, &["pack-refs"]),
    )
}

pub fn gc_default_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["gc"]),
        sh_devnull(&ctx.git, &["gc", "--quiet"]),
    )
}

pub fn repack_full_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["gc", "repack"]),
        sh_devnull(&ctx.git, &["repack", "-ad"]),
    )
}

pub fn fsck_full_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["fsck"]),
        sh_devnull(&ctx.git, &["fsck"]),
    )
}

pub fn repack_commit_graph_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["gc", "commit-graph"]),
        sh_devnull(
            &ctx.git,
            &["commit-graph", "write", "--reachable", "--changed-paths"],
        ),
    )
}

pub fn config_get_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["config", "core.repositoryformatversion"]),
        sh_devnull(&ctx.git, &["config", "core.repositoryformatversion"]),
    )
}

pub fn config_set_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["config", CONFIG_BENCH_KEY, CONFIG_BENCH_VALUE]),
        sh_devnull(&ctx.git, &["config", CONFIG_BENCH_KEY, CONFIG_BENCH_VALUE]),
    )
}

pub fn ignore_check_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let list_path = ctx.repo.join("bench-ignore-paths.txt");
    if !list_path.is_file() {
        let _ = write_ignore_paths(&ctx.repo, 1_000);
    }
    let git_inner = shell_command(&ctx.git, &["check-ignore", "-q", "--stdin"]);
    let git_cmd = format!("{} < {}", git_inner, shell_quote_path(&list_path));
    let grit_q = shell_quote_path(&ctx.grit);
    let repo_q = shell_quote_path(&ctx.repo);
    let list_q = shell_quote_path(&list_path);
    let grit_cmd = format!(
        "set -eu\ncd {repo_q} && xargs -a {list_q} {grit_q} ignore check --all > /dev/null 2>&1"
    );
    (
        sh_script(grit_cmd),
        sh_script(format!("cd {repo_q} && {git_cmd}")),
    )
}

fn write_ignore_paths(repo: &Path, count: usize) -> Result<()> {
    use std::io::Write;
    let list_path = repo.join("bench-ignore-paths.txt");
    if list_path.is_file() {
        return Ok(());
    }
    let mut f = std::fs::File::create(&list_path).context("create ignore path list")?;
    for i in 0..count {
        writeln!(f, "d{i:04}/f{i:04}.txt")?;
    }
    let _ = std::fs::write(repo.join(".gitignore"), "*.txt\n");
    Ok(())
}

pub fn hooks_list_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let hooks_path = shell_command(&ctx.git, &["rev-parse", "--git-path", "hooks"]);
    let git_ls = format!(
        "set -eu\ncd {} && hooks_dir=$({hooks_path}) && ls \"$hooks_dir\"/ >/dev/null 2>&1",
        shell_quote_path(&ctx.repo),
        hooks_path = hooks_path
    );
    (sh_devnull(&ctx.grit, &["hooks"]), sh_script(git_ls))
}

pub fn notes_add_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(
            &ctx.grit,
            &[
                "notes",
                "add",
                "--force",
                "--message",
                NOTES_BENCH_MSG,
                "HEAD",
            ],
        ),
        sh_devnull(
            &ctx.git,
            &["notes", "add", "-f", "-m", NOTES_BENCH_MSG, "HEAD"],
        ),
    )
}

pub fn notes_list_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["notes"]),
        sh_devnull(&ctx.git, &["notes", "list"]),
    )
}

pub fn bundle_create_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let bundle = ctx
        .bundle_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| BUNDLE_FILE.into());
    (
        sh_devnull(&ctx.grit, &["bundle", "create", &bundle, "HEAD"]),
        sh_devnull(&ctx.git, &["bundle", "create", &bundle, "HEAD"]),
    )
}

pub fn bundle_verify_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let bundle = ctx
        .bundle_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| BUNDLE_FILE.into());
    (
        sh_devnull(&ctx.grit, &["bundle", "verify", &bundle]),
        sh_devnull(&ctx.git, &["bundle", "verify", &bundle]),
    )
}

pub fn worktree_add_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let path = default_linked_worktree_path(ctx).display().to_string();
    (
        sh_devnull(&ctx.grit, &["worktree", "add", &path]),
        sh_devnull(&ctx.git, &["worktree", "add", &path, "HEAD"]),
    )
}

pub fn worktree_remove_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let path = default_linked_worktree_path(ctx).display().to_string();
    (
        sh_devnull(&ctx.grit, &["worktree", "remove", "--force", &path]),
        sh_devnull(&ctx.git, &["worktree", "remove", "--force", &path]),
    )
}

pub fn remote_list_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["remote"]),
        sh_devnull(&ctx.git, &["remote"]),
    )
}

pub fn verify_bundle(ctx: &DailyHeadlineContext) -> Result<()> {
    let bundle = ctx.bundle_path.as_ref().context("bundle path")?;
    if !bundle.is_file() {
        let rel = bundle
            .file_name()
            .and_then(|n| n.to_str())
            .map(String::from)
            .unwrap_or_else(|| BUNDLE_FILE.into());
        run_git(
            &ctx.git,
            &ctx.repo,
            &["bundle", "create", rel.as_str(), "HEAD"],
        )?;
    }
    let mut cmd = Command::new(&ctx.git);
    cmd.args(["bundle", "verify", &bundle.display().to_string()])
        .current_dir(&ctx.repo);
    git_env(&mut cmd);
    let out = cmd.output().context("git bundle verify")?;
    if !out.status.success() {
        anyhow::bail!(
            "git bundle verify failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

pub fn verify_git_fsck(ctx: &DailyHeadlineContext) -> Result<()> {
    let mut cmd = Command::new(&ctx.git);
    cmd.args(["fsck"]).current_dir(&ctx.repo);
    git_env(&mut cmd);
    let out = cmd.output().context("git fsck")?;
    if !out.status.success() {
        anyhow::bail!("git fsck failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}

/// On-disk path for the canonical commit-graph file (`.git/objects/info/commit-graph`).
#[must_use]
pub fn commit_graph_path(repo: &Path) -> PathBuf {
    repo.join(".git/objects/info/commit-graph")
}

/// Whether a non-empty commit-graph file exists (omission-sensitive preflight check).
#[must_use]
pub fn commit_graph_present(repo: &Path) -> bool {
    std::fs::metadata(commit_graph_path(repo)).is_ok_and(|meta| meta.is_file() && meta.len() > 0)
}

/// Write a reachable commit-graph with system `git commit-graph write`.
pub fn write_git_commit_graph_reachable(git: &Path, repo: &Path) -> Result<()> {
    let mut cmd = Command::new(git);
    cmd.args(["commit-graph", "write", "--reachable"])
        .current_dir(repo);
    git_env(&mut cmd);
    let out = cmd.output().context("git commit-graph write")?;
    if !out.status.success() {
        anyhow::bail!(
            "git commit-graph write failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    anyhow::ensure!(
        commit_graph_present(repo),
        "git commit-graph write did not create objects/info/commit-graph"
    );
    Ok(())
}

/// Validate the commit-graph chain with system `git commit-graph verify`.
pub fn verify_git_commit_graph_in_repo(git: &Path, repo: &Path) -> Result<()> {
    let mut cmd = Command::new(git);
    cmd.args(["commit-graph", "verify"]).current_dir(repo);
    git_env(&mut cmd);
    let out = cmd.output().context("git commit-graph verify")?;
    if !out.status.success() {
        anyhow::bail!(
            "git commit-graph verify failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

/// Validate the commit-graph chain with system `git commit-graph verify`.
pub fn verify_git_commit_graph(ctx: &DailyHeadlineContext) -> Result<()> {
    verify_git_commit_graph_in_repo(&ctx.git, &ctx.repo)
}

/// Fail when a commit-graph file is present (no-graph benchmark/daily preflight).
pub fn ensure_commit_graph_absent(repo: &Path) -> Result<()> {
    anyhow::ensure!(
        !commit_graph_present(repo),
        "expected no commit-graph at {}",
        commit_graph_path(repo).display()
    );
    Ok(())
}

/// Run one grit subcommand in the scenario repo (for preflight correctness checks).
pub fn run_grit_in_repo(ctx: &DailyHeadlineContext, args: &[&str]) -> Result<()> {
    let mut cmd = Command::new(&ctx.grit);
    cmd.args(args).current_dir(&ctx.repo);
    let out = cmd
        .output()
        .with_context(|| format!("grit {}", args.join(" ")))?;
    if !out.status.success() {
        anyhow::bail!(
            "grit {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

/// Reset fixture state, run grit maintenance, then validate output with system git fsck.
pub fn preflight_grit_gc_then_fsck(ctx: &DailyHeadlineContext, grit_args: &[&str]) -> Result<()> {
    run_prepare_daily_reset(&ctx.git)?;
    run_grit_in_repo(ctx, grit_args)?;
    verify_git_fsck(ctx)
}

/// Preflight `grit gc repack` output before timing `repack/full@git-git`.
pub fn correctness_grit_repack_full(ctx: &DailyHeadlineContext) -> Result<()> {
    preflight_grit_gc_then_fsck(ctx, &["gc", "repack"])
}

/// Preflight `grit gc commit-graph` output before timing `repack/commit-graph@git-git`.
pub fn correctness_grit_commit_graph(ctx: &DailyHeadlineContext) -> Result<()> {
    run_prepare_daily_reset(&ctx.git)?;
    run_grit_in_repo(ctx, &["gc", "commit-graph"])?;
    anyhow::ensure!(
        commit_graph_present(&ctx.repo),
        "grit gc commit-graph did not write objects/info/commit-graph"
    );
    verify_git_commit_graph(ctx)?;
    verify_git_fsck(ctx)
}

/// Count loose object files under `.git/objects` (excluding `pack/` and `info/`).
pub fn count_loose_objects(repo: &Path) -> Result<usize> {
    use std::fs;
    let objects = repo.join(".git/objects");
    let mut count = 0usize;
    for entry in fs::read_dir(&objects).context("read objects dir")? {
        let entry = entry.context("objects entry")?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "pack" || name == "info" {
            continue;
        }
        if name.len() != 2 {
            continue;
        }
        for leaf in fs::read_dir(entry.path()).context("read shard dir")? {
            let leaf = leaf.context("shard entry")?;
            if leaf.file_type().is_ok_and(|t| t.is_file()) {
                count += 1;
            }
        }
    }
    Ok(count)
}

pub fn setup_ignore_check(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    let ctx = setup_synth10k(setup)?;
    let out = Command::new(setup.git)
        .args(["rm", "-r", "--cached", "."])
        .current_dir(&ctx.repo)
        .output()
        .context("git rm --cached for ignore bench")?;
    if !out.status.success() {
        anyhow::bail!("git rm --cached failed");
    }
    write_ignore_paths(&ctx.repo, 1_000)?;
    Ok(ctx)
}

pub fn setup_notes_list(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    let ctx = setup_synth10k(setup)?;
    run_git(
        setup.git,
        &ctx.repo,
        &["notes", "add", "-m", NOTES_BENCH_MSG, "HEAD"],
    )?;
    Ok(ctx)
}

pub fn setup_worktree_remove(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    let ctx = setup_synth10k_linked_worktree(setup)?;
    let linked = default_linked_worktree_path(&ctx);
    if linked.exists() {
        crate::fixture::remove_dir_robust(&linked);
    }
    run_git(
        setup.git,
        &ctx.repo,
        &[
            "worktree",
            "add",
            linked.to_str().context("linked utf-8")?,
            "HEAD",
        ],
    )?;
    Ok(ctx)
}

pub fn prepare_branch_create(ctx: &DailyHeadlineContext) -> DailyPrepare {
    DailyPrepare::Shell(prepare_reset_delete_branch(
        &ctx.git,
        ctx.branch_bench_name
            .as_deref()
            .unwrap_or(BRANCH_BENCH_NAME),
    ))
}

pub fn prepare_branch_delete(ctx: &DailyHeadlineContext) -> DailyPrepare {
    DailyPrepare::Shell(prepare_reset_recreate_branch(
        &ctx.git,
        ctx.branch_bench_name
            .as_deref()
            .unwrap_or(BRANCH_BENCH_NAME),
    ))
}

pub fn prepare_branch_rename(ctx: &DailyHeadlineContext) -> DailyPrepare {
    DailyPrepare::Shell(prepare_reset_branch_rename(
        &ctx.git,
        ctx.branch_bench_name
            .as_deref()
            .unwrap_or(BRANCH_BENCH_NAME),
        BRANCH_RENAME_TARGET,
    ))
}

pub fn prepare_reset_branch_rename(git: &Path, old: &str, new: &str) -> BenchArgv {
    let reset = prepare_daily_reset_script(git);
    let script = format!(
        "{reset}\n{} 2>/dev/null || true\n{}",
        shell_command(git, &["branch", "-D", new]),
        shell_command(git, &["branch", old, "HEAD"])
    );
    BenchArgv::new("/bin/sh", ["-c", script.as_str()])
}

pub fn prepare_tag_create(ctx: &DailyHeadlineContext) -> DailyPrepare {
    DailyPrepare::Shell(prepare_reset_delete_tag(
        &ctx.git,
        ctx.tag_bench_name.as_deref().unwrap_or(TAG_BENCH_NAME),
    ))
}

pub fn prepare_gc_seed(ctx: &DailyHeadlineContext) -> DailyPrepare {
    DailyPrepare::Shell(prepare_reset_seed_gc(&ctx.git))
}

pub fn prepare_config_set(ctx: &DailyHeadlineContext) -> DailyPrepare {
    DailyPrepare::Shell(prepare_reset_unset_config(&ctx.git, CONFIG_BENCH_KEY))
}

pub fn prepare_notes_add(ctx: &DailyHeadlineContext) -> DailyPrepare {
    DailyPrepare::Shell(prepare_reset_notes(&ctx.git))
}

pub fn prepare_bundle_create(ctx: &DailyHeadlineContext) -> DailyPrepare {
    let bundle = ctx
        .bundle_path
        .as_ref()
        .cloned()
        .unwrap_or_else(|| ctx.repo.join(BUNDLE_FILE));
    DailyPrepare::Shell(prepare_reset_bundle(&ctx.git, &bundle))
}

pub fn prepare_bundle_verify(ctx: &DailyHeadlineContext) -> DailyPrepare {
    let bundle = ctx
        .bundle_path
        .as_ref()
        .cloned()
        .unwrap_or_else(|| ctx.repo.join(BUNDLE_FILE));
    DailyPrepare::Shell(prepare_reset_create_bundle(&ctx.git, &bundle))
}

pub fn prepare_worktree_add(ctx: &DailyHeadlineContext) -> DailyPrepare {
    let linked = default_linked_worktree_path(ctx);
    DailyPrepare::Shell(prepare_reset_linked_worktree(&ctx.git, &linked))
}

pub fn prepare_worktree_remove(ctx: &DailyHeadlineContext) -> DailyPrepare {
    let linked = default_linked_worktree_path(ctx);
    let reset = prepare_daily_reset_script(&ctx.git);
    let repo_q = shell_quote_path(&ctx.repo);
    let linked_q = shell_quote_path(&linked);
    let remove = shell_command(
        &ctx.git,
        &[
            "worktree",
            "remove",
            "--force",
            &linked.display().to_string(),
        ],
    );
    let add = shell_command(
        &ctx.git,
        &["worktree", "add", &linked.display().to_string(), "HEAD"],
    );
    let script = format!(
        "{reset}\ncd {repo_q} && ({remove} 2>/dev/null || true) && rm -rf {linked_q} && {add}"
    );
    DailyPrepare::Shell(BenchArgv::new("/bin/sh", ["-c", script.as_str()]))
}

pub fn prepare_daily_reset(ctx: &DailyHeadlineContext) -> DailyPrepare {
    DailyPrepare::Shell(prepare_reset_shell(&ctx.git))
}

pub fn prepare_stash_list(ctx: &DailyHeadlineContext) -> DailyPrepare {
    let reset = prepare_daily_reset_script(&ctx.git);
    let push = shell_command(
        &ctx.git,
        &["stash", "push", "-u", "-m", "daily-bench-stash"],
    );
    let repo_q = shell_quote_path(&ctx.repo);
    let script = format!(
        "set -eu\n{reset}\ncd {repo_q} && echo daily-bench >> daily-stash-touch.txt && {push}",
        push = push
    );
    DailyPrepare::Shell(BenchArgv::new("/bin/sh", ["-c", script.as_str()]))
}

pub fn prepare_stash_list_seed(ctx: &DailyHeadlineContext) -> Result<()> {
    run_prepare_daily_reset(&ctx.git)?;
    let touch = ctx.repo.join("daily-stash-touch.txt");
    std::fs::write(&touch, "daily-bench\n").context("stash list seed touch file")?;
    run_git(
        &ctx.git,
        &ctx.repo,
        &["stash", "push", "-u", "-m", "daily-bench-stash"],
    )?;
    Ok(())
}

pub fn correctness_bundle_create(ctx: &DailyHeadlineContext) -> Result<()> {
    let bundle = ctx.bundle_path.as_ref().context("bundle path")?;
    if bundle.exists() {
        std::fs::remove_file(bundle).ok();
    }
    let mut cmd = Command::new(&ctx.grit);
    cmd.args(["bundle", "create", &bundle.display().to_string(), "HEAD"])
        .current_dir(&ctx.repo);
    let out = cmd.output().context("grit bundle create for verify")?;
    if !out.status.success() {
        anyhow::bail!(
            "grit bundle create failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    verify_bundle(ctx)
}

pub fn setup_remote_list(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    let ctx = setup_small(setup)?;
    let _ = run_git(setup.git, &ctx.repo, &["remote", "remove", "origin"]);
    run_git(
        setup.git,
        &ctx.repo,
        &["remote", "add", "origin", "file:///dev/null"],
    )?;
    Ok(ctx)
}
