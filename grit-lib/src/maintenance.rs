//! Repository maintenance: repack, fsck, and garbage collection orchestration.
//!
//! These APIs wrap existing pack, prune, and fsck primitives for embedders and the
//! `grit` CLI. They never shell out to the system `git` binary.
//!
//! Library guide: [maintenance](https://grit-scm.com/docs/library/maintenance)
//!
//! ```rust
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let _repo = Repository::init(dir.path(), &InitOptions::default())?;
//! # Ok(())
//! # }
//! ```

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use crate::alternate_boundary::AlternateSources;
use crate::commit_graph_file::CommitGraphChain;
use crate::commit_graph_write::{
    atomic_replace_commit_graph, build_commit_graph_bytes_from_oid_maps,
    collect_reachable_commit_oids_for_repository, load_commit_graph_commit_info,
    remove_commit_graph_chain_dir, remove_commit_graph_files, BloomWriteStats,
};
use crate::config::{canonical_key, parse_bool, ConfigSet};
use crate::diff::bloom::BloomFilterSettings;
use crate::environment::Environment;
use crate::error::{ConfigError, Error, Result};
use crate::fsck as repo_fsck;
use crate::gc::{prune_loose_unreachable_with, LazyReachable, PruneStats, S_IFDIR, S_IFMT};
use crate::git_date::approx::approxidate_careful_at;
use crate::index_pack::{install_pack_bytes, install_pack_path, IngestPackOptions};
use crate::midx::{
    clear_pack_midx_state, midx_version_from_config, midx_write_version,
    write_multi_pack_index_with_options, MidxBitmapWriteOutcome, WriteMultiPackIndexOptions,
    WriteMultiPackIndexResult,
};
use crate::objects::{
    oid_hash_map, tag_object_line_oid, CommitRef, FileMode, ObjectId, ObjectKind, OidHashSet,
    TreeRefIter,
};
use crate::odb::Odb;
use crate::odb::UnreachablePackRetention;
use crate::pack_bitmap::{PackBitmapWriteError, PackBitmapWriteOptions, PackBitmapWriter};
use crate::pack_delta::{pack_delta_search_parallel_options, pack_write_parallel_options};
use crate::pack_name_hash::pack_name_hash_child;
use crate::pack_object_select::{
    enumerate_pack_objects_naming, NameTrees, PackEnumerateOptions, PackObjectSelection,
};
use crate::pack_objects::{PackBuildOptions, SerializedDeltaPack};
use crate::pack_rev::write_pack_rev_sidecar_for_index_path;
use crate::prune_packed::{prune_packed_objects, PrunePackedOptions};
use crate::reflog::{
    expire_reflog_git_with_cache, load_gc_reflog_expire_config, reflog_refs_on_store,
    ReflogExpireParams, ReflogExpiryCache,
};
use crate::repack_pack::{write_repack_pack, RepackPackSettings};
use crate::repo::Repository;
use crate::shallow::load_shallow_boundaries;
use time::OffsetDateTime;

/// Options for [`repack_all()`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RepackOptions {
    /// Report actions without writing packs or deleting files.
    pub dry_run: bool,
    /// After repack, write `objects/info/commit-graph` when the ODB is files-backed.
    pub write_commit_graph: bool,
    /// After repack, write a `.bitmap` sidecar for the new pack (files-backed only).
    pub write_bitmap: bool,
    /// When set, prune unreachable loose objects not newer than this instant after repack.
    pub prune_older_than: Option<OffsetDateTime>,
    /// Wall clock passed to maintenance helpers that require an explicit time (e.g. bitmap writes).
    pub now: Option<OffsetDateTime>,
    /// When true (the default), do not delete unreachable loose objects after repack (matches `git repack -ad`).
    pub keep_unreachable: bool,
    /// Worker threads for pack indexing (`None` reads `pack.threads` / CPU count).
    pub threads: Option<usize>,
    /// Override the delta settings of a full repack; `None` uses
    /// [`PackBuildOptions::for_local_copy`] (`pack.window` / `pack.depth` from config).
    ///
    /// A full repack honors `delta`, `window`, `max_depth`, `reuse_deltas`, `reuse_objects`,
    /// `use_ofs_delta` and `use_bitmaps`. Objects already in a pack keep their stored form
    /// when the reuse flags allow it; only the rest go through the `window` search.
    pub pack_build: Option<PackBuildOptions>,
    /// Config/environment for ingest and pack threads (defaults to repo config).
    pub merged_config: Option<ConfigSet>,
    /// Process environment for config discovery and thread counts.
    pub environment: Environment,
    /// Pack only the reachable loose objects into a new pack, keeping existing packs (Git
    /// `repack -d` without `-a`). Unreachable loose objects stay loose for the prune step.
    /// The new pack is delta-compressed like a full repack, with bases taken only from the
    /// loose objects it holds, so it is self-contained.
    ///
    /// When [`Self::write_bitmap`] is true and the repository already has pack files, bitmaps are
    /// written via a multi-pack-index sidecar instead of a per-pack `.bitmap` on the new pack alone.
    /// Bitmaps are omitted when known promises prevent a complete closure.
    pub incremental: bool,
    /// When true, [`gc()`] may skip repack on a single pack with no loose objects when every
    /// packed object is still reachable and none would be dropped by the active prune policy.
    /// Explicit [`repack_all`] / [`repack`] leave this false so maintenance still runs.
    pub auto_skip: bool,
    /// When true, recompute every delta instead of keeping the ones stored in packs
    /// (`git repack -f`, `grit repack --force`). Whole objects that stay whole are
    /// still copied verbatim.
    pub force_recompress: bool,
}

impl Default for RepackOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            write_commit_graph: false,
            write_bitmap: false,
            prune_older_than: None,
            now: None,
            keep_unreachable: true,
            threads: None,
            pack_build: None,
            merged_config: None,
            environment: Environment::empty(),
            incremental: false,
            auto_skip: false,
            force_recompress: false,
        }
    }
}

impl RepackOptions {
    /// Start from [`Default::default`] with [`Self::dry_run`] set.
    #[must_use]
    pub fn with_dry_run(dry_run: bool) -> Self {
        Self {
            dry_run,
            ..Self::default()
        }
    }
}

struct RepackDetail {
    report: RepackReport,
    pack_path: Option<PathBuf>,
    prune_packed_loose: usize,
    unreachable_loose: PruneStats,
    unreachable_prune_ms: u64,
    unreachable_prune_ran: bool,
}

/// Counters produced by [`repack`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RepackReport {
    /// New pack files installed (0 or 1 for all-into-one repack).
    pub packs_written: usize,
    /// Pack sidecar files removed from `objects/pack/` (excluding `.keep` and promisor packs).
    pub packs_removed: usize,
    /// Loose object files removed (`prune-packed` plus unreachable prune).
    pub loose_removed: usize,
    /// Objects written into the new pack.
    pub objects: usize,
}

/// Extended counters and paths produced by [`repack_all`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RepackStats {
    /// Loose object files before repack.
    pub loose_before: usize,
    /// Pack files before repack.
    pub packs_before: usize,
    /// Objects written into the new local pack, or estimated for a dry run.
    /// `None` when no objects need packing.
    pub objects_packed: Option<usize>,
    /// Bytes in the new local pack, excluding retained packs (`None` on dry run).
    pub pack_bytes: Option<u64>,
    /// Path to the installed local pack, even when other packs are retained.
    /// `None` on dry run or when nothing was packed.
    pub pack_path: Option<PathBuf>,
    /// Sidecar/index files removed from `objects/pack/`.
    pub removed_pack_files: usize,
    /// Loose objects removed because they were redundant with the new pack.
    pub pruned_loose: usize,
    /// Unreachable loose objects removed after repack when pruning is enabled.
    pub unreachable_loose_pruned: usize,
    /// Unreachable loose objects kept (grace window or concurrent safety).
    pub unreachable_loose_kept: usize,
    /// Wall time spent in unreachable loose-object pruning during this repack.
    pub unreachable_prune_ms: u64,
    /// True when unreachable loose-object pruning was executed (even if nothing was removed).
    pub unreachable_prune_ran: bool,
}

/// Options for [`Repository::write_commit_graph`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CommitGraphOptions {
    /// When true, emit Bloom filter chunks for changed-path queries.
    pub changed_paths: bool,
    /// Report without writing `objects/info/commit-graph`.
    pub dry_run: bool,
}

impl CommitGraphOptions {
    /// Build options with an explicit changed-paths Bloom setting.
    #[must_use]
    pub fn with_changed_paths(changed_paths: bool) -> Self {
        Self {
            changed_paths,
            ..Self::default()
        }
    }

    /// Build options with [`Self::dry_run`] set.
    #[must_use]
    pub fn with_dry_run(dry_run: bool) -> Self {
        Self {
            dry_run,
            ..Self::default()
        }
    }
}

/// Result of writing `objects/info/commit-graph`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CommitGraphWriteReport {
    /// Path to the monolithic commit-graph file.
    pub path: PathBuf,
    /// Commits encoded in the graph (`0` when [`Self::skipped`] is set).
    pub commit_count: usize,
    /// Whether a split `commit-graphs/` chain was removed.
    pub removed_chain: bool,
    /// Bloom generation counters when [`CommitGraphOptions::changed_paths`] was set and a
    /// graph was written.
    pub bloom_stats: Option<BloomWriteStats>,
    /// Why no graph was written, when the repository rewrites parent lists.
    pub skipped: Option<CommitGraphSkipReason>,
}

/// Why [`write_commit_graph`] wrote no graph.
///
/// A commit-graph stores each commit's parents as recorded in the commit object. When the
/// repository overrides those parents, a graph would disagree with what Git sees: Git
/// neither writes nor reads a graph in that state, and `git fsck` rejects one that exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CommitGraphSkipReason {
    /// `.git/shallow` lists boundary commits whose parents are cut off.
    Shallow,
    /// `info/grafts` overrides the parents of at least one commit.
    Grafts,
    /// Replace refs (`refs/replace/`) are present and enabled.
    ReplaceRefs,
}

impl CommitGraphSkipReason {
    /// Stable kebab-case identifier: `shallow`, `grafts` or `replace-refs`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shallow => "shallow",
            Self::Grafts => "grafts",
            Self::ReplaceRefs => "replace-refs",
        }
    }

    /// Check whether the repository at `git_dir` rewrites parent lists.
    ///
    /// `git_dir` may be a linked worktree's git directory; shallow and graft files are read
    /// from the common directory. `has_replace_refs` is only called when neither applies and
    /// reports whether enabled replace refs exist.
    pub(crate) fn detect(
        git_dir: &Path,
        has_replace_refs: impl FnOnce() -> Result<bool>,
    ) -> Result<Option<Self>> {
        let common = crate::repo::common_git_dir_for_config(git_dir);
        if !crate::shallow::load_shallow_boundaries(&common).is_empty() {
            return Ok(Some(Self::Shallow));
        }
        if !crate::rev_parse::load_graft_parents(&common).is_empty() {
            return Ok(Some(Self::Grafts));
        }
        Ok(has_replace_refs()?.then_some(Self::ReplaceRefs))
    }

    /// Like [`Self::detect`] for an open repository, honoring `core.useReplaceRefs`,
    /// `GIT_NO_REPLACE_OBJECTS` and `GIT_REPLACE_REF_BASE`.
    pub(crate) fn detect_for_repository(repo: &Repository) -> Result<Option<Self>> {
        Self::detect(repo.git_dir(), || {
            let Some(base) = repo.replace_ref_base_if_enabled() else {
                return Ok(false);
            };
            Ok(!repo.references().list(&base)?.is_empty())
        })
    }
}

/// Options for [`Repository::write_midx`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct MidxOptions {
    /// Write a reachability `.bitmap` sidecar when the MIDX is a root file.
    pub bitmap: bool,
    /// Report without writing `multi-pack-index`.
    pub dry_run: bool,
    /// On-disk MIDX version (`1` or `2`). `None` uses `midx.version` from the repository
    /// config, and v1 when that is unset, so every Git with multi-pack-index support can
    /// read the result.
    pub version: Option<u8>,
}

impl MidxOptions {
    /// Build MIDX write options with an explicit bitmap sidecar flag.
    #[must_use]
    pub fn with_bitmap(bitmap: bool) -> Self {
        Self {
            bitmap,
            ..Self::default()
        }
    }

    /// Build options with [`Self::dry_run`] set.
    #[must_use]
    pub fn with_dry_run(dry_run: bool) -> Self {
        Self {
            dry_run,
            ..Self::default()
        }
    }
}

/// Result of writing `objects/pack/multi-pack-index`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct MidxWriteReport {
    /// Path to the tip MIDX file.
    pub path: PathBuf,
    /// Pack indexes covered by the MIDX.
    pub pack_count: usize,
    /// Whether a reachability bitmap was written or skipped.
    pub bitmap: MidxBitmapWriteOutcome,
}

/// Options for [`References::pack`](crate::refs::References::pack).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct PackRefsOptions {
    /// When true, pack every packable ref (`git pack-refs --all`); otherwise only tags.
    pub all: bool,
    /// Report without rewriting `packed-refs` or deleting loose refs.
    pub dry_run: bool,
}

impl PackRefsOptions {
    /// Build pack-refs options matching `git pack-refs` / `--all`.
    #[must_use]
    pub fn with_all(all: bool) -> Self {
        Self {
            all,
            ..Self::default()
        }
    }

    /// Build options with [`Self::dry_run`] set.
    #[must_use]
    pub fn with_dry_run(dry_run: bool) -> Self {
        Self {
            dry_run,
            ..Self::default()
        }
    }
}

/// Result of packing refs (files `packed-refs` or reftable compaction).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PackRefsReport {
    /// `packed-refs` path or the `reftable/` directory.
    pub path: PathBuf,
    /// Loose refs merged into the packed store.
    pub refs_packed: usize,
    /// Loose ref files removed (files backend) or tables merged (reftable).
    pub refs_pruned: usize,
}

/// Options for [`gc()`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct GcOptions {
    /// Repack options applied before pruning unreachable objects.
    pub repack: RepackOptions,
    /// Expire reflogs using `gc.reflogExpire*` before repack/prune.
    pub expire_reflog: bool,
    /// Unix seconds for reflog expiry rules.
    pub now_unix: i64,
    /// When true, pack loose refs before other gc steps.
    pub pack_refs: bool,
    /// When true, write `objects/info/commit-graph` after repack/prune.
    ///
    /// Changed-path Bloom filters are kept when the existing graph has them and are not
    /// added otherwise, matching `git gc`.
    pub write_commit_graph: bool,
}

impl Default for GcOptions {
    fn default() -> Self {
        Self {
            repack: RepackOptions {
                auto_skip: true,
                ..RepackOptions::default()
            },
            expire_reflog: true,
            now_unix: 0,
            pack_refs: true,
            write_commit_graph: true,
        }
    }
}

impl GcOptions {
    /// Run gc with repack dry-run control and an explicit wall-clock for reflog expiry.
    #[must_use]
    pub fn with_repack_dry_run(dry_run: bool, now_unix: i64) -> Self {
        Self {
            repack: RepackOptions {
                dry_run,
                auto_skip: true,
                ..RepackOptions::default()
            },
            expire_reflog: true,
            now_unix,
            ..Self::default()
        }
    }

    /// Default repack with explicit reflog expiry policy and wall-clock.
    #[must_use]
    pub fn with_reflog_policy(now_unix: i64, expire_reflog: bool) -> Self {
        Self {
            repack: RepackOptions {
                auto_skip: true,
                ..RepackOptions::default()
            },
            expire_reflog,
            now_unix,
            ..Self::default()
        }
    }

    /// Full maintenance run: pack-refs, reflog expiry, repack, prune, commit-graph.
    #[must_use]
    pub fn full_run(dry_run: bool, now_unix: i64) -> Self {
        Self {
            repack: RepackOptions {
                dry_run,
                auto_skip: true,
                ..RepackOptions::default()
            },
            expire_reflog: true,
            now_unix,
            pack_refs: true,
            write_commit_graph: true,
            ..Self::default()
        }
    }
}

impl PruneOptions {
    /// Prune with an optional explicit cutoff (`None` uses `gc.pruneExpire`).
    #[must_use]
    pub fn with_older_than(older_than: Option<SystemTime>) -> Self {
        Self {
            older_than,
            ..Self::default()
        }
    }
}

/// Object ids that must stay reachable during repack, prune, and fsck walks.
///
/// Includes ref tips, symbolic `HEAD`, every reflog `old`/`new` oid, and every
/// blob/tree oid recorded in the index (including unmerged stages; gitlinks are excluded).
///
/// # Errors
///
/// Propagates ref, reflog, or index load failures.
pub fn collect_maintenance_object_roots(repo: &Repository) -> Result<Vec<ObjectId>> {
    repo_fsck::reachability_roots(repo)
}

pub(crate) fn bloom_settings_for_commit_graph_write(
    config: &ConfigSet,
    changed_paths: bool,
) -> BloomFilterSettings {
    let mut bloom = BloomFilterSettings::default();
    if !changed_paths {
        return bloom;
    }
    if let Some(v) = config
        .get("commitgraph.changedpathsversion")
        .and_then(|s| s.trim().parse::<i32>().ok())
    {
        if v == 2 {
            bloom.hash_version = 2;
        } else if v == 1 {
            bloom.hash_version = 1;
        }
    }
    bloom
}

/// Whether the current commit-graph carries changed-path Bloom filters.
///
/// Maintenance rewrites keep Bloom filters when the graph already has them and otherwise
/// write without them, as Git's gc does: computing filters from scratch diffs every
/// commit's tree and costs far more than the rest of the graph.
fn existing_commit_graph_has_changed_paths(repo: &Repository) -> bool {
    repo.odb()
        .files_objects_dir()
        .and_then(|dir| CommitGraphChain::try_load(dir).ok().flatten())
        .is_some_and(|chain| chain.top_layer_bloom_settings().is_some())
}

pub(crate) fn commit_graph_writes_generation_data(config: &ConfigSet) -> bool {
    config
        .get("commitgraph.generationversion")
        .and_then(|s| s.trim().parse::<i32>().ok())
        .map(|v| v >= 2)
        .unwrap_or(true)
}

/// A commit-graph file built in memory by [`build_commit_graph`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BuiltCommitGraph {
    /// The complete file, byte-for-byte what Git reads from `objects/info/commit-graph`.
    pub bytes: Vec<u8>,
    /// Commits recorded in the graph.
    pub commit_count: usize,
    /// Bloom generation counters when [`CommitGraphOptions::changed_paths`] was set.
    pub bloom_stats: Option<BloomWriteStats>,
}

impl BuiltCommitGraph {
    /// Parse [`Self::bytes`] into a chain for
    /// [`OdbBuilder::commit_graph`](crate::odb::OdbBuilder::commit_graph).
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptObject`] if the bytes fail verification (see
    /// [`CommitGraphChain::from_bytes`]).
    pub fn to_chain(&self) -> Result<CommitGraphChain> {
        CommitGraphChain::from_bytes(self.bytes.clone())
    }
}

/// Outcome of [`build_commit_graph`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitGraphBuild {
    /// The graph of every commit reachable from the repository's refs.
    Built(BuiltCommitGraph),
    /// The repository rewrites parent lists, so no graph may describe it.
    Skipped(CommitGraphSkipReason),
}

/// Build a reachable commit-graph in memory, for any object store.
///
/// Reachable commits are discovered from [`Repository::refs`](crate::repo::Repository::refs)
/// and read through [`Repository::odb`](crate::repo::Repository::odb), so this works for a
/// custom primary ([`crate::odb::OdbBuilder::primary`]) as well as the files backend. Nothing is
/// written: a custom store persists [`BuiltCommitGraph::bytes`] in its own storage and attaches
/// the graph when it next opens the repository, with
/// [`OdbBuilder::commit_graph`](crate::odb::OdbBuilder::commit_graph) and
/// [`CommitGraphChain::from_bytes`]. For a files-backed repository [`write_commit_graph`] builds
/// the same bytes and installs them under `objects/info/`.
///
/// With [`CommitGraphOptions::changed_paths`], changed-path Bloom filters are added, reusing
/// the filters of the graph the repository's walks already use (the attached graph, or the
/// files backend's on-disk graph) where their settings match.
/// [`CommitGraphOptions::dry_run`] has no effect. In a shallow repository, with
/// `info/grafts`, or with enabled replace refs, [`CommitGraphBuild::Skipped`] says why no graph
/// was built (see [`CommitGraphSkipReason`]).
///
/// # Errors
///
/// Propagates ref, object read and config errors.
pub fn build_commit_graph(
    repo: &Repository,
    opts: &CommitGraphOptions,
) -> Result<CommitGraphBuild> {
    if let Some(reason) = CommitGraphSkipReason::detect_for_repository(repo)? {
        return Ok(CommitGraphBuild::Skipped(reason));
    }
    let odb = repo.odb();
    let existing = odb.commit_graph_chain();
    let commits = collect_reachable_commit_oids_for_repository(repo)?;
    build_commit_graph_for_commits(
        odb,
        commits,
        &*repo.config()?,
        opts.changed_paths,
        existing.as_deref(),
    )
    .map(CommitGraphBuild::Built)
}

/// Encode a commit-graph of `commits`, read through `odb`.
///
/// Bloom filters and generation data follow `config` (`commitGraph.changedPathsVersion`,
/// `commitGraph.generationVersion`); with `changed_paths`, filters already in `existing` are
/// reused where their settings match.
pub(crate) fn build_commit_graph_for_commits(
    odb: &Odb,
    commits: impl IntoIterator<Item = ObjectId>,
    config: &ConfigSet,
    changed_paths: bool,
    existing: Option<&CommitGraphChain>,
) -> Result<BuiltCommitGraph> {
    let bloom = bloom_settings_for_commit_graph_write(config, changed_paths);
    let write_generation = commit_graph_writes_generation_data(config);
    let mut sorted: Vec<ObjectId> = commits.into_iter().collect();
    sorted.sort_unstable();

    let mut infos = oid_hash_map();
    for oid in &sorted {
        infos.insert(*oid, load_commit_graph_commit_info(odb, *oid)?);
    }

    let mut existing_filters = oid_hash_map();
    let mut upgraded_filters = oid_hash_map();
    if let (true, Some(chain)) = (changed_paths, existing) {
        for oid in &sorted {
            if let Some(bytes) = chain.existing_filter_bytes(oid, &bloom) {
                existing_filters.insert(*oid, bytes);
            } else if let Some(bytes) = chain.upgradable_filter_bytes(oid, &bloom) {
                upgraded_filters.insert(*oid, bytes);
            }
        }
    }

    let (bytes, bloom_stats) = build_commit_graph_bytes_from_oid_maps(
        &sorted,
        &infos,
        odb,
        changed_paths,
        &bloom,
        None,
        &[],
        None,
        &existing_filters,
        &upgraded_filters,
        write_generation,
    )?;
    Ok(BuiltCommitGraph {
        bytes,
        commit_count: sorted.len(),
        bloom_stats: changed_paths.then_some(bloom_stats),
    })
}

/// Write a monolithic reachable commit-graph under `objects/info/commit-graph`.
///
/// Reachable commits are discovered from [`Repository::refs`](crate::repo::Repository::refs)
/// (reftable, packed-refs, and linked-worktree visibility), not by scanning `git_dir/refs` alone.
///
/// In a shallow repository, with `info/grafts`, or with enabled replace refs, nothing is
/// written and [`CommitGraphWriteReport::skipped`] says why (see [`CommitGraphSkipReason`]).
/// Git only skips the write in that state, but `git fsck` still verifies a graph that is
/// already there, so an existing graph and split chain are deleted as well (except in
/// [`CommitGraphOptions::dry_run`]).
///
/// Only a files-backed repository has an `objects/info/` directory to install the graph in.
/// A custom primary builds the same graph with [`build_commit_graph`] and stores it itself.
///
/// # Errors
///
/// Returns [`Error::UnsupportedObjectStore`] for non-files ODB backends, or errors from graph
/// generation and atomic I/O.
pub fn write_commit_graph(
    repo: &Repository,
    opts: &CommitGraphOptions,
) -> Result<CommitGraphWriteReport> {
    let odb = repo.odb();
    odb.require_files_primary("commit-graph write")?;
    let objects_dir = odb
        .files_objects_dir()
        .ok_or(Error::UnsupportedObjectStore {
            operation: "commit-graph write",
        })?;
    let path = objects_dir.join("info").join("commit-graph");

    if let Some(reason) = CommitGraphSkipReason::detect_for_repository(repo)? {
        let removed_chain = if opts.dry_run {
            false
        } else {
            let removed = remove_commit_graph_files(objects_dir)?;
            repo.caches().invalidate_commit_graph_cache();
            removed
        };
        return Ok(CommitGraphWriteReport {
            path,
            commit_count: 0,
            removed_chain,
            bloom_stats: None,
            skipped: Some(reason),
        });
    }

    let commits = collect_reachable_commit_oids_for_repository(repo)?;
    if opts.dry_run {
        return Ok(CommitGraphWriteReport {
            path,
            commit_count: commits.len(),
            removed_chain: false,
            bloom_stats: opts.changed_paths.then_some(BloomWriteStats::default()),
            skipped: None,
        });
    }

    let existing = CommitGraphChain::try_load(objects_dir).ok().flatten();
    let built = build_commit_graph_for_commits(
        odb,
        commits,
        &*repo.config()?,
        opts.changed_paths,
        existing.as_ref(),
    )?;

    let written_path = atomic_replace_commit_graph(objects_dir, &built.bytes)?;
    let removed_chain = remove_commit_graph_chain_dir(objects_dir);
    repo.caches().invalidate_commit_graph_cache();

    Ok(CommitGraphWriteReport {
        path: written_path,
        commit_count: built.commit_count,
        removed_chain,
        bloom_stats: built.bloom_stats,
        skipped: None,
    })
}

/// Write or refresh `objects/pack/multi-pack-index` for the repository.
///
/// The on-disk version comes from [`MidxOptions::version`], then `midx.version`, then v1.
///
/// # Errors
///
/// Returns [`Error::UnsupportedObjectStore`] for non-files backends, [`Error::Config`] when
/// `midx.version` is not an integer, [`crate::error::MidxError::UnknownWriteVersion`] (as
/// [`Error::Midx`]) when the version is not `1` or `2`, or MIDX write failures.
pub fn write_midx(repo: &Repository, opts: &MidxOptions) -> Result<MidxWriteReport> {
    let odb = repo.odb();
    odb.require_files_primary("midx write")?;
    let requested = match opts.version {
        Some(v) => Some(v),
        None => midx_version_from_config(&*repo.config()?)?,
    };
    let version = Some(midx_write_version(requested)?);
    let objects_dir = odb
        .files_objects_dir()
        .ok_or(Error::UnsupportedObjectStore {
            operation: "midx write",
        })?;
    let pack_dir = objects_dir.join("pack");
    let pack_count = fs::read_dir(&pack_dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|x| x == "idx"))
                .filter(|e| e.path().with_extension("pack").is_file())
                .count()
        })
        .unwrap_or(0);

    let path = pack_dir.join("multi-pack-index");
    if opts.dry_run {
        return Ok(MidxWriteReport {
            path,
            pack_count,
            bitmap: MidxBitmapWriteOutcome::NotRequested,
        });
    }
    let write_opts = WriteMultiPackIndexOptions {
        write_bitmap: opts.bitmap,
        version,
        ..WriteMultiPackIndexOptions::default()
    };
    let WriteMultiPackIndexResult { bitmap } =
        write_multi_pack_index_with_options(&pack_dir, &write_opts)?;
    Ok(MidxWriteReport {
        path,
        pack_count,
        bitmap,
    })
}

/// Options for [`prune_loose_objects`].
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct PruneOptions {
    /// When set, only delete unreachable loose objects strictly older than this time.
    ///
    /// When `None`, uses `gc.pruneExpire` from repository config (default `2.weeks.ago`).
    pub older_than: Option<SystemTime>,
    /// Report without deleting objects.
    pub dry_run: bool,
}

/// Wall-clock milliseconds for each phase of [`gc`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct GcPhaseTimings {
    /// [`pack_refs`] when run as part of gc.
    pub pack_refs_ms: u64,
    /// Reflog expiry before repack.
    pub reflog_ms: u64,
    /// Repack work excluding embedded unreachable prune.
    pub repack_ms: u64,
    /// Unreachable loose-object prune (during repack or standalone).
    pub prune_ms: u64,
    /// Commit-graph write after repack.
    pub commit_graph_ms: u64,
}

/// Result of [`gc`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcStats {
    /// Loose refs packed during gc (`None` when skipped).
    pub pack_refs: Option<PackRefsReport>,
    /// Counters from the repack phase.
    pub repack: RepackStats,
    /// Counters from loose-object pruning after repack.
    pub prune: PruneStats,
    /// True when full gc ran unreachable loose-object pruning during repack.
    pub prune_ran: bool,
    /// Monolithic commit-graph written after repack/prune (`None` when skipped).
    pub commit_graph: Option<CommitGraphWriteReport>,
    /// Reflog lines removed by expiry during gc.
    pub reflog_entries_pruned: usize,
    /// Per-phase wall times for the gc orchestration.
    pub phase_timings: GcPhaseTimings,
}

/// Repack local objects into a single new pack and drop redundant storage.
///
/// Mirrors a minimal `git repack -ad`: one pack for [`reachability_roots`](crate::fsck::reachability_roots), optional `.rev` /
/// `.bitmap` sidecars, delete other packs, [`prune_packed_objects`], and optional unreachable
/// loose-object pruning. Existing promisor packs and all their sidecars are retained
/// separately, as are ordinary packs holding promised objects unavailable in those
/// promisor packs. Promised objects are excluded from the local pack; missing promised
/// targets never trigger remote reads. Bitmaps are omitted when promises prevent a
/// complete closure. Dry runs and incremental repacks use the same promise boundary.
///
/// # Errors
///
/// Propagates pack build/install failures, unsupported custom ODB backends, and I/O errors.
pub fn repack(repo: &Repository, opts: &RepackOptions) -> Result<RepackReport> {
    Ok(repack_detail(repo, opts)?.report)
}

fn repack_detail(repo: &Repository, opts: &RepackOptions) -> Result<RepackDetail> {
    let odb = repo.odb();
    odb.require_files_primary("repack")?;
    let objects_dir = odb
        .files_objects_dir()
        .ok_or(Error::UnsupportedObjectStore {
            operation: "repack",
        })?;
    let pack_dir = objects_dir.join("pack");

    let mut report = RepackReport::default();
    let loose_before = count_loose_objects(odb)?;
    let packs_before = count_pack_files(&pack_dir);
    let pack_inventory = pack_repack_inventory(&pack_dir);

    #[cfg(test)]
    gc_test_hooks::run(gc_test_hooks::GcPausePoint::AfterInventoryBeforeRoots);

    let roots = collect_maintenance_object_roots(repo)?;

    #[cfg(test)]
    gc_test_hooks::run(gc_test_hooks::GcPausePoint::AfterRootsBeforeDropAll);
    if roots.is_empty() && loose_before == 0 && packs_before == 0 {
        return Ok(RepackDetail {
            report,
            pack_path: None,
            prune_packed_loose: 0,
            unreachable_loose: PruneStats::default(),
            unreachable_prune_ms: 0,
            unreachable_prune_ran: !opts.keep_unreachable && !opts.dry_run,
        });
    }

    let cfg = opts
        .merged_config
        .clone()
        .or_else(|| repo.config().ok().map(|c| (*c).clone()));
    let mut pack_opts = opts
        .pack_build
        .unwrap_or_else(|| PackBuildOptions::for_local_copy(cfg.as_ref()));
    if opts.force_recompress {
        pack_opts.reuse_deltas = false;
    }

    let run_incremental = opts.incremental;
    let shallow = load_shallow_boundaries(repo.git_dir());
    let reachable = LazyReachable::new(odb, &roots, &shallow);

    let skip_repack = !opts.force_recompress
        && !opts.write_bitmap
        && !opts.write_commit_graph
        && should_auto_skip_repack(
            loose_before,
            packs_before,
            run_incremental,
            opts.auto_skip,
            cfg.as_ref(),
        )
        && !skipping_repack_would_retain_prunable_packed_objects(odb, &pack_dir, &reachable, opts)?;

    if skip_repack {
        // Nothing is loose, but pruning still validates the roots without walking
        // their closure. A missing local root must not become a successful no-op.
        reachable.require_roots_exist()?;
        return Ok(RepackDetail {
            report,
            pack_path: None,
            prune_packed_loose: 0,
            unreachable_loose: PruneStats::default(),
            unreachable_prune_ms: 0,
            unreachable_prune_ran: !opts.keep_unreachable && !opts.dry_run,
        });
    }
    let promised = reachable.promised()?;
    let partial_clone = !promised.is_empty()
        || cfg.as_ref().is_some_and(|config| {
            crate::promisor::repo_treats_promisor_packs(repo.git_dir(), config)
        });
    if partial_clone {
        // Validate local reachability without giving the pack enumerator a chance to
        // lazy-fetch genuinely missing local objects in a partial clone.
        reachable.get()?;
    }
    if opts.dry_run {
        report.objects = if run_incremental {
            reachable_loose_objects(odb, &reachable)?.len()
        } else {
            if !opts.keep_unreachable {
                reachable
                    .get()?
                    .iter()
                    .filter(|oid| !promised.contains(oid))
                    .count()
            } else {
                repack_object_list(odb, &roots, &shallow, true, pack_opts.use_bitmaps, promised)?
                    .send
                    .len()
            }
        };
        return Ok(RepackDetail {
            report,
            pack_path: None,
            prune_packed_loose: 0,
            unreachable_loose: PruneStats::default(),
            unreachable_prune_ms: 0,
            unreachable_prune_ran: false,
        });
    }

    let retained_packs = if run_incremental {
        HashSet::new()
    } else {
        packs_holding_available_promises(odb, promised)?
    };
    let built_pack = if run_incremental {
        let (loose, name_hashes) = named_reachable_loose_objects(odb, &reachable, &roots)?;
        write_staged_repack_pack(
            odb,
            &pack_dir,
            &loose,
            &name_hashes,
            &pack_opts,
            opts.threads,
            cfg.as_ref(),
        )?
    } else {
        build_repack_pack(
            odb,
            &pack_dir,
            &roots,
            &shallow,
            opts.keep_unreachable,
            &pack_opts,
            opts.threads,
            cfg.as_ref(),
            promised,
        )?
    };
    let pack_bytes_len = built_pack
        .staging_path
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len() as usize)
        .unwrap_or_else(|| built_pack.bytes.len());
    let mut prune_packed_loose = 0usize;
    let mut unreachable_loose = PruneStats::default();
    let mut installed_pack = None;
    let pack_trailer_len = 12 + odb.hash_algo().len();
    if pack_bytes_len > pack_trailer_len {
        clear_pack_midx_state(&pack_dir)?;

        let trusted_rows = (!built_pack.index_rows.is_empty()).then_some(built_pack.index_rows);
        let ingest_opts = IngestPackOptions {
            fix_thin: false,
            threads: opts.threads,
            merged_config: cfg.clone(),
            environment: opts.environment.clone(),
            // Locally built pack: trailer checked at write time; index rows match the stream.
            skip_post_index_verify: trusted_rows.is_some(),
            trusted_index_rows: trusted_rows,
            ..IngestPackOptions::default()
        };
        let ingested = if let Some(staging) = built_pack.staging_path.as_ref() {
            install_pack_path(staging, odb, &ingest_opts)?
        } else {
            install_pack_bytes(built_pack.bytes, odb, &ingest_opts)?
        };
        report.packs_written = 1;
        report.objects = ingested.object_ids.len();
        let pack_path = ingested.pack_path.ok_or(Error::UnsupportedObjectStore {
            operation: "repack",
        })?;

        let idx_path = pack_path.with_extension("idx");
        if cfg
            .as_ref()
            .is_none_or(|c| c.pack_write_reverse_index_default(&opts.environment))
        {
            write_pack_rev_sidecar_for_index_path(&idx_path)?;
        }

        if opts.write_bitmap && promised.is_empty() && !AlternateSources::load(odb)?.in_use() {
            if opts.incremental && packs_before > 0 {
                // Per-pack bitmaps require reachability closure within one pack; incremental
                // repack keeps older packs and adds a new layer. Write a multi-pack-index
                // bitmap covering all packs instead of a sidecar on the new pack alone.
                let version = match cfg.as_ref() {
                    Some(c) => midx_version_from_config(c)?,
                    None => None,
                };
                let midx_opts = WriteMultiPackIndexOptions {
                    write_bitmap: true,
                    version,
                    ..WriteMultiPackIndexOptions::default()
                };
                let WriteMultiPackIndexResult { bitmap: _ } =
                    write_multi_pack_index_with_options(&pack_dir, &midx_opts)?;
            } else {
                let bitmap_opts = PackBitmapWriteOptions {
                    reachability_roots: Some(roots.clone()),
                    ..PackBitmapWriteOptions::default()
                };
                let now = opts
                    .now
                    .map(offset_to_system_time)
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                match PackBitmapWriter::write(repo, &idx_path, &bitmap_opts, now) {
                    Ok(_) => {}
                    Err(PackBitmapWriteError::NotClosed { .. }) => {
                        // Pack was built from an earlier root snapshot; live tips moved on
                        // (concurrent receive-pack). Skip the bitmap this run.
                    }
                    Err(e) => return Err(Error::CorruptObject(e.to_string())),
                }
            }
        }

        let retention = if opts.keep_unreachable {
            UnreachablePackRetention::KeepAll
        } else if let Some(cutoff) = opts.prune_older_than.map(offset_to_system_time) {
            UnreachablePackRetention::KeepGraceProtected(cutoff)
        } else {
            UnreachablePackRetention::DropAll
        };
        // Incremental repack keeps every existing pack, so nothing is superseded.
        if !opts.incremental {
            odb.loosen_unreachable_from_superseded_packs(
                &pack_dir,
                &pack_path,
                &reachable,
                retention,
                &retained_packs,
                &pack_inventory,
            )?;
        }

        // Drop cached pack/index mmaps before deleting superseded pack artifacts (Windows).
        odb.invalidate_packs();

        report.packs_removed = if opts.incremental {
            0
        } else {
            remove_superseded_pack_files_retaining(
                &pack_dir,
                &pack_path,
                &retained_packs,
                &pack_inventory,
            )?
        };

        prune_packed_loose =
            prune_packed_objects(objects_dir, PrunePackedOptions::default())?.len();

        installed_pack = Some(pack_path);

        if opts.write_commit_graph {
            write_commit_graph(
                repo,
                &CommitGraphOptions::with_changed_paths(existing_commit_graph_has_changed_paths(
                    repo,
                )),
            )?;
        }
    }

    let mut unreachable_prune_ms = 0u64;
    let mut unreachable_prune_ran = false;
    if !opts.keep_unreachable {
        let keep_newer_than = opts.prune_older_than.map(offset_to_system_time);
        let prune_start = Instant::now();
        unreachable_loose = prune_loose_unreachable_with(odb, &reachable, keep_newer_than, false)?;
        unreachable_prune_ms = prune_start.elapsed().as_millis() as u64;
        unreachable_prune_ran = true;
    }

    report.loose_removed = prune_packed_loose + unreachable_loose.pruned;
    Ok(RepackDetail {
        report,
        pack_path: installed_pack,
        prune_packed_loose,
        unreachable_loose,
        unreachable_prune_ms,
        unreachable_prune_ran,
    })
}

/// Like [`repack`], with legacy [`RepackStats`] fields for the CLI.
pub fn repack_all(repo: &Repository, opts: &RepackOptions) -> Result<RepackStats> {
    let odb = repo.odb();
    let objects_dir = odb
        .files_objects_dir()
        .ok_or(Error::UnsupportedObjectStore {
            operation: "repack",
        })?;
    let pack_dir = objects_dir.join("pack");
    let mut stats = RepackStats {
        loose_before: count_loose_objects(odb)?,
        packs_before: count_pack_files(&pack_dir),
        ..RepackStats::default()
    };
    if opts.dry_run {
        let count = repack_detail(repo, opts)?.report.objects;
        stats.objects_packed = (count > 0).then_some(count);
        return Ok(stats);
    }
    let detail = repack_detail(repo, opts)?;
    let report = detail.report;
    stats.objects_packed = (report.packs_written > 0).then_some(report.objects);
    stats.removed_pack_files = report.packs_removed;
    stats.pruned_loose = detail.prune_packed_loose;
    stats.unreachable_loose_pruned = detail.unreachable_loose.pruned;
    stats.unreachable_loose_kept = detail.unreachable_loose.kept;
    stats.unreachable_prune_ms = detail.unreachable_prune_ms;
    stats.unreachable_prune_ran = detail.unreachable_prune_ran;
    stats.pack_path = detail.pack_path;
    stats.pack_bytes = stats
        .pack_path
        .as_ref()
        .and_then(|p| p.metadata().ok().map(|m| m.len()));
    Ok(stats)
}

fn offset_to_system_time(odt: OffsetDateTime) -> SystemTime {
    SystemTime::UNIX_EPOCH
        + std::time::Duration::new(odt.unix_timestamp().max(0) as u64, odt.nanosecond())
}

fn apply_gc_prune_policy_to_repack(repack: &mut RepackOptions, policy: &GcLoosePrunePolicy) {
    match policy {
        GcLoosePrunePolicy::Disabled => {
            repack.keep_unreachable = true;
        }
        GcLoosePrunePolicy::Enabled { keep_newer_than } => {
            repack.keep_unreachable = false;
            repack.prune_older_than = keep_newer_than.map(|t| {
                let dur = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
                OffsetDateTime::from_unix_timestamp(dur.as_secs() as i64)
                    .unwrap_or(OffsetDateTime::UNIX_EPOCH)
            });
        }
    }
}

fn config_bool_or_error(config: &ConfigSet, key: &str) -> Result<Option<bool>> {
    match config.get_bool(key) {
        None => Ok(None),
        Some(Ok(v)) => Ok(Some(v)),
        Some(Err(e)) => Err(Error::Config(ConfigError::Other(format!(
            "bad config value for '{key}': {e}"
        )))),
    }
}

/// Last `repack.writeBitmaps` or `pack.writeBitmaps` entry in load order (Git synonym).
fn gc_repack_writes_bitmap(repo: &Repository, config: &ConfigSet) -> Result<bool> {
    let repack_key = canonical_key("repack.writeBitmaps").ok();
    let pack_key = canonical_key("pack.writeBitmaps").ok();
    let mut last: Option<bool> = None;
    for entry in config.entries() {
        let key = entry.key.as_str();
        if repack_key.as_deref() == Some(key) || pack_key.as_deref() == Some(key) {
            let raw = entry.value.as_deref().unwrap_or("true").trim().to_owned();
            last = Some(parse_bool(&raw).map_err(|_| {
                Error::Config(ConfigError::Other(format!(
                    "bad config value '{raw}' for '{key}'"
                )))
            })?);
        }
    }
    Ok(last.unwrap_or_else(|| repo.work_tree().is_none()))
}

fn gc_should_pack_refs(repo: &Repository, config: &ConfigSet) -> Result<bool> {
    let Some(spec) = config.get("gc.packRefs") else {
        return Ok(true);
    };
    if spec.eq_ignore_ascii_case("notbare") {
        return Ok(repo.work_tree().is_some());
    }
    parse_bool(&spec).map_err(|e| {
        Error::Config(ConfigError::Other(format!(
            "bad config value '{spec}' for 'gc.packRefs': {e}"
        )))
    })
}

fn gc_should_write_commit_graph(config: &ConfigSet) -> Result<bool> {
    if config_bool_or_error(config, "gc.writeCommitGraph")? == Some(false) {
        return Ok(false);
    }
    Ok(config_bool_or_error(config, "core.commitGraph")? != Some(false))
}

/// Loose objects reachable from the maintenance roots: what Git's incremental `repack -d`
/// packs. Unreachable loose objects stay loose so `gc.pruneExpire` can expire them.
fn reachable_loose_objects(odb: &Odb, reachable: &LazyReachable<'_>) -> Result<Vec<ObjectId>> {
    let loose = odb.enumerate_local_loose_objects()?;
    if loose.is_empty() {
        return Ok(Vec::new());
    }
    let promised = reachable.promised()?;
    let reachable = reachable.get()?;
    let alternates = AlternateSources::load(odb)?;
    Ok(loose
        .into_iter()
        .map(|(oid, _)| oid)
        .filter(|oid| {
            reachable.contains(oid) && !promised.contains(oid) && !alternates.contains(oid)
        })
        .collect())
}

/// [`reachable_loose_objects`] in pack order, with the path name hash of each tree and
/// blob found under a loose commit so versions of one file meet in the delta window.
///
/// Only loose objects are read: the walk follows loose commits from `roots` (newest
/// first) and descends into loose trees, stopping wherever history or a subtree is
/// already packed. Objects it does not reach keep their place after the walked ones
/// without a name hash.
fn named_reachable_loose_objects(
    odb: &Odb,
    reachable: &LazyReachable<'_>,
    roots: &[ObjectId],
) -> Result<(Vec<ObjectId>, HashMap<ObjectId, u32>)> {
    let loose = reachable_loose_objects(odb, reachable)?;
    let mut pending: OidHashSet = loose.iter().copied().collect();
    let mut order = Vec::with_capacity(loose.len());
    let mut names = HashMap::new();
    let mut commits = Vec::new();
    let mut root_trees = Vec::new();
    let mut stack: Vec<ObjectId> = roots
        .iter()
        .filter(|oid| pending.contains(*oid))
        .copied()
        .collect();
    let mut seen = OidHashSet::default();
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        match odb.read_info(&oid)?.kind {
            ObjectKind::Commit => {
                let obj = odb.read(&oid)?;
                let commit = CommitRef::parse(&obj.data)?;
                commits.push((
                    commit.committer_unix_seconds_for_ordering(),
                    oid,
                    commit.tree(),
                ));
                stack.extend(commit.parents().iter().filter(|p| pending.contains(*p)));
            }
            ObjectKind::Tag => {
                let obj = odb.read(&oid)?;
                pending.remove(&oid);
                order.push(oid);
                stack.extend(tag_object_line_oid(&obj.data).filter(|t| pending.contains(t)));
            }
            ObjectKind::Tree => root_trees.push(oid),
            ObjectKind::Blob => {}
        }
    }
    commits.sort_by_key(|&(time, _, _)| std::cmp::Reverse(time));
    for &(_, oid, _) in &commits {
        pending.remove(&oid);
        order.push(oid);
    }
    let hash_len = odb.hash_algo().len();
    let mut trees: Vec<(ObjectId, Option<u32>)> = Vec::new();
    for tree in commits.iter().map(|&(_, _, tree)| tree).chain(root_trees) {
        trees.push((tree, None));
        while let Some((tree, name_hash)) = trees.pop() {
            if !pending.remove(&tree) {
                continue;
            }
            order.push(tree);
            if let Some(hash) = name_hash {
                names.insert(tree, hash);
            }
            let obj = odb.read(&tree)?;
            for entry in TreeRefIter::with_oid_len(&obj.data, hash_len) {
                let entry = entry?;
                if FileMode::new(entry.mode).is_gitlink() || !pending.contains(&entry.oid) {
                    continue;
                }
                let hash = pack_name_hash_child(name_hash, entry.name);
                if entry.mode & S_IFMT == S_IFDIR {
                    trees.push((entry.oid, Some(hash)));
                } else {
                    pending.remove(&entry.oid);
                    order.push(entry.oid);
                    names.insert(entry.oid, hash);
                }
            }
        }
    }
    order.extend(loose.into_iter().filter(|oid| pending.contains(oid)));
    Ok((order, names))
}

/// Whether maintenance should skip rebuilding packs when [`RepackOptions::auto_skip`] is set.
///
/// Used by [`gc`]: skip when a single pack already holds all objects and nothing is loose.
/// Multiple packs never skip (consolidation required), even when `gc.autoPackLimit` is zero.
fn should_auto_skip_repack(
    loose_objects: usize,
    pack_count: usize,
    incremental: bool,
    auto_skip: bool,
    _config: Option<&ConfigSet>,
) -> bool {
    if !auto_skip || incremental {
        return false;
    }
    if pack_count > 1 {
        return false;
    }
    if pack_count == 1 {
        return loose_objects == 0;
    }
    loose_objects == 0
}

/// True when skipping repack would leave unreachable objects in pack storage that repack would
/// remove under `opts` (for example expired unreachable commits when `gc.pruneExpire=now`).
///
/// The reachability walk only runs once some pack is old enough to have objects dropped.
fn skipping_repack_would_retain_prunable_packed_objects(
    odb: &Odb,
    pack_dir: &Path,
    reachable: &LazyReachable<'_>,
    opts: &RepackOptions,
) -> Result<bool> {
    if opts.keep_unreachable {
        return Ok(false);
    }
    let retention = if let Some(cutoff) = opts.prune_older_than.map(offset_to_system_time) {
        UnreachablePackRetention::KeepGraceProtected(cutoff)
    } else {
        UnreachablePackRetention::DropAll
    };
    let indexes = odb.local_pack_indexes()?;
    for idx in indexes {
        if idx.is_promisor || pack_has_keep_marker(pack_dir, &idx.pack_path) {
            continue;
        }
        let pack_mtime = fs::metadata(&idx.pack_path)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let drop_unreachable_from_pack = match retention {
            UnreachablePackRetention::KeepAll => false,
            UnreachablePackRetention::DropAll => true,
            UnreachablePackRetention::KeepGraceProtected(cutoff) => pack_mtime < cutoff,
        };
        if !drop_unreachable_from_pack {
            continue;
        }
        let reachable = reachable.get()?;
        for entry in idx.iter() {
            let Ok(oid) = ObjectId::from_bytes(entry.oid()) else {
                continue;
            };
            if !reachable.contains(&oid) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Delete unreachable loose objects using `gc.pruneExpire` or an explicit cutoff.
///
/// # Errors
///
/// Propagates config parse failures, unsupported ODB backends, and I/O errors.
pub fn prune_loose_objects(repo: &Repository, opts: &PruneOptions) -> Result<PruneStats> {
    let odb = repo.odb();
    odb.require_files_primary("prune")?;
    let cfg = repo.config()?;
    let now = gc_now_unix(0);
    let keep_newer_than = match opts.older_than {
        Some(t) => Some(t),
        None => match gc_loose_prune_policy(&cfg, now)? {
            GcLoosePrunePolicy::Disabled => return Ok(PruneStats::default()),
            GcLoosePrunePolicy::Enabled { keep_newer_than } => keep_newer_than,
        },
    };
    let roots = collect_maintenance_object_roots(repo)?;
    let shallow = load_shallow_boundaries(repo.git_dir());
    prune_loose_unreachable_with(
        odb,
        &LazyReachable::new(odb, &roots, &shallow),
        keep_newer_than,
        opts.dry_run,
    )
}

fn duration_ms(d: std::time::Duration) -> u64 {
    d.as_millis() as u64
}

/// Run pack-refs, reflog expiry, repack, prune, and commit-graph maintenance.
///
/// Unless [`RepackOptions::dry_run`] is set, holds the repository's `gc.pid` lock for the
/// whole run, so it never overlaps another grit or Git gc (see [`GcLock`]).
///
/// # Errors
///
/// [`Error::MaintenanceRunning`] when another process holds the `gc.pid` lock, plus failures
/// from each maintenance step.
pub fn gc(repo: &Repository, opts: &GcOptions) -> Result<GcStats> {
    repo.odb().require_files_primary("gc")?;
    let now = gc_now_unix(opts.now_unix);
    let _lock = if opts.repack.dry_run {
        None
    } else {
        match GcLock::try_acquire(repo, now)? {
            GcLockAttempt::Acquired(lock) => Some(lock),
            GcLockAttempt::Held(holder) => return Err(Error::MaintenanceRunning { holder }),
        }
    };
    gc_locked(repo, opts, now)
}

/// Why automatic maintenance is due, as decided by [`auto_gc_needed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoGcReason {
    /// The loose-object estimate is above `gc.auto`. [`gc_auto`] packs the reachable loose
    /// objects into one new pack and leaves existing packs alone.
    TooManyLoose {
        /// Loose objects in the sampled fan-out directory (`objects/17`) times 256.
        estimate: usize,
        /// Effective `gc.auto` threshold.
        limit: usize,
    },
    /// There are more non-`.keep` packs than `gc.autoPackLimit`. [`gc_auto`] consolidates
    /// every pack into one.
    TooManyPacks {
        /// Local packs without a `.keep` marker.
        count: usize,
        /// Effective `gc.autoPackLimit`.
        limit: usize,
    },
}

/// Fan-out directory sampled to estimate the loose-object count, as Git documents for `gc.auto`.
const AUTO_GC_SAMPLE_FANOUT: &str = "17";

/// Decide whether automatic maintenance is due, using Git's `gc --auto` heuristics.
///
/// Pack consolidation wins when the number of local packs without a `.keep` marker is above
/// [`ConfigSet::gc_auto_pack_limit`]. Otherwise the loose-object count is estimated from a
/// single fan-out directory (`objects/17`) and compared with [`ConfigSet::gc_auto_threshold`],
/// so the check costs two directory reads however large the repository is. `gc.auto <= 0`
/// turns both checks off. Repositories without a files object store never need it.
///
/// This does not look at `maintenance.auto`; callers that start maintenance after a command
/// check [`ConfigSet::maintenance_auto`] themselves.
///
/// # Errors
///
/// Config load failures and I/O errors other than a missing directory.
pub fn auto_gc_needed(repo: &Repository) -> Result<Option<AutoGcReason>> {
    let cfg = repo.config()?;
    let Some(threshold) = cfg.gc_auto_threshold() else {
        return Ok(None);
    };
    let Some(objects_dir) = repo.odb().files_objects_dir() else {
        return Ok(None);
    };
    if let Some(limit) = cfg.gc_auto_pack_limit() {
        let count = count_pack_files(&objects_dir.join("pack"));
        if count > limit {
            return Ok(Some(AutoGcReason::TooManyPacks { count, limit }));
        }
    }
    let name_len = repo.odb().hash_algo().hex_len() - 2;
    let sampled = count_loose_in_fanout(&objects_dir.join(AUTO_GC_SAMPLE_FANOUT), name_len)?;
    if sampled > threshold.div_ceil(256) {
        return Ok(Some(AutoGcReason::TooManyLoose {
            estimate: sampled.saturating_mul(256),
            limit: threshold,
        }));
    }
    Ok(None)
}

/// Count entries of one loose-object fan-out directory that look like object files.
fn count_loose_in_fanout(dir: &Path, name_len: usize) -> Result<usize> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(Error::Io(e)),
    };
    Ok(entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name = name.as_encoded_bytes();
            name.len() == name_len && name.iter().all(u8::is_ascii_hexdigit)
        })
        .count())
}

/// Result of [`gc_auto`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AutoGcOutcome {
    /// Neither threshold is exceeded (or `gc.auto` is off); nothing ran.
    NotNeeded,
    /// Another process holds the `gc.pid` lock; nothing ran.
    AlreadyRunning(GcLockHolder),
    /// The `pre-auto-gc` hook exited non-zero; nothing ran.
    DeclinedByHook {
        /// Exit status of the hook.
        status: i32,
        /// What the hook wrote, when hook output is captured ([`Repository::hook_stdio`]).
        output: crate::hooks::HookOutput,
    },
    /// Maintenance ran.
    Ran {
        /// Which threshold triggered the run.
        reason: AutoGcReason,
        /// What each step did.
        stats: Box<GcStats>,
    },
}

/// Run maintenance only when Git's `gc --auto` heuristics say it is due ([`auto_gc_needed`]).
///
/// When too many loose objects triggered the run, the reachable ones are packed into one new
/// pack without rewriting existing packs and without a bitmap, so the cost scales with the
/// loose objects rather than the repository. Unreachable loose objects stay loose until
/// `gc.pruneExpire` lets the prune step delete them, as in Git. Too many packs runs a full consolidating [`gc`]. Either way the
/// other `opts` steps (pack-refs, reflog expiry, unreachable prune under `gc.pruneExpire`,
/// commit-graph) run as in [`gc`].
///
/// Before doing any work this takes the `gc.pid` lock ([`GcLock`]) and runs the `pre-auto-gc`
/// hook; a held lock or a failing hook ends the call without changes. With
/// [`RepackOptions::dry_run`] neither happens and the returned stats describe the planned work.
///
/// The library never forks: callers that honor [`ConfigSet::gc_auto_detach`] start a separate
/// process that calls this function.
///
/// # Errors
///
/// Config, hook spawn, lock and I/O failures, and any error from the maintenance steps.
pub fn gc_auto(repo: &Repository, opts: &GcOptions) -> Result<AutoGcOutcome> {
    let Some(reason) = auto_gc_needed(repo)? else {
        return Ok(AutoGcOutcome::NotNeeded);
    };
    let now = gc_now_unix(opts.now_unix);
    let dry_run = opts.repack.dry_run;
    let _lock = if dry_run {
        None
    } else {
        match GcLock::try_acquire(repo, now)? {
            GcLockAttempt::Acquired(lock) => Some(lock),
            GcLockAttempt::Held(holder) => return Ok(AutoGcOutcome::AlreadyRunning(holder)),
        }
    };
    if !dry_run {
        if let crate::hooks::HookResult::Failed { status, output } =
            crate::hooks::run_hook(repo, "pre-auto-gc", &[], None)?
        {
            return Ok(AutoGcOutcome::DeclinedByHook { status, output });
        }
    }
    let mut run_opts = opts.clone();
    if matches!(reason, AutoGcReason::TooManyLoose { .. }) {
        run_opts.repack.incremental = true;
        run_opts.repack.write_bitmap = false;
    }
    let stats = gc_locked(repo, &run_opts, now)?;
    Ok(AutoGcOutcome::Ran {
        reason,
        stats: Box::new(stats),
    })
}

/// Process recorded in `gc.pid` by a maintenance run that is still in progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcLockHolder {
    /// Process id from `gc.pid`, when it could be read.
    pub pid: Option<u64>,
    /// Host name from `gc.pid`, when it could be read.
    pub host: Option<String>,
}

/// Outcome of [`GcLock::try_acquire`].
#[derive(Debug)]
#[must_use]
pub enum GcLockAttempt {
    /// This process now owns `gc.pid`.
    Acquired(GcLock),
    /// Another live maintenance run owns it.
    Held(GcLockHolder),
}

/// How long a `gc.pid` written by another run is honored before it is treated as stale.
const GC_PID_STALE_AFTER: time::Duration = time::Duration::hours(12);

/// Repository-wide maintenance lock, compatible with Git's `gc.pid`.
///
/// The lock file holds `"<pid> <hostname>"` and is removed when this value is dropped. An
/// existing `gc.pid` counts as held while it is younger than 12 hours and either names another
/// host or a process on this host that is still running; older or dead entries are taken over.
#[derive(Debug)]
pub struct GcLock {
    path: PathBuf,
}

impl GcLock {
    /// Try to take the `gc.pid` lock in the repository's common directory without waiting.
    ///
    /// `now_unix` is the wall clock used to decide whether an existing `gc.pid` is stale.
    ///
    /// # Errors
    ///
    /// I/O failures while creating, writing or renaming the lock file.
    pub fn try_acquire(repo: &Repository, now_unix: i64) -> Result<GcLockAttempt> {
        let path = repo.common_git_dir().join("gc.pid");
        let mut lock = match crate::lockfile::LockFile::acquire(
            &path.with_extension("pid.lock"),
            crate::lockfile::LockWait::Immediately,
        ) {
            Ok(lock) => lock,
            Err(crate::lockfile::LockError::Held { .. }) => {
                return Ok(GcLockAttempt::Held(read_gc_pid(&path).unwrap_or(
                    GcLockHolder {
                        pid: None,
                        host: None,
                    },
                )));
            }
            Err(crate::lockfile::LockError::Io { source, .. }) => return Err(Error::Io(source)),
        };
        let host = local_host_name();
        if let Some(holder) = live_gc_pid_holder(&path, &host, now_unix) {
            lock.rollback();
            return Ok(GcLockAttempt::Held(holder));
        }
        use std::io::Write as _;
        write!(lock, "{} {host}", std::process::id())?;
        lock.commit_to(&path).map_err(|e| match e {
            crate::lockfile::LockError::Io { source, .. } => Error::Io(source),
            held @ crate::lockfile::LockError::Held { .. } => Error::Io(io::Error::other(held)),
        })?;
        Ok(GcLockAttempt::Acquired(GcLock { path }))
    }

    /// The maintenance run currently holding `gc.pid`, without taking the lock.
    ///
    /// Uses the same staleness rules as [`Self::try_acquire`], so `None` means an attempt would
    /// most likely succeed. Lets a caller avoid starting a process that would only find the
    /// lock held.
    #[must_use]
    pub fn current_holder(repo: &Repository, now_unix: i64) -> Option<GcLockHolder> {
        let path = repo.common_git_dir().join("gc.pid");
        live_gc_pid_holder(&path, &local_host_name(), now_unix)
    }
}

impl Drop for GcLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn read_gc_pid(path: &Path) -> Option<GcLockHolder> {
    let raw = fs::read_to_string(path).ok()?;
    let mut fields = raw.split_whitespace();
    let pid = fields.next().and_then(|p| p.parse::<u64>().ok());
    let host = fields.next().map(str::to_owned);
    Some(GcLockHolder { pid, host })
}

/// The holder recorded in `gc.pid` when it still blocks a new run on `host`.
fn live_gc_pid_holder(path: &Path, host: &str, now_unix: i64) -> Option<GcLockHolder> {
    let mtime = fs::metadata(path).and_then(|m| m.modified()).ok()?;
    let mtime = OffsetDateTime::from(mtime);
    let now = OffsetDateTime::from_unix_timestamp(now_unix).ok()?;
    if now - mtime > GC_PID_STALE_AFTER {
        return None;
    }
    let holder = read_gc_pid(path)?;
    let pid = holder.pid?;
    let same_host = holder.host.as_deref() == Some(host);
    if same_host && !crate::lockfile::is_process_running(pid) {
        return None;
    }
    Some(holder)
}

/// Host name as Git records it in `gc.pid` (`gethostname`), so either tool can tell whether
/// the other's lock belongs to this machine.
fn local_host_name() -> String {
    #[cfg(unix)]
    {
        if let Ok(uts) = nix::sys::utsname::uname() {
            if let Some(name) = uts.nodename().to_str() {
                return name.to_owned();
            }
        }
    }
    #[cfg(windows)]
    {
        if let Some(name) = windows_dns_host_name() {
            return name;
        }
    }
    "localhost".to_owned()
}

/// The DNS host name, which is what Winsock `gethostname` (used by Git for Windows) returns.
#[cfg(windows)]
fn windows_dns_host_name() -> Option<String> {
    use windows_sys::Win32::System::SystemInformation::{
        ComputerNameDnsHostname, GetComputerNameExW,
    };
    let mut len: u32 = 0;
    // SAFETY: a null buffer with zero length only asks for the required size in `len`.
    unsafe { GetComputerNameExW(ComputerNameDnsHostname, std::ptr::null_mut(), &mut len) };
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u16; len as usize];
    // SAFETY: `buf` has room for the `len` UTF-16 units the previous call asked for.
    let ok = unsafe { GetComputerNameExW(ComputerNameDnsHostname, buf.as_mut_ptr(), &mut len) };
    (ok != 0).then(|| String::from_utf16_lossy(&buf[..len as usize]))
}

fn gc_locked(repo: &Repository, opts: &GcOptions, now: i64) -> Result<GcStats> {
    let mut stats = GcStats::default();
    let cfg = repo.config()?;
    let prune_policy = gc_loose_prune_policy(&cfg, now)?;
    let dry_run = opts.repack.dry_run;

    if opts.pack_refs && gc_should_pack_refs(repo, &cfg)? && !dry_run {
        let start = Instant::now();
        stats.pack_refs = Some(pack_refs(repo, &PackRefsOptions::with_all(true))?);
        stats.phase_timings.pack_refs_ms = duration_ms(start.elapsed());
    }

    if opts.expire_reflog {
        let start = Instant::now();
        stats.reflog_entries_pruned = expire_reflogs_for_gc(repo, dry_run, now)?;
        stats.phase_timings.reflog_ms = duration_ms(start.elapsed());
    }

    let mut repack_opts = opts.repack.clone();
    apply_gc_prune_policy_to_repack(&mut repack_opts, &prune_policy);
    if repack_opts.now.is_none() {
        repack_opts.now =
            Some(OffsetDateTime::from_unix_timestamp(now).unwrap_or(OffsetDateTime::UNIX_EPOCH));
    }
    // Incremental runs skip bitmaps, as Git's `gc --auto` does: a bitmap covers the whole
    // object graph and costs far more than packing the loose objects.
    if !repack_opts.write_bitmap && !repack_opts.incremental {
        repack_opts.write_bitmap = gc_repack_writes_bitmap(repo, &cfg)?;
    }
    let repack_start = Instant::now();
    stats.repack = repack_all(repo, &repack_opts)?;
    let repack_total_ms = duration_ms(repack_start.elapsed());
    stats.phase_timings.prune_ms = stats.repack.unreachable_prune_ms;
    stats.phase_timings.repack_ms = repack_total_ms.saturating_sub(stats.phase_timings.prune_ms);

    if dry_run {
        return Ok(stats);
    }

    // Repack prunes unreachable loose objects itself (sharing its reachability walk), so gc
    // only reports the outcome.
    stats.prune_ran = stats.repack.unreachable_prune_ran;
    stats.prune = PruneStats {
        pruned: stats.repack.unreachable_loose_pruned,
        kept: stats.repack.unreachable_loose_kept,
    };

    if opts.write_commit_graph && gc_should_write_commit_graph(&cfg)? {
        let start = Instant::now();
        stats.commit_graph = Some(write_commit_graph(
            repo,
            &CommitGraphOptions::with_changed_paths(existing_commit_graph_has_changed_paths(repo)),
        )?);
        stats.phase_timings.commit_graph_ms = duration_ms(start.elapsed());
    }
    Ok(stats)
}

/// How [`gc`] applies loose-object pruning for `gc.pruneExpire`.
enum GcLoosePrunePolicy {
    /// `gc.pruneExpire=never` — do not delete unreachable loose objects.
    Disabled,
    /// Prune unreachable loose objects not protected by the grace cutoff.
    Enabled {
        /// When `Some(t)`, only delete loose objects strictly older than `t` (Git grace window).
        keep_newer_than: Option<SystemTime>,
    },
}

fn gc_loose_prune_policy(config: &ConfigSet, now_unix: i64) -> Result<GcLoosePrunePolicy> {
    let spec = config
        .get("gc.pruneExpire")
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "2.weeks.ago".to_owned());
    if spec.eq_ignore_ascii_case("never") || spec.eq_ignore_ascii_case("false") {
        return Ok(GcLoosePrunePolicy::Disabled);
    }
    // Git expires everything for `now` and `all`, including objects written this second.
    if spec.eq_ignore_ascii_case("now") || spec.eq_ignore_ascii_case("all") {
        return Ok(GcLoosePrunePolicy::Enabled {
            keep_newer_than: None,
        });
    }
    let mut err = 0;
    let cutoff_sec = approxidate_careful_at(&spec, now_unix, Some(&mut err)) as i64;
    if err != 0 {
        return Err(Error::Config(ConfigError::Other(format!(
            "bad config value '{spec}' for 'gc.pruneExpire'"
        ))));
    }
    let keep_newer_than = SystemTime::UNIX_EPOCH
        .checked_add(std::time::Duration::from_secs(cutoff_sec.max(0) as u64));
    Ok(GcLoosePrunePolicy::Enabled { keep_newer_than })
}

fn gc_now_unix(now_unix: i64) -> i64 {
    if now_unix != 0 {
        return now_unix;
    }
    time::OffsetDateTime::now_utc().unix_timestamp()
}

fn expire_reflogs_for_gc(repo: &Repository, dry_run: bool, now: i64) -> Result<usize> {
    let cfg = repo.config()?;
    let gc_cfg = load_gc_reflog_expire_config(&cfg, now);
    let params = ReflogExpireParams {
        stale_fix: false,
        dry_run,
        verbose: false,
    };
    let mut total = 0usize;
    let cache = ReflogExpiryCache::default();
    for refname in reflog_refs_on_store(repo.refs())? {
        let result = expire_reflog_git_with_cache(
            repo,
            &refname,
            &params,
            None,
            None,
            &gc_cfg.patterns,
            gc_cfg.global_total,
            gc_cfg.global_unreachable,
            now,
            &cache,
        )?;
        total += result.pruned;
    }
    Ok(total)
}

fn count_loose_objects(odb: &Odb) -> Result<usize> {
    Ok(odb.enumerate_local_loose_objects()?.len())
}

/// Build the all-into-one repack pack for `roots`, stopping history at the `shallow`
/// boundary commits.
#[allow(clippy::too_many_arguments)]
fn build_repack_pack(
    odb: &Odb,
    pack_dir: &Path,
    roots: &[ObjectId],
    shallow: &HashSet<ObjectId>,
    keep_unreachable: bool,
    opts: &PackBuildOptions,
    threads: Option<usize>,
    cfg: Option<&ConfigSet>,
    promised: &HashSet<ObjectId>,
) -> Result<SerializedDeltaPack> {
    let selection = repack_object_list(
        odb,
        roots,
        shallow,
        keep_unreachable,
        opts.use_bitmaps,
        promised,
    )?;
    write_staged_repack_pack(
        odb,
        pack_dir,
        &selection.send,
        &selection.name_hashes,
        opts,
        threads,
        cfg,
    )
}

/// Stream a local pack holding exactly `objects` to a `tmp_repack_*` file in `pack_dir`,
/// with the delta search tuned by `opts` (`pack.window`, `pack.depth`) and
/// `pack.windowMemory` from `cfg`.
///
/// Every delta base is one of `objects`, so the pack is self-contained. `name_hashes`
/// pairs same-path blobs and trees in the search window. An empty `objects` writes
/// nothing and returns an empty pack.
///
/// # Errors
///
/// Propagates thread-option, object read, and staging-file I/O failures; the staging
/// file is removed when writing fails.
pub(crate) fn write_staged_repack_pack(
    odb: &Odb,
    pack_dir: &Path,
    objects: &[ObjectId],
    name_hashes: &HashMap<ObjectId, u32>,
    opts: &PackBuildOptions,
    threads: Option<usize>,
    cfg: Option<&ConfigSet>,
) -> Result<SerializedDeltaPack> {
    if objects.is_empty() {
        return Ok(SerializedDeltaPack {
            bytes: Vec::new(),
            index_rows: Vec::new(),
            staging_path: None,
        });
    }
    odb.clear_delta_base_cache();
    let max_depth = if opts.delta { opts.max_depth } else { 0 };
    let settings = RepackPackSettings {
        window: opts.window,
        max_depth,
        reuse_deltas: opts.reuse_deltas,
        reuse_objects: opts.reuse_objects,
        use_ofs_delta: opts.use_ofs_delta,
        window_memory: cfg.map_or(0, ConfigSet::pack_window_memory_bytes),
        search_threads: pack_delta_search_parallel_options(odb, threads)?
            .threads
            .get(),
        write_threads: pack_write_parallel_options(odb, threads)?.threads.get(),
    };
    let git_dir = odb
        .config_git_dir()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| {
            pack_dir
                .parent()
                .and_then(|objects| objects.parent())
                .map(Path::to_path_buf)
                .unwrap_or_else(|| pack_dir.to_path_buf())
        });
    crate::git_dir::io_error::io_with_git_path(
        &git_dir,
        pack_dir,
        crate::git_dir::GitDirPathOperation::GcPack,
        fs::create_dir_all(pack_dir),
    )?;
    let staging = pack_dir.join(format!("tmp_repack_{}", std::process::id()));
    let written = write_repack_pack(odb, objects, name_hashes, &settings, &staging);
    match written {
        Ok(index_rows) => Ok(SerializedDeltaPack {
            bytes: Vec::new(),
            index_rows,
            staging_path: Some(staging),
        }),
        Err(err) => {
            let _ = fs::remove_file(&staging);
            Err(err)
        }
    }
}

/// Objects a full repack writes: everything reachable from `roots` down to the
/// `shallow` boundary commits, plus every other packed and loose object when
/// `keep_unreachable` is set.
fn repack_object_list(
    odb: &Odb,
    roots: &[ObjectId],
    shallow: &HashSet<ObjectId>,
    keep_unreachable: bool,
    use_bitmaps: bool,
    promised: &HashSet<ObjectId>,
) -> Result<PackObjectSelection> {
    // Roots arrive in hash-set order; sorting them makes the pack (and its name)
    // the same on every run.
    let mut roots: Vec<_> = roots
        .iter()
        .copied()
        .filter(|oid| !promised.contains(oid))
        .collect();
    roots.sort_unstable();
    let alternates = AlternateSources::load(odb)?;
    let use_bitmaps = use_bitmaps && promised.is_empty() && !alternates.in_use();
    let mut selection = enumerate_pack_objects_naming(
        odb,
        &roots,
        &[],
        &PackEnumerateOptions {
            filter: None,
            shallow_grafts: shallow,
            have_shallow_grafts: Some(shallow),
            use_bitmaps,
            exclude_objects: Some(promised),
            peer_odb: None,
            have_boundary: crate::pack_object_select::HaveBoundary::CommitClosure,
        },
        NameTrees::Yes,
        true,
        &crate::progress::CancelSignal::never(),
    )?;
    if keep_unreachable {
        append_repack_unreachable_retention(odb, &mut selection.send, promised)?;
    }
    Ok(selection)
}

/// Append packed and loose objects not already selected for the repack send list.
fn append_repack_unreachable_retention(
    odb: &Odb,
    send: &mut Vec<ObjectId>,
    promised: &HashSet<ObjectId>,
) -> Result<()> {
    let alternates = AlternateSources::load(odb)?;
    let mut in_send: OidHashSet = send.iter().copied().collect();
    odb.primary()?
        .packed_objects()
        .for_each_removable_pack_object(&mut |oid| {
            if !promised.contains(oid) && !alternates.contains(oid) && in_send.insert(*oid) {
                send.push(*oid);
            }
            std::ops::ControlFlow::Continue(())
        })?;
    for (oid, _) in odb.enumerate_local_loose_objects()? {
        if !promised.contains(&oid) && !alternates.contains(&oid) && in_send.insert(oid) {
            send.push(oid);
        }
    }
    Ok(())
}

fn pack_repack_inventory(pack_dir: &Path) -> crate::odb::RepackPackInventorySnapshot {
    let pack_paths = local_pack_paths(pack_dir);
    let kept_at_inventory = pack_paths
        .iter()
        .filter(|pack_path| pack_has_keep_marker(pack_dir, pack_path))
        .cloned()
        .collect();
    crate::odb::RepackPackInventorySnapshot {
        pack_paths,
        kept_at_inventory,
    }
}

/// Every local `.pack` path under `pack_dir` at inventory time.
fn local_pack_paths(pack_dir: &Path) -> HashSet<PathBuf> {
    let Ok(entries) = fs::read_dir(pack_dir) else {
        return HashSet::new();
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| pack_dir.join(e.file_name()))
        .filter(|p| p.extension().is_some_and(|e| e == "pack"))
        .collect()
}

/// Packs in `pack_dir` without a `.keep` marker, from a single directory read.
fn count_pack_files(pack_dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(pack_dir) else {
        return 0;
    };
    let mut packs = HashSet::new();
    let mut kept = HashSet::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = PathBuf::from(entry.file_name());
        let (Some(stem), Some(ext)) = (path.file_stem(), path.extension()) else {
            continue;
        };
        if ext == "pack" {
            packs.insert(stem.to_owned());
        } else if ext == "keep" {
            kept.insert(stem.to_owned());
        }
    }
    packs.difference(&kept).count()
}

/// Promises may be stored outside marked packs. Keep their source ordinary packs
/// when no retained promisor pack supplies a copy, so repacking cannot turn an
/// available promise into a missing one. Only indexes are read; no object fetches.
fn packs_holding_available_promises(
    odb: &Odb,
    promised: &HashSet<ObjectId>,
) -> Result<HashSet<PathBuf>> {
    if promised.is_empty() {
        return Ok(HashSet::new());
    }
    let indexes = odb.local_pack_indexes()?;
    let promisor_indexes: Vec<_> = indexes.iter().filter(|idx| idx.is_promisor).collect();
    let mut retained = HashSet::new();
    for idx in indexes.iter().filter(|idx| !idx.is_promisor) {
        for entry in idx.iter() {
            let oid = ObjectId::from_bytes(entry.oid())?;
            if promised.contains(&oid)
                && !promisor_indexes
                    .iter()
                    .any(|idx| idx.find_position(&oid).is_some())
            {
                retained.insert(idx.pack_path.clone());
                break;
            }
        }
    }
    Ok(retained)
}

/// Remove pack artifacts in `pack_dir` except `keep_pack`, `.keep` packs, promisor packs, and
/// `retained_packs` (including every sidecar of those retained packs).
fn remove_superseded_pack_files_retaining(
    pack_dir: &Path,
    keep_pack: &Path,
    retained_packs: &HashSet<PathBuf>,
    inventory: &crate::odb::RepackPackInventorySnapshot,
) -> Result<usize> {
    let git_dir = crate::git_dir::io_error::git_dir_from_pack_dir(pack_dir);
    let keep_stem = keep_pack
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| Error::Io(io::Error::new(io::ErrorKind::InvalidInput, "bad pack path")))?;
    if !pack_dir.is_dir() {
        return Ok(0);
    }
    let mut removed = 0usize;
    for entry in crate::git_dir::io_error::io_with_git_path(
        &git_dir,
        pack_dir,
        crate::git_dir::GitDirPathOperation::GcPack,
        fs::read_dir(pack_dir),
    )? {
        let entry = entry.map_err(|source| {
            crate::git_dir::io_error::map_git_dir_io(
                &git_dir,
                pack_dir,
                crate::git_dir::GitDirPathOperation::GcPack,
                source,
            )
        })?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str == keep_stem || name_str.starts_with(&format!("{keep_stem}.")) {
            continue;
        }
        if !(name_str.starts_with("pack-") || name_str.starts_with(".pack-")) {
            continue;
        }
        let path = entry.path();
        if path.is_file() {
            let pack_path = path.with_extension("pack");
            if inventory.kept_at_inventory.contains(&pack_path)
                || path.with_extension("promisor").is_file()
                || retained_packs.contains(&pack_path)
                || !inventory.pack_paths.contains(&pack_path)
            {
                continue;
            }
            crate::git_dir::io_error::io_with_git_path(
                &git_dir,
                &path,
                crate::git_dir::GitDirPathOperation::GcPack,
                fs::remove_file(&path),
            )?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// What [`rewrite_local_objects_without`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PromisorRewrite {
    /// The promisor pack that now holds the kept objects; `None` when nothing was
    /// rewritten.
    pub(crate) pack_path: Option<PathBuf>,
    /// Objects written into that pack.
    pub(crate) objects: usize,
    /// Objects of the drop set that were stored (packed or loose) and are now gone.
    pub(crate) dropped: usize,
    /// Pack files (with their sidecars) removed.
    pub(crate) removed_pack_files: usize,
}

/// When [`rewrite_local_objects_without`] rewrites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromisorRewriteWhen {
    /// Only when it drops something.
    Dropping,
    /// Also to fold every local pack and loose object into one promisor pack.
    Always,
}

/// Rewrite the repository's own packs and loose objects into one promisor pack that
/// leaves out `omit`.
///
/// Every object in the local packs without a `.keep` marker, promisor or not, and
/// every loose object goes into the new pack, except those in `omit`; there is no
/// reachability walk, so unreachable objects are kept and nothing else can be lost.
/// The new pack is marked `.promisor`, which makes the dropped objects promised as
/// long as an object in it refers to them (a tree naming a dropped blob, say).
/// `.keep` packs and alternates are left alone, so objects stored there stay.
///
/// The new pack is installed before anything is removed, and only the packs listed
/// when the rewrite started are removed afterwards: a pack that arrives meanwhile
/// (a push) survives. Then loose copies of the dropped objects and loose objects
/// now packed are deleted. The multi-pack-index is removed; no bitmap is written.
///
/// The new pack, its index and markers are flushed to disk before any old pack is
/// removed, so a crash never leaves an object in neither.
///
/// Callers hold the [`GcLock`]. With [`PromisorRewriteWhen::Dropping`], an `omit`
/// that names nothing stored in the rewritten packs or loose changes nothing. With
/// [`PromisorRewriteWhen::Always`] the rewrite also runs to fold other packs and
/// loose objects into one promisor pack, and is skipped only when the local
/// objects already are exactly one promisor pack (plus `.keep` packs).
///
/// # Errors
///
/// [`Error::UnsupportedObjectStore`] without a files-backed object store, and pack
/// read, write and file removal failures.
pub(crate) fn rewrite_local_objects_without(
    repo: &Repository,
    omit: &OidHashSet,
    opts: &RepackOptions,
    when: PromisorRewriteWhen,
) -> Result<PromisorRewrite> {
    let odb = repo.odb();
    odb.require_files_primary("promisor rewrite")?;
    if omit.is_empty() && when == PromisorRewriteWhen::Dropping {
        return Ok(PromisorRewrite::default());
    }
    let objects_dir = odb.objects_dir().to_path_buf();
    let pack_dir = objects_dir.join("pack");
    odb.reprepare_local_packs();
    let snapshot: Vec<_> = odb
        .local_pack_indexes()?
        .into_iter()
        .filter(|idx| !pack_has_keep_marker(&pack_dir, &idx.pack_path))
        .collect();

    let mut send = Vec::new();
    let mut seen: OidHashSet = OidHashSet::default();
    let mut dropped: OidHashSet = OidHashSet::default();
    let mut take = |oid: ObjectId| {
        if omit.contains(&oid) {
            dropped.insert(oid);
        } else if seen.insert(oid) {
            send.push(oid);
        }
    };
    for idx in &snapshot {
        for entry in idx.iter() {
            take(ObjectId::from_bytes(entry.oid())?);
        }
    }
    let loose = odb.enumerate_local_loose_objects()?;
    for (oid, _) in &loose {
        take(*oid);
    }
    let folded = loose.is_empty() && snapshot.len() <= 1 && snapshot.iter().all(|i| i.is_promisor);
    let nothing_to_do = match when {
        PromisorRewriteWhen::Dropping => dropped.is_empty(),
        PromisorRewriteWhen::Always => dropped.is_empty() && folded,
    };
    if nothing_to_do {
        return Ok(PromisorRewrite::default());
    }

    let cfg = opts
        .merged_config
        .clone()
        .or_else(|| repo.config().ok().map(|c| (*c).clone()));
    let pack_opts = opts
        .pack_build
        .unwrap_or_else(|| PackBuildOptions::for_local_copy(cfg.as_ref()));
    let built = write_staged_repack_pack(
        odb,
        &pack_dir,
        &send,
        &HashMap::new(),
        &pack_opts,
        opts.threads,
        cfg.as_ref(),
    )?;
    clear_pack_midx_state(&pack_dir)?;
    let mut rewrite = PromisorRewrite {
        dropped: dropped.len(),
        ..PromisorRewrite::default()
    };
    if let Some(staging) = built.staging_path.as_ref() {
        let trusted_rows = (!built.index_rows.is_empty()).then_some(built.index_rows);
        let ingested = install_pack_path(
            staging,
            odb,
            &IngestPackOptions {
                threads: opts.threads,
                merged_config: cfg.clone(),
                environment: opts.environment.clone(),
                skip_post_index_verify: trusted_rows.is_some(),
                trusted_index_rows: trusted_rows,
                mark_promisor_pack: true,
                ..IngestPackOptions::default()
            },
        )?;
        rewrite.objects = ingested.object_ids.len();
        if let Some(pack_path) = &ingested.pack_path {
            if cfg
                .as_ref()
                .is_none_or(|c| c.pack_write_reverse_index_default(&opts.environment))
            {
                write_pack_rev_sidecar_for_index_path(&pack_path.with_extension("idx"))?;
            }
            crate::index_pack::sync_installed_pack(pack_path)?;
        }
        rewrite.pack_path = ingested.pack_path;
    }

    // Drop cached pack mmaps before deleting pack files (Windows).
    odb.invalidate_packs();
    let new_stem = rewrite
        .pack_path
        .as_deref()
        .and_then(Path::file_stem)
        .map(|s| s.to_os_string());
    let stems: HashSet<std::ffi::OsString> = snapshot
        .iter()
        .filter_map(|idx| idx.pack_path.file_stem().map(|s| s.to_os_string()))
        .filter(|stem| Some(stem) != new_stem.as_ref())
        .collect();
    drop(snapshot);
    rewrite.removed_pack_files = remove_pack_files_with_stems(&pack_dir, &stems)?;
    for oid in &dropped {
        odb.remove_local_loose(oid)?;
    }
    prune_packed_objects(&objects_dir, PrunePackedOptions::default())?;
    odb.invalidate_packs();
    Ok(rewrite)
}

/// Remove every file in `pack_dir` named `<stem>.<ext>` for one of `stems` (the
/// pack, its index and every sidecar). A pack that has gained a `.keep` marker since
/// it was listed is left whole. Returns how many files went.
fn remove_pack_files_with_stems(
    pack_dir: &Path,
    stems: &HashSet<std::ffi::OsString>,
) -> Result<usize> {
    if stems.is_empty() {
        return Ok(0);
    }
    let mut removed = 0usize;
    for entry in fs::read_dir(pack_dir)?.filter_map(|e| e.ok()) {
        let path = entry.path();
        let Some(stem) = path.file_stem().filter(|stem| stems.contains(*stem)) else {
            continue;
        };
        if !path.is_file() || pack_dir.join(stem).with_extension("keep").is_file() {
            continue;
        }
        fs::remove_file(&path)?;
        removed += 1;
    }
    Ok(removed)
}

fn pack_has_keep_marker(pack_dir: &Path, pack_path: &Path) -> bool {
    let Some(stem) = pack_path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    pack_dir.join(format!("{stem}.keep")).is_file()
}

/// Pack loose refs into `packed-refs` (files backend) or compact reftable stacks.
///
/// With the files backend, directories emptied by removing packed loose refs are
/// deleted, but `refs/` itself is kept so Git still recognizes the repository.
///
/// # Errors
///
/// Propagates ref-store, locking, and I/O failures.
pub fn pack_refs(repo: &Repository, opts: &PackRefsOptions) -> Result<PackRefsReport> {
    let (path, refs_packed, refs_pruned) =
        crate::refs::pack_refs_for_repository(repo, opts.all, opts.dry_run)?;
    Ok(PackRefsReport {
        path,
        refs_packed,
        refs_pruned,
    })
}

impl Repository {
    /// Write a reachable commit-graph file for this repository.
    ///
    /// # Errors
    ///
    /// See [`write_commit_graph`].
    pub fn write_commit_graph(&self, opts: &CommitGraphOptions) -> Result<CommitGraphWriteReport> {
        write_commit_graph(self, opts)
    }

    /// Build a reachable commit-graph in memory, for any object store.
    ///
    /// # Errors
    ///
    /// See [`build_commit_graph`].
    pub fn build_commit_graph(&self, opts: &CommitGraphOptions) -> Result<CommitGraphBuild> {
        build_commit_graph(self, opts)
    }

    /// Write or refresh the multi-pack index for this repository's pack directory.
    ///
    /// # Errors
    ///
    /// See [`write_midx`].
    pub fn write_midx(&self, opts: &MidxOptions) -> Result<MidxWriteReport> {
        write_midx(self, opts)
    }
}

/// Standalone object fsck helpers.
pub mod fsck {
    pub use crate::fsck_msg_policy::{
        FsckMsgLevel, ObjectFsckChecker, ObjectFsckPolicy, SkipListError,
    };
    pub use crate::fsck_standalone::*;
}

/// Repository-wide fsck (object store, connectivity, refs).
pub mod repository_fsck {
    pub use crate::fsck::*;
}

#[cfg(test)]
pub(crate) mod gc_test_hooks {
    use std::cell::RefCell;

    /// Where in [`super::repack_detail`] a test hook interleaves another writer.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum GcPausePoint {
        /// After pack inventory is recorded and before maintenance roots are collected.
        AfterInventoryBeforeRoots,
        /// After maintenance roots are collected and before superseded packs are dropped.
        AfterRootsBeforeDropAll,
    }

    type GcHook = (GcPausePoint, Box<dyn FnOnce() + Send>);

    thread_local! {
        static GC_HOOK: RefCell<Option<GcHook>> = const { RefCell::new(None) };
    }

    /// Run `hook` during the next repack on this thread when it reaches `point`.
    pub fn on_next_gc(point: GcPausePoint, hook: impl FnOnce() + Send + 'static) {
        GC_HOOK.with(|slot| *slot.borrow_mut() = Some((point, Box::new(hook))));
    }

    pub(crate) fn run(point: GcPausePoint) {
        let hook = GC_HOOK.with(|slot| {
            let mut slot = slot.borrow_mut();
            match slot.as_ref() {
                Some((at, _)) if *at == point => slot.take().map(|(_, hook)| hook),
                _ => None,
            }
        });
        if let Some(hook) = hook {
            hook();
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::config::{ConfigFile, ConfigScope, ConfigSet};
    use crate::porcelain::add::stage_for_commit;
    use crate::porcelain::commit::{create_commit, CommitRequest};
    use crate::progress::{NeverCancel, NullProgress};
    use crate::repo::InitOptions;
    use tempfile::TempDir;

    /// Pack `oids` (already stored) into one new pack of `repo`, with a `.keep` marker
    /// when `keep`; returns the pack path.
    fn pack_into(repo: &Repository, oids: &[ObjectId], keep: bool) -> PathBuf {
        let pack = crate::pack_objects::build_pack_from_send_list(
            repo.odb(),
            oids,
            &HashSet::new(),
            &PackBuildOptions::default(),
            &HashMap::new(),
        )
        .unwrap();
        install_pack_bytes(
            pack,
            repo.odb(),
            &IngestPackOptions {
                keep_message: keep.then(|| "pinned".to_owned()),
                ..IngestPackOptions::default()
            },
        )
        .unwrap()
        .pack_path
        .unwrap()
    }

    #[test]
    fn promisor_rewrite_drops_only_the_omitted_objects() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::new().with_bare(true)).unwrap();
        let odb = repo.odb();
        let big = odb.write(ObjectKind::Blob, b"big blob").unwrap();
        let packed = odb.write(ObjectKind::Blob, b"packed").unwrap();
        let kept = odb.write(ObjectKind::Blob, b"kept").unwrap();
        let unreachable = odb.write(ObjectKind::Blob, b"unreachable loose").unwrap();
        let old_pack = pack_into(&repo, &[packed, big], false);
        let kept_pack = pack_into(&repo, &[kept], true);
        crate::prune_packed::prune_packed_objects(odb.objects_dir(), Default::default()).unwrap();
        // A loose copy of an omitted object goes too.
        odb.write_loose_materialize(ObjectKind::Blob, b"big blob")
            .unwrap();

        let omit: OidHashSet = [big, kept].into_iter().collect();
        let rewrite = rewrite_local_objects_without(
            &repo,
            &omit,
            &RepackOptions::default(),
            PromisorRewriteWhen::Dropping,
        )
        .unwrap();
        assert_eq!(rewrite.dropped, 1, "the kept copy is not ours to drop");
        assert_eq!(rewrite.objects, 2, "packed + unreachable");
        let new_pack = rewrite.pack_path.expect("new pack");
        assert!(new_pack.with_extension("promisor").is_file());
        assert!(!old_pack.exists());
        assert!(kept_pack.is_file() && kept_pack.with_extension("keep").is_file());
        assert!(rewrite.removed_pack_files >= 2, "old .pack and .idx");

        let repo =
            Repository::open(dir.path(), &crate::environment::RepositoryOptions::empty()).unwrap();
        let odb = repo.odb();
        assert!(!odb.exists_stored(&big));
        assert!(odb.local_loose_file_size(&big).is_none());
        for oid in [packed, kept, unreachable] {
            assert!(odb.exists_stored(&oid), "{oid} kept");
        }
        assert!(odb.enumerate_local_loose_objects().unwrap().is_empty());
    }

    #[test]
    fn promisor_rewrite_without_anything_to_drop_changes_nothing() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::new().with_bare(true)).unwrap();
        let blob = repo.odb().write(ObjectKind::Blob, b"x").unwrap();
        let absent = ObjectId::from_hex("0123456789abcdef0123456789abcdef01234567").unwrap();
        for omit in [OidHashSet::default(), [absent].into_iter().collect()] {
            let rewrite = rewrite_local_objects_without(
                &repo,
                &omit,
                &RepackOptions::default(),
                PromisorRewriteWhen::Dropping,
            )
            .unwrap();
            assert_eq!(rewrite, PromisorRewrite::default());
        }
        assert!(repo.odb().local_loose_file_size(&blob).is_some());
    }

    #[test]
    fn promisor_rewrite_always_folds_until_one_promisor_pack_is_left() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::new().with_bare(true)).unwrap();
        let odb = repo.odb();
        let pushed = odb.write(ObjectKind::Blob, b"pushed").unwrap();
        let packed = odb.write(ObjectKind::Blob, b"packed").unwrap();
        let old_pack = pack_into(&repo, &[packed], false);
        crate::prune_packed::prune_packed_objects(odb.objects_dir(), Default::default()).unwrap();
        odb.write_loose_materialize(ObjectKind::Blob, b"pushed")
            .unwrap();
        let fold = || {
            rewrite_local_objects_without(
                &repo,
                &OidHashSet::default(),
                &RepackOptions::default(),
                PromisorRewriteWhen::Always,
            )
            .unwrap()
        };
        let first = fold();
        assert_eq!(first.dropped, 0);
        assert_eq!(first.objects, 2);
        let pack = first.pack_path.expect("folded pack");
        assert!(pack.with_extension("promisor").is_file());
        assert!(!old_pack.exists());
        let repo =
            Repository::open(dir.path(), &crate::environment::RepositoryOptions::empty()).unwrap();
        assert!(repo
            .odb()
            .enumerate_local_loose_objects()
            .unwrap()
            .is_empty());
        assert!(repo.odb().exists_stored(&pushed));
        // One promisor pack and nothing loose: nothing left to fold.
        let second = rewrite_local_objects_without(
            &repo,
            &OidHashSet::default(),
            &RepackOptions::default(),
            PromisorRewriteWhen::Always,
        )
        .unwrap();
        assert_eq!(second, PromisorRewrite::default());
        assert!(pack.is_file());
    }

    #[test]
    fn pack_removal_spares_unlisted_and_newly_kept_packs() {
        let dir = TempDir::new().unwrap();
        for name in [
            "pack-a.pack",
            "pack-a.idx",
            "pack-a.rev",
            "pack-b.pack",
            "pack-b.idx",
            "pack-c.pack",
            "pack-c.keep",
        ] {
            fs::write(dir.path().join(name), b"").unwrap();
        }
        let stems: HashSet<std::ffi::OsString> =
            ["pack-a", "pack-c"].into_iter().map(Into::into).collect();
        assert_eq!(remove_pack_files_with_stems(dir.path(), &stems).unwrap(), 3);
        let mut left: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            ["pack-b.idx", "pack-b.pack", "pack-c.keep", "pack-c.pack"]
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn gc_auto_skip_single_pack_without_loose() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        std::fs::write(repo.work_tree().unwrap().join("a"), b"x").unwrap();
        stage_for_commit(&repo, &mut NullProgress, &NeverCancel).expect("stage");
        let ident = "Test <t@example.com> 1 +0000".to_string();
        create_commit(
            &repo,
            &CommitRequest {
                message: "init".into(),
                author: Some(ident.parse().expect("signature")),
                committer: Some(ident.parse().expect("signature")),
                allow_empty: false,
                sign_override: None,
                amend: false,
                assume_worktree_synced: false,
                skip_pre_commit_tree_match: false,
                staged_change_count: None,
                ..Default::default()
            },
            &mut NullProgress,
            &NeverCancel,
        )
        .expect("commit");
        git_cmd(dir.path(), &["repack", "-a", "-d"]);
        assert!(should_auto_skip_repack(0, 1, false, true, None));
        assert!(!should_auto_skip_repack(3, 1, false, true, None));
        assert!(!should_auto_skip_repack(0, 2, false, true, None));
        assert!(!should_auto_skip_repack(0, 2, false, false, None));
    }

    #[test]
    fn count_pack_files_ignores_keep_markers() {
        let dir = TempDir::new().unwrap();
        let pack_dir = dir.path().join("pack");
        fs::create_dir_all(&pack_dir).unwrap();
        fs::write(pack_dir.join("pack-aaa.pack"), b"PACK").unwrap();
        fs::write(pack_dir.join("pack-bbb.pack"), b"PACK").unwrap();
        fs::write(pack_dir.join("pack-bbb.keep"), b"").unwrap();
        assert_eq!(count_pack_files(&pack_dir), 1);
    }

    fn repo_with_config(snippet: &str) -> (TempDir, Repository) {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let config_path = repo.git_dir().join("config");
        let mut config = fs::read_to_string(&config_path).unwrap();
        config.push_str(snippet);
        fs::write(&config_path, config).unwrap();
        repo.reload_config().expect("reload config");
        (dir, repo)
    }

    fn fake_loose(repo: &Repository, fanout: &str, names: &[&str]) {
        let dir = repo.git_dir().join("objects").join(fanout);
        fs::create_dir_all(&dir).unwrap();
        for name in names {
            fs::write(dir.join(name), b"").unwrap();
        }
    }

    const NAME_A: &str = "00000000000000000000000000000000000001";
    const NAME_B: &str = "00000000000000000000000000000000000002";
    const NAME_C: &str = "00000000000000000000000000000000000003";

    #[test]
    fn auto_gc_needed_samples_one_fanout_directory() {
        // gc.auto = 300 allows ceil(300 / 256) = 2 sampled objects.
        let (_dir, repo) = repo_with_config("[gc]\n\tauto = 300\n");
        assert_eq!(auto_gc_needed(&repo).unwrap(), None);
        fake_loose(&repo, "17", &[NAME_A, NAME_B]);
        fake_loose(&repo, "18", &[NAME_C]);
        fake_loose(
            &repo,
            "17",
            &["tmp_obj_123", "0000000000000000000000000000000000000z"],
        );
        assert_eq!(auto_gc_needed(&repo).unwrap(), None);
        fake_loose(&repo, "17", &[NAME_C]);
        assert_eq!(
            auto_gc_needed(&repo).unwrap(),
            Some(AutoGcReason::TooManyLoose {
                estimate: 3 * 256,
                limit: 300
            })
        );
    }

    #[test]
    fn auto_gc_needed_is_off_when_gc_auto_is_zero() {
        let (_dir, repo) = repo_with_config("[gc]\n\tauto = 0\n\tautoPackLimit = 1\n");
        fake_loose(&repo, "17", &[NAME_A, NAME_B, NAME_C]);
        let pack_dir = repo.git_dir().join("objects/pack");
        fs::write(pack_dir.join("pack-aaa.pack"), b"PACK").unwrap();
        fs::write(pack_dir.join("pack-bbb.pack"), b"PACK").unwrap();
        assert_eq!(auto_gc_needed(&repo).unwrap(), None);
        assert_eq!(
            gc_auto(&repo, &GcOptions::default()).unwrap(),
            AutoGcOutcome::NotNeeded
        );
    }

    #[test]
    fn auto_gc_needed_counts_packs_without_keep_markers() {
        let (_dir, repo) = repo_with_config("[gc]\n\tautoPackLimit = 2\n");
        let pack_dir = repo.git_dir().join("objects/pack");
        for stem in ["pack-aaa", "pack-bbb", "pack-ccc"] {
            fs::write(pack_dir.join(format!("{stem}.pack")), b"PACK").unwrap();
        }
        assert_eq!(
            auto_gc_needed(&repo).unwrap(),
            Some(AutoGcReason::TooManyPacks { count: 3, limit: 2 })
        );
        fs::write(pack_dir.join("pack-ccc.keep"), b"").unwrap();
        assert_eq!(auto_gc_needed(&repo).unwrap(), None);
    }

    fn config_snippet(text: &str) -> ConfigSet {
        let file = ConfigFile::parse(Path::new(".git/config"), text, ConfigScope::Local).unwrap();
        let mut set = ConfigSet::new();
        set.merge(&file);
        set
    }

    #[test]
    fn prune_expire_now_and_all_have_no_grace_window() {
        let now = 1_700_000_000;
        for spec in ["now", "all", "NOW"] {
            let cfg = config_snippet(&format!("[gc]\n\tpruneExpire = {spec}\n"));
            assert!(matches!(
                gc_loose_prune_policy(&cfg, now).unwrap(),
                GcLoosePrunePolicy::Enabled {
                    keep_newer_than: None
                }
            ));
        }
        for spec in ["never", "false"] {
            let cfg = config_snippet(&format!("[gc]\n\tpruneExpire = {spec}\n"));
            assert!(matches!(
                gc_loose_prune_policy(&cfg, now).unwrap(),
                GcLoosePrunePolicy::Disabled
            ));
        }
        let cfg = ConfigSet::new();
        assert!(matches!(
            gc_loose_prune_policy(&cfg, now).unwrap(),
            GcLoosePrunePolicy::Enabled {
                keep_newer_than: Some(_)
            }
        ));
    }

    #[test]
    fn gc_lock_is_exclusive_and_released_on_drop() {
        let (_dir, repo) = repo_with_config("");
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let pid_path = repo.git_dir().join("gc.pid");
        let GcLockAttempt::Acquired(lock) = GcLock::try_acquire(&repo, now).unwrap() else {
            panic!("first acquire must succeed");
        };
        let recorded = read_gc_pid(&pid_path).expect("gc.pid written");
        assert_eq!(recorded.pid, Some(u64::from(std::process::id())));
        assert_eq!(recorded.host.as_deref(), Some(local_host_name().as_str()));
        match GcLock::try_acquire(&repo, now).unwrap() {
            GcLockAttempt::Held(holder) => assert_eq!(holder, recorded),
            GcLockAttempt::Acquired(_) => panic!("second acquire must see the live holder"),
        }
        assert_eq!(GcLock::current_holder(&repo, now), Some(recorded));
        assert!(matches!(
            gc(&repo, &GcOptions::default()),
            Err(Error::MaintenanceRunning { .. })
        ));
        assert!(!repo.git_dir().join("gc.pid.lock").exists());
        drop(lock);
        assert!(!pid_path.exists());
        assert_eq!(GcLock::current_holder(&repo, now), None);
        assert!(matches!(
            GcLock::try_acquire(&repo, now).unwrap(),
            GcLockAttempt::Acquired(_)
        ));
    }

    #[test]
    fn gc_lock_takes_over_stale_entries() {
        let (_dir, repo) = repo_with_config("");
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let pid_path = repo.git_dir().join("gc.pid");
        let host = local_host_name();

        // A process on this host that is no longer running.
        fs::write(&pid_path, format!("{} {host}", i32::MAX)).unwrap();
        assert!(matches!(
            GcLock::try_acquire(&repo, now).unwrap(),
            GcLockAttempt::Acquired(_)
        ));

        // Another host cannot be checked, so a recent entry blocks.
        fs::write(&pid_path, "4242 some-other-host").unwrap();
        assert!(matches!(
            GcLock::try_acquire(&repo, now).unwrap(),
            GcLockAttempt::Held(GcLockHolder {
                pid: Some(4242),
                ..
            })
        ));

        // The same entry is ignored once it is older than 12 hours.
        let later = now + time::Duration::hours(13).whole_seconds();
        assert!(matches!(
            GcLock::try_acquire(&repo, later).unwrap(),
            GcLockAttempt::Acquired(_)
        ));
    }

    #[test]
    fn gc_auto_reports_held_lock_without_running() {
        let (_dir, repo) = repo_with_config("[gc]\n\tauto = 1\n");
        fake_loose(&repo, "17", &[NAME_A, NAME_B]);
        fs::write(repo.git_dir().join("gc.pid"), "4242 some-other-host").unwrap();
        let outcome = gc_auto(&repo, &GcOptions::default()).unwrap();
        assert!(
            matches!(&outcome, AutoGcOutcome::AlreadyRunning(holder) if holder.pid == Some(4242)),
            "{outcome:?}"
        );
        assert!(repo.git_dir().join("objects/17").join(NAME_A).exists());
    }

    #[cfg(unix)]
    #[test]
    fn gc_auto_honors_failing_pre_auto_gc_hook() {
        use std::os::unix::fs::PermissionsExt as _;
        let (_dir, repo) = repo_with_config("[gc]\n\tauto = 1\n");
        fake_loose(&repo, "17", &[NAME_A, NAME_B]);
        let hook = repo.git_dir().join("hooks/pre-auto-gc");
        fs::create_dir_all(hook.parent().unwrap()).unwrap();
        fs::write(&hook, "#!/bin/sh\nexit 3\n").unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        let outcome = gc_auto(&repo, &GcOptions::default()).unwrap();
        assert!(
            matches!(outcome, AutoGcOutcome::DeclinedByHook { status: 3, .. }),
            "{outcome:?}"
        );
        assert!(!repo.git_dir().join("gc.pid").exists());
    }

    fn git_cmd(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?}");
    }

    fn assert_fsck_clean(repo: &Repository) {
        let opts = repo_fsck::FsckOptions::full_repository(true).with_check_dangling(false);
        let report = repo_fsck::fsck(repo, &opts, &mut NullProgress, &NeverCancel).expect("fsck");
        assert!(
            report.ok(&repo.config().expect("config"), &opts),
            "{:?}",
            report.issues
        );
    }

    #[test]
    fn fsck_clean_repo_ok() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        std::fs::write(repo.work_tree().unwrap().join("a"), b"x").unwrap();
        stage_for_commit(&repo, &mut NullProgress, &NeverCancel).expect("stage");
        let ident = "Test <t@example.com> 1 +0000".to_string();
        create_commit(
            &repo,
            &CommitRequest {
                message: "init".into(),
                author: Some(ident.parse().expect("signature")),
                committer: Some(ident.parse().expect("signature")),
                allow_empty: false,
                sign_override: None,
                amend: false,
                assume_worktree_synced: false,
                skip_pre_commit_tree_match: false,
                staged_change_count: None,
                ..Default::default()
            },
            &mut NullProgress,
            &NeverCancel,
        )
        .expect("commit");
        assert_fsck_clean(&repo);
    }

    #[test]
    fn incremental_repack_with_bitmap_keeps_base_pack_and_writes_midx_bitmap() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let wt = repo.work_tree().unwrap();
        std::fs::write(wt.join("base"), b"v1").unwrap();
        stage_for_commit(&repo, &mut NullProgress, &NeverCancel).expect("stage");
        let ident = "Test <t@example.com> 1 +0000".to_string();
        create_commit(
            &repo,
            &CommitRequest {
                message: "base".into(),
                author: Some(ident.parse().expect("signature")),
                committer: Some(ident.parse().expect("signature")),
                allow_empty: false,
                sign_override: None,
                amend: false,
                assume_worktree_synced: false,
                skip_pre_commit_tree_match: false,
                staged_change_count: None,
                ..Default::default()
            },
            &mut NullProgress,
            &NeverCancel,
        )
        .expect("commit");
        repack_all(
            &repo,
            &RepackOptions {
                keep_unreachable: false,
                ..RepackOptions::default()
            },
        )
        .expect("repack base");
        let pack_dir = repo.git_dir().join("objects/pack");
        assert_eq!(count_pack_files(&pack_dir), 1);

        std::fs::write(wt.join("child"), b"v2").unwrap();
        stage_for_commit(&repo, &mut NullProgress, &NeverCancel).expect("stage child");
        create_commit(
            &repo,
            &CommitRequest {
                message: "child".into(),
                author: Some(ident.parse().expect("signature")),
                committer: Some(ident.parse().expect("signature")),
                allow_empty: false,
                sign_override: None,
                amend: false,
                assume_worktree_synced: false,
                skip_pre_commit_tree_match: false,
                staged_change_count: None,
                ..Default::default()
            },
            &mut NullProgress,
            &NeverCancel,
        )
        .expect("child commit");
        assert!(count_loose_objects(repo.odb()).expect("loose") >= 2);

        let packs_before = count_pack_files(&pack_dir);
        repack_all(
            &repo,
            &RepackOptions {
                incremental: true,
                write_bitmap: true,
                ..RepackOptions::default()
            },
        )
        .expect("incremental repack with bitmap must not fail after installing a pack");

        assert!(
            count_pack_files(&pack_dir) > packs_before,
            "incremental repack should add a pack"
        );
        let midx = pack_dir.join("multi-pack-index");
        assert!(
            midx.is_file(),
            "expected multi-pack-index after bitmap repack"
        );
        let midx_bytes = fs::read(&midx).expect("read midx");
        assert_eq!(
            midx_bytes.get(4),
            Some(&1),
            "MIDX on-disk version must be v1 for system git compatibility"
        );
        let bitmaps: Vec<_> = fs::read_dir(&pack_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "bitmap"))
            .collect();
        assert!(
            !bitmaps.is_empty(),
            "expected a MIDX or pack bitmap sidecar after incremental repack with write_bitmap"
        );

        assert_fsck_clean(&repo);
    }

    #[test]
    fn incremental_repack_adds_pack_without_removing_base() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let wt = repo.work_tree().unwrap();
        std::fs::write(wt.join("base"), b"v1").unwrap();
        stage_for_commit(&repo, &mut NullProgress, &NeverCancel).expect("stage");
        let ident = "Test <t@example.com> 1 +0000".to_string();
        create_commit(
            &repo,
            &CommitRequest {
                message: "base".into(),
                author: Some(ident.parse().expect("signature")),
                committer: Some(ident.parse().expect("signature")),
                allow_empty: false,
                sign_override: None,
                amend: false,
                assume_worktree_synced: false,
                skip_pre_commit_tree_match: false,
                staged_change_count: None,
                ..Default::default()
            },
            &mut NullProgress,
            &NeverCancel,
        )
        .expect("commit");
        repack_all(
            &repo,
            &RepackOptions {
                keep_unreachable: false,
                ..RepackOptions::default()
            },
        )
        .expect("repack");
        let pack_dir = repo.git_dir().join("objects/pack");
        let packs_before = count_pack_files(&pack_dir);
        assert_eq!(packs_before, 1);
        for i in 0..5 {
            create_commit(
                &repo,
                &CommitRequest {
                    message: format!("empty-{i}"),
                    author: Some(ident.parse().expect("signature")),
                    committer: Some(ident.parse().expect("signature")),
                    allow_empty: true,
                    sign_override: None,
                    amend: false,
                    assume_worktree_synced: false,
                    skip_pre_commit_tree_match: false,
                    staged_change_count: None,
                    ..Default::default()
                },
                &mut NullProgress,
                &NeverCancel,
            )
            .expect("empty");
        }
        assert!(count_loose_objects(repo.odb()).expect("loose") >= 2);
        repack_all(
            &repo,
            &RepackOptions {
                incremental: true,
                write_bitmap: false,
                ..RepackOptions::default()
            },
        )
        .expect("incremental repack");
        assert!(
            count_pack_files(&pack_dir) >= 2,
            "incremental repack should add a pack, not replace the base"
        );
    }

    #[test]
    fn gc_full_run_records_per_phase_timings() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let wt = repo.work_tree().unwrap();
        for i in 0..5 {
            std::fs::write(wt.join(format!("f{i}.txt")), format!("{i}\n")).unwrap();
            stage_for_commit(&repo, &mut NullProgress, &NeverCancel).expect("stage");
            let ident = "Test <t@example.com> 1 +0000".to_string();
            create_commit(
                &repo,
                &CommitRequest {
                    message: format!("c{i}"),
                    author: Some(ident.parse().expect("signature")),
                    committer: Some(ident.parse().expect("signature")),
                    allow_empty: false,
                    sign_override: None,
                    amend: false,
                    assume_worktree_synced: false,
                    skip_pre_commit_tree_match: false,
                    staged_change_count: None,
                    ..Default::default()
                },
                &mut NullProgress,
                &NeverCancel,
            )
            .expect("commit");
        }
        let stats = gc(
            &repo,
            &GcOptions::full_run(false, OffsetDateTime::now_utc().unix_timestamp()),
        )
        .expect("gc");
        let t = stats.phase_timings;
        assert!(stats.repack.objects_packed.is_some_and(|n| n > 0));
        assert!(
            t.repack_ms > 0,
            "repack phase must be timed separately from full gc"
        );
        assert!(stats.pack_refs.is_some());
        assert!(stats.commit_graph.is_some());
        assert_eq!(
            t.prune_ms, stats.repack.unreachable_prune_ms,
            "prune_ms must track unreachable loose prune inside repack, not repack_ms"
        );
        // Cached ref/config paths can finish pack-refs or commit-graph in under 1 ms (0 in
        // millisecond buckets). Require repack timing plus evidence the other phases ran.
        assert!(
            t.repack_ms > 0 && stats.pack_refs.is_some() && stats.commit_graph.is_some(),
            "full gc must time repack and run pack-refs + commit-graph, got {t:?}"
        );
        assert!(stats.prune_ran, "full gc should run unreachable prune");
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn full_repack_keeps_migrated_pack_before_refs_updated() {
        use crate::receive_quarantine::ReceiveQuarantine;

        let tmp = tempfile::tempdir().expect("tempdir");
        let remote = tmp.path().join("remote.git");
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&remote).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            assert!(std::process::Command::new("git")
                .current_dir(dir)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .expect("git")
                .success());
        };
        git(&remote, &["init", "-q", "--bare", "-b", "main"]);
        git(&wt, &["init", "-q", "-b", "main"]);
        std::fs::write(wt.join("payload.txt"), b"payload\n").unwrap();
        git(&wt, &["add", "payload.txt"]);
        git(&wt, &["commit", "-qm", "base"]);
        git(&wt, &["push", "-q", remote.to_str().unwrap(), "main"]);
        git(&remote, &["repack", "-a", "-d"]);
        let repo = Repository::open(&remote, &crate::environment::RepositoryOptions::empty())
            .expect("open remote");
        let pack_dir = repo.git_dir().join("objects/pack");
        let base_pack = super::local_pack_paths(&pack_dir)
            .into_iter()
            .next()
            .expect("base pack");
        let mut q = ReceiveQuarantine::create(repo.odb(), repo.git_dir()).expect("quarantine");
        let q_root = PathBuf::from(
            q.hook_env()
                .into_iter()
                .find(|(k, _)| k == "GIT_QUARANTINE_PATH")
                .map(|(_, v)| v)
                .expect("quarantine path"),
        );
        let q_pack_dir = q_root.join("pack");
        fs::create_dir_all(&q_pack_dir).unwrap();
        for ext in ["pack", "idx", "rev"] {
            let src = base_pack.with_extension(ext);
            if src.is_file() {
                fs::copy(&src, q_pack_dir.join(format!("pack-incoming.{ext}"))).unwrap();
            }
        }
        let guards = q.migrate().expect("migrate");
        assert!(
            super::local_pack_paths(&pack_dir).len() >= 2,
            "migrated push pack must be visible before gc inventories packs"
        );
        repack_all(
            &repo,
            &RepackOptions {
                auto_skip: false,
                write_bitmap: false,
                ..RepackOptions::default()
            },
        )
        .expect("repack");
        assert!(
            pack_dir.join("pack-incoming.pack").is_file(),
            "incoming pack must survive full repack while .keep guards are held"
        );
        drop(guards);
    }

    #[test]
    fn full_repack_drops_unprotected_migrated_pack_before_refs_updated() {
        use crate::receive_quarantine::ReceiveQuarantine;

        let tmp = tempfile::tempdir().expect("tempdir");
        let remote = tmp.path().join("remote.git");
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&remote).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            assert!(std::process::Command::new("git")
                .current_dir(dir)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .expect("git")
                .success());
        };
        git(&remote, &["init", "-q", "--bare", "-b", "main"]);
        git(&wt, &["init", "-q", "-b", "main"]);
        git(&wt, &["commit", "--allow-empty", "-qm", "base"]);
        git(&wt, &["push", "-q", remote.to_str().unwrap(), "main"]);
        git(&remote, &["repack", "-a", "-d"]);
        let repo = Repository::open(&remote, &crate::environment::RepositoryOptions::empty())
            .expect("open");
        let mut q = ReceiveQuarantine::create(repo.odb(), repo.git_dir()).expect("quarantine");
        let orphan = q
            .odb()
            .write(crate::objects::ObjectKind::Blob, b"orphan-bytes")
            .expect("write in quarantine");
        drop(q.migrate().expect("migrate"));
        repack_all(
            &repo,
            &RepackOptions {
                auto_skip: false,
                write_bitmap: false,
                keep_unreachable: false,
                ..RepackOptions::default()
            },
        )
        .expect("repack");
        assert!(
            !repo.odb().exists(&orphan),
            "without .keep protection gc may drop an unreachable migrated loose object"
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn repack_retains_inventory_kept_pack_after_ref_publish_and_guard_drop() {
        use crate::receive_quarantine::ReceiveQuarantine;
        use std::process::Command;
        use std::sync::{Arc, Mutex};

        let tmp = tempfile::tempdir().expect("tempdir");
        let remote = tmp.path().join("remote.git");
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&remote).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            assert!(Command::new("git")
                .current_dir(dir)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .expect("git")
                .success());
        };
        git(&remote, &["init", "-q", "--bare", "-b", "main"]);
        git(&wt, &["init", "-q", "-b", "main"]);
        std::fs::write(wt.join("payload.txt"), b"v1\n").unwrap();
        git(&wt, &["add", "payload.txt"]);
        git(&wt, &["commit", "-qm", "base"]);
        git(&wt, &["push", "-q", remote.to_str().unwrap(), "main"]);
        git(&remote, &["repack", "-a", "-d"]);
        std::fs::write(wt.join("payload.txt"), b"v2\n").unwrap();
        git(&wt, &["add", "payload.txt"]);
        git(&wt, &["commit", "-qm", "incoming"]);
        git(&wt, &["repack", "-a", "-d"]);
        let incoming_hex = String::from_utf8(
            Command::new("git")
                .current_dir(&wt)
                .args(["rev-parse", "HEAD"])
                .output()
                .expect("rev-parse")
                .stdout,
        )
        .expect("utf8")
        .trim()
        .to_owned();
        let incoming = ObjectId::from_hex(&incoming_hex).expect("incoming tip");

        let repo = Repository::open(&remote, &crate::environment::RepositoryOptions::empty())
            .expect("open remote");
        let mut q = ReceiveQuarantine::create(repo.odb(), repo.git_dir()).expect("quarantine");
        let q_root = PathBuf::from(
            q.hook_env()
                .into_iter()
                .find(|(k, _)| k == "GIT_QUARANTINE_PATH")
                .map(|(_, v)| v)
                .expect("quarantine path"),
        );
        let q_pack_dir = q_root.join("pack");
        fs::create_dir_all(&q_pack_dir).unwrap();
        let wt_pack_dir = wt.join(".git/objects/pack");
        let wt_pack = fs::read_dir(&wt_pack_dir)
            .expect("wt pack dir")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|ext| ext == "pack"))
            .expect("wt repack pack");
        for ext in ["pack", "idx", "rev"] {
            let src = wt_pack.with_extension(ext);
            if src.is_file() {
                fs::copy(&src, q_pack_dir.join(format!("pack-incoming.{ext}"))).unwrap();
            }
        }
        assert!(
            q_pack_dir.join("pack-incoming.pack").is_file(),
            "incoming pack sidecar"
        );

        let pack_dir = repo.git_dir().join("objects/pack");
        let incoming_keep = pack_dir.join("pack-incoming.keep");
        let guards_slot = Arc::new(Mutex::new(None));
        let guards = q.migrate().expect("migrate");
        *guards_slot.lock().expect("lock") = Some(guards);
        assert!(incoming_keep.is_file());

        let remote_for_hook = remote.clone();
        let incoming_for_hook = incoming_hex.clone();
        let incoming_keep_for_hook = incoming_keep.clone();
        let guards_for_hook = Arc::clone(&guards_slot);
        super::gc_test_hooks::on_next_gc(
            super::gc_test_hooks::GcPausePoint::AfterRootsBeforeDropAll,
            move || {
                assert!(Command::new("git")
                    .current_dir(&remote_for_hook)
                    .args(["update-ref", "refs/heads/main", &incoming_for_hook,])
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .env("GIT_CONFIG_SYSTEM", "/dev/null")
                    .status()
                    .expect("update-ref")
                    .success());
                drop(guards_for_hook.lock().expect("lock").take());
                assert!(
                    !incoming_keep_for_hook.exists(),
                    "receive path drops live keep markers after publication"
                );
            },
        );

        repack_all(
            &repo,
            &RepackOptions {
                auto_skip: false,
                write_bitmap: false,
                ..RepackOptions::default()
            },
        )
        .expect("repack");

        assert!(
            repo.odb().exists(&incoming),
            "inventory-time keep must protect the pack after live markers are dropped"
        );
        assert!(Command::new("git")
            .current_dir(&remote)
            .args(["fsck", "--strict"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("fsck")
            .success());
    }
}
