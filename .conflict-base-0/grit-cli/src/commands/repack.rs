//! `grit repack` — pack loose objects and remove redundant packs.

use std::io::Write;

use anyhow::Result;
use grit_lib::maintenance::{repack_all, RepackOptions, RepackStats};
use schemars::JsonSchema;
use serde::Serialize;

use crate::context;
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

impl From<(RepackStats, bool, bool)> for RepackOutcome {
    fn from((stats, dry_run, quiet): (RepackStats, bool, bool)) -> Self {
        Self {
            quiet,
            dry_run,
            loose_before: stats.loose_before,
            packs_before: stats.packs_before,
            objects_packed: stats.objects_packed,
            pack_bytes: stats.pack_bytes,
            pack_path: stats.pack_path.map(|p| p.display().to_string()),
            removed_pack_files: stats.removed_pack_files,
            pruned_loose: stats.pruned_loose,
        }
    }
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

pub fn run(dry_run: bool, quiet: bool) -> Result<RepackOutcome> {
    let repo = context::discover()?;
    let stats = repack_all(&repo, &RepackOptions::with_dry_run(dry_run))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok((stats, dry_run, quiet).into())
}
