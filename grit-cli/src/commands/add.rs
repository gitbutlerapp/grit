//! `grit add` — stage changes. With no paths, stages everything.
//!
//! Staging is delegated to [`grit_lib::porcelain::add::stage`] so the CLI stays
//! a thin wrapper over the library.

use crate::pathspec_args::PathspecArgs;
use anyhow::{bail, Result};
use grit_lib::error::Error as LibError;
use grit_lib::pathspec::ResolvedPathspecs;
use grit_lib::porcelain::add::{stage as stage_paths, StageMode, StageOptions};
use grit_lib::porcelain::index_mode::StageExecutableHint;
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::context;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// Result of `grit add`: how many changes were staged.
#[derive(Serialize, JsonSchema)]
pub struct AddOutcome {
    pub staged: usize,
    /// Whether the invocation had no path arguments (stages everything).
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) no_paths: bool,
}

impl HumanRender for AddOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self.staged {
            0 if self.no_paths => outln!(w, "Nothing to stage — working tree clean."),
            0 => {}
            1 => outln!(w, "Staged 1 change."),
            n => outln!(w, "Staged {n} changes."),
        }
        Ok(())
    }
}

impl MarkdownRender for AddOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_add_outcome(w, self)?;
        Ok(())
    }
}

pub fn run(args: &PathspecArgs, executable: bool, non_executable: bool) -> Result<AddOutcome> {
    if executable && non_executable {
        bail!("--executable and --non-executable cannot be used together");
    }
    let repo = context::discover()?;
    let resolved = args.resolve(&repo)?;
    if (executable || non_executable) && resolved.is_entire_tree() {
        bail!("--executable and --non-executable require at least one path");
    }
    let staged = stage_with_hint(
        &repo,
        &resolved,
        if executable {
            StageExecutableHint::SetExecutable
        } else if non_executable {
            StageExecutableHint::SetNonExecutable
        } else {
            StageExecutableHint::None
        },
    )?;
    Ok(AddOutcome {
        staged,
        no_paths: resolved.is_entire_tree(),
    })
}

fn stage_with_hint(
    repo: &Repository,
    resolved: &ResolvedPathspecs,
    executable_hint: StageExecutableHint,
) -> Result<usize> {
    let (pathspecs, pathspec_sources) = if resolved.is_entire_tree() {
        (Vec::new(), Vec::new())
    } else {
        (resolved.specs.clone(), resolved.sources.clone())
    };

    let opts = StageOptions {
        pathspecs,
        pathspec_sources,
        mode: StageMode::All,
        executable_hint,
        staging_thresholds: None,
    };
    let outcome = stage_paths(repo, &opts, &mut NullProgress).map_err(map_add_error)?;
    Ok(outcome.total())
}

fn map_add_error(err: LibError) -> anyhow::Error {
    match err {
        LibError::PathspecIgnored { paths } => {
            let mut msg =
                String::from("the following paths are ignored by one of your ignore files:\n");
            for path in paths {
                msg.push_str("    ");
                msg.push_str(&path);
                msg.push('\n');
            }
            anyhow::anyhow!("{msg}")
        }
        other => anyhow::anyhow!("{other}"),
    }
}
