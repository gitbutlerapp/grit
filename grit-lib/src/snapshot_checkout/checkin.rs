//! The server's side of a check-in, one branch at a time: check what the
//! client pushed and graft it ([`graft_checkin_branch`]), then set the
//! server's branch to the graft ([`set_checked_in_branch`]).
//!
//! Which check-ins may move a branch the server already has is decided by
//! [`decide_checkin`]: a fast-forward, a branch this checkout set last, and a
//! branch that keeps every commit the server's had by change id (a rebase of
//! it, perhaps with more on top, whose copies keep what the originals changed
//! outside the checkout's paths) may; anything else needs `force`. The rule
//! that allowed it ([`CheckinRule`]) is reported with the result.
//!
//! Setting a branch is one compare-and-swap against the tip the decision was
//! made on (create-only for a new branch). When the branch moves in between,
//! the decision is made again with its new tip, so a check-in that loses a
//! race gets the answer it would have got had it come second (usually
//! [`CheckinRefusal::Diverged`]) instead of overwriting the winner.

use serde::{Deserialize, Serialize};

use crate::ident::Signature;
use crate::objects::ObjectId;
use crate::repo::Repository;

use super::graft::{graft, GraftError, Grafted};
use super::graft_log::GraftLog;
use super::ids::keeps_every_commit;
use super::refs::{swap_ref, Swap};
use super::{
    out_ref, same_branch_name, valid_branch_name, CheckinBranch, CheckinStatus, CheckoutError,
    CheckoutInfo, Result,
};

/// How many times [`set_checked_in_branch`] decides again after losing a
/// race before it gives up.
const ATTEMPTS: usize = 16;

/// Why the server did not set a branch of a check-in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CheckinRefusal {
    /// The name is not a valid branch name.
    InvalidName,
    /// The branch is the server's target branch (in any case), which only
    /// `integrate` and slices change.
    TargetBranch {
        /// The target branch's name as the server has it.
        target: String,
    },
    /// The server has a branch whose name differs from this one only in case,
    /// which its case-insensitive file system can't keep apart.
    CaseConflict {
        /// The server's branch.
        existing: String,
    },
    /// The tip the client named is not an object id.
    InvalidTip,
    /// The server has no push of the branch at the tip the client named.
    NotPushed {
        /// Where the client should have pushed it.
        out_ref: String,
    },
    /// A commit does not build on the checkout's snapshots.
    NotOnSnapshot {
        /// The commit (12 hex digits).
        commit: String,
    },
    /// A commit changes a path the target holds differently.
    Conflict {
        /// The commit (12 hex digits).
        commit: String,
        /// The path.
        path: String,
        /// Whether the path is outside the checkout's paths.
        outside_paths: bool,
    },
    /// A merge commit's parents conflict outside the checkout's paths.
    MergeConflict {
        /// The merge commit (12 hex digits).
        commit: String,
        /// The conflicting paths.
        paths: Vec<String>,
    },
    /// The server's branch has commits the graft would drop.
    Diverged {
        /// Where the server's branch is.
        tip: String,
    },
}

impl CheckinRefusal {
    /// A short, stable name for the kind of refusal (the serialized `kind`).
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::InvalidName => "invalid_name",
            Self::TargetBranch { .. } => "target_branch",
            Self::CaseConflict { .. } => "case_conflict",
            Self::InvalidTip => "invalid_tip",
            Self::NotPushed { .. } => "not_pushed",
            Self::NotOnSnapshot { .. } => "not_on_snapshot",
            Self::Conflict { .. } => "conflict",
            Self::MergeConflict { .. } => "merge_conflict",
            Self::Diverged { .. } => "diverged",
        }
    }
}

/// Why a check-in was allowed to move a branch the server already had to a
/// commit that does not simply replace nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckinRule {
    /// The graft has the server's branch in its history.
    FastForward,
    /// This checkout set the branch last, so it replaces its own work (after
    /// an amend or a rebase).
    OwnLastSet,
    /// The graft has every commit of the server's branch that the target
    /// lacks, matched by change id: a rebase of it, perhaps with more on top,
    /// whose copies hold what the originals changed outside the checkout's
    /// paths ([`keeps_every_commit`]).
    KeepsEveryCommit,
    /// None of the others held; the check-in asked to replace the branch.
    Forced,
}

impl CheckinRule {
    /// A short, stable name for the rule (its serialized form).
    #[must_use]
    pub fn kind(self) -> &'static str {
        match self {
            Self::FastForward => "fast_forward",
            Self::OwnLastSet => "own_last_set",
            Self::KeepsEveryCommit => "keeps_every_commit",
            Self::Forced => "forced",
        }
    }
}

/// How the server decides whether a check-in may set a branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckinDecision {
    /// The branch does not exist yet.
    Create,
    /// The branch already has the graft: it points at it, or at commits on
    /// top of it.
    Unchanged,
    /// The branch moves to the graft.
    Update {
        /// What allows it.
        rule: CheckinRule,
    },
    /// The server's branch has commits the graft would drop.
    Diverged,
}

/// Decide whether `grafted` may replace the server's branch, now at
/// `previous`.
///
/// An update is allowed, checked in this order, when it is a fast-forward;
/// when this checkout set the branch last (`set_last`), so a checkout can
/// replace its own work after an amend or rebase; when it keeps every commit
/// of the server's branch that the target lacks, by change id, with what
/// each changed outside `paths` ([`keeps_every_commit`]); and otherwise only
/// when `force` is set. A branch that already has the graft, with more on
/// top (a server branch the checkout pulled, which moved on since), is left
/// unchanged unless this checkout set it last or `force` is set.
///
/// # Parameters
///
/// - `repo`: the served repository.
/// - `previous`: where the server's branch is; `None` when it has none.
/// - `grafted`: the graft of the checked-in tip.
/// - `targets`: the tips of the target branch (the one the checkout was made
///   from and the one the server's `HEAD` names, when they differ); commits
///   on them are never counted as dropped.
/// - `paths`: the checking-in checkout's paths (empty: the whole tree).
/// - `set_last`: what this checkout last set the branch to, if it did.
/// - `force`: replace the branch even where that drops commits it has.
///
/// # Returns
///
/// The decision, naming the rule that allows an update.
///
/// # Errors
///
/// Ancestry walk and object read failures.
pub fn decide_checkin(
    repo: &Repository,
    previous: Option<ObjectId>,
    grafted: ObjectId,
    targets: &[ObjectId],
    paths: &[String],
    set_last: Option<ObjectId>,
    force: bool,
) -> Result<CheckinDecision> {
    let Some(previous) = previous else {
        return Ok(CheckinDecision::Create);
    };
    if previous == grafted {
        return Ok(CheckinDecision::Unchanged);
    }
    let rule = if crate::merge_base::is_ancestor(repo, previous, grafted)? {
        CheckinRule::FastForward
    } else if set_last == Some(previous) {
        CheckinRule::OwnLastSet
    } else if !force && crate::merge_base::is_ancestor(repo, grafted, previous)? {
        return Ok(CheckinDecision::Unchanged);
    } else if keeps_every_commit(repo, previous, grafted, targets, paths)? {
        CheckinRule::KeepsEveryCommit
    } else if force {
        CheckinRule::Forced
    } else {
        return Ok(CheckinDecision::Diverged);
    };
    Ok(CheckinDecision::Update { rule })
}

impl From<GraftError> for CheckinRefusal {
    fn from(e: GraftError) -> Self {
        match e {
            GraftError::NotOnSnapshot { commit } => Self::NotOnSnapshot { commit },
            GraftError::Conflict {
                commit,
                path,
                outside_paths,
            } => Self::Conflict {
                commit,
                path,
                outside_paths,
            },
            GraftError::MergeConflict { commit, paths } => Self::MergeConflict { commit, paths },
        }
    }
}

/// Check a branch of a check-in and graft it onto the real history.
///
/// The branch is refused when its name is not a valid branch name; when it
/// names, in any case, the branch the checkout was made from or the
/// repository's current `target`; when its tip is not an object id or is not
/// what the server has at [`out_ref`]; and when the graft fails
/// ([`graft`]).
///
/// # Parameters
///
/// - `repo`: the served repository.
/// - `checkout`: the checkout checking in.
/// - `target`: the repository's target branch now ([`super::target_branch`]).
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
    repo: &Repository,
    checkout: &CheckoutInfo,
    target: &str,
    branch: &CheckinBranch,
    known: Option<&GraftLog>,
    targets: &[ObjectId],
) -> Result<std::result::Result<Grafted, CheckinRefusal>> {
    if !valid_branch_name(&branch.name) {
        return Ok(Err(CheckinRefusal::InvalidName));
    }
    if let Some(target) = [checkout.branch.as_str(), target]
        .into_iter()
        .find(|t| same_branch_name(&branch.name, t))
    {
        return Ok(Err(CheckinRefusal::TargetBranch {
            target: target.to_owned(),
        }));
    }
    let Ok(raw) = ObjectId::from_hex(&branch.raw) else {
        return Ok(Err(CheckinRefusal::InvalidTip));
    };
    let pushed_to = out_ref(&checkout.id, &branch.name);
    if crate::refs::resolve_ref(repo.git_dir(), &pushed_to).ok() != Some(raw) {
        return Ok(Err(CheckinRefusal::NotPushed { out_ref: pushed_to }));
    }
    Ok(
        graft(repo, &checkout.roots, &checkout.paths, known, targets, raw)?
            .map_err(CheckinRefusal::from),
    )
}

/// Where a check-in left one of the server's branches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BranchSet {
    /// [`CheckinStatus::Created`], [`CheckinStatus::Updated`] or
    /// [`CheckinStatus::Unchanged`].
    pub status: CheckinStatus,
    /// Where the branch was before; `None` when the server had no such branch.
    pub previous: Option<ObjectId>,
    /// For [`CheckinStatus::Updated`], the rule that allowed it.
    pub rule: Option<CheckinRule>,
}

/// Set the server's branch `name` to `grafted` when [`decide_checkin`]
/// allows it, as one compare-and-swap against the tip it decided on (a new
/// branch must still be missing), logged by `who` with `message` where the
/// repository keeps a reflog for it.
///
/// When the branch moves between deciding and setting, it decides again with
/// the branch's new tip. On a case-insensitive repository (`core.ignorecase`)
/// a name that differs only in case from an existing branch is refused, since
/// both would be one file.
///
/// # Parameters
///
/// - `repo`: the served repository.
/// - `name`: the branch, without `refs/heads/`.
/// - `grafted`: the graft of the checked-in tip.
/// - `targets`: the tips of the target branch, whose commits a check-in never
///   counts as dropped (see [`decide_checkin`]).
/// - `paths`: the checking-in checkout's paths (see [`decide_checkin`]).
/// - `set_last`: what this checkout last set the branch to, if it did.
/// - `force`: set it even where it drops commits the server's branch has.
/// - `who`, `message`: the reflog entry's identity, time and message.
///
/// # Returns
///
/// What happened, or `Ok(Err(..))` with [`CheckinRefusal::Diverged`] or
/// [`CheckinRefusal::CaseConflict`].
///
/// # Errors
///
/// [`CheckoutError::RefKeptMoving`] when the branch moved under every one of
/// several attempts, and ref, reflog and ancestry failures.
#[allow(clippy::too_many_arguments)]
pub fn set_checked_in_branch(
    repo: &Repository,
    name: &str,
    grafted: ObjectId,
    targets: &[ObjectId],
    paths: &[String],
    set_last: Option<ObjectId>,
    force: bool,
    who: &Signature,
    message: &str,
) -> Result<std::result::Result<BranchSet, CheckinRefusal>> {
    let refname = format!("refs/heads/{name}");
    let decide = Decide {
        grafted,
        targets,
        paths,
        set_last,
        force,
    };
    set_branch_reading(repo, &refname, &decide, who, message, || {
        Ok(crate::refs::resolve_ref(repo.git_dir(), &refname).ok())
    })
}

/// What [`decide_checkin`] is asked each time [`set_branch_reading`] reads
/// the branch.
struct Decide<'a> {
    grafted: ObjectId,
    targets: &'a [ObjectId],
    paths: &'a [String],
    set_last: Option<ObjectId>,
    force: bool,
}

/// [`set_checked_in_branch`], reading the branch's tip with `read` before
/// each decision.
fn set_branch_reading(
    repo: &Repository,
    refname: &str,
    decide: &Decide<'_>,
    who: &Signature,
    message: &str,
    mut read: impl FnMut() -> Result<Option<ObjectId>>,
) -> Result<std::result::Result<BranchSet, CheckinRefusal>> {
    if let Some(existing) = case_variant(repo, refname)? {
        return Ok(Err(CheckinRefusal::CaseConflict { existing }));
    }
    let grafted = decide.grafted;
    for _ in 0..ATTEMPTS {
        let previous = read()?;
        let decision = decide_checkin(
            repo,
            previous,
            grafted,
            decide.targets,
            decide.paths,
            decide.set_last,
            decide.force,
        )?;
        let (status, rule) = match decision {
            CheckinDecision::Unchanged => (CheckinStatus::Unchanged, None),
            CheckinDecision::Diverged => {
                return Ok(Err(CheckinRefusal::Diverged {
                    tip: previous.map(|p| p.to_hex()).unwrap_or_default(),
                }))
            }
            CheckinDecision::Create => (CheckinStatus::Created, None),
            CheckinDecision::Update { rule } => (CheckinStatus::Updated, Some(rule)),
        };
        if status != CheckinStatus::Unchanged
            && swap_ref(repo, refname, previous, grafted, who, message)? == Swap::Moved
        {
            continue;
        }
        return Ok(Ok(BranchSet {
            status,
            previous,
            rule,
        }));
    }
    Err(CheckoutError::RefKeptMoving {
        refname: refname.to_owned(),
    })
}

/// The server's branch whose name differs from `refname`'s only in case, when
/// the repository's file system can't tell them apart (`core.ignorecase`).
fn case_variant(repo: &Repository, refname: &str) -> Result<Option<String>> {
    if !ignores_case(repo)? {
        return Ok(None);
    }
    let name = refname.trim_start_matches("refs/heads/");
    Ok(crate::refs::list_refs(repo.git_dir(), "refs/heads/")?
        .into_iter()
        .filter_map(|(r, _)| r.strip_prefix("refs/heads/").map(str::to_owned))
        .find(|existing| existing != name && same_branch_name(existing, name)))
}

/// Whether the repository's file system can't tell names apart by case
/// (`core.ignorecase`), so that two branches differing only in case are one.
pub(crate) fn ignores_case(repo: &Repository) -> Result<bool> {
    Ok(repo
        .config()?
        .get_bool("core.ignorecase")
        .and_then(|r| r.ok())
        .unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::ObjectKind;
    use crate::repo::InitOptions;

    fn who() -> Signature {
        let time = time::OffsetDateTime::from_unix_timestamp(1_700_000_000)
            .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
        Signature::cleaned("Server", "server@example.com", time).expect("signature")
    }

    fn commit(repo: &Repository, parents: &[ObjectId], msg: &str) -> ObjectId {
        let tree = repo.odb().write(ObjectKind::Tree, b"").expect("empty tree");
        let mut s = format!("tree {}\n", tree.to_hex());
        for p in parents {
            s.push_str(&format!("parent {}\n", p.to_hex()));
        }
        s.push_str(&format!(
            "author A <a@e> 1700000000 +0000\ncommitter A <a@e> 1700000000 +0000\n\n{msg}\n"
        ));
        repo.odb()
            .write(ObjectKind::Commit, s.as_bytes())
            .expect("commit")
    }

    fn tip(repo: &Repository, name: &str) -> Option<ObjectId> {
        crate::refs::resolve_ref(repo.git_dir(), &format!("refs/heads/{name}")).ok()
    }

    #[test]
    fn branches_are_created_updated_and_refused_by_the_rules() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let a = commit(&repo, &[], "a");
        let b = commit(&repo, &[a], "b");
        let other = commit(&repo, &[], "other");
        let set = |grafted, last, force| {
            set_checked_in_branch(
                &repo,
                "topic",
                grafted,
                &[],
                &[],
                last,
                force,
                &who(),
                "checkin",
            )
            .expect("set")
        };
        assert_eq!(
            set(a, None, false),
            Ok(BranchSet {
                status: CheckinStatus::Created,
                previous: None,
                rule: None,
            })
        );
        assert_eq!(
            set(a, None, false).map(|s| s.status),
            Ok(CheckinStatus::Unchanged)
        );
        assert_eq!(
            set(b, None, false),
            Ok(BranchSet {
                status: CheckinStatus::Updated,
                previous: Some(a),
                rule: Some(CheckinRule::FastForward),
            })
        );
        assert_eq!(
            set(other, None, false),
            Err(CheckinRefusal::Diverged { tip: b.to_hex() })
        );
        assert_eq!(tip(&repo, "topic"), Some(b));
        assert_eq!(
            set(other, Some(b), false).map(|s| (s.previous, s.rule)),
            Ok((Some(b), Some(CheckinRule::OwnLastSet))),
            "the checkout that set it last may replace it"
        );
        assert_eq!(tip(&repo, "topic"), Some(other));
    }

    /// A commit on `parents` with `msg`, and a `change-id` header when `id`
    /// is given.
    fn commit_with_id(
        repo: &Repository,
        parents: &[ObjectId],
        msg: &str,
        id: Option<&str>,
    ) -> ObjectId {
        let tree = repo.odb().write(ObjectKind::Tree, b"").expect("empty tree");
        let mut s = format!("tree {}\n", tree.to_hex());
        for p in parents {
            s.push_str(&format!("parent {}\n", p.to_hex()));
        }
        s.push_str("author A <a@e> 1700000000 +0000\ncommitter A <a@e> 1700000000 +0000\n");
        if let Some(id) = id {
            s.push_str(&format!("change-id {id}\n"));
        }
        s.push_str(&format!("\n{msg}\n"));
        repo.odb()
            .write(ObjectKind::Commit, s.as_bytes())
            .expect("commit")
    }

    #[test]
    fn checkins_update_by_a_named_rule() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let a = commit(&repo, &[], "a");
        let b = commit(&repo, &[a], "b");
        let other = commit(&repo, &[], "other");
        let decide = |prev, new, last, force| {
            decide_checkin(&repo, prev, new, &[], &[], last, force).expect("decide")
        };
        let update = |rule| CheckinDecision::Update { rule };
        assert_eq!(decide(None, b, None, false), CheckinDecision::Create);
        assert_eq!(decide(Some(b), b, None, false), CheckinDecision::Unchanged);
        assert_eq!(
            decide(Some(a), b, None, false),
            update(CheckinRule::FastForward)
        );
        assert_eq!(
            decide(Some(a), b, Some(a), true),
            update(CheckinRule::FastForward),
            "a fast-forward is named so even when forced"
        );
        assert_eq!(
            decide(Some(b), other, None, false),
            CheckinDecision::Diverged
        );
        assert_eq!(
            decide(Some(b), other, Some(b), false),
            update(CheckinRule::OwnLastSet)
        );
        assert_eq!(
            decide(Some(b), other, None, true),
            update(CheckinRule::Forced)
        );
        // Behind the server's branch: nothing to check in, unless forced, or
        // a checkout that set it last takes its own commit back.
        assert_eq!(decide(Some(b), a, None, false), CheckinDecision::Unchanged);
        assert_eq!(decide(Some(b), a, None, true), update(CheckinRule::Forced));
        assert_eq!(
            decide(Some(b), a, Some(b), false),
            update(CheckinRule::OwnLastSet)
        );
        for (rule, kind) in [
            (CheckinRule::FastForward, "fast_forward"),
            (CheckinRule::OwnLastSet, "own_last_set"),
            (CheckinRule::KeepsEveryCommit, "keeps_every_commit"),
            (CheckinRule::Forced, "forced"),
        ] {
            assert_eq!(rule.kind(), kind);
            assert_eq!(
                serde_json::to_value(rule).expect("json"),
                serde_json::json!(kind)
            );
        }
    }

    #[test]
    fn a_rebase_that_keeps_every_commit_by_change_id_needs_no_force() {
        const X: &str = "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
        const Y: &str = "yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy";
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        // The target moved from m0 to m1; the server's branch is x on m0.
        let m0 = commit(&repo, &[], "m0");
        let m1 = commit(&repo, &[m0], "m1");
        let x = commit_with_id(&repo, &[m0], "change x", Some(X));
        let decide = |prev, new, targets: &[ObjectId]| {
            decide_checkin(&repo, Some(prev), new, targets, &[], None, false).expect("decide")
        };
        let kept = CheckinDecision::Update {
            rule: CheckinRule::KeepsEveryCommit,
        };

        // A rebased copy of x (same id, other parent), alone or with more on top.
        let x2 = commit_with_id(&repo, &[m1], "change x", Some(X));
        assert_eq!(decide(x, x2, &[m1]), kept);
        let y = commit_with_id(&repo, &[x2], "add y", Some(Y));
        assert_eq!(decide(x, y, &[m1]), kept);
        let plain = commit_with_id(&repo, &[x2], "made with git", None);
        assert_eq!(decide(x, plain, &[m1]), kept);
        assert!(keeps_every_commit(&repo, x, y, &[m1], &[]).expect("keeps"));

        // A copy with another id, or with none, drops x.
        let renamed = commit_with_id(&repo, &[m1], "change x", Some(Y));
        assert_eq!(decide(x, renamed, &[m1]), CheckinDecision::Diverged);
        let unnamed = commit_with_id(&repo, &[m1], "change x", None);
        assert_eq!(decide(x, unnamed, &[m1]), CheckinDecision::Diverged);
        // So does a branch that leaves x out.
        let without = commit_with_id(&repo, &[m1], "add y", Some(Y));
        assert_eq!(decide(x, without, &[m1]), CheckinDecision::Diverged);

        // A commit of the server's branch the target already has is not
        // dropped: here x landed in the target (t), and the new branch is
        // other work on it.
        let t = commit(&repo, &[m1, x], "merge x");
        let next = commit_with_id(&repo, &[t], "next", Some(Y));
        assert_eq!(
            decide(x, next, &[t]),
            CheckinDecision::Update {
                rule: CheckinRule::FastForward
            }
        );
        let on_server = commit_with_id(&repo, &[m0], "server only", Some(Y));
        let branch = commit_with_id(&repo, &[x], "on x", Some(Y));
        let fresh = commit_with_id(&repo, &[t], "on y", Some(Y));
        assert_eq!(
            decide(branch, fresh, &[t]),
            kept,
            "x is on the target; the branch's own commit is kept by id"
        );
        assert_eq!(
            decide(on_server, without, &[t]),
            kept,
            "the server-only commit is matched by id"
        );
        let other_work = commit_with_id(&repo, &[t], "other", Some(X));
        assert_eq!(
            decide(branch, other_work, &[t]),
            CheckinDecision::Diverged,
            "the branch's own commit is gone"
        );

        // set_checked_in_branch uses the same rule, naming it.
        crate::refs::write_ref(repo.git_dir(), "refs/heads/shared", &x).expect("ref");
        assert_eq!(
            set_checked_in_branch(
                &repo,
                "shared",
                y,
                &[m1],
                &[],
                None,
                false,
                &who(),
                "checkin",
            )
            .expect("set"),
            Ok(BranchSet {
                status: CheckinStatus::Updated,
                previous: Some(x),
                rule: Some(CheckinRule::KeepsEveryCommit),
            })
        );
        assert_eq!(tip(&repo, "shared"), Some(y));
    }

    #[test]
    fn a_lost_race_is_decided_again_instead_of_overwriting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let winner = commit(&repo, &[], "winner");
        let loser = commit(&repo, &[], "loser");
        crate::refs::write_ref(repo.git_dir(), "refs/heads/shared", &winner).expect("ref");

        // The loser read the branch before the winner created it.
        let mut reads = 0;
        let decide = |grafted| Decide {
            grafted,
            targets: &[],
            paths: &[],
            set_last: None,
            force: false,
        };
        let got = set_branch_reading(
            &repo,
            "refs/heads/shared",
            &decide(loser),
            &who(),
            "checkin",
            || {
                reads += 1;
                Ok(if reads == 1 {
                    None
                } else {
                    tip(&repo, "shared")
                })
            },
        )
        .expect("set");
        assert_eq!(
            got,
            Err(CheckinRefusal::Diverged {
                tip: winner.to_hex()
            })
        );
        assert_eq!(reads, 2);
        assert_eq!(
            tip(&repo, "shared"),
            Some(winner),
            "the winner's work stays"
        );

        // The same for an update that read a tip the branch has since left.
        let next = commit(&repo, &[winner], "next");
        crate::refs::write_ref(repo.git_dir(), "refs/heads/shared", &next).expect("ref");
        let ahead = commit(&repo, &[winner], "ahead");
        let mut reads = 0;
        let got = set_branch_reading(
            &repo,
            "refs/heads/shared",
            &decide(ahead),
            &who(),
            "checkin",
            || {
                reads += 1;
                Ok(if reads == 1 {
                    Some(winner)
                } else {
                    tip(&repo, "shared")
                })
            },
        )
        .expect("set");
        assert_eq!(got, Err(CheckinRefusal::Diverged { tip: next.to_hex() }));
        assert_eq!(tip(&repo, "shared"), Some(next));
    }

    #[test]
    fn case_variants_of_a_branch_are_refused_where_case_is_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let a = commit(&repo, &[], "a");
        crate::refs::write_ref(repo.git_dir(), "refs/heads/feature", &a).expect("ref");
        let config = repo.git_dir().join("config");
        let text = std::fs::read_to_string(&config).expect("config");
        let mut file =
            crate::config::ConfigFile::parse(&config, &text, crate::config::ConfigScope::Local)
                .expect("parse");
        file.set("core.ignorecase", "true").expect("set");
        file.write().expect("write");
        repo.reload_config().expect("reload");
        let b = commit(&repo, &[a], "b");
        assert_eq!(
            set_checked_in_branch(
                &repo,
                "Feature",
                b,
                &[],
                &[],
                None,
                false,
                &who(),
                "checkin",
            )
            .expect("set"),
            Err(CheckinRefusal::CaseConflict {
                existing: "feature".to_owned()
            })
        );
        assert_eq!(
            set_checked_in_branch(
                &repo,
                "feature",
                b,
                &[],
                &[],
                None,
                false,
                &who(),
                "checkin",
            )
            .expect("set")
            .map(|s| s.status),
            Ok(CheckinStatus::Updated)
        );
    }

    #[test]
    fn checkin_branches_are_checked_before_they_are_grafted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        let odb = repo.odb();
        let blob = odb.write(ObjectKind::Blob, b"a").expect("blob");
        let full =
            super::super::tree::edit(odb, None, &[(b"src/a".to_vec(), Some((0o100644, blob)))])
                .expect("tree");
        let mut text = format!("tree {}\n", full.to_hex());
        text.push_str(
            "author A <a@e> 1700000000 +0000\ncommitter A <a@e> 1700000000 +0000\n\nbase\n",
        );
        let base = odb
            .write(ObjectKind::Commit, text.as_bytes())
            .expect("commit");
        let paths = vec!["src".to_owned()];
        let snap = super::super::make_snapshot(&repo, base, &paths, None).expect("snapshot");
        let work = commit(&repo, &[snap.commit], "work");
        let id = "0123456789abcdef0123";
        let checkout = CheckoutInfo {
            id: id.to_owned(),
            name: "demo".to_owned(),
            branch: "main".to_owned(),
            paths,
            base: base.to_hex(),
            root: snap.commit.to_hex(),
            root_object: String::new(),
            tree: snap.tree.to_hex(),
            roots: vec![super::super::SnapshotRoot {
                commit: snap.commit.to_hex(),
                base: base.to_hex(),
            }],
            files: 1,
            bytes: 1,
            bundle_bytes: 0,
            work: None,
        };
        let check = |name: &str, raw: &str, target: &str| {
            let branch = CheckinBranch {
                name: name.to_owned(),
                raw: raw.to_owned(),
            };
            graft_checkin_branch(&repo, &checkout, target, &branch, None, &[]).expect("check")
        };
        let raw = work.to_hex();
        assert_eq!(
            check("Main", &raw, "main").err(),
            Some(CheckinRefusal::TargetBranch {
                target: "main".to_owned()
            }),
            "the checkout's target, in any case"
        );
        assert_eq!(
            check("TRUNK", &raw, "trunk").err(),
            Some(CheckinRefusal::TargetBranch {
                target: "trunk".to_owned()
            }),
            "the repository's target now"
        );
        assert_eq!(
            check("a..b", &raw, "main").err(),
            Some(CheckinRefusal::InvalidName)
        );
        assert_eq!(
            check("topic", "aéééééééééééé", "main").err(),
            Some(CheckinRefusal::InvalidTip)
        );
        assert_eq!(
            check("topic", &raw, "main").err(),
            Some(CheckinRefusal::NotPushed {
                out_ref: out_ref(id, "topic")
            })
        );
        crate::refs::write_ref(repo.git_dir(), &out_ref(id, "topic"), &work).expect("push");
        let grafted = check("topic", &raw, "main").expect("grafted");
        assert_eq!(grafted.commits.len(), 1);
    }

    #[test]
    fn refusals_have_stable_kinds_and_wire_form() {
        let refusal = CheckinRefusal::Conflict {
            commit: "0123456789ab".to_owned(),
            path: "docs/d.md".to_owned(),
            outside_paths: true,
        };
        assert_eq!(refusal.kind(), "conflict");
        let json = serde_json::to_value(&refusal).expect("json");
        assert_eq!(json["kind"], "conflict");
        assert_eq!(json["outside_paths"], true);
        assert_eq!(
            serde_json::from_value::<CheckinRefusal>(json).ok(),
            Some(refusal)
        );
        assert_eq!(
            CheckinRefusal::from(GraftError::MergeConflict {
                commit: "c".to_owned(),
                paths: vec!["p".to_owned()],
            })
            .kind(),
            "merge_conflict"
        );
        for (r, kind) in [
            (CheckinRefusal::InvalidName, "invalid_name"),
            (CheckinRefusal::InvalidTip, "invalid_tip"),
            (CheckinRefusal::Diverged { tip: String::new() }, "diverged"),
        ] {
            assert_eq!(r.kind(), kind);
        }
    }
}
