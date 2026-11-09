//! `grit shortlog` outcomes and rendering.

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::diff_format::DiffFormatArgs;
use crate::log_commit::LogCommit;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::render::list::format_inline_code;
use crate::stdio;

/// Pre-rendered human commit row plus optional diff/stat tail.
#[derive(Clone)]
pub(crate) struct ShortlogHumanCommitLine {
    pub row: String,
    pub detail: Option<String>,
    pub blank_after_detail: bool,
}

/// Result of `grit shortlog`.
#[derive(Serialize, JsonSchema)]
pub struct ShortlogOutcome {
    pub branch: String,
    /// Target branch name, or `null` when none could be resolved.
    pub target: Option<String>,
    pub ahead: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub behind: Option<usize>,
    pub commits: Vec<LogCommit>,
    /// True when more ahead commits exist than listed (`ahead` exceeds `commits.len()`).
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub behind_commits: Option<Vec<LogCommit>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub behind_truncated: Option<bool>,
    /// Continuation cursor when the list is resumable; always `null` for `shortlog` today.
    pub next: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) human_ahead: Vec<ShortlogHumanCommitLine>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) human_behind: Vec<ShortlogHumanCommitLine>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) markdown_ahead_bullets: Vec<String>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) markdown_behind_bullets: Vec<String>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) markdown_ahead_diffs: String,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) markdown_behind_diffs: String,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) format: DiffFormatArgs,
}

impl ShortlogOutcome {
    fn render_human_commit_block(
        w: &mut dyn Write,
        lines: &[ShortlogHumanCommitLine],
    ) -> Result<()> {
        for line in lines {
            outln!(w, "{}", line.row);
            if let Some(detail) = &line.detail {
                stdio::io_result(write!(w, "{detail}"))
                    .map_err(|e| anyhow::anyhow!("writing output: {e}"))?;
                if line.blank_after_detail {
                    outln!(w);
                }
            }
        }
        Ok(())
    }

    fn print_markdown_commit_bullets(w: &mut dyn Write, bullets: &[String]) -> Result<()> {
        if bullets.is_empty() {
            outln!(w, "No commits.");
            return Ok(());
        }
        for bullet in bullets {
            outln!(w, "{bullet}");
        }
        Ok(())
    }

    fn print_markdown_diff_extra(w: &mut dyn Write, body: &str) -> Result<()> {
        if body.is_empty() {
            return Ok(());
        }
        stdio::io_result(write!(w, "{body}")).map_err(|e| anyhow::anyhow!("writing output: {e}"))
    }
}

impl HumanRender for ShortlogOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "On {}", self.branch);
        let Some(target) = &self.target else {
            outln!(w, "No target branch found (tried target.branch, origin/master, origin/main, master, main).");
            return Ok(());
        };
        outln!(
            w,
            "Ahead of {} by {} commit{}",
            target,
            self.ahead,
            if self.ahead == 1 { "" } else { "s" }
        );
        Self::render_human_commit_block(w, &self.human_ahead)?;
        if let (Some(behind), true) = (self.behind, self.behind_commits.is_some()) {
            outln!(w);
            outln!(
                w,
                "Behind {} by {} commit{}",
                target,
                behind,
                if behind == 1 { "" } else { "s" }
            );
            Self::render_human_commit_block(w, &self.human_behind)?;
        }
        Ok(())
    }
}

impl MarkdownRender for ShortlogOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "# Branch {}", format_inline_code(&self.branch));
        let Some(target) = &self.target else {
            outln!(w);
            outln!(
                w,
                "No target branch found (tried `target.branch`, `origin/master`, `origin/main`, `master`, `main`)."
            );
            return Ok(());
        };
        outln!(w);
        outln!(
            w,
            "Ahead of {} by **{}** commit{}.",
            format_inline_code(target),
            self.ahead,
            if self.ahead == 1 { "" } else { "s" }
        );
        outln!(w);
        Self::print_markdown_commit_bullets(w, &self.markdown_ahead_bullets)?;
        if self.format.any_format_flag() {
            Self::print_markdown_diff_extra(w, &self.markdown_ahead_diffs)?;
        }
        if let (Some(behind), true) = (self.behind, self.behind_commits.is_some()) {
            outln!(w);
            outln!(
                w,
                "Behind {} by **{}** commit{}.",
                format_inline_code(target),
                behind,
                if behind == 1 { "" } else { "s" }
            );
            outln!(w);
            Self::print_markdown_commit_bullets(w, &self.markdown_behind_bullets)?;
            if self.format.any_format_flag() {
                Self::print_markdown_diff_extra(w, &self.markdown_behind_diffs)?;
            }
        }
        Ok(())
    }
}
