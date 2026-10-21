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

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use crate::commit_graph_file::CommitGraphChain;
use crate::commit_graph_write::{
    atomic_replace_commit_graph, build_commit_graph_bytes,
    collect_reachable_commit_oids_for_repository, load_commit_graph_commit_info,
    remove_commit_graph_chain_dir, BloomWriteStats,
};
use crate::config::ConfigSet;
use crate::diff::bloom::BloomFilterSettings;
use crate::environment::Environment;
use crate::error::{ConfigError, Error, Result};
use crate::fsck as repo_fsck;
use crate::gc::{prune_loose_unreachable, reachability_closure_for_roots, PruneStats};
use crate::git_date::approx::approxidate_careful_at;
use crate::index_pack::{install_pack_bytes, IngestPackOptions};
use crate::midx::{
    clear_pack_midx_state, write_multi_pack_index_with_options, MidxBitmapWriteOutcome,
    WriteMultiPackIndexOptions, WriteMultiPackIndexResult,
};
use crate::objects::{
    oid_hash_map, oid_hash_map_from, oid_hash_set_from, parse_commit, parse_tag, parse_tree,
    ObjectId, ObjectKind,
};
use crate::odb::Odb;
use crate::odb::UnreachablePackRetention;
use crate::pack_bitmap::{PackBitmapWriteOptions, PackBitmapWriter};
use crate::pack_object_select::{enumerate_pack_objects, PackEnumerateOptions};
use crate::pack_objects::{
    build_pack_from_send_list_with_parallel_threads, build_pack_with_write_threads,
    PackBuildOptions,
};
use crate::pack_rev::write_pack_rev_sidecar_for_index_path;
use crate::progress::{NeverCancel, NullProgress};
use crate::prune_packed::{prune_packed_objects, PrunePackedOptions};
use crate::reflog::{
    expire_reflog_git, list_reflog_refs, load_gc_reflog_expire_config, ReflogExpireParams,
};
use crate::refs_fsck::{RefsFsckIssue, RefsFsckSeverity};
use crate::repo::Repository;
use repo_fsck::FsckIssue as RepoFsckIssue;
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
    /// Override pack-objects settings; `None` uses [`PackBuildOptions::for_local_copy`].
    pub pack_build: Option<PackBuildOptions>,
    /// Config/environment for ingest and pack threads (defaults to repo config).
    pub merged_config: Option<ConfigSet>,
    /// Process environment for config discovery and thread counts.
    pub environment: Environment,
    /// Pack only loose objects into a new pack, keeping existing packs (Git cruft-style incremental repack).
    ///
    /// When [`Self::write_bitmap`] is true and the repository already has pack files, bitmaps are
    /// written via a multi-pack-index sidecar instead of a per-pack `.bitmap` on the new pack alone.
    pub incremental: bool,
    /// When true, skip rebuilding packs when Git `gc` would not (`gc.auto` / `gc.autoPackLimit`).
    pub auto_skip: bool,
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
    /// Pack sidecar files removed from `objects/pack/` (excluding `.keep` packs).
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
    /// Objects written into the new pack (`None` when [`RepackOptions::dry_run`]).
    pub objects_packed: Option<usize>,
    /// Bytes in the new pack (`None` on dry run).
    pub pack_bytes: Option<u64>,
    /// Path to the installed pack (`None` on dry run or when nothing was packed).
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
}

/// Result of writing `objects/info/commit-graph`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CommitGraphWriteReport {
    /// Path to the monolithic commit-graph file.
    pub path: PathBuf,
    /// Commits encoded in the graph.
    pub commit_count: usize,
    /// Whether a split `commit-graphs/` chain was removed.
    pub removed_chain: bool,
    /// Bloom generation counters when [`CommitGraphOptions::changed_paths`] was set.
    pub bloom_stats: Option<BloomWriteStats>,
}

/// Options for [`Repository::write_midx`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct MidxOptions {
    /// Write a reachability `.bitmap` sidecar when the MIDX is a root file.
    pub bitmap: bool,
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

/// Options for [`fsck()`].
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct FsckOptions {
    /// Treat ref-fsck warnings as errors (Git `--strict` for references).
    pub strict: bool,
    /// Validate reachable object graph and object headers.
    pub check_objects: bool,
    /// Run [`refs_fsck()`](crate::refs_fsck::refs_fsck).
    pub check_refs: bool,
    /// Run [`verify_midx`](crate::midx::verify_midx) when a multi-pack-index is present.
    pub check_midx: bool,
}

/// Severity of one [`FsckIssue`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsckSeverity {
    /// Repository should be treated as broken.
    Error,
    /// Problem that may be acceptable depending on policy.
    Warning,
    /// Informational object-buffer diagnostic.
    Info,
}

/// One fsck diagnostic (refs, objects, or MIDX).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsckIssue {
    /// Whether this issue should fail the overall fsck run.
    pub severity: FsckSeverity,
    /// Ref name, object id hex, or `multi-pack-index`.
    pub path: String,
    /// CamelCase message id (Git fsck vocabulary where applicable).
    pub msg_id: String,
    /// Human-readable detail after the message id.
    pub detail: String,
}

/// Aggregate fsck result.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FsckReport {
    /// True when no [`FsckSeverity::Error`] issues were recorded.
    pub ok: bool,
    /// All recorded fsck findings in discovery order.
    pub issues: Vec<FsckIssue>,
}

/// Format one issue like Git fsck / refs verify (`level: path: msgId: detail`).
#[must_use]
pub fn format_fsck_line(issue: &FsckIssue) -> String {
    let level = match issue.severity {
        FsckSeverity::Error => "error",
        FsckSeverity::Warning => "warning",
        FsckSeverity::Info => "info",
    };
    format!(
        "{}: {}: {}: {}",
        level, issue.path, issue.msg_id, issue.detail
    )
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
    /// When true, write `objects/info/commit-graph` with changed paths after repack/prune.
    pub write_commit_graph: bool,
}

impl Default for GcOptions {
    fn default() -> Self {
        Self {
            repack: RepackOptions {
                auto_skip: false,
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
                auto_skip: false,
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
                auto_skip: false,
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
                auto_skip: false,
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

impl FsckOptions {
    /// Check objects, refs, and MIDX with optional strict ref policy.
    #[must_use]
    pub fn full_repository(strict: bool) -> Self {
        Self {
            strict,
            check_objects: true,
            check_refs: true,
            check_midx: true,
        }
    }

    /// Check objects and refs without validating MIDX.
    #[must_use]
    pub fn objects_and_refs(strict: bool) -> Self {
        Self {
            strict,
            check_objects: true,
            check_refs: true,
            check_midx: false,
        }
    }

    /// Validate only the multi-pack-index when present.
    #[must_use]
    pub fn midx_only(strict: bool) -> Self {
        Self {
            strict,
            check_objects: false,
            check_refs: false,
            check_midx: true,
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

pub(crate) fn commit_graph_writes_generation_data(config: &ConfigSet) -> bool {
    config
        .get("commitgraph.generationversion")
        .and_then(|s| s.trim().parse::<i32>().ok())
        .map(|v| v >= 2)
        .unwrap_or(true)
}

/// Write a monolithic reachable commit-graph under `objects/info/commit-graph`.
///
/// Reachable commits are discovered from [`Repository::refs`](crate::repo::Repository::refs)
/// (reftable, packed-refs, and linked-worktree visibility), not by scanning `git_dir/refs` alone.
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
    let config = repo.config()?;
    let bloom = bloom_settings_for_commit_graph_write(&config, opts.changed_paths);
    let write_generation = commit_graph_writes_generation_data(&config);

    let chain = CommitGraphChain::try_load(objects_dir).ok().flatten();

    let commits = collect_reachable_commit_oids_for_repository(repo)?;
    let mut sorted: Vec<ObjectId> = commits.into_iter().collect();
    sorted.sort();
    let commit_count = sorted.len();

    let mut infos = oid_hash_map();
    for oid in &sorted {
        infos.insert(*oid, load_commit_graph_commit_info(odb, *oid)?);
    }

    let mut existing_filters = oid_hash_map();
    let mut upgraded_filters = oid_hash_map();
    if opts.changed_paths {
        if let Some(ref chain) = chain {
            for oid in &sorted {
                if let Some(bytes) = chain.existing_filter_bytes(oid, &bloom) {
                    existing_filters.insert(*oid, bytes);
                } else if let Some(bytes) = chain.upgradable_filter_bytes(oid, &bloom) {
                    upgraded_filters.insert(*oid, bytes);
                }
            }
        }
    }

    let (bytes, bloom_stats) = build_commit_graph_bytes(
        &sorted,
        &infos,
        odb,
        opts.changed_paths,
        &bloom,
        None,
        &[],
        None,
        &existing_filters,
        &upgraded_filters,
        write_generation,
    )?;

    let path = atomic_replace_commit_graph(objects_dir, &bytes)?;
    let removed_chain = remove_commit_graph_chain_dir(objects_dir);
    repo.caches().invalidate_commit_graph_cache();

    Ok(CommitGraphWriteReport {
        path,
        commit_count,
        removed_chain,
        bloom_stats: opts.changed_paths.then_some(bloom_stats),
    })
}

/// Write or refresh `objects/pack/multi-pack-index` for the repository.
///
/// # Errors
///
/// Returns [`Error::UnsupportedObjectStore`] for non-files backends, or MIDX write failures.
pub fn write_midx(repo: &Repository, opts: &MidxOptions) -> Result<MidxWriteReport> {
    let odb = repo.odb();
    odb.require_files_primary("midx write")?;
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

    let write_opts = WriteMultiPackIndexOptions {
        write_bitmap: opts.bitmap,
        ..WriteMultiPackIndexOptions::default()
    };
    let WriteMultiPackIndexResult { bitmap } =
        write_multi_pack_index_with_options(&pack_dir, &write_opts)?;
    let path = pack_dir.join("multi-pack-index");
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

/// Repack all reachable objects into a single pack and drop redundant storage.
///
/// Mirrors a minimal `git repack -ad`: one pack for [`reachability_roots`](crate::fsck::reachability_roots), optional `.rev` /
/// `.bitmap` sidecars, delete other packs, [`prune_packed_objects`], and optional unreachable
/// loose-object pruning.
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

    let roots = collect_maintenance_object_roots(repo)?;
    if roots.is_empty() && loose_before == 0 && packs_before == 0 {
        return Ok(RepackDetail {
            report,
            prune_packed_loose: 0,
            unreachable_loose: PruneStats::default(),
            unreachable_prune_ms: 0,
            unreachable_prune_ran: false,
        });
    }

    let cfg = opts
        .merged_config
        .clone()
        .or_else(|| repo.config().ok().map(|c| (*c).clone()));
    let pack_opts = opts.pack_build.unwrap_or_else(|| {
        if opts.incremental {
            PackBuildOptions::for_local_copy(cfg.as_ref())
        } else if opts.auto_skip && packs_before > 0 {
            // `grit gc --auto` consolidating existing packs: reuse on-disk deltas.
            PackBuildOptions::for_repack_rewrite(cfg.as_ref())
        } else {
            // Explicit `grit repack`, and gc when building the first pack from loose
            // objects: match Git repack/gc delta consolidation quality.
            PackBuildOptions::for_local_copy(cfg.as_ref())
        }
    });

    let run_incremental = opts.incremental;

    let skip_repack = should_auto_skip_repack(
        loose_before,
        packs_before,
        run_incremental,
        opts.auto_skip,
        cfg.as_ref(),
    );

    if opts.dry_run {
        report.objects = if skip_repack {
            0
        } else if run_incremental {
            odb.enumerate_local_loose_objects()?.len()
        } else {
            estimate_repack_object_count(odb, &roots, opts.keep_unreachable, &pack_opts)?
        };
        return Ok(RepackDetail {
            report,
            prune_packed_loose: 0,
            unreachable_loose: PruneStats::default(),
            unreachable_prune_ms: 0,
            unreachable_prune_ran: false,
        });
    }

    let pack_bytes = if skip_repack {
        Vec::new()
    } else if run_incremental {
        pack_loose_objects_incremental(odb, &pack_opts, opts.threads)?
    } else {
        build_repack_pack(odb, &roots, opts.keep_unreachable, &pack_opts, opts.threads)?
    };
    if pack_bytes.is_empty() {
        return Ok(RepackDetail {
            report,
            prune_packed_loose: 0,
            unreachable_loose: PruneStats::default(),
            unreachable_prune_ms: 0,
            unreachable_prune_ran: false,
        });
    }
    let mut prune_packed_loose = 0usize;
    let mut unreachable_loose = PruneStats::default();
    let pack_trailer_len = 12 + odb.hash_algo().len();
    if pack_bytes.len() > pack_trailer_len {
        clear_pack_midx_state(&pack_dir)?;

        let ingest_opts = IngestPackOptions {
            fix_thin: false,
            threads: opts.threads,
            merged_config: cfg.clone(),
            environment: opts.environment.clone(),
            ..IngestPackOptions::default()
        };
        let ingested = install_pack_bytes(pack_bytes, odb, &ingest_opts)?;
        report.packs_written = 1;
        report.objects = ingested.object_ids.len();

        let idx_path = ingested.pack_path.with_extension("idx");
        if cfg
            .as_ref()
            .is_none_or(|c| c.pack_write_reverse_index_default(&opts.environment))
        {
            write_pack_rev_sidecar_for_index_path(&idx_path)?;
        }

        if opts.write_bitmap {
            if opts.incremental && packs_before > 0 {
                // Per-pack bitmaps require reachability closure within one pack; incremental
                // repack keeps older packs and adds a new layer. Write a multi-pack-index
                // bitmap covering all packs instead of a sidecar on the new pack alone.
                let midx_opts = WriteMultiPackIndexOptions {
                    write_bitmap: true,
                    // On-disk v2 is not readable by Git 2.43; match grit_write_midx compat tests.
                    version: Some(1),
                    ..WriteMultiPackIndexOptions::default()
                };
                let WriteMultiPackIndexResult { bitmap: _ } =
                    write_multi_pack_index_with_options(&pack_dir, &midx_opts)?;
            } else {
                let bitmap_opts = PackBitmapWriteOptions::default();
                let now = opts
                    .now
                    .map(offset_to_system_time)
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                PackBitmapWriter::write(repo, &idx_path, &bitmap_opts, now)
                    .map_err(|e| Error::CorruptObject(e.to_string()))?;
            }
        }

        let retention = if opts.keep_unreachable {
            UnreachablePackRetention::KeepAll
        } else if let Some(cutoff) = opts.prune_older_than.map(offset_to_system_time) {
            UnreachablePackRetention::KeepGraceProtected(cutoff)
        } else {
            UnreachablePackRetention::DropAll
        };
        let reachable = oid_hash_set_from(&reachability_closure_for_roots(odb, &roots)?);
        let _ = odb.loosen_unreachable_from_superseded_packs(
            &pack_dir,
            &ingested.pack_path,
            &reachable,
            retention,
        )?;

        // Drop cached pack/index mmaps before deleting superseded pack artifacts (Windows).
        odb.invalidate_packs();

        report.packs_removed = if opts.incremental {
            0
        } else {
            remove_superseded_pack_files(&pack_dir, &ingested.pack_path)?
        };

        prune_packed_loose =
            prune_packed_objects(objects_dir, PrunePackedOptions::default())?.len();

        if opts.write_commit_graph {
            write_commit_graph(repo, &CommitGraphOptions::with_changed_paths(true))?;
        }
    }

    let mut unreachable_prune_ms = 0u64;
    let mut unreachable_prune_ran = false;
    if !opts.keep_unreachable {
        let keep_newer_than = opts.prune_older_than.map(offset_to_system_time);
        let prune_start = Instant::now();
        unreachable_loose = prune_loose_unreachable(odb, &roots, keep_newer_than)?;
        unreachable_prune_ms = prune_start.elapsed().as_millis() as u64;
        unreachable_prune_ran = true;
    }

    report.loose_removed = prune_packed_loose + unreachable_loose.pruned;
    Ok(RepackDetail {
        report,
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
    stats.objects_packed = Some(report.objects);
    stats.removed_pack_files = report.packs_removed;
    stats.pruned_loose = detail.prune_packed_loose;
    stats.unreachable_loose_pruned = detail.unreachable_loose.pruned;
    stats.unreachable_loose_kept = detail.unreachable_loose.kept;
    stats.unreachable_prune_ms = detail.unreachable_prune_ms;
    stats.unreachable_prune_ran = detail.unreachable_prune_ran;
    if report.packs_written > 0 {
        let packs: Vec<_> = fs::read_dir(&pack_dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
            .map(|e| e.path())
            .collect();
        if packs.len() == 1 {
            stats.pack_path = Some(packs[0].clone());
            stats.pack_bytes = packs[0].metadata().ok().map(|m| m.len());
        }
    }
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

/// Check ref integrity, reachable object graph, and optional MIDX.
pub fn fsck(repo: &Repository, opts: &FsckOptions) -> Result<FsckReport> {
    let config = repo.config()?;
    let inner_opts = repo_fsck::FsckOptions {
        connectivity_only: !opts.check_objects,
        check_connectivity: opts.check_objects,
        check_dangling: false,
        strict: opts.strict,
    };
    let inner = repo_fsck::fsck(repo, &inner_opts, &mut NullProgress, &NeverCancel)?;
    let mut issues = Vec::new();
    for issue in inner.issues {
        match &issue {
            RepoFsckIssue::BadRef(_) if !opts.check_refs => continue,
            RepoFsckIssue::MidxInvalid { .. } if !opts.check_midx => continue,
            _ => {}
        }
        issues.push(fsck_issue_from_repo(issue, &config, opts.strict));
    }
    let ok = !issues.iter().any(|i| i.severity == FsckSeverity::Error);
    Ok(FsckReport { ok, issues })
}

fn gc_repack_writes_bitmap(repo: &Repository, config: &ConfigSet) -> bool {
    if let Some(Ok(v)) = config.get_bool("repack.writeBitmaps") {
        return v;
    }
    repo.work_tree().is_none()
}

fn pack_loose_objects_incremental(
    odb: &Odb,
    pack_opts: &PackBuildOptions,
    threads: Option<usize>,
) -> Result<Vec<u8>> {
    let loose: Vec<ObjectId> = odb
        .enumerate_local_loose_objects()?
        .into_iter()
        .map(|(oid, _)| oid)
        .collect();
    if loose.is_empty() {
        return Ok(Vec::new());
    }
    let mut incremental_opts = *pack_opts;
    incremental_opts.delta = false;
    incremental_opts.reuse_deltas = false;
    incremental_opts.use_bitmaps = false;
    build_pack_from_send_list_with_parallel_threads(
        odb,
        &loose,
        &HashSet::new(),
        &incremental_opts,
        &oid_hash_map(),
        None,
        threads,
    )
}

/// Whether maintenance should skip rebuilding packs (Git `gc --auto` heuristics).
///
/// Unreachable or grace-protected objects in an existing pack do **not** trigger repack; only
/// `gc.auto` (loose count) and `gc.autoPackLimit` (pack count) do, matching Git.
fn should_auto_skip_repack(
    loose_objects: usize,
    pack_count: usize,
    incremental: bool,
    auto_skip: bool,
    config: Option<&ConfigSet>,
) -> bool {
    if !auto_skip || incremental {
        return false;
    }
    if !config.is_some_and(ConfigSet::gc_auto_loose_disabled) {
        let loose_auto = config.map(ConfigSet::gc_auto_loose_limit).unwrap_or(6700);
        if loose_objects >= loose_auto {
            return false;
        }
    }
    if pack_count == 0 {
        return loose_objects == 0;
    }
    if config.is_some_and(ConfigSet::gc_auto_pack_limit_disabled) {
        return true;
    }
    let pack_limit = config.map(ConfigSet::gc_auto_pack_limit).unwrap_or(50);
    pack_count <= pack_limit
}

/// Delete unreachable loose objects using `gc.pruneExpire` or an explicit cutoff.
///
/// # Errors
///
/// Propagates config parse failures, unsupported ODB backends, and I/O errors.
pub fn prune_loose_objects(repo: &Repository, opts: &PruneOptions) -> Result<PruneStats> {
    let odb = repo.odb();
    odb.require_files_primary("prune")?;
    if opts.dry_run {
        return Ok(PruneStats::default());
    }
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
    prune_loose_unreachable(odb, &roots, keep_newer_than)
}

fn duration_ms(d: std::time::Duration) -> u64 {
    d.as_millis() as u64
}

/// Run pack-refs, reflog expiry, repack, prune, and commit-graph maintenance.
pub fn gc(repo: &Repository, opts: &GcOptions) -> Result<GcStats> {
    let mut stats = GcStats::default();
    let odb = repo.odb();
    odb.require_files_primary("gc")?;

    let now = gc_now_unix(opts.now_unix);
    let cfg = repo.config()?;
    let prune_policy = gc_loose_prune_policy(&cfg, now)?;
    let dry_run = opts.repack.dry_run;

    if opts.pack_refs && !dry_run {
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
    if !repack_opts.write_bitmap {
        repack_opts.write_bitmap = gc_repack_writes_bitmap(repo, &cfg);
    }
    let repack_start = Instant::now();
    stats.repack = repack_all(repo, &repack_opts)?;
    let repack_total_ms = duration_ms(repack_start.elapsed());
    stats.phase_timings.prune_ms = stats.repack.unreachable_prune_ms;
    stats.phase_timings.repack_ms = repack_total_ms.saturating_sub(stats.phase_timings.prune_ms);

    if dry_run {
        return Ok(stats);
    }

    if repack_opts.keep_unreachable {
        stats.prune = PruneStats::default();
        stats.prune_ran = false;
    } else if stats.repack.unreachable_prune_ran {
        stats.prune = PruneStats {
            pruned: stats.repack.unreachable_loose_pruned,
            kept: stats.repack.unreachable_loose_kept,
        };
        stats.prune_ran = true;
    } else {
        let roots = collect_maintenance_object_roots(repo)?;
        let keep_newer_than = repack_opts.prune_older_than.map(offset_to_system_time);
        let prune_start = Instant::now();
        stats.prune = prune_loose_unreachable(odb, &roots, keep_newer_than)?;
        stats.phase_timings.prune_ms = duration_ms(prune_start.elapsed());
        stats.prune_ran = true;
    }

    if opts.write_commit_graph {
        let start = Instant::now();
        stats.commit_graph = Some(write_commit_graph(
            repo,
            &CommitGraphOptions::with_changed_paths(true),
        )?);
        stats.phase_timings.commit_graph_ms = duration_ms(start.elapsed());
    }
    let _ = odb;
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
    if spec.eq_ignore_ascii_case("never") {
        return Ok(GcLoosePrunePolicy::Disabled);
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
    let git_dir = repo.git_dir();
    let params = ReflogExpireParams {
        stale_fix: false,
        dry_run,
        verbose: false,
    };
    let mut total = 0usize;
    for refname in list_reflog_refs(git_dir)? {
        let result = expire_reflog_git(
            repo,
            git_dir,
            &refname,
            &params,
            None,
            None,
            &gc_cfg.patterns,
            gc_cfg.global_total,
            gc_cfg.global_unreachable,
            now,
        )?;
        total += result.pruned;
    }
    Ok(total)
}

fn fsck_issue_from_refs(issue: RefsFsckIssue) -> FsckIssue {
    FsckIssue {
        severity: match issue.severity {
            RefsFsckSeverity::Error => FsckSeverity::Error,
            RefsFsckSeverity::Warning => FsckSeverity::Warning,
        },
        path: issue.path,
        msg_id: issue.msg_id.to_owned(),
        detail: issue.detail,
    }
}

fn fsck_issue_from_repo(issue: RepoFsckIssue, config: &ConfigSet, strict: bool) -> FsckIssue {
    if let RepoFsckIssue::BadRef(r) = issue {
        return fsck_issue_from_refs(r);
    }
    let severity = repo_fsck::issue_severity(&issue, config, strict);
    let msg_id = repo_fsck::issue_msg_id(&issue).to_owned();
    let (path, detail) = match &issue {
        RepoFsckIssue::CorruptLoose { oid, path } => {
            (oid.to_hex(), format!("could not read {}", path.display()))
        }
        RepoFsckIssue::HashMismatch { oid, path } => (
            oid.to_hex(),
            format!("loose hash mismatch at {}", path.display()),
        ),
        RepoFsckIssue::BadObject { oid, error } => (oid.to_hex(), error.detail.clone()),
        RepoFsckIssue::PackChecksum { pack } => (
            pack.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("pack")
                .to_owned(),
            format!("pack checksum invalid for {}", pack.display()),
        ),
        RepoFsckIssue::IdxMismatch { pack } => (
            pack.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("pack")
                .to_owned(),
            format!("index checksum mismatch for {}", pack.display()),
        ),
        RepoFsckIssue::MissingObject { oid, referrer } => (
            oid.to_hex(),
            repo_fsck::missing_object_detail(*oid, *referrer),
        ),
        RepoFsckIssue::BrokenLink {
            tree,
            name,
            mode,
            target,
        } => (
            tree.to_hex(),
            format!("entry {name} mode {mode:o} -> {}", target.to_hex()),
        ),
        RepoFsckIssue::BadRef(_) => unreachable!(),
        RepoFsckIssue::MidxInvalid { details } => {
            ("multi-pack-index".to_owned(), details.join("; "))
        }
        RepoFsckIssue::CommitGraphInvalid { detail } => ("commit-graph".to_owned(), detail.clone()),
    };
    FsckIssue {
        severity: match severity {
            repo_fsck::FsckSeverity::Error => FsckSeverity::Error,
            repo_fsck::FsckSeverity::Warning => FsckSeverity::Warning,
            repo_fsck::FsckSeverity::Info => FsckSeverity::Info,
        },
        path,
        msg_id,
        detail,
    }
}

fn count_loose_objects(odb: &Odb) -> Result<usize> {
    Ok(odb.enumerate_local_loose_objects()?.len())
}

fn build_repack_pack(
    odb: &Odb,
    roots: &[ObjectId],
    keep_unreachable: bool,
    opts: &PackBuildOptions,
    threads: Option<usize>,
) -> Result<Vec<u8>> {
    if !keep_unreachable {
        return build_pack_with_write_threads(odb, roots, &[], opts, threads);
    }
    let empty_shallow = HashSet::new();
    let selection = enumerate_pack_objects(
        odb,
        roots,
        &[],
        &PackEnumerateOptions {
            filter: None,
            shallow_grafts: &empty_shallow,
            have_shallow_grafts: Some(&empty_shallow),
            use_bitmaps: opts.use_bitmaps,
            exclude_objects: None,
        },
    )?;
    let mut send = selection.send;
    append_repack_unreachable_retention(odb, &mut send)?;
    build_pack_from_send_list_with_parallel_threads(
        odb,
        &send,
        &selection.have_closure,
        opts,
        &oid_hash_map_from(&selection.blob_name_hashes),
        threads,
        threads,
    )
}

/// Append packed and loose objects not already selected for the repack send list.
fn append_repack_unreachable_retention(odb: &Odb, send: &mut Vec<ObjectId>) -> Result<()> {
    let mut in_send: HashSet<ObjectId> = send.iter().copied().collect();
    odb.primary()?
        .packed_objects()
        .for_each_removable_pack_object(&mut |oid| {
            if in_send.insert(*oid) {
                send.push(*oid);
            }
            std::ops::ControlFlow::Continue(())
        })?;
    for (oid, _) in odb.enumerate_local_loose_objects()? {
        if in_send.insert(oid) {
            send.push(oid);
        }
    }
    Ok(())
}

fn estimate_repack_object_count(
    odb: &Odb,
    roots: &[ObjectId],
    keep_unreachable: bool,
    pack_opts: &PackBuildOptions,
) -> Result<usize> {
    if !keep_unreachable {
        return estimate_reachable_count(odb, roots);
    }
    let empty_shallow = HashSet::new();
    let selection = enumerate_pack_objects(
        odb,
        roots,
        &[],
        &PackEnumerateOptions {
            filter: None,
            shallow_grafts: &empty_shallow,
            have_shallow_grafts: Some(&empty_shallow),
            use_bitmaps: pack_opts.use_bitmaps,
            exclude_objects: None,
        },
    )?;
    let mut send = selection.send;
    append_repack_unreachable_retention(odb, &mut send)?;
    Ok(send.len())
}

fn count_pack_files(pack_dir: &Path) -> usize {
    if !pack_dir.is_dir() {
        return 0;
    }
    fs::read_dir(pack_dir)
        .ok()
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| {
                    let path = e.path();
                    path.extension().is_some_and(|ext| ext == "pack")
                        && !pack_has_keep_marker(pack_dir, &path)
                })
                .count()
        })
        .unwrap_or(0)
}

fn estimate_reachable_count(odb: &Odb, roots: &[ObjectId]) -> Result<usize> {
    let mut seen: HashSet<ObjectId> = HashSet::new();
    let mut queue: VecDeque<ObjectId> = VecDeque::new();
    for &root in roots {
        if seen.insert(root) {
            queue.push_back(root);
        }
    }
    while let Some(oid) = queue.pop_front() {
        let obj = match odb.read(&oid) {
            Ok(o) => o,
            Err(_) => continue,
        };
        match obj.kind {
            ObjectKind::Commit => {
                let commit = parse_commit(&obj.data)?;
                for parent in commit.parents {
                    if seen.insert(parent) {
                        queue.push_back(parent);
                    }
                }
                if seen.insert(commit.tree) {
                    queue.push_back(commit.tree);
                }
            }
            ObjectKind::Tree => {
                for entry in parse_tree(&obj.data)? {
                    if entry.mode == 0o160000 {
                        continue;
                    }
                    if seen.insert(entry.oid) {
                        queue.push_back(entry.oid);
                    }
                }
            }
            ObjectKind::Tag => {
                let tag = parse_tag(&obj.data)?;
                if seen.insert(tag.object) {
                    queue.push_back(tag.object);
                }
            }
            ObjectKind::Blob => {}
        }
    }
    Ok(seen.len())
}

/// Remove pack artifacts in `pack_dir` except those belonging to `keep_pack`.
pub fn remove_superseded_pack_files(pack_dir: &Path, keep_pack: &Path) -> Result<usize> {
    let keep_stem = keep_pack
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| Error::Io(io::Error::new(io::ErrorKind::InvalidInput, "bad pack path")))?;
    if !pack_dir.is_dir() {
        return Ok(0);
    }
    let mut removed = 0usize;
    for entry in fs::read_dir(pack_dir).map_err(Error::Io)? {
        let entry = entry.map_err(Error::Io)?;
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
            if pack_has_keep_marker(pack_dir, &path) {
                continue;
            }
            fs::remove_file(&path).map_err(Error::Io)?;
            removed += 1;
        }
    }
    Ok(removed)
}

fn pack_has_keep_marker(pack_dir: &Path, path: &Path) -> bool {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    pack_dir.join(format!("{stem}.keep")).is_file()
}

/// Pack loose refs into `packed-refs` (files backend) or compact reftable stacks.
///
/// # Errors
///
/// Propagates ref-store, locking, and I/O failures.
pub fn pack_refs(repo: &Repository, opts: &PackRefsOptions) -> Result<PackRefsReport> {
    let (path, refs_packed, refs_pruned) = crate::refs::pack_refs_for_repository(repo, opts.all)?;
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
    pub use crate::fsck_standalone::*;
}

/// Repository-wide fsck (object store, connectivity, refs).
pub mod repository_fsck {
    pub use crate::fsck::*;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::config::{ConfigFile, ConfigScope, ConfigSet};
    use crate::porcelain::add::stage_for_commit;
    use crate::porcelain::commit::{create_commit, CommitRequest};
    use crate::progress::NullProgress;
    use crate::repo::InitOptions;
    use tempfile::TempDir;

    #[test]
    fn auto_skip_repack_matches_gc_auto_heuristics() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        std::fs::write(repo.work_tree().unwrap().join("a"), b"x").unwrap();
        stage_for_commit(&repo, &mut NullProgress).expect("stage");
        let ident = "Test <t@example.com> 1 +0000".to_string();
        create_commit(
            &repo,
            &CommitRequest {
                message: "init".into(),
                author: ident.clone(),
                committer: ident,
                allow_empty: false,
                sign_override: None,
                amend: false,
                assume_worktree_synced: false,
                skip_pre_commit_tree_match: false,
                staged_change_count: None,
            },
            &mut NullProgress,
        )
        .expect("commit");
        git_cmd(dir.path(), &["repack", "-a", "-d"]);
        assert!(should_auto_skip_repack(0, 1, false, true, None));
        assert!(should_auto_skip_repack(3, 1, false, true, None));
        assert!(!should_auto_skip_repack(6700, 1, false, true, None));
        assert!(!should_auto_skip_repack(0, 51, false, true, None));
        let mut disabled = ConfigSet::new();
        let file = ConfigFile::parse(
            Path::new(".git/config"),
            "[gc]\n\tautoPackLimit = 0\n",
            ConfigScope::Local,
        )
        .expect("parse");
        disabled.merge(&file);
        assert!(should_auto_skip_repack(0, 2, false, true, Some(&disabled)));
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

    #[test]
    fn fsck_clean_repo_ok() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        std::fs::write(repo.work_tree().unwrap().join("a"), b"x").unwrap();
        stage_for_commit(&repo, &mut NullProgress).expect("stage");
        let ident = "Test <t@example.com> 1 +0000".to_string();
        create_commit(
            &repo,
            &CommitRequest {
                message: "init".into(),
                author: ident.clone(),
                committer: ident,
                allow_empty: false,
                sign_override: None,
                amend: false,
                assume_worktree_synced: false,
                skip_pre_commit_tree_match: false,
                staged_change_count: None,
            },
            &mut NullProgress,
        )
        .expect("commit");
        let report = fsck(
            &repo,
            &FsckOptions {
                strict: true,
                check_objects: true,
                check_refs: true,
                check_midx: true,
            },
        )
        .expect("fsck");
        assert!(report.ok, "{:?}", report.issues);
    }

    #[test]
    fn incremental_repack_with_bitmap_keeps_base_pack_and_writes_midx_bitmap() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let wt = repo.work_tree().unwrap();
        std::fs::write(wt.join("base"), b"v1").unwrap();
        stage_for_commit(&repo, &mut NullProgress).expect("stage");
        let ident = "Test <t@example.com> 1 +0000".to_string();
        create_commit(
            &repo,
            &CommitRequest {
                message: "base".into(),
                author: ident.clone(),
                committer: ident.clone(),
                allow_empty: false,
                sign_override: None,
                amend: false,
                assume_worktree_synced: false,
                skip_pre_commit_tree_match: false,
                staged_change_count: None,
            },
            &mut NullProgress,
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
        stage_for_commit(&repo, &mut NullProgress).expect("stage child");
        create_commit(
            &repo,
            &CommitRequest {
                message: "child".into(),
                author: ident.clone(),
                committer: ident.clone(),
                allow_empty: false,
                sign_override: None,
                amend: false,
                assume_worktree_synced: false,
                skip_pre_commit_tree_match: false,
                staged_change_count: None,
            },
            &mut NullProgress,
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

        let report = fsck(
            &repo,
            &FsckOptions {
                strict: true,
                check_objects: true,
                check_refs: true,
                check_midx: true,
            },
        )
        .expect("fsck");
        assert!(report.ok, "{:?}", report.issues);
    }

    #[test]
    fn incremental_repack_adds_pack_without_removing_base() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let wt = repo.work_tree().unwrap();
        std::fs::write(wt.join("base"), b"v1").unwrap();
        stage_for_commit(&repo, &mut NullProgress).expect("stage");
        let ident = "Test <t@example.com> 1 +0000".to_string();
        create_commit(
            &repo,
            &CommitRequest {
                message: "base".into(),
                author: ident.clone(),
                committer: ident.clone(),
                allow_empty: false,
                sign_override: None,
                amend: false,
                assume_worktree_synced: false,
                skip_pre_commit_tree_match: false,
                staged_change_count: None,
            },
            &mut NullProgress,
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
                    author: ident.clone(),
                    committer: ident.clone(),
                    allow_empty: true,
                    sign_override: None,
                    amend: false,
                    assume_worktree_synced: false,
                    skip_pre_commit_tree_match: false,
                    staged_change_count: None,
                },
                &mut NullProgress,
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
    fn refs_fsck_line_roundtrip() {
        let issue = FsckIssue {
            severity: FsckSeverity::Error,
            path: "refs/heads/main".to_owned(),
            msg_id: "badSymref".to_owned(),
            detail: "symref target missing".to_owned(),
        };
        assert!(format_fsck_line(&issue).contains("badSymref"));
        let refs = RefsFsckIssue {
            severity: RefsFsckSeverity::Error,
            path: issue.path.clone(),
            msg_id: "badSymref",
            detail: issue.detail.clone(),
        };
        assert_eq!(
            format_fsck_line(&issue),
            crate::refs_fsck::format_refs_fsck_line(&refs)
        );
    }

    #[test]
    fn gc_full_run_records_per_phase_timings() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let wt = repo.work_tree().unwrap();
        for i in 0..5 {
            std::fs::write(wt.join(format!("f{i}.txt")), format!("{i}\n")).unwrap();
            stage_for_commit(&repo, &mut NullProgress).expect("stage");
            let ident = "Test <t@example.com> 1 +0000".to_string();
            create_commit(
                &repo,
                &CommitRequest {
                    message: format!("c{i}"),
                    author: ident.clone(),
                    committer: ident,
                    allow_empty: false,
                    sign_override: None,
                    amend: false,
                    assume_worktree_synced: false,
                    skip_pre_commit_tree_match: false,
                    staged_change_count: None,
                },
                &mut NullProgress,
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
}
