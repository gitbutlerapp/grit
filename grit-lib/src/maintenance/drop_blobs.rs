//! Reclaim space in a partial clone by dropping large promised blobs.
//!
//! A partial clone fetches blobs on demand, and over time keeps every large blob it
//! ever needed. The promisor remote still has them, so the local copies can go once
//! nothing needs them nearby: they come back through the ordinary lazy fetch the
//! next time something reads them. [`drop_promised_blobs`] does that for blobs
//! stored in promisor packs, at or above a size, and not needed recently.
//!
//! A blob is dropped only when all of these hold:
//!
//! - it is stored in a `.promisor` pack, and nowhere else in this repository (not
//!   loose, not in an ordinary pack), and none of its packs has a `.keep` marker;
//! - a promisor can supply it again (see below);
//! - some tree or tag stored in a promisor pack names it, so it stays a promised
//!   object once it is gone (Git's rule for an acceptable missing object);
//! - no worktree's index (nor the `GIT_INDEX_FILE` one, when set) or `HEAD` tree
//!   uses it;
//! - no commit named by a worktree's `HEAD` reflog since the cutoff uses it;
//! - none of the promisor packs holding it was written or freshened since the
//!   cutoff (a lazy fetch writes a new pack, so this is when it was last fetched).
//!
//! In a partial clone, a blob in a promisor pack came from the promisor remote, so
//! it can be fetched again. That stops being true once [`offload_blobs`] has run:
//! the promisor pack it writes also holds the blobs only this repository has (Git
//! would delete them anywhere else; see [`offload_blobs`]). So the promisors this
//! machine can read are checked directly: promisor remotes with a local URL (a path
//! or `file://`) and the LOP directories offload recorded in
//! [`LOP_GIT_DIR_KEY`]. A blob one of them stores in its own object directory (not
//! through its alternates) can go; in a recorded LOP, only a copy in a pack offload
//! wrote there counts, since the LOP's own `gc` prunes any other (nothing in it
//! reaches a blob). A promisor that stores its objects where this repository does
//! (a linked worktree of it, say) holds nothing for this. A blob none of them
//! stores stays when every promisor was read ([`KeptBlobCounts::not_in_promisor`]),
//! and also ([`KeptBlobCounts::unverified`]) when offload has run here, since the
//! network promisors left cannot be asked without fetching, or when a local
//! promisor cannot be read (its directory is not a repository now, an unmounted
//! disk say, or uses another object format). Otherwise, a partial clone of network
//! remotes only, the promisor pack is trusted as Git does, and nothing extra is
//! read.
//!
//! [`offload_blobs`]: crate::maintenance::offload_blobs
//!
//! Git 2.56 has a manual version of the same operation,
//! `git repack -a -d --filter=blob:limit=<n> --drop-filtered`: the same candidates,
//! without the recency and promise checks, and refusing to run when the index
//! names a candidate rather than keeping it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use smallvec::SmallVec;
use time::OffsetDateTime;

use crate::config::ConfigSet;
use crate::environment::RepositoryOptions;
use crate::error::{BadNumericSource, ConfigError, Error, Result};
use crate::git_date::approx::approxidate_careful_at;
use crate::index_pack::{install_pack_path, IngestPackOptions};
use crate::objects::{
    oid_hash_map, oid_hash_set, CommitRef, ObjectId, ObjectKind, OidHashMap, OidHashSet,
    TreeRefIter,
};
use crate::odb::Odb;
use crate::offload::LOP_GIT_DIR_KEY;
use crate::pack::PackedType;
use crate::pack_index::PackIndex;
use crate::pack_objects::PackBuildOptions;
use crate::repo::Repository;
use crate::state::InProgressOperation;

use super::{GcLock, GcLockAttempt};

/// Name of the maintenance task, as used in its config keys
/// (`maintenance.drop-large-blobs.*`) and in `grit gc drop-large-blobs`.
pub const DROP_LARGE_BLOBS_TASK: &str = "drop-large-blobs";

/// Size threshold used when neither `maintenance.drop-large-blobs.minSize` nor a
/// promisor remote's `blob:limit` filter gives one: 1 MiB.
pub const DEFAULT_DROP_MIN_SIZE: u64 = 1 << 20;

/// Recency window used when `maintenance.drop-large-blobs.expire` is unset: blobs
/// needed in the last two weeks stay.
pub const DEFAULT_DROP_EXPIRE: &str = "2.weeks.ago";

const ENABLED_KEY: &str = "maintenance.drop-large-blobs.enabled";
const MIN_SIZE_KEY: &str = "maintenance.drop-large-blobs.minsize";
const EXPIRE_KEY: &str = "maintenance.drop-large-blobs.expire";

/// The operation name used in errors.
const OPERATION: &str = "drop-large-blobs";

/// Options for [`drop_promised_blobs`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DropPromisedBlobsOptions {
    /// Current time: for the `gc.pid` maintenance lock. Cutoffs are given explicitly
    /// in [`Self::keep_newer_than`].
    pub now: OffsetDateTime,
    /// Drop blobs of at least this many bytes (uncompressed), like a
    /// `blob:limit=<n>` filter.
    pub min_size: u64,
    /// Keep blobs needed at or after this time (see the module documentation for
    /// what counts as needed). `None` keeps nothing for recency.
    pub keep_newer_than: Option<OffsetDateTime>,
    /// Work out what would be dropped without changing anything (and without taking
    /// the maintenance lock).
    pub dry_run: bool,
    /// Pack writing and indexing threads (`None` reads `pack.threads`).
    pub threads: Option<usize>,
}

impl DropPromisedBlobsOptions {
    /// Options that drop blobs of `min_size` bytes or more not needed since
    /// `keep_newer_than`, at time `now`.
    #[must_use]
    pub fn new(
        now: OffsetDateTime,
        min_size: u64,
        keep_newer_than: Option<OffsetDateTime>,
    ) -> Self {
        Self {
            now,
            min_size,
            keep_newer_than,
            dry_run: false,
            threads: None,
        }
    }

    /// Options from `maintenance.drop-large-blobs.minSize` and
    /// `maintenance.drop-large-blobs.expire`, at time `now`.
    ///
    /// `minSize` takes Git's `k`/`m`/`g` suffixes. When unset it is the smallest
    /// `blob:limit` among the promisor remotes' `partialCloneFilter`, or
    /// [`DEFAULT_DROP_MIN_SIZE`]. `expire` is read with [`DropExpiry::parse`]
    /// ([`DEFAULT_DROP_EXPIRE`] when unset).
    ///
    /// # Returns
    ///
    /// `None` when `expire` is [`DropExpiry::Never`].
    ///
    /// # Errors
    ///
    /// [`ConfigError::BadNumericValue`] for a `minSize` that is not a non-negative
    /// size, and [`ConfigError::InvalidDate`] for an `expire` that is not a date.
    pub fn from_config(config: &ConfigSet, now: OffsetDateTime) -> Result<Option<Self>> {
        Self::from_config_with_overrides(config, now, None, None)
    }

    /// [`Self::from_config`] with values the caller gives (a command line's
    /// `--min-size` and `--older-than`) in place of the config ones.
    ///
    /// A config key that is overridden is not read, so an invalid value there does
    /// not matter. The other one still comes from config, with the same defaults.
    ///
    /// # Parameters
    ///
    /// - `min_size`: replaces `maintenance.drop-large-blobs.minSize` and its defaults.
    /// - `expiry`: replaces `maintenance.drop-large-blobs.expire`.
    ///
    /// # Returns
    ///
    /// `None` when the expiry in effect is [`DropExpiry::Never`].
    ///
    /// # Errors
    ///
    /// As [`Self::from_config`], for the keys that are read.
    pub fn from_config_with_overrides(
        config: &ConfigSet,
        now: OffsetDateTime,
        min_size: Option<u64>,
        expiry: Option<DropExpiry>,
    ) -> Result<Option<Self>> {
        let expiry = match expiry {
            Some(expiry) => expiry,
            None => configured_expiry(config, now)?,
        };
        let keep_newer_than = match expiry {
            DropExpiry::Never => return Ok(None),
            DropExpiry::Immediately => None,
            DropExpiry::KeepNewerThan(cutoff) => Some(cutoff),
        };
        let min_size = match min_size {
            Some(min_size) => min_size,
            None => configured_min_size(config)?,
        };
        Ok(Some(Self::new(now, min_size, keep_newer_than)))
    }
}

/// How long recently needed blobs are kept: the value of
/// `maintenance.drop-large-blobs.expire`, or of a command line's `--older-than`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropExpiry {
    /// `never` or `false`: nothing is dropped, and `gc` skips the phase.
    Never,
    /// `now` or `all`: nothing is kept for recency.
    Immediately,
    /// A date: blobs needed at or after it are kept.
    KeepNewerThan(OffsetDateTime),
}

impl DropExpiry {
    /// Parse `spec` (case-insensitive keywords, else a Git approxidate such as
    /// `2.weeks.ago` or `2026-01-01`) relative to `now`.
    ///
    /// # Returns
    ///
    /// `None` when `spec` is neither a keyword nor a date.
    #[must_use]
    pub fn parse(spec: &str, now: OffsetDateTime) -> Option<Self> {
        let spec = spec.trim();
        if spec.eq_ignore_ascii_case("never") || spec.eq_ignore_ascii_case("false") {
            return Some(Self::Never);
        }
        if spec.eq_ignore_ascii_case("now") || spec.eq_ignore_ascii_case("all") {
            return Some(Self::Immediately);
        }
        if spec.is_empty() {
            return None;
        }
        let mut err = 0;
        let secs = approxidate_careful_at(spec, now.unix_timestamp(), Some(&mut err));
        if err != 0 {
            return None;
        }
        let cutoff = i64::try_from(secs)
            .ok()
            .and_then(|secs| OffsetDateTime::from_unix_timestamp(secs).ok())?;
        Some(Self::KeepNewerThan(cutoff))
    }
}

/// `maintenance.drop-large-blobs.enabled` (default false): whether `gc` runs the
/// drop phase. The `grit gc drop-large-blobs` command runs regardless.
///
/// # Errors
///
/// [`Error::Config`] when the value is not a boolean.
pub fn drop_large_blobs_enabled(config: &ConfigSet) -> Result<bool> {
    match config.get_bool(ENABLED_KEY) {
        None => Ok(false),
        Some(Ok(value)) => Ok(value),
        Some(Err(err)) => Err(Error::Config(err.into())),
    }
}

/// `expire`, or [`DEFAULT_DROP_EXPIRE`] when unset or empty.
fn configured_expiry(config: &ConfigSet, now: OffsetDateTime) -> Result<DropExpiry> {
    let spec = config
        .get(EXPIRE_KEY)
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_DROP_EXPIRE.to_owned());
    DropExpiry::parse(&spec, now).ok_or_else(|| {
        Error::Config(ConfigError::InvalidDate {
            key: EXPIRE_KEY.to_owned(),
            value: spec,
        })
    })
}

/// `minSize`, or the smallest promisor `blob:limit`, or [`DEFAULT_DROP_MIN_SIZE`].
fn configured_min_size(config: &ConfigSet) -> Result<u64> {
    if let Some(entry) = config.get_last_entry(MIN_SIZE_KEY) {
        let raw = entry.value.as_deref().unwrap_or_default();
        let bad = |reason| {
            Error::Config(ConfigError::BadNumericValue {
                key: MIN_SIZE_KEY.to_owned(),
                value: raw.to_owned(),
                reason,
            })
        };
        let value = crate::config::parse_git_config_int_strict(raw).map_err(|e| {
            bad(match e {
                crate::config::GitConfigIntStrictError::InvalidUnit => {
                    BadNumericSource::InvalidUnit
                }
                crate::config::GitConfigIntStrictError::OutOfRange => BadNumericSource::OutOfRange,
            })
        })?;
        return u64::try_from(value).map_err(|_| bad(BadNumericSource::OutOfRange));
    }
    Ok(
        crate::promisor_remote::configured_promisor_remotes(config, &[])
            .iter()
            .filter_map(|remote| {
                crate::partial_clone::stored_remote_filter(config, &remote.name)
                    .and_then(|filter| filter.blob_limit_bytes())
            })
            .min()
            .unwrap_or(DEFAULT_DROP_MIN_SIZE),
    )
}

/// One blob [`drop_promised_blobs`] dropped (or, on a dry run, would drop).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DroppedBlob {
    /// The blob.
    pub oid: ObjectId,
    /// Its size, uncompressed.
    pub size: u64,
    /// A promisor pack it was stored in (the first, when several held it).
    pub pack: PathBuf,
}

/// Why candidate blobs stayed. Each candidate is counted once, under the first
/// reason that applies, in the order of the fields.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct KeptBlobCounts {
    /// A promisor pack holding it has a `.keep` marker.
    pub kept_pack: usize,
    /// It is also stored loose or in a pack without `.promisor`, so dropping the
    /// promisor copy would free nothing (and Git would no longer treat it as
    /// promised).
    pub outside_promisor_packs: usize,
    /// Every promisor is on this machine and was read, and none stores it, so
    /// nothing could supply it again.
    pub not_in_promisor: usize,
    /// No promisor this machine could read stores it, and the rest cannot be asked:
    /// network remotes, which cannot be asked without fetching (counted here once
    /// offload has put blobs only this repository has into its promisor packs, so
    /// being in one proves nothing), or a local promisor that does not open as a
    /// repository of this object format (an unmounted disk, say).
    pub unverified: usize,
    /// A worktree's index (or the `GIT_INDEX_FILE` one) or `HEAD` tree uses it.
    pub checked_out: usize,
    /// The tree of a commit a worktree's `HEAD` reflog named since the cutoff uses
    /// it.
    pub recently_checked_out: usize,
    /// A promisor pack holding it was written (or freshened) since the cutoff.
    pub recently_fetched: usize,
    /// No tree or tag in a promisor pack names it, so once gone it would be missing
    /// rather than promised.
    pub not_promised_elsewhere: usize,
}

impl KeptBlobCounts {
    /// All candidates kept, for any reason.
    #[must_use]
    pub fn total(&self) -> usize {
        self.kept_pack
            + self.outside_promisor_packs
            + self.checked_out
            + self.recently_checked_out
            + self.recently_fetched
            + self.not_promised_elsewhere
            + self.not_in_promisor
            + self.unverified
    }
}

/// Why `gc` did not run its drop phase ([`DropPromisedBlobsReport::skipped`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DropPromisedBlobsSkip {
    /// `maintenance.drop-large-blobs.enabled` is not true, or `expire` is `never`.
    Disabled,
    /// The repository has no promisor remote, so there is nothing to fetch back from.
    NoPromisorRemote,
    /// A worktree is in the middle of this operation, which may need the blobs.
    OperationInProgress(InProgressOperation),
    /// `extensions.preciousObjects` is set: objects are never deleted.
    PreciousObjects,
    /// The `gc` options carry no current time ([`super::GcOptions::now_unix`] is 0,
    /// or not a representable time), so there is no recency cutoff to apply.
    NoCurrentTime,
}

/// What [`drop_promised_blobs`] did, or with [`DropPromisedBlobsOptions::dry_run`]
/// would do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct DropPromisedBlobsReport {
    /// The size threshold used.
    pub min_size: u64,
    /// The recency cutoff used (`None`: no recency protection).
    pub keep_newer_than: Option<OffsetDateTime>,
    /// Blobs stored in promisor packs at or above [`Self::min_size`].
    pub candidates: usize,
    /// The blobs dropped (on a dry run, those that would be), sorted by id.
    pub dropped: Vec<DroppedBlob>,
    /// The candidates that stayed, by reason.
    pub kept: KeptBlobCounts,
    /// Bytes freed: the old packs' size (with index and reverse index) minus the
    /// new ones'. On a dry run, an estimate: the packed size of the dropped entries.
    pub bytes_reclaimed: u64,
    /// Promisor packs rewritten without the dropped blobs.
    pub packs_rewritten: usize,
    /// Promisor packs removed because they held only dropped blobs.
    pub packs_removed: usize,
    /// Set when `gc` skipped the phase; the counts are then zero.
    pub skipped: Option<DropPromisedBlobsSkip>,
}

impl DropPromisedBlobsReport {
    fn skipped_for(reason: DropPromisedBlobsSkip) -> Self {
        Self {
            skipped: Some(reason),
            ..Self::default()
        }
    }
}

/// Drop large blobs a promisor remote can supply again from `repo`'s promisor
/// packs, keeping those needed recently (see the module documentation for the
/// exact rules).
///
/// A blob stays unless a promisor can supply it again: after
/// [`crate::maintenance::offload_blobs`] has run, or when a promisor remote has a
/// local URL, that is checked in the promisor's own object directory
/// ([`KeptBlobCounts::not_in_promisor`], [`KeptBlobCounts::unverified`]).
///
/// Nothing is fetched while deciding. Unless [`DropPromisedBlobsOptions::dry_run`]
/// is set, the repository's `gc.pid` lock is held throughout. A promisor pack that
/// held only dropped blobs is removed; any other affected one is rewritten without
/// them (deltas against a dropped blob are re-encoded) and keeps its `.promisor`
/// contents and its modification time, so the recency of the blobs it still holds
/// survives. The new pack is on disk before the old one is removed. A
/// multi-pack-index is rewritten without a bitmap.
///
/// # Parameters
///
/// - `repo`: a partial clone (or any repository with a promisor remote), files-backed.
/// - `opts`: threshold, cutoff and time.
///
/// # Errors
///
/// - [`Error::UnsupportedObjectStore`] without a files-backed object store;
/// - [`Error::PreciousObjectsRepository`] when `extensions.preciousObjects` is set;
/// - [`Error::NoPromisorRemote`] when no remote is a promisor and
///   `extensions.partialClone` is unset;
/// - [`Error::OperationInProgress`] when a worktree is in the middle of a merge,
///   rebase, cherry-pick, revert, am or bisect;
/// - [`Error::MaintenanceRunning`] when another process holds `gc.pid`;
/// - pack read, write and removal failures.
pub fn drop_promised_blobs(
    repo: &Repository,
    opts: &DropPromisedBlobsOptions,
) -> Result<DropPromisedBlobsReport> {
    repo.odb().require_files_primary(OPERATION)?;
    let config = repo.config()?;
    if let Some(refusal) = gate(repo, &config)? {
        return Err(refusal.into());
    }
    let _lock = if opts.dry_run {
        None
    } else {
        match GcLock::try_acquire(repo, opts.now.unix_timestamp())? {
            GcLockAttempt::Acquired(lock) => Some(lock),
            GcLockAttempt::Held(holder) => return Err(Error::MaintenanceRunning { holder }),
        }
    };
    run(repo, &config, opts)
}

impl Repository {
    /// Drop large promised blobs not needed recently; see [`drop_promised_blobs`].
    ///
    /// # Errors
    ///
    /// See [`drop_promised_blobs`].
    pub fn drop_promised_blobs(
        &self,
        opts: &DropPromisedBlobsOptions,
    ) -> Result<DropPromisedBlobsReport> {
        drop_promised_blobs(self, opts)
    }
}

/// What the `gc` drop phase will do, settled from config before `gc` changes
/// anything, so a bad value fails the run before its first phase.
pub(crate) enum GcDropPlan {
    /// The phase reports this skip.
    Skip(DropPromisedBlobsSkip),
    /// The phase runs with these options (`dry_run` and `threads` still to set).
    Run(DropPromisedBlobsOptions),
}

/// Settle the `gc` phase: off unless `maintenance.drop-large-blobs.enabled` is
/// true, options from config, and the current time from `now_unix` (0 means none
/// was given, which skips the phase rather than read a clock here).
///
/// # Errors
///
/// [`Error::Config`] for an invalid `enabled`, `minSize` or `expire`.
pub(crate) fn plan_drop_for_gc(config: &ConfigSet, now_unix: i64) -> Result<GcDropPlan> {
    if !drop_large_blobs_enabled(config)? {
        return Ok(GcDropPlan::Skip(DropPromisedBlobsSkip::Disabled));
    }
    let now = (now_unix != 0)
        .then(|| OffsetDateTime::from_unix_timestamp(now_unix).ok())
        .flatten();
    let Some(now) = now else {
        // `expire` is relative to the time, so only `minSize` can be checked.
        configured_min_size(config)?;
        return Ok(GcDropPlan::Skip(DropPromisedBlobsSkip::NoCurrentTime));
    };
    Ok(match DropPromisedBlobsOptions::from_config(config, now)? {
        Some(opts) => GcDropPlan::Run(opts),
        None => GcDropPlan::Skip(DropPromisedBlobsSkip::Disabled),
    })
}

/// The `gc` phase: runs a [`GcDropPlan::Run`], and reports a skip instead of
/// failing on the gates. The caller holds the `gc.pid` lock (unless `dry_run`).
pub(crate) fn run_drop_for_gc(
    repo: &Repository,
    config: &ConfigSet,
    plan: GcDropPlan,
    dry_run: bool,
    threads: Option<usize>,
) -> Result<DropPromisedBlobsReport> {
    let mut opts = match plan {
        GcDropPlan::Skip(reason) => return Ok(DropPromisedBlobsReport::skipped_for(reason)),
        GcDropPlan::Run(opts) => opts,
    };
    if let Some(refusal) = gate(repo, config)? {
        return Ok(DropPromisedBlobsReport::skipped_for(refusal.into()));
    }
    opts.dry_run = dry_run;
    opts.threads = threads;
    run(repo, config, &opts)
}

/// Why the repository is not in a state to drop anything.
enum Refusal {
    PreciousObjects,
    NoPromisorRemote,
    InProgress(InProgressOperation),
}

impl From<Refusal> for DropPromisedBlobsSkip {
    fn from(refusal: Refusal) -> Self {
        match refusal {
            Refusal::PreciousObjects => Self::PreciousObjects,
            Refusal::NoPromisorRemote => Self::NoPromisorRemote,
            Refusal::InProgress(op) => Self::OperationInProgress(op),
        }
    }
}

impl From<Refusal> for Error {
    fn from(refusal: Refusal) -> Self {
        match refusal {
            Refusal::PreciousObjects => Error::PreciousObjectsRepository {
                operation: OPERATION,
            },
            Refusal::NoPromisorRemote => Error::NoPromisorRemote {
                operation: OPERATION,
            },
            Refusal::InProgress(op) => Error::OperationInProgress(op),
        }
    }
}

/// The checks that must pass before anything is considered: `None` when they do.
fn gate(repo: &Repository, config: &ConfigSet) -> Result<Option<Refusal>> {
    if config
        .get_bool("extensions.preciousobjects")
        .and_then(|v| v.ok())
        .unwrap_or(false)
    {
        return Ok(Some(Refusal::PreciousObjects));
    }
    if !crate::promisor::repo_treats_promisor_packs(repo.git_dir(), config) {
        return Ok(Some(Refusal::NoPromisorRemote));
    }
    for wt in crate::worktree::list_worktrees(repo)? {
        if wt.is_bare {
            continue;
        }
        if let Some(op) = crate::state::detect_in_progress(&wt.admin_dir).first() {
            return Ok(Some(Refusal::InProgress(*op)));
        }
    }
    Ok(None)
}

/// A promisor pack in the local object directory.
struct PromisorPack {
    idx: Arc<PackIndex>,
    mtime: Option<SystemTime>,
    kept: bool,
    /// Index rows of the trees and tags it stores.
    referrers: Vec<u32>,
    /// Index rows of the commits it stores.
    commits: Vec<u32>,
}

/// A blob of at least the threshold stored in promisor packs.
struct Candidate {
    size: u64,
    /// Indexes into the promisor pack list.
    packs: SmallVec<[u32; 1]>,
    /// Packed bytes of its entries (the dry-run estimate).
    entry_bytes: u64,
}

fn run(
    repo: &Repository,
    config: &ConfigSet,
    opts: &DropPromisedBlobsOptions,
) -> Result<DropPromisedBlobsReport> {
    let odb = repo.odb();
    let pack_dir = odb.objects_dir().join("pack");
    odb.reprepare_local_packs();
    let (promisor, others): (Vec<_>, Vec<_>) = odb
        .local_pack_indexes()?
        .into_iter()
        .partition(|idx| idx.is_promisor);
    let mut packs: Vec<PromisorPack> = promisor
        .into_iter()
        .map(|idx| PromisorPack {
            mtime: std::fs::metadata(&idx.pack_path)
                .and_then(|m| m.modified())
                .ok(),
            kept: super::pack_has_keep_marker(&pack_dir, &idx.pack_path),
            idx,
            referrers: Vec::new(),
            commits: Vec::new(),
        })
        .collect();
    // Same order on every run, so the report's pack names are stable.
    packs.sort_by(|a, b| a.idx.pack_path.cmp(&b.idx.pack_path));

    let mut report = DropPromisedBlobsReport {
        min_size: opts.min_size,
        keep_newer_than: opts.keep_newer_than,
        ..DropPromisedBlobsReport::default()
    };
    let mut candidates: OidHashMap<Candidate> = oid_hash_map();
    for (pack_no, pack) in packs.iter_mut().enumerate() {
        scan_pack(odb, pack, pack_no as u32, opts.min_size, &mut candidates)?;
    }
    report.candidates = candidates.len();
    if candidates.is_empty() {
        return Ok(report);
    }

    // Decide each candidate's fate; the first reason that applies wins.
    let cutoff = opts.keep_newer_than.map(SystemTime::from);
    let mut stored_only_here: OidHashSet = oid_hash_set();
    for (oid, cand) in &candidates {
        let holding = || cand.packs.iter().map(|&p| &packs[p as usize]);
        if holding().any(|p| p.kept) {
            report.kept.kept_pack += 1;
        } else if odb.local_loose_file_size(oid).is_some()
            || others.iter().any(|idx| idx.find_position(oid).is_some())
        {
            report.kept.outside_promisor_packs += 1;
        } else {
            stored_only_here.insert(*oid);
        }
    }
    // Before the walks: in an offloaded repository most candidates stop here.
    let mut undecided =
        PromisorSupply::load(repo, config).suppliable(stored_only_here, &mut report.kept)?;
    let mut walk = ProtectionWalk::new(odb, &undecided);
    walk.mark_checkouts(repo)?;
    let checked_out = std::mem::take(&mut walk.hits);
    if let Some(cutoff) = opts.keep_newer_than {
        walk.mark_recent_heads(repo, cutoff)?;
    }
    let recently_checked_out = std::mem::take(&mut walk.hits);
    drop(walk);
    report.kept.checked_out = checked_out.len();
    report.kept.recently_checked_out = recently_checked_out.len();
    undecided.retain(|oid| !checked_out.contains(oid) && !recently_checked_out.contains(oid));
    undecided.retain(|oid| {
        let fresh = cutoff.is_some_and(|cutoff| {
            candidates[oid]
                .packs
                .iter()
                .filter_map(|&p| packs[p as usize].mtime)
                .any(|mtime| mtime >= cutoff)
        });
        if fresh {
            report.kept.recently_fetched += 1;
        }
        !fresh
    });
    let promised = promised_by_promisor_packs(odb, &packs, &undecided)?;
    report.kept.not_promised_elsewhere = undecided.len() - promised.len();
    let drop_set = promised;

    let mut dropped: Vec<DroppedBlob> = drop_set
        .iter()
        .map(|oid| {
            let cand = &candidates[oid];
            DroppedBlob {
                oid: *oid,
                size: cand.size,
                pack: cand
                    .packs
                    .first()
                    .map(|&p| packs[p as usize].idx.pack_path.clone())
                    .unwrap_or_default(),
            }
        })
        .collect();
    dropped.sort_by_key(|d| d.oid);
    report.dropped = dropped;
    if drop_set.is_empty() {
        return Ok(report);
    }
    if opts.dry_run {
        report.bytes_reclaimed = drop_set.iter().map(|oid| candidates[oid].entry_bytes).sum();
        return Ok(report);
    }

    let mut affected: Vec<u32> = drop_set
        .iter()
        .flat_map(|oid| candidates[oid].packs.iter().copied())
        .collect();
    affected.sort_unstable();
    affected.dedup();
    rewrite_packs(
        repo,
        config,
        opts,
        &packs,
        &affected,
        &drop_set,
        &mut report,
    )?;
    Ok(report)
}

/// The promisors `drop-large-blobs` can check without fetching, and whether it
/// must.
struct PromisorSupply {
    /// Promisors on this machine that opened: promisor remotes with a local URL,
    /// and the LOPs offload recorded in [`LOP_GIT_DIR_KEY`].
    local: Vec<LocalPromisor>,
    /// Some promisor on this machine could not be read: its directory does not
    /// open as a repository (an unmounted disk, say) or uses another object format.
    /// What it holds is unknown, so blobs no readable promisor has are kept.
    unreadable: bool,
    /// Some promisor is on this machine (read or not, or `repo` itself).
    any_local: bool,
    /// Some promisor remote has a network URL.
    network: bool,
    /// [`LOP_GIT_DIR_KEY`] is set: offload has put blobs only this repository has
    /// into its promisor packs.
    offloaded: bool,
}

/// A promisor on this machine, and which of its copies count.
struct LocalPromisor {
    repo: Repository,
    /// A LOP offload recorded: only its packs offload wrote count (see
    /// [`crate::offload::lop_keep_packs`]). Nothing reaches a blob in a ref-less
    /// LOP, so its own `gc` prunes any other copy.
    lop: bool,
}

impl PromisorSupply {
    /// The promisors of `repo` per `config`.
    ///
    /// A local promisor that stores its objects where `repo` does (`repo` itself, a
    /// linked worktree of it, an `objects/` linked to its) supplies nothing. One
    /// that does not open as a repository of the same object format is
    /// [`Self::unreadable`].
    fn load(repo: &Repository, config: &ConfigSet) -> Self {
        let recorded: Vec<PathBuf> = config
            .get_all(LOP_GIT_DIR_KEY)
            .into_iter()
            .filter(|v| !v.trim().is_empty())
            .map(PathBuf::from)
            .collect();
        let mut dirs: Vec<(PathBuf, bool)> = recorded.iter().map(|d| (d.clone(), true)).collect();
        let mut network = false;
        for remote in crate::promisor_remote::configured_promisor_remotes(config, &[]) {
            let Some(url) = remote.url else { continue };
            if crate::transport_path::is_local_path_remote_url(&url) {
                let dir = crate::transport_path::resolve_local_remote_git_dir(
                    &url,
                    repo.git_dir(),
                    repo.work_tree(),
                );
                dirs.push((dir, false));
            } else {
                network = true;
            }
        }
        let mut supply = Self {
            local: Vec::new(),
            unreadable: false,
            any_local: !dirs.is_empty(),
            network,
            offloaded: !recorded.is_empty(),
        };
        for (dir, lop) in dirs {
            let opened = Repository::open(
                &dir,
                &RepositoryOptions::with_environment(repo.environment().clone()),
            );
            let other = match opened {
                Ok(other) if other.odb().hash_algo() == repo.odb().hash_algo() => other,
                _ => {
                    supply.unreadable = true;
                    continue;
                }
            };
            if crate::offload::same_object_store(&other, repo) {
                continue;
            }
            // Listed twice (a recorded LOP that is also a remote): the first, LOP, rule.
            if supply
                .local
                .iter()
                .any(|p| crate::offload::same_object_store(&other, &p.repo))
            {
                continue;
            }
            supply.local.push(LocalPromisor { repo: other, lop });
        }
        supply
    }

    /// The blobs of `promised` a promisor can supply again; the rest are counted
    /// in `kept`.
    ///
    /// A partial clone of network remotes only (no local promisor, no offload) is
    /// trusted without reading anything. Otherwise a blob goes only if a local
    /// promisor holds it, or, without offload and with every local promisor read,
    /// a network one may.
    fn suppliable(&self, promised: OidHashSet, kept: &mut KeptBlobCounts) -> Result<OidHashSet> {
        if !self.any_local && !self.offloaded {
            return Ok(promised);
        }
        if promised.is_empty() {
            return Ok(promised);
        }
        let mut stores = Vec::with_capacity(self.local.len());
        for promisor in &self.local {
            let odb = promisor.repo.odb();
            let indexes = if promisor.lop {
                crate::offload::lop_keep_packs(odb)?
            } else {
                odb.local_pack_indexes()?
            };
            stores.push((odb, indexes, !promisor.lop));
        }
        let held = |oid: &ObjectId| {
            stores.iter().any(|(odb, indexes, loose_counts)| {
                indexes.iter().any(|idx| idx.find_position(oid).is_some())
                    || (*loose_counts && odb.local_loose_file_size(oid).is_some())
            })
        };
        let trust_network = self.network && !self.offloaded && !self.unreadable;
        let mut out = oid_hash_set();
        for oid in promised {
            if held(&oid) || trust_network {
                out.insert(oid);
            } else if self.network || self.unreadable {
                kept.unverified += 1;
            } else {
                kept.not_in_promisor += 1;
            }
        }
        Ok(out)
    }
}

/// Record `pack`'s blobs of at least `min_size` in `candidates`, and its trees and
/// tags in [`PromisorPack::referrers`]. Only entry headers are decoded, plus the
/// start of each blob delta for its size.
fn scan_pack(
    odb: &Odb,
    pack: &mut PromisorPack,
    pack_no: u32,
    min_size: u64,
    candidates: &mut OidHashMap<Candidate>,
) -> Result<()> {
    let idx = Arc::clone(&pack.idx);
    let data = crate::pack::read_pack_bytes_cached(&idx.pack_path)?;
    let bytes: &[u8] = &data;
    let pack_end = (bytes.len() as u64).saturating_sub(idx.hash_bytes() as u64);
    let mut kinds = EntryKinds::new(odb, &idx, bytes);
    for pos in 0..idx.len() {
        match kinds.kind(pos)? {
            Some(ObjectKind::Blob) => {}
            Some(ObjectKind::Tree | ObjectKind::Tag) => {
                pack.referrers.push(pos as u32);
                continue;
            }
            Some(ObjectKind::Commit) => {
                pack.commits.push(pos as u32);
                continue;
            }
            None => continue,
        }
        let offset = idx.offset_at(pos)?;
        let size = entry_result_size(bytes, offset, idx.hash_bytes())?;
        if size < min_size {
            continue;
        }
        let oid = ObjectId::from_bytes(idx.oid_at(pos))?;
        let entry_bytes = idx
            .next_pack_offset_after(offset, pack_end)?
            .saturating_sub(offset);
        let cand = candidates.entry(oid).or_insert_with(|| Candidate {
            size,
            packs: SmallVec::new(),
            entry_bytes: 0,
        });
        cand.packs.push(pack_no);
        cand.entry_bytes += entry_bytes;
    }
    Ok(())
}

/// The uncompressed size of the entry at `offset`: its header size for a whole
/// object, the result size recorded at the start of a delta.
fn entry_result_size(bytes: &[u8], offset: u64, hash_bytes: usize) -> Result<u64> {
    let mut pos = usize::try_from(offset)
        .map_err(|_| Error::CorruptObject("pack offset out of range".to_owned()))?;
    let (packed, size) = crate::pack::parse_pack_object_header(bytes, &mut pos)?;
    match packed {
        PackedType::OfsDelta => {
            crate::pack::parse_ofs_delta_base(bytes, &mut pos, offset)?;
        }
        PackedType::RefDelta => pos += hash_bytes,
        _ => return Ok(size),
    }
    crate::pack::read_delta_result_size_from_pack_zlib(bytes, &mut pos)
}

/// Kinds of a pack's entries from their headers, following delta chains to their
/// bases and remembering every entry resolved.
struct EntryKinds<'a> {
    odb: &'a Odb,
    idx: &'a PackIndex,
    bytes: &'a [u8],
    /// Per index row: 0 unknown, 1-4 commit/tree/blob/tag, 5 unresolvable.
    kinds: Vec<u8>,
}

/// Deepest delta chain followed before the pack is treated as corrupt.
const MAX_CHAIN: usize = 10_000;

impl<'a> EntryKinds<'a> {
    fn new(odb: &'a Odb, idx: &'a PackIndex, bytes: &'a [u8]) -> Self {
        Self {
            odb,
            idx,
            bytes,
            kinds: vec![0; idx.len()],
        }
    }

    fn encode(kind: Option<ObjectKind>) -> u8 {
        match kind {
            Some(ObjectKind::Commit) => 1,
            Some(ObjectKind::Tree) => 2,
            Some(ObjectKind::Blob) => 3,
            Some(ObjectKind::Tag) => 4,
            None => 5,
        }
    }

    fn decode(code: u8) -> Option<ObjectKind> {
        match code {
            1 => Some(ObjectKind::Commit),
            2 => Some(ObjectKind::Tree),
            3 => Some(ObjectKind::Blob),
            4 => Some(ObjectKind::Tag),
            _ => None,
        }
    }

    /// The kind of the object at index row `pos`; `None` when its delta base is in
    /// neither this pack nor the local object store.
    fn kind(&mut self, pos: usize) -> Result<Option<ObjectKind>> {
        let corrupt = |what: &str| {
            Error::CorruptObject(format!("{what} in {}", self.idx.pack_path.display()))
        };
        let mut chain: Vec<usize> = Vec::new();
        let mut cur = pos;
        let kind = loop {
            if self.kinds[cur] != 0 {
                break Self::decode(self.kinds[cur]);
            }
            if chain.len() > MAX_CHAIN {
                return Err(corrupt("delta chain too deep"));
            }
            chain.push(cur);
            let offset = self.idx.offset_at(cur)?;
            let mut at =
                usize::try_from(offset).map_err(|_| corrupt("pack offset out of range"))?;
            let (packed, _) = crate::pack::parse_pack_object_header(self.bytes, &mut at)?;
            match packed {
                PackedType::Commit => break Some(ObjectKind::Commit),
                PackedType::Tree => break Some(ObjectKind::Tree),
                PackedType::Blob => break Some(ObjectKind::Blob),
                PackedType::Tag => break Some(ObjectKind::Tag),
                PackedType::OfsDelta => {
                    let base = crate::pack::parse_ofs_delta_base(self.bytes, &mut at, offset)?;
                    cur = self
                        .idx
                        .find_position_by_offset_sorted(base)?
                        .ok_or_else(|| corrupt("delta base offset not in index"))?;
                }
                PackedType::RefDelta => {
                    let hash_bytes = self.idx.hash_bytes();
                    let raw = self
                        .bytes
                        .get(at..at + hash_bytes)
                        .ok_or_else(|| corrupt("truncated delta base id"))?;
                    let base = ObjectId::from_bytes(raw)?;
                    match self.idx.find_position(&base) {
                        Some(base_pos) => cur = base_pos,
                        None => match self.odb.read_info_without_fetch(&base) {
                            Ok(info) => break Some(info.kind),
                            Err(Error::ObjectNotFound(_)) => break None,
                            Err(err) => return Err(err),
                        },
                    }
                }
            }
        };
        let code = Self::encode(kind);
        for row in chain {
            self.kinds[row] = code;
        }
        Ok(kind)
    }
}

/// The blobs of `wanted` some tree or tag stored in a promisor pack names: Git's
/// one-hop rule for a promised object. Stops reading once every one is found.
///
/// Trees are read breadth first from the root trees of the commits in promisor
/// packs, so the usual case, large files a few directories down the history the
/// clone fetched, is settled after reading the top levels. Promisor-pack trees and
/// tags that walk does not reach (a fetch by id, a tree only a non-promisor tree
/// names) are read afterwards, until every blob is found or none is left.
fn promised_by_promisor_packs(
    odb: &Odb,
    packs: &[PromisorPack],
    wanted: &OidHashSet,
) -> Result<OidHashSet> {
    let mut found = oid_hash_set();
    if wanted.is_empty() {
        return Ok(found);
    }
    let oid_len = odb.hash_algo().len();
    let mut read: Vec<Vec<bool>> = packs.iter().map(|_| Vec::new()).collect();
    let mut referrer = |pack_no: usize,
                        row: usize,
                        queue: Option<&mut std::collections::VecDeque<ObjectId>>,
                        found: &mut OidHashSet|
     -> Result<()> {
        let seen = &mut read[pack_no];
        if seen.is_empty() {
            *seen = vec![false; packs[pack_no].idx.len()];
        }
        if std::mem::replace(&mut seen[row], true) {
            return Ok(());
        }
        let idx = &packs[pack_no].idx;
        let obj = crate::pack::read_object_at(idx, idx.offset_at(row)?)?;
        match obj.kind {
            ObjectKind::Tree => {
                let mut queue = queue;
                for entry in TreeRefIter::with_oid_len(&obj.data, oid_len) {
                    let entry = entry?;
                    match entry.mode & 0o170000 {
                        0o040000 => {
                            if let Some(q) = queue.as_deref_mut() {
                                q.push_back(entry.oid);
                            }
                        }
                        0o160000 => {}
                        _ if wanted.contains(&entry.oid) => {
                            found.insert(entry.oid);
                        }
                        _ => {}
                    }
                }
            }
            ObjectKind::Tag => {
                if let Some(target) = crate::objects::tag_object_line_oid(&obj.data) {
                    if wanted.contains(&target) {
                        found.insert(target);
                    }
                }
            }
            ObjectKind::Blob | ObjectKind::Commit => {}
        }
        Ok(())
    };
    let locate = |oid: &ObjectId| {
        packs
            .iter()
            .enumerate()
            .find_map(|(n, p)| p.idx.find_position(oid).map(|row| (n, row)))
    };

    let mut queue: std::collections::VecDeque<ObjectId> = std::collections::VecDeque::new();
    for pack in packs {
        for &row in &pack.commits {
            let obj = crate::pack::read_object_at(&pack.idx, pack.idx.offset_at(row as usize)?)?;
            if obj.kind == ObjectKind::Commit {
                queue.push_back(CommitRef::parse(&obj.data)?.tree());
            }
        }
    }
    let mut trees_seen = oid_hash_set();
    while let Some(tree) = queue.pop_front() {
        if found.len() == wanted.len() {
            return Ok(found);
        }
        if !trees_seen.insert(tree) {
            continue;
        }
        // A tree outside promisor packs names nothing promised; the scan below still
        // reads any promisor-pack trees under it.
        if let Some((pack_no, row)) = locate(&tree) {
            referrer(pack_no, row, Some(&mut queue), &mut found)?;
        }
    }
    drop(trees_seen);
    for (pack_no, pack) in packs.iter().enumerate() {
        for &row in &pack.referrers {
            if found.len() == wanted.len() {
                return Ok(found);
            }
            referrer(pack_no, row as usize, None, &mut found)?;
        }
    }
    Ok(found)
}

/// Marks the candidate blobs checkouts and recent `HEAD`s use. Trees are read
/// once across all walks; missing objects are skipped, never fetched.
struct ProtectionWalk<'a> {
    odb: &'a Odb,
    wanted: &'a OidHashSet,
    trees_seen: OidHashSet,
    hits: OidHashSet,
}

impl<'a> ProtectionWalk<'a> {
    fn new(odb: &'a Odb, wanted: &'a OidHashSet) -> Self {
        Self {
            odb,
            wanted,
            trees_seen: oid_hash_set(),
            hits: oid_hash_set(),
        }
    }

    fn mark(&mut self, oid: ObjectId) {
        if self.wanted.contains(&oid) {
            self.hits.insert(oid);
        }
    }

    fn walk_tree(&mut self, root: ObjectId) -> Result<()> {
        let oid_len = self.odb.hash_algo().len();
        let mut stack = vec![root];
        while let Some(tree) = stack.pop() {
            if !self.trees_seen.insert(tree) {
                continue;
            }
            let Some(obj) = self.odb.try_read_once(&tree)? else {
                continue;
            };
            if obj.kind != ObjectKind::Tree {
                continue;
            }
            for entry in TreeRefIter::with_oid_len(&obj.data, oid_len) {
                let entry = entry?;
                match entry.mode & 0o170000 {
                    0o040000 => stack.push(entry.oid),
                    0o160000 => {}
                    _ => self.mark(entry.oid),
                }
            }
        }
        Ok(())
    }

    fn walk_commit(&mut self, commit: ObjectId) -> Result<()> {
        if commit.is_zero() {
            return Ok(());
        }
        let Some(obj) = self.odb.try_read_once(&commit)? else {
            return Ok(());
        };
        if obj.kind != ObjectKind::Commit {
            return Ok(());
        }
        let tree = CommitRef::parse(&obj.data)?.tree();
        self.walk_tree(tree)
    }

    /// Every non-bare worktree's own index (sparse directories included) and `HEAD`
    /// tree, plus the `GIT_INDEX_FILE` index when one is set.
    fn mark_checkouts(&mut self, repo: &Repository) -> Result<()> {
        use crate::state::HeadState;
        for wt in crate::worktree::list_worktrees(repo)? {
            if wt.is_bare {
                continue;
            }
            self.mark_index(repo, &wt.admin_dir.join("index"))?;
            if let HeadState::Detached { oid } | HeadState::Branch { oid: Some(oid), .. } = wt.head
            {
                self.walk_commit(oid)?;
            }
        }
        // A `GIT_INDEX_FILE` index is in use too, besides the worktree's own.
        if repo.environment().git_index_file.is_some() {
            self.mark_index(repo, &repo.index_path_for_env()?)?;
        }
        Ok(())
    }

    /// The entries of the index at `path` (none when it does not exist), sparse
    /// directories walked as trees.
    fn mark_index(&mut self, repo: &Repository, path: &Path) -> Result<()> {
        let index = repo.load_index_at_preserving_sparse(
            path,
            crate::index::IndexParseContext::from_policy(repo.thread_policy()),
        )?;
        for entry in index.entries() {
            match entry.mode & 0o170000 {
                0o040000 => self.walk_tree(entry.oid)?,
                0o160000 => {}
                _ => self.mark(entry.oid),
            }
        }
        Ok(())
    }

    /// The commits each non-bare worktree's `HEAD` reflog named at or after `cutoff`.
    fn mark_recent_heads(&mut self, repo: &Repository, cutoff: OffsetDateTime) -> Result<()> {
        for wt in crate::worktree::list_worktrees(repo)? {
            if wt.is_bare {
                continue;
            }
            for entry in crate::reflog::read_reflog(&wt.admin_dir, "HEAD")? {
                if entry.signature().is_some_and(|sig| sig.time >= cutoff) {
                    self.walk_commit(entry.old_oid)?;
                    self.walk_commit(entry.new_oid)?;
                }
            }
        }
        Ok(())
    }
}

/// Remove `drop_set` from each `affected` pack: delete a pack that held nothing
/// else, rewrite the others. New packs are installed before old ones go.
#[allow(clippy::too_many_arguments)]
fn rewrite_packs(
    repo: &Repository,
    config: &ConfigSet,
    opts: &DropPromisedBlobsOptions,
    packs: &[PromisorPack],
    affected: &[u32],
    drop_set: &OidHashSet,
    report: &mut DropPromisedBlobsReport,
) -> Result<()> {
    let odb = repo.odb();
    let pack_dir = odb.objects_dir().join("pack");
    let had_midx =
        pack_dir.join("multi-pack-index").is_file() || pack_dir.join("multi-pack-index.d").is_dir();
    let env = repo.environment().clone();
    let pack_opts = PackBuildOptions::for_local_copy(Some(config));
    let mut bytes_before = 0u64;
    let mut bytes_after = 0u64;
    let mut stems: std::collections::HashSet<std::ffi::OsString> = std::collections::HashSet::new();
    for &p in affected {
        let pack = &packs[p as usize];
        let old = &pack.idx.pack_path;
        bytes_before += pack_files_size(old);
        let mut keep: Vec<(u64, ObjectId)> = Vec::new();
        for entry in pack.idx.iter() {
            let oid = ObjectId::from_bytes(entry.oid())?;
            if !drop_set.contains(&oid) {
                keep.push((entry.offset()?, oid));
            }
        }
        // An old pack is removed only once nothing in it is still needed or its
        // replacement is installed.
        let old_stem = old.file_stem().map(|stem| stem.to_os_string());
        if keep.is_empty() {
            stems.extend(old_stem);
            report.packs_removed += 1;
            continue;
        }
        keep.sort_unstable();
        let objects: Vec<ObjectId> = keep.into_iter().map(|(_, oid)| oid).collect();
        let built = super::write_staged_repack_pack(
            odb,
            &pack_dir,
            &objects,
            &HashMap::new(),
            &pack_opts,
            opts.threads,
            Some(config),
        )?;
        let Some(staging) = built.staging_path.as_ref() else {
            continue;
        };
        let trusted_rows = (!built.index_rows.is_empty()).then_some(built.index_rows);
        let ingested = install_pack_path(
            staging,
            odb,
            &IngestPackOptions {
                threads: opts.threads,
                merged_config: Some(config.clone()),
                environment: env.clone(),
                skip_post_index_verify: trusted_rows.is_some(),
                trusted_index_rows: trusted_rows,
                mark_promisor_pack: true,
                ..IngestPackOptions::default()
            },
        )?;
        let Some(new_pack) = ingested.pack_path else {
            continue;
        };
        // Keep the ref lines fetch wrote, as Git's own repack does not.
        if let Ok(lines) = std::fs::read(old.with_extension("promisor")) {
            std::fs::write(new_pack.with_extension("promisor"), lines)?;
        }
        if config.pack_write_reverse_index_default(&env) {
            crate::pack_rev::write_pack_rev_sidecar_for_index_path(
                &new_pack.with_extension("idx"),
            )?;
        }
        // The blobs that stay were fetched when the old pack was: keep that time.
        if let Some(mtime) = pack.mtime {
            filetime::set_file_mtime(&new_pack, filetime::FileTime::from_system_time(mtime))?;
        }
        crate::index_pack::sync_installed_pack(&new_pack)?;
        bytes_after += pack_files_size(&new_pack);
        if new_pack.file_stem() != old.file_stem() {
            stems.extend(old_stem);
        }
        report.packs_rewritten += 1;
    }
    if had_midx {
        crate::midx::clear_pack_midx_state(&pack_dir)?;
    }
    // Drop cached mappings before deleting pack files (Windows).
    odb.invalidate_packs();
    super::remove_pack_files_with_stems(&pack_dir, &stems)?;
    crate::pack::clear_pack_cache();
    odb.invalidate_packs();
    odb.reprepare_local_packs();
    let packs_left = odb.local_pack_indexes()?.len();
    if had_midx && packs_left > 0 {
        super::write_midx(repo, &super::MidxOptions::default())?;
    }
    report.bytes_reclaimed = bytes_before.saturating_sub(bytes_after);
    Ok(())
}

/// Bytes of a pack with its index and reverse index.
fn pack_files_size(pack: &Path) -> u64 {
    ["pack", "idx", "rev"]
        .iter()
        .filter_map(|ext| std::fs::metadata(pack.with_extension(ext)).ok())
        .map(|m| m.len())
        .sum()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::config::{ConfigFile, ConfigScope};

    fn config(text: &str) -> ConfigSet {
        let file = ConfigFile::parse(Path::new("test.config"), text, ConfigScope::Local).unwrap();
        let mut set = ConfigSet::new();
        set.merge(&file);
        set
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    #[test]
    fn defaults_are_one_mebibyte_and_two_weeks() {
        let opts = DropPromisedBlobsOptions::from_config(&config(""), now())
            .unwrap()
            .unwrap();
        assert_eq!(opts.min_size, DEFAULT_DROP_MIN_SIZE);
        assert_eq!(opts.keep_newer_than, Some(now() - time::Duration::weeks(2)));
        assert_eq!(opts.now, now());
        assert!(!opts.dry_run);
    }

    #[test]
    fn min_size_takes_units_and_rejects_junk() {
        let min = |v: &str| {
            DropPromisedBlobsOptions::from_config(
                &config(&format!(
                    "[maintenance \"drop-large-blobs\"]\n\tminSize = {v}\n"
                )),
                now(),
            )
            .map(|o| o.unwrap().min_size)
        };
        assert_eq!(min("100k").unwrap(), 100 * 1024);
        assert_eq!(min("1m").unwrap(), 1 << 20);
        assert_eq!(min("0").unwrap(), 0);
        for bad in ["lots", "-1"] {
            assert!(
                matches!(
                    min(bad),
                    Err(Error::Config(ConfigError::BadNumericValue { .. }))
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn min_size_follows_the_smallest_promisor_blob_limit() {
        let cfg = config(
            "[remote \"origin\"]\n\turl = /o\n\tpromisor = true\n\tpartialclonefilter = blob:limit=200k\n\
             [remote \"lop\"]\n\turl = /l\n\tpromisor = true\n\tpartialclonefilter = blob:limit=50k\n\
             [remote \"none\"]\n\turl = /n\n\tpromisor = true\n\tpartialclonefilter = blob:none\n",
        );
        let opts = DropPromisedBlobsOptions::from_config(&cfg, now())
            .unwrap()
            .unwrap();
        assert_eq!(opts.min_size, 50 * 1024);
        let blobless = config(
            "[remote \"origin\"]\n\turl = /o\n\tpromisor = true\n\tpartialclonefilter = blob:none\n",
        );
        let opts = DropPromisedBlobsOptions::from_config(&blobless, now())
            .unwrap()
            .unwrap();
        assert_eq!(opts.min_size, DEFAULT_DROP_MIN_SIZE);
    }

    #[test]
    fn expire_values() {
        let expire = |v: &str| {
            DropPromisedBlobsOptions::from_config(
                &config(&format!(
                    "[maintenance \"drop-large-blobs\"]\n\texpire = {v}\n"
                )),
                now(),
            )
        };
        assert_eq!(expire("now").unwrap().unwrap().keep_newer_than, None);
        assert_eq!(expire("all").unwrap().unwrap().keep_newer_than, None);
        assert!(expire("never").unwrap().is_none());
        assert!(expire("false").unwrap().is_none());
        assert_eq!(
            expire("3.days.ago").unwrap().unwrap().keep_newer_than,
            Some(now() - time::Duration::days(3))
        );
        assert!(matches!(
            expire("not a date at all"),
            Err(Error::Config(ConfigError::InvalidDate { key, value }))
                if key == EXPIRE_KEY && value == "not a date at all"
        ));
    }

    #[test]
    fn expiry_keywords_dates_and_junk() {
        assert_eq!(DropExpiry::parse(" Never ", now()), Some(DropExpiry::Never));
        assert_eq!(DropExpiry::parse("false", now()), Some(DropExpiry::Never));
        assert_eq!(
            DropExpiry::parse("NOW", now()),
            Some(DropExpiry::Immediately)
        );
        assert_eq!(
            DropExpiry::parse("all", now()),
            Some(DropExpiry::Immediately)
        );
        assert_eq!(
            DropExpiry::parse("1.week.ago", now()),
            Some(DropExpiry::KeepNewerThan(now() - time::Duration::weeks(1)))
        );
        for junk in ["", "   ", "not a date at all"] {
            assert_eq!(DropExpiry::parse(junk, now()), None, "{junk:?}");
        }
    }

    #[test]
    fn overrides_replace_only_their_own_key() {
        let cfg = config("[maintenance \"drop-large-blobs\"]\n\texpire = never\n\tminSize = 10m\n");
        assert!(DropPromisedBlobsOptions::from_config(&cfg, now())
            .unwrap()
            .is_none());
        // An expiry override runs the task, and minSize still comes from config.
        let opts = DropPromisedBlobsOptions::from_config_with_overrides(
            &cfg,
            now(),
            None,
            Some(DropExpiry::Immediately),
        )
        .unwrap()
        .unwrap();
        assert_eq!(opts.min_size, 10 << 20);
        assert_eq!(opts.keep_newer_than, None);
        let opts = DropPromisedBlobsOptions::from_config_with_overrides(
            &cfg,
            now(),
            Some(5),
            Some(DropExpiry::KeepNewerThan(now())),
        )
        .unwrap()
        .unwrap();
        assert_eq!((opts.min_size, opts.keep_newer_than), (5, Some(now())));
        assert!(DropPromisedBlobsOptions::from_config_with_overrides(
            &cfg,
            now(),
            Some(5),
            Some(DropExpiry::Never)
        )
        .unwrap()
        .is_none());

        // An overridden key is not read, so its bad value does not matter.
        let bad =
            config("[maintenance \"drop-large-blobs\"]\n\texpire = bogus\n\tminSize = lots\n");
        let opts = DropPromisedBlobsOptions::from_config_with_overrides(
            &bad,
            now(),
            Some(7),
            Some(DropExpiry::Immediately),
        )
        .unwrap()
        .unwrap();
        assert_eq!(opts.min_size, 7);
        assert!(matches!(
            DropPromisedBlobsOptions::from_config_with_overrides(
                &bad,
                now(),
                None,
                Some(DropExpiry::Immediately)
            ),
            Err(Error::Config(ConfigError::BadNumericValue { .. }))
        ));
        assert!(matches!(
            DropPromisedBlobsOptions::from_config_with_overrides(&bad, now(), Some(7), None),
            Err(Error::Config(ConfigError::InvalidDate { .. }))
        ));
    }

    #[test]
    fn gc_plan_checks_config_and_needs_a_time() {
        let plan = |text: &str, now_unix: i64| plan_drop_for_gc(&config(text), now_unix);
        let on = "[maintenance \"drop-large-blobs\"]\n\tenabled = true\n";
        assert!(matches!(
            plan("", 0).unwrap(),
            GcDropPlan::Skip(DropPromisedBlobsSkip::Disabled)
        ));
        assert!(matches!(
            plan(&format!("{on}\texpire = never\n"), 1_700_000_000).unwrap(),
            GcDropPlan::Skip(DropPromisedBlobsSkip::Disabled)
        ));
        let GcDropPlan::Run(opts) = plan(on, 1_700_000_000).unwrap() else {
            panic!("runs with a time");
        };
        assert_eq!(opts.now, now());
        // No time: skipped, not measured against a clock read here or the epoch.
        for no_time in [0, i64::MAX] {
            assert!(matches!(
                plan(on, no_time).unwrap(),
                GcDropPlan::Skip(DropPromisedBlobsSkip::NoCurrentTime)
            ));
        }
        // Bad values fail before any phase; without a time only minSize is checked.
        for (bad, now_unix) in [
            ("\texpire = bogus\n", 1_700_000_000),
            ("\tminSize = lots\n", 1_700_000_000),
            ("\tminSize = lots\n", 0),
        ] {
            assert!(
                matches!(plan(&format!("{on}{bad}"), now_unix), Err(Error::Config(_))),
                "{bad}"
            );
        }
        assert!(matches!(
            plan("[maintenance \"drop-large-blobs\"]\n\tenabled = maybe\n", 1),
            Err(Error::Config(_))
        ));
    }

    #[test]
    fn enabled_is_off_by_default_and_must_be_a_boolean() {
        assert!(!drop_large_blobs_enabled(&config("")).unwrap());
        let on = config("[maintenance \"drop-large-blobs\"]\n\tenabled = true\n");
        assert!(drop_large_blobs_enabled(&on).unwrap());
        let bad = config("[maintenance \"drop-large-blobs\"]\n\tenabled = sometimes\n");
        assert!(matches!(
            drop_large_blobs_enabled(&bad),
            Err(Error::Config(_))
        ));
    }

    #[test]
    fn kept_counts_total() {
        let kept = KeptBlobCounts {
            kept_pack: 1,
            outside_promisor_packs: 2,
            checked_out: 3,
            recently_checked_out: 4,
            recently_fetched: 5,
            not_promised_elsewhere: 6,
            not_in_promisor: 7,
            unverified: 8,
        };
        assert_eq!(kept.total(), 36);
    }
}
