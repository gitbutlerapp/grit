//! Offset-order pack object verification for repository fsck.
//!
//! Inflates and hashes each packed object once while walking the pack in offset order,
//! reusing resolved bases for delta chains and parallelizing independent delta families.
#![allow(clippy::too_many_arguments)]

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::hash::hash_object;
use crate::objects::{HashAlgo, ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::pack::{
    pack_cache, parse_ofs_delta_base, parse_pack_object_header, PackedType, VerifiedPackForFsck,
    VerifyObjectRecord,
};
use crate::parallel::{try_par_map, ParallelOptions};
use crate::unpack_objects::{apply_delta, decompress_zlib_at};

/// One fully verified object from a pack fsck pass.
#[derive(Debug, Clone)]
pub(crate) struct FsckVerifiedPackObject {
    pub oid: ObjectId,
    pub kind: ObjectKind,
    pub data: Vec<u8>,
}

/// Per-object result from offset-order pack verification.
#[derive(Debug)]
pub(crate) enum PackFsckObjectOutcome {
    /// Object inflated, hash-checked, and ready for semantic fsck.
    Clean(FsckVerifiedPackObject),
    /// One object could not be read or verified; other objects in the pack may still be checked.
    Failed {
        /// Index object id for this pack slot.
        oid: ObjectId,
        /// Underlying read/hash/decompress error.
        err: Error,
    },
}

struct FsckResolvedBase {
    kind: ObjectKind,
    data: Arc<[u8]>,
}

struct PendingFsckDelta {
    offset: usize,
    oid: ObjectId,
    base_offset: Option<usize>,
    base_oid: Option<ObjectId>,
    zlib_offset: usize,
    uncompressed_size: usize,
}

fn packed_type_to_kind(pt: PackedType) -> Result<ObjectKind> {
    match pt {
        PackedType::Commit => Ok(ObjectKind::Commit),
        PackedType::Tree => Ok(ObjectKind::Tree),
        PackedType::Blob => Ok(ObjectKind::Blob),
        PackedType::Tag => Ok(ObjectKind::Tag),
        PackedType::OfsDelta | PackedType::RefDelta => Err(Error::CorruptObject(
            "expected whole object type in fsck whole path".into(),
        )),
    }
}

fn type_code_from_packed(pt: PackedType) -> Result<u8> {
    Ok(match pt {
        PackedType::Commit => 1,
        PackedType::Tree => 2,
        PackedType::Blob => 3,
        PackedType::Tag => 4,
        PackedType::OfsDelta => 6,
        PackedType::RefDelta => 7,
    })
}

fn record_zlib_bounds(
    pack_bytes: &[u8],
    record: &VerifyObjectRecord,
    hash_bytes: usize,
) -> Result<(usize, usize)> {
    let offset = record.offset;
    let mut pos = offset as usize;
    let (packed_type, size) = parse_pack_object_header(pack_bytes, &mut pos)?;
    let _ = type_code_from_packed(packed_type)?;
    match packed_type {
        PackedType::RefDelta => {
            if pos + hash_bytes > pack_bytes.len() {
                return Err(Error::CorruptObject(format!(
                    "truncated ref-delta base at offset {offset}"
                )));
            }
            pos += hash_bytes;
        }
        PackedType::OfsDelta => {
            parse_ofs_delta_base(pack_bytes, &mut pos, offset)?;
        }
        PackedType::Commit | PackedType::Tree | PackedType::Blob | PackedType::Tag => {}
    }
    let size = usize::try_from(size).map_err(|_| {
        Error::CorruptObject(format!("pack object size overflow at offset {offset}"))
    })?;
    Ok((pos, size))
}

fn verify_oid_hash(
    algo: HashAlgo,
    kind: ObjectKind,
    data: &[u8],
    expected: &ObjectId,
) -> Result<()> {
    let actual = hash_object(algo, kind, data);
    if actual != *expected {
        return Err(Error::CorruptObject(format!(
            "packed object {} hashes to {}",
            expected.to_hex(),
            actual.to_hex()
        )));
    }
    Ok(())
}

fn delta_base_ready(
    delta: &PendingFsckDelta,
    ofs_bases: &HashMap<usize, FsckResolvedBase>,
    oid_bases: &HashMap<ObjectId, FsckResolvedBase>,
    odb: &Odb,
) -> bool {
    if let Some(base_off) = delta.base_offset {
        return ofs_bases.contains_key(&base_off);
    }
    if let Some(base_id) = delta.base_oid {
        return oid_bases.contains_key(&base_id) || odb.exists(&base_id);
    }
    false
}

fn base_bytes(
    pack_bytes: &[u8],
    delta: &PendingFsckDelta,
    ofs_bases: &HashMap<usize, FsckResolvedBase>,
    oid_bases: &HashMap<ObjectId, FsckResolvedBase>,
    odb: &Odb,
) -> Result<(ObjectKind, Arc<[u8]>)> {
    if let Some(base_off) = delta.base_offset {
        let base = ofs_bases.get(&base_off).ok_or_else(|| {
            Error::CorruptObject(format!("missing ofs-delta base at offset {base_off}"))
        })?;
        return Ok((base.kind, Arc::clone(&base.data)));
    }
    if let Some(base_id) = delta.base_oid {
        if let Some(base) = oid_bases.get(&base_id) {
            return Ok((base.kind, Arc::clone(&base.data)));
        }
        let obj = odb.read(&base_id)?;
        return Ok((obj.kind, Arc::from(obj.data)));
    }
    let _ = pack_bytes;
    Err(Error::CorruptObject("delta without base".into()))
}

fn store_resolved_base(
    pack_id: u32,
    offset: usize,
    kind: ObjectKind,
    data: Arc<[u8]>,
    ofs_bases: &mut HashMap<usize, FsckResolvedBase>,
    oid_bases: &mut HashMap<ObjectId, FsckResolvedBase>,
    oid: ObjectId,
) {
    ofs_bases.insert(
        offset,
        FsckResolvedBase {
            kind,
            data: Arc::clone(&data),
        },
    );
    oid_bases.insert(
        oid,
        FsckResolvedBase {
            kind,
            data: data.clone(),
        },
    );
    pack_cache::put_delta_bases(kind, &[(pack_id, offset as u64, data)]);
}

fn resolve_one_pending_delta(
    pack_bytes: &[u8],
    delta: PendingFsckDelta,
    ofs_bases: &HashMap<usize, FsckResolvedBase>,
    oid_bases: &HashMap<ObjectId, FsckResolvedBase>,
    odb: &Odb,
    algo: HashAlgo,
) -> PackFsckObjectOutcome {
    let oid = delta.oid;
    let outcome = (|| -> Result<FsckVerifiedPackObject> {
        let (delta_data, _) =
            decompress_zlib_at(pack_bytes, delta.zlib_offset, delta.uncompressed_size)?;
        let (base_kind, base_data) = base_bytes(pack_bytes, &delta, ofs_bases, oid_bases, odb)?;
        let result = apply_delta(base_data.as_ref(), &delta_data)?;
        verify_oid_hash(algo, base_kind, &result, &oid)?;
        Ok(FsckVerifiedPackObject {
            oid,
            kind: base_kind,
            data: result,
        })
    })();
    match outcome {
        Ok(obj) => PackFsckObjectOutcome::Clean(obj),
        Err(err) => PackFsckObjectOutcome::Failed { oid, err },
    }
}

fn count_delta_children(
    records: &[VerifyObjectRecord],
) -> (HashMap<usize, u32>, HashMap<ObjectId, u32>) {
    let mut ofs_out_refs: HashMap<usize, u32> = HashMap::new();
    let mut oid_out_refs: HashMap<ObjectId, u32> = HashMap::new();
    for record in records {
        if let Some(base_off) = record.base_pack_offset {
            *ofs_out_refs.entry(base_off as usize).or_insert(0) += 1;
        }
        if let Some(base_raw) = &record.base_oid {
            if let Ok(base_id) = ObjectId::from_bytes(base_raw) {
                *oid_out_refs.entry(base_id).or_insert(0) += 1;
            }
        }
    }
    (ofs_out_refs, oid_out_refs)
}

fn verify_whole_object(
    pack_bytes: &[u8],
    record: &VerifyObjectRecord,
    hash_bytes: usize,
    algo: HashAlgo,
    pack_id: u32,
    ofs_out_refs: &HashMap<usize, u32>,
    oid_out_refs: &HashMap<ObjectId, u32>,
    ofs_bases: &mut HashMap<usize, FsckResolvedBase>,
    oid_bases: &mut HashMap<ObjectId, FsckResolvedBase>,
) -> PackFsckObjectOutcome {
    let oid = match ObjectId::from_bytes(&record.oid) {
        Ok(id) => id,
        Err(err) => {
            return PackFsckObjectOutcome::Failed {
                oid: ObjectId::zero(),
                err,
            };
        }
    };
    let (zlib_offset, uncompressed_size) = match record_zlib_bounds(pack_bytes, record, hash_bytes)
    {
        Ok(v) => v,
        Err(err) => return PackFsckObjectOutcome::Failed { oid, err },
    };
    let kind = match packed_type_to_kind(record.packed_type) {
        Ok(k) => k,
        Err(err) => return PackFsckObjectOutcome::Failed { oid, err },
    };
    let (data, _) = match decompress_zlib_at(pack_bytes, zlib_offset, uncompressed_size) {
        Ok(v) => v,
        Err(err) => return PackFsckObjectOutcome::Failed { oid, err },
    };
    if let Err(err) = verify_oid_hash(algo, kind, &data, &oid) {
        return PackFsckObjectOutcome::Failed { oid, err };
    }
    let retain = ofs_out_refs
        .get(&(record.offset as usize))
        .copied()
        .unwrap_or(0)
        > 0
        || oid_out_refs.get(&oid).copied().unwrap_or(0) > 0;
    if retain {
        store_resolved_base(
            pack_id,
            record.offset as usize,
            kind,
            Arc::from(data.as_slice()),
            ofs_bases,
            oid_bases,
            oid,
        );
    }
    PackFsckObjectOutcome::Clean(FsckVerifiedPackObject { oid, kind, data })
}

/// Verify every object in `prep`, walking the pack in offset order and reusing inflated bases.
///
/// Returns one outcome per index entry; failures on individual objects do not stop the scan.
///
/// # Errors
///
/// Returns an error only for parallel pool failures or unrecoverable pack structure problems
/// before per-object work begins.
pub(crate) fn verify_pack_objects_offset_order(
    prep: VerifiedPackForFsck,
    odb: &Odb,
    par_opts: ParallelOptions,
) -> Result<Vec<PackFsckObjectOutcome>> {
    let VerifiedPackForFsck {
        idx,
        pack_bytes,
        records,
    } = prep;
    let algo = odb.hash_algo();
    let hash_bytes = algo.len();
    let pack_id = pack_cache::pack_id_for(&idx.pack_path);
    let (ofs_out_refs, oid_out_refs) = count_delta_children(&records);
    let mut ofs_bases: HashMap<usize, FsckResolvedBase> = HashMap::new();
    let mut oid_bases: HashMap<ObjectId, FsckResolvedBase> = HashMap::new();
    let mut outcomes: Vec<PackFsckObjectOutcome> = Vec::with_capacity(records.len());
    let mut pending: Vec<PendingFsckDelta> = Vec::new();

    for record in &records {
        match record.packed_type {
            PackedType::Commit | PackedType::Tree | PackedType::Blob | PackedType::Tag => {
                outcomes.push(verify_whole_object(
                    &pack_bytes,
                    record,
                    hash_bytes,
                    algo,
                    pack_id,
                    &ofs_out_refs,
                    &oid_out_refs,
                    &mut ofs_bases,
                    &mut oid_bases,
                ));
            }
            PackedType::OfsDelta | PackedType::RefDelta => {
                let oid = ObjectId::from_bytes(&record.oid)?;
                let (zlib_offset, uncompressed_size) =
                    record_zlib_bounds(&pack_bytes, record, hash_bytes)?;
                pending.push(PendingFsckDelta {
                    offset: record.offset as usize,
                    oid,
                    base_offset: record.base_pack_offset.map(|o| o as usize),
                    base_oid: record
                        .base_oid
                        .as_ref()
                        .and_then(|raw| ObjectId::from_bytes(raw).ok()),
                    zlib_offset,
                    uncompressed_size,
                });
            }
        }
    }

    let threads = par_opts.parallelism().threads().get().max(1);
    loop {
        if pending.is_empty() {
            break;
        }
        let mut ready: Vec<PendingFsckDelta> = Vec::new();
        let mut still: Vec<PendingFsckDelta> = Vec::new();
        for delta in pending {
            if delta_base_ready(&delta, &ofs_bases, &oid_bases, odb) {
                ready.push(delta);
            } else {
                still.push(delta);
            }
        }
        pending = still;
        if ready.is_empty() {
            for delta in pending {
                outcomes.push(PackFsckObjectOutcome::Failed {
                    oid: delta.oid,
                    err: Error::CorruptObject(format!(
                        "delta base for {} could not be resolved during pack fsck",
                        delta.oid.to_hex()
                    )),
                });
            }
            break;
        }

        let resolve_delta = |delta: &PendingFsckDelta| {
            (
                delta.offset,
                resolve_one_pending_delta(
                    &pack_bytes,
                    PendingFsckDelta {
                        offset: delta.offset,
                        oid: delta.oid,
                        base_offset: delta.base_offset,
                        base_oid: delta.base_oid,
                        zlib_offset: delta.zlib_offset,
                        uncompressed_size: delta.uncompressed_size,
                    },
                    &ofs_bases,
                    &oid_bases,
                    odb,
                    algo,
                ),
            )
        };

        let batch: Vec<(usize, PackFsckObjectOutcome)> = if threads <= 1 || ready.len() == 1 {
            ready.iter().map(resolve_delta).collect()
        } else {
            try_par_map(&ready, par_opts, ready.len(), |delta| {
                Ok::<_, std::convert::Infallible>(resolve_delta(delta))
            })
            .map_err(
                |e: crate::parallel::ParallelHashError<std::convert::Infallible>| {
                    Error::Message(format!("parallel pack fsck: {e}"))
                },
            )?
        };

        for (off, outcome) in batch {
            if let PackFsckObjectOutcome::Clean(ref item) = outcome {
                let retain_ofs = ofs_out_refs.get(&off).copied().unwrap_or(0) > 0;
                let retain_oid = oid_out_refs.get(&item.oid).copied().unwrap_or(0) > 0;
                if retain_ofs || retain_oid {
                    store_resolved_base(
                        pack_id,
                        off,
                        item.kind,
                        Arc::from(item.data.as_slice()),
                        &mut ofs_bases,
                        &mut oid_bases,
                        item.oid,
                    );
                }
            }
            outcomes.push(outcome);
        }
    }

    let _ = idx;
    Ok(outcomes)
}
