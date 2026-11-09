//! JSON outcome types and rendering for `grit update`.

use std::io::Write;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use crate::markdown;
use crate::render::{HumanRender, MarkdownRender, RenderContext};

/// Result of `grit update`.
#[derive(Serialize, JsonSchema)]
pub struct UpdateOutcome {
    /// Whether the installer ran successfully.
    pub updated: bool,
    /// The version of `grit` that ran the update (the new version is whatever the
    /// installer fetched).
    pub version: String,
}

impl HumanRender for UpdateOutcome {
    fn render_human(&self, _w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        Ok(())
    }
}

impl MarkdownRender for UpdateOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_update_outcome(w, self)?;
        Ok(())
    }
}
