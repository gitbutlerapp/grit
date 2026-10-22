//! `grit pull` — fetch from the remote, then integrate the upstream into the
//! current branch (fast-forward when possible, otherwise a merge).

#[path = "pull_outcome.rs"]
mod pull_outcome;

use crate::commands::auth;
use crate::commands::fetch::map_remote_error;
use crate::context;
use crate::output::OutputMode;
use crate::progress::TerminalProgress;
use anyhow::{bail, Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::porcelain::pull::{pull, PullError, PullMode, PullOptions};
use grit_lib::remote::transfer::{resolve_fetch_prune, FetchOptions, TagMode};
use grit_lib::remote::{DefaultHttpClientFactory, DEFAULT_REMOTE};
use grit_lib::state::{resolve_head, HeadState};

pub use pull_outcome::{map_outcome, PullOutcome, PullResult};

pub fn run(ff_only: bool, no_rebase: bool, mode: OutputMode) -> Result<PullOutcome> {
    let repo = context::discover()?;
    let config = ConfigSet::load(&context::environment(), Some(repo.git_dir()), true)
        .context("could not load config")?;

    let short_name = match resolve_head(repo.git_dir())? {
        HeadState::Branch { short_name, .. } => short_name,
        HeadState::Detached { .. } => bail!("HEAD is detached; grit pull needs a branch"),
        HeadState::Invalid => bail!("HEAD is in an unknown state"),
    };

    let remote_name = config
        .get(&format!("branch.{short_name}.remote"))
        .filter(|r| !r.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_REMOTE.to_owned());

    let cli_mode = if ff_only {
        Some(PullMode::FfOnly)
    } else if no_rebase {
        Some(PullMode::Merge)
    } else {
        None
    };
    let prune = resolve_fetch_prune(&config, &remote_name, None)
        .context("could not resolve fetch prune settings")?;
    let options = PullOptions {
        remote: None,
        branch: None,
        mode: cli_mode,
        fetch: FetchOptions {
            tags: TagMode::Following,
            thread_policy: repo.thread_policy(),
            environment: context::environment(),
            prune,
            ..Default::default()
        },
    };
    let factory = DefaultHttpClientFactory;
    let mut progress = TerminalProgress::new(mode);
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
