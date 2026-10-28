//! Ordered scoped-thread parallelism and Git config-driven thread policy.
//!
//! There is no global pool and no environment reads in this module. Embedders and
//! the CLI pass an explicit [`ThreadPolicy`] (for example `--threads`) when opening
//! a [`crate::repo::Repository`].
//!
//! Library guide: [parallelism](https://grit-scm.com/docs/library/parallelism)
//!
//! ```rust
//! use grit_lib::parallel::ThreadPolicy;
//! # fn main() {
//! let _ = ThreadPolicy::default();
//! # }
//! ```

mod map;
mod policy;
mod pool;

pub(crate) use map::pool_worthwhile;
pub(crate) use pool::{run_ordered_batches, run_ordered_pool};

pub use map::{
    par_map, parallel_worthwhile, try_par_map, try_par_map_force, try_par_map_with_local,
};
pub use policy::{ParallelOp, ParallelOptions, ThreadPolicy};

use std::num::NonZeroUsize;

/// Failure from fallible parallel helpers when a closure returns an error.
///
/// If a worker panics, partial results are dropped and the panic is resumed on the
/// caller thread after workers join.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParallelHashError<E> {
    /// The first error returned by a worker (additional concurrent errors are discarded).
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

/// Resolved worker thread count for parallel helpers.
///
/// Config keys are interpreted by [`ThreadPolicy::resolve`]; this type only clamps
/// an optional request to [`std::thread::available_parallelism`].
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

    /// Explicit worker count passed to parallel entry points.
    #[must_use]
    pub fn threads(self) -> NonZeroUsize {
        self.threads
    }
}

/// Worker count for index preload / parallel worktree hashing from config alone.
#[must_use]
pub fn index_parallelism_from_config(config: &crate::config::ConfigSet) -> Parallelism {
    ThreadPolicy::default()
        .resolve(ParallelOp::IndexRefresh, config)
        .parallelism()
}

/// Resolve `IngestPackOptions::threads` from explicit fetch options, CLI policy, or config.
///
/// Returns `Some(n)` only when `explicit` or a [`ThreadPolicy::threads_override`] is set.
/// Otherwise returns `None` so index-pack reads `pack.threads` from the repository config.
pub fn index_pack_threads_for_ingest(
    policy: ThreadPolicy,
    config: &crate::config::ConfigSet,
    explicit: Option<usize>,
) -> Result<Option<usize>, crate::error::ConfigError> {
    if let Some(n) = explicit {
        return Ok(Some(n));
    }
    if policy.threads_override.is_some() {
        return Ok(Some(
            policy
                .try_resolve(ParallelOp::IndexPack, config)?
                .threads
                .get(),
        ));
    }
    Ok(None)
}

/// Lazy name-hash thread count from config and Git's `LAZY_THREAD_COST` thresholds.
#[must_use]
pub fn index_load_lazy_thread_count(
    policy: &ThreadPolicy,
    config: &crate::config::ConfigSet,
    cache_nr: usize,
    try_threaded: bool,
) -> usize {
    if !try_threaded {
        return 0;
    }
    let opts = policy.resolve(ParallelOp::IndexLoad, config);
    if opts.threads.get() <= 1 {
        return 0;
    }
    if cache_nr < 2 * crate::index_name_hash_lazy::LAZY_THREAD_COST {
        return 0;
    }
    let mut nr = opts.threads.get();
    if cache_nr < nr * crate::index_name_hash_lazy::LAZY_THREAD_COST {
        nr = cache_nr / crate::index_name_hash_lazy::LAZY_THREAD_COST;
    }
    nr
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn threads(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).expect("non-zero threads")
    }

    #[test]
    fn try_par_map_preserves_order() {
        let items: Vec<usize> = (0..256).collect();
        let opts = ParallelOptions {
            threads: threads(4),
            min_items: 0,
            min_total_bytes: None,
            signed_entry_threshold: None,
        };
        let got = par_map(&items, opts, items.len(), |&x| x * 2);
        let expect: Vec<_> = items.iter().map(|x| x * 2).collect();
        assert_eq!(got, expect);
    }

    #[test]
    fn try_par_map_surfaces_task_error() {
        let items: Vec<i32> = (0..128).collect();
        let opts = ParallelOptions {
            threads: threads(4),
            min_items: 0,
            min_total_bytes: None,
            signed_entry_threshold: None,
        };
        let err = try_par_map(&items, opts, items.len(), |&x| {
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
    fn ordered_batches_consume_in_item_order_with_worker_state() {
        let items: Vec<usize> = (0..1000).collect();
        let inits = AtomicUsize::new(0);
        let mut seen = Vec::new();
        let mut batch_lens = Vec::new();
        run_ordered_batches(
            &items,
            7,
            4,
            || {
                inits.fetch_add(1, Ordering::Relaxed);
                0usize
            },
            |mapped: &mut usize, &x| -> Result<usize, ()> {
                *mapped += 1;
                Ok(x * 3)
            },
            |batch, results| {
                assert_eq!(batch.len(), results.len());
                batch_lens.push(batch.len());
                seen.extend(results);
                Ok(())
            },
        )
        .expect("batches");
        assert_eq!(seen, items.iter().map(|x| x * 3).collect::<Vec<_>>());
        assert_eq!(batch_lens.iter().sum::<usize>(), items.len());
        assert!(batch_lens[..batch_lens.len() - 1].iter().all(|&n| n == 7));
        assert_eq!(
            inits.load(Ordering::Relaxed),
            4,
            "init runs once per worker"
        );
    }

    #[test]
    fn ordered_batches_surface_map_and_consume_errors() {
        let items: Vec<i32> = (0..500).collect();
        let err = run_ordered_batches(
            &items,
            10,
            3,
            || (),
            |_, &x| if x == 250 { Err("map") } else { Ok(x) },
            |_, _| Ok(()),
        )
        .unwrap_err();
        assert_eq!(err, "map");

        let mut consumed = 0;
        let err = run_ordered_batches(
            &items,
            10,
            3,
            || (),
            |_, &x| Ok::<_, &str>(x),
            |_, _| {
                consumed += 1;
                if consumed == 2 {
                    Err("consume")
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        assert_eq!(err, "consume");
        assert_eq!(consumed, 2, "no batch is consumed after an error");

        run_ordered_batches(
            &[] as &[i32],
            10,
            3,
            || (),
            |_, &x| Ok::<_, ()>(x),
            |_, _| panic!("nothing to consume"),
        )
        .expect("empty input");
    }

    #[test]
    fn ordered_batches_resume_worker_panics() {
        let items: Vec<i32> = (0..200).collect();
        let outcome = std::panic::catch_unwind(|| {
            run_ordered_batches(
                &items,
                5,
                4,
                || (),
                |_, &x| -> Result<i32, ()> {
                    assert!(x != 120, "worker boom");
                    Ok(x)
                },
                |_, _| Ok(()),
            )
        });
        assert!(outcome.is_err());
    }

    #[test]
    fn index_pack_threads_for_ingest_honors_override_only() {
        let cfg = config_from("[pack]\n\tthreads = 2\n");
        assert_eq!(
            index_pack_threads_for_ingest(ThreadPolicy::default(), &cfg, None).expect("ingest"),
            None
        );
        assert_eq!(
            index_pack_threads_for_ingest(ThreadPolicy::default(), &cfg, Some(3)).expect("ingest"),
            Some(3)
        );
        let policy = ThreadPolicy::with_threads_override(Some(6));
        assert_eq!(
            index_pack_threads_for_ingest(policy, &cfg, None).expect("ingest"),
            Some(policy.resolve(ParallelOp::IndexPack, &cfg).threads.get())
        );
    }

    fn config_from(text: &str) -> crate::config::ConfigSet {
        use crate::config::{ConfigFile, ConfigScope, ConfigSet};
        use std::path::Path;
        let file = ConfigFile::parse(Path::new("test"), text, ConfigScope::Local).expect("parse");
        let mut set = ConfigSet::new();
        set.merge(&file);
        set
    }

    #[test]
    fn try_par_map_with_local_reuses_per_worker_state() {
        let items: Vec<i32> = (0..256).collect();
        let opts = ParallelOptions {
            threads: threads(4),
            min_items: 0,
            min_total_bytes: None,
            signed_entry_threshold: None,
        };
        let init_count = Arc::new(AtomicUsize::new(0));
        let init_count_for_init = Arc::clone(&init_count);
        let got = try_par_map_with_local(
            &items,
            opts,
            items.len(),
            move || {
                init_count_for_init.fetch_add(1, Ordering::SeqCst);
                0_i32
            },
            |state, &x| -> Result<i32, ()> {
                *state += 1;
                Ok(x + *state)
            },
        )
        .expect("map");
        assert_eq!(got.len(), items.len());
        let inits = init_count.load(Ordering::SeqCst);
        assert!(
            inits >= 2,
            "expected multiple workers to init state, got {inits}"
        );
        assert!(
            inits <= 4,
            "init count should not exceed worker count, got {inits}"
        );
    }
}
