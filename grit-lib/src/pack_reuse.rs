//! Verbatim pack reuse for bitmap-accelerated upload-pack (streaming PACK v2).
//!
//! ```rust
//! use grit_lib::pack_objects::PackStats;
//! let _ = std::any::type_name::<PackStats>();
//! ```

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use crate::bitmap_walk::ReachableSet;
use crate::error::{Error, Result};
use crate::ewah_bitmap::Bitmap;
use crate::midx::{read_midx_btmp_ranges, read_midx_pack_idx_names, read_midx_preferred_idx_name};
use crate::objects::{oid_hash_map_from, ObjectId};
use crate::pack::{
    find_position_by_pack_offset, pack_entry_raw_end, pack_entry_zlib_start,
    parse_pack_object_header, read_local_pack_indexes_cached, read_pack_bytes_cached,
    read_packed_delta_dependency, PackedDeltaDependency, PackedType,
};
use crate::pack_index::PackIndex;
use crate::pack_map::PackData;
use crate::pack_object_select::try_bitmap_reachable_for_upload;
use crate::pack_objects::{
    encode_ofs_delta_distance, encode_pack_object_header, write_delta_pack_stream,
    PackBuildOptions, PackProgress, PackStats,
};
use crate::repo::Repository;
use crate::rev_list::ObjectFilter;

struct BitmappedPack {
    idx: Arc<PackIndex>,
    pack_bytes: Arc<PackData>,
    bitmap_pos: u32,
    bitmap_nr: u32,
}

/// Counters updated while streaming reused pack bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReuseWriteStats {
    /// Pack body bytes copied verbatim from an existing on-disk pack.
    pub verbatim_reuse_bytes: u64,
    /// Objects emitted via verbatim pack reuse.
    pub verbatim_reuse_objects: u32,
    /// Reused OFS deltas with remapped base offsets.
    pub pack_ofs_fixup_objects: u32,
    /// OFS deltas converted to REF deltas for thin packs.
    pub pack_ref_from_ofs_objects: u32,
}

impl ReuseWriteStats {
    fn note_ofs_fixup(&mut self) {
        self.pack_ofs_fixup_objects += 1;
        self.verbatim_reuse_objects += 1;
    }

    fn note_ref_from_ofs(&mut self) {
        self.pack_ref_from_ofs_objects += 1;
        self.verbatim_reuse_objects += 1;
    }

    fn note_verbatim_slice(&mut self, bytes: u64) {
        self.verbatim_reuse_bytes += bytes;
        self.verbatim_reuse_objects += 1;
    }
}

/// Active verbatim reuse context while streaming a pack body.
pub struct StreamingReuse {
    pack: BitmappedPack,
    reuse_bits: Bitmap,
    index: Option<Arc<crate::pack_bitmap::BitmapIndex>>,
    source_to_output: HashMap<u64, u64>,
    /// Source-pack offset for each object in the repack send list (repack rewrite only).
    source_entry_offset: HashMap<ObjectId, u64>,
    /// Reverse map for the source pack index (offset → OID).
    offset_to_oid: HashMap<u64, ObjectId>,
    stats: ReuseWriteStats,
    /// When set, any object present in [`Self::pack`] may be copied verbatim (repack rewrite).
    reuse_all_in_pack: bool,
}

impl StreamingReuse {
    #[must_use]
    pub(crate) fn take_stats(&mut self) -> ReuseWriteStats {
        std::mem::take(&mut self.stats)
    }

    fn note_verbatim(&mut self, bytes: u64) {
        self.stats.verbatim_reuse_bytes += bytes;
        self.stats.verbatim_reuse_objects += 1;
    }

    fn note_ofs_fixup(&mut self) {
        self.stats.pack_ofs_fixup_objects += 1;
        self.stats.verbatim_reuse_objects += 1;
    }

    pub(crate) fn record_ofs_fixup(&mut self) {
        self.note_ofs_fixup();
    }

    pub(crate) fn record_verbatim_zlib(&mut self, zlib_bytes: u64) {
        self.stats.verbatim_reuse_bytes += zlib_bytes;
    }
}

impl StreamingReuse {
    fn new(pack: BitmappedPack, reuse_bits: Bitmap, send_set: &ReachableSet) -> Self {
        Self {
            pack,
            reuse_bits,
            index: Some(Arc::clone(send_set.bitmap_index())),
            source_to_output: HashMap::new(),
            source_entry_offset: HashMap::new(),
            offset_to_oid: HashMap::new(),
            stats: ReuseWriteStats::default(),
            reuse_all_in_pack: false,
        }
    }

    fn marks_reuse(&self, oid: &ObjectId) -> bool {
        if self.reuse_all_in_pack {
            return self.source_entry_offset.contains_key(oid);
        }
        let Some(index) = &self.index else {
            return false;
        };
        let Some(pos) = index.position_of(oid) else {
            return false;
        };
        self.reuse_bits.get(pos as usize)
    }

    /// Emit a whole packed object when marked reusable.
    pub(crate) fn try_emit_whole(
        &mut self,
        oid: &ObjectId,
        out_offset: u64,
        opts: &PackBuildOptions,
    ) -> Result<Option<Vec<u8>>> {
        if !opts.reuse_objects || !self.marks_reuse(oid) {
            return Ok(None);
        }
        let objects_dir = self
            .pack
            .idx
            .idx_path
            .parent()
            .and_then(|p| p.parent())
            .ok_or_else(|| Error::CorruptObject("pack path layout".to_owned()))?;
        let mut out = Vec::new();
        match write_whole_reused(&mut out, objects_dir, oid, opts) {
            Ok(n) => {
                self.note_verbatim(n);
            }
            Err(_) => return Ok(None),
        };
        if let Some(entry_off) = self.source_entry_offset.get(oid).copied() {
            self.source_to_output.insert(entry_off, out_offset);
        }
        Ok(Some(out))
    }

    /// Emit a delta object when marked reusable.
    pub(crate) fn try_emit_delta(
        &mut self,
        oid: &ObjectId,
        out_offset: u64,
        in_pack_base: Option<u64>,
        opts: &PackBuildOptions,
        oid_to_offset: &crate::objects::OidHashMap<u64>,
    ) -> Result<Option<Vec<u8>>> {
        if !self.marks_reuse(oid) {
            return Ok(None);
        }
        let Some(entry_off) = self.source_entry_offset.get(oid).copied() else {
            return Ok(None);
        };
        let pack_bytes = self.pack.pack_bytes.as_ref();
        let hash_bytes = self.pack.idx.oid_at(0).len();
        if !delta_base_in_output_set(
            pack_bytes,
            entry_off,
            hash_bytes,
            &self.source_to_output,
            &self.offset_to_oid,
            oid_to_offset,
            in_pack_base,
        ) {
            return Ok(None);
        }
        let hb = self.pack.idx.hash_bytes();
        let mut out = Vec::new();
        match emit_one_reused(
            &mut out,
            &mut self.stats,
            &self.pack,
            pack_bytes,
            entry_off,
            hb,
            opts,
            &self.source_to_output,
            oid_to_offset,
            &self.offset_to_oid,
            out_offset,
            in_pack_base,
        ) {
            Ok(_) => {}
            Err(_) => return Ok(None),
        };
        self.source_to_output.insert(entry_off, out_offset);
        Ok(Some(out))
    }
}

fn delta_base_in_output_set(
    pack_bytes: &[u8],
    entry_off: u64,
    hash_bytes: usize,
    source_to_output: &HashMap<u64, u64>,
    offset_to_oid: &HashMap<u64, ObjectId>,
    oid_to_offset: &crate::objects::OidHashMap<u64>,
    in_pack_base: Option<u64>,
) -> bool {
    let Some(dep) = read_packed_delta_dependency(pack_bytes, entry_off, hash_bytes)
        .ok()
        .flatten()
    else {
        return false;
    };
    match dep {
        PackedDeltaDependency::RefBase { base_oid } => oid_to_offset.contains_key(&base_oid),
        PackedDeltaDependency::OfsBase { base_offset } => {
            if source_to_output.contains_key(&base_offset) {
                return true;
            }
            if in_pack_base.is_some() {
                return true;
            }
            offset_to_oid
                .get(&base_offset)
                .is_some_and(|base_oid| oid_to_offset.contains_key(base_oid))
        }
    }
}

fn remapped_ofs_base(
    source_to_output: &HashMap<u64, u64>,
    offset_to_oid: &HashMap<u64, ObjectId>,
    oid_to_offset: &crate::objects::OidHashMap<u64>,
    base_offset: u64,
    in_pack_base: Option<u64>,
) -> Option<u64> {
    source_to_output
        .get(&base_offset)
        .copied()
        .or(in_pack_base)
        .or_else(|| {
            offset_to_oid
                .get(&base_offset)
                .and_then(|base_oid| oid_to_offset.get(base_oid).copied())
        })
}

/// Streaming verbatim reuse for maintenance repack (`window == 0`), when objects live in one pack.
pub fn try_streaming_reuse_for_repack(
    odb: &crate::odb::Odb,
    send: &[ObjectId],
) -> Result<Option<StreamingReuse>> {
    if odb.hash_algo() != crate::objects::HashAlgo::Sha1 {
        return Ok(None);
    }
    let objects_dir = odb.objects_dir();
    let indexes = read_local_pack_indexes_cached(objects_dir)?;
    if indexes.is_empty() {
        return Ok(None);
    }
    let mut best: Option<(usize, Arc<PackIndex>, HashMap<ObjectId, u64>)> = None;
    for idx in indexes {
        let mut source_entry_offset = HashMap::with_capacity(send.len());
        for oid in send {
            if let Some(off) = idx.find_offset(oid).ok().flatten() {
                source_entry_offset.insert(*oid, off);
            }
        }
        let hits = source_entry_offset.len();
        if hits == 0 {
            continue;
        }
        if best.as_ref().is_none_or(|(c, _, _)| hits > *c) {
            best = Some((hits, idx, source_entry_offset));
        }
    }
    let Some((hits, idx, source_entry_offset)) = best else {
        return Ok(None);
    };
    if hits != send.len() {
        // Streaming reuse only when every object is already in the source pack (zero-loose
        // rewrite). Loose objects must take the planned zlib-reuse path instead.
        return Ok(None);
    }
    let pack_bytes = read_pack_bytes_cached(&idx.pack_path)?;
    let mut offset_to_oid = HashMap::with_capacity(idx.len());
    for row in 0..idx.len() {
        let off = idx.offset_at(row)?;
        let oid = ObjectId::from_bytes(idx.oid_at(row))?;
        offset_to_oid.insert(off, oid);
    }
    let pack = BitmappedPack {
        idx,
        pack_bytes,
        bitmap_pos: 0,
        bitmap_nr: 0,
    };
    Ok(Some(StreamingReuse {
        pack,
        reuse_bits: Bitmap::new(),
        index: None,
        source_to_output: HashMap::with_capacity(send.len()),
        source_entry_offset,
        offset_to_oid,
        stats: ReuseWriteStats::default(),
        reuse_all_in_pack: true,
    }))
}

/// Write a PACK stream to `w` using bitmap reachability and verbatim reuse when possible.
///
/// Returns [`None`] when reuse is unavailable (caller should stream via [`crate::pack_objects::PackObjects`]).
#[allow(clippy::too_many_arguments)]
pub fn try_write_streaming_pack(
    repo: &Repository,
    wants: &[ObjectId],
    haves: &[ObjectId],
    source_shallow: &HashSet<ObjectId>,
    have_shallow: &HashSet<ObjectId>,
    filter: Option<&ObjectFilter>,
    opts: &PackBuildOptions,
    progress: &mut dyn FnMut(PackProgress),
    w: &mut dyn Write,
) -> Result<Option<PackStats>> {
    if !source_shallow.is_empty() || !have_shallow.is_empty() {
        return Ok(None);
    }
    let Some((send_set, have_closure)) =
        try_bitmap_reachable_for_upload(repo.odb(), wants, haves, filter, source_shallow)?
    else {
        return Ok(None);
    };
    let index = send_set.bitmap_index();
    let objects_dir = repo.odb().objects_dir();
    let packs = resolve_bitmapped_packs(objects_dir, index, allow_multi_pack_reuse(repo))?;
    if packs.is_empty() {
        return Ok(None);
    }
    let Some(pack) = packs.into_iter().next() else {
        return Ok(None);
    };
    let send: Vec<ObjectId> = send_set.iter_grouped_by_kind().collect();
    progress(PackProgress::Enumerated { total: send.len() });

    let send_bits = send_set.reuse_send_bits();
    let reuse_bits = compute_reuse_bits(&pack, send_bits, &send_set, opts)?;

    let blob_name_hashes =
        crate::pack_object_select::blob_name_hashes_for_send(index, &send, &have_closure);
    let plan = crate::pack_objects::plan_deltas(
        repo.odb(),
        &send,
        &have_closure,
        opts,
        &oid_hash_map_from(&blob_name_hashes),
        None,
        HashMap::new(),
    )?;
    progress(PackProgress::PlannedDeltas);

    let mut reuse = StreamingReuse::new(pack, reuse_bits, &send_set);
    let stats = write_delta_pack_stream(repo.odb(), &plan, opts, w, Some(&mut reuse))?;
    Ok(Some(stats))
}

fn allow_multi_pack_reuse(_repo: &Repository) -> bool {
    false
}

fn resolve_bitmapped_packs(
    objects_dir: &Path,
    index: &crate::pack_bitmap::BitmapIndex,
    multi: bool,
) -> Result<Vec<BitmappedPack>> {
    let pack_dir = objects_dir.join("pack");
    let btmp = read_midx_btmp_ranges(objects_dir)?;
    let idx_names = read_midx_pack_idx_names(objects_dir).unwrap_or_default();
    let indexes: HashMap<String, Arc<PackIndex>> = read_local_pack_indexes_cached(objects_dir)?
        .into_iter()
        .map(|idx| {
            let stem = idx
                .idx_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_owned();
            (stem, idx)
        })
        .collect();

    if !btmp.is_empty() {
        let mut ranges: Vec<_> = btmp.into_iter().filter(|r| r.bitmap_nr > 0).collect();
        if !multi {
            if let Ok(pref) = read_midx_preferred_idx_name(objects_dir) {
                let stem = pref.strip_suffix(".idx").unwrap_or(&pref);
                if let Some(pos) = idx_names.iter().position(|n| n == &pref) {
                    ranges.retain(|r| r.pack_id == pos as u32);
                } else if indexes.contains_key(stem) {
                    ranges.retain(|r| {
                        idx_names
                            .get(r.pack_id as usize)
                            .is_some_and(|n| n.strip_suffix(".idx").unwrap_or(n) == stem)
                    });
                } else {
                    ranges.truncate(1);
                }
            } else {
                ranges.truncate(1);
            }
        }
        let mut out = Vec::new();
        for range in ranges {
            let Some(name) = idx_names.get(range.pack_id as usize) else {
                continue;
            };
            let stem = name.strip_suffix(".idx").unwrap_or(name);
            let Some(idx) = indexes.get(stem) else {
                continue;
            };
            let pack_bytes = read_pack_bytes_cached(&idx.pack_path)?;
            out.push(BitmappedPack {
                idx: Arc::clone(idx),
                pack_bytes,
                bitmap_pos: range.bitmap_pos,
                bitmap_nr: range.bitmap_nr,
            });
        }
        if !out.is_empty() {
            return Ok(out);
        }
    }

    let count = index.object_count();
    if count == 0 {
        return Ok(Vec::new());
    }
    if let Some(idx) = index.backing_pack_index() {
        let pack_bytes = read_pack_bytes_cached(&idx.pack_path)?;
        return Ok(vec![BitmappedPack {
            idx,
            pack_bytes,
            bitmap_pos: 0,
            bitmap_nr: count,
        }]);
    }
    let mut with_bitmap: Vec<Arc<PackIndex>> = indexes
        .values()
        .filter(|idx| {
            idx.idx_path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|stem| pack_dir.join(format!("{stem}.bitmap")).is_file())
        })
        .cloned()
        .collect();
    with_bitmap.sort_by_key(|idx| std::cmp::Reverse(idx.len()));
    if let Some(idx) = with_bitmap.into_iter().next() {
        let pack_bytes = read_pack_bytes_cached(&idx.pack_path)?;
        return Ok(vec![BitmappedPack {
            idx,
            pack_bytes,
            bitmap_pos: 0,
            bitmap_nr: count,
        }]);
    }
    Ok(Vec::new())
}

fn compute_reuse_bits(
    pack: &BitmappedPack,
    send_bits: &Bitmap,
    send_set: &ReachableSet,
    opts: &PackBuildOptions,
) -> Result<Bitmap> {
    let mut reuse = Bitmap::new();
    let pack_bytes = pack.pack_bytes.as_ref();
    let hb = pack.idx.hash_bytes();
    loop {
        let mut changed = false;
        for rel in 0..pack.bitmap_nr {
            let pos = pack.bitmap_pos + rel;
            if !send_bits.get(pos as usize) || reuse.get(pos as usize) {
                continue;
            }
            let Ok(oid) = send_set.bitmap_index().oid_at(pos) else {
                continue;
            };
            let Some(entry_off) = pack.idx.find_offset(&oid)? else {
                continue;
            };
            if !entry_crc_ok(&pack.idx, pack_bytes, entry_off)? {
                continue;
            }
            let mut p = entry_off as usize;
            let (ty, _) = parse_pack_object_header(pack_bytes, &mut p)?;
            match ty {
                PackedType::Commit | PackedType::Tree | PackedType::Blob | PackedType::Tag => {
                    reuse.set(pos as usize);
                    changed = true;
                }
                PackedType::OfsDelta | PackedType::RefDelta => {
                    // Streaming delta reuse is only enabled for thin packs; incremental
                    // OFS fixups use on-disk zlib reuse during planning instead.
                    if !opts.thin {
                        continue;
                    }
                    let Some(base_oid) = delta_base_oid(&pack.idx, pack_bytes, entry_off, hb)?
                    else {
                        continue;
                    };
                    let Some(base_pos) = send_set.bitmap_index().position_of(&base_oid) else {
                        continue;
                    };
                    if !reuse.get(base_pos as usize) {
                        continue;
                    }
                    let Some(base_off) = pack.idx.find_offset(&base_oid)? else {
                        continue;
                    };
                    let mut bp = base_off as usize;
                    let Ok((base_ty, _)) = parse_pack_object_header(pack_bytes, &mut bp) else {
                        continue;
                    };
                    if matches!(base_ty, PackedType::OfsDelta | PackedType::RefDelta) {
                        continue;
                    }
                    reuse.set(pos as usize);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    Ok(reuse)
}

fn entry_crc_ok(idx: &PackIndex, pack_bytes: &[u8], entry_off: u64) -> Result<bool> {
    let Some(end) = pack_entry_raw_end(idx, pack_bytes, entry_off)? else {
        return Ok(false);
    };
    let slice = &pack_bytes[entry_off as usize..end];
    match idx.crc32_for_pack_offset(entry_off)? {
        Some(crc) => Ok(crc32fast::hash(slice) == crc),
        None => Ok(true),
    }
}

fn delta_base_oid(
    idx: &PackIndex,
    pack_bytes: &[u8],
    entry_off: u64,
    _hb: usize,
) -> Result<Option<ObjectId>> {
    let hb = idx.oid_at(0).len();
    let Some(dep) = read_packed_delta_dependency(pack_bytes, entry_off, hb)? else {
        return Ok(None);
    };
    Ok(Some(match dep {
        PackedDeltaDependency::RefBase { base_oid } => base_oid,
        PackedDeltaDependency::OfsBase { base_offset } => {
            let Some(row) = find_position_by_pack_offset(idx, base_offset)? else {
                return Ok(None);
            };
            ObjectId::from_bytes(idx.oid_at(row))?
        }
    }))
}

fn write_whole_reused(
    w: &mut dyn Write,
    objects_dir: &Path,
    oid: &ObjectId,
    opts: &PackBuildOptions,
) -> Result<u64> {
    if opts.reuse_objects {
        if let Some(raw) = crate::pack::packed_full_object_slice(objects_dir, oid)? {
            w.write_all(&raw).map_err(Error::Io)?;
            return Ok(raw.len() as u64);
        }
    }
    Err(Error::CorruptObject(
        "expected reusable whole object in pack".to_owned(),
    ))
}

#[allow(clippy::too_many_arguments)]
fn emit_one_reused(
    out: &mut Vec<u8>,
    stats: &mut ReuseWriteStats,
    pack: &BitmappedPack,
    pack_bytes: &[u8],
    entry_off: u64,
    _hb: usize,
    opts: &PackBuildOptions,
    source_to_output: &HashMap<u64, u64>,
    oid_to_offset: &crate::objects::OidHashMap<u64>,
    offset_to_oid: &HashMap<u64, ObjectId>,
    pack_offset: u64,
    in_pack_base: Option<u64>,
) -> Result<()> {
    let Some(end) = pack_entry_raw_end(&pack.idx, pack_bytes, entry_off)? else {
        return Err(Error::CorruptObject(
            "truncated reused pack entry".to_owned(),
        ));
    };
    let slice = &pack_bytes[entry_off as usize..end];
    if let Some(crc) = pack.idx.crc32_for_pack_offset(entry_off)? {
        if crc32fast::hash(slice) != crc {
            return Err(Error::CorruptObject("pack reuse CRC mismatch".to_owned()));
        }
    }
    let hash_bytes = pack.idx.oid_at(0).len();
    let mut p = entry_off as usize;
    let (ty, size) = parse_pack_object_header(pack_bytes, &mut p)?;
    let zlib_start = pack_entry_zlib_start(pack_bytes, entry_off, hash_bytes)?;
    match ty {
        PackedType::OfsDelta if opts.use_ofs_delta => {
            let Some(dep) = read_packed_delta_dependency(pack_bytes, entry_off, hash_bytes)? else {
                return Err(Error::CorruptObject(
                    "ofs-delta missing dependency for reuse".to_owned(),
                ));
            };
            let PackedDeltaDependency::OfsBase { base_offset } = dep else {
                return Err(Error::CorruptObject(
                    "expected ofs-delta dependency".to_owned(),
                ));
            };
            if let Some(new_base) = remapped_ofs_base(
                source_to_output,
                offset_to_oid,
                oid_to_offset,
                base_offset,
                in_pack_base,
            ) {
                let dist = pack_offset
                    .checked_sub(new_base)
                    .ok_or_else(|| Error::CorruptObject("ofs-delta fixup underflow".to_owned()))?;
                encode_pack_object_header(out, 6, size as usize);
                encode_ofs_delta_distance(out, dist);
                out.extend_from_slice(&pack_bytes[zlib_start..end]);
                stats.note_ofs_fixup();
                stats.verbatim_reuse_bytes += (end - zlib_start) as u64;
                Ok(())
            } else if opts.thin {
                append_ref_delta_from_ofs(out, pack_bytes, entry_off, end, size, pack)?;
                stats.note_ref_from_ofs();
                stats.verbatim_reuse_bytes += (end - zlib_start) as u64;
                Ok(())
            } else {
                Err(Error::CorruptObject(
                    "ofs-delta reuse missing remapped base".to_owned(),
                ))
            }
        }
        PackedType::OfsDelta if !opts.use_ofs_delta && opts.thin => {
            append_ref_delta_from_ofs(out, pack_bytes, entry_off, end, size, pack)?;
            stats.note_ref_from_ofs();
            stats.verbatim_reuse_bytes += (end - zlib_start) as u64;
            Ok(())
        }
        PackedType::RefDelta => {
            let Some(dep) = read_packed_delta_dependency(pack_bytes, entry_off, hash_bytes)? else {
                return Err(Error::CorruptObject(
                    "ref-delta missing dependency for reuse".to_owned(),
                ));
            };
            let PackedDeltaDependency::RefBase { base_oid } = dep else {
                return Err(Error::CorruptObject(
                    "expected ref-delta dependency".to_owned(),
                ));
            };
            if !oid_to_offset.contains_key(&base_oid) {
                return Err(Error::CorruptObject(
                    "ref-delta base missing from output pack".to_owned(),
                ));
            }
            out.extend_from_slice(slice);
            stats.note_verbatim_slice(slice.len() as u64);
            Ok(())
        }
        _ => {
            out.extend_from_slice(slice);
            stats.note_verbatim_slice(slice.len() as u64);
            Ok(())
        }
    }
}

fn append_ref_delta_from_ofs(
    out: &mut Vec<u8>,
    pack_bytes: &[u8],
    entry_off: u64,
    end: usize,
    size: u64,
    pack: &BitmappedPack,
) -> Result<()> {
    let hash_bytes = pack.idx.oid_at(0).len();
    let zlib_start = pack_entry_zlib_start(pack_bytes, entry_off, hash_bytes)?;
    let Some(dep) = read_packed_delta_dependency(pack_bytes, entry_off, hash_bytes)? else {
        return Err(Error::CorruptObject("missing delta base".to_owned()));
    };
    let base_oid = match dep {
        PackedDeltaDependency::RefBase { base_oid } => base_oid,
        PackedDeltaDependency::OfsBase { base_offset } => {
            let row = find_position_by_pack_offset(&pack.idx, base_offset)?.ok_or_else(|| {
                Error::CorruptObject("ofs delta base missing from index".to_owned())
            })?;
            ObjectId::from_bytes(pack.idx.oid_at(row))?
        }
    };
    encode_pack_object_header(out, 7, size as usize);
    out.extend_from_slice(base_oid.as_bytes());
    out.extend_from_slice(&pack_bytes[zlib_start..end]);
    Ok(())
}
