//! `git notes` tree manipulation — the fanout tree mapping `object -> note blob`.
//!
//! Notes are stored as blobs in a tree referenced by `refs/notes/commits` (or a
//! custom namespace via `--ref`). Each leaf in the notes tree is named by the
//! full hex SHA of the annotated object, optionally split into a fanout layout
//! (`ab/cd/ef…`) once the note count grows large.
//!
//! This module owns the *pure tree operations* over the object database: reading
//! the notes tree into a flat entry list, computing the fanout layout, writing a
//! new notes tree + commit, combining note blobs (concatenate / cat_sort_uniq),
//! and the notes-merge data model (pairing local/remote/base changes and applying
//! a merge strategy). The `grit` binary keeps argument parsing, editor launch,
//! stdin reading, output, and exit-code mapping.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;

use crate::commit::{assemble_identity, now_for_identity};
use crate::config::ConfigSet;
use crate::diff::zero_oid;
use crate::error::{Error, Result};
use crate::merge_base::merge_bases_first_vs_rest;
use crate::objects::{
    parse_commit, parse_tree, serialize_commit, serialize_tree, tree_entry_cmp, CommitData,
    ObjectId, ObjectKind, TreeEntry,
};
use crate::refs::{append_reflog, resolve_ref, should_autocreate_reflog, write_ref};
use crate::repo::Repository;
use crate::rev_parse::resolve_revision;
use merge3::{Merge3, StandardMarkers};

/// Per-worktree subdirectory holding the conflicted note blobs during a notes merge.
pub const NOTES_MERGE_WORKTREE: &str = "NOTES_MERGE_WORKTREE";

#[derive(Clone)]
pub struct NotesTreeEntry {
    pub mode: u32,
    pub path: Vec<u8>,
    pub oid: ObjectId,
}

/// One annotated object's note change: `(object_id, old_blob, new_blob)`.
type NoteBlobChange = (ObjectId, Option<ObjectId>, Option<ObjectId>);

enum NotesTreeChild {
    Blob { mode: u32, oid: ObjectId },
    Tree(Vec<NotesTreeEntry>),
}

pub fn note_object_name(path: &[u8]) -> Option<String> {
    let compact: Vec<u8> = path.iter().copied().filter(|byte| *byte != b'/').collect();
    if !ObjectId::is_hex_len(compact.len()) || !compact.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    String::from_utf8(compact)
        .ok()
        .map(|name| name.to_ascii_lowercase())
}

pub fn display_note_path(entry: &NotesTreeEntry) -> Cow<'_, str> {
    if let Some(name) = note_object_name(&entry.path) {
        Cow::Owned(name)
    } else {
        String::from_utf8_lossy(&entry.path)
    }
}

fn collect_notes_tree_entries(
    repo: &Repository,
    tree_oid: &ObjectId,
    prefix: &[u8],
    out: &mut Vec<NotesTreeEntry>,
) -> Result<()> {
    let tree_obj = repo.odb.read(tree_oid)?;
    if tree_obj.kind != ObjectKind::Tree {
        return Err(Error::Message("notes commit has invalid tree".into()));
    }

    for entry in parse_tree(&tree_obj.data)? {
        let mut path = prefix.to_vec();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(&entry.name);

        if entry.mode == 0o040000 {
            collect_notes_tree_entries(repo, &entry.oid, &path, out)?;
        } else {
            out.push(NotesTreeEntry {
                mode: entry.mode,
                path,
                oid: entry.oid,
            });
        }
    }

    Ok(())
}

/// Read the notes tree entries from the notes ref.  Returns an empty vec if
/// the ref doesn't exist yet.
pub fn read_notes_tree(repo: &Repository, notes_ref: &str) -> Result<Vec<NotesTreeEntry>> {
    let tree_oid = match resolve_ref(&repo.git_dir, notes_ref) {
        Ok(oid) => {
            let obj = repo.odb.read(&oid)?;
            match obj.kind {
                ObjectKind::Commit => parse_commit(&obj.data)?.tree,
                ObjectKind::Tree => oid,
                _ => {
                    return Err(Error::Message(format!(
                        "{notes_ref} does not point to a commit or tree"
                    )))
                }
            }
        }
        Err(_) => {
            let oid = match resolve_revision(repo, notes_ref) {
                Ok(o) => o,
                Err(_) => return Ok(Vec::new()),
            };
            let obj = repo.odb.read(&oid)?;
            match obj.kind {
                ObjectKind::Commit => parse_commit(&obj.data)?.tree,
                ObjectKind::Tree => oid,
                _ => {
                    return Err(Error::Message(format!(
                        "{notes_ref} does not point to a commit or tree"
                    )))
                }
            }
        }
    };
    let mut entries = Vec::new();
    collect_notes_tree_entries(repo, &tree_oid, b"", &mut entries)?;
    Ok(entries)
}

fn notes_fanout(entries: &[NotesTreeEntry]) -> usize {
    let mut note_count = entries
        .iter()
        .filter(|entry| note_object_name(&entry.path).is_some())
        .count();
    let mut fanout = 0usize;
    while note_count > 0xff {
        note_count >>= 8;
        fanout += 1;
    }
    fanout
}

fn path_with_fanout(hex: &str, fanout: usize) -> Vec<u8> {
    let mut path = Vec::with_capacity(hex.len() + fanout);
    let bytes = hex.as_bytes();
    let split = fanout.min(bytes.len() / 2);
    for idx in 0..split {
        let start = idx * 2;
        path.extend_from_slice(&bytes[start..start + 2]);
        path.push(b'/');
    }
    path.extend_from_slice(&bytes[split * 2..]);
    path
}

fn write_notes_subtree(repo: &Repository, entries: &[NotesTreeEntry]) -> Result<ObjectId> {
    let mut children: BTreeMap<Vec<u8>, NotesTreeChild> = BTreeMap::new();

    for entry in entries {
        if let Some(slash_pos) = entry.path.iter().position(|byte| *byte == b'/') {
            let child_name = entry.path[..slash_pos].to_vec();
            let child_entry = NotesTreeEntry {
                mode: entry.mode,
                path: entry.path[slash_pos + 1..].to_vec(),
                oid: entry.oid,
            };
            children
                .entry(child_name)
                .or_insert_with(|| NotesTreeChild::Tree(Vec::new()));
            if let Some(NotesTreeChild::Tree(tree_entries)) =
                children.get_mut(&entry.path[..slash_pos])
            {
                tree_entries.push(child_entry);
            }
        } else {
            children.insert(
                entry.path.clone(),
                NotesTreeChild::Blob {
                    mode: entry.mode,
                    oid: entry.oid,
                },
            );
        }
    }

    let mut tree_entries = Vec::with_capacity(children.len());
    for (name, child) in children {
        match child {
            NotesTreeChild::Blob { mode, oid } => tree_entries.push(TreeEntry { mode, name, oid }),
            NotesTreeChild::Tree(child_entries) => {
                let oid = write_notes_subtree(repo, &child_entries)?;
                tree_entries.push(TreeEntry {
                    mode: 0o040000,
                    name,
                    oid,
                });
            }
        }
    }

    tree_entries
        .sort_by(|a, b| tree_entry_cmp(&a.name, a.mode == 0o040000, &b.name, b.mode == 0o040000));

    let tree_data = serialize_tree(&tree_entries);
    repo.odb.write(ObjectKind::Tree, &tree_data)
}

/// Write a new notes tree and commit, updating the notes ref.
pub fn write_notes_commit(
    repo: &Repository,
    notes_ref: &str,
    entries: &[NotesTreeEntry],
    message: &str,
) -> Result<()> {
    let fanout = notes_fanout(entries);
    let rewritten_entries: Vec<_> = entries
        .iter()
        .map(|entry| NotesTreeEntry {
            mode: entry.mode,
            path: note_object_name(&entry.path)
                .map(|name| path_with_fanout(&name, fanout))
                .unwrap_or_else(|| entry.path.clone()),
            oid: entry.oid,
        })
        .collect();
    let tree_oid = write_notes_subtree(repo, &rewritten_entries)?;

    // Get existing notes commit as parent (if any)
    let parent = resolve_ref(&repo.git_dir, notes_ref).ok();

    // Build committer/author ident
    let config = ConfigSet::load(
        &crate::environment::Environment::capture_process(),
        Some(&repo.git_dir),
        true,
    )?;
    let now = now_for_identity();
    let author = build_ident_role(&config, "AUTHOR", now);
    let committer = build_ident_role(&config, "COMMITTER", now);

    let commit = CommitData {
        tree: tree_oid,
        parents: parent.into_iter().collect(),
        author,
        committer: committer.clone(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: if message.ends_with('\n') {
            message.to_owned()
        } else {
            format!("{message}\n")
        },
        raw_message: None,
    };

    let commit_data = serialize_commit(&commit);
    let commit_oid = repo.odb.write(ObjectKind::Commit, &commit_data)?;

    let old_oid = resolve_ref(&repo.git_dir, notes_ref).unwrap_or_else(|_| zero_oid());
    write_ref(&repo.git_dir, notes_ref, &commit_oid)?;
    if should_autocreate_reflog(&repo.git_dir, notes_ref) {
        let msg = message.trim_end_matches('\n');
        let reflog_msg = format!("notes: {msg}");
        let _ = append_reflog(
            &repo.git_dir,
            notes_ref,
            &old_oid,
            &commit_oid,
            &committer,
            &reflog_msg,
            false,
        );
    }
    Ok(())
}

/// Build a Git ident string from config.
/// Build an identity line for the notes commit, honoring the
/// `GIT_{AUTHOR,COMMITTER}_{NAME,EMAIL,DATE}` environment variables exactly like
/// `git notes` (and `git commit-tree`). `prefix` is either "AUTHOR" or "COMMITTER".
fn build_ident_role(config: &ConfigSet, prefix: &str, now: time::OffsetDateTime) -> String {
    let name_key = format!("GIT_{prefix}_NAME");
    let email_key = format!("GIT_{prefix}_EMAIL");
    let date_key = format!("GIT_{prefix}_DATE");

    let name = std::env::var(&name_key)
        .ok()
        .filter(|n| !n.trim().is_empty())
        .or_else(|| {
            if prefix == "COMMITTER" {
                std::env::var("GIT_AUTHOR_NAME")
                    .ok()
                    .filter(|n| !n.trim().is_empty())
            } else {
                None
            }
        })
        .or_else(|| config.get("user.name"))
        .unwrap_or_else(|| "Unknown".to_owned());

    let email = std::env::var(&email_key)
        .ok()
        .filter(|e| !e.trim().is_empty())
        .or_else(|| {
            if prefix == "COMMITTER" {
                std::env::var("GIT_AUTHOR_EMAIL")
                    .ok()
                    .filter(|e| !e.trim().is_empty())
            } else {
                None
            }
        })
        .or_else(|| config.get("user.email"))
        .unwrap_or_default();

    let date_override = std::env::var(&date_key)
        .ok()
        .filter(|d| !d.trim().is_empty());
    assemble_identity(&name, &email, date_override.as_deref(), now)
}

/// Per-worktree git directory for `NOTES_MERGE_*` (main: `.git/`, linked: `.git/worktrees/<id>/`).
pub fn notes_merge_git_dir(repo: &Repository) -> std::path::PathBuf {
    repo.git_dir.clone()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotesMergeStrategy {
    Manual,
    Ours,
    Theirs,
    Union,
    CatSortUniq,
}

#[derive(Clone, Debug)]
enum LocalNoteState {
    Unset,
    Deleted,
    Present(ObjectId),
}

#[derive(Clone, Debug)]
struct NotesMergePair {
    obj: ObjectId,
    base_blob: Option<ObjectId>,
    remote_blob: Option<ObjectId>,
    local: LocalNoteState,
}

/// Match Git's `expand_notes_ref` (`notes.c`): only `--ref` uses this; env/config refs are verbatim.
pub fn expand_notes_ref(short_or_full: &str) -> String {
    if short_or_full.starts_with("refs/notes/") {
        short_or_full.to_owned()
    } else if short_or_full.starts_with("notes/") {
        format!("refs/{short_or_full}")
    } else {
        format!("refs/notes/{short_or_full}")
    }
}

pub fn notes_merge_worktree_path(repo: &Repository) -> std::path::PathBuf {
    notes_merge_git_dir(repo).join(NOTES_MERGE_WORKTREE)
}

/// True when `NOTES_MERGE_WORKTREE` exists and is not empty (matches Git `is_empty_dir`).
pub fn notes_merge_worktree_nonempty(worktree: &std::path::Path) -> bool {
    if !worktree.is_dir() {
        return false;
    }
    let Ok(entries) = fs::read_dir(worktree) else {
        return false;
    };
    entries.flatten().next().is_some()
}

pub fn parse_notes_merge_strategy_value(s: &str) -> Option<NotesMergeStrategy> {
    match s {
        "manual" => Some(NotesMergeStrategy::Manual),
        "ours" => Some(NotesMergeStrategy::Ours),
        "theirs" => Some(NotesMergeStrategy::Theirs),
        "union" => Some(NotesMergeStrategy::Union),
        "cat_sort_uniq" => Some(NotesMergeStrategy::CatSortUniq),
        _ => None,
    }
}

/// Map annotated object → note blob OID for one notes tree (any fanout layout).
fn notes_tree_blob_by_object(
    repo: &Repository,
    tree_oid: &ObjectId,
) -> Result<HashMap<ObjectId, ObjectId>> {
    let mut flat = Vec::new();
    collect_notes_tree_entries(repo, tree_oid, b"", &mut flat)?;
    let mut map = HashMap::new();
    for entry in flat {
        let Some(hex) = note_object_name(&entry.path) else {
            continue;
        };
        let obj = ObjectId::from_hex(&hex)
            .map_err(|e| Error::Message(format!("invalid note object id in tree: {e}")))?;
        map.insert(obj, entry.oid);
    }
    Ok(map)
}

fn diff_note_blob_changes(
    repo: &Repository,
    old_tree: Option<&ObjectId>,
    new_tree: Option<&ObjectId>,
) -> Result<Vec<NoteBlobChange>> {
    let old_map = match old_tree {
        Some(t) => notes_tree_blob_by_object(repo, t)?,
        None => HashMap::new(),
    };
    let new_map = match new_tree {
        Some(t) => notes_tree_blob_by_object(repo, t)?,
        None => HashMap::new(),
    };
    let keys: BTreeSet<ObjectId> = old_map.keys().chain(new_map.keys()).copied().collect();
    let mut out = Vec::new();
    for obj in keys {
        let o_old = old_map.get(&obj).copied();
        let o_new = new_map.get(&obj).copied();
        match (o_old, o_new) {
            (None, Some(new_b)) => out.push((obj, None, Some(new_b))),
            (Some(old_b), None) => out.push((obj, Some(old_b), None)),
            (Some(old_b), Some(new_b)) if old_b != new_b => {
                out.push((obj, Some(old_b), Some(new_b)));
            }
            _ => {}
        }
    }
    Ok(out)
}

fn build_merge_pairs(
    base_tree: Option<ObjectId>,
    local_tree: ObjectId,
    remote_tree: ObjectId,
    repo: &Repository,
) -> Result<Vec<NotesMergePair>> {
    let remote_raw = diff_note_blob_changes(repo, base_tree.as_ref(), Some(&remote_tree))?;
    let local_raw = diff_note_blob_changes(repo, base_tree.as_ref(), Some(&local_tree))?;
    let mut map: HashMap<ObjectId, NotesMergePair> = HashMap::new();
    for (obj, old_b, new_b) in remote_raw {
        map.insert(
            obj,
            NotesMergePair {
                obj,
                base_blob: old_b,
                remote_blob: new_b,
                local: LocalNoteState::Unset,
            },
        );
    }

    for (obj, old_b, new_b) in local_raw {
        let local_state = match new_b {
            Some(new_oid) => LocalNoteState::Present(new_oid),
            None => LocalNoteState::Deleted,
        };
        if let Some(p) = map.get_mut(&obj) {
            p.local = local_state;
        } else {
            map.insert(
                obj,
                NotesMergePair {
                    obj,
                    base_blob: old_b,
                    remote_blob: old_b,
                    local: local_state,
                },
            );
        }
    }

    let mut v: Vec<_> = map.into_values().collect();
    v.sort_by_key(|a| a.obj);
    Ok(v)
}

fn read_blob_bytes(repo: &Repository, oid: &ObjectId) -> Result<Vec<u8>> {
    let obj = repo.odb.read(oid)?;
    if obj.kind != ObjectKind::Blob {
        return Err(Error::Message("expected blob for note".into()));
    }
    Ok(obj.data)
}

/// Matches Git's `combine_notes_concatenate`: join two note blobs with a blank line between them.
pub fn combine_notes_concatenate(
    repo: &Repository,
    cur: Option<&ObjectId>,
    new_oid: Option<&ObjectId>,
) -> Result<ObjectId> {
    let new_data = match new_oid {
        Some(n) => {
            let obj = repo.odb.read(n)?;
            if obj.kind != ObjectKind::Blob || obj.data.is_empty() {
                Vec::new()
            } else {
                obj.data
            }
        }
        None => Vec::new(),
    };
    if new_data.is_empty() {
        let Some(c) = cur else {
            return Err(Error::Message(
                "combine_notes_concatenate: empty new and no current".into(),
            ));
        };
        return Ok(*c);
    }

    let cur_data = match cur {
        Some(c) => {
            let obj = repo.odb.read(c)?;
            if obj.kind != ObjectKind::Blob || obj.data.is_empty() {
                Vec::new()
            } else {
                obj.data
            }
        }
        None => Vec::new(),
    };

    if cur_data.is_empty() {
        return repo.odb.write(ObjectKind::Blob, &new_data);
    }

    let mut cur_len = cur_data.len();
    if cur_len > 0 && cur_data[cur_len - 1] == b'\n' {
        cur_len -= 1;
    }
    let mut buf = Vec::with_capacity(cur_len + 2 + new_data.len());
    buf.extend_from_slice(&cur_data[..cur_len]);
    buf.push(b'\n');
    buf.push(b'\n');
    buf.extend_from_slice(&new_data);
    repo.odb.write(ObjectKind::Blob, &buf)
}

fn note_blob_lines(data: &[u8]) -> Vec<String> {
    if data.is_empty() {
        return Vec::new();
    }
    let s = String::from_utf8_lossy(data);
    s.split('\n').map(|l| l.to_owned()).collect()
}

/// Matches Git's `combine_notes_cat_sort_uniq`: all lines from both blobs, de-duplicated and sorted.
pub fn combine_notes_cat_sort_uniq(
    repo: &Repository,
    cur: Option<&ObjectId>,
    new_oid: Option<&ObjectId>,
) -> Result<ObjectId> {
    let mut lines: Vec<String> = Vec::new();
    for oid in [cur, new_oid].into_iter().flatten() {
        let obj = repo.odb.read(oid)?;
        if obj.kind == ObjectKind::Blob && !obj.data.is_empty() {
            lines.extend(note_blob_lines(&obj.data));
        }
    }
    lines.retain(|l| !l.is_empty());
    lines.sort();
    lines.dedup();
    let mut buf = String::new();
    for l in &lines {
        buf.push_str(l);
        buf.push('\n');
    }
    repo.odb.write(ObjectKind::Blob, buf.as_bytes())
}

fn blob_to_lines(data: &[u8]) -> Vec<String> {
    if data.is_empty() {
        return vec![String::new()];
    }
    let s = String::from_utf8_lossy(data).into_owned();
    s.split_inclusive('\n').map(|l| l.to_owned()).collect()
}

fn merge_note_blobs_conflict_markers(
    repo: &Repository,
    base: Option<&ObjectId>,
    local: &ObjectId,
    remote: &ObjectId,
    local_ref: &str,
    remote_ref: &str,
) -> Result<Vec<u8>> {
    let base_lines: Vec<String> = match base {
        Some(b) => blob_to_lines(&read_blob_bytes(repo, b)?),
        None => vec![String::new()],
    };
    let local_lines = blob_to_lines(&read_blob_bytes(repo, local)?);
    let remote_lines = blob_to_lines(&read_blob_bytes(repo, remote)?);

    let base_refs: Vec<&str> = base_lines.iter().map(|s| s.as_str()).collect();
    let local_refs: Vec<&str> = local_lines.iter().map(|s| s.as_str()).collect();
    let remote_refs: Vec<&str> = remote_lines.iter().map(|s| s.as_str()).collect();
    let m3 = Merge3::new(&base_refs, &local_refs, &remote_refs);
    let markers = StandardMarkers::new(Some(local_ref), Some(remote_ref));
    let merged: String = m3
        .merge_lines(false, &markers)
        .into_iter()
        .map(|cow| cow.into_owned())
        .collect();
    Ok(merged.into_bytes())
}

fn write_note_conflict_file(
    path: &std::path::Path,
    repo: &Repository,
    pair: &NotesMergePair,
    local_ref: &str,
    remote_ref: &str,
) -> Result<()> {
    let data = match (&pair.local, &pair.remote_blob) {
        (LocalNoteState::Deleted, Some(r)) => read_blob_bytes(repo, r)?,
        (LocalNoteState::Present(l), None) => read_blob_bytes(repo, l)?,
        (LocalNoteState::Present(l), Some(r)) => merge_note_blobs_conflict_markers(
            repo,
            pair.base_blob.as_ref(),
            l,
            r,
            local_ref,
            remote_ref,
        )?,
        _ => {
            return Err(Error::Message(
                "unexpected notes merge conflict shape".into(),
            ))
        }
    };
    fs::write(path, data)?;
    Ok(())
}

#[expect(clippy::too_many_arguments)]
fn merge_one_note_change(
    repo: &Repository,
    pair: &NotesMergePair,
    strategy: NotesMergeStrategy,
    local_ref: &str,
    remote_ref: &str,
    worktree: &std::path::Path,
    commit_msg: &mut String,
    entries: &mut Vec<NotesTreeEntry>,
    has_worktree: &mut bool,
) -> Result<bool> {
    let obj_hex = pair.obj.to_hex();
    let path = worktree.join(&obj_hex);
    match strategy {
        NotesMergeStrategy::Manual => {
            if !*has_worktree && notes_merge_worktree_nonempty(worktree) {
                return Err(Error::Message(
                    "You have not concluded your previous notes merge (.git/NOTES_MERGE_* exists).\n\
Please, use 'git notes merge --commit' or 'git notes merge --abort' to commit/abort the \
previous merge before you start a new notes merge."
                        .into(),
                ));
            }
            if !commit_msg.contains("Conflicts:") {
                commit_msg.push_str("\n\nConflicts:\n");
            }
            commit_msg.push_str(&format!("\t{obj_hex}\n"));
            if !*has_worktree {
                let test = worktree.join(".test");
                fs::create_dir_all(worktree)?;
                fs::write(&test, b"")?;
                let _ = fs::remove_file(test);
                *has_worktree = true;
            }
            write_note_conflict_file(&path, repo, pair, local_ref, remote_ref)?;
            entries.retain(|e| note_object_name(&e.path).as_deref() != Some(obj_hex.as_str()));
            Ok(true)
        }
        NotesMergeStrategy::Ours => Ok(false),
        NotesMergeStrategy::Theirs => {
            if let Some(r) = pair.remote_blob {
                upsert_note_entry(entries, &obj_hex, r);
            } else {
                entries.retain(|e| note_object_name(&e.path).as_deref() != Some(obj_hex.as_str()));
            }
            Ok(false)
        }
        NotesMergeStrategy::Union => {
            match (&pair.local, &pair.remote_blob) {
                (LocalNoteState::Deleted, None) => {
                    entries
                        .retain(|e| note_object_name(&e.path).as_deref() != Some(obj_hex.as_str()));
                }
                (LocalNoteState::Deleted, Some(r)) => {
                    let out = combine_notes_concatenate(repo, None, Some(r))?;
                    upsert_note_entry(entries, &obj_hex, out);
                }
                (LocalNoteState::Present(_), None) => {}
                (LocalNoteState::Present(l), Some(r)) => {
                    let out = combine_notes_concatenate(repo, Some(l), Some(r))?;
                    upsert_note_entry(entries, &obj_hex, out);
                }
                (LocalNoteState::Unset, _) => {
                    return Err(Error::Message(
                        "unexpected notes merge pair: local unset in union strategy".into(),
                    ));
                }
            }
            Ok(false)
        }
        NotesMergeStrategy::CatSortUniq => {
            match (&pair.local, &pair.remote_blob) {
                (LocalNoteState::Deleted, None) => {
                    entries
                        .retain(|e| note_object_name(&e.path).as_deref() != Some(obj_hex.as_str()));
                }
                (LocalNoteState::Deleted, Some(r)) => {
                    let out = combine_notes_cat_sort_uniq(repo, None, Some(r))?;
                    upsert_note_entry(entries, &obj_hex, out);
                }
                (LocalNoteState::Present(_), None) => {}
                (LocalNoteState::Present(l), Some(r)) => {
                    let out = combine_notes_cat_sort_uniq(repo, Some(l), Some(r))?;
                    upsert_note_entry(entries, &obj_hex, out);
                }
                (LocalNoteState::Unset, _) => {
                    return Err(Error::Message(
                        "unexpected notes merge pair: local unset in cat_sort_uniq strategy".into(),
                    ));
                }
            }
            Ok(false)
        }
    }
}

pub fn upsert_note_entry(entries: &mut Vec<NotesTreeEntry>, hex: &str, blob: ObjectId) {
    entries.retain(|e| note_object_name(&e.path).as_deref() != Some(hex));
    entries.push(NotesTreeEntry {
        mode: 0o100644,
        path: hex.as_bytes().to_vec(),
        oid: blob,
    });
}

fn remote_unchanged(base: Option<ObjectId>, remote: Option<ObjectId>) -> bool {
    match (base, remote) {
        (Some(b), Some(r)) => b == r,
        (None, None) => true,
        _ => false,
    }
}

fn same_change_local_remote(p: &NotesMergePair) -> bool {
    match (&p.local, p.remote_blob) {
        (LocalNoteState::Present(l), Some(r)) => l == &r,
        (LocalNoteState::Deleted, None) => true,
        (LocalNoteState::Unset, Some(r)) => Some(r) == p.base_blob,
        (LocalNoteState::Unset, None) => p.base_blob.is_none(),
        _ => false,
    }
}

fn no_local_change(local: &LocalNoteState, base: Option<ObjectId>) -> bool {
    match local {
        LocalNoteState::Unset => true,
        LocalNoteState::Present(l) => Some(*l) == base,
        LocalNoteState::Deleted => false,
    }
}

fn adopt_remote_note(entries: &mut Vec<NotesTreeEntry>, obj_hex: &str, remote: Option<ObjectId>) {
    match remote {
        Some(oid) => upsert_note_entry(entries, obj_hex, oid),
        None => entries.retain(|e| note_object_name(&e.path).as_deref() != Some(obj_hex)),
    }
}

#[expect(clippy::too_many_arguments)]
fn merge_changes_into_entries(
    repo: &Repository,
    pairs: &[NotesMergePair],
    strategy: NotesMergeStrategy,
    local_ref: &str,
    remote_ref: &str,
    worktree: &std::path::Path,
    commit_msg: &mut String,
    entries: &mut Vec<NotesTreeEntry>,
) -> Result<usize> {
    let mut conflicts = 0usize;
    let mut has_worktree = false;
    for p in pairs {
        if remote_unchanged(p.base_blob, p.remote_blob) {
            continue;
        }
        if same_change_local_remote(p) {
            continue;
        }
        if no_local_change(&p.local, p.base_blob) {
            adopt_remote_note(entries, &p.obj.to_hex(), p.remote_blob);
            continue;
        }
        if merge_one_note_change(
            repo,
            p,
            strategy,
            local_ref,
            remote_ref,
            worktree,
            commit_msg,
            entries,
            &mut has_worktree,
        )? {
            conflicts += 1;
        }
    }
    Ok(conflicts)
}

fn resolve_commit_tree(repo: &Repository, commit_oid: &ObjectId) -> Result<ObjectId> {
    let obj = repo.odb.read(commit_oid)?;
    if obj.kind != ObjectKind::Commit {
        return Err(Error::Message("expected commit".into()));
    }
    Ok(parse_commit(&obj.data)?.tree)
}

fn resolve_notes_commit_optional(repo: &Repository, notes_ref: &str) -> Result<Option<ObjectId>> {
    let oid = match resolve_ref(&repo.git_dir, notes_ref) {
        Ok(o) => o,
        Err(_) => return Ok(None),
    };
    let obj = repo.odb.read(&oid)?;
    if obj.kind != ObjectKind::Commit {
        return Err(Error::Message(format!(
            "{notes_ref} does not point to a commit"
        )));
    }
    Ok(Some(oid))
}

pub fn write_notes_commit_with_parents(
    repo: &Repository,
    _notes_ref: &str,
    entries: &[NotesTreeEntry],
    message: &str,
    parents: &[ObjectId],
) -> Result<ObjectId> {
    let fanout = notes_fanout(entries);
    let rewritten_entries: Vec<_> = entries
        .iter()
        .map(|entry| NotesTreeEntry {
            mode: entry.mode,
            path: note_object_name(&entry.path)
                .map(|name| path_with_fanout(&name, fanout))
                .unwrap_or_else(|| entry.path.clone()),
            oid: entry.oid,
        })
        .collect();
    let tree_oid = write_notes_subtree(repo, &rewritten_entries)?;
    let config = ConfigSet::load(
        &crate::environment::Environment::capture_process(),
        Some(&repo.git_dir),
        true,
    )?;
    let now = now_for_identity();
    let author = build_ident_role(&config, "AUTHOR", now);
    let committer = build_ident_role(&config, "COMMITTER", now);
    let commit = CommitData {
        tree: tree_oid,
        parents: parents.to_vec(),
        author,
        committer,
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: if message.ends_with('\n') {
            message.to_owned()
        } else {
            format!("{message}\n")
        },
        raw_message: None,
    };
    let commit_data = serialize_commit(&commit);
    repo.odb.write(ObjectKind::Commit, &commit_data)
}

pub fn notes_merge_inner(
    repo: &Repository,
    local_ref: &str,
    remote_ref: &str,
    strategy: NotesMergeStrategy,
) -> Result<std::result::Result<ObjectId, ObjectId>> {
    let local_commit = resolve_notes_commit_optional(repo, local_ref)?;
    let remote_commit = resolve_notes_commit_optional(repo, remote_ref)?;
    match (local_commit, remote_commit) {
        (None, None) => Err(Error::Message(format!(
            "Cannot merge empty notes ref ({remote_ref}) into empty notes ref ({local_ref})"
        ))),
        (None, Some(r)) => Ok(Ok(r)),
        (Some(l), None) => Ok(Ok(l)),
        (Some(local_oid), Some(remote_oid)) => {
            if local_oid == remote_oid {
                return Ok(Ok(local_oid));
            }
            let bases = merge_bases_first_vs_rest(repo, local_oid, &[remote_oid])?;
            let base_commit = bases.into_iter().next();
            if Some(local_oid) == base_commit {
                return Ok(Ok(remote_oid));
            }
            if Some(remote_oid) == base_commit {
                return Ok(Ok(local_oid));
            }
            let base_tree = base_commit
                .map(|bc| resolve_commit_tree(repo, &bc))
                .transpose()?;
            let local_tree = resolve_commit_tree(repo, &local_oid)?;
            let remote_tree = resolve_commit_tree(repo, &remote_oid)?;
            let mut commit_msg = format!("Merged notes from {remote_ref} into {local_ref}\n\n");
            let pairs = build_merge_pairs(base_tree, local_tree, remote_tree, repo)?;
            let mut entries = read_notes_tree(repo, local_ref)?;
            let worktree = notes_merge_worktree_path(repo);
            let conflicts = merge_changes_into_entries(
                repo,
                &pairs,
                strategy,
                local_ref,
                remote_ref,
                &worktree,
                &mut commit_msg,
                &mut entries,
            )?;
            let merge_parents = vec![local_oid, remote_oid];
            if conflicts > 0 {
                let partial = write_notes_commit_with_parents(
                    repo,
                    local_ref,
                    &entries,
                    &commit_msg,
                    &merge_parents,
                )?;
                return Ok(Err(partial));
            }
            let new_oid = write_notes_commit_with_parents(
                repo,
                local_ref,
                &entries,
                &commit_msg,
                &merge_parents,
            )?;
            Ok(Ok(new_oid))
        }
    }
}
