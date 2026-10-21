//! `grit add` — stage changes. With no paths, stages everything.
//!
//! Staging is delegated to [`grit_lib::porcelain::add::stage`] so the CLI stays
//! a thin wrapper over the library.

use anyhow::{bail, Context, Result};
use grit_lib::error::Error as LibError;
use grit_lib::porcelain::add::{stage as stage_paths, StageMode, StageOptions, StageOutcome};
use grit_lib::porcelain::index_mode::StageExecutableHint;
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::worktree::pathspec::{pathdiff, resolve_pathspec_in_worktree_with_git_dir};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::context;
use crate::json_error;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

const HUMAN_PATH_LIMIT: usize = 10;

/// Result of `grit add`: paths staged or that would be staged.
#[derive(Serialize, JsonSchema)]
pub struct AddOutcome {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub removed: Vec<String>,
    pub dry_run: bool,
    /// Total paths touched (same as `staged`; kept for one release).
    pub count: usize,
    /// How many paths were staged (alias of `count` for scripts written before path lists).
    pub staged: usize,
    /// Whether the invocation had no path arguments (stages everything).
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) no_paths: bool,
}

impl AddOutcome {
    fn from_stage(outcome: StageOutcome, dry_run: bool, no_paths: bool) -> Self {
        let count = outcome.total();
        Self {
            added: outcome.added,
            modified: outcome.modified,
            removed: outcome.removed,
            dry_run,
            count,
            staged: count,
            no_paths,
        }
    }
}

impl HumanRender for AddOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let prefix = if self.dry_run {
            "Would stage"
        } else {
            "Staged"
        };
        let paths = self.all_paths();
        match paths.len() {
            0 if self.no_paths => outln!(w, "Nothing to stage — working tree clean."),
            0 => {}
            1 => outln!(w, "{prefix} {}", paths[0]),
            n => {
                for path in paths.iter().take(HUMAN_PATH_LIMIT) {
                    outln!(w, "{prefix} {path}");
                }
                if n > HUMAN_PATH_LIMIT {
                    outln!(w, "… and {} more", n - HUMAN_PATH_LIMIT);
                }
            }
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

impl AddOutcome {
    fn all_paths(&self) -> Vec<&str> {
        let mut out = Vec::new();
        out.extend(self.added.iter().map(String::as_str));
        out.extend(self.modified.iter().map(String::as_str));
        out.extend(self.removed.iter().map(String::as_str));
        out
    }
}

pub fn run(
    paths: &[String],
    update: bool,
    dry_run: bool,
    executable: bool,
    non_executable: bool,
) -> Result<AddOutcome> {
    if executable && non_executable {
        bail!("--executable and --non-executable cannot be used together");
    }
    if (executable || non_executable) && paths.is_empty() {
        bail!("--executable and --non-executable require at least one path");
    }
    let repo = context::discover()?;
    let outcome = stage_with_hint(
        &repo,
        paths,
        if update {
            StageMode::Update
        } else {
            StageMode::All
        },
        dry_run,
        if executable {
            StageExecutableHint::SetExecutable
        } else if non_executable {
            StageExecutableHint::SetNonExecutable
        } else {
            StageExecutableHint::None
        },
    )?;
    Ok(AddOutcome::from_stage(outcome, dry_run, paths.is_empty()))
}

fn stage_with_hint(
    repo: &Repository,
    selectors: &[String],
    mode: StageMode,
    dry_run: bool,
    executable_hint: StageExecutableHint,
) -> Result<StageOutcome> {
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

    let opts = StageOptions {
        pathspecs,
        pathspec_sources,
        mode,
        executable_hint,
        staging_thresholds: None,
        dry_run,
    };
    stage_paths(repo, &opts, &mut NullProgress).map_err(map_add_error)
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
            msg.push_str("(ignored paths cannot be staged; adjust ignore rules or use a future force option)");
            anyhow::anyhow!(msg)
        }
        other => json_error::map_lib_error(other),
    }
}
