//! Write Git pack reachability bitmap (`.bitmap`) sidecars for a single pack index.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use tempfile::NamedTempFile;
use thiserror::Error;

use crate::ewah_bitmap::{Bitmap, EwahBitmap};
use crate::gc::collect_referenced_object_roots;
use crate::ident::parse_signature_times;
use crate::objects::{parse_commit, parse_tag, Object, ObjectId, ObjectKind, TreeRefIter};
use crate::pack::{read_pack_index, PackIndex, SingleLocalPackReader};
use crate::pack_name_hash::pack_name_hash_extend;
use crate::pack_rev::{
    append_hashfile_checksum, build_pack_rev_bytes_from_index_order_offsets_and_checksum,
    rev_path_for_index, verify_pack_rev_file,
};
use crate::refs;
use crate::repo::Repository;

use super::order::BitmapOrder;

const BITMAP_MAGIC: &[u8; 4] = b"BITM";
const BITMAP_VERSION: u16 = 1;
const BITMAP_OPT_FULL_DAG: u16 = 0x1;
const BITMAP_OPT_HASH_CACHE: u16 = 0x4;
const BITMAP_OPT_LOOKUP_TABLE: u16 = 0x10;
const MAX_XOR_SEARCH: usize = 10;
const XOR_ROW_NONE: u32 = 0xffff_ffff;

/// Rank-based spacing for history sampling (newest commit = rank 0).
///
/// Calibrated against system `git repack -adb` on a full [git.git](https://github.com/git/git)
/// clone (~62k commits, ~321 bitmap entries — denser near HEAD, sparser in ancient history).
/// This plan uses rank tiers and linear stride growth, not Git's window-index function.
#[derive(Debug, Clone, Copy)]
struct HistorySamplePlan {
    /// Ranks in `[0, recent_dense_end)` are visited with stride zero (every window picks HEAD of slice).
    recent_dense_end: usize,
    /// Ranks below this use [`Self::mid_stride`].
    mid_history_end: usize,
    mid_stride_base: usize,
    mid_stride_step: usize,
    far_stride_base: usize,
    far_stride_step: usize,
}

impl Default for HistorySamplePlan {
    fn default() -> Self {
        Self {
            recent_dense_end: 96,
            mid_history_end: 4096,
            mid_stride_base: 12,
            mid_stride_step: 3,
            far_stride_base: 384,
            far_stride_step: 48,
        }
    }
}

impl HistorySamplePlan {
    fn mid_stride(&self, rank: usize) -> usize {
        debug_assert!(rank >= self.recent_dense_end && rank < self.mid_history_end);
        let depth = rank - self.recent_dense_end;
        self.mid_stride_base + (depth / 64) * self.mid_stride_step
    }

    fn far_stride(&self, rank: usize) -> usize {
        debug_assert!(rank >= self.mid_history_end);
        let depth = rank - self.mid_history_end;
        self.far_stride_base + (depth / 32) * self.far_stride_step
    }

    fn gap_before_window(&self, rank: usize) -> usize {
        if rank < self.recent_dense_end {
            0
        } else if rank < self.mid_history_end {
            self.mid_stride(rank)
        } else {
            self.far_stride(rank)
        }
    }
}

/// Options controlling pack bitmap generation (caller-supplied; not read from the environment).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackBitmapWriteOptions {
    /// Write `BITMAP_OPT_FULL_DAG` (required for Git-compatible readers).
    pub full_dag: bool,
    /// Include the v1 name-hash tail section.
    pub hash_cache: bool,
    /// Include the commit lookup table extension.
    pub lookup_table: bool,
    /// Ref name patterns (`refs/heads/`, …) that prefer tip commits inside each selection window.
    pub prefer_bitmap_tips: Vec<String>,
    /// When true, every object reachable from ref tips must appear in the bitmap order.
    pub verify_reachability_closure: bool,
}

impl Default for PackBitmapWriteOptions {
    fn default() -> Self {
        Self {
            full_dag: true,
            hash_cache: true,
            lookup_table: false,
            prefer_bitmap_tips: Vec::new(),
            verify_reachability_closure: true,
        }
    }
}

/// Failure while writing a pack reachability bitmap.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum PackBitmapWriteError {
    /// An object reachable from the chosen tips is missing from the pack index.
    #[error("pack is not closed under reachability from tips")]
    NotClosed {
        /// Object ids missing from the pack or unreachable from the validated ref roots.
        missing: Vec<ObjectId>,
    },
    /// The repository does not use a files-backed object store.
    #[error("bitmap write requires a files-backed object database")]
    NotFilesBacked,
    /// Pack index or sidecar path is invalid.
    #[error("invalid pack index path: {0}")]
    InvalidPackPath(String),
    /// Unexpected I/O failure.
    #[error("I/O error: {0}")]
    Io(String),
    /// Object database or pack data could not be read.
    #[error("object read error: {0}")]
    Object(String),
}

impl PackBitmapWriteError {
    fn io(err: std::io::Error) -> Self {
        Self::Io(err.to_string())
    }

    /// Missing object ids when [`Self::NotClosed`] ended the write.
    #[must_use]
    pub fn not_closed_missing(&self) -> Option<&[ObjectId]> {
        match self {
            Self::NotClosed { missing } => Some(missing.as_slice()),
            _ => None,
        }
    }
}

/// Writes `.bitmap` sidecars for multi-pack-index files (MIDX checksum in header).
pub struct MidxBitmapWriter;

impl MidxBitmapWriter {
    /// Write a reachability bitmap for a MIDX layer that was just serialized to `midx_bytes`.
    pub fn write_for_midx_layer(
        repo: &Repository,
        midx_bytes: &[u8],
        rid_order: &[u32],
        midx_checksum: &[u8],
        bitmap_path: &Path,
        options: &PackBitmapWriteOptions,
    ) -> Result<PathBuf, PackBitmapWriteError> {
        let order = BitmapOrder::from_midx_bytes_and_rid_order(midx_bytes, rid_order)
            .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?;
        write_bitmap_for_order(repo, &order, midx_checksum, bitmap_path, options)
    }

    /// Write a reachability bitmap for the tip MIDX under `pack_dir`.
    ///
    /// The bitmap header checksum must match the MIDX file checksum (`midx_checksum`).
    ///
    /// # Errors
    ///
    /// Returns [`PackBitmapWriteError::NotClosed`] when objects reachable from ref
    /// tips are missing from the MIDX pseudo-pack order.
    pub fn write(
        repo: &Repository,
        _pack_dir: &Path,
        midx_checksum: &[u8],
        bitmap_path: &Path,
        options: &PackBitmapWriteOptions,
        _now: SystemTime,
    ) -> Result<PathBuf, PackBitmapWriteError> {
        if !repo.odb().uses_files_primary() {
            return Err(PackBitmapWriteError::NotFilesBacked);
        }
        let objects_dir = repo
            .odb()
            .files_objects_dir()
            .ok_or(PackBitmapWriteError::NotFilesBacked)?;
        let order = BitmapOrder::load_midx(objects_dir)
            .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?
            .ok_or_else(|| PackBitmapWriteError::InvalidPackPath("no MIDX loaded".into()))?;
        write_bitmap_for_order(repo, &order, midx_checksum, bitmap_path, options)
    }
}

/// Writes `.bitmap` sidecars for pack indexes produced by all-into-one repacks.
pub struct PackBitmapWriter;

impl PackBitmapWriter {
    /// Write a reachability bitmap for the pack identified by `pack_idx_path`.
    ///
    /// When no `.rev` sidecar exists, one is created first so object order matches Git.
    ///
    /// # Errors
    ///
    /// Returns [`PackBitmapWriteError::NotClosed`] when the pack is not the full
    /// reachability closure from the ref tips used for validation.
    pub fn write(
        repo: &Repository,
        pack_idx_path: &Path,
        options: &PackBitmapWriteOptions,
        _now: SystemTime,
    ) -> Result<PathBuf, PackBitmapWriteError> {
        if !repo.odb().uses_files_primary() {
            return Err(PackBitmapWriteError::NotFilesBacked);
        }
        let idx = read_pack_index(pack_idx_path)
            .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?;
        let objects_dir = repo
            .odb()
            .files_objects_dir()
            .ok_or(PackBitmapWriteError::NotFilesBacked)?;
        ensure_rev_sidecar(&idx)?;
        let order = BitmapOrder::load_pack(objects_dir, Arc::new(idx))
            .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?;
        let pack_checksum = read_pack_trailer(
            order.pack_index().as_ref().ok_or_else(|| {
                PackBitmapWriteError::InvalidPackPath("pack index missing".into())
            })?,
            repo.odb().hash_algo().len(),
        )?;
        let bitmap_path = bitmap_path_for_index(pack_idx_path);
        write_bitmap_for_order(repo, &order, &pack_checksum, &bitmap_path, options)
    }
}

/// Shared bitmap writer core (pack order or MIDX pseudo-pack order).
///
/// Runs inside the object database's pack read context: the writer reads every commit and
/// tree, and the context also lets [`pack_entry_kinds`] map the pack directly.
fn write_bitmap_for_order(
    repo: &Repository,
    order: &BitmapOrder,
    header_checksum: &[u8],
    bitmap_path: &Path,
    options: &PackBitmapWriteOptions,
) -> Result<PathBuf, PackBitmapWriteError> {
    repo.odb().with_pack_read_context(|| {
        write_bitmap_for_order_in_context(repo, order, header_checksum, bitmap_path, options)
    })
}

fn write_bitmap_for_order_in_context(
    repo: &Repository,
    order: &BitmapOrder,
    header_checksum: &[u8],
    bitmap_path: &Path,
    options: &PackBitmapWriteOptions,
) -> Result<PathBuf, PackBitmapWriteError> {
    let roots = collect_referenced_object_roots(repo.git_dir())
        .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?;
    let strict = options.verify_reachability_closure;
    let (type_bitmaps, commits) = build_type_bitmaps_and_commits(repo, order)?;
    let tips = commit_tips_from_roots(repo, order, &commits, &roots, strict)?;
    let needs_bitmap = tip_needs_flags(repo, &tips, &options.prefer_bitmap_tips)?;
    let hash_cache_len = hash_cache_row_count(order);
    let mut name_hashes = vec![0u32; hash_cache_len];
    let mut selected = select_commits(&commits.rows, &tips, &needs_bitmap);
    if !strict {
        selected.retain(|c| order.position_of(c).is_some());
    }
    let commit_bitmaps =
        build_commit_bitmaps(repo, order, &commits, &selected, &mut name_hashes, strict)?;
    let (selected_sorted, xor_plan) = plan_xor_compression(&commits, &selected, &commit_bitmaps);
    let bytes = assemble_bitmap_bytes(
        order,
        header_checksum,
        options,
        &type_bitmaps,
        &selected_sorted,
        &xor_plan,
        &name_hashes,
    )?;
    write_bitmap_atomically(bitmap_path, &bytes)?;
    repo.caches().bitmap_index().set(None);
    Ok(bitmap_path.to_path_buf())
}

fn bitmap_path_for_index(idx_path: &Path) -> PathBuf {
    let stem = idx_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("pack");
    idx_path
        .parent()
        .map(|p| p.join(format!("{stem}.bitmap")))
        .unwrap_or_else(|| PathBuf::from(format!("{stem}.bitmap")))
}

fn read_pack_trailer(idx: &PackIndex, hash_len: usize) -> Result<Vec<u8>, PackBitmapWriteError> {
    let mut file = fs::File::open(&idx.pack_path).map_err(PackBitmapWriteError::io)?;
    let len = file.metadata().map_err(PackBitmapWriteError::io)?.len();
    let trailer_len = hash_len as u64;
    if len < trailer_len {
        return Err(PackBitmapWriteError::InvalidPackPath(
            "pack file too short".into(),
        ));
    }
    file.seek(SeekFrom::Start(len - trailer_len))
        .map_err(PackBitmapWriteError::io)?;
    let mut trailer = vec![0u8; hash_len];
    file.read_exact(&mut trailer)
        .map_err(PackBitmapWriteError::io)?;
    Ok(trailer)
}

fn ensure_rev_sidecar(idx: &PackIndex) -> Result<(), PackBitmapWriteError> {
    let rev_path = rev_path_for_index(&idx.idx_path);
    if rev_path.is_file() && verify_pack_rev_file(&rev_path, idx).is_ok() {
        return Ok(());
    }
    let hash_len = idx.hash_bytes();
    let pack_checksum = read_pack_trailer(idx, hash_len)?;
    let offsets: Vec<u64> = (0..idx.len())
        .map(|i| idx.offset_at(i))
        .collect::<crate::error::Result<Vec<_>>>()
        .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?;
    let rev_bytes =
        build_pack_rev_bytes_from_index_order_offsets_and_checksum(&offsets, &pack_checksum);
    let tmp =
        NamedTempFile::new_in(rev_path.parent().ok_or_else(|| {
            PackBitmapWriteError::InvalidPackPath("rev path has no parent".into())
        })?)
        .map_err(PackBitmapWriteError::io)?;
    tmp.as_file()
        .write_all(&rev_bytes)
        .map_err(PackBitmapWriteError::io)?;
    tmp.persist(&rev_path)
        .map_err(|e| PackBitmapWriteError::io(e.error))?;
    Ok(())
}

fn write_bitmap_atomically(path: &Path, bytes: &[u8]) -> Result<(), PackBitmapWriteError> {
    let parent = path
        .parent()
        .ok_or_else(|| PackBitmapWriteError::InvalidPackPath("bitmap path has no parent".into()))?;
    let tmp = NamedTempFile::new_in(parent).map_err(PackBitmapWriteError::io)?;
    tmp.as_file()
        .write_all(bytes)
        .map_err(PackBitmapWriteError::io)?;
    tmp.persist(path)
        .map_err(|e| PackBitmapWriteError::io(e.error))?;
    Ok(())
}

#[derive(Clone)]
struct CommitRow {
    oid: ObjectId,
    committer_date: i64,
    tree: ObjectId,
    parents: Vec<ObjectId>,
}

/// Every commit in the bitmap order, parsed once so later phases never re-read commits.
struct CommitTable {
    /// Newest committer date first (stable on ties, so pack order breaks them).
    rows: Vec<CommitRow>,
    by_oid: HashMap<ObjectId, usize>,
}

impl CommitTable {
    fn new(mut rows: Vec<CommitRow>) -> Self {
        rows.sort_by_key(|c| std::cmp::Reverse(c.committer_date));
        let by_oid = rows.iter().enumerate().map(|(i, c)| (c.oid, i)).collect();
        Self { rows, by_oid }
    }

    fn index_of(&self, oid: &ObjectId) -> Option<usize> {
        self.by_oid.get(oid).copied()
    }

    fn get(&self, oid: &ObjectId) -> Option<&CommitRow> {
        self.index_of(oid).map(|i| &self.rows[i])
    }

    /// Committer date of `oid`, or `0` for commits outside the order.
    fn date(&self, oid: &ObjectId) -> i64 {
        self.get(oid).map_or(0, |c| c.committer_date)
    }

    /// Parents of `oid`, reading commits outside the order from the object database.
    fn parents(
        &self,
        repo: &Repository,
        oid: &ObjectId,
    ) -> Result<Cow<'_, [ObjectId]>, PackBitmapWriteError> {
        if let Some(row) = self.get(oid) {
            return Ok(Cow::Borrowed(&row.parents));
        }
        let obj = read_object(repo, oid)?;
        let commit =
            parse_commit(&obj.data).map_err(|e| PackBitmapWriteError::Object(e.to_string()))?;
        Ok(Cow::Owned(commit.parents))
    }
}

fn read_object(repo: &Repository, oid: &ObjectId) -> Result<Object, PackBitmapWriteError> {
    repo.odb()
        .read(oid)
        .map_err(|e| PackBitmapWriteError::Object(e.to_string()))
}

/// Kind of each position of a single-pack `order`, read from pack entry headers.
///
/// Empty for MIDX orders or when the pack cannot be mapped directly; entries are `None` where
/// the headers alone cannot tell (see
/// [`SingleLocalPackReader::object_kinds_at_sorted_offsets`]).
fn pack_entry_kinds(
    repo: &Repository,
    order: &BitmapOrder,
) -> Result<Vec<Option<ObjectKind>>, PackBitmapWriteError> {
    let (Some(idx), Some(objects_dir)) = (order.pack_index(), repo.odb().files_objects_dir())
    else {
        return Ok(Vec::new());
    };
    let Some(reader) = SingleLocalPackReader::open_for_pack_index(objects_dir, Arc::clone(&idx))
        .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?
    else {
        return Ok(Vec::new());
    };
    let offsets = (0..order.object_count())
        .map(|pos| {
            let row = order.storage_row_for_bitmap_rank(pos).ok_or_else(|| {
                PackBitmapWriteError::Object("bitmap position out of range".into())
            })?;
            idx.offset_at(row as usize)
                .map_err(|e| PackBitmapWriteError::Object(e.to_string()))
        })
        .collect::<Result<Vec<u64>, _>>()?;
    reader
        .object_kinds_at_sorted_offsets(&offsets)
        .map_err(|e| PackBitmapWriteError::Object(e.to_string()))
}

/// Type bitmaps for every position in `order`, plus the parsed commits.
///
/// Only commits are inflated; other objects' types come from pack entry headers where possible,
/// and from [`crate::odb::Odb::read_info`] otherwise.
fn build_type_bitmaps_and_commits(
    repo: &Repository,
    order: &BitmapOrder,
) -> Result<(TypeBitmaps, CommitTable), PackBitmapWriteError> {
    let mut commits = Vec::new();
    let mut commit_bm = Bitmap::new();
    let mut trees = Bitmap::new();
    let mut blobs = Bitmap::new();
    let mut tags = Bitmap::new();
    let header_kinds = pack_entry_kinds(repo, order)?;
    let n = order.object_count();
    for pos in 0..n {
        let oid = order
            .oid_at(pos)
            .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?;
        let p = pos as usize;
        let kind = match header_kinds.get(p).copied().flatten() {
            Some(kind) => kind,
            None => {
                repo.odb()
                    .read_info(&oid)
                    .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?
                    .kind
            }
        };
        match kind {
            ObjectKind::Commit => {
                commit_bm.set(p);
                let obj = read_object(repo, &oid)?;
                let c = parse_commit(&obj.data)
                    .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?;
                let committer_date = parse_signature_times(&c.committer)
                    .map(|t| t.unix_seconds)
                    .unwrap_or(0);
                commits.push(CommitRow {
                    oid,
                    committer_date,
                    tree: c.tree,
                    parents: c.parents,
                });
            }
            ObjectKind::Tree => trees.set(p),
            ObjectKind::Blob => blobs.set(p),
            ObjectKind::Tag => tags.set(p),
        }
    }
    Ok((
        TypeBitmaps {
            commits: EwahBitmap::from_bitmap(&commit_bm),
            trees: EwahBitmap::from_bitmap(&trees),
            blobs: EwahBitmap::from_bitmap(&blobs),
            tags: EwahBitmap::from_bitmap(&tags),
        },
        CommitTable::new(commits),
    ))
}

/// Peel every root to the commit it names, sorted and deduplicated.
///
/// With `strict`, also checks that each root, every tag on its peel chain, and the full
/// closure of roots that peel to a tree are in `order`. Closure under commits is checked by
/// the strict bitmap walk instead: every tip is selected, and that walk fails on the first
/// reachable object missing from the order.
///
/// # Errors
///
/// [`PackBitmapWriteError::NotClosed`] lists up to 32 missing objects;
/// [`PackBitmapWriteError::Object`] reports unreadable or unparsable objects.
fn commit_tips_from_roots(
    repo: &Repository,
    order: &BitmapOrder,
    commits: &CommitTable,
    roots: &[ObjectId],
    strict: bool,
) -> Result<Vec<ObjectId>, PackBitmapWriteError> {
    const MAX_REPORTED_MISSING: usize = 32;
    let mut tips = Vec::new();
    let mut missing = Vec::new();
    let mut seen_trees = HashSet::new();
    for &root in roots {
        let mut oid = root;
        loop {
            if strict && order.position_of(&oid).is_none() {
                missing.push(oid);
                break;
            }
            if commits.get(&oid).is_some() {
                tips.push(oid);
                break;
            }
            let obj = read_object(repo, &oid)?;
            match obj.kind {
                ObjectKind::Commit => {
                    tips.push(oid);
                    break;
                }
                ObjectKind::Tag => {
                    oid = parse_tag(&obj.data)
                        .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?
                        .object;
                }
                ObjectKind::Tree => {
                    if strict {
                        collect_missing_in_tree(repo, order, oid, &mut seen_trees, &mut missing)?;
                    }
                    break;
                }
                ObjectKind::Blob => break,
            }
        }
        if missing.len() >= MAX_REPORTED_MISSING {
            break;
        }
    }
    if !missing.is_empty() {
        missing.truncate(MAX_REPORTED_MISSING);
        return Err(PackBitmapWriteError::NotClosed { missing });
    }
    tips.sort_by_key(|o| o.to_hex());
    tips.dedup();
    Ok(tips)
}

/// Record objects reachable from `tree` (already known to be in `order`) that the order lacks.
fn collect_missing_in_tree(
    repo: &Repository,
    order: &BitmapOrder,
    tree: ObjectId,
    seen: &mut HashSet<ObjectId>,
    missing: &mut Vec<ObjectId>,
) -> Result<(), PackBitmapWriteError> {
    let hash_len = repo.odb().hash_algo().len();
    let mut stack = vec![tree];
    while let Some(tree_oid) = stack.pop() {
        if !seen.insert(tree_oid) {
            continue;
        }
        let obj = read_object(repo, &tree_oid)?;
        for entry in TreeRefIter::with_oid_len(&obj.data, hash_len) {
            let entry = entry.map_err(|e| PackBitmapWriteError::Object(e.to_string()))?;
            let kind = TreeChild::from_mode(entry.mode);
            if kind == TreeChild::Gitlink {
                continue;
            }
            if order.position_of(&entry.oid).is_none() {
                missing.push(entry.oid);
            } else if kind == TreeChild::Tree {
                stack.push(entry.oid);
            }
        }
    }
    Ok(())
}

/// What a tree entry points at, decided by its mode alone (Git never stores the type).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TreeChild {
    Tree,
    Gitlink,
    Blob,
}

impl TreeChild {
    const TYPE_MASK: u32 = 0o170000;
    const TREE: u32 = 0o040000;
    const GITLINK: u32 = 0o160000;

    fn from_mode(mode: u32) -> Self {
        match mode & Self::TYPE_MASK {
            Self::TREE => Self::Tree,
            Self::GITLINK => Self::Gitlink,
            _ => Self::Blob,
        }
    }
}

fn tip_needs_flags(
    repo: &Repository,
    tips: &[ObjectId],
    prefer_patterns: &[String],
) -> Result<HashSet<ObjectId>, PackBitmapWriteError> {
    let mut needs = HashSet::new();
    if prefer_patterns.is_empty() {
        needs.extend(tips.iter().copied());
        return Ok(needs);
    }
    let git_dir = &repo.git_dir();
    for (name, oid) in refs::list_refs(git_dir, "refs/")
        .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?
    {
        let peeled = peel_to_commit(repo, oid)?;
        let Some(commit) = peeled else { continue };
        if prefer_patterns
            .iter()
            .any(|pat| ref_matches_pattern(&name, pat))
        {
            needs.insert(commit);
        }
    }
    needs.extend(tips.iter().copied());
    Ok(needs)
}

fn ref_matches_pattern(ref_name: &str, pattern: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    if ref_name == pattern {
        return true;
    }
    if pattern.ends_with('/') {
        ref_name.starts_with(pattern) || format!("{ref_name}/").starts_with(pattern)
    } else {
        ref_name == pattern
            || ref_name
                .strip_prefix("refs/")
                .is_some_and(|r| r == pattern || r.starts_with(&format!("{pattern}/")))
    }
}

fn peel_to_commit(
    repo: &Repository,
    oid: ObjectId,
) -> Result<Option<ObjectId>, PackBitmapWriteError> {
    repo.odb()
        .peel_to_commit(oid)
        .map_err(|e| PackBitmapWriteError::Object(e.to_string()))
}

fn pick_window_commit(window: &[CommitRow], needs_bitmap: &HashSet<ObjectId>) -> ObjectId {
    if window.len() == 1 {
        return window[0].oid;
    }
    let mut chosen = window[window.len() - 1].oid;
    for cm in window {
        if needs_bitmap.contains(&cm.oid) {
            return cm.oid;
        }
        if cm.parents.len() > 1 {
            chosen = cm.oid;
        }
    }
    chosen
}

fn select_commits(
    commits: &[CommitRow],
    tips: &[ObjectId],
    needs_bitmap: &HashSet<ObjectId>,
) -> Vec<ObjectId> {
    let mut selected: HashSet<ObjectId> = tips.iter().copied().collect();
    let nr = commits.len();
    let plan = HistorySamplePlan::default();
    if nr <= plan.recent_dense_end {
        selected.extend(commits.iter().map(|c| c.oid));
    } else {
        let mut rank = 0usize;
        while rank < nr {
            let gap = plan.gap_before_window(rank);
            if rank + gap >= nr {
                break;
            }
            let window_end = rank + gap;
            let window = &commits[rank..=window_end];
            selected.insert(pick_window_commit(window, needs_bitmap));
            rank = window_end + 1;
        }
    }
    let mut list: Vec<ObjectId> = selected.into_iter().collect();
    list.sort_by_key(|o| o.to_hex());
    list
}

struct TypeBitmaps {
    commits: EwahBitmap,
    trees: EwahBitmap,
    blobs: EwahBitmap,
    tags: EwahBitmap,
}

fn build_commit_bitmaps(
    repo: &Repository,
    order: &BitmapOrder,
    commits: &CommitTable,
    selected: &[ObjectId],
    name_hashes: &mut [u32],
    strict_closure: bool,
) -> Result<HashMap<ObjectId, Bitmap>, PackBitmapWriteError> {
    let mut topo = selected.to_vec();
    topo.extend(first_parent_fork_points(commits, selected));
    topo.sort_by_key(|a| commits.date(a));
    let mut fill = TreeBitmapFill {
        repo,
        order,
        hash_len: repo.odb().hash_algo().len(),
        name_hashes,
        strict_closure,
    };
    let mut computed: HashMap<ObjectId, Bitmap> = HashMap::new();
    for commit in topo {
        let mut bm = seed_from_computed_ancestors(repo, commits, commit, &computed)?;
        fill_commit_bitmap(&mut fill, commits, &mut bm, commit, &computed)?;
        computed.insert(commit, bm);
    }
    Ok(computed)
}

/// Unselected commits where the first-parent chains of two or more selected commits meet.
///
/// A release branch tip and the next selected commit on the branch it forked from both reach
/// the history below the fork point, and neither bitmap contains the other. Computing an
/// (unwritten) bitmap at the fork lets both seed from it, so that history is walked once.
/// Only first-parent edges are followed: side branches merged back are already covered by the
/// selected commit below the merge.
fn first_parent_fork_points(commits: &CommitTable, selected: &[ObjectId]) -> Vec<ObjectId> {
    let mut visited = vec![false; commits.rows.len()];
    let mut children = vec![0u32; commits.rows.len()];
    let mut is_selected = vec![false; commits.rows.len()];
    let starts: Vec<usize> = selected
        .iter()
        .filter_map(|c| commits.index_of(c))
        .collect();
    for &i in &starts {
        is_selected[i] = true;
    }
    for &start in &starts {
        if std::mem::replace(&mut visited[start], true) {
            continue;
        }
        let mut cur = start;
        while let Some(parent) = commits.rows[cur]
            .parents
            .first()
            .and_then(|p| commits.index_of(p))
        {
            children[parent] += 1;
            if std::mem::replace(&mut visited[parent], true) {
                break;
            }
            cur = parent;
        }
    }
    let mut forks: Vec<ObjectId> = (0..commits.rows.len())
        .filter(|&i| children[i] >= 2 && !is_selected[i])
        .map(|i| commits.rows[i].oid)
        .collect();
    forks.sort_by_key(|o| o.to_hex());
    forks
}

fn seed_from_computed_ancestors(
    repo: &Repository,
    commits: &CommitTable,
    commit: ObjectId,
    computed: &HashMap<ObjectId, Bitmap>,
) -> Result<Bitmap, PackBitmapWriteError> {
    let mut bm = Bitmap::new();
    for &parent in commits.parents(repo, &commit)?.iter() {
        let mut walk = parent;
        loop {
            if let Some(ancestor_bm) = computed.get(&walk) {
                bm.or_assign(ancestor_bm);
                break;
            }
            match commits.parents(repo, &walk)?.first().copied() {
                Some(next) => walk = next,
                None => break,
            }
        }
    }
    Ok(bm)
}

fn hash_cache_row_count(order: &BitmapOrder) -> usize {
    order
        .pack_index()
        .map_or(order.object_count() as usize, |idx| idx.len())
}

/// Name hash of the path `parent/name`, or of `name` alone for root tree entries.
fn child_name_hash(parent: Option<u32>, name: &[u8]) -> u32 {
    match parent {
        None => pack_name_hash_extend(0, name),
        Some(hash) => pack_name_hash_extend(pack_name_hash_extend(hash, b"/"), name),
    }
}

fn fill_commit_bitmap(
    fill: &mut TreeBitmapFill<'_>,
    commits: &CommitTable,
    bitmap: &mut Bitmap,
    tip: ObjectId,
    computed: &HashMap<ObjectId, Bitmap>,
) -> Result<(), PackBitmapWriteError> {
    let mut queue: VecDeque<ObjectId> = VecDeque::new();
    queue.push_back(tip);
    while let Some(oid) = queue.pop_front() {
        let Some((pos, _)) = fill.locate(&oid)? else {
            continue;
        };
        let p = pos as usize;
        if bitmap.get(p) {
            continue;
        }
        if let Some(cached) = computed.get(&oid) {
            bitmap.or_assign(cached);
            continue;
        }
        bitmap.set(p);
        // Every commit in the order is in the table; anything else is not a commit.
        let Some(row) = commits.get(&oid) else {
            continue;
        };
        queue.extend(row.parents.iter().copied());
        if let Some((tree_pos, tree_row)) = fill.locate(&row.tree)? {
            fill.fill_tree(bitmap, row.tree, tree_pos, tree_row, None)?;
        }
    }
    Ok(())
}

/// Walks trees into a commit bitmap, recording each object's path name hash.
struct TreeBitmapFill<'a> {
    repo: &'a Repository,
    order: &'a BitmapOrder,
    hash_len: usize,
    name_hashes: &'a mut [u32],
    strict_closure: bool,
}

impl TreeBitmapFill<'_> {
    /// Bitmap position and storage row of `oid`; `None` when the order lacks it and the
    /// closure is not strict.
    fn locate(&self, oid: &ObjectId) -> Result<Option<(u32, u32)>, PackBitmapWriteError> {
        match self.order.locate(oid) {
            Some(slot) => Ok(Some(slot)),
            None if self.strict_closure => Err(PackBitmapWriteError::NotClosed {
                missing: vec![*oid],
            }),
            None => Ok(None),
        }
    }

    fn store_name_hash(&mut self, row: u32, hash: u32) {
        if let Some(slot) = self.name_hashes.get_mut(row as usize) {
            *slot = hash;
        }
    }

    /// Set `tree` (at `pos` / `row`) and everything below it that `bitmap` lacks.
    ///
    /// Blobs are classified by tree entry mode and never read. A subtree whose bit is already
    /// set (from this walk or an ancestor seed) is skipped without reading it. A blob's name
    /// hash is overwritten by every newly walked tree that names it, so the walk order (oldest
    /// selected commit first) leaves the newest path; a tree keeps the path it was first set by.
    fn fill_tree(
        &mut self,
        bitmap: &mut Bitmap,
        tree: ObjectId,
        pos: u32,
        row: u32,
        name_hash: Option<u32>,
    ) -> Result<(), PackBitmapWriteError> {
        let p = pos as usize;
        if bitmap.get(p) {
            return Ok(());
        }
        bitmap.set(p);
        if let Some(hash) = name_hash {
            self.store_name_hash(row, hash);
        }
        let obj = read_object(self.repo, &tree)?;
        for entry in TreeRefIter::with_oid_len(&obj.data, self.hash_len) {
            let entry = entry.map_err(|e| PackBitmapWriteError::Object(e.to_string()))?;
            let kind = TreeChild::from_mode(entry.mode);
            if kind == TreeChild::Gitlink {
                continue;
            }
            let Some((child_pos, child_row)) = self.locate(&entry.oid)? else {
                continue;
            };
            let hash = child_name_hash(name_hash, entry.name);
            if kind == TreeChild::Tree {
                self.fill_tree(bitmap, entry.oid, child_pos, child_row, Some(hash))?;
            } else {
                bitmap.set(child_pos as usize);
                self.store_name_hash(child_row, hash);
            }
        }
        Ok(())
    }
}

struct XorPlan {
    /// For each selected commit (sorted by date ascending), optional index of xor base.
    xor_prev: Vec<Option<usize>>,
    payload: Vec<EwahBitmap>,
}

fn plan_xor_compression(
    commits: &CommitTable,
    selected: &[ObjectId],
    commit_bitmaps: &HashMap<ObjectId, Bitmap>,
) -> (Vec<ObjectId>, XorPlan) {
    let mut selected_sorted: Vec<ObjectId> = selected.to_vec();
    selected_sorted.sort_by_key(|a| commits.date(a));
    let n = selected_sorted.len();
    let mut xor_prev = vec![None; n];
    let mut payload = Vec::with_capacity(n);
    for i in 0..n {
        let base = &commit_bitmaps[&selected_sorted[i]];
        let mut best_bm = base.clone();
        let mut best_off = 0usize;
        let mut best_words = best_bm.word_len();
        for off in 1..=MAX_XOR_SEARCH.min(i) {
            let mut candidate = base.clone();
            candidate.xor_assign(&commit_bitmaps[&selected_sorted[i - off]]);
            let words = candidate.word_len();
            if words < best_words {
                best_bm = candidate;
                best_off = off;
                best_words = words;
            }
        }
        xor_prev[i] = if best_off == 0 {
            None
        } else {
            Some(i - best_off)
        };
        payload.push(EwahBitmap::from_bitmap(&best_bm));
    }
    (selected_sorted, XorPlan { xor_prev, payload })
}

fn assemble_bitmap_bytes(
    order: &BitmapOrder,
    pack_checksum: &[u8],
    options: &PackBitmapWriteOptions,
    types: &TypeBitmaps,
    selected_sorted: &[ObjectId],
    xor_plan: &XorPlan,
    name_hashes: &[u32],
) -> Result<Vec<u8>, PackBitmapWriteError> {
    let hash_len = pack_checksum.len();
    let mut flags = 0u16;
    if options.full_dag {
        flags |= BITMAP_OPT_FULL_DAG;
    }
    if options.hash_cache {
        flags |= BITMAP_OPT_HASH_CACHE;
    }
    if options.lookup_table {
        flags |= BITMAP_OPT_LOOKUP_TABLE;
    }
    if flags & BITMAP_OPT_FULL_DAG == 0 {
        return Err(PackBitmapWriteError::InvalidPackPath(
            "FULL_DAG required".into(),
        ));
    }

    let mut body = Vec::new();
    body.extend_from_slice(BITMAP_MAGIC);
    body.extend_from_slice(&BITMAP_VERSION.to_be_bytes());
    body.extend_from_slice(&flags.to_be_bytes());
    body.extend_from_slice(&(selected_sorted.len() as u32).to_be_bytes());
    body.extend_from_slice(pack_checksum);

    types.commits.serialize(&mut body);
    types.trees.serialize(&mut body);
    types.blobs.serialize(&mut body);
    types.tags.serialize(&mut body);

    let mut entry_offsets: Vec<u64> = Vec::new();
    for (i, commit) in selected_sorted.iter().enumerate() {
        let commit_pos =
            order
                .storage_row_for_oid(commit)
                .ok_or_else(|| PackBitmapWriteError::NotClosed {
                    missing: vec![*commit],
                })?;
        if options.lookup_table {
            entry_offsets.push(body.len() as u64);
        }
        body.extend_from_slice(&commit_pos.to_be_bytes());
        let xor_off = xor_plan
            .xor_prev
            .get(i)
            .and_then(|opt| opt.map(|prev| i - prev))
            .unwrap_or(0);
        if xor_off > u8::MAX as usize || xor_off > MAX_XOR_SEARCH {
            return Err(PackBitmapWriteError::Object(
                "xor offset out of range".into(),
            ));
        }
        body.push(xor_off as u8);
        body.push(0); // flags
        xor_plan
            .payload
            .get(i)
            .ok_or_else(|| PackBitmapWriteError::Object("missing xor payload".into()))?
            .serialize(&mut body);
    }

    if options.lookup_table {
        write_lookup_table(&mut body, order, selected_sorted, &entry_offsets, xor_plan)?;
    }

    if options.hash_cache {
        for &hash in name_hashes {
            body.extend_from_slice(&hash.to_be_bytes());
        }
    }

    append_hashfile_checksum(&mut body, hash_len);
    Ok(body)
}

fn write_lookup_table(
    body: &mut Vec<u8>,
    order: &BitmapOrder,
    selected: &[ObjectId],
    offsets: &[u64],
    xor_plan: &XorPlan,
) -> Result<(), PackBitmapWriteError> {
    let mut table_order: Vec<usize> = (0..selected.len()).collect();
    table_order.sort_by_key(|&i| order.storage_row_for_oid(&selected[i]).unwrap_or(u32::MAX));
    let mut inv = vec![0u32; selected.len()];
    for (rank, &i) in table_order.iter().enumerate() {
        inv[i] = u32::try_from(rank).unwrap_or(u32::MAX);
    }
    for &i in &table_order {
        let commit_pos = order.storage_row_for_oid(&selected[i]).unwrap_or(0);
        body.extend_from_slice(&commit_pos.to_be_bytes());
        body.extend_from_slice(&offsets[i].to_be_bytes());
        let xor_row = xor_plan
            .xor_prev
            .get(i)
            .copied()
            .flatten()
            .map(|prev| inv[prev])
            .unwrap_or(XOR_ROW_NONE);
        body.extend_from_slice(&xor_row.to_be_bytes());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_sample_plan_increases_stride_with_rank() {
        let plan = HistorySamplePlan::default();
        assert_eq!(plan.gap_before_window(10), 0);
        assert!(plan.gap_before_window(200) >= plan.mid_stride_base);
        assert!(plan.gap_before_window(10_000) >= plan.far_stride_base);
    }

    #[test]
    fn tree_child_kind_comes_from_mode_bits() {
        assert_eq!(TreeChild::from_mode(0o040000), TreeChild::Tree);
        assert_eq!(TreeChild::from_mode(0o160000), TreeChild::Gitlink);
        for mode in [0o100644, 0o100755, 0o120000, 0o100664] {
            assert_eq!(TreeChild::from_mode(mode), TreeChild::Blob, "{mode:o}");
        }
    }

    #[test]
    fn child_name_hash_matches_full_path_hash() {
        use crate::pack_name_hash::pack_name_hash;
        assert_eq!(
            child_name_hash(None, b"Cargo.toml"),
            pack_name_hash("Cargo.toml")
        );
        let src = child_name_hash(None, b"src");
        let cargo = child_name_hash(Some(src), b"cargo");
        assert_eq!(cargo, pack_name_hash("src/cargo"));
        assert_eq!(
            child_name_hash(Some(cargo), b"lib.rs"),
            pack_name_hash("src/cargo/lib.rs")
        );
    }
}
