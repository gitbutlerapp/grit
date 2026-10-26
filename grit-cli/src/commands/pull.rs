//! `grit pull` — fetch from the remote, then integrate the upstream into the
//! current branch (fast-forward when possible, otherwise a merge). In a snapshot
//! checkout it refreshes the checkout onto the server's newest snapshot, and
//! `grit pull <branch>` brings a branch onto the server's branch of that name.

#[path = "pull_outcome.rs"]
mod pull_outcome;

use crate::commands::auth;
use crate::commands::fetch::map_remote_error;
use crate::commands::fetch_args::FetchArgs;
use crate::context;
use crate::output::OutputMode;
use crate::progress::summary::transfer_header;
use anyhow::{Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::porcelain::pull::{pull, PullError, PullMode, PullOptions};
use grit_lib::progress::NeverCancel;
use grit_lib::remote::fetch_request::resolve_pull_invocation;
use grit_lib::remote::snapshot_checkout::{
    ensure_refreshable, fetch_branch, refresh, refresh_branch, same_branch_name, valid_branch_name,
    CheckoutClient, CheckoutError, CheckoutMarker,
};
use grit_lib::remote::transfer::{resolve_fetch_prune, resolve_fetch_tags, FetchOptions};
use grit_lib::remote::HttpClientFactory;
use grit_lib::repo::Repository;
use grit_lib::state::{resolve_head_for_repository, InProgressOperation};

pub(crate) use pull_outcome::{changes_phrase, commits_on_snapshots, gave_change_ids_phrase};
pub use pull_outcome::{map_outcome, PullOutcome, PullResult};

#[derive(Default)]
pub struct PullRunArgs {
    pub fetch: FetchArgs,
    pub ff_only: bool,
    pub no_rebase: bool,
    pub rebase: bool,
    pub autostash: bool,
    pub check: bool,
}

impl PullRunArgs {
    /// A plain `grit pull`, as `grit integrate` runs it to refresh a checkout.
    #[must_use]
    pub fn refresh() -> Self {
        Self::default()
    }
}

/// The integration mode the flags ask for, if any.
fn cli_mode(args: &PullRunArgs) -> Option<PullMode> {
    if args.ff_only {
        Some(PullMode::FfOnly)
    } else if args.no_rebase {
        Some(PullMode::Merge)
    } else if args.rebase {
        Some(PullMode::Rebase)
    } else {
        None
    }
}

pub fn run(args: &PullRunArgs, mode: OutputMode) -> Result<PullOutcome> {
    if args.check && args.autostash {
        anyhow::bail!("--check cannot be used with --autostash");
    }
    let repo = context::discover()?;
    let config = ConfigSet::load(&context::environment(), Some(repo.git_dir()), true)
        .context("could not load config")?;
    match checkout_pull(&repo, &config, args) {
        Some(CheckoutPull::Refresh(marker)) => {
            return refresh_checkout(&repo, &config, &marker, args, mode)
        }
        Some(CheckoutPull::Branch(marker, name)) => {
            return pull_server_branch(&repo, &config, &marker, &name, args, mode)
        }
        None => {}
    }
    let mut options =
        PullOptions::for_working_branch(&repo, context::environment(), cli_mode(args))
            .map_err(anyhow::Error::new)?;
    let head = resolve_head_for_repository(&repo)?;
    let invocation = resolve_pull_invocation(
        &config,
        args.fetch.remote.as_deref(),
        &args.fetch.refspecs,
        head.branch_name(),
    );
    options.remote = Some(invocation.remote.clone());
    options.explicit_fetch = !invocation.user_refspecs.is_empty();
    if invocation.user_refspecs.len() == 1 {
        options.branch = Some(invocation.user_refspecs[0].clone());
    }
    options.autostash = args.autostash;
    options.check = args.check;
    options.fetch = FetchOptions {
        refspecs: invocation.user_refspecs,
        tags: resolve_fetch_tags(&config, &invocation.remote, args.fetch.tag_mode_override()),
        thread_policy: repo.thread_policy(),
        environment: context::environment(),
        prune: resolve_fetch_prune(&config, &invocation.remote, args.fetch.prune_override())
            .context("could not resolve fetch prune settings")?,
        dry_run: args.fetch.dry_run,
        remote_name: Some(invocation.remote.clone()),
        ..Default::default()
    };
    let remote_name = invocation.remote;
    let factory = crate::context::http_client_factory();
    // The work-tree update after the fetch reports no progress, so the overall
    // bar covers the transfer only.
    let mut progress = args.fetch.quiet.progress(mode).with_overall(false);
    let url = config.get(&format!("remote.{remote_name}.url"));
    if progress.is_drawing() && remote_name != "." {
        eprintln!(
            "{}",
            transfer_header(
                "Pulling",
                &remote_name,
                url.as_deref(),
                progress.uses_color()
            )
        );
    }
    let lib = match pull(&repo, &options, &mut progress, &NeverCancel, Some(&factory)) {
        Ok(outcome) => outcome,
        Err(PullError::Remote(remote_err)) => {
            let mapped = map_remote_error(remote_err);
            if auth::offer_reauth(&mapped, url.as_deref().unwrap_or_default())? {
                pull(&repo, &options, &mut progress, &NeverCancel, Some(&factory))?
            } else {
                return Err(mapped);
            }
        }
        Err(other) => {
            return Err(match other {
                PullError::Remote(remote) => map_remote_error(remote),
                rest => anyhow::Error::new(rest),
            });
        }
    };
    let mut outcome = map_outcome(lib);
    outcome.transfer.records = progress.phases().to_vec();
    outcome.transfer.elapsed = progress.elapsed();
    Ok(outcome)
}

/// What `grit pull` does in a snapshot checkout.
enum CheckoutPull {
    /// Refresh it onto the server's newest snapshot.
    Refresh(CheckoutMarker),
    /// Bring the local branch onto the server's branch of this name.
    Branch(CheckoutMarker, String),
}

/// What `grit pull` should do in a snapshot checkout, if it is one.
///
/// A pull that names no refspecs and no remote refreshes it, and so does one
/// naming the checkout's own server (`origin`, or whatever it is called now).
/// `grit pull <branch>`, where `<branch>` is not a configured remote, and
/// `grit pull <server> <branch>` bring the branch onto the server's; naming
/// the branch the checkout follows is a refresh. Anything else is a plain
/// pull.
fn checkout_pull(
    repo: &Repository,
    config: &ConfigSet,
    args: &PullRunArgs,
) -> Option<CheckoutPull> {
    if args.fetch.all {
        return None;
    }
    let marker = CheckoutMarker::read(repo.git_dir())?;
    let remote_url = |remote: &str| config.get(&format!("remote.{remote}.url"));
    let is_server =
        |remote: &str| remote == marker.url || remote_url(remote).as_deref() == Some(&marker.url);
    let branch = match (args.fetch.remote.as_deref(), args.fetch.refspecs.as_slice()) {
        (None, []) => None,
        (Some(remote), []) if is_server(remote) => None,
        (Some(name), []) if remote_url(name).is_none() && valid_branch_name(name) => {
            Some(name.to_owned())
        }
        (Some(remote), [branch]) if is_server(remote) => {
            let branch = branch.strip_prefix("refs/heads/").unwrap_or(branch);
            if !valid_branch_name(branch) {
                return None;
            }
            Some(branch.to_owned())
        }
        _ => return None,
    };
    Some(match branch {
        Some(name) if !same_branch_name(&name, &marker.branch) => {
            CheckoutPull::Branch(marker, name)
        }
        _ => CheckoutPull::Refresh(marker),
    })
}

/// `grit pull <branch>` in a snapshot checkout: the server shows its branch
/// as this checkout sees it, the projection is fetched into
/// `origin/<branch>`, and the local branch is created, moved or rebased onto
/// it. With `--check`, says what it would do to the branch.
fn pull_server_branch(
    repo: &Repository,
    config: &ConfigSet,
    marker: &CheckoutMarker,
    name: &str,
    args: &PullRunArgs,
    mode: OutputMode,
) -> Result<PullOutcome> {
    if args.ff_only || args.no_rebase || args.autostash || args.fetch.dry_run {
        anyhow::bail!(
            "`grit pull {name}` in a snapshot checkout rebases {name} onto the server's; it takes no --ff-only, --no-rebase, --autostash or --dry-run"
        );
    }
    ensure_refreshable(repo).map_err(refresh_error)?;
    let http = crate::context::http_client_factory()
        .create(config)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let client = CheckoutClient::new(http.as_ref(), &marker.url)?;
    let mut progress = args.fetch.quiet.progress(mode).with_overall(false);
    if progress.is_drawing() {
        eprintln!(
            "{}",
            transfer_header("Pulling", name, Some(&marker.url), progress.uses_color())
        );
    }
    let info = client.branch(&marker.id, name)?;
    let fetch = FetchOptions {
        thread_policy: repo.thread_policy(),
        environment: context::environment(),
        ..Default::default()
    };
    let transfer = fetch_branch(
        repo,
        marker,
        &info,
        fetch,
        &mut progress,
        &NeverCancel,
        Some(&crate::context::http_client_factory()),
    )
    .map_err(refresh_error)?;
    let refreshed = refresh_branch(repo, marker, &info, args.check).map_err(refresh_error)?;
    let mut outcome = PullOutcome::from_server_branch(&info, &refreshed, transfer);
    outcome.transfer.records = progress.phases().to_vec();
    outcome.transfer.elapsed = progress.elapsed();
    Ok(outcome)
}

/// `grit pull` in a snapshot checkout: ask the server for a snapshot of the
/// newest target, fetch it from the checkout's server and bring the current
/// branch onto it. With `--dry-run`, only ask where the target is now.
fn refresh_checkout(
    repo: &Repository,
    config: &ConfigSet,
    marker: &CheckoutMarker,
    args: &PullRunArgs,
    mode: OutputMode,
) -> Result<PullOutcome> {
    ensure_refreshable(repo).map_err(refresh_error)?;
    let http = crate::context::http_client_factory()
        .create(config)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let client = CheckoutClient::new(http.as_ref(), &marker.url)?;
    if args.fetch.dry_run {
        let state = client.show(&marker.id)?;
        return Ok(PullOutcome::refresh_dry_run(marker, &state));
    }
    let mut progress = args.fetch.quiet.progress(mode).with_overall(false);
    if progress.is_drawing() {
        eprintln!(
            "{}",
            transfer_header(
                "Refreshing",
                &marker.branch,
                Some(&marker.url),
                progress.uses_color()
            )
        );
    }
    let info = client.update(&marker.id)?;
    let options = PullOptions {
        mode: cli_mode(args),
        autostash: args.autostash,
        check: args.check,
        fetch: FetchOptions {
            thread_policy: repo.thread_policy(),
            environment: context::environment(),
            ..Default::default()
        },
        ..Default::default()
    };
    let refreshed = refresh(
        repo,
        marker,
        &info,
        &options,
        &mut progress,
        &NeverCancel,
        Some(&crate::context::http_client_factory()),
    )
    .map_err(refresh_error)?;
    let mut outcome = PullOutcome::from_refresh(refreshed);
    outcome.transfer.records = progress.phases().to_vec();
    outcome.transfer.elapsed = progress.elapsed();
    Ok(outcome)
}

/// A refresh failure in words that say what to do next.
fn refresh_error(err: CheckoutError) -> anyhow::Error {
    match err {
        CheckoutError::Remote(remote) => map_remote_error(*remote),
        CheckoutError::Pull(pull) => match *pull {
            PullError::Remote(remote) => map_remote_error(remote),
            other => anyhow::Error::new(other),
        },
        CheckoutError::OperationInProgress(operation) => {
            let next = match operation {
                InProgressOperation::Rebase | InProgressOperation::RebaseInteractive => {
                    "resolve the conflicts, `grit add` the files and run `grit rebase --continue`, or put the branch back with `grit rebase --abort`"
                }
                InProgressOperation::Merge => {
                    "finish it with `grit commit`, or cancel it with `grit merge --abort`"
                }
                _ => "finish or cancel it",
            };
            anyhow::anyhow!(
                "a {} is in progress; {next}, then run `grit pull` again",
                operation.description()
            )
        }
        moved @ CheckoutError::SnapshotMoved { .. } => {
            anyhow::anyhow!("{moved}; run `grit pull` again")
        }
        CheckoutError::BranchMoved {
            name,
            expected,
            got,
        } => anyhow::anyhow!(
            "{}; run `grit pull {name}` again",
            CheckoutError::BranchMoved {
                name: name.clone(),
                expected,
                got
            }
        ),
        CheckoutError::UnrelatedBranch { name } => anyhow::anyhow!(
            "your {name} has nothing in common with the server's {name}; rename yours (`grit branch -m {name} <new name>`), then `grit pull {name}` gets the server's"
        ),
        other => other.into(),
    }
}

/// Exit code for `grit pull --check` (0 when up-to-date or fast-forward is possible).
#[must_use]
pub fn pull_check_exit_code(outcome: &PullOutcome) -> i32 {
    if let Some(check) = &outcome.check {
        if check.fast_forward_possible {
            return 0;
        }
        return 1;
    }
    match outcome.result {
        PullResult::Conflicts
        | PullResult::RefusedNotFf
        | PullResult::AutostashConflict
        | PullResult::RebaseStopped => 1,
        _ => 0,
    }
}
