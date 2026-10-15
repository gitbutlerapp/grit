//! Typed revision-range parsing and high-level rev-walk queries on [`super::RevWalk`].
use crate::error::Result;
use crate::merge_base::{
    is_ancestor as merge_base_is_ancestor, merge_bases_all, merge_bases_first_vs_rest,
};
use crate::objects::ObjectId;
use crate::repo::Repository;
use crate::rev_list::{rev_list, split_revision_token, RevListOptions, RevListResult};
use crate::rev_parse::{
    peel_to_commit_for_merge_base, resolve_revision, resolve_revision_for_range_end,
    split_triple_dot_range,
};

use super::RevWalk;

/// Options for listing or counting commits through [`RevWalk`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RevWalkListOptions {
    /// Maximum number of commits to return (matches `rev-list --max-count`).
    pub limit: Option<usize>,
    /// Follow only the first parent at merges.
    pub first_parent: bool,
    /// Limit history to commits that touch these paths (same rules as `rev-list --`).
    pub paths: Vec<String>,
}

/// Positive and negative revision specs produced by [`parse_rev_walk_tokens`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RevWalkParsedSpecs {
    /// Tips and ranges to include.
    pub positive: Vec<String>,
    /// Excluded tips (`^rev` and range left sides).
    pub negative: Vec<String>,
    /// Left tip when a token used symmetric difference (`A...B`).
    pub symmetric_left: Option<ObjectId>,
    /// Right tip when a token used symmetric difference (`A...B`).
    pub symmetric_right: Option<ObjectId>,
}

/// Parse user revision tokens into positive/negative specs for [`rev_list`].
///
/// Supports plain tips, `A..B`, `A...B`, and `^X` exclusions across multiple tokens.
///
/// # Errors
///
/// Returns rev-parse and merge-base errors from symmetric-diff expansion.
pub fn parse_rev_walk_tokens(repo: &Repository, tokens: &[&str]) -> Result<RevWalkParsedSpecs> {
    let mut positive = Vec::new();
    let mut negative = Vec::new();
    let mut symmetric_left = None;
    let mut symmetric_right = None;
    for token in tokens {
        if let Some((left, right)) = split_triple_dot_range(token) {
            let left_spec = if left.is_empty() { "HEAD" } else { left };
            let right_spec = if right.is_empty() { "HEAD" } else { right };
            let (pos, neg, left_oid, right_oid) =
                symmetric_rev_list_specs(repo, left_spec, right_spec)?;
            positive.extend(pos);
            negative.extend(neg);
            symmetric_left = Some(left_oid);
            symmetric_right = Some(right_oid);
        } else {
            let (pos, neg) = split_revision_token(token);
            positive.extend(pos);
            negative.extend(neg);
        }
    }
    Ok(RevWalkParsedSpecs {
        positive,
        negative,
        symmetric_left,
        symmetric_right,
    })
}

fn symmetric_rev_list_specs(
    repo: &Repository,
    left_spec: &str,
    right_spec: &str,
) -> Result<(Vec<String>, Vec<String>, ObjectId, ObjectId)> {
    let left_tip = resolve_revision_for_range_end(repo, left_spec)?;
    let right_tip = resolve_revision_for_range_end(repo, right_spec)?;
    let left = peel_to_commit_for_merge_base(repo, left_tip)?;
    let right = peel_to_commit_for_merge_base(repo, right_tip)?;
    let bases = merge_bases_first_vs_rest(repo, left, &[right])?;
    let mut negative = Vec::new();
    for base in bases {
        negative.push(base.to_hex());
    }
    Ok((vec![left.to_hex(), right.to_hex()], negative, left, right))
}

fn rev_list_options_from_walk(
    walk: &RevWalkListOptions,
    parsed: &RevWalkParsedSpecs,
    count: bool,
) -> RevListOptions {
    RevListOptions {
        first_parent: walk.first_parent,
        max_count: walk.limit,
        paths: walk.paths.clone(),
        count,
        use_commit_graph: true,
        use_commit_graph_bloom: !walk.paths.is_empty(),
        symmetric_left: parsed.symmetric_left,
        symmetric_right: parsed.symmetric_right,
        ..RevListOptions::default()
    }
}

fn commit_count_from_result(result: RevListResult) -> Result<usize> {
    Ok(result.commit_count())
}

impl<'repo> RevWalk<'repo> {
    /// Parse range tokens the same way [`Self::commits`] and [`Self::count`] do.
    pub fn parse_tokens(&self, tokens: &[&str]) -> Result<RevWalkParsedSpecs> {
        parse_rev_walk_tokens(self.repository(), tokens)
    }

    /// List commit object ids reachable from `tokens` in default `rev-list` order.
    ///
    /// # Errors
    ///
    /// Propagates rev-parse and rev-list errors.
    pub fn commits(&self, tokens: &[&str], options: &RevWalkListOptions) -> Result<Vec<ObjectId>> {
        let repo = self.repository();
        let parsed = parse_rev_walk_tokens(repo, tokens)?;
        let opts = rev_list_options_from_walk(options, &parsed, false);
        let result = rev_list(repo, &parsed.positive, &parsed.negative, &opts)?;
        Ok(result.commits)
    }

    /// Count reachable commits without collecting them, using the commit-graph when loaded.
    ///
    /// # Errors
    ///
    /// Propagates rev-parse and rev-list errors.
    pub fn count(&self, tokens: &[&str], options: &RevWalkListOptions) -> Result<usize> {
        let repo = self.repository();
        let parsed = parse_rev_walk_tokens(repo, tokens)?;
        let opts = rev_list_options_from_walk(options, &parsed, true);
        let result = rev_list(repo, &parsed.positive, &parsed.negative, &opts)?;
        commit_count_from_result(result)
    }

    /// Merge bases of two revisions (`git merge-base`, or `--all` when `all` is true).
    ///
    /// # Errors
    ///
    /// Propagates rev-parse, peel, and merge-base errors.
    pub fn merge_bases(&self, a: &str, b: &str, all: bool) -> Result<Vec<ObjectId>> {
        let repo = self.repository();
        let a_tip = resolve_revision(repo, a)?;
        let b_tip = resolve_revision(repo, b)?;
        let a_commit = peel_to_commit_for_merge_base(repo, a_tip)?;
        let b_commit = peel_to_commit_for_merge_base(repo, b_tip)?;
        if all {
            merge_bases_all(repo, &[a_commit, b_commit])
        } else {
            merge_bases_first_vs_rest(repo, a_commit, &[b_commit])
        }
    }

    /// Whether `ancestor` is reachable from `descendant` (`git merge-base --is-ancestor`).
    ///
    /// # Errors
    ///
    /// Propagates rev-parse, peel, and merge-base errors.
    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        let repo = self.repository();
        let ancestor_tip = resolve_revision(repo, ancestor)?;
        let descendant_tip = resolve_revision(repo, descendant)?;
        let ancestor_commit = peel_to_commit_for_merge_base(repo, ancestor_tip)?;
        let descendant_commit = peel_to_commit_for_merge_base(repo, descendant_tip)?;
        merge_base_is_ancestor(repo, ancestor_commit, descendant_commit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::{InitOptions, Repository};

    #[test]
    fn split_double_dot_token_without_repo() {
        let (pos, neg) = crate::rev_list::split_revision_token("main..feature");
        assert_eq!(pos, vec!["feature".to_owned()]);
        assert_eq!(neg, vec!["main".to_owned()]);
    }
}
