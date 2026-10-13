//! `grit patch-id` — stable patch-IDs for commits.

use std::io::Write;

use anyhow::{bail, Result};
use grit_lib::diff::patch_ids::PatchIdMode;
use grit_lib::revwalk::rev_parse::resolve_revision_without_index_dwim;
use schemars::JsonSchema;
use serde::Serialize;

use crate::context;
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

/// Resolve revisions and compute patch-IDs (stable by default).
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
    let mut entries = Vec::new();
    for rev in revs {
        let oid = resolve_revision_without_index_dwim(&repo, rev)
            .map_err(|e| anyhow::anyhow!("{rev}: {e}"))?;
        let Some(patch_id) = repo
            .patch_id(&oid, mode)
            .map_err(|e| anyhow::anyhow!("{e}"))?
        else {
            continue;
        };
        entries.push(PatchIdLine {
            patch_id: patch_id.to_hex(),
            commit: oid.to_hex(),
        });
    }
    Ok(PatchIdOutcome { entries })
}
