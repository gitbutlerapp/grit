//! `grit clean` — remove untracked files from the working tree.

use crate::pathspec_args::PathspecArgs;
use anyhow::{bail, Result};
use grit_lib::porcelain::clean::{clean_untracked, CleanOptions, CleanScope};
use grit_lib::progress::NullProgress;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::context;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// JSON scope label for [`CleanScope`].
fn scope_label(scope: CleanScope) -> &'static str {
    match scope {
        CleanScope::Untracked => "untracked",
        CleanScope::UntrackedAndIgnored => "untracked_and_ignored",
        CleanScope::OnlyIgnored => "only_ignored",
    }
}

/// Result of `grit clean`.
#[derive(Serialize, JsonSchema)]
pub struct CleanOutcome {
    /// Paths removed, or that would be removed when dry-run.
    pub removed: Vec<String>,
    /// Whether this run only previewed removals (no `-f`).
    pub dry_run: bool,
    /// Which paths were eligible for removal.
    pub scope: String,
}

impl HumanRender for CleanOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.removed.is_empty() {
            if self.dry_run {
                outln!(w, "Nothing to clean.");
            } else {
                outln!(w, "Removed nothing.");
            }
            return Ok(());
        }
        let prefix = if self.dry_run {
            "Would remove"
        } else {
            "Removed"
        };
        for path in &self.removed {
            outln!(w, "{prefix} {path}");
        }
        Ok(())
    }
}

impl MarkdownRender for CleanOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.removed.is_empty() {
            outln!(w, "No paths to remove.");
            return Ok(());
        }
        let heading = if self.dry_run {
            "Would remove"
        } else {
            "Removed"
        };
        outln!(w, "## {heading}\n");
        for path in &self.removed {
            outln!(w, "- `{path}`");
        }
        Ok(())
    }
}

pub fn run(
    args: &PathspecArgs,
    force: bool,
    dry_run_flag: bool,
    ignored: bool,
    only_ignored: bool,
    excludes: &[String],
    files_only: bool,
) -> Result<CleanOutcome> {
    if ignored && only_ignored {
        bail!("--ignored and --only-ignored cannot be used together");
    }
    if force && dry_run_flag {
        bail!("--dry-run cannot be used with -f/--force");
    }
    let scope = if only_ignored {
        CleanScope::OnlyIgnored
    } else if ignored {
        CleanScope::UntrackedAndIgnored
    } else {
        CleanScope::Untracked
    };
    let dry_run = !force;

    let repo = context::discover()?;
    let resolved = args.resolve(&repo)?;
    let pathspecs = resolved.into_specs();
    let lib_outcome = clean_untracked(
        &repo,
        &CleanOptions {
            pathspecs,
            directories: true,
            files_only,
            scope,
            excludes: excludes.to_vec(),
            dry_run,
        },
        &mut NullProgress,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(CleanOutcome {
        removed: lib_outcome.removed,
        dry_run,
        scope: scope_label(scope).to_owned(),
    })
}
