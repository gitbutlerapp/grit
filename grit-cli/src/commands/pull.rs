//! `grit pull` — fetch from the remote, then integrate the upstream into the
//! current branch (fast-forward when possible, otherwise a merge).

#[path = "pull_outcome.rs"]
mod pull_outcome;

use crate::commands::auth;
use crate::commands::fetch::map_remote_error;
use crate::context;
use crate::output::OutputMode;
use crate::progress::TerminalProgress;
use anyhow::{Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::porcelain::pull::{pull, PullError, PullMode, PullOptions};
use grit_lib::remote::DefaultHttpClientFactory;

pub use pull_outcome::{map_outcome, PullOutcome, PullResult};

pub fn run(ff_only: bool, no_rebase: bool, rebase: bool, mode: OutputMode) -> Result<PullOutcome> {
    let repo = context::discover()?;
    let cli_mode = if ff_only {
        Some(PullMode::FfOnly)
    } else if no_rebase {
        Some(PullMode::Merge)
    } else if rebase {
        Some(PullMode::Rebase)
    } else {
        None
    };
    let options = PullOptions::for_working_branch(&repo, context::environment(), cli_mode)
        .map_err(anyhow::Error::new)?;
    let remote_name = options
        .fetch
        .remote_name
        .clone()
        .unwrap_or_else(|| "origin".to_owned());
    let config = ConfigSet::load(&context::environment(), Some(repo.git_dir()), true)
        .context("could not load config")?;
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
