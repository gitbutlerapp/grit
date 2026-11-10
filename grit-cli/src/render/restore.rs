//! `grit restore` outcomes and rendering.

use std::collections::BTreeMap;
use std::io::Write;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::outln;
use crate::output::OutputMode;
use crate::render::clone::render_path_collision_warning;
use crate::render::list::format_inline_code;
use crate::render::{HumanRender, MarkdownRender, RenderContext};
use crate::ui::PathDisplayOptions;

/// CLI result (mirrors library restore outcomes).
#[derive(Serialize, JsonSchema, Clone)]
pub struct RestoreCliOutcome {
    pub restored: Vec<String>,
    pub removed: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path_collisions: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub restored_hunks: BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub restored_lines: BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dry_run: bool,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) path_display: Option<PathDisplayOptions>,
}

impl RestoreCliOutcome {
    pub(crate) fn with_path_display(mut self, path_display: Option<PathDisplayOptions>) -> Self {
        self.path_display = path_display;
        self
    }

    fn format_path(&self, path: &str) -> String {
        self.path_display
            .as_ref()
            .map(|opts| opts.format_path_for_human(path))
            .unwrap_or_else(|| path.to_owned())
    }
}

impl HumanRender for RestoreCliOutcome {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        let prefix = if self.dry_run {
            "Would restore"
        } else {
            "Restored"
        };
        for path in &self.restored {
            outln!(w, "{prefix} {}", self.format_path(path));
        }
        for path in &self.removed {
            outln!(w, "Removed {}", self.format_path(path));
        }
        render_path_collision_warning(w, ctx.mode, &self.path_collisions)?;
        Ok(())
    }
}

impl MarkdownRender for RestoreCliOutcome {
    fn render_markdown(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Restore");
        if self.restored.is_empty() && self.removed.is_empty() && self.path_collisions.is_empty() {
            outln!(w, "\nNo paths changed.");
            return Ok(());
        }
        if !self.restored.is_empty() {
            outln!(w, "\n### Restored");
            for path in &self.restored {
                let shown = format_inline_code(&self.format_path(path));
                outln!(w, "- {shown}");
            }
        }
        if !self.removed.is_empty() {
            outln!(w, "\n### Removed");
            for path in &self.removed {
                let shown = format_inline_code(&self.format_path(path));
                outln!(w, "- {shown}");
            }
        }
        if !self.path_collisions.is_empty() && ctx.mode == OutputMode::Markdown {
            outln!(w, "\n### Path collisions");
            outln!(
                w,
                "> Case-insensitive paths share one dentry; only one spelling exists on disk."
            );
            for path in &self.path_collisions {
                let shown = format_inline_code(path);
                outln!(w, "- {shown}");
            }
        }
        Ok(())
    }
}
