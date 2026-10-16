//! Squash a connected revision range into the oldest commit and replay descendants.

use std::collections::{HashMap, HashSet};

use crate::merge_base::is_ancestor;
use crate::objects::{parse_commit, CommitData, ObjectId, ObjectKind};
use crate::porcelain::rebase::{
    append_nth_squash_message, append_skipped_squash_message, commit_subject_single_line,
    fixup_replacement_message, message_body_after_subject, skip_fixupish_prefix,
    update_squash_message_for_fixup, RebaseTodoCmd,
};
use crate::repo::Repository;
use crate::revwalk::rev_list::{rev_list, OrderingMode, RevListOptions};
use crate::revwalk::rev_parse::{peel_to_commit_for_merge_base, resolve_revision_for_range_end};
use crate::revwalk::{parse_rev_walk_tokens, RevWalkParsedSpecs};
use crate::state::resolve_head;

use super::error::HistoryError;
use super::plan::plan_rewrite;
use super::reword::normalize_rewrite_message;
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
    let resolved = resolve_squash_range(repo, range)?;
    check_divergent_branch_tips(
        repo,
        resolved.oldest,
        resolved.tip,
        &resolved.commits,
        &opts.rewrite.scope,
    )?;
    let tip_commit = load_commit(repo, resolved.tip)?;
    let message = choose_squash_message(repo, &resolved, opts)?;
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
    let raw = repo.odb().read(&oid).map_err(planning_err)?;
    if raw.kind != ObjectKind::Commit {
        return Err(HistoryError::Planning {
            message: format!("{oid} is not a commit"),
        });
    }
    parse_commit(&raw.data).map_err(planning_err)
}

fn planning_err(err: crate::error::Error) -> HistoryError {
    HistoryError::Planning {
        message: err.to_string(),
    }
}

fn resolve_squash_range(
    repo: &Repository,
    range: &[&str],
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
    let tip = *commits
        .last()
        .ok_or_else(|| HistoryError::Planning {
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
    let mut marker_state = MarkerState::default();
    for &oid in &commits {
        let commit = load_commit(repo, oid)?;
        marker_state.check_commit(repo, oid, &commit, &selected)?;
    }
    let oldest_message = if !marker_state.squash_message.is_empty() {
        marker_state.squash_message.clone()
    } else {
        oldest_commit.message.clone()
    };
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
        if branch_tip == tip || !selected.contains(&branch_tip) {
            continue;
        }
        if is_ancestor(repo, oldest, branch_tip).map_err(planning_err)? {
            return Err(HistoryError::DivergentBranch { name });
        }
    }
    if let Ok(crate::state::HeadState::Detached { oid }) = resolve_head(repo.git_dir()) {
        if oid != tip
            && selected.contains(&oid)
            && is_ancestor(repo, oldest, oid).map_err(planning_err)?
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

#[derive(Default)]
struct MarkerState {
    subjects: HashMap<String, u8>,
    squash_message: String,
}

const FLAG_SEEN: u8 = 1;
const FLAG_AMEND_TARGET: u8 = 2;

impl MarkerState {
    fn check_commit(
        &mut self,
        repo: &Repository,
        oid: ObjectId,
        commit: &CommitData,
        selected: &HashSet<ObjectId>,
    ) -> Result<(), HistoryError> {
        let subject = commit_subject_single_line(&commit.message);
        if self.subjects.is_empty() {
            self.squash_message = commit.message.clone();
            let key = strip_one_fixupish_prefix(&subject);
            self.subjects.insert(key, FLAG_AMEND_TARGET | FLAG_SEEN);
            return Ok(());
        }
        if is_autosquash_subject(&subject) {
            self.check_autosquash(repo, oid, commit, selected)?;
        } else {
            self.subjects.insert(subject, FLAG_SEEN);
        }
        Ok(())
    }

    fn check_autosquash(
        &mut self,
        repo: &Repository,
        oid: ObjectId,
        commit: &CommitData,
        selected: &HashSet<ObjectId>,
    ) -> Result<(), HistoryError> {
        let subject = commit_subject_single_line(&commit.message);
        let mut s = subject.as_str();
        while let Some(rest) = skip_fixupish_prefix(s) {
            s = rest.trim_start();
            if let Some(flags) = self.subjects.get(s).copied().filter(|f| *f != 0) {
                return self.apply_autosquash(oid, commit, flags);
            }
        }
        if let Ok(target) = resolve_revision_for_range_end(repo, s) {
            let target = peel_to_commit_for_merge_base(repo, target).map_err(planning_err)?;
            if target.to_hex().starts_with(s) {
                let mut flags = self.flags_for_commit(repo, target, selected);
                if flags == 0 {
                    let target_commit = load_commit(repo, target)?;
                    let target_subject = commit_subject_single_line(&target_commit.message);
                    flags = self.subjects.get(&target_subject).copied().unwrap_or(0);
                }
                if flags != 0 {
                    return self.apply_autosquash(oid, commit, flags);
                }
            }
        }
        for (key, flags) in self.subjects.clone() {
            if flags == 0 {
                continue;
            }
            let mut t = subject.as_str();
            while let Some(rest) = skip_fixupish_prefix(t) {
                t = rest.trim_start();
                if key.starts_with(t) {
                    return self.apply_autosquash(oid, commit, flags);
                }
            }
        }
        Err(HistoryError::MarkerTargetOutsideRange { commit: oid })
    }

    fn flags_for_commit(
        &self,
        repo: &Repository,
        oid: ObjectId,
        selected: &HashSet<ObjectId>,
    ) -> u8 {
        if !selected.contains(&oid) {
            return 0;
        }
        let Ok(commit) = load_commit(repo, oid) else {
            return 0;
        };
        let subject = commit_subject_single_line(&commit.message);
        self.subjects.get(&subject).copied().unwrap_or(0)
    }

    fn apply_autosquash(
        &mut self,
        oid: ObjectId,
        commit: &CommitData,
        flags: u8,
    ) -> Result<(), HistoryError> {
        let subject = commit_subject_single_line(&commit.message);
        if subject.starts_with("fixup! ") {
            return Ok(());
        }
        let body = message_body_after_subject(&commit.message);
        let body = body.trim_start_matches([' ', '\t']);
        if body.is_empty() && subject.starts_with("amend! ") {
            return Ok(());
        }
        if flags & FLAG_AMEND_TARGET == 0 {
            return Err(HistoryError::MarkerTargetOutsideRange { commit: oid });
        }
        if subject.starts_with("amend! ") {
            if self.squash_message.starts_with("squash! ") {
                return Err(HistoryError::SquashDiscardsMessage { commit: oid });
            }
            if self.squash_message.starts_with("fixup! ") {
                self.squash_message.replace_range(0..5, "amend!");
            }
            if self.squash_message.starts_with("amend! ") {
                if let Some(pos) = self.squash_message.find("\n\n") {
                    self.squash_message.truncate(pos + 1);
                }
            } else {
                self.squash_message.clear();
            }
            self.squash_message.push_str(body);
            if !self.squash_message.ends_with('\n') {
                self.squash_message.push('\n');
            }
            return Ok(());
        }
        if subject.starts_with("squash! ") {
            if self.squash_message.starts_with("fixup! ") {
                if let Some(pos) = self.squash_message.find('\n') {
                    self.squash_message.truncate(pos + 1);
                }
                self.squash_message.replace_range(0..5, "squash!");
            }
            if self.squash_message.starts_with("squash! ") {
                self.squash_message.push('\n');
                self.squash_message.push_str(body);
                if !self.squash_message.ends_with('\n') {
                    self.squash_message.push('\n');
                }
            } else {
                return Err(HistoryError::SquashDiscardsMessage { commit: oid });
            }
            return Ok(());
        }
        Ok(())
    }
}

fn is_autosquash_subject(subject: &str) -> bool {
    subject.starts_with("amend! ")
        || subject.starts_with("fixup! ")
        || subject.starts_with("squash! ")
}

fn strip_one_fixupish_prefix(subject: &str) -> String {
    skip_fixupish_prefix(subject)
        .unwrap_or(subject)
        .trim_start()
        .to_owned()
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
        let cmd = if subject.starts_with("squash! ") {
            RebaseTodoCmd::Squash
        } else if subject.starts_with("fixup! ") || subject.starts_with("amend! ") {
            RebaseTodoCmd::Fixup
        } else {
            RebaseTodoCmd::Pick
        };
        let skip = matches!(cmd, RebaseTodoCmd::Fixup) && !subject.starts_with("amend! ");
        if skip {
            append_skipped_squash_message(&mut buf, &commit.message, i + 1);
        } else {
            append_nth_squash_message(&mut buf, &commit.message, cmd, seen_squash, i + 1);
        }
        if cmd == RebaseTodoCmd::Squash {
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
