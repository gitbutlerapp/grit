//! Build v2 pack index records from an in-memory pack (index-pack hashing path).
//!
//! Multi-threaded builds overlap zlib-skip (boundary scan) with parallel inflate+hash
//! on batched whole objects; delta chains inflate+apply+hash in parallel rounds.
//! Single-threaded builds inflate and hash each whole object once in pack order.

use crate::error::{Error, Result};
use crate::hash::{hash_object, Parallelism};
use crate::objects::{oid_hash_map, HashAlgo, ObjectId, ObjectKind, OidHashMap};
use crate::odb::Odb;
use crate::pack_zlib::skip_zlib_at_with_scratch;
use crate::progress::{ProgressPhase, ProgressSink};
use crate::unpack_objects::{
    apply_delta, apply_delta_into, type_code_to_kind, PackIndexRecord, PackReader,
};
use crate::zlib_inflate::ZlibInflateScratch;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

/// Poll the coordinating thread every N object slots / resolved deltas.
const PROGRESS_POLL_EVERY: u64 = 32;

/// Coordinates pack-index progress on the caller thread (workers bump atomics only).
struct PackIndexReporter<'a> {
    sink: &'a mut dyn ProgressSink,
    object_total: u64,
    indexed: AtomicU64,
    resolved: AtomicU64,
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
            resolved: AtomicU64::new(0),
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
        self.resolved.store(0, Ordering::Relaxed);
        self.last_delta_reported = 0;
        self.sink
            .start(ProgressPhase::ResolvingDeltas, Some(delta_total));
    }

    fn on_deltas_resolved(&mut self, count: u64) {
        if count == 0 {
            return;
        }
        self.resolved.fetch_add(count, Ordering::Relaxed);
        self.poll_deltas();
    }

    fn poll_deltas(&mut self) {
        let cur = self.resolved.load(Ordering::Relaxed);
        if cur.saturating_sub(self.last_delta_reported) >= PROGRESS_POLL_EVERY {
            self.sink.set(cur);
            self.last_delta_reported = cur;
        }
    }

    fn finish_deltas(&mut self) {
        let cur = self.resolved.load(Ordering::Relaxed);
        self.sink.set(cur);
        self.sink.finish();
    }
}

/// Maximum ofs/ref delta chain depth when re-inflating from the pack.
const MAX_DELTA_CHAIN_DEPTH: usize = 3000;

#[derive(Clone)]
enum BasePayload {
    /// Re-inflate from the pack when a dependent delta is resolved.
    InPack {
        zlib_offset: usize,
        uncompressed_size: usize,
    },
    /// Resolved delta still in the pack; re-apply from compressed bytes when needed.
    DeltaInPack {
        zlib_offset: usize,
        uncompressed_size: usize,
        base_offset: Option<usize>,
        base_oid: Option<ObjectId>,
    },
}

#[derive(Clone)]
struct IndexBase {
    kind: ObjectKind,
    payload: BasePayload,
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
    oid_base_refs: &'a mut OidHashMap<u32>,
    ofs_bases: &'a mut HashMap<usize, IndexBase>,
    oid_bases: &'a mut OidHashMap<IndexBase>,
    records: &'a mut Vec<PackIndexRecord>,
}

struct WholeSlot {
    offset: usize,
    pack_end: usize,
    zlib_offset: usize,
    kind: ObjectKind,
    uncompressed_size: usize,
}

struct WholeHashed {
    record: PackIndexRecord,
    offset: usize,
    oid: ObjectId,
    kind: ObjectKind,
    zlib_offset: usize,
    uncompressed_size: usize,
}

/// Hash oversized whole objects outside the parallel pool to cap per-thread memory.
const LARGE_WHOLE_OBJECT: usize = 4 * 1024 * 1024;
/// Soft cap on total uncompressed bytes being hashed by whole-object workers at once.
const WHOLE_INFLIGHT_BYTES_PER_THREAD: usize = 8 * 1024 * 1024;
/// How many ready deltas to gather before starting another dependency round.
const DELTA_RESOLVE_ROUND_CHUNK: usize = 4096;
/// Reused delta buffers may grow to the largest object; trim excess capacity periodically.
const DELTA_SCRATCH_CAP: usize = 2 * 1024 * 1024;
struct PackHeader {
    nr_objects: usize,
}

/// Build index records for `pack`, using up to `parallelism` worker threads.
///
/// When `verify_trailer` is false, skips the full-pack SHA-1 trailer check during
/// the object scan (used when the caller already validated the stream on the wire).
pub fn build_pack_index_records(
    pack: &[u8],
    odb: &Odb,
    parallelism: Parallelism,
    verify_trailer: bool,
) -> Result<Vec<PackIndexRecord>> {
    build_pack_index_records_with_progress(pack, odb, parallelism, verify_trailer, None)
}

/// Like [`build_pack_index_records`], emitting [`ProgressPhase::IndexingObjects`] then
/// [`ProgressPhase::ResolvingDeltas`] on `progress` when provided.
pub fn build_pack_index_records_with_progress(
    pack: &[u8],
    odb: &Odb,
    parallelism: Parallelism,
    verify_trailer: bool,
    progress: Option<&mut dyn ProgressSink>,
) -> Result<Vec<PackIndexRecord>> {
    crate::trace::trace_span!("grit.pack.delta.resolve");
    let algo = odb.hash_algo();
    let threads = parallelism.threads();
    let header = read_pack_header(pack)?;
    let object_total = u64::try_from(header.nr_objects).unwrap_or(u64::MAX);
    let mut reporter = progress.map(|sink| PackIndexReporter::begin_indexing(sink, object_total));
    let mut ofs_base_refs: HashMap<usize, u32> = HashMap::new();
    let mut oid_base_refs: OidHashMap<u32> = oid_hash_map();
    let mut pending: Vec<PendingIndexDelta> = Vec::new();
    let mut records: Vec<PackIndexRecord> = Vec::with_capacity(header.nr_objects);
    let mut ofs_bases: HashMap<usize, IndexBase> = HashMap::new();
    let mut oid_bases: OidHashMap<IndexBase> = oid_hash_map();

    let mut state = IndexBuildState {
        odb,
        ofs_base_refs: &mut ofs_base_refs,
        oid_base_refs: &mut oid_base_refs,
        ofs_bases: &mut ofs_bases,
        oid_bases: &mut oid_bases,
        records: &mut records,
    };

    if threads.get() <= 1 {
        scan_and_hash_wholes_serial(
            pack,
            &header,
            &mut pending,
            &mut state,
            algo,
            verify_trailer,
            reporter.as_mut(),
        )?;
    } else {
        scan_and_hash_whole_slots_overlapped(
            pack,
            &header,
            &mut pending,
            &mut state,
            algo,
            verify_trailer,
            threads,
            reporter.as_mut(),
        )?;
    }

    let delta_total = u64::try_from(pending.len()).unwrap_or(u64::MAX);
    if let Some(r) = reporter.as_mut() {
        r.finish_indexing();
        if delta_total > 0 {
            r.begin_deltas(delta_total);
        }
    }

    resolve_pending_deltas_parallel(
        pack,
        threads,
        algo,
        &mut pending,
        &mut state,
        reporter.as_mut(),
    )?;

    if let Some(r) = reporter.as_mut() {
        if delta_total > 0 {
            r.finish_deltas();
        }
    }
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
    Ok(PackHeader { nr_objects })
}

/// Single-threaded pack walk: one inflate+hash per whole object; zlib-skip deltas only.
fn scan_and_hash_wholes_serial(
    pack: &[u8],
    header: &PackHeader,
    pending: &mut Vec<PendingIndexDelta>,
    state: &mut IndexBuildState<'_>,
    algo: HashAlgo,
    verify_trailer: bool,
    mut progress: Option<&mut PackIndexReporter<'_>>,
) -> Result<()> {
    let hb = state.odb.hash_algo().len();
    let mut skip_scratch = ZlibInflateScratch::default();
    let mut rd = PackReader::new(pack);
    rd.read_exact(4)?;
    rd.read_u32_be()?;
    rd.read_u32_be()?;
    let mut whole_items: Vec<WholeHashed> = Vec::new();

    for _ in 0..header.nr_objects {
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
                let mut hash_scratch = ZlibInflateScratch::default();
                let item = hash_one_whole_slot_with_scratch(
                    pack,
                    &work,
                    algo,
                    state.ofs_base_refs,
                    state.oid_base_refs,
                    &mut hash_scratch,
                )?;
                whole_items.push(item);
            }
            6 => {
                push_ofs_delta(
                    pack,
                    &mut rd,
                    obj_offset,
                    size,
                    hb,
                    pending,
                    state,
                    &mut skip_scratch,
                )?;
            }
            7 => {
                push_ref_delta(
                    pack,
                    &mut rd,
                    obj_offset,
                    size,
                    hb,
                    pending,
                    state,
                    &mut skip_scratch,
                )?;
            }
            other => {
                return Err(Error::CorruptObject(format!(
                    "unknown packed-object type {other}"
                )));
            }
        }
        if let Some(r) = progress.as_mut() {
            r.on_object_slot();
        }
    }

    for item in whole_items {
        apply_whole_hashed(state, item);
    }

    let consumed = rd.pos;
    if verify_trailer {
        verify_pack_trailer(pack, consumed, state.odb.hash_algo())?;
    }
    Ok(())
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
    pending: &mut Vec<PendingIndexDelta>,
    state: &mut IndexBuildState<'_>,
    algo: HashAlgo,
    verify_trailer: bool,
    threads: std::num::NonZeroUsize,
    mut progress: Option<&mut PackIndexReporter<'_>>,
) -> Result<()> {
    let n_workers = threads.get();
    let inflight_limit = n_workers.saturating_mul(WHOLE_INFLIGHT_BYTES_PER_THREAD);
    let ofs_base_refs = state.ofs_base_refs.clone();
    let oid_base_refs = state.oid_base_refs.clone();
    let queue = Arc::new(WholeHashQueue::new(inflight_limit));
    let first_error: Arc<Mutex<Option<Error>>> = Arc::new(Mutex::new(None));
    let hashed: Arc<Mutex<Vec<(usize, WholeHashed)>>> = Arc::new(Mutex::new(Vec::new()));

    thread::scope(|scope| {
        for _ in 0..n_workers {
            let queue = Arc::clone(&queue);
            let first_error = Arc::clone(&first_error);
            let hashed = Arc::clone(&hashed);
            let ofs_base_refs = ofs_base_refs.clone();
            let oid_base_refs = oid_base_refs.clone();
            scope.spawn(move || {
                let mut zlib = ZlibInflateScratch::default();
                loop {
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
                        hash_one_whole_slot(pack, &job.slot, algo, &ofs_base_refs, &oid_base_refs)
                    } else {
                        hash_one_whole_slot_with_scratch(
                            pack,
                            &job.slot,
                            algo,
                            &ofs_base_refs,
                            &oid_base_refs,
                            &mut zlib,
                        )
                    };
                    queue.release_inflight(inflight);
                    match hash_result {
                        Ok(item) => hashed
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push((job.index, item)),
                        Err(err) => record_whole_hash_error(&first_error, err),
                    }
                }
            });
        }

        let scan_result = (|| -> Result<usize> {
            let hb = state.odb.hash_algo().len();
            let mut skip_scratch = ZlibInflateScratch::default();
            let mut rd = PackReader::new(pack);
            rd.read_exact(4)?;
            rd.read_u32_be()?;
            rd.read_u32_be()?;
            let mut whole_index = 0usize;

            for _ in 0..header.nr_objects {
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
                    }
                    6 => {
                        push_ofs_delta(
                            pack,
                            &mut rd,
                            obj_offset,
                            size,
                            hb,
                            pending,
                            state,
                            &mut skip_scratch,
                        )?;
                    }
                    7 => {
                        push_ref_delta(
                            pack,
                            &mut rd,
                            obj_offset,
                            size,
                            hb,
                            pending,
                            state,
                            &mut skip_scratch,
                        )?;
                    }
                    other => {
                        return Err(Error::CorruptObject(format!(
                            "unknown packed-object type {other}"
                        )));
                    }
                }
                if let Some(r) = progress.as_mut() {
                    r.on_object_slot();
                }
            }

            let consumed = rd.pos;
            if verify_trailer {
                verify_pack_trailer(pack, consumed, state.odb.hash_algo())?;
            }
            Ok(consumed)
        })();

        queue.finish_scan();
        if let Err(err) = scan_result {
            record_whole_hash_error(&first_error, err);
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
    for (_, item) in hashed {
        apply_whole_hashed(state, item);
    }
    Ok(())
}

fn record_whole_hash_error(first_error: &Mutex<Option<Error>>, err: Error) {
    if let Ok(mut guard) = first_error.lock() {
        if guard.is_none() {
            *guard = Some(err);
        }
    }
}

fn apply_whole_hashed(state: &mut IndexBuildState<'_>, item: WholeHashed) {
    state.records.push(item.record);
    let retain_ofs = state.ofs_base_refs.get(&item.offset).copied().unwrap_or(0) > 0;
    let retain_oid = state.oid_base_refs.get(&item.oid).copied().unwrap_or(0) > 0;
    if retain_ofs || retain_oid {
        retain_pack_base_for_whole(
            item.offset,
            item.oid,
            item.kind,
            item.zlib_offset,
            item.uncompressed_size,
            state.ofs_base_refs,
            state.oid_base_refs,
            state.ofs_bases,
            state.oid_bases,
        );
    }
}

fn hash_one_whole_slot(
    pack: &[u8],
    work: &WholeSlot,
    algo: HashAlgo,
    ofs_base_refs: &HashMap<usize, u32>,
    oid_base_refs: &OidHashMap<u32>,
) -> Result<WholeHashed> {
    let mut scratch = ZlibInflateScratch::default();
    hash_one_whole_slot_with_scratch(pack, work, algo, ofs_base_refs, oid_base_refs, &mut scratch)
}

fn hash_one_whole_slot_with_scratch(
    pack: &[u8],
    work: &WholeSlot,
    algo: HashAlgo,
    ofs_base_refs: &HashMap<usize, u32>,
    oid_base_refs: &OidHashMap<u32>,
    scratch: &mut ZlibInflateScratch,
) -> Result<WholeHashed> {
    let oid = hash_whole_slot_oid_from_pack(pack, work, algo, scratch)?;
    hash_whole_slot_from_oid(pack, work, oid, algo, ofs_base_refs, oid_base_refs)
}

fn hash_whole_slot_oid_from_pack(
    pack: &[u8],
    work: &WholeSlot,
    algo: HashAlgo,
    scratch: &mut ZlibInflateScratch,
) -> Result<ObjectId> {
    use crate::hash::ObjectHasher;
    let len = u64::try_from(work.uncompressed_size).unwrap_or(u64::MAX);
    let mut hasher = ObjectHasher::for_object(algo, work.kind, len);
    let mut pos = work.zlib_offset;
    scratch.decompress_fixed_update(pack, &mut pos, len, |chunk| {
        hasher.update(chunk);
        Ok(())
    })?;
    hasher.finalize()
}

fn hash_whole_slot_from_oid(
    pack: &[u8],
    work: &WholeSlot,
    oid: ObjectId,
    algo: HashAlgo,
    _ofs_base_refs: &HashMap<usize, u32>,
    _oid_base_refs: &OidHashMap<u32>,
) -> Result<WholeHashed> {
    let _ = algo;
    Ok(WholeHashed {
        record: PackIndexRecord {
            oid,
            offset: u64::try_from(work.offset).unwrap_or(u64::MAX),
            crc32: crc32fast::hash(&pack[work.offset..work.pack_end]),
            kind: work.kind,
        },
        offset: work.offset,
        oid,
        kind: work.kind,
        zlib_offset: work.zlib_offset,
        uncompressed_size: work.uncompressed_size,
    })
}

#[allow(clippy::too_many_arguments)]
fn push_ofs_delta(
    pack: &[u8],
    rd: &mut PackReader<'_>,
    obj_offset: usize,
    size: usize,
    hb: usize,
    pending: &mut Vec<PendingIndexDelta>,
    state: &mut IndexBuildState<'_>,
    skip_scratch: &mut ZlibInflateScratch,
) -> Result<()> {
    let _ = hb;
    let neg = rd.read_ofs_neg_offset()?;
    let base_offset = obj_offset
        .checked_sub(neg)
        .ok_or_else(|| Error::CorruptObject("ofs-delta base offset underflow".to_owned()))?;
    *state.ofs_base_refs.entry(base_offset).or_insert(0) += 1;
    let zlib_offset = rd.pos;
    let consumed = skip_zlib_at_with_scratch(skip_scratch, pack, zlib_offset, size)?;
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

#[allow(clippy::too_many_arguments)]
fn push_ref_delta(
    pack: &[u8],
    rd: &mut PackReader<'_>,
    obj_offset: usize,
    size: usize,
    hb: usize,
    pending: &mut Vec<PendingIndexDelta>,
    state: &mut IndexBuildState<'_>,
    skip_scratch: &mut ZlibInflateScratch,
) -> Result<()> {
    let base_bytes = rd.read_exact(hb)?;
    let base_oid = ObjectId::from_bytes(base_bytes)?;
    *state.oid_base_refs.entry(base_oid).or_insert(0) += 1;
    let zlib_offset = rd.pos;
    let consumed = skip_zlib_at_with_scratch(skip_scratch, pack, zlib_offset, size)?;
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

fn verify_pack_trailer(pack: &[u8], consumed: usize, algo: HashAlgo) -> Result<()> {
    let hb = algo.len();
    let Some(body_end) = pack.len().checked_sub(hb) else {
        return Err(Error::CorruptObject("pack stream truncated".to_owned()));
    };
    if consumed != body_end {
        return Err(Error::CorruptObject(format!(
            "pack object stream ends at {consumed} but pack trailer starts at {body_end}"
        )));
    }
    let expected = algo.checksum_digest(&pack[..consumed]);
    let trailing = &pack[consumed..body_end + hb];
    if expected.as_slice() != trailing {
        return Err(Error::CorruptObject(
            "pack trailing checksum mismatch".to_owned(),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn retain_pack_base_for_whole(
    offset: usize,
    oid: ObjectId,
    kind: ObjectKind,
    zlib_offset: usize,
    uncompressed_size: usize,
    ofs_base_refs: &HashMap<usize, u32>,
    oid_base_refs: &OidHashMap<u32>,
    ofs_bases: &mut HashMap<usize, IndexBase>,
    oid_bases: &mut OidHashMap<IndexBase>,
) {
    let payload = BasePayload::InPack {
        zlib_offset,
        uncompressed_size,
    };
    let retain_ofs = ofs_base_refs.get(&offset).copied().unwrap_or(0) > 0;
    let retain_oid = oid_base_refs.get(&oid).copied().unwrap_or(0) > 0;
    let base = IndexBase { kind, payload };
    match (retain_ofs, retain_oid) {
        (true, true) => {
            ofs_bases.insert(offset, base.clone());
            oid_bases.insert(oid, base);
        }
        (true, false) => {
            ofs_bases.insert(offset, base);
        }
        (false, true) => {
            oid_bases.insert(oid, base);
        }
        (false, false) => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn retain_delta_pack_base_for_delta(
    offset: usize,
    oid: ObjectId,
    kind: ObjectKind,
    zlib_offset: usize,
    uncompressed_size: usize,
    base_offset: Option<usize>,
    base_oid: Option<ObjectId>,
    ofs_base_refs: &HashMap<usize, u32>,
    oid_base_refs: &OidHashMap<u32>,
    ofs_bases: &mut HashMap<usize, IndexBase>,
    oid_bases: &mut OidHashMap<IndexBase>,
) {
    let payload = BasePayload::DeltaInPack {
        zlib_offset,
        uncompressed_size,
        base_offset,
        base_oid,
    };
    let retain_ofs = ofs_base_refs.get(&offset).copied().unwrap_or(0) > 0;
    let retain_oid = oid_base_refs.get(&oid).copied().unwrap_or(0) > 0;
    let base = IndexBase { kind, payload };
    match (retain_ofs, retain_oid) {
        (true, true) => {
            ofs_bases.insert(offset, base.clone());
            oid_bases.insert(oid, base);
        }
        (true, false) => {
            ofs_bases.insert(offset, base);
        }
        (false, true) => {
            oid_bases.insert(oid, base);
        }
        (false, false) => {}
    }
}

/// Inflate a pack index base into `data`, using `op` for delta opcode scratch.
#[allow(clippy::too_many_arguments)]
fn inflate_index_base(
    pack: &[u8],
    base: &IndexBase,
    ofs_bases: &HashMap<usize, IndexBase>,
    oid_bases: &OidHashMap<IndexBase>,
    odb: &Odb,
    op: &mut Vec<u8>,
    data: &mut Vec<u8>,
    depth: usize,
) -> Result<ObjectKind> {
    if depth > MAX_DELTA_CHAIN_DEPTH {
        return Err(Error::CorruptObject(
            "delta chain too deep for pack index".to_owned(),
        ));
    }
    match &base.payload {
        BasePayload::InPack {
            zlib_offset,
            uncompressed_size,
        } => {
            data.clear();
            crate::pack_zlib::decompress_zlib_at_index_into(
                pack,
                *zlib_offset,
                *uncompressed_size,
                data,
            )?;
            Ok(base.kind)
        }
        BasePayload::DeltaInPack {
            zlib_offset,
            uncompressed_size,
            base_offset,
            base_oid,
        } => {
            crate::pack_zlib::decompress_zlib_at_index_into(
                pack,
                *zlib_offset,
                *uncompressed_size,
                op,
            )?;
            let ops = op.clone();
            let base_kind = if let Some(base_off) = base_offset {
                let Some(inner) = ofs_bases.get(base_off) else {
                    return Err(Error::CorruptObject("delta base not available".to_owned()));
                };
                inflate_index_base(pack, inner, ofs_bases, oid_bases, odb, op, data, depth + 1)?
            } else if let Some(ref base_id) = base_oid {
                if let Some(inner) = oid_bases.get(base_id) {
                    inflate_index_base(pack, inner, ofs_bases, oid_bases, odb, op, data, depth + 1)?
                } else if let Ok(obj) = odb.read(base_id) {
                    data.clear();
                    data.extend_from_slice(&obj.data);
                    obj.kind
                } else {
                    return Err(Error::CorruptObject("delta base not available".to_owned()));
                }
            } else {
                return Err(Error::CorruptObject("delta base not available".to_owned()));
            };
            let patched = apply_delta(data, &ops)?;
            *data = patched;
            Ok(base_kind)
        }
    }
}

struct DeltaHashed {
    record: PackIndexRecord,
    offset: usize,
    oid: ObjectId,
    kind: ObjectKind,
    zlib_offset: usize,
    uncompressed_size: usize,
    retain: bool,
    base_offset: Option<usize>,
    base_oid: Option<ObjectId>,
}

struct DeltaResolveScratch {
    delta: Vec<u8>,
    base: Vec<u8>,
    result: Vec<u8>,
}

impl DeltaResolveScratch {
    fn release_excess(&mut self) {
        for buf in [&mut self.delta, &mut self.base, &mut self.result] {
            if buf.capacity() > DELTA_SCRATCH_CAP {
                buf.shrink_to_fit();
            }
        }
    }
}

fn resolve_pending_deltas_parallel(
    pack: &[u8],
    threads: std::num::NonZeroUsize,
    algo: HashAlgo,
    pending: &mut Vec<PendingIndexDelta>,
    state: &mut IndexBuildState<'_>,
    mut progress: Option<&mut PackIndexReporter<'_>>,
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

        let ofs_out_refs = state.ofs_base_refs.clone();
        let oid_out_refs = state.oid_base_refs.clone();
        let ready_count = u64::try_from(ready.len()).unwrap_or(u64::MAX);
        let _ = threads;
        let mut scratch = DeltaResolveScratch {
            delta: Vec::new(),
            base: Vec::new(),
            result: Vec::new(),
        };
        for batch in ready.chunks(DELTA_RESOLVE_ROUND_CHUNK) {
            for delta in batch {
                let item = resolve_one_delta(
                    &mut scratch,
                    pack,
                    delta,
                    state.ofs_bases,
                    state.oid_bases,
                    state.odb,
                    algo,
                    &ofs_out_refs,
                    &oid_out_refs,
                )?;
                state.records.push(item.record);
                if item.retain {
                    retain_delta_pack_base_for_delta(
                        item.offset,
                        item.oid,
                        item.kind,
                        item.zlib_offset,
                        item.uncompressed_size,
                        item.base_offset,
                        item.base_oid,
                        state.ofs_base_refs,
                        state.oid_base_refs,
                        state.ofs_bases,
                        state.oid_bases,
                    );
                }
                if let Some(base_off) = item.base_offset {
                    if let Some(n) = state.ofs_base_refs.get_mut(&base_off) {
                        *n = n.saturating_sub(1);
                    }
                }
                if let Some(base_id) = item.base_oid {
                    if let Some(n) = state.oid_base_refs.get_mut(&base_id) {
                        *n = n.saturating_sub(1);
                    }
                }
            }
        }

        if let Some(r) = progress.as_mut() {
            r.on_deltas_resolved(ready_count);
        }
    }
    Ok(())
}

fn delta_base_ready(
    delta: &PendingIndexDelta,
    ofs_bases: &HashMap<usize, IndexBase>,
    oid_bases: &OidHashMap<IndexBase>,
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

#[allow(clippy::too_many_arguments)]
fn resolve_one_delta(
    scratch: &mut DeltaResolveScratch,
    pack: &[u8],
    delta: &PendingIndexDelta,
    ofs_bases: &HashMap<usize, IndexBase>,
    oid_bases: &OidHashMap<IndexBase>,
    odb: &Odb,
    algo: HashAlgo,
    ofs_out_refs: &HashMap<usize, u32>,
    oid_out_refs: &OidHashMap<u32>,
) -> Result<DeltaHashed> {
    crate::pack_zlib::decompress_zlib_at_index_into(
        pack,
        delta.zlib_offset,
        delta.uncompressed_size,
        &mut scratch.delta,
    )?;
    let base_kind = if let Some(base_off) = delta.base_offset {
        let Some(b) = ofs_bases.get(&base_off) else {
            return Err(Error::CorruptObject("delta base not available".to_owned()));
        };
        inflate_index_base(
            pack,
            b,
            ofs_bases,
            oid_bases,
            odb,
            &mut scratch.result,
            &mut scratch.base,
            0,
        )?
    } else if let Some(ref base_id) = delta.base_oid {
        if let Some(b) = oid_bases.get(base_id) {
            inflate_index_base(
                pack,
                b,
                ofs_bases,
                oid_bases,
                odb,
                &mut scratch.result,
                &mut scratch.base,
                0,
            )?
        } else if let Ok(obj) = odb.read(base_id) {
            scratch.base.clear();
            scratch.base.extend_from_slice(&obj.data);
            obj.kind
        } else {
            return Err(Error::CorruptObject("delta base not available".to_owned()));
        }
    } else {
        return Err(Error::CorruptObject("delta base not available".to_owned()));
    };
    apply_delta_into(&mut scratch.result, &scratch.base, &scratch.delta)?;
    let oid = hash_object(algo, base_kind, &scratch.result)?;
    let retain_ofs = ofs_out_refs.get(&delta.offset).copied().unwrap_or(0) > 0;
    let retain_oid = oid_out_refs.get(&oid).copied().unwrap_or(0) > 0;
    let retain = retain_ofs || retain_oid;
    scratch.result.clear();
    scratch.release_excess();
    Ok(DeltaHashed {
        record: PackIndexRecord {
            oid,
            offset: u64::try_from(delta.offset).unwrap_or(u64::MAX),
            crc32: delta.crc32,
            kind: base_kind,
        },
        offset: delta.offset,
        oid,
        kind: base_kind,
        zlib_offset: delta.zlib_offset,
        uncompressed_size: delta.uncompressed_size,
        retain,
        base_offset: delta.base_offset,
        base_oid: delta.base_oid,
    })
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
