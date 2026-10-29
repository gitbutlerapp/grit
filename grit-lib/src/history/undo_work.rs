//! Index and work tree state that [`super::undo::undo_last_rewrite`] puts back.
//!
//! Fixup and absorb fold staged (or selected) edits into history. Reverting only their refs and
//! `HEAD` tree would throw those edits away, so the apply stores, next to the undo record, the
//! pre-command index entry and work tree file of every path it consumed (`restore`) plus a digest
//! of the same paths right after the command (`expect`). Undo refuses when a path no longer
//! matches `expect`, then installs `restore` after the inverse checkout and records the reverse
//! pair so a second undo redoes the rewrite.
//!
//! The sidecar lives at `.git/grit/history-last-work` and is written or removed together with
//! `.git/grit/history-last`.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::index::{entry_from_metadata, IndexEntry, MODE_GITLINK, MODE_TREE};
use crate::lockfile::{LockFile, LockWait};
use crate::objects::HashAlgo;
use crate::objects::{ObjectId, ObjectKind};
use crate::porcelain::index_from_tree::refresh_stage0_index_stats_for_paths;
use crate::repo::Repository;

use super::apply::{
    read_worktree_path_snapshot, restore_worktree_path, WorktreePathKind, WorktreePathSnapshot,
};
use super::error::HistoryError;

const FORMAT_VERSION: u32 = 1;

/// Stage-0 index entry contents for one path.
///
/// A gitlink keeps only its mode and commit id: the commit lives in the submodule's object
/// store, so `data` stays empty and is never read or written as a blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexBlob {
    mode: u32,
    oid: ObjectId,
    data: Vec<u8>,
}

impl IndexBlob {
    fn is_gitlink(&self) -> bool {
        self.mode == MODE_GITLINK
    }
}

/// Work tree placeholder for a gitlink path: undo never touches a submodule's directory.
fn submodule_worktree() -> WorktreePathSnapshot {
    WorktreePathSnapshot::from_parts(WorktreePathKind::Directory, None, 0)
}

/// Index entry and work tree file for one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PathState {
    path: String,
    index: Option<IndexBlob>,
    worktree: WorktreePathSnapshot,
}

/// Comparable fingerprint of a [`PathState`]: modes, kinds and blob ids instead of bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PathDigest {
    index: Option<(u32, ObjectId)>,
    worktree: WorktreeDigest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorktreeDigest {
    Absent,
    Directory,
    File { executable: bool, oid: ObjectId },
    Symlink { oid: ObjectId },
}

/// One path recorded in the sidecar.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UndoWorkPath {
    restore: PathState,
    expect: PathDigest,
}

/// Index and work tree state an undo installs, and the live state it may replace.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct UndoWork {
    paths: Vec<UndoWorkPath>,
}

/// Read the current index entry and work tree file for each of `paths`.
///
/// A gitlink entry is recorded by mode and commit id; its submodule directory is not read.
pub(crate) fn capture_path_states(
    repo: &Repository,
    paths: &[String],
) -> Result<Vec<PathState>, HistoryError> {
    let Some(work_tree) = repo.work_tree() else {
        return Ok(Vec::new());
    };
    let index = repo.load_index().map_err(apply_err)?;
    let mut states = Vec::with_capacity(paths.len());
    for path in paths {
        let entry = index.get(path.as_bytes(), 0);
        let index_blob = match entry {
            Some(e) if e.mode == MODE_GITLINK => Some(IndexBlob {
                mode: e.mode,
                oid: e.oid,
                data: Vec::new(),
            }),
            Some(e) => Some(IndexBlob {
                mode: e.mode,
                oid: e.oid,
                data: repo.odb().read(&e.oid).map_err(apply_err)?.data,
            }),
            None => None,
        };
        let worktree = if index_blob.as_ref().is_some_and(IndexBlob::is_gitlink) {
            submodule_worktree()
        } else {
            read_worktree_path_snapshot(work_tree, path).map_err(io_err)?
        };
        states.push(PathState {
            path: path.clone(),
            index: index_blob,
            worktree,
        });
    }
    Ok(states)
}

/// Build the clean state of `paths` in `tree`: index entry and work tree file both match it.
pub(crate) fn tree_path_states(
    repo: &Repository,
    tree: ObjectId,
    paths: &[String],
) -> Result<Vec<PathState>, HistoryError> {
    paths
        .iter()
        .map(|path| {
            let entry = tree_blob_at_path(repo, tree, path)?;
            let worktree = match &entry {
                Some(blob) if blob.is_gitlink() => submodule_worktree(),
                Some(blob) => WorktreePathSnapshot::file(blob.mode, blob.data.clone()),
                None => WorktreePathSnapshot::absent(),
            };
            Ok(PathState {
                path: path.clone(),
                index: entry,
                worktree,
            })
        })
        .collect()
}

/// Write each state's index entry and work tree file.
///
/// An entry whose file was written with exactly the blob's bytes takes the file's stat data.
/// Any other file (CRLF from `core.autocrlf`, smudge filters, or a genuinely different
/// unstaged layer) is checked through the repository's clean conversion: when it cleans to the
/// restored blob the entry takes its stat data, otherwise the entry keeps an unset stat cache
/// (size and mtime zero) so Git and grit both rehash the file instead of trusting the stat.
pub(crate) fn install_path_states(
    repo: &Repository,
    states: &[PathState],
) -> Result<(), HistoryError> {
    let Some(work_tree) = repo.work_tree() else {
        return Ok(());
    };
    let mut index = repo.load_index().map_err(apply_err)?;
    let mut unstatted = BTreeSet::new();
    for state in states {
        let abs = work_tree.join(&state.path);
        install_worktree(&abs, &state.worktree).map_err(io_err)?;
        let key = state.path.as_bytes();
        let Some(blob) = &state.index else {
            index.remove_path_all_stages(key);
            continue;
        };
        let oid = if blob.is_gitlink() {
            blob.oid
        } else {
            repo.odb()
                .write(ObjectKind::Blob, &blob.data)
                .map_err(apply_err)?
        };
        let entry = if state.worktree.content_matches(blob.mode, &blob.data) {
            let meta = fs::symlink_metadata(&abs).map_err(io_err)?;
            entry_from_metadata(&meta, key, oid, blob.mode)
        } else {
            unstatted.insert(state.path.as_str());
            unstatted_entry(key, oid, blob.mode)
        };
        index.stage_file(entry);
    }
    index.sort();
    refresh_stage0_index_stats_for_paths(repo, work_tree, &mut index, &unstatted)
        .map_err(apply_err)?;
    repo.write_index(&mut index).map_err(apply_err)
}

/// The `target` states that `live` does not already hold (same index entry and file content).
pub(crate) fn states_not_in_place(target: Vec<PathState>, live: &[PathState]) -> Vec<PathState> {
    let live: HashMap<&str, &PathState> = live.iter().map(|s| (s.path.as_str(), s)).collect();
    target
        .into_iter()
        .filter(|want| {
            !live
                .get(want.path.as_str())
                .is_some_and(|have| have.same_content(want))
        })
        .collect()
}

impl PathState {
    fn same_content(&self, other: &Self) -> bool {
        let index = |s: &Self| s.index.as_ref().map(|b| (b.mode, b.oid));
        let (a, b) = (&self.worktree, &other.worktree);
        index(self) == index(other)
            && a.kind == b.kind
            && a.bytes == b.bytes
            && a.is_executable() == b.is_executable()
    }

    fn digest(&self, algo: HashAlgo) -> Result<PathDigest, HistoryError> {
        let index = self.index.as_ref().map(|blob| (blob.mode, blob.oid));
        let wt = &self.worktree;
        let worktree = match (wt.kind, wt.bytes.as_deref()) {
            (WorktreePathKind::Regular, Some(bytes)) => WorktreeDigest::File {
                executable: wt.is_executable(),
                oid: hash_blob(algo, bytes)?,
            },
            (WorktreePathKind::Symlink, Some(bytes)) => WorktreeDigest::Symlink {
                oid: hash_blob(algo, bytes)?,
            },
            (WorktreePathKind::Directory, _) => WorktreeDigest::Directory,
            _ => WorktreeDigest::Absent,
        };
        Ok(PathDigest { index, worktree })
    }
}

impl UndoWork {
    /// Pair each pre-command `restore` state with a digest of the same path as it is now.
    pub(crate) fn after_command(
        repo: &Repository,
        restore: Vec<PathState>,
    ) -> Result<Self, HistoryError> {
        let paths: Vec<String> = restore.iter().map(|s| s.path.clone()).collect();
        let now = capture_path_states(repo, &paths)?;
        Self::pair(repo.odb().hash_algo(), restore, &now)
    }

    /// Pair each `restore` state with the digest of the state `now` holds for the same path
    /// (the restore state itself when `now` has none).
    pub(crate) fn pair(
        algo: HashAlgo,
        restore: Vec<PathState>,
        now: &[PathState],
    ) -> Result<Self, HistoryError> {
        let now: HashMap<&str, &PathState> = now.iter().map(|s| (s.path.as_str(), s)).collect();
        let paths = restore
            .into_iter()
            .map(|restore| {
                let expect = now
                    .get(restore.path.as_str())
                    .copied()
                    .unwrap_or(&restore)
                    .digest(algo)?;
                Ok(UndoWorkPath { restore, expect })
            })
            .collect::<Result<_, HistoryError>>()?;
        Ok(Self { paths })
    }

    /// Paths this record restores, in record order.
    pub(crate) fn paths(&self) -> Vec<String> {
        self.paths.iter().map(|p| p.restore.path.clone()).collect()
    }

    /// States to install after the inverse checkout.
    pub(crate) fn restore_states(&self) -> Vec<PathState> {
        self.paths.iter().map(|p| p.restore.clone()).collect()
    }

    /// Live state of the recorded paths, provided each still matches the state the command left.
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError::LocalChangesWouldBeOverwritten`] naming every recorded path whose
    /// index entry or work tree file changed since the command.
    pub(crate) fn unchanged_live_states(
        &self,
        repo: &Repository,
    ) -> Result<Vec<PathState>, HistoryError> {
        let algo = repo.odb().hash_algo();
        let live = capture_path_states(repo, &self.paths())?;
        let by_path: HashMap<&str, &PathState> =
            live.iter().map(|s| (s.path.as_str(), s)).collect();
        let mut changed = Vec::new();
        for recorded in &self.paths {
            let matches = match by_path.get(recorded.restore.path.as_str()) {
                Some(state) => state.digest(algo)? == recorded.expect,
                None => false,
            };
            if !matches {
                changed.push(recorded.restore.path.clone());
            }
        }
        if !changed.is_empty() {
            return Err(HistoryError::LocalChangesWouldBeOverwritten { paths: changed });
        }
        Ok(live)
    }
}

fn sidecar_path(repo: &Repository) -> PathBuf {
    repo.git_dir().join("grit/history-last-work")
}

/// Sidecar bytes written to a lock file, published by [`Self::commit`].
pub(crate) struct StagedUndoWork {
    lock: LockFile,
    target: PathBuf,
}

impl StagedUndoWork {
    /// Write `work` to the sidecar's lock file without publishing it.
    ///
    /// Called by [`super::last`] while it holds the `history-last` lock.
    pub(crate) fn stage(repo: &Repository, work: &UndoWork) -> Result<Self, HistoryError> {
        let target = sidecar_path(repo);
        let mut lock = LockFile::acquire(&target.with_extension("lock"), LockWait::Immediately)
            .map_err(write_err)?;
        lock.write_all(&serialize(work)).map_err(write_err)?;
        Ok(Self { lock, target })
    }

    /// Rename the staged bytes over the sidecar.
    pub(crate) fn commit(self) -> Result<(), HistoryError> {
        self.lock.commit_to(&self.target).map_err(write_err)
    }
}

/// Delete the sidecar so it cannot be paired with a newer record.
pub(crate) fn remove_undo_work(repo: &Repository) -> Result<(), HistoryError> {
    match fs::remove_file(sidecar_path(repo)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(write_err(e)),
        _ => Ok(()),
    }
}

fn write_err(err: impl std::fmt::Display) -> HistoryError {
    HistoryError::UndoRecordWrite {
        message: err.to_string(),
    }
}

/// Read the sidecar when present.
///
/// # Errors
///
/// Returns [`HistoryError::UndoRecordRead`] when the file exists but is invalid.
pub(crate) fn read_undo_work(repo: &Repository) -> Result<Option<UndoWork>, HistoryError> {
    match fs::read(sidecar_path(repo)) {
        Ok(bytes) => parse(&bytes).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(HistoryError::UndoRecordRead {
            message: e.to_string(),
        }),
    }
}

fn install_worktree(abs: &Path, state: &WorktreePathSnapshot) -> std::io::Result<()> {
    match state.kind {
        WorktreePathKind::Absent => match fs::symlink_metadata(abs) {
            Ok(meta) if !meta.is_dir() => fs::remove_file(abs),
            _ => Ok(()),
        },
        WorktreePathKind::Directory => Ok(()),
        WorktreePathKind::Regular | WorktreePathKind::Symlink => restore_worktree_path(abs, state),
    }
}

/// Entry with an unset stat cache ([`IndexEntry::stat_cache_unset`]), which forces a rehash.
fn unstatted_entry(key: &[u8], oid: ObjectId, mode: u32) -> IndexEntry {
    IndexEntry {
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
        oid,
        flags: key.len().min(0xfff) as u16,
        flags_extended: None,
        path: key.to_vec(),
        base_index_pos: 0,
    }
}

fn tree_blob_at_path(
    repo: &Repository,
    tree: ObjectId,
    path: &str,
) -> Result<Option<IndexBlob>, HistoryError> {
    let odb = repo.odb();
    let Some(entry) = odb
        .tree_entry_at_path(&tree, path.as_bytes())
        .map_err(apply_err)?
        .filter(|entry| entry.mode != MODE_TREE)
    else {
        return Ok(None);
    };
    let data = if entry.mode == MODE_GITLINK {
        Vec::new()
    } else {
        odb.read(&entry.oid).map_err(apply_err)?.data
    };
    Ok(Some(IndexBlob {
        mode: entry.mode,
        oid: entry.oid,
        data,
    }))
}

fn hash_blob(algo: HashAlgo, data: &[u8]) -> Result<ObjectId, HistoryError> {
    algo.hash_object(ObjectKind::Blob, data).map_err(apply_err)
}

fn apply_err(err: impl std::fmt::Display) -> HistoryError {
    HistoryError::WorktreeApply {
        message: err.to_string(),
    }
}

fn io_err(err: std::io::Error) -> HistoryError {
    apply_err(err)
}

/// Text header lines with length-prefixed binary payloads:
///
/// ```text
/// version 1
/// path <len>\n<path>\n
/// restore-index <mode> <oid> <len>\n<bytes>\n   | restore-index none
/// restore-worktree <kind> <mode> <len>\n<bytes>\n   | restore-worktree <kind> <mode> none
/// expect-index <mode> <oid>               | expect-index none
/// expect-worktree absent | directory | file <0|1> <oid> | symlink <oid>
/// ```
fn serialize(work: &UndoWork) -> Vec<u8> {
    let mut out = format!("version {FORMAT_VERSION}\n").into_bytes();
    for p in &work.paths {
        let r = &p.restore;
        push_payload(&mut out, "path", r.path.as_bytes());
        match &r.index {
            Some(blob) => push_payload(
                &mut out,
                &format!("restore-index {:o} {}", blob.mode, blob.oid.to_hex()),
                &blob.data,
            ),
            None => out.extend_from_slice(b"restore-index none\n"),
        }
        let kind = kind_name(r.worktree.kind);
        let mode = r.worktree.unix_mode();
        match &r.worktree.bytes {
            Some(bytes) => push_payload(
                &mut out,
                &format!("restore-worktree {kind} {mode:o}"),
                bytes,
            ),
            None => {
                out.extend_from_slice(format!("restore-worktree {kind} {mode:o} none\n").as_bytes())
            }
        }
        match &p.expect.index {
            Some((mode, oid)) => out
                .extend_from_slice(format!("expect-index {mode:o} {}\n", oid.to_hex()).as_bytes()),
            None => out.extend_from_slice(b"expect-index none\n"),
        }
        let line = match &p.expect.worktree {
            WorktreeDigest::Absent => "expect-worktree absent\n".to_owned(),
            WorktreeDigest::Directory => "expect-worktree directory\n".to_owned(),
            WorktreeDigest::File { executable, oid } => format!(
                "expect-worktree file {} {}\n",
                u8::from(*executable),
                oid.to_hex()
            ),
            WorktreeDigest::Symlink { oid } => {
                format!("expect-worktree symlink {}\n", oid.to_hex())
            }
        };
        out.extend_from_slice(line.as_bytes());
    }
    out
}

fn push_payload(out: &mut Vec<u8>, header: &str, bytes: &[u8]) {
    out.extend_from_slice(format!("{header} {}\n", bytes.len()).as_bytes());
    out.extend_from_slice(bytes);
    out.push(b'\n');
}

fn kind_name(kind: WorktreePathKind) -> &'static str {
    match kind {
        WorktreePathKind::Absent => "absent",
        WorktreePathKind::Directory => "directory",
        WorktreePathKind::Regular => "file",
        WorktreePathKind::Symlink => "symlink",
    }
}

fn parse_kind(name: &str) -> Option<WorktreePathKind> {
    Some(match name {
        "absent" => WorktreePathKind::Absent,
        "directory" => WorktreePathKind::Directory,
        "file" => WorktreePathKind::Regular,
        "symlink" => WorktreePathKind::Symlink,
        _ => return None,
    })
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn at_end(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    fn line(&mut self) -> Result<Vec<&'a str>, HistoryError> {
        let rest = &self.bytes[self.pos..];
        let end = rest
            .iter()
            .position(|&b| b == b'\n')
            .ok_or_else(|| parse_err("truncated header line"))?;
        self.pos += end + 1;
        let text = std::str::from_utf8(&rest[..end]).map_err(|e| parse_err(&e.to_string()))?;
        Ok(text.split(' ').collect())
    }

    fn payload(&mut self, len: &str) -> Result<Vec<u8>, HistoryError> {
        let len: usize = len.parse().map_err(|_| parse_err("bad payload length"))?;
        let end = self
            .pos
            .checked_add(len)
            .filter(|&end| end < self.bytes.len() && self.bytes[end] == b'\n')
            .ok_or_else(|| parse_err("truncated payload"))?;
        let data = self.bytes[self.pos..end].to_vec();
        self.pos = end + 1;
        Ok(data)
    }

    fn expect_keyword(&mut self, keyword: &str) -> Result<Vec<&'a str>, HistoryError> {
        let mut parts = self.line()?;
        if parts.first() != Some(&keyword) {
            return Err(parse_err(&format!("expected {keyword}")));
        }
        parts.remove(0);
        Ok(parts)
    }
}

fn parse(bytes: &[u8]) -> Result<UndoWork, HistoryError> {
    let mut cur = Cursor { bytes, pos: 0 };
    let version = cur.expect_keyword("version")?;
    if version != [FORMAT_VERSION.to_string().as_str()] {
        return Err(parse_err("unsupported version"));
    }
    let mut paths = Vec::new();
    while !cur.at_end() {
        let header = cur.expect_keyword("path")?;
        let path = String::from_utf8(cur.payload(field(&header, 0)?)?)
            .map_err(|e| parse_err(&e.to_string()))?;

        let header = cur.expect_keyword("restore-index")?;
        let index = match header.as_slice() {
            ["none"] => None,
            [mode, oid, len] => Some(IndexBlob {
                mode: parse_mode(mode)?,
                oid: parse_oid(oid)?,
                data: cur.payload(len)?,
            }),
            _ => return Err(parse_err("bad restore-index")),
        };

        let header = cur.expect_keyword("restore-worktree")?;
        let (kind, mode, bytes) = match header.as_slice() {
            [kind, mode, "none"] => (*kind, parse_mode(mode)?, None),
            [kind, mode, len] => (*kind, parse_mode(mode)?, Some(cur.payload(len)?)),
            _ => return Err(parse_err("bad restore-worktree")),
        };
        let kind = parse_kind(kind).ok_or_else(|| parse_err("bad worktree kind"))?;
        let worktree = WorktreePathSnapshot::from_parts(kind, bytes, mode);

        let header = cur.expect_keyword("expect-index")?;
        let expect_index = match header.as_slice() {
            ["none"] => None,
            [mode, oid] => Some((parse_mode(mode)?, parse_oid(oid)?)),
            _ => return Err(parse_err("bad expect-index")),
        };
        let header = cur.expect_keyword("expect-worktree")?;
        let expect_worktree = match header.as_slice() {
            ["absent"] => WorktreeDigest::Absent,
            ["directory"] => WorktreeDigest::Directory,
            ["file", exec, oid] => WorktreeDigest::File {
                executable: *exec == "1",
                oid: parse_oid(oid)?,
            },
            ["symlink", oid] => WorktreeDigest::Symlink {
                oid: parse_oid(oid)?,
            },
            _ => return Err(parse_err("bad expect-worktree")),
        };
        paths.push(UndoWorkPath {
            restore: PathState {
                path,
                index,
                worktree,
            },
            expect: PathDigest {
                index: expect_index,
                worktree: expect_worktree,
            },
        });
    }
    Ok(UndoWork { paths })
}

fn field<'a>(parts: &[&'a str], i: usize) -> Result<&'a str, HistoryError> {
    parts
        .get(i)
        .copied()
        .ok_or_else(|| parse_err("missing field"))
}

fn parse_mode(text: &str) -> Result<u32, HistoryError> {
    u32::from_str_radix(text, 8).map_err(|_| parse_err("bad mode"))
}

fn parse_oid(text: &str) -> Result<ObjectId, HistoryError> {
    ObjectId::from_hex(text).map_err(|e| parse_err(&e.to_string()))
}

fn parse_err(detail: &str) -> HistoryError {
    HistoryError::UndoRecordRead {
        message: format!("history-last-work: {detail}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(byte: u8) -> ObjectId {
        let mut bytes = [0u8; 20];
        bytes[19] = byte;
        ObjectId::from_bytes(&bytes).expect("oid")
    }

    #[test]
    fn sidecar_roundtrips_binary_payloads_and_every_kind() {
        let work = UndoWork {
            paths: vec![
                UndoWorkPath {
                    restore: PathState {
                        path: "dir/with space.txt".into(),
                        index: Some(IndexBlob {
                            mode: 0o100644,
                            oid: oid(9),
                            data: b"a\nb\n\0binary\n".to_vec(),
                        }),
                        worktree: WorktreePathSnapshot::file(0o100755, b"no newline".to_vec()),
                    },
                    expect: PathDigest {
                        index: Some((0o100644, oid(1))),
                        worktree: WorktreeDigest::File {
                            executable: true,
                            oid: oid(2),
                        },
                    },
                },
                UndoWorkPath {
                    restore: PathState {
                        path: "gone".into(),
                        index: None,
                        worktree: WorktreePathSnapshot::absent(),
                    },
                    expect: PathDigest {
                        index: None,
                        worktree: WorktreeDigest::Symlink { oid: oid(3) },
                    },
                },
                UndoWorkPath {
                    restore: PathState {
                        path: "link".into(),
                        index: Some(IndexBlob {
                            mode: 0o120000,
                            oid: oid(8),
                            data: b"target".to_vec(),
                        }),
                        worktree: WorktreePathSnapshot::file(0o120000, b"target".to_vec()),
                    },
                    expect: PathDigest {
                        index: None,
                        worktree: WorktreeDigest::Directory,
                    },
                },
                UndoWorkPath {
                    restore: PathState {
                        path: "sub".into(),
                        index: Some(IndexBlob {
                            mode: MODE_GITLINK,
                            oid: oid(7),
                            data: Vec::new(),
                        }),
                        worktree: submodule_worktree(),
                    },
                    expect: PathDigest {
                        index: Some((MODE_GITLINK, oid(6))),
                        worktree: WorktreeDigest::Directory,
                    },
                },
            ],
        };
        let parsed = parse(&serialize(&work)).expect("parse");
        assert_eq!(parsed, work);
        let empty = parse(&serialize(&UndoWork::default())).expect("parse empty");
        assert_eq!(empty, UndoWork::default());
    }

    #[test]
    fn sidecar_rejects_truncated_payload() {
        let work = UndoWork {
            paths: vec![UndoWorkPath {
                restore: PathState {
                    path: "a".into(),
                    index: None,
                    worktree: WorktreePathSnapshot::absent(),
                },
                expect: PathDigest {
                    index: None,
                    worktree: WorktreeDigest::Absent,
                },
            }],
        };
        let bytes = serialize(&work);
        let version_line = b"version 1\n".len();
        for cut in (1..bytes.len()).filter(|&cut| cut != version_line) {
            assert!(
                parse(&bytes[..cut]).is_err(),
                "prefix of {cut} bytes parsed"
            );
        }
    }
}
