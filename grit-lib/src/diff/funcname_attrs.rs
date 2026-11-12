//! Shared `.gitattributes` rules for per-path diff funcname (hunk header) lookup.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::attributes::{
    collect_attrs_for_path, load_gitattributes_for_check_attr_source, load_gitattributes_for_diff,
    AttrValue, ParsedGitAttributes,
};
use crate::config::ConfigSet;
use crate::error::Result;
use crate::objects::ObjectId;
use crate::repo::Repository;
use crate::userdiff::{matcher_for_driver_arc, FuncnameMatcher};

/// Parsed attribute rules loaded once per diff, reused for every file path.
#[derive(Clone)]
pub(crate) struct PatchFuncnameAttrs {
    diff_stack: Option<ParsedGitAttributes>,
    commit_tree: Option<(ObjectId, ParsedGitAttributes)>,
    driver_matchers: Arc<Mutex<BTreeMap<String, Arc<FuncnameMatcher>>>>,
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
            driver_matchers: Arc::new(Mutex::new(BTreeMap::new())),
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
            driver_matchers: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    /// Shared matcher for one path (cheap `Arc` clone after the driver is warm).
    ///
    /// # Errors
    ///
    /// Propagates attribute and pattern errors.
    pub(crate) fn matcher_arc_for_path(
        &self,
        _repo: &Repository,
        config: &ConfigSet,
        rel_path: &str,
    ) -> Result<Option<Arc<FuncnameMatcher>>> {
        let (rules, macros) = if let Some((_, parsed)) = &self.commit_tree {
            (&parsed.rules[..], &parsed.macros)
        } else if let Some(parsed) = &self.diff_stack {
            (&parsed.rules[..], &parsed.macros)
        } else {
            return Ok(None);
        };
        let map = collect_attrs_for_path(rules, macros, rel_path, false);
        let Some(AttrValue::Value(driver)) = map.get("diff") else {
            return Ok(None);
        };
        let mut cache = self
            .driver_matchers
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(matcher) = cache.get(driver) {
            return Ok(Some(Arc::clone(matcher)));
        }
        let Some(matcher) = matcher_for_driver_arc(config, driver.as_str())? else {
            return Ok(None);
        };
        Ok(Some(cache.entry(driver.clone()).or_insert(matcher).clone()))
    }
}
