//! `grit checkout` outcomes and rendering.

use crate::output::outln;
use crate::progress::{format_bytes, format_duration, paint, BOLD, DIM};
use crate::render::{HumanRender, MarkdownRender, RenderContext};
use anyhow::Result;
use grit_lib::objects::short_hex;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

/// Result of `grit checkout <url>`.
#[derive(Serialize, JsonSchema)]
pub struct CheckoutOutcome {
    /// The repository URL, configured as `origin`, with any credentials removed.
    pub url: String,
    /// Directory the checkout was made in.
    pub path: String,
    /// The server's target branch the snapshot was taken from.
    pub branch: String,
    /// The new branch checked out for your work.
    pub work: String,
    /// The real commit of the target branch the snapshot stands for.
    pub base: String,
    /// The snapshot commit (the checkout's only commit).
    pub snapshot: String,
    /// The paths checked out; empty for the whole tree.
    pub paths: Vec<String>,
    /// Files in the snapshot.
    pub files: u64,
    /// Bytes of those files.
    pub bytes: u64,
    /// Size of the download.
    pub download_bytes: u64,
    /// Bytes an earlier, interrupted checkout had already downloaded.
    pub resumed_from: u64,
    /// The server's id for the checkout.
    pub id: String,
    /// Wall-clock seconds.
    pub seconds: f64,
}

impl HumanRender for CheckoutOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let color = crate::progress::summary::stdout_color();
        let short = |s: &str| short_hex(s).to_owned();
        outln!(
            w,
            "{} Checked out {} on {} in {}",
            paint(color, "32", "✓"),
            paint(color, BOLD, &self.path),
            paint(color, BOLD, &self.work),
            format_duration(std::time::Duration::from_secs_f64(self.seconds)),
        );
        outln!(w);
        let paths = if self.paths.is_empty() {
            "the whole tree".to_owned()
        } else {
            self.paths.join(", ")
        };
        outln!(w, "  {}  {paths}", paint(color, DIM, "paths   "));
        outln!(
            w,
            "  {}  {} files, {} as a {} download{}",
            paint(color, DIM, "files   "),
            self.files,
            format_bytes(self.bytes),
            format_bytes(self.download_bytes),
            if self.resumed_from > 0 {
                format!(" (resumed at {})", format_bytes(self.resumed_from))
            } else {
                String::new()
            }
        );
        outln!(
            w,
            "  {}  one commit, {}, standing for {} at {}",
            paint(color, DIM, "history "),
            short(&self.snapshot),
            self.branch,
            short(&self.base),
        );
        outln!(w);
        outln!(
            w,
            "Commit on {} as usual; origin/{} is the snapshot.",
            self.work,
            self.branch
        );
        Ok(())
    }
}

impl MarkdownRender for CheckoutOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Checkout complete");
        outln!(w);
        outln!(w, "- **path**: `{}`", self.path);
        outln!(w, "- **work branch**: `{}`", self.work);
        outln!(w, "- **url**: `{}`", self.url);
        outln!(w, "- **target**: `{}` at `{}`", self.branch, self.base);
        outln!(w, "- **snapshot**: `{}`", self.snapshot);
        if self.paths.is_empty() {
            outln!(w, "- **paths**: the whole tree");
        } else {
            let paths: Vec<String> = self.paths.iter().map(|p| format!("`{p}`")).collect();
            outln!(w, "- **paths**: {}", paths.join(", "));
        }
        outln!(w, "- **files**: {} ({} bytes)", self.files, self.bytes);
        outln!(
            w,
            "- **download**: {} bytes (resumed at {})",
            self.download_bytes,
            self.resumed_from
        );
        Ok(())
    }
}
