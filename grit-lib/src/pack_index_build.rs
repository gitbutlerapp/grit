//! Build v2 pack index records from an in-memory pack (index-pack hashing path).
//!
//! Multi-threaded builds overlap boundary discovery (zlib skip) with parallel
//! inflate+hash on whole objects; delta chains inflate+apply+hash in parallel rounds.
//! Single-threaded builds use one inflate per object (no skip+reinflate).

use crate::error::{Error, Result};
use crate::hash::{hash_object, try_par_hash_with_force, Parallelism};
use crate::objects::{HashAlgo, ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::pack_zlib::{decompress_zlib_at_index, skip_zlib_at};
use crate::unpack_objects::{apply_delta, type_code_to_kind, PackIndexRecord, PackReader};
use std::borrow::Cow;
use std::collections::HashMap;

#[derive(Clone)]
struct IndexBase {
    kind: ObjectKind,
    data: Vec<u8>,
}

struct PendingIndexDelta {
    offset: usize,
    crc32: u32,
    base_oid: Option<ObjectId>,
    base_offset: Option<usize>,
    zlib_offset: usize,
    uncompressed_size: usize,
}

struct IndexBuildState<'a> {
    odb: &'a Odb,
    ofs_base_refs: &'a mut HashMap<usize, u32>,
    oid_base_refs: &'a mut HashMap<ObjectId, u32>,
    ofs_bases: &'a mut HashMap<usize, IndexBase>,
    oid_bases: &'a mut HashMap<ObjectId, IndexBase>,
    records: &'a mut Vec<PackIndexRecord>,
}

struct WholeSlot {
    offset: usize,
    pack_end: usize,
    zlib_offset: usize,
    kind: ObjectKind,
    uncompressed_size: usize,
}

struct WholeInflated {
    offset: usize,
    pack_end: usize,
    kind: ObjectKind,
    data: Vec<u8>,
}

struct WholeResolved {
    record: PackIndexRecord,
    offset: usize,
    oid: ObjectId,
    kind: ObjectKind,
    data: Vec<u8>,
}

struct PackHeader {
    #[allow(dead_code)]
    version: u32,
    nr_objects: usize,
}

/// Build index records for `pack`, using up to `parallelism` worker threads.
pub fn build_pack_index_records(
    pack: &[u8],
    odb: &Odb,
    parallelism: Parallelism,
) -> Result<Vec<PackIndexRecord>> {
    let algo = odb.hash_algo();
    let threads = parallelism.threads();
    let header = read_pack_header(pack)?;
    let mut ofs_base_refs: HashMap<usize, u32> = HashMap::new();
    let mut oid_base_refs: HashMap<ObjectId, u32> = HashMap::new();
    let mut pending: Vec<PendingIndexDelta> = Vec::new();
    let mut records: Vec<PackIndexRecord> = Vec::with_capacity(header.nr_objects);
    let mut ofs_bases: HashMap<usize, IndexBase> = HashMap::new();
    let mut oid_bases: HashMap<ObjectId, IndexBase> = HashMap::new();

    let mut state = IndexBuildState {
        odb,
        ofs_base_refs: &mut ofs_base_refs,
        oid_base_refs: &mut oid_base_refs,
        ofs_bases: &mut ofs_bases,
        oid_bases: &mut oid_bases,
        records: &mut records,
    };

    if threads.get() <= 1 {
        let whole_objects = scan_whole_objects_inflate(pack, &header, &mut pending, &mut state)?;
        hash_whole_objects_serial(pack, whole_objects, algo, &mut state)?;
    } else {
        let whole_slots = scan_whole_slots_skip(pack, &header, &mut pending, &mut state, algo)?;
        inflate_hash_whole_objects_parallel(pack, whole_slots, threads, algo, &mut state)?;
    }

    resolve_pending_deltas_parallel(pack, threads, algo, &mut pending, &mut state)?;
    Ok(records)
}

fn read_pack_header(pack: &[u8]) -> Result<PackHeader> {
    let sig = &pack[0..4.min(pack.len())];
    if sig != b"PACK" {
        return Err(Error::CorruptObject(
            "not a pack stream: invalid signature".to_owned(),
        ));
    }
    let mut rd = PackReader::new(pack);
    rd.read_exact(4)?;
    let version = rd.read_u32_be()?;
    if version != 2 && version != 3 {
        return Err(Error::CorruptObject(format!(
            "unsupported pack version {version}"
        )));
    }
    let nr_objects = rd.read_u32_be()? as usize;
    Ok(PackHeader {
        version,
        nr_objects,
    })
}

fn scan_whole_objects_inflate(
    pack: &[u8],
    header: &PackHeader,
    pending: &mut Vec<PendingIndexDelta>,
    state: &mut IndexBuildState<'_>,
) -> Result<Vec<WholeInflated>> {
    let hb = state.odb.hash_algo().len();
    let mut rd = PackReader::new(pack);
    rd.read_exact(4)?;
    rd.read_u32_be()?;
    rd.read_u32_be()?;
    let mut whole_objects = Vec::new();

    for _ in 0..header.nr_objects {
        let obj_offset = rd.pos;
        let (type_code, size) = rd.read_type_size()?;
        match type_code {
            1..=4 => {
                let kind = type_code_to_kind(type_code)?;
                let data = rd.decompress(size)?;
                let pack_end = rd.pos;
                whole_objects.push(WholeInflated {
                    offset: obj_offset,
                    pack_end,
                    kind,
                    data,
                });
            }
            6 => {
                push_ofs_delta(pack, &mut rd, obj_offset, size, hb, pending, state)?;
            }
            7 => {
                push_ref_delta(pack, &mut rd, obj_offset, size, hb, pending, state)?;
            }
            other => {
                return Err(Error::CorruptObject(format!(
                    "unknown packed-object type {other}"
                )))
            }
        }
    }

    let consumed = rd.pos;
    verify_pack_trailer(pack, consumed, state.odb.hash_algo())?;
    Ok(whole_objects)
}

fn scan_whole_slots_skip(
    pack: &[u8],
    header: &PackHeader,
    pending: &mut Vec<PendingIndexDelta>,
    state: &mut IndexBuildState<'_>,
    algo: HashAlgo,
) -> Result<Vec<WholeSlot>> {
    let hb = algo.len();
    let mut rd = PackReader::new(pack);
    rd.read_exact(4)?;
    rd.read_u32_be()?;
    rd.read_u32_be()?;
    let mut whole_slots = Vec::new();

    for _ in 0..header.nr_objects {
        let obj_offset = rd.pos;
        let (type_code, size) = rd.read_type_size()?;
        match type_code {
            1..=4 => {
                let kind = type_code_to_kind(type_code)?;
                let zlib_offset = rd.pos;
                let consumed = skip_zlib_at(pack, zlib_offset, size)?;
                let pack_end = rd.pos + consumed;
                rd.pos = pack_end;
                whole_slots.push(WholeSlot {
                    offset: obj_offset,
                    pack_end,
                    zlib_offset,
                    kind,
                    uncompressed_size: size,
                });
            }
            6 => {
                push_ofs_delta(pack, &mut rd, obj_offset, size, hb, pending, state)?;
            }
            7 => {
                push_ref_delta(pack, &mut rd, obj_offset, size, hb, pending, state)?;
            }
            other => {
                return Err(Error::CorruptObject(format!(
                    "unknown packed-object type {other}"
                )))
            }
        }
    }

    let consumed = rd.pos;
    verify_pack_trailer(pack, consumed, algo)?;
    Ok(whole_slots)
}

fn inflate_hash_whole_objects_parallel(
    pack: &[u8],
    whole_slots: Vec<WholeSlot>,
    threads: std::num::NonZeroUsize,
    algo: HashAlgo,
    state: &mut IndexBuildState<'_>,
) -> Result<()> {
    if whole_slots.is_empty() {
        return Ok(());
    }
    let resolved = try_par_hash_with_force(&whole_slots, threads, |work| {
        let data = decompress_zlib_at_index(pack, work.zlib_offset, work.uncompressed_size)?;
        let oid = hash_object(algo, work.kind, &data);
        Ok(WholeResolved {
            record: PackIndexRecord {
                oid,
                offset: u64::try_from(work.offset).unwrap_or(u64::MAX),
                crc32: crc32fast::hash(&pack[work.offset..work.pack_end]),
            },
            offset: work.offset,
            oid,
            kind: work.kind,
            data,
        })
    })
    .map_err(|e: crate::hash::ParallelHashError<Error>| match e {
        crate::hash::ParallelHashError::Task(err) => err,
    })?;

    for item in resolved {
        state.records.push(item.record);
        retain_bases_for_whole(
            item.offset,
            item.oid,
            item.kind,
            item.data,
            state.ofs_base_refs,
            state.oid_base_refs,
            state.ofs_bases,
            state.oid_bases,
        );
    }
    Ok(())
}

fn push_ofs_delta(
    pack: &[u8],
    rd: &mut PackReader<'_>,
    obj_offset: usize,
    size: usize,
    hb: usize,
    pending: &mut Vec<PendingIndexDelta>,
    state: &mut IndexBuildState<'_>,
) -> Result<()> {
    let _ = hb;
    let neg = rd.read_ofs_neg_offset()?;
    let base_offset = obj_offset
        .checked_sub(neg)
        .ok_or_else(|| Error::CorruptObject("ofs-delta base offset underflow".to_owned()))?;
    *state.ofs_base_refs.entry(base_offset).or_insert(0) += 1;
    let zlib_offset = rd.pos;
    let consumed = skip_zlib_at(pack, zlib_offset, size)?;
    let pack_end = rd.pos + consumed;
    rd.pos = pack_end;
    pending.push(PendingIndexDelta {
        offset: obj_offset,
        crc32: crc32fast::hash(&pack[obj_offset..pack_end]),
        base_oid: None,
        base_offset: Some(base_offset),
        zlib_offset,
        uncompressed_size: size,
    });
    Ok(())
}

fn push_ref_delta(
    pack: &[u8],
    rd: &mut PackReader<'_>,
    obj_offset: usize,
    size: usize,
    hb: usize,
    pending: &mut Vec<PendingIndexDelta>,
    state: &mut IndexBuildState<'_>,
) -> Result<()> {
    let base_bytes = rd.read_exact(hb)?;
    let base_oid = ObjectId::from_bytes(base_bytes)?;
    *state.oid_base_refs.entry(base_oid).or_insert(0) += 1;
    let zlib_offset = rd.pos;
    let consumed = skip_zlib_at(pack, zlib_offset, size)?;
    let pack_end = rd.pos + consumed;
    rd.pos = pack_end;
    pending.push(PendingIndexDelta {
        offset: obj_offset,
        crc32: crc32fast::hash(&pack[obj_offset..pack_end]),
        base_oid: Some(base_oid),
        base_offset: None,
        zlib_offset,
        uncompressed_size: size,
    });
    Ok(())
}

fn hash_whole_objects_serial(
    pack: &[u8],
    whole_objects: Vec<WholeInflated>,
    algo: HashAlgo,
    state: &mut IndexBuildState<'_>,
) -> Result<()> {
    for work in whole_objects {
        let oid = hash_object(algo, work.kind, &work.data);
        state.records.push(PackIndexRecord {
            oid,
            offset: u64::try_from(work.offset).unwrap_or(u64::MAX),
            crc32: crc32fast::hash(&pack[work.offset..work.pack_end]),
        });
        retain_bases_for_whole(
            work.offset,
            oid,
            work.kind,
            work.data,
            state.ofs_base_refs,
            state.oid_base_refs,
            state.ofs_bases,
            state.oid_bases,
        );
    }
    Ok(())
}

fn verify_pack_trailer(pack: &[u8], consumed: usize, algo: HashAlgo) -> Result<()> {
    let hb = algo.len();
    if pack.len() < consumed + hb {
        return Err(Error::CorruptObject("pack stream truncated".to_owned()));
    }
    let trailing = &pack[consumed..consumed + hb];
    let expected = algo.digest(&pack[..consumed]);
    if expected.as_bytes() != trailing {
        return Err(Error::CorruptObject(
            "pack trailing checksum mismatch".to_owned(),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn retain_bases_for_whole(
    offset: usize,
    oid: ObjectId,
    kind: ObjectKind,
    data: Vec<u8>,
    ofs_base_refs: &HashMap<usize, u32>,
    oid_base_refs: &HashMap<ObjectId, u32>,
    ofs_bases: &mut HashMap<usize, IndexBase>,
    oid_bases: &mut HashMap<ObjectId, IndexBase>,
) {
    let retain_ofs = ofs_base_refs.get(&offset).copied().unwrap_or(0) > 0;
    let retain_oid = oid_base_refs.get(&oid).copied().unwrap_or(0) > 0;
    match (retain_ofs, retain_oid) {
        (true, true) => {
            ofs_bases.insert(
                offset,
                IndexBase {
                    kind,
                    data: data.clone(),
                },
            );
            oid_bases.insert(oid, IndexBase { kind, data });
        }
        (true, false) => {
            ofs_bases.insert(offset, IndexBase { kind, data });
        }
        (false, true) => {
            oid_bases.insert(oid, IndexBase { kind, data });
        }
        (false, false) => {}
    }
}

struct DeltaResolved {
    record: PackIndexRecord,
    offset: usize,
    oid: ObjectId,
    kind: ObjectKind,
    data: Vec<u8>,
    base_offset: Option<usize>,
    base_oid: Option<ObjectId>,
}

fn resolve_pending_deltas_parallel(
    pack: &[u8],
    threads: std::num::NonZeroUsize,
    algo: HashAlgo,
    pending: &mut Vec<PendingIndexDelta>,
    state: &mut IndexBuildState<'_>,
) -> Result<()> {
    let mut remaining = std::mem::take(pending);
    loop {
        if remaining.is_empty() {
            break;
        }
        let before = remaining.len();
        let mut ready: Vec<PendingIndexDelta> = Vec::new();
        let mut still_pending: Vec<PendingIndexDelta> = Vec::new();
        for delta in remaining {
            if delta_base_ready(&delta, state.ofs_bases, state.oid_bases, state.odb) {
                ready.push(delta);
            } else {
                still_pending.push(delta);
            }
        }
        remaining = still_pending;
        if ready.is_empty() {
            return Err(Error::CorruptObject(format!(
                "{before} delta(s) could not be resolved for pack index"
            )));
        }

        let resolved = try_par_hash_with_force(&ready, threads, |delta| {
            resolve_one_delta(
                pack,
                delta,
                state.ofs_bases,
                state.oid_bases,
                state.odb,
                algo,
            )
        })
        .map_err(|e: crate::hash::ParallelHashError<Error>| match e {
            crate::hash::ParallelHashError::Task(err) => err,
        })?;

        for item in resolved {
            state.records.push(item.record);
            if state.ofs_base_refs.get(&item.offset).copied().unwrap_or(0) > 0 {
                state.ofs_bases.insert(
                    item.offset,
                    IndexBase {
                        kind: item.kind,
                        data: item.data.clone(),
                    },
                );
            }
            if state.oid_base_refs.get(&item.oid).copied().unwrap_or(0) > 0 {
                state.oid_bases.insert(
                    item.oid,
                    IndexBase {
                        kind: item.kind,
                        data: item.data,
                    },
                );
            }
            if let Some(base_off) = item.base_offset {
                if let Some(n) = state.ofs_base_refs.get_mut(&base_off) {
                    *n = n.saturating_sub(1);
                    if *n == 0 {
                        state.ofs_bases.remove(&base_off);
                    }
                }
            }
            if let Some(ref base_id) = item.base_oid {
                if let Some(n) = state.oid_base_refs.get_mut(base_id) {
                    *n = n.saturating_sub(1);
                    if *n == 0 {
                        state.oid_bases.remove(base_id);
                    }
                }
            }
        }
    }
    Ok(())
}

fn delta_base_ready(
    delta: &PendingIndexDelta,
    ofs_bases: &HashMap<usize, IndexBase>,
    oid_bases: &HashMap<ObjectId, IndexBase>,
    odb: &Odb,
) -> bool {
    if let Some(base_off) = delta.base_offset {
        return ofs_bases.contains_key(&base_off);
    }
    if let Some(ref base_id) = delta.base_oid {
        if oid_bases.contains_key(base_id) {
            return true;
        }
        return odb.read(base_id).is_ok();
    }
    false
}

fn resolve_one_delta(
    pack: &[u8],
    delta: &PendingIndexDelta,
    ofs_bases: &HashMap<usize, IndexBase>,
    oid_bases: &HashMap<ObjectId, IndexBase>,
    odb: &Odb,
    algo: HashAlgo,
) -> Result<DeltaResolved> {
    let delta_data = decompress_zlib_at_index(pack, delta.zlib_offset, delta.uncompressed_size)?;
    let base: Option<(ObjectKind, Cow<'_, [u8]>)> = if let Some(base_off) = delta.base_offset {
        ofs_bases
            .get(&base_off)
            .map(|b| (b.kind, Cow::Borrowed(b.data.as_slice())))
    } else if let Some(ref base_id) = delta.base_oid {
        if let Some(b) = oid_bases.get(base_id) {
            Some((b.kind, Cow::Borrowed(b.data.as_slice())))
        } else if let Ok(obj) = odb.read(base_id) {
            Some((obj.kind, Cow::Owned(obj.data)))
        } else {
            None
        }
    } else {
        None
    };
    let Some((base_kind, base_data)) = base else {
        return Err(Error::CorruptObject("delta base not available".to_owned()));
    };
    let result = apply_delta(base_data.as_ref(), &delta_data)?;
    let oid = hash_object(algo, base_kind, &result);
    Ok(DeltaResolved {
        record: PackIndexRecord {
            oid,
            offset: u64::try_from(delta.offset).unwrap_or(u64::MAX),
            crc32: delta.crc32,
        },
        offset: delta.offset,
        oid,
        kind: base_kind,
        data: result,
        base_offset: delta.base_offset,
        base_oid: delta.base_oid,
    })
}
