//! Typed revision-range parsing and high-level rev-walk queries on [`super::RevWalk`].
use crate::error::{Error, Result};
use crate::git_date::parse::{parse_date_basic_at, DateParseError};
use crate::merge_base::{
    commits_reachable_excluding_ancestors_of_cutoffs,
    count_commits_reachable_excluding_ancestors_of, is_ancestor as merge_base_is_ancestor,
    merge_bases_first_vs_rest, ReachableCountFilter, ReachableWalkCommitterCutoffs,
};
use crate::objects::parse_commit;
use crate::objects::ObjectId;
use crate::refs::{list_refs_for_repository, list_refs_glob_for_repository};
use crate::repo::Repository;
use crate::rev_list::{
    cherry_equivalent_commits, commit_output_parents, rev_list, split_revision_token, OutputMode,
    RevListOptions, RevListResult,
};
use crate::rev_list_error::RevListError;
use crate::rev_parse::{
    peel_to_commit_for_merge_base, resolve_revision, resolve_revision_for_range_end,
    split_triple_dot_range,
};
use std::collections::HashSet;

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

/// Cherry-pick equivalence handling for symmetric ranges (`A...B`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CherryMode {
    /// No patch-id filtering or marking.
    #[default]
    Off,
    /// Omit commits on one side that match a commit on the other (`--cherry-pick`).
    Pick,
    /// Keep all commits but mark patch-id matches (`--cherry-mark`).
    Mark,
}

/// Left or right side of a symmetric range when `--left-right` is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevWalkSide {
    /// Reachable from the left tip only.
    Left,
    /// Reachable from the right tip only.
    Right,
}

/// One commit row from a [`RevWalk`] query with optional parent rewrite and range metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevWalkCommit {
    /// Commit object id.
    pub oid: ObjectId,
    /// Rewritten parent links when [`RevWalkListOptions::include_parents`] is set.
    pub parents: Vec<ObjectId>,
    /// Symmetric-range side when [`RevWalkListOptions::left_right`] is set.
    pub side: Option<RevWalkSide>,
    /// True when `--cherry-mark` matched this commit by patch-id.
    pub equivalent: bool,
}

/// Left/right commit counts for a symmetric range (`A...B`), matching `git rev-list --left-right --count`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeftRightCounts {
    /// Commits reachable from the left tip but not the right tip.
    pub left: usize,
    /// Commits reachable from the right tip but not the left tip.
    pub right: usize,
    /// With `--cherry-mark --left-right --count`, Git prints a third equivalent-commit total.
    pub equivalent: Option<usize>,
}

impl LeftRightCounts {
    /// Left/right totals without a cherry-mark equivalent column.
    #[must_use]
    pub const fn new(left: usize, right: usize) -> Self {
        Self {
            left,
            right,
            equivalent: None,
        }
    }
}

/// Options for listing or counting commits through [`RevWalk`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
    /// Include rewritten parent oids for each commit (`rev-list --parents`).
    pub include_parents: bool,
    /// Patch-id cherry handling on symmetric ranges.
    pub cherry: CherryMode,
    /// Classify commits as left/right on `A...B` (`rev-list --left-right`).
    pub left_right: bool,
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

fn rev_list_options_from_walk(
    repo: &Repository,
    walk: &RevWalkListOptions,
    parsed: &RevWalkParsedSpecs,
    all_refs: bool,
    count: bool,
) -> Result<RevListOptions> {
    let paths = repo.resolve_rev_walk_pathspecs(&walk.paths)?;
    let (min_parents, max_parents) = merge_filter_to_parent_bounds(walk.merges);
    let cherry_pick = walk.cherry == CherryMode::Pick;
    let cherry_mark = walk.cherry == CherryMode::Mark;
    let needs_side_map = walk.left_right || cherry_pick || cherry_mark;
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
        use_commit_graph: true,
        use_commit_graph_bloom: !walk.paths.is_empty(),
        paths_resolved_from_argv: !walk.paths.is_empty(),
        symmetric_left: parsed.symmetric_left,
        symmetric_right: parsed.symmetric_right,
        output_mode: if walk.include_parents {
            OutputMode::Parents
        } else {
            OutputMode::OidOnly
        },
        parent_rewrite: walk.include_parents,
        left_right: needs_side_map,
        left_right_explicit: walk.left_right,
        cherry_pick,
        cherry_mark,
        ..RevListOptions::default()
    })
}

/// Recompute rewritten parent links after the output commit set changed (e.g. `-n` truncation).
///
/// # Errors
///
/// Propagates pathspec resolution and parent lookup errors.
pub fn refresh_parent_rewrite_entries(
    repo: &Repository,
    entries: &mut [RevWalkCommit],
    walk: &RevWalkListOptions,
) -> Result<()> {
    if !walk.include_parents || entries.is_empty() {
        return Ok(());
    }
    let paths = repo.resolve_rev_walk_pathspecs(&walk.paths)?;
    let rev_list_options = RevListOptions {
        first_parent: walk.first_parent,
        paths,
        output_mode: OutputMode::Parents,
        parent_rewrite: true,
        ..RevListOptions::default()
    };
    let output_set: HashSet<ObjectId> = entries.iter().map(|e| e.oid).collect();
    for entry in entries {
        entry.parents = commit_output_parents(repo, entry.oid, &output_set, &rev_list_options)?;
    }
    Ok(())
}

fn rev_walk_commits_from_result(
    repo: &Repository,
    result: &RevListResult,
    list_options: &RevWalkListOptions,
    rev_list_options: &RevListOptions,
) -> Result<Vec<RevWalkCommit>> {
    let output_set: HashSet<ObjectId> = result.commits.iter().copied().collect();
    let mut out = Vec::with_capacity(result.commits.len());
    for &oid in &result.commits {
        let parents = if list_options.include_parents {
            commit_output_parents(repo, oid, &output_set, rev_list_options)?
        } else {
            Vec::new()
        };
        let side = if list_options.left_right {
            result.left_right_map.get(&oid).copied().map(|left| {
                if left {
                    RevWalkSide::Left
                } else {
                    RevWalkSide::Right
                }
            })
        } else {
            None
        };
        let equivalent =
            list_options.cherry == CherryMode::Mark && result.cherry_equivalent.contains(&oid);
        out.push(RevWalkCommit {
            oid,
            parents,
            side,
            equivalent,
        });
    }
    Ok(out)
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

fn positive_specs_for_walk(
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

/// Left/right counts when filters force a full `rev-list` walk (paths, skip, limit, …).
///
/// Applies [`RevWalkListOptions::limit`] once to the combined traversal, matching
/// `git rev-list --left-right --count -n`.
fn cherry_pick_left_right_counts_compatible(options: &RevWalkListOptions) -> bool {
    options.cherry == CherryMode::Pick
        && options.limit.is_none()
        && options.paths.is_empty()
        && options.refs.is_empty()
        && options.skip == 0
        && !options.reverse
        && !options.include_parents
        && !options.first_parent
        && options.merges == MergeFilter::Any
        && options.since.is_none()
        && options.until.is_none()
}

fn filter_side_commits_by_parent_count(
    repo: &Repository,
    oids: Vec<ObjectId>,
    min_parents: Option<usize>,
    max_parents: Option<usize>,
) -> Result<Vec<ObjectId>> {
    if min_parents.is_none() && max_parents.is_none() {
        return Ok(oids);
    }
    let min_p = min_parents.unwrap_or(0);
    let max_p = max_parents.unwrap_or(usize::MAX);
    let mut out = Vec::with_capacity(oids.len());
    for oid in oids {
        let obj = repo.odb().read(&oid)?;
        let commit = parse_commit(&obj.data)?;
        let parent_count = commit.parents.len();
        if parent_count >= min_p && parent_count <= max_p {
            out.push(oid);
        }
    }
    Ok(out)
}

fn try_side_commits_first_parent_to_merge_base(
    repo: &Repository,
    tip: ObjectId,
    merge_base: ObjectId,
) -> Result<Option<Vec<ObjectId>>> {
    if tip == merge_base {
        return Ok(Some(Vec::new()));
    }
    let mut out = Vec::new();
    let mut cur = tip;
    while cur != merge_base {
        out.push(cur);
        let obj = repo.odb().read(&cur)?;
        let commit = parse_commit(&obj.data)?;
        if commit.parents.len() != 1 {
            return Ok(None);
        }
        cur = commit.parents[0];
    }
    Ok(Some(out))
}

fn left_right_cherry_pick_counts(
    repo: &Repository,
    left: ObjectId,
    right: ObjectId,
    options: &RevWalkListOptions,
) -> Result<LeftRightCounts> {
    let (min_parents, max_parents) = merge_filter_to_parent_bounds(options.merges);
    let merge_base = merge_bases_first_vs_rest(repo, left, &[right])?
        .into_iter()
        .next()
        .ok_or_else(|| Error::Message("symmetric range has no merge base".into()))?;
    let mut left_only = match try_side_commits_first_parent_to_merge_base(repo, left, merge_base)? {
        Some(list) => list,
        None => commits_reachable_excluding_ancestors_of_cutoffs(
            repo,
            &[left],
            &[right],
            None,
            ReachableWalkCommitterCutoffs::default(),
        )?,
    };
    let mut right_only = match try_side_commits_first_parent_to_merge_base(repo, right, merge_base)?
    {
        Some(list) => list,
        None => commits_reachable_excluding_ancestors_of_cutoffs(
            repo,
            &[right],
            &[left],
            None,
            ReachableWalkCommitterCutoffs::default(),
        )?,
    };
    left_only = filter_side_commits_by_parent_count(repo, left_only, min_parents, max_parents)?;
    right_only = filter_side_commits_by_parent_count(repo, right_only, min_parents, max_parents)?;
    let paths = repo.resolve_rev_walk_pathspecs(&options.paths)?;
    let cherry_equivalent = cherry_equivalent_commits(repo, &left_only, &right_only, &paths)?;
    let left = left_only
        .iter()
        .filter(|oid| !cherry_equivalent.contains(oid))
        .count();
    let right = right_only
        .iter()
        .filter(|oid| !cherry_equivalent.contains(oid))
        .count();
    Ok(LeftRightCounts::new(left, right))
}

fn left_right_counts_from_combined_walk(
    repo: &Repository,
    options: &RevWalkListOptions,
    parsed: &RevWalkParsedSpecs,
    positive: &[String],
    all_refs: bool,
) -> Result<LeftRightCounts> {
    let mut opts = rev_list_options_from_walk(repo, options, parsed, all_refs, false)?;
    opts.left_right = true;
    let result = rev_list(repo, positive, &parsed.negative, &opts)?;
    let mut left = 0usize;
    let mut right = 0usize;
    for oid in &result.commits {
        if options.cherry == CherryMode::Mark && result.cherry_equivalent.contains(oid) {
            continue;
        }
        match result.left_right_map.get(oid) {
            Some(true) => left += 1,
            Some(false) => right += 1,
            None => {}
        }
    }
    let equivalent = if options.cherry == CherryMode::Mark {
        Some(
            result
                .commits
                .iter()
                .filter(|oid| result.cherry_equivalent.contains(oid))
                .count(),
        )
    } else {
        None
    };
    Ok(LeftRightCounts {
        left,
        right,
        equivalent,
    })
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
        Ok(self
            .commit_entries(tokens, options)?
            .into_iter()
            .map(|row| row.oid)
            .collect())
    }

    /// List commits with optional rewritten parents, left/right sides, and cherry-mark flags.
    ///
    /// # Errors
    ///
    /// Propagates rev-parse and rev-list errors.
    pub fn commit_entries(
        &self,
        tokens: &[&str],
        options: &RevWalkListOptions,
    ) -> Result<Vec<RevWalkCommit>> {
        let repo = self.repository();
        let (parsed, positive, all_refs) = positive_specs_for_walk(repo, tokens, options)?;
        let opts = rev_list_options_from_walk(repo, options, &parsed, all_refs, false)?;
        let result = rev_list(repo, &positive, &parsed.negative, &opts)?;
        rev_walk_commits_from_result(repo, &result, options, &opts)
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

        if cherry_pick_left_right_counts_compatible(options) {
            return left_right_cherry_pick_counts(repo, left, right, options);
        }

        if options.cherry == CherryMode::Off
            && options.limit.is_none()
            && options.paths.is_empty()
            && options.refs.is_empty()
            && options.skip == 0
            && !options.reverse
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
            return Ok(LeftRightCounts::new(left_count, right_count));
        }

        left_right_counts_from_combined_walk(repo, options, &parsed, &positive, all_refs)
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
