//! `grit pull` — fetch from the remote, then integrate the upstream into the
//! current branch (fast-forward when possible, otherwise a merge).

#[path = "pull_outcome.rs"]
mod pull_outcome;

use crate::commands::auth;
use crate::commands::fetch::map_remote_error;
use crate::commands::fetch_args::FetchArgs;
use crate::context;
use crate::output::OutputMode;
use anyhow::{Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::porcelain::pull::{pull, PullError, PullMode, PullOptions};
use grit_lib::remote::fetch_request::resolve_fetch_invocation;
use grit_lib::remote::transfer::{resolve_fetch_prune, resolve_fetch_tags, FetchOptions};
use grit_lib::remote::{DefaultHttpClientFactory, DEFAULT_REMOTE};

pub use pull_outcome::{map_outcome, PullOutcome, PullResult};

pub struct PullRunArgs {
    pub fetch: FetchArgs,
    pub ff_only: bool,
    pub no_rebase: bool,
    pub rebase: bool,
    pub autostash: bool,
    pub check: bool,
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
    let invocation = resolve_fetch_invocation(
        &config,
        args.fetch.remote.as_deref(),
        &args.fetch.refspecs,
        DEFAULT_REMOTE,
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
    let factory = DefaultHttpClientFactory;
    let mut progress = args.fetch.quiet.progress(mode);
    let lib = match pull(&repo, &options, &mut progress, Some(&factory)) {
        Ok(outcome) => outcome,
        Err(PullError::Remote(remote_err)) => {
            let url = config
                .get(&format!("remote.{remote_name}.url"))
                .unwrap_or_default();
            let mapped = map_remote_error(remote_err);
            if auth::offer_reauth(&mapped, &url)? {
                pull(&repo, &options, &mut progress, Some(&factory))?
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
    Ok(map_outcome(lib))
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
