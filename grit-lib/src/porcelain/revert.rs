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
//! - [`revision_set_newest_first`] — order the commits of a revert revision set
//!   (`<include> --not <exclude>`) newest-first, the order `git revert` replays a
//!   range in.
//! - [`merge_commit_message_for_revert`] — build the `Revert "..."` /
//!   `Reapply "..."` subject and the `This reverts commit ...` body for the
//!   revert commit message (plain or `--reference` form).

use std::collections::HashSet;

use crate::commit_pretty::format_reference_line;
use crate::error::Result;
use crate::objects::{parse_commit, CommitData, ObjectId};
use crate::repo::Repository;

/// Commits reachable from `include` tips but not from `exclude` tips, newest-first.
///
/// Mirrors `git rev-list <include> --not <exclude>` ordering for revert: a first-parent
/// reachability walk collecting commits whose ancestry is not pruned by an excluded tip,
/// returned in descending committer-date order (newest first), matching how `git revert`
/// replays a range.
///
/// # Errors
///
/// Returns an error if an included commit cannot be read or parsed.
pub fn revision_set_newest_first(
    repo: &Repository,
    include: &[ObjectId],
    exclude: &[ObjectId],
) -> Result<Vec<ObjectId>> {
    // Closure of all ancestors of the excluded tips (these commits are NOT reverted).
    let mut excluded: HashSet<ObjectId> = HashSet::new();
    let mut stack: Vec<ObjectId> = exclude.to_vec();
    while let Some(oid) = stack.pop() {
        if !excluded.insert(oid) {
            continue;
        }
        if let Ok(obj) = repo.odb.read(&oid) {
            if let Ok(commit) = parse_commit(&obj.data) {
                stack.extend(commit.parents.iter().copied());
            }
        }
    }

    // Closure of ancestors of the included tips, minus the excluded set.
    let mut seen: HashSet<ObjectId> = HashSet::new();
    let mut collected: Vec<(i64, ObjectId)> = Vec::new();
    let mut stack: Vec<ObjectId> = include.to_vec();
    while let Some(oid) = stack.pop() {
        if excluded.contains(&oid) || !seen.insert(oid) {
            continue;
        }
        let obj = repo.odb.read(&oid)?;
        let commit = parse_commit(&obj.data)?;
        let ts = committer_timestamp(&commit.committer);
        collected.push((ts, oid));
        stack.extend(commit.parents.iter().copied());
    }

    // Newest first: descending committer timestamp (stable on ties).
    collected.sort_by_key(|a| std::cmp::Reverse(a.0));
    Ok(collected.into_iter().map(|(_, oid)| oid).collect())
}

/// Parse the unix timestamp from an ident string (`Name <email> <ts> <tz>`).
#[must_use]
pub fn committer_timestamp(ident: &str) -> i64 {
    ident
        .rsplit(' ')
        .nth(1)
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0)
}

/// Build the revert commit-message subject and body for `commit`.
///
/// Returns `(title_line, body_suffix)`. The plain form is
/// `Revert "<subject>"` / `This reverts commit <oid>.` (or `Reapply "<subject>"`
/// when reverting a prior `Revert "..."`). The `--reference` form
/// (`use_reference`) emits the `*** SAY WHY ... ***` placeholder title and a
/// `This reverts commit <short> (<subject>, <date>).` body using
/// [`crate::commit_pretty::format_reference_line`]; `comment_char` is the
/// configured comment character used to prefix the placeholder title.
#[must_use]
pub fn merge_commit_message_for_revert(
    commit: &CommitData,
    commit_oid: ObjectId,
    use_reference: bool,
    comment_char: char,
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

    let body = format!("This reverts commit {oid_full}.\n");

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
