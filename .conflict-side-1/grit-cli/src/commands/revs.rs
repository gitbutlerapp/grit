//! `grit revs` — list or count commits matching revision ranges.

use anyhow::{bail, Result};
use grit_lib::objects::{read_commit_subject_line, ObjectId};
use grit_lib::revwalk::RevWalkListOptions;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::context;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// One commit row in `grit revs` output.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RevsCommitRow {
    pub oid: String,
    pub subject: String,
}

/// Result of `grit revs`.
#[derive(Serialize, JsonSchema)]
pub struct RevsOutcome {
    /// Listed commits in `rev-list` order (empty when `--count` is set).
    pub commits: Vec<RevsCommitRow>,
    /// Total reachable commits when `--count` is set; otherwise `null`.
    pub count: Option<usize>,
}

impl HumanRender for RevsOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
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

pub fn run(
    ranges: &[String],
    count: bool,
    limit: Option<usize>,
    first_parent: bool,
    paths: &[String],
) -> Result<RevsOutcome> {
    if ranges.is_empty() {
        bail!("at least one revision range is required");
    }
    let repo = context::discover()?;
    let walk = repo.rev_walk();
    let opts = RevWalkListOptions {
        limit,
        first_parent,
        paths: paths.to_vec(),
    };
    let token_refs: Vec<&str> = ranges.iter().map(String::as_str).collect();
    if count {
        let n = walk.count(&token_refs, &opts)?;
        return Ok(RevsOutcome {
            commits: Vec::new(),
            count: Some(n),
        });
    }
    let oids = walk.commits(&token_refs, &opts)?;
    let commits = load_commit_rows(&repo, &oids)?;
    Ok(RevsOutcome {
        commits,
        count: None,
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
