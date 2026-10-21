//! Typed revision-range parsing and high-level rev-walk queries on [`super::RevWalk`].
use crate::error::{Error, Result};
use crate::git_date::parse::{parse_date_basic_at, DateParseError};
use crate::merge_base::{
    count_commits_reachable_excluding_ancestors_of, is_ancestor as merge_base_is_ancestor,
    merge_bases_first_vs_rest, ReachableCountFilter,
};
use crate::objects::ObjectId;
use crate::refs::{list_refs_for_repository, list_refs_glob_for_repository};
use crate::repo::Repository;
use std::sync::Arc;

use crate::rev_list::{
    rev_list, split_revision_token, OrderingMode, RevListOptions, RevListResult,
};
use crate::rev_list_error::RevListError;
use crate::rev_parse::{
    peel_to_commit_for_merge_base, resolve_revision, resolve_revision_for_range_end,
    split_triple_dot_range,
};
use crate::revwalk::filter::CommitFilter;

use super::RevWalk;

/// Parent-count filter for commit lists (Git `--merges` / `--no-merges`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MergeFilter {
    /// Include commits with any parent count.
    #[default]
    Any,
    /// Only merge commits (at least two parents).
    OnlyMerges,
    /// Only non-merge commits (at most one parent).
    NoMerges,
}

/// Positive ref tips to include in a rev walk (Git `--all`, `--branches`, `--tags`, `--remotes`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefSelection {
    /// All refs, including `HEAD` when it points at a commit (`--all`).
    All,
    /// All branch tips, or those matching an optional `refs/heads/` glob pattern.
    Branches(Option<String>),
    /// All tag tips, or those matching an optional `refs/tags/` glob pattern.
    Tags(Option<String>),
    /// All remote-tracking tips, or those matching an optional `refs/remotes/` glob pattern.
    Remotes(Option<String>),
}

/// Left/right commit counts for a symmetric range (`A...B`), matching `git rev-list --left-right --count`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeftRightCounts {
    /// Commits reachable from the left tip but not the right tip.
    pub left: usize,
    /// Commits reachable from the right tip but not the left tip.
    pub right: usize,
}

/// Options for listing or counting commits through [`RevWalk`].
#[derive(Debug, Clone, Default)]
pub struct RevWalkListOptions {
    /// Maximum number of commits to return (matches `rev-list --max-count` / `-n`).
    pub limit: Option<usize>,
    /// Skip this many commits after ordering (matches `rev-list --skip`).
    pub skip: usize,
    /// Exclude commits with committer date strictly before this Unix timestamp (`--since` / `--after`).
    pub since: Option<i64>,
    /// Exclude commits with committer date strictly after this Unix timestamp (`--until` / `--before`).
    pub until: Option<i64>,
    /// Filter by parent count (`--merges` / `--no-merges`).
    pub merges: MergeFilter,
    /// Reverse the selected commit order (`--reverse`).
    pub reverse: bool,
    /// Additional positive ref tips (`--all`, `--branches`, `--tags`, `--remotes`).
    pub refs: Vec<RefSelection>,
    /// Follow only the first parent at merges.
    pub first_parent: bool,
    /// Limit history to commits that touch these paths (same rules as `rev-list --`).
    pub paths: Vec<String>,
    /// Author/committer/grep filter (same semantics as `git rev-list` / `git log`).
    pub commit_filter: Option<Arc<CommitFilter>>,
    /// Pickaxe filter (`-S` / `-G`).
    pub pickaxe: Option<Arc<crate::diff::Pickaxe>>,
    /// Topological order and graph-friendly path handling (`log --graph`).
    pub graph: bool,
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

fn expand_ref_selections(
    repo: &Repository,
    selections: &[RefSelection],
) -> Result<(bool, Vec<String>)> {
    let mut all_refs = false;
    let mut positive = Vec::new();
    for selection in selections {
        match selection {
            RefSelection::All => {
                all_refs = true;
            }
            RefSelection::Branches(pattern) => {
                positive.extend(ref_glob_tips(repo, "refs/heads/", pattern)?);
            }
            RefSelection::Tags(pattern) => {
                positive.extend(ref_glob_tips(repo, "refs/tags/", pattern)?);
            }
            RefSelection::Remotes(pattern) => {
                positive.extend(ref_glob_tips(repo, "refs/remotes/", pattern)?);
            }
        }
    }
    Ok((all_refs, positive))
}

fn ref_glob_tips(repo: &Repository, prefix: &str, pattern: &Option<String>) -> Result<Vec<String>> {
    let tips = if let Some(glob) = pattern {
        let full_pattern = format!("{prefix}{glob}");
        list_refs_glob_for_repository(repo, &full_pattern)?
    } else {
        list_refs_for_repository(repo, prefix)?
    };
    Ok(tips.into_iter().map(|(_, oid)| oid.to_hex()).collect())
}

fn merge_filter_to_parent_bounds(merges: MergeFilter) -> (Option<usize>, Option<usize>) {
    match merges {
        MergeFilter::Any => (None, None),
        MergeFilter::OnlyMerges => (Some(2), None),
        MergeFilter::NoMerges => (None, Some(1)),
    }
}

pub(crate) fn rev_list_options_from_walk(
    repo: &Repository,
    walk: &RevWalkListOptions,
    parsed: &RevWalkParsedSpecs,
    all_refs: bool,
    count: bool,
) -> Result<RevListOptions> {
    let paths = repo.resolve_rev_walk_pathspecs(&walk.paths)?;
    let (min_parents, max_parents) = merge_filter_to_parent_bounds(walk.merges);
    let graph = walk.graph;
    Ok(RevListOptions {
        first_parent: walk.first_parent,
        max_count: walk.limit,
        skip: walk.skip,
        since_cutoff: walk.since,
        until_cutoff: walk.until,
        reverse: walk.reverse,
        min_parents,
        max_parents,
        all_refs,
        paths,
        count,
        ordering: if graph {
            OrderingMode::Topo
        } else {
            OrderingMode::Default
        },
        preserve_simplify_merges_graph_merges: graph,
        path_graph_reorder: graph,
        parent_rewrite: graph && !walk.paths.is_empty(),
        use_commit_graph: true,
        use_commit_graph_bloom: !walk.paths.is_empty(),
        paths_resolved_from_argv: !walk.paths.is_empty(),
        symmetric_left: parsed.symmetric_left,
        symmetric_right: parsed.symmetric_right,
        commit_filter: walk.commit_filter.clone(),
        pickaxe: walk.pickaxe.clone(),
        ..RevListOptions::default()
    })
}

/// Parse a user date string into a Unix committer cutoff (Git `--since` / `--until`).
///
/// Uses Git's `parse_date_basic` rules with `now_sec` for relative dates.
///
/// # Errors
///
/// Returns [`Error::Message`] when the string cannot be parsed.
pub fn committer_cutoff_from_date(
    date: &str,
    now_sec: i64,
) -> std::result::Result<i64, DateParseError> {
    let trimmed = date.trim();
    if !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_digit()) {
        return trimmed.parse::<i64>().map_err(|_| DateParseError);
    }
    let (ts, _off) = parse_date_basic_at(date, now_sec)?;
    Ok(ts as i64)
}

/// Positive tips and hide tips for a default-order reachable walk (no path filter).
///
/// Used by [`crate::log_walk::LogWalkQuery`] to mirror `rev-list` tip resolution.
///
/// # Errors
///
/// Propagates rev-parse errors from resolving revision tokens and ref globs.
pub fn resolve_log_walk_tips_and_hide(
    repo: &Repository,
    tokens: &[&str],
    walk: &RevWalkListOptions,
) -> Result<(Vec<ObjectId>, Vec<ObjectId>)> {
    let (parsed, positive, all_refs) = positive_specs_for_walk(repo, tokens, walk)?;
    let mut include = crate::rev_list::resolve_revision_specs_to_commits(repo, &positive)?;
    if all_refs {
        include.extend(crate::rev_list::all_ref_commit_tips_for_walk(repo)?);
    }
    let exclude = crate::rev_list::resolve_revision_specs_to_commits(repo, &parsed.negative)?;
    Ok((include, exclude))
}

/// Commit OIDs reachable from `tokens`, preferring pack bitmap acceleration when compatible.
///
/// Used by author shortlog-style counts where walk order does not matter.
///
/// # Errors
///
/// Propagates rev-parse and rev-list errors.
pub fn rev_list_commits_for_author_group(
    repo: &Repository,
    tokens: &[&str],
    walk: &RevWalkListOptions,
) -> Result<Vec<ObjectId>> {
    let (parsed, positive, all_refs) = positive_specs_for_walk(repo, tokens, walk)?;
    let mut opts = rev_list_options_from_walk(repo, walk, &parsed, all_refs, false)?;
    opts.use_bitmap_index = true;
    let result = rev_list(repo, &positive, &parsed.negative, &opts)?;
    Ok(result.commits)
}

pub(crate) fn positive_specs_for_walk(
    repo: &Repository,
    tokens: &[&str],
    walk: &RevWalkListOptions,
) -> Result<(RevWalkParsedSpecs, Vec<String>, bool)> {
    let parsed = parse_rev_walk_tokens(repo, tokens)?;
    let (all_refs, mut positive) = expand_ref_selections(repo, &walk.refs)?;
    positive.extend(parsed.positive.iter().cloned());
    Ok((parsed, positive, all_refs))
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
        Ok(self.rev_list_result(tokens, options)?.commits)
    }

    /// Like [`Self::commits`], but retains bounded `--graph` linkage when available.
    ///
    /// # Errors
    ///
    /// Propagates rev-parse and rev-list errors.
    pub fn rev_list_result(
        &self,
        tokens: &[&str],
        options: &RevWalkListOptions,
    ) -> Result<crate::rev_list::RevListResult> {
        let repo = self.repository();
        let (parsed, positive, all_refs) = positive_specs_for_walk(repo, tokens, options)?;
        let opts = rev_list_options_from_walk(repo, options, &parsed, all_refs, false)?;
        rev_list(repo, &positive, &parsed.negative, &opts)
    }

    /// Count reachable commits without collecting them, using the commit-graph when loaded.
    ///
    /// # Errors
    ///
    /// Propagates rev-parse and rev-list errors.
    pub fn count(&self, tokens: &[&str], options: &RevWalkListOptions) -> Result<usize> {
        let repo = self.repository();
        let (parsed, positive, all_refs) = positive_specs_for_walk(repo, tokens, options)?;
        if parsed.negative.is_empty()
            && positive.len() == 1
            && options.skip == 0
            && options.limit.is_some()
            && (options.since.is_some() || options.until.is_some())
            && options.paths.is_empty()
            && options.refs.is_empty()
            && !options.reverse
            && options.merges == MergeFilter::Any
            && !options.first_parent
            && options
                .commit_filter
                .as_ref()
                .is_none_or(|f| !f.is_active())
            && options.pickaxe.is_none()
        {
            let tip = crate::rev_parse::resolve_revision_for_range_end(repo, &positive[0])?;
            let tip = crate::rev_parse::peel_to_commit_for_merge_base(repo, tip)?;
            let need = options.limit.unwrap_or(usize::MAX);
            let cutoffs = crate::merge_base::ReachableWalkCommitterCutoffs {
                since: options.since,
                until: options.until,
            };
            if let Some(list) =
                crate::merge_base::try_linear_chain_commits_with_cutoffs(repo, tip, need, cutoffs)?
            {
                return Ok(list.len());
            }
        }
        let opts = rev_list_options_from_walk(repo, options, &parsed, all_refs, true)?;
        let result = rev_list(repo, &positive, &parsed.negative, &opts)?;
        commit_count_from_result(result)
    }

    /// Left/right commit counts for a single symmetric range token (`A...B`).
    ///
    /// Matches `git rev-list --left-right --count` for the same range.
    ///
    /// # Errors
    ///
    /// Returns [`RevListError::SymmetricRangeRequired`] when `tokens` is not exactly one `A...B`
    /// range, or rev-parse / merge-base errors from resolving the tips.
    pub fn left_right_counts(
        &self,
        tokens: &[&str],
        options: &RevWalkListOptions,
    ) -> Result<LeftRightCounts> {
        if tokens.len() != 1 {
            return Err(Error::RevList(RevListError::SymmetricRangeRequired));
        }
        let repo = self.repository();
        let (parsed, positive, all_refs) = positive_specs_for_walk(repo, tokens, options)?;
        let (Some(left), Some(right)) = (parsed.symmetric_left, parsed.symmetric_right) else {
            return Err(Error::RevList(RevListError::SymmetricRangeRequired));
        };

        if options.paths.is_empty()
            && options.refs.is_empty()
            && options.skip == 0
            && options.limit.is_none()
            && !options.reverse
            && options
                .commit_filter
                .as_ref()
                .is_none_or(|f| !f.is_active())
            && options.pickaxe.is_none()
        {
            let (min_parents, max_parents) = merge_filter_to_parent_bounds(options.merges);
            let filter = ReachableCountFilter {
                min_parents,
                max_parents,
                first_parent_only: options.first_parent,
                since: options.since,
                until: options.until,
            };
            let left_count =
                count_commits_reachable_excluding_ancestors_of(repo, &[left], &[right], filter)?;
            let right_count =
                count_commits_reachable_excluding_ancestors_of(repo, &[right], &[left], filter)?;
            return Ok(LeftRightCounts {
                left: left_count,
                right: right_count,
            });
        }

        let mut opts = rev_list_options_from_walk(repo, options, &parsed, all_refs, true)?;
        // `--count` must not apply `-n`/`--max-count` per side; Git counts the full
        // left and right sets even when a list limit is also set.
        opts.max_count = None;
        opts.left_right = true;
        let left_count = {
            let mut left_opts = opts.clone();
            left_opts.left_only = true;
            rev_list(repo, &positive, &parsed.negative, &left_opts)?.commit_count()
        };
        let mut right_opts = opts;
        right_opts.right_only = true;
        let right_count = rev_list(repo, &positive, &parsed.negative, &right_opts)?.commit_count();
        Ok(LeftRightCounts {
            left: left_count,
            right: right_count,
        })
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
            crate::merge_base::merge_bases_all(repo, &[a_commit, b_commit])
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

    #[test]
    fn merge_filter_parent_bounds() {
        assert_eq!(
            merge_filter_to_parent_bounds(MergeFilter::OnlyMerges),
            (Some(2), None)
        );
        assert_eq!(
            merge_filter_to_parent_bounds(MergeFilter::NoMerges),
            (None, Some(1))
        );
    }
}
