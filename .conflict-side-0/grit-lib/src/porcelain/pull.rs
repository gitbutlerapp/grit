//! Fetch from a remote and integrate the configured upstream into the current branch.
//!
//! [`pull`] runs fetch, resolves the branch upstream via [`pull_upstream_branch`], then
//! fast-forwards, merges, rebases local commits onto the upstream tip, or refuses
//! when `--ff-only` / `pull.ff=only` forbids a merge commit. Rebase conflicts abort
//! atomically (no sequencer state); stop/continue rebase is a follow-up once the
//! shared history conflict model lands.

use crate::branch_tracking::pull_upstream_branch;
use crate::change_id::{logical_change_id_for_commit, ChangeId};
use crate::config::environment::Environment;
use crate::config::ident_resolve::{resolve_identity_with, IdentRole};
use crate::config::parse_git_config_maybe_bool;
use crate::config::ConfigSet;
use crate::error::{Error, Result};
use crate::merge_base::{count_symmetric_ahead_behind, is_ancestor, merge_bases_first_vs_rest};
use crate::objects::{parse_commit, ObjectId};
use crate::porcelain::checkout::checkout_between_trees;
use crate::porcelain::checkout_reflog::checkout_reflog_identity;
use crate::porcelain::merge::{
    ensure_branch_tip, integrate_branch_with_options, update_checked_out_branch,
    IntegrateBranchOptions, IntegrateBranchOutcome, MergeConflictDetail, MergeConflictKind,
};
use crate::porcelain::worktree_guard::prepare_tree_checkout;
use crate::remote::fetch::Progress;
use crate::remote::transfer::{resolve_fetch_prune, FetchOptions, TagMode};
use crate::remote::{HttpClientFactory, Remote, RemoteError, DEFAULT_REMOTE};
use crate::repo::Repository;
use crate::revwalk::history::{
    apply_rewrite, rebase_onto, ApplyOptions, EmptyPolicy, HistoryError, RebaseOntoOptions,
    RefScope, RewriteOptions,
};
use crate::state::{resolve_head, HeadState};
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
    /// Rebase local commits onto the upstream tip via the history engine.
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
    /// Local commits were replayed onto the upstream tip.
    Rebased,
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
    /// `pull.rebase=interactive` (or branch rebase) is not supported.
    #[error("interactive rebase is not supported; use grit history instead")]
    RebaseInteractive,
    /// Remote resolution or transport failure.
    #[error(transparent)]
    Remote(#[from] RemoteError),
    /// Other repository failures.
    #[error(transparent)]
    Repository(#[from] Error),
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
        let short_name = match resolve_head(repo.git_dir()).map_err(PullError::Repository)? {
            HeadState::Branch { short_name, .. } => short_name,
            HeadState::Detached { .. } => return Err(PullError::DetachedHead),
            HeadState::Invalid => return Err(PullError::InvalidHead),
        };
        let config = repo.config().map_err(PullError::Repository)?;
        let remote_name = config
            .get(&format!("branch.{short_name}.remote"))
            .map(|r| r.trim().to_owned())
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| DEFAULT_REMOTE.to_owned());
        let prune = resolve_fetch_prune(config.as_ref(), &remote_name, None)
            .map_err(PullError::Repository)?;
        Ok(Self {
            remote: None,
            branch: None,
            mode: cli_mode,
            fetch: FetchOptions {
                tags: TagMode::Following,
                thread_policy: repo.thread_policy(),
                environment,
                prune,
                remote_name: Some(remote_name),
                ..Default::default()
            },
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
/// # Errors
///
/// Returns [`PullError`] for detached HEAD, missing upstream, interactive rebase config,
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

    let integration = resolve_pull_integration(config.as_ref(), Some(&short_name), options.mode)?;

    let mut fetch_opts = options.fetch.clone();
    if fetch_opts.remote_name.is_none() {
        fetch_opts.remote_name = Some(remote_name.clone());
    }

    if remote_name != "." {
        let remote = Remote::from_config(config.as_ref(), &remote_name)?;
        remote
            .fetch(repo, fetch_opts, progress, http_factory)
            .map_err(PullError::Remote)?;
    }

    let upstream =
        pull_upstream_branch(repo, &short_name, &remote_name).ok_or(PullError::NoUpstream {
            branch: short_name.clone(),
        })?;
    let label = upstream.name.clone();
    let upstream_oid = upstream.oid;

    match head_oid {
        None => adopt_unborn_upstream(repo, &refname, &short_name, &label, upstream_oid),
        Some(head_oid) => {
            integrate_fetched_upstream(repo, &refname, head_oid, upstream_oid, &label, integration)
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
    ensure_branch_tip(repo, refname, None)?;
    let upstream_tree = commit_tree(repo, &upstream_oid)?;
    prepare_tree_checkout(repo, None, &upstream_tree).map_err(PullError::Repository)?;
    checkout_between_trees(repo, None, &upstream_tree).map_err(PullError::Repository)?;
    update_checked_out_branch(repo, refname, None, upstream_oid, INITIAL_PULL_REFLOG)?;
    Ok(PullOutcome {
        result: PullResult::SetUpstream,
        label: short_name.to_owned(),
        oid: Some(upstream_oid),
        change_id: logical_change_id_for_commit(repo, upstream_oid)?,
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
    integration: PullIntegration,
) -> std::result::Result<PullOutcome, PullError> {
    let diverged = !is_ancestor(repo, upstream_oid, head_oid).map_err(PullError::Repository)?
        && !is_ancestor(repo, head_oid, upstream_oid).map_err(PullError::Repository)?
        && head_oid != upstream_oid;

    if diverged {
        match integration.mode {
            PullMode::FfOnly => {
                let (ahead, behind) = count_symmetric_ahead_behind(repo, head_oid, upstream_oid)
                    .map_err(PullError::Repository)?;
                return Ok(PullOutcome {
                    result: PullResult::RefusedNotFf,
                    label: label.to_owned(),
                    oid: Some(head_oid),
                    change_id: logical_change_id_for_commit(repo, head_oid)?,
                    upstream: None,
                    conflicts: None,
                    ahead: Some(ahead),
                    behind: Some(behind),
                });
            }
            PullMode::Rebase => {
                return rebase_fetched_upstream(repo, refname, head_oid, upstream_oid, label);
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
    )
    .map_err(PullError::Repository)?;
    Ok(map_integrate_outcome(integrate, label))
}

fn rebase_fetched_upstream(
    repo: &Repository,
    refname: &str,
    head_oid: ObjectId,
    upstream_oid: ObjectId,
    label: &str,
) -> std::result::Result<PullOutcome, PullError> {
    let fork = merge_bases_first_vs_rest(repo, head_oid, &[upstream_oid])
        .map_err(PullError::Repository)?
        .into_iter()
        .next()
        .ok_or_else(|| {
            PullError::Repository(Error::Message(format!(
                "'{label}' has no common history with the current branch"
            )))
        })?;

    let config = repo.config().map_err(PullError::Repository)?;
    let committer = resolve_identity_with(
        repo.environment(),
        config.as_ref(),
        IdentRole::Committer,
        repo.identity_now(),
    )
    .map_err(|e| PullError::Repository(Error::Message(e.to_string())))?;

    let mut rebase_opts = RebaseOntoOptions::new(RewriteOptions {
        scope: RefScope::Head,
        empty: EmptyPolicy::Drop,
        committer,
        preserve_committer: false,
        sign: true,
    });
    rebase_opts.flatten_leaf_merges = true;

    let plan = match rebase_onto(repo, fork, upstream_oid, &[head_oid], &rebase_opts) {
        Ok(plan) => plan,
        Err(HistoryError::Conflict { paths, .. }) => {
            let details: Vec<MergeConflictDetail> = paths
                .into_iter()
                .map(|path| MergeConflictDetail {
                    path,
                    kind: MergeConflictKind::Content,
                })
                .collect();
            return Ok(PullOutcome {
                result: PullResult::Conflicts,
                label: label.to_owned(),
                oid: Some(head_oid),
                change_id: logical_change_id_for_commit(repo, head_oid)?,
                upstream: None,
                conflicts: Some(details),
                ahead: None,
                behind: None,
            });
        }
        Err(other) => return Err(map_history_error(other)),
    };

    if plan.head.old_commit == Some(head_oid) && plan.head.commit_changes {
        let applied = apply_rewrite(
            repo,
            &plan,
            &ApplyOptions {
                identity: checkout_reflog_identity(repo),
                reflog_message: format!("pull: rebase onto {label}"),
            },
        )
        .map_err(map_history_error)?;
        return Ok(PullOutcome {
            result: PullResult::Rebased,
            label: label.to_owned(),
            oid: Some(applied.head),
            change_id: logical_change_id_for_commit(repo, applied.head)?,
            upstream: None,
            conflicts: None,
            ahead: None,
            behind: None,
        });
    }

    // Skipped every local patch or nothing to rewrite: match `git pull --rebase` by
    // fast-forwarding when the branch is simply behind the upstream tip.
    if is_ancestor(repo, head_oid, upstream_oid).map_err(PullError::Repository)?
        && head_oid != upstream_oid
    {
        let integrate = integrate_branch_with_options(
            repo,
            refname,
            head_oid,
            upstream_oid,
            label,
            IntegrateBranchOptions {
                allow_fast_forward: true,
            },
        )
        .map_err(PullError::Repository)?;
        return Ok(map_integrate_outcome(integrate, label));
    }

    let integrate = integrate_branch_with_options(
        repo,
        refname,
        head_oid,
        upstream_oid,
        label,
        IntegrateBranchOptions {
            allow_fast_forward: true,
        },
    )
    .map_err(PullError::Repository)?;
    Ok(map_integrate_outcome(integrate, label))
}

fn map_history_error(err: HistoryError) -> PullError {
    match err {
        HistoryError::Library(e) => PullError::Repository(*e),
        other => PullError::Repository(Error::Message(other.to_string())),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::environment::RepositoryOptions;
    use crate::config::ConfigSet;
    use crate::environment::Environment;
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
        let outcome =
            pull(&repo, &pull_options(&repo), &mut NoProgress, None).expect("pull succeeds");
        assert_eq!(outcome.result, PullResult::FastForward);
        assert_eq!(outcome.oid, Some(tip));
        assert_eq!(git(&clone, &["rev-parse", "main"]), tip.to_hex());
    }

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
        let outcome =
            pull(&repo, &pull_options(&repo), &mut NoProgress, None).expect("local pull succeeds");
        assert_eq!(outcome.result, PullResult::FastForward);
        assert_eq!(outcome.oid, Some(tip));
        assert_eq!(git(dir, &["rev-parse", "main"]), tip.to_hex());
    }

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
        let outcome =
            pull(&repo, &pull_options(&repo), &mut NoProgress, None).expect("pull succeeds");
        assert_eq!(outcome.result, PullResult::SetUpstream);
        assert_eq!(outcome.oid, Some(tip));
        assert_eq!(
            git(&local, &["reflog", "show", "--format=%H %gs", "-1", "main"]),
            format!("{} {INITIAL_PULL_REFLOG}", tip.to_hex())
        );
    }

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
        let outcome =
            pull(&repo, &pull_options(&repo), &mut NoProgress, None).expect("pull completes");
        assert_eq!(outcome.result, PullResult::FastForward);
        assert_eq!(outcome.oid, Some(tip));
        assert_eq!(git(&clone, &["rev-parse", "main"]), tip.to_hex());
        assert!(sink.warnings().iter().any(|warning| matches!(
            warning,
            crate::diagnostics::Warning::ReflogNotWritten { refname, .. }
                if refname == "refs/heads/main"
        )));
    }
}
