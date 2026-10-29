//! Per-reference push results.
//!
//! After a push, each requested reference update resolves to one
//! [`PushRefStatus`] (up to date, updated, or one of the rejection reasons),
//! carried with its old and new values in a [`PushRefResult`]. Callers render
//! these themselves; `grit push` maps them to its human, JSON and Markdown output.

use crate::objects::ObjectId;

/// The resolved outcome of a single reference update during a push.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
#[cfg_attr(feature = "serde-models", serde(rename_all = "snake_case"))]
pub enum PushRefStatus {
    /// The reference was already at the requested value (`=`, `[up to date]`).
    UpToDate,
    /// A successful update (`*` new, `-` delete, ` ` fast-forward, `+` forced).
    Ok,
    /// Client-side rejection: the update is not a fast-forward and `--force`
    /// was not given (`!`, `[rejected] (non-fast-forward)`).
    RejectNonFastForward,
    /// Rejected because the new ref already exists (`!`, `[rejected] (already exists)`).
    RejectAlreadyExists,
    /// Rejected because the remote has the ref but we need to fetch first.
    RejectFetchFirst,
    /// Rejected because a forced update is required.
    RejectNeedsForce,
    /// Rejected because force-with-lease found stale info.
    RejectStale,
    /// The remote `receive-pack` declined the update (`!`, `[remote rejected] (<reason>)`).
    RemoteRejected,
    /// Client-side rejection: a delete was requested but the ref is absent on the remote
    /// (`!`, `[rejected] (remote ref does not exist)`).
    RejectRemoteRefMissing,
    /// Part of an atomic push that failed because another ref was rejected
    /// (`!`, `[rejected] (atomic push failed)`).
    AtomicPushFailed,
}

impl PushRefStatus {
    /// Whether this status represents a hard failure (causes a non-zero exit).
    #[must_use]
    pub fn is_error(&self) -> bool {
        !matches!(self, PushRefStatus::UpToDate | PushRefStatus::Ok)
    }
}

/// One reference's resolved push result, ready for display.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
pub struct PushRefResult {
    /// The local ref name (source side), e.g. `refs/heads/next`. `None` for deletions.
    pub local_ref: Option<String>,
    /// The remote ref name (destination side), e.g. `refs/heads/next`.
    pub remote_ref: String,
    /// Old value of the remote ref (`None`/zero for a new ref).
    pub old_oid: Option<ObjectId>,
    /// New value of the remote ref (`None`/zero for a deletion).
    pub new_oid: Option<ObjectId>,
    /// Whether this update was a forced (non-fast-forward but `--force`d) update.
    pub forced: bool,
    /// Whether this update deletes the remote ref.
    pub deletion: bool,
    /// The resolved status.
    pub status: PushRefStatus,
    /// Extra reason text (used for `[remote rejected]`).
    pub message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_up_to_date_and_ok_are_successes() {
        assert!(!PushRefStatus::UpToDate.is_error());
        assert!(!PushRefStatus::Ok.is_error());
        for status in [
            PushRefStatus::RejectNonFastForward,
            PushRefStatus::RejectAlreadyExists,
            PushRefStatus::RejectFetchFirst,
            PushRefStatus::RejectNeedsForce,
            PushRefStatus::RejectStale,
            PushRefStatus::RemoteRejected,
            PushRefStatus::RejectRemoteRefMissing,
            PushRefStatus::AtomicPushFailed,
        ] {
            assert!(status.is_error(), "{status:?}");
        }
    }
}
