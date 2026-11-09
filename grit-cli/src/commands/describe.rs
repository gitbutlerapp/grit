//! `grit describe` — nearest tag and distance for a commit.

use anyhow::Result;
use grit_lib::revwalk::DescribeOptions;

use crate::context;

pub use crate::render::describe::DescribeOutcome;

/// CLI flags for [`run`].
#[derive(Debug, clap::Args, Default)]
pub struct DescribeCliArgs {
    /// Commit or ref to describe (defaults to `HEAD`).
    #[arg(value_name = "REV", default_value = "HEAD")]
    pub rev: String,
    /// Include lightweight tags (default: annotated tags only).
    #[arg(long)]
    pub tags: bool,
    /// Only consider tags matching this glob (short name).
    #[arg(long = "match", value_name = "GLOB")]
    pub match_glob: Option<String>,
    /// Follow first-parent chains when counting distance after the tag.
    #[arg(long = "first-parent")]
    pub first_parent: bool,
    /// Minimum abbreviated commit id length (default follows `core.abbrev`).
    #[arg(long, value_name = "N")]
    pub abbrev: Option<usize>,
}

pub fn run(args: &DescribeCliArgs) -> Result<Option<DescribeOutcome>> {
    let repo = context::discover()?;
    let opts = DescribeOptions {
        tags: args.tags,
        match_glob: args.match_glob.clone(),
        first_parent: args.first_parent,
        abbrev: args.abbrev,
    };
    let described = repo.revwalk().describe(&args.rev, &opts)?;
    Ok(described.map(DescribeOutcome::from))
}

/// Dispatch parsed CLI args for `Describe`.
pub fn dispatch(
    args: crate::args::DescribeArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::commands;

    use crate::dispatch::{emit_success, CommandEnv};

    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let describe = args;
    match commands::describe::run(&describe)? {
        Some(outcome) => emit_success(outcome, opts, ctx),
        None => {
            anyhow::bail!(
                "cannot describe '{}': no annotated tag is reachable",
                describe.rev
            );
        }
    }
}
