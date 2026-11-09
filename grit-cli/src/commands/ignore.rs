//! `grit ignore check` — query whether paths match exclude rules.

use anyhow::{bail, Context, Result};
use grit_lib::worktree::ignore::{IgnoreMatch, IgnoreSource, RepoPathNormalizer};

use crate::context;
use crate::render::ignore::{IgnoreCheckEntryJson, IgnoreMatchJson, IgnoreSourceJson};

pub use crate::render::ignore::IgnoreCheckOutcome;

impl From<IgnoreSource> for IgnoreSourceJson {
    fn from(value: IgnoreSource) -> Self {
        match value {
            IgnoreSource::PerDirectory(path) => Self::PerDirectory { path },
            IgnoreSource::InfoExclude => Self::InfoExclude,
            IgnoreSource::CoreExcludesFile(path) => Self::CoreExcludesFile { path },
        }
    }
}

impl From<IgnoreMatch> for IgnoreMatchJson {
    fn from(m: IgnoreMatch) -> Self {
        Self {
            source: m.source.into(),
            line_number: m.line_number,
            pattern_text: m.pattern_text,
            negative: m.negative,
        }
    }
}

pub fn run(paths: &[String], all: bool) -> Result<IgnoreCheckOutcome> {
    if paths.is_empty() {
        bail!("at least one path is required");
    }
    let repo = context::discover()?;
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    let index = repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let normalizer = RepoPathNormalizer::new(&repo, &cwd).map_err(|e| anyhow::anyhow!("{e}"))?;
    let repo_paths = paths
        .iter()
        .map(|path| normalizer.normalize(path))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let checks = repo
        .ignore_check(&repo_paths, &index)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let any_ignored = checks.iter().any(|c| c.ignored);
    let entries: Vec<IgnoreCheckEntryJson> = checks
        .into_iter()
        .filter(|c| all || c.ignored)
        .map(|c| IgnoreCheckEntryJson {
            path: c.path,
            ignored: c.ignored,
            matched: c.matched.map(Into::into),
        })
        .collect();
    Ok(IgnoreCheckOutcome {
        any_ignored,
        entries,
    })
}

/// Exit **0** when at least one path is ignored, **1** when none are.
pub fn exit_code(outcome: &IgnoreCheckOutcome) -> i32 {
    if outcome.any_ignored {
        0
    } else {
        1
    }
}

/// Dispatch parsed CLI args for `Ignore`.
pub fn dispatch(
    args: crate::args::IgnoreArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;

    use crate::dispatch::{emit_success, CommandEnv};
    use crate::exit_status::ExitStatus;

    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let IgnoreArgs { action } = args;
    match action {
        IgnoreAction::Check { paths, all } => {
            let outcome = commands::ignore::run(&paths, all)?;
            let status = ExitStatus::from_ignore_check(&outcome);
            emit_success(outcome, opts, ctx)?;
            Ok(status)
        }
    }
}
