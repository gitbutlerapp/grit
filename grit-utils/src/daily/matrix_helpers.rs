//! Setup and argv helpers for daily matrix scenarios (branching, refs, maintenance, misc).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use super::helpers::{
    ctx, many_refs_kind, setup_hot_path_pick_merge, setup_registry_worktree, sh_devnull, sh_script,
    with_scratch,
};
use super::plan::{DailyHeadlineContext, DailyPrepare, DailySetup};
use super::runner::BenchArgv;
use crate::fixture_registry::FixtureKind;
use crate::history_rewrite_compat::{
    git_reword_primary_is_history, rebase_onto_scenario, reword_git_rebase_scenario,
    reword_scenario,
};
use crate::history_rewrite_fixture::{
    ensure_history_rewrite_work, load_history_rewrite_meta, HistoryRewriteMeta,
};
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
pub const REMOTE_RENAME_TARGET: &str = "upstream";
pub const TAG_BENCH_NAME: &str = "daily-bench-tag";
pub const BUNDLE_FILE: &str = "daily-bench.bundle";
pub const LINKED_WT_DIR: &str = "daily-linked-wt";
pub const WORKTREE_LIST_COUNT: usize = 20;
pub const CONFIG_BENCH_GET_KEY: &str = "core.repositoryformatversion";
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
    super::helpers::setup_shape_worktree(setup, super::registry::RepoShape::Synth10k)
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

/// Packed deep history (git.git) for `bundle create --all`.
pub fn setup_git_git_bundle(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    let mut ctx = setup_git_git(setup)?;
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

/// Tracked directories and files per directory in the delete-heavy wide fixture
/// (production / smoke), and how many leading directories `bench-b` deletes.
fn delete_heavy_wide_shape(smoke: bool) -> (usize, usize, usize) {
    if smoke {
        (48, 100, 2)
    } else {
        (480, 100, 20)
    }
}

/// Wide tree (480 directories of 100 small files) where `bench-b` deletes the first 20
/// top-level directories, the shape from issue #1754. `bench-a` keeps every file.
pub fn setup_delete_heavy_wide(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    use crate::hot_path_fixture::{meta_path, populate_tree, rev_parse, HotPathMeta};

    with_scratch("delete-heavy-wide", |_scratch| {
        let (dirs, files_per_dir, deleted) = delete_heavy_wide_shape(setup.smoke);
        let repo = crate::fixture::scratch_dir();
        crate::fixture::remove_dir_robust(&repo);
        std::fs::create_dir_all(&repo)?;
        run_git(setup.git, &repo, &["init", "-q"])?;
        // A detached auto-gc after the big commit would repack under the timed runs.
        run_git(setup.git, &repo, &["config", "gc.auto", "0"])?;
        populate_tree(&repo, dirs * files_per_dir, dirs)?;
        run_git(setup.git, &repo, &["add", "-A"])?;
        run_git(setup.git, &repo, &["commit", "-q", "-m", "wide tree"])?;
        run_git(setup.git, &repo, &["branch", "-q", "bench-a"])?;
        run_git(setup.git, &repo, &["checkout", "-q", "-b", "bench-b"])?;
        let doomed: Vec<String> = (0..deleted).map(|d| format!("d{d:04}")).collect();
        let mut rm = vec!["rm", "-rq"];
        rm.extend(doomed.iter().map(String::as_str));
        run_git(setup.git, &repo, &rm)?;
        run_git(
            setup.git,
            &repo,
            &["commit", "-q", "-m", "delete leading dirs"],
        )?;
        run_git(setup.git, &repo, &["checkout", "-q", "bench-a"])?;
        run_git(setup.git, &repo, &["gc", "-q"])?;
        let base_commit = rev_parse(setup.git, &repo, "bench-a")?;
        let meta = HotPathMeta {
            branch_a: "bench-a".into(),
            branch_b: "bench-b".into(),
            main_branch: "bench-a".into(),
            topic_branch: "bench-b".into(),
            base_commit: base_commit.clone(),
            main_tip: base_commit,
            pick_commit: String::new(),
            pick_series_commits: Vec::new(),
            wide: true,
            wide_branch_b: None,
        };
        std::fs::write(meta_path(&repo), serde_json::to_string_pretty(&meta)?)?;
        let mut headline = ctx(repo, setup);
        headline.hot_path_meta = Some(meta);
        Ok(headline)
    })
}

/// Directories and files per directory in the both-sides content-merge fixture
/// (production / smoke).
fn content_merge_wide_shape(smoke: bool) -> (usize, usize) {
    if smoke {
        (20, 100)
    } else {
        (200, 100)
    }
}

/// Rewrite every tracked file under `repo` (as listed in `files`) through `edit`.
fn edit_every_file(repo: &Path, files: &[PathBuf], edit: impl Fn(&str) -> String) -> Result<()> {
    for rel in files {
        let path = repo.join(rel);
        let text = std::fs::read_to_string(&path)?;
        std::fs::write(&path, edit(&text))?;
    }
    Ok(())
}

/// Wide tree (200 directories of 100 twenty-line files) where `topic` appends a line to every
/// file and `main` prepends one, so merging `topic` content-merges every path cleanly: the
/// shape from issue #1881.
pub fn setup_content_merge_wide(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    use crate::hot_path_fixture::{meta_path, rev_parse, HotPathMeta};

    with_scratch("content-merge-wide", |_scratch| {
        let (dirs, files_per_dir) = content_merge_wide_shape(setup.smoke);
        let repo = crate::fixture::scratch_dir();
        crate::fixture::remove_dir_robust(&repo);
        std::fs::create_dir_all(&repo)?;
        run_git(setup.git, &repo, &["init", "-q", "-b", "main"])?;
        // A detached auto-gc after the big commits would repack under the timed runs.
        run_git(setup.git, &repo, &["config", "gc.auto", "0"])?;
        let mut files = Vec::with_capacity(dirs * files_per_dir);
        for d in 0..dirs {
            let dir = PathBuf::from(format!("d{d:04}"));
            std::fs::create_dir_all(repo.join(&dir))?;
            for f in 0..files_per_dir {
                let rel = dir.join(format!("f{f:04}.txt"));
                let body: String = (0..20).map(|j| format!("line {j} of {d}/{f}\n")).collect();
                std::fs::write(repo.join(&rel), body)?;
                files.push(rel);
            }
        }
        run_git(setup.git, &repo, &["add", "-A"])?;
        run_git(setup.git, &repo, &["commit", "-q", "-m", "wide tree"])?;
        let base_commit = rev_parse(setup.git, &repo, "HEAD")?;
        run_git(setup.git, &repo, &["checkout", "-q", "-b", "topic"])?;
        edit_every_file(&repo, &files, |text| format!("{text}topic tail\n"))?;
        run_git(setup.git, &repo, &["commit", "-q", "-a", "-m", "topic"])?;
        run_git(setup.git, &repo, &["checkout", "-q", "main"])?;
        edit_every_file(&repo, &files, |text| format!("main head\n{text}"))?;
        run_git(setup.git, &repo, &["commit", "-q", "-a", "-m", "main"])?;
        run_git(setup.git, &repo, &["gc", "-q"])?;
        let main_tip = rev_parse(setup.git, &repo, "main")?;
        let meta = HotPathMeta {
            branch_a: "main".into(),
            branch_b: "topic".into(),
            main_branch: "main".into(),
            topic_branch: "topic".into(),
            base_commit,
            main_tip,
            pick_commit: String::new(),
            pick_series_commits: Vec::new(),
            wide: true,
            wide_branch_b: None,
        };
        std::fs::write(meta_path(&repo), serde_json::to_string_pretty(&meta)?)?;
        let mut headline = ctx(repo, setup);
        headline.hot_path_meta = Some(meta);
        Ok(headline)
    })
}

/// Merge `topic` into `main` where both sides edited every file of the wide tree.
pub fn merge_content_wide_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    merge_bench_from_meta(ctx)
}

/// Switch to the delete-heavy branch and back, so the return trip deletes files the first
/// trip just wrote (racily clean entries).
pub fn switch_delete_heavy_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    switch_bench_from_meta(ctx)
}

/// `reset --hard` to the delete-heavy branch and back to the full tree.
pub fn reset_hard_delete_heavy_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let Some(meta) = ctx.hot_path_meta.as_ref() else {
        return noop_bench();
    };
    let script = |program: &Path, quiet: bool| {
        let hops: Vec<Vec<String>> = [&meta.branch_b, &meta.base_commit]
            .into_iter()
            .map(|target| {
                let mut argv = vec!["reset".to_string()];
                if quiet {
                    argv.push("-q".into());
                }
                argv.extend(["--hard".to_string(), target.clone()]);
                argv
            })
            .collect();
        crate::scenarios::chain_shell(program, &hops)
    };
    (
        sh_script(script(&ctx.grit, false)),
        sh_script(script(&ctx.git, true)),
    )
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

pub fn branch_list_json_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["branch", "--json"]),
        sh_devnull(&ctx.git, &["branch", "-vv"]),
    )
}

pub fn branch_merged_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["branch", "--merged", "main"]),
        sh_devnull(&ctx.git, &["branch", "--merged", "main"]),
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

pub fn prepare_remote_rename_reset(ctx: &DailyHeadlineContext) -> DailyPrepare {
    let reset = prepare_daily_reset_script(&ctx.git);
    let undo = shell_command(
        &ctx.git,
        &["remote", "rename", REMOTE_RENAME_TARGET, "origin"],
    );
    let script = format!(
        "{reset}\ncd {} && ({undo} 2>/dev/null || true)\n",
        ctx.repo.display()
    );
    DailyPrepare::Shell(BenchArgv::new("/bin/sh", ["-c", script.as_str()]))
}

pub fn remote_rename_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(
            &ctx.grit,
            &["remote", "rename", "origin", REMOTE_RENAME_TARGET],
        ),
        sh_devnull(
            &ctx.git,
            &["remote", "rename", "origin", REMOTE_RENAME_TARGET],
        ),
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
        sh_devnull(&ctx.git, &["tag", "-l", "--sort=version:refname"]),
    )
}

pub fn tag_list_sort_version_git_git_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["tag", "--sort=version"]),
        sh_devnull(&ctx.git, &["tag", "-l", "--sort=version:refname"]),
    )
}

pub fn tag_contains_git_git_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    use super::helpers::GIT_GIT_SHORTLOG_TARGET;
    let rev = format!("{GIT_GIT_SHORTLOG_TARGET}^{{commit}}");
    (
        sh_devnull(
            &ctx.grit,
            &["tag", "--contains", rev.as_str(), "--sort=version"],
        ),
        sh_devnull(
            &ctx.git,
            &[
                "tag",
                "-l",
                "--contains",
                rev.as_str(),
                "--sort=version:refname",
            ],
        ),
    )
}

pub fn tag_create_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let name = ctx.tag_bench_name.as_deref().unwrap_or(TAG_BENCH_NAME);
    (
        sh_devnull(&ctx.grit, &["tag", name]),
        sh_devnull(&ctx.git, &["tag", name]),
    )
}

pub fn rebase_onto_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let Some(meta) = ctx.history_meta.as_ref() else {
        return noop_bench();
    };
    let (grit_argv, git_argv) = rebase_onto_scenario(&ctx.grit, &ctx.git, meta);
    (
        BenchArgv::new("/bin/sh", grit_argv),
        BenchArgv::new("/bin/sh", git_argv),
    )
}

pub fn rebase_onto_git_git_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    use super::helpers::GIT_GIT_REBASE_UPSTREAM;

    let meta = HistoryRewriteMeta {
        commit_count: 0,
        reword: String::new(),
        reword_rebase_line: 0,
        drop: String::new(),
        drop_rebase_line: 0,
        fixup: String::new(),
        fixup_path: String::new(),
        squash_range: String::new(),
        squash_rebase_parent: String::new(),
        rebase_upstream: GIT_GIT_REBASE_UPSTREAM.into(),
        bench_message: String::new(),
    };
    let (grit_argv, git_argv) = rebase_onto_scenario(&ctx.grit, &ctx.git, &meta);
    (
        BenchArgv::new("/bin/sh", grit_argv),
        BenchArgv::new("/bin/sh", git_argv),
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
        sh_devnull(&ctx.grit, &["config", CONFIG_BENCH_GET_KEY]),
        sh_devnull(&ctx.git, &["config", CONFIG_BENCH_GET_KEY]),
    )
}

pub fn config_get_origin_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["config", "--json", CONFIG_BENCH_GET_KEY]),
        sh_devnull(
            &ctx.git,
            &[
                "config",
                "--show-origin",
                "--show-scope",
                "--get",
                CONFIG_BENCH_GET_KEY,
            ],
        ),
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

fn notes_seed_count(smoke: bool) -> usize {
    if smoke {
        10
    } else {
        100
    }
}

pub fn setup_notes_list_n100(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    let ctx = setup_synth10k(setup)?;
    let count = notes_seed_count(setup.smoke);
    for i in 0..count {
        let rev = if i == 0 {
            "HEAD".to_string()
        } else {
            format!("HEAD~{i}")
        };
        run_git(
            setup.git,
            &ctx.repo,
            &["notes", "add", "-f", "-m", NOTES_BENCH_MSG, &rev],
        )?;
    }
    Ok(ctx)
}

fn tool_stdout(tool: &Path, cwd: &Path, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new(tool);
    cmd.args(args).current_dir(cwd);
    git_env(&mut cmd);
    let out = cmd
        .output()
        .with_context(|| format!("{} {}", tool.display(), args.join(" ")))?;
    if !out.status.success() {
        anyhow::bail!(
            "{} {} failed: {}",
            tool.display(),
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// One row of `git diff --numstat` / grit diff JSON numstat.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct NumstatRow {
    path: String,
    insertions: u64,
    deletions: u64,
}

fn parse_git_numstat(text: &str) -> Result<Vec<NumstatRow>> {
    let mut rows = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split('\t');
        let ins = parts.next().context("numstat insertions")?;
        let del = parts.next().context("numstat deletions")?;
        let path = parts.next().context("numstat path")?.to_string();
        let insertions = if ins == "-" {
            0
        } else {
            ins.parse()
                .with_context(|| format!("parse insertions {ins}"))?
        };
        let deletions = if del == "-" {
            0
        } else {
            del.parse()
                .with_context(|| format!("parse deletions {del}"))?
        };
        rows.push(NumstatRow {
            path,
            insertions,
            deletions,
        });
    }
    rows.sort();
    Ok(rows)
}

fn parse_grit_diff_numstat_json(text: &str) -> Result<Vec<NumstatRow>> {
    let json: serde_json::Value = serde_json::from_str(text.trim()).context("grit diff json")?;
    let files = json
        .get("files")
        .and_then(|v| v.as_array())
        .context("diff json files array")?;
    let mut rows = Vec::new();
    for file in files {
        let path = file
            .get("path")
            .and_then(|v| v.as_str())
            .context("diff file path")?
            .to_string();
        let stat = file.get("stat").context("diff file stat")?;
        let insertions = stat.get("insertions").and_then(|v| v.as_u64()).unwrap_or(0);
        let deletions = stat.get("deletions").and_then(|v| v.as_u64()).unwrap_or(0);
        rows.push(NumstatRow {
            path,
            insertions,
            deletions,
        });
    }
    rows.sort();
    Ok(rows)
}

pub fn verify_diff_stat(ctx: &DailyHeadlineContext) -> Result<()> {
    let git_out = tool_stdout(&ctx.git, &ctx.repo, &["diff", "--numstat"])?;
    let grit_out = tool_stdout(&ctx.grit, &ctx.repo, &["diff", "--json", "--numstat"])?;
    let git_rows = parse_git_numstat(&git_out)?;
    let grit_rows = parse_grit_diff_numstat_json(&grit_out)?;
    if git_rows != grit_rows {
        anyhow::bail!(
            "diff numstat mismatch (git {} rows, grit {} rows)",
            git_rows.len(),
            grit_rows.len()
        );
    }
    Ok(())
}

fn git_show_numstat_block(git: &Path, repo: &Path, oid: &str) -> Result<String> {
    Ok(
        tool_stdout(git, repo, &["show", "--numstat", "--format=", oid])?
            .trim_end()
            .to_string(),
    )
}

fn grit_log_commit_numstat_block(entry: &serde_json::Value) -> Result<String> {
    let files = entry
        .get("files")
        .and_then(|v| v.as_array())
        .context("log commit files")?;
    let mut lines = Vec::new();
    for file in files {
        let path = file.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let stat = file.get("stat").context("log file stat")?;
        let ins = stat.get("insertions").and_then(|v| v.as_u64()).unwrap_or(0);
        let del = stat.get("deletions").and_then(|v| v.as_u64()).unwrap_or(0);
        lines.push(format!("{ins}\t{del}\t{path}"));
    }
    Ok(lines.join("\n"))
}

pub fn verify_log_stat_n50(ctx: &DailyHeadlineContext) -> Result<()> {
    verify_log_json_filter(ctx)?;
    let grit_out = tool_stdout(
        &ctx.grit,
        &ctx.repo,
        &["log", "--limit", "50", "--json", "--numstat"],
    )?;
    let grit_json: serde_json::Value =
        serde_json::from_str(grit_out.trim()).context("parse grit log numstat json")?;
    let commits = grit_json
        .get("commits")
        .and_then(|v| v.as_array())
        .context("log json commits")?;
    for entry in commits {
        let oid = entry
            .get("oid")
            .and_then(|v| v.as_str())
            .context("log commit oid")?;
        let grit_block = grit_log_commit_numstat_block(entry)?;
        let git_block = git_show_numstat_block(&ctx.git, &ctx.repo, oid)?;
        if grit_block != git_block {
            anyhow::bail!("log numstat mismatch for commit {oid}");
        }
    }
    Ok(())
}

pub fn verify_log_json_filter(ctx: &DailyHeadlineContext) -> Result<()> {
    let git_out = tool_stdout(&ctx.git, &ctx.repo, &["log", "-50", "--format=%H"])?;
    let grit_out = tool_stdout(
        &ctx.grit,
        &ctx.repo,
        &[
            "log",
            "--limit",
            "50",
            "--json",
            "--filter",
            ".commits[].oid",
        ],
    )?;
    let mut git_oids: Vec<_> = git_out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    let mut grit_oids: Vec<String> = if grit_out.trim_start().starts_with('[') {
        serde_json::from_str(grit_out.trim()).context("parse grit log filter json array")?
    } else {
        grit_out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| l.trim_matches('"').to_string())
            .collect()
    };
    git_oids.sort();
    grit_oids.sort();
    if git_oids != grit_oids {
        anyhow::bail!(
            "log json-filter oids differ (git {} grit {})",
            git_oids.len(),
            grit_oids.len()
        );
    }
    Ok(())
}

fn parse_git_notes_list(text: &str) -> Result<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (note_blob, commit) = line
            .split_once(' ')
            .context("notes list note-blob and commit oids")?;
        pairs.push((commit.to_string(), note_blob.to_string()));
    }
    pairs.sort();
    Ok(pairs)
}

fn parse_grit_notes_list_json(text: &str) -> Result<Vec<(String, String)>> {
    let json: serde_json::Value = serde_json::from_str(text.trim()).context("grit notes json")?;
    if json.get("action").and_then(|v| v.as_str()) != Some("list") {
        anyhow::bail!("expected notes list json action");
    }
    let notes = json
        .get("notes")
        .and_then(|v| v.as_array())
        .context("notes json notes array")?;
    let mut pairs = Vec::new();
    for note in notes {
        let object = note
            .get("object")
            .and_then(|v| v.as_str())
            .context("note object")?
            .to_string();
        let blob = note
            .get("blob")
            .and_then(|v| v.as_str())
            .context("note blob")?
            .to_string();
        // `git notes list` prints `<note-blob> <commit>`; grit JSON uses object = commit, blob = note.
        pairs.push((object, blob));
    }
    pairs.sort();
    Ok(pairs)
}

pub fn verify_notes_list_n100(ctx: &DailyHeadlineContext) -> Result<()> {
    let expected = notes_seed_count(ctx.smoke);
    let git_out = tool_stdout(&ctx.git, &ctx.repo, &["notes", "list"])?;
    let grit_out = tool_stdout(&ctx.grit, &ctx.repo, &["notes", "--json"])?;
    let git_pairs = parse_git_notes_list(&git_out)?;
    let grit_pairs = parse_grit_notes_list_json(&grit_out)?;
    if git_pairs.len() != expected || grit_pairs.len() != expected {
        anyhow::bail!(
            "notes list count git {} grit {} expected {expected}",
            git_pairs.len(),
            grit_pairs.len()
        );
    }
    if git_pairs != grit_pairs {
        anyhow::bail!("notes (commit, note-blob) mappings differ between git and grit");
    }
    Ok(())
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

pub fn bundle_create_all_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let bundle = ctx
        .bundle_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| BUNDLE_FILE.into());
    (
        sh_devnull(&ctx.grit, &["bundle", "create", &bundle, "--all"]),
        sh_devnull(&ctx.git, &["bundle", "create", &bundle, "--all"]),
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

/// Both sides check out `HEAD` detached. Without a revision, `grit worktree add` creates a
/// branch named after the directory, which the per-run reset does not remove.
pub fn worktree_add_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let path = default_linked_worktree_path(ctx).display().to_string();
    (
        sh_devnull(&ctx.grit, &["worktree", "add", &path, "HEAD"]),
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

pub fn worktree_list_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["worktree", "list", "--json"]),
        sh_devnull(&ctx.git, &["worktree", "list", "--porcelain"]),
    )
}

pub fn setup_worktree_list_many(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    let ctx = setup_synth10k(setup)?;
    let parent = ctx.repo.parent().unwrap_or(&ctx.repo);
    for i in 0..WORKTREE_LIST_COUNT {
        let path = parent.join(format!("daily-wt-{i:02}"));
        if path.exists() {
            let _ = run_git(
                setup.git,
                &ctx.repo,
                &["worktree", "remove", "--force", &path.display().to_string()],
            );
            let _ = std::fs::remove_dir_all(&path);
        }
        run_git(
            setup.git,
            &ctx.repo,
            &["worktree", "add", "-q", &path.display().to_string(), "HEAD"],
        )?;
    }
    Ok(ctx)
}

pub fn prepare_stash_push_paths(ctx: &DailyHeadlineContext) -> DailyPrepare {
    let reset = prepare_daily_reset_script(&ctx.git);
    let prep = shell_command(
        &std::env::current_exe().unwrap_or_else(|_| PathBuf::from("grit-bench")),
        &[
            "prepare-worktree-add",
            "--git",
            &ctx.git.display().to_string(),
        ],
    );
    let repo_q = shell_quote_path(&ctx.repo);
    let script = format!("set -eu\n{reset}\ncd {repo_q} && {prep}");
    DailyPrepare::Shell(BenchArgv::new("/bin/sh", ["-c", script.as_str()]))
}

pub fn prepare_stash_pop_index(ctx: &DailyHeadlineContext) -> DailyPrepare {
    let reset = prepare_daily_reset_script(&ctx.git);
    let staged = shell_command(
        &std::env::current_exe().unwrap_or_else(|_| PathBuf::from("grit-bench")),
        &[
            "prepare-worktree-commit-staged",
            "--git",
            &ctx.git.display().to_string(),
        ],
    );
    let paths_q =
        shell_quote_path(&crate::fixture::scratch_dir().join(".grit-bench-worktree-paths"));
    let push = shell_command(
        &ctx.git,
        &["stash", "push", "-q", "-m", "daily-bench-index"],
    );
    let clear = shell_command(&ctx.git, &["stash", "clear"]);
    let repo_q = shell_quote_path(&ctx.repo);
    let script = format!(
        r#"set -eu
{reset}
cd {repo_q}
{staged}
first=$(head -1 {paths_q})
echo bench >> "$first"
({clear} 2>/dev/null || true)
{push}
"#
    );
    DailyPrepare::Shell(BenchArgv::new("/bin/sh", ["-c", script.as_str()]))
}

pub fn prepare_reset_keep(ctx: &DailyHeadlineContext) -> DailyPrepare {
    let reset = prepare_daily_reset_script(&ctx.git);
    let repo_q = shell_quote_path(&ctx.repo);
    let target_q =
        shell_quote_path(&crate::fixture::scratch_dir().join(".grit-bench-reset-keep-path"));
    let script = format!(
        r#"set -eu
{reset}
cd {repo_q}
target=$(cat {target_q})
echo bench-local >> "$target"
"#
    );
    DailyPrepare::Shell(BenchArgv::new("/bin/sh", ["-c", script.as_str()]))
}

/// Hyperfine prepare hook: reset fixture and apply unstaged edit on unchanged tracked path.
pub fn run_prepare_reset_keep(git: &Path) -> Result<()> {
    run_prepare_daily_reset(git)?;
    crate::worktree_fixture::apply_reset_keep_local_edit(
        &crate::worktree_fixture::active_worktree_repo(),
    )
}

pub fn prepare_clean_only_ignored(ctx: &DailyHeadlineContext) -> DailyPrepare {
    use crate::worktree_fixture::CLEAN_BENCH_IGNORED_SUFFIX;
    let reset = prepare_daily_reset_script(&ctx.git);
    let repo_q = shell_quote_path(&ctx.repo);
    let ignored = format!("daily-clean.{CLEAN_BENCH_IGNORED_SUFFIX}");
    let script = format!(
        r#"set -eu
{reset}
cd {repo_q}
exclude=.git/info/exclude
mkdir -p .git/info
grep -qxF '*.{CLEAN_BENCH_IGNORED_SUFFIX}' "$exclude" 2>/dev/null || echo '*.{CLEAN_BENCH_IGNORED_SUFFIX}' >> "$exclude"
echo ignored > {ignored}
echo scratch > daily-clean-scratch.txt
"#
    );
    DailyPrepare::Shell(BenchArgv::new("/bin/sh", ["-c", script.as_str()]))
}

pub fn remote_list_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["remote"]),
        sh_devnull(&ctx.git, &["remote"]),
    )
}

pub fn remote_remove_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["remote", "remove", "origin"]),
        sh_devnull(&ctx.git, &["remote", "remove", "origin"]),
    )
}

pub fn prepare_remote_remove(ctx: &DailyHeadlineContext) -> DailyPrepare {
    let Some(remote) = ctx.remote.as_ref() else {
        return DailyPrepare::None;
    };
    let reset = prepare_daily_reset_script(&ctx.git);
    let repo_q = shell_quote_path(&ctx.repo);
    let fetch = shell_command(
        &ctx.git,
        &[
            "fetch",
            "-q",
            "origin",
            "+refs/heads/*:refs/remotes/origin/*",
        ],
    );
    let pack = shell_command(&ctx.git, &["pack-refs", "--all"]);
    let remove = shell_command(&ctx.git, &["remote", "remove", "origin"]);
    let add = shell_command(
        &ctx.git,
        &["remote", "add", "origin", remote.remote_url.as_str()],
    );
    let script =
        format!("{reset}\ncd {repo_q}\n{remove} 2>/dev/null || true\n{add}\n{fetch}\n{pack}\n",);
    DailyPrepare::Shell(BenchArgv::new("/bin/sh", ["-c", script.as_str()]))
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

/// Remove a commit-graph file when present (shared git-git worktrees may carry one).
pub fn remove_commit_graph_if_present(repo: &Path) -> Result<()> {
    if !commit_graph_present(repo) {
        return Ok(());
    }
    let path = commit_graph_path(repo);
    std::fs::remove_file(&path)
        .with_context(|| format!("remove commit-graph at {}", path.display()))?;
    Ok(())
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
        "{reset}\n{} 2>/dev/null || true\n{} 2>/dev/null || true\n{}",
        shell_command(git, &["branch", "-D", new]),
        shell_command(git, &["branch", "-D", old]),
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

/// Reset git.git to the pinned tip and drop any in-progress rebase before timed runs.
pub fn prepare_rebase_git_git_reset(ctx: &DailyHeadlineContext) -> DailyPrepare {
    let reset = prepare_daily_reset_script(&ctx.git);
    let repo_q = shell_quote_path(&ctx.repo);
    let abort = shell_command(&ctx.git, &["rebase", "--abort"]);
    let script = format!("set -eu\ncd {repo_q} && ({abort} 2>/dev/null || true)\n{reset}");
    DailyPrepare::Shell(BenchArgv::new("/bin/sh", ["-c", script.as_str()]))
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

fn git_rev_parse(git: &Path, repo: &Path, spec: &str) -> Result<String> {
    let mut cmd = Command::new(git);
    cmd.args(["rev-parse", spec]).current_dir(repo);
    git_env(&mut cmd);
    let out = cmd
        .output()
        .with_context(|| format!("git rev-parse {spec}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "git rev-parse {spec} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Git-git worktree with a deep-history `--contains` target for tag ref queries.
pub fn setup_git_git_ref_contains(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    let mut ctx = super::helpers::setup_git_git_history(setup)?;
    ctx.ref_query_contains = Some(git_rev_parse(setup.git, &ctx.repo, "HEAD~5000")?);
    Ok(ctx)
}

/// Many-refs worktree with HEAD as `--merged` target for branch ref queries.
pub fn setup_many_refs_merged(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    let mut ctx = setup_many_refs(setup)?;
    ctx.ref_query_merged = Some(git_rev_parse(setup.git, &ctx.repo, "HEAD")?);
    Ok(ctx)
}

/// Tag `--contains` ref query via `refs-driver` vs `git for-each-ref`.
pub fn ref_query_contains_tags_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let Some(contains) = ctx.ref_query_contains.as_deref() else {
        return noop_bench();
    };
    let grit = BenchArgv::new(
        &ctx.bench,
        vec![
            "refs-driver".to_string(),
            "ref-query".to_string(),
            ctx.repo.display().to_string(),
            "--namespace".to_string(),
            "tags".to_string(),
            "--contains".to_string(),
            contains.to_string(),
        ],
    );
    let git = BenchArgv::new(
        &ctx.git,
        vec![
            "for-each-ref".to_string(),
            "refs/tags/".to_string(),
            format!("--contains={contains}"),
            "--format=%(refname)".to_string(),
        ],
    );
    (grit, git)
}

/// Branch `--merged` ref query via `refs-driver` vs `git for-each-ref`.
pub fn ref_query_merged_branches_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let Some(merged) = ctx.ref_query_merged.as_deref() else {
        return noop_bench();
    };
    let grit = BenchArgv::new(
        &ctx.bench,
        vec![
            "refs-driver".to_string(),
            "ref-query".to_string(),
            ctx.repo.display().to_string(),
            "--namespace".to_string(),
            "heads".to_string(),
            "--merged".to_string(),
            merged.to_string(),
        ],
    );
    let git = BenchArgv::new(
        &ctx.git,
        vec![
            "for-each-ref".to_string(),
            "refs/heads/".to_string(),
            format!("--merged={merged}"),
            "--format=%(refname)".to_string(),
        ],
    );
    (grit, git)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_add_bench_detaches_head_on_both_sides() {
        let ctx = DailyHeadlineContext::new(
            PathBuf::from("/tmp/sample/repo"),
            Path::new("/usr/bin/git"),
            Path::new("/usr/bin/grit"),
            Path::new("/usr/bin/grit-bench"),
            false,
        );
        let (grit, git) = worktree_add_bench(&ctx);
        for side in [&grit, &git] {
            let script = side.args.last().expect("shell script");
            assert!(
                script.contains("'worktree' 'add'") && script.contains("'HEAD'"),
                "{script}"
            );
        }
    }
}
