//! Restore working tree and/or index paths from the index, `HEAD`, or another tree
//! (`git restore` semantics).

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::Path;

use crate::diff::refresh_index_stat_content_verified_with_rules;
use crate::error::{Error, Result};
use crate::index::index_file_mtime;
use crate::index::{entry_from_metadata, Index, IndexEntry, MODE_GITLINK};
use crate::objects::{parse_commit, ObjectId};
use crate::pathspec::{matches_pathspec_list, pathspec_is_exclude};
use crate::porcelain::checkout::{
    remove_empty_parent_dirs, worktree_bytes_from_index_blob, write_to_worktree_cached,
    LeadingDirCache,
};
use crate::porcelain::stash::{flat_tree_lookup, flatten_tree_full, FlatTreeEntry};
use crate::repo::Repository;
use crate::state::{resolve_head, HeadState};
use crate::worktree_rules::WorktreeRules;

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
        .work_tree
        .as_deref()
        .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;

    if opts.pathspecs.is_empty() {
        return Err(Error::Message("you must specify path(s) to restore".into()));
    }

    let mut index = repo.load_index()?;
    let head_tree_oid = resolve_head_tree(repo)?;
    let head_entries = if let Some(tree) = head_tree_oid {
        flatten_tree_full(&repo.odb, &tree, "")?
    } else {
        Vec::new()
    };

    let explicit_source_entries = match opts.source {
        RestoreSource::Index => None,
        RestoreSource::Tree(oid) => Some(flatten_tree_full(&repo.odb, &oid, "")?),
    };

    let restore_staged = opts.staged;
    let restore_worktree = opts.worktree || !opts.staged;

    let matched = collect_matched_paths(
        &index,
        &head_entries,
        explicit_source_entries.as_deref(),
        &opts.pathspecs,
    );
    validate_pathspecs(opts, &matched)?;

    let index_source_entries = index_source_tree(
        opts,
        restore_staged,
        &head_entries,
        explicit_source_entries.as_deref(),
    );

    let mut outcome = RestoreOutcome::default();

    let tracked_before_index_restore: HashSet<Vec<u8>> = matched
        .iter()
        .filter(|path| index.get(path.as_bytes(), 0).is_some())
        .map(|path| path.as_bytes().to_vec())
        .collect();

    if restore_staged {
        apply_index_restore(
            repo,
            index_source_entries,
            &matched,
            &mut index,
            &mut outcome,
        )?;
    }

    if restore_worktree {
        let worktree_from_tree = matches!(opts.source, RestoreSource::Tree(_)) && !restore_staged;
        apply_worktree_restore(
            repo,
            work_tree,
            worktree_from_tree,
            explicit_source_entries.as_deref(),
            &matched,
            &tracked_before_index_restore,
            &mut index,
            &mut outcome,
        )?;
    }

    if restore_staged || restore_worktree {
        if restore_staged && !restore_worktree {
            refresh_restored_index_stats(repo, work_tree, &mut index, &outcome)?;
        }
        repo.write_index(&mut index)?;
    }

    outcome.restored.sort();
    outcome.removed.sort();
    outcome.restored.dedup();
    outcome.removed.dedup();
    Ok(outcome)
}

fn resolve_head_tree(repo: &Repository) -> Result<Option<ObjectId>> {
    match resolve_head(&repo.git_dir)? {
        HeadState::Branch { oid: Some(oid), .. } | HeadState::Detached { oid } => {
            let obj = repo.odb.read(&oid)?;
            let commit = parse_commit(&obj.data)?;
            Ok(Some(commit.tree))
        }
        HeadState::Branch { .. } => Ok(None),
        HeadState::Invalid => Err(Error::Message("HEAD is in an unknown state".into())),
    }
}

fn collect_matched_paths(
    index: &Index,
    head_entries: &[FlatTreeEntry],
    source_entries: Option<&[FlatTreeEntry]>,
    pathspecs: &[String],
) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    for entry in &index.entries {
        if entry.stage() != 0 {
            continue;
        }
        let path = String::from_utf8_lossy(&entry.path);
        if matches_pathspec_list(&path, pathspecs) {
            paths.insert(path.into_owned());
        }
    }
    for entry in head_entries {
        if matches_pathspec_list(&entry.path, pathspecs) {
            paths.insert(entry.path.clone());
        }
    }
    if let Some(src) = source_entries {
        for entry in src {
            if matches_pathspec_list(&entry.path, pathspecs) {
                paths.insert(entry.path.clone());
            }
        }
    }
    paths
}

fn validate_pathspecs(opts: &RestoreOptions, matched: &BTreeSet<String>) -> Result<()> {
    let sources = if opts.pathspec_sources.is_empty() {
        &opts.pathspecs
    } else {
        &opts.pathspec_sources
    };
    for (label, resolved) in sources.iter().zip(opts.pathspecs.iter()) {
        if pathspec_is_exclude(label) {
            continue;
        }
        let hits = matched
            .iter()
            .any(|p| matches_pathspec_list(p, std::slice::from_ref(resolved)));
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
        } else if index.get(path_bytes, 0).is_some() {
            index.remove(path_bytes);
            outcome.removed.push(path.clone());
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn apply_worktree_restore(
    repo: &Repository,
    work_tree: &Path,
    worktree_from_tree: bool,
    tree_source: Option<&[FlatTreeEntry]>,
    matched: &BTreeSet<String>,
    tracked_for_worktree_deletion: &HashSet<Vec<u8>>,
    index_out: &mut Index,
    outcome: &mut RestoreOutcome,
) -> Result<()> {
    let rules = WorktreeRules::from_repository(repo, index_out)?;
    let mut dir_cache = LeadingDirCache::new();

    for path in matched {
        let src = if worktree_from_tree {
            tree_source
                .and_then(|entries| flat_tree_lookup(entries, path))
                .cloned()
        } else {
            index_out.get(path.as_bytes(), 0).map(index_entry_as_flat)
        };

        let abs = work_tree.join(path);
        match src {
            Some(entry) => {
                if entry.mode == MODE_GITLINK {
                    continue;
                }
                if !worktree_from_tree {
                    if let Some(existing) = index_out.get(path.as_bytes(), 0) {
                        if crate::diff::path_checkout_skip_blob_write_when_up_to_date(
                            &repo.odb,
                            &repo.git_dir,
                            work_tree,
                            index_out,
                            existing,
                            &entry.oid,
                            entry.mode,
                            path,
                            Some(rules.filter_process()),
                        )? {
                            continue;
                        }
                    }
                }
                let object = repo.odb.read(&entry.oid)?;
                let worktree_bytes = worktree_bytes_from_index_blob(
                    repo,
                    &rules,
                    path,
                    entry.mode,
                    &object.data,
                    &entry.oid,
                    None,
                )?;
                let Some(bytes) = worktree_bytes else {
                    return Err(Error::Message(format!(
                        "filter delayed checkout for '{path}' is not supported by grit restore"
                    )));
                };
                write_to_worktree_cached(work_tree, path, &bytes, entry.mode, &mut dir_cache)?;
                let meta = fs::symlink_metadata(&abs).map_err(Error::Io)?;
                if worktree_from_tree {
                    if let Some(existing) = index_out.get(path.as_bytes(), 0) {
                        index_out.stage_file(entry_from_metadata(
                            &meta,
                            path.as_bytes(),
                            existing.oid,
                            existing.mode,
                        ));
                    }
                } else {
                    index_out.stage_file(entry_from_metadata(
                        &meta,
                        path.as_bytes(),
                        entry.oid,
                        entry.mode,
                    ));
                }
                outcome.restored.push(path.clone());
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

fn index_entry_as_flat(entry: &IndexEntry) -> FlatTreeEntry {
    FlatTreeEntry {
        path: String::from_utf8_lossy(&entry.path).into_owned(),
        mode: entry.mode,
        oid: entry.oid,
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
        &repo.odb,
        &repo.git_dir,
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
    let obj = repo.odb.read(&entry.oid)?;
    Ok(obj.data.len() as u32)
}
