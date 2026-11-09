//! `grit merge-base` — find common ancestors or test reachability.

use anyhow::Result;
use grit_lib::revwalk::{MergeBaseMode, ResolveOptions};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::context;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// Result of `grit merge-base`.
#[derive(Serialize, JsonSchema)]
pub struct MergeBaseOutcome {
    /// Merge-base object ids (`--all` may return more than one).
    pub merge_bases: Vec<String>,
    /// Set when `--is-ancestor` is used.
    pub is_ancestor: Option<bool>,
}

impl HumanRender for MergeBaseOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.is_ancestor.is_some() {
            return Ok(());
        }
        for oid in &self.merge_bases {
            outln!(w, "{oid}");
        }
        Ok(())
    }
}

impl MarkdownRender for MergeBaseOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if let Some(is_anc) = self.is_ancestor {
            outln!(w, "# Ancestor check");
            outln!(w);
            outln!(
                w,
                "First revision **{}** an ancestor of the second.",
                if is_anc { "is" } else { "is not" }
            );
            return Ok(());
        }
        outln!(w, "# Merge bases");
        outln!(w);
        if self.merge_bases.is_empty() {
            outln!(w, "_No merge base._");
        } else {
            for oid in &self.merge_bases {
                outln!(w, "- `{oid}`");
            }
        }
        Ok(())
    }
}

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
    use crate::context;
    use crate::dispatch::{
        emit_rebase_outcome, emit_success, emit_success_quiet, finish_hunk_error, is_broken_pipe,
        CommandEnv,
    };
    use crate::exit_status::ExitStatus;
    use crate::json_error;
    use crate::message_input;
    use crate::output::{self, emit, OutputMode, OutputOptions, RenderContext};
    use crate::pager;
    use crate::patch_selection;
    use crate::progress;
    use crate::quiet_args;
    use crate::stdio;
    use anyhow::{bail, Result};
    let CommandEnv {
        opts,
        ctx,
        process_env,
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
        emit_success(outcome, opts, ctx)?;
        Ok(status)
    }
}
