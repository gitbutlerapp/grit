//! Offload large blobs from a repository to a large-object promisor (LOP).
//!
//! A server can keep its big binaries out of its main repository: the blobs live
//! in a second repository, the LOP, and the main repository becomes a partial
//! repository of it. The main repository's config names the LOP as a promisor
//! remote (`remote.<lop>.promisor = true`), its remaining objects sit in a
//! `.promisor` pack, and upload-pack leaves the offloaded blobs out for clients
//! that accept the LOP through the `promisor-remote` capability. This is the
//! layout Git's own large-object-promisor tests build with
//! `git repack -a -d --filter=blob:limit=<n> --filter-to=<lop>`.
//!
//! [`offload_blobs`] moves the blobs a filter leaves out (`blob:limit=<n>`: every
//! blob of `n` bytes or more) from the main repository to the LOP. It never removes
//! anything from the main repository before the LOP holds it, and blobs any checkout
//! of the main repository needs stay where they are. Running it again offloads blobs
//! pushed since, and folds the main repository's packs back into one promisor pack.

use std::path::{Path, PathBuf};

use crate::config::{ConfigFile, ConfigScope, ConfigSet};
use crate::environment::RepositoryOptions;
use crate::error::{Error, Result};
use crate::index_pack::{install_pack_path, IngestPackOptions};
use crate::maintenance::{GcLock, GcLockAttempt, GcLockHolder, RepackOptions};
use crate::objects::{
    oid_hash_set, CommitRef, HashAlgo, ObjectId, ObjectKind, OidHashMap, OidHashSet, TreeRefIter,
};
use crate::odb::Odb;
use crate::pack_index::PackIndex;
use crate::pack_objects::PackBuildOptions;
use crate::partial_clone::{stored_remote_filter, ObjectFilterSpec};
use crate::repo::Repository;

/// Message written into the `.keep` marker of every pack offload adds to a LOP.
pub const LOP_KEEP_MESSAGE: &str = "grit offload: large-object promisor pack";

/// Options for [`offload_blobs`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct OffloadOptions {
    /// The promisor remote the blobs move to; `remote.<name>.url` must be set in the
    /// main repository.
    pub remote: String,
    /// Which blobs move: those this filter leaves out (`blob:limit=<n>`: blobs of `n`
    /// bytes or more; `blob:none`: every blob). `None` uses
    /// `remote.<name>.partialCloneFilter`, the same value clients receive through
    /// `promisor.sendFields`.
    pub filter: Option<ObjectFilterSpec>,
    /// The LOP's git directory on this machine. Needed when `remote.<name>.url` is
    /// not a local path, for example an `https://` URL advertised to clients for a
    /// repository the server also has on disk.
    pub lop_git_dir: Option<PathBuf>,
    /// Also set `uploadpack.allowFilter` and `uploadpack.allowAnySHA1InWant` in the
    /// LOP, so it can answer the lazy fetches clients send it.
    pub configure_lop: bool,
    /// Work out what would move without writing anything.
    pub dry_run: bool,
    /// Current time in Unix seconds, for the `gc.pid` maintenance lock.
    pub now_unix: i64,
    /// Pack writing and indexing threads (`None` reads `pack.threads`).
    pub threads: Option<usize>,
}

impl OffloadOptions {
    /// Options that offload to `remote` with its configured filter, at time `now_unix`.
    #[must_use]
    pub fn new(remote: impl Into<String>, now_unix: i64) -> Self {
        Self {
            remote: remote.into(),
            filter: None,
            lop_git_dir: None,
            configure_lop: false,
            dry_run: false,
            now_unix,
            threads: None,
        }
    }
}

/// Why [`offload_blobs`] refused to run.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OffloadError {
    /// `remote.<name>.url` is not set in the main repository.
    #[error("remote '{remote}' has no url configured")]
    RemoteNotConfigured {
        /// The remote named in [`OffloadOptions::remote`].
        remote: String,
    },
    /// The remote's URL is not a local path and no [`OffloadOptions::lop_git_dir`]
    /// was given.
    #[error("remote '{remote}' is not a local repository ({url}); give its git directory")]
    LopNotLocal {
        /// The remote name.
        remote: String,
        /// Its configured URL (credentials removed).
        url: String,
    },
    /// The LOP location is not a Git repository.
    #[error("{} is not a git repository", path.display())]
    LopNotARepository {
        /// Where the LOP was looked for.
        path: PathBuf,
    },
    /// The LOP is the main repository itself.
    #[error("the large-object promisor is this repository itself")]
    LopIsMainRepository,
    /// The two repositories use different object formats.
    #[error(
        "object formats differ: this repository uses {}, the promisor {}",
        main.name(),
        lop.name()
    )]
    HashAlgoMismatch {
        /// The main repository's object format.
        main: HashAlgo,
        /// The LOP's object format.
        lop: HashAlgo,
    },
    /// No filter was given and the remote has no `partialCloneFilter`.
    #[error("no size threshold: give a filter or set remote.{remote}.partialCloneFilter")]
    NoThreshold {
        /// The remote name.
        remote: String,
    },
    /// A blob copied to the LOP cannot be found there afterwards; nothing was
    /// removed from the main repository.
    #[error("blob {oid} is not in the promisor after copying it there")]
    NotInLopAfterCopy {
        /// The blob.
        oid: ObjectId,
    },
    /// Another maintenance run holds the main repository's `gc.pid` lock.
    #[error("maintenance is already running on this repository")]
    MaintenanceRunning {
        /// Who holds the lock.
        holder: GcLockHolder,
    },
}

/// Which repository a [`OffloadConfigChange`] was made in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OffloadRepo {
    /// The repository the blobs moved out of.
    Main,
    /// The large-object promisor.
    Lop,
}

/// One config value [`offload_blobs`] set.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OffloadConfigChange {
    /// The repository whose local config changed.
    pub repo: OffloadRepo,
    /// The key, as written (for example `remote.lop.promisor`).
    pub key: String,
    /// The new value.
    pub value: String,
}

/// Whether a LOP can answer the lazy fetches of Git clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct LopReadiness {
    /// `uploadpack.allowFilter` is true, so it accepts `filter blob:none`.
    pub allow_filter: bool,
    /// `uploadpack.allowAnySHA1InWant` is true, so it serves blobs no ref points at.
    pub allow_any_sha1_in_want: bool,
}

impl LopReadiness {
    /// Both settings are on.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.allow_filter && self.allow_any_sha1_in_want
    }

    fn from_config(config: &ConfigSet) -> Self {
        let flag = |key: &str| config.get_bool(key).and_then(|v| v.ok()).unwrap_or(false);
        Self {
            allow_filter: flag("uploadpack.allowfilter"),
            allow_any_sha1_in_want: flag("uploadpack.allowanysha1inwant"),
        }
    }
}

/// What [`offload_blobs`] did (or, with [`OffloadOptions::dry_run`], would do).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OffloadReport {
    /// The promisor remote the blobs moved to.
    pub remote: String,
    /// The LOP's git directory.
    pub lop_git_dir: PathBuf,
    /// The filter that chose the blobs.
    pub filter: ObjectFilterSpec,
    /// True when nothing was written.
    pub dry_run: bool,
    /// Blobs stored in the main repository that the filter selects and that may move.
    pub candidates: usize,
    /// Their total size, uncompressed.
    pub candidate_bytes: u64,
    /// Candidates copied into a new LOP pack.
    pub copied_to_lop: usize,
    /// Candidates the LOP already had in a `.keep` pack of its own. A loose or
    /// unkept copy there does not count (the LOP's `gc` may prune it), nor does one
    /// it reaches through alternates; those candidates are copied.
    pub already_in_lop: usize,
    /// Candidates no longer stored in the main repository afterwards.
    pub dropped_from_main: usize,
    /// Selected blobs kept because a checkout of the main repository needs them
    /// (a worktree's index or `HEAD`).
    pub skipped_checked_out: usize,
    /// Selected blobs kept because they are in a `.keep` pack (or the only tree
    /// naming them is), which offload does not rewrite.
    pub skipped_kept_pack: usize,
    /// The pack added to the LOP, if any.
    pub lop_pack: Option<PathBuf>,
    /// The promisor pack the main repository's objects now live in, if rewritten.
    pub main_pack: Option<PathBuf>,
    /// Bytes of the main repository's local packs and loose objects before.
    pub main_bytes_before: u64,
    /// The same afterwards (equal to the before value on a dry run).
    pub main_bytes_after: u64,
    /// Config values set, in the order they were written.
    pub config_changes: Vec<OffloadConfigChange>,
    /// The LOP's upload-pack settings after the run.
    pub lop_ready: LopReadiness,
    /// `promisor.advertise` is true in the main repository, so its upload-pack tells
    /// clients about the LOP.
    pub advertised: bool,
}

/// Move the blobs [`OffloadOptions::filter`] selects from `repo` to the large-object
/// promisor [`OffloadOptions::remote`].
///
/// The steps run in an order that never loses an object:
///
/// 1. Blobs reachable from the refs, reflogs and `HEAD`s, stored in `repo`, and left
///    out by the filter are the candidates. Those a checkout needs, and those in
///    `.keep` packs, stay.
/// 2. The candidates the LOP does not already hold in a `.keep` pack of its own are
///    copied into one new pack there, whole (no deltas), with a `.keep` marker
///    written before the pack appears: nothing in the LOP refers to them, so an
///    unprotected pack could be pruned. Every candidate is then checked to be in a
///    `.keep` pack of the LOP, and the new pack, index and marker are flushed to
///    disk.
/// 3. The main repository's config gets `remote.<name>.promisor = true`, when the
///    filter came from `opts` `remote.<name>.partialCloneFilter`, and
///    `repack.writeBitmaps = false`. A reachability bitmap (pack or multi-pack)
///    needs every object its commits reach in the packs it covers, and the
///    offloaded blobs are in none of them. Git's repack keeps promisor and other
///    objects in separate packs, so once a push adds a commit outside the
///    promisor pack, Git 2.50 and 2.56 fail with "Packfile doesn't have full
///    closure": `git gc` exits 128 on a bare repository, where bitmaps are on by
///    default, and `git repack -a -d -b --write-midx` fails too. Grit's own repack
///    leaves the bitmap out in that case, so the setting is for Git.
///    `promisor.advertise` and `extensions.partialClone` are left to the caller;
///    the report says whether clients are told about the LOP.
/// 4. The main repository's local packs and loose objects are rewritten into one
///    `.promisor` pack without the candidates (see the module documentation). This
///    happens on every run, even when no blob moves, so trees pushed since the last
///    run (which name offloaded blobs) join the promisor pack again. The new pack is
///    flushed to disk before the old packs are removed.
///
/// # Parameters
///
/// - `repo`: the main repository (files-backed object store).
/// - `opts`: what to move and where.
///
/// # Errors
///
/// [`Error::Offload`] for the [`OffloadError`] cases, [`Error::UnsupportedObjectStore`]
/// without a files-backed object store, and read, write and config failures.
pub fn offload_blobs(repo: &Repository, opts: &OffloadOptions) -> Result<OffloadReport> {
    let odb = repo.odb();
    odb.require_files_primary("offload")?;
    let config = repo.config()?;
    let remote = opts.remote.as_str();
    let url = config
        .get(&format!("remote.{remote}.url"))
        .filter(|u| !u.trim().is_empty())
        .ok_or_else(|| OffloadError::RemoteNotConfigured {
            remote: remote.to_owned(),
        })?;
    let filter = opts
        .filter
        .or_else(|| stored_remote_filter(&config, remote))
        .ok_or_else(|| OffloadError::NoThreshold {
            remote: remote.to_owned(),
        })?;
    let lop = open_lop(repo, opts, &url)?;

    let _lock = if opts.dry_run {
        None
    } else {
        match GcLock::try_acquire(repo, opts.now_unix)? {
            GcLockAttempt::Acquired(lock) => Some(lock),
            GcLockAttempt::Held(holder) => {
                return Err(OffloadError::MaintenanceRunning { holder }.into())
            }
        }
    };

    let main_bytes_before = local_object_bytes(odb)?;
    let selection = select_candidates(repo, filter)?;
    let candidates: Vec<ObjectId> = selection.candidates.keys().copied().collect();
    let in_lop = held_in_lop_keep_packs(lop.odb(), &candidates)?;
    let missing_in_lop: Vec<ObjectId> = candidates
        .iter()
        .zip(&in_lop)
        .filter_map(|(oid, has)| (!has).then_some(*oid))
        .collect();

    let mut report = OffloadReport {
        remote: remote.to_owned(),
        lop_git_dir: lop.git_dir().to_path_buf(),
        filter,
        dry_run: opts.dry_run,
        candidates: candidates.len(),
        candidate_bytes: selection.candidates.values().sum(),
        copied_to_lop: 0,
        already_in_lop: candidates.len() - missing_in_lop.len(),
        dropped_from_main: 0,
        skipped_checked_out: selection.skipped_checked_out,
        skipped_kept_pack: selection.skipped_kept_pack,
        lop_pack: None,
        main_pack: None,
        main_bytes_before,
        main_bytes_after: main_bytes_before,
        config_changes: Vec::new(),
        lop_ready: LopReadiness::from_config(&lop_config(&lop)?),
        advertised: config
            .get_bool("promisor.advertise")
            .and_then(|v| v.ok())
            .unwrap_or(false),
    };
    if opts.dry_run {
        return Ok(report);
    }

    if !missing_in_lop.is_empty() {
        report.lop_pack = copy_to_lop(repo, &lop, &missing_in_lop, opts)?;
        report.copied_to_lop = missing_in_lop.len();
    }
    lop.odb().invalidate_packs();
    let present = held_in_lop_keep_packs(lop.odb(), &candidates)?;
    if let Some((oid, _)) = candidates.iter().zip(&present).find(|(_, has)| !**has) {
        return Err(OffloadError::NotInLopAfterCopy { oid: *oid }.into());
    }
    if let Some(pack) = &report.lop_pack {
        // The main repository is about to drop its copies: the LOP's must be on disk.
        crate::index_pack::sync_installed_pack(pack)?;
    }

    let mut wanted: Vec<(String, String)> = Vec::new();
    let promisor_key = format!("remote.{remote}.promisor");
    if config.get_bool(&promisor_key).and_then(|v| v.ok()) != Some(true) {
        wanted.push((promisor_key, "true".to_owned()));
    }
    let filter_key = format!("remote.{remote}.partialCloneFilter");
    if opts.filter.is_some() && config.get(&filter_key).is_none() {
        wanted.push((filter_key, filter.to_config_value()));
    }
    // No pack or multi-pack-index holds the offloaded blobs, so no bitmap can have
    // full closure; once a push lands outside the promisor pack, Git's `gc` fails
    // trying (bitmaps are on by default in bare repositories).
    let bitmaps_key = "repack.writeBitmaps";
    if config.get_bool(bitmaps_key).and_then(|v| v.ok()) != Some(false) {
        wanted.push((bitmaps_key.to_owned(), "false".to_owned()));
    }
    write_config(repo.git_dir(), &wanted)?;
    report
        .config_changes
        .extend(wanted.into_iter().map(|(key, value)| OffloadConfigChange {
            repo: OffloadRepo::Main,
            key,
            value,
        }));
    if opts.configure_lop {
        let lop_changes: Vec<(String, String)> = [
            ("uploadpack.allowFilter", report.lop_ready.allow_filter),
            (
                "uploadpack.allowAnySHA1InWant",
                report.lop_ready.allow_any_sha1_in_want,
            ),
        ]
        .into_iter()
        .filter(|(_, already)| !already)
        .map(|(key, _)| (key.to_owned(), "true".to_owned()))
        .collect();
        write_config(lop.git_dir(), &lop_changes)?;
        report
            .config_changes
            .extend(
                lop_changes
                    .into_iter()
                    .map(|(key, value)| OffloadConfigChange {
                        repo: OffloadRepo::Lop,
                        key,
                        value,
                    }),
            );
        report.lop_ready = LopReadiness::from_config(&lop_config(&lop)?);
    }

    let omit: OidHashSet = candidates.iter().copied().collect();
    let repack = RepackOptions {
        threads: opts.threads,
        environment: repo.environment().clone(),
        ..RepackOptions::default()
    };
    // Fold in what arrived since the last run even when nothing moves now: a
    // pushed tree naming an offloaded blob outside the promisor pack makes every
    // promise check for that blob scan all promisor packs.
    let rewrite = crate::maintenance::rewrite_local_objects_without(
        repo,
        &omit,
        &repack,
        crate::maintenance::PromisorRewriteWhen::Always,
    )?;
    report.main_pack = rewrite.pack_path;
    report.dropped_from_main = odb.missing_stored_oids(candidates.iter().copied())?.len();
    report.main_bytes_after = local_object_bytes(odb)?;
    Ok(report)
}

/// Open the LOP named by `opts` (or by the remote's local `url`) and check it can
/// take the blobs.
fn open_lop(repo: &Repository, opts: &OffloadOptions, url: &str) -> Result<Repository> {
    let path = match &opts.lop_git_dir {
        Some(path) => path.clone(),
        None if crate::transport_path::is_local_path_remote_url(url) => {
            crate::transport_path::resolve_local_remote_git_dir(
                url,
                repo.git_dir(),
                repo.work_tree(),
            )
        }
        None => {
            return Err(OffloadError::LopNotLocal {
                remote: opts.remote.clone(),
                url: crate::transport_path::redact_url_for_display(url),
            }
            .into())
        }
    };
    let lop = Repository::open(
        &path,
        &RepositoryOptions::with_environment(repo.environment().clone()),
    )
    .map_err(|_| OffloadError::LopNotARepository { path: path.clone() })?;
    use crate::git_path::CanonicalizeExt as _;
    let same = match (
        repo.git_dir().canonicalize_simplified(),
        lop.git_dir().canonicalize_simplified(),
    ) {
        (Ok(main), Ok(other)) => main == other,
        _ => repo.git_dir() == lop.git_dir(),
    };
    if same {
        return Err(OffloadError::LopIsMainRepository.into());
    }
    let (main_algo, lop_algo) = (repo.odb().hash_algo(), lop.odb().hash_algo());
    if main_algo != lop_algo {
        return Err(OffloadError::HashAlgoMismatch {
            main: main_algo,
            lop: lop_algo,
        }
        .into());
    }
    lop.odb().require_files_primary("offload")?;
    Ok(lop)
}

/// The LOP's own config (system and global included, as its upload-pack sees it).
fn lop_config(lop: &Repository) -> Result<ConfigSet> {
    ConfigSet::load(lop.environment(), Some(lop.git_dir()), true)
}

/// Blobs chosen by [`select_candidates`], with their sizes.
struct Selection {
    candidates: OidHashMap<u64>,
    skipped_checked_out: usize,
    skipped_kept_pack: usize,
}

/// Walk every commit, tag and tree reachable from the maintenance roots and pick the
/// blobs `filter` leaves out that this repository stores and can drop.
///
/// A blob qualifies only when a tree naming it will be in the rewritten promisor
/// pack (stored loose or in a pack without `.keep`): that is what makes it promised
/// once it is gone. Nothing is fetched; unreadable objects are skipped.
fn select_candidates(repo: &Repository, filter: ObjectFilterSpec) -> Result<Selection> {
    let odb = repo.odb();
    let pack_dir = odb.objects_dir().join("pack");
    let (kept, rewritten): (Vec<_>, Vec<_>) = odb
        .local_pack_indexes()?
        .into_iter()
        .partition(|idx| pack_dir.join(keep_name(&idx.pack_path)).is_file());
    let in_any = |indexes: &[std::sync::Arc<PackIndex>], oid: &ObjectId| {
        indexes.iter().any(|idx| idx.find_position(oid).is_some())
    };
    let will_be_rewritten =
        |oid: &ObjectId| in_any(&rewritten, oid) || odb.local_loose_file_size(oid).is_some();
    let checked_out = crate::promisor::blobs_needed_by_checkouts(repo)?;

    let mut selection = Selection {
        candidates: crate::objects::oid_hash_map(),
        skipped_checked_out: 0,
        skipped_kept_pack: 0,
    };
    // Selected blobs no rewritten tree names yet; settled when one does.
    let mut unanchored: OidHashMap<u64> = crate::objects::oid_hash_map();
    let mut blobs_seen = oid_hash_set();
    let mut walked = oid_hash_set();
    let mut queue: Vec<ObjectId> = crate::maintenance::collect_maintenance_object_roots(repo)?;
    let oid_len = odb.hash_algo().len();
    while let Some(oid) = queue.pop() {
        if !walked.insert(oid) {
            continue;
        }
        let Some(obj) = odb.try_read_once(&oid)? else {
            continue;
        };
        match obj.kind {
            ObjectKind::Commit => {
                let commit = CommitRef::parse(&obj.data)?;
                queue.push(commit.tree());
                queue.extend(commit.parents().iter().copied());
            }
            ObjectKind::Tag => {
                if let Some(target) = crate::objects::tag_object_line_oid(&obj.data) {
                    queue.push(target);
                }
            }
            ObjectKind::Tree => {
                let anchors = will_be_rewritten(&oid);
                for entry in TreeRefIter::with_oid_len(&obj.data, oid_len) {
                    let entry = entry?;
                    match entry.mode & 0o170000 {
                        0o100000 | 0o120000 => {}
                        0o040000 => {
                            queue.push(entry.oid);
                            continue;
                        }
                        // Submodule commits are not ours to move.
                        _ => continue,
                    }
                    if anchors {
                        if let Some(size) = unanchored.remove(&entry.oid) {
                            selection.candidates.insert(entry.oid, size);
                        }
                    }
                    if !blobs_seen.insert(entry.oid) {
                        continue;
                    }
                    let Some(size) = selected_blob_size(odb, filter, &entry.oid)? else {
                        continue;
                    };
                    if checked_out.contains(&entry.oid) {
                        selection.skipped_checked_out += 1;
                    } else if in_any(&kept, &entry.oid) {
                        selection.skipped_kept_pack += 1;
                    } else if anchors {
                        selection.candidates.insert(entry.oid, size);
                    } else {
                        unanchored.insert(entry.oid, size);
                    }
                }
            }
            ObjectKind::Blob => {}
        }
    }
    selection.skipped_kept_pack += unanchored.len();
    Ok(selection)
}

/// The size of `oid` when it is a blob stored locally that `filter` leaves out.
fn selected_blob_size(odb: &Odb, filter: ObjectFilterSpec, oid: &ObjectId) -> Result<Option<u64>> {
    if !odb.try_exists_stored(oid)? {
        return Ok(None);
    }
    let info = match odb.read_info_without_fetch(oid) {
        Ok(info) => info,
        Err(Error::ObjectNotFound(_)) => return Ok(None),
        Err(err) => return Err(err),
    };
    if info.kind != ObjectKind::Blob {
        return Ok(None);
    }
    let selected = match filter {
        ObjectFilterSpec::BlobNone => true,
        ObjectFilterSpec::BlobLimit(limit) => info.size >= limit,
    };
    Ok(selected.then_some(info.size))
}

fn keep_name(pack_path: &Path) -> PathBuf {
    PathBuf::from(pack_path.file_name().unwrap_or_default()).with_extension("keep")
}

/// Copy `blobs` from `repo` into one new `.keep` pack in `lop`.
fn copy_to_lop(
    repo: &Repository,
    lop: &Repository,
    blobs: &[ObjectId],
    opts: &OffloadOptions,
) -> Result<Option<PathBuf>> {
    let cfg = repo.config().ok();
    let pack_opts = PackBuildOptions {
        delta: false,
        window: 0,
        max_depth: 0,
        reuse_deltas: false,
        reuse_objects: true,
        use_bitmaps: false,
        thin: false,
        ..PackBuildOptions::for_local_copy(cfg.as_deref())
    };
    let lop_pack_dir = lop.odb().objects_dir().join("pack");
    let built = crate::maintenance::write_staged_repack_pack(
        repo.odb(),
        &lop_pack_dir,
        blobs,
        &std::collections::HashMap::new(),
        &pack_opts,
        opts.threads,
        cfg.as_deref(),
    )?;
    let Some(staging) = built.staging_path.as_ref() else {
        return Ok(None);
    };
    let trusted_rows = (!built.index_rows.is_empty()).then_some(built.index_rows);
    let ingested = install_pack_path(
        staging,
        lop.odb(),
        &IngestPackOptions {
            threads: opts.threads,
            environment: lop.environment().clone(),
            skip_post_index_verify: trusted_rows.is_some(),
            trusted_index_rows: trusted_rows,
            keep_message: Some(LOP_KEEP_MESSAGE.to_owned()),
            ..IngestPackOptions::default()
        },
    )?;
    Ok(ingested.pack_path)
}

/// For each of `oids`, whether the LOP itself holds it in a pack with a `.keep`
/// marker.
///
/// Only such a copy is safe to rely on before the main repository drops its own:
/// a loose object or one in an unkept pack is unreachable in the LOP and its next
/// `gc` may prune it, and a copy found through the LOP's alternates (possibly the
/// main repository itself) is not the LOP's at all.
fn held_in_lop_keep_packs(lop: &Odb, oids: &[ObjectId]) -> Result<Vec<bool>> {
    let pack_dir = lop.objects_dir().join("pack");
    let kept: Vec<_> = lop
        .local_pack_indexes()?
        .into_iter()
        .filter(|idx| pack_dir.join(keep_name(&idx.pack_path)).is_file())
        .collect();
    Ok(oids
        .iter()
        .map(|oid| kept.iter().any(|idx| idx.find_position(oid).is_some()))
        .collect())
}

/// Set `changes` in the local config of the repository at `git_dir`.
fn write_config(git_dir: &Path, changes: &[(String, String)]) -> Result<()> {
    if changes.is_empty() {
        return Ok(());
    }
    let path = git_dir.join("config");
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(Error::Io(err)),
    };
    let mut config = ConfigFile::parse(&path, &content, ConfigScope::Local)?;
    for (key, value) in changes {
        config.set(key, value)?;
    }
    config.write()
}

/// Bytes of the repository's own packs (with indexes) and loose objects.
fn local_object_bytes(odb: &Odb) -> Result<u64> {
    odb.invalidate_packs();
    let mut total = 0u64;
    for idx in odb.local_pack_indexes()? {
        for path in [&idx.pack_path, &idx.idx_path] {
            total += std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        }
    }
    for (_, path) in odb.enumerate_local_loose_objects()? {
        total += std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    }
    Ok(total)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::repo::InitOptions;

    const NOW: i64 = 1_700_000_000;

    /// A bare main repository whose `main` commit holds one blob per entry of
    /// `sizes` (file `f<i>`), a bare LOP next to it, and `remote.lop` in main.
    struct Fixture {
        dir: tempfile::TempDir,
        blobs: Vec<ObjectId>,
    }

    /// `size` bytes that do not compress, different for each `seed`.
    fn blob_data(seed: usize, size: usize) -> Vec<u8> {
        let mut state = (seed as u64 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        (0..size)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state.to_le_bytes()[0]
            })
            .collect()
    }

    impl Fixture {
        fn new(sizes: &[usize]) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let init = InitOptions::new()
                .with_bare(true)
                .with_initial_branch("main");
            let main = Repository::init(&dir.path().join("main.git"), &init).unwrap();
            Repository::init(&dir.path().join("lop.git"), &init).unwrap();
            let odb = main.odb();
            let blobs: Vec<ObjectId> = sizes
                .iter()
                .enumerate()
                .map(|(i, &size)| odb.write(ObjectKind::Blob, &blob_data(i, size)).unwrap())
                .collect();
            let mut entries: Vec<crate::objects::TreeEntry> = blobs
                .iter()
                .enumerate()
                .map(|(i, oid)| crate::objects::TreeEntry {
                    mode: 0o100644,
                    name: format!("f{i}").into_bytes(),
                    oid: *oid,
                })
                .collect();
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            let tree = odb
                .write(ObjectKind::Tree, &crate::objects::serialize_tree(&entries))
                .unwrap();
            let commit =
                format!("tree {tree}\nauthor A <a@e> 1 +0000\ncommitter A <a@e> 1 +0000\n\nseed\n");
            let commit = odb.write(ObjectKind::Commit, commit.as_bytes()).unwrap();
            std::fs::write(
                dir.path().join("main.git/refs/heads/main"),
                format!("{commit}\n"),
            )
            .unwrap();
            let fx = Self { dir, blobs };
            fx.append_main_config("[remote \"lop\"]\n\turl = ../lop.git\n");
            fx
        }

        fn main_dir(&self) -> PathBuf {
            self.dir.path().join("main.git")
        }

        fn lop_dir(&self) -> PathBuf {
            self.dir.path().join("lop.git")
        }

        fn main(&self) -> Repository {
            Repository::open(&self.main_dir(), &RepositoryOptions::empty()).unwrap()
        }

        fn lop(&self) -> Repository {
            Repository::open(&self.lop_dir(), &RepositoryOptions::empty()).unwrap()
        }

        fn append_main_config(&self, text: &str) {
            let path = self.main_dir().join("config");
            let mut config = std::fs::read_to_string(&path).unwrap();
            config.push_str(text);
            std::fs::write(path, config).unwrap();
        }

        fn offload(&self, limit: u64) -> OffloadReport {
            let mut opts = OffloadOptions::new("lop", NOW);
            opts.filter = Some(ObjectFilterSpec::BlobLimit(limit));
            offload_blobs(&self.main(), &opts).expect("offload")
        }

        fn files(&self, dir: &Path, ext: &str) -> Vec<PathBuf> {
            std::fs::read_dir(dir.join("objects/pack"))
                .map(|entries| {
                    entries
                        .flatten()
                        .map(|e| e.path())
                        .filter(|p| p.extension().is_some_and(|e| e == ext))
                        .collect()
                })
                .unwrap_or_default()
        }
    }

    #[test]
    fn blobs_at_the_limit_move_and_smaller_ones_stay() {
        let fx = Fixture::new(&[10_000, 9_999, 30_000]);
        let report = fx.offload(10_000);
        assert_eq!(report.candidates, 2);
        assert_eq!(report.candidate_bytes, 40_000);
        assert_eq!(report.copied_to_lop, 2);
        assert_eq!(report.dropped_from_main, 2);
        assert_eq!(report.filter, ObjectFilterSpec::BlobLimit(10_000));

        let main = fx.main();
        let lop = fx.lop();
        for (i, moved) in [(0, true), (1, false), (2, true)] {
            let blob = fx.blobs[i];
            assert_eq!(!main.odb().exists_stored(&blob), moved, "main f{i}");
            assert_eq!(lop.odb().exists_stored(&blob), moved, "lop f{i}");
        }
        // The rest of main lives in one promisor pack; the loose objects are gone.
        let promisor = fx.files(&fx.main_dir(), "promisor");
        assert_eq!(promisor.len(), 1);
        assert_eq!(
            report.main_pack.as_deref().and_then(Path::file_name),
            promisor[0].with_extension("pack").file_name()
        );
        assert!(main
            .odb()
            .enumerate_local_loose_objects()
            .unwrap()
            .is_empty());
        assert!(report.main_bytes_after < report.main_bytes_before);
    }

    #[test]
    fn lop_pack_is_kept_and_complete() {
        let fx = Fixture::new(&[4096]);
        let report = fx.offload(1024);
        let lop_pack = report.lop_pack.expect("lop pack");
        assert_eq!(
            std::fs::read_to_string(lop_pack.with_extension("keep")).unwrap(),
            format!("{LOP_KEEP_MESSAGE}\n")
        );
        let blob = fx.lop().odb().read(&fx.blobs[0]).unwrap();
        assert_eq!(blob.data, blob_data(0, 4096));
    }

    #[test]
    fn config_is_set_in_main_and_optionally_in_the_lop() {
        let fx = Fixture::new(&[2048]);
        let mut opts = OffloadOptions::new("lop", NOW);
        opts.filter = Some(ObjectFilterSpec::BlobLimit(1024));
        opts.configure_lop = true;
        let report = offload_blobs(&fx.main(), &opts).unwrap();
        let keys: Vec<(OffloadRepo, &str, &str)> = report
            .config_changes
            .iter()
            .map(|c| (c.repo, c.key.as_str(), c.value.as_str()))
            .collect();
        assert_eq!(
            keys,
            [
                (OffloadRepo::Main, "remote.lop.promisor", "true"),
                (
                    OffloadRepo::Main,
                    "remote.lop.partialCloneFilter",
                    "blob:limit=1024"
                ),
                (OffloadRepo::Main, "repack.writeBitmaps", "false"),
                (OffloadRepo::Lop, "uploadpack.allowFilter", "true"),
                (OffloadRepo::Lop, "uploadpack.allowAnySHA1InWant", "true"),
            ]
        );
        assert!(report.lop_ready.is_ready());
        assert!(!report.advertised);
        let config = fx.main().config().unwrap();
        assert_eq!(config.get("remote.lop.promisor").as_deref(), Some("true"));
        assert!(config.get("extensions.partialclone").is_none());
        assert_eq!(
            crate::partial_clone::stored_remote_filter(&config, "lop"),
            Some(ObjectFilterSpec::BlobLimit(1024))
        );
    }

    #[test]
    fn stored_filter_is_used_and_lop_is_left_alone_by_default() {
        let fx = Fixture::new(&[2048, 10]);
        fx.append_main_config("\tpartialCloneFilter = blob:limit=1k\n");
        let report = offload_blobs(&fx.main(), &OffloadOptions::new("lop", NOW)).unwrap();
        assert_eq!(report.candidates, 1);
        assert_eq!(report.filter, ObjectFilterSpec::BlobLimit(1024));
        let keys: Vec<&str> = report
            .config_changes
            .iter()
            .map(|c| c.key.as_str())
            .collect();
        assert_eq!(keys, ["remote.lop.promisor", "repack.writeBitmaps"]);
        assert_eq!(report.lop_ready, LopReadiness::default());
    }

    /// Every file under `dir` with its contents, sorted by path.
    fn tree_snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(path) = stack.pop() {
            if path.is_dir() {
                stack.extend(
                    std::fs::read_dir(&path)
                        .unwrap()
                        .flatten()
                        .map(|e| e.path()),
                );
            } else {
                out.push((path.clone(), std::fs::read(&path).unwrap()));
            }
        }
        out.sort();
        out
    }

    #[test]
    fn dry_run_writes_nothing() {
        let fx = Fixture::new(&[2048]);
        let before = tree_snapshot(&fx.main_dir());
        let mut opts = OffloadOptions::new("lop", NOW);
        opts.filter = Some(ObjectFilterSpec::BlobNone);
        opts.dry_run = true;
        opts.configure_lop = true;
        let report = offload_blobs(&fx.main(), &opts).unwrap();
        assert!(report.dry_run);
        assert_eq!(report.candidates, 1);
        assert_eq!(report.copied_to_lop, 0);
        assert!(report.config_changes.is_empty());
        assert_eq!(tree_snapshot(&fx.main_dir()), before);
        assert!(fx.files(&fx.lop_dir(), "pack").is_empty());
        assert!(!fx.main_dir().join("gc.pid").exists());
    }

    #[test]
    fn running_again_is_a_no_op_until_new_blobs_arrive() {
        let fx = Fixture::new(&[2048, 10]);
        fx.offload(1024);
        let again = fx.offload(1024);
        assert_eq!(again.candidates, 0);
        assert_eq!(again.copied_to_lop, 0);
        assert!(again.main_pack.is_none(), "nothing to rewrite");
        assert_eq!(fx.files(&fx.lop_dir(), "pack").len(), 1);
        assert_eq!(fx.files(&fx.main_dir(), "pack").len(), 1);
    }

    #[test]
    fn blobs_the_lop_keeps_are_not_copied_again() {
        let fx = Fixture::new(&[2048]);
        let lop = fx.lop();
        let pack = crate::pack_objects::build_pack_from_send_list(
            fx.main().odb(),
            &[fx.blobs[0]],
            &std::collections::HashSet::new(),
            &PackBuildOptions::default(),
            &std::collections::HashMap::new(),
        )
        .unwrap();
        crate::index_pack::install_pack_bytes(
            pack,
            lop.odb(),
            &IngestPackOptions {
                keep_message: Some("pinned".into()),
                ..IngestPackOptions::default()
            },
        )
        .unwrap();
        let report = fx.offload(1024);
        assert_eq!(report.already_in_lop, 1);
        assert_eq!(report.copied_to_lop, 0);
        assert!(report.lop_pack.is_none());
        assert_eq!(report.dropped_from_main, 1);
    }

    /// Remove the LOP's loose objects, as its `gc --prune=now` would for blobs
    /// nothing in it references.
    fn prune_lop_loose(fx: &Fixture) {
        for (_, path) in fx.lop().odb().enumerate_local_loose_objects().unwrap() {
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn a_loose_copy_in_the_lop_is_not_trusted() {
        let fx = Fixture::new(&[2048]);
        fx.lop()
            .odb()
            .write(ObjectKind::Blob, &blob_data(0, 2048))
            .unwrap();
        let report = fx.offload(1024);
        assert_eq!(report.already_in_lop, 0);
        assert_eq!(report.copied_to_lop, 1);
        assert!(report.lop_pack.is_some());
        assert_eq!(report.dropped_from_main, 1);
        prune_lop_loose(&fx);
        let lop = fx.lop();
        assert_eq!(
            lop.odb().read(&fx.blobs[0]).unwrap().data,
            blob_data(0, 2048),
            "the kept pack survives the LOP's prune"
        );
    }

    #[test]
    fn a_copy_the_lop_borrows_from_main_is_not_trusted() {
        let fx = Fixture::new(&[2048]);
        let alternates = fx.lop_dir().join("objects/info/alternates");
        std::fs::create_dir_all(alternates.parent().unwrap()).unwrap();
        std::fs::write(
            &alternates,
            format!("{}\n", fx.main_dir().join("objects").display()),
        )
        .unwrap();
        let report = fx.offload(1024);
        assert_eq!(report.already_in_lop, 0);
        assert_eq!(report.copied_to_lop, 1);
        std::fs::remove_file(&alternates).unwrap();
        assert_eq!(
            fx.lop().odb().read(&fx.blobs[0]).unwrap().data,
            blob_data(0, 2048),
            "the LOP has its own copy"
        );
    }

    #[test]
    fn running_again_folds_pushed_objects_into_the_promisor_pack() {
        let fx = Fixture::new(&[2048, 10]);
        fx.offload(1024);
        // A push since: a loose tree that still names the offloaded blob.
        let main = fx.main();
        let mut body = b"100644 big\0".to_vec();
        body.extend_from_slice(fx.blobs[0].as_bytes());
        let tree = main
            .odb()
            .write_loose_materialize(ObjectKind::Tree, &body)
            .unwrap();
        let again = fx.offload(1024);
        assert_eq!(again.candidates, 0);
        let pack = again.main_pack.expect("folded");
        let promisor: Vec<_> = fx
            .files(&fx.main_dir(), "promisor")
            .iter()
            .map(|p| p.file_name().map(|n| n.to_os_string()))
            .collect();
        assert_eq!(
            promisor,
            [pack
                .with_extension("promisor")
                .file_name()
                .map(|n| n.to_os_string())]
        );
        let main = fx.main();
        assert!(main
            .odb()
            .enumerate_local_loose_objects()
            .unwrap()
            .is_empty());
        assert!(main.odb().exists_stored(&tree));
        assert!(!main.odb().exists_stored(&fx.blobs[0]));
    }

    #[test]
    fn blobs_in_kept_packs_stay() {
        let fx = Fixture::new(&[2048]);
        let main = fx.main();
        let pack = crate::pack_objects::build_pack_from_send_list(
            main.odb(),
            &[fx.blobs[0]],
            &std::collections::HashSet::new(),
            &PackBuildOptions::default(),
            &std::collections::HashMap::new(),
        )
        .unwrap();
        crate::index_pack::install_pack_bytes(
            pack,
            main.odb(),
            &IngestPackOptions {
                keep_message: Some("pinned".into()),
                ..IngestPackOptions::default()
            },
        )
        .unwrap();
        let report = fx.offload(1024);
        assert_eq!(report.candidates, 0);
        assert_eq!(report.skipped_kept_pack, 1);
        assert!(fx.main().odb().exists_stored(&fx.blobs[0]));
    }

    #[test]
    fn blobs_a_checkout_needs_stay() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        let repo =
            Repository::init(&work, &InitOptions::new().with_initial_branch("main")).unwrap();
        Repository::init(
            &dir.path().join("lop.git"),
            &InitOptions::new().with_bare(true),
        )
        .unwrap();
        std::fs::write(work.join("big"), vec![b'x'; 4096]).unwrap();
        crate::porcelain::add::stage_for_commit(
            &repo,
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let ident = "T <t@example.com> 1 +0000".to_string();
        crate::porcelain::commit::create_commit(
            &repo,
            &crate::porcelain::commit::CommitRequest {
                message: "big".into(),
                author: Some(ident.parse().unwrap()),
                committer: Some(ident.parse().unwrap()),
                ..Default::default()
            },
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .unwrap();
        let config = repo.git_dir().join("config");
        let mut text = std::fs::read_to_string(&config).unwrap();
        text.push_str(&format!(
            "[remote \"lop\"]\n\turl = {}\n",
            dir.path().join("lop.git").display()
        ));
        std::fs::write(&config, text).unwrap();
        let repo = Repository::discover(Some(&work), &RepositoryOptions::empty()).unwrap();
        let mut opts = OffloadOptions::new("lop", NOW);
        opts.filter = Some(ObjectFilterSpec::BlobLimit(1024));
        let report = offload_blobs(&repo, &opts).unwrap();
        assert_eq!(report.candidates, 0);
        assert_eq!(report.skipped_checked_out, 1);
    }

    fn offload_err(fx: &Fixture, opts: &OffloadOptions) -> OffloadError {
        match offload_blobs(&fx.main(), opts) {
            Err(Error::Offload(err)) => err,
            other => panic!("expected an offload error, got {other:?}"),
        }
    }

    #[test]
    fn refusals_are_typed() {
        let fx = Fixture::new(&[2048]);
        let with_filter = |remote: &str| {
            let mut opts = OffloadOptions::new(remote, NOW);
            opts.filter = Some(ObjectFilterSpec::BlobNone);
            opts
        };
        assert!(matches!(
            offload_err(&fx, &with_filter("nope")),
            OffloadError::RemoteNotConfigured { remote } if remote == "nope"
        ));
        assert!(matches!(
            offload_err(&fx, &OffloadOptions::new("lop", NOW)),
            OffloadError::NoThreshold { .. }
        ));

        fx.append_main_config(
            "[remote \"web\"]\n\turl = https://user:secret@lop.example/r.git\n\
             [remote \"self\"]\n\turl = .\n\
             [remote \"gone\"]\n\turl = ../nothing-here.git\n",
        );
        match offload_err(&fx, &with_filter("web")) {
            OffloadError::LopNotLocal { url, .. } => assert!(!url.contains("secret")),
            other => panic!("got {other:?}"),
        }
        assert!(matches!(
            offload_err(&fx, &with_filter("self")),
            OffloadError::LopIsMainRepository
        ));
        assert!(matches!(
            offload_err(&fx, &with_filter("gone")),
            OffloadError::LopNotARepository { .. }
        ));

        let sha256 = fx.dir.path().join("sha256.git");
        Repository::init(
            &sha256,
            &InitOptions::new()
                .with_bare(true)
                .with_object_format(HashAlgo::Sha256),
        )
        .unwrap();
        let mut opts = with_filter("web");
        opts.lop_git_dir = Some(sha256);
        assert!(matches!(
            offload_err(&fx, &opts),
            OffloadError::HashAlgoMismatch {
                main: HashAlgo::Sha1,
                lop: HashAlgo::Sha256
            }
        ));

        std::fs::write(fx.main_dir().join("gc.pid"), "1 some-other-host").unwrap();
        assert!(matches!(
            offload_err(&fx, &with_filter("lop")),
            OffloadError::MaintenanceRunning { .. }
        ));
    }
}
