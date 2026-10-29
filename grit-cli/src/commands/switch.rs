//! `grit switch` — move to another branch, remote branch, or detached commit.

use anyhow::{bail, Result};
use grit_lib::error::Error as LibError;
use grit_lib::porcelain::switch::{
    resolve_create_branch, resolve_revision_spec, resolve_switch_target, switch_repository,
    SwitchMode, SwitchOptions,
};
use grit_lib::state::{status_operation, wt_status_get_state, HeadState};
use grit_lib::PostOperationHooks;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::commands::reset::OrphanedCommitJson;
use crate::context;
use crate::json_error;
use crate::markdown::{self, short_hex};
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
    /// Hooks that ran after the switch (`post-checkout` when installed).
    pub hooks_run: Vec<String>,
    /// Newest-first commits only the previous detached `HEAD` reached (capped at 20).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub stranded_commits: Vec<OrphanedCommitJson>,
    /// Total commits left behind; may exceed `stranded_commits` when truncated.
    #[serde(default, skip_serializing_if = "is_zero")]
    #[schemars(default)]
    /// Advisory count; without a commit-graph, severe clock skew can make it inaccurate.
    pub stranded_total: usize,
    #[serde(default, skip_serializing_if = "is_false")]
    #[schemars(default)]
    pub stranded_truncated: bool,
    /// Index paths that collided during checkout on a case-insensitive filesystem.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub path_collisions: Vec<String>,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

impl SwitchOutcome {
    /// Warn on stderr that commits were (or would be) left behind, with a command to keep them.
    fn warn_stranded(&self) {
        let Some(newest) = self.stranded_commits.first() else {
            return;
        };
        let count = self.stranded_total;
        let (noun, them) = if count == 1 {
            ("commit", "it")
        } else {
            ("commits", "them")
        };
        let would = if self.dry_run { " would be" } else { "" };
        eprintln!("Warning: {count} {noun}{would} left behind, not on any branch:");
        for commit in &self.stranded_commits {
            eprintln!("  {} {}", short_hex(&commit.oid), commit.subject);
        }
        if self.stranded_truncated {
            eprintln!("  … and {} more", count - self.stranded_commits.len());
        }
        eprintln!(
            "Keep {them} on a branch with: grit branch <name> {}",
            newest.oid
        );
    }
}

impl HumanRender for SwitchOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        self.render_switch_line(w)?;
        if !self.stranded_commits.is_empty() {
            crate::stdio::io_result(w.flush())
                .map_err(|e| anyhow::anyhow!("writing output: {e}"))?;
            self.warn_stranded();
        }
        Ok(())
    }
}

impl SwitchOutcome {
    fn render_switch_line(&self, w: &mut dyn Write) -> Result<()> {
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
            return render_hooks_run(w, &self.hooks_run);
        };
        if self.created {
            outln!(w, "Created and switched to branch {branch}");
        } else {
            outln!(w, "Switched to branch {branch}");
        }
        render_hooks_run(w, &self.hooks_run)
    }
}

fn render_hooks_run(w: &mut dyn Write, hooks_run: &[String]) -> Result<()> {
    if !hooks_run.is_empty() {
        outln!(w, "hooks: {}", hooks_run.join(", "));
    }
    Ok(())
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

    let interrupt = crate::interrupt::InterruptCancel::install();
    let outcome = switch_repository(
        &repo,
        &SwitchOptions {
            mode,
            dry_run,
            hooks: PostOperationHooks::Run,
        },
        &mut grit_lib::progress::NullProgress,
        &interrupt,
    )
    .map_err(|err| match err {
        LibError::OperationInProgress(kind) => {
            let operation = wt_status_get_state(repo.git_dir(), &HeadState::Invalid, false)
                .ok()
                .and_then(|state| status_operation(&state, false));
            json_error::operation_in_progress("switch branches", kind, operation.as_ref())
        }
        other => json_error::map_lib_error(other),
    })?;

    let detached = outcome.is_detached();
    Ok(SwitchOutcome {
        branch: outcome.branch,
        created: outcome.created,
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
