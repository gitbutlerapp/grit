//! Fetch from a remote and integrate the configured upstream into the current branch.
//!
//! [`pull`] runs fetch, resolves the branch upstream via [`upstream_branch`], then
//! fast-forwards, merges, or refuses when `--ff-only` / `pull.ff=only` forbids a
//! merge commit. Rebase pulls return [`PullError::RebaseUnsupported`] until the
//! history engine wires `pull.rebase`.

use crate::branch_tracking::upstream_branch;
use crate::change_id::{resolved_change_id, ChangeId, ChangeIdPolicy};
use crate::config::parse_git_config_maybe_bool;
use crate::config::ConfigSet;
use crate::error::{Error, Result};
use crate::merge_base::{count_symmetric_ahead_behind, is_ancestor};
use crate::objects::{parse_commit, ObjectId};
use crate::porcelain::checkout::checkout_between_trees;
use crate::porcelain::merge::{integrate_branch, IntegrateBranchOutcome, MergeConflictDetail};
use crate::porcelain::worktree_guard::prepare_tree_checkout;
use crate::refs;
use crate::remote::fetch::Progress;
use crate::remote::transfer::FetchOptions;
use crate::remote::{HttpClientFactory, Remote, RemoteError, DEFAULT_REMOTE};
use crate::repo::Repository;
use crate::state::{resolve_head, HeadState};
use thiserror::Error;

/// How [`pull`] integrates upstream commits when a fast-forward is not possible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PullMode {
    /// Refuse when diverged history would require a merge commit.
    FfOnly,
    /// Merge (or fast-forward when possible).
    Merge,
    /// Rebase local commits onto the upstream tip (not implemented yet).
    Rebase,
}

/// Options for [`pull`].
#[derive(Clone, Default)]
pub struct PullOptions {
    /// Remote name override; when `None`, uses `branch.<name>.remote` or [`DEFAULT_REMOTE`].
    pub remote: Option<String>,
    /// Local branch to update; when `None`, uses the current branch.
    pub branch: Option<String>,
    /// CLI override for integration mode; when `None`, [`resolve_pull_mode`] applies config.
    pub mode: Option<PullMode>,
    /// Fetch options (tags, prune, depth, environment, …).
    pub fetch: FetchOptions,
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
    /// Conflicted paths when [`Self::result`] is [`PullResult::Conflicts`].
    pub conflicts: Option<Vec<MergeConflictDetail>>,
    /// Commits on the branch not reachable from upstream when refused as not ff.
    pub ahead: Option<usize>,
    /// Commits on upstream not reachable from the branch when refused as not ff.
    pub behind: Option<usize>,
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
    /// `pull.rebase` was selected but rebase pull is not implemented yet.
    #[error("pull with rebase is not supported yet")]
    RebaseUnsupported,
    /// Remote resolution or transport failure.
    #[error(transparent)]
    Remote(#[from] RemoteError),
    /// Other repository failures.
    #[error(transparent)]
    Repository(#[from] Error),
}

/// Resolve pull integration mode from config with optional CLI override.
///
/// Precedence matches Git: explicit `--ff-only` / `--no-rebase`, then `pull.rebase`,
/// then `pull.ff=only`, otherwise merge (fast-forward when possible).
#[must_use]
pub fn resolve_pull_mode(config: &ConfigSet, cli: Option<PullMode>) -> PullMode {
    if cli == Some(PullMode::FfOnly) {
        return PullMode::FfOnly;
    }
    if cli == Some(PullMode::Merge) {
        return PullMode::Merge;
    }
    if cli == Some(PullMode::Rebase) {
        return PullMode::Rebase;
    }
    if pull_rebase_enabled(config) {
        return PullMode::Rebase;
    }
    if config
        .get("pull.ff")
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("only"))
    {
        return PullMode::FfOnly;
    }
    PullMode::Merge
}

fn pull_rebase_enabled(config: &ConfigSet) -> bool {
    let Some(raw) = config.get("pull.rebase") else {
        return false;
    };
    let value = raw.trim();
    if value.eq_ignore_ascii_case("false") || value == "0" {
        return false;
    }
    if let Some(parsed) = parse_git_config_maybe_bool(value) {
        return parsed;
    }
    true
}

/// Fetch from a remote and integrate its upstream into the current branch.
///
/// # Errors
///
/// Returns [`PullError`] for detached HEAD, missing upstream, unsupported rebase,
/// or transport/repository failures.
pub fn pull<P>(
    repo: &Repository,
    options: &PullOptions,
    progress: &mut P,
    http_factory: Option<&dyn HttpClientFactory>,
) -> std::result::Result<PullOutcome, PullError>
where
    P: Progress,
{
    let (refname, short_name, head_oid) = match resolve_head(repo.git_dir())? {
        HeadState::Branch {
            refname,
            short_name,
            oid,
        } => (refname, short_name, oid),
        HeadState::Detached { .. } => return Err(PullError::DetachedHead),
        HeadState::Invalid => return Err(PullError::InvalidHead),
    };

    if let Some(requested) = options.branch.as_deref() {
        if requested != short_name {
            return Err(PullError::Repository(Error::Message(format!(
                "pull for branch '{requested}' requires checking out that branch first"
            ))));
        }
    }

    let config = repo.config().map_err(PullError::Repository)?;
    let remote_name = options
        .remote
        .clone()
        .or_else(|| {
            config
                .get(&format!("branch.{short_name}.remote"))
                .map(|r| r.trim().to_owned())
                .filter(|r| !r.is_empty())
        })
        .unwrap_or_else(|| DEFAULT_REMOTE.to_owned());

    let mode = resolve_pull_mode(config.as_ref(), options.mode);

    let mut fetch_opts = options.fetch.clone();
    if fetch_opts.remote_name.is_none() {
        fetch_opts.remote_name = Some(remote_name.clone());
    }

    let remote = Remote::from_config(config.as_ref(), &remote_name)?;
    remote
        .fetch(repo, fetch_opts, progress, http_factory)
        .map_err(PullError::Remote)?;

    let upstream = upstream_branch(repo, &short_name).ok_or(PullError::NoUpstream {
        branch: short_name.clone(),
    })?;
    let label = upstream.name.clone();
    let upstream_oid = upstream.oid;

    match head_oid {
        None => adopt_unborn_upstream(repo, &refname, &short_name, &label, upstream_oid),
        Some(head_oid) => {
            integrate_fetched_upstream(repo, &refname, head_oid, upstream_oid, &label, mode)
        }
    }
}

fn adopt_unborn_upstream(
    repo: &Repository,
    refname: &str,
    short_name: &str,
    label: &str,
    upstream_oid: ObjectId,
) -> std::result::Result<PullOutcome, PullError> {
    let upstream_tree = commit_tree(repo, &upstream_oid)?;
    prepare_tree_checkout(repo, None, &upstream_tree).map_err(PullError::Repository)?;
    checkout_between_trees(repo, None, &upstream_tree).map_err(PullError::Repository)?;
    refs::write_ref(repo.git_dir(), refname, &upstream_oid).map_err(PullError::Repository)?;
    Ok(PullOutcome {
        result: PullResult::SetUpstream,
        label: short_name.to_owned(),
        oid: Some(upstream_oid),
        change_id: logical_change_id(repo, upstream_oid),
        upstream: Some(label.to_owned()),
        conflicts: None,
        ahead: None,
        behind: None,
    })
}

fn integrate_fetched_upstream(
    repo: &Repository,
    refname: &str,
    head_oid: ObjectId,
    upstream_oid: ObjectId,
    label: &str,
    mode: PullMode,
) -> std::result::Result<PullOutcome, PullError> {
    let diverged = !is_ancestor(repo, upstream_oid, head_oid).map_err(PullError::Repository)?
        && !is_ancestor(repo, head_oid, upstream_oid).map_err(PullError::Repository)?
        && head_oid != upstream_oid;

    if diverged {
        match mode {
            PullMode::FfOnly => {
                let (ahead, behind) = count_symmetric_ahead_behind(repo, head_oid, upstream_oid)
                    .map_err(PullError::Repository)?;
                return Ok(PullOutcome {
                    result: PullResult::RefusedNotFf,
                    label: label.to_owned(),
                    oid: Some(head_oid),
                    change_id: logical_change_id(repo, head_oid),
                    upstream: None,
                    conflicts: None,
                    ahead: Some(ahead),
                    behind: Some(behind),
                });
            }
            PullMode::Rebase => return Err(PullError::RebaseUnsupported),
            PullMode::Merge => {}
        }
    } else if mode == PullMode::Rebase && head_oid != upstream_oid {
        // Fast-forward path while `pull.rebase` is set: same result as merge until rebase lands.
    }

    let integrate = integrate_branch(repo, refname, head_oid, upstream_oid, label)
        .map_err(PullError::Repository)?;
    Ok(map_integrate_outcome(integrate, label))
}

fn map_integrate_outcome(outcome: IntegrateBranchOutcome, label: &str) -> PullOutcome {
    match outcome {
        IntegrateBranchOutcome::UpToDate => PullOutcome {
            result: PullResult::UpToDate,
            label: label.to_owned(),
            oid: None,
            change_id: None,
            upstream: None,
            conflicts: None,
            ahead: None,
            behind: None,
        },
        IntegrateBranchOutcome::FastForward { oid, change_id } => PullOutcome {
            result: PullResult::FastForward,
            label: label.to_owned(),
            oid: Some(oid),
            change_id,
            upstream: None,
            conflicts: None,
            ahead: None,
            behind: None,
        },
        IntegrateBranchOutcome::Merged { oid, change_id } => PullOutcome {
            result: PullResult::Merged,
            label: label.to_owned(),
            oid: Some(oid),
            change_id,
            upstream: None,
            conflicts: None,
            ahead: None,
            behind: None,
        },
        IntegrateBranchOutcome::Conflicts { details, .. } => PullOutcome {
            result: PullResult::Conflicts,
            label: label.to_owned(),
            oid: None,
            change_id: None,
            upstream: None,
            conflicts: Some(details),
            ahead: None,
            behind: None,
        },
    }
}

fn commit_tree(repo: &Repository, oid: &ObjectId) -> Result<ObjectId> {
    let obj = repo.odb().read(oid)?;
    let commit = parse_commit(&obj.data)?;
    Ok(commit.tree)
}

fn logical_change_id(repo: &Repository, oid: ObjectId) -> Option<ChangeId> {
    let obj = repo.odb().read(&oid).ok()?;
    let data = parse_commit(&obj.data).ok()?;
    let config = repo.config().ok()?;
    if ChangeIdPolicy::from_config(config.as_ref()).enabled() {
        Some(resolved_change_id(&oid, &data).id)
    } else {
        crate::change_id::change_id(&data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigSet;

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
            resolve_pull_mode(&config, Some(PullMode::FfOnly)),
            PullMode::FfOnly
        );
    }

    #[test]
    fn resolve_pull_mode_no_rebase_overrides_rebase_config() {
        let config = cfg(&[("pull.rebase", "true")]);
        assert_eq!(
            resolve_pull_mode(&config, Some(PullMode::Merge)),
            PullMode::Merge
        );
    }

    #[test]
    fn resolve_pull_mode_rebase_from_config() {
        let config = cfg(&[("pull.rebase", "merges")]);
        assert_eq!(resolve_pull_mode(&config, None), PullMode::Rebase);
    }

    #[test]
    fn resolve_pull_mode_ff_only_from_config() {
        let config = cfg(&[("pull.ff", "only")]);
        assert_eq!(resolve_pull_mode(&config, None), PullMode::FfOnly);
    }

    #[test]
    fn resolve_pull_mode_defaults_to_merge() {
        let config = ConfigSet::new();
        assert_eq!(resolve_pull_mode(&config, None), PullMode::Merge);
    }
}
