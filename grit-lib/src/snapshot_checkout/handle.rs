//! [`RepositorySnapshotCheckouts`]: snapshot-checkout operations on one open repository.

use super::*;
use crate::ident::Signature;
use crate::porcelain::pull::PullOptions;
use crate::remote::HttpClientFactory;
use crate::transfer::{FetchOptions, PushOutcome, TransferStats};
use time::OffsetDateTime;

/// Snapshot-checkout operations for one open repository, from
/// [`Repository::snapshot_checkouts`].
///
/// A serving repository makes snapshots, projects its branches for checkouts, grafts
/// check-ins back onto its history, integrates branches into its target, and keeps
/// slices ([`Self::make_snapshot`], [`Self::project_branch`], [`Self::graft_checkin_branch`],
/// [`Self::set_checked_in_branch`], [`Self::integrate`], [`Self::edit_slices`]). A checkout
/// (a repository [`materialize`] built) refreshes onto new snapshots, fetches projected
/// branches, gives its commits change ids and pushes them for check-in ([`Self::refresh`],
/// [`Self::fetch_branch`], [`Self::give_change_ids`], [`Self::push_for_checkin`]). See the
/// [`snapshot_checkout`](crate::remote::snapshot_checkout) module docs for the model.
#[derive(Debug, Copy, Clone)]
pub struct RepositorySnapshotCheckouts<'repo> {
    repo: &'repo Repository,
}

impl<'repo> RepositorySnapshotCheckouts<'repo> {
    pub(crate) fn new(repo: &'repo Repository) -> Self {
        Self { repo }
    }

    /// Borrow the underlying repository.
    #[must_use]
    pub fn repository(&self) -> &'repo Repository {
        self.repo
    }

    /// Write the snapshot of `base` filtered to `paths` (see [`filtered_tree`])
    /// on `parent` into this repository, with the number and size of its files.
    ///
    /// A snapshot holds every file it names, so in a repository whose large blobs
    /// live on a promisor remote, the ones the snapshot needs are fetched first
    /// ([`fetch_promised_blobs`](Self::fetch_promised_blobs)).
    ///
    /// # Errors
    ///
    /// [`CheckoutError::MissingPath`] for a path `base` lacks,
    /// [`CheckoutError::PromisedBlobs`] and [`CheckoutError::PromisedBlobsNotFetched`]
    /// for blobs on a promisor remote that cannot be fetched, and object read or
    /// write failures.
    pub fn make_snapshot(
        &self,
        base: ObjectId,
        paths: &[String],
        parent: Option<ObjectId>,
    ) -> Result<Snapshot> {
        super::make_snapshot(self.repo, base, paths, parent)
    }

    /// Make sure this repository stores every blob of the snapshot commit `snapshot`, fetching
    /// the ones it keeps on a promisor remote.
    ///
    /// A server whose large blobs were offloaded to a large-object promisor lacks
    /// them locally, but a snapshot (and its bundle) must hold every file. This
    /// follows upload-pack's rule for a client that accepted no promisor remote: the
    /// blobs are fetched from the repository's promisor remotes when lazy fetch is on,
    /// which the host decides by installing a
    /// [`PromisorFetcher`](crate::promisor_fetch::PromisorFetcher) on the repository
    /// (as [`crate::serve::ServeOptions::attach_promisor_fetcher`] does for
    /// upload-pack), and the request fails otherwise. Every missing blob is fetched in
    /// one request. A repository without promisor remotes is not checked.
    ///
    /// # Parameters
    ///
    /// - `snapshot`: a snapshot commit (or any commit) whose tree must be complete.
    ///
    /// # Returns
    ///
    /// How many blobs were fetched (`0` when nothing was missing).
    ///
    /// # Errors
    ///
    /// [`CheckoutError::PromisedBlobs`] when blobs are missing and the repository has
    /// no promisor fetcher, [`CheckoutError::PromisedBlobsNotFetched`] when the fetch
    /// failed or left some missing,
    /// [`crate::error::ObjectError::NotFound`] for a missing blob nothing promises,
    /// and object read failures.
    pub fn fetch_promised_blobs(&self, snapshot: ObjectId) -> Result<usize> {
        super::fetch_promised_blobs(self.repo, snapshot)
    }

    /// Local branches with work to check in: every branch whose tip is a commit
    /// on top of one of the checkout's snapshots (not a snapshot itself).
    ///
    /// # Errors
    ///
    /// Ref and ancestry failures.
    pub fn work_branches(&self, marker: &CheckoutMarker) -> Result<Vec<(String, ObjectId)>> {
        super::branches_with_work(self.repo, marker)
    }

    /// Project the server's branch `name` for `checkout`, record the new pairs in
    /// `known`, and point the checkout's `refs/checkouts/<id>/in/heads/<name>` at the
    /// projection for the checkout to fetch.
    ///
    /// The commits hidden are those of the branch the checkout follows
    /// ([`CheckoutInfo::branch`]), which refreshes bring as snapshots.
    ///
    /// # Parameters
    ///
    /// - `checkout`: the checkout the branch is shown to.
    /// - `known`: the repository's graft log
    ///   ([`super::CheckoutStore::graft_log`]).
    /// - `name`: the branch, without `refs/heads/`. Where the repository ignores
    ///   case (`core.ignorecase`), a name that differs from one of its branches
    ///   only in case is that branch.
    ///
    /// # Returns
    ///
    /// What the checkout fetches, named as the server spells the branch
    /// ([`BranchInfo::name`]), or `Ok(Err(..))` with why the branch can't be
    /// shown: an invalid name, the branch the checkout follows (in any case), a
    /// branch the server doesn't have, or the projection's refusals.
    ///
    /// # Errors
    ///
    /// Ref, walk, object and graft log failures.
    pub fn project_branch(
        &self,
        checkout: &CheckoutInfo,
        known: &GraftLog,
        name: &str,
    ) -> Result<std::result::Result<BranchInfo, ProjectError>> {
        super::project::project_branch(self.repo, checkout, known, name)
    }

    /// Check a branch of a check-in and graft it onto the real history.
    ///
    /// The branch is refused when its name is not a valid branch name; when it
    /// names, in any case, the branch the checkout was made from or the
    /// repository's current `target`; when its tip is not an object id or is not
    /// what the server has at [`out_ref`]; and when the graft fails
    /// (a graft).
    ///
    /// # Parameters
    ///
    /// - `checkout`: the checkout checking in.
    /// - `target`: the repository's target branch now ([`target_branch`](Self::target_branch)).
    /// - `branch`: the branch and the tip the client pushed.
    /// - `known`: the repository's graft log ([`super::CheckoutStore::graft_log`]),
    ///   so commits built on projected commits, or grafted before, graft onto the
    ///   real commits they stand for; `None` to graft from the snapshots alone.
    /// - `targets`: the tips of the target branch, where the graft's walk stops.
    ///
    /// # Returns
    ///
    /// The graft, or `Ok(Err(..))` with why the branch is refused. Once the
    /// branch is set, record [`Grafted::commits`] in `known`.
    ///
    /// # Errors
    ///
    /// Ref, object and graft log failures.
    pub fn graft_checkin_branch(
        &self,
        checkout: &CheckoutInfo,
        target: &str,
        branch: &CheckinBranch,
        known: Option<&GraftLog>,
        targets: &[ObjectId],
    ) -> Result<std::result::Result<Grafted, CheckinRefusal>> {
        super::checkin::graft_checkin_branch(self.repo, checkout, target, branch, known, targets)
    }

    /// Whether `grafted` has every commit of `previous` that the target lacks,
    /// matched by change key: a rebase of it, perhaps with more
    /// on top.
    ///
    /// It holds when every commit of `previous --not grafted <targets>` has a
    /// copy with its key among `grafted --not previous <targets>`, so commits the
    /// target already has never count as dropped. A copy counts only when its
    /// tree holds what the original changed (against its first parent) outside
    /// `paths`, the checkout's paths: the checkout never sees those files, so a
    /// teammate's amend that also changed one must not be dropped unnoticed.
    /// Inside the paths the copy's version wins, as the checkout's own edit of
    /// the change.
    ///
    /// # Parameters
    ///
    /// - `previous`: where the server's branch is.
    /// - `grafted`: what a check-in would set it to.
    /// - `targets`: the tips of the target branch.
    /// - `paths`: the checking-in checkout's paths (empty: the whole tree, so
    ///   nothing is outside them).
    ///
    /// # Errors
    ///
    /// Walk and object read failures.
    pub fn keeps_every_commit(
        &self,
        previous: ObjectId,
        grafted: ObjectId,
        targets: &[ObjectId],
        paths: &[String],
    ) -> Result<bool> {
        super::ids::keeps_every_commit(self.repo, previous, grafted, targets, paths)
    }

    /// Give a change id to every commit made with plain Git on the checkout's
    /// branches above its snapshots, so the server can tell a rebased copy of it
    /// from new work later.
    ///
    /// A commit without a `change-id` header is rewritten with the id
    /// [`ChangeId::derived_from_commit_id`](crate::change_id::ChangeId::derived_from_commit_id) gives its own commit id (what
    /// [`resolved_change_id`](crate::change_id::resolved_change_id) already reports for it, and what the server's graft
    /// would give it). A commit whose parent was rewritten is rewritten onto the
    /// new parent, keeping its id. Rewritten commits drop signatures, which no
    /// longer hold. Trees are unchanged, so the index and worktree stay as they
    /// are.
    ///
    /// Only branches with work on a snapshot ([`work_branches`](Self::work_branches)) are
    /// rewritten, never history under the snapshots, a remote-tracking ref or a
    /// tip that landed in the target ([`CheckoutMarker::landed`]). Each branch
    /// moves by compare-and-swap with a reflog entry (one that moved meanwhile
    /// is left alone), and so does a detached `HEAD` on a rewritten commit. The
    /// reflog entries are by the committer Git would log (from the repository's
    /// environment and config) at `now`.
    ///
    /// Nothing happens while an operation is in progress in any of the
    /// checkout's worktrees (a rebase, merge, cherry-pick or revert, one paused
    /// between the commits of a sequence too, am or bisect), whose state names
    /// the commits as they are, or when `grit.changeIds` is off
    /// ([`ChangeIdPolicy`](crate::change_id::ChangeIdPolicy)).
    ///
    /// # Parameters
    ///
    /// - `marker`: its marker.
    /// - `only`: the branches (without `refs/heads/`) to give ids on; `None` for
    ///   every branch with work.
    /// - `now`: the time the reflog entries record.
    ///
    /// # Returns
    ///
    /// How many commits were given an id (commits rewritten only because their
    /// parent was are not counted).
    ///
    /// # Errors
    ///
    /// Config, worktree, walk, object and ref failures.
    pub fn give_change_ids(
        &self,
        marker: &CheckoutMarker,
        only: Option<&[String]>,
        now: OffsetDateTime,
    ) -> Result<usize> {
        super::ids::give_change_ids(self.repo, marker, only, now)
    }

    /// The target branch of a served repository: the branch its `HEAD` names,
    /// without `refs/heads/`, or `main` when `HEAD` names no branch.
    ///
    /// # Returns
    ///
    /// The branch name; the branch itself may not exist yet.
    pub fn target_branch(&self) -> String {
        super::target::target_branch(self.repo)
    }

    /// Merge branch `name` into the target branch `target_ref` with a merge
    /// commit by `who`, when the merge is clean.
    ///
    /// The name must be a valid branch name other than the target's (in any
    /// case); `refs/heads/<name>` is read from the repository. A branch the
    /// target already has is left alone, and one that shares no history with
    /// the target is refused rather than merged against an empty tree. When the
    /// target moves while the merge is made, the merge is made again on its new
    /// tip. The move is logged as `integrate: <merge message>` where the
    /// repository keeps a reflog for the target, and a worktree that has the
    /// target checked out moves with it.
    ///
    /// # Parameters
    ///
    /// - `target_ref`: the target branch's full ref (`refs/heads/<target>`).
    /// - `name`: the branch to merge, without `refs/heads/`.
    /// - `who`: author and committer of the merge commit.
    ///
    /// # Returns
    ///
    /// What happened, or `Ok(Err(..))` with why the target was left alone.
    ///
    /// # Errors
    ///
    /// Object, ref, index and worktree failures.
    pub fn integrate(
        &self,
        target_ref: &str,
        name: &str,
        who: &Signature,
    ) -> Result<std::result::Result<Integration, TargetRefusal>> {
        super::target::integrate(self.repo, target_ref, name, who)
    }

    /// The slices at `commit` (none when it has no slices file).
    ///
    /// # Errors
    ///
    /// Object read failures and a malformed slices file.
    pub fn read_slices(&self, commit: ObjectId) -> Result<Vec<Slice>> {
        super::target::read_slices(self.repo, commit)
    }

    /// Change the slices file on the target branch `target_ref` with a commit by
    /// `who`, as `edit` says.
    ///
    /// The edit is checked against the slices and the tree at the target's tip:
    /// a new slice's name must be free, a changed or removed one must exist, and
    /// every path of a slice must be in the tree. When the target moves while
    /// the commit is made, the edit is checked and applied again on its new tip.
    /// The move is logged as `slice: <message>` where the repository keeps a
    /// reflog for the target, and a worktree that has the target checked out
    /// moves with it.
    ///
    /// # Parameters
    ///
    /// - `target_ref`: the target branch's full ref (`refs/heads/<target>`).
    /// - `edit`: the change; slice paths are taken as given (see
    ///   [`super::normalize_paths`]).
    /// - `who`: author and committer of the commit.
    ///
    /// # Returns
    ///
    /// What happened, with the target unchanged when the file came out the
    /// same, or `Ok(Err(..))` with why the target was left alone.
    ///
    /// # Errors
    ///
    /// Object, ref, index and worktree failures, and a malformed slices file.
    pub fn edit_slices(
        &self,
        target_ref: &str,
        edit: &SliceEdit,
        who: &Signature,
    ) -> Result<std::result::Result<SlicesEdited, TargetRefusal>> {
        super::target::edit_slices(self.repo, target_ref, edit, who)
    }

    /// Decide what the integration the server answered with `response`
    /// ([`CheckoutClient::integrate`]) means for this checkout, and record
    /// what landed. Call it right after the integration, then refresh the
    /// checkout ([`refresh`](Self::refresh)), then call [`Landing::move_off`].
    ///
    /// The work landed here when the branch the server merged into
    /// ([`IntegrateResponse::target`]) is exactly the one the checkout snapshots
    /// ([`CheckoutMarker::branch`]); a case variant may be another branch on the
    /// server, and treating it as this one would drop the branch's commits from
    /// the next snapshot. A merge and an answer that the target already had the
    /// branch both mean the tip this integration checked in, `checked_in`, is in
    /// the target, so it is recorded as landed: later
    /// refreshes never replay its commits. Otherwise nothing is recorded.
    ///
    /// # Parameters
    ///
    /// - `marker`: its marker, used when the one on disk can't be read.
    /// - `response`: the server's answer to the integration.
    /// - `checked_in`: the tip of the integrated branch this integration checked
    ///   in first (its raw, local commit), if any.
    ///
    /// # Returns
    ///
    /// [`Landing::Here`] or [`Landing::Elsewhere`].
    ///
    /// # Errors
    ///
    /// Marker write failures.
    pub fn land_integration(
        &self,
        marker: &CheckoutMarker,
        response: &IntegrateResponse,
        checked_in: Option<ObjectId>,
    ) -> Result<Landing> {
        super::landing::land_integration(self.repo, marker, response, checked_in)
    }

    /// Refuse to refresh a checkout that is in the middle of an operation, such
    /// as a refresh's rebase that stopped on a conflict: that has to be
    /// continued or aborted first. Check this before asking the server for a new
    /// snapshot.
    ///
    /// # Errors
    ///
    /// [`CheckoutError::OperationInProgress`] naming the operation.
    pub fn ensure_refreshable(&self) -> Result<()> {
        super::refresh::ensure_refreshable(self.repo)
    }

    /// Bring the checkout onto `info`, the snapshot the server answered
    /// [`super::CheckoutClient::update`] with.
    ///
    /// 0. With `HEAD` on a branch, gives that branch's commits made with plain
    ///    Git a change id ([`give_change_ids`](Self::give_change_ids); not with `options.check`), so the
    ///    rebase below keeps a key the server can match them by. Those moves are
    ///    logged at the time of the repository handle's clock
    ///    ([`Repository::identity_now`]), as the rebase's commits and reflog
    ///    entries are.
    /// 1. Fetches the snapshot from the marker's URL into [`upstream_ref`] and
    ///    records it in the marker.
    /// 2. With `HEAD` on a branch, integrates the snapshot as [`pull`](crate::porcelain::pull::pull) does with
    ///    `options` (a rebase unless `options.mode` says otherwise). A rebase that
    ///    conflicts stops in a resumable rebase session
    ///    ([`PullOptions::stop_on_conflict`]). Commits up to a tip that landed in
    ///    the target ([`CheckoutMarker::landed`]) are not replayed: a branch with
    ///    nothing above one moves to the snapshot keeping uncommitted changes
    ///    ([`crate::porcelain::pull::PullResult::Landed`]), and one with commits
    ///    above it rebases only those ([`FetchedUpstream::landed`](crate::porcelain::pull::FetchedUpstream::landed)). With `HEAD`
    ///    detached, nothing else moves.
    /// 3. Moves `refs/heads/<target>`, when it is not the current branch and has
    ///    no commits of its own, to the new snapshot, and every other branch but
    ///    the current one whose commits all landed (not with `options.check`).
    /// 4. Lists the other branches with work that still build on an older
    ///    snapshot, and forgets landed tips no branch builds on any more (not
    ///    with `options.check`).
    ///
    /// # Parameters
    ///
    /// - `marker`: its marker as it is now.
    /// - `info`: the checkout as the server has it after the update.
    /// - `options`: how to integrate (mode, autostash, check) and the fetch's
    ///   environment and threads; its remote, branch, fetch refspecs and
    ///   upstream are replaced.
    /// - `progress`, `cancel`, `http_factory`: as for [`pull`](crate::porcelain::pull::pull).
    ///
    /// # Errors
    ///
    /// [`CheckoutError::OperationInProgress`] before anything is fetched, the
    /// errors of fetching the snapshot, and pull or history errors when the branch
    /// could not be integrated (the new snapshot is fetched and recorded by then).
    pub fn refresh<P: Progress>(
        &self,
        marker: &CheckoutMarker,
        info: &CheckoutInfo,
        options: &PullOptions,
        progress: &mut P,
        cancel: &CancelToken,
        http_factory: Option<&dyn HttpClientFactory>,
    ) -> Result<RefreshOutcome> {
        super::refresh::refresh(
            self.repo,
            marker,
            info,
            options,
            progress,
            cancel,
            http_factory,
        )
    }

    /// Bring the local branch `info.name` onto the server's branch as the
    /// checkout sees it, `info.projected`, fetched already (see the module
    /// docs).
    ///
    /// First the branch's commits made with plain Git are given a change id
    /// ([`give_change_ids`](Self::give_change_ids), not with `check`), logged at the repository
    /// handle's clock ([`Repository::identity_now`]). The branch's own commits
    /// are then those above `from`, the newer of its merge base with the
    /// projection and the newest snapshot it builds on, past the copies of the
    /// server's commits right above it; other copies among them (same change key
    /// as one of the projection's commits above the snapshots, or same patch-id
    /// as one of the projection's commits the branch lacks) are left out of the
    /// rebase too.
    ///
    /// When the branch is not the current one, it is moved by compare-and-swap
    /// to the projection when it has nothing of its own, and otherwise `HEAD` is
    /// switched to it first (as `git rebase <upstream> <branch>` does) and it is
    /// rebased in place, with the worktree. When that rebase fails or refuses
    /// (local changes it would overwrite, say), `HEAD` is switched back.
    ///
    /// # Parameters
    ///
    /// - `marker`: its marker.
    /// - `info`: the server's answer to [`super::CheckoutClient::branch`], whose
    ///   projection must be at [`upstream_ref`]`(<name>)` here.
    /// - `check`: say what would happen without changing anything.
    ///
    /// # Errors
    ///
    /// [`CheckoutError::OperationInProgress`] while a rebase or merge is in
    /// progress; [`CheckoutError::BranchMoved`] when the projection is not at
    /// [`upstream_ref`]`(<name>)`; [`CheckoutError::UnrelatedBranch`] for a local
    /// branch that shares nothing with the projection;
    /// [`CheckoutError::RefKeptMoving`] when the branch moved meanwhile;
    /// branch and worktree errors for a branch checked out (or being rebased or
    /// bisected) in another worktree, or a switch that would overwrite local
    /// changes; [`PullError`](crate::porcelain::pull::PullError) and history errors when the rebase fails;
    /// [`CheckoutError::StillSwitched`] when it fails and `HEAD` can't be
    /// switched back; and ref, walk and object failures.
    pub fn refresh_branch(
        &self,
        marker: &CheckoutMarker,
        info: &BranchInfo,
        check: bool,
    ) -> Result<BranchRefresh> {
        super::branch_refresh::refresh_branch(self.repo, marker, info, check)
    }

    /// Push `branches` (each a branch name and its tip) to their [`out_ref`]s on
    /// the server the checkout came from, replacing what was there, for a
    /// check-in to graft.
    ///
    /// # Parameters
    ///
    /// - `marker`: its marker, which names the server and the checkout's id.
    /// - `branches`: the branches to push, by name (without `refs/heads/`) and tip.
    /// - `progress`, `cancel`: as for [`Remote::push`](crate::remote::Remote::push).
    /// - `http_factory`: builds the HTTP client, as for [`Remote::push`](crate::remote::Remote::push).
    ///
    /// # Returns
    ///
    /// The push's per-ref results; a ref the server refused is reported there,
    /// not as an error.
    ///
    /// # Errors
    ///
    /// [`RemoteError`](crate::remote::RemoteError) when the server can't be reached or read.
    pub fn push_for_checkin(
        &self,
        marker: &CheckoutMarker,
        branches: &[(String, ObjectId)],
        progress: &mut dyn Progress,
        cancel: &CancelToken,
        http_factory: Option<&dyn HttpClientFactory>,
    ) -> Result<PushOutcome> {
        super::transfer::push_for_checkin(
            self.repo,
            marker,
            branches,
            progress,
            cancel,
            http_factory,
        )
    }

    /// Fetch the server's branch as the checkout sees it, which `info` describes
    /// (the server's answer to [`super::CheckoutClient::branch`]), from
    /// [`in_ref`] into [`upstream_ref`]`(<name>)` (`refs/remotes/origin/<name>`)
    /// from the server at the marker's URL, and check that it is that
    /// projection.
    ///
    /// Nothing is fetched when the projection is already there. No tags are
    /// fetched, and nothing else.
    ///
    /// # Parameters
    ///
    /// - `marker`: its marker.
    /// - `info`: the projection the server described.
    /// - `fetch`: environment and thread settings for the fetch; its refspecs,
    ///   tags, prune and dry-run settings are replaced.
    /// - `progress`, `cancel`, `http_factory`: as for [`Remote::fetch`](crate::remote::Remote::fetch).
    ///
    /// # Returns
    ///
    /// What the fetch received.
    ///
    /// # Errors
    ///
    /// [`CheckoutError::Protocol`] for an answer naming no valid branch, or the
    /// branch the checkout follows, or no commit; [`RemoteError`](crate::remote::RemoteError) when
    /// the fetch fails; [`CheckoutError::BranchMoved`] when what arrived is not
    /// `info`'s projection.
    pub fn fetch_branch(
        &self,
        marker: &CheckoutMarker,
        info: &BranchInfo,
        fetch: FetchOptions,
        progress: &mut dyn Progress,
        cancel: &CancelToken,
        http_factory: Option<&dyn HttpClientFactory>,
    ) -> Result<TransferStats> {
        super::transfer::fetch_branch(
            self.repo,
            marker,
            info,
            fetch,
            progress,
            cancel,
            http_factory,
        )
    }

    /// Set the server's branch `update.name` to `update.grafted` when the check-in rules allow
    /// it, as one compare-and-swap against the tip they decided on (a new branch must still be
    /// missing), logged by `update.who` with `update.message` where the repository keeps a
    /// reflog for it.
    ///
    /// When the branch moves between deciding and setting, it decides again with the branch's
    /// new tip. On a case-insensitive repository (`core.ignorecase`) a name that differs only
    /// in case from an existing branch is refused, since both would be one file.
    ///
    /// # Returns
    ///
    /// What happened, or `Ok(Err(..))` with [`CheckinRefusal::Diverged`] or
    /// [`CheckinRefusal::CaseConflict`].
    ///
    /// # Errors
    ///
    /// [`CheckoutError::RefKeptMoving`] when the branch moved under every one of several
    /// attempts, and ref, reflog and ancestry failures.
    pub fn set_checked_in_branch(
        &self,
        update: &CheckinUpdate<'_>,
    ) -> Result<std::result::Result<BranchSet, CheckinRefusal>> {
        super::checkin::set_checked_in_branch(self.repo, update)
    }
}
