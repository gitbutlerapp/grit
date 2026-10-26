//! Build v2 pack index records from an in-memory pack (index-pack hashing path).
//!
//! Multi-threaded builds overlap zlib-skip (boundary scan) with parallel inflate+hash
//! on batched whole objects. Single-threaded builds inflate and hash each whole object
//! once in pack order.
//!
//! Deltas are resolved depth-first from each base that has delta children (see
//! [`crate::pack_delta_walk`]), so peak memory follows the deepest chain rather than the
//! number of deltas waiting on resolved bases.
//!
//! When the pack is memory-mapped, the sequential passes (trailer hash and object scan)
//! charge what they read to a [`ResidentBudget`] that drops pages behind them, and delta
//! resolution reads each compressed object with a positioned read from the file instead
//! of through the mapping. The mapping's resident pages therefore stay bounded instead
//! of growing to the pack's size.

use crate::error::{Error, Result};
use crate::hash::{hash_object, Parallelism};
use crate::objects::{HashAlgo, ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::pack_delta_walk::{
    record_first_error, DeltaBase, DeltaChildren, DeltaRoot, DeltaVisitor, DeltaWalk, PackBytes,
    PendingDelta, DELTA_CHAIN_BYTES_PER_WORKER,
};
use crate::pack_map::{PackData, ResidentBudget};
use crate::pack_zlib::skip_zlib_at_with_scratch;
use crate::progress::{
    cancel_watch_arc, poll_cancel, poll_watch, Cancel, ProgressPhase, ProgressSink,
};
use crate::unpack_objects::{type_code_to_kind, PackIndexRecord, PackReader};
use crate::zlib_inflate::ZlibInflateScratch;
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

/// Poll the coordinating thread every N object slots / resolved deltas.
const PROGRESS_POLL_EVERY: u64 = 32;

/// Poll cancel handles during index-pack (main thread and worker loops).
fn poll_index_build_cancel(
    cancel: Option<&dyn Cancel>,
    progress: Option<&mut dyn ProgressSink>,
    watch: Option<&Arc<std::sync::atomic::AtomicBool>>,
) -> Result<()> {
    if let Some(p) = progress {
        if let Some(c) = cancel {
            poll_cancel(p, c)?;
        }
    } else if let Some(c) = cancel {
        if c.is_cancelled() {
            return Err(Error::Cancelled);
        }
    }
    if let Some(w) = watch {
        poll_watch(w)?;
    }
    Ok(())
}

/// Coordinates pack-index progress on the caller thread (workers bump atomics only).
struct PackIndexReporter<'a> {
    sink: &'a mut dyn ProgressSink,
    object_total: u64,
    indexed: AtomicU64,
    resolved: u64,
    last_index_reported: u64,
    last_delta_reported: u64,
}

impl<'a> PackIndexReporter<'a> {
    fn begin_indexing(sink: &'a mut dyn ProgressSink, object_total: u64) -> Self {
        sink.start(ProgressPhase::IndexingObjects, Some(object_total));
        Self {
            sink,
            object_total,
            indexed: AtomicU64::new(0),
            resolved: 0,
            last_index_reported: 0,
            last_delta_reported: 0,
        }
    }

    fn on_object_slot(&mut self) {
        self.indexed.fetch_add(1, Ordering::Relaxed);
        self.poll_index();
    }

    fn poll_index(&mut self) {
        let cur = self.indexed.load(Ordering::Relaxed);
        if cur == self.object_total
            || cur.saturating_sub(self.last_index_reported) >= PROGRESS_POLL_EVERY
        {
            self.sink.set(cur);
            self.last_index_reported = cur;
        }
    }

    fn finish_indexing(&mut self) {
        let cur = self.indexed.load(Ordering::Relaxed);
        self.sink.set(cur);
        self.sink.finish();
    }

    fn begin_deltas(&mut self, delta_total: u64) {
        self.resolved = 0;
        self.last_delta_reported = 0;
        self.sink
            .start(ProgressPhase::ResolvingDeltas, Some(delta_total));
    }

    fn set_deltas_resolved(&mut self, total: u64) {
        self.resolved = total;
        if total.saturating_sub(self.last_delta_reported) >= PROGRESS_POLL_EVERY {
            self.sink.set(total);
            self.last_delta_reported = total;
        }
    }

    fn finish_deltas(&mut self) {
        self.sink.set(self.resolved);
        self.sink.finish();
    }
}

/// Deltas found by the pack scan, each indexed under its base, plus where each whole
/// object's zlib stream ends (aligned with the whole-object records, in pack order).
#[derive(Default)]
struct PendingDeltas {
    deltas: Vec<PendingDelta>,
    /// CRC-32 of each delta's packed slot, aligned with `deltas`.
    crc32: Vec<u32>,
    children: DeltaChildren,
    whole_ends: Vec<usize>,
}

impl PendingDeltas {
    fn push(
        &mut self,
        pack: &[u8],
        offset: usize,
        zlib_offset: usize,
        pack_end: usize,
        uncompressed_size: usize,
        base: DeltaBase,
    ) -> Result<()> {
        let index = u32::try_from(self.deltas.len())
            .map_err(|_| Error::CorruptObject("too many deltas in pack".to_owned()))?;
        self.deltas.push(PendingDelta::new(
            offset,
            zlib_offset,
            pack_end,
            uncompressed_size,
        )?);
        self.crc32.push(crc32fast::hash(&pack[offset..pack_end]));
        self.children.push(base, index);
        Ok(())
    }
}

struct WholeSlot {
    offset: usize,
    pack_end: usize,
    zlib_offset: usize,
    kind: ObjectKind,
    uncompressed_size: usize,
}

/// Hash oversized whole objects outside the parallel pool to cap per-thread memory.
const LARGE_WHOLE_OBJECT: usize = 4 * 1024 * 1024;
/// Soft cap on total uncompressed bytes being hashed by whole-object workers at once.
const WHOLE_INFLIGHT_BYTES_PER_THREAD: usize = 8 * 1024 * 1024;
/// Bytes of a memory-mapped pack the sequential passes of an index build keep resident.
pub(crate) const RESIDENT_PACK_BYTES: usize = 2 * 1024 * 1024;

struct PackHeader {
    nr_objects: usize,
}

/// Records for every object in a pack, plus the thin-pack bases it was resolved against.
pub(crate) struct PackIndexBuild {
    /// One record per object slot, in no particular order.
    pub(crate) records: Vec<PackIndexRecord>,
    /// Ref-delta bases read from the object database because the pack lacks them,
    /// sorted by object id. Empty unless the pack is thin.
    pub(crate) thin_bases: Vec<ObjectId>,
}

/// Index a memory-mapped pack, keeping at most about [`RESIDENT_PACK_BYTES`] of it resident.
///
/// `file` is the pack file behind `pack`; delta resolution reads from it directly. Same
/// contract as [`build_pack_index_records_with_progress`], but also reports the
/// thin-pack bases taken from `odb` so the caller can complete the pack.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_mapped_pack_index(
    pack: &PackData,
    file: &std::fs::File,
    odb: &Odb,
    parallelism: Parallelism,
    verify_trailer: bool,
    progress: Option<&mut dyn ProgressSink>,
    cancel: Option<&dyn Cancel>,
) -> Result<PackIndexBuild> {
    build_records(
        pack,
        odb,
        parallelism,
        verify_trailer,
        progress,
        cancel,
        DELTA_CHAIN_BYTES_PER_WORKER,
        &ResidentBudget::new(pack, RESIDENT_PACK_BYTES),
        PackBytes::File(file),
    )
}

/// Build index records for `pack`, using up to `parallelism` worker threads.
///
/// Returns one record per object slot, in no particular order. Every delta is applied
/// exactly once; besides the records, memory is bounded by the delta chain each worker
/// is walking (capped per worker, with older ancestors rebuilt from the pack on demand),
/// not by the number of deltas in the pack. Ref-delta bases missing from the pack are
/// read from `odb` (thin packs).
///
/// When `verify_trailer` is false, skips the full-pack SHA-1 trailer check during
/// the object scan (used when the caller already validated the stream on the wire).
/// The last object must still end exactly where the trailer starts.
///
/// # Errors
///
/// Returns [`Error::CorruptObject`] for a malformed pack or a bad trailer (when
/// `verify_trailer`), and [`Error::UnresolvedDeltas`] for deltas whose bases are in
/// neither the pack nor `odb`.
pub fn build_pack_index_records(
    pack: &[u8],
    odb: &Odb,
    parallelism: Parallelism,
    verify_trailer: bool,
) -> Result<Vec<PackIndexRecord>> {
    build_pack_index_records_with_progress(pack, odb, parallelism, verify_trailer, None, None)
}

/// Like [`build_pack_index_records`], emitting [`ProgressPhase::IndexingObjects`] then
/// [`ProgressPhase::ResolvingDeltas`] on `progress` when provided.
pub fn build_pack_index_records_with_progress(
    pack: &[u8],
    odb: &Odb,
    parallelism: Parallelism,
    verify_trailer: bool,
    progress: Option<&mut dyn ProgressSink>,
    cancel: Option<&dyn Cancel>,
) -> Result<Vec<PackIndexRecord>> {
    build_records(
        pack,
        odb,
        parallelism,
        verify_trailer,
        progress,
        cancel,
        DELTA_CHAIN_BYTES_PER_WORKER,
        &ResidentBudget::unbounded(),
        PackBytes::Slice(pack),
    )
    .map(|build| build.records)
}

/// Index `pack` with an explicit per-worker chain byte cap, charging the sequential
/// passes to `resident` and resolving deltas from `source`.
#[allow(clippy::too_many_arguments)]
fn build_records(
    pack: &[u8],
    odb: &Odb,
    parallelism: Parallelism,
    verify_trailer: bool,
    progress: Option<&mut dyn ProgressSink>,
    cancel: Option<&dyn Cancel>,
    chain_limit: usize,
    resident: &ResidentBudget<'_>,
    source: PackBytes<'_>,
) -> Result<PackIndexBuild> {
    crate::trace::trace_span!("grit.pack.delta.resolve");
    let cancel_watch = cancel.map(cancel_watch_arc);
    poll_index_build_cancel(cancel, None, cancel_watch.as_ref())?;
    let algo = odb.hash_algo();
    let threads = parallelism.threads();
    let header = read_pack_header(pack)?;
    let object_total = u64::try_from(header.nr_objects).unwrap_or(u64::MAX);
    let mut reporter = progress.map(|sink| PackIndexReporter::begin_indexing(sink, object_total));
    let mut pending = PendingDeltas::default();
    let mut records: Vec<PackIndexRecord> = Vec::with_capacity(header.nr_objects);

    if threads.get() <= 1 {
        scan_and_hash_wholes_serial(
            pack,
            &header,
            &mut pending,
            &mut records,
            algo,
            verify_trailer,
            reporter.as_mut(),
            cancel,
            cancel_watch.as_ref(),
            resident,
        )?;
    } else {
        scan_and_hash_whole_slots_overlapped(
            pack,
            &header,
            &mut pending,
            &mut records,
            algo,
            verify_trailer,
            threads,
            reporter.as_mut(),
            cancel_watch.clone(),
            resident,
        )?;
    }
    poll_index_build_cancel(cancel, None, cancel_watch.as_ref())?;

    pending.children.sort();
    let delta_total = u64::try_from(pending.deltas.len()).unwrap_or(u64::MAX);
    if let Some(r) = reporter.as_mut() {
        r.finish_indexing();
        if delta_total > 0 {
            r.begin_deltas(delta_total);
        }
    }

    let thin_bases = resolve_deltas(
        source,
        odb,
        algo,
        threads,
        &pending,
        &mut records,
        reporter.as_mut(),
        chain_limit,
        cancel,
        cancel_watch.as_ref(),
    )?;

    if let Some(r) = reporter.as_mut() {
        if delta_total > 0 {
            r.finish_deltas();
        }
    }
    Ok(PackIndexBuild {
        records,
        thin_bases,
    })
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
    Ok(PackHeader { nr_objects })
}

/// Single-threaded pack walk: one inflate+hash per whole object; zlib-skip deltas only.
#[allow(clippy::too_many_arguments)]
fn scan_and_hash_wholes_serial(
    pack: &[u8],
    header: &PackHeader,
    pending: &mut PendingDeltas,
    records: &mut Vec<PackIndexRecord>,
    algo: HashAlgo,
    verify_trailer: bool,
    mut progress: Option<&mut PackIndexReporter<'_>>,
    cancel: Option<&dyn Cancel>,
    cancel_watch: Option<&Arc<AtomicBool>>,
    resident: &ResidentBudget<'_>,
) -> Result<()> {
    let hb = algo.len();
    let mut skip_scratch = ZlibInflateScratch::default();
    let mut hash_scratch = ZlibInflateScratch::default();
    let mut rd = PackReader::new(pack);
    rd.read_exact(4)?;
    rd.read_u32_be()?;
    rd.read_u32_be()?;

    for slot in 0..header.nr_objects {
        if slot % 64 == 0 {
            poll_index_build_cancel(cancel, None, cancel_watch)?;
        }
        let obj_offset = rd.pos;
        let (type_code, size) = rd.read_type_size()?;
        match type_code {
            1..=4 => {
                let kind = type_code_to_kind(type_code)?;
                let zlib_offset = rd.pos;
                let consumed =
                    skip_zlib_at_with_scratch(&mut skip_scratch, pack, zlib_offset, size)?;
                let pack_end = zlib_offset + consumed;
                rd.pos = pack_end;
                let work = WholeSlot {
                    offset: obj_offset,
                    pack_end,
                    zlib_offset,
                    kind,
                    uncompressed_size: size,
                };
                records.push(hash_whole_slot(pack, &work, algo, &mut hash_scratch)?);
                pending.whole_ends.push(pack_end);
            }
            6 => push_ofs_delta(pack, &mut rd, obj_offset, size, pending, &mut skip_scratch)?,
            7 => push_ref_delta(
                pack,
                &mut rd,
                obj_offset,
                size,
                hb,
                pending,
                &mut skip_scratch,
            )?,
            other => {
                return Err(Error::CorruptObject(format!(
                    "unknown packed-object type {other}"
                )));
            }
        }
        resident.charge(rd.pos - obj_offset);
        if let Some(r) = progress.as_mut() {
            r.on_object_slot();
        }
    }

    verify_pack_trailer(pack, rd.pos, algo, verify_trailer.then_some(resident))
}

struct WholeHashJob {
    index: usize,
    slot: WholeSlot,
}

struct WholeHashQueue {
    jobs: Mutex<VecDeque<WholeHashJob>>,
    jobs_added: Condvar,
    scan_done: AtomicBool,
    inflight_bytes: AtomicUsize,
    inflight_limit: usize,
    inflight_room: Condvar,
}

impl WholeHashQueue {
    fn new(inflight_limit: usize) -> Self {
        Self {
            jobs: Mutex::new(VecDeque::new()),
            jobs_added: Condvar::new(),
            scan_done: AtomicBool::new(false),
            inflight_bytes: AtomicUsize::new(0),
            inflight_limit,
            inflight_room: Condvar::new(),
        }
    }

    fn push(&self, job: WholeHashJob) {
        let mut guard = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        guard.push_back(job);
        self.jobs_added.notify_one();
    }

    fn finish_scan(&self) {
        // Set the flag under the queue lock: a worker that saw `scan_done == false`
        // still holds the lock until `wait` parks it, so the notify cannot be lost.
        let _guard = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        self.scan_done.store(true, Ordering::Release);
        self.jobs_added.notify_all();
    }

    fn acquire_inflight(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        let mut guard = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let cur = self.inflight_bytes.load(Ordering::Relaxed);
            let allowed = if bytes > self.inflight_limit {
                // One worker may inflate a valid object larger than the total budget.
                cur == 0
            } else {
                cur.saturating_add(bytes) <= self.inflight_limit
            };
            if allowed {
                self.inflight_bytes.fetch_add(bytes, Ordering::Relaxed);
                return;
            }
            guard = self
                .inflight_room
                .wait(guard)
                .unwrap_or_else(|e| e.into_inner());
        }
    }

    fn release_inflight(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        self.inflight_bytes.fetch_sub(bytes, Ordering::Relaxed);
        self.inflight_room.notify_all();
    }
}

/// Scan the pack while worker threads inflate+hash each whole object as soon as it is discovered.
#[allow(clippy::too_many_arguments)]
fn scan_and_hash_whole_slots_overlapped(
    pack: &[u8],
    header: &PackHeader,
    pending: &mut PendingDeltas,
    records: &mut Vec<PackIndexRecord>,
    algo: HashAlgo,
    verify_trailer: bool,
    threads: std::num::NonZeroUsize,
    mut progress: Option<&mut PackIndexReporter<'_>>,
    cancel_watch: Option<Arc<AtomicBool>>,
    resident: &ResidentBudget<'_>,
) -> Result<()> {
    let n_workers = threads.get();
    let inflight_limit = n_workers.saturating_mul(WHOLE_INFLIGHT_BYTES_PER_THREAD);
    let queue = Arc::new(WholeHashQueue::new(inflight_limit));
    let first_error: Arc<Mutex<Option<Error>>> = Arc::new(Mutex::new(None));
    let hashed: Arc<Mutex<Vec<(usize, PackIndexRecord)>>> = Arc::new(Mutex::new(Vec::new()));

    thread::scope(|scope| {
        for _ in 0..n_workers {
            let queue = Arc::clone(&queue);
            let first_error = Arc::clone(&first_error);
            let hashed = Arc::clone(&hashed);
            let worker_watch = cancel_watch.clone();
            scope.spawn(move || {
                let mut zlib = ZlibInflateScratch::default();
                loop {
                    if let Some(w) = worker_watch.as_ref() {
                        if poll_watch(w).is_err() {
                            record_first_error(&first_error, Error::Cancelled);
                            return;
                        }
                    }
                    if first_error
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .is_some()
                    {
                        return;
                    }
                    let job = {
                        let mut guard = queue.jobs.lock().unwrap_or_else(|e| e.into_inner());
                        loop {
                            if let Some(job) = guard.pop_front() {
                                break job;
                            }
                            if queue.scan_done.load(Ordering::Acquire) {
                                return;
                            }
                            guard = queue
                                .jobs_added
                                .wait(guard)
                                .unwrap_or_else(|e| e.into_inner());
                        }
                    };
                    let inflight = job.slot.uncompressed_size;
                    queue.acquire_inflight(inflight);
                    let hash_result = if job.slot.uncompressed_size >= LARGE_WHOLE_OBJECT {
                        let mut fresh = ZlibInflateScratch::default();
                        hash_whole_slot(pack, &job.slot, algo, &mut fresh)
                    } else {
                        hash_whole_slot(pack, &job.slot, algo, &mut zlib)
                    };
                    queue.release_inflight(inflight);
                    resident.charge(job.slot.pack_end - job.slot.zlib_offset);
                    match hash_result {
                        Ok(item) => hashed
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push((job.index, item)),
                        Err(err) => record_first_error(&first_error, err),
                    }
                }
            });
        }

        let scan_result = (|| -> Result<usize> {
            let hb = algo.len();
            let mut skip_scratch = ZlibInflateScratch::default();
            let mut rd = PackReader::new(pack);
            rd.read_exact(4)?;
            rd.read_u32_be()?;
            rd.read_u32_be()?;
            let mut whole_index = 0usize;

            for slot in 0..header.nr_objects {
                if slot % 64 == 0 {
                    if let Some(w) = cancel_watch.as_ref() {
                        poll_watch(w)?;
                    }
                }
                if first_error
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_some()
                {
                    return Err(Error::CorruptObject("index-pack hash worker failed".into()));
                }
                let obj_offset = rd.pos;
                let (type_code, size) = rd.read_type_size()?;
                match type_code {
                    1..=4 => {
                        let kind = type_code_to_kind(type_code)?;
                        let zlib_offset = rd.pos;
                        let consumed =
                            skip_zlib_at_with_scratch(&mut skip_scratch, pack, zlib_offset, size)?;
                        let pack_end = rd.pos + consumed;
                        rd.pos = pack_end;
                        queue.push(WholeHashJob {
                            index: whole_index,
                            slot: WholeSlot {
                                offset: obj_offset,
                                pack_end,
                                zlib_offset,
                                kind,
                                uncompressed_size: size,
                            },
                        });
                        whole_index += 1;
                        pending.whole_ends.push(pack_end);
                    }
                    6 => {
                        push_ofs_delta(pack, &mut rd, obj_offset, size, pending, &mut skip_scratch)?
                    }
                    7 => push_ref_delta(
                        pack,
                        &mut rd,
                        obj_offset,
                        size,
                        hb,
                        pending,
                        &mut skip_scratch,
                    )?,
                    other => {
                        return Err(Error::CorruptObject(format!(
                            "unknown packed-object type {other}"
                        )));
                    }
                }
                resident.charge(rd.pos - obj_offset);
                if let Some(r) = progress.as_mut() {
                    r.on_object_slot();
                }
            }

            let consumed = rd.pos;
            verify_pack_trailer(pack, consumed, algo, verify_trailer.then_some(resident))?;
            Ok(consumed)
        })();

        queue.finish_scan();
        if let Err(err) = scan_result {
            record_first_error(&first_error, err);
        }
    });

    if let Some(err) = first_error.lock().unwrap_or_else(|e| e.into_inner()).take() {
        return Err(err);
    }

    let mut hashed = Arc::try_unwrap(hashed)
        .map_err(|_| Error::CorruptObject("index-pack hash worker stopped".into()))?
        .into_inner()
        .unwrap_or_else(|e| e.into_inner());
    hashed.sort_unstable_by_key(|(index, _)| *index);
    records.extend(hashed.into_iter().map(|(_, record)| record));
    Ok(())
}

fn hash_whole_slot(
    pack: &[u8],
    work: &WholeSlot,
    algo: HashAlgo,
    scratch: &mut ZlibInflateScratch,
) -> Result<PackIndexRecord> {
    let oid = hash_whole_slot_oid_from_pack(pack, work, algo, scratch)?;
    Ok(PackIndexRecord {
        oid,
        offset: u64::try_from(work.offset).unwrap_or(u64::MAX),
        crc32: crc32fast::hash(&pack[work.offset..work.pack_end]),
        kind: work.kind,
    })
}

fn hash_whole_slot_oid_from_pack(
    pack: &[u8],
    work: &WholeSlot,
    algo: HashAlgo,
    scratch: &mut ZlibInflateScratch,
) -> Result<ObjectId> {
    use crate::hash::ObjectPayloadHasher;
    let len = u64::try_from(work.uncompressed_size).unwrap_or(u64::MAX);
    let mut hasher = ObjectPayloadHasher::new(algo, work.kind, len);
    let mut pos = work.zlib_offset;
    scratch.decompress_fixed_update(pack, &mut pos, len, |chunk| {
        hasher.update(chunk);
        Ok(())
    })?;
    hasher.finish()
}

fn push_ofs_delta(
    pack: &[u8],
    rd: &mut PackReader<'_>,
    obj_offset: usize,
    size: usize,
    pending: &mut PendingDeltas,
    skip_scratch: &mut ZlibInflateScratch,
) -> Result<()> {
    let neg = rd.read_ofs_neg_offset()?;
    let base_offset = obj_offset
        .checked_sub(neg)
        .ok_or_else(|| Error::CorruptObject("ofs-delta base offset underflow".to_owned()))?;
    let zlib_offset = rd.pos;
    let consumed = skip_zlib_at_with_scratch(skip_scratch, pack, zlib_offset, size)?;
    let pack_end = rd.pos + consumed;
    rd.pos = pack_end;
    pending.push(
        pack,
        obj_offset,
        zlib_offset,
        pack_end,
        size,
        DeltaBase::Offset(base_offset),
    )
}

fn push_ref_delta(
    pack: &[u8],
    rd: &mut PackReader<'_>,
    obj_offset: usize,
    size: usize,
    hb: usize,
    pending: &mut PendingDeltas,
    skip_scratch: &mut ZlibInflateScratch,
) -> Result<()> {
    let base_bytes = rd.read_exact(hb)?;
    let base_oid = ObjectId::from_bytes(base_bytes)?;
    let zlib_offset = rd.pos;
    let consumed = skip_zlib_at_with_scratch(skip_scratch, pack, zlib_offset, size)?;
    let pack_end = rd.pos + consumed;
    rd.pos = pack_end;
    pending.push(
        pack,
        obj_offset,
        zlib_offset,
        pack_end,
        size,
        DeltaBase::Oid(base_oid),
    )
}

/// Check that the objects end exactly where the trailer starts and, when `checksum` is
/// set, that the trailer matches the pack body (charging the pass to that budget).
fn verify_pack_trailer(
    pack: &[u8],
    consumed: usize,
    algo: HashAlgo,
    checksum: Option<&ResidentBudget<'_>>,
) -> Result<()> {
    let hb = algo.len();
    let Some(body_end) = pack.len().checked_sub(hb) else {
        return Err(Error::CorruptObject("pack stream truncated".to_owned()));
    };
    if consumed != body_end {
        return Err(Error::CorruptObject(format!(
            "pack object stream ends at {consumed} but pack trailer starts at {body_end}"
        )));
    }
    let Some(resident) = checksum else {
        return Ok(());
    };
    let expected = resident.checksum(algo, &pack[..consumed]);
    let trailing = &pack[consumed..body_end + hb];
    if expected.as_slice() != trailing {
        return Err(Error::CorruptObject(
            "pack trailing checksum mismatch".to_owned(),
        ));
    }
    Ok(())
}

/// Turns each resolved delta into an index record; any failure aborts the build.
struct IndexRecordVisitor<'a> {
    algo: HashAlgo,
    pending: &'a PendingDeltas,
}

impl DeltaVisitor for IndexRecordVisitor<'_> {
    type Out = Vec<PackIndexRecord>;

    fn root(
        &self,
        _root: &DeltaRoot,
        _kind: ObjectKind,
        _data: &[u8],
        _out: &mut Self::Out,
    ) -> Result<bool> {
        Ok(true)
    }

    fn delta(
        &self,
        index: u32,
        kind: ObjectKind,
        data: &[u8],
        out: &mut Self::Out,
    ) -> Result<Option<ObjectId>> {
        let oid = hash_object(self.algo, kind, data)?;
        let index = index as usize;
        out.push(PackIndexRecord {
            oid,
            offset: u64::try_from(self.pending.deltas[index].offset).unwrap_or(u64::MAX),
            crc32: self.pending.crc32[index],
            kind,
        });
        Ok(Some(oid))
    }

    fn len(out: &Self::Out) -> usize {
        out.len()
    }

    fn merge(into: &mut Self::Out, from: &mut Self::Out) {
        into.append(from);
    }
}

/// Resolve every pending delta, appending one record per delta to `records`.
///
/// Bases in the pack are walked first; ref-deltas still unresolved afterwards are retried
/// from bases in `odb` (thin packs). Returns those odb bases, sorted by object id. Errors
/// when some delta has no reachable base.
#[allow(clippy::too_many_arguments)]
fn resolve_deltas(
    source: PackBytes<'_>,
    odb: &Odb,
    algo: HashAlgo,
    threads: std::num::NonZeroUsize,
    pending: &PendingDeltas,
    records: &mut Vec<PackIndexRecord>,
    mut progress: Option<&mut PackIndexReporter<'_>>,
    chain_limit: usize,
    cancel: Option<&dyn Cancel>,
    cancel_watch: Option<&Arc<AtomicBool>>,
) -> Result<Vec<ObjectId>> {
    if pending.deltas.is_empty() {
        return Ok(Vec::new());
    }
    poll_index_build_cancel(cancel, None, cancel_watch)?;
    let visitor = IndexRecordVisitor { algo, pending };
    let walk = DeltaWalk::new(
        source,
        odb,
        &pending.deltas,
        &pending.children,
        chain_limit,
        &visitor,
    );
    let in_pack: Vec<DeltaRoot> = records
        .iter()
        .zip(&pending.whole_ends)
        .filter_map(|(record, &end)| {
            let offset = usize::try_from(record.offset).ok()?;
            pending
                .children
                .has_children(offset, &record.oid)
                .then_some(DeltaRoot::Pack {
                    offset,
                    end,
                    oid: record.oid,
                })
        })
        .collect();
    let mut report = |resolved: u64| {
        if let Some(r) = progress.as_mut() {
            r.set_deltas_resolved(resolved);
        }
    };
    walk.run(&in_pack, threads, records, &mut report)?;
    let thin = walk.thin_roots();
    walk.run(&thin, threads, records, &mut report)?;

    let total = u64::try_from(pending.deltas.len()).unwrap_or(u64::MAX);
    let unresolved = total.saturating_sub(walk.resolved());
    if unresolved > 0 {
        return Err(Error::UnresolvedDeltas { count: unresolved });
    }
    let mut thin_bases: Vec<ObjectId> = thin
        .iter()
        .filter_map(|root| match root {
            DeltaRoot::Odb(oid) => Some(*oid),
            DeltaRoot::Pack { .. } => None,
        })
        .collect();
    // A base can also be a delta in this pack that another thin base resolved.
    if !thin_bases.is_empty() {
        let candidates: HashSet<ObjectId> = thin_bases.iter().copied().collect();
        let in_pack: HashSet<ObjectId> = records
            .iter()
            .filter(|record| candidates.contains(&record.oid))
            .map(|record| record.oid)
            .collect();
        thin_bases.retain(|oid| !in_pack.contains(oid));
    }
    Ok(thin_bases)
}
#[cfg(test)]
mod line_coverage_tests {
    use super::*;
    use crate::hash::Parallelism;
    use crate::progress::{ProgressPhase, ProgressSink};
    use grit_test_support::objects::{HashAlgo, ObjectKind as PK, PackBuilder};

    #[derive(Default)]
    struct PhaseCapture {
        phases: Vec<(ProgressPhase, Option<u64>)>,
        max_by_phase: std::collections::HashMap<ProgressPhase, u64>,
    }

    impl ProgressSink for PhaseCapture {
        fn start(&mut self, phase: ProgressPhase, total: Option<u64>) {
            self.phases.push((phase, total));
        }

        fn set(&mut self, current: u64) {
            if let Some((phase, _)) = self.phases.last() {
                let e = self.max_by_phase.entry(*phase).or_insert(0);
                *e = (*e).max(current);
            }
        }

        fn finish(&mut self) {}
    }

    fn delta_pack_with_ofs_chain() -> (Vec<u8>, Odb) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(tmp.path());
        let mut pb = PackBuilder::new(HashAlgo::Sha1);
        for i in 0..8 {
            pb.add_full(PK::Blob, format!("standalone-{i}\n").as_bytes());
        }
        let mut bases = Vec::new();
        for i in 0..6 {
            let data = format!("delta-base-{i}\n");
            bases.push(pb.add_full(PK::Blob, data.as_bytes()));
        }
        for (i, base_idx) in bases.into_iter().enumerate() {
            let base = format!("delta-base-{i}\n");
            let target = format!("delta-base-{i}-extended\n");
            let delta = crate::delta_encode::encode_lcp_delta(base.as_bytes(), target.as_bytes())
                .expect("delta");
            pb.add_ofs_delta(base_idx, &delta, delta.len());
        }
        (pb.build().bytes, odb)
    }

    #[test]
    fn build_records_serial_parallel_and_progress() {
        let (pack, odb) = delta_pack_with_ofs_chain();
        for threads in [1_usize, 2] {
            let records =
                build_pack_index_records(&pack, &odb, Parallelism::resolve(Some(threads)), true)
                    .unwrap_or_else(|e| panic!("threads={threads}: {e}"));
            assert!(!records.is_empty(), "threads={threads}");
        }
        let mut cap = PhaseCapture::default();
        let records = build_pack_index_records_with_progress(
            &pack,
            &odb,
            Parallelism::resolve(Some(2)),
            true,
            Some(&mut cap),
            None,
        )
        .expect("with progress");
        assert!(!records.is_empty());
        assert!(cap
            .max_by_phase
            .contains_key(&ProgressPhase::IndexingObjects));
    }

    #[test]
    fn parallel_index_whole_object_larger_than_inflight_budget() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(tmp.path());
        let mut pb = PackBuilder::new(HashAlgo::Sha1);
        pb.add_full(PK::Blob, &vec![0xAB_u8; 20 * 1024 * 1024]);
        let pack = pb.build().bytes;
        let records = build_pack_index_records(&pack, &odb, Parallelism::resolve(Some(2)), false)
            .unwrap_or_else(|e| panic!("20 MiB whole object with 2 threads: {e}"));
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn build_skips_trailer_verify_when_requested() {
        let (pack, odb) = delta_pack_with_ofs_chain();
        let with = build_pack_index_records(&pack, &odb, Parallelism::resolve(Some(1)), true)
            .expect("verify");
        let without = build_pack_index_records(&pack, &odb, Parallelism::resolve(Some(1)), false)
            .expect("no verify");
        assert_eq!(with.len(), without.len());
    }

    #[test]
    fn read_pack_header_rejects_garbage() {
        assert!(read_pack_header(b"NOTPACK").is_err());
        let mut pb = PackBuilder::new(HashAlgo::Sha1);
        pb.add_full(PK::Blob, b"x");
        let mut pack = pb.build().bytes;
        pack[4] = 99;
        pack[5] = 99;
        pack[6] = 99;
        pack[7] = 99;
        assert!(read_pack_header(&pack).is_err());
    }

    #[test]
    fn large_pack_many_threads_exercises_parallel_scan() {
        let mut pb = PackBuilder::new(HashAlgo::Sha1);
        for i in 0..32 {
            pb.add_full(PK::Blob, format!("blob-{i}\n").as_bytes());
        }
        let pack = pb.build().bytes;
        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(tmp.path());
        for threads in [2_usize, 4] {
            let records =
                build_pack_index_records(&pack, &odb, Parallelism::resolve(Some(threads)), true)
                    .unwrap_or_else(|e| panic!("threads={threads}: {e}"));
            assert_eq!(records.len(), 32, "threads={threads}");
        }
    }

    #[test]
    fn unresolved_delta_chain_errors() {
        let mut pb = PackBuilder::new(HashAlgo::Sha1);
        let missing_base =
            crate::objects::ObjectId::from_hex("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef")
                .expect("oid");
        let delta = b"invalid delta bytes";
        pb.add_ref_delta(missing_base.as_bytes(), delta, delta.len());
        let pack = pb.build().bytes;
        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(tmp.path());
        assert!(
            build_pack_index_records(&pack, &odb, Parallelism::resolve(Some(2)), true).is_err()
        );
    }

    #[test]
    fn ref_delta_resolves_when_base_in_odb() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(tmp.path());
        let base_bytes = b"ref-delta base payload\n";
        let base_oid = odb.write(ObjectKind::Blob, base_bytes).expect("write base");
        let target = b"ref-delta base PAYLOAD\n";
        let delta = crate::delta_encode::encode_lcp_delta(base_bytes, target).expect("delta");
        let mut pb = PackBuilder::new(HashAlgo::Sha1);
        pb.add_ref_delta(base_oid.as_bytes(), &delta, delta.len());
        let pack = pb.build().bytes;
        let records = build_pack_index_records(&pack, &odb, Parallelism::resolve(Some(1)), true)
            .expect("ref delta build");
        assert_eq!(records.len(), 1);
        let no_verify = build_pack_index_records(&pack, &odb, Parallelism::resolve(Some(8)), false)
            .expect("parallel no verify");
        assert_eq!(no_verify.len(), 1);
    }

    /// Records sorted by oid, so serial and parallel builds compare directly.
    fn sorted_records(pack: &[u8], odb: &Odb, threads: usize) -> Vec<(ObjectId, u64, u32)> {
        let mut out: Vec<_> =
            build_pack_index_records(pack, odb, Parallelism::resolve(Some(threads)), true)
                .unwrap_or_else(|e| panic!("threads={threads}: {e}"))
                .into_iter()
                .map(|r| (r.oid, r.offset, r.crc32))
                .collect();
        out.sort();
        out
    }

    fn extend(base: &[u8], suffix: &str) -> Vec<u8> {
        let mut next = base.to_vec();
        next.extend_from_slice(suffix.as_bytes());
        next
    }

    #[test]
    fn parallel_delta_chain_retains_delta_results_as_bases() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(tmp.path());
        let mut pb = PackBuilder::new(HashAlgo::Sha1);

        // c0 <- c1 <- c2 <- c3 <- c4 (ofs-deltas). c2 is also a ref-delta base (retained
        // by offset and oid), c4 only by oid, c3 only by offset.
        let mut contents = vec![b"chain-0 shared prefix for every link\n".to_vec()];
        let mut idx = pb.add_full(PK::Blob, &contents[0]);
        for i in 1..5 {
            let next = extend(&contents[i - 1], &format!("link-{i}\n"));
            let delta =
                crate::delta_encode::encode_lcp_delta(&contents[i - 1], &next).expect("ofs delta");
            idx = pb.add_ofs_delta(idx, &delta, delta.len());
            contents.push(next);
        }
        for base in [&contents[2], &contents[4]] {
            let oid = odb.hash(ObjectKind::Blob, base).expect("hash");
            let target = extend(base, "ref-child\n");
            let delta = crate::delta_encode::encode_lcp_delta(base, &target).expect("ref delta");
            pb.add_ref_delta(oid.as_bytes(), &delta, delta.len());
        }
        let pack = pb.build().bytes;

        let serial = sorted_records(&pack, &odb, 1);
        assert_eq!(serial.len(), 7);
        for threads in [2_usize, 4] {
            assert_eq!(
                sorted_records(&pack, &odb, threads),
                serial,
                "threads={threads}"
            );
        }
        let tip = odb.hash(ObjectKind::Blob, &contents[4]).expect("hash");
        assert!(serial.iter().any(|(oid, _, _)| *oid == tip));
    }

    fn records_with_limit(
        pack: &[u8],
        odb: &Odb,
        threads: usize,
        chain_limit: usize,
    ) -> Result<Vec<(ObjectId, u64, u32)>> {
        let mut out: Vec<_> = build_records(
            pack,
            odb,
            Parallelism::resolve(Some(threads)),
            true,
            None,
            None,
            chain_limit,
            &ResidentBudget::unbounded(),
            PackBytes::Slice(pack),
        )?
        .records
        .into_iter()
        .map(|r| (r.oid, r.offset, r.crc32))
        .collect();
        out.sort();
        Ok(out)
    }

    fn delta(base: &[u8], target: &[u8]) -> Vec<u8> {
        crate::delta_encode::encode_lcp_delta(base, target).expect("delta")
    }

    /// Blob history shaped like a real pack: a long ofs chain with side branches (one
    /// nested), a ref-delta hanging off the middle of the chain by oid, and an ofs
    /// child of that ref-delta. Returns the pack and the content of every object.
    fn branching_chain_pack(odb: &Odb) -> (Vec<u8>, Vec<Vec<u8>>) {
        let mut pb = PackBuilder::new(HashAlgo::Sha1);
        let mut contents = vec![b"root of the history: shared prefix\n".repeat(8)];
        let mut chain = vec![pb.add_full(PK::Blob, &contents[0])];
        for i in 1..12 {
            let base = contents[chain.len() - 1].clone();
            let next = extend(&base, &format!("chain-{i}\n"));
            let d = delta(&base, &next);
            let idx = pb.add_ofs_delta(chain[chain.len() - 1], &d, d.len());
            chain.push(idx);
            contents.push(next);
        }
        for at in [3_usize, 6] {
            for side in ["a", "b"] {
                let base = contents[at].clone();
                let next = extend(&base, &format!("side-{at}{side}\n"));
                let d = delta(&base, &next);
                let idx = pb.add_ofs_delta(chain[at], &d, d.len());
                contents.push(next.clone());
                if side == "a" {
                    let nested = extend(&next, "nested\n");
                    let d = delta(&next, &nested);
                    pb.add_ofs_delta(idx, &d, d.len());
                    contents.push(nested);
                }
            }
        }
        let mid = contents[5].clone();
        let mid_oid = odb.hash(ObjectKind::Blob, &mid).expect("hash");
        let by_oid = extend(&mid, "ref-child\n");
        let d = delta(&mid, &by_oid);
        let ref_idx = pb.add_ref_delta(mid_oid.as_bytes(), &d, d.len());
        let grandchild = extend(&by_oid, "ofs-grandchild\n");
        let d = delta(&by_oid, &grandchild);
        pb.add_ofs_delta(ref_idx, &d, d.len());
        contents.push(by_oid);
        contents.push(grandchild);
        (pb.build().bytes, contents)
    }

    #[test]
    fn depth_first_resolution_is_independent_of_threads_and_chain_cap() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(tmp.path());
        let (pack, contents) = branching_chain_pack(&odb);
        let mut expected: Vec<ObjectId> = contents
            .iter()
            .map(|c| odb.hash(ObjectKind::Blob, c).expect("hash"))
            .collect();
        expected.sort();

        let reference = records_with_limit(&pack, &odb, 1, DELTA_CHAIN_BYTES_PER_WORKER)
            .expect("reference build");
        let oids: Vec<ObjectId> = reference.iter().map(|(oid, _, _)| *oid).collect();
        assert_eq!(oids, expected);
        // A cap of zero evicts every ancestor, so each later sibling rebuilds its base
        // from the root, through released parents and through retained frames.
        for chain_limit in [0_usize, 300, DELTA_CHAIN_BYTES_PER_WORKER] {
            for threads in [1_usize, 2, 4] {
                assert_eq!(
                    records_with_limit(&pack, &odb, threads, chain_limit)
                        .unwrap_or_else(|e| panic!("threads={threads} cap={chain_limit}: {e}")),
                    reference,
                    "threads={threads} cap={chain_limit}"
                );
            }
        }

        let mut cap = PhaseCapture::default();
        build_pack_index_records_with_progress(
            &pack,
            &odb,
            Parallelism::resolve(Some(2)),
            true,
            Some(&mut cap),
            None,
        )
        .expect("with progress");
        let deltas = u64::try_from(contents.len() - 1).expect("count");
        assert!(cap
            .phases
            .contains(&(ProgressPhase::ResolvingDeltas, Some(deltas))));
        assert_eq!(
            cap.max_by_phase.get(&ProgressPhase::ResolvingDeltas),
            Some(&deltas)
        );
    }

    #[test]
    fn duplicate_base_in_pack_resolves_ref_child_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(tmp.path());
        let base = b"stored twice in one pack\n".repeat(4);
        let base_oid = odb.hash(ObjectKind::Blob, &base).expect("hash");
        let child = extend(&base, "child\n");
        let grandchild = extend(&child, "grandchild\n");
        let mut pb = PackBuilder::new(HashAlgo::Sha1);
        pb.add_full(PK::Blob, &base);
        pb.add_full(PK::Blob, &base);
        let d = delta(&base, &child);
        let child_idx = pb.add_ref_delta(base_oid.as_bytes(), &d, d.len());
        let d = delta(&child, &grandchild);
        pb.add_ofs_delta(child_idx, &d, d.len());
        let pack = pb.build().bytes;
        for threads in [1_usize, 2, 4] {
            let records = records_with_limit(&pack, &odb, threads, 0)
                .unwrap_or_else(|e| panic!("threads={threads}: {e}"));
            assert_eq!(records.len(), 4, "threads={threads}");
            let child_oid = odb.hash(ObjectKind::Blob, &child).expect("hash");
            assert_eq!(
                records
                    .iter()
                    .filter(|(oid, _, _)| *oid == child_oid)
                    .count(),
                1,
                "threads={threads}"
            );
        }
    }

    #[test]
    fn thin_base_in_odb_resolves_its_whole_delta_tree() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(tmp.path());
        let base = b"only in the receiving repository\n".repeat(4);
        let base_oid = odb.write(ObjectKind::Blob, &base).expect("write base");
        let first = extend(&base, "first\n");
        let second = extend(&first, "second\n");
        let third = extend(&second, "third\n");
        let side = extend(&first, "side\n");
        let second_oid = odb.hash(ObjectKind::Blob, &second).expect("hash");
        let mut pb = PackBuilder::new(HashAlgo::Sha1);
        let d = delta(&base, &first);
        let first_idx = pb.add_ref_delta(base_oid.as_bytes(), &d, d.len());
        let d = delta(&first, &second);
        pb.add_ofs_delta(first_idx, &d, d.len());
        let d = delta(&second, &third);
        pb.add_ref_delta(second_oid.as_bytes(), &d, d.len());
        let d = delta(&first, &side);
        pb.add_ofs_delta(first_idx, &d, d.len());
        let pack = pb.build().bytes;

        let mut expected: Vec<ObjectId> = [&first, &second, &third, &side]
            .iter()
            .map(|c| odb.hash(ObjectKind::Blob, c).expect("hash"))
            .collect();
        expected.sort();
        for chain_limit in [0_usize, DELTA_CHAIN_BYTES_PER_WORKER] {
            for threads in [1_usize, 4] {
                let records = records_with_limit(&pack, &odb, threads, chain_limit)
                    .unwrap_or_else(|e| panic!("threads={threads} cap={chain_limit}: {e}"));
                let oids: Vec<ObjectId> = records.iter().map(|(oid, _, _)| *oid).collect();
                assert_eq!(oids, expected, "threads={threads} cap={chain_limit}");
            }
        }
    }

    #[test]
    fn ref_delta_cycle_is_reported_unresolved() {
        let a = b"cycle member a\n".to_vec();
        let b = b"cycle member b\n".to_vec();
        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(tmp.path());
        let a_oid = odb.hash(ObjectKind::Blob, &a).expect("hash");
        let b_oid = odb.hash(ObjectKind::Blob, &b).expect("hash");
        let mut pb = PackBuilder::new(HashAlgo::Sha1);
        pb.add_full(PK::Blob, b"unrelated whole object\n");
        let d = delta(&b, &a);
        pb.add_ref_delta(b_oid.as_bytes(), &d, d.len());
        let d = delta(&a, &b);
        pb.add_ref_delta(a_oid.as_bytes(), &d, d.len());
        let pack = pb.build().bytes;
        for threads in [1_usize, 2] {
            match build_pack_index_records(&pack, &odb, Parallelism::resolve(Some(threads)), true) {
                Err(Error::UnresolvedDeltas { count: 2 }) => {}
                other => panic!("threads={threads}: expected unresolved deltas, got {other:?}"),
            }
        }
    }

    #[test]
    fn parallel_scan_hashes_large_whole_objects_outside_scratch() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(tmp.path());
        let large: Vec<u8> = (0..LARGE_WHOLE_OBJECT + 64)
            .map(|i| u8::try_from(i % 251).unwrap_or(0))
            .collect();
        let mut pb = PackBuilder::new(HashAlgo::Sha1);
        let large_idx = pb.add_full(PK::Blob, &large);
        for i in 0..4 {
            pb.add_full(PK::Blob, format!("small-{i}\n").as_bytes());
        }
        let target = extend(&large, "tail\n");
        let delta = crate::delta_encode::encode_lcp_delta(&large, &target).expect("delta");
        pb.add_ofs_delta(large_idx, &delta, delta.len());
        let pack = pb.build().bytes;

        let serial = sorted_records(&pack, &odb, 1);
        assert_eq!(serial.len(), 6);
        assert_eq!(sorted_records(&pack, &odb, 2), serial);
        let large_oid = odb.hash(ObjectKind::Blob, &large).expect("hash");
        assert!(serial.iter().any(|(oid, _, _)| *oid == large_oid));
    }

    #[test]
    fn parallel_scan_rejects_unknown_object_type() {
        let mut pb = PackBuilder::new(HashAlgo::Sha1);
        for i in 0..4 {
            pb.add_full(PK::Blob, format!("ok-{i}\n").as_bytes());
        }
        // Type 5 is reserved; low nibble is the size (1).
        pb.add_full_with_header_override(&[0x51], b"x");
        let pack = pb.build().bytes;
        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(tmp.path());
        for threads in [1_usize, 2] {
            let err =
                build_pack_index_records(&pack, &odb, Parallelism::resolve(Some(threads)), true)
                    .expect_err("reserved type must fail");
            assert!(
                matches!(err, Error::CorruptObject(_)),
                "threads={threads}: {err:?}"
            );
        }
    }

    #[test]
    fn parallel_scan_surfaces_corrupt_whole_object() {
        let mut pb = PackBuilder::new(HashAlgo::Sha1);
        for i in 0..4 {
            pb.add_full(PK::Blob, format!("fine-{i}\n").as_bytes());
        }
        // Blob header claims 15 bytes but the zlib stream inflates to 1.
        pb.add_full_with_header_override(&[0x3F], b"x");
        for i in 0..4 {
            pb.add_full(PK::Blob, format!("after-{i}\n").as_bytes());
        }
        let pack = pb.build().bytes;
        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(tmp.path());
        for threads in [1_usize, 2, 4] {
            assert!(
                build_pack_index_records(&pack, &odb, Parallelism::resolve(Some(threads)), true)
                    .is_err(),
                "threads={threads}: size mismatch must fail"
            );
        }
    }

    /// One whole blob plus `extra_deltas` unique append OFS-deltas (system git accepts these packs).
    fn pack_one_whole_plus_append_ofs_deltas(extra_deltas: usize) -> Vec<u8> {
        let mut pb = PackBuilder::new(HashAlgo::Sha1);
        let mut data = b"deep-chain-seed\n".to_vec();
        let mut idx = pb.add_full(PK::Blob, &data);
        for _ in 0..extra_deltas {
            let prev = data.clone();
            data.push(b'!');
            let delta = crate::delta_encode::encode_lcp_delta(&prev, &data).expect("append delta");
            idx = pb.add_ofs_delta(idx, &delta, delta.len());
        }
        let _ = idx;
        pb.build().bytes
    }

    fn git_index_pack_succeeds(pack: &[u8]) -> bool {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("in.pack");
        std::fs::write(&path, pack).expect("write pack");
        std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["index-pack", "-v", &path.to_string_lossy()])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn deep_ofs_delta_chain_matches_git_index_pack() {
        const EXTRA: usize = 3002;
        const OBJECTS: usize = 1 + EXTRA;
        let pack = pack_one_whole_plus_append_ofs_deltas(EXTRA);
        assert_eq!(
            u32::from_be_bytes([pack[8], pack[9], pack[10], pack[11]]) as usize,
            OBJECTS
        );
        assert!(
            git_index_pack_succeeds(&pack),
            "system git must index a {OBJECTS}-object OFS-delta chain"
        );
        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(tmp.path());
        for threads in [1_usize, 2] {
            let records =
                build_pack_index_records(&pack, &odb, Parallelism::resolve(Some(threads)), true)
                    .unwrap_or_else(|e| panic!("threads={threads} deep chain: {e}"));
            assert_eq!(records.len(), OBJECTS, "threads={threads}");
        }
    }

    #[test]
    fn parallel_scan_rejects_bad_trailer() {
        let mut pb = PackBuilder::new(HashAlgo::Sha1);
        for i in 0..4 {
            pb.add_full(PK::Blob, format!("t-{i}\n").as_bytes());
        }
        pb.set_corrupt_trailer();
        let pack = pb.build().bytes;
        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(tmp.path());
        assert!(
            build_pack_index_records(&pack, &odb, Parallelism::resolve(Some(2)), true).is_err()
        );
        assert!(
            build_pack_index_records(&pack, &odb, Parallelism::resolve(Some(2)), false).is_ok()
        );
    }
}
