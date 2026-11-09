//! `grit reset` — move the branch tip and optionally the index or worktree.

use anyhow::{Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::error::Error;
use grit_lib::error::ObjectError;
use grit_lib::error::RefError;
use grit_lib::porcelain::reset::{ResetMode, ResetOptions, ResetOutcome, DEFAULT_ORPHAN_LIMIT};
use grit_lib::revwalk::{IndexDwim, Peel, ResolveOptions};

use crate::context;

pub use crate::render::reset::{ResetCliMode, ResetCliOutcome};

impl From<ResetMode> for ResetCliMode {
    fn from(mode: ResetMode) -> Self {
        match mode {
            ResetMode::Soft => Self::Soft,
            ResetMode::Mixed => Self::Mixed,
            ResetMode::Hard => Self::Hard,
            ResetMode::Keep => Self::Keep,
        }
    }
}

impl From<ResetOutcome> for ResetCliOutcome {
    fn from(value: ResetOutcome) -> Self {
        Self {
            mode: value.mode.into(),
            target: value.target.to_hex(),
            previous: value.previous.to_hex(),
            r#ref: value.ref_updated,
            dry_run: value.dry_run,
            would_overwrite: value.would_overwrite,
            would_block: value.would_block,
            orphaned_commits: value.orphaned_commits.into_iter().map(Into::into).collect(),
            orphaned_total: value.orphaned_total,
            orphaned_truncated: value.orphaned_truncated,
        }
    }
}

/// `--mixed` is the default mode; the flag is accepted so Git habits keep working.
pub fn run(
    rev: Option<String>,
    soft: bool,
    _mixed: bool,
    hard: bool,
    keep: bool,
    dry_run: bool,
    orphan_limit: Option<usize>,
) -> Result<ResetCliOutcome> {
    let mode = if soft {
        ResetMode::Soft
    } else if hard {
        ResetMode::Hard
    } else if keep {
        ResetMode::Keep
    } else {
        ResetMode::Mixed
    };

    let repo = context::discover()?;
    let spec = rev.unwrap_or_else(|| "HEAD".to_owned());
    let target = repo
        .revwalk()
        .resolve(
            &spec,
            &ResolveOptions::default()
                .with_peel(Peel::Commit)
                .with_index_dwim(IndexDwim::Off),
        )
        .map_err(|err| map_reset_committish_error(err, &spec))?;

    let env = repo.environment();
    let config = ConfigSet::load(
        env,
        Some(repo.git_dir()),
        &grit_lib::config::LoadConfigOptions::standard(env, Some(repo.git_dir())),
    )
    .context("could not load config")?;
    let now = context::wall_clock_now(env);
    let identity = context::committer_reflog_signature(&repo, &config, now)?;

    let message = format!("reset: moving to {spec}");
    let mut opts = ResetOptions::new(target, mode, identity, message);
    opts.dry_run = dry_run;
    opts.orphan_limit = orphan_limit.unwrap_or(DEFAULT_ORPHAN_LIMIT);

    let _interrupt = crate::interrupt::InterruptCancel::install();
    let cancel = crate::interrupt::process_cancel_token();
    let outcome = repo
        .checkout()
        .reset(&opts, &mut grit_lib::progress::NullProgress, &cancel)
        .map_err(anyhow::Error::new)?;

    Ok(outcome.into())
}

fn map_reset_committish_error(err: Error, spec: &str) -> anyhow::Error {
    match err {
        Error::Ref(RefError::Invalid { detail: msg }) => anyhow::anyhow!(msg),
        Error::Object(ObjectError::NotFound { name: msg }) => {
            crate::json_error::structured_revision_not_found(format!(
                "invalid revision '{spec}': {msg}"
            ))
        }
        other => anyhow::Error::new(other),
    }
}

/// Dispatch parsed CLI args for `Reset`.
pub fn dispatch(
    args: crate::args::ResetArgs,
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
    let ResetArgs {
        rev,
        soft,
        mixed,
        hard,
        keep,
        dry_run,
        orphan_limit,
    } = args;
    emit_success(
        commands::reset::run(rev, soft, mixed, hard, keep, dry_run, orphan_limit)?,
        opts,
        ctx,
    )
}
