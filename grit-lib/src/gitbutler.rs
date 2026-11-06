//! Recognize a GitButler workspace.
//!
//! GitButler applies several branches at once by checking out `gitbutler/workspace`, whose tip
//! is a merge of every applied branch that GitButler maintains itself. That tip is bookkeeping,
//! not work: tools that show history or status should show the applied branches instead, and
//! should not commit on top of it, because GitButler rewrites the workspace branch and would
//! lose such a commit.
//!
//! [`GitButlerWorkspace::standing`] turns `HEAD`'s comparison with the target branch into the
//! workspace's view: the ahead count and commits without the workspace commit, and how far
//! each applied head is ahead on its own.
//!
//! ```no_run
//! # fn main() -> grit_lib::error::Result<()> {
//! use grit_lib::gitbutler::GitButlerWorkspace;
//! use grit_lib::prelude::{Repository, RepositoryOptions};
//!
//! let repo = Repository::discover(None, &RepositoryOptions::empty())?;
//! if let Some(workspace) = GitButlerWorkspace::detect(&repo)? {
//!     for head in &workspace.heads {
//!         println!("{} {}", head.commit, head.branches.join(", "));
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! The branch and the commit marker it looks for are constants:
//!
//! ```rust
//! use grit_lib::gitbutler::{WORKSPACE_BRANCH, WORKSPACE_COMMIT_MARKER};
//!
//! assert_eq!(WORKSPACE_BRANCH, "refs/heads/gitbutler/workspace");
//! assert!("GitButler Workspace Commit\n\nbranches: a, b".starts_with(WORKSPACE_COMMIT_MARKER));
//! ```

use crate::error::Result;
use crate::objects::ObjectId;
use crate::refs::branch_tracking::{TipComparison, TrackedComparison};
use crate::refs::list_refs_for_repository;
use crate::repo::Repository;
use crate::state::{resolve_head_for_repository, HeadState};

/// The branch GitButler checks out to apply several branches at once.
pub const WORKSPACE_BRANCH: &str = "refs/heads/gitbutler/workspace";

/// Text GitButler puts in the message of the workspace branch's tip commit.
pub const WORKSPACE_COMMIT_MARKER: &str = "GitButler Workspace Commit";

/// Local branches under this prefix are GitButler's own, not applied work.
const GITBUTLER_BRANCH_PREFIX: &str = "refs/heads/gitbutler/";

/// A checked-out GitButler workspace: `HEAD` is on [`WORKSPACE_BRANCH`] and its tip is the
/// workspace commit ([`WORKSPACE_COMMIT_MARKER`] in its message).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitButlerWorkspace {
    /// The workspace commit at the tip of [`WORKSPACE_BRANCH`].
    pub commit: ObjectId,
    /// The heads merged into the workspace commit, in its parent order.
    pub heads: Vec<WorkspaceHead>,
}

/// One head merged into a [`GitButlerWorkspace`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceHead {
    /// The merged commit (a parent of the workspace commit).
    pub commit: ObjectId,
    /// Short names of the local branches that point at [`Self::commit`], sorted, leaving out
    /// GitButler's own `gitbutler/*` branches. Empty when no branch points there.
    pub branches: Vec<String>,
}

impl GitButlerWorkspace {
    /// The workspace `repo` has checked out, or `None` when `HEAD` is not on one.
    ///
    /// `HEAD` must be on [`WORKSPACE_BRANCH`] and that branch's tip must be a commit whose
    /// message contains [`WORKSPACE_COMMIT_MARKER`]; a `gitbutler/workspace` branch with any
    /// other tip is treated as an ordinary branch.
    ///
    /// # Errors
    ///
    /// Returns ref store and object database errors from reading `HEAD`, the tip commit or the
    /// local branches.
    pub fn detect(repo: &Repository) -> Result<Option<Self>> {
        let HeadState::Branch {
            refname,
            oid: Some(tip),
            ..
        } = resolve_head_for_repository(repo)?
        else {
            return Ok(None);
        };
        if refname != WORKSPACE_BRANCH {
            return Ok(None);
        }
        let commit = repo.odb().read_commit(&tip)?;
        if !commit.message.contains(WORKSPACE_COMMIT_MARKER) {
            return Ok(None);
        }
        let branches = list_refs_for_repository(repo, "refs/heads/")?;
        let heads = commit
            .parents
            .iter()
            .map(|&parent| {
                let mut names: Vec<String> = branches
                    .iter()
                    .filter(|(name, oid)| {
                        *oid == parent && !name.starts_with(GITBUTLER_BRANCH_PREFIX)
                    })
                    .filter_map(|(name, _)| name.strip_prefix("refs/heads/"))
                    .map(str::to_owned)
                    .collect();
                names.sort();
                WorkspaceHead {
                    commit: parent,
                    branches: names,
                }
            })
            .collect();
        Ok(Some(Self { commit: tip, heads }))
    }

    /// The merged heads' commits, in parent order: where history of the applied work starts.
    #[must_use]
    pub fn head_commits(&self) -> Vec<ObjectId> {
        self.heads.iter().map(|head| head.commit).collect()
    }

    /// How the applied work stands against `target`, with the workspace commit left out.
    ///
    /// `target` is the comparison of `HEAD` (the workspace commit) with the target branch, as
    /// `BranchStanding::compute` returns it. The workspace commit is bookkeeping, so it is dropped from the ahead count
    /// and the newest ahead commits, and each head is compared with the target on its own.
    /// When `target` was computed without counts, no head is compared either.
    ///
    /// # Parameters
    ///
    /// - `repo`: the repository the workspace was detected in.
    /// - `target`: `HEAD`'s comparison with the target branch.
    ///
    /// # Errors
    ///
    /// Propagates object read and parse errors from comparing a head with the target.
    pub fn standing(
        &self,
        repo: &Repository,
        target: &TrackedComparison,
    ) -> Result<WorkspaceStanding> {
        let heads_ahead = match target.ahead {
            Some(_) => self
                .heads
                .iter()
                .map(|head| {
                    TipComparison::compute(repo, head.commit, target.oid, 0)
                        .map(|comparison| Some(comparison.ahead))
                })
                .collect::<Result<_>>()?,
            None => vec![None; self.heads.len()],
        };
        Ok(WorkspaceStanding {
            ahead: target.ahead.map(|n| n.saturating_sub(1)),
            newest_ahead: target
                .newest_ahead
                .iter()
                .copied()
                .filter(|&oid| oid != self.commit)
                .collect(),
            heads_ahead,
        })
    }
}

/// A [`GitButlerWorkspace`]'s standing against the target branch, from
/// [`GitButlerWorkspace::standing`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceStanding {
    /// Commits of the applied work the target lacks, not counting the workspace commit; `None`
    /// when the target comparison was made without counts.
    pub ahead: Option<usize>,
    /// The newest of those commits, newest first: the target comparison's newest ahead commits
    /// without the workspace commit.
    pub newest_ahead: Vec<ObjectId>,
    /// For each of [`GitButlerWorkspace::heads`], in order, how many of its commits the target
    /// lacks; every entry is `None` when the target comparison was made without counts.
    pub heads_ahead: Vec<Option<usize>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::RepositoryOptions;
    use crate::objects::ObjectKind;
    use crate::refs::{write_ref, write_symbolic_ref};
    use crate::repo::InitOptions;
    use tempfile::TempDir;

    fn write_commit(repo: &Repository, parents: &[ObjectId], msg: &str) -> Result<ObjectId> {
        let odb = repo.odb();
        let tree = odb.write_loose_materialize(ObjectKind::Tree, b"")?;
        let mut body = format!("tree {tree}\n");
        for parent in parents {
            body.push_str(&format!("parent {parent}\n"));
        }
        body.push_str(&format!(
            "author T <t@e.com> 100 +0000\ncommitter T <t@e.com> 100 +0000\n\n{msg}\n"
        ));
        odb.write_loose_materialize(ObjectKind::Commit, body.as_bytes())
    }

    /// `main` at `base`, `one` two commits past it, `two` one commit past it, and the workspace
    /// merging both checked out.
    fn workspace_repo(dir: &TempDir) -> Result<(Repository, ObjectId, ObjectId)> {
        let (repo, _) = Repository::init(
            dir.path(),
            &InitOptions {
                bare: true,
                ..InitOptions::default()
            },
        )?;
        let repo = Repository::open(dir.path(), &RepositoryOptions::empty())?;
        let base = write_commit(&repo, &[], "base")?;
        let one_a = write_commit(&repo, &[base], "one a")?;
        let one = write_commit(&repo, &[one_a], "one b")?;
        let two = write_commit(&repo, &[base], "two")?;
        let workspace = write_commit(&repo, &[one, two], WORKSPACE_COMMIT_MARKER)?;
        for (name, oid) in [("main", base), ("one", one), ("two", two)] {
            write_ref(repo.git_dir(), &format!("refs/heads/{name}"), &oid)?;
        }
        write_ref(repo.git_dir(), WORKSPACE_BRANCH, &workspace)?;
        write_symbolic_ref(repo.git_dir(), "HEAD", WORKSPACE_BRANCH)?;
        Ok((repo, base, workspace))
    }

    fn comparison(
        repo: &Repository,
        head: ObjectId,
        target: ObjectId,
        counts: bool,
    ) -> Result<TrackedComparison> {
        let tip = TipComparison::compute(repo, head, target, 10)?;
        Ok(TrackedComparison {
            name: "main".to_owned(),
            full_ref: "refs/heads/main".to_owned(),
            oid: target,
            ahead: counts.then_some(tip.ahead),
            behind: counts.then_some(tip.behind),
            newest_ahead: if counts { tip.newest_ahead } else { Vec::new() },
        })
    }

    #[test]
    fn standing_drops_the_workspace_commit_and_counts_each_head() -> Result<()> {
        let dir = TempDir::new().map_err(crate::error::Error::Io)?;
        let (repo, main, commit) = workspace_repo(&dir)?;
        let workspace = GitButlerWorkspace::detect(&repo)?.expect("a workspace");
        let target = comparison(&repo, commit, main, true)?;
        assert_eq!(target.ahead, Some(4));

        let standing = workspace.standing(&repo, &target)?;
        assert_eq!(standing.ahead, Some(3));
        assert_eq!(standing.newest_ahead.len(), 3);
        assert!(!standing.newest_ahead.contains(&commit));
        assert_eq!(standing.heads_ahead, [Some(2), Some(1)]);
        Ok(())
    }

    #[test]
    fn standing_without_counts_compares_no_head() -> Result<()> {
        let dir = TempDir::new().map_err(crate::error::Error::Io)?;
        let (repo, main, commit) = workspace_repo(&dir)?;
        let workspace = GitButlerWorkspace::detect(&repo)?.expect("a workspace");
        let standing = workspace.standing(&repo, &comparison(&repo, commit, main, false)?)?;
        assert_eq!(
            standing,
            WorkspaceStanding {
                ahead: None,
                newest_ahead: Vec::new(),
                heads_ahead: vec![None, None],
            }
        );
        Ok(())
    }

    #[test]
    fn standing_against_a_target_holding_the_workspace_counts_nothing_ahead() -> Result<()> {
        let dir = TempDir::new().map_err(crate::error::Error::Io)?;
        let (repo, _, commit) = workspace_repo(&dir)?;
        let workspace = GitButlerWorkspace::detect(&repo)?.expect("a workspace");
        let standing = workspace.standing(&repo, &comparison(&repo, commit, commit, true)?)?;
        assert_eq!(standing.ahead, Some(0));
        assert!(standing.newest_ahead.is_empty());
        assert_eq!(standing.heads_ahead, [Some(0), Some(0)]);
        Ok(())
    }
}
