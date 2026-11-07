//! A checkout's own transfers to and from the server it came from.
//!
//! They always go to the URL the checkout's [`CheckoutMarker`] records, with
//! explicit refspecs, so renaming or re-pointing `origin` can't send raw
//! snapshot-based commits elsewhere or fetch from the wrong place.

use crate::fetch::Progress;
use crate::objects::ObjectId;
use crate::progress::CancelToken;
use crate::remote::{HttpClientFactory, Remote};
use crate::repo::Repository;
use crate::transfer::{
    FetchOptions, PushOptions, PushOutcome, PushRefSpec, TagMode, TransferStats,
};

use super::{
    in_ref, out_ref, same_branch_name, snapshot_fetch_refspec, upstream_ref, valid_branch_name,
    BranchInfo, CheckoutError, CheckoutInfo, CheckoutMarker, Result, SnapshotRoot,
};

impl CheckoutMarker {
    /// The server as a remote at [`Self::url`] (not `origin`), fetching only
    /// the checkout's snapshot (`snapshot_fetch_refspec`).
    ///
    /// # Errors
    ///
    /// [`RemoteError`](crate::remote::RemoteError) for a URL no transport handles.
    pub fn remote(&self) -> Result<Remote> {
        let mut remote = Remote::from_url(&self.url)?;
        remote.fetch_refspecs = vec![snapshot_fetch_refspec(&self.id, &self.branch)];
        Ok(remote)
    }
}

/// [`RepositorySnapshotCheckouts::push_for_checkin`](crate::repo_handles::RepositorySnapshotCheckouts::push_for_checkin) for `repo`.
pub(crate) fn push_for_checkin(
    repo: &Repository,
    marker: &CheckoutMarker,
    branches: &[(String, ObjectId)],
    progress: &mut dyn Progress,
    cancel: &CancelToken,
    http_factory: Option<&dyn HttpClientFactory>,
) -> Result<PushOutcome> {
    let specs: Vec<PushRefSpec> = branches
        .iter()
        .map(|(name, tip)| PushRefSpec {
            src: Some(*tip),
            src_ref: Some(format!("refs/heads/{name}")),
            dst: out_ref(&marker.id, name),
            force: true,
            ..Default::default()
        })
        .collect();
    marker.remote()?.push(
        repo,
        &specs,
        PushOptions::default(),
        progress,
        cancel,
        http_factory,
    )
}

/// What [`fetch_snapshot`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FetchedSnapshot {
    /// The marker, now listing the new snapshot; it has been written.
    pub marker: CheckoutMarker,
    /// The checkout's newest snapshot before.
    pub previous: Option<SnapshotRoot>,
    /// Its newest snapshot now, at [`upstream_ref`].
    pub current: SnapshotRoot,
    /// What the fetch received (nothing when the snapshot was already here).
    pub transfer: TransferStats,
}

impl FetchedSnapshot {
    /// Whether the newest snapshot is a new one: the target moved since the
    /// checkout's last snapshot.
    #[must_use]
    pub fn moved(&self) -> bool {
        self.previous
            .as_ref()
            .is_none_or(|p| p.commit != self.current.commit)
    }
}

/// Fetch the snapshot `info` describes (the server's answer to
/// [`super::CheckoutClient::update`]) into [`upstream_ref`] from the server at
/// the marker's URL, check that it is that snapshot, and only then record it
/// in the marker.
///
/// Nothing is fetched when the snapshot is already at [`upstream_ref`]. No
/// tags are fetched, and nothing else.
///
/// # Parameters
///
/// - `repo`: the checkout.
/// - `marker`: its marker as it is now.
/// - `info`: the checkout as the server has it after the update.
/// - `fetch`: environment and thread settings for the fetch; its refspecs,
///   tags, prune and dry-run settings are replaced.
/// - `progress`, `cancel`, `http_factory`: as for [`Remote::fetch`].
///
/// # Errors
///
/// [`CheckoutError::Remote`] when the fetch fails,
/// [`CheckoutError::SnapshotMoved`] when what arrived is not `info`'s
/// snapshot (the marker is then left as it was), [`CheckoutError::Protocol`]
/// for an answer that names no commit, and marker write failures.
pub(crate) fn fetch_snapshot(
    repo: &Repository,
    marker: &CheckoutMarker,
    info: &CheckoutInfo,
    fetch: FetchOptions,
    progress: &mut dyn Progress,
    cancel: &CancelToken,
    http_factory: Option<&dyn HttpClientFactory>,
) -> Result<FetchedSnapshot> {
    let root = ObjectId::from_hex(&info.root).map_err(|e| CheckoutError::Protocol {
        url: marker.url.clone(),
        detail: format!("the snapshot {:?} is not a commit id: {e}", info.root),
    })?;
    let upstream = upstream_ref(&info.branch);
    let local = || crate::refs::resolve_ref(repo.git_dir(), &upstream).ok();
    let mut transfer = TransferStats::default();
    if local() != Some(root) || !repo.odb().exists(&root) {
        let options = FetchOptions {
            refspecs: vec![snapshot_fetch_refspec(&info.id, &info.branch)],
            tags: TagMode::None,
            prune: false,
            dry_run: false,
            remote_name: None,
            ..fetch
        };
        transfer = marker
            .remote()?
            .fetch(repo, options, progress, cancel, http_factory)?
            .transfer;
        let got = local();
        if got != Some(root) {
            return Err(CheckoutError::SnapshotMoved {
                expected: info.root.clone(),
                got: got
                    .map(|g| g.to_hex())
                    .unwrap_or_else(|| "nothing".to_owned()),
            }
            .into());
        }
    }
    let updated = CheckoutMarker {
        roots: info.roots.clone(),
        ..marker.clone()
    };
    updated.write(repo.git_dir())?;
    Ok(FetchedSnapshot {
        previous: marker.last().cloned(),
        current: SnapshotRoot {
            commit: info.root.clone(),
            base: info.base.clone(),
        },
        marker: updated,
        transfer,
    })
}

/// [`RepositorySnapshotCheckouts::fetch_branch`](crate::repo_handles::RepositorySnapshotCheckouts::fetch_branch) for `repo`.
pub(crate) fn fetch_branch(
    repo: &Repository,
    marker: &CheckoutMarker,
    info: &BranchInfo,
    fetch: FetchOptions,
    progress: &mut dyn Progress,
    cancel: &CancelToken,
    http_factory: Option<&dyn HttpClientFactory>,
) -> Result<TransferStats> {
    let protocol = |detail: String| CheckoutError::Protocol {
        url: marker.url.clone(),
        detail,
    };
    if !valid_branch_name(&info.name) || same_branch_name(&info.name, &marker.branch) {
        return Err(crate::error::Error::from(protocol(format!(
            "the server answered with branch {:?}, which this checkout can't take",
            info.name
        ))));
    }
    let projected = ObjectId::from_hex(&info.projected).map_err(|e| {
        protocol(format!(
            "the projection {:?} is not a commit id: {e}",
            info.projected
        ))
    })?;
    let upstream = upstream_ref(&info.name);
    let local = || crate::refs::resolve_ref(repo.git_dir(), &upstream).ok();
    if local() == Some(projected) && repo.odb().exists(&projected) {
        return Ok(TransferStats::default());
    }
    let options = FetchOptions {
        refspecs: vec![format!("+{}:{upstream}", in_ref(&marker.id, &info.name))],
        tags: TagMode::None,
        prune: false,
        dry_run: false,
        remote_name: None,
        ..fetch
    };
    let transfer = marker
        .remote()?
        .fetch(repo, options, progress, cancel, http_factory)?
        .transfer;
    let got = local();
    if got != Some(projected) {
        return Err(CheckoutError::BranchMoved {
            name: info.name.clone(),
            expected: info.projected.clone(),
            got: got
                .map(|g| g.to_hex())
                .unwrap_or_else(|| "nothing".to_owned()),
        }
        .into());
    }
    Ok(transfer)
}
