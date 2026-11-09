//! `grit rm` outcomes and rendering.

use std::io::Write;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::outln;
use crate::render::list::format_inline_code;
use crate::render::{HumanRender, MarkdownRender, RenderContext};
use crate::ui::PathDisplayOptions;

/// CLI outcome for `grit rm`.
#[derive(Serialize, JsonSchema)]
pub struct RmOutcome {
    /// Paths removed from the index (and work tree unless `--cached`).
    pub removed: Vec<String>,
    /// Whether this run only previewed removals.
    pub dry_run: bool,
    #[serde(skip)]
    #[schemars(skip)]
    path_display: Option<PathDisplayOptions>,
}

impl RmOutcome {
    pub(crate) fn new(
        removed: Vec<String>,
        dry_run: bool,
        path_display: Option<PathDisplayOptions>,
    ) -> Self {
        Self {
            removed,
            dry_run,
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

impl HumanRender for RmOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let prefix = if self.dry_run {
            "Would remove"
        } else {
            "Removed"
        };
        match self.removed.len() {
            0 => {}
            1 => outln!(w, "{prefix} {}.", self.format_path(&self.removed[0])),
            n => outln!(w, "{prefix} {n} paths."),
        }
        Ok(())
    }
}

impl MarkdownRender for RmOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let heading = if self.dry_run {
            "Would remove"
        } else {
            "Removed"
        };
        if self.removed.is_empty() {
            outln!(w, "- **{heading}**: _(none)_");
            return Ok(());
        }
        outln!(w, "- **{heading}**:");
        for path in &self.removed {
            let shown = format_inline_code(&self.format_path(path));
            outln!(w, "  - {shown}");
        }
        Ok(())
    }
}
