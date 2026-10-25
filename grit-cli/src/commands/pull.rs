//! `grit pull` — fetch from the remote, then integrate the upstream into the
//! current branch (fast-forward when possible, otherwise a merge).

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
use grit_lib::remote::transfer::{resolve_fetch_prune, resolve_fetch_tags, FetchOptions};
use grit_lib::state::resolve_head_for_repository;

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

pub fn run(args: &PullRunArgs, mode: OutputMode) -> Result<PullOutcome> {
    if args.check && args.autostash {
        anyhow::bail!("--check cannot be used with --autostash");
    }
    let repo = context::discover()?;
    let config = ConfigSet::load(&context::environment(), Some(repo.git_dir()), true)
        .context("could not load config")?;
    let cli_mode = if args.ff_only {
        Some(PullMode::FfOnly)
    } else if args.no_rebase {
        Some(PullMode::Merge)
    } else if args.rebase {
        Some(PullMode::Rebase)
    } else {
        None
    };
    let mut options = PullOptions::for_working_branch(&repo, context::environment(), cli_mode)
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
    // In a snapshot checkout, pull refreshes onto the server's newest snapshot.
    let refresh = if args.fetch.remote.is_none() && args.fetch.refspecs.is_empty() {
        crate::commands::checkout::refresh_pull_options(&repo, &mut options)?
    } else {
        None
    };
    let remote_name = if refresh.is_some() {
        "origin".to_owned()
    } else {
        invocation.remote
    };
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
    outcome.refresh = refresh;
    outcome.transfer.records = progress.phases().to_vec();
    outcome.transfer.elapsed = progress.elapsed();
    Ok(outcome)
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
        PullResult::Conflicts | PullResult::RefusedNotFf | PullResult::AutostashConflict => 1,
        _ => 0,
    }
}
