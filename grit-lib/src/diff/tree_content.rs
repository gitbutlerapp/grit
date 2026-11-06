//! Parallel blob loads for tree-to-tree diffs (commit ranges, `grit diff A B`).

use crate::config::ConfigSet;
use crate::diff_rename_parallel::parallel_map_indices;
use crate::error::{Error, Result};
use crate::merge_diff::{diff_attr_forces_text, diff_driver_forces_text, diff_textconv_active};
use crate::objects::FileMode;
use crate::objects::ObjectId;
use crate::objects::ObjectKind;
use crate::objects::OidHashSet;
use crate::odb::Odb;
use crate::pack_store::PackReadContextGuard;
use crate::parallel::run_ordered_batches;
use crate::parallel::{try_par_map_with_local, ParallelHashError};
use crate::parallel::{ParallelOp, ParallelOptions, ThreadPolicy};
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;

use super::detail::{line_change_counts, LineStat};
use super::structured::{structured_file_diff, SideBytes, StructuredFileDiff};
use super::DiffEntry;
use super::DiffOptions;

fn note_blob_read() {
    crate::test_probe_hooks::hooks::diff_blob_read();
}

fn collect_tree_diff_blob_oids(entries: &[DiffEntry]) -> Vec<ObjectId> {
    let mut set = OidHashSet::default();
    for entry in entries {
        if entry.old_mode != FileMode::GITLINK
            && entry.old_mode != FileMode::ABSENT
            && !entry.old_oid.is_zero()
        {
            set.insert(entry.old_oid);
        }
        if entry.new_mode != FileMode::GITLINK
            && entry.new_mode != FileMode::ABSENT
            && !entry.new_oid.is_zero()
        {
            set.insert(entry.new_oid);
        }
    }
    set.into_iter().collect()
}

pub(crate) fn prefetch_tree_diff_blobs(odb: &Odb, entries: &[DiffEntry]) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    odb.prefetch_promisor_read_batch(collect_tree_diff_blob_oids(entries))
}

/// Fewest changed entries for which per-entry blob loads and line diffs use worker threads.
///
/// Starting workers (stacks, allocator arenas, per-thread pack inflaters) costs more than
/// loading and diffing a small commit's blobs on the calling thread: showing git.git commits
/// that touch up to 17 files ran faster serially, while from about 18 files the pool won.
/// Fewer, larger files gain nothing either, since each entry is diffed by one thread.
const PER_ENTRY_POOL_MIN_ITEMS: usize = 16;

/// Pool options for per-entry content work, fanning out once there are enough entries.
///
/// Below [`PER_ENTRY_POOL_MIN_ITEMS`] entries (or the policy's
/// [`ThreadPolicy::min_items_override`], never below two) the work runs on the calling
/// thread and the policy, which probes the CPU count and cgroup quota, is not consulted.
fn per_entry_parallel_options(
    thread_policy: &ThreadPolicy,
    config: &ConfigSet,
    entry_count: usize,
) -> ParallelOptions {
    let min_items = thread_policy
        .min_items_override
        .unwrap_or(PER_ENTRY_POOL_MIN_ITEMS)
        .max(2);
    if entry_count < min_items {
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
    /// Old-side byte length (for binary `--stat` rows).
    pub old_size: u64,
    /// New-side byte length (for binary `--stat` rows).
    pub new_size: u64,
    /// Worktree/index side bytes when content is not materialized in the ODB.
    pub patch_bytes: Option<(SideBytes, SideBytes)>,
}

/// One tree diff entry with both sides loaded for line-level diffing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeDiffContent {
    /// Tree diff metadata (paths, modes, oids, status).
    pub entry: DiffEntry,
    /// Old-side bytes prepared for structured text diffing.
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
    prefetch_tree_diff_blobs(odb, &entries)?;
    let mut par_opts = thread_policy.resolve(ParallelOp::DiffContent, config);
    par_opts.min_items = par_opts.min_items.min(8);
    let est_bytes = entries.len().saturating_mul(8192);
    let rows = parallel_map_indices(odb, entries.len(), par_opts, est_bytes, |i| {
        let entry = &entries[i];
        let (old_bytes, old_bin) = load_old_side(odb, entry)?;
        let (new_bytes, new_bin) = load_new_side(odb, entry)?;
        Ok::<TreeDiffContent, Error>(TreeDiffContent {
            entry: entry.clone(),
            old_bytes,
            new_bytes,
            binary: old_bin || new_bin,
        })
    })?;
    Ok(rows)
}

/// One tree diff entry's stat row: metadata, binary flag, line counts, side byte lengths.
pub(crate) type TreeDiffStatRow = (DiffEntry, bool, Option<LineStat>, u64, u64);

/// Per-file line counts for stat modes (loads blobs, no hunk allocation).
///
/// Worker threads are used only for 16 or more entries (or the policy's
/// [`ThreadPolicy::min_items_override`]); smaller diffs run on the calling thread.
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
) -> Result<Vec<TreeDiffStatRow>> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    prefetch_tree_diff_blobs(odb, &entries)?;
    let par_opts = per_entry_parallel_options(thread_policy, config, entries.len());
    let line_diff_opts = diff_options;
    let store = odb.pack_store_for(odb.objects_dir());
    let rows = try_par_map_with_local(
        &entries,
        par_opts,
        0,
        || PackReadContextGuard::install(Arc::clone(&store)),
        move |_ctx, entry| {
            if !entry.old_oid.is_zero()
                && entry.old_oid == entry.new_oid
                && entry.old_mode == entry.new_mode
            {
                let (old_bytes, old_bin) = load_old_side(odb, entry)?;
                let binary = old_bin;
                let size = old_bytes.byte_len();
                return Ok((
                    entry.clone(),
                    binary,
                    if binary {
                        None
                    } else {
                        Some(LineStat::default())
                    },
                    size,
                    size,
                ));
            }
            let (old_bytes, old_bin) = load_old_side(odb, entry)?;
            let (new_bytes, new_bin) = load_new_side(odb, entry)?;
            let binary = old_bin || new_bin;
            let old_size = old_bytes.byte_len();
            let new_size = new_bytes.byte_len();
            let stat = if binary {
                None
            } else {
                Some(line_change_counts(
                    &String::from_utf8_lossy(&old_bytes.data),
                    &String::from_utf8_lossy(&new_bytes.data),
                    &line_diff_opts,
                ))
            };
            Ok((entry.clone(), binary, stat, old_size, new_size))
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
    if !entry.old_oid.is_zero()
        && entry.old_oid == entry.new_oid
        && entry.old_mode == entry.new_mode
    {
        let (old_bytes, old_bin) = load_old_side(odb, entry)?;
        let size = old_bytes.byte_len();
        return Ok(TreeDiffStructured {
            entry: entry.clone(),
            structured: StructuredFileDiff {
                old_mode: entry.old_mode.octal_if_present(),
                new_mode: entry.new_mode.octal_if_present(),
                old_encoding_lossy: old_bytes.encoding_lossy,
                new_encoding_lossy: old_bytes.encoding_lossy,
                hunks: Vec::new(),
            },
            binary: old_bin,
            old_size: size,
            new_size: size,
            patch_bytes: None,
        });
    }
    let (old_bytes, old_bin) = load_old_side(odb, entry)?;
    let (new_bytes, new_bin) = load_new_side(odb, entry)?;
    let binary = old_bin || new_bin;
    let old_size = old_bytes.byte_len();
    let new_size = new_bytes.byte_len();
    let structured = if binary {
        StructuredFileDiff {
            old_mode: entry.old_mode.octal_if_present(),
            new_mode: entry.new_mode.octal_if_present(),
            old_encoding_lossy: old_bytes.encoding_lossy,
            new_encoding_lossy: new_bytes.encoding_lossy,
            hunks: Vec::new(),
        }
    } else {
        structured_file_diff(
            entry.old_mode,
            entry.new_mode,
            &old_bytes,
            &new_bytes,
            diff_options.clone(),
        )
    };
    Ok(TreeDiffStructured {
        entry: entry.clone(),
        structured,
        binary,
        old_size,
        new_size,
        patch_bytes: None,
    })
}

/// Visit structured tree diffs in path order without retaining every file's hunks at once.
///
/// When thread policy allows, workers compute up to about `2 × threads` entries ahead while
/// the callback consumes results strictly in order.
///
/// Worker threads are used only for 16 or more entries (or the policy's
/// [`ThreadPolicy::min_items_override`]); smaller diffs run on the calling thread.
///
/// # Errors
///
/// Propagates object database read failures and callback errors.
pub fn for_each_tree_diff_structured(
    odb: &Odb,
    entries: &[DiffEntry],
    diff_options: DiffOptions,
    config: &ConfigSet,
    thread_policy: &ThreadPolicy,
    mut visit: impl FnMut(TreeDiffStructured) -> Result<()>,
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    prefetch_tree_diff_blobs(odb, entries)?;
    let diff_options = diff_options.cap_word_level_inline_for_changed_files(entries.len());
    let par_opts = per_entry_parallel_options(thread_policy, config, entries.len());
    let n_workers = par_opts.threads.get();
    if n_workers <= 1 || entries.len() < 2 {
        for entry in entries {
            visit(tree_diff_structured_entry(odb, entry, &diff_options)?)?;
        }
        return Ok(());
    }
    let line_diff_opts = diff_options;
    let store = odb.pack_store_for(odb.objects_dir());
    run_ordered_batches(
        entries,
        1,
        n_workers,
        || PackReadContextGuard::install(Arc::clone(&store)),
        move |_ctx, entry| tree_diff_structured_entry(odb, entry, &line_diff_opts),
        move |_batch, rows| {
            for row in rows {
                visit(row)?;
            }
            Ok(())
        },
    )
}

/// Load sides and compute structured line diffs for each entry in parallel.
///
/// Worker threads are used only for 16 or more entries (or the policy's
/// [`ThreadPolicy::min_items_override`]); smaller diffs run on the calling thread.
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
    prefetch_tree_diff_blobs(odb, &entries)?;
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

pub(crate) fn load_old_side(odb: &Odb, e: &DiffEntry) -> Result<(SideBytes, bool)> {
    if e.old_mode == FileMode::GITLINK {
        return Ok((
            SideBytes::from_bytes(gitlink_line(&e.old_oid).into_bytes()),
            false,
        ));
    }
    if e.old_mode == FileMode::ABSENT {
        return Ok((SideBytes::from_bytes(Vec::new()), false));
    }
    let path = e.old_path.as_deref().or(e.new_path.as_deref());
    load_odb_blob_side_for_diff(odb, &e.old_oid, path, false)
}

pub(crate) fn load_new_side(odb: &Odb, e: &DiffEntry) -> Result<(SideBytes, bool)> {
    if e.new_mode == FileMode::GITLINK {
        return Ok((
            SideBytes::from_bytes(gitlink_line(&e.new_oid).into_bytes()),
            false,
        ));
    }
    if e.new_mode == FileMode::ABSENT {
        return Ok((SideBytes::from_bytes(Vec::new()), false));
    }
    let path = e.new_path.as_deref().or(e.old_path.as_deref());
    load_odb_blob_side_for_diff(odb, &e.new_oid, path, false)
}

/// Text a gitlink side contributes to a patch: `Subproject commit <hex>\n`,
/// or nothing for the zero id (an absent side).
pub(crate) fn gitlink_line(oid: &ObjectId) -> String {
    if *oid == ObjectId::zero() {
        return String::new();
    }
    format!("Subproject commit {}\n", oid.to_hex())
}

fn git_dir_for_diff(odb: &Odb) -> Option<&Path> {
    odb.config_git_dir().or_else(|| odb.objects_dir().parent())
}

/// True when a path's attributes or textconv require reading a blob above
/// [`Odb::big_file_threshold`](crate::objects::odb::Odb::big_file_threshold).
fn diff_side_must_read_large_blob(odb: &Odb, path: &str) -> Result<bool> {
    let Some(git_dir) = git_dir_for_diff(odb) else {
        return Ok(false);
    };
    if diff_attr_forces_text(git_dir, path) {
        return Ok(true);
    }
    let config = crate::config::ConfigSet::load(
        &crate::environment::Environment::empty(),
        Some(git_dir),
        &crate::config::LoadConfigOptions::standard(
            &crate::environment::Environment::empty(),
            Some(git_dir),
        ),
    )?;
    if diff_textconv_active(git_dir, &config, path) {
        return Ok(true);
    }
    Ok(diff_driver_forces_text(git_dir, &config, path))
}

/// Whether a blob/worktree side at `size` bytes should be treated as binary without reading
/// content (Git `diff_populate_filespec`; strict `>` on `core.bigFileThreshold`).
pub(crate) fn treat_side_as_binary_without_reading(
    odb: &Odb,
    size: u64,
    path: Option<&str>,
    force_read_content: bool,
) -> Result<bool> {
    if force_read_content || size == 0 {
        return Ok(false);
    }
    let threshold = odb.big_file_threshold()?;
    if size <= threshold {
        return Ok(false);
    }
    if let Some(path) = path {
        if diff_side_must_read_large_blob(odb, path)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Classify blob bytes for diffing after inflation.
///
/// When `retain_binary_payload` is false, binary sides drop payload bytes (line diffs stay lean).
/// When true (patch-text / `--binary` emission), payload is kept and `binary` is still set.
pub(crate) fn side_bytes_after_blob_read(
    data: Vec<u8>,
    retain_binary_payload: bool,
) -> (SideBytes, bool) {
    let binary = crate::crlf::is_binary(&data);
    let size = data.len() as u64;
    if binary && !retain_binary_payload {
        (SideBytes::empty_with_size(size), true)
    } else {
        (SideBytes::from_bytes(data), binary)
    }
}

/// Load ODB blob bytes for diffing, skipping inflation when over `core.bigFileThreshold`.
///
/// Returns empty side bytes with `binary = true` when content is not read. When
/// `force_read_content` is true, large blobs are inflated and binary payload bytes are retained
/// (for `GIT binary patch` / `git apply`).
///
/// # Errors
///
/// Propagates object database and config errors.
pub fn load_odb_blob_side_for_diff(
    odb: &Odb,
    oid: &ObjectId,
    path: Option<&str>,
    force_read_content: bool,
) -> Result<(SideBytes, bool)> {
    if *oid == ObjectId::zero() {
        return Ok((SideBytes::empty_with_size(0), false));
    }
    if !force_read_content {
        let info = odb.read_info(oid)?;
        if info.kind == ObjectKind::Blob
            && treat_side_as_binary_without_reading(odb, info.size, path, false)?
        {
            return Ok((SideBytes::empty_with_size(info.size), true));
        }
    }
    note_blob_read();
    #[cfg(test)]
    odb.hot_path_metrics().record_blob_content_read();
    let data = odb.read(oid)?.data;
    Ok(side_bytes_after_blob_read(data, force_read_content))
}

/// Classify raw blob bytes for line diffs (NUL in the first 8 KiB ⇒ binary).
#[must_use]
pub fn bytes_for_diff(data: Vec<u8>) -> (SideBytes, bool) {
    let size = data.len() as u64;
    if crate::crlf::is_binary(&data) {
        (SideBytes::empty_with_size(size), true)
    } else {
        (SideBytes::from_bytes(data), false)
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
        let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        (dir, repo)
    }

    fn init_repo_with_core_config(snippet: &str) -> (tempfile::TempDir, Repository) {
        let dir = tempfile::tempdir().expect("tempdir");
        let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        std::fs::write(repo.git_dir().join("config"), snippet).expect("config");
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
        let config = repo.config().snapshot().expect("config");
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
    fn small_entry_counts_run_serially_without_consulting_the_policy() {
        let config = ConfigSet::new();
        let policy = ThreadPolicy::with_threads_override(Some(4));
        for count in [0, 1, 2, PER_ENTRY_POOL_MIN_ITEMS - 1] {
            let opts = per_entry_parallel_options(&policy, &config, count);
            assert_eq!(opts.threads, NonZeroUsize::MIN, "{count} entries");
        }
        for count in [PER_ENTRY_POOL_MIN_ITEMS, PER_ENTRY_POOL_MIN_ITEMS * 4] {
            let opts = per_entry_parallel_options(&policy, &config, count);
            assert_eq!(opts.min_items, 0);
            assert_eq!(opts.min_total_bytes, None);
            assert_eq!(
                opts.threads,
                policy.resolve(ParallelOp::DiffContent, &config).threads
            );
        }
    }

    #[test]
    fn min_items_override_moves_the_pool_threshold() {
        let config = ConfigSet::new();
        let threads = ThreadPolicy::with_threads_override(Some(4))
            .resolve(ParallelOp::DiffContent, &config)
            .threads;
        let forced = ThreadPolicy::with_threads_override(Some(4)).min_items_override(Some(0));
        assert_eq!(
            per_entry_parallel_options(&forced, &config, 1).threads,
            NonZeroUsize::MIN
        );
        assert_eq!(
            per_entry_parallel_options(&forced, &config, 2).threads,
            threads
        );
        let raised = ThreadPolicy::with_threads_override(Some(4)).min_items_override(Some(100));
        assert_eq!(
            per_entry_parallel_options(&raised, &config, 99).threads,
            NonZeroUsize::MIN
        );
        assert_eq!(
            per_entry_parallel_options(&raised, &config, 100).threads,
            threads
        );
    }

    #[test]
    fn structured_diff_below_and_above_pool_threshold_matches_serial() {
        let (_dir, repo) = init_repo();
        let odb = repo.odb();
        let tree_of = |version: &str, files: usize| {
            let entries: Vec<TreeEntry> = (0..files)
                .map(|i| TreeEntry {
                    mode: 0o100644,
                    name: format!("f{i:03}.txt").into_bytes(),
                    oid: odb
                        .write(
                            ObjectKind::Blob,
                            format!("file {i}\n{version}\nend\n").as_bytes(),
                        )
                        .expect("blob"),
                })
                .collect();
            odb.write(ObjectKind::Tree, &serialize_tree(&entries))
                .expect("tree")
        };
        let config = repo.config().snapshot().expect("config");
        for files in [PER_ENTRY_POOL_MIN_ITEMS - 1, PER_ENTRY_POOL_MIN_ITEMS + 1] {
            let (old, new) = (tree_of("old", files), tree_of("new", files));
            let entries = diff_trees(odb, Some(&old), Some(&new), "").expect("entries");
            assert_eq!(entries.len(), files);
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
            assert_eq!(serial, run(4), "{files} files");
            assert!(serial.iter().all(|f| f.structured.hunks.len() == 1));
        }
    }

    #[test]
    fn big_file_threshold_honors_diff_driver_binary_false() -> Result<()> {
        let (dir, repo) = init_repo_with_core_config(
            "[core]\n\tbigFileThreshold = 4k\n\n[diff \"foo\"]\n\tbinary = false\n",
        );
        std::fs::write(dir.path().join(".gitattributes"), "*.bin diff=foo\n")?;
        let odb = repo.odb();
        let mut old_body = vec![b'a'; 100];
        old_body.extend(std::iter::repeat_n(b'x', 4900));
        assert_eq!(old_body.len(), 5000);
        let mut new_body = old_body.clone();
        new_body[0] = b'b';
        let old_oid = odb.write(ObjectKind::Blob, &old_body)?;
        let new_oid = odb.write(ObjectKind::Blob, &new_body)?;
        let tree_old = odb.write(
            ObjectKind::Tree,
            &serialize_tree(&[TreeEntry {
                mode: 0o100644,
                name: b"a.bin".to_vec(),
                oid: old_oid,
            }]),
        )?;
        let tree_new = odb.write(
            ObjectKind::Tree,
            &serialize_tree(&[TreeEntry {
                mode: 0o100644,
                name: b"a.bin".to_vec(),
                oid: new_oid,
            }]),
        )?;
        let entries = diff_trees(odb, Some(&tree_old), Some(&tree_new), "")?;
        assert_eq!(entries.len(), 1);
        let metrics = odb.hot_path_metrics();
        metrics.set_blob_counting(true);
        metrics.reset_blob_content_reads();
        let structured = tree_diff_structured_entry(odb, &entries[0], &DiffOptions::default())?;
        metrics.set_blob_counting(false);
        assert!(
            !structured.binary,
            "diff.foo.binary=false must force a textual diff"
        );
        assert!(
            !structured.structured.hunks.is_empty(),
            "expected unified hunks, not binary short-circuit"
        );
        assert!(
            metrics.blob_content_reads() >= 2,
            "both sides must be read for a line diff"
        );
        Ok(())
    }

    #[test]
    fn tree_diff_skips_reading_blobs_over_big_file_threshold() {
        let (_dir, repo) = init_repo_with_core_config("[core]\n\tbigFileThreshold = 4k\n");
        let odb = repo.odb();

        let big_a = odb
            .write(ObjectKind::Blob, &vec![b'a'; 5000])
            .expect("big blob a");
        let big_b = odb
            .write(ObjectKind::Blob, &vec![b'b'; 5001])
            .expect("big blob b");
        let tree_old = odb
            .write(
                ObjectKind::Tree,
                &serialize_tree(&[TreeEntry {
                    mode: 0o100644,
                    name: b"f.bin".to_vec(),
                    oid: big_a,
                }]),
            )
            .expect("old tree");
        let tree_new = odb
            .write(
                ObjectKind::Tree,
                &serialize_tree(&[TreeEntry {
                    mode: 0o100644,
                    name: b"f.bin".to_vec(),
                    oid: big_b,
                }]),
            )
            .expect("new tree");
        let entries = diff_trees(odb, Some(&tree_old), Some(&tree_new), "").expect("entries");
        let metrics = odb.hot_path_metrics();
        metrics.set_blob_counting(true);
        metrics.reset_blob_content_reads();
        let config = repo.config().snapshot().expect("config");
        let _ = tree_diff_stat_parallel(
            odb,
            entries,
            &config,
            &ThreadPolicy::default(),
            DiffOptions::default(),
        )
        .expect("stat");
        metrics.set_blob_counting(false);
        assert_eq!(metrics.blob_content_reads(), 0);
    }

    #[test]
    fn blob_at_exactly_big_file_threshold_is_still_read() {
        let (_dir, repo) = init_repo_with_core_config("[core]\n\tbigFileThreshold = 4k\n");
        let odb = repo.odb();

        let at_threshold = odb
            .write(ObjectKind::Blob, &vec![b'b'; 4096])
            .expect("at threshold");
        let other = odb.write(ObjectKind::Blob, b"z\n").expect("other");
        let tree_old = odb
            .write(
                ObjectKind::Tree,
                &serialize_tree(&[TreeEntry {
                    mode: 0o100644,
                    name: b"g.bin".to_vec(),
                    oid: other,
                }]),
            )
            .expect("old tree");
        let tree_new = odb
            .write(
                ObjectKind::Tree,
                &serialize_tree(&[TreeEntry {
                    mode: 0o100644,
                    name: b"g.bin".to_vec(),
                    oid: at_threshold,
                }]),
            )
            .expect("new tree");
        let entries = diff_trees(odb, Some(&tree_old), Some(&tree_new), "").expect("entries");
        let metrics = odb.hot_path_metrics();
        metrics.set_blob_counting(true);
        metrics.reset_blob_content_reads();
        let config = repo.config().snapshot().expect("config");
        let _ = tree_diff_stat_parallel(
            odb,
            entries,
            &config,
            &ThreadPolicy::default(),
            DiffOptions::default(),
        )
        .expect("stat");
        metrics.set_blob_counting(false);
        assert!(metrics.blob_content_reads() >= 1);
    }

    #[test]
    fn blob_under_big_file_threshold_is_read_for_stat() {
        let (_dir, repo) = init_repo_with_core_config("[core]\n\tbigFileThreshold = 4k\n");
        let odb = repo.odb();

        let under = odb
            .write(ObjectKind::Blob, &vec![b'c'; 4000])
            .expect("under");
        let other = odb.write(ObjectKind::Blob, b"x\n").expect("other");
        let tree_a = odb
            .write(
                ObjectKind::Tree,
                &serialize_tree(&[TreeEntry {
                    mode: 0o100644,
                    name: b"h.bin".to_vec(),
                    oid: other,
                }]),
            )
            .expect("tree a");
        let tree_b = odb
            .write(
                ObjectKind::Tree,
                &serialize_tree(&[TreeEntry {
                    mode: 0o100644,
                    name: b"h.bin".to_vec(),
                    oid: under,
                }]),
            )
            .expect("tree b");
        let entries = diff_trees(odb, Some(&tree_a), Some(&tree_b), "").expect("entries");
        let metrics = odb.hot_path_metrics();
        metrics.set_blob_counting(true);
        metrics.reset_blob_content_reads();
        let config = repo.config().snapshot().expect("config");
        let _ = tree_diff_stat_parallel(
            odb,
            entries,
            &config,
            &ThreadPolicy::default(),
            DiffOptions::default(),
        )
        .expect("stat");
        metrics.set_blob_counting(false);
        assert!(metrics.blob_content_reads() >= 2);
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
        let config = repo.config().snapshot().expect("config");
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
