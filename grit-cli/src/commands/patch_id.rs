//! `grit patch-id` — stable patch-IDs for commits.

use anyhow::{bail, Result};
use grit_lib::diff::patch_ids::PatchIdMode;

use crate::context;

pub use crate::render::patch_id::{PatchIdLine, PatchIdOutcome};

/// Resolve revisions (expanding ranges) and compute patch-IDs (stable by default).
pub fn run(revs: &[String], unstable: bool) -> Result<PatchIdOutcome> {
    if revs.is_empty() {
        bail!("specify at least one revision");
    }
    let mode = if unstable {
        PatchIdMode::Unstable
    } else {
        PatchIdMode::Stable
    };
    let repo = context::discover()?;
    let specs: Vec<&str> = revs.iter().map(String::as_str).collect();
    let entries = repo
        .patch_ids_for_revs(&specs, mode)?
        .into_iter()
        .map(|(patch_id, commit)| PatchIdLine {
            patch_id: patch_id.to_hex(),
            commit: commit.to_hex(),
        })
        .collect();
    Ok(PatchIdOutcome { entries })
}

/// Dispatch parsed CLI args for `PatchId`.
pub fn dispatch(
    args: crate::args::PatchIdArgs,
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
    let PatchIdArgs { revs, unstable } = args;
    {
        emit_success(commands::patch_id::run(&revs, unstable)?, opts, ctx)
    }
}
