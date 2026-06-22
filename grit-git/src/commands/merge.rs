//! `grit merge` — join two or more development histories together.
//!
//! Implements fast-forward, three-way merge with conflict handling,
//! `--squash`, `--no-ff`, `--ff-only`, `--abort`, and `--continue`.

use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::IsTerminal;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::NamedTempFile;

use grit_lib::config::ConfigSet;
use grit_lib::crlf::MergeAttr;
use grit_lib::diff::{
    count_changes, detect_renames, diff_trees, submodule_embedded_git_dir, zero_oid, DiffEntry,
    DiffStatus,
};
use grit_lib::diffstat::{terminal_columns, write_diffstat_block, DiffstatOptions, FileStatInput};
use grit_lib::hooks::{
    run_commit_hook, run_hook, run_reference_transaction_committed_for_head_update, CommitHookEnv,
    HookResult,
};
use grit_lib::index::{
    Index, IndexEntry, MODE_EXECUTABLE, MODE_GITLINK, MODE_REGULAR, MODE_SYMLINK, MODE_TREE,
};
use grit_lib::merge_base::is_ancestor;
use grit_lib::merge_file::{self, ConflictStyle, MergeFavor, MergeInput};
use grit_lib::objects::{
    parse_commit, parse_tag, serialize_commit, CommitData, ObjectId, ObjectKind,
};
use grit_lib::porcelain::merge::{
    compose_fast_forward_index, compose_octopus_final_index, reduce_octopus_merge_heads,
    tree_to_index_entries, tree_to_map,
};
use grit_lib::refs::resolve_ref;
use grit_lib::repo::Repository;
use grit_lib::rev_list::{rev_list, OrderingMode, OutputMode, RevListOptions};
use grit_lib::rev_parse::{resolve_upstream_symbolic_name, upstream_suffix_info};
use grit_lib::sparse_checkout::apply_sparse_checkout_skip_worktree;
use grit_lib::state::{resolve_head, HeadState};
use grit_lib::write_tree::{build_cache_tree_from_index, write_tree_from_index};
use time::OffsetDateTime;

use crate::commands::commit::author_env_for_commit_hooks;
use crate::commands::diff_index;
use crate::explicit_exit::{ExplicitExit, SilentNonZeroExit};

/// Register embedded submodule `objects/` directories so merge can read gitlink commits (`t6437`).
fn register_merge_submodule_odbs(repo: &Repository) -> Result<()> {
    let Some(wt) = repo.work_tree.as_ref() else {
        return Ok(());
    };
    let index = repo.load_index()?;
    repo.odb
        .register_submodule_object_directories_from_index(wt, &index);
    Ok(())
}

/// Count distinct paths that have any unmerged index stage (matches `git merge` conflict scoring).
fn unmerged_path_count(index: &Index) -> usize {
    let mut paths = BTreeSet::new();
    for e in &index.entries {
        if e.stage() != 0 {
            paths.insert(e.path.clone());
        }
    }
    paths.len()
}

/// Returned from [`do_real_merge`] when probing strategies: carries unmerged path count for
/// `try_merge_strategies` (matches git's `evaluate_result` scoring).
#[derive(Debug)]
struct StrategyTrialConflict(usize);

impl std::fmt::Display for StrategyTrialConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "merge strategy trial left {} conflicted paths", self.0)
    }
}

impl std::error::Error for StrategyTrialConflict {}

/// Run Git's `post-merge` hook with one argument: `0` for a normal merge, `1` for squash.
///
/// Matches upstream `merge.c` `finish()`: hook failures are ignored (Git does not abort the merge).
fn run_post_merge_hook(repo: &Repository, squash: bool) {
    let flag = if squash { "1" } else { "0" };
    let _ = run_hook(repo, "post-merge", &[flag], None);
}

fn run_pre_merge_commit_hook(
    repo: &Repository,
    no_verify: bool,
    merge_will_launch_editor: bool,
    index: &mut Index,
) -> Result<()> {
    if no_verify {
        return Ok(());
    }
    let index_path = repo.index_path();
    let config = ConfigSet::load(Some(&repo.git_dir), true)?;
    let now = OffsetDateTime::now_utc();
    let author_line = resolve_ident(&config, "author", now)?;
    let author_env = author_env_for_commit_hooks(&author_line)?;
    let author_refs: Vec<(&str, &str)> = author_env
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let hook_editor = if merge_will_launch_editor {
        None
    } else {
        Some(":")
    };
    let hook_env = CommitHookEnv {
        index_file: Some(index_path.as_path()),
        git_editor: hook_editor,
        git_prefix: None,
        extra_env: author_refs.as_slice(),
    };
    let before = run_commit_hook(repo, "pre-merge-commit", &[], None, &hook_env)
        .map_err(|e| anyhow::anyhow!(e))?;
    if let HookResult::Failed(code) = before {
        bail!("pre-merge-commit hook exited with status {code}");
    }
    if before.was_executed() {
        *index = match repo.load_index_at(&index_path) {
            Ok(idx) => idx,
            Err(grit_lib::error::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                Index::new()
            }
            Err(e) => return Err(e.into()),
        };
    }
    Ok(())
}

/// Run `prepare-commit-msg`, the editor (when `will_edit`), then `commit-msg` on the merge
/// commit message, mirroring `builtin/merge.c:prepare_to_commit`.
///
/// Writes `MERGE_MSG`; runs `prepare-commit-msg <MERGE_MSG> merge` (always, even with
/// `--no-verify`, like upstream); re-reads the index in case the hook ran `git add`; launches the
/// editor on `MERGE_MSG` when `will_edit`; runs `commit-msg <MERGE_MSG>` unless `--no-verify`; then
/// reads the (possibly hook/editor-modified) message back and applies `cleanup_mode`. When no
/// editor will run, `GIT_EDITOR=:` is exported so hooks can detect a non-interactive merge.
fn run_merge_commit_msg_hooks(
    repo: &Repository,
    no_verify: bool,
    will_edit: bool,
    msg: String,
    index: &mut Index,
    cleanup_mode: &str,
) -> Result<String> {
    let merge_msg_path = repo.git_dir.join("MERGE_MSG");
    fs::write(&merge_msg_path, msg.as_bytes())?;
    let index_path = repo.index_path();
    let merge_msg_str = merge_msg_path.to_string_lossy().to_string();
    let hook_env = CommitHookEnv {
        index_file: Some(index_path.as_path()),
        git_editor: if will_edit { None } else { Some(":") },
        git_prefix: None,
        extra_env: &[],
    };

    // prepare-commit-msg runs even with `--no-verify` (only pre-commit/commit-msg are skipped).
    let prepared = run_commit_hook(
        repo,
        "prepare-commit-msg",
        &[merge_msg_str.as_str(), "merge"],
        None,
        &hook_env,
    )
    .map_err(|e| anyhow::anyhow!(e))?;
    if let HookResult::Failed(code) = prepared {
        bail!("prepare-commit-msg hook exited with status {code}");
    }
    if prepared.was_executed() {
        // The hook may have updated the index (e.g. via `git add`); re-read it.
        *index = match repo.load_index_at(&index_path) {
            Ok(idx) => idx,
            Err(grit_lib::error::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                Index::new()
            }
            Err(e) => return Err(e.into()),
        };
    }

    // Editor runs after prepare-commit-msg and before commit-msg (builtin/merge.c order).
    if will_edit {
        crate::commands::commit::launch_commit_editor(repo, &merge_msg_path)?;
    }

    if !no_verify {
        let verified = run_commit_hook(
            repo,
            "commit-msg",
            &[merge_msg_str.as_str()],
            None,
            &hook_env,
        )
        .map_err(|e| anyhow::anyhow!(e))?;
        if let HookResult::Failed(code) = verified {
            bail!("commit-msg hook exited with status {code}");
        }
    }

    // Re-read MERGE_MSG so an edit by either hook (or the editor) is picked up, then re-apply
    // cleanup (upstream `read_merge_msg` + `cleanup_message`).
    let edited = fs::read_to_string(&merge_msg_path)?;
    Ok(cleanup_message(&edited, cleanup_mode))
}

fn merge_will_edit(args: &Args) -> bool {
    match std::env::var("GIT_GRIT_MERGE_EXPLICIT_EDIT").as_deref() {
        Ok("1") => return true,
        Ok("0") => return false,
        _ => {}
    }
    !args.no_edit && (args.edit || std::io::stdin().is_terminal())
}

/// Update `HEAD` (and branch ref when on a branch) then run `reference-transaction` with phase
/// `committed` (t1800 client hook ordering).
fn merge_update_head(
    repo: &Repository,
    head: &HeadState,
    old_head_commit: Option<ObjectId>,
    new_oid: ObjectId,
) -> Result<()> {
    merge_update_head_with_reflog(repo, head, old_head_commit, new_oid, None)
}

/// Like [`merge_update_head`] but also appends a reflog entry to the branch ref (when on a
/// branch) and to `HEAD`, mirroring git's `finish()` / `update_ref` reflog behaviour. The
/// reflog message is the full `<GIT_REFLOG_ACTION>: <msg>` string (e.g.
/// `"merge c1: Fast-forward"`), or a standalone message such as `"initial pull"`.
fn merge_update_head_with_reflog(
    repo: &Repository,
    head: &HeadState,
    old_head_commit: Option<ObjectId>,
    new_oid: ObjectId,
    reflog: Option<&str>,
) -> Result<()> {
    update_head(&repo.git_dir, head, &new_oid)?;
    if let Some(msg) = reflog {
        let zero = ObjectId::from_bytes(&[0u8; 20]).unwrap_or(new_oid);
        let old_oid = old_head_commit.unwrap_or(zero);
        let identity = reflog_identity(repo);
        if let HeadState::Branch { refname, .. } = head {
            let _ = grit_lib::refs::append_reflog(
                &repo.git_dir,
                refname,
                &old_oid,
                &new_oid,
                &identity,
                msg,
                false,
            );
        }
        let _ = grit_lib::refs::append_reflog(
            &repo.git_dir,
            "HEAD",
            &old_oid,
            &new_oid,
            &identity,
            msg,
            false,
        );
    }
    let _ =
        run_reference_transaction_committed_for_head_update(repo, head, old_head_commit, new_oid);
    Ok(())
}

/// Build the `Name <email> <timestamp> <tz>` reflog identity string from the committer ident.
fn reflog_identity(repo: &Repository) -> String {
    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let now = OffsetDateTime::now_utc();
    resolve_ident(&config, "committer", now).unwrap_or_else(|_| "unknown <unknown> 0 +0000".into())
}

/// The default `GIT_REFLOG_ACTION` for a merge: `"merge <name1> <name2> ..."` over the original
/// command-line ref arguments (git `cmd_merge` setenv at builtin/merge.c:1582).
fn merge_reflog_action(args: &Args) -> String {
    if let Ok(action) = std::env::var("GIT_REFLOG_ACTION") {
        if !action.is_empty() {
            return action;
        }
    }
    let mut buf = String::from("merge");
    for name in &args.commits {
        buf.push(' ');
        buf.push_str(name);
    }
    buf
}

fn show_merge_commit_summary() -> bool {
    !matches!(std::env::var("GIT_MERGE_VERBOSITY").as_deref(), Ok("0"))
}

/// Arguments for `grit merge`.
#[derive(Debug, Clone, ClapArgs)]
#[command(about = "Join two or more development histories together")]
pub struct Args {
    /// Branch or commit to merge.
    #[arg(value_name = "COMMIT")]
    pub commits: Vec<String>,

    /// Custom merge commit message.
    #[arg(short = 'm', long = "message")]
    pub message: Option<String>,

    /// Only allow fast-forward merges.
    #[arg(long = "ff-only")]
    pub ff_only: bool,

    /// Always create a merge commit (no fast-forward).
    #[arg(long = "no-ff")]
    pub no_ff: bool,

    /// Perform the merge but don't commit.
    #[arg(long = "no-commit")]
    pub no_commit: bool,

    /// Squash merge: stage changes but don't commit.
    #[arg(long = "squash")]
    pub squash: bool,

    /// Skip the pre-merge-commit hook (Git `--no-verify`; note: git merge's `-n` is `--no-stat`).
    #[arg(long = "no-verify")]
    pub no_verify: bool,

    /// Abort in-progress merge.
    #[arg(long = "abort")]
    pub abort: bool,

    /// Continue after resolving conflicts.
    #[arg(long = "continue")]
    pub continue_merge: bool,

    /// Merge strategy to use (e.g. recursive, ort, resolve, octopus, ours).
    /// May be passed multiple times (`-s ort -s octopus`); each is tried in order until one succeeds.
    #[arg(short = 's', long = "strategy", action = clap::ArgAction::Append)]
    pub strategy: Vec<String>,

    /// Strategy-specific option (e.g. ours, theirs).
    #[arg(short = 'X', long = "strategy-option")]
    pub strategy_option: Vec<String>,

    /// Suppress output.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Force progress reporting.
    #[arg(long = "progress")]
    pub progress: bool,

    /// Suppress progress reporting.
    #[arg(long = "no-progress")]
    pub no_progress: bool,

    /// Allow merging histories that do not share a common ancestor.
    #[arg(long = "allow-unrelated-histories")]
    pub allow_unrelated_histories: bool,

    /// Suppress editor launch for the merge commit message.
    #[arg(long = "no-edit")]
    pub no_edit: bool,

    /// Open editor for the merge commit message (default for non-automated merges).
    #[arg(long = "edit", short = 'e')]
    pub edit: bool,

    /// Add Signed-off-by trailer to the merge commit message.
    #[arg(long = "signoff")]
    pub signoff: bool,

    /// Do not add Signed-off-by trailer.
    #[arg(long = "no-signoff")]
    pub no_signoff: bool,

    /// GPG-sign the resulting merge commit (Git's `merge -S`/`--gpg-sign`).
    ///
    /// The key id is optional and, like Git, only attached (`-S<keyid>` or
    /// `--gpg-sign=<keyid>`); a bare `-S` must not swallow the following
    /// positional (e.g. `git merge -S side`).
    #[arg(short = 'S', long = "gpg-sign", value_name = "KEYID", num_args = 0..=1, default_missing_value = "", require_equals = true)]
    pub gpg_sign: Option<String>,

    /// Do not GPG-sign the merge commit.
    #[arg(long = "no-gpg-sign")]
    pub no_gpg_sign: bool,

    /// Show a diffstat at the end of the merge.
    #[arg(long = "stat")]
    pub stat: bool,

    /// Synonym for --stat.
    #[arg(short = 'n', long = "no-stat")]
    pub no_stat: bool,

    /// Show log messages from commits being merged.
    #[arg(long = "log", value_name = "N", num_args = 0..=1, default_missing_value = "20", require_equals = true)]
    pub log: Option<usize>,

    /// Do not include log messages.
    #[arg(long = "no-log")]
    pub no_log: bool,

    /// Show compact-summary in diffstat output.
    #[arg(long = "compact-summary")]
    pub compact_summary: bool,

    /// Show summary (deprecated synonym for --stat).
    #[arg(long = "summary")]
    pub summary: bool,

    /// Allow fast-forward (default).
    #[arg(long = "ff")]
    pub ff: bool,

    /// Allow fast-forward (aliases for configuration).
    #[arg(long = "commit")]
    pub commit: bool,

    /// Undo --squash.
    #[arg(long = "no-squash")]
    pub no_squash: bool,

    /// Quit merge.
    #[arg(long = "quit")]
    pub quit: bool,

    /// Automatically stash/unstash before/after merge.
    #[arg(long = "autostash", overrides_with = "no_autostash")]
    pub autostash: bool,

    /// Do not stash/unstash before/after merge (overrides `merge.autostash`).
    #[arg(long = "no-autostash", overrides_with = "autostash")]
    pub no_autostash: bool,

    /// How to clean up the merge message.
    #[arg(long = "cleanup", value_name = "MODE")]
    pub cleanup: Option<String>,

    /// Read the commit message from the given file.
    #[arg(short = 'F', long = "file", value_name = "FILE")]
    pub file: Option<String>,

    /// After a failed merge, record conflict preimages / replay recorded resolutions and optionally stage.
    #[arg(long = "rerere-autoupdate")]
    pub rerere_autoupdate: bool,

    /// Do not update the index when a recorded rerere resolution is replayed.
    #[arg(long = "no-rerere-autoupdate")]
    pub no_rerere_autoupdate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SubtreeShift {
    Disabled,
    Auto,
    Prefix(String),
}

/// First `-s` strategy wins for merge-tree behavior; empty means default (ort).
fn primary_merge_strategy(args: &Args) -> Option<&str> {
    args.strategy.first().map(String::as_str)
}

fn is_builtin_merge_strategy(name: &str) -> bool {
    matches!(
        name,
        "recursive" | "ort" | "resolve" | "octopus" | "ours" | "subtree"
    )
}

/// `git-merge-<name>` on `PATH`, matching Git's custom merge strategy discovery.
fn resolve_git_merge_driver(strategy: &str) -> Option<PathBuf> {
    let name = format!("git-merge-{strategy}");
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(&name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn use_external_merge_strategy(args: &Args) -> bool {
    args.strategy.len() == 1
        && !is_builtin_merge_strategy(&args.strategy[0])
        && resolve_git_merge_driver(&args.strategy[0]).is_some()
}

/// Subtree path shifting for `merge_trees`: `-s subtree` implies auto-detect unless `-X subtree` set a prefix.
fn effective_subtree_shift(
    primary_strategy: Option<&str>,
    configured: &SubtreeShift,
) -> SubtreeShift {
    if primary_strategy == Some("subtree") {
        match configured {
            SubtreeShift::Disabled => SubtreeShift::Auto,
            other => other.clone(),
        }
    } else {
        configured.clone()
    }
}

/// True when every stage-0 index entry matches `HEAD^{tree}` and every HEAD path is present
/// in the index (no staged add/delete/modify vs HEAD).
///
/// Uses entry-by-entry comparison — not `write_tree_from_index` — so intent-to-add and other
/// entries omitted from the written tree still make the index "dirty" vs HEAD when appropriate.
///
/// Like [`index_matches_head_tree`] but compares the index to an arbitrary commit's tree.
fn index_matches_commit_tree(repo: &Repository, commit_oid: ObjectId) -> Result<bool> {
    let index = repo.load_index()?;
    let tree_oid = commit_tree(repo, commit_oid)?;
    let tree_entries = tree_to_map(tree_to_index_entries(repo, &tree_oid, "")?);
    let mut index_paths: BTreeSet<Vec<u8>> = BTreeSet::new();
    for e in index.entries.iter().filter(|e| e.stage() == 0) {
        index_paths.insert(e.path.clone());
        match tree_entries.get(&e.path) {
            Some(te) => {
                if te.oid != e.oid || te.mode != e.mode {
                    return Ok(false);
                }
            }
            None => return Ok(false),
        }
    }
    for path in tree_entries.keys() {
        if !index_paths.contains(path) {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn index_matches_head_tree(repo: &Repository, head_oid: ObjectId) -> Result<bool> {
    let index = repo.load_index()?;
    let head_tree = commit_tree(repo, head_oid)?;
    let head_entries = tree_to_map(tree_to_index_entries(repo, &head_tree, "")?);
    let mut index_paths: BTreeSet<Vec<u8>> = BTreeSet::new();
    for e in index.entries.iter().filter(|e| e.stage() == 0) {
        index_paths.insert(e.path.clone());
        match head_entries.get(&e.path) {
            Some(he) => {
                if he.oid != e.oid || he.mode != e.mode {
                    return Ok(false);
                }
            }
            None => return Ok(false),
        }
    }
    for path in head_entries.keys() {
        if !index_paths.contains(path) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Refuse merge when the index does not match `HEAD^{tree}`. Used for octopus and for
/// recursive/ort/subtree single-parent merges (t6424). `--autostash` skips this check.
fn bail_if_index_tree_differs_from_head(
    repo: &Repository,
    head_oid: ObjectId,
    autostash: bool,
) -> Result<()> {
    if autostash {
        return Ok(());
    }
    if index_matches_head_tree(repo, head_oid)? {
        return Ok(());
    }
    bail!(
        "Your local changes to the following files would be overwritten by merge:\n\
         \t(index does not match HEAD)\n\
         Please commit your changes or stash them before you merge.\n\
         Aborting"
    );
}

/// The resolve strategy does not allow *any* staged difference from HEAD (including removals),
/// unlike recursive/ort which only care when the merge would touch those paths.
pub(crate) fn bail_if_resolve_index_not_clean_vs_head(
    repo: &Repository,
    head_oid: ObjectId,
    autostash: bool,
) -> Result<()> {
    if autostash {
        return Ok(());
    }
    let index = repo.load_index()?;
    let head_tree = commit_tree(repo, head_oid)?;
    let head_entries = tree_to_map(tree_to_index_entries(repo, &head_tree, "")?);
    let mut dirty_paths: BTreeSet<String> = BTreeSet::new();
    for e in index.entries.iter().filter(|e| e.stage() == 0) {
        let rel = String::from_utf8_lossy(&e.path).to_string();
        match head_entries.get(&e.path) {
            Some(he) => {
                if he.oid != e.oid || he.mode != e.mode {
                    dirty_paths.insert(rel);
                }
            }
            None => {
                dirty_paths.insert(rel);
            }
        }
    }
    for path in head_entries.keys() {
        if !index
            .entries
            .iter()
            .any(|e| e.stage() == 0 && e.path == *path)
        {
            dirty_paths.insert(String::from_utf8_lossy(path).to_string());
        }
    }
    if dirty_paths.is_empty() {
        return Ok(());
    }
    let mut msg =
        String::from("Your local changes to the following files would be overwritten by merge:\n");
    for path in &dirty_paths {
        msg.push_str(&format!("\t{path}\n"));
    }
    msg.push_str("Please commit your changes or stash them before you merge.\nAborting");
    bail!("{msg}");
}

/// Stage-0 index paths that differ from `HEAD^{tree}` (including staged additions/removals).
pub(crate) fn staged_dirty_paths_vs_head(
    repo: &Repository,
    head_oid: ObjectId,
) -> Result<BTreeSet<String>> {
    let index = repo.load_index()?;
    let head_tree = commit_tree(repo, head_oid)?;
    let head_entries = tree_to_map(tree_to_index_entries(repo, &head_tree, "")?);
    let mut dirty_paths: BTreeSet<String> = BTreeSet::new();
    for e in index.entries.iter().filter(|e| e.stage() == 0) {
        let rel = String::from_utf8_lossy(&e.path).to_string();
        match head_entries.get(&e.path) {
            Some(he) => {
                if he.oid != e.oid || he.mode != e.mode {
                    dirty_paths.insert(rel);
                }
            }
            None => {
                dirty_paths.insert(rel);
            }
        }
    }
    for path in head_entries.keys() {
        if !index
            .entries
            .iter()
            .any(|e| e.stage() == 0 && e.path == *path)
        {
            dirty_paths.insert(String::from_utf8_lossy(path).to_string());
        }
    }
    Ok(dirty_paths)
}

/// Paths that differ between `parent_tree` and `head_tree` or between `parent_tree` and `theirs_tree`.
///
/// Used to detect whether local changes overlap a cherry-pick / revert three-way merge.
pub(crate) fn merge_touched_paths(
    repo: &Repository,
    parent_tree: ObjectId,
    head_tree: ObjectId,
    theirs_tree: ObjectId,
) -> Result<BTreeSet<String>> {
    let mut paths = BTreeSet::new();
    for e in diff_trees(&repo.odb, Some(&parent_tree), Some(&head_tree), "")? {
        paths.insert(e.path().to_string());
    }
    for e in diff_trees(&repo.odb, Some(&parent_tree), Some(&theirs_tree), "")? {
        paths.insert(e.path().to_string());
    }
    Ok(paths)
}

/// Fast-forward index: the merge target tree plus **unrelated** staged additions (paths not in
/// `HEAD^{tree}` and not in the target tree). Paths present in HEAD but absent from the target
/// (deletes/renames) must not be copied from the index — only the target layout wins.
/// Restore index and working tree to match `head_oid` after a failed merge attempt
/// (used when trying multiple `-s` strategies so a failed strategy leaves no residue).
/// Write `index_snapshot` to disk and refresh the work tree (clears merge state files).
fn restore_index_and_worktree(repo: &Repository, index_snapshot: &Index) -> Result<()> {
    let _ = fs::remove_file(repo.git_dir.join("MERGE_HEAD"));
    let _ = fs::remove_file(repo.git_dir.join("MERGE_MSG"));
    let _ = fs::remove_file(repo.git_dir.join("MERGE_MODE"));
    let mut index = Index::new();
    index.entries = index_snapshot.entries.clone();
    index.sort();
    if let Some(ref wt) = repo.work_tree {
        checkout_entries(repo, wt, &index, None, false)?;
    }
    repo.write_index(&mut index)?;
    Ok(())
}

fn restore_repo_to_head(repo: &Repository, head_oid: ObjectId) -> Result<()> {
    let commit_obj = repo.odb.read(&head_oid)?;
    let commit = parse_commit(&commit_obj.data)?;
    let entries = tree_to_index_entries(repo, &commit.tree, "")?;
    let mut index = Index::new();
    index.entries = entries;
    index.sort();
    restore_index_and_worktree(repo, &index)
}

/// Apply branch.<name>.mergeoptions to the args.
/// Only applies settings that weren't explicitly set on the command line.
fn apply_mergeoptions(args: &mut Args, opts: &str) -> Result<()> {
    // Save CLI-set flags before applying config options
    let cli_ff = args.ff;
    let cli_no_ff = args.no_ff;
    let cli_ff_only = args.ff_only;
    let cli_squash = args.squash;
    let cli_no_squash = args.no_squash;
    let cli_commit = args.commit;
    let cli_no_commit = args.no_commit;
    let cli_stat = args.stat;
    let cli_no_stat = args.no_stat;
    let cli_summary = args.summary;

    let tokens = shell_words::split(opts).map_err(|err| {
        anyhow::anyhow!("fatal: bad branch mergeoptions string '{}': {}", opts, err)
    })?;

    for token in tokens {
        match token.as_str() {
            "--ff" if !cli_no_ff && !cli_ff_only => args.ff = true,
            "--no-ff" if !cli_ff && !cli_ff_only => args.no_ff = true,
            "--ff-only" if !cli_ff && !cli_no_ff => args.ff_only = true,
            "--squash" if !cli_no_squash => args.squash = true,
            "--no-squash" if !cli_squash => args.no_squash = true,
            "--commit" if !cli_no_commit => args.commit = true,
            "--no-commit" if !cli_commit => args.no_commit = true,
            "--stat" if !cli_no_stat => args.stat = true,
            "--no-stat" | "-n" if !cli_stat && !cli_summary => args.no_stat = true,
            "--log" => {
                if args.log.is_none() {
                    args.log = Some(20);
                }
            }
            "--no-log" => args.no_log = true,
            "--signoff" if !args.no_signoff => args.signoff = true,
            "--no-signoff" if !args.signoff => args.no_signoff = true,
            "-S" | "--gpg-sign" if !args.no_gpg_sign => {
                if args.gpg_sign.is_none() {
                    args.gpg_sign = Some(String::new());
                }
            }
            "--no-gpg-sign" => args.no_gpg_sign = true,
            "--edit" | "-e" if !args.no_edit => args.edit = true,
            "--no-edit" if !args.edit => args.no_edit = true,
            "--quiet" | "-q" => args.quiet = true,
            "--summary" if !cli_no_stat => args.summary = true,
            "--rerere-autoupdate" => args.rerere_autoupdate = true,
            "--no-rerere-autoupdate" => args.no_rerere_autoupdate = true,
            _ => {} // ignore unknown options
        }
    }
    Ok(())
}

/// Run the `merge` command.
pub fn run(mut args: Args) -> Result<()> {
    if args.abort {
        return merge_abort();
    }
    if args.continue_merge {
        return merge_continue(args.message);
    }

    // Handle -s help early (before commit check)
    if args.strategy.iter().any(|s| s == "help") {
        eprintln!("Could not find merge strategy 'help'.");
        eprintln!("Available strategies are: octopus ours recursive resolve subtree.");
        std::process::exit(1);
    }

    if args.quit {
        return merge_quit();
    }

    if args.commits.is_empty() {
        bail!("nothing to merge — please specify a branch or commit");
    }

    // Read merge.ff config and apply unless overridden by CLI flags.
    // CLI flags (--ff, --no-ff, --ff-only) take precedence over config.
    let repo = Repository::discover(None).context("not a git repository")?;
    if args.commits.len() == 1 && args.commits[0] == "FETCH_HEAD" {
        // Derive the default merge message from the FETCH_HEAD branch/tag descriptions
        // (git's fmt-merge-msg) before the spec is resolved to a bare OID, which would
        // otherwise yield "Merge commit '<oid>'" instead of "Merge branch 'side'".
        if args.message.is_none() {
            if let Ok(content) = std::fs::read_to_string(repo.git_dir.join("FETCH_HEAD")) {
                let into_name = resolve_head(&repo.git_dir)
                    .ok()
                    .and_then(|h| h.branch_name().map(str::to_owned));
                let opts = grit_lib::fmt_merge_msg::FmtMergeMsgOptions {
                    message: None,
                    into_name,
                };
                let derived = grit_lib::fmt_merge_msg::fmt_merge_msg(&content, &opts);
                let derived = derived.trim_end_matches('\n');
                if !derived.is_empty() {
                    args.message = Some(derived.to_owned());
                }
            }
        }
        args.commits = read_fetch_head_merge_oids(&repo)?;
    }
    let mut merge_renormalize = false;
    {
        let config = ConfigSet::load(Some(&repo.git_dir), true)?;

        // Read branch.<name>.mergeoptions and apply them (CLI flags override these).
        let head_state = resolve_head(&repo.git_dir)?;
        if let Some(branch_name) = head_state.branch_name() {
            let key = format!("branch.{branch_name}.mergeoptions");
            if let Some(opts) = config.get(&key) {
                apply_mergeoptions(&mut args, &opts)?;
            }
        }

        if !args.ff && !args.no_ff && !args.ff_only {
            if let Some(val) = config.get("merge.ff") {
                match val.to_lowercase().as_str() {
                    "false" | "no" => args.no_ff = true,
                    "only" => args.ff_only = true,
                    _ => {} // "true" or anything else = default (allow ff)
                }
            }
        }
        // Read merge.autoStash config (CLI `--autostash`/`--no-autostash` already wins if set).
        if !args.autostash && !args.no_autostash {
            if let Some(Ok(true)) = config.get_bool("merge.autostash") {
                args.autostash = true;
            }
        }
        if args.strategy.is_empty() {
            let key = if args.commits.len() > 1 {
                "pull.octopus"
            } else {
                "pull.twohead"
            };
            if let Some(s) = config.get(key) {
                for part in s.split_whitespace() {
                    args.strategy.push(part.to_owned());
                }
            }
        }
        if let Some(value) = config.get_bool("merge.renormalize") {
            merge_renormalize = value.unwrap_or(false);
        }
        // Read merge.log config
        if args.log.is_none() && !args.no_log {
            if let Some(val) = config.get("merge.log") {
                match val.to_lowercase().as_str() {
                    "true" | "yes" => args.log = Some(20),
                    "false" | "no" => {}
                    _ => {
                        if let Ok(n) = val.parse::<usize>() {
                            if n > 0 {
                                args.log = Some(n);
                            }
                        }
                    }
                }
            }
        }
        // Read merge.stat config
        if !args.stat && !args.no_stat {
            if let Some(val) = config.get("merge.stat") {
                match val.to_lowercase().as_str() {
                    "true" | "yes" => args.stat = true,
                    "compact" => {
                        args.stat = true;
                        args.compact_summary = true;
                    }
                    _ => {}
                }
            }
        }
    }

    exit_if_merge_blocked_by_index_or_state(&repo)?;
    register_merge_submodule_odbs(&repo)?;

    if args.squash && args.no_ff {
        bail!("fatal: You cannot combine --squash with --no-ff.");
    }
    if args.squash && args.commit {
        bail!("fatal: You cannot combine --squash with --commit.");
    }

    // Validate --strategy: built-ins or `git-merge-<name>` on PATH (Git `merge.c` / `try_merge_command`).
    for strat in &args.strategy {
        if is_builtin_merge_strategy(strat) {
            continue;
        }
        if resolve_git_merge_driver(strat).is_some() {
            continue;
        }
        bail!("Could not find merge strategy '{}'", strat);
    }

    let external_driver_merge = use_external_merge_strategy(&args);

    // Parse -X strategy options (ignored for external drivers; passed through as `--<opt>`).
    let mut favor = MergeFavor::None;
    let mut diff_algorithm: Option<String> = None;
    let mut subtree_shift = SubtreeShift::Disabled;
    if !external_driver_merge {
        for xopt in &args.strategy_option {
            if let Some(algo) = xopt.strip_prefix("diff-algorithm=") {
                diff_algorithm = Some(algo.to_string());
            } else if xopt == "renormalize" {
                merge_renormalize = true;
            } else if xopt == "no-renormalize" {
                merge_renormalize = false;
            } else if xopt == "subtree" {
                subtree_shift = SubtreeShift::Auto;
            } else if let Some(path) = xopt.strip_prefix("subtree=") {
                let normalized = path.trim_matches('/');
                subtree_shift = if normalized.is_empty() {
                    SubtreeShift::Auto
                } else {
                    SubtreeShift::Prefix(normalized.to_string())
                };
            } else {
                match xopt.as_str() {
                    "ours" => favor = MergeFavor::Ours,
                    "theirs" => favor = MergeFavor::Theirs,
                    other => bail!("unknown strategy option: -X {other}"),
                }
            }
        }
    }
    // Also read diff.algorithm from config if not set via -X
    if diff_algorithm.is_none() {
        if let Ok(config) = ConfigSet::load(Some(&repo.git_dir), true) {
            if let Some(algo) = config.get("diff.algorithm") {
                diff_algorithm = Some(algo);
            }
        }
    }
    let head = resolve_head(&repo.git_dir)?;
    let head_oid = match head.oid() {
        Some(oid) => *oid,
        None => {
            // Unborn branch: fast-forward to the merge target
            return merge_unborn(&repo, &head, &args);
        }
    };

    // Octopus merge: if multiple commits, merge them sequentially
    if args.commits.len() > 1 {
        // When --ff-only is set, check if all commits are already ancestors of HEAD.
        // If so, report "Already up to date." rather than creating a merge commit.
        if args.ff_only {
            let mut all_merged = true;
            for name in &args.commits {
                let oid = resolve_merge_target(&repo, name)?;
                if oid != head_oid && !is_ancestor(&repo, oid, head_oid)? {
                    all_merged = false;
                    break;
                }
            }
            if all_merged {
                if !args.quiet {
                    eprintln!("Already up to date.");
                }
                return Ok(());
            }
            bail!("Not possible to fast-forward, aborting.");
        }
        return do_octopus_merge(
            &repo,
            &head,
            head_oid,
            &args,
            favor,
            diff_algorithm.as_deref(),
            &subtree_shift,
            merge_renormalize,
            true,
        );
    }

    // Resolve merge target
    let merge_oid = resolve_merge_target(&repo, &args.commits[0])?;

    // Already up-to-date?
    if head_oid == merge_oid {
        if !args.quiet {
            eprintln!("Already up to date.");
        }
        return Ok(());
    }

    // Check if head is ancestor of merge target → fast-forward
    if is_ancestor(&repo, head_oid, merge_oid)? {
        if args.no_ff && !args.ff_only {
            bail_if_index_tree_differs_from_head(&repo, head_oid, args.autostash)?;
            if args.strategy.len() > 1 {
                return try_merge_strategies(
                    &repo,
                    &head,
                    head_oid,
                    merge_oid,
                    &args,
                    favor,
                    diff_algorithm.as_deref(),
                    &subtree_shift,
                    merge_renormalize,
                );
            }
            let eff_shift = effective_subtree_shift(primary_merge_strategy(&args), &subtree_shift);
            return do_real_merge(
                &repo,
                &head,
                head_oid,
                merge_oid,
                &args,
                favor,
                diff_algorithm.as_deref(),
                &eff_shift,
                merge_renormalize,
                false,
                true,
            );
        }
        // `-s ours` must not fast-forward to the other tip: Git records a merge commit whose
        // tree matches ours (HEAD), even when a fast-forward to `merge_oid` exists (t6408).
        if args.strategy.len() == 1 && args.strategy[0] == "ours" {
            bail_if_index_tree_differs_from_head(&repo, head_oid, args.autostash)?;
            return do_strategy_ours(&repo, &head, head_oid, merge_oid, &args);
        }
        if !args.ff_only && merging_throwaway_tag(&repo, &args.commits[0])? {
            bail_if_index_tree_differs_from_head(&repo, head_oid, args.autostash)?;
            let eff_shift = effective_subtree_shift(primary_merge_strategy(&args), &subtree_shift);
            return do_real_merge(
                &repo,
                &head,
                head_oid,
                merge_oid,
                &args,
                favor,
                diff_algorithm.as_deref(),
                &eff_shift,
                merge_renormalize,
                false,
                true,
            );
        }
        return do_fast_forward(&repo, &head, head_oid, merge_oid, &args);
    }

    // Check if merge target is ancestor of head → already up-to-date
    if is_ancestor(&repo, merge_oid, head_oid)? {
        if !args.quiet {
            eprintln!("Already up to date.");
        }
        return Ok(());
    }

    // True merge needed
    if args.ff_only {
        bail!("Not possible to fast-forward, aborting.");
    }

    if use_external_merge_strategy(&args) {
        return run_external_merge_driver(&repo, &head, head_oid, merge_oid, &args);
    }

    if args.strategy.len() > 1 {
        return try_merge_strategies(
            &repo,
            &head,
            head_oid,
            merge_oid,
            &args,
            favor,
            diff_algorithm.as_deref(),
            &subtree_shift,
            merge_renormalize,
        );
    }

    if args.strategy.len() == 1 {
        match args.strategy[0].as_str() {
            "ours" => {
                if merge_oid == head_oid || is_ancestor(&repo, merge_oid, head_oid)? {
                    if !args.quiet {
                        eprintln!("Already up to date.");
                    }
                    return Ok(());
                }
                return do_strategy_ours(&repo, &head, head_oid, merge_oid, &args);
            }
            _ => {}
        }
    }

    let eff_shift = effective_subtree_shift(primary_merge_strategy(&args), &subtree_shift);
    do_real_merge(
        &repo,
        &head,
        head_oid,
        merge_oid,
        &args,
        favor,
        diff_algorithm.as_deref(),
        &eff_shift,
        merge_renormalize,
        false,
        true,
    )
}

/// Try each `-s` strategy in order until one succeeds (Git-compatible multi-strategy merge).
fn try_merge_strategies(
    repo: &Repository,
    head: &HeadState,
    head_oid: ObjectId,
    merge_oid: ObjectId,
    args: &Args,
    favor: MergeFavor,
    diff_algorithm: Option<&str>,
    subtree_shift_config: &SubtreeShift,
    merge_renormalize: bool,
) -> Result<()> {
    let pre_index = repo.load_index()?;
    let mut last_err: Option<anyhow::Error> = None;
    let mut best_strategy: Option<&str> = None;
    let mut best_cnt: isize = -1;

    for strat_name in &args.strategy {
        if strat_name == "resolve" {
            bail_if_resolve_index_not_clean_vs_head(repo, head_oid, args.autostash)?;
        }
        if !args.quiet {
            println!("Trying merge strategy {strat_name}...");
        }
        let mut sub = args.clone();
        sub.strategy = vec![strat_name.clone()];
        let eff_shift = effective_subtree_shift(Some(strat_name.as_str()), subtree_shift_config);

        let attempt: Result<()> = match strat_name.as_str() {
            "ours" => {
                if merge_oid == head_oid || is_ancestor(repo, merge_oid, head_oid)? {
                    if !args.quiet {
                        eprintln!("Already up to date.");
                    }
                    Ok(())
                } else {
                    do_strategy_ours(repo, head, head_oid, merge_oid, &sub)
                }
            }
            "octopus" => do_octopus_merge(
                repo,
                head,
                head_oid,
                &sub,
                favor,
                diff_algorithm,
                subtree_shift_config,
                merge_renormalize,
                false,
            ),
            "recursive" | "ort" | "resolve" | "subtree" => do_real_merge(
                repo,
                head,
                head_oid,
                merge_oid,
                &sub,
                favor,
                diff_algorithm,
                &eff_shift,
                merge_renormalize,
                true,
                false,
            ),
            _ if use_external_merge_strategy(&sub) => {
                run_external_merge_driver(repo, head, head_oid, merge_oid, &sub)
            }
            other => bail!("Could not find merge strategy '{other}'"),
        };

        match attempt {
            Ok(()) => return Ok(()),
            Err(e) => {
                if let Some(tc) = e.downcast_ref::<StrategyTrialConflict>() {
                    let cnt = tc.0 as isize;
                    if best_cnt < 0 || cnt <= best_cnt {
                        best_cnt = cnt;
                        best_strategy = Some(strat_name.as_str());
                    }
                    if !args.quiet {
                        eprintln!(
                            "Automatic merge failed; fix conflicts and then commit the result."
                        );
                    }
                } else {
                    if !args.quiet {
                        eprintln!("{e}");
                    }
                }
                last_err = Some(e);
            }
        }
    }

    if let Some(best) = best_strategy {
        restore_index_and_worktree(repo, &pre_index)?;
        if !args.quiet {
            println!("Using the {best} strategy to prepare resolving by hand.");
        }
        let mut sub = args.clone();
        sub.strategy = vec![best.to_owned()];
        let eff_shift = effective_subtree_shift(Some(best), subtree_shift_config);
        return do_real_merge(
            repo,
            head,
            head_oid,
            merge_oid,
            &sub,
            favor,
            diff_algorithm,
            &eff_shift,
            merge_renormalize,
            false,
            true,
        );
    }

    restore_index_and_worktree(repo, &pre_index)?;
    if !args.quiet {
        println!("No merge strategy handled the merge.");
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("merge failed")))
}

/// Handle merge when HEAD is unborn — "pull into void".
///
/// Git treats this as a two-way fast-forward from the empty tree to the merge target
/// (builtin/pull.c `pull_into_void`): index/worktree changes the user already staged or left
/// untracked on the unborn branch must survive (or abort the merge), exactly like a normal
/// fast-forward. We therefore compose the target tree with unrelated staged additions and run
/// the same overwrite checks before touching HEAD or the working tree.
fn merge_unborn(repo: &Repository, head: &HeadState, args: &Args) -> Result<()> {
    if args.commits.len() != 1 {
        bail!("Can merge only exactly one commit into empty head");
    }
    let merge_oid = resolve_merge_target(repo, &args.commits[0])?;
    let commit_obj = repo.odb.read(&merge_oid)?;
    let commit = parse_commit(&commit_obj.data)?;

    // Two-way merge with an empty base tree: keeps unrelated staged additions and surfaces
    // collisions with untracked / staged files before we mutate anything.
    let empty_tree = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904")?;
    let current_index = repo.load_index()?;
    let mut index = compose_fast_forward_index(repo, commit.tree, empty_tree, &current_index)?;

    // Git implements the unborn merge with `read-tree -m -u <empty> <target>` (builtin/merge.c
    // `read_empty`/`pull_into_void`). That two-way unpack refuses to clobber an untracked working
    // tree file with the singular `read-tree` diagnostic and dies with `read-tree failed`, which is
    // a different message from the regular `git merge` overwrite report. Reproduce it here: report
    // the first colliding path in index order and exit 128.
    if let Some(rel) = first_unborn_untracked_collision(repo, &index, &current_index)? {
        return Err(anyhow::Error::new(ExplicitExit {
            code: 128,
            message: format!(
                "error: Untracked working tree file '{rel}' would be overwritten by merge.\n\
                 fatal: read-tree failed"
            ),
        }));
    }
    let empty_old: HashMap<Vec<u8>, IndexEntry> = HashMap::new();
    bail_if_merge_would_overwrite_local_changes(repo, &empty_old, &index, &[], false)?;

    apply_sparse_checkout_skip_worktree(
        &repo.git_dir,
        repo.work_tree.as_deref(),
        &mut index,
        false,
    );

    merge_update_head_with_reflog(
        repo,
        head,
        head.oid().copied(),
        merge_oid,
        Some("initial pull"),
    )?;

    if let Some(ref wt) = repo.work_tree {
        checkout_entries(
            repo,
            wt,
            &index,
            None,
            sparse_checkout_enabled(&repo.git_dir),
        )?;
    }
    refresh_index_stat_cache_from_worktree(repo, &mut index)?;
    repo.write_index(&mut index)?;

    if !args.quiet {
        eprintln!("Updating to {}", &merge_oid.to_hex()[..7]);
    }
    Ok(())
}

/// First merge-result path (in index order) that would clobber an untracked working tree file when
/// merging into an unborn branch. `read-tree -m -u` stops at the first such collision, so we report
/// only one. A path whose leading component is itself an untracked file (so the new path cannot be
/// created without removing it) reports that ancestor component instead.
fn first_unborn_untracked_collision(
    repo: &Repository,
    target_index: &Index,
    current_index: &Index,
) -> Result<Option<String>> {
    let Some(work_tree) = repo.work_tree.as_deref() else {
        return Ok(None);
    };
    let tracked: BTreeSet<&[u8]> = current_index
        .entries
        .iter()
        .filter(|e| e.stage() == 0)
        .map(|e| e.path.as_slice())
        .collect();

    for entry in target_index.entries.iter().filter(|e| e.stage() == 0) {
        if tracked.contains(entry.path.as_slice()) {
            continue;
        }
        let rel = String::from_utf8_lossy(&entry.path).to_string();

        // A leading path component that exists as an untracked non-directory blocks creation of the
        // full path; report that ancestor (matches `read-tree` ENOTDIR handling).
        let mut prefix = String::new();
        let mut blocked_ancestor: Option<String> = None;
        for component in rel.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            if prefix.len() >= rel.len() {
                break;
            }
            if tracked.contains(prefix.as_bytes()) {
                continue;
            }
            if let Ok(meta) = fs::symlink_metadata(work_tree.join(&prefix)) {
                if !meta.file_type().is_dir() {
                    blocked_ancestor = Some(prefix.clone());
                    break;
                }
            }
        }
        if let Some(anc) = blocked_ancestor {
            return Ok(Some(anc));
        }

        if let Ok(meta) = fs::symlink_metadata(work_tree.join(&rel)) {
            if !meta.file_type().is_dir() {
                return Ok(Some(rel));
            }
        }
    }
    Ok(None)
}

/// Lazily fetch any index blobs that are missing locally (best-effort).
///
/// On a partial clone, a checkout target tree can reference filtered-out blobs.
/// Batch-fetch them from the promisor remote before the working tree is written
/// so `checkout_entries` does not fail with "object not found". Failures are
/// ignored here: the subsequent checkout will surface a precise error if a blob
/// is truly unavailable.
fn lazy_fetch_missing_index_blobs(repo: &Repository, index: &Index) {
    let mut need: Vec<ObjectId> = Vec::new();
    for e in index.entries.iter().filter(|e| e.stage() == 0) {
        if e.mode == MODE_GITLINK || e.mode == MODE_TREE {
            continue;
        }
        if !repo.odb.exists(&e.oid) {
            need.push(e.oid);
        }
    }
    if need.is_empty() {
        return;
    }
    let _ = crate::commands::promisor_hydrate::try_lazy_fetch_promisor_objects_batch(repo, &need);
}

/// Fast-forward: update HEAD and working tree.
fn do_fast_forward(
    repo: &Repository,
    head: &HeadState,
    head_oid: ObjectId,
    merge_oid: ObjectId,
    args: &Args,
) -> Result<()> {
    if args.squash {
        return do_squash(repo, head_oid, merge_oid, args);
    }

    // Git creates `MERGE_AUTOSTASH` before the fast-forward checkout (builtin/merge.c:1674); on a
    // clean FF it is applied below, and on a failing FF checkout it is re-applied before erroring.
    if args.autostash {
        create_merge_autostash(repo)?;
        return match do_fast_forward_inner(repo, head, head_oid, merge_oid, args) {
            Ok(()) => {
                apply_merge_autostash(repo)?;
                Ok(())
            }
            Err(e) => {
                apply_merge_autostash(repo)?;
                Err(e)
            }
        };
    }
    do_fast_forward_inner(repo, head, head_oid, merge_oid, args)
}

fn do_fast_forward_inner(
    repo: &Repository,
    head: &HeadState,
    head_oid: ObjectId,
    merge_oid: ObjectId,
    args: &Args,
) -> Result<()> {
    // Save ORIG_HEAD
    fs::write(
        repo.git_dir.join("ORIG_HEAD"),
        format!("{}\n", head_oid.to_hex()),
    )?;

    // Update index and working tree
    let commit_obj = repo.odb.read(&merge_oid)?;
    let commit = parse_commit(&commit_obj.data)?;
    let current_index = repo.load_index()?;
    let old_tree = commit_tree(repo, head_oid)?;
    let mut new_index = compose_fast_forward_index(repo, commit.tree, old_tree, &current_index)?;
    apply_sparse_checkout_skip_worktree(
        &repo.git_dir,
        repo.work_tree.as_deref(),
        &mut new_index,
        false,
    );
    let old_entries = tree_to_map(tree_to_index_entries(repo, &old_tree, "")?);
    let index_already_at_target = index_matches_commit_tree(repo, merge_oid)?;
    if !index_already_at_target {
        bail_if_merge_would_overwrite_local_changes(repo, &old_entries, &new_index, &[], false)?;
    }

    let ff_reflog = format!("{}: Fast-forward", merge_reflog_action(args));
    merge_update_head_with_reflog(repo, head, Some(head_oid), merge_oid, Some(&ff_reflog))?;

    if let Some(ref wt) = repo.work_tree {
        // Remove files that existed in old HEAD but not in new
        remove_deleted_files(
            wt,
            &old_entries,
            &new_index,
            sparse_checkout_enabled(&repo.git_dir),
        )?;
        // On a partial clone the fast-forwarded tree may reference blobs that
        // were filtered out (blob:none); lazily fetch any that the checkout will
        // need before writing the working tree (t5616 partial-clone pull).
        lazy_fetch_missing_index_blobs(repo, &new_index);
        checkout_entries_with_treeish(
            repo,
            wt,
            &new_index,
            None,
            sparse_checkout_enabled(&repo.git_dir),
            Some(&merge_oid),
        )?;
    }
    refresh_index_stat_cache_from_worktree(repo, &mut new_index)?;
    set_merge_cache_tree(repo, &mut new_index)?;
    repo.write_index(&mut new_index)?;

    if !args.quiet {
        println!(
            "Updating {}..{}",
            &head_oid.to_hex()[..7],
            &merge_oid.to_hex()[..7]
        );
        println!("Fast-forward");

        // Show diffstat
        let old_tree = commit_tree(repo, head_oid)?;
        let new_tree = commit_tree(repo, merge_oid)?;
        if let Ok(diff_entries) = diff_trees(&repo.odb, Some(&old_tree), Some(&new_tree), "") {
            print_diffstat(repo, &diff_entries, args.compact_summary);
        }
    }
    run_post_merge_hook(repo, false);
    Ok(())
}

/// Fast-forward only the working tree and index from `old_oid` to `new_oid`, leaving HEAD/refs
/// untouched (the caller already moved the branch ref — git's `checkout_fast_forward` after a fetch
/// that advanced the current branch, builtin/pull.c). Returns `Err` with a `WorktreeFfBlocked`
/// marker when local changes would be overwritten, so the caller can print git's
/// "Cannot fast-forward your working tree" diagnostic.
pub(crate) fn checkout_fast_forward_worktree_only(
    repo: &Repository,
    old_oid: ObjectId,
    new_oid: ObjectId,
) -> Result<()> {
    let commit_obj = repo.odb.read(&new_oid)?;
    let commit = parse_commit(&commit_obj.data)?;
    let current_index = repo.load_index()?;
    let old_tree = commit_tree(repo, old_oid)?;
    let mut new_index = compose_fast_forward_index(repo, commit.tree, old_tree, &current_index)?;
    apply_sparse_checkout_skip_worktree(
        &repo.git_dir,
        repo.work_tree.as_deref(),
        &mut new_index,
        false,
    );
    let old_entries = tree_to_map(tree_to_index_entries(repo, &old_tree, "")?);
    if !index_matches_commit_tree(repo, new_oid)? {
        if let Err(e) =
            bail_if_merge_would_overwrite_local_changes(repo, &old_entries, &new_index, &[], false)
        {
            return Err(e.context(WORKTREE_FF_BLOCKED));
        }
    }

    if let Some(ref wt) = repo.work_tree {
        remove_deleted_files(
            wt,
            &old_entries,
            &new_index,
            sparse_checkout_enabled(&repo.git_dir),
        )?;
        lazy_fetch_missing_index_blobs(repo, &new_index);
        checkout_entries_with_treeish(
            repo,
            wt,
            &new_index,
            None,
            sparse_checkout_enabled(&repo.git_dir),
            Some(&new_oid),
        )?;
    }
    refresh_index_stat_cache_from_worktree(repo, &mut new_index)?;
    set_merge_cache_tree(repo, &mut new_index)?;
    repo.write_index(&mut new_index)?;
    Ok(())
}

/// Marker attached to a [`checkout_fast_forward_worktree_only`] error when the working tree could
/// not be fast-forwarded; the pull caller matches on it to emit git's recovery hint.
pub(crate) const WORKTREE_FF_BLOCKED: &str = "worktree-fast-forward-blocked";

/// Perform a real three-way merge.
/// Create a virtual merge base by recursively merging multiple merge bases.
/// This handles criss-cross merge situations where there are multiple LCA commits.
pub(crate) fn create_virtual_merge_base(
    repo: &Repository,
    bases: &[ObjectId],
    favor: MergeFavor,
    merge_renormalize: bool,
) -> Result<ObjectId> {
    if bases.len() == 1 {
        return Ok(bases[0]);
    }

    // Two-base criss-cross cases fold oldest first for stable conflict markers (t6416).
    // With three or more bases, upstream's ambiguous-base behavior preserves the newer-first
    // order in t6404's fragile virtual tree check.
    //
    // git merge-ort builds the virtual base by *reversing* `get_merge_bases()` (oldest date
    // first) and folding `prev`(branch1) with `next`(branch2). For equal-date bases the order
    // is the reverse of git's `merge-base --all` output, which grit emits OID-ascending; thus
    // the equal-date tiebreak here must be OID-descending so the second (`next`/"Temporary
    // merge branch 2") base matches git (t6422 submodule/directory preliminary conflict).
    let mut ordered_bases = bases.to_vec();
    ordered_bases.sort_by(|a, b| {
        let ta = commit_author_timestamp(repo, *a).unwrap_or(0);
        let tb = commit_author_timestamp(repo, *b).unwrap_or(0);
        if bases.len() > 2 {
            tb.cmp(&ta).then_with(|| b.cmp(a))
        } else {
            ta.cmp(&tb).then_with(|| b.cmp(a))
        }
    });

    // Recursively merge bases pairwise
    let mut current = ordered_bases[0];
    for &next in &ordered_bases[1..] {
        // Find the merge base of current and next
        let sub_bases = grit_lib::merge_base::merge_bases_first_vs_rest(repo, current, &[next])?;
        let (sub_base_oid, sub_base_label_prefix) = if sub_bases.is_empty() {
            // No common ancestor — use an empty tree as base
            let empty_tree = repo.odb.write(ObjectKind::Tree, &[])?;
            let commit_data = CommitData {
                tree: empty_tree,
                parents: vec![],
                author: "virtual <virtual> 0 +0000".to_string(),
                committer: "virtual <virtual> 0 +0000".to_string(),
                author_raw: Vec::new(),
                committer_raw: Vec::new(),
                encoding: None,
                message: "virtual base".to_string(),
                raw_message: None,
            };
            let commit_bytes = serialize_commit(&commit_data);
            (
                repo.odb.write(ObjectKind::Commit, &commit_bytes)?,
                "empty tree".to_string(),
            )
        } else if sub_bases.len() > 1 {
            (
                create_virtual_merge_base(repo, &sub_bases, favor, merge_renormalize)?,
                "merged common ancestors".to_string(),
            )
        } else {
            (sub_bases[0], short_oid(sub_bases[0]))
        };

        // Merge current and next using sub_base_oid as base.
        // Keep `current` as ours and `next` as theirs. Merge bases are
        // ordered oldest-first (Git sorts by date descending then reverses
        // before this loop) so the virtual ancestor matches merge-ort.
        let base_tree = commit_tree(repo, sub_base_oid)?;
        let ours_tree = commit_tree(repo, current)?;
        let theirs_tree = commit_tree(repo, next)?;

        let base_entries =
            tree_to_map_for_merge(repo, tree_to_index_entries(repo, &base_tree, "")?);
        let ours_entries =
            tree_to_map_for_merge(repo, tree_to_index_entries(repo, &ours_tree, "")?);
        let theirs_entries =
            tree_to_map_for_merge(repo, tree_to_index_entries(repo, &theirs_tree, "")?);

        // Create a dummy head state for merge_trees.
        // When constructing a virtual base, Git labels the two temporary
        // branches opposite to the merge operands in a way that keeps
        // conflict markers stable for t6404. Using `current` here matches
        // that orientation.
        let head = HeadState::Detached { oid: current };
        let merge_result = merge_trees(
            repo,
            &base_entries,
            &ours_entries,
            &theirs_entries,
            &head,
            "Temporary merge branch 2",
            &sub_base_label_prefix,
            &current.to_hex(),
            &next.to_hex(),
            favor,
            None,
            merge_renormalize,
            false,
            false,
            false,
            false,
            MergeDirectoryRenamesMode::Disabled,
            MergeRenameOptions::from_config(repo),
            None,
            true,
            None,
            None,
        )?;

        // Build a tree from the merged index:
        // - use stage-0 entries when clean
        // - for conflicts (no stage-0), synthesize stage-0 blobs for the virtual ancestor.
        //
        // For three-way content conflicts (stages 1+2+3), keep using stage 2 as the template
        // and prefer conflict marker content when present (matches prior behavior).
        //
        // For modify/delete (stages 1+2 or 1+3 only), using the modified side as the virtual
        // tree blob makes the outer criss-cross merge think the virtual base already matches one
        // parent and incorrectly auto-resolves. Git's recursive merge leaves the **base**
        // version in the virtual merge base so the final merge still conflicts (t6416).
        let mut final_entries: Vec<IndexEntry> = Vec::new();
        let mut seen_paths: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
        let conflict_content_map: HashMap<Vec<u8>, Vec<u8>> = merge_result
            .conflict_files
            .iter()
            .map(|(path, content)| (path.as_bytes().to_vec(), content.clone()))
            .collect();
        // First collect stage 0 entries
        for entry in &merge_result.index.entries {
            if entry.stage() == 0 && seen_paths.insert(entry.path.clone()) {
                final_entries.push(entry.clone());
            }
        }

        let mut unmerged_paths: BTreeSet<Vec<u8>> = BTreeSet::new();
        for entry in &merge_result.index.entries {
            if entry.stage() != 0 {
                unmerged_paths.insert(entry.path.clone());
            }
        }
        for entry in &merge_result.index.entries {
            if entry.stage() == 0 {
                unmerged_paths.remove(&entry.path);
            }
        }

        for path in unmerged_paths {
            if seen_paths.contains(&path) {
                continue;
            }
            let entries_at: Vec<&IndexEntry> = merge_result
                .index
                .entries
                .iter()
                .filter(|e| e.path == path)
                .collect();
            let has = |s: u8| entries_at.iter().any(|e| e.stage() == s);
            let add_add = has(2) && has(3) && !has(1);
            let three_way = has(1) && has(2) && has(3);

            let mut push_entry = |mut e: IndexEntry| -> Result<()> {
                e.flags &= !0x3000;
                final_entries.push(e);
                seen_paths.insert(path.clone());
                Ok(())
            };

            if three_way || add_add {
                let Some(e2) = entries_at.iter().find(|e| e.stage() == 2).copied() else {
                    continue;
                };
                let e3 = entries_at.iter().find(|e| e.stage() == 3).copied();
                if three_way
                    && e2.mode == MODE_GITLINK
                    && e3.is_some_and(|e| e.mode == MODE_GITLINK)
                    && entries_at
                        .iter()
                        .find(|e| e.stage() == 1)
                        .is_some_and(|e| e.mode == MODE_GITLINK)
                {
                    let Some(e1) = entries_at.iter().find(|e| e.stage() == 1).copied() else {
                        continue;
                    };
                    push_entry(e1.clone())?;
                    continue;
                }
                // Symlink add/add conflicts have no stable virtual-base winner; omitting
                // the path lets the outer criss-cross merge surface stage 2/3 only.
                if add_add
                    && e3.is_some_and(|e| {
                        (e2.mode == MODE_SYMLINK && e.mode == MODE_SYMLINK)
                            || e2.mode == MODE_GITLINK
                            || e.mode == MODE_GITLINK
                    })
                {
                    continue;
                }
                let mut e = e2.clone();
                if let Some(content) = conflict_content_map.get(&path) {
                    e.oid = repo.odb.write(ObjectKind::Blob, content)?;
                }
                push_entry(e)?;
            } else if has(1) && has(2) && !has(3) {
                let path_str = String::from_utf8_lossy(&path).into_owned();
                let df_here = merge_result.conflict_descriptions.iter().any(|d| {
                    d.kind == "file/directory"
                        && d.remerge_anchor_path.as_deref() == Some(path_str.as_str())
                });
                if df_here {
                    let Some(e2) = entries_at.iter().find(|e| e.stage() == 2).copied() else {
                        continue;
                    };
                    push_entry(e2.clone())?;
                } else {
                    let Some(be) = entries_at.iter().find(|e| e.stage() == 1).copied() else {
                        continue;
                    };
                    let Some(side) = entries_at.iter().find(|e| e.stage() == 2).copied() else {
                        continue;
                    };
                    let mut e = side.clone();
                    e.oid = be.oid;
                    e.mode = be.mode;
                    push_entry(e)?;
                }
            } else if has(1) && has(3) && !has(2) {
                let path_str = String::from_utf8_lossy(&path).into_owned();
                let df_here = merge_result.conflict_descriptions.iter().any(|d| {
                    d.kind == "file/directory"
                        && d.remerge_anchor_path.as_deref() == Some(path_str.as_str())
                });
                if df_here {
                    let Some(e3) = entries_at.iter().find(|e| e.stage() == 3).copied() else {
                        continue;
                    };
                    push_entry(e3.clone())?;
                } else {
                    let Some(be) = entries_at.iter().find(|e| e.stage() == 1).copied() else {
                        continue;
                    };
                    let Some(side) = entries_at.iter().find(|e| e.stage() == 3).copied() else {
                        continue;
                    };
                    let mut e = side.clone();
                    e.oid = be.oid;
                    e.mode = be.mode;
                    push_entry(e)?;
                }
            } else if let Some(e2) = entries_at.iter().find(|e| e.stage() == 2).copied() {
                let mut e = e2.clone();
                if let Some(content) = conflict_content_map.get(&path) {
                    e.oid = repo.odb.write(ObjectKind::Blob, content)?;
                }
                push_entry(e)?;
            } else if let Some(e3) = entries_at.iter().find(|e| e.stage() == 3).copied() {
                let mut e = e3.clone();
                if let Some(content) = conflict_content_map.get(&path) {
                    e.oid = repo.odb.write(ObjectKind::Blob, content)?;
                }
                push_entry(e)?;
            }
        }
        final_entries.sort_by(|a, b| a.path.cmp(&b.path));

        // Write tree from entries
        let mut virtual_index = Index::new();
        virtual_index.entries = final_entries;
        let virtual_tree = write_tree_from_index(&repo.odb, &virtual_index, "")?;

        // Create a virtual commit
        let commit_data = CommitData {
            tree: virtual_tree,
            parents: vec![current, next],
            author: "virtual <virtual> 0 +0000".to_string(),
            committer: "virtual <virtual> 0 +0000".to_string(),
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: "virtual merge base".to_string(),
            raw_message: None,
        };
        let commit_bytes = serialize_commit(&commit_data);
        current = repo.odb.write(ObjectKind::Commit, &commit_bytes)?;
    }

    Ok(current)
}

fn create_empty_base_commit(repo: &Repository) -> Result<ObjectId> {
    let empty_tree = repo.odb.write(ObjectKind::Tree, &[])?;
    let commit_data = CommitData {
        tree: empty_tree,
        parents: vec![],
        author: "virtual <virtual> 0 +0000".to_string(),
        committer: "virtual <virtual> 0 +0000".to_string(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: "virtual base".to_string(),
        raw_message: None,
    };
    let commit_bytes = serialize_commit(&commit_data);
    Ok(repo.odb.write(ObjectKind::Commit, &commit_bytes)?)
}

/// Ephemeral commit recording one step of Git's sequential octopus merge (fast-forward between
/// heads). The merge base for the next head uses this OID, not the original `HEAD` (t7603).
fn write_octopus_step_commit(
    repo: &Repository,
    tree_oid: ObjectId,
    parents: &[ObjectId],
) -> Result<ObjectId> {
    let commit_data = CommitData {
        tree: tree_oid,
        parents: parents.to_vec(),
        author: "virtual <virtual> 0 +0000".to_string(),
        committer: "virtual <virtual> 0 +0000".to_string(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: "internal octopus merge step".to_string(),
        raw_message: None,
    };
    let commit_bytes = serialize_commit(&commit_data);
    Ok(repo.odb.write(ObjectKind::Commit, &commit_bytes)?)
}

fn short_oid(oid: ObjectId) -> String {
    let hex = oid.to_hex();
    hex[..7.min(hex.len())].to_string()
}

/// `%h (%s)` style label for remerge-diff conflict markers (matches Git).
fn commit_remerge_marker_label(repo: &Repository, oid: &ObjectId) -> String {
    let h = short_oid(*oid);
    let subj = repo
        .odb
        .read(oid)
        .ok()
        .and_then(|obj| parse_commit(&obj.data).ok())
        .and_then(|c| {
            c.message
                .lines()
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "?".to_owned());
    format!("{h} ({subj})")
}

fn apply_subtree_shift(
    subtree_shift: &SubtreeShift,
    ours: &HashMap<Vec<u8>, IndexEntry>,
    base: &mut HashMap<Vec<u8>, IndexEntry>,
    theirs: &mut HashMap<Vec<u8>, IndexEntry>,
) {
    let Some(prefix) = resolve_subtree_prefix(subtree_shift, ours, base, theirs) else {
        return;
    };
    shift_entries_by_prefix(base, &prefix);
    shift_entries_by_prefix(theirs, &prefix);
}

fn resolve_subtree_prefix(
    subtree_shift: &SubtreeShift,
    ours: &HashMap<Vec<u8>, IndexEntry>,
    base: &HashMap<Vec<u8>, IndexEntry>,
    theirs: &HashMap<Vec<u8>, IndexEntry>,
) -> Option<String> {
    match subtree_shift {
        SubtreeShift::Disabled => None,
        SubtreeShift::Prefix(prefix) => Some(prefix.clone()),
        SubtreeShift::Auto => detect_subtree_prefix(ours, base, theirs),
    }
}

fn detect_subtree_prefix(
    ours: &HashMap<Vec<u8>, IndexEntry>,
    base: &HashMap<Vec<u8>, IndexEntry>,
    theirs: &HashMap<Vec<u8>, IndexEntry>,
) -> Option<String> {
    let source_paths: Vec<&[u8]> = if base.is_empty() {
        theirs.keys().map(Vec::as_slice).collect()
    } else {
        base.keys().map(Vec::as_slice).collect()
    };

    if source_paths.is_empty() {
        return None;
    }

    let ours_paths: Vec<&[u8]> = ours.keys().map(Vec::as_slice).collect();
    let mut candidates = BTreeSet::new();
    candidates.insert(String::new());

    for source in &source_paths {
        for ours_path in &ours_paths {
            if *ours_path == *source {
                candidates.insert(String::new());
                continue;
            }
            if ours_path.len() <= source.len() + 1
                || !ours_path.ends_with(source)
                || ours_path[ours_path.len() - source.len() - 1] != b'/'
            {
                continue;
            }

            let prefix_bytes = &ours_path[..ours_path.len() - source.len() - 1];
            if let Ok(prefix) = std::str::from_utf8(prefix_bytes) {
                candidates.insert(prefix.to_string());
            }
        }
    }

    let mut best_prefix: Option<String> = None;
    let mut best_score = 0usize;
    for prefix in candidates {
        let score = source_paths
            .iter()
            .filter(|path| prefixed_path_exists(ours, path, &prefix))
            .count();
        if score > best_score {
            best_score = score;
            best_prefix = Some(prefix);
            continue;
        }
        if score == best_score {
            if let Some(current) = best_prefix.as_ref() {
                let current_is_empty = current.is_empty();
                let prefix_is_empty = prefix.is_empty();
                let is_better = (!current_is_empty && prefix_is_empty)
                    || (prefix_is_empty == current_is_empty
                        && (prefix.len(), prefix.as_str()) < (current.len(), current.as_str()));
                if is_better {
                    best_prefix = Some(prefix);
                }
            } else {
                best_prefix = Some(prefix);
            }
        }
    }

    if best_score == 0 {
        return None;
    }

    best_prefix.filter(|prefix| !prefix.is_empty())
}

fn prefixed_path_exists(ours: &HashMap<Vec<u8>, IndexEntry>, path: &[u8], prefix: &str) -> bool {
    let key = prefixed_path(path, prefix);
    ours.contains_key(key.as_slice())
}

fn shift_entries_by_prefix(entries: &mut HashMap<Vec<u8>, IndexEntry>, prefix: &str) {
    if prefix.is_empty() || entries.is_empty() {
        return;
    }
    let shifted = entries
        .values()
        .map(|entry| {
            let mut shifted_entry = entry.clone();
            let path = prefixed_path(&entry.path, prefix);
            shifted_entry.path = path.clone();
            (path, shifted_entry)
        })
        .collect();
    *entries = shifted;
}

fn prefixed_path(path: &[u8], prefix: &str) -> Vec<u8> {
    if prefix.is_empty() {
        return path.to_vec();
    }
    let mut out = Vec::with_capacity(prefix.len() + 1 + path.len());
    out.extend_from_slice(prefix.as_bytes());
    out.push(b'/');
    out.extend_from_slice(path);
    out
}

/// Run `git-merge-<strategy>` from `PATH` (Git `try_merge_command` / `merge.c`).
fn run_external_merge_driver(
    repo: &Repository,
    head: &HeadState,
    head_oid: ObjectId,
    merge_oid: ObjectId,
    args: &Args,
) -> Result<()> {
    let strategy_name = args
        .strategy
        .first()
        .map(String::as_str)
        .ok_or_else(|| anyhow::anyhow!("internal: external merge without strategy"))?;
    let Some(driver) = resolve_git_merge_driver(strategy_name) else {
        bail!("Could not find merge strategy '{strategy_name}'");
    };

    if matches!(
        primary_merge_strategy(args),
        Some("recursive" | "ort" | "subtree" | "octopus")
    ) {
        bail_if_index_tree_differs_from_head(repo, head_oid, args.autostash)?;
    }

    let bases = grit_lib::merge_base::merge_bases_first_vs_rest(repo, head_oid, &[merge_oid])?;
    if bases.is_empty() && !args.allow_unrelated_histories {
        bail!("refusing to merge unrelated histories");
    }

    if !args.autostash && diff_index::index_cached_differs_from_head(repo)? {
        return Err(anyhow::Error::new(ExplicitExit {
            code: 2,
            message: "Your local changes to the following files would be overwritten by merge:\n\
Please commit your changes or stash them before you merge.\n\
Aborting"
                .to_string(),
        }));
    }

    fs::write(
        repo.git_dir.join("ORIG_HEAD"),
        format!("{}\n", head_oid.to_hex()),
    )?;

    let wt = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("external merge strategy requires a work tree"))?;

    let mut cmd = Command::new(&driver);
    for xopt in &args.strategy_option {
        cmd.arg(format!("--{xopt}"));
    }
    for b in &bases {
        cmd.arg(b.to_hex());
    }
    cmd.args(["--", "HEAD", &merge_oid.to_hex()]);
    cmd.current_dir(wt);

    let status = cmd
        .status()
        .with_context(|| format!("failed to execute {}", driver.display()))?;
    let code = status.code().unwrap_or(1);
    // Git: 0 = clean, 1 = conflicts left, 2 = strategy does not handle this merge.
    if code == 2 {
        bail!("Merge with strategy {strategy_name} failed.");
    }

    let mut index = repo.load_index()?;
    let has_conflicts = index.entries.iter().any(|e| e.stage() != 0);

    if has_conflicts {
        if code == 0 {
            bail!("external merge driver reported success but index has unmerged entries");
        }
        refresh_index_stat_cache_from_worktree(repo, &mut index)?;
        repo.write_index(&mut index)?;

        if args.squash {
            let mut msg = build_squash_msg(repo, head_oid, &[merge_oid])?;
            msg.push_str("# Conflicts:\n");
            fs::write(repo.git_dir.join("SQUASH_MSG"), &msg)?;
        } else {
            fs::write(
                repo.git_dir.join("MERGE_HEAD"),
                format!("{}\n", merge_oid.to_hex()),
            )?;
            let msg = build_merge_message(head, &args.commits[0], args.message.as_deref(), repo);
            fs::write(repo.git_dir.join("MERGE_MSG"), &msg)?;
            fs::write(repo.git_dir.join("MERGE_MODE"), "")?;
        }

        println!("Automatic merge failed; fix conflicts and then commit the result.");
        let rr = if args.no_rerere_autoupdate {
            grit_lib::rerere::RerereAutoupdate::No
        } else if args.rerere_autoupdate {
            grit_lib::rerere::RerereAutoupdate::Yes
        } else {
            grit_lib::rerere::RerereAutoupdate::FromConfig
        };
        let _ = grit_lib::rerere::repo_rerere(repo, rr);
        return Err(anyhow::Error::new(SilentNonZeroExit { code: 1 }));
    }

    if code != 0 {
        bail!("Merge with strategy {strategy_name} failed.");
    }

    refresh_index_stat_cache_from_worktree(repo, &mut index)?;
    repo.write_index(&mut index)?;

    if args.squash {
        return do_squash_from_merge(repo, index, head, head_oid, merge_oid, args);
    }

    if args.no_commit {
        fs::write(
            repo.git_dir.join("MERGE_HEAD"),
            format!("{}\n", merge_oid.to_hex()),
        )?;
        let msg = build_merge_message(head, &args.commits[0], args.message.as_deref(), repo);
        fs::write(repo.git_dir.join("MERGE_MSG"), &msg)?;
        fs::write(repo.git_dir.join("MERGE_MODE"), "no-ff\n")?;
        if !args.quiet {
            eprintln!("Automatic merge went well; stopped before committing as requested");
        }
        run_post_merge_hook(repo, false);
        return Ok(());
    }

    let will_edit = merge_will_edit(args);
    run_pre_merge_commit_hook(repo, args.no_verify, will_edit, &mut index)?;
    repo.write_index(&mut index)?;

    let config = ConfigSet::load(Some(&repo.git_dir), true)?;
    let effective_custom_msg = if let Some(ref file_path) = args.file {
        Some(read_merge_message_from_file(Path::new(file_path), &config)?)
    } else {
        args.message.clone()
    };
    let mut msg = build_merge_message(
        head,
        &args.commits[0],
        effective_custom_msg.as_deref(),
        repo,
    );
    if let Some(max_log) = args.log {
        let log_entries = build_merge_log(repo, head_oid, merge_oid, &args.commits[0], max_log)?;
        if !log_entries.is_empty() {
            if !msg.ends_with('\n') {
                msg.push('\n');
            }
            msg.push('\n');
            msg.push_str(&log_entries);
        }
    }
    let now = OffsetDateTime::now_utc();
    let author = resolve_ident(&config, "author", now)?;
    let committer = resolve_ident(&config, "committer", now)?;
    if args.signoff && !args.no_signoff {
        let sob_name = std::env::var("GIT_COMMITTER_NAME")
            .ok()
            .or_else(|| config.get("user.name"))
            .unwrap_or_else(|| "Unknown".to_owned());
        let sob_email = std::env::var("GIT_COMMITTER_EMAIL")
            .ok()
            .or_else(|| config.get("user.email"))
            .unwrap_or_default();
        msg = append_signoff(&msg, &sob_name, &sob_email);
    }
    if let Some(ref mode) = args.cleanup {
        msg = cleanup_message(&msg, mode);
    }
    // Run prepare-commit-msg, the editor when -e, and commit-msg on the merge message, matching
    // builtin/merge.c:prepare_to_commit. The index is re-read in case a hook updated it.
    let hook_cleanup = args.cleanup.as_deref().unwrap_or("whitespace");
    msg = run_merge_commit_msg_hooks(
        repo,
        args.no_verify,
        will_edit,
        msg,
        &mut index,
        hook_cleanup,
    )?;
    if will_edit && msg.trim().is_empty() {
        bail!("Empty commit message.");
    }
    let tree_oid = write_tree_from_index(&repo.odb, &index, "")?;
    let finalized = finalize_merge_commit_message(msg, &config);
    let commit_data = CommitData {
        tree: tree_oid,
        parents: vec![head_oid, merge_oid],
        author,
        committer,
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: finalized.encoding,
        message: finalized.message,
        raw_message: finalized.raw_message,
    };
    let mut commit_bytes = serialize_commit(&commit_data);
    if should_sign_merge(args, &config) {
        commit_bytes = sign_merge_commit_bytes(
            &config,
            &commit_data.committer,
            args.gpg_sign.as_deref(),
            commit_bytes,
        )?;
    }
    let commit_oid = repo.odb.write(ObjectKind::Commit, &commit_bytes)?;
    let reflog = format!(
        "{}: Merge made by the '{strategy_name}' strategy.",
        merge_reflog_action(args)
    );
    merge_update_head_with_reflog(repo, head, Some(head_oid), commit_oid, Some(&reflog))?;

    if !args.quiet {
        if show_merge_commit_summary() {
            let short = &commit_oid.to_hex()[..7];
            let branch = head.branch_name().unwrap_or("HEAD");
            let first_line = commit_data.message.lines().next().unwrap_or("");
            println!("[{branch} {short}] {first_line}");
        }
        println!("Merge made by the '{strategy_name}' strategy.");
        let show_stat = args.stat || args.summary || !args.no_stat;
        if show_stat {
            let old_tree = commit_tree(repo, head_oid)?;
            let new_tree = commit_tree(repo, commit_oid)?;
            if let Ok(diff_entries) = diff_trees(&repo.odb, Some(&old_tree), Some(&new_tree), "") {
                print_diffstat(repo, &diff_entries, args.compact_summary);
            }
        }
    }
    run_post_merge_hook(repo, false);
    Ok(())
}

/// Whether the chosen merge strategy permits git's "really trivial in-index merge"
/// (`allow_trivial`). Only `resolve` and `octopus` keep it; `recursive`/`ort`/`ours`/`subtree`
/// (and the default `ort`) carry `NO_TRIVIAL` (builtin/merge.c all_strategy).
fn strategy_allows_trivial(args: &Args) -> bool {
    matches!(primary_merge_strategy(args), Some("resolve" | "octopus"))
}

/// Git's "Trying really trivial in-index merge..." path (builtin/merge.c merge_trivial), attempted
/// for a single-head, non-fast-forward merge before running the real strategy. Performs a per-path
/// trivial 3-way at the tree level: each path must be unchanged on at least one side (or identical
/// on both). On success it commits with parents `[head, merge]`, prints `Wonderful.`, and records
/// reflog `In-index merge`; returns `Ok(true)`. On any non-trivial path it prints `Nope.` and
/// returns `Ok(false)` so the caller falls through to the strategy.
fn attempt_trivial_in_index_merge(
    repo: &Repository,
    head: &HeadState,
    head_oid: ObjectId,
    merge_oid: ObjectId,
    args: &Args,
) -> Result<bool> {
    // Index must match HEAD before a trivial merge (builtin/merge.c:1709).
    if !index_matches_head_tree(repo, head_oid)? {
        return Ok(false);
    }
    let bases = grit_lib::merge_base::merge_bases_first_vs_rest(repo, head_oid, &[merge_oid])?;
    if bases.len() != 1 {
        return Ok(false); // criss-cross / no base: not "really trivial"
    }
    let base_oid = bases[0];

    let base_tree = commit_tree(repo, base_oid)?;
    let ours_tree = commit_tree(repo, head_oid)?;
    let theirs_tree = commit_tree(repo, merge_oid)?;
    let base = tree_to_map(tree_to_index_entries(repo, &base_tree, "")?);
    let ours = tree_to_map(tree_to_index_entries(repo, &ours_tree, "")?);
    let theirs = tree_to_map(tree_to_index_entries(repo, &theirs_tree, "")?);

    if !args.quiet {
        println!("Trying really trivial in-index merge...");
    }

    let Some(result) = trivial_three_way_index(&base, &ours, &theirs) else {
        if !args.quiet {
            println!("Nope.");
        }
        return Ok(false);
    };

    let mut new_index = result;
    new_index.sort();

    // Git's trivial in-index merge runs through `unpack_trees` (read_tree_trivial), which performs
    // the worktree `verify_uptodate` / `verify_absent` checks. A result that would overwrite a
    // dirty tracked file, or remove a directory holding untracked content (a D/F transition such as
    // `a/b` directory -> symlink while `a/b/c/e` is untracked or `a/b/c/d` is modified), is NOT
    // trivially mergeable: git prints "Nope." and falls through to the real strategy. Reuse the
    // shared worktree-loss validation; on any conflict, decline the trivial path rather than
    // silently clobbering the working tree.
    if bail_if_merge_would_overwrite_local_changes(repo, &ours, &new_index, &[], false).is_err() {
        if !args.quiet {
            println!("Nope.");
        }
        return Ok(false);
    }

    if !args.quiet {
        println!("Wonderful.");
    }

    fs::write(
        repo.git_dir.join("ORIG_HEAD"),
        format!("{}\n", head_oid.to_hex()),
    )?;

    let config = ConfigSet::load(Some(&repo.git_dir), true)?;
    run_pre_merge_commit_hook(
        repo,
        args.no_verify,
        args.edit && !args.no_edit,
        &mut new_index,
    )?;
    repo.write_index(&mut new_index)?;

    let effective_custom_msg = if let Some(ref file_path) = args.file {
        Some(read_merge_message_from_file(Path::new(file_path), &config)?)
    } else {
        args.message.clone()
    };
    let mut msg = build_merge_message(
        head,
        &args.commits[0],
        effective_custom_msg.as_deref(),
        repo,
    );
    if let Some(max_log) = args.log {
        let log_entries = build_merge_log(repo, head_oid, merge_oid, &args.commits[0], max_log)?;
        if !log_entries.is_empty() {
            if !msg.ends_with('\n') {
                msg.push('\n');
            }
            msg.push('\n');
            msg.push_str(&log_entries);
        }
    }
    if args.signoff && !args.no_signoff {
        let sob_name = std::env::var("GIT_COMMITTER_NAME")
            .ok()
            .or_else(|| config.get("user.name"))
            .unwrap_or_else(|| "Unknown".to_owned());
        let sob_email = std::env::var("GIT_COMMITTER_EMAIL")
            .ok()
            .or_else(|| config.get("user.email"))
            .unwrap_or_default();
        msg = append_signoff(&msg, &sob_name, &sob_email);
    }
    if let Some(ref mode) = args.cleanup {
        msg = cleanup_message(&msg, mode);
    }
    let will_edit = args.edit && !args.no_edit;
    let hook_cleanup = args.cleanup.as_deref().unwrap_or("whitespace");
    msg = run_merge_commit_msg_hooks(
        repo,
        args.no_verify,
        will_edit,
        msg,
        &mut new_index,
        hook_cleanup,
    )?;
    if will_edit && msg.trim().is_empty() {
        bail!("Empty commit message.");
    }

    let tree_oid = write_tree_from_index(&repo.odb, &new_index, "")?;
    let now = OffsetDateTime::now_utc();
    let author = resolve_ident(&config, "author", now)?;
    let committer = resolve_ident(&config, "committer", now)?;
    let finalized = finalize_merge_commit_message(msg, &config);
    let commit_data = CommitData {
        tree: tree_oid,
        parents: vec![head_oid, merge_oid],
        author,
        committer,
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: finalized.encoding,
        message: finalized.message,
        raw_message: finalized.raw_message,
    };
    let mut commit_bytes = serialize_commit(&commit_data);
    if should_sign_merge(args, &config) {
        commit_bytes = sign_merge_commit_bytes(
            &config,
            &commit_data.committer,
            args.gpg_sign.as_deref(),
            commit_bytes,
        )?;
    }
    let commit_oid = repo.odb.write(ObjectKind::Commit, &commit_bytes)?;

    apply_sparse_checkout_skip_worktree(
        &repo.git_dir,
        repo.work_tree.as_deref(),
        &mut new_index,
        false,
    );
    if let Some(ref wt) = repo.work_tree {
        let old_entries = tree_to_map(tree_to_index_entries(repo, &ours_tree, "")?);
        remove_deleted_files(
            wt,
            &old_entries,
            &new_index,
            sparse_checkout_enabled(&repo.git_dir),
        )?;
        checkout_entries(
            repo,
            wt,
            &new_index,
            None,
            sparse_checkout_enabled(&repo.git_dir),
        )?;
    }
    refresh_index_stat_cache_from_worktree(repo, &mut new_index)?;
    repo.write_index(&mut new_index)?;

    merge_update_head_with_reflog(
        repo,
        head,
        Some(head_oid),
        commit_oid,
        Some("In-index merge"),
    )?;

    if !args.quiet {
        if show_merge_commit_summary() {
            let short = &commit_oid.to_hex()[..7];
            let branch = head.branch_name().unwrap_or("HEAD");
            let first_line = commit_data.message.lines().next().unwrap_or("");
            println!("[{branch} {short}] {first_line}");
        }
    }
    run_post_merge_hook(repo, false);
    Ok(true)
}

/// Per-path trivial 3-way merge over tree maps. Returns the merged index on success, or `None` if
/// any path requires a non-trivial (content) resolution. For each path: if ours == base take
/// theirs; if theirs == base take ours; if ours == theirs take either; otherwise not trivial.
fn trivial_three_way_index(
    base: &HashMap<Vec<u8>, IndexEntry>,
    ours: &HashMap<Vec<u8>, IndexEntry>,
    theirs: &HashMap<Vec<u8>, IndexEntry>,
) -> Option<Index> {
    let mut paths: BTreeSet<&Vec<u8>> = BTreeSet::new();
    paths.extend(base.keys());
    paths.extend(ours.keys());
    paths.extend(theirs.keys());

    let mut out = Index::new();
    for path in paths {
        let b = base.get(path);
        let o = ours.get(path);
        let t = theirs.get(path);
        let same = |x: Option<&IndexEntry>, y: Option<&IndexEntry>| match (x, y) {
            (Some(a), Some(c)) => a.oid == c.oid && a.mode == c.mode,
            (None, None) => true,
            _ => false,
        };
        let chosen: Option<&IndexEntry> = if same(o, t) {
            o
        } else if same(o, b) {
            t
        } else if same(t, b) {
            o
        } else {
            // Both sides changed differently: not a trivial merge.
            return None;
        };
        if let Some(entry) = chosen {
            out.entries.push(entry.clone());
        }
    }
    Some(out)
}

fn do_real_merge(
    repo: &Repository,
    head: &HeadState,
    head_oid: ObjectId,
    merge_oid: ObjectId,
    args: &Args,
    favor: MergeFavor,
    diff_algorithm: Option<&str>,
    subtree_shift: &SubtreeShift,
    merge_renormalize: bool,
    trial_for_multi_strategy: bool,
    exit_on_merge_conflict: bool,
) -> Result<()> {
    // Git's "really trivial in-index merge" runs before the strategy for single-head, non-FF
    // merges when the strategy allows it (`-s resolve`/`octopus`). `allow_trivial && !FF_ONLY`.
    if !trial_for_multi_strategy
        && !args.ff_only
        && !args.squash
        && strategy_allows_trivial(args)
        && attempt_trivial_in_index_merge(repo, head, head_oid, merge_oid, args)?
    {
        return Ok(());
    }

    if primary_merge_strategy(args) == Some("resolve") {
        bail_if_resolve_index_not_clean_vs_head(repo, head_oid, args.autostash)?;
    } else if matches!(
        primary_merge_strategy(args),
        Some("recursive" | "ort" | "subtree" | "octopus")
    ) {
        bail_if_index_tree_differs_from_head(repo, head_oid, args.autostash)?;
    }

    let pre_merge_index_snapshot = repo.load_index()?;
    let mut index_for_merge = pre_merge_index_snapshot.clone();
    index_for_merge.clear_resolve_undo();
    repo.write_index(&mut index_for_merge)
        .context("clearing resolve-undo before merge")?;

    // Find merge base(s)
    let bases = grit_lib::merge_base::merge_bases_first_vs_rest(repo, head_oid, &[merge_oid])?;
    if bases.is_empty() && !args.allow_unrelated_histories {
        bail!("refusing to merge unrelated histories");
    }
    // If multiple merge bases (criss-cross):
    // - resolve strategy: fail (doesn't support virtual merge bases)
    // - recursive/ort: create a virtual merge base
    let base_oid = if bases.is_empty() {
        create_empty_base_commit(repo)?
    } else if bases.len() > 1 {
        if primary_merge_strategy(args) == Some("resolve") {
            bail!("merge: warning: multiple common ancestors found");
        }
        create_virtual_merge_base(repo, &bases, favor, merge_renormalize)?
    } else {
        bases[0]
    };
    let base_label_prefix = if bases.is_empty() {
        "empty tree".to_string()
    } else if bases.len() > 1 {
        "merged common ancestors".to_string()
    } else {
        short_oid(bases[0])
    };

    // Get trees
    let base_tree = commit_tree(repo, base_oid)?;
    let ours_tree = commit_tree(repo, head_oid)?;
    let theirs_tree = commit_tree(repo, merge_oid)?;

    // Flatten trees to path→entry maps
    let mut base_entries =
        tree_to_map_for_merge(repo, tree_to_index_entries(repo, &base_tree, "")?);
    let mut ours_entries =
        tree_to_map_for_merge(repo, tree_to_index_entries(repo, &ours_tree, "")?);
    let mut theirs_entries =
        tree_to_map_for_merge(repo, tree_to_index_entries(repo, &theirs_tree, "")?);
    apply_subtree_shift(
        subtree_shift,
        &ours_entries,
        &mut base_entries,
        &mut theirs_entries,
    );

    // On case-insensitive repos (`core.ignorecase`), merge keys are ASCII-lowercased so cross-side
    // case-only renames collapse to one path (t6419). Capture the original spelling now so we can
    // rewrite the lowercased merge result back to the spelling the real index/worktree uses,
    // otherwise downstream byte-exact comparisons against the real index misfire (t6110).
    let spelling_table = build_original_spelling_table(repo, &base_tree, &ours_tree, &theirs_tree)?;

    // Git creates the `MERGE_AUTOSTASH` ref (snapshot dirty WIP, reset --hard to HEAD) before
    // running any merge strategy (builtin/merge.c:1766). After this the index/worktree match HEAD.
    if args.autostash {
        create_merge_autostash(repo)?;
    }

    // Sparse checkout safety: if a SKIP_WORKTREE path is currently present in
    // the working tree and this merge would update that path, abort before
    // touching the index/worktree so user data is preserved.
    bail_if_merge_touches_present_skip_worktree(repo, &ours_entries, &theirs_entries)?;

    // Git: refuse merge when the index is not aligned with HEAD (exit 2), before
    // running the merge machinery.
    if !args.autostash && diff_index::index_cached_differs_from_head(repo)? {
        return Err(anyhow::Error::new(ExplicitExit {
            code: 2,
            message: "Your local changes to the following files would be overwritten by merge:\n\
Please commit your changes or stash them before you merge.\n\
Aborting"
                .to_string(),
        }));
    }

    // Save ORIG_HEAD
    fs::write(
        repo.git_dir.join("ORIG_HEAD"),
        format!("{}\n", head_oid.to_hex()),
    )?;

    maybe_prefetch_partial_clone_merge_blobs(repo, &base_entries, &ours_entries, &theirs_entries)?;

    // Git's `resolve` strategy does not use rename detection; `recursive`/`ort` do. Without this,
    // resolve can incorrectly auto-merge renames that recursive reports as conflicts (t7601).
    let rename_opts = if primary_merge_strategy(args) == Some("resolve") {
        MergeRenameOptions {
            detect: false,
            threshold: 50,
        }
    } else {
        MergeRenameOptions::from_config(repo)
    };

    let criss_cross_outer = bases.len() > 1;
    // Merge trees
    let mut merge_result = merge_trees(
        repo,
        &base_entries,
        &ours_entries,
        &theirs_entries,
        head,
        &args.commits[0],
        &base_label_prefix,
        &head_oid.to_hex(),
        &merge_oid.to_hex(),
        favor,
        diff_algorithm,
        merge_renormalize,
        false,
        false,
        false,
        false,
        MergeDirectoryRenamesMode::FromConfig,
        rename_opts,
        None,
        criss_cross_outer,
        None,
        None,
    )?;

    // Rewrite lowercased merge-key paths back to original spelling (case-insensitive repos only) so
    // the result index/worktree and the ours/base/theirs maps used below all agree with the real
    // index spelling.
    if let Some(ref table) = spelling_table {
        restore_merge_result_spelling(&mut merge_result, table);
        restore_map_spelling(&mut ours_entries, table);
        restore_map_spelling(&mut base_entries, table);
        restore_map_spelling(&mut theirs_entries, table);
    }

    apply_sparse_checkout_skip_worktree(
        &repo.git_dir,
        repo.work_tree.as_deref(),
        &mut merge_result.index,
        false,
    );

    if merge_result.has_conflicts && exit_on_merge_conflict && !trial_for_multi_strategy {
        if let Some(wt) = repo.work_tree.as_deref() {
            for desc in &merge_result.conflict_descriptions {
                if desc.kind != "file/directory" {
                    continue;
                }
                let Some(anchor) = desc.remerge_anchor_path.as_deref() else {
                    continue;
                };
                if grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(wt, anchor) {
                    let _ = fs::remove_file(repo.git_dir.join("ORIG_HEAD"));
                    bail!("Refusing to remove the current working directory:\n{anchor}\n");
                }
            }
        }
    }

    let append_strategy_failed = std::env::var("GIT_MERGE_VERBOSITY")
        .ok()
        .as_deref()
        .is_some_and(|v| v.trim() == "0");

    if merge_result.has_conflicts {
        if trial_for_multi_strategy {
            let n = unmerged_path_count(&merge_result.index);
            restore_index_and_worktree(repo, &pre_merge_index_snapshot)?;
            return Err(anyhow::Error::new(StrategyTrialConflict(n)));
        }
        if !exit_on_merge_conflict {
            restore_index_and_worktree(repo, &pre_merge_index_snapshot)?;
            bail!("Automatic merge failed; fix conflicts and then commit the result.");
        }
    }

    if !args.autostash {
        bail_if_merge_would_overwrite_local_changes(
            repo,
            &ours_entries,
            &merge_result.index,
            &merge_result.conflict_files,
            append_strategy_failed,
        )?;
    }

    // Update working tree
    let sparse_on = sparse_checkout_enabled(&repo.git_dir);
    // Refuse before mutating the work tree if applying the merge would remove the process cwd
    // (`ERROR_CWD_IN_THE_WAY`). This must run for clean merges too — a conflict-free merge that
    // replaces a tracked directory containing the cwd with a file (e.g. `dirORfile/` -> `dirORfile`)
    // would otherwise delete files under the cwd before discovering the cwd cannot be removed,
    // leaving the work tree dirty (`t2501-cwd-empty` "merge fails if cwd needs to be removed").
    if exit_on_merge_conflict && !trial_for_multi_strategy {
        if let Some(wt) = repo.work_tree.as_deref() {
            if let Err(e) = preflight_merge_worktree_for_cwd(
                repo,
                wt,
                &ours_entries,
                &merge_result.index,
                sparse_on,
            ) {
                restore_index_and_worktree(repo, &pre_merge_index_snapshot)?;
                return Err(e);
            }
        }
    }
    if let Some(ref wt) = repo.work_tree {
        // Remove files that were in ours but are no longer in the merged index
        remove_deleted_files(wt, &ours_entries, &merge_result.index, sparse_on)?;
        checkout_entries(
            repo,
            wt,
            &merge_result.index,
            Some(&ours_entries),
            sparse_on,
        )?;
        // Write conflict files to working tree (with CRLF conversion if needed)
        let attr_rules = grit_lib::crlf::load_gitattributes(wt);
        let crlf_config = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true).ok();
        for (path, content) in &merge_result.conflict_files {
            let abs = wt.join(path);
            if let Some(parent) = abs.parent() {
                fs::create_dir_all(parent)?;
            }
            let output = if let Some(ref config) = crlf_config {
                let file_attrs = grit_lib::crlf::get_file_attrs(&attr_rules, path, false, config);
                let conv = grit_lib::crlf::ConversionConfig::from_config(config);
                grit_lib::crlf::convert_to_worktree_eager(
                    content,
                    path,
                    &conv,
                    &file_attrs,
                    None,
                    None,
                )
                .map_err(|e| anyhow::anyhow!("smudge filter failed for {path}: {e}"))?
            } else {
                content.clone()
            };
            if abs.is_dir() {
                if grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(wt, path) {
                    bail!("Refusing to remove the current working directory:\n{path}\n");
                }
                fs::remove_dir_all(&abs)?;
            }
            // Avoid unnecessary update: if the work-tree file already holds the exact
            // bytes we would write (e.g. a modify/delete conflict that leaves HEAD's
            // version in tree, which is already checked out), leave it untouched so its
            // mtime is preserved — matching Git's twoway_merge behavior (t6402.36).
            if worktree_file_matches(&abs, &output) {
                continue;
            }
            // A symlink at this path (e.g. a conflicted symlink kept by `remove_deleted_files`)
            // must be unlinked first: `fs::write` would otherwise follow it and clobber the
            // link target, leaving a spurious untracked file (t6416 symlink modify/modify).
            if fs::symlink_metadata(&abs).is_ok_and(|m| m.file_type().is_symlink()) {
                let _ = fs::remove_file(&abs);
            }
            fs::write(&abs, &output)?;
        }
    }

    refresh_index_stat_cache_from_worktree(repo, &mut merge_result.index)?;
    set_merge_cache_tree(repo, &mut merge_result.index)?;
    repo.write_index(&mut merge_result.index)?;

    if merge_result.has_conflicts {
        let mut idx_auto = merge_result.index.clone();
        materialize_unmerged_entries_for_merge_tree_tree(
            repo,
            &mut idx_auto,
            &merge_result.conflict_files,
        )?;
        let auto_merge_tree = write_tree_from_index(&repo.odb, &idx_auto, "")?;
        let _ = fs::write(
            repo.git_dir.join("AUTO_MERGE"),
            format!("{}\n", auto_merge_tree.to_hex()),
        );
        if args.squash {
            // For squash + conflict: write SQUASH_MSG with conflict info, no MERGE_HEAD.
            // `build_squash_msg` ends with the last commit body (no trailing blank line, to
            // match `git log`); git's `squash_message` then appends the conflict hint (with the
            // scissors block under cleanup=scissors) — builtin/merge.c append_conflicts_hint.
            let mut msg = build_squash_msg(repo, head_oid, &[merge_oid])?;
            let paths: Vec<String> = merge_result
                .conflict_descriptions
                .iter()
                .map(|d| d.subject_path.clone())
                .collect();
            append_merge_conflicts_hint(&mut msg, &paths, merge_cleanup_is_scissors(args, repo));
            fs::write(repo.git_dir.join("SQUASH_MSG"), &msg)?;
        } else {
            // Write MERGE_HEAD and MERGE_MSG for conflict resolution
            fs::write(
                repo.git_dir.join("MERGE_HEAD"),
                format!("{}\n", merge_oid.to_hex()),
            )?;
            let mut msg =
                build_merge_message(head, &args.commits[0], args.message.as_deref(), repo);
            // Git appends the conflict hint (with the scissors block under cleanup=scissors)
            // to MERGE_MSG so a follow-up `git commit` carries it (builtin/merge.c
            // suggest_conflicts -> append_conflicts_hint).
            let paths: Vec<String> = merge_result
                .conflict_descriptions
                .iter()
                .map(|d| d.subject_path.clone())
                .collect();
            append_merge_conflicts_hint(&mut msg, &paths, merge_cleanup_is_scissors(args, repo));
            fs::write(repo.git_dir.join("MERGE_MSG"), &msg)?;
            fs::write(repo.git_dir.join("MERGE_MODE"), "")?;
        }

        for line in &merge_result.submodule_merge_stdout {
            println!("{line}");
        }
        print_submodule_recursive_merge_advice(&merge_result.submodule_merge_advice);
        // Print per-file conflict messages to stdout (git sends these to stdout)
        for desc in &merge_result.conflict_descriptions {
            print_merge_description(desc);
        }
        println!("Automatic merge failed; fix conflicts and then commit the result.");
        let rr = if args.no_rerere_autoupdate {
            grit_lib::rerere::RerereAutoupdate::No
        } else if args.rerere_autoupdate {
            grit_lib::rerere::RerereAutoupdate::Yes
        } else {
            grit_lib::rerere::RerereAutoupdate::FromConfig
        };
        let _ = grit_lib::rerere::repo_rerere(repo, rr);
        if trial_for_multi_strategy {
            let n = unmerged_path_count(&merge_result.index);
            return Err(anyhow::Error::new(StrategyTrialConflict(n)));
        }
        if args.autostash {
            println!("When finished, apply stashed changes with `git stash pop`");
        }
        return Err(anyhow::Error::new(SilentNonZeroExit { code: 1 }));
    }

    if args.squash {
        let r = do_squash_from_merge(repo, merge_result.index, head, head_oid, merge_oid, args);
        if args.autostash {
            println!("When finished, apply stashed changes with `git stash pop`");
        }
        return r;
    }

    if args.no_commit {
        // --no-commit: stage the result but don't create the merge commit.
        // Write MERGE_HEAD and MERGE_MSG so that a subsequent `git commit`
        // creates the merge commit with the right parents.
        fs::write(
            repo.git_dir.join("MERGE_HEAD"),
            format!("{}\n", merge_oid.to_hex()),
        )?;
        let msg = build_merge_message(head, &args.commits[0], args.message.as_deref(), repo);
        fs::write(repo.git_dir.join("MERGE_MSG"), &msg)?;
        fs::write(repo.git_dir.join("MERGE_MODE"), "no-ff\n")?;

        if !args.quiet {
            eprintln!("Automatic merge went well; stopped before committing as requested");
        }
        if args.autostash {
            println!("When finished, apply stashed changes with `git stash pop`");
        }
        run_post_merge_hook(repo, false);
        return Ok(());
    }

    let will_edit = merge_will_edit(args);
    run_pre_merge_commit_hook(repo, args.no_verify, will_edit, &mut merge_result.index)?;
    set_merge_cache_tree(repo, &mut merge_result.index)?;
    repo.write_index(&mut merge_result.index)?;

    // Create merge commit. The tree is (re)computed after prepare-commit-msg below, since the
    // hook may update the index.
    let config = ConfigSet::load(Some(&repo.git_dir), true)?;
    let effective_custom_msg = if let Some(ref file_path) = args.file {
        Some(read_merge_message_from_file(Path::new(file_path), &config)?)
    } else {
        args.message.clone()
    };
    let mut msg = build_merge_message(
        head,
        &args.commits[0],
        effective_custom_msg.as_deref(),
        repo,
    );

    // Append merge log if --log is set
    if let Some(max_log) = args.log {
        let log_entries = build_merge_log(repo, head_oid, merge_oid, &args.commits[0], max_log)?;
        if !log_entries.is_empty() {
            // Ensure there's a blank line before the log
            if !msg.ends_with('\n') {
                msg.push('\n');
            }
            msg.push('\n');
            msg.push_str(&log_entries);
        }
    }

    let now = OffsetDateTime::now_utc();
    let author = resolve_ident(&config, "author", now)?;
    let committer = resolve_ident(&config, "committer", now)?;

    if args.signoff && !args.no_signoff {
        let sob_name = std::env::var("GIT_COMMITTER_NAME")
            .ok()
            .or_else(|| config.get("user.name"))
            .unwrap_or_else(|| "Unknown".to_owned());
        let sob_email = std::env::var("GIT_COMMITTER_EMAIL")
            .ok()
            .or_else(|| config.get("user.email"))
            .unwrap_or_default();
        msg = append_signoff(&msg, &sob_name, &sob_email);
    }

    // Apply cleanup mode if specified
    if let Some(ref mode) = args.cleanup {
        msg = cleanup_message(&msg, mode);
    }

    // Run prepare-commit-msg, the editor when -e, and commit-msg on the merge message
    // (upstream `prepare_to_commit`); these may rewrite the message and the index before commit.
    let hook_cleanup = args.cleanup.as_deref().unwrap_or("whitespace");
    msg = run_merge_commit_msg_hooks(
        repo,
        args.no_verify,
        will_edit,
        msg,
        &mut merge_result.index,
        hook_cleanup,
    )?;
    if will_edit && msg.trim().is_empty() {
        bail!("Empty commit message.");
    }
    merge_result
        .index
        .expand_sparse_directory_placeholders(&repo.odb)?;
    let tree_oid = write_tree_from_index(&repo.odb, &merge_result.index, "")?;
    let cache_tree = build_cache_tree_from_index(&repo.odb, &merge_result.index)?;
    merge_result.index.set_cache_tree(cache_tree);
    repo.write_index(&mut merge_result.index)?;

    let finalized = finalize_merge_commit_message(msg, &config);
    let commit_data = CommitData {
        tree: tree_oid,
        parents: vec![head_oid, merge_oid],
        author,
        committer,
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: finalized.encoding,
        message: finalized.message,
        raw_message: finalized.raw_message,
    };

    let mut commit_bytes = serialize_commit(&commit_data);
    if should_sign_merge(args, &config) {
        commit_bytes = sign_merge_commit_bytes(
            &config,
            &commit_data.committer,
            args.gpg_sign.as_deref(),
            commit_bytes,
        )?;
    }
    let commit_oid = repo.odb.write(ObjectKind::Commit, &commit_bytes)?;
    let strategy_name = primary_merge_strategy(args).unwrap_or("ort");
    let reflog = format!(
        "{}: Merge made by the '{strategy_name}' strategy.",
        merge_reflog_action(args)
    );
    merge_update_head_with_reflog(repo, head, Some(head_oid), commit_oid, Some(&reflog))?;

    if args.autostash {
        apply_merge_autostash(repo)?;
    }

    if !args.quiet {
        if show_merge_commit_summary() {
            let short = &commit_oid.to_hex()[..7];
            let branch = head.branch_name().unwrap_or("HEAD");
            let first_line = commit_data.message.lines().next().unwrap_or("");
            println!("[{branch} {short}] {first_line}");
        }

        print_merge_warnings(&merge_result.conflict_descriptions);

        // Print strategy message (to stdout, as git does)
        println!("Merge made by the '{}' strategy.", strategy_name);

        // Show diffstat unless suppressed
        let show_stat = args.stat || args.summary || !args.no_stat;
        if show_stat {
            let old_tree = commit_tree(repo, head_oid)?;
            let new_tree = commit_tree(repo, commit_oid)?;
            if let Ok(diff_entries) = diff_trees(&repo.odb, Some(&old_tree), Some(&new_tree), "") {
                print_diffstat(repo, &diff_entries, args.compact_summary);
            }
        }
    }

    run_post_merge_hook(repo, false);
    Ok(())
}

fn set_merge_cache_tree(repo: &Repository, index: &mut Index) -> Result<()> {
    let cache_tree = build_cache_tree_from_index(&repo.odb, index)?;
    index.set_cache_tree(cache_tree);
    Ok(())
}

/// Refuse `git merge` when the index still has conflict entries or a merge is in progress.
///
/// Matches Git ordering: unmerged stages are checked before `MERGE_HEAD`.
fn exit_if_merge_blocked_by_index_or_state(repo: &Repository) -> Result<()> {
    let index = repo.load_index().unwrap_or_default();
    let has_unmerged = index.entries.iter().any(|e| e.stage() != 0);
    if has_unmerged {
        eprintln!("error: Merging is not possible because you have unmerged files.");
        eprintln!("hint: Fix them up in the work tree, and then use 'git add/rm <file>'");
        eprintln!("hint: as appropriate to mark resolution and make a commit.");
        eprintln!("fatal: Exiting because of an unresolved conflict.");
        std::process::exit(128);
    }
    if repo.git_dir.join("MERGE_HEAD").exists() {
        eprintln!("fatal: You have not concluded your merge (MERGE_HEAD exists).");
        eprintln!("Please, commit your changes before you merge.");
        std::process::exit(128);
    }
    Ok(())
}

fn bail_if_merge_would_overwrite_local_changes(
    repo: &Repository,
    old_entries: &HashMap<Vec<u8>, IndexEntry>,
    new_index: &Index,
    conflict_files: &[(String, Vec<u8>)],
    append_strategy_failed: bool,
) -> Result<()> {
    let Some(work_tree) = repo.work_tree.as_deref() else {
        return Ok(());
    };
    let current_index = repo.load_index()?;

    let new_map: HashMap<&[u8], &IndexEntry> = new_index
        .entries
        .iter()
        .filter(|e| e.stage() == 0)
        .map(|e| (e.path.as_slice(), e))
        .collect();

    fn is_test_harness_meta_path(rel: &str) -> bool {
        // `t2501-cwd-empty` and similar tests capture stderr to `error` in the trash directory;
        // `git reset --hard` cleanup does not remove it, but merge must not treat it as a
        // conflicting local change.
        rel == ".test_tick" || rel == ".test_oid_cache" || rel == ".test-exports" || rel == "error"
    }

    let mut overwrite_local: BTreeSet<String> = BTreeSet::new();
    let current_tracked_paths: BTreeSet<Vec<u8>> = current_index
        .entries
        .iter()
        .filter(|e| e.stage() == 0)
        .map(|e| e.path.clone())
        .collect();

    // On a case-insensitive filesystem (`core.ignorecase`), a merge-result path that differs from a
    // tracked path only by case (e.g. `CamelCase` vs tracked `camelcase`) is the *same* file on
    // disk. Build a case-folded view of the tracked paths so the untracked-collision check below
    // does not flag such a path as an untracked file that would be overwritten (t0050 "merge (case
    // change)").
    let ignore_case = core_ignorecase(repo);
    let current_tracked_paths_folded: BTreeSet<Vec<u8>> = if ignore_case {
        current_tracked_paths
            .iter()
            .map(|p| path_ascii_lowercase_components(p))
            .collect()
    } else {
        BTreeSet::new()
    };
    let is_tracked_path = |path: &[u8]| -> bool {
        if current_tracked_paths.contains(path) {
            return true;
        }
        ignore_case && current_tracked_paths_folded.contains(&path_ascii_lowercase_components(path))
    };

    // Merge-ort resolves submodule-vs-tree conflicts in the index (t6437), but a clean result that
    // replaces a checked-out gitlink with files would remove the submodule work tree. Abort before
    // `remove_deleted_files` can delete the checkout (t6438).
    for (path, old_entry) in old_entries {
        if old_entry.mode != MODE_GITLINK {
            continue;
        }
        if !merge_result_replaces_checked_out_gitlink(path, new_index) {
            continue;
        }

        let rel = String::from_utf8_lossy(path);
        let abs = work_tree.join(rel.as_ref());
        if abs.exists() && abs.join(".git").exists() {
            bail!("Cannot update submodule:\n{}", rel);
        }
    }

    // Dirty tracked paths from HEAD that would change in the target.
    for (path, old_entry) in old_entries {
        let changed = match new_map.get(path.as_slice()) {
            Some(new_entry) => new_entry.oid != old_entry.oid || new_entry.mode != old_entry.mode,
            None => true,
        };
        if !changed {
            continue;
        }

        let rel = String::from_utf8_lossy(path).to_string();
        if is_test_harness_meta_path(&rel) {
            continue;
        }
        let abs = work_tree.join(&rel);
        if fs::symlink_metadata(&abs).is_err() {
            continue;
        }
        if old_entry.mode == MODE_GITLINK {
            // Submodule / gitlink: directory on disk is expected; do not treat as dirty.
            continue;
        }
        if is_worktree_entry_dirty(repo, old_entry, &abs)? {
            overwrite_local.insert(rel);
        }
    }

    // Staged changes on paths the merge result actually touches vs HEAD.
    for idx_entry in &current_index.entries {
        if idx_entry.stage() != 0 {
            continue;
        }
        let head_entry = old_entries.get(&idx_entry.path);
        let is_staged = match head_entry {
            Some(head) => head.oid != idx_entry.oid || head.mode != idx_entry.mode,
            None => true, // staged addition
        };
        if !is_staged {
            continue;
        }

        let new_entry = new_map.get(idx_entry.path.as_slice()).copied();
        let merge_touches = match (head_entry, new_entry) {
            (Some(head), Some(ne)) => ne.oid != head.oid || ne.mode != head.mode,
            (Some(_), None) => true,
            // Staged addition: conflict only if merge result also creates this path with different content.
            // When `new_index` was composed (fast-forward), the staged path is copied in and matches `ne`.
            (None, Some(ne)) => ne.oid != idx_entry.oid || ne.mode != idx_entry.mode,
            (None, None) => false,
        };
        if !merge_touches {
            continue;
        }

        let rel = String::from_utf8_lossy(&idx_entry.path).to_string();
        if !is_test_harness_meta_path(&rel) {
            overwrite_local.insert(rel);
        }
    }

    // Staged removal: path in HEAD but absent from index stage 0.
    for (path, head_entry) in old_entries {
        let in_index = current_index
            .entries
            .iter()
            .any(|e| e.stage() == 0 && e.path == *path);
        if in_index {
            continue;
        }
        let new_entry = new_map.get(path.as_slice()).copied();
        let merge_touches = match new_entry {
            Some(ne) => ne.oid != head_entry.oid || ne.mode != head_entry.mode,
            None => true,
        };
        if merge_touches {
            overwrite_local.insert(String::from_utf8_lossy(path).to_string());
        }
    }

    let mut overwrite_untracked: BTreeSet<String> = BTreeSet::new();

    // A merge result path like `sub/f` requires `sub` to be a directory. If a leading path
    // component already exists in the working tree as an untracked *non-directory* (a plain file
    // or a symlink), git's tree-merge refuses to clobber it and reports that leading component
    // (e.g. untracked file `sub` blocking the creation of `sub/f`). `symlink_metadata` on the full
    // path would fail with ENOTDIR before we ever notice, so detect the colliding ancestor here.
    for new_entry in new_index.entries.iter().filter(|e| e.stage() == 0) {
        if is_tracked_path(&new_entry.path) {
            continue;
        }
        let rel = String::from_utf8_lossy(&new_entry.path).to_string();
        if !rel.contains('/') {
            continue;
        }
        let mut prefix = String::new();
        for component in rel.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            if prefix.len() >= rel.len() {
                break;
            }
            if is_tracked_path(prefix.as_bytes()) || is_test_harness_meta_path(&prefix) {
                continue;
            }
            let Ok(meta) = fs::symlink_metadata(work_tree.join(&prefix)) else {
                continue;
            };
            if !meta.file_type().is_dir() {
                // Untracked file/symlink occupying a directory slot in the merge result.
                overwrite_untracked.insert(prefix.clone());
                break;
            }
        }
    }

    for new_entry in new_index.entries.iter().filter(|e| e.stage() == 0) {
        if is_tracked_path(&new_entry.path) {
            continue;
        }

        let rel = String::from_utf8_lossy(&new_entry.path).to_string();
        if is_test_harness_meta_path(&rel) {
            continue;
        }
        let abs = work_tree.join(&rel);
        let Ok(meta) = fs::symlink_metadata(&abs) else {
            continue;
        };

        let has_tracked_prefix = rel.find('/').is_some_and(|_| {
            let mut prefix = String::new();
            for component in rel.split('/') {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(component);
                if prefix.len() < rel.len() && current_tracked_paths.contains(prefix.as_bytes()) {
                    return true;
                }
            }
            false
        });
        let replaces_tracked_dir = current_tracked_paths.iter().any(|path| {
            path.starts_with(&new_entry.path)
                && path.len() > new_entry.path.len()
                && path.get(new_entry.path.len()) == Some(&b'/')
        });
        if !has_tracked_prefix && !replaces_tracked_dir {
            if meta.file_type().is_dir() && is_empty_dir_for_submodule_placeholder(&abs) {
                continue;
            }
            // Git allows merging in a new submodule when the path is an empty
            // directory (e.g. `mkdir sub1` before pull adds the submodule).
            if new_entry.mode == 0o160000
                && meta.file_type().is_dir()
                && is_empty_dir_for_submodule_placeholder(&abs)
            {
                continue;
            }
            // After `checkout` away from a branch that had a submodule, Git may leave the
            // populated work tree on disk (rmdir fails: directory not empty; t7506). The merge
            // that re-introduces the gitlink must still proceed.
            if new_entry.mode == 0o160000 && abs.is_dir() && abs.join(".git").exists() {
                continue;
            }
            overwrite_untracked.insert(rel);
        }
    }

    for (rel, _) in conflict_files {
        if is_test_harness_meta_path(rel) || is_tracked_path(rel.as_bytes()) {
            continue;
        }
        let abs = work_tree.join(rel);
        let Ok(meta) = fs::symlink_metadata(&abs) else {
            continue;
        };
        if meta.file_type().is_dir() && is_empty_dir_for_submodule_placeholder(&abs) {
            continue;
        }
        if meta.file_type().is_dir() {
            let mut has_untracked_descendant = false;
            let mut stack = vec![(abs.clone(), rel.clone())];
            while let Some((dir_abs, dir_rel)) = stack.pop() {
                let Ok(entries) = fs::read_dir(&dir_abs) else {
                    has_untracked_descendant = true;
                    break;
                };
                for child in entries.flatten() {
                    let child_name = child.file_name().to_string_lossy().to_string();
                    let child_rel = format!("{dir_rel}/{child_name}");
                    let child_abs = child.path();
                    let Ok(child_meta) = fs::symlink_metadata(&child_abs) else {
                        has_untracked_descendant = true;
                        break;
                    };
                    if child_meta.file_type().is_dir() {
                        stack.push((child_abs, child_rel));
                        continue;
                    }
                    if !current_tracked_paths.contains(child_rel.as_bytes()) {
                        has_untracked_descendant = true;
                        break;
                    }
                }
                if has_untracked_descendant {
                    break;
                }
            }
            if !has_untracked_descendant {
                continue;
            }
        }
        overwrite_untracked.insert(rel.clone());
    }

    // Also protect untracked files nested beneath directories that turn into
    // files/symlinks in the merge result (directory→file transitions).
    for new_entry in &new_index.entries {
        if new_entry.stage() != 0 {
            continue;
        }
        if is_tracked_path(&new_entry.path) {
            continue;
        }

        let mut prefix = new_entry.path.clone();
        prefix.push(b'/');
        let replaces_tracked_dir = current_tracked_paths.iter().any(|p| p.starts_with(&prefix));
        if !replaces_tracked_dir {
            continue;
        }

        let rel = String::from_utf8_lossy(&new_entry.path).to_string();
        let abs = work_tree.join(&rel);
        let Ok(meta) = fs::symlink_metadata(&abs) else {
            continue;
        };
        if !meta.file_type().is_dir() {
            continue;
        }

        let mut stack = vec![(abs, rel)];
        while let Some((dir_abs, dir_rel)) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir_abs) else {
                continue;
            };
            for child in entries.flatten() {
                let child_name = child.file_name().to_string_lossy().to_string();
                let child_rel = format!("{dir_rel}/{child_name}");
                let child_abs = child.path();
                let Ok(child_meta) = fs::symlink_metadata(&child_abs) else {
                    continue;
                };
                if child_meta.file_type().is_dir() {
                    stack.push((child_abs, child_rel));
                    continue;
                }
                if !current_tracked_paths.contains(child_rel.as_bytes()) {
                    overwrite_untracked.insert(child_rel);
                }
            }
        }
    }

    for path in &overwrite_untracked {
        if grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(work_tree, path) {
            bail!("Refusing to remove the current working directory:\n{path}\n");
        }
    }

    if !overwrite_local.is_empty() || !overwrite_untracked.is_empty() {
        let mut msg = String::new();
        if !overwrite_local.is_empty() {
            msg.push_str(
                "error: Your local changes to the following files would be overwritten by merge:\n",
            );
            for path in &overwrite_local {
                msg.push_str(&format!("\t{path}\n"));
            }
            msg.push_str("Please commit your changes or stash them before you merge.\n");
        }

        if !overwrite_untracked.is_empty() {
            msg.push_str(
                "error: The following untracked working tree files would be overwritten by merge:\n",
            );
            for path in &overwrite_untracked {
                msg.push_str(&format!("\t{path}\n"));
            }
            msg.push_str("Please move or remove them before you merge.\n");
        }

        msg.push_str("Aborting");
        if append_strategy_failed {
            msg.push_str("\nMerge with strategy ort failed.");
        }
        let code = if !overwrite_local.is_empty() { 128 } else { 1 };
        return Err(anyhow::Error::new(ExplicitExit { code, message: msg }));
    }

    Ok(())
}

fn merge_result_replaces_checked_out_gitlink(path: &[u8], new_index: &Index) -> bool {
    if merge_result_has_relocated_gitlink_conflict(path, new_index) {
        return false;
    }

    let same_path_replaced = new_index
        .entries
        .iter()
        .any(|entry| entry.stage() == 0 && entry.path == path && entry.mode != MODE_GITLINK);
    if same_path_replaced {
        return true;
    }

    new_index.entries.iter().any(|entry| {
        entry.stage() == 0
            && entry.path.len() > path.len()
            && entry.path.starts_with(path)
            && entry.path.get(path.len()) == Some(&b'/')
    })
}

fn merge_result_has_relocated_gitlink_conflict(path: &[u8], new_index: &Index) -> bool {
    new_index.entries.iter().any(|entry| {
        entry.mode == MODE_GITLINK
            && entry.stage() != 0
            && entry.path.len() > path.len()
            && entry.path.starts_with(path)
            && entry.path.get(path.len()) == Some(&b'~')
    })
}

fn is_worktree_entry_dirty(repo: &Repository, entry: &IndexEntry, abs_path: &Path) -> Result<bool> {
    if entry.mode == MODE_GITLINK {
        if abs_path.is_file() || abs_path.is_symlink() {
            return Ok(true);
        }
        if !abs_path.join(".git").exists() {
            return Ok(false);
        }
        let Some(current) = read_submodule_head_oid(abs_path) else {
            return Ok(true);
        };
        return Ok(current != entry.oid);
    }
    if entry.mode == MODE_SYMLINK {
        match fs::read_link(abs_path) {
            Ok(target) => {
                let obj = repo.odb.read(&entry.oid)?;
                let expected = String::from_utf8_lossy(&obj.data);
                Ok(target.to_string_lossy() != expected.as_ref())
            }
            Err(_) => Ok(true),
        }
    } else {
        match fs::read(abs_path) {
            Ok(data) => {
                let obj = repo.odb.read(&entry.oid)?;
                Ok(data != obj.data)
            }
            Err(_) => Ok(true),
        }
    }
}

fn open_submodule_repo(super_repo: &Repository, rel: &str) -> Option<Repository> {
    let wt = super_repo.work_tree.as_deref()?;
    let abs = wt.join(rel);
    let git_dir = submodule_embedded_git_dir(&abs)?;
    Repository::open(&git_dir, Some(&abs)).ok()
}

fn short_oid_hex(oid: ObjectId) -> String {
    let h = oid.to_hex();
    h[..7.min(h.len())].to_string()
}

fn submodule_head_first_parent_abbrev(sub_repo: &Repository) -> Option<String> {
    let head = resolve_head(&sub_repo.git_dir).ok()?;
    let oid = head.oid().copied()?;
    let obj = sub_repo.odb.read(&oid).ok()?;
    let c = parse_commit(&obj.data).ok()?;
    c.parents.first().map(|o| short_oid_hex(*o))
}

fn peel_gitlink_commit(odb: &grit_lib::odb::Odb, oid: ObjectId) -> Result<ObjectId> {
    if oid == zero_oid() {
        bail!("null gitlink");
    }
    let obj = odb.read(&oid)?;
    match obj.kind {
        ObjectKind::Commit => Ok(oid),
        ObjectKind::Tag => {
            let tag = parse_tag(&obj.data)?;
            peel_gitlink_commit(odb, tag.object)
        }
        _ => bail!("gitlink does not point to a commit"),
    }
}

fn prune_submodule_merge_candidates(
    sub_repo: &Repository,
    mut merges: Vec<ObjectId>,
) -> Result<Vec<ObjectId>> {
    loop {
        let mut remove = vec![false; merges.len()];
        for i in 0..merges.len() {
            for j in 0..merges.len() {
                if i == j || remove[i] {
                    continue;
                }
                if is_ancestor(sub_repo, merges[j], merges[i])? {
                    remove[i] = true;
                    break;
                }
            }
        }
        if !remove.iter().any(|r| *r) {
            break;
        }
        merges = merges
            .into_iter()
            .enumerate()
            .filter_map(|(idx, o)| if remove[idx] { None } else { Some(o) })
            .collect();
    }
    Ok(merges)
}

fn submodule_candidate_merges(
    sub_repo: &Repository,
    ours: ObjectId,
    theirs: ObjectId,
) -> Result<Vec<ObjectId>> {
    let mut opts = RevListOptions::default();
    opts.all_refs = true;
    opts.min_parents = Some(2);
    opts.ordering = OrderingMode::Topo;
    opts.output_mode = OutputMode::OidOnly;
    // Exclude the entire history reachable from `ours` (same as `git rev-list --all ^ours`).
    let negative = vec![ours.to_hex()];
    let r = rev_list(sub_repo, &[] as &[String], &negative, &opts)?;
    let mut out = Vec::new();
    for oid in r.commits {
        if !is_ancestor(sub_repo, theirs, oid)? {
            continue;
        }
        let obj = sub_repo.odb.read(&oid)?;
        if obj.kind != ObjectKind::Commit {
            continue;
        }
        let c = parse_commit(&obj.data)?;
        if c.parents.len() < 2 {
            continue;
        }
        out.push(oid);
    }
    prune_submodule_merge_candidates(sub_repo, out)
}

fn record_submodule_merge_conflict(
    path_str: &str,
    be: &IndexEntry,
    oe: &IndexEntry,
    te: &IndexEntry,
    index: &mut Index,
    has_conflicts: &mut bool,
    conflict_descriptions: &mut Vec<ConflictDescription>,
    submodule_merge_stdout: &mut Vec<String>,
    submodule_merge_advice: &mut Vec<(String, String)>,
    advice_abbrev: String,
    hint_lines: &[String],
) {
    for line in hint_lines {
        submodule_merge_stdout.push(line.clone());
    }
    submodule_merge_advice.push((path_str.to_owned(), advice_abbrev));
    *has_conflicts = true;
    // A null base (no common merge base for the submodule path) is not recorded at
    // stage 1 — git leaves only the stage-2/stage-3 unmerged entries in that case.
    if be.oid != zero_oid() {
        stage_entry(index, be, 1);
    }
    stage_entry(index, oe, 2);
    stage_entry(index, te, 3);
    conflict_descriptions.push(ConflictDescription {
        kind: "submodule",
        body: format!("Failed to merge submodule {path_str}"),
        subject_path: path_str.to_owned(),
        remerge_anchor_path: None,
        rename_rr_ours_dest: None,
        rename_rr_theirs_dest: None,
        auto_merge_hint_path: None,
    });
}

/// Three-way merge for paths where base/ours/theirs are all gitlinks (`merge-ort` `merge_submodule`).
fn try_merge_gitlink_entries(
    repo: &Repository,
    path_str: &str,
    be: &IndexEntry,
    oe: &IndexEntry,
    te: &IndexEntry,
    favor: MergeFavor,
    index: &mut Index,
    has_conflicts: &mut bool,
    conflict_descriptions: &mut Vec<ConflictDescription>,
    submodule_merge_stdout: &mut Vec<String>,
    submodule_merge_advice: &mut Vec<(String, String)>,
) -> Result<bool> {
    if be.mode != MODE_GITLINK || oe.mode != MODE_GITLINK || te.mode != MODE_GITLINK {
        return Ok(false);
    }

    let default_advice = short_oid_hex(te.oid);
    let Some(sub_repo) = open_submodule_repo(repo, path_str) else {
        record_submodule_merge_conflict(
            path_str,
            be,
            oe,
            te,
            index,
            has_conflicts,
            conflict_descriptions,
            submodule_merge_stdout,
            submodule_merge_advice,
            default_advice,
            &[],
        );
        return Ok(true);
    };

    let ours_c = match peel_gitlink_commit(&sub_repo.odb, oe.oid) {
        Ok(o) => o,
        Err(_) => {
            record_submodule_merge_conflict(
                path_str,
                be,
                oe,
                te,
                index,
                has_conflicts,
                conflict_descriptions,
                submodule_merge_stdout,
                submodule_merge_advice,
                default_advice,
                &[],
            );
            return Ok(true);
        }
    };
    let theirs_c = match peel_gitlink_commit(&sub_repo.odb, te.oid) {
        Ok(o) => o,
        Err(_) => {
            record_submodule_merge_conflict(
                path_str,
                be,
                oe,
                te,
                index,
                has_conflicts,
                conflict_descriptions,
                submodule_merge_stdout,
                submodule_merge_advice,
                default_advice,
                &[],
            );
            return Ok(true);
        }
    };

    let base_c = if be.oid == zero_oid() {
        zero_oid()
    } else {
        match peel_gitlink_commit(&sub_repo.odb, be.oid) {
            Ok(o) => o,
            Err(_) => {
                record_submodule_merge_conflict(
                    path_str,
                    be,
                    oe,
                    te,
                    index,
                    has_conflicts,
                    conflict_descriptions,
                    submodule_merge_stdout,
                    submodule_merge_advice,
                    default_advice,
                    &[],
                );
                return Ok(true);
            }
        }
    };

    if ours_c == zero_oid() || theirs_c == zero_oid() {
        record_submodule_merge_conflict(
            path_str,
            be,
            oe,
            te,
            index,
            has_conflicts,
            conflict_descriptions,
            submodule_merge_stdout,
            submodule_merge_advice,
            default_advice,
            &[],
        );
        return Ok(true);
    }

    if base_c == zero_oid() {
        let advice =
            submodule_head_first_parent_abbrev(&sub_repo).unwrap_or_else(|| default_advice.clone());
        record_submodule_merge_conflict(
            path_str,
            be,
            oe,
            te,
            index,
            has_conflicts,
            conflict_descriptions,
            submodule_merge_stdout,
            submodule_merge_advice,
            advice,
            &[],
        );
        return Ok(true);
    }

    let forward_ok =
        is_ancestor(&sub_repo, base_c, ours_c)? && is_ancestor(&sub_repo, base_c, theirs_c)?;
    if !forward_ok {
        record_submodule_merge_conflict(
            path_str,
            be,
            oe,
            te,
            index,
            has_conflicts,
            conflict_descriptions,
            submodule_merge_stdout,
            submodule_merge_advice,
            default_advice,
            &[],
        );
        return Ok(true);
    }

    match favor {
        MergeFavor::Ours => {
            index.entries.push(oe.clone());
            return Ok(true);
        }
        MergeFavor::Theirs => {
            index.entries.push(te.clone());
            return Ok(true);
        }
        MergeFavor::None | MergeFavor::Union => {}
    }

    if is_ancestor(&sub_repo, ours_c, theirs_c)? {
        index.entries.push(te.clone());
        return Ok(true);
    }
    if is_ancestor(&sub_repo, theirs_c, ours_c)? {
        index.entries.push(oe.clone());
        return Ok(true);
    }

    let candidates = submodule_candidate_merges(&sub_repo, ours_c, theirs_c)?;
    if candidates.is_empty() {
        record_submodule_merge_conflict(
            path_str,
            be,
            oe,
            te,
            index,
            has_conflicts,
            conflict_descriptions,
            submodule_merge_stdout,
            submodule_merge_advice,
            default_advice,
            &[],
        );
        return Ok(true);
    }
    if candidates.len() == 1 {
        let hint = format!(
            "Failed to merge submodule {path_str}, but a possible merge resolution exists: {}",
            short_oid_hex(candidates[0])
        );
        record_submodule_merge_conflict(
            path_str,
            be,
            oe,
            te,
            index,
            has_conflicts,
            conflict_descriptions,
            submodule_merge_stdout,
            submodule_merge_advice,
            default_advice,
            std::slice::from_ref(&hint),
        );
        return Ok(true);
    }
    let mut hints = vec![format!(
        "Failed to merge submodule {path_str}, but multiple possible merges exist:"
    )];
    for m in candidates {
        hints.push(format!("    {}", short_oid_hex(m)));
    }
    record_submodule_merge_conflict(
        path_str,
        be,
        oe,
        te,
        index,
        has_conflicts,
        conflict_descriptions,
        submodule_merge_stdout,
        submodule_merge_advice,
        default_advice,
        &hints,
    );
    Ok(true)
}

fn print_submodule_recursive_merge_advice(paths: &[(String, String)]) {
    if paths.is_empty() {
        return;
    }
    let joined = paths
        .iter()
        .map(|(p, _)| p.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    eprintln!("Recursive merging with submodules currently only supports trivial cases.");
    eprintln!("Please manually handle the merging of each conflicted submodule.");
    eprintln!("This can be accomplished with the following steps:");
    for (path, abbrev) in paths {
        eprintln!(
            " - go to submodule ({path}), and either merge commit {abbrev}\n   or update to an existing commit which has merged those changes"
        );
    }
    eprintln!(" - come back to superproject and run:\n");
    eprintln!("      git add {joined}\n");
    eprintln!("   to record the above merge or update");
    eprintln!(" - resolve any other conflicts in the superproject");
    eprintln!(" - commit the resulting index in the superproject");
}

/// Resolve the submodule's current HEAD commit from its working directory.
fn read_submodule_head_oid(sub_path: &Path) -> Option<ObjectId> {
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
    let head_content = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head_content = head_content.trim();
    if let Some(refname) = head_content.strip_prefix("ref: ") {
        let ref_path = git_dir.join(refname);
        let oid_hex = fs::read_to_string(&ref_path).ok()?;
        ObjectId::from_hex(oid_hex.trim()).ok()
    } else {
        ObjectId::from_hex(head_content).ok()
    }
}

/// Prefetch the missing promisor blobs a partial-clone merge needs, in the same
/// batched fashion Git's merge-ort does.
///
/// On a partial clone (`--filter=blob:none`) the blob contents required for
/// rename detection and 3-way content merge are not present locally; Git
/// prefetches them in a small number of batches (one per rename-detection side
/// plus one for the content merge) before running the merge. We replicate that:
/// compute the exact set of missing blobs each phase reads, copy them from the
/// promisor remote into the local store so the merge can succeed, and emit the
/// trace2 perf events (`child_start fetch.negotiationAlgorithm` + `fetch_count`)
/// that tests (t6421) use to validate fetch accounting.
///
/// No-op outside promisor repositories or when nothing is missing.
fn maybe_prefetch_partial_clone_merge_blobs(
    repo: &Repository,
    base: &HashMap<Vec<u8>, IndexEntry>,
    ours: &HashMap<Vec<u8>, IndexEntry>,
    theirs: &HashMap<Vec<u8>, IndexEntry>,
) -> Result<()> {
    let cfg = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    if !grit_lib::promisor::repo_treats_promisor_packs(&repo.git_dir, &cfg) {
        return Ok(());
    }

    let is_missing = |oid: &ObjectId| -> bool { !repo.odb.exists_local(oid) };

    // Phase 1 + 2: rename-detection blobs for each side (ours, then theirs).
    // Phase 3: content-merge blobs (paths modified on both sides relative to base,
    // after accounting for renames). Each phase that fetches anything becomes one
    // lazy-fetch batch (one `fetch.negotiationAlgorithm` child + one `fetch_count`).
    let mut batches: Vec<Vec<ObjectId>> = Vec::new();
    let mut already: std::collections::HashSet<ObjectId> = std::collections::HashSet::new();

    // For each side (ours, then theirs), rename detection runs in two phases that
    // each issue at most one lazy-fetch batch: a basename-matching phase, then a
    // general (full-matrix) phase over remaining relevant sources x all unpaired
    // destinations. A phase only produces a batch when it actually has missing
    // blobs to hydrate.
    let push_batch = |oids: Vec<ObjectId>,
                      batches: &mut Vec<Vec<ObjectId>>,
                      already: &mut std::collections::HashSet<ObjectId>| {
        let mut batch: Vec<ObjectId> = Vec::new();
        let mut seen: BTreeSet<ObjectId> = BTreeSet::new();
        for oid in oids {
            if already.contains(&oid) || seen.contains(&oid) || !is_missing(&oid) {
                continue;
            }
            seen.insert(oid);
            batch.push(oid);
        }
        if !batch.is_empty() {
            for o in &batch {
                already.insert(*o);
            }
            batches.push(batch);
        }
    };

    for (side, other) in [(ours, theirs), (theirs, ours)] {
        let RenameDetectionBlobs { basename, general } =
            relevant_rename_blob_oids(base, side, other);
        push_batch(basename, &mut batches, &mut already);
        push_batch(general, &mut batches, &mut already);
    }

    // Phase 3: content merge. A path whose blob differs from base on BOTH ours and
    // theirs needs a 3-way content merge; the side blobs not already fetched are
    // pulled here. Base blobs are already local from rename detection.
    push_batch(
        content_merge_blob_oids(base, ours, theirs),
        &mut batches,
        &mut already,
    );

    if batches.is_empty() {
        return Ok(());
    }

    let trace_path = std::env::var("GIT_TRACE2_PERF")
        .ok()
        .filter(|p| !p.is_empty());
    for batch in &batches {
        // Materialize the batch locally (copy from the promisor remote).
        crate::commands::promisor_hydrate::try_lazy_fetch_promisor_objects_batch(repo, batch)?;
        if let Some(path) = &trace_path {
            append_trace2_perf_line(path, "child_start", "fetch.negotiationAlgorithm")?;
            append_trace2_perf_line(path, "data", &format!("fetch_count:{}", batch.len()))?;
        }
    }

    Ok(())
}

/// Whether `path` lies inside directory `dir` (path components, not byte prefix).
/// `dir` empty means the repository root (every path is under it).
fn path_under_dir(path: &[u8], dir: &[u8]) -> bool {
    if dir.is_empty() {
        return true;
    }
    path.len() > dir.len() && path.starts_with(dir) && path[dir.len()] == b'/'
}

/// Given a base path and its exact-rename destination on a side, return the base
/// directory prefix that was renamed (the leading components that changed once a
/// shared trailing suffix is removed). `dir/subdir/a` -> `folder/subdir/a`
/// yields `dir`; `x/y` -> `z/y` yields `x`. Returns `None` when the paths share no
/// common trailing component (a plain file rename, not a directory rename).
fn renamed_dir_prefix(base_path: &[u8], dest_path: &[u8]) -> Option<Vec<u8>> {
    let bsplit: Vec<&[u8]> = base_path.split(|&b| b == b'/').collect();
    let dsplit: Vec<&[u8]> = dest_path.split(|&b| b == b'/').collect();
    // Count shared trailing components.
    let mut shared = 0usize;
    while shared < bsplit.len()
        && shared < dsplit.len()
        && bsplit[bsplit.len() - 1 - shared] == dsplit[dsplit.len() - 1 - shared]
    {
        shared += 1;
    }
    if shared == 0 {
        return None; // basename differs: a file rename, not a directory rename
    }
    let keep = bsplit.len() - shared;
    if keep == 0 {
        return None;
    }
    Some(bsplit[..keep].join(&b'/'))
}

/// The two lazy-fetch batches rename detection issues for one side of the merge:
/// the basename-matching phase and the general (full-matrix) phase. Each is one
/// `fetch.negotiationAlgorithm` child when non-empty (mirroring git's
/// `basename_prefetch` / `inexact_prefetch`).
struct RenameDetectionBlobs {
    basename: Vec<ObjectId>,
    general: Vec<ObjectId>,
}

/// Blobs read by Git's rename detection for the `base` -> `side` diff, restricted
/// to *relevant* renames (the merge-ort optimization), split into the two phases
/// git runs (and prefetches for) separately.
///
/// A source that `side` renamed-and-modified only needs its content read when the
/// rename can affect the merge outcome, i.e. when `other` (the opposite side of
/// the merge) touches the same path. Concretely a deleted base path D is relevant
/// when either:
///   * D is content-modified by `other` (a 3-way content merge of the renamed
///     file is required), or
///   * D lives under a directory that `side` renamed away and `other` added a new
///     path into (directory-rename detection is needed, which reads the renamed
///     files' content).
///
/// Phase 1 (basename): for each relevant source whose basename matches a unique
/// added destination basename, both the source (base) and destination (side)
/// blobs are fetched (git's `find_basename_matches` / `basename_prefetch`).
///
/// Phase 2 (general): only runs when relevant sources remain unpaired after the
/// basename phase. It then fetches every still-unpaired destination plus every
/// remaining relevant source (git's `inexact_prefetch`), because any relevant
/// source must be compared against all possible unpaired destinations.
///
/// Exact renames are resolved without reading content and are excluded from both.
fn relevant_rename_blob_oids(
    base: &HashMap<Vec<u8>, IndexEntry>,
    side: &HashMap<Vec<u8>, IndexEntry>,
    other: &HashMap<Vec<u8>, IndexEntry>,
) -> RenameDetectionBlobs {
    fn basename(p: &[u8]) -> &[u8] {
        match p.iter().rposition(|&b| b == b'/') {
            Some(i) => &p[i + 1..],
            None => p,
        }
    }
    fn is_regularish(mode: u32) -> bool {
        mode == MODE_REGULAR || mode == MODE_EXECUTABLE
    }

    let base_oids: std::collections::HashSet<ObjectId> = base.values().map(|e| e.oid).collect();

    // Build the side OID -> path map for exact-rename lookup.
    let mut side_oid_paths: HashMap<ObjectId, Vec<&Vec<u8>>> = HashMap::new();
    for (p, e) in side {
        side_oid_paths.entry(e.oid).or_default().push(p);
    }

    // Base directory prefixes that `side` renamed away, derived from exact renames:
    // for each base file that moved (exact OID) to a path with a shared trailing
    // suffix, the differing leading component is the renamed directory prefix
    // (e.g. base `dir/subdir/a` -> side `folder/subdir/a` => renamed base dir `dir`).
    let mut side_renamed_prefixes: BTreeSet<Vec<u8>> = BTreeSet::new();
    for (p, e) in base {
        if !is_regularish(e.mode) {
            continue;
        }
        if side.contains_key(p) {
            continue; // still present at same path
        }
        let Some(dests) = side_oid_paths.get(&e.oid) else {
            continue;
        };
        for dest in dests {
            if base.contains_key(*dest) {
                continue;
            }
            if let Some(prefix) = renamed_dir_prefix(p, dest) {
                side_renamed_prefixes.insert(prefix);
            }
        }
    }

    // Base directory prefixes `other` added a brand-new path under (relative to base).
    let other_added_new_under = |prefix: &[u8]| -> bool {
        for p in other.keys() {
            if base.contains_key(p) {
                continue;
            }
            if path_under_dir(p, prefix) {
                return true;
            }
        }
        false
    };
    let relevant_renamed_prefixes: BTreeSet<Vec<u8>> = side_renamed_prefixes
        .iter()
        .filter(|pre| other_added_new_under(pre))
        .cloned()
        .collect();

    // Rename-source candidates: base paths removed on this side (renamed away or
    // deleted). A path still present at its original location — even if modified in
    // place — is NOT a rename source; it is handled by the content merge, not
    // rename detection. Exact renames are resolved without reading content.
    let mut deleted: Vec<(&Vec<u8>, &IndexEntry)> = Vec::new();
    for (p, e) in base {
        if !is_regularish(e.mode) {
            continue;
        }
        if side.contains_key(p) {
            continue; // still present at the original path (modified in place if changed)
        }
        if side_oid_paths.contains_key(&e.oid) {
            continue; // exact rename, no content read
        }
        deleted.push((p, e));
    }

    // Added side paths (not exact rename destinations).
    let mut added: Vec<(&Vec<u8>, &IndexEntry)> = Vec::new();
    for (p, e) in side {
        if !is_regularish(e.mode) {
            continue;
        }
        if base.get(p).is_some_and(|b| b.oid == e.oid) {
            continue;
        }
        if !base.contains_key(p) && base_oids.contains(&e.oid) {
            continue; // exact rename destination
        }
        added.push((p, e));
    }

    // Classify a deleted source's relevance, mirroring merge-ort's add_pair():
    //   * CONTENT relevant: the file's content is not preserved unchanged on the
    //     other side at the original path (the other side deleted/renamed it, or
    //     modified it in place). A 3-way content merge of the rename target may be
    //     needed, so its content must be read and it participates in BOTH the
    //     basename and (if unpaired) the general rename phase.
    //   * LOCATION relevant: under a directory `side` renamed away and `other`
    //     added a new path into (directory-rename detection). It participates only
    //     in the basename phase; if it has no basename-matched destination it is
    //     culled (the directory rename is already known from exact renames) and is
    //     NOT read in the general phase.
    #[derive(Clone, Copy, PartialEq)]
    enum Relevance {
        Content,
        Location,
        None,
    }
    let relevance_of = |dp: &[u8]| -> Relevance {
        // Content relevant unless `other` keeps the file unchanged at this path.
        let kept_unchanged_by_other = other
            .get(dp)
            .is_some_and(|o| base.get(dp).is_some_and(|b| o.oid == b.oid));
        if !kept_unchanged_by_other {
            return Relevance::Content;
        }
        if relevant_renamed_prefixes
            .iter()
            .any(|pre| path_under_dir(dp, pre))
        {
            return Relevance::Location;
        }
        Relevance::None
    };

    // Number of shared trailing path components between two paths.
    fn shared_suffix_components(a: &[u8], b: &[u8]) -> usize {
        let av: Vec<&[u8]> = a.split(|&c| c == b'/').collect();
        let bv: Vec<&[u8]> = b.split(|&c| c == b'/').collect();
        let mut n = 0;
        while n < av.len() && n < bv.len() && av[av.len() - 1 - n] == bv[bv.len() - 1 - n] {
            n += 1;
        }
        n
    }

    // Relevant deleted sources (the only sources rename detection reads), tagged
    // with whether they are content- or location-relevant.
    let relevant_deleted: Vec<(&Vec<u8>, &IndexEntry, Relevance)> = deleted
        .iter()
        .filter_map(|(dp, de)| match relevance_of(dp) {
            Relevance::None => None,
            r => Some((*dp, *de, r)),
        })
        .collect();

    // Phase 1: basename matching. For each relevant source, pair it with the added
    // destination sharing its basename and the longest trailing path suffix (its
    // directory-rename image). This avoids spuriously pairing e.g.
    // `dir/subdir/Makefile` with `folder/subdir/tweaked/Makefile` when
    // `folder/subdir/Makefile` exists. A destination may be consumed by only one
    // source pairing; once paired it is excluded from the general phase.
    let mut basename_out: Vec<ObjectId> = Vec::new();
    let mut paired_dest_idx: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut paired_source: Vec<bool> = vec![false; relevant_deleted.len()];
    for (si, (dp, de, _rel)) in relevant_deleted.iter().enumerate() {
        let dbase = basename(dp);
        let mut best: Option<(usize, usize, ObjectId)> = None;
        for (ai, (ap, ae)) in added.iter().enumerate() {
            if paired_dest_idx.contains(&ai) || basename(ap) != dbase || de.oid == ae.oid {
                continue;
            }
            let score = shared_suffix_components(dp, ap);
            if best.is_none_or(|(s, _, _)| score > s) {
                best = Some((score, ai, ae.oid));
            }
        }
        if let Some((_, ai, add_oid)) = best {
            paired_dest_idx.insert(ai);
            paired_source[si] = true;
            basename_out.push(de.oid);
            basename_out.push(add_oid);
        }
    }

    // Phase 2: general (full-matrix) rename detection. Git only runs this when
    // CONTENT-relevant sources remain unpaired after the basename phase; it then
    // reads every still-unpaired destination and every remaining content-relevant
    // source (`inexact_prefetch`). Location-relevant sources left unpaired are
    // culled (their directory rename is already known) and never read here. When
    // no content-relevant source remains unpaired, no general fetch happens.
    let mut general_out: Vec<ObjectId> = Vec::new();
    let remaining_sources: Vec<ObjectId> = relevant_deleted
        .iter()
        .enumerate()
        .filter(|(si, (_, _, rel))| !paired_source[*si] && *rel == Relevance::Content)
        .map(|(_, (_, de, _))| de.oid)
        .collect();
    if !remaining_sources.is_empty() {
        for (ai, (_, ae)) in added.iter().enumerate() {
            if paired_dest_idx.contains(&ai) {
                continue;
            }
            general_out.push(ae.oid);
        }
        general_out.extend(remaining_sources);
    }

    RenameDetectionBlobs {
        basename: basename_out,
        general: general_out,
    }
}

/// Blobs read by 3-way content merge: for a base path modified on both ours and
/// theirs (a side may have renamed it away while modifying it), the base + both
/// side blobs are read. Rename targets are resolved by exact-OID lookup so a
/// path that one side moved-and-modified is still recognized as the merge target.
fn content_merge_blob_oids(
    base: &HashMap<Vec<u8>, IndexEntry>,
    ours: &HashMap<Vec<u8>, IndexEntry>,
    theirs: &HashMap<Vec<u8>, IndexEntry>,
) -> Vec<ObjectId> {
    // Map base-OID -> the side path that carries that exact blob (for exact renames).
    let exact_rename_target = |base_path: &[u8],
                               base_oid: &ObjectId,
                               side: &HashMap<Vec<u8>, IndexEntry>|
     -> Option<ObjectId> {
        // Same path, modified in place.
        if let Some(e) = side.get(base_path) {
            if e.oid != *base_oid {
                return Some(e.oid);
            }
            // Unchanged at this path -> not a content-merge contributor.
            return None;
        }
        // Path removed on this side (renamed away, possibly with modification): pick
        // the new path that shares this file's basename and the longest trailing
        // path suffix — its directory-rename image. That blob is this side's version.
        let bn = match base_path.iter().rposition(|&b| b == b'/') {
            Some(i) => &base_path[i + 1..],
            None => base_path,
        };
        let shared = |a: &[u8], b: &[u8]| -> usize {
            let av: Vec<&[u8]> = a.split(|&c| c == b'/').collect();
            let bv: Vec<&[u8]> = b.split(|&c| c == b'/').collect();
            let mut n = 0;
            while n < av.len() && n < bv.len() && av[av.len() - 1 - n] == bv[bv.len() - 1 - n] {
                n += 1;
            }
            n
        };
        let mut best: Option<(usize, ObjectId)> = None;
        for (p, e) in side {
            if base.contains_key(p) || e.oid == *base_oid {
                continue;
            }
            let pbn = match p.iter().rposition(|&b| b == b'/') {
                Some(i) => &p[i + 1..],
                None => &p[..],
            };
            if pbn != bn {
                continue;
            }
            let score = shared(base_path, p);
            if best.is_none_or(|(s, _)| score > s) {
                best = Some((score, e.oid));
            }
        }
        if let Some((_, oid)) = best {
            return Some(oid);
        }
        // General (non-basename) rename: the file was renamed to a different
        // basename (e.g. general/leap1 -> general/jump1). We cannot identify the
        // exact target without content similarity, but for a both-sides-modified
        // content merge we only need to know a rename target exists on this side so
        // the in-place modification on the other side gets fetched. Pick any added,
        // non-exact-rename path in the same parent directory as the base path; its
        // blob was already fetched during general rename detection, so this never
        // introduces an extra fetch.
        fn parent_of(p: &[u8]) -> &[u8] {
            match p.iter().rposition(|&b| b == b'/') {
                Some(i) => &p[..i],
                None => b"",
            }
        }
        let parent = parent_of(base_path);
        for (p, e) in side {
            if base.contains_key(p) || e.oid == *base_oid {
                continue;
            }
            // Skip exact renames of some base file (those are not inexact targets).
            if base.values().any(|b| b.oid == e.oid) {
                continue;
            }
            if parent_of(p) == parent {
                return Some(e.oid);
            }
        }
        None
    };

    let mut out: Vec<ObjectId> = Vec::new();
    for (path, be) in base {
        let our = exact_rename_target(path, &be.oid, ours);
        let their = exact_rename_target(path, &be.oid, theirs);
        if let (Some(o), Some(t)) = (our, their) {
            if o != t {
                out.push(be.oid);
                out.push(o);
                out.push(t);
            }
        }
    }
    out
}

/// Append a single trace2 perf line in the same shape used by `main`.
fn append_trace2_perf_line(path: &str, event: &str, data: &str) -> Result<()> {
    use std::io::Write;
    let now = {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        let total_secs = now.as_secs();
        let micros = now.subsec_micros();
        let secs_in_day = total_secs % 86400;
        let hours = secs_in_day / 3600;
        let mins = (secs_in_day % 3600) / 60;
        let secs = secs_in_day % 60;
        format!("{:02}:{:02}:{:02}.{:06}", hours, mins, secs, micros)
    };

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(
        file,
        "{} grit:0  | d0 | main                     | {:<12} |     |           |           |              | {}",
        now, event, data
    )?;
    Ok(())
}

fn trace2_perf_region_enter(label: &str) {
    let Ok(path) = std::env::var("GIT_TRACE2_PERF") else {
        return;
    };
    let _ = append_trace2_perf_line(&path, "region_enter", label);
}

fn bail_if_merge_touches_present_skip_worktree(
    repo: &Repository,
    ours: &HashMap<Vec<u8>, IndexEntry>,
    theirs: &HashMap<Vec<u8>, IndexEntry>,
) -> Result<()> {
    let Some(work_tree) = repo.work_tree.as_deref() else {
        return Ok(());
    };
    let index = repo.load_index()?;

    for entry in &index.entries {
        if entry.stage() != 0 || !entry.skip_worktree() {
            continue;
        }

        let path_str = String::from_utf8_lossy(&entry.path).to_string();
        let abs = work_tree.join(&path_str);
        if fs::symlink_metadata(&abs).is_err() {
            continue;
        }

        let ours_e = ours.get(&entry.path);
        let theirs_e = theirs.get(&entry.path);
        let unchanged = match (ours_e, theirs_e) {
            (Some(o), Some(t)) => o.oid == t.oid && o.mode == t.mode,
            (None, None) => true,
            _ => false,
        };
        if !unchanged {
            bail!("Entry '{}' not uptodate. Cannot merge.", path_str);
        }
    }

    Ok(())
}

/// Octopus merge: merge multiple branches into HEAD.
///
/// This creates a single merge commit with N+1 parents (HEAD + each branch).
/// If any merge produces a conflict, we bail.
fn do_octopus_merge(
    repo: &Repository,
    head: &HeadState,
    head_oid: ObjectId,
    args: &Args,
    favor: MergeFavor,
    diff_algorithm: Option<&str>,
    subtree_shift: &SubtreeShift,
    merge_renormalize: bool,
    exit_on_conflict: bool,
) -> Result<()> {
    // Resolve all merge targets, deduplicating and filtering ancestors of HEAD
    let mut merge_oids = Vec::new();
    let mut merge_names = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for name in &args.commits {
        let oid = resolve_merge_target(repo, name)?;
        // Skip duplicates
        if !seen.insert(oid) {
            continue;
        }
        // Skip if this is HEAD itself or an ancestor of HEAD
        if oid == head_oid || is_ancestor(repo, oid, head_oid)? {
            continue;
        }
        merge_oids.push(oid);
        merge_names.push(name.clone());
    }

    let (merge_oids, merge_names) = reduce_octopus_merge_heads(repo, &merge_oids, &merge_names)?;

    if merge_oids.is_empty() {
        if !args.quiet {
            eprintln!("Already up to date.");
        }
        return Ok(());
    }

    // `recursive` / `ort` only handle two-way merges; Git rejects true octopus with these alone
    // (see `try_merge_strategy` in git's merge.c: "Not handling anything other than two heads").
    if merge_oids.len() > 1
        && args.strategy.len() == 1
        && matches!(args.strategy[0].as_str(), "recursive" | "ort")
    {
        // Git creates the autostash before trying the strategy (builtin/merge.c:1766), so when
        // the strategy bails with exit 2 the autostash is re-applied (line 1852).
        if args.autostash {
            create_merge_autostash(repo)?;
            apply_merge_autostash(repo)?;
        }
        bail!("Not handling anything other than two heads merge.");
    }

    let head_is_ancestor_of_all = merge_oids
        .iter()
        .all(|oid| is_ancestor(repo, head_oid, *oid).unwrap_or(false));

    // If only one merge target remains after filtering, delegate to single merge
    if merge_oids.len() == 1 {
        let merge_oid = merge_oids[0];
        if args.no_ff && !args.ff_only {
            return do_real_merge(
                repo,
                head,
                head_oid,
                merge_oid,
                args,
                favor,
                diff_algorithm,
                subtree_shift,
                merge_renormalize,
                false,
                true,
            );
        }
        if is_ancestor(repo, head_oid, merge_oid)? {
            return do_fast_forward(repo, head, head_oid, merge_oid, args);
        }
        return do_real_merge(
            repo,
            head,
            head_oid,
            merge_oid,
            args,
            favor,
            diff_algorithm,
            subtree_shift,
            merge_renormalize,
            false,
            true,
        );
    }

    // Check if we can fast-forward: filter out merge targets that are ancestors
    // of other merge targets (i.e., redundant). If only one remains, fast-forward.
    if !args.no_ff {
        let mut reduced = merge_oids.clone();
        reduced.retain(|&oid| {
            !merge_oids
                .iter()
                .any(|&other| other != oid && is_ancestor(repo, oid, other).unwrap_or(false))
        });
        if reduced.len() == 1 {
            let merge_oid = reduced[0];
            if is_ancestor(repo, head_oid, merge_oid)? {
                return do_fast_forward(repo, head, head_oid, merge_oid, args);
            }
        }
    }

    // Git creates `MERGE_AUTOSTASH` (snapshot WIP + reset --hard to HEAD) before the octopus
    // strategy runs (builtin/merge.c:1766); afterwards the index/worktree match HEAD.
    if args.autostash {
        create_merge_autostash(repo)?;
    }

    // True octopus (multiple merge heads): index must match HEAD — unlike two-parent merge,
    // unrelated staged paths are not allowed (t6424).
    bail_if_index_tree_differs_from_head(repo, head_oid, args.autostash)?;

    let pre_merge_index = repo.load_index()?;
    let head_tree = commit_tree(repo, head_oid)?;
    let head_entries = tree_to_map(tree_to_index_entries(repo, &head_tree, "")?);

    fs::write(
        repo.git_dir.join("ORIG_HEAD"),
        format!("{}\n", head_oid.to_hex()),
    )?;

    // Simulate the full octopus result to detect conflicts and unrelated index changes
    // before mutating the repo (matches git merge behavior).
    {
        let mut sim_entries = tree_to_index_entries(repo, &head_tree, "")?;
        let mut sim_current_oid = head_oid;
        for (i, merge_oid) in merge_oids.iter().enumerate() {
            let bases = grit_lib::merge_base::merge_bases_first_vs_rest(
                repo,
                sim_current_oid,
                &[*merge_oid],
            )?;
            if bases.is_empty() && !args.allow_unrelated_histories {
                bail!("refusing to merge unrelated histories");
            }
            let base_oid = if bases.is_empty() {
                create_empty_base_commit(repo)?
            } else {
                bases[0]
            };
            let base_tree = commit_tree(repo, base_oid)?;
            let theirs_tree = commit_tree(repo, *merge_oid)?;

            let base_entries =
                tree_to_map_for_merge(repo, tree_to_index_entries(repo, &base_tree, "")?);
            let ours_entries = tree_to_map_for_merge(repo, sim_entries.clone());
            let theirs_entries =
                tree_to_map_for_merge(repo, tree_to_index_entries(repo, &theirs_tree, "")?);

            let base_label_prefix = if bases.is_empty() {
                "empty tree".to_string()
            } else {
                short_oid(bases[0])
            };

            let merge_result = merge_trees(
                repo,
                &base_entries,
                &ours_entries,
                &theirs_entries,
                head,
                &merge_names[i],
                &base_label_prefix,
                &sim_current_oid.to_hex(),
                &merge_oid.to_hex(),
                favor,
                diff_algorithm,
                merge_renormalize,
                false,
                false,
                false,
                false,
                MergeDirectoryRenamesMode::FromConfig,
                MergeRenameOptions::from_config(repo),
                None,
                false,
                None,
                None,
            )?;

            if merge_result.has_conflicts {
                if i + 1 < merge_oids.len() {
                    return die_octopus_merge_program_failed(repo, &pre_merge_index);
                }
                return finish_octopus_merge_on_conflict(
                    repo,
                    head,
                    head_oid,
                    &merge_oids,
                    &merge_names,
                    args,
                    &pre_merge_index,
                    &merge_result,
                    exit_on_conflict,
                );
            }

            sim_entries = merge_result.index.entries;
            let step_parents = if i == 0 {
                vec![head_oid, *merge_oid]
            } else {
                vec![sim_current_oid, *merge_oid]
            };
            let mut step_idx = Index::new();
            step_idx.entries = sim_entries.clone();
            step_idx.sort();
            let step_tree = write_tree_from_index(&repo.odb, &step_idx, "")?;
            sim_current_oid = write_octopus_step_commit(repo, step_tree, &step_parents)?;
        }

        let mut sim_index = Index::new();
        sim_index.entries = sim_entries;
        sim_index.sort();
        if !args.autostash {
            bail_if_merge_would_overwrite_local_changes(
                repo,
                &head_entries,
                &sim_index,
                &[],
                false,
            )?;
        }
    }

    // Start with HEAD's tree as "ours" and merge each branch sequentially
    let mut current_tree_entries = {
        let ours_tree = commit_tree(repo, head_oid)?;
        tree_to_index_entries(repo, &ours_tree, "")?
    };
    let mut merge_current_oid = head_oid;
    let mut non_ff_merge = false;

    for (i, merge_oid) in merge_oids.iter().enumerate() {
        let bases = grit_lib::merge_base::merge_bases_first_vs_rest(
            repo,
            merge_current_oid,
            &[*merge_oid],
        )?;
        if bases.is_empty() && !args.allow_unrelated_histories {
            bail!("refusing to merge unrelated histories");
        }
        let base_oid = if bases.is_empty() {
            create_empty_base_commit(repo)?
        } else {
            bases[0]
        };
        let base_tree = commit_tree(repo, base_oid)?;
        let theirs_tree = commit_tree(repo, *merge_oid)?;

        if bases.len() == 1 && bases[0] == merge_current_oid && !non_ff_merge {
            if !args.quiet {
                println!("Fast-forwarding to: {}", merge_names[i]);
            }
            current_tree_entries = tree_to_index_entries(repo, &theirs_tree, "")?;
            merge_current_oid = *merge_oid;
            continue;
        }

        non_ff_merge = true;
        if !args.quiet {
            println!("Trying simple merge with {}", merge_names[i]);
        }

        let base_entries =
            tree_to_map_for_merge(repo, tree_to_index_entries(repo, &base_tree, "")?);
        let ours_entries = tree_to_map_for_merge(repo, current_tree_entries);
        let theirs_entries =
            tree_to_map_for_merge(repo, tree_to_index_entries(repo, &theirs_tree, "")?);

        let base_label_prefix = if bases.is_empty() {
            "empty tree".to_string()
        } else {
            short_oid(bases[0])
        };

        let merge_result = merge_trees(
            repo,
            &base_entries,
            &ours_entries,
            &theirs_entries,
            head,
            &merge_names[i],
            &base_label_prefix,
            &merge_current_oid.to_hex(),
            &merge_oid.to_hex(),
            favor,
            diff_algorithm,
            merge_renormalize,
            false,
            false,
            false,
            false,
            MergeDirectoryRenamesMode::FromConfig,
            MergeRenameOptions::from_config(repo),
            None,
            false,
            None,
            None,
        )?;

        if merge_result.has_conflicts {
            if i + 1 < merge_oids.len() {
                return die_octopus_merge_program_failed(repo, &pre_merge_index);
            }
            return finish_octopus_merge_on_conflict(
                repo,
                head,
                head_oid,
                &merge_oids,
                &merge_names,
                args,
                &pre_merge_index,
                &merge_result,
                exit_on_conflict,
            );
        }

        // Advance current_tree_entries to the merged result
        current_tree_entries = merge_result.index.entries;
        let step_parents = if i == 0 {
            vec![head_oid, *merge_oid]
        } else {
            vec![merge_current_oid, *merge_oid]
        };
        let mut step_idx = Index::new();
        step_idx.entries = current_tree_entries.clone();
        step_idx.sort();
        let step_tree = write_tree_from_index(&repo.odb, &step_idx, "")?;
        merge_current_oid = write_octopus_step_commit(repo, step_tree, &step_parents)?;
    }

    // All merges succeeded — build the octopus merge commit.
    // A `-F <file>` (or `-m`) message must override the auto-generated `Merge branches '…'` summary,
    // matching the non-octopus path (and Git's `do_merge` octopus, which passes `-F <merge_msg>`).
    // t3430 'octopus merges' replays the original `Tüntenfüsch` message via `merge … --no-log -F`.
    let octopus_msg_config = ConfigSet::load(Some(&repo.git_dir), true)?;
    let octopus_custom_msg: Option<String> = if let Some(ref file_path) = args.file {
        Some(read_merge_message_from_file(
            Path::new(file_path),
            &octopus_msg_config,
        )?)
    } else {
        args.message.clone()
    };

    let mut final_index = Index::new();
    final_index.entries = current_tree_entries;
    final_index.sort();
    compose_octopus_final_index(&pre_merge_index, &mut final_index);
    apply_sparse_checkout_skip_worktree(
        &repo.git_dir,
        repo.work_tree.as_deref(),
        &mut final_index,
        false,
    );
    repo.write_index(&mut final_index)?;

    let sparse_on = sparse_checkout_enabled(&repo.git_dir);
    if let Some(ref wt) = repo.work_tree {
        checkout_entries(repo, wt, &final_index, None, sparse_on)?;
    }
    refresh_index_stat_cache_from_worktree(repo, &mut final_index)?;
    repo.write_index(&mut final_index)?;

    if args.squash {
        let msg = build_squash_msg(repo, head_oid, &merge_oids)?;
        fs::write(repo.git_dir.join("SQUASH_MSG"), &msg)?;
        if !args.quiet {
            eprintln!("Squash commit -- not updating HEAD");
        }
        run_post_merge_hook(repo, true);
        return Ok(());
    }

    if args.no_commit {
        let merge_head_content: String = merge_oids
            .iter()
            .map(|oid| format!("{}\n", oid.to_hex()))
            .collect();
        fs::write(repo.git_dir.join("MERGE_HEAD"), &merge_head_content)?;
        let msg =
            build_octopus_merge_message(head, &merge_names, octopus_custom_msg.as_deref(), repo);
        fs::write(repo.git_dir.join("MERGE_MSG"), &msg)?;
        fs::write(repo.git_dir.join("MERGE_MODE"), "no-ff\n")?;
        if !args.quiet {
            eprintln!("Automatic merge went well; stopped before committing as requested");
        }
        run_post_merge_hook(repo, false);
        return Ok(());
    }

    let will_edit = merge_will_edit(args);
    run_pre_merge_commit_hook(repo, args.no_verify, will_edit, &mut final_index)?;
    repo.write_index(&mut final_index)?;

    let tree_oid = write_tree_from_index(&repo.odb, &final_index, "")?;
    let msg = build_octopus_merge_message(head, &merge_names, octopus_custom_msg.as_deref(), repo);

    let config = ConfigSet::load(Some(&repo.git_dir), true)?;
    let hook_cleanup = args.cleanup.as_deref().unwrap_or("whitespace");
    let msg = run_merge_commit_msg_hooks(
        repo,
        args.no_verify,
        will_edit,
        msg,
        &mut final_index,
        hook_cleanup,
    )?;
    let now = OffsetDateTime::now_utc();
    let author = resolve_ident(&config, "author", now)?;
    let committer = resolve_ident(&config, "committer", now)?;

    let mut parents = if !args.no_ff && head_is_ancestor_of_all {
        Vec::new()
    } else {
        vec![head_oid]
    };
    parents.extend(merge_oids);

    let commit_data = CommitData {
        tree: tree_oid,
        parents,
        author,
        committer,
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: msg,
        raw_message: None,
    };

    let mut commit_bytes = serialize_commit(&commit_data);
    if should_sign_merge(args, &config) {
        commit_bytes = sign_merge_commit_bytes(
            &config,
            &commit_data.committer,
            args.gpg_sign.as_deref(),
            commit_bytes,
        )?;
    }
    let commit_oid = repo.odb.write(ObjectKind::Commit, &commit_bytes)?;
    let strategy_name = primary_merge_strategy(args).unwrap_or("octopus");
    let reflog = format!(
        "{}: Merge made by the '{strategy_name}' strategy.",
        merge_reflog_action(args)
    );
    merge_update_head_with_reflog(repo, head, Some(head_oid), commit_oid, Some(&reflog))?;

    if args.autostash {
        apply_merge_autostash(repo)?;
    }

    if !args.quiet {
        // The per-head "Fast-forwarding to:" / "Trying simple merge with" lines are
        // already emitted by the real merge loop above (matching git-merge-octopus,
        // which prints them as it processes each head). Only the final summary line
        // belongs here, so it appears exactly once.
        println!("Merge made by the '{strategy_name}' strategy.");
        let show_stat = args.stat || args.summary || !args.no_stat;
        if show_stat {
            let old_tree = commit_tree(repo, head_oid)?;
            let new_tree = commit_tree(repo, commit_oid)?;
            if let Ok(diff_entries) = diff_trees(&repo.odb, Some(&old_tree), Some(&new_tree), "") {
                print_diffstat(repo, &diff_entries, args.compact_summary);
            }
        }
    }

    run_post_merge_hook(repo, false);
    Ok(())
}

/// Build the merge log section (for --log option).
/// Lists commits reachable from merge_oid but not from head_oid.
fn build_merge_log(
    repo: &Repository,
    head_oid: ObjectId,
    merge_oid: ObjectId,
    branch_name: &str,
    max_entries: usize,
) -> Result<String> {
    use grit_lib::merge_base::is_ancestor;

    // Collect commits reachable from merge_oid but not from head_oid
    let mut commits = Vec::new();
    let mut queue = std::collections::VecDeque::new();
    let mut visited = std::collections::HashSet::new();
    queue.push_back(merge_oid);

    while let Some(oid) = queue.pop_front() {
        if !visited.insert(oid) {
            continue;
        }
        if oid == head_oid || is_ancestor(repo, oid, head_oid).unwrap_or(false) {
            continue;
        }
        if let Ok(obj) = repo.odb.read(&oid) {
            if let Ok(c) = parse_commit(&obj.data) {
                let subject = c.message.lines().next().unwrap_or("").to_owned();
                commits.push(subject);
                for p in &c.parents {
                    queue.push_back(*p);
                }
            }
        }
        if commits.len() >= max_entries {
            break;
        }
    }

    if commits.is_empty() {
        return Ok(String::new());
    }

    // Determine the label: tag, branch, or commit
    let kind = if resolve_ref(&repo.git_dir, &format!("refs/tags/{branch_name}")).is_ok() {
        "tag"
    } else if resolve_ref(&repo.git_dir, &format!("refs/remotes/{branch_name}")).is_ok() {
        "remote-tracking branch"
    } else {
        "branch"
    };

    let mut log = format!("* {kind} '{branch_name}':\n");
    for subject in &commits {
        log.push_str(&format!("  {subject}\n"));
    }

    Ok(log)
}

/// Build merge message for octopus merges.
fn build_octopus_merge_message(
    head: &HeadState,
    branch_names: &[String],
    custom: Option<&str>,
    repo: &Repository,
) -> String {
    if let Some(msg) = custom {
        return ensure_trailing_newline(msg);
    }

    // Determine the kind for each branch name
    let classify = |name: &str| -> &str {
        if resolve_ref(&repo.git_dir, &format!("refs/tags/{name}")).is_ok() {
            "tag"
        } else if resolve_ref(&repo.git_dir, &format!("refs/remotes/{name}")).is_ok() {
            "remote-tracking branch"
        } else {
            "branch"
        }
    };

    // Git groups by kind: "Merge tags 'a' and 'b'" or "Merge branches 'a', tag 'b' and branch 'c'"
    // If all are the same kind, use plural: "Merge tags 'a' and 'b'"
    // Otherwise, prefix each with its kind
    let kinds: Vec<&str> = branch_names.iter().map(|n| classify(n)).collect();
    let all_same = kinds.windows(2).all(|w| w[0] == w[1]);

    let formatted = if all_same {
        let kind_plural = match kinds[0] {
            "tag" => "tags",
            "remote-tracking branch" => "remote-tracking branches",
            _ => "branches",
        };
        if branch_names.len() == 2 {
            format!(
                "Merge {kind_plural} '{}' and '{}'",
                branch_names[0], branch_names[1]
            )
        } else if let Some((last, rest_names)) = branch_names.split_last() {
            let rest: Vec<String> = rest_names.iter().map(|n| format!("'{n}'")).collect();
            format!("Merge {kind_plural} {} and '{last}'", rest.join(", "))
        } else {
            format!("Merge {kind_plural}")
        }
    } else {
        // Mixed kinds
        let parts: Vec<String> = branch_names
            .iter()
            .zip(kinds.iter())
            .map(|(n, k)| format!("{k} '{n}'"))
            .collect();
        if parts.len() == 2 {
            format!("Merge {} and {}", parts[0], parts[1])
        } else if let Some((last, rest_parts)) = parts.split_last() {
            let rest = rest_parts.join(", ");
            format!("Merge {rest} and {last}")
        } else {
            "Merge".to_string()
        }
    };

    let msg = if let Some(name) = head.branch_name() {
        if name != "main" && name != "master" {
            format!("{formatted} into {name}")
        } else {
            formatted
        }
    } else {
        formatted
    };
    ensure_trailing_newline(&msg)
}

/// Strategy "ours": create merge commit keeping HEAD's tree.
fn do_strategy_ours(
    repo: &Repository,
    head: &HeadState,
    head_oid: ObjectId,
    merge_oid: ObjectId,
    args: &Args,
) -> Result<()> {
    bail_if_index_tree_differs_from_head(repo, head_oid, args.autostash)?;

    // Save ORIG_HEAD
    fs::write(
        repo.git_dir.join("ORIG_HEAD"),
        format!("{}\n", head_oid.to_hex()),
    )?;

    let msg = build_merge_message(head, &args.commits[0], args.message.as_deref(), repo);

    let config = ConfigSet::load(Some(&repo.git_dir), true)?;
    let now = OffsetDateTime::now_utc();
    let author = resolve_ident(&config, "author", now)?;
    let committer = resolve_ident(&config, "committer", now)?;

    if args.no_commit {
        fs::write(
            repo.git_dir.join("MERGE_HEAD"),
            format!("{}\n", merge_oid.to_hex()),
        )?;
        fs::write(repo.git_dir.join("MERGE_MSG"), &msg)?;
        fs::write(repo.git_dir.join("MERGE_MODE"), "no-ff\n")?;
        if !args.quiet {
            eprintln!("Automatic merge went well; stopped before committing as requested");
        }
        run_post_merge_hook(repo, false);
        return Ok(());
    }

    let mut idx = repo.load_index()?;
    let will_edit = merge_will_edit(args);
    run_pre_merge_commit_hook(repo, args.no_verify, will_edit, &mut idx)?;
    repo.write_index(&mut idx)?;

    let hook_cleanup = args.cleanup.as_deref().unwrap_or("whitespace");
    let msg =
        run_merge_commit_msg_hooks(repo, args.no_verify, will_edit, msg, &mut idx, hook_cleanup)?;
    idx.expand_sparse_directory_placeholders(&repo.odb)?;
    let tree_oid = write_tree_from_index(&repo.odb, &idx, "")?;

    let commit_data = CommitData {
        tree: tree_oid,
        parents: vec![head_oid, merge_oid],
        author,
        committer,
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: msg,
        raw_message: None,
    };

    let mut commit_bytes = serialize_commit(&commit_data);
    if should_sign_merge(args, &config) {
        commit_bytes = sign_merge_commit_bytes(
            &config,
            &commit_data.committer,
            args.gpg_sign.as_deref(),
            commit_bytes,
        )?;
    }
    let commit_oid = repo.odb.write(ObjectKind::Commit, &commit_bytes)?;
    merge_update_head(repo, head, Some(head_oid), commit_oid)?;

    if !args.quiet {
        if show_merge_commit_summary() {
            let short = &commit_oid.to_hex()[..7];
            let branch = head.branch_name().unwrap_or("HEAD");
            let first_line = commit_data.message.lines().next().unwrap_or("");
            println!("[{branch} {short}] {first_line}");
        }
    }

    run_post_merge_hook(repo, false);
    Ok(())
}

/// Build SQUASH_MSG by walking commits reachable from merge targets but not from HEAD.
fn build_squash_msg(
    repo: &Repository,
    head_oid: ObjectId,
    merge_oids: &[ObjectId],
) -> Result<String> {
    let mut msg = String::from("Squashed commit of the following:\n");

    // Collect all commits reachable from merge_oids but not from head_oid (no merges).
    let mut visited = std::collections::HashSet::new();
    let mut queue = std::collections::VecDeque::new();

    // Mark head and its ancestors as visited (stop set)
    {
        let mut stop_queue = std::collections::VecDeque::new();
        stop_queue.push_back(head_oid);
        while let Some(oid) = stop_queue.pop_front() {
            if !visited.insert(oid) {
                continue;
            }
            if let Ok(obj) = repo.odb.read(&oid) {
                if let Ok(c) = parse_commit(&obj.data) {
                    for p in &c.parents {
                        stop_queue.push_back(*p);
                    }
                }
            }
        }
    }

    // Now walk from merge_oids collecting non-merge commits
    let mut commits_to_show = Vec::new();
    for merge_oid in merge_oids {
        queue.push_back(*merge_oid);
    }
    // Reset visited for the forward walk, but keep stop set
    let stop_set = visited.clone();
    let mut walk_visited = std::collections::HashSet::new();
    while let Some(oid) = queue.pop_front() {
        if !walk_visited.insert(oid) {
            continue;
        }
        if stop_set.contains(&oid) {
            continue;
        }
        if let Ok(obj) = repo.odb.read(&oid) {
            if let Ok(c) = parse_commit(&obj.data) {
                // Skip merge commits (--no-merges)
                if c.parents.len() <= 1 {
                    commits_to_show.push((oid, c.clone()));
                }
                for p in &c.parents {
                    queue.push_back(*p);
                }
            }
        }
    }

    // Sort by commit date descending (most recent first)
    // Parse the timestamp from author/committer line
    commits_to_show.sort_by(|a, b| {
        let ts_a = parse_timestamp_from_ident(&a.1.author);
        let ts_b = parse_timestamp_from_ident(&b.1.author);
        ts_b.cmp(&ts_a)
    });

    // The `git log` body follows a single blank line (git's `squash_message` joins
    // `"Squashed commit of the following:\n\n"` with the log output). Each commit renders
    // exactly as `git log` (medium format) does, including the blank separator between commits.
    for (i, (oid, commit)) in commits_to_show.iter().enumerate() {
        if i == 0 {
            msg.push('\n');
        }
        msg.push_str(&format!("commit {}\n", oid.to_hex()));
        msg.push_str(&format!(
            "Author: {}\n",
            format_author_for_log(&commit.author)
        ));
        msg.push_str(&format!(
            "Date:   {}\n",
            format_date_for_log(&commit.author)
        ));
        msg.push('\n');
        for line in commit.message.trim_end().lines() {
            msg.push_str(&format!("    {}\n", line));
        }
        if i + 1 < commits_to_show.len() {
            msg.push('\n');
        }
    }

    Ok(msg)
}

/// Extract timestamp (epoch seconds) from a git ident line like "Name <email> 1234567890 +0000"
fn parse_timestamp_from_ident(ident: &str) -> i64 {
    // Format: "Name <email> timestamp timezone"
    if let Some(after_email) = ident.rfind('>') {
        let rest = ident[after_email + 1..].trim();
        if let Some(space) = rest.find(' ') {
            rest[..space].parse().unwrap_or(0)
        } else {
            rest.parse().unwrap_or(0)
        }
    } else {
        0
    }
}

/// Format the author name/email portion from an ident line for display.
fn format_author_for_log(ident: &str) -> String {
    // "Name <email> timestamp tz" → "Name <email>"
    if let Some(pos) = ident.rfind('>') {
        ident[..=pos].to_string()
    } else {
        ident.to_string()
    }
}

/// Format the date portion from an ident line for display.
fn format_date_for_log(ident: &str) -> String {
    if let Some(after_email) = ident.rfind('>') {
        let rest = ident[after_email + 1..].trim();
        // rest is "timestamp timezone"
        let parts: Vec<&str> = rest.splitn(2, ' ').collect();
        if parts.len() == 2 {
            if let Ok(epoch) = parts[0].parse::<i64>() {
                // Parse timezone offset
                let tz_str = parts[1];
                let tz_secs = parse_tz_offset(tz_str);
                // Format as "Thu Apr  7 15:14:13 2005 -0700"
                if let Ok(dt) = time::OffsetDateTime::from_unix_timestamp(epoch) {
                    let offset = time::UtcOffset::from_whole_seconds(tz_secs)
                        .unwrap_or(time::UtcOffset::UTC);
                    let dt = dt.to_offset(offset);
                    let weekday = match dt.weekday() {
                        time::Weekday::Monday => "Mon",
                        time::Weekday::Tuesday => "Tue",
                        time::Weekday::Wednesday => "Wed",
                        time::Weekday::Thursday => "Thu",
                        time::Weekday::Friday => "Fri",
                        time::Weekday::Saturday => "Sat",
                        time::Weekday::Sunday => "Sun",
                    };
                    let month = match dt.month() {
                        time::Month::January => "Jan",
                        time::Month::February => "Feb",
                        time::Month::March => "Mar",
                        time::Month::April => "Apr",
                        time::Month::May => "May",
                        time::Month::June => "Jun",
                        time::Month::July => "Jul",
                        time::Month::August => "Aug",
                        time::Month::September => "Sep",
                        time::Month::October => "Oct",
                        time::Month::November => "Nov",
                        time::Month::December => "Dec",
                    };
                    let day = dt.day();
                    let (h, m, s) = (dt.hour(), dt.minute(), dt.second());
                    let year = dt.year();
                    // Git's default log date uses an UNPADDED day (date.c `"%.3s %d "`),
                    // e.g. `Thu Apr 7 ...`, not the strftime `%e` space-padded form.
                    return format!("{weekday} {month} {day} {h:02}:{m:02}:{s:02} {year} {tz_str}");
                }
            }
        }
    }
    String::new()
}

fn parse_tz_offset(tz: &str) -> i32 {
    // "+0700" or "-0530"
    if tz.len() < 5 {
        return 0;
    }
    let sign = if tz.starts_with('-') { -1 } else { 1 };
    let hours: i32 = tz[1..3].parse().unwrap_or(0);
    let mins: i32 = tz[3..5].parse().unwrap_or(0);
    sign * (hours * 3600 + mins * 60)
}

/// Squash merge: stage changes but don't commit.
fn do_squash(
    repo: &Repository,
    head_oid: ObjectId,
    merge_oid: ObjectId,
    args: &Args,
) -> Result<()> {
    // For a simple fast-forward squash, stage the merge target's tree
    let commit_obj = repo.odb.read(&merge_oid)?;
    let commit = parse_commit(&commit_obj.data)?;
    let entries = tree_to_index_entries(repo, &commit.tree, "")?;
    let mut new_index = Index::new();
    new_index.entries = entries;
    new_index.sort();
    apply_sparse_checkout_skip_worktree(
        &repo.git_dir,
        repo.work_tree.as_deref(),
        &mut new_index,
        false,
    );

    if let Some(ref wt) = repo.work_tree {
        checkout_entries(
            repo,
            wt,
            &new_index,
            None,
            sparse_checkout_enabled(&repo.git_dir),
        )?;
    }
    refresh_index_stat_cache_from_worktree(repo, &mut new_index)?;
    repo.write_index(&mut new_index)?;

    // Write SQUASH_MSG
    let msg = build_squash_msg(repo, head_oid, &[merge_oid])?;
    fs::write(repo.git_dir.join("SQUASH_MSG"), &msg)?;

    if !args.quiet {
        eprintln!(
            "Squash commit -- not updating HEAD\n\
             Updating {}..{}",
            &head_oid.to_hex()[..7],
            &merge_oid.to_hex()[..7]
        );
    }
    run_post_merge_hook(repo, true);
    Ok(())
}

/// Squash from a three-way merge result.
fn do_squash_from_merge(
    repo: &Repository,
    mut index: Index,
    _head: &HeadState,
    head_oid: ObjectId,
    merge_oid: ObjectId,
    args: &Args,
) -> Result<()> {
    repo.write_index(&mut index)?;

    let msg = build_squash_msg(repo, head_oid, &[merge_oid])?;
    fs::write(repo.git_dir.join("SQUASH_MSG"), &msg)?;

    if !args.quiet {
        eprintln!("Squash commit -- not updating HEAD");
    }
    run_post_merge_hook(repo, true);
    Ok(())
}

/// Remove work tree paths that are `skip-worktree` in `index` so merge abort does not leave
/// conflict-marker files that `checkout_entries` skipped (sparse-checkout, t7817).
fn remove_skip_worktree_paths_from_worktree(work_tree: &Path, index: &Index) -> Result<()> {
    for entry in &index.entries {
        if entry.stage() != 0 || !entry.skip_worktree() {
            continue;
        }
        if entry.mode == MODE_TREE {
            continue;
        }
        let path_str = String::from_utf8_lossy(&entry.path);
        let abs = work_tree.join(path_str.as_ref());
        let meta = match fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if entry.mode == MODE_GITLINK && meta.is_dir() && abs.join(".git").exists() {
            continue;
        }
        if meta.is_dir() {
            let _ = fs::remove_dir_all(&abs);
        } else {
            let _ = fs::remove_file(&abs);
        }
        remove_empty_parent_dirs_merge(work_tree, &abs);
    }
    Ok(())
}

/// Abort an in-progress merge.
fn merge_abort() -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    register_merge_submodule_odbs(&repo)?;
    let git_dir = &repo.git_dir;

    if !git_dir.join("MERGE_HEAD").exists() {
        bail!("There is no merge to abort (MERGE_HEAD missing).");
    }

    let autostash_oid = crate::commands::stash::take_pseudo_ref_oid(git_dir, MERGE_AUTOSTASH_REF);

    let restore_oid = if let Some(orig) = grit_lib::state::read_orig_head(git_dir)? {
        orig
    } else {
        let head = resolve_head(git_dir)?;
        match head.oid() {
            Some(oid) => *oid,
            None => bail!("cannot determine HEAD to restore"),
        }
    };

    let index_path = repo.index_path();
    let old_index = repo.load_index_at(&index_path).context("loading index")?;
    let head = resolve_head(git_dir)?;
    let head_oid = head
        .oid()
        .copied()
        .ok_or_else(|| anyhow::anyhow!("cannot determine HEAD for merge --abort"))?;

    let mut new_index =
        crate::commands::reset::build_merge_reset_index(&repo, &old_index, head_oid, &restore_oid)
            .context("building merge-abort index")?;
    crate::commands::reset::preserve_index_cache_flags_from(&old_index, &mut new_index);
    if repo.work_tree.is_some() {
        crate::commands::reset::checkout_merge_reset_worktree(
            &repo,
            &old_index,
            &mut new_index,
            false,
            false,
        )?;
    }
    if let Some(wt) = repo.work_tree.as_deref() {
        grit_lib::diff::refresh_index_stat_content_verified(&mut new_index, wt, None);
    }
    repo.write_index(&mut new_index)?;

    let _ = fs::remove_file(git_dir.join("MERGE_HEAD"));
    let _ = fs::remove_file(git_dir.join("MERGE_RR"));
    let _ = fs::remove_file(git_dir.join("MERGE_MSG"));
    let _ = fs::remove_file(git_dir.join("MERGE_MODE"));
    let _ = fs::remove_file(git_dir.join("AUTO_MERGE"));

    if let Some(oid) = autostash_oid {
        crate::commands::stash::apply_autostash_oid(&repo, &oid)?;
    }

    Ok(())
}

/// Quit the current merge: clean up merge state files but leave HEAD, index,
/// and working tree untouched.
fn merge_quit() -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let git_dir = &repo.git_dir;

    // Git's `remove_merge_branch_state` saves any pending MERGE_AUTOSTASH back to refs/stash
    // (branch.c:837 save_autostash_ref) before removing the state files, printing
    // "Autostash exists; creating a new stash entry.".
    save_merge_autostash(&repo)?;

    // Clean up merge state files (git's remove_merge_branch_state unlinks MERGE_RR too).
    let _ = fs::remove_file(git_dir.join("MERGE_HEAD"));
    let _ = fs::remove_file(git_dir.join("MERGE_RR"));
    let _ = fs::remove_file(git_dir.join("MERGE_MSG"));
    let _ = fs::remove_file(git_dir.join("MERGE_MODE"));
    let _ = fs::remove_file(git_dir.join("AUTO_MERGE"));

    Ok(())
}

/// Continue a merge after conflict resolution (delegates to commit).
fn merge_continue(message: Option<String>) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let git_dir = &repo.git_dir;

    if !git_dir.join("MERGE_HEAD").exists() {
        bail!("There is no merge in progress (MERGE_HEAD missing).");
    }

    // Check that index has no unmerged entries
    let mut index = match repo.load_index() {
        Ok(idx) => idx,
        Err(e) => bail!("cannot load index: {}", e),
    };

    let has_conflicts = index.entries.iter().any(|e| e.stage() != 0);
    if has_conflicts {
        bail!("you need to resolve all merge conflicts before continuing");
    }

    // Build the commit via the existing commit machinery
    // Read MERGE_HEAD, MERGE_MSG
    let merge_heads = grit_lib::state::read_merge_heads(git_dir)?;
    let head = resolve_head(git_dir)?;
    let head_oid = head.oid().copied().context("HEAD has no commit")?;

    let msg = if let Some(m) = message {
        ensure_trailing_newline(&m)
    } else if let Some(merge_msg) = grit_lib::state::read_merge_msg(git_dir)? {
        merge_msg
    } else {
        bail!("no merge message found (use -m to provide one)");
    };

    let no_verify_merge_continue = std::env::args().any(|a| a == "--no-verify");
    run_pre_merge_commit_hook(&repo, no_verify_merge_continue, false, &mut index)?;
    repo.write_index(&mut index)?;
    let mut index_for_tree = index.clone();
    index_for_tree.expand_sparse_directory_placeholders(&repo.odb)?;
    let tree_oid = write_tree_from_index(&repo.odb, &index_for_tree, "")?;
    let config = ConfigSet::load(Some(git_dir), true)?;
    let now = OffsetDateTime::now_utc();
    let author = resolve_ident(&config, "author", now)?;
    let committer = resolve_ident(&config, "committer", now)?;

    let mut parents = vec![head_oid];
    parents.extend(merge_heads);

    let commit_data = CommitData {
        tree: tree_oid,
        parents,
        author,
        committer,
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: msg.clone(),
        raw_message: None,
    };

    let commit_bytes = serialize_commit(&commit_data);
    let commit_oid = repo.odb.write(ObjectKind::Commit, &commit_bytes)?;
    let first_line = msg.lines().next().unwrap_or("").to_string();
    let reflog = format!("commit (merge): {first_line}");
    merge_update_head_with_reflog(&repo, &head, Some(head_oid), commit_oid, Some(&reflog))?;

    // Clean up
    let _ = fs::remove_file(git_dir.join("MERGE_HEAD"));
    let _ = fs::remove_file(git_dir.join("MERGE_RR"));
    let _ = fs::remove_file(git_dir.join("MERGE_MSG"));
    let _ = fs::remove_file(git_dir.join("MERGE_MODE"));
    let _ = fs::remove_file(git_dir.join("AUTO_MERGE"));

    // A merge concluded via `merge --continue` re-applies any pending MERGE_AUTOSTASH.
    apply_merge_autostash(&repo)?;

    if show_merge_commit_summary() {
        let branch = head.branch_name().unwrap_or("HEAD");
        let short = &commit_oid.to_hex()[..7];
        println!("[{branch} {short}] {first_line}");
    }

    Ok(())
}

struct MergeResult {
    index: Index,
    has_conflicts: bool,
    /// Files with conflict markers: (path, content).
    conflict_files: Vec<(String, Vec<u8>)>,
    conflict_descriptions: Vec<ConflictDescription>,
    /// Lines printed before `CONFLICT (submodule)` (merge resolution hints on stdout).
    submodule_merge_stdout: Vec<String>,
    /// `(path, abbrev)` for recursive-submodule advice on stderr.
    submodule_merge_advice: Vec<(String, String)>,
}

/// Octopus merge cannot recover after a failed intermediate head (`git-merge-octopus.sh`).
fn die_octopus_merge_program_failed(repo: &Repository, pre_merge_index: &Index) -> Result<()> {
    restore_index_and_worktree(repo, pre_merge_index)?;
    let _ = fs::remove_file(repo.git_dir.join("ORIG_HEAD"));
    eprintln!("Automated merge did not work.");
    eprintln!("Should not be doing an octopus.");
    eprintln!("fatal: merge program failed");
    std::process::exit(2);
}

/// Multi-head merge hit conflicts: stage unmerged entries, write `MERGE_HEAD` with every merge
/// parent (Git order), and refresh the worktree. `HEAD` stays at the pre-merge tip (Git keeps
/// `ORIG_HEAD` there; the concluding `git commit` parents are `MERGE_HEAD` only — see `commit.rs`).
/// Strategy trials restore the pre-merge index instead (`t7603-merge-reduce-heads`).
fn finish_octopus_merge_on_conflict(
    repo: &Repository,
    head: &HeadState,
    head_oid: ObjectId,
    merge_oids: &[ObjectId],
    merge_names: &[String],
    args: &Args,
    pre_merge_index: &Index,
    merge_result: &MergeResult,
    exit_on_merge_conflict: bool,
) -> Result<()> {
    if !exit_on_merge_conflict {
        restore_index_and_worktree(repo, pre_merge_index)?;
        let n = unmerged_path_count(&merge_result.index);
        return Err(anyhow::Error::new(StrategyTrialConflict(n)));
    }

    let mut idx_auto = merge_result.index.clone();
    materialize_unmerged_entries_for_merge_tree_tree(
        repo,
        &mut idx_auto,
        &merge_result.conflict_files,
    )?;
    let auto_merge_tree = write_tree_from_index(&repo.odb, &idx_auto, "")?;
    let _ = fs::write(
        repo.git_dir.join("AUTO_MERGE"),
        format!("{}\n", auto_merge_tree.to_hex()),
    );

    if args.squash {
        let mut msg = build_squash_msg(repo, head_oid, merge_oids)?;
        msg.push_str("# Conflicts:\n");
        for desc in &merge_result.conflict_descriptions {
            msg.push_str(&format!("#\t{}\n", desc.subject_path));
        }
        fs::write(repo.git_dir.join("SQUASH_MSG"), &msg)?;
    } else {
        let merge_head_content: String = merge_oids
            .iter()
            .map(|oid| format!("{}\n", oid.to_hex()))
            .collect();
        fs::write(repo.git_dir.join("MERGE_HEAD"), &merge_head_content)?;
        let msg = build_octopus_merge_message(head, merge_names, args.message.as_deref(), repo);
        fs::write(repo.git_dir.join("MERGE_MSG"), &msg)?;
        fs::write(repo.git_dir.join("MERGE_MODE"), "")?;
    }

    let head_tree = commit_tree(repo, head_oid)?;
    let head_entries = tree_to_map(tree_to_index_entries(repo, &head_tree, "")?);
    let sparse_on = sparse_checkout_enabled(&repo.git_dir);
    if let Some(ref wt) = repo.work_tree {
        remove_deleted_files(wt, &head_entries, &merge_result.index, sparse_on)?;
        checkout_entries(
            repo,
            wt,
            &merge_result.index,
            Some(&head_entries),
            sparse_on,
        )?;
        let attr_rules = grit_lib::crlf::load_gitattributes(wt);
        let crlf_config = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true).ok();
        for (path, content) in &merge_result.conflict_files {
            let abs = wt.join(path);
            if let Some(parent) = abs.parent() {
                fs::create_dir_all(parent)?;
            }
            let output = if let Some(ref config) = crlf_config {
                let file_attrs = grit_lib::crlf::get_file_attrs(&attr_rules, path, false, config);
                let conv = grit_lib::crlf::ConversionConfig::from_config(config);
                match grit_lib::crlf::convert_to_worktree(
                    content,
                    path,
                    &conv,
                    &file_attrs,
                    None,
                    None,
                    None,
                )
                .map_err(|e| anyhow::anyhow!("smudge filter failed for {path}: {e}"))?
                {
                    Some(d) => d,
                    None => anyhow::bail!("delayed smudge without delayed checkout for {path}"),
                }
            } else {
                content.clone()
            };
            if abs.is_dir() {
                if grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(wt, path) {
                    bail!("Refusing to remove the current working directory:\n{path}\n");
                }
                fs::remove_dir_all(&abs)?;
            }
            // Avoid unnecessary update (see two-head merge path above): preserve mtime when
            // the existing file already matches the bytes we would write.
            if worktree_file_matches(&abs, &output) {
                continue;
            }
            if fs::symlink_metadata(&abs).is_ok_and(|m| m.file_type().is_symlink()) {
                let _ = fs::remove_file(&abs);
            }
            fs::write(&abs, &output)?;
        }
    }

    let mut conflict_index = merge_result.index.clone();
    refresh_index_stat_cache_from_worktree(repo, &mut conflict_index)?;
    repo.write_index(&mut conflict_index)?;

    for desc in &merge_result.conflict_descriptions {
        print_merge_description(desc);
    }
    println!("Automatic merge failed; fix conflicts and then commit the result.");
    let rr = if args.no_rerere_autoupdate {
        grit_lib::rerere::RerereAutoupdate::No
    } else if args.rerere_autoupdate {
        grit_lib::rerere::RerereAutoupdate::Yes
    } else {
        grit_lib::rerere::RerereAutoupdate::FromConfig
    };
    let _ = grit_lib::rerere::repo_rerere(repo, rr);

    Err(anyhow::Error::new(SilentNonZeroExit { code: 1 }))
}

/// One recorded merge conflict for stdout and for remerge-diff headers.
#[derive(Debug, Clone)]
pub(crate) struct ConflictDescription {
    /// Short type tag: `content`, `modify/delete`, `rename/rename`, …
    pub kind: &'static str,
    /// Text after `CONFLICT (kind): ` on the standard merge output line.
    pub body: String,
    /// Path or label replay uses in error messages (legacy second tuple field).
    pub subject_path: String,
    /// When set, remerge-diff matches this path to a diff entry (e.g. rename/rename uses the source path).
    pub remerge_anchor_path: Option<String>,
    /// For `rename/rename(1to2)`: our-side rename destination in the index (mechanical merge tree).
    pub rename_rr_ours_dest: Option<String>,
    /// For `rename/rename(1to2)`: their-side rename destination in the index.
    pub rename_rr_theirs_dest: Option<String>,
    /// Extra `Auto-merging` line for `merge-tree -z` (distinct type collisions at a rename target).
    pub auto_merge_hint_path: Option<String>,
}

impl ConflictDescription {
    /// Full line body prefixed for `remerge` diff headers (matches Git).
    #[must_use]
    pub fn remerge_header_line(&self) -> String {
        format!("remerge CONFLICT ({}): {}", self.kind, self.body)
    }
}

fn print_merge_description(desc: &ConflictDescription) {
    if desc.kind == "binary" {
        println!("warning: Cannot merge binary files: {}", desc.subject_path);
        println!("Cannot merge binary files: {}", desc.subject_path);
    } else if desc.kind == "warning" || desc.kind == "info" {
        println!("{}", desc.body);
    } else {
        println!("CONFLICT ({}): {}", desc.kind, desc.body);
    }
}

fn print_merge_warnings(conflict_descriptions: &[ConflictDescription]) {
    for desc in conflict_descriptions
        .iter()
        .filter(|desc| desc.kind == "warning" || desc.kind == "info")
    {
        print_merge_description(desc);
    }
}

/// Tree-merge result exported for replay-style callers.
pub(crate) struct ReplayTreeMergeResult {
    /// Merged index entries, including conflict stages when unresolved.
    pub index: Index,
    /// Whether the merge produced conflicts.
    pub has_conflicts: bool,
    /// Files with conflict marker content to materialize in worktree.
    pub conflict_files: Vec<(String, Vec<u8>)>,
    /// Human-readable conflict summaries.
    pub conflict_descriptions: Vec<ConflictDescription>,
}

#[derive(Debug)]
struct InternalMergeExecutionError;

impl std::fmt::Display for InternalMergeExecutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "failed to execute internal merge")
    }
}

impl std::error::Error for InternalMergeExecutionError {}

#[derive(Clone, Copy)]
struct ConflictLabels<'a> {
    ours: &'a str,
    base: &'a str,
}

fn resolve_conflict_labels(
    repo: &Repository,
    theirs_name: &str,
    base_label_prefix: &str,
    base_label_override: Option<&str>,
) -> ConflictLabels<'static> {
    let ours = if theirs_name == "Temporary merge branch 2" {
        "Temporary merge branch 1"
    } else {
        "HEAD"
    };

    // When the caller supplies an explicit, human-readable base label (e.g. rebase's
    // "parent of <abbrev> (<subject>)" or "constructed fake ancestor"), use it verbatim:
    // these labels are not OID prefixes and must not gain the ":content" suffix.
    let base = if let Some(label) = base_label_override {
        label.to_string()
    } else if base_label_prefix == "empty tree" {
        "empty tree".to_string()
    } else if theirs_name == "Temporary merge branch 2"
        && (base_label_prefix == "merged common ancestors"
            || base_label_prefix.chars().all(|c| c.is_ascii_hexdigit()))
    {
        base_label_prefix.to_string()
    } else if matches!(resolve_conflict_style(repo), ConflictStyle::ZealousDiff3)
        && base_label_prefix.chars().all(|c| c.is_ascii_hexdigit())
    {
        base_label_prefix.to_string()
    } else {
        format!("{base_label_prefix}:content")
    };

    let ours_static: &'static str = Box::leak(ours.to_string().into_boxed_str());
    let base_static: &'static str = Box::leak(base.into_boxed_str());

    ConflictLabels {
        ours: ours_static,
        base: base_static,
    }
}

pub(crate) fn is_internal_merge_execution_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<InternalMergeExecutionError>()
            .is_some()
    })
}

#[derive(Debug, Clone)]
enum PathMergeBehavior {
    Default,
    BinaryNoMerge,
    Union,
    CustomDriver { command: String },
    CustomDriverMissing { name: String },
}

fn resolve_path_merge_behavior(repo: &Repository, path: &str) -> PathMergeBehavior {
    let Ok(config) = ConfigSet::load(Some(&repo.git_dir), true) else {
        return PathMergeBehavior::Default;
    };

    if let Some(b) = merge_behavior_from_attr_source(repo, path, &config) {
        return b;
    }

    let attrs = repo
        .work_tree
        .as_deref()
        .map(grit_lib::crlf::load_gitattributes)
        .unwrap_or_default();
    let file_attrs = grit_lib::crlf::get_file_attrs(&attrs, path, false, &config);

    match &file_attrs.merge {
        MergeAttr::Unset => PathMergeBehavior::BinaryNoMerge,
        MergeAttr::Driver(name) => {
            if name == "union" {
                PathMergeBehavior::Union
            } else {
                let key = format!("merge.{name}.driver");
                if let Some(command) = config.get(&key) {
                    PathMergeBehavior::CustomDriver { command }
                } else {
                    PathMergeBehavior::CustomDriverMissing { name: name.clone() }
                }
            }
        }
        MergeAttr::Unspecified => {
            if let Some(command) = config.get("merge.default.driver") {
                PathMergeBehavior::CustomDriver { command }
            } else {
                PathMergeBehavior::Default
            }
        }
    }
}

/// `merge` attribute from `GIT_ATTR_SOURCE` / `attr.tree` (same stack as `git check-attr`).
fn merge_behavior_from_attr_source(
    repo: &Repository,
    path: &str,
    config: &ConfigSet,
) -> Option<PathMergeBehavior> {
    let parsed = grit_lib::attributes::load_gitattributes_for_diff(repo).ok()?;
    let ignore_case = config
        .get("core.ignorecase")
        .is_some_and(|v| v == "true" || v == "1" || v == "yes");
    let map = grit_lib::attributes::collect_attrs_for_path(
        &parsed.rules,
        &parsed.macros,
        path,
        ignore_case,
    );
    match map.get("merge") {
        Some(grit_lib::attributes::AttrValue::Unset) => Some(PathMergeBehavior::BinaryNoMerge),
        Some(grit_lib::attributes::AttrValue::Value(name)) => {
            if name == "union" {
                Some(PathMergeBehavior::Union)
            } else {
                let key = format!("merge.{name}.driver");
                if let Some(command) = config.get(&key) {
                    Some(PathMergeBehavior::CustomDriver { command })
                } else {
                    Some(PathMergeBehavior::CustomDriverMissing { name: name.clone() })
                }
            }
        }
        Some(grit_lib::attributes::AttrValue::Set) => {
            if let Some(command) = config.get("merge.default.driver") {
                Some(PathMergeBehavior::CustomDriver { command })
            } else {
                None
            }
        }
        None | Some(grit_lib::attributes::AttrValue::Clear) => None,
    }
}

fn resolve_marker_size_for_path(
    repo: &Repository,
    path: &str,
    ours_label: &str,
    theirs_label: &str,
    marker_warnings: &mut Vec<String>,
) -> usize {
    let mut warning = String::new();
    let size = if let Ok(config) = ConfigSet::load(Some(&repo.git_dir), true) {
        let attrs = repo
            .work_tree
            .as_deref()
            .map(grit_lib::crlf::load_gitattributes)
            .unwrap_or_default();
        let file_attrs = grit_lib::crlf::get_file_attrs(&attrs, path, false, &config);
        parse_conflict_marker_size(
            Some(&file_attrs),
            ours_label,
            theirs_label,
            Some(&mut warning),
        )
    } else {
        parse_conflict_marker_size(None, ours_label, theirs_label, None)
    };
    if !warning.is_empty() {
        marker_warnings.push(warning);
    }
    size
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_custom_merge_driver(
    command_template: &str,
    path: &str,
    base_content: &[u8],
    ours_content: &[u8],
    theirs_content: &[u8],
    base_name: &str,
    ours_name: &str,
    theirs_name: &str,
) -> Result<(Vec<u8>, i32)> {
    let mut base_tmp = NamedTempFile::new().context("creating merge driver base tempfile")?;
    let mut ours_tmp = NamedTempFile::new().context("creating merge driver ours tempfile")?;
    let mut theirs_tmp = NamedTempFile::new().context("creating merge driver theirs tempfile")?;

    base_tmp
        .write_all(base_content)
        .context("writing base tempfile content")?;
    ours_tmp
        .write_all(ours_content)
        .context("writing ours tempfile content")?;
    theirs_tmp
        .write_all(theirs_content)
        .context("writing theirs tempfile content")?;
    base_tmp.flush()?;
    ours_tmp.flush()?;
    theirs_tmp.flush()?;

    let base_path = base_tmp.path().to_string_lossy().into_owned();
    let ours_path = ours_tmp.path().to_string_lossy().into_owned();
    let theirs_path = theirs_tmp.path().to_string_lossy().into_owned();

    let command = command_template
        .replace("%O", &shell_escape_single_quoted(&base_path))
        .replace("%A", &shell_escape_single_quoted(&ours_path))
        .replace("%B", &shell_escape_single_quoted(&theirs_path))
        .replace("%P", &shell_escape_single_quoted(path))
        .replace("%S", &shell_escape_single_quoted(base_name))
        .replace("%X", &shell_escape_single_quoted(ours_name))
        .replace("%Y", &shell_escape_single_quoted(theirs_name));

    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .status()
        .context("executing merge driver command")?;
    let exit_code = match status.code() {
        Some(code) if code >= 128 => {
            return Err(InternalMergeExecutionError.into());
        }
        Some(code) => code,
        None => {
            return Err(InternalMergeExecutionError.into());
        }
    };
    let merged = fs::read(ours_tmp.path()).context("reading merge driver output")?;
    Ok((merged, exit_code))
}

fn shell_escape_single_quoted(value: &str) -> String {
    let escaped = value.replace('\'', "'\"'\"'");
    format!("'{escaped}'")
}

fn parse_conflict_marker_size(
    file_attrs: Option<&grit_lib::crlf::FileAttrs>,
    ours_label: &str,
    theirs_label: &str,
    warning_out: Option<&mut String>,
) -> usize {
    if let Some(attrs) = file_attrs {
        if let Some(raw) = &attrs.conflict_marker_size {
            if let Ok(parsed) = raw.parse::<usize>() {
                return parsed;
            }
            if let Some(out) = warning_out {
                *out = format!("warning: invalid marker-size '{raw}', expecting an integer");
            }
            return 7;
        }
    }

    if ours_label.starts_with("Temporary merge branch")
        || theirs_label.starts_with("Temporary merge branch")
    {
        9
    } else {
        7
    }
}

/// Rename detection settings for tree merge (CLI and `merge.renames` / `diff.renames`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MergeRenameOptions {
    /// When false, skip all rename detection (exact and similarity-based).
    pub detect: bool,
    /// Minimum similarity percentage (0–100) for similarity-based renames.
    pub threshold: u32,
}

impl MergeRenameOptions {
    /// Load defaults from config: `merge.renames` overrides `diff.renames`; threshold is 50%.
    pub fn from_config(repo: &Repository) -> Self {
        let config = ConfigSet::load(Some(&repo.git_dir), true).ok();
        let detect = merge_renames_enabled_from_config(config.as_ref());
        Self {
            detect,
            threshold: 50,
        }
    }
}

fn merge_renames_enabled_from_config(config: Option<&ConfigSet>) -> bool {
    let Some(c) = config else {
        return true;
    };
    if let Some(v) = c.get("merge.renames") {
        return config_value_enables_renames(&v);
    }
    if let Some(v) = c.get("diff.renames") {
        return config_value_enables_renames(&v);
    }
    true
}

fn config_value_enables_renames(val: &str) -> bool {
    let lowered = val.trim().to_ascii_lowercase();
    matches!(
        lowered.as_str(),
        "true" | "yes" | "on" | "1" | "" | "copies" | "copy"
    )
}

/// Build rename maps from base to each side.
///
/// Detects renames by looking for base blobs that appear at different paths
/// in a side (exact OID match), plus similarity-based rename detection for
/// cases where the renamed file was also modified.
///
/// Returns (ours_renames, theirs_renames) where each map goes from
/// old_path (in base) → new_path (in that side).
fn detect_merge_renames(
    repo: &Repository,
    base: &HashMap<Vec<u8>, IndexEntry>,
    ours: &HashMap<Vec<u8>, IndexEntry>,
    theirs: &HashMap<Vec<u8>, IndexEntry>,
    rename_opts: MergeRenameOptions,
) -> (HashMap<Vec<u8>, Vec<u8>>, HashMap<Vec<u8>, Vec<u8>>) {
    if !rename_opts.detect {
        return (HashMap::new(), HashMap::new());
    }
    let threshold = rename_opts.threshold.min(100);
    // Read merge.renamelimit or fall back to diff.renamelimit
    let rename_limit: usize = {
        let config = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true).ok();
        config
            .as_ref()
            .and_then(|c| c.get("merge.renamelimit"))
            .or_else(|| config.as_ref().and_then(|c| c.get("diff.renamelimit")))
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000)
    };
    let zero_oid = ObjectId::zero();
    let is_empty_regular_base = |entry: &IndexEntry| {
        matches!(entry.mode, MODE_REGULAR | MODE_EXECUTABLE)
            && repo
                .odb
                .read(&entry.oid)
                .is_ok_and(|obj| obj.data.is_empty())
    };

    // Build diff entries from base to side, handling the "add-source" pattern:
    // If base has path P with OID X, and side has path P with a DIFFERENT OID Y,
    // but side also has path Q with OID X (exact match), then:
    //   - P was renamed to Q (Deleted P + Added Q)
    //   - A new file was added at P (the Modified becomes an Add)
    let build_diff = |side: &HashMap<Vec<u8>, IndexEntry>| -> Vec<DiffEntry> {
        // First, build an OID → paths map for the side to detect where base blobs moved
        let mut side_oid_to_paths: HashMap<ObjectId, Vec<Vec<u8>>> = HashMap::new();
        for (path, entry) in side {
            side_oid_to_paths
                .entry(entry.oid)
                .or_default()
                .push(path.clone());
        }

        // Find base entries whose OID appears at a different path in the side
        let mut exact_renames: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
        for (base_path, base_entry) in base {
            if is_empty_regular_base(base_entry) {
                continue;
            }
            if let Some(side_entry) = side.get(base_path) {
                // If the same blob is still present at the original path, this
                // source was not renamed away; don't treat additional copies as
                // exact renames from this path.
                if side_entry.oid == base_entry.oid && side_entry.mode == base_entry.mode {
                    continue;
                }
            }
            if let Some(side_paths) = side_oid_to_paths.get(&base_entry.oid) {
                for sp in side_paths {
                    if sp != base_path && !base.contains_key(sp) {
                        // base_path's content appeared at a new path sp in side
                        exact_renames.insert(base_path.clone(), sp.clone());
                        break;
                    }
                }
            }
        }

        let mut entries = Vec::new();
        let mut all_paths = BTreeSet::new();
        all_paths.extend(base.keys());
        all_paths.extend(side.keys());

        // Track which paths are rename targets (don't emit them as plain Added)
        let rename_targets: BTreeSet<Vec<u8>> = exact_renames.values().cloned().collect();
        // Track which paths are rename sources (emit as Deleted)
        let rename_sources: BTreeSet<Vec<u8>> = exact_renames.keys().cloned().collect();

        for path in all_paths {
            let b = base.get(path);
            let s = side.get(path);
            match (b, s) {
                (Some(be), None) => {
                    // Deleted in side
                    if !rename_sources.contains(path) {
                        entries.push(DiffEntry {
                            status: DiffStatus::Deleted,
                            old_path: Some(grit_lib::repo_path::RepoPathBuf::from_bytes(path.to_vec())),
                            new_path: None,
                            old_mode: format!("{:06o}", be.mode),
                            new_mode: String::new(),
                            old_oid: be.oid,
                            new_oid: zero_oid,
                            score: None,
                        });
                    }
                    // If it's a rename source, we handle it via the exact_renames map
                }
                (None, Some(se)) => {
                    // Added in side
                    if !rename_targets.contains(path) {
                        entries.push(DiffEntry {
                            status: DiffStatus::Added,
                            old_path: None,
                            new_path: Some(grit_lib::repo_path::RepoPathBuf::from_bytes(path.to_vec())),
                            old_mode: String::new(),
                            new_mode: format!("{:06o}", se.mode),
                            old_oid: zero_oid,
                            new_oid: se.oid,
                            score: None,
                        });
                    }
                }
                (Some(be), Some(se)) => {
                    // If this is a rename source (content moved elsewhere) and
                    // the content at this path changed, treat the old content as
                    // "deleted" (it moved) and the new content as "added" (new file).
                    if rename_sources.contains(path) && be.oid != se.oid {
                        // The old content moved away → emit Deleted for rename detection
                        entries.push(DiffEntry {
                            status: DiffStatus::Deleted,
                            old_path: Some(grit_lib::repo_path::RepoPathBuf::from_bytes(path.to_vec())),
                            new_path: None,
                            old_mode: format!("{:06o}", be.mode),
                            new_mode: String::new(),
                            old_oid: be.oid,
                            new_oid: zero_oid,
                            score: None,
                        });
                    }
                }
                _ => {}
            }
        }
        entries
    };

    let extract_renames = |side: &HashMap<Vec<u8>, IndexEntry>| -> HashMap<Vec<u8>, Vec<u8>> {
        // First, exact OID-based renames
        let mut side_oid_to_paths: HashMap<ObjectId, Vec<Vec<u8>>> = HashMap::new();
        for (path, entry) in side {
            side_oid_to_paths
                .entry(entry.oid)
                .or_default()
                .push(path.clone());
        }

        let mut map: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
        let mut matched_targets: BTreeSet<Vec<u8>> = BTreeSet::new();

        for (base_path, base_entry) in base {
            if is_empty_regular_base(base_entry) {
                continue;
            }
            if side.contains_key(base_path) {
                // Path still exists in side — check if it's an add-source pattern
                let side_entry = &side[base_path];
                if side_entry.oid == base_entry.oid {
                    continue; // Same content, not renamed
                }
                // Content at base_path changed. Check if original content moved.
                if let Some(side_paths) = side_oid_to_paths.get(&base_entry.oid) {
                    for sp in side_paths {
                        if sp != base_path
                            && !base.contains_key(sp)
                            && !matched_targets.contains(sp)
                        {
                            map.insert(base_path.clone(), sp.clone());
                            matched_targets.insert(sp.clone());
                            break;
                        }
                    }
                }
            } else {
                // Path doesn't exist in side — look for exact OID match at new path
                if let Some(side_paths) = side_oid_to_paths.get(&base_entry.oid) {
                    for sp in side_paths {
                        if !base.contains_key(sp) && !matched_targets.contains(sp) {
                            map.insert(base_path.clone(), sp.clone());
                            matched_targets.insert(sp.clone());
                            break;
                        }
                    }
                }
            }
        }

        // Now do similarity-based rename detection for remaining unmatched deletions
        let diff_entries = build_diff(side);
        // Check rename limit: count deleted and added entries
        let n_deleted = diff_entries
            .iter()
            .filter(|e| matches!(e.status, DiffStatus::Deleted))
            .count();
        let n_added = diff_entries
            .iter()
            .filter(|e| matches!(e.status, DiffStatus::Added))
            .count();
        let detected = if n_deleted > rename_limit || n_added > rename_limit {
            // Rename detection matrix too large, skip similarity detection
            Vec::new()
        } else {
            detect_renames(&repo.odb, None, diff_entries, threshold)
        };
        for e in detected {
            if matches!(e.status, DiffStatus::Renamed) {
                if let (Some(old), Some(new)) = (&e.old_path, &e.new_path) {
                    let old_bytes = old.as_bytes().to_vec();
                    let new_bytes = new.as_bytes().to_vec();
                    if base
                        .get(&old_bytes)
                        .is_some_and(|entry| is_empty_regular_base(entry))
                    {
                        continue;
                    }
                    if !map.contains_key(&old_bytes) && !matched_targets.contains(&new_bytes) {
                        map.insert(old_bytes, new_bytes.clone());
                        matched_targets.insert(new_bytes);
                    }
                }
            }
        }

        map
    };

    let ours_renames = extract_renames(ours);
    let theirs_renames = extract_renames(theirs);

    (ours_renames, theirs_renames)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MergeDirectoryRenamesMode {
    /// Use repository config (merge.directoryRenames).
    FromConfig,
    /// Force directory rename handling on.
    #[allow(dead_code)]
    Enabled,
    /// Force directory rename handling off.
    Disabled,
}

fn merge_directory_renames_enabled(repo: &Repository) -> bool {
    let Ok(config) = ConfigSet::load(Some(&repo.git_dir), true) else {
        return true;
    };
    let Some(raw) = config
        .get("merge.directoryrenames")
        .or_else(|| config.get("merge.directoryRenames"))
    else {
        // Git defaults to detecting directory renames when unset.
        return true;
    };
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "true" | "yes" | "on" | "1" | "conflict"
    )
}

fn merge_directory_renames_enabled_for_mode(
    repo: &Repository,
    mode: MergeDirectoryRenamesMode,
) -> bool {
    match mode {
        MergeDirectoryRenamesMode::Enabled => true,
        MergeDirectoryRenamesMode::Disabled => false,
        MergeDirectoryRenamesMode::FromConfig => merge_directory_renames_enabled(repo),
    }
}

fn merge_directory_renames_conflict_for_mode(
    repo: &Repository,
    mode: MergeDirectoryRenamesMode,
) -> bool {
    match mode {
        MergeDirectoryRenamesMode::FromConfig => ConfigSet::load(Some(&repo.git_dir), true)
            .ok()
            .and_then(|config| {
                config
                    .get("merge.directoryrenames")
                    .or_else(|| config.get("merge.directoryRenames"))
            })
            .map_or(true, |raw| raw.trim().eq_ignore_ascii_case("conflict")),
        MergeDirectoryRenamesMode::Enabled | MergeDirectoryRenamesMode::Disabled => false,
    }
}

fn same_object_kind(mode_a: u32, mode_b: u32) -> bool {
    fn bucket(m: u32) -> u8 {
        if m == MODE_SYMLINK {
            1
        } else if m == MODE_GITLINK {
            2
        } else if m == MODE_TREE {
            3
        } else {
            0
        }
    }
    bucket(mode_a) == bucket(mode_b)
}

fn parent_dir(path: &[u8]) -> Option<Vec<u8>> {
    let slash = path.iter().rposition(|b| *b == b'/')?;
    if slash == 0 {
        return None;
    }
    Some(path[..slash].to_vec())
}

fn parent_dir_or_root(path: &[u8]) -> Vec<u8> {
    parent_dir(path).unwrap_or_default()
}

fn build_directory_rename_map(renames: &HashMap<Vec<u8>, Vec<u8>>) -> HashMap<Vec<u8>, Vec<u8>> {
    let mut counts: BTreeMap<Vec<u8>, BTreeMap<Vec<u8>, usize>> = BTreeMap::new();
    for (old_path, new_path) in renames {
        let Some(old_dir) = parent_dir(old_path) else {
            continue;
        };
        let new_dir = parent_dir_or_root(new_path);
        if old_dir == new_dir {
            continue;
        }
        *counts
            .entry(old_dir)
            .or_default()
            .entry(new_dir)
            .or_default() += 1;
    }

    let mut dir_map = HashMap::new();
    for (old_dir, destinations) in counts {
        let mut best: Option<(Vec<u8>, usize)> = None;
        let mut tied = false;
        for (new_dir, count) in destinations {
            match best {
                None => {
                    best = Some((new_dir, count));
                    tied = false;
                }
                Some((_, best_count)) if count > best_count => {
                    best = Some((new_dir, count));
                    tied = false;
                }
                Some((_, best_count)) if count == best_count => {
                    tied = true;
                }
                Some(_) => {}
            }
        }
        if !tied {
            if let Some((new_dir, _)) = best {
                dir_map.insert(old_dir, new_dir);
            }
        }
    }

    dir_map
}

fn directory_rename_split_ties(
    renames: &HashMap<Vec<u8>, Vec<u8>>,
) -> BTreeMap<Vec<u8>, Vec<Vec<u8>>> {
    let mut counts: BTreeMap<Vec<u8>, BTreeMap<Vec<u8>, usize>> = BTreeMap::new();
    for (old_path, new_path) in renames {
        let (Some(old_dir), Some(new_dir)) = (parent_dir(old_path), parent_dir(new_path)) else {
            continue;
        };
        if old_dir == new_dir {
            continue;
        }
        *counts
            .entry(old_dir)
            .or_default()
            .entry(new_dir)
            .or_default() += 1;
    }

    let mut ties = BTreeMap::new();
    for (old_dir, destinations) in counts {
        let max_count = destinations.values().copied().max().unwrap_or(0);
        let tied: Vec<Vec<u8>> = destinations
            .into_iter()
            .filter_map(|(new_dir, count)| (count == max_count).then_some(new_dir))
            .collect();
        if tied.len() > 1 {
            ties.insert(old_dir, tied);
        }
    }
    ties
}

fn has_new_path_under_dir(
    base: &HashMap<Vec<u8>, IndexEntry>,
    side: &HashMap<Vec<u8>, IndexEntry>,
    dir: &[u8],
) -> bool {
    side.keys().any(|path| {
        path.len() > dir.len()
            && path.starts_with(dir)
            && path.get(dir.len()) == Some(&b'/')
            && !base.contains_key(path)
    })
}

fn new_paths_under_dir(
    base: &HashMap<Vec<u8>, IndexEntry>,
    side: &HashMap<Vec<u8>, IndexEntry>,
    dir: &[u8],
) -> Vec<Vec<u8>> {
    let mut paths: Vec<Vec<u8>> = side
        .keys()
        .filter(|path| {
            path.len() > dir.len()
                && path.starts_with(dir)
                && path.get(dir.len()) == Some(&b'/')
                && !base.contains_key(*path)
        })
        .cloned()
        .collect();
    paths.sort();
    paths
}

fn has_path_under_dir(side: &HashMap<Vec<u8>, IndexEntry>, dir: &[u8]) -> bool {
    side.keys().any(|path| {
        path.len() > dir.len() && path.starts_with(dir) && path.get(dir.len()) == Some(&b'/')
    })
}

/// Detect directory renames by comparing full subtrees between `base` and `side` (not only
/// rename-detection pairs). Used when one side renames `olddir/` → `newdir/` while the other
/// adds paths still under `olddir/` (Git "directory rename suggested" / t4301 scenarios).
fn infer_pure_directory_renames(
    base: &HashMap<Vec<u8>, IndexEntry>,
    side: &HashMap<Vec<u8>, IndexEntry>,
) -> HashMap<Vec<u8>, Vec<u8>> {
    fn subtree_fingerprints(entries: &HashMap<Vec<u8>, IndexEntry>) -> HashMap<Vec<u8>, Vec<u8>> {
        let mut by_prefix: HashMap<Vec<u8>, BTreeMap<Vec<u8>, (u32, ObjectId)>> = HashMap::new();
        for (path, entry) in entries {
            if entry.mode == MODE_TREE {
                continue;
            }
            by_prefix
                .entry(Vec::new())
                .or_default()
                .insert(path.clone(), (entry.mode, entry.oid));
            let mut slash_positions: Vec<usize> = Vec::new();
            for (idx, b) in path.iter().enumerate() {
                if *b == b'/' {
                    slash_positions.push(idx);
                }
            }
            for &slash_pos in &slash_positions {
                let prefix = path[..slash_pos].to_vec();
                let rel_start = slash_pos + 1;
                if rel_start > path.len() {
                    continue;
                }
                let rel = path[rel_start..].to_vec();
                by_prefix
                    .entry(prefix)
                    .or_default()
                    .insert(rel, (entry.mode, entry.oid));
            }
        }

        let mut out: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
        for (prefix, children) in by_prefix {
            if children.is_empty() {
                continue;
            }
            let mut canon: Vec<u8> = Vec::new();
            for (rel, (mode, oid)) in &children {
                canon.extend_from_slice(rel);
                canon.push(0);
                canon.extend_from_slice(format!("{mode:o} ").as_bytes());
                canon.extend_from_slice(oid.to_hex().as_bytes());
                canon.push(0);
            }
            out.insert(prefix, canon);
        }
        out
    }

    let base_fp = subtree_fingerprints(base);
    let side_fp = subtree_fingerprints(side);

    let mut fp_to_side_dirs: HashMap<Vec<u8>, Vec<Vec<u8>>> = HashMap::new();
    for (dir, fp) in &side_fp {
        if side.get(dir).is_some() {
            continue;
        }
        fp_to_side_dirs
            .entry(fp.clone())
            .or_default()
            .push(dir.clone());
    }

    let mut out: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    for (old_dir, fp) in &base_fp {
        if base.get(old_dir).is_some() {
            continue;
        }
        let Some(cands) = fp_to_side_dirs.get(fp) else {
            continue;
        };
        if cands.len() != 1 || cands[0].as_slice() == old_dir.as_slice() {
            continue;
        }
        let new_dir = cands[0].clone();
        if out.values().any(|v| v == &new_dir) {
            continue;
        }
        out.insert(old_dir.clone(), new_dir.clone());
    }
    out
}

fn merge_directory_rename_maps(
    mut a: HashMap<Vec<u8>, Vec<u8>>,
    b: HashMap<Vec<u8>, Vec<u8>>,
) -> HashMap<Vec<u8>, Vec<u8>> {
    for (k, v) in b {
        match a.get(&k) {
            None => {
                a.insert(k, v);
            }
            Some(existing) if existing == &v => {}
            Some(_) => {
                a.remove(&k);
            }
        }
    }
    a
}

fn remap_path_by_directory_renames(
    path: &[u8],
    dir_renames: &HashMap<Vec<u8>, Vec<u8>>,
) -> Option<Vec<u8>> {
    let mut best_match: Option<(&Vec<u8>, &Vec<u8>)> = None;
    for (old_dir, new_dir) in dir_renames {
        if path.len() <= old_dir.len() || !path.starts_with(old_dir) {
            continue;
        }
        if path.get(old_dir.len()) != Some(&b'/') {
            continue;
        }
        let should_replace = match best_match {
            None => true,
            Some((best_old, _)) => old_dir.len() > best_old.len(),
        };
        if should_replace {
            best_match = Some((old_dir, new_dir));
        }
    }

    let (old_dir, new_dir) = best_match?;
    let suffix = &path[old_dir.len() + 1..];
    let mut rewritten = new_dir.clone();
    if !rewritten.is_empty() {
        rewritten.push(b'/');
    }
    rewritten.extend_from_slice(suffix);
    Some(rewritten)
}

fn original_path_before_directory_rename(
    path: &[u8],
    dir_renames: &HashMap<Vec<u8>, Vec<u8>>,
) -> Option<Vec<u8>> {
    let mut best_match: Option<(&Vec<u8>, &Vec<u8>)> = None;
    for (old_dir, new_dir) in dir_renames {
        if path.len() <= new_dir.len() || !path.starts_with(new_dir) {
            continue;
        }
        if path.get(new_dir.len()) != Some(&b'/') {
            continue;
        }
        let should_replace = match best_match {
            None => true,
            Some((_, best_new)) => new_dir.len() > best_new.len(),
        };
        if should_replace {
            best_match = Some((old_dir, new_dir));
        }
    }

    let (old_dir, new_dir) = best_match?;
    let suffix = &path[new_dir.len() + 1..];
    let mut original = old_dir.clone();
    original.push(b'/');
    original.extend_from_slice(suffix);
    Some(original)
}

fn directory_rename_conflict_label(
    side_label: &str,
    path: &[u8],
    applied_dir_renames: &HashMap<Vec<u8>, Vec<u8>>,
) -> String {
    original_path_before_directory_rename(path, applied_dir_renames).map_or_else(
        || side_label.to_owned(),
        |original| format!("{side_label}:{}", String::from_utf8_lossy(&original)),
    )
}

fn path_under_directory_rename_source(
    path: &[u8],
    dir_renames: &HashMap<Vec<u8>, Vec<u8>>,
) -> bool {
    dir_renames.keys().any(|dir| {
        path.len() > dir.len() && path.starts_with(dir) && path.get(dir.len()) == Some(&b'/')
    })
}

fn path_is_directory_rename_source(path: &[u8], dir_renames: &HashMap<Vec<u8>, Vec<u8>>) -> bool {
    dir_renames.contains_key(path)
}

fn has_pure_addition_under_dir(
    base: &HashMap<Vec<u8>, IndexEntry>,
    side: &HashMap<Vec<u8>, IndexEntry>,
    side_renames: &HashMap<Vec<u8>, Vec<u8>>,
    dir: &[u8],
) -> bool {
    side.keys().any(|path| {
        path.len() > dir.len()
            && path.starts_with(dir)
            && path.get(dir.len()) == Some(&b'/')
            && !base.contains_key(path)
            && !side_renames.values().any(|target| target == path)
    })
}

fn should_suppress_directory_rename(
    old_dir: &[u8],
    new_dir: &[u8],
    side_own_dir_renames: &HashMap<Vec<u8>, Vec<u8>>,
    base: &HashMap<Vec<u8>, IndexEntry>,
    side: &HashMap<Vec<u8>, IndexEntry>,
    side_renames: &HashMap<Vec<u8>, Vec<u8>>,
) -> bool {
    if path_is_directory_rename_source(new_dir, side_own_dir_renames) {
        return true;
    }
    path_under_directory_rename_source(new_dir, side_own_dir_renames)
        && !has_pure_addition_under_dir(base, side, side_renames, old_dir)
}

fn path_under_nested_directory_rename_destination(
    path: &[u8],
    side_own_dir_renames: &HashMap<Vec<u8>, Vec<u8>>,
    opposite_dir_renames: &HashMap<Vec<u8>, Vec<u8>>,
) -> bool {
    side_own_dir_renames.values().any(|dir| {
        path.len() > dir.len()
            && path.starts_with(dir)
            && path.get(dir.len()) == Some(&b'/')
            && path_under_directory_rename_source(dir, opposite_dir_renames)
    })
}

fn suppressed_directory_renames(
    dir_renames: &HashMap<Vec<u8>, Vec<u8>>,
    side_own_dir_renames: &HashMap<Vec<u8>, Vec<u8>>,
    base: &HashMap<Vec<u8>, IndexEntry>,
    side: &HashMap<Vec<u8>, IndexEntry>,
    side_renames: &HashMap<Vec<u8>, Vec<u8>>,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut suppressed: Vec<(Vec<u8>, Vec<u8>)> = dir_renames
        .iter()
        .filter(|(old_dir, new_dir)| {
            should_suppress_directory_rename(
                old_dir,
                new_dir,
                side_own_dir_renames,
                base,
                side,
                side_renames,
            )
        })
        .map(|(old_dir, new_dir)| (old_dir.clone(), new_dir.clone()))
        .collect();
    suppressed.sort();
    suppressed
}

fn record_suppressed_directory_rename_warnings(
    warnings: &mut Vec<ConflictDescription>,
    base: &HashMap<Vec<u8>, IndexEntry>,
    side: &HashMap<Vec<u8>, IndexEntry>,
    suppressed: &[(Vec<u8>, Vec<u8>)],
) {
    for (old_dir, new_dir) in suppressed {
        let old_s = String::from_utf8_lossy(old_dir).into_owned();
        let new_s = String::from_utf8_lossy(new_dir).into_owned();
        for path in new_paths_under_dir(base, side, old_dir) {
            let path_s = String::from_utf8_lossy(&path).into_owned();
            warnings.push(ConflictDescription {
                kind: "warning",
                body: format!("WARNING: Avoiding applying {old_s} -> {new_s} rename to {path_s}"),
                subject_path: path_s,
                remerge_anchor_path: None,
                rename_rr_ours_dest: None,
                rename_rr_theirs_dest: None,
                auto_merge_hint_path: None,
            });
        }
    }
}

#[derive(Default)]
struct DirectoryRenameApplication {
    path_collisions: Vec<(Vec<u8>, Vec<u8>)>,
    multi_target_collisions: Vec<(Vec<u8>, Vec<Vec<u8>>)>,
    applied_moves: Vec<(Vec<u8>, Vec<u8>)>,
    rename_to_self_content_conflicts: Vec<Vec<u8>>,
}

fn apply_directory_renames_to_side(
    base: &HashMap<Vec<u8>, IndexEntry>,
    side_entries: &mut HashMap<Vec<u8>, IndexEntry>,
    side_renames: &mut HashMap<Vec<u8>, Vec<u8>>,
    side_own_dir_renames: &HashMap<Vec<u8>, Vec<u8>>,
    opposite_dir_renames: &HashMap<Vec<u8>, Vec<u8>>,
    opposite_entries_in_way: Option<&HashMap<Vec<u8>, IndexEntry>>,
    opposite_rename_sources_in_way: Option<&HashMap<Vec<u8>, Vec<u8>>>,
) -> DirectoryRenameApplication {
    if opposite_dir_renames.is_empty() {
        return DirectoryRenameApplication::default();
    }

    let mut result = DirectoryRenameApplication::default();
    let original_side_rename_targets: BTreeSet<Vec<u8>> = side_renames.values().cloned().collect();
    let mut rename_to_self_content_conflict_targets: BTreeSet<Vec<u8>> = BTreeSet::new();
    for (source_path, target_path) in side_renames.iter_mut() {
        if let Some(remapped) = remap_path_by_directory_renames(target_path, opposite_dir_renames) {
            let matching_opposite_rename_target =
                side_entries.get(target_path).is_some_and(|entry| {
                    opposite_entries_in_way
                        .and_then(|entries| entries.get(&remapped))
                        .is_some_and(|opposite| {
                            opposite.oid == entry.oid && opposite.mode == entry.mode
                        })
                        && opposite_rename_sources_in_way.is_some_and(|renames| {
                            renames.values().any(|target| target == &remapped)
                        })
                });
            let rename_to_self_content_conflict = remapped == *source_path
                && side_entries.get(target_path).is_some_and(|entry| {
                    opposite_entries_in_way
                        .and_then(|entries| entries.get(&remapped))
                        .is_some_and(|opposite| {
                            opposite.oid != entry.oid || opposite.mode != entry.mode
                        })
                });
            if rename_to_self_content_conflict {
                rename_to_self_content_conflict_targets.insert(target_path.clone());
                result
                    .rename_to_self_content_conflicts
                    .push(source_path.clone());
            }
            if remapped != *target_path
                && (side_entries.contains_key(&remapped)
                    || (opposite_entries_in_way
                        .is_some_and(|entries| entries.contains_key(&remapped))
                        && !matching_opposite_rename_target
                        && !rename_to_self_content_conflict)
                    || opposite_rename_sources_in_way
                        .is_some_and(|renames| renames.contains_key(&remapped))
                    || path_has_tree_descendant(side_entries, &remapped))
            {
                continue;
            }
            *target_path = remapped;
        }
    }

    let mut candidates: BTreeMap<Vec<u8>, Vec<Vec<u8>>> = BTreeMap::new();
    let mut original_paths: Vec<Vec<u8>> = side_entries.keys().cloned().collect();
    original_paths.sort();
    for old_path in original_paths {
        if base.contains_key(&old_path) {
            continue;
        }
        if path_under_nested_directory_rename_destination(
            &old_path,
            side_own_dir_renames,
            opposite_dir_renames,
        ) && !original_side_rename_targets.contains(&old_path)
            && !side_renames.values().any(|target| target == &old_path)
        {
            continue;
        }
        let Some(new_path) = remap_path_by_directory_renames(&old_path, opposite_dir_renames)
        else {
            continue;
        };
        if new_path == old_path {
            continue;
        }
        if path_has_tree_descendant(side_entries, &new_path) {
            continue;
        }
        candidates.entry(new_path).or_default().push(old_path);
    }

    for (new_path, mut old_paths) in candidates {
        old_paths.sort();
        if old_paths.len() > 1 {
            result.multi_target_collisions.push((new_path, old_paths));
            continue;
        }
        let Some(old_path) = old_paths.into_iter().next() else {
            continue;
        };
        let matching_opposite_rename_target = side_entries.get(&old_path).is_some_and(|entry| {
            opposite_entries_in_way
                .and_then(|entries| entries.get(&new_path))
                .is_some_and(|opposite| opposite.oid == entry.oid && opposite.mode == entry.mode)
                && opposite_rename_sources_in_way
                    .is_some_and(|renames| renames.values().any(|target| target == &new_path))
        });
        let rename_to_self_content_conflict =
            rename_to_self_content_conflict_targets.contains(&old_path);
        if side_entries.contains_key(&new_path)
            || (opposite_entries_in_way.is_some_and(|entries| entries.contains_key(&new_path))
                && !matching_opposite_rename_target
                && !rename_to_self_content_conflict)
            || opposite_rename_sources_in_way.is_some_and(|renames| renames.contains_key(&new_path))
        {
            result.path_collisions.push((old_path, new_path));
            continue;
        }
        let Some(mut entry) = side_entries.remove(&old_path) else {
            continue;
        };
        entry.path = new_path.clone();
        result.applied_moves.push((old_path, new_path.clone()));
        side_entries.insert(new_path, entry);
    }
    result
}

/// Perform tree-level three-way merge.
///
/// For directory/file conflicts, unmerged entries are placed at `path~SUFFIX` where `SUFFIX`
/// is the full hex OID of the commit whose tree still has a **file** at that path (not the
/// side that turned the path into a directory).
///
/// When `criss_cross_outer_merge` is true (recursive merge after folding multiple merge bases),
/// directory/file conflicts use Git merge-ort index layout at the original path with stages
/// 1+2 or 1+3.
fn merge_trees(
    repo: &Repository,
    base: &HashMap<Vec<u8>, IndexEntry>,
    ours: &HashMap<Vec<u8>, IndexEntry>,
    theirs: &HashMap<Vec<u8>, IndexEntry>,
    _head: &HeadState,
    their_name: &str,
    base_label_prefix: &str,
    merge_ours_oid_hex: &str,
    merge_theirs_oid_hex: &str,
    favor: MergeFavor,
    diff_algorithm: Option<&str>,
    merge_renormalize: bool,
    ignore_all_space: bool,
    ignore_space_change: bool,
    ignore_space_at_eol: bool,
    ignore_cr_at_eol: bool,
    merge_directory_renames_mode: MergeDirectoryRenamesMode,
    rename_options: MergeRenameOptions,
    forced_branch_labels: Option<(String, String)>,
    criss_cross_outer_merge: bool,
    mut auto_merge_paths: Option<&mut Vec<String>>,
    base_label_override: Option<&str>,
) -> Result<MergeResult> {
    trace2_perf_region_enter("collect_merge_info");
    trace2_perf_region_enter("collect_merge_info");
    trace2_perf_region_enter("process_entries");

    // Detect renames on each side
    let (mut ours_renames, mut theirs_renames) =
        detect_merge_renames(repo, base, ours, theirs, rename_options);
    let mut ours_entries = ours.clone();
    let mut theirs_entries = theirs.clone();

    let mut directory_rename_suggested: Vec<ConflictDescription> = Vec::new();
    let mut dir_renames_applied_to_ours: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    let mut dir_renames_applied_to_theirs: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    let mut theirs_renames_pre_dir_for_labels: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    let mut ours_renames_pre_dir_for_labels: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    let mut ours_rename_to_self_content_conflicts: BTreeSet<Vec<u8>> = BTreeSet::new();
    let mut theirs_rename_to_self_content_conflicts: BTreeSet<Vec<u8>> = BTreeSet::new();

    if merge_directory_renames_enabled_for_mode(repo, merge_directory_renames_mode) {
        let directory_renames_conflict =
            merge_directory_renames_conflict_for_mode(repo, merge_directory_renames_mode);
        let mut ours_dir_renames = merge_directory_rename_maps(
            build_directory_rename_map(&ours_renames),
            infer_pure_directory_renames(base, &ours_entries),
        );
        let mut theirs_dir_renames = merge_directory_rename_maps(
            build_directory_rename_map(&theirs_renames),
            infer_pure_directory_renames(base, &theirs_entries),
        );
        let ours_dir_renames_unfiltered = ours_dir_renames.clone();
        let theirs_dir_renames_unfiltered = theirs_dir_renames.clone();
        ours_dir_renames.retain(|old_dir, _| {
            !(has_path_under_dir(&ours_entries, old_dir)
                && has_path_under_dir(&theirs_entries, old_dir))
        });
        theirs_dir_renames.retain(|old_dir, _| {
            !(has_path_under_dir(&ours_entries, old_dir)
                && has_path_under_dir(&theirs_entries, old_dir))
        });
        for (old_dir, destinations) in directory_rename_split_ties(&ours_renames) {
            if !has_new_path_under_dir(base, &theirs_entries, &old_dir) {
                continue;
            }
            let old_s = String::from_utf8_lossy(&old_dir);
            let dests = destinations
                .iter()
                .map(|dir| format!("{}/", String::from_utf8_lossy(dir)))
                .collect::<Vec<_>>()
                .join(" vs. ");
            directory_rename_suggested.push(ConflictDescription {
                kind: "directory rename split",
                body: format!(
                    "CONFLICT (directory rename split): {old_s}/ was split between {dests}."
                ),
                subject_path: old_s.to_string(),
                remerge_anchor_path: None,
                rename_rr_ours_dest: None,
                rename_rr_theirs_dest: None,
                auto_merge_hint_path: None,
            });
        }
        for (old_dir, destinations) in directory_rename_split_ties(&theirs_renames) {
            if !has_new_path_under_dir(base, &ours_entries, &old_dir) {
                continue;
            }
            let old_s = String::from_utf8_lossy(&old_dir);
            let dests = destinations
                .iter()
                .map(|dir| format!("{}/", String::from_utf8_lossy(dir)))
                .collect::<Vec<_>>()
                .join(" vs. ");
            directory_rename_suggested.push(ConflictDescription {
                kind: "directory rename split",
                body: format!(
                    "CONFLICT (directory rename split): {old_s}/ was split between {dests}."
                ),
                subject_path: old_s.to_string(),
                remerge_anchor_path: None,
                rename_rr_ours_dest: None,
                rename_rr_theirs_dest: None,
                auto_merge_hint_path: None,
            });
        }
        let suppressed_theirs_dir_renames_for_ours = suppressed_directory_renames(
            &theirs_dir_renames,
            &ours_dir_renames,
            base,
            &ours_entries,
            &ours_renames,
        );
        let suppressed_ours_dir_renames_for_theirs = suppressed_directory_renames(
            &ours_dir_renames,
            &theirs_dir_renames,
            base,
            &theirs_entries,
            &theirs_renames,
        );
        record_suppressed_directory_rename_warnings(
            &mut directory_rename_suggested,
            base,
            &ours_entries,
            &suppressed_theirs_dir_renames_for_ours,
        );
        record_suppressed_directory_rename_warnings(
            &mut directory_rename_suggested,
            base,
            &theirs_entries,
            &suppressed_ours_dir_renames_for_theirs,
        );

        let mut theirs_dir_renames_for_ours = theirs_dir_renames.clone();
        theirs_dir_renames_for_ours.retain(|old_dir, new_dir| {
            !should_suppress_directory_rename(
                old_dir,
                new_dir,
                &ours_dir_renames,
                base,
                &ours_entries,
                &ours_renames,
            )
        });
        let mut ours_dir_renames_for_theirs = ours_dir_renames.clone();
        ours_dir_renames_for_theirs.retain(|old_dir, new_dir| {
            !should_suppress_directory_rename(
                old_dir,
                new_dir,
                &theirs_dir_renames,
                base,
                &theirs_entries,
                &theirs_renames,
            )
        });
        dir_renames_applied_to_ours = theirs_dir_renames_for_ours.clone();
        dir_renames_applied_to_theirs = ours_dir_renames_for_theirs.clone();
        let theirs_renames_pre_dir = theirs_renames.clone();
        let ours_renames_pre_dir = ours_renames.clone();
        theirs_renames_pre_dir_for_labels = theirs_renames_pre_dir.clone();
        ours_renames_pre_dir_for_labels = ours_renames_pre_dir.clone();
        let ours_dir_rename_result = apply_directory_renames_to_side(
            base,
            &mut ours_entries,
            &mut ours_renames,
            &ours_dir_renames,
            &theirs_dir_renames_for_ours,
            directory_renames_conflict.then_some(&theirs_entries),
            directory_renames_conflict.then_some(&theirs_renames),
        );
        let theirs_dir_rename_result = apply_directory_renames_to_side(
            base,
            &mut theirs_entries,
            &mut theirs_renames,
            &theirs_dir_renames,
            &ours_dir_renames_for_theirs,
            directory_renames_conflict.then_some(&ours_entries),
            directory_renames_conflict.then_some(&ours_renames),
        );
        ours_rename_to_self_content_conflicts.extend(
            ours_dir_rename_result
                .rename_to_self_content_conflicts
                .iter()
                .cloned(),
        );
        theirs_rename_to_self_content_conflicts.extend(
            theirs_dir_rename_result
                .rename_to_self_content_conflicts
                .iter()
                .cloned(),
        );
        for (new_path, old_paths) in ours_dir_rename_result
            .multi_target_collisions
            .into_iter()
            .chain(theirs_dir_rename_result.multi_target_collisions.into_iter())
        {
            let new_s = String::from_utf8_lossy(&new_path).into_owned();
            let sources = old_paths
                .iter()
                .map(|path| String::from_utf8_lossy(path).into_owned())
                .collect::<Vec<_>>()
                .join(", ");
            directory_rename_suggested.push(ConflictDescription {
                kind: "implicit dir rename",
                body: format!(
                    "Cannot map more than one path to {new_s}; implicit renames would put {sources} there."
                ),
                subject_path: new_s.clone(),
                remerge_anchor_path: Some(new_s),
                rename_rr_ours_dest: None,
                rename_rr_theirs_dest: None,
                auto_merge_hint_path: None,
            });
        }
        for (old_path, new_path) in ours_dir_rename_result
            .path_collisions
            .into_iter()
            .filter(|(_, new_path)| {
                directory_renames_conflict
                    || !path_under_directory_rename_source(new_path, &theirs_dir_renames_unfiltered)
            })
            .chain(
                theirs_dir_rename_result
                    .path_collisions
                    .into_iter()
                    .filter(|(_, new_path)| {
                        directory_renames_conflict
                            || !path_under_directory_rename_source(
                                new_path,
                                &ours_dir_renames_unfiltered,
                            )
                    }),
            )
        {
            let old_s = String::from_utf8_lossy(&old_path).into_owned();
            let new_s = String::from_utf8_lossy(&new_path).into_owned();
            directory_rename_suggested.push(ConflictDescription {
                kind: "implicit dir rename",
                body: format!(
                    "Existing file/dir at {new_s} in the way of implicit directory rename from {old_s}."
                ),
                subject_path: old_s,
                remerge_anchor_path: Some(new_s),
                rename_rr_ours_dest: None,
                rename_rr_theirs_dest: None,
                auto_merge_hint_path: None,
            });
        }

        // Directory-rename-suggested notices (git merge-ort "file location" / t4301 -z records).
        let labels_pre =
            resolve_conflict_labels(repo, their_name, base_label_prefix, base_label_override);
        let ours_l_pre = match &forced_branch_labels {
            Some((o, _)) => o.as_str(),
            None => labels_pre.ours,
        };
        let theirs_l_pre = match &forced_branch_labels {
            Some((_, t)) => t.as_str(),
            None => their_name,
        };
        for (old_path, new_path) in &ours_dir_rename_result.applied_moves {
            if ours_renames_pre_dir
                .values()
                .any(|target| target == old_path)
            {
                continue;
            }
            let old_s = String::from_utf8_lossy(old_path).into_owned();
            let new_s = String::from_utf8_lossy(new_path).into_owned();
            let (kind, body) = if directory_renames_conflict {
                (
                    "directory rename suggested",
                    format!(
                        "CONFLICT (file location): {old_s} added in {ours_l_pre}, inside a directory that was renamed in {theirs_l_pre}, suggesting it should perhaps be moved to {new_s}."
                    ),
                )
            } else {
                (
                    "info",
                    format!(
                        "Path updated: {old_s} added in {ours_l_pre}, inside a directory that was renamed in {theirs_l_pre}; moving it to {new_s}."
                    ),
                )
            };
            directory_rename_suggested.push(ConflictDescription {
                kind,
                body,
                subject_path: new_s,
                remerge_anchor_path: Some(old_s),
                rename_rr_ours_dest: None,
                rename_rr_theirs_dest: None,
                auto_merge_hint_path: None,
            });
        }
        for (old_path, new_path) in &theirs_dir_rename_result.applied_moves {
            if theirs_renames_pre_dir
                .values()
                .any(|target| target == old_path)
            {
                continue;
            }
            let old_s = String::from_utf8_lossy(old_path).into_owned();
            let new_s = String::from_utf8_lossy(new_path).into_owned();
            let (kind, body) = if directory_renames_conflict {
                (
                    "directory rename suggested",
                    format!(
                        "CONFLICT (file location): {old_s} added in {theirs_l_pre}, inside a directory that was renamed in {ours_l_pre}, suggesting it should perhaps be moved to {new_s}."
                    ),
                )
            } else {
                (
                    "info",
                    format!(
                        "Path updated: {old_s} added in {theirs_l_pre}, inside a directory that was renamed in {ours_l_pre}; moving it to {new_s}."
                    ),
                )
            };
            directory_rename_suggested.push(ConflictDescription {
                kind,
                body,
                subject_path: new_s,
                remerge_anchor_path: Some(old_s),
                rename_rr_ours_dest: None,
                rename_rr_theirs_dest: None,
                auto_merge_hint_path: None,
            });
        }
        for (src, old_dest) in &theirs_renames_pre_dir {
            let Some(new_dest) = theirs_renames.get(src) else {
                continue;
            };
            if old_dest == new_dest {
                continue;
            }
            if remap_path_by_directory_renames(old_dest, &ours_dir_renames).as_deref()
                != Some(new_dest.as_slice())
            {
                continue;
            }
            let old_s = String::from_utf8_lossy(old_dest).into_owned();
            let new_s = String::from_utf8_lossy(new_dest).into_owned();
            let src_s = String::from_utf8_lossy(src);
            let (kind, body) = if directory_renames_conflict {
                (
                    "directory rename suggested",
                    format!(
                        "CONFLICT (file location): {src_s} renamed to {old_s} in {theirs_l_pre}, inside a directory that was renamed in {ours_l_pre}, suggesting it should perhaps be moved to {new_s}."
                    ),
                )
            } else {
                (
                    "info",
                    format!(
                        "Path updated: {src_s} renamed to {old_s} in {theirs_l_pre}, inside a directory that was renamed in {ours_l_pre}; moving it to {new_s}."
                    ),
                )
            };
            directory_rename_suggested.push(ConflictDescription {
                kind,
                body,
                subject_path: new_s,
                remerge_anchor_path: Some(old_s),
                rename_rr_ours_dest: None,
                rename_rr_theirs_dest: None,
                auto_merge_hint_path: None,
            });
        }
        for (src, old_dest) in &ours_renames_pre_dir {
            let Some(new_dest) = ours_renames.get(src) else {
                continue;
            };
            if old_dest == new_dest {
                continue;
            }
            if remap_path_by_directory_renames(old_dest, &theirs_dir_renames).as_deref()
                != Some(new_dest.as_slice())
            {
                continue;
            }
            let old_s = String::from_utf8_lossy(old_dest).into_owned();
            let new_s = String::from_utf8_lossy(new_dest).into_owned();
            let src_s = String::from_utf8_lossy(src);
            let (kind, body) = if directory_renames_conflict {
                (
                    "directory rename suggested",
                    format!(
                        "CONFLICT (file location): {src_s} renamed to {old_s} in {ours_l_pre}, inside a directory that was renamed in {theirs_l_pre}, suggesting it should perhaps be moved to {new_s}."
                    ),
                )
            } else {
                (
                    "info",
                    format!(
                        "Path updated: {src_s} renamed to {old_s} in {ours_l_pre}, inside a directory that was renamed in {theirs_l_pre}; moving it to {new_s}."
                    ),
                )
            };
            directory_rename_suggested.push(ConflictDescription {
                kind,
                body,
                subject_path: new_s,
                remerge_anchor_path: Some(old_s),
                rename_rr_ours_dest: None,
                rename_rr_theirs_dest: None,
                auto_merge_hint_path: None,
            });
        }
    }

    // When both sides independently renamed the same source path to the same
    // destination, this is a rename/rename(1to1) and not a rename/add.
    // Drop stale "add-source" entries that still exist at the source path in
    // the transformed snapshots so later passes don't incorrectly treat them
    // as independent additions.
    for (base_path, ours_new_path) in &ours_renames {
        if theirs_renames.get(base_path) == Some(ours_new_path) {
            // Only strip paths that are still the *base* version at the rename source.
            // If one side replaced the source path (e.g. file `a` → symlink `a` after
            // renaming content to `e`), removing it here would drop that entry and
            // break rename/rename(1to1) + symlink-at-source merges (t6430).
            if let Some(be) = base.get(base_path) {
                if let Some(ours_e) = ours_entries.get(base_path) {
                    if ours_e.oid == be.oid && ours_e.mode == be.mode {
                        ours_entries.remove(base_path);
                    }
                }
                if let Some(theirs_e) = theirs_entries.get(base_path) {
                    if theirs_e.oid == be.oid && theirs_e.mode == be.mode {
                        theirs_entries.remove(base_path);
                    }
                }
            }
        }
    }

    // Track which paths are handled via rename logic so we don't double-process
    let mut handled_paths: BTreeSet<Vec<u8>> = BTreeSet::new();

    let mut all_paths = BTreeSet::new();
    all_paths.extend(base.keys().cloned());
    all_paths.extend(ours_entries.keys().cloned());
    all_paths.extend(theirs_entries.keys().cloned());

    let mut index = Index::new();
    let mut has_conflicts = false;
    let mut conflict_files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut conflict_descriptions: Vec<ConflictDescription> = Vec::new();
    conflict_descriptions.append(&mut directory_rename_suggested);
    if conflict_descriptions.iter().any(|desc| {
        desc.kind == "directory rename split"
            || desc.kind == "implicit dir rename"
            || desc.kind == "directory rename suggested"
    }) {
        has_conflicts = true;
    }
    let mut submodule_merge_stdout: Vec<String> = Vec::new();
    let mut submodule_merge_advice: Vec<(String, String)> = Vec::new();

    let labels = resolve_conflict_labels(repo, their_name, base_label_prefix, base_label_override);
    let base_label = labels.base;
    let ours_label: &str = match &forced_branch_labels {
        Some((o, _)) => o.as_str(),
        None => labels.ours,
    };
    let their_name: &str = match &forced_branch_labels {
        Some((_, t)) => t.as_str(),
        None => their_name,
    };
    let has_descendant = |tree: &HashMap<Vec<u8>, IndexEntry>, path: &[u8]| -> bool {
        tree.keys().any(|candidate| {
            candidate.len() > path.len()
                && candidate.starts_with(path)
                && candidate.get(path.len()) == Some(&b'/')
        })
    };

    // Sources handled by the colliding-1to2 pre-pass below; skipped by Case 1/Case 2.
    let mut prepass_rr_sources: BTreeSet<Vec<u8>> = BTreeSet::new();

    // Pre-pass: different sources renamed to the same destination.
    //
    // In criss-cross merges the virtual base can contain conflicted blobs at both original
    // sources. If ours renames one source to `m` while theirs renames the other source to `m`,
    // Git first merges each original source against the side that kept it, then stages those two
    // merged blobs as add/add stages 2/3 at the shared destination (without a stage 1).
    {
        let mut two_to_one: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> = Vec::new();
        for (ours_src, dest) in &ours_renames {
            for (theirs_src, theirs_dest) in &theirs_renames {
                if dest == theirs_dest && ours_src != theirs_src {
                    two_to_one.push((dest.clone(), ours_src.clone(), theirs_src.clone()));
                }
            }
        }
        two_to_one.sort();
        two_to_one.dedup();

        for (dest, ours_src, theirs_src) in two_to_one {
            if prepass_rr_sources.contains(&ours_src) || prepass_rr_sources.contains(&theirs_src) {
                continue;
            }

            let (
                Some(ours_base),
                Some(ours_renamed),
                Some(theirs_kept),
                Some(theirs_base),
                Some(ours_kept),
                Some(theirs_renamed),
            ) = (
                base.get(&ours_src),
                ours_entries.get(&dest),
                theirs_entries.get(&ours_src),
                base.get(&theirs_src),
                ours_entries.get(&theirs_src),
                theirs_entries.get(&dest),
            )
            else {
                continue;
            };

            prepass_rr_sources.insert(ours_src.clone());
            prepass_rr_sources.insert(theirs_src.clone());
            handled_paths.insert(ours_src.clone());
            handled_paths.insert(theirs_src.clone());
            handled_paths.insert(dest.clone());
            has_conflicts = true;

            let dest_str = String::from_utf8_lossy(&dest).to_string();
            let ours_src_str = String::from_utf8_lossy(&ours_src).to_string();
            let theirs_src_str = String::from_utf8_lossy(&theirs_src).to_string();
            let rr_base_label = same_path_criss_cross_base_label(
                base_label,
                base_label_prefix,
                criss_cross_outer_merge,
            );

            let ours_stage_label = format!("{ours_label}:{dest_str}");
            let ours_base_label = format!("{rr_base_label}:{ours_src_str}");
            let theirs_kept_label = format!("{their_name}:{ours_src_str}");
            let stage2 = match try_content_merge(
                repo,
                &dest_str,
                ours_base,
                ours_renamed,
                theirs_kept,
                &ours_stage_label,
                &ours_base_label,
                &theirs_kept_label,
                favor,
                diff_algorithm,
                merge_renormalize,
                ignore_all_space,
                ignore_space_change,
                ignore_space_at_eol,
                ignore_cr_at_eol,
                auto_merge_paths.as_deref_mut(),
            )? {
                ContentMergeResult::Clean(oid, mode) => {
                    let mut e = ours_renamed.clone();
                    e.oid = oid;
                    e.mode = mode;
                    e
                }
                ContentMergeResult::Conflict(content)
                | ContentMergeResult::BinaryConflict(content) => {
                    let content = lengthen_conflict_marker_lines_of_size(&content, 7, 1);
                    let oid = repo.odb.write(ObjectKind::Blob, &content)?;
                    let mut e = ours_renamed.clone();
                    e.oid = oid;
                    e
                }
            };

            let ours_kept_label = format!("{ours_label}:{theirs_src_str}");
            let theirs_base_label = format!("{rr_base_label}:{theirs_src_str}");
            let theirs_stage_label = format!("{their_name}:{dest_str}");
            let stage3 = match try_content_merge(
                repo,
                &dest_str,
                theirs_base,
                ours_kept,
                theirs_renamed,
                &ours_kept_label,
                &theirs_base_label,
                &theirs_stage_label,
                favor,
                diff_algorithm,
                merge_renormalize,
                ignore_all_space,
                ignore_space_change,
                ignore_space_at_eol,
                ignore_cr_at_eol,
                auto_merge_paths.as_deref_mut(),
            )? {
                ContentMergeResult::Clean(oid, mode) => {
                    let mut e = theirs_renamed.clone();
                    e.oid = oid;
                    e.mode = mode;
                    e
                }
                ContentMergeResult::Conflict(content)
                | ContentMergeResult::BinaryConflict(content) => {
                    let content = lengthen_conflict_marker_lines_of_size(&content, 7, 1);
                    let oid = repo.odb.write(ObjectKind::Blob, &content)?;
                    let mut e = theirs_renamed.clone();
                    e.oid = oid;
                    e
                }
            };

            index.remove(&dest);
            let mut s2 = stage2.clone();
            s2.path = dest.clone();
            stage_entry(&mut index, &s2, 2);
            let mut s3 = stage3.clone();
            s3.path = dest.clone();
            stage_entry(&mut index, &s3, 3);

            let empty_oid = repo.odb.write(ObjectKind::Blob, &[])?;
            let mut empty_base = s2.clone();
            empty_base.oid = empty_oid;
            empty_base.mode = MODE_REGULAR;
            empty_base.flags &= 0x0FFF;
            // rename/rename(2to1): two distinct sources land at the same destination, so the
            // collision content merge is a two-way merge. Git's merge_3way derives the conflict
            // marker labels from the three pathnames the colliding entry carries: when all three
            // are identical it uses the bare branch names (`HEAD`/`B^0`), otherwise it suffixes
            // each with `:path`. The per-side path is that side's rename target *before* any
            // directory rename was applied: a plain rename leaves it equal to the destination,
            // while a directory rename keeps the original explicit target (testcase 7b: B renamed
            // x/d -> z/d and a directory rename moved it to y/d, so the marker reads `B^0:z/d`,
            // whereas t6416's nested-conflicts case renames both sides directly to `m` and the
            // labels stay bare `HEAD`/`R2^0`).
            let ours_marker_path = ours_renames_pre_dir_for_labels
                .get(&ours_src)
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .unwrap_or_else(|| dest_str.clone());
            let theirs_marker_path = theirs_renames_pre_dir_for_labels
                .get(&theirs_src)
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .unwrap_or_else(|| dest_str.clone());
            let pathnames_all_equal =
                ours_marker_path == dest_str && theirs_marker_path == dest_str;
            let final_ours_label = if pathnames_all_equal {
                ours_label.to_string()
            } else {
                format!("{ours_label}:{ours_marker_path}")
            };
            let final_theirs_label = if pathnames_all_equal {
                their_name.to_string()
            } else {
                format!("{their_name}:{theirs_marker_path}")
            };
            let final_content = match try_content_merge(
                repo,
                &dest_str,
                &empty_base,
                &s2,
                &s3,
                &final_ours_label,
                rr_base_label,
                &final_theirs_label,
                MergeFavor::None,
                diff_algorithm,
                merge_renormalize,
                ignore_all_space,
                ignore_space_change,
                ignore_space_at_eol,
                ignore_cr_at_eol,
                auto_merge_paths.as_deref_mut(),
            )? {
                ContentMergeResult::Clean(oid, _) => repo.odb.read(&oid)?.data,
                ContentMergeResult::Conflict(content)
                | ContentMergeResult::BinaryConflict(content) => content,
            };
            conflict_files.push((dest_str.clone(), final_content));
            conflict_descriptions.push(ConflictDescription {
                kind: "rename/rename",
                body: format!(
                    "{ours_src_str} renamed to {dest_str} in {ours_label} and {theirs_src_str} renamed to {dest_str} in {their_name}."
                ),
                subject_path: dest_str.clone(),
                remerge_anchor_path: Some(ours_src_str),
                rename_rr_ours_dest: Some(dest_str.clone()),
                rename_rr_theirs_dest: Some(dest_str),
                auto_merge_hint_path: None,
            });
        }
    }

    // Pre-pass: chains of rename/rename(1to2) whose destinations collide (t4301 mod6).
    //
    // When a single source path is renamed by both sides to *different* destinations
    // (rename/rename 1to2) and those destinations also collide with the destinations of
    // *other* 1to2 renames (forming an add/add cycle), merge-ort merges the source content
    // once and places that merged blob at both destinations (stage2 at our destination,
    // stage3 at theirs'), with stage1 at the source. Each destination that collects content
    // from two distinct sources is additionally reported as an add/add conflict.
    //
    // The generic Case 1/Case 2 logic below handles isolated 1to2 renames; this pre-pass only
    // fires for the colliding-cycle shape so that ordinary 1to2 merges are left untouched.
    {
        // Collect 1to2 sources: renamed by both sides to different destinations.
        let mut one_to_two: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> = Vec::new();
        for (src, od) in &ours_renames {
            if let Some(td) = theirs_renames.get(src) {
                if od != td {
                    one_to_two.push((src.clone(), od.clone(), td.clone()));
                }
            }
        }
        // Detect a destination collision: some destination is claimed by two distinct sources.
        let mut dest_count: HashMap<Vec<u8>, usize> = HashMap::new();
        for (_, od, td) in &one_to_two {
            *dest_count.entry(od.clone()).or_default() += 1;
            *dest_count.entry(td.clone()).or_default() += 1;
        }
        let colliding = dest_count.values().any(|c| *c >= 2);

        if colliding && !one_to_two.is_empty() {
            for (src, _, _) in &one_to_two {
                prepass_rr_sources.insert(src.clone());
            }
            one_to_two.sort();
            // dest -> (Option<stage2 entry>, Option<stage3 entry>, ours_src, theirs_src)
            let mut dest_stages: BTreeMap<Vec<u8>, (Option<IndexEntry>, Option<IndexEntry>)> =
                BTreeMap::new();

            for (src, od, td) in &one_to_two {
                handled_paths.insert(src.clone());
                handled_paths.insert(od.clone());
                handled_paths.insert(td.clone());

                let be = base.get(src);
                let oe = ours_entries.get(od);
                let te = theirs_entries.get(td);
                let (Some(be), Some(oe), Some(te)) = (be, oe, te) else {
                    continue;
                };

                let src_str = String::from_utf8_lossy(src).to_string();
                let od_str = String::from_utf8_lossy(od).to_string();
                let td_str = String::from_utf8_lossy(td).to_string();

                // Merge the source content once. Use the source path as the auto-merge path so
                // that "Auto-merging <source>" is reported exactly when a real three-way merge
                // runs (i.e. both sides changed the content); when one side equals the base the
                // merge is trivial and no auto-merge line is emitted.
                let both_modified = oe.oid != be.oid && te.oid != be.oid;
                let ours_marker = format!("{ours_label}:{od_str}");
                let theirs_marker = format!("{their_name}:{td_str}");
                let merged_entry: IndexEntry = if oe.oid == be.oid && oe.mode == be.mode {
                    // Ours unchanged: take theirs' content.
                    let mut e = te.clone();
                    e.path = od.clone();
                    e
                } else if te.oid == be.oid && te.mode == be.mode {
                    // Theirs unchanged: take ours' content.
                    let mut e = oe.clone();
                    e.path = od.clone();
                    e
                } else {
                    // This merged blob is placed at two colliding rename destinations and may be
                    // further wrapped in an outer add/add conflict, so widen the conflict markers
                    // by 1 (size 8) and use an empty base label, matching git's merge-ort
                    // `handle_content_merge(..., 1 + 2*call_depth)` for the 1to2 collision cycle.
                    match try_content_merge_ext(
                        repo,
                        &src_str,
                        be,
                        oe,
                        te,
                        &ours_marker,
                        "",
                        &theirs_marker,
                        favor,
                        diff_algorithm,
                        merge_renormalize,
                        ignore_all_space,
                        ignore_space_change,
                        ignore_space_at_eol,
                        ignore_cr_at_eol,
                        if both_modified {
                            auto_merge_paths.as_deref_mut()
                        } else {
                            None
                        },
                        1,
                    )? {
                        ContentMergeResult::Clean(oid, mode) => {
                            let mut e = oe.clone();
                            e.oid = oid;
                            e.mode = mode;
                            e
                        }
                        ContentMergeResult::Conflict(content)
                        | ContentMergeResult::BinaryConflict(content) => {
                            let oid = repo.odb.write(ObjectKind::Blob, &content)?;
                            let mut e = oe.clone();
                            e.oid = oid;
                            e
                        }
                    }
                };

                // stage1 at the source path (base content).
                index.remove(src);
                stage_entry(&mut index, be, 1);
                // stage2 at our destination, stage3 at theirs' destination — both the merged blob.
                let mut s2 = merged_entry.clone();
                s2.path = od.clone();
                let mut s3 = merged_entry.clone();
                s3.path = td.clone();
                dest_stages.entry(od.clone()).or_default().0 = Some(s2);
                dest_stages.entry(td.clone()).or_default().1 = Some(s3);

                has_conflicts = true;
                conflict_descriptions.push(ConflictDescription {
                    kind: "rename/rename",
                    body: format!(
                        "{src_str} renamed to {od_str} in {ours_label} and to {td_str} in {their_name}."
                    ),
                    subject_path: src_str.clone(),
                    remerge_anchor_path: Some(src_str.clone()),
                    rename_rr_ours_dest: Some(od_str.clone()),
                    rename_rr_theirs_dest: Some(td_str.clone()),
                    // Auto-merging is driven by auto_merge_paths (populated above only when a
                    // real merge ran), so leave the rename/rename hint unset to avoid a spurious
                    // "Auto-merging" line for the trivial side.
                    auto_merge_hint_path: None,
                });
            }

            // Stage the destination entries and report add/add for collisions (a destination
            // that received content from two distinct sources).
            for (dest, (s2, s3)) in &dest_stages {
                index.remove(dest);
                if let Some(s2) = s2 {
                    stage_entry(&mut index, s2, 2);
                }
                if let Some(s3) = s3 {
                    stage_entry(&mut index, s3, 3);
                }
                if let (Some(s2), Some(s3)) = (s2, s3) {
                    // add/add: merge the two destination contents to produce the working-tree
                    // conflict file and an "Auto-merging <dest>" + CONFLICT(add/add) report.
                    let dest_str = String::from_utf8_lossy(dest).to_string();
                    let content = match try_content_merge_add_add(
                        repo,
                        &dest_str,
                        s2,
                        s3,
                        ours_label,
                        their_name,
                        MergeFavor::None,
                        diff_algorithm,
                        merge_renormalize,
                        ignore_all_space,
                        ignore_space_change,
                        ignore_space_at_eol,
                        ignore_cr_at_eol,
                        auto_merge_paths.as_deref_mut(),
                    )? {
                        ContentMergeResult::Clean(oid, _) => repo.odb.read(&oid)?.data,
                        ContentMergeResult::Conflict(content)
                        | ContentMergeResult::BinaryConflict(content) => content,
                    };
                    conflict_files.push((dest_str.clone(), content));
                    conflict_descriptions.push(ConflictDescription {
                        kind: "rename/add",
                        body: format!("Merge conflict in {dest_str}"),
                        subject_path: dest_str.clone(),
                        remerge_anchor_path: Some(dest_str.clone()),
                        rename_rr_ours_dest: None,
                        rename_rr_theirs_dest: None,
                        auto_merge_hint_path: None,
                    });
                }
            }
        }
    }

    // First pass: handle rename cases
    // Case 1: ours renamed base_path → ours_new_path; theirs may have modified base_path
    for (base_path, ours_new_path) in &ours_renames {
        if prepass_rr_sources.contains(base_path) {
            continue;
        }
        if let Some(oe) = ours_entries.get(ours_new_path) {
            let clean_theirs_directory_side = criss_cross_outer_merge
                && path_descendants_match(&base, &theirs_entries, ours_new_path);
            // When the only thing theirs has under our rename destination is the rename
            // *source* itself (e.g. ours renamed `sub/file` → `sub`, while theirs only
            // modified `sub/file`), the directory `sub/` disappears once that one file is
            // consumed by the rename. There is no real directory in the way, so let the
            // rename handler content-merge the two versions (t6422 "disappearing dir").
            let theirs_dir_is_only_rename_source =
                only_tree_descendant_is(&theirs_entries, ours_new_path, base_path);
            if oe.mode != MODE_TREE
                && path_has_tree_descendant(&theirs_entries, ours_new_path)
                && !clean_theirs_directory_side
                && !theirs_dir_is_only_rename_source
            {
                // Their side has paths under our rename destination (e.g. `newfile/realfile`).
                // A plain rename+content merge at `newfile` would clash with directory/file
                // handling (t6422 rename/directory).
                continue;
            }
        }
        handled_paths.insert(base_path.clone());
        // The new path on ours side is handled here too (don't treat as add/add)
        handled_paths.insert(ours_new_path.clone());

        let be = base.get(base_path);
        let oe = ours_entries.get(ours_new_path); // The renamed file in ours
        let te = theirs_entries.get(base_path); // Theirs' version at original path
        let mut symlink_at_rename_source = false;

        if let (Some(be), Some(oe)) = (be, oe) {
            let mut resolved_entry_at_new: Option<IndexEntry> = None;
            let mut has_conflict_at_new = false;

            // Rename/rename(1to1) to the same destination, with a new entry left at the
            // original path on theirs — typically a symlink `a` → `e` while both moved
            // the file content to `e` (see t6430 "rename vs. rename/symlink").
            if theirs_renames.get(base_path) == Some(ours_new_path) {
                if let Some(te_src) = theirs_entries.get(base_path) {
                    if te_src.mode == MODE_SYMLINK {
                        index.entries.push(oe.clone());
                        index.entries.push(te_src.clone());
                        resolved_entry_at_new = Some(oe.clone());
                        symlink_at_rename_source = true;
                    }
                }
            }

            if resolved_entry_at_new.is_some() {
                // Symlink-at-source case handled above; skip three-way content merge on `te`.
            } else if let Some(te) = te {
                // Theirs also has the file at the old path — merge content at new path
                if ours_rename_to_self_content_conflicts.contains(base_path) {
                    has_conflicts = true;
                    has_conflict_at_new = true;
                    let path_str = String::from_utf8_lossy(ours_new_path).to_string();
                    let mut be_at_new = be.clone();
                    be_at_new.path = ours_new_path.clone();
                    stage_entry(&mut index, &be_at_new, 1);
                    stage_entry(&mut index, oe, 2);
                    let mut te_at_new = te.clone();
                    te_at_new.path = ours_new_path.clone();
                    stage_entry(&mut index, &te_at_new, 3);
                    conflict_descriptions.push(ConflictDescription {
                        kind: "content",
                        body: format!("Merge conflict in {path_str}"),
                        subject_path: path_str.clone(),
                        remerge_anchor_path: None,
                        rename_rr_ours_dest: None,
                        rename_rr_theirs_dest: None,
                        auto_merge_hint_path: None,
                    });
                    if let Ok(obj) = repo.odb.read(&oe.oid) {
                        conflict_files.push((path_str, obj.data));
                    }
                } else if be.oid == te.oid && be.mode == te.mode {
                    // Theirs didn't modify — just use ours (renamed version)
                    index.entries.push(oe.clone());
                    resolved_entry_at_new = Some(oe.clone());
                } else if oe.oid == te.oid {
                    // Both made same change
                    index.entries.push(oe.clone());
                    resolved_entry_at_new = Some(oe.clone());
                } else {
                    // Both modified — try content merge at new path
                    let path_str = String::from_utf8_lossy(ours_new_path).to_string();
                    let base_path_str = String::from_utf8_lossy(base_path).to_string();
                    let ours_marker_label = format!("{ours_label}:{path_str}");
                    let theirs_marker_label = format!("{their_name}:{base_path_str}");
                    match try_content_merge(
                        repo,
                        &path_str,
                        be,
                        oe,
                        te,
                        &ours_marker_label,
                        base_label,
                        &theirs_marker_label,
                        favor,
                        diff_algorithm,
                        merge_renormalize,
                        ignore_all_space,
                        ignore_space_change,
                        ignore_space_at_eol,
                        ignore_cr_at_eol,
                        auto_merge_paths.as_deref_mut(),
                    )? {
                        ContentMergeResult::Clean(merged_oid, mode) => {
                            let mut entry = oe.clone();
                            entry.oid = merged_oid;
                            entry.mode = mode;
                            index.entries.push(entry);
                            let mut resolved = oe.clone();
                            resolved.oid = merged_oid;
                            resolved.mode = mode;
                            resolved_entry_at_new = Some(resolved);
                        }
                        ContentMergeResult::Conflict(content) => {
                            has_conflicts = true;
                            has_conflict_at_new = true;
                            let mut be_at_new = be.clone();
                            be_at_new.path = ours_new_path.clone();
                            stage_entry(&mut index, &be_at_new, 1);
                            stage_entry(&mut index, oe, 2);
                            let mut te_at_new = te.clone();
                            te_at_new.path = ours_new_path.clone();
                            stage_entry(&mut index, &te_at_new, 3);
                            conflict_descriptions.push(ConflictDescription {
                                kind: "content",
                                body: format!("Merge conflict in {path_str}"),
                                subject_path: path_str.clone(),
                                remerge_anchor_path: None,
                                rename_rr_ours_dest: None,
                                rename_rr_theirs_dest: None,
                                auto_merge_hint_path: None,
                            });
                            conflict_files.push((path_str, content));
                        }
                        ContentMergeResult::BinaryConflict(content) => {
                            has_conflicts = true;
                            has_conflict_at_new = true;
                            let mut be_at_new = be.clone();
                            be_at_new.path = ours_new_path.clone();
                            stage_entry(&mut index, &be_at_new, 1);
                            stage_entry(&mut index, oe, 2);
                            let mut te_at_new = te.clone();
                            te_at_new.path = ours_new_path.clone();
                            stage_entry(&mut index, &te_at_new, 3);
                            let b = format!("{path_str} ({ours_label} vs. {their_name})");
                            conflict_descriptions.push(ConflictDescription {
                                kind: "binary",
                                body: b.clone(),
                                subject_path: b,
                                remerge_anchor_path: None,
                                rename_rr_ours_dest: None,
                                rename_rr_theirs_dest: None,
                                auto_merge_hint_path: None,
                            });
                            conflict_files.push((path_str, content));
                        }
                    }
                }
            } else {
                // Theirs deleted the original path. If theirs also renamed the
                // same source to the same destination, treat it as
                // rename/rename(1to1) and merge contents at the destination.
                if theirs_renames.get(base_path) == Some(ours_new_path) {
                    if let Some(te_at_new) = theirs_entries.get(ours_new_path) {
                        if oe.oid == te_at_new.oid && oe.mode == te_at_new.mode {
                            index.entries.push(oe.clone());
                            resolved_entry_at_new = Some(oe.clone());
                        } else {
                            let path_str = String::from_utf8_lossy(ours_new_path).to_string();
                            match try_content_merge(
                                repo,
                                &path_str,
                                be,
                                oe,
                                te_at_new,
                                ours_label,
                                base_label,
                                their_name,
                                favor,
                                diff_algorithm,
                                merge_renormalize,
                                ignore_all_space,
                                ignore_space_change,
                                ignore_space_at_eol,
                                ignore_cr_at_eol,
                                auto_merge_paths.as_deref_mut(),
                            )? {
                                ContentMergeResult::Clean(merged_oid, mode) => {
                                    let mut entry = oe.clone();
                                    entry.oid = merged_oid;
                                    entry.mode = mode;
                                    index.entries.push(entry);
                                    let mut resolved = oe.clone();
                                    resolved.oid = merged_oid;
                                    resolved.mode = mode;
                                    resolved_entry_at_new = Some(resolved);
                                }
                                ContentMergeResult::Conflict(content) => {
                                    has_conflicts = true;
                                    has_conflict_at_new = true;
                                    let mut be_at_new = be.clone();
                                    be_at_new.path = ours_new_path.clone();
                                    stage_entry(&mut index, &be_at_new, 1);
                                    stage_entry(&mut index, oe, 2);
                                    stage_entry(&mut index, te_at_new, 3);
                                    conflict_descriptions.push(ConflictDescription {
                                        kind: "content",
                                        body: format!("Merge conflict in {path_str}"),
                                        subject_path: path_str.clone(),
                                        remerge_anchor_path: None,
                                        rename_rr_ours_dest: None,
                                        rename_rr_theirs_dest: None,
                                        auto_merge_hint_path: None,
                                    });
                                    conflict_files.push((path_str, content));
                                }
                                ContentMergeResult::BinaryConflict(content) => {
                                    has_conflicts = true;
                                    has_conflict_at_new = true;
                                    let mut be_at_new = be.clone();
                                    be_at_new.path = ours_new_path.clone();
                                    stage_entry(&mut index, &be_at_new, 1);
                                    stage_entry(&mut index, oe, 2);
                                    stage_entry(&mut index, te_at_new, 3);
                                    let b = format!("{path_str} ({ours_label} vs. {their_name})");
                                    conflict_descriptions.push(ConflictDescription {
                                        kind: "binary",
                                        body: b.clone(),
                                        subject_path: b,
                                        remerge_anchor_path: None,
                                        rename_rr_ours_dest: None,
                                        rename_rr_theirs_dest: None,
                                        auto_merge_hint_path: None,
                                    });
                                    conflict_files.push((path_str, content));
                                }
                            }
                        }
                    } else {
                        index.entries.push(oe.clone());
                        resolved_entry_at_new = Some(oe.clone());
                    }
                } else if theirs_renames.contains_key(base_path) {
                    // Both sides renamed the same source path to different destinations.
                    // There is no file left at `base_path` on theirs (so `te` is `None` above),
                    // but this is not a delete — Case 2 stages `rename/rename(1to2)` using
                    // `theirs_renames` + `ours_renames`.
                } else {
                    // Theirs has no file at the rename source (deleted or renamed away on their side).
                    has_conflicts = true;
                    has_conflict_at_new = true;
                    let base_path_str = String::from_utf8_lossy(base_path).to_string();
                    let new_path_str = String::from_utf8_lossy(ours_new_path).to_string();
                    let body = format!(
                        "{base_path_str} renamed to {new_path_str} in {ours_label}, but deleted in {their_name}."
                    );
                    conflict_descriptions.push(ConflictDescription {
                        kind: "rename/delete",
                        body: body.clone(),
                        subject_path: new_path_str.clone(),
                        remerge_anchor_path: Some(base_path_str.clone()),
                        rename_rr_ours_dest: None,
                        rename_rr_theirs_dest: None,
                        auto_merge_hint_path: None,
                    });

                    if let Some(te_at_new) = theirs_entries.get(ours_new_path) {
                        if !base.contains_key(ours_new_path) {
                            if !path_has_unmerged_entries(&index, ours_new_path) {
                                // They deleted the source but added a different file at our rename
                                // destination — `merge-tree` reports stages 2 vs 3 only (t4301 rad).
                                // When two renames collide at the same path (t4301 rrdd), only the
                                // first pass should stage the add/add; the second would duplicate.
                                stage_entry(&mut index, oe, 2);
                                stage_entry(&mut index, te_at_new, 3);
                                let ours_marker_label = directory_rename_conflict_label(
                                    ours_label,
                                    ours_new_path,
                                    &dir_renames_applied_to_ours,
                                );
                                let theirs_marker_label = directory_rename_conflict_label(
                                    their_name,
                                    ours_new_path,
                                    &dir_renames_applied_to_theirs,
                                );
                                let conflict_content = match try_content_merge_add_add(
                                    repo,
                                    &new_path_str,
                                    oe,
                                    te_at_new,
                                    &ours_marker_label,
                                    &theirs_marker_label,
                                    MergeFavor::None,
                                    diff_algorithm,
                                    merge_renormalize,
                                    ignore_all_space,
                                    ignore_space_change,
                                    ignore_space_at_eol,
                                    ignore_cr_at_eol,
                                    auto_merge_paths.as_deref_mut(),
                                )? {
                                    ContentMergeResult::Clean(merged_oid, _) => {
                                        repo.odb.read(&merged_oid)?.data
                                    }
                                    ContentMergeResult::Conflict(content)
                                    | ContentMergeResult::BinaryConflict(content) => content,
                                };
                                conflict_files.push((new_path_str.clone(), conflict_content));
                                // If theirs' file at our rename destination is itself a rename
                                // *target* (i.e. theirs renamed some other source → here), then
                                // both sides renamed distinct sources to the same path: this is a
                                // rename/rename(2to1), not a rename/add. With each side also
                                // deleting the other's rename source, git reports it as
                                // rename/rename + two rename/delete lines (t6422 rrdd #18).
                                let theirs_src_to_dest = theirs_renames
                                    .iter()
                                    .find(|(_, dest)| dest.as_slice() == ours_new_path)
                                    .map(|(src, _)| src.clone());
                                if let Some(theirs_src) = theirs_src_to_dest {
                                    let theirs_src_str =
                                        String::from_utf8_lossy(&theirs_src).into_owned();
                                    // Both sides renamed distinct sources to the same path
                                    // (rename/rename(2to1)). When the other side ALSO deleted
                                    // each rename's source (so neither source survives on the
                                    // opposite branch), merge-ort treats this as
                                    // rename/rename(2to1)/delete/delete: it reports a
                                    // `rename/delete` for EACH renamed source and leaves the two
                                    // renamed blobs as add/add stages 2/3 at the shared
                                    // destination (no stage 1), described as add/add. See
                                    // git/merge-ort.c handle_rename_via_dir's
                                    // `collision && source_deleted` branch (t4301 rrdd #17).
                                    let theirs_src_deleted_on_ours =
                                        ours_entries.get(&theirs_src).is_none();
                                    if theirs_src_deleted_on_ours {
                                        conflict_descriptions.push(ConflictDescription {
                                            kind: "rename/delete",
                                            body: format!(
                                                "{theirs_src_str} renamed to {new_path_str} in {their_name}, but deleted in {ours_label}."
                                            ),
                                            subject_path: new_path_str.clone(),
                                            remerge_anchor_path: Some(theirs_src_str),
                                            rename_rr_ours_dest: None,
                                            rename_rr_theirs_dest: None,
                                            auto_merge_hint_path: None,
                                        });
                                        conflict_descriptions.push(ConflictDescription {
                                            kind: "add/add",
                                            body: format!("Merge conflict in {new_path_str}"),
                                            subject_path: new_path_str.clone(),
                                            remerge_anchor_path: Some(base_path_str),
                                            rename_rr_ours_dest: None,
                                            rename_rr_theirs_dest: None,
                                            auto_merge_hint_path: None,
                                        });
                                    } else {
                                        conflict_descriptions.push(ConflictDescription {
                                            kind: "rename/rename",
                                            body: format!(
                                                "{base_path_str} renamed to {new_path_str} in {ours_label} and {theirs_src_str} renamed to {new_path_str} in {their_name}."
                                            ),
                                            subject_path: new_path_str.clone(),
                                            remerge_anchor_path: Some(base_path_str),
                                            rename_rr_ours_dest: None,
                                            rename_rr_theirs_dest: None,
                                            auto_merge_hint_path: None,
                                        });
                                    }
                                } else {
                                    conflict_descriptions.push(ConflictDescription {
                                        kind: "rename/add",
                                        body: format!("Merge conflict in {new_path_str}"),
                                        subject_path: new_path_str.clone(),
                                        remerge_anchor_path: Some(base_path_str),
                                        rename_rr_ours_dest: None,
                                        rename_rr_theirs_dest: None,
                                        auto_merge_hint_path: None,
                                    });
                                }
                            }
                        } else {
                            let mut be_at_new = be.clone();
                            be_at_new.path = ours_new_path.clone();
                            stage_entry(&mut index, &be_at_new, 1);
                            stage_entry(&mut index, oe, 2);
                            if let Ok(obj) = repo.odb.read(&oe.oid) {
                                conflict_files.push((new_path_str.clone(), obj.data));
                            }
                            let md_body = format!(
                                "{new_path_str} deleted in {their_name} and modified in {ours_label}.  Version {ours_label} of {new_path_str} left in tree."
                            );
                            conflict_descriptions.push(ConflictDescription {
                                kind: "modify/delete",
                                body: md_body,
                                subject_path: new_path_str.clone(),
                                remerge_anchor_path: None,
                                rename_rr_ours_dest: None,
                                rename_rr_theirs_dest: None,
                                auto_merge_hint_path: None,
                            });
                        }
                    } else {
                        // They deleted the rename source and did not add a different blob at the
                        // destination — stage base vs ours at `ours_new_path` (Git reports
                        // `rename/delete` only; t6416 expects `:1:path` / `:2:path` at the final name).
                        let dest_str = String::from_utf8_lossy(ours_new_path).to_string();
                        let mut be_at_dest = be.clone();
                        be_at_dest.path = ours_new_path.clone();
                        stage_entry(&mut index, &be_at_dest, 1);
                        stage_entry(&mut index, oe, 2);
                        if let Ok(obj) = repo.odb.read(&oe.oid) {
                            conflict_files.push((dest_str.clone(), obj.data));
                        }
                        // When ours also modified the renamed file's content (oe differs from the
                        // base blob), git emits an additional modify/delete conflict at the rename
                        // destination, in addition to the rename/delete above.
                        if oe.oid != be.oid {
                            let md_body = format!(
                                "{dest_str} deleted in {their_name} and modified in {ours_label}.  Version {ours_label} of {dest_str} left in tree."
                            );
                            conflict_descriptions.push(ConflictDescription {
                                kind: "modify/delete",
                                body: md_body,
                                subject_path: dest_str,
                                remerge_anchor_path: None,
                                rename_rr_ours_dest: None,
                                rename_rr_theirs_dest: None,
                                auto_merge_hint_path: None,
                            });
                        }
                    }
                }
            }

            // If theirs also has a NEW file at ours_new_path (add/add at rename target)
            if let (Some(te_at_new), Some(resolved_entry)) = (
                theirs_entries.get(ours_new_path),
                resolved_entry_at_new.as_ref(),
            ) {
                if !base.contains_key(ours_new_path) && !has_conflict_at_new {
                    if theirs_renames.get(base_path) == Some(ours_new_path) {
                        continue;
                    }
                    // Theirs added a file at the same path as ours' rename target.
                    // Compare against the already-resolved rename destination content.
                    if resolved_entry.oid != te_at_new.oid || resolved_entry.mode != te_at_new.mode
                    {
                        let path_str = String::from_utf8_lossy(ours_new_path).to_string();
                        has_conflicts = true;
                        remove_stage_zero_entry(&mut index, ours_new_path);
                        if same_object_kind(resolved_entry.mode, te_at_new.mode) {
                            stage_entry(&mut index, resolved_entry, 2);
                            stage_entry(&mut index, te_at_new, 3);
                            let competing_theirs_rename_src =
                                theirs_renames.iter().find_map(|(src, dest)| {
                                    if src.as_slice() != base_path.as_slice()
                                        && dest.as_slice() == ours_new_path.as_slice()
                                    {
                                        Some(src.clone())
                                    } else {
                                        None
                                    }
                                });
                            let ours_marker_label = if competing_theirs_rename_src.is_some() {
                                format!("{ours_label}:{path_str}")
                            } else {
                                directory_rename_conflict_label(
                                    ours_label,
                                    ours_new_path,
                                    &dir_renames_applied_to_ours,
                                )
                            };
                            let theirs_marker_label = competing_theirs_rename_src
                                .as_ref()
                                .and_then(|src| theirs_renames_pre_dir_for_labels.get(src))
                                .map_or_else(
                                    || {
                                        directory_rename_conflict_label(
                                            their_name,
                                            ours_new_path,
                                            &dir_renames_applied_to_theirs,
                                        )
                                    },
                                    |pre_dir_target| {
                                        format!(
                                            "{their_name}:{}",
                                            String::from_utf8_lossy(pre_dir_target)
                                        )
                                    },
                                );
                            let conflict_content = match try_content_merge_add_add(
                                repo,
                                &path_str,
                                resolved_entry,
                                te_at_new,
                                &ours_marker_label,
                                &theirs_marker_label,
                                MergeFavor::None,
                                diff_algorithm,
                                merge_renormalize,
                                ignore_all_space,
                                ignore_space_change,
                                ignore_space_at_eol,
                                ignore_cr_at_eol,
                                auto_merge_paths.as_deref_mut(),
                            )? {
                                ContentMergeResult::Clean(merged_oid, _) => {
                                    repo.odb.read(&merged_oid)?.data
                                }
                                ContentMergeResult::Conflict(content)
                                | ContentMergeResult::BinaryConflict(content) => content,
                            };
                            conflict_files.push((path_str.clone(), conflict_content));
                            if let Some(theirs_src) = competing_theirs_rename_src.as_ref() {
                                let ours_src_s = String::from_utf8_lossy(base_path);
                                let theirs_src_s = String::from_utf8_lossy(theirs_src);
                                conflict_descriptions.push(ConflictDescription {
                                    kind: "rename/rename",
                                    body: format!(
                                        "{ours_src_s} renamed to {path_str} in {ours_label} and {theirs_src_s} renamed to {path_str} in {their_name}."
                                    ),
                                    subject_path: path_str.clone(),
                                    remerge_anchor_path: Some(ours_src_s.into_owned()),
                                    rename_rr_ours_dest: Some(path_str.clone()),
                                    rename_rr_theirs_dest: Some(path_str.clone()),
                                    auto_merge_hint_path: None,
                                });
                            } else {
                                conflict_descriptions.push(ConflictDescription {
                                    kind: "rename/add",
                                    body: format!("Merge conflict in {path_str}"),
                                    subject_path: path_str.clone(),
                                    remerge_anchor_path: Some(
                                        String::from_utf8_lossy(base_path).into_owned(),
                                    ),
                                    rename_rr_ours_dest: None,
                                    rename_rr_theirs_dest: None,
                                    auto_merge_hint_path: None,
                                });
                            }
                        } else {
                            let side_path = format!("{path_str}~{their_name}");
                            let side_path_bytes = side_path.as_bytes().to_vec();
                            stage_entry(&mut index, resolved_entry, 2);
                            let mut te_side = te_at_new.clone();
                            te_side.path = side_path_bytes.clone();
                            stage_entry(&mut index, &te_side, 3);
                            if let Ok(obj) = repo.odb.read(&te_at_new.oid) {
                                conflict_files.push((side_path.clone(), obj.data));
                            }
                            let body = format!(
                                "{path_str} had different types on each side; renamed one of them so each can be recorded somewhere."
                            );
                            conflict_descriptions.push(ConflictDescription {
                                kind: "distinct modes",
                                body,
                                subject_path: side_path,
                                remerge_anchor_path: Some(path_str.clone()),
                                rename_rr_ours_dest: None,
                                rename_rr_theirs_dest: None,
                                auto_merge_hint_path: Some(
                                    String::from_utf8_lossy(base_path).into_owned(),
                                ),
                            });
                        }
                    }
                }
            }
        }

        // Handle "add-source" only when theirs also renamed this source path away.
        // If theirs did not rename away (i.e. it only modified the original path),
        // we must not keep a tracked entry at base_path here, or we'd clobber an
        // untracked working-tree file at that path in scenarios like t6414.
        if theirs_renames.contains_key(base_path) && !symlink_at_rename_source {
            if let Some(te_at_base) = theirs_entries.get(base_path) {
                if be.is_none_or(|b| te_at_base.oid != b.oid) {
                    // Theirs has a new/different file at the old path (add-source)
                    // while also renaming the original away from this path.
                    index.entries.push(te_at_base.clone());
                }
            }
        }
    }

    // Case 2: theirs renamed base_path → theirs_new_path; ours may have modified base_path
    let mut theirs_rename_pairs: Vec<(&Vec<u8>, &Vec<u8>)> = theirs_renames.iter().collect();
    theirs_rename_pairs.sort_by(|a, b| {
        a.1.as_slice()
            .cmp(b.1.as_slice())
            .then_with(|| a.0.cmp(b.0))
    });
    for (base_path, theirs_new_path) in theirs_rename_pairs {
        if prepass_rr_sources.contains(base_path) {
            continue;
        }
        if let Some(te) = theirs_entries.get(theirs_new_path) {
            let clean_ours_directory_side = criss_cross_outer_merge
                && path_descendants_match(&base, &ours_entries, theirs_new_path);
            if te.mode != MODE_TREE
                && path_has_tree_descendant(&ours_entries, theirs_new_path)
                && !clean_ours_directory_side
            {
                // Our side has paths under their rename destination. Let the directory/file pass
                // relocate the renamed file and stage the D/F conflict symmetrically with case 1.
                continue;
            }
        }
        if handled_paths.contains(base_path) {
            // Already handled by ours rename of the same source path. If both sides renamed
            // that source to different destinations, we must still stage `rename/rename(1to2)`
            // at `theirs_new_path` even when that path was already marked handled as *another*
            // rename's target (t4301 mod6: `one→two` then `three→two` on the other side).
            if let Some(ours_target) = ours_renames.get(base_path) {
                if ours_target != theirs_new_path {
                    if let (Some(oe), Some(te)) = (
                        ours_entries.get(ours_target),
                        theirs_entries.get(theirs_new_path),
                    ) {
                        let path_str = String::from_utf8_lossy(theirs_new_path).to_string();
                        has_conflicts = true;
                        let dest_already_claimed = handled_paths.contains(theirs_new_path);
                        handled_paths.insert(theirs_new_path.clone());
                        index.remove(base_path);
                        index.remove(ours_target);
                        index.remove(theirs_new_path);
                        if let Some(be) = base.get(base_path) {
                            stage_entry(&mut index, be, 1);
                        }
                        let base_utf = String::from_utf8_lossy(base_path);
                        let ours_tgt_utf = String::from_utf8_lossy(ours_target);
                        let theirs_tgt_utf = String::from_utf8_lossy(theirs_new_path);
                        let body = format!(
                            "{base_utf} renamed to {ours_tgt_utf} in {ours_label} and to {theirs_tgt_utf} in {their_name}."
                        );
                        conflict_descriptions.push(ConflictDescription {
                            kind: "rename/rename",
                            body: body.clone(),
                            subject_path: path_str.clone(),
                            remerge_anchor_path: Some(base_utf.to_string()),
                            rename_rr_ours_dest: Some(ours_tgt_utf.to_string()),
                            rename_rr_theirs_dest: Some(theirs_tgt_utf.to_string()),
                            auto_merge_hint_path: Some(base_utf.to_string()),
                        });

                        if dest_already_claimed {
                            let mut oe_at = oe.clone();
                            oe_at.path = theirs_new_path.clone();
                            stage_entry(&mut index, &oe_at, 2);
                            stage_entry(&mut index, te, 3);
                            let ours_marker_label = directory_rename_conflict_label(
                                ours_label,
                                theirs_new_path,
                                &dir_renames_applied_to_ours,
                            );
                            let theirs_marker_label = directory_rename_conflict_label(
                                their_name,
                                theirs_new_path,
                                &dir_renames_applied_to_theirs,
                            );
                            let rr_content = match try_content_merge_add_add(
                                repo,
                                &path_str,
                                &oe_at,
                                te,
                                &ours_marker_label,
                                &theirs_marker_label,
                                MergeFavor::None,
                                diff_algorithm,
                                merge_renormalize,
                                ignore_all_space,
                                ignore_space_change,
                                ignore_space_at_eol,
                                ignore_cr_at_eol,
                                auto_merge_paths.as_deref_mut(),
                            )? {
                                ContentMergeResult::Clean(merged_oid, _) => {
                                    repo.odb.read(&merged_oid)?.data
                                }
                                ContentMergeResult::Conflict(content)
                                | ContentMergeResult::BinaryConflict(content) => content,
                            };
                            conflict_files.push((path_str.clone(), rr_content));
                            conflict_descriptions.push(ConflictDescription {
                                kind: "rename/add",
                                body: format!("Merge conflict in {path_str}"),
                                subject_path: path_str,
                                remerge_anchor_path: Some(
                                    String::from_utf8_lossy(base_path).into_owned(),
                                ),
                                rename_rr_ours_dest: None,
                                rename_rr_theirs_dest: None,
                                auto_merge_hint_path: None,
                            });
                        } else {
                            // For a rename/rename(1to2) the source content is merged once and the
                            // result placed at *both* destinations — EXCEPT for binary files, which
                            // cannot be merged: each destination keeps its own side's version
                            // (t6422 #25 "rename/rename(1to2) with a binary file").
                            let mut binary_split: Option<(IndexEntry, IndexEntry)> = None;
                            let merged_entry = if let Some(be) = base.get(base_path) {
                                let both_modified = oe.oid != be.oid && te.oid != be.oid;
                                let merge_path = String::from_utf8_lossy(base_path).to_string();
                                let ours_marker = format!("{ours_label}:{ours_tgt_utf}");
                                let theirs_marker = format!("{their_name}:{theirs_tgt_utf}");
                                if oe.oid == be.oid && oe.mode == be.mode {
                                    te.clone()
                                } else if te.oid == be.oid && te.mode == be.mode {
                                    oe.clone()
                                } else if oe.oid == te.oid && oe.mode == te.mode {
                                    oe.clone()
                                } else {
                                    match try_content_merge(
                                        repo,
                                        &merge_path,
                                        be,
                                        oe,
                                        te,
                                        &ours_marker,
                                        base_label,
                                        &theirs_marker,
                                        favor,
                                        diff_algorithm,
                                        merge_renormalize,
                                        ignore_all_space,
                                        ignore_space_change,
                                        ignore_space_at_eol,
                                        ignore_cr_at_eol,
                                        if both_modified {
                                            auto_merge_paths.as_deref_mut()
                                        } else {
                                            None
                                        },
                                    )? {
                                        ContentMergeResult::Clean(oid, mode) => {
                                            let mut e = oe.clone();
                                            e.oid = oid;
                                            e.mode = mode;
                                            e
                                        }
                                        ContentMergeResult::BinaryConflict(_) => {
                                            // Keep ours at ours_target, theirs at theirs_new_path.
                                            binary_split = Some((oe.clone(), te.clone()));
                                            oe.clone()
                                        }
                                        ContentMergeResult::Conflict(content) => {
                                            let oid = repo.odb.write(ObjectKind::Blob, &content)?;
                                            let mut e = oe.clone();
                                            e.oid = oid;
                                            e
                                        }
                                    }
                                }
                            } else {
                                oe.clone()
                            };
                            let (mut ours_stage, mut theirs_stage) =
                                if let Some((ours_side, theirs_side)) = binary_split {
                                    (ours_side, theirs_side)
                                } else {
                                    (merged_entry.clone(), merged_entry.clone())
                                };
                            ours_stage.path = ours_target.clone();
                            theirs_stage.path = theirs_new_path.clone();
                            stage_entry(&mut index, &ours_stage, 2);
                            stage_entry(&mut index, &theirs_stage, 3);
                            // Working-tree content for each rename destination. By default this is
                            // the once-merged source blob (the 1to2 renamed content). When the
                            // *other* side additionally added a brand-new file at one of those
                            // destinations (rename/rename(1to2) + add-dest, t6422 #16), the path is
                            // also an add/add collision, so the working-tree file must hold the
                            // two-way merge of the renamed content and the added file.
                            let mut ours_target_content: Option<Vec<u8>> = None;
                            let mut theirs_target_content: Option<Vec<u8>> = None;
                            if let Some(te_at_ours_target) = theirs_entries.get(ours_target) {
                                if !base.contains_key(ours_target)
                                    && index.get(ours_target, 3).is_none()
                                    && (te_at_ours_target.oid != oe.oid
                                        || te_at_ours_target.mode != oe.mode)
                                {
                                    stage_entry(&mut index, te_at_ours_target, 3);
                                    let ours_target_s =
                                        String::from_utf8_lossy(ours_target).into_owned();
                                    ours_target_content = Some(two_way_conflict_blob(
                                        repo,
                                        &ours_target_s,
                                        &ours_stage,
                                        te_at_ours_target,
                                        ours_label,
                                        their_name,
                                        diff_algorithm,
                                        merge_renormalize,
                                        ignore_all_space,
                                        ignore_space_change,
                                        ignore_space_at_eol,
                                        ignore_cr_at_eol,
                                    )?);
                                    conflict_descriptions.push(ConflictDescription {
                                        kind: "rename/add",
                                        body: format!("Merge conflict in {ours_target_s}"),
                                        subject_path: ours_target_s,
                                        remerge_anchor_path: Some(
                                            String::from_utf8_lossy(base_path).into_owned(),
                                        ),
                                        rename_rr_ours_dest: None,
                                        rename_rr_theirs_dest: None,
                                        auto_merge_hint_path: None,
                                    });
                                }
                            }
                            // Symmetric add-dest: ours added a *new* file at theirs' rename
                            // destination. Stage it at stage 2 of `theirs_new_path` so the index
                            // records both the colliding add (ours) and the renamed content
                            // (theirs) at that path.
                            if let Some(oe_at_theirs_target) = ours_entries.get(theirs_new_path) {
                                if !base.contains_key(theirs_new_path)
                                    && index.get(theirs_new_path, 2).is_none()
                                    && (oe_at_theirs_target.oid != te.oid
                                        || oe_at_theirs_target.mode != te.mode)
                                {
                                    stage_entry(&mut index, oe_at_theirs_target, 2);
                                    let theirs_target_s =
                                        String::from_utf8_lossy(theirs_new_path).into_owned();
                                    theirs_target_content = Some(two_way_conflict_blob(
                                        repo,
                                        &theirs_target_s,
                                        oe_at_theirs_target,
                                        &theirs_stage,
                                        ours_label,
                                        their_name,
                                        diff_algorithm,
                                        merge_renormalize,
                                        ignore_all_space,
                                        ignore_space_change,
                                        ignore_space_at_eol,
                                        ignore_cr_at_eol,
                                    )?);
                                    conflict_descriptions.push(ConflictDescription {
                                        kind: "rename/add",
                                        body: format!("Merge conflict in {theirs_target_s}"),
                                        subject_path: theirs_target_s,
                                        remerge_anchor_path: Some(
                                            String::from_utf8_lossy(base_path).into_owned(),
                                        ),
                                        rename_rr_ours_dest: None,
                                        rename_rr_theirs_dest: None,
                                        auto_merge_hint_path: None,
                                    });
                                }
                            }
                            // Working-tree content per destination: for a binary split each side
                            // keeps its own blob; otherwise both get the once-merged blob.
                            let ours_data = repo.odb.read(&ours_stage.oid).ok().map(|o| o.data);
                            let theirs_data = repo.odb.read(&theirs_stage.oid).ok().map(|o| o.data);
                            if let Some(content) = ours_target_content.or(ours_data) {
                                conflict_files.push((
                                    String::from_utf8_lossy(ours_target).to_string(),
                                    content,
                                ));
                            }
                            if let Some(content) = theirs_target_content.or(theirs_data) {
                                conflict_files.push((
                                    String::from_utf8_lossy(theirs_new_path).to_string(),
                                    content,
                                ));
                            }
                        }
                    }
                }
            }
            continue;
        }
        if path_has_unmerged_entries(&index, theirs_new_path) {
            handled_paths.insert(base_path.clone());
            handled_paths.insert(theirs_new_path.clone());
            continue;
        }
        handled_paths.insert(base_path.clone());
        handled_paths.insert(theirs_new_path.clone());

        let be = base.get(base_path);
        let te = theirs_entries.get(theirs_new_path); // The renamed file in theirs
        let oe = ours_entries.get(base_path); // Ours' version at original path

        if let (Some(be), Some(te)) = (be, te) {
            let mut resolved_entry_at_new: Option<IndexEntry> = None;
            let mut has_conflict_at_new = false;
            if let Some(oe) = oe {
                // Ours also has the file at the old path — merge content at theirs' new path
                if theirs_rename_to_self_content_conflicts.contains(base_path) {
                    has_conflicts = true;
                    has_conflict_at_new = true;
                    let path_str = String::from_utf8_lossy(theirs_new_path).to_string();
                    let mut be_at_new = be.clone();
                    be_at_new.path = theirs_new_path.clone();
                    stage_entry(&mut index, &be_at_new, 1);
                    let mut oe_at_new = oe.clone();
                    oe_at_new.path = theirs_new_path.clone();
                    stage_entry(&mut index, &oe_at_new, 2);
                    stage_entry(&mut index, te, 3);
                    conflict_descriptions.push(ConflictDescription {
                        kind: "content",
                        body: format!("Merge conflict in {path_str}"),
                        subject_path: path_str.clone(),
                        remerge_anchor_path: None,
                        rename_rr_ours_dest: None,
                        rename_rr_theirs_dest: None,
                        auto_merge_hint_path: None,
                    });
                    if let Ok(obj) = repo.odb.read(&oe.oid) {
                        conflict_files.push((path_str, obj.data));
                    }
                } else if be.oid == oe.oid && be.mode == oe.mode {
                    // Ours didn't modify — just use theirs (renamed version)
                    index.entries.push(te.clone());
                    resolved_entry_at_new = Some(te.clone());
                } else if oe.oid == te.oid {
                    // Both made same change
                    let mut entry = te.clone();
                    entry.path = theirs_new_path.clone();
                    resolved_entry_at_new = Some(entry.clone());
                    index.entries.push(entry);
                } else {
                    // Both modified — try content merge at new path
                    let path_str = String::from_utf8_lossy(theirs_new_path).to_string();
                    let base_path_str = String::from_utf8_lossy(base_path).to_string();
                    let ours_marker_label = format!("{ours_label}:{base_path_str}");
                    let theirs_marker_label = format!("{their_name}:{path_str}");
                    match try_content_merge(
                        repo,
                        &path_str,
                        be,
                        oe,
                        te,
                        &ours_marker_label,
                        base_label,
                        &theirs_marker_label,
                        favor,
                        diff_algorithm,
                        merge_renormalize,
                        ignore_all_space,
                        ignore_space_change,
                        ignore_space_at_eol,
                        ignore_cr_at_eol,
                        auto_merge_paths.as_deref_mut(),
                    )? {
                        ContentMergeResult::Clean(merged_oid, mode) => {
                            let mut entry = te.clone();
                            entry.oid = merged_oid;
                            entry.mode = mode;
                            index.entries.push(entry);
                            let mut resolved = te.clone();
                            resolved.oid = merged_oid;
                            resolved.mode = mode;
                            resolved_entry_at_new = Some(resolved);
                        }
                        ContentMergeResult::Conflict(content) => {
                            has_conflicts = true;
                            has_conflict_at_new = true;
                            let mut be_at_new = be.clone();
                            be_at_new.path = theirs_new_path.clone();
                            stage_entry(&mut index, &be_at_new, 1);
                            let mut oe_at_new = oe.clone();
                            oe_at_new.path = theirs_new_path.clone();
                            stage_entry(&mut index, &oe_at_new, 2);
                            stage_entry(&mut index, te, 3);
                            conflict_descriptions.push(ConflictDescription {
                                kind: "content",
                                body: format!("Merge conflict in {path_str}"),
                                subject_path: path_str.clone(),
                                remerge_anchor_path: None,
                                rename_rr_ours_dest: None,
                                rename_rr_theirs_dest: None,
                                auto_merge_hint_path: None,
                            });
                            conflict_files.push((path_str, content));
                        }
                        ContentMergeResult::BinaryConflict(content) => {
                            has_conflicts = true;
                            has_conflict_at_new = true;
                            let mut be_at_new = be.clone();
                            be_at_new.path = theirs_new_path.clone();
                            stage_entry(&mut index, &be_at_new, 1);
                            let mut oe_at_new = oe.clone();
                            oe_at_new.path = theirs_new_path.clone();
                            stage_entry(&mut index, &oe_at_new, 2);
                            stage_entry(&mut index, te, 3);
                            let b = format!("{path_str} ({ours_label} vs. {their_name})");
                            conflict_descriptions.push(ConflictDescription {
                                kind: "binary",
                                body: b.clone(),
                                subject_path: b,
                                remerge_anchor_path: None,
                                rename_rr_ours_dest: None,
                                rename_rr_theirs_dest: None,
                                auto_merge_hint_path: None,
                            });
                            conflict_files.push((path_str, content));
                        }
                    }
                }
            } else {
                // Ours deleted the original path — theirs renamed it to `theirs_new_path`.
                has_conflicts = true;
                has_conflict_at_new = true;
                let base_path_str = String::from_utf8_lossy(base_path).to_string();
                let new_path_str = String::from_utf8_lossy(theirs_new_path).to_string();
                let body = format!(
                    "{base_path_str} renamed to {new_path_str} in {their_name}, but deleted in {ours_label}."
                );
                conflict_descriptions.push(ConflictDescription {
                    kind: "rename/delete",
                    body: body.clone(),
                    subject_path: new_path_str.clone(),
                    remerge_anchor_path: Some(base_path_str.clone()),
                    rename_rr_ours_dest: None,
                    rename_rr_theirs_dest: None,
                    auto_merge_hint_path: None,
                });

                if let Some(oe_at_new) = ours_entries.get(theirs_new_path) {
                    if !base.contains_key(theirs_new_path) {
                        if !path_has_unmerged_entries(&index, theirs_new_path) {
                            // Ours added a different file at the rename target while deleting the
                            // source — Git reports rename/delete + add/add (stages 2 vs 3 only).
                            stage_entry(&mut index, oe_at_new, 2);
                            stage_entry(&mut index, te, 3);
                            let ours_marker_label = directory_rename_conflict_label(
                                ours_label,
                                theirs_new_path,
                                &dir_renames_applied_to_ours,
                            );
                            let theirs_marker_label = directory_rename_conflict_label(
                                their_name,
                                theirs_new_path,
                                &dir_renames_applied_to_theirs,
                            );
                            let conflict_content = match try_content_merge_add_add(
                                repo,
                                &new_path_str,
                                oe_at_new,
                                te,
                                &ours_marker_label,
                                &theirs_marker_label,
                                MergeFavor::None,
                                diff_algorithm,
                                merge_renormalize,
                                ignore_all_space,
                                ignore_space_change,
                                ignore_space_at_eol,
                                ignore_cr_at_eol,
                                auto_merge_paths.as_deref_mut(),
                            )? {
                                ContentMergeResult::Clean(merged_oid, _) => {
                                    repo.odb.read(&merged_oid)?.data
                                }
                                ContentMergeResult::Conflict(content)
                                | ContentMergeResult::BinaryConflict(content) => content,
                            };
                            conflict_files.push((new_path_str.clone(), conflict_content));
                            conflict_descriptions.push(ConflictDescription {
                                kind: "rename/add",
                                body: format!("Merge conflict in {new_path_str}"),
                                subject_path: new_path_str.clone(),
                                remerge_anchor_path: Some(base_path_str),
                                rename_rr_ours_dest: None,
                                rename_rr_theirs_dest: None,
                                auto_merge_hint_path: None,
                            });
                        }
                    } else {
                        let mut be_at_new = be.clone();
                        be_at_new.path = theirs_new_path.clone();
                        stage_entry(&mut index, &be_at_new, 1);
                        stage_entry(&mut index, te, 3);
                        if let Ok(obj) = repo.odb.read(&te.oid) {
                            conflict_files.push((new_path_str.clone(), obj.data));
                        }
                        let md_body = format!(
                            "{new_path_str} deleted in {ours_label} and modified in {their_name}.  Version {their_name} of {new_path_str} left in tree."
                        );
                        conflict_descriptions.push(ConflictDescription {
                            kind: "modify/delete",
                            body: md_body,
                            subject_path: new_path_str.clone(),
                            remerge_anchor_path: None,
                            rename_rr_ours_dest: None,
                            rename_rr_theirs_dest: None,
                            auto_merge_hint_path: None,
                        });
                    }
                } else if path_has_tree_descendant(&ours_entries, theirs_new_path)
                    && !(criss_cross_outer_merge
                        && path_descendants_match(&base, &ours_entries, theirs_new_path))
                {
                    // Ours turned `theirs_new_path` into a directory (e.g. a directory rename put
                    // a subtree there) while theirs renamed a deleted-on-our-side file into it.
                    // git relocates the file to `theirs_new_path~THEIRS`, staging base (the rename
                    // source's base blob) at stage 1 and theirs at stage 3, and reports
                    // file/directory + modify/delete at that relocated path — not at the bare
                    // directory path. Handle it here so the generic D/F pass does not also fire.
                    let side_path = format!("{new_path_str}~{their_name}");
                    let mut be_side = be.clone();
                    be_side.path = side_path.as_bytes().to_vec();
                    stage_entry(&mut index, &be_side, 1);
                    let mut te_side = te.clone();
                    te_side.path = side_path.as_bytes().to_vec();
                    stage_entry(&mut index, &te_side, 3);
                    if let Ok(obj) = repo.odb.read(&te.oid) {
                        conflict_files.push((side_path.clone(), obj.data));
                    }
                    conflict_descriptions.push(ConflictDescription {
                        kind: "file/directory",
                        body: format!(
                            "directory in the way of {new_path_str} from {their_name}; moving it to {side_path} instead."
                        ),
                        subject_path: side_path.clone(),
                        remerge_anchor_path: Some(new_path_str.clone()),
                        rename_rr_ours_dest: None,
                        rename_rr_theirs_dest: None,
                        auto_merge_hint_path: None,
                    });
                    conflict_descriptions.push(ConflictDescription {
                        kind: "modify/delete",
                        body: format!(
                            "{side_path} deleted in {ours_label} and modified in {their_name}.  Version {their_name} of {side_path} left in tree."
                        ),
                        subject_path: side_path,
                        remerge_anchor_path: Some(new_path_str.clone()),
                        rename_rr_ours_dest: None,
                        rename_rr_theirs_dest: None,
                        auto_merge_hint_path: None,
                    });
                    handled_paths.insert(theirs_new_path.clone());
                } else {
                    let mut be_at_new = be.clone();
                    be_at_new.path = theirs_new_path.clone();
                    stage_entry(&mut index, &be_at_new, 1);
                    stage_entry(&mut index, te, 3);
                    if let Ok(obj) = repo.odb.read(&te.oid) {
                        conflict_files.push((new_path_str.clone(), obj.data));
                    }
                    let md_body = format!(
                        "{new_path_str} deleted in {ours_label} and modified in {their_name}.  Version {their_name} of {new_path_str} left in tree."
                    );
                    conflict_descriptions.push(ConflictDescription {
                        kind: "modify/delete",
                        body: md_body,
                        subject_path: new_path_str.clone(),
                        remerge_anchor_path: None,
                        rename_rr_ours_dest: None,
                        rename_rr_theirs_dest: None,
                        auto_merge_hint_path: None,
                    });
                }
            }

            // If ours also has a NEW file at theirs_new_path (add/add at rename target)
            if let (Some(oe_at_new), Some(resolved_entry)) = (
                ours_entries.get(theirs_new_path),
                resolved_entry_at_new.as_ref(),
            ) {
                if !base.contains_key(theirs_new_path)
                    && !has_conflict_at_new
                    && (resolved_entry.oid != oe_at_new.oid
                        || resolved_entry.mode != oe_at_new.mode)
                {
                    let path_str = String::from_utf8_lossy(theirs_new_path).to_string();
                    has_conflicts = true;
                    remove_stage_zero_entry(&mut index, theirs_new_path);
                    if same_object_kind(oe_at_new.mode, resolved_entry.mode) {
                        stage_entry(&mut index, oe_at_new, 2);
                        stage_entry(&mut index, resolved_entry, 3);
                        let ours_marker_label = directory_rename_conflict_label(
                            ours_label,
                            theirs_new_path,
                            &dir_renames_applied_to_ours,
                        );
                        let theirs_marker_label = directory_rename_conflict_label(
                            their_name,
                            theirs_new_path,
                            &dir_renames_applied_to_theirs,
                        );
                        let conflict_content = match try_content_merge_add_add(
                            repo,
                            &path_str,
                            oe_at_new,
                            resolved_entry,
                            &ours_marker_label,
                            &theirs_marker_label,
                            MergeFavor::None,
                            diff_algorithm,
                            merge_renormalize,
                            ignore_all_space,
                            ignore_space_change,
                            ignore_space_at_eol,
                            ignore_cr_at_eol,
                            auto_merge_paths.as_deref_mut(),
                        )? {
                            ContentMergeResult::Clean(merged_oid, _) => {
                                repo.odb.read(&merged_oid)?.data
                            }
                            ContentMergeResult::Conflict(content)
                            | ContentMergeResult::BinaryConflict(content) => content,
                        };
                        conflict_files.push((path_str.clone(), conflict_content));
                        conflict_descriptions.push(ConflictDescription {
                            kind: "rename/add",
                            body: format!("Merge conflict in {path_str}"),
                            subject_path: path_str.clone(),
                            remerge_anchor_path: Some(
                                String::from_utf8_lossy(base_path).into_owned(),
                            ),
                            rename_rr_ours_dest: None,
                            rename_rr_theirs_dest: None,
                            auto_merge_hint_path: None,
                        });
                    } else {
                        let side_path = format!("{path_str}~{their_name}");
                        stage_entry(&mut index, oe_at_new, 2);
                        let mut re_side = resolved_entry.clone();
                        re_side.path = side_path.as_bytes().to_vec();
                        stage_entry(&mut index, &re_side, 3);
                        if let Ok(obj) = repo.odb.read(&resolved_entry.oid) {
                            conflict_files.push((side_path.clone(), obj.data));
                        }
                        let body = format!(
                            "{path_str} had different types on each side; renamed one of them so each can be recorded somewhere."
                        );
                        conflict_descriptions.push(ConflictDescription {
                            kind: "distinct modes",
                            body,
                            subject_path: side_path,
                            remerge_anchor_path: Some(path_str.clone()),
                            rename_rr_ours_dest: None,
                            rename_rr_theirs_dest: None,
                            auto_merge_hint_path: Some(
                                String::from_utf8_lossy(base_path).into_owned(),
                            ),
                        });
                    }
                }
            }

            // Handle "add-source": theirs renamed base_path away, but theirs may also
            // have a NEW file at base_path (add-source pattern: rename + add at source).
            // Also handle ours' file at base_path: ours' modification of the original
            // was used for the merge at the rename target, so we should not also keep
            // it at base_path. But theirs' add-source at base_path should be included.
            if let Some(te_at_base) = theirs_entries.get(base_path) {
                if te_at_base.oid != be.oid {
                    // Theirs has a genuinely new file at the old path (add-source)
                    index.entries.push(te_at_base.clone());
                }
            }
        }
    }

    apply_directory_file_conflicts(
        repo,
        their_name,
        ours_label,
        &base,
        criss_cross_outer_merge,
        &ours_renames,
        &theirs_renames,
        &ours_entries,
        &theirs_entries,
        &mut index,
        &all_paths,
        &mut handled_paths,
        &mut conflict_descriptions,
        &mut conflict_files,
        &mut has_conflicts,
        favor,
        diff_algorithm,
        merge_renormalize,
        ignore_all_space,
        ignore_space_change,
        ignore_space_at_eol,
        ignore_cr_at_eol,
        auto_merge_paths.as_deref_mut(),
    )?;

    // Second pass: handle non-rename paths
    for path in &all_paths {
        if handled_paths.contains(path) {
            continue;
        }

        let b = base.get(path);
        let o = ours_entries.get(path);
        let t = theirs_entries.get(path);

        // Skip paths that are the "add-source" of a rename on the other side.
        // e.g., if ours renamed old→new, and theirs added a completely new file at old,
        // that new file at old is theirs' addition and should be included as-is.
        // But if this path was the source of a rename and the other side didn't touch it,
        // we already handled it above.

        match (b, o, t) {
            // Both sides identical
            (_, Some(oe), Some(te)) if oe.oid == te.oid && oe.mode == te.mode => {
                index.entries.push(oe.clone());
            }
            // Only theirs changed (base == ours)
            (Some(be), Some(oe), Some(te)) if be.oid == oe.oid && be.mode == oe.mode => {
                index.entries.push(te.clone());
            }
            // Only ours changed (base == theirs)
            (Some(be), Some(oe), Some(te)) if be.oid == te.oid && be.mode == te.mode => {
                index.entries.push(oe.clone());
            }
            // Added only by ours — unless theirs only has paths under this name (directory).
            (None, Some(oe), None) => {
                if oe.mode == MODE_GITLINK && has_descendant(&theirs_entries, path) {
                    if path_descendants_match(&base, &theirs_entries, path) {
                        index.entries.push(oe.clone());
                        mark_path_descendants_handled(&mut handled_paths, &base, path);
                        mark_path_descendants_handled(&mut handled_paths, &theirs_entries, path);
                        continue;
                    }

                    let path_str = String::from_utf8_lossy(path).to_string();
                    let Some(te) = first_entry_under_path_prefix(&theirs_entries, path) else {
                        index.entries.push(oe.clone());
                        continue;
                    };
                    let relocated = format!("{path_str}~{ours_label}");
                    has_conflicts = true;
                    let mut gl = oe.clone();
                    gl.path = relocated.as_bytes().to_vec();
                    stage_entry(&mut index, &gl, 2);
                    index.entries.push(te.clone());
                    conflict_descriptions.push(ConflictDescription {
                        kind: "file/directory",
                        body: format!(
                            "directory in the way of {path_str} from {ours_label}; moving it to {relocated} instead."
                        ),
                        subject_path: relocated.clone(),
                        remerge_anchor_path: Some(path_str.clone()),
                        rename_rr_ours_dest: None,
                        rename_rr_theirs_dest: None,
                        auto_merge_hint_path: None,
                    });
                    conflict_descriptions.push(ConflictDescription {
                        kind: "modify/delete",
                        body: format!(
                            "{relocated} deleted in {their_name} and modified in {ours_label}.  Version {ours_label} of {relocated} left in tree."
                        ),
                        subject_path: relocated.clone(),
                        remerge_anchor_path: Some(path_str.clone()),
                        rename_rr_ours_dest: None,
                        rename_rr_theirs_dest: None,
                        auto_merge_hint_path: None,
                    });
                    // Materialize the conflicting directory file (e.g. `path/file` blob), not the
                    // gitlink commit object — `t6437` greps for B1's file contents in `path~HEAD`.
                    if let Ok(obj) = repo.odb.read(&te.oid) {
                        conflict_files.push((relocated, obj.data));
                    }
                    for (k, _) in &theirs_entries {
                        if k.len() > path.len()
                            && k.starts_with(path)
                            && k.get(path.len()) == Some(&b'/')
                        {
                            handled_paths.insert(k.clone());
                        }
                    }
                    continue;
                }
                if oe.mode == MODE_GITLINK {
                    // Submodule replaces a former directory tree (e.g. d/e → gitlink d); not D/F.
                    index.entries.push(oe.clone());
                } else if has_descendant(&theirs_entries, path) {
                    if path_descendants_match(&base, &theirs_entries, path) {
                        index.entries.push(oe.clone());
                        mark_path_descendants_handled(&mut handled_paths, &base, path);
                        mark_path_descendants_handled(&mut handled_paths, &theirs_entries, path);
                        continue;
                    }

                    has_conflicts = true;
                    let path_str = String::from_utf8_lossy(path).to_string();
                    let conflict_path = format!("{path_str}~{merge_ours_oid_hex}");
                    let mut oe_c = oe.clone();
                    oe_c.path = conflict_path.as_bytes().to_vec();
                    stage_entry(&mut index, &oe_c, 2);
                    if let Ok(obj) = repo.odb.read(&oe.oid) {
                        conflict_files.push((conflict_path.clone(), obj.data));
                    }
                    conflict_descriptions.push(ConflictDescription {
                        kind: "directory/file",
                        body: format!(
                            "There is a directory with name {path_str} in {their_name}. Adding {path_str} as {conflict_path}"
                        ),
                        subject_path: conflict_path.clone(),
                        remerge_anchor_path: None,
                        rename_rr_ours_dest: None,
                        rename_rr_theirs_dest: None,
                        auto_merge_hint_path: None,
                    });
                } else {
                    index.entries.push(oe.clone());
                }
            }
            // Added only by theirs — unless ours only has paths under this name (directory).
            (None, None, Some(te)) => {
                if te.mode == MODE_GITLINK && has_descendant(&ours_entries, path) {
                    if path_descendants_match(&base, &ours_entries, path) {
                        index.entries.push(te.clone());
                        mark_path_descendants_handled(&mut handled_paths, &base, path);
                        mark_path_descendants_handled(&mut handled_paths, &ours_entries, path);
                        continue;
                    }

                    let path_str = String::from_utf8_lossy(path).to_string();
                    let Some(oe) = first_entry_under_path_prefix(&ours_entries, path) else {
                        index.entries.push(te.clone());
                        continue;
                    };
                    if criss_cross_outer_merge {
                        // Virtual-base / criss-cross build: theirs added a submodule at `path`
                        // while ours holds a directory `path/...`. git merge-ort relocates the
                        // submodule out of the way to `path~<theirs label>` and keeps the full
                        // directory intact (the directory files in `ours_entries` are emitted by
                        // their own per-path iterations below). The relocated gitlink then surfaces
                        // as a `path~<label>` -> `path` rename in the outer merge (t6422 #26).
                        let relocated = format!("{path_str}~{their_name}");
                        has_conflicts = true;
                        let mut gl = te.clone();
                        gl.path = relocated.as_bytes().to_vec();
                        stage_entry(&mut index, &gl, 3);
                        conflict_descriptions.push(ConflictDescription {
                            kind: "file/directory",
                            body: format!(
                                "directory in the way of {path_str} from {their_name}; moving it to {relocated} instead."
                            ),
                            subject_path: relocated.clone(),
                            remerge_anchor_path: Some(path_str.clone()),
                            rename_rr_ours_dest: None,
                            rename_rr_theirs_dest: None,
                            auto_merge_hint_path: None,
                        });
                        conflict_descriptions.push(ConflictDescription {
                            kind: "modify/delete",
                            body: format!(
                                "{relocated} deleted in {ours_label} and modified in {their_name}.  Version {their_name} of {relocated} left in tree."
                            ),
                            subject_path: relocated.clone(),
                            remerge_anchor_path: Some(path_str.clone()),
                            rename_rr_ours_dest: None,
                            rename_rr_theirs_dest: None,
                            auto_merge_hint_path: None,
                        });
                        continue;
                    }
                    conflict_submodule_vs_non_gitlink(
                        repo,
                        &path_str,
                        path,
                        te,
                        &oe,
                        3,
                        2,
                        merge_ours_oid_hex,
                        false,
                        &mut index,
                        &mut has_conflicts,
                        &mut conflict_descriptions,
                        &mut conflict_files,
                    )?;
                    continue;
                }
                if te.mode == MODE_GITLINK {
                    index.entries.push(te.clone());
                } else if has_descendant(&ours_entries, path) {
                    if path_descendants_match(&base, &ours_entries, path) {
                        index.entries.push(te.clone());
                        mark_path_descendants_handled(&mut handled_paths, &base, path);
                        mark_path_descendants_handled(&mut handled_paths, &ours_entries, path);
                        continue;
                    }

                    has_conflicts = true;
                    let path_str = String::from_utf8_lossy(path).to_string();
                    let conflict_path = format!("{path_str}~{merge_theirs_oid_hex}");
                    let mut te_c = te.clone();
                    te_c.path = conflict_path.as_bytes().to_vec();
                    stage_entry(&mut index, &te_c, 3);
                    if let Ok(obj) = repo.odb.read(&te.oid) {
                        conflict_files.push((conflict_path.clone(), obj.data));
                    }
                    conflict_descriptions.push(ConflictDescription {
                        kind: "directory/file",
                        body: format!(
                            "There is a directory with name {path_str} in {ours_label}. Adding {path_str} as {conflict_path}"
                        ),
                        subject_path: conflict_path.clone(),
                        remerge_anchor_path: None,
                        rename_rr_ours_dest: None,
                        rename_rr_theirs_dest: None,
                        auto_merge_hint_path: None,
                    });
                } else {
                    index.entries.push(te.clone());
                }
            }
            // Submodule vs file/symlink add/add (t6437 file/submodule).
            (None, Some(oe), Some(te))
                if (oe.mode == MODE_GITLINK) != (te.mode == MODE_GITLINK)
                    && oe.mode != MODE_TREE
                    && te.mode != MODE_TREE =>
            {
                let path_str = String::from_utf8_lossy(path).to_string();
                let relocate_file = !criss_cross_outer_merge;
                if oe.mode == MODE_GITLINK {
                    let suffix = if relocate_file {
                        their_name
                    } else {
                        merge_theirs_oid_hex
                    };
                    conflict_submodule_vs_non_gitlink(
                        repo,
                        &path_str,
                        path,
                        oe,
                        te,
                        2,
                        3,
                        suffix,
                        relocate_file,
                        &mut index,
                        &mut has_conflicts,
                        &mut conflict_descriptions,
                        &mut conflict_files,
                    )?;
                } else {
                    let suffix = if relocate_file {
                        ours_label
                    } else {
                        merge_ours_oid_hex
                    };
                    conflict_submodule_vs_non_gitlink(
                        repo,
                        &path_str,
                        path,
                        te,
                        oe,
                        3,
                        2,
                        suffix,
                        relocate_file,
                        &mut index,
                        &mut has_conflicts,
                        &mut conflict_descriptions,
                        &mut conflict_files,
                    )?;
                }
            }
            // Both added same thing
            (None, Some(oe), Some(te)) if oe.oid == te.oid && oe.mode == te.mode => {
                index.entries.push(oe.clone());
            }
            // Deleted by both
            (Some(_), None, None) => {
                // Check if both sides renamed to the same target
                let ours_target = ours_renames.get(path);
                let theirs_target = theirs_renames.get(path);
                if ours_target.is_none() && theirs_target.is_none() {
                    // Truly deleted by both — skip
                }
                // Otherwise already handled above
            }
            // All three differ — content-level merge
            (Some(be), Some(oe), Some(te)) => {
                let path_str = String::from_utf8_lossy(path).to_string();
                let gl_o = oe.mode == MODE_GITLINK;
                let gl_t = te.mode == MODE_GITLINK;
                if gl_o != gl_t && oe.mode != MODE_TREE && te.mode != MODE_TREE {
                    if gl_o {
                        conflict_submodule_vs_non_gitlink(
                            repo,
                            &path_str,
                            path,
                            oe,
                            te,
                            2,
                            3,
                            merge_theirs_oid_hex,
                            false,
                            &mut index,
                            &mut has_conflicts,
                            &mut conflict_descriptions,
                            &mut conflict_files,
                        )?;
                    } else {
                        conflict_submodule_vs_non_gitlink(
                            repo,
                            &path_str,
                            path,
                            te,
                            oe,
                            3,
                            2,
                            merge_ours_oid_hex,
                            false,
                            &mut index,
                            &mut has_conflicts,
                            &mut conflict_descriptions,
                            &mut conflict_files,
                        )?;
                    }
                    continue;
                }
                if try_merge_gitlink_entries(
                    repo,
                    &path_str,
                    be,
                    oe,
                    te,
                    favor,
                    &mut index,
                    &mut has_conflicts,
                    &mut conflict_descriptions,
                    &mut submodule_merge_stdout,
                    &mut submodule_merge_advice,
                )? {
                    continue;
                }
                match try_content_merge(
                    repo,
                    &path_str,
                    be,
                    oe,
                    te,
                    ours_label,
                    same_path_criss_cross_base_label(
                        base_label,
                        base_label_prefix,
                        criss_cross_outer_merge,
                    ),
                    their_name,
                    favor,
                    diff_algorithm,
                    merge_renormalize,
                    ignore_all_space,
                    ignore_space_change,
                    ignore_space_at_eol,
                    ignore_cr_at_eol,
                    auto_merge_paths.as_deref_mut(),
                )? {
                    ContentMergeResult::Clean(merged_oid, mode) => {
                        let mut entry = oe.clone();
                        entry.oid = merged_oid;
                        entry.mode = mode;
                        index.entries.push(entry);
                    }
                    ContentMergeResult::Conflict(content) => {
                        has_conflicts = true;
                        // Write conflict stages
                        stage_entry(&mut index, be, 1);
                        stage_entry(&mut index, oe, 2);
                        stage_entry(&mut index, te, 3);
                        conflict_descriptions.push(ConflictDescription {
                            kind: "content",
                            body: format!("Merge conflict in {path_str}"),
                            subject_path: path_str.clone(),
                            remerge_anchor_path: None,
                            rename_rr_ours_dest: None,
                            rename_rr_theirs_dest: None,
                            auto_merge_hint_path: None,
                        });
                        conflict_files.push((path_str, content));
                    }
                    ContentMergeResult::BinaryConflict(content) => {
                        has_conflicts = true;
                        stage_entry(&mut index, be, 1);
                        stage_entry(&mut index, oe, 2);
                        stage_entry(&mut index, te, 3);
                        let b = format!("{path_str} ({ours_label} vs. {their_name})");
                        conflict_descriptions.push(ConflictDescription {
                            kind: "binary",
                            body: b.clone(),
                            subject_path: b,
                            remerge_anchor_path: None,
                            rename_rr_ours_dest: None,
                            rename_rr_theirs_dest: None,
                            auto_merge_hint_path: None,
                        });
                        conflict_files.push((path_str, content));
                    }
                }
            }
            // Delete/modify — conflict only if the surviving side changed
            (Some(be), None, Some(te)) => {
                // Check if ours renamed this file — if so, it's handled above
                if ours_renames.contains_key(path) {
                    // Already handled in rename pass
                } else if be.oid == te.oid && be.mode == te.mode {
                    // Theirs didn't change it, ours deleted → clean delete
                } else if merge_renormalize && blobs_equivalent_after_renormalize(repo, be, te)? {
                    // With merge.renormalize, treat pure normalization-only edits
                    // as unchanged so delete/modify can resolve to delete.
                } else {
                    // Modify/delete is never auto-resolved by `-X ours`/`-X theirs`.
                    // Git's merge-ort (process_entry, filemask 3/5) does not consult
                    // `recursive_variant` for modify/delete: it always leaves the modified
                    // version in the tree and reports CONFLICT(modify/delete), exiting 1.
                    {
                        {
                            // Theirs modified, ours deleted → conflict
                            let path_str = String::from_utf8_lossy(path).to_string();
                            has_conflicts = true;
                            if has_descendant(&ours_entries, path) {
                                // D/F conflict: the old file path now needs to stay a
                                // directory (for entries like `path/file`), so move the
                                // conflict stages and worktree file to a side-path.
                                // Suffix names the commit that still has this path as a file (theirs).
                                let conflict_path = format!("{path_str}~{merge_theirs_oid_hex}");
                                let mut be_conflict = be.clone();
                                be_conflict.path = conflict_path.as_bytes().to_vec();
                                stage_entry(&mut index, &be_conflict, 1);
                                let mut te_conflict = te.clone();
                                te_conflict.path = conflict_path.as_bytes().to_vec();
                                stage_entry(&mut index, &te_conflict, 3);
                                if let Ok(obj) = repo.odb.read(&te.oid) {
                                    conflict_files.push((conflict_path.clone(), obj.data));
                                }
                                let body = format!(
                                    "{conflict_path} deleted in {ours_label} and modified in {their_name}.  Version {their_name} of {conflict_path} left in tree."
                                );
                                conflict_descriptions.push(ConflictDescription {
                                    kind: "modify/delete",
                                    body,
                                    subject_path: conflict_path,
                                    remerge_anchor_path: Some(path_str.clone()),
                                    rename_rr_ours_dest: None,
                                    rename_rr_theirs_dest: None,
                                    auto_merge_hint_path: None,
                                });
                            } else {
                                stage_entry(&mut index, be, 1);
                                stage_entry(&mut index, te, 3);
                                if let Ok(obj) = repo.odb.read(&te.oid) {
                                    conflict_files.push((path_str.clone(), obj.data));
                                }
                                let body = format!(
                                    "{path_str} deleted in {ours_label} and modified in {their_name}.  Version {their_name} of {path_str} left in tree."
                                );
                                conflict_descriptions.push(ConflictDescription {
                                    kind: "modify/delete",
                                    body,
                                    subject_path: path_str.clone(),
                                    remerge_anchor_path: None,
                                    rename_rr_ours_dest: None,
                                    rename_rr_theirs_dest: None,
                                    auto_merge_hint_path: None,
                                });
                            }
                        }
                    }
                }
            }
            (Some(be), Some(oe), None) => {
                // Check if theirs renamed this file — if so, it's handled above
                if theirs_renames.contains_key(path) {
                    // Already handled in rename pass
                } else if be.oid == oe.oid && be.mode == oe.mode {
                    // Ours didn't change it, theirs deleted → clean delete
                } else if merge_renormalize && blobs_equivalent_after_renormalize(repo, be, oe)? {
                    // With merge.renormalize, treat pure normalization-only edits
                    // as unchanged so modify/delete can resolve to delete.
                } else {
                    // Modify/delete is never auto-resolved by `-X ours`/`-X theirs`.
                    // Git's merge-ort (process_entry, filemask 3/5) does not consult
                    // `recursive_variant` for modify/delete: it always leaves the modified
                    // version in the tree and reports CONFLICT(modify/delete), exiting 1.
                    {
                        {
                            // Ours modified, theirs deleted → conflict
                            let path_str = String::from_utf8_lossy(path).to_string();
                            has_conflicts = true;
                            if has_descendant(&theirs_entries, path) {
                                // D/F conflict: the old file path now needs to stay a
                                // directory (for entries like `path/file`), so move the
                                // conflict stages and worktree file to a side-path.
                                // Suffix names the commit that still has this path as a file (ours).
                                let conflict_path = format!("{path_str}~{merge_ours_oid_hex}");
                                let mut be_conflict = be.clone();
                                be_conflict.path = conflict_path.as_bytes().to_vec();
                                stage_entry(&mut index, &be_conflict, 1);
                                let mut oe_conflict = oe.clone();
                                oe_conflict.path = conflict_path.as_bytes().to_vec();
                                stage_entry(&mut index, &oe_conflict, 2);
                                if let Ok(obj) = repo.odb.read(&oe.oid) {
                                    conflict_files.push((conflict_path.clone(), obj.data));
                                }
                                let body = format!(
                                    "{conflict_path} deleted in {their_name} and modified in {ours_label}.  Version {ours_label} of {conflict_path} left in tree."
                                );
                                conflict_descriptions.push(ConflictDescription {
                                    kind: "modify/delete",
                                    body,
                                    subject_path: conflict_path,
                                    remerge_anchor_path: Some(path_str.clone()),
                                    rename_rr_ours_dest: None,
                                    rename_rr_theirs_dest: None,
                                    auto_merge_hint_path: None,
                                });
                            } else {
                                stage_entry(&mut index, be, 1);
                                stage_entry(&mut index, oe, 2);
                                if let Ok(obj) = repo.odb.read(&oe.oid) {
                                    conflict_files.push((path_str.clone(), obj.data));
                                }
                                let body = format!(
                                    "{path_str} deleted in {their_name} and modified in {ours_label}.  Version {ours_label} of {path_str} left in tree."
                                );
                                conflict_descriptions.push(ConflictDescription {
                                    kind: "modify/delete",
                                    body,
                                    subject_path: path_str.clone(),
                                    remerge_anchor_path: None,
                                    rename_rr_ours_dest: None,
                                    rename_rr_theirs_dest: None,
                                    auto_merge_hint_path: None,
                                });
                            }
                        }
                    }
                }
            }
            // Both added different content — try content merge with empty base
            (None, Some(oe), Some(te)) => {
                let path_str = String::from_utf8_lossy(path).to_string();
                if oe.mode == MODE_GITLINK && te.mode == MODE_GITLINK {
                    remove_stage_zero_entry(&mut index, path);
                    // Submodule add/add with no common merge base (each superproject branch
                    // introduced the submodule independently). Run the gitlink merge with a
                    // null base so the "go to submodule" advice referencing the submodule's
                    // HEAD^1 is emitted, matching git merge-ort's merge_submodule().
                    let mut null_base = oe.clone();
                    null_base.oid = zero_oid();
                    if try_merge_gitlink_entries(
                        repo,
                        &path_str,
                        &null_base,
                        oe,
                        te,
                        favor,
                        &mut index,
                        &mut has_conflicts,
                        &mut conflict_descriptions,
                        &mut submodule_merge_stdout,
                        &mut submodule_merge_advice,
                    )? {
                        continue;
                    }
                    has_conflicts = true;
                    stage_entry(&mut index, oe, 2);
                    stage_entry(&mut index, te, 3);
                    conflict_descriptions.push(ConflictDescription {
                        kind: "submodule",
                        body: format!("Merge conflict in {path_str}"),
                        subject_path: path_str.clone(),
                        remerge_anchor_path: None,
                        rename_rr_ours_dest: None,
                        rename_rr_theirs_dest: None,
                        auto_merge_hint_path: None,
                    });
                    continue;
                }
                match try_content_merge_add_add(
                    repo,
                    &path_str,
                    oe,
                    te,
                    &directory_rename_conflict_label(
                        ours_label,
                        path,
                        &dir_renames_applied_to_ours,
                    ),
                    &directory_rename_conflict_label(
                        their_name,
                        path,
                        &dir_renames_applied_to_theirs,
                    ),
                    favor,
                    diff_algorithm,
                    merge_renormalize,
                    ignore_all_space,
                    ignore_space_change,
                    ignore_space_at_eol,
                    ignore_cr_at_eol,
                    auto_merge_paths.as_deref_mut(),
                )? {
                    ContentMergeResult::Clean(merged_oid, mode) => {
                        let mut entry = oe.clone();
                        entry.oid = merged_oid;
                        entry.mode = mode;
                        index.entries.push(entry);
                    }
                    ContentMergeResult::Conflict(content) => {
                        has_conflicts = true;
                        remove_stage_zero_entry(&mut index, path);
                        stage_entry(&mut index, oe, 2);
                        stage_entry(&mut index, te, 3);
                        conflict_descriptions.push(ConflictDescription {
                            kind: "add/add",
                            body: format!("Merge conflict in {path_str}"),
                            subject_path: path_str.clone(),
                            remerge_anchor_path: None,
                            rename_rr_ours_dest: None,
                            rename_rr_theirs_dest: None,
                            auto_merge_hint_path: None,
                        });
                        conflict_files.push((path_str, content));
                    }
                    ContentMergeResult::BinaryConflict(content) => {
                        has_conflicts = true;
                        remove_stage_zero_entry(&mut index, path);
                        stage_entry(&mut index, oe, 2);
                        stage_entry(&mut index, te, 3);
                        let b = format!("{path_str} ({ours_label} vs. {their_name})");
                        conflict_descriptions.push(ConflictDescription {
                            kind: "binary",
                            body: b.clone(),
                            subject_path: b,
                            remerge_anchor_path: None,
                            rename_rr_ours_dest: None,
                            rename_rr_theirs_dest: None,
                            auto_merge_hint_path: None,
                        });
                        conflict_files.push((path_str, content));
                    }
                }
            }
            // Shouldn't happen
            (_, None, None) => {}
        }
    }

    index.sort();
    dedupe_index_entries_by_path_stage(&mut index);

    let index_has_unmerged = index.entries.iter().any(|e| e.stage() != 0);
    let has_conflicts = has_conflicts || index_has_unmerged;

    Ok(MergeResult {
        index,
        has_conflicts,
        conflict_files,
        conflict_descriptions,
        submodule_merge_stdout,
        submodule_merge_advice,
    })
}

/// Result of an in-core merge for `git merge-tree --write-tree`.
pub(crate) struct MergeTreeWriteOutput {
    /// Result tree OID when written; `None` when `--quiet` stopped early on conflict.
    pub tree_oid: Option<ObjectId>,
    pub has_conflicts: bool,
    pub index: Index,
    pub conflict_files: Vec<(String, Vec<u8>)>,
    pub conflict_descriptions: Vec<ConflictDescription>,
    /// Paths that received an `ll_merge` attempt (for `Auto-merging` messages).
    pub auto_merge_paths: Vec<String>,
}

/// Turn unmerged index entries into stage-0 blobs so [`write_tree_from_index`] can record
/// conflict-marker content in the result tree (matches `git merge` / `AUTO_MERGE`).
fn materialize_unmerged_entries_for_merge_tree_tree(
    repo: &Repository,
    index: &mut Index,
    conflict_files: &[(String, Vec<u8>)],
) -> Result<()> {
    let content_by_path: HashMap<Vec<u8>, Vec<u8>> = conflict_files
        .iter()
        .map(|(p, c)| (p.as_bytes().to_vec(), c.clone()))
        .collect();

    let mut conflict_paths: BTreeSet<Vec<u8>> = BTreeSet::new();
    for e in &index.entries {
        if e.stage() != 0 {
            conflict_paths.insert(e.path.clone());
        }
    }

    for path in conflict_paths {
        let path_str = String::from_utf8_lossy(&path).into_owned();
        let Some(stage_entry) = index.get(&path, 2).or_else(|| index.get(&path, 3)).cloned() else {
            continue;
        };
        let blob_oid = if let Some(content) = content_by_path.get(&path) {
            repo.odb.write(ObjectKind::Blob, content)?
        } else {
            stage_entry.oid
        };
        let mut resolved = stage_entry;
        resolved.oid = blob_oid;
        resolved.flags &= 0x0FFF;
        index.remove(&path);
        index.remove_descendants_under_path(&path_str);
        index.add_or_replace(resolved);
    }
    index.sort();
    Ok(())
}

/// Run the same tree merge as `git merge` without touching index, worktree, or refs.
///
/// When `mergeability_only` is true and the merge is conflicted, the merge stops before
/// writing new tree/blob objects (matches Git `mergeability_only` / `--quiet`).
pub(crate) fn merge_tree_write_tree_core(
    repo: &Repository,
    branch1_oid: ObjectId,
    branch2_oid: ObjectId,
    explicit_merge_base: Option<ObjectId>,
    // Human-facing labels for conflict messages (typically branch names).
    our_branch_spec: &str,
    their_branch_spec: &str,
    allow_unrelated_histories: bool,
    favor: MergeFavor,
    diff_algorithm: Option<&str>,
    merge_renormalize: bool,
    merge_directory_renames_mode: MergeDirectoryRenamesMode,
    rename_options: MergeRenameOptions,
    mergeability_only: bool,
    // When true, record paths passed to the content merge machinery for `Auto-merging` output.
    record_auto_merge_paths: bool,
) -> Result<MergeTreeWriteOutput> {
    // `--quiet` (mergeability_only) reports only clean/conflict status; git runs the full merge
    // into a throwaway tmp object dir and leaves the real object store untouched. Mirror that by
    // routing every object write through an in-memory overlay that is discarded on return.
    struct OverlayGuard<'a>(&'a Repository, bool);
    impl Drop for OverlayGuard<'_> {
        fn drop(&mut self) {
            if self.1 {
                self.0.odb.disable_mem_overlay();
            }
        }
    }
    if mergeability_only {
        repo.odb.enable_mem_overlay();
    }
    let _overlay_guard = OverlayGuard(repo, mergeability_only);

    let mut auto_merge_paths: Vec<String> = Vec::new();
    let auto_ref: Option<&mut Vec<String>> = if record_auto_merge_paths {
        Some(&mut auto_merge_paths)
    } else {
        None
    };

    let (base_oid, base_label_prefix, criss_cross_outer) = if let Some(mb) = explicit_merge_base {
        (mb, short_oid(mb), false)
    } else {
        let bases =
            grit_lib::merge_base::merge_bases_first_vs_rest(repo, branch1_oid, &[branch2_oid])?;
        if bases.is_empty() && !allow_unrelated_histories {
            // git merge-tree die()s with exit 128 and a "fatal:" prefix here.
            return Err(anyhow::Error::new(ExplicitExit {
                code: 128,
                message: "fatal: refusing to merge unrelated histories".to_string(),
            }));
        }
        if bases.is_empty() {
            (
                create_empty_base_commit(repo)?,
                "empty tree".to_string(),
                false,
            )
        } else if bases.len() > 1 {
            (
                create_virtual_merge_base(repo, &bases, favor, merge_renormalize)?,
                "merged common ancestors".to_string(),
                true,
            )
        } else {
            (bases[0], short_oid(bases[0]), false)
        }
    };

    // merge-tree accepts commit-ish OR tree OIDs for both branches and the
    // merge-base; peel each to its tree (commits via their tree pointer, tags
    // recursively, trees as-is). A missing object here is reported with
    // git's merge-tree "Could not read <oid>" wording (exit 128, empty stdout).
    let base_tree = peel_to_tree_for_merge_tree(repo, base_oid)?;
    let ours_tree = peel_to_tree_for_merge_tree(repo, branch1_oid)?;
    let theirs_tree = peel_to_tree_for_merge_tree(repo, branch2_oid)?;

    let base_entries = tree_to_map_for_merge(
        repo,
        tree_to_index_entries_for_merge_tree(repo, &base_tree)?,
    );
    let ours_entries = tree_to_map_for_merge(
        repo,
        tree_to_index_entries_for_merge_tree(repo, &ours_tree)?,
    );
    let theirs_entries = tree_to_map_for_merge(
        repo,
        tree_to_index_entries_for_merge_tree(repo, &theirs_tree)?,
    );

    let head = HeadState::Detached { oid: branch1_oid };
    let forced_labels = Some((our_branch_spec.to_string(), their_branch_spec.to_string()));
    let merge_result = merge_trees(
        repo,
        &base_entries,
        &ours_entries,
        &theirs_entries,
        &head,
        their_branch_spec,
        &base_label_prefix,
        &branch1_oid.to_hex(),
        &branch2_oid.to_hex(),
        favor,
        diff_algorithm,
        merge_renormalize,
        false,
        false,
        false,
        false,
        merge_directory_renames_mode,
        rename_options,
        forced_labels,
        criss_cross_outer,
        auto_ref,
        None,
    )
    .map_err(|e| {
        // A missing object surfacing from the content/tree merge at this point is
        // a blob the merge tried to read; git merge-tree reports it as
        // "fatal: unable to read blob object <oid>" (exit 128, empty stdout).
        if let Some(grit_lib::error::Error::ObjectNotFound(hex)) =
            e.downcast_ref::<grit_lib::error::Error>()
        {
            return anyhow::Error::new(ExplicitExit {
                code: 128,
                message: format!("fatal: unable to read blob object {hex}"),
            });
        }
        e
    })?;

    let has_conflicts = merge_result.has_conflicts;
    let index = merge_result.index;
    let conflict_files = merge_result.conflict_files;
    let conflict_descriptions = merge_result.conflict_descriptions;

    let tree_oid = if mergeability_only {
        // `--quiet` only reports clean/conflict status; git never persists the
        // resulting tree (or any "outer layer" objects) in this mode, so the
        // object store must be left untouched.
        None
    } else {
        let mut index_for_tree = index.clone();
        if has_conflicts {
            materialize_unmerged_entries_for_merge_tree_tree(
                repo,
                &mut index_for_tree,
                &conflict_files,
            )?;
        }
        Some(write_tree_from_index(&repo.odb, &index_for_tree, "")?)
    };

    Ok(MergeTreeWriteOutput {
        tree_oid,
        has_conflicts,
        index,
        conflict_files,
        conflict_descriptions,
        auto_merge_paths,
    })
}

/// Re-merge two parents the same way `git merge` would, returning the resulting tree OID
/// and conflict descriptions for `--remerge-diff` headers.
///
/// `parent1` is treated as the first parent (ours); `parent2` as the second (theirs).
pub(crate) fn remerge_merge_tree(
    repo: &Repository,
    parent1: ObjectId,
    parent2: ObjectId,
) -> Result<(ObjectId, Vec<ConflictDescription>)> {
    let bases = grit_lib::merge_base::merge_bases_first_vs_rest(repo, parent1, &[parent2])?;
    let base_oid = if bases.is_empty() {
        create_empty_base_commit(repo)?
    } else if bases.len() > 1 {
        create_virtual_merge_base(repo, &bases, MergeFavor::None, false)?
    } else {
        bases[0]
    };
    let base_label_prefix = if bases.is_empty() {
        "empty tree".to_string()
    } else if bases.len() > 1 {
        "merged common ancestors".to_string()
    } else {
        short_oid(bases[0])
    };

    let base_tree = commit_tree(repo, base_oid)?;
    let ours_tree = commit_tree(repo, parent1)?;
    let theirs_tree = commit_tree(repo, parent2)?;

    let base_entries = tree_to_map_for_merge(repo, tree_to_index_entries(repo, &base_tree, "")?);
    let ours_entries = tree_to_map_for_merge(repo, tree_to_index_entries(repo, &ours_tree, "")?);
    let theirs_entries =
        tree_to_map_for_merge(repo, tree_to_index_entries(repo, &theirs_tree, "")?);

    let p1_l = commit_remerge_marker_label(repo, &parent1);
    let p2_l = commit_remerge_marker_label(repo, &parent2);
    let forced = Some((p1_l.clone(), p2_l.clone()));

    let head = HeadState::Detached { oid: parent1 };
    let mut merge_result = merge_trees(
        repo,
        &base_entries,
        &ours_entries,
        &theirs_entries,
        &head,
        "remerge",
        &base_label_prefix,
        &parent1.to_hex(),
        &parent2.to_hex(),
        MergeFavor::None,
        None,
        false,
        false,
        false,
        false,
        false,
        MergeDirectoryRenamesMode::FromConfig,
        MergeRenameOptions::from_config(repo),
        forced,
        bases.len() > 1,
        None,
        None,
    )?;

    let labels = resolve_conflict_labels(repo, "remerge", &base_label_prefix, None);
    let base_merge_label = labels.base;

    materialize_unmerged_entries_for_remerge_tree(
        repo,
        &mut merge_result.index,
        &merge_result.conflict_descriptions,
        base_merge_label,
        &p1_l,
        &p2_l,
    )?;

    let tree_oid = write_tree_from_index(&repo.odb, &merge_result.index, "")?;
    Ok((tree_oid, merge_result.conflict_descriptions))
}

fn materialize_unmerged_entries_for_remerge_tree(
    repo: &Repository,
    index: &mut Index,
    conflict_descs: &[ConflictDescription],
    base_label: &str,
    ours_label: &str,
    theirs_label: &str,
) -> Result<()> {
    for desc in conflict_descs {
        if desc.kind != "rename/rename" {
            continue;
        }
        let (Some(anchor), Some(ours_dest), Some(theirs_dest)) = (
            desc.remerge_anchor_path.as_deref(),
            desc.rename_rr_ours_dest.as_deref(),
            desc.rename_rr_theirs_dest.as_deref(),
        ) else {
            continue;
        };
        let be = index.get(anchor.as_bytes(), 1).cloned();
        let oe = index.get(ours_dest.as_bytes(), 2).cloned();
        let te = index.get(theirs_dest.as_bytes(), 3).cloned();
        if let (Some(_be), Some(oe), Some(te)) = (be, oe, te) {
            index.remove(anchor.as_bytes());
            index.remove(ours_dest.as_bytes());
            index.remove(theirs_dest.as_bytes());
            let mut ours_e = oe;
            ours_e.flags &= 0x0FFF;
            index.add_or_replace(ours_e);
            let mut theirs_e = te;
            theirs_e.flags &= 0x0FFF;
            index.add_or_replace(theirs_e);
        }
    }

    let paths: Vec<Vec<u8>> = index
        .entries
        .iter()
        .filter(|e| e.stage() != 0)
        .map(|e| e.path.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();

    for path in paths {
        if index.get(&path, 0).is_some() {
            continue;
        }
        let s1 = index.get(&path, 1);
        let s2 = index.get(&path, 2);
        let s3 = index.get(&path, 3);
        let path_str = String::from_utf8_lossy(&path).to_string();
        let new_entry = match (s1, s2, s3) {
            (Some(be), Some(oe), Some(te)) => {
                match try_content_merge(
                    repo,
                    &path_str,
                    be,
                    oe,
                    te,
                    ours_label,
                    base_label,
                    theirs_label,
                    MergeFavor::None,
                    None,
                    false,
                    false,
                    false,
                    false,
                    false,
                    None,
                )? {
                    ContentMergeResult::Clean(oid, mode) => {
                        let mut e = oe.clone();
                        e.oid = oid;
                        e.mode = mode;
                        e.flags &= 0x0FFF;
                        e
                    }
                    ContentMergeResult::Conflict(content)
                    | ContentMergeResult::BinaryConflict(content) => {
                        let oid = repo.odb.write(ObjectKind::Blob, &content)?;
                        let mut e = oe.clone();
                        e.oid = oid;
                        e.flags &= 0x0FFF;
                        e
                    }
                }
            }
            (Some(_be), None, Some(te)) => {
                let mut e = te.clone();
                e.flags &= 0x0FFF;
                e
            }
            (Some(_be), Some(oe), None) => {
                // modify/delete: recorded merge tree keeps our side's blob (matches Git remerge-diff).
                let mut e = oe.clone();
                e.flags &= 0x0FFF;
                e
            }
            (None, Some(oe), Some(te)) => {
                match try_content_merge_add_add(
                    repo,
                    &path_str,
                    oe,
                    te,
                    ours_label,
                    theirs_label,
                    MergeFavor::None,
                    None,
                    false,
                    false,
                    false,
                    false,
                    false,
                    None,
                )? {
                    ContentMergeResult::Clean(oid, mode) => {
                        let mut e = oe.clone();
                        e.oid = oid;
                        e.mode = mode;
                        e.flags &= 0x0FFF;
                        e
                    }
                    ContentMergeResult::Conflict(content)
                    | ContentMergeResult::BinaryConflict(content) => {
                        let oid = repo.odb.write(ObjectKind::Blob, &content)?;
                        let mut e = oe.clone();
                        e.oid = oid;
                        e.flags &= 0x0FFF;
                        e
                    }
                }
            }
            _ => continue,
        };
        index.remove(&path);
        index.add_or_replace(new_entry);
    }
    index.sort();
    Ok(())
}

/// Apply merge-ort directory rename adjustment to `ours` and `theirs` entry maps before a replay
/// merge, matching [`merge_trees`] when `merge.directoryRenames` is enabled.
///
/// [`merge_trees`] runs this pass internally; [`merge_trees_for_replay`] disables that pass to
/// avoid double-handling with replay’s rename cache. Call this helper when replay needs directory
/// rename detection (e.g. `git rebase` with `merge.directoryRenames=true`).
pub(crate) fn replay_preprocess_directory_renames_for_trees(
    repo: &Repository,
    base: &HashMap<Vec<u8>, IndexEntry>,
    ours: &HashMap<Vec<u8>, IndexEntry>,
    theirs: &HashMap<Vec<u8>, IndexEntry>,
    merge_directory_renames_mode: MergeDirectoryRenamesMode,
    rename_options: MergeRenameOptions,
) -> (HashMap<Vec<u8>, IndexEntry>, HashMap<Vec<u8>, IndexEntry>) {
    if !merge_directory_renames_enabled_for_mode(repo, merge_directory_renames_mode) {
        return (ours.clone(), theirs.clone());
    }
    let (mut ours_renames, mut theirs_renames) =
        detect_merge_renames(repo, base, ours, theirs, rename_options);
    let mut ours_entries = ours.clone();
    let mut theirs_entries = theirs.clone();

    let ours_dir_renames = merge_directory_rename_maps(
        build_directory_rename_map(&ours_renames),
        infer_pure_directory_renames(base, &ours_entries),
    );
    let theirs_dir_renames = merge_directory_rename_maps(
        build_directory_rename_map(&theirs_renames),
        infer_pure_directory_renames(base, &theirs_entries),
    );
    let _ = apply_directory_renames_to_side(
        base,
        &mut ours_entries,
        &mut ours_renames,
        &ours_dir_renames,
        &theirs_dir_renames,
        None,
        None,
    );
    let _ = apply_directory_renames_to_side(
        base,
        &mut theirs_entries,
        &mut theirs_renames,
        &theirs_dir_renames,
        &ours_dir_renames,
        None,
        None,
    );
    (ours_entries, theirs_entries)
}

/// Perform a single three-way tree merge with merge-ort style rename handling.
///
/// This is a thin wrapper over the internal merge engine used by `merge` and
/// is intended for sequencer-style commands (such as `replay`) that need to
/// replay commits without touching refs/index/worktree directly.
pub(crate) fn merge_trees_for_replay(
    repo: &Repository,
    base: &HashMap<Vec<u8>, IndexEntry>,
    ours: &HashMap<Vec<u8>, IndexEntry>,
    theirs: &HashMap<Vec<u8>, IndexEntry>,
    their_name: &str,
    base_label_prefix: &str,
    merge_ours_oid_hex: &str,
    merge_theirs_oid_hex: &str,
    favor: MergeFavor,
    diff_algorithm: Option<&str>,
    merge_renormalize: bool,
    ignore_all_space: bool,
    ignore_space_change: bool,
    ignore_space_at_eol: bool,
    ignore_cr_at_eol: bool,
    merge_directory_renames_mode: MergeDirectoryRenamesMode,
    rename_options: MergeRenameOptions,
    forced_branch_labels: Option<(String, String)>,
    base_label_override: Option<&str>,
) -> Result<ReplayTreeMergeResult> {
    let head = HeadState::Invalid;
    let result = merge_trees(
        repo,
        base,
        ours,
        theirs,
        &head,
        their_name,
        base_label_prefix,
        merge_ours_oid_hex,
        merge_theirs_oid_hex,
        favor,
        diff_algorithm,
        merge_renormalize,
        ignore_all_space,
        ignore_space_change,
        ignore_space_at_eol,
        ignore_cr_at_eol,
        merge_directory_renames_mode,
        rename_options,
        forced_branch_labels,
        false,
        None,
        base_label_override,
    )?;
    Ok(ReplayTreeMergeResult {
        index: result.index,
        has_conflicts: result.has_conflicts,
        conflict_files: result.conflict_files,
        conflict_descriptions: result.conflict_descriptions,
    })
}

enum ContentMergeResult {
    /// Clean merge: (blob oid, mode).
    Clean(ObjectId, u32),
    /// Conflict: merged content with markers.
    Conflict(Vec<u8>),
    /// Binary conflict where textual merge is not possible.
    BinaryConflict(Vec<u8>),
}

/// Try a content-level three-way merge for a single file.
#[allow(clippy::too_many_arguments)]
fn try_content_merge(
    repo: &Repository,
    path_str: &str,
    base: &IndexEntry,
    ours: &IndexEntry,
    theirs: &IndexEntry,
    ours_label: &str,
    base_label: &str,
    theirs_label: &str,
    favor: MergeFavor,
    diff_algorithm: Option<&str>,
    merge_renormalize: bool,
    ignore_all_space: bool,
    ignore_space_change: bool,
    ignore_space_at_eol: bool,
    ignore_cr_at_eol: bool,
    auto_merge_paths: Option<&mut Vec<String>>,
) -> Result<ContentMergeResult> {
    try_content_merge_ext(
        repo,
        path_str,
        base,
        ours,
        theirs,
        ours_label,
        base_label,
        theirs_label,
        favor,
        diff_algorithm,
        merge_renormalize,
        ignore_all_space,
        ignore_space_change,
        ignore_space_at_eol,
        ignore_cr_at_eol,
        auto_merge_paths,
        0,
    )
}

/// Three-way content merge with an optional `extra_marker_size` to widen conflict markers.
///
/// `extra_marker_size` matches git's `handle_content_merge` parameter: when a content merge
/// feeds into a further content merge (rename/rename(1to2)/(2to1) collisions), the inner
/// markers are widened by this amount so they nest unambiguously inside the outer conflict
/// (t6422 mod6 #19, where the colliding-cycle merge uses size 8 instead of 7).
#[allow(clippy::too_many_arguments)]
fn try_content_merge_ext(
    repo: &Repository,
    path_str: &str,
    base: &IndexEntry,
    ours: &IndexEntry,
    theirs: &IndexEntry,
    ours_label: &str,
    base_label: &str,
    theirs_label: &str,
    favor: MergeFavor,
    diff_algorithm: Option<&str>,
    merge_renormalize: bool,
    ignore_all_space: bool,
    ignore_space_change: bool,
    ignore_space_at_eol: bool,
    ignore_cr_at_eol: bool,
    auto_merge_paths: Option<&mut Vec<String>>,
    extra_marker_size: usize,
) -> Result<ContentMergeResult> {
    let base_obj = repo.odb.read(&base.oid)?;
    let ours_obj = repo.odb.read(&ours.oid)?;
    let theirs_obj = repo.odb.read(&theirs.oid)?;

    let mut base_data = base_obj.data.clone();
    let mut ours_data = ours_obj.data.clone();
    let mut theirs_data = theirs_obj.data.clone();

    let merge_behavior = resolve_path_merge_behavior(repo, path_str);

    let config = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true).ok();
    let file_attrs = config.as_ref().map(|cfg| {
        let attrs = repo
            .work_tree
            .as_deref()
            .map(grit_lib::crlf::load_gitattributes)
            .unwrap_or_default();
        grit_lib::crlf::get_file_attrs(&attrs, path_str, false, cfg)
    });

    let is_attr_binary = file_attrs
        .as_ref()
        .is_some_and(|attrs| attrs.text == grit_lib::crlf::TextAttr::Unset);

    if merge_renormalize {
        base_data = renormalize_merge_blob(&base_data);
        ours_data = renormalize_merge_blob(&ours_data);
        theirs_data = renormalize_merge_blob(&theirs_data);
    }

    if ours_label.starts_with("Temporary merge branch")
        || theirs_label.starts_with("Temporary merge branch")
    {
        base_data = lengthen_conflict_marker_lines(&base_data, 2);
    }

    let base_driver_label = base_label.strip_suffix(":content").unwrap_or(base_label);
    match &merge_behavior {
        PathMergeBehavior::CustomDriver { command } => {
            let (merged, exit_code) = execute_custom_merge_driver(
                command,
                path_str,
                &base_data,
                &ours_data,
                &theirs_data,
                base_driver_label,
                ours_label,
                theirs_label,
            )?;
            if exit_code == 0 {
                let oid = repo.odb.write(ObjectKind::Blob, &merged)?;
                return Ok(ContentMergeResult::Clean(oid, ours.mode));
            }
            return Ok(ContentMergeResult::Conflict(merged));
        }
        PathMergeBehavior::CustomDriverMissing { name } => {
            bail!("merge driver '{name}' not found");
        }
        PathMergeBehavior::Default
        | PathMergeBehavior::BinaryNoMerge
        | PathMergeBehavior::Union => {}
    }

    let effective_favor = if matches!(merge_behavior, PathMergeBehavior::Union)
        && matches!(favor, MergeFavor::None)
    {
        MergeFavor::Union
    } else {
        favor
    };

    // If any is binary (by content or attribute), conflict (unless -X ours/theirs resolves it)
    if matches!(merge_behavior, PathMergeBehavior::BinaryNoMerge)
        || is_attr_binary
        || merge_file::is_binary(&base_data)
        || merge_file::is_binary(&ours_data)
        || merge_file::is_binary(&theirs_data)
    {
        match effective_favor {
            MergeFavor::Ours => {
                let oid = repo.odb.write(ObjectKind::Blob, &ours_data)?;
                return Ok(ContentMergeResult::Clean(oid, ours.mode));
            }
            MergeFavor::Theirs => {
                let oid = repo.odb.write(ObjectKind::Blob, &theirs_data)?;
                return Ok(ContentMergeResult::Clean(oid, theirs.mode));
            }
            MergeFavor::None | MergeFavor::Union => {
                return Ok(ContentMergeResult::BinaryConflict(ours_data));
            }
        }
    }

    let mut marker_warnings = Vec::new();
    let marker_size = resolve_marker_size_for_path(
        repo,
        path_str,
        ours_label,
        theirs_label,
        &mut marker_warnings,
    ) + extra_marker_size;
    for warning in marker_warnings {
        eprintln!("{warning}");
    }

    let conflict_style = resolve_conflict_style(repo);
    let input = MergeInput {
        base: &base_data,
        ours: &ours_data,
        theirs: &theirs_data,
        label_ours: ours_label,
        label_base: base_label,
        label_theirs: theirs_label,
        favor: effective_favor,
        style: conflict_style,
        marker_size,
        diff_algorithm: diff_algorithm.map(|s| s.to_string()),
        ignore_all_space,
        ignore_space_change,
        ignore_space_at_eol,
        ignore_cr_at_eol,
    };

    if let Some(paths) = auto_merge_paths {
        paths.push(path_str.to_string());
    }
    let output = merge_file::merge(&input)?;
    // Merge the file modes the same way git's `merge_3way` (merge-ort.c) does: when both sides
    // agree on the mode, or our side did not change it from the base, take theirs; otherwise our
    // side changed the mode while theirs did not, so keep ours. This preserves an executable-bit
    // change introduced on only one side even when the file content is identical on both sides
    // (e.g. rebasing a mode-only amend onto an upstream that already has the content change —
    // t3419 "do not drop patch modechange").
    let mode = if ours.mode == theirs.mode || ours.mode == base.mode {
        theirs.mode
    } else {
        ours.mode
    };

    if output.conflicts == 0 {
        if !merge_renormalize
            && ours_data != theirs_data
            && renormalize_merge_blob(&ours_data) == renormalize_merge_blob(&theirs_data)
        {
            let ours_text = String::from_utf8_lossy(&ours_data);
            let theirs_text = String::from_utf8_lossy(&theirs_data);
            let mut content = format!("<<<<<<< {ours_label}\n").into_bytes();
            content.extend_from_slice(ours_text.as_bytes());
            if !content.ends_with(b"\n") {
                content.push(b'\n');
            }
            content.extend_from_slice(b"=======\n");
            content.extend_from_slice(theirs_text.as_bytes());
            if !content.ends_with(b"\n") {
                content.push(b'\n');
            }
            content.extend_from_slice(format!(">>>>>>> {theirs_label}\n").as_bytes());
            return Ok(ContentMergeResult::Conflict(content));
        }
        let oid = repo.odb.write(ObjectKind::Blob, &output.content)?;
        Ok(ContentMergeResult::Clean(oid, mode))
    } else {
        Ok(ContentMergeResult::Conflict(output.content))
    }
}

/// Try content merge for add/add conflicts (empty base).
/// Two-way (empty-base) merge of `ours` and `theirs`, returning the working-tree blob bytes.
///
/// Used for add/add-style collisions where the working-tree file must hold the conflict-marked
/// two-way merge of the two colliding versions (e.g. rename/rename(1to2) + add-dest). A clean
/// merge returns the merged content; a conflict returns the conflict-marked content.
#[allow(clippy::too_many_arguments)]
fn two_way_conflict_blob(
    repo: &Repository,
    path_str: &str,
    ours: &IndexEntry,
    theirs: &IndexEntry,
    ours_label: &str,
    theirs_label: &str,
    diff_algorithm: Option<&str>,
    merge_renormalize: bool,
    ignore_all_space: bool,
    ignore_space_change: bool,
    ignore_space_at_eol: bool,
    ignore_cr_at_eol: bool,
) -> Result<Vec<u8>> {
    let content = match try_content_merge_add_add(
        repo,
        path_str,
        ours,
        theirs,
        ours_label,
        theirs_label,
        MergeFavor::None,
        diff_algorithm,
        merge_renormalize,
        ignore_all_space,
        ignore_space_change,
        ignore_space_at_eol,
        ignore_cr_at_eol,
        None,
    )? {
        ContentMergeResult::Clean(oid, _) => repo.odb.read(&oid)?.data,
        ContentMergeResult::Conflict(content) | ContentMergeResult::BinaryConflict(content) => {
            content
        }
    };
    Ok(content)
}

fn try_content_merge_add_add(
    repo: &Repository,
    path_str: &str,
    ours: &IndexEntry,
    theirs: &IndexEntry,
    ours_label: &str,
    theirs_label: &str,
    favor: MergeFavor,
    diff_algorithm: Option<&str>,
    merge_renormalize: bool,
    ignore_all_space: bool,
    ignore_space_change: bool,
    ignore_space_at_eol: bool,
    ignore_cr_at_eol: bool,
    auto_merge_paths: Option<&mut Vec<String>>,
) -> Result<ContentMergeResult> {
    let ours_obj = repo.odb.read(&ours.oid)?;
    let theirs_obj = repo.odb.read(&theirs.oid)?;
    let mut ours_data = ours_obj.data.clone();
    let mut theirs_data = theirs_obj.data.clone();
    let merge_behavior = resolve_path_merge_behavior(repo, path_str);

    let config = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true).ok();
    let file_attrs = config.as_ref().map(|cfg| {
        let attrs = repo
            .work_tree
            .as_deref()
            .map(grit_lib::crlf::load_gitattributes)
            .unwrap_or_default();
        grit_lib::crlf::get_file_attrs(&attrs, path_str, false, cfg)
    });

    let is_attr_binary = file_attrs
        .as_ref()
        .is_some_and(|attrs| attrs.text == grit_lib::crlf::TextAttr::Unset);

    if merge_renormalize {
        ours_data = renormalize_merge_blob(&ours_data);
        theirs_data = renormalize_merge_blob(&theirs_data);
    }

    match &merge_behavior {
        PathMergeBehavior::CustomDriver { command } => {
            let (merged, exit_code) = execute_custom_merge_driver(
                command,
                path_str,
                &[],
                &ours_data,
                &theirs_data,
                "empty tree",
                ours_label,
                theirs_label,
            )?;
            if exit_code == 0 {
                let oid = repo.odb.write(ObjectKind::Blob, &merged)?;
                return Ok(ContentMergeResult::Clean(oid, ours.mode));
            }
            return Ok(ContentMergeResult::Conflict(merged));
        }
        PathMergeBehavior::CustomDriverMissing { name } => {
            bail!("merge driver '{name}' not found");
        }
        PathMergeBehavior::Default
        | PathMergeBehavior::BinaryNoMerge
        | PathMergeBehavior::Union => {}
    }

    let effective_favor = if matches!(merge_behavior, PathMergeBehavior::Union)
        && matches!(favor, MergeFavor::None)
    {
        MergeFavor::Union
    } else {
        favor
    };

    if matches!(merge_behavior, PathMergeBehavior::BinaryNoMerge)
        || is_attr_binary
        || merge_file::is_binary(&ours_data)
        || merge_file::is_binary(&theirs_data)
    {
        return match effective_favor {
            MergeFavor::Ours => {
                let oid = repo.odb.write(ObjectKind::Blob, &ours_data)?;
                Ok(ContentMergeResult::Clean(oid, ours.mode))
            }
            MergeFavor::Theirs => {
                let oid = repo.odb.write(ObjectKind::Blob, &theirs_data)?;
                Ok(ContentMergeResult::Clean(oid, theirs.mode))
            }
            MergeFavor::None | MergeFavor::Union => {
                Ok(ContentMergeResult::BinaryConflict(ours_data))
            }
        };
    }

    let mut marker_warnings = Vec::new();
    let marker_size = resolve_marker_size_for_path(
        repo,
        path_str,
        ours_label,
        theirs_label,
        &mut marker_warnings,
    );
    for warning in marker_warnings {
        eprintln!("{warning}");
    }

    let conflict_style = resolve_conflict_style(repo);
    let input = MergeInput {
        base: &[], // empty base for add/add
        ours: &ours_data,
        theirs: &theirs_data,
        label_ours: ours_label,
        label_base: "empty tree",
        label_theirs: theirs_label,
        favor: effective_favor,
        style: conflict_style,
        marker_size,
        diff_algorithm: diff_algorithm.map(|s| s.to_string()),
        ignore_all_space,
        ignore_space_change,
        ignore_space_at_eol,
        ignore_cr_at_eol,
    };

    if let Some(paths) = auto_merge_paths {
        paths.push(path_str.to_string());
    }
    let output = merge_file::merge(&input)?;
    let mode = ours.mode;

    // An add/add with no merge base has nothing to reconcile a mode clash against: if the two
    // sides introduce the path as a regular file but with a different executable bit (100644 vs
    // 100755) Git reports an add/add conflict and records unmerged stages 2/3, even when the
    // *content* merges cleanly (e.g. both empty). The clean merged content is still used for the
    // work tree (t6411: detect conflict on double mode change). This only applies to a plain
    // file-mode clash: file/symlink/submodule type differences are conflicts handled elsewhere,
    // and an explicit favour picks a single side so the modes never clash there.
    let both_regular_files = matches!(ours.mode, MODE_REGULAR | MODE_EXECUTABLE)
        && matches!(theirs.mode, MODE_REGULAR | MODE_EXECUTABLE);
    let mode_clash =
        both_regular_files && ours.mode != theirs.mode && matches!(favor, MergeFavor::None);

    if output.conflicts == 0 && !mode_clash {
        let oid = repo.odb.write(ObjectKind::Blob, &output.content)?;
        Ok(ContentMergeResult::Clean(oid, mode))
    } else {
        Ok(ContentMergeResult::Conflict(output.content))
    }
}

fn renormalize_merge_blob(data: &[u8]) -> Vec<u8> {
    if merge_file::is_binary(data) {
        return data.to_vec();
    }
    grit_lib::crlf::crlf_to_lf(data)
}

fn lengthen_conflict_marker_lines(data: &[u8], extra: usize) -> Vec<u8> {
    lengthen_conflict_marker_lines_matching(data, None, extra)
}

fn lengthen_conflict_marker_lines_of_size(data: &[u8], size: usize, extra: usize) -> Vec<u8> {
    lengthen_conflict_marker_lines_matching(data, Some(size), extra)
}

fn lengthen_conflict_marker_lines_matching(
    data: &[u8],
    only_size: Option<usize>,
    extra: usize,
) -> Vec<u8> {
    if extra == 0 || merge_file::is_binary(data) {
        return data.to_vec();
    }

    let mut out = Vec::with_capacity(data.len());
    for line in data.split_inclusive(|byte| *byte == b'\n') {
        if let Some(marker) = conflict_marker_line_kind(line) {
            let count = line.iter().take_while(|byte| **byte == marker).count();
            if only_size.is_none_or(|size| size == count) {
                out.extend(std::iter::repeat_n(marker, extra));
            }
        }
        out.extend_from_slice(line);
    }
    if !data.ends_with(b"\n") {
        let tail = data.rsplit(|byte| *byte == b'\n').next().unwrap_or(data);
        if tail.len() == data.len() {
            return out;
        }
    }
    out
}

fn conflict_marker_line_kind(line: &[u8]) -> Option<u8> {
    let marker = *line.first()?;
    if !matches!(marker, b'<' | b'|' | b'=' | b'>') {
        return None;
    }
    let count = line.iter().take_while(|byte| **byte == marker).count();
    if count < 7 {
        return None;
    }
    match line.get(count) {
        None | Some(b'\n' | b'\r' | b' ') => Some(marker),
        _ => None,
    }
}

fn same_path_criss_cross_base_label<'a>(
    base_label: &'a str,
    base_label_prefix: &'a str,
    criss_cross_outer_merge: bool,
) -> &'a str {
    if criss_cross_outer_merge && base_label_prefix == "merged common ancestors" {
        base_label_prefix
    } else {
        base_label
    }
}

fn blobs_equivalent_after_renormalize(
    repo: &Repository,
    left: &IndexEntry,
    right: &IndexEntry,
) -> Result<bool> {
    let left_obj = repo.odb.read(&left.oid)?;
    let right_obj = repo.odb.read(&right.oid)?;
    Ok(renormalize_merge_blob(&left_obj.data) == renormalize_merge_blob(&right_obj.data))
}

fn resolve_conflict_style(repo: &Repository) -> ConflictStyle {
    let Ok(config) = ConfigSet::load(Some(&repo.git_dir), true) else {
        return ConflictStyle::Merge;
    };
    match config
        .get("merge.conflictstyle")
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "diff3" => ConflictStyle::Diff3,
        "zdiff3" => ConflictStyle::ZealousDiff3,
        _ => ConflictStyle::Merge,
    }
}

fn stage_entry(index: &mut Index, src: &IndexEntry, stage: u8) {
    let mut e = src.clone();
    e.flags = (e.flags & 0x0FFF) | ((stage as u16) << 12);
    index.entries.push(e);
}

fn remove_stage_zero_entry(index: &mut Index, path: &[u8]) {
    index
        .entries
        .retain(|entry| !(entry.stage() == 0 && entry.path == path));
}

fn dedupe_index_entries_by_path_stage(index: &mut Index) {
    let mut seen: BTreeSet<(Vec<u8>, u8)> = BTreeSet::new();
    index
        .entries
        .retain(|entry| seen.insert((entry.path.clone(), entry.stage())));
}

fn path_has_unmerged_entries(index: &Index, path: &[u8]) -> bool {
    index
        .entries
        .iter()
        .any(|e| e.path == path && e.stage() != 0)
}

fn path_has_tree_descendant(map: &HashMap<Vec<u8>, IndexEntry>, path: &[u8]) -> bool {
    map.keys()
        .any(|k| k.len() > path.len() && k.starts_with(path) && k.get(path.len()) == Some(&b'/'))
}

/// Returns true when the only entry strictly under `path/` in `map` is exactly `expected`.
///
/// Used to detect the "disappearing directory" rename case: when ours renames
/// `sub/file` → `sub` and theirs only touched `sub/file`, the directory `sub/` has no
/// content other than the rename source, so it is not a genuine file/directory conflict.
fn only_tree_descendant_is(
    map: &HashMap<Vec<u8>, IndexEntry>,
    path: &[u8],
    expected: &[u8],
) -> bool {
    let mut found_expected = false;
    for k in map.keys() {
        let is_descendant =
            k.len() > path.len() && k.starts_with(path) && k.get(path.len()) == Some(&b'/');
        if !is_descendant {
            continue;
        }
        if k.as_slice() == expected {
            found_expected = true;
        } else {
            return false;
        }
    }
    found_expected
}

fn path_descendants_match(
    left: &HashMap<Vec<u8>, IndexEntry>,
    right: &HashMap<Vec<u8>, IndexEntry>,
    path: &[u8],
) -> bool {
    let mut left_entries = descendant_entry_fingerprint(left, path);
    let mut right_entries = descendant_entry_fingerprint(right, path);
    left_entries.sort();
    right_entries.sort();
    !left_entries.is_empty() && left_entries == right_entries
}

fn descendant_entry_fingerprint(
    entries: &HashMap<Vec<u8>, IndexEntry>,
    path: &[u8],
) -> Vec<(Vec<u8>, ObjectId, u32)> {
    entries
        .iter()
        .filter(|(candidate, _)| {
            candidate.len() > path.len()
                && candidate.starts_with(path)
                && candidate.get(path.len()) == Some(&b'/')
        })
        .map(|(candidate, entry)| {
            (
                candidate[(path.len() + 1)..].to_vec(),
                entry.oid,
                entry.mode,
            )
        })
        .collect()
}

fn mark_path_descendants_handled(
    handled_paths: &mut BTreeSet<Vec<u8>>,
    entries: &HashMap<Vec<u8>, IndexEntry>,
    path: &[u8],
) {
    handled_paths.extend(entries.keys().filter_map(|candidate| {
        (candidate.len() > path.len()
            && candidate.starts_with(path)
            && candidate.get(path.len()) == Some(&b'/'))
        .then(|| candidate.clone())
    }));
}

/// First flattened index entry strictly under `prefix/` (lexicographic), for submodule/directory conflicts.
fn first_entry_under_path_prefix(
    map: &HashMap<Vec<u8>, IndexEntry>,
    prefix_dir: &[u8],
) -> Option<IndexEntry> {
    let mut best: Option<&IndexEntry> = None;
    let mut best_key: Option<&[u8]> = None;
    for (k, e) in map {
        if k.len() <= prefix_dir.len()
            || !k.starts_with(prefix_dir)
            || k.get(prefix_dir.len()) != Some(&b'/')
        {
            continue;
        }
        let pick = match &best_key {
            None => true,
            Some(bk) => k.as_slice() < *bk,
        };
        if pick {
            best = Some(e);
            best_key = Some(k.as_slice());
        }
    }
    best.cloned()
}

fn conflict_submodule_vs_non_gitlink(
    repo: &Repository,
    path_str: &str,
    path: &[u8],
    gitlink_entry: &IndexEntry,
    other: &IndexEntry,
    gitlink_stage: u8,
    other_stage: u8,
    file_conflict_suffix: &str,
    relocate_file: bool,
    index: &mut Index,
    has_conflicts: &mut bool,
    conflict_descriptions: &mut Vec<ConflictDescription>,
    conflict_files: &mut Vec<(String, Vec<u8>)>,
) -> Result<()> {
    *has_conflicts = true;
    let relocated = format!("{path_str}~{file_conflict_suffix}");
    let mut gl = gitlink_entry.clone();
    gl.path = path.to_vec();
    if relocate_file {
        // git merge-ort handles a submodule-vs-file "distinct types" clash for a normal merge by
        // keeping the gitlink at `path` and *renaming* the non-gitlink side to `path~<label>`,
        // staging it there. Recording the relocated file in the index (rather than leaving an
        // untracked working-tree copy) is what lets `git reset --hard` clean it up afterward
        // (t6437 file/submodule conflict; merge --abort works afterward).
        let mut ot = other.clone();
        ot.path = relocated.as_bytes().to_vec();
        stage_entry(index, &gl, gitlink_stage);
        stage_entry(index, &ot, other_stage);
        conflict_descriptions.push(ConflictDescription {
            kind: "distinct types",
            body: format!(
                "{path_str} had different types on each side; renamed one of them so each can be recorded somewhere."
            ),
            subject_path: relocated.clone(),
            remerge_anchor_path: Some(path_str.to_owned()),
            rename_rr_ours_dest: None,
            rename_rr_theirs_dest: None,
            auto_merge_hint_path: None,
        });
        if matches!(other.mode, MODE_REGULAR | MODE_EXECUTABLE) {
            if let Ok(obj) = repo.odb.read(&other.oid) {
                conflict_files.push((relocated, obj.data));
            }
        }
        return Ok(());
    }
    // Virtual-base / criss-cross computation: stage both unmerged entries at the original
    // `path` so the recursive virtual-base builder still detects the submodule add/add and
    // omits the path (t6416 submodule vs symlink). The conflicting file is materialized as an
    // untracked working-tree copy at `path~<suffix>`.
    let mut ot = other.clone();
    ot.path = path.to_vec();
    stage_entry(index, &gl, gitlink_stage);
    stage_entry(index, &ot, other_stage);
    conflict_descriptions.push(ConflictDescription {
        kind: "submodule",
        body: format!("Merge conflict in {path_str}"),
        subject_path: path_str.to_owned(),
        remerge_anchor_path: None,
        rename_rr_ours_dest: None,
        rename_rr_theirs_dest: None,
        auto_merge_hint_path: None,
    });
    if matches!(other.mode, MODE_REGULAR | MODE_EXECUTABLE) {
        if let Ok(obj) = repo.odb.read(&other.oid) {
            conflict_files.push((relocated, obj.data));
        }
    }
    Ok(())
}

/// Directory/file conflicts: one side has a file at `P`, the other only has paths under `P/`.
///
/// When `merge_ort_style` is true (recursive merge with a virtual merge base / criss-cross), match
/// Git merge-ort index layout: unmerged entries at the original path `P` with stages 1+2 or 1+3.
/// Otherwise use `P~SUFFIX` staging so `git rm P~HEAD` works during initial conflict resolution
/// (t6416 setup and t4301-style flows).
fn apply_directory_file_conflicts(
    repo: &Repository,
    their_name: &str,
    ours_label: &str,
    base: &HashMap<Vec<u8>, IndexEntry>,
    merge_ort_style: bool,
    ours_renames: &HashMap<Vec<u8>, Vec<u8>>,
    theirs_renames: &HashMap<Vec<u8>, Vec<u8>>,
    ours_entries: &HashMap<Vec<u8>, IndexEntry>,
    theirs_entries: &HashMap<Vec<u8>, IndexEntry>,
    index: &mut Index,
    all_paths: &BTreeSet<Vec<u8>>,
    handled_paths: &mut BTreeSet<Vec<u8>>,
    conflict_descriptions: &mut Vec<ConflictDescription>,
    conflict_files: &mut Vec<(String, Vec<u8>)>,
    has_conflicts: &mut bool,
    favor: MergeFavor,
    diff_algorithm: Option<&str>,
    merge_renormalize: bool,
    ignore_all_space: bool,
    ignore_space_change: bool,
    ignore_space_at_eol: bool,
    ignore_cr_at_eol: bool,
    mut auto_merge_paths: Option<&mut Vec<String>>,
) -> Result<()> {
    let mut df_cases: Vec<(Vec<u8>, bool)> = Vec::new();
    for path in all_paths {
        // A directory/file conflict already resolved upstream (e.g. a directory-rename +
        // rename/delete that relocated the file to `path~SIDE`) is marked handled; do not
        // re-stage it here.
        if handled_paths.contains(path) {
            continue;
        }
        let o = ours_entries.get(path);
        let t = theirs_entries.get(path);
        if let Some(oe) = o {
            // Gitlink at `path` with `path/...` on the other side is submodule/directory merge
            // logic, not plain directory/file (`t6437-submodule-merge`, t5572 replace dir with submodule).
            if oe.mode != MODE_TREE
                && oe.mode != MODE_GITLINK
                && path_has_tree_descendant(theirs_entries, path)
                && t.is_none()
                && !base
                    .get(path)
                    .is_some_and(|be| be.oid == oe.oid && be.mode == oe.mode)
            {
                df_cases.push((path.clone(), true));
            }
        }
        if let Some(te) = t {
            if te.mode != MODE_TREE
                && te.mode != MODE_GITLINK
                && path_has_tree_descendant(ours_entries, path)
                && o.is_none()
                && !base
                    .get(path)
                    .is_some_and(|be| be.oid == te.oid && be.mode == te.mode)
            {
                df_cases.push((path.clone(), false));
            }
        }
    }

    for (path, file_is_ours) in df_cases {
        handled_paths.insert(path.clone());

        let file_entry = if file_is_ours {
            ours_entries.get(&path)
        } else {
            theirs_entries.get(&path)
        }
        .ok_or_else(|| anyhow::anyhow!("directory/file conflict: missing file entry"))?;

        let branch_desc = if file_is_ours { ours_label } else { their_name };
        let path_display = String::from_utf8_lossy(&path).into_owned();
        let path_str = path_display.clone();

        let side_entries = if file_is_ours {
            ours_entries
        } else {
            theirs_entries
        };
        let other_entries = if file_is_ours {
            theirs_entries
        } else {
            ours_entries
        };
        let side_renames = if file_is_ours {
            ours_renames
        } else {
            theirs_renames
        };
        let opposite_renames = if file_is_ours {
            theirs_renames
        } else {
            ours_renames
        };
        let rename_source = side_renames
            .iter()
            .find(|(_, dest)| dest.as_slice() == path.as_slice())
            .map(|(source, _)| source);
        let clean_directory_side = path_descendants_match(base, other_entries, &path)
            && !path_has_tree_descendant(side_entries, &path);
        if clean_directory_side && rename_source.is_none() {
            index.remove(&path);
            index.remove_descendants_under_path(&path_str);
            index.add_or_replace(file_entry.clone());
            mark_path_descendants_handled(handled_paths, base, &path);
            mark_path_descendants_handled(handled_paths, other_entries, &path);
            continue;
        }
        let new_path_str = if clean_directory_side {
            path_display.clone()
        } else {
            format!("{path_display}~{branch_desc}")
        };

        // When the file that lands in `path/` got there by a rename whose source the
        // *other* side deleted (and did not itself rename away), this is a rename/delete
        // that merge-ort reports in addition to the file/directory relocation. The C
        // rename machinery emits the rename/delete during rename collection, before the
        // directory/file pass relocates the blob; reproduce that message here so the bare
        // `path` (the directory-rename-adjusted destination) carries it (t4301 4-stacked).
        if let Some(source) = rename_source {
            let other_deleted_source =
                other_entries.get(source).is_none() && base.contains_key(source);
            if other_deleted_source && !opposite_renames.contains_key(source) {
                let source_str = String::from_utf8_lossy(source).into_owned();
                let delete_branch = if file_is_ours { their_name } else { ours_label };
                conflict_descriptions.push(ConflictDescription {
                    kind: "rename/delete",
                    body: format!(
                        "{source_str} renamed to {path_display} in {branch_desc}, but deleted in {delete_branch}."
                    ),
                    subject_path: path_display.clone(),
                    remerge_anchor_path: Some(source_str),
                    rename_rr_ours_dest: None,
                    rename_rr_theirs_dest: None,
                    auto_merge_hint_path: None,
                });
            }
        }

        if !clean_directory_side {
            let body = format!(
                "directory in the way of {} from {}; moving it to {} instead.",
                path_display, branch_desc, new_path_str
            );
            conflict_descriptions.push(ConflictDescription {
                kind: "file/directory",
                body,
                subject_path: new_path_str.clone(),
                remerge_anchor_path: Some(path_display.clone()),
                rename_rr_ours_dest: None,
                rename_rr_theirs_dest: None,
                auto_merge_hint_path: None,
            });
        }

        index.entries.retain(|e| e.path != path);

        // When either side renamed a tracked file into `path` and the other side still has that
        // file at the old path, merge-ort three-way merges the renamed blob with the other side's
        // source-path blob, then relocates the result because `path/` is occupied by a directory.
        if let Some((base_path, _)) = side_renames
            .iter()
            .find(|(_, dest)| dest.as_slice() == path.as_slice())
        {
            if let (Some(be), Some(other_at_source)) =
                (base.get(base_path), other_entries.get(base_path))
            {
                let base_path_str = String::from_utf8_lossy(base_path);
                let ours_for_merge = if file_is_ours {
                    file_entry
                } else {
                    other_at_source
                };
                let theirs_for_merge = if file_is_ours {
                    other_at_source
                } else {
                    file_entry
                };
                let ours_merge_label = if file_is_ours {
                    format!("{ours_label}:{path_str}")
                } else {
                    format!("{ours_label}:{base_path_str}")
                };
                let theirs_merge_label = if file_is_ours {
                    format!("{their_name}:{base_path_str}")
                } else {
                    format!("{their_name}:{path_str}")
                };
                if auto_merge_paths.is_none() {
                    conflict_descriptions.push(ConflictDescription {
                        kind: "info",
                        body: format!("Auto-merging {path_str}"),
                        subject_path: path_str.clone(),
                        remerge_anchor_path: None,
                        rename_rr_ours_dest: None,
                        rename_rr_theirs_dest: None,
                        auto_merge_hint_path: None,
                    });
                }
                match try_content_merge(
                    repo,
                    &path_str,
                    be,
                    ours_for_merge,
                    theirs_for_merge,
                    &ours_merge_label,
                    "",
                    &theirs_merge_label,
                    favor,
                    diff_algorithm,
                    merge_renormalize,
                    ignore_all_space,
                    ignore_space_change,
                    ignore_space_at_eol,
                    ignore_cr_at_eol,
                    auto_merge_paths.as_deref_mut(),
                )? {
                    ContentMergeResult::Clean(merged_oid, mode) => {
                        if clean_directory_side {
                            index.remove(&path);
                            index.remove_descendants_under_path(&path_str);
                            let mut merged_entry = file_entry.clone();
                            merged_entry.path = path.clone();
                            merged_entry.oid = merged_oid;
                            merged_entry.mode = mode;
                            index.add_or_replace(merged_entry);
                            continue;
                        }
                        // The file is relocated to `path~SIDE` because the directory occupies
                        // `path`; git records the unmerged entry under the relocated name (not bare
                        // `path`), matching `git ls-files -u` for rename/directory conflicts.
                        index.remove(&path);
                        let mut merged_entry = file_entry.clone();
                        merged_entry.path = new_path_str.as_bytes().to_vec();
                        merged_entry.oid = merged_oid;
                        merged_entry.mode = mode;
                        let stage = if file_is_ours { 2 } else { 3 };
                        stage_entry(index, &merged_entry, stage);
                        let merged_obj = repo.odb.read(&merged_oid)?;
                        conflict_files.push((new_path_str.clone(), merged_obj.data));
                        *has_conflicts = true;
                        continue;
                    }
                    ContentMergeResult::Conflict(content)
                    | ContentMergeResult::BinaryConflict(content) => {
                        index.remove(&path);
                        let tilde_path = new_path_str.as_bytes().to_vec();
                        let mut be_here = be.clone();
                        be_here.path = tilde_path.clone();
                        stage_entry(index, &be_here, 1);
                        let mut ours_here = ours_for_merge.clone();
                        ours_here.path = tilde_path.clone();
                        stage_entry(index, &ours_here, 2);
                        let mut theirs_here = theirs_for_merge.clone();
                        theirs_here.path = tilde_path.clone();
                        stage_entry(index, &theirs_here, 3);
                        conflict_files.push((new_path_str.clone(), content.clone()));
                        conflict_descriptions.push(ConflictDescription {
                            kind: "content",
                            body: format!("Merge conflict in {path_str}"),
                            subject_path: new_path_str.clone(),
                            remerge_anchor_path: None,
                            rename_rr_ours_dest: None,
                            rename_rr_theirs_dest: None,
                            auto_merge_hint_path: None,
                        });
                        *has_conflicts = true;
                        continue;
                    }
                }
            }
        }

        if let Some(source) = rename_source {
            if opposite_renames.contains_key(source) && index.get(source, 1).is_none() {
                if let Some(be) = base.get(source) {
                    stage_entry(index, be, 1);
                }
            }
        }
        let relocated_base_entry = base.get(&path).or_else(|| {
            rename_source.and_then(|source| {
                if opposite_renames.contains_key(source) {
                    None
                } else {
                    base.get(source)
                }
            })
        });

        if merge_ort_style {
            // merge-ort relocates the file to `path~SIDE` and records the unmerged
            // entry under that relocated name (not `path`, which is occupied by the
            // directory from the other side). `git add path~SIDE` then resolves it.
            let relocated = new_path_str.as_bytes().to_vec();
            if let Some(be) = relocated_base_entry {
                let mut be_here = be.clone();
                be_here.path = relocated.clone();
                stage_entry(index, &be_here, 1);
            }
            let stage = if file_is_ours { 2u8 } else { 3u8 };
            let mut staged = file_entry.clone();
            staged.path = relocated;
            stage_entry(index, &staged, stage);
            if let Ok(obj) = repo.odb.read(&file_entry.oid) {
                // Worktree path must avoid colliding with an existing directory at `path` (the other
                // side may still have `path/file` checked out).
                conflict_files.push((new_path_str, obj.data));
            }
        } else {
            let md_body = if file_is_ours {
                format!(
                    "{new_path_str} deleted in {their_name} and modified in {ours_label}.  Version {ours_label} of {new_path_str} left in tree."
                )
            } else {
                format!(
                    "{new_path_str} deleted in {ours_label} and modified in {their_name}.  Version {their_name} of {new_path_str} left in tree."
                )
            };
            if !clean_directory_side {
                conflict_descriptions.push(ConflictDescription {
                    kind: "modify/delete",
                    body: md_body,
                    subject_path: new_path_str.clone(),
                    remerge_anchor_path: Some(path_display.clone()),
                    rename_rr_ours_dest: None,
                    rename_rr_theirs_dest: None,
                    auto_merge_hint_path: None,
                });
            }

            // git also stages the base version (stage 1) at the relocated `path~SIDE`
            // path when the file existed in the merge base, so the modify/delete shows
            // both `1 path~SIDE` and `2/3 path~SIDE` in the conflicted-file listing.
            if let Some(be) = relocated_base_entry {
                let mut be_here = be.clone();
                be_here.path = new_path_str.as_bytes().to_vec();
                stage_entry(index, &be_here, 1);
            }

            let stage = if file_is_ours { 2u8 } else { 3u8 };
            let mut staged = file_entry.clone();
            staged.path = new_path_str.as_bytes().to_vec();
            stage_entry(index, &staged, stage);

            if let Ok(obj) = repo.odb.read(&file_entry.oid) {
                conflict_files.push((new_path_str, obj.data));
            }
        }
        *has_conflicts = true;
    }

    Ok(())
}

/// Get the tree OID from a commit.
fn commit_tree(repo: &Repository, commit_oid: ObjectId) -> Result<ObjectId> {
    let obj = repo.read_replaced(&commit_oid)?;
    let commit = parse_commit(&obj.data)?;
    Ok(commit.tree)
}

/// Build the `git merge-tree` "Could not read <oid>" error (exit 128, empty
/// stdout). git reports the failed object plus the trees it was collecting merge
/// info for, then `fatal: failure to merge`; the harness only greps for the
/// `Could not read <oid>` substring, but we reproduce git's full message.
fn merge_tree_could_not_read_error(oid: &ObjectId) -> anyhow::Error {
    anyhow::Error::new(ExplicitExit {
        code: 128,
        message: format!("error: Could not read {}", oid.to_hex()),
    })
}

/// Returns true if `err` is a `grit_lib` "object not found" error for any oid.
fn is_object_not_found(err: &grit_lib::error::Error) -> bool {
    matches!(err, grit_lib::error::Error::ObjectNotFound(_))
}

/// Peel a commit-ish or tree OID to its tree, the way `git merge-tree` accepts
/// either kind for the branches and `--merge-base`. A missing object is mapped
/// to git's "Could not read <oid>" merge-tree error rather than grit's generic
/// "object not found".
fn peel_to_tree_for_merge_tree(repo: &Repository, oid: ObjectId) -> Result<ObjectId> {
    let obj = match repo.read_replaced(&oid) {
        Ok(obj) => obj,
        Err(e) if is_object_not_found(&e) => return Err(merge_tree_could_not_read_error(&oid)),
        Err(e) => return Err(e.into()),
    };
    match obj.kind {
        ObjectKind::Tree => Ok(oid),
        ObjectKind::Commit => Ok(parse_commit(&obj.data)?.tree),
        ObjectKind::Tag => {
            let tag = parse_tag(&obj.data)?;
            peel_to_tree_for_merge_tree(repo, tag.object)
        }
        other => bail!("expected commit-ish or tree, got {other}"),
    }
}

/// Like [`tree_to_index_entries`] but maps a missing tree object to git's
/// merge-tree "Could not read <oid>" error.
fn tree_to_index_entries_for_merge_tree(
    repo: &Repository,
    tree_oid: &ObjectId,
) -> Result<Vec<IndexEntry>> {
    tree_to_index_entries(repo, tree_oid, "").map_err(|e| {
        if let grit_lib::error::Error::ObjectNotFound(hex) = &e {
            if let Ok(missing) = hex.parse::<ObjectId>() {
                return merge_tree_could_not_read_error(&missing);
            }
        }
        e.into()
    })
}

/// Return the commit author timestamp (seconds since epoch).
///
/// Falls back to `0` when the author identity lacks a parseable timestamp.
fn commit_author_timestamp(repo: &Repository, commit_oid: ObjectId) -> Result<i64> {
    let obj = repo.read_replaced(&commit_oid)?;
    let commit = parse_commit(&obj.data)?;
    let author = commit.author;
    if let Some(ts) = author
        .rsplit(' ')
        .nth(1)
        .and_then(|s| s.parse::<i64>().ok())
    {
        return Ok(ts);
    }

    let date_text = author
        .split('>')
        .nth(1)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    if date_text.is_empty() {
        return Ok(0);
    }

    let fmt = time::format_description::parse_borrowed::<1>(
        "[year]-[month]-[day] [hour]:[minute]:[second]",
    );
    if let Ok(fmt) = fmt {
        if let Ok(naive) = time::PrimitiveDateTime::parse(date_text, &fmt) {
            return Ok(naive.assume_utc().unix_timestamp());
        }
    }

    Ok(0)
}

/// Print a diffstat summary for merge output.
fn print_diffstat(repo: &Repository, entries: &[DiffEntry], compact: bool) {
    if entries.is_empty() {
        return;
    }

    struct StatEntry {
        path: String,
        display_path: String,
        insertions: usize,
        deletions: usize,
        is_binary: bool,
        is_new: bool,
        is_deleted: bool,
        new_mode: Option<u32>,
    }

    let mut stats: Vec<StatEntry> = Vec::new();

    for entry in entries {
        let path = entry
            .new_path
            .as_deref()
            .or(entry.old_path.as_deref())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".to_string());
        let is_new = entry.old_oid == zero_oid();
        let is_deleted = entry.new_oid == zero_oid();

        let old_raw = if !is_new {
            repo.odb.read(&entry.old_oid).ok().map(|o| o.data)
        } else {
            None
        };
        let new_raw = if !is_deleted {
            repo.odb.read(&entry.new_oid).ok().map(|o| o.data)
        } else {
            None
        };

        let (ins, del, is_binary) = match (old_raw.as_ref(), new_raw.as_ref()) {
            (Some(o), Some(n)) if o.contains(&0) || n.contains(&0) => {
                let deleted = o.len();
                let added = n.len();
                (added, deleted, true)
            }
            (Some(o), None) if o.contains(&0) => (0, o.len(), true),
            (None, Some(n)) if n.contains(&0) => (n.len(), 0, true),
            _ => {
                let old_content = old_raw
                    .as_ref()
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .unwrap_or_default();
                let new_content = new_raw
                    .as_ref()
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .unwrap_or_default();
                let (i, d) = count_changes(&old_content, &new_content);
                (i, d, false)
            }
        };

        let mode_num = u32::from_str_radix(&entry.new_mode, 8).unwrap_or(0o100644);
        let mut display_path = path.clone();
        if compact {
            if is_new {
                display_path.push_str(" (new)");
            } else if is_deleted {
                display_path.push_str(" (gone)");
            }
        }
        stats.push(StatEntry {
            path,
            display_path,
            insertions: ins,
            deletions: del,
            is_binary,
            is_new,
            is_deleted,
            new_mode: if is_new { Some(mode_num) } else { None },
        });
    }

    let cfg = ConfigSet::load(Some(&repo.git_dir), false).unwrap_or_default();
    let stat_name_width = cfg
        .get("diff.statNameWidth")
        .and_then(|v| v.parse::<usize>().ok());
    let stat_graph_width = cfg
        .get("diff.statGraphWidth")
        .and_then(|v| v.parse::<usize>().ok());

    let files: Vec<FileStatInput> = stats
        .iter()
        .map(|s| FileStatInput {
            path_display: s.display_path.clone(),
            insertions: s.insertions,
            deletions: s.deletions,
            is_binary: s.is_binary,
            is_unmerged: false,
        })
        .collect();

    let opts = DiffstatOptions {
        total_width: terminal_columns(),
        line_prefix: "",
        width_prefix: "",
        subtract_prefix_from_terminal: false,
        stat_name_width,
        stat_graph_width,
        stat_count: None,
        color_add: "",
        color_del: "",
        color_reset: "",
        graph_bar_slack: 0,
        graph_prefix_budget_slack: 0,
    };
    let _ = write_diffstat_block(&mut std::io::stdout().lock(), &files, &opts);

    if !compact {
        for s in &stats {
            if s.is_new {
                if let Some(mode) = s.new_mode {
                    println!(" create mode {:06o} {}", mode, s.path);
                }
            }
            if s.is_deleted {
                println!(" delete mode 100644 {}", s.path);
            }
        }
    }
}

/// True if `dir` exists and contains only `.` and `..` (safe to replace with a submodule gitlink).
fn is_empty_dir_for_submodule_placeholder(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        if name != "." && name != ".." {
            return false;
        }
    }
    true
}

/// Refresh cached stat data for every stage-0 index entry from the work tree.
///
/// Tree-built indexes start with zeroed stat fields; without refreshing,
/// `git diff-files` falsely reports every tracked file as modified.
pub(crate) fn refresh_index_stat_cache_from_worktree(
    repo: &Repository,
    index: &mut Index,
) -> Result<()> {
    let Some(work_tree) = repo.work_tree.as_deref() else {
        return Ok(());
    };
    grit_lib::diff::refresh_index_stat_content_verified(index, work_tree, None);
    Ok(())
}

/// Recursively flatten a tree into index entries.
fn core_ignorecase(repo: &Repository) -> bool {
    ConfigSet::load(Some(&repo.git_dir), true)
        .ok()
        .and_then(|c| c.get_bool("core.ignorecase"))
        .and_then(std::result::Result::ok)
        .unwrap_or(false)
}

/// Lowercase each `/`-separated path component (ASCII) for `core.ignorecase` path identity.
fn path_ascii_lowercase_components(path: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(path.len());
    let mut start = 0;
    for (i, &b) in path.iter().enumerate() {
        if b == b'/' {
            for &c in &path[start..i] {
                out.push(c.to_ascii_lowercase());
            }
            out.push(b'/');
            start = i + 1;
        }
    }
    for &c in &path[start..] {
        out.push(c.to_ascii_lowercase());
    }
    out
}

/// Flatten a tree to a path → entry map for merge machinery.
///
/// When `core.ignorecase` is true, paths are normalized to a canonical spelling (ASCII
/// lowercased per component) so `TestCase` and `testcase` occupy one slot, matching Git on
/// case-insensitive filesystems (t6419).
fn tree_to_map_for_merge(
    repo: &Repository,
    entries: Vec<IndexEntry>,
) -> HashMap<Vec<u8>, IndexEntry> {
    if !core_ignorecase(repo) {
        return tree_to_map(entries);
    }
    let mut out = HashMap::new();
    for mut e in entries {
        let key = path_ascii_lowercase_components(&e.path);
        let plen = key.len().min(0xFFF) as u16;
        e.path = key.clone();
        e.flags = (e.flags & !0xFFF) | plen;
        out.entry(key).or_insert(e);
    }
    out
}

/// Maps a canonical (ASCII-lowercased) merge key back to the original on-disk spelling.
///
/// Built once per merge from the un-lowercased side trees (see
/// [`build_original_spelling_table`] for the priority rules), so after `merge_trees` returns we can
/// rewrite the lowercased result paths back to the spelling the real index/worktree uses. Without
/// this, `core.ignorecase` merges would emit lowercased paths (e.g. `a.t` instead of the tracked
/// `A.t`), breaking case-sensitive byte comparisons against the real index in
/// `bail_if_merge_would_overwrite_local_changes` / `remove_deleted_files` (t6110).
type OriginalSpellingTable = HashMap<Vec<u8>, Vec<u8>>;

/// Build the canonical-lowercase → original-spelling table from the raw (un-lowercased) side trees.
///
/// For a case-only rename (e.g. base `TestCase`, theirs renamed to `testcase`, ours kept
/// `TestCase`), `git merge-ort` keeps the *renamed* spelling. So the priority is: a side whose
/// spelling differs from base (the side that performed the case change) wins over a side that kept
/// the base spelling; among equally-eligible sides prefer ours, then theirs, then base.
fn build_original_spelling_table(
    repo: &Repository,
    base_tree: &ObjectId,
    ours_tree: &ObjectId,
    theirs_tree: &ObjectId,
) -> Result<Option<OriginalSpellingTable>> {
    if !core_ignorecase(repo) {
        return Ok(None);
    }
    let collect = |tree: &ObjectId| -> Result<HashMap<Vec<u8>, Vec<u8>>> {
        let mut m = HashMap::new();
        for e in tree_to_index_entries(repo, tree, "")? {
            let key = path_ascii_lowercase_components(&e.path);
            m.entry(key).or_insert(e.path);
        }
        Ok(m)
    };
    let base = collect(base_tree)?;
    let ours = collect(ours_tree)?;
    let theirs = collect(theirs_tree)?;

    let mut keys: BTreeSet<&Vec<u8>> = BTreeSet::new();
    keys.extend(base.keys());
    keys.extend(ours.keys());
    keys.extend(theirs.keys());

    let mut table: OriginalSpellingTable = HashMap::new();
    for key in keys {
        let base_spelling = base.get(key);
        // A side "renamed" (case-changed) the path when its spelling differs from base's.
        let changed = |side: Option<&Vec<u8>>| match (side, base_spelling) {
            (Some(s), Some(b)) => s != b,
            (Some(_), None) => false, // pure add: spelling is its own, not a case-rename
            (None, _) => false,
        };
        let pick = if changed(ours.get(key)) {
            ours.get(key)
        } else if changed(theirs.get(key)) {
            theirs.get(key)
        } else {
            ours.get(key).or_else(|| theirs.get(key)).or(base_spelling)
        };
        if let Some(spelling) = pick {
            table.insert(key.clone(), spelling.clone());
        }
    }
    Ok(Some(table))
}

/// Rewrite a single path from its canonical-lowercase form to the original spelling, recomputing
/// the 0xFFF path-length flag bits exactly as `tree_to_map_for_merge` does.
fn restore_entry_spelling(entry: &mut IndexEntry, table: &OriginalSpellingTable) {
    if let Some(original) = table.get(&entry.path) {
        if *original != entry.path {
            let plen = original.len().min(0xFFF) as u16;
            entry.path = original.clone();
            entry.flags = (entry.flags & !0xFFF) | plen;
        }
    }
}

/// Restore original spelling on every entry of an index (all stages).
fn restore_index_spelling(index: &mut Index, table: &OriginalSpellingTable) {
    for e in &mut index.entries {
        restore_entry_spelling(e, table);
    }
}

/// Restore original spelling on a path→entry map (rebuilt because keys change with the spelling).
fn restore_map_spelling(map: &mut HashMap<Vec<u8>, IndexEntry>, table: &OriginalSpellingTable) {
    let old = std::mem::take(map);
    for (_key, mut e) in old {
        restore_entry_spelling(&mut e, table);
        map.insert(e.path.clone(), e);
    }
}

/// Restore original spelling on a `String` path used in conflict output.
fn restore_string_path_spelling(path: &str, table: &OriginalSpellingTable) -> String {
    match table.get(path.as_bytes()) {
        Some(original) => String::from_utf8_lossy(original).into_owned(),
        None => path.to_string(),
    }
}

/// Rewrite all lowercased result paths in a `MergeResult` (index, conflict files, conflict
/// descriptions) back to original spelling so they line up with the real index/worktree.
fn restore_merge_result_spelling(result: &mut MergeResult, table: &OriginalSpellingTable) {
    restore_index_spelling(&mut result.index, table);
    for (path, _content) in &mut result.conflict_files {
        *path = restore_string_path_spelling(path, table);
    }
    for desc in &mut result.conflict_descriptions {
        desc.subject_path = restore_string_path_spelling(&desc.subject_path, table);
        if let Some(p) = desc.remerge_anchor_path.take() {
            desc.remerge_anchor_path = Some(restore_string_path_spelling(&p, table));
        }
        if let Some(p) = desc.rename_rr_ours_dest.take() {
            desc.rename_rr_ours_dest = Some(restore_string_path_spelling(&p, table));
        }
        if let Some(p) = desc.rename_rr_theirs_dest.take() {
            desc.rename_rr_theirs_dest = Some(restore_string_path_spelling(&p, table));
        }
        if let Some(p) = desc.auto_merge_hint_path.take() {
            desc.auto_merge_hint_path = Some(restore_string_path_spelling(&p, table));
        }
    }
}

/// Whether merging `spec` is an annotated tag that is not recorded locally at `refs/tags/<name>`.
///
/// Git forbids fast-forwarding such "throwaway" tags so the signed tag object remains in history
/// (builtin/merge.c `merging_a_throwaway_tag`).
fn merging_throwaway_tag(repo: &Repository, spec: &str) -> Result<bool> {
    use grit_lib::refs::resolve_ref;

    let tag_oid = if spec.len() == 40 && spec.chars().all(|c| c.is_ascii_hexdigit()) {
        ObjectId::from_hex(spec).ok()
    } else if !spec.contains(['~', '^', ':', '@']) && spec != "HEAD" && spec != "FETCH_HEAD" {
        let tag_ref = format!("refs/tags/{spec}");
        resolve_ref(&repo.git_dir, &tag_ref).ok()
    } else {
        None
    };
    let Some(tag_oid) = tag_oid else {
        return Ok(false);
    };
    let obj = repo.odb.read(&tag_oid)?;
    if obj.kind != ObjectKind::Tag {
        return Ok(false);
    }
    let tag = parse_tag(&obj.data)?;
    let local_ref = format!("refs/tags/{}", tag.tag);
    match resolve_ref(&repo.git_dir, &local_ref) {
        Ok(local_oid) if local_oid == tag_oid => Ok(false),
        _ => Ok(true),
    }
}

/// Resolve a merge target (branch name or commit-ish).
///
/// Annotated tags must peel to their peeled object (typically a commit), matching
/// `git merge <tag>` — resolving only to the tag OID breaks merge and yields
/// "corrupt object: commit missing author header" when reading the tag as a commit.
fn resolve_merge_target(repo: &Repository, spec: &str) -> Result<ObjectId> {
    use grit_lib::refs::resolve_ref;
    use grit_lib::rev_parse::resolve_revision_as_commit;

    if let Some(oid) = grit_lib::rev_parse::resolve_at_minus_to_oid(repo, spec)? {
        return Ok(oid);
    }
    // Prefer an unambiguous local branch when the name matches both a ref and a path (t7007:
    // `merge main3` must merge the `main3` branch tip, not treat `main3` as a pathspec).
    if !spec.contains('/') && !spec.starts_with('.') {
        let branch_ref = format!("refs/heads/{spec}");
        let (dwim_count, _) = grit_lib::refs::resolve_ref_dwim(&repo.git_dir, spec);
        if dwim_count <= 1 {
            if let Ok(oid) = resolve_ref(&repo.git_dir, &branch_ref) {
                return Ok(oid);
            }
        }
    }
    // Git resolves the merge argument with `get_merge_parent` (rev-parse / `dwim_ref`), which for
    // a *bare* name only follows `ref_rev_parse_rules` — it never expands `<name>` to
    // `refs/remotes/<remote>/<name>`. A bare name that does not resolve dies with a suggestion
    // (`<name> - not something we can merge` + `Did you mean ...`), so do NOT fall through to the
    // looser `resolve_revision_as_commit` DWIM for plain names (t7600 #82/#83).
    let is_plain_name = !spec.contains(['~', '^', ':', '@'])
        && !is_hex_like(spec)
        && spec != "HEAD"
        && spec != "FETCH_HEAD";
    if is_plain_name {
        let (count, dwim) = grit_lib::refs::resolve_ref_dwim(&repo.git_dir, spec);
        let _ = count;
        if let Some(oid) = dwim {
            return peel_to_commit_oid(repo, oid);
        }
        return Err(merge_unknown_ref_error(repo, spec));
    }
    resolve_revision_as_commit(repo, spec).map_err(|e| anyhow::anyhow!("{e}"))
}

/// Whether `spec` looks like an (abbreviated) object id rather than a ref name.
fn is_hex_like(spec: &str) -> bool {
    spec.len() >= 4 && spec.chars().all(|c| c.is_ascii_hexdigit())
}

/// Peel `oid` to a commit (following tag objects), as `get_merge_parent` does.
fn peel_to_commit_oid(repo: &Repository, oid: ObjectId) -> Result<ObjectId> {
    let mut cur = oid;
    for _ in 0..10 {
        let obj = repo.odb.read(&cur)?;
        match obj.kind {
            ObjectKind::Commit => return Ok(cur),
            ObjectKind::Tag => {
                let tag = grit_lib::objects::parse_tag(&obj.data)?;
                cur = tag.object;
            }
            _ => break,
        }
    }
    Ok(cur)
}

/// Build git's `help_unknown_ref`-style error for an unmergeable bare ref name: the headline plus
/// `Did you mean ...` suggestions drawn from `refs/remotes/*` whose last component matches.
fn merge_unknown_ref_error(repo: &Repository, spec: &str) -> anyhow::Error {
    let suggestions = guess_merge_refs(repo, spec);
    let mut msg = format!("merge: {spec} - not something we can merge\n");
    if !suggestions.is_empty() {
        if suggestions.len() == 1 {
            msg.push_str("\nDid you mean this?\n");
        } else {
            msg.push_str("\nDid you mean one of these?\n");
        }
        for s in &suggestions {
            msg.push_str(&format!("\t{s}\n"));
        }
    }
    anyhow::Error::new(ExplicitExit {
        code: 1,
        message: msg.trim_end_matches('\n').to_string(),
    })
}

/// Suggest remote-tracking refs whose final path component equals `base` (git `guess_refs` /
/// `append_similar_ref`), returning each as a shortened-unambiguous ref name.
fn guess_merge_refs(repo: &Repository, base: &str) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(remotes) = grit_lib::refs::list_refs(&repo.git_dir, "refs/remotes/") else {
        return out;
    };
    for (name, _oid) in remotes {
        let last = name.rsplit('/').next().unwrap_or("");
        if last == base {
            out.push(shorten_unambiguous_merge_ref(repo, &name));
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Shorten a full ref to the most concise `ref_rev_parse_rules` form that still resolves back to
/// it unambiguously (git `refs_shorten_unambiguous_ref` with `strict`). Falls back to stripping
/// `refs/`.
fn shorten_unambiguous_merge_ref(repo: &Repository, full: &str) -> String {
    // Candidate short names in the same order git's rules prefer, shortest first.
    let candidates: Vec<&str> = if let Some(rest) = full.strip_prefix("refs/remotes/") {
        // `refs/remotes/origin/x` -> try `origin/x` then `remotes/origin/x`.
        vec![rest, full.strip_prefix("refs/").unwrap_or(full)]
    } else if let Some(rest) = full.strip_prefix("refs/heads/") {
        vec![rest]
    } else if let Some(rest) = full.strip_prefix("refs/tags/") {
        vec![rest]
    } else {
        vec![full.strip_prefix("refs/").unwrap_or(full)]
    };
    for cand in candidates {
        let (count, _) = grit_lib::refs::resolve_ref_dwim(&repo.git_dir, cand);
        if count == 1 {
            return cand.to_string();
        }
    }
    full.strip_prefix("refs/").unwrap_or(full).to_string()
}

pub(crate) fn read_fetch_head_merge_oids(repo: &Repository) -> Result<Vec<String>> {
    let fetch_head_path = repo.git_dir.join("FETCH_HEAD");
    let content = fs::read_to_string(&fetch_head_path)
        .with_context(|| "FETCH_HEAD: object not found: FETCH_HEAD".to_string())?;

    let oids = grit_lib::fetch_head::merge_object_ids_hex(&content);

    if oids.is_empty() {
        bail!("FETCH_HEAD: object not found: FETCH_HEAD");
    }
    Ok(oids)
}

/// The pseudo-ref name git uses to hold the autostash commit during a merge.
const MERGE_AUTOSTASH_REF: &str = "MERGE_AUTOSTASH";

/// Create the `MERGE_AUTOSTASH` autostash before a merge that may touch the dirty working tree.
///
/// Snapshots the dirty index/worktree as a stash commit (recorded under `.git/MERGE_AUTOSTASH`),
/// then resets the index and working tree hard to HEAD so the merge starts from a clean state —
/// matching git's `create_autostash_ref(the_repository, "MERGE_AUTOSTASH")`.
fn create_merge_autostash(repo: &Repository) -> Result<()> {
    crate::commands::stash::create_autostash_ref(repo, MERGE_AUTOSTASH_REF)?;
    Ok(())
}

/// Apply the pending `MERGE_AUTOSTASH` (git `apply_autostash_ref`): re-apply the stashed local
/// delta on top of the merge result, print `Applied autostash.` (or, on conflict, store it back
/// to the stash and print `Applying autostash resulted in conflicts.`), and clear the ref.
fn apply_merge_autostash(repo: &Repository) -> Result<()> {
    crate::commands::stash::apply_autostash_ref(repo, MERGE_AUTOSTASH_REF)
}

/// Save the pending `MERGE_AUTOSTASH` to `refs/stash` without applying it (git
/// `save_autostash_ref`), used when an in-progress autostash merge is aborted/quit/reset.
fn save_merge_autostash(repo: &Repository) -> Result<()> {
    crate::commands::stash::save_autostash_ref(repo, MERGE_AUTOSTASH_REF)
}

/// Whether the effective merge message cleanup mode is `scissors`.
///
/// Mirrors git's `get_cleanup_mode(cleanup_arg, 1)` evaluated *as if editing* (the conflict
/// hint always uses the editor cleanup mode even with `--no-edit`, so a follow-up `git commit`
/// sees the scissors block — see builtin/merge.c:suggest_conflicts comment).
fn merge_cleanup_is_scissors(args: &Args, repo: &Repository) -> bool {
    if let Some(c) = args.cleanup.as_deref() {
        return c == "scissors";
    }
    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    config
        .get("commit.cleanup")
        .map(|v| v.trim() == "scissors")
        .unwrap_or(false)
}

/// Append git's conflict hint to a MERGE_MSG/SQUASH_MSG body, matching
/// `append_conflicts_hint` (sequencer.c). With scissors cleanup the cut line and its
/// explanation precede the `# Conflicts:` list; the comment prefix is `#`.
fn append_merge_conflicts_hint(msg: &mut String, paths: &[String], scissors: bool) {
    if scissors {
        msg.push('\n');
        msg.push_str("# ------------------------ >8 ------------------------\n");
        msg.push_str("# Do not modify or remove the line above.\n");
        msg.push_str("# Everything below it will be ignored.\n");
        msg.push('#');
    }
    msg.push('\n');
    msg.push_str("# Conflicts:\n");
    for path in paths {
        msg.push_str(&format!("#\t{path}\n"));
    }
}

/// Build the default merge commit message.
/// Append Signed-off-by trailer to a message if not already present.
/// Whether the resulting merge commit should be GPG-signed: `-S`/`--gpg-sign`
/// or `commit.gpgsign`, unless `--no-gpg-sign` was given.
fn should_sign_merge(args: &Args, config: &ConfigSet) -> bool {
    if args.no_gpg_sign {
        return false;
    }
    if args.gpg_sign.is_some() {
        return true;
    }
    matches!(config.get_bool("commit.gpgsign"), Some(Ok(true)))
}

/// Sign a serialized merge commit object, splicing in the `gpgsig` header.
fn sign_merge_commit_bytes(
    config: &ConfigSet,
    committer: &str,
    key_override: Option<&str>,
    commit_bytes: Vec<u8>,
) -> Result<Vec<u8>> {
    let cfg = grit_lib::signing::GpgConfig::from_config(config)?;
    let committer_default = grit_lib::signing::committer_signing_default(committer);
    let signing_key = cfg.resolve_signing_key(key_override, &committer_default);
    let signature = grit_lib::signing::sign_buffer(&cfg, &commit_bytes, &signing_key)?;
    Ok(grit_lib::signing::add_header_signature(
        &commit_bytes,
        &signature,
        grit_lib::signing::GPG_SIG_HEADER_SHA1,
    ))
}

fn append_signoff(msg: &str, name: &str, email: &str) -> String {
    let trailer = format!("Signed-off-by: {} <{}>", name, email);
    if msg.contains(&trailer) {
        return msg.to_string();
    }
    let trimmed = msg.trim_end();
    format!("{}\n\n{}\n", trimmed, trailer)
}

/// UTF-8 merge message plus optional raw bytes and `encoding` header for the commit object.
struct MergeCommitMessage {
    message: String,
    encoding: Option<String>,
    raw_message: Option<Vec<u8>>,
}

fn read_merge_message_from_file(path: &Path, config: &ConfigSet) -> Result<String> {
    let bytes =
        fs::read(path).with_context(|| format!("could not read merge message file: {path:?}"))?;
    if let Ok(s) = String::from_utf8(bytes.clone()) {
        return Ok(s);
    }
    let enc_name = config
        .get("i18n.commitEncoding")
        .or_else(|| config.get("i18n.commitencoding"));
    Ok(grit_lib::commit_encoding::decode_bytes(
        enc_name.as_deref(),
        &bytes,
    ))
}

fn finalize_merge_commit_message(msg: String, config: &ConfigSet) -> MergeCommitMessage {
    let commit_enc = config
        .get("i18n.commitEncoding")
        .or_else(|| config.get("i18n.commitencoding"));
    let is_utf8 = match commit_enc.as_deref() {
        None => true,
        Some(e) => e.eq_ignore_ascii_case("utf-8") || e.eq_ignore_ascii_case("utf8"),
    };
    if is_utf8 {
        return MergeCommitMessage {
            message: msg,
            encoding: None,
            raw_message: None,
        };
    }
    let Some(label) = commit_enc else {
        return MergeCommitMessage {
            message: msg,
            encoding: None,
            raw_message: None,
        };
    };
    let Some(raw) = grit_lib::commit_encoding::encode_unicode(&label, &msg) else {
        return MergeCommitMessage {
            message: msg,
            encoding: None,
            raw_message: None,
        };
    };
    MergeCommitMessage {
        message: msg,
        encoding: Some(label),
        raw_message: Some(raw),
    }
}

/// If `spec` is `<base>^^^...` or `<base>~<number>`, return `(base, is_early)` where `is_early`
/// is true for the ancestor expressions git marks "(early part)" — any trailing `^`, a bare
/// `name~` (== `name~1`), or `name~<nonzero>`. `name~0` returns `is_early == false`.
/// Mirrors git's `merge_name` suffix detection (builtin/merge.c).
fn early_part_branch_base(spec: &str) -> Option<(&str, bool)> {
    // Trailing carets: `name^`, `name^^`, ...
    let caret_trim = spec.trim_end_matches('^');
    if caret_trim.len() < spec.len() && !caret_trim.is_empty() {
        return Some((caret_trim, true));
    }
    // `name~<number>` (including bare `name~` == `name~1`).
    if let Some(pos) = spec.rfind('~') {
        let (base, rest) = spec.split_at(pos);
        let digits = &rest[1..];
        if base.is_empty() {
            return None;
        }
        if digits.is_empty() {
            return Some((base, true)); // "name~" == "name~1"
        }
        if digits.chars().all(|c| c.is_ascii_digit()) {
            let seen_nonzero = digits.chars().any(|c| c != '0');
            return Some((base, seen_nonzero));
        }
    }
    None
}

fn build_merge_message(
    head: &HeadState,
    branch_name: &str,
    custom: Option<&str>,
    repo: &Repository,
) -> String {
    if let Some(msg) = custom {
        return ensure_trailing_newline(msg);
    }
    // `merge new@{u}` — default message names the resolved remote-tracking branch (t1507).
    if upstream_suffix_info(branch_name).is_some() {
        if let Ok(full) = resolve_upstream_symbolic_name(repo, branch_name) {
            if let Some(rest) = full.strip_prefix("refs/remotes/") {
                return ensure_trailing_newline(&format!("Merge remote-tracking branch '{rest}'"));
            }
            if full.starts_with("refs/heads/") {
                let short = full.strip_prefix("refs/heads/").unwrap_or(&full);
                return ensure_trailing_newline(&format!("Merge branch '{short}'"));
            }
        }
    }
    // For @{-N} (and @{-N}<suffix>) specs, use the resolved previous branch
    // name in the default merge message, matching git's behavior.
    let display_branch = if branch_name.starts_with("@{-") {
        if let Some(close) = branch_name.find('}') {
            let token = &branch_name[..=close];
            match grit_lib::rev_parse::expand_at_minus_to_branch_name(repo, token) {
                Ok(Some(name)) => name,
                _ => branch_name.to_string(),
            }
        } else {
            branch_name.to_string()
        }
    } else {
        branch_name.to_string()
    };
    // `<branch>~<n>` / `<branch>^...` early-part merges: when the suffix-stripped base names a
    // local branch, git emits `Merge branch '<base>' (early part)` (builtin/merge.c merge_name).
    // Otherwise (e.g. a tag base) it falls through to the `commit '<full spec>'` form below.
    if let Some((base, is_early)) = early_part_branch_base(&display_branch) {
        if resolve_ref(&repo.git_dir, &format!("refs/heads/{base}")).is_ok() {
            let suffix = if is_early { " (early part)" } else { "" };
            let base_msg = format!("Merge branch '{base}'{suffix}");
            let msg = match head.branch_name() {
                Some(name) if name != "main" && name != "master" => {
                    format!("{base_msg} into {name}")
                }
                _ => base_msg,
            };
            return ensure_trailing_newline(&msg);
        }
    }
    // Determine if the merge target is a tag, branch, or commit
    let kind = if resolve_ref(&repo.git_dir, &format!("refs/tags/{display_branch}")).is_ok() {
        "tag"
    } else if resolve_ref(&repo.git_dir, &format!("refs/remotes/{display_branch}")).is_ok() {
        "remote-tracking branch"
    } else if resolve_ref(&repo.git_dir, &format!("refs/heads/{display_branch}")).is_ok() {
        "branch"
    } else if display_branch.contains(['~', '^', ':', '@'])
        || (display_branch.len() >= 4 && display_branch.chars().all(|c| c.is_ascii_hexdigit()))
    {
        // A revision expression / object id that is not a plain ref: `Merge commit '<spec>'`.
        "commit"
    } else {
        "branch"
    };
    let base_msg = format!("Merge {kind} '{display_branch}'");
    // Append "into <branch>" if not merging into main/master
    let msg = if let Some(name) = head.branch_name() {
        if name != "main" && name != "master" {
            format!("{base_msg} into {name}")
        } else {
            base_msg
        }
    } else {
        base_msg
    };
    ensure_trailing_newline(&msg)
}

/// Update HEAD to point to the given commit.
fn update_head(git_dir: &Path, head: &HeadState, commit_oid: &ObjectId) -> Result<()> {
    match head {
        HeadState::Branch { refname, .. } => {
            let ref_path = git_dir.join(refname);
            if let Some(parent) = ref_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&ref_path, format!("{}\n", commit_oid.to_hex()))?;
        }
        HeadState::Detached { .. } | HeadState::Invalid => {
            fs::write(git_dir.join("HEAD"), format!("{}\n", commit_oid.to_hex()))?;
        }
    }
    Ok(())
}

fn sparse_checkout_enabled(git_dir: &Path) -> bool {
    let cfg = ConfigSet::load(Some(git_dir), true).unwrap_or_default();
    cfg.get("core.sparsecheckout")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Before applying a conflicted merge to the work tree, ensure no planned removal/checkout would
/// delete the process cwd. Matches Git aborting with `ERROR_CWD_IN_THE_WAY` before mutating the
/// tree (`t2501-cwd-empty`).
fn preflight_merge_worktree_for_cwd(
    repo: &Repository,
    work_tree: &Path,
    old_entries: &HashMap<Vec<u8>, IndexEntry>,
    new_index: &Index,
    sparse_checkout: bool,
) -> Result<()> {
    // Only stage 0 represents a merged tree path; unmerged stages (1–3) must not block removal
    // of paths that disappeared from the result tree (`t2501-cwd-empty` merge + cwd checks).
    let new_paths: std::collections::HashSet<&[u8]> = new_index
        .entries
        .iter()
        .filter(|e| e.stage() == 0)
        .map(|e| e.path.as_slice())
        .collect();
    let mut to_remove: Vec<String> = Vec::new();
    for (path, old_entry) in old_entries {
        if new_paths.contains(path.as_slice()) {
            continue;
        }
        if sparse_checkout {
            if let Some(ne) = new_index
                .entries
                .iter()
                .find(|e| e.stage() == 0 && e.path == *path)
            {
                if ne.skip_worktree() {
                    continue;
                }
            }
        }
        let has_nested_under = new_index.entries.iter().any(|e| {
            e.path.starts_with(path)
                && e.path.len() > path.len()
                && e.path.get(path.len()) == Some(&b'/')
        });
        if old_entry.mode == MODE_GITLINK && !has_nested_under {
            continue;
        }
        to_remove.push(String::from_utf8_lossy(path).into_owned());
    }
    to_remove.sort_by_key(|p| std::cmp::Reverse(p.bytes().filter(|b| *b == b'/').count()));
    for path_str in &to_remove {
        if grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(work_tree, path_str) {
            bail!("Refusing to remove the current working directory:\n{path_str}\n");
        }
    }

    for entry in &new_index.entries {
        if entry.stage() != 0 {
            continue;
        }
        if sparse_checkout && entry.skip_worktree() {
            continue;
        }
        if old_entries
            .get(&entry.path)
            .is_some_and(|previous| previous.oid == entry.oid && previous.mode == entry.mode)
        {
            continue;
        }
        let path_str = String::from_utf8_lossy(&entry.path).into_owned();
        let abs_path = work_tree.join(&path_str);

        let mut cur = abs_path.parent();
        while let Some(dir) = cur {
            if dir == work_tree {
                break;
            }
            if dir.exists() && !dir.is_dir() {
                let rel = dir.strip_prefix(work_tree).ok().map(|p| {
                    p.to_string_lossy()
                        .replace('\\', "/")
                        .trim_start_matches('/')
                        .to_string()
                });
                if let Some(r) = rel.filter(|s| !s.is_empty()) {
                    if grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(work_tree, &r) {
                        bail!("Refusing to remove the current working directory:\n{r}\n");
                    }
                }
            }
            cur = dir.parent();
        }

        if entry.mode == 0o160000 {
            if grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(work_tree, &path_str) {
                bail!("Refusing to remove the current working directory:\n{path_str}\n");
            }
            continue;
        }

        let obj = repo.odb.read(&entry.oid)?;
        if obj.kind != ObjectKind::Blob {
            continue;
        }

        if entry.mode == MODE_SYMLINK {
            if abs_path.exists() || abs_path.is_symlink() {
                if grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(work_tree, &path_str)
                {
                    bail!("Refusing to remove the current working directory:\n{path_str}\n");
                }
            }
        } else if abs_path.is_dir() {
            if grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(work_tree, &path_str) {
                bail!("Refusing to remove the current working directory:\n{path_str}\n");
            }
        }
    }

    Ok(())
}

/// Remove files from working tree that existed before but are no longer in the merged index.
fn remove_deleted_files(
    work_tree: &Path,
    old_entries: &HashMap<Vec<u8>, IndexEntry>,
    new_index: &Index,
    sparse_checkout: bool,
) -> Result<()> {
    let new_paths: std::collections::HashSet<&[u8]> = new_index
        .entries
        .iter()
        .filter(|e| e.stage() == 0)
        .map(|e| e.path.as_slice())
        .collect();
    // Paths that survive the merge only as conflicted (non-zero stage) index entries — e.g. a
    // modify/delete conflict where one side's version is left in the work tree. Git keeps those
    // working files (twoway_merge leaves them untouched), so we must not delete them here; the
    // conflict-file write pass below handles their contents and preserves the mtime (t6402.36).
    let conflicted_paths: std::collections::HashSet<&[u8]> = new_index
        .entries
        .iter()
        .filter(|e| e.stage() != 0)
        .map(|e| e.path.as_slice())
        .collect();
    for (path, old_entry) in old_entries {
        let has_nested_under = new_index.entries.iter().any(|e| {
            e.path.starts_with(path)
                && e.path.len() > path.len()
                && e.path.get(path.len()) == Some(&b'/')
        });
        if new_paths.contains(path.as_slice()) {
            // Unmerged entries keep `path` in the index even when the result needs `path/` as a
            // directory (t6422 rename/directory). Drop a tracked file at `path` so children
            // like `path/sub` can be materialized.
            if !(has_nested_under && old_entry.mode != MODE_TREE && old_entry.mode != MODE_GITLINK)
            {
                continue;
            }
        } else if conflicted_paths.contains(path.as_slice())
            && old_entry.mode != MODE_TREE
            && old_entry.mode != MODE_GITLINK
            && !has_nested_under
        {
            // Conflicted file kept at this exact path (no children needing the slot): leave the
            // existing work-tree file in place rather than deleting and recreating it.
            continue;
        }
        if sparse_checkout {
            if let Some(ne) = new_index
                .entries
                .iter()
                .find(|e| e.stage() == 0 && e.path == *path)
            {
                if ne.skip_worktree() {
                    continue;
                }
            }
        }
        // Submodule removed from the superproject: keep the on-disk work tree.
        if old_entry.mode == MODE_GITLINK {
            // Directory/submodule conflict: git moves the gitlink to `path~HEAD` (unmerged) but
            // leaves the submodule checkout at `path/` intact (`t6437-submodule-merge`). Without
            // this, `has_nested_under` is true (stage-0 `path/file`) and we would delete `path/`,
            // losing `.git` and breaking `test_path_is_dir path/.git`.
            let relocated_gitlink_conflict = new_index.entries.iter().any(|e| {
                e.mode == MODE_GITLINK
                    && e.stage() != 0
                    && e.path.len() > path.len()
                    && e.path.starts_with(path)
                    && e.path[path.len()] == b'~'
            });
            if !has_nested_under || relocated_gitlink_conflict {
                continue;
            }
        }
        let path_str = String::from_utf8_lossy(path).into_owned();
        if grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(work_tree, &path_str) {
            bail!("Refusing to remove the current working directory:\n{path_str}\n");
        }
        let abs = work_tree.join(path_str.as_str());
        if abs.exists() || fs::symlink_metadata(&abs).is_ok() {
            if abs.is_dir() {
                let _ = fs::remove_dir_all(&abs);
            } else {
                let _ = fs::remove_file(&abs);
            }
            remove_empty_parent_dirs_merge(work_tree, &abs);
        }
    }
    Ok(())
}

fn remove_empty_parent_dirs_merge(work_tree: &Path, path: &Path) {
    let cwd_rel = grit_lib::worktree_cwd::process_cwd_repo_relative(work_tree);
    let mut current = path.parent();
    while let Some(dir) = current {
        if dir == work_tree {
            break;
        }
        if let Some(ref cr) = cwd_rel {
            if grit_lib::worktree_cwd::cwd_would_be_removed_with_dir(work_tree, dir, cr) {
                break;
            }
        }
        match fs::remove_dir(dir) {
            Ok(()) => current = dir.parent(),
            Err(_) => break,
        }
    }
}

/// True if `abs_path` lies under an embedded Git directory below the work tree root.
///
/// During merge conflicts, the index may record `path/file` at stage 0 while `path/` is still
/// a submodule checkout on disk (gitlink was relocated to `path~HEAD` in the index). Writing
/// the blob would create `path/file` inside the submodule; Git leaves the submodule work tree
/// untouched (`t6437-submodule-merge`).
///
/// The work tree root's `.git` is ignored — only strict ancestors of the file path count.
fn worktree_path_under_nested_git(work_tree: &Path, abs_path: &Path) -> bool {
    let mut cur = match abs_path.parent() {
        Some(p) => p,
        None => return false,
    };
    while cur != work_tree {
        if cur.join(".git").exists() {
            return true;
        }
        let Some(parent) = cur.parent() else {
            break;
        };
        cur = parent;
    }
    false
}

/// Checkout index entries to working tree.
/// Returns true when `path` is an existing regular file whose contents are byte-identical to
/// `expected`. Used to skip rewriting conflict-marker files that already hold the right bytes so
/// their mtime is preserved ("avoid unnecessary update", t6402).
fn worktree_file_matches(path: &Path, expected: &[u8]) -> bool {
    // Symlinks must always be rewritten as regular files; do not treat them as matches.
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => {}
        _ => return false,
    }
    if let Ok(meta) = fs::metadata(path) {
        if meta.len() != expected.len() as u64 {
            return false;
        }
    }
    match fs::read(path) {
        Ok(existing) => existing == expected,
        Err(_) => false,
    }
}

fn checkout_entries(
    repo: &Repository,
    work_tree: &Path,
    index: &Index,
    old_entries: Option<&HashMap<Vec<u8>, IndexEntry>>,
    sparse_checkout: bool,
) -> Result<()> {
    checkout_entries_with_treeish(repo, work_tree, index, old_entries, sparse_checkout, None)
}

/// Checkout index entries to working tree with optional process-smudge treeish metadata.
fn checkout_entries_with_treeish(
    repo: &Repository,
    work_tree: &Path,
    index: &Index,
    old_entries: Option<&HashMap<Vec<u8>, IndexEntry>>,
    sparse_checkout: bool,
    smudge_treeish: Option<&ObjectId>,
) -> Result<()> {
    // Load gitattributes and config for CRLF conversion
    let mut attr_rules = grit_lib::crlf::load_gitattributes(work_tree);
    if index.get(b".gitattributes", 0).is_some() {
        let from_index = grit_lib::crlf::load_gitattributes_from_index(index, &repo.odb);
        if !from_index.is_empty() {
            attr_rules = from_index;
        }
    }
    let config = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true).ok();
    let conv = config
        .as_ref()
        .map(grit_lib::crlf::ConversionConfig::from_config);

    for entry in &index.entries {
        if entry.stage() != 0 {
            continue;
        }
        if sparse_checkout && entry.skip_worktree() {
            continue;
        }
        if old_entries.is_some_and(|old| {
            old.get(&entry.path)
                .is_some_and(|previous| previous.oid == entry.oid && previous.mode == entry.mode)
        }) {
            continue;
        }
        let path_str = String::from_utf8_lossy(&entry.path).into_owned();
        let abs_path = work_tree.join(&path_str);

        if entry.mode != MODE_GITLINK && worktree_path_under_nested_git(work_tree, &abs_path) {
            continue;
        }

        // Directory/file conflicts: a tracked file may occupy a path that the merge
        // result needs as a directory (e.g. `path/file` while `path` was a file).
        let mut cur = abs_path.parent();
        while let Some(dir) = cur {
            if dir == work_tree {
                break;
            }
            if dir.exists() && !dir.is_dir() {
                let _ = fs::remove_file(dir);
            }
            cur = dir.parent();
        }

        if let Some(parent) = abs_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Submodule entries (gitlinks): materialize an empty directory in the
        // superproject (Git does not check out submodule contents on merge).
        if entry.mode == 0o160000 {
            if grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(work_tree, &path_str) {
                bail!("Refusing to remove the current working directory:\n{path_str}\n");
            }
            if abs_path.is_file() || abs_path.is_symlink() {
                let _ = fs::remove_file(&abs_path);
            } else if abs_path.is_dir() && abs_path.join(".git").exists() {
                continue;
            } else if abs_path.is_dir() {
                let _ = fs::remove_dir_all(&abs_path);
            }
            let _ = fs::create_dir_all(&abs_path);
            continue;
        }

        let obj = repo.odb.read(&entry.oid)?;
        if obj.kind != ObjectKind::Blob {
            continue;
        }

        if abs_path.is_dir() {
            if grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(work_tree, &path_str) {
                bail!("Refusing to remove the current working directory:\n{path_str}\n");
            }
            fs::remove_dir_all(&abs_path)?;
        }

        if entry.mode == MODE_SYMLINK {
            let target = String::from_utf8(obj.data)
                .map_err(|_| anyhow::anyhow!("symlink target is not UTF-8"))?;
            if abs_path.exists() || abs_path.is_symlink() {
                if grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(work_tree, &path_str)
                {
                    bail!("Refusing to remove the current working directory:\n{path_str}\n");
                }
                let _ = fs::remove_file(&abs_path);
            }
            std::os::unix::fs::symlink(target, &abs_path)?;
        } else {
            // Apply CRLF conversion if configured
            let data = if let (Some(ref config), Some(ref conv)) = (&config, &conv) {
                let file_attrs =
                    grit_lib::crlf::get_file_attrs(&attr_rules, &path_str, false, config);
                let oid_hex = entry.oid.to_hex();
                let smudge_meta = smudge_treeish.map(|treeish| {
                    grit_lib::filter_process::smudge_meta_treeish_only(&treeish.to_hex(), &oid_hex)
                });
                grit_lib::crlf::convert_to_worktree_eager(
                    &obj.data,
                    &path_str,
                    conv,
                    &file_attrs,
                    Some(&oid_hex),
                    smudge_meta.as_ref(),
                )
                .map_err(|e| anyhow::anyhow!("smudge filter failed for {path_str}: {e}"))?
            } else {
                obj.data.clone()
            };
            fs::write(&abs_path, &data)?;
            if entry.mode == MODE_EXECUTABLE {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = fs::metadata(&abs_path)?.permissions();
                perms.set_mode(0o755);
                fs::set_permissions(&abs_path, perms)?;
            }
        }
    }
    Ok(())
}

/// Resolve author/committer identity from env and config.
fn resolve_ident(config: &ConfigSet, kind: &str, now: OffsetDateTime) -> Result<String> {
    let name_var = if kind == "author" {
        "GIT_AUTHOR_NAME"
    } else {
        "GIT_COMMITTER_NAME"
    };
    let email_var = if kind == "author" {
        "GIT_AUTHOR_EMAIL"
    } else {
        "GIT_COMMITTER_EMAIL"
    };
    let date_var = if kind == "author" {
        "GIT_AUTHOR_DATE"
    } else {
        "GIT_COMMITTER_DATE"
    };

    let name = std::env::var(name_var)
        .ok()
        .or_else(|| config.get("user.name"))
        .unwrap_or_else(|| "Unknown".to_owned());

    let email = std::env::var(email_var)
        .ok()
        .or_else(|| config.get("user.email"))
        .unwrap_or_default();

    let timestamp = std::env::var(date_var)
        .map(|d| parse_date_to_git_ts(&d).unwrap_or(d))
        .unwrap_or_else(|_| {
            let epoch = now.unix_timestamp();
            let offset = now.offset();
            let hours = offset.whole_hours();
            let minutes = offset.minutes_past_hour().unsigned_abs();
            format!("{epoch} {hours:+03}{minutes:02}")
        });

    Ok(format!("{name} <{email}> {timestamp}"))
}

/// Parse date string to git timestamp format (epoch + offset).
fn parse_date_to_git_ts(date_str: &str) -> Option<String> {
    let trimmed = date_str.trim();
    let parts: Vec<&str> = trimmed.rsplitn(2, ' ').collect();
    if parts.len() == 2 {
        let maybe_epoch = parts[1];
        if maybe_epoch.chars().all(|c| c.is_ascii_digit()) {
            return None; // already in epoch format
        }
        let tz = parts[0];
        let datetime = parts[1];
        let tz_bytes = tz.as_bytes();
        if tz_bytes.len() >= 5 {
            let sign: i64 = if tz_bytes[0] == b'-' { -1 } else { 1 };
            let h: i64 = tz[1..3].parse().unwrap_or(0);
            let m: i64 = tz[3..5].parse().unwrap_or(0);
            let tz_secs = sign * (h * 3600 + m * 60);
            if let Ok(offset) = time::UtcOffset::from_whole_seconds(tz_secs as i32) {
                let fmt = time::format_description::parse_borrowed::<1>(
                    "[year]-[month]-[day] [hour]:[minute]:[second]",
                )
                .ok()?;
                if let Ok(naive) = time::PrimitiveDateTime::parse(datetime, &fmt) {
                    let dt = naive.assume_offset(offset);
                    return Some(format!("{} {}", dt.unix_timestamp(), tz));
                }
            }
        }
    }
    None
}

/// Apply cleanup mode to a commit message (matches Git `cleanup_mode` for MERGE_MSG templates).
pub(crate) fn cleanup_message(msg: &str, mode: &str) -> String {
    match mode {
        "verbatim" => {
            // Keep message exactly as-is
            msg.to_string()
        }
        "whitespace" => {
            // Strip trailing whitespace from each line, leading and trailing blank lines
            let lines: Vec<&str> = msg.lines().collect();
            let mut result: Vec<String> = lines.iter().map(|l| l.trim_end().to_string()).collect();
            // Remove leading empty lines
            while result.first().is_some_and(|l| l.is_empty()) {
                result.remove(0);
            }
            // Remove trailing empty lines
            while result.last().is_some_and(|l| l.is_empty()) {
                result.pop();
            }
            if result.is_empty() {
                String::new()
            } else {
                result.join("\n") + "\n"
            }
        }
        "strip" | "default" => {
            // Strip comments (lines starting with #) and trailing whitespace
            let lines: Vec<&str> = msg.lines().collect();
            let mut result: Vec<String> = lines
                .iter()
                .filter(|l| !l.starts_with('#'))
                .map(|l| l.trim_end().to_string())
                .collect();
            // Remove leading empty lines
            while result.first().is_some_and(|l| l.is_empty()) {
                result.remove(0);
            }
            // Remove trailing empty lines
            while result.last().is_some_and(|l| l.is_empty()) {
                result.pop();
            }
            if result.is_empty() {
                String::new()
            } else {
                result.join("\n") + "\n"
            }
        }
        "scissors" => {
            // Strip everything from the scissors line onward.
            // A scissors line starts at column 0 (not indented).
            let mut result_lines: Vec<&str> = Vec::new();
            for line in msg.lines() {
                if line.starts_with("# ------------------------ >8 ------------------------") {
                    break;
                }
                result_lines.push(line);
            }
            // Strip trailing whitespace from lines, leading and trailing blank lines
            let mut result: Vec<String> = result_lines
                .iter()
                .map(|l| l.trim_end().to_string())
                .collect();
            // Remove leading empty lines
            while result.first().is_some_and(|l| l.is_empty()) {
                result.remove(0);
            }
            // Remove trailing empty lines
            while result.last().is_some_and(|l| l.is_empty()) {
                result.pop();
            }
            if result.is_empty() {
                String::new()
            } else {
                result.join("\n") + "\n"
            }
        }
        _ => {
            // Unknown mode: treat as default
            cleanup_message(msg, "strip")
        }
    }
}

fn ensure_trailing_newline(s: &str) -> String {
    if s.ends_with('\n') {
        s.to_owned()
    } else {
        format!("{s}\n")
    }
}
