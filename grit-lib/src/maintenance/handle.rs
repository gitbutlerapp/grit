//! Repository-scoped maintenance via [`Maintenance`].
use crate::error::Result;
use crate::fsck::{FsckOptions, FsckReport};
use crate::gc::PruneStats;
use crate::objects::ObjectId;
use crate::progress::{CancelToken, ProgressSink};
use crate::repo::Repository;

use super::{
    AutoGcOutcome, AutoGcReason, CommitGraphBuild, CommitGraphOptions, CommitGraphWriteReport,
    DropPromisedBlobsOptions, DropPromisedBlobsReport, GcLock, GcLockAttempt, GcLockHolder,
    GcOptions, GcStats, MidxOptions, MidxWriteReport, OffloadOptions, OffloadReport,
    PackRefsOptions, PackRefsReport, PruneOptions, RepackOptions, RepackReport, RepackStats,
};

/// Garbage collection, repacking, commit-graph and MIDX writes, fsck and the `gc.pid` lock
/// for one repository.
///
/// Long-running operations take a [`ProgressSink`] and a [`CancelToken`] as their last two
/// parameters; pass `&mut NullProgress` and `&CancelToken::never()` when neither matters.
///
/// # Examples
///
/// ```
/// use grit_lib::maintenance::{GcOptions, RepackOptions};
/// use grit_lib::objects::ObjectKind;
/// use grit_lib::progress::{CancelToken, NullProgress};
/// use grit_lib::repo::{InitOptions, Repository};
///
/// # fn main() -> grit_lib::error::Result<()> {
/// let dir = tempfile::tempdir()?;
/// let (repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
/// repo.objects().write(ObjectKind::Blob, b"loose\n")?;
///
/// let maintenance = repo.maintenance();
/// let stats = maintenance.repack_all(
///     &RepackOptions::default(),
///     &mut NullProgress,
///     &CancelToken::never(),
/// )?;
/// assert_eq!(stats.loose_before, 1);
/// maintenance.gc(&GcOptions::default(), &mut NullProgress, &CancelToken::never())?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Copy, Clone)]
pub struct Maintenance<'repo> {
    repo: &'repo Repository,
}

impl<'repo> Maintenance<'repo> {
    /// Bind maintenance operations to `repo`.
    #[inline]
    #[must_use]
    pub fn new(repo: &'repo Repository) -> Self {
        Self { repo }
    }

    /// Underlying repository.
    #[inline]
    #[must_use]
    pub fn repository(&self) -> &'repo Repository {
        self.repo
    }

    /// Run garbage collection: pack refs, expire reflogs, repack, prune unreachable loose
    /// objects, drop large promised blobs when enabled, and write the commit-graph, as
    /// [`GcOptions`] selects. Takes the `gc.pid` lock for the whole run.
    ///
    /// # Errors
    ///
    /// [`crate::repo::RepositoryError::MaintenanceRunning`] when another live run holds `gc.pid`,
    /// [`crate::error::Error::Cancelled`] when `cancel` fires between phases, and pack,
    /// prune, ref, config and I/O failures from the phases.
    pub fn gc(
        &self,
        opts: &GcOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<GcStats> {
        super::gc(self.repo, opts, progress, cancel)
    }

    /// Run [`Self::gc`] only when [`Self::auto_gc_needed`] reports a reason (Git's
    /// `gc --auto`), as an incremental repack that keeps existing packs.
    ///
    /// # Errors
    ///
    /// Same as [`Self::gc`].
    pub fn gc_auto(
        &self,
        opts: &GcOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<AutoGcOutcome> {
        super::gc_auto(self.repo, opts, progress, cancel)
    }

    /// Whether `gc.auto` / `gc.autoPackLimit` thresholds call for an automatic gc.
    ///
    /// Returns `None` when gc is not needed (or `gc.auto` is 0).
    ///
    /// # Errors
    ///
    /// Config load failures and I/O errors while counting loose objects and packs.
    pub fn auto_gc_needed(&self) -> Result<Option<AutoGcReason>> {
        super::auto_gc_needed(self.repo)
    }

    /// Repack reachable objects into one pack and remove redundant packs and loose objects,
    /// returning summary counters.
    ///
    /// # Errors
    ///
    /// Same as [`Self::repack_all`].
    pub fn repack(
        &self,
        opts: &RepackOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<RepackReport> {
        super::repack(self.repo, opts, progress, cancel)
    }

    /// Repack reachable objects into one pack (or, with [`RepackOptions::incremental`], pack
    /// only reachable loose objects) and return detailed statistics.
    ///
    /// Indexing the new pack reports [`crate::progress::ProgressPhase::IndexingObjects`] and
    /// [`crate::progress::ProgressPhase::ResolvingDeltas`] through `progress`.
    ///
    /// # Errors
    ///
    /// [`crate::repo::RepositoryError::UnsupportedObjectStore`] without a files-backed
    /// object store, [`crate::error::Error::Cancelled`] when `cancel` fires, and pack, prune
    /// and I/O failures.
    pub fn repack_all(
        &self,
        opts: &RepackOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<RepackStats> {
        super::repack_all(self.repo, opts, progress, cancel)
    }

    /// Delete unreachable loose objects older than the cutoff in `opts` (`gc.pruneExpire`
    /// when unset).
    ///
    /// `cancel` is polled before the reachability walk and between batches of deletions;
    /// objects already deleted stay deleted (they were unreachable). No progress phases are
    /// reported yet; `progress` is accepted for future use.
    ///
    /// # Errors
    ///
    /// Ref, reflog, index and I/O failures while collecting roots or deleting objects, and
    /// [`crate::error::Error::Cancelled`] when `cancel` fires.
    pub fn prune_loose_objects(
        &self,
        opts: &PruneOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<PruneStats> {
        super::prune_loose_objects(self.repo, opts, progress, cancel)
    }

    /// Build a reachable commit-graph in memory, for any object store.
    ///
    /// The commit walk reports [`crate::progress::ProgressPhase::WalkingHistory`] through
    /// `progress` and polls `cancel` every few hundred commits.
    ///
    /// # Errors
    ///
    /// Ref and object read failures while walking commits, and
    /// [`crate::error::Error::Cancelled`] when `cancel` fires.
    pub fn build_commit_graph(
        &self,
        opts: &CommitGraphOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<CommitGraphBuild> {
        super::build_commit_graph(self.repo, opts, progress, cancel)
    }

    /// Write `objects/info/commit-graph` for every commit reachable from refs.
    ///
    /// The commit walk reports [`crate::progress::ProgressPhase::WalkingHistory`] through
    /// `progress`. `cancel` is polled during the walk and once more before the new graph
    /// replaces the old one; a cancelled run leaves the existing graph in place.
    ///
    /// # Errors
    ///
    /// [`crate::repo::RepositoryError::UnsupportedObjectStore`] without a files-backed
    /// object store, walk and I/O failures, and [`crate::error::Error::Cancelled`] when
    /// `cancel` fires.
    pub fn write_commit_graph(
        &self,
        opts: &CommitGraphOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<CommitGraphWriteReport> {
        super::write_commit_graph(self.repo, opts, progress, cancel)
    }

    /// Write or refresh the multi-pack-index for the pack directory.
    ///
    /// `cancel` is polled before the index is built; once writing starts it runs to
    /// completion, so the pack directory never holds a half-written index. No progress
    /// phases are reported yet; `progress` is accepted for future use.
    ///
    /// # Errors
    ///
    /// [`crate::repo::RepositoryError::UnsupportedObjectStore`] without a files-backed
    /// object store, an invalid `midx.version`, pack read and I/O failures, and
    /// [`crate::error::Error::Cancelled`] when `cancel` fires.
    pub fn write_midx(
        &self,
        opts: &MidxOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<MidxWriteReport> {
        super::write_midx(self.repo, opts, progress, cancel)
    }

    /// Pack loose refs into `packed-refs` (files backend) or compact reftable stacks.
    ///
    /// Unlike the other maintenance steps this takes no progress sink or cancel token:
    /// it rewrites one file (or one reftable stack) under a single lock, and stopping
    /// half way would gain nothing over letting it finish.
    ///
    /// # Errors
    ///
    /// Ref-store, locking and I/O failures.
    pub fn pack_refs(&self, opts: &PackRefsOptions) -> Result<PackRefsReport> {
        super::pack_refs(self.repo, opts)
    }

    /// Move blobs the filter in `opts` leaves out to a large-object promisor remote and drop
    /// them locally.
    ///
    /// Copying the blobs into the large-object promisor's pack reports
    /// [`crate::progress::ProgressPhase::IndexingObjects`] and
    /// [`crate::progress::ProgressPhase::ResolvingDeltas`] through `progress`. `cancel` is
    /// polled until the blobs are safely in the promisor; once this repository starts
    /// dropping its copies the run completes.
    ///
    /// # Errors
    ///
    /// [`crate::error::ObjectError::Offload`] for the [`super::OffloadError`] cases, read,
    /// write and config failures, and [`crate::error::Error::Cancelled`] when `cancel` fires.
    pub fn offload_blobs(
        &self,
        opts: &OffloadOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<OffloadReport> {
        crate::offload::offload_blobs(self.repo, opts, progress, cancel)
    }

    /// Drop large promised blobs that no recent checkout needed.
    ///
    /// `cancel` is polled per promisor pack while scanning and before any pack is
    /// rewritten; once rewriting starts the run completes. No progress phases are reported
    /// yet; `progress` is accepted for future use.
    ///
    /// # Errors
    ///
    /// [`crate::repo::RepositoryError::MaintenanceRunning`] when `gc.pid` is held, read, write
    /// and config failures, and [`crate::error::Error::Cancelled`] when `cancel` fires.
    pub fn drop_promised_blobs(
        &self,
        opts: &DropPromisedBlobsOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<DropPromisedBlobsReport> {
        super::drop_blobs::drop_promised_blobs(self.repo, opts, progress, cancel)
    }

    /// Object ids that must stay reachable during repack, prune and fsck walks: ref tips,
    /// `HEAD`, every reflog old/new id, and every blob and tree recorded in each worktree's
    /// index (gitlinks excluded).
    ///
    /// # Errors
    ///
    /// Ref, reflog and index load failures.
    pub fn reachability_roots(&self) -> Result<Vec<ObjectId>> {
        crate::fsck::reachability_roots(self.repo)
    }

    /// Check object store connectivity, refs and (optionally) pack indexes.
    ///
    /// Progress is reported through `progress`; the library never prints. A custom primary
    /// object store is validated through the objects it lists.
    ///
    /// # Errors
    ///
    /// Object-store, ref or index failures that abort the run, and
    /// [`crate::error::Error::Cancelled`] when `cancel` fires.
    pub fn fsck(
        &self,
        opts: &FsckOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<FsckReport> {
        crate::fsck::fsck(self.repo, opts, progress, cancel)
    }

    /// Try to take the `gc.pid` maintenance lock in the common git directory without waiting.
    ///
    /// `now_unix` is the wall clock used to decide whether an existing `gc.pid` is stale.
    ///
    /// # Errors
    ///
    /// I/O failures while creating, writing or renaming the lock file.
    pub fn try_lock(&self, now_unix: i64) -> Result<GcLockAttempt> {
        GcLock::try_acquire(self.repo, now_unix)
    }

    /// The maintenance run currently holding `gc.pid`, without taking the lock.
    ///
    /// Uses the same staleness rules as [`Self::try_lock`], so `None` means an attempt would
    /// most likely succeed.
    #[must_use]
    pub fn lock_holder(&self, now_unix: i64) -> Option<GcLockHolder> {
        GcLock::current_holder(self.repo, now_unix)
    }
}
