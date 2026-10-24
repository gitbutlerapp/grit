//! Ordered work-stealing pool over [`std::thread::scope`] with panic-safe slots.

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};

/// Items claimed per atomic work-stealing step (contiguous index range).
pub(crate) const DEFAULT_CHUNK: usize = 16;

pub(crate) struct Slots<R> {
    cells: Vec<UnsafeCell<MaybeUninit<R>>>,
    initialized: Vec<AtomicBool>,
}

impl<R> Slots<R> {
    pub(crate) fn new(len: usize) -> Self {
        Self {
            cells: (0..len)
                .map(|_| UnsafeCell::new(MaybeUninit::uninit()))
                .collect(),
            initialized: (0..len).map(|_| AtomicBool::new(false)).collect(),
        }
    }

    pub(crate) fn store(&self, index: usize, value: R) {
        // SAFETY: each index is written at most once while `initialized` is false.
        unsafe {
            (*self.cells[index].get()).write(value);
        }
        self.initialized[index].store(true, Ordering::Release);
    }

    pub(crate) fn drop_initialized(&self) {
        for i in 0..self.cells.len() {
            if self.initialized[i].swap(false, Ordering::AcqRel) {
                // SAFETY: `initialized` was true, so the slot holds a valid `R`.
                unsafe {
                    (*self.cells[i].get()).assume_init_drop();
                }
            }
        }
    }

    pub(crate) fn into_vec(self) -> Vec<R> {
        let Self {
            mut cells,
            initialized,
        } = self;
        let mut out = Vec::with_capacity(cells.len());
        for (cell, was_init) in cells.drain(..).zip(initialized) {
            assert!(
                was_init.into_inner(),
                "parallel pool success path left an uninitialized slot"
            );
            // SAFETY: success path initializes every index exactly once.
            unsafe {
                out.push(cell.into_inner().assume_init());
            }
        }
        out
    }
}

unsafe impl<R: Send> Sync for Slots<R> {}

/// Run `f` over `items` with up to `n_workers` scope threads, preserving input order.
///
/// When a worker returns `Err`, earlier results are dropped before returning.
/// Worker panics resume on the caller thread after join.
pub(crate) fn run_ordered_pool<T, R, E, F>(
    items: &[T],
    n_workers: usize,
    chunk: usize,
    f: F,
) -> Result<Vec<R>, E>
where
    T: Sync,
    R: Send,
    E: Send + 'static,
    F: Fn(&T) -> Result<R, E> + Sync,
{
    let len = items.len();
    let slots: Arc<Slots<R>> = Arc::new(Slots::new(len));

    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let first_error: Arc<Mutex<Option<E>>> = Arc::new(Mutex::new(None));
    let panic_payload: Arc<Mutex<Option<Box<dyn std::any::Any + Send>>>> =
        Arc::new(Mutex::new(None));

    let record_task_error = |err: E, first_error: &Mutex<Option<E>>| {
        if let Ok(mut guard) = first_error.lock() {
            if guard.is_none() {
                *guard = Some(err);
            }
        }
        failed.store(true, Ordering::Release);
    };

    let run_worker =
        |slots: Arc<Slots<R>>,
         first_error: Arc<Mutex<Option<E>>>,
         panic_payload: Arc<Mutex<Option<Box<dyn std::any::Any + Send>>>>| {
            loop {
                if failed.load(Ordering::Acquire) {
                    return;
                }
                let start = next.fetch_add(chunk, Ordering::AcqRel);
                if start >= len {
                    return;
                }
                let end = (start + chunk).min(len);
                for (offset, item) in items[start..end].iter().enumerate() {
                    if failed.load(Ordering::Acquire) {
                        return;
                    }
                    let i = start + offset;
                    match catch_unwind(AssertUnwindSafe(|| f(item))) {
                        Ok(Ok(value)) => slots.store(i, value),
                        Ok(Err(e)) => record_task_error(e, &first_error),
                        Err(payload) => {
                            if let Ok(mut guard) = panic_payload.lock() {
                                if guard.is_none() {
                                    *guard = Some(payload);
                                }
                            }
                            failed.store(true, Ordering::Release);
                        }
                    }
                }
            }
        };

    std::thread::scope(|scope| {
        for _ in 1..n_workers {
            let slots = Arc::clone(&slots);
            let first_error = Arc::clone(&first_error);
            let panic_payload = Arc::clone(&panic_payload);
            scope.spawn(move || run_worker(slots, first_error, panic_payload));
        }
        run_worker(
            Arc::clone(&slots),
            Arc::clone(&first_error),
            Arc::clone(&panic_payload),
        );
    });

    if let Ok(mut guard) = panic_payload.lock() {
        if let Some(payload) = guard.take() {
            slots.drop_initialized();
            resume_unwind(payload);
        }
    }

    if let Ok(mut guard) = first_error.lock() {
        if let Some(e) = guard.take() {
            drop(guard);
            slots.drop_initialized();
            return Err(e);
        }
    }

    let slots = match Arc::try_unwrap(slots) {
        Ok(s) => s,
        Err(arc) => {
            arc.drop_initialized();
            panic!("parallel slot buffer still shared after scope join");
        }
    };
    Ok(slots.into_vec())
}

/// Like [`run_ordered_pool`], but each worker calls `init()` once and reuses `S` for its chunks.
pub(crate) fn run_ordered_pool_with_local<T, R, E, S, Init, F>(
    items: &[T],
    n_workers: usize,
    chunk: usize,
    init: Init,
    f: F,
) -> Result<Vec<R>, E>
where
    T: Sync,
    R: Send,
    E: Send + 'static,
    S: Send,
    Init: Fn() -> S + Sync,
    F: Fn(&mut S, &T) -> Result<R, E> + Sync,
{
    let len = items.len();
    let slots: Arc<Slots<R>> = Arc::new(Slots::new(len));

    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let first_error: Arc<Mutex<Option<E>>> = Arc::new(Mutex::new(None));
    let panic_payload: Arc<Mutex<Option<Box<dyn std::any::Any + Send>>>> =
        Arc::new(Mutex::new(None));

    let record_task_error = |err: E, first_error: &Mutex<Option<E>>| {
        if let Ok(mut guard) = first_error.lock() {
            if guard.is_none() {
                *guard = Some(err);
            }
        }
        failed.store(true, Ordering::Release);
    };

    let run_worker =
        |slots: Arc<Slots<R>>,
         first_error: Arc<Mutex<Option<E>>>,
         panic_payload: Arc<Mutex<Option<Box<dyn std::any::Any + Send>>>>| {
            let mut state = init();
            loop {
                if failed.load(Ordering::Acquire) {
                    return;
                }
                let start = next.fetch_add(chunk, Ordering::AcqRel);
                if start >= len {
                    return;
                }
                let end = (start + chunk).min(len);
                for (offset, item) in items[start..end].iter().enumerate() {
                    if failed.load(Ordering::Acquire) {
                        return;
                    }
                    let i = start + offset;
                    match catch_unwind(AssertUnwindSafe(|| f(&mut state, item))) {
                        Ok(Ok(value)) => slots.store(i, value),
                        Ok(Err(e)) => record_task_error(e, &first_error),
                        Err(payload) => {
                            if let Ok(mut guard) = panic_payload.lock() {
                                if guard.is_none() {
                                    *guard = Some(payload);
                                }
                            }
                            failed.store(true, Ordering::Release);
                        }
                    }
                }
            }
        };

    std::thread::scope(|scope| {
        for _ in 1..n_workers {
            let slots = Arc::clone(&slots);
            let first_error = Arc::clone(&first_error);
            let panic_payload = Arc::clone(&panic_payload);
            scope.spawn(move || run_worker(slots, first_error, panic_payload));
        }
        run_worker(
            Arc::clone(&slots),
            Arc::clone(&first_error),
            Arc::clone(&panic_payload),
        );
    });

    if let Ok(mut guard) = panic_payload.lock() {
        if let Some(payload) = guard.take() {
            slots.drop_initialized();
            resume_unwind(payload);
        }
    }

    if let Ok(mut guard) = first_error.lock() {
        if let Some(e) = guard.take() {
            drop(guard);
            slots.drop_initialized();
            return Err(e);
        }
    }

    let slots = match Arc::try_unwrap(slots) {
        Ok(s) => s,
        Err(arc) => {
            arc.drop_initialized();
            panic!("parallel slot buffer still shared after scope join");
        }
    };
    Ok(slots.into_vec())
}

/// Hand-off state between [`run_ordered_batches`] workers and its consumer.
struct BatchGate {
    /// Batches already passed to the consumer.
    consumed: usize,
    /// Set when the consumer finished early or a worker panicked.
    stop: bool,
}

/// Stops the other workers when a worker unwinds, so the consumer is not left waiting.
struct StopOnPanic<'a> {
    gate: &'a Mutex<BatchGate>,
    wake: &'a Condvar,
}

impl Drop for StopOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.gate.lock().unwrap_or_else(|e| e.into_inner()).stop = true;
            self.wake.notify_all();
        }
    }
}

/// Map `items` in consecutive batches on `n_workers` long-lived worker threads and
/// pass each batch's results to `consume` on the calling thread, in item order.
///
/// Each worker calls `init()` once on its own thread and reuses the state for every
/// batch it maps, so per-thread caches survive across batches. Workers run at most
/// `2 * n_workers` batches ahead of `consume`, which bounds memory to a few batches
/// of results while keeping every worker busy.
///
/// # Errors
///
/// Returns the first error received from `f` or returned by `consume`; remaining
/// batches are abandoned. A worker panic is resumed on the calling thread.
pub(crate) fn run_ordered_batches<T, R, E, S, Init, F, C>(
    items: &[T],
    batch: usize,
    n_workers: usize,
    init: Init,
    f: F,
    mut consume: C,
) -> Result<(), E>
where
    T: Sync,
    R: Send,
    E: Send,
    Init: Fn() -> S + Sync,
    F: Fn(&mut S, &T) -> Result<R, E> + Sync,
    C: FnMut(&[T], Vec<R>) -> Result<(), E>,
{
    let batch = batch.max(1);
    let n_batches = items.len().div_ceil(batch);
    let ahead = 2 * n_workers.max(1);
    let batch_items = |k: usize| &items[k * batch..((k + 1) * batch).min(items.len())];
    let next = AtomicUsize::new(0);
    let gate = Mutex::new(BatchGate {
        consumed: 0,
        stop: false,
    });
    let wake = Condvar::new();
    let (tx, rx) = mpsc::channel::<(usize, Result<Vec<R>, E>)>();

    std::thread::scope(|scope| {
        for _ in 0..n_workers.max(1) {
            let tx = tx.clone();
            let (next, gate, wake, init, f) = (&next, &gate, &wake, &init, &f);
            scope.spawn(move || {
                let _stop_on_panic = StopOnPanic { gate, wake };
                let mut state = init();
                loop {
                    let k = next.fetch_add(1, Ordering::AcqRel);
                    if k >= n_batches {
                        return;
                    }
                    {
                        let mut g = gate.lock().unwrap_or_else(|e| e.into_inner());
                        while !g.stop && k >= g.consumed + ahead {
                            g = wake.wait(g).unwrap_or_else(|e| e.into_inner());
                        }
                        if g.stop {
                            return;
                        }
                    }
                    let result = batch_items(k)
                        .iter()
                        .map(|item| f(&mut state, item))
                        .collect::<Result<Vec<R>, E>>();
                    let failed = result.is_err();
                    if tx.send((k, result)).is_err() || failed {
                        return;
                    }
                }
            });
        }
        drop(tx);

        let mut ready: std::collections::BTreeMap<usize, Vec<R>> = Default::default();
        let mut outcome = Ok(());
        let mut want = 0;
        while want < n_batches {
            if let Some(results) = ready.remove(&want) {
                if let Err(e) = consume(batch_items(want), results) {
                    outcome = Err(e);
                    break;
                }
                want += 1;
                gate.lock().unwrap_or_else(|e| e.into_inner()).consumed = want;
                wake.notify_all();
                continue;
            }
            match rx.recv() {
                Ok((k, Ok(results))) => {
                    ready.insert(k, results);
                }
                Ok((_, Err(e))) => {
                    outcome = Err(e);
                    break;
                }
                // Every worker exited without delivering `want`: one panicked, and the
                // scope resumes that panic when it joins.
                Err(_) => break,
            }
        }
        gate.lock().unwrap_or_else(|e| e.into_inner()).stop = true;
        wake.notify_all();
        outcome
    })
}
