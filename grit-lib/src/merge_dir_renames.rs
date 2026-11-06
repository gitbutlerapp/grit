//! Directory rename detection for three-way tree merges.
//!
//! When one side of a merge moves every file out of a directory and the other side adds a file
//! inside that directory, the new file most likely belongs in the directory's new home. This
//! module works out which directories each side renamed and where the other side's new files
//! should go, following the rules of Git's `ort` merge so merged trees and unmerged index rows
//! match what Git writes:
//!
//! - A directory counts as renamed by a side only when that side removed it entirely, and only
//!   when the other side put a new file directly inside it (or inside a removed directory above
//!   it). Each rename out of the directory is a vote for its destination directory; a rename
//!   `a/b/c/x -> d/b/c/x` also votes `a/b -> d/b` and `a -> d` while the trailing names match.
//!   The destination with the most votes wins; a tie is a [`DirectoryRenameConflict::Split`].
//! - A directory renamed by both sides is left to the per-file rename handling.
//! - A new file is moved by its deepest renamed ancestor directory, unless the destination
//!   directory is itself one the adding side renamed away, or the destination path is taken on
//!   the adding side or claimed by several new files ([`DirectoryRenameConflict::Blocked`]).
//!   A file the base or the renaming side has at the destination does not block the move: the
//!   moved file is merged with it there as the adding side's version.

use super::merge_tree_cache::{lookup_index_entry_at_path_cached, MergeTreeCache};
use super::{
    merge_one_path, stage_entry, DirectoryRenameConflict, DirectoryRenameMode, MergeSide,
    PathMergeContext, TheirsConflictLabel, TreeMergeConflictPresentation, TreeMergeOutput,
    WhitespaceMergeOptions,
};
use crate::index::{Index, IndexEntry};
use crate::merge_file::MergeFavor;
use crate::objects::ObjectId;
use crate::odb::Odb;
use std::collections::{BTreeMap, HashMap, HashSet};

const MODE_DIR: u32 = 0o040000;

/// One side's changes relative to the merge base, as directory rename detection needs them.
pub(super) struct SideChanges<'a> {
    /// The side's root tree.
    pub tree: ObjectId,
    /// The side's files at the paths the merge touches.
    pub files: &'a BTreeMap<Vec<u8>, IndexEntry>,
    /// Files the side renamed, as `(base path, new path)` with distinct paths.
    pub renames: Vec<(&'a [u8], &'a [u8])>,
    /// Files the side has at paths where the base has none (additions and rename destinations).
    pub new_paths: Vec<&'a [u8]>,
}

/// A new file that follows its directory's rename.
struct Relocation {
    to: Vec<u8>,
    added_by: MergeSide,
    /// Base, ours and theirs versions of a file the adding side renamed into the old directory
    /// while the renaming side edited it: Git leaves all three at the new path in
    /// [`DirectoryRenameMode::Conflict`].
    edited_rename: Option<[IndexEntry; 3]>,
    /// Files the base and the renaming side already have at `to`, when either has one: the
    /// moved file then stands for the adding side's version of `to` in a three-way merge there.
    occupied: Option<Occupant>,
}

/// What a destination held before a file moved onto it.
struct Occupant {
    base: Option<IndexEntry>,
    renamer: Option<IndexEntry>,
}

/// Where new files move and which directory renames could not be applied.
#[derive(Default)]
pub(super) struct DirectoryRenamePlan {
    relocations: BTreeMap<Vec<u8>, Relocation>,
    conflicts: Vec<DirectoryRenameConflict>,
}

/// How a removed directory takes part in rename voting.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Relevance {
    /// Not removed, or nothing the other side added needs its rename.
    None,
    /// Removed below a directory that needs its rename: lets votes pass through to it.
    Ancestor,
    /// Removed, and the other side added a file directly inside it.
    Own,
}

/// Work out the directory renames of both sides and the new files they move.
///
/// # Errors
///
/// Propagates tree read failures.
pub(super) fn plan_directory_renames(
    cache: &mut MergeTreeCache,
    odb: &Odb,
    base_tree: ObjectId,
    base_files: &BTreeMap<Vec<u8>, IndexEntry>,
    ours: &SideChanges<'_>,
    theirs: &SideChanges<'_>,
) -> crate::error::Result<DirectoryRenamePlan> {
    let mut plan = DirectoryRenamePlan::default();
    let mut ours_dirs = side_directory_renames(
        cache,
        odb,
        base_tree,
        ours,
        theirs,
        MergeSide::Ours,
        &mut plan.conflicts,
    )?;
    let mut theirs_dirs = side_directory_renames(
        cache,
        odb,
        base_tree,
        theirs,
        ours,
        MergeSide::Theirs,
        &mut plan.conflicts,
    )?;
    if ours_dirs.is_empty() && theirs_dirs.is_empty() {
        return Ok(plan);
    }
    ours_dirs.retain(|dir, _| theirs_dirs.remove(dir).is_none());

    let ours_targets = implicit_targets(&ours.new_paths, &theirs_dirs);
    let theirs_targets = implicit_targets(&theirs.new_paths, &ours_dirs);
    for (adding, renamer, added_by, renamer_dirs, own_dirs, targets, other_targets) in [
        (
            ours,
            theirs,
            MergeSide::Ours,
            &theirs_dirs,
            &ours_dirs,
            &ours_targets,
            &theirs_targets,
        ),
        (
            theirs,
            ours,
            MergeSide::Theirs,
            &ours_dirs,
            &theirs_dirs,
            &theirs_targets,
            &ours_targets,
        ),
    ] {
        let mut reported: HashSet<&[u8]> = HashSet::new();
        let mut rename_sources: Option<HashMap<&[u8], &[u8]>> = None;
        for &path in &adding.new_paths {
            if other_targets.contains_key(path) {
                continue;
            }
            let Some((old_dir, new_dir)) = renamed_ancestor(path, renamer_dirs) else {
                continue;
            };
            if own_dirs.contains_key(new_dir) {
                continue;
            }
            let to = moved_path(path, old_dir, new_dir);
            let Some((to_key, sources)) = targets.get_key_value(&to) else {
                continue;
            };
            if reported.contains(to_key.as_slice()) {
                continue;
            }
            let renamer_at = lookup_index_entry_at_path_cached(cache, odb, &renamer.tree, &to)?;
            let existing = renamer_at.as_ref().is_some_and(|e| e.mode == MODE_DIR)
                || lookup_index_entry_at_path_cached(cache, odb, &adding.tree, &to)?.is_some();
            if existing || sources.len() > 1 {
                reported.insert(to_key);
                plan.conflicts.push(DirectoryRenameConflict::Blocked {
                    path: to,
                    sources: sources.iter().map(|s| s.to_vec()).collect(),
                    existing,
                });
                continue;
            }
            let dest_to_source = rename_sources.get_or_insert_with(|| {
                adding
                    .renames
                    .iter()
                    .map(|&(old, new)| (new, old))
                    .collect()
            });
            let edited_rename = dest_to_source.get(path).and_then(|&old| {
                let base = base_files.get(old)?;
                let edited = renamer.files.get(old)?;
                let renamed = adding.files.get(path)?;
                if edited.oid == base.oid && edited.mode == base.mode {
                    return None;
                }
                let (base, edited, renamed) = (base.clone(), edited.clone(), renamed.clone());
                Some(match added_by {
                    MergeSide::Ours => [base, renamed, edited],
                    MergeSide::Theirs => [base, edited, renamed],
                })
            });
            let base_at = lookup_index_entry_at_path_cached(cache, odb, &base_tree, &to)?
                .filter(|e| e.mode != MODE_DIR);
            let occupied = (base_at.is_some() || renamer_at.is_some()).then_some(Occupant {
                base: base_at,
                renamer: renamer_at,
            });
            plan.relocations.insert(
                path.to_vec(),
                Relocation {
                    to,
                    added_by,
                    edited_rename,
                    occupied,
                },
            );
        }
    }
    Ok(plan)
}

/// New paths of one side grouped by the destination an implicit directory rename gives them.
fn implicit_targets<'a>(
    new_paths: &[&'a [u8]],
    renamer_dirs: &HashMap<&[u8], &[u8]>,
) -> HashMap<Vec<u8>, Vec<&'a [u8]>> {
    let mut out: HashMap<Vec<u8>, Vec<&'a [u8]>> = HashMap::new();
    if renamer_dirs.is_empty() {
        return out;
    }
    for &path in new_paths {
        if let Some((old_dir, new_dir)) = renamed_ancestor(path, renamer_dirs) {
            out.entry(moved_path(path, old_dir, new_dir))
                .or_default()
                .push(path);
        }
    }
    for sources in out.values_mut() {
        sources.sort_unstable();
    }
    out
}

/// The directories `renamer` renamed, as old directory → new directory.
///
/// Only directories `renamer` removed and `other` added files to are considered; ties between
/// destinations are recorded in `conflicts` as [`DirectoryRenameConflict::Split`].
fn side_directory_renames<'a>(
    cache: &mut MergeTreeCache,
    odb: &Odb,
    base_tree: ObjectId,
    renamer: &SideChanges<'a>,
    other: &SideChanges<'_>,
    renamed_by: MergeSide,
    conflicts: &mut Vec<DirectoryRenameConflict>,
) -> crate::error::Result<HashMap<&'a [u8], &'a [u8]>> {
    let mut out = HashMap::new();
    if renamer.renames.is_empty() {
        return Ok(out);
    }
    let added_dirs: HashSet<&[u8]> = other
        .new_paths
        .iter()
        .map(|path| split_last(path).0)
        .filter(|dir| !dir.is_empty())
        .collect();
    if added_dirs.is_empty() {
        return Ok(out);
    }

    let mut relevance = RelevanceLookup {
        cache,
        odb,
        base_tree,
        side_tree: renamer.tree,
        added_dirs: &added_dirs,
        removed: HashMap::new(),
    };
    let mut votes: BTreeMap<&'a [u8], BTreeMap<&'a [u8], usize>> = BTreeMap::new();
    for &(old, new) in &renamer.renames {
        let (mut old_dir, mut new_dir) = (split_last(old).0, split_last(new).0);
        let mut first = true;
        loop {
            let level = relevance.of(old_dir)?;
            if level == Relevance::Own || (first && level == Relevance::Ancestor) {
                *votes
                    .entry(old_dir)
                    .or_default()
                    .entry(new_dir)
                    .or_default() += 1;
            }
            if level == Relevance::None || old_dir.is_empty() || new_dir.is_empty() {
                break;
            }
            let (old_parent, old_name) = split_last(old_dir);
            let (new_parent, new_name) = split_last(new_dir);
            if old_name != new_name {
                break;
            }
            (old_dir, new_dir) = (old_parent, new_parent);
            first = false;
        }
    }

    for (old_dir, destinations) in votes {
        let mut best: Option<(&[u8], usize)> = None;
        let mut tied = false;
        for (&new_dir, &count) in &destinations {
            match best {
                Some((_, max)) if count == max => tied = true,
                Some((_, max)) if count < max => {}
                _ => {
                    best = Some((new_dir, count));
                    tied = false;
                }
            }
        }
        match best {
            Some(_) if tied => conflicts.push(DirectoryRenameConflict::Split {
                dir: old_dir.to_vec(),
                renamed_by,
            }),
            Some((new_dir, _)) => {
                out.insert(old_dir, new_dir);
            }
            None => {}
        }
    }
    Ok(out)
}

/// Memoized [`Relevance`] of base directories for one renaming side.
struct RelevanceLookup<'c, 'd> {
    cache: &'c mut MergeTreeCache,
    odb: &'c Odb,
    base_tree: ObjectId,
    side_tree: ObjectId,
    added_dirs: &'c HashSet<&'d [u8]>,
    removed: HashMap<Vec<u8>, bool>,
}

impl RelevanceLookup<'_, '_> {
    fn of(&mut self, dir: &[u8]) -> crate::error::Result<Relevance> {
        if !self.is_removed(dir)? {
            return Ok(Relevance::None);
        }
        if self.added_dirs.contains(dir) {
            return Ok(Relevance::Own);
        }
        let mut ancestor = dir;
        while !ancestor.is_empty() {
            ancestor = split_last(ancestor).0;
            if self.added_dirs.contains(ancestor) && self.is_removed(ancestor)? {
                return Ok(Relevance::Ancestor);
            }
        }
        Ok(Relevance::None)
    }

    /// True when the base has a directory at `dir` and the renaming side does not.
    fn is_removed(&mut self, dir: &[u8]) -> crate::error::Result<bool> {
        if dir.is_empty() {
            return Ok(false);
        }
        if let Some(&removed) = self.removed.get(dir) {
            return Ok(removed);
        }
        let is_dir = |entry: Option<IndexEntry>| entry.is_some_and(|e| e.mode == MODE_DIR);
        let removed = is_dir(lookup_index_entry_at_path_cached(
            self.cache,
            self.odb,
            &self.base_tree,
            dir,
        )?) && !is_dir(lookup_index_entry_at_path_cached(
            self.cache,
            self.odb,
            &self.side_tree,
            dir,
        )?);
        self.removed.insert(dir.to_vec(), removed);
        Ok(removed)
    }
}

/// Merge settings for a moved file that lands on a path the base or the renaming side has.
pub(super) struct DestinationMerge<'a> {
    pub ctx: &'a PathMergeContext<'a>,
    pub favor: MergeFavor,
    pub ws: WhitespaceMergeOptions,
    pub diff_algorithm: Option<&'a str>,
    pub presentation: TreeMergeConflictPresentation<'a>,
}

/// Apply `plan` to a finished path-by-path merge.
///
/// Moved files take every index row they had at the old path (and their conflict-marker
/// blob); in [`DirectoryRenameMode::Conflict`] a cleanly merged file is left unmerged at the
/// stage of the side that added it. A file moved onto a path the base or the renaming side
/// has is merged with those versions there instead, replacing the walk's result for that path.
/// Old paths are added to `renamed_away` so the result tree does not keep them.
///
/// # Errors
///
/// Propagates object read and write failures from the merge at an occupied destination.
pub(super) fn apply_directory_rename_plan(
    output: &mut TreeMergeOutput,
    plan: DirectoryRenamePlan,
    mode: DirectoryRenameMode,
    renamed_away: &mut HashSet<Vec<u8>>,
    merge: &DestinationMerge<'_>,
) -> crate::error::Result<()> {
    let DirectoryRenamePlan {
        relocations,
        conflicts,
    } = plan;
    output.directory_rename_conflicts.extend(conflicts);
    if !relocations.is_empty() {
        let occupied: HashSet<Vec<u8>> = relocations
            .values()
            .filter(|r| r.occupied.is_some())
            .map(|r| r.to.clone())
            .collect();
        for path in &occupied {
            output.conflict_content.remove(path);
        }
        let (moving, kept): (Vec<_>, Vec<_>) = std::mem::take(output.index.entries_mut())
            .into_iter()
            .partition(|entry| {
                relocations.contains_key(&entry.path) || occupied.contains(&entry.path)
            });
        *output.index.entries_mut() = kept;
        let mut moved_rows: BTreeMap<Vec<u8>, Vec<IndexEntry>> = BTreeMap::new();
        for entry in moving
            .into_iter()
            .filter(|entry| relocations.contains_key(&entry.path))
        {
            moved_rows
                .entry(entry.path.clone())
                .or_default()
                .push(entry);
        }
        for (from, relocation) in relocations {
            let Some(rows) = moved_rows.remove(&from) else {
                continue;
            };
            let moved_markers = output.conflict_content.remove(&from);
            let Relocation {
                to,
                added_by,
                edited_rename,
                occupied,
            } = relocation;
            let unmerged = rows.iter().any(|row| row.stage() != 0);
            match (occupied, rows.as_slice()) {
                (Some(occupant), [moved]) if !unmerged => {
                    let paths = [from.as_slice(), to.as_slice()];
                    merge_at_destination(output, paths, added_by, &occupant, moved, mode, merge)?;
                }
                _ => {
                    move_rows(output, &to, added_by, &rows, edited_rename.as_ref(), mode);
                    if let Some(blob) = moved_markers {
                        output.conflict_content.insert(to.clone(), blob);
                    }
                }
            }
            output.touched_paths.insert(to.clone());
            if mode == DirectoryRenameMode::Conflict {
                output
                    .directory_rename_conflicts
                    .push(DirectoryRenameConflict::Moved {
                        from: from.clone(),
                        to,
                        added_by,
                    });
            }
            renamed_away.insert(from);
        }
        output.index.sort();
    }
    output
        .directory_rename_conflicts
        .sort_by(|a, b| a.path().cmp(b.path()));
    Ok(())
}

/// Put a moved file's rows at `to`, the destination being free on every side.
fn move_rows(
    output: &mut TreeMergeOutput,
    to: &[u8],
    added_by: MergeSide,
    rows: &[IndexEntry],
    edited_rename: Option<&[IndexEntry; 3]>,
    mode: DirectoryRenameMode,
) {
    let unmerged = rows.iter().any(|row| row.stage() != 0);
    if let (DirectoryRenameMode::Conflict, false, Some(stages), [merged]) =
        (mode, unmerged, edited_rename, rows)
    {
        output.conflict_content.insert(to.to_vec(), merged.oid);
        for (stage, entry) in (1..=3).zip(stages) {
            stage_entry(&mut output.index, to, entry, stage);
        }
        return;
    }
    for row in rows {
        let stage = match mode {
            DirectoryRenameMode::Conflict if !unmerged => {
                // Like a modify/delete conflict: the work tree gets the only version.
                output.conflict_content.insert(to.to_vec(), row.oid);
                added_by.index_stage()
            }
            _ => row.stage(),
        };
        stage_entry(&mut output.index, to, row, stage);
    }
}

/// Three-way merge of a moved file, as the adding side's version, with what the base and the
/// renaming side have at `to`. In [`DirectoryRenameMode::Conflict`] a clean result still
/// leaves every version unmerged (with the merged blob in the work tree), as Git does.
///
/// `paths` is `[from, to]`; conflict markers name each side's own path for the file.
fn merge_at_destination(
    output: &mut TreeMergeOutput,
    paths: [&[u8]; 2],
    added_by: MergeSide,
    occupant: &Occupant,
    moved: &IndexEntry,
    mode: DirectoryRenameMode,
    merge: &DestinationMerge<'_>,
) -> crate::error::Result<()> {
    let [from, to] = paths;
    let (ours, theirs, ours_path, theirs_path) = match added_by {
        MergeSide::Ours => (Some(moved), occupant.renamer.as_ref(), from, to),
        MergeSide::Theirs => (occupant.renamer.as_ref(), Some(moved), to, from),
    };
    let label_ours = format!(
        "{}:{}",
        merge.presentation.label_ours,
        String::from_utf8_lossy(ours_path)
    );
    let theirs_path = String::from_utf8_lossy(theirs_path);
    let label_theirs = match merge.presentation.label_theirs {
        TheirsConflictLabel::PathUtf8 => theirs_path.into_owned(),
        TheirsConflictLabel::Fixed(label) => format!("{label}:{theirs_path}"),
    };
    let presentation = TreeMergeConflictPresentation {
        label_ours: &label_ours,
        label_theirs: TheirsConflictLabel::Fixed(&label_theirs),
        ..merge.presentation
    };
    let mut index = Index::new();
    let mut markers = BTreeMap::new();
    merge_one_path(
        merge.ctx,
        &mut index,
        &mut markers,
        to,
        occupant.base.as_ref(),
        ours,
        theirs,
        merge.favor,
        merge.ws,
        merge.diff_algorithm,
        presentation,
    )?;
    let clean = markers.is_empty() && index.entries().iter().all(|e| e.stage() == 0);
    if mode == DirectoryRenameMode::Conflict && clean {
        if let Some(merged) = index.entries().first() {
            output.conflict_content.insert(to.to_vec(), merged.oid);
        }
        for (stage, entry) in [(1, occupant.base.as_ref()), (2, ours), (3, theirs)] {
            if let Some(entry) = entry {
                stage_entry(&mut output.index, to, entry, stage);
            }
        }
        return Ok(());
    }
    for entry in index.entries() {
        output.index.push_entry_unsorted(entry.clone());
    }
    output.conflict_content.extend(markers);
    Ok(())
}

/// Deepest ancestor directory of `path` with a rename in `dirs`, as `(old dir, new dir)`.
fn renamed_ancestor<'d>(
    path: &[u8],
    dirs: &HashMap<&'d [u8], &'d [u8]>,
) -> Option<(&'d [u8], &'d [u8])> {
    let mut dir = path;
    while !dir.is_empty() {
        dir = split_last(dir).0;
        if let Some((&old, &new)) = dirs.get_key_value(dir) {
            return Some((old, new));
        }
    }
    None
}

/// `path` (under `old_dir/`) with `old_dir` replaced by `new_dir`; an empty `new_dir` is the
/// repository root.
fn moved_path(path: &[u8], old_dir: &[u8], new_dir: &[u8]) -> Vec<u8> {
    let rest = &path[old_dir.len() + 1..];
    if new_dir.is_empty() {
        return rest.to_vec();
    }
    let mut out = Vec::with_capacity(new_dir.len() + 1 + rest.len());
    out.extend_from_slice(new_dir);
    out.push(b'/');
    out.extend_from_slice(rest);
    out
}

/// Split `path` at its last `/` into `(parent, name)`; the parent of a top-level name is empty.
fn split_last(path: &[u8]) -> (&[u8], &[u8]) {
    match path.iter().rposition(|&b| b == b'/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => (&[], path),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::super::{
        merge_trees_three_way, DirectoryRenameConflict, DirectoryRenameMode, MergeSide,
        TreeMergeConflictPresentation, TreeMergeOutput,
    };
    use crate::index::{Index, IndexEntry, MODE_REGULAR};
    use crate::merge_file::MergeFavor;
    use crate::objects::{ObjectId, ObjectKind};
    use crate::operation::{conflicts_from_tree_merge, tree_merge_has_conflicts, ConflictKind};
    use crate::repo::{init_repository, Repository};
    use crate::write_tree::write_tree_from_index;

    fn body(tag: &str) -> String {
        (1..=20).map(|n| format!("{tag} {n}\n")).collect()
    }

    /// Tree of `files`; each body is derived from the file name, or from `tag` in `path=tag`.
    fn tree(repo: &Repository, files: &[&str]) -> ObjectId {
        let mut index = Index::new();
        for &file in files {
            let (path, name) = match file.split_once('=') {
                Some((path, tag)) => (path, tag),
                None => (file, file.rsplit('/').next().unwrap()),
            };
            let oid = repo
                .odb()
                .write(ObjectKind::Blob, body(name).as_bytes())
                .unwrap();
            index.add_or_replace(IndexEntry {
                ctime_sec: 0,
                ctime_nsec: 0,
                mtime_sec: 0,
                mtime_nsec: 0,
                dev: 0,
                ino: 0,
                mode: MODE_REGULAR,
                uid: 0,
                gid: 0,
                size: 0,
                oid,
                flags: path.len() as u16,
                flags_extended: None,
                path: path.as_bytes().to_vec(),
                base_index_pos: 0,
            });
        }
        index.sort();
        write_tree_from_index(repo.odb(), &index, "").unwrap()
    }

    struct Case {
        _dir: tempfile::TempDir,
        repo: Repository,
    }

    fn case() -> Case {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repository(
            dir.path(),
            true,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .unwrap();
        Case { _dir: dir, repo }
    }

    impl Case {
        fn merge(
            &self,
            base: &[&str],
            ours: &[&str],
            theirs: &[&str],
            mode: DirectoryRenameMode,
        ) -> TreeMergeOutput {
            let repo = &self.repo;
            merge_trees_three_way(
                repo,
                tree(repo, base),
                tree(repo, ours),
                tree(repo, theirs),
                MergeFavor::None,
                Default::default(),
                None,
                TreeMergeConflictPresentation::default(),
                mode,
            )
            .unwrap()
        }

        /// Files of the tree a merge records, conflict-marker and conflict-only blobs included.
        fn files(&self, merged: &TreeMergeOutput) -> Vec<String> {
            let tree =
                super::super::materialized_merge_result_tree(self.repo.odb(), merged).unwrap();
            crate::diff::diff_trees(self.repo.odb(), None, Some(&tree), "")
                .unwrap()
                .into_iter()
                .filter_map(|entry| entry.new_path)
                .map(|path| path.to_string())
                .collect()
        }
    }

    fn unmerged(merged: &TreeMergeOutput) -> Vec<(String, u8)> {
        merged
            .index
            .entries()
            .iter()
            .filter(|e| e.stage() != 0)
            .map(|e| (String::from_utf8_lossy(&e.path).into_owned(), e.stage()))
            .collect()
    }

    const BASE: &[&str] = &["a/x", "a/y", "a/z", "keep"];
    const OURS_RENAMED: &[&str] = &["b/x", "b/y", "b/z", "keep"];
    const THEIRS_ADDED: &[&str] = &["a/new", "a/x", "a/y", "a/z", "keep"];

    #[test]
    fn conflict_mode_moves_the_file_and_leaves_it_unmerged() {
        let c = case();
        let merged = c.merge(
            BASE,
            OURS_RENAMED,
            THEIRS_ADDED,
            DirectoryRenameMode::Conflict,
        );
        assert_eq!(c.files(&merged), ["b/new", "b/x", "b/y", "b/z", "keep"]);
        assert_eq!(unmerged(&merged), [("b/new".to_owned(), 3)]);
        assert_eq!(
            merged.directory_rename_conflicts,
            [DirectoryRenameConflict::Moved {
                from: b"a/new".to_vec(),
                to: b"b/new".to_vec(),
                added_by: MergeSide::Theirs,
            }]
        );
        let conflicts = conflicts_from_tree_merge(&merged);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].path, "b/new");
        assert_eq!(conflicts[0].kind.as_str(), "directory_rename_moved");
        assert!(tree_merge_has_conflicts(&merged));
    }

    #[test]
    fn apply_mode_moves_the_file_cleanly() {
        let c = case();
        let merged = c.merge(BASE, OURS_RENAMED, THEIRS_ADDED, DirectoryRenameMode::Apply);
        assert_eq!(c.files(&merged), ["b/new", "b/x", "b/y", "b/z", "keep"]);
        assert!(unmerged(&merged).is_empty());
        assert!(!tree_merge_has_conflicts(&merged));
    }

    #[test]
    fn off_mode_leaves_the_file_in_the_old_directory() {
        let c = case();
        let merged = c.merge(BASE, OURS_RENAMED, THEIRS_ADDED, DirectoryRenameMode::Off);
        assert_eq!(c.files(&merged), ["a/new", "b/x", "b/y", "b/z", "keep"]);
        assert!(!tree_merge_has_conflicts(&merged));
    }

    #[test]
    fn rename_on_their_side_moves_our_new_file_to_stage_two() {
        let c = case();
        let merged = c.merge(
            BASE,
            THEIRS_ADDED,
            OURS_RENAMED,
            DirectoryRenameMode::Conflict,
        );
        assert_eq!(c.files(&merged), ["b/new", "b/x", "b/y", "b/z", "keep"]);
        assert_eq!(unmerged(&merged), [("b/new".to_owned(), 2)]);
    }

    #[test]
    fn a_tie_between_destinations_is_a_split_and_moves_nothing() {
        let c = case();
        for mode in [DirectoryRenameMode::Conflict, DirectoryRenameMode::Apply] {
            let merged = c.merge(BASE, &["b/x", "c/y", "keep"], THEIRS_ADDED, mode);
            assert_eq!(c.files(&merged), ["a/new", "b/x", "c/y", "keep"]);
            assert!(unmerged(&merged).is_empty());
            assert_eq!(
                merged.directory_rename_conflicts,
                [DirectoryRenameConflict::Split {
                    dir: b"a".to_vec(),
                    renamed_by: MergeSide::Ours,
                }]
            );
            let conflicts = conflicts_from_tree_merge(&merged);
            assert_eq!(conflicts[0].path, "a");
            assert_eq!(conflicts[0].kind, ConflictKind::DirectoryRenameSplit);
            assert!(tree_merge_has_conflicts(&merged));
        }
        let merged = c.merge(
            BASE,
            &["b/x", "c/y", "keep"],
            THEIRS_ADDED,
            DirectoryRenameMode::Off,
        );
        assert!(!tree_merge_has_conflicts(&merged));
    }

    #[test]
    fn a_split_with_nothing_added_is_not_reported() {
        let c = case();
        let merged = c.merge(
            BASE,
            &["b/x", "c/y", "keep"],
            &["a/x", "a/y", "a/z"],
            DirectoryRenameMode::Conflict,
        );
        assert!(merged.directory_rename_conflicts.is_empty());
    }

    #[test]
    fn the_most_popular_destination_wins_without_a_majority() {
        let c = case();
        let base = ["a/w", "a/x", "a/y", "a/z"];
        let merged = c.merge(
            &base,
            &["b/x", "b/y", "c/z", "d/w"],
            &["a/new", "a/w", "a/x", "a/y", "a/z"],
            DirectoryRenameMode::Apply,
        );
        assert_eq!(c.files(&merged), ["b/new", "b/x", "b/y", "c/z", "d/w"]);
    }

    #[test]
    fn a_directory_that_still_has_files_is_not_renamed() {
        let c = case();
        let merged = c.merge(
            BASE,
            &["a/z", "b/x", "b/y", "keep"],
            THEIRS_ADDED,
            DirectoryRenameMode::Conflict,
        );
        assert_eq!(c.files(&merged), ["a/new", "a/z", "b/x", "b/y", "keep"]);
        assert!(!tree_merge_has_conflicts(&merged));
    }

    #[test]
    fn an_existing_file_blocks_the_move() {
        let c = case();
        let merged = c.merge(
            BASE,
            OURS_RENAMED,
            &["a/new", "a/x", "a/y", "a/z", "b/new", "keep"],
            DirectoryRenameMode::Apply,
        );
        assert_eq!(
            c.files(&merged),
            ["a/new", "b/new", "b/x", "b/y", "b/z", "keep"]
        );
        assert!(unmerged(&merged).is_empty());
        assert_eq!(
            merged.directory_rename_conflicts,
            [DirectoryRenameConflict::Blocked {
                path: b"b/new".to_vec(),
                sources: vec![b"a/new".to_vec()],
                existing: true,
            }]
        );
        assert!(tree_merge_has_conflicts(&merged));
    }

    #[test]
    fn a_destination_both_sides_deleted_is_merged_as_a_modify_delete() {
        let c = case();
        let merged = c.merge(
            &["a/x", "a/y", "b/new=old"],
            &["b/x", "b/y"],
            &["a/new", "a/x", "a/y"],
            DirectoryRenameMode::Apply,
        );
        assert_eq!(c.files(&merged), ["b/new", "b/x", "b/y"]);
        assert_eq!(
            unmerged(&merged),
            [("b/new".to_owned(), 1), ("b/new".to_owned(), 3)]
        );
        assert!(merged.directory_rename_conflicts.is_empty());
    }

    #[test]
    fn a_destination_the_renaming_side_kept_takes_the_moved_file() {
        let c = case();
        let merged = c.merge(
            &["a/x", "a/y", "b/new=old"],
            &["b/new=old", "b/x", "b/y"],
            &["a/new", "a/x", "a/y"],
            DirectoryRenameMode::Apply,
        );
        assert_eq!(c.files(&merged), ["b/new", "b/x", "b/y"]);
        assert!(!tree_merge_has_conflicts(&merged));
    }

    #[test]
    fn two_new_files_claiming_one_destination_block_each_other() {
        let c = case();
        let merged = c.merge(
            &["a/x", "c/c"],
            &["b/c", "b/x"],
            &["a/n", "a/x", "c/c", "c/n"],
            DirectoryRenameMode::Apply,
        );
        assert_eq!(c.files(&merged), ["a/n", "b/c", "b/x", "c/n"]);
        assert_eq!(
            merged.directory_rename_conflicts,
            [DirectoryRenameConflict::Blocked {
                path: b"b/n".to_vec(),
                sources: vec![b"a/n".to_vec(), b"c/n".to_vec()],
                existing: false,
            }]
        );
    }

    #[test]
    fn a_directory_renamed_on_both_sides_is_left_to_file_renames() {
        let c = case();
        let merged = c.merge(
            BASE,
            OURS_RENAMED,
            &["c/x", "c/y", "c/z", "keep"],
            DirectoryRenameMode::Conflict,
        );
        assert!(merged.directory_rename_conflicts.is_empty());
        assert_eq!(merged.rename_conflicts.len(), 3);
    }

    #[test]
    fn a_new_subdirectory_alone_does_not_trigger_the_rename() {
        let c = case();
        let mut theirs = BASE.to_vec();
        theirs.push("a/d/e/n");
        let merged = c.merge(BASE, OURS_RENAMED, &theirs, DirectoryRenameMode::Conflict);
        assert_eq!(c.files(&merged), ["a/d/e/n", "b/x", "b/y", "b/z", "keep"]);
        assert!(!tree_merge_has_conflicts(&merged));
    }

    #[test]
    fn nested_new_files_follow_once_the_directory_is_renamed() {
        let c = case();
        let mut theirs = THEIRS_ADDED.to_vec();
        theirs.push("a/d/e/n");
        let merged = c.merge(BASE, OURS_RENAMED, &theirs, DirectoryRenameMode::Apply);
        assert_eq!(
            c.files(&merged),
            ["b/d/e/n", "b/new", "b/x", "b/y", "b/z", "keep"]
        );
    }

    #[test]
    fn a_renamed_parent_directory_moves_files_in_its_subdirectories() {
        let c = case();
        let merged = c.merge(
            &["p/a/x", "p/a/y", "p/q"],
            &["r/a/x", "r/a/y", "r/q"],
            &["p/a/new", "p/a/x", "p/a/y", "p/new", "p/q"],
            DirectoryRenameMode::Apply,
        );
        assert_eq!(
            c.files(&merged),
            ["r/a/new", "r/a/x", "r/a/y", "r/new", "r/q"]
        );
    }

    #[test]
    fn a_file_renamed_into_the_old_directory_follows_it() {
        let c = case();
        let merged = c.merge(
            &["a/x", "a/y", "q/f"],
            &["b/x", "b/y", "q/f"],
            &["a/f", "a/x", "a/y"],
            DirectoryRenameMode::Conflict,
        );
        assert_eq!(c.files(&merged), ["b/f", "b/x", "b/y"]);
        assert_eq!(unmerged(&merged), [("b/f".to_owned(), 3)]);
    }

    #[test]
    fn moving_into_the_repository_root() {
        let c = case();
        let merged = c.merge(
            &["a/x", "a/y"],
            &["x", "y"],
            &["a/new", "a/x", "a/y"],
            DirectoryRenameMode::Apply,
        );
        assert_eq!(c.files(&merged), ["new", "x", "y"]);
    }

    #[test]
    fn config_values() {
        for (value, mode) in [
            ("conflict", DirectoryRenameMode::Conflict),
            ("Conflict", DirectoryRenameMode::Conflict),
            ("true", DirectoryRenameMode::Apply),
            ("yes", DirectoryRenameMode::Apply),
            ("1", DirectoryRenameMode::Apply),
            ("false", DirectoryRenameMode::Off),
            ("off", DirectoryRenameMode::Off),
            ("", DirectoryRenameMode::Off),
        ] {
            assert_eq!(
                DirectoryRenameMode::from_config_value(value),
                Ok(mode),
                "{value}"
            );
        }
        assert_eq!(
            DirectoryRenameMode::from_config_value("sometimes"),
            Err(crate::error::ConfigError::InvalidDirectoryRenames {
                value: "sometimes".into()
            })
        );
        assert_eq!(
            DirectoryRenameMode::default(),
            DirectoryRenameMode::Conflict
        );
    }
}
