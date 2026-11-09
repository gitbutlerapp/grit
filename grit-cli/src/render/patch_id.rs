//! Outcome types and renderers for `grit patch-id`.

use std::io::Write;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// One `(patch_id, commit)` pair (Git `patch-id` stdout format).
#[derive(Serialize, JsonSchema, Clone)]
pub struct PatchIdLine {
    pub patch_id: String,
    pub commit: String,
}

/// Result of `grit patch-id`.
#[derive(Serialize, JsonSchema)]
pub struct PatchIdOutcome {
    pub entries: Vec<PatchIdLine>,
}

impl HumanRender for PatchIdOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        for line in &self.entries {
            outln!(w, "{} {}", line.patch_id, line.commit);
        }
        Ok(())
    }
}

impl MarkdownRender for PatchIdOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Patch IDs\n");
        if self.entries.is_empty() {
            outln!(w, "No patch IDs (empty diffs or merge commits).");
            return Ok(());
        }
        outln!(w, "| Patch ID | Commit |");
        outln!(w, "| --- | --- |");
        for line in &self.entries {
            outln!(w, "| `{}` | `{}` |", line.patch_id, line.commit);
        }
        Ok(())
    }
}
