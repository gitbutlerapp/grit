//! `grit pick` outcomes and rendering.

use anyhow::Result;
use grit_lib::objects::short_hex;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::render::list::format_inline_code;
use crate::render::operation::{
    render_conflicts_human, render_conflicts_markdown, render_operation_guidance_markdown,
    render_pick_check_conflicts_human, render_pick_conflict_footer_human, render_rerere_markdown,
    ConflictJson,
};
use crate::ui::path_display_for_discovered_repo;
use grit_lib::operation::Conflict;

/// Integration result for `grit pick` (`--json` contract).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PickResult {
    Picked,
    /// Every step would apply cleanly (`grit pick --check` only).
    Clean,
    Conflicts,
    Empty,
    Aborted,
    Skipped,
}

/// One commit recorded in pick `--json` output.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct PickCommitRecord {
    /// Source commit that was replayed (full hex oid).
    pub source: String,
    /// New commit on the current branch, or JSON `null` when `--no-commit` or stopped early.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub oid: Option<String>,
    /// Stored `change-id` on `oid` when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    /// Subject line from the source commit (or the new commit when available).
    pub subject: String,
}

/// Result of `grit pick`.
#[derive(Serialize, JsonSchema)]
pub struct PickOutcome {
    pub result: PickResult,
    /// Commits picked or attempted in this invocation.
    pub commits: Vec<PickCommitRecord>,
    /// Source commits skipped as empty/redundant (`--empty drop` or already applied).
    pub dropped: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub conflicts: Option<Vec<ConflictJson>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub next_steps: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub alternatives: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
    /// Predicted result tree (full hex) when `result` is `clean` from `--check`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub oid: Option<String>,
    /// True when this outcome is from `grit pick --check` (nothing was picked).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub check: bool,
    /// Rerere replay or record events when `rerere.enabled` is active.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rerere: Vec<crate::render::operation::RerereEventJson>,
    #[serde(skip)]
    pub(crate) conflict_sources: Option<Vec<Conflict>>,
}

impl PickOutcome {
    pub(crate) fn new(result: PickResult) -> Self {
        Self {
            result,
            commits: Vec::new(),
            dropped: Vec::new(),
            conflicts: None,
            next_steps: None,
            alternatives: None,
            dry_run: None,
            oid: None,
            check: false,
            rerere: Vec::new(),
            conflict_sources: None,
        }
    }
}

impl HumanRender for PickOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self.result {
            PickResult::Picked | PickResult::Skipped => {
                if self.dry_run == Some(true) {
                    outln!(w, "Would pick {} commit(s)", self.commits.len());
                    for row in &self.commits {
                        let src = short_hex(&row.source);
                        outln!(w, "  {src}");
                    }
                    return Ok(());
                }
                for row in &self.commits {
                    if let Some(oid) = &row.oid {
                        let src = short_hex(&row.source);
                        let new = short_hex(oid);
                        outln!(w, "Picked {src} → {new} {}", row.subject);
                    } else {
                        let src = short_hex(&row.source);
                        outln!(w, "Applied {src} {} (no commit)", row.subject);
                    }
                }
                if !self.dropped.is_empty() {
                    outln!(w, "Dropped {} empty commit(s)", self.dropped.len());
                }
                crate::render::operation::render_rerere_human(&self.rerere);
            }
            PickResult::Clean => {
                if let Some(oid) = &self.oid {
                    outln!(w, "Would pick cleanly (tree {oid})");
                } else {
                    outln!(w, "Would pick cleanly");
                }
            }
            PickResult::Conflicts => {
                if self.check {
                    if let Some(conflicts) = &self.conflict_sources {
                        let path_opts = path_display_for_discovered_repo();
                        render_pick_check_conflicts_human(conflicts, path_opts.as_ref(), w)?;
                    }
                } else if let Some(conflicts) = &self.conflict_sources {
                    let path_opts = path_display_for_discovered_repo();
                    render_conflicts_human(conflicts, None, path_opts.as_ref(), w)?;
                    crate::render::operation::render_rerere_human(&self.rerere);
                    render_pick_conflict_footer_human();
                }
                if !self.check {
                    if let Some(steps) = &self.next_steps {
                        eprintln!();
                        for step in steps {
                            eprintln!("→ {step}");
                        }
                    }
                    if let Some(alt) = &self.alternatives {
                        let joined = alt.join(" or ");
                        eprintln!("→ or {joined}");
                    }
                }
            }
            PickResult::Empty => {
                if self.check {
                    if let Some(row) = self.commits.last() {
                        let src = short_hex(&row.source);
                        outln!(w, "Would stop on an empty pick step ({src})");
                    } else {
                        outln!(w, "Would stop on an empty pick step");
                    }
                } else if let Some(row) = self.commits.last() {
                    let src = short_hex(&row.source);
                    outln!(w, "Stopped on empty commit {src} — {}", row.subject);
                }
                if let Some(steps) = &self.next_steps {
                    for step in steps {
                        eprintln!("→ {step}");
                    }
                }
            }
            PickResult::Aborted => outln!(w, "Pick aborted."),
        }
        Ok(())
    }
}

impl MarkdownRender for PickOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Pick");
        outln!(w);
        match self.result {
            PickResult::Picked | PickResult::Skipped => {
                if self.dry_run == Some(true) {
                    outln!(w, "- Would pick **{}** commit(s)", self.commits.len());
                    for row in &self.commits {
                        let src = short_hex(&row.source);
                        outln!(w, "  - `{src}`");
                    }
                } else {
                    for row in &self.commits {
                        let src = short_hex(&row.source);
                        if let Some(oid) = &row.oid {
                            let new = short_hex(oid);
                            outln!(w, "- `{src}` → `{new}` — {}", row.subject);
                        } else {
                            outln!(w, "- `{src}` — {} (no commit)", row.subject);
                        }
                    }
                    if !self.dropped.is_empty() {
                        outln!(w, "- Dropped **{}** empty commit(s)", self.dropped.len());
                    }
                    render_rerere_markdown(&self.rerere, w)?;
                }
            }
            PickResult::Conflicts => {
                if self.check {
                    outln!(w, "- Would conflict during pick");
                    if let Some(details) = &self.conflict_sources {
                        let path_opts = path_display_for_discovered_repo();
                        for conflict in details {
                            let shown = path_opts
                                .as_ref()
                                .map(|opts| {
                                    format_inline_code(&opts.format_path_for_human(&conflict.path))
                                })
                                .unwrap_or_else(|| format_inline_code(&conflict.path));
                            outln!(w, "  - {shown}");
                        }
                    }
                } else {
                    outln!(w, "- Pick stopped with conflicts");
                    outln!(w);
                    if let Some(details) = &self.conflict_sources {
                        let path_opts = path_display_for_discovered_repo();
                        render_conflicts_markdown(details, None, path_opts.as_ref(), w)?;
                    }
                    render_rerere_markdown(&self.rerere, w)?;
                    render_operation_guidance_markdown(
                        self.next_steps.as_deref().unwrap_or(&[]),
                        self.alternatives.as_deref(),
                        w,
                    )?;
                }
            }
            PickResult::Clean => {
                outln!(w, "- Would pick cleanly");
                if let Some(oid) = &self.oid {
                    outln!(w, "  - tree `{oid}`");
                }
            }
            PickResult::Empty => {
                if self.check {
                    outln!(w, "- Would stop on an empty pick step");
                } else if let Some(row) = self.commits.last() {
                    let src = short_hex(&row.source);
                    outln!(w, "- Stopped on empty commit `{src}` — {}", row.subject);
                    render_operation_guidance_markdown(
                        self.next_steps.as_deref().unwrap_or(&[]),
                        self.alternatives.as_deref(),
                        w,
                    )?;
                } else {
                    outln!(w, "- Pick stopped on an empty commit");
                    render_operation_guidance_markdown(
                        self.next_steps.as_deref().unwrap_or(&[]),
                        self.alternatives.as_deref(),
                        w,
                    )?;
                }
            }
            PickResult::Aborted => outln!(w, "Pick aborted."),
        }
        Ok(())
    }
}
