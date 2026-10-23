//! Create a commit from the current index and move the checked-out branch.
//!
//! [`create_commit`] performs incremental cache-tree write-tree, writes the commit
//! object, persists the index with a valid cache-tree, then updates the branch and `HEAD`
//! reflogs through [`crate::refs::update_branch_for_commit`].

use crate::change_id::{change_id, set_change_id, ChangeId, ChangeIdPolicy};
use crate::commit::assemble_identity;
use crate::commit::trailers::interpret::{
    process_trailers, NewTrailerArg, ProcessTrailerOptions, TrailerIfExists, TrailerIfMissing,
    TrailerWhere,
};
use crate::commit_trailers::{append_signoff_trailer_with_dedup, format_signoff_line};
use crate::config::ConfigSet;
use crate::diff::refresh_index_stat_content_verified_with_rules;
use crate::diff::{
    diff_tree_path_change_count, diff_trees_with_renames, flatten_tree_entries,
    materialize_worktree_blob, zero_oid, DiffEntry, DiffStatus,
};
use crate::error::{Error, Result};
use crate::hooks::{run_commit_hook, CommitHookEnv, HookError, HookName, HookResult};
use crate::ident::parse_signature_times;
use crate::index::Index;
use crate::objects::odb::Odb;
use crate::objects::{parse_commit, serialize_commit, CommitData, ObjectId, ObjectKind};
use crate::pathspec::{pathspec_is_exclude, ResolvedPathspecs};
use crate::porcelain::add::{stage_on_index, StageMode, StageOptions};
use crate::porcelain::index_from_tree::index_from_tree_with_worktree_stats;
use crate::porcelain::pathspec_match::KnownPathspecChecker;
use crate::progress::ProgressSink;
use crate::refs::{
    update_branch_for_commit_with_config, update_detached_head_for_commit_with_config,
    BranchCommitRefUpdate,
};
use crate::repo::Repository;
use crate::signing::{
    extra_headers_without_commit_signatures, should_sign_commit, sign_serialized_commit, GpgConfig,
};
use crate::state::{finish_merge_state, read_merge_heads, resolve_head, HeadState};
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

/// Whether commit hooks run before the object is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HookPolicy {
    /// Run `pre-commit` and `commit-msg` when installed (default).
    #[default]
    Run,
    /// Skip `pre-commit` and `commit-msg` (`--no-verify`); `post-commit` still runs.
    SkipVerify,
}

/// Author name, email, and date suffix from a written commit header line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitAuthorInfo {
    /// Author display name.
    pub name: String,
    /// Author email.
    pub email: String,
    /// Git timestamp suffix (`<unix> <+HHMM>`).
    pub date: String,
}

/// Inputs for [`create_commit`].
#[derive(Debug, Clone, Default)]
pub struct CommitRequest {
    /// Commit message body (first line becomes the subject). A trailing newline is added when missing.
    pub message: String,
    /// Author identity in Git header form: `Name <email> <unix-time> <tz>`.
    pub author: String,
    /// Committer identity in the same form as [`Self::author`].
    pub committer: String,
    /// When false (default), reject commits whose tree equals the parent's tree or is the empty tree on an unborn branch.
    pub allow_empty: bool,
    /// When `Some(true)`, sign even if `commit.gpgsign` is false; when `Some(false)`, skip signing even when config requests it. When `None`, honor `commit.gpgsign`.
    pub sign_override: Option<bool>,
    /// Replace the current branch tip: parents and author come from the tip being replaced; an empty
    /// [`Self::message`] reuses that commit's message. Same-tree amends are allowed.
    pub amend: bool,
    /// When true, skip the post-commit full-tree index stat refresh because the caller already
    /// synced the index from the worktree (for example via [`crate::porcelain::add::stage_for_commit`]).
    pub assume_worktree_synced: bool,
    /// When true with [`Self::assume_worktree_synced`], skip the pre-commit index↔parent tree
    /// match; the post-`write_tree` parent comparison still rejects empty commits (issue #1064).
    #[doc(hidden)]
    pub skip_pre_commit_tree_match: bool,
    /// When set with [`Self::skip_pre_commit_tree_match`], use this instead of a full tree diff for
    /// [`CommitOutcome::changes`] (issue #1064).
    #[doc(hidden)]
    pub staged_change_count: Option<usize>,
    /// Hook execution policy for `pre-commit` and `commit-msg`.
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
}

/// One path-level change that would be recorded in a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitScope {
    /// Stage every worktree change, then commit (default `grit commit` behavior).
    AutoStage,
    /// Commit the index as-is (`git commit` without `-a`).
    Staged,
    /// Commit only matching paths from the worktree (`git commit --only --` paths).
    Paths(ResolvedPathspecs),
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

/// Record a commit using `scope` to decide how the index is prepared.
///
/// # Errors
///
/// Same as [`create_commit`] and [`commit_worktree`], plus [`Error::PartialCommitDuringMerge`]
/// for [`CommitScope::Paths`] during an in-progress merge, and pathspec validation errors.
pub fn commit_with_scope(
    repo: &Repository,
    req: &CommitRequest,
    scope: CommitScope,
    progress: &mut dyn ProgressSink,
) -> Result<CommitOutcome> {
    match scope {
        CommitScope::AutoStage => commit_worktree(repo, req, progress),
        CommitScope::Staged => create_commit(repo, req, progress),
        CommitScope::Paths(paths) => commit_paths_scope(repo, req, &paths, progress),
    }
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
) -> Result<CommitPlan> {
    let mut req = req.clone();
    req.dry_run = true;
    let outcome = commit_with_scope(repo, &req, scope, progress)?;
    Ok(CommitPlan {
        files: outcome.files,
        parents: outcome.parents,
        author: outcome.author,
        tree_unchanged: outcome.changes == 0,
    })
}

fn commit_paths_scope(
    repo: &Repository,
    req: &CommitRequest,
    paths: &ResolvedPathspecs,
    progress: &mut dyn ProgressSink,
) -> Result<CommitOutcome> {
    if paths.specs.is_empty() {
        return Err(Error::Message("you must specify path(s)".into()));
    }
    if !read_merge_heads(repo.git_dir())?.is_empty() {
        return Err(Error::PartialCommitDuringMerge);
    }
    validate_commit_pathspecs(repo, paths)?;

    let head = resolve_head(repo.git_dir())?;
    let parent_tree = match head {
        HeadState::Branch { oid: Some(p), .. } | HeadState::Detached { oid: p } => {
            Some(commit_tree_oid(repo, &p)?)
        }
        HeadState::Branch { oid: None, .. } => None,
        HeadState::Invalid => {
            return Err(Error::Message("HEAD is in an unknown state".into()));
        }
    };

    let mut real_index = repo.load_index()?;
    if real_index.has_unmerged_entries() {
        return Err(Error::IndexUnmerged);
    }
    let mut commit_index = match parent_tree.as_ref() {
        Some(tree) => index_from_tree_with_worktree_stats(repo, tree)?,
        None => Index::new(),
    };

    let stage_opts = StageOptions {
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

    let staged_commit = stage_on_index(repo, &mut commit_index, &stage_opts, progress)?;
    if !req.dry_run {
        let _staged_real = stage_on_index(repo, &mut real_index, &stage_opts, progress)?;
    }

    let mut req = req.clone();
    req.assume_worktree_synced = true;
    req.skip_pre_commit_tree_match = false;
    req.staged_change_count = Some(staged_commit.total().max(1));

    create_commit_with_index(
        repo,
        &mut commit_index,
        &req,
        progress,
        Some(&mut real_index),
    )
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
/// Returns [`Error::Message`] when the cleaned body would be empty.
pub fn normalize_commit_message(raw: &str) -> Result<String> {
    let processed = process(raw.as_bytes(), &Mode::Default);
    let message = String::from_utf8(processed)
        .map_err(|_| Error::Message("invalid commit message".into()))?;
    if message.is_empty() {
        return Err(Error::Message("empty commit message".into()));
    }
    let mut message = message;
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
    match run_commit_hook(repo, hook.as_str(), args, stdin_data, commit_env)
        .map_err(HookError::Internal)?
    {
        HookResult::Success => Ok(true),
        HookResult::NotFound => Ok(false),
        HookResult::Failed(status) => Err(HookError::Failed {
            hook_name: hook.as_str().to_owned(),
            status,
        }),
    }
}

/// Like [`run_commit_hook_recorded`], but a non-zero exit from `post-commit` does not fail the commit (Git-compatible).
fn run_post_commit_hook_recorded(
    repo: &Repository,
    commit_env: &CommitHookEnv<'_>,
) -> std::result::Result<bool, HookError> {
    match run_commit_hook(repo, HookName::PostCommit.as_str(), &[], None, commit_env)
        .map_err(HookError::Internal)?
    {
        HookResult::Success | HookResult::Failed(_) => Ok(true),
        HookResult::NotFound => Ok(false),
    }
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

fn apply_author_date_suffix(author: &str, date_suffix: &str) -> String {
    let info = author_info_from_header_line(author);
    if info.email.is_empty() && !info.name.is_empty() && info.name.contains('@') {
        format!("{} {}", info.name.trim(), date_suffix.trim())
    } else if info.email.is_empty() {
        format!("{} {}", author.trim_end(), date_suffix.trim())
    } else {
        format!("{} <{}> {}", info.name, info.email, date_suffix.trim())
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
pub struct CommitOutcome {
    /// OID of the new commit object written to the object database.
    pub oid: ObjectId,
    /// Short branch name (e.g. `main`), or `detached HEAD` when not on a branch.
    pub branch: String,
    /// Parent commit OID, or `None` for the root commit on an unborn branch.
    pub parent: Option<ObjectId>,
    /// Every parent OID stored in the new commit object (empty only before the first commit is written).
    pub parents: Vec<ObjectId>,
    /// Author recorded on the commit object.
    pub author: CommitAuthorInfo,
    /// Commit hooks that were installed and executed successfully during this commit.
    pub hooks_run: Vec<HookName>,
    /// Number of path-level changes between the parent commit tree and this commit's tree.
    pub changes: usize,
    /// Stored `change-id` header when present on the new commit object.
    pub change_id: Option<ChangeId>,
    /// Path-level changes vs the parent tree (`git diff --name-status` between parent and new tree).
    pub files: Vec<CommitFile>,
}

/// Write a commit from the current index and advance the checked-out branch.
///
/// Loads the index from disk, builds the commit tree with an incremental cache-tree
/// write-tree, writes the commit object, writes the index back with an updated cache-tree,
/// then atomically updates the branch ref and reflogs (see [`crate::refs::update_branch_for_commit_with_config`]).
///
/// # Parameters
///
/// - `repo` — open repository with a work tree and branch `HEAD`.
/// - `req` — message, author/committer identities, and empty-commit policy.
/// - `progress` — optional progress sink (single `commit` phase).
///
/// # Errors
///
/// - [`Error::AmendUnborn`] when [`CommitRequest::amend`] is set but the branch has no commits yet.
/// - [`Error::NothingToCommit`] when the new tree is empty (unborn branch) or matches the parent tree and [`CommitRequest::allow_empty`] is false (not applied when amending).
/// - [`Error::IndexUnmerged`] when the index has conflict stages.
/// - Index write or diff failures before the branch is updated; the branch tip is not advanced.
/// - Ref/reflog failures from [`crate::refs::update_branch_for_commit_with_config`]; the branch tip is not left advanced without reflogs when logging is enabled.
pub fn create_commit(
    repo: &Repository,
    req: &CommitRequest,
    progress: &mut dyn ProgressSink,
) -> Result<CommitOutcome> {
    let mut index = repo.load_index()?;
    create_commit_with_index(repo, &mut index, req, progress, None)
}

/// Stage every worktree change and record a commit using a single in-memory index (issue #1064).
///
/// Avoids an extra index load/write between implicit staging and commit tree construction.
///
/// # Errors
///
/// Same as [`create_commit`], plus staging failures from [`crate::porcelain::add::stage_for_commit`].
pub fn commit_worktree(
    repo: &Repository,
    req: &CommitRequest,
    progress: &mut dyn ProgressSink,
) -> Result<CommitOutcome> {
    let mut index = repo.load_index()?;
    let staged =
        crate::porcelain::add::stage_for_commit_on_index(repo, &mut index, progress, req.dry_run)?;
    if index.has_unmerged_entries() {
        return Err(Error::IndexUnmerged);
    }
    let mut req = req.clone();
    if req.assume_worktree_synced && req.skip_pre_commit_tree_match {
        req.staged_change_count = Some(staged.total());
    }
    create_commit_with_index(repo, &mut index, &req, progress, None)
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
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;
    let rules = WorktreeRules::from_repository(repo, index)?;
    for entry in index
        .entries
        .iter()
        .filter(|e| e.stage() == 0 && !e.intent_to_add())
    {
        if odb.exists(&entry.oid) {
            continue;
        }
        let rel = String::from_utf8_lossy(&entry.path);
        let abs = crate::index::worktree_path_from_index_rel(work_tree, &entry.path);
        let meta = fs::metadata(&abs).map_err(Error::Io)?;
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
    short_name: String,
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
            return Err(Error::NothingToCommit);
        }
        if !concluding_merge && files.is_empty() {
            return Err(Error::NothingToCommit);
        }
    }

    let mut message = if !req.message.trim().is_empty() {
        normalize_commit_message(&req.message)?
    } else if let Some(reuse_msg) = reuse_message {
        reuse_msg
    } else if req_amend {
        amend_default_message.unwrap_or_default()
    } else {
        return Err(Error::Message("empty commit message".into()));
    };
    if message.trim().is_empty() {
        return Err(Error::Message("empty commit message".into()));
    }
    if !message.ends_with('\n') {
        message.push('\n');
    }
    let config = repo.config()?;
    let _message_with_trailers = enrich_commit_message(
        message,
        &req.trailers,
        req.signoff,
        &req.committer,
        config.as_ref(),
        repo.git_dir(),
    )?;

    let (author, _author_raw) = dry_run_author_fields(
        repo,
        req,
        amend_author,
        amend_author_raw,
        reuse_author,
        reuse_author_raw,
        req_amend,
    );
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

fn dry_run_author_fields(
    repo: &Repository,
    req: &CommitRequest,
    amend_author: Option<String>,
    amend_author_raw: Vec<u8>,
    reuse_author: Option<String>,
    reuse_author_raw: Vec<u8>,
    req_amend: bool,
) -> (String, Vec<u8>) {
    let now = repo.identity_now();
    if req.reset_author {
        return (
            author_line_from_committer(&req.committer, req.author_date.as_deref(), now),
            Vec::new(),
        );
    }
    if !req.author.is_empty() {
        let mut author = req.author.clone();
        if let Some(ref date) = req.author_date {
            author = apply_author_date_suffix(&author, date);
        }
        return (author, Vec::new());
    }
    if let Some(a) = reuse_author {
        let mut author = a;
        if !req.reset_author {
            if let Some(ref date) = req.author_date {
                author = apply_author_date_suffix(&author, date);
            }
        }
        return (author, reuse_author_raw);
    }
    if req_amend {
        let mut author = amend_author.unwrap_or_default();
        if let Some(ref date) = req.author_date {
            author = apply_author_date_suffix(&author, date);
        }
        return (author, amend_author_raw);
    }
    let mut author = req.author.clone();
    if let Some(ref date) = req.author_date {
        author = apply_author_date_suffix(&author, date);
    }
    (author, Vec::new())
}

/// Removes a partial-commit hook index file on drop (Git `next-index-<pid>`).
struct TempHookIndexFile(PathBuf);

impl Drop for TempHookIndexFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn create_commit_with_index(
    repo: &Repository,
    index: &mut Index,
    req: &CommitRequest,
    _progress: &mut dyn ProgressSink,
    persist_index: Option<&mut Index>,
) -> Result<CommitOutcome> {
    let head = resolve_head(repo.git_dir())?;
    let (refname, short_name, mut parent, head_detached) = match head {
        HeadState::Branch {
            refname,
            short_name,
            oid,
        } => (refname, short_name, oid, false),
        HeadState::Detached { oid } => (
            "HEAD".to_owned(),
            "detached HEAD".to_owned(),
            Some(oid),
            true,
        ),
        HeadState::Invalid => {
            return Err(Error::Message("HEAD is in an unknown state".into()));
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
        let obj = repo.odb().read(&reuse_oid)?;
        if obj.kind != ObjectKind::Commit {
            return Err(Error::CorruptObject(format!(
                "expected commit for reuse-message, got {}",
                obj.kind.as_str()
            )));
        }
        let source = parse_commit(&obj.data)?;
        reuse_message = Some(source.message);
        reuse_author = Some(source.author);
        reuse_author_raw = source.author_raw;
    }
    if req.amend {
        let Some(tip) = parent.take() else {
            return Err(Error::AmendUnborn);
        };
        let obj = repo.odb().read(&tip)?;
        if obj.kind != ObjectKind::Commit {
            return Err(Error::CorruptObject(format!(
                "expected commit, got {}",
                obj.kind.as_str()
            )));
        }
        let old = parse_commit(&obj.data)?;
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
        .map(|parent_oid| commit_tree_oid(repo, parent_oid))
        .transpose()?;

    if !req.allow_empty && !req.amend && !concluding_merge {
        let skip_tree_match = req.assume_worktree_synced && req.skip_pre_commit_tree_match;
        if !skip_tree_match {
            if let Some(ref old) = parent_tree {
                if index_staged_tree_matches(repo.odb(), index, old) {
                    return Err(Error::NothingToCommit);
                }
            } else if index.entries.is_empty() {
                return Err(Error::NothingToCommit);
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

    let partial_commit = persist_index.is_some();
    let _temp_hook_index_guard = if partial_commit {
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

    let tree = write_tree_update_index(repo.odb(), index, "", WriteTreeFlags::silent())?;
    let commit_files = commit_files_between_trees(repo.odb(), parent_tree.as_ref(), &tree)?;

    if !req.allow_empty && !req.amend {
        if parent_tree.is_none() && is_empty_tree_oid(repo.odb(), &tree) {
            return Err(Error::NothingToCommit);
        }
        if !concluding_merge {
            if let Some(ref old) = parent_tree {
                if old == &tree {
                    return Err(Error::NothingToCommit);
                }
            }
        }
    }

    let mut message = if !req.message.trim().is_empty() {
        normalize_commit_message(&req.message)?
    } else if let Some(reuse_msg) = reuse_message {
        reuse_msg
    } else if req.amend {
        amend_default_message.unwrap_or_default()
    } else {
        return Err(Error::Message("empty commit message".into()));
    };
    if message.trim().is_empty() {
        return Err(Error::Message("empty commit message".into()));
    }
    if !message.ends_with('\n') {
        message.push('\n');
    }
    let config = repo.config()?;
    message = enrich_commit_message(
        message,
        &req.trailers,
        req.signoff,
        &req.committer,
        config.as_ref(),
        repo.git_dir(),
    )?;
    let editmsg_path = repo.git_dir().join("COMMIT_EDITMSG");
    fs::write(&editmsg_path, &message).map_err(Error::Io)?;
    let editmsg_arg = commit_editmsg_hook_argument(repo, &editmsg_path);
    if req.hooks != HookPolicy::SkipVerify
        && run_commit_hook_recorded(
            repo,
            HookName::CommitMsg,
            &[editmsg_arg.as_str()],
            None,
            &commit_env,
        )?
    {
        hooks_run.push(HookName::CommitMsg);
    }
    message = fs::read_to_string(&editmsg_path).map_err(Error::Io)?;
    if message.trim().is_empty() {
        return Err(Error::Message("empty commit message".into()));
    }
    if !message.ends_with('\n') {
        message.push('\n');
    }

    let (mut author, mut author_raw) = if req.reset_author {
        (
            author_line_from_committer(
                &req.committer,
                req.author_date.as_deref(),
                repo.identity_now(),
            ),
            Vec::new(),
        )
    } else if !req.author.is_empty() {
        (req.author.clone(), Vec::new())
    } else if let Some(a) = reuse_author {
        (a, reuse_author_raw)
    } else if req.amend {
        (amend_author.unwrap_or_default(), amend_author_raw)
    } else {
        (req.author.clone(), Vec::new())
    };
    if !req.reset_author {
        if let Some(ref date) = req.author_date {
            author = apply_author_date_suffix(&author, date);
            author_raw = Vec::new();
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
        committer: req.committer.clone(),
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
    let oid = repo.odb().write(ObjectKind::Commit, &bytes)?;

    let previous_tip = replaced_tip.or(parent);
    let reflog_old = previous_tip.unwrap_or_else(zero_oid);
    let subject = commit_subject(&commit_data.message);
    let reflog_msg = if req.amend {
        format!("commit (amend): {subject}")
    } else if parent.is_some() {
        format!("commit: {subject}")
    } else {
        format!("commit (initial): {subject}")
    };

    let expected_old = previous_tip.or_else(|| {
        if crate::refs::resolve_ref(repo.git_dir(), &refname).is_ok() {
            Some(reflog_old)
        } else {
            None
        }
    });

    if !partial_commit {
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

    if head_detached {
        update_detached_head_for_commit_with_config(
            repo.git_dir(),
            expected_old,
            oid,
            &req.committer,
            &reflog_msg,
            config.as_ref(),
        )?;
    } else {
        update_branch_for_commit_with_config(
            repo.git_dir(),
            &BranchCommitRefUpdate {
                branch_ref: &refname,
                expected_old,
                new_oid: oid,
                identity: &req.committer,
                reflog_message: &reflog_msg,
            },
            config.as_ref(),
        )?;
    }

    if concluding_merge {
        let _ = finish_merge_state(repo)?;
    }

    if let Some(persist) = persist_index {
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
        repo.write_index(persist)?;
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
    if run_post_commit_hook_recorded(repo, &post_commit_env)? {
        hooks_run.push(HookName::PostCommit);
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

fn commit_tree_oid(repo: &Repository, commit_oid: &ObjectId) -> Result<ObjectId> {
    let obj = repo.odb().read(commit_oid)?;
    if obj.kind != ObjectKind::Commit {
        return Err(Error::CorruptObject(format!(
            "expected commit, got {}",
            obj.kind.as_str()
        )));
    }
    Ok(parse_commit(&obj.data)?.tree)
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

    fn commit_req(message: &str) -> CommitRequest {
        let ident = ident();
        CommitRequest {
            message: message.to_owned(),
            author: ident.clone(),
            committer: ident,
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

    #[test]
    fn create_commit_initial() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("README"), b"hello\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();

        let outcome = create_commit(&repo, &commit_req("initial"), &mut NullProgress).unwrap();
        assert!(outcome.parent.is_none());
        assert_eq!(outcome.branch, "main");
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
        let err = create_commit(&repo, &commit_req("empty"), &mut NullProgress).unwrap_err();
        assert!(matches!(err, Error::NothingToCommit));
        assert!(resolve_ref(&repo.git_dir(), "refs/heads/main").is_err());
    }

    #[test]
    fn create_commit_normal() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        create_commit(&repo, &commit_req("first"), &mut NullProgress).unwrap();

        fs::write(root.join("a.txt"), b"2\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let outcome = create_commit(&repo, &commit_req("second"), &mut NullProgress).unwrap();
        assert_eq!(outcome.changes, 1);
        assert!(outcome.parent.is_some());
    }

    #[test]
    fn create_commit_nothing_to_commit() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        create_commit(&repo, &commit_req("first"), &mut NullProgress).unwrap();

        let err = create_commit(&repo, &commit_req("noop"), &mut NullProgress).unwrap_err();
        assert!(matches!(err, Error::NothingToCommit));
    }

    #[test]
    fn create_commit_detached_head_advances_head() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let first = create_commit(&repo, &commit_req("first"), &mut NullProgress).unwrap();
        fs::write(repo.git_dir().join("HEAD"), first.oid.to_hex()).unwrap();

        fs::write(root.join("b.txt"), b"2\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let second = create_commit(&repo, &commit_req("second"), &mut NullProgress).unwrap();
        assert_eq!(second.branch, "detached HEAD");
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
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let outcome = create_commit(&repo, &commit_req("subject line"), &mut NullProgress).unwrap();

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
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();

        set_test_inject_index_write_fail(true);
        let err = create_commit(&repo, &commit_req("fail"), &mut NullProgress).unwrap_err();
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
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();

        set_test_inject_reflog_fail(Some("HEAD"));
        let err = create_commit(&repo, &commit_req("fail"), &mut NullProgress).unwrap_err();
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

        let outcome = create_commit(&repo, &commit_req("tree"), &mut NullProgress).unwrap();

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
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        create_commit(&repo, &commit_req("first"), &mut NullProgress).unwrap();
        fs::write(root.join("a.txt"), b"2\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        create_commit(&repo, &commit_req("second"), &mut NullProgress).unwrap();

        let mut req = commit_req("");
        req.amend = true;
        let tip = resolve_ref(&repo.git_dir(), "refs/heads/main").unwrap();
        let outcome = create_commit(&repo, &req, &mut NullProgress).unwrap();
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
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        create_commit(&repo, &commit_req("oneline subject"), &mut NullProgress).unwrap();
        let baseline = {
            let oid = resolve_ref(&repo.git_dir(), "refs/heads/main").unwrap();
            parse_commit(&repo.odb().read(&oid).unwrap().data)
                .unwrap()
                .message
        };

        for extra in 1..=2 {
            fs::write(root.join("a.txt"), format!("a\n{extra}\n")).unwrap();
            stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
            let mut req = commit_req("");
            req.amend = true;
            create_commit(&repo, &req, &mut NullProgress).unwrap();
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
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let mut req = commit_req("nope");
        req.amend = true;
        let err = create_commit(&repo, &req, &mut NullProgress).unwrap_err();
        assert!(matches!(err, Error::AmendUnborn));
    }

    #[test]
    fn create_commit_allow_empty() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        create_commit(&repo, &commit_req("first"), &mut NullProgress).unwrap();

        let mut req = commit_req("empty");
        req.allow_empty = true;
        let parent = resolve_ref(&repo.git_dir(), "refs/heads/main").unwrap();
        let outcome = create_commit(&repo, &req, &mut NullProgress).unwrap();
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
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        create_commit(&repo, &commit_req("first"), &mut NullProgress).unwrap();

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
        )
        .unwrap();
        assert_eq!(outcome.changes, 1);
        let index = repo.load_index().unwrap();
        let b_oid = index.get(b"b.txt", 0).unwrap().oid;
        let b_blob = repo.odb().read(&b_oid).unwrap();
        assert_eq!(b_blob.data, b"1\n", "unstaged paths stay in the index");
    }

    #[test]
    fn scope_paths_leaves_other_staged_entries() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("file"), b"1\n").unwrap();
        fs::write(root.join("baz"), b"1\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        create_commit(&repo, &commit_req("initial"), &mut NullProgress).unwrap();

        fs::write(root.join("file"), b"2\n").unwrap();
        fs::write(root.join("baz"), b"2\n").unwrap();
        stage(
            &repo,
            &StageOptions {
                pathspecs: vec!["baz".into()],
                ..StageOptions::default()
            },
            &mut NullProgress,
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
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        create_commit(&repo, &commit_req("first"), &mut NullProgress).unwrap();

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
        )
        .unwrap();

        let index = repo.load_index().unwrap();
        assert!(index.get(b"tracked.txt", 0).is_none());
        assert!(index.get(b"new.txt", 0).is_some());
    }

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
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        create_commit(&repo, &commit_req("first"), &mut NullProgress).unwrap();

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
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        create_commit(&repo, &commit_req("first"), &mut NullProgress).unwrap();
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
        )
        .unwrap_err();
        assert!(matches!(err, Error::PartialCommitDuringMerge));
    }

    fn author_info_from_commit(repo: &Repository, oid: ObjectId) -> CommitAuthorInfo {
        let data = repo.odb().read(&oid).unwrap().data;
        author_info_from_header_line(&parse_commit(&data).unwrap().author)
    }

    #[test]
    fn message_file_stripspace() {
        let msg = normalize_commit_message("  subject  \n\n  body line  \n\n").unwrap();
        assert_eq!(msg, "  subject\n\n  body line\n");
    }

    #[test]
    fn no_verify_skips_pre_commit_and_commit_msg() {
        use crate::hooks::HookName;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        std::fs::write(
            repo.git_dir().join("config"),
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tlogAllRefUpdates = true\n[hook \"hc\"]\n\tcommand = exit 1\n\tevent = pre-commit\n[hook \"hc2\"]\n\tcommand = exit 1\n\tevent = commit-msg\n",
        )
        .unwrap();
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();

        let mut req = commit_req("blocked");
        req.hooks = HookPolicy::Run;
        assert!(create_commit(&repo, &req, &mut NullProgress).is_err());

        req.hooks = HookPolicy::SkipVerify;
        let outcome = create_commit(&repo, &req, &mut NullProgress).unwrap();
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
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let outcome = create_commit(&repo, &commit_req("after post"), &mut NullProgress).unwrap();
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
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tlogAllRefUpdates = true\n[hook \"hc\"]\n\tcommand = true\n\tevent = pre-commit\n[hook \"hc2\"]\n\tcommand = true\n\tevent = commit-msg\n[hook \"hc3\"]\n\tcommand = true\n\tevent = post-commit\n",
        )
        .unwrap();
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let outcome = create_commit(&repo, &commit_req("hooks"), &mut NullProgress).unwrap();
        assert_eq!(
            outcome.hooks_run,
            vec![
                HookName::PreCommit,
                HookName::CommitMsg,
                HookName::PostCommit
            ]
        );
    }

    #[test]
    fn author_date_override() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let mut req = commit_req("dated");
        req.author_date = Some("946684800 +0000".to_owned());
        let outcome = create_commit(&repo, &req, &mut NullProgress).unwrap();
        assert_eq!(outcome.author.date, "946684800 +0000");
    }

    #[test]
    fn trailers_appended_and_deduped() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let mut req = commit_req("trailers");
        req.trailers = vec![
            ("Acked-by".to_owned(), "Alice".to_owned()),
            ("Acked-by".to_owned(), "Alice".to_owned()),
        ];
        let outcome = create_commit(&repo, &req, &mut NullProgress).unwrap();
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
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let mut req = commit_req("signoff");
        req.committer = "Committer Name <committer@example.com> 2 +0000".to_owned();
        req.signoff = true;
        let outcome = create_commit(&repo, &req, &mut NullProgress).unwrap();
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
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let mut req = commit_req("subject\n");
        req.committer = "Committer Name <committer@example.com> 2 +0000".to_owned();
        req.signoff = true;
        req.trailers = vec![("Acked-by".to_owned(), "Alice".to_owned())];
        let outcome = create_commit(&repo, &req, &mut NullProgress).unwrap();
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
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let mut source = commit_req("source subject\n");
        source.author = "Source Author <author@example.com> 978307200 +0000".to_owned();
        source.committer = ident();
        let source_out = create_commit(&repo, &source, &mut NullProgress).unwrap();

        fs::write(root.join("a.txt"), b"2\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let mut tip = commit_req("tip subject\n");
        tip.author = "Tip Author <tip@example.com> 1792591163 +0000".to_owned();
        create_commit(&repo, &tip, &mut NullProgress).unwrap();

        fs::write(root.join("a.txt"), b"3\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let mut amend = commit_req("");
        amend.amend = true;
        amend.author = String::new();
        amend.reuse_message_from = Some(source_out.oid);
        let outcome = create_commit(&repo, &amend, &mut NullProgress).unwrap();
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
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let mut source = commit_req("source subject\n");
        source.author = "Source Author <author@example.com> 978307200 +0000".to_owned();
        let source_out = create_commit(&repo, &source, &mut NullProgress).unwrap();

        fs::write(root.join("a.txt"), b"2\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        create_commit(&repo, &commit_req("tip"), &mut NullProgress).unwrap();

        fs::write(root.join("a.txt"), b"3\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let mut amend = commit_req("");
        amend.amend = true;
        amend.author = String::new();
        amend.reset_author = true;
        amend.committer = "Reset Author <reset@example.com> 1800000000 +0000".to_owned();
        amend.reuse_message_from = Some(source_out.oid);
        let outcome = create_commit(&repo, &amend, &mut NullProgress).unwrap();
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
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let mut first = commit_req("source");
        first.author = "Source Author <source@example.com> 99 +0000".to_owned();
        first.committer = ident();
        let first_out = create_commit(&repo, &first, &mut NullProgress).unwrap();

        fs::write(root.join("a.txt"), b"2\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let mut req = commit_req("");
        req.author = String::new();
        req.reuse_message_from = Some(first_out.oid);
        let outcome = create_commit(&repo, &req, &mut NullProgress).unwrap();
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
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let mut first = commit_req("source");
        first.author = "Old <old@example.com> 1 +0000".to_owned();
        create_commit(&repo, &first, &mut NullProgress).unwrap();

        fs::write(root.join("a.txt"), b"2\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        create_commit(&repo, &commit_req("second"), &mut NullProgress).unwrap();

        let mut amend = commit_req("");
        amend.amend = true;
        amend.author = String::new();
        amend.reset_author = true;
        amend.committer = "New Committer <new@example.com> 2000 +0000".to_owned();
        let outcome = create_commit(&repo, &amend, &mut NullProgress).unwrap();
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
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let source = create_commit(&repo, &commit_req("source"), &mut NullProgress).unwrap();
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
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let mut req = commit_req("");
        req.author = String::new();
        req.reuse_message_from = Some(source.oid);
        let copy = create_commit(&repo, &req, &mut NullProgress).unwrap();
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
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let first = create_commit(&repo, &commit_req("first"), &mut NullProgress).unwrap();

        fs::write(root.join("b.txt"), b"2\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let second = create_commit(&repo, &commit_req("second"), &mut NullProgress).unwrap();

        fs::write(
            repo.git_dir().join("MERGE_HEAD"),
            format!("{}\n", first.oid.to_hex()),
        )
        .unwrap();
        fs::write(root.join("a.txt"), b"merged\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let merge = create_commit(&repo, &commit_req("merge"), &mut NullProgress).unwrap();
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
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        create_commit(&repo, &commit_req("first"), &mut NullProgress).unwrap();
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
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        create_commit(&repo, &commit_req("first"), &mut NullProgress).unwrap();
        fs::write(root.join("a.txt"), b"2\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        let mut req = dry_run_req("amend");
        req.amend = true;
        req.author = "Override <o@example.com> 1700000000 +0000".into();
        let outcome =
            commit_with_scope(&repo, &req, CommitScope::Staged, &mut NullProgress).unwrap();
        assert_eq!(outcome.author.email, "o@example.com");
        assert_eq!(outcome.author.name, "Override");
    }
}
