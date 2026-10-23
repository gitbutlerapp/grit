//! Shared JSON, human, and markdown fragments for paused operations (merge, pick, revert).

use crate::output::outln;
use anyhow::Result;
use grit_lib::operation::{Conflict, ConflictKind};
use grit_lib::state::{operation_alternatives, operation_next_steps, InProgressOperation};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

/// One conflicted path in `--json` operation output.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ConflictJson {
    pub path: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// Shared conflict + next-step block for merge, pick, revert, and pull.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct OperationConflictBlock {
    pub conflicts: Vec<ConflictJson>,
    /// Ordered follow-ups (resolve → stage → continue/commit).
    pub next_steps: Vec<String>,
    /// Mutually exclusive options (abort, skip) — not part of the ordered sequence.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub alternatives: Option<Vec<String>>,
}

impl OperationConflictBlock {
    /// Build conflict JSON and suggested `grit` commands for a paused merge.
    #[must_use]
    pub fn for_merge(conflicts: &[Conflict], merge_label: &str) -> Self {
        Self::for_operation(InProgressOperation::Merge, conflicts, |c| {
            conflict_json(c, Some(merge_label))
        })
    }

    /// Build conflict JSON and suggested `grit` commands for a paused revert sequence.
    #[must_use]
    pub fn for_revert(conflicts: &[Conflict]) -> Self {
        Self::for_operation(InProgressOperation::Revert, conflicts, |c| {
            conflict_json(c, None)
        })
    }

    /// Build conflict JSON and suggested `grit` commands for a paused pick sequence.
    #[must_use]
    pub fn for_pick(conflicts: &[Conflict]) -> Self {
        Self::for_operation(InProgressOperation::CherryPick, conflicts, |c| {
            conflict_json(c, None)
        })
    }

    /// Build conflict JSON and suggested `grit` commands for a paused rebase session.
    #[must_use]
    pub fn for_rebase(conflicts: &[Conflict]) -> Self {
        Self::for_operation(InProgressOperation::Rebase, conflicts, |c| {
            conflict_json(c, None)
        })
    }

    fn for_operation(
        kind: InProgressOperation,
        conflicts: &[Conflict],
        map_conflict: impl Fn(&Conflict) -> ConflictJson,
    ) -> Self {
        let has_unmerged = !conflicts.is_empty();
        let next_steps =
            grit_lib::operation::next_step_grit_commands(&operation_next_steps(kind, has_unmerged));
        let alternatives =
            grit_lib::operation::next_step_grit_commands(&operation_alternatives(kind));
        Self {
            conflicts: conflicts.iter().map(map_conflict).collect(),
            next_steps,
            alternatives: if alternatives.is_empty() {
                None
            } else {
                Some(alternatives)
            },
        }
    }
}

/// Map a library [`Conflict`] to CLI JSON, optionally with merge-style summaries.
#[must_use]
pub fn conflict_json(conflict: &Conflict, merge_label: Option<&str>) -> ConflictJson {
    let (summary, kind) = match conflict.kind {
        ConflictKind::Content => (None, conflict.kind.as_str().to_owned()),
        ConflictKind::AddAdd => (None, conflict.kind.as_str().to_owned()),
        ConflictKind::ModifyDelete { deleted_in_current } => {
            let summary = merge_label.map(|merge_label| {
                if deleted_in_current {
                    format!(
                        "{} deleted on the current branch and modified in {merge_label}; {merge_label}'s version is in the work tree",
                        conflict.path
                    )
                } else {
                    format!(
                        "{} modified on the current branch and deleted in {merge_label}; the current branch's version is in the work tree",
                        conflict.path
                    )
                }
            });
            (summary, conflict.kind.as_str().to_owned())
        }
    };
    ConflictJson {
        path: conflict.path.clone(),
        kind,
        summary,
    }
}

/// Human stderr lines for conflict summaries (merge-style).
pub fn render_conflicts_human(conflicts: &[ConflictJson], w: &mut dyn Write) -> Result<()> {
    for item in conflicts {
        if let Some(summary) = &item.summary {
            eprintln!("CONFLICT ({kind}): {summary}", kind = item.kind);
        } else {
            eprintln!("CONFLICT ({kind}): {}", item.path, kind = item.kind);
        }
    }
    let _ = w;
    Ok(())
}

/// Markdown bullet list for conflicts.
pub fn render_conflicts_markdown(conflicts: &[ConflictJson], w: &mut dyn Write) -> Result<()> {
    for item in conflicts {
        if let Some(summary) = &item.summary {
            outln!(w, "- **{}** — {summary}", item.kind);
        } else {
            outln!(w, "- **{}** — `{path}`", item.kind, path = item.path);
        }
    }
    Ok(())
}

/// Markdown footer listing ordered next steps and optional alternatives.
pub fn render_operation_guidance_markdown(
    next_steps: &[String],
    alternatives: Option<&[String]>,
    w: &mut dyn Write,
) -> Result<()> {
    if next_steps.is_empty() && alternatives.is_none_or(|a| a.is_empty()) {
        return Ok(());
    }
    outln!(w);
    if !next_steps.is_empty() {
        let joined = next_steps
            .iter()
            .map(|s| format!("`{s}`"))
            .collect::<Vec<_>>()
            .join(", then ");
        outln!(w, "→ {joined}");
    }
    if let Some(alt) = alternatives.filter(|a| !a.is_empty()) {
        let joined = alt
            .iter()
            .map(|s| format!("`{s}`"))
            .collect::<Vec<_>>()
            .join(" or ");
        outln!(w, "→ or {joined}");
    }
    Ok(())
}

/// Human hint after conflicts (merge-specific wording).
pub fn render_merge_conflict_footer_human() {
    eprintln!(
        "\nAutomatic merge failed; fix conflicts and run `grit merge --continue`, or run `grit merge --abort`."
    );
}

/// Human output for `grit merge --check` when the merge would conflict.
pub fn render_merge_check_conflicts_human(
    branch: &str,
    conflicts: &[ConflictJson],
    w: &mut dyn Write,
) -> Result<()> {
    outln!(w, "Would conflict merging {branch}");
    for item in conflicts {
        outln!(w, "  {}", item.path);
    }
    Ok(())
}

/// Markdown output for `grit merge --check` when the merge would conflict.
pub fn render_merge_check_conflicts_markdown(
    branch: &str,
    conflicts: &[ConflictJson],
    w: &mut dyn Write,
) -> Result<()> {
    outln!(w, "- Would conflict merging **`{branch}`**");
    for item in conflicts {
        outln!(w, "  - `{path}`", path = item.path);
    }
    Ok(())
}

/// Human hint after a paused revert sequence.
pub fn render_revert_conflict_footer_human() {
    eprintln!(
        "\nRevert stopped on conflicts; fix them and run `grit revert --continue`, or `grit revert --abort` / `grit revert --skip`."
    );
}

/// Human hint after a paused rebase session.
pub fn render_rebase_conflict_footer_human() {
    eprintln!(
        "\nRebase stopped on conflicts; fix them and run `grit rebase --continue`, or `grit rebase --abort` / `grit rebase --skip`."
    );
}
