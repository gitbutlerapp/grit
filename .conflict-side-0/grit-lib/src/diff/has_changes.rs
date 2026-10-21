//! Fast change detection for diffs without building hunks.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::config::ConfigSet;
use crate::diff::DiffTarget;
use crate::diff::{
    apply_rename_detection_to_diff_entries, diff_index_to_tree_with_renames,
    diff_index_to_worktree_with_options, diff_tree_to_worktree_with_git_dir_and_rules,
    diff_trees_with_renames_and_policy, index_mtime_for_diff, merge_file, tree_oid_of_commit,
    DiffEntry, DiffIndexToWorktreeOptions, DiffOptions, DiffStatus,
};
use crate::error::{Error, Result};
use crate::index::Index;
use crate::objects::odb::Odb;
use crate::objects::{parse_commit, ObjectId};
use crate::repo::Repository;
use crate::state::resolve_head;
use crate::worktree::worktree_rules::WorktreeRules;

/// Hook for tests that assert early exit while scanning worktree diffs.
#[doc(hidden)]
pub static WORKTREE_HAS_CHANGES_ENTRIES_SCANNED: AtomicUsize = AtomicUsize::new(0);

/// Hook for tests that assert `Diff::has_changes` was (not) invoked.
#[doc(hidden)]
pub static HAS_CHANGES_CALL_COUNT: AtomicUsize = AtomicUsize::new(0);

fn opts_need_content_compare(opts: &DiffOptions) -> bool {
    opts.whitespace.is_active()
}

/// Returns whether `pathspecs` selects this diff entry (empty means all paths).
fn entry_matches_pathspecs(repo: &Repository, entry: &DiffEntry, pathspecs: &[String]) -> bool {
    if pathspecs.is_empty() {
        return true;
    }
    let path = entry.path();
    if repo.paths_match_pathspecs(path, pathspecs) {
        return true;
    }
    if let Some(old) = entry.old_path.as_deref() {
        if old != path && repo.paths_match_pathspecs(old, pathspecs) {
            return true;
        }
    }
    false
}

fn trust_filemode(config: &ConfigSet) -> bool {
    config
        .get_bool("core.filemode")
        .and_then(|r| r.ok())
        .unwrap_or(true)
}

fn rename_threshold(opts: &DiffOptions) -> u32 {
    opts.renames.threshold_percent().unwrap_or(101)
}

/// Whether two text sides differ under whitespace options in `opts`.
#[must_use]
pub fn text_sides_differ(old: &str, new: &str, opts: &DiffOptions) -> bool {
    if old == new {
        return false;
    }
    let ws = opts.whitespace;
    let (a, b) = preprocess_for_whitespace(old, new, ws);
    if ws.ignore_blank_lines {
        let a: Vec<_> = a.lines().filter(|l| !l.trim().is_empty()).collect();
        let b: Vec<_> = b.lines().filter(|l| !l.trim().is_empty()).collect();
        return a != b;
    }
    a != b
}

fn preprocess_for_whitespace(
    old: &str,
    new: &str,
    ws: crate::diff::WhitespaceMode,
) -> (String, String) {
    if ws.ignore_all_space {
        return (strip_all_whitespace(old), strip_all_whitespace(new));
    }
    if ws.ignore_space_change {
        return (
            crate::diff::normalize_ignore_space_change(old),
            crate::diff::normalize_ignore_space_change(new),
        );
    }
    if ws.ignore_space_at_eol {
        return (trim_trailing_ws_lines(old), trim_trailing_ws_lines(new));
    }
    (old.to_owned(), new.to_owned())
}

fn strip_all_whitespace(s: &str) -> String {
    s.lines()
        .map(|line| {
            line.chars()
                .filter(|c| !c.is_whitespace())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn trim_trailing_ws_lines(s: &str) -> String {
    s.lines()
        .map(|line| line.trim_end_matches(|c: char| c.is_whitespace()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn worktree_file_index_blob_differ(
    odb: &Odb,
    work_tree: &Path,
    rel_path: &str,
    index_oid: &ObjectId,
    opts: &DiffOptions,
) -> Result<bool> {
    let file_path = work_tree.join(rel_path);
    let wt_bytes = std::fs::read(&file_path).map_err(Error::Io)?;
    let index_raw = odb.read(index_oid)?.data;
    if merge_file::is_binary(&wt_bytes) || merge_file::is_binary(&index_raw) {
        return Ok(wt_bytes != index_raw);
    }
    let wt_s = String::from_utf8_lossy(&wt_bytes);
    let idx_s = String::from_utf8_lossy(&index_raw);
    Ok(text_sides_differ(&idx_s, &wt_s, opts))
}

fn worktree_index_entry_has_changes(
    entry: &DiffEntry,
    trust_fm: bool,
    odb: &Odb,
    work_tree: &Path,
    opts: &DiffOptions,
) -> Result<bool> {
    match entry.status {
        DiffStatus::Added
        | DiffStatus::Deleted
        | DiffStatus::TypeChanged
        | DiffStatus::Unmerged => {
            return Ok(true);
        }
        DiffStatus::Renamed | DiffStatus::Copied => return Ok(true),
        DiffStatus::Modified => {}
    }
    if entry.old_mode != entry.new_mode && trust_fm {
        return Ok(true);
    }
    let rel_path = entry.path();
    let content_aware = opts_need_content_compare(opts);
    if entry.new_oid.is_zero() && !entry.old_oid.is_zero() {
        if content_aware {
            return worktree_file_index_blob_differ(odb, work_tree, rel_path, &entry.old_oid, opts);
        }
        return Ok(true);
    }
    if entry.old_oid != entry.new_oid {
        return blob_bytes_differ(odb, &entry.old_oid, &entry.new_oid, opts);
    }
    if content_aware {
        return worktree_file_index_blob_differ(odb, work_tree, rel_path, &entry.old_oid, opts);
    }
    Ok(true)
}

fn blob_bytes_differ(
    odb: &Odb,
    old_oid: &ObjectId,
    new_oid: &ObjectId,
    opts: &DiffOptions,
) -> Result<bool> {
    if old_oid == new_oid {
        return Ok(false);
    }
    let old_raw = odb.read(old_oid)?.data;
    let new_raw = odb.read(new_oid)?.data;
    if merge_file::is_binary(&old_raw) || merge_file::is_binary(&new_raw) {
        return Ok(old_raw != new_raw);
    }
    let old_s = String::from_utf8_lossy(&old_raw);
    let new_s = String::from_utf8_lossy(&new_raw);
    Ok(text_sides_differ(&old_s, &new_s, opts))
}

/// Whether a tree/index diff entry counts as a real change for `--exit-code` semantics.
pub(crate) fn tree_entry_has_changes(
    odb: &Odb,
    entry: &DiffEntry,
    trust_fm: bool,
    opts: &DiffOptions,
) -> Result<bool> {
    match entry.status {
        DiffStatus::Added
        | DiffStatus::Deleted
        | DiffStatus::TypeChanged
        | DiffStatus::Unmerged => {
            return Ok(true);
        }
        DiffStatus::Renamed | DiffStatus::Copied => return Ok(true),
        DiffStatus::Modified => {}
    }
    if entry.old_mode != entry.new_mode && trust_fm {
        return Ok(true);
    }
    // Index↔worktree diffs use the null OID on the worktree side when content differs.
    if entry.new_oid.is_zero() && !entry.old_oid.is_zero() {
        return Ok(true);
    }
    if entry.old_oid.is_zero() && !entry.new_oid.is_zero() {
        return Ok(true);
    }
    if entry.old_oid != entry.new_oid {
        return blob_bytes_differ(odb, &entry.old_oid, &entry.new_oid, opts);
    }
    Ok(false)
}

fn head_tree(repo: &Repository) -> Result<Option<ObjectId>> {
    let head = resolve_head(repo.git_dir())?;
    Ok(match head.oid() {
        Some(oid) => Some(tree_oid_of_commit(repo.odb(), oid)?),
        None => None,
    })
}

fn worktree_has_changes(
    repo: &Repository,
    index: &mut Index,
    base_tree: Option<&ObjectId>,
    pathspecs: &[String],
    opts: &DiffOptions,
) -> Result<bool> {
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::NotARepository("bare repository".into()))?;
    let config = repo.config()?;
    let index_mtime = index_mtime_for_diff(index, None);
    let rules_arc = Arc::new(std::sync::Mutex::new(WorktreeRules::from_repository(
        repo, index,
    )?));

    let cache_valid =
        crate::index::write_tree::cache_tree_extension_valid(index.cache_tree.as_ref());
    let index_backed = match base_tree {
        // Unstaged diffs are always index vs worktree.
        None => true,
        Some(tree) => {
            index
                .cache_tree_root
                .as_ref()
                .is_some_and(|root| root == tree)
                && cache_valid
        }
    };

    if index_backed {
        WORKTREE_HAS_CHANGES_ENTRIES_SCANNED.store(0, Ordering::SeqCst);
        let content_compare = opts_need_content_compare(opts);
        let (entries, _) = diff_index_to_worktree_with_options(
            repo.odb(),
            index,
            work_tree,
            DiffIndexToWorktreeOptions {
                repository_git_dir: Some(repo.git_dir().to_path_buf()),
                index_mtime,
                config: Some(Arc::clone(&config)),
                thread_policy: Some(repo.thread_policy()),
                refresh_index_stat_in_pass: false,
                for_status: !content_compare,
                worktree_rules: Some(Arc::clone(&rules_arc)),
                ..DiffIndexToWorktreeOptions::default()
            },
        )?;
        for entry in &entries {
            WORKTREE_HAS_CHANGES_ENTRIES_SCANNED.fetch_add(1, Ordering::SeqCst);
            if !entry_matches_pathspecs(repo, entry, pathspecs) {
                continue;
            }
            if worktree_index_entry_has_changes(
                entry,
                trust_filemode(config.as_ref()),
                repo.odb(),
                work_tree,
                opts,
            )? {
                return Ok(true);
            }
        }
        return Ok(false);
    }

    let worktree_rules = WorktreeRules::from_repository(repo, index)?;
    let raw = diff_tree_to_worktree_with_git_dir_and_rules(
        repo.odb(),
        base_tree,
        work_tree,
        repo.git_dir(),
        index,
        Some(config.as_ref()),
        Some(&worktree_rules),
    )?;
    let entries = apply_rename_detection_to_diff_entries(
        repo.odb(),
        raw,
        Some(work_tree),
        rename_threshold(opts),
        config.as_ref(),
        &repo.thread_policy(),
    )?;
    for entry in entries {
        if !entry_matches_pathspecs(repo, &entry, pathspecs) {
            continue;
        }
        if tree_entry_has_changes(repo.odb(), &entry, trust_filemode(config.as_ref()), opts)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn staged_has_changes(repo: &Repository, pathspecs: &[String], opts: &DiffOptions) -> Result<bool> {
    let head_tree = head_tree(repo)?;
    let index = repo.index_handle().load()?;
    let config = repo.config()?;
    let mut entries = diff_index_to_tree_with_renames(
        repo.odb(),
        &index,
        head_tree.as_ref(),
        false,
        rename_threshold(opts),
        config.as_ref(),
        &repo.thread_policy(),
    )?;
    for entry in entries.drain(..) {
        if !entry_matches_pathspecs(repo, &entry, pathspecs) {
            continue;
        }
        if tree_entry_has_changes(repo.odb(), &entry, trust_filemode(config.as_ref()), opts)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn unstaged_has_changes(
    repo: &Repository,
    pathspecs: &[String],
    opts: &DiffOptions,
) -> Result<bool> {
    let mut index = repo.index_handle().load()?;
    worktree_has_changes(repo, &mut index, None, pathspecs, opts)
}

fn commit_trees_has_changes(
    repo: &Repository,
    base: &ObjectId,
    tip: &ObjectId,
    pathspecs: &[String],
    opts: &DiffOptions,
) -> Result<bool> {
    let config = repo.config()?;
    let base_tree = tree_oid_of_commit(repo.odb(), base)?;
    let tip_tree = tree_oid_of_commit(repo.odb(), tip)?;
    let mut entries = diff_trees_with_renames_and_policy(
        repo.odb(),
        Some(&base_tree),
        Some(&tip_tree),
        "",
        rename_threshold(opts),
        config.as_ref(),
        &repo.thread_policy(),
    )?;
    for entry in entries.drain(..) {
        if !entry_matches_pathspecs(repo, &entry, pathspecs) {
            continue;
        }
        if tree_entry_has_changes(repo.odb(), &entry, trust_filemode(config.as_ref()), opts)? {
            return Ok(true);
        }
    }
    Ok(false)
}

impl<'repo> super::handle::Diff<'repo> {
    /// Whether the selected diff has any real changes, stopping at the first one.
    ///
    /// Worktree targets honor the index stat cache and only hash blob content when needed.
    /// When whitespace ignore flags are set on `opts`, content is compared under those rules;
    /// a change that differs only in ignored whitespace returns `false`.
    ///
    /// # Errors
    ///
    /// Propagates repository, ODB, and diff errors.
    pub fn has_changes(
        &self,
        target: &DiffTarget,
        pathspecs: &[String],
        opts: DiffOptions,
    ) -> Result<bool> {
        HAS_CHANGES_CALL_COUNT.fetch_add(1, Ordering::SeqCst);
        let repo = self.repository();
        match target {
            DiffTarget::WorktreeVsHead => {
                let mut index = repo.index_handle().load()?;
                let head_tree = head_tree(repo)?;
                worktree_has_changes(repo, &mut index, head_tree.as_ref(), pathspecs, &opts)
            }
            DiffTarget::IndexVsHead => staged_has_changes(repo, pathspecs, &opts),
            DiffTarget::WorktreeVsIndex => unstaged_has_changes(repo, pathspecs, &opts),
            DiffTarget::WorktreeVsTree(tree) => {
                let mut index = repo.index_handle().load()?;
                worktree_has_changes(repo, &mut index, Some(tree), pathspecs, &opts)
            }
            DiffTarget::Commit(commit) => {
                let obj = repo.odb().read(commit)?;
                let parsed = parse_commit(&obj.data)?;
                let new_tree = parsed.tree;
                let old_tree = parsed
                    .parents
                    .first()
                    .map(|parent| tree_oid_of_commit(repo.odb(), parent))
                    .transpose()?;
                let config = repo.config()?;
                let mut entries = diff_trees_with_renames_and_policy(
                    repo.odb(),
                    old_tree.as_ref(),
                    Some(&new_tree),
                    "",
                    rename_threshold(&opts),
                    config.as_ref(),
                    &repo.thread_policy(),
                )?;
                for entry in entries.drain(..) {
                    if !entry_matches_pathspecs(repo, &entry, pathspecs) {
                        continue;
                    }
                    if tree_entry_has_changes(
                        repo.odb(),
                        &entry,
                        trust_filemode(config.as_ref()),
                        &opts,
                    )? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            DiffTarget::Trees { old, new } => match old {
                Some(base) => commit_trees_has_changes(repo, base, new, pathspecs, &opts),
                None => {
                    let config = repo.config()?;
                    let tip_tree = tree_oid_of_commit(repo.odb(), new)?;
                    let mut entries = diff_trees_with_renames_and_policy(
                        repo.odb(),
                        None,
                        Some(&tip_tree),
                        "",
                        rename_threshold(&opts),
                        config.as_ref(),
                        &repo.thread_policy(),
                    )?;
                    for entry in entries.drain(..) {
                        if !entry_matches_pathspecs(repo, &entry, pathspecs) {
                            continue;
                        }
                        if tree_entry_has_changes(
                            repo.odb(),
                            &entry,
                            trust_filemode(config.as_ref()),
                            &opts,
                        )? {
                            return Ok(true);
                        }
                    }
                    Ok(false)
                }
            },
        }
    }
}
