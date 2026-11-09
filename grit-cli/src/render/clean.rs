//! `grit clean` outcomes and rendering.

use std::io::Write;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::outln;
use crate::render::list::format_inline_code;
use crate::render::{HumanRender, MarkdownRender, RenderContext};
use crate::ui::PathDisplayOptions;

/// Result of `grit clean`.
#[derive(Serialize, JsonSchema)]
pub struct CleanOutcome {
    /// Paths removed, or that would be removed when dry-run.
    pub removed: Vec<String>,
    /// Whether this run only previewed removals (no `-f`).
    pub dry_run: bool,
    /// Which paths were eligible for removal.
    pub scope: String,
    #[serde(skip)]
    #[schemars(skip)]
    path_display: Option<PathDisplayOptions>,
}

impl CleanOutcome {
    pub(crate) fn new(
        removed: Vec<String>,
        dry_run: bool,
        scope: String,
        path_display: Option<PathDisplayOptions>,
    ) -> Self {
        Self {
            removed,
            dry_run,
            scope,
            path_display,
        }
    }

    fn format_path(&self, path: &str) -> String {
        self.path_display
            .as_ref()
            .map(|opts| opts.format_path_for_human(path))
            .unwrap_or_else(|| path.to_owned())
    }
}

impl HumanRender for CleanOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.removed.is_empty() {
            if self.dry_run {
                outln!(w, "Nothing to clean.");
            } else {
                outln!(w, "Removed nothing.");
            }
            return Ok(());
        }
        let prefix = if self.dry_run {
            "Would remove"
        } else {
            "Removed"
        };
        for path in &self.removed {
            outln!(w, "{prefix} {}", self.format_path(path));
        }
        Ok(())
    }
}

impl MarkdownRender for CleanOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.removed.is_empty() {
            outln!(w, "No paths to remove.");
            return Ok(());
        }
        let heading = if self.dry_run {
            "Would remove"
        } else {
            "Removed"
        };
        outln!(w, "## {heading}\n");
        for path in &self.removed {
            let shown = format_inline_code(&self.format_path(path));
            outln!(w, "- {shown}");
        }
        Ok(())
    }
}
