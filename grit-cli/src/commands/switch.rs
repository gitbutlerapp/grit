//! `grit switch` — move to another branch, remote branch, or detached commit.

use anyhow::{bail, Context, Result};
use grit_lib::porcelain::branch_error::TrackMode;
use grit_lib::porcelain::switch::{
    resolve_orphan_branch, resolve_revision_spec, switch_repository, SwitchMode, SwitchOptions,
};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::context;
use crate::json_error;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

fn is_false(value: &bool) -> bool {
    !*value
}

/// Result of `grit switch`.
#[derive(Serialize, JsonSchema)]
pub struct SwitchOutcome {
    pub branch: String,
    pub created: bool,
    pub detached: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    #[schemars(default)]
    pub dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub changed_paths: Option<usize>,
}

impl HumanRender for SwitchOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.dry_run {
            outln!(
                w,
                "Would switch to {} ({} path(s) would change)",
                if self.branch.is_empty() {
                    self.commit
                        .as_deref()
                        .map(|c| &c[..7.min(c.len())])
                        .unwrap_or("HEAD")
                } else {
                    self.branch.as_str()
                },
                self.changed_paths.unwrap_or(0)
            );
            return Ok(());
        }
        if self.detached {
            let short = self
                .commit
                .as_deref()
                .map(|c| &c[..7.min(c.len())])
                .unwrap_or("HEAD");
            outln!(w, "HEAD detached at {short}");
            return Ok(());
        }
        if self.created {
            outln!(w, "Created and switched to branch {}", self.branch);
        } else {
            outln!(w, "Switched to branch {}", self.branch);
        }
        Ok(())
    }
}

impl MarkdownRender for SwitchOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_switch_outcome(w, self)?;
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    target: Option<String>,
    start_point: Option<String>,
    create: bool,
    force_create: bool,
    detach: bool,
    orphan: Option<String>,
    discard_changes: bool,
    track: bool,
    no_track: bool,
    no_guess: bool,
    dry_run: bool,
) -> Result<SwitchOutcome> {
    let repo = context::discover()?;

    if create && force_create {
        bail!("cannot use --create and --force-create (-C) together");
    }
    if create && detach {
        bail!("cannot use --create and --detach together");
    }
    if force_create && detach {
        bail!("cannot use --force-create (-C) and --detach together");
    }
    if orphan.is_some() && (create || force_create || detach) {
        bail!("cannot combine --orphan with --create, -C, or --detach");
    }
    if track && no_track {
        bail!("cannot use --track and --no-track together");
    }

    let track_mode = if track {
        TrackMode::Always
    } else if no_track {
        TrackMode::Never
    } else {
        TrackMode::Auto
    };

    let guess = if no_guess {
        false
    } else {
        match repo
            .config()
            .ok()
            .and_then(|c| c.get_bool("checkout.guess"))
        {
            Some(Ok(v)) => v,
            _ => true,
        }
    };

    let mode = if let Some(branch) = orphan {
        resolve_orphan_branch(&repo, &branch).map_err(anyhow::Error::new)?
    } else if detach {
        let spec = target
            .as_deref()
            .or(start_point.as_deref())
            .ok_or_else(|| anyhow::anyhow!("revision required with --detach"))?;
        SwitchMode::Detached {
            commit: resolve_revision_spec(&repo, spec).map_err(anyhow::Error::new)?,
            reflog_to_label: spec.to_owned(),
        }
    } else if create || force_create {
        let branch = target.ok_or_else(|| {
            anyhow::anyhow!("branch name required with --create or --force-create (-C)")
        })?;
        grit_lib::porcelain::switch::resolve_create_branch_with(
            &repo,
            &branch,
            start_point.as_deref(),
            force_create,
            track_mode,
        )
        .map_err(anyhow::Error::new)?
    } else {
        let spec = target.ok_or_else(|| anyhow::anyhow!("branch or revision required"))?;
        grit_lib::porcelain::switch::resolve_switch_target_with_guess(
            &repo, &spec, guess, track_mode,
        )
        .map_err(anyhow::Error::new)?
    };

    let outcome = switch_repository(
        &repo,
        &SwitchOptions {
            mode,
            discard_changes,
            dry_run,
        },
    )
    .map_err(json_error::map_lib_error)
    .context("could not switch")?;

    Ok(SwitchOutcome {
        branch: outcome.branch,
        created: outcome.created,
        detached: outcome.detached,
        commit: if outcome.detached {
            Some(outcome.commit.to_hex())
        } else {
            None
        },
        dry_run: outcome.dry_run,
        changed_paths: if outcome.dry_run {
            Some(outcome.changed_paths)
        } else {
            None
        },
    })
}
