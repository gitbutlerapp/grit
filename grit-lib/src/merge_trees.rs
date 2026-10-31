//! Rename-aware three-way tree merge for cherry-pick / revert style merges.
//!
//! Flattens trees to path → [`IndexEntry`] maps, aligns paths using rename detection
//! between base↔ours and base↔theirs (same idea as Git's merge-ort rename paths), then
//! runs path-by-path three-way rules with optional content merge.

use std::cell::OnceCell;
use std::collections::{BTreeMap, HashMap, HashSet};

#[path = "merge_tree_cache.rs"]
mod merge_tree_cache;
pub(crate) use merge_tree_cache::{lookup_index_entry_at_path_cached, MergeTreeCache};
#[path = "merge_dir_renames.rs"]
mod merge_dir_renames;
#[path = "merge_trees_walk.rs"]
mod merge_trees_walk;

use crate::config::ConfigSet;
use crate::crlf::{DiffAttr, FileAttrs, MergeAttr, TextAttr};
use crate::diff::{detect_renames, diff_trees, DiffStatus};
use crate::error::ConfigError;
use crate::index::{
    entry_from_stat, worktree_path_from_index_rel, Index, IndexEntry, MODE_REGULAR,
};
use crate::merge_driver::{try_merge_driver, MergeDriverLabels, MergeDriverOutcome};
use crate::merge_file::{is_binary, merge, ConflictStyle, MergeFavor, MergeInput};
use crate::objects::{ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::repo::Repository;
use crate::worktree_rules::WorktreeRules;
use crate::write_tree::write_tree_from_index;

type RenameTriple = (Vec<u8>, Vec<u8>, u32);

/// Result of merging three trees with optional conflict-marker blobs for checkout.
#[derive(Debug)]
pub struct TreeMergeOutput {
    /// Merged index entries for touched paths (may include unmerged stages).
    pub index: Index,
    /// Conflict-marker blob OIDs keyed by path (stage-2 path bytes).
    pub conflict_content: BTreeMap<Vec<u8>, ObjectId>,
    /// Result tree OID (clean merge layout; conflict paths use stage-0 entries from [`Self::index`]).
    pub result_tree: ObjectId,
    /// Repository-relative paths whose merge outcome differs from `ours_tree`.
    pub touched_paths: HashSet<Vec<u8>>,
    /// Base paths renamed to a different destination on each side, sorted by base path.
    ///
    /// Each one leaves the base entry at stage 1 under [`RenameRenameConflict::base_path`],
    /// stage 2 under [`RenameRenameConflict::ours_path`] and stage 3 under
    /// [`RenameRenameConflict::theirs_path`], with the merged content at both destinations.
    pub rename_conflicts: Vec<RenameRenameConflict>,
    /// Directory renames that need the user's attention, sorted by [`DirectoryRenameConflict::path`].
    ///
    /// [`DirectoryRenameConflict::Moved`] paths also have unmerged index rows; split and
    /// blocked directory renames have none, yet still make the merge conflicted.
    pub directory_rename_conflicts: Vec<DirectoryRenameConflict>,
}

/// How a three-way merge treats files one side adds inside a directory the other side renamed.
///
/// A directory counts as renamed when one side moved all of its files out of it and most of
/// them (the destination with the most files, without a tie) into one other directory. Files the
/// other side added to the old directory, or renamed into it, then most likely belong in the new
/// one. Read from the `merge.directoryRenames` config with [`Self::from_config`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DirectoryRenameMode {
    /// Leave such files where the adding side put them (`merge.directoryRenames=false`).
    Off,
    /// Move them into the renamed directory as a clean merge (`true`).
    Apply,
    /// Move them, but leave each one unmerged at the adding side's stage so the user confirms
    /// the new location (`conflict`; Git's default).
    #[default]
    Conflict,
}

impl DirectoryRenameMode {
    /// Config key holding the mode.
    pub const CONFIG_KEY: &'static str = "merge.directoryRenames";

    /// Parse a `merge.directoryRenames` value: `conflict` or a Git boolean (`true` applies the
    /// renames, `false` turns them off).
    ///
    /// # Errors
    ///
    /// [`ConfigError::InvalidDirectoryRenames`] for any other value.
    pub fn from_config_value(value: &str) -> std::result::Result<Self, ConfigError> {
        if value.eq_ignore_ascii_case("conflict") {
            return Ok(Self::Conflict);
        }
        match crate::config::parse_bool(value) {
            Ok(true) => Ok(Self::Apply),
            Ok(false) => Ok(Self::Off),
            Err(_) => Err(ConfigError::InvalidDirectoryRenames {
                value: value.to_owned(),
            }),
        }
    }

    /// The mode set by [`Self::CONFIG_KEY`] in `config`, or [`Self::Conflict`] when unset.
    ///
    /// # Errors
    ///
    /// [`ConfigError::InvalidDirectoryRenames`] when the value is not recognised.
    pub fn from_config(config: &ConfigSet) -> std::result::Result<Self, ConfigError> {
        config
            .get(Self::CONFIG_KEY)
            .map_or(Ok(Self::default()), |value| Self::from_config_value(&value))
    }

    /// [`Self::from_config`] for the configuration of `repo`.
    ///
    /// # Errors
    ///
    /// Config load failures, and [`ConfigError::InvalidDirectoryRenames`].
    pub fn from_repo(repo: &Repository) -> crate::error::Result<Self> {
        let config = repo.config()?;
        Ok(Self::from_config(&config)?)
    }
}

/// One side of a three-way merge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MergeSide {
    /// The side merged into (index stage 2).
    Ours,
    /// The side merged in (index stage 3).
    Theirs,
}

impl MergeSide {
    /// Index stage holding this side's version of an unmerged path (2 or 3).
    #[must_use]
    pub fn index_stage(self) -> u8 {
        match self {
            Self::Ours => 2,
            Self::Theirs => 3,
        }
    }
}

/// A directory rename that a merge could not settle on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectoryRenameConflict {
    /// A file was moved into the renamed directory and left unmerged
    /// ([`DirectoryRenameMode::Conflict`]) so the user can confirm its new location.
    Moved {
        /// Path the adding side gave the file, inside the old directory.
        from: Vec<u8>,
        /// Path in the renamed directory, where the merge put it.
        to: Vec<u8>,
        /// Side that added the file (or renamed it into the old directory).
        added_by: MergeSide,
    },
    /// A side spread a directory's files over several directories with no single most popular
    /// destination, so nothing added to it on the other side was moved.
    Split {
        /// The directory in the merge base.
        dir: Vec<u8>,
        /// Side that emptied the directory.
        renamed_by: MergeSide,
    },
    /// New files were not moved into a renamed directory because their destination is taken on
    /// the side that added them, or claimed by several of them. (A file the base or the renaming
    /// side has there does not block the move; the moved file is merged with it.)
    Blocked {
        /// Destination the directory rename gives the files.
        path: Vec<u8>,
        /// The new files that would have moved there, sorted; they stay where they were added.
        sources: Vec<Vec<u8>>,
        /// `true` when the adding side already has a file or directory at
        /// [`Self::Blocked::path`] (or the renaming side has a directory there); `false`
        /// when several new files would land on it.
        existing: bool,
    },
}

impl DirectoryRenameConflict {
    /// The path the conflict is reported at: the new location for [`Self::Moved`], the old
    /// directory for [`Self::Split`] and the blocked destination for [`Self::Blocked`].
    #[must_use]
    pub fn path(&self) -> &[u8] {
        match self {
            Self::Moved { to, .. } => to,
            Self::Split { dir, .. } => dir,
            Self::Blocked { path, .. } => path,
        }
    }
}

/// One base path that each side of a merge renamed to a different destination.
///
/// Git calls this a rename/rename (1to2) conflict: the merge cannot pick a name, so both
/// destinations are kept and the path stays unmerged until the user resolves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameRenameConflict {
    /// Path of the file in the merge base.
    pub base_path: Vec<u8>,
    /// Destination the ours side renamed it to.
    pub ours_path: Vec<u8>,
    /// Destination the theirs side renamed it to.
    pub theirs_path: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default)]
/// `WhitespaceMergeOptions` struct.
pub struct WhitespaceMergeOptions {
    /// `ignore_all_space` field.
    pub ignore_all_space: bool,
    /// `ignore_space_change` field.
    pub ignore_space_change: bool,
    /// `ignore_space_at_eol` field.
    pub ignore_space_at_eol: bool,
    /// `ignore_cr_at_eol` field.
    pub ignore_cr_at_eol: bool,
}

/// How to label the "theirs" side in textual conflict markers.
#[derive(Clone, Copy, Debug)]
pub enum TheirsConflictLabel<'a> {
    /// Use the UTF-8 file path (matches cherry-pick / revert).
    PathUtf8,
    /// Fixed label (e.g. `local` for `git checkout -m`).
    Fixed(&'a str),
}

/// Labels and marker style for conflict output during tree merges.
#[derive(Clone, Copy, Debug)]
pub struct TreeMergeConflictPresentation<'a> {
    /// Label after `<<<<<<<` (Git: ours side name).
    pub label_ours: &'a str,
    /// Label after `>>>>>>>` (path-based or fixed).
    pub label_theirs: TheirsConflictLabel<'a>,
    /// Label after `|||||||` in diff3 output.
    pub label_base: &'a str,
    /// Two-way vs diff3 markers.
    pub style: ConflictStyle,
    /// `git checkout -m`: always leave unmerged index entries when ours ≠ theirs.
    pub checkout_merge: bool,
    /// Ours-side commit oid for `path~<commit>` when relocating a blocking **ours** file.
    pub df_directory_ours_commit: Option<ObjectId>,
    /// Theirs-side commit oid for `path~<commit>` when relocating a blocking **theirs** file.
    pub df_directory_theirs_commit: Option<ObjectId>,
}

impl Default for TreeMergeConflictPresentation<'_> {
    fn default() -> Self {
        Self {
            label_ours: "HEAD",
            label_theirs: TheirsConflictLabel::PathUtf8,
            label_base: "merged common ancestors",
            style: ConflictStyle::Merge,
            checkout_merge: false,
            df_directory_ours_commit: None,
            df_directory_theirs_commit: None,
        }
    }
}

/// Conflict-marker labels for replaying one commit onto a new base (pick and rebase).
///
/// The incoming side is named `"<abbrev> (<subject>)"` and the diff3 base
/// `"parent of <abbrev> (<subject>)"`, the labels Git uses for the same replays, so each
/// stop of a multi-commit replay names the commit it came from. Ours stays `HEAD`.
pub(crate) struct ReplayConflictLabels {
    theirs: String,
    base: String,
}

impl ReplayConflictLabels {
    /// Labels for replaying `commit`, whose message subject line is `subject`.
    pub(crate) fn new(commit: ObjectId, subject: &str) -> Self {
        let theirs = format!("{} ({subject})", &commit.to_hex()[..7]);
        let base = format!("parent of {theirs}");
        Self { theirs, base }
    }

    /// Presentation using these labels with marker `style`; other fields keep their defaults.
    pub(crate) fn presentation(&self, style: ConflictStyle) -> TreeMergeConflictPresentation<'_> {
        TreeMergeConflictPresentation {
            label_theirs: TheirsConflictLabel::Fixed(&self.theirs),
            label_base: &self.base,
            style,
            ..TreeMergeConflictPresentation::default()
        }
    }
}

#[expect(clippy::too_many_arguments)]
/// Merge `base` + `ours` + `their` trees (by OID) into an index using rename detection.
///
/// Paths in the result follow the **ours** tree naming. Rename detection uses a 50%
/// similarity threshold, matching Git's default rename detection during merges.
///
/// A rename onto a path the other side also fills with a different file (two files renamed
/// to one name, or a rename onto an added file) is an add/add conflict at that path, as in
/// Git: stage 2 and stage 3 hold each side's file, a renamed one already merged with the other
/// side's edits of its source (conflict markers included when those overlap), and the old
/// names are gone. Nothing is dropped until the user resolves the path.
///
/// # Parameters
///
/// - `base_tree` — common ancestor tree (parent tree for cherry-pick, reverted commit tree for revert).
/// - `ours_tree` — current branch tree (HEAD during pick/revert).
/// - `theirs_tree` — tree being applied (picked commit for cherry-pick, parent of reverted commit for revert).
/// - `favor` / `ws` — merge strategy favour and whitespace options for textual merges.
/// - `diff_algorithm` — optional line diff algorithm name used for textual merges.
/// - `presentation` — conflict marker labels and style (`label_base` is the diff3 "base" name).
/// - `directory_renames` — whether files one side added inside a directory the other side
///   renamed follow the rename ([`DirectoryRenameMode`]; callers usually pass
///   [`DirectoryRenameMode::from_repo`]). Conflicts are reported in
///   [`TreeMergeOutput::directory_rename_conflicts`].
///
/// # Errors
///
/// Returns [`crate::error::Error`] on ODB read failures or corrupt trees.
pub fn merge_trees_three_way(
    repo: &Repository,
    base_tree: ObjectId,
    ours_tree: ObjectId,
    theirs_tree: ObjectId,
    favor: MergeFavor,
    ws: WhitespaceMergeOptions,
    diff_algorithm: Option<&str>,
    presentation: TreeMergeConflictPresentation<'_>,
    directory_renames: DirectoryRenameMode,
) -> crate::error::Result<TreeMergeOutput> {
    merge_trees_three_way_cached(
        repo,
        &mut MergeTreeCache::default(),
        base_tree,
        ours_tree,
        theirs_tree,
        favor,
        ws,
        diff_algorithm,
        presentation,
        directory_renames,
    )
}

/// Like [`merge_trees_three_way`], reusing parsed-tree cache across calls (e.g. replay sequences).
#[expect(clippy::too_many_arguments)]
pub(crate) fn merge_trees_three_way_cached(
    repo: &Repository,
    cache: &mut MergeTreeCache,
    base_tree: ObjectId,
    ours_tree: ObjectId,
    theirs_tree: ObjectId,
    favor: MergeFavor,
    ws: WhitespaceMergeOptions,
    diff_algorithm: Option<&str>,
    presentation: TreeMergeConflictPresentation<'_>,
    directory_renames: DirectoryRenameMode,
) -> crate::error::Result<TreeMergeOutput> {
    merge_trees_at_depth(
        repo,
        cache,
        base_tree,
        ours_tree,
        theirs_tree,
        favor,
        ws,
        diff_algorithm,
        presentation,
        directory_renames,
        0,
        None,
    )
}

/// Precomputed fork→onto file renames for [`merge_trees_replay_transplant`].
pub(crate) struct ReplayUpstreamRenames<'a> {
    pub cached_ours_old_to_new: &'a HashMap<Vec<u8>, Vec<u8>>,
    pub fork_onto_rename_triples: &'a [(Vec<u8>, Vec<u8>, u32)],
    pub upstream_touched: &'a HashSet<Vec<u8>>,
    pub commit_theirs_diff: Vec<crate::diff::DiffEntry>,
}

/// Three-way merge for replay transplant when upstream fork→onto renames are precomputed.
///
/// Skips diffing `base_tree`→`ours_tree` (the expensive upstream delta) and reuses
/// `cached_ours_old_to_new` for rename alignment. `commit_theirs_diff` must be `base_tree`→`theirs_tree`.
#[expect(clippy::too_many_arguments)]
pub(crate) fn merge_trees_replay_transplant(
    repo: &Repository,
    cache: &mut MergeTreeCache,
    base_tree: ObjectId,
    ours_tree: ObjectId,
    theirs_tree: ObjectId,
    commit_theirs_diff: Vec<crate::diff::DiffEntry>,
    cached_ours_old_to_new: &HashMap<Vec<u8>, Vec<u8>>,
    fork_onto_rename_triples: &[(Vec<u8>, Vec<u8>, u32)],
    upstream_touched: &HashSet<Vec<u8>>,
    favor: MergeFavor,
    ws: WhitespaceMergeOptions,
    diff_algorithm: Option<&str>,
    presentation: TreeMergeConflictPresentation<'_>,
    directory_renames: DirectoryRenameMode,
) -> crate::error::Result<TreeMergeOutput> {
    merge_trees_at_depth(
        repo,
        cache,
        base_tree,
        ours_tree,
        theirs_tree,
        favor,
        ws,
        diff_algorithm,
        presentation,
        directory_renames,
        0,
        Some(ReplayUpstreamRenames {
            cached_ours_old_to_new,
            fork_onto_rename_triples,
            upstream_touched,
            commit_theirs_diff,
        }),
    )
}

fn paths_overlap_for_merge(a: &[u8], b: &[u8]) -> bool {
    a.starts_with(b) && (a.len() == b.len() || a.get(b.len()) == Some(&b'/'))
        || b.starts_with(a) && (b.len() == a.len() || b.get(a.len()) == Some(&b'/'))
}

fn map_path_dir_prefix(path: &[u8], dir_map: &HashMap<Vec<u8>, Vec<u8>>) -> Option<Vec<u8>> {
    let mut best: Option<(usize, &Vec<u8>, &Vec<u8>)> = None;
    for (old_dir, new_dir) in dir_map {
        if path.starts_with(old_dir)
            && (path.len() == old_dir.len() || path.get(old_dir.len()) == Some(&b'/'))
        {
            let len = old_dir.len();
            if best.is_none_or(|(bl, _, _)| len > bl) {
                best = Some((len, old_dir, new_dir));
            }
        }
    }
    best.map(|(len, _old_dir, new_dir)| {
        let mut out = new_dir.clone();
        if path.len() > len {
            if !out.is_empty() {
                out.push(b'/');
            }
            out.extend_from_slice(&path[len + 1..]);
        }
        out
    })
}

/// Three-way tree merge at recursion level `call_depth` of a multi-base merge.
///
/// Level 0 is the merge the caller asked for. Higher levels merge merge bases into a
/// virtual base, and their result tree (not their conflict state) is what matters: every
/// conflict is settled to a single stage-0 entry, as Git's recursive merge does. Content
/// conflicts keep their markers (each level two characters wider, so nested markers stay
/// distinct), while modify/delete, binary, symlink, submodule and type-change conflicts keep
/// the base version. `favor` should be [`MergeFavor::None`] for levels above 0, where
/// directory renames are never applied.
#[expect(clippy::too_many_arguments)]
pub(crate) fn merge_trees_at_depth(
    repo: &Repository,
    cache: &mut MergeTreeCache,
    base_tree: ObjectId,
    ours_tree: ObjectId,
    theirs_tree: ObjectId,
    favor: MergeFavor,
    ws: WhitespaceMergeOptions,
    diff_algorithm: Option<&str>,
    presentation: TreeMergeConflictPresentation<'_>,
    directory_renames: DirectoryRenameMode,
    call_depth: usize,
    replay_upstream: Option<ReplayUpstreamRenames<'_>>,
) -> crate::error::Result<TreeMergeOutput> {
    use crate::porcelain::cherry_pick::directory_renames_from_file_renames;

    let directory_renames = if call_depth > 0 {
        DirectoryRenameMode::Off
    } else {
        directory_renames
    };
    let odb = &repo.odb();

    let (mut touched, mut ours_old_to_new_pairs, mut theirs_pairs) = if let Some(replay) =
        replay_upstream
    {
        let dir_map = directory_renames_from_file_renames(
            &replay
                .cached_ours_old_to_new
                .iter()
                .map(|(o, n)| (o.clone(), n.clone()))
                .collect::<Vec<_>>(),
        );
        let mut touched = paths_from_diff_entries(&replay.commit_theirs_diff);
        for path in touched.clone() {
            if let Some(new) = replay.cached_ours_old_to_new.get(&path) {
                touched.insert(new.clone());
            }
            if let Some(mapped) = map_path_dir_prefix(&path, &dir_map) {
                touched.insert(mapped);
            }
        }
        let commit_touched = touched.clone();
        if commit_touched.iter().any(|p| {
            replay.upstream_touched.contains(p) && !replay.cached_ours_old_to_new.contains_key(p)
        }) {
            touched.extend(replay.cached_ours_old_to_new.keys().cloned());
            touched.extend(replay.cached_ours_old_to_new.values().cloned());
            touched.extend(replay.upstream_touched.iter().cloned());
        } else {
            for path in replay.upstream_touched.iter() {
                if touched.iter().any(|t| paths_overlap_for_merge(t, path)) {
                    touched.insert(path.clone());
                }
            }
        }
        for (old, new) in replay.cached_ours_old_to_new {
            if old != new
                && (touched.contains(old)
                    || touched.contains(new)
                    || touched.iter().any(|t| paths_overlap_for_merge(t, old)))
            {
                touched.insert(old.clone());
                touched.insert(new.clone());
            }
        }
        let ours_old_to_new_pairs: Vec<RenameTriple> = replay
            .fork_onto_rename_triples
            .iter()
            .filter(|(old, new, _)| old != new && (touched.contains(old) || touched.contains(new)))
            .cloned()
            .collect();
        let theirs_pairs = rename_pairs_from_diff_entries(odb, replay.commit_theirs_diff)?;
        (touched, ours_old_to_new_pairs, theirs_pairs)
    } else {
        let ours_diff = diff_trees(odb, Some(&base_tree), Some(&ours_tree), "")?;
        let theirs_diff = diff_trees(odb, Some(&base_tree), Some(&theirs_tree), "")?;
        let mut touched = paths_from_diff_entries(&ours_diff);
        touched.extend(paths_from_diff_entries(&theirs_diff));
        let ours_old_to_new_pairs = rename_pairs_from_diff_entries(odb, ours_diff)?;
        let theirs_pairs = rename_pairs_from_diff_entries(odb, theirs_diff)?;
        (touched, ours_old_to_new_pairs, theirs_pairs)
    };

    let (base, mut ours, mut theirs) = merge_trees_walk::build_sparse_maps(
        cache,
        repo,
        base_tree,
        ours_tree,
        theirs_tree,
        &touched,
    )?;

    augment_renames_with_same_blob(&base, &ours, &mut ours_old_to_new_pairs);
    augment_renames_with_same_blob(&base, &theirs, &mut theirs_pairs);

    let mut df_escapes = DfEscapes::default();
    if presentation.df_directory_ours_commit.is_some()
        && presentation.df_directory_theirs_commit.is_some()
    {
        df_escapes = apply_df_file_directory_conflicts_trees(
            cache,
            repo,
            &base_tree,
            &ours_tree,
            &theirs_tree,
            &base,
            &mut ours,
            &mut theirs,
            &mut touched,
            presentation,
        )?;
    }

    let mut ours_new_to_old: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    let mut ours_best_by_dest: HashMap<Vec<u8>, (Vec<u8>, u32)> = HashMap::new();
    for (old, new, score) in &ours_old_to_new_pairs {
        let new_b = new.clone();
        let should_take = match ours_best_by_dest.get(&new_b) {
            None => true,
            Some((_, s)) => *score > *s,
        };
        if should_take {
            ours_best_by_dest.insert(new_b, (old.clone(), *score));
        }
    }
    for (new_path, (old_path, _)) in ours_best_by_dest {
        ours_new_to_old.insert(new_path, old_path);
    }
    let mut theirs_old_to_new: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    let mut best_by_dest: HashMap<Vec<u8>, (Vec<u8>, u32)> = HashMap::new();
    for (old, new, score) in theirs_pairs {
        let new_b = new.clone();
        let should_take = match best_by_dest.get(&new_b) {
            None => true,
            Some((_, s)) => score > *s,
        };
        if should_take {
            best_by_dest.insert(new_b, (old.clone(), score));
        }
    }
    for (new_path, (old_path, _)) in best_by_dest {
        theirs_old_to_new.insert(old_path, new_path);
    }

    for (old, new, _) in &ours_old_to_new_pairs {
        touched.insert(old.clone());
        touched.insert(new.clone());
    }
    for (old, new) in &theirs_old_to_new {
        touched.insert(old.clone());
        touched.insert(new.clone());
    }
    let rename_conflicts =
        rename_rename_conflicts(&ours_new_to_old, &theirs_old_to_new, &base, &ours, &theirs);
    let directory_rename_plan = if directory_renames == DirectoryRenameMode::Off {
        None
    } else {
        let ours_changes = merge_dir_renames::SideChanges {
            tree: ours_tree,
            files: &ours,
            renames: ours_new_to_old
                .iter()
                .filter(|(new, old)| new != old)
                .map(|(new, old)| (old.as_slice(), new.as_slice()))
                .collect(),
            new_paths: paths_missing_from(&ours, &base),
        };
        let theirs_changes = merge_dir_renames::SideChanges {
            tree: theirs_tree,
            files: &theirs,
            renames: theirs_old_to_new
                .iter()
                .filter(|(old, new)| old != new)
                .map(|(old, new)| (old.as_slice(), new.as_slice()))
                .collect(),
            new_paths: paths_missing_from(&theirs, &base),
        };
        Some(merge_dir_renames::plan_directory_renames(
            cache,
            odb,
            base_tree,
            &base,
            &ours_changes,
            &theirs_changes,
        )?)
    };

    drop(ours_old_to_new_pairs);
    let df_skip = df_escapes.skip_paths();
    let mut output = merge_trees_walk::three_way_on_touched_paths(
        cache,
        repo,
        base_tree,
        ours_tree,
        theirs_tree,
        &touched,
        &base,
        &ours,
        &theirs,
        &ours_new_to_old,
        &theirs_old_to_new,
        rename_conflicts,
        &df_skip,
        favor,
        ws,
        diff_algorithm,
        presentation,
        call_depth,
    )?;
    df_escapes.apply_to_output(&mut output);
    output.index.sort();
    drop((base, ours, theirs, touched));
    let mut renamed_away = HashSet::new();
    for (old, new) in &theirs_old_to_new {
        if old != new {
            renamed_away.insert(old.clone());
        }
    }
    for (new, old) in &ours_new_to_old {
        if new != old {
            renamed_away.insert(old.clone());
        }
    }
    drop((ours_new_to_old, theirs_old_to_new));
    if let Some(plan) = directory_rename_plan {
        merge_dir_renames::apply_directory_rename_plan(
            &mut output,
            plan,
            directory_renames,
            &mut renamed_away,
            &merge_dir_renames::DestinationMerge {
                ctx: &PathMergeContext::new(repo, call_depth),
                favor,
                ws,
                diff_algorithm,
                presentation,
            },
        )?;
    }
    output.result_tree = merge_trees_walk::finish_merge_result_tree(
        cache,
        odb,
        &output,
        base_tree,
        ours_tree,
        theirs_tree,
        &renamed_away,
    )?;
    Ok(output)
}

/// Paths of `side` that `base` has no file at.
fn paths_missing_from<'a>(
    side: &'a BTreeMap<Vec<u8>, IndexEntry>,
    base: &BTreeMap<Vec<u8>, IndexEntry>,
) -> Vec<&'a [u8]> {
    side.keys()
        .filter(|path| !base.contains_key(*path))
        .map(Vec::as_slice)
        .collect()
}

fn augment_renames_with_same_blob(
    base: &BTreeMap<Vec<u8>, IndexEntry>,
    side: &BTreeMap<Vec<u8>, IndexEntry>,
    pairs: &mut Vec<RenamePathPair>,
) {
    use crate::porcelain::cherry_pick::same_blob_renames_from_entries;
    for (old, new) in same_blob_renames_from_entries(
        base.iter(),
        side.iter(),
        |path| base.contains_key(path),
        |path| side.contains_key(path),
    ) {
        if pairs
            .iter()
            .any(|(o, n, _)| o.as_slice() == old.as_slice() && n.as_slice() == new.as_slice())
        {
            continue;
        }
        pairs.push((old, new, 100));
    }
}

/// Base files each side renamed to a different, otherwise unoccupied destination.
///
/// Destinations that collide with another file on the opposite side (rename/add,
/// rename/rename 2to1) become add/add conflicts in the path-by-path walk.
fn rename_rename_conflicts(
    ours_new_to_old: &HashMap<Vec<u8>, Vec<u8>>,
    theirs_old_to_new: &HashMap<Vec<u8>, Vec<u8>>,
    base: &BTreeMap<Vec<u8>, IndexEntry>,
    ours: &BTreeMap<Vec<u8>, IndexEntry>,
    theirs: &BTreeMap<Vec<u8>, IndexEntry>,
) -> Vec<RenameRenameConflict> {
    let mut out: Vec<RenameRenameConflict> = ours_new_to_old
        .iter()
        .filter_map(|(ours_path, base_path)| {
            let theirs_path = theirs_old_to_new.get(base_path)?;
            let distinct =
                ours_path != base_path && theirs_path != base_path && theirs_path != ours_path;
            let unoccupied = !ours.contains_key(base_path)
                && !theirs.contains_key(base_path)
                && !ours.contains_key(theirs_path)
                && !theirs.contains_key(ours_path);
            let present = base.contains_key(base_path)
                && ours.contains_key(ours_path)
                && theirs.contains_key(theirs_path);
            (distinct && unoccupied && present).then(|| RenameRenameConflict {
                base_path: base_path.clone(),
                ours_path: ours_path.clone(),
                theirs_path: theirs_path.clone(),
            })
        })
        .collect();
    out.sort_by(|a, b| a.base_path.cmp(&b.base_path));
    out
}

/// Record a rename/rename (1to2) conflict the way Git's index does.
///
/// The base entry stays at stage 1 under the old name. Both destinations get the three-way
/// merge of the content (with conflict markers when the edits overlap): stage 2 at the ours
/// name and stage 3 at the theirs name, and the same blob in the work tree. Content that
/// cannot be merged line by line (binary, symlink, gitlink) keeps each side's own version.
#[expect(clippy::too_many_arguments)]
pub(crate) fn merge_rename_rename(
    ctx: &PathMergeContext<'_>,
    index: &mut Index,
    conflict_content: &mut BTreeMap<Vec<u8>, ObjectId>,
    conflict: &RenameRenameConflict,
    base: &IndexEntry,
    ours: &IndexEntry,
    theirs: &IndexEntry,
    favor: MergeFavor,
    ws: WhitespaceMergeOptions,
    diff_algorithm: Option<&str>,
    presentation: TreeMergeConflictPresentation<'_>,
) -> crate::error::Result<()> {
    let merged = renamed_file_content(
        ctx,
        [
            &conflict.base_path,
            &conflict.ours_path,
            &conflict.theirs_path,
        ],
        base,
        ours,
        theirs,
        favor,
        ws,
        diff_algorithm,
        presentation,
    )?;
    let (at_ours, at_theirs) = match &merged {
        Some(entry) => (entry, entry),
        None => (ours, theirs),
    };
    stage_entry(index, &conflict.base_path, base, 1);
    stage_entry(index, &conflict.ours_path, at_ours, 2);
    stage_entry(index, &conflict.theirs_path, at_theirs, 3);
    conflict_content.insert(conflict.ours_path.clone(), at_ours.oid);
    conflict_content.insert(conflict.theirs_path.clone(), at_theirs.oid);
    Ok(())
}

/// Three-way merge of one file that a merge side renamed, or `None` when each side keeps its own
/// blob (binary, symlink or gitlink content).
///
/// `paths` holds the file's base, ours and theirs paths; they label the conflict markers, which
/// are 8 characters wide so the blob matches the one Git writes for a renamed file.
#[expect(clippy::too_many_arguments)]
fn renamed_file_content(
    ctx: &PathMergeContext<'_>,
    paths: [&[u8]; 3],
    base: &IndexEntry,
    ours: &IndexEntry,
    theirs: &IndexEntry,
    favor: MergeFavor,
    ws: WhitespaceMergeOptions,
    diff_algorithm: Option<&str>,
    presentation: TreeMergeConflictPresentation<'_>,
) -> crate::error::Result<Option<IndexEntry>> {
    let [base_path, ours_path, theirs_path] = paths;
    if same_blob(ours, theirs) || same_blob(base, theirs) {
        return Ok(Some(ours.clone()));
    }
    if same_blob(base, ours) {
        return Ok(Some(theirs.clone()));
    }
    let regular = |e: &IndexEntry| matches!(e.mode, 0o100644 | 0o100755);
    if !(regular(base) && regular(ours) && regular(theirs)) {
        return Ok(None);
    }
    let repo = ctx.repo;
    let base_obj = repo.odb().read(&base.oid)?;
    let ours_obj = repo.odb().read(&ours.oid)?;
    let theirs_obj = repo.odb().read(&theirs.oid)?;
    let favor = merge_favor_for_path(ctx.file_attrs(base_path).as_ref(), favor);
    let binary = |path: &[u8], data: &[u8]| {
        is_binary(data) || attrs_force_binary(ctx.file_attrs(path).as_ref())
    };
    if binary(base_path, &base_obj.data)
        || binary(ours_path, &ours_obj.data)
        || binary(theirs_path, &theirs_obj.data)
    {
        if ctx.virtual_base() {
            return Ok(Some(base.clone()));
        }
        return Ok(match favor {
            MergeFavor::Ours => Some(ours.clone()),
            MergeFavor::Theirs => Some(theirs.clone()),
            _ => None,
        });
    }

    let label_base = format!(
        "{}:{}",
        presentation.label_base,
        String::from_utf8_lossy(base_path)
    );
    let label_ours = format!(
        "{}:{}",
        presentation.label_ours,
        String::from_utf8_lossy(ours_path)
    );
    let theirs_path = String::from_utf8_lossy(theirs_path);
    let label_theirs = match presentation.label_theirs {
        TheirsConflictLabel::PathUtf8 => theirs_path.into_owned(),
        TheirsConflictLabel::Fixed(s) => format!("{s}:{theirs_path}"),
    };
    let result = merge(&MergeInput {
        base: &base_obj.data,
        ours: &ours_obj.data,
        theirs: &theirs_obj.data,
        label_ours: &label_ours,
        label_base: &label_base,
        label_theirs: &label_theirs,
        favor,
        style: presentation.style,
        marker_size: 8 + 2 * ctx.call_depth,
        diff_algorithm: diff_algorithm.map(str::to_owned),
        ignore_all_space: ws.ignore_all_space,
        ignore_space_change: ws.ignore_space_change,
        ignore_space_at_eol: ws.ignore_space_at_eol,
        ignore_cr_at_eol: ws.ignore_cr_at_eol,
    })?;
    let mut entry = ours.clone();
    entry.oid = repo.odb().write(ObjectKind::Blob, &result.content)?;
    if base.mode == ours.mode {
        entry.mode = theirs.mode;
    }
    Ok(Some(entry))
}

struct DfParkedFile {
    orig: Vec<u8>,
    escaped: Vec<u8>,
    base: Option<IndexEntry>,
    side: IndexEntry,
    side_stage: u8,
}

#[derive(Default)]
struct DfEscapes {
    parked: Vec<DfParkedFile>,
}

impl DfEscapes {
    fn skip_paths(&self) -> HashSet<Vec<u8>> {
        self.parked.iter().map(|p| p.orig.clone()).collect()
    }

    fn apply_to_output(&self, output: &mut TreeMergeOutput) {
        for parked in &self.parked {
            output.conflict_content.remove(&parked.orig);
            output.index.remove_path_all_stages(&parked.orig);
            output.index.remove_path_all_stages(&parked.escaped);
            if let Some(be) = &parked.base {
                stage_entry(&mut output.index, &parked.escaped, be, 1);
            }
            stage_entry(
                &mut output.index,
                &parked.escaped,
                &parked.side,
                parked.side_stage,
            );
            output
                .conflict_content
                .insert(parked.escaped.clone(), parked.side.oid);
            output.touched_paths.insert(parked.escaped.clone());
        }
    }
}

fn side_has_descendant(side: &BTreeMap<Vec<u8>, IndexEntry>, prefix: &[u8]) -> bool {
    side.keys()
        .any(|p| p.len() > prefix.len() && p.starts_with(prefix) && p[prefix.len()] == b'/')
}

#[expect(clippy::too_many_arguments)]
fn apply_df_file_directory_conflicts_trees(
    cache: &mut merge_tree_cache::MergeTreeCache,
    repo: &Repository,
    base_tree: &ObjectId,
    ours_tree: &ObjectId,
    theirs_tree: &ObjectId,
    base: &BTreeMap<Vec<u8>, IndexEntry>,
    ours: &mut BTreeMap<Vec<u8>, IndexEntry>,
    theirs: &mut BTreeMap<Vec<u8>, IndexEntry>,
    touched: &mut HashSet<Vec<u8>>,
    presentation: TreeMergeConflictPresentation<'_>,
) -> crate::error::Result<DfEscapes> {
    use merge_tree_cache::MergeTreeSide;

    let odb = repo.odb();
    let mut out = DfEscapes::default();
    // Relocation mutates the maps; borrow the original path snapshots for prefix queries.
    let ours_paths: Vec<Vec<u8>> = ours.keys().cloned().collect();
    let theirs_paths: Vec<Vec<u8>> = theirs.keys().cloned().collect();
    let theirs_sparse = merge_trees_walk::PathPrefixIndex::new(&theirs_paths);
    let ours_sparse = merge_trees_walk::PathPrefixIndex::new(&ours_paths);
    for path in &ours_paths {
        let Some(entry) = ours.get(path) else {
            continue;
        };
        if entry.mode == 0o040000 {
            continue;
        }
        if !merge_trees_walk::side_tree_has_descendant_at(
            cache,
            odb,
            theirs_tree,
            MergeTreeSide::Theirs,
            path,
            &theirs_sparse,
        )? {
            continue;
        }
        if skip_df_expansion_ours_file(base, ours, theirs, path) {
            continue;
        }
        if parent_has_file(theirs, path) {
            continue;
        }
        if directory_unchanged_from_base(cache, odb, base_tree, theirs_tree, path)? {
            continue;
        }
        let label = presentation.label_ours.as_bytes();
        let side_entry = ours.remove(path);
        if let Some(parked) = park_df_file_side(base, ours, theirs, path, label, 2, side_entry) {
            touched.insert(path.clone());
            touched.insert(parked.escaped.clone());
            out.parked.push(parked);
        }
    }
    for path in &theirs_paths {
        let Some(entry) = theirs.get(path) else {
            continue;
        };
        if entry.mode == 0o040000 {
            continue;
        }
        if !merge_trees_walk::side_tree_has_descendant_at(
            cache,
            odb,
            ours_tree,
            MergeTreeSide::Ours,
            path,
            &ours_sparse,
        )? {
            continue;
        }
        if skip_df_expansion_theirs_file(base, ours, theirs, path) {
            continue;
        }
        if parent_has_file(ours, path) {
            continue;
        }
        if directory_unchanged_from_base(cache, odb, base_tree, ours_tree, path)? {
            continue;
        }
        let label = df_theirs_escape_label(presentation, path);
        let side_entry = theirs.remove(path);
        if let Some(parked) =
            park_df_file_side(base, ours, theirs, path, label.as_bytes(), 3, side_entry)
        {
            touched.insert(path.clone());
            touched.insert(parked.escaped.clone());
            out.parked.push(parked);
        }
    }
    Ok(out)
}

fn df_theirs_escape_label<'a>(
    presentation: TreeMergeConflictPresentation<'a>,
    path: &[u8],
) -> std::borrow::Cow<'a, str> {
    match presentation.label_theirs {
        TheirsConflictLabel::Fixed(label) => std::borrow::Cow::Borrowed(label),
        TheirsConflictLabel::PathUtf8 => {
            std::borrow::Cow::Owned(String::from_utf8_lossy(path).into_owned())
        }
    }
}

fn occupied_paths(
    base: &BTreeMap<Vec<u8>, IndexEntry>,
    ours: &BTreeMap<Vec<u8>, IndexEntry>,
    theirs: &BTreeMap<Vec<u8>, IndexEntry>,
) -> HashSet<Vec<u8>> {
    base.keys()
        .chain(ours.keys())
        .chain(theirs.keys())
        .cloned()
        .collect()
}

fn uniquify_df_escape_path(path: &[u8], label: &[u8], occupied: &HashSet<Vec<u8>>) -> Vec<u8> {
    let mut candidate = Vec::with_capacity(path.len() + 1 + label.len());
    candidate.extend_from_slice(path);
    candidate.push(b'~');
    candidate.extend_from_slice(label);
    if !occupied.contains(&candidate) {
        return candidate;
    }
    let mut n = 2u32;
    loop {
        let mut alt = candidate.clone();
        alt.push(b'~');
        alt.extend_from_slice(n.to_string().as_bytes());
        if !occupied.contains(&alt) {
            return alt;
        }
        n += 1;
    }
}

fn park_df_file_side(
    base: &BTreeMap<Vec<u8>, IndexEntry>,
    ours: &BTreeMap<Vec<u8>, IndexEntry>,
    theirs: &BTreeMap<Vec<u8>, IndexEntry>,
    path: &[u8],
    label: &[u8],
    side_stage: u8,
    side_entry: Option<IndexEntry>,
) -> Option<DfParkedFile> {
    let mut side_entry = side_entry?;
    if side_entry.mode == 0o040000 {
        return None;
    }
    let occupied = occupied_paths(base, ours, theirs);
    let escaped = uniquify_df_escape_path(path, label, &occupied);
    side_entry.path = escaped.clone();
    side_entry.flags = path_len_flags(&escaped);
    let base_at_path = base.get(path).filter(|e| e.mode != 0o040000).cloned();
    Some(DfParkedFile {
        orig: path.to_vec(),
        escaped,
        base: base_at_path,
        side: side_entry,
        side_stage,
    })
}

/// True when `side_tree` holds exactly the base's directory at `path`.
///
/// The other side put a file at `path`, so it has nothing under `path/` and deleted every base
/// entry there. With the directory side unchanged, all of those deletions merge cleanly, nothing
/// is left under `path/`, and the file does not clash with a directory.
fn directory_unchanged_from_base(
    cache: &mut merge_tree_cache::MergeTreeCache,
    odb: &Odb,
    base_tree: &ObjectId,
    side_tree: &ObjectId,
    path: &[u8],
) -> crate::error::Result<bool> {
    let Some(base) = merge_trees_walk::lookup_index_entry_at_path(cache, odb, base_tree, path)?
    else {
        return Ok(false);
    };
    let Some(side) = merge_trees_walk::lookup_index_entry_at_path(cache, odb, side_tree, path)?
    else {
        return Ok(false);
    };
    Ok(base.mode == 0o040000 && side.mode == 0o040000 && base.oid == side.oid)
}

/// Git skips D/F relocation when one side only expanded a base file into `path/…`.
fn skip_df_expansion_ours_file(
    base: &BTreeMap<Vec<u8>, IndexEntry>,
    ours: &BTreeMap<Vec<u8>, IndexEntry>,
    theirs: &BTreeMap<Vec<u8>, IndexEntry>,
    path: &[u8],
) -> bool {
    let Some(base_e) = base.get(path) else {
        return false;
    };
    let Some(ours_e) = ours.get(path) else {
        return false;
    };
    if base_e.mode == 0o040000 {
        return false;
    }
    if !same_blob(base_e, ours_e) {
        return false;
    }
    if theirs.contains_key(path) {
        return false;
    }
    side_has_descendant(theirs, path)
}

fn skip_df_expansion_theirs_file(
    base: &BTreeMap<Vec<u8>, IndexEntry>,
    ours: &BTreeMap<Vec<u8>, IndexEntry>,
    theirs: &BTreeMap<Vec<u8>, IndexEntry>,
    path: &[u8],
) -> bool {
    let Some(base_e) = base.get(path) else {
        return false;
    };
    let Some(theirs_e) = theirs.get(path) else {
        return false;
    };
    if base_e.mode == 0o040000 {
        return false;
    }
    if !same_blob(base_e, theirs_e) {
        return false;
    }
    if ours.contains_key(path) {
        return false;
    }
    side_has_descendant(ours, path)
}

fn parent_has_file(side: &BTreeMap<Vec<u8>, IndexEntry>, path: &[u8]) -> bool {
    let Some(pos) = path.iter().rposition(|b| *b == b'/') else {
        return false;
    };
    side.get(&path[..pos]).is_some_and(|e| e.mode != 0o040000)
}

type RenamePathPair = (Vec<u8>, Vec<u8>, u32);

fn paths_from_diff_entries(entries: &[crate::diff::DiffEntry]) -> HashSet<Vec<u8>> {
    use crate::diff::DiffStatus;
    let mut out = HashSet::new();
    for e in entries {
        if e.status == DiffStatus::Unmerged {
            continue;
        }
        if let Some(p) = &e.old_path {
            out.insert(crate::repo_relpath::storage_bytes(p).into_owned());
        }
        if let Some(p) = &e.new_path {
            out.insert(crate::repo_relpath::storage_bytes(p).into_owned());
        }
    }
    out
}

fn rename_pairs_from_diff_entries(
    odb: &Odb,
    entries: Vec<crate::diff::DiffEntry>,
) -> crate::error::Result<Vec<RenamePathPair>> {
    let entries = detect_renames(odb, None, entries, 50);
    let mut out = Vec::new();
    for e in entries {
        if e.status != DiffStatus::Renamed {
            continue;
        }
        let Some(old) = e.old_path else {
            continue;
        };
        let Some(new) = e.new_path else {
            continue;
        };
        let score = e.score.unwrap_or(0);
        out.push((
            crate::repo_relpath::storage_bytes(&old).into_owned(),
            crate::repo_relpath::storage_bytes(&new).into_owned(),
            score,
        ));
    }
    Ok(out)
}

#[expect(clippy::too_many_arguments)]
pub(crate) fn merge_one_path(
    ctx: &PathMergeContext<'_>,
    index: &mut Index,
    conflict_content: &mut BTreeMap<Vec<u8>, ObjectId>,
    out_path: &[u8],
    b: Option<&IndexEntry>,
    o: Option<&IndexEntry>,
    t: Option<&IndexEntry>,
    favor: MergeFavor,
    ws: WhitespaceMergeOptions,
    diff_algorithm: Option<&str>,
    presentation: TreeMergeConflictPresentation<'_>,
) -> crate::error::Result<()> {
    match (b, o, t) {
        (_, Some(oe), Some(te)) if same_blob(oe, te) => {
            let mut e = oe.clone();
            e.path = out_path.to_vec();
            e.flags = path_len_flags(out_path);
            index.push_entry_unsorted(e);
        }
        (Some(be), Some(oe), Some(te)) if same_blob(be, oe) => {
            let mut e = te.clone();
            e.path = out_path.to_vec();
            e.flags = path_len_flags(out_path);
            index.push_entry_unsorted(e);
        }
        (Some(be), Some(oe), Some(te)) if same_blob(be, te) => {
            let mut e = oe.clone();
            e.path = out_path.to_vec();
            e.flags = path_len_flags(out_path);
            index.push_entry_unsorted(e);
        }
        (Some(be), Some(oe), Some(te))
            if be.mode == oe.mode
                && oe.mode == te.mode
                && matches!(be.mode, 0o120000 | 0o160000) =>
        {
            if ctx.virtual_base() {
                push_stage0(index, out_path, be);
            } else if same_blob(oe, te) {
                let mut e = oe.clone();
                e.path = out_path.to_vec();
                e.flags = path_len_flags(out_path);
                index.push_entry_unsorted(e);
            } else if same_blob(be, oe) {
                let mut e = te.clone();
                e.path = out_path.to_vec();
                e.flags = path_len_flags(out_path);
                index.push_entry_unsorted(e);
            } else if same_blob(be, te) {
                let mut e = oe.clone();
                e.path = out_path.to_vec();
                e.flags = path_len_flags(out_path);
                index.push_entry_unsorted(e);
            } else {
                stage_entry(index, out_path, be, 1);
                stage_entry(index, out_path, oe, 2);
                stage_entry(index, out_path, te, 3);
            }
        }
        (Some(be), Some(oe), Some(te)) => {
            content_merge_or_conflict(
                ctx,
                index,
                conflict_content,
                out_path,
                be,
                oe,
                te,
                favor,
                ws,
                diff_algorithm,
                presentation,
            )?;
        }
        (None, Some(oe), None) => {
            let mut e = oe.clone();
            e.path = out_path.to_vec();
            e.flags = path_len_flags(out_path);
            index.push_entry_unsorted(e);
        }
        (None, None, Some(te)) => {
            let mut e = te.clone();
            e.path = out_path.to_vec();
            e.flags = path_len_flags(out_path);
            index.push_entry_unsorted(e);
        }
        (None, Some(oe), Some(te)) if same_blob(oe, te) => {
            let mut e = oe.clone();
            e.path = out_path.to_vec();
            e.flags = path_len_flags(out_path);
            index.push_entry_unsorted(e);
        }
        (None, Some(oe), Some(te)) => {
            // add/add conflict: both sides introduced the path with differing content and there
            // is no merge base. Git performs a content merge against an empty base, leaving
            // conflict markers in the working tree (and unmerged stages 2/3 in the index). We
            // previously only recorded the index stages, leaving the working-tree file as the
            // plain "ours" blob — that hid the conflict from `rerere` and the user (t3504).
            add_add_content_conflict(
                ctx,
                index,
                conflict_content,
                out_path,
                oe,
                te,
                favor,
                ws,
                diff_algorithm,
                presentation,
            )?;
        }
        (Some(_), None, None) => {}
        (Some(be), Some(oe), None) if same_blob(be, oe) && be.path.as_slice() == out_path => {}
        (Some(be), None, Some(te)) if same_blob(be, te) => {}
        (Some(be), Some(_), None) | (Some(be), None, Some(_)) if ctx.virtual_base() => {
            push_stage0(index, out_path, be);
        }
        (Some(be), Some(oe), None) => {
            stage_entry(index, out_path, be, 1);
            stage_entry(index, out_path, oe, 2);
            // Modify/delete: deleted on the other side; leave our version in the worktree.
            conflict_content.insert(out_path.to_vec(), oe.oid);
        }
        (Some(be), None, Some(te)) => {
            stage_entry(index, out_path, be, 1);
            stage_entry(index, out_path, te, 3);
            // Modify/delete: deleted on ours; leave their version in the worktree.
            conflict_content.insert(out_path.to_vec(), te.oid);
        }
        (None, None, None) => {}
    }
    Ok(())
}

fn path_len_flags(path: &[u8]) -> u16 {
    path.len().min(0xFFF) as u16
}

/// Record `src` as the merged (stage-0) entry at `path`.
fn push_stage0(index: &mut Index, path: &[u8], src: &IndexEntry) {
    let mut e = src.clone();
    e.path = path.to_vec();
    e.flags = path_len_flags(path);
    index.push_entry_unsorted(e);
}

/// Record merged `content` at `path` as a stage-0 entry with `mode`.
fn push_merged_blob(
    repo: &Repository,
    index: &mut Index,
    path: &[u8],
    template: &IndexEntry,
    mode: u32,
    content: &[u8],
) -> crate::error::Result<()> {
    let mut e = template.clone();
    e.path = path.to_vec();
    e.flags = path_len_flags(path);
    e.oid = repo.odb().write(ObjectKind::Blob, content)?;
    e.mode = mode;
    index.push_entry_unsorted(e);
    Ok(())
}

fn same_blob(a: &IndexEntry, b: &IndexEntry) -> bool {
    a.oid == b.oid && a.mode == b.mode
}

/// Repository handle plus the attribute rules shared by every path of one tree merge.
///
/// The rules come from a single load of the repository index, made the first time a path
/// needs a content merge; a failure to load them means "no attributes", as for each path before.
pub(crate) struct PathMergeContext<'r> {
    repo: &'r Repository,
    rules: OnceCell<Option<WorktreeRules>>,
    /// Recursion level of a multi-base merge (see [`merge_trees_at_depth`]); 0 for the real merge.
    call_depth: usize,
}

impl<'r> PathMergeContext<'r> {
    pub(crate) fn new(repo: &'r Repository, call_depth: usize) -> Self {
        Self {
            repo,
            rules: OnceCell::new(),
            call_depth,
        }
    }

    /// True while building a virtual merge base, where every conflict settles to one entry.
    fn virtual_base(&self) -> bool {
        self.call_depth > 0
    }

    /// Conflict marker width for `attrs`, widened by two per recursion level.
    fn marker_size(&self, attrs: &FileAttrs) -> usize {
        attrs.effective_conflict_marker_size() + 2 * self.call_depth
    }

    /// A virtual base keeps a driver's conflicted output as its content.
    fn settle_driver_outcome(&self, outcome: MergeDriverOutcome) -> MergeDriverOutcome {
        match outcome {
            MergeDriverOutcome::ConflictWithMarkers(content) if self.virtual_base() => {
                MergeDriverOutcome::Clean(content)
            }
            other => other,
        }
    }

    fn file_attrs(&self, path: &[u8]) -> Option<FileAttrs> {
        let rules = self
            .rules
            .get_or_init(|| {
                let index = self.repo.load_index().ok()?;
                WorktreeRules::from_repository(self.repo, &index).ok()
            })
            .as_ref()?;
        Some(rules.file_attrs(&String::from_utf8_lossy(path), false))
    }
}

fn merge_favor_for_path(attrs: Option<&FileAttrs>, favor: MergeFavor) -> MergeFavor {
    match attrs.map(|a| &a.merge) {
        Some(MergeAttr::Driver(name)) if favor == MergeFavor::None && name == "union" => {
            MergeFavor::Union
        }
        _ => favor,
    }
}

/// True when attributes force a binary merge (`-merge`, `-text` or `-diff`).
fn attrs_force_binary(attrs: Option<&FileAttrs>) -> bool {
    attrs.is_some_and(|a| {
        matches!(a.merge, MergeAttr::Unset)
            || a.text == TextAttr::Unset
            || matches!(a.diff_attr, DiffAttr::Unset)
    })
}

fn add_add_executable_mode_conflict(
    ours: &IndexEntry,
    theirs: &IndexEntry,
    favor: MergeFavor,
) -> bool {
    matches!(ours.mode, 0o100644 | 0o100755)
        && matches!(theirs.mode, 0o100644 | 0o100755)
        && ours.mode != theirs.mode
        && favor == MergeFavor::None
}

pub(crate) fn stage_entry(index: &mut Index, path: &[u8], src: &IndexEntry, stage: u8) {
    let mut e = src.clone();
    e.path = path.to_vec();
    e.flags = path_len_flags(path) | ((stage as u16) << 12);
    index.push_entry_unsorted(e);
}

#[allow(clippy::too_many_arguments)]
fn finish_merge_driver_result(
    repo: &Repository,
    index: &mut Index,
    conflict_content: &mut BTreeMap<Vec<u8>, ObjectId>,
    path: &[u8],
    base: Option<&IndexEntry>,
    ours: &IndexEntry,
    theirs: &IndexEntry,
    outcome: MergeDriverOutcome,
) -> crate::error::Result<()> {
    match outcome {
        MergeDriverOutcome::Clean(merged) => {
            let merged_oid = repo.odb().write(ObjectKind::Blob, &merged)?;
            let mut entry = ours.clone();
            entry.path = path.to_vec();
            entry.flags = path_len_flags(path);
            entry.oid = merged_oid;
            if let Some(base) = base {
                if base.mode == ours.mode && base.mode != theirs.mode {
                    entry.mode = theirs.mode;
                }
            }
            index.push_entry_unsorted(entry);
        }
        MergeDriverOutcome::ConflictWithMarkers(content) => {
            let conflict_oid = repo.odb().write(ObjectKind::Blob, &content)?;
            conflict_content.insert(path.to_vec(), conflict_oid);
            if let Some(base) = base {
                stage_entry(index, path, base, 1);
            }
            stage_entry(index, path, ours, 2);
            stage_entry(index, path, theirs, 3);
        }
        MergeDriverOutcome::Conflict => {
            if let Some(base) = base {
                stage_entry(index, path, base, 1);
            }
            stage_entry(index, path, ours, 2);
            stage_entry(index, path, theirs, 3);
        }
    }
    Ok(())
}
#[expect(clippy::too_many_arguments)]
fn content_merge_or_conflict(
    ctx: &PathMergeContext<'_>,
    index: &mut Index,
    conflict_content: &mut BTreeMap<Vec<u8>, ObjectId>,
    path: &[u8],
    base: &IndexEntry,
    ours: &IndexEntry,
    theirs: &IndexEntry,
    favor: MergeFavor,
    ws: WhitespaceMergeOptions,
    diff_algorithm: Option<&str>,
    presentation: TreeMergeConflictPresentation<'_>,
) -> crate::error::Result<()> {
    if matches!(base.mode, 0o120000 | 0o160000)
        || matches!(ours.mode, 0o120000 | 0o160000)
        || matches!(theirs.mode, 0o120000 | 0o160000)
    {
        if ctx.virtual_base() {
            push_stage0(index, path, base);
            return Ok(());
        }
        stage_entry(index, path, base, 1);
        stage_entry(index, path, ours, 2);
        stage_entry(index, path, theirs, 3);
        return Ok(());
    }

    // `git checkout -m` runs the normal three-way line merge below: when the merge auto-resolves
    // (non-overlapping edits) Git records a clean stage-0 entry and reports a single `M <path>`;
    // only a genuine line conflict (`result.conflicts > 0`) records unmerged stages 1/2/3. The
    // marker style (merge vs. diff3) is taken from `presentation.style`, so `--conflict=diff3`
    // produces the `||||||| base` section (t7201 5, 6, 9).
    let repo = ctx.repo;
    let base_obj = repo.odb().read(&base.oid)?;
    let ours_obj = repo.odb().read(&ours.oid)?;
    let theirs_obj = repo.odb().read(&theirs.oid)?;
    let attrs = ctx.file_attrs(path);
    let file_attrs = attrs.clone().unwrap_or_default();
    let favor = merge_favor_for_path(attrs.as_ref(), favor);
    let path_str = String::from_utf8_lossy(path).into_owned();
    let label_theirs = match presentation.label_theirs {
        TheirsConflictLabel::PathUtf8 => path_str.clone(),
        TheirsConflictLabel::Fixed(s) => s.to_owned(),
    };
    let driver_labels = MergeDriverLabels {
        base: presentation.label_base,
        ours: presentation.label_ours,
        theirs: &label_theirs,
    };
    if let Some((outcome, _content)) = try_merge_driver(
        repo,
        &file_attrs,
        &path_str,
        Some(&base_obj.data),
        &ours_obj.data,
        &theirs_obj.data,
        favor,
        presentation.style,
        diff_algorithm,
        ws.ignore_all_space,
        ws.ignore_space_change,
        ws.ignore_space_at_eol,
        ws.ignore_cr_at_eol,
        driver_labels,
    )? {
        return finish_merge_driver_result(
            repo,
            index,
            conflict_content,
            path,
            Some(base),
            ours,
            theirs,
            ctx.settle_driver_outcome(outcome),
        );
    }

    if attrs_force_binary(attrs.as_ref())
        || is_binary(&base_obj.data)
        || is_binary(&ours_obj.data)
        || is_binary(&theirs_obj.data)
    {
        if ctx.virtual_base() {
            push_stage0(index, path, base);
            return Ok(());
        }
        match favor {
            MergeFavor::Theirs => {
                let mut e = theirs.clone();
                e.path = path.to_vec();
                e.flags = path_len_flags(path);
                index.push_entry_unsorted(e);
                return Ok(());
            }
            MergeFavor::Ours => {
                let mut e = ours.clone();
                e.path = path.to_vec();
                e.flags = path_len_flags(path);
                index.push_entry_unsorted(e);
                return Ok(());
            }
            _ => {
                stage_entry(index, path, base, 1);
                stage_entry(index, path, ours, 2);
                stage_entry(index, path, theirs, 3);
                return Ok(());
            }
        }
    }

    let input = MergeInput {
        base: &base_obj.data,
        ours: &ours_obj.data,
        theirs: &theirs_obj.data,
        label_ours: presentation.label_ours,
        label_base: presentation.label_base,
        label_theirs: &label_theirs,
        favor,
        style: presentation.style,
        marker_size: ctx.marker_size(&file_attrs),
        diff_algorithm: diff_algorithm.map(str::to_owned),
        ignore_all_space: ws.ignore_all_space,
        ignore_space_change: ws.ignore_space_change,
        ignore_space_at_eol: ws.ignore_space_at_eol,
        ignore_cr_at_eol: ws.ignore_cr_at_eol,
    };

    let result = merge(&input)?;

    if result.conflicts > 0 && !ctx.virtual_base() {
        let conflict_oid = repo.odb().write(ObjectKind::Blob, &result.content)?;
        conflict_content.insert(path.to_vec(), conflict_oid);
        stage_entry(index, path, base, 1);
        stage_entry(index, path, ours, 2);
        stage_entry(index, path, theirs, 3);
    } else {
        let merged_oid = repo.odb().write(ObjectKind::Blob, &result.content)?;
        let mut entry = ours.clone();
        entry.path = path.to_vec();
        entry.flags = path_len_flags(path);
        entry.oid = merged_oid;
        if base.mode == ours.mode && base.mode != theirs.mode {
            entry.mode = theirs.mode;
        }
        index.push_entry_unsorted(entry);
    }

    Ok(())
}

/// Resolve an add/add conflict (no merge base, both sides created `path`).
///
/// Mirrors Git's behaviour: a content merge with an empty base. When the line merge does not
/// auto-resolve (the common case for genuinely different content), the conflict-marker blob is
/// recorded for the working tree and the unmerged stages 2/3 are written to the index. Gitlink
/// (submodule) and binary add/adds fall back to plain unmerged stages, matching the
/// `content_merge_or_conflict` policy.
#[allow(clippy::too_many_arguments)]
fn add_add_content_conflict(
    ctx: &PathMergeContext<'_>,
    index: &mut Index,
    conflict_content: &mut BTreeMap<Vec<u8>, ObjectId>,
    path: &[u8],
    ours: &IndexEntry,
    theirs: &IndexEntry,
    favor: MergeFavor,
    ws: WhitespaceMergeOptions,
    diff_algorithm: Option<&str>,
    presentation: TreeMergeConflictPresentation<'_>,
) -> crate::error::Result<()> {
    // Gitlink add/add: no textual merge is possible; leave unmerged stages. A virtual base keeps
    // the (absent) base version, so the path is left out.
    if matches!(ours.mode, 0o120000 | 0o160000) || matches!(theirs.mode, 0o120000 | 0o160000) {
        if ctx.virtual_base() {
            return Ok(());
        }
        stage_entry(index, path, ours, 2);
        stage_entry(index, path, theirs, 3);
        return Ok(());
    }

    let repo = ctx.repo;
    let ours_obj = repo.odb().read(&ours.oid)?;
    let theirs_obj = repo.odb().read(&theirs.oid)?;
    let attrs = ctx.file_attrs(path);
    let file_attrs = attrs.clone().unwrap_or_default();
    let favor = merge_favor_for_path(attrs.as_ref(), favor);
    let path_str = String::from_utf8_lossy(path).into_owned();
    let label_theirs = match presentation.label_theirs {
        TheirsConflictLabel::PathUtf8 => path_str.clone(),
        TheirsConflictLabel::Fixed(s) => s.to_owned(),
    };
    let driver_labels = MergeDriverLabels {
        base: presentation.label_base,
        ours: presentation.label_ours,
        theirs: &label_theirs,
    };
    if let Some((outcome, content)) = try_merge_driver(
        repo,
        &file_attrs,
        &path_str,
        None,
        &ours_obj.data,
        &theirs_obj.data,
        favor,
        presentation.style,
        diff_algorithm,
        ws.ignore_all_space,
        ws.ignore_space_change,
        ws.ignore_space_at_eol,
        ws.ignore_cr_at_eol,
        driver_labels,
    )? {
        let outcome = ctx.settle_driver_outcome(outcome);
        if !ctx.virtual_base() && add_add_executable_mode_conflict(ours, theirs, favor) {
            let worktree_bytes = match &outcome {
                MergeDriverOutcome::Clean(c) | MergeDriverOutcome::ConflictWithMarkers(c) => {
                    c.as_slice()
                }
                MergeDriverOutcome::Conflict => content.as_slice(),
            };
            let conflict_oid = repo.odb().write(ObjectKind::Blob, worktree_bytes)?;
            conflict_content.insert(path.to_vec(), conflict_oid);
            stage_entry(index, path, ours, 2);
            stage_entry(index, path, theirs, 3);
            return Ok(());
        }
        return finish_merge_driver_result(
            repo,
            index,
            conflict_content,
            path,
            None,
            ours,
            theirs,
            outcome,
        );
    }

    // Binary add/add: honour an explicit favour, else leave unmerged stages. A virtual base
    // takes the empty base content.
    if attrs_force_binary(attrs.as_ref())
        || is_binary(&ours_obj.data)
        || is_binary(&theirs_obj.data)
    {
        if ctx.virtual_base() {
            return push_merged_blob(repo, index, path, ours, ours.mode, b"");
        }
        match favor {
            MergeFavor::Theirs => {
                let mut e = theirs.clone();
                e.path = path.to_vec();
                e.flags = path_len_flags(path);
                index.push_entry_unsorted(e);
            }
            MergeFavor::Ours => {
                let mut e = ours.clone();
                e.path = path.to_vec();
                e.flags = path_len_flags(path);
                index.push_entry_unsorted(e);
            }
            _ => {
                stage_entry(index, path, ours, 2);
                stage_entry(index, path, theirs, 3);
            }
        }
        return Ok(());
    }

    let input = MergeInput {
        base: b"",
        ours: &ours_obj.data,
        theirs: &theirs_obj.data,
        label_ours: presentation.label_ours,
        label_base: presentation.label_base,
        label_theirs: &label_theirs,
        favor,
        style: presentation.style,
        marker_size: ctx.marker_size(&file_attrs),
        diff_algorithm: diff_algorithm.map(str::to_owned),
        ignore_all_space: ws.ignore_all_space,
        ignore_space_change: ws.ignore_space_change,
        ignore_space_at_eol: ws.ignore_space_at_eol,
        ignore_cr_at_eol: ws.ignore_cr_at_eol,
    };

    let result = merge(&input)?;
    if ctx.virtual_base() {
        return push_merged_blob(repo, index, path, ours, ours.mode, &result.content);
    }

    // Git treats an add/add as a conflict whenever the two sides cannot be reconciled to a single
    // stage-0 entry. With no merge base there is nothing to disambiguate a mode clash, so when both
    // sides introduce the path as a regular file with a different executable bit (100644 vs 100755)
    // the result is a conflict even if the *content* merges cleanly — Git records unmerged stages
    // 2/3 with the diverging modes (t6411: double mode change with identical content). Type clashes
    // (file/symlink/submodule) are handled by the gitlink branch above / elsewhere.
    let both_regular_files =
        matches!(ours.mode, 0o100644 | 0o100755) && matches!(theirs.mode, 0o100644 | 0o100755);
    let mode_conflict = both_regular_files && ours.mode != theirs.mode && favor == MergeFavor::None;

    if result.conflicts > 0 || mode_conflict {
        let conflict_oid = repo.odb().write(ObjectKind::Blob, &result.content)?;
        conflict_content.insert(path.to_vec(), conflict_oid);
        stage_entry(index, path, ours, 2);
        stage_entry(index, path, theirs, 3);
    } else {
        // A favour (ours/theirs/union) or identical-after-normalisation merge resolved the
        // content cleanly: record a single stage-0 entry with the merged blob.
        let merged_oid = repo.odb().write(ObjectKind::Blob, &result.content)?;
        let mut entry = ours.clone();
        entry.path = path.to_vec();
        entry.flags = path_len_flags(path);
        entry.oid = merged_oid;
        index.push_entry_unsorted(entry);
    }

    Ok(())
}

/// Merge sparse [`TreeMergeOutput::index`] entries into `index` at [`TreeMergeOutput::touched_paths`].
///
/// Removes all stages at each touched path, then merges `merged.index` in one sorted pass via
/// [`Index::remove_paths_and_insert`] (same semantics as the former remove/push/sort/dedup cycle).
pub fn apply_tree_merge_output_to_index(index: &mut Index, merged: &TreeMergeOutput) {
    let paths: Vec<&[u8]> = merged.touched_paths.iter().map(Vec::as_slice).collect();
    let replacements: Vec<IndexEntry> = merged.index.entries().to_vec();
    index.remove_paths_and_insert(paths, replacements);
}

/// Refresh stage-0 stat cache for sparse merge index rows after a worktree checkout.
///
/// Sparse merge entries are built without reading the work tree; without this pass,
/// [`apply_tree_merge_output_to_index`] would leave stale zeroed stat fields and
/// `status` would report clean merge results as modified.
///
/// # Errors
///
/// Propagates work tree and `stat` failures from [`entry_from_stat`].
pub fn refresh_tree_merge_index_stat_cache(
    repo: &Repository,
    index: &mut Index,
    merged: &TreeMergeOutput,
) -> crate::error::Result<()> {
    let Some(work_tree) = repo.work_tree() else {
        return Ok(());
    };
    let mut refreshed_rows = Vec::new();
    for entry in merged.index.entries() {
        if entry.stage() != 0 {
            continue;
        }
        if entry.mode != MODE_REGULAR && entry.mode != crate::index::MODE_EXECUTABLE {
            continue;
        }
        if let Some(existing) = index.get(&entry.path, 0) {
            if existing.oid == entry.oid
                && existing.mode == entry.mode
                && existing.size > 0
                && (existing.mtime_sec > 0 || existing.ctime_sec > 0)
            {
                continue;
            }
        }
        let abs = worktree_path_from_index_rel(work_tree, &entry.path);
        if !abs.is_file() {
            continue;
        }
        refreshed_rows.push(entry_from_stat(&abs, &entry.path, entry.oid, entry.mode)?);
    }
    index.overwrite_stage0_entries(refreshed_rows);
    Ok(())
}

/// Apply sparse merge index rows and sync stat cache from the work tree when present.
///
/// Skips paths whose stage-0 row already matches the merge result (including when only
/// in-memory extended flags such as fsmonitor-valid differ). Applies the remainder in one
/// index pass, then patches stat fields in place for any rows that still need refresh.
///
/// # Errors
///
/// Propagates errors from [`refresh_tree_merge_index_stat_cache`].
pub fn apply_tree_merge_output_to_worktree_index(
    repo: &Repository,
    index: &mut Index,
    merged: &TreeMergeOutput,
) -> crate::error::Result<()> {
    if repo.work_tree().is_none() {
        apply_tree_merge_output_to_index(index, merged);
        return Ok(());
    }
    let pending = pending_tree_merge_index_rows(index, merged);
    if pending.touched_paths.is_empty() && pending.index.entries().is_empty() {
        return Ok(());
    }
    apply_tree_merge_output_to_index(index, &pending);
    refresh_tree_merge_index_stat_cache(repo, index, &pending)
}

/// The part of `merged` that still has to be written into `index`.
///
/// After a worktree checkout the index usually already holds every merged row, with fresh stat
/// data, so re-removing, re-inserting and re-stat'ing those paths only costs O(n) vector moves
/// per path. A path is dropped from the result when `index` already has exactly the merged
/// stage-0 regular-file row with populated stat data (the state the stat refresh would keep),
/// or when neither side has any row for it.
fn pending_tree_merge_index_rows(index: &Index, merged: &TreeMergeOutput) -> TreeMergeOutput {
    let mut merged_rows: HashMap<&[u8], Vec<&IndexEntry>> = HashMap::new();
    for entry in merged.index.entries() {
        merged_rows
            .entry(entry.path.as_slice())
            .or_default()
            .push(entry);
    }
    let up_to_date = |path: &[u8]| -> bool {
        let has_unmerged = (1..=3).any(|stage| index.get(path, stage).is_some());
        if has_unmerged {
            return false;
        }
        let existing = index.get(path, 0);
        match (merged_rows.get(path).map(Vec::as_slice), existing) {
            (None, None) => true,
            // Checkout recorded this row without writing the file (sparse checkout skipped the
            // path); stat'ing whatever is on disk would mark foreign content as clean.
            (Some([row]), Some(existing))
                if existing.skip_worktree() || existing.stat_cache_unset() =>
            {
                row.stage() == 0 && existing.oid == row.oid && existing.mode == row.mode
            }
            (Some([row]), Some(existing)) => {
                row.stage() == 0
                    && (row.mode == MODE_REGULAR || row.mode == crate::index::MODE_EXECUTABLE)
                    && existing.oid == row.oid
                    && existing.mode == row.mode
                    && existing.flags == row.flags
                    && IndexEntry::extended_flags_equal_ignoring_volatile(
                        existing.flags_extended,
                        row.flags_extended,
                    )
                    && !existing.intent_to_add()
                    && existing.size > 0
                    && (existing.mtime_sec > 0 || existing.ctime_sec > 0)
            }
            _ => false,
        }
    };

    let mut pending_index = Index::new();
    for entry in merged.index.entries() {
        if !up_to_date(&entry.path) {
            pending_index.push_entry_unsorted(entry.clone());
        }
    }
    pending_index.sort();
    TreeMergeOutput {
        index: pending_index,
        conflict_content: BTreeMap::new(),
        result_tree: merged.result_tree,
        touched_paths: merged
            .touched_paths
            .iter()
            .filter(|path| !up_to_date(path))
            .cloned()
            .collect(),
        rename_conflicts: Vec::new(),
        directory_rename_conflicts: Vec::new(),
    }
}

/// Tree OID to record in a commit after a three-way merge (applies conflict-marker policy).
pub fn materialized_merge_result_tree(
    odb: &Odb,
    merged: &TreeMergeOutput,
) -> crate::error::Result<ObjectId> {
    use std::collections::HashSet;

    use crate::write_tree::write_tree_partial_from_index;

    if merged.conflict_content.is_empty() {
        return Ok(merged.result_tree);
    }

    let mut patch_paths: HashSet<Vec<u8>> = HashSet::new();
    let mut patch_index = Index::new();

    for path in merged.conflict_content.keys().chain(
        merged
            .index
            .entries()
            .iter()
            .filter(|e| e.stage() != 0)
            .map(|e| &e.path),
    ) {
        patch_paths.insert(path.clone());
    }

    for path in &patch_paths {
        if let Some(marker) = merged.conflict_content.get(path) {
            let mode = merged
                .index
                .get(path, 2)
                .or_else(|| merged.index.get(path, 3))
                .map(|e| e.mode)
                .unwrap_or(crate::index::MODE_REGULAR);
            let mut entry = IndexEntry {
                ctime_sec: 0,
                ctime_nsec: 0,
                mtime_sec: 0,
                mtime_nsec: 0,
                dev: 0,
                ino: 0,
                mode,
                uid: 0,
                gid: 0,
                size: 0,
                oid: *marker,
                flags: (path.len().min(0xFFF) as u16),
                flags_extended: None,
                path: path.clone(),
                base_index_pos: 0,
            };
            entry.set_stage(0);
            entry.flags = path.len().min(0xFFF) as u16;
            patch_index.add_or_replace(entry);
        } else if let Some(stage2) = merged.index.get(path, 2) {
            let mut entry = stage2.clone();
            entry.set_stage(0);
            entry.flags = path.len().min(0xFFF) as u16;
            patch_index.add_or_replace(entry);
        } else if let Some(stage3) = merged.index.get(path, 3) {
            let mut entry = stage3.clone();
            entry.set_stage(0);
            entry.flags = path.len().min(0xFFF) as u16;
            patch_index.add_or_replace(entry);
        }
    }

    write_tree_partial_from_index(odb, &patch_index, &merged.result_tree, &patch_paths)
}

/// True when the index tree matches `head_tree_oid` (used for empty pick detection).
pub fn index_tree_oid_matches_head(
    odb: &Odb,
    index: &Index,
    head_tree_oid: &ObjectId,
) -> crate::error::Result<bool> {
    let merged = write_tree_from_index(odb, index, "")?;
    Ok(merged == *head_tree_oid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::zero_oid;
    use crate::environment::RepositoryOptions;
    use crate::index::{IndexEntry, MODE_REGULAR};
    use crate::objects::{parse_commit, ObjectId};
    use crate::repo::Repository;

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn merge_trees_df_skips_relocation_when_theirs_expands_base_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        grit_test_support::git(tmp.path(), &["init", "-q", "-b", "main"]);
        let repo =
            Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("repo");
        std::fs::write(tmp.path().join("dir"), "base\n").expect("write");
        grit_test_support::git(tmp.path(), &["add", "dir"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "base"]);
        let base_tree = {
            let base: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
                .trim()
                .parse()
                .expect("base");
            let obj = repo.odb().read(&base).expect("read");
            parse_commit(&obj.data).expect("parse").tree
        };
        grit_test_support::git(tmp.path(), &["switch", "-c", "theirs"]);
        grit_test_support::git(tmp.path(), &["rm", "-f", "dir"]);
        std::fs::create_dir_all(tmp.path().join("dir")).expect("mkdir");
        std::fs::write(tmp.path().join("dir/foo"), "foo\n").expect("write");
        grit_test_support::git(tmp.path(), &["add", "dir/foo"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "theirs"]);
        let theirs_commit: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("theirs");
        let theirs_tree = {
            let obj = repo.odb().read(&theirs_commit).expect("read");
            parse_commit(&obj.data).expect("parse").tree
        };
        grit_test_support::git(tmp.path(), &["switch", "main"]);
        let ours_tree = base_tree;
        let ours_commit: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("ours");
        let out = merge_trees_three_way(
            &repo,
            base_tree,
            ours_tree,
            theirs_tree,
            MergeFavor::default(),
            WhitespaceMergeOptions::default(),
            None,
            TreeMergeConflictPresentation {
                df_directory_ours_commit: Some(ours_commit),
                df_directory_theirs_commit: Some(theirs_commit),
                ..TreeMergeConflictPresentation::default()
            },
            DirectoryRenameMode::default(),
        )
        .expect("merge");
        assert!(out.index.get(b"dir/foo", 0).is_some());
        assert!(
            out.index.entries().iter().all(|e| e.stage() == 0),
            "expected clean merge without df escape entries"
        );
    }

    /// A repository with `base`, `changed` (from `base`, `src/lib` swaps between a directory and
    /// a file) and `other` (from `base`, adds `src/b.rs` and leaves `src/lib` alone).
    struct DirectoryFileFixture {
        dir: tempfile::TempDir,
        repo: Repository,
        base: ObjectId,
        changed: ObjectId,
        other: ObjectId,
    }

    fn directory_file_fixture(base_has_directory: bool) -> DirectoryFileFixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path();
        let git = |args: &[&str]| grit_test_support::git(path, args);
        let head = || -> ObjectId { git(&["rev-parse", "HEAD"]).trim().parse().expect("oid") };
        git(&["init", "-q", "-b", "main"]);
        std::fs::create_dir_all(path.join("src")).expect("mkdir");
        std::fs::write(path.join("src/a.rs"), "a\n").expect("write");
        let as_directory = |path: &std::path::Path| {
            std::fs::create_dir_all(path.join("src/lib")).expect("mkdir");
            std::fs::write(path.join("src/lib/x.rs"), "x\n").expect("write");
        };
        let as_file = |path: &std::path::Path| {
            std::fs::write(path.join("src/lib"), "now a file\n").expect("write");
        };
        if base_has_directory {
            as_directory(path);
        } else {
            as_file(path);
        }
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        let base = head();
        git(&["switch", "-q", "-c", "changed"]);
        git(&["rm", "-q", "-r", "src/lib"]);
        if base_has_directory {
            as_file(path);
        } else {
            as_directory(path);
        }
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "swap src/lib"]);
        let changed = head();
        git(&["switch", "-q", "-c", "other", "main"]);
        std::fs::write(path.join("src/b.rs"), "b\n").expect("write");
        git(&["add", "src/b.rs"]);
        git(&["commit", "-q", "-m", "add b"]);
        let other = head();
        let repo = Repository::discover(Some(path), &RepositoryOptions::empty()).expect("repo");
        DirectoryFileFixture {
            dir,
            repo,
            base,
            changed,
            other,
        }
    }

    impl DirectoryFileFixture {
        fn tree(&self, commit: ObjectId) -> ObjectId {
            self.repo.odb().read_commit(&commit).expect("commit").tree
        }

        /// Merge `ours` and `theirs` over `base`, with or without merge's D/F relocation.
        fn merge(&self, ours: ObjectId, theirs: ObjectId, relocate: bool) -> TreeMergeOutput {
            let presentation = if relocate {
                TreeMergeConflictPresentation {
                    df_directory_ours_commit: Some(ours),
                    df_directory_theirs_commit: Some(theirs),
                    ..TreeMergeConflictPresentation::default()
                }
            } else {
                TreeMergeConflictPresentation::default()
            };
            merge_trees_three_way(
                &self.repo,
                self.tree(self.base),
                self.tree(ours),
                self.tree(theirs),
                MergeFavor::default(),
                WhitespaceMergeOptions::default(),
                None,
                presentation,
                DirectoryRenameMode::default(),
            )
            .expect("merge")
        }

        /// The tree system git's `merge-tree` writes for the same merge, when it is clean.
        fn git_merge_tree(&self, ours: ObjectId, theirs: ObjectId) -> Option<ObjectId> {
            let out = grit_test_support::git_cmd(&[
                "merge-tree",
                "--write-tree",
                &format!("--merge-base={}", self.base),
                &ours.to_hex(),
                &theirs.to_hex(),
            ])
            .in_dir(self.dir.path())
            .exec();
            out.ok().then(|| {
                out.stdout
                    .lines()
                    .next()
                    .expect("tree line")
                    .parse()
                    .expect("tree")
            })
        }
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn a_path_swapped_between_directory_and_file_merges_with_a_side_that_left_it_alone() {
        for base_has_directory in [true, false] {
            let fx = directory_file_fixture(base_has_directory);
            for (ours, theirs) in [(fx.changed, fx.other), (fx.other, fx.changed)] {
                let expected = fx
                    .git_merge_tree(ours, theirs)
                    .expect("git merges this cleanly");
                for relocate in [false, true] {
                    let case = format!(
                        "base has a directory: {base_has_directory}, ours is the changed side: {}, \
                         relocate: {relocate}",
                        ours == fx.changed
                    );
                    let out = fx.merge(ours, theirs, relocate);
                    assert!(!out.index.has_unmerged_entries(), "no conflict ({case})");
                    assert!(out.conflict_content.is_empty(), "no markers ({case})");
                    assert!(
                        out.index.entries().iter().all(|e| e.mode != 0o040000),
                        "no directory rows in the index ({case})"
                    );
                    assert_eq!(out.result_tree, expected, "same tree as git ({case})");
                }
            }
        }
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn a_directory_changed_on_one_side_and_made_a_file_on_the_other_still_conflicts() {
        let fx = directory_file_fixture(true);
        let path = fx.dir.path();
        grit_test_support::git(path, &["switch", "-q", "other"]);
        std::fs::write(path.join("src/lib/x.rs"), "x changed\n").expect("write");
        grit_test_support::git(path, &["commit", "-q", "-am", "change x"]);
        let other: ObjectId = grit_test_support::git(path, &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("oid");
        assert_eq!(fx.git_merge_tree(fx.changed, other), None, "git conflicts");
        for relocate in [false, true] {
            let out = fx.merge(fx.changed, other, relocate);
            assert!(
                out.index.has_unmerged_entries(),
                "a conflict (relocate: {relocate})"
            );
        }
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn merge_trees_three_way_theirs_only_addition() {
        let tmp = tempfile::tempdir().expect("tempdir");
        grit_test_support::git(tmp.path(), &["init", "-q", "-b", "main"]);
        let repo =
            Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("repo");
        std::fs::write(tmp.path().join("base.txt"), "b\n").expect("write");
        grit_test_support::git(tmp.path(), &["add", "base.txt"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "base"]);
        let base: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("base");
        let base_tree = {
            let obj = repo.odb().read(&base).expect("read");
            parse_commit(&obj.data).expect("parse").tree
        };
        grit_test_support::git(tmp.path(), &["switch", "-c", "topic"]);
        std::fs::write(tmp.path().join("theirs-only.txt"), "t\n").expect("write");
        grit_test_support::git(tmp.path(), &["add", "theirs-only.txt"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "topic"]);
        let theirs: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("theirs");
        let theirs_tree = {
            let obj = repo.odb().read(&theirs).expect("read");
            parse_commit(&obj.data).expect("parse").tree
        };
        grit_test_support::git(tmp.path(), &["switch", "main"]);
        let ours: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("ours");
        let ours_tree = {
            let obj = repo.odb().read(&ours).expect("read");
            parse_commit(&obj.data).expect("parse").tree
        };
        let out = merge_trees_three_way(
            &repo,
            base_tree,
            ours_tree,
            theirs_tree,
            MergeFavor::default(),
            WhitespaceMergeOptions::default(),
            None,
            TreeMergeConflictPresentation::default(),
            DirectoryRenameMode::default(),
        )
        .expect("merge");
        assert!(out.index.get(b"theirs-only.txt", 0).is_some());
        assert!(out.conflict_content.is_empty());
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn merge_trees_clean_textual_merge_auto_resolves() {
        let tmp = tempfile::tempdir().expect("tempdir");
        grit_test_support::git(tmp.path(), &["init", "-q", "-b", "main"]);
        let repo =
            Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("repo");
        std::fs::write(tmp.path().join("f.txt"), "1\n2\n3\n4\n").expect("write");
        grit_test_support::git(tmp.path(), &["add", "f.txt"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "base"]);
        let base_tree = {
            let base: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
                .trim()
                .parse()
                .expect("base");
            let obj = repo.odb().read(&base).expect("read");
            parse_commit(&obj.data).expect("parse").tree
        };
        grit_test_support::git(tmp.path(), &["switch", "-c", "topic"]);
        std::fs::write(tmp.path().join("f.txt"), "1\n2\n3\nFOUR\n").expect("write");
        grit_test_support::git(tmp.path(), &["add", "f.txt"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "theirs"]);
        let theirs_tree = {
            let oid: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
                .trim()
                .parse()
                .expect("theirs");
            let obj = repo.odb().read(&oid).expect("read");
            parse_commit(&obj.data).expect("parse").tree
        };
        grit_test_support::git(tmp.path(), &["switch", "main"]);
        std::fs::write(tmp.path().join("f.txt"), "1\nTWO\n3\n4\n").expect("write");
        grit_test_support::git(tmp.path(), &["add", "f.txt"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "ours"]);
        let ours_tree = {
            let oid: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
                .trim()
                .parse()
                .expect("ours");
            let obj = repo.odb().read(&oid).expect("read");
            parse_commit(&obj.data).expect("parse").tree
        };
        let out = merge_trees_three_way(
            &repo,
            base_tree,
            ours_tree,
            theirs_tree,
            MergeFavor::default(),
            WhitespaceMergeOptions::default(),
            None,
            TreeMergeConflictPresentation::default(),
            DirectoryRenameMode::default(),
        )
        .expect("merge");
        assert!(out.index.get(b"f.txt", 0).is_some());
        assert!(out.conflict_content.is_empty());
        let blob = repo
            .odb()
            .read(&out.index.get(b"f.txt", 0).expect("entry").oid)
            .expect("blob");
        assert_eq!(blob.data, b"1\nTWO\n3\nFOUR\n");
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn merge_trees_gitlink_three_way_conflict_stages() {
        let tmp = tempfile::tempdir().expect("tempdir");
        grit_test_support::git(tmp.path(), &["init", "-q", "-b", "main"]);
        let nested = tmp.path().join("nested");
        std::fs::create_dir_all(&nested).expect("mkdir");
        grit_test_support::git(&nested, &["init", "-q"]);
        std::fs::write(nested.join("inner.txt"), "x\n").expect("write");
        grit_test_support::git(&nested, &["add", "inner.txt"]);
        grit_test_support::git(&nested, &["commit", "-m", "nested"]);
        let sub_a = grit_test_support::git(&nested, &["rev-parse", "HEAD"])
            .trim()
            .to_owned();
        std::process::Command::new("git")
            .current_dir(tmp.path())
            .args([
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{sub_a},sub"),
            ])
            .status()
            .expect("update-index");
        grit_test_support::git(tmp.path(), &["commit", "-m", "base"]);
        let repo =
            Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("repo");
        let base_tree = {
            let base: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
                .trim()
                .parse()
                .expect("base");
            let obj = repo.odb().read(&base).expect("read");
            parse_commit(&obj.data).expect("parse").tree
        };
        grit_test_support::git(&nested, &["commit", "--allow-empty", "-m", "bump"]);
        let sub_b = grit_test_support::git(&nested, &["rev-parse", "HEAD"])
            .trim()
            .to_owned();
        grit_test_support::git(tmp.path(), &["switch", "-c", "topic"]);
        std::process::Command::new("git")
            .current_dir(tmp.path())
            .args([
                "update-index",
                "--cacheinfo",
                &format!("160000,{sub_b},sub"),
            ])
            .status()
            .expect("update-index");
        grit_test_support::git(tmp.path(), &["commit", "-m", "theirs sub"]);
        let theirs_tree = {
            let oid: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
                .trim()
                .parse()
                .expect("theirs");
            let obj = repo.odb().read(&oid).expect("read");
            parse_commit(&obj.data).expect("parse").tree
        };
        grit_test_support::git(&nested, &["commit", "--allow-empty", "-m", "bump2"]);
        let sub_c = grit_test_support::git(&nested, &["rev-parse", "HEAD"])
            .trim()
            .to_owned();
        grit_test_support::git(tmp.path(), &["switch", "main"]);
        std::process::Command::new("git")
            .current_dir(tmp.path())
            .args([
                "update-index",
                "--cacheinfo",
                &format!("160000,{sub_c},sub"),
            ])
            .status()
            .expect("update-index");
        grit_test_support::git(tmp.path(), &["commit", "-m", "ours sub"]);
        let ours_tree = {
            let oid: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
                .trim()
                .parse()
                .expect("ours");
            let obj = repo.odb().read(&oid).expect("read");
            parse_commit(&obj.data).expect("parse").tree
        };
        let out = merge_trees_three_way(
            &repo,
            base_tree,
            ours_tree,
            theirs_tree,
            MergeFavor::default(),
            WhitespaceMergeOptions::default(),
            None,
            TreeMergeConflictPresentation::default(),
            DirectoryRenameMode::default(),
        )
        .expect("merge");
        assert!(out.index.get(b"sub", 1).is_some());
        assert!(out.index.get(b"sub", 2).is_some());
        assert!(out.index.get(b"sub", 3).is_some());
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn merge_trees_binary_conflict_honors_ours_favor() {
        let tmp = tempfile::tempdir().expect("tempdir");
        grit_test_support::git(tmp.path(), &["init", "-q", "-b", "main"]);
        let repo =
            Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("repo");
        std::fs::write(tmp.path().join("bin.dat"), [0, 1, 2]).expect("write");
        grit_test_support::git(tmp.path(), &["add", "bin.dat"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "base"]);
        let base_tree = {
            let base: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
                .trim()
                .parse()
                .expect("base");
            let obj = repo.odb().read(&base).expect("read");
            parse_commit(&obj.data).expect("parse").tree
        };
        grit_test_support::git(tmp.path(), &["switch", "-c", "topic"]);
        std::fs::write(tmp.path().join("bin.dat"), [0, 1, 3]).expect("write");
        grit_test_support::git(tmp.path(), &["add", "bin.dat"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "theirs"]);
        let theirs_tree = {
            let oid: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
                .trim()
                .parse()
                .expect("theirs");
            let obj = repo.odb().read(&oid).expect("read");
            parse_commit(&obj.data).expect("parse").tree
        };
        grit_test_support::git(tmp.path(), &["switch", "main"]);
        std::fs::write(tmp.path().join("bin.dat"), [0, 1, 4]).expect("write");
        grit_test_support::git(tmp.path(), &["add", "bin.dat"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "ours"]);
        let ours_tree = {
            let oid: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
                .trim()
                .parse()
                .expect("ours");
            let obj = repo.odb().read(&oid).expect("read");
            parse_commit(&obj.data).expect("parse").tree
        };
        let out = merge_trees_three_way(
            &repo,
            base_tree,
            ours_tree,
            theirs_tree,
            MergeFavor::Ours,
            WhitespaceMergeOptions::default(),
            None,
            TreeMergeConflictPresentation::default(),
            DirectoryRenameMode::default(),
        )
        .expect("merge");
        let entry = out.index.get(b"bin.dat", 0).expect("stage 0");
        let blob = repo.odb().read(&entry.oid).expect("blob");
        assert_eq!(blob.data, vec![0, 1, 4]);
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn merge_trees_binary_conflict_honors_theirs_favor() {
        let tmp = tempfile::tempdir().expect("tempdir");
        grit_test_support::git(tmp.path(), &["init", "-q", "-b", "main"]);
        let repo =
            Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("repo");
        std::fs::write(tmp.path().join("bin.dat"), [0, 1, 2]).expect("write");
        grit_test_support::git(tmp.path(), &["add", "bin.dat"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "base"]);
        let base: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("base");
        let base_tree = {
            let obj = repo.odb().read(&base).expect("read");
            parse_commit(&obj.data).expect("parse").tree
        };
        grit_test_support::git(tmp.path(), &["switch", "-c", "topic"]);
        std::fs::write(tmp.path().join("bin.dat"), [0, 1, 3]).expect("write");
        grit_test_support::git(tmp.path(), &["add", "bin.dat"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "theirs"]);
        let theirs_tree = {
            let oid: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
                .trim()
                .parse()
                .expect("theirs");
            let obj = repo.odb().read(&oid).expect("read");
            parse_commit(&obj.data).expect("parse").tree
        };
        grit_test_support::git(tmp.path(), &["switch", "main"]);
        std::fs::write(tmp.path().join("bin.dat"), [0, 1, 4]).expect("write");
        grit_test_support::git(tmp.path(), &["add", "bin.dat"]);
        grit_test_support::git(tmp.path(), &["commit", "-m", "ours"]);
        let ours_tree = {
            let oid: ObjectId = grit_test_support::git(tmp.path(), &["rev-parse", "HEAD"])
                .trim()
                .parse()
                .expect("ours");
            let obj = repo.odb().read(&oid).expect("read");
            parse_commit(&obj.data).expect("parse").tree
        };
        let out = merge_trees_three_way(
            &repo,
            base_tree,
            ours_tree,
            theirs_tree,
            MergeFavor::Theirs,
            WhitespaceMergeOptions::default(),
            None,
            TreeMergeConflictPresentation::default(),
            DirectoryRenameMode::default(),
        )
        .expect("merge");
        let entry = out.index.get(b"bin.dat", 0).expect("stage 0");
        let theirs_blob = repo.odb().read(&entry.oid).expect("blob");
        assert_eq!(theirs_blob.data, vec![0, 1, 3]);
    }

    fn head_tree(repo: &Repository, dir: &std::path::Path) -> ObjectId {
        let oid: ObjectId = grit_test_support::git(dir, &["rev-parse", "HEAD"])
            .trim()
            .parse()
            .expect("HEAD");
        let obj = repo.odb().read(&oid).expect("read");
        parse_commit(&obj.data).expect("parse").tree
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn merge_trees_loads_attributes_once_and_applies_them_per_path() {
        use crate::worktree_rules::file_load_counters;

        const N: usize = 20;
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();
        grit_test_support::git(dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join(".gitattributes"), "*.u merge=union\n*.bin -text\n")
            .expect("write");
        std::fs::write(dir.join(".git/info/exclude"), "*.tmp\n").expect("write");
        let body = |i: usize| -> String { (0..10).map(|j| format!("line {j} of {i}\n")).collect() };
        for i in 0..N {
            std::fs::write(dir.join(format!("f{i}.u")), "shared\n").expect("write");
            std::fs::write(dir.join(format!("f{i}.bin")), body(i)).expect("write");
            std::fs::write(dir.join(format!("f{i}.txt")), body(i)).expect("write");
        }
        grit_test_support::git(dir, &["add", "-A"]);
        grit_test_support::git(dir, &["commit", "-qm", "base"]);
        grit_test_support::git(dir, &["switch", "-qc", "topic"]);
        for i in 0..N {
            std::fs::write(dir.join(format!("f{i}.u")), "shared\ntheirs\n").expect("write");
            std::fs::write(dir.join(format!("f{i}.bin")), format!("{}tail\n", body(i)))
                .expect("write");
            std::fs::write(dir.join(format!("f{i}.txt")), format!("{}tail\n", body(i)))
                .expect("write");
        }
        grit_test_support::git(dir, &["commit", "-qam", "theirs"]);
        let repo = Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("repo");
        let theirs_tree = head_tree(&repo, dir);
        grit_test_support::git(dir, &["switch", "-q", "main"]);
        let base_tree = head_tree(&repo, dir);
        for i in 0..N {
            std::fs::write(dir.join(format!("f{i}.u")), "shared\nours\n").expect("write");
            std::fs::write(dir.join(format!("f{i}.bin")), format!("head\n{}", body(i)))
                .expect("write");
            std::fs::write(dir.join(format!("f{i}.txt")), format!("head\n{}", body(i)))
                .expect("write");
        }
        grit_test_support::git(dir, &["commit", "-qam", "ours"]);
        let ours_tree = head_tree(&repo, dir);

        file_load_counters::reset();
        let out = merge_trees_three_way(
            &repo,
            base_tree,
            ours_tree,
            theirs_tree,
            MergeFavor::default(),
            WhitespaceMergeOptions::default(),
            None,
            TreeMergeConflictPresentation::default(),
            DirectoryRenameMode::default(),
        )
        .expect("merge");
        assert!(
            file_load_counters::max_reads_per_file() <= 1,
            "attribute/ignore files must be read at most once per tree merge, got {}",
            file_load_counters::max_reads_per_file()
        );
        assert_eq!(
            file_load_counters::reads_for(&dir.join(".git/info/exclude")),
            1
        );

        for i in 0..N {
            let union = out
                .index
                .get(format!("f{i}.u").as_bytes(), 0)
                .expect("union path merges cleanly");
            let blob = repo.odb().read(&union.oid).expect("blob");
            assert_eq!(blob.data, b"shared\nours\ntheirs\n");

            let bin = format!("f{i}.bin");
            for stage in 1..=3 {
                assert!(
                    out.index.get(bin.as_bytes(), stage).is_some(),
                    "-text path {bin} conflicts as binary (stage {stage})"
                );
            }
            assert!(!out.conflict_content.contains_key(bin.as_bytes()));

            let txt = out
                .index
                .get(format!("f{i}.txt").as_bytes(), 0)
                .expect("text path merges cleanly");
            let blob = repo.odb().read(&txt.oid).expect("blob");
            assert_eq!(blob.data, format!("head\n{}tail\n", body(i)).as_bytes());
        }
    }

    #[test]
    fn augment_renames_includes_same_blob_pairs() {
        let mut base = BTreeMap::new();
        let mut side = BTreeMap::new();
        let oid = zero_oid();
        let old_e = IndexEntry {
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
            flags: 0,
            flags_extended: None,
            path: b"old".to_vec(),
            base_index_pos: 0,
        };
        let mut new_e = old_e.clone();
        new_e.path = b"new".to_vec();
        base.insert(b"old".to_vec(), old_e);
        side.insert(b"new".to_vec(), new_e);
        let mut pairs = Vec::new();
        augment_renames_with_same_blob(&base, &side, &mut pairs);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].2, 100);
    }

    fn sparse_merge_test_entry(path: &str, oid: ObjectId, stage: u8) -> IndexEntry {
        let mut flags = path.len().min(0xFFF) as u16;
        if stage > 0 {
            flags |= (stage as u16) << 12;
        }
        IndexEntry {
            ctime_sec: 10,
            ctime_nsec: 0,
            mtime_sec: 10,
            mtime_nsec: 0,
            dev: 1,
            ino: 1,
            mode: MODE_REGULAR,
            uid: 0,
            gid: 0,
            size: 4,
            oid,
            flags,
            flags_extended: None,
            path: path.as_bytes().to_vec(),
            base_index_pos: 0,
        }
    }

    #[test]
    fn apply_tree_merge_output_to_index_sparse_updates_keep_sorted() {
        let oid = zero_oid();
        let changed_oid =
            ObjectId::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").expect("oid");
        let mut index = Index::new();
        for i in 0..800 {
            index.push_entry_unsorted(sparse_merge_test_entry(
                &format!("wide/f{i:04}.txt"),
                oid,
                0,
            ));
        }
        index.sort();
        assert!(index.entries_sorted);
        let before_len = index.entries().len();

        let del_path = b"wide/f0100.txt".to_vec();
        let conflict_path = b"wide/f0200.txt".to_vec();
        let update_path = b"wide/f0300.txt".to_vec();
        let mut merge_index = Index::new();
        merge_index.push_entry_unsorted(sparse_merge_test_entry("wide/f0200.txt", oid, 2));
        merge_index.push_entry_unsorted(sparse_merge_test_entry("wide/f0200.txt", changed_oid, 3));
        merge_index.push_entry_unsorted(sparse_merge_test_entry("wide/f0300.txt", changed_oid, 0));
        merge_index.sort();
        let mut touched = HashSet::new();
        touched.insert(del_path.clone());
        touched.insert(conflict_path.clone());
        touched.insert(update_path.clone());
        let merged = TreeMergeOutput {
            index: merge_index,
            conflict_content: BTreeMap::new(),
            result_tree: oid,
            touched_paths: touched,
            rename_conflicts: Vec::new(),
            directory_rename_conflicts: Vec::new(),
        };

        apply_tree_merge_output_to_index(&mut index, &merged);

        assert!(index.entries_sorted);
        assert!(index.get(&del_path, 0).is_none());
        assert!(index.get(&conflict_path, 2).is_some());
        assert!(index.get(&conflict_path, 3).is_some());
        assert_eq!(index.get(&update_path, 0).map(|e| e.oid), Some(changed_oid));
        assert_eq!(index.entries().len(), before_len);
    }

    #[test]
    fn apply_tree_merge_worktree_index_skips_fsmonitor_only_diff() {
        let dir = tempfile::tempdir().expect("tempdir");
        grit_test_support::git(dir.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(dir.path().join("tracked.txt"), "hello\n").expect("write");
        grit_test_support::git(dir.path(), &["add", "tracked.txt"]);
        let repo =
            Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("repo");
        let mut index = repo.index_handle().load().expect("index");
        let row = index.get(b"tracked.txt", 0).expect("row").clone();
        let oid = row.oid;
        let mut indexed = row.clone();
        indexed.set_fsmonitor_valid(true);
        index.overwrite_stage0_entries(vec![indexed]);
        let entries_before = index.entries().to_vec();

        let mut merge_index = Index::new();
        merge_index.push_entry_unsorted(IndexEntry {
            flags_extended: None,
            size: 0,
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            ..sparse_merge_test_entry("tracked.txt", oid, 0)
        });
        merge_index.sort();
        let mut touched = HashSet::new();
        touched.insert(b"tracked.txt".to_vec());
        let merged = TreeMergeOutput {
            index: merge_index,
            conflict_content: BTreeMap::new(),
            result_tree: oid,
            touched_paths: touched,
            rename_conflicts: Vec::new(),
            directory_rename_conflicts: Vec::new(),
        };

        apply_tree_merge_output_to_worktree_index(&repo, &mut index, &merged).expect("apply");

        assert_eq!(index.entries(), entries_before.as_slice());
        assert!(index.get(b"tracked.txt", 0).unwrap().fsmonitor_valid());
    }

    #[test]
    fn apply_tree_merge_worktree_index_applies_oid_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        grit_test_support::git(dir.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(dir.path().join("tracked.txt"), "hello\n").expect("write");
        grit_test_support::git(dir.path(), &["add", "tracked.txt"]);
        let repo =
            Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("repo");
        let mut index = repo.index_handle().load().expect("index");
        let old_oid = index.get(b"tracked.txt", 0).expect("row").oid;
        let mut indexed = index.get(b"tracked.txt", 0).expect("row").clone();
        indexed.set_fsmonitor_valid(true);
        index.overwrite_stage0_entries(vec![indexed]);

        let new_oid = repo
            .odb()
            .write(crate::objects::ObjectKind::Blob, b"other\n")
            .expect("write");
        assert_ne!(old_oid, new_oid);

        let mut merge_index = Index::new();
        merge_index.push_entry_unsorted(sparse_merge_test_entry("tracked.txt", new_oid, 0));
        merge_index.sort();
        let mut touched = HashSet::new();
        touched.insert(b"tracked.txt".to_vec());
        let merged = TreeMergeOutput {
            index: merge_index,
            conflict_content: BTreeMap::new(),
            result_tree: new_oid,
            touched_paths: touched,
            rename_conflicts: Vec::new(),
            directory_rename_conflicts: Vec::new(),
        };

        apply_tree_merge_output_to_worktree_index(&repo, &mut index, &merged).expect("apply");

        assert_eq!(index.get(b"tracked.txt", 0).map(|e| e.oid), Some(new_oid));
    }
}
