//! `grit mv` outcomes and rendering.

use std::io::Write;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::outln;
use crate::render::list::format_inline_code;
use crate::render::{HumanRender, MarkdownRender, RenderContext};
use crate::ui::PathDisplayOptions;

/// One path rename in `grit mv` JSON output.
#[derive(Serialize, JsonSchema, Clone)]
pub struct PathMoveJson {
    /// Source path before the move.
    pub from: String,
    /// Destination path after the move.
    pub to: String,
}

/// CLI outcome for `grit mv`.
#[derive(Serialize, JsonSchema)]
pub struct MvOutcome {
    /// Individual renames performed or previewed.
    pub moves: Vec<PathMoveJson>,
    /// Whether this run only previewed moves.
    pub dry_run: bool,
    #[serde(skip)]
    #[schemars(skip)]
    path_display: Option<PathDisplayOptions>,
}

impl MvOutcome {
    pub(crate) fn new(
        moves: Vec<PathMoveJson>,
        dry_run: bool,
        path_display: Option<PathDisplayOptions>,
    ) -> Self {
        Self {
            moves,
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

impl HumanRender for MvOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let prefix = if self.dry_run {
            "Would rename"
        } else {
            "Renamed"
        };
        if self.moves.is_empty() {
            return Ok(());
        }
        if self.moves.len() == 1 {
            let mv = &self.moves[0];
            outln!(
                w,
                "{prefix} {} -> {}.",
                self.format_path(&mv.from),
                self.format_path(&mv.to)
            );
            return Ok(());
        }
        for mv in &self.moves {
            outln!(
                w,
                "{prefix} {} -> {}.",
                self.format_path(&mv.from),
                self.format_path(&mv.to)
            );
        }
        Ok(())
    }
}

impl MarkdownRender for MvOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.moves.is_empty() {
            outln!(w, "- **moves**: _(none)_");
            return Ok(());
        }
        outln!(w, "- **moves**:");
        for mv in &self.moves {
            let from = format_inline_code(&self.format_path(&mv.from));
            let to = format_inline_code(&self.format_path(&mv.to));
            outln!(w, "  - {from} → {to}");
        }
        Ok(())
    }
}
