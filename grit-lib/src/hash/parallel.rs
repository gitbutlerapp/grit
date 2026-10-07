//! Parallel batch hashing built on [`std::thread::scope`].
//!
//! Thread count is always explicit (see [`Parallelism`]); there is no global pool
//! and no environment reads in this module.

use std::cell::UnsafeCell;
use std::convert::Infallible;
use std::mem::MaybeUninit;
use std::num::NonZeroUsize;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::objects::{HashAlgo, ObjectId, ObjectKind};

use super::hash_object;

/// Minimum item count before parallel helpers use more than one thread.
///
/// Measured on the factory VM (2026-10): parallel `hash_object` for 16 KiB blobs
/// breaks even near 24 items at 8 threads vs a serial loop; 32 adds margin for
/// scope spawn overhead.
pub const PAR_HASH_MIN_ITEMS: usize = 32;

/// Minimum total payload bytes (summed over items) for parallel hashing.
///
/// With fewer than this many bytes, thread startup dominates even for larger
/// per-item counts.
pub const PAR_HASH_MIN_TOTAL_BYTES: usize = 256 * 1024;

/// Items claimed per atomic work-stealing step (contiguous index range).
const PAR_HASH_CHUNK: usize = 16;

/// Failure from [`try_par_hash_with`] when a closure returns an error.
///
/// If a worker panics, initialized partial results are dropped and the panic is
/// **resumed on the caller thread** after workers join; this enum is not used
/// for that case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParallelHashError<E> {
    /// The first error returned by the closure (additional concurrent errors are discarded).
    Task(E),
}

impl<E: std::fmt::Display> std::fmt::Display for ParallelHashError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Task(e) => write!(f, "{e}"),
        }
    }
}

impl<E: std::fmt::Debug + std::fmt::Display> std::error::Error for ParallelHashError<E> {}

/// Resolved worker thread count for parallel hash helpers.
///
/// Config keys such as `pack.threads` / `index.threads` are interpreted by
/// callers; this type only clamps an optional request to
/// [`std::thread::available_parallelism`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Parallelism {
    threads: NonZeroUsize,
}

impl Parallelism {
    /// Use exactly `threads` workers (must be at least 1).
    #[must_use]
    pub fn new(threads: NonZeroUsize) -> Self {
        Self { threads }
    }

    /// Resolve a thread count from config (`None` or `Some(0)` → all logical CPUs).
    #[must_use]
    pub fn resolve(requested: Option<usize>) -> Self {
        let avail = std::thread::available_parallelism()
            .map(NonZeroUsize::get)
            .unwrap_or(1);
        let n = match requested {
            None | Some(0) => avail,
            Some(n) => n.min(avail),
        };
        Self {
            threads: NonZeroUsize::new(n.max(1)).unwrap_or(NonZeroUsize::MIN),
        }
    }

    /// Explicit worker count passed to parallel hash entry points.
    #[must_use]
    pub fn threads(self) -> NonZeroUsize {
        self.threads
    }
}

/// Worker count for index preload / parallel worktree hashing.
///
/// Honors Git's `core.preloadindex` (when false, only one worker) and
/// `index.threads` (when unset or zero, all logical CPUs).
#[must_use]
pub fn index_parallelism_from_config(config: &crate::config::ConfigSet) -> Parallelism {
    use std::num::NonZeroUsize;

    let preload = config
        .get_bool("core.preloadindex")
        .and_then(|r| r.ok())
        .unwrap_or(true);
    if !preload {
        return Parallelism::new(NonZeroUsize::MIN);
    }
    let requested = config
        .get_i64("index.threads")
        .and_then(|r| r.ok())
        .map(|n| n.max(0) as usize);
    Parallelism::resolve(requested)
}

/// Returns true when parallel hashing is expected to beat a serial loop.
#[must_use]
pub fn parallel_hash_worthwhile(item_count: usize, total_bytes: usize, threads: usize) -> bool {
    threads > 1 && item_count >= PAR_HASH_MIN_ITEMS && total_bytes >= PAR_HASH_MIN_TOTAL_BYTES
}

/// Hash Git objects in parallel, preserving input order.
///
/// Uses [`par_hash_with`] internally; see [`PAR_HASH_MIN_ITEMS`] and
/// [`PAR_HASH_MIN_TOTAL_BYTES`] for serial fallback thresholds.
#[must_use]
pub fn hash_objects_parallel(
    algo: HashAlgo,
    items: &[(ObjectKind, &[u8])],
    threads: NonZeroUsize,
) -> Vec<ObjectId> {
    let total_bytes: usize = items.iter().map(|(_, data)| data.len()).sum();
    par_hash_with(items, threads, total_bytes, |(kind, data)| {
        hash_object(algo, *kind, data)
    })
}

/// Run `f` over `items` with up to `threads` workers, preserving order.
///
/// When [`parallel_hash_worthwhile`] is false, runs serially on the caller thread.
/// If `f` panics, partial results are dropped and the panic is rethrown after
/// workers join.
#[must_use]
pub fn par_hash_with<T, R, F>(
    items: &[T],
    threads: NonZeroUsize,
    total_bytes: usize,
    f: F,
) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    try_par_hash_with::<T, R, Infallible, _>(items, threads, total_bytes, |item| Ok(f(item)))
        .unwrap_or_else(|e| match e {
            ParallelHashError::Task(infallible) => match infallible {},
        })
}

/// Fallible variant of [`par_hash_with`]: first closure error stops the pool.
///
/// When a worker returns `Err`, every value already stored for earlier indices is
/// dropped before this function returns. If a worker panics, the same cleanup runs
/// and the panic payload is **resumed on the caller thread** (this function does
/// not return).
///
/// # Errors
///
/// Returns [`ParallelHashError::Task`] with the first closure error observed.
pub fn try_par_hash_with<T, R, E, F>(
    items: &[T],
    threads: NonZeroUsize,
    total_bytes: usize,
    f: F,
) -> Result<Vec<R>, ParallelHashError<E>>
where
    T: Sync,
    R: Send,
    E: Send + 'static,
    F: Fn(&T) -> Result<R, E> + Sync,
{
    let len = items.len();
    if len == 0 {
        return Ok(Vec::new());
    }

    let n_workers = threads.get().min(len).max(1);
    if !parallel_hash_worthwhile(len, total_bytes, n_workers) {
        return items
            .iter()
            .map(&f)
            .collect::<Result<Vec<R>, E>>()
            .map_err(ParallelHashError::Task);
    }

    run_par_hash_pool(items, n_workers, f)
}

/// Like [`try_par_hash_with`], but uses the thread pool whenever `threads` > 1 and
/// `items` is non-empty (ignores [`parallel_hash_worthwhile`]).
///
/// Used for pack index-pack where object counts are large and per-item work (inflate
/// + hash) dominates thread startup cost.
///
/// # Errors
///
/// Returns [`ParallelHashError::Task`] with the first closure error observed.
pub fn try_par_hash_with_force<T, R, E, F>(
    items: &[T],
    threads: NonZeroUsize,
    f: F,
) -> Result<Vec<R>, ParallelHashError<E>>
where
    T: Sync,
    R: Send,
    E: Send + 'static,
    F: Fn(&T) -> Result<R, E> + Sync,
{
    let len = items.len();
    if len == 0 {
        return Ok(Vec::new());
    }

    let n_workers = threads.get().min(len).max(1);
    if n_workers <= 1 {
        return items
            .iter()
            .map(&f)
            .collect::<Result<Vec<R>, E>>()
            .map_err(ParallelHashError::Task);
    }

    run_par_hash_pool(items, n_workers, f)
}

fn run_par_hash_pool<T, R, E, F>(
    items: &[T],
    n_workers: usize,
    f: F,
) -> Result<Vec<R>, ParallelHashError<E>>
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
                let start = next.fetch_add(PAR_HASH_CHUNK, Ordering::AcqRel);
                if start >= len {
                    return;
                }
                let end = (start + PAR_HASH_CHUNK).min(len);
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
            return Err(ParallelHashError::Task(e));
        }
    }

    let slots = match Arc::try_unwrap(slots) {
        Ok(s) => s,
        Err(arc) => {
            arc.drop_initialized();
            panic!("parallel hash slot buffer still shared after scope join");
        }
    };
    Ok(slots.into_vec())
}

struct Slots<R> {
    cells: Vec<UnsafeCell<MaybeUninit<R>>>,
    initialized: Vec<AtomicBool>,
}

impl<R> Slots<R> {
    fn new(len: usize) -> Self {
        Self {
            cells: (0..len)
                .map(|_| UnsafeCell::new(MaybeUninit::uninit()))
                .collect(),
            initialized: (0..len).map(|_| AtomicBool::new(false)).collect(),
        }
    }

    fn store(&self, index: usize, value: R) {
        // SAFETY: each index is written at most once while `initialized` is false.
        unsafe {
            (*self.cells[index].get()).write(value);
        }
        self.initialized[index].store(true, Ordering::Release);
    }

    fn drop_initialized(&self) {
        for i in 0..self.cells.len() {
            if self.initialized[i].swap(false, Ordering::AcqRel) {
                // SAFETY: `initialized` was true, so the slot holds a valid `R`.
                unsafe {
                    (*self.cells[i].get()).assume_init_drop();
                }
            }
        }
    }

    fn into_vec(self) -> Vec<R> {
        let Self {
            mut cells,
            initialized,
        } = self;
        let mut out = Vec::with_capacity(cells.len());
        for (cell, was_init) in cells.drain(..).zip(initialized) {
            assert!(
                was_init.into_inner(),
                "parallel hash success path left an uninitialized slot"
            );
            // SAFETY: success path initializes every index exactly once.
            unsafe {
                out.push(cell.into_inner().assume_init());
            }
        }
        out
    }
}

// Each cell is written once at a distinct index; `initialized` guards drops.
unsafe impl<R: Send> Sync for Slots<R> {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::Arc;

    fn threads(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).expect("non-zero threads")
    }

    fn serial_ids(algo: HashAlgo, items: &[(ObjectKind, &[u8])]) -> Vec<ObjectId> {
        items
            .iter()
            .map(|(kind, data)| hash_object(algo, *kind, data))
            .collect()
    }

    struct DropCounter {
        dropped: Arc<AtomicUsize>,
    }

    impl Drop for DropCounter {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, AtomicOrdering::SeqCst);
        }
    }

    #[test]
    fn parallel_matches_serial_counts_and_threads() {
        let sizes = [0_usize, 1, 7, 10_000];
        let thread_counts = [1_usize, 2, 8];
        let blob = vec![0xAB_u8; 512];
        for &n in &sizes {
            let items: Vec<(ObjectKind, &[u8])> = (0..n)
                .map(|_| (ObjectKind::Blob, blob.as_slice()))
                .collect();
            let expect = serial_ids(HashAlgo::Sha1, &items);
            for &t in &thread_counts {
                let got = hash_objects_parallel(HashAlgo::Sha1, &items, threads(t));
                assert_eq!(got, expect, "n={n} threads={t}");
            }
        }
    }

    #[test]
    fn parallel_sha256_matches_serial() {
        let blobs: Vec<[u8; 64]> = (0..500).map(|i| [i as u8; 64]).collect();
        let items: Vec<(ObjectKind, &[u8])> = blobs
            .iter()
            .map(|b| (ObjectKind::Blob, b.as_slice()))
            .collect();
        let expect = serial_ids(HashAlgo::Sha256, &items);
        let got = hash_objects_parallel(HashAlgo::Sha256, &items, threads(4));
        assert_eq!(got, expect);
    }

    #[test]
    fn par_hash_with_preserves_order() {
        let items: Vec<usize> = (0..256).collect();
        let got = par_hash_with(&items, threads(4), items.len(), |&x| x * 2);
        let expect: Vec<_> = items.iter().map(|x| x * 2).collect();
        assert_eq!(got, expect);
    }

    #[test]
    fn try_par_hash_surfaces_task_error_without_deadlock() {
        let items: Vec<i32> = (0..128).collect();
        let err = try_par_hash_with(&items, threads(4), items.len(), |&x| {
            if x == 64 {
                Err("boom")
            } else {
                Ok(x)
            }
        })
        .unwrap_err();
        assert_eq!(err, ParallelHashError::Task("boom"));
    }

    #[test]
    fn try_par_hash_drops_partial_results_on_task_error() {
        let items: Vec<i32> = (0..128).collect();
        let made = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let err = try_par_hash_with::<_, _, (), _>(&items, threads(8), items.len(), |&x| {
            if x == 127 {
                while made.load(AtomicOrdering::Acquire) < 127 {
                    std::hint::spin_loop();
                }
                return Err(());
            }
            made.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(DropCounter {
                dropped: Arc::clone(&dropped),
            })
        });
        assert!(matches!(err, Err(ParallelHashError::Task(()))));
        assert_eq!(made.load(AtomicOrdering::SeqCst), 127);
        assert_eq!(dropped.load(AtomicOrdering::SeqCst), 127);
    }

    #[test]
    fn try_par_hash_concurrent_errors_are_raced_without_ub() {
        let items: Vec<i32> = (0..512).collect();
        let err = try_par_hash_with(&items, threads(8), items.len(), |&x| {
            if x % 7 == 0 {
                Err(x)
            } else {
                Ok(x)
            }
        })
        .unwrap_err();
        match err {
            ParallelHashError::Task(code) => assert_eq!(code % 7, 0),
        }
    }

    #[test]
    fn par_hash_panic_propagates_and_drops_partials() {
        let items: Vec<i32> = (0..128).collect();
        let made = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ = try_par_hash_with::<_, _, std::convert::Infallible, _>(
                &items,
                threads(8),
                items.len(),
                |&x| {
                    if x == 127 {
                        while made.load(AtomicOrdering::Acquire) < 127 {
                            std::hint::spin_loop();
                        }
                        panic!("fail");
                    }
                    made.fetch_add(1, AtomicOrdering::SeqCst);
                    Ok(DropCounter {
                        dropped: Arc::clone(&dropped),
                    })
                },
            );
        }));
        assert!(result.is_err());
        assert_eq!(made.load(AtomicOrdering::SeqCst), 127);
        assert_eq!(dropped.load(AtomicOrdering::SeqCst), 127);
    }

    #[test]
    fn parallelism_resolve_clamps_to_available() {
        let avail = std::thread::available_parallelism()
            .map(NonZeroUsize::get)
            .unwrap_or(1);
        let p = Parallelism::resolve(Some(usize::MAX));
        assert_eq!(p.threads().get(), avail);
        let auto = Parallelism::resolve(None);
        assert_eq!(auto.threads().get(), avail);
        let zero = Parallelism::resolve(Some(0));
        assert_eq!(zero.threads().get(), avail);
    }

    #[test]
    fn serial_fallback_below_threshold() {
        let items: Vec<i32> = (0..4).collect();
        let counter = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&counter);
        let _ = par_hash_with(&items, threads(8), 0, move |&x| {
            c.fetch_add(1, AtomicOrdering::Relaxed);
            x
        });
        assert_eq!(counter.load(AtomicOrdering::Relaxed), 4);
    }
}
