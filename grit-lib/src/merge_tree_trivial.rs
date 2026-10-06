//! Trivial three-tree merge for `git merge-tree` (no `--write-tree`).
//!
//! Mirrors upstream `builtin/merge-tree.c` (`threeway_callback`, `unresolved`,
//! `unresolved_directory`, `resolve`, `show_result`, `show_diff`).

use crate::attributes::{
    collect_attrs_for_path, load_gitattributes_for_diff, AttrValue, MacroTable,
};
use crate::config::ConfigSet;
use crate::diff::unified_diff;
use crate::error::{Error, Result};
use crate::merge_file::{merge, ConflictStyle, MergeFavor, MergeInput};
use crate::objects::{parse_tree, tree_entry_cmp, ObjectId, ObjectKind, TreeEntry};
use crate::odb::Odb;
use crate::repo::Repository;
use std::cmp::Ordering;

#[derive(Debug)]
struct MergeList {
    stage: u8,
    mode: u32,
    oid: ObjectId,
    path: String,
    link: Option<Box<MergeList>>,
}

fn is_tree_mode(mode: u32) -> bool {
    mode == 0o040000
}

fn read_tree_entries(odb: &Odb, tree_oid: &ObjectId) -> Result<Vec<TreeEntry>> {
    let obj = odb.read(tree_oid)?;
    if obj.kind != ObjectKind::Tree {
        return Err(Error::CorruptObject(format!(
            "expected tree object, got {}",
            obj.kind.as_str()
        )));
    }
    parse_tree(&obj.data)
}

fn format_mode_octal(mode: u32) -> String {
    format!("{:06o}", mode)
}

fn is_null_oid(oid: &ObjectId) -> bool {
    oid.is_zero()
}

fn same_entry(a: &TreeEntry, b: &TreeEntry) -> bool {
    !is_null_oid(&a.oid) && !is_null_oid(&b.oid) && a.oid == b.oid && a.mode == b.mode
}

fn both_empty(a: &TreeEntry, b: &TreeEntry) -> bool {
    is_null_oid(&a.oid) && is_null_oid(&b.oid)
}

fn format_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

fn explanation(head: &MergeList) -> &'static str {
    match head.stage {
        0 => "merged",
        3 => "added in remote",
        2 => {
            if head.link.is_some() {
                "added in both"
            } else {
                "added in local"
            }
        }
        1 => {
            let Some(e2) = head.link.as_deref() else {
                return "removed in both";
            };
            if e2.link.is_some() {
                return "changed in both";
            }
            if e2.stage == 3 {
                "removed in local"
            } else {
                "removed in remote"
            }
        }
        _ => "changed in both",
    }
}

fn merge_favor_for_path(
    repo: &Repository,
    macros: &MacroTable,
    rules: &[crate::attributes::AttrRule],
    rel_path: &str,
) -> MergeFavor {
    let Ok(config) = ConfigSet::load(Some(&repo.git_dir), true) else {
        return MergeFavor::None;
    };
    let ignore_case = config
        .get("core.ignorecase")
        .is_some_and(|v| v == "true" || v == "1" || v == "yes");
    let map = collect_attrs_for_path(rules, macros, rel_path, ignore_case);
    match map.get("merge") {
        Some(AttrValue::Value(v)) if v == "union" => MergeFavor::Union,
        _ => MergeFavor::None,
    }
}

fn merge_three_blobs(
    path: &str,
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    favor: MergeFavor,
) -> Result<Vec<u8>> {
    let out = merge(&MergeInput {
        base,
        ours,
        theirs,
        label_ours: ".our",
        label_base: "",
        label_theirs: ".their",
        favor,
        style: ConflictStyle::Merge,
        marker_size: 7,
        diff_algorithm: None,
        ignore_all_space: false,
        ignore_space_change: false,
        ignore_space_at_eol: false,
        ignore_cr_at_eol: false,
    })
    .map_err(|e| Error::Message(format!("merge blobs at {path}: {e}")))?;
    Ok(out.content)
}

fn read_blob_bytes(odb: &Odb, oid: &ObjectId) -> Result<Vec<u8>> {
    if oid.is_zero() {
        return Ok(Vec::new());
    }
    let obj = odb.read(oid)?;
    if obj.kind != ObjectKind::Blob {
        return Err(Error::CorruptObject(format!(
            "expected blob object, got {}",
            obj.kind.as_str()
        )));
    }
    Ok(obj.data)
}

fn origin_bytes(odb: &Odb, entry: &MergeList) -> Result<Vec<u8>> {
    let mut cur = Some(entry);
    while let Some(e) = cur {
        if e.stage == 2 {
            return read_blob_bytes(odb, &e.oid);
        }
        cur = e.link.as_deref();
    }
    Ok(Vec::new())
}

fn result_bytes(
    repo: &Repository,
    macros: &MacroTable,
    rules: &[crate::attributes::AttrRule],
    odb: &Odb,
    entry: &MergeList,
) -> Result<Vec<u8>> {
    if entry.stage == 0 {
        return read_blob_bytes(odb, &entry.oid);
    }
    let mut base: Option<Vec<u8>> = None;
    let mut ours: Option<Vec<u8>> = None;
    let mut theirs: Option<Vec<u8>> = None;
    let mut cur = Some(entry);
    while let Some(e) = cur {
        match e.stage {
            1 => base = Some(read_blob_bytes(odb, &e.oid)?),
            2 => ours = Some(read_blob_bytes(odb, &e.oid)?),
            3 => theirs = Some(read_blob_bytes(odb, &e.oid)?),
            _ => {}
        }
        cur = e.link.as_deref();
    }
    let has_base = base.is_some();
    let has_ours = ours.is_some();
    let has_theirs = theirs.is_some();
    // Matches Git `merge_blobs` for incomplete stages (see `builtin/merge-tree.c:result`).
    if has_base && has_ours && !has_theirs {
        return Ok(Vec::new());
    }
    if has_base && has_theirs && !has_ours {
        return Ok(Vec::new());
    }
    let favor = merge_favor_for_path(repo, macros, rules, &entry.path);
    merge_three_blobs(
        &entry.path,
        base.as_deref().unwrap_or(&[]),
        ours.as_deref().unwrap_or(&[]),
        theirs.as_deref().unwrap_or(&[]),
        favor,
    )
}

fn show_diff(
    repo: &Repository,
    macros: &MacroTable,
    rules: &[crate::attributes::AttrRule],
    odb: &Odb,
    entry: &MergeList,
) -> Result<String> {
    let src = origin_bytes(odb, entry)?;
    let dst = result_bytes(repo, macros, rules, odb, entry)?;
    let old_s = String::from_utf8_lossy(&src);
    let new_s = String::from_utf8_lossy(&dst);
    let cfg = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let patch = unified_diff(
        old_s.as_ref(),
        new_s.as_ref(),
        &entry.path,
        &entry.path,
        3,
        true,
        cfg.quote_path_fully(),
    );
    Ok(trim_diff_header(&patch))
}

fn trim_diff_header(patch: &str) -> String {
    let mut out = String::new();
    let mut in_hunk = false;
    let mut skip_next_line: Option<String> = None;
    for line in patch.lines() {
        if let Some(ref expect) = skip_next_line {
            if line == expect {
                skip_next_line = None;
                continue;
            }
            skip_next_line = None;
        }
        if line.starts_with("@@") {
            in_hunk = true;
        }
        if in_hunk {
            if !out.is_empty() {
                out.push('\n');
            }
            if line.starts_with("@@") {
                if let Some((header, tail)) = split_unified_hunk_header(line) {
                    out.push_str(&header);
                    if let Some(rest) = tail {
                        if !rest.is_empty() {
                            out.push('\n');
                            out.push_str(rest);
                            skip_next_line = Some(rest.to_string());
                        }
                    }
                } else {
                    out.push_str(line);
                }
            } else {
                out.push_str(line);
            }
        }
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// Git's `merge-tree` hunk headers end with `@@` on its own line when the diff library puts the
/// first context fragment on the same line as the header (`@@ -1,3 +1,3 @@ context`).
fn split_unified_hunk_header(line: &str) -> Option<(String, Option<&str>)> {
    if !line.starts_with("@@") {
        return None;
    }
    let search = 2usize;
    let rel = &line[search..];
    let i = rel.find("@@")?;
    let second = search + i;
    let after_second = second + 2;
    if after_second >= line.len() {
        return Some((line.to_string(), None));
    }
    let header = line[..after_second].to_string();
    let tail = &line[after_second..];
    Some((header, Some(tail)))
}

fn append_show_result(
    out: &mut String,
    repo: &Repository,
    macros: &MacroTable,
    rules: &[crate::attributes::AttrRule],
    odb: &Odb,
    entry: &MergeList,
) -> Result<()> {
    out.push_str(explanation(entry));
    out.push('\n');
    let mut cur = Some(entry);
    while let Some(e) = cur {
        let desc = match e.stage {
            0 => "result",
            1 => "base",
            2 => "our",
            3 => "their",
            _ => "our",
        };
        out.push_str(&format!(
            "  {:<6} {} {} {}\n",
            desc,
            format_mode_octal(e.mode),
            e.oid.to_hex(),
            e.path
        ));
        cur = e.link.as_deref();
    }
    out.push_str(&show_diff(repo, macros, rules, odb, entry)?);
    Ok(())
}

fn link_entry(
    stage: u8,
    path: String,
    mode: u32,
    oid: ObjectId,
    tail: Option<Box<MergeList>>,
) -> Box<MergeList> {
    Box::new(MergeList {
        stage,
        mode,
        oid,
        path,
        link: tail,
    })
}
#[expect(clippy::too_many_arguments)]
fn resolve(
    records: &mut Vec<(String, String)>,
    repo: &Repository,
    macros: &MacroTable,
    rules: &[crate::attributes::AttrRule],
    odb: &Odb,
    ours: Option<&TreeEntry>,
    result: &TreeEntry,
    path: String,
) -> Result<()> {
    let Some(ours) = ours else {
        return Ok(());
    };
    let sort_path = path.clone();
    let head = link_entry(
        0,
        path.clone(),
        result.mode,
        result.oid,
        Some(link_entry(2, path, ours.mode, ours.oid, None)),
    );
    let mut block = String::new();
    append_show_result(&mut block, repo, macros, rules, odb, &head)?;
    records.push((sort_path, block));
    Ok(())
}

fn unresolved_list_only(
    records: &mut Vec<(String, String)>,
    repo: &Repository,
    macros: &MacroTable,
    rules: &[crate::attributes::AttrRule],
    odb: &Odb,
    n: &[TreeEntry; 3],
    path: String,
) -> Result<()> {
    let mut dirmask = 0u8;
    for (i, entry) in n.iter().enumerate() {
        if entry.mode == 0 || is_tree_mode(entry.mode) {
            dirmask |= 1 << i;
        }
    }
    if dirmask == 0b111 {
        return Ok(());
    }

    let mut entry: Option<Box<MergeList>> = None;
    if n[2].mode != 0 && !is_tree_mode(n[2].mode) {
        entry = Some(link_entry(3, path.clone(), n[2].mode, n[2].oid, entry));
    }
    if n[1].mode != 0 && !is_tree_mode(n[1].mode) {
        entry = Some(link_entry(2, path.clone(), n[1].mode, n[1].oid, entry));
    }
    if n[0].mode != 0 && !is_tree_mode(n[0].mode) {
        entry = Some(link_entry(1, path.clone(), n[0].mode, n[0].oid, entry));
    }
    if let Some(e) = entry {
        let mut block = String::new();
        append_show_result(&mut block, repo, macros, rules, odb, &e)?;
        records.push((path, block));
    }
    Ok(())
}

fn write_empty_tree(odb: &Odb) -> Result<ObjectId> {
    odb.write(ObjectKind::Tree, &[])
}

/// Tree OID to descend into for `unresolved_directory` (`NULL` in Git → empty tree).
fn tree_oid_for_side(empty_tree: &ObjectId, e: &TreeEntry) -> ObjectId {
    if e.mode != 0 && is_tree_mode(e.mode) {
        e.oid
    } else {
        *empty_tree
    }
}

fn path_depth(rel_path: &str) -> usize {
    rel_path.bytes().filter(|b| *b == b'/').count()
}

fn merge_record_sort_key(path: &str) -> (usize, &str) {
    (usize::MAX.saturating_sub(path_depth(path)), path)
}

fn cmp_merge_records(a: &(String, String), b: &(String, String)) -> Ordering {
    let ka = merge_record_sort_key(&a.0);
    let kb = merge_record_sort_key(&b.0);
    ka.0.cmp(&kb.0).then_with(|| ka.1.cmp(kb.1))
}
#[expect(clippy::too_many_arguments)]
fn merge_trees_at(
    records: &mut Vec<(String, String)>,
    repo: &Repository,
    macros: &MacroTable,
    rules: &[crate::attributes::AttrRule],
    odb: &Odb,
    empty_tree: &ObjectId,
    base_oid: &ObjectId,
    ours_oid: &ObjectId,
    theirs_oid: &ObjectId,
    prefix: &str,
) -> Result<()> {
    let base_e = read_tree_entries(odb, base_oid)?;
    let ours_e = read_tree_entries(odb, ours_oid)?;
    let theirs_e = read_tree_entries(odb, theirs_oid)?;

    let mut i0 = 0usize;
    let mut i1 = 0usize;
    let mut i2 = 0usize;

    while i0 < base_e.len() || i1 < ours_e.len() || i2 < theirs_e.len() {
        let e0 = base_e.get(i0);
        let e1 = ours_e.get(i1);
        let e2 = theirs_e.get(i2);

        let k0 = e0.map(|e| (&e.name[..], is_tree_mode(e.mode)));
        let k1 = e1.map(|e| (&e.name[..], is_tree_mode(e.mode)));
        let k2 = e2.map(|e| (&e.name[..], is_tree_mode(e.mode)));

        let mut keys: Vec<(&[u8], bool)> = Vec::new();
        for key in [k0, k1, k2].into_iter().flatten() {
            if !keys.iter().any(|x| x.0 == key.0 && x.1 == key.1) {
                keys.push(key);
            }
        }
        if keys.is_empty() {
            break;
        }
        let mut min_k = keys[0];
        for k in keys.iter().skip(1) {
            if tree_entry_cmp(k.0, k.1, min_k.0, min_k.1).is_lt() {
                min_k = *k;
            }
        }

        let n0 = if k0 == Some(min_k) {
            i0 += 1;
            e0
        } else {
            None
        };
        let n1 = if k1 == Some(min_k) {
            i1 += 1;
            e1
        } else {
            None
        };
        let n2 = if k2 == Some(min_k) {
            i2 += 1;
            e2
        } else {
            None
        };

        let name_str = String::from_utf8_lossy(min_k.0).into_owned();
        let path = format_path(prefix, &name_str);

        let b = n0.cloned().unwrap_or_else(|| TreeEntry {
            mode: 0,
            name: min_k.0.to_vec(),
            oid: ObjectId::zero(),
        });
        let o = n1.cloned().unwrap_or_else(|| TreeEntry {
            mode: 0,
            name: min_k.0.to_vec(),
            oid: ObjectId::zero(),
        });
        let t = n2.cloned().unwrap_or_else(|| TreeEntry {
            mode: 0,
            name: min_k.0.to_vec(),
            oid: ObjectId::zero(),
        });
        let triple = [b, o, t];

        let any_tree = triple.iter().any(|e| e.mode != 0 && is_tree_mode(e.mode));
        if any_tree {
            let tb = tree_oid_for_side(empty_tree, &triple[0]);
            let to = tree_oid_for_side(empty_tree, &triple[1]);
            let tt = tree_oid_for_side(empty_tree, &triple[2]);
            merge_trees_at(
                records, repo, macros, rules, odb, empty_tree, &tb, &to, &tt, &path,
            )?;
            unresolved_list_only(records, repo, macros, rules, odb, &triple, path)?;
            continue;
        }

        if same_entry(&triple[1], &triple[2]) || both_empty(&triple[1], &triple[2]) {
            resolve(records, repo, macros, rules, odb, None, &triple[1], path)?;
            continue;
        }

        if same_entry(&triple[0], &triple[1])
            && !is_null_oid(&triple[2].oid)
            && !is_tree_mode(triple[2].mode)
        {
            resolve(
                records,
                repo,
                macros,
                rules,
                odb,
                Some(&triple[1]),
                &triple[2],
                path,
            )?;
            continue;
        }

        if same_entry(&triple[0], &triple[2]) || both_empty(&triple[0], &triple[2]) {
            resolve(records, repo, macros, rules, odb, None, &triple[1], path)?;
            continue;
        }

        unresolved_list_only(records, repo, macros, rules, odb, &triple, path)?;
    }

    Ok(())
}

/// Trivial three-way tree merge; output matches `git merge-tree <base> <ours> <theirs>`.
pub fn trivial_merge_trees_stdout(
    repo: &Repository,
    base: ObjectId,
    ours: ObjectId,
    theirs: ObjectId,
) -> Result<String> {
    let parsed = load_gitattributes_for_diff(repo)?;
    let mut records: Vec<(String, String)> = Vec::new();
    let empty_tree = write_empty_tree(&repo.odb)?;
    merge_trees_at(
        &mut records,
        repo,
        &parsed.macros,
        &parsed.rules,
        &repo.odb,
        &empty_tree,
        &base,
        &ours,
        &theirs,
        "",
    )?;
    records.sort_by(cmp_merge_records);
    let mut out = String::new();
    for (_, block) in records {
        out.push_str(&block);
    }
    Ok(out)
}
