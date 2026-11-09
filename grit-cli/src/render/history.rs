//! `grit history` rewrite outcomes and rendering.

use anyhow::Result;
use grit_lib::objects::short_hex;
use grit_lib::revwalk::history::{
    format_hunk_range_header, HunkAssignment, HunkRange, RewritePlan, UnassignedHunk,
};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::render::table::escape_table_cell;

fn short_change_id(id: &str) -> &str {
    id.get(..7).unwrap_or(id)
}

/// One `rebase --exec` command result in `--json` output.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ExecResultJson {
    /// Commit OID (full hex) checked out when the command ran.
    pub commit: String,
    /// Shell command string.
    pub command: String,
    /// Subprocess exit code.
    pub exit_code: i32,
}

/// Result of `grit history drop`, `move`, `reword`, `fixup`, `squash`, `absorb`, `rebase`, and `undo`, and top-level `grit rebase`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HistoryRewriteOutcome {
    /// Operation name (`drop`, `move`, `reword`, `fixup`, `squash`, `rebase`, or `undo`).
    pub operation: String,
    /// Original and replacement commit OIDs for every rewritten commit.
    pub rewritten: Vec<RewrittenPair>,
    /// Planned or applied ref tip updates.
    pub refs: Vec<RefPair>,
    /// Previous and new `HEAD` commit OIDs when `HEAD`'s commit changes; JSON `null` when unchanged.
    pub head: Option<HeadPair>,
    /// Whether refs were left unchanged (`true` for `--dry-run`).
    pub dry_run: bool,
    /// `--exec` subprocess results when `--exec` was used (success only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exec: Vec<ExecResultJson>,
}

/// One commit OID before and after rewrite.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RewrittenPair {
    /// Commit OID before rewrite (full hex).
    pub old: String,
    /// Replacement commit OID (full hex), or JSON `null` when the commit was dropped.
    pub new: Option<String>,
    /// Stored `change-id` on the replacement commit, or JSON `null` when dropped or absent.
    pub change_id: Option<String>,
    /// First line of the replacement commit message, or the original when dropped.
    pub subject: Option<String>,
}

/// One ref tip before and after rewrite.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RefPair {
    /// Full ref name.
    #[serde(rename = "ref")]
    pub ref_name: String,
    /// Deprecated duplicate of [`Self::ref_name`]; removed in a future release.
    pub name: String,
    /// Previous tip OID (full hex).
    pub old: String,
    /// New tip OID (full hex).
    pub new: String,
}

/// `HEAD` commit before and after rewrite.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HeadPair {
    /// Commit OID at `HEAD` before rewrite (full hex).
    pub old: String,
    /// Commit OID at `HEAD` after rewrite (full hex).
    pub new: String,
}

impl HistoryRewriteOutcome {
    /// Outcome when there is nothing to replay (`rewritten` is empty).
    pub fn up_to_date(operation: &str, dry_run: bool) -> Self {
        Self {
            operation: operation.into(),
            rewritten: Vec::new(),
            refs: Vec::new(),
            head: None,
            dry_run,
            exec: Vec::new(),
        }
    }

    pub(crate) fn new(operation: &str, plan: &RewritePlan, dry_run: bool) -> Self {
        Self {
            operation: operation.into(),
            rewritten: plan
                .commits
                .iter()
                .map(|row| RewrittenPair {
                    old: row.old.to_hex(),
                    new: row.new.map(|new| new.to_hex()),
                    change_id: row.change_id.as_ref().map(|id| id.as_str().to_owned()),
                    subject: row.subject.clone(),
                })
                .collect(),
            refs: plan
                .ref_updates
                .iter()
                .map(|u| RefPair {
                    ref_name: u.name.to_string(),
                    name: u.name.to_string(),
                    old: u.old.to_hex(),
                    new: u.new.to_hex(),
                })
                .collect(),
            head: plan.head_change().map(|(old, new)| HeadPair {
                old: old.to_hex(),
                new: new.to_hex(),
            }),
            dry_run,
            exec: Vec::new(),
        }
    }

    pub(crate) fn with_exec(mut self, exec: Vec<ExecResultJson>) -> Self {
        self.exec = exec;
        self
    }
}

fn render_rewrite_rows_human(
    w: &mut dyn Write,
    rewritten: &[RewrittenPair],
    refs: &[RefPair],
) -> Result<()> {
    for pair in rewritten {
        match &pair.new {
            Some(new) => {
                if let Some(subject) = pair.subject.as_deref() {
                    outln!(
                        w,
                        "  commit {} → {} ({subject})",
                        short_hex(&pair.old),
                        short_hex(new)
                    );
                } else {
                    outln!(w, "  commit {} → {}", short_hex(&pair.old), short_hex(new));
                }
            }
            None => {
                if let Some(subject) = pair.subject.as_deref() {
                    outln!(
                        w,
                        "  commit {} → (dropped) ({subject})",
                        short_hex(&pair.old)
                    );
                } else {
                    outln!(w, "  commit {} → (dropped)", short_hex(&pair.old));
                }
            }
        }
    }
    for update in refs {
        outln!(
            w,
            "  {} {} → {}",
            update.ref_name,
            short_hex(&update.old),
            short_hex(&update.new)
        );
    }
    Ok(())
}

fn history_rewrite_title(operation: &str) -> &'static str {
    match operation {
        "drop" => "History drop",
        "move" => "History move",
        "fixup" => "History fixup",
        "squash" => "History squash",
        "reword" => "History reword",
        "rebase" => "History rebase",
        "split" => "History split",
        "undo" => "History undo",
        _ => "History rewrite",
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
                if let Some(id) = pair.change_id.as_deref() {
                    outln!(w, "  - change id `{short}`", short = short_change_id(id));
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

fn print_history_rewrite_outcome(
    w: &mut dyn Write,
    title: &str,
    outcome: &HistoryRewriteOutcome,
) -> Result<()> {
    outln!(w, "## {title}");
    outln!(w);
    if outcome.operation == "rebase" && outcome.rewritten.is_empty() && outcome.head.is_none() {
        if !outcome.exec.is_empty() {
            outln!(w, "- Exec commands ran; branch is unchanged");
        } else {
            outln!(w, "- Already up to date");
        }
        return Ok(());
    }
    if outcome.dry_run {
        outln!(w, "- Dry run (refs unchanged)");
    }
    print_history_rewrite_rows(w, &outcome.rewritten, &outcome.refs, outcome.head.as_ref())
}

fn print_history_undo_restores(
    w: &mut dyn Write,
    dry_run: bool,
    restored_paths: &[String],
    unrestored_edits: Option<&str>,
) -> Result<()> {
    let verb = if dry_run { "would restore" } else { "restored" };
    for path in restored_paths {
        outln!(w, "- {verb} `{path}` (index and work tree)");
    }
    if let Some(oid) = unrestored_edits {
        outln!(
            w,
            "- staged edits from the undone command were not restored; they remain in commit `{}`",
            short_hex(oid)
        );
    }
    Ok(())
}

impl HumanRender for HistoryRewriteOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.operation == "rebase" && self.rewritten.is_empty() && self.head.is_none() {
            if !self.exec.is_empty() {
                outln!(w, "Exec commands ran; branch is unchanged");
                return Ok(());
            }
            if let Some(head) = &self.head {
                outln!(
                    w,
                    "Rebased branch tip from {} to {}",
                    short_hex(&head.old),
                    short_hex(&head.new)
                );
                return Ok(());
            }
            outln!(w, "Already up to date");
            return Ok(());
        }
        if self.dry_run {
            if self.operation == "undo" {
                outln!(w, "Dry run — undo would restore refs:");
            } else {
                outln!(w, "Dry run — refs would change:");
            }
        } else if self.operation == "undo" {
            outln!(w, "Undid last history rewrite");
        } else if self.operation == "drop" {
            outln!(
                w,
                "Dropped 1 commit, replayed {} descendant commit(s)",
                self.rewritten.len()
            );
        } else if self.operation == "rebase" {
            outln!(w, "Rebased {} commit(s)", self.rewritten.len());
        } else {
            let verb = match self.operation.as_str() {
                "fixup" => "Fixed up",
                "squash" => "Squashed",
                "split" => "Split",
                _ => "Rewrote",
            };
            outln!(w, "{verb} {} commit(s)", self.rewritten.len());
        }
        render_rewrite_rows_human(w, &self.rewritten, &self.refs)
    }
}

impl MarkdownRender for HistoryRewriteOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        print_history_rewrite_outcome(w, history_rewrite_title(&self.operation), self)
    }
}

/// Result of `grit history undo`: the ref movement plus any edits put back.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HistoryUndoOutcome {
    /// Refs and `HEAD` the undo moved (or would move).
    #[serde(flatten)]
    pub rewrite: HistoryRewriteOutcome,
    /// Paths whose staged and work tree edits were (or, with `--dry-run`, would be) put back,
    /// because the undone fixup or absorb had folded them into history. Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restored_paths: Vec<String>,
    /// Commit (full hex) that still holds the edits an undone fixup or absorb folded in, when
    /// they could not be put back. Omitted otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unrestored_edits: Option<String>,
}

impl HumanRender for HistoryUndoOutcome {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        self.rewrite.render_human(w, ctx)?;
        let verb = if self.rewrite.dry_run {
            "would restore"
        } else {
            "restored"
        };
        for path in &self.restored_paths {
            outln!(w, "  {verb} {path} (index and work tree)");
        }
        if let Some(oid) = &self.unrestored_edits {
            outln!(
                w,
                "Staged edits from the undone command were not restored; they remain in commit {}",
                short_hex(oid)
            );
        }
        Ok(())
    }
}

impl MarkdownRender for HistoryUndoOutcome {
    fn render_markdown(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        self.rewrite.render_markdown(w, ctx)?;
        print_history_undo_restores(
            w,
            self.rewrite.dry_run,
            &self.restored_paths,
            self.unrestored_edits.as_deref(),
        )
    }
}

/// JSON/markdown assignment row for `grit history absorb`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct AbsorbAssignmentOut {
    /// Repository-relative path.
    pub path: String,
    /// Staged hunk range in the index↔`HEAD` diff.
    pub hunk_range: HunkRangeOut,
    /// Target commit (full hex).
    pub commit: String,
    /// Target commit subject (first line, folded).
    pub subject: String,
}

/// Hunk coordinates for JSON output.
#[derive(Debug, Serialize, JsonSchema)]
pub struct HunkRangeOut {
    /// Old side start line (1-based).
    pub old_start: usize,
    /// Old side line count.
    pub old_lines: usize,
    /// New side start line (1-based).
    pub new_start: usize,
    /// New side line count.
    pub new_lines: usize,
}

impl From<&HunkRange> for HunkRangeOut {
    fn from(range: &HunkRange) -> Self {
        Self {
            old_start: range.old_start,
            old_lines: range.old_lines,
            new_start: range.new_start,
            new_lines: range.new_lines,
        }
    }
}

impl From<&HunkAssignment> for AbsorbAssignmentOut {
    fn from(row: &HunkAssignment) -> Self {
        Self {
            path: row.path.clone(),
            hunk_range: (&row.hunk_range).into(),
            commit: row.commit.to_hex(),
            subject: row.subject.clone(),
        }
    }
}

impl From<&UnassignedHunk> for UnassignedHunkOut {
    fn from(row: &UnassignedHunk) -> Self {
        Self {
            path: row.path.clone(),
            hunk_range: (&row.hunk_range).into(),
            reason: row.reason.as_str().into(),
            detail: row.detail.clone(),
        }
    }
}

/// JSON/markdown unassigned hunk row.
#[derive(Debug, Serialize, JsonSchema)]
pub struct UnassignedHunkOut {
    /// Repository-relative path.
    pub path: String,
    /// Staged hunk range.
    pub hunk_range: HunkRangeOut,
    /// Stable reason code.
    pub reason: String,
    /// Human-readable detail when the reason alone is not enough (for example `hidden_by_ref`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Local branch tip excluded from the absorb stack.
#[derive(Debug, Serialize, JsonSchema)]
pub struct AbsorbExcludedRefOut {
    /// Full ref name.
    #[serde(rename = "ref")]
    pub ref_name: String,
    /// Tip commit OID (full hex).
    pub commit: String,
}

/// Result of `grit history absorb`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct HistoryAbsorbOutcome {
    /// Operation name (`absorb`).
    pub operation: String,
    /// Assigned hunks.
    pub assignments: Vec<AbsorbAssignmentOut>,
    /// Hunks left staged.
    pub unassigned: Vec<UnassignedHunkOut>,
    /// Local branch tips hidden from the absorb stack (empty when none apply).
    pub excluded_refs: Vec<AbsorbExcludedRefOut>,
    /// Original and replacement commit OIDs for every rewritten commit.
    pub rewritten: Vec<RewrittenPair>,
    /// Planned or applied ref tip updates.
    pub refs: Vec<RefPair>,
    /// Previous and new `HEAD` commit OIDs when `HEAD`'s commit changes; JSON `null` when unchanged.
    pub head: Option<HeadPair>,
    /// Whether refs were left unchanged (`true` for `--dry-run`).
    pub dry_run: bool,
}

#[must_use]
pub(crate) fn format_absorb_unassigned_reason(row: &UnassignedHunkOut) -> String {
    if let Some(detail) = &row.detail {
        format!("{}: {detail}", row.reason)
    } else {
        row.reason.clone()
    }
}

fn format_absorb_hunk_header(range: &HunkRangeOut) -> String {
    format_hunk_range_header(&HunkRange {
        old_start: range.old_start,
        old_lines: range.old_lines,
        new_start: range.new_start,
        new_lines: range.new_lines,
    })
}

fn print_history_absorb_outcome(w: &mut dyn Write, outcome: &HistoryAbsorbOutcome) -> Result<()> {
    outln!(w, "## History absorb");
    outln!(w);
    if outcome.dry_run {
        outln!(w, "- Dry run (refs unchanged)");
        outln!(w);
    }
    if !outcome.assignments.is_empty() {
        outln!(w, "### Assigned hunks");
        outln!(w);
        outln!(w, "| Path | Hunk | Commit | Subject |");
        outln!(w, "| --- | --- | --- | --- |");
        for row in &outcome.assignments {
            outln!(
                w,
                "| `{}` | `{}` | `{}` | {} |",
                escape_table_cell(&row.path),
                format_absorb_hunk_header(&row.hunk_range),
                short_hex(&row.commit),
                escape_table_cell(&row.subject)
            );
        }
        outln!(w);
    }
    if !outcome.unassigned.is_empty() {
        outln!(w, "### Unassigned hunks");
        outln!(w);
        outln!(w, "| Path | Hunk | Reason |");
        outln!(w, "| --- | --- | --- |");
        for row in &outcome.unassigned {
            outln!(
                w,
                "| `{}` | `{}` | {} |",
                escape_table_cell(&row.path),
                format_absorb_hunk_header(&row.hunk_range),
                escape_table_cell(&format_absorb_unassigned_reason(row))
            );
        }
        outln!(w);
    }
    print_history_rewrite_rows(w, &outcome.rewritten, &outcome.refs, outcome.head.as_ref())
}

impl HumanRender for HistoryAbsorbOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.dry_run {
            outln!(
                w,
                "Dry run — absorb would change {} commit(s)",
                self.rewritten.len()
            );
        } else {
            outln!(
                w,
                "Absorbed {} hunk(s) into {} commit(s)",
                self.assignments.len(),
                self.rewritten.len()
            );
        }
        for row in &self.assignments {
            let header = format_absorb_hunk_header(&row.hunk_range);
            outln!(
                w,
                "  {} {} → {} {}",
                row.path,
                header,
                short_hex(&row.commit),
                row.subject
            );
        }
        for row in &self.unassigned {
            let header = format_absorb_hunk_header(&row.hunk_range);
            outln!(
                w,
                "  {} {} — {}",
                row.path,
                header,
                format_absorb_unassigned_reason(row)
            );
        }
        render_rewrite_rows_human(w, &self.rewritten, &self.refs)
    }
}

impl MarkdownRender for HistoryAbsorbOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        print_history_absorb_outcome(w, self)
    }
}
