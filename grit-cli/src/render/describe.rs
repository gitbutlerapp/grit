//! `grit describe` outcomes and rendering.

use std::io::Write;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::outln;
use crate::render::{HumanRender, MarkdownRender, RenderContext};

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
