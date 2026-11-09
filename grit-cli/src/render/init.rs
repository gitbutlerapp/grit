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
    /// `true` when a new repository was created.
    pub initialized: bool,
    /// `true` when an existing repository was reinitialized in place.
    #[serde(default, skip_serializing_if = "is_false")]
    #[schemars(default)]
    pub reinitialized: bool,
    /// The created `.git` directory.
    pub path: String,
    pub bare: bool,
    pub branch: String,
    /// Ref storage backend (`files` or `reftable`).
    pub ref_format: String,
    /// Object hash algorithm (`sha1` or `sha256`).
    pub object_format: String,
    /// Separate git directory when `--separate-git-dir` was used (omitted when co-located).
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
