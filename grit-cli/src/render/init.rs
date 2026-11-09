//! Render `grit init` outcomes.

use std::io::Write;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

fn is_false(value: &bool) -> bool {
    !*value
}

/// Result of `grit init`.
#[derive(Serialize, JsonSchema)]
pub struct InitOutcome {
    pub initialized: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    #[schemars(default)]
    pub reinitialized: bool,
    pub path: String,
    pub bare: bool,
    pub branch: String,
    pub ref_format: String,
    pub object_format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub git_dir: Option<String>,
}

impl HumanRender for InitOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let kind = if self.bare {
            "bare repository"
        } else {
            "repository"
        };
        if self.reinitialized {
            outln!(
                w,
                "Reinitialized existing {kind} in {} (ref-format: {})",
                self.path,
                self.ref_format
            );
        } else {
            outln!(
                w,
                "Initialized empty {kind} in {} (ref-format: {})",
                self.path,
                self.ref_format
            );
        }
        Ok(())
    }
}

impl MarkdownRender for InitOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let kind = if self.bare {
            "bare repository"
        } else {
            "repository"
        };
        outln!(
            w,
            "Initialized empty {kind} at `{}` (default branch `{}`, ref-format `{}`, object format `{}`).",
            self.path,
            self.branch,
            self.ref_format,
            self.object_format
        );
        Ok(())
    }
}
