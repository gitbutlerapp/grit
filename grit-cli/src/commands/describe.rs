//! `grit describe` — nearest tag and distance for a commit.

use anyhow::Result;
use grit_lib::revwalk::{DescribeOptions, DescribedCommit};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::context;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

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

/// Result of `grit describe`.
#[derive(Serialize, JsonSchema)]
pub struct DescribeOutcome {
    /// Full `refs/tags/…` name of the chosen tag.
    pub tag: String,
    /// Short tag name.
    pub tag_short: String,
    /// Object id stored at the tag ref.
    pub tag_oid: String,
    /// Commit the tag peel chain resolves to.
    pub peeled: String,
    /// Exact commit count in `peeled..commit` (see library docs).
    pub distance: usize,
    /// Described commit oid.
    pub commit: String,
    /// Abbreviated described commit oid.
    pub abbrev: String,
    /// Human long form: `tag-distance-gabbrev` (includes `-0-` on exact tag hits).
    pub long_format: String,
}

impl From<DescribedCommit> for DescribeOutcome {
    fn from(d: DescribedCommit) -> Self {
        let tag_short = d
            .tag
            .as_str()
            .strip_prefix("refs/tags/")
            .unwrap_or(d.tag.as_str())
            .to_owned();
        let long_format = format!("{}-{}-g{}", tag_short, d.distance, d.abbrev);
        Self {
            tag: d.tag.as_str().to_owned(),
            tag_short,
            tag_oid: d.tag_oid.to_hex(),
            peeled: d.peeled.to_hex(),
            distance: d.distance,
            commit: d.commit.to_hex(),
            abbrev: d.abbrev.clone(),
            long_format,
        }
    }
}

impl HumanRender for DescribeOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "{}", self.long_format);
        Ok(())
    }
}

impl MarkdownRender for DescribeOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "# Describe");
        outln!(w);
        outln!(w, "- **Tag:** `{}`", self.tag_short);
        outln!(w, "- **Distance:** {}", self.distance);
        outln!(w, "- **Commit:** `{}`", self.abbrev);
        outln!(w, "- **Long:** `{}`", self.long_format);
        Ok(())
    }
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
