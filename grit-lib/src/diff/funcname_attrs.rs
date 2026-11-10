//! Shared `.gitattributes` rules for per-path diff funcname (hunk header) lookup.

use crate::attributes::{
    load_gitattributes_for_check_attr_source, load_gitattributes_for_diff, ParsedGitAttributes,
};
use crate::config::ConfigSet;
use crate::error::Result;
use crate::objects::ObjectId;
use crate::repo::Repository;
use crate::userdiff::{matcher_for_path_parsed, FuncnameMatcher};

/// Parsed attribute rules loaded once per diff, reused for every file path.
#[derive(Clone)]
pub(crate) struct PatchFuncnameAttrs {
    diff_stack: Option<ParsedGitAttributes>,
    commit_tree: Option<(ObjectId, ParsedGitAttributes)>,
}

impl PatchFuncnameAttrs {
    /// Load the same attribute stack as `git diff` (work tree, `attr.tree`, `GIT_ATTR_SOURCE`).
    ///
    /// # Errors
    ///
    /// Propagates attribute load failures.
    pub(crate) fn load_for_diff(repo: &Repository) -> Result<Self> {
        Ok(Self {
            diff_stack: Some(load_gitattributes_for_diff(repo)?),
            commit_tree: None,
        })
    }

    /// Attributes from one commit tree (`git show` / `git log -p`).
    ///
    /// # Errors
    ///
    /// Propagates attribute load failures.
    pub(crate) fn load_for_commit_tree(repo: &Repository, tree: &ObjectId) -> Result<Self> {
        Ok(Self {
            diff_stack: None,
            commit_tree: Some((*tree, load_gitattributes_for_check_attr_source(repo, tree)?)),
        })
    }

    /// Resolve the funcname matcher for one path.
    ///
    /// When `commit_tree` is set, uses that tree's attributes (log/show). Otherwise uses the
    /// diff stack from [`Self::load_for_diff`], loading on demand when absent.
    ///
    /// # Errors
    ///
    /// Propagates attribute and pattern errors.
    pub(crate) fn matcher_for_path(
        &self,
        _repo: &Repository,
        config: &ConfigSet,
        rel_path: &str,
    ) -> Result<Option<FuncnameMatcher>> {
        if let Some((_, parsed)) = &self.commit_tree {
            return Ok(matcher_for_path_parsed(
                config,
                &parsed.rules,
                &parsed.macros,
                rel_path,
                false,
            )?);
        }
        let parsed = match &self.diff_stack {
            Some(p) => p,
            None => {
                return Ok(matcher_for_path_parsed(
                    config,
                    &[],
                    &crate::worktree::attributes::MacroTable::default(),
                    rel_path,
                    false,
                )?);
            }
        };
        Ok(matcher_for_path_parsed(
            config,
            &parsed.rules,
            &parsed.macros,
            rel_path,
            false,
        )?)
    }
}
