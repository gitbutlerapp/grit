//! Outcome types and renderers for `grit merge-base`.

use std::io::Write;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

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
