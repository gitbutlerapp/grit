//! Rename-aware three-way tree merge for cherry-pick / revert style merges.
//!
//! Flattens trees to path → [`IndexEntry`] maps, aligns paths using rename detection
//! between base↔ours and base↔theirs (same idea as Git's merge-ort rename paths), then
//! runs path-by-path three-way rules with optional content merge.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::diff::{detect_renames, diff_trees, DiffStatus};
use crate::index::{Index, IndexEntry};
use crate::merge_file::{merge, ConflictStyle, MergeFavor, MergeInput};
use crate::objects::{parse_tree, ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::repo::Repository;
use crate::write_tree::write_tree_from_index;

/// Result of merging three trees with optional conflict-marker blobs for checkout.
#[derive(Debug)]
pub struct TreeMergeOutput {
    /// Merged index (may include unmerged stages).
    pub index: Index,
    /// Conflict-marker blob OIDs keyed by path (stage-2 path bytes).
    pub conflict_content: BTreeMap<Vec<u8>, ObjectId>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct WhitespaceMergeOptions {
    pub ignore_all_space: bool,
    pub ignore_space_change: bool,
    pub ignore_space_at_eol: bool,
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
}

impl Default for TreeMergeConflictPresentation<'_> {
    fn default() -> Self {
        Self {
            label_ours: "HEAD",
            label_theirs: TheirsConflictLabel::PathUtf8,
            label_base: "merged common ancestors",
            style: ConflictStyle::Merge,
            checkout_merge: false,
        }
    }
}
#[expect(clippy::too_many_arguments)]
/// Merge `base` + `ours` + `their` trees (by OID) into an index using rename detection.
///
/// Paths in the result follow the **ours** tree naming. Rename detection uses a 50%
/// similarity threshold, matching Git's default rename detection during merges.
///
/// # Parameters
///
/// - `base_tree` — common ancestor tree (parent tree for cherry-pick, reverted commit tree for revert).
/// - `ours_tree` — current branch tree (HEAD during pick/revert).
/// - `theirs_tree` — tree being applied (picked commit for cherry-pick, parent of reverted commit for revert).
/// - `favor` / `ws` — merge strategy favour and whitespace options for textual merges.
/// - `diff_algorithm` — optional line diff algorithm name used for textual merges.
/// - `presentation` — conflict marker labels and style (`label_base` is the diff3 "base" name).
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
) -> crate::error::Result<TreeMergeOutput> {
    let odb = &repo.odb;
    let base = tree_to_map(tree_to_index_entries(repo, &base_tree, "")?);
    let ours = tree_to_map(tree_to_index_entries(repo, &ours_tree, "")?);
    let theirs = tree_to_map(tree_to_index_entries(repo, &theirs_tree, "")?);

    let ours_old_to_new = rename_pairs_base_to_other(odb, &base_tree, &ours_tree)?;
    let theirs_pairs = rename_pairs_base_to_other(odb, &base_tree, &theirs_tree)?;

    let mut ours_new_to_old: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    let mut ours_best_by_dest: HashMap<Vec<u8>, (Vec<u8>, u32)> = HashMap::new();
    for (old, new, score) in &ours_old_to_new {
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

    three_way_on_aligned_paths(
        repo,
        &base,
        &ours,
        &theirs,
        &ours_new_to_old,
        &theirs_old_to_new,
        favor,
        ws,
        diff_algorithm,
        presentation,
    )
}

type RenamePathPair = (Vec<u8>, Vec<u8>, u32);

fn rename_pairs_base_to_other(
    odb: &Odb,
    base_tree: &ObjectId,
    other_tree: &ObjectId,
) -> crate::error::Result<Vec<RenamePathPair>> {
    let mut entries = diff_trees(odb, Some(base_tree), Some(other_tree), "")?;
    entries = detect_renames(odb, None, entries, 50);
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
        out.push((old.into_bytes(), new.into_bytes(), score));
    }
    Ok(out)
}
#[expect(clippy::too_many_arguments)]
fn three_way_on_aligned_paths(
    repo: &Repository,
    base: &HashMap<Vec<u8>, IndexEntry>,
    ours: &HashMap<Vec<u8>, IndexEntry>,
    theirs: &HashMap<Vec<u8>, IndexEntry>,
    ours_new_to_old: &HashMap<Vec<u8>, Vec<u8>>,
    theirs_old_to_new: &HashMap<Vec<u8>, Vec<u8>>,
    favor: MergeFavor,
    ws: WhitespaceMergeOptions,
    diff_algorithm: Option<&str>,
    presentation: TreeMergeConflictPresentation<'_>,
) -> crate::error::Result<TreeMergeOutput> {
    let mut out = Index::new();
    let mut conflict_content = BTreeMap::new();
    let mut handled_base: HashSet<Vec<u8>> = HashSet::new();
    let mut handled_theirs: HashSet<Vec<u8>> = HashSet::new();

    for op in sorted_paths(ours.keys()) {
        let bp = if let Some(old) = ours_new_to_old.get(&op) {
            Some(old.clone())
        } else if base.contains_key(&op) {
            Some(op.clone())
        } else {
            None
        };

        let tp = if let Some(ref bpath) = bp {
            theirs_old_to_new
                .get(bpath)
                .cloned()
                .unwrap_or_else(|| bpath.clone())
        } else {
            op.clone()
        };

        let b = bp.as_ref().and_then(|p| base.get(p));
        let o = ours.get(&op);
        let t = theirs.get(&tp);
        if t.is_some() {
            handled_theirs.insert(tp.clone());
        }

        if let Some(ref p) = bp {
            handled_base.insert(p.clone());
        }

        // When our branch still has the file at the same path as the merge base, but the side
        // being applied renamed that path, the result must use their pathname (matches Git
        // cherry-pick / merge-ort: e.g. base+HEAD at `file.txt`, picked commit has `renamed.txt`).
        let out_path = if bp.as_ref().is_some_and(|b| b == &op) && tp != op {
            tp.clone()
        } else {
            op.clone()
        };

        merge_one_path(
            repo,
            &mut out,
            &mut conflict_content,
            &out_path,
            b,
            o,
            t,
            favor,
            ws,
            diff_algorithm,
            presentation,
        )?;
    }

    for bp in sorted_paths(base.keys()) {
        if handled_base.contains(&bp) {
            continue;
        }
        let tp = theirs_old_to_new
            .get(&bp)
            .cloned()
            .unwrap_or_else(|| bp.clone());
        let b = base.get(&bp);
        let o: Option<&IndexEntry> = None;
        let t = theirs.get(&tp);
        if t.is_some() {
            handled_theirs.insert(tp.clone());
        }
        merge_one_path(
            repo,
            &mut out,
            &mut conflict_content,
            &bp,
            b,
            o,
            t,
            favor,
            ws,
            diff_algorithm,
            presentation,
        )?;
    }

    for tp in sorted_paths(theirs.keys()) {
        if handled_theirs.contains(&tp) {
            continue;
        }
        let b: Option<&IndexEntry> = None;
        let o: Option<&IndexEntry> = None;
        let t = theirs.get(&tp);
        merge_one_path(
            repo,
            &mut out,
            &mut conflict_content,
            &tp,
            b,
            o,
            t,
            favor,
            ws,
            diff_algorithm,
            presentation,
        )?;
    }

    out.sort();
    Ok(TreeMergeOutput {
        index: out,
        conflict_content,
    })
}

fn sorted_paths<'a>(keys: impl Iterator<Item = &'a Vec<u8>>) -> Vec<Vec<u8>> {
    let mut v: Vec<Vec<u8>> = keys.cloned().collect();
    v.sort();
    v
}
#[expect(clippy::too_many_arguments)]
fn merge_one_path(
    repo: &Repository,
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
            if be.mode == 0o160000 && oe.mode == 0o160000 && te.mode == 0o160000 =>
        {
            if same_blob(oe, te) {
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
                repo,
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
                repo,
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
        (Some(be), Some(oe), None) if same_blob(be, oe) => {}
        (Some(be), None, Some(te)) if same_blob(be, te) => {}
        (Some(be), Some(oe), None) => {
            stage_entry(index, out_path, be, 1);
            stage_entry(index, out_path, oe, 2);
        }
        (Some(be), None, Some(te)) => {
            stage_entry(index, out_path, be, 1);
            stage_entry(index, out_path, te, 3);
        }
        (None, None, None) => {}
    }
    Ok(())
}

fn path_len_flags(path: &[u8]) -> u16 {
    path.len().min(0xFFF) as u16
}

fn same_blob(a: &IndexEntry, b: &IndexEntry) -> bool {
    a.oid == b.oid && a.mode == b.mode
}

fn stage_entry(index: &mut Index, path: &[u8], src: &IndexEntry, stage: u8) {
    let mut e = src.clone();
    e.path = path.to_vec();
    e.flags = path_len_flags(path) | ((stage as u16) << 12);
    index.push_entry_unsorted(e);
}
#[expect(clippy::too_many_arguments)]
fn content_merge_or_conflict(
    repo: &Repository,
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
    if base.mode == 0o160000 || ours.mode == 0o160000 || theirs.mode == 0o160000 {
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
    let base_obj = repo.odb.read(&base.oid)?;
    let ours_obj = repo.odb.read(&ours.oid)?;
    let theirs_obj = repo.odb.read(&theirs.oid)?;

    if crate::merge_file::is_binary(&base_obj.data)
        || crate::merge_file::is_binary(&ours_obj.data)
        || crate::merge_file::is_binary(&theirs_obj.data)
    {
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

    let path_label = String::from_utf8_lossy(path);
    let label_theirs: std::borrow::Cow<'_, str> = match presentation.label_theirs {
        TheirsConflictLabel::PathUtf8 => path_label.clone(),
        TheirsConflictLabel::Fixed(s) => std::borrow::Cow::Borrowed(s),
    };
    let input = MergeInput {
        base: &base_obj.data,
        ours: &ours_obj.data,
        theirs: &theirs_obj.data,
        label_ours: presentation.label_ours,
        label_base: presentation.label_base,
        label_theirs: label_theirs.as_ref(),
        favor,
        style: presentation.style,
        marker_size: 7,
        diff_algorithm: diff_algorithm.map(str::to_owned),
        ignore_all_space: ws.ignore_all_space,
        ignore_space_change: ws.ignore_space_change,
        ignore_space_at_eol: ws.ignore_space_at_eol,
        ignore_cr_at_eol: ws.ignore_cr_at_eol,
    };

    let result = merge(&input)?;

    if result.conflicts > 0 {
        let conflict_oid = repo.odb.write(ObjectKind::Blob, &result.content)?;
        conflict_content.insert(path.to_vec(), conflict_oid);
        stage_entry(index, path, base, 1);
        stage_entry(index, path, ours, 2);
        stage_entry(index, path, theirs, 3);
    } else {
        let merged_oid = repo.odb.write(ObjectKind::Blob, &result.content)?;
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
    repo: &Repository,
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
    // Gitlink add/add: no textual merge is possible; leave unmerged stages.
    if ours.mode == 0o160000 || theirs.mode == 0o160000 {
        stage_entry(index, path, ours, 2);
        stage_entry(index, path, theirs, 3);
        return Ok(());
    }

    let ours_obj = repo.odb.read(&ours.oid)?;
    let theirs_obj = repo.odb.read(&theirs.oid)?;

    // Binary add/add: honour an explicit favour, else leave unmerged stages.
    if crate::merge_file::is_binary(&ours_obj.data)
        || crate::merge_file::is_binary(&theirs_obj.data)
    {
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

    let path_label = String::from_utf8_lossy(path);
    let label_theirs: std::borrow::Cow<'_, str> = match presentation.label_theirs {
        TheirsConflictLabel::PathUtf8 => path_label.clone(),
        TheirsConflictLabel::Fixed(s) => std::borrow::Cow::Borrowed(s),
    };
    let input = MergeInput {
        base: b"",
        ours: &ours_obj.data,
        theirs: &theirs_obj.data,
        label_ours: presentation.label_ours,
        label_base: presentation.label_base,
        label_theirs: label_theirs.as_ref(),
        favor,
        style: presentation.style,
        marker_size: 7,
        diff_algorithm: diff_algorithm.map(str::to_owned),
        ignore_all_space: ws.ignore_all_space,
        ignore_space_change: ws.ignore_space_change,
        ignore_space_at_eol: ws.ignore_space_at_eol,
        ignore_cr_at_eol: ws.ignore_cr_at_eol,
    };

    let result = merge(&input)?;

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
        let conflict_oid = repo.odb.write(ObjectKind::Blob, &result.content)?;
        conflict_content.insert(path.to_vec(), conflict_oid);
        stage_entry(index, path, ours, 2);
        stage_entry(index, path, theirs, 3);
    } else {
        // A favour (ours/theirs/union) or identical-after-normalisation merge resolved the
        // content cleanly: record a single stage-0 entry with the merged blob.
        let merged_oid = repo.odb.write(ObjectKind::Blob, &result.content)?;
        let mut entry = ours.clone();
        entry.path = path.to_vec();
        entry.flags = path_len_flags(path);
        entry.oid = merged_oid;
        index.push_entry_unsorted(entry);
    }

    Ok(())
}

fn tree_to_index_entries(
    repo: &Repository,
    oid: &ObjectId,
    prefix: &str,
) -> crate::error::Result<Vec<IndexEntry>> {
    let obj = repo.odb.read(oid)?;
    if obj.kind != ObjectKind::Tree {
        return Err(crate::error::Error::CorruptObject(format!(
            "expected tree, got {}",
            obj.kind.as_str()
        )));
    }
    let entries = parse_tree(&obj.data)?;
    let mut result = Vec::new();

    for te in entries {
        let name = String::from_utf8_lossy(&te.name).into_owned();
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };

        if te.mode == 0o040000 {
            let sub = tree_to_index_entries(repo, &te.oid, &path)?;
            result.extend(sub);
        } else {
            let path_bytes = path.into_bytes();
            result.push(IndexEntry {
                ctime_sec: 0,
                ctime_nsec: 0,
                mtime_sec: 0,
                mtime_nsec: 0,
                dev: 0,
                ino: 0,
                mode: te.mode,
                uid: 0,
                gid: 0,
                size: 0,
                oid: te.oid,
                flags: path_bytes.len().min(0xFFF) as u16,
                flags_extended: None,
                path: path_bytes,
                base_index_pos: 0,
            });
        }
    }
    Ok(result)
}

fn tree_to_map(entries: Vec<IndexEntry>) -> HashMap<Vec<u8>, IndexEntry> {
    let mut out = HashMap::new();
    for e in entries {
        out.insert(e.path.clone(), e);
    }
    out
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
