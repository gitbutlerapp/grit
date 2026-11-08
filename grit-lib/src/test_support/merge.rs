//! Merge helpers for workspace tests (unstable; not part of semver surface).

use crate::error::Result;
use crate::index::IndexEntry;
use crate::objects::ObjectId;
use crate::repo::Repository;

/// Flatten a tree object into index entries ([`crate::porcelain::merge::tree_to_index_entries`]).
pub fn tree_to_index_entries(
    repo: &Repository,
    oid: &ObjectId,
    prefix: &str,
) -> Result<Vec<IndexEntry>> {
    crate::porcelain::merge::tree_to_index_entries(repo, oid, prefix)
}
