//! Multi-parent combined tree diff (Git `diff_tree_paths` / `find_paths_multitree`).
//!
//! Implements the `D(T,P1...Pn)` walk from `tree-diff.c` for merge commits, producing
//! per-path parent status letters used by `git diff-tree -c --name-status` and
//! `--find-object` filtering (`combined_objfind` in `combine-diff.c`).

use crate::diff::zero_oid;
use crate::error::Result;
use crate::objects::{parse_commit, parse_tree, tree_entry_cmp, ObjectId, ObjectKind, TreeEntry};
use crate::odb::Odb;

/// One character per parent for raw / name-status combined output (`A`/`M`/`D`).
#[must_use]
pub fn combined_parent_status_char(s: CombinedParentStatus) -> char {
    match s {
        CombinedParentStatus::Added => 'A',
        CombinedParentStatus::Modified => 'M',
        CombinedParentStatus::Deleted => 'D',
        CombinedParentStatus::Renamed => 'R',
    }
}

/// Git raw combined line (`::::modes... oids... MM\tpath`).
///
/// When `combined_all_paths` is set and a parent side is a rename, the source name for every
/// parent is emitted (tab-separated) before the merge-result path, each C-quoted with `quote_fully`.
#[must_use]
pub fn format_combined_raw_line_all_paths(
    p: &CombinedDiffPath,
    abbrev_len: Option<usize>,
    combined_all_paths: bool,
    quote_fully: bool,
) -> String {
    if !combined_all_paths {
        return format_combined_raw_line(p, abbrev_len);
    }
    // Emit metadata (modes/oids/status) followed by one source name per parent and the merge path.
    // We can't string-split the single-path line on `\t` because funny names contain tabs.
    let mut line = combined_raw_meta(p, abbrev_len);
    for side in &p.parents {
        let name = side.rename_from.as_deref().unwrap_or(&p.path);
        line.push('\t');
        line.push_str(&crate::quote_path::quote_c_style(name, quote_fully));
    }
    line.push('\t');
    line.push_str(&crate::quote_path::quote_c_style(&p.path, quote_fully));
    line
}

/// Git raw combined line (`::::modes... oids... MM\tpath`).
#[must_use]
pub fn format_combined_raw_line(p: &CombinedDiffPath, abbrev_len: Option<usize>) -> String {
    format!("{}\t{}", combined_raw_meta(p, abbrev_len), p.path)
}

/// Metadata portion of a combined raw line: `::::modes... oids... <status>` (no trailing tab/path).
#[must_use]
pub fn combined_raw_meta(p: &CombinedDiffPath, abbrev_len: Option<usize>) -> String {
    let n = p.parents.len();
    let mut colons = String::with_capacity(n);
    for _ in 0..n {
        colons.push(':');
    }
    let mut modes = String::new();
    for side in &p.parents {
        modes.push_str(&format!("{:06o} ", side.mode));
    }
    modes.push_str(&format!("{:06o}", p.merge_mode));
    // When OIDs are abbreviated, Git appends `...` if `GIT_PRINT_SHA1_ELLIPSIS=yes`
    // (matches the non-combined raw format).
    let ellipsis = if abbrev_len.is_some()
        && crate::environment::Environment::capture_process()
            .var("GIT_PRINT_SHA1_ELLIPSIS")
            .as_deref()
            == Some("yes")
    {
        "..."
    } else {
        ""
    };
    let mut oids = String::new();
    for side in &p.parents {
        let h = format!("{}", side.oid);
        let disp = if let Some(len) = abbrev_len {
            &h[..len.min(h.len())]
        } else {
            h.as_str()
        };
        oids.push(' ');
        oids.push_str(disp);
        oids.push_str(ellipsis);
    }
    let rh = format!("{}", p.merge_oid);
    let rdisp = if let Some(len) = abbrev_len {
        &rh[..len.min(rh.len())]
    } else {
        rh.as_str()
    };
    oids.push(' ');
    oids.push_str(rdisp);
    oids.push_str(ellipsis);
    oids.push(' ');
    let status: String = p
        .parents
        .iter()
        .map(|s| combined_parent_status_char(s.status))
        .collect();
    format!("{colons}{modes}{oids}{status}")
}

/// Per-parent coarse status in a combined diff (`A` / `M` / `D`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CombinedParentStatus {
    /// Parent lacks the path; merge tree added it.
    Added,
    /// Parent has the path at the same name as the merge result.
    Modified,
    /// Path removed from merge (only in parents that had it at the min name).
    Deleted,
    /// Path renamed relative to this parent (`-M`/`--combined-all-paths`); the source name is
    /// carried on [`CombinedParentSide::rename_from`].
    Renamed,
}

/// One path in a combined diff: merge result tree vs each parent tree.
#[derive(Debug, Clone)]
pub struct CombinedDiffPath {
    /// Path relative to repository root.
    pub path: String,
    /// Mode and OID on the merge result side (`0` / zero OID when deleted from merge).
    pub merge_mode: u32,
    pub merge_oid: ObjectId,
    /// One slot per parent commit (same order as `parents` passed to [`combined_diff_paths_filtered`]).
    pub parents: Vec<CombinedParentSide>,
}

/// One parent's contribution at a combined-diff path.
#[derive(Debug, Clone)]
pub struct CombinedParentSide {
    pub mode: u32,
    pub oid: ObjectId,
    pub status: CombinedParentStatus,
    /// Source path when this parent saw the result as a rename (`-M`); `None` otherwise.
    pub rename_from: Option<String>,
}

/// Options for the multitree walk.
#[derive(Debug, Clone, Default)]
pub struct CombinedTreeDiffOptions {
    /// Recurse into sub-trees (`diff_options.flags.recursive`).
    pub recursive: bool,
    /// Emit tree directory lines when recursing (`tree_in_recursive`, e.g. `--find-object`).
    pub tree_in_recursive: bool,
}

fn is_tree_mode(mode: u32) -> bool {
    (mode & 0o170000) == 0o040000
}

fn read_tree_entries(odb: &Odb, oid: Option<&ObjectId>) -> Result<Vec<TreeEntry>> {
    let Some(oid) = oid else {
        return Ok(Vec::new());
    };
    let obj = odb.read(oid)?;
    if obj.kind != ObjectKind::Tree {
        return Ok(Vec::new());
    }
    parse_tree(&obj.data)
}

fn tree_entry_pathcmp(a: Option<&TreeEntry>, b: Option<&TreeEntry>) -> std::cmp::Ordering {
    match (a, b) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(e1), Some(e2)) => tree_entry_cmp(
            &e1.name,
            is_tree_mode(e1.mode),
            &e2.name,
            is_tree_mode(e2.mode),
        ),
    }
}

fn combined_matches_find_object(path: &CombinedDiffPath, find: Option<&ObjectId>) -> bool {
    let Some(target) = find else {
        return true;
    };
    if path.merge_oid == *target {
        return true;
    }
    path.parents.iter().any(|p| p.oid == *target)
}

fn emit_combined_path(
    path: String,
    merge_entry: Option<&TreeEntry>,
    tp: &[Vec<TreeEntry>],
    tp_idx: &[usize],
    parent_neq: &[bool],
    find_object: Option<&ObjectId>,
    out: &mut Vec<CombinedDiffPath>,
) {
    let nparent = tp.len();
    let mut parents = Vec::with_capacity(nparent);

    if let Some(te) = merge_entry {
        for i in 0..nparent {
            let tpi_valid = !parent_neq[i];
            let (mode_i, oid_i, status) = if tpi_valid {
                let e = &tp[i][tp_idx[i]];
                (e.mode, e.oid, CombinedParentStatus::Modified)
            } else {
                (0u32, zero_oid(), CombinedParentStatus::Added)
            };
            parents.push(CombinedParentSide {
                mode: mode_i,
                oid: oid_i,
                status,
                rename_from: None,
            });
        }
        let p = CombinedDiffPath {
            path,
            merge_mode: te.mode,
            merge_oid: te.oid,
            parents,
        };
        if combined_matches_find_object(&p, find_object) {
            out.push(p);
        }
    } else {
        for i in 0..nparent {
            let tpi_valid = !parent_neq[i];
            let (mode_i, oid_i, status) = if tpi_valid {
                let e = &tp[i][tp_idx[i]];
                (e.mode, e.oid, CombinedParentStatus::Deleted)
            } else {
                (0u32, zero_oid(), CombinedParentStatus::Added)
            };
            parents.push(CombinedParentSide {
                mode: mode_i,
                oid: oid_i,
                status,
                rename_from: None,
            });
        }
        let p = CombinedDiffPath {
            path,
            merge_mode: 0,
            merge_oid: zero_oid(),
            parents,
        };
        if combined_matches_find_object(&p, find_object) {
            out.push(p);
        }
    }
}

fn should_recurse_dir(_path: &str, _opt: &CombinedTreeDiffOptions) -> bool {
    // Git's `diff_tree_paths` / combined merge diff always recurses into subtrees
    // (see `diff_tree_combined`: `diffopts.flags.recursive = 1`).
    true
}

fn ll_diff_tree_paths(
    odb: &Odb,
    out: &mut Vec<CombinedDiffPath>,
    base_path: &str,
    opt: &CombinedTreeDiffOptions,
    merge_oid: Option<&ObjectId>,
    parents_oid: &[Option<ObjectId>],
    find_object: Option<&ObjectId>,
) -> Result<()> {
    let nparent = parents_oid.len();
    let t_entries = read_tree_entries(odb, merge_oid)?;
    let mut tp_entries: Vec<Vec<TreeEntry>> = Vec::with_capacity(nparent);
    for po in parents_oid {
        tp_entries.push(read_tree_entries(odb, po.as_ref())?);
    }

    let mut ti = 0usize;
    let mut tp_idx = vec![0usize; nparent];

    loop {
        let t_cur = t_entries.get(ti);

        if t_cur.is_none() && (0..nparent).all(|i| tp_idx[i] >= tp_entries[i].len()) {
            break;
        }

        let mut imin = 0usize;
        if nparent > 0 {
            for i in 1..nparent {
                let e_imin = tp_entries[imin].get(tp_idx[imin]);
                let e_i = tp_entries[i].get(tp_idx[i]);
                if tree_entry_pathcmp(e_i, e_imin) == std::cmp::Ordering::Less {
                    imin = i;
                }
            }
        }

        let mut parent_neq = vec![false; nparent];
        for i in 0..nparent {
            let e_imin = tp_entries[imin].get(tp_idx[imin]);
            let e_i = tp_entries[i].get(tp_idx[i]);
            parent_neq[i] = tree_entry_pathcmp(e_i, e_imin) != std::cmp::Ordering::Equal;
        }
        for ne in parent_neq.iter_mut().take(imin) {
            *ne = true;
        }

        let p_min = tp_entries[imin].get(tp_idx[imin]);

        let cmp = tree_entry_pathcmp(t_cur, p_min);

        if cmp == std::cmp::Ordering::Equal {
            if let Some(te) = t_cur {
                // Combined-diff intersection rule (git tree-diff.c ll_diff_tree_paths,
                // cmp == 0 branch): skip the path when it is identical to *some* parent
                // that has it at p[imin]. A path is only interesting for a combined diff
                // if it differs from every parent. (find_copies_harder is not modelled
                // here, matching the fast multitree path.)
                let mut skip_emit = false;
                for i in 0..nparent {
                    // p[i] > p[imin]: path absent in this parent -> "+t" in D(T,Pi).
                    if parent_neq[i] {
                        continue;
                    }
                    if let Some(pe) = tp_entries[i].get(tp_idx[i]) {
                        if pe.oid == te.oid && pe.mode == te.mode {
                            // diff(t, pi) == empty: result matches this parent -> skip.
                            skip_emit = true;
                            break;
                        }
                    }
                }

                if !skip_emit {
                    let name = std::str::from_utf8(&te.name).unwrap_or("");
                    let full_path = if base_path.is_empty() {
                        name.to_string()
                    } else {
                        format!("{base_path}/{name}")
                    };

                    let isdir = is_tree_mode(te.mode);
                    let mut do_emit = true;
                    if isdir && should_recurse_dir(&full_path, opt) {
                        do_emit = opt.tree_in_recursive;
                    }

                    if do_emit {
                        emit_combined_path(
                            full_path.clone(),
                            Some(te),
                            &tp_entries,
                            &tp_idx,
                            &parent_neq,
                            find_object,
                            out,
                        );
                    }

                    if isdir && should_recurse_dir(&full_path, opt) {
                        let merge_child = te.oid;
                        let mut child_parent_opts = vec![None; nparent];
                        for i in 0..nparent {
                            if parent_neq[i] {
                                continue;
                            }
                            if let Some(pe) = tp_entries[i].get(tp_idx[i]) {
                                if tree_entry_cmp(
                                    &pe.name,
                                    is_tree_mode(pe.mode),
                                    &te.name,
                                    is_tree_mode(te.mode),
                                ) == std::cmp::Ordering::Equal
                                {
                                    child_parent_opts[i] = Some(pe.oid);
                                }
                            }
                        }
                        ll_diff_tree_paths(
                            odb,
                            out,
                            &full_path,
                            opt,
                            Some(&merge_child),
                            &child_parent_opts,
                            find_object,
                        )?;
                    }
                }
            }

            ti += 1;
            for i in 0..nparent {
                if !parent_neq[i] {
                    tp_idx[i] += 1;
                }
            }
        } else if cmp == std::cmp::Ordering::Less {
            let Some(te) = t_cur else {
                ti += 1;
                continue;
            };
            let name = std::str::from_utf8(&te.name).unwrap_or("");
            let full_path = if base_path.is_empty() {
                name.to_string()
            } else {
                format!("{base_path}/{name}")
            };
            let isdir = is_tree_mode(te.mode);
            let mut do_emit = true;
            if isdir && should_recurse_dir(&full_path, opt) {
                do_emit = opt.tree_in_recursive;
            }
            if do_emit {
                let all_parents_absent: Vec<bool> = vec![true; nparent];
                emit_combined_path(
                    full_path.clone(),
                    Some(te),
                    &tp_entries,
                    &tp_idx,
                    &all_parents_absent,
                    find_object,
                    out,
                );
            }
            if isdir && should_recurse_dir(&full_path, opt) {
                let merge_child = te.oid;
                let child_parent_opts = vec![None; nparent];
                ll_diff_tree_paths(
                    odb,
                    out,
                    &full_path,
                    opt,
                    Some(&merge_child),
                    &child_parent_opts,
                    find_object,
                )?;
            }
            ti += 1;
        } else {
            // git tree-diff.c (t > p[imin] branch): "-p[imin]" is only in D(T,Pi)
            // when *every* parent has the path at p[imin] (deleted from all of them).
            // Skip when any parent lacks it (parent_neq[i] set).
            let skip_emit_tp = (0..nparent).any(|i| parent_neq[i]);
            if !skip_emit_tp {
                if let Some(pe) = p_min {
                    let name = std::str::from_utf8(&pe.name).unwrap_or("");
                    let full_path = if base_path.is_empty() {
                        name.to_string()
                    } else {
                        format!("{base_path}/{name}")
                    };
                    let isdir = is_tree_mode(pe.mode);
                    let mut do_emit = true;
                    if isdir && should_recurse_dir(&full_path, opt) {
                        do_emit = opt.tree_in_recursive;
                    }
                    if do_emit {
                        emit_combined_path(
                            full_path.clone(),
                            None,
                            &tp_entries,
                            &tp_idx,
                            &parent_neq,
                            find_object,
                            out,
                        );
                    }
                    if isdir && should_recurse_dir(&full_path, opt) {
                        let mut child_parent_opts = vec![None; nparent];
                        for i in 0..nparent {
                            if !parent_neq[i] {
                                if let Some(e) = tp_entries[i].get(tp_idx[i]) {
                                    child_parent_opts[i] = Some(e.oid);
                                }
                            }
                        }
                        ll_diff_tree_paths(
                            odb,
                            out,
                            &full_path,
                            opt,
                            None,
                            &child_parent_opts,
                            find_object,
                        )?;
                    }
                }
            }
            for i in 0..nparent {
                if !parent_neq[i] {
                    tp_idx[i] += 1;
                }
            }
        }
    }

    Ok(())
}

/// Build combined-diff paths for a merge commit's tree against each parent's tree.
pub fn combined_diff_paths_filtered(
    odb: &Odb,
    commit_tree: &ObjectId,
    parents: &[ObjectId],
    walk: &CombinedTreeDiffOptions,
    find_object: Option<&ObjectId>,
) -> Result<Vec<CombinedDiffPath>> {
    if parents.is_empty() {
        return Ok(Vec::new());
    }
    let mut parent_trees = Vec::with_capacity(parents.len());
    for p in parents {
        let obj = odb.read(p)?;
        if obj.kind != ObjectKind::Commit {
            return Ok(Vec::new());
        }
        let c = parse_commit(&obj.data)?;
        parent_trees.push(Some(c.tree));
    }
    let parent_opts: Vec<Option<ObjectId>> = parent_trees;
    combined_diff_paths_trees(odb, commit_tree, &parent_opts, walk, find_object)
}

/// Combined diff paths when `parents` are already tree OIDs (plumbing `git diff --cc` with N+1 trees).
pub fn combined_diff_paths_trees(
    odb: &Odb,
    merge_tree: &ObjectId,
    parent_trees: &[Option<ObjectId>],
    walk: &CombinedTreeDiffOptions,
    find_object: Option<&ObjectId>,
) -> Result<Vec<CombinedDiffPath>> {
    if parent_trees.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    ll_diff_tree_paths(
        odb,
        &mut out,
        "",
        walk,
        Some(merge_tree),
        parent_trees,
        find_object,
    )?;
    Ok(out)
}
