//! `grit revs` — list or count commits matching revision ranges.

use anyhow::{bail, Result};
use grit_lib::objects::{read_commit_subject_line, ObjectId};
use grit_lib::revwalk::{
    refresh_parent_rewrite_entries, CherryMode, LeftRightCounts, RevWalkCommit, RevWalkSide,
};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::commits_json::{fetch_limit_plus_one, truncate_oid_list, CommitJsonLoader};
use crate::context;
use crate::output::{outln, CommitJson, HumanRender, MarkdownRender, OutputMode, RenderContext};
use crate::rev_select::RevSelectArgs;

/// Left/right counts for a symmetric range (`--left-right --count`).
#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
pub struct RevsLeftRightCount {
    pub left: usize,
    pub right: usize,
    /// Present for `--cherry-mark --left-right --count` (Git's third column).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub equivalent: Option<usize>,
}

impl From<LeftRightCounts> for RevsLeftRightCount {
    fn from(c: LeftRightCounts) -> Self {
        Self {
            left: c.left,
            right: c.right,
            equivalent: c.equivalent,
        }
    }
}

/// Walk modifiers specific to `grit revs` (not shared with `log`).
#[derive(Debug, Clone, Copy, Default)]
pub struct RevsWalkFlags {
    pub parents: bool,
    pub cherry_pick: bool,
    pub cherry_mark: bool,
    pub left_right: bool,
}

/// Result of `grit revs`.
#[derive(Serialize, JsonSchema)]
pub struct RevsOutcome {
    /// Listed commits in `rev-list` order (empty when `--count` is set).
    pub commits: Vec<CommitJson>,
    /// Total reachable commits when `--count` is set without `--left-right`; otherwise `null`.
    pub count: Option<usize>,
    /// Left/right counts when `--count` and `--left-right` are set; otherwise `null`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left_right: Option<RevsLeftRightCount>,
    /// True when more commits matched than returned (`-n`).
    pub truncated: bool,
    /// Continuation cursor when the list is resumable; always `null` for `revs` today.
    pub next: Option<String>,
    /// Whether human/markdown lines include rewritten parent oids (`--parents`).
    #[serde(skip)]
    pub list_with_parents: bool,
}

impl HumanRender for RevsOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if let Some(lr) = self.left_right {
            if let Some(eq) = lr.equivalent {
                outln!(w, "{}\t{}\t{}", lr.left, lr.right, eq);
            } else {
                outln!(w, "{}\t{}", lr.left, lr.right);
            }
            return Ok(());
        }
        if let Some(n) = self.count {
            outln!(w, "{n}");
            return Ok(());
        }
        for row in &self.commits {
            outln!(w, "{}", format_revs_line(row, true, self.list_with_parents));
        }
        Ok(())
    }
}

impl MarkdownRender for RevsOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if let Some(lr) = self.left_right {
            outln!(w, "# Symmetric range counts");
            outln!(w);
            if let Some(eq) = lr.equivalent {
                outln!(
                    w,
                    "**Left:** {left} · **Right:** {right} · **Equivalent:** {eq}",
                    left = lr.left,
                    right = lr.right,
                    eq = eq
                );
            } else {
                outln!(
                    w,
                    "**Left:** {left} · **Right:** {right}",
                    left = lr.left,
                    right = lr.right
                );
            }
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
            outln!(
                w,
                "- {}",
                format_revs_line(row, false, self.list_with_parents)
            );
        }
        Ok(())
    }
}

pub fn run(
    rev: RevSelectArgs,
    count: bool,
    walk_flags: RevsWalkFlags,
    output: OutputMode,
) -> Result<RevsOutcome> {
    if rev.revs.is_empty()
        && !rev.all
        && rev.branches.is_none()
        && rev.tags.is_none()
        && rev.remotes.is_none()
    {
        bail!("at least one revision range is required");
    }
    if walk_flags.cherry_pick && walk_flags.cherry_mark {
        bail!("`--cherry-pick` and `--cherry-mark` cannot be used together");
    }
    let repo = context::discover()?;
    let env = repo.environment();
    let mut deprecated = None;
    let mut warn = false;
    let mut opts = rev.to_rev_walk_options(env, &[], &mut deprecated, &mut warn)?;
    if deprecated.is_some() {
        bail!("`grit revs` does not support log continuation cursors");
    }
    opts.include_parents = walk_flags.parents;
    opts.left_right = walk_flags.left_right;
    opts.cherry = if walk_flags.cherry_pick {
        CherryMode::Pick
    } else if walk_flags.cherry_mark {
        CherryMode::Mark
    } else {
        CherryMode::Off
    };

    let walk = repo.rev_walk();
    let token_refs = rev.rev_tokens(&[]);

    if count {
        if rev.max_count == Some(0) {
            return Ok(RevsOutcome {
                commits: Vec::new(),
                count: if walk_flags.left_right { None } else { Some(0) },
                left_right: walk_flags.left_right.then_some(RevsLeftRightCount {
                    left: 0,
                    right: 0,
                    equivalent: walk_flags.cherry_mark.then_some(0),
                }),
                truncated: false,
                next: None,
                list_with_parents: false,
            });
        }
        if walk_flags.left_right {
            let lr = walk.left_right_counts(&token_refs, &opts)?;
            return Ok(RevsOutcome {
                commits: Vec::new(),
                count: None,
                left_right: Some(lr.into()),
                truncated: false,
                next: None,
                list_with_parents: false,
            });
        }
        let n = walk.count(&token_refs, &opts)?;
        return Ok(RevsOutcome {
            commits: Vec::new(),
            count: Some(n),
            left_right: None,
            truncated: false,
            next: None,
            list_with_parents: false,
        });
    }

    if rev.max_count == Some(0) {
        return Ok(RevsOutcome {
            commits: Vec::new(),
            count: None,
            left_right: None,
            truncated: false,
            next: None,
            list_with_parents: walk_flags.parents,
        });
    }

    let mut fetch_opts = opts.clone();
    let use_limit_plus_one = rev.max_count.is_some() && !walk_flags.parents;
    fetch_opts.limit = if use_limit_plus_one {
        fetch_limit_plus_one(rev.max_count)
    } else {
        rev.max_count
    };
    let mut entries = walk.commit_entries(&token_refs, &fetch_opts)?;
    let oids: Vec<ObjectId> = entries.iter().map(|e| e.oid).collect();
    let (oids, truncated) = truncate_oid_list(oids, rev.max_count);
    entries.truncate(oids.len());
    if walk_flags.parents {
        refresh_parent_rewrite_entries(&repo, &mut entries, &opts)?;
    }

    let now = context::wall_clock_now(env).unix_timestamp();
    let commits = if output == OutputMode::Human {
        load_revs_commits_light(&repo, &entries, walk_flags)?
    } else {
        let loader = CommitJsonLoader::new(&repo, &oids)?;
        let mut rows = loader.load(&repo, &oids, now)?;
        apply_walk_metadata(&mut rows, &entries, walk_flags);
        rows
    };

    Ok(RevsOutcome {
        commits,
        count: None,
        left_right: None,
        truncated,
        next: None,
        list_with_parents: walk_flags.parents,
    })
}

fn load_revs_commits_light(
    repo: &grit_lib::repo::Repository,
    entries: &[RevWalkCommit],
    walk_flags: RevsWalkFlags,
) -> Result<Vec<CommitJson>> {
    let mut out = Vec::with_capacity(entries.len());
    for row in entries {
        let obj = repo.odb().read(&row.oid)?;
        let subject = read_commit_subject_line(&obj.data)?;
        let parents = if walk_flags.parents {
            row.parents.iter().map(ObjectId::to_hex).collect()
        } else {
            Vec::new()
        };
        let side = if walk_flags.left_right {
            row.side.map(|s| match s {
                RevWalkSide::Left => "left".to_owned(),
                RevWalkSide::Right => "right".to_owned(),
            })
        } else {
            None
        };
        let equivalent = if walk_flags.cherry_mark && row.equivalent {
            Some(true)
        } else {
            None
        };
        out.push(CommitJson {
            oid: row.oid.to_hex(),
            parents,
            author: crate::output::Person {
                name: String::new(),
                email: String::new(),
                date: String::new(),
            },
            committer: crate::output::Person {
                name: String::new(),
                email: String::new(),
                date: String::new(),
            },
            subject: subject.clone(),
            message: subject,
            refs: Vec::new(),
            relative_date: None,
            side,
            equivalent,
        });
    }
    Ok(out)
}

fn apply_walk_metadata(
    commits: &mut [CommitJson],
    entries: &[RevWalkCommit],
    walk_flags: RevsWalkFlags,
) {
    for (json, row) in commits.iter_mut().zip(entries.iter()) {
        if walk_flags.parents {
            json.parents = row.parents.iter().map(ObjectId::to_hex).collect();
        }
        if walk_flags.left_right {
            if let Some(side) = row.side {
                json.side = Some(match side {
                    RevWalkSide::Left => "left".to_owned(),
                    RevWalkSide::Right => "right".to_owned(),
                });
            }
        }
        if walk_flags.cherry_mark && row.equivalent {
            json.equivalent = Some(true);
        }
    }
}

fn format_revs_line(row: &CommitJson, human_prefix: bool, show_parents: bool) -> String {
    let mut out = String::new();
    if human_prefix {
        if row.equivalent == Some(true) {
            out.push('=');
        } else if let Some(side) = row.side.as_deref() {
            out.push(match side {
                "left" => '<',
                "right" => '>',
                _ => ' ',
            });
        }
    } else if row.equivalent == Some(true) {
        out.push_str("(equivalent) ");
    } else if let Some(side) = row.side.as_deref() {
        out.push_str(match side {
            "left" => "(left) ",
            "right" => "(right) ",
            _ => "",
        });
    }
    out.push_str(&row.oid);
    if show_parents {
        for parent in &row.parents {
            out.push(' ');
            out.push_str(parent);
        }
    }
    out.push_str("  ");
    out.push_str(&row.subject);
    out
}
