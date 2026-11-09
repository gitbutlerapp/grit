//! Render `grit skill` outcomes.

use std::io::Write;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::{HumanRender, MarkdownRender, RenderContext};
use crate::stdio;

/// Result of `grit skill`.
#[derive(Serialize, JsonSchema)]
pub struct SkillOutcome {
    pub name: String,
    pub version: String,
    pub path: String,
    pub content: String,
}

impl HumanRender for SkillOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        write_skill_content(w, &self.content)
    }
}

impl MarkdownRender for SkillOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        write_skill_content(w, &self.content)
    }
}

fn write_skill_content(w: &mut dyn Write, content: &str) -> Result<()> {
    stdio::io_result(w.write_all(content.as_bytes()))
        .map_err(|e| anyhow::anyhow!("writing output: {e}"))
}
