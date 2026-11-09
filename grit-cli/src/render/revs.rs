//! `grit revs` outcomes and rendering.

use std::io::Write;

use anyhow::Result;
use grit_lib::objects::ObjectId;
use grit_lib::revwalk::LeftRightCounts;
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::{outln, CommitJson};
use crate::render::{HumanRender, MarkdownRender, RenderContext};

/// How revs listing is emitted (mapped from `OutputMode` in dispatch only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RevsEmitKind {
    Human,
    Json,
    Markdown,
}

/// Left/right counts for a symmetric range (`--left-right --count`).
#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
pub struct RevsLeftRightCount {
    pub left: usize,
    pub right: usize,
}

impl From<LeftRightCounts> for RevsLeftRightCount {
    fn from(c: LeftRightCounts) -> Self {
        Self {
            left: c.left,
            right: c.right,
        }
    }
}

/// Which side of a symmetric range (`A...B`) a listed commit is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RevsSide {
    /// Reachable from the left tip only.
    Left,
    /// Reachable from the right tip only.
    Right,
}

/// One listed commit: the shared commit shape plus `--left-right` / `--cherry-mark` marks.
#[derive(Serialize, JsonSchema)]
pub struct RevsCommit {
    #[serde(flatten)]
    pub commit: CommitJson,
    /// Side of the symmetric range, with `--left-right` or `--cherry-mark`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub side: Option<RevsSide>,
    /// With `--cherry-mark`: a commit on the other side has the same patch-id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub equivalent: Option<bool>,
    /// With `--cherry-mark`, for a one-to-one equivalent pair: the other side's commit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub equivalent_to: Option<String>,
    /// With `--cherry-mark`, for a one-to-one equivalent pair: whether the messages differ.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_changed: Option<bool>,
}

/// `--left-right` / `--cherry-mark` annotations of one listed commit.
#[derive(Debug, Default)]
pub(crate) struct RevsMarks {
    pub(crate) side: Option<RevsSide>,
    pub(crate) equivalent: Option<bool>,
    pub(crate) equivalent_to: Option<String>,
    pub(crate) message_changed: Option<bool>,
}

impl RevsMarks {
    pub(crate) fn marker(&self) -> Option<char> {
        match (self.equivalent, self.side?) {
            (Some(true), _) => Some('='),
            (_, RevsSide::Left) => Some('<'),
            (_, RevsSide::Right) => Some('>'),
        }
    }

    pub(crate) fn note(&self) -> Option<String> {
        let other = self.equivalent_to.as_deref()?;
        let short = other.get(..12).unwrap_or(other);
        Some(if self.message_changed == Some(true) {
            format!("message changed, pairs with {short}")
        } else {
            format!("pairs with {short}")
        })
    }
}

impl RevsCommit {
    pub(crate) fn new(commit: CommitJson, marks: RevsMarks) -> Self {
        Self {
            commit,
            side: marks.side,
            equivalent: marks.equivalent,
            equivalent_to: marks.equivalent_to,
            message_changed: marks.message_changed,
        }
    }
}

/// One listed commit as human and Markdown output show it.
#[derive(Debug)]
pub(crate) struct RevsLine {
    pub(crate) oid: ObjectId,
    pub(crate) subject: String,
    pub(crate) marks: RevsMarks,
}

/// Result of `grit revs`.
#[derive(Serialize, JsonSchema)]
pub struct RevsOutcome {
    /// Listed commits in `rev-list` order (empty when `--count` is set).
    pub commits: Vec<RevsCommit>,
    /// The listed commits for human and Markdown output, which need only the id, subject
    /// and marks; [`Self::commits`] stays empty then.
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) lines: Vec<RevsLine>,
    /// Total reachable commits when `--count` is set without `--left-right`; otherwise `null`.
    pub count: Option<usize>,
    /// Left/right counts when `--count` and `--left-right` are set; otherwise `null`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left_right: Option<RevsLeftRightCount>,
    /// True when more commits matched than returned (`-n`).
    pub truncated: bool,
    /// Continuation cursor when the list is resumable; always `null` for `revs` today.
    pub next: Option<String>,
}

impl HumanRender for RevsOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if let Some(lr) = self.left_right {
            outln!(w, "{}\t{}", lr.left, lr.right);
            return Ok(());
        }
        if let Some(n) = self.count {
            outln!(w, "{n}");
            return Ok(());
        }
        for row in &self.lines {
            let oid = &row.oid;
            let subject = &row.subject;
            match (row.marks.marker(), row.marks.note()) {
                (Some(m), Some(note)) => outln!(w, "{m} {oid}  {subject}  ({note})"),
                (Some(m), None) => outln!(w, "{m} {oid}  {subject}"),
                (None, _) => outln!(w, "{oid}  {subject}"),
            }
        }
        Ok(())
    }
}

impl MarkdownRender for RevsOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if let Some(lr) = self.left_right {
            outln!(w, "# Symmetric range counts");
            outln!(w);
            outln!(
                w,
                "**Left:** {left} · **Right:** {right}",
                left = lr.left,
                right = lr.right
            );
            return Ok(());
        }
        if let Some(n) = self.count {
            outln!(w, "# Revision count");
            outln!(w);
            outln!(w, "**{n}** commits reachable.");
            return Ok(());
        }
        outln!(w, "# Commits");
        outln!(w);
        if self.lines.is_empty() {
            outln!(w, "_No commits._");
            return Ok(());
        }
        for row in &self.lines {
            let oid = &row.oid;
            let subject = &row.subject;
            let side = match (row.marks.side, row.marks.equivalent) {
                (Some(RevsSide::Left), Some(true)) => "**left, equivalent** ",
                (Some(RevsSide::Right), Some(true)) => "**right, equivalent** ",
                (Some(RevsSide::Left), _) => "**left** ",
                (Some(RevsSide::Right), _) => "**right** ",
                (None, _) => "",
            };
            match row.marks.note() {
                Some(note) => outln!(w, "- {side}`{oid}` {subject} _({note})_"),
                None => outln!(w, "- {side}`{oid}` {subject}"),
            }
        }
        Ok(())
    }
}
