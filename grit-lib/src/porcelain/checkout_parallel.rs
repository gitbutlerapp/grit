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
use crate::objects::ObjectId;
use crate::parallel::{try_par_map_with_local, ParallelHashError, ParallelOp, ParallelOptions};
use crate::repo::Repository;
use crate::unicode_normalization::precompose_utf8_path;
use crate::worktree_rules::WorktreeRules;

use super::{
    worktree_bytes_from_index_blob, write_checkout_entry, DelayedCheckoutTarget, LeadingDirCache,
};

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

#[cfg(test)]
/// Whether two repository-relative paths can refer to the same on-disk file (Git `paths_collide`).
#[must_use]
pub(crate) fn paths_collide(a: &str, b: &str, ignorecase: bool) -> bool {
    if a.len() == b.len() {
        return paths_equal(a, b, ignorecase);
    }
    if a.len() < b.len() {
        return b.as_bytes().get(a.len()) == Some(&b'/')
            && paths_equal(a, &b[..a.len()], ignorecase);
    }
    a.as_bytes().get(b.len()) == Some(&b'/') && paths_equal(&a[..b.len()], b, ignorecase)
}

#[cfg(test)]
fn paths_equal(a: &str, b: &str, ignorecase: bool) -> bool {
    if ignorecase {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
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
    if options.threads.get() <= 1 || len == 0 {
        return false;
    }
    if let Some(threshold) = options.signed_entry_threshold {
        if (len as i64) < threshold {
            return false;
        }
    }
    true
}

struct ParallelWriteWorker {
    work_tree: PathBuf,
    dir_cache: LeadingDirCache,
}

struct PreparedWrite {
    idx: usize,
    bytes: Vec<u8>,
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
) -> Result<Vec<u8>> {
    let object = repo.odb().read(&job.oid)?;
    worktree_bytes_for_parallel_job(conv, job, &object.data)
}

fn write_prepared_job(
    work_tree: &Path,
    jobs: &[CheckoutWriteJob],
    prepared: &PreparedWrite,
    dir_cache: &mut LeadingDirCache,
) -> Result<IndexEntry> {
    let job = &jobs[prepared.idx];
    write_checkout_entry(
        work_tree,
        &job.path,
        &prepared.bytes,
        job.mode,
        &job.change,
        dir_cache,
        job.oid,
    )
}

fn flush_prepared_writes_serial(
    work_tree: &Path,
    jobs: &[CheckoutWriteJob],
    prepared: &[PreparedWrite],
    dir_cache: &mut LeadingDirCache,
    outcomes: &mut [Option<WriteJobOutcome>],
) -> Result<()> {
    for item in prepared {
        let entry = write_prepared_job(work_tree, jobs, item, dir_cache)?;
        outcomes[item.idx] = Some(WriteJobOutcome::Index(entry));
    }
    Ok(())
}

fn flush_prepared_writes_parallel(
    repo: &Repository,
    config: &Arc<ConfigSet>,
    work_tree: &Path,
    jobs: &[CheckoutWriteJob],
    prepared: &[PreparedWrite],
    parent_dirs: &LeadingDirCache,
    outcomes: &mut [Option<WriteJobOutcome>],
) -> Result<()> {
    let policy = repo.thread_policy();
    let options = policy.resolve(ParallelOp::Checkout, config.as_ref());
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
            let job = &jobs[item.idx];
            write_checkout_entry(
                &worker.work_tree,
                &job.path,
                &item.bytes,
                job.mode,
                &job.change,
                &mut worker.dir_cache,
                job.oid,
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
    let object = repo.odb().read(&job.oid)?;
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
        &worktree_bytes,
        job.mode,
        &job.change,
        dir_cache,
        job.oid,
    )?;
    Ok(WriteJobOutcome::Index(entry))
}

#[allow(clippy::too_many_arguments)]
fn flush_parallel_batch(
    repo: &Repository,
    config: &Arc<ConfigSet>,
    conv: &ConversionConfig,
    work_tree: &Path,
    jobs: &[CheckoutWriteJob],
    batch: &[usize],
    dir_cache: &mut LeadingDirCache,
    outcomes: &mut [Option<WriteJobOutcome>],
    parallel_prepare_blobs: bool,
    checkout_parallel_threshold: Option<i64>,
) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let policy = repo.thread_policy();
    let mut options = policy.resolve(ParallelOp::Checkout, config.as_ref());
    if let Some(threshold) = checkout_parallel_threshold {
        options.signed_entry_threshold = Some(threshold);
    }
    let use_parallel = options.threads.get() > 1 && checkout_pool_worthwhile(batch.len(), options);

    let prepared: Vec<PreparedWrite> = if parallel_prepare_blobs
        && options.threads.get() > 1
        && batch.len() > 1
    {
        try_par_map_with_local(
            batch,
            options,
            0,
            || (),
            |_, &idx| {
                prepare_write_job(repo, conv, &jobs[idx]).map(|bytes| PreparedWrite { idx, bytes })
            },
        )
        .map_err(|e: ParallelHashError<Error>| match e {
            ParallelHashError::Task(err) => err,
        })?
    } else {
        let mut prepared = Vec::with_capacity(batch.len());
        for &idx in batch {
            match prepare_write_job(repo, conv, &jobs[idx]) {
                Ok(bytes) => prepared.push(PreparedWrite { idx, bytes }),
                Err(err) => {
                    flush_prepared_writes_serial(work_tree, jobs, &prepared, dir_cache, outcomes)?;
                    return Err(err);
                }
            }
        }
        prepared
    };

    if use_parallel {
        let parent_snapshot = LeadingDirCache::with_ready_from(&*dir_cache);
        flush_prepared_writes_parallel(
            repo,
            config,
            work_tree,
            jobs,
            &prepared,
            &parent_snapshot,
            outcomes,
        )
    } else {
        flush_prepared_writes_serial(work_tree, jobs, &prepared, dir_cache, outcomes)
    }
}

/// Materialize write jobs in input order: one ODB read and conversion per job.
///
/// Parallel-eligible jobs are batched, but each batch is flushed before the next serial job runs
/// so missing blobs and filter errors match the historical serial checkout behavior.
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
) -> Result<Vec<WriteJobOutcome>> {
    let mut outcomes: Vec<Option<WriteJobOutcome>> = vec![None; jobs.len()];
    let mut parallel_batch: Vec<usize> = Vec::new();

    for idx in 0..jobs.len() {
        if parallel_indices.contains(&idx) {
            parallel_batch.push(idx);
            continue;
        }
        flush_parallel_batch(
            repo,
            config,
            conv,
            work_tree,
            jobs,
            &parallel_batch,
            dir_cache,
            &mut outcomes,
            parallel_prepare_blobs,
            checkout_parallel_threshold,
        )?;
        parallel_batch.clear();
        let outcome =
            materialize_write_job_serial(repo, rules, work_tree, dir_cache, &jobs[idx], delayed)?;
        outcomes[idx] = Some(outcome);
    }
    flush_parallel_batch(
        repo,
        config,
        conv,
        work_tree,
        jobs,
        &parallel_batch,
        dir_cache,
        &mut outcomes,
        parallel_prepare_blobs,
        checkout_parallel_threshold,
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
    fn paths_collide_dir_and_file() {
        assert!(paths_collide("a", "a/b", false));
        assert!(paths_collide("A/B", "a", true));
        assert!(!paths_collide("a/b", "a/c", false));
    }

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
}
