//! `grit clean` — remove untracked files from the working tree.

use crate::pathspec_args::PathspecArgs;
use anyhow::{bail, Result};
use grit_lib::porcelain::clean::{CleanOptions, CleanScope};
use grit_lib::progress::NullProgress;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::context;
use crate::markdown::format_inline_code;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::ui::PathDisplayOptions;

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
    #[serde(skip)]
    #[schemars(skip)]
    path_display: Option<PathDisplayOptions>,
}

impl CleanOutcome {
    fn format_path(&self, path: &str) -> String {
        self.path_display
            .as_ref()
            .map(|opts| opts.format_path_for_human(path))
            .unwrap_or_else(|| path.to_owned())
    }
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
            outln!(w, "{prefix} {}", self.format_path(path));
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
            let shown = format_inline_code(&self.format_path(path));
            outln!(w, "- {shown}");
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
    let _interrupt = crate::interrupt::InterruptCancel::install();
    let resolved = args.resolve(&repo)?;
    let pathspecs = resolved.specs().to_vec();
    let cancel = crate::interrupt::process_cancel_token();
    let lib_outcome = repo
        .checkout()
        .clean(
            &CleanOptions {
                pathspecs,
                directories: true,
                files_only,
                scope,
                excludes: excludes.to_vec(),
                dry_run,
            },
            &mut NullProgress,
            &cancel,
        )
        .map_err(crate::json_error::map_lib_error)?;
    Ok(CleanOutcome {
        removed: lib_outcome.removed,
        dry_run,
        scope: scope_label(scope).to_owned(),
        path_display: PathDisplayOptions::from_repo(&repo).ok(),
    })
}

/// Dispatch parsed CLI args for `Clean`.
pub fn dispatch(
    args: crate::args::CleanArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;

    use crate::dispatch::{emit_success, CommandEnv};

    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let CleanArgs {
        pathspecs,
        force,
        dry_run,
        ignored,
        only_ignored,
        exclude,
        files_only,
    } = args;
    emit_success(
        commands::clean::run(
            &pathspecs,
            force,
            dry_run,
            ignored,
            only_ignored,
            &exclude,
            files_only,
        )?,
        opts,
        ctx,
    )
}
