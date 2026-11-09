//! `grit merge-base` — find common ancestors or test reachability.

use anyhow::Result;
use grit_lib::revwalk::{MergeBaseMode, ResolveOptions};

use crate::context;

pub use crate::render::merge_base::MergeBaseOutcome;

pub fn run(a: &str, b: &str, all: bool, is_ancestor: bool) -> Result<MergeBaseOutcome> {
    let repo = context::discover()?;
    let walk = repo.revwalk();
    let commit = |spec: &str| walk.peel_to_commit(walk.resolve(spec, &ResolveOptions::default())?);
    let (a, b) = (commit(a)?, commit(b)?);
    if is_ancestor {
        let ok = walk.is_ancestor(a, b)?;
        return Ok(MergeBaseOutcome {
            merge_bases: Vec::new(),
            is_ancestor: Some(ok),
        });
    }
    let mode = if all {
        MergeBaseMode::All
    } else {
        MergeBaseMode::Best
    };
    let bases = walk.merge_bases(&[a, b], mode)?;
    Ok(MergeBaseOutcome {
        merge_bases: bases.into_iter().map(|oid| oid.to_hex()).collect(),
        is_ancestor: None,
    })
}

/// Exit code for `--is-ancestor`: 0 when true, 1 when false.
#[must_use]
pub fn exit_code(outcome: &MergeBaseOutcome) -> i32 {
    match outcome.is_ancestor {
        Some(true) => 0,
        Some(false) => 1,
        None => 0,
    }
}

/// Dispatch parsed CLI args for `MergeBase`.
pub fn dispatch(
    args: crate::args::MergeBaseArgs,
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
    let MergeBaseArgs {
        a,
        b,
        all,
        is_ancestor,
    } = args;
    {
        let outcome = commands::merge_base_cmd::run(&a, &b, all, is_ancestor)?;
        let status = if is_ancestor {
            ExitStatus::from_merge_base(&outcome)
        } else {
            ExitStatus::Success
        };
        emit_success(outcome, opts, ctx).map(|_| status)
    }
}
