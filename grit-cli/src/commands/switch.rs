//! `grit switch` — move to another branch, remote branch, or detached commit.

use anyhow::{bail, Result};
use grit_lib::porcelain::switch::{
    resolve_create_branch, resolve_revision_spec, resolve_switch_target, switch_repository,
    SwitchMode, SwitchOptions,
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
    /// Branch `HEAD` is on after the switch, or JSON `null` when detached.
    pub branch: Option<String>,
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
                self.target_label(),
                self.changed_paths.unwrap_or(0)
            );
            return Ok(());
        }
        let Some(branch) = self.branch.as_deref() else {
            outln!(w, "HEAD detached at {}", self.target_label());
            return Ok(());
        };
        if self.created {
            outln!(w, "Created and switched to branch {branch}");
        } else {
            outln!(w, "Switched to branch {branch}");
        }
        Ok(())
    }
}

impl SwitchOutcome {
    /// The branch name, or the short detached commit, for human and Markdown output.
    pub fn target_label(&self) -> &str {
        self.branch.as_deref().unwrap_or_else(|| {
            self.commit
                .as_deref()
                .map(|c| &c[..7.min(c.len())])
                .unwrap_or("HEAD")
        })
    }
}

impl MarkdownRender for SwitchOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_switch_outcome(w, self)?;
        Ok(())
    }
}

pub fn run(
    target: Option<String>,
    start_point: Option<String>,
    create: bool,
    detach: bool,
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

    let outcome = switch_repository(&repo, &SwitchOptions { mode, dry_run })
        .map_err(json_error::map_lib_error)?;

    let detached = outcome.is_detached();
    Ok(SwitchOutcome {
        branch: outcome.branch,
        created: outcome.created,
        detached,
        commit: detached.then(|| outcome.commit.to_hex()),
        dry_run: outcome.dry_run,
        changed_paths: outcome.dry_run.then_some(outcome.changed_paths),
    })
}
