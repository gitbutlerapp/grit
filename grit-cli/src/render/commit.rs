//! `grit commit` outcomes and rendering.

use crate::markdown;
use crate::output::outln;
use crate::render::{HumanRender, MarkdownRender, RenderContext};
use anyhow::Result;
use grit_lib::objects::short_hex;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

/// JSON scope label for [`CommitOutcome::scope`].
#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommitScopeJson {
    AutoStage,
    Staged,
    Paths,
    Patch,
}

/// Author recorded on the new commit (`--json` / `--markdown`).
#[derive(Serialize, JsonSchema, Clone)]
pub struct CommitAuthorOutcome {
    pub name: String,
    pub email: String,
    /// Author timestamp in RFC 3339 (same shape as log/show).
    pub date: String,
}

/// One path that would be or was included in the commit (`--json`).
#[derive(Serialize, JsonSchema, Clone)]
pub struct CommitFileOutcome {
    pub path: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
}

/// Result of `grit commit`.
#[derive(Serialize, JsonSchema)]
pub struct CommitOutcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oid: Option<String>,
    /// Branch the commit advanced, or JSON `null` on a detached `HEAD`.
    pub branch: Option<String>,
    pub detached: bool,
    pub subject: String,
    pub changes: usize,
    pub amended: bool,
    pub dry_run: bool,
    pub scope: CommitScopeJson,
    pub files: Vec<CommitFileOutcome>,
    /// Stored `change-id` header on the new commit, or JSON `null` when absent or dry-run.
    pub change_id: Option<String>,
    pub parents: Vec<String>,
    pub author: CommitAuthorOutcome,
    pub hooks_run: Vec<String>,
    /// Nested repositories newly recorded as gitlinks while auto-staging before this commit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub embedded_gitlinks: Vec<String>,
}

impl CommitOutcome {
    /// The branch name, or `detached HEAD`, for human and Markdown output.
    pub fn branch_label(&self) -> &str {
        self.branch.as_deref().unwrap_or("detached HEAD")
    }
}

impl HumanRender for CommitOutcome {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        crate::embedded_gitlink::emit_embedded_gitlink_warnings(
            ctx.mode,
            &self.embedded_gitlinks,
            self.dry_run,
            None,
        );
        if self.dry_run {
            outln!(
                w,
                "Would commit {} file{}",
                self.files.len(),
                if self.files.len() == 1 { "" } else { "s" }
            );
            const MAX_LIST: usize = 20;
            for file in self.files.iter().take(MAX_LIST) {
                if let Some(ref old) = file.old_path {
                    outln!(w, "  {} {} -> {}", file.status, old, file.path);
                } else {
                    outln!(w, "  {} {}", file.status, file.path);
                }
            }
            if self.files.len() > MAX_LIST {
                outln!(w, "  … and {} more", self.files.len() - MAX_LIST);
            }
            return Ok(());
        }
        outln!(
            w,
            "[{} {}] {}",
            self.branch_label(),
            short_hex(self.oid.as_deref().unwrap_or("")),
            self.subject
        );
        outln!(
            w,
            "{} change{} committed",
            self.changes,
            if self.changes == 1 { "" } else { "s" }
        );
        if !self.hooks_run.is_empty() {
            outln!(w, "hooks: {}", self.hooks_run.join(", "));
        }
        Ok(())
    }
}

impl MarkdownRender for CommitOutcome {
    fn render_markdown(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        crate::embedded_gitlink::emit_embedded_gitlink_warnings(
            ctx.mode,
            &self.embedded_gitlinks,
            self.dry_run,
            None,
        );
        markdown::print_commit_outcome(w, self)?;
        Ok(())
    }
}
