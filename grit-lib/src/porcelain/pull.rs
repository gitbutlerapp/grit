//! `pull`: integrate the current branch's upstream after a fetch.
//!
//! A pull is two steps. [`plan_pull`] reads `HEAD` and `branch.<name>.*` config to decide
//! which remote to fetch from and which local ref will hold the upstream afterwards. The
//! caller then fetches (for example with [`crate::remote::Remote::fetch`], choosing its own
//! transport, progress sink and options) and calls [`integrate_upstream`], which
//! fast-forwards, merges, or (on an unborn branch) adopts the upstream as the branch's first
//! commit.
//!
//! # Examples
//!
//! ```no_run
//! use grit_lib::config::environment::RepositoryOptions;
//! use grit_lib::porcelain::pull::{integrate_upstream, plan_pull, PullSource};
//! use grit_lib::repo::Repository;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let repo = Repository::discover(None, &RepositoryOptions::empty())?;
//! let plan = plan_pull(&repo)?;
//! if let PullSource::Remote(name) = &plan.source {
//!     // fetch `name` here
//!     let _ = name;
//! }
//! let outcome = integrate_upstream(&repo, &plan)?;
//! # let _ = outcome;
//! # Ok(())
//! # }
//! ```

use thiserror::Error;

use crate::branch_tracking::upstream_ref_for_merge;
use crate::change_id::{logical_change_id_for_commit, ChangeId};
use crate::diff::tree_oid_of_commit_in_repo;
use crate::error::Error;
use crate::objects::ObjectId;
use crate::porcelain::checkout::checkout_between_trees;
use crate::porcelain::merge::{
    ensure_branch_tip, integrate_branch, update_checked_out_branch, IntegrateBranchOutcome,
};
use crate::porcelain::worktree_guard::prepare_tree_checkout;
use crate::refs::store::RawRef;
use crate::remote::DEFAULT_REMOTE;
use crate::repo::Repository;
use crate::rev_parse::abbreviate_ref_name;
use crate::state::{resolve_head, HeadState};

/// Reflog message for the branch created by pulling into an unborn branch.
const INITIAL_PULL_REFLOG: &str = "initial pull";

/// Errors from [`plan_pull`] and [`integrate_upstream`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PullError {
    /// `HEAD` is detached; a pull needs a checked-out branch to integrate into.
    #[error("HEAD is detached")]
    DetachedHead,
    /// `HEAD` could not be resolved to a branch or commit.
    #[error("HEAD is in an unknown state")]
    InvalidHead,
    /// The remote's fetch refspecs do not store the upstream branch in a local ref.
    #[error("'{merge}' from remote '{remote}' is not fetched into a remote-tracking ref")]
    UpstreamNotTracked {
        /// Remote name from `branch.<name>.remote` (or the default remote).
        remote: String,
        /// Remote ref from `branch.<name>.merge` (or the branch's own name).
        merge: String,
    },
    /// The local ref that should hold the upstream does not exist.
    #[error("upstream ref '{upstream_ref}' does not exist")]
    MissingUpstream {
        /// Full name of the missing ref (for example `refs/remotes/origin/main`).
        upstream_ref: String,
    },
    /// An underlying repository, merge, checkout or ref error.
    #[error(transparent)]
    Library(#[from] Error),
}

/// Where a pull fetches from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullSource {
    /// A configured remote, by name: fetch it before [`integrate_upstream`].
    Remote(String),
    /// The local repository (`branch.<name>.remote = .`): the upstream is a local branch and
    /// there is nothing to fetch.
    Local,
}

/// What a pull integrates, resolved by [`plan_pull`] before fetching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullPlan {
    /// Full ref of the checked-out branch (for example `refs/heads/main`).
    pub branch_ref: String,
    /// Short name of the checked-out branch (for example `main`).
    pub branch: String,
    /// Current tip of the branch, or `None` when it has no commits yet.
    pub head: Option<ObjectId>,
    /// Where to fetch from before integrating.
    pub source: PullSource,
    /// Local ref holding the upstream after the fetch (for example
    /// `refs/remotes/origin/main`, or `refs/heads/dev` for [`PullSource::Local`]).
    pub upstream_ref: String,
}

impl PullPlan {
    /// Short display name of the upstream (`origin/main`), also used in the merge commit
    /// message and reflog.
    #[must_use]
    pub fn upstream_label(&self) -> String {
        abbreviate_ref_name(&self.upstream_ref)
    }
}

/// Result of [`integrate_upstream`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullOutcome {
    /// The upstream was integrated into an existing branch: up to date, fast-forward, merge,
    /// or conflicts left in a merge-in-progress state.
    Integrated(IntegrateBranchOutcome),
    /// The branch had no commits; it now points at the upstream tip, which was checked out.
    Adopted {
        /// The upstream commit the branch now points at.
        oid: ObjectId,
        /// The commit's change id: its stored header, or the derived id when change ids are
        /// enabled.
        change_id: Option<ChangeId>,
    },
}

/// Resolve the checked-out branch, the remote to fetch from and the upstream ref.
///
/// The remote is `branch.<name>.remote` (default [`DEFAULT_REMOTE`]) and the remote branch
/// is `branch.<name>.merge` (default: the branch's own name). For a named remote, the merge
/// ref is mapped through `remote.<name>.fetch`, so custom and negative refspecs are honoured.
///
/// # Errors
///
/// - [`PullError::DetachedHead`] / [`PullError::InvalidHead`] when `HEAD` is not a branch.
/// - [`PullError::UpstreamNotTracked`] when the fetch refspecs do not store the upstream.
/// - [`PullError::Library`] when `HEAD` or the config cannot be read.
pub fn plan_pull(repo: &Repository) -> Result<PullPlan, PullError> {
    let (branch_ref, branch, head) = match resolve_head(repo.git_dir())? {
        HeadState::Branch {
            refname,
            short_name,
            oid,
        } => (refname, short_name, oid),
        HeadState::Detached { .. } => return Err(PullError::DetachedHead),
        HeadState::Invalid => return Err(PullError::InvalidHead),
    };
    let config = repo.config()?;
    let configured = |key: &str| {
        config
            .get(&format!("branch.{branch}.{key}"))
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    let remote = configured("remote").unwrap_or_else(|| DEFAULT_REMOTE.to_owned());
    let merge = configured("merge").unwrap_or_else(|| branch.clone());
    let upstream_ref = upstream_ref_for_merge(&config, &remote, &merge).ok_or_else(|| {
        PullError::UpstreamNotTracked {
            remote: remote.clone(),
            merge: merge.clone(),
        }
    })?;
    let source = if remote == "." {
        PullSource::Local
    } else {
        PullSource::Remote(remote)
    };
    Ok(PullPlan {
        branch_ref,
        branch,
        head,
        source,
        upstream_ref,
    })
}

/// Integrate the upstream named by `plan` into the checked-out branch.
///
/// With commits on the branch this is [`integrate_branch`] labelled with
/// [`PullPlan::upstream_label`]. On an unborn branch the upstream tree is checked out
/// (refusing to overwrite untracked files) and the branch is created at the upstream tip with
/// an `initial pull` reflog entry.
///
/// # Errors
///
/// - [`PullError::MissingUpstream`] when [`PullPlan::upstream_ref`] does not exist.
/// - [`PullError::Library`] for merge, checkout, ref compare-and-swap and I/O failures,
///   including local changes or untracked files that would be overwritten.
pub fn integrate_upstream(repo: &Repository, plan: &PullPlan) -> Result<PullOutcome, PullError> {
    let upstream = resolve_upstream(repo, &plan.upstream_ref)?;
    let Some(head) = plan.head else {
        return adopt_upstream(repo, &plan.branch_ref, upstream);
    };
    let outcome = integrate_branch(
        repo,
        &plan.branch_ref,
        head,
        upstream,
        &plan.upstream_label(),
    )?;
    Ok(PullOutcome::Integrated(outcome))
}

fn resolve_upstream(repo: &Repository, upstream_ref: &str) -> Result<ObjectId, PullError> {
    let missing = || PullError::MissingUpstream {
        upstream_ref: upstream_ref.to_owned(),
    };
    match repo.refs().read_raw(upstream_ref)? {
        None => Err(missing()),
        Some(RawRef::Direct(oid)) => Ok(oid),
        Some(RawRef::Symbolic(_)) => repo.refs().resolve(upstream_ref).map_err(|_| missing()),
    }
}

fn adopt_upstream(
    repo: &Repository,
    branch_ref: &str,
    upstream: ObjectId,
) -> Result<PullOutcome, PullError> {
    ensure_branch_tip(repo, branch_ref, None)?;
    let tree = tree_oid_of_commit_in_repo(repo, &upstream)?;
    prepare_tree_checkout(repo, None, &tree)?;
    checkout_between_trees(repo, None, &tree)?;
    update_checked_out_branch(repo, branch_ref, None, upstream, INITIAL_PULL_REFLOG)?;
    Ok(PullOutcome::Adopted {
        oid: upstream,
        change_id: logical_change_id_for_commit(repo, upstream)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::environment::RepositoryOptions;
    use std::path::Path;
    use std::process::{Command, Output};
    use tempfile::TempDir;

    fn git_output(dir: &Path, args: &[&str]) -> Output {
        Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@e.com")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@e.com")
            .output()
            .expect("run git")
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = git_output(dir, args);
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn open(dir: &Path) -> Repository {
        Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("open")
    }

    fn commit(dir: &Path, file: &str, msg: &str) -> ObjectId {
        std::fs::write(dir.join(file), msg.as_bytes()).expect("write");
        git(dir, &["add", file]);
        git(dir, &["commit", "-qm", msg]);
        ObjectId::from_hex(&git(dir, &["rev-parse", "HEAD"])).expect("oid")
    }

    /// An `origin` repository with one commit on `main`, cloned to `clone`.
    fn origin_and_clone(tmp: &TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
        let origin = tmp.path().join("origin");
        std::fs::create_dir_all(&origin).expect("mkdir");
        git(&origin, &["init", "-q", "-b", "main"]);
        commit(&origin, "a.txt", "one");
        git(tmp.path(), &["clone", "-q", "origin", "clone"]);
        (origin, tmp.path().join("clone"))
    }

    fn assert_fsck_clean(dir: &Path) {
        git(dir, &["fsck", "--strict", "--no-progress"]);
    }

    #[test]
    fn plan_matches_git_upstream_for_a_clone() {
        let tmp = TempDir::new().expect("tempdir");
        let (_, clone) = origin_and_clone(&tmp);
        let plan = plan_pull(&open(&clone)).expect("plan");
        assert_eq!(plan.branch_ref, "refs/heads/main");
        assert_eq!(plan.branch, "main");
        assert_eq!(plan.source, PullSource::Remote("origin".into()));
        assert_eq!(
            plan.upstream_ref,
            git(
                &clone,
                &["rev-parse", "--symbolic-full-name", "@{upstream}"]
            )
        );
        assert_eq!(plan.upstream_label(), "origin/main");
        assert!(plan.head.is_some());
    }

    #[test]
    fn plan_follows_custom_fetch_refspec_like_git() {
        let tmp = TempDir::new().expect("tempdir");
        let (_, clone) = origin_and_clone(&tmp);
        git(
            &clone,
            &[
                "config",
                "--replace-all",
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/mirror/*",
            ],
        );
        git(&clone, &["fetch", "-q", "origin"]);
        let plan = plan_pull(&open(&clone)).expect("plan");
        assert_eq!(plan.upstream_ref, "refs/remotes/mirror/main");
        assert_eq!(
            plan.upstream_ref,
            git(
                &clone,
                &["rev-parse", "--symbolic-full-name", "@{upstream}"]
            )
        );
    }

    #[test]
    fn plan_defaults_to_origin_and_the_branch_name() {
        let tmp = TempDir::new().expect("tempdir");
        let (_, clone) = origin_and_clone(&tmp);
        git(&clone, &["config", "--unset", "branch.main.remote"]);
        git(&clone, &["config", "--unset", "branch.main.merge"]);
        let plan = plan_pull(&open(&clone)).expect("plan");
        assert_eq!(plan.source, PullSource::Remote("origin".into()));
        assert_eq!(plan.upstream_ref, "refs/remotes/origin/main");
    }

    #[test]
    fn plan_local_upstream_needs_no_fetch() {
        let tmp = TempDir::new().expect("tempdir");
        let dir = tmp.path();
        git(dir, &["init", "-q", "-b", "main"]);
        commit(dir, "a.txt", "one");
        git(dir, &["branch", "dev"]);
        git(dir, &["branch", "--set-upstream-to", "dev", "main"]);
        let plan = plan_pull(&open(dir)).expect("plan");
        assert_eq!(plan.source, PullSource::Local);
        assert_eq!(plan.upstream_ref, "refs/heads/dev");
        assert_eq!(plan.upstream_label(), "dev");
    }

    #[test]
    fn plan_rejects_detached_head_and_untracked_upstream() {
        let tmp = TempDir::new().expect("tempdir");
        let (_, clone) = origin_and_clone(&tmp);
        git(
            &clone,
            &["config", "--add", "remote.origin.fetch", "^refs/heads/main"],
        );
        assert!(matches!(
            plan_pull(&open(&clone)),
            Err(PullError::UpstreamNotTracked { remote, merge })
                if remote == "origin" && merge == "refs/heads/main"
        ));
        git(&clone, &["checkout", "-q", "--detach"]);
        assert!(matches!(
            plan_pull(&open(&clone)),
            Err(PullError::DetachedHead)
        ));
    }

    #[test]
    fn integrate_fast_forwards_with_reflog() {
        let tmp = TempDir::new().expect("tempdir");
        let (origin, clone) = origin_and_clone(&tmp);
        let before = ObjectId::from_hex(&git(&clone, &["rev-parse", "HEAD"])).expect("oid");
        let tip = commit(&origin, "b.txt", "two");
        git(&clone, &["fetch", "-q", "origin"]);

        let repo = open(&clone);
        let plan = plan_pull(&repo).expect("plan");
        let outcome = integrate_upstream(&repo, &plan).expect("integrate");
        assert!(matches!(
            outcome,
            PullOutcome::Integrated(IntegrateBranchOutcome::FastForward { oid, .. }) if oid == tip
        ));
        assert_eq!(git(&clone, &["rev-parse", "main"]), tip.to_hex());
        assert_eq!(
            std::fs::read_to_string(clone.join("b.txt")).expect("b"),
            "two"
        );
        assert_eq!(git(&clone, &["status", "--porcelain"]), "");
        let reflog = git(&clone, &["reflog", "show", "--format=%H %gs", "-1", "main"]);
        assert_eq!(
            reflog,
            format!("{} merge origin/main: fast-forward", tip.to_hex())
        );
        let head_reflog = git(&clone, &["reflog", "show", "--format=%H", "-1", "HEAD"]);
        assert_eq!(head_reflog, tip.to_hex());
        assert_ne!(before, tip);
        assert_fsck_clean(&clone);

        let again = integrate_upstream(&open(&clone), &plan_pull(&repo).expect("plan"))
            .expect("integrate again");
        assert_eq!(
            again,
            PullOutcome::Integrated(IntegrateBranchOutcome::UpToDate)
        );
    }

    #[test]
    fn integrate_refuses_a_branch_moved_since_planning() {
        let tmp = TempDir::new().expect("tempdir");
        let (origin, clone) = origin_and_clone(&tmp);
        let tip = commit(&origin, "b.txt", "two");
        git(&clone, &["fetch", "-q", "origin"]);
        let repo = open(&clone);
        let plan = plan_pull(&repo).expect("plan");
        git(&clone, &["commit", "-q", "--allow-empty", "-m", "racing"]);
        let racing = git(&clone, &["rev-parse", "main"]);
        assert!(integrate_upstream(&repo, &plan).is_err());
        assert_eq!(git(&clone, &["rev-parse", "main"]), racing);
        assert!(!clone.join("b.txt").exists());
        assert_ne!(racing, tip.to_hex());
    }

    #[test]
    fn integrate_reports_missing_upstream() {
        let tmp = TempDir::new().expect("tempdir");
        let (_, clone) = origin_and_clone(&tmp);
        git(&clone, &["update-ref", "-d", "refs/remotes/origin/main"]);
        let repo = open(&clone);
        let plan = plan_pull(&repo).expect("plan");
        assert!(matches!(
            integrate_upstream(&repo, &plan),
            Err(PullError::MissingUpstream { upstream_ref }) if upstream_ref == "refs/remotes/origin/main"
        ));
    }

    /// A fresh repository with `origin` configured and fetched, still on unborn `main`.
    fn unborn_with_fetched_origin(tmp: &TempDir) -> (std::path::PathBuf, ObjectId) {
        let origin = tmp.path().join("origin");
        std::fs::create_dir_all(&origin).expect("mkdir");
        git(&origin, &["init", "-q", "-b", "main"]);
        let tip = commit(&origin, "a.txt", "one");
        let local = tmp.path().join("local");
        std::fs::create_dir_all(&local).expect("mkdir");
        git(&local, &["init", "-q", "-b", "main"]);
        git(&local, &["remote", "add", "origin", "../origin"]);
        git(&local, &["fetch", "-q", "origin"]);
        (local, tip)
    }

    #[test]
    fn integrate_adopts_upstream_on_unborn_branch() {
        let tmp = TempDir::new().expect("tempdir");
        let (local, tip) = unborn_with_fetched_origin(&tmp);
        let repo = open(&local);
        let plan = plan_pull(&repo).expect("plan");
        assert_eq!(plan.head, None);
        let outcome = integrate_upstream(&repo, &plan).expect("adopt");
        assert_eq!(
            outcome,
            PullOutcome::Adopted {
                oid: tip,
                change_id: Some(ChangeId::derived_from_commit_id(&tip)),
            }
        );
        assert_eq!(git(&local, &["rev-parse", "main"]), tip.to_hex());
        assert_eq!(
            std::fs::read_to_string(local.join("a.txt")).expect("a"),
            "one"
        );
        assert_eq!(git(&local, &["status", "--porcelain"]), "");
        let reflog = git(&local, &["reflog", "show", "--format=%H %gs", "main"]);
        assert_eq!(reflog, format!("{} initial pull", tip.to_hex()));
        assert_fsck_clean(&local);
    }

    #[test]
    fn adopted_change_id_respects_policy() {
        let tmp = TempDir::new().expect("tempdir");
        let (local, tip) = unborn_with_fetched_origin(&tmp);
        git(&local, &["config", "grit.changeIds", "false"]);
        let repo = open(&local);
        let outcome = integrate_upstream(&repo, &plan_pull(&repo).expect("plan")).expect("adopt");
        assert_eq!(
            outcome,
            PullOutcome::Adopted {
                oid: tip,
                change_id: None
            }
        );
    }

    #[test]
    fn adopt_refuses_to_overwrite_untracked_file() {
        let tmp = TempDir::new().expect("tempdir");
        let (local, _) = unborn_with_fetched_origin(&tmp);
        std::fs::write(local.join("a.txt"), "mine").expect("write");
        let repo = open(&local);
        let plan = plan_pull(&repo).expect("plan");
        assert!(matches!(
            integrate_upstream(&repo, &plan),
            Err(PullError::Library(_))
        ));
        assert_eq!(
            std::fs::read_to_string(local.join("a.txt")).expect("a"),
            "mine"
        );
        assert!(
            !git_output(&local, &["rev-parse", "--verify", "-q", "main"])
                .status
                .success()
        );
    }
}
