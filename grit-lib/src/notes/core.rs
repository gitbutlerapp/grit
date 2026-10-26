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
//! a merge strategy). Notes refs are read and updated through the repository's
//! [`RefStore`](crate::refs::store::RefStore), so every ref backend (loose/packed,
//! reftable, or an injected store) sees the same notes.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;

use time::{OffsetDateTime, UtcOffset};

use crate::commit::assemble_identity;
use crate::config::ConfigSet;
use crate::error::{Error, Result};
use crate::ident::parse_signature_times;
use crate::ident_resolve::{IdentRole, IdentityEnv};
use crate::merge_base::merge_bases_first_vs_rest;
use crate::objects::{
    parse_commit, parse_tree, serialize_commit, serialize_tree, tree_entry_cmp, CommitData,
    ObjectId, ObjectKind, TreeEntry,
};
use crate::refs::should_autocreate_reflog_with_config;
use crate::refs::store::{Expected, RawRef, ReflogUpdate};
use crate::refs::store_write_value_on_store;
use crate::repo::Repository;
use crate::rev_parse::resolve_revision;
use merge3::{Merge3, StandardMarkers};

/// Per-worktree subdirectory holding the conflicted note blobs during a notes merge.
const NOTES_MERGE_WORKTREE: &str = "NOTES_MERGE_WORKTREE";

/// One leaf entry in a notes fanout tree (blob pointing at a note object).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotesTreeEntry {
    /// Git tree entry mode for the note blob (typically `100644`).
    pub mode: u32,
    /// Fanout path segments ending in the annotated object's hex id.
    pub path: Vec<u8>,
    /// Note blob object id stored at this path.
    pub oid: ObjectId,
}

/// One annotated object's note change: `(object_id, old_blob, new_blob)`.
type NoteBlobChange = (ObjectId, Option<ObjectId>, Option<ObjectId>);

enum NotesTreeChild {
    Blob {
        /// Tree entry mode for the note blob.
        mode: u32,
        /// Note blob object id.
        oid: ObjectId,
    },
    Tree(Vec<NotesTreeEntry>),
}

/// Extract the annotated object's hex id from a notes tree entry path, if well-formed.
pub fn note_object_name(path: &[u8]) -> Option<String> {
    let compact: Vec<u8> = path.iter().copied().filter(|byte| *byte != b'/').collect();
    if !ObjectId::is_hex_len(compact.len()) || !compact.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    String::from_utf8(compact)
        .ok()
        .map(|name| name.to_ascii_lowercase())
}

fn collect_notes_tree_entries(
    repo: &Repository,
    tree_oid: &ObjectId,
    prefix: &[u8],
    out: &mut Vec<NotesTreeEntry>,
    max_listable_notes: Option<usize>,
    listable_seen: &mut usize,
    truncated: &mut bool,
) -> Result<()> {
    if max_listable_notes.is_some_and(|max| *listable_seen >= max) {
        *truncated = true;
        return Ok(());
    }
    let tree_obj = repo.odb().read(tree_oid)?;
    if tree_obj.kind != ObjectKind::Tree {
        return Err(Error::NotesObjectKind {
            oid: *tree_oid,
            expected: ObjectKind::Tree,
            found: tree_obj.kind,
        });
    }

    for entry in parse_tree(&tree_obj.data)? {
        if max_listable_notes.is_some_and(|max| *listable_seen >= max) {
            *truncated = true;
            return Ok(());
        }
        let mut path = prefix.to_vec();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(&entry.name);

        if entry.mode == 0o040000 {
            collect_notes_tree_entries(
                repo,
                &entry.oid,
                &path,
                out,
                max_listable_notes,
                listable_seen,
                truncated,
            )?;
            if *truncated {
                return Ok(());
            }
        } else {
            let counts = note_object_name(&path).is_some();
            if max_listable_notes.is_none() || counts {
                out.push(NotesTreeEntry {
                    mode: entry.mode,
                    path,
                    oid: entry.oid,
                });
            }
            if counts {
                *listable_seen += 1;
                if max_listable_notes.is_some_and(|max| *listable_seen >= max) {
                    *truncated = true;
                    return Ok(());
                }
            }
        }
    }

    Ok(())
}

/// Current target of `notes_ref` in the repository's ref store, or `None` when the ref is absent.
fn notes_ref_tip(repo: &Repository, notes_ref: &str) -> Result<Option<ObjectId>> {
    match repo.refs().read_raw(notes_ref)? {
        None => Ok(None),
        Some(RawRef::Direct(oid)) => Ok(Some(oid)),
        Some(RawRef::Symbolic(_)) => repo.refs().resolve(notes_ref).map(Some),
    }
}

/// Read the notes tree entries from the notes ref.
///
/// `notes_ref` is looked up in the repository's ref store; when no such ref exists it is
/// resolved as a revision instead (so a notes commit or tree id works too). The target may be a
/// notes commit or a bare tree.
///
/// # Errors
///
/// - [`Error::NotesRefNotCommit`] when the target is neither a commit nor a tree.
/// - [`Error::NotesObjectKind`] when a subtree entry is not a tree.
/// - Ref store and object read failures.
///
/// Returns an empty vec when neither a ref nor a revision named `notes_ref` exists.
pub fn read_notes_tree(repo: &Repository, notes_ref: &str) -> Result<Vec<NotesTreeEntry>> {
    Ok(read_notes_snapshot(repo, notes_ref)?.entries)
}

/// Notes tree entries together with the notes ref tip they were read from.
pub(crate) struct NotesSnapshot {
    /// Tip of the notes ref when the entries were read; `None` when the ref did not exist.
    /// Writes based on these entries must use it as both parent and compare-and-swap value.
    pub(crate) tip: Option<ObjectId>,
    /// Flat note entries of the tree at `tip`.
    pub(crate) entries: Vec<NotesTreeEntry>,
}

/// Like [`read_notes_tree`], also returning the ref tip the entries came from.
pub(crate) fn read_notes_snapshot(repo: &Repository, notes_ref: &str) -> Result<NotesSnapshot> {
    let ref_tip = notes_ref_tip(repo, notes_ref)?;
    let tip = match ref_tip {
        Some(oid) => oid,
        None => match resolve_revision(repo, notes_ref) {
            Ok(oid) => oid,
            Err(_) => {
                return Ok(NotesSnapshot {
                    tip: None,
                    entries: Vec::new(),
                })
            }
        },
    };
    let obj = repo.odb().read(&tip)?;
    let tree_oid = match obj.kind {
        ObjectKind::Commit => parse_commit(&obj.data)?.tree,
        ObjectKind::Tree => tip,
        found => {
            return Err(Error::NotesRefNotCommit {
                notes_ref: notes_ref.to_owned(),
                found,
            })
        }
    };
    let mut entries = Vec::new();
    let mut listable_seen = 0usize;
    let mut truncated = false;
    collect_notes_tree_entries(
        repo,
        &tree_oid,
        b"",
        &mut entries,
        None,
        &mut listable_seen,
        &mut truncated,
    )?;
    debug_assert!(!truncated, "unlimited notes collect must not truncate");
    Ok(NotesSnapshot {
        tip: ref_tip,
        entries,
    })
}

/// Read note tree entries, stopping after `max_listable_notes` **listable** notes when set.
///
/// Non-object path segments (for example fanout `!` markers) do not count toward the cap.
///
/// Returns `(entries, truncated)` where `truncated` is true when the tree held more listable
/// notes than returned.
pub fn read_notes_tree_limited(
    repo: &Repository,
    notes_ref: &str,
    max_entries: Option<usize>,
) -> Result<(Vec<NotesTreeEntry>, bool)> {
    let ref_tip = notes_ref_tip(repo, notes_ref)?;
    let tip = match ref_tip {
        Some(oid) => oid,
        None => match resolve_revision(repo, notes_ref) {
            Ok(oid) => oid,
            Err(_) => return Ok((Vec::new(), false)),
        },
    };
    let obj = repo.odb().read(&tip)?;
    let tree_oid = match obj.kind {
        ObjectKind::Commit => parse_commit(&obj.data)?.tree,
        ObjectKind::Tree => tip,
        found => {
            return Err(Error::NotesRefNotCommit {
                notes_ref: notes_ref.to_owned(),
                found,
            })
        }
    };
    let mut entries = Vec::new();
    let mut listable_seen = 0usize;
    let mut truncated = false;
    collect_notes_tree_entries(
        repo,
        &tree_oid,
        b"",
        &mut entries,
        max_entries,
        &mut listable_seen,
        &mut truncated,
    )?;
    Ok((entries, truncated))
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
    repo.odb().write(ObjectKind::Tree, &tree_data)
}

/// Write `entries` as a notes tree, re-laying out note paths with the fanout their count needs.
fn write_notes_tree(repo: &Repository, entries: &[NotesTreeEntry]) -> Result<ObjectId> {
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
    write_notes_subtree(repo, &rewritten_entries)
}

/// Identity lines and message for one notes commit.
pub(crate) struct NotesCommit<'a> {
    pub(crate) message: &'a str,
    /// Full author line (`Name <email> unix tz`).
    pub(crate) author: &'a str,
    /// Full committer line (`Name <email> unix tz`).
    pub(crate) committer: &'a str,
}

/// Write the notes tree for `entries` and a commit on top of `parents`; returns the commit id.
pub(crate) fn preview_notes_commit(
    repo: &Repository,
    parent: Option<ObjectId>,
    entries: &[NotesTreeEntry],
    commit: &NotesCommit<'_>,
) -> Result<ObjectId> {
    write_notes_commit_object(repo, entries, commit, parent.into_iter().collect())
}

fn write_notes_commit_object(
    repo: &Repository,
    entries: &[NotesTreeEntry],
    commit: &NotesCommit<'_>,
    parents: Vec<ObjectId>,
) -> Result<ObjectId> {
    let tree = write_notes_tree(repo, entries)?;
    let message = if commit.message.ends_with('\n') {
        commit.message.to_owned()
    } else {
        format!("{}\n", commit.message)
    };
    let data = CommitData {
        tree,
        parents,
        author: commit.author.to_owned(),
        committer: commit.committer.to_owned(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message,
        raw_message: None,
        extra_headers: Vec::new(),
    };
    repo.odb()
        .write(ObjectKind::Commit, &serialize_commit(&data))
}

/// Timestamp of a full identity line, for the reflog entry written next to it.
fn signature_time(ident: &str) -> OffsetDateTime {
    parse_signature_times(ident)
        .and_then(|times| {
            let offset =
                UtcOffset::from_whole_seconds(i32::try_from(times.tz_offset_secs).ok()?).ok()?;
            let at = OffsetDateTime::from_unix_timestamp(times.unix_seconds).ok()?;
            Some(at.to_offset(offset))
        })
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

/// Move `notes_ref` from `old` (`None`: the ref must not exist yet) to `new`, logging the update
/// in the same ref transaction when the ref has or may auto-create a reflog.
fn update_notes_ref(
    repo: &Repository,
    notes_ref: &str,
    old: Option<ObjectId>,
    new: ObjectId,
    commit: &NotesCommit<'_>,
) -> Result<()> {
    let config = repo.config()?;
    let log = should_autocreate_reflog_with_config(config.as_ref(), repo.git_dir(), notes_ref)
        || repo.refs().reflog_exists(notes_ref)?;
    let reflog = log.then(|| ReflogUpdate {
        identity: commit.committer.to_owned(),
        message: format!("notes: {}", commit.message.trim_end_matches('\n')),
        time: signature_time(commit.committer),
        old_oid: None,
        new_oid: None,
    });
    store_write_value_on_store(
        repo.refs(),
        repo.git_dir(),
        notes_ref,
        Some(RawRef::Direct(new)),
        old.map_or(Expected::Missing, Expected::Oid),
        reflog,
    )
}

/// Commit `entries` on top of `parent` and move `notes_ref` from `parent` to the new commit.
///
/// `parent` must be the tip the entries were read from, so a concurrent update in between
/// fails the compare-and-swap instead of being dropped from the new tree.
fn commit_notes_tree(
    repo: &Repository,
    notes_ref: &str,
    parent: Option<ObjectId>,
    entries: &[NotesTreeEntry],
    commit: &NotesCommit<'_>,
) -> Result<ObjectId> {
    let oid = write_notes_commit_object(repo, entries, commit, parent.into_iter().collect())?;
    update_notes_ref(repo, notes_ref, parent, oid, commit)?;
    Ok(oid)
}

/// Write a new notes tree and commit, updating the notes ref.
///
/// Author and committer come from `GIT_{AUTHOR,COMMITTER}_{NAME,EMAIL,DATE}` and `user.*`
/// config, timestamped with the repository's wall clock. Use
/// [`RepositoryNotes`](crate::repo_handles::RepositoryNotes) to pass explicit identities.
///
/// `entries` replace the whole notes tree. The commit's parent and the compare-and-swap value
/// are the tip read here, so an update made after the caller read `entries` but before this call
/// is overwritten; [`RepositoryNotes`](crate::repo_handles::RepositoryNotes) reads and writes
/// against one tip and rejects such races.
///
/// # Errors
///
/// - Ref store errors, including a compare-and-swap failure when `notes_ref` moves while the
///   commit is written.
/// - Object write and config load failures.
pub fn write_notes_commit(
    repo: &Repository,
    notes_ref: &str,
    entries: &[NotesTreeEntry],
    message: &str,
) -> Result<()> {
    let (author, committer) = wall_clock_notes_idents(repo)?;
    let parent = notes_ref_tip(repo, notes_ref)?;
    commit_notes_tree(
        repo,
        notes_ref,
        parent,
        entries,
        &NotesCommit {
            message,
            author: &author,
            committer: &committer,
        },
    )?;
    Ok(())
}

/// Write a notes tree commit using explicit author and committer identity lines.
///
/// Identity strings must already include timestamp and timezone (`Name <email> unix tz`).
/// `parent` is the [`NotesSnapshot::tip`] the entries were read from.
pub(crate) fn write_notes_commit_idents(
    repo: &Repository,
    notes_ref: &str,
    parent: Option<ObjectId>,
    entries: &[NotesTreeEntry],
    message: &str,
    author: &str,
    committer: &str,
) -> Result<()> {
    commit_notes_tree(
        repo,
        notes_ref,
        parent,
        entries,
        &NotesCommit {
            message,
            author,
            committer,
        },
    )?;
    Ok(())
}

/// Remove a note for `object_hex` when present. Returns whether an entry was removed.
#[must_use]
pub fn remove_note_entry(entries: &mut Vec<NotesTreeEntry>, object_hex: &str) -> bool {
    let before = entries.len();
    entries.retain(|e| note_object_name(&e.path).as_deref() != Some(object_hex));
    entries.len() < before
}

/// Author and committer lines for notes commits written without explicit identities.
fn wall_clock_notes_idents(repo: &Repository) -> Result<(String, String)> {
    let config = repo.config()?;
    let now = repo.identity_now();
    Ok((
        notes_ident(repo.environment(), config.as_ref(), IdentRole::Author, now),
        notes_ident(
            repo.environment(),
            config.as_ref(),
            IdentRole::Committer,
            now,
        ),
    ))
}

/// Identity line for a notes commit from `GIT_<ROLE>_{NAME,EMAIL,DATE}` (the committer also
/// falls back to the author variables), then `user.name` / `user.email`.
fn notes_ident<E: IdentityEnv>(
    env: &E,
    config: &ConfigSet,
    role: IdentRole,
    now: OffsetDateTime,
) -> String {
    let (name_key, email_key, date_key) = match role {
        IdentRole::Author => ("GIT_AUTHOR_NAME", "GIT_AUTHOR_EMAIL", "GIT_AUTHOR_DATE"),
        IdentRole::Committer => (
            "GIT_COMMITTER_NAME",
            "GIT_COMMITTER_EMAIL",
            "GIT_COMMITTER_DATE",
        ),
    };
    let non_blank = |key: &str| env.var(key).filter(|v| !v.trim().is_empty());
    let author_fallback = |key: &str| match role {
        IdentRole::Committer => non_blank(key),
        IdentRole::Author => None,
    };

    let name = non_blank(name_key)
        .or_else(|| author_fallback("GIT_AUTHOR_NAME"))
        .or_else(|| config.get("user.name"))
        .unwrap_or_else(|| "Unknown".to_owned());
    let email = non_blank(email_key)
        .or_else(|| author_fallback("GIT_AUTHOR_EMAIL"))
        .or_else(|| config.get("user.email"))
        .unwrap_or_default();
    assemble_identity(&name, &email, non_blank(date_key).as_deref(), now)
}

/// How to combine conflicting note blobs during a notes merge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotesMergeStrategy {
    /// Leave conflicts for the user to resolve in the worktree.
    Manual,
    /// Keep the local note blob on conflict.
    Ours,
    /// Keep the remote note blob on conflict.
    Theirs,
    /// Concatenate both note blobs on conflict.
    Union,
    /// Concatenate, sort lines, and deduplicate on conflict.
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

impl NotesMergePair {
    /// The local side's note relative to the merge base: `None` when local left the note as it
    /// was at the base, `Some(None)` when local deleted it, `Some(Some(blob))` when local set it.
    fn local_change(&self) -> Option<Option<ObjectId>> {
        match self.local {
            LocalNoteState::Unset => None,
            LocalNoteState::Present(blob) if Some(blob) == self.base_blob => None,
            LocalNoteState::Present(blob) => Some(Some(blob)),
            LocalNoteState::Deleted => Some(None),
        }
    }
}

/// Full ref name for a Git notes namespace (`refs/notes/…`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NotesRef(String);

impl NotesRef {
    /// Default notes ref (`refs/notes/commits`).
    #[must_use]
    pub fn commits() -> Self {
        Self("refs/notes/commits".to_owned())
    }

    /// Parse a full ref name that must live under `refs/notes/`.
    ///
    /// Used for `GIT_NOTES_REF`, `core.notesRef`, and other configured refs (not
    /// `--ref`, which uses [`Self::parse`]).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidNotesRef`](crate::error::Error::InvalidNotesRef) when
    /// `full` is not prefixed with `refs/notes/` or is not a valid ref name (env/config
    /// values are not trimmed or otherwise normalized).
    pub fn from_full(full: &str) -> Result<Self> {
        if !full.starts_with("refs/notes/") {
            return Err(Error::InvalidNotesRef(full.to_owned()));
        }
        use crate::check_ref_format::{check_refname_format, RefNameOptions};
        if check_refname_format(full, &RefNameOptions::default()).is_err() {
            return Err(Error::InvalidNotesRef(full.to_owned()));
        }
        Ok(Self(full.to_owned()))
    }

    /// Parse a short or full notes ref the same way Git's `--ref` flag does: `name` and
    /// `notes/name` both become `refs/notes/name`; a name already under `refs/notes/` is kept.
    #[must_use]
    pub fn parse(short_or_full: &str) -> Self {
        Self(expand_notes_ref(short_or_full))
    }

    /// Borrow the canonical ref string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for NotesRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Resolve the default notes ref from `GIT_NOTES_REF`, then `core.notesRef`, then
/// [`NotesRef::commits`].
///
/// Config and environment values must be full refs under `refs/notes/` (same rules
/// as Git's `init_notes_ref`).
///
/// # Errors
///
/// Returns [`Error::InvalidNotesRef`](crate::error::Error::InvalidNotesRef) when a
/// configured value is empty or outside `refs/notes/`.
pub fn resolve_configured_notes_ref<E: IdentityEnv>(
    env: &E,
    config: &ConfigSet,
) -> Result<NotesRef> {
    if let Some(value) = env.var("GIT_NOTES_REF") {
        return NotesRef::from_full(&value);
    }
    if let Some(value) = config.get("core.notesRef") {
        return NotesRef::from_full(&value);
    }
    Ok(NotesRef::commits())
}

/// Expansion behind [`NotesRef::parse`]; configured refs are used verbatim instead.
fn expand_notes_ref(short_or_full: &str) -> String {
    if short_or_full.starts_with("refs/notes/") {
        short_or_full.to_owned()
    } else if short_or_full.starts_with("notes/") {
        format!("refs/{short_or_full}")
    } else {
        format!("refs/notes/{short_or_full}")
    }
}

/// Path to the per-merge worktree directory under the repository git dir.
fn notes_merge_worktree_path(repo: &Repository) -> std::path::PathBuf {
    repo.git_dir().join(NOTES_MERGE_WORKTREE)
}

/// True when `NOTES_MERGE_WORKTREE` exists and holds at least one entry.
fn notes_merge_worktree_nonempty(worktree: &std::path::Path) -> bool {
    if !worktree.is_dir() {
        return false;
    }
    let Ok(entries) = fs::read_dir(worktree) else {
        return false;
    };
    entries.flatten().next().is_some()
}

/// Map annotated object → note blob OID for one notes tree (any fanout layout).
fn notes_tree_blob_by_object(
    repo: &Repository,
    tree_oid: &ObjectId,
) -> Result<HashMap<ObjectId, ObjectId>> {
    let mut flat = Vec::new();
    let mut listable_seen = 0usize;
    let mut truncated = false;
    collect_notes_tree_entries(
        repo,
        tree_oid,
        b"",
        &mut flat,
        None,
        &mut listable_seen,
        &mut truncated,
    )?;
    debug_assert!(!truncated);
    let mut map = HashMap::new();
    for entry in flat {
        let Some(hex) = note_object_name(&entry.path) else {
            continue;
        };
        map.insert(ObjectId::from_hex(&hex)?, entry.oid);
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

/// Bytes of the note blob `oid`.
///
/// # Errors
///
/// [`Error::NotesObjectKind`] when `oid` is not a blob, or object read failures.
pub(crate) fn read_blob_bytes(repo: &Repository, oid: &ObjectId) -> Result<Vec<u8>> {
    let obj = repo.odb().read(oid)?;
    if obj.kind != ObjectKind::Blob {
        return Err(Error::NotesObjectKind {
            oid: *oid,
            expected: ObjectKind::Blob,
            found: obj.kind,
        });
    }
    Ok(obj.data)
}

/// Join two note blobs with a blank line between them (Git's `concatenate` notes combiner).
fn combine_notes_concatenate(
    repo: &Repository,
    cur: Option<&ObjectId>,
    new_oid: Option<&ObjectId>,
) -> Result<ObjectId> {
    let new_data = match new_oid {
        Some(n) => {
            let obj = repo.odb().read(n)?;
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
            let obj = repo.odb().read(c)?;
            if obj.kind != ObjectKind::Blob || obj.data.is_empty() {
                Vec::new()
            } else {
                obj.data
            }
        }
        None => Vec::new(),
    };

    if cur_data.is_empty() {
        return repo.odb().write(ObjectKind::Blob, &new_data);
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
    repo.odb().write(ObjectKind::Blob, &buf)
}

fn note_blob_lines(data: &[u8]) -> Vec<String> {
    if data.is_empty() {
        return Vec::new();
    }
    let s = String::from_utf8_lossy(data);
    s.split('\n').map(|l| l.to_owned()).collect()
}

/// All lines from both blobs, de-duplicated and sorted (Git's `cat_sort_uniq` notes combiner).
fn combine_notes_cat_sort_uniq(
    repo: &Repository,
    cur: Option<&ObjectId>,
    new_oid: Option<&ObjectId>,
) -> Result<ObjectId> {
    let mut lines: Vec<String> = Vec::new();
    for oid in [cur, new_oid].into_iter().flatten() {
        let obj = repo.odb().read(oid)?;
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
    repo.odb().write(ObjectKind::Blob, buf.as_bytes())
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

/// Note blobs on each side of one conflicting note in a notes merge (`None`: no note).
struct NoteConflict {
    base: Option<ObjectId>,
    local: Option<ObjectId>,
    remote: Option<ObjectId>,
}

fn remove_entry(entries: &mut Vec<NotesTreeEntry>, obj_hex: &str) {
    entries.retain(|e| note_object_name(&e.path).as_deref() != Some(obj_hex));
}

/// Resolve one note changed on both sides; returns `true` when it is left as a conflict file.
#[expect(clippy::too_many_arguments)]
fn merge_one_note_change(
    repo: &Repository,
    obj: &ObjectId,
    sides: &NoteConflict,
    strategy: NotesMergeStrategy,
    local_ref: &str,
    remote_ref: &str,
    worktree: &std::path::Path,
    commit_msg: &mut String,
    entries: &mut Vec<NotesTreeEntry>,
    has_worktree: &mut bool,
) -> Result<bool> {
    let obj_hex = obj.to_hex();
    let combine = match strategy {
        NotesMergeStrategy::Ours => return Ok(false),
        NotesMergeStrategy::Theirs => {
            match sides.remote {
                Some(r) => upsert_note_entry(entries, &obj_hex, r),
                None => remove_entry(entries, &obj_hex),
            }
            return Ok(false);
        }
        NotesMergeStrategy::Union => combine_notes_concatenate,
        NotesMergeStrategy::CatSortUniq => combine_notes_cat_sort_uniq,
        NotesMergeStrategy::Manual => {
            let data = match (&sides.local, &sides.remote) {
                (None, None) => {
                    remove_entry(entries, &obj_hex);
                    return Ok(false);
                }
                (None, Some(r)) => read_blob_bytes(repo, r)?,
                (Some(l), None) => read_blob_bytes(repo, l)?,
                (Some(l), Some(r)) => merge_note_blobs_conflict_markers(
                    repo,
                    sides.base.as_ref(),
                    l,
                    r,
                    local_ref,
                    remote_ref,
                )?,
            };
            if !*has_worktree {
                if notes_merge_worktree_nonempty(worktree) {
                    return Err(Error::NotesMergeInProgress {
                        path: worktree.to_path_buf(),
                    });
                }
                fs::create_dir_all(worktree)?;
                *has_worktree = true;
            }
            if !commit_msg.contains("Conflicts:") {
                commit_msg.push_str("\n\nConflicts:\n");
            }
            commit_msg.push_str(&format!("\t{obj_hex}\n"));
            fs::write(worktree.join(&obj_hex), data)?;
            remove_entry(entries, &obj_hex);
            return Ok(true);
        }
    };
    match (&sides.local, &sides.remote) {
        (None, None) => remove_entry(entries, &obj_hex),
        (Some(_), None) => {}
        (local, Some(r)) => {
            let out = combine(repo, local.as_ref(), Some(r))?;
            upsert_note_entry(entries, &obj_hex, out);
        }
    }
    Ok(false)
}

/// Replace or insert the note entry for `hex` in a flat notes entry list.
pub fn upsert_note_entry(entries: &mut Vec<NotesTreeEntry>, hex: &str, blob: ObjectId) {
    remove_entry(entries, hex);
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

fn adopt_remote_note(entries: &mut Vec<NotesTreeEntry>, obj_hex: &str, remote: Option<ObjectId>) {
    match remote {
        Some(oid) => upsert_note_entry(entries, obj_hex, oid),
        None => remove_entry(entries, obj_hex),
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
        let Some(local) = p.local_change() else {
            adopt_remote_note(entries, &p.obj.to_hex(), p.remote_blob);
            continue;
        };
        let sides = NoteConflict {
            base: p.base_blob,
            local,
            remote: p.remote_blob,
        };
        if merge_one_note_change(
            repo,
            &p.obj,
            &sides,
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
    let obj = repo.odb().read(commit_oid)?;
    if obj.kind != ObjectKind::Commit {
        return Err(Error::NotesObjectKind {
            oid: *commit_oid,
            expected: ObjectKind::Commit,
            found: obj.kind,
        });
    }
    Ok(parse_commit(&obj.data)?.tree)
}

fn resolve_notes_commit_optional(repo: &Repository, notes_ref: &str) -> Result<Option<ObjectId>> {
    let Some(oid) = notes_ref_tip(repo, notes_ref)? else {
        return Ok(None);
    };
    let kind = repo.odb().read(&oid)?.kind;
    if kind != ObjectKind::Commit {
        return Err(Error::NotesRefNotCommit {
            notes_ref: notes_ref.to_owned(),
            found: kind,
        });
    }
    Ok(Some(oid))
}

/// Whether a notes merge commit moves the notes ref.
#[derive(Clone, Copy)]
enum NotesRefUpdate {
    /// Store the commit in the ODB only (manual merge conflicts keep the live ref at the local tip).
    Leave,
    /// Move the ref, which must still point at this tip (`None`: must not exist).
    From(Option<ObjectId>),
}

/// Write a notes merge commit with `parents`, then apply `update` to `notes_ref`.
fn write_notes_commit_with_parents(
    repo: &Repository,
    notes_ref: &str,
    entries: &[NotesTreeEntry],
    message: &str,
    parents: &[ObjectId],
    update: NotesRefUpdate,
) -> Result<ObjectId> {
    let (author, committer) = wall_clock_notes_idents(repo)?;
    let commit = NotesCommit {
        message,
        author: &author,
        committer: &committer,
    };
    let oid = write_notes_commit_object(repo, entries, &commit, parents.to_vec())?;
    if let NotesRefUpdate::From(old) = update {
        update_notes_ref(repo, notes_ref, old, oid, &commit)?;
    }
    Ok(oid)
}

/// Merge two notes refs and return the resulting notes commit, or a conflict commit oid.
///
/// # Errors
///
/// - [`Error::NotesMergeBothEmpty`] when neither ref has a notes commit.
/// - [`Error::NotesRefNotCommit`] when either ref points at something other than a commit.
/// - [`Error::NotesMergeInProgress`] when [`NotesMergeStrategy::Manual`] finds conflict files
///   left by an earlier merge.
/// - Ref store, object, and merge-base failures, or a failure while combining blobs.
pub fn notes_merge_inner(
    repo: &Repository,
    local_ref: &str,
    remote_ref: &str,
    strategy: NotesMergeStrategy,
) -> Result<std::result::Result<ObjectId, ObjectId>> {
    let local_commit = resolve_notes_commit_optional(repo, local_ref)?;
    let remote_commit = resolve_notes_commit_optional(repo, remote_ref)?;
    match (local_commit, remote_commit) {
        (None, None) => Err(Error::NotesMergeBothEmpty {
            local: local_ref.to_owned(),
            remote: remote_ref.to_owned(),
        }),
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
            let mut entries = Vec::new();
            let mut listable_seen = 0usize;
            let mut truncated = false;
            collect_notes_tree_entries(
                repo,
                &local_tree,
                b"",
                &mut entries,
                None,
                &mut listable_seen,
                &mut truncated,
            )?;
            debug_assert!(!truncated);
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
                    NotesRefUpdate::Leave,
                )?;
                return Ok(Err(partial));
            }
            let new_oid = write_notes_commit_with_parents(
                repo,
                local_ref,
                &entries,
                &commit_msg,
                &merge_parents,
                NotesRefUpdate::From(Some(local_oid)),
            )?;
            Ok(Ok(new_oid))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::RepositoryOptions;
    use crate::objects::ObjectKind;
    use crate::repo::Repository;
    use std::process::Command;
    use tempfile::TempDir;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn open_repo(dir: &std::path::Path) -> Repository {
        git(dir, &["init", "-q", "-b", "main", "."]);
        std::fs::write(dir.join("f"), b"x\n").unwrap();
        git(dir, &["add", "f"]);
        git(dir, &["commit", "-qm", "init"]);
        Repository::open(
            &dir.join(".git"),
            &RepositoryOptions::empty().with_work_tree(dir),
        )
        .expect("open")
    }

    #[test]
    fn note_object_name_parses_flat_and_fanout_paths() {
        let full = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(note_object_name(full.as_bytes()), Some(full.to_owned()));
        assert_eq!(
            note_object_name(b"01/23/456789abcdef0123456789abcdef01234567"),
            Some(full.to_owned())
        );
        assert!(note_object_name(b"not-a-note").is_none());
    }

    #[test]
    fn from_full_rejects_whitespace_padded_ref() {
        let err = NotesRef::from_full(" refs/notes/spaced ").expect_err("padded");
        assert!(matches!(err, Error::InvalidNotesRef(_)));
        let err = NotesRef::from_full("refs/notes/spaced ").expect_err("trailing");
        assert!(matches!(err, Error::InvalidNotesRef(_)));
        let ok = NotesRef::from_full("refs/notes/spaced").expect("valid");
        assert_eq!(ok.as_str(), "refs/notes/spaced");
    }

    #[test]
    fn resolve_configured_notes_ref_rejects_empty_git_notes_ref() {
        use crate::config::ConfigSet;
        use std::collections::HashMap;
        use std::ffi::OsString;

        struct MapEnv(HashMap<String, String>);
        impl crate::ident_resolve::IdentityEnv for MapEnv {
            fn var(&self, key: &str) -> Option<String> {
                self.0.get(key).cloned()
            }
            fn var_os(&self, key: &str) -> Option<OsString> {
                self.var(key).map(OsString::from)
            }
        }

        let env = MapEnv(HashMap::from([("GIT_NOTES_REF".to_owned(), String::new())]));
        let err = resolve_configured_notes_ref(&env, &ConfigSet::default())
            .expect_err("empty GIT_NOTES_REF");
        assert!(matches!(err, Error::InvalidNotesRef(_)));
    }

    #[test]
    fn expand_notes_ref_and_parse() {
        assert_eq!(expand_notes_ref("commits"), "refs/notes/commits");
        assert_eq!(expand_notes_ref("notes/foo"), "refs/notes/foo");
        assert_eq!(expand_notes_ref("refs/notes/bar"), "refs/notes/bar");
        assert_eq!(NotesRef::parse("review").as_str(), "refs/notes/review");
    }

    #[test]
    fn notes_merge_worktree_nonempty_detects_files() {
        let tmp = TempDir::new().unwrap();
        let wt = tmp.path().join("wt");
        assert!(!notes_merge_worktree_nonempty(&wt));
        std::fs::create_dir_all(&wt).unwrap();
        assert!(!notes_merge_worktree_nonempty(&wt));
        std::fs::write(wt.join("x"), b"").unwrap();
        assert!(notes_merge_worktree_nonempty(&wt));
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn combine_notes_helpers_and_upsert() {
        let tmp = TempDir::new().unwrap();
        let repo = open_repo(tmp.path());
        let a = repo.odb().write(ObjectKind::Blob, b"line1\n").unwrap();
        let b = repo.odb().write(ObjectKind::Blob, b"line2\n").unwrap();
        let cat = combine_notes_concatenate(&repo, Some(&a), Some(&b)).unwrap();
        let merged = repo.odb().read(&cat).unwrap();
        assert!(String::from_utf8_lossy(&merged.data).contains("line1"));
        let _uniq = combine_notes_cat_sort_uniq(&repo, Some(&a), Some(&b)).unwrap();

        let hex = "0123456789abcdef0123456789abcdef01234567";
        let mut entries = Vec::new();
        upsert_note_entry(&mut entries, hex, a);
        upsert_note_entry(&mut entries, hex, b);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].oid, b);
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn read_and_write_notes_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let repo = open_repo(tmp.path());
        let head = Command::new("git")
            .current_dir(tmp.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let commit = ObjectId::from_hex(std::str::from_utf8(&head.stdout).unwrap().trim()).unwrap();
        let blob = repo.odb().write(ObjectKind::Blob, b"hello\n").unwrap();
        upsert_note_entry(&mut vec![], &commit.to_hex(), blob);
        let entries = vec![NotesTreeEntry {
            mode: 0o100644,
            path: commit.to_hex().into_bytes(),
            oid: blob,
        }];
        write_notes_commit(&repo, "refs/notes/commits", &entries, "note").unwrap();
        let read_back = read_notes_tree(&repo, "refs/notes/commits").unwrap();
        assert_eq!(read_back.len(), 1);
        assert_eq!(read_back[0].oid, blob);
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn notes_merge_ours_keeps_local_note() {
        let tmp = TempDir::new().unwrap();
        let repo = open_repo(tmp.path());
        let commit = ObjectId::from_hex(
            std::str::from_utf8(
                &Command::new("git")
                    .current_dir(tmp.path())
                    .args(["rev-parse", "HEAD"])
                    .output()
                    .unwrap()
                    .stdout,
            )
            .unwrap()
            .trim(),
        )
        .unwrap();
        git(
            tmp.path(),
            &["notes", "add", "-m", "base", &commit.to_hex()],
        );
        let base = crate::refs::resolve_ref(repo.git_dir(), "refs/notes/commits").unwrap();
        git(
            tmp.path(),
            &["notes", "add", "-f", "-m", "local", &commit.to_hex()],
        );
        let local = crate::refs::resolve_ref(repo.git_dir(), "refs/notes/commits").unwrap();
        git(
            tmp.path(),
            &["update-ref", "refs/notes/remote", &base.to_hex()],
        );
        git(
            tmp.path(),
            &[
                "notes",
                "--ref=refs/notes/remote",
                "add",
                "-f",
                "-m",
                "remote",
                &commit.to_hex(),
            ],
        );
        git(
            tmp.path(),
            &["update-ref", "refs/notes/commits", &local.to_hex()],
        );
        notes_merge_inner(
            &repo,
            "refs/notes/commits",
            "refs/notes/remote",
            NotesMergeStrategy::Ours,
        )
        .expect("merge")
        .expect("ok");
        let show = Command::new("git")
            .current_dir(tmp.path())
            .args(["notes", "show", &commit.to_hex()])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&show.stdout).trim(), "local");
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn notes_merge_manual_leaves_conflict_commit() {
        let tmp = TempDir::new().unwrap();
        let repo = open_repo(tmp.path());
        let commit = ObjectId::from_hex(
            std::str::from_utf8(
                &Command::new("git")
                    .current_dir(tmp.path())
                    .args(["rev-parse", "HEAD"])
                    .output()
                    .unwrap()
                    .stdout,
            )
            .unwrap()
            .trim(),
        )
        .unwrap();
        git(
            tmp.path(),
            &["notes", "add", "-m", "base", &commit.to_hex()],
        );
        let base = crate::refs::resolve_ref(repo.git_dir(), "refs/notes/commits").unwrap();
        git(
            tmp.path(),
            &["notes", "add", "-f", "-m", "local", &commit.to_hex()],
        );
        let local = crate::refs::resolve_ref(repo.git_dir(), "refs/notes/commits").unwrap();
        git(
            tmp.path(),
            &["update-ref", "refs/notes/remote", &base.to_hex()],
        );
        git(
            tmp.path(),
            &[
                "notes",
                "--ref=refs/notes/remote",
                "add",
                "-f",
                "-m",
                "remote",
                &commit.to_hex(),
            ],
        );
        git(
            tmp.path(),
            &["update-ref", "refs/notes/commits", &local.to_hex()],
        );
        let result = notes_merge_inner(
            &repo,
            "refs/notes/commits",
            "refs/notes/remote",
            NotesMergeStrategy::Manual,
        )
        .expect("merge");
        let partial = match result {
            Err(partial) => partial,
            Ok(_) => panic!("manual merge must stop with conflicts"),
        };
        assert_ne!(
            partial, local,
            "partial merge commit must differ from the pre-merge tip"
        );
        assert_eq!(
            crate::refs::resolve_ref(repo.git_dir(), "refs/notes/commits").unwrap(),
            local,
            "manual merge conflicts must leave the live notes ref at the local tip"
        );
        assert!(notes_merge_worktree_nonempty(&notes_merge_worktree_path(
            &repo
        )));

        let err = notes_merge_inner(
            &repo,
            "refs/notes/commits",
            "refs/notes/remote",
            NotesMergeStrategy::Manual,
        )
        .expect_err("earlier conflicts are still unresolved");
        assert!(matches!(
            err,
            Error::NotesMergeInProgress { ref path } if *path == notes_merge_worktree_path(&repo)
        ));
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn read_notes_tree_missing_ref_is_empty() {
        let tmp = TempDir::new().unwrap();
        let repo = open_repo(tmp.path());
        assert!(read_notes_tree(&repo, "refs/notes/commits")
            .unwrap()
            .is_empty());
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn notes_merge_empty_remote_adopts_remote() {
        let tmp = TempDir::new().unwrap();
        let repo = open_repo(tmp.path());
        let local = write_notes_commit_with_parents(
            &repo,
            "refs/notes/commits",
            &[],
            "local only",
            &[],
            NotesRefUpdate::From(None),
        )
        .unwrap();
        let result = notes_merge_inner(
            &repo,
            "refs/notes/commits",
            "refs/notes/missing",
            NotesMergeStrategy::Theirs,
        )
        .expect("merge");
        assert_eq!(result.expect("ok"), local);
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn notes_merge_of_two_empty_refs_is_typed() {
        let tmp = TempDir::new().unwrap();
        let repo = open_repo(tmp.path());
        let err = notes_merge_inner(
            &repo,
            "refs/notes/commits",
            "refs/notes/missing",
            NotesMergeStrategy::Union,
        )
        .expect_err("nothing to merge");
        assert!(matches!(
            err,
            Error::NotesMergeBothEmpty { ref local, ref remote }
                if local == "refs/notes/commits" && remote == "refs/notes/missing"
        ));
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn read_notes_tree_rejects_blob_tip() {
        let tmp = TempDir::new().unwrap();
        let repo = open_repo(tmp.path());
        let blob = repo.odb().write(ObjectKind::Blob, b"not notes\n").unwrap();
        git(
            tmp.path(),
            &["update-ref", "refs/notes/blob", &blob.to_hex()],
        );
        let err = read_notes_tree(&repo, "refs/notes/blob").expect_err("blob tip");
        assert!(matches!(
            err,
            Error::NotesRefNotCommit { ref notes_ref, found: ObjectKind::Blob }
                if notes_ref == "refs/notes/blob"
        ));
        let err = notes_merge_inner(
            &repo,
            "refs/notes/commits",
            "refs/notes/blob",
            NotesMergeStrategy::Ours,
        )
        .expect_err("blob remote");
        assert!(matches!(
            err,
            Error::NotesRefNotCommit {
                found: ObjectKind::Blob,
                ..
            }
        ));
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn write_notes_commit_goes_through_injected_ref_store() {
        let tmp = TempDir::new().unwrap();
        let repo = open_repo(tmp.path()).with_ref_store(std::sync::Arc::new(
            crate::refs::store::MemoryRefStore::new(),
        ));
        let blob = repo.odb().write(ObjectKind::Blob, b"in memory\n").unwrap();
        let mut entries = Vec::new();
        upsert_note_entry(&mut entries, &"ab".repeat(20), blob);
        write_notes_commit(&repo, "refs/notes/commits", &entries, "first").unwrap();
        let first = repo.refs().resolve("refs/notes/commits").unwrap();
        write_notes_commit(&repo, "refs/notes/commits", &[], "second").unwrap();
        let second = repo.refs().resolve("refs/notes/commits").unwrap();

        let commit = parse_commit(&repo.odb().read(&second).unwrap().data).unwrap();
        assert_eq!(commit.parents, vec![first]);
        assert!(read_notes_tree(&repo, "refs/notes/commits")
            .unwrap()
            .is_empty());
        assert!(
            !tmp.path().join(".git/refs/notes/commits").exists(),
            "notes ref must live in the injected store, not on disk"
        );
    }

    #[test]
    fn local_change_distinguishes_unchanged_deleted_and_set() {
        let base = ObjectId::from_hex(&"11".repeat(20)).unwrap();
        let other = ObjectId::from_hex(&"22".repeat(20)).unwrap();
        let pair = |local| NotesMergePair {
            obj: other,
            base_blob: Some(base),
            remote_blob: None,
            local,
        };
        assert_eq!(pair(LocalNoteState::Unset).local_change(), None);
        assert_eq!(pair(LocalNoteState::Present(base)).local_change(), None);
        assert_eq!(
            pair(LocalNoteState::Present(other)).local_change(),
            Some(Some(other))
        );
        assert_eq!(pair(LocalNoteState::Deleted).local_change(), Some(None));
    }
}
