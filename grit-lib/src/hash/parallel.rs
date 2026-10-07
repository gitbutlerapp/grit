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

/// Failure from [`try_par_hash_with`] when a worker panics or returns an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParallelHashError<E> {
    /// The first error returned by the closure.
    Task(E),
    /// A worker thread panicked (payload is dropped).
    Panic,
}

impl<E: std::fmt::Display> std::fmt::Display for ParallelHashError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Task(e) => write!(f, "{e}"),
            Self::Panic => f.write_str("parallel hash worker panicked"),
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
/// If `f` panics, the panic is rethrown after worker threads finish (no deadlock).
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
            ParallelHashError::Panic => unreachable!("worker panic is resumed before return"),
        })
}

/// Fallible variant of [`par_hash_with`]: first error or panic stops the pool.
///
/// # Errors
///
/// Returns [`ParallelHashError::Task`] on the first closure error, or
/// [`ParallelHashError::Panic`] when a worker panics.
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

    let slots: Arc<Slots<R>> = Arc::new(Slots(
        (0..len)
            .map(|_| UnsafeCell::new(MaybeUninit::uninit()))
            .collect(),
    ));

    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let err_slot: Arc<ErrorSlot<E>> = Arc::new(ErrorSlot(UnsafeCell::new(None)));
    let panic_payload: Arc<Mutex<Option<Box<dyn std::any::Any + Send>>>> =
        Arc::new(Mutex::new(None));

    let run_worker =
        |slots: Arc<Slots<R>>,
         err_slot: Arc<ErrorSlot<E>>,
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
                        Ok(Ok(value)) => {
                            // SAFETY: each index is written exactly once.
                            unsafe {
                                (*slots.0[i].get()).write(value);
                            }
                        }
                        Ok(Err(e)) => {
                            err_slot.store(e);
                            failed.store(true, Ordering::Release);
                        }
                        Err(payload) => {
                            if let Ok(mut guard) = panic_payload.lock() {
                                *guard = Some(payload);
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
            let err_slot = Arc::clone(&err_slot);
            let panic_payload = Arc::clone(&panic_payload);
            scope.spawn(move || run_worker(slots, err_slot, panic_payload));
        }
        run_worker(
            Arc::clone(&slots),
            Arc::clone(&err_slot),
            Arc::clone(&panic_payload),
        );
    });

    if let Ok(mut guard) = panic_payload.lock() {
        if let Some(payload) = guard.take() {
            resume_unwind(payload);
        }
    }

    if let Ok(err_slot) = Arc::try_unwrap(err_slot) {
        if let Some(e) = err_slot.take() {
            return Err(ParallelHashError::Task(e));
        }
    }

    let slots = match Arc::try_unwrap(slots) {
        Ok(s) => s,
        Err(_) => {
            return Err(ParallelHashError::Panic);
        }
    };
    Ok(slots.into_vec())
}

struct Slots<R>(Vec<UnsafeCell<MaybeUninit<R>>>);

impl<R> Slots<R> {
    fn into_vec(self) -> Vec<R> {
        let Self(mut slots) = self;
        let mut out = Vec::with_capacity(slots.len());
        // SAFETY: every index was initialized exactly once on success paths.
        unsafe {
            for cell in slots.drain(..) {
                out.push(cell.into_inner().assume_init());
            }
        }
        out
    }
}

// Each cell is written once at a distinct index.
unsafe impl<R: Send> Sync for Slots<R> {}

struct ErrorSlot<E>(UnsafeCell<Option<E>>);

impl<E> ErrorSlot<E> {
    fn store(&self, err: E) {
        // SAFETY: only the first error is kept; writers set `failed` afterward.
        unsafe {
            let slot = &mut *self.0.get();
            if slot.is_none() {
                *slot = Some(err);
            }
        }
    }

    fn take(self) -> Option<E> {
        unsafe { (*self.0.get()).take() }
    }
}

unsafe impl<E: Send> Sync for ErrorSlot<E> {}

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
    fn par_hash_panic_propagates() {
        let items: Vec<i32> = (0..128).collect();
        let result = catch_unwind(AssertUnwindSafe(|| {
            par_hash_with(&items, threads(4), items.len(), |&x| {
                if x == 64 {
                    panic!("fail");
                }
                x
            })
        }));
        assert!(result.is_err());
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
