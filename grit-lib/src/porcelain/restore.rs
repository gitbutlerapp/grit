//! Restore working tree and/or index paths from the index, `HEAD`, or another tree
//! (`git restore` semantics).

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::Path;
use std::sync::Arc;

use crate::diff::{
    entry_is_racy, hash_worktree_file, index_mtime_for_diff,
    refresh_index_stat_content_verified_with_rules, stat_matches,
};
use crate::error::{Error, Result};
use crate::index::index_file_mtime;
use crate::index::{entry_from_metadata, Index, IndexEntry, MODE_EXECUTABLE, MODE_GITLINK};
use crate::merge_file::{merge, ConflictStyle, MergeFavor, MergeInput};
use crate::objects::ObjectKind;
use crate::objects::{parse_commit, ObjectId};
use crate::pathspec::{
    pathspec_is_exclude, sorted_paths_match_pathspec, PathspecListMatcher, PathspecMatchContext,
};
use crate::porcelain::checkout::{
    remove_empty_parent_dirs, worktree_bytes_from_index_blob, write_smudged_blob_to_worktree,
    write_to_worktree_cached, LeadingDirCache,
};
use crate::porcelain::stash::{flat_tree_lookup, flatten_tree_full, FlatTreeEntry};
use crate::repo::Repository;
use crate::state::{resolve_head, HeadState};
use crate::worktree_rules::WorktreeRules;

/// Unmerged index side to check out into the worktree (`git restore --ours|--theirs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictSide {
    /// Index stage 2 (current branch / HEAD side of the conflict).
    Ours,
    /// Index stage 3 (incoming side of the conflict).
    Theirs,
}

/// Where explicit `--source` content comes from; default targets use the index or `HEAD`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreSource {
    /// Restore the working tree from the index (default when `--source` is omitted).
    Index,
    /// Restore from this tree object ( `--source=<rev>` ).
    Tree(ObjectId),
}

/// Options for [`restore_paths`], translated from the `grit restore` CLI.
#[derive(Debug, Clone)]
pub struct RestoreOptions {
    /// Resolved pathspec patterns (may differ from user input after cwd-relative resolution).
    pub pathspecs: Vec<String>,
    /// Original pathspec strings for errors (defaults to `pathspecs` when empty).
    pub pathspec_sources: Vec<String>,
    /// Explicit `--source` tree, when provided.
    pub source: RestoreSource,
    /// Restore the index (`--staged`).
    pub staged: bool,
    /// Restore the working tree (`--worktree`, or implied when `--staged` is absent).
    pub worktree: bool,
    /// For unmerged paths, check out stage 2 or 3 into the worktree only.
    pub stage: Option<ConflictSide>,
    /// Rebuild conflict markers in the worktree from unmerged index stages (`--merge`).
    pub remerge: Option<ConflictStyle>,
}

/// Outcome of a restore operation: paths written and paths removed in each target.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreOutcome {
    /// Paths whose working tree file was created or updated.
    pub restored: Vec<String>,
    /// Paths removed from the working tree because the source had no entry.
    pub removed: Vec<String>,
}

/// Restore paths matching `opts.pathspecs` in the index and/or working tree.
///
/// Follows `git restore` rules: default restores the worktree from the index;
/// `--staged` restores the index from `HEAD` (or `--source`); `--source` alone
/// restores the worktree from that tree; both flags restore index then worktree.
/// Paths missing in the source are removed from the target. Untracked files are
/// never modified. Stat data is refreshed for restored index entries.
///
/// # Errors
///
/// Returns [`Error::PathspecNoMatch`] when a positive pathspec matches nothing,
/// or I/O / object-store failures while reading trees and blobs.
pub fn restore_paths(repo: &Repository, opts: &RestoreOptions) -> Result<RestoreOutcome> {
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;

    if opts.pathspecs.is_empty() {
        return Err(Error::Message("you must specify path(s) to restore".into()));
    }

    if opts.stage.is_some() || opts.remerge.is_some() {
        validate_conflict_restore_options(opts)?;
        return restore_conflict_paths(repo, work_tree, opts);
    }

    let mut index = repo.load_index()?;
    let protect = crate::path_validate::ProtectOptions::from_config(&*repo.config()?);

    let restore_staged = opts.staged;
    let restore_worktree = opts.worktree || !opts.staged;
    let worktree_from_tree = matches!(opts.source, RestoreSource::Tree(_)) && !restore_staged;

    let explicit_source_entries = match opts.source {
        RestoreSource::Index => None,
        RestoreSource::Tree(oid) => Some(flatten_tree_full(repo.odb(), &oid, "", protect)?),
    };

    let head_entries = if needs_head_tree_entries(restore_staged, opts.source) {
        match resolve_head_tree(repo)? {
            Some(tree) => flatten_tree_full(repo.odb(), &tree, "", protect)?,
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };

    let select_all_tracked = pathspecs_select_all_tracked(&opts.pathspecs);
    let pathspec_ctx = repo.pathspec_match_context();
    let matched = collect_matched_paths(
        &index,
        head_entries.as_slice(),
        explicit_source_entries.as_deref(),
        &opts.pathspecs,
        restore_staged,
        worktree_from_tree,
        opts.source,
        select_all_tracked,
        pathspec_ctx,
    );
    validate_pathspecs(opts, &matched, &index, select_all_tracked, pathspec_ctx)?;

    let index_source_entries = index_source_tree(
        opts,
        restore_staged,
        head_entries.as_slice(),
        explicit_source_entries.as_deref(),
    );

    let mut outcome = RestoreOutcome::default();
    let mut index_dirty = false;

    let tracked_before_index_restore: HashSet<Vec<u8>> = if restore_worktree {
        matched
            .iter()
            .filter(|path| index.get(path.as_bytes(), 0).is_some())
            .map(|path| path.as_bytes().to_vec())
            .collect()
    } else {
        HashSet::new()
    };

    if restore_staged {
        apply_index_restore(
            repo,
            index_source_entries,
            &matched,
            &mut index,
            &mut outcome,
            &mut index_dirty,
        )?;
    }

    if restore_worktree {
        let worktree_from_tree = matches!(opts.source, RestoreSource::Tree(_)) && !restore_staged;
        apply_worktree_restore(WorktreeRestoreContext {
            repo,
            work_tree,
            worktree_from_tree,
            tree_source: explicit_source_entries.as_deref(),
            pathspecs: &opts.pathspecs,
            matched: &matched,
            tracked_for_worktree_deletion: &tracked_before_index_restore,
            index_out: &mut index,
            outcome: &mut outcome,
            index_dirty: &mut index_dirty,
        })?;
    }

    if restore_staged || restore_worktree {
        if restore_staged && !restore_worktree {
            refresh_restored_index_stats(repo, work_tree, &mut index, &outcome)?;
        }
        if index_dirty {
            repo.write_index(&mut index)?;
        }
    }

    if !outcome.restored.is_empty() {
        outcome.restored.sort();
        outcome.restored.dedup();
    }
    if !outcome.removed.is_empty() {
        outcome.removed.sort();
        outcome.removed.dedup();
    }
    Ok(outcome)
}

fn validate_conflict_restore_options(opts: &RestoreOptions) -> Result<()> {
    if opts.staged {
        return Err(Error::RestoreConflictWithStaged);
    }
    if !matches!(opts.source, RestoreSource::Index) {
        return Err(Error::RestoreConflictWithSource);
    }
    if opts.stage.is_some() && opts.remerge.is_some() {
        return Err(Error::Message(
            "cannot combine --ours/--theirs with --merge".into(),
        ));
    }
    Ok(())
}

fn restore_conflict_paths(
    repo: &Repository,
    work_tree: &Path,
    opts: &RestoreOptions,
) -> Result<RestoreOutcome> {
    let index = repo.load_index()?;
    let pathspec_ctx = repo.pathspec_match_context();
    let include_resolved_undo = opts.remerge.is_some();
    let matched = collect_conflict_matched_paths(
        &index,
        &opts.pathspecs,
        include_resolved_undo,
        pathspec_ctx,
    );
    validate_unmerged_pathspecs(opts, &matched, pathspec_ctx)?;

    let mut outcome = RestoreOutcome::default();
    let rules = WorktreeRules::from_repository(repo, &index)?;

    if let Some(side) = opts.stage {
        for path in &matched {
            restore_conflict_side(repo, work_tree, &index, path, side, &rules, &mut outcome)?;
        }
        return Ok(outcome);
    }

    let style = opts
        .remerge
        .unwrap_or_else(|| conflict_style_from_config(repo));
    let mut index = index;
    let mut index_dirty = false;
    for path in &matched {
        restore_conflict_remerge(
            repo,
            work_tree,
            &mut index,
            path,
            style,
            &rules,
            &mut outcome,
            &mut index_dirty,
        )?;
    }
    if index_dirty {
        repo.write_index(&mut index)?;
    }
    Ok(outcome)
}

fn conflict_style_from_config(repo: &Repository) -> ConflictStyle {
    let Ok(config) = repo.config() else {
        return ConflictStyle::Merge;
    };
    let Some(value) = config.get("merge.conflictStyle") else {
        return ConflictStyle::Merge;
    };
    match value.as_str() {
        "diff3" => ConflictStyle::Diff3,
        "zdiff3" => ConflictStyle::ZealousDiff3,
        _ => ConflictStyle::Merge,
    }
}

fn collect_conflict_matched_paths(
    index: &Index,
    pathspecs: &[String],
    include_resolved_undo: bool,
    pathspec_ctx: PathspecMatchContext,
) -> BTreeSet<String> {
    let matcher = PathspecListMatcher::new(pathspecs, pathspec_ctx);
    let mut paths = BTreeSet::new();
    for entry in &index.entries {
        if entry.stage() == 0 {
            continue;
        }
        let path = String::from_utf8_lossy(&entry.path);
        if matcher.matches(&path) {
            paths.insert(path.into_owned());
        }
    }
    if include_resolved_undo {
        if let Some(undo) = &index.resolve_undo {
            for path_bytes in undo.keys() {
                let path = String::from_utf8_lossy(path_bytes);
                if matcher.matches(&path) {
                    paths.insert(path.into_owned());
                }
            }
        }
    }
    paths
}

fn validate_unmerged_pathspecs(
    opts: &RestoreOptions,
    matched: &BTreeSet<String>,
    pathspec_ctx: PathspecMatchContext,
) -> Result<()> {
    let sources = if opts.pathspec_sources.is_empty() {
        &opts.pathspecs
    } else {
        &opts.pathspec_sources
    };
    let matched_sorted: Vec<&str> = matched.iter().map(String::as_str).collect();
    for (label, resolved) in sources.iter().zip(opts.pathspecs.iter()) {
        if pathspec_is_exclude(label) {
            continue;
        }
        if !sorted_paths_match_pathspec(resolved, &matched_sorted, pathspec_ctx) {
            return Err(Error::PathspecNoMatch {
                spec: label.clone(),
            });
        }
    }
    Ok(())
}

fn restore_conflict_side(
    repo: &Repository,
    work_tree: &Path,
    index: &Index,
    path: &str,
    side: ConflictSide,
    rules: &WorktreeRules,
    outcome: &mut RestoreOutcome,
) -> Result<()> {
    let stage = match side {
        ConflictSide::Ours => 2u8,
        ConflictSide::Theirs => 3u8,
    };
    let path_bytes = path.as_bytes();
    let Some(entry) = index.get(path_bytes, stage) else {
        return Err(match side {
            ConflictSide::Ours => Error::RestoreConflictMissingOurs {
                path: path.to_string(),
            },
            ConflictSide::Theirs => Error::RestoreConflictMissingTheirs {
                path: path.to_string(),
            },
        });
    };
    if entry.mode == MODE_GITLINK {
        return Ok(());
    }
    write_smudged_blob_to_worktree(repo, work_tree, rules, path, &entry.oid, entry.mode)?;
    outcome.restored.push(path.to_string());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn restore_conflict_remerge(
    repo: &Repository,
    work_tree: &Path,
    index: &mut Index,
    path: &str,
    style: ConflictStyle,
    rules: &WorktreeRules,
    outcome: &mut RestoreOutcome,
    index_dirty: &mut bool,
) -> Result<()> {
    let path_bytes = path.as_bytes();
    if index.get(path_bytes, 0).is_some()
        && !path_has_unmerged(index, path_bytes)
        && index.unmerge_path_from_resolve_undo(path_bytes)
    {
        *index_dirty = true;
    }
    let (Some(base), Some(ours), Some(theirs)) = (
        index.get(path_bytes, 1),
        index.get(path_bytes, 2),
        index.get(path_bytes, 3),
    ) else {
        return Err(Error::RestoreConflictNotUnmerged {
            path: path.to_string(),
        });
    };
    if base.mode == MODE_GITLINK || ours.mode == MODE_GITLINK || theirs.mode == MODE_GITLINK {
        return Ok(());
    }
    let base_blob = repo.odb().read(&base.oid)?;
    let ours_blob = repo.odb().read(&ours.oid)?;
    let theirs_blob = repo.odb().read(&theirs.oid)?;
    let input = MergeInput {
        base: &base_blob.data,
        ours: &ours_blob.data,
        theirs: &theirs_blob.data,
        label_ours: "ours",
        label_base: "base",
        label_theirs: "theirs",
        favor: MergeFavor::None,
        style,
        marker_size: 7,
        diff_algorithm: None,
        ignore_all_space: false,
        ignore_space_change: false,
        ignore_space_at_eol: false,
        ignore_cr_at_eol: false,
    };
    let merged = merge(&input)?;
    let mode = conflict_worktree_mode(ours.mode, theirs.mode);
    let merged_oid = repo.odb().write(ObjectKind::Blob, &merged.content)?;
    write_smudged_blob_to_worktree(repo, work_tree, rules, path, &merged_oid, mode)?;
    outcome.restored.push(path.to_string());
    Ok(())
}

fn path_has_unmerged(index: &Index, path: &[u8]) -> bool {
    index
        .entries
        .iter()
        .any(|e| e.path == path && e.stage() != 0)
}

fn conflict_worktree_mode(ours_mode: u32, theirs_mode: u32) -> u32 {
    if ours_mode == theirs_mode {
        ours_mode
    } else if matches!(ours_mode, 0o100644 | 0o100755) && matches!(theirs_mode, 0o100644 | 0o100755)
    {
        0o100644
    } else {
        ours_mode
    }
}

fn pathspecs_select_all_tracked(pathspecs: &[String]) -> bool {
    pathspecs.iter().any(|s| matches!(s.as_str(), "." | ":/."))
        && !pathspecs.iter().any(|s| pathspec_is_exclude(s))
}

fn resolve_head_tree(repo: &Repository) -> Result<Option<ObjectId>> {
    match resolve_head(repo.git_dir())? {
        HeadState::Branch { oid: Some(oid), .. } | HeadState::Detached { oid } => {
            let obj = repo.odb().read(&oid)?;
            let commit = parse_commit(&obj.data)?;
            Ok(Some(commit.tree))
        }
        HeadState::Branch { .. } => Ok(None),
        HeadState::Invalid => Err(Error::Message("HEAD is in an unknown state".into())),
    }
}

fn needs_head_tree_entries(restore_staged: bool, source: RestoreSource) -> bool {
    restore_staged && matches!(source, RestoreSource::Index)
}

#[allow(clippy::too_many_arguments)]
fn collect_matched_paths(
    index: &Index,
    head_entries: &[FlatTreeEntry],
    source_entries: Option<&[FlatTreeEntry]>,
    pathspecs: &[String],
    restore_staged: bool,
    worktree_from_tree: bool,
    source: RestoreSource,
    select_all_tracked: bool,
    pathspec_ctx: PathspecMatchContext,
) -> BTreeSet<String> {
    let matcher = PathspecListMatcher::new(pathspecs, pathspec_ctx);
    let mut paths = BTreeSet::new();
    let skip_index_union = select_all_tracked && !restore_staged && !worktree_from_tree;
    if !skip_index_union {
        for entry in &index.entries {
            if entry.stage() != 0 {
                continue;
            }
            let path = String::from_utf8_lossy(&entry.path);
            if matcher.matches(&path) {
                paths.insert(path.into_owned());
            }
        }
    }
    if restore_staged {
        let staged_source = match source {
            RestoreSource::Index => head_entries,
            RestoreSource::Tree(_) => source_entries.unwrap_or(&[]),
        };
        for entry in staged_source {
            if matcher.matches(&entry.path) {
                paths.insert(entry.path.clone());
            }
        }
    }
    if worktree_from_tree {
        if let Some(src) = source_entries {
            for entry in src {
                if matcher.matches(&entry.path) {
                    paths.insert(entry.path.clone());
                }
            }
        }
    }
    paths
}

fn validate_pathspecs(
    opts: &RestoreOptions,
    matched: &BTreeSet<String>,
    index: &Index,
    select_all_tracked: bool,
    pathspec_ctx: PathspecMatchContext,
) -> Result<()> {
    let sources = if opts.pathspec_sources.is_empty() {
        &opts.pathspecs
    } else {
        &opts.pathspec_sources
    };
    let matched_sorted: Vec<&str> = matched.iter().map(String::as_str).collect();
    for (label, resolved) in sources.iter().zip(opts.pathspecs.iter()) {
        if pathspec_is_exclude(label) {
            continue;
        }
        let hits = if select_all_tracked && matches!(resolved.as_str(), "." | ":/.") {
            index.entries.iter().any(|e| e.stage() == 0)
        } else {
            sorted_paths_match_pathspec(resolved, &matched_sorted, pathspec_ctx)
        };
        if !hits {
            return Err(Error::PathspecNoMatch {
                spec: label.clone(),
            });
        }
    }
    Ok(())
}

fn index_source_tree<'a>(
    opts: &RestoreOptions,
    restore_staged: bool,
    head_entries: &'a [FlatTreeEntry],
    explicit_source: Option<&'a [FlatTreeEntry]>,
) -> &'a [FlatTreeEntry] {
    if !restore_staged {
        return head_entries;
    }
    match opts.source {
        RestoreSource::Tree(_) => explicit_source.unwrap_or(head_entries),
        RestoreSource::Index => head_entries,
    }
}

fn apply_index_restore(
    repo: &Repository,
    source_entries: &[FlatTreeEntry],
    matched: &BTreeSet<String>,
    index: &mut Index,
    outcome: &mut RestoreOutcome,
    index_dirty: &mut bool,
) -> Result<()> {
    for path in matched {
        let path_bytes = path.as_bytes();
        if let Some(src) = flat_tree_lookup(source_entries, path) {
            if index
                .get(path_bytes, 0)
                .is_some_and(|e| e.oid == src.oid && e.mode == src.mode)
            {
                continue;
            }
            let size = blob_index_size(repo, src)?;
            index.stage_file(index_entry_from_flat(path, src, size));
            outcome.restored.push(path.clone());
            *index_dirty = true;
        } else if index.get(path_bytes, 0).is_some() {
            index.remove(path_bytes);
            outcome.removed.push(path.clone());
            *index_dirty = true;
        }
    }
    Ok(())
}

struct WorktreeRestoreContext<'a> {
    repo: &'a Repository,
    work_tree: &'a Path,
    worktree_from_tree: bool,
    tree_source: Option<&'a [FlatTreeEntry]>,
    pathspecs: &'a [String],
    matched: &'a BTreeSet<String>,
    tracked_for_worktree_deletion: &'a HashSet<Vec<u8>>,
    index_out: &'a mut Index,
    outcome: &'a mut RestoreOutcome,
    index_dirty: &'a mut bool,
}

fn apply_worktree_restore(ctx: WorktreeRestoreContext<'_>) -> Result<()> {
    let WorktreeRestoreContext {
        repo,
        work_tree,
        worktree_from_tree,
        tree_source,
        pathspecs,
        matched,
        tracked_for_worktree_deletion,
        index_out,
        outcome,
        index_dirty,
    } = ctx;
    let config = repo.config()?;
    let protect = crate::path_validate::ProtectOptions::from_config(&config);
    let mut dir_cache = LeadingDirCache::with_protect(protect);

    if !worktree_from_tree {
        let attr_index = index_out.gitattributes_snapshot();
        let rules = WorktreeRules::from_parts(repo, &attr_index, Arc::clone(&config))?;
        let index_mtime = index_mtime_for_diff(index_out, None);
        let matcher = PathspecListMatcher::new(pathspecs, repo.pathspec_match_context());
        let mut to_write: Vec<usize> = Vec::new();
        for (pos, entry) in index_out.entries.iter().enumerate() {
            if entry.stage() != 0 {
                continue;
            }
            let path = crate::repo_relpath::storage_string(&entry.path);
            if !matcher.matches(&path) {
                continue;
            }
            if entry.mode == MODE_GITLINK {
                continue;
            }
            if worktree_index_entry_up_to_date(repo, work_tree, entry, &path, index_mtime, &rules)?
            {
                continue;
            }
            to_write.push(pos);
        }
        if !to_write.is_empty() {
            outcome.restored.reserve(to_write.len());
        }
        for pos in to_write {
            let path = crate::repo_relpath::storage_string(&index_out.entries[pos].path);
            let oid = index_out.entries[pos].oid;
            let mode = index_out.entries[pos].mode;
            restore_worktree_blob_from_index(
                repo,
                work_tree,
                &path,
                oid,
                mode,
                &rules,
                &mut dir_cache,
                index_out,
                outcome,
                index_dirty,
            )?;
        }
        for path in matched {
            if index_out.get(path.as_bytes(), 0).is_some() {
                continue;
            }
            if !tracked_for_worktree_deletion.contains(path.as_bytes()) {
                continue;
            }
            let abs = work_tree.join(path);
            let Ok(meta) = fs::symlink_metadata(&abs) else {
                continue;
            };
            if meta.is_dir() && !meta.file_type().is_symlink() {
                continue;
            }
            if fs::remove_file(&abs).is_ok() {
                remove_empty_parent_dirs(work_tree, &abs);
                outcome.removed.push(path.clone());
            }
        }
        return Ok(());
    }

    let rules = WorktreeRules::from_repository(repo, index_out)?;
    for path in matched {
        let src = tree_source
            .and_then(|entries| flat_tree_lookup(entries, path))
            .cloned();
        let abs = work_tree.join(path);
        match src {
            Some(entry) => {
                let existing = index_out.get(path.as_bytes(), 0).cloned();
                restore_worktree_entry(
                    repo,
                    work_tree,
                    path,
                    &entry,
                    existing.as_ref(),
                    true,
                    &rules,
                    &mut dir_cache,
                    index_out,
                    outcome,
                    index_dirty,
                )?;
            }
            None => {
                if !tracked_for_worktree_deletion.contains(path.as_bytes()) {
                    continue;
                }
                let Ok(meta) = fs::symlink_metadata(&abs) else {
                    continue;
                };
                if meta.is_dir() && !meta.file_type().is_symlink() {
                    continue;
                }
                if fs::remove_file(&abs).is_ok() {
                    remove_empty_parent_dirs(work_tree, &abs);
                    outcome.removed.push(path.clone());
                }
            }
        }
    }

    Ok(())
}

fn worktree_index_entry_up_to_date(
    repo: &Repository,
    work_tree: &Path,
    entry: &IndexEntry,
    path: &str,
    index_mtime: Option<(u32, u32)>,
    rules: &WorktreeRules,
) -> Result<bool> {
    if entry.mode != crate::index::MODE_REGULAR && entry.mode != MODE_EXECUTABLE {
        return Ok(false);
    }
    let abs_path = crate::repo_relpath::worktree_abs(work_tree, path);
    let meta = match fs::symlink_metadata(&abs_path) {
        Ok(m) => m,
        Err(_) => return Ok(false),
    };
    if !meta.is_file() || !stat_matches(entry, &meta) {
        return Ok(false);
    }
    let trust_filemode = repo
        .config()?
        .get_bool("core.filemode")
        .and_then(|r| r.ok())
        .unwrap_or(true);
    if trust_filemode && crate::diff::mode_from_metadata(&meta) != entry.mode {
        return Ok(false);
    }
    if !entry_is_racy(entry, index_mtime) {
        return Ok(true);
    }
    let wt_oid = hash_worktree_file(
        repo.odb(),
        &abs_path,
        &meta,
        rules.conversion(),
        &rules.file_attrs(path, false),
        path,
        Some(entry),
        Some(rules.filter_process()),
    )?;
    Ok(wt_oid == entry.oid)
}

#[allow(clippy::too_many_arguments)]
fn restore_worktree_blob_from_index(
    repo: &Repository,
    work_tree: &Path,
    path: &str,
    oid: ObjectId,
    mode: u32,
    rules: &WorktreeRules,
    dir_cache: &mut LeadingDirCache,
    index_out: &mut Index,
    outcome: &mut RestoreOutcome,
    index_dirty: &mut bool,
) -> Result<()> {
    if mode == MODE_GITLINK {
        return Ok(());
    }
    let object = repo.odb().read(&oid)?;
    let worktree_bytes =
        worktree_bytes_from_index_blob(repo, rules, path, mode, &object.data, &oid, None, None)?;
    let Some(bytes) = worktree_bytes else {
        return Err(Error::Message(format!(
            "filter delayed checkout for '{path}' is not supported by grit restore"
        )));
    };
    write_to_worktree_cached(work_tree, path, &bytes, mode, dir_cache)?;
    let abs = crate::repo_relpath::worktree_abs(work_tree, path);
    let meta = fs::symlink_metadata(&abs).map_err(Error::Io)?;
    let index_path = crate::repo_relpath::storage_bytes(path);
    index_out.add_or_replace(entry_from_metadata(&meta, index_path.as_ref(), oid, mode));
    *index_dirty = true;
    outcome.restored.push(path.to_string());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn restore_worktree_entry(
    repo: &Repository,
    work_tree: &Path,
    path: &str,
    entry: &FlatTreeEntry,
    existing_index: Option<&IndexEntry>,
    worktree_from_tree: bool,
    rules: &WorktreeRules,
    dir_cache: &mut LeadingDirCache,
    index_out: &mut Index,
    outcome: &mut RestoreOutcome,
    index_dirty: &mut bool,
) -> Result<()> {
    if entry.mode == MODE_GITLINK {
        return Ok(());
    }
    let abs = work_tree.join(path);
    let object = repo.odb().read(&entry.oid)?;
    let worktree_bytes = worktree_bytes_from_index_blob(
        repo,
        rules,
        path,
        entry.mode,
        &object.data,
        &entry.oid,
        None,
        None,
    )?;
    let Some(bytes) = worktree_bytes else {
        return Err(Error::Message(format!(
            "filter delayed checkout for '{path}' is not supported by grit restore"
        )));
    };
    write_to_worktree_cached(work_tree, path, &bytes, entry.mode, dir_cache)?;
    let meta = fs::symlink_metadata(&abs).map_err(Error::Io)?;
    if worktree_from_tree {
        if let Some(existing) = existing_index {
            index_out.add_or_replace(entry_from_metadata(
                &meta,
                path.as_bytes(),
                existing.oid,
                existing.mode,
            ));
            *index_dirty = true;
        }
    } else {
        index_out.add_or_replace(entry_from_metadata(
            &meta,
            path.as_bytes(),
            entry.oid,
            entry.mode,
        ));
        *index_dirty = true;
    }
    outcome.restored.push(path.to_string());
    Ok(())
}

fn index_entry_from_flat(path: &str, src: &FlatTreeEntry, size: u32) -> IndexEntry {
    IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: src.mode,
        uid: 0,
        gid: 0,
        size,
        oid: src.oid,
        flags: path.len().min(0xFFF) as u16,
        flags_extended: None,
        path: path.as_bytes().to_vec(),
        base_index_pos: 0,
    }
}
fn refresh_restored_index_stats(
    repo: &Repository,
    work_tree: &Path,
    index: &mut Index,
    outcome: &RestoreOutcome,
) -> Result<()> {
    let touched: BTreeSet<&str> = outcome
        .restored
        .iter()
        .chain(outcome.removed.iter())
        .map(String::as_str)
        .collect();
    if touched.is_empty() {
        return Ok(());
    }
    let index_mtime = index_file_mtime(&repo.index_path());
    let rules = WorktreeRules::from_repository(repo, index).ok();
    let rules_arc = rules.map(|r| std::sync::Arc::new(std::sync::Mutex::new(r)));
    let mut subset = index.clone();
    subset
        .entries
        .retain(|e| e.stage() == 0 && touched.contains(String::from_utf8_lossy(&e.path).as_ref()));
    if subset.entries.is_empty() {
        return Ok(());
    }
    let _ = refresh_index_stat_content_verified_with_rules(
        repo.odb(),
        repo.git_dir(),
        &mut subset,
        work_tree,
        index_mtime,
        repo.config().ok().as_deref(),
        None,
        rules_arc.as_ref(),
    )?;
    for entry in subset.entries {
        index.stage_file(entry);
    }
    Ok(())
}

fn blob_index_size(repo: &Repository, entry: &FlatTreeEntry) -> Result<u32> {
    if entry.mode == MODE_GITLINK {
        return Ok(0);
    }
    let obj = repo.odb().read(&entry.oid)?;
    Ok(obj.data.len() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::{serialize_commit, CommitData, ObjectKind};
    use crate::porcelain::status::{status, StatusOptions};
    use crate::progress::NullProgress;
    use crate::repo::{init_repository, Repository};
    use crate::write_tree::{write_tree_update_index, WriteTreeFlags};
    use crate::RefStorageFormat;
    use tempfile::TempDir;

    fn init_repo() -> (TempDir, Repository) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(dir.path(), false, "main", None, RefStorageFormat::Files)
            .expect("init");
        (dir, repo)
    }

    #[cfg(unix)]
    #[test]
    fn restore_worktree_reverts_exec_bit_only_changes() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, repo) = init_repo();
        let wt = repo.work_tree().expect("wt");
        std::fs::write(wt.join("f"), "data\n").expect("write");
        let mut index = repo.load_index().expect("index");
        let oid = repo.odb().write(ObjectKind::Blob, b"data\n").expect("blob");
        let entry =
            crate::index::entry_from_stat(&wt.join("f"), b"f", oid, crate::index::MODE_REGULAR)
                .expect("entry");
        index.add_or_replace(entry);
        index.sort();
        let tree = write_tree_update_index(&repo.odb(), &mut index, "", WriteTreeFlags::silent())
            .expect("tree");
        repo.write_index(&mut index).expect("write index");
        let commit = CommitData {
            tree,
            parents: vec![],
            author: "T <t@t> 0 +0000".into(),
            committer: "T <t@t> 0 +0000".into(),
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: "c\n".into(),
            raw_message: None,
            extra_headers: Vec::new(),
        };
        let co = repo
            .odb()
            .write(ObjectKind::Commit, &serialize_commit(&commit))
            .expect("commit");
        crate::refs::write_ref(&repo.git_dir(), "refs/heads/main", &co).expect("branch");
        crate::refs::write_symbolic_ref(&repo.git_dir(), "HEAD", "refs/heads/main")
            .expect("symhead");

        let mut perms = std::fs::metadata(wt.join("f")).expect("meta").permissions();
        perms.set_mode(0o100755);
        std::fs::set_permissions(wt.join("f"), perms).expect("chmod");

        restore_paths(
            &repo,
            &RestoreOptions {
                pathspecs: vec!["f".into()],
                pathspec_sources: vec!["f".into()],
                source: RestoreSource::Index,
                staged: false,
                worktree: true,
                stage: None,
                remerge: None,
            },
        )
        .expect("restore");

        assert_eq!(
            std::fs::metadata(wt.join("f"))
                .expect("meta")
                .permissions()
                .mode()
                & 0o111,
            0
        );
        let st = status(&repo, &StatusOptions::default(), &mut NullProgress).expect("status");
        assert!(st.unstaged.is_empty(), "{st:?}");
    }
}
