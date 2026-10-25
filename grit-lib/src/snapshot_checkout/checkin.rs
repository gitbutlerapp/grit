//! The server's side of a check-in, one branch at a time: check what the
//! client pushed and graft it ([`graft_checkin_branch`]), then set the
//! server's branch to the graft ([`set_checked_in_branch`]).
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

use super::graft::{decide_checkin, graft, CheckinDecision, GraftError, Grafted};
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
///
/// # Returns
///
/// The graft, or `Ok(Err(..))` with why the branch is refused.
///
/// # Errors
///
/// Ref and object failures.
pub fn graft_checkin_branch(
    repo: &Repository,
    checkout: &CheckoutInfo,
    target: &str,
    branch: &CheckinBranch,
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
    Ok(graft(repo, &checkout.roots, &checkout.paths, raw)?.map_err(CheckinRefusal::from))
}

/// Where a check-in left one of the server's branches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BranchSet {
    /// [`CheckinStatus::Created`], [`CheckinStatus::Updated`] or
    /// [`CheckinStatus::Unchanged`].
    pub status: CheckinStatus,
    /// Where the branch was before; `None` when the server had no such branch.
    pub previous: Option<ObjectId>,
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
pub fn set_checked_in_branch(
    repo: &Repository,
    name: &str,
    grafted: ObjectId,
    set_last: Option<ObjectId>,
    force: bool,
    who: &Signature,
    message: &str,
) -> Result<std::result::Result<BranchSet, CheckinRefusal>> {
    let refname = format!("refs/heads/{name}");
    set_branch_reading(
        repo,
        &refname,
        grafted,
        set_last,
        force,
        who,
        message,
        || Ok(crate::refs::resolve_ref(repo.git_dir(), &refname).ok()),
    )
}

/// [`set_checked_in_branch`], reading the branch's tip with `read` before
/// each decision.
#[allow(clippy::too_many_arguments)]
fn set_branch_reading(
    repo: &Repository,
    refname: &str,
    grafted: ObjectId,
    set_last: Option<ObjectId>,
    force: bool,
    who: &Signature,
    message: &str,
    mut read: impl FnMut() -> Result<Option<ObjectId>>,
) -> Result<std::result::Result<BranchSet, CheckinRefusal>> {
    if let Some(existing) = case_variant(repo, refname)? {
        return Ok(Err(CheckinRefusal::CaseConflict { existing }));
    }
    for _ in 0..ATTEMPTS {
        let previous = read()?;
        let status = match decide_checkin(repo, previous, grafted, set_last, force)? {
            CheckinDecision::Unchanged => CheckinStatus::Unchanged,
            CheckinDecision::Diverged => {
                return Ok(Err(CheckinRefusal::Diverged {
                    tip: previous.map(|p| p.to_hex()).unwrap_or_default(),
                }))
            }
            CheckinDecision::Create => CheckinStatus::Created,
            CheckinDecision::Update => CheckinStatus::Updated,
        };
        if status != CheckinStatus::Unchanged
            && swap_ref(repo, refname, previous, grafted, who, message)? == Swap::Moved
        {
            continue;
        }
        return Ok(Ok(BranchSet { status, previous }));
    }
    Err(CheckoutError::RefKeptMoving {
        refname: refname.to_owned(),
    })
}

/// The server's branch whose name differs from `refname`'s only in case, when
/// the repository's file system can't tell them apart (`core.ignorecase`).
fn case_variant(repo: &Repository, refname: &str) -> Result<Option<String>> {
    let ignorecase = repo
        .config()?
        .get_bool("core.ignorecase")
        .and_then(|r| r.ok())
        .unwrap_or(false);
    if !ignorecase {
        return Ok(None);
    }
    let name = refname.trim_start_matches("refs/heads/");
    Ok(crate::refs::list_refs(repo.git_dir(), "refs/heads/")?
        .into_iter()
        .filter_map(|(r, _)| r.strip_prefix("refs/heads/").map(str::to_owned))
        .find(|existing| existing != name && same_branch_name(existing, name)))
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
            set_checked_in_branch(&repo, "topic", grafted, last, force, &who(), "checkin")
                .expect("set")
        };
        assert_eq!(
            set(a, None, false),
            Ok(BranchSet {
                status: CheckinStatus::Created,
                previous: None
            })
        );
        assert_eq!(
            set(a, None, false).map(|s| s.status),
            Ok(CheckinStatus::Unchanged)
        );
        assert_eq!(
            set(b, None, false).map(|s| s.status),
            Ok(CheckinStatus::Updated)
        );
        assert_eq!(
            set(other, None, false),
            Err(CheckinRefusal::Diverged { tip: b.to_hex() })
        );
        assert_eq!(tip(&repo, "topic"), Some(b));
        assert_eq!(
            set(other, Some(b), false).map(|s| s.previous),
            Ok(Some(b)),
            "the checkout that set it last may replace it"
        );
        assert_eq!(tip(&repo, "topic"), Some(other));
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
        let got = set_branch_reading(
            &repo,
            "refs/heads/shared",
            loser,
            None,
            false,
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
            ahead,
            None,
            false,
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
            set_checked_in_branch(&repo, "Feature", b, None, false, &who(), "checkin")
                .expect("set"),
            Err(CheckinRefusal::CaseConflict {
                existing: "feature".to_owned()
            })
        );
        assert_eq!(
            set_checked_in_branch(&repo, "feature", b, None, false, &who(), "checkin")
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
            graft_checkin_branch(&repo, &checkout, target, &branch).expect("check")
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
