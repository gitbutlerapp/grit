//! Build the single pack written by a full repack.
//!
//! Every object keeps the cheapest representation available:
//!
//! * an entry already stored in a local pack is copied byte for byte: whole
//!   objects verbatim, deltas with only their base reference rewritten for the
//!   new layout (when their base is also being packed and the chain stays within
//!   the depth limit);
//! * everything else (loose objects, deltas whose base is dropped, and every
//!   object when deltas are not reused) goes through the streaming window search
//!   in [`crate::delta_search`], and is written as a fresh delta or recompressed
//!   whole.
//!
//! Planning holds a few dozen bytes of metadata per object; payloads are read
//! only for objects that are searched or recompressed, and the pack streams to a
//! staging file, so memory does not grow with the size of the repository.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Arc;

use flate2::write::ZlibEncoder;
use flate2::Compression;

use crate::delta_search::{search_deltas, DeltaCandidate, DeltaSearchParams};
use crate::error::{Error, Result};
use crate::hash::HashingWriter;
use crate::objects::{oid_hash_map, ObjectId, ObjectKind, OidHashMap};
use crate::odb::Odb;
use crate::pack::{
    parse_ofs_delta_base, parse_pack_object_header, read_delta_result_size_from_pack_zlib,
    read_local_pack_indexes_cached, read_pack_bytes_cached, sort_pack_indexes_oldest_first,
    PackData, PackIndex, PackedType,
};
use crate::pack_objects::{encode_ofs_delta_distance, encode_pack_object_header, pack_type_code};

/// Marker for "no object" in `u32` object-number tables.
const NONE: u32 = u32::MAX;

/// Objects encoded per parallel batch while writing; bounds buffered output.
const WRITE_BATCH: usize = 256;

/// Below this many objects the write stays on the calling thread.
const MIN_OBJECTS_FOR_PARALLEL_WRITE: usize = 2048;

/// How [`write_repack_pack`] encodes objects.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RepackPackSettings {
    /// Earlier objects tried as bases for each searched object (`0` disables new deltas).
    pub window: usize,
    /// Maximum delta chain length (`0` writes every object whole).
    pub max_depth: usize,
    /// Keep deltas already stored in local packs.
    pub reuse_deltas: bool,
    /// Copy whole objects already stored in local packs without recompressing.
    pub reuse_objects: bool,
    /// Reference in-pack bases by offset (`OFS_DELTA`) instead of by id.
    pub use_ofs_delta: bool,
    /// Soft cap on bytes held by one search window (`0` = unlimited).
    pub window_memory: u64,
    /// Delta search worker threads.
    pub search_threads: usize,
    /// Pack write worker threads.
    pub write_threads: usize,
}

/// Where an object to pack is stored now.
#[derive(Clone, Copy)]
enum Stored {
    Loose,
    Whole {
        pack: u32,
        row: u32,
        kind: ObjectKind,
        size: u64,
    },
    Delta {
        pack: u32,
        row: u32,
        /// Object number of the base when it is also being packed.
        base: u32,
        /// Pack offset of the compressed delta instructions.
        data_start: u64,
        delta_size: u64,
    },
    /// Second copy of an object already listed earlier; not written.
    Duplicate,
}

impl Stored {
    fn pack(self) -> Option<u32> {
        match self {
            Self::Whole { pack, .. } | Self::Delta { pack, .. } => Some(pack),
            Self::Loose | Self::Duplicate => None,
        }
    }
}

/// How an object is written.
enum Plan {
    /// Copy the stored whole entry verbatim.
    CopyWhole,
    /// Copy the stored delta instructions against object number `base`.
    CopyDelta { base: u32 },
    /// Write a delta chosen by the search.
    NewDelta {
        base: u32,
        delta_len: usize,
        zlib: Box<[u8]>,
    },
    /// Read the object and store it whole, freshly compressed.
    Compress,
    /// Not written.
    Skip,
}

impl Plan {
    fn base(&self) -> Option<u32> {
        match self {
            Self::CopyDelta { base } | Self::NewDelta { base, .. } => Some(*base),
            _ => None,
        }
    }
}

struct SourcePack {
    idx: Arc<PackIndex>,
    data: Arc<PackData>,
    /// Offset where the trailing checksum starts.
    end: u64,
    /// Object number for each index row, [`NONE`] when the row is not packed.
    row_to_obj: Vec<u32>,
}

/// Write a pack holding exactly `objects` to `staging_path`.
///
/// `objects` must be duplicate-free, in the order the pack should prefer
/// (most recent first); deltas are pulled after their bases as needed.
/// `name_hashes` maps blobs and trees to their path name hash for the delta
/// search.
///
/// Returns `(object id, pack offset, CRC32)` for each written entry, in pack order.
///
/// # Errors
///
/// Returns [`Error::CorruptObject`] for malformed source packs or CRC mismatches,
/// object read failures, and [`Error::Io`] when writing the staging file fails.
pub(crate) fn write_repack_pack(
    odb: &Odb,
    objects: &[ObjectId],
    name_hashes: &HashMap<ObjectId, u32>,
    settings: &RepackPackSettings,
    staging_path: &Path,
) -> Result<Vec<(ObjectId, u64, u32)>> {
    let n = u32::try_from(objects.len())
        .map_err(|_| Error::CorruptObject("too many objects for one pack".to_owned()))?;
    let _pack_context = odb.pack_read_context_guard();
    let mut packs = open_source_packs(odb)?;
    let stored = locate_objects(odb, objects, &mut packs)?;
    let mut plan = initial_plan(&stored, settings);
    let mut dependent = vec![0u32; objects.len()];
    limit_reused_chains(&mut plan, &mut dependent, settings.max_depth);

    if settings.window > 0 && settings.max_depth > 0 {
        let candidates = delta_candidates(
            odb,
            objects,
            &stored,
            &plan,
            &dependent,
            &packs,
            name_hashes,
            settings,
        )?;
        drop(dependent);
        let found = search_deltas(
            odb,
            candidates,
            DeltaSearchParams {
                window: settings.window,
                max_depth: settings.max_depth,
                window_memory: settings.window_memory,
                hash_len: odb.hash_algo().len(),
                compression: odb.pack_compression()?,
                threads: settings.search_threads,
            },
            &crate::delta_search::any_pair,
        )?;
        for delta in found {
            plan[delta.target as usize] = Plan::NewDelta {
                base: delta.base,
                delta_len: delta.delta_len,
                zlib: delta.zlib.into_boxed_slice(),
            };
        }
    }

    let order = write_order(&plan, n);
    write_pack(
        odb,
        objects,
        &stored,
        &plan,
        &packs,
        &order,
        settings,
        staging_path,
    )
}

fn open_source_packs(odb: &Odb) -> Result<Vec<SourcePack>> {
    let hash_len = odb.hash_algo().len();
    let mut indexes = read_local_pack_indexes_cached(odb.objects_dir())?;
    sort_pack_indexes_oldest_first(&mut indexes);
    indexes
        .into_iter()
        .filter(|idx| idx.hash_bytes() == hash_len)
        .map(|idx| {
            let data = read_pack_bytes_cached(&idx.pack_path)?;
            let end = (data.len() as u64).saturating_sub(hash_len as u64);
            let rows = idx.len();
            Ok(SourcePack {
                idx,
                data,
                end,
                row_to_obj: vec![NONE; rows],
            })
        })
        .collect()
}

/// Find each object's stored entry (oldest pack first) and decode its header.
fn locate_objects(
    odb: &Odb,
    objects: &[ObjectId],
    packs: &mut [SourcePack],
) -> Result<Vec<Stored>> {
    let mut rows: Vec<(u32, u32)> = Vec::with_capacity(objects.len());
    let mut loose_numbers = oid_hash_map();
    for (num, oid) in (0u32..).zip(objects) {
        let hit = packs.iter_mut().enumerate().find_map(|(p, sp)| {
            sp.idx.find_position(oid).map(|row| {
                let first = sp.row_to_obj[row] == NONE;
                if first {
                    sp.row_to_obj[row] = num;
                }
                (p as u32, row as u32, first)
            })
        });
        match hit {
            Some((p, row, true)) => rows.push((p, row)),
            Some((_, _, false)) => rows.push((NONE, NONE)),
            None => {
                loose_numbers.insert(*oid, num);
                rows.push((NONE, 0));
            }
        }
    }

    let hash_len = odb.hash_algo().len();
    rows.into_iter()
        .map(|(pack, row)| {
            if pack == NONE {
                return Ok(if row == NONE {
                    Stored::Duplicate
                } else {
                    Stored::Loose
                });
            }
            let sp = &packs[pack as usize];
            let offset = sp.idx.offset_at(row as usize)?;
            let mut pos = usize::try_from(offset)
                .map_err(|_| Error::CorruptObject("pack offset exceeds memory".to_owned()))?;
            let (packed_type, size) = parse_pack_object_header(&sp.data, &mut pos)?;
            let kind = match packed_type {
                PackedType::Commit => ObjectKind::Commit,
                PackedType::Tree => ObjectKind::Tree,
                PackedType::Blob => ObjectKind::Blob,
                PackedType::Tag => ObjectKind::Tag,
                PackedType::OfsDelta => {
                    let base_offset = parse_ofs_delta_base(&sp.data, &mut pos, offset)?;
                    // The base's copy in this pack is unclaimed when an older pack
                    // also holds it; the delta still applies to that object.
                    let base = match sp.idx.find_position_by_offset_sorted(base_offset)? {
                        Some(r) if sp.row_to_obj[r] != NONE => sp.row_to_obj[r],
                        Some(r) => object_number(
                            &ObjectId::from_bytes(sp.idx.oid_at(r))?,
                            packs,
                            &loose_numbers,
                        ),
                        None => NONE,
                    };
                    return Ok(Stored::Delta {
                        pack,
                        row,
                        base,
                        data_start: pos as u64,
                        delta_size: size,
                    });
                }
                PackedType::RefDelta => {
                    let base_oid = sp
                        .data
                        .get(pos..pos + hash_len)
                        .ok_or_else(|| Error::CorruptObject("truncated ref-delta base".to_owned()))
                        .and_then(ObjectId::from_bytes)?;
                    pos += hash_len;
                    let base = object_number(&base_oid, packs, &loose_numbers);
                    return Ok(Stored::Delta {
                        pack,
                        row,
                        base,
                        data_start: pos as u64,
                        delta_size: size,
                    });
                }
            };
            Ok(Stored::Whole {
                pack,
                row,
                kind,
                size,
            })
        })
        .collect()
}

fn object_number(oid: &ObjectId, packs: &[SourcePack], loose: &OidHashMap<u32>) -> u32 {
    packs
        .iter()
        .find_map(|sp| {
            sp.idx
                .find_position(oid)
                .map(|row| sp.row_to_obj[row])
                .filter(|&num| num != NONE)
        })
        .or_else(|| loose.get(oid).copied())
        .unwrap_or(NONE)
}

fn initial_plan(stored: &[Stored], settings: &RepackPackSettings) -> Vec<Plan> {
    stored
        .iter()
        .map(|s| match *s {
            Stored::Whole { .. } if settings.reuse_objects => Plan::CopyWhole,
            Stored::Delta { base, .. }
                if settings.reuse_deltas && settings.max_depth > 0 && base != NONE =>
            {
                Plan::CopyDelta { base }
            }
            Stored::Duplicate => Plan::Skip,
            _ => Plan::Compress,
        })
        .collect()
}

/// Cut kept delta chains that would exceed `max_depth` (or loop), and record for
/// every chain root how deep the kept deltas hanging off it go.
fn limit_reused_chains(plan: &mut [Plan], dependent: &mut [u32], max_depth: usize) {
    const UNKNOWN: u32 = u32::MAX;
    const VISITING: u32 = u32::MAX - 1;
    let max_depth = u32::try_from(max_depth).unwrap_or(VISITING - 1);
    let mut depth = vec![UNKNOWN; plan.len()];
    let mut root = vec![NONE; plan.len()];
    let mut chain: Vec<u32> = Vec::new();
    for start in 0..plan.len() {
        if depth[start] != UNKNOWN {
            continue;
        }
        let mut cur = start as u32;
        let (mut d, mut r) = loop {
            let c = cur as usize;
            match depth[c] {
                UNKNOWN => {}
                VISITING => {
                    // A cycle through kept deltas: store this one some other way.
                    plan[c] = Plan::Compress;
                    break (0, cur);
                }
                known => break (known, root[c]),
            }
            match plan[c] {
                Plan::CopyDelta { base } => {
                    depth[c] = VISITING;
                    chain.push(cur);
                    cur = base;
                }
                _ => {
                    depth[c] = 0;
                    root[c] = cur;
                    break (0, cur);
                }
            }
        };
        while let Some(x) = chain.pop() {
            let xi = x as usize;
            if !matches!(plan[xi], Plan::CopyDelta { .. }) || d >= max_depth {
                plan[xi] = Plan::Compress;
                d = 0;
                r = x;
            } else {
                d += 1;
                dependent[r as usize] = dependent[r as usize].max(d);
            }
            depth[xi] = d;
            root[xi] = r;
        }
    }
}

/// Describe every object that is not a kept delta to the delta search.
#[allow(clippy::too_many_arguments)]
fn delta_candidates(
    odb: &Odb,
    objects: &[ObjectId],
    stored: &[Stored],
    plan: &[Plan],
    dependent: &[u32],
    packs: &[SourcePack],
    name_hashes: &HashMap<ObjectId, u32>,
    settings: &RepackPackSettings,
) -> Result<Vec<DeltaCandidate>> {
    let mut kinds: Vec<Option<ObjectKind>> = vec![None; objects.len()];
    let mut out = Vec::with_capacity(objects.len() / 4);
    for (num, oid) in objects.iter().enumerate() {
        if !matches!(plan[num], Plan::CopyWhole | Plan::Compress) {
            continue;
        }
        let (kind, size) = match stored[num] {
            Stored::Whole { kind, size, .. } => (kind, size),
            Stored::Delta {
                pack, data_start, ..
            } => {
                let size = delta_result_size(&packs[pack as usize], data_start)?;
                (stored_kind(odb, objects, stored, &mut kinds, num)?, size)
            }
            Stored::Loose => {
                let info = odb.read_info(oid)?;
                (info.kind, info.size)
            }
            Stored::Duplicate => continue,
        };
        let in_pack = stored[num].pack();
        let whole_in_pack = match stored[num] {
            Stored::Whole { pack, .. } if settings.reuse_deltas => Some(pack),
            _ => None,
        };
        out.push(DeltaCandidate {
            id: num as u32,
            oid: *oid,
            kind,
            size,
            name_hash: name_hashes.get(oid).copied().unwrap_or(0),
            whole_in_pack,
            in_pack,
            dependent_depth: dependent[num] as usize,
        });
    }
    Ok(out)
}

/// Kind of a stored delta: the kind at the root of its chain.
fn stored_kind(
    odb: &Odb,
    objects: &[ObjectId],
    stored: &[Stored],
    kinds: &mut [Option<ObjectKind>],
    num: usize,
) -> Result<ObjectKind> {
    let mut chain = Vec::new();
    let mut cur = num;
    let kind = loop {
        if let Some(kind) = kinds[cur] {
            break kind;
        }
        match stored[cur] {
            Stored::Whole { kind, .. } => break kind,
            Stored::Delta { base, .. } if base != NONE && chain.len() < objects.len() => {
                chain.push(cur);
                cur = base as usize;
            }
            _ => break odb.read_info(&objects[cur])?.kind,
        }
    };
    kinds[cur] = Some(kind);
    for c in chain {
        kinds[c] = Some(kind);
    }
    Ok(kind)
}

fn delta_result_size(sp: &SourcePack, data_start: u64) -> Result<u64> {
    let mut pos = usize::try_from(data_start)
        .map_err(|_| Error::CorruptObject("pack offset exceeds memory".to_owned()))?;
    read_delta_result_size_from_pack_zlib(&sp.data, &mut pos)
}

/// Object numbers in write order: listing order, with each not-yet-written delta
/// base chain pulled in (deepest base first) right before the first object that
/// needs it.
fn write_order(plan: &[Plan], n: u32) -> Vec<u32> {
    let mut placed = vec![false; plan.len()];
    let mut order = Vec::with_capacity(plan.len());
    let mut chain = Vec::new();
    for start in 0..n {
        let mut cur = start;
        while !placed[cur as usize] {
            placed[cur as usize] = true;
            chain.push(cur);
            match plan[cur as usize].base() {
                Some(base) => cur = base,
                None => break,
            }
        }
        while let Some(num) = chain.pop() {
            if !matches!(plan[num as usize], Plan::Skip) {
                order.push(num);
            }
        }
    }
    order
}

/// Encoded body of one entry, prepared off the writer thread.
enum Prepared {
    /// Copy `[offset, end)` of the source pack; `crc` covers exactly those bytes.
    Verbatim {
        pack: u32,
        offset: u64,
        end: u64,
        crc: u32,
    },
    /// Copy the delta instructions `[start, end)` of the source pack.
    DeltaBody {
        pack: u32,
        start: u64,
        end: u64,
        delta_size: u64,
    },
    /// Freshly compressed whole object.
    Whole {
        kind: ObjectKind,
        size: usize,
        zlib: Vec<u8>,
    },
    /// Delta whose compressed body is in the plan.
    NewDelta,
}

#[allow(clippy::too_many_arguments)]
fn write_pack(
    odb: &Odb,
    objects: &[ObjectId],
    stored: &[Stored],
    plan: &[Plan],
    packs: &[SourcePack],
    order: &[u32],
    settings: &RepackPackSettings,
    staging_path: &Path,
) -> Result<Vec<(ObjectId, u64, u32)>> {
    let algo = odb.hash_algo();
    let compression = odb.pack_compression()?;
    let count = u32::try_from(order.len())
        .map_err(|_| Error::CorruptObject("pack object count exceeds u32".to_owned()))?;
    let file = File::create(staging_path).map_err(Error::Io)?;
    let mut out = HashingWriter::new(algo, BufWriter::with_capacity(1 << 20, file));
    let mut header = Vec::with_capacity(32);
    header.extend_from_slice(b"PACK");
    header.extend_from_slice(&2u32.to_be_bytes());
    header.extend_from_slice(&count.to_be_bytes());
    out.write_all(&header).map_err(Error::Io)?;
    let mut offset = header.len() as u64;

    let mut new_offset = vec![u64::MAX; objects.len()];
    let mut rows = Vec::with_capacity(order.len());
    let mut emit = |num: u32, prep: Prepared| -> Result<()> {
        let n = num as usize;
        header.clear();
        let mut crc = crc32fast::Hasher::new();
        let (body, known_crc): (&[u8], Option<u32>) = match &prep {
            Prepared::Verbatim {
                pack,
                offset: start,
                end,
                crc,
            } => (source_slice(packs, *pack, *start, *end)?, Some(*crc)),
            Prepared::Whole { kind, size, zlib } => {
                encode_pack_object_header(&mut header, pack_type_code(*kind), *size);
                (zlib.as_slice(), None)
            }
            Prepared::DeltaBody {
                pack,
                start,
                end,
                delta_size,
            } => {
                let size = usize::try_from(*delta_size)
                    .map_err(|_| Error::CorruptObject("delta size exceeds memory".to_owned()))?;
                delta_header(
                    &mut header,
                    plan,
                    &new_offset,
                    objects,
                    n,
                    offset,
                    size,
                    settings,
                )?;
                (source_slice(packs, *pack, *start, *end)?, None)
            }
            Prepared::NewDelta => {
                let Plan::NewDelta {
                    delta_len, zlib, ..
                } = &plan[n]
                else {
                    return Err(Error::CorruptObject("delta plan changed".to_owned()));
                };
                delta_header(
                    &mut header,
                    plan,
                    &new_offset,
                    objects,
                    n,
                    offset,
                    *delta_len,
                    settings,
                )?;
                (&zlib[..], None)
            }
        };
        out.write_all(&header).map_err(Error::Io)?;
        out.write_all(body).map_err(Error::Io)?;
        let crc = known_crc.unwrap_or_else(|| {
            crc.update(&header);
            crc.update(body);
            crc.finalize()
        });
        rows.push((objects[n], offset, crc));
        new_offset[n] = offset;
        offset += (header.len() + body.len()) as u64;
        Ok(())
    };

    let prepare = |num: &u32| prepare_entry(odb, objects, stored, plan, packs, *num, compression);
    let workers = settings.write_threads.max(1);
    if workers > 1 && order.len() >= MIN_OBJECTS_FOR_PARALLEL_WRITE {
        crate::parallel::run_ordered_batches(
            order,
            WRITE_BATCH,
            workers,
            || odb.pack_read_context_guard(),
            |_, num| prepare(num),
            |nums, prepared| {
                for (&num, prep) in nums.iter().zip(prepared) {
                    emit(num, prep)?;
                }
                Ok(())
            },
        )?;
    } else {
        for num in order {
            emit(*num, prepare(num)?)?;
        }
    }
    let (mut inner, _) = out.finish().map_err(Error::Io)?;
    inner.flush().map_err(Error::Io)?;
    Ok(rows)
}

fn prepare_entry(
    odb: &Odb,
    objects: &[ObjectId],
    stored: &[Stored],
    plan: &[Plan],
    packs: &[SourcePack],
    num: u32,
    compression: Compression,
) -> Result<Prepared> {
    let n = num as usize;
    match (&plan[n], stored[n]) {
        (Plan::CopyWhole, Stored::Whole { pack, row, .. }) => {
            let (offset, end, crc) = checked_entry(packs, pack, row, &objects[n])?;
            Ok(Prepared::Verbatim {
                pack,
                offset,
                end,
                crc,
            })
        }
        (
            Plan::CopyDelta { .. },
            Stored::Delta {
                pack,
                row,
                data_start,
                delta_size,
                ..
            },
        ) => {
            let (_, end, _) = checked_entry(packs, pack, row, &objects[n])?;
            Ok(Prepared::DeltaBody {
                pack,
                start: data_start,
                end,
                delta_size,
            })
        }
        (Plan::NewDelta { .. }, _) => Ok(Prepared::NewDelta),
        _ => {
            let obj = odb.read(&objects[n])?;
            let mut enc =
                ZlibEncoder::new(Vec::with_capacity(obj.data.len() / 3 + 64), compression);
            enc.write_all(&obj.data).map_err(Error::Io)?;
            Ok(Prepared::Whole {
                kind: obj.kind,
                size: obj.data.len(),
                zlib: enc.finish().map_err(Error::Io)?,
            })
        }
    }
}

/// Bounds of a stored entry, after checking it against the index CRC when there is one.
fn checked_entry(
    packs: &[SourcePack],
    pack: u32,
    row: u32,
    oid: &ObjectId,
) -> Result<(u64, u64, u32)> {
    let sp = &packs[pack as usize];
    let offset = sp.idx.offset_at(row as usize)?;
    let end = sp.idx.next_pack_offset_after(offset, sp.end)?;
    let crc = crc32fast::hash(source_slice(packs, pack, offset, end)?);
    if sp
        .idx
        .crc32_at(row as usize)
        .is_some_and(|want| want != crc)
    {
        return Err(Error::CorruptObject(format!(
            "packed object {oid} does not match its index CRC"
        )));
    }
    Ok((offset, end, crc))
}

fn source_slice(packs: &[SourcePack], pack: u32, start: u64, end: u64) -> Result<&[u8]> {
    let data: &[u8] = &packs[pack as usize].data;
    usize::try_from(start)
        .ok()
        .zip(usize::try_from(end).ok())
        .and_then(|(s, e)| data.get(s..e))
        .ok_or_else(|| Error::CorruptObject("pack entry out of bounds".to_owned()))
}

/// Header of a delta entry at `offset` whose base was written earlier.
#[allow(clippy::too_many_arguments)]
fn delta_header(
    header: &mut Vec<u8>,
    plan: &[Plan],
    new_offset: &[u64],
    objects: &[ObjectId],
    n: usize,
    offset: u64,
    size: usize,
    settings: &RepackPackSettings,
) -> Result<()> {
    let base = plan[n]
        .base()
        .ok_or_else(|| Error::CorruptObject("delta without base".to_owned()))?
        as usize;
    let base_offset = new_offset[base];
    if base_offset == u64::MAX {
        return Err(Error::CorruptObject(
            "delta base not written before its target".to_owned(),
        ));
    }
    if settings.use_ofs_delta {
        encode_pack_object_header(header, 6, size);
        encode_ofs_delta_distance(header, offset - base_offset);
    } else {
        encode_pack_object_header(header, 7, size);
        header.extend_from_slice(objects[base].as_bytes());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn copy_delta(base: u32) -> Plan {
        Plan::CopyDelta { base }
    }

    fn bases(plan: &[Plan]) -> Vec<Option<u32>> {
        plan.iter().map(Plan::base).collect()
    }

    #[test]
    fn limit_reused_chains_cuts_every_link_past_max_depth() {
        // 0 <- 1 <- 2 <- 3 <- 4 <- 5, max depth 2: 3 is cut and starts a new chain.
        let mut plan = vec![
            Plan::CopyWhole,
            copy_delta(0),
            copy_delta(1),
            copy_delta(2),
            copy_delta(3),
            copy_delta(4),
        ];
        let mut dependent = vec![0; plan.len()];
        limit_reused_chains(&mut plan, &mut dependent, 2);
        assert_eq!(
            bases(&plan),
            vec![None, Some(0), Some(1), None, Some(3), Some(4)]
        );
        assert!(matches!(plan[3], Plan::Compress));
        assert_eq!(dependent[0], 2);
        assert_eq!(dependent[3], 2);
    }

    #[test]
    fn limit_reused_chains_records_deepest_branch_per_root() {
        // Root 0 with branches 0 <- 1 and 0 <- 2 <- 3.
        let mut plan = vec![Plan::CopyWhole, copy_delta(0), copy_delta(0), copy_delta(2)];
        let mut dependent = vec![0; plan.len()];
        limit_reused_chains(&mut plan, &mut dependent, 50);
        assert_eq!(bases(&plan), vec![None, Some(0), Some(0), Some(2)]);
        assert_eq!(dependent, vec![2, 0, 0, 0]);
    }

    #[test]
    fn limit_reused_chains_breaks_cycles() {
        let mut plan = vec![copy_delta(1), copy_delta(0), copy_delta(1)];
        let mut dependent = vec![0; plan.len()];
        limit_reused_chains(&mut plan, &mut dependent, 50);
        let kept: Vec<bool> = plan
            .iter()
            .map(|p| matches!(p, Plan::CopyDelta { .. }))
            .collect();
        assert_eq!(kept.iter().filter(|k| !**k).count(), 1, "one link is cut");
        assert!(matches!(plan[2], Plan::CopyDelta { base: 1 }));
    }

    #[test]
    fn limit_reused_chains_with_zero_depth_keeps_no_delta() {
        let mut plan = vec![Plan::CopyWhole, copy_delta(0), copy_delta(1)];
        let mut dependent = vec![0; plan.len()];
        limit_reused_chains(&mut plan, &mut dependent, 0);
        assert_eq!(bases(&plan), vec![None, None, None]);
    }

    #[test]
    fn write_order_puts_bases_first_and_drops_skipped() {
        // 0 deltas against 3, 3 against 2; 1 is a duplicate.
        let plan = vec![
            copy_delta(3),
            Plan::Skip,
            Plan::CopyWhole,
            Plan::NewDelta {
                base: 2,
                delta_len: 4,
                zlib: Box::new([]),
            },
            Plan::Compress,
        ];
        assert_eq!(write_order(&plan, 5), vec![2, 3, 0, 4]);
    }
}
