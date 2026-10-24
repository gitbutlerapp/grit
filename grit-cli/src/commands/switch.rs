//! `grit switch` — move to another branch, remote branch, or detached commit.

use anyhow::{bail, Result};
use grit_lib::error::Error as LibError;
use grit_lib::porcelain::switch::{
    resolve_create_branch, resolve_revision_spec, resolve_switch_target, switch_repository,
    SwitchMode, SwitchOptions,
};
use grit_lib::state::{status_operation, wt_status_get_state, HeadState};
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

    let outcome =
        switch_repository(&repo, &SwitchOptions { mode, dry_run }).map_err(|err| match err {
            LibError::OperationInProgress(kind) => {
                let operation = wt_status_get_state(repo.git_dir(), &HeadState::Invalid, false)
                    .ok()
                    .and_then(|state| status_operation(&state, false));
                json_error::operation_in_progress("switch branches", kind, operation.as_ref())
            }
            other => json_error::map_lib_error(other),
        })?;

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
        changed_paths: outcome.dry_run.then_some(outcome.changed_paths),
    })
}
