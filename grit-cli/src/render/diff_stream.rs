//! Streamed rendering for per-file diffs (human and Markdown).

use std::io::Write;

use anyhow::Result;

use super::diff::FileDiff;
use super::stream::StreamRender;
use super::RenderContext;
use crate::markdown;
use crate::render::diff::render_file;

/// Stream patch bodies one [`FileDiff`] at a time.
pub struct FileDiffStream {
    markdown_index: usize,
    path_display: Option<crate::ui::PathDisplayOptions>,
}

impl FileDiffStream {
    /// Build a renderer with optional path shortening rules.
    #[must_use]
    pub fn new(path_display: Option<crate::ui::PathDisplayOptions>) -> Self {
        Self {
            markdown_index: 0,
            path_display,
        }
    }
}

/// Trailing counts for a streamed diff document.
#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct FileDiffStreamSummary {
    /// Whether any file change was emitted.
    pub changed: bool,
}

impl StreamRender for FileDiffStream {
    type Item = FileDiff;
    type Summary = FileDiffStreamSummary;

    fn item(&mut self, w: &mut dyn Write, ctx: &RenderContext, value: &Self::Item) -> Result<()> {
        match ctx.mode {
            super::OutputMode::Human => {
                render_file(w, value, ctx.color, self.path_display.as_ref())?;
            }
            super::OutputMode::Markdown => {
                markdown::print_diff_file(
                    w,
                    value,
                    self.path_display.as_ref(),
                    self.markdown_index > 0,
                )?;
            }
            super::OutputMode::Json => {}
        }
        self.markdown_index += 1;
        Ok(())
    }

    fn finish(
        &mut self,
        w: &mut dyn Write,
        ctx: &RenderContext,
        summary: &Self::Summary,
    ) -> Result<()> {
        if !summary.changed && matches!(ctx.mode, super::OutputMode::Markdown) {
            crate::output::outln!(w, "No changes.");
        }
        Ok(())
    }
}
