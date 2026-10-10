//! Write Git pack reachability bitmap (`.bitmap`) sidecars for a single pack index.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use tempfile::NamedTempFile;
use thiserror::Error;

use crate::ewah_bitmap::{Bitmap, EwahBitmap};
use crate::gc::collect_referenced_object_roots;
use crate::ident::parse_signature_times;
use crate::objects::{parse_commit, parse_tag, parse_tree, ObjectId, ObjectKind};
use crate::pack::{read_pack_index, PackIndex};
use crate::pack_name_hash::pack_name_hash;
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
}

impl Default for PackBitmapWriteOptions {
    fn default() -> Self {
        Self {
            full_dag: true,
            hash_cache: true,
            lookup_table: false,
            prefer_bitmap_tips: Vec::new(),
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
        if !repo.odb.uses_files_primary() {
            return Err(PackBitmapWriteError::NotFilesBacked);
        }
        let idx = read_pack_index(pack_idx_path)
            .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?;
        let objects_dir = repo
            .odb
            .files_objects_dir()
            .ok_or(PackBitmapWriteError::NotFilesBacked)?;
        ensure_rev_sidecar(&idx)?;
        let order = BitmapOrder::load_pack(objects_dir, Arc::new(idx))
            .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?;
        let pack_checksum = read_pack_trailer(
            order.pack_index().as_ref().ok_or_else(|| {
                PackBitmapWriteError::InvalidPackPath("MIDX write unsupported".into())
            })?,
            repo.odb.hash_algo().len(),
        )?;
        let roots = collect_referenced_object_roots(&repo.git_dir)
            .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?;
        verify_pack_closure(repo, &order, &roots)?;
        let tips = commit_tips_from_roots(repo, &roots)?;
        let needs_bitmap = tip_needs_flags(repo, &tips, &options.prefer_bitmap_tips)?;
        let hash_cache_len = hash_cache_row_count(&order);
        let mut name_hashes = vec![0u32; hash_cache_len];
        let (type_bitmaps, commits) = build_type_bitmaps_and_commits(repo, &order)?;
        let selected = select_commits(&commits, &tips, &needs_bitmap);
        let commit_bitmaps =
            build_commit_bitmaps(repo, &order, &commits, &selected, &mut name_hashes)?;
        let (selected_sorted, xor_plan) = plan_xor_compression(repo, &selected, &commit_bitmaps);
        let bitmap_path = bitmap_path_for_index(pack_idx_path);
        let bytes = assemble_bitmap_bytes(
            &order,
            &pack_checksum,
            options,
            &type_bitmaps,
            &selected_sorted,
            &xor_plan,
            &name_hashes,
        )?;
        write_bitmap_atomically(&bitmap_path, &bytes)?;
        repo.caches().bitmap_index().set(None);
        Ok(bitmap_path)
    }
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
    let data = fs::read(&idx.pack_path).map_err(PackBitmapWriteError::io)?;
    if data.len() < hash_len {
        return Err(PackBitmapWriteError::InvalidPackPath(
            "pack file too short".into(),
        ));
    }
    Ok(data[data.len() - hash_len..].to_vec())
}

fn ensure_rev_sidecar(idx: &PackIndex) -> Result<(), PackBitmapWriteError> {
    let rev_path = rev_path_for_index(&idx.idx_path);
    if rev_path.is_file() && verify_pack_rev_file(&rev_path, idx).is_ok() {
        return Ok(());
    }
    let hash_len = idx.hash_bytes();
    let pack_checksum = read_pack_trailer(idx, hash_len)?;
    let offsets: Vec<u64> = (0..idx.len()).map(|i| idx.offset_at(i)).collect();
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
    parents: Vec<ObjectId>,
}

fn build_type_bitmaps_and_commits(
    repo: &Repository,
    order: &BitmapOrder,
) -> Result<(TypeBitmaps, Vec<CommitRow>), PackBitmapWriteError> {
    let mut commits = Vec::new();
    let mut commit_bm = Bitmap::new();
    let mut trees = Bitmap::new();
    let mut blobs = Bitmap::new();
    let mut tags = Bitmap::new();
    let n = order.object_count();
    for pos in 0..n {
        let oid = order
            .oid_at(pos)
            .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?;
        let obj = repo
            .odb
            .read(&oid)
            .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?;
        let p = pos as usize;
        match obj.kind {
            ObjectKind::Commit => {
                commit_bm.set(p);
                let c = parse_commit(&obj.data)
                    .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?;
                let committer_date = parse_signature_times(&c.committer)
                    .map(|t| t.unix_seconds)
                    .unwrap_or(0);
                commits.push(CommitRow {
                    oid,
                    committer_date,
                    parents: c.parents,
                });
            }
            ObjectKind::Tree => trees.set(p),
            ObjectKind::Blob => blobs.set(p),
            ObjectKind::Tag => tags.set(p),
        }
    }
    commits.sort_by_key(|c| std::cmp::Reverse(c.committer_date));
    Ok((
        TypeBitmaps {
            commits: EwahBitmap::from_bitmap(&commit_bm),
            trees: EwahBitmap::from_bitmap(&trees),
            blobs: EwahBitmap::from_bitmap(&blobs),
            tags: EwahBitmap::from_bitmap(&tags),
        },
        commits,
    ))
}

fn commit_tips_from_roots(
    repo: &Repository,
    roots: &[ObjectId],
) -> Result<Vec<ObjectId>, PackBitmapWriteError> {
    let mut tips = Vec::new();
    for &oid in roots {
        let Some(commit) = peel_to_commit(repo, oid)? else {
            continue;
        };
        tips.push(commit);
    }
    tips.sort_by_key(|o| o.to_hex());
    tips.dedup();
    Ok(tips)
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
    let git_dir = &repo.git_dir;
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
    let obj = repo
        .odb
        .read(&oid)
        .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?;
    match obj.kind {
        ObjectKind::Commit => Ok(Some(oid)),
        ObjectKind::Tag => {
            let tag =
                parse_tag(&obj.data).map_err(|e| PackBitmapWriteError::Object(e.to_string()))?;
            Ok(
                if repo
                    .odb
                    .read(&tag.object)
                    .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?
                    .kind
                    == ObjectKind::Commit
                {
                    Some(tag.object)
                } else {
                    None
                },
            )
        }
        _ => Ok(None),
    }
}

fn verify_pack_closure(
    repo: &Repository,
    order: &BitmapOrder,
    tips: &[ObjectId],
) -> Result<(), PackBitmapWriteError> {
    let reachable = reachable_from_tips(repo, tips)?;
    let mut missing = Vec::new();
    for oid in &reachable {
        if order.position_of(oid).is_none() {
            missing.push(*oid);
            if missing.len() >= 32 {
                break;
            }
        }
    }
    if !missing.is_empty() {
        return Err(PackBitmapWriteError::NotClosed { missing });
    }
    Ok(())
}

fn reachable_from_tips(
    repo: &Repository,
    tips: &[ObjectId],
) -> Result<HashSet<ObjectId>, PackBitmapWriteError> {
    let mut seen = HashSet::new();
    let mut queue = VecDeque::new();
    for &tip in tips {
        if seen.insert(tip) {
            queue.push_back(tip);
        }
    }
    while let Some(oid) = queue.pop_front() {
        let obj = repo
            .odb
            .read(&oid)
            .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?;
        match obj.kind {
            ObjectKind::Commit => {
                let c = parse_commit(&obj.data)
                    .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?;
                for p in c.parents {
                    if seen.insert(p) {
                        queue.push_back(p);
                    }
                }
                if seen.insert(c.tree) {
                    queue.push_back(c.tree);
                }
            }
            ObjectKind::Tree => {
                for entry in parse_tree(&obj.data)
                    .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?
                {
                    if entry.mode == 0o160000 {
                        continue;
                    }
                    if seen.insert(entry.oid) {
                        queue.push_back(entry.oid);
                    }
                }
            }
            ObjectKind::Tag => {
                let tag = parse_tag(&obj.data)
                    .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?;
                if seen.insert(tag.object) {
                    queue.push_back(tag.object);
                }
            }
            ObjectKind::Blob => {}
        }
    }
    Ok(seen)
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
    _commits: &[CommitRow],
    selected: &[ObjectId],
    name_hashes: &mut [u32],
) -> Result<HashMap<ObjectId, Bitmap>, PackBitmapWriteError> {
    let mut topo = selected.to_vec();
    topo.sort_by_key(|a| commit_date(repo, *a));
    let mut computed: HashMap<ObjectId, Bitmap> = HashMap::new();
    for commit in topo {
        let mut bm = seed_from_computed_ancestors(repo, commit, &computed)?;
        fill_commit_bitmap(repo, order, &mut bm, commit, name_hashes, &computed)?;
        computed.insert(commit, bm);
    }
    Ok(computed)
}

fn seed_from_computed_ancestors(
    repo: &Repository,
    commit: ObjectId,
    computed: &HashMap<ObjectId, Bitmap>,
) -> Result<Bitmap, PackBitmapWriteError> {
    let obj = repo
        .odb
        .read(&commit)
        .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?;
    let c = parse_commit(&obj.data).map_err(|e| PackBitmapWriteError::Object(e.to_string()))?;
    let mut bm = Bitmap::new();
    for parent in c.parents {
        if let Some(parent_bm) = computed.get(&parent) {
            bm.or_assign(parent_bm);
            continue;
        }
        let mut walk = parent;
        loop {
            if let Some(ancestor_bm) = computed.get(&walk) {
                bm.or_assign(ancestor_bm);
                break;
            }
            let parent_obj = repo
                .odb
                .read(&walk)
                .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?;
            let parent_commit = parse_commit(&parent_obj.data)
                .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?;
            match parent_commit.parents.first().copied() {
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

fn store_name_hash(order: &BitmapOrder, name_hashes: &mut [u32], oid: &ObjectId, path: &str) {
    if path.is_empty() {
        return;
    }
    let Some(row) = order.storage_row_for_oid(oid) else {
        return;
    };
    let r = row as usize;
    if r < name_hashes.len() {
        name_hashes[r] = pack_name_hash(path);
    }
}

fn commit_date(repo: &Repository, oid: ObjectId) -> i64 {
    repo.odb
        .read(&oid)
        .ok()
        .filter(|o| o.kind == ObjectKind::Commit)
        .and_then(|o| parse_commit(&o.data).ok())
        .and_then(|c| parse_signature_times(&c.committer).map(|t| t.unix_seconds))
        .unwrap_or(0)
}

fn fill_commit_bitmap(
    repo: &Repository,
    order: &BitmapOrder,
    bitmap: &mut Bitmap,
    tip: ObjectId,
    name_hashes: &mut [u32],
    computed: &HashMap<ObjectId, Bitmap>,
) -> Result<(), PackBitmapWriteError> {
    let mut queue: VecDeque<ObjectId> = VecDeque::new();
    queue.push_back(tip);
    while let Some(oid) = queue.pop_front() {
        let Some(pos) = order.position_of(&oid) else {
            return Err(PackBitmapWriteError::NotClosed { missing: vec![oid] });
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
        let obj = repo
            .odb
            .read(&oid)
            .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?;
        match obj.kind {
            ObjectKind::Commit => {
                let c = parse_commit(&obj.data)
                    .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?;
                for parent in c.parents {
                    queue.push_back(parent);
                }
                fill_tree_bitmap(repo, order, bitmap, c.tree, String::new(), name_hashes)?;
            }
            ObjectKind::Tree | ObjectKind::Blob | ObjectKind::Tag => {}
        }
    }
    Ok(())
}

fn fill_tree_bitmap(
    repo: &Repository,
    order: &BitmapOrder,
    bitmap: &mut Bitmap,
    tree_oid: ObjectId,
    prefix: String,
    name_hashes: &mut [u32],
) -> Result<(), PackBitmapWriteError> {
    let Some(pos) = order.position_of(&tree_oid) else {
        return Err(PackBitmapWriteError::NotClosed {
            missing: vec![tree_oid],
        });
    };
    let p = pos as usize;
    if bitmap.get(p) {
        return Ok(());
    }
    bitmap.set(p);
    store_name_hash(order, name_hashes, &tree_oid, &prefix);
    let obj = repo
        .odb
        .read(&tree_oid)
        .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?;
    for entry in parse_tree(&obj.data).map_err(|e| PackBitmapWriteError::Object(e.to_string()))? {
        if entry.mode == 0o160000 {
            continue;
        }
        let name = String::from_utf8_lossy(&entry.name).into_owned();
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let Some(child_pos) = order.position_of(&entry.oid) else {
            return Err(PackBitmapWriteError::NotClosed {
                missing: vec![entry.oid],
            });
        };
        let cp = child_pos as usize;
        let child = repo
            .odb
            .read(&entry.oid)
            .map_err(|e| PackBitmapWriteError::Object(e.to_string()))?;
        match child.kind {
            ObjectKind::Tree => {
                fill_tree_bitmap(repo, order, bitmap, entry.oid, path, name_hashes)?;
            }
            ObjectKind::Blob => {
                if !bitmap.get(cp) {
                    bitmap.set(cp);
                }
                store_name_hash(order, name_hashes, &entry.oid, &path);
            }
            ObjectKind::Tag | ObjectKind::Commit => {
                if !bitmap.get(cp) {
                    bitmap.set(cp);
                }
            }
        }
    }
    Ok(())
}

struct XorPlan {
    /// For each selected commit (sorted by date ascending), optional index of xor base.
    xor_prev: Vec<Option<usize>>,
    payload: Vec<EwahBitmap>,
}

fn plan_xor_compression(
    repo: &Repository,
    selected: &[ObjectId],
    commit_bitmaps: &HashMap<ObjectId, Bitmap>,
) -> (Vec<ObjectId>, XorPlan) {
    let mut selected_sorted: Vec<ObjectId> = selected.to_vec();
    selected_sorted.sort_by_key(|a| commit_date(repo, *a));
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
}
