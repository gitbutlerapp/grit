//! Recognize a GitButler workspace.
//!
//! GitButler applies several branches at once by checking out `gitbutler/workspace`, whose tip
//! is a merge of every applied branch that GitButler maintains itself. That tip is bookkeeping,
//! not work: tools that show history or status should show the applied branches instead, and
//! should not commit on top of it, because GitButler rewrites the workspace branch and would
//! lose such a commit.
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
}
