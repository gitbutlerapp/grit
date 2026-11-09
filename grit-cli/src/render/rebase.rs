//! `grit rebase` outcomes and rendering.

use anyhow::Result;
use grit_lib::objects::{short_hex, ObjectId};
use grit_lib::operation::Conflict;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::render::history::{
    ExecResultJson, HeadPair, HistoryRewriteOutcome, RefPair, RewrittenPair,
};
use crate::render::list::format_inline_code;
use crate::render::operation::{
    conflict_json, render_conflicts_human, render_conflicts_markdown,
    render_operation_guidance_markdown, render_rebase_conflict_footer_human, render_rerere_human,
    render_rerere_markdown, ConflictJson, OperationConflictBlock,
};
use crate::ui::path_display_for_discovered_repo;

/// Result of `grit rebase --check`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RebaseCheckOutcome {
    pub operation: String,
    pub result: RebaseCheckResult,
    pub check: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub commits: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub conflicts: Option<Vec<ConflictJson>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Other branches whose replay would conflict; absent when the conflict is on the
    /// rebased branch itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub refs: Option<Vec<String>>,
    #[serde(skip)]
    pub(crate) conflict_sources: Option<Vec<Conflict>>,
}

/// Outcome category for [`RebaseCheckOutcome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RebaseCheckResult {
    UpToDate,
    Clean,
    Conflicts,
}

impl RebaseCheckOutcome {
    pub(crate) fn up_to_date() -> Self {
        Self {
            operation: "rebase".into(),
            result: RebaseCheckResult::UpToDate,
            check: true,
            commits: None,
            conflicts: None,
            commit: None,
            refs: None,
            conflict_sources: None,
        }
    }

    pub(crate) fn clean(commits: Vec<ObjectId>) -> Self {
        Self {
            operation: "rebase".into(),
            result: RebaseCheckResult::Clean,
            check: true,
            commits: Some(commits.iter().map(ObjectId::to_hex).collect()),
            conflicts: None,
            commit: None,
            refs: None,
            conflict_sources: None,
        }
    }

    pub(crate) fn conflicts(commit: ObjectId, conflicts: &[Conflict], refs: Vec<String>) -> Self {
        let conflict_sources = Some(conflicts.to_vec());
        let conflicts = Some(conflicts.iter().map(|c| conflict_json(c, None)).collect());
        Self {
            operation: "rebase".into(),
            result: RebaseCheckResult::Conflicts,
            check: true,
            commits: None,
            conflicts,
            commit: Some(commit.to_hex()),
            refs: (!refs.is_empty()).then_some(refs),
            conflict_sources,
        }
    }
}

fn print_rebase_check_outcome(w: &mut dyn Write, outcome: &RebaseCheckOutcome) -> Result<()> {
    outln!(w, "## Rebase check");
    outln!(w);
    match outcome.result {
        RebaseCheckResult::UpToDate => outln!(w, "- Already up to date"),
        RebaseCheckResult::Clean => {
            let n = outcome.commits.as_ref().map_or(0, Vec::len);
            outln!(w, "- Would rebase {n} commit(s) cleanly");
        }
        RebaseCheckResult::Conflicts => {
            match &outcome.refs {
                Some(refs) => {
                    let refs: Vec<String> = refs.iter().map(|r| format!("`{r}`")).collect();
                    outln!(
                        w,
                        "- Would conflict moving {} onto the rebased history",
                        refs.join(", ")
                    );
                }
                None => outln!(w, "- Would conflict during rebase"),
            }
            if let Some(conflicts) = &outcome.conflict_sources {
                let path_opts = path_display_for_discovered_repo();
                for conflict in conflicts {
                    let shown = path_opts
                        .as_ref()
                        .map(|opts| format_inline_code(&opts.format_path_for_human(&conflict.path)))
                        .unwrap_or_else(|| format_inline_code(&conflict.path));
                    outln!(w, "- {shown} ({})", conflict.kind.as_str());
                }
            }
        }
    }
    Ok(())
}

impl HumanRender for RebaseCheckOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self.result {
            RebaseCheckResult::UpToDate => outln!(w, "Already up to date"),
            RebaseCheckResult::Clean => {
                let n = self.commits.as_ref().map_or(0, Vec::len);
                outln!(w, "Would rebase {n} commit(s) cleanly");
            }
            RebaseCheckResult::Conflicts => {
                match &self.refs {
                    Some(refs) => outln!(
                        w,
                        "Would conflict moving {} onto the rebased history",
                        refs.join(", ")
                    ),
                    None => outln!(w, "Would conflict during rebase"),
                }
                if let Some(conflicts) = &self.conflict_sources {
                    let path_opts = path_display_for_discovered_repo();
                    render_conflicts_human(conflicts, None, path_opts.as_ref(), w)?;
                }
            }
        }
        Ok(())
    }
}

impl MarkdownRender for RebaseCheckOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        print_rebase_check_outcome(w, self)
    }
}

/// High-level result of `grit rebase`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RebaseResult {
    Completed,
    Conflicts,
    ExecDirty,
    Aborted,
}

/// Result of `grit rebase` and `grit history rebase`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RebaseCliOutcome {
    /// Successful exec dirtied tracked files; preserved at `commit` until recovery.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Changed tracked paths when `result` is `exec_dirty`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    pub result: RebaseResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rewritten: Vec<RewrittenPair>,
    /// Ref tip updates. For `aborted`, branches something else moved during the stop that the
    /// abort reset: `old` is the overwritten tip, `new` the restored pre-rebase tip.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<RefPair>,
    /// Previous and new `HEAD` commit OIDs for a completed invocation; null when unchanged.
    /// On continue/skip, compares the stopped HEAD with HEAD after branch reattachment.
    /// Dry runs describe the planned change; conflict and abort outcomes use null.
    #[serde(default)]
    pub head: Option<HeadPair>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub conflicts: Option<Vec<ConflictJson>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub next_steps: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub alternatives: Option<Vec<String>>,
    /// `--exec` subprocess results through completion or a dirty exec stop.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exec: Vec<ExecResultJson>,
    /// Rerere replay or record events when `rerere.enabled` is active.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rerere: Vec<crate::render::operation::RerereEventJson>,
    #[serde(skip)]
    pub(crate) conflict_sources: Option<Vec<Conflict>>,
}

impl RebaseCliOutcome {
    pub(crate) fn aborted() -> Self {
        Self {
            command: None,
            paths: Vec::new(),
            result: RebaseResult::Aborted,
            operation: None,
            rewritten: Vec::new(),
            refs: Vec::new(),
            head: None,
            dry_run: false,
            commit: None,
            conflicts: None,
            next_steps: None,
            alternatives: None,
            exec: Vec::new(),
            rerere: Vec::new(),
            conflict_sources: None,
        }
    }

    pub(crate) fn from_rewrite(outcome: HistoryRewriteOutcome) -> Self {
        Self {
            command: None,
            paths: Vec::new(),
            result: RebaseResult::Completed,
            operation: Some(outcome.operation),
            rewritten: outcome.rewritten,
            refs: outcome.refs,
            head: outcome.head,
            dry_run: outcome.dry_run,
            exec: outcome.exec,
            commit: None,
            conflicts: None,
            next_steps: None,
            alternatives: None,
            rerere: Vec::new(),
            conflict_sources: None,
        }
    }

    pub(crate) fn from_exec_dirty(
        commit: ObjectId,
        command: String,
        paths: Vec<String>,
        exec: Vec<ExecResultJson>,
    ) -> Self {
        let mut outcome = Self::aborted();
        outcome.result = RebaseResult::ExecDirty;
        outcome.operation = Some("rebase".into());
        outcome.commit = Some(commit.to_hex());
        outcome.command = Some(command);
        outcome.paths = paths;
        outcome.exec = exec;
        outcome.next_steps = Some(vec![
            "grit stash push -m 'exec edits'".into(),
            "grit rebase --continue".into(),
        ]);
        outcome.alternatives = Some(vec![
            "grit rebase --skip".into(),
            "grit rebase --abort".into(),
        ]);
        outcome
    }

    pub(crate) fn from_exec_conflict(
        commit: ObjectId,
        conflicts: &[Conflict],
        exec: Vec<ExecResultJson>,
    ) -> Self {
        let mut outcome = Self::from_stop(commit, conflicts, &[]);
        outcome.exec = exec;
        outcome
    }

    pub(crate) fn from_stop(
        commit: ObjectId,
        conflicts: &[Conflict],
        rerere: &[grit_lib::diff::rerere::RerereEvent],
    ) -> Self {
        let block = OperationConflictBlock::for_rebase(conflicts);
        Self {
            command: None,
            paths: Vec::new(),
            result: RebaseResult::Conflicts,
            operation: Some("rebase".into()),
            rewritten: Vec::new(),
            refs: Vec::new(),
            head: None,
            dry_run: false,
            commit: Some(commit.to_hex()),
            conflicts: Some(block.conflicts),
            next_steps: Some(block.next_steps),
            alternatives: block.alternatives,
            exec: Vec::new(),
            rerere: crate::render::operation::rerere_events_json(rerere),
            conflict_sources: Some(conflicts.to_vec()),
        }
    }
}

fn print_history_rewrite_rows(
    w: &mut dyn Write,
    rewritten: &[RewrittenPair],
    refs: &[RefPair],
    head: Option<&HeadPair>,
) -> Result<()> {
    for pair in rewritten {
        match &pair.new {
            Some(new) => {
                if let Some(subject) = pair.subject.as_deref() {
                    outln!(
                        w,
                        "- commit `{}` → `{}` — {subject}",
                        short_hex(&pair.old),
                        short_hex(new)
                    );
                } else {
                    outln!(
                        w,
                        "- commit `{}` → `{}`",
                        short_hex(&pair.old),
                        short_hex(new)
                    );
                }
            }
            None => {
                if let Some(subject) = pair.subject.as_deref() {
                    outln!(
                        w,
                        "- commit `{}` → (dropped) — {subject}",
                        short_hex(&pair.old)
                    );
                } else {
                    outln!(w, "- commit `{}` → (dropped)", short_hex(&pair.old));
                }
            }
        }
    }
    for update in refs {
        outln!(
            w,
            "- `{}` `{}` → `{}`",
            update.ref_name,
            short_hex(&update.old),
            short_hex(&update.new)
        );
    }
    if let Some(head) = head {
        outln!(
            w,
            "- HEAD `{}` → `{}`",
            short_hex(&head.old),
            short_hex(&head.new)
        );
    }
    Ok(())
}

fn print_rebase_outcome(w: &mut dyn Write, outcome: &RebaseCliOutcome) -> Result<()> {
    outln!(w, "## Rebase");
    outln!(w);
    match outcome.result {
        RebaseResult::Aborted => {
            outln!(w, "Rebase aborted.");
            for pair in &outcome.refs {
                outln!(
                    w,
                    "- `{}` was moved during the rebase; reset `{}` → `{}` (the moved tip stays in its reflog)",
                    pair.ref_name,
                    short_hex(&pair.old),
                    short_hex(&pair.new)
                );
            }
        }
        RebaseResult::ExecDirty => {
            outln!(
                w,
                "- Rebase stopped: exec left tracked changes; edits are preserved."
            );
            outln!(
                w,
                "Save edits before continuing or skipping. Aborting discards exec edits."
            );
            for path in &outcome.paths {
                outln!(w, "- `{path}`");
            }
            render_operation_guidance_markdown(
                outcome.next_steps.as_deref().unwrap_or(&[]),
                outcome.alternatives.as_deref(),
                w,
            )?;
        }
        RebaseResult::Conflicts => {
            outln!(w, "- Rebase stopped with conflicts");
            outln!(w);
            if let Some(details) = &outcome.conflict_sources {
                let path_opts = path_display_for_discovered_repo();
                render_conflicts_markdown(details, None, path_opts.as_ref(), w)?;
            }
            render_rerere_markdown(&outcome.rerere, w)?;
            if let Some(commit) = &outcome.commit {
                outln!(
                    w,
                    "- Stopped at commit `{short}`",
                    short = short_hex(commit)
                );
            }
            render_operation_guidance_markdown(
                outcome.next_steps.as_deref().unwrap_or(&[]),
                outcome.alternatives.as_deref(),
                w,
            )?;
        }
        RebaseResult::Completed => {
            if outcome.dry_run {
                outln!(w, "- Dry run (refs unchanged)");
            }
            if outcome.rewritten.is_empty() && outcome.head.is_none() {
                if !outcome.exec.is_empty() {
                    outln!(w, "- Exec commands ran; branch is unchanged");
                } else {
                    outln!(w, "- Already up to date");
                }
            } else {
                print_history_rewrite_rows(
                    w,
                    &outcome.rewritten,
                    &outcome.refs,
                    outcome.head.as_ref(),
                )?;
            }
            render_rerere_markdown(&outcome.rerere, w)?;
        }
    }
    Ok(())
}

impl HumanRender for RebaseCliOutcome {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        match self.result {
            RebaseResult::Aborted => {
                outln!(w, "Rebase aborted.");
                for pair in &self.refs {
                    outln!(
                        w,
                        "  {} was moved during the rebase; reset {} → {} (the moved tip stays in its reflog)",
                        pair.ref_name,
                        short_hex(&pair.old),
                        short_hex(&pair.new)
                    );
                }
            }
            RebaseResult::ExecDirty => {
                outln!(
                    w,
                    "Rebase stopped: exec left tracked changes; edits are preserved."
                );
                outln!(
                    w,
                    "Save edits before continuing or skipping. Aborting discards exec edits."
                );
                for path in &self.paths {
                    outln!(w, "  {path}");
                }
                for step in self.next_steps.as_deref().unwrap_or(&[]) {
                    outln!(w, "{step}");
                }
                for step in self.alternatives.as_deref().unwrap_or(&[]) {
                    outln!(w, "{step}");
                }
            }
            RebaseResult::Conflicts => {
                if let Some(details) = &self.conflict_sources {
                    let path_opts = path_display_for_discovered_repo();
                    render_conflicts_human(details, None, path_opts.as_ref(), w)?;
                }
                render_rerere_human(&self.rerere);
                render_rebase_conflict_footer_human();
            }
            RebaseResult::Completed => {
                HistoryRewriteOutcome {
                    operation: self.operation.clone().unwrap_or_else(|| "rebase".into()),
                    rewritten: self.rewritten.clone(),
                    refs: self.refs.clone(),
                    head: self.head.clone(),
                    dry_run: self.dry_run,
                    exec: self.exec.clone(),
                }
                .render_human(w, ctx)?;
                render_rerere_human(&self.rerere);
            }
        }
        Ok(())
    }
}

impl MarkdownRender for RebaseCliOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        print_rebase_outcome(w, self)
    }
}

/// Either a resumable rebase outcome or a `--check` prediction.
#[derive(Serialize, JsonSchema)]
#[serde(untagged)]
pub enum RebaseRunOutcome {
    Rebase(RebaseCliOutcome),
    Check(RebaseCheckOutcome),
}

impl HumanRender for RebaseRunOutcome {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        match self {
            Self::Rebase(o) => o.render_human(w, ctx),
            Self::Check(o) => o.render_human(w, ctx),
        }
    }
}

impl MarkdownRender for RebaseRunOutcome {
    fn render_markdown(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        match self {
            Self::Rebase(o) => o.render_markdown(w, ctx),
            Self::Check(o) => o.render_markdown(w, ctx),
        }
    }
}
