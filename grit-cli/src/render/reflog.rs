//! `grit reflog` outcomes and rendering.

use std::io::Write;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::outln;
use crate::output::Person;
use crate::render::{HumanRender, MarkdownRender, RenderContext};

/// Result of `grit reflog` (list/show).
#[derive(Serialize, JsonSchema)]
pub struct ReflogListOutcome {
    pub r#ref: String,
    pub entries: Vec<ReflogListEntry>,
    /// `true` when the query stopped before the oldest reflog entry (typically due to `-n`).
    pub truncated: bool,
}

#[derive(Serialize, JsonSchema)]
pub struct ReflogListEntry {
    pub index: usize,
    /// Hex `new_oid` (unchanged v1 field).
    pub oid: String,
    pub old_oid: String,
    pub new_oid: String,
    /// RFC 3339 timestamp with the recorded zone offset.
    pub date: String,
    pub relative_date: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub committer: Option<Person>,
    pub message: String,
    /// Best-effort action prefix from the message (before the first `": "`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// Abbreviated `new_oid` for human/markdown lines (not in JSON).
    #[serde(skip)]
    pub abbreviated_oid: String,
}

impl HumanRender for ReflogListOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        for entry in &self.entries {
            let line = format_entry_line(&self.r#ref, entry, false);
            outln!(w, "{line}");
        }
        Ok(())
    }
}

impl MarkdownRender for ReflogListOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.entries.is_empty() {
            outln!(w, "_No reflog entries for `{}`._", self.r#ref);
            return Ok(());
        }
        for entry in &self.entries {
            let line = format_entry_line(&self.r#ref, entry, true);
            outln!(w, "- {line}");
        }
        Ok(())
    }
}

fn format_entry_line(ref_name: &str, entry: &ReflogListEntry, absolute_date: bool) -> String {
    let when = if absolute_date {
        entry.date.as_str()
    } else {
        entry.relative_date.as_str()
    };
    format!(
        "{ref_name}@{{{}}} {} {} {}",
        entry.index, entry.abbreviated_oid, when, entry.message
    )
}

/// Result of `grit reflog delete`.
#[derive(Serialize, JsonSchema)]
pub struct ReflogDeleteOutcome {
    pub deleted: Vec<String>,
}

impl HumanRender for ReflogDeleteOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        for spec in &self.deleted {
            outln!(w, "Deleted {spec}");
        }
        Ok(())
    }
}

impl MarkdownRender for ReflogDeleteOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        for spec in &self.deleted {
            outln!(w, "- Deleted `{spec}`");
        }
        Ok(())
    }
}

/// Result of `grit reflog list` (refs that currently have a log).
#[derive(Serialize, JsonSchema)]
pub struct ReflogRefsListOutcome {
    pub refs: Vec<ReflogRefRow>,
}

#[derive(Serialize, JsonSchema)]
pub struct ReflogRefRow {
    pub r#ref: String,
}

impl HumanRender for ReflogRefsListOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        for row in &self.refs {
            outln!(w, "{}", row.r#ref);
        }
        Ok(())
    }
}

impl MarkdownRender for ReflogRefsListOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.refs.is_empty() {
            outln!(w, "_No reflogs._");
            return Ok(());
        }
        for row in &self.refs {
            outln!(w, "- `{}`", row.r#ref);
        }
        Ok(())
    }
}

/// Result of `grit reflog expire`.
#[derive(Serialize, JsonSchema)]
pub struct ReflogExpireOutcome {
    pub entries_pruned: usize,
    pub dry_run: bool,
    pub refs: Vec<ReflogExpireRefOutcome>,
}

#[derive(Serialize, JsonSchema)]
pub struct ReflogExpireRefOutcome {
    pub r#ref: String,
    pub pruned: usize,
}

impl HumanRender for ReflogExpireOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let verb = if self.dry_run {
            "Would prune"
        } else {
            "Pruned"
        };
        outln!(
            w,
            "{verb} {} reflog {}",
            self.entries_pruned,
            if self.entries_pruned == 1 {
                "entry"
            } else {
                "entries"
            }
        );
        Ok(())
    }
}

impl MarkdownRender for ReflogExpireOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let verb = if self.dry_run {
            "Would prune"
        } else {
            "Pruned"
        };
        outln!(
            w,
            "{verb} **{}** reflog {}.",
            self.entries_pruned,
            if self.entries_pruned == 1 {
                "entry"
            } else {
                "entries"
            }
        );
        Ok(())
    }
}
