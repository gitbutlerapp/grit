//! `grit switch` — move to another branch, remote branch, or detached commit.

use anyhow::{bail, Context, Result};
use grit_lib::porcelain::switch::{
    resolve_create_branch, resolve_revision_spec, resolve_switch_target, switch_repository,
    SwitchMode, SwitchOptions,
};
use serde::Serialize;
use std::io::Write;

use crate::context;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// Result of `grit switch`.
#[derive(Serialize)]
pub struct SwitchOutcome {
    pub branch: String,
    pub created: bool,
    pub detached: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

impl HumanRender for SwitchOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
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

impl MarkdownRender for SwitchOutcome {}

pub fn run(
    target: Option<String>,
    start_point: Option<String>,
    create: bool,
    detach: bool,
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
            commit: resolve_revision_spec(&repo, spec).map_err(anyhow::Error::new)?,
            reflog_to_label: spec.to_owned(),
        }
    } else if create {
        let branch = target.ok_or_else(|| anyhow::anyhow!("branch name required with --create"))?;
        resolve_create_branch(&repo, &branch, start_point.as_deref()).map_err(anyhow::Error::new)?
    } else {
        let spec = target.ok_or_else(|| anyhow::anyhow!("branch or revision required"))?;
        resolve_switch_target(&repo, &spec).map_err(anyhow::Error::new)?
    };

    let outcome = switch_repository(&repo, &SwitchOptions { mode })
        .map_err(anyhow::Error::new)
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
    })
}
