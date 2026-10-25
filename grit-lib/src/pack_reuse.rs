//! Verbatim pack reuse for bitmap-accelerated upload-pack (streaming PACK v2).
//!
//! Pack reuse is internal to upload-pack; there is no separate public embedder API.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use crate::bitmap_walk::ReachableSet;
use crate::error::{Error, Result};
use crate::ewah_bitmap::Bitmap;
use crate::midx::{read_midx_btmp_ranges, read_midx_pack_idx_names, read_midx_preferred_idx_name};
use crate::objects::ObjectId;
use crate::pack::{
    pack_entry_raw_end, parse_pack_object_header, read_local_pack_indexes_cached,
    read_pack_bytes_cached, read_packed_delta_dependency, PackedDeltaDependency, PackedType,
};
use crate::pack_index::PackIndex;
use crate::pack_map::PackData;
use crate::pack_object_select::try_bitmap_reachable_for_upload;
use crate::pack_objects::{write_delta_pack_stream, PackBuildOptions, PackProgress, PackStats};
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

/// Active verbatim reuse context while streaming a pack body.
pub struct StreamingReuse {
    pack: BitmappedPack,
    reuse_bits: Bitmap,
    index: Arc<crate::pack_bitmap::BitmapIndex>,
    stats: ReuseWriteStats,
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
            index: Arc::clone(send_set.bitmap_index()),
            stats: ReuseWriteStats::default(),
        }
    }

    fn marks_reuse(&self, oid: &ObjectId) -> bool {
        self.index
            .position_of(oid)
            .is_some_and(|pos| self.reuse_bits.get(pos as usize))
    }

    /// Emit a whole packed object when marked reusable.
    pub(crate) fn try_emit_whole(
        &mut self,
        oid: &ObjectId,
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
        Ok(Some(out))
    }
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
        crate::pack_object_select::name_hashes_for_send(index, &send, &have_closure);
    let plan = crate::pack_objects::plan_deltas(
        repo.odb(),
        &send,
        &have_closure,
        opts,
        &blob_name_hashes,
        None,
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
            let Some(row) = idx.find_position_by_offset_sorted(base_offset)? else {
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
