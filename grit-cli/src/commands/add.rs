//! `grit add` — stage changes. With no paths, stages everything.
//!
//! Staging is delegated to [`grit_lib::porcelain::add::stage`] so the CLI stays
//! a thin wrapper over the library.

use anyhow::{bail, Context, Result};
use grit_lib::pathspec::{pathdiff, resolve_pathspec_in_worktree};
use grit_lib::porcelain::add::{stage as stage_paths, StageMode, StageOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use serde::Serialize;

use crate::context;
use crate::output::HumanRender;

/// Result of `grit add`: how many changes were staged.
#[derive(Serialize)]
pub struct AddOutcome {
    pub staged: usize,
    /// Whether the invocation had no path arguments (stages everything).
    #[serde(skip)]
    no_paths: bool,
}

impl HumanRender for AddOutcome {
    fn render_human(&self) {
        match self.staged {
            0 if self.no_paths => println!("Nothing to stage — working tree clean."),
            0 => {}
            1 => println!("Staged 1 change."),
            n => println!("Staged {n} changes."),
        }
    }
}

pub fn run(paths: &[String]) -> Result<AddOutcome> {
    let repo = context::discover()?;
    let staged = stage(&repo, paths)?;
    Ok(AddOutcome {
        staged,
        no_paths: paths.is_empty(),
    })
}

/// Stage all changes matching `selectors` (empty selectors = everything).
///
/// Returns the number of paths staged. Shared with `grit commit -a`.
pub fn stage(repo: &Repository, selectors: &[String]) -> Result<usize> {
    let work_tree = repo
        .work_tree
        .clone()
        .context("grit add needs a working tree")?;
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    let prefix = pathdiff(&cwd, &work_tree);

    let (pathspecs, pathspec_sources) = if selectors.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        let mut specs = Vec::with_capacity(selectors.len());
        for sel in selectors {
            if sel.is_empty() {
                bail!("invalid path ''");
            }
            let resolved = resolve_pathspec_in_worktree(sel, sel, &work_tree, prefix.as_deref())
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            specs.push(resolved);
        }
        (specs, selectors.to_vec())
    };

    let opts = StageOptions {
        pathspecs,
        pathspec_sources,
        mode: StageMode::All,
    };
    let outcome =
        stage_paths(repo, &opts, &mut NullProgress).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(outcome.total())
}
