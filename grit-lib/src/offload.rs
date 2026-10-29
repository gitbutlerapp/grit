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
//! The blobs that stay (smaller than the threshold, or needed by a checkout) are in
//! that promisor pack too, although only the main repository has them. Git treats
//! every object a tree in a promisor pack names as promised, and its `repack -a -d`
//! and `gc` keep only the promised objects they find in promisor packs: a kept blob
//! in an ordinary pack beside it would be deleted, with nothing to fetch it back
//! from. The price is that a blob in a promisor pack no longer proves the promisor
//! can supply it, so offload records the LOP's directory in `grit.lopGitDir`, and
//! [`crate::maintenance::drop_promised_blobs`] drops a blob only once a promisor
//! it can read is seen to hold it.
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

/// Multi-valued config key [`offload_blobs`] adds to the main repository: the
/// absolute git directory of each LOP it moved blobs to.
///
/// Its presence marks a repository whose promisor pack holds blobs only it has, so
/// [`crate::maintenance::drop_promised_blobs`] drops a blob there only after seeing
/// it in a promisor it can read (one of these directories, or a promisor remote
/// with a local URL).
pub const LOP_GIT_DIR_KEY: &str = "grit.lopGitDir";

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
    /// The LOP is the main repository itself, or stores its objects in the same
    /// directory (a linked worktree of it, or an `objects/` linked to its).
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
    /// An ordinary `.keep` pack (one without `.promisor`) of the main repository
    /// holds the only copy of an object that a commit, tree or tag going into the
    /// promisor pack names. Git counts every object a promisor-pack object names as
    /// promised, and its `repack -a -d` keeps a promised object only in a promisor
    /// pack: once the marker goes, it would delete this one, which no promisor has.
    /// Offload does not rewrite kept packs, and a copy in the promisor pack does
    /// not help (Git's repack leaves out objects a kept pack holds), so it refuses
    /// before changing anything. Removing the marker lets offload fold the pack
    /// in; marking the pack `.promisor` keeps it as it is.
    #[error(
        "{oid} is only in kept pack {}, and {referrer} names it",
        pack.display()
    )]
    KeptPackObjectPromised {
        /// The kept pack holding the object.
        pack: PathBuf,
        /// The object.
        oid: ObjectId,
        /// An object going into the promisor pack that names it.
        referrer: ObjectId,
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
    /// Candidates the LOP already had in a pack an earlier offload wrote there (its
    /// `.keep` marker holds [`LOP_KEEP_MESSAGE`]). A loose or unkept copy there
    /// does not count (the LOP's `gc` may prune it), nor does one in a pack another
    /// writer keeps for a while (`receive-pack`, `fetch-pack`), nor one it reaches
    /// through alternates; those candidates are copied.
    pub already_in_lop: usize,
    /// Candidates no longer stored in the main repository afterwards.
    pub dropped_from_main: usize,
    /// Selected blobs kept because a checkout of the main repository needs them
    /// (a worktree's index or `HEAD`).
    pub skipped_checked_out: usize,
    /// Selected blobs kept because they are in a `.keep` pack (or the only tree
    /// naming them is), which offload does not rewrite.
    pub skipped_kept_pack: usize,
    /// The pack added to the LOP, holding the blobs copied there, if any.
    pub lop_pack: Option<OffloadPack>,
    /// The promisor pack the main repository's objects now live in (its commits,
    /// trees, tags and the blobs it keeps), if rewritten.
    pub main_pack: Option<OffloadPack>,
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

impl OffloadReport {
    /// The packs the run wrote, in the order they were written: the LOP pack, then
    /// the main repository's promisor pack.
    pub fn packs(&self) -> impl Iterator<Item = &OffloadPack> {
        [&self.lop_pack, &self.main_pack].into_iter().flatten()
    }
}

/// Which of the packs [`offload_blobs`] writes an [`OffloadPack`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OffloadPackRole {
    /// The LOP's new pack of the blobs that moved, with a `.keep` marker.
    Lop,
    /// The main repository's `.promisor` pack of every object it keeps.
    MainPromisor,
}

/// A pack [`offload_blobs`] wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OffloadPack {
    /// Which pack this is.
    pub role: OffloadPackRole,
    /// The `.pack` file (its `.idx` and other sidecars are beside it).
    pub path: PathBuf,
    /// Objects in the pack.
    pub objects: usize,
    /// Size of the `.pack` file in bytes.
    pub bytes: u64,
}

impl OffloadPack {
    /// Describe the pack at `path`, reading its size from disk.
    fn written(role: OffloadPackRole, path: PathBuf, objects: usize) -> Result<Self> {
        let bytes = std::fs::metadata(&path)?.len();
        Ok(Self {
            role,
            path,
            objects,
            bytes,
        })
    }
}

/// Move the blobs [`OffloadOptions::filter`] selects from `repo` to the large-object
/// promisor [`OffloadOptions::remote`].
///
/// The steps run in an order that never loses an object:
///
/// 1. Blobs reachable from the refs, reflogs and `HEAD`s, stored in `repo`, and left
///    out by the filter are the candidates. Those a checkout needs, and those in
///    `.keep` packs, stay.
/// 2. The candidates the LOP does not already hold in a pack an earlier offload
///    wrote there are copied into one new pack there, whole (no deltas), with a
///    `.keep` marker holding [`LOP_KEEP_MESSAGE`] written before the pack appears:
///    nothing in the LOP refers to them, so an unprotected pack could be pruned.
///    A pack another writer keeps only for a while (`receive-pack`, `fetch-pack`)
///    does not count. Every candidate is then checked to be in such a pack of the
///    LOP, and the new pack, index and marker are flushed to disk.
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
///    It also gets the LOP's git directory in `grit.lopGitDir` (once per LOP),
///    which tells `drop-large-blobs` where to check that a blob it would drop
///    exists in the LOP.
/// 4. The main repository's local packs and loose objects are rewritten into one
///    `.promisor` pack without the candidates; its trees keep the offloaded blobs
///    promised. The blobs that stay go into it too: Git's repack and `gc` keep a
///    promised object only when it is in a promisor pack (see the module
///    documentation). This happens on every run, even when no blob moves, so
///    objects pushed since the last run (trees naming offloaded blobs, new small
///    blobs) join the promisor pack, and a repository whose kept blobs sit in an
///    ordinary pack is folded back. The new pack is flushed to disk before the old
///    packs are removed. `.keep` packs are not rewritten (see
///    [`OffloadError::KeptPackObjectPromised`]).
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
    if let Some(refusal) = kept_object_the_promisor_pack_names(odb, &selection.candidates)? {
        return Err(refusal.into());
    }
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
        report.lop_pack = copy_to_lop(repo, &lop, &missing_in_lop, opts)?
            .map(|(path, objects)| OffloadPack::written(OffloadPackRole::Lop, path, objects))
            .transpose()?;
        report.copied_to_lop = missing_in_lop.len();
    }
    lop.odb().invalidate_packs();
    let present = held_in_lop_keep_packs(lop.odb(), &candidates)?;
    if let Some((oid, _)) = candidates.iter().zip(&present).find(|(_, has)| !**has) {
        return Err(OffloadError::NotInLopAfterCopy { oid: *oid }.into());
    }
    if let Some(pack) = &report.lop_pack {
        // The main repository is about to drop its copies: the LOP's must be on disk.
        crate::index_pack::sync_installed_pack(&pack.path)?;
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
    // The promisor pack is about to hold blobs the LOP does not have; record where
    // the LOP is, so dropping promised blobs can check it first.
    let mut added: Vec<(String, String)> = Vec::new();
    let lop_dir = lop_dir_value(lop.git_dir());
    if !config
        .get_all(LOP_GIT_DIR_KEY)
        .iter()
        .any(|v| same_dir(Path::new(v), lop.git_dir()))
    {
        added.push((LOP_GIT_DIR_KEY.to_owned(), lop_dir));
    }
    write_config(repo.git_dir(), &wanted, &added)?;
    report
        .config_changes
        .extend(
            wanted
                .into_iter()
                .chain(added)
                .map(|(key, value)| OffloadConfigChange {
                    repo: OffloadRepo::Main,
                    key,
                    value,
                }),
        );
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
        write_config(lop.git_dir(), &lop_changes, &[])?;
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
    report.main_pack = rewrite
        .pack_path
        .map(|path| OffloadPack::written(OffloadPackRole::MainPromisor, path, rewrite.objects))
        .transpose()?;
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
    if same_object_store(repo, &lop) {
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

/// The refusal [`OffloadError::KeptPackObjectPromised`] when an object going into
/// the promisor pack (one of the unkept packs or loose objects, except `omit`)
/// names an object only an ordinary `.keep` pack holds.
///
/// Free without such packs. With them, every commit, tree and tag to be rewritten
/// is read, as the rewrite reads them anyway.
fn kept_object_the_promisor_pack_names(
    odb: &Odb,
    omit: &OidHashMap<u64>,
) -> Result<Option<OffloadError>> {
    odb.reprepare_local_packs();
    let (kept, rewritten): (Vec<_>, Vec<_>) = odb
        .local_pack_indexes()?
        .into_iter()
        .partition(|idx| idx.pack_path.with_extension("keep").is_file());
    let (kept_promisor, kept_ordinary): (Vec<_>, Vec<_>) =
        kept.into_iter().partition(|idx| idx.is_promisor);
    if kept_ordinary.is_empty() {
        return Ok(None);
    }
    let loose: OidHashSet = odb
        .enumerate_local_loose_objects()?
        .into_iter()
        .map(|(oid, _)| oid)
        .collect();
    let in_promisor_pack_after = |oid: &ObjectId| {
        loose.contains(oid)
            || [&rewritten, &kept_promisor]
                .into_iter()
                .flatten()
                .any(|idx| idx.find_position(oid).is_some())
    };
    let mut only_kept: OidHashMap<usize> = crate::objects::oid_hash_map();
    for (pack_no, idx) in kept_ordinary.iter().enumerate() {
        for entry in idx.iter() {
            let oid = ObjectId::from_bytes(entry.oid())?;
            if !in_promisor_pack_after(&oid) {
                only_kept.entry(oid).or_insert(pack_no);
            }
        }
    }
    if only_kept.is_empty() {
        return Ok(None);
    }
    let mut referrers: Vec<ObjectId> = loose.iter().copied().collect();
    for idx in &rewritten {
        for entry in idx.iter() {
            referrers.push(ObjectId::from_bytes(entry.oid())?);
        }
    }
    let oid_len = odb.hash_algo().len();
    let mut checked = oid_hash_set();
    for referrer in referrers {
        if omit.contains_key(&referrer) || !checked.insert(referrer) {
            continue;
        }
        let kind = match odb.read_info_without_fetch(&referrer) {
            Ok(info) => info.kind,
            Err(Error::ObjectNotFound(_)) => continue,
            Err(err) => return Err(err),
        };
        if kind == ObjectKind::Blob {
            continue;
        }
        let obj = odb.read(&referrer)?;
        let mut named: Vec<ObjectId> = Vec::new();
        match kind {
            ObjectKind::Commit => {
                let commit = CommitRef::parse(&obj.data)?;
                named.push(commit.tree());
                named.extend(commit.parents().iter().copied());
            }
            ObjectKind::Tree => {
                for entry in TreeRefIter::with_oid_len(&obj.data, oid_len) {
                    let entry = entry?;
                    if entry.mode & 0o170000 != 0o160000 {
                        named.push(entry.oid);
                    }
                }
            }
            ObjectKind::Tag => named.extend(crate::objects::tag_object_line_oid(&obj.data)),
            ObjectKind::Blob => {}
        }
        if let Some((oid, &pack_no)) = named.iter().find_map(|oid| only_kept.get_key_value(oid)) {
            return Ok(Some(OffloadError::KeptPackObjectPromised {
                pack: kept_ordinary[pack_no].pack_path.clone(),
                oid: *oid,
                referrer,
            }));
        }
    }
    Ok(None)
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

/// Copy `blobs` from `repo` into one new `.keep` pack in `lop`; returns the pack and
/// how many objects it holds.
fn copy_to_lop(
    repo: &Repository,
    lop: &Repository,
    blobs: &[ObjectId],
    opts: &OffloadOptions,
) -> Result<Option<(PathBuf, usize)>> {
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
    let objects = ingested.object_ids.len();
    Ok(ingested.pack_path.map(|path| (path, objects)))
}

/// For each of `oids`, whether the LOP itself holds it in a pack offload wrote
/// there (see [`lop_keep_packs`]).
fn held_in_lop_keep_packs(lop: &Odb, oids: &[ObjectId]) -> Result<Vec<bool>> {
    let kept = lop_keep_packs(lop)?;
    Ok(oids
        .iter()
        .map(|oid| kept.iter().any(|idx| idx.find_position(oid).is_some()))
        .collect())
}

/// The LOP's own packs whose `.keep` marker holds [`LOP_KEEP_MESSAGE`]: the packs
/// offload wrote there.
///
/// Only a copy in one of those is safe to rely on before the main repository drops
/// its own. Nothing in a ref-less LOP reaches a blob, so a loose object or one in
/// an unkept pack goes at its next `gc`, and so does one in a pack kept only for a
/// while: `receive-pack` and `fetch-pack` keep the pack they are writing until
/// they have updated the refs, then remove the marker. A copy found through the
/// LOP's alternates (possibly the main repository itself) is not the LOP's at all.
///
/// # Errors
///
/// Pack directory read failures.
pub(crate) fn lop_keep_packs(lop: &Odb) -> Result<Vec<std::sync::Arc<PackIndex>>> {
    Ok(lop
        .local_pack_indexes()?
        .into_iter()
        .filter(|idx| has_lop_keep_marker(&idx.pack_path))
        .collect())
}

/// Whether the `.keep` marker next to `pack_path` is one offload wrote.
fn has_lop_keep_marker(pack_path: &Path) -> bool {
    std::fs::read(pack_path.with_extension("keep"))
        .is_ok_and(|contents| contents.trim_ascii_end() == LOP_KEEP_MESSAGE.as_bytes())
}

/// Set `changes` and add the values of `additions` (keeping earlier ones) in the
/// local config of the repository at `git_dir`.
fn write_config(
    git_dir: &Path,
    changes: &[(String, String)],
    additions: &[(String, String)],
) -> Result<()> {
    if changes.is_empty() && additions.is_empty() {
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
    for (key, value) in additions {
        config.add_value(key, value)?;
    }
    config.write()
}

/// `git_dir` as recorded in [`LOP_GIT_DIR_KEY`]: absolute, symlinks resolved when
/// possible.
fn lop_dir_value(git_dir: &Path) -> String {
    use crate::git_path::CanonicalizeExt as _;
    git_dir
        .canonicalize_simplified()
        .unwrap_or_else(|_| git_dir.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Whether two repositories store their objects in the same directory: one is the
/// other, a linked worktree of it, or has its `objects/` linked to the other's.
pub(crate) fn same_object_store(a: &Repository, b: &Repository) -> bool {
    same_dir(a.odb().objects_dir(), b.odb().objects_dir())
}

/// Whether two paths name the same directory.
pub(crate) fn same_dir(a: &Path, b: &Path) -> bool {
    use crate::git_path::CanonicalizeExt as _;
    match (a.canonicalize_simplified(), b.canonicalize_simplified()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
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
        // The rest of main, the kept blob included, lives in one promisor pack; the
        // loose objects are gone.
        let promisor = fx.files(&fx.main_dir(), "promisor");
        assert_eq!(promisor.len(), 1);
        assert_eq!(
            report.main_pack.as_ref().and_then(|p| p.path.file_name()),
            promisor[0].with_extension("pack").file_name()
        );
        let indexes = main.odb().local_pack_indexes().unwrap();
        let promisor_idx = indexes
            .iter()
            .find(|i| i.is_promisor)
            .expect("promisor pack");
        assert_eq!(indexes.len(), 1);
        assert_eq!(promisor_idx.len(), 3, "the commit, its tree and f1");
        assert!(promisor_idx.find_position(&fx.blobs[1]).is_some());
        assert!(main
            .odb()
            .enumerate_local_loose_objects()
            .unwrap()
            .is_empty());
        assert!(report.main_bytes_after < report.main_bytes_before);

        // The report describes each pack it wrote.
        let roles: Vec<_> = report.packs().map(|p| (p.role, p.objects)).collect();
        assert_eq!(
            roles,
            [
                (OffloadPackRole::Lop, 2),
                (OffloadPackRole::MainPromisor, 3),
            ]
        );
        for pack in report.packs() {
            assert_eq!(pack.bytes, std::fs::metadata(&pack.path).unwrap().len());
        }
        assert!(report
            .lop_pack
            .as_ref()
            .unwrap()
            .path
            .starts_with(fx.lop().git_dir()));
    }

    #[test]
    fn lop_pack_is_kept_and_complete() {
        let fx = Fixture::new(&[4096]);
        let report = fx.offload(1024);
        let lop_pack = report.lop_pack.expect("lop pack").path;
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
        let lop_dir = lop_dir_value(fx.lop().git_dir());
        assert!(Path::new(&lop_dir).is_absolute());
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
                (OffloadRepo::Main, LOP_GIT_DIR_KEY, lop_dir.as_str()),
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
        assert_eq!(config.get_all(LOP_GIT_DIR_KEY), [lop_dir.clone()]);

        // The LOP is recorded once, however often offload runs.
        let again = offload_blobs(&fx.main(), &opts).unwrap();
        assert!(
            again.config_changes.is_empty(),
            "{:?}",
            again.config_changes
        );
        assert_eq!(
            fx.main().config().unwrap().get_all(LOP_GIT_DIR_KEY),
            [lop_dir]
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
        assert_eq!(
            keys,
            [
                "remote.lop.promisor",
                "repack.writeBitmaps",
                LOP_GIT_DIR_KEY
            ]
        );
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

    /// Put `fx`'s first blob into a new LOP pack whose `.keep` marker says `message`.
    fn pack_into_lop(fx: &Fixture, message: &str) {
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
            fx.lop().odb(),
            &IngestPackOptions {
                keep_message: Some(message.to_owned()),
                ..IngestPackOptions::default()
            },
        )
        .unwrap();
    }

    #[test]
    fn blobs_the_lop_keeps_are_not_copied_again() {
        let fx = Fixture::new(&[2048]);
        pack_into_lop(&fx, LOP_KEEP_MESSAGE);
        let report = fx.offload(1024);
        assert_eq!(report.already_in_lop, 1);
        assert_eq!(report.copied_to_lop, 0);
        assert!(report.lop_pack.is_none());
        assert_eq!(report.dropped_from_main, 1);
    }

    #[test]
    fn a_copy_another_writer_keeps_for_a_while_is_not_trusted() {
        // receive-pack and fetch-pack keep the pack they write until the refs are
        // updated, then remove the marker; an operator's marker is no promise either.
        for message in ["receive-pack 4242 on build-host", "pinned"] {
            let fx = Fixture::new(&[2048]);
            pack_into_lop(&fx, message);
            let report = fx.offload(1024);
            assert_eq!(report.already_in_lop, 0, "{message}");
            assert_eq!(report.copied_to_lop, 1, "{message}");
            let lop_pack = report.lop_pack.expect("copied").path;
            let keep = std::fs::read_to_string(lop_pack.with_extension("keep")).unwrap();
            assert_eq!(keep.trim_end(), LOP_KEEP_MESSAGE);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_lop_sharing_main_objects_is_refused() {
        let fx = Fixture::new(&[2048]);
        let lop_objects = fx.lop_dir().join("objects");
        std::fs::remove_dir_all(&lop_objects).unwrap();
        std::os::unix::fs::symlink(fx.main_dir().join("objects"), &lop_objects).unwrap();
        let mut opts = OffloadOptions::new("lop", NOW);
        opts.filter = Some(ObjectFilterSpec::BlobLimit(1024));
        assert!(matches!(
            offload_err(&fx, &opts),
            OffloadError::LopIsMainRepository
        ));
        assert!(fx.main().odb().exists_stored(&fx.blobs[0]));
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
        let pack = again.main_pack.expect("folded").path;
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

    /// Pack `oids` of main into a new pack with a `.keep` marker, dropping their
    /// loose copies.
    fn keep_in_main(fx: &Fixture, oids: &[ObjectId]) -> PathBuf {
        let main = fx.main();
        let pack = crate::pack_objects::build_pack_from_send_list(
            main.odb(),
            oids,
            &std::collections::HashSet::new(),
            &PackBuildOptions::default(),
            &std::collections::HashMap::new(),
        )
        .unwrap();
        let installed = crate::index_pack::install_pack_bytes(
            pack,
            main.odb(),
            &IngestPackOptions {
                keep_message: Some("pinned".into()),
                ..IngestPackOptions::default()
            },
        )
        .unwrap();
        crate::prune_packed::prune_packed_objects(main.odb().objects_dir(), Default::default())
            .unwrap();
        installed.pack_path.unwrap()
    }

    #[test]
    fn a_kept_object_the_promisor_pack_would_name_stops_offload() {
        // f1 (small, staying) has its only copy in an operator's kept pack, and the
        // tree naming it is loose: in the promisor pack it would make f1 promised.
        let fx = Fixture::new(&[2048, 10]);
        let kept = keep_in_main(&fx, &[fx.blobs[1]]);
        let mut opts = OffloadOptions::new("lop", NOW);
        opts.filter = Some(ObjectFilterSpec::BlobLimit(1024));
        for dry_run in [true, false] {
            opts.dry_run = dry_run;
            match offload_err(&fx, &opts) {
                OffloadError::KeptPackObjectPromised { pack, oid, .. } => {
                    assert_eq!(pack, kept);
                    assert_eq!(oid, fx.blobs[1]);
                }
                other => panic!("got {other:?}"),
            }
        }
        assert!(fx.files(&fx.lop_dir(), "pack").is_empty(), "nothing copied");
        assert!(fx.files(&fx.main_dir(), "promisor").is_empty());
        assert!(fx.main().odb().exists_stored(&fx.blobs[0]));

        // Without the marker the pack is folded in like any other.
        std::fs::remove_file(kept.with_extension("keep")).unwrap();
        let report = fx.offload(1024);
        assert_eq!(report.dropped_from_main, 1);
        let main = fx.main();
        let indexes = main.odb().local_pack_indexes().unwrap();
        assert_eq!(indexes.len(), 1);
        assert!(indexes[0].is_promisor);
        assert!(indexes[0].find_position(&fx.blobs[1]).is_some());
    }

    #[test]
    fn a_kept_pack_nothing_names_does_not_stop_offload() {
        let fx = Fixture::new(&[2048]);
        let stray = fx
            .main()
            .odb()
            .write(ObjectKind::Blob, b"named by nothing")
            .unwrap();
        let kept = keep_in_main(&fx, &[stray]);
        let report = fx.offload(1024);
        assert_eq!(report.dropped_from_main, 1);
        assert!(kept.is_file() && kept.with_extension("keep").is_file());
        assert!(fx.main().odb().exists_stored(&stray));
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
        let mut req = crate::porcelain::commit::CommitRequest::new("big");
        let sig: crate::ident::Signature = ident.parse().unwrap();
        req.author = Some(sig.clone());
        req.committer = Some(sig);
        crate::porcelain::commit::create_commit(
            &repo,
            &req,
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
