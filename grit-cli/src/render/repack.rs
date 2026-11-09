//! Render `grit repack` outcomes.

use std::io::Write;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// Result of `grit repack`.
#[derive(Serialize, JsonSchema)]
pub struct RepackOutcome {
    #[serde(skip)]
    pub quiet: bool,
    pub dry_run: bool,
    pub loose_before: usize,
    pub packs_before: usize,
    pub objects_packed: Option<usize>,
    pub pack_bytes: Option<u64>,
    pub pack_path: Option<String>,
    pub removed_pack_files: usize,
    pub pruned_loose: usize,
}

impl HumanRender for RepackOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.quiet {
            return Ok(());
        }
        if self.dry_run {
            outln!(
                w,
                "Would pack {} objects ({} loose, {} pack files now).",
                self.objects_packed.unwrap_or(0),
                self.loose_before,
                self.packs_before
            );
            return Ok(());
        }
        if let Some(path) = &self.pack_path {
            outln!(
                w,
                "Packed {} objects into {} ({} redundant pack files removed, {} loose objects pruned).",
                self.objects_packed.unwrap_or(0),
                path,
                self.removed_pack_files,
                self.pruned_loose
            );
        } else {
            outln!(w, "Nothing to repack.");
        }
        Ok(())
    }
}

impl MarkdownRender for RepackOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Repack\n");
        outln!(w, "- **dry_run:** {}", self.dry_run);
        outln!(w, "- **loose_before:** {}", self.loose_before);
        outln!(w, "- **packs_before:** {}", self.packs_before);
        if let Some(n) = self.objects_packed {
            outln!(w, "- **objects_packed:** {n}");
        }
        if let Some(bytes) = self.pack_bytes {
            outln!(w, "- **pack_bytes:** {bytes}");
        }
        if let Some(path) = &self.pack_path {
            outln!(w, "- **pack_path:** `{path}`");
        }
        outln!(w, "- **removed_pack_files:** {}", self.removed_pack_files);
        outln!(w, "- **pruned_loose:** {}", self.pruned_loose);
        Ok(())
    }
}
