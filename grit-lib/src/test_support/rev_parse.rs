//! Rev-parse helpers for workspace tests (outside semver).

use std::collections::HashMap;

use crate::objects::ObjectId;
use crate::repo::Repository;

/// Load graft parents for a repository (wraps the internal `rev_parse` reader).
#[must_use]
pub fn load_graft_parents(repo: &Repository) -> HashMap<ObjectId, Vec<ObjectId>> {
    crate::rev_parse::load_graft_parents(repo.git_dir())
}
