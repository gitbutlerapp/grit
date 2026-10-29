//! Create a commit from the current index and move the checked-out branch.
//!
//! [`create_commit`] performs incremental cache-tree write-tree, writes the commit
//! object, persists the index with a valid cache-tree, then updates the branch and `HEAD`
//! reflogs through [`crate::refs::update_branch_for_commit_on_store`].

use crate::apply::{apply_patch_to_index, FilePatch, IndexApplyOptions};
use crate::change_id::{change_id, set_change_id, ChangeId, ChangeIdPolicy};
use crate::commit::assemble_identity;
use crate::commit::trailers::interpret::{
    process_trailers, NewTrailerArg, ProcessTrailerOptions, TrailerIfExists, TrailerIfMissing,
    TrailerWhere,
};
use crate::commit_trailers::{append_signoff_trailer_with_dedup, format_signoff_line};
use crate::config::ident_resolve::IdentRole;
use crate::config::ConfigSet;
use crate::diff::refresh_index_stat_content_verified_with_rules;
use crate::diff::{
    diff_tree_path_change_count, diff_trees_with_renames, flatten_tree_entries,
    materialize_worktree_blob, zero_oid, DiffEntry, DiffStatus,
};
use crate::error::ApplyError;
use crate::error::{Error, Result};
use crate::history::{ensure_rewrite_apply_ready, HistoryError};
use crate::hooks::{
    run_advisory_hook, run_commit_hook, run_post_rewrite, CommitHookEnv, HookError, HookName,
    PostOperationHooks, RewriteKind,
};
use crate::ident::{
    fsck_commit_idents, parse_signature_times, prepare_commit_author_from_parent,
    prepare_commit_identity_line, replace_identity_date_suffix, Signature,
};
use crate::index::{Index, IndexLock};
use crate::objects::odb::Odb;
use crate::objects::{parse_commit, serialize_commit, CommitData, ObjectId, ObjectKind};
use crate::pathspec::{matches_pathspec_list_with_context, pathspec_is_exclude, ResolvedPathspecs};
use crate::porcelain::add::{
    stage_on_index, validate_commit_staging_pathspecs, StageMode, StageOptions,
};
use crate::porcelain::hunk_select::{
    diff_options_for_hunk_select, resolve_selection, HunkSelectError, HunkSelection,
};
use crate::porcelain::index_from_tree::index_from_tree_with_seeded_worktree_stats;
use crate::porcelain::pathspec_match::KnownPathspecChecker;
use crate::porcelain::replay_sequence::finish_replay_commit_state;
use crate::progress::{poll_cancel, Cancel, ProgressSink};
use crate::refs::{
    update_branch_for_commit_on_store, update_detached_head_for_commit_on_store,
    BranchCommitRefUpdate,
};
use crate::repo::Repository;
use crate::signing::{
    extra_headers_without_commit_signatures, should_sign_commit, sign_serialized_commit, GpgConfig,
};
use crate::state::{
    finish_merge_state, read_cherry_pick_head, read_merge_heads, read_merge_msg, read_revert_head,
    resolve_head_for_repository, HeadState, InProgressOperation,
};
use crate::stripspace::{process, Mode};
use crate::worktree_rules::WorktreeRules;
use crate::write_tree::{
    cache_tree_extension_valid, is_empty_tree_oid, write_tree_update_index, WriteTreeFlags,
};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::{Arc, Mutex};
use thiserror::Error;

/// Whether the verifying hooks run before a commit object is written.
///
/// The verifying hooks are `pre-commit` (or `pre-merge-commit` for a merge) and `commit-msg`.
/// `prepare-commit-msg` and the `post-*` hooks run under either policy, as in Git.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HookPolicy {
    /// Run the verifying hooks when installed (default).
    #[default]
    Run,
    /// Skip the verifying hooks (`--no-verify`).
    SkipVerify,
}

/// Where a commit message came from, as `prepare-commit-msg` is told in its second (and third)
/// argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageSource {
    /// Given by the caller (`-m`, `-F`).
    Message,
    /// Reused from this commit (`--amend`, `-C`).
    Commit(ObjectId),
    /// The merge message in `MERGE_MSG`.
    Merge,
    /// The squash message in `SQUASH_MSG`.
    Squash,
}

/// Run `prepare-commit-msg` and then, unless `verify` is [`HookPolicy::SkipVerify`],
/// `commit-msg` on the message file at `msg_path`, appending each hook that ran to `hooks_run`.
///
/// Returns the file's contents (with a trailing newline) when either hook ran, since a hook
/// may have rewritten it, and `None` when neither is installed.
///
/// # Errors
///
/// [`Error::Hook`] when a hook fails or cannot start, [`Error::Io`] when the file cannot be
/// read back, and [`Error::EmptyCommitMessage`] when a hook left it empty.
pub(crate) fn run_commit_message_hooks(
    repo: &Repository,
    msg_path: &Path,
    source: MessageSource,
    verify: HookPolicy,
    commit_env: &CommitHookEnv<'_>,
    hooks_run: &mut Vec<HookName>,
) -> Result<Option<String>> {
    let path_arg = commit_editmsg_hook_argument(repo, msg_path);
    let source_oid;
    let prepare_args: Vec<&str> = match source {
        MessageSource::Message => vec![&path_arg, "message"],
        MessageSource::Commit(oid) => {
            source_oid = oid.to_hex();
            vec![&path_arg, "commit", &source_oid]
        }
        MessageSource::Merge => vec![&path_arg, "merge"],
        MessageSource::Squash => vec![&path_arg, "squash"],
    };
    let ran_before = hooks_run.len();
    if run_commit_hook_recorded(
        repo,
        HookName::PrepareCommitMsg,
        &prepare_args,
        None,
        commit_env,
    )? {
        hooks_run.push(HookName::PrepareCommitMsg);
    }
    if verify != HookPolicy::SkipVerify
        && run_commit_hook_recorded(repo, HookName::CommitMsg, &[&path_arg], None, commit_env)?
    {
        hooks_run.push(HookName::CommitMsg);
    }
    if hooks_run.len() == ran_before {
        return Ok(None);
    }
    let mut message = fs::read_to_string(msg_path).map_err(Error::Io)?;
    if message.trim().is_empty() {
        return Err(Error::EmptyCommitMessage);
    }
    if !message.ends_with('\n') {
        message.push('\n');
    }
    Ok(Some(message))
}

/// Author name, email, and date suffix from a written commit header line.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
pub struct CommitAuthorInfo {
    /// Author display name.
    pub name: String,
    /// Author email.
    pub email: String,
    /// Git timestamp suffix (`<unix> <+HHMM>`).
    pub date: String,
}

fn guard_incomplete_rewrite_apply(repo: &Repository) -> Result<()> {
    ensure_rewrite_apply_ready(repo).map_err(|err| match err {
        HistoryError::RewriteApplyIncomplete => {
            Error::OperationInProgress(InProgressOperation::RewriteApply)
        }
        other => Error::History(other),
    })
}

/// Inputs for [`create_commit`].
#[derive(Debug, Clone, Default)]
pub struct CommitRequest {
    /// Commit message body (first line becomes the subject). A trailing newline is added when missing.
    ///
    /// The message is also written to `.git/COMMIT_EDITMSG` for the `prepare-commit-msg` and
    /// `commit-msg` hooks. The file is read back only when one of them ran, so concurrent
    /// committers sharing the file never record each other's message.
    pub message: String,
    /// Tell `prepare-commit-msg` that [`Self::message`] is the in-progress merge's `MERGE_MSG`
    /// (source `merge` instead of `message`), as when finishing a merge.
    #[doc(hidden)]
    pub message_from_merge: bool,
    /// Author to record. `None` keeps the replaced commit's author for [`Self::amend`] and the
    /// source commit's author for [`Self::reuse_message_from`]; otherwise the repository's
    /// author identity ([`Repository::identity`]) is used. [`Self::author_date`] and
    /// [`Self::reset_author`] still apply.
    pub author: Option<Signature>,
    /// Committer to record; `None` uses the repository's committer identity
    /// ([`Repository::identity`]).
    pub committer: Option<Signature>,
    /// When false (default), reject commits whose tree equals the parent's tree or is the empty tree on an unborn branch.
    pub allow_empty: bool,
    /// When `Some(true)`, sign even if `commit.gpgsign` is false; when `Some(false)`, skip signing even when config requests it. When `None`, honor `commit.gpgsign`.
    pub sign_override: Option<bool>,
    /// Replace the current branch tip: parents and author come from the tip being replaced; an empty
    /// [`Self::message`] reuses that commit's message. Same-tree amends are allowed.
    pub amend: bool,
    /// When true, skip the post-commit full-tree index stat refresh because the caller already
    /// synced the index from the worktree (for example via [`crate::porcelain::add::stage_for_commit`]).
    ///
    /// [`commit_worktree`] and every [`commit_with_scope`] scope set this themselves, since their
    /// staging pass already refreshed the index; only [`create_commit`] callers need to choose.
    pub assume_worktree_synced: bool,
    /// When true with [`Self::assume_worktree_synced`], skip the pre-commit index↔parent tree
    /// match; the post-`write_tree` parent comparison still rejects empty commits (issue #1064).
    #[doc(hidden)]
    pub skip_pre_commit_tree_match: bool,
    /// When set with [`Self::skip_pre_commit_tree_match`], use this instead of a full tree diff for
    /// [`CommitOutcome::changes`] (issue #1064).
    #[doc(hidden)]
    pub staged_change_count: Option<usize>,
    /// Hook execution policy for `pre-commit` and `commit-msg`; `prepare-commit-msg` runs either
    /// way.
    #[doc(alias = "no-verify")]
    pub hooks: HookPolicy,
    /// Author timestamp suffix (`<unix> <+HHMM>`) replacing the author line date when set.
    pub author_date: Option<String>,
    /// Trailers to append via [`process_trailers`] (same semantics as `git commit --trailer`).
    pub trailers: Vec<(String, String)>,
    /// Append `Signed-off-by` using the committer identity (`-s` / `--signoff`).
    pub signoff: bool,
    /// Use the committer as the author with a fresh author date (`--reset-author`).
    pub reset_author: bool,
    /// Reuse log message and author from this commit (`-C` / `--reuse-message`); copies get a new change id.
    pub reuse_message_from: Option<ObjectId>,
    /// When true, compute the commit tree and file list without writing objects, updating refs, or running hooks.
    pub dry_run: bool,
    /// When true, a [`Error::NothingToCommit`] is upgraded to [`Error::NothingStagedToCommit`] if the
    /// work tree still has unstaged or untracked paths (index-only commit; issue #1969).
    #[doc(hidden)]
    pub index_only: bool,
    /// Whether an amend runs `post-rewrite amend` ([`PostOperationHooks::Skip`] is Git's
    /// `--no-post-rewrite`). `post-commit` runs either way.
    pub post_rewrite: PostOperationHooks,
}

/// One path-level change that would be recorded in a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
pub struct CommitFile {
    /// Primary path (`git diff --name-status` path column).
    pub path: String,
    /// Kind of change vs the parent commit tree.
    pub status: DiffStatus,
    /// Previous path for renames and copies.
    pub old_path: Option<String>,
}

/// Planned commit contents and metadata (see [`plan_commit`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitPlan {
    /// Paths that differ from the parent tree, in `git diff --name-status` order.
    pub files: Vec<CommitFile>,
    /// Parent OIDs that would be stored on the new commit.
    pub parents: Vec<ObjectId>,
    /// Author line that would be recorded.
    pub author: CommitAuthorInfo,
    /// Whether the computed tree OID equals the parent tree.
    pub tree_unchanged: bool,
}

/// Which paths are included when recording a commit ([`commit_with_scope`]).
#[derive(Debug, Clone)]
pub enum CommitScope {
    /// Stage every worktree change, then commit (default `grit commit` behavior).
    AutoStage,
    /// Commit the index as-is (`git commit` without `-a`).
    Staged,
    /// Commit only matching paths from the worktree (`git commit --only --` paths).
    Paths(ResolvedPathspecs),
    /// Commit only hunks from a unified diff or stable hunk ids (`grit commit --hunks` / `--patch-file`).
    Patch {
        /// Hunk ids or pre-parsed file patches (exactly one is populated).
        input: CommitPatchInput,
        /// Optional pathspecs narrowing hunk resolution (same as `grit add --hunks`).
        pathspecs: Vec<String>,
    },
}

impl PartialEq for CommitScope {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::AutoStage, Self::AutoStage) | (Self::Staged, Self::Staged) => true,
            (Self::Paths(a), Self::Paths(b)) => a == b,
            (
                Self::Patch {
                    input: a,
                    pathspecs: ap,
                },
                Self::Patch {
                    input: b,
                    pathspecs: bp,
                },
            ) => ap == bp && patch_input_same_kind(a, b),
            _ => false,
        }
    }
}

impl Eq for CommitScope {}

fn patch_input_same_kind(a: &CommitPatchInput, b: &CommitPatchInput) -> bool {
    matches!(
        (a, b),
        (CommitPatchInput::Hunks(_), CommitPatchInput::Hunks(_))
            | (CommitPatchInput::Patches(_), CommitPatchInput::Patches(_))
    )
}

/// Hunk ids or parsed patches for [`CommitScope::Patch`].
#[derive(Debug, Clone)]
pub enum CommitPatchInput {
    /// Stable ids from `grit diff --json`.
    Hunks(HunkSelection),
    /// Unified diff hunks (`git apply --cached` preimage = index).
    Patches(Vec<FilePatch>),
}

/// Failure while resolving or applying a patch-scoped commit.
#[derive(Debug, Error)]
pub enum CommitPatchError {
    /// Hunk id resolution failed (unknown, stale, wrong side, binary).
    #[error(transparent)]
    Select(#[from] HunkSelectError),
    /// Index apply failed after resolution.
    #[error(transparent)]
    Apply(#[from] ApplyError),
    /// Commit, index, or repository error.
    #[error(transparent)]
    Commit(#[from] Error),
}

/// Default [`CommitScope`] from `commit.autoStage` (default true → [`CommitScope::AutoStage`]).
///
/// # Errors
///
/// Propagates config read failures from [`Repository::config`].
pub fn default_commit_scope(repo: &Repository) -> Result<CommitScope> {
    let config = repo.config()?;
    Ok(default_commit_scope_from_config(config.as_ref()))
}

/// Read `commit.autoStage` (default true).
#[must_use]
pub fn default_commit_scope_from_config(config: &ConfigSet) -> CommitScope {
    let auto_stage = config
        .get_bool("commit.autoStage")
        .and_then(|r| r.ok())
        .unwrap_or(true);
    if auto_stage {
        CommitScope::AutoStage
    } else {
        CommitScope::Staged
    }
}

/// Remove squash merge sentinel files after a successful commit (Git removes `SQUASH_MSG` on commit).
///
/// # Errors
///
/// Returns [`Error::Io`] if a present file cannot be removed.
pub(crate) fn clear_squash_commit_state(git_dir: &std::path::Path) -> Result<()> {
    let squash = git_dir.join("SQUASH_MSG");
    if !squash.is_file() {
        return Ok(());
    }
    fs::remove_file(&squash).map_err(Error::Io)?;
    if !git_dir.join("MERGE_HEAD").is_file() {
        let merge_msg = git_dir.join("MERGE_MSG");
        if merge_msg.is_file() {
            fs::remove_file(&merge_msg).map_err(Error::Io)?;
        }
    }
    Ok(())
}

/// Default message for the next commit when `SQUASH_MSG` or merge `MERGE_MSG` is present.
///
/// # Errors
///
/// Propagates I/O and merge-state read failures.
pub fn pending_commit_message(repo: &Repository) -> Result<Option<String>> {
    let git_dir = repo.git_dir();
    let squash = git_dir.join("SQUASH_MSG");
    let merge_msg = git_dir.join("MERGE_MSG");
    // Squash merge conflicts store the editable message (including `-m`) in `MERGE_MSG`.
    if squash.is_file() && merge_msg.is_file() {
        return fs::read_to_string(merge_msg).map_err(Error::Io).map(Some);
    }
    if squash.is_file() {
        return fs::read_to_string(squash).map_err(Error::Io).map(Some);
    }
    if !read_merge_heads(git_dir)?.is_empty() {
        return read_merge_msg(git_dir);
    }
    if read_cherry_pick_head(git_dir)?.is_some() || read_revert_head(git_dir)?.is_some() {
        let Some(raw) = read_merge_msg(git_dir)? else {
            return Ok(None);
        };
        let processed = process(raw.as_bytes(), &Mode::StripComments("#".to_owned()));
        let message = String::from_utf8(processed).map_err(|_| Error::EmptyCommitMessage)?;
        if message.trim().is_empty() {
            return Ok(None);
        }
        return Ok(Some(message));
    }
    Ok(None)
}

fn replay_blocks_partial_commit(git_dir: &std::path::Path) -> Result<bool> {
    Ok(read_cherry_pick_head(git_dir)?.is_some() || read_revert_head(git_dir)?.is_some())
}

/// Record a commit using `scope` to decide how the index is prepared.
///
/// Staging reports [`crate::progress::ProgressPhase::StagingFiles`] to `progress`; `cancel` (or
/// [`ProgressSink::should_cancel`]) stops the commit with [`Error::Cancelled`] up to the point
/// the commit object is written, leaving `HEAD` and the branch where they were.
///
/// # Errors
///
/// Same as [`create_commit`] and [`commit_worktree`], plus:
///
/// - [`Error::CommitPathsRequired`] for an empty [`CommitScope::Paths`] list.
/// - [`Error::PartialCommitDuringMerge`] for [`CommitScope::Paths`] or [`CommitScope::Patch`]
///   during an in-progress merge, and pathspec validation errors.
/// - [`Error::HunkSelection`] when a [`CommitScope::Patch`] hunk id is unknown, stale, binary, or
///   for the wrong side, and [`Error::Apply`] when the selected patch does not apply to the
///   index. These carry the same values as [`CommitPatchError::Select`] and
///   [`CommitPatchError::Apply`] from [`commit_with_patch`].
pub fn commit_with_scope(
    repo: &Repository,
    req: &CommitRequest,
    scope: CommitScope,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<CommitOutcome> {
    guard_incomplete_rewrite_apply(repo)?;
    match scope {
        CommitScope::AutoStage => commit_worktree(repo, req, progress, cancel),
        CommitScope::Staged => {
            let mut req = req.clone();
            // Index stats were refreshed when paths were staged; skip full-tree refresh (#1622).
            req.assume_worktree_synced = true;
            req.index_only = true;
            create_commit(repo, &req, progress, cancel)
        }
        CommitScope::Paths(paths) => commit_paths_scope(repo, req, &paths, progress, cancel),
        CommitScope::Patch { input, pathspecs } => {
            commit_with_patch(repo, req, input, &pathspecs, progress, cancel).map_err(|e| match e {
                CommitPatchError::Select(err) => Error::HunkSelection(err),
                CommitPatchError::Apply(err) => Error::Apply(err),
                CommitPatchError::Commit(err) => err,
            })
        }
    }
}

/// Record a commit after applying `input` to the current index (partial stage + commit).
///
/// Applies the selection in memory, commits the resulting tree, then writes the updated index.
/// Does not auto-stage the worktree (same effective scope as [`CommitScope::Staged`]).
///
/// # Errors
///
/// [`CommitPatchError::Select`] for hunk id failures; [`CommitPatchError::Apply`] for patch apply;
/// same commit errors as [`create_commit`].
pub fn commit_with_patch(
    repo: &Repository,
    req: &CommitRequest,
    input: CommitPatchInput,
    pathspecs: &[String],
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> std::result::Result<CommitOutcome, CommitPatchError> {
    guard_incomplete_rewrite_apply(repo).map_err(CommitPatchError::Commit)?;
    commit_patch_scope(repo, req, input, pathspecs, progress, cancel)
}

fn commit_patch_scope(
    repo: &Repository,
    req: &CommitRequest,
    input: CommitPatchInput,
    pathspecs: &[String],
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> std::result::Result<CommitOutcome, CommitPatchError> {
    if !read_merge_heads(repo.git_dir())?.is_empty() {
        return Err(Error::PartialCommitDuringMerge.into());
    }
    if replay_blocks_partial_commit(repo.git_dir())? {
        return Err(Error::PartialCommitDuringReplay.into());
    }
    let index_lock = (!req.dry_run)
        .then(|| repo.begin_index_update())
        .transpose()?;

    let patches = match input {
        CommitPatchInput::Hunks(selection) => {
            if selection.ids.is_empty() && selection.ranges.is_empty() {
                return Err(Error::NothingToCommit.into());
            }
            resolve_selection(
                repo,
                crate::diff::hunk_id::DiffSide::Unstaged,
                &selection,
                pathspecs,
                diff_options_for_hunk_select(repo).map_err(CommitPatchError::Commit)?,
            )?
        }
        CommitPatchInput::Patches(patches) => {
            if patches.is_empty() {
                return Err(Error::NothingToCommit.into());
            }
            patches
        }
    };

    let head = resolve_head_for_repository(repo).map_err(CommitPatchError::Commit)?;
    let mut index = repo.load_index().map_err(CommitPatchError::Commit)?;
    if index.has_unmerged_entries() {
        return Err(Error::IndexUnmerged.into());
    }

    let dry_run = req.dry_run;
    let _patch_overlay = if dry_run {
        repo.odb().enable_mem_overlay();
        Some(MemOverlayGuard(repo.odb()))
    } else {
        None
    };

    apply_patch_to_index(
        repo,
        &mut index,
        &patches,
        &IndexApplyOptions {
            dry_run: false,
            ..IndexApplyOptions::default()
        },
    )?;

    let mut req = req.clone();
    req.assume_worktree_synced = true;
    let persist = index_lock.map(|lock| LockedIndex { index: None, lock });
    create_commit_with_index(repo, head, &mut index, &req, progress, cancel, persist)
        .map_err(CommitPatchError::Commit)
}

/// Preview a commit for `scope` without mutating the repository (objects, index on disk, refs, hooks).
///
/// Uses in-memory blob hashing during staging (no new loose objects) and returns the same validation
/// errors as [`commit_with_scope`] for unmerged index and nothing-to-commit.
///
/// # Errors
///
/// Same as [`commit_with_scope`].
pub fn plan_commit(
    repo: &Repository,
    req: &CommitRequest,
    scope: CommitScope,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<CommitPlan> {
    let mut req = req.clone();
    req.dry_run = true;
    let outcome = commit_with_scope(repo, &req, scope, progress, cancel)?;
    Ok(CommitPlan {
        files: outcome.files,
        parents: outcome.parents,
        author: outcome.author,
        tree_unchanged: outcome.changes == 0,
    })
}

/// Copy stage-0 rows from `source` into `commit_index` when they are absent from the parent-tree
/// snapshot but needed for path-scoped staging.
///
/// Path-scoped commits build `commit_index` from the parent tree. Ignored paths that are
/// already staged in the repository index (force-added or tracked) must appear there before
/// worktree staging runs, or ignored untracked collection skips them. Rows already present from
/// the parent tree are left untouched so explicitly excluded paths keep their HEAD versions.
fn seed_commit_index_stage0_from_index(
    repo: &Repository,
    commit_index: &mut Index,
    source: &Index,
    paths: &ResolvedPathspecs,
) -> Result<()> {
    if paths.specs.is_empty() {
        return Ok(());
    }
    let ctx = repo.pathspec_match_context();
    let mut seeded = false;
    for entry in &source.entries {
        if entry.stage() != 0 {
            continue;
        }
        let path = match std::str::from_utf8(&entry.path) {
            Ok(p) if !p.is_empty() => p,
            _ => continue,
        };
        if commit_index.get(path.as_bytes(), 0).is_some() {
            continue;
        }
        if !matches_pathspec_list_with_context(path, &paths.specs, ctx) {
            continue;
        }
        commit_index.stage_file(entry.clone());
        seeded = true;
    }
    if seeded {
        commit_index.ensure_canonical_order();
    }
    Ok(())
}

fn commit_paths_scope(
    repo: &Repository,
    req: &CommitRequest,
    paths: &ResolvedPathspecs,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<CommitOutcome> {
    if paths.specs.is_empty() {
        return Err(Error::CommitPathsRequired);
    }
    if !read_merge_heads(repo.git_dir())?.is_empty() {
        return Err(Error::PartialCommitDuringMerge);
    }
    if replay_blocks_partial_commit(repo.git_dir())? {
        return Err(Error::PartialCommitDuringReplay);
    }
    validate_commit_pathspecs(repo, paths)?;

    // Lock before reading HEAD and the index so nothing visible (objects, ref, reflog) is written
    // when another process holds it, and so nobody can commit or change the index under us.
    let index_lock = (!req.dry_run)
        .then(|| repo.begin_index_update())
        .transpose()?;
    let head = resolve_head_for_repository(repo)?;
    let parent_tree = match &head {
        HeadState::Branch { oid: Some(p), .. } | HeadState::Detached { oid: p } => {
            Some(repo.odb().commit_tree(p)?)
        }
        HeadState::Branch { oid: None, .. } => None,
        HeadState::Invalid => {
            return Err(Error::InvalidHead);
        }
    };

    let mut real_index = repo.load_index()?;
    if real_index.has_unmerged_entries() {
        return Err(Error::IndexUnmerged);
    }
    let mut stage_opts = StageOptions {
        pathspecs: paths.specs.clone(),
        pathspec_sources: if paths.sources.is_empty() {
            paths.specs.clone()
        } else {
            paths.sources.clone()
        },
        mode: StageMode::All,
        dry_run: req.dry_run,
        ..StageOptions::default()
    };

    validate_commit_staging_pathspecs(repo, &real_index, &stage_opts)?;
    stage_opts.skip_pathspec_ignore_check = true;

    let mut commit_index = match parent_tree.as_ref() {
        Some(tree) => index_from_tree_with_seeded_worktree_stats(repo, tree, &real_index)?,
        None => Index::new(),
    };
    seed_commit_index_stage0_from_index(repo, &mut commit_index, &real_index, paths)?;

    let staged_commit = stage_on_index(repo, &mut commit_index, &stage_opts, progress, cancel)?;
    let real_index = match index_lock {
        Some(lock) => {
            stage_on_index(repo, &mut real_index, &stage_opts, progress, cancel)?;
            Some(LockedIndex {
                index: Some(real_index),
                lock,
            })
        }
        None => None,
    };

    let mut req = req.clone();
    req.assume_worktree_synced = true;
    req.skip_pre_commit_tree_match = false;
    req.staged_change_count = Some(staged_commit.total().max(1));
    req.index_only = true;

    create_commit_with_index(
        repo,
        head,
        &mut commit_index,
        &req,
        progress,
        cancel,
        real_index,
    )
}

/// The repository index to persist after a commit, with the lock taken before the commit started.
///
/// Hooks read a temporary copy of the commit index; the repository index is written through the
/// lock only after the ref moved. Dropping it unwritten releases the lock and leaves the on-disk
/// index untouched.
struct LockedIndex {
    /// Index to persist when it differs from the committed one (path-scoped commits keep other
    /// staged changes); `None` persists the committed index itself.
    index: Option<Index>,
    lock: IndexLock,
}

fn validate_commit_pathspecs(repo: &Repository, paths: &ResolvedPathspecs) -> Result<()> {
    let pairs: Vec<(&str, &str)> =
        if paths.sources.len() == paths.specs.len() && !paths.sources.is_empty() {
            paths
                .sources
                .iter()
                .zip(paths.specs.iter())
                .map(|(a, b)| (a.as_str(), b.as_str()))
                .collect()
        } else {
            paths
                .specs
                .iter()
                .map(|s| (s.as_str(), s.as_str()))
                .collect()
        };
    let mut checker = KnownPathspecChecker::new(repo);
    for (source, resolved) in pairs {
        if pathspec_is_exclude(source) {
            continue;
        }
        checker.ensure(source, resolved)?;
    }
    Ok(())
}

/// Normalize a commit message body the same way [`create_commit`] does before hooks: stripspace,
/// reject empty bodies, ensure a trailing newline.
///
/// # Errors
///
/// Returns [`Error::EmptyCommitMessage`] when the cleaned body would be empty.
pub fn normalize_commit_message(raw: &str) -> Result<String> {
    let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
    let processed = process(normalized.as_bytes(), &Mode::Default);
    let message = String::from_utf8(processed)
        .map_err(|_| Error::Message("invalid commit message".into()))?;
    if message.is_empty() {
        return Err(Error::EmptyCommitMessage);
    }
    let mut message = message;
    while message.ends_with("\n\n") {
        message.pop();
    }
    if !message.ends_with('\n') {
        message.push('\n');
    }
    Ok(message)
}

fn run_commit_hook_recorded(
    repo: &Repository,
    hook: HookName,
    args: &[&str],
    stdin_data: Option<&[u8]>,
    commit_env: &CommitHookEnv<'_>,
) -> std::result::Result<bool, HookError> {
    run_commit_hook(repo, hook.as_str(), args, stdin_data, commit_env)?.check(hook.as_str())
}

fn author_info_from_header_line(line: &str) -> CommitAuthorInfo {
    let line = line.trim();
    let date = parse_signature_times(line)
        .map(|p| {
            let tz = &line[p.tz_hhmm_range];
            format!("{} {tz}", p.unix_seconds)
        })
        .unwrap_or_default();
    let contact_end = line.rfind('>').map(|i| i + 1).unwrap_or(line.len());
    let contact = line[..contact_end].trim();
    let (name, email) = if let Some(lt) = contact.find('<') {
        if let Some(gt) = contact.find('>') {
            (contact[..lt].trim(), contact[lt + 1..gt].trim())
        } else {
            (contact, "")
        }
    } else if contact.contains('@') && !contact.chars().any(char::is_whitespace) {
        ("", contact)
    } else {
        (contact, "")
    };
    CommitAuthorInfo {
        name: name.to_owned(),
        email: email.to_owned(),
        date,
    }
}

fn author_line_from_committer(
    committer: &str,
    author_date: Option<&str>,
    now: time::OffsetDateTime,
) -> String {
    let info = author_info_from_header_line(committer);
    assemble_identity(&info.name, &info.email, author_date, now)
}

fn enrich_commit_message(
    message: String,
    trailers: &[(String, String)],
    signoff: bool,
    committer: &str,
    config: &ConfigSet,
    git_dir: &Path,
) -> Result<String> {
    let mut out = message;
    if signoff {
        let info = author_info_from_header_line(committer);
        let sob = format_signoff_line(&info.name, &info.email);
        append_signoff_trailer_with_dedup(&mut out, &sob, config, true);
    }
    if !trailers.is_empty() {
        let new_args: Vec<NewTrailerArg> = trailers
            .iter()
            .map(|(key, value)| NewTrailerArg {
                text: if value.is_empty() {
                    format!("{key}:")
                } else {
                    format!("{key}: {value}")
                },
                where_: TrailerWhere::End,
                if_exists: TrailerIfExists::AddIfDifferent,
                if_missing: TrailerIfMissing::Add,
            })
            .collect();
        let opts = ProcessTrailerOptions {
            only_trailers: false,
            trim_empty: true,
            only_input: false,
            unfold: false,
            no_divider: false,
        };
        out = process_trailers(&out, &opts, &new_args, Some(git_dir));
    }
    Ok(out)
}

/// Result of [`create_commit`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
pub struct CommitOutcome {
    /// OID of the new commit object written to the object database.
    pub oid: ObjectId,
    /// Short name of the branch the commit advanced (e.g. `main`), or `None` when `HEAD` is
    /// detached and only `HEAD` moved.
    pub branch: Option<String>,
    /// Parent commit OID, or `None` for the root commit on an unborn branch.
    pub parent: Option<ObjectId>,
    /// Every parent OID stored in the new commit object (empty only before the first commit is written).
    pub parents: Vec<ObjectId>,
    /// Author recorded on the commit object.
    pub author: CommitAuthorInfo,
    /// Hooks that were installed and ran during this commit, in order: `pre-commit`,
    /// `prepare-commit-msg`, `commit-msg`, `post-commit`, and `post-rewrite` (with `amend`) when
    /// [`CommitRequest::amend`] is set.
    pub hooks_run: Vec<HookName>,
    /// Number of path-level changes between the parent commit tree and this commit's tree.
    pub changes: usize,
    /// Stored `change-id` header when present on the new commit object.
    pub change_id: Option<ChangeId>,
    /// Path-level changes vs the parent tree (`git diff --name-status` between parent and new tree).
    pub files: Vec<CommitFile>,
}

impl CommitOutcome {
    /// Whether the commit was made on a detached `HEAD` ([`Self::branch`] is `None`).
    #[must_use]
    pub fn is_detached(&self) -> bool {
        self.branch.is_none()
    }
}

/// Write a commit from the current index and advance the checked-out branch.
///
/// Loads the index from disk, builds the commit tree with an incremental cache-tree
/// write-tree, writes the commit object, writes the index back with an updated cache-tree,
/// then atomically updates the branch ref and reflogs (see [`crate::refs::update_branch_for_commit_on_store`]).
///
/// # Parameters
///
/// - `repo` — open repository with a work tree and branch `HEAD`.
/// - `req` — message, author/committer identities, and empty-commit policy.
/// - `progress` — progress sink; staging callers see [`crate::progress::ProgressPhase::StagingFiles`].
/// - `cancel` — checked before anything is written and again before the commit object is
///   written; after that the branch update always completes.
///
/// # Errors
///
/// - [`Error::Cancelled`] when `cancel` or [`ProgressSink::should_cancel`] fired before the
///   commit object was written; `HEAD` and the branch are unchanged. Hook failures and cancel
///   after hooks leave the on-disk index unchanged when the caller holds an index lock (for
///   example [`commit_worktree`] and path-scoped commits).
/// - [`Error::AmendUnborn`] when [`CommitRequest::amend`] is set but the branch has no commits yet.
/// - [`Error::NothingToCommit`] when the new tree is empty (unborn branch) or matches the parent tree and [`CommitRequest::allow_empty`] is false (not applied when amending).
/// - [`Error::IndexUnmerged`] when the index has conflict stages.
/// - Index write or diff failures before the branch is updated; the branch tip is not advanced.
/// - Ref/reflog failures from [`crate::refs::update_branch_for_commit_on_store`]; the branch tip is not left advanced without reflogs when logging is enabled.
pub fn create_commit(
    repo: &Repository,
    req: &CommitRequest,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<CommitOutcome> {
    guard_incomplete_rewrite_apply(repo)?;
    let head = resolve_head_for_repository(repo)?;
    let mut index = repo.load_index()?;
    create_commit_with_index(repo, head, &mut index, req, progress, cancel, None)
}

/// Stage every worktree change and record a commit using a single in-memory index (issue #1064).
///
/// Avoids an extra index load/write between implicit staging and commit tree construction. The
/// staging pass is the only work-tree scan: it refreshes the stat data of every tracked entry, so
/// the commit always runs as if [`CommitRequest::assume_worktree_synced`] were set.
///
/// When staging changed the index but the result matches the parent tree (for example a new
/// file that was staged and then deleted from the work tree), the staged index is still written
/// before [`Error::NothingToCommit`] is returned, so the on-disk index matches `HEAD` afterwards.
/// [`CommitRequest::dry_run`] never writes the index, refs, or stored objects. Symlinks are
/// compared and previewed as link-target bytes, including directory and dangling links.
///
/// Staging reports [`crate::progress::ProgressPhase::StagingFiles`] and polls `cancel`; a cancel
/// during staging returns [`Error::Cancelled`] with the on-disk index, `HEAD` and refs unchanged.
/// A failing `pre-commit`, `prepare-commit-msg`, or `commit-msg` hook also leaves the on-disk
/// index and worktree unchanged (issue #2253): auto-staging runs in memory and hooks read a
/// temporary index file until the ref moves.
///
/// # Errors
///
/// Same as [`create_commit`], plus staging failures from [`crate::porcelain::add::stage_for_commit`],
/// including [`Error::UnmergedConflictMarkers`] when a path's conflict stages were dropped
/// (after a merge, pick, revert, rebase or stash pop) while its worktree file still has
/// a conflict-marker block that its `HEAD` version does not. Neither the index nor `HEAD` moves in that case.
pub fn commit_worktree(
    repo: &Repository,
    req: &CommitRequest,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<CommitOutcome> {
    guard_incomplete_rewrite_apply(repo)?;
    let index_lock = (!req.dry_run)
        .then(|| repo.begin_index_update())
        .transpose()?;
    let head = resolve_head_for_repository(repo)?;
    let mut index = repo.load_index()?;
    let staged = crate::porcelain::add::stage_for_commit_on_index(
        repo,
        &mut index,
        progress,
        cancel,
        req.dry_run,
    )?;
    let staged_count = staged.total();
    let stat_refreshed = staged.stat_refreshed;
    if index.has_unmerged_entries() {
        return Err(Error::IndexUnmerged);
    }
    let mut req = req.clone();
    req.assume_worktree_synced = true;
    if req.skip_pre_commit_tree_match {
        req.staged_change_count = Some(staged_count);
    }
    let persist = index_lock.map(|lock| LockedIndex { index: None, lock });
    let result = create_commit_with_index(repo, head, &mut index, &req, progress, cancel, persist);
    if matches!(result, Err(Error::NothingToCommit))
        && !req.dry_run
        && (staged_count > 0 || stat_refreshed)
    {
        repo.write_index(&mut index)?;
    }
    result
}

fn commit_files_from_diff_entries(entries: &[DiffEntry]) -> Vec<CommitFile> {
    let mut files: Vec<CommitFile> = entries
        .iter()
        .filter(|e| e.status != DiffStatus::Unmerged)
        .map(|e| {
            let (path, old_path) = match e.status {
                DiffStatus::Renamed | DiffStatus::Copied => {
                    (e.new_path.clone().unwrap_or_default(), e.old_path.clone())
                }
                DiffStatus::Deleted => (e.old_path.clone().unwrap_or_default(), None),
                _ => (e.new_path.clone().unwrap_or_default(), None),
            };
            CommitFile {
                path,
                status: e.status,
                old_path,
            }
        })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

fn commit_files_between_trees(
    odb: &crate::odb::Odb,
    old_tree: Option<&ObjectId>,
    new_tree: &ObjectId,
) -> Result<Vec<CommitFile>> {
    let entries = diff_trees_with_renames(odb, old_tree, Some(new_tree), "", 50)?;
    Ok(commit_files_from_diff_entries(&entries))
}

/// Files of a root commit, read from the index its tree was just written from: every path is
/// added, so this skips the whole-tree diff and its per-path entries.
///
/// Returns `None` when the index holds sparse-directory placeholders, whose files only the
/// tree lists.
fn root_commit_files_from_index(index: &Index) -> Option<Vec<CommitFile>> {
    let mut files = Vec::with_capacity(index.entries().len());
    for entry in index.entries() {
        if entry.mode == crate::index::MODE_TREE {
            return None;
        }
        if entry.stage() != 0 || entry.intent_to_add() {
            continue;
        }
        files.push(CommitFile {
            path: crate::repo_relpath::storage_string(&entry.path),
            status: DiffStatus::Added,
            old_path: None,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Some(files)
}

fn commit_files_index_vs_parent_tree(
    odb: &crate::odb::Odb,
    index: &Index,
    parent_tree: Option<&ObjectId>,
) -> Result<Vec<CommitFile>> {
    let mut parent: BTreeMap<String, ObjectId> = BTreeMap::new();
    if let Some(tree) = parent_tree {
        for (path, _mode, oid) in flatten_tree_entries(odb, tree)? {
            parent.insert(path, oid);
        }
    }
    let mut files = Vec::new();
    for entry in index.entries() {
        if entry.stage() != 0 || entry.intent_to_add() {
            continue;
        }
        let path = String::from_utf8_lossy(&entry.path).into_owned();
        match parent.remove(&path) {
            None => files.push(CommitFile {
                path,
                status: DiffStatus::Added,
                old_path: None,
            }),
            Some(old_oid) if old_oid != entry.oid => files.push(CommitFile {
                path,
                status: DiffStatus::Modified,
                old_path: None,
            }),
            _ => {}
        }
    }
    for (path, _) in parent {
        files.push(CommitFile {
            path,
            status: DiffStatus::Deleted,
            old_path: None,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

struct MemOverlayGuard<'a>(&'a Odb);

impl Drop for MemOverlayGuard<'_> {
    fn drop(&mut self) {
        self.0.disable_mem_overlay();
    }
}

fn materialize_missing_index_blobs_for_overlay(repo: &Repository, index: &Index) -> Result<()> {
    let odb = repo.odb();
    let work_tree = repo.work_tree().ok_or(Error::RequiresWorkTree)?;
    let rules = WorktreeRules::from_repository(repo, index)?;
    for entry in index
        .entries
        .iter()
        .filter(|e| e.stage() == 0 && !e.intent_to_add())
    {
        if odb.contains(&entry.oid)? {
            continue;
        }
        let rel = String::from_utf8_lossy(&entry.path);
        let abs = crate::index::worktree_path_from_index_rel(work_tree, &entry.path);
        let meta = fs::symlink_metadata(&abs).map_err(Error::Io)?;
        let file_attrs = rules.file_attrs(&rel, false);
        let stored = materialize_worktree_blob(
            odb,
            &abs,
            &meta,
            rules.conversion(),
            &file_attrs,
            &rel,
            Some(entry),
            Some(rules.filter_process()),
        )?;
        if stored != entry.oid {
            return Err(Error::Message(format!(
                "internal error: staged oid mismatch for {rel}"
            )));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn finish_commit_dry_run(
    repo: &Repository,
    index: &mut Index,
    req: &CommitRequest,
    short_name: Option<String>,
    parent: Option<ObjectId>,
    parent_tree: Option<ObjectId>,
    merge_heads: Vec<ObjectId>,
    concluding_merge: bool,
    amend_author: Option<String>,
    amend_author_raw: Vec<u8>,
    amend_parents: Vec<ObjectId>,
    amend_default_message: Option<String>,
    req_amend: bool,
    reuse_author: Option<String>,
    reuse_author_raw: Vec<u8>,
    reuse_message: Option<String>,
) -> Result<CommitOutcome> {
    let odb = repo.odb();
    odb.enable_mem_overlay();
    let _overlay = MemOverlayGuard(odb);
    materialize_missing_index_blobs_for_overlay(repo, index)?;
    let files = commit_files_index_vs_parent_tree(odb, index, parent_tree.as_ref())?;

    if !req.allow_empty && !req_amend {
        if parent_tree.is_none() && index.entries.is_empty() {
            return Err(empty_commit_error(repo, index, req));
        }
        if !concluding_merge && files.is_empty() {
            return Err(empty_commit_error(repo, index, req));
        }
    }

    let mut message = if !req.message.trim().is_empty() {
        normalize_commit_message(&req.message)?
    } else if let Some(reuse_msg) = reuse_message {
        reuse_msg
    } else if req_amend {
        amend_default_message.unwrap_or_default()
    } else if let Some(pending) = pending_commit_message(repo)? {
        normalize_commit_message(&pending)?
    } else {
        return Err(Error::EmptyCommitMessage);
    };
    if message.trim().is_empty() {
        return Err(Error::EmptyCommitMessage);
    }
    if !message.ends_with('\n') {
        message.push('\n');
    }
    let config = repo.config()?;
    let now = repo.identity_now();
    let committer = commit_identity_line(repo, req.committer.as_ref(), IdentRole::Committer, now)?;
    let _message_with_trailers = enrich_commit_message(
        message,
        &req.trailers,
        req.signoff,
        &committer,
        config.as_ref(),
        repo.git_dir(),
    )?;

    let (author, _author_raw) = resolve_author_for_commit(
        repo,
        req,
        &committer,
        amend_author,
        amend_author_raw,
        reuse_author,
        reuse_author_raw,
        req_amend,
    )?;
    let mut commit_parents = if req_amend {
        amend_parents
    } else {
        parent.into_iter().collect()
    };
    if !req_amend {
        for merge_head in merge_heads {
            if !commit_parents.iter().any(|p| p == &merge_head) {
                commit_parents.push(merge_head);
            }
        }
    }
    let author_info = author_info_from_header_line(&author);
    let changes = files.len();

    Ok(CommitOutcome {
        oid: zero_oid(),
        branch: short_name,
        parent,
        parents: commit_parents,
        author: author_info,
        hooks_run: Vec::new(),
        changes,
        change_id: None,
        files,
    })
}

#[allow(clippy::too_many_arguments)]
fn resolve_author_for_commit(
    repo: &Repository,
    req: &CommitRequest,
    committer: &str,
    amend_author: Option<String>,
    amend_author_raw: Vec<u8>,
    reuse_author: Option<String>,
    reuse_author_raw: Vec<u8>,
    req_amend: bool,
) -> Result<(String, Vec<u8>)> {
    let now = repo.identity_now();
    if req.reset_author {
        return Ok((
            author_line_from_committer(committer, req.author_date.as_deref(), now),
            Vec::new(),
        ));
    }
    if let Some(author) = req.author.as_ref() {
        author.validate()?;
        let user_line = author.header_line();
        if req.author_date.is_none() {
            if let Some(picked) = read_cherry_pick_head(repo.git_dir())? {
                let obj = repo.odb().read(&picked)?;
                if obj.kind == ObjectKind::Commit {
                    let picked_commit = parse_commit(&obj.data)?;
                    if let Ok((picked_line, _)) = prepare_commit_author_from_parent(
                        &picked_commit.author,
                        &picked_commit.author_raw,
                        None,
                        now,
                    ) {
                        if let Some(date_suffix) = picked_line.split('>').nth(1).map(str::trim) {
                            if !date_suffix.is_empty() {
                                return Ok((
                                    replace_identity_date_suffix(&user_line, date_suffix),
                                    Vec::new(),
                                ));
                            }
                        }
                    }
                }
            }
        }
        let author = prepare_commit_identity_line(&user_line, req.author_date.as_deref(), now)?;
        return Ok((author, Vec::new()));
    }
    if let Some(a) = reuse_author {
        return Ok(prepare_commit_author_from_parent(
            &a,
            &reuse_author_raw,
            req.author_date.as_deref(),
            now,
        )?);
    }
    if !req_amend && !req.reset_author && req.author.is_none() {
        if let Some(picked) = read_cherry_pick_head(repo.git_dir())? {
            let picked_commit = repo.odb().read_commit(&picked)?;
            return Ok(prepare_commit_author_from_parent(
                &picked_commit.author,
                &picked_commit.author_raw,
                req.author_date.as_deref(),
                now,
            )?);
        }
    }
    if req_amend {
        let author = amend_author.unwrap_or_default();
        return Ok(prepare_commit_author_from_parent(
            &author,
            &amend_author_raw,
            req.author_date.as_deref(),
            now,
        )?);
    }
    let author = repo.identity(IdentRole::Author, now)?.header_line();
    let author = prepare_commit_identity_line(&author, req.author_date.as_deref(), now)?;
    Ok((author, Vec::new()))
}

/// The committer (or author) header line for a new commit: `identity` when given, otherwise
/// the repository's identity for `role`.
fn commit_identity_line(
    repo: &Repository,
    identity: Option<&Signature>,
    role: IdentRole,
    now: time::OffsetDateTime,
) -> Result<String> {
    match identity {
        Some(signature) => {
            signature.validate()?;
            Ok(signature.header_line())
        }
        None => Ok(repo.identity(role, now)?.header_line()),
    }
}

/// Removes a partial-commit hook index file on drop (Git `next-index-<pid>`).
struct TempHookIndexFile(PathBuf);

impl Drop for TempHookIndexFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Write a merge-continue (or other) commit from a caller-owned index.
///
/// The index must have no unmerged stages; it is written back with the new cache-tree.
pub(crate) fn create_commit_with_loaded_index(
    repo: &Repository,
    index: &mut Index,
    req: &CommitRequest,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<CommitOutcome> {
    if index.has_unmerged_entries() {
        return Err(Error::IndexUnmerged);
    }
    let head = resolve_head_for_repository(repo)?;
    create_commit_with_index(repo, head, index, req, progress, cancel, None)
}

/// Record a commit of `index` on top of `head`.
///
/// `head` must be read before the content `index` was built from: the branch update then
/// compares against that same tip, so a commit landed by another process in between makes this
/// one fail instead of silently reverting it.
///
/// Cancellation is checked before anything is written and again after the pre-commit and
/// commit-msg hooks, before the commit object; once the object is written the ref update always
/// follows.
#[allow(clippy::too_many_arguments)]
fn create_commit_with_index(
    repo: &Repository,
    head: HeadState,
    index: &mut Index,
    req: &CommitRequest,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
    persist_index: Option<LockedIndex>,
) -> Result<CommitOutcome> {
    poll_cancel(progress, cancel)?;
    let (refname, short_name, mut parent) = match head {
        HeadState::Branch {
            refname,
            short_name,
            oid,
        } => (refname, Some(short_name), oid),
        HeadState::Detached { oid } => ("HEAD".to_owned(), None, Some(oid)),
        HeadState::Invalid => {
            return Err(Error::InvalidHead);
        }
    };

    let mut amend_parents = Vec::new();
    let mut amend_author = None;
    let mut amend_author_raw = Vec::new();
    let mut amend_extra_headers = Vec::new();
    let mut amend_default_message = None;
    let mut replaced_tip = None;
    let mut reuse_author = None;
    let mut reuse_author_raw = Vec::new();
    let mut reuse_message = None;
    if let Some(reuse_oid) = req.reuse_message_from {
        let source = repo.odb().read_commit(&reuse_oid)?;
        reuse_message = Some(source.message);
        reuse_author = Some(source.author);
        reuse_author_raw = source.author_raw;
    }
    if req.amend {
        if replay_blocks_partial_commit(repo.git_dir())? {
            return Err(Error::PartialCommitDuringReplay);
        }
        let Some(tip) = parent.take() else {
            return Err(Error::AmendUnborn);
        };
        let old = repo.odb().read_commit(&tip)?;
        amend_parents = old.parents.clone();
        parent = old.parents.first().copied();
        amend_author = Some(old.author);
        amend_author_raw = old.author_raw;
        amend_extra_headers = extra_headers_without_commit_signatures(&old.extra_headers);
        amend_default_message = Some(old.message);
        replaced_tip = Some(tip);
    }

    if index.has_unmerged_entries() {
        return Err(Error::IndexUnmerged);
    }
    let merge_heads = read_merge_heads(repo.git_dir())?;
    let concluding_merge = !merge_heads.is_empty();
    let index_path = repo.git_dir().join("index");
    let parent_tree = parent
        .as_ref()
        .map(|parent_oid| repo.odb().commit_tree(parent_oid))
        .transpose()?;

    if !req.allow_empty && !req.amend && !concluding_merge {
        let skip_tree_match = req.assume_worktree_synced && req.skip_pre_commit_tree_match;
        if !skip_tree_match {
            if let Some(ref old) = parent_tree {
                if index_staged_tree_matches(repo.odb(), index, old) {
                    return Err(empty_commit_error(repo, index, req));
                }
            } else if index.entries.is_empty() {
                return Err(empty_commit_error(repo, index, req));
            }
        }
    }

    if req.dry_run {
        return finish_commit_dry_run(
            repo,
            index,
            req,
            short_name,
            parent,
            parent_tree,
            merge_heads,
            concluding_merge,
            amend_author,
            amend_author_raw,
            amend_parents,
            amend_default_message,
            req.amend,
            reuse_author,
            reuse_author_raw,
            reuse_message,
        );
    }

    let persist_through_lock = persist_index.is_some();
    let _temp_hook_index_guard = if persist_through_lock {
        let path = repo.git_dir().join(format!("next-index-{}", process::id()));
        repo.write_index_at(&path, index)?;
        Some(TempHookIndexFile(path))
    } else {
        repo.write_index(index)?;
        None
    };

    let temp_hook_index_path = _temp_hook_index_guard.as_ref().map(|g| g.0.clone());

    let hook_index_path: PathBuf = temp_hook_index_path
        .clone()
        .unwrap_or_else(|| index_path.clone());

    let commit_env = CommitHookEnv {
        index_file: Some(hook_index_path.as_path()),
        git_editor: Some(":"),
        git_prefix: None,
        extra_env: &[],
    };
    let mut hooks_run = Vec::new();
    if req.hooks != HookPolicy::SkipVerify
        && run_commit_hook_recorded(repo, HookName::PreCommit, &[], None, &commit_env)?
    {
        hooks_run.push(HookName::PreCommit);
    }

    let message_source = if req.message_from_merge {
        MessageSource::Merge
    } else if !req.message.trim().is_empty() {
        MessageSource::Message
    } else if let Some(oid) = req.reuse_message_from.or(replaced_tip) {
        MessageSource::Commit(oid)
    } else if repo.git_dir().join("SQUASH_MSG").is_file() {
        MessageSource::Squash
    } else {
        MessageSource::Merge
    };
    let mut message = if !req.message.trim().is_empty() {
        normalize_commit_message(&req.message)?
    } else if let Some(reuse_msg) = reuse_message {
        reuse_msg
    } else if req.amend {
        amend_default_message.unwrap_or_default()
    } else if let Some(pending) = pending_commit_message(repo)? {
        normalize_commit_message(&pending)?
    } else {
        return Err(Error::EmptyCommitMessage);
    };
    if message.trim().is_empty() {
        return Err(Error::EmptyCommitMessage);
    }
    if !message.ends_with('\n') {
        message.push('\n');
    }
    let config = repo.config()?;
    let now = repo.identity_now();
    let committer = commit_identity_line(repo, req.committer.as_ref(), IdentRole::Committer, now)?;
    message = enrich_commit_message(
        message,
        &req.trailers,
        req.signoff,
        &committer,
        config.as_ref(),
        repo.git_dir(),
    )?;
    let editmsg_path = repo.git_dir().join("COMMIT_EDITMSG");
    fs::write(&editmsg_path, &message).map_err(Error::Io)?;
    // COMMIT_EDITMSG is shared by every committer in the repository, so it is only read back
    // when a hook may have edited it; otherwise the in-memory message wins.
    if let Some(edited) = run_commit_message_hooks(
        repo,
        &editmsg_path,
        message_source,
        req.hooks,
        &commit_env,
        &mut hooks_run,
    )? {
        message = edited;
    }

    let (author, author_raw) = resolve_author_for_commit(
        repo,
        req,
        &committer,
        amend_author,
        amend_author_raw,
        reuse_author,
        reuse_author_raw,
        req.amend,
    )?;

    let tree = write_tree_update_index(repo.odb(), index, "", WriteTreeFlags::silent())?;
    let root_files = parent_tree
        .is_none()
        .then(|| root_commit_files_from_index(index))
        .flatten();
    let commit_files = match root_files {
        Some(files) => files,
        None => commit_files_between_trees(repo.odb(), parent_tree.as_ref(), &tree)?,
    };

    if !req.allow_empty && !req.amend {
        if parent_tree.is_none() && is_empty_tree_oid(repo.odb(), &tree) {
            return Err(empty_commit_error(repo, index, req));
        }
        if !concluding_merge {
            if let Some(ref old) = parent_tree {
                if old == &tree {
                    return Err(empty_commit_error(repo, index, req));
                }
            }
        }
    }

    let mut commit_parents = if req.amend {
        amend_parents
    } else {
        parent.into_iter().collect()
    };
    if !req.amend {
        for merge_head in merge_heads {
            if !commit_parents.iter().any(|p| p == &merge_head) {
                commit_parents.push(merge_head);
            }
        }
    }
    let author_info = author_info_from_header_line(&author);
    let mut commit_data = CommitData {
        tree,
        parents: commit_parents.clone(),
        author,
        committer: committer.clone(),
        author_raw,
        committer_raw: Vec::new(),
        encoding: None,
        message,
        raw_message: None,
        extra_headers: amend_extra_headers,
    };
    let policy = ChangeIdPolicy::from_config(config.as_ref());
    if policy.enabled() {
        if req.amend {
            if change_id(&commit_data).is_none() {
                if let Some(tip) = replaced_tip {
                    set_change_id(&mut commit_data, &ChangeId::derived_from_commit_id(&tip));
                }
            }
        } else {
            stamp_change_id_on_new_commit(repo, &policy, &mut commit_data)?;
        }
    }
    let bytes = prepare_commit_bytes(repo, config.as_ref(), &commit_data, req.sign_override)?;
    fsck_commit_idents(&bytes, repo.odb().hash_algo())
        .map_err(|detail| Error::CorruptObject(format!("commit would fail fsck: {detail}")))?;
    poll_cancel(progress, cancel)?;
    let oid = repo.odb().write(ObjectKind::Commit, &bytes)?;

    let concluding_replay = read_cherry_pick_head(repo.git_dir())?.is_some()
        || read_revert_head(repo.git_dir())?.is_some();
    let previous_tip = replaced_tip.or(parent);
    let reflog_old = previous_tip.unwrap_or_else(zero_oid);
    let subject = commit_subject(&commit_data.message);
    let reflog_msg = if req.amend {
        format!("commit (amend): {subject}")
    } else if concluding_replay && read_cherry_pick_head(repo.git_dir())?.is_some() {
        format!("commit (cherry-pick): {subject}")
    } else if parent.is_some() {
        format!("commit: {subject}")
    } else {
        format!("commit (initial): {subject}")
    };

    let expected_old = previous_tip.or_else(|| {
        matches!(repo.references().resolve_if_exists(&refname), Ok(Some(_))).then_some(reflog_old)
    });

    if !persist_through_lock {
        if !req.assume_worktree_synced {
            if let Some(work_tree) = repo.work_tree() {
                let rules = WorktreeRules::from_repository(repo, index).ok();
                let rules_arc = rules.map(|r| Arc::new(Mutex::new(r)));
                let _ = refresh_index_stat_content_verified_with_rules(
                    repo.odb(),
                    repo.git_dir(),
                    index,
                    work_tree,
                    None,
                    Some(config.as_ref()),
                    None,
                    rules_arc.as_ref(),
                )?;
            }
        }
        repo.write_index(index)?;
    }

    let changes = if parent_tree.as_ref() == Some(&tree) {
        0
    } else if !commit_files.is_empty() {
        commit_files.len()
    } else if req.assume_worktree_synced && req.skip_pre_commit_tree_match {
        // Large-index commits skip the tree diff for speed (issue #1064); smaller repos keep
        // accurate path counts for pre-staged and JSON output.
        const LARGE_INDEX_CHANGE_COUNT_FAST_PATH: usize = 4096;
        let large_index = index.entries.len() >= LARGE_INDEX_CHANGE_COUNT_FAST_PATH;
        if large_index {
            if let Some(n) = req.staged_change_count.filter(|&n| n > 0) {
                n
            } else {
                diff_tree_path_change_count(repo.odb(), parent_tree.as_ref(), Some(&tree), "")?
            }
        } else {
            diff_tree_path_change_count(repo.odb(), parent_tree.as_ref(), Some(&tree), "")?
        }
    } else {
        diff_tree_path_change_count(repo.odb(), parent_tree.as_ref(), Some(&tree), "")?
    };

    if short_name.is_none() {
        update_detached_head_for_commit_on_store(
            repo.refs(),
            expected_old,
            oid,
            &committer,
            &reflog_msg,
            config.as_ref(),
        )?;
    } else {
        update_branch_for_commit_on_store(
            repo.refs(),
            &BranchCommitRefUpdate {
                branch_ref: &refname,
                expected_old,
                new_oid: oid,
                identity: &committer,
                reflog_message: &reflog_msg,
            },
            config.as_ref(),
        )?;
    }

    if concluding_merge {
        let _ = finish_merge_state(repo)?;
    }
    if concluding_replay {
        finish_replay_commit_state(repo, oid)?;
    }
    clear_squash_commit_state(repo.git_dir())?;

    if let Some(LockedIndex {
        index: mut separate,
        mut lock,
    }) = persist_index
    {
        let persist = separate.as_mut().unwrap_or(index);
        if !req.assume_worktree_synced {
            if let Some(work_tree) = repo.work_tree() {
                let rules = WorktreeRules::from_repository(repo, persist).ok();
                let rules_arc = rules.map(|r| Arc::new(Mutex::new(r)));
                let _ = refresh_index_stat_content_verified_with_rules(
                    repo.odb(),
                    repo.git_dir(),
                    persist,
                    work_tree,
                    None,
                    Some(config.as_ref()),
                    None,
                    rules_arc.as_ref(),
                )?;
            }
        }
        repo.write_index_with_lock(&mut lock, persist, false)?;
    }

    let post_commit_env = CommitHookEnv {
        index_file: Some(
            temp_hook_index_path
                .as_deref()
                .unwrap_or(index_path.as_path()),
        ),
        git_editor: Some(":"),
        git_prefix: None,
        extra_env: &[],
    };
    if run_advisory_hook(
        repo,
        HookName::PostCommit,
        &[],
        None,
        Some(&post_commit_env),
    ) {
        hooks_run.push(HookName::PostCommit);
    }
    if let Some(old_tip) = replaced_tip.filter(|_| req.post_rewrite.enabled()) {
        if run_post_rewrite(repo, RewriteKind::Amend, &[(old_tip, oid)]) {
            hooks_run.push(HookName::PostRewrite);
        }
    }

    let stored_change_id = change_id(&commit_data);
    Ok(CommitOutcome {
        oid,
        branch: short_name,
        parent,
        parents: commit_data.parents,
        author: author_info,
        hooks_run,
        changes,
        change_id: stored_change_id,
        files: commit_files,
    })
}

/// When [`ChangeIdPolicy`] is enabled, assign a fresh id to a newly written commit.
///
/// Does not run on amends or history rewrites; callers pass an empty `extra_headers`
/// so source change ids never carry over (pick, revert, clean merge).
///
/// # Errors
///
/// Propagates change-id generation failures as [`Error::Message`].
pub(crate) fn stamp_change_id_on_new_commit(
    repo: &Repository,
    policy: &ChangeIdPolicy,
    data: &mut CommitData,
) -> Result<()> {
    if let Some(id) = policy
        .id_for_new_commit(
            data,
            repo.odb().hash_algo(),
            repo.change_id_generator().as_ref(),
        )
        .map_err(|e| Error::Message(format!("change id: {e}")))?
    {
        set_change_id(data, &id);
    }
    Ok(())
}

/// Serialize `data` and sign the bytes when [`should_sign_commit`] says so.
///
/// Does not assign change ids. Callers that need a `change-id` header (for example
/// [`create_commit`]) set it with [`crate::change_id::set_change_id`] first. Stash,
/// notes, and other auxiliary commits use this path without adding ids.
///
/// # Errors
///
/// Propagates signing and config errors from [`GpgConfig::from_config`] and
/// [`sign_serialized_commit`].
pub fn prepare_commit_bytes(
    repo: &Repository,
    config: &ConfigSet,
    data: &CommitData,
    sign_override: Option<bool>,
) -> Result<Vec<u8>> {
    let unsigned = serialize_commit(data);
    if !should_sign_commit(config, sign_override)? {
        return Ok(unsigned);
    }
    let gpg = GpgConfig::from_config(config)?.with_command_runner(repo.command_runner());
    sign_serialized_commit(
        &gpg,
        &unsigned,
        &data.committer,
        None,
        repo.odb().hash_algo(),
    )
}

/// Write a commit object to the ODB, honoring `commit.gpgsign` and `sign_override`.
///
/// Does not assign change ids; see [`prepare_commit_bytes`].
///
/// # Errors
///
/// Same as [`prepare_commit_bytes`], plus object-database write failures.
pub fn write_commit_object(
    repo: &Repository,
    data: &CommitData,
    sign_override: Option<bool>,
) -> Result<ObjectId> {
    let config = repo.config()?;
    let bytes = prepare_commit_bytes(repo, config.as_ref(), data, sign_override)?;
    repo.odb().write(ObjectKind::Commit, &bytes)
}

fn empty_commit_error(repo: &Repository, index: &mut Index, req: &CommitRequest) -> Error {
    if req.index_only {
        let mut worktree_index = match repo.load_index() {
            Ok(real) => real,
            Err(_) => index.clone(),
        };
        if let Ok((unstaged, untracked)) =
            crate::porcelain::status::count_unstaged_and_untracked_paths(repo, &mut worktree_index)
        {
            if unstaged + untracked > 0 {
                return Error::NothingStagedToCommit {
                    unstaged,
                    untracked,
                };
            }
        }
    }
    Error::NothingToCommit
}

/// True when stage-0 index entries describe `tree_oid`, using a valid cache-tree when present.
fn index_staged_tree_matches(
    odb: &crate::odb::Odb,
    index: &crate::index::Index,
    tree_oid: &ObjectId,
) -> bool {
    if index.cache_tree_root == Some(*tree_oid)
        && cache_tree_extension_valid(index.cache_tree.as_ref())
    {
        return true;
    }
    crate::diff::diff_index_to_tree(odb, index, Some(tree_oid), false)
        .map(|d| d.is_empty())
        .unwrap_or(false)
}

fn commit_subject(message: &str) -> &str {
    message.split('\n').next().unwrap_or("").trim_end()
}

/// Path passed as `argv[1]` to the `commit-msg` hook (Git: `.git/COMMIT_EDITMSG` when possible).
fn commit_editmsg_hook_argument(repo: &Repository, editmsg_path: &Path) -> String {
    if let Some(wt) = repo.work_tree() {
        if let Ok(rel) = editmsg_path.strip_prefix(wt) {
            let s = rel.to_string_lossy();
            if !s.is_empty() {
                return s.replace('\\', "/");
            }
        }
    }
    editmsg_path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use crate::environment::RepositoryOptions;

    use super::*;
    use crate::index::{entry_from_stat, Index, MODE_REGULAR};
    use crate::objects::ObjectKind;
    use crate::pathspec::ResolvedPathspecs;
    use crate::porcelain::add::{stage, StageOptions};
    use crate::progress::NullProgress;
    use crate::reflog::read_reflog;
    use crate::refs::{resolve_ref, set_test_inject_reflog_fail};
    use crate::repo::set_test_inject_index_write_fail;
    use crate::write_tree::{cache_tree_fully_valid, verify_cache_tree};
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use std::str::FromStr;
    use tempfile::TempDir;

    fn init_repo(root: &Path) -> Repository {
        let git = root.join(".git");
        fs::create_dir_all(git.join("objects")).unwrap();
        fs::create_dir_all(git.join("refs/heads")).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(
            git.join("config"),
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tlogAllRefUpdates = true\n",
        )
        .unwrap();
        Repository::open(&git, &RepositoryOptions::empty().with_work_tree(root)).unwrap()
    }

    fn ident() -> String {
        "Test User <t@example.com> 1 +0000".to_owned()
    }

    fn stage_file_in_index(repo: &Repository, index: &mut Index, rel: &str, contents: &[u8]) {
        let wt = repo.work_tree().unwrap();
        let abs = wt.join(rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&abs, contents).unwrap();
        let oid = repo.odb().write(ObjectKind::Blob, contents).unwrap();
        let entry = entry_from_stat(&abs, rel.as_bytes(), oid, MODE_REGULAR).unwrap();
        index.add_or_replace(entry);
    }

    fn write_index(repo: &Repository, index: &mut Index) {
        index.sort();
        repo.write_index(index).unwrap();
    }

    #[test]
    fn root_commit_files_from_index_match_the_tree_diff() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo(dir.path());
        let mut index = Index::new();
        for rel in ["a.txt", "dir/sub/b.txt", "dir-x.txt", "z/last.txt"] {
            stage_file_in_index(&repo, &mut index, rel, rel.as_bytes());
        }
        let template = index.get(b"a.txt", 0).unwrap().clone();
        let with_path = |path: &[u8], mode: u32, oid: ObjectId| {
            let mut entry = template.clone();
            entry.path = path.to_vec();
            entry.flags = path.len() as u16;
            entry.mode = mode;
            entry.oid = oid;
            entry
        };
        let blob = repo.odb().write(ObjectKind::Blob, b"raw\n").unwrap();
        index.add_or_replace(with_path(b"bin\xff.txt", MODE_REGULAR, blob));
        let gitlink = ObjectId::from_hex("1111111111111111111111111111111111111111").unwrap();
        index.add_or_replace(with_path(b"module", crate::index::MODE_GITLINK, gitlink));
        let mut ita = with_path(b"ita.txt", MODE_REGULAR, blob);
        ita.set_intent_to_add(true);
        index.add_or_replace(ita);
        index.sort();

        let tree =
            write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent()).unwrap();
        let from_tree = commit_files_between_trees(repo.odb(), None, &tree).unwrap();
        let from_index = root_commit_files_from_index(&index).expect("no sparse placeholders");
        assert_eq!(from_index.len(), 6);
        assert_eq!(from_index, from_tree);

        let mut placeholder = with_path(b"sparse/", crate::index::MODE_TREE, tree);
        placeholder.set_skip_worktree(true);
        index.add_or_replace(placeholder);
        assert!(root_commit_files_from_index(&index).is_none());
    }

    fn commit_req(message: &str) -> CommitRequest {
        let ident = ident();
        CommitRequest {
            message: message.to_owned(),
            author: Some(ident.parse().expect("signature")),
            committer: Some(ident.parse().expect("signature")),
            ..Default::default()
        }
    }

    fn repo_snapshot(repo: &Repository) -> (Vec<u8>, String, usize, bool, usize) {
        let index_path = repo.git_dir().join("index");
        let index_bytes = fs::read(&index_path).unwrap_or_default();
        let head = fs::read_to_string(repo.git_dir().join("HEAD")).unwrap_or_default();
        let reflog_len = read_reflog(repo.git_dir(), "refs/heads/main")
            .map(|e| e.len())
            .unwrap_or(0);
        let editmsg = repo.git_dir().join("COMMIT_EDITMSG").exists();
        let mut objects = 0usize;
        if let Ok(entries) = fs::read_dir(repo.git_dir().join("objects")) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() && path.file_name().is_some_and(|n| n.len() == 2) {
                    objects += fs::read_dir(path).map(|d| d.count()).unwrap_or(0);
                } else if path.is_file() {
                    objects += 1;
                }
            }
        }
        (index_bytes, head, reflog_len, editmsg, objects)
    }

    fn dry_run_req(message: &str) -> CommitRequest {
        let mut req = commit_req(message);
        req.dry_run = true;
        req
    }

    #[cfg(unix)]
    #[test]
    fn parallel_symlink_worktree_preview_is_clean_or_matches_commit() {
        use crate::parallel::ThreadPolicy;
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::create_dir(root.join("files")).unwrap();
        for n in 0..1024 {
            fs::write(root.join(format!("files/{n}")), b"data\n").unwrap();
        }
        for (path, target) in [
            ("linkdir", "files"),
            ("linkfile", "files/0"),
            ("linkmissing", "missing"),
        ] {
            symlink(target, root.join(path)).unwrap();
        }
        commit_worktree(
            &repo,
            &commit_req("initial"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let mut index = repo.load_index().unwrap();
        for entry in &mut index.entries {
            entry.mtime_sec = 0;
            entry.mtime_nsec = 0;
            entry.size = 0;
        }
        repo.write_index(&mut index).unwrap();
        let before = repo_snapshot(&repo);
        let before_tip = resolve_ref(repo.git_dir(), "refs/heads/main").unwrap();
        for threads in [1, 4] {
            let repo = Repository::open(
                repo.git_dir(),
                &RepositoryOptions::empty().with_work_tree(root),
            )
            .unwrap()
            .with_thread_policy(ThreadPolicy::with_threads_override(Some(threads)));
            let err = commit_worktree(
                &repo,
                &dry_run_req("clean"),
                &mut NullProgress,
                &crate::progress::NeverCancel,
            )
            .unwrap_err();
            assert!(matches!(err, Error::NothingToCommit), "{err:?}");
            assert_eq!(repo_snapshot(&repo), before);
            assert_eq!(
                resolve_ref(repo.git_dir(), "refs/heads/main").unwrap(),
                before_tip
            );
        }
        for (path, target) in [
            ("linkdir", "."),
            ("linkfile", "files/1"),
            ("linkmissing", "absent"),
        ] {
            fs::remove_file(root.join(path)).unwrap();
            symlink(target, root.join(path)).unwrap();
        }
        let mut preview_files = None;
        for threads in [1, 4] {
            let repo = Repository::open(
                repo.git_dir(),
                &RepositoryOptions::empty().with_work_tree(root),
            )
            .unwrap()
            .with_thread_policy(ThreadPolicy::with_threads_override(Some(threads)));
            let preview = commit_worktree(
                &repo,
                &dry_run_req("retarget"),
                &mut NullProgress,
                &crate::progress::NeverCancel,
            )
            .unwrap();
            assert_eq!(preview.changes, 3);
            if let Some(expected) = &preview_files {
                assert_eq!(&preview.files, expected);
            }
            preview_files = Some(preview.files);
            assert_eq!(repo_snapshot(&repo), before);
            assert_eq!(
                resolve_ref(repo.git_dir(), "refs/heads/main").unwrap(),
                before_tip
            );
        }
        let committed = commit_worktree(
            &repo,
            &commit_req("retarget"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        assert_eq!(Some(committed.files), preview_files);
    }

    #[test]
    fn create_commit_initial() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("README"), b"hello\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        let outcome = create_commit(
            &repo,
            &commit_req("initial"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        assert!(outcome.parent.is_none());
        assert_eq!(outcome.branch.as_deref(), Some("main"));
        assert!(!outcome.is_detached());
        assert_eq!(outcome.changes, 1);
        assert_eq!(
            resolve_ref(&repo.git_dir(), "refs/heads/main").unwrap(),
            outcome.oid
        );
    }

    #[test]
    fn create_commit_rejects_empty_initial_index() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        let err = create_commit(
            &repo,
            &commit_req("empty"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap_err();
        assert!(matches!(err, Error::NothingToCommit));
        assert!(resolve_ref(&repo.git_dir(), "refs/heads/main").is_err());
    }

    #[test]
    fn create_commit_normal() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        create_commit(
            &repo,
            &commit_req("first"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        fs::write(root.join("a.txt"), b"2\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let outcome = create_commit(
            &repo,
            &commit_req("second"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        assert_eq!(outcome.changes, 1);
        assert!(outcome.parent.is_some());
    }

    #[test]
    fn create_commit_nothing_to_commit() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        create_commit(
            &repo,
            &commit_req("first"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        let err = create_commit(
            &repo,
            &commit_req("noop"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap_err();
        assert!(matches!(err, Error::NothingToCommit));
    }

    #[test]
    fn missing_identity_does_not_write_dangling_tree() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let objects_before = fs::read_dir(repo.git_dir().join("objects"))
            .unwrap()
            .filter_map(|e| e.ok())
            .count();
        let err = commit_worktree(
            &repo,
            &CommitRequest {
                message: "first".into(),
                ..CommitRequest::default()
            },
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            Error::Identity(
                crate::config::ident_resolve::IdentityError::MissingConfiguredEmail { .. }
            )
        ));
        let objects_after = fs::read_dir(repo.git_dir().join("objects"))
            .unwrap()
            .filter_map(|e| e.ok())
            .count();
        assert_eq!(
            objects_before, objects_after,
            "commit must not store objects"
        );
        let fsck = Command::new("git")
            .args(["--git-dir", &repo.git_dir().to_string_lossy(), "fsck"])
            .output()
            .unwrap();
        assert!(
            fsck.status.success(),
            "{}",
            String::from_utf8_lossy(&fsck.stderr)
        );
        let stderr = String::from_utf8_lossy(&fsck.stderr);
        assert!(
            !stderr.contains("dangling tree"),
            "unexpected dangling tree: {stderr}"
        );
    }

    #[test]
    fn commit_worktree_nothing_to_commit_persists_staged_deletion() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        commit_worktree(
            &repo,
            &commit_req("first"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        let mut index = repo.load_index().unwrap();
        stage_file_in_index(&repo, &mut index, "n.txt", b"staged\n");
        write_index(&repo, &mut index);
        fs::remove_file(root.join("n.txt")).unwrap();

        let err = commit_worktree(
            &repo,
            &dry_run_req("noop"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap_err();
        assert!(matches!(err, Error::NothingToCommit));
        assert!(repo.load_index().unwrap().get(b"n.txt", 0).is_some());

        let err = commit_worktree(
            &repo,
            &commit_req("noop"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap_err();
        assert!(matches!(err, Error::NothingToCommit));
        let index = repo.load_index().unwrap();
        assert!(index.get(b"n.txt", 0).is_none());
        assert!(index.get(b"a.txt", 0).is_some());
    }

    #[test]
    fn create_commit_detached_head_advances_head() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let first = create_commit(
            &repo,
            &commit_req("first"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        fs::write(repo.git_dir().join("HEAD"), first.oid.to_hex()).unwrap();

        fs::write(root.join("b.txt"), b"2\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let second = create_commit(
            &repo,
            &commit_req("second"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        assert_eq!(second.branch, None);
        assert!(second.is_detached());
        assert_eq!(second.parent, Some(first.oid));
        let head = fs::read_to_string(repo.git_dir().join("HEAD")).unwrap();
        assert_eq!(head.trim(), second.oid.to_hex());
        let main_tip = crate::refs::resolve_ref(repo.git_dir(), "refs/heads/main").unwrap();
        assert_eq!(main_tip, first.oid, "branch tip must stay unchanged");
    }

    #[test]
    fn create_commit_updates_reflogs() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let outcome = create_commit(
            &repo,
            &commit_req("subject line"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        let branch_log = read_reflog(&repo.git_dir(), "refs/heads/main").unwrap();
        assert_eq!(branch_log.len(), 1);
        assert_eq!(branch_log[0].new_oid, outcome.oid);
        assert_eq!(branch_log[0].message, "commit (initial): subject line");

        let head_log = read_reflog(&repo.git_dir(), "HEAD").unwrap();
        assert_eq!(head_log.len(), 1);
        assert_eq!(head_log[0].new_oid, outcome.oid);
    }

    #[test]
    fn create_commit_index_write_failure_does_not_advance_branch() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        set_test_inject_index_write_fail(true);
        let err = create_commit(
            &repo,
            &commit_req("fail"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap_err();
        set_test_inject_index_write_fail(false);
        assert!(!matches!(err, Error::NothingToCommit));

        assert!(
            resolve_ref(&repo.git_dir(), "refs/heads/main").is_err(),
            "branch must not advance when index write fails"
        );
    }

    #[test]
    fn create_commit_rolls_back_branch_when_head_reflog_fails() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        set_test_inject_reflog_fail(Some("HEAD"));
        let err = create_commit(
            &repo,
            &commit_req("fail"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap_err();
        set_test_inject_reflog_fail(None);
        assert!(!matches!(err, Error::NothingToCommit));

        assert!(
            resolve_ref(&repo.git_dir(), "refs/heads/main").is_err(),
            "branch must not advance when HEAD reflog fails"
        );
        assert!(read_reflog(&repo.git_dir(), "refs/heads/main")
            .unwrap()
            .is_empty());
        assert!(read_reflog(&repo.git_dir(), "HEAD").unwrap().is_empty());
    }

    #[test]
    fn create_commit_leaves_valid_cache_tree() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let mut index = Index::new();
        stage_file_in_index(&repo, &mut index, "dir/one.txt", b"1\n");
        stage_file_in_index(&repo, &mut index, "dir/two.txt", b"2\n");
        write_index(&repo, &mut index);

        let outcome = create_commit(
            &repo,
            &commit_req("tree"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        let index = repo.load_index().unwrap();
        verify_cache_tree(&index).expect("cache-tree valid");
        assert!(cache_tree_fully_valid(
            &repo.odb(),
            index.cache_tree.as_ref()
        ));
        let commit_obj = repo.odb().read(&outcome.oid).unwrap();
        let tree = parse_commit(&commit_obj.data).unwrap().tree;
        assert_eq!(index.cache_tree_root, Some(tree));
    }

    #[test]
    fn create_commit_amend_same_tree() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        create_commit(
            &repo,
            &commit_req("first"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        fs::write(root.join("a.txt"), b"2\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        create_commit(
            &repo,
            &commit_req("second"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        let mut req = commit_req("");
        req.amend = true;
        let tip = resolve_ref(&repo.git_dir(), "refs/heads/main").unwrap();
        let outcome = create_commit(
            &repo,
            &req,
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        assert_eq!(
            outcome.oid, tip,
            "same tree and message with fixed committer yields the same object id"
        );
        assert!(outcome.parent.is_some());

        let branch_log = read_reflog(&repo.git_dir(), "refs/heads/main").unwrap();
        assert_eq!(branch_log.len(), 3);
        assert_eq!(branch_log[2].message, "commit (amend): second");
    }

    #[test]
    fn create_commit_amend_preserves_message_bytes() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"a\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        create_commit(
            &repo,
            &commit_req("oneline subject"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let baseline = {
            let oid = resolve_ref(&repo.git_dir(), "refs/heads/main").unwrap();
            parse_commit(&repo.odb().read(&oid).unwrap().data)
                .unwrap()
                .message
        };

        for extra in 1..=2 {
            fs::write(root.join("a.txt"), format!("a\n{extra}\n")).unwrap();
            stage(
                &repo,
                &StageOptions::default(),
                &mut NullProgress,
                &crate::progress::NeverCancel,
            )
            .unwrap();
            let mut req = commit_req("");
            req.amend = true;
            create_commit(
                &repo,
                &req,
                &mut NullProgress,
                &crate::progress::NeverCancel,
            )
            .unwrap();
            let oid = resolve_ref(&repo.git_dir(), "refs/heads/main").unwrap();
            let message = parse_commit(&repo.odb().read(&oid).unwrap().data)
                .unwrap()
                .message;
            assert_eq!(message, baseline, "after amend {extra}");
        }
    }

    #[test]
    fn create_commit_amend_unborn() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        fs::write(tmp.path().join("a.txt"), b"1\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let mut req = commit_req("nope");
        req.amend = true;
        let err = create_commit(
            &repo,
            &req,
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap_err();
        assert!(matches!(err, Error::AmendUnborn));
    }

    #[test]
    fn create_commit_allow_empty() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        create_commit(
            &repo,
            &commit_req("first"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        let mut req = commit_req("empty");
        req.allow_empty = true;
        let parent = resolve_ref(&repo.git_dir(), "refs/heads/main").unwrap();
        let outcome = create_commit(
            &repo,
            &req,
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        assert_eq!(outcome.changes, 0);
        assert_eq!(outcome.parent, Some(parent));
        assert_ne!(outcome.oid, parent);
    }

    #[test]
    fn scope_staged_commits_index_only() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        fs::write(root.join("b.txt"), b"1\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        create_commit(
            &repo,
            &commit_req("first"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        fs::write(root.join("a.txt"), b"2\n").unwrap();
        fs::write(root.join("b.txt"), b"2\n").unwrap();
        let mut index = repo.load_index().unwrap();
        stage_file_in_index(&repo, &mut index, "a.txt", b"2\n");
        write_index(&repo, &mut index);

        let outcome = commit_with_scope(
            &repo,
            &commit_req("only a"),
            CommitScope::Staged,
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        assert_eq!(outcome.changes, 1);
        let index = repo.load_index().unwrap();
        let b_oid = index.get(b"b.txt", 0).unwrap().oid;
        let b_blob = repo.odb().read(&b_oid).unwrap();
        assert_eq!(b_blob.data, b"1\n", "unstaged paths stay in the index");
    }

    #[test]
    fn scope_paths_does_not_hash_a_throwaway_validation_index() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let mut bytes = vec![b'x'; 2 * 1024 * 1024];
        fs::write(root.join("large"), &bytes).unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        create_commit(
            &repo,
            &commit_req("initial"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        bytes.push(b'y');
        fs::write(root.join("large"), &bytes).unwrap();
        let passes = repo.odb().hot_path_metrics().staging_passes();
        let result = commit_with_scope(
            &repo,
            &commit_req("selected path"),
            CommitScope::Paths(ResolvedPathspecs {
                specs: vec!["large".into()],
                sources: vec!["large".into()],
                scoped: true,
            }),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        assert_eq!(result.changes, 1);
        // The commit view and repository view each stage once. Validation must not
        // add a third pass, regardless of extra hashing needed for racy stat rows.
        assert_eq!(repo.odb().hot_path_metrics().staging_passes(), passes + 2);
        let index = repo.load_index().unwrap();
        let oid = index.get(b"large", 0).unwrap().oid;
        assert_eq!(repo.odb().read(&oid).unwrap().data, bytes);
    }

    #[test]
    fn scope_paths_leaves_other_staged_entries() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("file"), b"1\n").unwrap();
        fs::write(root.join("baz"), b"1\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        create_commit(
            &repo,
            &commit_req("initial"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        fs::write(root.join("file"), b"2\n").unwrap();
        fs::write(root.join("baz"), b"2\n").unwrap();
        stage(
            &repo,
            &StageOptions {
                pathspecs: vec!["baz".into()],
                ..StageOptions::default()
            },
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        let paths = ResolvedPathspecs {
            specs: vec!["file".into()],
            sources: vec!["file".into()],
            scoped: true,
        };
        commit_with_scope(
            &repo,
            &commit_req("file only"),
            CommitScope::Paths(paths),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        let index = repo.load_index().unwrap();
        let baz_oid = index.get(b"baz", 0).unwrap().oid;
        assert_eq!(repo.odb().read(&baz_oid).unwrap().data, b"2\n");
    }

    #[test]
    fn scope_paths_deleted_and_untracked() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("tracked.txt"), b"1\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        create_commit(
            &repo,
            &commit_req("first"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        fs::write(root.join("new.txt"), b"new\n").unwrap();
        fs::remove_file(root.join("tracked.txt")).unwrap();

        let paths = ResolvedPathspecs {
            specs: vec!["new.txt".into(), "tracked.txt".into()],
            sources: vec!["new.txt".into(), "tracked.txt".into()],
            scoped: true,
        };
        commit_with_scope(
            &repo,
            &commit_req("add and remove"),
            CommitScope::Paths(paths),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        let index = repo.load_index().unwrap();
        assert!(index.get(b"tracked.txt", 0).is_none());
        assert!(index.get(b"new.txt", 0).is_some());
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn scope_paths_nested_dirs_round_trip_with_git() {
        use std::process::Command;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let git = |args: &[&str]| -> String {
            let out = Command::new("git")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .current_dir(root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap()
        };
        git(&["init", "-q", "-b", "main"]);
        for (rel, body) in [
            ("a/x.txt", "x1\n"),
            ("a/y.txt", "y1\n"),
            ("b/c/z.txt", "z1\n"),
            ("b/w.txt", "w1\n"),
            ("d/e.txt", "e1\n"),
            ("top.txt", "t1\n"),
        ] {
            fs::create_dir_all(root.join(rel).parent().unwrap()).unwrap();
            fs::write(root.join(rel), body).unwrap();
        }
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);

        fs::write(root.join("a/x.txt"), b"x2\n").unwrap();
        fs::write(root.join("b/c/z.txt"), b"z2\n").unwrap();
        fs::write(root.join("b/w.txt"), b"w2\n").unwrap();
        fs::write(root.join("top.txt"), b"t2\n").unwrap();
        git(&["add", "b/w.txt"]);

        let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).unwrap();
        let specs = vec!["a/x.txt".to_owned(), "b/c/z.txt".to_owned()];
        commit_with_scope(
            &repo,
            &commit_req("partial"),
            CommitScope::Paths(ResolvedPathspecs {
                specs: specs.clone(),
                sources: specs,
                scoped: true,
            }),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        git(&["fsck", "--strict", "--no-progress"]);
        assert_eq!(
            git(&["diff", "--name-only", "HEAD~1", "HEAD"]),
            "a/x.txt\nb/c/z.txt\n"
        );
        assert_eq!(git(&["show", "HEAD:a/x.txt"]), "x2\n");
        assert_eq!(git(&["show", "HEAD:b/c/z.txt"]), "z2\n");
        assert_eq!(git(&["show", "HEAD:b/w.txt"]), "w1\n");
        assert_eq!(
            git(&["rev-parse", "HEAD:d"]),
            git(&["rev-parse", "HEAD~1:d"])
        );
        assert_eq!(git(&["diff", "--cached", "--name-only"]), "b/w.txt\n");
        assert_eq!(git(&["diff", "--name-only"]), "top.txt\n");
    }

    #[test]
    fn scope_paths_no_match_errors() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        create_commit(
            &repo,
            &commit_req("first"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        let paths = ResolvedPathspecs {
            specs: vec!["missing.txt".into()],
            sources: vec!["missing.txt".into()],
            scoped: true,
        };
        let err = commit_with_scope(
            &repo,
            &commit_req("nope"),
            CommitScope::Paths(paths),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap_err();
        assert!(matches!(err, Error::PathspecNoMatch { .. }));
    }

    #[test]
    fn scope_paths_refused_during_merge() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        create_commit(
            &repo,
            &commit_req("first"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let tip = resolve_ref(&repo.git_dir(), "refs/heads/main").unwrap();
        fs::write(repo.git_dir().join("MERGE_HEAD"), format!("{tip}\n")).unwrap();

        let paths = ResolvedPathspecs {
            specs: vec!["a.txt".into()],
            sources: vec!["a.txt".into()],
            scoped: true,
        };
        let err = commit_with_scope(
            &repo,
            &commit_req("partial"),
            CommitScope::Paths(paths),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap_err();
        assert!(matches!(err, Error::PartialCommitDuringMerge));
    }

    fn system_git(root: &Path, args: &[&str]) -> String {
        let out = grit_test_support::merge_git_config_env(&mut Command::new("git"), &[])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .current_dir(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// `git init` + one commit of `f1.txt`, then `f1.txt` modified in the worktree (issue #1789).
    fn git_repo_with_modified_file(root: &Path) -> Repository {
        system_git(root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("f1.txt"), b"a\n").unwrap();
        system_git(root, &["add", "."]);
        system_git(root, &["commit", "-qm", "init"]);
        fs::write(root.join("f1.txt"), b"a\nb\n").unwrap();
        Repository::discover(Some(root), &RepositoryOptions::empty()).unwrap()
    }

    fn f1_paths() -> ResolvedPathspecs {
        ResolvedPathspecs {
            specs: vec!["f1.txt".into()],
            sources: vec!["f1.txt".into()],
            scoped: true,
        }
    }

    /// Run `scope` while a foreign `index.lock` exists and check nothing became visible.
    ///
    /// `objects_untouched` additionally requires that no loose object was written; scopes that
    /// hash content before writing the index may leave unreferenced blobs.
    fn assert_commit_refused_by_foreign_lock(
        root: &Path,
        repo: &Repository,
        scope: CommitScope,
        objects_untouched: bool,
    ) {
        let lock_path = repo.git_dir().join("index.lock");
        fs::write(&lock_path, b"foreign\n").unwrap();
        let before = repo_snapshot(repo);
        let tip_before = resolve_ref(&repo.git_dir(), "refs/heads/main").unwrap();

        let err = commit_with_scope(
            repo,
            &commit_req("x"),
            scope,
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap_err();
        match &err {
            Error::LockHeld {
                path: reported,
                holder,
            } => {
                assert_eq!(reported, &lock_path);
                assert_eq!(*holder, None);
            }
            other => panic!("expected Error::LockHeld, got {other:?}"),
        }
        assert!(err.to_string().contains("index.lock"), "{err}");

        let (index, head, reflog, _editmsg, objects) = repo_snapshot(repo);
        assert_eq!(index, before.0, "index bytes must be unchanged");
        assert_eq!(head, before.1, "HEAD must be unchanged");
        assert_eq!(reflog, before.2, "no reflog entry may be written");
        if objects_untouched {
            assert_eq!(objects, before.4, "no objects may be written");
        }
        assert_eq!(
            resolve_ref(&repo.git_dir(), "refs/heads/main").unwrap(),
            tip_before
        );
        assert_eq!(
            fs::read(&lock_path).unwrap(),
            b"foreign\n",
            "someone else's lock must be left alone"
        );
        let leftovers: Vec<_> = fs::read_dir(repo.git_dir())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("next-index-"))
            .collect();
        assert!(leftovers.is_empty(), "temporary hook index left behind");

        fs::remove_file(&lock_path).unwrap();
        assert_eq!(system_git(root, &["status", "--porcelain"]), " M f1.txt\n");
        system_git(root, &["fsck", "--strict", "--no-progress"]);
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn scope_paths_with_held_index_lock_creates_nothing() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = git_repo_with_modified_file(root);
        assert_commit_refused_by_foreign_lock(root, &repo, CommitScope::Paths(f1_paths()), true);

        let outcome = commit_with_scope(
            &repo,
            &commit_req("x"),
            CommitScope::Paths(f1_paths()),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        assert!(!repo.git_dir().join("index.lock").exists());
        assert_eq!(
            system_git(root, &["rev-parse", "HEAD"]).trim(),
            outcome.oid.to_hex()
        );
        assert_eq!(system_git(root, &["status", "--porcelain"]), "");
        system_git(root, &["fsck", "--strict", "--no-progress"]);
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn scope_staged_and_auto_stage_with_held_index_lock_create_nothing() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = git_repo_with_modified_file(root);
        assert_commit_refused_by_foreign_lock(root, &repo, CommitScope::AutoStage, true);

        system_git(root, &["add", "f1.txt"]);
        let lock_path = repo.git_dir().join("index.lock");
        fs::write(&lock_path, b"foreign\n").unwrap();
        let tip_before = resolve_ref(&repo.git_dir(), "refs/heads/main").unwrap();
        let err = commit_with_scope(
            &repo,
            &commit_req("x"),
            CommitScope::Staged,
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap_err();
        assert!(
            matches!(&err, Error::LockHeld { path, .. } if *path == repo.git_dir().join("index.lock")),
            "{err:?}"
        );
        assert_eq!(
            resolve_ref(&repo.git_dir(), "refs/heads/main").unwrap(),
            tip_before
        );
        assert_eq!(fs::read(&lock_path).unwrap(), b"foreign\n");
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn scope_patch_with_held_index_lock_creates_nothing() {
        use crate::diff::hunk_id::{hunk_ids_with_duplicate_peers, DiffSide};
        use crate::diff::{unstaged_file_diffs_with_peer_hunks, DiffOptions};
        use crate::porcelain::hunk_select::HunkSelection;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = git_repo_with_modified_file(root);
        let (files, peers) =
            unstaged_file_diffs_with_peer_hunks(&repo, &[], DiffOptions::default()).unwrap();
        let ids = hunk_ids_with_duplicate_peers(
            DiffSide::Unstaged,
            "f1.txt",
            &files[0].hunks,
            peers.get("f1.txt").map(|v| v.as_slice()).unwrap_or(&[]),
        );
        let scope = CommitScope::Patch {
            input: CommitPatchInput::Hunks(HunkSelection {
                ids: vec![ids[0].clone()],
                ranges: Vec::new(),
            }),
            pathspecs: Vec::new(),
        };
        assert_commit_refused_by_foreign_lock(root, &repo, scope.clone(), true);

        let outcome = commit_with_scope(
            &repo,
            &commit_req("x"),
            scope,
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        assert!(!repo.git_dir().join("index.lock").exists());
        assert_eq!(
            system_git(root, &["rev-parse", "HEAD"]).trim(),
            outcome.oid.to_hex()
        );
        assert_eq!(system_git(root, &["status", "--porcelain"]), "");
        let index = repo.load_index().unwrap();
        verify_cache_tree(&index).expect("cache-tree valid");
        assert!(cache_tree_fully_valid(
            &repo.odb(),
            index.cache_tree.as_ref()
        ));
        let tree = parse_commit(&repo.odb().read(&outcome.oid).unwrap().data)
            .unwrap()
            .tree;
        assert_eq!(index.cache_tree_root, Some(tree));
        system_git(root, &["fsck", "--strict", "--no-progress"]);
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn scope_paths_split_index_round_trips_with_git() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        system_git(root, &["init", "-q", "-b", "main"]);
        system_git(root, &["config", "core.splitIndex", "true"]);
        system_git(root, &["config", "splitIndex.maxPercentChange", "100"]);
        for name in ["a.txt", "b.txt", "c.txt"] {
            fs::write(root.join(name), b"1\n").unwrap();
        }
        system_git(root, &["add", "."]);
        system_git(root, &["commit", "-qm", "init"]);
        fs::write(root.join("a.txt"), b"2\n").unwrap();
        fs::write(root.join("b.txt"), b"2\n").unwrap();
        system_git(root, &["add", "b.txt"]);

        let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).unwrap();
        let specs = vec!["a.txt".to_owned()];
        commit_with_scope(
            &repo,
            &commit_req("partial"),
            CommitScope::Paths(ResolvedPathspecs {
                specs: specs.clone(),
                sources: specs,
                scoped: true,
            }),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        assert!(!repo.git_dir().join("index.lock").exists());
        assert!(
            system_git(root, &["ls-files", "--debug"]).contains("a.txt"),
            "git must read the split index"
        );
        assert_eq!(system_git(root, &["status", "--porcelain"]), "M  b.txt\n");
        assert_eq!(system_git(root, &["show", "HEAD:a.txt"]), "2\n");
        assert_eq!(system_git(root, &["show", "HEAD:b.txt"]), "1\n");
        system_git(root, &["fsck", "--strict", "--no-progress"]);
        let shared: Vec<_> = fs::read_dir(repo.git_dir())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("sharedindex."))
            .collect();
        assert!(!shared.is_empty(), "index must stay split");
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn commit_on_split_index_with_skip_hash_stays_readable_by_git() {
        for key in ["index.skipHash", "feature.manyFiles"] {
            let tmp = TempDir::new().unwrap();
            let root = tmp.path();
            system_git(root, &["init", "-q", "-b", "main"]);
            fs::write(root.join("a"), b"1\n").unwrap();
            system_git(root, &["add", "a"]);
            system_git(root, &["commit", "-qm", "c1"]);
            system_git(root, &["config", "core.splitIndex", "true"]);
            system_git(root, &["update-index", "--split-index"]);
            fs::write(root.join("a"), b"1\n2\n").unwrap();
            system_git(root, &["add", "a"]);
            system_git(root, &["commit", "-qm", "c2"]);
            system_git(root, &["config", key, "true"]);
            assert_eq!(system_git(root, &["status", "--short"]), "");
            fs::write(root.join("n"), b"n\n").unwrap();

            let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).unwrap();
            let outcome = commit_with_scope(
                &repo,
                &commit_req("g"),
                CommitScope::AutoStage,
                &mut NullProgress,
                &crate::progress::NeverCancel,
            )
            .unwrap();

            assert_eq!(system_git(root, &["ls-files"]), "a\nn\n", "{key}");
            // Keep git from rebuilding the shared index itself: under skipHash it names it
            // `sharedindex.000…0` and then reads its own index as unsplit.
            system_git(root, &["config", "splitIndex.maxPercentChange", "100"]);
            assert_eq!(system_git(root, &["status", "--porcelain"]), "", "{key}");
            assert_eq!(
                system_git(root, &["rev-parse", "HEAD"]).trim(),
                outcome.oid.to_hex()
            );
            system_git(root, &["fsck", "--strict", "--no-progress"]);
            fs::write(root.join("m"), b"m\n").unwrap();
            system_git(root, &["add", "-A"]);
            system_git(root, &["commit", "-qm", "git after grit"]);
            assert_eq!(system_git(root, &["ls-files"]), "a\nm\nn\n", "{key}");
            assert_eq!(repo.load_index().unwrap().entries.len(), 3, "{key}");
        }
    }

    fn author_info_from_commit(repo: &Repository, oid: ObjectId) -> CommitAuthorInfo {
        let data = repo.odb().read(&oid).unwrap().data;
        author_info_from_header_line(&parse_commit(&data).unwrap().author)
    }

    fn git_head_commit_message(repo: &std::path::Path) -> String {
        let out = std::process::Command::new("git")
            .args(["cat-file", "commit", "HEAD"])
            .current_dir(repo)
            .output()
            .expect("git cat-file commit");
        assert!(out.status.success());
        let bytes = out.stdout;
        let mut i = 0usize;
        while i < bytes.len() {
            if i + 1 < bytes.len() && bytes[i] == b'\n' && bytes[i + 1] == b'\n' {
                i += 2;
                break;
            }
            i += 1;
        }
        String::from_utf8(bytes[i..].to_vec()).expect("utf-8 commit message")
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn message_file_stripspace() {
        let raw = "  subject  \n\n  body line  \n\n";
        let msg = normalize_commit_message(raw).unwrap();
        let tmp = TempDir::new().unwrap();
        let msg_path = tmp.path().join("msg.txt");
        std::fs::write(&msg_path, raw).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "T"])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "t@e.com"])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        std::fs::write(tmp.path().join("f"), "x\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "f"])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-F"])
            .arg(&msg_path)
            .current_dir(tmp.path())
            .status()
            .unwrap();
        let git_body = git_head_commit_message(tmp.path());
        assert_eq!(msg, git_body);
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn message_normalization_crlf_and_trailing_blanks_match_git_commit_f() {
        let raw = "\r\n  title  \r\n\r\n  body  \r\n\r\n\r\n";
        let grit_msg = normalize_commit_message(raw).unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let msg_path = tmp.path().join("msg.txt");
        std::fs::write(&msg_path, raw).unwrap();
        let git_msg = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        assert!(git_msg.success());
        std::process::Command::new("git")
            .args(["config", "user.name", "T"])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "t@e.com"])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        std::fs::write(tmp.path().join("f"), "x\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "f"])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-F"])
            .arg(&msg_path)
            .current_dir(tmp.path())
            .status()
            .unwrap();
        let git_body = git_head_commit_message(tmp.path());
        assert_eq!(grit_msg, git_body);
    }

    #[test]
    fn no_verify_skips_pre_commit_and_commit_msg() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        std::fs::write(
            repo.git_dir().join("config"),
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tlogAllRefUpdates = true\n[hook \"hc\"]\n\tcommand = exit 1\n\tevent = pre-commit\n[hook \"hc2\"]\n\tcommand = exit 1\n\tevent = commit-msg\n",
        )
        .unwrap();
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        let mut req = commit_req("blocked");
        req.hooks = HookPolicy::Run;
        assert!(create_commit(
            &repo,
            &req,
            &mut NullProgress,
            &crate::progress::NeverCancel
        )
        .is_err());

        req.hooks = HookPolicy::SkipVerify;
        let outcome = create_commit(
            &repo,
            &req,
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        assert!(outcome.hooks_run.is_empty());
    }

    #[test]
    fn post_commit_hook_failure_does_not_fail_commit() {
        use crate::hooks::HookName;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        std::fs::write(
            repo.git_dir().join("config"),
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tlogAllRefUpdates = true\n[hook \"hc\"]\n\tcommand = exit 9\n\tevent = post-commit\n",
        )
        .unwrap();
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let outcome = create_commit(
            &repo,
            &commit_req("after post"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        assert_eq!(
            resolve_ref(&repo.git_dir(), "refs/heads/main").unwrap(),
            outcome.oid
        );
        assert!(outcome.hooks_run.contains(&HookName::PostCommit));
    }

    #[test]
    fn hooks_run_lists_executed_hooks() {
        use crate::hooks::HookName;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        std::fs::write(
            repo.git_dir().join("config"),
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tlogAllRefUpdates = true\n[hook \"hc\"]\n\tcommand = true\n\tevent = pre-commit\n[hook \"hc2\"]\n\tcommand = true\n\tevent = commit-msg\n[hook \"hc3\"]\n\tcommand = true\n\tevent = post-commit\n[hook \"hp\"]\n\tcommand = echo \"prepared $2\" >>$1\n\tevent = prepare-commit-msg\n",
        )
        .unwrap();
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let outcome = create_commit(
            &repo,
            &commit_req("hooks"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        assert_eq!(
            outcome.hooks_run,
            vec![
                HookName::PreCommit,
                HookName::PrepareCommitMsg,
                HookName::CommitMsg,
                HookName::PostCommit
            ]
        );
        let commit = parse_commit(&repo.odb().read(&outcome.oid).unwrap().data).unwrap();
        assert_eq!(commit.message, "hooks\nprepared message\n");

        fs::write(root.join("a.txt"), b"2\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let mut req = commit_req("skip verify");
        req.hooks = HookPolicy::SkipVerify;
        let outcome = create_commit(
            &repo,
            &req,
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        assert_eq!(
            outcome.hooks_run,
            vec![HookName::PrepareCommitMsg, HookName::PostCommit]
        );
    }

    #[test]
    fn author_date_override() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let mut req = commit_req("dated");
        req.author_date = Some("946684800 +0000".to_owned());
        let outcome = create_commit(
            &repo,
            &req,
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        assert_eq!(outcome.author.date, "946684800 +0000");
    }

    #[test]
    fn trailers_appended_and_deduped() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let mut req = commit_req("trailers");
        req.trailers = vec![
            ("Acked-by".to_owned(), "Alice".to_owned()),
            ("Acked-by".to_owned(), "Alice".to_owned()),
        ];
        let outcome = create_commit(
            &repo,
            &req,
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let msg = parse_commit(&repo.odb().read(&outcome.oid).unwrap().data)
            .unwrap()
            .message;
        assert_eq!(msg.matches("Acked-by: Alice").count(), 1);
    }

    #[test]
    fn signoff_uses_committer() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let mut req = commit_req("signoff");
        req.committer = Some(
            "Committer Name <committer@example.com> 2 +0000"
                .parse()
                .expect("signature"),
        );
        req.signoff = true;
        let outcome = create_commit(
            &repo,
            &req,
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let msg = parse_commit(&repo.odb().read(&outcome.oid).unwrap().data)
            .unwrap()
            .message;
        assert!(msg.contains("Signed-off-by: Committer Name <committer@example.com>"));
    }

    #[test]
    fn signoff_before_explicit_trailers() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let mut req = commit_req("subject\n");
        req.committer = Some(
            "Committer Name <committer@example.com> 2 +0000"
                .parse()
                .expect("signature"),
        );
        req.signoff = true;
        req.trailers = vec![("Acked-by".to_owned(), "Alice".to_owned())];
        let outcome = create_commit(
            &repo,
            &req,
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let msg = parse_commit(&repo.odb().read(&outcome.oid).unwrap().data)
            .unwrap()
            .message;
        let sob = msg.find("Signed-off-by:").expect("signoff trailer");
        let ack = msg.find("Acked-by:").expect("acked trailer");
        assert!(sob < ack, "signoff must precede explicit trailers:\n{msg}");
    }

    #[test]
    fn amend_reuse_message_from_source() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let mut source = commit_req("source subject\n");
        source.author = Some(
            "Source Author <author@example.com> 978307200 +0000"
                .parse()
                .expect("signature"),
        );
        source.committer = Some(ident().parse().expect("signature"));
        let source_out = create_commit(
            &repo,
            &source,
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        fs::write(root.join("a.txt"), b"2\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let mut tip = commit_req("tip subject\n");
        tip.author = Some(
            "Tip Author <tip@example.com> 1792591163 +0000"
                .parse()
                .expect("signature"),
        );
        create_commit(
            &repo,
            &tip,
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        fs::write(root.join("a.txt"), b"3\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let mut amend = commit_req("");
        amend.amend = true;
        amend.author = None;
        amend.reuse_message_from = Some(source_out.oid);
        let outcome = create_commit(
            &repo,
            &amend,
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let author = author_info_from_commit(&repo, outcome.oid);
        assert_eq!(author.name, "Source Author");
        assert_eq!(author.email, "author@example.com");
        assert_eq!(author.date, "978307200 +0000");
        let msg = parse_commit(&repo.odb().read(&outcome.oid).unwrap().data)
            .unwrap()
            .message;
        assert!(msg.starts_with("source subject"));
    }

    #[test]
    fn amend_reuse_message_reset_author_uses_committer() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let mut source = commit_req("source subject\n");
        source.author = Some(
            "Source Author <author@example.com> 978307200 +0000"
                .parse()
                .expect("signature"),
        );
        let source_out = create_commit(
            &repo,
            &source,
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        fs::write(root.join("a.txt"), b"2\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        create_commit(
            &repo,
            &commit_req("tip"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        fs::write(root.join("a.txt"), b"3\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let mut amend = commit_req("");
        amend.amend = true;
        amend.author = None;
        amend.reset_author = true;
        amend.committer = Some(
            "Reset Author <reset@example.com> 1800000000 +0000"
                .parse()
                .expect("signature"),
        );
        amend.reuse_message_from = Some(source_out.oid);
        let outcome = create_commit(
            &repo,
            &amend,
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let author = author_info_from_commit(&repo, outcome.oid);
        assert_eq!(author.name, "Reset Author");
        assert_eq!(author.email, "reset@example.com");
    }

    #[test]
    fn reuse_message_keeps_author() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let mut first = commit_req("source");
        first.author = Some(
            "Source Author <source@example.com> 99 +0000"
                .parse()
                .expect("signature"),
        );
        first.committer = Some(ident().parse().expect("signature"));
        let first_out = create_commit(
            &repo,
            &first,
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        fs::write(root.join("a.txt"), b"2\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let mut req = commit_req("");
        req.author = None;
        req.reuse_message_from = Some(first_out.oid);
        let outcome = create_commit(
            &repo,
            &req,
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let author = author_info_from_commit(&repo, outcome.oid);
        assert_eq!(author.name, "Source Author");
        assert_eq!(author.email, "source@example.com");
        assert_eq!(author.date, "99 +0000");
    }

    #[test]
    fn reset_author_with_reuse_and_amend() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let mut first = commit_req("source");
        first.author = Some("Old <old@example.com> 1 +0000".parse().expect("signature"));
        create_commit(
            &repo,
            &first,
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        fs::write(root.join("a.txt"), b"2\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        create_commit(
            &repo,
            &commit_req("second"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        let mut amend = commit_req("");
        amend.amend = true;
        amend.author = None;
        amend.reset_author = true;
        amend.committer = Some(
            "New Committer <new@example.com> 2000 +0000"
                .parse()
                .expect("signature"),
        );
        let outcome = create_commit(
            &repo,
            &amend,
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let author = author_info_from_commit(&repo, outcome.oid);
        assert_eq!(author.name, "New Committer");
        assert_eq!(author.email, "new@example.com");
    }

    #[test]
    fn reuse_message_gets_new_change_id() {
        use crate::change_id::{change_id, ChangeIdError, ChangeIdGenerator};
        use std::sync::Arc;

        struct FixedGenerator([u8; 16]);
        impl ChangeIdGenerator for FixedGenerator {
            fn random_bytes(&self) -> std::result::Result<[u8; 16], ChangeIdError> {
                Ok(self.0)
            }
        }

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let gen1: Arc<dyn ChangeIdGenerator> = Arc::new(FixedGenerator([0x11; 16]));
        let git = root.join(".git");
        fs::create_dir_all(git.join("objects")).unwrap();
        fs::create_dir_all(git.join("refs/heads")).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(
            git.join("config"),
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tlogAllRefUpdates = true\n",
        )
        .unwrap();
        let repo = Repository::open(
            &git,
            &RepositoryOptions::empty()
                .with_work_tree(root)
                .with_change_id_generator(gen1),
        )
        .unwrap();
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let source = create_commit(
            &repo,
            &commit_req("source"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let source_data = parse_commit(&repo.odb().read(&source.oid).unwrap().data).unwrap();
        let source_id = change_id(&source_data).expect("source change id");

        let gen2: Arc<dyn ChangeIdGenerator> = Arc::new(FixedGenerator([0x22; 16]));
        let repo = Repository::open(
            &git,
            &RepositoryOptions::empty()
                .with_work_tree(root)
                .with_change_id_generator(gen2),
        )
        .unwrap();
        fs::write(root.join("a.txt"), b"2\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let mut req = commit_req("");
        req.author = None;
        req.reuse_message_from = Some(source.oid);
        let copy = create_commit(
            &repo,
            &req,
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let copy_data = parse_commit(&repo.odb().read(&copy.oid).unwrap().data).unwrap();
        let copy_id = change_id(&copy_data).expect("copy change id");
        assert_ne!(copy_id, source_id);
    }

    #[test]
    fn outcome_parents_merge_commit() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let first = create_commit(
            &repo,
            &commit_req("first"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        fs::write(root.join("b.txt"), b"2\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let second = create_commit(
            &repo,
            &commit_req("second"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        fs::write(
            repo.git_dir().join("MERGE_HEAD"),
            format!("{}\n", first.oid.to_hex()),
        )
        .unwrap();
        fs::write(root.join("a.txt"), b"merged\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let merge = create_commit(
            &repo,
            &commit_req("merge"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        assert_eq!(merge.parents.len(), 2);
        assert_eq!(merge.parents[0], second.oid);
        assert_eq!(merge.parents[1], first.oid);
    }

    #[test]
    fn auto_stage_config_false_defaults_to_staged() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let git = root.join(".git");
        fs::create_dir_all(git.join("objects")).unwrap();
        fs::create_dir_all(git.join("refs/heads")).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(
            git.join("config"),
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tlogAllRefUpdates = true\n[commit]\n\tautoStage = false\n",
        )
        .unwrap();
        let repo =
            Repository::open(&git, &RepositoryOptions::empty().with_work_tree(root)).unwrap();
        assert_eq!(
            default_commit_scope_from_config(&repo.config().unwrap()),
            CommitScope::Staged
        );
    }

    #[test]
    fn dry_run_path_scope_includes_untracked_add() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("tracked.txt"), b"1\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        create_commit(
            &repo,
            &commit_req("first"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        fs::write(root.join("new.txt"), b"new\n").unwrap();
        let paths = ResolvedPathspecs {
            specs: vec!["new.txt".into()],
            sources: vec!["new.txt".into()],
            scoped: true,
        };
        let before = repo_snapshot(&repo);
        let plan = plan_commit(
            &repo,
            &dry_run_req("add new"),
            CommitScope::Paths(paths),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.files[0].path, "new.txt");
        assert_eq!(repo_snapshot(&repo), before);
    }

    #[test]
    fn dry_run_amend_honors_author_override() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        create_commit(
            &repo,
            &commit_req("first"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        fs::write(root.join("a.txt"), b"2\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let mut req = dry_run_req("amend");
        req.amend = true;
        req.author = Some(
            "Override <o@example.com> 1700000000 +0000"
                .parse()
                .expect("signature"),
        );
        let outcome = commit_with_scope(
            &repo,
            &req,
            CommitScope::Staged,
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        assert_eq!(outcome.author.email, "o@example.com");
        assert_eq!(outcome.author.name, "Override");
    }

    fn git(dir: &std::path::Path, args: &[&str]) {
        let out = grit_test_support::merge_git_config_env(&mut Command::new("git"), &[])
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn two_hunk_file() -> (String, String) {
        let mut base = String::new();
        for i in 0..24 {
            base.push_str(&format!("line{i}\n"));
        }
        let mut edited = base.clone();
        edited = edited.replacen("line5\n", "LINE5\n", 1);
        edited = edited.replacen("line18\n", "LINE18\n", 1);
        (base, edited)
    }

    fn tree_blob(repo: &Repository, tree_oid: &ObjectId, path: &str) -> Vec<u8> {
        for (p, _mode, oid) in flatten_tree_entries(repo.odb(), tree_oid).unwrap() {
            if p == path {
                return repo.odb().read(&oid).unwrap().data;
            }
        }
        panic!("missing {path} in tree");
    }

    #[test]
    fn commit_hunks_only_selected_in_tree() {
        use crate::diff::hunk_id::{hunk_ids_with_duplicate_peers, DiffSide};
        use crate::diff::{unstaged_file_diffs_with_peer_hunks, DiffOptions};
        use crate::porcelain::hunk_select::HunkSelection;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let (base, edited) = two_hunk_file();
        fs::write(root.join("f.txt"), base).unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        create_commit(
            &repo,
            &commit_req("init"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        fs::write(root.join("f.txt"), edited).unwrap();

        let (files, peers) =
            unstaged_file_diffs_with_peer_hunks(&repo, &[], DiffOptions::default()).unwrap();
        let ids = hunk_ids_with_duplicate_peers(
            DiffSide::Unstaged,
            "f.txt",
            &files[0].hunks,
            peers.get("f.txt").map(|v| v.as_slice()).unwrap_or(&[]),
        );
        commit_with_patch(
            &repo,
            &commit_req("one hunk"),
            CommitPatchInput::Hunks(HunkSelection {
                ids: vec![ids[0].clone()],
                ranges: Vec::new(),
            }),
            &[],
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        let tip = resolve_ref(&repo.git_dir(), "refs/heads/main").unwrap();
        let tree = parse_commit(&repo.odb().read(&tip).unwrap().data)
            .unwrap()
            .tree;
        let blob = tree_blob(&repo, &tree, "f.txt");
        let text = String::from_utf8_lossy(&blob);
        assert!(text.contains("LINE5"), "first hunk in commit tree");
        assert!(
            !text.contains("LINE18"),
            "second hunk must not be in commit tree: {text}"
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn commit_hunks_leaves_other_unstaged() {
        use crate::diff::hunk_id::{hunk_ids_with_duplicate_peers, DiffSide};
        use crate::diff::{unstaged_file_diffs_with_peer_hunks, DiffOptions};
        use crate::porcelain::hunk_select::HunkSelection;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let (base, edited) = two_hunk_file();
        fs::write(root.join("f.txt"), base).unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        create_commit(
            &repo,
            &commit_req("init"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        fs::write(root.join("f.txt"), edited).unwrap();

        let (files, peers) =
            unstaged_file_diffs_with_peer_hunks(&repo, &[], DiffOptions::default()).unwrap();
        let ids = hunk_ids_with_duplicate_peers(
            DiffSide::Unstaged,
            "f.txt",
            &files[0].hunks,
            peers.get("f.txt").map(|v| v.as_slice()).unwrap_or(&[]),
        );
        commit_with_patch(
            &repo,
            &commit_req("partial"),
            CommitPatchInput::Hunks(HunkSelection {
                ids: vec![ids[0].clone()],
                ranges: Vec::new(),
            }),
            &[],
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        let diff = run_grit_status_unstaged(root);
        assert!(
            diff.contains("line18") || diff.contains("LINE18"),
            "remaining hunk still unstaged: {diff}"
        );
        git(root, &["fsck", "--strict"]);
    }

    fn run_grit_status_unstaged(root: &std::path::Path) -> String {
        let out = Command::new("git")
            .current_dir(root)
            .args(["diff"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn format_patch_for_git(patch: &FilePatch) -> String {
        use crate::apply::HunkLine;
        let mut out = String::new();
        let path = patch.effective_path().unwrap_or("f");
        out.push_str(&format!("diff --git a/{path} b/{path}\n"));
        if patch.is_new {
            out.push_str("new file mode 100644\n");
        }
        let old = if patch.is_new {
            "/dev/null".to_string()
        } else {
            format!("a/{}", patch.old_path.as_deref().unwrap_or(path))
        };
        let new = if patch.is_deleted {
            "/dev/null".to_string()
        } else {
            format!("b/{}", patch.new_path.as_deref().unwrap_or(path))
        };
        out.push_str(&format!("--- {old}\n"));
        out.push_str(&format!("+++ {new}\n"));
        for h in &patch.hunks {
            out.push_str(&format!(
                "@@ -{},{} +{},{} @@\n",
                h.old_start, h.old_count, h.new_start, h.new_count
            ));
            for line in &h.lines {
                match line {
                    HunkLine::Context(s) => {
                        out.push(' ');
                        out.push_str(s);
                        if !s.ends_with('\n') {
                            out.push('\n');
                        }
                    }
                    HunkLine::Add(s) => {
                        out.push('+');
                        out.push_str(s);
                        if !s.ends_with('\n') {
                            out.push('\n');
                        }
                    }
                    HunkLine::Remove(s) => {
                        out.push('-');
                        out.push_str(s);
                        if !s.ends_with('\n') {
                            out.push('\n');
                        }
                    }
                    HunkLine::NoNewline => out.push_str("\\ No newline at end of file\n"),
                }
            }
        }
        out
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn commit_patch_file_matches_git_apply_cached_then_commit() {
        use crate::diff::hunk_id::{hunk_ids_with_duplicate_peers, DiffSide};
        use crate::diff::{unstaged_file_diffs_with_peer_hunks, DiffOptions};
        use crate::porcelain::hunk_select::{resolve_selection, HunkSelection};

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let (base, edited) = two_hunk_file();
        fs::write(root.join("f.txt"), base.clone()).unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        create_commit(
            &repo,
            &commit_req("init"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        fs::write(root.join("f.txt"), edited.clone()).unwrap();

        let (files, peers) =
            unstaged_file_diffs_with_peer_hunks(&repo, &[], DiffOptions::default()).unwrap();
        let ids = hunk_ids_with_duplicate_peers(
            DiffSide::Unstaged,
            "f.txt",
            &files[0].hunks,
            peers.get("f.txt").map(|v| v.as_slice()).unwrap_or(&[]),
        );
        let patch = resolve_selection(
            &repo,
            DiffSide::Unstaged,
            &HunkSelection {
                ids: vec![ids[0].clone()],
                ranges: Vec::new(),
            },
            &[],
            DiffOptions::default(),
        )
        .unwrap();
        let patch_text = patch.iter().map(format_patch_for_git).collect::<String>();

        commit_with_patch(
            &repo,
            &commit_req("grit partial"),
            CommitPatchInput::Patches(patch),
            &[],
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let grit_tip = resolve_ref(&repo.git_dir(), "refs/heads/main").unwrap();
        let grit_tree = parse_commit(&repo.odb().read(&grit_tip).unwrap().data)
            .unwrap()
            .tree;

        let git_dir = tempfile::tempdir().unwrap();
        git(git_dir.path(), &["init", "-q", "-b", "main"]);
        fs::write(git_dir.path().join("f.txt"), base).unwrap();
        git(git_dir.path(), &["add", "f.txt"]);
        git(git_dir.path(), &["commit", "-qm", "init"]);
        fs::write(git_dir.path().join("f.txt"), edited).unwrap();
        fs::write(git_dir.path().join("partial.patch"), &patch_text).unwrap();
        git(git_dir.path(), &["apply", "--cached", "partial.patch"]);
        git(git_dir.path(), &["commit", "-qm", "git partial"]);
        let git_tip = Command::new("git")
            .current_dir(git_dir.path())
            .args(["rev-parse", "HEAD^{tree}"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        let git_tree_hex = String::from_utf8_lossy(&git_tip.stdout);
        let git_tree = ObjectId::from_hex(git_tree_hex.trim()).unwrap();
        assert_eq!(grit_tree, git_tree);
        git(root, &["fsck", "--strict"]);
    }

    fn loose_object_count(repo: &Repository) -> usize {
        repo.odb().count_loose_objects().unwrap_or(0)
    }

    #[test]
    fn commit_patch_dry_run_writes_no_loose_objects() {
        use crate::diff::hunk_id::{hunk_ids_with_duplicate_peers, DiffSide};
        use crate::diff::{unstaged_file_diffs_with_peer_hunks, DiffOptions};
        use crate::porcelain::hunk_select::{resolve_selection, HunkSelection};

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let (base, edited) = two_hunk_file();
        fs::write(root.join("f.txt"), base).unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        create_commit(
            &repo,
            &commit_req("init"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        fs::write(root.join("f.txt"), edited).unwrap();

        let before_loose = loose_object_count(&repo);
        let (files, peers) =
            unstaged_file_diffs_with_peer_hunks(&repo, &[], DiffOptions::default()).unwrap();
        let ids = hunk_ids_with_duplicate_peers(
            DiffSide::Unstaged,
            "f.txt",
            &files[0].hunks,
            peers.get("f.txt").map(|v| v.as_slice()).unwrap_or(&[]),
        );
        let patch = resolve_selection(
            &repo,
            DiffSide::Unstaged,
            &HunkSelection {
                ids: vec![ids[0].clone()],
                ranges: Vec::new(),
            },
            &[],
            DiffOptions::default(),
        )
        .unwrap();
        let before_tip = resolve_ref(&repo.git_dir(), "refs/heads/main").unwrap();
        let before_index = repo.load_index().unwrap().entries[0].oid;

        let mut req = commit_req("dry preview");
        req.dry_run = true;
        commit_with_patch(
            &repo,
            &req,
            CommitPatchInput::Patches(patch),
            &[],
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();

        assert_eq!(loose_object_count(&repo), before_loose);
        assert_eq!(
            resolve_ref(&repo.git_dir(), "refs/heads/main").unwrap(),
            before_tip
        );
        assert_eq!(repo.load_index().unwrap().entries[0].oid, before_index);
    }

    #[test]
    fn commit_hunks_stale_id_no_commit() {
        use crate::diff::hunk_id::HunkId;
        use crate::porcelain::hunk_select::HunkSelection;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("f.txt"), b"one\n").unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        create_commit(
            &repo,
            &commit_req("init"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let before_tip = resolve_ref(&repo.git_dir(), "refs/heads/main").unwrap();
        let before_index_oid = repo.load_index().unwrap().entries[0].oid;

        fs::write(root.join("f.txt"), b"two\n").unwrap();
        let err = commit_with_patch(
            &repo,
            &commit_req("nope"),
            CommitPatchInput::Hunks(HunkSelection {
                ids: vec![HunkId::from_str("000000000001").unwrap()],
                ranges: Vec::new(),
            }),
            &[],
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap_err();
        assert!(matches!(err, CommitPatchError::Select(_)));
        assert_eq!(
            resolve_ref(&repo.git_dir(), "refs/heads/main").unwrap(),
            before_tip
        );
        assert_eq!(repo.load_index().unwrap().entries[0].oid, before_index_oid);
    }

    #[test]
    fn commit_with_scope_stale_hunk_id_is_typed() {
        use crate::diff::hunk_id::HunkId;
        use crate::porcelain::hunk_select::{HunkSelectError, HunkSelection};

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"one\ntwo\n").unwrap();
        commit_with_scope(
            &repo,
            &commit_req("first"),
            CommitScope::AutoStage,
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let before_tip = resolve_ref(&repo.git_dir(), "refs/heads/main").unwrap();
        let before_index_oid = repo.load_index().unwrap().entries[0].oid;

        fs::write(root.join("a.txt"), b"one\ntwo\nthree\n").unwrap();
        let stale = HunkId::from_str("000000000000").unwrap();
        let err = commit_with_scope(
            &repo,
            &commit_req("bogus"),
            CommitScope::Patch {
                input: CommitPatchInput::Hunks(HunkSelection {
                    ids: vec![stale.clone()],
                    ranges: Vec::new(),
                }),
                pathspecs: Vec::new(),
            },
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap_err();
        match &err {
            Error::HunkSelection(HunkSelectError::StaleId { id, .. }) => assert_eq!(id, &stale),
            other => panic!("expected stale hunk id error, got {other:?}"),
        }
        assert_eq!(err.to_string(), "stale hunk id 000000000000");
        assert_eq!(err.kind_name(), "hunk_selection");
        assert_eq!(
            resolve_ref(&repo.git_dir(), "refs/heads/main").unwrap(),
            before_tip
        );
        assert_eq!(repo.load_index().unwrap().entries[0].oid, before_index_oid);

        let plan_err = plan_commit(
            &repo,
            &commit_req("bogus"),
            CommitScope::Patch {
                input: CommitPatchInput::Hunks(HunkSelection {
                    ids: vec![stale],
                    ranges: Vec::new(),
                }),
                pathspecs: Vec::new(),
            },
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap_err();
        assert!(matches!(
            plan_err,
            Error::HunkSelection(HunkSelectError::StaleId { .. })
        ));
    }

    #[test]
    fn commit_with_scope_patch_apply_failure_is_typed() {
        use crate::diff::hunk_id::{hunk_ids_with_duplicate_peers, DiffSide};
        use crate::diff::{unstaged_file_diffs_with_peer_hunks, DiffOptions};
        use crate::porcelain::hunk_select::{resolve_selection, HunkSelection};

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        let (base, edited) = two_hunk_file();
        fs::write(root.join("f.txt"), base).unwrap();
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        create_commit(
            &repo,
            &commit_req("init"),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        fs::write(root.join("f.txt"), edited).unwrap();

        let (files, peers) =
            unstaged_file_diffs_with_peer_hunks(&repo, &[], DiffOptions::default()).unwrap();
        let ids = hunk_ids_with_duplicate_peers(
            DiffSide::Unstaged,
            "f.txt",
            &files[0].hunks,
            peers.get("f.txt").map(|v| v.as_slice()).unwrap_or(&[]),
        );
        let patch = resolve_selection(
            &repo,
            DiffSide::Unstaged,
            &HunkSelection {
                ids: vec![ids[0].clone()],
                ranges: Vec::new(),
            },
            &[],
            DiffOptions::default(),
        )
        .unwrap();
        let patch_scope = |patch: Vec<FilePatch>| CommitScope::Patch {
            input: CommitPatchInput::Patches(patch),
            pathspecs: Vec::new(),
        };
        commit_with_scope(
            &repo,
            &commit_req("first hunk"),
            patch_scope(patch.clone()),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let before_tip = resolve_ref(&repo.git_dir(), "refs/heads/main").unwrap();

        // The index already holds the postimage, so the same patch no longer applies.
        let err = commit_with_scope(
            &repo,
            &commit_req("again"),
            patch_scope(patch),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap_err();
        assert!(matches!(err, Error::Apply(_)), "got {err:?}");
        assert_eq!(err.kind_name(), "apply");
        assert_eq!(
            resolve_ref(&repo.git_dir(), "refs/heads/main").unwrap(),
            before_tip
        );
    }

    #[test]
    fn commit_with_scope_empty_paths_is_typed() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"one\n").unwrap();
        commit_with_scope(
            &repo,
            &commit_req("first"),
            CommitScope::AutoStage,
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let before_tip = resolve_ref(&repo.git_dir(), "refs/heads/main").unwrap();

        fs::write(root.join("a.txt"), b"two\n").unwrap();
        let err = commit_with_scope(
            &repo,
            &commit_req("x"),
            CommitScope::Paths(ResolvedPathspecs::default()),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap_err();
        assert!(matches!(err, Error::CommitPathsRequired), "got {err:?}");
        assert_eq!(err.to_string(), "you must specify path(s)");
        assert_eq!(err.kind_name(), "commit_paths_required");
        assert_eq!(
            resolve_ref(&repo.git_dir(), "refs/heads/main").unwrap(),
            before_tip
        );
    }
}
