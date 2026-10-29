//! Changes the server makes to its target branch: merging a checked-in
//! branch ([`integrate`]) and editing the slices file ([`edit_slices`]).
//!
//! Each reads the target, makes its commit on that tip and moves the target
//! with a compare-and-swap. When the target moved meanwhile, it starts over
//! on the new tip (merging again, re-checking the slices), so concurrent
//! changes take turns instead of failing or being lost; only a target that
//! keeps moving is refused ([`TargetRefusal::Moved`]). Moves are logged in
//! the target's reflog where the repository keeps one.
//!
//! When the served repository has the target checked out in its worktree,
//! the worktree and index move with the branch, keeping local changes as
//! `git reset --keep` does; local changes the move would overwrite refuse it
//! ([`TargetRefusal::WorktreeBlocked`]) and leave the branch where it was.
//!
//! The target is the branch the served repository's `HEAD` names
//! ([`target_branch`]).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ident::Signature;
use crate::merge_base::{
    merge_bases_first_vs_rest, walk_commits_reachable_excluding_ancestors_of, ReachableWalkLimit,
};
use crate::merge_file::MergeFavor;
use crate::merge_trees::{
    merge_trees_three_way, DirectoryRenameMode, TreeMergeConflictPresentation,
    WhitespaceMergeOptions,
};
use crate::objects::{ObjectId, ObjectKind};
use crate::repo::Repository;

use super::refs::{log_head_move, swap_ref, Swap};
use super::tree::{edit, lookup};
use super::{valid_branch_name, CheckoutError, IntegrateStatus, Result, Slice};

/// Where the slices live on the target branch.
pub const SLICES_PATH: &str = ".grit/slices.toml";

/// How many times [`integrate`] and [`edit_slices`] start over on a target
/// that moved under them before refusing with [`TargetRefusal::Moved`].
const ATTEMPTS: usize = 16;

/// The target branch of a served repository: the branch its `HEAD` names,
/// without `refs/heads/`, or `main` when `HEAD` names no branch.
///
/// # Parameters
///
/// - `repo`: the served repository.
///
/// # Returns
///
/// The branch name; the branch itself may not exist yet.
#[must_use]
pub fn target_branch(repo: &Repository) -> String {
    crate::refs::read_symbolic_ref(repo.git_dir(), "HEAD")
        .ok()
        .flatten()
        .and_then(|head| head.strip_prefix("refs/heads/").map(str::to_owned))
        .unwrap_or_else(|| "main".to_owned())
}

/// Whether branch names `a` and `b` are the same apart from case, so a
/// case-insensitive file system (macOS, Windows) keeps them in one file.
///
/// # Parameters
///
/// - `a`, `b`: branch names without `refs/heads/`.
#[must_use]
pub fn same_branch_name(a: &str, b: &str) -> bool {
    a == b || a.to_lowercase() == b.to_lowercase()
}

/// Why the target could not be changed.
///
/// The [`Display`](std::fmt::Display) text is a terse summary; servers word
/// each case for their users themselves.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TargetRefusal {
    /// The name to integrate is not a valid branch name.
    #[error("{name} is not a valid branch name")]
    InvalidName {
        /// The name as given.
        name: String,
    },
    /// The branch to integrate is the target itself (in any case).
    #[error("{target} is the target branch")]
    IsTarget {
        /// The target branch.
        target: String,
    },
    /// The repository has no branch of that name.
    #[error("there is no branch {name}")]
    NoSuchBranch {
        /// The branch.
        name: String,
    },
    /// The target branch has no commits.
    #[error("{target} has no commits")]
    Unborn {
        /// The target branch.
        target: String,
    },
    /// The branch shares no history with the target, so there is nothing to
    /// merge it against.
    #[error("{branch} shares no history with {target}")]
    Unrelated {
        /// The branch.
        branch: String,
        /// The target.
        target: String,
    },
    /// The branch does not merge cleanly into the target.
    #[error("{branch} conflicts with {target} in {}", paths.join(", "))]
    Conflict {
        /// The branch.
        branch: String,
        /// The target.
        target: String,
        /// The conflicting paths.
        paths: Vec<String>,
    },
    /// The slice's name is not one [`valid_slice_name`] allows.
    #[error("{slice} is not a valid slice name")]
    InvalidSliceName {
        /// The name as given.
        slice: String,
    },
    /// A slice was given no paths.
    #[error("slice {slice} has no paths")]
    SliceWithoutPaths {
        /// The slice.
        slice: String,
    },
    /// Paths of a slice are not in the target's tree.
    #[error("{} not in {target}", paths.join(", "))]
    MissingPaths {
        /// The target.
        target: String,
        /// The missing paths.
        paths: Vec<String>,
    },
    /// A new slice's name is taken.
    #[error("slice {slice} already exists")]
    SliceExists {
        /// The slice.
        slice: String,
    },
    /// There is no slice of that name.
    #[error("there is no slice {slice}")]
    NoSuchSlice {
        /// The slice.
        slice: String,
    },
    /// The target is checked out in the served repository's worktree, and
    /// local changes there would be overwritten by moving it.
    #[error("{target} is checked out on the server, where local changes to {} block it", paths.join(", "))]
    WorktreeBlocked {
        /// The target.
        target: String,
        /// The paths with local changes in the way (unmerged paths, when
        /// the index has conflicts).
        paths: Vec<String>,
    },
    /// The target kept moving while the change was being made.
    #[error("{target} kept moving while it was being changed")]
    Moved {
        /// The target.
        target: String,
    },
}

/// Where a change left the target branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetChange {
    /// The target's tip the change was made on.
    pub previous: ObjectId,
    /// The target's tip after it: a new commit, or `previous` when nothing
    /// changed.
    pub head: ObjectId,
    /// Whether the served repository's worktree, which has the target
    /// checked out, moved with it (keeping local changes).
    pub worktree_moved: bool,
}

impl TargetChange {
    fn unchanged(tip: ObjectId) -> Self {
        Self {
            previous: tip,
            head: tip,
            worktree_moved: false,
        }
    }
}

/// What [`integrate`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Integration {
    /// Whether a merge commit moved the target, or it had the branch already.
    pub status: IntegrateStatus,
    /// The branch's commits the merge brought in (0 when already integrated).
    pub commits: usize,
    /// Where the target was and is.
    pub change: TargetChange,
}

/// The tip of `refname`, or `None` when it doesn't exist.
fn tip(repo: &Repository, refname: &str) -> Option<ObjectId> {
    crate::refs::resolve_ref(repo.git_dir(), refname).ok()
}

fn short(target_ref: &str) -> &str {
    target_ref.trim_start_matches("refs/heads/")
}

fn write_commit(
    repo: &Repository,
    tree: ObjectId,
    parents: &[ObjectId],
    who: &Signature,
    message: &str,
) -> crate::error::Result<ObjectId> {
    let mut s = format!("tree {}\n", tree.to_hex());
    for p in parents {
        s.push_str(&format!("parent {}\n", p.to_hex()));
    }
    let line = who.header_line();
    s.push_str(&format!("author {line}\ncommitter {line}\n\n{message}\n"));
    repo.odb().write(ObjectKind::Commit, s.as_bytes())
}

/// What [`move_target`] did.
enum Move {
    /// The target moved; `worktree` says whether the served worktree moved too.
    Done { worktree: bool },
    /// The target was not at `previous` any more; nothing changed.
    Lost,
    /// Local changes in the served worktree block the move; nothing changed.
    Blocked(Vec<String>),
}

/// Move `target_ref` from `previous` to `new`, unless it moved meanwhile,
/// logging the move by `who` with `message`. When `HEAD` of a repository with
/// a worktree names the target, the index and worktree move from
/// `previous`'s tree to `new`'s with keep semantics first, and the move is
/// logged in `HEAD`'s reflog as well.
fn move_target(
    repo: &Repository,
    target_ref: &str,
    previous: ObjectId,
    new: ObjectId,
    who: &Signature,
    message: &str,
) -> Result<Move> {
    let checked_out = repo.work_tree().is_some()
        && crate::refs::read_symbolic_ref(repo.git_dir(), "HEAD")
            .ok()
            .flatten()
            .is_some_and(|head| head == target_ref);
    if !checked_out {
        return Ok(
            match swap_ref(repo, target_ref, Some(previous), new, who, message)? {
                Swap::Done => Move::Done { worktree: false },
                Swap::Moved => Move::Lost,
            },
        );
    }
    if tip(repo, target_ref) != Some(previous) {
        return Ok(Move::Lost);
    }
    let from = repo.odb().commit_tree(&previous)?;
    let to = repo.odb().commit_tree(&new)?;
    if let Some(paths) = keep_worktree(repo, from, to)? {
        return Ok(Move::Blocked(paths));
    }
    match swap_ref(repo, target_ref, Some(previous), new, who, message)? {
        Swap::Done => {
            log_head_move(repo, previous, new, who, message)?;
            Ok(Move::Done { worktree: true })
        }
        Swap::Moved => {
            // Someone moved the branch between the check and the swap; put
            // the worktree back so the next attempt starts from it.
            keep_worktree(repo, to, from)?;
            Ok(Move::Lost)
        }
    }
}

/// Bring the index and worktree from tree `from` to tree `to`, keeping local
/// changes as `git reset --keep` does.
///
/// # Returns
///
/// `None` once moved, or the paths that block the move (local changes it
/// would overwrite, or unmerged paths) with nothing changed.
fn keep_worktree(repo: &Repository, from: ObjectId, to: ObjectId) -> Result<Option<Vec<String>>> {
    let index = repo.load_index()?;
    if index.has_unmerged_entries() {
        let mut paths: Vec<String> = index
            .entries()
            .iter()
            .filter(|e| e.stage() != 0)
            .map(|e| String::from_utf8_lossy(&e.path).into_owned())
            .collect();
        paths.dedup();
        return Ok(Some(paths));
    }
    match crate::porcelain::switch::checkout_tree_transition_allow_local_reset(
        repo,
        Some(&from),
        &to,
        &mut crate::progress::NullProgress,
        &crate::progress::NeverCancel,
    ) {
        Ok(()) => Ok(None),
        Err(crate::error::Error::WouldOverwrite { paths, .. }) => Ok(Some(paths)),
        Err(e) => Err(e.into()),
    }
}

/// Merge branch `name` into the target branch `target_ref` with a merge
/// commit by `who`, when the merge is clean.
///
/// The name must be a valid branch name other than the target's (in any
/// case); `refs/heads/<name>` is read from the repository. A branch the
/// target already has is left alone, and one that shares no history with
/// the target is refused rather than merged against an empty tree. When the
/// target moves while the merge is made, the merge is made again on its new
/// tip. The move is logged as `integrate: <merge message>` where the
/// repository keeps a reflog for the target, and a worktree that has the
/// target checked out moves with it.
///
/// # Parameters
///
/// - `repo`: the served repository.
/// - `target_ref`: the target branch's full ref (`refs/heads/<target>`).
/// - `name`: the branch to merge, without `refs/heads/`.
/// - `who`: author and committer of the merge commit.
///
/// # Returns
///
/// What happened, or `Ok(Err(..))` with why the target was left alone.
///
/// # Errors
///
/// Object, ref, index and worktree failures.
pub fn integrate(
    repo: &Repository,
    target_ref: &str,
    name: &str,
    who: &Signature,
) -> Result<std::result::Result<Integration, TargetRefusal>> {
    let target = short(target_ref);
    if !valid_branch_name(name) {
        return Ok(Err(TargetRefusal::InvalidName {
            name: name.to_owned(),
        }));
    }
    if same_branch_name(name, target) {
        return Ok(Err(TargetRefusal::IsTarget {
            target: target.to_owned(),
        }));
    }
    let branch_ref = format!("refs/heads/{name}");
    for _ in 0..ATTEMPTS {
        let Some(previous) = tip(repo, target_ref) else {
            return Ok(Err(TargetRefusal::Unborn {
                target: target.to_owned(),
            }));
        };
        let Some(branch) = tip(repo, &branch_ref) else {
            return Ok(Err(TargetRefusal::NoSuchBranch {
                name: name.to_owned(),
            }));
        };
        if branch == previous || crate::merge_base::is_ancestor(repo, branch, previous)? {
            return Ok(Ok(Integration {
                status: IntegrateStatus::AlreadyIntegrated,
                commits: 0,
                change: TargetChange::unchanged(previous),
            }));
        }
        let Some(base) = merge_bases_first_vs_rest(repo, previous, &[branch])?
            .first()
            .copied()
        else {
            return Ok(Err(TargetRefusal::Unrelated {
                branch: name.to_owned(),
                target: target.to_owned(),
            }));
        };
        let merged = merge_trees_three_way(
            repo,
            repo.odb().commit_tree(&base)?,
            repo.odb().commit_tree(&previous)?,
            repo.odb().commit_tree(&branch)?,
            MergeFavor::default(),
            WhitespaceMergeOptions::default(),
            None,
            TreeMergeConflictPresentation::default(),
            DirectoryRenameMode::from_repo(repo)?,
        )?;
        let conflicts: Vec<String> = crate::operation::conflicts_from_tree_merge(&merged)
            .into_iter()
            .map(|c| c.path)
            .collect();
        if !conflicts.is_empty() {
            return Ok(Err(TargetRefusal::Conflict {
                branch: name.to_owned(),
                target: target.to_owned(),
                paths: conflicts,
            }));
        }
        let (commits, _) = walk_commits_reachable_excluding_ancestors_of(
            repo,
            &[branch],
            &[previous],
            ReachableWalkLimit::Unlimited,
        )?;
        let message = format!("Merge branch '{name}' into {target}");
        let head = write_commit(repo, merged.result_tree, &[previous, branch], who, &message)?;
        match move_target(
            repo,
            target_ref,
            previous,
            head,
            who,
            &format!("integrate: {message}"),
        )? {
            Move::Done { worktree } => {
                return Ok(Ok(Integration {
                    status: IntegrateStatus::Merged,
                    commits,
                    change: TargetChange {
                        previous,
                        head,
                        worktree_moved: worktree,
                    },
                }))
            }
            Move::Lost => continue,
            Move::Blocked(paths) => {
                return Ok(Err(TargetRefusal::WorktreeBlocked {
                    target: target.to_owned(),
                    paths,
                }))
            }
        }
    }
    Ok(Err(TargetRefusal::Moved {
        target: target.to_owned(),
    }))
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SlicesFile {
    #[serde(default)]
    slices: BTreeMap<String, SliceEntry>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SliceEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(default)]
    paths: Vec<String>,
}

/// Parse the slices file.
///
/// # Errors
///
/// [`CheckoutError::Protocol`] for text that is not a valid slices file.
pub fn parse_slices(text: &str) -> Result<Vec<Slice>> {
    let file: SlicesFile = toml::from_str(text).map_err(|e| CheckoutError::Protocol {
        url: SLICES_PATH.to_owned(),
        detail: e.to_string(),
    })?;
    Ok(file
        .slices
        .into_iter()
        .map(|(name, e)| Slice {
            name,
            description: e.description,
            paths: e.paths,
        })
        .collect())
}

/// Write the slices file's text.
#[must_use]
pub fn render_slices(slices: &[Slice]) -> String {
    let file = SlicesFile {
        slices: slices
            .iter()
            .map(|s| {
                (
                    s.name.clone(),
                    SliceEntry {
                        description: s.description.clone(),
                        paths: s.paths.clone(),
                    },
                )
            })
            .collect(),
    };
    let body = toml::to_string_pretty(&file).unwrap_or_default();
    format!("# Named sets of paths to check out together (`grit slice`).\n\n{body}")
}

/// The slices at `commit` (none when it has no slices file).
///
/// # Errors
///
/// Object read failures and a malformed slices file.
pub fn read_slices(repo: &Repository, commit: ObjectId) -> Result<Vec<Slice>> {
    let odb = repo.odb();
    let tree = odb.commit_tree(&commit)?;
    match lookup(odb, tree, SLICES_PATH.as_bytes())? {
        Some((_, blob)) => parse_slices(&String::from_utf8_lossy(&odb.read(&blob)?.data)),
        None => Ok(Vec::new()),
    }
}

/// Whether `name` can name a slice: letters, digits, `-`, `_` and `.`, not
/// starting with `.` or `-`.
#[must_use]
pub fn valid_slice_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['.', '-'])
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Paths among `paths` that `commit`'s tree doesn't have.
///
/// # Errors
///
/// Object read failures.
pub fn missing_paths(repo: &Repository, commit: ObjectId, paths: &[String]) -> Result<Vec<String>> {
    let odb = repo.odb();
    let tree = odb.commit_tree(&commit)?;
    let mut missing = Vec::new();
    for p in paths {
        if lookup(odb, tree, p.as_bytes())?.is_none() {
            missing.push(p.clone());
        }
    }
    Ok(missing)
}

/// A change to the slices on the target branch, for [`edit_slices`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SliceEdit {
    /// Make slice `name` with these paths and description; refused when the
    /// target has a slice of that name.
    Create(Slice),
    /// Give slice `name` these paths, and this description when it is set
    /// (otherwise the slice keeps its own); refused when there is no such
    /// slice.
    Change(Slice),
    /// Remove the slice of this name; refused when there is none.
    Delete(String),
}

/// What [`edit_slices`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlicesEdited {
    /// The slice as it is now; `None` once deleted.
    pub slice: Option<Slice>,
    /// The commit message of the change (`Add slice <name>`, `Change slice
    /// <name>` or `Remove slice <name>`).
    pub message: String,
    /// Where the target was and is; unchanged when the slices file came out
    /// the same.
    pub change: TargetChange,
}

/// Apply `edit` to `slices` read at `tip` of `target`.
///
/// # Returns
///
/// The commit message and the slice as it now is, or why the edit is refused.
fn apply_slice_edit(
    repo: &Repository,
    target: &str,
    tip: ObjectId,
    slices: &mut Vec<Slice>,
    edit: &SliceEdit,
) -> Result<std::result::Result<(String, Option<Slice>), TargetRefusal>> {
    let refusal = |r| Ok(Err(r));
    if let SliceEdit::Create(slice) | SliceEdit::Change(slice) = edit {
        let missing = missing_paths(repo, tip, &slice.paths)?;
        if !missing.is_empty() {
            return refusal(TargetRefusal::MissingPaths {
                target: target.to_owned(),
                paths: missing,
            });
        }
    }
    let name = match edit {
        SliceEdit::Create(s) | SliceEdit::Change(s) => &s.name,
        SliceEdit::Delete(name) => name,
    };
    let existing = slices.iter().position(|s| &s.name == name);
    Ok(Ok(match (edit, existing) {
        (SliceEdit::Create(_), Some(_)) => {
            return refusal(TargetRefusal::SliceExists {
                slice: name.clone(),
            })
        }
        (SliceEdit::Change(_) | SliceEdit::Delete(_), None) => {
            return refusal(TargetRefusal::NoSuchSlice {
                slice: name.clone(),
            })
        }
        (SliceEdit::Create(slice), None) => {
            slices.push(slice.clone());
            (format!("Add slice {name}"), Some(slice.clone()))
        }
        (SliceEdit::Change(slice), Some(i)) => {
            let changed = Slice {
                name: slice.name.clone(),
                description: slice
                    .description
                    .clone()
                    .or_else(|| slices[i].description.clone()),
                paths: slice.paths.clone(),
            };
            slices[i] = changed.clone();
            (format!("Change slice {name}"), Some(changed))
        }
        (SliceEdit::Delete(_), Some(i)) => {
            slices.remove(i);
            (format!("Remove slice {name}"), None)
        }
    }))
}

/// Change the slices file on the target branch `target_ref` with a commit by
/// `who`, as `edit` says.
///
/// The edit is checked against the slices and the tree at the target's tip:
/// a new slice's name must be free, a changed or removed one must exist, and
/// every path of a slice must be in the tree. When the target moves while
/// the commit is made, the edit is checked and applied again on its new tip.
/// The move is logged as `slice: <message>` where the repository keeps a
/// reflog for the target, and a worktree that has the target checked out
/// moves with it.
///
/// # Parameters
///
/// - `repo`: the served repository.
/// - `target_ref`: the target branch's full ref (`refs/heads/<target>`).
/// - `edit`: the change; slice paths are taken as given (see
///   [`super::normalize_paths`]).
/// - `who`: author and committer of the commit.
///
/// # Returns
///
/// What happened, with the target unchanged when the file came out the
/// same, or `Ok(Err(..))` with why the target was left alone.
///
/// # Errors
///
/// Object, ref, index and worktree failures, and a malformed slices file.
pub fn edit_slices(
    repo: &Repository,
    target_ref: &str,
    edit: &SliceEdit,
    who: &Signature,
) -> Result<std::result::Result<SlicesEdited, TargetRefusal>> {
    let target = short(target_ref);
    match edit {
        SliceEdit::Create(slice) | SliceEdit::Change(slice) => {
            if !valid_slice_name(&slice.name) {
                return Ok(Err(TargetRefusal::InvalidSliceName {
                    slice: slice.name.clone(),
                }));
            }
            if slice.paths.is_empty() {
                return Ok(Err(TargetRefusal::SliceWithoutPaths {
                    slice: slice.name.clone(),
                }));
            }
        }
        SliceEdit::Delete(name) if !valid_slice_name(name) => {
            return Ok(Err(TargetRefusal::NoSuchSlice {
                slice: name.clone(),
            }))
        }
        SliceEdit::Delete(_) => {}
    }
    for _ in 0..ATTEMPTS {
        let Some(previous) = tip(repo, target_ref) else {
            return Ok(Err(TargetRefusal::Unborn {
                target: target.to_owned(),
            }));
        };
        let mut slices = read_slices(repo, previous)?;
        let (message, slice) = match apply_slice_edit(repo, target, previous, &mut slices, edit)? {
            Ok(applied) => applied,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let odb = repo.odb();
        let tree = repo.odb().commit_tree(&previous)?;
        let entry = if slices.is_empty() {
            None
        } else {
            let blob = odb.write(ObjectKind::Blob, render_slices(&slices).as_bytes())?;
            Some((0o100644, blob))
        };
        let new_tree = edit_tree(repo, tree, entry)?;
        if new_tree == tree {
            return Ok(Ok(SlicesEdited {
                slice,
                message,
                change: TargetChange::unchanged(previous),
            }));
        }
        let head = write_commit(repo, new_tree, &[previous], who, &message)?;
        match move_target(
            repo,
            target_ref,
            previous,
            head,
            who,
            &format!("slice: {message}"),
        )? {
            Move::Done { worktree } => {
                return Ok(Ok(SlicesEdited {
                    slice,
                    message,
                    change: TargetChange {
                        previous,
                        head,
                        worktree_moved: worktree,
                    },
                }))
            }
            Move::Lost => continue,
            Move::Blocked(paths) => {
                return Ok(Err(TargetRefusal::WorktreeBlocked {
                    target: target.to_owned(),
                    paths,
                }))
            }
        }
    }
    Ok(Err(TargetRefusal::Moved {
        target: target.to_owned(),
    }))
}

/// `tree` with the slices file set to `entry` (removed when `None`).
fn edit_tree(
    repo: &Repository,
    tree: ObjectId,
    entry: Option<(u32, ObjectId)>,
) -> Result<ObjectId> {
    Ok(edit(
        repo.odb(),
        Some(tree),
        &[(SLICES_PATH.as_bytes().to_vec(), entry)],
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::InitOptions;

    fn who() -> Signature {
        let time = time::OffsetDateTime::from_unix_timestamp(1_700_000_000)
            .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
        Signature::cleaned("Alice", "alice@example.com", time).expect("signature")
    }

    fn slice(name: &str, paths: &[&str]) -> Slice {
        Slice {
            name: name.to_owned(),
            description: None,
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        }
    }

    /// A repository with `main` at a commit holding `a` and `docs/d`, and the
    /// helpers to grow it.
    struct Fixture {
        _dir: tempfile::TempDir,
        repo: Repository,
        base: ObjectId,
    }

    impl Fixture {
        fn new(bare: bool) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let repo = Repository::init(
                dir.path(),
                &InitOptions {
                    bare,
                    initial_branch: Some("main".to_owned()),
                    ..InitOptions::default()
                },
            )
            .expect("init");
            let mut fixture = Self {
                _dir: dir,
                repo,
                base: ObjectId::zero(),
            };
            let tree = fixture.tree(None, &[("a", Some("a\n")), ("docs/d", Some("d\n"))]);
            fixture.base = fixture.commit(tree, &[], "base");
            crate::refs::write_ref(fixture.repo.git_dir(), "refs/heads/main", &fixture.base)
                .expect("ref");
            fixture
        }

        fn tree(&self, from: Option<ObjectId>, files: &[(&str, Option<&str>)]) -> ObjectId {
            let odb = self.repo.odb();
            let edits: Vec<_> = files
                .iter()
                .map(|(path, text)| {
                    let entry = text.map(|t| {
                        (
                            0o100644,
                            odb.write(ObjectKind::Blob, t.as_bytes()).expect("blob"),
                        )
                    });
                    (path.as_bytes().to_vec(), entry)
                })
                .collect();
            edit(odb, from, &edits).expect("tree")
        }

        fn commit(&self, tree: ObjectId, parents: &[ObjectId], message: &str) -> ObjectId {
            write_commit(&self.repo, tree, parents, &who(), message).expect("commit")
        }

        /// A branch `name` on `parent` that sets `files`.
        fn branch(&self, name: &str, parent: ObjectId, files: &[(&str, Option<&str>)]) -> ObjectId {
            let tree = self.tree(
                Some(self.repo.odb().commit_tree(&parent).expect("tree")),
                files,
            );
            let tip = self.commit(tree, &[parent], name);
            crate::refs::write_ref(self.repo.git_dir(), &format!("refs/heads/{name}"), &tip)
                .expect("ref");
            tip
        }

        fn main(&self) -> ObjectId {
            tip(&self.repo, "refs/heads/main").expect("main")
        }

        fn integrate(&self, name: &str) -> std::result::Result<Integration, TargetRefusal> {
            integrate(&self.repo, "refs/heads/main", name, &who()).expect("integrate")
        }

        /// Check `main` out into the empty worktree, files and index.
        fn check_out_main(&self) {
            crate::porcelain::checkout::checkout_between_trees(
                &self.repo,
                None,
                &self.repo.odb().commit_tree(&self.main()).expect("tree"),
            )
            .expect("checkout");
        }
    }

    #[test]
    fn slices_round_trip_through_toml() {
        let slices = vec![
            Slice {
                name: "desktop".to_owned(),
                description: Some("The desktop app".to_owned()),
                paths: vec!["apps/desktop".to_owned(), "packages/ui".to_owned()],
            },
            Slice {
                name: "docs".to_owned(),
                description: None,
                paths: vec!["docs".to_owned()],
            },
        ];
        let text = render_slices(&slices);
        assert!(text.contains("[slices.desktop]"), "{text}");
        assert_eq!(parse_slices(&text).ok(), Some(slices));
        assert_eq!(parse_slices("").ok(), Some(Vec::new()));
        assert!(parse_slices("slices = 3").is_err());
    }

    #[test]
    fn slice_names_are_plain() {
        assert!(valid_slice_name("desktop-ui_2.0"));
        assert!(!valid_slice_name(""));
        assert!(!valid_slice_name(".hidden"));
        assert!(!valid_slice_name("a/b"));
        assert!(!valid_slice_name("-x"));
    }

    #[test]
    fn the_target_is_heads_branch_and_names_compare_without_case() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(
            dir.path(),
            &crate::repo::InitOptions {
                initial_branch: Some("trunk".to_owned()),
                ..crate::repo::InitOptions::default()
            },
        )
        .expect("init");
        assert_eq!(target_branch(&repo), "trunk");
        crate::refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main").expect("head");
        assert_eq!(target_branch(&repo), "main");
        for (a, b, same) in [
            ("main", "main", true),
            ("Main", "main", true),
            ("MAIN", "main", true),
            ("feature", "main", false),
            ("main-2", "main", false),
        ] {
            assert_eq!(same_branch_name(a, b), same, "{a} {b}");
        }
    }

    #[test]
    fn integrating_and_slices_log_the_target_move() {
        let f = Fixture::new(true);
        let work = f.branch("topic", f.base, &[("b", Some("b\n"))]);

        let merged = f.integrate("topic").expect("merged");
        assert_eq!(merged.status, IntegrateStatus::Merged);
        assert_eq!(merged.commits, 1);
        assert_eq!(merged.change.previous, f.base);
        assert!(!merged.change.worktree_moved, "a bare repository has none");
        let head = merged.change.head;
        assert_eq!(f.main(), head);
        let parents = f.repo.odb().read_commit(&head).expect("commit").parents;
        assert_eq!(parents, vec![f.base, work]);
        assert_eq!(
            f.integrate("topic"),
            Ok(Integration {
                status: IntegrateStatus::AlreadyIntegrated,
                commits: 0,
                change: TargetChange::unchanged(head),
            })
        );
        let sliced = edit_slices(
            &f.repo,
            "refs/heads/main",
            &SliceEdit::Create(slice("docs", &["a"])),
            &who(),
        )
        .expect("slices")
        .expect("committed");
        assert_eq!(sliced.message, "Add slice docs");
        let log =
            crate::reflog::read_reflog(f.repo.git_dir(), "refs/heads/main").unwrap_or_default();
        let moves: Vec<(ObjectId, &str)> = log
            .iter()
            .map(|e| (e.new_oid, e.message.as_str()))
            .collect();
        // A bare repository keeps no reflog unless asked; one with a worktree does.
        assert!(moves.is_empty(), "{moves:?}");

        let f = Fixture::new(false);
        f.branch("topic", f.base, &[("b", Some("b\n"))]);
        crate::refs::write_symbolic_ref(f.repo.git_dir(), "HEAD", "refs/heads/elsewhere")
            .expect("head");
        let head = f.integrate("topic").expect("merged").change.head;
        let sliced = edit_slices(
            &f.repo,
            "refs/heads/main",
            &SliceEdit::Create(slice("docs", &["a"])),
            &who(),
        )
        .expect("slices")
        .expect("committed")
        .change
        .head;
        let log = crate::reflog::read_reflog(f.repo.git_dir(), "refs/heads/main").expect("reflog");
        let moves: Vec<(ObjectId, &str)> = log
            .iter()
            .map(|e| (e.new_oid, e.message.as_str()))
            .collect();
        assert!(
            moves.contains(&(head, "integrate: Merge branch 'topic' into main"))
                && moves.contains(&(sliced, "slice: Add slice docs")),
            "{moves:?}"
        );
        assert!(log
            .iter()
            .all(|e| e.identity == "Alice <alice@example.com> 1700000000 +0000"));
    }

    #[test]
    fn integrate_refuses_bad_names_the_target_and_unrelated_histories() {
        let f = Fixture::new(true);
        let refused = |name: &str| f.integrate(name).expect_err("refused");
        for name in [
            "../checkouts/x/target",
            "../../HEAD",
            "a..b",
            "",
            "a b",
            "x.lock",
        ] {
            assert_eq!(
                refused(name),
                TargetRefusal::InvalidName {
                    name: name.to_owned()
                },
                "{name:?}"
            );
        }
        for name in ["main", "Main", "MAIN"] {
            assert_eq!(
                refused(name),
                TargetRefusal::IsTarget {
                    target: "main".to_owned()
                }
            );
        }
        assert_eq!(
            refused("nope"),
            TargetRefusal::NoSuchBranch {
                name: "nope".to_owned()
            }
        );
        // A snapshot-like root (or any orphan) is not merged against an empty tree.
        let orphan_tree = f.tree(None, &[("a", Some("a\n")), ("z", Some("z\n"))]);
        let orphan = f.commit(orphan_tree, &[], "Snapshot of something");
        crate::refs::write_ref(f.repo.git_dir(), "refs/heads/orphan", &orphan).expect("ref");
        let work = f.commit(orphan_tree, &[orphan], "work on the snapshot");
        crate::refs::write_ref(f.repo.git_dir(), "refs/heads/raw", &work).expect("ref");
        for name in ["orphan", "raw"] {
            assert_eq!(
                refused(name),
                TargetRefusal::Unrelated {
                    branch: name.to_owned(),
                    target: "main".to_owned()
                }
            );
        }
        assert_eq!(f.main(), f.base, "nothing moved");

        f.branch("left", f.base, &[("a", Some("left\n"))]);
        f.integrate("left").expect("merged");
        f.branch("right", f.base, &[("a", Some("right\n"))]);
        let before = f.main();
        assert_eq!(
            refused("right"),
            TargetRefusal::Conflict {
                branch: "right".to_owned(),
                target: "main".to_owned(),
                paths: vec!["a".to_owned()],
            }
        );
        assert_eq!(f.main(), before);
    }

    #[test]
    fn a_target_moved_meanwhile_is_merged_onto_again() {
        let f = Fixture::new(true);
        f.branch("one", f.base, &[("one", Some("1\n"))]);
        f.branch("two", f.base, &[("two", Some("2\n"))]);
        let first = f.integrate("one").expect("merged");
        // Two moved on from the base, after one was integrated: its merge
        // lands on the new tip, not on the one it was asked about.
        let second = f.integrate("two").expect("merged");
        assert_eq!(second.change.previous, first.change.head);
        let tree = f.repo.odb().commit_tree(&f.main()).expect("tree");
        for path in ["one", "two", "a"] {
            assert!(lookup(f.repo.odb(), tree, path.as_bytes())
                .expect("lookup")
                .is_some());
        }

        // A compare-and-swap that loses is retried on the new tip.
        let tip_now = f.main();
        let stale = f.branch("three", f.base, &[("three", Some("3\n"))]);
        let message = "integrate: test";
        let moved = f.commit(
            f.repo.odb().commit_tree(&tip_now).expect("tree"),
            &[tip_now],
            "other",
        );
        crate::refs::write_ref(f.repo.git_dir(), "refs/heads/main", &moved).expect("ref");
        assert!(matches!(
            move_target(&f.repo, "refs/heads/main", tip_now, stale, &who(), message).expect("move"),
            Move::Lost
        ));
        assert_eq!(f.main(), moved);
        let third = f.integrate("three").expect("merged");
        assert_eq!(third.change.previous, moved);
    }

    #[test]
    fn slices_are_checked_on_the_tip_they_land_on() {
        let f = Fixture::new(true);
        let edit_main =
            |edit: SliceEdit| edit_slices(&f.repo, "refs/heads/main", &edit, &who()).expect("edit");
        assert_eq!(
            edit_main(SliceEdit::Create(slice("bad/name", &["a"]))),
            Err(TargetRefusal::InvalidSliceName {
                slice: "bad/name".to_owned()
            })
        );
        assert_eq!(
            edit_main(SliceEdit::Create(slice("empty", &[]))),
            Err(TargetRefusal::SliceWithoutPaths {
                slice: "empty".to_owned()
            })
        );
        assert_eq!(
            edit_main(SliceEdit::Create(slice("gone", &["nope", "a"]))),
            Err(TargetRefusal::MissingPaths {
                target: "main".to_owned(),
                paths: vec!["nope".to_owned()]
            })
        );
        assert_eq!(
            edit_main(SliceEdit::Change(slice("docs", &["docs"]))),
            Err(TargetRefusal::NoSuchSlice {
                slice: "docs".to_owned()
            })
        );
        let mut docs = slice("docs", &["docs"]);
        docs.description = Some("The docs".to_owned());
        let made = edit_main(SliceEdit::Create(docs.clone())).expect("made");
        assert_eq!(made.slice.as_ref(), Some(&docs));
        assert_eq!(read_slices(&f.repo, f.main()).expect("slices"), vec![docs]);
        assert_eq!(
            edit_main(SliceEdit::Create(slice("docs", &["a"]))),
            Err(TargetRefusal::SliceExists {
                slice: "docs".to_owned()
            })
        );
        // A change keeps the description it doesn't set.
        let changed = edit_main(SliceEdit::Change(slice("docs", &["a", "docs"]))).expect("changed");
        assert_eq!(changed.message, "Change slice docs");
        assert_eq!(
            changed.slice.and_then(|s| s.description).as_deref(),
            Some("The docs")
        );
        // Setting what is there already changes nothing.
        let same = edit_main(SliceEdit::Change(slice("docs", &["a", "docs"]))).expect("same");
        assert_eq!(same.change, TargetChange::unchanged(f.main()));
        let removed = edit_main(SliceEdit::Delete("docs".to_owned())).expect("removed");
        assert!(removed.slice.is_none());
        assert!(read_slices(&f.repo, f.main()).expect("slices").is_empty());
        assert_eq!(
            edit_main(SliceEdit::Delete("docs".to_owned())),
            Err(TargetRefusal::NoSuchSlice {
                slice: "docs".to_owned()
            })
        );
    }

    #[test]
    fn a_checked_out_target_moves_its_worktree_keeping_local_changes() {
        let f = Fixture::new(false);
        f.check_out_main();
        let wt = f.repo.work_tree().expect("worktree").to_path_buf();
        assert_eq!(
            std::fs::read_to_string(wt.join("a")).ok().as_deref(),
            Some("a\n")
        );
        f.branch(
            "topic",
            f.base,
            &[("a", Some("a2\n")), ("new", Some("n\n"))],
        );
        // An uncommitted change the merge doesn't touch survives it.
        std::fs::write(wt.join("docs/d"), "local\n").expect("write");

        let merged = f.integrate("topic").expect("merged");
        assert!(merged.change.worktree_moved);
        assert_eq!(
            std::fs::read_to_string(wt.join("a")).ok().as_deref(),
            Some("a2\n")
        );
        assert_eq!(
            std::fs::read_to_string(wt.join("new")).ok().as_deref(),
            Some("n\n")
        );
        assert_eq!(
            std::fs::read_to_string(wt.join("docs/d")).ok().as_deref(),
            Some("local\n")
        );
        let index = f.repo.load_index().expect("index");
        let staged: Vec<(String, ObjectId)> = index
            .entries()
            .iter()
            .map(|e| (String::from_utf8_lossy(&e.path).into_owned(), e.oid))
            .collect();
        let head_tree = f.repo.odb().commit_tree(&f.main()).expect("tree");
        for (path, oid) in &staged {
            let in_head = lookup(f.repo.odb(), head_tree, path.as_bytes())
                .expect("lookup")
                .map(|(_, o)| o);
            assert_eq!(
                in_head,
                Some(*oid),
                "{path} is staged as the new main has it"
            );
        }
        let head_log = crate::reflog::read_reflog(f.repo.git_dir(), "HEAD").expect("reflog");
        assert!(head_log
            .iter()
            .any(|e| e.new_oid == f.main() && e.message.starts_with("integrate: ")));

        // A slice change moves it too.
        let sliced = edit_slices(
            &f.repo,
            "refs/heads/main",
            &SliceEdit::Create(slice("docs", &["docs"])),
            &who(),
        )
        .expect("slices")
        .expect("committed");
        assert!(sliced.change.worktree_moved);
        assert!(wt.join(SLICES_PATH).is_file());

        // A local change the merge would overwrite refuses it, and nothing moves.
        f.branch("clash", f.main(), &[("a", Some("a3\n"))]);
        std::fs::write(wt.join("a"), "mine\n").expect("write");
        let before = f.main();
        assert_eq!(
            f.integrate("clash"),
            Err(TargetRefusal::WorktreeBlocked {
                target: "main".to_owned(),
                paths: vec!["a".to_owned()],
            })
        );
        assert_eq!(f.main(), before);
        assert_eq!(
            std::fs::read_to_string(wt.join("a")).ok().as_deref(),
            Some("mine\n")
        );
    }
}
