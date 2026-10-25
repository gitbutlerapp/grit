//! Pack object verification for repository fsck.
//!
//! Every object is inflated once and hashed against the pack index while the pack's
//! delta trees are walked depth-first on worker threads (see [`crate::pack_delta_walk`]).
//! Each verified object's bytes go to a caller-supplied check and are dropped right
//! after, so memory follows the delta chains being walked and the checks' compact
//! results, not the uncompressed size of the pack.

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{Error, Result};
use crate::hash::hash_object;
use crate::objects::{HashAlgo, ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::pack::{
    parse_ofs_delta_base, parse_pack_object_header, PackedType, VerifiedPackForFsck,
    VerifyObjectRecord,
};
use crate::pack_delta_walk::{
    DeltaBase, DeltaRoot, DeltaVisitor, DeltaWalk, PendingDelta, DELTA_CHAIN_BYTES_PER_WORKER,
};

/// Per-object result from pack verification.
#[derive(Debug)]
pub(crate) enum PackFsckObjectOutcome<T> {
    /// Object inflated and hash-checked; `facts` is what the caller's check returned.
    Clean {
        /// Index object id for this pack slot.
        oid: ObjectId,
        /// Result of the caller's check on the object's bytes.
        facts: T,
    },
    /// One object could not be read or verified; other objects in the pack may still be checked.
    Failed {
        /// Index object id for this pack slot.
        oid: ObjectId,
        /// Underlying read/hash/decompress error.
        err: Error,
    },
}

/// Outcomes tagged with the pack offset they belong to, so they can be put in pack order.
type Outcomes<T> = Vec<(usize, PackFsckObjectOutcome<T>)>;

/// Hash-checks each object against the index and hands verified bytes to `check`.
struct FsckVisitor<'a, F> {
    algo: HashAlgo,
    /// Index object id of each pending delta.
    delta_oids: &'a [ObjectId],
    pending: &'a [PendingDelta],
    check: &'a F,
    cancel: &'a AtomicBool,
}

impl<T, F> FsckVisitor<'_, F>
where
    F: Fn(ObjectId, ObjectKind, &[u8]) -> T + Sync,
{
    /// Verify one object and record its outcome; true when it hashed to `oid`.
    fn visit(
        &self,
        offset: usize,
        oid: ObjectId,
        kind: ObjectKind,
        data: &[u8],
        out: &mut Outcomes<T>,
    ) -> Result<bool> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let outcome = match hash_object(self.algo, kind, data) {
            Ok(actual) if actual == oid => PackFsckObjectOutcome::Clean {
                oid,
                facts: (self.check)(oid, kind, data),
            },
            Ok(actual) => PackFsckObjectOutcome::Failed {
                oid,
                err: Error::CorruptObject(format!(
                    "packed object {} hashes to {}",
                    oid.to_hex(),
                    actual.to_hex()
                )),
            },
            Err(err) => PackFsckObjectOutcome::Failed { oid, err },
        };
        let clean = matches!(outcome, PackFsckObjectOutcome::Clean { .. });
        out.push((offset, outcome));
        Ok(clean)
    }
}

impl<T, F> DeltaVisitor for FsckVisitor<'_, F>
where
    T: Send,
    F: Fn(ObjectId, ObjectKind, &[u8]) -> T + Sync,
{
    type Out = Outcomes<T>;

    fn root(
        &self,
        root: &DeltaRoot,
        kind: ObjectKind,
        data: &[u8],
        out: &mut Self::Out,
    ) -> Result<bool> {
        match *root {
            DeltaRoot::Pack { offset, oid } => self.visit(offset, oid, kind, data, out),
            // Bases from other packs or loose objects are checked where they live.
            DeltaRoot::Odb(_) => Ok(true),
        }
    }

    fn root_failed(&self, root: &DeltaRoot, err: Error, out: &mut Self::Out) -> Result<()> {
        if let DeltaRoot::Pack { offset, oid } = *root {
            out.push((offset, PackFsckObjectOutcome::Failed { oid, err }));
        }
        Ok(())
    }

    fn delta(
        &self,
        index: u32,
        kind: ObjectKind,
        data: &[u8],
        out: &mut Self::Out,
    ) -> Result<Option<ObjectId>> {
        let oid = self.delta_oids[index as usize];
        let offset = self.pending[index as usize].offset;
        Ok(self.visit(offset, oid, kind, data, out)?.then_some(oid))
    }

    fn delta_failed(&self, index: u32, err: Error, out: &mut Self::Out) -> Result<()> {
        let oid = self.delta_oids[index as usize];
        let offset = self.pending[index as usize].offset;
        out.push((offset, PackFsckObjectOutcome::Failed { oid, err }));
        Ok(())
    }

    fn merge(into: &mut Self::Out, mut from: Self::Out) {
        into.append(&mut from);
    }
}

/// Locate the zlib stream of the delta in `record` and describe where its base lives.
fn pending_delta(
    pack_bytes: &[u8],
    record: &VerifyObjectRecord,
    hash_bytes: usize,
) -> Result<PendingDelta> {
    let offset = record.offset as usize;
    let mut pos = offset;
    let (packed_type, size) = parse_pack_object_header(pack_bytes, &mut pos)?;
    let base = match packed_type {
        PackedType::RefDelta => {
            let raw = pack_bytes.get(pos..pos + hash_bytes).ok_or_else(|| {
                Error::CorruptObject(format!("truncated ref-delta base at offset {offset}"))
            })?;
            pos += hash_bytes;
            DeltaBase::Oid(ObjectId::from_bytes(raw)?)
        }
        PackedType::OfsDelta => {
            let base = parse_ofs_delta_base(pack_bytes, &mut pos, record.offset)?;
            DeltaBase::Offset(base as usize)
        }
        PackedType::Commit | PackedType::Tree | PackedType::Blob | PackedType::Tag => {
            return Err(Error::CorruptObject(format!(
                "expected a delta at offset {offset}"
            )));
        }
    };
    let uncompressed_size = usize::try_from(size).map_err(|_| {
        Error::CorruptObject(format!("pack object size overflow at offset {offset}"))
    })?;
    Ok(PendingDelta {
        offset,
        base,
        zlib_offset: pos,
        uncompressed_size,
    })
}

/// Verify every object in `prep`, passing each verified object's bytes to `check`.
///
/// `check` runs on up to `threads` worker threads, once per object whose bytes hash to
/// its index id; the bytes are dropped when it returns. Deltas whose base cannot be
/// resolved (missing, corrupt, or failed its own hash check) are reported as failed.
/// Returns one outcome per index entry, in pack offset order; failures on individual
/// objects do not stop the scan.
///
/// # Errors
///
/// Returns [`Error::Cancelled`] when `cancel` is set during the walk, or an error for
/// pack structure problems found before per-object work begins.
pub(crate) fn verify_pack_objects<T, F>(
    prep: VerifiedPackForFsck,
    odb: &Odb,
    threads: NonZeroUsize,
    cancel: &AtomicBool,
    check: F,
) -> Result<impl Iterator<Item = PackFsckObjectOutcome<T>>>
where
    T: Send,
    F: Fn(ObjectId, ObjectKind, &[u8]) -> T + Sync,
{
    let VerifiedPackForFsck {
        pack_bytes,
        records,
        ..
    } = prep;
    let algo = odb.hash_algo();
    let hash_bytes = algo.len();
    let mut roots = Vec::new();
    let mut pending = Vec::new();
    let mut delta_oids = Vec::new();
    for record in &records {
        let oid = ObjectId::from_bytes(&record.oid)?;
        match record.packed_type {
            PackedType::Commit | PackedType::Tree | PackedType::Blob | PackedType::Tag => {
                roots.push(DeltaRoot::Pack {
                    offset: record.offset as usize,
                    oid,
                });
            }
            PackedType::OfsDelta | PackedType::RefDelta => {
                pending.push(pending_delta(&pack_bytes, record, hash_bytes)?);
                delta_oids.push(oid);
            }
        }
    }
    drop(records);

    let visitor = FsckVisitor {
        algo,
        delta_oids: &delta_oids,
        pending: &pending,
        check: &check,
        cancel,
    };
    let walk = DeltaWalk::new(
        &pack_bytes,
        odb,
        &pending,
        DELTA_CHAIN_BYTES_PER_WORKER,
        &visitor,
    )?;
    let mut outcomes = Outcomes::with_capacity(roots.len() + pending.len());
    walk.run(&roots, threads, &mut outcomes, &mut |_| {})?;
    walk.run(&walk.thin_roots(), threads, &mut outcomes, &mut |_| {})?;
    for index in walk.unclaimed() {
        let oid = delta_oids[index as usize];
        outcomes.push((
            pending[index as usize].offset,
            PackFsckObjectOutcome::Failed {
                oid,
                err: Error::CorruptObject(format!(
                    "delta base for {} could not be resolved during pack fsck",
                    oid.to_hex()
                )),
            },
        ));
    }
    outcomes.sort_unstable_by_key(|(offset, _)| *offset);
    Ok(outcomes.into_iter().map(|(_, outcome)| outcome))
}
