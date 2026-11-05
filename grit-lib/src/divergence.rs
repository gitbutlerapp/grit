//! What happened when two heads of one shared branch diverged.
//!
//! On a grit hosting server, every member of a shared branch has their own
//! head of it ([`crate::remote::grit_server_share`]). When two heads have
//! each moved on from the last version both had (their **base**), this
//! module works out, commit by commit, what each side did, and names the
//! situation with one of a fixed set of [`Scenario`]s, so a person or an
//! agent can decide what to do.
//!
//! Commits are matched across the two heads by **change id**: the
//! `change-id` header `grit commit` writes and `grit rebase` keeps. A
//! commit without one (made or rebased with plain `git`) is matched by its
//! patch id instead, which can't follow an amend.
//!
//! For each commit of the base, each side either left it alone, moved it,
//! reworded it, amended it, dropped it, squashed it into another, or split
//! it; commits of a side that aren't in the base are new (or pieces of a
//! split). [`classify`] puts the two sides together into a [`Divergence`].
//!
//! ```rust
//! use grit_lib::divergence::Choice;
//! let _ = Choice::KeepMine;
//! ```

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::change_id::change_id;
use crate::diff::{diff_trees, unified_diff};
use crate::error::Result;
use crate::merge_base::{
    is_ancestor, merge_bases_first_vs_rest, walk_commits_reachable_excluding_ancestors_of,
    ReachableWalkLimit,
};
use crate::merge_file::MergeFavor;
use crate::merge_trees::{
    merge_trees_three_way, DirectoryRenameMode, TreeMergeConflictPresentation,
    WhitespaceMergeOptions,
};
use crate::objects::ObjectId;
use crate::patch_ids::{compute_patch_id, PatchIdMode};
use crate::repo::Repository;

/// The most commits of one head looked at.
const MAX_COMMITS: usize = 500;

/// What two diverged heads add up to. The codes are stable: scripts and
/// agents match on them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scenario {
    // Not diverged.
    /// The same commit.
    Same,
    /// Ours builds on theirs: a fast-forward.
    Ahead,
    /// Theirs builds on ours: nothing new here.
    Behind,
    /// Different commits, the same changes in the same order (both rebased
    /// onto the same target, say).
    SameContent,
    /// Both made the same rewrite (the same amend, drop or squash).
    SameRewrite,

    // Resolvable without asking.
    /// Both added commits on top of the base, which don't conflict.
    AddedBoth,
    /// One side rewrote commits (amended, reordered, split, squashed or
    /// dropped); the other only added on top, or did nothing.
    RewroteOne,
    /// Both amended the same commit, without overlapping.
    AmendedBothCompatible,
    /// One reordered commits; the other amended one of them.
    AmendedOneReorderedOther,
    /// Each amended different commits.
    AmendedDifferent,
    /// One rebased onto a newer target; the other added or amended.
    RebasedOne,
    /// Both rebased, onto different target commits.
    RebasedBoth,
    /// One dropped a commit the other didn't touch.
    DroppedOne,
    /// Both dropped the same commit.
    DroppedBoth,
    /// One squashed commits; the other added on top.
    SquashedOneAddedOther,
    /// One split a commit the other didn't touch.
    SplitOne,
    /// One squashed a commit into another; the other amended it.
    SquashedOneAmendedOther,
    /// One split a commit; the other's amendment of it falls in one piece.
    SplitOneAmendedOther,

    // Needs a decision.
    /// Both amended the same commit, and the edits overlap.
    AmendedBothConflicting,
    /// Both added commits, which conflict with each other.
    AddedBothConflict,
    /// One deleted a commit; the other amended or reworded it.
    DroppedOneAmendedOther,
    /// Both reordered, into different orders.
    ReorderedBoth,
    /// Both split the same commit, differently.
    SplitBoth,
    /// One split a commit; the other's amendment of it spans the pieces.
    SplitOneAmendedOtherSpanning,
    /// Both squashed overlapping commits, differently.
    SquashedBothDifferently,
    /// One head drops most of the branch: perhaps an accidental reset.
    ResetOne,
    /// One head has a merge commit.
    MergeCommit,
    /// Commits were matched by patch id because they lack change ids.
    NoChangeIds,
}

impl Scenario {
    /// The stable code, such as `amended_both_conflicting`.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Same => "same",
            Self::Ahead => "ahead",
            Self::Behind => "behind",
            Self::SameContent => "same_content",
            Self::SameRewrite => "same_rewrite",
            Self::AddedBoth => "added_both",
            Self::RewroteOne => "rewrote_one",
            Self::AmendedBothCompatible => "amended_both_compatible",
            Self::AmendedOneReorderedOther => "amended_one_reordered_other",
            Self::AmendedDifferent => "amended_different",
            Self::RebasedOne => "rebased_one",
            Self::RebasedBoth => "rebased_both",
            Self::DroppedOne => "dropped_one",
            Self::DroppedBoth => "dropped_both",
            Self::SquashedOneAddedOther => "squashed_one_added_other",
            Self::SplitOne => "split_one",
            Self::SquashedOneAmendedOther => "squashed_one_amended_other",
            Self::SplitOneAmendedOther => "split_one_amended_other",
            Self::AmendedBothConflicting => "amended_both_conflicting",
            Self::AddedBothConflict => "added_both_conflict",
            Self::DroppedOneAmendedOther => "dropped_one_amended_other",
            Self::ReorderedBoth => "reordered_both",
            Self::SplitBoth => "split_both",
            Self::SplitOneAmendedOtherSpanning => "split_one_amended_other_spanning",
            Self::SquashedBothDifferently => "squashed_both_differently",
            Self::ResetOne => "reset_one",
            Self::MergeCommit => "merge_commit",
            Self::NoChangeIds => "no_change_ids",
        }
    }

    /// Where it ranks when several apply: lower is more pressing.
    fn priority(self) -> usize {
        const ORDER: [Scenario; 28] = [
            Scenario::MergeCommit,
            Scenario::ResetOne,
            Scenario::AmendedBothConflicting,
            Scenario::DroppedOneAmendedOther,
            Scenario::SplitBoth,
            Scenario::SplitOneAmendedOtherSpanning,
            Scenario::SquashedBothDifferently,
            Scenario::ReorderedBoth,
            Scenario::AddedBothConflict,
            Scenario::NoChangeIds,
            Scenario::AmendedBothCompatible,
            Scenario::AmendedOneReorderedOther,
            Scenario::SplitOneAmendedOther,
            Scenario::SquashedOneAmendedOther,
            Scenario::AmendedDifferent,
            Scenario::RebasedBoth,
            Scenario::RebasedOne,
            Scenario::DroppedBoth,
            Scenario::DroppedOne,
            Scenario::SplitOne,
            Scenario::SquashedOneAddedOther,
            Scenario::RewroteOne,
            Scenario::AddedBoth,
            Scenario::SameRewrite,
            Scenario::SameContent,
            Scenario::Ahead,
            Scenario::Behind,
            Scenario::Same,
        ];
        ORDER.iter().position(|s| *s == self).unwrap_or(ORDER.len())
    }

    /// Every scenario, most pressing first.
    #[must_use]
    pub fn all() -> Vec<Self> {
        let mut all = vec![
            Self::Same,
            Self::Ahead,
            Self::Behind,
            Self::SameContent,
            Self::SameRewrite,
            Self::AddedBoth,
            Self::RewroteOne,
            Self::AmendedBothCompatible,
            Self::AmendedOneReorderedOther,
            Self::AmendedDifferent,
            Self::RebasedOne,
            Self::RebasedBoth,
            Self::DroppedOne,
            Self::DroppedBoth,
            Self::SquashedOneAddedOther,
            Self::SplitOne,
            Self::SquashedOneAmendedOther,
            Self::SplitOneAmendedOther,
            Self::AmendedBothConflicting,
            Self::AddedBothConflict,
            Self::DroppedOneAmendedOther,
            Self::ReorderedBoth,
            Self::SplitBoth,
            Self::SplitOneAmendedOtherSpanning,
            Self::SquashedBothDifferently,
            Self::ResetOne,
            Self::MergeCommit,
            Self::NoChangeIds,
        ];
        all.sort_by_key(|s| s.priority());
        all
    }

    /// Whether the heads differ at all.
    #[must_use]
    pub fn diverged(self) -> bool {
        !matches!(
            self,
            Self::Same | Self::Ahead | Self::Behind | Self::SameContent | Self::SameRewrite
        )
    }

    /// Whether a person (or agent) has to choose what to do.
    #[must_use]
    pub fn needs_decision(self) -> bool {
        matches!(
            self,
            Self::AmendedBothConflicting
                | Self::AddedBothConflict
                | Self::DroppedOneAmendedOther
                | Self::ReorderedBoth
                | Self::SplitBoth
                | Self::SplitOneAmendedOtherSpanning
                | Self::SquashedBothDifferently
                | Self::ResetOne
                | Self::MergeCommit
                | Self::NoChangeIds
        )
    }
}

/// What one side did to one commit since the base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Did {
    /// Left it as it was.
    Unchanged,
    /// Kept it, in another place.
    Moved,
    /// Changed only its message.
    Reworded,
    /// Changed what it changes.
    Amended,
    /// Removed it.
    Dropped,
    /// Folded it into another commit (that commit's key).
    SquashedInto(String),
    /// Replaced it with several commits (their keys, this one's among them
    /// when it kept its id).
    Split(Vec<String>),
    /// A new commit, not in the base.
    New,
    /// A piece of the base commit with this key, which was split.
    SplitFrom(String),
    /// Not there, and never was (the other side's new commit).
    Absent,
}

impl Did {
    /// One word for it.
    #[must_use]
    pub fn word(&self) -> &'static str {
        match self {
            Self::Unchanged => "unchanged",
            Self::Moved => "moved",
            Self::Reworded => "reworded",
            Self::Amended => "amended",
            Self::Dropped => "dropped",
            Self::SquashedInto(_) => "squashed",
            Self::Split(_) => "split",
            Self::New => "new",
            Self::SplitFrom(_) => "split_piece",
            Self::Absent => "absent",
        }
    }

    fn changed(&self) -> bool {
        !matches!(self, Self::Unchanged | Self::Absent)
    }

    fn rewrote(&self) -> bool {
        matches!(
            self,
            Self::Moved
                | Self::Reworded
                | Self::Amended
                | Self::Dropped
                | Self::SquashedInto(_)
                | Self::Split(_)
        )
    }
}

/// One commit, as both sides have it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// The change id, or `patch:<patch id>` for a commit without one.
    pub key: String,
    /// Its subject line (the newest version's).
    pub subject: String,
    /// What we did to it.
    pub ours: Did,
    /// What they did to it.
    pub theirs: Did,
    /// Our version's commit, if we have one.
    pub ours_commit: Option<ObjectId>,
    /// Their version's commit, if they have one.
    pub theirs_commit: Option<ObjectId>,
    /// The paths its versions change.
    pub paths: Vec<String>,
}

/// Two diverged heads, explained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    /// What it adds up to: the first scenario that needs a decision, else
    /// the main resolvable one.
    pub scenario: Scenario,
    /// Every scenario that applies, most pressing first.
    pub scenarios: Vec<Scenario>,
    /// The last version both had, when known.
    pub base: Option<ObjectId>,
    /// The commits either side changed (or added), oldest first.
    pub changes: Vec<Change>,
    /// Paths where the two heads' changes conflict, when a merge was tried.
    pub conflicts: Vec<String>,
    /// Whether some commits were matched by patch id, lacking change ids.
    pub matched_without_ids: bool,
}

impl Divergence {
    fn simple(scenario: Scenario, base: Option<ObjectId>) -> Self {
        Self {
            scenario,
            scenarios: vec![scenario],
            base,
            changes: Vec::new(),
            conflicts: Vec::new(),
            matched_without_ids: false,
        }
    }

    /// A sentence saying what happened, naming the other side `them`.
    #[must_use]
    pub fn summary(&self, them: &str) -> String {
        let subject = |pred: &dyn Fn(&Change) -> bool| {
            self.changes
                .iter()
                .find(|c| pred(c))
                .map(|c| format!("\"{}\"", c.subject))
                .unwrap_or_else(|| "a commit".to_owned())
        };
        let both = |a: &Did, b: &Did| a == b;
        match self.scenario {
            Scenario::Same => format!("Your head and {them}'s are the same."),
            Scenario::Ahead => format!("Your head builds on {them}'s."),
            Scenario::Behind => format!("{them}'s head builds on yours."),
            Scenario::SameContent => {
                format!("Your head and {them}'s have the same changes, as different commits.")
            }
            Scenario::SameRewrite => format!("You and {them} made the same rewrite."),
            Scenario::AddedBoth => format!(
                "You and {them} both added commits on top of the same base; they don't conflict."
            ),
            Scenario::AddedBothConflict => format!(
                "You and {them} both added commits, and they conflict in {}.",
                self.conflict_paths()
            ),
            Scenario::RewroteOne if self.scenarios.contains(&Scenario::SameRewrite) => format!(
                "You and {them} made the same rewrite, and one of you added commits on top."
            ),
            Scenario::RewroteOne => format!(
                "One of you rewrote commits ({}); the other only added on top.",
                self.rewrite_words()
            ),
            Scenario::AmendedBothCompatible => format!(
                "You and {them} both amended {}; the edits don't overlap.",
                subject(&|c| c.ours == Did::Amended && c.theirs == Did::Amended)
            ),
            Scenario::AmendedBothConflicting => format!(
                "You and {them} both amended {}, and the edits overlap in {}.",
                subject(&|c| matches!(c.ours, Did::Amended | Did::Reworded)
                    && matches!(c.theirs, Did::Amended | Did::Reworded)),
                self.conflict_paths()
            ),
            Scenario::AmendedOneReorderedOther => format!(
                "One of you reordered commits, and the other amended {}.",
                subject(&|c| c.ours == Did::Amended || c.theirs == Did::Amended)
            ),
            Scenario::AmendedDifferent => {
                format!("You and {them} amended different commits.")
            }
            Scenario::RebasedOne => {
                "One of you rebased the branch onto a newer target; the other only changed commits on it."
                    .to_owned()
            }
            Scenario::RebasedBoth => {
                format!("You and {them} both rebased the branch, onto different target commits.")
            }
            Scenario::DroppedOne => format!(
                "One of you dropped {}; the other didn't touch it.",
                subject(&|c| c.ours == Did::Dropped || c.theirs == Did::Dropped)
            ),
            Scenario::DroppedBoth => format!(
                "You and {them} both dropped {}.",
                subject(&|c| both(&c.ours, &Did::Dropped) && both(&c.theirs, &Did::Dropped))
            ),
            Scenario::DroppedOneAmendedOther => {
                let mine = self
                    .changes
                    .iter()
                    .find(|c| c.ours == Did::Dropped && matches!(c.theirs, Did::Amended | Did::Reworded));
                match mine {
                    Some(c) => format!(
                        "You dropped \"{}\", and {them} amended it.",
                        c.subject
                    ),
                    None => format!(
                        "{them} dropped {}, and you amended it.",
                        subject(&|c| c.theirs == Did::Dropped)
                    ),
                }
            }
            Scenario::ReorderedBoth => {
                format!("You and {them} both reordered the commits, into different orders.")
            }
            Scenario::SplitOne => format!(
                "One of you split {}; the other didn't touch it.",
                subject(&|c| matches!(c.ours, Did::Split(_)) || matches!(c.theirs, Did::Split(_)))
            ),
            Scenario::SplitBoth => format!(
                "You and {them} both split {}, differently.",
                subject(&|c| matches!(c.ours, Did::Split(_)) && matches!(c.theirs, Did::Split(_)))
            ),
            Scenario::SplitOneAmendedOther => format!(
                "One of you split {}, and the other amended it; the amendment fits one piece.",
                subject(&|c| matches!(c.ours, Did::Split(_)) || matches!(c.theirs, Did::Split(_)))
            ),
            Scenario::SplitOneAmendedOtherSpanning => format!(
                "One of you split {}, and the other's amendment of it spans the pieces.",
                subject(&|c| matches!(c.ours, Did::Split(_)) || matches!(c.theirs, Did::Split(_)))
            ),
            Scenario::SquashedOneAddedOther => {
                "One of you squashed commits; the other only added on top.".to_owned()
            }
            Scenario::SquashedOneAmendedOther => format!(
                "One of you squashed {} into another commit, and the other amended it.",
                subject(&|c| matches!(c.ours, Did::SquashedInto(_))
                    || matches!(c.theirs, Did::SquashedInto(_)))
            ),
            Scenario::SquashedBothDifferently => {
                format!("You and {them} both squashed commits, differently.")
            }
            Scenario::ResetOne => {
                "One head drops most of the branch's commits: perhaps an accidental reset."
                    .to_owned()
            }
            Scenario::MergeCommit => {
                "One head has a merge commit, so the two can't be compared commit by commit."
                    .to_owned()
            }
            Scenario::NoChangeIds => format!(
                "Some commits have no change ids (made or rebased with plain git), so your head and {them}'s were matched by patch, which can't follow an amend."
            ),
        }
    }

    fn conflict_paths(&self) -> String {
        match self.conflicts.as_slice() {
            [] => "the same files".to_owned(),
            [one] => one.clone(),
            [first, rest @ ..] => format!("{first} and {} more", rest.len()),
        }
    }

    fn rewrite_words(&self) -> String {
        let mut words = BTreeSet::new();
        for c in &self.changes {
            for did in [&c.ours, &c.theirs] {
                if did.rewrote() {
                    words.insert(did.word());
                }
            }
        }
        words.into_iter().collect::<Vec<_>>().join(", ")
    }
}

/// One commit of a head.
#[derive(Debug, Clone)]
struct Version {
    oid: ObjectId,
    key: String,
    by_patch: bool,
    subject: String,
    message: String,
    patch: Option<ObjectId>,
    tree: ObjectId,
    parent: Option<ObjectId>,
    merge: bool,
    /// When it was committed (seconds since the epoch).
    time: i64,
    /// Lines it adds and removes, as `(path, '+'|'-', line)`.
    lines: Option<HashSet<(String, char, String)>>,
    paths: Vec<String>,
}

/// The commits of `head` that `floor` doesn't have, oldest first along
/// first parents.
fn sequence(repo: &Repository, head: ObjectId, floor: &[ObjectId]) -> Result<Vec<Version>> {
    let (_, reachable) = walk_commits_reachable_excluding_ancestors_of(
        repo,
        &[head],
        floor,
        ReachableWalkLimit::StopAfter(MAX_COMMITS * 4),
    )?;
    let reachable: HashSet<ObjectId> = reachable.into_iter().collect();
    let mut out = Vec::new();
    let mut at = Some(head);
    while let Some(oid) = at {
        if !reachable.contains(&oid) || out.len() >= MAX_COMMITS {
            break;
        }
        let data = repo.odb().read_commit(&oid)?;
        let (key, by_patch, patch) = match change_id(&data) {
            Some(id) => (id.as_str().to_owned(), false, None),
            None => {
                let patch = compute_patch_id(
                    repo.odb().hash_algo(),
                    repo.odb(),
                    &oid,
                    PatchIdMode::Stable,
                )?;
                let key = match patch {
                    Some(p) => format!("patch:{}", p.to_hex()),
                    None => format!("commit:{}", oid.to_hex()),
                };
                (key, true, patch)
            }
        };
        out.push(Version {
            oid,
            key,
            by_patch,
            subject: data.message.lines().next().unwrap_or_default().to_owned(),
            message: data.message.clone(),
            patch,
            tree: data.tree,
            parent: data.parents.first().copied(),
            merge: data.parents.len() > 1,
            time: crate::ident::committer_timestamp_for_until_filter(&data.committer),
            lines: None,
            paths: Vec::new(),
        });
        at = data.parents.first().copied();
    }
    out.reverse();
    Ok(out)
}

/// What a version changes: its patch id, lines and paths, filled in.
fn load_changes(repo: &Repository, v: &mut Version) -> Result<()> {
    if v.lines.is_some() {
        return Ok(());
    }
    if v.patch.is_none() {
        v.patch = compute_patch_id(
            repo.odb().hash_algo(),
            repo.odb(),
            &v.oid,
            PatchIdMode::Stable,
        )?;
    }
    let odb = repo.odb();
    let parent_tree = match v.parent {
        Some(p) => Some(odb.commit_tree(&p)?),
        None => None,
    };
    let mut lines = HashSet::new();
    let mut paths = BTreeSet::new();
    for entry in diff_trees(odb, parent_tree.as_ref(), Some(&v.tree), "")? {
        let path = entry
            .new_path
            .clone()
            .or_else(|| entry.old_path.clone())
            .unwrap_or_default();
        paths.insert(path.clone());
        let text = |oid: &ObjectId| -> String {
            if oid.is_zero() {
                return String::new();
            }
            odb.read(oid)
                .ok()
                .filter(|o| o.data.len() <= 1 << 20 && !o.data.contains(&0))
                .map(|o| String::from_utf8_lossy(&o.data).into_owned())
                .unwrap_or_default()
        };
        let patch = unified_diff(
            &text(&entry.old_oid),
            &text(&entry.new_oid),
            &path,
            &path,
            0,
            false,
            false,
        );
        for line in patch.lines() {
            if line.starts_with("+++") || line.starts_with("---") {
                continue;
            }
            if let Some(rest) = line.strip_prefix('+') {
                lines.insert((path.clone(), '+', rest.to_owned()));
            } else if let Some(rest) = line.strip_prefix('-') {
                lines.insert((path.clone(), '-', rest.to_owned()));
            }
        }
        // A file whose content can't be read as lines still counts.
        lines.insert((path.clone(), '=', entry.new_oid.to_hex()));
    }
    v.lines = Some(lines);
    v.paths = paths.into_iter().collect();
    Ok(())
}

fn lines_of(v: &Version) -> HashSet<(String, char, String)> {
    v.lines
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, sign, _)| *sign != '=')
        .collect()
}

/// Whether `inner`'s line changes all appear in `outer`'s.
fn within(inner: &Version, outer: &Version) -> bool {
    let a = lines_of(inner);
    !a.is_empty() && a.is_subset(&lines_of(outer))
}

/// Without a head both sides had (a fresh clone has no reflog to find one
/// in), the base is where the two heads meet, and commits both sides have
/// past it, by change id, would read as new on both. Take the older version
/// of each such commit as the base's: the newer side changed it.
fn extend_base(base: &mut Vec<Version>, ours: &[Version], theirs: &[Version]) {
    let known: HashSet<String> = base.iter().map(|v| v.key.clone()).collect();
    for o in ours {
        if known.contains(&o.key) || o.by_patch {
            continue;
        }
        if let Some(t) = theirs.iter().find(|t| t.key == o.key) {
            base.push(if t.time < o.time {
                t.clone()
            } else {
                o.clone()
            });
        }
    }
}

/// Whether `big` holds every commit of `small`: the same change, with the
/// same content and message.
fn holds_all(repo: &Repository, big: &mut [Version], small: &mut [Version]) -> Result<bool> {
    if small.is_empty() {
        return Ok(false);
    }
    for v in big.iter_mut().chain(small.iter_mut()) {
        load_changes(repo, v)?;
    }
    Ok(small.iter().all(|s| {
        big.iter()
            .any(|b| b.key == s.key && b.patch == s.patch && b.message == s.message)
    }))
}

/// When one head holds everything the other has, and the other added
/// commits since `base` (a reset, then put back together by the one that
/// kept the branch, say), compare from the other head instead: nothing of
/// it is dropped. The new base.
fn rebase_on_held(
    repo: &Repository,
    base: Option<ObjectId>,
    base_seq: &mut Vec<Version>,
    ours: ObjectId,
    ours_seq: &mut [Version],
    theirs: ObjectId,
    theirs_seq: &mut [Version],
) -> Result<Option<ObjectId>> {
    let added = |seq: &[Version], base_seq: &[Version]| {
        seq.iter().any(|v| !base_seq.iter().any(|b| b.key == v.key))
    };
    if added(ours_seq, base_seq) && holds_all(repo, theirs_seq, ours_seq)? {
        *base_seq = ours_seq.to_vec();
        return Ok(Some(ours));
    }
    if added(theirs_seq, base_seq) && holds_all(repo, ours_seq, theirs_seq)? {
        *base_seq = theirs_seq.to_vec();
        return Ok(Some(theirs));
    }
    Ok(base)
}

/// Whether `seq` leaves out most of `base`'s commits, only dropping (an
/// accidental reset, say): at least two, and more than half.
fn resets(base: &[Version], seq: &[Version]) -> bool {
    let kept: HashSet<&str> = seq.iter().map(|v| v.key.as_str()).collect();
    let gone = base
        .iter()
        .filter(|b| !kept.contains(b.key.as_str()))
        .count();
    gone >= 2 && gone * 2 > base.len()
}

/// What one side did to each commit of the base, and its new commits.
struct Side {
    did: BTreeMap<String, Did>,
    new: Vec<String>,
    rebased: bool,
}

fn compare_side(repo: &Repository, base: &mut [Version], side: &mut [Version]) -> Result<Side> {
    let base_parent = base.first().and_then(|v| v.parent);
    let side_parent = side.first().and_then(|v| v.parent);
    let rebased = !base.is_empty() && !side.is_empty() && base_parent != side_parent;
    let side_index: HashMap<String, usize> = side
        .iter()
        .enumerate()
        .map(|(i, v)| (v.key.clone(), i))
        .collect();
    let base_keys: HashSet<String> = base.iter().map(|v| v.key.clone()).collect();
    for v in base.iter_mut().chain(side.iter_mut()) {
        load_changes(repo, v)?;
    }
    let mut did = BTreeMap::new();
    // The order of the commits both have, for moves: the ones outside the
    // longest run kept in order moved.
    let common: Vec<(usize, usize)> = base
        .iter()
        .enumerate()
        .filter_map(|(bi, v)| side_index.get(&v.key).map(|si| (bi, *si)))
        .collect();
    let kept = longest_increasing(&common.iter().map(|(_, si)| *si).collect::<Vec<_>>());
    let moved: HashSet<usize> = common
        .iter()
        .enumerate()
        .filter(|(i, _)| !kept.contains(i))
        .map(|(_, (bi, _))| *bi)
        .collect();
    let new_keys: Vec<String> = side
        .iter()
        .filter(|v| !base_keys.contains(&v.key))
        .map(|v| v.key.clone())
        .collect();
    let mut pieces_taken: HashSet<String> = HashSet::new();
    for (bi, b) in base.iter().enumerate() {
        let entry = match side_index.get(&b.key) {
            Some(si) => {
                let s = &side[*si];
                let same_patch = b.patch == s.patch;
                let same_message = b.message == s.message;
                // New commits holding pieces of this one, while it shrank:
                // a split that kept the id on one piece.
                let pieces: Vec<String> = side
                    .iter()
                    .filter(|n| !base_keys.contains(&n.key) && within(n, b))
                    .map(|n| n.key.clone())
                    .collect();
                if !same_patch && !pieces.is_empty() && within(s, b) {
                    let mut all = vec![b.key.clone()];
                    all.extend(pieces.iter().cloned());
                    pieces_taken.extend(pieces);
                    Did::Split(all)
                } else if !same_patch {
                    Did::Amended
                } else if !same_message {
                    Did::Reworded
                } else if moved.contains(&bi) {
                    Did::Moved
                } else {
                    Did::Unchanged
                }
            }
            None => {
                let pieces: Vec<String> = side
                    .iter()
                    .filter(|n| !base_keys.contains(&n.key) && within(n, b))
                    .map(|n| n.key.clone())
                    .collect();
                let into = side
                    .iter()
                    .filter(|n| base_keys.contains(&n.key) && n.key != b.key)
                    .find(|n| within(b, n))
                    .map(|n| n.key.clone());
                if pieces.len() >= 2 {
                    pieces_taken.extend(pieces.iter().cloned());
                    Did::Split(pieces)
                } else if let Some(into) = into {
                    Did::SquashedInto(into)
                } else {
                    Did::Dropped
                }
            }
        };
        did.insert(b.key.clone(), entry);
    }
    let mut new = Vec::new();
    for key in new_keys {
        if pieces_taken.contains(&key) {
            let from = did
                .iter()
                .find(|(_, d)| matches!(d, Did::Split(p) if p.contains(&key)))
                .map(|(k, _)| k.clone())
                .unwrap_or_default();
            did.insert(key.clone(), Did::SplitFrom(from));
        } else {
            new.push(key);
        }
    }
    Ok(Side { did, new, rebased })
}

/// Indices (into `seq`) of one longest strictly increasing subsequence.
fn longest_increasing(seq: &[usize]) -> HashSet<usize> {
    let n = seq.len();
    let mut len = vec![1usize; n];
    let mut prev = vec![usize::MAX; n];
    for i in 0..n {
        for j in 0..i {
            if seq[j] < seq[i] && len[j] + 1 > len[i] {
                len[i] = len[j] + 1;
                prev[i] = j;
            }
        }
    }
    let mut out = HashSet::new();
    let Some(mut at) = (0..n).max_by_key(|i| len[*i]) else {
        return out;
    };
    loop {
        out.insert(at);
        if prev[at] == usize::MAX {
            break;
        }
        at = prev[at];
    }
    out
}

/// The paths where merging `ours` and `theirs` over `base` (trees)
/// conflicts.
fn tree_conflicts(
    repo: &Repository,
    base: ObjectId,
    ours: ObjectId,
    theirs: ObjectId,
) -> Result<Vec<String>> {
    let merged = merge_trees_three_way(
        repo,
        base,
        ours,
        theirs,
        MergeFavor::default(),
        WhitespaceMergeOptions::default(),
        None,
        TreeMergeConflictPresentation::default(),
        DirectoryRenameMode::from_repo(repo)?,
    )?;
    let mut paths: Vec<String> = crate::operation::conflicts_from_tree_merge(&merged)
        .into_iter()
        .map(|c| c.path)
        .collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// The heads `refname` has held, newest first, from its reflog (empty when
/// it has none).
#[must_use]
pub fn heads_seen(repo: &Repository, refname: &str) -> Vec<ObjectId> {
    crate::reflog::read_reflog(repo.git_dir(), refname)
        .map(|entries| entries.into_iter().rev().map(|e| e.new_oid).collect())
        .unwrap_or_default()
}

/// The last head both sides had: the newest commit in both `ours_seen` and
/// `theirs_seen` (heads each side has held, such as reflog entries), else
/// the merge base of the two heads.
///
/// # Errors
///
/// Fails when the object database can't be read.
pub fn common_base(
    repo: &Repository,
    ours: ObjectId,
    theirs: ObjectId,
    ours_seen: &[ObjectId],
    theirs_seen: &[ObjectId],
) -> Result<Option<ObjectId>> {
    let theirs_set: HashSet<&ObjectId> = theirs_seen.iter().chain([&theirs]).collect();
    for oid in [ours].iter().chain(ours_seen.iter()) {
        if theirs_set.contains(oid) {
            return Ok(Some(*oid));
        }
    }
    Ok(merge_bases_first_vs_rest(repo, ours, &[theirs])?
        .first()
        .copied())
}

/// Explain how `ours` and `theirs`, two heads of one shared branch, differ.
///
/// `floor` holds the commits the branch builds on (its target, such as
/// `main`): their history is left out. `base` is the last head both had
/// (see [`common_base`]); without one, the merge base of the two heads is
/// used, and commits both amended can't be told apart from commits only
/// one did.
///
/// # Errors
///
/// Fails when the object database can't be read.
pub fn classify(
    repo: &Repository,
    floor: &[ObjectId],
    base: Option<ObjectId>,
    ours: ObjectId,
    theirs: ObjectId,
) -> Result<Divergence> {
    if ours == theirs {
        return Ok(Divergence::simple(Scenario::Same, Some(ours)));
    }
    if is_ancestor(repo, theirs, ours)? {
        return Ok(Divergence::simple(Scenario::Ahead, Some(theirs)));
    }
    if is_ancestor(repo, ours, theirs)? {
        return Ok(Divergence::simple(Scenario::Behind, Some(ours)));
    }
    let base = match base {
        Some(b) => Some(b),
        None => merge_bases_first_vs_rest(repo, ours, &[theirs])?
            .first()
            .copied(),
    };
    let mut base_seq = match base {
        Some(b) => sequence(repo, b, floor)?,
        None => Vec::new(),
    };
    let mut ours_seq = sequence(repo, ours, floor)?;
    let mut theirs_seq = sequence(repo, theirs, floor)?;
    let base = rebase_on_held(
        repo,
        base,
        &mut base_seq,
        ours,
        &mut ours_seq,
        theirs,
        &mut theirs_seq,
    )?;
    extend_base(&mut base_seq, &ours_seq, &theirs_seq);

    let mut scenarios = BTreeSet::new();
    if ours_seq.iter().chain(&theirs_seq).any(|v| v.merge) {
        // A head that drops most of the branch is a reset first, merges or
        // not: the dropped commits can be brought back.
        let reset = |seq: &[Version]| resets(&base_seq, seq) && !seq.iter().any(|v| v.merge);
        let scenario = if reset(&ours_seq) || reset(&theirs_seq) {
            Scenario::ResetOne
        } else {
            Scenario::MergeCommit
        };
        let mut d = Divergence::simple(scenario, base);
        if scenario == Scenario::ResetOne {
            d.scenarios = vec![Scenario::ResetOne, Scenario::MergeCommit];
            let has = |seq: &[Version], key: &str| seq.iter().any(|v| v.key == key);
            for b in &base_seq {
                let did = |seq: &[Version]| {
                    if has(seq, &b.key) {
                        Did::Unchanged
                    } else {
                        Did::Dropped
                    }
                };
                let (o, t) = (did(&ours_seq), did(&theirs_seq));
                if o.changed() || t.changed() {
                    d.changes.push(Change {
                        key: b.key.clone(),
                        subject: b.subject.clone(),
                        ours: o,
                        theirs: t,
                        ours_commit: None,
                        theirs_commit: None,
                        paths: Vec::new(),
                    });
                }
            }
            for (seq, mine) in [(&ours_seq, true), (&theirs_seq, false)] {
                for v in seq.iter().filter(|v| !has(&base_seq, &v.key)) {
                    if d.changes.iter().any(|c| c.key == v.key) {
                        continue;
                    }
                    let other = if mine { &theirs_seq } else { &ours_seq };
                    let there = if has(other, &v.key) {
                        Did::New
                    } else {
                        Did::Absent
                    };
                    let (o, t) = if mine {
                        (Did::New, there)
                    } else {
                        (there, Did::New)
                    };
                    d.changes.push(Change {
                        key: v.key.clone(),
                        subject: v.subject.clone(),
                        ours: o,
                        theirs: t,
                        ours_commit: None,
                        theirs_commit: None,
                        paths: Vec::new(),
                    });
                }
            }
        }
        return Ok(d);
    }
    let same_content = ours_seq.len() == theirs_seq.len()
        && ours_seq
            .iter()
            .zip(&theirs_seq)
            .all(|(a, b)| a.key == b.key && a.message == b.message)
        && {
            let mut same = true;
            for (a, b) in ours_seq.iter_mut().zip(theirs_seq.iter_mut()) {
                load_changes(repo, a)?;
                load_changes(repo, b)?;
                same &= a.patch == b.patch;
            }
            same
        };
    if same_content {
        return Ok(Divergence::simple(Scenario::SameContent, base));
    }

    let ours_side = compare_side(repo, &mut base_seq, &mut ours_seq)?;
    let theirs_side = compare_side(repo, &mut base_seq, &mut theirs_seq)?;
    let find = |seq: &[Version], key: &str| seq.iter().find(|v| v.key == key).cloned();

    // Every key either side has, oldest first: the base's, then ours, then
    // theirs.
    let mut keys: Vec<String> = Vec::new();
    for v in base_seq.iter().chain(&ours_seq).chain(&theirs_seq) {
        if !keys.contains(&v.key) {
            keys.push(v.key.clone());
        }
    }
    let did_of = |side: &Side, seq: &[Version], key: &str| -> Did {
        if let Some(d) = side.did.get(key) {
            return d.clone();
        }
        if side.new.iter().any(|k| k == key) {
            return Did::New;
        }
        if seq.iter().any(|v| v.key == key) {
            return Did::New;
        }
        Did::Absent
    };
    let mut changes = Vec::new();
    let mut conflicts = Vec::new();
    for key in &keys {
        let ours_did = did_of(&ours_side, &ours_seq, key);
        let theirs_did = did_of(&theirs_side, &theirs_seq, key);
        let o = find(&ours_seq, key);
        let t = find(&theirs_seq, key);
        let b = find(&base_seq, key);
        let newest = o.as_ref().or(t.as_ref()).or(b.as_ref());
        let mut paths: BTreeSet<String> = BTreeSet::new();
        for v in [&o, &t, &b].into_iter().flatten() {
            paths.extend(v.paths.iter().cloned());
        }
        match (&ours_did, &theirs_did) {
            (Did::Amended | Did::Reworded, Did::Amended | Did::Reworded) => {
                // Amended and reworded commits are on both sides.
                let (Some(o), Some(t)) = (o.clone(), t.clone()) else {
                    continue;
                };
                if o.patch == t.patch && o.message == t.message {
                    scenarios.insert(Scenario::SameRewrite);
                } else if ours_did == Did::Reworded || theirs_did == Did::Reworded {
                    // A message on one side, content on the other.
                    if o.patch == t.patch {
                        // Both reworded, differently: the content agrees.
                        scenarios.insert(Scenario::AmendedBothConflicting);
                    } else {
                        scenarios.insert(Scenario::AmendedBothCompatible);
                    }
                } else {
                    let base_tree = match &b {
                        Some(b) => b.tree,
                        None => o
                            .parent
                            .map_or(Ok(o.tree), |p| repo.odb().commit_tree(&p))?,
                    };
                    let found = tree_conflicts(repo, base_tree, o.tree, t.tree)?;
                    if found.is_empty() {
                        scenarios.insert(Scenario::AmendedBothCompatible);
                    } else {
                        conflicts.extend(found);
                        scenarios.insert(Scenario::AmendedBothConflicting);
                    }
                }
            }
            (Did::Dropped, Did::Dropped) => {
                scenarios.insert(Scenario::DroppedBoth);
            }
            (Did::Dropped, Did::Amended | Did::Reworded)
            | (Did::Amended | Did::Reworded, Did::Dropped) => {
                scenarios.insert(Scenario::DroppedOneAmendedOther);
            }
            (Did::Split(a), Did::Split(b2)) => {
                scenarios.insert(if a == b2 {
                    Scenario::SameRewrite
                } else {
                    Scenario::SplitBoth
                });
            }
            (Did::Split(pieces), Did::Amended) | (Did::Amended, Did::Split(pieces)) => {
                let (amended, split_seq) = if matches!(ours_did, Did::Amended) {
                    (o.clone(), &theirs_seq)
                } else {
                    (t.clone(), &ours_seq)
                };
                let Some(amended) = amended else {
                    continue;
                };
                let base_lines = b.as_ref().map(lines_of).unwrap_or_default();
                let delta: HashSet<_> = lines_of(&amended)
                    .difference(&base_lines)
                    .cloned()
                    .collect();
                let fits = split_seq
                    .iter()
                    .filter(|v| pieces.contains(&v.key))
                    .any(|piece| {
                        let touched: HashSet<String> = piece.paths.iter().cloned().collect();
                        delta.iter().all(|(p, _, _)| touched.contains(p))
                    });
                scenarios.insert(if fits {
                    Scenario::SplitOneAmendedOther
                } else {
                    Scenario::SplitOneAmendedOtherSpanning
                });
            }
            (Did::SquashedInto(a), Did::SquashedInto(b2)) => {
                scenarios.insert(if a == b2 {
                    Scenario::SameRewrite
                } else {
                    Scenario::SquashedBothDifferently
                });
            }
            (Did::SquashedInto(_), Did::Amended | Did::Reworded)
            | (Did::Amended | Did::Reworded, Did::SquashedInto(_)) => {
                scenarios.insert(Scenario::SquashedOneAmendedOther);
            }
            _ => {}
        }
        // A commit both rewrote the same way is agreed on: not a change.
        let agreed = matches!(ours_did, Did::Amended | Did::Reworded)
            && matches!(theirs_did, Did::Amended | Did::Reworded)
            && matches!((&o, &t), (Some(o), Some(t)) if o.patch == t.patch && o.message == t.message);
        if (ours_did.changed() || theirs_did.changed()) && !agreed {
            changes.push(Change {
                key: key.clone(),
                subject: newest.map(|v| v.subject.clone()).unwrap_or_default(),
                ours: ours_did,
                theirs: theirs_did,
                ours_commit: o.map(|v| v.oid),
                theirs_commit: t.map(|v| v.oid),
                paths: paths.into_iter().collect(),
            });
        }
    }

    let moved = |side: &Side| side.did.values().any(|d| *d == Did::Moved);
    let amended = |side: &Side| side.did.values().any(|d| *d == Did::Amended);
    let rewrote = |side: &Side| side.did.values().any(Did::rewrote);
    let dropped = |side: &Side| side.did.values().filter(|d| **d == Did::Dropped).count();
    if moved(&ours_side) && moved(&theirs_side) {
        let order = |seq: &[Version]| -> Vec<String> {
            seq.iter()
                .filter(|v| base_seq.iter().any(|b| b.key == v.key))
                .map(|v| v.key.clone())
                .collect()
        };
        if order(&ours_seq) != order(&theirs_seq) {
            scenarios.insert(Scenario::ReorderedBoth);
        }
    }
    if (moved(&ours_side) && amended(&theirs_side)) || (moved(&theirs_side) && amended(&ours_side))
    {
        scenarios.insert(Scenario::AmendedOneReorderedOther);
    }
    for side in [&ours_side, &theirs_side] {
        let n = dropped(side);
        let only_drops = side
            .did
            .values()
            .all(|d| matches!(d, Did::Dropped | Did::Unchanged));
        // New commits on top don't make it any less of a reset.
        if n >= 2 && n * 2 > base_seq.len() && only_drops {
            scenarios.insert(Scenario::ResetOne);
        }
    }
    match (ours_side.rebased, theirs_side.rebased) {
        (true, true) => {
            let first_parent = |seq: &[Version]| seq.first().and_then(|v| v.parent);
            if first_parent(&ours_seq) != first_parent(&theirs_seq) {
                scenarios.insert(Scenario::RebasedBoth);
            }
        }
        (true, false) | (false, true) => {
            scenarios.insert(Scenario::RebasedOne);
        }
        _ => {}
    }
    let ours_rewrote = rewrote(&ours_side);
    let theirs_rewrote = rewrote(&theirs_side);
    if !ours_side.new.is_empty() && !theirs_side.new.is_empty() {
        let tip_tree =
            |seq: &[Version], fallback: ObjectId| seq.last().map_or(fallback, |v| v.tree);
        let base_tree = match base {
            Some(b) => repo.odb().commit_tree(&b)?,
            None => ours_seq
                .first()
                .and_then(|v| v.parent)
                .map_or(Ok(ours_seq[0].tree), |p| repo.odb().commit_tree(&p))?,
        };
        let found = tree_conflicts(
            repo,
            base_tree,
            tip_tree(&ours_seq, base_tree),
            tip_tree(&theirs_seq, base_tree),
        )?;
        if found.is_empty() {
            scenarios.insert(Scenario::AddedBoth);
        } else if !scenarios.contains(&Scenario::AmendedBothConflicting) {
            conflicts.extend(found);
            scenarios.insert(Scenario::AddedBothConflict);
        }
    }
    if ours_rewrote != theirs_rewrote {
        let side = if ours_rewrote {
            &ours_side
        } else {
            &theirs_side
        };
        let other = if ours_rewrote {
            &theirs_side
        } else {
            &ours_side
        };
        let only = |pred: &dyn Fn(&Did) -> bool| {
            side.did
                .values()
                .all(|d| pred(d) || matches!(d, Did::Unchanged | Did::SplitFrom(_)))
        };
        if only(&|d| *d == Did::Dropped) {
            scenarios.insert(Scenario::DroppedOne);
        } else if only(&|d| matches!(d, Did::Split(_))) {
            scenarios.insert(Scenario::SplitOne);
        } else if only(&|d| matches!(d, Did::SquashedInto(_))) && !other.new.is_empty() {
            scenarios.insert(Scenario::SquashedOneAddedOther);
        } else {
            scenarios.insert(Scenario::RewroteOne);
        }
    } else if ours_rewrote && theirs_rewrote {
        let amended_keys = |side: &Side| -> BTreeSet<String> {
            side.did
                .iter()
                .filter(|(_, d)| matches!(d, Did::Amended | Did::Reworded))
                .map(|(k, _)| k.clone())
                .collect()
        };
        let (a, b2) = (amended_keys(&ours_side), amended_keys(&theirs_side));
        if !a.is_empty() && !b2.is_empty() && a.is_disjoint(&b2) {
            scenarios.insert(Scenario::AmendedDifferent);
        }
    }
    let matched_without_ids = ours_seq
        .iter()
        .chain(&theirs_seq)
        .chain(&base_seq)
        .any(|v| v.by_patch);
    let unmatched_patch = changes.iter().any(|c| {
        c.key.starts_with("patch:")
            && matches!(
                (&c.ours, &c.theirs),
                (Did::Dropped, _)
                    | (_, Did::Dropped)
                    | (Did::New, Did::Absent)
                    | (Did::Absent, Did::New)
            )
    });
    if matched_without_ids && unmatched_patch && (ours_rewrote || theirs_rewrote) {
        scenarios.insert(Scenario::NoChangeIds);
    }
    // The same rewrite on both sides, and new commits on one: they still
    // differ, and combining puts them together.
    if !scenarios.is_empty()
        && scenarios.iter().all(|s| !s.diverged())
        && (!ours_side.new.is_empty() || !theirs_side.new.is_empty())
    {
        scenarios.insert(Scenario::RewroteOne);
    }
    if scenarios.is_empty() {
        scenarios.insert(if changes.is_empty() {
            Scenario::SameContent
        } else {
            Scenario::AddedBoth
        });
    }
    // Most pressing first.
    let mut ordered: Vec<Scenario> = scenarios.into_iter().collect();
    ordered.sort_by_key(|s| s.priority());
    conflicts.sort();
    conflicts.dedup();
    Ok(Divergence {
        scenario: ordered[0],
        scenarios: ordered,
        base,
        changes,
        conflicts,
        matched_without_ids,
    })
}

/// A way of putting two diverged heads back together. The ids are stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Every change from both sides, combined commit by commit: their
    /// commits kept as they are where possible, yours replayed on top.
    Combine,
    /// Your head as it is.
    KeepMine,
    /// Their head as it is.
    KeepTheirs,
    /// For a commit both amended: their version, then your amendment as a
    /// commit of its own on top.
    KeepBoth,
    /// Where one side dropped a commit the other amended: keep the amended
    /// version.
    KeepAmended,
    /// Where one side dropped a commit the other amended: drop it.
    Drop,
    /// Both reordered: your order.
    MyOrder,
    /// Both reordered: their order.
    TheirOrder,
    /// Both split a commit: your pieces.
    MySplit,
    /// Both split a commit: their pieces.
    TheirSplit,
    /// One head dropped most of the branch: bring the dropped commits back.
    Restore,
    /// Resolve it by hand: rebase onto their head and settle the conflicts.
    Resolve,
}

impl Choice {
    /// The stable id, such as `keep_both`.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Combine => "combine",
            Self::KeepMine => "keep_mine",
            Self::KeepTheirs => "keep_theirs",
            Self::KeepBoth => "keep_both",
            Self::KeepAmended => "keep_amended",
            Self::Drop => "drop",
            Self::MyOrder => "my_order",
            Self::TheirOrder => "their_order",
            Self::MySplit => "my_split",
            Self::TheirSplit => "their_split",
            Self::Restore => "restore",
            Self::Resolve => "resolve",
        }
    }

    /// The choice with id `id`.
    #[must_use]
    pub fn parse(id: &str) -> Option<Self> {
        [
            Self::Combine,
            Self::KeepMine,
            Self::KeepTheirs,
            Self::KeepBoth,
            Self::KeepAmended,
            Self::Drop,
            Self::MyOrder,
            Self::TheirOrder,
            Self::MySplit,
            Self::TheirSplit,
            Self::Restore,
            Self::Resolve,
        ]
        .into_iter()
        .find(|c| c.id() == id)
    }
}

/// One way out of a divergence, as offered for its scenario.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveOption {
    /// What it does.
    pub choice: Choice,
    /// What it does, in a few words.
    pub label: String,
    /// What it leaves out, or `None` when it keeps every change. What it
    /// leaves out is never lost: every head stays in the share's log.
    pub loses: Option<String>,
}

impl Divergence {
    /// The ways out of this divergence, the recommended one first.
    #[must_use]
    pub fn options(&self, them: &str) -> Vec<ResolveOption> {
        let opt = |choice, label: &str, loses: Option<String>| ResolveOption {
            choice,
            label: label.to_owned(),
            loses,
        };
        let theirs_lost = Some(format!("{them}'s changes since you last agreed"));
        let mine_lost = Some("your changes since you last agreed".to_owned());
        let keep_mine = opt(Choice::KeepMine, "keep yours", theirs_lost.clone());
        let keep_theirs = opt(
            Choice::KeepTheirs,
            &format!("keep {them}'s"),
            mine_lost.clone(),
        );
        let resolve = opt(
            Choice::Resolve,
            &format!("rebase onto {them}'s head and resolve the conflicts yourself"),
            None,
        );
        if !self.scenario.diverged() {
            return Vec::new();
        }
        match self.scenario {
            Scenario::AmendedBothConflicting => vec![
                opt(
                    Choice::KeepBoth,
                    &format!("keep both: {them}'s version, then yours as a commit on top"),
                    None,
                ),
                resolve,
                keep_mine,
                keep_theirs,
            ],
            Scenario::AddedBothConflict
            | Scenario::SplitOneAmendedOtherSpanning
            | Scenario::NoChangeIds
            | Scenario::MergeCommit => vec![resolve, keep_mine, keep_theirs],
            Scenario::DroppedOneAmendedOther => vec![
                opt(Choice::KeepAmended, "keep the amended commit", None),
                opt(
                    Choice::Drop,
                    "drop it",
                    Some("the amendment to the dropped commit".to_owned()),
                ),
                keep_mine,
                keep_theirs,
            ],
            Scenario::ReorderedBoth => vec![
                opt(Choice::MyOrder, "your order", None),
                opt(Choice::TheirOrder, &format!("{them}'s order"), None),
                keep_mine,
                keep_theirs,
            ],
            Scenario::SplitBoth => vec![
                opt(Choice::MySplit, "your split", None),
                opt(Choice::TheirSplit, &format!("{them}'s split"), None),
                keep_mine,
                keep_theirs,
            ],
            Scenario::SquashedBothDifferently => vec![keep_mine, keep_theirs],
            Scenario::ResetOne => vec![
                opt(Choice::Restore, "bring the dropped commits back", None),
                keep_mine,
                keep_theirs,
            ],
            _ => vec![
                opt(Choice::Combine, "combine both", None),
                keep_mine,
                keep_theirs,
            ],
        }
    }
}

/// What putting two heads together made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Combined {
    /// The new head, when it worked.
    pub head: Option<ObjectId>,
    /// Paths that conflicted, when it didn't.
    pub conflicts: Vec<String>,
    /// The commit whose changes conflicted, when it didn't.
    pub stopped_at: Option<String>,
}

/// One commit to make: a version's changes, perhaps with another
/// version's amendment folded in.
struct Step {
    version: Version,
    /// Apply what `other` changed relative to `from` too.
    fold: Option<(Version, Version)>,
    /// Use this message instead of the version's.
    message: Option<String>,
    /// Write it as a new change, with an id of its own.
    new_change: bool,
    /// Replay what it changes relative to this version, not its parent.
    against: Option<Version>,
}

/// Put `ours` and `theirs` back together as `choice` says (see
/// [`classify`] for `floor` and `base`).
///
/// [`Choice::Resolve`] makes nothing: it is done by hand.
///
/// # Errors
///
/// Fails when the object database can't be read or written.
pub fn combine(
    repo: &Repository,
    floor: &[ObjectId],
    base: Option<ObjectId>,
    ours: ObjectId,
    theirs: ObjectId,
    choice: Choice,
) -> Result<Combined> {
    let done = |head| Combined {
        head: Some(head),
        conflicts: Vec::new(),
        stopped_at: None,
    };
    match choice {
        Choice::KeepMine => return Ok(done(ours)),
        Choice::KeepTheirs => return Ok(done(theirs)),
        Choice::Resolve => {
            return Ok(Combined {
                head: None,
                conflicts: Vec::new(),
                stopped_at: None,
            })
        }
        _ => {}
    }
    if is_ancestor(repo, theirs, ours)? {
        return Ok(done(ours));
    }
    if is_ancestor(repo, ours, theirs)? {
        return Ok(done(theirs));
    }
    let base = match base {
        Some(b) => Some(b),
        None => merge_bases_first_vs_rest(repo, ours, &[theirs])?
            .first()
            .copied(),
    };
    let mut base_seq = match base {
        Some(b) => sequence(repo, b, floor)?,
        None => Vec::new(),
    };
    let mut ours_seq = sequence(repo, ours, floor)?;
    let mut theirs_seq = sequence(repo, theirs, floor)?;
    rebase_on_held(
        repo,
        base,
        &mut base_seq,
        ours,
        &mut ours_seq,
        theirs,
        &mut theirs_seq,
    )?;
    extend_base(&mut base_seq, &ours_seq, &theirs_seq);
    let ours_side = compare_side(repo, &mut base_seq, &mut ours_seq)?;
    let theirs_side = compare_side(repo, &mut base_seq, &mut theirs_seq)?;

    // Their commits keep their place, and yours go on top, unless only you
    // reordered (or you chose your order): then yours lead.
    let moved = |side: &Side| side.did.values().any(|d| *d == Did::Moved);
    let ours_lead = match choice {
        Choice::MyOrder => true,
        Choice::TheirOrder => false,
        _ => moved(&ours_side) && !moved(&theirs_side),
    };
    let (lead_seq, lead, follow_seq, follow) = if ours_lead {
        (&ours_seq, &ours_side, &theirs_seq, &theirs_side)
    } else {
        (&theirs_seq, &theirs_side, &ours_seq, &ours_side)
    };
    // Whose pieces of a commit both split.
    let lead_split = match choice {
        Choice::MySplit => ours_lead,
        Choice::TheirSplit => !ours_lead,
        _ => true,
    };
    let find = |seq: &[Version], key: &str| seq.iter().find(|v| v.key == key).cloned();
    let base_has = |key: &str| base_seq.iter().any(|v| v.key == key);

    // keep_both and restore leave one head exactly as it is, so its owner
    // can fast-forward to the result, and put the other's changes on top as
    // commits of their own: for keep_both, their head and your amendments;
    // for restore, the head that kept the branch and the new commits of the
    // one that dropped it.
    if matches!(choice, Choice::KeepBoth | Choice::Restore) {
        let drops = |side: &Side| side.did.values().filter(|d| **d == Did::Dropped).count();
        let ours_keeps = choice == Choice::Restore && drops(&ours_side) < drops(&theirs_side);
        let (keep, keep_seq, add_seq, add_side) = if ours_keeps {
            (ours, &ours_seq, &theirs_seq, &theirs_side)
        } else {
            (theirs, &theirs_seq, &ours_seq, &ours_side)
        };
        let mut tip = keep;
        for v in add_seq {
            let kept = keep_seq.iter().find(|k| k.key == v.key);
            let step = if !base_has(&v.key) {
                if kept.is_some() {
                    continue;
                }
                Step {
                    version: v.clone(),
                    fold: None,
                    message: None,
                    new_change: false,
                    against: None,
                }
            } else if matches!(add_side.did.get(&v.key), Some(Did::Amended)) {
                let Some(against) = kept.cloned().or_else(|| find(&base_seq, &v.key)) else {
                    continue;
                };
                Step {
                    version: v.clone(),
                    fold: None,
                    message: None,
                    new_change: true,
                    against: Some(against),
                }
            } else {
                continue;
            };
            match replay(repo, tip, &step)? {
                Ok(Some(next)) => tip = next,
                Ok(None) => {}
                Err(conflicts) => {
                    return Ok(Combined {
                        head: None,
                        conflicts,
                        stopped_at: Some(step.version.subject.clone()),
                    })
                }
            }
        }
        return Ok(done(tip));
    }
    // Whether `side` dropped `key` and that drop stands.
    let dropped = |side: &Side, other: &Side, key: &str| -> bool {
        let gone = matches!(side.did.get(key), Some(Did::Dropped));
        let other_amended = matches!(other.did.get(key), Some(Did::Amended | Did::Reworded));
        match choice {
            Choice::Restore => false,
            Choice::KeepAmended => gone && !other_amended,
            _ => gone,
        }
    };

    let mut steps: Vec<Step> = Vec::new();
    let mut placed: HashSet<String> = HashSet::new();
    for v in lead_seq {
        if placed.contains(&v.key) {
            continue;
        }
        let key = v.key.clone();
        if !base_has(&key) {
            // The lead's own new commit, or a piece of its split.
            if let Some(Did::SplitFrom(from)) = lead.did.get(&key) {
                if !lead_split && matches!(follow.did.get(from), Some(Did::Split(_))) {
                    continue;
                }
            }
            placed.insert(key);
            steps.push(Step {
                version: v.clone(),
                fold: None,
                message: None,
                new_change: false,
                against: None,
            });
            continue;
        }
        if dropped(follow, lead, &key) {
            placed.insert(key);
            continue;
        }
        let lead_did = lead.did.get(&key).cloned().unwrap_or(Did::Unchanged);
        let follow_did = follow.did.get(&key).cloned().unwrap_or(Did::Unchanged);
        let base_v = find(&base_seq, &key);
        let follow_v = find(follow_seq, &key);
        match (&lead_did, &follow_did) {
            // The follower squashed it away: its squashed commit carries it.
            (_, Did::SquashedInto(_)) => {
                placed.insert(key);
                continue;
            }
            // The follower split it, and its split wins: its pieces, in its
            // order.
            (
                Did::Unchanged | Did::Moved | Did::Amended | Did::Reworded | Did::Split(_),
                Did::Split(pieces),
            ) if !lead_split || !matches!(lead_did, Did::Split(_)) => {
                for piece in follow_seq.iter().filter(|p| pieces.contains(&p.key)) {
                    if placed.insert(piece.key.clone()) {
                        steps.push(Step {
                            version: piece.clone(),
                            fold: None,
                            message: None,
                            new_change: false,
                            against: None,
                        });
                    }
                }
                if let (Did::Amended, Some(base_v)) = (&lead_did, &base_v) {
                    if let Some(last) = steps.last_mut() {
                        last.fold = Some((base_v.clone(), v.clone()));
                    }
                }
                placed.insert(key);
                continue;
            }
            _ => {}
        }
        let mut step = Step {
            version: v.clone(),
            fold: None,
            message: None,
            new_change: false,
            against: None,
        };
        match (&lead_did, &follow_did, &follow_v, &base_v) {
            // Only the follower changed it: its version.
            (Did::Unchanged | Did::Moved, Did::Amended | Did::Reworded, Some(fv), _) => {
                step.version = fv.clone();
            }
            // Both changed it.
            (Did::Amended | Did::Reworded, Did::Amended | Did::Reworded, Some(fv), Some(bv)) => {
                if choice == Choice::KeepBoth {
                    steps.push(step);
                    steps.push(Step {
                        version: fv.clone(),
                        fold: None,
                        message: None,
                        new_change: true,
                        against: Some(v.clone()),
                    });
                    placed.insert(key);
                    continue;
                }
                if fv.patch != bv.patch {
                    step.fold = Some((bv.clone(), fv.clone()));
                }
                if fv.message != bv.message && v.message == bv.message {
                    step.message = Some(fv.message.clone());
                }
            }
            // The follower squashed another commit into this one: its
            // version holds both.
            (_, Did::Unchanged, Some(fv), _) if fv.patch != v.patch => {
                if lead_did == Did::Unchanged {
                    step.version = fv.clone();
                } else if let Some(bv) = &base_v {
                    step.fold = Some((bv.clone(), fv.clone()));
                }
            }
            _ => {}
        }
        placed.insert(key);
        steps.push(step);
    }
    // Commits of the base the lead dropped but the follower kept and
    // changed (or that are restored), then the follower's new commits.
    for v in follow_seq {
        if placed.contains(&v.key) {
            continue;
        }
        if base_has(&v.key) {
            let lead_dropped = dropped(lead, follow, &v.key);
            let lead_squashed = matches!(lead.did.get(&v.key), Some(Did::SquashedInto(_)));
            if lead_dropped || lead_squashed {
                continue;
            }
        }
        if let Some(Did::SplitFrom(from)) = follow.did.get(&v.key) {
            if lead_split && matches!(lead.did.get(from), Some(Did::Split(_))) {
                continue;
            }
        }
        placed.insert(v.key.clone());
        steps.push(Step {
            version: v.clone(),
            fold: None,
            message: None,
            new_change: false,
            against: None,
        });
    }
    if choice == Choice::Restore {
        for b in &base_seq {
            if !placed.contains(&b.key) {
                placed.insert(b.key.clone());
                steps.push(Step {
                    version: b.clone(),
                    fold: None,
                    message: None,
                    new_change: false,
                    against: None,
                });
            }
        }
    }

    // Build on the newer of the two heads' roots.
    let root = |seq: &[Version]| seq.first().and_then(|v| v.parent);
    let mut tip = match (root(&ours_seq), root(&theirs_seq)) {
        (Some(a), Some(b)) if a != b => {
            if is_ancestor(repo, a, b)? {
                b
            } else {
                a
            }
        }
        (Some(a), _) | (None, Some(a)) => a,
        (None, None) => return Ok(done(ours)),
    };
    for step in steps {
        match replay(repo, tip, &step)? {
            Ok(Some(next)) => tip = next,
            Ok(None) => {}
            Err(conflicts) => {
                return Ok(Combined {
                    head: None,
                    conflicts,
                    stopped_at: Some(step.version.subject.clone()),
                })
            }
        }
    }
    Ok(done(tip))
}

/// Make `step` on `tip`: the new tip, `None` when it changes nothing
/// there, or the conflicting paths.
fn replay(
    repo: &Repository,
    tip: ObjectId,
    step: &Step,
) -> Result<std::result::Result<Option<ObjectId>, Vec<String>>> {
    let v = &step.version;
    if v.parent == Some(tip) && step.fold.is_none() && step.message.is_none() && !step.new_change {
        return Ok(Ok(Some(v.oid)));
    }
    let odb = repo.odb();
    let tip_tree = odb.commit_tree(&tip)?;
    let parent_tree = match (&step.against, v.parent) {
        (Some(against), _) => against.tree,
        (None, Some(p)) => odb.commit_tree(&p)?,
        (None, None) => tip_tree,
    };
    let mut tree = if v.parent == Some(tip) && step.against.is_none() {
        v.tree
    } else {
        let found = tree_conflicts_with_result(repo, parent_tree, tip_tree, v.tree)?;
        match found {
            Ok(tree) => tree,
            Err(paths) => return Ok(Err(paths)),
        }
    };
    if let Some((from, other)) = &step.fold {
        match tree_conflicts_with_result(repo, from.tree, tree, other.tree)? {
            Ok(t) => tree = t,
            Err(paths) => return Ok(Err(paths)),
        }
    }
    if step.new_change {
        // Only what this version changes beyond the tip.
        if tree == tip_tree {
            return Ok(Ok(None));
        }
    } else if tree == tip_tree && !v.lines.as_ref().is_some_and(HashSet::is_empty) {
        // Its changes are there already.
        return Ok(Ok(None));
    }
    let mut data = odb.read_commit(&v.oid)?;
    data.tree = tree;
    data.parents = vec![tip];
    if let Some(message) = &step.message {
        data.message = message.clone();
    }
    data.extra_headers = crate::objects::remove_header_lines(&data.extra_headers, |key| {
        key == b"gpgsig" || key == b"gpgsig-sha256" || key == b"mergetag"
    });
    if step.new_change {
        let id = crate::change_id::ChangeId::derived_from_commit_id(&v.oid);
        crate::change_id::set_change_id(&mut data, &id);
    }
    let oid = odb.write(
        crate::objects::ObjectKind::Commit,
        &crate::objects::serialize_commit(&data),
    )?;
    Ok(Ok(Some(oid)))
}

/// Merge `ours` and `theirs` over `base` (trees): the result, or the
/// conflicting paths.
fn tree_conflicts_with_result(
    repo: &Repository,
    base: ObjectId,
    ours: ObjectId,
    theirs: ObjectId,
) -> Result<std::result::Result<ObjectId, Vec<String>>> {
    let merged = merge_trees_three_way(
        repo,
        base,
        ours,
        theirs,
        MergeFavor::default(),
        WhitespaceMergeOptions::default(),
        None,
        TreeMergeConflictPresentation::default(),
        DirectoryRenameMode::from_repo(repo)?,
    )?;
    let mut paths: Vec<String> = crate::operation::conflicts_from_tree_merge(&merged)
        .into_iter()
        .map(|c| c.path)
        .collect();
    if paths.is_empty() {
        return Ok(Ok(merged.result_tree));
    }
    paths.sort();
    paths.dedup();
    Ok(Err(paths))
}

/// One commit of a head, by the key it's matched on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keyed {
    /// The change id, or `patch:<patch id>` for a commit without one.
    pub key: String,
    /// The commit.
    pub commit: ObjectId,
    /// Its subject line.
    pub subject: String,
}

/// The commits of `head` that `floor` doesn't have, oldest first along
/// first parents, with the keys they are matched on.
///
/// # Errors
///
/// When a commit can't be read.
pub fn keyed_commits(repo: &Repository, head: ObjectId, floor: &[ObjectId]) -> Result<Vec<Keyed>> {
    Ok(sequence(repo, head, floor)?
        .into_iter()
        .map(|v| Keyed {
            key: v.key,
            commit: v.oid,
            subject: v.subject,
        })
        .collect())
}

/// The commits of `theirs` whose change `ours` doesn't have, leaving out
/// what `floor` (the branch both are merged into) has: what merging `ours`
/// would leave out.
///
/// # Errors
///
/// When a commit can't be read.
pub fn missing_from(
    repo: &Repository,
    floor: &[ObjectId],
    ours: ObjectId,
    theirs: ObjectId,
) -> Result<Vec<Keyed>> {
    if is_ancestor(repo, theirs, ours)? {
        return Ok(Vec::new());
    }
    let have: HashSet<String> = sequence(repo, ours, floor)?
        .into_iter()
        .map(|v| v.key)
        .collect();
    Ok(keyed_commits(repo, theirs, floor)?
        .into_iter()
        .filter(|k| !have.contains(&k.key))
        .collect())
}

/// Replay the commits of `head` that `floor` doesn't have onto `onto`,
/// leaving out those whose key is in `skip`: the new tip (`onto` when
/// nothing is left), or the paths of the first commit that conflicts.
///
/// # Errors
///
/// When a commit or tree can't be read or written.
pub fn replay_onto(
    repo: &Repository,
    head: ObjectId,
    floor: &[ObjectId],
    onto: ObjectId,
    skip: &HashSet<String>,
) -> Result<std::result::Result<ObjectId, Vec<String>>> {
    let mut tip = onto;
    for mut version in sequence(repo, head, floor)? {
        if skip.contains(&version.key) || version.merge {
            continue;
        }
        load_changes(repo, &mut version)?;
        let step = Step {
            version,
            fold: None,
            message: None,
            new_change: false,
            against: None,
        };
        match replay(repo, tip, &step)? {
            Ok(Some(next)) => tip = next,
            Ok(None) => {}
            Err(paths) => return Ok(Err(paths)),
        }
    }
    Ok(Ok(tip))
}
