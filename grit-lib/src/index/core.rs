//! Git index (staging area) reading and writing.
//!
//! The index file (`.git/index`) stores the current state of the staging area.
//! It uses a binary format with a 12-byte header, fixed-size index entries,
//! and optional extensions, followed by a trailing SHA-1 over the whole file.
//!
//! # Format version
//!
//! This implementation supports index versions 2 and 3. Requests for version 4
//! currently fall back to a non-compressed index on write because path
//! compression is not yet implemented.
//!
//! # References
//!
//! See `Documentation/technical/index-format.txt` in the Git source tree for
//! the authoritative format specification.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::hash::verify_trailer;

use crate::config::ConfigSet;
use crate::error::{Error, Result};
use crate::lockfile::{grit_lockfile_pid_enabled, pid_path_for_lock};
use crate::objects::{parse_tree, HashAlgo, ObjectId, ObjectKind, TreeEntry};
use crate::odb::Odb;
use crate::repo::Repository;
use crate::resolve_undo::{self, write_resolve_undo_payload, ResolveUndoRecord};
use crate::rev_parse;
use crate::untracked_cache;

fn gitattributes_index_path(path: &[u8]) -> bool {
    path == b".gitattributes" || path.ends_with(b"/.gitattributes")
}

/// File mode for a regular (non-executable) file.
pub const MODE_REGULAR: u32 = 0o100644;
/// File mode for an executable file.
pub const MODE_EXECUTABLE: u32 = 0o100755;
/// File mode for a symbolic link.
pub const MODE_SYMLINK: u32 = 0o120000;
/// File mode for a gitlink (submodule).
pub const MODE_GITLINK: u32 = 0o160000;
/// File mode for a directory (tree) entry — only used in tree objects, not index.
pub const MODE_TREE: u32 = 0o040000;

/// Git `match_stat_data` policy (`core.trustctime`, `core.checkstat`, `core.usenanosec`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatMatchPolicy {
    /// When true, compare ctime fields when `check_stat` is enabled.
    pub trust_ctime: bool,
    /// When true, compare owner, inode, device, and nanoseconds (if `use_nanosec`).
    pub check_stat: bool,
    /// When true with `check_stat`, compare sub-second mtime/ctime fractions, and judge an
    /// entry racily clean only when its mtime is at or after the index file's to the
    /// nanosecond rather than to the second.
    ///
    /// Defaults to [`Self::DEFAULT_USE_NANOSEC`].
    pub use_nanosec: bool,
}

impl Default for StatMatchPolicy {
    fn default() -> Self {
        Self {
            trust_ctime: true,
            check_stat: true,
            use_nanosec: Self::DEFAULT_USE_NANOSEC,
        }
    }
}

impl StatMatchPolicy {
    /// `core.usenanosec` when the key is unset: on for Windows, off elsewhere.
    ///
    /// A clone or switch writes the whole tree and then its index within the same second.
    /// Under whole-second racy detection every entry is then re-read once by the next status,
    /// and opening files is what makes that slow on Windows. NTFS keeps 100 ns timestamps, so
    /// comparing sub-second fractions there is sound and keeps that first status stat-only,
    /// as it is with Git for Windows. Elsewhere Git's default is off.
    pub const DEFAULT_USE_NANOSEC: bool = cfg!(windows);

    /// Load policy from repository config, using the platform defaults for unset keys.
    #[must_use]
    pub fn from_config(config: Option<&ConfigSet>) -> Self {
        let mut policy = Self::default();
        let Some(cfg) = config else {
            return policy;
        };
        if let Some(trust) = cfg.get_bool("core.trustctime").and_then(|r| r.ok()) {
            policy.trust_ctime = trust;
        }
        if let Some(raw) = cfg.get("core.checkstat") {
            policy.check_stat = match raw.to_ascii_lowercase().as_str() {
                "minimal" => false,
                "default" => true,
                other => cfg
                    .get_bool("core.checkstat")
                    .and_then(|r| r.ok())
                    .unwrap_or(!other.eq_ignore_ascii_case("minimal")),
            };
        }
        if let Some(use_ns) = cfg.get_bool("core.usenanosec").and_then(|r| r.ok()) {
            policy.use_nanosec = use_ns;
        }
        policy
    }
}

/// Git-compatible `(ctime, mtime)` seconds and nanoseconds from worktree metadata.
///
/// On Windows uses `filetime` (last write + creation times) so index stat comparison
/// matches Git's `mingw_stat` / `match_stat_data` rather than `Metadata::modified()` alone.
#[must_use]
pub(crate) fn metadata_times(meta: &fs::Metadata) -> ((u32, u32), (u32, u32)) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (
            (meta.ctime() as u32, meta.ctime_nsec() as u32),
            (meta.mtime() as u32, meta.mtime_nsec() as u32),
        )
    }
    #[cfg(not(unix))]
    {
        use filetime::FileTime;
        let mtime_ft = FileTime::from_last_modification_time(meta);
        let ctime_ft = FileTime::from_creation_time(meta).unwrap_or(mtime_ft);
        (
            filetime_to_index_pair(ctime_ft),
            filetime_to_index_pair(mtime_ft),
        )
    }
}

#[cfg(not(unix))]
#[must_use]
fn filetime_to_index_pair(ft: filetime::FileTime) -> (u32, u32) {
    let secs = ft.unix_seconds();
    if secs.is_negative() {
        (0, 0)
    } else {
        (secs as u32, ft.nanoseconds())
    }
}

/// Map `st_size` into the 32-bit size stored in the index (Git `munge_st_size`).
#[must_use]
pub fn munge_index_size(st_size: u64) -> u32 {
    let sd_size = st_size as u32;
    if sd_size == 0 && st_size != 0 {
        0x8000_0000
    } else {
        sd_size
    }
}

/// Whether two index entries carry the same stat fields under `policy`.
#[must_use]
pub fn index_entries_stat_equivalent(
    a: &IndexEntry,
    b: &IndexEntry,
    policy: StatMatchPolicy,
) -> bool {
    IndexStatFields::of_entry(a).equivalent_to(&IndexStatFields::of_entry(b), policy)
}

/// Git index extension signature `sdir` (sparse directory entries present).
pub(crate) const INDEX_EXT_SPARSE_DIRECTORIES: u32 = u32::from_be_bytes(*b"sdir");
/// Git index extension signature `UNTR` (untracked cache).
pub(crate) const INDEX_EXT_UNTRACKED: u32 = u32::from_be_bytes(*b"UNTR");
/// Git index extension signature `FSMN` (fsmonitor).
pub(crate) const INDEX_EXT_FSMONITOR: u32 = u32::from_be_bytes(*b"FSMN");
/// Fsmonitor index extension header version 1 (u64 timestamp token).
/// Index files at least this large verify their trailing checksum on a second thread while
/// the entries parse; below it, spawning the thread costs more than the hash.
const PARALLEL_TRAILER_VERIFY_MIN_BYTES: usize = 256 * 1024;

const FSMONITOR_EXT_VERSION1: u32 = 1;
/// Fsmonitor index extension header version 2 (NUL-terminated string token).
const FSMONITOR_EXT_VERSION2: u32 = 2;
/// Git index extension signature `REUC` (resolve undo).
pub(crate) const INDEX_EXT_RESOLVE_UNDO: u32 = u32::from_be_bytes(*b"REUC");
/// Git index extension signature `link` (split index).
pub(crate) const INDEX_EXT_LINK: u32 = u32::from_be_bytes(*b"link");

/// A single entry in the Git index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    /// Time the file metadata last changed (seconds since epoch).
    pub ctime_sec: u32,
    /// Nanosecond fraction of `ctime_sec`.
    pub ctime_nsec: u32,
    /// Time the file data last changed (seconds since epoch).
    pub mtime_sec: u32,
    /// Nanosecond fraction of `mtime_sec`.
    pub mtime_nsec: u32,
    /// Device number.
    pub dev: u32,
    /// Inode number.
    pub ino: u32,
    /// Unix file mode (`MODE_REGULAR`, `MODE_EXECUTABLE`, `MODE_SYMLINK`, …).
    pub mode: u32,
    /// Owner UID.
    pub uid: u32,
    /// Owner GID.
    pub gid: u32,
    /// File size in bytes (truncated to 32 bits).
    pub size: u32,
    /// SHA-1 of the blob object.
    pub oid: ObjectId,
    /// Entry flags (stage, assume-valid, extended, …).
    pub flags: u16,
    /// Extended flags (v3+ only).
    pub flags_extended: Option<u16>,
    /// Path relative to the repository root.  May contain `/` separators.
    pub path: Vec<u8>,
    /// Split index: position in shared base (1-based), or 0 if not from shared index.
    pub base_index_pos: u32,
}

impl IndexEntry {
    /// Merge stage (0 = normal, 1–3 = conflict stages).
    #[must_use]
    pub fn stage(&self) -> u8 {
        ((self.flags >> 12) & 0x3) as u8
    }

    /// Whether cached stat fields were never populated (e.g. index built from a tree).
    #[must_use]
    pub fn stat_cache_unset(&self) -> bool {
        self.size == 0 && self.mtime_sec == 0 && self.mtime_nsec == 0
    }

    pub(crate) fn set_stage(&mut self, stage: u8) {
        self.flags = (self.flags & 0x0FFF) | ((stage as u16 & 0x3) << 12);
    }

    /// Whether the assume-unchanged bit is set.
    #[must_use]
    pub fn assume_unchanged(&self) -> bool {
        self.flags & 0x8000 != 0
    }

    /// Whether the skip-worktree bit is set (extended flags, v3+).
    #[must_use]
    pub fn skip_worktree(&self) -> bool {
        self.flags_extended
            .map(|f| f & 0x4000 != 0)
            .unwrap_or(false)
    }

    /// Set the assume-unchanged bit.
    pub fn set_assume_unchanged(&mut self, value: bool) {
        if value {
            self.flags |= 0x8000;
        } else {
            self.flags &= !0x8000;
        }
    }

    /// Set the skip-worktree bit (promotes entry to v3).
    pub fn set_skip_worktree(&mut self, value: bool) {
        let fe = self.flags_extended.get_or_insert(0);
        if value {
            *fe |= 0x4000;
        } else {
            *fe &= !0x4000;
            if *fe == 0 {
                self.flags_extended = None;
            }
        }
    }

    /// Whether the intent-to-add bit is set (extended flags, v3+).
    #[must_use]
    pub fn intent_to_add(&self) -> bool {
        self.flags_extended
            .map(|f| f & 0x2000 != 0)
            .unwrap_or(false)
    }

    /// Set the intent-to-add bit (promotes entry to v3).
    pub fn set_intent_to_add(&mut self, value: bool) {
        let fe = self.flags_extended.get_or_insert(0);
        if value {
            *fe |= 0x2000;
        } else {
            *fe &= !0x2000;
            if *fe == 0 {
                self.flags_extended = None;
            }
        }
    }

    /// Sparse-index placeholder: tree mode, stage 0, and `SKIP_WORKTREE` set.
    #[must_use]
    pub fn is_sparse_directory_placeholder(&self) -> bool {
        self.mode == MODE_TREE && self.stage() == 0 && self.skip_worktree()
    }

    /// In-memory only: `ls-files --with-tree` hides stage-1 overlay rows that duplicate stage 0.
    const FLAG_EXT_OVERLAY_TREE_SKIP: u16 = 0x8000;
    /// In-memory and on-disk compatibility bit for fsmonitor validity (`git ls-files -f`).
    const FLAG_EXT_FSMONITOR_VALID: u16 = 0x1000;
    /// In-memory only (Git `CE_UPTODATE`): see [`Self::mark_content_verified`].
    const FLAG_EXT_CONTENT_VERIFIED: u16 = 0x0800;
    /// Extended flags Git persists in the on-disk index (`CE_EXTENDED_FLAGS` in `read-cache-ll.h`).
    /// `CE_FSMONITOR_VALID` is in-memory only; fsmonitor state lives in the `FSMN` extension.
    const FLAG_EXT_ON_DISK: u16 = 0x2000 | 0x4000;

    /// Extended flag bits safe to write to a Git-compatible on-disk index.
    fn disk_flags_extended(fe: u16) -> u16 {
        fe & Self::FLAG_EXT_ON_DISK
    }

    /// Whether extended index flags request skipping overlay tree output.
    #[must_use]
    pub fn overlay_tree_skip_output(&self) -> bool {
        self.flags_extended
            .is_some_and(|fe| fe & Self::FLAG_EXT_OVERLAY_TREE_SKIP != 0)
    }

    fn set_overlay_tree_skip_output(&mut self, value: bool) {
        let fe = self.flags_extended.get_or_insert(0);
        if value {
            *fe |= Self::FLAG_EXT_OVERLAY_TREE_SKIP;
        } else {
            *fe &= !Self::FLAG_EXT_OVERLAY_TREE_SKIP;
            if *fe == 0 {
                self.flags_extended = None;
            }
        }
    }

    /// Whether the fsmonitor-valid bit is set.
    #[must_use]
    pub fn fsmonitor_valid(&self) -> bool {
        self.flags_extended
            .is_some_and(|fe| fe & Self::FLAG_EXT_FSMONITOR_VALID != 0)
    }

    /// Set or clear the fsmonitor-valid bit.
    pub fn set_fsmonitor_valid(&mut self, value: bool) {
        let fe = self.flags_extended.get_or_insert(0);
        if value {
            *fe |= Self::FLAG_EXT_FSMONITOR_VALID;
        } else {
            *fe &= !Self::FLAG_EXT_FSMONITOR_VALID;
            if *fe == 0 {
                self.flags_extended = None;
            }
        }
    }

    /// Compare extended flags for merge index fast paths, ignoring in-memory-only bits.
    pub(crate) fn extended_flags_equal_ignoring_volatile(
        left: Option<u16>,
        right: Option<u16>,
    ) -> bool {
        (left.unwrap_or(0) & Self::FLAG_EXT_ON_DISK)
            == (right.unwrap_or(0) & Self::FLAG_EXT_ON_DISK)
    }

    /// Mark this entry's `oid` as just hashed from the worktree file whose stat data it holds
    /// (in-memory only, like [`Index::mark_content_verified`] without a per-path map entry).
    pub(crate) fn mark_content_verified(&mut self) {
        *self.flags_extended.get_or_insert(0) |= Self::FLAG_EXT_CONTENT_VERIFIED;
    }

    /// Clear the content-verified bit, returning whether it was set.
    pub(crate) fn take_content_verified(&mut self) -> bool {
        let Some(fe) = self.flags_extended else {
            return false;
        };
        if fe & Self::FLAG_EXT_CONTENT_VERIFIED == 0 {
            return false;
        }
        let rest = fe & !Self::FLAG_EXT_CONTENT_VERIFIED;
        self.flags_extended = (rest != 0).then_some(rest);
        true
    }
}

/// The in-memory representation of the Git index file (staging area).
///
/// # Examples
///
/// ```
/// use grit_lib::repo::{InitOptions, Repository};
///
/// # fn main() -> grit_lib::error::Result<()> {
/// let dir = tempfile::tempdir()?;
/// let repo = Repository::init(dir.path(), &InitOptions::default())?;
/// let index = repo.index_handle().load()?;
/// assert!(index.entries().is_empty());
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct Index {
    /// Index format version (2 or 3).
    pub version: u32,
    /// Index entries, sorted by (path, stage) when [`Self::entries_sorted`] is true.
    pub(crate) entries: Vec<IndexEntry>,
    /// When `true`, `entries` are in Git `(path, stage)` order and O(log n) lookups apply.
    pub(crate) entries_sorted: bool,
    /// When true, the on-disk index includes the `sdir` extension (sparse index).
    pub sparse_directories: bool,
    /// Optional untracked-cache extension (`UNTR`), matching Git's `istate->untracked`.
    pub untracked_cache: Option<untracked_cache::UntrackedCache>,
    /// Optional fsmonitor token extension (`FSMN`).
    pub fsmonitor_last_update: Option<String>,
    /// Parsed fsmonitor dirty bitmap from the `FSMN` extension, when present.
    pub(crate) fsmonitor_dirty: Option<crate::ewah_bitmap::EwahBitmap>,
    /// On-disk `FSMN` extension payload for byte-for-byte round-trip when entries are unchanged.
    pub(crate) fsmonitor_raw_extension: Option<Vec<u8>>,
    /// Optional `REUC` resolve-undo extension (paths that were unmerged before a resolution).
    pub resolve_undo: Option<BTreeMap<Vec<u8>, ResolveUndoRecord>>,
    /// Split index `link` extension (bitmaps cleared after load merge).
    pub(crate) split_link: Option<crate::split_index::SplitIndexLink>,
    /// Root tree OID from a valid `TREE` index extension (`cache_tree`), when present.
    pub cache_tree_root: Option<ObjectId>,
    /// Parsed `TREE` index extension (`cache-tree`) preserving invalid and subtree nodes.
    pub cache_tree: Option<CacheTreeNode>,
    /// The repository's object hash algorithm. Determines the width of entry
    /// object IDs and the trailing checksum on disk. Set authoritatively from
    /// the object database when loaded via the repository; defaults to SHA-1.
    pub hash_algo: HashAlgo,
    /// On-disk index mtime when this index was last read (Git `istate->timestamp`).
    pub source_mtime: Option<(u32, u32)>,
    /// When set, [`Self::invalidate_untracked_cache_for_path`] is a no-op (bulk staging defers invalidation).
    pub(crate) defer_untracked_cache_invalidation: bool,
    /// Skip the pre-write racy smudge walk (checkout just aligned worktree and index entries).
    pub(crate) skip_racy_smudge_on_write: bool,
    /// In-memory only (Git `CE_UPTODATE`): stage-0 paths whose worktree content this process
    /// hashed to the recorded object id after taking the stat data now in the entry. The
    /// pre-write racy smudge skips an entry that still holds that id instead of reading the
    /// file again; the next index write consumes the map. Bulk staging marks freshly built
    /// entries with [`IndexEntry::mark_content_verified`] instead, which needs no path copy.
    pub(crate) content_verified: HashMap<Vec<u8>, ObjectId>,
    /// In-memory only: Git `istate->fsmonitor_has_run_once`.
    pub(crate) fsmonitor_refresh_ran: bool,
    /// Skip racy smudge and UNTR shell setup on the next index write (checkout fast path).
    pub(crate) suppress_pre_write_index_housekeeping: bool,
}

impl Default for Index {
    fn default() -> Self {
        Self::new()
    }
}

/// One node from Git's `TREE` index extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheTreeNode {
    /// Path component for this node. The root node stores an empty name.
    pub name: Vec<u8>,
    /// Number of index entries covered by this node, or `-1` when invalid.
    pub entry_count: i32,
    /// Tree object ID for valid nodes. Invalid nodes do not store an object ID.
    pub oid: Option<ObjectId>,
    /// Immediate child cache-tree nodes.
    pub children: Vec<CacheTreeNode>,
}

impl CacheTreeNode {
    /// Create a valid cache-tree node.
    #[must_use]
    pub fn valid(
        name: Vec<u8>,
        entry_count: i32,
        oid: ObjectId,
        children: Vec<CacheTreeNode>,
    ) -> Self {
        Self {
            name,
            entry_count,
            oid: Some(oid),
            children,
        }
    }

    /// Mark this node as invalid while preserving its children.
    pub fn invalidate(&mut self) {
        self.entry_count = -1;
        self.oid = None;
    }

    /// Returns whether this node has a valid cached tree object ID.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.entry_count >= 0 && self.oid.is_some()
    }
}

pub use crate::index_eoie_ieot::IndexParseContext;

/// Options for loading an index from disk.
#[derive(Debug, Clone, Copy)]
pub struct IndexLoadOptions {
    /// If the index contains sparse directory placeholders, expand them to full file entries.
    pub expand_sparse_directories: bool,
    /// Thread policy for parallel index entry load (EOIE/IEOT).
    pub parse_context: IndexParseContext,
}

impl Default for IndexLoadOptions {
    fn default() -> Self {
        Self {
            expand_sparse_directories: true,
            parse_context: IndexParseContext::default(),
        }
    }
}

/// Version used after an invalid `GIT_INDEX_VERSION` value (matches Git stderr: "Using version 3").
const INDEX_ENV_INVALID_FALLBACK: u32 = 3;
/// Version used after an invalid `index.version` config value (same message as env).
const INDEX_CONFIG_INVALID_FALLBACK: u32 = 3;
/// Minimum supported index version.
const INDEX_FORMAT_LB: u32 = 2;
/// Maximum supported index version (version 4 requests are accepted and
/// downgraded on write).
const INDEX_FORMAT_UB: u32 = 4;
/// Index extension signature `TREE` (cache-tree).
pub(crate) const INDEX_EXT_CACHE_TREE: u32 = 0x5452_4545;

/// Best-effort read of Git's `TREE` index extension (`cache_tree_read`).
pub(crate) fn parse_cache_tree(data: &[u8], hash_len: usize) -> Option<CacheTreeNode> {
    let (node, pos) = parse_cache_tree_node(data, 0, hash_len)?;
    if pos == data.len() {
        Some(node)
    } else {
        None
    }
}

fn parse_cache_tree_node(
    data: &[u8],
    mut pos: usize,
    hash_len: usize,
) -> Option<(CacheTreeNode, usize)> {
    let name_end = data.get(pos..)?.iter().position(|&b| b == 0)? + pos;
    let name = data[pos..name_end].to_vec();
    pos = name_end + 1;

    let (entry_count, consumed) = parse_signed_int_prefix(&data[pos..])?;
    pos += consumed;
    if data.get(pos) != Some(&b' ') {
        return None;
    }
    pos += 1;
    let (subtree_count, consumed) = parse_signed_int_prefix(&data[pos..])?;
    if subtree_count < 0 {
        return None;
    }
    pos += consumed;
    if data.get(pos) != Some(&b'\n') {
        return None;
    }
    pos += 1;

    let oid = if entry_count >= 0 {
        if data.len().saturating_sub(pos) < hash_len {
            return None;
        }
        let oid = ObjectId::from_bytes(&data[pos..pos + hash_len]).ok()?;
        pos += hash_len;
        Some(oid)
    } else {
        None
    };

    let mut children = Vec::with_capacity(subtree_count as usize);
    for _ in 0..subtree_count {
        let (child, next) = parse_cache_tree_node(data, pos, hash_len)?;
        children.push(child);
        pos = next;
    }

    Some((
        CacheTreeNode {
            name,
            entry_count,
            oid,
            children,
        },
        pos,
    ))
}

fn parse_signed_int_prefix(data: &[u8]) -> Option<(i32, usize)> {
    let mut j = 0usize;
    while j < data.len() && data[j] == b' ' {
        j += 1;
    }
    let start = j;
    if j < data.len() && data[j] == b'-' {
        j += 1;
    }
    let digit_start = j;
    while j < data.len() && data[j].is_ascii_digit() {
        j += 1;
    }
    if j == digit_start {
        return None;
    }
    let s = std::str::from_utf8(&data[start..j]).ok()?;
    let v: i32 = s.parse().ok()?;
    Some((v, j))
}

fn serialize_cache_tree_node(node: &CacheTreeNode, out: &mut Vec<u8>) {
    out.extend_from_slice(&node.name);
    out.push(0);
    out.extend_from_slice(node.entry_count.to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(node.children.len().to_string().as_bytes());
    out.push(b'\n');
    if node.entry_count >= 0 {
        if let Some(oid) = node.oid {
            out.extend_from_slice(oid.as_bytes());
        }
    }
    for child in &node.children {
        serialize_cache_tree_node(child, out);
    }
}

fn format_cache_tree_node(node: &CacheTreeNode, parent_path: &str, out: &mut String) {
    let path = if node.name.is_empty() {
        String::new()
    } else {
        let name = String::from_utf8_lossy(&node.name);
        format!("{parent_path}{name}/")
    };
    if node.is_valid() {
        if let Some(oid) = node.oid {
            out.push_str(&format!(
                "{} {} ({} entries, {} subtrees)\n",
                oid,
                path,
                node.entry_count,
                node.children.len()
            ));
        }
    } else {
        out.push_str(&format!(
            "{:<40} {} ({} subtrees)\n",
            "invalid",
            path,
            node.children.len()
        ));
    }
    for child in &node.children {
        format_cache_tree_node(child, &path, out);
    }
}

/// Emit a single cache-tree node line in `test-tool dump-cache-tree` format.
fn dump_one_line(node: &CacheTreeNode, pfx: &str, out: &mut String) {
    if node.entry_count < 0 {
        out.push_str(&format!(
            "{:<40} {} ({} subtrees)\n",
            "invalid",
            pfx,
            node.children.len()
        ));
    } else {
        let oid = node.oid.unwrap_or_else(ObjectId::zero);
        out.push_str(&format!(
            "{} {} ({} entries, {} subtrees)\n",
            oid,
            pfx,
            node.entry_count,
            node.children.len()
        ));
    }
}

/// Walk the stored cache-tree (`it`) against a freshly built reference (`ref`),
/// emitting only the nodes that exist in both — matching Git's `dump_cache_tree`.
fn dump_cache_tree_pair(
    it: &CacheTreeNode,
    reference: &CacheTreeNode,
    pfx: &str,
    out: &mut String,
) {
    dump_one_line(it, pfx, out);

    for child in &it.children {
        let Some(ref_child) = reference.children.iter().find(|c| c.name == child.name) else {
            continue;
        };
        let name = String::from_utf8_lossy(&child.name);
        let child_pfx = format!("{pfx}{name}/");
        dump_cache_tree_pair(child, ref_child, &child_pfx, out);
    }
}

/// Read `GIT_INDEX_VERSION` and return the requested version.
///
/// If the environment variable is unset, returns `None`.
/// If it is set but invalid (non-numeric or out of range 2..=4), prints a
/// warning to stderr and returns the default version.
pub fn get_index_format_from_env() -> Option<u32> {
    let val = crate::environment::Environment::empty().var("GIT_INDEX_VERSION")?;
    if val.is_empty() {
        return None;
    }
    match val.parse::<u32>() {
        Ok(v) if (INDEX_FORMAT_LB..=INDEX_FORMAT_UB).contains(&v) => Some(v),
        _ => Some(INDEX_ENV_INVALID_FALLBACK),
    }
}

/// Parse Git's `FSMN` index extension payload (`read_fsmonitor_extension`).
pub(crate) fn parse_fsmonitor_extension(
    ext_data: &[u8],
) -> Result<(String, crate::ewah_bitmap::EwahBitmap)> {
    if ext_data.len() < 4 + 1 + 4 {
        return Err(Error::IndexFsmonitorExtensionTruncated);
    }
    let hdr_version = u32::from_be_bytes(
        ext_data[0..4]
            .try_into()
            .map_err(|_| Error::IndexFsmonitorExtensionTruncated)?,
    );
    let mut pos = 4usize;
    let token = match hdr_version {
        FSMONITOR_EXT_VERSION1 => {
            if ext_data.len() < pos + 8 + 4 {
                return Err(Error::IndexFsmonitorExtensionTruncated);
            }
            let ts = u64::from_be_bytes(
                ext_data[pos..pos + 8]
                    .try_into()
                    .map_err(|_| Error::IndexFsmonitorExtensionTruncated)?,
            );
            pos += 8;
            ts.to_string()
        }
        FSMONITOR_EXT_VERSION2 => {
            let nul = ext_data[pos..]
                .iter()
                .position(|&b| b == 0)
                .ok_or(Error::IndexFsmonitorExtensionTruncated)?;
            let token = String::from_utf8_lossy(&ext_data[pos..pos + nul]).into_owned();
            pos += nul + 1;
            token
        }
        v => return Err(Error::IndexFsmonitorExtensionBadVersion(v)),
    };
    if ext_data.len() < pos + 4 {
        return Err(Error::IndexFsmonitorExtensionTruncated);
    }
    let ewah_size = u32::from_be_bytes(
        ext_data[pos..pos + 4]
            .try_into()
            .map_err(|_| Error::IndexFsmonitorExtensionTruncated)?,
    ) as usize;
    pos += 4;
    if ext_data.len() < pos + ewah_size {
        return Err(Error::IndexFsmonitorExtensionTruncated);
    }
    let ewah_slice = &ext_data[pos..pos + ewah_size];
    let (dirty, consumed) = crate::ewah_bitmap::EwahBitmap::deserialize_prefix(ewah_slice)
        .ok_or(Error::IndexFsmonitorExtensionEwahInvalid)?;
    if consumed != ewah_size {
        return Err(Error::IndexFsmonitorExtensionEwahInvalid);
    }
    Ok((token, dirty))
}

/// Apply `FSMN` dirty bits to in-memory `CE_FSMONITOR_VALID` (`fsmonitor_ewah_callback`).
pub(crate) fn apply_fsmonitor_dirty_to_entries(
    entries: &mut [IndexEntry],
    dirty: &crate::ewah_bitmap::EwahBitmap,
) {
    for entry in entries.iter_mut() {
        entry.set_fsmonitor_valid(true);
    }
    dirty.each_set_bit(|pos| {
        if let Some(entry) = entries.get_mut(pos) {
            entry.set_fsmonitor_valid(false);
        }
    });
}

impl Index {
    /// Create a new, empty index.
    ///
    /// Respects `GIT_INDEX_VERSION` if set, otherwise defaults to version 2.
    #[must_use]
    pub fn new() -> Self {
        let version = get_index_format_from_env().unwrap_or(2);
        Self {
            version,
            entries: Vec::new(),
            entries_sorted: true,
            sparse_directories: false,
            untracked_cache: None,
            fsmonitor_last_update: None,
            fsmonitor_dirty: None,
            fsmonitor_raw_extension: None,
            resolve_undo: None,
            split_link: None,
            cache_tree_root: None,
            cache_tree: None,
            hash_algo: HashAlgo::Sha1,
            source_mtime: None,
            defer_untracked_cache_invalidation: false,
            skip_racy_smudge_on_write: false,
            content_verified: HashMap::new(),
            fsmonitor_refresh_ran: false,
            suppress_pre_write_index_housekeeping: false,
        }
    }

    /// Index entries used for `.gitattributes` fallback when the worktree copy is missing.
    ///
    /// Contains only stage-0 paths named `.gitattributes` or `*/.gitattributes`, so callers
    /// can pass attribute context without cloning unrelated tracked files.
    #[must_use]
    pub fn gitattributes_snapshot(&self) -> Self {
        let mut snap = Self::new();
        snap.version = self.version;
        snap.hash_algo = self.hash_algo;
        for entry in &self.entries {
            if entry.stage() != 0 {
                continue;
            }
            if gitattributes_index_path(&entry.path) {
                snap.add_or_replace(entry.clone());
            }
        }
        snap
    }

    /// Create a new, empty index at a fixed version without consulting
    /// `GIT_INDEX_VERSION` or config.
    ///
    /// Unlike [`Self::new`], this never reads the environment and therefore never
    /// emits the "GIT_INDEX_VERSION set, but the value is invalid" warning. Use it
    /// for throwaway baseline indexes (e.g. sparse-index diff references) where the
    /// version is irrelevant and Git resolves the format only once per operation.
    #[must_use]
    pub fn empty(version: u32) -> Self {
        Self {
            version,
            entries: Vec::new(),
            entries_sorted: true,
            sparse_directories: false,
            untracked_cache: None,
            fsmonitor_last_update: None,
            fsmonitor_dirty: None,
            fsmonitor_raw_extension: None,
            resolve_undo: None,
            split_link: None,
            cache_tree_root: None,
            cache_tree: None,
            hash_algo: HashAlgo::Sha1,
            source_mtime: None,
            defer_untracked_cache_invalidation: false,
            skip_racy_smudge_on_write: false,
            content_verified: HashMap::new(),
            fsmonitor_refresh_ran: false,
            suppress_pre_write_index_housekeeping: false,
        }
    }

    /// Create a new empty index, respecting config values for version.
    ///
    /// Priority matches Git's `prepare_repo_settings`: `GIT_INDEX_VERSION` env, then
    /// `feature.manyFiles` (implies version 4), then `index.version` (overrides version).
    pub fn new_with_config(
        config_index_version: Option<&str>,
        config_many_files: Option<&str>,
    ) -> Self {
        if let Some(v) = get_index_format_from_env() {
            return Self {
                version: v,
                entries: Vec::new(),
                entries_sorted: true,
                sparse_directories: false,
                untracked_cache: None,
                fsmonitor_last_update: None,
                fsmonitor_dirty: None,
                fsmonitor_raw_extension: None,
                resolve_undo: None,
                split_link: None,
                cache_tree_root: None,
                cache_tree: None,
                hash_algo: HashAlgo::default(),
                source_mtime: None,
                defer_untracked_cache_invalidation: false,
                skip_racy_smudge_on_write: false,
                content_verified: HashMap::new(),
                fsmonitor_refresh_ran: false,
                suppress_pre_write_index_housekeeping: false,
            };
        }

        let many_files = config_truthy(config_many_files);
        let mut version = if many_files { 4 } else { 2 };

        if let Some(val) = config_index_version {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                match trimmed.parse::<u32>() {
                    Ok(v) if (INDEX_FORMAT_LB..=INDEX_FORMAT_UB).contains(&v) => {
                        version = v;
                    }
                    _ => {
                        version = INDEX_CONFIG_INVALID_FALLBACK;
                    }
                }
            }
        }

        Self {
            version,
            entries: Vec::new(),
            entries_sorted: true,
            sparse_directories: false,
            untracked_cache: None,
            fsmonitor_last_update: None,
            fsmonitor_dirty: None,
            fsmonitor_raw_extension: None,
            resolve_undo: None,
            split_link: None,
            cache_tree_root: None,
            cache_tree: None,
            hash_algo: HashAlgo::default(),
            source_mtime: None,
            defer_untracked_cache_invalidation: false,
            skip_racy_smudge_on_write: false,
            content_verified: HashMap::new(),
            fsmonitor_refresh_ran: false,
            suppress_pre_write_index_housekeeping: false,
        }
    }

    /// New empty index using a loaded [`ConfigSet`] (includes `-c` / `GIT_CONFIG_PARAMETERS`).
    ///
    /// Same precedence as [`Self::new_with_config`], but reads `feature.manyFiles` and
    /// `index.version` from `config`.
    #[must_use]
    pub fn new_from_config(config: &ConfigSet) -> Self {
        if let Some(v) = get_index_format_from_env() {
            return Self {
                version: v,
                entries: Vec::new(),
                entries_sorted: true,
                sparse_directories: false,
                untracked_cache: None,
                fsmonitor_last_update: None,
                fsmonitor_dirty: None,
                fsmonitor_raw_extension: None,
                resolve_undo: None,
                split_link: None,
                cache_tree_root: None,
                cache_tree: None,
                hash_algo: HashAlgo::default(),
                source_mtime: None,
                defer_untracked_cache_invalidation: false,
                skip_racy_smudge_on_write: false,
                content_verified: HashMap::new(),
                fsmonitor_refresh_ran: false,
                suppress_pre_write_index_housekeeping: false,
            };
        }

        let many_files = config
            .get_bool("feature.manyFiles")
            .and_then(|r| r.ok())
            .unwrap_or(false);
        let mut version = if many_files { 4 } else { 2 };

        if let Some(val) = config.get("index.version") {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                match trimmed.parse::<u32>() {
                    Ok(v) if (INDEX_FORMAT_LB..=INDEX_FORMAT_UB).contains(&v) => {
                        version = v;
                    }
                    _ => {
                        version = INDEX_CONFIG_INVALID_FALLBACK;
                    }
                }
            }
        }

        Self {
            version,
            entries: Vec::new(),
            entries_sorted: true,
            sparse_directories: false,
            untracked_cache: None,
            fsmonitor_last_update: None,
            fsmonitor_dirty: None,
            fsmonitor_raw_extension: None,
            resolve_undo: None,
            split_link: None,
            cache_tree_root: None,
            cache_tree: None,
            hash_algo: HashAlgo::default(),
            source_mtime: None,
            defer_untracked_cache_invalidation: false,
            skip_racy_smudge_on_write: false,
            content_verified: HashMap::new(),
            fsmonitor_refresh_ran: false,
            suppress_pre_write_index_housekeeping: false,
        }
    }

    /// Load an index from the given file path without expanding sparse-directory placeholders.
    ///
    /// Returns an empty index if the file does not exist.
    ///
    /// # Errors
    ///
    /// Returns [`Error::IndexError`] if the file is present but corrupt.
    pub fn load(path: &Path) -> Result<Self> {
        match read_index_file(path) {
            Ok((data, mtime)) => {
                let mut idx = Self::parse(&data)?;
                idx.source_mtime = Some(mtime);
                Ok(idx)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self {
                sparse_directories: false,
                ..Self::new()
            }),
            Err(e) => Err(Error::Io(e)),
        }
    }

    /// Load an index and expand sparse-directory placeholders using the object database.
    ///
    /// After a successful return, [`Index::sparse_directories`] is cleared and every
    /// placeholder is replaced by the blob entries from the referenced tree.
    pub fn load_expand_sparse(path: &Path, odb: &Odb) -> Result<Self> {
        let mut idx = match read_index_file(path) {
            Ok((data, mtime)) => Self {
                source_mtime: Some(mtime),
                ..Self::parse_with_algo(&data, odb.hash_algo())?
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => Self {
                sparse_directories: false,
                hash_algo: odb.hash_algo(),
                ..Self::new()
            },
            Err(e) => return Err(Error::Io(e)),
        };
        idx.expand_sparse_directory_placeholders(
            odb,
            crate::path_validate::ProtectOptions::default(),
        )?;
        Ok(idx)
    }

    /// Like [`Self::load_expand_sparse`], but applies path protection from `config` when expanding sparse directories.
    pub fn load_expand_sparse_with_config(
        path: &Path,
        odb: &Odb,
        config: &ConfigSet,
    ) -> Result<Self> {
        Self::load_expand_sparse_optional_with_context(
            path,
            odb,
            Some(config),
            IndexParseContext::default(),
        )
    }

    /// Like [`Index::load_expand_sparse`], but treats a missing index as an empty index.
    ///
    /// An index file too short to hold a header and checksum is an error, as in Git.
    pub fn load_expand_sparse_optional(path: &Path, odb: &Odb) -> Result<Self> {
        Self::load_expand_sparse_optional_with_context(
            path,
            odb,
            None,
            IndexParseContext::default(),
        )
    }

    /// Like [`Self::load_expand_sparse_optional`], but passes config and thread policy for EOIE/IEOT load.
    pub fn load_expand_sparse_optional_with_context(
        path: &Path,
        odb: &Odb,
        config: Option<&ConfigSet>,
        parse_context: IndexParseContext,
    ) -> Result<Self> {
        let mut idx =
            Self::load_preserving_sparse_optional_with_context(path, odb, config, parse_context)?;
        let protect = config
            .map(crate::path_validate::ProtectOptions::from_config)
            .unwrap_or_default();
        idx.expand_sparse_directory_placeholders(odb, protect)?;
        Ok(idx)
    }

    /// Read index rows without expanding tree placeholders. Callers must resolve split indexes
    /// and expand any directories their operation needs before examining individual paths.
    pub(crate) fn load_preserving_sparse_optional_with_context(
        path: &Path,
        odb: &Odb,
        config: Option<&ConfigSet>,
        parse_context: IndexParseContext,
    ) -> Result<Self> {
        let algo = odb.hash_algo();
        let idx = match read_index_file(path) {
            Ok((data, mtime)) => Self {
                source_mtime: Some(mtime),
                ..Self::parse_with_algo_and_config(&data, algo, config, parse_context)?
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => Self {
                hash_algo: algo,
                ..Self::new()
            },
            Err(e) => return Err(Error::Io(e)),
        };
        Ok(idx)
    }

    /// Returns true if the index contains sparse-index tree placeholders (`MODE_TREE` + skip-worktree).
    #[must_use]
    pub fn has_sparse_directory_placeholders(&self) -> bool {
        self.entries
            .iter()
            .any(IndexEntry::is_sparse_directory_placeholder)
    }

    /// Replace sparse-directory placeholder entries with all blob paths from their trees.
    ///
    /// Each placeholder must reference a tree object. New entries are marked skip-worktree like Git's
    /// expanded index, except we keep `sparse_directories` false in memory after expansion.
    pub fn expand_sparse_directory_placeholders(
        &mut self,
        odb: &Odb,
        protect: crate::path_validate::ProtectOptions,
    ) -> Result<()> {
        self.expand_sparse_directories_matching(odb, protect, |_| true)
    }

    /// Expand only the selected sparse directories, leaving other subtrees opaque.
    pub(crate) fn expand_sparse_directories_matching(
        &mut self,
        odb: &Odb,
        protect: crate::path_validate::ProtectOptions,
        mut expand: impl FnMut(&IndexEntry) -> bool,
    ) -> Result<()> {
        if !self.has_sparse_directory_placeholders() {
            return Ok(());
        }
        let mut out: Vec<IndexEntry> = Vec::with_capacity(self.entries.len());
        for entry in self.entries.drain(..) {
            if entry.is_sparse_directory_placeholder() && expand(&entry) {
                let prefix = trim_trailing_slash_bytes(&entry.path);
                let blobs = flatten_tree_blobs(odb, &entry.oid, prefix, protect)?;
                #[cfg(test)]
                odb.hot_path_metrics()
                    .record_sparse_expanded_rows(blobs.len());
                out.extend(blobs);
            } else {
                out.push(entry);
            }
        }
        self.entries = out;
        self.sparse_directories = self.has_sparse_directory_placeholders();
        self.sort();
        Ok(())
    }

    /// Collapse consecutive skip-worktree subtrees into sparse-directory placeholders when
    /// `cone_mode` is true and each directory is outside the sparse cone.
    ///
    /// `head_tree` is the tree OID at `HEAD`. When `enable_sparse_index` is false, clears
    /// [`Index::sparse_directories`] and returns without collapsing.
    pub fn try_collapse_sparse_directories(
        &mut self,
        odb: &Odb,
        head_tree: &ObjectId,
        patterns: &[String],
        cone_mode: bool,
        enable_sparse_index: bool,
        protect: crate::path_validate::ProtectOptions,
    ) -> Result<()> {
        if !enable_sparse_index || !cone_mode {
            self.sparse_directories = false;
            return Ok(());
        }

        self.ensure_canonical_order();
        let mut prefixes = BTreeSet::<Vec<u8>>::new();
        let mut last_dir: &[u8] = &[];
        for e in &self.entries {
            if e.stage() != 0 || e.mode == MODE_TREE || !e.skip_worktree() {
                continue;
            }
            // Sorted rows share their directory with their neighbours; every ancestor of a
            // directory already seen is already in the set.
            let Some(slash) = e.path.iter().rposition(|&b| b == b'/') else {
                continue;
            };
            let dir = &e.path[..slash];
            if dir == last_dir {
                continue;
            }
            last_dir = dir;
            collect_directory_prefixes(&e.path, &mut prefixes);
        }

        // Deepest prefixes first so nested dirs collapse before parents.
        let mut ordered: Vec<Vec<u8>> = prefixes.into_iter().collect();
        ordered.sort_by_key(|p| std::cmp::Reverse(p.len()));

        // Each directory is judged against the index as it would look with the directories
        // collapsed so far, but `entries` is only rewritten once at the end: removing rows and
        // inserting a placeholder per directory is O(entries) each.
        let mut collapsed = CollapsedDirectories::default();
        let mut head_trees = PrefixTreeResolver::new(*head_tree);
        for pref in ordered {
            let pref_str = String::from_utf8_lossy(&pref);
            if directory_in_cone(&pref_str, patterns, cone_mode) {
                continue;
            }
            let Some(subtree_oid) = head_trees.resolve(odb, &pref)? else {
                continue;
            };
            let view = SparseCollapseView {
                entries: &self.entries,
                collapsed: &collapsed,
            };
            let mut expected = collect_sparse_aware_expected_blobs(
                odb,
                &subtree_oid,
                &pref,
                patterns,
                cone_mode,
                &view,
                protect,
            )?;
            if expected.is_empty() {
                continue;
            }
            let mut directory = pref.clone();
            directory.push(b'/');
            let start = self.entries.partition_point(|e| e.path < directory);
            let end =
                start + self.entries[start..].partition_point(|e| e.path.starts_with(&directory));
            let rows = &self.entries[start..end];
            if rows.iter().any(|e| e.stage() != 0) {
                continue;
            }
            // Rows are sorted and all stage 0, so `matched` is already in path order.
            let nested = collapsed.has_descendants(&directory);
            let mut scratch = Vec::new();
            let matched: Vec<&IndexEntry> = rows
                .iter()
                .filter(|e| {
                    e.mode != MODE_TREE && !(nested && collapsed.covers(&e.path, &mut scratch))
                })
                .collect();
            if matched.len() != expected.len() {
                continue;
            }
            expected.sort_by(|a, b| a.path.cmp(&b.path));
            if !matched
                .iter()
                .zip(expected.iter())
                .all(|(a, b)| a.path == b.path && a.oid == b.oid && a.mode == b.mode)
            {
                continue;
            }
            if !matched.iter().all(|e| e.skip_worktree()) {
                continue;
            }
            // Git's convert_to_sparse_rec refuses to collapse a directory that contains a
            // submodule (gitlink); the sparse-directory entry could not faithfully represent
            // the gitlink's committed OID. Leave such directories expanded.
            if matched.iter().any(|e| e.mode == MODE_GITLINK)
                || expected.iter().any(|e| e.mode == MODE_GITLINK)
            {
                continue;
            }

            collapsed.insert(directory.clone(), subtree_oid);
            if let Ok(p) = std::str::from_utf8(&directory) {
                self.invalidate_untracked_cache_for_path(p);
            }
            self.invalidate_cache_tree_for_path(&directory);
            self.touch_fsmonitor_after_entry_change();
        }

        if !collapsed.is_empty() {
            let covered = collapsed.covered_rows(&self.entries);
            let mut entries: Vec<IndexEntry> = std::mem::take(&mut self.entries)
                .into_iter()
                .zip(covered)
                .filter(|(e, covered)| e.stage() != 0 || !covered)
                .map(|(e, _)| e)
                .collect();
            entries.extend(collapsed.into_placeholders());
            self.entries = entries;
            self.entries_sorted = false;
            self.sort();
        }
        self.sparse_directories = self.has_sparse_directory_placeholders();
        Ok(())
    }

    /// Parse index bytes (the whole file including trailing SHA-1).
    ///
    /// # Errors
    ///
    /// Returns [`Error::IndexError`] on structural problems.
    pub fn parse(data: &[u8]) -> Result<Self> {
        // The index file does not record its hash width; it is fixed by the
        // repository. Detect it from the trailing checksum (callers with a
        // repository should prefer [`Self::parse_with_algo`]).
        Self::parse_with_algo(data, detect_index_hash_algo(data))
    }

    /// Parse an index using a known hash algorithm (e.g. from the repository's
    /// `extensions.objectformat`), which determines the entry OID width and the
    /// trailing checksum width.
    pub fn parse_with_algo(data: &[u8], hash_algo: HashAlgo) -> Result<Self> {
        Self::parse_with_algo_and_config(data, hash_algo, None, IndexParseContext::default())
    }

    /// Parse an index with optional repository config and thread policy for EOIE/IEOT load.
    pub fn parse_with_algo_and_config(
        data: &[u8],
        hash_algo: HashAlgo,
        config: Option<&ConfigSet>,
        parse_context: IndexParseContext,
    ) -> Result<Self> {
        const INDEX_HEADER_LEN: usize = 12;

        let hash_len = hash_algo.len();
        if data.len() < INDEX_HEADER_LEN + hash_len {
            return Err(Error::IndexError(
                "file too short for index header and checksum".to_owned(),
            ));
        }
        let (body, checksum) = data.split_at(data.len() - hash_len);
        if body.len() < INDEX_HEADER_LEN {
            return Err(Error::IndexError(
                "file too short for index header and checksum".to_owned(),
            ));
        }
        let trailer_error = |e: crate::hash::TrailerMismatch| {
            Error::IndexError(format!(
                "{} checksum mismatch: {e}",
                hash_algo.name().to_uppercase()
            ))
        };
        if checksum.iter().all(|&b| b == 0) {
            return Self::parse_body(body, hash_algo, config, parse_context);
        }
        let verify_in_parallel = data.len() >= PARALLEL_TRAILER_VERIFY_MIN_BYTES
            && parse_context.thread_policy.threads_override != Some(1);
        if !verify_in_parallel {
            verify_trailer(hash_algo, data).map_err(trailer_error)?;
            return Self::parse_body(body, hash_algo, config, parse_context);
        }
        // Hashing a large index costs about as much as parsing it; overlap the two and
        // report a checksum mismatch ahead of any structural error, as the serial path does.
        std::thread::scope(|scope| {
            let verifier = scope.spawn(|| verify_trailer(hash_algo, data));
            let parsed = Self::parse_body(body, hash_algo, config, parse_context);
            verifier
                .join()
                .map_err(|_| Error::Message("index checksum worker thread panicked".to_owned()))?
                .map_err(trailer_error)?;
            parsed
        })
    }

    /// Parse the index body (header, entries and extensions) without the trailing checksum.
    fn parse_body(
        body: &[u8],
        hash_algo: HashAlgo,
        config: Option<&ConfigSet>,
        parse_context: IndexParseContext,
    ) -> Result<Self> {
        use crate::index_eoie_ieot::{
            index_load_worker_count, parallel_index_load_allowed, parse_entries_parallel,
            parse_entries_serial, parse_index_extensions, read_eoie_extension, read_ieot_extension,
            ParsedIndexExtensions,
        };

        let hash_len = hash_algo.len();
        let magic = &body[..4];
        if magic != b"DIRC" {
            return Err(Error::IndexError("bad magic: expected DIRC".to_owned()));
        }
        let version = u32::from_be_bytes(
            body[4..8]
                .try_into()
                .map_err(|_| Error::IndexError("cannot read version".to_owned()))?,
        );
        if version != 2 && version != 3 && version != 4 {
            return Err(Error::IndexError(format!(
                "unsupported index version {version}"
            )));
        }
        let count = u32::from_be_bytes(
            body[8..12]
                .try_into()
                .map_err(|_| Error::IndexError("cannot read entry count".to_owned()))?,
        );

        let workers = index_load_worker_count(parse_context.thread_policy, config, count as usize);
        let extension_offset = if workers > 1 {
            read_eoie_extension(body, hash_algo)
        } else {
            None
        };

        let eoie_payload = 4 + hash_len;
        let eoie_with_header = 8 + eoie_payload;
        let extensions_end = body
            .len()
            .checked_sub(eoie_with_header)
            .filter(|_| extension_offset.is_some())
            .unwrap_or(body.len());

        let try_parallel = extension_offset.is_some()
            && workers > 1
            && parallel_index_load_allowed(parse_context.thread_policy, config, count as usize);

        let (entries, ext_fields): (Vec<IndexEntry>, ParsedIndexExtensions) = if try_parallel {
            let Some(ext_off) = extension_offset else {
                return Err(Error::IndexError(
                    "parallel index load missing EOIE offset".to_owned(),
                ));
            };
            let ieot = read_ieot_extension(body, ext_off, extensions_end);
            let mut loaded_entries = if let Some(ref ieot) = ieot {
                parse_entries_parallel(body, version, count, hash_len, ieot, workers)?
            } else {
                parse_entries_serial(body, version, count, hash_len, 12)?.0
            };
            let ext_fields =
                parse_index_extensions(body, ext_off, extensions_end, hash_algo, None)?;
            if let Some(dirty) = ext_fields.fsmonitor_dirty.as_ref() {
                apply_fsmonitor_dirty_to_entries(&mut loaded_entries, dirty);
            }
            (loaded_entries, ext_fields)
        } else {
            let (mut entries, pos) = parse_entries_serial(body, version, count, hash_len, 12)?;
            let ext_fields =
                parse_index_extensions(body, pos, body.len(), hash_algo, Some(&mut entries))?;
            (entries, ext_fields)
        };

        Ok(Self {
            version,
            entries,
            entries_sorted: true,
            sparse_directories: ext_fields.sparse_directories,
            untracked_cache: ext_fields.untracked_cache,
            fsmonitor_last_update: ext_fields.fsmonitor_last_update,
            fsmonitor_dirty: ext_fields.fsmonitor_dirty,
            fsmonitor_raw_extension: ext_fields.fsmonitor_raw_extension,
            resolve_undo: ext_fields.resolve_undo,
            split_link: ext_fields.split_link,
            cache_tree_root: ext_fields.cache_tree_root,
            cache_tree: ext_fields.cache_tree,
            hash_algo,
            source_mtime: None,
            defer_untracked_cache_invalidation: false,
            skip_racy_smudge_on_write: false,
            content_verified: HashMap::new(),
            fsmonitor_refresh_ran: false,
            suppress_pre_write_index_housekeeping: false,
        })
    }

    /// Write the index to a file, computing and appending the trailing SHA-1.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] on filesystem errors.
    pub fn write(&mut self, path: &Path) -> Result<()> {
        let git_dir = path.parent();
        let config = git_dir.and_then(|d| {
            ConfigSet::load(&crate::environment::Environment::empty(), Some(d), true).ok()
        });
        let skip_hash = index_skip_hash_for_write(config.as_ref());
        self.write_to_path(path, skip_hash)
    }

    /// Write the index using an already-loaded config snapshot (avoids re-loading the cascade).
    ///
    /// # Errors
    ///
    /// Same as [`Self::write`].
    pub fn write_with_config(&mut self, path: &Path, config: &ConfigSet) -> Result<()> {
        self.write_with_config_and_policy(path, config, crate::parallel::ThreadPolicy::default())
    }

    /// Write the index using config and an explicit thread policy (EOIE/IEOT emission).
    pub fn write_with_config_and_policy(
        &mut self,
        path: &Path,
        config: &ConfigSet,
        policy: crate::parallel::ThreadPolicy,
    ) -> Result<()> {
        let skip_hash = index_skip_hash_for_write(Some(config));
        self.write_to_path_with_config_and_policy(path, skip_hash, Some(config), policy)
    }

    /// Write this index to `path` with an explicit trailing-checksum policy.
    ///
    /// When `skip_hash` is true, the trailing SHA-1 is written as all zeros (Git `index.skipHash`).
    pub fn write_to_path(&mut self, path: &Path, skip_hash: bool) -> Result<()> {
        self.write_to_path_with_config(path, skip_hash, None)
    }

    /// Like [`Self::write_to_path`], but uses `config` for `core.lockfilepid` instead of reloading.
    pub(crate) fn write_to_path_with_config(
        &mut self,
        path: &Path,
        skip_hash: bool,
        config: Option<&ConfigSet>,
    ) -> Result<()> {
        self.write_to_path_with_config_and_policy(
            path,
            skip_hash,
            config,
            crate::parallel::ThreadPolicy::default(),
        )
    }

    pub(crate) fn write_to_path_with_config_and_policy(
        &mut self,
        path: &Path,
        skip_hash: bool,
        config: Option<&ConfigSet>,
        policy: crate::parallel::ThreadPolicy,
    ) -> Result<()> {
        self.write_to_path_with_config_and_lock_policy(path, skip_hash, config, None, policy)
    }

    pub(crate) fn write_to_path_with_config_and_lock_policy(
        &mut self,
        path: &Path,
        skip_hash: bool,
        config: Option<&ConfigSet>,
        existing_lock: Option<&mut IndexLock>,
        policy: crate::parallel::ThreadPolicy,
    ) -> Result<()> {
        self.write_to_path_with_config_and_lock_mode_policy(
            path,
            skip_hash,
            config,
            existing_lock,
            true,
            policy,
        )
    }

    pub(crate) fn write_to_path_with_config_and_lock_mode_policy(
        &mut self,
        path: &Path,
        skip_hash: bool,
        config: Option<&ConfigSet>,
        existing_lock: Option<&mut IndexLock>,
        commit: bool,
        policy: crate::parallel::ThreadPolicy,
    ) -> Result<()> {
        let body = self.serialize_index_body(config, policy)?;

        if let Some(lock) = existing_lock {
            if lock.index_path != path {
                return Err(Error::Message(
                    "index lock path does not match write target".into(),
                ));
            }
            let checksum: Vec<u8> = if skip_hash {
                vec![0u8; self.hash_algo.len()]
            } else {
                hash_index_body(self.hash_algo, &body)
            };
            lock.write_index_contents(&body, &checksum)?;
            if commit {
                lock.commit_replace_index()?;
            }
            return Ok(());
        }

        let tmp_path = path.with_extension("lock");
        let pid_path = pid_path_for_lock(&tmp_path);
        let lockfile_pid_enabled = grit_lockfile_pid_enabled(config, path.parent());

        use crate::lockfile::{LockFile, LockWait};
        let wait = LockWait::files_ref_lock(config);
        let mut lock_file = LockFile::acquire(&tmp_path, wait).map_err(Error::from)?;
        if lockfile_pid_enabled {
            if let Err(e) = lock_file
                .write_holder_pid()
                .map_err(crate::error::Error::from)
            {
                lock_file.rollback();
                return Err(e);
            }
        }
        let wrote_pid_file = lockfile_pid_enabled;
        let (tmp_path, mut lock_file) = lock_file.into_open_file()?;

        if let Err(e) =
            write_index_body_and_trailer(&mut lock_file, self.hash_algo, skip_hash, &body)
        {
            let _ = fs::remove_file(&tmp_path);
            if wrote_pid_file {
                let _ = fs::remove_file(&pid_path);
            }
            return Err(Error::Io(e));
        }
        drop(lock_file);

        if let Err(e) = fs::rename(&tmp_path, path) {
            let _ = fs::remove_file(&tmp_path);
            if wrote_pid_file {
                let _ = fs::remove_file(&pid_path);
            }
            return Err(Error::Io(e));
        }
        if wrote_pid_file {
            let _ = fs::remove_file(&pid_path);
        }
        Ok(())
    }

    fn serialize_index_body(
        &mut self,
        config: Option<&ConfigSet>,
        policy: crate::parallel::ThreadPolicy,
    ) -> Result<Vec<u8>> {
        self.ensure_canonical_order();
        let mut body = Vec::new();
        self.serialize_into_with_options(&mut body, config, policy)?;
        Ok(body)
    }

    #[cfg(test)]
    /// Serialise the index body (without trailing checksum) into `out`.
    ///
    /// Callers must have sorted entries when using format 4 (path compression depends on order).
    pub(crate) fn serialize_into(&self, out: &mut Vec<u8>) -> Result<()> {
        self.serialize_into_with_options(out, None, crate::parallel::ThreadPolicy::default())
    }

    pub(crate) fn serialize_into_with_options(
        &self,
        out: &mut Vec<u8>,
        config: Option<&ConfigSet>,
        policy: crate::parallel::ThreadPolicy,
    ) -> Result<()> {
        use crate::index_eoie_ieot::{
            entries_per_ieot_block, ieot_block_count_for_write, record_end_of_index_entries,
            write_ieot_payload, IeotEntry, IeotTable, INDEX_EXT_EOIE, INDEX_EXT_IEOT,
        };
        let has_extended_flags = self.entries.iter().any(|e| {
            e.flags_extended
                .is_some_and(|fe| IndexEntry::disk_flags_extended(fe) != 0)
        });
        let write_version = if self.version >= 4 {
            4
        } else if has_extended_flags {
            3
        } else if self.version >= 3 {
            2
        } else {
            self.version
        };
        out.reserve(self.estimate_body_capacity(write_version, config));
        // Header
        out.extend_from_slice(b"DIRC");
        out.extend_from_slice(&write_version.to_be_bytes());
        out.extend_from_slice(&(self.entries.len() as u32).to_be_bytes());

        let ieot_blocks = ieot_block_count_for_write(policy, config, self.entries.len());
        let ieot_stride = ieot_blocks.map(|b| entries_per_ieot_block(self.entries.len(), b));
        let mut ieot_table = IeotTable {
            entries: Vec::new(),
        };
        let mut nr_in_block = 0u32;
        let mut block_start_offset = out.len();

        if write_version == 4 {
            let mut previous_path: Vec<u8> = Vec::new();
            for (i, entry) in self.entries.iter().enumerate() {
                if let Some(stride) = ieot_stride {
                    if i > 0 && i % stride == 0 {
                        ieot_table.entries.push(IeotEntry {
                            offset: block_start_offset as u32,
                            nr: nr_in_block,
                        });
                        nr_in_block = 0;
                        // Git `do_write_index`: invalidate first byte so the next entry
                        // encodes with no common prefix, without shrinking `previous_path`.
                        if !previous_path.is_empty() {
                            previous_path[0] = 0;
                        }
                        block_start_offset = out.len();
                    }
                }
                serialize_entry_v4(entry, &mut previous_path, out);
                nr_in_block += 1;
            }
        } else {
            for (i, entry) in self.entries.iter().enumerate() {
                if let Some(stride) = ieot_stride {
                    if i > 0 && i % stride == 0 {
                        ieot_table.entries.push(IeotEntry {
                            offset: block_start_offset as u32,
                            nr: nr_in_block,
                        });
                        nr_in_block = 0;
                        block_start_offset = out.len();
                    }
                }
                serialize_entry(entry, write_version, out);
                nr_in_block += 1;
            }
        }
        if ieot_blocks.is_some() && nr_in_block > 0 {
            ieot_table.entries.push(IeotEntry {
                offset: block_start_offset as u32,
                nr: nr_in_block,
            });
        }

        let extension_offset = out.len();
        let write_eoie = record_end_of_index_entries(config);
        let mut eoie_header_bytes: Option<Vec<u8>> = write_eoie.then(Vec::new);

        if !ieot_table.entries.is_empty() {
            let mut payload = Vec::new();
            write_ieot_payload(&ieot_table, &mut payload);
            append_index_extension(out, &mut eoie_header_bytes, INDEX_EXT_IEOT, &payload);
        }
        if self.sparse_directories {
            append_index_extension(
                out,
                &mut eoie_header_bytes,
                INDEX_EXT_SPARSE_DIRECTORIES,
                &[],
            );
        }
        if let Some(uc) = &self.untracked_cache {
            let payload = untracked_cache::write_untracked_extension(uc);
            append_index_extension(out, &mut eoie_header_bytes, INDEX_EXT_UNTRACKED, &payload);
        }
        self.append_fsmonitor_extension(out, &mut eoie_header_bytes)?;
        if let Some(ru) = &self.resolve_undo {
            let payload = write_resolve_undo_payload(ru);
            if !payload.is_empty() {
                append_index_extension(
                    out,
                    &mut eoie_header_bytes,
                    INDEX_EXT_RESOLVE_UNDO,
                    &payload,
                );
            }
        }
        if let Some(sl) = &self.split_link {
            use crate::ewah_bitmap::EwahBitmap;
            let del = sl
                .delete_bitmap
                .as_ref()
                .cloned()
                .unwrap_or_else(EwahBitmap::new);
            let rep = sl
                .replace_bitmap
                .as_ref()
                .cloned()
                .unwrap_or_else(EwahBitmap::new);
            let payload =
                crate::split_index::serialize_link_extension_payload(&sl.base_oid, &del, &rep);
            append_index_extension(out, &mut eoie_header_bytes, INDEX_EXT_LINK, &payload);
        }
        if let Some(cache_tree) = &self.cache_tree {
            let mut payload = Vec::new();
            serialize_cache_tree_node(cache_tree, &mut payload);
            append_index_extension(out, &mut eoie_header_bytes, INDEX_EXT_CACHE_TREE, &payload);
        }
        if write_eoie {
            let mut eoie_payload = Vec::new();
            eoie_payload.extend_from_slice(&(extension_offset as u32).to_be_bytes());
            let headers = eoie_header_bytes.as_deref().unwrap_or(&[]);
            eoie_payload.extend_from_slice(&self.hash_algo.checksum_digest(headers));
            append_index_extension(out, &mut None, INDEX_EXT_EOIE, &eoie_payload);
        }
        Ok(())
    }

    fn append_fsmonitor_extension(
        &self,
        out: &mut Vec<u8>,
        eoie_headers: &mut Option<Vec<u8>>,
    ) -> Result<()> {
        if let Some(raw) = &self.fsmonitor_raw_extension {
            if self.fsmonitor_raw_still_valid() {
                append_index_extension(out, eoie_headers, INDEX_EXT_FSMONITOR, raw);
                return Ok(());
            }
        }
        let Some(token) = &self.fsmonitor_last_update else {
            return Ok(());
        };
        let dirty = self
            .fsmonitor_dirty
            .as_ref()
            .cloned()
            .unwrap_or_else(|| self.build_fsmonitor_dirty_from_entries());
        if dirty.bit_size > self.fsmonitor_active_entry_count() {
            return Ok(());
        }
        let mut payload = Vec::new();
        payload.extend_from_slice(&FSMONITOR_EXT_VERSION2.to_be_bytes());
        payload.extend_from_slice(token.as_bytes());
        payload.push(0);
        let ewah_size_fixup = payload.len();
        payload.extend_from_slice(&0u32.to_be_bytes());
        let ewah_start = payload.len();
        dirty.serialize(&mut payload);
        let ewah_size = (payload.len() - ewah_start) as u32;
        payload[ewah_size_fixup..ewah_size_fixup + 4].copy_from_slice(&ewah_size.to_be_bytes());
        append_index_extension(out, eoie_headers, INDEX_EXT_FSMONITOR, &payload);
        Ok(())
    }

    /// Number of index rows counted by the fsmonitor dirty bitmap (Git `cache_nr` minus `CE_REMOVE`).
    fn fsmonitor_active_entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Build the fsmonitor dirty EWAH from in-memory `CE_FSMONITOR_VALID` (`fill_fsmonitor_bitmap`).
    fn build_fsmonitor_dirty_from_entries(&self) -> crate::ewah_bitmap::EwahBitmap {
        use crate::ewah_bitmap::EwahBitmap;
        let mut dirty = EwahBitmap::new();
        if self.fsmonitor_last_update.is_none() {
            return dirty;
        }
        #[cfg(test)]
        tests::fsmonitor_rebuild_counter::note_build();
        for (i, entry) in self.entries.iter().enumerate() {
            if !entry.fsmonitor_valid() {
                dirty.set_bit_extend(i);
            }
        }
        dirty
    }

    /// Whether the on-disk `FSMN` payload still matches token and fsmonitor-valid state.
    fn fsmonitor_raw_still_valid(&self) -> bool {
        use crate::ewah_bitmap::ewah_bitmaps_equal;
        let Some(raw) = self.fsmonitor_raw_extension.as_ref() else {
            return false;
        };
        let Some(token) = self.fsmonitor_last_update.as_deref() else {
            return false;
        };
        let Ok((raw_token, raw_dirty)) = parse_fsmonitor_extension(raw) else {
            return false;
        };
        if raw_token != token {
            return false;
        }
        let current = self.build_fsmonitor_dirty_from_entries();
        if !ewah_bitmaps_equal(&raw_dirty, &current) {
            return false;
        }
        current.bit_size <= self.fsmonitor_active_entry_count()
    }

    /// Rebuild the in-memory fsmonitor dirty bitmap from `CE_FSMONITOR_VALID` (`fill_fsmonitor_bitmap`).
    fn rebuild_fsmonitor_dirty_from_entries(&mut self) {
        if self.fsmonitor_last_update.is_none() {
            self.fsmonitor_dirty = None;
            return;
        }
        self.fsmonitor_dirty = Some(self.build_fsmonitor_dirty_from_entries());
    }

    /// Drop cached and on-disk fsmonitor snapshots; rebuild on the next read or write.
    fn invalidate_fsmonitor_bitmap(&mut self) {
        if self.fsmonitor_last_update.is_none() {
            return;
        }
        self.fsmonitor_raw_extension = None;
        self.fsmonitor_dirty = None;
    }

    /// Update the fsmonitor token; clears any stale on-disk snapshot when the token changes.
    pub fn set_fsmonitor_last_update(&mut self, token: Option<String>) {
        if self.fsmonitor_last_update.as_deref() != token.as_deref() {
            self.fsmonitor_raw_extension = None;
        }
        self.fsmonitor_last_update = token;
        self.rebuild_fsmonitor_dirty_from_entries();
    }

    /// Remove fsmonitor metadata from the index (extension and in-memory valid bits are cleared on write).
    pub fn clear_fsmonitor_extension(&mut self) {
        self.fsmonitor_last_update = None;
        self.fsmonitor_dirty = None;
        self.fsmonitor_raw_extension = None;
    }

    /// Refresh the in-memory fsmonitor EWAH from entry valid bits (does not invalidate raw).
    pub fn sync_fsmonitor_dirty_bitmap(&mut self) {
        self.rebuild_fsmonitor_dirty_from_entries();
    }

    /// Call after mutating entry fsmonitor-valid bits without changing index membership.
    pub fn fsmonitor_validity_changed(&mut self) {
        self.invalidate_fsmonitor_bitmap();
    }

    /// Clear fsmonitor-valid on stage-0 entries without disturbing sort order.
    pub(crate) fn invalidate_all_fsmonitor_valid(&mut self) {
        for entry in &mut self.entries {
            if entry.stage() == 0 {
                entry.set_fsmonitor_valid(false);
            }
        }
        self.fsmonitor_validity_changed();
    }

    /// Invalidate fsmonitor-valid bits for one hook-reported path (Git `fsmonitor_refresh_callback`).
    pub(crate) fn invalidate_fsmonitor_for_path(&mut self, path: &str, ignorecase: bool) {
        if let Some(uc) = self.untracked_cache.as_mut() {
            crate::untracked_cache::invalidate_path(uc, path);
        }
        let mut invalidated = if path.ends_with('/') {
            self.invalidate_fsmonitor_directory_cone(path.trim_end_matches('/'), ignorecase)
        } else {
            self.invalidate_fsmonitor_unqualified_path(path, ignorecase)
        };
        if !invalidated && ignorecase {
            invalidated = self.invalidate_fsmonitor_icase_fallback(path);
        }
        if invalidated {
            self.fsmonitor_validity_changed();
        }
    }

    fn invalidate_fsmonitor_unqualified_path(&mut self, path: &str, ignorecase: bool) -> bool {
        if let Some(entry) = self.get_mut(path.as_bytes(), 0) {
            entry.set_fsmonitor_valid(false);
            return true;
        }
        if self.has_stage0_descendant_under(path) {
            return self.invalidate_fsmonitor_directory_cone(path, ignorecase);
        }
        false
    }

    fn invalidate_fsmonitor_directory_cone(&mut self, prefix: &str, ignorecase: bool) -> bool {
        if prefix.is_empty() {
            self.invalidate_all_fsmonitor_valid();
            return true;
        }
        let prefix_slash = format!("{prefix}/");
        let mut any = false;
        for entry in &mut self.entries {
            if entry.stage() != 0 {
                continue;
            }
            let Some(rel) = std::str::from_utf8(&entry.path).ok() else {
                continue;
            };
            if Self::fsmonitor_path_under_prefix(rel, prefix, &prefix_slash, ignorecase) {
                entry.set_fsmonitor_valid(false);
                any = true;
            }
        }
        any
    }

    fn invalidate_fsmonitor_icase_fallback(&mut self, path: &str) -> bool {
        let is_dir = path.ends_with('/');
        let prefix = path.trim_end_matches('/');
        let prefix_slash = format!("{prefix}/");
        let mut any = false;
        for entry in &mut self.entries {
            if entry.stage() != 0 {
                continue;
            }
            let Some(rel) = std::str::from_utf8(&entry.path).ok() else {
                continue;
            };
            let matches = if is_dir {
                Self::fsmonitor_path_under_prefix(rel, prefix, &prefix_slash, true)
            } else {
                rel.eq_ignore_ascii_case(prefix)
            };
            if matches {
                entry.set_fsmonitor_valid(false);
                any = true;
            }
        }
        any
    }

    fn fsmonitor_path_under_prefix(
        rel: &str,
        prefix: &str,
        prefix_slash: &str,
        ignorecase: bool,
    ) -> bool {
        if ignorecase {
            rel.eq_ignore_ascii_case(prefix)
                || (rel.len() > prefix.len()
                    && rel.as_bytes().get(prefix.len()) == Some(&b'/')
                    && rel[..prefix.len()].eq_ignore_ascii_case(prefix))
        } else {
            rel == prefix || rel.starts_with(prefix_slash)
        }
    }

    /// Drop byte-for-byte `FSMN` preservation after index entries change.
    fn touch_fsmonitor_after_entry_change(&mut self) {
        self.invalidate_fsmonitor_bitmap();
    }

    /// Read-only view of index entries.
    #[must_use]
    pub fn entries(&self) -> &[IndexEntry] {
        &self.entries
    }

    /// Mutable entry list; marks the index unsorted until [`Self::sort`].
    pub fn entries_mut(&mut self) -> &mut Vec<IndexEntry> {
        self.entries_sorted = false;
        &mut self.entries
    }

    /// Mark `entries` as not in canonical `(path, stage)` order after direct vector mutation.
    pub fn mark_entries_unsorted(&mut self) {
        self.entries_sorted = false;
    }

    /// Append an entry without maintaining sort order (used while building a merge index).
    pub(crate) fn push_entry_unsorted(&mut self, entry: IndexEntry) {
        self.entries.push(entry);
        self.entries_sorted = false;
    }

    /// Whether [`Self::entries`] are in Git canonical `(path, stage)` order (full scan).
    #[must_use]
    pub fn entries_are_sorted(&self) -> bool {
        self.entries
            .windows(2)
            .all(|pair| (&pair[0].path, pair[0].stage()) <= (&pair[1].path, pair[1].stage()))
    }

    /// Compare two index paths (counts comparisons in unit tests).
    fn cmp_paths(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
        record_path_cmp_for_tests();
        a.cmp(b)
    }

    fn entry_matches(entry: &IndexEntry, path: &[u8], stage: u8) -> bool {
        entry.path == path && entry.stage() == stage
    }

    /// Index of `(path, stage)`; binary search when sorted, linear scan when not.
    pub(crate) fn find_entry_pos(&self, path: &[u8], stage: u8) -> Option<usize> {
        if self.entries_sorted {
            return match self.entries.binary_search_by(|e| {
                Self::cmp_paths(e.path.as_slice(), path).then_with(|| e.stage().cmp(&stage))
            }) {
                Ok(pos) if Self::entry_matches(&self.entries[pos], path, stage) => Some(pos),
                _ => None,
            };
        }
        self.entries
            .iter()
            .position(|e| Self::entry_matches(e, path, stage))
    }

    /// Inclusive start and exclusive end of all entries sharing `path` (sorted indexes only).
    fn path_run_range_sorted(&self, path: &[u8]) -> (usize, usize) {
        debug_assert!(self.entries_sorted);
        let start = self
            .entries
            .partition_point(|e| Self::cmp_paths(e.path.as_slice(), path).is_lt());
        let end = start
            + self.entries[start..]
                .iter()
                .take_while(|e| e.path == path)
                .count();
        (start, end)
    }

    /// Whether any stage-0 entry lies strictly under `rel_dir/` (or any stage-0 entry exists when
    /// `rel_dir` is empty).
    #[must_use]
    pub fn has_stage0_descendant_under(&self, rel_dir: &str) -> bool {
        if rel_dir.is_empty() {
            return self.entries.iter().any(|e| e.stage() == 0);
        }
        if self.entries_sorted {
            let (start, end) = self.descendant_run_range_sorted(rel_dir);
            return self.entries[start..end].iter().any(|e| e.stage() == 0);
        }
        let prefix = format!("{rel_dir}/");
        self.entries
            .iter()
            .any(|e| e.stage() == 0 && e.path.starts_with(prefix.as_bytes()))
    }

    /// Inclusive start and exclusive end of entries strictly under `path/` (sorted indexes only).
    fn descendant_run_range_sorted(&self, path: &str) -> (usize, usize) {
        debug_assert!(self.entries_sorted);
        let prefix = path.as_bytes();
        let plen = prefix.len();
        let mut prefix_slash = Vec::with_capacity(plen + 1);
        prefix_slash.extend_from_slice(prefix);
        prefix_slash.push(b'/');
        let start = self.entries.partition_point(|e| {
            Self::cmp_paths(e.path.as_slice(), prefix_slash.as_slice()).is_lt()
        });
        let mut end = start;
        while end < self.entries.len() {
            let ep = self.entries[end].path.as_slice();
            if ep.len() > plen && ep.starts_with(prefix) && ep[plen] == b'/' {
                end += 1;
            } else {
                break;
            }
        }
        (start, end)
    }

    fn record_resolve_undo_for_path(&mut self, path: &[u8]) {
        for e in &self.entries {
            if e.path == path && e.stage() != 0 {
                resolve_undo::record_resolve_undo_for_entry(&mut self.resolve_undo, e);
            }
        }
    }

    fn record_resolve_undo_for_run(&mut self, start: usize, end: usize) {
        for e in &self.entries[start..end] {
            if e.stage() != 0 {
                resolve_undo::record_resolve_undo_for_entry(&mut self.resolve_undo, e);
            }
        }
    }

    /// Drop derived `TREE` / untracked-cache extensions before a large checkout rewrite.
    pub(crate) fn drop_derived_index_extensions(&mut self) {
        self.cache_tree = None;
        self.cache_tree_root = None;
        self.untracked_cache = None;
    }

    /// Partially invalidate cache-tree nodes for a bulk checkout without dropping the whole extension.
    pub(crate) fn invalidate_cache_tree_for_bulk_checkout<'a>(
        &mut self,
        dir_prefixes: &BTreeSet<String>,
        paths: impl IntoIterator<Item = &'a [u8]>,
        replacements: &[IndexEntry],
    ) {
        if self.cache_tree.is_none() {
            return;
        }
        for dir in dir_prefixes {
            self.invalidate_cache_tree_for_path(dir.as_bytes());
        }
        for path in paths {
            self.invalidate_cache_tree_for_path(path);
        }
        for entry in replacements {
            self.invalidate_cache_tree_for_path(&entry.path);
        }
    }

    fn invalidate_after_path_removal(&mut self, path: &[u8]) {
        if let Ok(p) = std::str::from_utf8(path) {
            self.invalidate_untracked_cache_for_path(p);
        }
        self.invalidate_cache_tree_for_path(path);
    }

    /// One index compaction pass: drop entries under `dir_prefixes`, drop exact `paths`, merge `replacements`.
    pub(crate) fn remove_prefixes_paths_and_insert<'a>(
        &mut self,
        dir_prefixes: &BTreeSet<String>,
        paths: impl IntoIterator<Item = &'a [u8]>,
        replacements: impl IntoIterator<Item = IndexEntry>,
    ) {
        let mut replacements: Vec<IndexEntry> = replacements.into_iter().collect();
        if !replacements.is_empty() {
            replacements
                .sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.stage().cmp(&b.stage())));
            dedup_sorted_replacement_entries(&mut replacements);
        }

        let mut paths: Vec<&[u8]> = paths.into_iter().collect();
        paths.sort_unstable_by(|a, b| Self::cmp_paths(a, b));
        paths.dedup_by(|a, b| a == b);

        let mut prefix_bytes: Vec<&[u8]> = dir_prefixes.iter().map(|d| d.as_bytes()).collect();
        prefix_bytes.sort_unstable_by(|a, b| Self::cmp_paths(a, b));
        prefix_bytes.dedup_by(|a, b| a == b);

        if dir_prefixes.is_empty() && paths.is_empty() {
            self.merge_replacements_into_sorted_index(replacements);
            return;
        }

        let mut invalidated_paths: Vec<Vec<u8>> = Vec::new();
        let mut kept = Vec::with_capacity(self.entries.len());
        let skip_per_path_extension_inval = self.defer_untracked_cache_invalidation;

        if self.entries_sorted {
            let mut path_idx = 0usize;
            for entry in self.entries.drain(..) {
                let ep = entry.path.as_slice();
                while path_idx < paths.len() && Self::cmp_paths(paths[path_idx], ep).is_lt() {
                    path_idx += 1;
                }
                let remove = (path_idx < paths.len()
                    && Self::cmp_paths(paths[path_idx], ep).is_eq())
                    || Self::entry_under_any_directory_prefix(ep, &prefix_bytes);
                if remove {
                    if entry.stage() != 0 {
                        resolve_undo::record_resolve_undo_for_entry(&mut self.resolve_undo, &entry);
                    }
                    if !skip_per_path_extension_inval
                        && invalidated_paths.last().is_none_or(|p| p.as_slice() != ep)
                    {
                        invalidated_paths.push(entry.path.clone());
                    }
                } else {
                    kept.push(entry);
                }
            }
            let repl_for_inval: Vec<Vec<u8>> = if skip_per_path_extension_inval {
                Vec::new()
            } else {
                replacements
                    .iter()
                    .filter(|e| e.stage() == 0)
                    .map(|e| e.path.clone())
                    .collect()
            };
            self.entries = merge_sorted_index_entries(kept, replacements);
            self.entries_sorted = true;
            if skip_per_path_extension_inval {
                self.untracked_cache = None;
                self.touch_fsmonitor_after_entry_change();
                return;
            }
            const BULK_INDEX_EXTENSION_INVALIDATE_MIN: usize = 64;
            let mut invalidations = dir_prefixes.len();
            for path in &invalidated_paths {
                if dir_prefixes
                    .iter()
                    .all(|d| !Self::entry_under_directory_prefix(path.as_slice(), d.as_bytes()))
                {
                    invalidations += 1;
                }
            }
            invalidations += repl_for_inval.len();
            if invalidations >= BULK_INDEX_EXTENSION_INVALIDATE_MIN {
                self.drop_derived_index_extensions();
            } else {
                for dir in dir_prefixes {
                    self.invalidate_after_path_removal(dir.as_bytes());
                }
                for path in invalidated_paths {
                    if dir_prefixes
                        .iter()
                        .all(|d| !Self::entry_under_directory_prefix(path.as_slice(), d.as_bytes()))
                    {
                        self.invalidate_after_path_removal(&path);
                    }
                }
                for path in repl_for_inval {
                    if let Ok(p) = std::str::from_utf8(&path) {
                        self.invalidate_untracked_cache_for_path(p);
                    }
                    self.invalidate_cache_tree_for_path(&path);
                }
            }
            self.touch_fsmonitor_after_entry_change();
            return;
        }
        if !dir_prefixes.is_empty() {
            self.remove_directory_prefixes(dir_prefixes);
        }
        self.remove_paths_and_insert(paths, replacements);
    }

    /// Remove every entry whose path equals `dir` or lies under `dir/` for each `dir` in `dirs`.
    pub(crate) fn remove_directory_prefixes(&mut self, dirs: &BTreeSet<String>) {
        if dirs.is_empty() {
            return;
        }
        let mut prefixes: Vec<&[u8]> = dirs.iter().map(|d| d.as_bytes()).collect();
        prefixes.sort_unstable_by(|a, b| Self::cmp_paths(a, b));
        prefixes.dedup_by(|a, b| a == b);

        let mut invalidated_paths: Vec<Vec<u8>> = Vec::new();
        let mut kept = Vec::with_capacity(self.entries.len());

        if self.entries_sorted {
            for entry in self.entries.drain(..) {
                let ep = entry.path.as_slice();
                let remove = Self::entry_under_any_directory_prefix(ep, &prefixes);
                if remove {
                    if entry.stage() != 0 {
                        resolve_undo::record_resolve_undo_for_entry(&mut self.resolve_undo, &entry);
                    }
                    if invalidated_paths.last().is_none_or(|p| p.as_slice() != ep) {
                        invalidated_paths.push(entry.path.clone());
                    }
                } else {
                    kept.push(entry);
                }
            }
            self.entries = kept;
        } else {
            for entry in self.entries.drain(..) {
                let remove =
                    Self::entry_under_any_directory_prefix(entry.path.as_slice(), &prefixes);
                if remove {
                    if entry.stage() != 0 {
                        resolve_undo::record_resolve_undo_for_entry(&mut self.resolve_undo, &entry);
                    }
                    if invalidated_paths
                        .last()
                        .is_none_or(|p| p.as_slice() != entry.path)
                    {
                        invalidated_paths.push(entry.path.clone());
                    }
                } else {
                    kept.push(entry);
                }
            }
            self.entries = kept;
        }
        if invalidated_paths.is_empty() {
            return;
        }
        for dir in dirs {
            self.invalidate_after_path_removal(dir.as_bytes());
        }
        self.touch_fsmonitor_after_entry_change();
    }

    fn entry_under_directory_prefix(path: &[u8], prefix: &[u8]) -> bool {
        if path.len() < prefix.len() {
            return false;
        }
        if path[..prefix.len()] != *prefix {
            return false;
        }
        path.len() == prefix.len() || path[prefix.len()] == b'/'
    }

    fn entry_under_any_directory_prefix(path: &[u8], prefixes: &[&[u8]]) -> bool {
        prefixes
            .iter()
            .any(|prefix| Self::entry_under_directory_prefix(path, prefix))
    }

    /// Remove many paths (all merge stages per path) in one `O(N + M log M)` compaction pass.
    pub fn remove_paths<'a>(&mut self, paths: impl IntoIterator<Item = &'a [u8]>) {
        use std::collections::BTreeSet;

        let mut paths: Vec<&[u8]> = paths.into_iter().collect();
        if paths.is_empty() {
            return;
        }
        paths.sort_unstable_by(|a, b| Self::cmp_paths(a, b));
        paths.dedup_by(|a, b| a == b);

        let mut invalidated_paths: Vec<Vec<u8>> = Vec::new();
        let mut kept = Vec::with_capacity(self.entries.len());

        if self.entries_sorted {
            let mut path_idx = 0usize;
            for entry in self.entries.drain(..) {
                let ep = entry.path.as_slice();
                while path_idx < paths.len() && Self::cmp_paths(paths[path_idx], ep).is_lt() {
                    path_idx += 1;
                }
                let remove = path_idx < paths.len() && Self::cmp_paths(paths[path_idx], ep).is_eq();
                if remove {
                    if entry.stage() != 0 {
                        resolve_undo::record_resolve_undo_for_entry(&mut self.resolve_undo, &entry);
                    }
                    if invalidated_paths.last().is_none_or(|p| p.as_slice() != ep) {
                        invalidated_paths.push(entry.path.clone());
                    }
                } else {
                    kept.push(entry);
                }
            }
            self.entries = kept;
        } else {
            let remove_set: BTreeSet<&[u8]> = paths.into_iter().collect();
            for entry in self.entries.drain(..) {
                if remove_set.contains(entry.path.as_slice()) {
                    if entry.stage() != 0 {
                        resolve_undo::record_resolve_undo_for_entry(&mut self.resolve_undo, &entry);
                    }
                    if invalidated_paths
                        .last()
                        .is_none_or(|p| p.as_slice() != entry.path.as_slice())
                    {
                        invalidated_paths.push(entry.path.clone());
                    }
                } else {
                    kept.push(entry);
                }
            }
            self.entries = kept;
        }

        for path in invalidated_paths {
            self.invalidate_after_path_removal(&path);
        }
        self.touch_fsmonitor_after_entry_change();
    }

    /// Replace many stage-0 entries in one sorted merge (bulk `git add` restage).
    pub(crate) fn replace_stage0_entries_batch(&mut self, replacements: Vec<IndexEntry>) {
        self.merge_replacements_into_sorted_index(replacements);
    }

    /// Record that the worktree file at `path` was just hashed to `oid` (see
    /// [`Self::content_verified`]), so the next write does not re-read it for the racy check.
    pub(crate) fn mark_content_verified(&mut self, path: &[u8], oid: ObjectId) {
        self.content_verified.insert(path.to_vec(), oid);
    }

    /// Drop every just-hashed mark: the [`Self::content_verified`] map and the per-entry bits.
    pub(crate) fn clear_content_verified(&mut self) {
        self.content_verified.clear();
        for entry in &mut self.entries {
            entry.take_content_verified();
        }
    }

    /// Overwrite the stage-0 rows that already hold each replacement's path, in place.
    ///
    /// Same result and cache invalidation as [`Self::replace_stage0_entries_batch`], without
    /// growing `entries` to merge: restaging only changes rows, never their order. Replacements
    /// with no stage-0 row at their path (or not at stage 0) go through the sorted merge.
    pub(crate) fn overwrite_stage0_entries(&mut self, replacements: Vec<IndexEntry>) {
        const BULK_INVALIDATE_MIN: usize = 64;
        if replacements.is_empty() {
            return;
        }
        let defer_untracked = self.defer_untracked_cache_invalidation;
        let drop_caches = !defer_untracked && replacements.len() >= BULK_INVALIDATE_MIN;
        let mut unmatched = Vec::new();
        for mut entry in replacements {
            let pos = if entry.stage() == 0 {
                self.find_entry_pos(&entry.path, 0)
            } else {
                None
            };
            let Some(pos) = pos else {
                unmatched.push(entry);
                continue;
            };
            if !drop_caches {
                if !defer_untracked {
                    if let Ok(p) = std::str::from_utf8(&entry.path) {
                        self.invalidate_untracked_cache_for_path(p);
                    }
                }
                self.invalidate_cache_tree_for_path(&entry.path);
            }
            entry.base_index_pos = self.entries[pos].base_index_pos;
            self.entries[pos] = entry;
        }
        if drop_caches {
            self.cache_tree = None;
            self.cache_tree_root = None;
            self.untracked_cache = None;
        }
        self.touch_fsmonitor_after_entry_change();
        if !unmatched.is_empty() {
            self.merge_replacements_into_sorted_index(unmatched);
        }
    }

    /// Append sorted `replacements` to a sorted index and merge in one linear pass.
    fn merge_sorted_replacements_into_entries(&mut self, mut replacements: Vec<IndexEntry>) {
        if replacements.is_empty() {
            return;
        }
        if self.entries.is_empty() {
            self.entries = replacements;
            return;
        }
        self.entries.reserve(replacements.len());
        let mid = self.entries.len();
        self.entries.append(&mut replacements);
        merge_sorted_index_adjacent_halves(&mut self.entries, mid);
    }

    /// Merge sorted `replacements` into a sorted index in one linear pass.
    fn merge_replacements_into_sorted_index(&mut self, replacements: Vec<IndexEntry>) {
        if replacements.is_empty() {
            return;
        }
        if !self.entries_sorted {
            for entry in replacements {
                self.add_or_replace(entry);
            }
            return;
        }
        const BULK_MERGE_MIN: usize = 64;
        if self.defer_untracked_cache_invalidation {
            // Campaign-scale tracked restage: merge in bulk but keep UNTR and invalidate only
            // touched cache-tree paths (issue #1184).
            for entry in replacements.iter().filter(|e| e.stage() == 0) {
                self.invalidate_cache_tree_for_path(&entry.path);
            }
            self.merge_sorted_replacements_into_entries(replacements);
            self.touch_fsmonitor_after_entry_change();
            return;
        }
        let repl_for_inval: Vec<Vec<u8>> = replacements
            .iter()
            .filter(|e| e.stage() == 0)
            .map(|e| e.path.clone())
            .collect();
        self.merge_sorted_replacements_into_entries(replacements);
        if repl_for_inval.len() >= BULK_MERGE_MIN {
            self.cache_tree = None;
            self.cache_tree_root = None;
            self.untracked_cache = None;
            self.touch_fsmonitor_after_entry_change();
            return;
        }
        for path in repl_for_inval {
            if let Ok(p) = std::str::from_utf8(&path) {
                self.invalidate_untracked_cache_for_path(p);
            }
            self.invalidate_cache_tree_for_path(&path);
        }
        self.touch_fsmonitor_after_entry_change();
    }

    /// Remove every entry at `paths` (all stages), then merge `replacements` in sorted order.
    ///
    /// When the index is already sorted, this is one removal scan plus an `O(N + M)` merge,
    /// avoiding per-path `Vec::drain` shifts and repeated `insert`/`sort` work.
    pub fn remove_paths_and_insert<'a>(
        &mut self,
        paths: impl IntoIterator<Item = &'a [u8]>,
        replacements: impl IntoIterator<Item = IndexEntry>,
    ) {
        let mut replacements: Vec<IndexEntry> = replacements.into_iter().collect();
        if replacements.is_empty() {
            self.remove_paths(paths);
            return;
        }
        replacements.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.stage().cmp(&b.stage())));
        dedup_sorted_replacement_entries(&mut replacements);

        let mut paths: Vec<&[u8]> = paths.into_iter().collect();
        if paths.is_empty() {
            self.merge_replacements_into_sorted_index(replacements);
            return;
        }
        paths.sort_unstable_by(|a, b| Self::cmp_paths(a, b));
        paths.dedup_by(|a, b| a == b);

        let mut invalidated_paths: Vec<Vec<u8>> = Vec::new();
        let mut kept = Vec::with_capacity(self.entries.len());

        if self.entries_sorted {
            let mut path_idx = 0usize;
            for entry in self.entries.drain(..) {
                let ep = entry.path.as_slice();
                while path_idx < paths.len() && Self::cmp_paths(paths[path_idx], ep).is_lt() {
                    path_idx += 1;
                }
                let remove = path_idx < paths.len() && Self::cmp_paths(paths[path_idx], ep).is_eq();
                if remove {
                    if entry.stage() != 0 {
                        resolve_undo::record_resolve_undo_for_entry(&mut self.resolve_undo, &entry);
                    }
                    if invalidated_paths.last().is_none_or(|p| p.as_slice() != ep) {
                        invalidated_paths.push(entry.path.clone());
                    }
                } else {
                    kept.push(entry);
                }
            }
            let repl_for_inval = replacements
                .iter()
                .filter(|e| e.stage() == 0)
                .map(|e| e.path.clone())
                .collect::<Vec<_>>();
            self.entries = merge_sorted_index_entries(kept, replacements);
            self.entries_sorted = true;
            const BULK_INDEX_EXTENSION_INVALIDATE_MIN: usize = 64;
            let invalidations = invalidated_paths.len() + repl_for_inval.len();
            if invalidations >= BULK_INDEX_EXTENSION_INVALIDATE_MIN {
                self.drop_derived_index_extensions();
            } else {
                for path in invalidated_paths {
                    self.invalidate_after_path_removal(&path);
                }
                for path in repl_for_inval {
                    if let Ok(p) = std::str::from_utf8(&path) {
                        self.invalidate_untracked_cache_for_path(p);
                    }
                    self.invalidate_cache_tree_for_path(&path);
                }
            }
            self.touch_fsmonitor_after_entry_change();
            return;
        }
        self.remove_paths(paths);
        for entry in replacements {
            self.add_or_replace(entry);
        }
    }

    /// Add or replace an entry (matched by path + stage).
    pub fn add_or_replace(&mut self, entry: IndexEntry) {
        let path = entry.path.clone();
        let stage = entry.stage();
        let mut inserted_stage0 = false;
        if self.entries_sorted {
            let result = self.entries.binary_search_by(|e| {
                e.path
                    .as_slice()
                    .cmp(path.as_slice())
                    .then_with(|| e.stage().cmp(&stage))
            });
            match result {
                Ok(pos) => {
                    let mut e = entry;
                    e.base_index_pos = self.entries[pos].base_index_pos;
                    self.entries[pos] = e;
                }
                Err(pos) => {
                    self.entries.insert(pos, entry);
                    inserted_stage0 = stage == 0;
                }
            }
        } else if let Some(pos) = self
            .entries
            .iter()
            .position(|e| e.path == path && e.stage() == stage)
        {
            let mut e = entry;
            e.base_index_pos = self.entries[pos].base_index_pos;
            self.entries[pos] = e;
        } else {
            self.entries.push(entry);
            inserted_stage0 = stage == 0;
        }
        if inserted_stage0 {
            if let Ok(p) = std::str::from_utf8(&path) {
                self.invalidate_untracked_cache_for_path(p);
            }
        }
        if stage == 0 {
            self.invalidate_cache_tree_for_path(&path);
        }
        self.touch_fsmonitor_after_entry_change();
    }

    /// Returns whether the index contains any unmerged (stage 1/2/3) entries.
    #[must_use]
    pub fn has_unmerged_entries(&self) -> bool {
        self.entries.iter().any(|e| e.stage() != 0)
    }

    /// Stage a file at stage 0, removing any conflict stage entries (1, 2, 3)
    /// for the same path. This is the correct behavior for `git add` on a
    /// conflicted file during merge/cherry-pick resolution.
    pub fn stage_file(&mut self, entry: IndexEntry) {
        let path = entry.path.clone();
        if self.entries_sorted {
            let (start, end) = self.path_run_range_sorted(&path);
            for i in (start..end).rev() {
                if self.entries[i].stage() != 0 {
                    resolve_undo::record_resolve_undo_for_entry(
                        &mut self.resolve_undo,
                        &self.entries[i],
                    );
                    self.entries.remove(i);
                }
            }
        } else {
            for e in &self.entries {
                if e.path == path && e.stage() != 0 {
                    resolve_undo::record_resolve_undo_for_entry(&mut self.resolve_undo, e);
                }
            }
            self.entries.retain(|e| e.path != path || e.stage() == 0);
        }
        self.add_or_replace(entry);
    }

    /// Drop all resolve-undo records (matches Git `resolve_undo_clear_index`).
    pub fn clear_resolve_undo(&mut self) {
        self.resolve_undo = None;
    }

    /// Remove and return the resolve-undo record for `path`, if any.
    pub fn take_resolve_undo_record(&mut self, path: &[u8]) -> Option<ResolveUndoRecord> {
        let map = self.resolve_undo.as_mut()?;
        let ru = map.remove(path)?;
        if map.is_empty() {
            self.resolve_undo = None;
        }
        Some(ru)
    }

    /// Replace all index entries for `path` with unmerged stages from `record`.
    pub fn install_unmerged_from_resolve_undo(&mut self, path: &[u8], record: &ResolveUndoRecord) {
        if self.entries_sorted {
            let (start, end) = self.path_run_range_sorted(path);
            if start < end {
                self.entries.drain(start..end);
            }
        } else {
            self.entries.retain(|e| e.path != path);
        }
        for stage in 1u8..=3u8 {
            let i = (stage - 1) as usize;
            if record.modes[i] == 0 {
                continue;
            }
            let entry = IndexEntry {
                ctime_sec: 0,
                ctime_nsec: 0,
                mtime_sec: 0,
                mtime_nsec: 0,
                dev: 0,
                ino: 0,
                mode: record.modes[i],
                uid: 0,
                gid: 0,
                size: 0,
                oid: record.oids[i],
                flags: path.len().min(0xFFF) as u16 | ((stage as u16) << 12),
                flags_extended: None,
                path: path.to_vec(),
                base_index_pos: 0,
            };
            self.add_or_replace(entry);
        }
    }

    /// Re-create unmerged index entries for `path` from the resolve-undo extension.
    ///
    /// Returns `true` when a resolve-undo record existed and was consumed (Git `unmerge_one`).
    pub fn unmerge_path_from_resolve_undo(&mut self, path: &[u8]) -> bool {
        let Some(record) = self.take_resolve_undo_record(path) else {
            return false;
        };
        self.install_unmerged_from_resolve_undo(path, &record);
        // Git's `unmerge_index_entry` swaps the resolved stage-0 entry for unmerged
        // stages 1/2/3 via `remove_index_entry_at` + `add_index_entry`, both of which
        // call `cache_tree_invalidate_path`. After committing the resolution the index
        // carries a valid TREE extension whose stage-0 entry for `path` no longer
        // matches; without invalidating it, the next write-tree trips
        // `verify_cache_tree` ("corrupted cache-tree has entries not present in index").
        if let Ok(p) = std::str::from_utf8(path) {
            self.invalidate_untracked_cache_for_path(p);
        }
        self.invalidate_cache_tree_for_path(path);
        true
    }

    /// Remove all entries matching the given path (all stages).
    ///
    /// Returns `true` if at least one entry was removed.
    pub fn remove(&mut self, path: &[u8]) -> bool {
        if self.entries_sorted {
            let (start, end) = self.path_run_range_sorted(path);
            if start < end {
                self.record_resolve_undo_for_run(start, end);
                self.entries.drain(start..end);
                self.invalidate_after_path_removal(path);
                self.touch_fsmonitor_after_entry_change();
                return true;
            }
            if !self.entries.iter().any(|e| e.path == path) {
                return false;
            }
            self.entries_sorted = false;
        }
        self.record_resolve_undo_for_path(path);
        let before = self.entries.len();
        self.entries.retain(|e| e.path != path);
        if self.entries.len() == before {
            return false;
        }
        self.invalidate_after_path_removal(path);
        self.touch_fsmonitor_after_entry_change();
        true
    }

    /// Remove every index entry for `path` (all merge stages), like `remove_file_from_index`.
    ///
    /// Returns whether any entry was removed.
    pub fn remove_path_all_stages(&mut self, path: &[u8]) -> bool {
        self.remove(path)
    }

    /// Invalidate UNTR nodes affected by an index change (Git `untracked_cache_*_index`).
    pub fn invalidate_untracked_cache_for_path(&mut self, path: &str) {
        if self.defer_untracked_cache_invalidation {
            return;
        }
        if let Some(uc) = self.untracked_cache.as_mut() {
            untracked_cache::invalidate_path(uc, path);
        }
    }

    /// Remove every index entry whose path lies strictly under `path` (all stages).
    ///
    /// Used when staging a file at `path` that replaces a former directory: Git removes
    /// tracked paths like `path/child` from the index so they do not remain alongside
    /// the new blob entry.
    pub fn remove_descendants_under_path(&mut self, path: &str) {
        let prefix = path.as_bytes();
        if prefix.is_empty() {
            return;
        }
        let plen = prefix.len();
        if self.entries_sorted {
            let (start, end) = self.descendant_run_range_sorted(path);
            if start < end {
                self.record_resolve_undo_for_run(start, end);
                self.entries.drain(start..end);
                self.invalidate_untracked_cache_for_path(path);
                self.invalidate_cache_tree_for_path(path.as_bytes());
                return;
            }
            let any = self.entries.iter().any(|e| {
                let ep = e.path.as_slice();
                ep.len() > plen && ep.starts_with(prefix) && ep[plen] == b'/'
            });
            if !any {
                return;
            }
            self.entries_sorted = false;
        }
        {
            let had_descendant = self.entries.iter().any(|e| {
                let ep = e.path.as_slice();
                ep.len() > plen && ep.starts_with(prefix) && ep[plen] == b'/'
            });
            for e in self.entries.iter() {
                let ep = e.path.as_slice();
                if ep.len() > plen && ep.starts_with(prefix) && ep[plen] == b'/' && e.stage() != 0 {
                    resolve_undo::record_resolve_undo_for_entry(&mut self.resolve_undo, e);
                }
            }
            self.entries.retain(|e| {
                let ep = e.path.as_slice();
                if ep.len() <= plen {
                    return true;
                }
                if !ep.starts_with(prefix) {
                    return true;
                }
                ep[plen] != b'/'
            });
            if !had_descendant {
                return;
            }
        }
        self.invalidate_untracked_cache_for_path(path);
        self.invalidate_cache_tree_for_path(path.as_bytes());
    }

    /// Replace the cache-tree extension with a valid tree.
    pub fn set_cache_tree(&mut self, cache_tree: CacheTreeNode) {
        self.cache_tree_root = cache_tree.oid.filter(|_| cache_tree.entry_count >= 0);
        self.cache_tree = Some(cache_tree);
    }

    /// Remove the cache-tree extension.
    pub fn clear_cache_tree(&mut self) {
        self.cache_tree_root = None;
        self.cache_tree = None;
    }

    /// Mark cache-tree nodes affected by an index path change as invalid.
    ///
    /// Mirrors Git's `do_invalidate_path` (`cache-tree.c`): each ancestor node along `path`
    /// is invalidated (`entry_count = -1`), and when the final path component names an existing
    /// **subtree** node, that subtree is removed entirely. The removal is essential for the
    /// directory→file transition (e.g. tracked dir `a/b/` replaced by file `a/b`): without it,
    /// stale descendant nodes (`a/b/c`, …) keep their positive `entry_count` and later trip
    /// [`crate::write_tree::verify_cache_tree`] with "corrupted cache-tree has entries not present
    /// in index".
    pub fn invalidate_cache_tree_for_path(&mut self, path: &[u8]) {
        let Some(root) = self.cache_tree.as_mut() else {
            self.cache_tree_root = None;
            return;
        };
        self.cache_tree_root = None;
        Self::do_invalidate_cache_tree_path(root, path);
    }

    /// Recursive worker for [`Self::invalidate_cache_tree_for_path`], mirroring
    /// Git's `do_invalidate_path`.
    fn do_invalidate_cache_tree_path(node: &mut CacheTreeNode, path: &[u8]) {
        node.invalidate();
        let slash = path.iter().position(|&b| b == b'/');
        match slash {
            // Final component: drop the matching subtree node, if any.
            None => {
                if !path.is_empty() {
                    node.children.retain(|child| child.name != path);
                }
            }
            // Interior component: descend into the named subtree and recurse on the rest.
            Some(idx) => {
                let (component, rest) = (&path[..idx], &path[idx + 1..]);
                if let Some(child) = node
                    .children
                    .iter_mut()
                    .find(|child| child.name == component)
                {
                    Self::do_invalidate_cache_tree_path(child, rest);
                }
            }
        }
    }

    /// Format the parsed cache-tree extension like Git's `test-tool dump-cache-tree`.
    #[must_use]
    pub fn format_cache_tree_dump(&self) -> String {
        let Some(root) = self.cache_tree.as_ref() else {
            return String::new();
        };
        let mut out = String::new();
        format_cache_tree_node(root, "", &mut out);
        out
    }

    /// Produce `test-tool dump-cache-tree` output for this index.
    ///
    /// Mirrors Git's `cmd__dump_cache_tree`: the stored cache-tree (`it`) is
    /// compared against a freshly computed reference (`ref`) built from the
    /// current index entries with `WRITE_TREE_DRY_RUN`. Only nodes present in
    /// both trees are dumped; the `#(ref)` divergence lines that Git would emit
    /// are filtered out by the harness, so they are intentionally omitted here.
    ///
    /// If the index has no stored cache-tree, output is empty (Git dumps nothing
    /// because `it` is NULL).
    ///
    /// # Errors
    ///
    /// Returns an error if the reference cache-tree cannot be built (for example,
    /// if a tree object cannot be written to the object database).
    pub fn dump_cache_tree(&self, odb: &Odb) -> Result<String> {
        let Some(it_root) = self.cache_tree.as_ref() else {
            return Ok(String::new());
        };
        let ref_root = crate::write_tree::build_cache_tree_from_index(odb, self)?;
        let mut out = String::new();
        dump_cache_tree_pair(it_root, &ref_root, "", &mut out);
        Ok(out)
    }

    /// Sort entries in Git's canonical order: by path, then by stage.
    pub fn sort(&mut self) {
        self.ensure_canonical_order();
    }

    /// Restore Git canonical `(path, stage)` order before writing the index.
    ///
    /// Unlike [`Self::sort`], this re-sorts when the sorted flag is stale (for example after
    /// appending entries without using [`Self::add_or_replace`]).
    pub fn ensure_canonical_order(&mut self) {
        if self.entries_sorted && self.entries_are_sorted() {
            return;
        }
        self.sort_entries_canonical();
    }

    fn sort_entries_canonical(&mut self) {
        self.entries
            .sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.stage().cmp(&b.stage())));
        self.entries_sorted = true;
    }

    /// Collapse duplicate `(path, stage)` entries, keeping the **last** one in current order.
    ///
    /// A valid Git index never holds two entries with the same path and stage; `add_index_entry`
    /// replaces an existing same-name entry in place. When grit builds an index by flattening a
    /// tree that has duplicate path entries (see `t4058-diff-duplicates`), the naive flatten yields
    /// several identical-path entries. This restores Git's invariant by keeping the last entry for
    /// each `(path, stage)` — matching `add_index_entry`'s replace-on-collision semantics where the
    /// final tree entry for a path wins.
    pub fn dedup_paths_keep_last(&mut self) {
        let mut seen: std::collections::HashSet<(Vec<u8>, u8)> = std::collections::HashSet::new();
        let mut kept: Vec<IndexEntry> = Vec::with_capacity(self.entries.len());
        // Walk in reverse so the *last* occurrence of each (path, stage) is the one retained.
        for entry in self.entries.iter().rev() {
            if seen.insert((entry.path.clone(), entry.stage())) {
                kept.push(entry.clone());
            }
        }
        kept.reverse();
        self.entries = kept;
        self.entries_sorted = false;
    }

    /// OID of the shared index when this index uses split-index mode (`link` extension).
    #[must_use]
    pub fn split_index_base_oid(&self) -> Option<ObjectId> {
        self.split_link.as_ref().map(|l| l.base_oid)
    }

    /// Find an entry by path and stage (0 for normal entries).
    #[must_use]
    pub fn get(&self, path: &[u8], stage: u8) -> Option<&IndexEntry> {
        self.find_entry_pos(path, stage)
            .map(|pos| &self.entries[pos])
    }

    /// Find a mutable entry by path and stage.
    pub fn get_mut(&mut self, path: &[u8], stage: u8) -> Option<&mut IndexEntry> {
        let pos = self.find_entry_pos(path, stage)?;
        let entry = &mut self.entries[pos];
        // The caller may change `oid` or stat data, which voids a just-hashed mark.
        entry.take_content_verified();
        Some(entry)
    }

    /// Index positions of stage-0 entries, keyed like [`index_path_outcome_key`].
    ///
    /// Used when ordering [`crate::porcelain::add::StageOutcome`] path lists without repeated
    /// linear scans. Valid UTF-8 paths borrow from the index entry; invalid UTF-8 uses lossy keys.
    pub(crate) fn stage0_path_positions<'a>(
        &'a self,
    ) -> std::collections::HashMap<Cow<'a, str>, usize> {
        let stage0_count = self.entries.iter().filter(|e| e.stage() == 0).count();
        let mut positions = std::collections::HashMap::with_capacity(stage0_count);
        for (pos, entry) in self.entries.iter().enumerate() {
            if entry.stage() == 0 {
                let key = index_path_outcome_key(&entry.path);
                positions.entry(key).or_insert(pos);
            }
        }
        positions
    }

    /// Merge tree contents from `treeish` into this index as virtual stage-1 entries, matching
    /// Git's `overlay_tree_on_index` used by `git ls-files --with-tree`.
    ///
    /// Existing unmerged entries (stages 1–3) are shifted to stage 3 so stage 1 is free for the
    /// overlay. Stage-1 paths that already exist at stage 0 are marked so `ls-files` can skip
    /// them (Git's `CE_UPDATE` on the stage-1 entry).
    ///
    /// # Parameters
    ///
    /// - `repo` — repository whose object database is used to read the tree.
    /// - `treeish` — revision or tree OID string (`HEAD`, `HEAD~1`, full SHA, etc.).
    /// - `prefix` — optional path prefix (bytes, no trailing slash except empty); only paths under
    ///   this prefix are considered from the tree. Pass empty slice for the full tree.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] if `treeish` cannot be resolved, the tree cannot be read, or an object is
    /// missing from the ODB.
    pub fn overlay_tree_on_index(
        &mut self,
        repo: &Repository,
        treeish: &str,
        prefix: &[u8],
    ) -> Result<()> {
        let oid = rev_parse::resolve_revision(repo, treeish)?;
        let tree_oid = repo.odb().peel_to_tree(oid)?.ok_or_else(|| {
            Error::ObjectNotFound(format!("cannot peel {oid} to tree for --with-tree"))
        })?;
        for e in self.entries.iter_mut() {
            if e.stage() != 0 {
                e.set_stage(3);
            }
        }
        self.sort();
        let has_stage1 = self.entries.iter().any(|e| e.stage() == 1);
        let mut appended: Vec<IndexEntry> = Vec::new();
        let protect = crate::path_validate::ProtectOptions::from_config(&*repo.config()?);
        read_tree_into_overlay(
            repo,
            &tree_oid,
            prefix,
            &[],
            has_stage1,
            protect,
            &mut appended,
        )?;
        for e in appended {
            self.add_or_replace(e);
        }
        if !has_stage1 {
            self.sort();
        }
        let mut last_stage0: Option<&[u8]> = None;
        for e in &mut self.entries {
            match e.stage() {
                0 => {
                    last_stage0 = Some(e.path.as_slice());
                }
                1 if last_stage0.is_some_and(|p| p == e.path.as_slice()) => {
                    e.set_overlay_tree_skip_output(true);
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn read_tree_into_overlay(
    repo: &Repository,
    tree_oid: &ObjectId,
    prefix: &[u8],
    rel_base: &[u8],
    use_replace_path: bool,
    protect: crate::path_validate::ProtectOptions,
    out: &mut Vec<IndexEntry>,
) -> Result<()> {
    let obj = repo.odb().read(tree_oid)?;
    if obj.kind != ObjectKind::Tree {
        return Err(Error::ObjectNotFound(format!(
            "object {tree_oid} is not a tree"
        )));
    }
    let entries = parse_tree(&obj.data)?;
    for TreeEntry { mode, name, oid } in entries {
        let mut path = rel_base.to_vec();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(&name);
        if !prefix_under_or_equal(prefix, &path) {
            continue;
        }
        crate::path_population::validate_tree_walk_entry(rel_base, &name, mode, protect)?;
        if mode == MODE_TREE {
            read_tree_into_overlay(repo, &oid, prefix, &path, use_replace_path, protect, out)?;
            continue;
        }
        if mode == MODE_GITLINK {
            continue;
        }
        let entry = synthetic_stage1_index_entry(mode, &path, oid);
        if use_replace_path {
            if let Some(pos) = out.iter().position(|e| e.path == path && e.stage() == 1) {
                out[pos] = entry;
            } else {
                out.push(entry);
            }
        } else {
            out.push(entry);
        }
    }
    Ok(())
}

/// Collapse consecutive sorted rows with the same `(path, stage)`; the last row wins.
fn dedup_sorted_replacement_entries(replacements: &mut Vec<IndexEntry>) {
    if replacements.len() <= 1 {
        return;
    }
    let mut out: Vec<IndexEntry> = Vec::with_capacity(replacements.len());
    for entry in replacements.drain(..) {
        match out.last_mut() {
            Some(last) if last.path == entry.path && last.stage() == entry.stage() => {
                *last = entry;
            }
            _ => out.push(entry),
        }
    }
    *replacements = out;
}

fn index_entry_path_stage_cmp(a: &IndexEntry, b: &IndexEntry) -> std::cmp::Ordering {
    a.path.cmp(&b.path).then_with(|| a.stage().cmp(&b.stage()))
}

/// Merge sorted `insert` into sorted `kept` without holding a third full-length copy at peak.
///
/// `insert` is appended to `kept`, then the two adjacent sorted halves are merged in place.
fn merge_sorted_index_entries(
    mut kept: Vec<IndexEntry>,
    mut insert: Vec<IndexEntry>,
) -> Vec<IndexEntry> {
    if insert.is_empty() {
        return kept;
    }
    if kept.is_empty() {
        return insert;
    }
    kept.reserve(insert.len());
    let mid = kept.len();
    kept.append(&mut insert);
    merge_sorted_index_adjacent_halves(&mut kept, mid);
    kept
}

/// Merge sorted `v[0..mid)` with `v[mid..]` in place; auxiliary storage holds only the shorter half.
fn merge_sorted_index_adjacent_halves(v: &mut Vec<IndexEntry>, mid: usize) {
    let n = v.len();
    if mid == 0 || mid >= n {
        return;
    }
    let right_len = n - mid;
    let smaller_is_right = right_len <= mid;
    let mut aux = if smaller_is_right {
        v.drain(mid..).collect::<Vec<_>>()
    } else {
        v.drain(..mid).collect::<Vec<_>>()
    };
    let left_len = if smaller_is_right { v.len() } else { aux.len() };
    let pad = aux.len();
    v.reserve(pad);
    for _ in 0..pad {
        v.push(merge_padding_index_entry());
    }
    let total = v.len();
    merge_backward_into_place(v, left_len, total, &mut aux, smaller_is_right);
}

/// Placeholder slot for backward merge scratch space (replaced before any read).
fn merge_padding_index_entry() -> IndexEntry {
    IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: 0,
        uid: 0,
        gid: 0,
        size: 0,
        oid: ObjectId::zero(),
        flags: 0,
        flags_extended: None,
        path: Vec::new(),
        base_index_pos: 0,
    }
}

/// Backward merge into `v` using `aux` (copy of the shorter sorted run).
fn merge_backward_into_place(
    v: &mut Vec<IndexEntry>,
    left_len: usize,
    total: usize,
    aux: &mut Vec<IndexEntry>,
    smaller_is_right: bool,
) {
    let pad = aux.len();
    let mut li = left_len;
    let mut ri = if smaller_is_right {
        aux.len()
    } else {
        total.saturating_sub(pad)
    };
    let mut k = total;
    while li > 0 && ri > 0 {
        k -= 1;
        let ord = if smaller_is_right {
            index_entry_path_stage_cmp(&v[li - 1], &aux[ri - 1])
        } else {
            index_entry_path_stage_cmp(&aux[li - 1], &v[ri - 1])
        };
        match ord {
            std::cmp::Ordering::Greater => {
                if smaller_is_right {
                    let entry = std::mem::replace(&mut v[li - 1], merge_padding_index_entry());
                    drop(std::mem::replace(&mut v[k], entry));
                    li -= 1;
                } else {
                    let entry = std::mem::replace(&mut aux[li - 1], merge_padding_index_entry());
                    li -= 1;
                    drop(std::mem::replace(&mut v[k], entry));
                }
            }
            std::cmp::Ordering::Equal => {
                let base = if smaller_is_right {
                    v[li - 1].base_index_pos
                } else {
                    aux[li - 1].base_index_pos
                };
                let mut entry = if smaller_is_right {
                    let Some(e) = aux.pop() else {
                        break;
                    };
                    ri -= 1;
                    e
                } else {
                    ri -= 1;
                    std::mem::replace(&mut v[ri], merge_padding_index_entry())
                };
                entry.base_index_pos = base;
                if smaller_is_right {
                    drop(std::mem::replace(
                        &mut v[li - 1],
                        merge_padding_index_entry(),
                    ));
                    li -= 1;
                } else {
                    drop(std::mem::replace(
                        &mut aux[li - 1],
                        merge_padding_index_entry(),
                    ));
                    li -= 1;
                }
                drop(std::mem::replace(&mut v[k], entry));
            }
            std::cmp::Ordering::Less => {
                if smaller_is_right {
                    let Some(entry) = aux.pop() else {
                        break;
                    };
                    ri -= 1;
                    drop(std::mem::replace(&mut v[k], entry));
                } else {
                    ri -= 1;
                    let entry = std::mem::replace(&mut v[ri], merge_padding_index_entry());
                    drop(std::mem::replace(&mut v[k], entry));
                }
            }
        }
    }
    while ri > 0 {
        k -= 1;
        if smaller_is_right {
            let Some(entry) = aux.pop() else {
                break;
            };
            ri -= 1;
            drop(std::mem::replace(&mut v[k], entry));
        } else {
            ri -= 1;
            let entry = std::mem::replace(&mut v[ri], merge_padding_index_entry());
            drop(std::mem::replace(&mut v[k], entry));
        }
    }
    if !smaller_is_right && li > 0 {
        for i in 0..li {
            let entry = std::mem::replace(&mut aux[i], merge_padding_index_entry());
            drop(std::mem::replace(&mut v[i], entry));
        }
    }
    let tail_len = total - k;
    if li < k && tail_len > 0 {
        for offset in 0..tail_len {
            let entry = std::mem::replace(&mut v[k + offset], merge_padding_index_entry());
            drop(std::mem::replace(&mut v[li + offset], entry));
        }
    }
    v.truncate(li + tail_len);
}

fn prefix_under_or_equal(prefix: &[u8], path: &[u8]) -> bool {
    if prefix.is_empty() {
        return true;
    }
    if path == prefix {
        return true;
    }
    path.len() > prefix.len() && path.starts_with(prefix) && path[prefix.len()] == b'/'
}

fn synthetic_stage1_index_entry(mode: u32, path: &[u8], oid: ObjectId) -> IndexEntry {
    let path_len = path.len().min(0xFFF) as u16;
    let flags = (1u16 << 12) | path_len;
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
        flags,
        flags_extended: None,
        path: path.to_vec(),
        base_index_pos: 0,
    }
}

fn config_truthy(raw: Option<&str>) -> bool {
    let Some(val) = raw else {
        return false;
    };
    let lowered = val.trim().to_lowercase();
    matches!(lowered.as_str(), "true" | "yes" | "1" | "on")
}

/// Whether to write 20 zero bytes instead of the SHA-1 of the index body.
///
/// Mirrors Git `prepare_repo_settings`: `feature.manyFiles` enables skip-hash unless
/// `index.skipHash` / `index.skiphash` is explicitly false; otherwise honor true `index.skipHash`.
pub(crate) fn index_skip_hash_for_write(config: Option<&ConfigSet>) -> bool {
    let Some(config) = config else {
        return false;
    };
    let many_files = config
        .get_bool("feature.manyFiles")
        .and_then(|r| r.ok())
        .unwrap_or(false);
    if many_files {
        if let Some(Ok(false)) = config.get_bool("index.skipHash") {
            return false;
        }
        if let Some(Ok(false)) = config.get_bool("index.skiphash") {
            return false;
        }
        return true;
    }
    for key in ["index.skipHash", "index.skiphash"] {
        if let Some(Ok(true)) = config.get_bool(key) {
            return true;
        }
    }
    false
}

fn trim_trailing_slash_bytes(path: &[u8]) -> &[u8] {
    path.strip_suffix(b"/").unwrap_or(path)
}

fn directory_in_cone(dir_path: &str, patterns: &[String], cone_mode: bool) -> bool {
    // Match Git `path_in_cone_mode_sparse_checkout`: a *directory* is in the cone when its
    // contents are recursively included. `path_matches_sparse_patterns` distinguishes a
    // directory from a file by a trailing slash (expanded-cone matching uses dtype the way
    // Git does), so pass the directory with a trailing slash. Collapse prefixes arrive
    // without one (e.g. `before`, `folder1`), so append it to avoid a top-level directory
    // being mistaken for an always-in-cone top-level file. The root (empty) directory is
    // always in the cone (`/*` + `!/*/` excludes every top-level directory, so e.g. `deep`
    // collapses to a single placeholder instead of leaving `deep/deeper1/` etc.).
    let dir = dir_path.trim_end_matches('/');
    if dir.is_empty() {
        return true;
    }
    let with_slash = format!("{dir}/");
    crate::sparse_checkout::path_matches_sparse_patterns(&with_slash, patterns, cone_mode)
}

fn collect_directory_prefixes(path: &[u8], out: &mut BTreeSet<Vec<u8>>) {
    for (i, &b) in path.iter().enumerate() {
        if b == b'/' && !out.contains(&path[..i]) {
            out.insert(path[..i].to_vec());
        }
    }
}

/// Directories chosen for collapse so far, keyed by their path with a trailing slash.
///
/// No key is nested under another: collapsing a directory drops the descendants it subsumes.
#[derive(Default)]
struct CollapsedDirectories {
    dirs: BTreeMap<Vec<u8>, ObjectId>,
}

impl CollapsedDirectories {
    fn is_empty(&self) -> bool {
        self.dirs.is_empty()
    }

    /// Record `directory` (with trailing slash) as collapsed to `oid`.
    fn insert(&mut self, directory: Vec<u8>, oid: ObjectId) {
        let nested: Vec<Vec<u8>> = self
            .dirs
            .range::<[u8], _>((
                std::ops::Bound::Excluded(directory.as_slice()),
                std::ops::Bound::Unbounded,
            ))
            .map(|(k, _)| k)
            .take_while(|k| k.starts_with(&directory))
            .cloned()
            .collect();
        for key in nested {
            self.dirs.remove(&key);
        }
        self.dirs.insert(directory, oid);
    }

    fn get(&self, directory: &[u8]) -> Option<&ObjectId> {
        self.dirs.get(directory)
    }

    /// Whether a collapsed directory lies strictly under `directory` (with trailing slash).
    fn has_descendants(&self, directory: &[u8]) -> bool {
        self.dirs
            .range::<[u8], _>((
                std::ops::Bound::Excluded(directory),
                std::ops::Bound::Unbounded,
            ))
            .next()
            .is_some_and(|(k, _)| k.starts_with(directory))
    }

    /// For sorted `entries`, which rows [`Self::covers`] holds for, found by binary search per
    /// collapsed directory instead of a lookup per row.
    fn covered_rows(&self, entries: &[IndexEntry]) -> Vec<bool> {
        let mut covered = vec![false; entries.len()];
        let mut mark = |start: usize, matches: &dyn Fn(&[u8]) -> bool| {
            for (row, entry) in covered[start..].iter_mut().zip(&entries[start..]) {
                if !matches(&entry.path) {
                    break;
                }
                *row = true;
            }
        };
        for directory in self.dirs.keys() {
            let name = trim_trailing_slash_bytes(directory);
            let start = entries.partition_point(|e| e.path.as_slice() < name);
            mark(start, &|path| path == name);
            let start = entries.partition_point(|e| e.path.as_slice() < directory.as_slice());
            mark(start, &|path| path.starts_with(directory));
        }
        covered
    }

    /// Whether a row at `path` is replaced by a collapsed placeholder: it lies under a
    /// collapsed directory, or names the directory itself. `scratch` is reused between calls.
    fn covers(&self, path: &[u8], scratch: &mut Vec<u8>) -> bool {
        if self.dirs.is_empty() {
            return false;
        }
        // Keys are never nested, so the only key that can prefix `path` is the greatest key
        // not after it.
        let under = self
            .dirs
            .range::<[u8], _>((std::ops::Bound::Unbounded, std::ops::Bound::Included(path)))
            .next_back()
            .is_some_and(|(k, _)| path.starts_with(k));
        if under {
            return true;
        }
        scratch.clear();
        scratch.extend_from_slice(path);
        scratch.push(b'/');
        self.dirs.contains_key(scratch.as_slice())
    }

    fn into_placeholders(self) -> impl Iterator<Item = IndexEntry> {
        self.dirs.into_iter().map(|(path, oid)| {
            let mut placeholder = IndexEntry {
                ctime_sec: 0,
                ctime_nsec: 0,
                mtime_sec: 0,
                mtime_nsec: 0,
                dev: 0,
                ino: 0,
                mode: MODE_TREE,
                uid: 0,
                gid: 0,
                size: 0,
                oid,
                flags: path.len().min(0xFFF) as u16,
                flags_extended: Some(0),
                path,
                base_index_pos: 0,
            };
            placeholder.set_skip_worktree(true);
            placeholder
        })
    }
}

/// The index as [`Index::try_collapse_sparse_directories`] sees it mid-run: sorted `entries`
/// with the directories in `collapsed` replaced by placeholders.
struct SparseCollapseView<'a> {
    entries: &'a [IndexEntry],
    collapsed: &'a CollapsedDirectories,
}

impl SparseCollapseView<'_> {
    /// Whether a stage-0 sparse-directory placeholder for `path_slash` points at `oid`.
    fn has_placeholder(&self, path_slash: &[u8], oid: &ObjectId, scratch: &mut Vec<u8>) -> bool {
        if let Some(collapsed_oid) = self.collapsed.get(path_slash) {
            return collapsed_oid == oid;
        }
        if self.collapsed.covers(path_slash, scratch) {
            return false;
        }
        let start = self
            .entries
            .partition_point(|e| e.path.as_slice() < path_slash);
        self.entries[start..]
            .iter()
            .take_while(|e| e.path == path_slash)
            .any(|e| e.stage() == 0 && e.is_sparse_directory_placeholder() && e.oid == *oid)
    }
}

/// Resolves directory prefixes to their tree OIDs under one root tree, reading and parsing
/// each tree on the way at most once.
struct PrefixTreeResolver {
    root: ObjectId,
    resolved: HashMap<Vec<u8>, Option<ObjectId>>,
    parsed: HashMap<ObjectId, Option<Vec<TreeEntry>>>,
}

impl PrefixTreeResolver {
    fn new(root: ObjectId) -> Self {
        Self {
            root,
            resolved: HashMap::new(),
            parsed: HashMap::new(),
        }
    }

    /// The tree at `prefix` (no trailing slash), or `None` when a component is missing or
    /// is not a tree.
    fn resolve(&mut self, odb: &Odb, prefix: &[u8]) -> Result<Option<ObjectId>> {
        if prefix.is_empty() {
            return Ok(Some(self.root));
        }
        if let Some(known) = self.resolved.get(prefix) {
            return Ok(*known);
        }
        let (parent, name) = match prefix.iter().rposition(|&b| b == b'/') {
            Some(i) => (&prefix[..i], &prefix[i + 1..]),
            None => (&prefix[..0], prefix),
        };
        let found = match self.resolve(odb, parent)? {
            Some(parent_oid) if !name.is_empty() => {
                let entries = match self.parsed.entry(parent_oid) {
                    std::collections::hash_map::Entry::Occupied(known) => known.into_mut(),
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        let obj = odb.read(&parent_oid)?;
                        slot.insert(if obj.kind == ObjectKind::Tree {
                            Some(parse_tree(&obj.data)?)
                        } else {
                            None
                        })
                    }
                };
                entries
                    .as_ref()
                    .and_then(|entries| entries.iter().find(|e| e.name == name))
                    .filter(|e| e.mode == MODE_TREE)
                    .map(|e| e.oid)
            }
            // An empty component (`a//b`) is skipped, as when splitting on `/`.
            Some(parent_oid) => Some(parent_oid),
            None => None,
        };
        self.resolved.insert(prefix.to_vec(), found);
        Ok(found)
    }
}

/// Build the list of blob index entries under `prefix` that match `HEAD` at `tree_oid`,
/// treating existing sparse-directory placeholders in `view` as opaque subtrees (like Git).
fn collect_sparse_aware_expected_blobs(
    odb: &Odb,
    tree_oid: &ObjectId,
    prefix: &[u8],
    patterns: &[String],
    cone_mode: bool,
    view: &SparseCollapseView<'_>,
    protect: crate::path_validate::ProtectOptions,
) -> Result<Vec<ExpectedBlob>> {
    let mut out = Vec::new();
    let mut scratch = Vec::new();
    walk_sparse_aware(
        odb,
        tree_oid,
        prefix,
        patterns,
        cone_mode,
        view,
        protect,
        &mut scratch,
        &mut out,
    )?;
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn walk_sparse_aware(
    odb: &Odb,
    tree_oid: &ObjectId,
    prefix: &[u8],
    patterns: &[String],
    cone_mode: bool,
    view: &SparseCollapseView<'_>,
    protect: crate::path_validate::ProtectOptions,
    scratch: &mut Vec<u8>,
    out: &mut Vec<ExpectedBlob>,
) -> Result<()> {
    // Switch and checkout have usually just diffed these trees, so share the handle's cache.
    let tree = odb.cached_tree_for_walk(tree_oid)?;
    let mut visit = |name: &[u8], mode: u32, oid: ObjectId| -> Result<()> {
        let path = if prefix.is_empty() {
            name.to_vec()
        } else {
            let mut p = Vec::with_capacity(prefix.len() + 1 + name.len());
            p.extend_from_slice(prefix);
            p.push(b'/');
            p.extend_from_slice(name);
            p
        };
        crate::path_population::validate_tree_walk_entry(&path, name, mode, protect)?;
        if mode == MODE_TREE {
            let mut path_slash = path.clone();
            path_slash.push(b'/');
            if view.has_placeholder(&path_slash, &oid, scratch) {
                return Ok(());
            }
            return walk_sparse_aware(
                odb, &oid, &path, patterns, cone_mode, view, protect, scratch, out,
            );
        }
        let path_str = String::from_utf8_lossy(&path);
        if crate::sparse_checkout::path_matches_sparse_patterns(&path_str, patterns, cone_mode) {
            return Ok(());
        }
        out.push(ExpectedBlob { path, mode, oid });
        Ok(())
    };
    match &tree {
        crate::odb::SessionTree::Parsed(entries) => {
            for te in entries.iter() {
                visit(&te.name, te.mode, te.oid)?;
            }
        }
        crate::odb::SessionTree::Raw(data) => {
            for te in crate::objects::TreeRefIter::with_oid_len(data, odb.hash_algo().len()) {
                let te = te?;
                visit(te.name, te.mode, te.oid)?;
            }
        }
    }
    Ok(())
}

/// A blob a collapsible sparse directory must hold, from the `HEAD` tree.
struct ExpectedBlob {
    path: Vec<u8>,
    mode: u32,
    oid: ObjectId,
}

fn flatten_tree_blobs(
    odb: &Odb,
    tree_oid: &ObjectId,
    prefix: &[u8],
    protect: crate::path_validate::ProtectOptions,
) -> Result<Vec<IndexEntry>> {
    let obj = odb.read(tree_oid)?;
    if obj.kind != ObjectKind::Tree {
        return Err(Error::IndexError(format!("expected tree at {}", tree_oid)));
    }
    let entries = parse_tree(&obj.data)?;
    let mut out = Vec::new();
    for te in entries {
        let path = if prefix.is_empty() {
            te.name.clone()
        } else {
            let mut p = prefix.to_vec();
            p.push(b'/');
            p.extend_from_slice(&te.name);
            p
        };
        crate::path_population::validate_tree_walk_entry(prefix, &te.name, te.mode, protect)?;
        if te.mode == MODE_TREE {
            let sub = flatten_tree_blobs(odb, &te.oid, &path, protect)?;
            out.extend(sub);
        } else {
            let path_len = path.len().min(0xFFF) as u16;
            let mut e = IndexEntry {
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
                flags: path_len,
                flags_extended: Some(0),
                path,
                base_index_pos: 0,
            };
            e.set_skip_worktree(true);
            out.push(e);
        }
    }
    Ok(out)
}

/// Exclusive `.git/index.lock` held for a mutating porcelain operation.
///
/// Acquire with [`Self::acquire`] before changing the working tree; persist the index with
/// [`IndexHandle::commit_update`](crate::index::IndexHandle::commit_update) or
/// [`IndexHandle::write`](crate::index::IndexHandle::write), then release on drop when
/// uncommitted.
pub struct IndexLock {
    pub(crate) index_path: PathBuf,
    pub(crate) lock_path: PathBuf,
    pub(crate) pid_path: PathBuf,
    pub(crate) file: fs::File,
    pub(crate) wrote_pid: bool,
    pub(crate) committed: bool,
}

impl IndexLock {
    /// Write serialized index bytes into the held lock file without replacing `index`.
    pub(crate) fn write_index_contents(&mut self, body: &[u8], checksum: &[u8]) -> Result<()> {
        self.file.write_all(body).map_err(Error::Io)?;
        self.file.write_all(checksum).map_err(Error::Io)?;
        drop(std::mem::replace(
            &mut self.file,
            fs::OpenOptions::new()
                .write(true)
                .open(&self.lock_path)
                .map_err(Error::Io)?,
        ));
        Ok(())
    }

    /// Rename the staged lock file over the on-disk index.
    pub(crate) fn commit_replace_index(&mut self) -> Result<()> {
        fs::rename(&self.lock_path, &self.index_path).map_err(Error::Io)?;
        self.committed = true;
        if self.wrote_pid {
            let _ = fs::remove_file(&self.pid_path);
        }
        Ok(())
    }

    /// Create `.git/index.lock` exclusively, failing when another process holds it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::LockHeld`] (with the pid sidecar's [`LockHolder`](crate::lockfile::LockHolder)
    /// when one was written) when `index.lock` already exists after the
    /// `core.filesRefLockTimeout` budget, or [`Error::Io`] for other filesystem failures.
    pub fn acquire(index_path: &Path, config: Option<&ConfigSet>) -> Result<Self> {
        let lock_path = index_path.with_extension("lock");
        let pid_path = pid_path_for_lock(&lock_path);
        let lockfile_pid_enabled = grit_lockfile_pid_enabled(config, index_path.parent());

        use crate::lockfile::{LockFile, LockWait};
        let wait = LockWait::files_ref_lock(config);
        let mut lock_file = LockFile::acquire(&lock_path, wait).map_err(Error::from)?;
        if lockfile_pid_enabled {
            if let Err(e) = lock_file
                .write_holder_pid()
                .map_err(crate::error::Error::from)
            {
                lock_file.rollback();
                return Err(e);
            }
        }
        let wrote_pid = lockfile_pid_enabled;
        let (lock_path, file) = lock_file.into_open_file()?;

        Ok(Self {
            index_path: index_path.to_path_buf(),
            lock_path,
            pid_path,
            file,
            wrote_pid,
            committed: false,
        })
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let _ = fs::remove_file(&self.lock_path);
        if self.wrote_pid {
            let _ = fs::remove_file(&self.pid_path);
        }
    }
}

/// Hash an index body with the given algorithm, returning the raw checksum.
fn hash_index_body(algo: HashAlgo, body: &[u8]) -> Vec<u8> {
    algo.checksum_digest(body)
}

/// Write index bytes and trailing checksum in one buffered pass, updating the digest while writing.
fn write_index_body_and_trailer(
    file: &mut fs::File,
    algo: HashAlgo,
    skip_hash: bool,
    body: &[u8],
) -> io::Result<()> {
    use std::io::BufWriter;
    let mut out = BufWriter::with_capacity(256 * 1024, file);
    if skip_hash {
        out.write_all(body)?;
        out.write_all(&vec![0u8; algo.len()])?;
        return out.flush();
    }
    let mut hasher = algo.checksum_hasher();
    for chunk in body.chunks(256 * 1024) {
        hasher.update(chunk);
        out.write_all(chunk)?;
    }
    out.write_all(&hasher.finalize())?;
    out.flush()
}

impl Index {
    /// Conservative pre-size for serializing the index body (header, entries, extensions).
    fn estimate_body_capacity(&self, write_version: u32, config: Option<&ConfigSet>) -> usize {
        use crate::index_eoie_ieot::record_end_of_index_entries;
        let hash_len = self.hash_algo.len();
        let mut entries_bytes = 0usize;
        for entry in &self.entries {
            let mut n = 62usize
                .saturating_add(hash_len)
                .saturating_add(entry.path.len());
            if write_version >= 3
                && entry
                    .flags_extended
                    .map(IndexEntry::disk_flags_extended)
                    .filter(|fe| *fe != 0)
                    .is_some()
            {
                n += 2;
            }
            if write_version == 4 {
                n += 12;
            } else {
                n = (n + 7) & !7;
            }
            entries_bytes = entries_bytes.saturating_add(n);
        }
        let mut extra = 256usize;
        if self.cache_tree.is_some() {
            extra += 4096;
        }
        if self.untracked_cache.is_some() {
            extra += 4096;
        }
        if self.fsmonitor_last_update.is_some() {
            extra += 512;
        }
        if record_end_of_index_entries(config) {
            extra += 64;
        }
        12 + entries_bytes + extra
    }
}

fn append_index_extension(
    out: &mut Vec<u8>,
    eoie_headers: &mut Option<Vec<u8>>,
    signature: u32,
    payload: &[u8],
) {
    let size = payload.len() as u32;
    out.extend_from_slice(&signature.to_be_bytes());
    out.extend_from_slice(&size.to_be_bytes());
    if let Some(headers) = eoie_headers {
        headers.extend_from_slice(&signature.to_be_bytes());
        headers.extend_from_slice(&size.to_be_bytes());
    }
    out.extend_from_slice(payload);
}

/// Best-effort detection of an index file's hash algorithm from its trailing
/// checksum. Used by [`Index::parse`] when no repository algorithm is known.
/// Callers with a repository should use [`Index::parse_with_algo`] instead,
/// which is authoritative even when the checksum is skipped (all zeros).
fn detect_index_hash_algo(data: &[u8]) -> HashAlgo {
    // A SHA-256 index ends with a 32-byte checksum over the body. If that
    // verifies, the file is SHA-256; otherwise assume SHA-1.
    let sha256_len = HashAlgo::Sha256.len();
    if data.len() > sha256_len + 12 {
        let (body, checksum) = data.split_at(data.len() - sha256_len);
        if checksum.iter().any(|&b| b != 0) && hash_index_body(HashAlgo::Sha256, body) == checksum {
            return HashAlgo::Sha256;
        }
    }
    HashAlgo::Sha1
}

/// Parse a single index entry from `data`, returning `(entry, bytes_consumed)`.
///
/// `hash_len` is the raw object-id width (20 for SHA-1, 32 for SHA-256).
pub(crate) fn parse_entry(
    data: &[u8],
    version: u32,
    prev_path: &[u8],
    hash_len: usize,
) -> Result<(IndexEntry, usize)> {
    // 40 bytes of stat fields + the OID + 2 flag bytes.
    if data.len() < 40 + hash_len + 2 {
        return Err(Error::IndexError("entry too short".to_owned()));
    }

    let mut pos = 0;

    macro_rules! read_u32 {
        () => {{
            let v = u32::from_be_bytes(
                data[pos..pos + 4]
                    .try_into()
                    .map_err(|_| Error::IndexError("truncated u32".to_owned()))?,
            );
            pos += 4;
            v
        }};
    }

    let ctime_sec = read_u32!();
    let ctime_nsec = read_u32!();
    let mtime_sec = read_u32!();
    let mtime_nsec = read_u32!();
    let dev = read_u32!();
    let ino = read_u32!();
    let mode = read_u32!();
    let uid = read_u32!();
    let gid = read_u32!();
    let size = read_u32!();

    let oid = ObjectId::from_bytes(&data[pos..pos + hash_len])?;
    pos += hash_len;

    let flags = u16::from_be_bytes(
        data[pos..pos + 2]
            .try_into()
            .map_err(|_| Error::IndexError("truncated flags".to_owned()))?,
    );
    pos += 2;

    let flags_extended = if version >= 3 && flags & 0x4000 != 0 {
        let fe = u16::from_be_bytes(
            data[pos..pos + 2]
                .try_into()
                .map_err(|_| Error::IndexError("truncated extended flags".to_owned()))?,
        );
        pos += 2;
        Some(fe)
    } else {
        None
    };

    let path;
    if version == 4 {
        // V4: prefix-compressed path
        let (strip_len, varint_bytes) = read_varint(&data[pos..]);
        pos += varint_bytes;
        let nul = data[pos..]
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| Error::IndexError("v4 entry path missing NUL".to_owned()))?;
        let suffix = &data[pos..pos + nul];
        pos += nul + 1;
        let keep = prev_path.len().saturating_sub(strip_len);
        let mut full_path = Vec::with_capacity(keep + suffix.len());
        full_path.extend_from_slice(&prev_path[..keep]);
        full_path.extend_from_slice(suffix);
        path = full_path;
    } else {
        // V2/V3: NUL-terminated full path + padding
        let nul = data[pos..]
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| Error::IndexError("entry path missing NUL terminator".to_owned()))?;
        path = data[pos..pos + nul].to_vec();
        pos += nul + 1;
        let entry_start = 0usize;
        let entry_len = pos - entry_start;
        let padded = (entry_len + 7) & !7;
        let padding = padded.saturating_sub(entry_len);
        pos += padding;
    }

    Ok((
        IndexEntry {
            ctime_sec,
            ctime_nsec,
            mtime_sec,
            mtime_nsec,
            dev,
            ino,
            mode,
            uid,
            gid,
            size,
            oid,
            flags,
            flags_extended,
            path,
            base_index_pos: 0,
        },
        pos,
    ))
}

/// Serialise a single index entry into `out`.
/// Read a variable-length integer (git's index v4 varint encoding).
/// Returns (value, bytes_consumed).
fn write_varint(out: &mut Vec<u8>, mut value: usize) {
    loop {
        let mut b = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            b |= 0x80;
        }
        out.push(b);
        if value == 0 {
            break;
        }
    }
}

fn read_varint(data: &[u8]) -> (usize, usize) {
    let mut value: usize = 0;
    let mut shift = 0usize;
    let mut pos = 0;
    loop {
        if pos >= data.len() {
            break;
        }
        let byte = data[pos] as usize;
        pos += 1;
        value |= (byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
        // Prevent infinite loops on malformed data
        if shift > 28 {
            break;
        }
    }
    (value, pos)
}

fn serialize_entry_v4(entry: &IndexEntry, previous_path: &mut Vec<u8>, out: &mut Vec<u8>) {
    let write_u32 = |out: &mut Vec<u8>, v: u32| out.extend_from_slice(&v.to_be_bytes());

    write_u32(out, entry.ctime_sec);
    write_u32(out, entry.ctime_nsec);
    write_u32(out, entry.mtime_sec);
    write_u32(out, entry.mtime_nsec);
    write_u32(out, entry.dev);
    write_u32(out, entry.ino);
    write_u32(out, entry.mode);
    write_u32(out, entry.uid);
    write_u32(out, entry.gid);
    write_u32(out, entry.size);
    out.extend_from_slice(entry.oid.as_bytes());

    let mut flags = entry.flags;
    let disk_ext = entry
        .flags_extended
        .map(IndexEntry::disk_flags_extended)
        .filter(|fe| *fe != 0);
    if disk_ext.is_some() {
        flags |= 0x4000;
    } else {
        flags &= !0x4000;
    }
    let path_len = entry.path.len().min(0xFFF) as u16;
    flags = (flags & 0xF000) | path_len;
    out.extend_from_slice(&flags.to_be_bytes());

    if let Some(fe) = disk_ext {
        out.extend_from_slice(&fe.to_be_bytes());
    }

    let common = previous_path
        .iter()
        .zip(entry.path.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let to_remove = previous_path.len().saturating_sub(common);
    write_varint(out, to_remove);
    out.extend_from_slice(&entry.path[common..]);
    out.push(0);

    previous_path.clear();
    previous_path.extend_from_slice(&entry.path);
}

fn serialize_entry(entry: &IndexEntry, version: u32, out: &mut Vec<u8>) {
    let start = out.len();

    let write_u32 = |out: &mut Vec<u8>, v: u32| out.extend_from_slice(&v.to_be_bytes());

    write_u32(out, entry.ctime_sec);
    write_u32(out, entry.ctime_nsec);
    write_u32(out, entry.mtime_sec);
    write_u32(out, entry.mtime_nsec);
    write_u32(out, entry.dev);
    write_u32(out, entry.ino);
    write_u32(out, entry.mode);
    write_u32(out, entry.uid);
    write_u32(out, entry.gid);
    write_u32(out, entry.size);
    out.extend_from_slice(entry.oid.as_bytes());

    // Set or clear the extended-flags bit in flags
    let mut flags = entry.flags;
    let disk_ext = entry
        .flags_extended
        .map(IndexEntry::disk_flags_extended)
        .filter(|fe| *fe != 0);
    if version >= 3 && disk_ext.is_some() {
        flags |= 0x4000;
    } else {
        flags &= !0x4000;
    }
    // Overwrite path length bits (bottom 12)
    let path_len = entry.path.len().min(0xFFF) as u16;
    flags = (flags & 0xF000) | path_len;
    out.extend_from_slice(&flags.to_be_bytes());

    if version >= 3 {
        if let Some(fe) = disk_ext {
            out.extend_from_slice(&fe.to_be_bytes());
        }
    }

    out.extend_from_slice(&entry.path);
    out.push(0);

    // Pad to 8-byte boundary
    let entry_len = out.len() - start;
    let padded = (entry_len + 7) & !7;
    let padding = padded - entry_len;
    for _ in 0..padding {
        out.push(0);
    }
}

/// Build an [`IndexEntry`] by stat-ing a file on disk.
///
/// # Parameters
///
/// - `path` — absolute path to the file.
/// - `rel_path` — path relative to the repo root (stored in the index).
/// - `oid` — the object ID of the file's blob.
/// - `mode` — file mode (use [`MODE_REGULAR`], [`MODE_EXECUTABLE`], etc.).
///
/// # Errors
///
/// Returns [`Error::Io`] if `stat` fails.
pub fn entry_from_stat(
    path: &Path,
    rel_path: &[u8],
    oid: ObjectId,
    mode: u32,
) -> Result<IndexEntry> {
    let meta = fs::symlink_metadata(path)?;
    Ok(entry_from_metadata(&meta, rel_path, oid, mode))
}

/// Returns `(mtime_sec, mtime_nsec)` for an on-disk index file, or `None` if it does not exist.
///
/// Uses the `filetime` crate so index mtime reads behave the same on Unix and Windows.
#[must_use]
pub fn index_file_mtime(path: &Path) -> Option<(u32, u32)> {
    let meta = fs::metadata(path).ok()?;
    Some(index_mtime_from_metadata(&meta))
}

fn index_mtime_from_metadata(meta: &fs::Metadata) -> (u32, u32) {
    let ft = filetime::FileTime::from_last_modification_time(meta);
    let secs = ft.unix_seconds();
    if secs.is_negative() {
        return (0, 0);
    }
    (secs as u32, ft.nanoseconds())
}

/// Index files at least this large are memory-mapped instead of copied into a buffer.
const INDEX_MMAP_MIN_BYTES: u64 = 64 * 1024;

/// Raw bytes of an on-disk index file.
enum IndexFileBytes {
    Owned(Vec<u8>),
    Mapped(memmap2::Mmap),
}

impl std::ops::Deref for IndexFileBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Self::Owned(bytes) => bytes,
            Self::Mapped(map) => map,
        }
    }
}

/// Read an index file along with the mtime of the same open file.
///
/// Large files are mapped read-only: index writers replace the file by renaming a lock file
/// over it, so a mapped inode is never truncated underneath the reader.
fn read_index_file(path: &Path) -> io::Result<(IndexFileBytes, (u32, u32))> {
    use std::io::Read;

    let mut file = fs::File::open(path)?;
    let meta = file.metadata()?;
    let mtime = index_mtime_from_metadata(&meta);
    let len = meta.len();
    if meta.is_file() && len >= INDEX_MMAP_MIN_BYTES {
        if let Ok(len) = usize::try_from(len) {
            // SAFETY: the map is only read through `&[u8]`, its length comes from fstat on this
            // same file, and index writers swap files with rename instead of rewriting in place.
            if let Ok(map) = unsafe { memmap2::MmapOptions::new().len(len).map(&file) } {
                return Ok((IndexFileBytes::Mapped(map), mtime));
            }
        }
    }
    let mut bytes = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
    file.read_to_end(&mut bytes)?;
    Ok((IndexFileBytes::Owned(bytes), mtime))
}

fn max_stage0_entry_mtime(entries: &[IndexEntry]) -> (u32, u32) {
    let mut max_sec = 0u32;
    let mut max_nsec = 0u32;
    for ie in entries {
        if ie.stage() != 0 {
            continue;
        }
        if ie.mtime_sec > max_sec || (ie.mtime_sec == max_sec && ie.mtime_nsec > max_nsec) {
            max_sec = ie.mtime_sec;
            max_nsec = ie.mtime_nsec;
        }
    }
    (max_sec, max_nsec)
}

/// Maximum stage-0 mtime across two `(sec, nsec)` pairs.
#[must_use]
pub(crate) fn max_stage0_mtime_pair(a: (u32, u32), b: (u32, u32)) -> (u32, u32) {
    if a.0 > b.0 || (a.0 == b.0 && a.1 > b.1) {
        a
    } else {
        b
    }
}

/// Stage-0 mtime maximum in `entries`.
pub(crate) fn stage0_mtime_max(entries: &[IndexEntry]) -> (u32, u32) {
    max_stage0_entry_mtime(entries)
}

/// Advance the on-disk index mtime to be strictly newer than every stage-0 entry mtime.
///
/// This only acts when the index write lands on the same timestamp tick as the newest entry
/// (or earlier), as coarse filesystem clocks allow, and moves it the smallest step past that
/// entry: one tick of the filesystem clock, which is 100 ns on NTFS (a 1 ns step would be
/// truncated back onto the entry's own tick there). That step clears raciness only under
/// `core.usenanosec` (the default on Windows, see [`StatMatchPolicy::DEFAULT_USE_NANOSEC`]).
/// Under the whole-second rule, entries in the index's own second stay racy, and the next
/// status verifies them by content once and then rewrites the index. Never move the mtime
/// into a later second: whole-second stat matching would then hide an edit made within the
/// entry's second. When the newest entry sits on the last tick of its second, the index is
/// left as it is and those entries stay racy.
///
/// When `touched_only_max` is set, it must be the true maximum stage-0 mtime in the index after
/// checkout (not just the touched entries), or later switch prepare may treat paths as racy.
///
/// # Errors
///
/// Returns [`Error::Io`] when the index timestamp cannot be updated.
pub(crate) fn advance_index_file_mtime_past_entries(
    index: &Index,
    index_path: &Path,
    touched_only_max: Option<(u32, u32)>,
) -> Result<()> {
    use filetime::FileTime;
    let (max_sec, max_nsec) = match touched_only_max {
        Some(pair) => pair,
        None => max_stage0_entry_mtime(&index.entries),
    };
    if max_sec == 0 && max_nsec == 0 {
        return Ok(());
    }
    let needs_bump = index_file_mtime(index_path)
        .is_none_or(|(sec, nsec)| sec < max_sec || (sec == max_sec && nsec <= max_nsec));
    if !needs_bump {
        return Ok(());
    }
    const TICK_NSEC: u32 = if cfg!(windows) { 100 } else { 1 };
    if max_nsec >= 1_000_000_000 - TICK_NSEC {
        return Ok(());
    }
    let ft = FileTime::from_unix_time(i64::from(max_sec), max_nsec + TICK_NSEC);
    filetime::set_file_mtime(index_path, ft).map_err(Error::Io)
}

/// String form of an index path as surfaced in porcelain outcomes (UTF-8 or lossy).
#[must_use]
pub(crate) fn index_path_outcome_key(path: &[u8]) -> Cow<'_, str> {
    match std::str::from_utf8(path) {
        Ok(s) => Cow::Borrowed(s),
        Err(_) => Cow::Owned(String::from_utf8_lossy(path).into_owned()),
    }
}

/// Join a repository work tree root with an index-relative path stored as raw bytes.
///
/// Index paths may be non-UTF-8 on Unix; this preserves bytes when building the absolute path.
#[must_use]
pub fn worktree_path_from_index_rel(work_tree: &Path, rel: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        work_tree.join(OsStr::from_bytes(rel))
    }
    #[cfg(not(unix))]
    {
        let rel = std::str::from_utf8(rel)
            .map(Cow::Borrowed)
            .unwrap_or_else(|_| Cow::Owned(String::from_utf8_lossy(rel).into_owned()));
        work_tree.join(rel.as_ref())
    }
}

/// Stat fields copied from a worktree `Metadata` probe during index staging.
#[derive(Debug, Clone, Copy)]
pub struct IndexStatFields {
    ctime_sec: u32,
    ctime_nsec: u32,
    mtime_sec: u32,
    mtime_nsec: u32,
    dev: u32,
    ino: u32,
    uid: u32,
    gid: u32,
    size: u32,
}

impl IndexStatFields {
    /// Capture the same index fields as `from_metadata` from a Linux fstatat result.
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    pub(crate) fn from_linux_stat(stat: &libc::stat) -> Self {
        Self {
            ctime_sec: stat.st_ctime as u32,
            ctime_nsec: stat.st_ctime_nsec as u32,
            mtime_sec: stat.st_mtime as u32,
            mtime_nsec: stat.st_mtime_nsec as u32,
            dev: stat.st_dev as u32,
            ino: stat.st_ino as u32,
            uid: stat.st_uid,
            gid: stat.st_gid,
            size: munge_index_size(stat.st_size as u64),
        }
    }

    /// Munged size field stored in the index (see [`munge_index_size`]).
    #[must_use]
    pub(crate) fn index_size(&self) -> u32 {
        self.size
    }

    /// Capture stat fields from filesystem metadata.
    #[must_use]
    pub fn from_metadata(meta: &fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let ((ctime_sec, ctime_nsec), (mtime_sec, mtime_nsec)) = metadata_times(meta);
            Self {
                ctime_sec,
                ctime_nsec,
                mtime_sec,
                mtime_nsec,
                dev: meta.dev() as u32,
                ino: meta.ino() as u32,
                uid: meta.uid(),
                gid: meta.gid(),
                size: munge_index_size(meta.size()),
            }
        }
        #[cfg(not(unix))]
        {
            let ((ctime_sec, ctime_nsec), (mtime_sec, mtime_nsec)) = metadata_times(meta);
            Self {
                ctime_sec,
                ctime_nsec,
                mtime_sec,
                mtime_nsec,
                dev: 0,
                ino: 0,
                uid: 0,
                gid: 0,
                size: munge_index_size(meta.len()),
            }
        }
    }

    /// Stat fields currently cached on `ie`.
    #[must_use]
    pub(crate) fn of_entry(ie: &IndexEntry) -> Self {
        Self {
            ctime_sec: ie.ctime_sec,
            ctime_nsec: ie.ctime_nsec,
            mtime_sec: ie.mtime_sec,
            mtime_nsec: ie.mtime_nsec,
            dev: ie.dev,
            ino: ie.ino,
            uid: ie.uid,
            gid: ie.gid,
            size: ie.size,
        }
    }

    /// Whether `self` and `other` carry the same stat data under `policy`.
    #[must_use]
    pub(crate) fn equivalent_to(&self, other: &Self, policy: StatMatchPolicy) -> bool {
        if self.size != other.size || self.mtime_sec != other.mtime_sec {
            return false;
        }
        if policy.use_nanosec && policy.check_stat && self.mtime_nsec != other.mtime_nsec {
            return false;
        }
        if policy.trust_ctime && policy.check_stat {
            if self.ctime_sec != other.ctime_sec {
                return false;
            }
            if policy.use_nanosec && self.ctime_nsec != other.ctime_nsec {
                return false;
            }
        }
        !policy.check_stat
            || (self.dev == other.dev
                && self.ino == other.ino
                && self.uid == other.uid
                && self.gid == other.gid)
    }

    /// Replace the stat data cached on `ie` with these fields (OID, mode and path are kept).
    pub(crate) fn store_in(&self, ie: &mut IndexEntry) {
        ie.ctime_sec = self.ctime_sec;
        ie.ctime_nsec = self.ctime_nsec;
        ie.mtime_sec = self.mtime_sec;
        ie.mtime_nsec = self.mtime_nsec;
        ie.dev = self.dev;
        ie.ino = self.ino;
        ie.uid = self.uid;
        ie.gid = self.gid;
        ie.size = self.size;
    }

    /// Build a stage-0 index entry using cached stat fields.
    #[must_use]
    pub fn index_entry(self, rel_path: &[u8], oid: ObjectId, mode: u32) -> IndexEntry {
        IndexEntry {
            ctime_sec: self.ctime_sec,
            ctime_nsec: self.ctime_nsec,
            mtime_sec: self.mtime_sec,
            mtime_nsec: self.mtime_nsec,
            dev: self.dev,
            ino: self.ino,
            mode,
            uid: self.uid,
            gid: self.gid,
            size: self.size,
            oid,
            flags: rel_path.len().min(0xFFF) as u16,
            flags_extended: None,
            path: rel_path.to_vec(),
            base_index_pos: 0,
        }
    }
}

impl From<&fs::Metadata> for IndexStatFields {
    fn from(meta: &fs::Metadata) -> Self {
        Self::from_metadata(meta)
    }
}

impl IndexStatFields {
    /// Whether munged size and mtime match the index row, plus the sub-second mtime/ctime
    /// fractions that `policy` compares; ctime seconds and identity fields are ignored.
    #[must_use]
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn size_and_times_match_index(
        &self,
        ie: &IndexEntry,
        policy: StatMatchPolicy,
    ) -> bool {
        if self.size != ie.size || self.mtime_sec != ie.mtime_sec {
            return false;
        }
        if !(policy.use_nanosec && policy.check_stat) {
            return true;
        }
        self.mtime_nsec == ie.mtime_nsec
            && (!policy.trust_ctime || self.ctime_nsec == ie.ctime_nsec)
    }

    /// Whether cached stat fields match a stage-0 index row (Git `ie_match_stat`).
    #[must_use]
    pub(crate) fn matches_index_entry(&self, ie: &IndexEntry, policy: StatMatchPolicy) -> bool {
        if self.size != ie.size {
            return false;
        }
        if self.mtime_sec != ie.mtime_sec {
            return false;
        }
        if policy.use_nanosec && policy.check_stat && self.mtime_nsec != ie.mtime_nsec {
            return false;
        }
        if policy.trust_ctime && policy.check_stat {
            if self.ctime_sec != ie.ctime_sec {
                return false;
            }
            if policy.use_nanosec && self.ctime_nsec != ie.ctime_nsec {
                return false;
            }
        }
        if policy.check_stat {
            // Directory scans on Windows populate identity fields with zero (Git `mingw_stat`
            // also stores dev/ino/uid/gid as zero). Do not treat zeroed scan fields as a mismatch
            // against index rows written by Git or grit (#1518, #1702).
            #[cfg(windows)]
            {
                if self.ino != ie.ino && self.ino != 0 {
                    return false;
                }
                if self.dev != ie.dev && self.dev != 0 {
                    return false;
                }
                if self.uid != ie.uid && self.uid != 0 {
                    return false;
                }
                if self.gid != ie.gid && self.gid != 0 {
                    return false;
                }
            }
            #[cfg(not(windows))]
            if self.ino != ie.ino {
                return false;
            }
        }
        true
    }
}

/// Build an [`IndexEntry`] from already-obtained metadata.
///
/// This avoids a redundant `stat()` call when the caller already has
/// filesystem metadata (e.g. from `symlink_metadata`).
#[must_use]
pub fn entry_from_metadata(
    meta: &fs::Metadata,
    rel_path: &[u8],
    oid: ObjectId,
    mode: u32,
) -> IndexEntry {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let ((ctime_sec, ctime_nsec), (mtime_sec, mtime_nsec)) = metadata_times(meta);
        IndexEntry {
            ctime_sec,
            ctime_nsec,
            mtime_sec,
            mtime_nsec,
            dev: meta.dev() as u32,
            ino: meta.ino() as u32,
            mode,
            uid: meta.uid(),
            gid: meta.gid(),
            size: munge_index_size(meta.size()),
            oid,
            flags: rel_path.len().min(0xFFF) as u16,
            flags_extended: None,
            path: rel_path.to_vec(),
            base_index_pos: 0,
        }
    }
    #[cfg(not(unix))]
    {
        let ((ctime_sec, ctime_nsec), (mtime_sec, mtime_nsec)) = metadata_times(meta);
        IndexEntry {
            ctime_sec,
            ctime_nsec,
            mtime_sec,
            mtime_nsec,
            dev: 0,
            ino: 0,
            mode,
            uid: 0,
            gid: 0,
            size: munge_index_size(meta.len()),
            oid,
            flags: rel_path.len().min(0xFFF) as u16,
            flags_extended: None,
            path: rel_path.to_vec(),
            base_index_pos: 0,
        }
    }
}

/// Convert a `stat` mode to the Git index mode, normalised to one of the
/// known constants ([`MODE_REGULAR`], [`MODE_EXECUTABLE`], [`MODE_SYMLINK`]).
///
/// Only the `S_IFMT` and execute bits are inspected; all other permission bits
/// are discarded (Git stores only 644 or 755 for regular files).
///
/// # Parameters
///
/// - `raw_mode` — the raw `st_mode` value from `stat(2)`.
#[must_use]
pub fn normalize_mode(raw_mode: u32) -> u32 {
    const S_IFMT: u32 = 0o170000;
    const S_IFLNK: u32 = 0o120000;
    const S_IFREG: u32 = 0o100000;

    let fmt = raw_mode & S_IFMT;
    if fmt == S_IFLNK {
        return MODE_SYMLINK;
    }
    if fmt == S_IFREG {
        // Executable if any execute bit is set
        if raw_mode & 0o111 != 0 {
            return MODE_EXECUTABLE;
        }
        return MODE_REGULAR;
    }
    // Fallback for everything else (devices, etc.) — treat as regular
    MODE_REGULAR
}

#[cfg(test)]
pub(crate) mod index_path_cmp {
    use std::cell::Cell;

    thread_local! { // hygiene: index path comparison test counter (cfg(test) only)
        static PATH_CMP_COUNT: Cell<usize> = const { Cell::new(0) };
    }

    pub fn reset() {
        PATH_CMP_COUNT.with(|c| c.set(0));
    }

    pub fn take() -> usize {
        PATH_CMP_COUNT.with(|c| {
            let n = c.get();
            c.set(0);
            n
        })
    }

    pub fn bump() {
        PATH_CMP_COUNT.with(|c| c.set(c.get() + 1));
    }
}

fn record_path_cmp_for_tests() {
    #[cfg(test)]
    index_path_cmp::bump();
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use tempfile::TempDir;

    /// Per-thread count of fsmonitor EWAH rebuilds. Thread-local so concurrently running tests
    /// that also write fsmonitor indexes do not disturb each other's counts.
    pub(super) mod fsmonitor_rebuild_counter {
        use std::cell::Cell;

        thread_local! { // hygiene: fsmonitor rebuild counter for cfg(test) integration tests
            static COUNT: Cell<usize> = const { Cell::new(0) };
        }

        pub fn reset() {
            COUNT.with(|c| c.set(0));
        }

        pub fn count() -> usize {
            COUNT.with(Cell::get)
        }

        pub fn note_build() {
            COUNT.with(|c| c.set(c.get() + 1));
        }
    }

    fn dummy_oid() -> ObjectId {
        ObjectId::from_bytes(&[0u8; 20]).unwrap()
    }

    fn make_entry(path: &str) -> IndexEntry {
        IndexEntry {
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
            oid: dummy_oid(),
            flags: path.len().min(0xFFF) as u16,
            flags_extended: None,
            path: path.as_bytes().to_vec(),
            base_index_pos: 0,
        }
    }

    #[test]
    fn content_verified_bit_is_in_memory_only() {
        let mut index = Index::new();
        let mut plain = make_entry("a.txt");
        plain.mark_content_verified();
        let mut skipped = make_entry("b.txt");
        skipped.set_skip_worktree(true);
        skipped.mark_content_verified();
        index.add_or_replace(plain);
        index.add_or_replace(skipped);

        let mut only_marked = index.clone();
        only_marked.remove(b"b.txt");
        let mut out = Vec::new();
        only_marked.serialize_into(&mut out).unwrap();
        assert_eq!(&out[4..8], &2u32.to_be_bytes(), "the bit alone keeps v2");

        out.clear();
        index.serialize_into(&mut out).unwrap();
        let checksum = hash_index_body(index.hash_algo, &out);
        out.extend_from_slice(&checksum);
        let reread = Index::parse(&out).unwrap();
        assert_eq!(reread.version, 3);
        assert!(reread
            .entries
            .iter()
            .all(|e| !e.clone().take_content_verified()));
        assert!(reread.get(b"b.txt", 0).unwrap().skip_worktree());

        assert!(index.get_mut(b"a.txt", 0).is_some());
        assert_eq!(
            index.entries[0].flags_extended, None,
            "get_mut voids the mark"
        );
        index.clear_content_verified();
        assert!(index.entries[1].skip_worktree());
        assert!(!index.entries[1].clone().take_content_verified());
    }

    #[test]
    fn round_trip_empty_index() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("index");

        let mut idx = Index::new();
        idx.write(&path).unwrap();

        let loaded = Index::load(&path).unwrap();
        assert_eq!(loaded.entries.len(), 0);
    }

    #[test]
    fn read_index_file_maps_only_large_files() {
        let dir = TempDir::new().unwrap();
        let threshold = usize::try_from(INDEX_MMAP_MIN_BYTES).unwrap();
        for (len, mapped) in [(0, false), (threshold - 1, false), (threshold, true)] {
            let path = dir.path().join(format!("index-{len}"));
            let contents: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            fs::write(&path, &contents).unwrap();
            let (bytes, mtime) = read_index_file(&path).unwrap();
            assert_eq!(matches!(bytes, IndexFileBytes::Mapped(_)), mapped, "{len}");
            assert_eq!(&*bytes, contents.as_slice());
            assert_eq!(Some(mtime), index_file_mtime(&path));
        }
        let missing = read_index_file(&dir.path().join("missing")).err().unwrap();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn round_trip_with_entries() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("index");

        let mut idx = Index::new();
        idx.add_or_replace(make_entry("foo.txt"));
        idx.add_or_replace(make_entry("bar/baz.txt"));
        idx.write(&path).unwrap(); // idx is mut in each test scope

        let loaded = Index::load(&path).unwrap();
        assert_eq!(loaded.entries.len(), 2);
        assert_eq!(loaded.entries[0].path, b"bar/baz.txt");
        assert_eq!(loaded.entries[1].path, b"foo.txt");
    }

    fn overwrite_fixture() -> Index {
        let mut idx = Index::new();
        for i in 0..200 {
            let mut e = make_entry(&format!("d{}/f{i:03}.txt", i % 4));
            e.base_index_pos = i + 1;
            idx.add_or_replace(e);
        }
        for stage in 1..=3u16 {
            let mut e = make_entry("d0/conflict.txt");
            e.flags |= stage << 12;
            idx.add_or_replace(e);
        }
        let leaf =
            |name: &str| CacheTreeNode::valid(name.as_bytes().to_vec(), 50, dummy_oid(), vec![]);
        idx.cache_tree_root = Some(dummy_oid());
        idx.cache_tree = Some(CacheTreeNode::valid(
            Vec::new(),
            203,
            dummy_oid(),
            vec![leaf("d0"), leaf("d1"), leaf("d2"), leaf("d3")],
        ));
        assert!(idx.entries_sorted);
        idx
    }

    fn overwrite_replacements(count: usize) -> Vec<IndexEntry> {
        let changed = ObjectId::from_bytes(&[7u8; 20]).unwrap();
        let mut out: Vec<IndexEntry> = (0..count)
            .map(|i| {
                let mut e = make_entry(&format!("d{}/f{:03}.txt", (i * 3) % 4, i * 3));
                e.oid = changed;
                e.size = 9;
                e
            })
            .collect();
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    #[test]
    fn overwrite_stage0_entries_matches_sorted_merge() {
        for count in [3, 64] {
            let mut merged = overwrite_fixture();
            merged.replace_stage0_entries_batch(overwrite_replacements(count));
            let mut overwritten = overwrite_fixture();
            overwritten.overwrite_stage0_entries(overwrite_replacements(count));
            assert_eq!(overwritten.entries, merged.entries, "count {count}");
            assert_eq!(overwritten.cache_tree, merged.cache_tree, "count {count}");
            assert_eq!(overwritten.cache_tree_root, merged.cache_tree_root);
            assert_eq!(overwritten.entries.len(), 203);
        }
    }

    #[test]
    fn overwrite_stage0_entries_keeps_split_positions_and_invalidates_touched_dirs() {
        let mut idx = overwrite_fixture();
        let target = b"d1/f001.txt".to_vec();
        let pos = idx.find_entry_pos(&target, 0).unwrap();
        let before = idx.entries[pos].base_index_pos;
        let mut e = make_entry("d1/f001.txt");
        e.size = 42;
        idx.overwrite_stage0_entries(vec![e]);
        assert_eq!(idx.entries[pos].size, 42);
        assert_eq!(idx.entries[pos].base_index_pos, before);
        let tree = idx.cache_tree.as_ref().unwrap();
        assert!(!tree.is_valid());
        let child = |name: &[u8]| tree.children.iter().find(|c| c.name == name).unwrap();
        assert!(!child(b"d1").is_valid());
        assert!(child(b"d0").is_valid());
    }

    #[test]
    fn overwrite_stage0_entries_merges_paths_without_a_stage0_row() {
        let mut idx = overwrite_fixture();
        let mut new_path = make_entry("d2/new.txt");
        new_path.size = 1;
        idx.overwrite_stage0_entries(vec![make_entry("d0/f000.txt"), new_path]);
        assert_eq!(idx.entries.len(), 204);
        assert!(idx.get(b"d2/new.txt", 0).is_some());
        assert_eq!(
            idx.entries
                .iter()
                .filter(|e| e.path == b"d0/conflict.txt")
                .count(),
            3
        );
        assert!(idx.entries.windows(2).all(|w| {
            Index::cmp_paths(&w[0].path, &w[1].path)
                .then_with(|| w[0].stage().cmp(&w[1].stage()))
                .is_lt()
        }));
    }

    #[test]
    fn remove_descendants_under_path_drops_nested_only() {
        let mut idx = Index::new();
        idx.add_or_replace(make_entry("d/e"));
        idx.add_or_replace(make_entry("d-other"));
        idx.add_or_replace(make_entry("prefix/d"));
        idx.remove_descendants_under_path("d");
        let paths: Vec<_> = idx.entries.iter().map(|e| e.path.as_slice()).collect();
        assert_eq!(paths, vec![b"d-other".as_slice(), b"prefix/d".as_slice()]);
    }

    #[test]
    fn fsmonitor_extension_round_trip_matches_git_layout() {
        // Payload from `git -c core.fsmonitor=true status` on a one-file repo (v2 + builtin:fake + EWAH).
        let fsmn_payload: [u8; 41] = [
            0, 0, 0, 2, b'b', b'u', b'i', b'l', b't', b'i', b'n', b':', b'f', b'a', b'k', b'e', 0,
            0, 0, 0, 20, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        let (token, dirty) = parse_fsmonitor_extension(&fsmn_payload).unwrap();
        assert_eq!(token, "builtin:fake");
        assert_eq!(dirty.bit_size, 0);

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("index");
        let mut idx = Index::new();
        idx.add_or_replace(make_entry("a"));
        if let Some(e) = idx.get_mut(b"a", 0) {
            e.set_fsmonitor_valid(true);
        }
        idx.fsmonitor_last_update = Some(token);
        idx.fsmonitor_dirty = Some(dirty);
        idx.fsmonitor_raw_extension = Some(fsmn_payload.to_vec());
        idx.write(&path).unwrap(); // idx is mut in each test scope

        let loaded = Index::load(&path).unwrap();
        assert_eq!(
            loaded.fsmonitor_last_update.as_deref(),
            Some("builtin:fake")
        );
        assert_eq!(loaded.fsmonitor_dirty.as_ref().map(|b| b.bit_size), Some(0));
        assert_eq!(
            loaded.fsmonitor_raw_extension.as_deref(),
            Some(fsmn_payload.as_slice())
        );

        let mut body = Vec::new();
        loaded.serialize_into(&mut body).unwrap();
        assert!(
            body.windows(4).any(|w| w == b"FSMN"),
            "serialized index must include FSMN"
        );
        let off = body.windows(4).position(|w| w == b"FSMN").unwrap();
        let ext_sz = u32::from_be_bytes(body[off + 4..off + 8].try_into().unwrap()) as usize;
        let written = &body[off + 8..off + 8 + ext_sz];
        assert_eq!(written, fsmn_payload);
    }

    #[test]
    fn fsmonitor_extension_written_from_token_only_state() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("index");
        let mut idx = Index::new();
        idx.add_or_replace(make_entry("a"));
        idx.set_fsmonitor_last_update(Some("builtin:fake".to_owned()));
        idx.write(&path).unwrap(); // idx is mut in each test scope

        let data = fs::read(&path).unwrap();
        assert!(
            data.windows(4).any(|w| w == b"FSMN"),
            "token-only enable must emit a full FSMN extension"
        );
        let loaded = Index::load(&path).unwrap();
        assert_eq!(
            loaded.fsmonitor_last_update.as_deref(),
            Some("builtin:fake")
        );
    }

    #[test]
    fn fsmonitor_raw_not_reused_after_token_change() {
        let fsmn_payload: [u8; 41] = [
            0, 0, 0, 2, b'b', b'u', b'i', b'l', b't', b'i', b'n', b':', b'f', b'a', b'k', b'e', 0,
            0, 0, 0, 20, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        let mut idx = Index::new();
        idx.add_or_replace(make_entry("a"));
        idx.fsmonitor_last_update = Some("builtin:fake".to_owned());
        idx.fsmonitor_dirty = Some(parse_fsmonitor_extension(&fsmn_payload).unwrap().1);
        idx.fsmonitor_raw_extension = Some(fsmn_payload.to_vec());

        idx.set_fsmonitor_last_update(Some("last_update_token".to_owned()));
        let mut body = Vec::new();
        idx.serialize_into(&mut body).unwrap();
        let off = body.windows(4).position(|w| w == b"FSMN").unwrap();
        let ext_sz = u32::from_be_bytes(body[off + 4..off + 8].try_into().unwrap()) as usize;
        let written = &body[off + 8..off + 8 + ext_sz];
        let (token, _) = parse_fsmonitor_extension(written).unwrap();
        assert_eq!(token, "last_update_token");
        assert_ne!(written, fsmn_payload);
    }

    #[test]
    fn fsmonitor_raw_not_reused_after_validity_change() {
        let fsmn_payload: [u8; 41] = [
            0, 0, 0, 2, b'b', b'u', b'i', b'l', b't', b'i', b'n', b':', b'f', b'a', b'k', b'e', 0,
            0, 0, 0, 20, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        let mut idx = Index::new();
        idx.add_or_replace(make_entry("a"));
        idx.fsmonitor_last_update = Some("builtin:fake".to_owned());
        idx.fsmonitor_dirty = Some(parse_fsmonitor_extension(&fsmn_payload).unwrap().1);
        idx.fsmonitor_raw_extension = Some(fsmn_payload.to_vec());
        if let Some(e) = idx.get_mut(b"a", 0) {
            e.set_fsmonitor_valid(false);
        }
        idx.fsmonitor_validity_changed();

        let mut body = Vec::new();
        idx.serialize_into(&mut body).unwrap();
        let off = body.windows(4).position(|w| w == b"FSMN").unwrap();
        let ext_sz = u32::from_be_bytes(body[off + 4..off + 8].try_into().unwrap()) as usize;
        let written = &body[off + 8..off + 8 + ext_sz];
        assert_ne!(written, fsmn_payload);
        let (_, dirty) = parse_fsmonitor_extension(written).unwrap();
        assert!(dirty.bit_size >= 1);
    }

    fn make_staged_entry(path: &str, stage: u8) -> IndexEntry {
        let mut e = make_entry(path);
        e.set_stage(stage);
        e
    }

    /// Naive O(n) index model mirroring pre–binary-search semantics.
    struct LinearIndex {
        entries: Vec<IndexEntry>,
        resolve_undo: Option<BTreeMap<Vec<u8>, ResolveUndoRecord>>,
    }

    impl LinearIndex {
        fn from_index(idx: &Index) -> Self {
            Self {
                entries: idx.entries.clone(),
                resolve_undo: idx.resolve_undo.clone(),
            }
        }

        fn assert_matches(&self, idx: &Index) {
            let mut actual = idx.entries.clone();
            actual.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.stage().cmp(&b.stage())));
            let mut expect = self.entries.clone();
            expect.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.stage().cmp(&b.stage())));
            assert_eq!(actual, expect);
            assert_eq!(idx.resolve_undo, self.resolve_undo);
        }

        fn add_or_replace(&mut self, entry: IndexEntry) {
            if let Some(pos) = self
                .entries
                .iter()
                .position(|e| e.path == entry.path && e.stage() == entry.stage())
            {
                self.entries[pos] = entry;
            } else {
                self.entries.push(entry);
            }
            self.entries
                .sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.stage().cmp(&b.stage())));
        }

        fn remove(&mut self, path: &[u8]) -> bool {
            let mut removed_any = false;
            for e in &self.entries {
                if e.path == path {
                    if e.stage() != 0 {
                        resolve_undo::record_resolve_undo_for_entry(&mut self.resolve_undo, e);
                    }
                    removed_any = true;
                }
            }
            if !removed_any {
                return false;
            }
            self.entries.retain(|e| e.path != path);
            removed_any
        }

        fn stage_file(&mut self, entry: IndexEntry) {
            let path = entry.path.clone();
            for e in &self.entries {
                if e.path == path && e.stage() != 0 {
                    resolve_undo::record_resolve_undo_for_entry(&mut self.resolve_undo, e);
                }
            }
            self.entries.retain(|e| e.path != path || e.stage() == 0);
            self.add_or_replace(entry);
        }

        fn remove_descendants_under_path(&mut self, path: &str) {
            let prefix = path.as_bytes();
            if prefix.is_empty() {
                return;
            }
            let plen = prefix.len();
            for e in self.entries.iter() {
                let ep = e.path.as_slice();
                if ep.len() > plen && ep.starts_with(prefix) && ep[plen] == b'/' && e.stage() != 0 {
                    resolve_undo::record_resolve_undo_for_entry(&mut self.resolve_undo, e);
                }
            }
            self.entries.retain(|e| {
                let ep = e.path.as_slice();
                if ep.len() <= plen {
                    return true;
                }
                if !ep.starts_with(prefix) {
                    return true;
                }
                ep[plen] != b'/'
            });
        }
    }

    #[test]
    fn sorted_ops_match_linear_oracle() {
        let paths: Vec<Vec<u8>> = [
            "a", "a-b", "a/b", "a/b/c", "aa", "b", "dir/x", "dir/x/y", "dir-xy",
        ]
        .into_iter()
        .map(|p| p.as_bytes().to_vec())
        .collect();

        let mut idx = Index::new();
        let mut oracle = LinearIndex::from_index(&idx);
        let mut rng_state: u64 = 376;

        fn next_u64(state: &mut u64) -> u64 {
            *state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            *state
        }

        for step in 0..400 {
            let op = next_u64(&mut rng_state) % 4;
            match op {
                0 => {
                    let p = &paths[(next_u64(&mut rng_state) as usize) % paths.len()];
                    let stage = (next_u64(&mut rng_state) % 4) as u8;
                    let mut e = make_staged_entry(std::str::from_utf8(p).unwrap(), stage);
                    e.oid = ObjectId::from_bytes(&[(step % 256) as u8; 20]).unwrap();
                    idx.add_or_replace(e.clone());
                    oracle.add_or_replace(e);
                }
                1 => {
                    let p = &paths[(next_u64(&mut rng_state) as usize) % paths.len()];
                    idx.remove(p);
                    oracle.remove(p);
                }
                2 => {
                    let p = &paths[(next_u64(&mut rng_state) as usize) % paths.len()];
                    let e = make_entry(std::str::from_utf8(p).unwrap());
                    idx.stage_file(e.clone());
                    oracle.stage_file(e);
                }
                _ => {
                    let p = &paths[(next_u64(&mut rng_state) as usize) % paths.len()];
                    let ps = std::str::from_utf8(p).unwrap();
                    idx.remove_descendants_under_path(ps);
                    oracle.remove_descendants_under_path(ps);
                }
            }
            oracle.assert_matches(&idx);
        }
    }

    #[test]
    fn stale_sorted_flag_miss_until_resorted() {
        let mut idx = Index::new();
        idx.entries.push(make_entry("z"));
        idx.entries.push(make_entry("a"));
        idx.entries.push(make_entry("b"));
        idx.entries_sorted = true;
        assert!(!idx.entries_are_sorted());
        assert!(idx.get(b"z", 0).is_none());
        idx.sort();
        assert!(idx.get(b"z", 0).is_some());
        assert!(idx.remove(b"z"));
        assert!(idx.get(b"z", 0).is_none());
    }

    #[test]
    fn write_restores_canonical_order_when_sorted_flag_is_stale() {
        let mut idx = Index::new();
        idx.entries.push(make_entry("zzz.txt"));
        idx.entries.push(make_entry("aaa.txt"));
        idx.entries_sorted = true;
        assert!(!idx.entries_are_sorted());
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("index");
        idx.write(&path).expect("write index");
        let parsed = Index::load(&path).expect("load index");
        assert!(parsed.entries_are_sorted());
        assert_eq!(parsed.entries[0].path, b"aaa.txt".as_slice());
    }

    #[test]
    fn find_entry_pos_sorted_miss_is_constant() {
        const N: usize = 100_000;
        const M: usize = 1_000;
        let mut idx = Index::new();
        for i in 0..N {
            idx.add_or_replace(make_entry(&format!("file/{i:06}.txt")));
        }
        assert!(idx.entries_sorted);
        let misses: Vec<Vec<u8>> = (0..M)
            .map(|i| format!("missing/{i:06}.txt").into_bytes())
            .collect();
        index_path_cmp::reset();
        for path in &misses {
            assert!(idx.get(path, 0).is_none());
        }
        let cmps = index_path_cmp::take();
        let log_n = (N as f64).log2().ceil() as usize;
        let bound = 100 * M * log_n;
        assert!(
            cmps < bound,
            "sorted miss path comparisons {cmps} should stay under {bound} (N={N}, M={M})"
        );
    }

    #[test]
    fn unsorted_remove_drops_every_matching_stage_globally() {
        let mut idx = Index::new();
        idx.mark_entries_unsorted();
        idx.entries.push(make_staged_entry("a", 1));
        idx.entries.push(make_entry("z"));
        idx.entries.push(make_staged_entry("a", 2));
        assert!(idx.remove(b"a"));
        assert!(idx.get(b"a", 1).is_none());
        assert!(idx.get(b"a", 2).is_none());
        assert!(idx.get(b"z", 0).is_some());
    }

    #[test]
    fn unsorted_remove_descendants_drops_noncontiguous_matches() {
        let mut idx = Index::new();
        idx.mark_entries_unsorted();
        idx.entries.push(make_entry("a/x"));
        idx.entries.push(make_entry("z"));
        idx.entries.push(make_entry("a/y"));
        idx.remove_descendants_under_path("a");
        let paths: Vec<_> = idx.entries.iter().map(|e| e.path.as_slice()).collect();
        assert_eq!(paths, vec![b"z".as_slice()]);
    }

    #[test]
    fn sorted_ops_match_linear_oracle_unsorted_noncontiguous() {
        let mut idx = Index::new();
        idx.mark_entries_unsorted();
        idx.entries.push(make_entry("a/x"));
        idx.entries.push(make_entry("z"));
        idx.entries.push(make_staged_entry("a", 1));
        idx.remove(b"a");
        idx.entries.push(make_entry("a/y"));
        idx.sort();
        let mut oracle = LinearIndex::from_index(&idx);
        oracle.entries = idx.entries.clone();
        oracle.assert_matches(&idx);
        idx.remove_descendants_under_path("a");
        oracle.remove_descendants_under_path("a");
        oracle.assert_matches(&idx);
    }

    #[test]
    fn merge_replacements_preserves_base_index_pos() {
        let mut idx = Index::new();
        let mut kept = make_entry("path/a.txt");
        kept.base_index_pos = 42;
        idx.add_or_replace(kept);
        let mut replacement = make_entry("path/a.txt");
        replacement.oid = dummy_oid();
        idx.remove_paths_and_insert([], [replacement]);
        assert_eq!(idx.entries[0].base_index_pos, 42);
    }

    #[test]
    fn remove_paths_and_insert_replaces_equal_path_without_duplicates() {
        let mut idx = Index::new();
        idx.add_or_replace(make_entry("path/a.txt"));
        idx.add_or_replace(make_entry("path/b.txt"));
        let mut replacement = make_entry("path/a.txt");
        replacement.oid = dummy_oid();
        idx.remove_paths_and_insert(["ghost.txt".as_bytes()], [replacement.clone()]);
        assert_eq!(idx.entries.len(), 2);
        assert_eq!(idx.entries[0].path, b"path/a.txt");
        assert_eq!(idx.entries[0].oid, replacement.oid);
        assert_eq!(idx.entries[1].path, b"path/b.txt");
    }

    #[test]
    fn remove_directory_prefixes_drops_nested_paths() {
        let mut idx = Index::new();
        idx.add_or_replace(make_entry("d005/f0.txt"));
        idx.add_or_replace(make_entry("d005/f1.txt"));
        idx.add_or_replace(make_entry("d006/f0.txt"));
        idx.add_or_replace(make_entry("keep.txt"));
        idx.remove_directory_prefixes(&BTreeSet::from(["d005".to_owned()]));
        assert!(idx.get(b"d005/f0.txt", 0).is_none());
        assert!(idx.get(b"d005/f1.txt", 0).is_none());
        assert!(idx.get(b"d006/f0.txt", 0).is_some());
        assert!(idx.get(b"keep.txt", 0).is_some());
    }

    #[test]
    fn remove_prefixes_paths_and_insert_combines_prefix_exact_and_replacements() {
        let mut idx = Index::new();
        idx.add_or_replace(make_entry("d005/f0.txt"));
        idx.add_or_replace(make_entry("d005/f1.txt"));
        idx.add_or_replace(make_entry("d006/f0.txt"));
        idx.add_or_replace(make_entry("solo.txt"));
        let mut replacement = make_entry("solo.txt");
        replacement.oid = dummy_oid();
        idx.remove_prefixes_paths_and_insert(
            &BTreeSet::from(["d005".to_owned()]),
            [b"d006/f0.txt".as_slice()],
            [replacement.clone()],
        );
        assert!(idx.get(b"d005/f0.txt", 0).is_none());
        assert!(idx.get(b"d005/f1.txt", 0).is_none());
        assert!(idx.get(b"d006/f0.txt", 0).is_none());
        assert_eq!(idx.get(b"solo.txt", 0).unwrap().oid, replacement.oid);
    }

    #[test]
    fn remove_directory_prefixes_does_not_skip_lexically_later_prefix_children() {
        let mut idx = Index::new();
        idx.add_or_replace(make_entry("d005/f0.txt"));
        idx.add_or_replace(make_entry("d005/f1.txt"));
        idx.remove_directory_prefixes(&BTreeSet::from(["d005".to_owned(), "d0050".to_owned()]));
        assert!(idx.get(b"d005/f0.txt", 0).is_none());
        assert!(idx.get(b"d005/f1.txt", 0).is_none());
    }

    #[test]
    fn remove_is_sublinear() {
        const N: usize = 100_000;
        const M: usize = 1_000;
        let mut idx = Index::new();
        for i in 0..N {
            idx.add_or_replace(make_entry(&format!("file/{i:06}.txt")));
        }
        let paths: Vec<Vec<u8>> = (0..M)
            .map(|i| format!("file/{i:06}.txt").into_bytes())
            .collect();
        index_path_cmp::reset();
        idx.remove_paths(paths.iter().map(|p| p.as_slice()));
        let cmps = index_path_cmp::take();
        let log_n = (N as f64).log2().ceil() as usize;
        let bound = 100 * M * log_n;
        assert!(
            cmps < bound,
            "path comparisons {cmps} should stay under {bound} (N={N}, M={M})"
        );
        assert_eq!(idx.entries.len(), N - M);
    }

    #[test]
    fn fsmonitor_rebuild_once_per_batch() {
        let fsmn_payload: [u8; 41] = [
            0, 0, 0, 2, b'b', b'u', b'i', b'l', b't', b'i', b'n', b':', b'f', b'a', b'k', b'e', 0,
            0, 0, 0, 20, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        let (token, dirty) = parse_fsmonitor_extension(&fsmn_payload).unwrap();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("index");

        let mut idx = Index::new();
        for i in 0..1000 {
            idx.add_or_replace(make_entry(&format!("path-{i:04}.txt")));
        }
        idx.sort();
        for e in &mut idx.entries {
            e.set_fsmonitor_valid(true);
        }
        idx.fsmonitor_last_update = Some(token);
        idx.fsmonitor_dirty = Some(dirty);
        idx.fsmonitor_raw_extension = Some(fsmn_payload.to_vec());

        fsmonitor_rebuild_counter::reset();
        for i in 0..1000 {
            idx.add_or_replace(make_entry(&format!("mut-{i:04}.txt")));
        }
        assert_eq!(
            fsmonitor_rebuild_counter::count(),
            0,
            "entry mutations must not scan entries for the EWAH"
        );
        idx.write(&path).unwrap(); // idx is mut in each test scope
        assert_eq!(
            fsmonitor_rebuild_counter::count(),
            1,
            "write must build the EWAH once for the batch"
        );
    }

    #[test]
    fn requested_v4_writes_v4_on_disk() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("index");

        let mut idx = Index {
            version: 4,
            ..Index::default()
        };
        idx.add_or_replace(make_entry("one"));
        idx.add_or_replace(make_entry("two/one"));
        idx.write(&path).unwrap(); // idx is mut in each test scope

        let data = fs::read(&path).unwrap();
        assert_eq!(&data[4..8], &4u32.to_be_bytes());

        let loaded = Index::load(&path).unwrap();
        assert_eq!(loaded.version, 4);
        assert_eq!(loaded.entries[0].path, b"one");
        assert_eq!(loaded.entries[1].path, b"two/one");
    }

    #[test]
    fn stat_match_policy_from_repo_config() {
        let dir = TempDir::new().unwrap();
        let git_dir = dir.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(
            git_dir.join("config"),
            "[core]\n\ttrustctime = false\n\tcheckstat = minimal\n\tusenanosec = true\n",
        )
        .unwrap();
        let cfg = crate::config::ConfigSet::load(
            &crate::environment::Environment::empty(),
            Some(&git_dir),
            true,
        )
        .unwrap();
        let policy = StatMatchPolicy::from_config(Some(&cfg));
        assert!(!policy.trust_ctime);
        assert!(!policy.check_stat);
        assert!(policy.use_nanosec);
    }

    fn policy_from_core_config(body: &str) -> StatMatchPolicy {
        let dir = TempDir::new().unwrap();
        let git_dir = dir.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(git_dir.join("config"), format!("[core]\n{body}")).unwrap();
        let cfg = crate::config::ConfigSet::load(
            &crate::environment::Environment::empty(),
            Some(&git_dir),
            true,
        )
        .unwrap();
        StatMatchPolicy::from_config(Some(&cfg))
    }

    #[test]
    fn stat_match_policy_uses_nanoseconds_by_default_on_windows_only() {
        assert_eq!(StatMatchPolicy::DEFAULT_USE_NANOSEC, cfg!(windows));
        assert_eq!(StatMatchPolicy::default().use_nanosec, cfg!(windows));
        assert_eq!(
            StatMatchPolicy::from_config(None),
            StatMatchPolicy::default()
        );
        assert_eq!(
            policy_from_core_config("\tbare = false\n"),
            StatMatchPolicy::default()
        );
        assert!(!policy_from_core_config("\tusenanosec = false\n").use_nanosec);
        assert!(policy_from_core_config("\tusenanosec = true\n").use_nanosec);
    }

    const NSEC_POLICY: StatMatchPolicy = StatMatchPolicy {
        trust_ctime: true,
        check_stat: true,
        use_nanosec: true,
    };
    const WHOLE_SECOND_POLICY: StatMatchPolicy = StatMatchPolicy {
        use_nanosec: false,
        ..NSEC_POLICY
    };

    fn entry_with_mtime(sec: u32, nsec: u32) -> IndexEntry {
        IndexEntry {
            mtime_sec: sec,
            mtime_nsec: nsec,
            ctime_sec: sec,
            ctime_nsec: nsec,
            size: 42,
            ..make_entry("f")
        }
    }

    #[test]
    fn nanosecond_policy_trusts_same_second_entries_but_sees_newer_fractions() {
        let ie = entry_with_mtime(1_700_000_000, 123_456_700);
        let index_mtime = Some((ie.mtime_sec, ie.mtime_nsec + 1));
        assert!(!crate::diff::entry_is_racy_with_policy(
            &ie,
            index_mtime,
            NSEC_POLICY
        ));
        assert!(crate::diff::entry_is_racy_with_policy(
            &ie,
            Some((ie.mtime_sec, ie.mtime_nsec)),
            NSEC_POLICY
        ));
        assert!(
            crate::diff::entry_is_racy_with_policy(&ie, index_mtime, WHOLE_SECOND_POLICY),
            "the whole-second rule keeps the index's own second racy"
        );

        // A same-second, same-size edit is only visible in the fraction.
        let mut edited = IndexStatFields::of_entry(&ie);
        edited.mtime_nsec += 1;
        assert!(IndexStatFields::of_entry(&ie).matches_index_entry(&ie, NSEC_POLICY));
        assert!(!edited.matches_index_entry(&ie, NSEC_POLICY));
        assert!(edited.matches_index_entry(&ie, WHOLE_SECOND_POLICY));
    }

    #[test]
    fn minimal_checkstat_keeps_whole_second_racy_rule_under_nanosec() {
        let minimal = StatMatchPolicy {
            check_stat: false,
            ..NSEC_POLICY
        };
        let ie = entry_with_mtime(1_700_000_000, 100_000_000);
        let index_mtime = Some((ie.mtime_sec, 500_000_000));
        assert!(
            crate::diff::entry_is_racy_with_policy(&ie, index_mtime, minimal),
            "without fraction equality, a same-second entry must stay racy"
        );
        let mut edited = IndexStatFields::of_entry(&ie);
        edited.mtime_nsec = 200_000_000;
        assert!(edited.matches_index_entry(&ie, minimal));
        assert!(!crate::diff::entry_is_racy_with_policy(
            &ie,
            Some((ie.mtime_sec + 1, 0)),
            minimal
        ));
    }

    #[test]
    fn advancing_index_mtime_never_leaves_the_last_tick_of_a_second() {
        let dir = TempDir::new().unwrap();
        let index_path = dir.path().join("index");
        let sec = 1_700_000_000u32;
        let last_tick = if cfg!(windows) {
            999_999_900
        } else {
            999_999_999
        };
        let mut index = Index::new();
        index.add_or_replace(entry_with_mtime(sec, last_tick));
        index.write(&index_path).unwrap();
        let pinned = filetime::FileTime::from_unix_time(i64::from(sec), last_tick);
        filetime::set_file_mtime(&index_path, pinned).unwrap();

        advance_index_file_mtime_past_entries(&index, &index_path, None).unwrap();
        assert_eq!(index_file_mtime(&index_path), Some((sec, last_tick)));
        for policy in [NSEC_POLICY, WHOLE_SECOND_POLICY] {
            assert!(crate::diff::entry_is_racy_with_policy(
                &index.entries[0],
                index_file_mtime(&index_path),
                policy
            ));
        }
    }

    #[test]
    fn size_and_times_match_index_compares_fractions_only_under_nanosec_policy() {
        let ie = entry_with_mtime(1_700_000_000, 123_456_700);
        let mut scan = IndexStatFields::of_entry(&ie);
        scan.ctime_sec += 5;
        scan.ino = 99;
        assert!(scan.size_and_times_match_index(&ie, NSEC_POLICY));

        let mut mtime_frac = IndexStatFields::of_entry(&ie);
        mtime_frac.mtime_nsec += 100;
        assert!(!mtime_frac.size_and_times_match_index(&ie, NSEC_POLICY));
        assert!(mtime_frac.size_and_times_match_index(&ie, WHOLE_SECOND_POLICY));

        let mut ctime_frac = IndexStatFields::of_entry(&ie);
        ctime_frac.ctime_nsec += 100;
        assert!(!ctime_frac.size_and_times_match_index(&ie, NSEC_POLICY));
        let no_ctime = StatMatchPolicy {
            trust_ctime: false,
            ..NSEC_POLICY
        };
        assert!(ctime_frac.size_and_times_match_index(&ie, no_ctime));

        let mut resized = IndexStatFields::of_entry(&ie);
        resized.size += 1;
        assert!(!resized.size_and_times_match_index(&ie, WHOLE_SECOND_POLICY));
    }

    #[test]
    fn advancing_index_mtime_clears_same_tick_entries_within_their_second() {
        let dir = TempDir::new().unwrap();
        let index_path = dir.path().join("index");
        let (sec, nsec) = (1_700_000_000u32, 123_456_700u32);
        let mut index = Index::new();
        index.add_or_replace(entry_with_mtime(sec, nsec));
        index.write(&index_path).unwrap();
        filetime::set_file_mtime(
            &index_path,
            filetime::FileTime::from_unix_time(i64::from(sec), nsec),
        )
        .unwrap();
        let ie = &index.entries[0];
        assert!(crate::diff::entry_is_racy_with_policy(
            ie,
            index_file_mtime(&index_path),
            NSEC_POLICY
        ));

        advance_index_file_mtime_past_entries(&index, &index_path, None).unwrap();
        let bumped = index_file_mtime(&index_path);
        assert_eq!(
            bumped.map(|(s, _)| s),
            Some(sec),
            "never leave the entry's second"
        );
        assert!(
            !crate::diff::entry_is_racy_with_policy(ie, bumped, NSEC_POLICY),
            "bumped index mtime {bumped:?} must be past the entry's filesystem tick"
        );
        assert!(crate::diff::entry_is_racy_with_policy(
            ie,
            bumped,
            WHOLE_SECOND_POLICY
        ));
    }

    #[test]
    fn index_entry_extended_flags_and_placeholders() {
        let mut e = make_entry("p");
        e.set_intent_to_add(true);
        assert!(e.intent_to_add());
        e.set_assume_unchanged(true);
        assert!(e.assume_unchanged());
        e.set_skip_worktree(true);
        assert!(e.skip_worktree());
        e.mode = MODE_TREE;
        e.set_skip_worktree(true);
        assert!(e.is_sparse_directory_placeholder());
        e.set_overlay_tree_skip_output(true);
        assert!(e.overlay_tree_skip_output());
    }

    #[test]
    fn cache_tree_formatting_and_invalidation_helpers() {
        let mut idx = Index::new();
        idx.add_or_replace(make_entry("a/b"));
        let child = CacheTreeNode::valid(b"a".to_vec(), 1, dummy_oid(), vec![]);
        let root = CacheTreeNode::valid(b"".to_vec(), 1, dummy_oid(), vec![child]);
        idx.set_cache_tree(root);
        let dump = idx.format_cache_tree_dump();
        assert!(dump.contains("a/") || dump.contains("invalid"));
        idx.invalidate_cache_tree_for_path(b"a/b");
        idx.clear_cache_tree();
        assert!(idx.cache_tree.is_none());
    }

    #[test]
    fn dedup_paths_and_stage0_descendant_queries() {
        let mut idx = Index::new();
        idx.add_or_replace(make_entry("dir/x"));
        idx.add_or_replace(make_entry("dir/x"));
        idx.dedup_paths_keep_last();
        assert_eq!(idx.entries.len(), 1);
        assert!(idx.has_stage0_descendant_under("dir"));
        assert!(!idx.has_stage0_descendant_under("missing"));
    }

    #[test]
    fn load_missing_index_is_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("no-index");
        let idx = Index::load(&path).unwrap();
        assert!(idx.entries.is_empty());
    }

    #[test]
    fn munge_and_stat_equivalence_helpers() {
        assert_eq!(munge_index_size(0x1_0000_0000), 0x8000_0000);
        let a = make_entry("x");
        let b = make_entry("x");
        assert!(index_entries_stat_equivalent(
            &a,
            &b,
            StatMatchPolicy::default()
        ));
    }

    #[test]
    fn new_from_config_invalid_index_version_falls_back() {
        let dir = TempDir::new().unwrap();
        let git_dir = dir.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(
            git_dir.join("config"),
            "[index]\n\tversion = not-a-number\n",
        )
        .unwrap();
        let cfg = crate::config::ConfigSet::load(
            &crate::environment::Environment::empty(),
            Some(&git_dir),
            true,
        )
        .unwrap();
        let idx = Index::new_from_config(&cfg);
        assert_eq!(idx.version, INDEX_CONFIG_INVALID_FALLBACK);
    }

    #[test]
    fn fsmonitor_invalidate_directory_prefix_without_trailing_slash() {
        let mut idx = Index::new();
        idx.add_or_replace(make_entry("dir/a.txt"));
        idx.add_or_replace(make_entry("dir/b.txt"));
        idx.set_fsmonitor_last_update(Some("tok".into()));
        for entry in idx.entries_mut() {
            entry.set_fsmonitor_valid(true);
        }
        idx.invalidate_fsmonitor_for_path("dir", false);
        assert!(!idx.get(b"dir/a.txt", 0).unwrap().fsmonitor_valid());
        assert!(!idx.get(b"dir/b.txt", 0).unwrap().fsmonitor_valid());
    }

    #[test]
    fn parse_fsmonitor_extension_rejects_truncated_ewah() {
        let mut payload = vec![0, 0, 0, 2, b't', 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0];
        let err = parse_fsmonitor_extension(&payload).unwrap_err();
        assert!(matches!(
            err,
            Error::IndexFsmonitorExtensionTruncated | Error::IndexFsmonitorExtensionEwahInvalid
        ));
        payload[12..16].copy_from_slice(&2u32.to_be_bytes());
        let err2 = parse_fsmonitor_extension(&payload).unwrap_err();
        assert!(matches!(
            err2,
            Error::IndexFsmonitorExtensionTruncated | Error::IndexFsmonitorExtensionEwahInvalid
        ));
    }

    #[test]
    fn unmerge_from_resolve_undo_installs_conflict_stages() {
        use crate::resolve_undo::ResolveUndoRecord;
        use std::collections::BTreeMap;

        let mut idx = Index::new();
        idx.add_or_replace(make_entry("f.txt"));
        let oid1 = dummy_oid();
        let oid2 = ObjectId::from_bytes(&[1u8; 20]).unwrap();
        idx.resolve_undo = Some(BTreeMap::from([(
            b"f.txt".to_vec(),
            ResolveUndoRecord {
                modes: [MODE_REGULAR, MODE_REGULAR, 0],
                oids: [oid1, oid2, ObjectId::zero()],
            },
        )]));
        assert!(idx.unmerge_path_from_resolve_undo(b"f.txt"));
        assert!(idx.has_unmerged_entries());
    }

    #[test]
    fn parse_rejects_truncated_headers() {
        for n in 0..12 {
            assert!(Index::parse(&vec![0u8; n]).is_err());
        }
    }

    #[test]
    fn entry_from_stat_populates_index_metadata() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("f.txt"), b"payload").unwrap();
        let oid = dummy_oid();
        let entry = entry_from_stat(dir.path(), b"f.txt", oid, MODE_REGULAR).unwrap();
        assert_eq!(entry.path, b"f.txt");
        assert!(entry.size > 0);
    }

    #[test]
    fn write_to_path_honors_skip_hash_flag() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("index");
        let mut idx = Index::new();
        idx.add_or_replace(make_entry("x"));
        idx.write_to_path(&path, true).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert!(bytes.ends_with(&[0u8; 20]));
    }

    #[test]
    fn index_lock_acquire_blocks_second_holder() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("index");
        let mut idx = Index::new();
        idx.add_or_replace(make_entry("tracked.txt"));
        let lock = IndexLock::acquire(&path, None).unwrap();
        assert!(matches!(
            IndexLock::acquire(&path, None),
            Err(Error::LockHeld { .. })
        ));
        drop(lock);
        idx.write(&path).unwrap(); // idx is mut in each test scope
        assert_eq!(Index::load(&path).unwrap().entries.len(), 1);
    }

    #[test]
    fn held_index_lock_is_a_typed_error_naming_the_holder() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("index");
        let lock_path = dir.path().join("index.lock");
        fs::write(&lock_path, b"foreign").unwrap();
        fs::write(
            dir.path().join("index~pid.lock"),
            format!("pid {}\n", std::process::id()),
        )
        .unwrap();

        let Err(acquire_err) = IndexLock::acquire(&path, None) else {
            panic!("lock must be refused");
        };
        let write_err = Index::new().write(&path).unwrap_err();
        for err in [acquire_err, write_err] {
            match &err {
                Error::LockHeld { path: p, holder } => {
                    assert_eq!(p, &lock_path);
                    assert_eq!(
                        *holder,
                        Some(crate::lockfile::LockHolder {
                            pid: u64::from(std::process::id()),
                            running: true,
                        })
                    );
                }
                other => panic!("expected LockHeld, got {other:?}"),
            }
            assert!(
                err.to_string().contains(&lock_path.display().to_string()),
                "{err}"
            );
        }
        assert_eq!(fs::read(&lock_path).unwrap(), b"foreign");
        assert!(!path.exists());
    }

    #[test]
    fn write_with_config_and_parallel_policy() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("index");
        let mut idx = Index::new();
        idx.add_or_replace(make_entry("p"));
        let cfg = crate::config::ConfigSet::new();
        idx.write_with_config_and_policy(&path, &cfg, crate::parallel::ThreadPolicy::default())
            .unwrap();
        assert!(path.is_file());
    }

    #[test]
    fn fsmonitor_sync_and_clear_helpers() {
        let mut idx = Index::new();
        idx.add_or_replace(make_entry("a"));
        idx.set_fsmonitor_last_update(Some("token".to_owned()));
        idx.sync_fsmonitor_dirty_bitmap();
        if let Some(e) = idx.get_mut(b"a", 0) {
            e.set_fsmonitor_valid(false);
        }
        idx.fsmonitor_validity_changed();
        idx.clear_fsmonitor_extension();
        assert!(idx.fsmonitor_last_update.is_none());
    }

    #[test]
    fn intent_to_add_entry_forces_empty_blob_on_write_tree_path() {
        let dir = TempDir::new().unwrap();
        let odb = crate::odb::Odb::new(dir.path());
        let mut idx = Index::new();
        let mut e = make_entry("ita.txt");
        e.set_intent_to_add(true);
        idx.add_or_replace(e);
        crate::write_tree::write_tree_from_index(&odb, &idx, "").unwrap();
    }

    #[test]
    fn optional_load_is_empty_only_when_the_index_is_missing() {
        let dir = TempDir::new().unwrap();
        let odb = crate::odb::Odb::new(dir.path());
        let path = dir.path().join("index");
        let missing = Index::load_expand_sparse_optional(&path, &odb).unwrap();
        assert!(missing.entries.is_empty());

        for len in [0, 12, 31] {
            std::fs::write(&path, b"DIRC\0\0\0\x02\0\0\0\0".repeat(3)[..len].to_vec()).unwrap();
            assert!(
                matches!(
                    Index::load_expand_sparse_optional(&path, &odb),
                    Err(Error::IndexError(_))
                ),
                "len={len}"
            );
        }
    }

    fn naive_merge_sorted(kept: &[IndexEntry], insert: &[IndexEntry]) -> Vec<IndexEntry> {
        let mut out: Vec<IndexEntry> = kept.iter().chain(insert.iter()).cloned().collect();
        out.sort_by(index_entry_path_stage_cmp);
        let mut deduped = Vec::with_capacity(out.len());
        for entry in out {
            if deduped
                .last()
                .is_some_and(|last| index_entry_path_stage_cmp(last, &entry).is_eq())
            {
                *deduped.last_mut().unwrap() = entry;
            } else {
                deduped.push(entry);
            }
        }
        deduped
    }

    #[test]
    fn merge_sorted_index_halves_matches_naive_merge() {
        let paths = [
            ".gitattributes",
            "a.txt",
            "b.txt",
            "f.txt",
            "z.txt",
            "dir/nested.txt",
        ];
        let mut kept = Vec::new();
        let mut insert = Vec::new();
        for (i, path) in paths.iter().enumerate() {
            let mut e = make_entry(path);
            e.oid = ObjectId::from_bytes(&[i as u8; 20]).unwrap();
            kept.push(e);
        }
        for (i, path) in paths.iter().enumerate() {
            if i % 2 == 0 {
                continue;
            }
            let mut e = make_entry(path);
            e.oid = ObjectId::from_bytes(&[(i + 100) as u8; 20]).unwrap();
            insert.push(e);
        }
        kept.sort_by(index_entry_path_stage_cmp);
        insert.sort_by(index_entry_path_stage_cmp);

        let expected = naive_merge_sorted(&kept, &insert);
        let merged = merge_sorted_index_entries(kept, insert);
        assert_eq!(merged, expected, "fixed scenario merge mismatch");

        for seed in 0u64..200 {
            let mut kept = Vec::new();
            let mut insert = Vec::new();
            for n in 0..((seed % 7) + 1) {
                let path = format!("p/{seed}/{n}.txt");
                let mut e = make_entry(&path);
                e.oid = ObjectId::from_bytes(&[(seed as u8).wrapping_add(n as u8); 20]).unwrap();
                kept.push(e);
            }
            for n in 0..((seed % 5) + 1) {
                let path = format!("p/{seed}/{n}.txt");
                let mut e = make_entry(&path);
                e.oid = ObjectId::from_bytes(
                    &[(seed as u8).wrapping_add(n as u8).wrapping_add(50); 20],
                )
                .unwrap();
                insert.push(e);
            }
            kept.sort_by(index_entry_path_stage_cmp);
            insert.sort_by(index_entry_path_stage_cmp);
            let expected = naive_merge_sorted(&kept, &insert);
            let merged = merge_sorted_index_entries(kept, insert);
            assert_eq!(merged, expected, "seed {seed} merge mismatch");
        }
    }

    mod sparse_collapse {
        use super::*;

        const PATTERNS: [&str; 3] = ["/*", "!/*/", "/a/"];

        fn patterns() -> Vec<String> {
            PATTERNS.iter().map(|p| (*p).to_owned()).collect()
        }

        /// Write a tree holding `files` (`path`, `mode`, blob body) and return its OID.
        fn write_tree(odb: &Odb, files: &[(&str, u32, &str)]) -> ObjectId {
            let mut here: BTreeMap<Vec<u8>, (u32, ObjectId)> = BTreeMap::new();
            let mut subdirs: BTreeMap<&str, Vec<(&str, u32, &str)>> = BTreeMap::new();
            for (path, mode, body) in files {
                match path.split_once('/') {
                    Some((dir, rest)) => subdirs.entry(dir).or_default().push((rest, *mode, body)),
                    None => {
                        let oid = if *mode == MODE_GITLINK {
                            ObjectId::from_bytes(&[7u8; 20]).unwrap()
                        } else {
                            odb.write(ObjectKind::Blob, body.as_bytes()).unwrap()
                        };
                        here.insert(path.as_bytes().to_vec(), (*mode, oid));
                    }
                }
            }
            for (dir, children) in subdirs {
                here.insert(
                    dir.as_bytes().to_vec(),
                    (MODE_TREE, write_tree(odb, &children)),
                );
            }
            let mut entries: Vec<TreeEntry> = here
                .into_iter()
                .map(|(name, (mode, oid))| TreeEntry { mode, name, oid })
                .collect();
            entries.sort_by(|a, b| {
                crate::objects::tree_entry_cmp(
                    &a.name,
                    a.mode == MODE_TREE,
                    &b.name,
                    b.mode == MODE_TREE,
                )
            });
            odb.write(ObjectKind::Tree, &crate::objects::serialize_tree(&entries))
                .unwrap()
        }

        fn subtree(odb: &Odb, root: &ObjectId, prefix: &str) -> ObjectId {
            PrefixTreeResolver::new(*root)
                .resolve(odb, prefix.as_bytes())
                .unwrap()
                .unwrap()
        }

        /// The fully expanded index for `root`, with everything outside `a/` and the top
        /// level marked skip-worktree.
        fn expanded_index(odb: &Odb, root: &ObjectId) -> Index {
            let mut index = Index::new();
            let rows = flatten_tree_blobs(
                odb,
                root,
                b"",
                crate::path_validate::ProtectOptions::default(),
            )
            .unwrap();
            for mut row in rows {
                let in_cone = !row.path.contains(&b'/') || row.path.starts_with(b"a/");
                row.set_skip_worktree(!in_cone);
                index.entries_mut().push(row);
            }
            index.sort();
            index
        }

        fn collapse(index: &mut Index, odb: &Odb, root: &ObjectId) {
            index
                .try_collapse_sparse_directories(
                    odb,
                    root,
                    &patterns(),
                    true,
                    true,
                    crate::path_validate::ProtectOptions::default(),
                )
                .unwrap();
        }

        fn rows(index: &Index) -> Vec<(String, u32, ObjectId)> {
            index
                .entries()
                .iter()
                .map(|e| (String::from_utf8_lossy(&e.path).into_owned(), e.mode, e.oid))
                .collect()
        }

        fn placeholder_paths(index: &Index) -> Vec<String> {
            index
                .entries()
                .iter()
                .filter(|e| e.is_sparse_directory_placeholder())
                .map(|e| String::from_utf8_lossy(&e.path).into_owned())
                .collect()
        }

        const FILES: [(&str, u32, &str); 8] = [
            ("a/in", 0o100644, "in\n"),
            ("top", 0o100644, "top\n"),
            ("w/f4", 0o100644, "f4\n"),
            ("w/f5", 0o100755, "f5\n"),
            ("x/f1", 0o100644, "f1\n"),
            ("x/y/f2", 0o100644, "f2\n"),
            ("x/y/z/f3", 0o100644, "f3\n"),
            ("x/y-z", 0o100644, "dash\n"),
        ];

        #[test]
        fn unchanged_excluded_directories_collapse_to_their_outermost_placeholder() {
            let dir = TempDir::new().unwrap();
            let odb = Odb::new(dir.path());
            let root = write_tree(&odb, &FILES);
            let mut index = expanded_index(&odb, &root);
            collapse(&mut index, &odb, &root);

            assert_eq!(
                rows(&index),
                vec![
                    ("a/in".to_owned(), 0o100644, index.entries()[0].oid),
                    ("top".to_owned(), 0o100644, index.entries()[1].oid),
                    ("w/".to_owned(), MODE_TREE, subtree(&odb, &root, "w")),
                    ("x/".to_owned(), MODE_TREE, subtree(&odb, &root, "x")),
                ]
            );
            assert!(index.sparse_directories);
            assert!(index.entries_are_sorted());
        }

        #[test]
        fn a_changed_row_keeps_its_ancestors_expanded_but_not_its_siblings() {
            let dir = TempDir::new().unwrap();
            let odb = Odb::new(dir.path());
            let root = write_tree(&odb, &FILES);
            let mut index = expanded_index(&odb, &root);
            let edited = odb.write(ObjectKind::Blob, b"edited\n").unwrap();
            let pos = index.find_entry_pos(b"x/f1", 0).unwrap();
            index.entries_mut()[pos].oid = edited;
            index.sort();
            collapse(&mut index, &odb, &root);

            assert_eq!(
                placeholder_paths(&index),
                vec!["w/".to_owned(), "x/y/".to_owned()]
            );
            let kept: Vec<String> = rows(&index).into_iter().map(|(p, _, _)| p).collect();
            assert_eq!(kept, ["a/in", "top", "w/", "x/f1", "x/y-z", "x/y/"]);
            let f1 = &index.entries()[index.find_entry_pos(b"x/f1", 0).unwrap()];
            assert_eq!(f1.oid, edited);
            let y = &index.entries()[index.find_entry_pos(b"x/y/", 0).unwrap()];
            assert_eq!(y.oid, subtree(&odb, &root, "x/y"));
        }

        #[test]
        fn directories_with_worktree_rows_gitlinks_or_conflicts_stay_expanded() {
            let dir = TempDir::new().unwrap();
            let odb = Odb::new(dir.path());
            let mut files = FILES.to_vec();
            files.push(("v/sub", MODE_GITLINK, ""));
            files.push(("u/f6", 0o100644, "f6\n"));
            let root = write_tree(&odb, &files);
            let mut index = expanded_index(&odb, &root);
            let pos = index.find_entry_pos(b"w/f4", 0).unwrap();
            index.entries_mut()[pos].set_skip_worktree(false);
            let pos = index.find_entry_pos(b"u/f6", 0).unwrap();
            let mut theirs = index.entries()[pos].clone();
            index.entries_mut()[pos].set_stage(2);
            theirs.set_stage(3);
            index.entries_mut().push(theirs);
            index.sort();
            collapse(&mut index, &odb, &root);

            assert_eq!(placeholder_paths(&index), vec!["x/".to_owned()]);
            for path in ["u/f6", "v/sub", "w/f4", "w/f5"] {
                assert!(
                    index.entries().iter().any(|e| e.path == path.as_bytes()),
                    "{path} must stay expanded"
                );
            }
        }

        #[test]
        fn existing_placeholders_count_only_when_they_match_head() {
            let dir = TempDir::new().unwrap();
            let odb = Odb::new(dir.path());
            let root = write_tree(&odb, &FILES);
            let y = subtree(&odb, &root, "x/y");

            let with_y_placeholder = |oid: ObjectId| {
                let mut index = expanded_index(&odb, &root);
                index.entries_mut().retain(|e| !e.path.starts_with(b"x/y/"));
                let mut placeholder = index.entries()[0].clone();
                placeholder.path = b"x/y/".to_vec();
                placeholder.mode = MODE_TREE;
                placeholder.oid = oid;
                placeholder.flags = 4;
                placeholder.set_skip_worktree(true);
                index.entries_mut().push(placeholder);
                index.sort();
                index
            };

            let mut matching = with_y_placeholder(y);
            collapse(&mut matching, &odb, &root);
            assert_eq!(
                placeholder_paths(&matching),
                vec!["w/".to_owned(), "x/".to_owned()]
            );

            let stale = odb.write(ObjectKind::Tree, b"").unwrap();
            let mut outdated = with_y_placeholder(stale);
            collapse(&mut outdated, &odb, &root);
            assert_eq!(
                placeholder_paths(&outdated),
                vec!["w/".to_owned(), "x/y/".to_owned()]
            );
            let y_row = &outdated.entries()[outdated.find_entry_pos(b"x/y/", 0).unwrap()];
            assert_eq!(y_row.oid, stale, "an outdated placeholder is left alone");
            assert!(outdated.entries().iter().any(|e| e.path == b"x/f1"));
        }

        #[test]
        fn collapsed_directories_subsume_nested_ones_and_cover_their_rows() {
            let oid = ObjectId::from_bytes(&[1u8; 20]).unwrap();
            let mut collapsed = CollapsedDirectories::default();
            collapsed.insert(b"a/b/".to_vec(), oid);
            collapsed.insert(b"a/c/".to_vec(), oid);
            assert!(collapsed.has_descendants(b"a/"));
            assert!(!collapsed.has_descendants(b"a/b/"));
            collapsed.insert(b"a/".to_vec(), oid);
            assert!(!collapsed.has_descendants(b"a/"));
            assert_eq!(collapsed.dirs.len(), 1);

            let mut scratch = Vec::new();
            for (path, covered) in [
                (&b"a/x"[..], true),
                (b"a/b/c", true),
                (b"a", true),
                (b"a-b", false),
                (b"ab/c", false),
                (b"0", false),
            ] {
                assert_eq!(
                    collapsed.covers(path, &mut scratch),
                    covered,
                    "{}",
                    String::from_utf8_lossy(path)
                );
            }

            let entry = |path: &str| IndexEntry {
                path: path.as_bytes().to_vec(),
                ..make_entry(path)
            };
            let entries = vec![
                entry("0"),
                entry("a"),
                entry("a-b"),
                entry("a/b/c"),
                entry("ab"),
            ];
            assert_eq!(
                collapsed.covered_rows(&entries),
                vec![false, true, false, true, false]
            );
        }
    }
}
