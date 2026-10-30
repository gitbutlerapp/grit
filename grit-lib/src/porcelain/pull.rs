//! Fetch from a remote and integrate the configured upstream into the current branch.
//!
//! [`pull`] runs fetch, resolves the branch upstream via [`resolve_pull_integration_base`], then
//! fast-forwards, merges, rebases local commits onto the upstream tip, or refuses
//! when `--ff-only` / `pull.ff=only` forbids a merge commit. Rebase conflicts stop a
//! resumable rebase session ([`start_rebase`]) with Git-compatible `.git/rebase-merge/`
//! state, finished with [`continue_rebase`](crate::history::continue_rebase),
//! [`skip_rebase`](crate::history::skip_rebase) or [`abort_rebase`](crate::history::abort_rebase).
//! A caller that fetched the upstream itself passes it as [`PullOptions::upstream`].

use crate::branch_tracking::resolve_pull_integration_base;
use crate::change_id::{logical_change_id_for_commit, ChangeId};
use crate::config::environment::Environment;
use crate::config::ident_resolve::IdentRole;
use crate::config::parse_git_config_maybe_bool;
use crate::config::ConfigSet;
use crate::error::{Error, OverwriteReason};
use crate::fetch_request::{configured_remote_for_pull, resolve_remote_argument};
use crate::merge_base::{count_symmetric_ahead_behind, is_ancestor, merge_bases_first_vs_rest};
use crate::objects::ObjectId;
use crate::operation::Conflict;
use crate::porcelain::checkout::checkout_between_trees;
use crate::porcelain::checkout_reflog::checkout_reflog_identity;
use crate::porcelain::merge::{
    ensure_branch_tip, integrate_branch_with_options, predict_merge, update_checked_out_branch,
    IntegrateBranchOptions, IntegrateBranchOutcome, MergePrediction, MergePredictionOptions,
};
use crate::porcelain::stash::{
    apply_operation_autostash, push_operation_autostash, record_merge_autostash,
    restore_operation_autostash,
};
use crate::porcelain::switch::checkout_tree_transition_allow_local_reset;
use crate::porcelain::worktree_guard::prepare_tree_checkout;
use crate::progress::{Cancel, ProgressSink};
use crate::remote::fetch::Progress;
use crate::remote::transfer::{resolve_fetch_prune, FetchOptions, TagMode, TransferStats};
use crate::remote::{HttpClientFactory, RemoteError};
use crate::repo::Repository;
use crate::revwalk::history::{
    plan_rebase_check, start_rebase, ApplyOptions, EmptyPolicy, HistoryError, PreRebase,
    RebaseCheckPlan, RebaseOntoOptions, RebaseOutcome, RebaseRequest, RebaseSessionError, RefScope,
    RewriteOptions, StartRebaseOptions,
};
use crate::state::{resolve_head_for_repository, HeadState};
use thiserror::Error;

const INITIAL_PULL_REFLOG: &str = "initial pull";

/// Resolved integration behaviour after config and CLI overrides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PullIntegration {
    mode: PullMode,
    allow_fast_forward: bool,
}

/// How [`pull`] integrates upstream commits when a fast-forward is not possible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PullMode {
    /// Refuse when diverged history would require a merge commit.
    FfOnly,
    /// Merge (or fast-forward when possible).
    Merge,
    /// Rebase local commits onto the upstream tip via the history engine, in one line like
    /// `git pull --rebase`: merge commits on the branch are dropped.
    Rebase,
}

/// Options for [`pull`].
#[derive(Clone, Default)]
pub struct PullOptions {
    /// Remote name override; when `None`, uses [`configured_remote_for_pull`].
    pub remote: Option<String>,
    /// Remote branch or refspec to integrate (`pull origin feature`); also see [`Self::fetch`].
    pub branch: Option<String>,
    /// When true, integration uses [`FETCH_HEAD`](https://git-scm.com/docs/git-fetch#_discussion) after fetch.
    pub explicit_fetch: bool,
    /// Stash dirty worktree/index changes around the integration (`pull --autostash`).
    pub autostash: bool,
    /// Fetch and report whether a fast-forward is possible without changing refs or the worktree.
    pub check: bool,
    /// CLI override for integration mode; when `None`, [`resolve_pull_mode`] applies config.
    pub mode: Option<PullMode>,
    /// Fetch options (tags, prune, depth, environment, …).
    pub fetch: FetchOptions,
    /// An upstream the caller already fetched: [`pull`] integrates it and fetches nothing.
    ///
    /// Snapshot checkouts set this: they fetch the server's snapshot themselves, from the URL
    /// the checkout records, and then bring the current branch onto it.
    pub upstream: Option<FetchedUpstream>,
    /// Report a rebase that stops on a conflict as [`PullResult::RebaseStopped`], with
    /// [`PullOutcome::oid`] at the branch's pre-pull tip, instead of
    /// [`PullResult::RebaseConflicts`]. Both leave the same resumable rebase session; snapshot
    /// checkouts set this so their refresh can tell its own stop apart. Ignored when an
    /// autostash was made.
    pub stop_on_conflict: bool,
}

/// An upstream commit [`pull`] integrates without fetching ([`PullOptions::upstream`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchedUpstream {
    /// What to call it in results and reflog messages, such as `origin/main`.
    pub label: String,
    /// The commit to integrate.
    pub oid: ObjectId,
    /// A commit of the current branch whose changes [`Self::oid`] already has, though not as
    /// an ancestor: work a server merged into the branch [`Self::oid`] is a snapshot of. The
    /// branch's commits up to it are never replayed. A rebase replays only the commits above
    /// it onto [`Self::oid`] (`git rebase --onto <oid> <landed>`), and a branch with nothing
    /// above it moves to [`Self::oid`] in any mode, keeping uncommitted changes as
    /// `git reset --keep` does ([`PullResult::Landed`]). Ignored unless it is in the
    /// branch's history.
    pub landed: Option<ObjectId>,
    /// Commits of the current branch above [`Self::landed`] whose changes [`Self::oid`] already
    /// has as other commits, which a patch-id can't match (a copy made on another base,
    /// recognized by its change id): a rebase leaves them out, replaying the next commit onto
    /// the one before.
    pub copies: Vec<ObjectId>,
}

/// Dry-run result for [`PullOptions::check`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullCheck {
    /// Commits on the current branch not reachable from the upstream tip.
    pub ahead: usize,
    /// Commits on the upstream tip not reachable from the current branch.
    pub behind: usize,
    /// Whether the branch could fast-forward to the upstream tip.
    pub fast_forward_possible: bool,
    /// Integration result if `--check` were not set.
    pub would: PullResult,
}

/// High-level result of a successful [`pull`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PullResult {
    /// The branch already contained the upstream tip.
    UpToDate,
    /// The branch fast-forwarded to the upstream tip.
    FastForward,
    /// A merge commit integrated the upstream tip.
    Merged,
    /// Diverged history and ff-only mode refused to merge.
    RefusedNotFf,
    /// A merge was attempted but stopped with conflicts.
    Conflicts,
    /// An unborn branch adopted the upstream as its first commit.
    SetUpstream,
    /// Local commits were replayed onto the upstream tip.
    Rebased,
    /// Replaying local commits onto the upstream tip stopped on conflicts.
    ///
    /// A resumable rebase session is in progress (`.git/rebase-merge/`, conflict markers and
    /// unmerged index entries, as `git pull --rebase` leaves them); the branch ref still points
    /// at its pre-pull tip. Finish it with [`continue_rebase`](crate::history::continue_rebase)
    /// after staging the resolution, or [`skip_rebase`](crate::history::skip_rebase) /
    /// [`abort_rebase`](crate::history::abort_rebase). With [`PullOptions::check`] this
    /// predicts the stop without touching anything.
    RebaseConflicts,
    /// Integration succeeded but re-applying `--autostash` left conflicts (stash kept).
    AutostashConflict,
    /// [`PullResult::RebaseConflicts`] for a pull that set [`PullOptions::stop_on_conflict`]:
    /// the conflicted files are in the worktree and index, `HEAD` is detached on the commits
    /// replayed so far, and the branch still points at its old tip until the rebase is
    /// continued (or is put back by aborting it).
    RebaseStopped,
    /// Every commit of the branch was already in the upstream, though not as an ancestor
    /// ([`FetchedUpstream::landed`]): the branch moved to the upstream tip instead of
    /// replaying them, keeping uncommitted and untracked changes as `git reset --keep` does.
    Landed,
}

/// Structured outcome of [`pull`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullOutcome {
    /// Integration result.
    pub result: PullResult,
    /// Remote-tracking label integrated (for example `origin/main`).
    pub label: String,
    /// Branch tip after a successful integration, when applicable.
    pub oid: Option<ObjectId>,
    /// Stored `change-id` on [`Self::oid`] when present.
    pub change_id: Option<ChangeId>,
    /// For [`PullResult::SetUpstream`], the upstream label that was adopted.
    pub upstream: Option<String>,
    /// Conflicted paths when [`Self::result`] is [`PullResult::Conflicts`] or
    /// [`PullResult::RebaseConflicts`].
    pub conflicts: Option<Vec<Conflict>>,
    /// For [`PullResult::RebaseConflicts`], the local commit that could not be replayed.
    pub stopped_commit: Option<ObjectId>,
    /// Autostash commit left on `refs/stash` because it could not be re-applied: the pull
    /// stopped on rebase conflicts ([`PullResult::RebaseConflicts`]) or re-applying it
    /// conflicted ([`PullResult::AutostashConflict`]).
    pub autostash: Option<ObjectId>,
    /// Commits on the branch not reachable from upstream when refused as not ff.
    pub ahead: Option<usize>,
    /// Commits on upstream not reachable from the branch when refused as not ff.
    pub behind: Option<usize>,
    /// Populated when [`PullOptions::check`] is set; the repository is unchanged.
    pub check: Option<PullCheck>,
    /// Objects and pack bytes the fetch step received (zero when nothing was
    /// transferred or the upstream is local, `remote = .`).
    pub transfer: TransferStats,
}

impl PullOutcome {
    fn new(result: PullResult, label: &str) -> Self {
        Self {
            result,
            label: label.to_owned(),
            oid: None,
            change_id: None,
            upstream: None,
            conflicts: None,
            stopped_commit: None,
            autostash: None,
            ahead: None,
            behind: None,
            check: None,
            transfer: TransferStats::default(),
        }
    }

    fn at_commit(
        result: PullResult,
        label: &str,
        repo: &Repository,
        oid: ObjectId,
    ) -> std::result::Result<Self, Error> {
        Ok(Self {
            oid: Some(oid),
            change_id: logical_change_id_for_commit(repo, oid)?,
            ..Self::new(result, label)
        })
    }
}

/// Errors from [`pull`].
#[derive(Debug, Error)]
pub enum PullError {
    /// `HEAD` points at a detached commit.
    #[error("HEAD is detached; pull needs a branch")]
    DetachedHead,
    /// `HEAD` could not be interpreted.
    #[error("HEAD is in an unknown state")]
    InvalidHead,
    /// No upstream is configured or the tracking ref did not resolve after fetch.
    #[error("no upstream configured for branch {branch}")]
    NoUpstream {
        /// Local branch short name.
        branch: String,
    },
    /// `pull.rebase=interactive` (or branch rebase) is not supported.
    #[error("interactive rebase is not supported; use grit history instead")]
    RebaseInteractive,
    /// Starting the rebase session failed (for example a rebase is already in progress).
    #[error(transparent)]
    Rebase(RebaseSessionError),
    /// Remote resolution or transport failure.
    #[error(transparent)]
    Remote(#[from] RemoteError),
    /// Other repository failures.
    #[error(transparent)]
    Repository(#[from] Error),
}

impl PullError {
    /// Whether the remote could not be read at all ([`Error::RemoteUnreadable`]).
    #[must_use]
    pub fn is_remote_unreadable(&self) -> bool {
        match self {
            Self::Remote(e) => e.is_remote_unreadable(),
            Self::Repository(e) => e.is_remote_unreadable(),
            Self::DetachedHead
            | Self::InvalidHead
            | Self::NoUpstream { .. }
            | Self::RebaseInteractive
            | Self::Rebase(_) => false,
        }
    }

    /// Whether the pull was cancelled ([`Error::Cancelled`]).
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        match self {
            Self::Remote(e) => e.is_cancelled(),
            Self::Repository(e) => e.is_cancelled(),
            Self::Rebase(e) => e.is_cancelled(),
            Self::DetachedHead
            | Self::InvalidHead
            | Self::NoUpstream { .. }
            | Self::RebaseInteractive => false,
        }
    }
}

/// Resolve pull integration mode from config with optional CLI override.
///
/// Precedence matches Git: explicit `--ff-only` / `--no-rebase` / `--rebase`, then
/// `pull.ff=only`, then `branch.<name>.rebase` / `pull.rebase`, then remaining `pull.ff`.
///
/// # Errors
///
/// Returns [`PullError::RebaseInteractive`] when rebase config requests interactive mode.
pub fn resolve_pull_mode(
    config: &ConfigSet,
    branch: Option<&str>,
    cli: Option<PullMode>,
) -> std::result::Result<PullMode, PullError> {
    Ok(resolve_pull_integration(config, branch, cli)?.mode)
}

fn resolve_pull_integration(
    config: &ConfigSet,
    branch: Option<&str>,
    cli: Option<PullMode>,
) -> std::result::Result<PullIntegration, PullError> {
    if cli == Some(PullMode::FfOnly) {
        return Ok(PullIntegration {
            mode: PullMode::FfOnly,
            allow_fast_forward: true,
        });
    }
    if cli == Some(PullMode::Merge) {
        return Ok(PullIntegration {
            mode: PullMode::Merge,
            allow_fast_forward: true,
        });
    }
    if cli == Some(PullMode::Rebase) {
        return Ok(PullIntegration {
            mode: PullMode::Rebase,
            allow_fast_forward: true,
        });
    }
    if let Some(raw) = config.get("pull.ff") {
        let value = raw.trim();
        if value.eq_ignore_ascii_case("only") {
            return Ok(PullIntegration {
                mode: PullMode::FfOnly,
                allow_fast_forward: true,
            });
        }
    }
    if pull_rebase_enabled(config, branch)? {
        return Ok(PullIntegration {
            mode: PullMode::Rebase,
            allow_fast_forward: true,
        });
    }
    if let Some(raw) = config.get("pull.ff") {
        let value = raw.trim();
        if parse_git_config_maybe_bool(value) == Some(false) {
            return Ok(PullIntegration {
                mode: PullMode::Merge,
                allow_fast_forward: false,
            });
        }
    }
    Ok(PullIntegration {
        mode: PullMode::Merge,
        allow_fast_forward: true,
    })
}

impl PullOptions {
    /// Build pull options for the current branch from config and CLI mode flags.
    ///
    /// # Errors
    ///
    /// Returns [`PullError::DetachedHead`] or [`PullError::InvalidHead`] when `HEAD`
    /// is not on a branch.
    pub fn for_working_branch(
        repo: &Repository,
        environment: Environment,
        cli_mode: Option<PullMode>,
    ) -> std::result::Result<Self, PullError> {
        if let Some(op) = crate::state::operation_blocking_switch(repo.git_dir())
            .map_err(PullError::Repository)?
        {
            return Err(PullError::Repository(Error::OperationInProgress(op)));
        }
        let short_name = match resolve_head_for_repository(repo).map_err(PullError::Repository)? {
            HeadState::Branch { short_name, .. } => short_name,
            HeadState::Detached { .. } => return Err(PullError::DetachedHead),
            HeadState::Invalid => return Err(PullError::InvalidHead),
        };
        let config = repo.config().map_err(PullError::Repository)?;
        let remote_name = configured_remote_for_pull(config.as_ref(), Some(&short_name));
        let prune = resolve_fetch_prune(config.as_ref(), &remote_name, None)
            .map_err(PullError::Repository)?;
        Ok(Self {
            remote: None,
            branch: None,
            explicit_fetch: false,
            autostash: false,
            check: false,
            mode: cli_mode,
            fetch: {
                let mut opts = FetchOptions::new();
                opts.tags = TagMode::Following;
                opts.thread_policy = repo.thread_policy();
                opts.environment = environment;
                opts.prune = prune;
                opts.remote_name = Some(remote_name);
                opts.reflog_action = "pull";
                opts
            },
            upstream: None,
            stop_on_conflict: false,
        })
    }
}

fn pull_rebase_enabled(
    config: &ConfigSet,
    branch: Option<&str>,
) -> std::result::Result<bool, PullError> {
    if let Some(name) = branch {
        if let Some(raw) = config.get(&format!("branch.{name}.rebase")) {
            return parse_pull_rebase_config(raw.as_str());
        }
    }
    if let Some(raw) = config.get("pull.rebase") {
        return parse_pull_rebase_config(raw.as_str());
    }
    Ok(false)
}

/// Whether `--autostash` is active for this pull (CLI flag or config).
#[must_use]
pub fn resolve_pull_autostash(config: &ConfigSet, mode: PullMode, cli_autostash: bool) -> bool {
    if cli_autostash {
        return true;
    }
    let key = match mode {
        PullMode::Rebase => "rebase.autoStash",
        PullMode::FfOnly | PullMode::Merge => "merge.autoStash",
    };
    config
        .get(key)
        .and_then(|raw| parse_git_config_maybe_bool(raw.trim()))
        .unwrap_or(false)
}

fn parse_pull_rebase_config(raw: &str) -> std::result::Result<bool, PullError> {
    let value = raw.trim();
    if value.eq_ignore_ascii_case("interactive") {
        return Err(PullError::RebaseInteractive);
    }
    if value.eq_ignore_ascii_case("false") || value == "0" {
        return Ok(false);
    }
    if let Some(parsed) = parse_git_config_maybe_bool(value) {
        return Ok(parsed);
    }
    Ok(true)
}

/// Fetch from a remote and integrate its upstream into the current branch.
///
/// With [`PullOptions::upstream`] set, nothing is fetched and that commit is integrated.
///
/// # Errors
///
/// Returns [`PullError`] for detached HEAD, missing upstream, interactive rebase config,
/// or transport/repository failures.
pub fn pull<P>(
    repo: &Repository,
    options: &PullOptions,
    progress: &mut P,
    cancel: &dyn crate::progress::Cancel,
    http_factory: Option<&dyn HttpClientFactory>,
) -> std::result::Result<PullOutcome, PullError>
where
    P: Progress,
{
    if let Some(op) = crate::state::operation_blocking_switch(repo.git_dir())? {
        return Err(PullError::Repository(Error::OperationInProgress(op)));
    }
    let (refname, short_name, head_oid) = match resolve_head_for_repository(repo)? {
        HeadState::Branch {
            refname,
            short_name,
            oid,
        } => (refname, short_name, oid),
        HeadState::Detached { .. } => return Err(PullError::DetachedHead),
        HeadState::Invalid => return Err(PullError::InvalidHead),
    };

    let config = repo.config().map_err(PullError::Repository)?;
    let remote_name = options
        .remote
        .clone()
        .unwrap_or_else(|| configured_remote_for_pull(config.as_ref(), Some(&short_name)));

    let integration = resolve_pull_integration(config.as_ref(), Some(&short_name), options.mode)?;
    let (explicit_fetch, branch_refspecs) = pull_fetch_refspecs(options);
    let mut fetch_opts = options.fetch.clone();
    fetch_opts.refspecs = branch_refspecs;
    if fetch_opts.remote_name.is_none() {
        fetch_opts.remote_name = Some(remote_name.clone());
    }
    if options.check {
        fetch_opts.dry_run = false;
    }
    let fetch_dry_run = fetch_opts.dry_run;

    let autostash = resolve_pull_autostash(config.as_ref(), integration.mode, options.autostash)
        && !options.check
        && !fetch_dry_run;

    let merge_in_progress =
        || crate::state::read_merge_heads(repo.git_dir()).is_ok_and(|heads| !heads.is_empty());
    let merge_was_in_progress = merge_in_progress();
    let stash_oid = if autostash {
        push_operation_autostash(repo, &checkout_reflog_identity(repo))
            .map_err(PullError::Repository)?
    } else {
        None
    };

    let stop_on_conflict = options.stop_on_conflict && stash_oid.is_none();
    let mut transfer = TransferStats::default();
    let core: std::result::Result<PullOutcome, PullError> = (|| {
        if options.upstream.is_none() && remote_name != "." {
            let remote = resolve_remote_argument(config.as_ref(), &remote_name)
                .map_err(PullError::Remote)?;
            let fetch_outcome = remote
                .fetch(repo, fetch_opts, progress, cancel, http_factory)
                .map_err(PullError::Remote)?;
            if fetch_outcome.has_rejections() {
                return Err(PullError::Repository(Error::Message(
                    "some ref updates from the remote were rejected".into(),
                )));
            }
            transfer = fetch_outcome.transfer;
        }

        if fetch_dry_run {
            return Ok(PullOutcome {
                oid: head_oid,
                change_id: head_oid
                    .and_then(|oid| logical_change_id_for_commit(repo, oid).ok())
                    .flatten(),
                ..PullOutcome::new(PullResult::UpToDate, &short_name)
            });
        }

        let (label, upstream_oid) = match &options.upstream {
            Some(fetched) => (fetched.label.clone(), fetched.oid),
            None => {
                let upstream =
                    resolve_pull_integration_base(repo, &short_name, &remote_name, explicit_fetch)
                        .ok_or(PullError::NoUpstream {
                            branch: short_name.clone(),
                        })?;
                (upstream.name, upstream.oid)
            }
        };

        let landed = options.upstream.as_ref().and_then(|u| u.landed);
        if options.check {
            return pull_check_outcome(repo, head_oid, upstream_oid, &label, integration, landed);
        }

        match head_oid {
            None => adopt_unborn_upstream(repo, &refname, &short_name, &label, upstream_oid),
            Some(head_oid) => integrate_fetched_upstream(
                repo,
                &refname,
                head_oid,
                Upstream {
                    oid: upstream_oid,
                    label: &label,
                    landed,
                    copies: options
                        .upstream
                        .as_ref()
                        .map_or(&[], |u| u.copies.as_slice()),
                },
                integration,
                stop_on_conflict,
                progress,
                cancel,
            ),
        }
    })();
    let core = core.map(|outcome| PullOutcome {
        transfer,
        ..outcome
    });

    match core {
        Err(err) => {
            // A hook that refused the merge commit leaves the merge in progress. The stash then
            // waits in MERGE_AUTOSTASH, as with Git, so `merge --continue` does not auto-stage it.
            match stash_oid.as_ref() {
                Some(oid) if !merge_was_in_progress && merge_in_progress() => {
                    record_merge_autostash(repo, oid).map_err(PullError::Repository)?;
                }
                _ => restore_autostash_after_abort(repo, stash_oid.as_ref()),
            }
            Err(err)
        }
        Ok(outcome) => {
            if fetch_dry_run {
                restore_autostash_after_abort(repo, stash_oid.as_ref());
                return Ok(outcome);
            }
            if outcome.result == PullResult::RebaseConflicts {
                // Applying the stash now would mix it into the conflict markers; it stays on
                // `refs/stash` for the caller to pop once the rebase is finished or aborted.
                return Ok(PullOutcome {
                    autostash: stash_oid,
                    ..outcome
                });
            }
            if matches!(
                outcome.result,
                PullResult::Conflicts | PullResult::RefusedNotFf | PullResult::RebaseStopped
            ) {
                restore_autostash_after_abort(repo, stash_oid.as_ref());
                return Ok(outcome);
            }
            apply_autostash_after_pull(repo, stash_oid.as_ref(), Ok(outcome))
        }
    }
}

/// Merge [`PullOptions::branch`] into fetch refspecs for embedders and tests.
fn pull_fetch_refspecs(options: &PullOptions) -> (bool, Vec<String>) {
    let mut refspecs = options.fetch.refspecs.clone();
    let mut explicit = options.explicit_fetch;
    if let Some(branch) = options.branch.as_deref().filter(|b| !b.is_empty()) {
        if refspecs.is_empty() {
            refspecs.push(branch.to_owned());
        }
        explicit = true;
    }
    (explicit, refspecs)
}

fn restore_autostash_after_abort(repo: &Repository, stash_oid: Option<&ObjectId>) {
    let Some(stash_oid) = stash_oid else {
        return;
    };
    let _ = restore_operation_autostash(repo, stash_oid);
}

fn apply_autostash_after_pull(
    repo: &Repository,
    stash_oid: Option<&ObjectId>,
    outcome: std::result::Result<PullOutcome, PullError>,
) -> std::result::Result<PullOutcome, PullError> {
    let mut outcome = outcome?;
    if stash_oid.is_none() {
        return Ok(outcome);
    }
    if !matches!(
        outcome.result,
        PullResult::UpToDate
            | PullResult::FastForward
            | PullResult::Merged
            | PullResult::Rebased
            | PullResult::Landed
            | PullResult::SetUpstream
    ) {
        return Ok(outcome);
    }
    if let Some(stash_oid) = stash_oid {
        if apply_operation_autostash(repo, stash_oid).map_err(PullError::Repository)? {
            outcome.result = PullResult::AutostashConflict;
            outcome.autostash = Some(*stash_oid);
        }
    }
    Ok(outcome)
}

fn pull_check_outcome(
    repo: &Repository,
    head_oid: Option<ObjectId>,
    upstream_oid: ObjectId,
    label: &str,
    integration: PullIntegration,
    landed: Option<ObjectId>,
) -> std::result::Result<PullOutcome, PullError> {
    let (ahead, behind, would, fast_forward_possible) = match head_oid {
        None => (0, 0, PullResult::SetUpstream, true),
        Some(head_oid) => {
            let (ahead, behind) = count_symmetric_ahead_behind(repo, head_oid, upstream_oid)
                .map_err(PullError::Repository)?;
            let would = if landed == Some(head_oid) && diverged(repo, head_oid, upstream_oid)? {
                PullResult::Landed
            } else {
                predict_pull_result(repo, head_oid, upstream_oid, integration)?
            };
            let up_to_date = head_oid == upstream_oid || is_ancestor(repo, upstream_oid, head_oid)?;
            let fast_forward = !up_to_date
                && is_ancestor(repo, head_oid, upstream_oid).map_err(PullError::Repository)?;
            (ahead, behind, would, up_to_date || fast_forward)
        }
    };
    Ok(PullOutcome {
        oid: head_oid,
        change_id: head_oid
            .and_then(|oid| logical_change_id_for_commit(repo, oid).ok())
            .flatten(),
        ahead: Some(ahead),
        behind: Some(behind),
        check: Some(PullCheck {
            ahead,
            behind,
            fast_forward_possible,
            would,
        }),
        ..PullOutcome::new(would, label)
    })
}

fn predict_pull_result(
    repo: &Repository,
    head_oid: ObjectId,
    upstream_oid: ObjectId,
    integration: PullIntegration,
) -> std::result::Result<PullResult, PullError> {
    if head_oid == upstream_oid || is_ancestor(repo, upstream_oid, head_oid)? {
        return Ok(PullResult::UpToDate);
    }
    if integration.allow_fast_forward && is_ancestor(repo, head_oid, upstream_oid)? {
        return Ok(PullResult::FastForward);
    }
    let diverged = !is_ancestor(repo, upstream_oid, head_oid)?
        && !is_ancestor(repo, head_oid, upstream_oid)?
        && head_oid != upstream_oid;
    if diverged {
        return match integration.mode {
            PullMode::FfOnly => Ok(PullResult::RefusedNotFf),
            PullMode::Rebase => {
                let mut opts = pull_rebase_options(repo)?;
                opts.rewrite.sign = false;
                let request = RebaseRequest {
                    upstream: upstream_oid,
                    onto: None,
                    branch_tip: head_oid,
                };
                match plan_rebase_check(repo, &request, &opts).map_err(map_history_error)? {
                    RebaseCheckPlan::UpToDate | RebaseCheckPlan::Clean { .. } => {
                        Ok(PullResult::Rebased)
                    }
                    RebaseCheckPlan::Conflict { .. } => Ok(PullResult::RebaseConflicts),
                }
            }
            PullMode::Merge => match predict_merge(
                repo,
                head_oid,
                upstream_oid,
                &MergePredictionOptions {
                    allow_fast_forward: integration.allow_fast_forward,
                    theirs_label: "upstream".into(),
                    ..Default::default()
                },
            )
            .map_err(PullError::Repository)?
            {
                MergePrediction::UpToDate => Ok(PullResult::UpToDate),
                MergePrediction::FastForward => Ok(PullResult::FastForward),
                MergePrediction::Clean { .. } => Ok(PullResult::Merged),
                MergePrediction::Conflicts { .. } => Ok(PullResult::Conflicts),
                MergePrediction::Unrelated => Ok(PullResult::Conflicts),
            },
        };
    }
    Ok(PullResult::Merged)
}

fn adopt_unborn_upstream(
    repo: &Repository,
    refname: &str,
    short_name: &str,
    label: &str,
    upstream_oid: ObjectId,
) -> std::result::Result<PullOutcome, PullError> {
    ensure_branch_tip(repo, refname, None)?;
    let upstream_tree = repo.odb().commit_tree(&upstream_oid)?;
    prepare_tree_checkout(repo, None, &upstream_tree).map_err(PullError::Repository)?;
    checkout_between_trees(repo, None, &upstream_tree).map_err(PullError::Repository)?;
    update_checked_out_branch(repo, refname, None, upstream_oid, INITIAL_PULL_REFLOG)?;
    Ok(PullOutcome {
        upstream: Some(label.to_owned()),
        ..PullOutcome::at_commit(PullResult::SetUpstream, short_name, repo, upstream_oid)?
    })
}

/// Whether `head_oid` and `upstream_oid` each have commits the other lacks.
fn diverged(
    repo: &Repository,
    head_oid: ObjectId,
    upstream_oid: ObjectId,
) -> std::result::Result<bool, PullError> {
    Ok(head_oid != upstream_oid
        && !is_ancestor(repo, upstream_oid, head_oid).map_err(PullError::Repository)?
        && !is_ancestor(repo, head_oid, upstream_oid).map_err(PullError::Repository)?)
}

/// The upstream a pull integrates.
#[derive(Clone, Copy)]
struct Upstream<'a> {
    oid: ObjectId,
    label: &'a str,
    /// [`FetchedUpstream::landed`].
    landed: Option<ObjectId>,
    /// [`FetchedUpstream::copies`].
    copies: &'a [ObjectId],
}

#[allow(clippy::too_many_arguments)]
fn integrate_fetched_upstream(
    repo: &Repository,
    refname: &str,
    head_oid: ObjectId,
    upstream: Upstream<'_>,
    integration: PullIntegration,
    stop_on_conflict: bool,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> std::result::Result<PullOutcome, PullError> {
    let Upstream {
        oid: upstream_oid,
        label,
        landed,
        ..
    } = upstream;
    let diverged = diverged(repo, head_oid, upstream_oid)?;

    if diverged && landed == Some(head_oid) {
        return move_landed_branch(
            repo,
            refname,
            head_oid,
            upstream_oid,
            label,
            progress,
            cancel,
        );
    }

    if diverged {
        match integration.mode {
            PullMode::FfOnly => {
                let (ahead, behind) = count_symmetric_ahead_behind(repo, head_oid, upstream_oid)
                    .map_err(PullError::Repository)?;
                return Ok(PullOutcome {
                    ahead: Some(ahead),
                    behind: Some(behind),
                    ..PullOutcome::at_commit(PullResult::RefusedNotFf, label, repo, head_oid)?
                });
            }
            PullMode::Rebase => {
                return rebase_fetched_upstream(
                    repo,
                    refname,
                    head_oid,
                    upstream,
                    stop_on_conflict,
                    progress,
                    cancel,
                );
            }
            PullMode::Merge => {}
        }
    }

    let integrate = integrate_branch_with_options(
        repo,
        refname,
        head_oid,
        upstream_oid,
        label,
        IntegrateBranchOptions {
            allow_fast_forward: integration.allow_fast_forward,
        },
        progress,
        cancel,
    )
    .map_err(PullError::Repository)?;
    Ok(map_integrate_outcome(integrate, label))
}

#[allow(clippy::too_many_arguments)]
fn rebase_fetched_upstream(
    repo: &Repository,
    refname: &str,
    head_oid: ObjectId,
    upstream: Upstream<'_>,
    stop_on_conflict: bool,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> std::result::Result<PullOutcome, PullError> {
    let Upstream {
        oid: upstream_oid,
        label,
        landed,
        copies,
    } = upstream;
    let fork = merge_bases_first_vs_rest(repo, head_oid, &[upstream_oid])
        .map_err(PullError::Repository)?
        .into_iter()
        .next()
        .ok_or_else(|| {
            PullError::Repository(Error::Message(format!(
                "'{label}' has no common history with the current branch"
            )))
        })?;
    // Commits up to a landed one are in the upstream already: replay only those above it,
    // as `git rebase --onto <upstream> <landed>` does.
    let request = match landed {
        Some(base)
            if base != fork
                && is_ancestor(repo, base, head_oid).map_err(PullError::Repository)?
                && is_ancestor(repo, fork, base).map_err(PullError::Repository)? =>
        {
            RebaseRequest {
                upstream: base,
                onto: Some(upstream_oid),
                branch_tip: head_oid,
            }
        }
        _ => RebaseRequest {
            upstream: upstream_oid,
            onto: None,
            branch_tip: head_oid,
        },
    };

    let mut rebase = pull_rebase_options(repo)?;
    // The upstream already has these commits: they are its copies of local ones.
    rebase.drop = copies.to_vec();
    // Git's pull hands rebase the fork point (or the fetched tip) as a commit id.
    let pre_rebase = PreRebase::Run {
        upstream: Some(request.upstream.to_hex()),
    };
    let start = StartRebaseOptions {
        request,
        rebase,
        apply: ApplyOptions {
            identity: repo
                .identity(IdentRole::Committer, repo.identity_now())
                .map_err(PullError::Repository)?,
            reflog_message: format!("pull: rebase onto {label}"),
            record_operation: Some("rebase".into()),
            hooks: crate::hooks::PostOperationHooks::Run,
        },
        pre_rebase,
    };
    match start_rebase(repo, &start, progress, cancel).map_err(map_rebase_session_error)? {
        RebaseOutcome::Stopped(stop) if stop_on_conflict => Ok(PullOutcome {
            conflicts: Some(stop.conflicts),
            stopped_commit: Some(stop.commit),
            ..PullOutcome::at_commit(PullResult::RebaseStopped, label, repo, head_oid)?
        }),
        RebaseOutcome::Stopped(stop) => Ok(PullOutcome {
            conflicts: Some(stop.conflicts),
            stopped_commit: Some(stop.commit),
            ..PullOutcome::new(PullResult::RebaseConflicts, label)
        }),
        RebaseOutcome::Done { applied, .. } | RebaseOutcome::ExecDone { applied, .. }
            if applied.head != head_oid =>
        {
            Ok(PullOutcome::at_commit(
                PullResult::Rebased,
                label,
                repo,
                applied.head,
            )?)
        }
        // Nothing was rewritten: integrate like `git pull --rebase`, which fast-forwards when
        // the branch is simply behind the upstream tip.
        RebaseOutcome::Done { .. } | RebaseOutcome::ExecDone { .. } => {
            let integrate = integrate_branch_with_options(
                repo,
                refname,
                head_oid,
                upstream_oid,
                label,
                IntegrateBranchOptions {
                    allow_fast_forward: true,
                },
                progress,
                cancel,
            )
            .map_err(PullError::Repository)?;
            Ok(map_integrate_outcome(integrate, label))
        }
    }
}

/// Replay options shared by the rebase pull and its `--check` prediction.
fn pull_rebase_options(repo: &Repository) -> std::result::Result<RebaseOntoOptions, PullError> {
    let committer = repo
        .identity(IdentRole::Committer, repo.identity_now())
        .map_err(PullError::Repository)?;
    let mut opts = RebaseOntoOptions::new(RewriteOptions {
        scope: RefScope::Head,
        empty: EmptyPolicy::Drop,
        committer,
        preserve_committer: false,
        sign: true,
    });
    opts.flatten_leaf_merges = true;
    Ok(opts)
}

/// Move the checked-out branch from `head_oid`, whose commits `upstream_oid` already has
/// ([`FetchedUpstream::landed`]), to `upstream_oid`, as `git reset --keep` does: the index
/// and worktree go from the one tree to the other, keeping uncommitted and untracked changes
/// that the move doesn't touch, and nothing moves when it would overwrite some.
#[allow(clippy::too_many_arguments)]
fn move_landed_branch(
    repo: &Repository,
    refname: &str,
    head_oid: ObjectId,
    upstream_oid: ObjectId,
    label: &str,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> std::result::Result<PullOutcome, PullError> {
    ensure_branch_tip(repo, refname, Some(head_oid))?;
    if repo.load_index()?.has_unmerged_entries() {
        return Err(Error::IndexUnmerged.into());
    }
    let from = repo.odb().commit_tree(&head_oid)?;
    let to = repo.odb().commit_tree(&upstream_oid)?;
    checkout_tree_transition_allow_local_reset(repo, Some(&from), &to, progress, cancel).map_err(
        |e| match e {
            Error::WouldOverwrite {
                reason: OverwriteReason::LocalChanges,
                paths,
                ..
            } => local_changes_would_be_overwritten(paths),
            other => other,
        },
    )?;
    update_checked_out_branch(
        repo,
        refname,
        Some(head_oid),
        upstream_oid,
        &format!("pull: move to {label}, which has the branch's commits"),
    )?;
    crate::state::write_orig_head(repo, head_oid)?;
    Ok(PullOutcome::at_commit(
        PullResult::Landed,
        label,
        repo,
        upstream_oid,
    )?)
}

/// [`Error::WouldOverwrite`] for uncommitted changes to `paths` that a pull would overwrite.
fn local_changes_would_be_overwritten(paths: Vec<String>) -> Error {
    Error::WouldOverwrite {
        reason: OverwriteReason::LocalChanges,
        paths,
        message: "local changes would be overwritten".to_owned(),
    }
}

fn map_history_error(err: HistoryError) -> PullError {
    match err {
        HistoryError::Library(e) => PullError::Repository(*e),
        HistoryError::LocalChangesWouldBeOverwritten { paths } => {
            PullError::Repository(local_changes_would_be_overwritten(paths))
        }
        other => PullError::Repository(Error::Message(other.to_string())),
    }
}

fn map_rebase_session_error(err: RebaseSessionError) -> PullError {
    match err {
        RebaseSessionError::Library(e) => PullError::Repository(e),
        RebaseSessionError::History(e) => map_history_error(e),
        other => PullError::Rebase(other),
    }
}

fn map_integrate_outcome(outcome: IntegrateBranchOutcome, label: &str) -> PullOutcome {
    match outcome {
        IntegrateBranchOutcome::UpToDate => PullOutcome::new(PullResult::UpToDate, label),
        IntegrateBranchOutcome::FastForward { oid, change_id } => PullOutcome {
            oid: Some(oid),
            change_id,
            ..PullOutcome::new(PullResult::FastForward, label)
        },
        IntegrateBranchOutcome::Merged { oid, change_id } => PullOutcome {
            oid: Some(oid),
            change_id,
            ..PullOutcome::new(PullResult::Merged, label)
        },
        IntegrateBranchOutcome::Conflicts { details, .. } => PullOutcome {
            conflicts: Some(details),
            ..PullOutcome::new(PullResult::Conflicts, label)
        },
        IntegrateBranchOutcome::Squashed | IntegrateBranchOutcome::Stopped => {
            unreachable!("pull does not use squash or no-commit merge modes")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::environment::RepositoryOptions;
    use crate::config::ConfigSet;
    use crate::environment::Environment;
    use crate::progress::NeverCancel;
    use crate::remote::fetch::NoProgress;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};
    use tempfile::TempDir;

    fn git_output(dir: &Path, args: &[&str]) -> Output {
        Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("run git")
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = git_output(dir, args);
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn open(dir: &Path) -> Repository {
        Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("open")
    }

    fn commit(dir: &Path, file: &str, contents: &str) -> ObjectId {
        std::fs::write(dir.join(file), contents).expect("write");
        git(dir, &["add", file]);
        git(dir, &["commit", "-qm", contents]);
        ObjectId::from_hex(&git(dir, &["rev-parse", "HEAD"])).expect("oid")
    }

    fn origin_and_clone(tmp: &TempDir) -> (PathBuf, PathBuf) {
        let origin = tmp.path().join("origin");
        std::fs::create_dir_all(&origin).expect("mkdir origin");
        git(&origin, &["init", "-q", "-b", "main"]);
        commit(&origin, "a.txt", "one");
        git(tmp.path(), &["clone", "-q", "origin", "clone"]);
        (origin, tmp.path().join("clone"))
    }

    fn pull_options(repo: &Repository) -> PullOptions {
        PullOptions::for_working_branch(repo, Environment::empty(), None).expect("options")
    }

    fn cfg(pairs: &[(&str, &str)]) -> ConfigSet {
        let mut set = ConfigSet::new();
        for (key, value) in pairs {
            set.add_command_override(key, value).expect("override");
        }
        set
    }

    #[test]
    fn resolve_pull_mode_cli_ff_only_wins() {
        let config = cfg(&[("pull.rebase", "true")]);
        assert_eq!(
            resolve_pull_mode(&config, Some("main"), Some(PullMode::FfOnly)).expect("mode"),
            PullMode::FfOnly
        );
    }

    #[test]
    fn resolve_pull_mode_no_rebase_overrides_rebase_config() {
        let config = cfg(&[("pull.rebase", "true")]);
        assert_eq!(
            resolve_pull_mode(&config, Some("main"), Some(PullMode::Merge)).expect("mode"),
            PullMode::Merge
        );
    }

    #[test]
    fn resolve_pull_mode_rebase_from_config() {
        let config = cfg(&[("pull.rebase", "merges")]);
        assert_eq!(
            resolve_pull_mode(&config, Some("main"), None).expect("mode"),
            PullMode::Rebase
        );
    }

    #[test]
    fn resolve_pull_mode_branch_rebase_overrides_global() {
        let config = cfg(&[("pull.rebase", "false"), ("branch.topic.rebase", "true")]);
        assert_eq!(
            resolve_pull_mode(&config, Some("topic"), None).expect("mode"),
            PullMode::Rebase
        );
    }

    #[test]
    fn resolve_pull_mode_interactive_is_error() {
        let config = cfg(&[("pull.rebase", "interactive")]);
        assert!(matches!(
            resolve_pull_mode(&config, Some("main"), None),
            Err(PullError::RebaseInteractive)
        ));
    }

    #[test]
    fn resolve_pull_mode_ff_only_from_config() {
        let config = cfg(&[("pull.ff", "only")]);
        assert_eq!(
            resolve_pull_mode(&config, Some("main"), None).expect("mode"),
            PullMode::FfOnly
        );
    }

    #[test]
    fn resolve_pull_mode_ff_only_beats_rebase_config() {
        let config = cfg(&[("pull.ff", "only"), ("pull.rebase", "true")]);
        assert_eq!(
            resolve_pull_mode(&config, Some("main"), None).expect("mode"),
            PullMode::FfOnly
        );
    }

    #[test]
    fn resolve_pull_mode_defaults_to_merge() {
        let config = ConfigSet::new();
        assert_eq!(
            resolve_pull_mode(&config, Some("main"), None).expect("mode"),
            PullMode::Merge
        );
    }

    #[test]
    fn resolve_pull_integration_ff_false_disables_fast_forward() {
        let config = cfg(&[("pull.ff", "false")]);
        let resolved = resolve_pull_integration(&config, Some("main"), None).expect("resolved");
        assert_eq!(resolved.mode, PullMode::Merge);
        assert!(!resolved.allow_fast_forward);
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn pull_maps_custom_fetch_refspec() {
        let tmp = TempDir::new().expect("tempdir");
        let (origin, clone) = origin_and_clone(&tmp);
        let tip = commit(&origin, "b.txt", "two");
        git(
            &clone,
            &[
                "config",
                "--replace-all",
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/mirror/*",
            ],
        );

        let repo = open(&clone);
        let outcome = pull(
            &repo,
            &pull_options(&repo),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("pull succeeds");
        assert_eq!(outcome.result, PullResult::FastForward);
        assert_eq!(outcome.oid, Some(tip));
        assert_eq!(git(&clone, &["rev-parse", "main"]), tip.to_hex());
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn pull_local_upstream_skips_fetch() {
        let tmp = TempDir::new().expect("tempdir");
        let dir = tmp.path();
        git(dir, &["init", "-q", "-b", "main"]);
        commit(dir, "a.txt", "one");
        git(dir, &["checkout", "-q", "-b", "dev"]);
        let tip = commit(dir, "b.txt", "two");
        git(dir, &["checkout", "-q", "main"]);
        git(dir, &["branch", "--set-upstream-to", "dev", "main"]);

        let repo = open(dir);
        let outcome = pull(
            &repo,
            &pull_options(&repo),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("local pull succeeds");
        assert_eq!(outcome.result, PullResult::FastForward);
        assert_eq!(outcome.oid, Some(tip));
        assert_eq!(git(dir, &["rev-parse", "main"]), tip.to_hex());
        assert_eq!(outcome.transfer, TransferStats::default());
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn pull_reports_objects_received_by_its_fetch() {
        let tmp = TempDir::new().expect("tempdir");
        let (origin, clone) = origin_and_clone(&tmp);
        commit(&origin, "b.txt", "two");

        let repo = open(&clone);
        let outcome = pull(
            &repo,
            &pull_options(&repo),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("pull succeeds");
        assert_eq!(outcome.result, PullResult::FastForward);
        // One new commit, its root tree and the added blob.
        assert_eq!(outcome.transfer.objects, 3, "{:?}", outcome.transfer);
        assert!(outcome.transfer.bytes > 0, "{:?}", outcome.transfer);

        let again = pull(
            &repo,
            &pull_options(&repo),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("second pull");
        assert_eq!(again.result, PullResult::UpToDate);
        assert_eq!(again.transfer.objects, 0, "{:?}", again.transfer);
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn pull_adopts_upstream_on_unborn_branch_with_reflog() {
        let tmp = TempDir::new().expect("tempdir");
        let origin = tmp.path().join("origin");
        std::fs::create_dir_all(&origin).expect("mkdir origin");
        git(&origin, &["init", "-q", "-b", "main"]);
        let tip = commit(&origin, "a.txt", "one");
        let local = tmp.path().join("local");
        std::fs::create_dir_all(&local).expect("mkdir local");
        git(&local, &["init", "-q", "-b", "main"]);
        git(&local, &["remote", "add", "origin", "../origin"]);

        let repo = open(&local);
        let outcome = pull(
            &repo,
            &pull_options(&repo),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("pull succeeds");
        assert_eq!(outcome.result, PullResult::SetUpstream);
        assert_eq!(outcome.oid, Some(tip));
        assert_eq!(
            git(&local, &["reflog", "show", "--format=%H %gs", "-1", "main"]),
            format!("{} {INITIAL_PULL_REFLOG}", tip.to_hex())
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn pull_integrates_a_fetched_upstream_without_fetching() {
        let tmp = TempDir::new().expect("tempdir");
        let (_origin, clone) = origin_and_clone(&tmp);
        git(&clone, &["checkout", "-q", "-b", "up"]);
        let tip = commit(&clone, "b.txt", "two");
        git(&clone, &["checkout", "-q", "main"]);
        // A fetch from this remote would fail.
        git(
            &clone,
            &["remote", "set-url", "origin", "/nonexistent/remote"],
        );

        let repo = open(&clone);
        let mut options = pull_options(&repo);
        options.upstream = Some(FetchedUpstream {
            label: "origin/up".to_owned(),
            oid: tip,
            landed: None,
            copies: Vec::new(),
        });
        let outcome = pull(&repo, &options, &mut NoProgress, &NeverCancel, None)
            .expect("pull without a fetch");
        assert_eq!(outcome.result, PullResult::FastForward);
        assert_eq!(outcome.label, "origin/up");
        assert_eq!(git(&clone, &["rev-parse", "main"]), tip.to_hex());
        assert_eq!(outcome.transfer, TransferStats::default());
    }

    /// An origin and a clone whose `main` each changed `a.txt` differently.
    fn conflicting_clone(tmp: &TempDir) -> (PathBuf, ObjectId, ObjectId) {
        let (origin, clone) = origin_and_clone(tmp);
        let upstream = commit(&origin, "a.txt", "theirs");
        let local = commit(&clone, "a.txt", "ours");
        git(&clone, &["config", "user.name", "T"]);
        git(&clone, &["config", "user.email", "t@example.com"]);
        git(&clone, &["fetch", "-q"]);
        (clone, local, upstream)
    }

    fn rebase_options(repo: &Repository, upstream: ObjectId, stop: bool) -> PullOptions {
        let mut options = pull_options(repo);
        options.mode = Some(PullMode::Rebase);
        options.stop_on_conflict = stop;
        options.upstream = Some(FetchedUpstream {
            label: "origin/main".to_owned(),
            oid: upstream,
            landed: None,
            copies: Vec::new(),
        });
        options
    }

    fn abort(repo: &Repository) {
        let identity = "T <t@example.com> 0 +0000".parse().expect("signature");
        crate::history::abort_rebase(repo, &identity).expect("abort");
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn a_conflicting_rebase_pull_stops_in_a_resumable_rebase() {
        let tmp = TempDir::new().expect("tempdir");
        let (clone, local, upstream) = conflicting_clone(&tmp);
        let repo = open(&clone);

        // Any rebase pull stops; `stop_on_conflict` only changes what it is called.
        let plain = pull(
            &repo,
            &rebase_options(&repo, upstream, false),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("pull");
        assert_eq!(plain.result, PullResult::RebaseConflicts);
        assert!(clone.join(".git/rebase-merge").exists());
        abort(&repo);
        assert_eq!(git(&clone, &["rev-parse", "HEAD"]), local.to_hex());

        let stopped = pull(
            &repo,
            &rebase_options(&repo, upstream, true),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("pull");
        assert_eq!(stopped.result, PullResult::RebaseStopped);
        assert_eq!(stopped.oid, Some(local));
        let paths: Vec<&str> = stopped
            .conflicts
            .iter()
            .flatten()
            .map(|c| c.path.as_str())
            .collect();
        assert_eq!(paths, ["a.txt"]);
        // The rebase is left for `rebase --continue`; the branch keeps its tip until then.
        assert!(clone.join(".git/rebase-merge/grit-plan").is_file());
        assert_eq!(git(&clone, &["rev-parse", "main"]), local.to_hex());
        assert_eq!(git(&clone, &["rev-parse", "HEAD"]), upstream.to_hex());
        assert!(std::fs::read_to_string(clone.join("a.txt"))
            .expect("conflicted file")
            .contains("<<<<<<<"));
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn a_rebase_pull_with_local_changes_is_refused() {
        let tmp = TempDir::new().expect("tempdir");
        let (clone, _, upstream) = conflicting_clone(&tmp);
        let local = commit(&clone, "b.txt", "tracked");
        std::fs::write(clone.join("b.txt"), "edited, not committed").expect("write");
        let repo = open(&clone);
        let refused = pull(
            &repo,
            &rebase_options(&repo, upstream, true),
            &mut NoProgress,
            &NeverCancel,
            None,
        );
        assert!(
            matches!(
                refused,
                Err(PullError::Repository(Error::DirtyWorktree { .. }))
            ),
            "{refused:?}"
        );
        assert!(!clone.join(".git/rebase-merge").exists());
        assert_eq!(git(&clone, &["rev-parse", "HEAD"]), local.to_hex());
        assert_eq!(
            std::fs::read_to_string(clone.join("b.txt")).expect("read"),
            "edited, not committed"
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn reflog_failure_after_ref_update_is_a_warning() {
        let tmp = TempDir::new().expect("tempdir");
        let (origin, clone) = origin_and_clone(&tmp);
        let tip = commit(&origin, "b.txt", "two");
        let branch_log = clone.join(".git/logs/refs/heads/main");
        std::fs::remove_file(&branch_log).expect("remove branch reflog");
        std::fs::create_dir(&branch_log).expect("replace reflog with directory");

        let mut repo = open(&clone);
        let sink = std::sync::Arc::new(crate::diagnostics::CollectingDiagnostics::new());
        repo.set_diagnostics(sink.clone());
        let outcome = pull(
            &repo,
            &pull_options(&repo),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("pull completes");
        assert_eq!(outcome.result, PullResult::FastForward);
        assert_eq!(outcome.oid, Some(tip));
        assert_eq!(git(&clone, &["rev-parse", "main"]), tip.to_hex());
        assert!(sink.warnings().iter().any(|warning| matches!(
            warning,
            crate::diagnostics::Warning::ReflogNotWritten { refname, .. }
                if refname == "refs/heads/main"
        )));
    }

    /// A clone whose `main` has three commits appending to `a.txt`, and an upstream commit
    /// (`up`, on the clone's starting commit) holding their combined result, as a server's
    /// snapshot of a merge of them would. Returns the clone, its tip and the upstream.
    fn landed_clone(tmp: &TempDir) -> (PathBuf, ObjectId, ObjectId) {
        let (_origin, clone) = origin_and_clone(tmp);
        git(&clone, &["config", "user.name", "T"]);
        git(&clone, &["config", "user.email", "t@example.com"]);
        std::fs::write(clone.join("b.txt"), "b\n").expect("write");
        git(&clone, &["add", "b.txt"]);
        git(&clone, &["commit", "-qm", "b"]);
        let base = git(&clone, &["rev-parse", "HEAD"]);
        let mut text = "one".to_owned();
        for line in ["1", "2", "3"] {
            text = format!("{text}\n{line}");
            commit(&clone, "a.txt", &text);
        }
        let tip = ObjectId::from_hex(&git(&clone, &["rev-parse", "HEAD"])).expect("oid");
        git(&clone, &["switch", "-q", "-c", "up", &base]);
        let upstream = commit(&clone, "a.txt", &text);
        git(&clone, &["switch", "-q", "main"]);
        (clone, tip, upstream)
    }

    fn landed_options(
        repo: &Repository,
        upstream: ObjectId,
        landed: ObjectId,
        stop: bool,
    ) -> PullOptions {
        let mut options = rebase_options(repo, upstream, stop);
        if let Some(fetched) = options.upstream.as_mut() {
            fetched.landed = Some(landed);
        }
        options
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn a_landed_branch_moves_to_the_upstream_and_keeps_local_changes() {
        let tmp = TempDir::new().expect("tempdir");
        let (clone, tip, upstream) = landed_clone(&tmp);
        let repo = open(&clone);
        // Replayed one by one, the first commit conflicts with the upstream that has all three.
        let replayed = pull(
            &repo,
            &rebase_options(&repo, upstream, false),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("pull");
        assert_eq!(replayed.result, PullResult::RebaseConflicts);
        abort(&repo);
        assert_eq!(git(&clone, &["rev-parse", "main"]), tip.to_hex());

        std::fs::write(clone.join("b.txt"), "b, edited\n").expect("write");
        std::fs::write(clone.join("draft.txt"), "not committed\n").expect("write");
        let mut check = landed_options(&repo, upstream, tip, false);
        check.check = true;
        let predicted = pull(&repo, &check, &mut NoProgress, &NeverCancel, None).expect("check");
        assert_eq!(predicted.check.map(|c| c.would), Some(PullResult::Landed));

        git(&clone, &["update-ref", "-d", "ORIG_HEAD"]);
        let landed = pull(
            &repo,
            &landed_options(&repo, upstream, tip, true),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("pull");
        assert_eq!(landed.result, PullResult::Landed);
        assert_eq!(landed.oid, Some(upstream));
        assert_eq!(git(&clone, &["rev-parse", "main"]), upstream.to_hex());
        assert_eq!(git(&clone, &["rev-parse", "ORIG_HEAD"]), tip.to_hex());
        assert_eq!(
            git(&clone, &["status", "--porcelain"]),
            "M b.txt\n?? draft.txt"
        );
        assert_eq!(
            git(&clone, &["reflog", "-1", "--format=%gs", "main"]),
            "pull: move to origin/main, which has the branch's commits"
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn a_landed_branch_does_not_move_over_local_changes_the_upstream_has_other_content_for() {
        let tmp = TempDir::new().expect("tempdir");
        let (clone, tip, _) = landed_clone(&tmp);
        // The upstream has the branch's commits and a change of its own to b.txt.
        git(&clone, &["switch", "-q", "up"]);
        let upstream = commit(&clone, "b.txt", "b, upstream");
        git(&clone, &["switch", "-q", "main"]);
        std::fs::write(clone.join("b.txt"), "b, edited\n").expect("write");
        let repo = open(&clone);
        let err = pull(
            &repo,
            &landed_options(&repo, upstream, tip, true),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect_err("blocked");
        assert!(
            matches!(
                &err,
                PullError::Repository(Error::WouldOverwrite { paths, .. }) if paths == &["b.txt"]
            ),
            "{err:?}"
        );
        assert_eq!(git(&clone, &["rev-parse", "main"]), tip.to_hex());
        assert_eq!(git(&clone, &["status", "--porcelain"]), "M b.txt");
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn commits_above_a_landed_one_are_rebased_without_it() {
        let tmp = TempDir::new().expect("tempdir");
        let (clone, landed, upstream) = landed_clone(&tmp);
        let own = commit(&clone, "c.txt", "c");
        let repo = open(&clone);
        let outcome = pull(
            &repo,
            &landed_options(&repo, upstream, landed, true),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("pull");
        assert_eq!(outcome.result, PullResult::Rebased);
        assert_eq!(git(&clone, &["rev-parse", "main^"]), upstream.to_hex());
        assert_eq!(git(&clone, &["log", "-1", "--format=%s", "main"]), "c");
        assert_ne!(git(&clone, &["rev-parse", "main"]), own.to_hex());

        // One that conflicts stops on its own commit, not on the landed ones.
        let tmp = TempDir::new().expect("tempdir");
        let (clone, landed, _) = landed_clone(&tmp);
        commit(&clone, "b.txt", "b, ours");
        git(&clone, &["switch", "-q", "up"]);
        let upstream = commit(&clone, "b.txt", "b, theirs");
        git(&clone, &["switch", "-q", "main"]);
        let repo = open(&clone);
        let stopped = pull(
            &repo,
            &landed_options(&repo, upstream, landed, true),
            &mut NoProgress,
            &NeverCancel,
            None,
        )
        .expect("pull");
        assert_eq!(stopped.result, PullResult::RebaseStopped);
        let paths: Vec<&str> = stopped
            .conflicts
            .iter()
            .flatten()
            .map(|c| c.path.as_str())
            .collect();
        assert_eq!(paths, ["b.txt"]);
        // Nothing was replayed before it: the session stopped right on the upstream.
        assert_eq!(git(&clone, &["rev-parse", "HEAD"]), upstream.to_hex());
    }
}
