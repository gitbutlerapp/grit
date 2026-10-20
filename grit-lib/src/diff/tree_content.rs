//! Parallel blob loads for tree-to-tree diffs (commit ranges, `grit diff A B`).

use crate::config::ConfigSet;
use crate::diff_rename_parallel::parallel_map_indices;
use crate::error::{Error, Result};
use crate::objects::pack_store::PackReadContextGuard;
use crate::objects::ObjectId;
use crate::odb::Odb;
use crate::parallel::{try_par_map_with_local, ParallelHashError};
use crate::parallel::{ParallelOp, ThreadPolicy};
use std::sync::Arc;

use super::structured::{structured_file_diff, SideBytes, StructuredFileDiff};
use super::DiffEntry;
use super::DiffOptions;

/// Tree diff entry with structured line hunks ready for CLI rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeDiffStructured {
    /// Tree diff metadata (paths, modes, oids, status).
    pub entry: DiffEntry,
    /// Structured hunks and mode metadata.
    pub structured: StructuredFileDiff,
    /// `true` when either side looks like a binary blob (NUL in the first 8 KiB).
    pub binary: bool,
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
    let mut par_opts = thread_policy.resolve(ParallelOp::DiffContent, config);
    par_opts.min_items = 0;
    par_opts.min_total_bytes = None;
    let store = odb.pack_store_for(odb.objects_dir());
    let rows = try_par_map_with_local(
        &entries,
        par_opts,
        0,
        || PackReadContextGuard::install(Arc::clone(&store)),
        |_ctx, entry| {
            if !entry.old_oid.is_zero()
                && entry.old_oid == entry.new_oid
                && entry.old_mode == entry.new_mode
            {
                return Ok(TreeDiffStructured {
                    entry: entry.clone(),
                    structured: StructuredFileDiff {
                        old_mode: mode_display(&entry.old_mode),
                        new_mode: mode_display(&entry.new_mode),
                        old_encoding_lossy: false,
                        new_encoding_lossy: false,
                        hunks: Vec::new(),
                    },
                    binary: false,
                });
            }
            let (old_bytes, old_bin) = load_old_side(odb, entry)?;
            let (new_bytes, new_bin) = load_new_side(odb, entry)?;
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
                    diff_options,
                )
            };
            Ok(TreeDiffStructured {
                entry: entry.clone(),
                structured,
                binary,
            })
        },
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

fn load_old_side(odb: &Odb, e: &DiffEntry) -> Result<(SideBytes, bool)> {
    if e.old_mode == "160000" {
        return Ok((
            SideBytes::from_bytes(gitlink_line(&e.old_oid).into_bytes()),
            false,
        ));
    }
    if e.old_mode == "000000" {
        return Ok((SideBytes::from_bytes(Vec::new()), false));
    }
    blob_bytes(odb, &e.old_oid)
}

fn load_new_side(odb: &Odb, e: &DiffEntry) -> Result<(SideBytes, bool)> {
    if e.new_mode == "160000" {
        return Ok((
            SideBytes::from_bytes(gitlink_line(&e.new_oid).into_bytes()),
            false,
        ));
    }
    if e.new_mode == "000000" {
        return Ok((SideBytes::from_bytes(Vec::new()), false));
    }
    blob_bytes(odb, &e.new_oid)
}

fn gitlink_line(oid: &ObjectId) -> String {
    if *oid == ObjectId::zero() {
        return String::new();
    }
    format!("Subproject commit {}\n", oid.to_hex())
}

fn blob_bytes(odb: &Odb, oid: &ObjectId) -> Result<(SideBytes, bool)> {
    if *oid == ObjectId::zero() {
        return Ok((SideBytes::from_bytes(Vec::new()), false));
    }
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
    use crate::diff::{diff_trees, DiffStatus};
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
}
