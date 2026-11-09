//! `grit merge` outcomes and rendering.

use anyhow::Result;
use grit_lib::change_id::ChangeId;
use grit_lib::objects::{short_hex, ObjectId};
use grit_lib::operation::Conflict;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
pub use crate::render::conflict::MergeConflictJson;
use crate::render::operation::{
    conflict_json, render_conflicts_human, ConflictJson, OperationConflictBlock,
};
use crate::ui::path_display_for_discovered_repo;

fn short_change_id(id: &str) -> &str {
    id.get(..7).unwrap_or(id)
}

/// Merge / pull integration result (`--json` contract).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MergeResult {
    UpToDate,
    FastForward,
    /// Three-way merge would succeed (`grit merge --check` only).
    Clean,
    Merged,
    Conflicts,
    Aborted,
    Squashed,
    Stopped,
    RefusedNotFf,
}

/// Result of `grit merge` (and the merge half of `grit pull`).
#[derive(Serialize, JsonSchema)]
pub struct MergeOutcome {
    pub result: MergeResult,
    /// The merge source label.
    pub branch: String,
    /// Current branch whose tip moved after a fast-forward; omitted when it matches `branch`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub updated_branch: Option<String>,
    /// Resulting commit (fast-forward target or merge commit).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub oid: Option<String>,
    /// Stored `change-id` on the resulting commit when `oid` is set and a header is present.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    /// Structured conflicts when `result` is `conflicts`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub conflicts: Option<Vec<ConflictJson>>,
    /// Rerere replay or record events when `rerere.enabled` is active.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rerere: Vec<crate::render::operation::RerereEventJson>,
    /// Suggested `grit` commands when `result` is `conflicts`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub next_steps: Option<Vec<String>>,
    /// Abort/skip options when `result` is `conflicts` (not part of the ordered sequence).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub alternatives: Option<Vec<String>>,
    /// True when this outcome is from `grit merge --check` (nothing was merged).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub check: bool,
    /// True when this outcome is from `grit merge --dry-run` (nothing was merged).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
    /// Parent commits of a predicted merge commit (`--dry-run` with `result: merged`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub parents: Option<Vec<String>>,
    /// True when `HEAD` was detached for this merge (not serialized in `--json`).
    #[serde(skip)]
    pub(crate) detached_head: bool,
    /// Library conflicts for safe human/markdown path display (JSON uses [`Self::conflicts`]).
    #[serde(skip)]
    pub(crate) conflict_sources: Option<Vec<Conflict>>,
}

pub(crate) fn merge_conflict_json(detail: &Conflict, merge_label: &str) -> MergeConflictJson {
    let ConflictJson {
        path,
        kind,
        summary,
    } = conflict_json(detail, Some(merge_label));
    MergeConflictJson {
        path,
        kind,
        summary,
    }
}

impl MergeOutcome {
    pub(crate) fn up_to_date(label: &str) -> Self {
        Self {
            result: MergeResult::UpToDate,
            branch: label.to_owned(),
            oid: None,
            change_id: None,
            conflicts: None,
            next_steps: None,
            alternatives: None,
            check: false,
            dry_run: None,
            parents: None,
            updated_branch: None,
            rerere: Vec::new(),
            detached_head: false,
            conflict_sources: None,
        }
    }

    pub(crate) fn up_to_date_check(label: &str) -> Self {
        let mut o = Self::up_to_date(label);
        o.check = true;
        o
    }

    pub(crate) fn up_to_date_dry_run(label: &str) -> Self {
        let mut o = Self::up_to_date(label);
        o.dry_run = Some(true);
        o
    }

    pub(crate) fn fast_forward(
        label: &str,
        head_short: &str,
        oid: ObjectId,
        change_id: Option<ChangeId>,
    ) -> Self {
        Self {
            result: MergeResult::FastForward,
            branch: label.to_owned(),
            oid: Some(oid.to_hex()),
            change_id: change_id.as_ref().map(|id| id.as_str().to_owned()),
            conflicts: None,
            next_steps: None,
            alternatives: None,
            check: false,
            dry_run: None,
            parents: None,
            updated_branch: fast_forward_updated_branch(head_short, label),
            rerere: Vec::new(),
            detached_head: false,
            conflict_sources: None,
        }
    }

    pub(crate) fn fast_forward_check(
        label: &str,
        head_short: &str,
        oid: ObjectId,
        change_id: Option<ChangeId>,
    ) -> Self {
        Self {
            result: MergeResult::FastForward,
            branch: label.to_owned(),
            oid: Some(oid.to_hex()),
            change_id: change_id.as_ref().map(|id| id.as_str().to_owned()),
            conflicts: None,
            next_steps: None,
            alternatives: None,
            check: true,
            dry_run: None,
            parents: None,
            updated_branch: fast_forward_updated_branch(head_short, label),
            rerere: Vec::new(),
            detached_head: false,
            conflict_sources: None,
        }
    }

    pub(crate) fn fast_forward_dry_run(
        label: &str,
        head_short: &str,
        oid: ObjectId,
        change_id: Option<ChangeId>,
    ) -> Self {
        let mut o = Self::fast_forward(label, head_short, oid, change_id);
        o.dry_run = Some(true);
        o
    }

    pub(crate) fn merged(label: &str, oid: ObjectId, change_id: Option<ChangeId>) -> Self {
        Self {
            result: MergeResult::Merged,
            branch: label.to_owned(),
            oid: Some(oid.to_hex()),
            change_id: change_id.as_ref().map(|id| id.as_str().to_owned()),
            conflicts: None,
            next_steps: None,
            alternatives: None,
            check: false,
            dry_run: None,
            parents: None,
            updated_branch: None,
            rerere: Vec::new(),
            detached_head: false,
            conflict_sources: None,
        }
    }

    pub(crate) fn merged_dry_run(label: &str, parents: Vec<ObjectId>) -> Self {
        Self {
            result: MergeResult::Merged,
            branch: label.to_owned(),
            oid: None,
            change_id: None,
            conflicts: None,
            next_steps: None,
            alternatives: None,
            check: false,
            dry_run: Some(true),
            parents: Some(parents.iter().map(ObjectId::to_hex).collect()),
            updated_branch: None,
            rerere: Vec::new(),
            detached_head: false,
            conflict_sources: None,
        }
    }

    pub(crate) fn clean_check(label: &str, tree: ObjectId) -> Self {
        Self {
            result: MergeResult::Clean,
            branch: label.to_owned(),
            oid: Some(tree.to_hex()),
            change_id: None,
            conflicts: None,
            next_steps: None,
            alternatives: None,
            check: true,
            dry_run: None,
            parents: None,
            updated_branch: None,
            rerere: Vec::new(),
            detached_head: false,
            conflict_sources: None,
        }
    }

    pub(crate) fn conflicts_check(label: &str, details: Vec<Conflict>) -> Self {
        let conflict_sources = Some(details.clone());
        let conflicts = Some(
            details
                .iter()
                .map(|d| conflict_json(d, Some(label)))
                .collect(),
        );
        Self {
            result: MergeResult::Conflicts,
            branch: label.to_owned(),
            oid: None,
            change_id: None,
            conflicts,
            next_steps: None,
            alternatives: None,
            check: true,
            dry_run: None,
            parents: None,
            updated_branch: None,
            rerere: Vec::new(),
            detached_head: false,
            conflict_sources,
        }
    }

    pub(crate) fn conflicts_dry_run(label: &str, details: Vec<Conflict>) -> Self {
        let mut o = Self::conflicts_check(label, details);
        o.check = false;
        o.dry_run = Some(true);
        o
    }

    pub(crate) fn conflicts(
        label: &str,
        details: Vec<Conflict>,
        rerere: Vec<grit_lib::diff::rerere::RerereEvent>,
    ) -> Self {
        let block = OperationConflictBlock::for_merge(&details, label);
        Self {
            result: MergeResult::Conflicts,
            branch: label.to_owned(),
            oid: None,
            change_id: None,
            conflicts: Some(block.conflicts),
            rerere: crate::render::operation::rerere_events_json(&rerere),
            next_steps: Some(block.next_steps),
            alternatives: block.alternatives,
            check: false,
            dry_run: None,
            parents: None,
            updated_branch: None,
            detached_head: false,
            conflict_sources: Some(details),
        }
    }

    pub(crate) fn squashed(label: &str) -> Self {
        Self {
            result: MergeResult::Squashed,
            branch: label.to_owned(),
            oid: None,
            change_id: None,
            conflicts: None,
            next_steps: Some(vec![
                "grit commit".into(),
                "Review SQUASH_MSG or pass -m / --message-file".into(),
            ]),
            alternatives: None,
            check: false,
            dry_run: None,
            parents: None,
            updated_branch: None,
            rerere: Vec::new(),
            detached_head: false,
            conflict_sources: None,
        }
    }

    pub(crate) fn stopped(label: &str) -> Self {
        Self {
            result: MergeResult::Stopped,
            branch: label.to_owned(),
            oid: None,
            change_id: None,
            conflicts: None,
            next_steps: Some(vec!["grit commit".into(), "grit merge --abort".into()]),
            alternatives: None,
            check: false,
            dry_run: None,
            parents: None,
            updated_branch: None,
            rerere: Vec::new(),
            detached_head: false,
            conflict_sources: None,
        }
    }

    pub(crate) fn refused_not_ff(label: &str) -> Self {
        Self {
            result: MergeResult::RefusedNotFf,
            branch: label.to_owned(),
            oid: None,
            change_id: None,
            conflicts: None,
            next_steps: None,
            alternatives: None,
            check: false,
            dry_run: None,
            parents: None,
            updated_branch: None,
            rerere: Vec::new(),
            detached_head: false,
            conflict_sources: None,
        }
    }

    pub(crate) fn refused_not_ff_dry_run(label: &str) -> Self {
        let mut o = Self::refused_not_ff(label);
        o.dry_run = Some(true);
        o
    }

    pub(crate) fn aborted() -> Self {
        Self {
            result: MergeResult::Aborted,
            branch: String::new(),
            oid: None,
            change_id: None,
            conflicts: None,
            next_steps: None,
            alternatives: None,
            check: false,
            dry_run: None,
            parents: None,
            updated_branch: None,
            rerere: Vec::new(),
            detached_head: false,
            conflict_sources: None,
        }
    }

    pub(crate) fn for_detached_head(mut self) -> Self {
        self.detached_head = true;
        self.updated_branch = None;
        self
    }
}

fn fast_forward_updated_branch(head_short: &str, merge_label: &str) -> Option<String> {
    (head_short != merge_label).then(|| head_short.to_owned())
}

fn write_fast_forward_human(w: &mut dyn Write, outcome: &MergeOutcome, would: bool) -> Result<()> {
    let short = outcome.oid.as_deref().map(short_hex).unwrap_or_default();
    let head = if outcome.detached_head {
        "HEAD"
    } else {
        outcome
            .updated_branch
            .as_deref()
            .unwrap_or(outcome.branch.as_str())
    };
    let source = outcome
        .updated_branch
        .as_ref()
        .map(|_| outcome.branch.as_str());
    match (would, source) {
        (true, Some(src)) => outln!(w, "Would fast-forward {head} → {short} ({src})"),
        (true, None) => outln!(w, "Would fast-forward {head} → {short}"),
        (false, Some(src)) => outln!(w, "Fast-forwarded {head} → {short} ({src})"),
        (false, None) => outln!(w, "Fast-forwarded {head} → {short}"),
    }
    Ok(())
}

impl HumanRender for MergeOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let short = self.oid.as_deref().map(short_hex).unwrap_or_default();
        match self.result {
            MergeResult::UpToDate if self.dry_run == Some(true) => {
                if self.detached_head {
                    outln!(w, "Would leave HEAD up to date with {}.", self.branch);
                } else {
                    outln!(
                        w,
                        "Would leave the current branch up to date with {}.",
                        self.branch
                    );
                }
            }
            MergeResult::UpToDate => outln!(w, "Already up to date."),
            MergeResult::FastForward if self.check || self.dry_run == Some(true) => {
                write_fast_forward_human(w, self, true)?;
            }
            MergeResult::FastForward => write_fast_forward_human(w, self, false)?,
            MergeResult::Clean => outln!(w, "Would merge {} cleanly (tree {short})", self.branch),
            MergeResult::Merged if self.dry_run == Some(true) => {
                if self.detached_head {
                    outln!(w, "Would merge {} into HEAD (merge commit)", self.branch);
                } else {
                    outln!(
                        w,
                        "Would merge {} into the current branch (merge commit)",
                        self.branch
                    );
                }
            }
            MergeResult::Merged if self.detached_head => {
                outln!(w, "Merged {} into HEAD ({short})", self.branch);
                crate::render::operation::render_rerere_human(&self.rerere);
            }
            MergeResult::Merged => {
                outln!(
                    w,
                    "Merged {} into the current branch ({short})",
                    self.branch
                );
                crate::render::operation::render_rerere_human(&self.rerere);
            }
            MergeResult::Squashed => {
                outln!(
                    w,
                    "Squash merge applied from {} — run `grit commit` to record the squashed commit",
                    self.branch
                );
            }
            MergeResult::Stopped => {
                outln!(
                    w,
                    "Merge from {} applied — run `grit commit` to finish, or `grit merge --abort` to cancel",
                    self.branch
                );
            }
            MergeResult::RefusedNotFf if self.dry_run == Some(true) => {
                outln!(
                    w,
                    "Would refuse to fast-forward {} — histories have diverged (use merge without --ff-only)",
                    self.branch
                );
            }
            MergeResult::RefusedNotFf => {
                outln!(
                    w,
                    "Cannot fast-forward {} — histories have diverged (use merge without --ff-only)",
                    self.branch
                );
            }
            MergeResult::Conflicts if self.check || self.dry_run == Some(true) => {
                if let Some(details) = &self.conflict_sources {
                    let path_opts = path_display_for_discovered_repo();
                    crate::render::operation::render_merge_check_conflicts_human(
                        &self.branch,
                        details,
                        path_opts.as_ref(),
                        w,
                    )?;
                }
            }
            MergeResult::Conflicts => {
                if let Some(details) = &self.conflict_sources {
                    let path_opts = path_display_for_discovered_repo();
                    render_conflicts_human(
                        details,
                        Some(self.branch.as_str()),
                        path_opts.as_ref(),
                        w,
                    )?;
                }
                crate::render::operation::render_rerere_human(&self.rerere);
                crate::render::operation::render_merge_conflict_footer_human();
            }
            MergeResult::Aborted => outln!(w, "Merge aborted."),
        }
        Ok(())
    }
}

fn write_merge_fast_forward_markdown(
    w: &mut dyn Write,
    outcome: &MergeOutcome,
    would: bool,
) -> Result<()> {
    let short = outcome.oid.as_deref().map(short_hex).unwrap_or("");
    let head = if outcome.detached_head {
        "HEAD"
    } else {
        outcome
            .updated_branch
            .as_deref()
            .unwrap_or(outcome.branch.as_str())
    };
    let source = outcome
        .updated_branch
        .as_ref()
        .map(|_| outcome.branch.as_str());
    match (would, source) {
        (true, Some(src)) => outln!(
            w,
            "- Would fast-forward **`{head}`** → `{short}` (**`{src}`**)"
        ),
        (true, None) => outln!(w, "- Would fast-forward **`{head}`** → `{short}`"),
        (false, Some(src)) => outln!(w, "- Fast-forwarded **`{head}`** → `{short}` (**`{src}`**)"),
        (false, None) => outln!(w, "- Fast-forwarded **`{head}`** → `{short}`"),
    }
    Ok(())
}

impl MarkdownRender for MergeOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Merge");
        outln!(w);
        match self.result {
            MergeResult::UpToDate => {
                outln!(w, "Already up to date with **`{}`**.", self.branch);
            }
            MergeResult::FastForward if self.check => {
                write_merge_fast_forward_markdown(w, self, true)?;
            }
            MergeResult::FastForward => {
                write_merge_fast_forward_markdown(w, self, false)?;
                if let Some(id) = self.change_id.as_deref() {
                    outln!(w, "- change id `{short}`", short = short_change_id(id));
                }
            }
            MergeResult::Clean => {
                let short = self.oid.as_deref().map(short_hex).unwrap_or("");
                outln!(
                    w,
                    "- Would merge **`{}`** cleanly · tree `{short}`",
                    self.branch
                );
            }
            MergeResult::Merged => {
                let short = self.oid.as_deref().map(short_hex).unwrap_or("");
                if self.detached_head {
                    outln!(w, "- Merged **`{}`** into HEAD · `{short}`", self.branch);
                } else {
                    outln!(
                        w,
                        "- Merged **`{}`** into the current branch · `{short}`",
                        self.branch
                    );
                }
                if let Some(id) = self.change_id.as_deref() {
                    outln!(w, "- change id `{short}`", short = short_change_id(id));
                }
                crate::render::operation::render_rerere_markdown(&self.rerere, w)?;
            }
            MergeResult::Squashed => {
                outln!(
                    w,
                    "- Squash merge from **`{}`** applied to the index (see `SQUASH_MSG`)",
                    self.branch
                );
            }
            MergeResult::Stopped => {
                outln!(
                w,
                "- Merge from **`{}`** applied; finish with `grit commit` or `grit merge --abort`",
                self.branch
            );
            }
            MergeResult::RefusedNotFf => {
                outln!(
                    w,
                    "- **`--ff-only`**: cannot fast-forward **`{}`** (histories diverged)",
                    self.branch
                );
            }
            MergeResult::Conflicts if self.check => {
                if let Some(details) = &self.conflict_sources {
                    let path_opts = crate::ui::path_display_for_discovered_repo();
                    crate::render::operation::render_merge_check_conflicts_markdown(
                        &self.branch,
                        details,
                        path_opts.as_ref(),
                        w,
                    )?;
                }
            }
            MergeResult::Conflicts => {
                outln!(w, "- Merge **`{}`** stopped with conflicts", self.branch);
                outln!(w);
                if let Some(details) = &self.conflict_sources {
                    let path_opts = crate::ui::path_display_for_discovered_repo();
                    crate::render::operation::render_conflicts_markdown(
                        details,
                        Some(self.branch.as_str()),
                        path_opts.as_ref(),
                        w,
                    )?;
                }
                crate::render::operation::render_rerere_markdown(&self.rerere, w)?;
                if self.next_steps.is_some() || self.alternatives.is_some() {
                    crate::render::operation::render_operation_guidance_markdown(
                        self.next_steps.as_deref().unwrap_or(&[]),
                        self.alternatives.as_deref(),
                        w,
                    )?;
                } else {
                    outln!(w);
                    outln!(
                        w,
                        "→ fix conflicts, then `grit merge --continue`, or `grit merge --abort`"
                    );
                }
            }
            MergeResult::Aborted => outln!(w, "Merge aborted."),
        }
        if let Some(steps) = &self.next_steps {
            let already_rendered = matches!(self.result, MergeResult::Conflicts) && !self.check;
            if !already_rendered {
                outln!(w);
                for step in steps {
                    outln!(w, "- `{step}`");
                }
            }
        }

        Ok(())
    }
}
