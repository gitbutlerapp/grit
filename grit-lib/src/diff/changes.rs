//! Staged, unstaged, and working-tree changes with both sides' contents loaded.
//!
//! [`UncommittedDiff`] collects [`DiffEntry`] rows for the index against `HEAD`, the working
//! tree against the index, or the working tree against any tree, reusing one loaded index and
//! one set of [`WorktreeRules`] for every step. [`UncommittedDiff::load_worktree_changes`] and
//! [`load_object_changes`] then turn entries into [`FileChange`]s whose bytes feed structured
//! hunks ([`FileChange::structured`]) or line counts ([`FileChange::line_stat`]).

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::diff::structured::{structured_file_diff, SideBytes, StructuredFileDiff};
use crate::diff::targets::{commit_tree, diff_index_to_tree_for_paths};
use crate::diff::tree_content::{gitlink_line, load_new_side, load_old_side};
use crate::diff::{
    apply_rename_detection_to_diff_entries, diff_index_to_tree_with_renames,
    diff_index_to_worktree_with_renames, diff_tree_to_worktree_with_git_dir_and_rules,
    index_mtime_for_diff, line_change_counts, load_worktree_side_for_diff, read_submodule_head_oid,
    sort_entries_by_path, submodule_porcelain_flags, DiffEntry, DiffIndexToWorktreeOptions,
    DiffOptions, DiffStatus, LineStat, RenameDetection, RENAME_DETECTION_DISABLED,
};
use crate::error::{Error, Result};
use crate::index::write_tree::cache_tree_extension_valid;
use crate::index::Index;
use crate::objects::odb::Odb;
use crate::objects::{FileMode, ObjectId};
use crate::parallel::{try_par_map, ParallelHashError, ParallelOp};
use crate::repo::Repository;
use crate::state::resolve_head;
use crate::worktree::fsmonitor::fsmonitor_enabled;
use crate::worktree::worktree_rules::WorktreeRules;

/// One changed path with both sides' bytes loaded for diffing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Display path (new path, or old path for a deletion).
    pub path: String,
    /// Pre-rename path, only when it differs from [`Self::path`].
    pub old_path: Option<String>,
    /// Change kind for this path.
    pub status: DiffStatus,
    /// Old side mode ([`FileMode::ABSENT`] when the path is added).
    pub old_mode: FileMode,
    /// New side mode ([`FileMode::ABSENT`] when the path is deleted).
    pub new_mode: FileMode,
    /// Old side object id (zero when absent).
    pub old_oid: ObjectId,
    /// New side object id (zero when absent or not yet hashed from the working tree).
    pub new_oid: ObjectId,
    /// Old side contents (a `Subproject commit` line for gitlinks).
    pub old_bytes: SideBytes,
    /// New side contents, read from the working tree for working-tree changes.
    pub new_bytes: SideBytes,
    /// Either side is treated as binary; binary sides may not carry their bytes.
    pub binary: bool,
    /// The object ids differ (false for a mode-only change).
    pub content_changed: bool,
    /// Rename or copy similarity (0–100), when applicable.
    pub similarity: Option<u32>,
}

impl FileChange {
    fn from_entry(
        entry: DiffEntry,
        old_bytes: SideBytes,
        new_bytes: SideBytes,
        binary: bool,
    ) -> Self {
        let path = entry
            .new_path
            .clone()
            .or_else(|| entry.old_path.clone())
            .unwrap_or_default();
        let similarity = entry.rename_similarity();
        // A deletion's display path is its old path; only a real rename keeps `old_path`.
        let old_path = entry.old_path.filter(|old| *old != path);
        Self {
            path,
            old_path,
            status: entry.status,
            old_mode: entry.old_mode,
            new_mode: entry.new_mode,
            old_oid: entry.old_oid,
            new_oid: entry.new_oid,
            old_bytes,
            new_bytes,
            binary,
            content_changed: entry.old_oid != entry.new_oid,
            similarity,
        }
    }

    /// Structured line hunks and mode metadata for this change.
    ///
    /// Binary changes get mode metadata and no hunks.
    ///
    /// # Parameters
    ///
    /// - `options` - line-diff settings (context, algorithm, whitespace handling).
    #[must_use]
    pub fn structured(&self, options: DiffOptions) -> StructuredFileDiff {
        if self.binary {
            return StructuredFileDiff {
                old_mode: self.old_mode.octal_if_present(),
                new_mode: self.new_mode.octal_if_present(),
                old_encoding_lossy: self.old_bytes.encoding_lossy,
                new_encoding_lossy: self.new_bytes.encoding_lossy,
                hunks: Vec::new(),
            };
        }
        structured_file_diff(
            self.old_mode,
            self.new_mode,
            &self.old_bytes,
            &self.new_bytes,
            options,
        )
    }

    /// Inserted and deleted line counts, or `None` for a binary change.
    ///
    /// # Parameters
    ///
    /// - `options` - line-diff settings; whitespace modes change what counts as a changed line.
    #[must_use]
    pub fn line_stat(&self, options: &DiffOptions) -> Option<LineStat> {
        if self.binary {
            return None;
        }
        Some(line_change_counts(
            &self.old_bytes.text(),
            &self.new_bytes.text(),
            options,
        ))
    }
}

/// Load both sides of each entry from the object database.
///
/// Use this for index-against-tree and tree-against-tree entries, whose sides are all stored
/// objects. Entries are loaded in parallel per the repository's thread policy; the result keeps
/// the order of `entries`.
///
/// # Parameters
///
/// - `repo` - repository whose object database holds both sides.
/// - `entries` - diff rows to load, e.g. from [`UncommittedDiff::staged_entries`].
///
/// # Errors
///
/// Returns config load failures and object read errors.
pub fn load_object_changes(repo: &Repository, entries: &[DiffEntry]) -> Result<Vec<FileChange>> {
    let config = repo.config()?;
    let par_opts = repo
        .thread_policy()
        .resolve(ParallelOp::DiffContent, config.as_ref());
    let odb = repo.odb();
    let est_bytes = entries.len().saturating_mul(8192);
    try_par_map(entries, par_opts, est_bytes, |entry| {
        load_change(odb, entry.clone(), None)
    })
    .map_err(|ParallelHashError::Task(err)| err)
}

/// Where a working-tree change's new side is read from.
pub(crate) struct WorktreeSide<'a> {
    pub(crate) work_tree: &'a Path,
    pub(crate) index: &'a Index,
    pub(crate) rules: &'a Mutex<WorktreeRules>,
}

/// Load one entry's sides: the old side from the object database, the new side from the
/// working tree when `worktree` is given, otherwise from the object database.
pub(crate) fn load_change(
    odb: &Odb,
    entry: DiffEntry,
    worktree: Option<&WorktreeSide<'_>>,
) -> Result<FileChange> {
    let (old_bytes, old_binary) = load_old_side(odb, &entry, None)?;
    let (new_bytes, new_binary) = match worktree {
        Some(side) => load_worktree_new_side(odb, &entry, side)?,
        None => load_new_side(odb, &entry, None)?,
    };
    Ok(FileChange::from_entry(
        entry,
        old_bytes,
        new_bytes,
        old_binary || new_binary,
    ))
}

/// Load every entry with [`load_change`], in order.
pub(crate) fn load_changes(
    odb: &Odb,
    entries: Vec<DiffEntry>,
    worktree: Option<&WorktreeSide<'_>>,
) -> Result<Vec<FileChange>> {
    entries
        .into_iter()
        .map(|entry| load_change(odb, entry, worktree))
        .collect()
}

fn load_worktree_new_side(
    odb: &Odb,
    entry: &DiffEntry,
    side: &WorktreeSide<'_>,
) -> Result<(SideBytes, bool)> {
    if entry.new_mode == FileMode::GITLINK {
        let line = worktree_gitlink_line(entry, side.work_tree);
        return Ok((SideBytes::from_bytes(line.into_bytes()), false));
    }
    if entry.new_mode == FileMode::ABSENT {
        return Ok((SideBytes::from_bytes(Vec::new()), false));
    }
    let Some(path) = entry.new_path.as_deref().or(entry.old_path.as_deref()) else {
        return load_new_side(odb, entry, None);
    };
    // Working-tree entries often carry only a hash of the normalized file, and the blob is not
    // in the object database until it is staged, so always read the live file.
    let index_entry = side.index.get(&crate::repo_relpath::storage_bytes(path), 0);
    let rules = side.rules.lock().unwrap_or_else(|e| e.into_inner());
    load_worktree_side_for_diff(
        odb,
        side.work_tree,
        path,
        entry.new_mode.bits(),
        rules.config(),
        index_entry,
        Some(&rules),
        false,
    )
}

/// The submodule's checked-out commit, with `-dirty` when its tracked content is modified.
fn worktree_gitlink_line(entry: &DiffEntry, work_tree: &Path) -> String {
    let path = entry.new_path.as_deref().or(entry.old_path.as_deref());
    let head = if entry.new_oid != ObjectId::zero() {
        Some(entry.new_oid)
    } else {
        path.and_then(|p| read_submodule_head_oid(&work_tree.join(p)))
    };
    let Some(head) = head.filter(|oid| *oid != ObjectId::zero()) else {
        return String::new();
    };
    let dirty = entry.old_mode == FileMode::GITLINK
        && path.is_some_and(|p| submodule_porcelain_flags(work_tree, p, entry.old_oid).modified);
    if dirty {
        format!("Subproject commit {}-dirty\n", head.to_hex())
    } else {
        gitlink_line(&head)
    }
}

fn rename_threshold(renames: RenameDetection) -> u32 {
    renames
        .threshold_percent()
        .unwrap_or(RENAME_DETECTION_DISABLED)
}

/// `HEAD`'s tree, or `None` on an unborn branch.
pub(crate) fn head_tree(repo: &Repository) -> Result<Option<ObjectId>> {
    resolve_head(repo.git_dir())?
        .oid()
        .map(|oid| commit_tree(repo, oid))
        .transpose()
}

/// Index against `HEAD`'s tree, sorted by path.
///
/// A non-empty `restrict_to_paths` compares only those exact paths, without rename detection.
pub(crate) fn staged_entries_in(
    repo: &Repository,
    index: &Index,
    rename_threshold: u32,
    restrict_to_paths: Option<&BTreeSet<String>>,
) -> Result<Vec<DiffEntry>> {
    let head_tree = head_tree(repo)?;
    let mut entries = match restrict_to_paths.filter(|paths| !paths.is_empty()) {
        Some(paths) => diff_index_to_tree_for_paths(repo, index, head_tree.as_ref(), paths)?,
        None => {
            let config = repo.config()?;
            diff_index_to_tree_with_renames(
                repo.odb(),
                index,
                head_tree.as_ref(),
                false,
                rename_threshold,
                config.as_ref(),
                &repo.thread_policy(),
            )?
        }
    };
    sort_entries_by_path(&mut entries);
    Ok(entries)
}

/// Working tree against `index`, sorted by path.
///
/// `restrict_to_paths` limits the scan to those paths and skips the fsmonitor refresh.
pub(crate) fn unstaged_entries_in(
    repo: &Repository,
    index: &mut Index,
    work_tree: &Path,
    rules: &Arc<Mutex<WorktreeRules>>,
    rename_threshold: u32,
    restrict_to_paths: Option<Arc<BTreeSet<String>>>,
) -> Result<Vec<DiffEntry>> {
    let config = rules.lock().unwrap_or_else(|e| e.into_inner()).config_arc();
    let refresh_fsmonitor = restrict_to_paths.is_none() && fsmonitor_enabled(config.as_ref());
    let options = DiffIndexToWorktreeOptions {
        repository_git_dir: Some(repo.git_dir().to_path_buf()),
        index_mtime: index_mtime_for_diff(index, None),
        config: Some(config),
        thread_policy: Some(repo.thread_policy()),
        refresh_index_stat_in_pass: false,
        refresh_fsmonitor,
        worktree_rules: Some(Arc::clone(rules)),
        command_runner: Some(repo.command_runner()),
        restrict_to_paths,
        ..DiffIndexToWorktreeOptions::default()
    };
    let (mut entries, _) =
        diff_index_to_worktree_with_renames(repo, index, work_tree, options, rename_threshold)?;
    sort_entries_by_path(&mut entries);
    Ok(entries)
}

/// Working tree (through `index`) against `base_tree`, sorted by path.
///
/// When the index's cache tree matches `base_tree`, this is the cheaper index-against-worktree
/// scan; otherwise the tree is walked against the working tree directly.
pub(crate) fn worktree_entries_in(
    repo: &Repository,
    index: &mut Index,
    work_tree: &Path,
    rules: &Arc<Mutex<WorktreeRules>>,
    base_tree: Option<&ObjectId>,
    rename_threshold: u32,
) -> Result<Vec<DiffEntry>> {
    let index_matches_base = match (base_tree, index.cache_tree_root.as_ref()) {
        (Some(base), Some(cached)) => base == cached,
        (None, None) => true,
        _ => false,
    };
    if index_matches_base && cache_tree_extension_valid(index.cache_tree.as_ref()) {
        return unstaged_entries_in(repo, index, work_tree, rules, rename_threshold, None);
    }
    let config = repo.config()?;
    let raw = {
        let rules = rules.lock().unwrap_or_else(|e| e.into_inner());
        diff_tree_to_worktree_with_git_dir_and_rules(
            repo.odb(),
            base_tree,
            work_tree,
            repo.git_dir(),
            index,
            Some(config.as_ref()),
            Some(&rules),
        )?
    };
    let mut entries = apply_rename_detection_to_diff_entries(
        repo.odb(),
        raw,
        Some(work_tree),
        rename_threshold,
        config.as_ref(),
        &repo.thread_policy(),
    )?;
    sort_entries_by_path(&mut entries);
    Ok(entries)
}

/// Staged, unstaged, and working-tree diffs over one loaded index.
///
/// Every method reuses the same [`Index`] and, once a working-tree method needs them, the same
/// [`WorktreeRules`], so collecting entries and then loading their contents reads neither twice.
/// Working-tree scans may refresh stat data on the in-memory index; nothing is written to disk.
///
/// Obtain one from [`Diff::uncommitted`](crate::diff::Diff::uncommitted) or
/// [`UncommittedDiff::new`].
pub struct UncommittedDiff<'repo> {
    repo: &'repo Repository,
    index: Index,
    rules: Option<Arc<Mutex<WorktreeRules>>>,
}

impl<'repo> UncommittedDiff<'repo> {
    /// Diff `repo` against an index the caller already loaded.
    ///
    /// # Parameters
    ///
    /// - `repo` - repository to diff.
    /// - `index` - its index (normally the on-disk index).
    #[must_use]
    pub fn new(repo: &'repo Repository, index: Index) -> Self {
        Self {
            repo,
            index,
            rules: None,
        }
    }

    /// The index every comparison uses.
    #[must_use]
    pub fn index(&self) -> &Index {
        &self.index
    }

    /// Index against `HEAD`'s tree (`grit diff --staged`), sorted by path.
    ///
    /// An unborn `HEAD` compares against the empty tree, so every index entry is added.
    ///
    /// # Parameters
    ///
    /// - `renames` - rename detection between deleted and added paths.
    ///
    /// # Errors
    ///
    /// Returns `HEAD` resolution, config, and object read errors.
    pub fn staged_entries(&self, renames: RenameDetection) -> Result<Vec<DiffEntry>> {
        staged_entries_in(self.repo, &self.index, rename_threshold(renames), None)
    }

    /// Working tree against the index (`grit diff --unstaged`), sorted by path.
    ///
    /// # Parameters
    ///
    /// - `renames` - rename detection between deleted and added paths.
    ///
    /// # Errors
    ///
    /// Returns [`Error::RequiresWorkTree`] in a bare repository, plus worktree rule, I/O, and
    /// object read errors.
    pub fn unstaged_entries(&mut self, renames: RenameDetection) -> Result<Vec<DiffEntry>> {
        let work_tree = self.work_tree()?;
        let rules = self.rules()?;
        unstaged_entries_in(
            self.repo,
            &mut self.index,
            work_tree,
            &rules,
            rename_threshold(renames),
            None,
        )
    }

    /// Working tree and index together against `base_tree` (`grit diff`, `grit diff --against`),
    /// sorted by path.
    ///
    /// # Parameters
    ///
    /// - `base_tree` - tree to compare with; `None` for the empty tree (an unborn `HEAD`).
    /// - `renames` - rename detection between deleted and added paths.
    ///
    /// # Errors
    ///
    /// Returns [`Error::RequiresWorkTree`] in a bare repository, plus worktree rule, I/O, and
    /// object read errors.
    pub fn worktree_entries(
        &mut self,
        base_tree: Option<&ObjectId>,
        renames: RenameDetection,
    ) -> Result<Vec<DiffEntry>> {
        let work_tree = self.work_tree()?;
        let rules = self.rules()?;
        worktree_entries_in(
            self.repo,
            &mut self.index,
            work_tree,
            &rules,
            base_tree,
            rename_threshold(renames),
        )
    }

    /// Load entries from [`Self::unstaged_entries`] or [`Self::worktree_entries`]: old sides
    /// from the object database, new sides from the working tree (with gitlinks showing the
    /// submodule's checked-out commit and `-dirty` state).
    ///
    /// # Errors
    ///
    /// Returns [`Error::RequiresWorkTree`] in a bare repository, plus I/O, filter, and object
    /// read errors.
    pub fn load_worktree_changes(&mut self, entries: Vec<DiffEntry>) -> Result<Vec<FileChange>> {
        let work_tree = self.work_tree()?;
        let rules = self.rules()?;
        let side = WorktreeSide {
            work_tree,
            index: &self.index,
            rules: &rules,
        };
        load_changes(self.repo.odb(), entries, Some(&side))
    }

    fn work_tree(&self) -> Result<&'repo Path> {
        self.repo.work_tree().ok_or(Error::RequiresWorkTree)
    }

    fn rules(&mut self) -> Result<Arc<Mutex<WorktreeRules>>> {
        if let Some(rules) = &self.rules {
            return Ok(Arc::clone(rules));
        }
        let rules = Arc::new(Mutex::new(WorktreeRules::from_repository(
            self.repo,
            &self.index,
        )?));
        self.rules = Some(Arc::clone(&rules));
        Ok(rules)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::InitOptions;

    fn entry(status: DiffStatus, old: Option<&str>, new: Option<&str>) -> DiffEntry {
        DiffEntry {
            status,
            old_path: old.map(str::to_owned),
            new_path: new.map(str::to_owned),
            old_mode: FileMode::REGULAR,
            new_mode: FileMode::REGULAR,
            old_oid: ObjectId::zero(),
            new_oid: ObjectId::zero(),
            score: Some(87),
        }
    }

    #[test]
    fn from_entry_keeps_old_path_only_for_real_renames() {
        let bytes = || SideBytes::from_bytes(Vec::new());
        let renamed = FileChange::from_entry(
            entry(DiffStatus::Renamed, Some("a"), Some("b")),
            bytes(),
            bytes(),
            false,
        );
        assert_eq!(renamed.path, "b");
        assert_eq!(renamed.old_path.as_deref(), Some("a"));
        assert_eq!(renamed.similarity, Some(87));

        let deleted = FileChange::from_entry(
            entry(DiffStatus::Deleted, Some("a"), None),
            bytes(),
            bytes(),
            false,
        );
        assert_eq!(deleted.path, "a");
        assert_eq!(deleted.old_path, None);
        assert_eq!(deleted.similarity, None);
    }

    #[test]
    fn binary_change_has_no_hunks_or_line_stat() {
        let mut change = FileChange::from_entry(
            entry(DiffStatus::Modified, Some("a"), Some("a")),
            SideBytes::from_bytes(b"one\n".to_vec()),
            SideBytes::from_bytes(b"two\n".to_vec()),
            true,
        );
        assert!(change.structured(DiffOptions::default()).hunks.is_empty());
        assert_eq!(change.line_stat(&DiffOptions::default()), None);

        change.binary = false;
        assert_eq!(change.structured(DiffOptions::default()).hunks.len(), 1);
        assert_eq!(
            change.line_stat(&DiffOptions::default()),
            Some(LineStat {
                insertions: 1,
                deletions: 1,
            })
        );
    }

    #[test]
    fn worktree_gitlink_line_reads_submodule_head_when_unhashed() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let mut gitlink = entry(DiffStatus::Added, None, Some("missing-sub"));
        gitlink.old_mode = FileMode::ABSENT;
        gitlink.new_mode = FileMode::GITLINK;
        assert_eq!(worktree_gitlink_line(&gitlink, dir.path()), "");

        let oid = ObjectId::from_hex("1111111111111111111111111111111111111111")
            .unwrap_or_else(|e| panic!("oid: {e}"));
        gitlink.new_oid = oid;
        assert_eq!(
            worktree_gitlink_line(&gitlink, dir.path()),
            format!("Subproject commit {}\n", oid.to_hex())
        );
    }

    #[test]
    fn bare_repository_worktree_methods_require_a_work_tree() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let init = InitOptions {
            bare: true,
            ..InitOptions::default()
        };
        let repo = Repository::init(dir.path(), &init).unwrap_or_else(|e| panic!("init: {e}"));
        let mut diff = UncommittedDiff::new(&repo, Index::default());
        assert!(diff
            .staged_entries(RenameDetection::Off)
            .is_ok_and(|e| e.is_empty()));
        assert!(matches!(
            diff.unstaged_entries(RenameDetection::Off),
            Err(Error::RequiresWorkTree)
        ));
        assert!(matches!(
            diff.worktree_entries(None, RenameDetection::Off),
            Err(Error::RequiresWorkTree)
        ));
        assert!(matches!(
            diff.load_worktree_changes(Vec::new()),
            Err(Error::RequiresWorkTree)
        ));
    }
}
