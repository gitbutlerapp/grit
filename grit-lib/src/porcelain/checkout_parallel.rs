//! Parallel checkout planning and execution (`checkout.workers` policy).

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::ConfigSet;
use crate::crlf::FileAttrs;
use crate::crlf::{self, ConversionConfig};
use crate::error::{Error, Result};
use crate::filter_process::{DelayedProcessCheckout, FilterSmudgeMeta};
use crate::index::{IndexEntry, MODE_EXECUTABLE, MODE_REGULAR, MODE_SYMLINK};
use crate::objects::pack_store::PackReadContextGuard;
use crate::objects::ObjectId;
use crate::parallel::{
    run_ordered_batches, try_par_map_with_local, ParallelHashError, ParallelOp, ParallelOptions,
};
use crate::repo::Repository;
use crate::unicode_normalization::precompose_utf8_path;
use crate::worktree_rules::WorktreeRules;

use super::{
    read_blob_unless_streamed, stream_checkout_entry, worktree_bytes_from_index_blob,
    write_checkout_entry, DelayedCheckoutTarget, LeadingDirCache, WorktreeFileContent,
};

/// Parallel-eligible checkout jobs buffered before the first flush, so a cancellation that
/// arrives early stops after a handful of files.
const PARALLEL_WRITE_FIRST_CHUNK: usize = 8;

/// Largest flush once chunks have doubled up from [`PARALLEL_WRITE_FIRST_CHUNK`]. Big enough
/// that worker start-up and the per-flush join stay a small share of a large checkout, small
/// enough to limit work between cancellation polls. A separate byte threshold bounds the
/// prepared contents retained within each chunk.
const PARALLEL_WRITE_MAX_CHUNK: usize = 2048;

/// Flush prepared contents at this byte threshold, independently of cancellation chunk sizes.
/// Parallel preparation additionally keeps at most two blobs per worker in flight.
const PREPARED_WRITE_BYTES: usize = 1024 * 1024;

/// One blob checkout after attribute/conversion resolution on the caller thread.
pub(crate) struct CheckoutWriteJob {
    pub path: String,
    pub mode: u32,
    pub oid: ObjectId,
    pub change: crate::diff::DiffEntry,
    pub file_attrs: FileAttrs,
    pub smudge_meta: FilterSmudgeMeta,
}

/// Planned slot for one diff entry (input order preserved).
pub(crate) enum CheckoutPlanSlot {
    /// Blob checkout deferred to [`execute_write_jobs`] (same order as [`CheckoutWriteJob`]).
    Write,
    Ready(IndexEntry),
}

/// Result of materializing one write job (single ODB read + conversion).
#[derive(Clone)]
pub(crate) enum WriteJobOutcome {
    Index(IndexEntry),
    Delayed(DelayedCheckoutTarget),
}

fn effective_path_for_collision(path: &str, precompose: bool) -> Cow<'_, str> {
    if precompose {
        precompose_utf8_path(path)
    } else {
        Cow::Borrowed(path)
    }
}

fn normalized_path_key(path: &str, precompose: bool, ignorecase: bool) -> String {
    let base = effective_path_for_collision(path, precompose);
    if ignorecase {
        base.to_ascii_lowercase()
    } else {
        base.into_owned()
    }
}

/// Paths that must not share a parallel worker pool with any colliding spelling.
///
/// Runs in O(n × average path depth) via prefix lookup, not O(n²) pairwise comparison.
#[must_use]
pub(crate) fn paths_needing_serial_checkout(
    paths: &[String],
    ignorecase: bool,
    precompose: bool,
) -> HashSet<String> {
    if !ignorecase && !precompose {
        return HashSet::new();
    }
    let normalized: Vec<String> = paths
        .iter()
        .map(|p| normalized_path_key(p, precompose, ignorecase))
        .collect();
    let mut serial = HashSet::new();

    let mut by_full: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, key) in normalized.iter().enumerate() {
        by_full.entry(key.as_str()).or_default().push(i);
    }
    for indices in by_full.values() {
        if indices.len() > 1 {
            for &i in indices {
                serial.insert(paths[i].clone());
            }
        }
    }

    let full_paths: HashSet<&str> = normalized.iter().map(String::as_str).collect();
    for (i, key) in normalized.iter().enumerate() {
        let mut prefix = String::new();
        for (part_idx, part) in key.split('/').enumerate() {
            if part_idx > 0 {
                prefix.push('/');
            }
            prefix.push_str(part);
            if prefix == *key {
                break;
            }
            if full_paths.contains(prefix.as_str()) {
                if let Some(others) = by_full.get(prefix.as_str()) {
                    for &j in others {
                        if i != j {
                            serial.insert(paths[i].clone());
                            serial.insert(paths[j].clone());
                        }
                    }
                }
            }
        }
    }
    serial
}

/// Git parallel-checkout eligibility for regular files (`parallel-checkout.c`).
#[must_use]
pub(crate) fn is_eligible_for_parallel_checkout(mode: u32, attrs: &FileAttrs) -> bool {
    if mode != MODE_REGULAR && mode != MODE_EXECUTABLE {
        return false;
    }
    if attrs.filter_smudge.is_some() || attrs.filter_process.is_some() {
        return false;
    }
    const LARGE_PACKET: usize = 65516;
    attrs
        .working_tree_encoding
        .as_ref()
        .map_or(0, |e| e.len())
        .saturating_add(300)
        < LARGE_PACKET
}

fn checkout_pool_worthwhile(len: usize, options: ParallelOptions) -> bool {
    if !crate::parallel::parallel_worthwhile(
        len,
        0,
        options.threads,
        options.min_items,
        options.min_total_bytes.unwrap_or(0),
    ) {
        return false;
    }
    if let Some(threshold) = options.signed_entry_threshold {
        if (len as i64) < threshold {
            return false;
        }
    }
    true
}

/// Worker configuration for a checkout writing `eligible` parallel-eligible files, or `None`
/// when they should be read and written serially.
///
/// Like Git, `checkout.workers` and `checkout.thresholdForParallelism` are judged once against
/// every eligible file in the checkout. The returned options carry no threshold, so the chunks
/// [`execute_write_jobs`] flushes for cancellation each use the pool however small they are.
fn parallel_pool_for_checkout(
    repo: &Repository,
    config: &ConfigSet,
    eligible: usize,
    threshold_override: Option<i64>,
) -> Option<ParallelOptions> {
    if eligible < 2 {
        return None;
    }
    let mut options = repo.thread_policy().resolve(ParallelOp::Checkout, config);
    if let Some(threshold) = threshold_override {
        options.signed_entry_threshold = Some(threshold);
    }
    chunk_pool_options(options, eligible)
}

/// Decide once from the resolved `options` whether `eligible` files get a worker pool, and if so
/// return options that every chunk of them can use without being re-judged.
pub(crate) fn chunk_pool_options(
    options: ParallelOptions,
    eligible: usize,
) -> Option<ParallelOptions> {
    checkout_pool_worthwhile(eligible, options).then_some(ParallelOptions {
        signed_entry_threshold: None,
        min_items: 0,
        min_total_bytes: None,
        ..options
    })
}

/// Split `items` into cancellation chunks sized by [`ChunkSizes`], and pick the options each
/// chunk is mapped with: the pool when `items` as a whole qualifies under `options`, otherwise a
/// single thread.
pub(crate) fn cancellable_chunks<T>(
    items: &[T],
    options: ParallelOptions,
) -> (impl Iterator<Item = &[T]>, ParallelOptions) {
    let chunk_options = chunk_pool_options(options, items.len()).unwrap_or(ParallelOptions {
        threads: std::num::NonZeroUsize::MIN,
        ..options
    });
    let mut sizes = ChunkSizes::new();
    let mut rest = items;
    let chunks = std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let (chunk, tail) = rest.split_at(sizes.current().min(rest.len()));
        rest = tail;
        sizes.grow();
        Some(chunk)
    });
    (chunks, chunk_options)
}

/// Flush sizes for parallel-eligible jobs: [`PARALLEL_WRITE_FIRST_CHUNK`], doubling after each
/// full flush up to [`PARALLEL_WRITE_MAX_CHUNK`].
struct ChunkSizes {
    current: usize,
}

impl ChunkSizes {
    fn new() -> Self {
        Self {
            current: PARALLEL_WRITE_FIRST_CHUNK,
        }
    }

    /// Sizes that start at the cap, for callers that do not need a quick first stop.
    fn full() -> Self {
        Self {
            current: PARALLEL_WRITE_MAX_CHUNK,
        }
    }

    fn current(&self) -> usize {
        self.current
    }

    fn grow(&mut self) {
        self.current = (self.current * 2).min(PARALLEL_WRITE_MAX_CHUNK);
    }
}

struct ParallelWriteWorker {
    work_tree: PathBuf,
    dir_cache: LeadingDirCache,
}

struct PreparedWrite {
    idx: usize,
    /// Work-tree bytes, or `None` when the blob is streamed from the object database at write
    /// time instead of being held in memory.
    bytes: Option<Vec<u8>>,
}

fn worktree_bytes_for_parallel_job(
    conv: &ConversionConfig,
    job: &CheckoutWriteJob,
    blob: &[u8],
) -> Result<Vec<u8>> {
    if job.mode == MODE_SYMLINK {
        return Ok(blob.to_vec());
    }
    let oid_hex = job.oid.to_string();
    crlf::convert_to_worktree_eager(
        blob,
        &job.path,
        conv,
        &job.file_attrs,
        Some(&oid_hex),
        Some(&job.smudge_meta),
        None,
    )
    .map_err(Error::from)
}

fn prepare_write_job(
    repo: &Repository,
    conv: &ConversionConfig,
    job: &CheckoutWriteJob,
) -> Result<Option<Vec<u8>>> {
    let Some(object) = read_blob_unless_streamed(repo, conv, &job.file_attrs, job.mode, &job.oid)?
    else {
        return Ok(None);
    };
    worktree_bytes_for_parallel_job(conv, job, &object.data).map(Some)
}

fn write_prepared_job(
    repo: &Repository,
    work_tree: &Path,
    job: &CheckoutWriteJob,
    bytes: Option<&[u8]>,
    dir_cache: &mut LeadingDirCache,
) -> Result<IndexEntry> {
    let Some(bytes) = bytes else {
        return stream_checkout_entry(
            repo,
            work_tree,
            &job.path,
            job.mode,
            &job.change,
            dir_cache,
            job.oid,
        );
    };
    write_checkout_entry(
        work_tree,
        &job.path,
        WorktreeFileContent::Bytes(bytes),
        job.mode,
        &job.change,
        dir_cache,
        job.oid,
    )
}

#[allow(clippy::too_many_arguments)]
fn flush_prepared_writes_serial(
    repo: &Repository,
    work_tree: &Path,
    jobs: &[CheckoutWriteJob],
    prepared: &[PreparedWrite],
    dir_cache: &mut LeadingDirCache,
    outcomes: &mut [Option<WriteJobOutcome>],
    progress: &mut dyn crate::progress::ProgressSink,
    cancel: &dyn crate::progress::Cancel,
) -> Result<()> {
    for item in prepared {
        crate::progress::poll_cancel(progress, cancel)?;
        let entry = write_prepared_job(
            repo,
            work_tree,
            &jobs[item.idx],
            item.bytes.as_deref(),
            dir_cache,
        )?;
        outcomes[item.idx] = Some(WriteJobOutcome::Index(entry));
    }
    Ok(())
}

fn flush_prepared_writes_parallel(
    repo: &Repository,
    options: ParallelOptions,
    work_tree: &Path,
    jobs: &[CheckoutWriteJob],
    prepared: &[PreparedWrite],
    parent_dirs: &LeadingDirCache,
    outcomes: &mut [Option<WriteJobOutcome>],
) -> Result<()> {
    let work_tree = work_tree.to_path_buf();
    let parent_dirs = LeadingDirCache::with_ready_from(parent_dirs);
    let entries = try_par_map_with_local(
        prepared,
        options,
        0,
        || ParallelWriteWorker {
            work_tree: work_tree.clone(),
            dir_cache: LeadingDirCache::with_ready_from(&parent_dirs),
        },
        |worker, item| {
            write_prepared_job(
                repo,
                &worker.work_tree,
                &jobs[item.idx],
                item.bytes.as_deref(),
                &mut worker.dir_cache,
            )
        },
    )
    .map_err(|e: ParallelHashError<Error>| match e {
        ParallelHashError::Task(err) => err,
    })?;
    for (item, entry) in prepared.iter().zip(entries) {
        outcomes[item.idx] = Some(WriteJobOutcome::Index(entry));
    }
    Ok(())
}

fn materialize_write_job_serial(
    repo: &Repository,
    rules: &WorktreeRules,
    work_tree: &Path,
    dir_cache: &mut LeadingDirCache,
    job: &CheckoutWriteJob,
    delayed: &mut DelayedProcessCheckout,
) -> Result<WriteJobOutcome> {
    let Some(object) = read_blob_unless_streamed(
        repo,
        rules.conversion(),
        &job.file_attrs,
        job.mode,
        &job.oid,
    )?
    else {
        return stream_checkout_entry(
            repo,
            work_tree,
            &job.path,
            job.mode,
            &job.change,
            dir_cache,
            job.oid,
        )
        .map(WriteJobOutcome::Index);
    };
    let worktree_bytes = worktree_bytes_from_index_blob(
        repo,
        rules,
        &job.path,
        job.mode,
        &object.data,
        &job.oid,
        Some(&job.smudge_meta),
        Some(delayed),
    )?;
    let Some(worktree_bytes) = worktree_bytes else {
        return Ok(WriteJobOutcome::Delayed(DelayedCheckoutTarget {
            path: job.path.clone(),
            mode: job.mode,
            oid: job.oid,
            change: job.change.clone(),
        }));
    };
    let entry = write_checkout_entry(
        work_tree,
        &job.path,
        WorktreeFileContent::Bytes(&worktree_bytes),
        job.mode,
        &job.change,
        dir_cache,
        job.oid,
    )?;
    Ok(WriteJobOutcome::Index(entry))
}

/// Write one chunk of parallel-eligible jobs.
///
/// `pool` is the worker configuration when the checkout as a whole qualified for parallel
/// writes (see [`parallel_pool_for_checkout`]), or `None` to read and write serially.
#[allow(clippy::too_many_arguments)]
fn flush_parallel_batch(
    repo: &Repository,
    conv: &ConversionConfig,
    work_tree: &Path,
    jobs: &[CheckoutWriteJob],
    batch: &[usize],
    dir_cache: &mut LeadingDirCache,
    outcomes: &mut [Option<WriteJobOutcome>],
    pool: Option<ParallelOptions>,
    parallel_prepare_blobs: bool,
    progress: &mut dyn crate::progress::ProgressSink,
    cancel: &dyn crate::progress::Cancel,
) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    crate::progress::poll_cancel(progress, cancel)?;
    let pool = pool.filter(|_| batch.len() > 1);

    let mut prepared = Vec::with_capacity(batch.len());
    let mut prepared_bytes = 0usize;
    match pool {
        Some(options) if parallel_prepare_blobs => {
            let pack_store = repo.odb().pack_store_for(repo.odb().objects_dir());
            run_ordered_batches(
                batch,
                // Keep read-ahead bounded by blobs, even when each conversion is large.
                1,
                options.threads.get().min(batch.len()),
                || PackReadContextGuard::install(Arc::clone(&pack_store)),
                |_, &idx| {
                    prepare_write_job(repo, conv, &jobs[idx])
                        .map(|bytes| PreparedWrite { idx, bytes })
                },
                |_, items| {
                    crate::progress::poll_cancel(progress, cancel)?;
                    for item in items {
                        prepared_bytes += item.bytes.as_ref().map_or(0, Vec::len);
                        prepared.push(item);
                        if prepared_bytes >= PREPARED_WRITE_BYTES {
                            flush_prepared_batch(
                                repo, pool, work_tree, jobs, &prepared, dir_cache, outcomes,
                                progress, cancel,
                            )?;
                            prepared.clear();
                            prepared_bytes = 0;
                        }
                    }
                    Ok(())
                },
            )?;
        }
        _ => {
            repo.odb().with_pack_read_context(|| {
                for &idx in batch {
                    crate::progress::poll_cancel(progress, cancel)?;
                    let bytes = match prepare_write_job(repo, conv, &jobs[idx]) {
                        Ok(bytes) => bytes,
                        Err(err) => {
                            flush_prepared_writes_serial(
                                repo, work_tree, jobs, &prepared, dir_cache, outcomes, progress,
                                cancel,
                            )?;
                            return Err(err);
                        }
                    };
                    prepared_bytes += bytes.as_ref().map_or(0, Vec::len);
                    prepared.push(PreparedWrite { idx, bytes });
                    if prepared_bytes >= PREPARED_WRITE_BYTES {
                        flush_prepared_batch(
                            repo, pool, work_tree, jobs, &prepared, dir_cache, outcomes, progress,
                            cancel,
                        )?;
                        prepared.clear();
                        prepared_bytes = 0;
                    }
                }
                Ok(())
            })?;
        }
    }
    flush_prepared_batch(
        repo, pool, work_tree, jobs, &prepared, dir_cache, outcomes, progress, cancel,
    )
}

#[allow(clippy::too_many_arguments)]
fn flush_prepared_batch(
    repo: &Repository,
    pool: Option<ParallelOptions>,
    work_tree: &Path,
    jobs: &[CheckoutWriteJob],
    prepared: &[PreparedWrite],
    dir_cache: &mut LeadingDirCache,
    outcomes: &mut [Option<WriteJobOutcome>],
    progress: &mut dyn crate::progress::ProgressSink,
    cancel: &dyn crate::progress::Cancel,
) -> Result<()> {
    if prepared.is_empty() {
        return Ok(());
    }
    if let Some(options) = pool {
        let parent_snapshot = LeadingDirCache::with_ready_from(&*dir_cache);
        flush_prepared_writes_parallel(
            repo,
            options,
            work_tree,
            jobs,
            prepared,
            &parent_snapshot,
            outcomes,
        )?;
    } else {
        flush_prepared_writes_serial(
            repo, work_tree, jobs, prepared, dir_cache, outcomes, progress, cancel,
        )?;
    }
    for _ in prepared {
        progress.inc(1);
        crate::progress::poll_cancel(progress, cancel)?;
    }
    Ok(())
}

/// Materialize write jobs in input order: one ODB read and conversion per job.
///
/// Parallel-eligible jobs are batched, but each batch is flushed before the next serial job runs
/// so missing blobs and filter errors match the historical serial checkout behavior. Batches are
/// also flushed at [`ChunkSizes`] boundaries so cancellation is polled between them; with
/// `skip_chunk_ramp` they start at full size.
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_write_jobs(
    repo: &Repository,
    rules: &WorktreeRules,
    config: &Arc<ConfigSet>,
    conv: &ConversionConfig,
    work_tree: &Path,
    dir_cache: &mut LeadingDirCache,
    delayed: &mut DelayedProcessCheckout,
    jobs: &[CheckoutWriteJob],
    parallel_indices: &HashSet<usize>,
    parallel_prepare_blobs: bool,
    checkout_parallel_threshold: Option<i64>,
    skip_chunk_ramp: bool,
    progress: &mut dyn crate::progress::ProgressSink,
    cancel: &dyn crate::progress::Cancel,
) -> Result<Vec<WriteJobOutcome>> {
    let mut outcomes: Vec<Option<WriteJobOutcome>> = vec![None; jobs.len()];
    let pool = parallel_pool_for_checkout(
        repo,
        config,
        parallel_indices.len(),
        checkout_parallel_threshold,
    );
    let mut chunks = if skip_chunk_ramp {
        ChunkSizes::full()
    } else {
        ChunkSizes::new()
    };
    let mut parallel_batch: Vec<usize> = Vec::new();

    for idx in 0..jobs.len() {
        crate::progress::poll_cancel(progress, cancel)?;
        if parallel_indices.contains(&idx) {
            parallel_batch.push(idx);
            if parallel_batch.len() >= chunks.current() {
                flush_parallel_batch(
                    repo,
                    conv,
                    work_tree,
                    jobs,
                    &parallel_batch,
                    dir_cache,
                    &mut outcomes,
                    pool,
                    parallel_prepare_blobs,
                    progress,
                    cancel,
                )?;
                parallel_batch.clear();
                chunks.grow();
            }
            continue;
        }
        flush_parallel_batch(
            repo,
            conv,
            work_tree,
            jobs,
            &parallel_batch,
            dir_cache,
            &mut outcomes,
            pool,
            parallel_prepare_blobs,
            progress,
            cancel,
        )?;
        parallel_batch.clear();
        let outcome =
            materialize_write_job_serial(repo, rules, work_tree, dir_cache, &jobs[idx], delayed)?;
        outcomes[idx] = Some(outcome);
        progress.inc(1);
    }
    flush_parallel_batch(
        repo,
        conv,
        work_tree,
        jobs,
        &parallel_batch,
        dir_cache,
        &mut outcomes,
        pool,
        parallel_prepare_blobs,
        progress,
        cancel,
    )?;

    outcomes
        .into_iter()
        .enumerate()
        .map(|(i, o)| {
            o.ok_or_else(|| Error::PathError(format!("missing checkout outcome for job {i}")))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignorecase_basename_collision_marks_serial() {
        let paths = vec!["FILE_x".to_owned(), "file_x".to_owned(), "other".to_owned()];
        let serial = paths_needing_serial_checkout(&paths, true, false);
        assert!(serial.contains("FILE_x"));
        assert!(serial.contains("file_x"));
        assert!(!serial.contains("other"));
    }

    #[test]
    fn prefix_collision_marks_serial_when_normalization_enabled() {
        let paths = vec!["dir".to_owned(), "dir/file".to_owned(), "other".to_owned()];
        let serial = paths_needing_serial_checkout(&paths, false, true);
        assert!(serial.contains("dir"));
        assert!(serial.contains("dir/file"));
        assert!(!serial.contains("other"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "wall-clock performance check")]
    fn collision_scan_scales_linearly() {
        let paths: Vec<String> = (0..10_000).map(|i| format!("files/{i}.txt")).collect();
        let start = std::time::Instant::now();
        let serial = paths_needing_serial_checkout(&paths, true, false);
        let elapsed = start.elapsed();
        assert!(serial.is_empty());
        assert!(
            elapsed.as_secs() < 2,
            "collision scan took {:?}, expected near-linear",
            elapsed
        );
    }

    #[test]
    fn filter_smudge_not_parallel_eligible() {
        let attrs = FileAttrs {
            filter_smudge: Some("rot13".into()),
            ..FileAttrs::default()
        };
        assert!(!is_eligible_for_parallel_checkout(MODE_REGULAR, &attrs));
    }

    fn checkout_options(threads: usize, threshold: i64) -> ParallelOptions {
        ParallelOptions {
            threads: std::num::NonZeroUsize::new(threads).expect("nonzero"),
            min_items: 0,
            min_total_bytes: None,
            signed_entry_threshold: Some(threshold),
        }
    }

    #[test]
    fn threshold_is_judged_on_the_whole_checkout_not_each_chunk() {
        let pool = chunk_pool_options(checkout_options(4, 100), 2_000)
            .expect("2000 files clear a threshold of 100");
        assert_eq!(pool.threads.get(), 4);
        assert_eq!(pool.signed_entry_threshold, None);
        assert!(
            checkout_pool_worthwhile(PARALLEL_WRITE_FIRST_CHUNK, pool),
            "a cancellation-sized chunk must still be written by the pool"
        );
    }

    #[test]
    fn small_or_single_threaded_checkouts_stay_serial() {
        assert_eq!(chunk_pool_options(checkout_options(4, 100), 99), None);
        assert_eq!(chunk_pool_options(checkout_options(1, 0), 2_000), None);
        assert!(chunk_pool_options(checkout_options(4, -5), 2).is_some());
    }

    #[test]
    fn chunk_pool_respects_explicit_policy_thresholds_before_clearing_them() {
        let options = ParallelOptions {
            min_items: 1_000,
            ..checkout_options(4, 100)
        };
        assert_eq!(chunk_pool_options(options, 999), None);
        assert!(chunk_pool_options(options, 1_000).is_some());
        assert_eq!(
            chunk_pool_options(
                ParallelOptions {
                    min_total_bytes: Some(1),
                    ..options
                },
                1_000,
            ),
            None,
        );
        assert_eq!(chunk_pool_options(checkout_options(4, 0), 1), None);
    }

    #[test]
    fn cancellable_chunks_cover_every_item_once_and_keep_the_selected_pool() {
        let items: Vec<usize> = (0..10_000).collect();
        let (chunks, options) = cancellable_chunks(&items, checkout_options(4, 100));
        let chunks: Vec<_> = chunks.collect();
        assert_eq!(chunks[0].len(), PARALLEL_WRITE_FIRST_CHUNK);
        assert!(chunks
            .iter()
            .all(|chunk| chunk.len() <= PARALLEL_WRITE_MAX_CHUNK));
        assert_eq!(chunks.concat(), items);
        assert!(checkout_pool_worthwhile(chunks[0].len(), options));

        let (_, options) = cancellable_chunks(&items[..99], checkout_options(4, 100));
        assert_eq!(options.threads.get(), 1);
    }

    #[test]
    fn flush_chunks_double_from_the_first_chunk_up_to_the_cap() {
        let mut chunks = ChunkSizes::new();
        let mut seen = vec![chunks.current()];
        for _ in 0..12 {
            chunks.grow();
            seen.push(chunks.current());
        }
        assert_eq!(seen[0], PARALLEL_WRITE_FIRST_CHUNK);
        assert!(seen
            .windows(2)
            .all(|w| w[1] == (w[0] * 2).min(PARALLEL_WRITE_MAX_CHUNK)));
        assert_eq!(seen.last().copied(), Some(PARALLEL_WRITE_MAX_CHUNK));
    }
}
