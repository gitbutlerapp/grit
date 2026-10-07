//! `unpack-objects`: unpack a pack stream into loose objects.
//!
//! Reads a pack-format byte stream, validates the trailing checksum, and
//! writes each object as a loose file in the object database.  Delta objects
//! (both `OFS_DELTA` and `REF_DELTA`) are resolved against already-unpacked
//! objects or objects already present in the ODB.
//!
//! Large blobs are written to the ODB and dropped from the in-memory maps so
//! cloning multi-gigabyte repositories does not require holding the full pack
//! in RAM (streaming read + bounded retention).

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io::{self, Read};

use crate::error::{Error, Result};
use crate::gitmodules;
use crate::hash::ObjectHasher;
use crate::index::MODE_GITLINK;
use crate::objects::{parse_commit, parse_tag, parse_tree, HashAlgo, Object, ObjectId, ObjectKind};
use crate::odb::Odb;
use flate2::read::ZlibDecoder;
use flate2::{Decompress, FlushDecompress, Status};

/// Options controlling `unpack-objects` behaviour.
#[derive(Debug, Default)]
pub struct UnpackOptions {
    /// Validate and decompress objects but do not write them to the ODB.
    pub dry_run: bool,
    /// Suppress informational output.
    pub quiet: bool,
    /// Reject packs whose commits/trees/tags reference missing objects.
    pub strict: bool,
    /// Object IDs that strict connectivity may treat as promised by a configured promisor remote.
    pub allowed_missing: HashSet<ObjectId>,
    /// Whether strict connectivity may tolerate references to missing objects in a promisor repo.
    pub allow_promisor_missing_references: bool,
    /// Maximum number of raw pack bytes that may be consumed (including the 20-byte trailer).
    ///
    /// Matches Git's `unpack-objects --max-input-size` / `receive.maxInputSize`: counts every
    /// byte read from the pack stream after crossing the limit. `None` means no limit.
    pub max_input_bytes: Option<u64>,
    /// Commit OIDs that are shallow boundaries (grafts): their parents are intentionally absent and
    /// must not be required during the `--strict` connectivity walk.
    ///
    /// Mirrors `unpack-objects --shallow-file <file>` in upstream `receive-pack`, where the shallow
    /// file lists the commits whose parent objects were deliberately not transferred.
    pub shallow_boundaries: HashSet<ObjectId>,
}

/// A delta that could not yet be resolved because its base was not yet known.
struct PendingDelta {
    /// Byte offset of this object in the pack stream (used to anchor
    /// `OFS_DELTA` back-references from later objects).
    offset: usize,
    /// For `REF_DELTA`: SHA-1 of the base object.
    base_oid: Option<ObjectId>,
    /// For `OFS_DELTA`: absolute byte offset of the base object.
    base_offset: Option<usize>,
    /// Decompressed delta data.
    delta_data: Vec<u8>,
}

/// Unpack a pack stream from `reader` into `odb`.
///
/// Reads the complete pack from `reader`, validates the trailing SHA-1
/// checksum, unpacks all objects (including full delta-chain resolution), and —
/// unless [`UnpackOptions::dry_run`] is set — writes each object to `odb`.
///
/// Returns the total number of objects processed.
///
/// # Errors
///
/// - [`Error::CorruptObject`] — invalid pack format, checksum mismatch, or
///   unresolvable delta chains.
/// - [`Error::Io`] — I/O failure reading from `reader`.
/// - [`Error::Zlib`] — decompression failure.
pub fn unpack_objects(reader: &mut dyn Read, odb: &Odb, opts: &UnpackOptions) -> Result<usize> {
    /// Blobs larger than this stay on disk only (after write) so huge packs do
    /// not retain every blob in RAM. Smaller objects are kept for delta bases
    /// and `--strict` graph walks without extra ODB reads.
    const MAX_RETAIN_BYTES: usize = 1024 * 1024;

    let algo = odb.hash_algo();
    let mut rd = StreamingPackReader::new(reader, opts.max_input_bytes, algo);

    // Validate magic and version.
    let sig = rd.read_exact_n(4)?;
    if sig != b"PACK" {
        return Err(Error::CorruptObject(
            "not a pack stream: invalid signature".to_owned(),
        ));
    }
    let version = rd.read_u32_be()?;
    if version != 2 && version != 3 {
        return Err(Error::CorruptObject(format!(
            "unsupported pack version {version}"
        )));
    }
    let nr_objects = rd.read_u32_be()? as usize;

    // pack-stream offset → resolved object (see [`PackedObjectEntry`]).
    let mut by_offset: HashMap<usize, PackedObjectEntry> = HashMap::new();
    // ObjectId → in-pack object for REF_DELTA resolution and strict checks.
    let mut by_oid: HashMap<ObjectId, PackedObjectEntry> = HashMap::new();

    let mut pending: Vec<PendingDelta> = Vec::new();
    let mut count = 0usize;

    for _ in 0..nr_objects {
        let obj_offset = rd.stream_pos();
        let (type_code, size) = rd.read_type_size()?;

        match type_code {
            1..=4 => {
                let kind = type_code_to_kind(type_code)?;
                let data = rd.decompress(size)?;
                let oid = write_or_hash(kind, &data, odb, opts.dry_run)?;
                let entry = packed_entry_after_write(kind, data, oid, odb, opts, MAX_RETAIN_BYTES);
                by_offset.insert(obj_offset, entry.clone());
                by_oid.insert(oid, entry);
                count += 1;
            }
            6 => {
                // OFS_DELTA: base at a negative encoded offset from this object.
                let neg = rd.read_ofs_neg_offset()?;
                let base_offset = obj_offset.checked_sub(neg).ok_or_else(|| {
                    Error::CorruptObject("ofs-delta base offset underflow".to_owned())
                })?;
                let delta_data = rd.decompress(size)?;
                pending.push(PendingDelta {
                    offset: obj_offset,
                    base_oid: None,
                    base_offset: Some(base_offset),
                    delta_data,
                });
            }
            7 => {
                // REF_DELTA: base identified by its object id (hash-width bytes).
                let base_bytes = rd.read_exact_n(algo.len())?;
                let base_oid = ObjectId::from_bytes(&base_bytes)?;
                let delta_data = rd.decompress(size)?;
                pending.push(PendingDelta {
                    offset: obj_offset,
                    base_oid: Some(base_oid),
                    base_offset: None,
                    delta_data,
                });
            }
            other => {
                return Err(Error::CorruptObject(format!(
                    "unknown packed-object type {other}"
                )))
            }
        }
    }

    // Trailing pack checksum (hash of all preceding bytes); not included in the hash.
    let digest = rd.finalize_hasher();
    let trailing = rd.read_trailer()?;
    if digest != trailing {
        return Err(Error::CorruptObject(
            "pack trailing checksum mismatch".to_owned(),
        ));
    }

    // Resolve pending deltas iteratively.  Each pass resolves all deltas whose
    // base is now known; repeat until none remain or we stall (corrupt pack).
    let mut remaining = pending;
    loop {
        if remaining.is_empty() {
            break;
        }
        let before = remaining.len();
        let mut still_pending: Vec<PendingDelta> = Vec::new();

        for delta in remaining {
            let base_res: Option<Result<(ObjectKind, Cow<'_, [u8]>)>> =
                if let Some(base_off) = delta.base_offset {
                    by_offset
                        .get(&base_off)
                        .map(|e| entry_object_bytes(e, odb).map(|d| (e.kind(), d)))
                } else if let Some(ref base_id) = delta.base_oid {
                    if let Some(e) = by_oid.get(base_id) {
                        Some(entry_object_bytes(e, odb).map(|d| (e.kind(), d)))
                    } else if !opts.dry_run {
                        odb.read(base_id)
                            .ok()
                            .map(|obj| Ok((obj.kind, Cow::Owned(obj.data))))
                    } else {
                        None
                    }
                } else {
                    None
                };

            match base_res {
                Some(Ok((base_kind, base_data))) => {
                    let result = apply_delta(base_data.as_ref(), &delta.delta_data)?;
                    let oid = write_or_hash(base_kind, &result, odb, opts.dry_run)?;
                    let new_entry = packed_entry_after_write(
                        base_kind,
                        result,
                        oid,
                        odb,
                        opts,
                        MAX_RETAIN_BYTES,
                    );
                    by_offset.insert(delta.offset, new_entry.clone());
                    by_oid.insert(oid, new_entry);
                    count += 1;
                }
                Some(Err(e)) => return Err(e),
                None => still_pending.push(delta),
            }
        }

        remaining = still_pending;
        if remaining.len() == before {
            return Err(Error::CorruptObject(format!(
                "{} delta(s) could not be resolved",
                remaining.len()
            )));
        }
    }

    if opts.strict {
        let mut dot_fsck_map: HashMap<ObjectId, (ObjectKind, Vec<u8>)> =
            HashMap::with_capacity(by_oid.len());
        for (oid, entry) in &by_oid {
            let kind = entry.kind();
            let data = match entry {
                PackedObjectEntry::InMemory { data, .. } => data.clone(),
                PackedObjectEntry::BlobOnDisk { oid: blob_oid } => odb.read(blob_oid)?.data,
            };
            dot_fsck_map.insert(*oid, (kind, data));
        }
        gitmodules::verify_packed_dot_special(&dot_fsck_map)?;
        strict_verify_packed_references_map(
            Some(odb),
            &by_oid,
            &opts.allowed_missing,
            opts.allow_promisor_missing_references,
            &opts.shallow_boundaries,
        )?;
    }

    Ok(count)
}

/// Resolved non-delta object: either full bytes in memory or a large blob on disk.
#[derive(Debug, Clone)]
enum PackedObjectEntry {
    InMemory { kind: ObjectKind, data: Vec<u8> },
    BlobOnDisk { oid: ObjectId },
}

impl PackedObjectEntry {
    fn kind(&self) -> ObjectKind {
        match self {
            PackedObjectEntry::InMemory { kind, .. } => *kind,
            PackedObjectEntry::BlobOnDisk { .. } => ObjectKind::Blob,
        }
    }
}

fn packed_entry_after_write(
    kind: ObjectKind,
    data: Vec<u8>,
    oid: ObjectId,
    _odb: &Odb,
    opts: &UnpackOptions,
    max_retain: usize,
) -> PackedObjectEntry {
    if !opts.dry_run && kind == ObjectKind::Blob && data.len() > max_retain {
        PackedObjectEntry::BlobOnDisk { oid }
    } else {
        PackedObjectEntry::InMemory { kind, data }
    }
}

fn entry_object_bytes<'a>(entry: &'a PackedObjectEntry, odb: &Odb) -> Result<Cow<'a, [u8]>> {
    match entry {
        PackedObjectEntry::InMemory { data, .. } => Ok(Cow::Borrowed(data.as_slice())),
        PackedObjectEntry::BlobOnDisk { oid } => Ok(Cow::Owned(odb.read(oid)?.data)),
    }
}

fn strict_verify_packed_references_map(
    odb: Option<&Odb>,
    pack: &HashMap<ObjectId, PackedObjectEntry>,
    allowed_missing: &HashSet<ObjectId>,
    allow_promisor_missing_references: bool,
    shallow_boundaries: &HashSet<ObjectId>,
) -> Result<()> {
    for (oid, entry) in pack {
        match entry {
            PackedObjectEntry::BlobOnDisk { .. } => {}
            PackedObjectEntry::InMemory { kind, data } => match kind {
                ObjectKind::Tree => {
                    for e in parse_tree(data)? {
                        // Gitlink (submodule) entries point at commits that live
                        // in the submodule repository, not the superproject's
                        // pack/ODB. Skip them in the connectivity walk, matching
                        // upstream git (git/fsck.c:374 `if (S_ISGITLINK) continue;`).
                        if e.mode == MODE_GITLINK {
                            continue;
                        }
                        if !strict_ref_resolves_map(
                            &e.oid,
                            pack,
                            odb,
                            allowed_missing,
                            allow_promisor_missing_references,
                        ) {
                            return Err(Error::CorruptObject(format!(
                                "strict: missing object {} referenced by tree",
                                e.oid.to_hex()
                            )));
                        }
                    }
                }
                ObjectKind::Commit => {
                    let c = parse_commit(data)?;
                    if !strict_ref_resolves_map(
                        &c.tree,
                        pack,
                        odb,
                        allowed_missing,
                        allow_promisor_missing_references,
                    ) {
                        return Err(Error::CorruptObject(format!(
                            "strict: missing tree {} referenced by commit",
                            c.tree.to_hex()
                        )));
                    }
                    // A commit recorded as a shallow boundary (graft) has its parents intentionally
                    // absent — skip parent connectivity for it, matching unpack-objects run with a
                    // `--shallow-file` listing this commit.
                    if shallow_boundaries.contains(oid) {
                        continue;
                    }
                    for p in &c.parents {
                        if !strict_ref_resolves_map(
                            p,
                            pack,
                            odb,
                            allowed_missing,
                            allow_promisor_missing_references,
                        ) {
                            return Err(Error::CorruptObject(format!(
                                "strict: missing parent {} referenced by commit",
                                p.to_hex()
                            )));
                        }
                    }
                }
                ObjectKind::Tag => {
                    let t = parse_tag(data)?;
                    if !strict_ref_resolves_map(
                        &t.object,
                        pack,
                        odb,
                        allowed_missing,
                        allow_promisor_missing_references,
                    ) {
                        return Err(Error::CorruptObject(format!(
                            "strict: missing object {} referenced by tag",
                            t.object.to_hex()
                        )));
                    }
                }
                ObjectKind::Blob => {}
            },
        }
    }
    Ok(())
}

fn strict_ref_resolves_map(
    oid: &ObjectId,
    pack: &HashMap<ObjectId, PackedObjectEntry>,
    odb: Option<&Odb>,
    allowed_missing: &HashSet<ObjectId>,
    allow_promisor_missing_references: bool,
) -> bool {
    pack.contains_key(oid)
        || allowed_missing.contains(oid)
        || odb.is_some_and(|o| o.exists(oid))
        || allow_promisor_missing_references
}

fn strict_ref_resolves(
    oid: &ObjectId,
    pack: &std::collections::HashMap<ObjectId, (ObjectKind, Vec<u8>)>,
    odb: Option<&Odb>,
) -> bool {
    pack.contains_key(oid) || odb.is_some_and(|o| o.exists(oid))
}

/// Verifies that references from commits, trees, and tags resolve to objects present in `pack`
/// or, when `odb` is [`Some`], to loose objects in that database.
///
/// Use [`None`] for `odb` when indexing or unpacking in a context with no repository (Git allows
/// `index-pack --strict` outside a work tree when the pack is self-contained).
pub fn strict_verify_packed_references(
    odb: Option<&Odb>,
    pack: &HashMap<ObjectId, (ObjectKind, Vec<u8>)>,
) -> Result<()> {
    for (kind, data) in pack.values() {
        match kind {
            ObjectKind::Tree => {
                for e in parse_tree(data)? {
                    // Gitlink (submodule) entries point at commits that live in
                    // the submodule repository, not this pack/ODB. Skip them in
                    // the connectivity walk, matching upstream git
                    // (git/fsck.c:374 `if (S_ISGITLINK) continue;`).
                    if e.mode == MODE_GITLINK {
                        continue;
                    }
                    if !strict_ref_resolves(&e.oid, pack, odb) {
                        return Err(Error::CorruptObject(format!(
                            "strict: missing object {} referenced by tree",
                            e.oid.to_hex()
                        )));
                    }
                }
            }
            ObjectKind::Commit => {
                let c = parse_commit(data)?;
                if !strict_ref_resolves(&c.tree, pack, odb) {
                    return Err(Error::CorruptObject(format!(
                        "strict: missing tree {} referenced by commit",
                        c.tree.to_hex()
                    )));
                }
                for p in &c.parents {
                    if !strict_ref_resolves(p, pack, odb) {
                        return Err(Error::CorruptObject(format!(
                            "strict: missing parent {} referenced by commit",
                            p.to_hex()
                        )));
                    }
                }
            }
            ObjectKind::Tag => {
                let t = parse_tag(data)?;
                if !strict_ref_resolves(&t.object, pack, odb) {
                    return Err(Error::CorruptObject(format!(
                        "strict: missing object {} referenced by tag",
                        t.object.to_hex()
                    )));
                }
            }
            ObjectKind::Blob => {}
        }
    }
    Ok(())
}

/// Whether `data` is a *thin* pack — i.e. it contains a `ref-delta` (type 7) whose base object is
/// not itself present in the pack. `git pack-objects --thin` produces such packs; a receiver that
/// rejects thin packs (`receive-pack --reject-thin-pack-for-testing`) uses this to refuse them.
///
/// Conservative: any parse error makes this return `false` (treat as non-thin) so a malformed pack
/// is handled by the normal ingestion path rather than mislabeled.
pub fn pack_is_thin(data: &[u8], algo: HashAlgo) -> bool {
    pack_is_thin_inner(data, algo).unwrap_or(false)
}

fn pack_is_thin_inner(data: &[u8], algo: HashAlgo) -> Result<bool> {
    let mut rd = PackReader::new(data);
    if rd.read_exact(4)? != b"PACK" {
        return Ok(false);
    }
    let _version = rd.read_u32_be()?;
    let nr_objects = rd.read_u32_be()? as usize;

    let mut in_pack: HashSet<ObjectId> = HashSet::new();
    let mut ref_delta_bases: Vec<ObjectId> = Vec::new();
    for _ in 0..nr_objects {
        let obj_offset = rd.pos;
        let (type_code, size) = rd.read_type_size()?;
        match type_code {
            1..=4 => {
                let kind = type_code_to_kind(type_code)?;
                let obj_data = rd.decompress(size)?;
                in_pack.insert(crate::hash::hash_object(algo, kind, &obj_data));
            }
            6 => {
                // ofs-delta: base is always in-pack (referenced by relative offset).
                let _neg = rd.read_ofs_neg_offset()?;
                let _ = obj_offset;
                let _ = rd.decompress(size)?;
            }
            7 => {
                let base_bytes = rd.read_exact(algo.len())?;
                ref_delta_bases.push(ObjectId::from_bytes(base_bytes)?);
                let _ = rd.decompress(size)?;
            }
            _ => return Ok(false),
        }
    }
    // Thin iff any ref-delta points at a base that is not packed alongside it.
    Ok(ref_delta_bases.iter().any(|b| !in_pack.contains(b)))
}

/// Parse a pack byte stream and return every resolved object (after delta resolution) keyed by OID.
///
/// Does not write to any object database. Used for receive-pack connectivity checks before
/// applying a push to the permanent ODB.
///
/// Thin-pack bases may be resolved from `odb` when they are not present in the pack.
pub fn pack_bytes_to_object_map(data: &[u8], odb: &Odb) -> Result<HashMap<ObjectId, Object>> {
    build_pack_object_map(PackReader::new(data), odb)
}

/// Resolved `(oid, pack_offset)` pairs for every object slot in a pack byte stream.
///
/// Offsets point at packed object headers (including delta slots). Thin-pack bases
/// may be resolved from `odb` when they are not present in the pack.
/// One row for a v2 pack index: resolved OID, header offset, CRC32 of raw pack bytes.
#[derive(Debug, Clone)]
pub struct PackIndexRecord {
    /// Object id (after delta resolution).
    pub oid: ObjectId,
    /// Byte offset of the object header in the pack.
    pub offset: u64,
    /// CRC32 of the packed object slice (header + zlib payload).
    pub crc32: u32,
}

/// Build index records by walking `data` once with bounded retention for delta bases.
///
/// Does not copy the pack buffer, retain every inflated object, or duplicate payloads
/// across lookup maps — only offsets still referenced by unresolved deltas are kept.
pub fn pack_index_records_from_bytes(data: &[u8], odb: &Odb) -> Result<Vec<PackIndexRecord>> {
    pack_index_records_inner(PackReader::new(data), data, odb)
}

struct PendingIndexDelta {
    offset: usize,
    crc32: u32,
    base_oid: Option<ObjectId>,
    base_offset: Option<usize>,
    delta_data: Vec<u8>,
}

#[derive(Clone)]
struct IndexBase {
    kind: ObjectKind,
    data: Vec<u8>,
}

/// Header-only scan: count how many unresolved deltas reference each in-pack base.
fn pack_index_base_ref_counts(
    data: &[u8],
    hash_bytes: usize,
) -> Result<(HashMap<usize, u32>, HashMap<ObjectId, u32>)> {
    let mut rd = PackReader::new(data);
    if rd.read_exact(4)? != b"PACK" {
        return Err(Error::CorruptObject(
            "not a pack stream: invalid signature".to_owned(),
        ));
    }
    let version = rd.read_u32_be()?;
    if version != 2 && version != 3 {
        return Err(Error::CorruptObject(format!(
            "unsupported pack version {version}"
        )));
    }
    let nr_objects = rd.read_u32_be()? as usize;
    let mut ofs_refs: HashMap<usize, u32> = HashMap::new();
    let mut oid_refs: HashMap<ObjectId, u32> = HashMap::new();
    for _ in 0..nr_objects {
        let obj_offset = rd.pos;
        let (type_code, size) = rd.read_type_size()?;
        match type_code {
            1..=4 => {
                rd.skip_object_payload(type_code, size, hash_bytes)?;
            }
            6 => {
                let neg = rd.read_ofs_neg_offset()?;
                let base_offset = obj_offset.checked_sub(neg).ok_or_else(|| {
                    Error::CorruptObject("ofs-delta base offset underflow".to_owned())
                })?;
                *ofs_refs.entry(base_offset).or_insert(0) += 1;
                let _ = rd.decompress(size)?;
            }
            7 => {
                let base_bytes = rd.read_exact(hash_bytes)?;
                let base_oid = ObjectId::from_bytes(base_bytes)?;
                *oid_refs.entry(base_oid).or_insert(0) += 1;
                let _ = rd.decompress(size)?;
            }
            other => {
                return Err(Error::CorruptObject(format!(
                    "unknown packed-object type {other}"
                )))
            }
        }
    }
    Ok((ofs_refs, oid_refs))
}

fn pack_index_records_inner(
    mut rd: PackReader<'_>,
    pack: &[u8],
    odb: &Odb,
) -> Result<Vec<PackIndexRecord>> {
    let algo = odb.hash_algo();
    let hb = algo.len();
    let (mut ofs_base_refs, mut oid_base_refs) = pack_index_base_ref_counts(pack, hb)?;

    let sig = rd.read_exact(4)?;
    if sig != b"PACK" {
        return Err(Error::CorruptObject(
            "not a pack stream: invalid signature".to_owned(),
        ));
    }
    let version = rd.read_u32_be()?;
    if version != 2 && version != 3 {
        return Err(Error::CorruptObject(format!(
            "unsupported pack version {version}"
        )));
    }
    let nr_objects = rd.read_u32_be()? as usize;

    let mut ofs_bases: HashMap<usize, IndexBase> = HashMap::new();
    let mut oid_bases: HashMap<ObjectId, IndexBase> = HashMap::new();
    let mut pending: Vec<PendingIndexDelta> = Vec::new();
    let mut records: Vec<PackIndexRecord> = Vec::with_capacity(nr_objects);

    for _ in 0..nr_objects {
        let obj_offset = rd.pos;
        let (type_code, size) = rd.read_type_size()?;
        match type_code {
            1..=4 => {
                let kind = type_code_to_kind(type_code)?;
                let obj_data = rd.decompress(size)?;
                let pack_end = rd.pos;
                let crc32 = crc32fast::hash(&pack[obj_offset..pack_end]);
                let oid = odb.hash(kind, &obj_data);
                records.push(PackIndexRecord {
                    oid,
                    offset: u64::try_from(obj_offset).unwrap_or(u64::MAX),
                    crc32,
                });
                let retain_ofs = ofs_base_refs.get(&obj_offset).copied().unwrap_or(0) > 0;
                let retain_oid = oid_base_refs.get(&oid).copied().unwrap_or(0) > 0;
                match (retain_ofs, retain_oid) {
                    (true, true) => {
                        ofs_bases.insert(
                            obj_offset,
                            IndexBase {
                                kind,
                                data: obj_data.clone(),
                            },
                        );
                        oid_bases.insert(
                            oid,
                            IndexBase {
                                kind,
                                data: obj_data,
                            },
                        );
                    }
                    (true, false) => {
                        ofs_bases.insert(
                            obj_offset,
                            IndexBase {
                                kind,
                                data: obj_data,
                            },
                        );
                    }
                    (false, true) => {
                        oid_bases.insert(
                            oid,
                            IndexBase {
                                kind,
                                data: obj_data,
                            },
                        );
                    }
                    (false, false) => {}
                }
            }
            6 => {
                let neg = rd.read_ofs_neg_offset()?;
                let base_offset = obj_offset.checked_sub(neg).ok_or_else(|| {
                    Error::CorruptObject("ofs-delta base offset underflow".to_owned())
                })?;
                let delta_data = rd.decompress(size)?;
                let pack_end = rd.pos;
                pending.push(PendingIndexDelta {
                    offset: obj_offset,
                    crc32: crc32fast::hash(&pack[obj_offset..pack_end]),
                    base_oid: None,
                    base_offset: Some(base_offset),
                    delta_data,
                });
            }
            7 => {
                let base_bytes = rd.read_exact(hb)?;
                let base_oid = ObjectId::from_bytes(base_bytes)?;
                let delta_data = rd.decompress(size)?;
                let pack_end = rd.pos;
                pending.push(PendingIndexDelta {
                    offset: obj_offset,
                    crc32: crc32fast::hash(&pack[obj_offset..pack_end]),
                    base_oid: Some(base_oid),
                    base_offset: None,
                    delta_data,
                });
            }
            other => {
                return Err(Error::CorruptObject(format!(
                    "unknown packed-object type {other}"
                )))
            }
        }
    }

    let consumed = rd.pos;
    {
        let mut hasher = ObjectHasher::new(algo);
        hasher.update(&pack[..consumed]);
        let digest = hasher.finalize();
        let trailing = rd.read_exact(hb)?;
        if digest.as_bytes() != trailing {
            return Err(Error::CorruptObject(
                "pack trailing checksum mismatch".to_owned(),
            ));
        }
    }

    let mut remaining = pending;
    loop {
        if remaining.is_empty() {
            break;
        }
        let before = remaining.len();
        let mut still_pending: Vec<PendingIndexDelta> = Vec::new();
        for delta in remaining {
            let base: Option<(ObjectKind, Cow<'_, [u8]>)> =
                if let Some(base_off) = delta.base_offset {
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
            if let Some((base_kind, base_data)) = base {
                let result = apply_delta(base_data.as_ref(), &delta.delta_data)?;
                let oid = odb.hash(base_kind, &result);
                records.push(PackIndexRecord {
                    oid,
                    offset: u64::try_from(delta.offset).unwrap_or(u64::MAX),
                    crc32: delta.crc32,
                });
                if ofs_base_refs.get(&delta.offset).copied().unwrap_or(0) > 0 {
                    ofs_bases.insert(
                        delta.offset,
                        IndexBase {
                            kind: base_kind,
                            data: result.clone(),
                        },
                    );
                }
                if oid_base_refs.get(&oid).copied().unwrap_or(0) > 0 {
                    oid_bases.insert(
                        oid,
                        IndexBase {
                            kind: base_kind,
                            data: result,
                        },
                    );
                }
                if let Some(base_off) = delta.base_offset {
                    if let Some(n) = ofs_base_refs.get_mut(&base_off) {
                        *n = n.saturating_sub(1);
                        if *n == 0 {
                            ofs_bases.remove(&base_off);
                        }
                    }
                }
                if let Some(ref base_id) = delta.base_oid {
                    if let Some(n) = oid_base_refs.get_mut(base_id) {
                        *n = n.saturating_sub(1);
                        if *n == 0 {
                            oid_bases.remove(base_id);
                        }
                    }
                }
            } else {
                still_pending.push(delta);
            }
        }
        remaining = still_pending;
        if remaining.len() == before {
            return Err(Error::CorruptObject(format!(
                "{} delta(s) could not be resolved for pack index",
                remaining.len()
            )));
        }
    }
    Ok(records)
}

fn build_pack_object_map(mut rd: PackReader<'_>, odb: &Odb) -> Result<HashMap<ObjectId, Object>> {
    let algo = odb.hash_algo();
    let sig = rd.read_exact(4)?;
    if sig != b"PACK" {
        return Err(Error::CorruptObject(
            "not a pack stream: invalid signature".to_owned(),
        ));
    }
    let version = rd.read_u32_be()?;
    if version != 2 && version != 3 {
        return Err(Error::CorruptObject(format!(
            "unsupported pack version {version}"
        )));
    }
    let nr_objects = rd.read_u32_be()? as usize;

    let mut by_offset: HashMap<usize, (ObjectKind, Vec<u8>)> = HashMap::new();
    let mut by_oid: HashMap<ObjectId, (ObjectKind, Vec<u8>)> = HashMap::new();
    let mut pending: Vec<PendingDelta> = Vec::new();

    fn base_from_pack_or_odb(
        by_oid: &HashMap<ObjectId, (ObjectKind, Vec<u8>)>,
        odb: &Odb,
        id: &ObjectId,
    ) -> Option<(ObjectKind, Vec<u8>)> {
        if let Some(e) = by_oid.get(id) {
            return Some(e.clone());
        }
        odb.read(id).ok().map(|o| (o.kind, o.data))
    }

    for _ in 0..nr_objects {
        let obj_offset = rd.pos;
        let (type_code, size) = rd.read_type_size()?;

        match type_code {
            1..=4 => {
                let kind = type_code_to_kind(type_code)?;
                let data = rd.decompress(size)?;
                let oid = odb.hash(kind, &data);
                by_offset.insert(obj_offset, (kind, data.clone()));
                by_oid.insert(oid, (kind, data));
            }
            6 => {
                let neg = rd.read_ofs_neg_offset()?;
                let base_offset = obj_offset.checked_sub(neg).ok_or_else(|| {
                    Error::CorruptObject("ofs-delta base offset underflow".to_owned())
                })?;
                let delta_data = rd.decompress(size)?;
                pending.push(PendingDelta {
                    offset: obj_offset,
                    base_oid: None,
                    base_offset: Some(base_offset),
                    delta_data,
                });
            }
            7 => {
                let base_bytes = rd.read_exact(algo.len())?;
                let base_oid = ObjectId::from_bytes(base_bytes)?;
                let delta_data = rd.decompress(size)?;
                pending.push(PendingDelta {
                    offset: obj_offset,
                    base_oid: Some(base_oid),
                    base_offset: None,
                    delta_data,
                });
            }
            other => {
                return Err(Error::CorruptObject(format!(
                    "unknown packed-object type {other}"
                )))
            }
        }
    }

    let consumed = rd.pos;
    {
        let mut hasher = ObjectHasher::new(algo);
        hasher.update(&rd.data[..consumed]);
        let digest = hasher.finalize();
        let trailing = rd.read_exact(algo.len())?;
        if digest.as_bytes() != trailing {
            return Err(Error::CorruptObject(
                "pack trailing checksum mismatch".to_owned(),
            ));
        }
    }

    let mut remaining = pending;
    loop {
        if remaining.is_empty() {
            break;
        }
        let before = remaining.len();
        let mut still_pending: Vec<PendingDelta> = Vec::new();

        for delta in remaining {
            let base = if let Some(base_off) = delta.base_offset {
                by_offset.get(&base_off).cloned()
            } else if let Some(ref base_id) = delta.base_oid {
                base_from_pack_or_odb(&by_oid, odb, base_id)
            } else {
                None
            };

            if let Some((base_kind, base_data)) = base {
                let result = apply_delta(&base_data, &delta.delta_data)?;
                let oid = odb.hash(base_kind, &result);
                by_offset.insert(delta.offset, (base_kind, result.clone()));
                by_oid.insert(oid, (base_kind, result));
            } else {
                still_pending.push(delta);
            }
        }

        remaining = still_pending;
        if remaining.len() == before {
            return Err(Error::CorruptObject(format!(
                "{} delta(s) could not be resolved",
                remaining.len()
            )));
        }
    }

    Ok(by_oid
        .into_iter()
        .map(|(oid, (kind, data))| (oid, Object::new(kind, data)))
        .collect())
}

/// Either write `data` as a loose object (if `!dry_run`) or just compute its
/// [`ObjectId`] without touching the filesystem.
fn write_or_hash(kind: ObjectKind, data: &[u8], odb: &Odb, dry_run: bool) -> Result<ObjectId> {
    if dry_run {
        Ok(odb.hash(kind, data))
    } else {
        // Always materialize into this ODB: objects reachable only via alternates must still be
        // written locally (matches git unpack-objects; t5519-push-alternates).
        odb.write_local(kind, data)
    }
}

/// Convert a pack object type code to an [`ObjectKind`].
fn type_code_to_kind(code: u8) -> Result<ObjectKind> {
    match code {
        1 => Ok(ObjectKind::Commit),
        2 => Ok(ObjectKind::Tree),
        3 => Ok(ObjectKind::Blob),
        4 => Ok(ObjectKind::Tag),
        _ => Err(Error::CorruptObject(format!(
            "type code {code} is not a regular object type"
        ))),
    }
}

/// Low-level cursor over a buffered pack byte stream (in-memory pack parsing).
struct PackReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> PackReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Skip one packed object body (used for header-only pre-scans).
    fn skip_object_payload(&mut self, type_code: u8, size: usize, hash_bytes: usize) -> Result<()> {
        match type_code {
            1..=4 | 6 | 7 => {
                if type_code == 6 {
                    let _ = self.read_ofs_neg_offset()?;
                } else if type_code == 7 {
                    let _ = self.read_exact(hash_bytes)?;
                }
                let _ = self.decompress(size)?;
                Ok(())
            }
            other => Err(Error::CorruptObject(format!(
                "unknown packed-object type {other}"
            ))),
        }
    }

    /// Read exactly `n` bytes and advance the cursor, returning a slice into
    /// the internal buffer.
    fn read_exact(&mut self, n: usize) -> Result<&[u8]> {
        if self.pos + n > self.data.len() {
            return Err(Error::CorruptObject(format!(
                "pack stream truncated: need {n} bytes at offset {}",
                self.pos
            )));
        }
        let slice = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    /// Read a single byte and advance the cursor.
    fn read_byte(&mut self) -> Result<u8> {
        if self.pos >= self.data.len() {
            return Err(Error::CorruptObject(
                "unexpected end of pack stream".to_owned(),
            ));
        }
        let b = self.data[self.pos];
        self.pos += 1;
        Ok(b)
    }

    /// Read a big-endian `u32`.
    fn read_u32_be(&mut self) -> Result<u32> {
        let bytes = self.read_exact(4)?;
        Ok(u32::from_be_bytes(bytes.try_into().map_err(|_| {
            Error::CorruptObject("u32 read failed".to_owned())
        })?))
    }

    /// Read the packed-object type + size header (variable-length big-endian
    /// encoding with the type in bits 4-6 of the first byte).
    ///
    /// Returns `(type_code, uncompressed_size)`.
    fn read_type_size(&mut self) -> Result<(u8, usize)> {
        let c = self.read_byte()?;
        let type_code = (c >> 4) & 0x7;
        let mut size = (c & 0x0f) as usize;
        let mut shift = 4u32;
        let mut cur = c;
        while cur & 0x80 != 0 {
            cur = self.read_byte()?;
            size |= ((cur & 0x7f) as usize) << shift;
            shift += 7;
        }
        Ok((type_code, size))
    }

    /// Read an `OFS_DELTA` negative-offset value.
    ///
    /// The encoding uses a big-endian variable-length integer with a +1 bias
    /// on each continuation byte, yielding values ≥ 1.
    fn read_ofs_neg_offset(&mut self) -> Result<usize> {
        let mut c = self.read_byte()?;
        let mut value = (c & 0x7f) as usize;
        while c & 0x80 != 0 {
            c = self.read_byte()?;
            value = (value + 1) << 7 | (c & 0x7f) as usize;
        }
        Ok(value)
    }

    /// Decompress zlib-compressed data starting at the current cursor position.
    ///
    /// Advances the cursor by exactly the number of compressed bytes consumed.
    /// Returns an error if the decompressed length differs from `expected_size`.
    fn decompress(&mut self, expected_size: usize) -> Result<Vec<u8>> {
        let slice = &self.data[self.pos..];
        let mut decoder = ZlibDecoder::new(slice);
        let mut out = Vec::with_capacity(expected_size);
        decoder
            .read_to_end(&mut out)
            .map_err(|e| Error::Zlib(e.to_string()))?;
        if out.len() != expected_size {
            return Err(Error::CorruptObject(format!(
                "decompressed {} bytes but expected {}",
                out.len(),
                expected_size
            )));
        }
        self.pos += decoder.total_in() as usize;
        Ok(out)
    }
}

fn io_to_corrupt_eof(e: io::Error, stream_pos: usize, context: &str) -> Error {
    if e.kind() == io::ErrorKind::UnexpectedEof {
        Error::CorruptObject(format!(
            "pack stream truncated ({context}) at offset {stream_pos}"
        ))
    } else {
        Error::Io(e)
    }
}

/// Streaming cursor over a pack file: hashes body bytes incrementally (no full-buffer read).
///
/// Raw pack bytes are either consumed as object headers (via [`Self::read_byte`]) or as zlib
/// payloads.  Zlib decoders may read ahead; overflow bytes stay in [`Self::pending`] so the next
/// object header or zlib stream starts at the correct offset.
struct StreamingPackReader<'a> {
    inner: &'a mut dyn Read,
    pack_hasher: ObjectHasher,
    stream_pos: usize,
    max_input_bytes: Option<u64>,
    /// Compressed (or other) bytes already read from `inner` and hashed but not yet consumed by
    /// the current parsing step.
    pending: Vec<u8>,
}

impl<'a> StreamingPackReader<'a> {
    fn new(inner: &'a mut dyn Read, max_input_bytes: Option<u64>, algo: HashAlgo) -> Self {
        Self {
            inner,
            pack_hasher: ObjectHasher::new(algo),
            stream_pos: 0,
            max_input_bytes,
            pending: Vec::new(),
        }
    }

    fn stream_pos(&self) -> usize {
        self.stream_pos
    }

    fn enforce_max_input(&self) -> Result<()> {
        if let Some(limit) = self.max_input_bytes {
            let pos = u64::try_from(self.stream_pos)
                .map_err(|_| Error::CorruptObject("pack stream position overflow".to_owned()))?;
            if pos > limit {
                return Err(Error::CorruptObject(
                    "pack exceeds maximum allowed size".to_owned(),
                ));
            }
        }
        Ok(())
    }

    /// Read pack-body bytes (hashed). Used for headers and non-zlib payload reads only.
    fn read_from_source(&mut self, buf: &mut [u8]) -> Result<usize> {
        let n = if !self.pending.is_empty() {
            let take = buf.len().min(self.pending.len());
            buf[..take].copy_from_slice(&self.pending[..take]);
            self.pending.drain(..take);
            take
        } else {
            self.inner.read(buf).map_err(Error::Io)?
        };
        if n > 0 {
            self.pack_hasher.update(&buf[..n]);
            self.stream_pos += n;
            self.enforce_max_input()?;
        }
        Ok(n)
    }

    fn read_byte(&mut self) -> Result<u8> {
        let mut b = [0u8; 1];
        let n = self.read_from_source(&mut b)?;
        if n == 0 {
            return Err(Error::CorruptObject(format!(
                "pack stream truncated (read byte) at offset {}",
                self.stream_pos
            )));
        }
        Ok(b[0])
    }

    fn read_exact_n(&mut self, n: usize) -> Result<Vec<u8>> {
        let mut v = vec![0u8; n];
        let mut got = 0usize;
        while got < n {
            let m = self.read_from_source(&mut v[got..n])?;
            if m == 0 {
                return Err(Error::CorruptObject(format!(
                    "pack stream truncated (read exact) at offset {}",
                    self.stream_pos
                )));
            }
            got += m;
        }
        Ok(v)
    }

    fn read_u32_be(&mut self) -> Result<u32> {
        let mut b = [0u8; 4];
        let mut got = 0usize;
        while got < 4 {
            let m = self.read_from_source(&mut b[got..4])?;
            if m == 0 {
                return Err(Error::CorruptObject(format!(
                    "pack stream truncated (read u32) at offset {}",
                    self.stream_pos
                )));
            }
            got += m;
        }
        Ok(u32::from_be_bytes(b))
    }

    fn read_type_size(&mut self) -> Result<(u8, usize)> {
        let c = self.read_byte()?;
        let type_code = (c >> 4) & 0x7;
        let mut size = (c & 0x0f) as usize;
        let mut shift = 4u32;
        let mut cur = c;
        while cur & 0x80 != 0 {
            cur = self.read_byte()?;
            size |= ((cur & 0x7f) as usize) << shift;
            shift += 7;
        }
        Ok((type_code, size))
    }

    fn read_ofs_neg_offset(&mut self) -> Result<usize> {
        let mut c = self.read_byte()?;
        let mut value = (c & 0x7f) as usize;
        while c & 0x80 != 0 {
            c = self.read_byte()?;
            value = (value + 1) << 7 | (c & 0x7f) as usize;
        }
        Ok(value)
    }

    /// Pull zlib-compressed bytes until one object inflates to `expected_size` bytes.
    ///
    /// Bytes read from `inner` into `pending` are not hashed until we know how many belong to the
    /// zlib stream (`total_in()`). Lookahead past the zlib end (including the 20-byte pack
    /// trailer) must never be fed to the pack checksum.
    ///
    /// When the pack arrives in small chunks (e.g. side-band-64k from `upload-pack`), `flate2` may
    /// return an error before the full deflate stream is in `pending`. Retry after reading more
    /// from `inner` (same idea as [`PackReader::decompress`], which sees the whole zlib at once).
    fn decompress(&mut self, expected_size: usize) -> Result<Vec<u8>> {
        // `Read::read_exact` into an empty buffer returns `Ok` immediately without touching the
        // decoder, so a 0-byte packed object would leave the zlib header in `pending` and desync
        // the pack stream (bundle / clone unpack). Always run the zlib decoder once.
        if expected_size == 0 {
            const CHUNK: usize = 64 * 1024;
            let mut scratch = [0u8; CHUNK];
            loop {
                let mut cursor = std::io::Cursor::new(self.pending.as_slice());
                let mut z = ZlibDecoder::new(&mut cursor);
                let mut sink = [0u8; 1];
                match z.read(&mut sink) {
                    Ok(0) => {
                        let consumed = z.total_in() as usize;
                        if consumed > self.pending.len() {
                            return Err(Error::CorruptObject(
                                "zlib total_in exceeds pending buffer".to_owned(),
                            ));
                        }
                        if consumed == 0 {
                            let n = self.inner.read(&mut scratch).map_err(Error::Io)?;
                            if n == 0 {
                                return Err(Error::CorruptObject(format!(
                                    "pack stream truncated (zlib) at offset {}",
                                    self.stream_pos
                                )));
                            }
                            self.pending.extend_from_slice(&scratch[..n]);
                            continue;
                        }
                        self.pack_hasher.update(&self.pending[..consumed]);
                        self.stream_pos += consumed;
                        self.pending.drain(..consumed);
                        self.enforce_max_input()?;
                        return Ok(Vec::new());
                    }
                    Ok(_) => {
                        return Err(Error::CorruptObject(
                            "0-byte packed object inflated to non-empty output".to_owned(),
                        ));
                    }
                    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                        let n = self.inner.read(&mut scratch).map_err(Error::Io)?;
                        if n == 0 {
                            return Err(Error::CorruptObject(format!(
                                "pack stream truncated (zlib) at offset {}",
                                self.stream_pos
                            )));
                        }
                        self.pending.extend_from_slice(&scratch[..n]);
                    }
                    Err(e) => return Err(Error::Zlib(e.to_string())),
                }
            }
        }

        const CHUNK: usize = 64 * 1024;
        let mut scratch = [0u8; CHUNK];

        let mut out = vec![0u8; expected_size];
        let mut z = Decompress::new(true);
        let mut out_pos = 0usize;
        let mut eof = false;
        loop {
            if self.pending.is_empty() && !eof {
                let n = self.inner.read(&mut scratch).map_err(Error::Io)?;
                if n == 0 {
                    eof = true;
                } else {
                    self.pending.extend_from_slice(&scratch[..n]);
                }
            }

            let flush = if eof && self.pending.is_empty() {
                FlushDecompress::Finish
            } else {
                FlushDecompress::None
            };

            let before_in = z.total_in();
            let before_out = z.total_out();
            let status = z
                .decompress(self.pending.as_slice(), &mut out[out_pos..], flush)
                .map_err(|e| Error::Zlib(e.to_string()))?;
            let consumed = (z.total_in() - before_in) as usize;
            if consumed > self.pending.len() {
                return Err(Error::CorruptObject(
                    "zlib consumed more than pending buffer".to_owned(),
                ));
            }
            self.pack_hasher.update(&self.pending[..consumed]);
            self.stream_pos += consumed;
            self.pending.drain(..consumed);
            self.enforce_max_input()?;
            out_pos += (z.total_out() - before_out) as usize;

            match status {
                Status::StreamEnd => {
                    if out_pos != expected_size {
                        return Err(Error::CorruptObject(format!(
                            "decompressed size mismatch: got {out_pos}, want {expected_size}"
                        )));
                    }
                    return Ok(out);
                }
                Status::Ok | Status::BufError => {
                    if consumed == 0 && !eof {
                        let n = self.inner.read(&mut scratch).map_err(Error::Io)?;
                        if n == 0 {
                            eof = true;
                        } else {
                            self.pending.extend_from_slice(&scratch[..n]);
                        }
                    } else if eof && self.pending.is_empty() && out_pos != expected_size {
                        return Err(Error::CorruptObject(format!(
                            "pack stream truncated (zlib) at offset {}",
                            self.stream_pos
                        )));
                    }
                }
            }
        }
    }

    /// Hash over all pack bytes read so far (objects only; trailer not yet read).
    fn finalize_hasher(&self) -> Vec<u8> {
        self.pack_hasher.clone().finalize().as_bytes().to_vec()
    }

    /// Trailing pack checksum (hash-width bytes); not included in [`Self::finalize_hasher`].
    fn read_trailer(&mut self) -> Result<Vec<u8>> {
        let hash_len = self.pack_hasher.algo().len();
        let mut b = vec![0u8; hash_len];
        if self.pending.len() >= hash_len {
            b.copy_from_slice(&self.pending[..hash_len]);
            self.pending.drain(..hash_len);
            self.stream_pos += hash_len;
            self.enforce_max_input()?;
            return Ok(b);
        }
        let tail = self.pending.len();
        if tail > 0 {
            b[..tail].copy_from_slice(&self.pending[..]);
            self.pending.clear();
        }
        self.inner
            .read_exact(&mut b[tail..])
            .map_err(|e| io_to_corrupt_eof(e, self.stream_pos, "trailer"))?;
        self.stream_pos += hash_len;
        self.enforce_max_input()?;
        Ok(b)
    }
}

/// Apply a git "patch delta" to `base`, producing the patched result.
///
/// The delta binary format is:
/// 1. Source size: variable-length little-endian integer (must equal
///    `base.len()`).
/// 2. Destination size: variable-length little-endian integer.
/// 3. A sequence of COPY (MSB set) and INSERT (MSB clear) instructions.
///
/// # Errors
///
/// Returns [`Error::CorruptObject`] if the delta is malformed, the source-size
/// field does not match `base.len()`, or the result length does not match the
/// declared destination size.
pub fn apply_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>> {
    let mut pos = 0usize;

    let src_size = read_delta_varint(delta, &mut pos)?;
    if src_size != base.len() {
        return Err(Error::CorruptObject(format!(
            "delta source size {src_size} != base size {}",
            base.len()
        )));
    }
    let dest_size = read_delta_varint(delta, &mut pos)?;
    let mut result = Vec::with_capacity(dest_size);

    while pos < delta.len() {
        let cmd = delta[pos];
        pos += 1;
        if cmd == 0 {
            return Err(Error::CorruptObject(
                "reserved opcode 0 in delta stream".to_owned(),
            ));
        }
        if cmd & 0x80 != 0 {
            // COPY instruction: up to 4 offset bytes (bits 0-3) and up to 3
            // size bytes (bits 4-6) are present, each controlled by a flag bit.
            let mut offset = 0usize;
            let mut size = 0usize;

            macro_rules! maybe_read_byte {
                ($flag:expr, $shift:expr, $target:expr) => {
                    if cmd & $flag != 0 {
                        let b = *delta.get(pos).ok_or_else(|| {
                            Error::CorruptObject("truncated delta COPY operand".to_owned())
                        })?;
                        pos += 1;
                        $target |= (b as usize) << $shift;
                    }
                };
            }

            maybe_read_byte!(0x01, 0, offset);
            maybe_read_byte!(0x02, 8, offset);
            maybe_read_byte!(0x04, 16, offset);
            maybe_read_byte!(0x08, 24, offset);
            maybe_read_byte!(0x10, 0, size);
            maybe_read_byte!(0x20, 8, size);
            maybe_read_byte!(0x40, 16, size);

            if size == 0 {
                size = 0x10000;
            }

            let end = offset.checked_add(size).ok_or_else(|| {
                Error::CorruptObject("delta COPY range overflows usize".to_owned())
            })?;
            let chunk = base.get(offset..end).ok_or_else(|| {
                Error::CorruptObject(format!(
                    "delta COPY [{offset},{end}) out of range (base is {} bytes)",
                    base.len()
                ))
            })?;
            result.extend_from_slice(chunk);
        } else {
            // INSERT instruction: copy the next `cmd` literal bytes verbatim.
            let n = cmd as usize;
            let chunk = delta
                .get(pos..pos + n)
                .ok_or_else(|| Error::CorruptObject("truncated delta INSERT data".to_owned()))?;
            result.extend_from_slice(chunk);
            pos += n;
        }
    }

    if result.len() != dest_size {
        return Err(Error::CorruptObject(format!(
            "delta produced {} bytes but expected {dest_size}",
            result.len()
        )));
    }

    Ok(result)
}

/// Read a variable-length little-endian integer from `data` starting at `*pos`.
///
/// Advances `*pos` past the consumed bytes.
fn read_delta_varint(data: &[u8], pos: &mut usize) -> Result<usize> {
    let mut value = 0usize;
    let mut shift = 0u32;
    loop {
        let b = *data
            .get(*pos)
            .ok_or_else(|| Error::CorruptObject("truncated delta varint".to_owned()))?;
        *pos += 1;
        value |= ((b & 0x7f) as usize) << shift;
        shift += 7;
        if b & 0x80 == 0 {
            break;
        }
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::vec_init_then_push)]
    use super::*;

    // Helper: build a minimal pack from a list of (kind, data) pairs.
    // Returns the raw pack bytes.
    fn make_pack(objects: &[(ObjectKind, &[u8])]) -> Vec<u8> {
        use flate2::write::ZlibEncoder;
        use std::io::Write;

        let mut entries: Vec<Vec<u8>> = Vec::new();
        for (kind, data) in objects {
            let type_code: u8 = match kind {
                ObjectKind::Commit => 1,
                ObjectKind::Tree => 2,
                ObjectKind::Blob => 3,
                ObjectKind::Tag => 4,
            };
            // Encode type+size header.
            let mut header = Vec::new();
            let mut size = data.len();
            let first = ((type_code & 0x7) << 4) | (size & 0x0f) as u8;
            size >>= 4;
            if size > 0 {
                header.push(first | 0x80);
                while size > 0 {
                    let b = (size & 0x7f) as u8;
                    size >>= 7;
                    header.push(if size > 0 { b | 0x80 } else { b });
                }
            } else {
                header.push(first);
            }
            // zlib-compress data.
            let mut enc = ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(data).unwrap();
            let compressed = enc.finish().unwrap();
            let mut entry = header;
            entry.extend_from_slice(&compressed);
            entries.push(entry);
        }

        // Assemble: PACK + version(2) + count + entries + SHA-1.
        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&(objects.len() as u32).to_be_bytes());
        for entry in &entries {
            pack.extend_from_slice(entry);
        }
        pack.extend_from_slice(HashAlgo::Sha1.digest(&pack).as_bytes());
        pack
    }

    #[test]
    fn test_apply_delta_simple() {
        // Build a trivial delta: insert "hello world".
        let base = b"hello";
        let mut delta = Vec::new();
        // src_size = 5
        delta.push(5u8);
        // dest_size = 11
        delta.push(11u8);
        // COPY instruction: copy base[0..5]
        // cmd = 0x80 | 0x01 (offset present, byte 0) | 0x10 (size byte 0)
        delta.push(0x80 | 0x01 | 0x10); // 0x91
        delta.push(0u8); // offset = 0
        delta.push(5u8); // size = 5
                         // INSERT " world" (6 bytes)
        delta.push(6u8);
        delta.extend_from_slice(b" world");

        let result = apply_delta(base, &delta).unwrap();
        assert_eq!(result, b"hello world");
    }

    #[test]
    fn test_apply_delta_insert_only() {
        let base = b"";
        let mut delta = Vec::new();
        delta.push(0u8); // src_size = 0
        delta.push(5u8); // dest_size = 5
        delta.push(5u8); // INSERT 5 bytes
        delta.extend_from_slice(b"hello");

        let result = apply_delta(base, &delta).unwrap();
        assert_eq!(result, b"hello");
    }

    #[test]
    fn test_apply_delta_copy_only() {
        let base = b"abcdef";
        let mut delta = Vec::new();
        delta.push(6u8); // src_size = 6
        delta.push(3u8); // dest_size = 3
                         // COPY base[2..5]: offset=2, size=3
                         // cmd = 0x80 | 0x01 | 0x10
        delta.push(0x91u8);
        delta.push(2u8); // offset = 2
        delta.push(3u8); // size = 3

        let result = apply_delta(base, &delta).unwrap();
        assert_eq!(result, b"cde");
    }

    #[test]
    fn test_apply_delta_size_zero_means_65536() {
        // A COPY with size bytes all zero means 0x10000 = 65536.
        let base = vec![0xABu8; 65536];
        let mut delta = Vec::new();
        // src_size = 65536, encoded as 3 bytes little-endian varint
        delta.push(0x80 | (65536 & 0x7f) as u8); // 0
        delta.push(0x80 | ((65536 >> 7) & 0x7f) as u8); // 0x80
        delta.push(((65536 >> 14) & 0x7f) as u8); // 4
                                                  // dest_size = 65536, same
        delta.push(0x80 | (65536 & 0x7f) as u8);
        delta.push(0x80 | ((65536 >> 7) & 0x7f) as u8);
        delta.push(((65536 >> 14) & 0x7f) as u8);
        // COPY: offset=0 (no offset bytes), size=0 (no size bytes) → means 0x10000
        // cmd = 0x80 (no offset/size bytes present at all → offset=0, size=0→65536)
        delta.push(0x80u8);

        let result = apply_delta(&base, &delta).unwrap();
        assert_eq!(result.len(), 65536);
        assert!(result.iter().all(|&b| b == 0xAB));
    }

    #[test]
    fn test_unpack_objects_blobs() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let objects_dir = tmp.path().join("objects");
        std::fs::create_dir_all(&objects_dir).unwrap();
        let odb = Odb::new(&objects_dir);

        let pack = make_pack(&[
            (ObjectKind::Blob, b"hello\n"),
            (ObjectKind::Blob, b"world\n"),
        ]);

        let opts = UnpackOptions::default();
        let count = unpack_objects(&mut pack.as_slice(), &odb, &opts).unwrap();
        assert_eq!(count, 2);

        // Verify both blobs can be read back.
        let oid1 = Odb::hash_object_data(ObjectKind::Blob, b"hello\n");
        let oid2 = Odb::hash_object_data(ObjectKind::Blob, b"world\n");
        let obj1 = odb.read(&oid1).unwrap();
        let obj2 = odb.read(&oid2).unwrap();
        assert_eq!(obj1.data, b"hello\n");
        assert_eq!(obj2.data, b"world\n");
    }

    #[test]
    fn test_unpack_objects_empty_tree() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let objects_dir = tmp.path().join("objects");
        std::fs::create_dir_all(&objects_dir).unwrap();
        let odb = Odb::new(&objects_dir);

        let pack = make_pack(&[(ObjectKind::Tree, b"")]);
        let opts = UnpackOptions::default();
        assert_eq!(
            unpack_objects(&mut pack.as_slice(), &odb, &opts).unwrap(),
            1
        );
        let oid = Odb::hash_object_data(ObjectKind::Tree, b"");
        assert!(odb.exists(&oid));
        let loose = objects_dir
            .join(oid.loose_prefix())
            .join(oid.loose_suffix());
        assert!(
            loose.is_file(),
            "empty tree must be materialized as a loose object during unpack"
        );
    }

    #[test]
    fn test_strict_skips_gitlink_tree_entries() {
        use crate::index::{MODE_GITLINK, MODE_REGULAR};
        use crate::objects::{serialize_tree, TreeEntry};

        // A submodule commit oid that is NOT in the pack/ODB (lives in the
        // submodule repository, like a 160000 gitlink target on push).
        let submodule_oid = ObjectId::from_hex(&"7f".repeat(20)).unwrap();

        // Superproject tree referencing the submodule via a gitlink entry.
        let tree_data = serialize_tree(&[TreeEntry {
            mode: MODE_GITLINK,
            name: b"sub".to_vec(),
            oid: submodule_oid,
        }]);
        let tree_oid = Odb::hash_object_data(ObjectKind::Tree, &tree_data);

        // Strict connectivity must NOT flag the gitlink target as missing,
        // matching upstream git (git/fsck.c skips S_ISGITLINK entries).
        let mut pack = HashMap::new();
        pack.insert(tree_oid, (ObjectKind::Tree, tree_data.clone()));
        assert!(strict_verify_packed_references(None, &pack).is_ok());

        // Regression guard: a non-gitlink (regular file) entry pointing at an
        // absent blob must still be reported as a strict connectivity error.
        let bad_tree = serialize_tree(&[TreeEntry {
            mode: MODE_REGULAR,
            name: b"file".to_vec(),
            oid: ObjectId::from_hex(&"ab".repeat(20)).unwrap(),
        }]);
        let bad_oid = Odb::hash_object_data(ObjectKind::Tree, &bad_tree);
        let mut bad_pack = HashMap::new();
        bad_pack.insert(bad_oid, (ObjectKind::Tree, bad_tree));
        assert!(matches!(
            strict_verify_packed_references(None, &bad_pack),
            Err(Error::CorruptObject(_))
        ));
    }

    /// `Read` that returns at most `max_len` bytes per call (simulates side-band chunking).
    struct ChunkedReader<'a> {
        data: &'a [u8],
        pos: usize,
        max_len: usize,
    }

    impl io::Read for ChunkedReader<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pos >= self.data.len() {
                return Ok(0);
            }
            let take = (self.data.len() - self.pos)
                .min(self.max_len)
                .min(buf.len());
            buf[..take].copy_from_slice(&self.data[self.pos..self.pos + take]);
            self.pos += take;
            Ok(take)
        }
    }

    #[test]
    fn test_unpack_objects_chunked_read_matches_full_buffer() {
        use tempfile::TempDir;
        let pack = make_pack(&[(ObjectKind::Blob, b"chunked-stream")]);
        let opts = UnpackOptions::default();
        let oid = Odb::hash_object_data(ObjectKind::Blob, b"chunked-stream");

        let tmp = TempDir::new().unwrap();
        let objects_dir = tmp.path().join("objects");
        std::fs::create_dir_all(&objects_dir).unwrap();
        let odb = Odb::new(&objects_dir);
        assert_eq!(
            unpack_objects(&mut pack.as_slice(), &odb, &opts).unwrap(),
            1
        );
        assert!(odb.exists(&oid));

        let tmp2 = TempDir::new().unwrap();
        let objects_dir2 = tmp2.path().join("objects");
        std::fs::create_dir_all(&objects_dir2).unwrap();
        let odb2 = Odb::new(&objects_dir2);
        let mut chunked = ChunkedReader {
            data: pack.as_slice(),
            pos: 0,
            max_len: 8,
        };
        assert_eq!(unpack_objects(&mut chunked, &odb2, &opts).unwrap(), 1);
        assert!(odb2.exists(&oid));
    }

    #[test]
    fn test_unpack_objects_dry_run_writes_nothing() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let objects_dir = tmp.path().join("objects");
        std::fs::create_dir_all(&objects_dir).unwrap();
        let odb = Odb::new(&objects_dir);

        let pack = make_pack(&[(ObjectKind::Blob, b"test content")]);

        let opts = UnpackOptions {
            dry_run: true,
            quiet: true,
            strict: false,
            allowed_missing: Default::default(),
            allow_promisor_missing_references: false,
            max_input_bytes: None,
            ..Default::default()
        };
        let count = unpack_objects(&mut pack.as_slice(), &odb, &opts).unwrap();
        assert_eq!(count, 1);

        // Nothing should be written.
        let oid = Odb::hash_object_data(ObjectKind::Blob, b"test content");
        assert!(!odb.exists(&oid));
    }

    #[test]
    fn test_unpack_objects_bad_signature() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let objects_dir = tmp.path().join("objects");
        std::fs::create_dir_all(&objects_dir).unwrap();
        let odb = Odb::new(&objects_dir);

        let mut bad = b"NOPE\x00\x00\x00\x02\x00\x00\x00\x00".to_vec();
        bad.extend_from_slice(&[0u8; 20]);
        let opts = UnpackOptions::default();
        let err = unpack_objects(&mut bad.as_slice(), &odb, &opts).unwrap_err();
        assert!(err.to_string().contains("invalid signature"));
    }

    #[test]
    fn test_unpack_objects_checksum_mismatch() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let objects_dir = tmp.path().join("objects");
        std::fs::create_dir_all(&objects_dir).unwrap();
        let odb = Odb::new(&objects_dir);

        let mut pack = make_pack(&[(ObjectKind::Blob, b"data")]);
        // Corrupt the trailing checksum.
        let n = pack.len();
        pack[n - 1] ^= 0xFF;

        let opts = UnpackOptions::default();
        let err = unpack_objects(&mut pack.as_slice(), &odb, &opts).unwrap_err();
        assert!(err.to_string().contains("checksum"));
    }

    #[test]
    fn test_apply_delta_source_size_mismatch() {
        let base = b"hi";
        let delta = [3u8, 2u8, 2u8, b'h', b'i']; // src_size=3 != base.len()=2
        let err = apply_delta(base, &delta).unwrap_err();
        assert!(err.to_string().contains("source size"));
    }

    #[test]
    fn pack_index_many_blobs_without_retaining_all_payloads() {
        const N: usize = 1500;
        let blobs: Vec<(ObjectKind, Vec<u8>)> = (0..N)
            .map(|i| (ObjectKind::Blob, format!("blob-{i}\n").into_bytes()))
            .collect();
        let blob_refs: Vec<(ObjectKind, &[u8])> =
            blobs.iter().map(|(k, d)| (*k, d.as_slice())).collect();
        let pack = make_pack(&blob_refs);
        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(tmp.path());
        let started = std::time::Instant::now();
        let records = pack_index_records_from_bytes(&pack, &odb).expect("index records");
        let elapsed = started.elapsed();
        assert_eq!(records.len(), N, "one index row per object");
        assert!(
            elapsed.as_secs() < 30,
            "indexing {N} blobs took {:?}; expected linear scan without full retention blow-up",
            elapsed
        );
    }
}
