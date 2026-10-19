//! `grit add` — stage changes. With no paths, stages everything.
//!
//! Staging is delegated to [`grit_lib::porcelain::add::stage`] so the CLI stays
//! a thin wrapper over the library.

use anyhow::{bail, Context, Result};
use grit_lib::porcelain::add::{stage as stage_paths, StageOptions};
use grit_lib::porcelain::index_mode::StageExecutableHint;
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::worktree::pathspec::{pathdiff, resolve_pathspec_in_worktree_with_git_dir};
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

pub fn run(paths: &[String], executable: bool, non_executable: bool) -> Result<AddOutcome> {
    if executable && non_executable {
        bail!("--executable and --non-executable cannot be used together");
    }
    if (executable || non_executable) && paths.is_empty() {
        bail!("--executable and --non-executable require at least one path");
    }
    let repo = context::discover()?;
    let staged = stage_with_hint(
        &repo,
        paths,
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
        no_paths: paths.is_empty(),
    })
}

fn stage_with_hint(
    repo: &Repository,
    selectors: &[String],
    executable_hint: StageExecutableHint,
) -> Result<usize> {
    let work_tree = repo.work_tree().context("grit add needs a working tree")?;
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    let prefix = pathdiff(&cwd, work_tree);

    let (pathspecs, pathspec_sources) = if selectors.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        let mut specs = Vec::with_capacity(selectors.len());
        for sel in selectors {
            if sel.is_empty() {
                bail!("invalid path ''");
            }
            let resolved = resolve_pathspec_in_worktree_with_git_dir(
                sel,
                sel,
                work_tree,
                prefix.as_deref(),
                Some(repo.git_dir()),
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?;
            specs.push(resolved);
        }
        (specs, selectors.to_vec())
    };

    let opts = if pathspecs.is_empty() {
        StageOptions::all().with_executable_hint(executable_hint)
    } else {
        StageOptions::pathspecs_with_sources(pathspecs, pathspec_sources)
            .with_executable_hint(executable_hint)
    };
    let outcome =
        stage_paths(repo, &opts, &mut NullProgress).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(outcome.total())
}
