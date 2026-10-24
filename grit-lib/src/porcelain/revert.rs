//! Revert-specific helpers (revision ordering and commit messages).
//!
//! Single-commit revert replay (three-way merge with swapped sides, checkout,
//! commit, and ref update) is implemented in [`crate::porcelain::replay`]
//! ([`crate::porcelain::replay::ReplayDirection::Revert`]). Shared merge-strategy helpers live in
//! [`crate::porcelain::cherry_pick`]; range revert ordering and the default
//! revert message template live here.
//!
//! # What this module owns
//!
//! - [`commits_to_revert`] — expand revert revision arguments into the commits
//!   to revert, in the order they are applied.
//! - [`revision_range_apply_order`] — order the commits of a revert revision set
//!   (`<include> --not <exclude>`) tip-first, the order `git revert` replays a
//!   range in.
//! - [`merge_commit_message_for_revert`] — build the `Revert "..."` /
//!   `Reapply "..."` subject and the `This reverts commit ...` body for the
//!   revert commit message (plain or `--reference` form).

use crate::commit_pretty::format_reference_line;
use crate::error::Result;
use crate::objects::{CommitData, ObjectId};
use crate::repo::Repository;
use crate::rev_list::{resolve_revision_specs_to_commits, rev_list, OrderingMode, RevListOptions};
use crate::rev_parse::resolve_revision_as_commit;
use crate::revwalk::parse_rev_walk_tokens;

/// Expand revert revision arguments into the commits to revert, in apply order.
///
/// When any spec uses range syntax (`A..B`, `A...B`, `^X`), all specs together
/// form one revision set (`git rev-list <specs>`), reverted tip-first via
/// [`revision_range_apply_order`]. Otherwise each spec names one commit and the
/// commits are reverted in argument order. An empty `specs` yields no commits.
///
/// # Parameters
///
/// - `repo` — repository the specs are resolved in.
/// - `specs` — revision arguments as given by the user.
///
/// # Returns
///
/// Commit ids in the order the revert sequence applies them.
///
/// # Errors
///
/// Rev-parse errors when a spec does not resolve to a commit (for example a tree-ish),
/// and rev-list or object read errors while walking a range.
pub fn commits_to_revert(repo: &Repository, specs: &[String]) -> Result<Vec<ObjectId>> {
    let tokens: Vec<&str> = specs.iter().map(String::as_str).collect();
    let parsed = parse_rev_walk_tokens(repo, &tokens)?;
    let include = resolve_revision_specs_to_commits(repo, &parsed.positive)?;
    let exclude = resolve_revision_specs_to_commits(repo, &parsed.negative)?;
    let range_syntax = tokens
        .iter()
        .any(|t| t.contains("..") || t.starts_with('^'))
        || !exclude.is_empty()
        || parsed.symmetric_left.is_some();
    if range_syntax {
        return revision_range_apply_order(repo, &include, &exclude);
    }
    tokens
        .iter()
        .map(|spec| resolve_revision_as_commit(repo, spec))
        .collect()
}

/// Commits reachable from `include` but not from `exclude`, in revert apply order.
///
/// Uses the same revision set as `git rev-list <include> --not <exclude>` with
/// `--topo-order` (tip before ancestors) so skewed committer dates cannot reorder
/// descendants before their parents.
///
/// # Errors
///
/// Propagates rev-list and object read errors.
pub fn revision_range_apply_order(
    repo: &Repository,
    include: &[ObjectId],
    exclude: &[ObjectId],
) -> Result<Vec<ObjectId>> {
    let positive: Vec<String> = include.iter().map(ObjectId::to_hex).collect();
    let negative: Vec<String> = exclude.iter().map(ObjectId::to_hex).collect();
    let opts = RevListOptions {
        ordering: OrderingMode::Topo,
        ..RevListOptions::default()
    };
    Ok(rev_list(repo, &positive, &negative, &opts)?.commits)
}

/// Build the revert commit-message subject and body for `commit`.
///
/// Returns `(title_line, body_suffix)`. The plain form is
/// `Revert "<subject>"` / `This reverts commit <oid>.` (or `Reapply "<subject>"`
/// when reverting a prior `Revert "..."`). The `--reference` form
/// (`use_reference`) emits the `*** SAY WHY ... ***` placeholder title and a
/// `This reverts commit <short> (<subject>, <date>).` body using
/// `format_reference_line` in the commit helper module; `comment_char` is the
/// configured comment character used to prefix the placeholder title.
#[must_use]
pub fn merge_commit_message_for_revert(
    commit: &CommitData,
    commit_oid: ObjectId,
    use_reference: bool,
    comment_char: char,
    mainline_parent: Option<ObjectId>,
) -> (String, String) {
    let subject_line = commit.message.lines().next().unwrap_or("");
    let oid_full = commit_oid.to_hex();

    if use_reference {
        let title = format!("{comment_char} *** SAY WHY WE ARE REVERTING ON THE TITLE LINE ***");
        let ref_line = format_reference_line(&commit_oid, subject_line, &commit.committer, 7);
        // Trailing blank line matches the template file `git revert --edit` presents
        // (see t3501 "git revert --reference with core.commentChar").
        let body = format!("This reverts commit {ref_line}.\n\n");
        return (title, body);
    }

    let body = if let Some(parent) = mainline_parent {
        format!(
            "This reverts merge commit {oid_full} (mainline parent {parent}).\n",
            parent = parent.to_hex()
        )
    } else {
        format!("This reverts commit {oid_full}.\n")
    };

    if let Some(rest) = subject_line.strip_prefix("Revert \"") {
        if let Some(orig) = rest.strip_suffix('"') {
            if !orig.starts_with("Revert \"") {
                let title = format!("Reapply \"{orig}\"\n");
                return (title, body);
            }
        }
    }

    let title = format!("Revert \"{subject_line}\"\n");
    (title, body)
}

/// Extra body line when reverting a merge with `--mainline` (grit-specific wording).
#[must_use]
pub fn revert_mainline_extra_body(_reverted: ObjectId, mainline_parent: ObjectId) -> String {
    format!(
        "Mainline parent {} was used when building the revert tree.\n",
        mainline_parent.to_hex()
    )
}

/// Note appended when reverting a merge using an explicit mainline parent.
#[must_use]
pub fn merge_mainline_revert_note(source_oid: ObjectId, mainline_parent: ObjectId) -> String {
    format!(
        "Reverted merge {} using mainline parent {}.\n\n",
        source_oid.to_hex(),
        mainline_parent.to_hex()
    )
}
