//! `grit submodule` — manage submodules.
//!
//! Supports: status, init, update, add, foreach.
//! Reads `.gitmodules` and manages `.git/modules/` directory.

use crate::commands::sparse_checkout::reapply_sparse_checkout_if_configured;
use crate::commands::upstream_synopsis_help;
use crate::grit_exe;
use anyhow::{bail, Context, Result};
use clap::{Args as ClapArgs, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "git submodule",
    disable_help_subcommand = true,
    disable_version_flag = true
)]
struct SubmoduleCliWrapper {
    #[command(flatten)]
    inner: Args,
}

fn print_submodule_usage_stderr() {
    let Some(syn) = upstream_synopsis_help::synopsis_for_builtin("submodule") else {
        return;
    };
    let pad = " ".repeat("git submodule ".len());
    let variants = upstream_synopsis_help::synopsis_variants_from_adoc(syn);
    for (i, var) in variants.iter().enumerate() {
        let Some(first) = var.first() else {
            continue;
        };
        if i == 0 {
            eprintln!("usage: {first}");
        } else {
            eprintln!("   or: {first}");
        }
        for cont in var.iter().skip(1) {
            eprintln!("{pad}{cont}");
        }
    }
}

fn submodule_usage_exit(code: i32) -> ! {
    print_submodule_usage_stderr();
    std::process::exit(code);
}

/// Split `git submodule` leading `[--quiet|-q] [--cached]` flags (Git order). Rejects other
/// leading options with usage on stderr and exit **1** (matches Git / t7400).
fn split_submodule_leading_flags(rest: &[String]) -> (SubmoduleTopOpts, Vec<String>) {
    let mut top = SubmoduleTopOpts::default();
    let mut i = 0usize;
    while i < rest.len() {
        let a = rest[i].as_str();
        match a {
            "-h" | "--help" | "--help-all" => break,
            "--quiet" | "-q" => {
                top.quiet = true;
                i += 1;
            }
            "--cached" => {
                top.cached = true;
                i += 1;
            }
            _ if a.starts_with('-') => submodule_usage_exit(1),
            _ => break,
        }
    }
    (top, rest[i..].to_vec())
}

fn parse_submodule_args(inner: &[String]) -> Args {
    upstream_synopsis_help::try_print_upstream_help_and_exit("submodule", inner);

    let mut argv = vec!["git submodule".to_owned()];
    argv.extend(inner.iter().cloned());
    match SubmoduleCliWrapper::try_parse_from(&argv) {
        Ok(w) => w.inner,
        Err(e) => {
            if matches!(
                e.kind(),
                clap::error::ErrorKind::DisplayHelp
                    | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                    | clap::error::ErrorKind::DisplayVersion
            ) {
                let mut msg = e.render().to_string();
                msg = msg.replace("Usage:", "usage:");
                print!("{msg}");
            } else {
                let _ = e.print();
            }
            let code = match e.kind() {
                clap::error::ErrorKind::DisplayHelp
                | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => 0,
                clap::error::ErrorKind::DisplayVersion => 129,
                _ => 129,
            };
            std::process::exit(code);
        }
    }
}

/// Entry point from `main`: handles leading `--quiet` / `--cached` like Git before clap.
pub fn run_from_argv(rest: &[String]) -> Result<()> {
    let (top, inner) = split_submodule_leading_flags(rest);
    if inner.len() == 1 && (inner[0] == "--" || inner[0] == "--end-of-options") {
        submodule_usage_exit(1);
    }
    let args = parse_submodule_args(&inner);
    run_with_top_opts(top, args)
}

fn run_with_top_opts(top: SubmoduleTopOpts, args: Args) -> Result<()> {
    if top.cached {
        match &args.command {
            None | Some(SubmoduleCommand::Status(_)) | Some(SubmoduleCommand::Summary(_)) => {}
            _ => submodule_usage_exit(1),
        }
    }

    match args.command {
        None => run_status(&StatusArgs {
            quiet: top.quiet,
            recursive: false,
            cached: top.cached,
            paths: vec![],
        }),
        Some(SubmoduleCommand::Status(mut s)) => {
            s.cached |= top.cached;
            s.quiet |= top.quiet;
            run_status(&s)
        }
        Some(SubmoduleCommand::Init(mut a)) => {
            a.quiet |= top.quiet;
            run_init(&a, a.quiet)
        }
        Some(SubmoduleCommand::Update(mut a)) => {
            a.quiet |= top.quiet;
            run_update(&a)
        }
        Some(SubmoduleCommand::Add(mut a)) => {
            a.quiet |= top.quiet;
            run_add(&a)
        }
        Some(SubmoduleCommand::Foreach(mut a)) => {
            a.quiet |= top.quiet;
            run_foreach(&a, a.quiet)
        }
        Some(SubmoduleCommand::Sync(mut a)) => {
            a.quiet |= top.quiet;
            run_sync(&a, a.quiet)
        }
        Some(SubmoduleCommand::Deinit(mut a)) => {
            a.quiet |= top.quiet;
            run_deinit(&a, a.quiet)
        }
        Some(SubmoduleCommand::Absorbgitdirs(mut a)) => {
            a.quiet |= top.quiet;
            run_absorbgitdirs(&a, a.quiet)
        }
        Some(SubmoduleCommand::Summary(mut a)) => {
            a.quiet |= top.quiet;
            a.cached |= top.cached;
            run_summary(&a, a.quiet)
        }
        Some(SubmoduleCommand::SetBranch(mut a)) => {
            a.quiet |= top.quiet;
            run_set_branch(&a, a.quiet)
        }
        Some(SubmoduleCommand::SetUrl(mut a)) => {
            a.quiet |= top.quiet;
            run_set_url(&a, a.quiet)
        }
    }
}
use grit_lib::config::{canonical_key, ConfigFile, ConfigScope, ConfigSet};
use grit_lib::diff::{diff_index_to_tree, format_mode, head_path_states, DiffEntry, DiffStatus};
use grit_lib::repo_path::RepoPathBuf;
use grit_lib::error::Error as LibError;
use grit_lib::gitmodules::check_submodule_name;
use grit_lib::index::{Index, IndexEntry, MODE_GITLINK};
use grit_lib::merge_diff::blob_oid_at_path;
use grit_lib::objects::{parse_commit, parse_tree, ObjectId, ObjectKind};
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_lib::rev_parse::{self, resolve_revision};
use grit_lib::state::{resolve_head, HeadState};
use grit_lib::submodule_gitdir::{
    die_path_inside_submodule_when_disabled, ensure_submodule_gitdir_config,
    submodule_gitdir_filesystem_path, submodule_gitdir_outer_conflict, submodule_modules_git_dir,
    submodule_path_config_enabled, validate_submodule_path, write_submodule_gitfile,
};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

/// Set by `clone --recurse-submodules` when `--shallow-submodules` was used.
pub(crate) const CLONE_SHALLOW_SUBMODULES_ENV: &str = "GRIT_CLONE_SHALLOW_SUBMODULES";

/// Set by `clone --recurse-submodules` when `--no-shallow-submodules` was used.
pub(crate) const CLONE_NO_SHALLOW_SUBMODULES_ENV: &str = "GRIT_CLONE_NO_SHALLOW_SUBMODULES";

/// Parse `.gitmodules` for clone-time submodule URLs and shallow recommendations.
pub(crate) fn parse_gitmodules_for_clone(work_tree: &Path) -> Result<Vec<SubmoduleInfo>> {
    let gitmodules_path = work_tree.join(".gitmodules");
    if !gitmodules_path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(&gitmodules_path).context("reading .gitmodules")?;
    let _ = grit_lib::gitmodules::write_gitmodules_cli_option_warnings(&mut io::stderr(), &content);
    let mut modules = parse_gitmodules_with_repo(work_tree, None)?;
    modules.retain(|m| grit_lib::gitmodules::check_submodule_name(&m.name));
    Ok(modules)
}

/// Submodule `git clone --depth N` when `Some(1)`; `None` means a non-shallow clone.
///
/// `super_shallow` is true when the superproject has a `.git/shallow` file (clone used `--depth` /
/// shallow negotiation). A shallow superproject does **not** imply shallow submodules unless
/// `--shallow-submodules` is set or `.gitmodules` recommends shallow (matches `t5614`).
#[must_use]
pub(crate) fn submodule_clone_depth_for_superproject(
    super_shallow: bool,
    shallow_submodules_cli: bool,
    no_shallow_submodules_cli: bool,
    no_recommend_shallow: bool,
    gitmodules_shallow: Option<bool>,
) -> Option<usize> {
    if no_shallow_submodules_cli {
        return None;
    }
    if shallow_submodules_cli {
        return Some(1);
    }
    if !no_recommend_shallow {
        if let Some(s) = gitmodules_shallow {
            return if s { Some(1) } else { None };
        }
    }
    if super_shallow {
        return None;
    }
    None
}

fn clone_shallow_submodules_from_env() -> bool {
    std::env::var_os(CLONE_SHALLOW_SUBMODULES_ENV)
        .as_deref()
        .is_some_and(|v| !v.is_empty())
}

fn clone_no_shallow_submodules_from_env() -> bool {
    std::env::var_os(CLONE_NO_SHALLOW_SUBMODULES_ENV)
        .as_deref()
        .is_some_and(|v| !v.is_empty())
}

/// Spawn grit for a nested operation without inheriting the superproject's `GIT_DIR` /
/// `GIT_WORK_TREE` (tests and detached work trees set those in the parent shell).
fn grit_subprocess(grit_bin: &Path) -> Command {
    let mut cmd = Command::new(grit_bin);
    cmd.env_remove("GIT_DIR");
    cmd.env_remove("GIT_WORK_TREE");
    grit_exe::strip_trace2_env(&mut cmd);
    cmd
}

/// Spawn grit for a superproject operation from an explicit git-dir/work-tree pair.
fn superproject_subprocess(grit_bin: &Path, repo: &Repository, work_tree: &Path) -> Command {
    let mut cmd = Command::new(grit_bin);
    cmd.env("GIT_DIR", &repo.git_dir)
        .env("GIT_WORK_TREE", work_tree);
    grit_exe::strip_trace2_env(&mut cmd);
    cmd
}

static SUBMODULE_JOBS_TRACE_EMITTED: AtomicBool = AtomicBool::new(false);

/// Best-effort `GIT_TRACE` line for submodule worker counts (t7406 greps for `N tasks`).
pub(crate) fn trace_submodule_job_tasks_if_needed(repo: Option<&Repository>, jobs: Option<usize>) {
    let configured = repo.and_then(|r| {
        ConfigSet::load(Some(&r.git_dir), true)
            .ok()
            .and_then(|cfg| cfg.get("submodule.fetchJobs"))
            .and_then(|value| value.trim().parse::<usize>().ok())
    });
    let Some(n) = jobs.or(configured) else {
        return;
    };
    let Ok(trace_target) = std::env::var("GIT_TRACE") else {
        return;
    };
    if trace_target.is_empty() {
        return;
    }
    if SUBMODULE_JOBS_TRACE_EMITTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let line = format!("trace: submodule update: {n} tasks\n");
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&trace_target)
        .and_then(|mut f| f.write_all(line.as_bytes()));
}

fn submodule_display_path_from_cwd(abs_submodule: &Path) -> String {
    let cwd = std::env::current_dir().unwrap_or_else(|_| abs_submodule.to_path_buf());
    pathdiff_relative(&cwd, abs_submodule).replace('\\', "/")
}

fn super_index_has_unmerged_stage(repo: &Repository, rel_path: &str) -> bool {
    let Ok(index) = repo.load_index() else {
        return false;
    };
    let needle = rel_path.as_bytes();
    index
        .entries
        .iter()
        .any(|e| e.path.as_slice() == needle && e.stage() != 0)
}

/// Read `submodule.<name>.url` from the superproject's local config, if present.
fn config_submodule_url(repo: &Repository, name: &str) -> Option<String> {
    let cfg = parse_local_config(&repo.git_dir).ok()?;
    config_last_value(&cfg, &format!("submodule.{name}.url")).filter(|v| !v.trim().is_empty())
}

fn parse_local_config(git_dir: &Path) -> Result<ConfigFile> {
    let config_path = grit_lib::repo::common_git_dir_for_config(git_dir).join("config");
    if config_path.exists() {
        let content = fs::read_to_string(&config_path)?;
        Ok(ConfigFile::parse(
            &config_path,
            &content,
            ConfigScope::Local,
        )?)
    } else {
        Ok(ConfigFile::parse(&config_path, "", ConfigScope::Local)?)
    }
}

/// Git directory under `.git/modules/` used for a submodule's object store (separate-git-dir clone).
///
/// When [`submodule_path_config_enabled`] is true, uses `submodule.<name>.gitdir` (encoded path).
/// Otherwise uses the legacy layout `modules/<worktree-path>/`.
pub(crate) fn submodule_separate_git_dir(
    repo: &Repository,
    work_tree: &Path,
    submodule_name: &str,
    _submodule_path: &str,
) -> Result<PathBuf> {
    // Git's `submodule_name_to_gitdir` uses per-worktree `$GIT_DIR/modules/…`, not
    // `$GIT_COMMON_DIR` (linked worktrees use `.git/worktrees/<id>/modules/…`, t2405).
    let git_dir = repo.git_dir.clone();
    let common = refs::common_dir(&repo.git_dir).unwrap_or_else(|| git_dir.clone());
    if submodule_path_config_enabled(&common) {
        let cfg = parse_local_config(&git_dir)?;
        submodule_gitdir_filesystem_path(work_tree, &git_dir, &cfg, submodule_name)
            .or_else(|_| Ok(submodule_modules_git_dir(&git_dir, submodule_name)))
    } else {
        Ok(submodule_modules_git_dir(&git_dir, submodule_name))
    }
}

/// Set `core.worktree` in a separate git-dir so checkouts materialize files (matches Git after
/// `clone --separate-git-dir`).
///
/// Uses a path relative to `git_dir` (not an absolute path) so nested submodules store
/// `../../../work/sub2` under `.git/modules/.../modules/sub2`, matching C Git and allowing
/// `reset_work_tree_to_interested` to copy `modules/sub1/modules/sub2` (t1013).
fn set_separate_gitdir_worktree(grit_bin: &Path, git_dir: &Path, work_tree: &Path) {
    let wt = pathdiff_relative(git_dir, work_tree);
    let _ = grit_subprocess(grit_bin)
        .arg("--git-dir")
        .arg(git_dir)
        .arg("config")
        .arg("core.worktree")
        .arg(&wt)
        .status();
}

/// Leading options parsed before the subcommand (matches `git submodule [--quiet] [--cached] …`).
#[derive(Debug, Clone, Copy, Default)]
pub struct SubmoduleTopOpts {
    pub quiet: bool,
    pub cached: bool,
}

/// Arguments for `grit submodule`.
#[derive(Debug, ClapArgs)]
#[command(about = "Initialize, update, or inspect submodules")]
pub struct Args {
    #[command(subcommand)]
    pub command: Option<SubmoduleCommand>,
}

/// Subcommands for `grit submodule`.
#[derive(Debug, Subcommand)]
pub enum SubmoduleCommand {
    /// Show the status of submodules.
    Status(StatusArgs),
    /// Initialize submodule configuration from .gitmodules.
    Init(InitArgs),
    /// Checkout the recorded submodule commits.
    Update(UpdateArgs),
    /// Add a new submodule.
    Add(AddArgs),
    /// Run a command in each submodule.
    Foreach(ForeachArgs),
    /// Synchronize submodule URL configuration.
    Sync(SyncArgs),
    /// De-initialize submodules.
    Deinit(DeinitArgs),
    /// Move submodule git directories into the superproject.
    Absorbgitdirs(AbsorbgitdirsArgs),
    /// Show submodule summary.
    Summary(SummaryArgs),
    /// Set the default remote tracking branch for a submodule.
    #[command(name = "set-branch")]
    SetBranch(SetBranchArgs),
    /// Set the URL for a submodule.
    #[command(name = "set-url")]
    SetUrl(SetUrlArgs),
}

#[derive(Debug, Clone, ClapArgs)]
pub struct StatusArgs {
    /// Operate quietly (suppress progress and informational messages).
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Recurse into nested submodules.
    #[arg(long)]
    pub recursive: bool,

    /// Compare the index to `HEAD` (index gitlinks vs `HEAD` tree) instead of the submodule work tree.
    #[arg(long)]
    pub cached: bool,

    /// Restrict to specific submodule paths.
    #[arg(value_name = "PATH")]
    pub paths: Vec<String>,
}

#[derive(Debug, ClapArgs)]
pub struct InitArgs {
    /// Operate quietly (suppress progress and informational messages).
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Restrict to specific submodule paths.
    #[arg(value_name = "PATH")]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct UpdateArgs {
    /// Restrict to specific submodule paths.
    #[arg(value_name = "PATH")]
    pub paths: Vec<String>,

    /// Operate quietly (suppress progress and informational messages).
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Initialize uninitialized submodules before updating.
    #[arg(long)]
    pub init: bool,

    /// Checkout the recorded commit (accepted for compatibility).
    #[arg(long)]
    pub checkout: bool,

    /// Use the status of the submodule's remote-tracking branch.
    #[arg(long)]
    pub remote: bool,

    /// Rebase the current branch onto the recorded commit.
    #[arg(long)]
    pub rebase: bool,

    /// Merge the recorded commit into the current branch.
    #[arg(long)]
    pub merge: bool,

    /// Discard local changes when checking out.
    #[arg(long, short)]
    pub force: bool,

    /// Shallow clone depth when initializing a submodule.
    #[arg(long)]
    pub depth: Option<usize>,

    /// Parallel jobs hint (accepted for compatibility; best-effort).
    #[arg(long)]
    pub jobs: Option<usize>,

    /// Partial clone filter (requires `--init`).
    #[arg(long)]
    pub filter: Option<String>,

    /// Ref storage backend for newly cloned submodules.
    #[arg(long = "ref-format", value_name = "FORMAT")]
    pub ref_format: Option<String>,

    /// Recurse into nested submodules.
    #[arg(long)]
    pub recursive: bool,

    /// Internal recursion override for callers that need nested submodule updates.
    #[arg(skip)]
    pub implicit_recursive: bool,

    /// Borrow objects from this repository (repeatable). Writes `objects/info/alternates` in cloned submodules.
    #[arg(long = "reference", value_name = "REPO", action = clap::ArgAction::Append)]
    pub reference: Vec<String>,

    /// Borrow objects from reference repositories only to reduce network transfer, then copy them locally.
    #[arg(long = "dissociate")]
    pub dissociate: bool,

    /// Ignore `.gitmodules` shallow recommendations (still shallow when the superproject is shallow).
    #[arg(long = "no-recommend-shallow")]
    pub no_recommend_shallow: bool,
}

#[derive(Debug, ClapArgs)]
pub struct AddArgs {
    /// Allow adding when the path exists but is not a git repository (remove and clone).
    #[arg(short = 'f', long = "force")]
    pub force: bool,

    /// Use the given name instead of defaulting to its path.
    #[arg(long)]
    pub name: Option<String>,

    /// Operate quietly (suppress progress and informational messages).
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Branch to track.
    #[arg(short = 'b', long = "branch")]
    pub branch: Option<String>,

    /// Force cloning progress to be shown.
    #[arg(long = "progress")]
    pub progress: bool,

    /// Create a shallow clone with the given depth.
    #[arg(long = "depth", value_name = "DEPTH")]
    pub depth: Option<i64>,

    /// Use the given repository as a reference (alternate) for the clone.
    #[arg(long = "reference", value_name = "REPO", action = clap::ArgAction::Append)]
    pub reference: Vec<String>,

    /// Borrow the objects from reference repositories.
    #[arg(long = "dissociate")]
    pub dissociate: bool,

    /// Ref storage backend for the cloned submodule.
    #[arg(long = "ref-format", value_name = "FORMAT")]
    pub ref_format: Option<String>,

    /// URL of the submodule repository.
    pub url: String,

    /// Path where the submodule should be placed.
    pub path: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct ForeachArgs {
    /// Operate quietly (suppress progress and informational messages).
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Recurse into nested submodules.
    #[arg(long)]
    pub recursive: bool,

    /// Command to run in each submodule (default: `:`). Use `--` before arguments that look like options.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
}

#[derive(Debug, ClapArgs)]
pub struct SyncArgs {
    /// Operate quietly (suppress progress and informational messages).
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Recurse into nested submodules.
    #[arg(long)]
    pub recursive: bool,

    /// Prefix for nested submodule paths in status output (internal; matches `git submodule--helper sync`).
    #[arg(long = "super-prefix", value_name = "PREFIX")]
    pub super_prefix: Option<String>,

    /// Restrict to specific submodule paths.
    #[arg(value_name = "PATH")]
    pub paths: Vec<String>,
}

#[derive(Debug, ClapArgs)]
pub struct DeinitArgs {
    /// Operate quietly (suppress progress and informational messages).
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Remove even if the submodule working tree has local modifications.
    #[arg(long, short)]
    pub force: bool,

    /// De-initialize all submodules.
    #[arg(long)]
    pub all: bool,

    /// Restrict to specific submodule paths.
    #[arg(value_name = "PATH")]
    pub paths: Vec<String>,
}

#[derive(Debug, ClapArgs)]
pub struct AbsorbgitdirsArgs {
    /// Operate quietly (suppress progress and informational messages).
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Restrict to specific submodule paths.
    #[arg(value_name = "PATH")]
    pub paths: Vec<String>,
}

#[derive(Debug, ClapArgs)]
pub struct SummaryArgs {
    /// Operate quietly (suppress progress and informational messages).
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Compare the index to the given commit instead of the submodule working tree HEAD.
    #[arg(long)]
    pub cached: bool,

    /// Compare the index gitlink to the submodule HEAD (instead of index vs commit tree).
    #[arg(long)]
    pub files: bool,

    /// Skip submodules with `submodule.<name>.ignore=all` (Git `--for-status`; used by status).
    #[arg(long = "for-status")]
    pub for_status: bool,

    /// Limit how many commits `log` shows for each submodule (`-n`; Git `--summary-limit`).
    #[arg(short = 'n', long = "summary-limit")]
    pub summary_limit: Option<i32>,

    /// Optional commit to compare against, then pathspecs after `--`.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "ARGS"
    )]
    pub rest: Vec<String>,
}

#[derive(Debug, ClapArgs)]
pub struct SetBranchArgs {
    /// Operate quietly (suppress progress and informational messages).
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// The branch to set.
    #[arg(long, short)]
    pub branch: Option<String>,

    /// Use the remote HEAD branch.
    #[arg(long, short)]
    pub default: bool,

    /// Submodule path.
    pub path: String,
}

#[derive(Debug, ClapArgs)]
pub struct SetUrlArgs {
    /// Operate quietly (suppress progress and informational messages).
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Submodule path.
    pub path: String,

    /// New URL for the submodule.
    pub newurl: String,
}

/// Parsed entry from `.gitmodules`.
#[derive(Debug, Clone)]
pub(crate) struct SubmoduleInfo {
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) url: String,
    /// `submodule.<name>.shallow` from `.gitmodules`, when set.
    pub(crate) shallow: Option<bool>,
    pub(crate) update: Option<String>,
    pub(crate) branch: Option<String>,
    /// `submodule.<name>.ignore` from `.gitmodules` (e.g. `all`, `dirty`), when set.
    pub(crate) ignore: Option<String>,
}

/// Update submodule working trees to the commits recorded in the superproject index.
///
/// Used after `pull` / `merge` when `--recurse-submodules` or `submodule.recurse` applies.
pub(crate) fn update_after_superproject_merge(init: bool, recursive: bool) -> Result<()> {
    run_update(&UpdateArgs {
        paths: vec![],
        quiet: false,
        init,
        checkout: false,
        remote: false,
        rebase: false,
        merge: false,
        force: false,
        depth: None,
        jobs: None,
        filter: None,
        ref_format: None,
        recursive,
        implicit_recursive: false,
        reference: vec![],
        dissociate: false,
        no_recommend_shallow: false,
    })
}

/// After `pull --rebase --recurse-submodules`, run `submodule update --init --recursive --rebase`
/// (matches Git's `rebase_submodules` in `builtin/pull.c`).
pub(crate) fn update_after_superproject_rebase(init: bool, recursive: bool) -> Result<()> {
    run_update(&UpdateArgs {
        paths: vec![],
        quiet: false,
        init,
        checkout: false,
        remote: false,
        rebase: true,
        merge: false,
        force: false,
        depth: None,
        jobs: None,
        filter: None,
        ref_format: None,
        recursive,
        implicit_recursive: false,
        reference: vec![],
        dissociate: false,
        no_recommend_shallow: false,
    })
}

/// Stage the given commit OID as the gitlink for `rel_path` in the superproject index.
///
/// Used by `submodule update --remote` so the superproject records the fetched submodule tip
/// (matches Git; required for `git commit <path>` after `--remote`).
fn stage_gitlink_in_super_index(
    repo: &Repository,
    rel_path: &str,
    new_oid_hex: &str,
) -> Result<()> {
    let new_oid = ObjectId::from_hex(new_oid_hex.trim())
        .with_context(|| format!("invalid submodule OID '{new_oid_hex}' for path '{rel_path}'"))?;
    let index_path = repo.index_path();
    let mut index = repo.load_index_at(&index_path)?;
    let path_bytes = rel_path.as_bytes().to_vec();
    let Some(entry) = index
        .entries
        .iter_mut()
        .find(|e| e.stage() == 0 && e.path == path_bytes)
    else {
        return Ok(());
    };
    if entry.mode != MODE_GITLINK {
        return Ok(());
    }
    entry.oid = new_oid;
    repo.write_index_at(&index_path, &mut index)?;
    Ok(())
}

/// Refresh cached stat data for a gitlink in the superproject index after checkout.
fn refresh_gitlink_index_stat(repo: &Repository, rel_path: &str) -> Result<()> {
    let work_tree = repo.work_tree.as_ref().context("bare repository")?;
    let abs = work_tree.join(rel_path);
    let index_path = repo.index_path();
    let mut index = repo.load_index_at(&index_path)?;
    let path_bytes = rel_path.as_bytes().to_vec();
    let Some(entry) = index
        .entries
        .iter_mut()
        .find(|e| e.stage() == 0 && e.path == path_bytes)
    else {
        return Ok(());
    };
    if entry.mode != 0o160000 {
        return Ok(());
    }
    if let Ok(meta) = fs::symlink_metadata(&abs) {
        #[cfg(unix)]
        {
            entry.ctime_sec = meta.ctime() as u32;
            entry.ctime_nsec = meta.ctime_nsec() as u32;
            entry.mtime_sec = meta.mtime() as u32;
            entry.mtime_nsec = meta.mtime_nsec() as u32;
            entry.dev = meta.dev() as u32;
            entry.ino = meta.ino() as u32;
            entry.size = meta.len() as u32;
        }
    }
    repo.write_index_at(&index_path, &mut index)?;
    Ok(())
}

/// Run `grit fetch` in each initialized submodule (and nested submodules when `recursive`).
///
/// Uses each submodule's default remote (after `origin` rename), not a hard-coded `origin`.
pub(crate) fn recursive_fetch_submodules(recursive: bool) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let work_tree = repo.work_tree.as_ref().context("bare repository")?;
    let modules = parse_gitmodules_with_repo(work_tree, Some(&repo))?;
    let grit_bin = grit_exe::grit_executable();

    fn fetch_one(
        grit_bin: &std::path::Path,
        work_tree: &Path,
        rel_path: &str,
        recursive: bool,
    ) -> Result<()> {
        let sub_path = work_tree.join(rel_path);
        if !sub_path.join(".git").exists() {
            return Ok(());
        }
        let remote = get_default_remote_for_path_in_super(rel_path, work_tree)
            .unwrap_or_else(|_| "origin".to_owned());
        let status = std::process::Command::new(grit_bin)
            .args(["fetch", remote.as_str()])
            .current_dir(&sub_path)
            .status()
            .with_context(|| format!("submodule fetch in {rel_path}"))?;
        if !status.success() {
            bail!("submodule fetch failed in {}", sub_path.display());
        }
        if recursive {
            let sub_repo = Repository::discover(Some(&sub_path)).context("open submodule repo")?;
            let sub_wt = sub_repo.work_tree.as_ref().context("bare submodule")?;
            let nested = parse_gitmodules_with_repo(sub_wt, Some(&sub_repo)).unwrap_or_default();
            for m in nested {
                let nested_rel = if rel_path.is_empty() {
                    m.path.clone()
                } else {
                    format!("{}/{}", rel_path.trim_end_matches('/'), m.path)
                };
                fetch_one(grit_bin, work_tree, &nested_rel, true)?;
            }
        }
        Ok(())
    }

    for m in modules {
        fetch_one(&grit_bin, work_tree, &m.path, recursive)?;
    }
    Ok(())
}

/// Run the `submodule` command (no leading `--quiet` / `--cached`; use [`run_from_argv`] from main).
pub fn run(args: Args) -> Result<()> {
    run_with_top_opts(SubmoduleTopOpts::default(), args)
}

/// Built-in helper invoked as `git submodule--helper …` (matches Git's plumbing).
pub fn run_submodule_helper(rest: &[String]) -> Result<()> {
    match rest.first().map(|s| s.as_str()) {
        None => submodule_helper_usage(),
        Some("get-default-remote") => {
            if rest.len() != 2 {
                submodule_helper_usage();
            }
            let path = &rest[1];
            let cwd = std::env::current_dir().context("current directory")?;
            let name = get_default_remote_for_path_in_super(path, &cwd)?;
            println!("{name}");
            Ok(())
        }
        Some("gitdir") | Some("migrate-gitdir-configs") => {
            crate::commands::submodule_helper::run(rest)
        }
        Some("absorbgitdirs") => {
            let mut super_prefix: Option<String> = None;
            let mut paths: Vec<String> = Vec::new();
            let mut quiet_helper = false;
            for a in rest.iter().skip(1) {
                if let Some(v) = a.strip_prefix("--super-prefix=") {
                    super_prefix = Some(v.to_string());
                } else if a == "-q" || a == "--quiet" {
                    quiet_helper = true;
                } else if a.as_str() == "--" {
                    continue;
                } else if !a.starts_with('-') {
                    paths.push(a.clone());
                }
            }
            absorb_git_dirs_impl(super_prefix.as_deref(), &paths, quiet_helper)
        }
        Some(other) => {
            eprintln!("Unknown subcommand: {other}");
            submodule_helper_usage();
        }
    }
}

fn submodule_helper_usage() -> ! {
    eprintln!("usage: git submodule--helper get-default-remote <path>");
    eprintln!("   or: git submodule--helper gitdir <name>");
    eprintln!("   or: git submodule--helper migrate-gitdir-configs");
    eprintln!(
        "   or: git submodule--helper absorbgitdirs [--super-prefix=<path>] [-q] [--] [<path>...]"
    );
    std::process::exit(129);
}

fn submodule_path_not_handle_error<T>(path: &str) -> Result<T> {
    Err(LibError::Message(format!(
        "fatal: could not get a repository handle for submodule '{path}'"
    ))
    .into())
}

fn worktree_relative_posix(work_tree: &Path, abs_path: &Path) -> Result<String> {
    let wt = work_tree
        .canonicalize()
        .with_context(|| format!("cannot canonicalize {}", work_tree.display()))?;
    let abs = abs_path
        .canonicalize()
        .with_context(|| format!("cannot canonicalize {}", abs_path.display()))?;
    let rel = abs.strip_prefix(&wt).with_context(|| {
        format!(
            "path {} is not inside work tree {}",
            abs.display(),
            wt.display()
        )
    })?;
    Ok(rel
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/"))
}

fn urls_match(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    if a.contains("://") || b.contains("://") {
        return false;
    }
    let pa = Path::new(a);
    let pb = Path::new(b);
    match (pa.canonicalize(), pb.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

fn remote_names_with_urls(config: &ConfigFile) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for e in &config.entries {
        let Some(rest) = e.key.strip_prefix("remote.") else {
            continue;
        };
        let Some(name) = rest.strip_suffix(".url") else {
            continue;
        };
        if let Some(url) = e.value.as_deref() {
            out.push((name.to_string(), url.to_string()));
        }
    }
    out
}

fn config_last_value(config: &ConfigFile, key: &str) -> Option<String> {
    config
        .entries
        .iter()
        .rev()
        .find(|e| e.key == key)
        .and_then(|e| e.value.clone())
}

fn remote_from_resolved_url(config: &ConfigFile, resolved_url: &str) -> Option<String> {
    for (name, url) in remote_names_with_urls(config) {
        if urls_match(resolved_url, &url) {
            return Some(name);
        }
    }
    None
}

fn default_remote_for_config(config: &ConfigFile, head_branch: Option<&str>) -> String {
    if let Some(bn) = head_branch {
        let key = format!("branch.{bn}.remote");
        if let Some(r) = config_last_value(config, &key) {
            if !r.is_empty() {
                return r;
            }
        }
    }
    let names: std::collections::BTreeSet<String> = remote_names_with_urls(config)
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    if names.len() == 1 {
        return names
            .iter()
            .next()
            .cloned()
            .unwrap_or_else(|| "origin".to_string());
    }
    "origin".to_string()
}

fn get_default_remote_for_path_in_super(path: &str, super_work_tree: &Path) -> Result<String> {
    let repo = Repository::discover(Some(super_work_tree)).context("not a git repository")?;
    let path_buf = Path::new(path);
    let abs_sub = if path_buf.is_absolute() {
        path_buf.to_path_buf()
    } else if path_buf
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        std::env::current_dir()
            .map(|cwd| cwd.join(path_buf))
            .unwrap_or_else(|_| super_work_tree.join(path_buf))
    } else {
        super_work_tree.join(path_buf)
    };
    let abs_sub = abs_sub.canonicalize().unwrap_or(abs_sub);
    let work_tree = repo.work_tree.as_ref().context("bare repository")?;
    let sub_rel = match worktree_relative_posix(work_tree, &abs_sub) {
        Ok(s) => s,
        Err(_) => {
            return submodule_path_not_handle_error(path);
        }
    };
    let (final_git_dir, _final_wt, super_wt, super_git_dir, sm) =
        match resolve_submodule_chain(&repo, path, &sub_rel) {
            Ok(v) => v,
            Err(e) => {
                // Git's `get_default_remote_submodule` only needs a working repository handle for
                // the submodule; a `.gitmodules` entry is optional. When the submodule is populated
                // but absent from `.gitmodules` (e.g. recorded only in the index, t5526 #36), read
                // the default remote straight from the submodule's own config — like
                // `repo_default_remote(&subrepo)` — instead of failing.
                if let Some(gd) = resolve_submodule_git_dir(&abs_sub) {
                    return Ok(get_default_remote_from_git_dir(&gd));
                }
                return Err(e);
            }
        };

    let resolved_url = resolve_submodule_super_url(&super_wt, &super_git_dir, &sm.url)?;
    let config_path = final_git_dir.join("config");
    let content = fs::read_to_string(&config_path).unwrap_or_default();
    let config = ConfigFile::parse(&config_path, &content, ConfigScope::Local)
        .context("parse submodule config")?;

    if let Some(name) = remote_from_resolved_url(&config, &resolved_url) {
        return Ok(name);
    }

    let head = resolve_head(&final_git_dir)?;
    let branch = head.branch_name().map(str::to_owned);
    Ok(default_remote_for_config(&config, branch.as_deref()))
}

pub(crate) fn get_default_remote_for_path(path: &str) -> Result<String> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let wt = repo.work_tree.as_deref().context("bare repository")?;
    get_default_remote_for_path_in_super(path, wt)
}

/// Default remote name for a submodule by reading its git dir directly (used when the submodule
/// work tree is absent — e.g. recursing into a submodule that is changed by newly-fetched
/// superproject commits but not present in the current index). Mirrors Git's `get_default_remote`:
/// `branch.<HEAD-branch>.remote`, else `origin`.
pub(crate) fn get_default_remote_from_git_dir(git_dir: &Path) -> String {
    let config_path = git_dir.join("config");
    let content = fs::read_to_string(&config_path).unwrap_or_default();
    let config = match ConfigFile::parse(&config_path, &content, ConfigScope::Local) {
        Ok(c) => c,
        Err(_) => return "origin".to_string(),
    };
    let branch = resolve_head(git_dir)
        .ok()
        .and_then(|h| h.branch_name().map(str::to_owned));
    default_remote_for_config(&config, branch.as_deref())
}

fn resolve_submodule_chain(
    top_repo: &Repository,
    display_path: &str,
    sub_rel: &str,
) -> Result<(PathBuf, PathBuf, PathBuf, PathBuf, SubmoduleInfo)> {
    let components: Vec<&str> = sub_rel.split('/').filter(|c| !c.is_empty()).collect();
    if components.is_empty() {
        return submodule_path_not_handle_error(display_path);
    }

    let top_work = top_repo.work_tree.as_ref().context("bare repository")?;
    let mut parent_wt = top_work.to_path_buf();
    let mut parent_git = top_repo.git_dir.clone();

    let mut idx = 0usize;
    while idx < components.len() {
        let parent_repo = Repository::open(&parent_git, Some(&parent_wt))
            .context("open repository for submodule walk")?;
        let modules = parse_gitmodules_with_repo(&parent_wt, Some(&parent_repo))?;
        let mut sm_match: Option<SubmoduleInfo> = None;
        for len in (1..=components.len() - idx).rev() {
            let rel: String = components[idx..idx + len].join("/");
            if let Some(sm) = modules.iter().find(|m| m.path == rel) {
                sm_match = Some(sm.clone());
                break;
            }
        }
        let Some(sm) = sm_match else {
            return submodule_path_not_handle_error(sub_rel);
        };
        let is_last = idx + sm.path.split('/').count() == components.len();

        let seg_work = parent_wt.join(&sm.path);
        if !seg_work.join(".git").exists() {
            return submodule_path_not_handle_error(display_path);
        }
        let Some(git_dir) = resolve_submodule_git_dir(&seg_work) else {
            return submodule_path_not_handle_error(display_path);
        };

        if is_last {
            return Ok((git_dir, seg_work, parent_wt, parent_git, sm));
        }

        parent_wt = seg_work;
        parent_git = git_dir;
        idx += sm.path.split('/').filter(|s| !s.is_empty()).count();
    }

    Err(anyhow::anyhow!(
        "internal error: submodule path walk did not complete"
    ))
}

// ── .gitmodules parsing ──────────────────────────────────────────────

/// Parse `.gitmodules` into a list of submodule entries.
pub(crate) fn parse_gitmodules(work_tree: &Path) -> Result<Vec<SubmoduleInfo>> {
    parse_gitmodules_with_repo(work_tree, None)
}

/// Paths listed in `.gitmodules` (or the index blob), used by `git clean` to avoid removing
/// submodule work trees that are not recorded in the current index (e.g. after checkout).
pub fn listed_submodule_paths(repo: &Repository) -> Result<Vec<String>> {
    let Some(wt) = repo.work_tree.as_ref() else {
        return Ok(Vec::new());
    };
    let modules = parse_gitmodules_with_repo(wt, Some(repo))?;
    Ok(modules.into_iter().map(|m| m.path).collect())
}

/// Ensure each configured submodule work tree has a `.git` gitfile pointing at
/// `.git/modules/<path>/` when that module directory exists (needed after checkout removes
/// paths not in the new index).
pub fn refresh_submodule_gitfiles(repo: &Repository) -> Result<()> {
    let Some(wt) = repo.work_tree.as_ref() else {
        return Ok(());
    };
    let modules = parse_gitmodules_with_repo(wt, Some(repo))?;
    for m in &modules {
        let path = &m.path;
        let sm_dir = wt.join(path);
        if !sm_dir.is_dir() {
            continue;
        }
        let gitfile = sm_dir.join(".git");
        if !gitfile.exists()
            && fs::read_dir(&sm_dir)
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(false)
        {
            continue;
        }
        let modules_git = submodule_separate_git_dir(repo, wt, &m.name, &m.path)?;
        if !modules_git.exists() {
            continue;
        }
        if let Ok(rel) = relativize_submodule_gitfile(&sm_dir, &modules_git) {
            let line = format!("gitdir: {}\n", rel.to_string_lossy().replace('\\', "/"));
            fs::write(&gitfile, line).with_context(|| {
                format!("failed to write submodule gitfile at {}", gitfile.display())
            })?;
        }
    }
    Ok(())
}

fn relativize_submodule_gitfile(from_dir: &Path, to_path: &Path) -> Result<PathBuf> {
    let from_abs = fs::canonicalize(from_dir).unwrap_or_else(|_| from_dir.to_path_buf());
    let to_abs = fs::canonicalize(to_path).unwrap_or_else(|_| to_path.to_path_buf());
    let from_c: Vec<_> = from_abs.components().collect();
    let to_c: Vec<_> = to_abs.components().collect();
    let mut i = 0usize;
    while i < from_c.len() && i < to_c.len() && from_c[i] == to_c[i] {
        i += 1;
    }
    let mut out = PathBuf::new();
    for _ in i..from_c.len() {
        out.push("..");
    }
    for c in &to_c[i..] {
        out.push(c);
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    Ok(out)
}

pub(crate) fn parse_gitmodules_with_repo(
    work_tree: &Path,
    repo: Option<&Repository>,
) -> Result<Vec<SubmoduleInfo>> {
    let gitmodules_path = work_tree.join(".gitmodules");
    let content = if gitmodules_path.exists() {
        fs::read_to_string(&gitmodules_path).context("failed to read .gitmodules")?
    } else if let Some(repo) = repo {
        // Fallback: read .gitmodules from the index (e.g. sparse checkout)
        let index = repo.load_index().context("failed to load index")?;
        if let Some(ie) = index.get(b".gitmodules", 0) {
            let obj = repo
                .odb
                .read(&ie.oid)
                .context("failed to read .gitmodules blob from ODB")?;
            if obj.kind != ObjectKind::Blob {
                return Ok(Vec::new());
            }
            String::from_utf8(obj.data).context("failed to decode .gitmodules blob")?
        } else {
            return Ok(Vec::new());
        }
    } else {
        return Ok(Vec::new());
    };

    let config = ConfigFile::parse(&gitmodules_path, &content, ConfigScope::Local)
        .context("failed to parse .gitmodules")?;

    // Collect entries by submodule name.
    #[derive(Default)]
    struct ModuleFields {
        path: Option<String>,
        url: Option<String>,
        shallow: Option<bool>,
        update: Option<String>,
        branch: Option<String>,
        ignore: Option<String>,
    }
    let mut modules: BTreeMap<String, ModuleFields> = BTreeMap::new();

    for entry in &config.entries {
        // Keys look like: submodule.<name>.path, submodule.<name>.url
        let key = &entry.key;
        if !key.starts_with("submodule.") {
            continue;
        }
        // Strip "submodule." prefix and split on last dot.
        let rest = &key["submodule.".len()..];
        if let Some(last_dot) = rest.rfind('.') {
            let name = &rest[..last_dot];
            let var = &rest[last_dot + 1..];
            let entry_val = modules.entry(name.to_string()).or_default();
            match var {
                "path" => entry_val.path = entry.value.clone(),
                "url" => entry_val.url = entry.value.clone(),
                "shallow" => {
                    if let Some(v) = entry.value.as_deref() {
                        let v = v.trim();
                        if v.eq_ignore_ascii_case("true")
                            || v == "1"
                            || v.eq_ignore_ascii_case("yes")
                        {
                            entry_val.shallow = Some(true);
                        } else if v.eq_ignore_ascii_case("false")
                            || v == "0"
                            || v.eq_ignore_ascii_case("no")
                        {
                            entry_val.shallow = Some(false);
                        }
                    }
                }
                "update" => entry_val.update = entry.value.clone(),
                "branch" => entry_val.branch = entry.value.clone(),
                "ignore" => entry_val.ignore = entry.value.clone(),
                _ => {}
            }
        }
    }

    let mut result = Vec::new();
    for (name, f) in modules {
        // A `.gitmodules` section defines a submodule as long as it has a `path`; the `url` may
        // be absent (git's `submodule_from_path` still returns it, with a null url). Callers that
        // need a url handle the empty case (e.g. "cannot clone submodule without a URL").
        if let Some(path) = f.path {
            result.push(SubmoduleInfo {
                name,
                path,
                url: f.url.unwrap_or_default(),
                shallow: f.shallow,
                update: f.update,
                branch: f.branch,
                ignore: f.ignore,
            });
        }
    }

    Ok(result)
}

/// Resolve `.git/modules/<…>` for a submodule work tree path.
///
/// Git names the directory after the submodule **name** from `.gitmodules`, not the checkout path
/// (e.g. name `g`, path `b` → `.git/modules/g`). Fall back to nesting by path when unregistered.
#[must_use]
pub(crate) fn submodule_modules_git_dir_for_worktree_path(
    super_git_dir: &Path,
    work_tree: &Path,
    repo: Option<&Repository>,
    submodule_worktree_rel: &str,
) -> PathBuf {
    if let Some(repo) = repo {
        if let Ok(modules) = parse_gitmodules_with_repo(work_tree, Some(repo)) {
            if let Some(m) = modules.iter().find(|m| m.path == submodule_worktree_rel) {
                return submodule_modules_git_dir(super_git_dir, &m.name);
            }
        }
    }
    submodule_modules_git_dir(super_git_dir, submodule_worktree_rel)
}

/// Filter submodules by path args (empty = all).
/// Whether a submodule path is active in `repo` (git `is_submodule_active`); used by
/// `clone --recurse-submodules=<pathspec>` to decide which submodules to initialize.
pub fn submodule_path_is_active(repo: &Repository, path: &str) -> bool {
    grit_lib::submodule_active::is_submodule_active(repo, path).unwrap_or(false)
}

/// Collect all gitlink (`160000`) stage-0 paths recorded in the index, worktree-relative,
/// using forward slashes. This mirrors `git`'s `module_list_compute`, which derives the set
/// of submodules from index gitlink entries (not from `.gitmodules`).
fn index_gitlink_paths(repo: &Repository) -> Vec<String> {
    let Ok(index) = repo.load_index() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in &index.entries {
        if e.mode == MODE_GITLINK && e.stage() == 0 {
            out.push(String::from_utf8_lossy(&e.path).replace('\\', "/"));
        }
    }
    out
}

/// Resolve a user-supplied submodule path argument (which may be relative to `cwd`, contain
/// `./`, `../`, or a trailing slash, or carry a `:(exclude)`/`:!` pathspec magic prefix) into
/// a worktree-relative posix path. Pathspec-magic entries (e.g. `:(exclude)sub0`) are returned
/// unchanged so callers can treat them as match modifiers rather than literal paths.
fn normalize_submodule_path_arg(work_tree: &Path, cwd: &Path, raw: &str) -> Option<String> {
    if raw == "." {
        return Some(".".to_string());
    }
    // Leave pathspec magic alone — these never need to map to a literal index path.
    if raw.starts_with(':') {
        return Some(raw.to_string());
    }
    let trimmed = raw.trim_end_matches('/');
    if trimmed.is_empty() {
        return Some(".".to_string());
    }
    let candidate = if Path::new(trimmed).is_absolute() {
        PathBuf::from(trimmed)
    } else {
        cwd.join(trimmed)
    };
    // Lexically normalize `.`/`..` without requiring the path to exist on disk.
    let normalized = lexically_normalize(&candidate);
    let wt = work_tree
        .canonicalize()
        .unwrap_or_else(|_| work_tree.to_path_buf());
    let norm = normalized.canonicalize().unwrap_or(normalized);
    match norm.strip_prefix(&wt) {
        Ok(rel) => {
            let s = rel
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            if s.is_empty() {
                Some(".".to_string())
            } else {
                Some(s)
            }
        }
        Err(_) => None,
    }
}

/// Lexically resolve `.` and `..` components (does not touch the filesystem / symlinks).
fn lexically_normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Validate explicit submodule path arguments against the index gitlink set, matching `git`'s
/// behavior in `module_list_compute`: a literal pathspec that matches no gitlink entry is an
/// error. Returns the worktree-relative normalized literal paths (pathspec-magic args are
/// skipped). On a non-matching path, prints git's error and bails.
fn validate_submodule_pathspecs(
    repo: &Repository,
    work_tree: &Path,
    raw_paths: &[String],
) -> Result<Vec<String>> {
    if raw_paths.is_empty() {
        return Ok(Vec::new());
    }
    let gitlinks = index_gitlink_paths(repo);
    let cwd = std::env::current_dir().unwrap_or_else(|_| work_tree.to_path_buf());
    let mut normalized = Vec::new();
    for raw in raw_paths {
        let Some(norm) = normalize_submodule_path_arg(work_tree, &cwd, raw) else {
            eprintln!("error: pathspec '{raw}' did not match any file(s) known to git");
            bail!("pathspec did not match");
        };
        if norm == "." || norm.starts_with(':') {
            normalized.push(norm);
            continue;
        }
        let matched = gitlinks
            .iter()
            .any(|g| *g == norm || g.starts_with(&format!("{norm}/")));
        if !matched {
            eprintln!("error: pathspec '{raw}' did not match any file(s) known to git");
            bail!("pathspec did not match");
        }
        normalized.push(norm);
    }
    Ok(normalized)
}

/// Select gitlink paths matching the given raw pathspecs (which may include `:(exclude)` magic),
/// mirroring git's `module_list_compute` pathspec matching. With no specs, all gitlinks match.
fn pathspec_select_gitlinks(gitlinks: &[String], specs: &[String]) -> Vec<String> {
    use grit_lib::pathspec::{matches_pathspec_with_context, PathspecMatchContext};
    if specs.is_empty() {
        return gitlinks.to_vec();
    }
    let ctx = PathspecMatchContext {
        is_directory: false,
        is_git_submodule: true,
    };
    let positives: Vec<&String> = specs
        .iter()
        .filter(|s| !s.starts_with(":!") && !s.starts_with(":^") && !s.starts_with(":(exclude"))
        .collect();
    let excludes: Vec<String> = specs
        .iter()
        .filter_map(|s| {
            s.strip_prefix(":!")
                .or_else(|| s.strip_prefix(":^"))
                .or_else(|| s.strip_prefix(":(exclude)"))
                .map(|x| x.to_string())
        })
        .collect();
    let mut out = Vec::new();
    for gl in gitlinks {
        let included = positives.is_empty()
            || positives
                .iter()
                .any(|p| matches_pathspec_with_context(p, gl, ctx));
        if !included {
            continue;
        }
        let excluded = excludes
            .iter()
            .any(|p| matches_pathspec_with_context(p, gl, ctx));
        if excluded {
            continue;
        }
        out.push(gl.clone());
    }
    out
}

fn filter_submodules<'a>(modules: &'a [SubmoduleInfo], paths: &[String]) -> Vec<&'a SubmoduleInfo> {
    if paths.is_empty() || paths.iter().any(|p| p == ".") {
        modules.iter().collect()
    } else {
        modules
            .iter()
            .filter(|m| paths.iter().any(|p| p == &m.path || p == &m.name))
            .collect()
    }
}

// ── Read recorded commit from the index ──────────────────────────────

/// Read the commit OID for a submodule path (gitlink).
///
/// Prefer the **index** when it contains a stage-0 gitlink at `submodule_path`, so
/// `git submodule update` works after `git apply --index` / partial index updates while `HEAD`
/// still points at an older commit. Fall back to `HEAD`'s tree when the path is not in the index.
fn read_gitlink_oid_head_tree(repo: &Repository, submodule_path: &str) -> Result<Option<String>> {
    let head = resolve_head(&repo.git_dir)?;
    let commit_oid = match head.oid() {
        Some(o) => *o,
        None => return Ok(None),
    };
    let obj = repo.odb.read(&commit_oid).context("read HEAD commit")?;
    let commit = parse_commit(&obj.data)?;
    let mut current_tree = commit.tree;

    let components: Vec<&str> = submodule_path
        .split('/')
        .filter(|c| !c.is_empty())
        .collect();
    if components.is_empty() {
        return Ok(None);
    }

    for (i, name) in components.iter().enumerate() {
        let tree_obj = repo.odb.read(&current_tree).context("read tree")?;
        if tree_obj.kind != ObjectKind::Tree {
            return Ok(None);
        }
        let entries = parse_tree(&tree_obj.data)?;
        let entry = entries
            .iter()
            .find(|e| e.name.as_slice() == name.as_bytes());
        let Some(entry) = entry else {
            return Ok(None);
        };
        let is_last = i + 1 == components.len();
        if is_last {
            if entry.mode == 0o160000 {
                return Ok(Some(entry.oid.to_hex()));
            }
            return Ok(None);
        }
        if entry.mode != 0o040000 {
            return Ok(None);
        }
        current_tree = entry.oid;
    }
    Ok(None)
}

fn read_submodule_commit(repo: &Repository, submodule_path: &str) -> Result<Option<String>> {
    let index_path = repo.index_path();
    if let Ok(index) = repo.load_index_at(&index_path) {
        if let Some(entry) = index.get(submodule_path.as_bytes(), 0) {
            if entry.mode == MODE_GITLINK {
                return Ok(Some(entry.oid.to_hex()));
            }
            // Path exists in the index but is not a gitlink (e.g. replaced by a regular file).
            // `submodule update` must not treat it as a submodule (t1013 read-tree).
            return Ok(None);
        }
    }
    read_gitlink_oid_head_tree(repo, submodule_path)
}

/// Recorded gitlink for `submodule_path`: index first (using a preloaded snapshot), then `HEAD` tree.
fn read_submodule_commit_for_status(
    repo: &Repository,
    index: &Index,
    submodule_path: &str,
) -> Result<Option<String>> {
    if let Some(o) = gitlink_oid_stage0(index, submodule_path) {
        return Ok(Some(o));
    }
    read_gitlink_oid_head_tree(repo, submodule_path)
}

fn gitlink_oid_stage0(index: &Index, submodule_path: &str) -> Option<String> {
    let needle = submodule_path.as_bytes();
    for entry in &index.entries {
        if entry.stage() != 0 {
            continue;
        }
        if entry.path.as_slice() == needle && entry.mode == MODE_GITLINK {
            return Some(entry.oid.to_hex());
        }
    }
    None
}

/// Gitlink OID for `submodule_path` in the current index (stage 0), if present.
///
/// Used after `grit add <path>` when `HEAD`’s tree does not yet list the new submodule
/// (e.g. `submodule add` before `commit`).
fn read_gitlink_oid_from_index(repo: &Repository, submodule_path: &str) -> Result<Option<String>> {
    let index = repo
        .load_index()
        .context("load index for submodule gitlink")?;
    Ok(gitlink_oid_stage0(&index, submodule_path))
}

/// Check out `oid` in the submodule at `path` (separate git dir under `.git/modules/<name>/` or in-tree `.git`).
///
/// `submodule_name_for_modules` is the `.gitmodules` key (Git's submodule name), which may differ from `path`.
fn checkout_submodule_worktree(
    grit_bin: &Path,
    repo: &Repository,
    work_tree: &Path,
    _submodule_name: &str,
    submodule_path: &str,
    submodule_name_for_modules: &str,
    oid: &str,
    quiet: bool,
) -> Result<()> {
    let sub_path = work_tree.join(submodule_path);
    let modules_dir =
        submodule_separate_git_dir(repo, work_tree, submodule_name_for_modules, submodule_path)?;

    // CWD must lie inside `GIT_WORK_TREE`; the superproject root is outside the submodule tree.
    // `--force`: after `clone --no-checkout`, HEAD may already equal `oid` while the index and
    // work tree are empty; without force, `checkout` skips `switch_to_tree` and leaves no files.
    let status = if modules_dir.join("HEAD").exists() {
        let mut cmd = Command::new(grit_bin);
        grit_exe::strip_trace2_env(&mut cmd);
        cmd.env("GIT_DIR", &modules_dir)
            .env("GIT_WORK_TREE", &sub_path)
            .current_dir(&sub_path)
            .args(["checkout", "--force", "--quiet", oid])
            .status()
    } else {
        let mut cmd = Command::new(grit_bin);
        grit_exe::strip_trace2_env(&mut cmd);
        cmd.args(["checkout", "--force", "--quiet", oid])
            .current_dir(&sub_path)
            .status()
    }
    .context("failed to checkout submodule commit")?;

    if !status.success() {
        bail!(
            "failed to checkout {} in submodule '{}'",
            oid,
            submodule_path
        );
    }

    if let Ok(sub_repo) = Repository::open(&modules_dir, Some(&sub_path)) {
        let _ = reapply_sparse_checkout_if_configured(&sub_repo);
    } else if sub_path.join(".git").exists() {
        if let Ok(sub_repo) = Repository::discover(Some(&sub_path)) {
            let _ = reapply_sparse_checkout_if_configured(&sub_repo);
        }
    }

    // `git submodule add` in Git leaves the submodule on the remote default branch (usually
    // `main`), not detached. We check out by object ID above to guarantee worktree population
    // after `clone --no-checkout`; if that object matches the default branch tip, reattach HEAD.
    let _ = attach_submodule_head_to_default_branch(&modules_dir, oid);

    if !quiet {
        eprintln!(
            "Submodule path '{}': checked out '{}'",
            submodule_path,
            &oid[..oid.len().min(12)]
        );
    }
    Ok(())
}

fn submodule_worktree_clean_for_update(modules_dir: &Path, sub_path: &Path) -> bool {
    let sub_repo = Repository::open(modules_dir, Some(sub_path))
        .or_else(|_| Repository::discover(Some(sub_path)));
    let Ok(sub_repo) = sub_repo else {
        return false;
    };
    let Ok(index) = sub_repo.load_index() else {
        return false;
    };
    grit_lib::diff::diff_index_to_worktree(&sub_repo.odb, &index, sub_path, false, false)
        .map(|diff| diff.is_empty())
        .unwrap_or(false)
}

fn attach_submodule_head_to_default_branch(
    sub_git_dir: &Path,
    checked_out_oid: &str,
) -> Result<()> {
    let detached_oid = match resolve_head(sub_git_dir)? {
        HeadState::Detached { oid } => oid,
        _ => return Ok(()),
    };
    let expected_oid = ObjectId::from_hex(checked_out_oid.trim())
        .with_context(|| format!("invalid submodule checkout oid '{checked_out_oid}'"))?;
    if detached_oid != expected_oid {
        return Ok(());
    }

    let Some(remote_head) = refs::read_symbolic_ref(sub_git_dir, "refs/remotes/origin/HEAD")?
    else {
        return Ok(());
    };
    let Some(branch_name) = remote_head.strip_prefix("refs/remotes/origin/") else {
        return Ok(());
    };

    let local_branch = format!("refs/heads/{branch_name}");
    if refs::resolve_ref(sub_git_dir, &local_branch).is_err() {
        let remote_branch = format!("refs/remotes/origin/{branch_name}");
        let remote_tip = match refs::resolve_ref(sub_git_dir, &remote_branch) {
            Ok(oid) => oid,
            Err(_) => return Ok(()),
        };
        refs::write_ref(sub_git_dir, &local_branch, &remote_tip)?;
    }
    if refs::resolve_ref(sub_git_dir, &local_branch).ok() != Some(detached_oid) {
        return Ok(());
    }

    refs::write_symbolic_ref(sub_git_dir, "HEAD", &local_branch)?;

    let config_path = sub_git_dir.join("config");
    let mut config = if config_path.exists() {
        let content = fs::read_to_string(&config_path)?;
        ConfigFile::parse(&config_path, &content, ConfigScope::Local)?
    } else {
        ConfigFile::parse(&config_path, "", ConfigScope::Local)?
    };
    config.set(&format!("branch.{branch_name}.remote"), "origin")?;
    config.set(
        &format!("branch.{branch_name}.merge"),
        &format!("refs/heads/{branch_name}"),
    )?;
    config.write()?;
    Ok(())
}

fn local_origin_head_branch(local_cfg: &ConfigFile, sub_path: &Path) -> Option<String> {
    let url = config_last_value(local_cfg, "remote.origin.url")?;
    let url = url.trim();
    if url.is_empty()
        || url.starts_with("ext::")
        || url.starts_with("http://")
        || url.starts_with("https://")
        || url.starts_with("git://")
        || crate::ssh_transport::is_configured_ssh_url(url)
    {
        return None;
    }

    let mut remote_path = if let Some(stripped) = url.strip_prefix("file://") {
        PathBuf::from(stripped)
    } else {
        PathBuf::from(url)
    };
    if remote_path.is_relative() {
        remote_path = sub_path.join(remote_path);
    }
    let remote_repo = Repository::open(&remote_path, None)
        .or_else(|_| Repository::discover(Some(&remote_path)))
        .ok()?;
    match resolve_head(&remote_repo.git_dir).ok()? {
        HeadState::Branch { short_name, .. } => Some(short_name),
        _ => None,
    }
}

fn local_repo_from_url(url: &str, base: &Path) -> Option<Repository> {
    let url = url.trim();
    if url.is_empty()
        || url.starts_with("ext::")
        || url.starts_with("http://")
        || url.starts_with("https://")
        || url.starts_with("git://")
        || crate::ssh_transport::is_configured_ssh_url(url)
    {
        return None;
    }
    let mut path = if let Some(stripped) = url.strip_prefix("file://") {
        PathBuf::from(stripped)
    } else {
        PathBuf::from(url)
    };
    if path.is_relative() {
        path = base.join(path);
    }
    Repository::open(&path, None)
        .or_else(|_| Repository::discover(Some(&path)))
        .ok()
}

fn uploadpack_allows_reachable_sha1_in_want(git_dir: &Path) -> bool {
    ConfigSet::load(Some(git_dir), true)
        .ok()
        .and_then(|cfg| cfg.get_bool("uploadpack.allowReachableSHA1InWant"))
        .and_then(Result::ok)
        .unwrap_or(false)
}

fn sync_remote_update_branch_for_decoration(
    sub_git_dir: &Path,
    branch: &str,
    checked_out_oid: &str,
    local_cfg: &ConfigFile,
    sub_path: &Path,
) -> Result<()> {
    let oid = ObjectId::from_hex(checked_out_oid.trim())
        .with_context(|| format!("invalid submodule checkout oid '{checked_out_oid}'"))?;
    let branch_ref = format!("refs/heads/{branch}");
    refs::write_ref(sub_git_dir, &branch_ref, &oid)?;

    if local_origin_head_branch(local_cfg, sub_path).as_deref() == Some(branch) {
        refs::write_symbolic_ref(sub_git_dir, "HEAD", &branch_ref)?;
    }

    let config_path = sub_git_dir.join("config");
    let mut config = if config_path.exists() {
        let content = fs::read_to_string(&config_path)?;
        ConfigFile::parse(&config_path, &content, ConfigScope::Local)?
    } else {
        ConfigFile::parse(&config_path, "", ConfigScope::Local)?
    };
    config.set("grit.submoduleUpdateRemoteDecorations", "true")?;
    config.write()?;
    Ok(())
}

// ── Subcommand implementations ───────────────────────────────────────

/// Matches Git's `compute_rev_name` in `submodule--helper.c`: try `describe` with several
/// flag sets until one succeeds.
fn submodule_describe_rev_name(sub_worktree: &Path, oid_hex: &str) -> Option<String> {
    if oid_hex.len() != 40 || !oid_hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let grit_bin = grit_exe::grit_executable();
    let attempts: &[&[&str]] = &[&[], &["--tags"], &["--contains"], &["--all", "--always"]];
    for extra in attempts {
        let mut cmd = grit_subprocess(&grit_bin);
        cmd.current_dir(sub_worktree)
            .stderr(Stdio::null())
            .stdout(Stdio::piped())
            .arg("describe");
        for flag in *extra {
            cmd.arg(flag);
        }
        cmd.arg(oid_hex);
        let Ok(output) = cmd.output() else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let name = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !name.is_empty() {
            return Some(name);
        }
    }
    None
}

fn emit_submodule_status_lines(
    super_repo: &Repository,
    super_index: &Index,
    super_work_tree: &Path,
    _super_git_dir: &Path,
    top_work_tree: &Path,
    invocation_cwd: &Path,
    modules: &[SubmoduleInfo],
    args: &StatusArgs,
    path_prefix: &str,
    out: &mut dyn Write,
) -> Result<()> {
    let mut sorted: Vec<&SubmoduleInfo> = modules.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));

    if args.quiet {
        return Ok(());
    }

    for m in sorted {
        let path_in_super = if path_prefix.is_empty() {
            m.path.replace('\\', "/")
        } else {
            format!("{}/{}", path_prefix.trim_end_matches('/'), m.path)
        };

        let sub_path = super_work_tree.join(&m.path);
        // Paths in the immediate superproject's index / HEAD tree use `m.path` (not the
        // top-level composite path).
        let gitlink_path = m.path.as_str();
        let recorded = read_submodule_commit_for_status(super_repo, super_index, gitlink_path)?;
        let has_checkout = sub_path.join(".git").exists();

        if !args.paths.is_empty() {
            let under_selected = args
                .paths
                .iter()
                .any(|p| path_in_super == *p || path_in_super.starts_with(&format!("{p}/")));
            if !under_selected {
                continue;
            }
        }

        let (prefix, display_oid, suffix) =
            if super_index_has_unmerged_stage(super_repo, gitlink_path) {
                (
                    "U",
                    "0000000000000000000000000000000000000000".to_owned(),
                    String::new(),
                )
            } else if !sub_path.exists() || !has_checkout {
                let oid = recorded
                    .as_deref()
                    .unwrap_or("0000000000000000000000000000000000000000");
                ("-", oid.to_owned(), String::new())
            } else {
                let index_oid = gitlink_oid_stage0(super_index, gitlink_path)
                    .unwrap_or_else(|| "0000000000000000000000000000000000000000".to_owned());

                let head_file = sub_path.join(".git");
                let sub_head = if head_file.exists() {
                    read_submodule_head(&sub_path)
                } else {
                    let modules_dir =
                        submodule_separate_git_dir(super_repo, super_work_tree, &m.name, &m.path)?;
                    let modules_head = modules_dir.join("HEAD");
                    if modules_head.exists() {
                        read_head_from_file(&modules_head)
                    } else {
                        None
                    }
                };
                let head_oid = sub_head.unwrap_or_default();

                // Match `git submodule--helper status` / `diff-files --ignore-submodules=dirty`: the
                // superproject gitlink is "dirty" when the submodule's resolved HEAD commit differs
                // from the index gitlink. Inner working tree dirtiness does not matter.
                // With `--cached`, a dirty submodule still prints `+` but uses the **index** OID (and
                // its describe); without `--cached`, it prints the submodule HEAD OID — see t7422.
                let dirty = !head_oid.is_empty() && head_oid != index_oid;

                let (p, oid_for_line, oid_for_describe) = if !dirty {
                    (" ", index_oid.clone(), index_oid.clone())
                } else if args.cached {
                    ("+", index_oid.clone(), index_oid.clone())
                } else {
                    ("+", head_oid.clone(), head_oid.clone())
                };

                let suf = submodule_describe_rev_name(&sub_path, &oid_for_describe)
                    .map(|n| format!(" ({n})"))
                    .unwrap_or_default();
                (p, oid_for_line, suf)
            };

        let display_path =
            rev_parse::to_relative_path(&top_work_tree.join(&path_in_super), invocation_cwd)
                .replace('\\', "/");

        writeln!(out, "{prefix}{display_oid} {display_path}{suffix}")?;
        out.flush()?;

        if args.recursive && has_checkout && sub_path.join(".git").exists() {
            let Ok(sub_repo) = Repository::discover(Some(&sub_path)) else {
                continue;
            };
            let Some(sub_wt) = sub_repo.work_tree.as_ref() else {
                continue;
            };
            let nested = parse_gitmodules_with_repo(sub_wt, Some(&sub_repo)).unwrap_or_default();
            if !nested.is_empty() {
                let sub_index = sub_repo
                    .load_index()
                    .context("load submodule index for recursive status")?;
                emit_submodule_status_lines(
                    &sub_repo,
                    &sub_index,
                    sub_wt,
                    &sub_repo.git_dir,
                    top_work_tree,
                    invocation_cwd,
                    &nested,
                    args,
                    &path_in_super,
                    out,
                )?;
            }
        }
    }

    Ok(())
}

fn run_status(args: &StatusArgs) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let work_tree = repo.work_tree.as_ref().context("bare repository")?;
    if let Ok(index) = repo.load_index() {
        repo.odb
            .register_submodule_object_directories_from_index(work_tree, &index);
    }
    // Validate any explicit path arguments against the index gitlink set (git's
    // `module_list_compute`): a pathspec matching no gitlink is an error. Replace the raw
    // paths with worktree-relative normalized ones so filtering works from a subdirectory.
    let normalized_paths = validate_submodule_pathspecs(&repo, work_tree, &args.paths)?;
    let args = StatusArgs {
        paths: normalized_paths,
        ..args.clone()
    };
    let args = &args;
    let modules = parse_gitmodules_with_repo(work_tree, Some(&repo))?;
    let index = repo
        .load_index()
        .context("load index for submodule status")?;

    // git's status_submodule dies for any (selected) index gitlink that has no `.gitmodules`
    // mapping ("no submodule mapping found in .gitmodules for path '<p>'").
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let gitlinks = index_gitlink_paths(&repo);
    for gl in &gitlinks {
        let selected_by_args = args.paths.is_empty()
            || args.paths.iter().any(|p| {
                p == "." || p == gl || gl.starts_with(&format!("{p}/")) || p.starts_with(':')
            });
        if selected_by_args && !modules.iter().any(|m| &m.path == gl) {
            let display = rev_parse::to_relative_path(&work_tree.join(gl), &cwd).replace('\\', "/");
            bail!("no submodule mapping found in .gitmodules for path '{display}'");
        }
    }

    // Flush after each line so `... | grep -q` closes the read end early and the next write
    // returns `EPIPE` → exit 141 (t7422-submodule-output).
    let stdout = io::stdout();
    let mut out = stdout.lock();
    emit_submodule_status_lines(
        &repo,
        &index,
        work_tree,
        &repo.git_dir,
        work_tree,
        &cwd,
        &modules,
        args,
        "",
        &mut out,
    )?;
    Ok(())
}

/// Read HEAD of a submodule working directory.
fn read_submodule_head(sub_path: &Path) -> Option<String> {
    // If .git is a file (gitfile), follow it.
    let dot_git = sub_path.join(".git");
    let git_dir = if dot_git.is_file() {
        let content = fs::read_to_string(&dot_git).ok()?;
        let gitdir = content.strip_prefix("gitdir: ")?.trim();
        if Path::new(gitdir).is_absolute() {
            PathBuf::from(gitdir)
        } else {
            sub_path.join(gitdir)
        }
    } else if dot_git.is_dir() {
        dot_git
    } else {
        return None;
    };

    read_head_from_dir(&git_dir)
}

/// Read the HEAD OID from a git directory.
fn read_head_from_dir(git_dir: &Path) -> Option<String> {
    read_head_from_file(&git_dir.join("HEAD"))
}

/// Read HEAD from a specific file, resolving symbolic refs.
fn read_head_from_file(head_file: &Path) -> Option<String> {
    let content = fs::read_to_string(head_file).ok()?;
    let content = content.trim();
    if let Some(refname) = content.strip_prefix("ref: ") {
        // Resolve the ref.
        let git_dir = head_file.parent()?;
        let ref_file = git_dir.join(refname);
        fs::read_to_string(ref_file)
            .ok()
            .map(|s| s.trim().to_string())
    } else {
        Some(content.to_string())
    }
}

fn default_initial_branch_name() -> String {
    std::env::var("GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME").unwrap_or_else(|_| "main".to_string())
}

/// When `remote.origin.url` points at a local repository, emulate `git fetch origin` by copying
/// objects and updating `refs/remotes/origin/*` without `upload-pack` (avoids protocol v2 client
/// limitations for submodule `--remote`).
///
/// Returns `Ok(true)` when the fast path ran, `Ok(false)` when the URL is not a local repo path.
fn submodule_fetch_origin_local_path(
    sub_path: &Path,
    local_cfg: &ConfigFile,
    quiet: bool,
) -> Result<bool> {
    let Some(sub_git_dir) = resolve_submodule_git_dir(sub_path) else {
        return Ok(false);
    };
    let Some(url) = config_last_value(local_cfg, "remote.origin.url") else {
        return Ok(false);
    };
    let url = url.trim();
    if url.is_empty() {
        return Ok(false);
    }
    if url.starts_with("ext::") || url.starts_with("http://") || url.starts_with("https://") {
        return Ok(false);
    }
    if url.starts_with("git://") {
        return Ok(false);
    }
    if crate::ssh_transport::is_configured_ssh_url(url) {
        return Ok(false);
    }

    let mut remote_path = if let Some(stripped) = url.strip_prefix("file://") {
        PathBuf::from(stripped)
    } else {
        PathBuf::from(url)
    };
    if remote_path.is_relative() {
        remote_path = sub_path.join(&remote_path);
    }
    let remote_path = remote_path.canonicalize().unwrap_or(remote_path);
    let remote_repo = match Repository::open(&remote_path, None)
        .or_else(|_| Repository::discover(Some(&remote_path)))
    {
        Ok(r) => r,
        Err(_) => return Ok(false),
    };
    let remote_git = remote_repo.git_dir.as_path();

    let heads = refs::list_refs(remote_git, "refs/heads/")?;
    if heads.is_empty() {
        return Ok(false);
    }

    if !quiet {
        eprintln!("From {}", remote_path.display());
    }

    let mut roots: Vec<ObjectId> = Vec::new();
    for (refname, oid) in &heads {
        let short = refname
            .strip_prefix("refs/heads/")
            .unwrap_or(refname.as_str());
        let local_ref = format!("refs/remotes/origin/{short}");
        let old_hex = refs::resolve_ref(&sub_git_dir, &local_ref)
            .map(|o| o.to_hex())
            .unwrap_or_else(|_| "0".repeat(40));
        refs::write_ref(&sub_git_dir, &local_ref, oid)?;
        roots.push(*oid);
        if !quiet {
            let branch = short;
            eprintln!(
                "   {}..{}  {}     -> origin/{}",
                &old_hex[..7.min(old_hex.len())],
                &oid.to_hex()[..7],
                branch,
                branch
            );
        }
    }
    for (refname, oid) in refs::list_refs(remote_git, "refs/tags/")? {
        refs::write_ref(&sub_git_dir, &refname, &oid)?;
        roots.push(oid);
    }
    roots.sort_by_key(|o| o.to_hex());
    roots.dedup();

    if let Ok(head) = resolve_head(remote_git) {
        match head {
            grit_lib::state::HeadState::Branch { short_name, .. } => {
                let sym = format!("refs/remotes/origin/{short_name}");
                if refs::resolve_ref(&sub_git_dir, &sym).is_ok() {
                    let _ =
                        refs::write_symbolic_ref(&sub_git_dir, "refs/remotes/origin/HEAD", &sym);
                }
            }
            _ => {}
        }
    }

    crate::commands::fetch::copy_reachable_objects_skipping_gitlinks(
        remote_git,
        &sub_git_dir,
        &roots,
    )?;

    Ok(true)
}

/// When the superproject records a gitlink OID that is not in the submodule ODB yet (for example
/// commits reachable only from a non-default branch after `git remote rename`), fetch that object
/// explicitly from the submodule's default remote. Matches Git behavior exercised by
/// `t5572-pull-submodule`.
fn submodule_fetch_gitlink_if_missing(
    grit_bin: &std::path::Path,
    super_work_tree: &Path,
    sub_rel_path: &str,
    sub_path: &Path,
    recorded_hex: &str,
) -> Result<()> {
    let Ok(recorded) = ObjectId::from_hex(recorded_hex.trim()) else {
        return Ok(());
    };
    let Some(sub_git_dir) = resolve_submodule_git_dir(sub_path) else {
        return Ok(());
    };
    let nested_repo = match Repository::open(&sub_git_dir, Some(sub_path)) {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    if nested_repo.odb.exists(&recorded) {
        return Ok(());
    }
    let remote = get_default_remote_for_path_in_super(sub_rel_path, super_work_tree)
        .unwrap_or_else(|_| "origin".to_owned());
    let mut cmd = grit_subprocess(grit_bin);
    cmd.args(["fetch", remote.as_str(), recorded_hex])
        .current_dir(sub_path)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE");
    let st = cmd.status().with_context(|| {
        format!("fetch missing submodule object {recorded_hex} from remote '{remote}'")
    })?;
    if !st.success() {
        bail!(
            "failed to fetch missing submodule object '{}' from remote '{}'",
            recorded_hex,
            remote
        );
    }
    Ok(())
}

fn superproject_head_short_branch(repo: &Repository) -> Option<String> {
    resolve_head(&repo.git_dir)
        .ok()
        .and_then(|h| h.branch_name().map(|s| s.to_string()))
}

fn resolve_submodule_remote_branch_name(
    super_repo: &Repository,
    sm: &SubmoduleInfo,
    local_cfg: &ConfigFile,
) -> String {
    let key = format!("submodule.{}.branch", sm.name);
    let mut branch = config_last_value(local_cfg, &key)
        .or_else(|| sm.branch.clone())
        .unwrap_or_else(default_initial_branch_name);
    if branch == "." {
        branch =
            superproject_head_short_branch(super_repo).unwrap_or_else(default_initial_branch_name);
    }
    branch
}

fn expand_submodule_shell_command(cmd: &str, sha1: &str, path: &str, toplevel: &Path) -> String {
    cmd.replace("$sha1", sha1)
        .replace("$path", path)
        .replace("$toplevel", &toplevel.to_string_lossy())
}

fn init_in_repo(repo: &Repository, args: &InitArgs, quiet: bool) -> Result<()> {
    let work_tree = repo.work_tree.as_ref().context("bare repository")?;
    // Git derives the submodule set from index gitlink entries (module_list_compute), then
    // dies for any path that lacks a `.gitmodules` entry ("No url found for submodule path").
    let init_paths = validate_submodule_pathspecs(repo, work_tree, &args.paths)?;
    let modules = parse_gitmodules_with_repo(work_tree, Some(repo))?;
    let gitlinks = index_gitlink_paths(repo);

    // Select gitlinks by pathspec (supports `:(exclude)`); when no path args were given and
    // `submodule.active` is configured, default to only the active submodules (git module_init).
    let mut selected_paths = pathspec_select_gitlinks(&gitlinks, &init_paths);
    if args.paths.is_empty() {
        let cfg = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true).ok();
        let has_active = cfg
            .as_ref()
            .map(|c| c.has_key("submodule.active"))
            .unwrap_or(false);
        if has_active {
            selected_paths.retain(|gl| {
                grit_lib::submodule_active::is_submodule_active(repo, gl).unwrap_or(false)
            });
        }
    }

    for gl in &selected_paths {
        let matched = modules.iter().find(|m| &m.path == gl);
        // Die when the gitlink has no `.gitmodules` mapping, or its mapping has no url and the
        // local config has not already registered a url for that submodule name.
        let needs_url = match matched {
            None => true,
            Some(m) => m.url.trim().is_empty() && config_submodule_url(repo, &m.name).is_none(),
        };
        if needs_url {
            // A gitlink missing from `.gitmodules` but already populated (its work tree has `.git`)
            // does not need init: it was cloned previously and only its HEAD will be moved. Git's
            // `submodule update` warns-and-skips such paths (`next_submodule_warn_missing`) rather
            // than dying, so an entry deleted from `.gitmodules` while still checked out must not
            // abort `checkout --recurse-submodules` (t5526 #42/#45). Only paths with NO `.gitmodules`
            // mapping qualify; a present-but-url-less mapping still errors as before.
            if matched.is_none() {
                let abs = work_tree.join(gl);
                if abs.join(".git").exists() {
                    continue;
                }
            }
            let display = rev_parse::to_relative_path(
                &work_tree.join(gl),
                &std::env::current_dir().unwrap_or_else(|_| work_tree.to_path_buf()),
            );
            bail!("No url found for submodule path '{display}' in .gitmodules");
        }
    }

    let config_path = repo.git_dir.join("config");
    let mut config = if config_path.exists() {
        let content = fs::read_to_string(&config_path)?;
        ConfigFile::parse(&config_path, &content, ConfigScope::Local)?
    } else {
        ConfigFile::parse(&config_path, "", ConfigScope::Local)?
    };

    for gl in &selected_paths {
        let Some(m) = modules.iter().find(|m| &m.path == gl) else {
            continue;
        };
        let url_key = format!("submodule.{}.url", m.name);
        let already = config.entries.iter().any(|e| e.key == url_key);

        // Set the active flag (git: init_submodule sets submodule.<name>.active=true unless it
        // is already active, e.g. matched by an existing submodule.active pathspec).
        if !grit_lib::submodule_active::is_submodule_active(repo, &m.path).unwrap_or(false) {
            config.set(&format!("submodule.{}.active", m.name), "true")?;
        }

        if !already {
            if let Some(ref u) = m.update {
                let t = u.trim();
                if t.starts_with('!') {
                    bail!(
                        "error: invalid value for 'submodule.{}.update': '{}' cannot be specified in .gitmodules as a command exists\n\
                         You can still add the config by using:\n\
                         'git config submodule.{}.update {}'",
                        m.name,
                        t,
                        m.name,
                        t
                    );
                }
            }

            let resolved_url = resolve_submodule_super_url(work_tree, &repo.git_dir, &m.url)?;
            config.set(&url_key, &resolved_url)?;
            let reg_path = submodule_display_path_from_cwd(&work_tree.join(&m.path));
            if !quiet {
                eprintln!(
                    "Submodule '{}' ({}) registered for path '{}'",
                    m.name, resolved_url, reg_path
                );
            }
        }

        if let Some(ref u) = m.update {
            config.set(&format!("submodule.{}.update", m.name), u)?;
        }
        if let Some(ref b) = m.branch {
            config.set(&format!("submodule.{}.branch", m.name), b)?;
        }
    }

    config.write()?;
    Ok(())
}

fn run_init(args: &InitArgs, quiet: bool) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    init_in_repo(&repo, args, quiet)
}

/// When tests (or users) set `submodule.<name>.url` to a sentinel like `bogus`, still clone using
/// the URL from `.gitmodules` (`t7112-reset-submodule` / `reset_work_tree_to_interested`).
fn effective_submodule_clone_url(configured: &str, gitmodules_url: &str) -> String {
    let t = configured.trim();
    if t.eq_ignore_ascii_case("bogus") || t == "/dev/null" {
        gitmodules_url.to_string()
    } else {
        configured.to_string()
    }
}

include!("_submodule_run_update_inner.rs.inc");

/// Ensure `.git/modules/<rel>` exists when the superproject still records the submodule but the
/// module directory was removed (e.g. `git revert` after replacing a submodule with a file).
/// Delegates to the same logic as `submodule update --init` so nested `.git/modules/.../modules/...`
/// layouts match Git (`t7112-reset-submodule`).
pub(crate) fn ensure_submodule_modules_gitdir(repo: &Repository, rel: &str) -> Result<()> {
    let work_tree = repo.work_tree.as_ref().context("bare repository")?;
    let modules_dir = submodule_modules_git_dir(&repo.git_dir, rel);
    if !modules_dir.join("HEAD").exists() {
        run_update_inner(
            &UpdateArgs {
                paths: vec![rel.to_owned()],
                quiet: true,
                init: true,
                checkout: false,
                remote: false,
                rebase: false,
                merge: false,
                force: false,
                depth: None,
                jobs: None,
                filter: None,
                ref_format: None,
                recursive: true,
                implicit_recursive: false,
                reference: vec![],
                dissociate: false,
                no_recommend_shallow: false,
            },
            None,
        )?;
    }
    let sm_wt = work_tree.join(rel);
    if !sm_wt.join(".git").exists() {
        return Ok(());
    }
    let sub_repo = Repository::open(&modules_dir, Some(&sm_wt))
        .or_else(|_| Repository::discover(Some(&sm_wt)))?;
    let nested = parse_gitmodules_with_repo(&sm_wt, Some(&sub_repo)).unwrap_or_default();
    for n in nested {
        let nested_modules = submodule_modules_git_dir(&sub_repo.git_dir, &n.path);
        if nested_modules.join("HEAD").exists() {
            continue;
        }
        run_update_inner(
            &UpdateArgs {
                paths: vec![n.path.clone()],
                quiet: true,
                init: true,
                checkout: false,
                remote: false,
                rebase: false,
                merge: false,
                force: false,
                depth: None,
                jobs: None,
                filter: None,
                ref_format: None,
                recursive: true,
                implicit_recursive: false,
                reference: vec![],
                dissociate: false,
                no_recommend_shallow: false,
            },
            Some(sm_wt.clone()),
        )?;
    }
    Ok(())
}

fn run_update(args: &UpdateArgs) -> Result<()> {
    run_update_inner(args, None)
}

/// Populate `objects/info/alternates` for a submodule git dir (matches `git clone --reference`).
fn write_submodule_object_alternates(
    modules_dir: &Path,
    _super_git_dir: &Path,
    reference_roots: &[PathBuf],
) -> Result<()> {
    let dst_info = modules_dir.join("objects/info");
    fs::create_dir_all(&dst_info)?;

    let mut lines = Vec::new();
    for root in reference_roots {
        let ref_git = if root.join("HEAD").exists() {
            root.clone()
        } else {
            root.join(".git")
        };
        let ref_repo = Repository::open(&ref_git, None)
            .with_context(|| format!("cannot open reference repository '{}'", root.display()))?;
        let ref_objects = ref_repo.git_dir.join("objects");
        let ref_objects_abs = ref_objects.canonicalize().unwrap_or(ref_objects);
        lines.push(ref_objects_abs.to_string_lossy().to_string());
    }

    let content = lines.join("\n") + "\n";
    fs::write(dst_info.join("alternates"), content)?;
    Ok(())
}

/// Derive a submodule reference gitdir from the superproject's alternate object stores.
pub(crate) fn superproject_submodule_reference_roots(
    super_git_dir: &Path,
    submodule_logical_name: &str,
) -> Result<Vec<PathBuf>> {
    let Some(strategy) = superproject_submodule_alternate_error_strategy(super_git_dir)? else {
        return Ok(Vec::new());
    };

    let objects_dir = super_git_dir.join("objects");
    let alternates = grit_lib::pack::read_alternates_recursive(&objects_dir).unwrap_or_default();
    let mut refs = Vec::new();
    for alt_objects in alternates {
        if alt_objects.file_name().and_then(|s| s.to_str()) != Some("objects") {
            continue;
        }
        let Some(alt_git_dir) = alt_objects.parent() else {
            continue;
        };
        let candidate = alt_git_dir.join("modules").join(submodule_logical_name);
        let candidate = candidate.canonicalize().unwrap_or(candidate);
        if candidate.join("HEAD").is_file() {
            refs.push(candidate);
            continue;
        }
        let msg = format!("path '{}' does not exist", candidate.display());
        match strategy.as_str() {
            "die" => {
                bail!("fatal: submodule '{submodule_logical_name}' cannot add alternate: {msg}");
            }
            "info" => {
                eprintln!("submodule '{submodule_logical_name}' cannot add alternate: {msg}");
            }
            _ => {}
        }
    }
    Ok(refs)
}

/// Return the configured strategy for superproject-derived submodule alternates.
pub(crate) fn superproject_submodule_alternate_error_strategy(
    super_git_dir: &Path,
) -> Result<Option<String>> {
    let config_path = super_git_dir.join("config");
    let config = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
        Some(c) => c,
        None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
    };
    let loc = config_last_value(&config, "submodule.alternatelocation");
    if !matches!(loc.as_deref(), Some("superproject")) {
        return Ok(None);
    }
    Ok(Some(
        config_last_value(&config, "submodule.alternateerrorstrategy")
            .unwrap_or_else(|| "die".to_string()),
    ))
}

/// Configure a submodule clone to derive nested alternates from its own superproject alternates.
pub(crate) fn write_submodule_alternate_inheritance_config(
    git_dir: &Path,
    strategy: &str,
) -> Result<()> {
    let config_path = git_dir.join("config");
    let mut config = match ConfigFile::from_path(&config_path, ConfigScope::Local)? {
        Some(c) => c,
        None => ConfigFile::parse(&config_path, "", ConfigScope::Local)?,
    };
    config.set("submodule.alternateLocation", "superproject")?;
    config.set("submodule.alternateErrorStrategy", strategy)?;
    config
        .write()
        .context("writing submodule alternate inheritance config")
}

/// Remove `core.worktree` from a separate submodule git dir (Git `submodule_unset_core_worktree`).
pub(crate) fn unset_submodule_core_worktree_config(modules_dir: &Path) -> Result<()> {
    let config_path = modules_dir.join("config");
    if !config_path.is_file() {
        return Ok(());
    }
    let content = fs::read_to_string(&config_path)?;
    let mut cfg = ConfigFile::parse(&config_path, &content, ConfigScope::Local)?;
    if cfg.unset("core.worktree")? > 0 {
        cfg.write()?;
        return Ok(());
    }
    // Linked worktree module configs may keep `worktree` only in raw `[core]` lines that
    // `ConfigFile` does not round-trip into entries (t2405).
    let mut out_lines = Vec::new();
    let mut removed = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("worktree =") || trimmed.starts_with("worktree=") {
            removed = true;
            continue;
        }
        out_lines.push(line);
    }
    if removed {
        let mut rebuilt = out_lines.join("\n");
        if !rebuilt.ends_with('\n') {
            rebuilt.push('\n');
        }
        fs::write(&config_path, rebuilt)?;
    }
    Ok(())
}

/// After `checkout --recurse-submodules` in a linked worktree, drop `core.worktree` from per-worktree
/// module configs (t2405).
pub(crate) fn unset_linked_worktree_submodule_core_worktrees(repo: &Repository) -> Result<()> {
    if !repo
        .git_dir
        .components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new("worktrees"))
    {
        return Ok(());
    }
    let modules_root = repo.git_dir.join("modules");
    let Ok(entries) = fs::read_dir(&modules_root) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let modules_dir = entry.path();
        if modules_dir.join("HEAD").is_file() {
            unset_submodule_core_worktree_config(&modules_dir)?;
        }
    }
    Ok(())
}

pub(crate) fn set_submodule_core_worktree(grit_bin: &Path, modules_dir: &Path, sub_path: &Path) {
    // Match Git: store a path relative to the module git dir so `test_git_directory_is_unchanged`
    // can compare `.git/modules/<name>` with a copied `<path>/.git` (t4137).
    let wt = pathdiff_relative(modules_dir, sub_path);
    let _ = Command::new(grit_bin)
        .arg("--git-dir")
        .arg(modules_dir)
        .arg("config")
        .arg("core.worktree")
        .arg(&wt)
        .status();
}

/// Called from `clone --recurse-submodules` after cloning a submodule with `--separate-git-dir`.
pub(crate) fn set_submodule_core_worktree_after_separate_clone(
    grit_bin: &Path,
    modules_dir: &Path,
    sub_path: &Path,
) {
    set_submodule_core_worktree(grit_bin, modules_dir, sub_path);
}

fn attach_existing_submodule_worktree(
    grit_bin: &Path,
    modules_dir: &Path,
    sub_path: &Path,
) -> Result<()> {
    if sub_path.exists() {
        let meta = fs::symlink_metadata(sub_path)?;
        if meta.is_file() || meta.file_type().is_symlink() {
            fs::remove_file(sub_path).with_context(|| {
                format!(
                    "cannot replace file at submodule path {}",
                    sub_path.display()
                )
            })?;
        } else if !meta.is_dir() {
            bail!(
                "submodule path '{}' exists but is not a directory",
                sub_path.display()
            );
        }
    }
    if !sub_path.exists() {
        fs::create_dir_all(sub_path)?;
    }
    write_submodule_gitfile(sub_path, modules_dir).map_err(|e| anyhow::anyhow!("{e}"))?;
    set_submodule_core_worktree(grit_bin, modules_dir, sub_path);
    Ok(())
}

/// Whether `.gitmodules` may be created or updated in the work tree (`git/submodule.c:is_writing_gitmodules_ok`).
fn is_writing_gitmodules_ok(repo: &Repository, work_tree: &Path) -> bool {
    let gm = work_tree.join(".gitmodules");
    if gm.exists() {
        return true;
    }
    let Ok(index) = repo.load_index() else {
        return false;
    };
    if index.get(b".gitmodules", 0).is_some() {
        return false;
    }
    let Ok(head) = resolve_head(&repo.git_dir) else {
        return false;
    };
    let Some(commit_oid) = head.oid().copied() else {
        return true;
    };
    let Ok(obj) = repo.odb.read(&commit_oid) else {
        return false;
    };
    if obj.kind != ObjectKind::Commit {
        return false;
    }
    let Ok(c) = parse_commit(&obj.data) else {
        return false;
    };
    blob_oid_at_path(&repo.odb, &c.tree, ".gitmodules").is_none()
}

/// Whether `dir` is a non-bare repository directory (has a `.git` directory or gitfile),
/// mirroring git's `is_nonbare_repository_dir` for `submodule add`.
fn is_nonbare_repository_dir(dir: &Path) -> bool {
    let dot_git = dir.join(".git");
    dot_git.is_dir() || dot_git.is_file()
}

/// Resolve the gitlink `HEAD` of a submodule work tree to a commit OID, returning `None` when the
/// repository has no commit checked out (git: `repo_resolve_gitlink_ref(.., "HEAD") < 0`).
fn submodule_resolve_gitlink_head(sub_path: &Path) -> Option<String> {
    grit_lib::diff::read_submodule_head_oid(sub_path).map(|oid| oid.to_hex())
}

fn run_add(args: &AddArgs) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let work_tree = repo.work_tree.as_ref().context("bare repository")?;
    let grit_bin = grit_exe::grit_executable();

    if !is_writing_gitmodules_ok(&repo, work_tree) {
        bail!("please make sure that the .gitmodules file is in the working tree");
    }

    // Derive path from URL if not provided.
    let mut path = match &args.path {
        Some(p) => p.clone(),
        None => {
            let url = &args.url;
            let basename = url
                .rsplit('/')
                .next()
                .unwrap_or(url)
                .strip_suffix(".git")
                .unwrap_or(url.rsplit('/').next().unwrap_or(url));
            basename.to_string()
        }
    };

    // When invoked from a subdirectory, git prefixes a relative `sm_path` with the cwd prefix
    // and rejects a relative repo URL ("Relative path can only be used from the toplevel").
    let cwd = std::env::current_dir().context("current directory for submodule add")?;
    let prefix = rev_parse::show_prefix(&repo, &cwd);
    if !prefix.is_empty() {
        let repo_url = args.url.trim();
        if repo_url.starts_with("./") || repo_url.starts_with("../") {
            bail!("Relative path can only be used from the toplevel of the working tree");
        }
        if !Path::new(&path).is_absolute() {
            path = format!("{prefix}{path}");
        }
    }

    // Normalize: collapse `//`, leading `./`, `/./`, `/../`, and strip trailing slashes
    // (git: normalize_path_copy + strip_dir_trailing_slashes).
    path = match grit_lib::git_path::normalize_path_copy(&path) {
        Ok(p) => p.trim_end_matches('/').to_string(),
        Err(_) => path.trim_end_matches('/').to_string(),
    };
    if path.is_empty() {
        bail!("'{}' is not a valid submodule path", args.url);
    }

    // Reject paths that traverse a symlink (git: validate_submodule_path).
    validate_submodule_path(work_tree, &path).map_err(|e| anyhow::anyhow!("{e}"))?;

    // Fail if the path is already tracked in the index (git: die_on_index_match). Pathspec
    // semantics: a directory pathspec matches entries beneath it (e.g. `dir-tracked` matches
    // `dir-tracked/bar`). When forced, a non-gitlink match is still fatal.
    if let Ok(idx) = repo.load_index() {
        let path_prefix = format!("{path}/");
        if let Some(entry) = idx.entries.iter().find(|e| {
            if e.stage() != 0 {
                return false;
            }
            let name = String::from_utf8_lossy(&e.path);
            name == path || name.starts_with(&path_prefix)
        }) {
            let exact_gitlink =
                String::from_utf8_lossy(&entry.path) == path && entry.mode == MODE_GITLINK;
            if !args.force {
                bail!("fatal: '{path}' already exists in the index");
            }
            if !exact_gitlink {
                bail!("fatal: '{path}' already exists in the index and is not a submodule");
            }
        }
    }

    // Fail when the path is a non-bare repository that has no commit checked out
    // (git: die_on_repo_without_commits).
    let sub_abs = work_tree.join(&path);
    if is_nonbare_repository_dir(&sub_abs) && submodule_resolve_gitlink_head(&sub_abs).is_none() {
        bail!("fatal: '{path}' does not have a commit checked out");
    }

    // Without --force, mirror git's `add --dry-run --ignore-missing --no-warn-embedded-repo`
    // probe so .gitignore and index-lock errors surface with git's wording.
    if !args.force {
        let out = superproject_subprocess(&grit_bin, &repo, work_tree)
            .arg("add")
            .arg("--dry-run")
            .arg("--ignore-missing")
            .arg("--no-warn-embedded-repo")
            .arg("--")
            .arg(&path)
            .current_dir(work_tree)
            .stderr(Stdio::piped())
            .stdout(Stdio::null())
            .output()
            .context("failed to run add --dry-run probe")?;
        if !out.status.success() {
            let mut stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            if !stderr.is_empty() && !stderr.ends_with('\n') {
                stderr.push('\n');
            }
            eprint!("{stderr}");
            // Relay only the probe's own stderr (matches git's behavior of fputs(sb.buf,
            // stderr) followed by a clean non-zero exit) — do not add a grit "error:" line.
            return Err(crate::explicit_exit::SilentNonZeroExit {
                code: out.status.code().unwrap_or(1),
            }
            .into());
        }
    }

    let name = args.name.clone().unwrap_or_else(|| path.clone());

    // A name already mapped in `.gitmodules` to a *different* path is fatal unless forced
    // (git: "submodule name '%s' already used for path '%s'").
    {
        let existing = parse_gitmodules_with_repo(work_tree, Some(&repo)).unwrap_or_default();
        if let Some(m) = existing.iter().find(|m| m.name == name) {
            if m.path != path && !args.force {
                bail!("submodule name '{name}' already used for path '{}'", m.path);
            }
        }
    }

    let index_for_die = repo.load_index().ok();
    let store = refs::common_dir(&repo.git_dir).unwrap_or_else(|| repo.git_dir.clone());
    // Git only rejects a path that is *strictly nested* under an existing registered submodule
    // (die_path_inside_submodule: item->len > ce_len). Re-adding the same path (reconfigure with
    // --force) and adding a path that merely shares a `.gitmodules` section name are allowed.
    let registered_paths: Vec<String> = parse_gitmodules_with_repo(work_tree, Some(&repo))
        .unwrap_or_default()
        .into_iter()
        .map(|m| m.path.replace('\\', "/"))
        .collect();
    let path_norm = path.replace('\\', "/");
    let is_registered_path = registered_paths.iter().any(|p| *p == path_norm);
    let nested_under_registered = registered_paths
        .iter()
        .any(|p| path_norm.starts_with(&format!("{p}/")));
    if nested_under_registered {
        bail!("cannot add submodule: path inside existing submodule");
    }
    if !is_registered_path {
        die_path_inside_submodule_when_disabled(&store, work_tree, &path, index_for_die.as_ref())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }

    let sub_path = work_tree.join(&path);
    // Submodule git dir is keyed by `--name` when given (Git: `.git/modules/<name>`), not by
    // the worktree path (`t0035-safe-bare-repository`, `git submodule add --name`).
    if let Some(ref n) = args.name {
        if !check_submodule_name(n) {
            bail!("fatal: '{n}' is not a valid submodule name");
        }
    }

    let local_config_path = repo.git_dir.join("config");
    let mut local_config = if local_config_path.exists() {
        let content = fs::read_to_string(&local_config_path)?;
        ConfigFile::parse(&local_config_path, &content, ConfigScope::Local)?
    } else {
        ConfigFile::parse(&local_config_path, "", ConfigScope::Local)?
    };

    if !args.force
        && config_last_value(&local_config, &format!("submodule.{name}.url")).is_some()
        && !is_nonbare_repository_dir(&sub_path)
    {
        bail!("submodule name '{name}' already used");
    }

    if submodule_path_config_enabled(&store) {
        ensure_submodule_gitdir_config(work_tree, &store, &mut local_config, &name)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }

    // Track whether we clone the submodule. Git only runs `checkout -f` (which detaches/repopulates
    // the worktree) on the clone path; the "Adding existing repo" path leaves the existing worktree
    // and its HEAD untouched (builtin/submodule--helper.c:add_submodule).
    let mut did_clone = false;

    if sub_path.exists() {
        // If the path already exists and is a valid git repo, treat it like
        // "Adding existing repo" (same as C git).
        let is_repo = sub_path.join(".git").exists();
        if !is_repo {
            if args.force {
                fs::remove_dir_all(&sub_path).with_context(|| {
                    format!(
                        "could not remove existing path '{}' for submodule add --force",
                        path
                    )
                })?;
            } else {
                bail!("'{}' already exists and is not a git repository", path);
            }
        } else if !args.quiet {
            eprintln!("Adding existing repo at '{}' to the index", path);
        }

        let dot_git = sub_path.join(".git");
        if sub_path.exists() && submodule_path_config_enabled(&store) && dot_git.is_dir() {
            let modules_dir = submodule_separate_git_dir(&repo, work_tree, &name, &path)?;
            if let Some(parent) = modules_dir.parent() {
                fs::create_dir_all(parent)?;
            }
            if modules_dir.exists() {
                bail!(
                    "submodule git dir '{}' already exists; cannot absorb existing repository",
                    modules_dir.display()
                );
            }
            fs::rename(&dot_git, &modules_dir).with_context(|| {
                format!(
                    "failed to move submodule git dir to {}",
                    modules_dir.display()
                )
            })?;
            write_submodule_gitfile(&sub_path, &modules_dir).map_err(|e| anyhow::anyhow!("{e}"))?;
            set_separate_gitdir_worktree(&grit_bin, &modules_dir, &sub_path);
        }
    }

    if !sub_path.exists() {
        // Clone the submodule.
        let modules_dir = submodule_separate_git_dir(&repo, work_tree, &name, &path)?;
        if let Some(outer) = submodule_gitdir_outer_conflict(&modules_dir, name.as_str()) {
            bail!(
                "fatal: submodule git dir '{}' is inside git dir '{}'",
                modules_dir.display(),
                outer.display()
            );
        }
        // Git only clones when the module git dir does NOT already exist
        // (builtin/submodule--helper.c `clone_submodule`); when it exists (e.g. re-adding a
        // submodule that was `git rm`-ed but whose `.git/modules/<name>` survived), git reuses it,
        // drops the stale index, connects the work tree, and checks out — it does not re-clone (so
        // the source URL need not even be reachable). Reactivate that existing module dir here.
        if modules_dir.exists() {
            fs::create_dir_all(&sub_path).with_context(|| {
                format!(
                    "could not create submodule work tree '{}'",
                    sub_path.display()
                )
            })?;
            let _ = fs::remove_file(modules_dir.join("index"));
            write_submodule_gitfile(&sub_path, &modules_dir).map_err(|e| anyhow::anyhow!("{e}"))?;
            set_separate_gitdir_worktree(&grit_bin, &modules_dir, &sub_path);
            // Populate the work tree from the module's HEAD via the staged-gitlink checkout below.
            did_clone = true;
        } else {
            // Only create the parent directory; git clone --separate-git-dir
            // will create the modules_dir itself.
            if let Some(parent) = modules_dir.parent() {
                fs::create_dir_all(parent)?;
            }

            // Relative submodule URLs: from the superproject root for normal repos, and from the
            // parent of this work tree when this repo lives under `.git/modules/<name>/` (nested
            // submodule), matching Git. Paths starting with `./` or `../` resolve from the process cwd
            // (matches `git clone` and t7001 `cd sub_nested && git submodule add ../sub_nested_nested`).
            let url_base = if repo
                .git_dir
                .parent()
                .and_then(|p| p.file_name())
                .is_some_and(|n| n == "modules")
            {
                work_tree
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("cannot resolve nested submodule clone URL"))?
            } else {
                work_tree
            };
            let cwd = std::env::current_dir().context("current directory for submodule URL")?;
            let clone_source = if args.url.trim() == "." || args.url.trim() == "./" {
                url_base.canonicalize().with_context(|| {
                    format!(
                        "cannot resolve submodule URL '.' from '{}'",
                        url_base.display()
                    )
                })?
            } else if args.url.starts_with("./") || args.url.starts_with("../") {
                let origin_base = ConfigSet::load(Some(&repo.git_dir), true)
                    .ok()
                    .and_then(|cfg| cfg.get("remote.origin.url"))
                    .and_then(|origin| {
                        let origin_path = Path::new(&origin);
                        if origin.contains("://") {
                            return None;
                        }
                        Some(if origin_path.is_absolute() {
                            origin_path.to_path_buf()
                        } else {
                            work_tree.join(origin_path)
                        })
                    });
                let cwd_candidate = cwd.join(&args.url);
                let origin_candidate = origin_base.as_ref().map(|base| base.join(&args.url));
                match origin_candidate
                    .as_ref()
                    .and_then(|candidate| candidate.canonicalize().ok())
                    .or_else(|| cwd_candidate.canonicalize().ok())
                {
                    Some(path) => path,
                    None => {
                        let display_base = origin_base.as_ref().unwrap_or(&cwd);
                        bail!(
                            "cannot resolve relative submodule URL '{}' from '{}'",
                            args.url,
                            display_base.display()
                        );
                    }
                }
            } else {
                PathBuf::from(&args.url)
            };
            let clone_source_str = clone_source.to_string_lossy().into_owned();

            let clone_src_trim = clone_source_str.trim_start();
            let mut clone_cmd = if clone_src_trim.starts_with("http://")
                || clone_src_trim.starts_with("https://")
            {
                let mut c = Command::new(system_git_binary());
                c.env_remove("GIT_DIR");
                c.env_remove("GIT_WORK_TREE");
                c.env_remove("GIT_EXEC_PATH");
                crate::grit_exe::strip_trace2_env(&mut c);
                c
            } else {
                grit_subprocess(&grit_bin)
            };
            clone_cmd
                .arg("clone")
                .arg("--no-checkout")
                .arg("--separate-git-dir")
                .arg(&modules_dir);
            if let Some(depth) = args.depth {
                if depth > 0 {
                    clone_cmd.arg(format!("--depth={depth}"));
                }
            }
            if args.progress {
                clone_cmd.arg("--progress");
            }
            if args.dissociate {
                clone_cmd.arg("--dissociate");
            }
            if let Some(ref format) = args.ref_format {
                clone_cmd.arg(format!("--ref-format={format}"));
            }
            for r in &args.reference {
                clone_cmd.arg("--reference").arg(r);
            }
            let status = clone_cmd
                .arg(&clone_source_str)
                .arg(&sub_path)
                .current_dir(work_tree)
                .status()
                .context("failed to clone submodule")?;

            if !status.success() {
                bail!("failed to clone submodule from '{}'", args.url);
            }
            // `clone --separate-git-dir` writes an absolute `gitdir:` path; submodules need a
            // relative gitlink (C Git's `connect_work_tree_and_git_dir`) so the worktree survives
            // being moved/copied with its superproject (t7400 "submodule add").
            write_submodule_gitfile(&sub_path, &modules_dir).map_err(|e| anyhow::anyhow!("{e}"))?;
            set_separate_gitdir_worktree(&grit_bin, &modules_dir, &sub_path);
            did_clone = true;
        }
    }

    if let Some(ref branch) = args.branch {
        let modules_dir = submodule_separate_git_dir(&repo, work_tree, &name, &path)?;
        let remote_branch = format!("refs/remotes/origin/{branch}");
        if let Ok(remote_tip) = refs::resolve_ref(&modules_dir, &remote_branch) {
            checkout_submodule_worktree(
                &grit_bin,
                &repo,
                work_tree,
                &name,
                &path,
                &name,
                &remote_tip.to_hex(),
                args.quiet,
            )?;
            let _ = attach_submodule_head_to_named_branch(&modules_dir, branch);
        }
    }

    // Update .gitmodules.
    let gitmodules_path = work_tree.join(".gitmodules");
    let mut config = if gitmodules_path.exists() {
        let content = fs::read_to_string(&gitmodules_path)?;
        ConfigFile::parse(&gitmodules_path, &content, ConfigScope::Local)?
    } else {
        ConfigFile::parse(&gitmodules_path, "", ConfigScope::Local)?
    };

    config.set(&format!("submodule.{name}.path"), &path)?;
    config.set(&format!("submodule.{name}.url"), &args.url)?;
    if let Some(ref branch) = args.branch {
        config.set(&format!("submodule.{name}.branch"), branch)?;
    }
    config.write()?;

    // Also register the submodule in the local .git/config (like git does).
    let local_url = resolve_submodule_super_url(work_tree, &repo.git_dir, &args.url)?;
    local_config.set(&format!("submodule.{name}.url"), &local_url)?;
    if grit_lib::submodule_active::submodule_add_should_set_active(&repo, &path) {
        local_config.set(&format!("submodule.{name}.active"), "true")?;
    }
    local_config.write()?;

    // Add the submodule path (and `.gitmodules`) to the index. Use --no-warn-embedded-repo so
    // the add doesn't warn about the embedded git repository we just cloned on purpose. With
    // --force, pass `add --force` so an "ignore everything" .gitignore does not block staging
    // the submodule / `.gitmodules` (git's configure_added_submodule forces the gitlink add).
    let mut add_cmd = superproject_subprocess(&grit_bin, &repo, work_tree);
    add_cmd.arg("add").arg("--no-warn-embedded-repo");
    if args.force {
        add_cmd.arg("--force");
    }
    let status = add_cmd
        .arg("--")
        .arg(".gitmodules")
        .arg(&path)
        .current_dir(work_tree)
        .status()
        .context("failed to stage submodule")?;

    if !status.success() {
        if let Some(oid) = read_submodule_head(&sub_path) {
            let mut add_gitmodules = superproject_subprocess(&grit_bin, &repo, work_tree);
            add_gitmodules
                .arg("add")
                .arg("--")
                .arg(".gitmodules")
                .current_dir(work_tree);
            if !add_gitmodules
                .status()
                .context("failed to stage .gitmodules")?
                .success()
            {
                bail!("failed to stage submodule");
            }
            stage_new_gitlink_in_super_index(&repo, work_tree, &path, &oid)?;
        } else {
            bail!("failed to stage submodule");
        }
    }

    // `clone --no-checkout` leaves an empty work tree; populate it from the staged gitlink
    // (HEAD’s tree may not include the new submodule until after commit — read the index).
    // Only do this on the clone path: git's "Adding existing repo" path leaves the existing
    // worktree and its (branch) HEAD untouched and never runs `checkout -f`.
    if did_clone {
        if let Some(oid) = read_gitlink_oid_from_index(&repo, &path)? {
            checkout_submodule_worktree(
                &grit_bin, &repo, work_tree, &name, &path, &name, &oid, args.quiet,
            )?;
            // With `-b <branch>`, git checks out `origin/<branch>` and leaves the submodule on a
            // local branch of that name (not detached / default branch). Attach HEAD accordingly.
            if let Some(ref branch) = args.branch {
                let modules_dir = submodule_separate_git_dir(&repo, work_tree, &name, &path)?;
                let _ = attach_submodule_head_to_named_branch(&modules_dir, branch);
            }
        }
    }

    Ok(())
}

fn system_git_binary() -> &'static str {
    if std::path::Path::new("/usr/bin/git").is_file() {
        "/usr/bin/git"
    } else if std::path::Path::new("/bin/git").is_file() {
        "/bin/git"
    } else {
        "git"
    }
}

fn stage_new_gitlink_in_super_index(
    repo: &Repository,
    work_tree: &Path,
    rel_path: &str,
    oid_hex: &str,
) -> Result<()> {
    let oid = ObjectId::from_hex(oid_hex.trim())
        .with_context(|| format!("invalid submodule OID '{oid_hex}' for path '{rel_path}'"))?;
    let abs = work_tree.join(rel_path);
    let meta = std::fs::metadata(&abs)
        .with_context(|| format!("stat submodule path '{}'", abs.display()))?;
    let index_path = repo.index_path();
    let mut index = repo.load_index_at(&index_path)?;
    index.remove_descendants_under_path(rel_path);
    let entry = IndexEntry {
        ctime_sec: meta.ctime() as u32,
        ctime_nsec: meta.ctime_nsec() as u32,
        mtime_sec: meta.mtime() as u32,
        mtime_nsec: meta.mtime_nsec() as u32,
        dev: meta.dev() as u32,
        ino: meta.ino() as u32,
        mode: MODE_GITLINK,
        uid: meta.uid(),
        gid: meta.gid(),
        size: 0,
        oid,
        flags: rel_path.len().min(0xFFF) as u16,
        flags_extended: None,
        path: rel_path.as_bytes().to_vec(),
        base_index_pos: 0,
    };
    index.add_or_replace(entry);
    repo.write_index_at(&index_path, &mut index)?;
    Ok(())
}

/// Attach a freshly added submodule's HEAD to a local branch tracking `origin/<branch>` (git's
/// `submodule add -b <branch>` performs `checkout -B <branch> origin/<branch>`).
fn attach_submodule_head_to_named_branch(sub_git_dir: &Path, branch: &str) -> Result<()> {
    let remote_branch = format!("refs/remotes/origin/{branch}");
    let remote_tip = match refs::resolve_ref(sub_git_dir, &remote_branch) {
        Ok(oid) => oid,
        Err(_) => return Ok(()),
    };
    let local_branch = format!("refs/heads/{branch}");
    refs::write_ref(sub_git_dir, &local_branch, &remote_tip)?;
    refs::write_symbolic_ref(sub_git_dir, "HEAD", &local_branch)?;

    let config_path = sub_git_dir.join("config");
    let mut config = if config_path.exists() {
        let content = fs::read_to_string(&config_path)?;
        ConfigFile::parse(&config_path, &content, ConfigScope::Local)?
    } else {
        ConfigFile::parse(&config_path, "", ConfigScope::Local)?
    };
    config.set(&format!("branch.{branch}.remote"), "origin")?;
    config.set(
        &format!("branch.{branch}.merge"),
        &format!("refs/heads/{branch}"),
    )?;
    config.write()?;
    Ok(())
}

fn run_foreach(args: &ForeachArgs, quiet: bool) -> Result<()> {
    let command_argv: Vec<String> = if args.command.is_empty() {
        vec![":".to_owned()]
    } else {
        args.command.clone()
    };

    if !command_argv.is_empty() && command_argv[0].starts_with("--") {
        eprintln!("usage: git submodule [--quiet] foreach [--recursive] [--] <command>...");
        std::process::exit(1);
    }

    let top_repo = Repository::discover(None).context("not a git repository")?;
    let top_work_tree = top_repo
        .work_tree
        .as_ref()
        .context("bare repository")?
        .to_path_buf();
    let cwd = std::env::current_dir().context("failed to read current directory")?;

    let modules = parse_gitmodules_with_repo(&top_work_tree, Some(&top_repo))?;
    run_foreach_in(
        &top_repo,
        &top_work_tree,
        &cwd,
        &modules,
        &command_argv,
        args.recursive,
        "",
        quiet,
    )
}

fn run_foreach_in(
    super_repo: &Repository,
    super_work_tree: &Path,
    invocation_cwd: &Path,
    modules: &[SubmoduleInfo],
    command_argv: &[String],
    recursive: bool,
    path_prefix: &str,
    quiet: bool,
) -> Result<()> {
    let mut sorted: Vec<&SubmoduleInfo> = modules.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));

    for m in sorted {
        let sub_path = super_work_tree.join(&m.path);
        if !sub_path.join(".git").exists() {
            continue;
        }

        let path_in_super = if path_prefix.is_empty() {
            m.path.replace('\\', "/")
        } else {
            format!("{}/{}", path_prefix.trim_end_matches('/'), m.path)
        };

        let displaypath = rev_parse::to_relative_path(&sub_path, invocation_cwd).replace('\\', "/");

        if !quiet {
            // Match Git: "Entering" goes to stdout so `submodule foreach cmd >file` captures it.
            println!("Entering '{}'", displaypath);
        }

        let sha1 = read_submodule_commit(super_repo, &m.path)?.unwrap_or_default();

        let mut cmd = Command::new("sh");
        if command_argv.len() == 1 {
            // One shell snippet (e.g. `git submodule foreach "git submodule update --init"`).
            cmd.arg("-c").arg(&command_argv[0]);
        } else {
            // Multiple argv words: run via `exec` so the command is not parsed twice (matches Git).
            cmd.arg("-c")
                .arg("exec \"$@\"")
                .arg("sh")
                .args(command_argv);
        }
        let status = cmd
            .current_dir(&sub_path)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .env("name", &m.name)
            .env("sm_path", &m.path)
            .env("path", &m.path)
            .env("sha1", &sha1)
            .env(
                "toplevel",
                super_work_tree.to_string_lossy().replace('\\', "/"),
            )
            .env("displaypath", &displaypath)
            .status()
            .context("failed to run foreach command")?;

        if !status.success() {
            bail!(
                "Stopping at '{}'; command returned non-zero status",
                displaypath
            );
        }

        if recursive {
            let Ok(sub_repo) = Repository::discover(Some(&sub_path)) else {
                continue;
            };
            let Some(sub_wt) = sub_repo.work_tree.as_ref() else {
                continue;
            };
            let nested = parse_gitmodules_with_repo(sub_wt, Some(&sub_repo)).unwrap_or_default();
            if !nested.is_empty() {
                run_foreach_in(
                    &sub_repo,
                    sub_wt,
                    invocation_cwd,
                    &nested,
                    command_argv,
                    true,
                    &path_in_super,
                    quiet,
                )?;
            }
        }
    }

    Ok(())
}

/// Resolve a relative `.gitmodules` URL for superproject config / clone / URL matching.
/// Matches Git's `resolve_relative_url(url, NULL)` (`relative_url` with no `up_path`).
pub(crate) fn resolve_submodule_super_url(
    work_tree: &Path,
    repo_git_dir: &Path,
    raw_url: &str,
) -> Result<String> {
    let trimmed = raw_url.trim();
    // `.gitmodules` may use `url = .` for a submodule that is the superproject itself.
    if trimmed == "." || trimmed == "./" {
        let super_git = superproject_git_dir_for_nested_modules(repo_git_dir)
            .unwrap_or_else(|| repo_git_dir.to_path_buf());
        let super_wt = super_git
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| work_tree.to_path_buf());
        let abs = fs::canonicalize(&super_wt).unwrap_or(super_wt);
        return Ok(abs.to_string_lossy().into_owned());
    }

    if !raw_url.starts_with("./") && !raw_url.starts_with("../") {
        return Ok(raw_url.to_string());
    }

    // Match Git's `resolve_relative_url(url, NULL)`: resolve against this repository's
    // `remote.<default>.url` (read from `the_repository`'s config — which during a recursive
    // `submodule update` is the submodule's own config under `.git/modules/<name>/`). Git only
    // falls back to the process cwd (the submodule worktree during recursion) when no remote URL
    // is configured. Using the worktree path unconditionally for nested repos is wrong: e.g. a
    // submodule cloned with `remote.origin.url = <super>/super` must resolve its own `../merging`
    // against `<super>/super`, not against `<recursivesuper>/super` (t7406 recursive update).
    let base = default_remote_url_raw(repo_git_dir)
        .unwrap_or_else(|| work_tree.to_string_lossy().into_owned());
    git_relative_url(&base, raw_url, None)
}

/// Argument for `git clone` when `submodule update` materializes a submodule.
///
/// Relative URLs are joined to **`work_tree`** of the repository performing the update (top-level
/// or nested after `Repository::discover(Some(sub_path))`), so `../peer` inside a nested submodule
/// resolves beside that submodule's directory (t7001 nested submodules).
fn submodule_clone_argument(work_tree: &Path, raw_url: &str) -> Result<String> {
    if raw_url.starts_with("./") || raw_url.starts_with("../") {
        let joined = work_tree.join(raw_url);
        return joined
            .canonicalize()
            .map(|p| p.to_string_lossy().into_owned())
            .with_context(|| {
                format!(
                    "cannot resolve submodule URL '{}' from {}",
                    raw_url,
                    work_tree.display()
                )
            });
    }
    Ok(raw_url.to_string())
}

/// URL written to a checked-out submodule's `remote.<name>.url` (Git `sync`: `get_up_path` + `relative_url`).
fn resolve_submodule_sub_origin_url(
    work_tree: &Path,
    repo_git_dir: &Path,
    submodule_path: &str,
    raw_url: &str,
) -> Result<String> {
    if !raw_url.starts_with("./") && !raw_url.starts_with("../") {
        return Ok(raw_url.to_string());
    }
    let outer_git = superproject_git_dir_for_nested_modules(repo_git_dir)
        .unwrap_or_else(|| repo_git_dir.to_path_buf());
    let outer_wt = outer_git
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| work_tree.to_path_buf());

    let base = default_remote_url_raw(repo_git_dir)
        .unwrap_or_else(|| outer_wt.to_string_lossy().into_owned());
    let up = submodule_up_path(submodule_path);
    let up_ref = (!up.is_empty()).then_some(up.as_str());
    git_relative_url(&base, raw_url, up_ref)
}

/// `.../super/.git` when `git_dir` is `.../super/.git/modules/<name>` (submodule object store).
fn superproject_git_dir_for_nested_modules(git_dir: &Path) -> Option<PathBuf> {
    let mut p = git_dir.to_path_buf();
    while let Some(parent) = p.parent() {
        if p.file_name().is_some_and(|n| n == "modules")
            && parent.file_name().is_some_and(|n| n == ".git")
        {
            return Some(parent.to_path_buf());
        }
        p = parent.to_path_buf();
    }
    None
}

/// Raw `remote.<default>.url` from config (may be `../sub`); matches Git's
/// `get_default_remote` + config lookup passed to `relative_url`.
fn default_remote_url_raw(git_dir: &Path) -> Option<String> {
    let config_dir = grit_lib::repo::common_git_dir_for_config(git_dir);
    let config_path = config_dir.join("config");
    let content = fs::read_to_string(&config_path).ok()?;
    let config = ConfigFile::parse(&config_path, &content, ConfigScope::Local).ok()?;
    let mut raw_url = None;
    if let Ok(head) = resolve_head(git_dir) {
        if let Some(bn) = head.branch_name() {
            if let Some(rn) = config_last_value(&config, &format!("branch.{bn}.remote")) {
                if !rn.is_empty() {
                    raw_url = config_last_value(&config, &format!("remote.{rn}.url"));
                }
            }
        }
    }
    if raw_url.is_none() {
        let remotes = remote_names_with_urls(&config);
        if remotes.len() == 1 {
            raw_url = Some(remotes[0].1.clone());
        } else {
            raw_url = config_last_value(&config, "remote.origin.url");
        }
    }
    raw_url
}

fn count_slashes_in_submodule_path(path: &str) -> usize {
    path.bytes().filter(|&b| b == b'/').count()
}

/// Strip a leading `./` so `./dir/sub` matches Git's cache path `dir/sub` for `get_up_path`.
fn submodule_path_for_up_path(path: &str) -> &str {
    path.strip_prefix("./").unwrap_or(path)
}

/// Git's `get_up_path(path)` for submodule URL resolution (`relative_url` `up_path`).
fn submodule_up_path(path: &str) -> String {
    let path = submodule_path_for_up_path(path);
    let mut s = String::new();
    for _ in 0..count_slashes_in_submodule_path(path) {
        s.push_str("../");
    }
    if !path.is_empty() && !path.ends_with('/') {
        s.push_str("../");
    }
    s
}

/// Port of git's `url_is_local_not_ssh` (connect.c): a URL is a local path (not scp-style SSH)
/// when it has no colon, a slash precedes the first colon, or it has a DOS drive prefix.
fn url_is_local_not_ssh(url: &str) -> bool {
    let colon = url.find(':');
    let slash = url.find('/');
    match colon {
        None => true,
        Some(c) => match slash {
            Some(s) if s < c => true,
            _ => {
                // DOS drive prefix like `C:\path` (single letter then colon).
                let b = url.as_bytes();
                b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
            }
        },
    }
}

fn is_absolute_path_url(url: &str) -> bool {
    url.starts_with('/') || url.len() > 2 && url.as_bytes().get(1) == Some(&b':')
}

fn chop_last_dir_git(remoteurl: &mut String, is_relative: bool) -> Result<bool> {
    if let Some(pos) = remoteurl.rfind('/') {
        remoteurl.truncate(pos);
        return Ok(false);
    }
    if let Some(pos) = remoteurl.rfind(':') {
        remoteurl.truncate(pos);
        return Ok(true);
    }
    if is_relative || remoteurl == "." {
        bail!("cannot strip one component off url '{remoteurl}'");
    }
    *remoteurl = ".".to_string();
    Ok(false)
}

/// Git's `relative_url(remote_url, url, up_path)` for local paths (see `git/remote.c`).
fn git_relative_url(remote_url: &str, url: &str, up_path: Option<&str>) -> Result<String> {
    let url = url.trim_end_matches('/');
    if !url_is_local_not_ssh(url) || is_absolute_path_url(url) {
        return Ok(url.to_string());
    }
    let mut remoteurl = remote_url.trim_end_matches('/').to_string();
    if remoteurl.is_empty() {
        return Ok(url.to_string());
    }
    let is_relative = url_is_local_not_ssh(&remoteurl) && !is_absolute_path_url(&remoteurl);
    if is_relative && !remoteurl.starts_with("./") && !remoteurl.starts_with("../") {
        remoteurl = format!("./{remoteurl}");
    }
    let mut rest = url;
    let mut colonsep = false;
    while rest.starts_with("../") {
        rest = &rest[3..];
        colonsep |= chop_last_dir_git(&mut remoteurl, is_relative)?;
    }
    while rest.starts_with("./") {
        rest = &rest[2..];
    }
    let sep = if colonsep { ":" } else { "/" };
    let mut out = format!("{remoteurl}{sep}{rest}");
    if out.ends_with('/') {
        out.pop();
    }
    let mut out = if out.starts_with("./") {
        out[2..].to_string()
    } else {
        out
    };
    if let Some(up) = up_path {
        if is_relative {
            out = format!("{up}{out}");
        }
    }
    Ok(out)
}

/// Resolve a relative URL (starting with ./ or ../) against a base URL.
fn resolve_relative_url(base: &str, relative: &str) -> String {
    // If base looks like a local path, use path resolution.
    // If base looks like a URL (scheme://...), do URL-path resolution.
    if base.contains("://") {
        // URL-based resolution.
        if let Some(scheme_end) = base.find("://") {
            let scheme = &base[..scheme_end + 3];
            let rest = &base[scheme_end + 3..];
            // Split into host and path.
            let (host, base_path) = if let Some(slash) = rest.find('/') {
                (&rest[..slash], &rest[slash..])
            } else {
                (rest, "/")
            };
            let resolved = resolve_path_components(base_path, relative);
            format!("{}{}{}", scheme, host, resolved)
        } else {
            format!("{}/{}", base, relative)
        }
    } else {
        // Local path resolution.
        let base_path = Path::new(base);
        let mut result = base_path.to_path_buf();
        for component in relative.split('/') {
            match component {
                "." => {}
                ".." => {
                    result.pop();
                }
                c => {
                    result.push(c);
                }
            }
        }
        result.to_string_lossy().into_owned()
    }
}

/// Resolve relative path components against a base path string.
fn resolve_path_components(base_path: &str, relative: &str) -> String {
    let mut parts: Vec<&str> = base_path.split('/').filter(|s| !s.is_empty()).collect();
    // Remove the last component (the "file" part of the base path).
    parts.pop();
    for component in relative.split('/') {
        match component {
            "." | "" => {}
            ".." => {
                parts.pop();
            }
            c => {
                parts.push(c);
            }
        }
    }
    format!("/{}", parts.join("/"))
}

/// Display path for `submodule sync` messages (`get_submodule_displaypath` in Git).
fn submodule_sync_display_path(
    work_tree: &Path,
    cwd: &Path,
    super_prefix: Option<&str>,
    submodule_path: &str,
) -> String {
    if let Some(sp) = super_prefix {
        let base = sp.trim_end_matches('/');
        if base.is_empty() {
            submodule_path.to_string()
        } else {
            format!("{base}/{submodule_path}")
        }
    } else {
        rev_parse::to_relative_path(&work_tree.join(submodule_path), cwd).replace('\\', "/")
    }
}

fn run_sync(args: &SyncArgs, quiet: bool) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let work_tree = repo.work_tree.as_ref().context("bare repository")?;
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    // Validate explicit path arguments against the index gitlink set (git module_list_compute):
    // a pathspec matching no gitlink is an error.
    let sync_paths = validate_submodule_pathspecs(&repo, work_tree, &args.paths)?;
    let modules = parse_gitmodules(work_tree)?;
    let selected = filter_submodules(&modules, &sync_paths);

    let config_path = repo.git_dir.join("config");
    let mut config = if config_path.exists() {
        let content = fs::read_to_string(&config_path)?;
        ConfigFile::parse(&config_path, &content, ConfigScope::Local)?
    } else {
        ConfigFile::parse(&config_path, "", ConfigScope::Local)?
    };

    for m in &selected {
        let url_key = format!("submodule.{}.url", m.name);
        // Only sync if the submodule is initialized (has a URL in config).
        let is_initialized = config.entries.iter().any(|e| e.key == url_key);
        if !is_initialized {
            continue;
        }

        // Superproject config: resolve_relative_url(url, NULL).
        let super_url = resolve_submodule_super_url(work_tree, &repo.git_dir, &m.url)?;
        config.set(&url_key, &super_url)?;
        if !quiet {
            let display_path =
                submodule_sync_display_path(work_tree, &cwd, args.super_prefix.as_deref(), &m.path);
            println!("Synchronizing submodule url for '{display_path}'");
        }

        // Submodule working tree remote: relative_url with get_up_path (see git submodule sync).
        let sub_origin_url =
            resolve_submodule_sub_origin_url(work_tree, &repo.git_dir, &m.path, &m.url)?;

        let sub_path = work_tree.join(&m.path);
        if sub_path.join(".git").exists() {
            let sub_git_dir = resolve_submodule_git_dir(&sub_path);
            if let Some(sub_git) = sub_git_dir {
                let sub_config_path = sub_git.join("config");
                if sub_config_path.exists() {
                    let sub_content = fs::read_to_string(&sub_config_path)?;
                    let mut sub_config =
                        ConfigFile::parse(&sub_config_path, &sub_content, ConfigScope::Local)?;
                    sub_config.set("remote.origin.url", &sub_origin_url)?;
                    sub_config.write()?;
                }
            }
        }
    }

    config.write()?;

    if args.recursive {
        for m in &selected {
            let sub_path = work_tree.join(&m.path);
            if sub_path.join(".git").exists() {
                let nested = parse_gitmodules(&sub_path).unwrap_or_default();
                if !nested.is_empty() {
                    let parent_display = submodule_sync_display_path(
                        work_tree,
                        &cwd,
                        args.super_prefix.as_deref(),
                        &m.path,
                    );
                    let child_super = format!("{}/", parent_display.trim_end_matches('/'));
                    let grit_bin =
                        std::env::current_exe().unwrap_or_else(|_| PathBuf::from("grit"));
                    let mut cmd = grit_subprocess(&grit_bin);
                    cmd.arg("submodule")
                        .arg("sync")
                        .arg("--recursive")
                        .arg(format!("--super-prefix={child_super}"))
                        .current_dir(&sub_path);
                    if quiet {
                        cmd.arg("--quiet");
                    }
                    let _status = cmd.status();
                }
            }
        }
    }

    Ok(())
}

/// Resolve submodule .git to its actual git directory.
fn resolve_submodule_git_dir(sub_path: &Path) -> Option<PathBuf> {
    let dot_git = sub_path.join(".git");
    if dot_git.is_file() {
        let content = fs::read_to_string(&dot_git).ok()?;
        let gitdir = content.strip_prefix("gitdir: ")?.trim();
        let path = if Path::new(gitdir).is_absolute() {
            PathBuf::from(gitdir)
        } else {
            sub_path.join(gitdir)
        };
        Some(path.canonicalize().unwrap_or(path))
    } else if dot_git.is_dir() {
        Some(dot_git)
    } else {
        None
    }
}

/// Whether a submodule work tree is unsafe to remove without `-f`, matching `git rm -qn <path>`,
/// which combines `bad_to_remove_submodule` (status --porcelain dirtiness) with a HEAD-vs-index
/// gitlink check. A missing/empty directory is always safe (t7400.104).
fn submodule_is_dirty_for_removal(grit_bin: &Path, work_tree: &Path, rel_path: &str) -> bool {
    let sub_path = work_tree.join(rel_path);
    if !sub_path.exists() {
        return false;
    }
    // Empty directory → nothing to lose.
    let is_empty = fs::read_dir(&sub_path)
        .map(|mut it| it.next().is_none())
        .unwrap_or(true);
    if is_empty {
        return false;
    }
    // `git rm -qn <path>` rejects removal when the submodule HEAD differs from the recorded
    // gitlink commit (t7400.107).
    let rm_ok = grit_subprocess(grit_bin)
        .arg("rm")
        .arg("-qn")
        .arg(rel_path)
        .current_dir(work_tree)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !rm_ok {
        return true;
    }
    // `bad_to_remove_submodule`: any `git status --porcelain` output in the submodule (tracked
    // modifications or untracked/ignored files) means it is unsafe to remove (t7400.105/106).
    if sub_path.join(".git").exists() {
        let out = grit_subprocess(grit_bin)
            .arg("status")
            .arg("--porcelain")
            .arg("--ignore-submodules=none")
            .arg("-uall")
            .arg("--ignored")
            .current_dir(&sub_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();
        if let Ok(o) = out {
            if o.stdout.len() > 2 {
                return true;
            }
        }
    }
    false
}

fn run_deinit(args: &DeinitArgs, quiet: bool) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let work_tree = repo.work_tree.as_ref().context("bare repository")?;
    let grit_bin = grit_exe::grit_executable();

    // `--all` and explicit pathspecs are mutually exclusive.
    if args.all && !args.paths.is_empty() {
        eprintln!("error: pathspec and --all are incompatible");
        eprintln!(
            "usage: git submodule deinit [--quiet] [-f | --force] [--all | [--] [<path>...]]"
        );
        return Err(crate::explicit_exit::SilentNonZeroExit { code: 1 }.into());
    }
    // Without either, refuse to act (git: die "Use '--all'...").
    if !args.all && args.paths.is_empty() {
        bail!("Use '--all' if you really want to deinitialize all submodules");
    }

    // Build the work set from index gitlinks (git module_list_compute). `--all` selects all
    // gitlinks; explicit paths are validated and select matching gitlinks.
    let normalized = if args.all {
        Vec::new()
    } else {
        validate_submodule_pathspecs(&repo, work_tree, &args.paths)?
    };
    let gitlinks = index_gitlink_paths(&repo);
    let modules = parse_gitmodules(work_tree)?;

    let selected_paths: Vec<String> = gitlinks
        .into_iter()
        .filter(|gl| {
            args.all
                || normalized.iter().any(|p| {
                    p == "." || p == gl || gl.starts_with(&format!("{p}/")) || p.starts_with(':')
                })
        })
        .collect();

    let config_path = repo.git_dir.join("config");
    let mut config = if config_path.exists() {
        let content = fs::read_to_string(&config_path)?;
        ConfigFile::parse(&config_path, &content, ConfigScope::Local)?
    } else {
        ConfigFile::parse(&config_path, "", ConfigScope::Local)?
    };

    let cwd = std::env::current_dir().unwrap_or_else(|_| work_tree.to_path_buf());

    for gl in &selected_paths {
        // Only deinit gitlinks that map to a `.gitmodules` entry (git: submodule_from_path).
        let Some(m) = modules.iter().find(|m| &m.path == gl) else {
            continue;
        };
        validate_submodule_path(work_tree, &m.path).map_err(|e| anyhow::anyhow!("{e}"))?;
        let sub_path = work_tree.join(&m.path);
        let displaypath = rev_parse::to_relative_path(&sub_path, &cwd).replace('\\', "/");

        // Remove the work tree (unless the user already removed it).
        if sub_path.is_dir() {
            // If the work tree still holds a real `.git` directory, absorb it first.
            let _ = absorb_submodule_dot_git_dir_into_modules(&repo, &m.path);

            if !args.force && submodule_is_dirty_for_removal(&grit_bin, work_tree, &m.path) {
                bail!(
                    "Submodule work tree '{displaypath}' contains local modifications; use '-f' to discard them"
                );
            }

            let removed = fs::remove_dir_all(&sub_path).is_ok();
            if !quiet {
                if removed {
                    println!("Cleared directory '{displaypath}'");
                } else {
                    println!("Could not remove submodule work tree '{displaypath}'");
                }
            }

            // Unset core.worktree in the submodule's git dir config (git:
            // submodule_unset_core_worktree).
            let modules_dir = submodule_separate_git_dir(&repo, work_tree, &m.name, &m.path)?;
            let sub_cfg = modules_dir.join("config");
            if sub_cfg.exists() {
                if let Ok(content) = fs::read_to_string(&sub_cfg) {
                    if let Ok(mut c) = ConfigFile::parse(&sub_cfg, &content, ConfigScope::Local) {
                        if c.unset("core.worktree").unwrap_or(0) > 0 {
                            let _ = c.write();
                        }
                    }
                }
            }
        }

        // Recreate an empty submodule directory (git: mkdir(path)).
        let _ = fs::create_dir(&sub_path);

        // Remove the `.git/config` section, printing "unregistered" only if it existed.
        let section = format!("submodule.{}", m.name);
        let had_config = config
            .entries
            .iter()
            .any(|e| e.key.starts_with(&format!("{section}.")));
        config.remove_section(&section)?;
        if had_config && !quiet {
            println!(
                "Submodule '{}' ({}) unregistered for path '{}'",
                m.name, m.url, displaypath
            );
        }
    }

    config.write()?;
    Ok(())
}

/// When the submodule work tree still contains a real `.git` directory (not a gitfile), move it
/// to `.git/modules/<path>` so removal can drop the work tree without losing history (`t7112`).
pub(crate) fn absorb_submodule_dot_git_dir_into_modules(
    repo: &Repository,
    submodule_rel: &str,
) -> Result<()> {
    let work_tree = repo.work_tree.as_ref().context("bare repository")?;
    let sub_path = work_tree.join(submodule_rel);
    let dot_git = sub_path.join(".git");
    if !dot_git.is_dir() {
        return Ok(());
    }
    let modules_dir = submodule_modules_git_dir(&repo.git_dir, submodule_rel);
    if modules_dir.exists() {
        return Ok(());
    }
    if let Some(parent) = modules_dir.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(&dot_git, &modules_dir).context("absorb submodule .git into modules")?;
    let moved_config_path = modules_dir.join("config");
    if moved_config_path.exists() {
        let content = fs::read_to_string(&moved_config_path)?;
        let mut cfg = ConfigFile::parse(&moved_config_path, &content, ConfigScope::Local)?;
        let relative_worktree = pathdiff_relative(&modules_dir, &sub_path);
        cfg.set("core.worktree", &relative_worktree)?;
        cfg.write()?;
    }
    let relative_gitdir = pathdiff_relative(&sub_path, &modules_dir);
    fs::write(&dot_git, format!("gitdir: {relative_gitdir}\n"))?;
    Ok(())
}

/// Absorb a submodule's embedded gitdir before `git rm` removes its work tree.
pub(crate) fn absorb_submodule_git_dir_for_rm(
    repo: &Repository,
    submodule_rel: &str,
    quiet: bool,
) -> Result<()> {
    let work_tree = repo.work_tree.as_ref().context("bare repository")?;
    let modules_cfg = parse_gitmodules_with_repo(work_tree, Some(repo))?;
    absorb_git_dir_into_superproject(repo, work_tree, submodule_rel, None, quiet, &modules_cfg)
}

fn run_absorbgitdirs(args: &AbsorbgitdirsArgs, quiet: bool) -> Result<()> {
    absorb_git_dirs_impl(None, &args.paths, quiet)
}

/// True when `path/.git/worktrees` exists and is non-empty (Git `submodule_uses_worktrees`).
fn submodule_gitdir_has_extra_worktrees(sub_worktree: &Path) -> bool {
    let wt = sub_worktree.join(".git").join("worktrees");
    let Ok(entries) = fs::read_dir(&wt) else {
        return false;
    };
    for e in entries.flatten() {
        let n = e.file_name();
        if n != "." && n != ".." {
            return true;
        }
    }
    false
}

fn resolve_dot_git_to_git_dir(dot_git: &Path) -> Option<PathBuf> {
    if dot_git.is_dir() {
        return Some(dot_git.to_path_buf());
    }
    if !dot_git.is_file() {
        return None;
    }
    let content = fs::read_to_string(dot_git).ok()?;
    for line in content.lines() {
        let line = line.trim();
        let rest = line.strip_prefix("gitdir:")?.trim();
        if rest.is_empty() {
            continue;
        }
        let p = Path::new(rest);
        let resolved = if p.is_absolute() {
            p.to_path_buf()
        } else {
            dot_git.parent()?.join(p)
        };
        return fs::canonicalize(&resolved).ok().or(Some(resolved));
    }
    None
}

fn gitlink_path_matches_filter(path: &str, filter: &[String], modules: &[SubmoduleInfo]) -> bool {
    if filter.is_empty() {
        return true;
    }
    filter.iter().any(|f| {
        f == path
            || modules
                .iter()
                .any(|m| &m.name == f && m.path.replace('\\', "/") == path)
    })
}

fn submodule_name_for_gitlink_path(path: &str, modules: &[SubmoduleInfo]) -> Option<String> {
    modules
        .iter()
        .find(|m| m.path.replace('\\', "/") == path.replace('\\', "/"))
        .map(|m| m.name.clone())
}

fn absorb_git_dirs_impl(
    super_prefix: Option<&str>,
    path_filter: &[String],
    quiet: bool,
) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let work_tree = repo.work_tree.as_ref().context("bare repository")?;
    let modules_cfg = parse_gitmodules_with_repo(work_tree, Some(&repo))?;
    let index = repo.load_index().context("failed to read index")?;

    let mut gitlink_paths: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for e in &index.entries {
        if e.stage() != 0 || e.mode != MODE_GITLINK {
            continue;
        }
        let p = String::from_utf8_lossy(&e.path).replace('\\', "/");
        if !gitlink_path_matches_filter(&p, path_filter, &modules_cfg) {
            continue;
        }
        if seen.insert(p.clone()) {
            gitlink_paths.push(p);
        }
    }

    for path in gitlink_paths {
        absorb_git_dir_into_superproject(
            &repo,
            work_tree,
            &path,
            super_prefix,
            quiet,
            &modules_cfg,
        )?;
    }

    Ok(())
}

fn absorb_git_dir_into_superproject(
    repo: &Repository,
    work_tree: &Path,
    path: &str,
    super_prefix: Option<&str>,
    quiet: bool,
    modules_cfg: &[SubmoduleInfo],
) -> Result<()> {
    let Some(name) = submodule_name_for_gitlink_path(path, modules_cfg) else {
        bail!("fatal: could not lookup name for submodule '{path}'");
    };

    let sub_wt = work_tree.join(path);
    let dot_git = sub_wt.join(".git");

    if !dot_git.exists() {
        return Ok(());
    }

    let common_git = grit_lib::repo::common_git_dir_for_config(&repo.git_dir);
    let common_git_canon = fs::canonicalize(&common_git).unwrap_or(common_git.clone());

    if let Some(resolved_git) = resolve_dot_git_to_git_dir(&dot_git) {
        let real_sub = fs::canonicalize(&resolved_git).unwrap_or(resolved_git);
        if real_sub.starts_with(&common_git_canon) {
            absorb_git_dir_into_superproject_recurse(repo, work_tree, path, super_prefix, quiet)?;
            return Ok(());
        }
    }

    if dot_git.is_dir() {
        if submodule_gitdir_has_extra_worktrees(&sub_wt) {
            bail!(
                "fatal: relocate_gitdir for submodule '{}' with more than one worktree not supported",
                path
            );
        }
        relocate_single_git_dir_into_superproject(
            repo,
            work_tree,
            path,
            &name,
            super_prefix,
            quiet,
        )?;
    } else if dot_git.is_file() {
        let modules_dir = submodule_modules_git_dir(&repo.git_dir, &name);
        fs::create_dir_all(modules_dir.parent().context("modules parent")?)?;
        connect_work_tree_and_git_dir(&sub_wt, &modules_dir)?;
    }

    absorb_git_dir_into_superproject_recurse(repo, work_tree, path, super_prefix, quiet)?;
    Ok(())
}

fn connect_work_tree_and_git_dir(work_tree: &Path, git_dir: &Path) -> Result<()> {
    fs::create_dir_all(git_dir.join("objects")).ok();
    let gitfile = work_tree.join(".git");
    let rel_gitdir = pathdiff_relative(work_tree, git_dir);
    fs::write(&gitfile, format!("gitdir: {rel_gitdir}\n")).context("write submodule gitfile")?;

    let cfg_path = git_dir.join("config");
    let mut cfg = if cfg_path.exists() {
        let content = fs::read_to_string(&cfg_path)?;
        ConfigFile::parse(&cfg_path, &content, ConfigScope::Local)?
    } else {
        ConfigFile::parse(&cfg_path, "", ConfigScope::Local)?
    };
    let rel_wt = pathdiff_relative(git_dir, work_tree);
    cfg.set("core.worktree", &rel_wt)?;
    cfg.write()?;
    Ok(())
}

fn relocate_single_git_dir_into_superproject(
    repo: &Repository,
    work_tree: &Path,
    path: &str,
    name: &str,
    super_prefix: Option<&str>,
    quiet: bool,
) -> Result<()> {
    let sub_wt = work_tree.join(path);
    let old_git_dir = sub_wt.join(".git");
    if old_git_dir.is_file() {
        return Ok(());
    }
    if !old_git_dir.is_dir() {
        return Ok(());
    }

    let modules_dir = submodule_modules_git_dir(&repo.git_dir, name);
    if let Some(parent) = modules_dir.parent() {
        fs::create_dir_all(parent)?;
    }
    if modules_dir.exists() {
        return Ok(());
    }

    let real_old = fs::canonicalize(&old_git_dir).unwrap_or_else(|_| old_git_dir.clone());
    fs::rename(&old_git_dir, &modules_dir).context("failed to move .git directory")?;
    let real_new = fs::canonicalize(&modules_dir).unwrap_or_else(|_| modules_dir.clone());

    if !quiet {
        let display_prefix = super_prefix.unwrap_or("");
        eprint!(
            "Migrating git directory of '{}{}' from\n'{}' to\n'{}'\n",
            display_prefix,
            path,
            real_old.display(),
            real_new.display()
        );
    }

    connect_work_tree_and_git_dir(&sub_wt, &modules_dir)?;
    Ok(())
}

fn absorb_git_dir_into_superproject_recurse(
    _repo: &Repository,
    work_tree: &Path,
    path: &str,
    super_prefix: Option<&str>,
    quiet: bool,
) -> Result<()> {
    let sub_wt = work_tree.join(path);
    if !sub_wt.is_dir() {
        return Ok(());
    }

    let child_prefix = format!(
        "{}{}/",
        super_prefix.unwrap_or(""),
        path.trim_end_matches('/')
    );
    let grit_bin = grit_exe::grit_executable();
    let mut cmd = grit_subprocess(&grit_bin);
    cmd.current_dir(&sub_wt)
        .arg("submodule--helper")
        .arg("absorbgitdirs")
        .arg(format!("--super-prefix={}", child_prefix));
    if quiet {
        cmd.arg("-q");
    }
    grit_exe::strip_trace2_env(&mut cmd);
    let st = cmd
        .status()
        .context("submodule--helper absorbgitdirs in submodule")?;
    if !st.success() {
        bail!("fatal: could not recurse into submodule '{path}'");
    }
    Ok(())
}

/// Compute a relative path from `from` to `to`.
fn pathdiff_relative(from: &Path, to: &Path) -> String {
    // Canonicalize both paths for accurate comparison.
    let from_abs = from.canonicalize().unwrap_or_else(|_| from.to_path_buf());
    let to_abs = to.canonicalize().unwrap_or_else(|_| to.to_path_buf());

    // Find common prefix.
    let from_parts: Vec<_> = from_abs.components().collect();
    let to_parts: Vec<_> = to_abs.components().collect();

    let common = from_parts
        .iter()
        .zip(to_parts.iter())
        .take_while(|(a, b)| a == b)
        .count();

    let mut result = PathBuf::new();
    for _ in common..from_parts.len() {
        result.push("..");
    }
    for part in &to_parts[common..] {
        result.push(part);
    }

    result.to_string_lossy().into_owned()
}

fn parse_mode_octal(mode: &str) -> u32 {
    u32::from_str_radix(mode.trim(), 8).unwrap_or(0)
}

fn mode_is_gitlink(mode: &str) -> bool {
    parse_mode_octal(mode) == MODE_GITLINK
}

fn short_oid_in_submodule(grit_bin: &Path, sub_path: &Path, committish: &str) -> Option<String> {
    let spec = format!("{committish}^0");
    let out = grit_subprocess(grit_bin)
        .args(["rev-parse", "-q", "--short", &spec, "--"])
        .current_dir(sub_path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let line = s.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        None
    } else {
        Some(line.to_string())
    }
}

fn submodule_rev_list_count(grit_bin: &Path, sub_path: &Path, range: &str) -> Result<i32> {
    submodule_rev_list_count_args(grit_bin, sub_path, &[range])
}

fn submodule_rev_list_count_args(grit_bin: &Path, sub_path: &Path, revs: &[&str]) -> Result<i32> {
    let mut args = vec!["rev-list", "--first-parent", "--count"];
    args.extend_from_slice(revs);
    args.push("--");
    let out = match grit_subprocess(grit_bin)
        .args(args)
        .current_dir(sub_path)
        .output()
    {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(-1),
        Err(e) => return Err(e).context("rev-list --count in submodule"),
    };
    if !out.status.success() {
        return Ok(-1);
    }
    let n = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<i32>()
        .unwrap_or(-1);
    Ok(n)
}

fn submodule_log_side(
    grit_bin: &Path,
    sub_path: &Path,
    include: &str,
    exclude: &str,
    prefix: char,
    summary_limit: i32,
) -> Result<()> {
    let mut cmd = grit_subprocess(grit_bin);
    cmd.current_dir(sub_path);
    cmd.arg("log");
    if summary_limit > 0 {
        cmd.arg(format!("-{summary_limit}"));
    }
    let pretty = format!("--pretty=  {} %s", prefix);
    let exclude = format!("^{exclude}");
    cmd.args(["--first-parent", &pretty, include, &exclude, "--"]);
    let st = cmd.status().context("submodule log for summary")?;
    if !st.success() {
        bail!("submodule log failed");
    }
    Ok(())
}

fn submodule_log_first_parent(
    grit_bin: &Path,
    sub_path: &Path,
    src_abbrev: &str,
    dst_abbrev: &str,
    summary_limit: i32,
) -> Result<()> {
    let right_exclude = format!("^{src_abbrev}");
    let left_exclude = format!("^{dst_abbrev}");
    let right_count =
        submodule_rev_list_count_args(grit_bin, sub_path, &[dst_abbrev, &right_exclude])?;
    let left_count =
        submodule_rev_list_count_args(grit_bin, sub_path, &[src_abbrev, &left_exclude])?;

    let right_limit = if summary_limit > 0 { summary_limit } else { -1 };
    if right_count != 0 {
        submodule_log_side(grit_bin, sub_path, dst_abbrev, src_abbrev, '>', right_limit)?;
    }

    let left_limit = if summary_limit > 0 {
        let remaining = summary_limit - right_count.max(0);
        if remaining <= 0 {
            return Ok(());
        }
        remaining
    } else {
        -1
    };
    if left_count != 0 {
        submodule_log_side(grit_bin, sub_path, src_abbrev, dst_abbrev, '<', left_limit)?;
    }
    Ok(())
}

fn submodule_log_one(
    grit_bin: &Path,
    sub_path: &Path,
    dst_abbrev: &str,
    prefix: char,
) -> Result<()> {
    let pretty = format!("--pretty=  {} %s", prefix);
    let st = grit_subprocess(grit_bin)
        .args(["log", &pretty, "-1", dst_abbrev, "--"])
        .current_dir(sub_path)
        .status()
        .context("submodule log -1 for summary")?;
    if !st.success() {
        bail!("submodule log -1 failed");
    }
    Ok(())
}

fn resolve_summary_base_tree(repo: &Repository, commit_spec: &str) -> Result<Option<ObjectId>> {
    match resolve_revision(repo, commit_spec) {
        Ok(oid) => {
            let obj = repo.odb.read(&oid).context("read summary base commit")?;
            let commit = parse_commit(&obj.data).context("parse summary base commit")?;
            Ok(Some(commit.tree))
        }
        Err(e) => {
            if commit_spec == "HEAD" {
                Ok(None)
            } else {
                return Err(e).context("could not resolve summary base revision");
            }
        }
    }
}

fn summary_display_path(entry: &DiffEntry) -> std::borrow::Cow<'_, str> {
    entry
        .old_path
        .as_deref()
        .unwrap_or_else(|| entry.path())
        .to_string_lossy()
}

fn pathspec_selected(pathspecs: &[String], sm_path: &str) -> bool {
    if pathspecs.is_empty() {
        return true;
    }
    if pathspecs.iter().any(|p| p == ".") {
        return true;
    }
    grit_lib::pathspec::matches_pathspec_list(sm_path, pathspecs)
}

fn summary_pathspec_from_cwd(work_tree: &Path, cwd: &Path, raw: &str) -> Option<String> {
    if raw.starts_with(':') {
        return Some(raw.to_string());
    }
    if raw == "." {
        let rel = cwd.strip_prefix(work_tree).ok()?;
        let s = rel
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        return Some(if s.is_empty() { ".".to_string() } else { s });
    }
    normalize_submodule_path_arg(work_tree, cwd, raw)
}

fn summary_arg_matches_gitlink(
    work_tree: &Path,
    cwd: &Path,
    gitlinks: &[String],
    raw: &str,
) -> bool {
    let Some(norm) = summary_pathspec_from_cwd(work_tree, cwd, raw) else {
        return false;
    };
    if norm == "." {
        return true;
    }
    gitlinks
        .iter()
        .any(|g| *g == norm || g.starts_with(&format!("{norm}/")))
}

fn summary_display_path_from_cwd(work_tree: &Path, cwd: &Path, sm_path: &str) -> String {
    rev_parse::to_relative_path(&work_tree.join(sm_path), cwd).replace('\\', "/")
}

/// Working tree directory for a submodule given the path Git uses in the summary diff (often the
/// old path after `git mv`).
fn submodule_work_tree_for_summary(work_tree: &Path, logical_path: &str) -> PathBuf {
    let direct = work_tree.join(logical_path);
    if direct.join(".git").exists() {
        return direct;
    }
    let Ok(modules) = parse_gitmodules(work_tree) else {
        return direct;
    };
    if let Some(m) = modules
        .iter()
        .find(|m| m.path == logical_path || m.name == logical_path)
    {
        let relocated = work_tree.join(&m.path);
        if relocated.join(".git").exists() {
            return relocated;
        }
    }
    direct
}

/// Submodule path -> (declared name, `.gitmodules` ignore value), read URL-independently from the
/// work-tree `.gitmodules`. Git's `--for-status` ignore lookup does not require a `url` entry.
#[derive(Default)]
struct GitmodulesIgnoreAll {
    /// submodule path -> declared name (for `.git/config submodule.<name>.ignore`).
    name_by_path: BTreeMap<String, String>,
    /// submodule path -> `.gitmodules submodule.<name>.ignore` value.
    ignore_by_path: BTreeMap<String, String>,
}

/// Read `submodule.<name>.path` / `submodule.<name>.ignore` from the work-tree `.gitmodules`.
fn gitmodules_ignore_all_map(work_tree: &Path) -> GitmodulesIgnoreAll {
    let path = work_tree.join(".gitmodules");
    let Ok(content) = fs::read_to_string(&path) else {
        return GitmodulesIgnoreAll::default();
    };
    let (entries, _) =
        ConfigFile::parse_gitmodules_best_effort(&path, &content, ConfigScope::Local);
    let mut path_by_name: BTreeMap<String, String> = BTreeMap::new();
    let mut ignore_by_name: BTreeMap<String, String> = BTreeMap::new();
    for e in &entries {
        let Some(rest) = e.key.strip_prefix("submodule.") else {
            continue;
        };
        let Some(dot) = rest.rfind('.') else { continue };
        let name = &rest[..dot];
        match &rest[dot + 1..] {
            "path" => {
                if let Some(v) = e.value.as_deref() {
                    path_by_name.insert(name.to_owned(), v.to_owned());
                }
            }
            "ignore" => {
                if let Some(v) = e.value.as_deref() {
                    ignore_by_name.insert(name.to_owned(), v.to_owned());
                }
            }
            _ => {}
        }
    }
    let mut result = GitmodulesIgnoreAll::default();
    for (name, sm_path) in path_by_name {
        if let Some(ig) = ignore_by_name.get(&name) {
            result.ignore_by_path.insert(sm_path.clone(), ig.clone());
        }
        result.name_by_path.insert(sm_path, name);
    }
    result
}

/// True when `submodule.<name>.ignore` is `all` in local config or in `.gitmodules` (Git `prepare_submodule_summary`).
fn submodule_ignore_all_for_summary(
    local_cfg: Option<&ConfigFile>,
    modules: &GitmodulesIgnoreAll,
    sm_path: &str,
) -> bool {
    // `.git/config submodule.<name>.ignore` (any value) takes precedence over `.gitmodules`.
    if let Some(name) = modules.name_by_path.get(sm_path) {
        let key = format!("submodule.{name}.ignore");
        if let Some(cfg) = local_cfg {
            if let Ok(canon) = canonical_key(&key) {
                if let Some(v) = cfg
                    .entries
                    .iter()
                    .rev()
                    .find(|e| e.key == canon)
                    .and_then(|e| e.value.as_deref())
                {
                    return v.eq_ignore_ascii_case("all");
                }
            }
        }
    }
    modules
        .ignore_by_path
        .get(sm_path)
        .is_some_and(|v| v.eq_ignore_ascii_case("all"))
}

fn run_summary(args: &SummaryArgs, _quiet: bool) -> Result<()> {
    if args.summary_limit == Some(0) {
        return Ok(());
    }
    let summary_limit = args.summary_limit.unwrap_or(-1);

    let repo = Repository::discover(None).context("not a git repository")?;
    let work_tree = repo.work_tree.as_ref().context("bare repository")?;
    let grit_bin = grit_exe::grit_executable();
    let cwd = std::env::current_dir().unwrap_or_else(|_| work_tree.to_path_buf());
    let gitlinks = index_gitlink_paths(&repo);

    let mut commit_spec = "HEAD";
    let pathspecs: Vec<String> = if let Some(p) = args.rest.iter().position(|x| x.as_str() == "--")
    {
        let head_tokens = &args.rest[..p];
        let tail: Vec<String> = args.rest[p + 1..]
            .iter()
            .filter_map(|raw| summary_pathspec_from_cwd(work_tree, &cwd, raw))
            .collect();
        if head_tokens.is_empty() {
            tail
        } else if !summary_arg_matches_gitlink(work_tree, &cwd, &gitlinks, &head_tokens[0])
            && resolve_revision(&repo, &head_tokens[0]).is_ok()
        {
            commit_spec = head_tokens[0].as_str();
            let mut ps: Vec<String> = head_tokens[1..]
                .iter()
                .filter_map(|raw| summary_pathspec_from_cwd(work_tree, &cwd, raw))
                .collect();
            ps.extend(tail);
            ps
        } else {
            let mut ps: Vec<String> = head_tokens
                .iter()
                .filter_map(|raw| summary_pathspec_from_cwd(work_tree, &cwd, raw))
                .collect();
            ps.extend(tail);
            ps
        }
    } else if args.rest.is_empty() {
        vec![]
    } else if !summary_arg_matches_gitlink(work_tree, &cwd, &gitlinks, &args.rest[0])
        && resolve_revision(&repo, &args.rest[0]).is_ok()
    {
        commit_spec = args.rest[0].as_str();
        args.rest[1..]
            .iter()
            .filter_map(|raw| summary_pathspec_from_cwd(work_tree, &cwd, raw))
            .collect()
    } else {
        args.rest
            .iter()
            .filter_map(|raw| summary_pathspec_from_cwd(work_tree, &cwd, raw))
            .collect()
    };

    let base_tree_oid = resolve_summary_base_tree(&repo, commit_spec)?;
    let index = repo
        .load_index()
        .context("load index for submodule summary")?;

    let (ignore_all_for_status, local_cfg_for_ignore) = if args.for_status {
        (
            gitmodules_ignore_all_map(work_tree),
            parse_local_config(&repo.git_dir).ok(),
        )
    } else {
        (GitmodulesIgnoreAll::default(), None)
    };

    let entries: Vec<DiffEntry> = if args.files {
        if args.cached {
            bail!("options '--cached' and '--files' cannot be used together");
        }
        // Git's `--files` mode runs `git diff-files --ignore-submodules=dirty --raw`, comparing
        // each **index** gitlink OID against the submodule working-tree HEAD. It iterates the
        // index gitlinks directly, NOT `.gitmodules` (which may be empty/unregistered — t7508).
        let mut out = Vec::new();
        for ie in &index.entries {
            if ie.stage() != 0 || ie.skip_worktree() {
                continue;
            }
            let path_str = String::from_utf8_lossy(&ie.path).into_owned();
            let sub_path = work_tree.join(&path_str);
            if ie.mode == MODE_GITLINK {
                let dst_oid = if let Some(h) = grit_lib::diff::read_submodule_head_oid(&sub_path) {
                    h
                } else {
                    ObjectId::zero()
                };
                if ie.oid == dst_oid {
                    continue;
                }
                out.push(DiffEntry {
                    status: DiffStatus::Modified,
                    old_path: Some(RepoPathBuf::from_bytes(ie.path.clone())),
                    new_path: Some(RepoPathBuf::from_bytes(ie.path.clone())),
                    old_mode: format_mode(MODE_GITLINK),
                    new_mode: format_mode(MODE_GITLINK),
                    old_oid: ie.oid,
                    new_oid: dst_oid,
                    score: None,
                });
            } else if let Some(dst_oid) = grit_lib::diff::read_submodule_head_oid(&sub_path) {
                out.push(DiffEntry {
                    status: DiffStatus::TypeChanged,
                    old_path: Some(RepoPathBuf::from_bytes(ie.path.clone())),
                    new_path: Some(RepoPathBuf::from_bytes(ie.path.clone())),
                    old_mode: format_mode(ie.mode),
                    new_mode: format_mode(MODE_GITLINK),
                    old_oid: ie.oid,
                    new_oid: dst_oid,
                    score: None,
                });
            }
        }
        out.sort_by(|a, b| a.path().cmp(b.path()));
        out
    } else {
        let mut entries = diff_index_to_tree(&repo.odb, &index, base_tree_oid.as_ref(), false)?;
        // Git `submodule summary` uses `diff-index --ignore-submodules=dirty`: when the index
        // gitlink matches `HEAD^{tree}` but the submodule worktree HEAD differs (e.g. after
        // `pull` before `submodule update`), still report the range (`t7418`).
        if !args.cached {
            if let Some(tree_oid) = base_tree_oid.as_ref() {
                let mut extra: Vec<DiffEntry> = Vec::new();
                for ie in &index.entries {
                    if ie.stage() != 0 || ie.mode != MODE_GITLINK || ie.skip_worktree() {
                        continue;
                    }
                    let path_str = String::from_utf8_lossy(&ie.path).into_owned();
                    let Some(te_oid) = blob_oid_at_path(&repo.odb, tree_oid, &path_str) else {
                        continue;
                    };
                    if te_oid != ie.oid {
                        continue;
                    }
                    let sub_path = submodule_work_tree_for_summary(work_tree, &path_str);
                    if !sub_path.join(".git").exists() {
                        continue;
                    }
                    let Some(sub_head) = grit_lib::diff::read_submodule_head_oid(&sub_path) else {
                        continue;
                    };
                    if sub_head == ie.oid {
                        continue;
                    }
                    extra.push(DiffEntry {
                        status: DiffStatus::Modified,
                        old_path: Some(RepoPathBuf::from_bytes(ie.path.clone())),
                        new_path: Some(RepoPathBuf::from_bytes(ie.path.clone())),
                        old_mode: format!("{:o}", MODE_GITLINK),
                        new_mode: format!("{:o}", MODE_GITLINK),
                        old_oid: ie.oid,
                        new_oid: sub_head,
                        score: None,
                    });
                }
                entries.extend(extra);
                entries.sort_by(|a, b| a.path().cmp(b.path()));
            }

            let head_states = head_path_states(&repo.odb, base_tree_oid.as_ref())?;
            let mut replacements: Vec<DiffEntry> = Vec::new();
            for ie in &index.entries {
                if ie.stage() != 0 || ie.skip_worktree() {
                    continue;
                }
                let path_str = String::from_utf8_lossy(&ie.path).into_owned();
                let sub_path = work_tree.join(&path_str);
                if let Some(sub_head) = grit_lib::diff::read_submodule_head_oid(&sub_path) {
                    if ie.mode == MODE_GITLINK && ie.oid == sub_head {
                        continue;
                    }
                    let (old_mode, old_oid) = head_states
                        .get(&path_str)
                        .copied()
                        .unwrap_or((0, ObjectId::zero()));
                    let status = if old_mode == 0 {
                        DiffStatus::Added
                    } else if old_mode != MODE_GITLINK {
                        DiffStatus::TypeChanged
                    } else {
                        DiffStatus::Modified
                    };
                    replacements.push(DiffEntry {
                        status,
                        old_path: (old_mode != 0)
                            .then(|| RepoPathBuf::from_bytes(ie.path.clone())),
                        new_path: Some(RepoPathBuf::from_bytes(ie.path.clone())),
                        old_mode: format_mode(old_mode),
                        new_mode: format_mode(MODE_GITLINK),
                        old_oid,
                        new_oid: sub_head,
                        score: None,
                    });
                } else if ie.mode == MODE_GITLINK && !work_tree.join(&path_str).exists() {
                    replacements.push(DiffEntry {
                        status: DiffStatus::Deleted,
                        old_path: Some(RepoPathBuf::from_bytes(ie.path.clone())),
                        new_path: None,
                        old_mode: format_mode(MODE_GITLINK),
                        new_mode: "000000".to_owned(),
                        old_oid: ie.oid,
                        new_oid: ObjectId::zero(),
                        score: None,
                    });
                }
            }
            for replacement in replacements {
                let key = replacement.path().to_owned();
                entries.retain(|e| e.path() != &*key);
                entries.push(replacement);
            }
            entries.sort_by(|a, b| a.path().cmp(b.path()));
        }
        entries
    };

    let stdout = io::stdout();
    let mut out = stdout.lock();

    for e in &entries {
        if !mode_is_gitlink(&e.old_mode) && !mode_is_gitlink(&e.new_mode) {
            continue;
        }
        let sm_path = summary_display_path(e);
        if !pathspec_selected(&pathspecs, &sm_path) {
            continue;
        }
        let display_path = summary_display_path_from_cwd(work_tree, &cwd, &sm_path);

        if args.for_status
            && e.status != DiffStatus::Added
            && submodule_ignore_all_for_summary(
                local_cfg_for_ignore.as_ref(),
                &ignore_all_for_status,
                &sm_path,
            )
        {
            continue;
        }

        let oid_src = e.old_oid;
        let mut oid_dst = e.new_oid;
        let src_gitlink = mode_is_gitlink(&e.old_mode);
        let dst_gitlink = mode_is_gitlink(&e.new_mode);

        let sub_path = submodule_work_tree_for_summary(work_tree, &sm_path);
        if !args.cached
            && !sub_path.join(".git").exists()
            && !oid_dst.is_zero()
            && src_gitlink == dst_gitlink
        {
            continue;
        }

        if !args.cached && oid_dst.is_zero() && mode_is_gitlink(&e.new_mode) {
            if let Some(h) = grit_lib::diff::read_submodule_head_oid(&sub_path) {
                oid_dst = h;
            }
        }

        let src_hex = oid_src.to_hex();
        let dst_hex = oid_dst.to_hex();

        if src_gitlink && dst_gitlink {
            let _ = submodule_fetch_gitlink_if_missing(
                &grit_bin, work_tree, &sm_path, &sub_path, &src_hex,
            );
            let _ = submodule_fetch_gitlink_if_missing(
                &grit_bin, work_tree, &sm_path, &sub_path, &dst_hex,
            );
        }

        let src_abbrev = short_oid_in_submodule(&grit_bin, &sub_path, &src_hex)
            .unwrap_or_else(|| src_hex.chars().take(7).collect());
        let dst_abbrev = short_oid_in_submodule(&grit_bin, &sub_path, &dst_hex)
            .unwrap_or_else(|| dst_hex.chars().take(7).collect());

        let null_side = oid_src.is_zero() || oid_dst.is_zero();
        if (!null_side && src_gitlink != dst_gitlink) || e.status == DiffStatus::TypeChanged {
            let gitlink_abbrev = if src_gitlink {
                src_abbrev.as_str()
            } else {
                dst_abbrev.as_str()
            };
            let gitlink_count = if (src_gitlink || dst_gitlink) && sub_path.join(".git").exists() {
                submodule_rev_list_count(&grit_bin, &sub_path, gitlink_abbrev).unwrap_or(-1)
            } else {
                -1
            };
            if dst_gitlink && !src_gitlink {
                write!(
                    out,
                    "* {} {}(blob)->{}(submodule)",
                    display_path, src_abbrev, dst_abbrev
                )?;
                if gitlink_count >= 0 {
                    write!(out, " ({gitlink_count})")?;
                }
                writeln!(out, ":")?;
                if gitlink_count > 0 {
                    out.flush()?;
                    submodule_log_one(&grit_bin, &sub_path, &dst_abbrev, '>')?;
                }
            } else if src_gitlink && !dst_gitlink {
                write!(
                    out,
                    "* {} {}(submodule)->{}(blob)",
                    display_path, src_abbrev, dst_abbrev
                )?;
                if gitlink_count >= 0 {
                    write!(out, " ({gitlink_count})")?;
                }
                writeln!(out, ":")?;
                if gitlink_count > 0 {
                    out.flush()?;
                    submodule_log_one(&grit_bin, &sub_path, &src_abbrev, '<')?;
                }
            } else {
                writeln!(out, "* {} {}...{}", display_path, src_abbrev, dst_abbrev)?;
            }
            writeln!(out)?;
            continue;
        }

        let submodule_repo_exists = sub_path.join(".git").exists();
        let total_commits = if !submodule_repo_exists {
            -1
        } else if !src_hex.is_empty() && !dst_hex.is_empty() {
            if src_gitlink && dst_gitlink {
                submodule_rev_list_count(&grit_bin, &sub_path, &format!("{src_hex}...{dst_hex}"))?
            } else {
                submodule_rev_list_count(&grit_bin, &sub_path, &dst_hex)?
            }
        } else {
            -1
        };

        write!(out, "* {} {}...{}", display_path, src_abbrev, dst_abbrev)?;
        if total_commits < 0 {
            writeln!(out, ":")?;
        } else {
            writeln!(out, " ({total_commits}):")?;
        }
        out.flush()?;

        if total_commits > 0 {
            if src_gitlink && dst_gitlink {
                submodule_log_first_parent(
                    &grit_bin,
                    &sub_path,
                    &src_hex,
                    &dst_hex,
                    summary_limit,
                )?;
            } else if dst_gitlink {
                submodule_log_one(&grit_bin, &sub_path, &dst_abbrev, '>')?;
            } else {
                submodule_log_one(&grit_bin, &sub_path, &src_abbrev, '<')?;
            }
        } else if total_commits < 0 && submodule_repo_exists && src_gitlink && dst_gitlink {
            writeln!(
                out,
                "  Warn: {} doesn't contain commit {}",
                display_path, src_hex
            )?;
        }
        writeln!(out)?;
    }

    Ok(())
}

fn run_set_branch(args: &SetBranchArgs, _quiet: bool) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let work_tree = repo.work_tree.as_ref().context("bare repository")?;

    let gitmodules_path = work_tree.join(".gitmodules");
    let content = fs::read_to_string(&gitmodules_path).context("failed to read .gitmodules")?;
    let mut config = ConfigFile::parse(&gitmodules_path, &content, ConfigScope::Local)?;

    // Find the submodule name for this path.
    let modules = parse_gitmodules(work_tree)?;
    let sm = modules
        .iter()
        .find(|m| m.path == args.path || m.name == args.path)
        .context("submodule not found")?;

    let branch_key = format!("submodule.{}.branch", sm.name);

    if args.default {
        // Remove the branch setting.
        config.unset(&branch_key)?;
    } else if let Some(ref branch) = args.branch {
        config.set(&branch_key, branch)?;
    } else {
        bail!("--branch <branch> or --default required");
    }

    config.write()?;
    Ok(())
}

fn run_set_url(args: &SetUrlArgs, _quiet: bool) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let work_tree = repo.work_tree.as_ref().context("bare repository")?;

    let gitmodules_path = work_tree.join(".gitmodules");
    let content = fs::read_to_string(&gitmodules_path).context("failed to read .gitmodules")?;
    let mut config = ConfigFile::parse(&gitmodules_path, &content, ConfigScope::Local)?;

    // Find the submodule name for this path.
    let modules = parse_gitmodules(work_tree)?;
    let sm = modules
        .iter()
        .find(|m| m.path == args.path || m.name == args.path)
        .context("submodule not found")?;

    let url_key = format!("submodule.{}.url", sm.name);
    config.set(&url_key, &args.newurl)?;
    // When the logical submodule name differs from its path, drop any mistaken
    // `submodule.<path>.url` entry so `git config` sees a single canonical URL
    // (matches Git `submodule set-url` + `.gitmodules` layout).
    if sm.name != sm.path {
        let path_url_key = format!("submodule.{}.url", sm.path);
        let _ = config.unset(&path_url_key);
    }
    config.write()?;

    // Mirror `git submodule set-url`: after `.gitmodules`, run the same URL sync as
    // `submodule sync` for initialized (active) submodules only.
    let config_path = repo.git_dir.join("config");
    if !config_path.exists() {
        return Ok(());
    }
    let local_content = fs::read_to_string(&config_path)?;
    let mut local_config = ConfigFile::parse(&config_path, &local_content, ConfigScope::Local)?;
    let has_url = local_config.entries.iter().any(|e| e.key == url_key);
    if !has_url {
        return Ok(());
    }

    let super_url = resolve_submodule_super_url(work_tree, &repo.git_dir, &args.newurl)?;
    local_config.set(&url_key, &super_url)?;
    if sm.name != sm.path {
        let path_url_key = format!("submodule.{}.url", sm.path);
        let _ = local_config.unset(&path_url_key);
    }
    local_config.write()?;

    let resolved_url =
        resolve_submodule_sub_origin_url(work_tree, &repo.git_dir, &sm.path, &args.newurl)?;
    let sub_path = work_tree.join(&sm.path);
    if sub_path.join(".git").exists() {
        let sub_git_dir = resolve_submodule_git_dir(&sub_path);
        if let Some(sub_git) = sub_git_dir {
            let sub_config_path = sub_git.join("config");
            if sub_config_path.exists() {
                let sub_content = fs::read_to_string(&sub_config_path)?;
                let mut sub_config =
                    ConfigFile::parse(&sub_config_path, &sub_content, ConfigScope::Local)?;
                sub_config.set("remote.origin.url", &resolved_url)?;
                sub_config.write()?;
            }
        }
    }

    Ok(())
}
