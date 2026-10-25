//! Build v2 pack index records from an in-memory pack (index-pack hashing path).
//!
//! Multi-threaded builds overlap zlib-skip (boundary scan) with parallel inflate+hash
//! on batched whole objects. Single-threaded builds inflate and hash each whole object
//! once in pack order.
//!
//! Deltas are resolved depth-first from each base that has delta children: a worker
//! inflates the base once, then walks its delta tree applying each delta exactly once.
//! Only the bytes of the chain currently being walked are held, a parent is released
//! as soon as its last child is taken, and a per-worker byte cap evicts ancestors that
//! are rebuilt from the pack if they are needed again. Peak memory therefore follows the
//! deepest chain rather than the number of deltas waiting on resolved bases.
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
use crate::pack_map::{PackData, ResidentBudget};
use crate::pack_zlib::skip_zlib_at_with_scratch;
use crate::progress::{
    cancel_watch_arc, poll_cancel, poll_watch, Cancel, ProgressPhase, ProgressSink,
};
use crate::unpack_objects::{apply_delta_into, type_code_to_kind, PackIndexRecord, PackReader};
use crate::zlib_inflate::ZlibInflateScratch;
use std::borrow::Cow;
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

/// Where a delta's base lives: an earlier pack offset (ofs-delta) or an object id (ref-delta).
#[derive(Clone, Copy)]
enum DeltaBase {
    Offset(usize),
    Oid(ObjectId),
}

/// A delta slot found by the scan; its base is recorded in [`DeltaChildren`].
///
/// Kept small because a large pack holds hundreds of thousands of these at once.
struct PendingIndexDelta {
    offset: usize,
    /// End of the object's zlib stream in the pack.
    pack_end: usize,
    uncompressed_size: usize,
    crc32: u32,
    /// Bytes from `offset` to the zlib stream (object header plus base reference).
    header_len: u8,
}

impl PendingIndexDelta {
    fn zlib_offset(&self) -> usize {
        self.offset + usize::from(self.header_len)
    }
}

/// Where delta resolution reads compressed objects from.
#[derive(Clone, Copy)]
pub(crate) enum PackBytes<'a> {
    /// The pack bytes in memory; reads borrow from them.
    Slice(&'a [u8]),
    /// The pack file, read with positioned reads so random access adds no resident pages.
    File(&'a std::fs::File),
}

impl PackBytes<'_> {
    /// Bytes `start..end` of the pack, borrowed or read into `buf`.
    fn read<'s>(&'s self, start: usize, end: usize, buf: &'s mut Vec<u8>) -> Result<&'s [u8]> {
        match self {
            Self::Slice(pack) => pack
                .get(start..end)
                .ok_or_else(|| Error::CorruptObject("pack object out of range".to_owned())),
            Self::File(file) => {
                buf.clear();
                buf.resize(end.saturating_sub(start), 0);
                read_exact_at(file, buf, start as u64).map_err(Error::Io)?;
                Ok(buf)
            }
        }
    }
}

#[cfg(unix)]
fn read_exact_at(file: &std::fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &std::fs::File, mut buf: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    while !buf.is_empty() {
        match std::os::windows::fs::FileExt::seek_read(file, buf, offset) {
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn read_exact_at(_file: &std::fs::File, _buf: &mut [u8], _offset: u64) -> std::io::Result<()> {
    Err(std::io::ErrorKind::Unsupported.into())
}

/// Deltas found by the pack scan, each indexed under its base, plus where each whole
/// object's zlib stream ends (aligned with the whole-object records, in pack order).
#[derive(Default)]
struct PendingDeltas {
    deltas: Vec<PendingIndexDelta>,
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
        let header_len = u8::try_from(zlib_offset - offset)
            .map_err(|_| Error::CorruptObject("pack object header too long".to_owned()))?;
        match base {
            DeltaBase::Offset(base) => self.children.by_offset.push((base, index)),
            DeltaBase::Oid(oid) => self.children.by_oid.push((oid, index)),
        }
        self.deltas.push(PendingIndexDelta {
            offset,
            pack_end,
            uncompressed_size,
            crc32: crc32fast::hash(&pack[offset..pack_end]),
            header_len,
        });
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
/// Soft cap on ancestor bytes one delta worker keeps for the chain it is walking.
///
/// The base being applied right now is always kept, so a single object larger than
/// the cap still resolves; older ancestors beyond the cap are rebuilt on demand.
const DELTA_CHAIN_BYTES_PER_WORKER: usize = 16 * 1024 * 1024;
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
/// Returns [`Error::CorruptObject`] for a malformed pack, a bad trailer (when
/// `verify_trailer`), or deltas whose bases are in neither the pack nor `odb`
/// (the message then contains `could not be resolved for pack index`).
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
        &pending.deltas,
        &pending.children,
        &pending.whole_ends,
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

fn record_first_error(first_error: &Mutex<Option<Error>>, err: Error) {
    if let Ok(mut guard) = first_error.lock() {
        if guard.is_none() {
            *guard = Some(err);
        }
    }
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

/// Delta indices grouped by base, sorted (by [`Self::sort`]) so each base's children
/// form one slice.
#[derive(Default)]
struct DeltaChildren {
    by_offset: Vec<(usize, u32)>,
    by_oid: Vec<(ObjectId, u32)>,
}

impl DeltaChildren {
    fn sort(&mut self) {
        self.by_offset.sort_unstable();
        self.by_oid.sort_unstable();
    }

    fn of_offset(&self, offset: usize) -> &[(usize, u32)] {
        let start = self.by_offset.partition_point(|(base, _)| *base < offset);
        let len = self.by_offset[start..].partition_point(|(base, _)| *base == offset);
        &self.by_offset[start..start + len]
    }

    fn of_oid(&self, oid: &ObjectId) -> &[(ObjectId, u32)] {
        let start = self.by_oid.partition_point(|(base, _)| base < oid);
        let len = self.by_oid[start..].partition_point(|(base, _)| base == oid);
        &self.by_oid[start..start + len]
    }
}

/// A base object whose delta tree one worker resolves depth-first.
enum DeltaRoot {
    /// A whole object in this pack (header at `offset`, zlib stream ending at `end`).
    Pack {
        offset: usize,
        end: usize,
        oid: ObjectId,
    },
    /// A thin-pack base that only exists in the object database.
    Odb(ObjectId),
}

/// How to rebuild a chain frame whose bytes were evicted.
#[derive(Clone, Copy)]
enum Anchor {
    /// Start from the walk's root object.
    Root,
    /// Start from the frame at this stack index.
    Frame(usize),
}

/// One object on the chain a worker is walking, with the children still to resolve.
struct ChainFrame<'c> {
    kind: ObjectKind,
    data: Option<Vec<u8>>,
    ofs_children: &'c [(usize, u32)],
    ref_children: &'c [(ObjectId, u32)],
    next_child: usize,
    anchor: Anchor,
    /// Deltas applied, in order, to the anchor's bytes to rebuild this frame. Parents
    /// released after their last child leave their delta here instead of a stack slot.
    via: Vec<u32>,
}

impl ChainFrame<'_> {
    fn take_next_child(&mut self) -> Option<u32> {
        let k = self.next_child;
        let child = match self.ofs_children.get(k) {
            Some(&(_, child)) => child,
            None => self.ref_children.get(k - self.ofs_children.len())?.1,
        };
        self.next_child += 1;
        Some(child)
    }

    fn is_exhausted(&self) -> bool {
        self.next_child >= self.ofs_children.len() + self.ref_children.len()
    }

    fn held_bytes(&self) -> usize {
        self.data.as_ref().map_or(0, Vec::capacity)
    }
}

/// Buffers one delta worker reuses across objects.
#[derive(Default)]
struct DeltaScratch {
    zlib: ZlibInflateScratch,
    /// Compressed bytes of the object being inflated (file reads only).
    packed: Vec<u8>,
    delta: Vec<u8>,
    leaf: Vec<u8>,
}

/// Shared, read-only view of the pack's deltas for the resolving workers.
struct DeltaResolver<'a> {
    source: PackBytes<'a>,
    odb: &'a Odb,
    algo: HashAlgo,
    pending: &'a [PendingIndexDelta],
    children: &'a DeltaChildren,
    /// Set by the worker that resolves a delta, so a base duplicated in the pack (or
    /// reached again as a thin base) never resolves the same ref-delta twice.
    claimed: &'a [AtomicBool],
    resolved: &'a AtomicU64,
    failed: &'a AtomicBool,
    chain_limit: usize,
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
    pending: &[PendingIndexDelta],
    children: &DeltaChildren,
    whole_ends: &[usize],
    records: &mut Vec<PackIndexRecord>,
    mut progress: Option<&mut PackIndexReporter<'_>>,
    chain_limit: usize,
    cancel: Option<&dyn Cancel>,
    cancel_watch: Option<&Arc<AtomicBool>>,
) -> Result<Vec<ObjectId>> {
    if pending.is_empty() {
        return Ok(Vec::new());
    }
    poll_index_build_cancel(cancel, None, cancel_watch)?;
    let claimed: Vec<AtomicBool> = pending.iter().map(|_| AtomicBool::new(false)).collect();
    let resolved = AtomicU64::new(0);
    let failed = AtomicBool::new(false);
    let resolver = DeltaResolver {
        source,
        odb,
        algo,
        pending,
        children,
        claimed: &claimed,
        resolved: &resolved,
        failed: &failed,
        chain_limit,
    };

    let in_pack: Vec<DeltaRoot> = records
        .iter()
        .zip(whole_ends)
        .filter_map(|(record, &end)| {
            let offset = usize::try_from(record.offset).ok()?;
            let has_children =
                !children.of_offset(offset).is_empty() || !children.of_oid(&record.oid).is_empty();
            has_children.then_some(DeltaRoot::Pack {
                offset,
                end,
                oid: record.oid,
            })
        })
        .collect();
    resolver.run(&in_pack, threads, records, progress.as_deref_mut())?;

    let thin: Vec<DeltaRoot> = children
        .by_oid
        .chunk_by(|a, b| a.0 == b.0)
        .filter(|group| {
            group
                .iter()
                .any(|(_, child)| !claimed[*child as usize].load(Ordering::Relaxed))
        })
        .map(|group| DeltaRoot::Odb(group[0].0))
        .collect();
    resolver.run(&thin, threads, records, progress)?;

    let total = u64::try_from(pending.len()).unwrap_or(u64::MAX);
    let unresolved = total.saturating_sub(resolved.load(Ordering::Relaxed));
    if unresolved > 0 {
        return Err(Error::CorruptObject(format!(
            "{unresolved} delta(s) could not be resolved for pack index"
        )));
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

impl DeltaResolver<'_> {
    /// Walk `roots` on up to `threads` workers; the caller thread works too and reports progress.
    fn run(
        &self,
        roots: &[DeltaRoot],
        threads: std::num::NonZeroUsize,
        records: &mut Vec<PackIndexRecord>,
        mut progress: Option<&mut PackIndexReporter<'_>>,
    ) -> Result<()> {
        if roots.is_empty() {
            return Ok(());
        }
        // Workers hand records over in small batches, so per-thread buffers stay bounded
        // instead of each growing to its share of the pack.
        const RECORD_BATCH: usize = 1024;
        let next_root = AtomicUsize::new(0);
        let first_error: Mutex<Option<Error>> = Mutex::new(None);
        let shared = Mutex::new(std::mem::take(records));
        let flush = |batch: &mut Vec<PackIndexRecord>| {
            shared
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .append(batch);
        };
        let helpers = threads.get().min(roots.len()).saturating_sub(1);
        let work = |after_root: &mut dyn FnMut()| {
            let mut scratch = DeltaScratch::default();
            let mut batch = Vec::with_capacity(RECORD_BATCH);
            while !self.failed.load(Ordering::Relaxed) {
                let Some(root) = roots.get(next_root.fetch_add(1, Ordering::Relaxed)) else {
                    break;
                };
                if let Err(err) = self.resolve_root(root, &mut scratch, &mut batch) {
                    record_first_error(&first_error, err);
                    self.failed.store(true, Ordering::Relaxed);
                }
                if batch.len() >= RECORD_BATCH {
                    flush(&mut batch);
                }
                after_root();
            }
            flush(&mut batch);
        };
        thread::scope(|scope| {
            let work = &work;
            let handles: Vec<_> = (0..helpers)
                .map(|_| scope.spawn(move || work(&mut || {})))
                .collect();
            work(&mut || {
                if let Some(r) = progress.as_mut() {
                    r.set_deltas_resolved(self.resolved.load(Ordering::Relaxed));
                }
            });
            for handle in handles {
                if handle.join().is_err() {
                    record_first_error(
                        &first_error,
                        Error::CorruptObject("index-pack delta worker stopped".to_owned()),
                    );
                }
            }
        });
        *records = shared.into_inner().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = progress.as_mut() {
            r.set_deltas_resolved(self.resolved.load(Ordering::Relaxed));
        }
        match first_error.into_inner().unwrap_or_else(|e| e.into_inner()) {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// Resolve every delta reachable from `root`, depth-first.
    fn resolve_root(
        &self,
        root: &DeltaRoot,
        scratch: &mut DeltaScratch,
        records: &mut Vec<PackIndexRecord>,
    ) -> Result<()> {
        let Some((kind, data)) = self.load_root(root, &mut scratch.zlib, &mut scratch.packed)?
        else {
            return Ok(());
        };
        let (ofs_children, ref_children) = match root {
            DeltaRoot::Pack { offset, oid, .. } => {
                (self.children.of_offset(*offset), self.children.of_oid(oid))
            }
            DeltaRoot::Odb(oid) => (&[][..], self.children.of_oid(oid)),
        };
        let mut held = data.capacity();
        let mut stack = vec![ChainFrame {
            kind,
            data: Some(data),
            ofs_children,
            ref_children,
            next_child: 0,
            anchor: Anchor::Root,
            via: Vec::new(),
        }];
        while let Some(top) = stack.len().checked_sub(1) {
            if self.failed.load(Ordering::Relaxed) {
                return Ok(());
            }
            let Some(child) = stack[top].take_next_child() else {
                if let Some(done) = stack.pop() {
                    held = held.saturating_sub(done.held_bytes());
                }
                continue;
            };
            if self.claimed[child as usize].swap(true, Ordering::AcqRel) {
                continue;
            }
            if stack[top].data.is_none() {
                let data = self.rebuild(&stack, top, root, scratch)?;
                held += data.capacity();
                stack[top].data = Some(data);
            }
            let delta = &self.pending[child as usize];
            let kind = stack[top].kind;
            let ofs_children = self.children.of_offset(delta.offset);
            // Results that become bases get their own exact allocation; leaves reuse one buffer.
            let mut result = if ofs_children.is_empty() {
                std::mem::take(&mut scratch.leaf)
            } else {
                Vec::new()
            };
            let base = stack[top].data.as_deref().unwrap_or_default();
            self.apply(
                delta,
                base,
                &mut scratch.zlib,
                &mut scratch.packed,
                &mut scratch.delta,
                &mut result,
            )?;
            let oid = hash_object(self.algo, kind, &result)?;
            records.push(PackIndexRecord {
                oid,
                offset: u64::try_from(delta.offset).unwrap_or(u64::MAX),
                crc32: delta.crc32,
                kind,
            });
            self.resolved.fetch_add(1, Ordering::Relaxed);
            let ref_children = self.children.of_oid(&oid);
            if ofs_children.is_empty() && ref_children.is_empty() {
                scratch.leaf = result;
                continue;
            }
            result.shrink_to_fit();
            let exhausted = stack[top].is_exhausted();
            let (anchor, via) = match exhausted.then(|| stack.pop()).flatten() {
                Some(parent) => {
                    held = held.saturating_sub(parent.held_bytes());
                    let mut via = parent.via;
                    via.push(child);
                    (parent.anchor, via)
                }
                None => (Anchor::Frame(top), vec![child]),
            };
            held += result.capacity();
            stack.push(ChainFrame {
                kind,
                data: Some(result),
                ofs_children,
                ref_children,
                next_child: 0,
                anchor,
                via,
            });
            held = evict_ancestors(&mut stack, held, self.chain_limit);
        }
        Ok(())
    }

    /// Bytes and kind of `root`, or `None` for a thin base the object database lacks.
    fn load_root(
        &self,
        root: &DeltaRoot,
        zlib: &mut ZlibInflateScratch,
        packed: &mut Vec<u8>,
    ) -> Result<Option<(ObjectKind, Vec<u8>)>> {
        match root {
            DeltaRoot::Pack { offset, end, .. } => {
                let bytes = self.source.read(*offset, *end, packed)?;
                let mut rd = PackReader::new(bytes);
                let (type_code, size) = rd.read_type_size()?;
                let kind = type_code_to_kind(type_code)?;
                let mut data = Vec::new();
                let size = u64::try_from(size).unwrap_or(u64::MAX);
                zlib.decompress_fixed_into(bytes, &mut rd.pos, size, &mut data)?;
                Ok(Some((kind, data)))
            }
            DeltaRoot::Odb(oid) => Ok(self.odb.read(oid).ok().map(|obj| (obj.kind, obj.data))),
        }
    }

    /// Recompute the bytes of the evicted frame at `index` from its anchor.
    fn rebuild(
        &self,
        stack: &[ChainFrame<'_>],
        index: usize,
        root: &DeltaRoot,
        scratch: &mut DeltaScratch,
    ) -> Result<Vec<u8>> {
        let frame = &stack[index];
        let mut current: Cow<'_, [u8]> = match frame.anchor {
            Anchor::Root => match self.load_root(root, &mut scratch.zlib, &mut scratch.packed)? {
                Some((_, data)) => Cow::Owned(data),
                None => {
                    return Err(Error::CorruptObject("delta base not available".to_owned()));
                }
            },
            Anchor::Frame(parent) => match stack[parent].data.as_deref() {
                Some(data) => Cow::Borrowed(data),
                None => Cow::Owned(self.rebuild(stack, parent, root, scratch)?),
            },
        };
        for &step in &frame.via {
            let mut out = Vec::new();
            self.apply(
                &self.pending[step as usize],
                &current,
                &mut scratch.zlib,
                &mut scratch.packed,
                &mut scratch.delta,
                &mut out,
            )?;
            current = Cow::Owned(out);
        }
        Ok(current.into_owned())
    }

    #[allow(clippy::too_many_arguments)]
    fn apply(
        &self,
        delta: &PendingIndexDelta,
        base: &[u8],
        zlib: &mut ZlibInflateScratch,
        packed: &mut Vec<u8>,
        delta_buf: &mut Vec<u8>,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let bytes = self
            .source
            .read(delta.zlib_offset(), delta.pack_end, packed)?;
        let size = u64::try_from(delta.uncompressed_size).unwrap_or(u64::MAX);
        zlib.decompress_fixed_into(bytes, &mut 0, size, delta_buf)?;
        apply_delta_into(out, base, delta_buf)
    }
}

/// Drop the bytes of the oldest ancestors until the chain fits in `limit`; the frame on
/// top is always kept. Returns the bytes still held.
fn evict_ancestors(stack: &mut [ChainFrame<'_>], mut held: usize, limit: usize) -> usize {
    let Some(keep) = stack.len().checked_sub(1) else {
        return held;
    };
    for frame in &mut stack[..keep] {
        if held <= limit {
            break;
        }
        if let Some(data) = frame.data.take() {
            held = held.saturating_sub(data.capacity());
        }
    }
    held
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
                Err(Error::CorruptObject(msg)) => assert!(
                    msg.contains("2 delta(s) could not be resolved for pack index"),
                    "threads={threads}: {msg}"
                ),
                other => panic!("threads={threads}: expected unresolved deltas, got {other:?}"),
            }
        }
    }

    #[test]
    fn evict_ancestors_keeps_the_frame_being_applied() {
        let frame = |len: usize| ChainFrame {
            kind: ObjectKind::Blob,
            data: Some(vec![0; len]),
            ofs_children: &[],
            ref_children: &[],
            next_child: 0,
            anchor: Anchor::Root,
            via: Vec::new(),
        };
        let mut stack = vec![frame(10), frame(20), frame(30)];
        let held: usize = stack.iter().map(ChainFrame::held_bytes).sum();
        assert_eq!(evict_ancestors(&mut stack, held, 35), 30);
        assert!(stack[0].data.is_none() && stack[1].data.is_none());
        assert!(stack[2].data.is_some());
        let mut stack = vec![frame(10), frame(20), frame(30)];
        assert_eq!(evict_ancestors(&mut stack, held, 50), 50);
        assert!(stack[0].data.is_none() && stack[1].data.is_some());
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
