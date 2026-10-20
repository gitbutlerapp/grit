//! Rename-following history walks for a single file path (`git log --follow` semantics).

use crate::error::{Error, Result};
use crate::objects::ObjectId;
use crate::repo::Repository;
use crate::rev_list::resolve_revision_specs_to_commits;
pub use crate::rev_list::{
    follow_history_commits, validate_follow_file_path_at_tip, FollowHistoryEntry,
};
use crate::rev_list_error::RevListError;
use crate::revwalk::queries::positive_specs_for_walk;
use crate::revwalk::RevWalkListOptions;

const DEFAULT_RENAME_THRESHOLD: u32 = 50;

/// Options for [`RevWalk::follow`](crate::revwalk::RevWalk::follow).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowWalkOptions {
    /// Revision walk limits and filters (`limit`, dates, `--first-parent`, …).
    pub walk: RevWalkListOptions,
    /// Rename similarity threshold (Git default 50%).
    pub rename_threshold: u32,
}

impl Default for FollowWalkOptions {
    fn default() -> Self {
        Self {
            walk: RevWalkListOptions::default(),
            rename_threshold: DEFAULT_RENAME_THRESHOLD,
        }
    }
}

/// One commit emitted by a follow walk, with the path name of the file at that commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowedCommit {
    /// Commit object id.
    pub oid: ObjectId,
    /// Repository-relative path to the tracked file in this commit's tree.
    pub path: String,
}

/// Lazy iterator over [`FollowedCommit`] values (see [`RevWalk::follow`](crate::revwalk::RevWalk::follow)).
pub struct FollowWalk {
    items: std::vec::IntoIter<FollowedCommit>,
}

impl FollowWalk {
    /// Collect all remaining commits.
    #[must_use]
    pub fn collect(self) -> Vec<FollowedCommit> {
        self.items.collect()
    }
}

impl Iterator for FollowWalk {
    type Item = FollowedCommit;

    fn next(&mut self) -> Option<Self::Item> {
        self.items.next()
    }
}

/// Walk history for `path`, following renames backward from revision tips in `tokens`.
pub fn follow_history(
    repo: &Repository,
    tokens: &[&str],
    path: &str,
    options: &FollowWalkOptions,
) -> Result<FollowWalk> {
    if options.walk.paths.len() > 1 {
        return Err(Error::RevList(RevListError::FollowMultiplePaths));
    }
    if !options.walk.paths.is_empty() && options.walk.paths[0] != path {
        return Err(Error::RevList(RevListError::FollowMultiplePaths));
    }

    let (parsed, positive, all_refs) = positive_specs_for_walk(repo, tokens, &options.walk)?;
    let mut include = resolve_revision_specs_to_commits(repo, &positive)?;
    if all_refs {
        include.extend(crate::rev_list::all_ref_commit_tips_for_walk(repo)?);
    }
    let exclude = resolve_revision_specs_to_commits(repo, &parsed.negative)?;
    if include.is_empty() {
        return Ok(FollowWalk {
            items: Vec::new().into_iter(),
        });
    }

    validate_follow_file_path_at_tip(repo, include[0], path)?;

    let excluded: std::collections::HashSet<ObjectId> = exclude.into_iter().collect();
    let raw = follow_history_commits(
        repo,
        &include,
        &excluded,
        path,
        &options.walk,
        options.rename_threshold,
    )?;
    let items = raw
        .into_iter()
        .map(|(oid, path)| FollowedCommit { oid, path })
        .collect::<Vec<_>>()
        .into_iter();
    Ok(FollowWalk { items })
}
