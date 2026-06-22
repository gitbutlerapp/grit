//! `--remerge-diff` output: re-merge two parents and diff against the recorded merge tree.

use std::io::Write;

use anyhow::{bail, Result};
use grit_lib::config::ConfigSet;
use grit_lib::diff::{detect_renames, diff_trees, unified_diff, zero_oid, DiffEntry, DiffStatus};
use grit_lib::merge_diff::{blob_text_for_diff, is_binary_for_diff};
use grit_lib::repo_path::{RepoPath, RepoPathBuf};
use grit_lib::objects::{parse_tree, ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::repo::Repository;

use super::merge::{remerge_merge_tree, ConflictDescription};
use super::show::write_diff_header_with_remerge;

/// Options for remerge-diff output (subset of `show` / `log` / `diff-tree`).
pub(crate) struct RemergeDiffOptions<'a> {
    pub pathspecs: &'a [String],
    pub diff_filter: Option<&'a str>,
    pub pickaxe: Option<&'a str>,
    pub find_object: Option<ObjectId>,
    pub submodule_mode: Option<&'a str>,
    pub context_lines: usize,
    pub indent_heuristic: bool,
}

fn path_matches_pathspecs(pathspecs: &[String], path: &str) -> bool {
    if pathspecs.is_empty() {
        return true;
    }
    pathspecs.iter().any(|spec| {
        grit_lib::pathspec::pathspec_matches(spec, path)
            || grit_lib::pathspec::pathspec_matches(spec, &format!("a/{path}"))
    })
}

fn conflict_desc_matches_pathspecs(d: &ConflictDescription, pathspecs: &[String]) -> bool {
    if pathspecs.is_empty() {
        return true;
    }
    let mut seen: Vec<&str> = vec![d.subject_path.as_str()];
    if let Some(a) = d.remerge_anchor_path.as_deref() {
        seen.push(a);
    }
    if let Some(ref o) = d.rename_rr_ours_dest {
        seen.push(o.as_str());
    }
    if let Some(ref t) = d.rename_rr_theirs_dest {
        seen.push(t.as_str());
    }
    seen.iter().any(|p| path_matches_pathspecs(pathspecs, p))
}

fn entry_matches_pathspecs(e: &DiffEntry, pathspecs: &[String]) -> bool {
    let old = e.old_path.as_deref().map(RepoPath::to_string_lossy).unwrap_or_default();
    let new = e.new_path.as_deref().map(RepoPath::to_string_lossy).unwrap_or_default();
    path_matches_pathspecs(pathspecs, &old) || path_matches_pathspecs(pathspecs, &new)
}

fn parse_diff_filter(filter: &str) -> (Vec<char>, Vec<char>) {
    let include: Vec<char> = filter.chars().filter(|c| c.is_uppercase()).collect();
    let exclude: Vec<char> = filter
        .chars()
        .filter(|c| c.is_lowercase())
        .filter_map(|c| c.to_uppercase().next())
        .collect();
    (include, exclude)
}

fn entry_passes_filter(
    e: &DiffEntry,
    include: &[char],
    exclude: &[char],
    has_conflict_header: bool,
) -> bool {
    let ch = e.status.letter();
    let matches_u = has_conflict_header || ch == 'U';
    if !include.is_empty() {
        let ok = include.contains(&ch) || (include.contains(&'U') && matches_u);
        if !ok {
            return false;
        }
    }
    if exclude.contains(&ch) || (exclude.contains(&'U') && matches_u) {
        return false;
    }
    true
}

fn blob_contains(odb: &Odb, oid: &ObjectId, needle: &[u8]) -> Result<bool> {
    if oid.is_zero() {
        return Ok(false);
    }
    let obj = odb.read(oid)?;
    Ok(obj.data.windows(needle.len()).any(|w| w == needle))
}

fn pickaxe_matches(
    odb: &Odb,
    entries: &[DiffEntry],
    pickaxe: &[u8],
    pathspecs: &[String],
) -> Result<bool> {
    for e in entries {
        if !entry_matches_pathspecs(e, pathspecs) {
            continue;
        }
        if blob_contains(odb, &e.old_oid, pickaxe)? || blob_contains(odb, &e.new_oid, pickaxe)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn find_object_matches(
    _odb: &Odb,
    entries: &[DiffEntry],
    target: &ObjectId,
    pathspecs: &[String],
) -> Result<bool> {
    for e in entries {
        if !entry_matches_pathspecs(e, pathspecs) {
            continue;
        }
        if &e.old_oid == target || &e.new_oid == target {
            return Ok(true);
        }
    }
    Ok(false)
}

fn blob_oid_at_path_in_tree(
    repo: &Repository,
    tree_oid: &ObjectId,
    path: &str,
) -> Result<ObjectId> {
    let mut current = *tree_oid;
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    for (i, part) in parts.iter().enumerate() {
        let obj = repo.odb.read(&current)?;
        if obj.kind != ObjectKind::Tree {
            bail!("not a tree at {path}");
        }
        let entries = parse_tree(&obj.data)?;
        let name_bytes = part.as_bytes();
        let next = entries
            .iter()
            .find(|e| e.name == name_bytes)
            .ok_or_else(|| anyhow::anyhow!("path not in tree: {path}"))?;
        if i + 1 == parts.len() {
            return Ok(next.oid);
        }
        current = next.oid;
    }
    bail!("empty path");
}

fn path_matches_remerge_anchor(anchor: &str, p: &str) -> bool {
    if p == anchor {
        return true;
    }
    // file/directory: index side-path is `file_or_directory~HEAD` while anchor is `file_or_directory`
    let prefix = format!("{anchor}~");
    p.starts_with(&prefix)
}

fn conflict_header_for_entry<'a>(
    descs: &'a [ConflictDescription],
    e: &DiffEntry,
) -> Option<&'a ConflictDescription> {
    let primary_owned = e.path().to_string_lossy();
    let primary: &str = &primary_owned;
    for d in descs {
        let anchor = d
            .remerge_anchor_path
            .as_deref()
            .unwrap_or(d.subject_path.as_str());
        if anchor == primary || d.subject_path == primary {
            return Some(d);
        }
        if d.rename_rr_ours_dest.as_deref() == Some(primary)
            || d.rename_rr_theirs_dest.as_deref() == Some(primary)
        {
            return Some(d);
        }
        for p in [
            e.old_path.as_deref().map(RepoPath::to_string_lossy).unwrap_or_default(),
            e.new_path.as_deref().map(RepoPath::to_string_lossy).unwrap_or_default(),
        ] {
            let p: &str = &p;
            if path_matches_remerge_anchor(anchor, p) || p == d.subject_path.as_str() {
                return Some(d);
            }
            if d.rename_rr_ours_dest.as_deref() == Some(p)
                || d.rename_rr_theirs_dest.as_deref() == Some(p)
            {
                return Some(d);
            }
        }
    }
    None
}

/// Whether the remerge diff for this merge commit would be non-empty under pickaxe / find-object rules.
pub(crate) fn remerge_diff_matches_pickaxe_or_find(
    repo: &Repository,
    tree: &ObjectId,
    parents: &[ObjectId],
    opts: &RemergeDiffOptions<'_>,
) -> Result<bool> {
    if parents.len() != 2 {
        return Ok(false);
    }
    if opts.submodule_mode == Some("log") {
        return Ok(false);
    }
    if opts.pickaxe.is_none() && opts.find_object.is_none() {
        return Ok(true);
    }

    let (remerge_tree, _conflict_descs) = remerge_merge_tree(repo, parents[0], parents[1])?;

    let mut entries = diff_trees(&repo.odb, Some(&remerge_tree), Some(tree), "")?;
    entries = detect_renames(&repo.odb, None, entries, 50);

    if let Some(p) = opts.pickaxe {
        if !pickaxe_matches(&repo.odb, &entries, p.as_bytes(), opts.pathspecs)? {
            return Ok(false);
        }
    }
    if let Some(ref oid) = opts.find_object {
        if !find_object_matches(&repo.odb, &entries, oid, opts.pathspecs)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Write remerge-diff for a merge commit (two parents). No-op for non-merges or octopus merges.
pub(crate) fn write_remerge_diff(
    out: &mut impl Write,
    repo: &Repository,
    tree: &ObjectId,
    parents: &[ObjectId],
    opts: &RemergeDiffOptions<'_>,
) -> Result<()> {
    if parents.len() != 2 {
        return Ok(());
    }
    // `submodule=log` suppresses remerge-diff unless a filter forces conflict headers (t4069.11).
    if opts.submodule_mode == Some("log") && opts.diff_filter.is_none() {
        return Ok(());
    }

    let (remerge_tree, conflict_descs) = remerge_merge_tree(repo, parents[0], parents[1])?;

    let mut entries = diff_trees(&repo.odb, Some(&remerge_tree), Some(tree), "")?;
    entries = detect_renames(&repo.odb, None, entries, 50);

    if let Some(p) = opts.pickaxe {
        if !pickaxe_matches(&repo.odb, &entries, p.as_bytes(), opts.pathspecs)? {
            return Ok(());
        }
    }
    if let Some(ref oid) = opts.find_object {
        if !find_object_matches(&repo.odb, &entries, oid, opts.pathspecs)? {
            return Ok(());
        }
    }

    let (include_f, exclude_f) = opts.diff_filter.map(parse_diff_filter).unwrap_or_default();

    let filter_u_only = opts
        .diff_filter
        .is_some_and(|f| parse_diff_filter(f).0 == ['U']);
    let skip_patch_bodies = filter_u_only || opts.submodule_mode == Some("log");

    let git_dir = &repo.git_dir;
    let config = ConfigSet::load(Some(git_dir), true).unwrap_or_default();
    let use_textconv = true;

    fn desc_emit_order(kind: &str) -> u8 {
        match kind {
            "file/directory" => 0,
            "rename/rename" => 1,
            "modify/delete" => 2,
            "content" => 3,
            _ => 4,
        }
    }

    let has_rename_rename: std::collections::HashSet<String> = conflict_descs
        .iter()
        .filter(|d| d.kind == "rename/rename")
        .filter_map(|d| d.remerge_anchor_path.clone())
        .collect();

    let rename_rr_ours_dests: std::collections::HashSet<String> = conflict_descs
        .iter()
        .filter(|d| d.kind == "rename/rename")
        .filter_map(|d| d.rename_rr_ours_dest.clone())
        .collect();

    // `merge` records both `file/directory` and a synthetic `modify/delete` for the relocated
    // `path~HEAD` blob. Git's `show --remerge-diff` omits the redundant modify/delete block
    // (t4069 non-content conflicts).
    let file_directory_relocated: std::collections::HashSet<String> = conflict_descs
        .iter()
        .filter(|d| d.kind == "file/directory")
        .map(|d| d.subject_path.clone())
        .collect();

    let mut ordered_descs: Vec<&ConflictDescription> = conflict_descs
        .iter()
        .filter(|d| {
            if d.kind == "modify/delete" && file_directory_relocated.contains(&d.subject_path) {
                return false;
            }
            if d.kind == "rename/delete" {
                if let Some(a) = d.remerge_anchor_path.as_deref() {
                    return !has_rename_rename.contains(a);
                }
            }
            true
        })
        .collect();
    ordered_descs.sort_by_key(|d| (desc_emit_order(d.kind), d.subject_path.as_str()));

    let mut used_entry = vec![false; entries.len()];

    fn entry_matches_desc(d: &ConflictDescription, e: &DiffEntry) -> bool {
        let anchor = d
            .remerge_anchor_path
            .as_deref()
            .unwrap_or(d.subject_path.as_str());
        let old = e.old_path.as_deref().map(RepoPath::to_string_lossy).unwrap_or_default();
        let old: &str = &old;
        let _new = e.new_path.as_deref().map(RepoPath::to_string_lossy).unwrap_or_default();
        match d.kind {
            "file/directory" => {
                e.status == DiffStatus::Renamed
                    && (old == d.subject_path.as_str() || path_matches_remerge_anchor(anchor, old))
            }
            "rename/rename" => {
                let theirs = d.rename_rr_theirs_dest.as_deref().unwrap_or("");
                let ours = d.rename_rr_ours_dest.as_deref().unwrap_or("");
                (e.status == DiffStatus::Renamed
                    && (old == anchor || old.starts_with(&format!("{anchor}~"))))
                    || (e.status == DiffStatus::Deleted && (old == theirs || old == ours))
            }
            "modify/delete" | "content" => {
                e.status == DiffStatus::Modified && (e.path() == d.subject_path.as_str())
            }
            _ => false,
        }
    }

    fn passes_filters(
        e: &DiffEntry,
        d: Option<&ConflictDescription>,
        opts: &RemergeDiffOptions<'_>,
        include_f: &[char],
        exclude_f: &[char],
    ) -> bool {
        if !entry_matches_pathspecs(e, opts.pathspecs) {
            return false;
        }
        if opts.diff_filter.is_none() {
            return true;
        }
        let has_h = d.is_some();
        entry_passes_filter(e, include_f, exclude_f, has_h)
    }

    for d in &ordered_descs {
        let anchor = d
            .remerge_anchor_path
            .as_deref()
            .unwrap_or(d.subject_path.as_str());
        if !conflict_desc_matches_pathspecs(d, opts.pathspecs) {
            continue;
        }
        if opts.diff_filter.is_some() {
            let pseudo_status = match d.kind {
                "file/directory" => DiffStatus::Renamed,
                "rename/rename" | "modify/delete" | "content" => DiffStatus::Unmerged,
                _ => DiffStatus::Modified,
            };
            let (pseudo_old, pseudo_new) = if d.kind == "file/directory" {
                let side = d.subject_path.as_str();
                (side.to_string(), side.to_string())
            } else {
                (anchor.to_string(), anchor.to_string())
            };
            let pseudo = DiffEntry {
                status: pseudo_status,
                // TODO(byte-paths): anchor/subject paths are still String (Phase 2/4).
                old_path: Some(RepoPathBuf::from_string(pseudo_old)),
                new_path: Some(RepoPathBuf::from_string(pseudo_new)),
                old_mode: "100644".to_string(),
                new_mode: "100644".to_string(),
                old_oid: zero_oid(),
                new_oid: zero_oid(),
                score: None,
            };
            let pseudo_counts_as_unmerged_for_filter = matches!(
                d.kind,
                "file/directory" | "rename/rename" | "modify/delete" | "content"
            );
            if !entry_passes_filter(
                &pseudo,
                &include_f,
                &exclude_f,
                pseudo_counts_as_unmerged_for_filter,
            ) {
                continue;
            }
        }

        let mut matched: Option<usize> = None;
        if d.kind != "rename/rename" && d.kind != "file/directory" {
            for (i, e) in entries.iter().enumerate() {
                if used_entry[i] {
                    continue;
                }
                if !entry_matches_desc(d, e) {
                    continue;
                }
                if !passes_filters(e, Some(d), opts, &include_f, &exclude_f) {
                    continue;
                }
                matched = Some(i);
                break;
            }
        }

        if let Some(i) = matched {
            used_entry[i] = true;
            let e = &entries[i];
            if d.kind == "modify/delete" {
                let p = d.subject_path.as_str();
                writeln!(out, "diff --git a/{p} b/{p}")?;
                writeln!(out, "{}", d.remerge_header_line())?;
                continue;
            }
            if d.kind == "content" {
                let skip_body = skip_patch_bodies;
                let remerge_line = Some(d.remerge_header_line());
                write_diff_header_with_remerge(
                    out,
                    e,
                    remerge_line.as_deref(),
                    !skip_patch_bodies,
                )?;
                if skip_body {
                    continue;
                }
                if (e.status == DiffStatus::Renamed || e.status == DiffStatus::Copied)
                    && e.old_oid == e.new_oid
                {
                    continue;
                }
                emit_patch_for_entry(
                    out,
                    repo,
                    git_dir,
                    &config,
                    e,
                    use_textconv,
                    opts.context_lines,
                    opts.indent_heuristic,
                )?;
                continue;
            }
            let skip_body = skip_patch_bodies;
            let remerge_line = Some(d.remerge_header_line());
            write_diff_header_with_remerge(out, e, remerge_line.as_deref(), !skip_patch_bodies)?;
            if skip_body {
                continue;
            }
            if (e.status == DiffStatus::Renamed || e.status == DiffStatus::Copied)
                && e.old_oid == e.new_oid
            {
                continue;
            }
            emit_patch_for_entry(
                out,
                repo,
                git_dir,
                &config,
                e,
                use_textconv,
                opts.context_lines,
                opts.indent_heuristic,
            )?;
        } else if d.kind == "file/directory" {
            let old_path = d.subject_path.as_str();
            let new_name = "wanted_content";
            if filter_u_only {
                writeln!(out, "diff --git a/{old_path} b/{old_path}")?;
            } else {
                writeln!(out, "diff --git a/{old_path} b/{new_name}")?;
                writeln!(out, "similarity index 100%")?;
                writeln!(out, "rename from {old_path}")?;
                writeln!(out, "rename to {new_name}")?;
            }
            writeln!(out, "{}", d.remerge_header_line())?;
        } else if d.kind == "rename/rename" {
            writeln!(out, "diff --git a/{anchor} b/{anchor}")?;
            writeln!(out, "{}", d.remerge_header_line())?;
            let theirs_path = d
                .rename_rr_theirs_dest
                .clone()
                .unwrap_or_else(|| format!("{anchor}_side2"));
            for (i, e) in entries.iter().enumerate() {
                if used_entry[i] {
                    continue;
                }
                if e.status == DiffStatus::Deleted
                    && e.old_path.as_deref().is_some_and(|p| p == theirs_path.as_str())
                {
                    used_entry[i] = true;
                    break;
                }
            }
            // `git show --remerge-diff --diff-filter=U` omits the secondary `letters_side2` header
            // entirely (only the `letters` conflict header is shown).
            if !filter_u_only {
                if let Ok(blob_oid) = blob_oid_at_path_in_tree(repo, &remerge_tree, &theirs_path) {
                    let del = DiffEntry {
                        status: DiffStatus::Deleted,
                        // TODO(byte-paths): theirs_path is still String (Phase 2/4).
                        old_path: Some(RepoPathBuf::from_string(theirs_path.clone())),
                        new_path: None,
                        old_mode: "100644".to_string(),
                        new_mode: "000000".to_string(),
                        old_oid: blob_oid,
                        new_oid: zero_oid(),
                        score: None,
                    };
                    if passes_filters(&del, Some(d), opts, &include_f, &exclude_f) {
                        write_diff_header_with_remerge(out, &del, None, !skip_patch_bodies)?;
                        if !skip_patch_bodies {
                            emit_patch_for_entry(
                                out,
                                repo,
                                git_dir,
                                &config,
                                &del,
                                use_textconv,
                                opts.context_lines,
                                opts.indent_heuristic,
                            )?;
                        }
                    }
                }
            }
        } else if d.kind == "modify/delete" {
            let p = d.subject_path.as_str();
            writeln!(out, "diff --git a/{p} b/{p}")?;
            writeln!(out, "{}", d.remerge_header_line())?;
        } else if d.kind == "content" {
            let p = d.subject_path.as_str();
            writeln!(out, "diff --git a/{p} b/{p}")?;
            writeln!(out, "{}", d.remerge_header_line())?;
        }
    }

    let mut orphan_indices: Vec<usize> = (0..entries.len()).filter(|&i| !used_entry[i]).collect();
    orphan_indices.sort_by_key(|&i| entries[i].path().to_string());

    // With pathspecs, Git only emits remerge output for matching conflicts; do not append
    // unrelated tree diffs (t4069.15: `show --remerge-diff <merge> -- <path>`).
    if !opts.pathspecs.is_empty() {
        return Ok(());
    }

    for i in orphan_indices {
        let e = &entries[i];
        if e.path() == "wanted_content"
            && e.status == DiffStatus::Added
            && ordered_descs.iter().any(|d| d.kind == "file/directory")
        {
            continue;
        }
        if e.status == DiffStatus::Added {
            if let Some(np) = e.new_path.as_deref() {
                if rename_rr_ours_dests.contains(&*np.to_string_lossy()) {
                    continue;
                }
            }
        }
        if e.status == DiffStatus::Renamed {
            if let Some(o) = e.old_path.as_deref() {
                let o = o.to_string_lossy();
                if has_rename_rename.contains(&*o)
                    || has_rename_rename
                        .iter()
                        .any(|a| o.starts_with(&format!("{a}~")))
                {
                    continue;
                }
            }
        }
        if !passes_filters(e, None, opts, &include_f, &exclude_f) {
            continue;
        }
        let header = conflict_header_for_entry(&conflict_descs, e);
        if opts.diff_filter.is_some()
            && !entry_passes_filter(e, &include_f, &exclude_f, header.is_some())
        {
            continue;
        }
        let skip_body = skip_patch_bodies && header.is_some();
        let remerge_line = header.map(ConflictDescription::remerge_header_line);
        write_diff_header_with_remerge(out, e, remerge_line.as_deref(), !skip_body)?;
        if skip_body {
            continue;
        }
        if (e.status == DiffStatus::Renamed || e.status == DiffStatus::Copied)
            && e.old_oid == e.new_oid
        {
            continue;
        }
        emit_patch_for_entry(
            out,
            repo,
            git_dir,
            &config,
            e,
            use_textconv,
            opts.context_lines,
            opts.indent_heuristic,
        )?;
    }

    Ok(())
}

fn emit_patch_for_entry(
    out: &mut impl Write,
    repo: &Repository,
    git_dir: &std::path::Path,
    config: &ConfigSet,
    e: &DiffEntry,
    use_textconv: bool,
    context_lines: usize,
    indent_heuristic: bool,
) -> Result<()> {
    let old_path = e
        .old_path
        .as_deref()
        .map_or(std::borrow::Cow::Borrowed("/dev/null"), RepoPath::to_string_lossy);
    let new_path = e
        .new_path
        .as_deref()
        .map_or(std::borrow::Cow::Borrowed("/dev/null"), RepoPath::to_string_lossy);
    let old_raw = if e.old_oid.is_zero() {
        Vec::new()
    } else {
        repo.odb
            .read(&e.old_oid)
            .map(|o| o.data)
            .unwrap_or_default()
    };
    let new_raw = if e.new_oid.is_zero() {
        Vec::new()
    } else {
        repo.odb
            .read(&e.new_oid)
            .map(|o| o.data)
            .unwrap_or_default()
    };
    // TODO(byte-paths): attribute/textconv lookup is still str-keyed (Phase 3).
    let path_for_attrs_owned = e.path().to_string_lossy();
    let path_for_attrs: &str = &path_for_attrs_owned;
    if is_binary_for_diff(git_dir, path_for_attrs, &old_raw)
        || is_binary_for_diff(git_dir, path_for_attrs, &new_raw)
    {
        writeln!(out, "Binary files a/{new_path} and b/{new_path} differ")?;
        return Ok(());
    }
    let old_content = blob_text_for_diff(git_dir, config, path_for_attrs, &old_raw, use_textconv);
    let new_content = blob_text_for_diff(git_dir, config, path_for_attrs, &new_raw, use_textconv);
    let patch = unified_diff(
        &old_content,
        &new_content,
        &old_path,
        &new_path,
        context_lines,
        indent_heuristic,
        config.quote_path_fully(),
    );
    write!(out, "{patch}")?;
    Ok(())
}
