//! Work that landed: a branch this checkout checked in, which the server then
//! merged into the target.
//!
//! The branch's commits are in every snapshot made after the merge, but not
//! as the snapshots' ancestors: a snapshot is one commit standing for the
//! target's tree. Replaying them onto such a snapshot could conflict with
//! themselves (three commits editing one line, say, against a snapshot that
//! already has the last edit), so they are never replayed:
//!
//! - [`land_integration`] decides, from the server's answer to an
//!   integration, whether the work landed in the branch this checkout
//!   snapshots, and if so notes the checked-in tip in the
//!   [`CheckoutMarker`] ([`record_landed`], [`CheckoutMarker::landed`]).
//!   Work merged into another branch (the served repository's `HEAD` can
//!   name a different branch than the one a checkout was made from) is not
//!   in this checkout's snapshots, so nothing is noted.
//! - A [`super::refresh`] then moves a branch with nothing above a landed tip
//!   to the new snapshot (`git reset --keep`), and rebases one with commits
//!   above it with only those commits (`git rebase --onto <snapshot> <tip>`),
//!   for the current branch now and for any other branch when it is
//!   refreshed later. A landed tip that no branch, and no worktree's
//!   detached `HEAD`, builds on any more is forgotten.
//! - [`Landing::move_off`] ([`move_off_landed_branch`]) puts the user, whose
//!   branch landed and now has nothing of its own, on a fresh work branch
//!   the server reserved ([`CheckoutClient::reserve_work_branch`]); the
//!   landed branch is gone here and stays on the server.

use crate::merge_base::{is_ancestor, merge_bases_first_vs_rest};
use crate::objects::ObjectId;
use crate::porcelain::branch::{rename_branch, RenameBranchOptions};
use crate::porcelain::branch_error::BranchError;
use crate::repo::Repository;
use crate::state::{resolve_head_for_repository, HeadState};

use super::{CheckoutClient, CheckoutError, CheckoutMarker, IntegrateResponse, Result};

/// How many names [`move_off_landed_branch`] asks the server for before
/// giving up on finding one that is free here.
const NAME_ATTEMPTS: usize = 8;

/// A branch [`move_off_landed_branch`] moved the user off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovedOff {
    /// The branch that landed, now gone here (without `refs/heads/`).
    pub from: String,
    /// The new work branch `HEAD` is on, which the server reserved.
    pub to: String,
    /// Where both are: the checkout's newest snapshot.
    pub tip: ObjectId,
}

/// What an integration means for the checkout that asked for it, as
/// [`land_integration`] decides it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Landing {
    /// The server merged into the branch this checkout snapshots
    /// ([`CheckoutMarker::branch`]), or that branch already had the work: its
    /// next snapshot has the integrated branch's commits.
    Here {
        /// The integrated branch, without `refs/heads/`.
        name: String,
        /// The tip this integration checked in, now recorded as landed
        /// ([`CheckoutMarker::landed`]); `None` when it checked nothing in.
        tip: Option<ObjectId>,
    },
    /// The server merged into `target`, which is not the branch this
    /// checkout snapshots: the served repository's `HEAD` names another
    /// branch than the one the checkout was made from. The checkout's
    /// snapshots don't get the commits, so nothing is recorded, and the
    /// branch keeps its commits here (a refresh rebases them as usual).
    Elsewhere {
        /// The branch the server merged into.
        target: String,
    },
}

impl Landing {
    /// After the refresh that follows the integration: when the work landed
    /// here, put `HEAD` on a fresh work branch in place of the integrated one
    /// ([`move_off_landed_branch`]).
    ///
    /// That happens only when `HEAD` is on the integrated branch and the
    /// branch is at the checkout's newest snapshot, with nothing of its own.
    /// It holds for a branch whose earlier move-off failed after a good
    /// refresh, so integrating it again finishes the move.
    ///
    /// # Parameters
    ///
    /// - `repo`: the checkout, opened after the refresh.
    /// - `client`: the checkout's server.
    /// - `wanted`: the name to ask the server for, such as
    ///   [`super::suggest_work_branch`]'s.
    ///
    /// # Returns
    ///
    /// The move, or `None` when there was nothing to move off (always for
    /// [`Landing::Elsewhere`], without asking the server).
    ///
    /// # Errors
    ///
    /// Those of [`move_off_landed_branch`].
    pub fn move_off(
        &self,
        repo: &Repository,
        client: &CheckoutClient<'_>,
        wanted: &str,
    ) -> Result<Option<MovedOff>> {
        match self {
            Self::Here { name, .. } => move_off_landed_branch(repo, client, name, wanted),
            Self::Elsewhere { .. } => Ok(None),
        }
    }
}

impl CheckoutMarker {
    /// Note that `tip`, a branch tip this checkout checked in, has landed in
    /// [`Self::branch`] ([`Self::landed`]).
    ///
    /// # Returns
    ///
    /// Whether it was not noted already.
    pub fn record_landed(&mut self, tip: ObjectId) -> bool {
        let hex = tip.to_hex();
        if self.landed.contains(&hex) {
            return false;
        }
        self.landed.push(hex);
        true
    }

    /// The landed tips that are commits ([`Self::landed`]).
    fn landed_tips(&self) -> impl Iterator<Item = ObjectId> + '_ {
        self.landed
            .iter()
            .filter_map(|tip| ObjectId::from_hex(tip).ok())
    }
}

/// Record in the marker of the checkout `repo` that `tip`, which it checked
/// in, landed in the branch the checkout snapshots: the server merged it
/// into that branch, or the branch already had it
/// ([`CheckoutMarker::landed`]). [`land_integration`] decides that from the
/// server's answer and calls this.
///
/// Every snapshot made since has the commits up to `tip`, but not as
/// ancestors, so replaying them onto one could conflict with themselves.
/// From then on [`super::refresh`] never replays them: a branch with nothing
/// above `tip` moves to the new snapshot keeping uncommitted changes, one
/// with commits above it rebases only those, and another branch with nothing
/// above it follows. [`move_off_landed_branch`] then puts the user on a fresh
/// work branch.
///
/// # Parameters
///
/// - `repo`: the checkout.
/// - `marker`: its marker, used when the one on disk can't be read.
/// - `tip`: the tip that was checked in (its raw, local commit).
///
/// # Returns
///
/// The marker as written.
///
/// # Errors
///
/// Marker write failures.
pub fn record_landed(
    repo: &Repository,
    marker: &CheckoutMarker,
    tip: ObjectId,
) -> Result<CheckoutMarker> {
    let mut current = CheckoutMarker::read(repo.git_dir()).unwrap_or_else(|| marker.clone());
    if current.record_landed(tip) {
        current.write(repo.git_dir())?;
    }
    Ok(current)
}

/// Decide what the integration the server answered with `response`
/// ([`CheckoutClient::integrate`]) means for the checkout `repo`, and record
/// what landed. Call it right after the integration, then refresh the
/// checkout ([`super::refresh`]), then call [`Landing::move_off`].
///
/// The work landed here when the branch the server merged into
/// ([`IntegrateResponse::target`]) is exactly the one the checkout snapshots
/// ([`CheckoutMarker::branch`]); a case variant may be another branch on the
/// server, and treating it as this one would drop the branch's commits from
/// the next snapshot. A merge and an answer that the target already had the
/// branch both mean the tip this integration checked in, `checked_in`, is in
/// the target, so it is recorded as landed ([`record_landed`]): later
/// refreshes never replay its commits. Otherwise nothing is recorded.
///
/// # Parameters
///
/// - `repo`: the checkout.
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
    repo: &Repository,
    marker: &CheckoutMarker,
    response: &IntegrateResponse,
    checked_in: Option<ObjectId>,
) -> Result<Landing> {
    if response.target != marker.branch {
        return Ok(Landing::Elsewhere {
            target: response.target.clone(),
        });
    }
    if let Some(tip) = checked_in {
        record_landed(repo, marker, tip)?;
    }
    Ok(Landing::Here {
        name: response.name.clone(),
        tip: checked_in,
    })
}

/// The newest commit of `tip`'s history whose changes the snapshot `root`
/// has though it isn't an ancestor of it: where `tip` meets one of the
/// marker's landed tips, when that is past where it meets `root`.
///
/// # Errors
///
/// Ancestry failures.
pub(crate) fn landed_base(
    repo: &Repository,
    marker: &CheckoutMarker,
    tip: ObjectId,
    root: ObjectId,
) -> Result<Option<ObjectId>> {
    if marker.landed.is_empty() {
        return Ok(None);
    }
    let Some(start) = merge_bases_first_vs_rest(repo, tip, &[root])?
        .into_iter()
        .next()
    else {
        return Ok(None);
    };
    let mut base = start;
    for landed in marker.landed_tips() {
        if !repo.odb().exists(&landed) {
            continue;
        }
        let Some(met) = merge_bases_first_vs_rest(repo, tip, &[landed])?
            .into_iter()
            .next()
        else {
            continue;
        };
        if met != base && is_ancestor(repo, base, met)? {
            base = met;
        }
    }
    Ok((base != start).then_some(base))
}

/// Forget the landed tips no local branch, nor the detached `HEAD` of any of
/// the checkout's worktrees, builds on any more: where every one of them
/// meets it is in the snapshot `root`.
///
/// # Errors
///
/// Ref, worktree, ancestry and marker write failures.
pub(crate) fn prune_landed(repo: &Repository, root: ObjectId) -> Result<()> {
    let Some(mut marker) = CheckoutMarker::read(repo.git_dir()) else {
        return Ok(());
    };
    if marker.landed.is_empty() {
        return Ok(());
    }
    let mut tips: Vec<ObjectId> = crate::refs::list_refs(repo.git_dir(), "refs/heads/")?
        .into_iter()
        .map(|(_, tip)| tip)
        .collect();
    // A refresh in one worktree must not forget the tip another one is
    // detached on: a branch started there later would replay the commits.
    tips.extend(
        crate::worktree::list_worktrees(repo)?
            .into_iter()
            .filter_map(|worktree| match worktree.head {
                HeadState::Detached { oid } => Some(oid),
                _ => None,
            }),
    );
    let mut kept = Vec::new();
    for landed in marker.landed_tips() {
        if repo.odb().exists(&landed) && builds_on(repo, &tips, landed, root)? {
            kept.push(landed.to_hex());
        }
    }
    if kept != marker.landed {
        marker.landed = kept;
        marker.write(repo.git_dir())?;
    }
    Ok(())
}

/// Whether one of `tips` has commits of `landed`'s that the snapshot `root`
/// doesn't have as ancestors.
fn builds_on(
    repo: &Repository,
    tips: &[ObjectId],
    landed: ObjectId,
    root: ObjectId,
) -> Result<bool> {
    for &tip in tips {
        if let Some(met) = merge_bases_first_vs_rest(repo, tip, &[landed])?
            .into_iter()
            .next()
        {
            if !is_ancestor(repo, met, root)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// After `branch` landed in the target and a refresh moved it to the newest
/// snapshot, leaving it with no commits of its own: rename it to a fresh
/// work branch the server reserves, so new work starts a new branch there
/// rather than adding to one the target already merged. The landed branch is
/// gone here; it stays on the server.
///
/// Nothing happens unless `HEAD` is on `branch` and `branch` is at the
/// checkout's newest snapshot.
///
/// # Parameters
///
/// - `repo`: the checkout.
/// - `client`: the checkout's server.
/// - `branch`: the branch that landed, without `refs/heads/`.
/// - `wanted`: the name to ask the server for, such as
///   [`super::suggest_work_branch`]'s; the server numbers it past names that
///   are taken.
///
/// # Returns
///
/// The move, or `None` when there was nothing to move off.
///
/// # Errors
///
/// The server's refusal, [`CheckoutError::WorkBranchTaken`] when every name
/// it reserved is a branch here already, and ref and config failures.
pub fn move_off_landed_branch(
    repo: &Repository,
    client: &CheckoutClient<'_>,
    branch: &str,
    wanted: &str,
) -> Result<Option<MovedOff>> {
    let Some(marker) = CheckoutMarker::read(repo.git_dir()) else {
        return Ok(None);
    };
    let Some(root) = marker
        .last()
        .and_then(|last| ObjectId::from_hex(&last.commit).ok())
    else {
        return Ok(None);
    };
    let on_branch = matches!(
        resolve_head_for_repository(repo)?,
        HeadState::Branch { short_name, oid: Some(tip), .. } if short_name == branch && tip == root
    );
    if !on_branch {
        return Ok(None);
    }
    let mut last = String::new();
    for _ in 0..NAME_ATTEMPTS {
        let name = client.reserve_work_branch(&marker.id, wanted)?;
        let renamed = rename_branch(
            repo,
            &RenameBranchOptions {
                old: branch.to_owned(),
                new: name.clone(),
                force: false,
                dry_run: false,
            },
        );
        match renamed {
            Ok(_) => {
                return Ok(Some(MovedOff {
                    from: branch.to_owned(),
                    to: name,
                    tip: root,
                }))
            }
            Err(crate::error::Error::Branch(BranchError::AlreadyExists { .. })) => last = name,
            Err(e) => return Err(e.into()),
        }
    }
    Err(CheckoutError::WorkBranchTaken { name: last })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::InitOptions;
    use crate::snapshot_checkout::IntegrateStatus;

    fn marker() -> CheckoutMarker {
        CheckoutMarker {
            url: "http://127.0.0.1/demo.git".to_owned(),
            id: "0123456789abcdef0123".to_owned(),
            branch: "main".to_owned(),
            paths: Vec::new(),
            roots: Vec::new(),
            landed: Vec::new(),
        }
    }

    #[test]
    fn a_landed_tip_is_recorded_once_and_old_markers_read_without_any() {
        let tip = ObjectId::from_hex("1111111111111111111111111111111111111111").expect("oid");
        let mut m = marker();
        assert!(m.record_landed(tip));
        assert!(!m.record_landed(tip));
        assert_eq!(m.landed, [tip.to_hex()]);

        let mut old = serde_json::to_value(marker()).expect("json");
        assert!(old.get("landed").is_none(), "an empty list is not written");
        old["roots"] = serde_json::json!([]);
        let read: CheckoutMarker = serde_json::from_value(old).expect("parse");
        assert!(read.landed.is_empty());
    }

    fn answer(target: &str, status: IntegrateStatus) -> IntegrateResponse {
        IntegrateResponse {
            status,
            name: "alice/work".to_owned(),
            target: target.to_owned(),
            previous: "2222222222222222222222222222222222222222".to_owned(),
            head: "3333333333333333333333333333333333333333".to_owned(),
            commits: 2,
        }
    }

    fn landed(repo: &Repository) -> Vec<String> {
        CheckoutMarker::read(repo.git_dir())
            .expect("a checkout")
            .landed
    }

    #[test]
    fn only_work_merged_into_the_branch_the_checkout_snapshots_lands() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        marker().write(repo.git_dir()).expect("marker");
        let tip = ObjectId::from_hex("1111111111111111111111111111111111111111").expect("oid");

        // The served HEAD names another branch, or a case variant of this one
        // (which may be another branch on the server): nothing lands here.
        for target in ["develop", "Main"] {
            let landing = land_integration(
                &repo,
                &marker(),
                &answer(target, IntegrateStatus::Merged),
                Some(tip),
            )
            .expect("land");
            assert_eq!(
                landing,
                Landing::Elsewhere {
                    target: target.to_owned()
                }
            );
            assert!(landed(&repo).is_empty(), "{target}");
        }

        // Merged into it, or it had the work already: the checked-in tip lands, once.
        for status in [IntegrateStatus::Merged, IntegrateStatus::AlreadyIntegrated] {
            let landing = land_integration(&repo, &marker(), &answer("main", status), Some(tip))
                .expect("land");
            assert_eq!(
                landing,
                Landing::Here {
                    name: "alice/work".to_owned(),
                    tip: Some(tip),
                }
            );
            assert_eq!(landed(&repo), [tip.to_hex()]);
        }

        // Integrating again with nothing to check in (the branch is already at
        // the snapshot) still lands here, so the move off can finish.
        let again = land_integration(
            &repo,
            &marker(),
            &answer("main", IntegrateStatus::AlreadyIntegrated),
            None,
        )
        .expect("land");
        assert_eq!(
            again,
            Landing::Here {
                name: "alice/work".to_owned(),
                tip: None,
            }
        );
        assert_eq!(landed(&repo), [tip.to_hex()]);
    }
}
