//! Parallel batch hashing — thin wrappers over [`crate::parallel`].

use std::convert::Infallible;
use std::num::NonZeroUsize;

use crate::objects::{HashAlgo, ObjectId, ObjectKind};
use crate::parallel::{
    parallel_worthwhile as parallel_worthwhile_opts, try_par_map, try_par_map_force,
    ParallelOptions,
};

pub use crate::parallel::index_parallelism_from_config;
pub use crate::parallel::{ParallelHashError, Parallelism};

use super::{hash_object, try_hash_object};
use crate::error::Error;

/// Minimum item count before parallel helpers use more than one thread.
pub const PAR_HASH_MIN_ITEMS: usize = 32;
/// Minimum total payload bytes (summed over items) for parallel hashing.
pub const PAR_HASH_MIN_TOTAL_BYTES: usize = 256 * 1024;

/// Returns true when parallel hashing is expected to beat a serial loop.
#[must_use]
pub fn parallel_hash_worthwhile(item_count: usize, total_bytes: usize, threads: usize) -> bool {
    if threads <= 1 {
        return false;
    }
    parallel_worthwhile_opts(
        item_count,
        total_bytes,
        NonZeroUsize::new(threads).unwrap_or(NonZeroUsize::MIN),
        PAR_HASH_MIN_ITEMS,
        PAR_HASH_MIN_TOTAL_BYTES,
    )
}

fn hash_parallel_options(threads: NonZeroUsize) -> ParallelOptions {
    ParallelOptions {
        threads,
        min_items: PAR_HASH_MIN_ITEMS,
        min_total_bytes: Some(PAR_HASH_MIN_TOTAL_BYTES),
        signed_entry_threshold: None,
    }
}

/// Hash Git objects in parallel, preserving input order.
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

/// Fallible variant of [`hash_objects_parallel`] (SHA-1 collision detection).
pub fn try_hash_objects_parallel(
    algo: HashAlgo,
    items: &[(ObjectKind, &[u8])],
    threads: NonZeroUsize,
) -> Result<Vec<ObjectId>, ParallelHashError<Error>> {
    let total_bytes: usize = items.iter().map(|(_, data)| data.len()).sum();
    try_par_hash_with(items, threads, total_bytes, |(kind, data)| {
        try_hash_object(algo, *kind, data)
    })
}

/// Run `f` over `items` with up to `threads` workers, preserving order.
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

/// Fallible variant of [`par_hash_with`].
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
    try_par_map(items, hash_parallel_options(threads), total_bytes, f)
}

/// Like [`try_par_hash_with`], but ignores batch thresholds.
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
    try_par_map_force(items, hash_parallel_options(threads), f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::{HashAlgo, ObjectKind};
    use std::panic::{catch_unwind, AssertUnwindSafe};
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
        // Miri interprets every SHA-1 round; keep its batch small enough to finish.
        let large = if cfg!(miri) { 64 } else { 10_000 };
        let sizes = [0_usize, 1, 7, large];
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
