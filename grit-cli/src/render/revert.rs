//! `grit revert` outcomes and rendering.

use anyhow::Result;
use grit_lib::objects::short_hex;
use grit_lib::operation::Conflict;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::render::list::format_inline_code;
use crate::render::operation::{
    render_conflicts_human, render_conflicts_markdown, render_operation_guidance_markdown,
    render_rerere_markdown, render_revert_check_conflicts_human,
    render_revert_conflict_footer_human, ConflictJson,
};
use crate::ui::path_display_for_discovered_repo;

/// One revert commit created on the current branch.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RevertCommitEntry {
    /// Source commit that was reverted (full hex oid).
    pub source: String,
    /// New revert commit on the current branch (full hex oid).
    pub oid: String,
    /// Subject line of the revert commit.
    pub subject: String,
    /// Stored `change-id` on the revert commit, or JSON `null` when absent.
    pub change_id: Option<String>,
}

/// High-level result of `grit revert`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RevertResult {
    /// Every requested step finished (or `--dry-run` printed the plan).
    Completed,
    /// Every step would apply cleanly (`grit revert --check` only).
    Clean,
    /// The sequence stopped on merge conflicts.
    Conflicts,
    /// `--abort` restored the pre-revert state.
    Aborted,
    /// A step would be empty and `--empty stop` halted the sequence.
    Empty,
}

/// Result of `grit revert`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RevertOutcome {
    pub result: RevertResult,
    /// The last source commit reverted in this invocation (full hex), when `result` is `completed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The last revert commit created (full hex), when `result` is `completed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub oid: Option<String>,
    /// Subject of the last revert commit, when `result` is `completed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// `change-id` on the last revert commit, when `result` is `completed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    /// Revert commits created in this invocation (newest first).
    pub commits: Vec<RevertCommitEntry>,
    /// Source commits skipped as empty per `--empty drop`.
    pub dropped: Vec<String>,
    /// Conflicted paths when `result` is `conflicts`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub conflicts: Option<Vec<ConflictJson>>,
    /// Suggested follow-up commands when `result` is `conflicts` or `empty`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub next_steps: Option<Vec<String>>,
    /// Mutually exclusive options (`--abort`, `--skip`) when paused.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub alternatives: Option<Vec<String>>,
    /// True when `--dry-run` was set (no refs or objects were updated).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
    /// True when this outcome is from `grit revert --check` (nothing was reverted).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub check: bool,
    /// Rerere replay or record events when `rerere.enabled` is active.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rerere: Vec<crate::render::operation::RerereEventJson>,
    #[serde(skip)]
    pub(crate) conflict_sources: Option<Vec<Conflict>>,
}

impl RevertOutcome {
    pub(crate) fn new(result: RevertResult) -> Self {
        Self {
            result,
            source: None,
            oid: None,
            subject: None,
            change_id: None,
            commits: Vec::new(),
            dropped: Vec::new(),
            conflicts: None,
            next_steps: None,
            alternatives: None,
            dry_run: None,
            check: false,
            rerere: Vec::new(),
            conflict_sources: None,
        }
    }

    pub(crate) fn dry_run(sources: Vec<grit_lib::objects::ObjectId>) -> Self {
        Self {
            commits: sources
                .into_iter()
                .map(|oid| RevertCommitEntry {
                    source: oid.to_hex(),
                    oid: String::new(),
                    subject: String::new(),
                    change_id: None,
                })
                .collect(),
            dry_run: Some(true),
            ..Self::new(RevertResult::Completed)
        }
    }
}

fn short_change_id(id: &str) -> &str {
    id.get(..7).unwrap_or(id)
}

impl HumanRender for RevertOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self.result {
            RevertResult::Aborted => {
                outln!(w, "Revert aborted.");
            }
            RevertResult::Conflicts => {
                if let Some(details) = &self.conflict_sources {
                    let path_opts = path_display_for_discovered_repo();
                    if self.check {
                        render_revert_check_conflicts_human(details, path_opts.as_ref(), w)?;
                    } else {
                        render_conflicts_human(details, None, path_opts.as_ref(), w)?;
                        crate::render::operation::render_rerere_human(&self.rerere);
                        render_revert_conflict_footer_human();
                    }
                }
            }
            RevertResult::Clean => {
                if let Some(oid) = &self.oid {
                    outln!(w, "Would revert cleanly (tree {oid})");
                } else {
                    outln!(w, "Would revert cleanly");
                }
            }
            RevertResult::Empty => {
                if self.check {
                    outln!(w, "Would stop on an empty revert step.");
                } else {
                    outln!(w, "Revert stopped on an empty commit.");
                    if let Some(steps) = &self.next_steps {
                        eprintln!("→ {}, or `--skip` / `--abort`.", steps.join(", then "));
                    }
                }
            }
            RevertResult::Completed => {
                if self.dry_run == Some(true) {
                    outln!(w, "Would revert {} commit(s):", self.commits.len());
                    for entry in &self.commits {
                        let short = short_hex(&entry.source);
                        outln!(w, "  {short}");
                    }
                    return Ok(());
                }
                for entry in &self.commits {
                    let new_short = short_hex(&entry.oid);
                    let src_short = short_hex(&entry.source);
                    outln!(w, "Reverted {src_short} → {new_short} {}", entry.subject);
                }
                crate::render::operation::render_rerere_human(&self.rerere);
            }
        }
        Ok(())
    }
}

impl MarkdownRender for RevertOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Revert");
        outln!(w);
        match self.result {
            RevertResult::Aborted => outln!(w, "Revert aborted."),
            RevertResult::Conflicts => {
                if self.check {
                    outln!(w, "- Would conflict during revert");
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
                    outln!(w, "- Revert stopped with conflicts");
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
            RevertResult::Clean => {
                outln!(w, "- Would revert cleanly");
                if let Some(oid) = &self.oid {
                    outln!(w, "  - tree `{oid}`");
                }
            }
            RevertResult::Empty => {
                if self.check {
                    outln!(w, "- Would stop on an empty revert step");
                } else {
                    outln!(w, "- Revert stopped on an empty commit");
                    render_operation_guidance_markdown(
                        self.next_steps.as_deref().unwrap_or(&[]),
                        self.alternatives.as_deref(),
                        w,
                    )?;
                }
            }
            RevertResult::Completed => {
                if self.dry_run == Some(true) {
                    outln!(w, "- Would revert {} commit(s)", self.commits.len());
                    for entry in &self.commits {
                        let src = short_hex(&entry.source);
                        outln!(w, "  - `{src}`");
                    }
                } else {
                    for entry in &self.commits {
                        let src = short_hex(&entry.source);
                        let new = short_hex(&entry.oid);
                        outln!(w, "- `{src}` → `{new}` — {}", entry.subject);
                        if let Some(id) = entry.change_id.as_deref() {
                            outln!(w, "- change id `{short}`", short = short_change_id(id));
                        }
                    }
                    render_rerere_markdown(&self.rerere, w)?;
                }
            }
        }
        Ok(())
    }
}
