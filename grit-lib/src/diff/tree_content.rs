//! Parallel blob loads for tree-to-tree diffs (commit ranges, `grit diff A B`).

use crate::config::ConfigSet;
use crate::diff_rename_parallel::parallel_map_indices;
use crate::error::{Error, Result};
use crate::objects::pack_store::PackReadContextGuard;
use crate::objects::ObjectId;
use crate::odb::Odb;
use crate::parallel::{try_par_map_with_local, ParallelHashError};
use crate::parallel::{ParallelOp, ParallelOptions, ThreadPolicy};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use super::detail::{line_change_counts, LineStat};
use super::structured::{structured_file_diff, SideBytes, StructuredFileDiff};
use super::DiffEntry;
use super::DiffOptions;

fn note_blob_read(counter: Option<&AtomicUsize>) {
    if let Some(c) = counter {
        c.fetch_add(1, Ordering::Relaxed);
    }
}

/// Pool options for per-entry content work that should fan out whenever it can.
///
/// A single entry always runs on the calling thread, so the policy (which probes the
/// CPU count and cgroup quota) is only consulted when there is more than one.
fn per_entry_parallel_options(
    thread_policy: &ThreadPolicy,
    config: &ConfigSet,
    entry_count: usize,
) -> ParallelOptions {
    if entry_count < 2 {
        return ParallelOptions {
            threads: NonZeroUsize::MIN,
            min_items: 0,
            min_total_bytes: None,
            signed_entry_threshold: None,
        };
    }
    let mut par_opts = thread_policy.resolve(ParallelOp::DiffContent, config);
    par_opts.min_items = 0;
    par_opts.min_total_bytes = None;
    par_opts
}

/// Tree diff entry with structured line hunks ready for CLI rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeDiffStructured {
    /// Tree diff metadata (paths, modes, oids, status).
    pub entry: DiffEntry,
    /// Structured hunks and mode metadata.
    pub structured: StructuredFileDiff,
    /// `true` when either side looks like a binary blob (NUL in the first 8 KiB).
    pub binary: bool,
    /// Worktree/index side bytes when content is not materialized in the ODB.
    pub patch_bytes: Option<(SideBytes, SideBytes)>,
}

/// One tree diff entry with both sides loaded for line-level diffing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeDiffContent {
    /// Tree diff metadata (paths, modes, oids, status).
    pub entry: DiffEntry,
    /// Old-side bytes prepared for [`super::structured::structured_text_diff`].
    pub old_bytes: SideBytes,
    /// New-side bytes prepared for structured diffing.
    pub new_bytes: SideBytes,
    /// `true` when either side looks like a binary blob (NUL in the first 8 KiB).
    pub binary: bool,
}

/// Load old/new blob bytes for each entry, using a worker pool when thresholds allow.
///
/// Submodule gitlinks are synthesized as `Subproject commit <oid>` lines; missing sides
/// are empty. Order matches `entries`.
///
/// # Errors
///
/// Propagates object database read failures.
pub fn tree_diff_contents_parallel(
    odb: &Odb,
    entries: Vec<DiffEntry>,
    config: &ConfigSet,
    thread_policy: &ThreadPolicy,
) -> Result<Vec<TreeDiffContent>> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let mut par_opts = thread_policy.resolve(ParallelOp::DiffContent, config);
    par_opts.min_items = par_opts.min_items.min(8);
    let est_bytes = entries.len().saturating_mul(8192);
    let rows = parallel_map_indices(odb, entries.len(), par_opts, est_bytes, |i| {
        let entry = &entries[i];
        let (old_bytes, old_bin) = load_old_side(odb, entry, None)?;
        let (new_bytes, new_bin) = load_new_side(odb, entry, None)?;
        Ok::<TreeDiffContent, Error>(TreeDiffContent {
            entry: entry.clone(),
            old_bytes,
            new_bytes,
            binary: old_bin || new_bin,
        })
    })?;
    Ok(rows)
}

/// Per-file line counts for stat modes (loads blobs, no hunk allocation).
///
/// # Errors
///
/// Propagates object database read failures.
pub fn tree_diff_stat_parallel(
    odb: &Odb,
    entries: Vec<DiffEntry>,
    config: &ConfigSet,
    thread_policy: &ThreadPolicy,
    diff_options: DiffOptions,
) -> Result<Vec<(DiffEntry, bool, Option<LineStat>)>> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let par_opts = per_entry_parallel_options(thread_policy, config, entries.len());
    let line_diff_opts = diff_options;
    let blob_counter = line_diff_opts.blob_read_counter.clone();
    let store = odb.pack_store_for(odb.objects_dir());
    let rows = try_par_map_with_local(
        &entries,
        par_opts,
        0,
        || PackReadContextGuard::install(Arc::clone(&store)),
        move |_ctx, entry| {
            let blob_reads = blob_counter.as_deref();
            if !entry.old_oid.is_zero()
                && entry.old_oid == entry.new_oid
                && entry.old_mode == entry.new_mode
            {
                let (_, old_bin) = load_old_side(odb, entry, blob_reads)?;
                let binary = old_bin;
                return Ok((
                    entry.clone(),
                    binary,
                    if binary {
                        None
                    } else {
                        Some(LineStat::default())
                    },
                ));
            }
            let (old_bytes, old_bin) = load_old_side(odb, entry, blob_reads)?;
            let (new_bytes, new_bin) = load_new_side(odb, entry, blob_reads)?;
            let binary = old_bin || new_bin;
            let stat = if binary {
                None
            } else {
                Some(line_change_counts(
                    &old_bytes.text(),
                    &new_bytes.text(),
                    &line_diff_opts,
                ))
            };
            Ok((entry.clone(), binary, stat))
        },
    );
    match rows {
        Ok(rows) => Ok(rows),
        Err(ParallelHashError::Task(err)) => Err(err),
    }
}

/// Load sides and compute a structured line diff for one tree diff entry.
///
/// # Errors
///
/// Propagates object database read failures.
pub fn tree_diff_structured_entry(
    odb: &Odb,
    entry: &DiffEntry,
    diff_options: &DiffOptions,
) -> Result<TreeDiffStructured> {
    let blob_reads = diff_options.blob_read_counter.as_deref();
    if !entry.old_oid.is_zero()
        && entry.old_oid == entry.new_oid
        && entry.old_mode == entry.new_mode
    {
        let (old_bytes, old_bin) = load_old_side(odb, entry, blob_reads)?;
        return Ok(TreeDiffStructured {
            entry: entry.clone(),
            structured: StructuredFileDiff {
                old_mode: mode_display(&entry.old_mode),
                new_mode: mode_display(&entry.new_mode),
                old_encoding_lossy: old_bytes.encoding_lossy,
                new_encoding_lossy: old_bytes.encoding_lossy,
                hunks: Vec::new(),
            },
            binary: old_bin,
            patch_bytes: None,
        });
    }
    let (old_bytes, old_bin) = load_old_side(odb, entry, blob_reads)?;
    let (new_bytes, new_bin) = load_new_side(odb, entry, blob_reads)?;
    let binary = old_bin || new_bin;
    let structured = if binary {
        StructuredFileDiff {
            old_mode: mode_display(&entry.old_mode),
            new_mode: mode_display(&entry.new_mode),
            old_encoding_lossy: old_bytes.encoding_lossy,
            new_encoding_lossy: new_bytes.encoding_lossy,
            hunks: Vec::new(),
        }
    } else {
        structured_file_diff(
            &entry.old_mode,
            &entry.new_mode,
            &old_bytes,
            &new_bytes,
            diff_options.clone(),
        )
    };
    Ok(TreeDiffStructured {
        entry: entry.clone(),
        structured,
        binary,
        patch_bytes: None,
    })
}

/// Visit structured tree diffs one entry at a time (serial blob loads and hunk generation).
///
/// Use this when rendering or writing patch output should not retain every file's hunks at once.
///
/// # Errors
///
/// Propagates object database read failures and callback errors.
pub fn for_each_tree_diff_structured(
    odb: &Odb,
    entries: &[DiffEntry],
    diff_options: DiffOptions,
    mut visit: impl FnMut(TreeDiffStructured) -> Result<()>,
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let diff_options = diff_options.cap_word_level_inline_for_changed_files(entries.len());
    for entry in entries {
        visit(tree_diff_structured_entry(odb, entry, &diff_options)?)?;
    }
    Ok(())
}

/// Load sides and compute structured line diffs for each entry in parallel.
///
/// # Errors
///
/// Propagates object database read failures.
pub fn tree_diff_structured_parallel(
    odb: &Odb,
    entries: Vec<DiffEntry>,
    config: &ConfigSet,
    thread_policy: &ThreadPolicy,
    diff_options: DiffOptions,
) -> Result<Vec<TreeDiffStructured>> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let diff_options = diff_options.cap_word_level_inline_for_changed_files(entries.len());
    let par_opts = per_entry_parallel_options(thread_policy, config, entries.len());
    let line_diff_opts = diff_options;
    let store = odb.pack_store_for(odb.objects_dir());
    let rows = try_par_map_with_local(
        &entries,
        par_opts,
        0,
        || PackReadContextGuard::install(Arc::clone(&store)),
        move |_ctx, entry| tree_diff_structured_entry(odb, entry, &line_diff_opts),
    );
    match rows {
        Ok(rows) => Ok(rows),
        Err(ParallelHashError::Task(err)) => Err(err),
    }
}

fn mode_display(mode: &str) -> Option<String> {
    if mode == "000000" {
        None
    } else {
        Some(mode.to_owned())
    }
}

fn load_old_side(
    odb: &Odb,
    e: &DiffEntry,
    blob_reads: Option<&AtomicUsize>,
) -> Result<(SideBytes, bool)> {
    if e.old_mode == "160000" {
        return Ok((
            SideBytes::from_bytes(gitlink_line(&e.old_oid).into_bytes()),
            false,
        ));
    }
    if e.old_mode == "000000" {
        return Ok((SideBytes::from_bytes(Vec::new()), false));
    }
    blob_bytes(odb, &e.old_oid, blob_reads)
}

fn load_new_side(
    odb: &Odb,
    e: &DiffEntry,
    blob_reads: Option<&AtomicUsize>,
) -> Result<(SideBytes, bool)> {
    if e.new_mode == "160000" {
        return Ok((
            SideBytes::from_bytes(gitlink_line(&e.new_oid).into_bytes()),
            false,
        ));
    }
    if e.new_mode == "000000" {
        return Ok((SideBytes::from_bytes(Vec::new()), false));
    }
    blob_bytes(odb, &e.new_oid, blob_reads)
}

fn gitlink_line(oid: &ObjectId) -> String {
    if *oid == ObjectId::zero() {
        return String::new();
    }
    format!("Subproject commit {}\n", oid.to_hex())
}

fn blob_bytes(
    odb: &Odb,
    oid: &ObjectId,
    blob_reads: Option<&AtomicUsize>,
) -> Result<(SideBytes, bool)> {
    if *oid == ObjectId::zero() {
        return Ok((SideBytes::from_bytes(Vec::new()), false));
    }
    note_blob_read(blob_reads);
    let data = odb.read(oid)?.data;
    Ok(bytes_for_diff(&data))
}

fn bytes_for_diff(data: &[u8]) -> (SideBytes, bool) {
    if data.iter().take(8000).any(|&b| b == 0) {
        (SideBytes::from_bytes(Vec::new()), true)
    } else {
        (SideBytes::from_bytes(data.to_vec()), false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigSet;
    use crate::diff::{diff_trees, DiffOptions, DiffStatus};
    use crate::objects::{serialize_tree, ObjectKind, TreeEntry};
    use crate::parallel::ThreadPolicy;
    use crate::repo::{InitOptions, Repository};

    fn init_repo() -> (tempfile::TempDir, Repository) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        (dir, repo)
    }

    #[test]
    fn tree_diff_contents_parallel_matches_serial_load() {
        let (_dir, repo) = init_repo();
        let odb = repo.odb();
        let a = odb.write(ObjectKind::Blob, b"alpha\n").expect("write a");
        let b = odb.write(ObjectKind::Blob, b"beta\n").expect("write b");
        let tree_a = odb
            .write(
                ObjectKind::Tree,
                &serialize_tree(&[TreeEntry {
                    mode: 0o100644,
                    name: b"file-a".to_vec(),
                    oid: a,
                }]),
            )
            .expect("tree a");
        let tree_b = odb
            .write(
                ObjectKind::Tree,
                &serialize_tree(&[TreeEntry {
                    mode: 0o100644,
                    name: b"file-b".to_vec(),
                    oid: b,
                }]),
            )
            .expect("tree b");
        let raw = diff_trees(odb, Some(&tree_a), Some(&tree_b), "").expect("diff");
        assert_eq!(raw.len(), 2);
        let config = repo.config().expect("config");
        let serial = tree_diff_contents_parallel(
            odb,
            raw.clone(),
            &config,
            &ThreadPolicy::with_threads_override(Some(1)),
        )
        .expect("serial load");
        let parallel = tree_diff_contents_parallel(
            odb,
            raw,
            &config,
            &ThreadPolicy::with_threads_override(Some(4)),
        )
        .expect("parallel load");
        assert_eq!(serial, parallel);
        assert!(serial
            .iter()
            .any(|c| c.entry.status == DiffStatus::Added || c.entry.status == DiffStatus::Deleted));
    }

    #[test]
    fn name_only_does_not_read_blobs_for_tree_to_tree() {
        use crate::config::ConfigSet;
        use crate::diff::{build_file_diffs, DiffDetail, DiffOptions};

        let (_dir, repo) = init_repo();
        let odb = repo.odb();
        let a = odb.write(ObjectKind::Blob, b"alpha\n").expect("write a");
        let b = odb.write(ObjectKind::Blob, b"beta\n").expect("write b");
        let old = odb
            .write(
                ObjectKind::Tree,
                &serialize_tree(&[TreeEntry {
                    mode: 0o100644,
                    name: b"f.txt".to_vec(),
                    oid: a,
                }]),
            )
            .expect("old tree");
        let new = odb
            .write(
                ObjectKind::Tree,
                &serialize_tree(&[TreeEntry {
                    mode: 0o100644,
                    name: b"f.txt".to_vec(),
                    oid: b,
                }]),
            )
            .expect("new tree");
        let entries = diff_trees(odb, Some(&old), Some(&new), "").expect("entries");
        let metrics = odb.hot_path_metrics();
        metrics.set_blob_counting(true);
        metrics.reset_blob_content_reads();
        let config = ConfigSet::new();
        let _ = build_file_diffs(
            odb,
            entries,
            DiffDetail::NameOnly,
            DiffOptions::default(),
            &config,
            &ThreadPolicy::default(),
        )
        .expect("name-only");
        metrics.set_blob_counting(false);
        assert_eq!(metrics.blob_content_reads(), 0);
    }

    #[test]
    fn single_entry_runs_serially_without_consulting_the_policy() {
        let config = ConfigSet::new();
        let policy = ThreadPolicy::with_threads_override(Some(4));
        for count in [0, 1] {
            let opts = per_entry_parallel_options(&policy, &config, count);
            assert_eq!(opts.threads, NonZeroUsize::MIN);
        }
        let opts = per_entry_parallel_options(&policy, &config, 2);
        assert_eq!(opts.min_items, 0);
        assert_eq!(opts.min_total_bytes, None);
        assert_eq!(
            opts.threads,
            policy.resolve(ParallelOp::DiffContent, &config).threads
        );
    }

    #[test]
    fn single_entry_structured_diff_matches_threaded_policy() {
        let (_dir, repo) = init_repo();
        let odb = repo.odb();
        let a = odb.write(ObjectKind::Blob, b"one\ntwo\n").expect("write a");
        let b = odb
            .write(ObjectKind::Blob, b"one\nthree\n")
            .expect("write b");
        let tree = |oid| {
            odb.write(
                ObjectKind::Tree,
                &serialize_tree(&[TreeEntry {
                    mode: 0o100644,
                    name: b"f.txt".to_vec(),
                    oid,
                }]),
            )
            .expect("tree")
        };
        let (old, new) = (tree(a), tree(b));
        let entries = diff_trees(odb, Some(&old), Some(&new), "").expect("entries");
        assert_eq!(entries.len(), 1);
        let config = repo.config().expect("config");
        let run = |threads| {
            tree_diff_structured_parallel(
                odb,
                entries.clone(),
                &config,
                &ThreadPolicy::with_threads_override(Some(threads)),
                DiffOptions::default(),
            )
            .expect("structured diff")
        };
        let serial = run(1);
        assert_eq!(serial, run(4));
        assert_eq!(serial[0].structured.hunks.len(), 1);
    }
}
