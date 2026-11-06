//! Commit listing for `log --graph` (topological order, graph-friendly path options).

use crate::error::Result;
use crate::objects::ObjectId;
use crate::repo::Repository;
use crate::revwalk::queries::RevWalkListOptions;

/// List commits for `log --graph` in the same order as `git log --graph --format=%H`.
///
/// # Errors
///
/// Propagates rev-parse and rev-list errors.
pub fn list_commits_for_log_graph(
    repo: &Repository,
    tokens: &[&str],
    walk: &RevWalkListOptions,
) -> Result<Vec<ObjectId>> {
    let mut opts = walk.clone();
    opts.graph = true;
    repo.rev_walk().commits(tokens, &opts)
}
