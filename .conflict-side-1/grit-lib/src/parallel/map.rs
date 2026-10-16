//! Ordered parallel map helpers built on the scoped thread pool.

use std::convert::Infallible;
use std::num::NonZeroUsize;

use super::policy::ParallelOptions;
use super::pool::{run_ordered_pool, run_ordered_pool_with_local, DEFAULT_CHUNK};
use super::ParallelHashError;

#[must_use]
pub(crate) fn pool_worthwhile(len: usize, total_bytes: usize, options: ParallelOptions) -> bool {
    if options.threads.get() <= 1 || len == 0 {
        return false;
    }
    if let Some(threshold) = options.signed_entry_threshold {
        if (len as i64) < threshold {
            return false;
        }
    }
    if options.min_items == 0 {
        return true;
    }
    if len < options.min_items {
        return false;
    }
    if let Some(min_bytes) = options.min_total_bytes {
        if total_bytes < min_bytes {
            return false;
        }
    }
    true
}

fn worker_count(options: ParallelOptions, len: usize) -> usize {
    options.threads.get().min(len).max(1)
}

/// Run `f` over `items` with up to `options.threads` workers, preserving order.
///
/// Falls back to a serial loop when [`ParallelOptions::min_items`] / `min_total_bytes`
/// thresholds are not met.
#[must_use]
pub fn par_map<T, R, F>(items: &[T], options: ParallelOptions, total_bytes: usize, f: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    try_par_map::<T, R, Infallible, _>(items, options, total_bytes, |item| Ok(f(item)))
        .unwrap_or_else(|e| match e {
            ParallelHashError::Task(infallible) => match infallible {},
        })
}

/// Fallible ordered parallel map; first error cancels the pool.
///
/// # Errors
///
/// Returns [`ParallelHashError::Task`] with the first closure error.
pub fn try_par_map<T, R, E, F>(
    items: &[T],
    options: ParallelOptions,
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
    let n_workers = worker_count(options, len);
    if !pool_worthwhile(len, total_bytes, options) {
        return items
            .iter()
            .map(&f)
            .collect::<Result<Vec<R>, E>>()
            .map_err(ParallelHashError::Task);
    }
    run_ordered_pool(items, n_workers, DEFAULT_CHUNK, f).map_err(ParallelHashError::Task)
}

/// Like [`try_par_map`], but ignores batch thresholds (still requires `threads` > 1).
///
/// # Errors
///
/// Returns [`ParallelHashError::Task`] with the first closure error.
pub fn try_par_map_force<T, R, E, F>(
    items: &[T],
    options: ParallelOptions,
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
    let n_workers = worker_count(options, len);
    if n_workers <= 1 {
        return items
            .iter()
            .map(&f)
            .collect::<Result<Vec<R>, E>>()
            .map_err(ParallelHashError::Task);
    }
    run_ordered_pool(items, n_workers, DEFAULT_CHUNK, f).map_err(ParallelHashError::Task)
}

/// Parallel map with local state from `init` for each item.
///
/// # Errors
///
/// Returns [`ParallelHashError::Task`] with the first closure error.
pub fn try_par_map_with_local<T, R, E, S, Init, F>(
    items: &[T],
    options: ParallelOptions,
    total_bytes: usize,
    init: Init,
    f: F,
) -> Result<Vec<R>, ParallelHashError<E>>
where
    T: Sync,
    R: Send,
    E: Send + 'static,
    S: Send,
    Init: Fn() -> S + Sync,
    F: Fn(&mut S, &T) -> Result<R, E> + Sync,
{
    let len = items.len();
    if len == 0 {
        return Ok(Vec::new());
    }
    let n_workers = worker_count(options, len);
    if !pool_worthwhile(len, total_bytes, options) {
        let mut state = init();
        return items
            .iter()
            .map(|item| f(&mut state, item))
            .collect::<Result<Vec<R>, E>>()
            .map_err(ParallelHashError::Task);
    }
    run_ordered_pool_with_local(items, n_workers, DEFAULT_CHUNK, init, f)
        .map_err(ParallelHashError::Task)
}

/// Legacy helper: returns true when parallel hashing is expected to beat a serial loop.
#[must_use]
pub fn parallel_worthwhile(
    item_count: usize,
    total_bytes: usize,
    threads: NonZeroUsize,
    min_items: usize,
    min_total_bytes: usize,
) -> bool {
    pool_worthwhile(
        item_count,
        total_bytes,
        ParallelOptions {
            threads,
            min_items,
            min_total_bytes: Some(min_total_bytes),
            signed_entry_threshold: None,
        },
    )
}
