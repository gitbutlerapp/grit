//! Shared setup and argv helpers for daily headline scenarios.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use super::matrix_registry::MATRIX_SCENARIOS;
use super::plan::{DailyHeadlineContext, DailySetup};
use super::registry::{HeadlineScenario, RepoShape, ScenarioSpec, HEADLINE_SCENARIOS};
use super::revision_selection_matrix_registry::REVISION_SELECTION_MATRIX_SCENARIOS;
use super::runner::BenchArgv;
use crate::fixture::scratch_dir;
use crate::fixture_registry::{self, FixtureKind};
use crate::hot_path_fixture::{
    setup_pick_merge_fixture, setup_switch_fixture, touch_paths_for_size, HotPathRepoSpec,
};
use crate::real_repo_fixture::{git_env, run_git};
use crate::scenarios::{merge_argv, pick_argv, switch_argv, PrepareKind};
use crate::shell::shell_quote;
use crate::worktree_fixture::{
    ensure_worktree_bench_repo, record_matrix_path_targets, set_worktree_bench_cwd,
    shell_git_add_modified_set, shell_git_add_modified_set_and_commit, shell_git_apply_cached,
    shell_git_commit_paths_only, shell_git_stash_push_paths, shell_grit_add_modified_set,
    shell_grit_add_patch_file, shell_grit_commit_paths, shell_grit_stash_push_paths, shell_mv_file,
    shell_redirect_devnull, shell_restore_modified_set, shell_rm_dir_dry_run_git,
    shell_rm_dir_dry_run_grit, shell_rm_dir_git, shell_rm_dir_grit,
};

/// Shortlog/compare target for git-git matrix rows (tag present on pinned git.git).
pub const GIT_GIT_SHORTLOG_TARGET: &str = "v2.46.0";

/// Upstream for `rebase/onto@git-git`: deep enough to replay real work, shallow enough
/// that replaying `upstream..HEAD` on the pinned v2.47.0 tip completes without conflicts.
pub const GIT_GIT_REBASE_UPSTREAM: &str = "HEAD~10";

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

/// Pin a stable scratch root for the current `run_daily` invocation (avoids nested `daily-*` paths).
pub fn pin_daily_scratch_root() {
    if std::env::var("GRIT_BENCH_DAILY_ROOT").is_ok() {
        return;
    }
    let mut root = std::env::var("GRIT_BENCH_SCRATCH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp/grit-bench-scratch"));
    while root
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("daily-"))
    {
        root = root
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp/grit-bench-scratch"));
    }
    std::env::set_var("GRIT_BENCH_DAILY_ROOT", &root);
}

pub(crate) fn with_scratch<T, F>(label: &str, f: F) -> Result<T>
where
    F: FnOnce(&Path) -> Result<T>,
{
    pin_daily_scratch_root();
    let base = std::env::var("GRIT_BENCH_DAILY_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp/grit-bench-scratch"));
    let scratch = base.join(format!("daily-{label}"));
    std::env::set_var("GRIT_BENCH_SCRATCH", &scratch);
    f(&scratch)
}

pub fn setup_registry_worktree(setup: &DailySetup<'_>, kind: FixtureKind) -> Result<PathBuf> {
    with_scratch(kind.cache_name(), |_scratch| {
        let (repo, _record) =
            ensure_worktree_bench_repo(setup.git, kind, setup.offline).context("worktree repo")?;
        set_worktree_bench_cwd(&repo)?;
        Ok(repo)
    })
}

pub fn setup_git_git_worktree(setup: &DailySetup<'_>) -> Result<PathBuf> {
    setup_registry_worktree(setup, FixtureKind::GitGitWt)
}

pub fn setup_hot_path_switch(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    with_scratch("switch", |_scratch| {
        let spec = hot_path_spec(setup.smoke);
        let (repo, meta) = setup_switch_fixture(setup.git, spec, false)?;
        let mut headline = ctx(repo, setup);
        headline.hot_path_meta = Some(meta);
        Ok(headline)
    })
}

pub fn setup_hot_path_pick_merge(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    with_scratch("pick-merge", |_scratch| {
        let spec = hot_path_spec(setup.smoke);
        let touch = touch_paths_for_size(spec.file_count);
        let (_repo, meta) = setup_pick_merge_fixture(setup.git, spec, touch)?;
        let mut headline = ctx(scratch_dir(), setup);
        headline.hot_path_meta = Some(meta);
        Ok(headline)
    })
}

pub fn setup_init_empty(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    with_scratch("init-empty", |scratch| {
        let repo = scratch.join("empty-init");
        if repo.exists() {
            crate::fixture::remove_dir_robust(&repo);
        }
        fs::create_dir_all(&repo).context("init empty dir")?;
        set_worktree_bench_cwd(&repo)?;
        Ok(ctx(repo, setup))
    })
}

fn git_rev_parse(git: &Path, repo: &Path, spec: &str) -> Result<String> {
    let mut cmd = Command::new(git);
    cmd.args(["rev-parse", spec]).current_dir(repo);
    git_env(&mut cmd);
    let out = cmd
        .output()
        .with_context(|| format!("git rev-parse {spec}"))?;
    if !out.status.success() {
        bail!(
            "git rev-parse {spec} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Pin git-git matrix defaults: shortlog target and a stable HEAD blob for `cat/blob`.
pub fn finish_git_git_matrix_ctx(headline: &mut DailyHeadlineContext, git: &Path) -> Result<()> {
    let path = headline
        .matrix_path
        .as_deref()
        .or(headline.blame_path.as_deref())
        .unwrap_or("README.md");
    let blob_spec = format!("HEAD:{path}");
    headline.cat_blob = Some(git_rev_parse(git, &headline.repo, &blob_spec)?);
    run_git(
        git,
        &headline.repo,
        &["config", "target.branch", GIT_GIT_SHORTLOG_TARGET],
    )?;
    Ok(())
}

/// Reproduce a fresh clone's historical loose-tag layout without moving any objects.
pub fn setup_git_git_loose_tags(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    let headline = setup_git_git_history(setup)?;
    let mut cmd = Command::new(setup.git);
    git_env(&mut cmd);
    let out = cmd
        .current_dir(&headline.repo)
        .args([
            "for-each-ref",
            "--format=%(objectname) %(refname) %(symref)",
            "refs/",
        ])
        .output()?;
    anyhow::ensure!(out.status.success(), "cannot list fixture tags");
    let git_dir = PathBuf::from(git_rev_parse(
        setup.git,
        &headline.repo,
        "--absolute-git-dir",
    )?);
    let odb = grit_lib::objects::odb::Odb::new(&git_dir.join("objects"));
    let mut packed = Vec::new();
    for line in std::str::from_utf8(&out.stdout)?.lines() {
        let mut fields = line.split_whitespace();
        let oid = fields.next().context("tag oid")?;
        let name = fields.next().context("ref name")?;
        if fields.next().is_some() {
            continue;
        } // Keep symbolic refs loose.
        if name.starts_with("refs/tags/") {
            let path = git_dir.join(name);
            fs::create_dir_all(path.parent().context("tag parent")?)?;
            fs::write(path, format!("{oid}\n"))?;
        } else {
            let oid = oid.parse()?;
            let (peeled, _) = odb.peel_tags(oid)?;
            packed.push(grit_lib::refs::PackedRefRecord {
                refname: name.to_owned(),
                oid,
                peeled: (peeled != oid).then_some(peeled),
            });
        }
    }
    // Identical loose and packed tag records would otherwise hide the object-read cost.
    grit_lib::refs::write_packed_refs_file(&git_dir, &packed)?;
    Ok(headline)
}

/// Same refs and objects as the loose-tag scenario, with packed ref storage.
pub fn setup_git_git_packed_tags(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    let headline = setup_git_git_history(setup)?;
    run_git(setup.git, &headline.repo, &["pack-refs", "--all"])?;
    Ok(headline)
}

pub fn setup_git_git_no_commit_graph(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    let headline = setup_git_git_history(setup)?;
    super::matrix_helpers::remove_commit_graph_if_present(&headline.repo)?;
    super::matrix_helpers::ensure_commit_graph_absent(&headline.repo)?;
    Ok(headline)
}

pub fn setup_git_git_history(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    let repo = setup_git_git_worktree(setup)?;
    let mut headline = ctx(repo, setup);
    headline.blame_path = Some(
        crate::campaign_worktree::blame_target_path(setup.git, &headline.repo)
            .unwrap_or_else(|_| "README.md".into()),
    );
    headline.matrix_path = headline.blame_path.clone();
    headline.merge_base_a = Some("v2.46.0".into());
    headline.merge_base_b = Some("v2.47.0".into());
    headline.shortlog_git_range = Some(format!("{GIT_GIT_SHORTLOG_TARGET}..HEAD"));
    finish_git_git_matrix_ctx(&mut headline, setup.git)?;
    Ok(headline)
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
    with_scratch("stash", |_scratch| {
        let size = if setup.smoke { 1_000 } else { 10_000 };
        let repo = crate::fixture::create_repo(setup.git, size).context("stash repo")?;
        crate::fixture::prepare_commit_iteration(&repo, setup.git)
            .context("stash bench initial dirty")?;
        Ok(repo)
    })
}

pub fn sh_devnull(program: &Path, args: &[&str]) -> BenchArgv {
    let inner = shell_redirect_devnull(program, args);
    BenchArgv::new("/bin/sh", ["-c", &format!("set -e; {inner}")])
}

pub fn sh_script(script: String) -> BenchArgv {
    BenchArgv::new("/bin/sh", ["-c", script.as_str()])
}

pub fn ctx(repo: PathBuf, setup: &DailySetup<'_>) -> DailyHeadlineContext {
    DailyHeadlineContext::new(repo, setup.git, setup.grit, setup.bench, setup.smoke)
}

pub fn fixture_kind(shape: RepoShape, smoke: bool) -> FixtureKind {
    match shape {
        RepoShape::Small => {
            if smoke {
                FixtureKind::SmallSmoke
            } else {
                FixtureKind::Small
            }
        }
        RepoShape::Synth10k => synth10k_kind(smoke),
        RepoShape::Synth100k => {
            if smoke {
                FixtureKind::Synth100kSmoke
            } else {
                FixtureKind::Synth100k
            }
        }
        RepoShape::GitGit => FixtureKind::GitGitWt,
        RepoShape::BigBlobs => {
            if smoke {
                FixtureKind::BigBlobsSmoke
            } else {
                FixtureKind::BigBlobs
            }
        }
        RepoShape::DeepTree => {
            if smoke {
                FixtureKind::DeepTreeSmoke
            } else {
                FixtureKind::DeepTree
            }
        }
        RepoShape::ManyRefs => many_refs_kind(smoke),
        RepoShape::ManyRefsLoose => {
            if smoke {
                FixtureKind::ManyRefsLooseSmoke
            } else {
                FixtureKind::ManyRefsLoose
            }
        }
    }
}

pub fn setup_shape_worktree(
    setup: &DailySetup<'_>,
    shape: RepoShape,
) -> Result<DailyHeadlineContext> {
    let kind = fixture_kind(shape, setup.smoke);
    let repo = setup_registry_worktree(setup, kind)?;
    record_matrix_path_targets(setup.git, &repo)?;
    Ok(ctx(repo, setup))
}

pub fn setup_small(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    setup_shape_worktree(setup, RepoShape::Small)
}

pub fn setup_synth100k(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    setup_shape_worktree(setup, RepoShape::Synth100k)
}

pub fn setup_deep_tree(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    setup_shape_worktree(setup, RepoShape::DeepTree)
}

pub fn setup_big_blobs(setup: &DailySetup<'_>) -> Result<DailyHeadlineContext> {
    setup_shape_worktree(setup, RepoShape::BigBlobs)
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

pub fn status_paths_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["status", "--", "data/d0000"]),
        sh_devnull(&ctx.git, &["status", "-s", "--", "data/d0000"]),
    )
}

pub fn status_renamed_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
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

pub fn add_patch_file_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_script(shell_grit_add_patch_file(&ctx.grit)),
        sh_script(shell_git_apply_cached(&ctx.git)),
    )
}

pub fn add_hunks_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_script(crate::worktree_fixture::shell_grit_add_hunks(&ctx.grit)),
        sh_script(crate::worktree_fixture::shell_git_apply_hunks_half(
            &ctx.git,
        )),
    )
}

pub fn add_patch_file_large_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_script(crate::worktree_fixture::shell_grit_add_patch_file_half(
            &ctx.grit,
        )),
        sh_script(crate::worktree_fixture::shell_git_apply_hunks_half(
            &ctx.git,
        )),
    )
}

pub fn restore_hunks_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_script(crate::worktree_fixture::shell_grit_restore_hunks(&ctx.grit)),
        sh_script(crate::worktree_fixture::shell_git_apply_hunks_reverse(
            &ctx.git,
        )),
    )
}

pub fn add_lines_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_script(crate::worktree_fixture::shell_grit_add_lines(&ctx.grit)),
        sh_script(crate::worktree_fixture::shell_git_apply_lines_half(
            &ctx.git,
        )),
    )
}

pub fn commit_small_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["commit", "bench"]),
        sh_script(shell_git_add_modified_set_and_commit(&ctx.git)),
    )
}

pub fn commit_dry_run_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["commit", "--dry-run", "-m", "bench"]),
        sh_devnull(&ctx.git, &["commit", "--dry-run", "-a", "-m", "bench"]),
    )
}

pub fn commit_staged_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["commit", "--staged", "bench"]),
        sh_devnull(&ctx.git, &["commit", "-q", "-m", "bench"]),
    )
}

pub fn commit_paths_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_script(shell_grit_commit_paths(&ctx.grit)),
        sh_script(shell_git_commit_paths_only(&ctx.git)),
    )
}

pub fn diff_worktree_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["diff"]),
        sh_devnull(&ctx.git, &["diff"]),
    )
}

pub fn diff_staged_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["diff", "--staged"]),
        sh_devnull(&ctx.git, &["diff", "--cached"]),
    )
}

pub fn diff_stat_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["diff", "--stat"]),
        sh_devnull(&ctx.git, &["diff", "--stat"]),
    )
}

pub fn log_stat_n50_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["log", "--limit", "50", "--stat"]),
        sh_devnull(&ctx.git, &["log", "--stat", "-50"]),
    )
}

pub fn log_json_filter_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(
            &ctx.grit,
            &[
                "log",
                "--limit",
                "50",
                "--json",
                "--filter",
                ".commits[].oid",
            ],
        ),
        sh_devnull(&ctx.git, &["log", "-50", "--format=%H"]),
    )
}

pub fn add_all_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["add"]),
        sh_devnull(&ctx.git, &["add", "-A"]),
    )
}

pub fn add_update_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["add", "-u"]),
        sh_devnull(&ctx.git, &["add", "-u"]),
    )
}

pub fn add_dry_run_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["add", "--dry-run"]),
        sh_devnull(&ctx.git, &["add", "-n", "-A"]),
    )
}

pub fn restore_paths_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_script(shell_restore_modified_set(&ctx.grit)),
        sh_script(shell_restore_modified_set(&ctx.git)),
    )
}

pub fn reset_hard_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["reset", "--hard", "HEAD"]),
        sh_devnull(&ctx.git, &["reset", "--hard", "HEAD"]),
    )
}

pub fn reset_mixed_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["reset", "HEAD"]),
        sh_devnull(&ctx.git, &["reset", "--mixed", "HEAD"]),
    )
}

pub fn rm_dir_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_script(shell_rm_dir_grit(&ctx.grit)),
        sh_script(shell_rm_dir_git(&ctx.git)),
    )
}

pub fn mv_file_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_script(shell_mv_file(&ctx.grit)),
        sh_script(shell_mv_file(&ctx.git)),
    )
}

pub fn clean_untracked_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["clean", "-f"]),
        sh_devnull(&ctx.git, &["clean", "-fd"]),
    )
}

pub fn init_empty_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["init"]),
        sh_devnull(&ctx.git, &["init"]),
    )
}

pub fn init_branch_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["init", "-b", "trunk"]),
        sh_devnull(&ctx.git, &["init", "-b", "trunk"]),
    )
}

pub fn stash_push_paths_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_script(shell_grit_stash_push_paths(&ctx.grit)),
        sh_script(shell_git_stash_push_paths(&ctx.git)),
    )
}

pub fn stash_pop_index_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["stash", "pop", "--index"]),
        sh_devnull(&ctx.git, &["stash", "pop", "--index"]),
    )
}

pub fn reset_keep_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["reset", "--keep", "HEAD~1"]),
        sh_devnull(&ctx.git, &["reset", "--keep", "HEAD~1"]),
    )
}

pub fn rm_dry_run_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_script(shell_rm_dir_dry_run_grit(&ctx.grit)),
        sh_script(shell_rm_dir_dry_run_git(&ctx.git)),
    )
}

pub fn clean_only_ignored_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["clean", "--only-ignored"]),
        sh_devnull(&ctx.git, &["clean", "-n", "-X"]),
    )
}

pub fn log_oneline_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["log", "--limit", "100"]),
        sh_devnull(&ctx.git, &["log", "--oneline", "-100"]),
    )
}

pub fn log_json_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["log", "--limit", "100", "--json"]),
        sh_devnull(
            &ctx.git,
            &[
                "log",
                "-100",
                "--format=%H%x00%P%x00%an%x00%ae%x00%at%x00%cn%x00%ce%x00%ct%x00%s%x00%b%x00",
            ],
        ),
    )
}

/// Equivalent bounded author/committer/parent/decoration/message metadata.
pub fn log_json_n10_decorations_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["log", "-n", "10", "--json"]),
        sh_devnull(
            &ctx.git,
            &[
                "log",
                "-n",
                "10",
                "--format=%H%x09%P%x09%an%x09%ae%x09%aI%x09%cn%x09%ce%x09%cI%x09%D%x09%B",
            ],
        ),
    )
}

pub fn log_all_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let cap = if ctx.smoke { "500" } else { "5000" };
    (
        sh_devnull(&ctx.grit, &["log", "--limit", cap]),
        sh_devnull(&ctx.git, &["log", "--oneline", &format!("-{cap}")]),
    )
}

pub fn log_path_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let path = ctx.matrix_path.as_deref().unwrap_or("README.md");
    (
        sh_devnull(&ctx.grit, &["log", "--", path]),
        sh_devnull(&ctx.git, &["log", "--oneline", "--", path]),
    )
}

pub fn revs_count_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["revs", "--count", "HEAD"]),
        sh_devnull(&ctx.git, &["rev-list", "--count", "HEAD"]),
    )
}

/// Literal-path count on git.git without commit-graph (issue #1517 repro).
pub fn revs_count_literal_path_git_git_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(
            &ctx.grit,
            &[
                "revs",
                "--count",
                GIT_GIT_LITERAL_PATH_BENCH_REV,
                "--",
                GIT_GIT_LITERAL_PATH_BENCH_FILE,
            ],
        ),
        sh_devnull(
            &ctx.git,
            &[
                "rev-list",
                "--count",
                GIT_GIT_LITERAL_PATH_BENCH_REV,
                "--",
                GIT_GIT_LITERAL_PATH_BENCH_FILE,
            ],
        ),
    )
}

/// Bounded path log on git.git Documentation/ (issue #1517 repro).
pub fn log_limit_documentation_git_git_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(
            &ctx.grit,
            &["log", "-n", "200", "--", GIT_GIT_LITERAL_PATH_BENCH_DIR],
        ),
        sh_devnull(
            &ctx.git,
            &[
                "log",
                "-n",
                "200",
                "--format=%H",
                "--",
                GIT_GIT_LITERAL_PATH_BENCH_DIR,
            ],
        ),
    )
}

/// Pinned revision for no-commit-graph path-limited benchmarks on git.git.
pub const GIT_GIT_LITERAL_PATH_BENCH_REV: &str = "7e34d81";
/// Single-file path for literal path-limited rev-list benchmarks.
pub const GIT_GIT_LITERAL_PATH_BENCH_FILE: &str = "Documentation/git-log.txt";
/// Directory path for bounded `log -n` benchmarks on git.git.
pub const GIT_GIT_LITERAL_PATH_BENCH_DIR: &str = "Documentation";

pub fn merge_base_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let a = ctx.merge_base_a.as_deref().unwrap_or("master");
    let b = ctx.merge_base_b.as_deref().unwrap_or("maint");
    (
        sh_devnull(&ctx.grit, &["merge-base", a, b]),
        sh_devnull(&ctx.git, &["merge-base", a, b]),
    )
}

pub fn resolve_head_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["resolve", "HEAD"]),
        sh_devnull(&ctx.git, &["rev-parse", "HEAD"]),
    )
}

pub fn resolve_change_id_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let Some(prefix) = ctx.change_id_bench_prefix.as_deref() else {
        return noop_bench();
    };
    (
        sh_devnull(&ctx.grit, &["resolve", prefix]),
        sh_devnull(&ctx.git, &["log", "--all", "--format=%H"]),
    )
}

pub fn cat_blob_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let Some(blob) = ctx.cat_blob.as_deref() else {
        return noop_bench();
    };
    (
        sh_devnull(&ctx.grit, &["cat", blob]),
        sh_devnull(&ctx.git, &["cat-file", "-p", blob]),
    )
}

pub fn shell_git_patch_id_stable_from_head(git: &Path) -> String {
    let git_q = shell_quote(&git.to_string_lossy());
    format!("set -eu; {git_q} show HEAD | {git_q} patch-id --stable > /dev/null 2>&1")
}

/// Ensure `HEAD` has at least `target` reflog lines (synthetic padding for bounded-list benches).
pub fn pad_head_reflog(repo: &Path, target: usize) -> std::io::Result<()> {
    use std::io::Write;
    let log_path = repo.join(".git/logs/HEAD");
    let content = std::fs::read_to_string(&log_path)?;
    let existing = content.lines().filter(|l| !l.is_empty()).count();
    if existing >= target {
        return Ok(());
    }
    let Some(template) = content.lines().rfind(|l| !l.is_empty()) else {
        return Ok(());
    };
    let (head, message) = template
        .split_once('\t')
        .map_or((template, "commit: bench pad"), |(h, m)| (h, m));
    let mut parts = head.splitn(3, ' ');
    let Some(old) = parts.next() else {
        return Ok(());
    };
    let Some(new) = parts.next() else {
        return Ok(());
    };
    let mut file = std::fs::OpenOptions::new().append(true).open(&log_path)?;
    for i in existing..target {
        let ts = 1_700_000_000_i64 - i as i64;
        writeln!(file, "{old} {new} T <t@e.c> {ts} +0000\t{message}")?;
    }
    Ok(())
}

pub fn reflog_list_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let _ = pad_head_reflog(&ctx.repo, 10_000);
    (
        sh_devnull(&ctx.grit, &["reflog", "-n", "100", "--json"]),
        sh_devnull(&ctx.git, &["reflog", "-n", "100"]),
    )
}

pub fn shortlog_ahead_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["shortlog"]),
        sh_devnull(
            &ctx.git,
            &[
                "log",
                "--oneline",
                ctx.shortlog_git_range.as_deref().unwrap_or("master..HEAD"),
            ],
        ),
    )
}

pub fn shortlog_behind_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    let ahead_range = ctx.shortlog_git_range.as_deref().unwrap_or("master..HEAD");
    let (base, tip) = ahead_range.split_once("..").unwrap_or(("master", "HEAD"));
    let behind_range = format!("{tip}..{base}");
    let git_q = shell_quote(&ctx.git.to_string_lossy());
    let script = format!(
        "set -eu; {git_q} rev-list --left-right --count {tip}...{base} >/dev/null; {git_q} log -n 20 {behind_range} >/dev/null"
    );
    (
        sh_devnull(&ctx.grit, &["shortlog", "--behind", "-n", "20"]),
        sh_script(script),
    )
}

pub fn patch_id_head_bench(ctx: &DailyHeadlineContext) -> (BenchArgv, BenchArgv) {
    (
        sh_devnull(&ctx.grit, &["patch-id", "HEAD"]),
        sh_script(shell_git_patch_id_stable_from_head(&ctx.git)),
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
        PrepareKind::WorktreeAddPatchFile => "prepare-worktree-add-patch-file",
        PrepareKind::WorktreeAddHunks => "prepare-worktree-add-hunks",
        PrepareKind::WorktreeAddLines => "prepare-worktree-add-lines",
        PrepareKind::WorktreeCommit => "prepare-worktree-commit",
        PrepareKind::WorktreeCommitStaged => "prepare-worktree-commit-staged",
        PrepareKind::WorktreeDiff => "prepare-worktree-diff",
        PrepareKind::WorktreeStagedDiff => "prepare-worktree-staged-diff",
        PrepareKind::WorktreeRestorePaths => "prepare-worktree-restore-paths",
        PrepareKind::WorktreeRmDir => "prepare-worktree-rm-dir",
        PrepareKind::WorktreeMvFile => "prepare-worktree-mv-file",
        PrepareKind::WorktreeCleanUntracked => "prepare-worktree-clean-untracked",
        PrepareKind::WorktreeInitEmpty => "prepare-worktree-init-empty",
        PrepareKind::RestoreReset => "prepare-restore",
        PrepareKind::HistoryRewriteReset => "prepare-history-rewrite",
        PrepareKind::RevertReset => "prepare-revert",
        _ => "prepare-worktree-status-clean",
    };
    BenchArgv::new(prepare_bin, [sub, "--git", &git.display().to_string()])
}

pub fn scenario_spec(id: &str) -> &'static ScenarioSpec {
    HEADLINE_SCENARIOS
        .iter()
        .find(|s| s.id == id)
        .or_else(|| MATRIX_SCENARIOS.iter().find(|s| s.id == id))
        .or_else(|| {
            super::registry::REMOTE_MATRIX_SCENARIOS
                .iter()
                .find(|s| s.id == id)
        })
        .or_else(|| {
            REVISION_SELECTION_MATRIX_SCENARIOS
                .iter()
                .find(|s| s.id == id)
        })
        .unwrap_or_else(|| panic!("unknown scenario id {id}"))
}

pub fn headline_spec(id: &str) -> &'static HeadlineScenario {
    scenario_spec(id)
}

pub fn prepare_none(_: &super::plan::DailyHeadlineContext) -> super::plan::DailyPrepare {
    super::plan::DailyPrepare::None
}

pub fn prepare_switch_reset(_: &super::plan::DailyHeadlineContext) -> super::plan::DailyPrepare {
    super::plan::DailyPrepare::Hook(PrepareKind::SwitchReset)
}

pub fn prepare_pick_reset(_: &super::plan::DailyHeadlineContext) -> super::plan::DailyPrepare {
    super::plan::DailyPrepare::Hook(PrepareKind::PickReset)
}

pub fn prepare_merge_reset(_: &super::plan::DailyHeadlineContext) -> super::plan::DailyPrepare {
    super::plan::DailyPrepare::Hook(PrepareKind::MergeReset)
}

pub fn prepare_revert_reset(_: &super::plan::DailyHeadlineContext) -> super::plan::DailyPrepare {
    super::plan::DailyPrepare::Hook(PrepareKind::RevertReset)
}

pub fn prepare_stash_reset(_: &super::plan::DailyHeadlineContext) -> super::plan::DailyPrepare {
    super::plan::DailyPrepare::Hook(PrepareKind::StashReset)
}

pub fn prepare_history_rewrite_reset(
    _: &super::plan::DailyHeadlineContext,
) -> super::plan::DailyPrepare {
    super::plan::DailyPrepare::Hook(PrepareKind::HistoryRewriteReset)
}

pub fn prepare_worktree_status_clean(
    _: &super::plan::DailyHeadlineContext,
) -> super::plan::DailyPrepare {
    super::plan::DailyPrepare::Hook(PrepareKind::WorktreeStatusClean)
}

pub fn prepare_worktree_status_dirty(
    _: &super::plan::DailyHeadlineContext,
) -> super::plan::DailyPrepare {
    super::plan::DailyPrepare::Hook(PrepareKind::WorktreeStatusDirty)
}

pub fn prepare_worktree_add(_: &super::plan::DailyHeadlineContext) -> super::plan::DailyPrepare {
    super::plan::DailyPrepare::Hook(PrepareKind::WorktreeAdd)
}

pub fn prepare_worktree_add_patch_file(
    _: &super::plan::DailyHeadlineContext,
) -> super::plan::DailyPrepare {
    super::plan::DailyPrepare::Hook(PrepareKind::WorktreeAddPatchFile)
}

pub fn prepare_worktree_restore_hunks(
    ctx: &super::plan::DailyHeadlineContext,
) -> super::plan::DailyPrepare {
    prepare_worktree_add_hunks(ctx)
}

pub fn prepare_worktree_add_hunks(
    _: &super::plan::DailyHeadlineContext,
) -> super::plan::DailyPrepare {
    super::plan::DailyPrepare::Hook(PrepareKind::WorktreeAddHunks)
}

pub fn prepare_worktree_add_lines(
    _: &super::plan::DailyHeadlineContext,
) -> super::plan::DailyPrepare {
    super::plan::DailyPrepare::Hook(PrepareKind::WorktreeAddLines)
}

pub fn prepare_worktree_commit(_: &super::plan::DailyHeadlineContext) -> super::plan::DailyPrepare {
    super::plan::DailyPrepare::Hook(PrepareKind::WorktreeCommit)
}

pub fn prepare_worktree_commit_staged(
    _: &super::plan::DailyHeadlineContext,
) -> super::plan::DailyPrepare {
    super::plan::DailyPrepare::Hook(PrepareKind::WorktreeCommitStaged)
}

pub fn prepare_worktree_diff(_: &super::plan::DailyHeadlineContext) -> super::plan::DailyPrepare {
    super::plan::DailyPrepare::Hook(PrepareKind::WorktreeDiff)
}

pub fn prepare_worktree_staged_diff(
    _: &super::plan::DailyHeadlineContext,
) -> super::plan::DailyPrepare {
    super::plan::DailyPrepare::Hook(PrepareKind::WorktreeStagedDiff)
}

pub fn prepare_worktree_restore_paths(
    _: &super::plan::DailyHeadlineContext,
) -> super::plan::DailyPrepare {
    super::plan::DailyPrepare::Hook(PrepareKind::WorktreeRestorePaths)
}

pub fn prepare_worktree_rm_dir(_: &super::plan::DailyHeadlineContext) -> super::plan::DailyPrepare {
    super::plan::DailyPrepare::Hook(PrepareKind::WorktreeRmDir)
}

pub fn prepare_worktree_mv_file(
    _: &super::plan::DailyHeadlineContext,
) -> super::plan::DailyPrepare {
    super::plan::DailyPrepare::Hook(PrepareKind::WorktreeMvFile)
}

pub fn prepare_worktree_clean_untracked(
    _: &super::plan::DailyHeadlineContext,
) -> super::plan::DailyPrepare {
    super::plan::DailyPrepare::Hook(PrepareKind::WorktreeCleanUntracked)
}

pub fn prepare_worktree_init_empty(
    _: &super::plan::DailyHeadlineContext,
) -> super::plan::DailyPrepare {
    super::plan::DailyPrepare::Hook(PrepareKind::WorktreeInitEmpty)
}

pub fn matrix_spec(id: &str) -> &'static super::registry::ScenarioSpec {
    scenario_spec(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_scratch_paths_do_not_nest() {
        std::env::remove_var("GRIT_BENCH_DAILY_ROOT");
        std::env::set_var("GRIT_BENCH_SCRATCH", "/tmp/grit-bench-scratch");
        let _a = with_scratch("first", |p| Ok(p.to_path_buf())).expect("first");
        let first = std::env::var("GRIT_BENCH_SCRATCH").expect("scratch");
        let _b = with_scratch("second", |p| Ok(p.to_path_buf())).expect("second");
        let second = std::env::var("GRIT_BENCH_SCRATCH").expect("scratch");
        assert_eq!(first, "/tmp/grit-bench-scratch/daily-first");
        assert_eq!(second, "/tmp/grit-bench-scratch/daily-second");
        assert!(!second.contains("daily-first/daily-second"));
        std::env::remove_var("GRIT_BENCH_DAILY_ROOT");
    }

    fn sample_ctx() -> DailyHeadlineContext {
        let mut ctx = DailyHeadlineContext::new(
            PathBuf::from("/tmp/sample-repo"),
            Path::new("/usr/bin/git"),
            Path::new("/usr/bin/grit"),
            Path::new("/usr/bin/grit-bench"),
            false,
        );
        ctx.cat_blob = Some("deadbeef".into());
        ctx.shortlog_git_range = Some(format!("{GIT_GIT_SHORTLOG_TARGET}..HEAD"));
        ctx
    }

    #[test]
    fn git_patch_id_bench_pipes_show_to_patch_id() {
        let ctx = sample_ctx();
        let (_grit, git) = patch_id_head_bench(&ctx);
        assert_eq!(git.program, PathBuf::from("/bin/sh"));
        let script = git.args.last().expect("shell script");
        assert!(script.contains("show HEAD"));
        assert!(script.contains("patch-id --stable"));
        assert!(!script.contains("patch-id --stable HEAD"));
    }

    #[test]
    fn cat_blob_bench_uses_resolved_blob_oid() {
        let ctx = sample_ctx();
        let (grit, git) = cat_blob_bench(&ctx);
        let grit_script = grit.args.last().expect("grit shell");
        let git_script = git.args.last().expect("git shell");
        assert!(grit_script.contains("deadbeef"));
        assert!(grit_script.contains("cat"));
        assert!(git_script.contains("deadbeef"));
        assert!(git_script.contains("cat-file"));
        assert!(!grit_script.contains("HEAD^{tree}"));
    }

    #[test]
    fn shortlog_git_range_matches_target_branch_config() {
        let ctx = sample_ctx();
        let (_grit, git) = shortlog_ahead_bench(&ctx);
        let script = git.args.last().expect("git shell");
        assert!(script.contains("log"));
        assert!(script.contains("--oneline"));
        assert!(script.contains(&format!("{GIT_GIT_SHORTLOG_TARGET}..HEAD")));
    }

    #[test]
    fn init_empty_prepare_hook_is_wired() {
        let argv = prepare_bench_argv(
            Path::new("/usr/bin/grit-bench"),
            Path::new("/usr/bin/git"),
            crate::scenarios::PrepareKind::WorktreeInitEmpty,
        );
        assert_eq!(argv.args[0], "prepare-worktree-init-empty");
    }
}
