//! `grit revs` — list or count commits matching revision ranges.

use anyhow::{bail, Result};
use grit_lib::objects::ObjectId;
use grit_lib::revwalk::rev_list::RevListResult;
use grit_lib::revwalk::LeftRightCounts;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::commits_json::{fetch_limit_plus_one, truncate_oid_list, CommitJsonLoader};
use crate::context;
use crate::output::{outln, CommitJson, HumanRender, MarkdownRender, OutputMode, RenderContext};
use crate::pathspec_args::TrailingPathspecArgs;
use crate::rev_select::RevSelectArgs;

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
struct RevsMarks {
    side: Option<RevsSide>,
    equivalent: Option<bool>,
    equivalent_to: Option<String>,
    message_changed: Option<bool>,
}

impl RevsMarks {
    fn marker(&self) -> Option<char> {
        match (self.equivalent, self.side?) {
            (Some(true), _) => Some('='),
            (_, RevsSide::Left) => Some('<'),
            (_, RevsSide::Right) => Some('>'),
        }
    }

    fn note(&self) -> Option<String> {
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
    fn new(commit: CommitJson, marks: RevsMarks) -> Self {
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
struct RevsLine {
    oid: ObjectId,
    subject: String,
    marks: RevsMarks,
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
    lines: Vec<RevsLine>,
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

/// Counting and side-marking switches for [`run`].
#[derive(Debug, Clone, Copy, Default)]
pub struct RevsMode {
    /// `--count`: print counts instead of commits.
    pub count: bool,
    /// `--left-right`: mark sides when listing, or count each side with `--count`.
    pub left_right: bool,
    /// `--cherry-mark`: pair the sides by patch-id when listing.
    pub cherry_mark: bool,
}

fn commit_marks(oids: &[ObjectId], result: &RevListResult, mode: RevsMode) -> Vec<RevsMarks> {
    let sides = mode.left_right || mode.cherry_mark;
    let matches = result.cherry_match_index();
    oids.iter()
        .map(|oid| {
            let side = sides
                .then(|| result.left_right_map.get(oid))
                .flatten()
                .map(|&left| {
                    if left {
                        RevsSide::Left
                    } else {
                        RevsSide::Right
                    }
                });
            let group = matches.get(oid);
            let counterpart = group.and_then(|g| g.counterpart(oid));
            RevsMarks {
                side,
                equivalent: mode.cherry_mark.then(|| group.is_some()),
                equivalent_to: counterpart.map(|o| o.to_hex()),
                message_changed: counterpart.and(group.and_then(|g| g.message_changed)),
            }
        })
        .collect()
}

pub fn run(
    rev: RevSelectArgs,
    pathspecs: TrailingPathspecArgs,
    mode: RevsMode,
    output_mode: OutputMode,
) -> Result<RevsOutcome> {
    let RevsMode {
        count, left_right, ..
    } = mode;
    if rev.revs.is_empty()
        && !rev.all
        && rev.branches.is_none()
        && rev.tags.is_none()
        && rev.remotes.is_none()
    {
        bail!("at least one revision range is required");
    }
    let repo = context::discover()?;
    let env = repo.environment();
    let walk_paths = pathspecs.rev_walk_tokens(&repo)?;
    let opts = rev.to_rev_walk_options(&repo, env, &[], &walk_paths)?;
    let walk = repo.rev_walk();
    let token_refs = rev.rev_tokens(&[]);
    if (left_right || mode.cherry_mark) && rev.max_count() == Some(0) {
        walk.symmetric_range_tips(&token_refs)?;
    }
    if count {
        if rev.max_count() == Some(0) {
            return Ok(RevsOutcome {
                commits: Vec::new(),
                lines: Vec::new(),
                count: if left_right { None } else { Some(0) },
                left_right: left_right.then_some(RevsLeftRightCount { left: 0, right: 0 }),
                truncated: false,
                next: None,
            });
        }
        if left_right {
            let lr = walk.left_right_counts(&token_refs, &opts)?;
            return Ok(RevsOutcome {
                commits: Vec::new(),
                lines: Vec::new(),
                count: None,
                left_right: Some(lr.into()),
                truncated: false,
                next: None,
            });
        }
        let n = walk.count(&token_refs, &opts)?;
        return Ok(RevsOutcome {
            commits: Vec::new(),
            lines: Vec::new(),
            count: Some(n),
            left_right: None,
            truncated: false,
            next: None,
        });
    }

    if rev.max_count() == Some(0) {
        return Ok(RevsOutcome {
            commits: Vec::new(),
            lines: Vec::new(),
            count: None,
            left_right: None,
            truncated: false,
            next: None,
        });
    }
    let mut fetch_opts = opts.clone();
    fetch_opts.limit = fetch_limit_plus_one(rev.max_count());
    fetch_opts.retain_commit_data = true;
    fetch_opts.left_right = left_right;
    fetch_opts.cherry_mark = mode.cherry_mark;
    let mut list_result = walk.rev_list_result(&token_refs, &fetch_opts)?;
    let (oids, truncated) = truncate_oid_list(
        std::mem::take(&mut list_result.commits),
        rev.max_count(),
        rev.reverse,
    );
    let marks = commit_marks(&oids, &list_result, mode);
    let mut parsed = std::mem::take(&mut list_result.commit_data);
    let mut outcome = RevsOutcome {
        commits: Vec::new(),
        lines: Vec::new(),
        count: None,
        left_right: None,
        truncated,
        next: None,
    };
    if output_mode == OutputMode::Json {
        let now = context::wall_clock_now(env).unix_timestamp();
        let commits = repo
            .odb()
            .with_pack_read_context(|| {
                CommitJsonLoader::for_listed_commits().load_with_summaries_reusing(
                    &repo,
                    &oids,
                    now,
                    &mut parsed,
                )
            })?
            .0;
        outcome.commits = commits
            .into_iter()
            .zip(marks)
            .map(|(c, m)| RevsCommit::new(c, m))
            .collect();
    } else {
        let summaries = repo.odb().with_pack_read_context(|| {
            CommitJsonLoader::load_summaries_only(&repo, &oids, &mut parsed)
        })?;
        outcome.lines = summaries
            .into_iter()
            .zip(marks)
            .map(|(s, marks)| RevsLine {
                oid: s.oid,
                subject: s.subject,
                marks,
            })
            .collect();
    }
    Ok(outcome)
}
