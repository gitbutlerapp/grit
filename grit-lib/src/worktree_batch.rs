//! Parallel read, filter, hash, and compress of worktree file blobs (add / index refresh).

use std::fs;
use std::path::{Path, PathBuf};

use crate::config::ConfigSet;
use crate::crlf::{self, ConversionConfig, GitAttributes};
use crate::diff::{hash_worktree_file, mode_from_metadata, worktree_blob_bytes};
use crate::error::{Error, Result};
use crate::hash::{hash_object, try_par_hash_with, ParallelHashError, Parallelism};
use crate::index::IndexEntry;
use crate::objects::{HashAlgo, ObjectId, ObjectKind};
use crate::odb::{self, Odb};

/// One worktree path to read and normalize in parallel.
pub(crate) struct WorktreeBlobReadInput {
    /// Absolute path to read.
    pub abs: PathBuf,
    /// Index-relative path (for attributes / CRLF).
    pub index_relpath: String,
    /// Stage-0 index entry when conversion may use the recorded OID blob.
    pub index_entry: Option<IndexEntry>,
}

/// Worktree blob after parallel read, hash, and zlib compression.
pub(crate) struct PreparedWorktreeBlob {
    /// Index path spelling used for staging.
    pub index_relpath: String,
    /// Object id of the normalized blob (computed in the parallel phase).
    pub oid: ObjectId,
    /// Zlib-compressed canonical store bytes for [`Odb::write_loose_zlib_prehashed`].
    pub zlib_store: Vec<u8>,
    /// Worktree metadata captured during the read pass.
    pub meta: fs::Metadata,
    /// Git mode derived from `meta`.
    pub mode: u32,
}

struct PrepareBlobContext {
    algo: HashAlgo,
    compression: flate2::Compression,
}

/// Read, CRLF-filter, hash, and compress many worktree blobs in parallel (input order preserved).
///
/// # Errors
///
/// Propagates I/O, conversion, hashing, or compression failures from any worker.
pub(crate) fn prepare_worktree_blobs_parallel(
    odb: &Odb,
    items: &[WorktreeBlobReadInput],
    conv: &ConversionConfig,
    attrs: &GitAttributes,
    config: &ConfigSet,
    parallelism: Parallelism,
) -> Result<Vec<PreparedWorktreeBlob>> {
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let ctx = PrepareBlobContext {
        algo: odb.hash_algo(),
        compression: odb.loose_compression()?,
    };
    let total_bytes = items.len().saturating_mul(4096);
    let threads = parallelism.threads();
    let prepared = try_par_hash_with(items, threads, total_bytes, |item| {
        let meta = fs::symlink_metadata(&item.abs).map_err(Error::Io)?;
        let file_attrs = crlf::get_file_attrs(attrs, &item.index_relpath, false, config);
        let data = worktree_blob_bytes(
            odb,
            &item.abs,
            &meta,
            conv,
            &file_attrs,
            &item.index_relpath,
            item.index_entry.as_ref(),
        )?;
        let store_bytes = odb::blob_store_bytes(&data);
        let oid = hash_object(ctx.algo, ObjectKind::Blob, &data);
        let zlib_store = odb::zlib_compress_loose_store_from_bytes(&store_bytes, ctx.compression)?;
        let mode = mode_from_metadata(&meta);
        Ok(PreparedWorktreeBlob {
            index_relpath: item.index_relpath.clone(),
            oid,
            zlib_store,
            meta,
            mode,
        })
    })
    .map_err(|e: ParallelHashError<Error>| match e {
        ParallelHashError::Task(err) => err,
    })?;
    Ok(prepared)
}

/// Action to apply after parallel content verification during index stat refresh.
#[allow(dead_code)]
pub(crate) enum RefreshHashOutcome {
    /// Racy entry: content no longer matches the recorded OID.
    InvalidateStat,
    /// Stat was stale but content still matches: adopt worktree stat fields.
    AdoptStat(fs::Metadata),
}

enum RefreshHashKind {
    RacyVerify,
    StatAdopt,
}

pub(crate) struct RefreshHashWork {
    pub(crate) entry_index: usize,
    pub(crate) abs: PathBuf,
    pub(crate) meta: fs::Metadata,
    pub(crate) expected_oid: ObjectId,
    kind: RefreshHashKind,
}

fn refresh_work_item_for_entry(
    entry_index: usize,
    entries: &[IndexEntry],
    work_tree: &Path,
    index_mtime: Option<(u32, u32)>,
) -> Option<RefreshHashWork> {
    use crate::diff::{entry_is_racy, stat_matches};
    use crate::index::{MODE_EXECUTABLE, MODE_REGULAR, MODE_SYMLINK};

    let ie = &entries[entry_index];
    if ie.stage() != 0 || ie.skip_worktree() || ie.assume_unchanged() || ie.intent_to_add() {
        return None;
    }
    if ie.mode != MODE_REGULAR && ie.mode != MODE_EXECUTABLE && ie.mode != MODE_SYMLINK {
        return None;
    }
    let Ok(rel_path) = std::str::from_utf8(&ie.path) else {
        return None;
    };
    let abs = work_tree.join(rel_path);
    let Ok(meta) = fs::symlink_metadata(&abs) else {
        return None;
    };
    if stat_matches(ie, &meta) {
        if entry_is_racy(ie, index_mtime) {
            return Some(RefreshHashWork {
                entry_index,
                abs,
                meta,
                expected_oid: ie.oid,
                kind: RefreshHashKind::RacyVerify,
            });
        }
        return None;
    }
    Some(RefreshHashWork {
        entry_index,
        abs,
        meta,
        expected_oid: ie.oid,
        kind: RefreshHashKind::StatAdopt,
    })
}

// Parallel stat/hash refresh helpers kept for embedders; index refresh uses directory-grouped scan in `diff`.
#[allow(dead_code)]
/// Build parallel refresh work items from a snapshot of index entries (read-only).
pub(crate) fn collect_refresh_hash_work(
    entries: &[IndexEntry],
    work_tree: &Path,
    index_mtime: Option<(u32, u32)>,
) -> Vec<RefreshHashWork> {
    let mut work = Vec::new();
    for entry_index in 0..entries.len() {
        if let Some(item) =
            refresh_work_item_for_entry(entry_index, entries, work_tree, index_mtime)
        {
            work.push(item);
        }
    }
    work
}

#[allow(dead_code)]
/// Like [`collect_refresh_hash_work`], but stat-probes index entries in parallel when worthwhile.
pub(crate) fn collect_refresh_hash_work_parallel(
    entries: &[IndexEntry],
    work_tree: &Path,
    index_mtime: Option<(u32, u32)>,
    parallelism: Parallelism,
) -> Result<Vec<RefreshHashWork>> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let threads = parallelism.threads().get();
    let est_bytes = entries.len().saturating_mul(512);
    if !crate::hash::parallel_hash_worthwhile(entries.len(), est_bytes, threads) {
        return Ok(collect_refresh_hash_work(entries, work_tree, index_mtime));
    }
    let indices: Vec<usize> = (0..entries.len()).collect();
    let rows = try_par_hash_with(&indices, parallelism.threads(), est_bytes, |&i| {
        Ok(refresh_work_item_for_entry(
            i,
            entries,
            work_tree,
            index_mtime,
        ))
    })
    .map_err(|e: ParallelHashError<Error>| match e {
        ParallelHashError::Task(err) => err,
    })?;
    Ok(rows.into_iter().flatten().collect())
}

#[allow(dead_code)]
/// Hash refresh candidates in parallel; returns `(entry_index, action)` pairs in input order.
///
/// # Errors
///
/// Propagates I/O or hashing failures from any worker.
pub(crate) fn parallel_refresh_index_stat_hashes(
    odb: &Odb,
    entries: &[IndexEntry],
    work: &[RefreshHashWork],
    conv: &ConversionConfig,
    attrs: &GitAttributes,
    config: &ConfigSet,
    parallelism: Parallelism,
) -> Result<Vec<(usize, RefreshHashOutcome)>> {
    if work.is_empty() {
        return Ok(Vec::new());
    }
    let total_bytes: usize = work.iter().map(|w| w.meta.len() as usize).sum();
    let threads = parallelism.threads();
    try_par_hash_with(work, threads, total_bytes, |item| {
        let ie = &entries[item.entry_index];
        let rel_path = std::str::from_utf8(&ie.path)
            .map_err(|_| Error::Message("index path is not valid UTF-8".into()))?;
        let file_attrs = crlf::get_file_attrs(attrs, rel_path, false, config);
        let wt_oid = hash_worktree_file(
            odb,
            &item.abs,
            &item.meta,
            conv,
            &file_attrs,
            rel_path,
            Some(ie),
        )?;
        let content_matches = wt_oid == item.expected_oid;
        let outcome = match item.kind {
            RefreshHashKind::RacyVerify if !content_matches => {
                Some(RefreshHashOutcome::InvalidateStat)
            }
            RefreshHashKind::StatAdopt if content_matches => {
                Some(RefreshHashOutcome::AdoptStat(item.meta.clone()))
            }
            RefreshHashKind::RacyVerify | RefreshHashKind::StatAdopt => None,
        };
        Ok((item.entry_index, outcome))
    })
    .map_err(|e: ParallelHashError<Error>| match e {
        ParallelHashError::Task(err) => err,
    })
    .map(|rows| {
        rows.into_iter()
            .filter_map(|(idx, outcome)| outcome.map(|o| (idx, o)))
            .collect()
    })
}
