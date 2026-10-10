//! Parsed pack or MIDX reachability bitmap index.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::diagnostics::Warning;
use crate::ewah_bitmap::{Bitmap, EwahView};
use crate::midx::{midx_checksum_hex, resolve_tip_midx_path};
use crate::objects::{ObjectId, ObjectKind};
use crate::pack::PackIndex;
use crate::pack_map::PackData;
use crate::pack_rev::hashfile_checksum_valid;
use crate::repo::Repository;

use super::error::BitmapError;
use super::order::BitmapOrder;
use crate::pack::read_local_pack_indexes;

const BITMAP_MAGIC: &[u8; 4] = b"BITM";
const BITMAP_VERSION: u16 = 1;
const BITMAP_OPT_FULL_DAG: u16 = 0x1;
const BITMAP_OPT_HASH_CACHE: u16 = 0x4;
const BITMAP_OPT_LOOKUP_TABLE: u16 = 0x10;
const BITMAP_OPT_PSEUDO_MERGES: u16 = 0x20;
const LOOKUP_TRIPLET_WIDTH: usize = 16;
const MAX_XOR_OFFSET: usize = 160;
const XOR_ROW_NONE: u32 = 0xffff_ffff;

/// Decoded reachability set for one commit (bits index pack/MIDX object positions).
#[derive(Debug, Clone)]
pub struct CommitReachabilityBitmap {
    bits: Arc<Bitmap>,
}

impl CommitReachabilityBitmap {
    /// Whether `position` is reachable from the indexed commit.
    #[must_use]
    pub fn contains(&self, position: u32) -> bool {
        self.bits.get(position as usize)
    }

    /// Iterate set bit positions in ascending order.
    pub fn positions(&self) -> impl Iterator<Item = u32> + '_ {
        self.bits
            .set_bits()
            .map(|p| u32::try_from(p).unwrap_or(u32::MAX))
    }
}

/// Borrowed type filter bitmap (commits, trees, blobs, or tags).
#[derive(Debug, Clone, Copy)]
pub struct TypeBitmap<'a> {
    view: EwahView<'a>,
}

impl TypeBitmap<'_> {
    /// Test whether `position` is marked in this type index.
    ///
    /// # Errors
    ///
    /// Returns [`BitmapError::InvalidEwah`] when the embedded EWAH cannot be read.
    pub fn contains(&self, position: u32) -> Result<bool, BitmapError> {
        let mut scratch = Bitmap::new();
        self.view
            .expand_into(&mut scratch)
            .map_err(|_| BitmapError::InvalidEwah)?;
        Ok(scratch.get(position as usize))
    }
}

/// Memory-mapped pack or MIDX reachability bitmap (Git `BITM` v1).
pub struct BitmapIndex {
    map: Arc<PackData>,
    entry_count: u32,
    has_lookup_table: bool,
    order: BitmapOrder,
    type_commits_off: usize,
    type_trees_off: usize,
    type_blobs_off: usize,
    type_tags_off: usize,
    name_hashes: Option<(usize, usize)>,
    lookup_table: Option<(usize, usize)>,
    eager_entries: Option<Vec<EagerEntry>>,
    decode_cache: Mutex<HashMap<ObjectId, Arc<Bitmap>>>,
}

#[derive(Debug, Clone)]
struct EagerEntry {
    oid: ObjectId,
    ewah_offset: usize,
    xor_prev_index: Option<usize>,
}

#[derive(Debug, Clone, Copy)]
struct LookupTriplet {
    commit_pos: u32,
    offset: u64,
    xor_row: u32,
}

impl BitmapIndex {
    /// Open the active reachability bitmap for `repo`, preferring MIDX over pack bitmaps.
    ///
    /// Returns `Ok(None)` when no usable bitmap exists, the primary store is not
    /// files-backed, or the on-disk file failed validation (a warning is emitted).
    ///
    /// # Errors
    ///
    /// Propagates unexpected I/O failures while discovering bitmap paths.
    pub fn open(repo: &Repository) -> Result<Option<Arc<Self>>, BitmapError> {
        if !repo.odb.uses_files_primary() {
            return Ok(None);
        }
        if let Some(cached) = repo.caches().bitmap_index().get() {
            return Ok(Some(cached));
        }
        let objects_dir = repo.odb.files_objects_dir().ok_or(BitmapError::Corrupt)?;
        let loaded = try_open_bitmap(objects_dir, repo)?;
        repo.caches().bitmap_index().set(loaded.clone());
        Ok(loaded)
    }

    /// Reachability bitmap for `commit`, when that commit is indexed.
    #[must_use]
    pub fn commit_bitmap(&self, commit: &ObjectId) -> Option<CommitReachabilityBitmap> {
        let bits = self.decode_commit_bitmap(commit).ok()??;
        Some(CommitReachabilityBitmap { bits })
    }

    /// Type filter bitmap for `kind` (commits, trees, blobs, or tags).
    #[must_use]
    pub fn type_bitmap(&self, kind: ObjectKind) -> TypeBitmap<'_> {
        let off = match kind {
            ObjectKind::Commit => self.type_commits_off,
            ObjectKind::Tree => self.type_trees_off,
            ObjectKind::Blob => self.type_blobs_off,
            ObjectKind::Tag => self.type_tags_off,
        };
        TypeBitmap {
            view: self.type_ewah_view(off),
        }
    }

    /// Name-hash cache entry for bitmap position `position`, when present.
    #[must_use]
    pub fn name_hash(&self, position: u32) -> Option<u32> {
        let (start, end) = self.name_hashes?;
        let row = self.order.storage_row_for_bitmap_rank(position)?;
        let base = start + usize::try_from(row).ok()?.checked_mul(4)?;
        if base + 4 > end {
            return None;
        }
        let slice = self.map.get(base..base + 4)?;
        Some(u32::from_be_bytes(slice.try_into().ok()?))
    }

    /// Number of objects in the pack/MIDX namespace (length of type bitmaps).
    #[must_use]
    pub fn object_count(&self) -> u32 {
        self.order.object_count()
    }

    /// Whether the on-disk index included a commit lookup table extension.
    #[must_use]
    pub fn has_lookup_table(&self) -> bool {
        self.has_lookup_table
    }

    /// Number of reachability bitmap entries (selected commits) in this file.
    #[must_use]
    pub fn selected_commit_count(&self) -> u32 {
        self.entry_count
    }

    /// Object id at bitmap bit `position`.
    ///
    /// # Errors
    ///
    /// Returns [`BitmapError::Corrupt`] when `position` is out of range.
    pub fn oid_at(&self, position: u32) -> Result<ObjectId, BitmapError> {
        self.order
            .oid_at(position)
            .map_err(|_| BitmapError::Corrupt)
    }

    /// Bitmap bit position for `oid`, when the object is in this index.
    #[must_use]
    pub fn position_of(&self, oid: &ObjectId) -> Option<u32> {
        self.order.position_of(oid)
    }

    fn ewah_at(&self, offset: usize) -> Result<EwahView<'_>, BitmapError> {
        let (view, _) = EwahView::parse(self.map.get(offset..).ok_or(BitmapError::Corrupt)?)
            .map_err(|_| BitmapError::InvalidEwah)?;
        Ok(view)
    }

    /// Type EWAH slices are validated when the index is opened.
    #[allow(clippy::expect_used)]
    fn type_ewah_view(&self, offset: usize) -> EwahView<'_> {
        self.ewah_at(offset)
            .expect("type bitmap EWAH validated at open")
    }

    fn decode_commit_bitmap(&self, commit: &ObjectId) -> Result<Option<Arc<Bitmap>>, BitmapError> {
        if let Ok(guard) = self.decode_cache.lock() {
            if let Some(bits) = guard.get(commit) {
                return Ok(Some(Arc::clone(bits)));
            }
        }
        let Some(bits) = self.decode_commit_bitmap_uncached(commit)? else {
            return Ok(None);
        };
        let arc = Arc::new(bits);
        if let Ok(mut guard) = self.decode_cache.lock() {
            guard.insert(*commit, Arc::clone(&arc));
        }
        Ok(Some(arc))
    }

    fn decode_commit_bitmap_uncached(
        &self,
        commit: &ObjectId,
    ) -> Result<Option<Bitmap>, BitmapError> {
        if self.has_lookup_table {
            return self.decode_via_lookup_table(commit);
        }
        let Some(entries) = self.eager_entries.as_ref() else {
            return Ok(None);
        };
        if let Some(idx) = entries.iter().position(|e| e.oid == *commit) {
            return self.decode_eager_index(idx);
        }
        Ok(None)
    }

    fn decode_eager_index(&self, idx: usize) -> Result<Option<Bitmap>, BitmapError> {
        let entries = self.eager_entries.as_ref().ok_or(BitmapError::Corrupt)?;
        let entry = entries.get(idx).ok_or(BitmapError::Corrupt)?;
        let xor = if let Some(prev) = entry.xor_prev_index {
            Some(self.decode_eager_index(prev)?.ok_or(BitmapError::Corrupt)?)
        } else {
            None
        };
        self.decode_ewah_payload_at(entry.ewah_offset, xor)
            .map(Some)
    }

    fn decode_via_lookup_table(&self, commit: &ObjectId) -> Result<Option<Bitmap>, BitmapError> {
        let commit_pos = match self.order.storage_row_for_oid(commit) {
            Some(p) => p,
            None => return Ok(None),
        };
        let (start, end) = self.lookup_table.ok_or(BitmapError::Corrupt)?;
        let table = self.map.get(start..end).ok_or(BitmapError::Corrupt)?;
        let Some(triplet) = lookup_triplet_by_commit_pos(table, self.entry_count, commit_pos)
        else {
            return Ok(None);
        };
        let mut xor_base: Option<Bitmap> = None;
        if triplet.xor_row != XOR_ROW_NONE {
            xor_base = Some(self.decode_xor_chain(triplet.xor_row)?);
        }
        self.decode_entry_at(triplet.offset as usize, xor_base)
            .map(Some)
    }

    fn decode_xor_chain(&self, mut xor_row: u32) -> Result<Bitmap, BitmapError> {
        let (start, end) = self.lookup_table.ok_or(BitmapError::Corrupt)?;
        let table = self.map.get(start..end).ok_or(BitmapError::Corrupt)?;
        let mut stack: Vec<LookupTriplet> = Vec::new();
        let mut steps = 0u32;
        while xor_row != XOR_ROW_NONE {
            steps += 1;
            if steps > self.entry_count {
                return Err(BitmapError::Corrupt);
            }
            let triplet = lookup_triplet_by_row(table, xor_row).ok_or(BitmapError::Corrupt)?;
            if let Ok(oid) = self.order.oid_at_storage_row(triplet.commit_pos) {
                if let Ok(guard) = self.decode_cache.lock() {
                    if let Some(existing) = guard.get(&oid) {
                        return Ok((**existing).clone());
                    }
                }
            }
            stack.push(triplet);
            xor_row = triplet.xor_row;
        }
        let mut acc: Option<Bitmap> = None;
        while let Some(t) = stack.pop() {
            acc = Some(self.decode_entry_at(t.offset as usize, acc)?);
            if let Ok(oid) = self.order.oid_at_storage_row(t.commit_pos) {
                if let Some(ref composed) = acc {
                    if let Ok(mut guard) = self.decode_cache.lock() {
                        guard.insert(oid, Arc::new(composed.clone()));
                    }
                }
            }
        }
        acc.ok_or(BitmapError::Corrupt)
    }

    /// Decode from the start of a commit entry (lookup-table offsets).
    fn decode_entry_at(
        &self,
        entry_start: usize,
        xor_with: Option<Bitmap>,
    ) -> Result<Bitmap, BitmapError> {
        let data = self.map.as_ref();
        if entry_start + 6 > data.len() {
            return Err(BitmapError::Corrupt);
        }
        let ewah_start = entry_start + 6;
        self.decode_ewah_payload_at(ewah_start, xor_with)
    }

    /// Decode an EWAH payload at `ewah_start` (after the 6-byte entry header).
    fn decode_ewah_payload_at(
        &self,
        ewah_start: usize,
        xor_with: Option<Bitmap>,
    ) -> Result<Bitmap, BitmapError> {
        let (view, _) = EwahView::parse(self.map.get(ewah_start..).ok_or(BitmapError::Corrupt)?)
            .map_err(|_| BitmapError::InvalidEwah)?;
        let mut out = xor_with.unwrap_or_default();
        let mut xor_layer = Bitmap::new();
        view.expand_into(&mut xor_layer)
            .map_err(|_| BitmapError::InvalidEwah)?;
        out.xor_assign(&xor_layer);
        Ok(out)
    }
}

fn try_open_bitmap(
    objects_dir: &Path,
    repo: &Repository,
) -> Result<Option<Arc<BitmapIndex>>, BitmapError> {
    let pack_dir = objects_dir.join("pack");
    if resolve_tip_midx_path(&pack_dir).is_some() {
        if let Ok(hex) = midx_checksum_hex(objects_dir) {
            let bitmap_path = pack_dir.join(format!("multi-pack-index-{hex}.bitmap"));
            if bitmap_path.is_file() {
                match open_bitmap_file(&bitmap_path, objects_dir, BitmapSource::Midx, repo) {
                    Ok(idx) => return Ok(Some(Arc::new(idx))),
                    Err(err) => warn_bitmap(repo, &bitmap_path, &err),
                }
            }
        }
    }
    let mut pack_indexes =
        read_local_pack_indexes(objects_dir).map_err(|e| BitmapError::Io(e.to_string()))?;
    pack_indexes.sort_by_key(|idx| std::cmp::Reverse(idx.len()));
    for idx in pack_indexes {
        let stem = idx
            .idx_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("pack");
        let bitmap_path = pack_dir.join(format!("{stem}.bitmap"));
        if !bitmap_path.is_file() {
            continue;
        }
        let index_arc = Arc::new(idx);
        match open_bitmap_file(
            &bitmap_path,
            objects_dir,
            BitmapSource::Pack {
                index: Arc::clone(&index_arc),
            },
            repo,
        ) {
            Ok(index) => return Ok(Some(Arc::new(index))),
            Err(err) => warn_bitmap(repo, &bitmap_path, &err),
        }
    }
    Ok(None)
}

enum BitmapSource {
    Pack { index: Arc<PackIndex> },
    Midx,
}

fn warn_bitmap(repo: &Repository, path: &Path, err: &BitmapError) {
    repo.caches()
        .diagnostics_handle()
        .warn(Warning::PackBitmapIgnored {
            path: path.to_path_buf(),
            reason: err.to_string(),
        });
}

fn open_bitmap_file(
    path: &Path,
    objects_dir: &Path,
    source: BitmapSource,
    repo: &Repository,
) -> Result<BitmapIndex, BitmapError> {
    let map = PackData::open(path).map_err(|e| BitmapError::Io(e.to_string()))?;
    let data = map.as_ref();
    if data.is_empty() {
        return Err(BitmapError::TooSmall);
    }
    let hash_len = repo.odb.hash_algo().len();
    if !hashfile_checksum_valid(data, hash_len) {
        return Err(BitmapError::FileChecksumMismatch);
    }
    let header_size = 12 + hash_len;
    if data.len() < header_size + hash_len {
        return Err(BitmapError::TooSmall);
    }
    if &data[0..4] != BITMAP_MAGIC {
        return Err(BitmapError::InvalidHeader);
    }
    let version = u16::from_be_bytes([data[4], data[5]]);
    if version != BITMAP_VERSION {
        return Err(BitmapError::InvalidHeader);
    }
    let flags = u16::from_be_bytes([data[6], data[7]]);
    if flags & BITMAP_OPT_FULL_DAG == 0 {
        return Err(BitmapError::MissingFullDag);
    }
    let entry_count = u32::from_be_bytes(data[8..12].try_into().map_err(|_| BitmapError::Corrupt)?);
    let header_checksum = &data[12..12 + hash_len];
    verify_source_checksum(&source, objects_dir, header_checksum, hash_len)?;

    let mut pos = header_size;
    let type_commits_off = pos;
    pos += ewah_consumed(data, pos)?;
    let type_trees_off = pos;
    pos += ewah_consumed(data, pos)?;
    let type_blobs_off = pos;
    pos += ewah_consumed(data, pos)?;
    let type_tags_off = pos;
    pos += ewah_consumed(data, pos)?;

    let order = match &source {
        BitmapSource::Pack { index } => BitmapOrder::load_pack(objects_dir, Arc::clone(index))
            .map_err(|_| BitmapError::Corrupt)?,
        BitmapSource::Midx => BitmapOrder::load_midx(objects_dir)
            .map_err(|_| BitmapError::Corrupt)?
            .ok_or(BitmapError::Corrupt)?,
    };

    let num_objects = order.object_count() as usize;
    let hash_cache_len = num_objects.checked_mul(4).unwrap_or(0);
    let mut tail_end = data.len() - hash_len;
    let mut name_hashes = None;
    let mut lookup_table = None;

    if flags & BITMAP_OPT_HASH_CACHE != 0 {
        if hash_cache_len > tail_end.saturating_sub(header_size) {
            return Err(BitmapError::Corrupt);
        }
        tail_end -= hash_cache_len;
        name_hashes = Some((tail_end, tail_end + hash_cache_len));
    }
    if flags & BITMAP_OPT_LOOKUP_TABLE != 0 {
        let table_len = entry_count as usize * LOOKUP_TRIPLET_WIDTH;
        if table_len > tail_end.saturating_sub(header_size) {
            return Err(BitmapError::Corrupt);
        }
        tail_end -= table_len;
        lookup_table = Some((tail_end, tail_end + table_len));
    }
    if flags & BITMAP_OPT_PSEUDO_MERGES != 0 {
        tail_end = skip_pseudo_merge_extension(data, header_size, tail_end)?;
    }

    let entries_start = pos;
    let eager_entries = if lookup_table.is_some() {
        None
    } else {
        Some(load_eager_entries(
            data,
            entries_start,
            entry_count,
            &order,
            tail_end,
        )?)
    };

    Ok(BitmapIndex {
        map,
        entry_count,
        has_lookup_table: lookup_table.is_some(),
        order,
        type_commits_off,
        type_trees_off,
        type_blobs_off,
        type_tags_off,
        name_hashes,
        lookup_table,
        eager_entries,
        decode_cache: Mutex::new(HashMap::new()),
    })
}

fn verify_source_checksum(
    source: &BitmapSource,
    objects_dir: &Path,
    header_checksum: &[u8],
    hash_len: usize,
) -> Result<(), BitmapError> {
    match source {
        BitmapSource::Pack { index } => {
            let pack_bytes = std::fs::read(&index.pack_path).map_err(BitmapError::io)?;
            if pack_bytes.len() < hash_len {
                return Err(BitmapError::PackChecksumMismatch);
            }
            let pack_hash = &pack_bytes[pack_bytes.len() - hash_len..];
            if pack_hash != header_checksum {
                return Err(BitmapError::PackChecksumMismatch);
            }
        }
        BitmapSource::Midx => {
            let pack_dir = objects_dir.join("pack");
            let midx_path = resolve_tip_midx_path(&pack_dir).ok_or(BitmapError::Corrupt)?;
            let midx_bytes = std::fs::read(&midx_path).map_err(BitmapError::io)?;
            if midx_bytes.len() < hash_len {
                return Err(BitmapError::PackChecksumMismatch);
            }
            let midx_hash = &midx_bytes[midx_bytes.len() - hash_len..];
            if midx_hash != header_checksum {
                return Err(BitmapError::PackChecksumMismatch);
            }
        }
    }
    Ok(())
}

fn skip_pseudo_merge_extension(
    data: &[u8],
    header_size: usize,
    mut tail_end: usize,
) -> Result<usize, BitmapError> {
    if tail_end < header_size + 8 {
        return Err(BitmapError::Corrupt);
    }
    let ext_size = u64::from_be_bytes(
        data[tail_end - 8..tail_end]
            .try_into()
            .map_err(|_| BitmapError::Corrupt)?,
    );
    let ext_size = usize::try_from(ext_size).map_err(|_| BitmapError::Corrupt)?;
    if ext_size > tail_end.saturating_sub(header_size) {
        return Err(BitmapError::Corrupt);
    }
    tail_end -= ext_size;
    Ok(tail_end)
}

fn ewah_consumed(data: &[u8], pos: usize) -> Result<usize, BitmapError> {
    let (_, consumed) = EwahView::parse(data.get(pos..).ok_or(BitmapError::Corrupt)?)
        .map_err(|_| BitmapError::InvalidEwah)?;
    Ok(consumed)
}

fn load_eager_entries(
    data: &[u8],
    mut pos: usize,
    entry_count: u32,
    order: &BitmapOrder,
    body_end: usize,
) -> Result<Vec<EagerEntry>, BitmapError> {
    let mut entries = Vec::with_capacity(entry_count as usize);
    for i in 0..entry_count {
        if pos + 6 > body_end {
            return Err(BitmapError::Corrupt);
        }
        let commit_pos = u32::from_be_bytes(
            data[pos..pos + 4]
                .try_into()
                .map_err(|_| BitmapError::Corrupt)?,
        );
        pos += 4;
        let xor_offset = data[pos];
        pos += 1;
        let _flags = data[pos];
        pos += 1;
        let ewah_offset = pos;
        pos += ewah_consumed(data, pos)?;
        let oid = order
            .oid_at_storage_row(commit_pos)
            .map_err(|_| BitmapError::Corrupt)?;
        let xor_prev_index = if xor_offset == 0 {
            None
        } else {
            let x = xor_offset as usize;
            if x > i as usize || x > MAX_XOR_OFFSET {
                return Err(BitmapError::Corrupt);
            }
            Some(i as usize - x)
        };
        entries.push(EagerEntry {
            oid,
            ewah_offset,
            xor_prev_index,
        });
    }
    Ok(entries)
}

fn lookup_triplet_by_commit_pos(
    table: &[u8],
    entry_count: u32,
    commit_pos: u32,
) -> Option<LookupTriplet> {
    let mut lo = 0usize;
    let mut hi = entry_count as usize;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let triplet = lookup_triplet_by_row(table, mid as u32)?;
        match triplet.commit_pos.cmp(&commit_pos) {
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
            std::cmp::Ordering::Equal => return Some(triplet),
        }
    }
    None
}

fn lookup_triplet_by_row(table: &[u8], row: u32) -> Option<LookupTriplet> {
    let base = row as usize * LOOKUP_TRIPLET_WIDTH;
    let slice = table.get(base..base + LOOKUP_TRIPLET_WIDTH)?;
    Some(LookupTriplet {
        commit_pos: u32::from_be_bytes(slice[0..4].try_into().ok()?),
        offset: u64::from_be_bytes(slice[4..12].try_into().ok()?),
        xor_row: u32::from_be_bytes(slice[12..16].try_into().ok()?),
    })
}

/// Repository-scoped cache slot for [`BitmapIndex::open`].
#[derive(Default)]
pub struct BitmapIndexCache {
    inner: std::sync::OnceLock<Option<Arc<BitmapIndex>>>,
}

impl BitmapIndexCache {
    pub(crate) fn get(&self) -> Option<Arc<BitmapIndex>> {
        self.inner.get().and_then(|opt| opt.clone())
    }

    pub(crate) fn set(&self, value: Option<Arc<BitmapIndex>>) {
        let _ = self.inner.set(value);
    }
}
