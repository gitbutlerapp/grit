//! `grit pull` — fetch from the remote, then integrate the upstream into the
//! current branch (fast-forward when possible, otherwise a merge).

use anyhow::{Context, Result};
use grit_lib::porcelain::pull::{
    integrate_upstream, plan_pull, PullError, PullOutcome, PullSource,
};
use grit_lib::remote::transfer::{resolve_fetch_prune, FetchOptions, TagMode};
use grit_lib::remote::{DefaultHttpClientFactory, Remote};

use crate::commands::fetch::map_remote_error;
use crate::commands::merge::MergeOutcome;
use crate::context;
use crate::output::OutputMode;
use crate::progress::TerminalProgress;

pub fn run(mode: OutputMode) -> Result<MergeOutcome> {
    let repo = context::discover()?;
    let plan = plan_pull(&repo).map_err(map_pull_error)?;

    if let PullSource::Remote(remote) = &plan.source {
        let config = repo.config().context("could not load config")?;
        let prune = resolve_fetch_prune(&config, remote, None)
            .context("could not resolve fetch prune settings")?;
        let factory = DefaultHttpClientFactory;
        let mut progress = TerminalProgress::new(mode);
        Remote::from_config(&config, remote)
            .map_err(map_remote_error)?
            .fetch(
                &repo,
                FetchOptions {
                    tags: TagMode::Following,
                    thread_policy: repo.thread_policy(),
                    environment: context::environment(),
                    prune,
                    ..Default::default()
                },
                &mut progress,
                Some(&factory),
            )
            .map_err(map_remote_error)?;
    }

    let label = plan.upstream_label();
    Ok(
        match integrate_upstream(&repo, &plan).map_err(map_pull_error)? {
            PullOutcome::Integrated(outcome) => MergeOutcome::from_integrate(&label, outcome),
            PullOutcome::Adopted { oid, change_id } => {
                MergeOutcome::set_upstream(&plan.branch, &label, oid, change_id)
            }
        },
    )
}

fn map_pull_error(err: PullError) -> anyhow::Error {
    match err {
        PullError::DetachedHead => anyhow::anyhow!("HEAD is detached; grit pull needs a branch"),
        PullError::MissingUpstream { upstream_ref } => {
            anyhow::anyhow!("no upstream tracking ref {upstream_ref} after fetch")
        }
        PullError::Library(err) => anyhow::Error::new(err),
        other => anyhow::Error::new(other),
    }
}
