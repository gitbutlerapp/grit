//! Read-ahead of queued trees for the pack-objects object walk.
//!
//! The walk ([`crate::pack_object_select`]) expands trees in the order they were queued,
//! and its queue only ever grows at the back. Inflating a tree does not depend on any
//! other tree, so worker threads can read the next runs of queued trees while the walk
//! parses the current ones. Results are handed back strictly in queue order, so the walk
//! processes every object exactly as if it had read it itself: the object list, its
//! order and the name hashes do not change.

use std::collections::{HashMap, VecDeque};
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::Scope;

use crate::error::Result;
use crate::objects::{Object, ObjectId};
use crate::odb::Odb;
use crate::pack::SingleLocalPackReader;

/// Queue entries handed to a worker at a time: a contiguous run keeps the delta bases
/// of neighbouring trees (successive versions of a directory) in one worker's cache.
const BATCH: usize = 256;

/// Batches in flight per worker, so a worker always has the next run ready to read.
const BATCHES_PER_WORKER: usize = 2;

/// One batch: its sequence number and, for each queue entry it covers, the object to
/// read (`None` for entries the walk reads itself).
type Job = (u64, Vec<Option<ObjectId>>);

/// A finished batch, or the panic payload of the worker that read it.
type Done = (u64, std::thread::Result<Vec<Option<Result<Object>>>>);

/// Something queued for the walk: the object id, and whether to read it ahead.
pub(crate) trait ReadAheadEntry {
    /// The object to read ahead, or `None` when the walk reads this entry itself.
    fn read_ahead_oid(&self) -> Option<ObjectId>;
}

/// Reads the front of the walk's queue ahead on scoped worker threads.
///
/// Workers start only once a full batch of trees is queued, so small walks (a push of
/// a few commits) never spawn threads.
pub(crate) struct TreeReadAhead<'scope, 'env> {
    scope: &'scope Scope<'scope, 'env>,
    odb: &'env Odb,
    workers: usize,
    jobs: Option<Sender<Job>>,
    done: Option<Receiver<Done>>,
    /// Results for the front of the queue, in queue order.
    ready: VecDeque<Option<Result<Object>>>,
    /// Batches that finished out of order, by sequence number.
    arrived: HashMap<u64, Vec<Option<Result<Object>>>>,
    next_seq: u64,
    want_seq: u64,
    /// Front queue entries covered by `ready` and the batches in flight.
    covered: usize,
}

impl<'scope, 'env> TreeReadAhead<'scope, 'env> {
    /// A read-ahead that will use up to `workers` threads of `scope`; `0` disables it.
    pub(crate) fn new(scope: &'scope Scope<'scope, 'env>, odb: &'env Odb, workers: usize) -> Self {
        Self {
            scope,
            odb,
            workers,
            jobs: None,
            done: None,
            ready: VecDeque::new(),
            arrived: HashMap::new(),
            next_seq: 0,
            want_seq: 0,
            covered: 0,
        }
    }

    /// Issue batches for the queue entries not yet covered, while workers have room.
    ///
    /// `queue` is the walk's queue as it is now; entries before the first uncovered one
    /// must not have been popped without [`Self::take_front`].
    pub(crate) fn top_up<E: ReadAheadEntry>(&mut self, queue: &VecDeque<E>) {
        if self.workers == 0 {
            return;
        }
        let limit = self.workers * BATCHES_PER_WORKER;
        while ((self.next_seq - self.want_seq) as usize) < limit
            && queue.len().saturating_sub(self.covered) >= BATCH
        {
            let batch: Vec<Option<ObjectId>> = queue
                .range(self.covered..self.covered + BATCH)
                .map(ReadAheadEntry::read_ahead_oid)
                .collect();
            let seq = self.next_seq;
            let Some(jobs) = self.start_workers() else {
                return;
            };
            if jobs.send((seq, batch)).is_err() {
                self.stop();
                return;
            }
            self.next_seq += 1;
            self.covered += BATCH;
        }
    }

    /// The result for the queue entry about to be popped: `Some` when a worker read it,
    /// `None` when the walk must read it itself (not covered, or not read ahead).
    pub(crate) fn take_front(&mut self) -> Option<Result<Object>> {
        if self.covered == 0 {
            return None;
        }
        if self.ready.is_empty() && !self.wait_for_next_batch() {
            return None;
        }
        self.covered -= 1;
        self.ready.pop_front().flatten()
    }

    /// Move batch `want_seq` into `ready`, waiting for it. `false` when the workers are
    /// gone; everything still covered is then read by the walk instead.
    fn wait_for_next_batch(&mut self) -> bool {
        loop {
            if let Some(batch) = self.arrived.remove(&self.want_seq) {
                self.ready.extend(batch);
                self.want_seq += 1;
                return true;
            }
            let Some(done) = self.done.as_ref() else {
                return false;
            };
            match done.recv() {
                Ok((seq, Ok(batch))) => {
                    self.arrived.insert(seq, batch);
                }
                Ok((_, Err(payload))) => resume_unwind(payload),
                Err(_) => {
                    self.stop();
                    return false;
                }
            }
        }
    }

    /// Stop reading ahead: nothing more is issued and covered entries are read by the walk.
    fn stop(&mut self) {
        self.workers = 0;
        self.jobs = None;
        self.done = None;
        self.ready.clear();
        self.arrived.clear();
        self.covered = 0;
    }

    fn start_workers(&mut self) -> Option<&Sender<Job>> {
        if self.jobs.is_none() {
            let (jobs_tx, jobs_rx) = channel::<Job>();
            let (done_tx, done_rx) = channel::<Done>();
            let jobs_rx = Arc::new(Mutex::new(jobs_rx));
            let odb = self.odb;
            for _ in 0..self.workers {
                let jobs_rx = Arc::clone(&jobs_rx);
                let done_tx = done_tx.clone();
                self.scope
                    .spawn(move || read_ahead_worker(odb, &jobs_rx, &done_tx));
            }
            self.jobs = Some(jobs_tx);
            self.done = Some(done_rx);
        }
        self.jobs.as_ref()
    }
}

fn read_ahead_worker(odb: &Odb, jobs: &Mutex<Receiver<Job>>, done: &Sender<Done>) {
    let _pack_context = odb.pack_read_context_guard();
    let mut reader = odb.open_session_pack_reader().ok().flatten();
    loop {
        let job = jobs.lock().unwrap_or_else(|e| e.into_inner()).recv();
        let Ok((seq, batch)) = job else {
            return;
        };
        let objects = catch_unwind(AssertUnwindSafe(|| {
            batch
                .iter()
                .map(|oid| oid.map(|oid| read_object(odb, reader.as_mut(), &oid)))
                .collect()
        }));
        if done.send((seq, objects)).is_err() {
            return;
        }
    }
}

/// Read `oid` through `reader` when it has it, otherwise through `odb`.
fn read_object(
    odb: &Odb,
    reader: Option<&mut SingleLocalPackReader>,
    oid: &ObjectId,
) -> Result<Object> {
    if let Some(Ok(obj)) = reader.map(|r| r.read_object(oid)) {
        return Ok(obj);
    }
    odb.read_once(oid)
}
