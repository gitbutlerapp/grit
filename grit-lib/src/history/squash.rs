//! Squash a connected revision range into the oldest commit and replay descendants.

use std::collections::{HashMap, HashSet};

use crate::merge_base::is_ancestor;
use crate::objects::{CommitData, ObjectId};
use crate::repo::Repository;
use crate::revwalk::rev_list::{rev_list, OrderingMode, RevListOptions};
use crate::revwalk::rev_parse::{peel_to_commit_for_merge_base, resolve_revision_for_range_end};
use crate::revwalk::{parse_rev_walk_tokens, RevWalkParsedSpecs};
use crate::state::resolve_head_for_repository;

use super::error::HistoryError;
use super::message::{
    append_nth_squash_message, append_skipped_squash_message, commit_subject_single_line,
    fixup_replacement_message, message_body_after_subject, skip_fixupish_prefix,
    update_squash_message_for_fixup, SquashStep,
};
use super::plan::{plan_fold_range_at_head, plan_rewrite};
use super::reword::normalize_rewrite_message;
use super::types::RefScope;
use super::types::{CommitEdit, HistoryEdits, RewriteOptions, RewritePlan};

/// How to choose the message on the squashed commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SquashMessage {
    /// Keep the oldest commit's message (after `amend!` handling in the range).
    Oldest,
    /// Build an autosquash-style combined message from every commit in the range.
    Combine,
    /// Use an explicit message (normalized like [`super::reword::normalize_rewrite_message`]).
    Custom(String),
}

/// Inputs for [`squash`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SquashOptions {
    /// Message mode for the folded commit.
    pub message: SquashMessage,
    /// Ref scope, empty policy, and committer for planning.
    pub rewrite: RewriteOptions,
}

struct ResolvedSquashRange {
    commits: Vec<ObjectId>,
    oldest: ObjectId,
    tip: ObjectId,
    oldest_message: String,
}

/// Plan squashing `range` into one commit and replaying descendants.
///
/// `range` uses the same revision syntax as `git rev-list` (for example `base..tip`, `^excl tip`).
///
/// # Errors
///
/// Returns [`HistoryError`] when the range is invalid, markers refuse, or planning fails.
pub fn squash(
    repo: &Repository,
    range: &[&str],
    opts: &SquashOptions,
) -> Result<RewritePlan, HistoryError> {
    if range.is_empty() {
        return Err(HistoryError::Planning {
            message: "revision range is required".into(),
        });
    }
    let resolved = resolve_squash_range(repo, range, &opts.message)?;
    check_divergent_branch_tips(
        repo,
        resolved.oldest,
        resolved.tip,
        &resolved.commits,
        &opts.rewrite.scope,
    )?;
    let tip_commit = load_commit(repo, resolved.tip)?;
    let message = choose_squash_message(repo, &resolved, opts)?;
    if matches!(opts.rewrite.scope, RefScope::Head)
        && !matches!(opts.message, SquashMessage::Combine)
        && resolved.tip == repo.head_commit()?
    {
        return plan_fold_range_at_head(
            repo,
            &resolved.commits,
            resolved.oldest,
            resolved.tip,
            tip_commit.tree,
            &message,
            &opts.rewrite,
        );
    }
    let mut edits = HistoryEdits::new();
    for &oid in &resolved.commits {
        if oid == resolved.oldest {
            edits.insert(
                oid,
                CommitEdit::Rewrite {
                    tree: Some(tip_commit.tree),
                    message: Some(message.clone()),
                    author: None,
                    parents: None,
                },
            );
        } else {
            edits.insert(oid, CommitEdit::Drop);
        }
    }
    plan_rewrite(repo, &edits, &opts.rewrite)
}

fn load_commit(repo: &Repository, oid: ObjectId) -> Result<CommitData, HistoryError> {
    repo.odb().read_commit(&oid).map_err(planning_err)
}

fn planning_err(err: crate::error::Error) -> HistoryError {
    HistoryError::Planning {
        message: err.to_string(),
    }
}

fn resolve_squash_range(
    repo: &Repository,
    range: &[&str],
    message_policy: &SquashMessage,
) -> Result<ResolvedSquashRange, HistoryError> {
    let RevWalkParsedSpecs {
        positive, negative, ..
    } = parse_rev_walk_tokens(repo, range).map_err(planning_err)?;
    if negative.is_empty() {
        return Err(HistoryError::Planning {
            message: "not a '<base>..<tip>' revision range".into(),
        });
    }
    let walk_opts = RevListOptions {
        ancestry_path: true,
        reverse: true,
        ordering: OrderingMode::Topo,
        full_history: true,
        use_commit_graph: true,
        ..RevListOptions::default()
    };
    let walk = rev_list(repo, &positive, &negative, &walk_opts).map_err(planning_err)?;
    let commits = walk.commits;
    if commits.is_empty() {
        return Err(HistoryError::Planning {
            message: "the revision range is empty".into(),
        });
    }
    validate_connected_range(repo, &commits)?;
    let oldest = commits[0];
    let tip = *commits.last().ok_or_else(|| HistoryError::Planning {
        message: "the revision range is empty".into(),
    })?;
    if oldest == tip {
        return Err(HistoryError::Planning {
            message: "the revision range holds a single commit; nothing to squash".into(),
        });
    }
    let oldest_commit = load_commit(repo, oldest)?;
    if oldest_commit.parents.is_empty() {
        return Err(HistoryError::RootCommit { commit: oldest });
    }
    let selected: HashSet<ObjectId> = commits.iter().copied().collect();
    let subject_index = RangeSubjectIndex::build(repo, &commits, &selected)?;
    let oldest_message = apply_fixup_markers_in_range(
        repo,
        &commits,
        oldest,
        &oldest_commit.message,
        &subject_index,
        &selected,
        message_policy,
    )?;
    Ok(ResolvedSquashRange {
        commits,
        oldest,
        tip,
        oldest_message,
    })
}

fn check_divergent_branch_tips(
    repo: &Repository,
    oldest: ObjectId,
    tip: ObjectId,
    commits: &[ObjectId],
    scope: &super::types::RefScope,
) -> Result<(), HistoryError> {
    if !matches!(scope, super::types::RefScope::Branches) {
        return Ok(());
    }
    let selected: HashSet<ObjectId> = commits.iter().copied().collect();
    let branches = repo
        .references()
        .list("refs/heads/")
        .map_err(planning_err)?;
    for (name, branch_tip) in branches {
        if branch_tip == tip {
            continue;
        }
        if !is_ancestor(repo, oldest, branch_tip).map_err(planning_err)? {
            continue;
        }
        if is_ancestor(repo, tip, branch_tip).map_err(planning_err)? {
            continue;
        }
        if is_ancestor(repo, branch_tip, tip).map_err(planning_err)?
            && selected.contains(&branch_tip)
        {
            return Err(HistoryError::DivergentBranch { name });
        }
        if !is_ancestor(repo, branch_tip, tip).map_err(planning_err)? {
            return Err(HistoryError::DivergentBranch { name });
        }
    }
    if let Ok(crate::state::HeadState::Detached { oid }) = resolve_head_for_repository(repo) {
        if oid != tip
            && is_ancestor(repo, oldest, oid).map_err(planning_err)?
            && !is_ancestor(repo, tip, oid).map_err(planning_err)?
        {
            return Err(HistoryError::DivergentBranch {
                name: "HEAD".into(),
            });
        }
        if oid != tip
            && is_ancestor(repo, oldest, oid).map_err(planning_err)?
            && is_ancestor(repo, oid, tip).map_err(planning_err)?
            && selected.contains(&oid)
        {
            return Err(HistoryError::DivergentBranch {
                name: "HEAD".into(),
            });
        }
    }
    Ok(())
}

fn validate_connected_range(repo: &Repository, commits: &[ObjectId]) -> Result<(), HistoryError> {
    let selected: HashSet<ObjectId> = commits.iter().copied().collect();
    let oldest = commits[0];
    let oldest_commit = load_commit(repo, oldest)?;
    let oldest_parent_set: HashSet<ObjectId> = oldest_commit.parents.iter().copied().collect();
    let mut tip_flags: HashSet<ObjectId> = HashSet::new();
    let mut tip_count = 0usize;
    for &oid in commits {
        let commit = load_commit(repo, oid)?;
        if oid != oldest {
            for parent in &commit.parents {
                if selected.contains(parent) && tip_flags.remove(parent) {
                    tip_count = tip_count.saturating_sub(1);
                }
                let allowed = selected.contains(parent) || oldest_parent_set.contains(parent);
                if !allowed {
                    return Err(HistoryError::RangeNotConnected);
                }
            }
        }
        tip_flags.insert(oid);
        tip_count += 1;
    }
    if tip_count != 1 {
        return Err(HistoryError::MultipleTips);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FixupMarkerKind {
    Amend,
    Fixup,
    Squash,
}

struct ParsedMarker {
    kind: FixupMarkerKind,
    target_label: String,
}

struct RangeSubjectIndex {
    by_subject: HashMap<String, ObjectId>,
}

impl RangeSubjectIndex {
    fn build(
        repo: &Repository,
        commits: &[ObjectId],
        selected: &HashSet<ObjectId>,
    ) -> Result<Self, HistoryError> {
        let mut by_subject = HashMap::new();
        for &oid in commits {
            let commit = load_commit(repo, oid)?;
            let subject = commit_subject_single_line(&commit.message);
            if parse_fixup_marker(&subject).is_some() {
                continue;
            }
            by_subject.entry(subject).or_insert(oid);
        }
        for &oid in commits {
            let commit = load_commit(repo, oid)?;
            for parent in &commit.parents {
                if selected.contains(parent) {
                    continue;
                }
                let parent_commit = load_commit(repo, *parent)?;
                let subject = commit_subject_single_line(&parent_commit.message);
                if parse_fixup_marker(&subject).is_some() {
                    continue;
                }
                by_subject.entry(subject).or_insert(*parent);
            }
        }
        Ok(Self { by_subject })
    }

    fn lookup(&self, label: &str) -> Option<ObjectId> {
        self.by_subject.get(label).copied()
    }
}

fn peel_marker_target_label(subject: &str) -> String {
    let mut label = subject;
    while let Some(rest) = skip_fixupish_prefix(label) {
        label = rest.trim_start();
    }
    label.to_owned()
}

fn parse_fixup_marker(subject: &str) -> Option<ParsedMarker> {
    let (kind, rest) = if let Some(rest) = subject.strip_prefix("amend! ") {
        (FixupMarkerKind::Amend, rest)
    } else if let Some(rest) = subject.strip_prefix("fixup! ") {
        (FixupMarkerKind::Fixup, rest)
    } else {
        let rest = subject.strip_prefix("squash! ")?;
        (FixupMarkerKind::Squash, rest)
    };
    Some(ParsedMarker {
        kind,
        target_label: peel_marker_target_label(rest),
    })
}

struct OldestMessageDraft {
    text: String,
}

impl OldestMessageDraft {
    fn from_oldest_commit(message: &str) -> Self {
        Self {
            text: message.to_owned(),
        }
    }

    fn allows_message_discard(&self, policy: &SquashMessage) -> bool {
        matches!(policy, SquashMessage::Combine | SquashMessage::Custom(_))
    }

    fn apply_amend(
        &mut self,
        marker_oid: ObjectId,
        body: &str,
        target: &MarkerTarget,
        policy: &SquashMessage,
    ) -> Result<(), HistoryError> {
        if !target.folds_into_oldest_message() {
            return Err(HistoryError::MarkerTargetOutsideRange { commit: marker_oid });
        }
        if self.text.starts_with("squash! ") && !self.allows_message_discard(policy) {
            return Err(HistoryError::SquashDiscardsMessage { commit: marker_oid });
        }
        if self.text.starts_with("fixup! ") {
            self.text.replace_range(0..5, "amend!");
        }
        if self.text.starts_with("amend! ") {
            if let Some(pos) = self.text.find("\n\n") {
                self.text.truncate(pos + 1);
            }
        } else {
            self.text.clear();
        }
        self.text.push_str(body);
        if !self.text.ends_with('\n') {
            self.text.push('\n');
        }
        Ok(())
    }

    fn apply_squash(
        &mut self,
        marker_oid: ObjectId,
        body: &str,
        target: &MarkerTarget,
        policy: &SquashMessage,
    ) -> Result<(), HistoryError> {
        if !target.folds_into_oldest_message() {
            return Err(HistoryError::MarkerTargetOutsideRange { commit: marker_oid });
        }
        if self.text.starts_with("fixup! ") {
            if let Some(pos) = self.text.find('\n') {
                self.text.truncate(pos + 1);
            }
            self.text.replace_range(0..5, "squash!");
        }
        if self.text.starts_with("squash! ") {
            self.text.push('\n');
            self.text.push_str(body);
            if !self.text.ends_with('\n') {
                self.text.push('\n');
            }
            Ok(())
        } else if self.allows_message_discard(policy) {
            Ok(())
        } else {
            Err(HistoryError::SquashDiscardsMessage { commit: marker_oid })
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum MarkerTarget {
    OldestAnchor,
    InRange { oid: ObjectId },
    Outside { label: String },
}

struct OutsideTargetMarkerPolicy {
    allowed_outside_label: Option<String>,
}

impl OutsideTargetMarkerPolicy {
    fn compute(
        repo: &Repository,
        commits: &[ObjectId],
        oldest: ObjectId,
        index: &RangeSubjectIndex,
        selected: &HashSet<ObjectId>,
    ) -> Result<Self, HistoryError> {
        let mut allowed_outside_label: Option<String> = None;
        for &oid in commits {
            let commit = load_commit(repo, oid)?;
            let subject = commit_subject_single_line(&commit.message);
            let Some(marker) = parse_fixup_marker(&subject) else {
                return Ok(Self {
                    allowed_outside_label: None,
                });
            };
            let target =
                MarkerTarget::resolve(repo, &marker.target_label, oid, oldest, index, selected)?;
            let MarkerTarget::Outside { label } = target else {
                return Ok(Self {
                    allowed_outside_label: None,
                });
            };
            if let Some(seen) = &allowed_outside_label {
                if seen != &label {
                    return Ok(Self {
                        allowed_outside_label: None,
                    });
                }
            } else {
                allowed_outside_label = Some(label);
            }
        }
        Ok(Self {
            allowed_outside_label,
        })
    }

    fn allows_outside_label(&self, label: &str) -> bool {
        self.allowed_outside_label.as_deref() == Some(label)
    }
}

impl MarkerTarget {
    fn folds_into_oldest_message(&self) -> bool {
        matches!(
            self,
            Self::OldestAnchor | Self::InRange { .. } | Self::Outside { .. }
        )
    }

    fn resolve(
        repo: &Repository,
        label: &str,
        marker_oid: ObjectId,
        oldest: ObjectId,
        index: &RangeSubjectIndex,
        selected: &HashSet<ObjectId>,
    ) -> Result<Self, HistoryError> {
        if let Some(oid) = index.lookup(label) {
            if !selected.contains(&oid) {
                return Ok(Self::Outside {
                    label: label.to_owned(),
                });
            }
            if oid == oldest {
                return Ok(Self::OldestAnchor);
            }
            return Ok(Self::InRange { oid });
        }
        if let Ok(rev) = resolve_revision_for_range_end(repo, label) {
            let oid = peel_to_commit_for_merge_base(repo, rev).map_err(planning_err)?;
            if selected.contains(&oid) {
                if oid == oldest {
                    return Ok(Self::OldestAnchor);
                }
                return Ok(Self::InRange { oid });
            }
            return Ok(Self::Outside {
                label: label.to_owned(),
            });
        }
        let matches: Vec<_> = index
            .by_subject
            .keys()
            .filter(|subject| subject.starts_with(label))
            .collect();
        if matches.len() == 1 {
            let oid = index.by_subject[matches[0]];
            if selected.contains(&oid) {
                if oid == oldest {
                    return Ok(Self::OldestAnchor);
                }
                return Ok(Self::InRange { oid });
            }
            return Ok(Self::Outside {
                label: label.to_owned(),
            });
        }
        Err(HistoryError::MarkerTargetOutsideRange { commit: marker_oid })
    }
}

fn apply_fixup_markers_in_range(
    repo: &Repository,
    commits: &[ObjectId],
    oldest: ObjectId,
    oldest_message: &str,
    index: &RangeSubjectIndex,
    selected: &HashSet<ObjectId>,
    policy: &SquashMessage,
) -> Result<String, HistoryError> {
    let outside_policy =
        OutsideTargetMarkerPolicy::compute(repo, commits, oldest, index, selected)?;
    let mut draft = OldestMessageDraft::from_oldest_commit(oldest_message);
    for &oid in commits {
        let commit = load_commit(repo, oid)?;
        let subject = commit_subject_single_line(&commit.message);
        let Some(marker) = parse_fixup_marker(&subject) else {
            continue;
        };
        let target =
            MarkerTarget::resolve(repo, &marker.target_label, oid, oldest, index, selected)?;
        if let MarkerTarget::Outside { ref label } = target {
            if !outside_policy.allows_outside_label(label) {
                return Err(HistoryError::MarkerTargetOutsideRange { commit: oid });
            }
        }
        match marker.kind {
            FixupMarkerKind::Fixup => {}
            FixupMarkerKind::Amend => {
                let body = message_body_after_subject(&commit.message);
                let body = body.trim_start_matches([' ', '\t']);
                if body.is_empty() {
                    continue;
                }
                draft.apply_amend(oid, body, &target, policy)?;
            }
            FixupMarkerKind::Squash => {
                let body = message_body_after_subject(&commit.message);
                let body = body.trim_start_matches([' ', '\t']);
                draft.apply_squash(oid, body, &target, policy)?;
            }
        }
    }
    Ok(draft.text)
}

fn choose_squash_message(
    repo: &Repository,
    resolved: &ResolvedSquashRange,
    opts: &SquashOptions,
) -> Result<String, HistoryError> {
    match &opts.message {
        SquashMessage::Oldest => normalize_rewrite_message(&resolved.oldest_message),
        SquashMessage::Combine => {
            let combined = build_combined_squash_message(repo, &resolved.commits)?;
            normalize_rewrite_message(&combined)
        }
        SquashMessage::Custom(raw) => normalize_rewrite_message(raw),
    }
}

fn build_combined_squash_message(
    repo: &Repository,
    commits: &[ObjectId],
) -> Result<String, HistoryError> {
    let mut buf = String::new();
    let nr = commits.len();
    buf.push_str(&format!("# This is a combination of {nr} commits.\n"));
    let mut seen_squash = false;
    for (i, &oid) in commits.iter().enumerate() {
        let commit = load_commit(repo, oid)?;
        let subject = commit_subject_single_line(&commit.message);
        let step = if subject.starts_with("squash! ") {
            SquashStep::Squash
        } else if subject.starts_with("fixup! ") || subject.starts_with("amend! ") {
            SquashStep::Fixup
        } else {
            SquashStep::Pick
        };
        let skip = matches!(step, SquashStep::Fixup) && !subject.starts_with("amend! ");
        if skip {
            append_skipped_squash_message(&mut buf, &commit.message, i + 1);
        } else {
            append_nth_squash_message(&mut buf, &commit.message, step, seen_squash, i + 1);
        }
        if step == SquashStep::Squash {
            seen_squash = true;
        }
        if subject.starts_with("amend! ") {
            update_squash_message_for_fixup(&mut buf);
            buf = fixup_replacement_message(&commit.message);
        }
    }
    Ok(strip_squash_comment_lines(&buf))
}

fn strip_squash_comment_lines(template: &str) -> String {
    let mut out = String::new();
    for line in template.lines() {
        if line.starts_with('#') {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}
