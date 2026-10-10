//! Verbatim pack reuse for bitmap-accelerated upload-pack (streaming PACK v2).

#![allow(dead_code, clippy::too_many_arguments)]

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
    find_position_by_pack_offset, pack_entry_raw_end, parse_pack_object_header,
    read_local_pack_indexes_cached, read_pack_bytes_cached, read_packed_delta_dependency,
    PackedDeltaDependency, PackedType,
};
use crate::pack_index::PackIndex;
use crate::pack_map::PackData;
use crate::pack_object_select::try_bitmap_reachable_for_upload;
use crate::pack_objects::{
    encode_ofs_delta_distance, encode_pack_object_header, PackBuildOptions, PackProgress, PackStats,
};
use crate::repo::Repository;
use crate::rev_list::ObjectFilter;

struct BitmappedPack {
    idx: Arc<PackIndex>,
    pack_bytes: Arc<PackData>,
    bitmap_pos: u32,
    bitmap_nr: u32,
}

/// Write a PACK stream to `w` using bitmap reachability and verbatim reuse when possible.
///
/// Returns [`None`] when reuse is unavailable (caller should stream via [`crate::pack_objects::PackObjects`]).
pub fn try_write_streaming_pack(
    repo: &Repository,
    wants: &[ObjectId],
    haves: &[ObjectId],
    shallow_grafts: &HashSet<ObjectId>,
    filter: Option<&ObjectFilter>,
    opts: &PackBuildOptions,
    progress: &mut dyn FnMut(PackProgress),
    w: &mut dyn Write,
) -> Result<Option<PackStats>> {
    let _ = (
        repo,
        wants,
        haves,
        shallow_grafts,
        filter,
        opts,
        progress,
        w,
    );
    if try_bitmap_reachable_for_upload(&repo.odb, wants, haves, filter, shallow_grafts)?.is_none() {
        return Ok(None);
    }
    // Verbatim reuse streaming is implemented below but not yet enabled; upload-pack
    // already streams via [`crate::pack_objects::PackObjects`] when this returns `None`.
    Ok(None)
}

struct CountWriter<W: Write> {
    inner: W,
    count: u64,
}

impl<W: Write> CountWriter<W> {
    fn new(inner: W) -> Self {
        Self { inner, count: 0 }
    }
    fn bytes(&self) -> u64 {
        self.count
    }
}

impl<W: Write> Write for CountWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.count += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn allow_multi_pack_reuse(_repo: &Repository) -> bool {
    false
}

fn write_pack_header(w: &mut dyn Write, count: u32) -> Result<()> {
    w.write_all(b"PACK").map_err(Error::Io)?;
    w.write_all(&2u32.to_be_bytes()).map_err(Error::Io)?;
    w.write_all(&count.to_be_bytes()).map_err(Error::Io)?;
    Ok(())
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
                } else if let Some((_, pack)) = indexes.get(stem).map(|i| (stem, i)) {
                    let _ = pack;
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
    for (stem, idx) in &indexes {
        let bitmap_path = pack_dir.join(format!("{stem}.bitmap"));
        if !bitmap_path.is_file() {
            continue;
        }
        let pack_bytes = read_pack_bytes_cached(&idx.pack_path)?;
        return Ok(vec![BitmappedPack {
            idx: Arc::clone(idx),
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
) -> Result<Bitmap> {
    let mut reuse = Bitmap::new();
    let pack_bytes = pack.pack_bytes.as_ref();
    let _hb = pack.idx.hash_bytes();
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
            let Some(entry_off) = pack.idx.find_offset(&oid) else {
                continue;
            };
            if !entry_crc_ok(&pack.idx, pack_bytes, entry_off) {
                continue;
            }
            let mut p = entry_off as usize;
            let (ty, _) = parse_pack_object_header(pack_bytes, &mut p)?;
            let mark = match ty {
                PackedType::Commit | PackedType::Tree | PackedType::Blob | PackedType::Tag => true,
                PackedType::RefDelta | PackedType::OfsDelta => false,
            };
            if mark {
                reuse.set(pos as usize);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    Ok(reuse)
}

fn entry_crc_ok(idx: &PackIndex, pack_bytes: &[u8], entry_off: u64) -> bool {
    let Some(end) = pack_entry_raw_end(idx, pack_bytes, entry_off) else {
        return false;
    };
    let slice = &pack_bytes[entry_off as usize..end];
    match idx.crc32_for_pack_offset(entry_off) {
        Some(crc) => crc32fast::hash(slice) == crc,
        None => true,
    }
}

fn delta_base_oid(
    idx: &PackIndex,
    pack_bytes: &[u8],
    entry_off: u64,
    _hb: usize,
) -> Result<Option<ObjectId>> {
    let Some(dep) = read_packed_delta_dependency(pack_bytes, entry_off)? else {
        return Ok(None);
    };
    Ok(Some(match dep {
        PackedDeltaDependency::RefBase { base_oid } => base_oid,
        PackedDeltaDependency::OfsBase { base_offset } => {
            let Some(row) = find_position_by_pack_offset(idx, base_offset) else {
                return Ok(None);
            };
            ObjectId::from_bytes(idx.oid_at(row))?
        }
    }))
}

fn write_reused_from_pack(
    w: &mut dyn Write,
    pack: &BitmappedPack,
    reuse_bits: &Bitmap,
    send_set: &ReachableSet,
    opts: &PackBuildOptions,
    reused_oids: &mut HashSet<ObjectId>,
    pack_offset: &mut u64,
) -> Result<u32> {
    let mut count = 0u32;
    let mut offset_map: HashMap<u64, u64> = HashMap::new();
    for rel in 0..pack.bitmap_nr {
        let pos = pack.bitmap_pos + rel;
        if !reuse_bits.get(pos as usize) {
            continue;
        }
        let oid = send_set
            .bitmap_index()
            .oid_at(pos)
            .map_err(|_| Error::CorruptObject("bitmap oid missing during reuse".to_owned()))?;
        let objects_dir = pack
            .idx
            .idx_path
            .parent()
            .and_then(|p| p.parent())
            .ok_or_else(|| Error::CorruptObject("pack path layout".to_owned()))?;
        let start_out = *pack_offset;
        let written = write_whole_reused(w, objects_dir, &oid, *pack_offset, opts)?;
        *pack_offset += written;
        if let Some(entry_off) = pack.idx.find_offset(&oid) {
            offset_map.insert(entry_off, start_out);
        }
        reused_oids.insert(oid);
        count += 1;
    }
    Ok(count)
}

fn write_whole_reused(
    w: &mut dyn Write,
    objects_dir: &Path,
    oid: &ObjectId,
    _pack_offset: u64,
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

fn write_one_reused(
    w: &mut dyn Write,
    pack: &BitmappedPack,
    pack_bytes: &[u8],
    entry_off: u64,
    _hb: usize,
    opts: &PackBuildOptions,
    offset_map: &HashMap<u64, u64>,
    pack_offset: u64,
) -> Result<u64> {
    let Some(end) = pack_entry_raw_end(&pack.idx, pack_bytes, entry_off) else {
        return Err(Error::CorruptObject(
            "truncated reused pack entry".to_owned(),
        ));
    };
    let slice = &pack_bytes[entry_off as usize..end];
    if let Some(crc) = pack.idx.crc32_for_pack_offset(entry_off) {
        if crc32fast::hash(slice) != crc {
            return Err(Error::CorruptObject("pack reuse CRC mismatch".to_owned()));
        }
    }
    let mut p = entry_off as usize;
    let (ty, size) = parse_pack_object_header(pack_bytes, &mut p)?;
    match ty {
        PackedType::OfsDelta if opts.use_ofs_delta => {
            let Some(dep) = read_packed_delta_dependency(pack_bytes, entry_off)? else {
                w.write_all(slice).map_err(Error::Io)?;
                return Ok(slice.len() as u64);
            };
            let PackedDeltaDependency::OfsBase { base_offset } = dep else {
                w.write_all(slice).map_err(Error::Io)?;
                return Ok(slice.len() as u64);
            };
            if let Some(new_base) = offset_map.get(&base_offset).copied() {
                let dist = pack_offset
                    .checked_sub(new_base)
                    .ok_or_else(|| Error::CorruptObject("ofs-delta fixup underflow".to_owned()))?;
                let mut out = Vec::new();
                encode_pack_object_header(&mut out, 6, size as usize);
                encode_ofs_delta_distance(&mut out, dist);
                out.extend_from_slice(&pack_bytes[p..end]);
                w.write_all(&out).map_err(Error::Io)?;
                Ok(out.len() as u64)
            } else if opts.thin {
                write_ref_delta_from_ofs(w, pack_bytes, entry_off, p, end, size, pack)
            } else {
                w.write_all(slice).map_err(Error::Io)?;
                Ok(slice.len() as u64)
            }
        }
        PackedType::OfsDelta if !opts.use_ofs_delta && opts.thin => {
            write_ref_delta_from_ofs(w, pack_bytes, entry_off, p, end, size, pack)
        }
        _ => {
            w.write_all(slice).map_err(Error::Io)?;
            Ok(slice.len() as u64)
        }
    }
}

fn write_ref_delta_from_ofs(
    w: &mut dyn Write,
    pack_bytes: &[u8],
    entry_off: u64,
    header_end: usize,
    end: usize,
    size: u64,
    pack: &BitmappedPack,
) -> Result<u64> {
    let Some(dep) = read_packed_delta_dependency(pack_bytes, entry_off)? else {
        return Err(Error::CorruptObject("missing delta base".to_owned()));
    };
    let base_oid = match dep {
        PackedDeltaDependency::RefBase { base_oid } => base_oid,
        PackedDeltaDependency::OfsBase { base_offset } => {
            let row = find_position_by_pack_offset(&pack.idx, base_offset).ok_or_else(|| {
                Error::CorruptObject("ofs delta base missing from index".to_owned())
            })?;
            ObjectId::from_bytes(pack.idx.oid_at(row))?
        }
    };
    let mut out = Vec::new();
    encode_pack_object_header(&mut out, 7, size as usize);
    out.extend_from_slice(base_oid.as_bytes());
    out.extend_from_slice(&pack_bytes[header_end..end]);
    w.write_all(&out).map_err(Error::Io)?;
    Ok(out.len() as u64)
}
