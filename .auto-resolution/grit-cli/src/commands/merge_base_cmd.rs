//! `grit merge-base` — find common ancestors or test reachability.

use anyhow::Result;
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
    let walk = repo.rev_walk();
    if is_ancestor {
        let ok = walk.is_ancestor(a, b)?;
        return Ok(MergeBaseOutcome {
            merge_bases: Vec::new(),
            is_ancestor: Some(ok),
        });
    }
    let bases = walk.merge_bases(a, b, all)?;
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
