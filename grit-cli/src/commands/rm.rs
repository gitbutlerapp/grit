//! `grit rm` — remove paths from the working tree and index.

use crate::pathspec_args::PathspecArgs;
use anyhow::{bail, Result};
use grit_lib::porcelain::paths::{remove_paths, BlockedRemoval, RemoveOptions, RemoveOutcome};
use grit_lib::repo::Repository;
use grit_lib::worktree::pathspec::ResolvedPathspecs;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::context;
use crate::markdown::format_inline_code;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::ui::PathDisplayOptions;

/// CLI outcome for `grit rm`.
#[derive(Serialize, JsonSchema)]
pub struct RmOutcome {
    /// Paths removed from the index (and work tree unless `--cached`).
    pub removed: Vec<String>,
    /// Whether this run only previewed removals.
    pub dry_run: bool,
    #[serde(skip)]
    #[schemars(skip)]
    path_display: Option<PathDisplayOptions>,
}

impl RmOutcome {
    fn format_path(&self, path: &str) -> String {
        self.path_display
            .as_ref()
            .map(|opts| opts.format_path_for_human(path))
            .unwrap_or_else(|| path.to_owned())
    }
}

impl HumanRender for RmOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let prefix = if self.dry_run {
            "Would remove"
        } else {
            "Removed"
        };
        match self.removed.len() {
            0 => {}
            1 => outln!(w, "{prefix} {}.", self.format_path(&self.removed[0])),
            n => outln!(w, "{prefix} {n} paths."),
        }
        Ok(())
    }
}

impl MarkdownRender for RmOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let heading = if self.dry_run {
            "Would remove"
        } else {
            "Removed"
        };
        if self.removed.is_empty() {
            outln!(w, "- **{heading}**: _(none)_");
            return Ok(());
        }
        outln!(w, "- **{heading}**:");
        for path in &self.removed {
            let shown = format_inline_code(&self.format_path(path));
            outln!(w, "  - {shown}");
        }
        Ok(())
    }
}

pub fn run(
    args: &PathspecArgs,
    cached: bool,
    force: bool,
    dry_run: bool,
    ignore_unmatch: bool,
) -> Result<RmOutcome> {
    if args.paths.is_empty() && args.pathspec_from_file.is_none() {
        bail!("you must specify path(s) to remove");
    }
    let interrupt = crate::interrupt::InterruptCancel::install();
    let repo = context::discover()?;
    let path_display = PathDisplayOptions::from_repo(&repo).ok();
    let resolved = args.resolve(&repo)?;
    let outcome = rm(
        &repo,
        resolved,
        cached,
        force,
        dry_run,
        ignore_unmatch,
        &interrupt,
        path_display.as_ref(),
    )?;
    Ok(RmOutcome {
        removed: outcome.removed,
        dry_run,
        path_display,
    })
}

#[allow(clippy::too_many_arguments)]
fn rm(
    repo: &Repository,
    resolved: ResolvedPathspecs,
    cached: bool,
    force: bool,
    dry_run: bool,
    ignore_unmatch: bool,
    cancel: &dyn grit_lib::progress::Cancel,
    path_display: Option<&PathDisplayOptions>,
) -> Result<RemoveOutcome> {
    let opts = RemoveOptions {
        pathspecs: resolved.specs,
        pathspec_sources: resolved.sources,
        cached,
        force,
        recursive: false,
        dry_run,
        ignore_unmatch,
    };
    remove_paths(repo, &opts, cancel).map_err(|err| map_paths_error(err, cached, path_display))
}

fn map_paths_error(
    err: grit_lib::error::Error,
    cached: bool,
    path_display: Option<&PathDisplayOptions>,
) -> anyhow::Error {
    match err {
        grit_lib::error::Error::RemoveBlocked { paths } => {
            anyhow::anyhow!(blocked_message(&paths, cached, path_display))
        }
        other => crate::json_error::map_lib_error(other),
    }
}

fn blocked_message(
    paths: &[BlockedRemoval],
    cached: bool,
    path_display: Option<&PathDisplayOptions>,
) -> String {
    let format_path = |p: &str| {
        path_display
            .map(|opts| opts.format_path_for_human(p))
            .unwrap_or_else(|| p.to_owned())
    };
    let mut msg = String::from("not removed, these files have content that would be lost:\n");
    for blocked in paths {
        msg.push_str(&format!(
            "    {} ({})\n",
            format_path(&blocked.path),
            blocked.reason
        ));
    }
    let cached_would_help = !cached
        && paths
            .iter()
            .all(|blocked| blocked.reason.allowed_with_cached());
    msg.push_str(if cached_would_help {
        "(use --cached to stop tracking them and keep the files, or --force to remove them anyway)"
    } else {
        "(use --force to remove them anyway)"
    });
    msg
}
