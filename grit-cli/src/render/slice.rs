//! JSON outcome types and rendering for `grit slice`.

use std::io::Write;

use anyhow::Result;
use grit_lib::objects::short_hex;
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::outln;
use crate::render::{HumanRender, MarkdownRender, RenderContext};

/// A slice as `grit slice` reports it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SliceJson {
    /// The slice's name.
    pub name: String,
    /// What it is for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Its paths.
    pub paths: Vec<String>,
}

/// Result of `grit slice list`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SliceListOutcome {
    /// The server repository, with any credentials removed.
    pub url: String,
    /// The target branch the slices are on.
    pub target: String,
    /// Where it is.
    pub head: String,
    /// The slices.
    pub slices: Vec<SliceJson>,
}

impl HumanRender for SliceListOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.slices.is_empty() {
            outln!(
                w,
                "{} has no slices yet; make one with `grit slice new <name> --path <dir>`.",
                self.target
            );
            return Ok(());
        }
        let width = self.slices.iter().map(|s| s.name.len()).max().unwrap_or(0);
        for s in &self.slices {
            outln!(w, "{:<width$}  {}", s.name, s.paths.join(", "));
            if let Some(d) = &s.description {
                outln!(w, "{:<width$}  {d}", "");
            }
        }
        Ok(())
    }
}

/// Result of `grit slice new`, `edit` and `delete`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SliceChangeOutcome {
    /// `new`, `edit` or `delete`.
    pub action: String,
    /// The slice's name.
    pub name: String,
    /// The slice as it is now; omitted once deleted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slice: Option<SliceJson>,
    /// The target branch.
    pub target: String,
    /// Where it was.
    pub previous: String,
    /// Where it is: the server's commit changing the slices file.
    pub head: String,
}

impl HumanRender for SliceChangeOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let verb = match self.action.as_str() {
            "new" => "Added",
            "edit" => "Changed",
            _ => "Removed",
        };
        let short = |s: &str| short_hex(s).to_owned();
        if self.previous == self.head {
            outln!(w, "· slice {} is unchanged", self.name);
        } else {
            outln!(
                w,
                "✓ {verb} slice {} on {}: {} → {}",
                self.name,
                self.target,
                short(&self.previous),
                short(&self.head)
            );
        }
        if let Some(s) = &self.slice {
            outln!(w, "  {}", s.paths.join(", "));
        }
        Ok(())
    }
}

impl MarkdownRender for SliceChangeOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Slice `{}` ({})", self.name, self.action);
        outln!(w);
        outln!(
            w,
            "- **target**: `{}` `{}` → `{}`",
            self.target,
            self.previous,
            self.head
        );
        if let Some(s) = &self.slice {
            let paths: Vec<String> = s.paths.iter().map(|p| format!("`{p}`")).collect();
            outln!(w, "- **paths**: {}", paths.join(", "));
        }
        Ok(())
    }
}

impl MarkdownRender for SliceListOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Slices on `{}`", self.target);
        outln!(w);
        if self.slices.is_empty() {
            outln!(w, "No slices.");
            return Ok(());
        }
        outln!(w, "| Slice | Paths | Description |");
        outln!(w, "| --- | --- | --- |");
        for s in &self.slices {
            let paths: Vec<String> = s.paths.iter().map(|p| format!("`{p}`")).collect();
            outln!(
                w,
                "| `{}` | {} | {} |",
                s.name,
                paths.join(", "),
                s.description.as_deref().unwrap_or_default()
            );
        }
        Ok(())
    }
}
