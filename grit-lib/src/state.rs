//! Repository state machine — HEAD resolution, branch status, and
//! in-progress operation detection.
//!
//! # Overview
//!
//! Git repositories can be in various states beyond just "clean":
//! merging, rebasing, cherry-picking, reverting, bisecting, etc.
//! This module detects those states by checking for sentinel files
//! (e.g. `MERGE_HEAD`, `rebase-merge/`) in the `.git` directory.
//!
//! It also resolves `HEAD` to determine the current branch and commit,
//! and provides working tree / index diff summaries used by `status`,
//! `commit`, and other porcelain commands.
//!
//! Library guide: [state](https://grit-scm.com/docs/library/repository)
//!
//! ```rust
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let _repo = Repository::init(dir.path(), &InitOptions::default())?;
//! use grit_lib::state::resolve_head; let _ = resolve_head(_repo.git_dir())?;
//!
//! # Ok(())
//! # }
//! ```

use std::fs;
use std::path::Path;

use crate::check_ref_format::{check_refname_format, RefNameOptions};
use crate::error::{Error, Result};
use crate::objects::ObjectId;
use crate::reflog;
use crate::refs::store::RefStoreError;
use crate::repo::Repository;

/// The current state of HEAD.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
#[cfg_attr(feature = "serde-models", serde(rename_all = "snake_case"))]
pub enum HeadState {
    /// HEAD points to a branch via a symbolic ref (e.g. `ref: refs/heads/main`).
    Branch {
        /// The full ref name (e.g. `refs/heads/main`).
        refname: String,
        /// The short branch name (e.g. `main`).
        short_name: String,
        /// The commit OID that the branch points to, or `None` if the
        /// branch is unborn (no commits yet).
        oid: Option<ObjectId>,
    },
    /// HEAD is detached — pointing directly at a commit.
    Detached {
        /// The commit OID.
        oid: ObjectId,
    },
    /// HEAD is in an invalid or unreadable state.
    Invalid,
}

impl HeadState {
    /// Return the commit OID if HEAD resolves to one.
    #[must_use]
    pub fn oid(&self) -> Option<&ObjectId> {
        match self {
            Self::Branch { oid, .. } => oid.as_ref(),
            Self::Detached { oid } => Some(oid),
            Self::Invalid => None,
        }
    }

    /// Return the branch name if HEAD is on a branch.
    #[must_use]
    pub fn branch_name(&self) -> Option<&str> {
        match self {
            Self::Branch { short_name, .. } => Some(short_name),
            _ => None,
        }
    }

    /// Whether HEAD is on an unborn branch (no commits yet).
    #[must_use]
    pub fn is_unborn(&self) -> bool {
        matches!(self, Self::Branch { oid: None, .. })
    }

    /// Whether HEAD is detached.
    #[must_use]
    pub fn is_detached(&self) -> bool {
        matches!(self, Self::Detached { .. })
    }

    /// The root tree of the commit this state names.
    ///
    /// Use this when an unreadable `HEAD` should be treated like an unborn branch (for example
    /// to diff the index against "nothing"); use [`Repository::head_tree`] when it must be an
    /// error.
    ///
    /// # Parameters
    ///
    /// - `odb`: the object database holding the commit.
    ///
    /// # Returns
    ///
    /// `Some(tree)` when [`Self::oid`] names a commit, `None` on an unborn branch or an
    /// [`HeadState::Invalid`] `HEAD`.
    ///
    /// # Errors
    ///
    /// Same as [`Odb::commit_tree`](crate::objects::odb::Odb::commit_tree): the commit is missing, is not
    /// a commit, or has no valid `tree` header.
    pub fn tree(&self, odb: &crate::objects::odb::Odb) -> Result<Option<ObjectId>> {
        self.oid().map(|oid| odb.commit_tree(oid)).transpose()
    }
}

/// An in-progress operation that the repository is in the middle of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
#[cfg_attr(feature = "serde-models", serde(rename_all = "snake_case"))]
pub enum InProgressOperation {
    /// A merge is in progress (`MERGE_HEAD` exists).
    Merge,
    /// An interactive rebase is in progress (`rebase-merge/` exists).
    RebaseInteractive,
    /// A non-interactive rebase is in progress (`rebase-apply/` exists).
    Rebase,
    /// A cherry-pick is in progress (`CHERRY_PICK_HEAD` exists).
    CherryPick,
    /// A revert is in progress (`REVERT_HEAD` exists).
    Revert,
    /// A bisect is in progress (`BISECT_LOG` exists).
    Bisect,
    /// An `am` (apply mailbox) is in progress (`rebase-apply/applying` exists).
    Am,
    /// A history rewrite updated refs but has not finished updating the worktree
    /// (`.git/grit/rewrite-apply/` exists).
    RewriteApply,
}

impl InProgressOperation {
    /// Human-readable description of the operation.
    #[must_use]
    pub fn description(&self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::RebaseInteractive => "interactive rebase",
            Self::Rebase => "rebase",
            Self::CherryPick => "cherry-pick",
            Self::Revert => "revert",
            Self::Bisect => "bisect",
            Self::Am => "am",
            Self::RewriteApply => "rewrite apply",
        }
    }
}

/// A suggested follow-up action while an operation is paused.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
#[cfg_attr(feature = "serde-models", serde(rename_all = "snake_case"))]
pub enum NextStep {
    /// Edit conflicted files in the worktree (inspect with `grit diff`).
    ResolveConflicts,
    /// Stage resolved paths after editing.
    StageResolved,
    /// Record the merge or sequencer result.
    Commit,
    /// Cancel the in-progress operation.
    Abort(InProgressOperation),
    /// Advance a sequencer operation after conflicts are resolved.
    Continue(InProgressOperation),
    /// Skip the current sequencer step.
    Skip(InProgressOperation),
    /// Advance an operation that `git` started and only `git` can resume (for example a
    /// rebase without grit's plan).
    ContinueWithGit(InProgressOperation),
}

impl NextStep {
    /// Render the suggested invocation: the [`Self::grit_command`] when grit implements the
    /// step, otherwise the [`Self::git_command`] for operations only `git` can finish.
    #[must_use]
    pub fn command(&self) -> Option<String> {
        self.grit_command().or_else(|| self.git_command())
    }

    /// Render the `git` invocation for [`Self::ContinueWithGit`]; `None` for every other step.
    #[must_use]
    pub fn git_command(&self) -> Option<String> {
        let Self::ContinueWithGit(kind) = self else {
            return None;
        };
        let verb = match kind {
            InProgressOperation::Merge => "merge",
            InProgressOperation::Rebase | InProgressOperation::RebaseInteractive => "rebase",
            InProgressOperation::CherryPick => "cherry-pick",
            InProgressOperation::Revert => "revert",
            InProgressOperation::Am => "am",
            InProgressOperation::Bisect | InProgressOperation::RewriteApply => return None,
        };
        Some(format!("git {verb} --continue"))
    }

    /// Render a non-interactive `grit` invocation for this step, when one exists.
    #[must_use]
    pub fn grit_command(&self) -> Option<String> {
        match self {
            Self::ResolveConflicts => Some("grit diff".to_owned()),
            Self::StageResolved => Some("grit add .".to_owned()),
            Self::Commit => Some("grit commit \"message\"".to_owned()),
            Self::Continue(InProgressOperation::Merge) => Some("grit merge --continue".to_owned()),
            Self::Abort(InProgressOperation::Merge) => Some("grit merge --abort".to_owned()),
            Self::Abort(InProgressOperation::CherryPick) => Some("grit pick --abort".to_owned()),
            Self::Abort(InProgressOperation::Revert) => Some("grit revert --abort".to_owned()),
            Self::Continue(InProgressOperation::CherryPick) => {
                Some("grit pick --continue".to_owned())
            }
            Self::Continue(InProgressOperation::Revert) => {
                Some("grit revert --continue".to_owned())
            }
            Self::Skip(InProgressOperation::CherryPick) => Some("grit pick --skip".to_owned()),
            Self::Skip(InProgressOperation::Revert) => Some("grit revert --skip".to_owned()),
            Self::Continue(InProgressOperation::Rebase) => {
                Some("grit rebase --continue".to_owned())
            }
            Self::Abort(InProgressOperation::Rebase | InProgressOperation::RebaseInteractive) => {
                Some("grit rebase --abort".to_owned())
            }
            Self::Skip(InProgressOperation::Rebase) => Some("grit rebase --skip".to_owned()),
            Self::Continue(InProgressOperation::RewriteApply) => {
                Some("grit rebase --continue".to_owned())
            }
            Self::Abort(InProgressOperation::RewriteApply) => {
                Some("grit rebase --abort".to_owned())
            }
            Self::ContinueWithGit(_)
            | Self::Abort(InProgressOperation::Am | InProgressOperation::Bisect)
            | Self::Continue(
                InProgressOperation::RebaseInteractive
                | InProgressOperation::Am
                | InProgressOperation::Bisect,
            )
            | Self::Skip(InProgressOperation::Merge)
            | Self::Skip(
                InProgressOperation::RebaseInteractive
                | InProgressOperation::Am
                | InProgressOperation::Bisect
                | InProgressOperation::RewriteApply,
            ) => None,
        }
    }
}

/// In-progress operation plus the steps grit can suggest today.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
pub struct Operation {
    /// Which operation is paused.
    pub kind: InProgressOperation,
    /// Ordered follow-ups (for example resolve → stage → commit).
    pub next_steps: Vec<NextStep>,
    /// Mutually exclusive options (for example abort the merge instead of finishing).
    pub alternatives: Vec<NextStep>,
}

/// Map [`WtStatusState`] to the primary paused operation, if any.
#[must_use]
pub fn primary_in_progress_operation(state: &WtStatusState) -> Option<InProgressOperation> {
    if state.merge_in_progress {
        return Some(InProgressOperation::Merge);
    }
    if state.rebase_interactive_in_progress {
        return Some(InProgressOperation::RebaseInteractive);
    }
    if state.rebase_in_progress {
        return Some(InProgressOperation::Rebase);
    }
    if state.rewrite_apply_in_progress {
        return Some(InProgressOperation::RewriteApply);
    }
    if state.am_in_progress {
        return Some(InProgressOperation::Am);
    }
    if state.cherry_pick_in_progress {
        return Some(InProgressOperation::CherryPick);
    }
    if state.revert_in_progress {
        return Some(InProgressOperation::Revert);
    }
    if state.bisect_in_progress {
        return Some(InProgressOperation::Bisect);
    }
    None
}

/// Next steps when a pick/revert sequencer stopped on an empty commit (`--empty stop`).
#[must_use]
pub fn operation_next_steps_empty(kind: InProgressOperation) -> Vec<NextStep> {
    match kind {
        InProgressOperation::CherryPick | InProgressOperation::Revert => {
            vec![NextStep::Skip(kind)]
        }
        other => operation_next_steps(other, false),
    }
}

/// Ordered next steps for a paused operation given whether unmerged index entries remain.
#[must_use]
pub fn operation_next_steps(
    kind: InProgressOperation,
    has_unmerged_conflicts: bool,
) -> Vec<NextStep> {
    match kind {
        InProgressOperation::Merge => {
            if has_unmerged_conflicts {
                vec![NextStep::ResolveConflicts, NextStep::StageResolved]
            } else {
                vec![NextStep::Continue(InProgressOperation::Merge)]
            }
        }
        InProgressOperation::CherryPick | InProgressOperation::Revert => {
            if has_unmerged_conflicts {
                vec![
                    NextStep::ResolveConflicts,
                    NextStep::StageResolved,
                    NextStep::Continue(kind),
                ]
            } else {
                vec![NextStep::Continue(kind)]
            }
        }
        InProgressOperation::Rebase => {
            if has_unmerged_conflicts {
                vec![
                    NextStep::ResolveConflicts,
                    NextStep::StageResolved,
                    NextStep::Continue(InProgressOperation::Rebase),
                ]
            } else {
                vec![NextStep::Continue(InProgressOperation::Rebase)]
            }
        }
        InProgressOperation::RebaseInteractive
        | InProgressOperation::Am
        | InProgressOperation::Bisect => vec![],
        InProgressOperation::RewriteApply => {
            vec![NextStep::Continue(InProgressOperation::RewriteApply)]
        }
    }
}

/// Steps that cancel or bypass the ordered [`operation_next_steps`] sequence.
#[must_use]
pub fn operation_alternatives(kind: InProgressOperation) -> Vec<NextStep> {
    match kind {
        InProgressOperation::Merge => vec![NextStep::Abort(InProgressOperation::Merge)],
        InProgressOperation::CherryPick => vec![
            NextStep::Abort(InProgressOperation::CherryPick),
            NextStep::Skip(InProgressOperation::CherryPick),
        ],
        InProgressOperation::Revert => vec![
            NextStep::Abort(InProgressOperation::Revert),
            NextStep::Skip(InProgressOperation::Revert),
        ],
        InProgressOperation::Rebase => vec![
            NextStep::Abort(InProgressOperation::Rebase),
            NextStep::Skip(InProgressOperation::Rebase),
        ],
        InProgressOperation::RebaseInteractive
        | InProgressOperation::Am
        | InProgressOperation::Bisect => vec![],
        InProgressOperation::RewriteApply => {
            vec![NextStep::Abort(InProgressOperation::RewriteApply)]
        }
    }
}

/// [`Operation`] guidance for [`StatusModel`](crate::porcelain::status::StatusModel).
///
/// Merge, cherry-pick, revert, and grit rebase sessions expose continue/abort/skip hints when
/// grit implements the matching commands. A rebase that `git` started suggests
/// [`NextStep::ContinueWithGit`] (grit can abort it but not resume git's todo list). Other
/// in-progress states detected via [`WtStatusState`] omit `operation` until grit implements them.
#[must_use]
pub fn status_operation(state: &WtStatusState, has_unmerged_conflicts: bool) -> Option<Operation> {
    let foreign_rebase = !state.merge_in_progress
        && !state.grit_rebase_in_progress
        && (state.rebase_in_progress || state.rebase_interactive_in_progress);
    if foreign_rebase {
        let kind = if state.rebase_interactive_in_progress {
            InProgressOperation::RebaseInteractive
        } else {
            InProgressOperation::Rebase
        };
        let mut next_steps = Vec::new();
        if has_unmerged_conflicts {
            next_steps.extend([NextStep::ResolveConflicts, NextStep::StageResolved]);
        }
        next_steps.push(NextStep::ContinueWithGit(kind));
        return Some(Operation {
            kind,
            next_steps,
            alternatives: vec![NextStep::Abort(kind)],
        });
    }
    let kind = if state.merge_in_progress {
        InProgressOperation::Merge
    } else if state.grit_rebase_in_progress {
        InProgressOperation::Rebase
    } else if let Some(kind) = state.grit_replay_kind {
        kind
    } else if state.cherry_pick_in_progress {
        InProgressOperation::CherryPick
    } else if state.revert_in_progress {
        InProgressOperation::Revert
    } else if state.rewrite_apply_in_progress {
        InProgressOperation::RewriteApply
    } else {
        return None;
    };
    let next_steps = if state.grit_replay_empty_stop && !has_unmerged_conflicts {
        operation_next_steps_empty(kind)
    } else {
        operation_next_steps(kind, has_unmerged_conflicts)
    };
    Some(Operation {
        kind,
        next_steps,
        alternatives: operation_alternatives(kind),
    })
}

/// Full snapshot of a repository's state.
///
/// This is the information that porcelain commands like `status` need to
/// display the repository's current situation.
#[derive(Debug, Clone)]
pub struct RepoState {
    /// Current HEAD state.
    pub head: HeadState,
    /// In-progress operations (there can be multiple, e.g. rebase + merge).
    pub in_progress: Vec<InProgressOperation>,
    /// Whether the repository is bare.
    pub is_bare: bool,
}

/// Resolve HEAD from the given git directory.
///
/// Reads `HEAD`, follows symbolic refs, and resolves the final OID.
///
/// # Parameters
///
/// - `git_dir` — path to the `.git` directory.
///
/// # Errors
///
/// Returns [`Error::Io`] if files cannot be read.
pub fn resolve_head(git_dir: &Path) -> Result<HeadState> {
    let store = crate::refs::store::open_ref_store(git_dir)?;
    if store.format() == crate::refs::store::RefStorageFormat::Reftable {
        return resolve_head_from_store(store.as_ref());
    }
    resolve_head_impl(git_dir, None)
}

/// Resolve `HEAD` through the repository's ref store.
///
/// Files repositories read the on-disk `HEAD` (including symlinks) and resolve branch
/// tips through the cached store. Reftable and injected stores are the only source
/// of `HEAD`; a loose discovery placeholder or stale file cannot override them.
///
/// # Errors
///
/// Propagates ref store read and resolution failures.
pub fn resolve_head_for_repository(repo: &crate::repo::Repository) -> Result<HeadState> {
    if repo.refs().format() == crate::refs::store::RefStorageFormat::Files {
        return resolve_head_impl(repo.git_dir(), Some(repo));
    }
    resolve_head_from_store(repo.refs())
}

fn resolve_head_from_store(store: &dyn crate::refs::store::RefStore) -> Result<HeadState> {
    use crate::refs::store::RawRef;
    match store.read_raw("HEAD")? {
        None => Ok(HeadState::Invalid),
        Some(RawRef::Direct(oid)) => Ok(HeadState::Detached { oid }),
        Some(RawRef::Symbolic(refname)) => {
            symbolic_head_state(refname, |name| match store.resolve(name) {
                Ok(oid) => Ok(Some(oid)),
                Err(Error::RefStore(RefStoreError::NotFound { .. })) => Ok(None),
                Err(err) => Err(err),
            })
        }
    }
}

impl Repository {
    /// The commit `HEAD` points at, whether through a branch or detached.
    ///
    /// `HEAD` is read with [`resolve_head_for_repository`], so injected ref stores are honoured.
    /// Use that function instead when the caller also needs the branch name or must treat an
    /// unborn branch as a normal state.
    ///
    /// # Errors
    ///
    /// - [`Error::UnbornHead`] when `HEAD` names a branch with no commits yet.
    /// - [`Error::InvalidHead`] when `HEAD` is missing or names something that is not a ref.
    /// - Ref store read failures while resolving `HEAD`.
    pub fn head_commit(&self) -> Result<ObjectId> {
        match resolve_head_for_repository(self)? {
            HeadState::Branch { oid: Some(oid), .. } | HeadState::Detached { oid } => Ok(oid),
            HeadState::Branch { oid: None, .. } => Err(Error::UnbornHead),
            HeadState::Invalid => Err(Error::InvalidHead),
        }
    }

    /// The root tree of the commit `HEAD` points at, or `None` on an unborn branch.
    ///
    /// `HEAD` is read like [`Self::head_commit`]. Use [`HeadState::tree`] on an already
    /// resolved `HEAD` when an invalid `HEAD` should also mean "no tree".
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidHead`] when `HEAD` is missing or names something that is not a ref.
    /// - Ref store read failures while resolving `HEAD`.
    /// - The errors of [`Odb::commit_tree`](crate::objects::odb::Odb::commit_tree) for the `HEAD` commit.
    pub fn head_tree(&self) -> Result<Option<ObjectId>> {
        match resolve_head_for_repository(self)? {
            HeadState::Invalid => Err(Error::InvalidHead),
            head => head.tree(self.odb()),
        }
    }
}

/// Resolve the `HEAD` file of the linked worktree administered at `admin_dir`.
///
/// Files and reftable repositories resolve it from `admin_dir` alone, as Git does. A
/// repository with an injected ref store keeps a linked worktree's `HEAD` in `admin_dir`
/// but its branches in that store, so the branch tip is resolved through `repo`.
pub(crate) fn resolve_linked_worktree_head(
    repo: &crate::repo::Repository,
    admin_dir: &Path,
) -> HeadState {
    if repo.refs_on_disk() {
        resolve_head(admin_dir)
    } else {
        resolve_head_impl(admin_dir, Some(repo))
    }
    .unwrap_or(HeadState::Invalid)
}

/// [`HeadState`] for `HEAD` pointing symbolically at `refname`.
///
/// A missing `refs/heads/*` target is an unborn branch; any other unresolvable or malformed
/// target is [`HeadState::Invalid`] (Git's `refs_resolve_ref_unsafe` yields no target there).
fn symbolic_head_state(
    refname: String,
    resolve_named: impl Fn(&str) -> Result<Option<ObjectId>>,
) -> Result<HeadState> {
    if check_refname_format(&refname, &RefNameOptions::default()).is_err() {
        return Ok(HeadState::Invalid);
    }
    let oid = match resolve_named(&refname)? {
        Some(oid) => Some(oid),
        None if refname.starts_with("refs/heads/") => None,
        None => return Ok(HeadState::Invalid),
    };
    let short_name = refname
        .strip_prefix("refs/heads/")
        .unwrap_or(&refname)
        .to_owned();
    Ok(HeadState::Branch {
        refname,
        short_name,
        oid,
    })
}

fn resolve_head_impl(git_dir: &Path, repo: Option<&crate::repo::Repository>) -> Result<HeadState> {
    let resolve_named = |refname: &str| -> Result<Option<crate::objects::ObjectId>> {
        if let Some(r) = repo {
            return r.references().resolve_if_exists(refname);
        }
        match crate::refs::resolve_ref(git_dir, refname) {
            Ok(oid) => Ok(Some(oid)),
            Err(Error::RefStore(RefStoreError::NotFound { .. })) => Ok(None),
            Err(e) => Err(e),
        }
    };
    let read_symbolic = |refname: &str| -> Result<Option<String>> {
        if let Some(r) = repo {
            crate::refs::read_symbolic_ref_for_repository(r, refname)
        } else {
            crate::refs::read_symbolic_ref(git_dir, refname)
        }
    };
    let head_path = git_dir.join("HEAD");
    let content = match fs::read_link(&head_path) {
        Ok(link_target) => {
            let rendered = link_target.to_string_lossy();
            if link_target.is_absolute() || rendered.starts_with("refs/") {
                format!("ref: {rendered}")
            } else {
                fs::read_to_string(&head_path).map_err(Error::Io)?
            }
        }
        Err(_) => match fs::read_to_string(&head_path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HeadState::Invalid),
            Err(e) => return Err(Error::Io(e)),
        },
    };

    let trimmed = content.trim();

    if let Some(refname) = trimmed.strip_prefix("ref: ") {
        let refname = if refname == "refs/heads/.invalid" {
            if let Ok(Some(target)) = read_symbolic("HEAD") {
                target
            } else if let Ok(Some(oid)) = resolve_named("HEAD") {
                // Detached HEAD: Git keeps the `.invalid` stub on disk while the OID lives in reftable.
                return Ok(HeadState::Detached { oid });
            } else {
                match crate::refs::read_ref_file(&git_dir.join("refs").join("heads")) {
                    Ok(crate::refs::Ref::Symbolic(target)) => target,
                    _ => refname.to_owned(),
                }
            }
        } else {
            refname.to_owned()
        };
        symbolic_head_state(refname, resolve_named)
    } else {
        // Detached HEAD — should be a hex OID
        match ObjectId::from_hex(trimmed) {
            Ok(oid) => Ok(HeadState::Detached { oid }),
            Err(_) => Ok(HeadState::Invalid),
        }
    }
}

/// True when grit owns the in-progress rebase (`.git/rebase-merge/grit-plan`).
fn grit_rebase_plan_active(rebase_merge: &Path) -> bool {
    rebase_merge.join("grit-plan").is_file() || rebase_merge.join("grit-exec").is_file()
}

/// True when grit stopped a resumable rebase on replay conflicts (`stopped-sha` is set).
pub(crate) fn grit_rebase_conflict_stop_active(git_dir: &Path) -> bool {
    let merge = git_dir.join("rebase-merge");
    merge.is_dir() && merge.join("grit-plan").is_file() && merge.join("stopped-sha").is_file()
}

/// Source commit being replayed when [`grit_rebase_conflict_stop_active`] is true.
pub(crate) fn read_grit_rebase_stopped_source(
    git_dir: &Path,
) -> crate::error::Result<Option<ObjectId>> {
    if !grit_rebase_conflict_stop_active(git_dir) {
        return Ok(None);
    }
    let raw = fs::read_to_string(git_dir.join("rebase-merge/stopped-sha"))?;
    let oid =
        ObjectId::from_hex(raw.trim()).map_err(|e| crate::error::Error::Message(e.to_string()))?;
    Ok(Some(oid))
}

/// Detect in-progress operations by checking for sentinel files.
///
/// # Parameters
///
/// - `git_dir` — path to the `.git` directory.
///
/// # Returns
///
/// A list of detected in-progress operations.
pub fn detect_in_progress(git_dir: &Path) -> Vec<InProgressOperation> {
    let mut ops = Vec::new();

    if git_dir.join("MERGE_HEAD").exists() {
        ops.push(InProgressOperation::Merge);
    }

    // Interactive rebase: rebase-merge/ directory
    let rebase_merge = git_dir.join("rebase-merge");
    if rebase_merge.is_dir() {
        if rebase_merge.join("interactive").exists() && !grit_rebase_plan_active(&rebase_merge) {
            ops.push(InProgressOperation::RebaseInteractive);
        } else {
            ops.push(InProgressOperation::Rebase);
        }
    }

    // Non-interactive rebase or am: rebase-apply/ directory
    let rebase_apply = git_dir.join("rebase-apply");
    if rebase_apply.is_dir() {
        if rebase_apply.join("applying").exists() {
            ops.push(InProgressOperation::Am);
        } else {
            ops.push(InProgressOperation::Rebase);
        }
    }

    if git_dir.join("CHERRY_PICK_HEAD").exists() {
        ops.push(InProgressOperation::CherryPick);
    }

    if git_dir.join("REVERT_HEAD").exists() {
        ops.push(InProgressOperation::Revert);
    }

    let bisect_log = crate::refs::common_dir(git_dir)
        .unwrap_or_else(|| git_dir.to_path_buf())
        .join("BISECT_LOG");
    if bisect_log.exists() {
        ops.push(InProgressOperation::Bisect);
    }

    if crate::history::rewrite_apply_journal_active(git_dir) {
        ops.push(InProgressOperation::RewriteApply);
    }

    ops
}

/// Remove in-progress operation sentinels without moving `HEAD`, refs, or the worktree.
///
/// Returns the operations that were active before clearing (same order as [`detect_in_progress`]).
///
/// # Errors
///
/// Returns [`Error::Io`] when sentinel files cannot be removed.
pub fn clear_in_progress(repo: &Repository) -> Result<Vec<InProgressOperation>> {
    crate::porcelain::workspace_snapshot::clear_in_progress(repo)
}

/// The paused operation that must finish before `HEAD` may move to another branch or commit.
///
/// Merge, rebase (grit's or git's), am, cherry-pick and revert all keep state that refers to
/// the current `HEAD`; moving it would make their continue step commit onto the wrong branch.
/// A bisect does not block a switch.
///
/// # Parameters
///
/// - `git_dir` — the (per-worktree) git directory.
///
/// # Errors
///
/// Returns [`Error::Io`] when an operation sentinel cannot be read.
pub fn operation_blocking_switch(git_dir: &Path) -> Result<Option<InProgressOperation>> {
    let state = wt_status_get_state(git_dir, &HeadState::Invalid, false)?;
    Ok(primary_in_progress_operation(&state).filter(|op| *op != InProgressOperation::Bisect))
}

/// Snapshot of repository state used by `git status` long-format output (`wt-status.c`).
///
/// This mirrors Git's `struct wt_status_state` closely enough for advice lines and
/// branch headers (merge, rebase, cherry-pick, revert, bisect, am, detached HEAD).
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
pub struct WtStatusState {
    /// `MERGE_HEAD` exists (merge or merge+rebase).
    pub merge_in_progress: bool,
    /// `.git/rebase-merge/` exists and `interactive` is present.
    pub rebase_interactive_in_progress: bool,
    /// Rebase without interactive marker (`rebase-merge` non-interactive or `rebase-apply`).
    pub rebase_in_progress: bool,
    /// Display string for the branch being rebased (from `head-name`, may be absent).
    pub rebase_branch: Option<String>,
    /// Display string for the rebase onto commit (from `onto`, abbreviated OID or name).
    pub rebase_onto: Option<String>,
    /// `rebase-apply/applying` exists.
    pub am_in_progress: bool,
    /// Empty patch in `am` session (`rebase-apply/patch` has size 0).
    pub am_empty_patch: bool,
    /// `CHERRY_PICK_HEAD` or sequencer pick without head.
    pub cherry_pick_in_progress: bool,
    /// Commit being cherry-picked; `None` when only the sequencer records progress.
    pub cherry_pick_head_oid: Option<ObjectId>,
    /// `REVERT_HEAD` or sequencer revert without head.
    pub revert_in_progress: bool,
    /// Commit being reverted when [`Self::revert_in_progress`] is true.
    pub revert_head_oid: Option<ObjectId>,
    /// `BISECT_LOG` exists (checked under common dir).
    pub bisect_in_progress: bool,
    /// Label read from `BISECT_LOG` for status output when bisecting.
    pub bisecting_from: Option<String>,
    /// Detached HEAD: human label (`wt_status_get_detached_from`).
    pub detached_from: Option<String>,
    /// True when `HEAD` OID equals the detached tip OID.
    pub detached_at: bool,
    /// Grit wrote `.git/sequencer/grit-opts` for this replay (show pick/revert operation hints).
    pub grit_replay_sequencer: bool,
    /// When [`Self::grit_replay_sequencer`] is true, the persisted pick/revert kind from `grit-opts`.
    pub grit_replay_kind: Option<InProgressOperation>,
    /// Grit `--empty stop` pause without index conflicts (suggest `--skip`, not `--continue`).
    pub grit_replay_empty_stop: bool,
    /// Grit wrote `.git/rebase-merge/grit-plan` for a resumable rebase session.
    pub grit_rebase_in_progress: bool,
    /// Grit recorded an incomplete rewrite apply under `.git/grit/rewrite-apply/`.
    pub rewrite_apply_in_progress: bool,
}

fn abbrev_oid(oid: &ObjectId) -> String {
    oid.to_hex()[..7].to_string()
}

fn read_trimmed_line(path: &Path) -> Option<String> {
    let s = fs::read_to_string(path).ok()?;
    let mut line = s.lines().next()?.to_string();
    while line.ends_with('\n') || line.ends_with('\r') {
        line.pop();
    }
    if line.is_empty() {
        None
    } else {
        Some(line)
    }
}

/// Read a single-line ref/OID file like Git `get_branch()` in `wt-status.c`.
fn get_branch_display(git_dir: &Path, rel: &str) -> Option<String> {
    let path = git_dir.join(rel);
    let mut sb = read_trimmed_line(&path)?;
    if let Some(branch_name) = sb.strip_prefix("refs/heads/") {
        sb = branch_name.to_string();
    } else if sb.starts_with("refs/") {
        // keep full ref for remotes etc.
    } else if ObjectId::from_hex(&sb).is_ok() {
        let oid = ObjectId::from_hex(&sb).ok()?;
        sb = abbrev_oid(&oid);
    } else if sb == "detached HEAD" {
        return None;
    }
    Some(sb)
}

fn strip_ref_for_display(full: &str) -> String {
    if let Some(s) = full.strip_prefix("refs/tags/") {
        return s.to_string();
    }
    if let Some(s) = full.strip_prefix("refs/remotes/") {
        return s.to_string();
    }
    if let Some(s) = full.strip_prefix("refs/heads/") {
        return s.to_string();
    }
    full.to_string()
}

fn ref_name_exists(git_dir: &Path, refname: &str) -> bool {
    git_dir.join(refname).exists()
        || crate::refs::packed_refs_entry_exists(git_dir, refname).unwrap_or(false)
}

fn dwim_detach_label(git_dir: &Path, target: &str, noid: ObjectId) -> String {
    if target == "HEAD" {
        return abbrev_oid(&noid);
    }
    if target.starts_with("refs/") {
        if let Ok(oid) = crate::refs::resolve_ref(git_dir, target) {
            if oid == noid {
                return strip_ref_for_display(target);
            }
        }
    }
    for candidate in [
        format!("refs/heads/{target}"),
        format!("refs/tags/{target}"),
        format!("refs/remotes/{target}"),
    ] {
        if candidate.starts_with("refs/tags/") && ref_name_exists(git_dir, &candidate) {
            return strip_ref_for_display(&candidate);
        }
        if let Ok(oid) = crate::refs::resolve_ref(git_dir, &candidate) {
            if oid == noid {
                return strip_ref_for_display(&candidate);
            }
        }
    }
    if ObjectId::is_hex_len(target.len()) {
        if let Ok(oid) = ObjectId::from_hex(target) {
            if oid == noid {
                return abbrev_oid(&noid);
            }
        }
    }
    // `checkout … to <abbrev>` records the object name from the user's input; show that
    // abbreviation (Git does not substitute a tag name here — see t3203 detached HEAD).
    if !target.is_empty()
        && target.chars().all(|c| c.is_ascii_hexdigit())
        && target.len() <= noid.algo().hex_len()
        && noid.to_hex().starts_with(target)
    {
        return target.to_owned();
    }
    abbrev_oid(&noid)
}

fn wt_status_get_detached_from(git_dir: &Path, head_oid: ObjectId) -> Option<(String, bool)> {
    let entries = reflog::read_reflog(git_dir, "HEAD").ok()?;
    for entry in entries.iter().rev() {
        let msg = entry.message.trim();
        let Some(rest) = msg.strip_prefix("checkout: moving from ") else {
            continue;
        };
        let Some(idx) = rest.rfind(" to ") else {
            continue;
        };
        let target = rest[idx + 4..].trim();
        let noid = entry.new_oid;
        let label = dwim_detach_label(git_dir, target, noid);
        let detached_at = head_oid == noid;
        return Some((label, detached_at));
    }
    None
}

fn wt_status_check_rebase(git_dir: &Path, state: &mut WtStatusState) -> bool {
    let apply = git_dir.join("rebase-apply");
    if apply.is_dir() {
        if apply.join("applying").exists() {
            state.am_in_progress = true;
            let patch = apply.join("patch");
            if let Ok(meta) = patch.metadata() {
                if meta.len() == 0 {
                    state.am_empty_patch = true;
                }
            }
        } else {
            state.rebase_in_progress = true;
            state.rebase_branch = get_branch_display(git_dir, "rebase-apply/head-name");
            state.rebase_onto = get_branch_display(git_dir, "rebase-apply/onto");
        }
        return true;
    }
    let merge = git_dir.join("rebase-merge");
    if merge.is_dir() {
        let grit_plan = grit_rebase_plan_active(&merge);
        if merge.join("interactive").exists() && !grit_plan {
            state.rebase_interactive_in_progress = true;
        } else {
            state.rebase_in_progress = true;
        }
        state.grit_rebase_in_progress = grit_plan;
        state.rebase_branch = get_branch_display(git_dir, "rebase-merge/head-name");
        state.rebase_onto = get_branch_display(git_dir, "rebase-merge/onto");
        return true;
    }
    false
}

fn sequencer_first_replay(git_dir: &Path) -> Option<bool> {
    let path = git_dir.join("sequencer").join("todo");
    if !path.is_file() {
        return None;
    }
    let content = fs::read_to_string(&path).ok()?;
    for line in content.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let mut parts = t.split_whitespace();
        let cmd = parts.next()?;
        return match cmd {
            "pick" | "p" => Some(true),
            "revert" | "r" => Some(false),
            _ => None,
        };
    }
    None
}

/// Fill [`WtStatusState`] the same way Git `wt_status_get_state` does (without sparse checkout %).
///
/// `get_detached_from` matches Git's third parameter: when true and `head` is detached, populate
/// `detached_from` / `detached_at` from the `HEAD` reflog.
pub fn wt_status_get_state(
    git_dir: &Path,
    head: &HeadState,
    get_detached_from: bool,
) -> Result<WtStatusState> {
    let mut state = WtStatusState::default();

    if git_dir.join("MERGE_HEAD").exists() {
        wt_status_check_rebase(git_dir, &mut state);
        state.merge_in_progress = true;
    } else if wt_status_check_rebase(git_dir, &mut state) {
        // rebase/am state already filled
    } else if let Some(oid) = read_cherry_pick_head(git_dir)? {
        state.cherry_pick_in_progress = true;
        state.cherry_pick_head_oid = Some(oid);
    }

    let bisect_base = crate::refs::common_dir(git_dir).unwrap_or_else(|| git_dir.to_path_buf());
    if bisect_base.join("BISECT_LOG").exists() {
        state.bisect_in_progress = true;
        state.bisecting_from = get_branch_display(&bisect_base, "BISECT_START");
    }

    if let Some(oid) = read_revert_head(git_dir)? {
        state.revert_in_progress = true;
        state.revert_head_oid = Some(oid);
    }

    if let Some(is_pick) = sequencer_first_replay(git_dir) {
        if is_pick && !state.cherry_pick_in_progress {
            state.cherry_pick_in_progress = true;
            state.cherry_pick_head_oid = None;
        } else if !is_pick && !state.revert_in_progress {
            state.revert_in_progress = true;
            state.revert_head_oid = None;
        }
    }

    if get_detached_from {
        if let HeadState::Detached { oid } = head {
            if let Some((label, at)) = wt_status_get_detached_from(git_dir, *oid) {
                state.detached_from = Some(label);
                state.detached_at = at;
            }
        }
    }

    state.rewrite_apply_in_progress = crate::history::rewrite_apply_journal_active(git_dir);

    state.grit_replay_sequencer = git_dir.join("sequencer").join("grit-opts").is_file();
    if state.grit_replay_sequencer {
        if let Some((kind, empty_stop)) = read_grit_replay_status_hints(git_dir) {
            state.grit_replay_kind = Some(kind);
            state.grit_replay_empty_stop = empty_stop;
            match kind {
                InProgressOperation::CherryPick => {
                    state.revert_in_progress = false;
                    state.revert_head_oid = None;
                    if !state.cherry_pick_in_progress {
                        state.cherry_pick_in_progress = true;
                    }
                }
                InProgressOperation::Revert => {
                    state.cherry_pick_in_progress = false;
                    state.cherry_pick_head_oid = None;
                    if !state.revert_in_progress {
                        state.revert_in_progress = true;
                    }
                }
                _ => {}
            }
        }
    }

    Ok(state)
}

fn read_grit_replay_status_hints(git_dir: &Path) -> Option<(InProgressOperation, bool)> {
    let path = git_dir.join("sequencer").join("grit-opts");
    let content = fs::read_to_string(&path).ok()?;
    let mut direction = None;
    let mut empty = "stop";
    for line in content.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "direction" => {
                direction = Some(if value.trim() == "revert" {
                    InProgressOperation::Revert
                } else {
                    InProgressOperation::CherryPick
                });
            }
            "empty" => empty = value.trim(),
            _ => {}
        }
    }
    let kind = direction?;
    let empty_stop = empty == "stop"
        && (read_cherry_pick_head(git_dir).ok().flatten().is_some()
            || read_revert_head(git_dir).ok().flatten().is_some());
    Some((kind, empty_stop))
}

/// Whether a split commit is in progress during interactive rebase (`wt-status.c` `split_commit_in_progress`).
pub fn split_commit_in_progress(git_dir: &Path, head: &HeadState) -> bool {
    let HeadState::Detached { oid: head_oid } = head else {
        return false;
    };
    let Some(amend_line) = read_trimmed_line(&git_dir.join("rebase-merge/amend")) else {
        return false;
    };
    let Some(orig_line) = read_trimmed_line(&git_dir.join("rebase-merge/orig-head")) else {
        return false;
    };
    let Ok(amend_oid) = ObjectId::from_hex(amend_line.trim()) else {
        return false;
    };
    let Ok(orig_head_oid) = ObjectId::from_hex(orig_line.trim()) else {
        return false;
    };
    if amend_line == orig_line {
        head_oid != &amend_oid
    } else if let Ok(Some(cur_orig)) = read_orig_head(git_dir) {
        cur_orig != orig_head_oid
    } else {
        false
    }
}

/// Build a complete [`RepoState`] snapshot for a repository.
///
/// # Parameters
///
/// - `git_dir` — path to the `.git` directory.
/// - `is_bare` — whether this is a bare repository.
///
/// # Errors
///
/// Returns [`Error::Io`] on filesystem failures.
pub fn repo_state(git_dir: &Path, is_bare: bool) -> Result<RepoState> {
    let head = resolve_head(git_dir)?;
    let in_progress = detect_in_progress(git_dir);

    Ok(RepoState {
        head,
        in_progress,
        is_bare,
    })
}

/// Read the MERGE_HEAD file and return the OIDs listed.
///
/// # Parameters
///
/// - `git_dir` — path to the `.git` directory.
///
/// # Returns
///
/// A vector of merge parent OIDs, or empty if not in a merge.
pub fn read_merge_heads(git_dir: &Path) -> Result<Vec<ObjectId>> {
    let path = git_dir.join("MERGE_HEAD");
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::Io(e)),
    };

    let mut oids = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            oids.push(ObjectId::from_hex(trimmed)?);
        }
    }
    Ok(oids)
}

/// Read the MERGE_MSG file.
///
/// # Parameters
///
/// - `git_dir` — path to the `.git` directory.
///
/// # Returns
///
/// The merge message text, or `None` if not in a merge.
pub fn read_merge_msg(git_dir: &Path) -> Result<Option<String>> {
    let path = git_dir.join("MERGE_MSG");
    match fs::read_to_string(&path) {
        Ok(c) => Ok(Some(c)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Remove merge-in-progress sentinel files after a successful merge commit.
///
/// Prefer [`finish_merge_state`] for merge commits; it also restores autostash and clears rerere.
///
/// # Parameters
///
/// - `git_dir` — path to the `.git` directory.
///
/// # Errors
///
/// Returns [`Error::Io`] if a present sentinel file cannot be removed.
pub fn clear_merge_state(git_dir: &Path) -> Result<()> {
    for name in merge_completion_sentinels() {
        let path = git_dir.join(name);
        if path.exists() {
            fs::remove_file(&path).map_err(Error::Io)?;
        }
    }
    Ok(())
}

/// Remove cherry-pick, revert, and sequencer state without touching merge sentinels.
///
/// Git's `reset` clears merge state separately; replay abort uses this together with
/// [`clear_merge_state`] after restoring `HEAD`.
///
/// # Parameters
///
/// - `git_dir` — path to the `.git` directory.
///
/// # Errors
///
/// Returns [`Error::Io`] if a sentinel file or the sequencer directory cannot be removed.
pub fn clear_replay_state(git_dir: &Path) -> Result<()> {
    for name in ["CHERRY_PICK_HEAD", "REVERT_HEAD", "MERGE_MSG"] {
        let path = git_dir.join(name);
        if path.exists() {
            fs::remove_file(&path).map_err(Error::Io)?;
        }
    }
    let seq = git_dir.join("sequencer");
    if seq.is_dir() {
        fs::remove_dir_all(&seq).map_err(Error::Io)?;
    }
    Ok(())
}

/// Sentinel filenames Git removes when a merge concludes with `git commit`.
#[must_use]
pub fn merge_completion_sentinels() -> &'static [&'static str] {
    &[
        "MERGE_HEAD",
        "MERGE_MSG",
        "MERGE_MODE",
        "MERGE_LOG",
        "AUTO_MERGE",
        "SQUASH_MSG",
        "MERGE_AUTOSTASH",
    ]
}

/// Complete merge auxiliary state after a successful merge commit.
///
/// Applies `MERGE_AUTOSTASH` (when present), clears rerere merge records, then removes merge
/// sentinel files via [`clear_merge_state`].
///
/// # Returns
///
/// `true` when applying the merge autostash left conflicts (the autostash entry is kept).
///
/// # Errors
///
/// Propagates autostash apply, rerere, and I/O failures.
pub fn finish_merge_state(repo: &Repository) -> Result<bool> {
    let autostash_conflicts = crate::porcelain::stash::apply_merge_autostash(repo)?;
    let _ = crate::rerere::finish_merge_rerere(repo)?;
    clear_merge_state(repo.git_dir())?;
    Ok(autostash_conflicts)
}

/// Read CHERRY_PICK_HEAD when it contains a valid 40-hex OID; `None` if missing, empty, or invalid
/// (Git ignores malformed `CHERRY_PICK_HEAD` for the "commit $abbrev" line; sequencer still applies).
pub fn read_cherry_pick_head(git_dir: &Path) -> Result<Option<ObjectId>> {
    read_oid_head_file_optional(&git_dir.join("CHERRY_PICK_HEAD"))
}

/// Read REVERT_HEAD when it contains a valid OID; `None` if missing, empty, or invalid.
pub fn read_revert_head(git_dir: &Path) -> Result<Option<ObjectId>> {
    read_oid_head_file_optional(&git_dir.join("REVERT_HEAD"))
}

fn read_oid_head_file_optional(path: &Path) -> Result<Option<ObjectId>> {
    match fs::read_to_string(path) {
        Ok(content) => {
            let trimmed = content.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(ObjectId::from_hex(trimmed).ok())
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Read the loose `$GIT_DIR/ORIG_HEAD` file.
///
/// Only sees a files-backend `ORIG_HEAD`; use [`read_orig_head_for_repository`] to read it
/// through the repository's ref store (reftable or injected stores).
///
/// # Errors
///
/// Returns [`Error::Io`] when the file cannot be read and an object id parse error when it
/// does not hold one.
pub fn read_orig_head(git_dir: &Path) -> Result<Option<ObjectId>> {
    read_single_oid_file(&git_dir.join("ORIG_HEAD"))
}

/// Read `ORIG_HEAD` through `repo`'s ref store.
///
/// # Returns
///
/// The commit `ORIG_HEAD` resolves to, or `None` when it is not set.
///
/// # Errors
///
/// Propagates ref store read and resolution failures.
pub fn read_orig_head_for_repository(repo: &Repository) -> Result<Option<ObjectId>> {
    repo.references().resolve_if_exists("ORIG_HEAD")
}

/// Record `oid` as `ORIG_HEAD`, the `HEAD` an operation started from.
///
/// Reset, merge and rebase call this so `ORIG_HEAD` names the pre-operation commit and
/// `reset --hard ORIG_HEAD` undoes them. The value goes through `repo`'s ref store (the loose
/// `$GIT_DIR/ORIG_HEAD` file on the files backend, the reftable stack otherwise), overwrites
/// any previous value, and writes no reflog, as Git does.
///
/// # Errors
///
/// Propagates ref store write failures (for example a held lock).
pub fn write_orig_head(repo: &Repository, oid: ObjectId) -> Result<()> {
    crate::refs::update_direct_ref_on_store(repo.refs(), repo.git_dir(), "ORIG_HEAD", &oid)
}

/// Read a file that contains a single OID on its first line.
fn read_single_oid_file(path: &Path) -> Result<Option<ObjectId>> {
    match fs::read_to_string(path) {
        Ok(content) => {
            let trimmed = content.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(ObjectId::from_hex(trimmed)?))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::Io(e)),
    }
}

#[cfg(test)]
mod merge_sentinel_tests {
    #![allow(clippy::unwrap_used)]

    use super::{clear_merge_state, merge_completion_sentinels};
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn clear_merge_state_removes_every_completion_sentinel() {
        let tmp = TempDir::new().unwrap();
        let git = tmp.path().join(".git");
        fs::create_dir_all(&git).unwrap();
        for name in merge_completion_sentinels() {
            fs::write(git.join(name), b"x").unwrap();
        }
        clear_merge_state(&git).unwrap();
        for name in merge_completion_sentinels() {
            assert!(!git.join(name).exists(), "expected {name} to be removed");
        }
    }
}

#[cfg(test)]
mod head_state_tests {
    #![allow(clippy::unwrap_used)]

    use super::{resolve_head, resolve_head_for_repository, HeadState};
    use crate::error::Error;
    use crate::objects::{ObjectId, ObjectKind};
    use crate::repo::init_repository;
    use crate::RefStorageFormat;
    use tempfile::TempDir;

    fn unborn_main() -> HeadState {
        HeadState::Branch {
            refname: "refs/heads/main".to_owned(),
            short_name: "main".to_owned(),
            oid: None,
        }
    }

    #[test]
    fn unborn_branch_resolves_without_oid_on_every_backend() {
        for format in [RefStorageFormat::Files, RefStorageFormat::Reftable] {
            let tmp = TempDir::new().unwrap();
            let repo = init_repository(tmp.path(), false, "main", None, format).unwrap();
            assert_eq!(resolve_head(repo.git_dir()).unwrap(), unborn_main());
            assert_eq!(resolve_head_for_repository(&repo).unwrap(), unborn_main());
        }
    }

    #[test]
    fn head_symref_to_missing_non_branch_is_invalid() {
        let tmp = TempDir::new().unwrap();
        let repo =
            init_repository(tmp.path(), false, "main", None, RefStorageFormat::Files).unwrap();
        std::fs::write(repo.git_dir().join("HEAD"), "ref: refs/tags/missing\n").unwrap();
        assert_eq!(resolve_head(repo.git_dir()).unwrap(), HeadState::Invalid);
        assert_eq!(
            resolve_head_for_repository(&repo).unwrap(),
            HeadState::Invalid
        );
    }

    fn commit_on_head(repo: &crate::repo::Repository) -> (ObjectId, ObjectId) {
        let tree = repo.odb().write(ObjectKind::Tree, b"").unwrap();
        let body = format!(
            "tree {tree}\nauthor A <a@example.com> 0 +0000\ncommitter A <a@example.com> 0 +0000\n\nc\n"
        );
        let commit = repo
            .odb()
            .write(ObjectKind::Commit, body.as_bytes())
            .unwrap();
        std::fs::write(repo.git_dir().join("HEAD"), format!("{commit}\n")).unwrap();
        (commit, tree)
    }

    #[test]
    fn head_tree_is_none_on_an_unborn_branch_on_every_backend() {
        for format in [RefStorageFormat::Files, RefStorageFormat::Reftable] {
            let tmp = TempDir::new().unwrap();
            let repo = init_repository(tmp.path(), false, "main", None, format).unwrap();
            assert_eq!(repo.head_tree().unwrap(), None);
            assert_eq!(unborn_main().tree(repo.odb()).unwrap(), None);
        }
    }

    #[test]
    fn head_tree_is_the_tree_of_the_head_commit() {
        let tmp = TempDir::new().unwrap();
        let repo =
            init_repository(tmp.path(), false, "main", None, RefStorageFormat::Files).unwrap();
        let (commit, tree) = commit_on_head(&repo);
        assert_eq!(repo.head_tree().unwrap(), Some(tree));
        assert_eq!(
            HeadState::Detached { oid: commit }
                .tree(repo.odb())
                .unwrap(),
            Some(tree)
        );
    }

    #[test]
    fn invalid_head_is_an_error_for_head_tree_and_none_for_head_state_tree() {
        let tmp = TempDir::new().unwrap();
        let repo =
            init_repository(tmp.path(), false, "main", None, RefStorageFormat::Files).unwrap();
        std::fs::write(repo.git_dir().join("HEAD"), "ref: refs/tags/missing\n").unwrap();
        assert!(matches!(repo.head_tree(), Err(Error::InvalidHead)));
        assert_eq!(HeadState::Invalid.tree(repo.odb()).unwrap(), None);
    }

    #[test]
    fn head_on_a_non_commit_is_an_unexpected_object_kind() {
        let tmp = TempDir::new().unwrap();
        let repo =
            init_repository(tmp.path(), false, "main", None, RefStorageFormat::Files).unwrap();
        let blob = repo.odb().write(ObjectKind::Blob, b"data").unwrap();
        std::fs::write(repo.git_dir().join("HEAD"), format!("{blob}\n")).unwrap();
        assert!(matches!(
            repo.head_tree(),
            Err(Error::UnexpectedObjectKind {
                expected: ObjectKind::Commit,
                actual: ObjectKind::Blob,
                ..
            })
        ));
    }
}

#[cfg(test)]
mod switch_guard_tests {
    #![allow(clippy::unwrap_used)]

    use super::{operation_blocking_switch, InProgressOperation};
    use std::fs;
    use tempfile::TempDir;

    fn blocking(setup: impl FnOnce(&std::path::Path)) -> Option<InProgressOperation> {
        let tmp = TempDir::new().unwrap();
        let git = tmp.path().join(".git");
        fs::create_dir_all(&git).unwrap();
        setup(&git);
        operation_blocking_switch(&git).unwrap()
    }

    #[test]
    fn clean_and_bisect_do_not_block_switch() {
        assert_eq!(blocking(|_| {}), None);
        assert_eq!(
            blocking(|git| fs::write(git.join("BISECT_LOG"), "").unwrap()),
            None
        );
    }

    #[test]
    fn paused_operations_block_switch() {
        let oid = "1111111111111111111111111111111111111111\n";
        assert_eq!(
            blocking(|git| fs::write(git.join("MERGE_HEAD"), oid).unwrap()),
            Some(InProgressOperation::Merge)
        );
        assert_eq!(
            blocking(|git| {
                fs::create_dir_all(git.join("rebase-merge")).unwrap();
                fs::write(git.join("rebase-merge/interactive"), "").unwrap();
            }),
            Some(InProgressOperation::RebaseInteractive)
        );
        assert_eq!(
            blocking(|git| {
                fs::create_dir_all(git.join("rebase-merge")).unwrap();
                fs::write(git.join("rebase-merge/interactive"), "").unwrap();
                fs::write(git.join("rebase-merge/grit-plan"), "{}").unwrap();
            }),
            Some(InProgressOperation::Rebase)
        );
        assert_eq!(
            blocking(|git| fs::create_dir_all(git.join("rebase-apply")).unwrap()),
            Some(InProgressOperation::Rebase)
        );
        assert_eq!(
            blocking(|git| {
                fs::create_dir_all(git.join("rebase-apply")).unwrap();
                fs::write(git.join("rebase-apply/applying"), "").unwrap();
            }),
            Some(InProgressOperation::Am)
        );
        assert_eq!(
            blocking(|git| fs::write(git.join("CHERRY_PICK_HEAD"), oid).unwrap()),
            Some(InProgressOperation::CherryPick)
        );
        assert_eq!(
            blocking(|git| fs::write(git.join("REVERT_HEAD"), oid).unwrap()),
            Some(InProgressOperation::Revert)
        );
    }
}
