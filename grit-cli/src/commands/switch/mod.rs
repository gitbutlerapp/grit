//! `grit switch` — move to another branch, remote branch, or detached commit.

use anyhow::{bail, Result};
use grit_lib::command_runner::hooks::PostOperationHooks;
use grit_lib::porcelain::branch::{upstream_tracking_display, TrackMode};
use grit_lib::porcelain::switch::{SwitchMode, SwitchOptions};
use grit_lib::progress::NullProgress;

use crate::context;
pub use crate::render::switch::SwitchOutcome;

fn track_mode(track: bool, no_track: bool) -> TrackMode {
    if no_track {
        TrackMode::Never
    } else if track {
        TrackMode::Always
    } else {
        TrackMode::Auto
    }
}

pub fn run(
    target: Option<String>,
    start_point: Option<String>,
    create: bool,
    detach: bool,
    track: bool,
    no_track: bool,
    dry_run: bool,
) -> Result<SwitchOutcome> {
    let repo = context::discover()?;

    if create && detach {
        bail!("cannot use --create and --detach together");
    }

    let mode = if detach {
        let spec = target
            .as_deref()
            .or(start_point.as_deref())
            .ok_or_else(|| anyhow::anyhow!("revision required with --detach"))?;
        SwitchMode::Detached {
            commit: repo
                .checkout()
                .resolve_revision_spec(spec)
                .map_err(anyhow::Error::new)?,
            reflog_to_label: spec.to_owned(),
        }
    } else if create {
        let branch = target.ok_or_else(|| anyhow::anyhow!("branch name required with --create"))?;
        repo.branches()
            .resolve_create_for_switch(&branch, start_point.as_deref(), track_mode(track, no_track))
            .map_err(anyhow::Error::new)?
    } else {
        let spec = target.ok_or_else(|| anyhow::anyhow!("branch or revision required"))?;
        repo.checkout()
            .resolve_switch_target(&spec)
            .map_err(anyhow::Error::new)?
    };

    let upstream_display = match &mode {
        SwitchMode::Create {
            track_upstream: Some((remote, merge)),
            ..
        } => Some(upstream_tracking_display(remote, merge)),
        _ => None,
    };

    let _interrupt = crate::interrupt::InterruptCancel::install();
    let cancel = crate::interrupt::process_cancel_token();
    let outcome = repo
        .checkout()
        .switch(
            &SwitchOptions {
                mode,
                dry_run,
                hooks: PostOperationHooks::Run,
            },
            &mut NullProgress,
            &cancel,
        )
        .map_err(|err| crate::error_map::map_switch_lib_error(&repo, err))?;

    let detached = outcome.is_detached();
    Ok(SwitchOutcome {
        branch: outcome.branch,
        created: outcome.created,
        upstream: upstream_display,
        detached,
        commit: detached.then(|| outcome.commit.to_hex()),
        dry_run: outcome.dry_run,
        changed_paths: outcome.dry_run.then_some(outcome.changed_paths),
        hooks_run: outcome
            .hooks_run
            .iter()
            .map(|h| h.as_str().to_owned())
            .collect(),
        stranded_commits: outcome
            .stranded_commits
            .into_iter()
            .map(Into::into)
            .collect(),
        stranded_total: outcome.stranded_total,
        stranded_truncated: outcome.stranded_truncated,
        path_collisions: outcome.path_collisions,
    })
}

mod dispatch;
pub use dispatch::dispatch;
