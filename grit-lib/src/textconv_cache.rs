//! Git-compatible `diff.<driver>.cachetextconv` storage under `refs/notes/textconv/<driver>`.
//!
//! Matches Git's `notes-cache.c`: the notes ref's **commit subject** stores the current
//! `diff.<driver>.textconv` command string; when it changes, the cache is treated as empty.

use std::collections::BTreeMap;
use std::path::Path;

use crate::config::ConfigSet;
use crate::objects::{
    parse_commit, parse_tree, serialize_commit, serialize_tree, tree_entry_cmp, CommitData,
    ObjectId, ObjectKind, TreeEntry,
};
use crate::odb::Odb;
use crate::refs::{resolve_ref, write_ref};

#[derive(Clone)]
struct NotesEntry {
    path: Vec<u8>,
    blob_oid: ObjectId,
}

fn note_object_hex(path: &[u8]) -> Option<String> {
    let compact: Vec<u8> = path.iter().copied().filter(|b| *b != b'/').collect();
    if compact.len() != 40 || !compact.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    String::from_utf8(compact)
        .ok()
        .map(|s| s.to_ascii_lowercase())
}

fn collect_tree_entries(
    odb: &Odb,
    tree_oid: &ObjectId,
    prefix: &[u8],
    out: &mut Vec<NotesEntry>,
) -> crate::error::Result<()> {
    let tree_obj = odb.read(tree_oid)?;
    if tree_obj.kind != ObjectKind::Tree {
        return Err(crate::error::Error::CorruptObject(
            "notes tree is not a tree object".to_owned(),
        ));
    }
    for entry in parse_tree(&tree_obj.data)? {
        let mut path = prefix.to_vec();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(&entry.name);
        if entry.mode == 0o040000 {
            collect_tree_entries(odb, &entry.oid, &path, out)?;
        } else {
            out.push(NotesEntry {
                path,
                blob_oid: entry.oid,
            });
        }
    }
    Ok(())
}

fn read_notes_entries(
    odb: &Odb,
    git_dir: &Path,
    notes_ref: &str,
) -> crate::error::Result<Vec<NotesEntry>> {
    let Ok(commit_oid) = resolve_ref(git_dir, notes_ref) else {
        return Ok(Vec::new());
    };
    let commit_obj = odb.read(&commit_oid)?;
    if commit_obj.kind != ObjectKind::Commit {
        return Err(crate::error::Error::CorruptObject(
            "notes ref does not point to a commit".to_owned(),
        ));
    }
    let commit = parse_commit(&commit_obj.data)?;
    let mut out = Vec::new();
    collect_tree_entries(odb, &commit.tree, b"", &mut out)?;
    Ok(out)
}

fn notes_fanout(entries: &[NotesEntry]) -> usize {
    let mut note_count = entries
        .iter()
        .filter(|e| note_object_hex(&e.path).is_some())
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

enum NotesChild {
    Blob(ObjectId),
    Tree(Vec<NotesEntry>),
}

fn write_notes_subtree(odb: &Odb, entries: &[NotesEntry]) -> crate::error::Result<ObjectId> {
    let mut children: BTreeMap<Vec<u8>, NotesChild> = BTreeMap::new();
    for entry in entries {
        if let Some(slash_pos) = entry.path.iter().position(|b| *b == b'/') {
            let child_name = entry.path[..slash_pos].to_vec();
            let child_entry = NotesEntry {
                path: entry.path[slash_pos + 1..].to_vec(),
                blob_oid: entry.blob_oid,
            };
            children
                .entry(child_name.clone())
                .or_insert_with(|| NotesChild::Tree(Vec::new()));
            if let Some(NotesChild::Tree(tree_entries)) = children.get_mut(&child_name) {
                tree_entries.push(child_entry);
            }
        } else {
            children.insert(entry.path.clone(), NotesChild::Blob(entry.blob_oid));
        }
    }
    let mut tree_entries = Vec::with_capacity(children.len());
    for (name, child) in children {
        match child {
            NotesChild::Blob(oid) => tree_entries.push(TreeEntry {
                mode: 0o100644,
                name,
                oid,
            }),
            NotesChild::Tree(child_entries) => {
                let oid = write_notes_subtree(odb, &child_entries)?;
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
    let data = serialize_tree(&tree_entries);
    odb.write(ObjectKind::Tree, &data)
}

fn write_notes_ref(
    odb: &Odb,
    git_dir: &Path,
    notes_ref: &str,
    entries: &[NotesEntry],
    message: &str,
) -> crate::error::Result<()> {
    let fanout = notes_fanout(entries);
    let rewritten: Vec<NotesEntry> = entries
        .iter()
        .map(|e| NotesEntry {
            path: note_object_hex(&e.path)
                .map(|h| path_with_fanout(&h, fanout))
                .unwrap_or_else(|| e.path.clone()),
            blob_oid: e.blob_oid,
        })
        .collect();
    let tree_oid = write_notes_subtree(odb, &rewritten)?;
    let parent = resolve_ref(git_dir, notes_ref).ok();
    let config = ConfigSet::load(
        &crate::environment::Environment::capture_process(),
        Some(git_dir),
        true,
    )
    .unwrap_or_default();
    let now = time::OffsetDateTime::now_utc();
    let ident = grit_ident(&config, now);
    let commit = CommitData {
        tree: tree_oid,
        parents: parent.into_iter().collect(),
        author: ident.clone(),
        committer: ident,
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
    let bytes = serialize_commit(&commit);
    let commit_oid = odb.write(ObjectKind::Commit, &bytes)?;
    write_ref(git_dir, notes_ref, &commit_oid)?;
    Ok(())
}

fn grit_ident(config: &ConfigSet, now: time::OffsetDateTime) -> String {
    let name = std::env::var("GIT_COMMITTER_NAME")
        .ok()
        .or_else(|| config.get("user.name"))
        .unwrap_or_else(|| "grit".to_owned());
    let email = std::env::var("GIT_COMMITTER_EMAIL")
        .ok()
        .or_else(|| config.get("user.email"))
        .unwrap_or_default();
    let epoch = now.unix_timestamp();
    let offset = now.offset();
    let hours = offset.whole_hours();
    let minutes = offset.minutes_past_hour().unsigned_abs();
    format!("{name} <{email}> {epoch} {hours:+03}{minutes:02}")
}

fn cache_commit_message_trimmed(odb: &Odb, git_dir: &Path, notes_ref: &str) -> Option<String> {
    let commit_oid = resolve_ref(git_dir, notes_ref).ok()?;
    let obj = odb.read(&commit_oid).ok()?;
    if obj.kind != ObjectKind::Commit {
        return None;
    }
    let c = parse_commit(&obj.data).ok()?;
    let mut msg = c.message;
    while msg.ends_with('\n') {
        msg.pop();
    }
    Some(msg)
}

fn cache_validity_matches(odb: &Odb, git_dir: &Path, notes_ref: &str, validity: &str) -> bool {
    let Some(stored) = cache_commit_message_trimmed(odb, git_dir, notes_ref) else {
        return false;
    };
    stored == validity
}

fn find_cached_blob_oid(entries: &[NotesEntry], blob_oid: &ObjectId) -> Option<ObjectId> {
    let hex = blob_oid.to_hex();
    for e in entries {
        if note_object_hex(&e.path).as_deref() == Some(hex.as_str()) {
            return Some(e.blob_oid);
        }
    }
    None
}

/// Read cached textconv bytes for `blob_oid`, or `None` on miss / invalid cache.
pub fn read_textconv_cache(
    odb: &Odb,
    git_dir: &Path,
    driver: &str,
    validity: &str,
    blob_oid: &ObjectId,
) -> Option<Vec<u8>> {
    let notes_ref = format!("refs/notes/textconv/{driver}");
    if !cache_validity_matches(odb, git_dir, &notes_ref, validity) {
        return None;
    }
    let entries = read_notes_entries(odb, git_dir, &notes_ref).ok()?;
    let note_blob = find_cached_blob_oid(&entries, blob_oid)?;
    let obj = odb.read(&note_blob).ok()?;
    if obj.kind != ObjectKind::Blob {
        return None;
    }
    Some(obj.data)
}

/// Store `data` as the note for `blob_oid`. Errors are ignored (read-only repos).
pub fn write_textconv_cache(
    odb: &Odb,
    git_dir: &Path,
    driver: &str,
    validity: &str,
    blob_oid: &ObjectId,
    data: &[u8],
) {
    let notes_ref = format!("refs/notes/textconv/{driver}");
    let mut entries = if cache_validity_matches(odb, git_dir, &notes_ref, validity) {
        read_notes_entries(odb, git_dir, &notes_ref).unwrap_or_default()
    } else {
        Vec::new()
    };
    let hex = blob_oid.to_hex();
    entries.retain(|e| note_object_hex(&e.path).as_deref() != Some(hex.as_str()));
    let value_oid = match odb.write(ObjectKind::Blob, data) {
        Ok(oid) => oid,
        Err(_) => return,
    };
    entries.push(NotesEntry {
        path: hex.into_bytes(),
        blob_oid: value_oid,
    });
    let _ = write_notes_ref(odb, git_dir, &notes_ref, &entries, validity);
}
