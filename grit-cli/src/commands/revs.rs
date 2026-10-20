//! `grit revs` — list or count commits matching revision ranges.

use anyhow::{bail, Result};
use grit_lib::objects::{read_commit_subject_line, ObjectId};
use grit_lib::revwalk::LeftRightCounts;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::context;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::rev_select::RevSelectArgs;

/// One commit row in `grit revs` output.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RevsCommitRow {
    pub oid: String,
    pub subject: String,
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

/// Result of `grit revs`.
#[derive(Serialize, JsonSchema)]
pub struct RevsOutcome {
    /// Listed commits in `rev-list` order (empty when `--count` is set).
    pub commits: Vec<RevsCommitRow>,
    /// Total reachable commits when `--count` is set without `--left-right`; otherwise `null`.
    pub count: Option<usize>,
    /// Left/right counts when `--count` and `--left-right` are set; otherwise `null`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left_right: Option<RevsLeftRightCount>,
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
        for row in &self.commits {
            outln!(w, "{}  {}", row.oid, row.subject);
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
        if self.commits.is_empty() {
            outln!(w, "_No commits._");
            return Ok(());
        }
        for row in &self.commits {
            outln!(w, "- `{}` {}", row.oid, row.subject);
        }
        Ok(())
    }
}

pub fn run(rev: RevSelectArgs, count: bool, left_right: bool) -> Result<RevsOutcome> {
    if rev.revs.is_empty()
        && !rev.all
        && rev.branches.is_none()
        && rev.tags.is_none()
        && rev.remotes.is_none()
    {
        bail!("at least one revision range is required");
    }
    if left_right && !count {
        bail!("`--left-right` requires `--count`");
    }
    let repo = context::discover()?;
    let env = repo.environment();
    let mut deprecated = None;
    let mut warn = false;
    let opts = rev.to_rev_walk_options(&repo, env, &[], &mut deprecated, &mut warn)?;
    if deprecated.is_some() {
        bail!("`grit revs` does not support log continuation cursors");
    }
    let walk = repo.rev_walk();
    let token_refs = rev.rev_tokens(&[]);

    if count {
        if rev.max_count == Some(0) {
            return Ok(RevsOutcome {
                commits: Vec::new(),
                count: Some(0),
                left_right: left_right.then_some(RevsLeftRightCount { left: 0, right: 0 }),
            });
        }
        if left_right {
            let lr = walk.left_right_counts(&token_refs, &opts)?;
            return Ok(RevsOutcome {
                commits: Vec::new(),
                count: None,
                left_right: Some(lr.into()),
            });
        }
        let n = walk.count(&token_refs, &opts)?;
        return Ok(RevsOutcome {
            commits: Vec::new(),
            count: Some(n),
            left_right: None,
        });
    }

    if rev.max_count == Some(0) {
        return Ok(RevsOutcome {
            commits: Vec::new(),
            count: None,
            left_right: None,
        });
    }
    let oids = walk.commits(&token_refs, &opts)?;
    let commits = load_commit_rows(&repo, &oids)?;
    Ok(RevsOutcome {
        commits,
        count: None,
        left_right: None,
    })
}

fn load_commit_rows(
    repo: &grit_lib::repo::Repository,
    oids: &[ObjectId],
) -> Result<Vec<RevsCommitRow>> {
    let mut out = Vec::with_capacity(oids.len());
    for oid in oids {
        let obj = repo.odb().read(oid)?;
        let subject = read_commit_subject_line(&obj.data)?;
        out.push(RevsCommitRow {
            oid: oid.to_hex(),
            subject,
        });
    }
    Ok(out)
}
