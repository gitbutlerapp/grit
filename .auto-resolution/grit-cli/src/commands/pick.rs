//! `grit pick` — cherry-pick a single commit onto the current branch.

use crate::context;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use anyhow::{bail, Context, Result};
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::config::ConfigSet;
use grit_lib::error::Error;
use grit_lib::porcelain::replay::{replay_commit, ReplayDirection, ReplayOutcome, ReplayRequest};
use grit_lib::revwalk::rev_parse::resolve_revision_as_commit;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

/// Result of `grit pick`.
#[derive(Serialize, JsonSchema)]
pub struct PickOutcome {
    /// The original commit that was picked (full hex oid).
    pub source: String,
    /// The new commit created on the current branch (full hex oid).
    pub oid: String,
    /// Subject line of the picked commit.
    pub subject: String,
}

impl HumanRender for PickOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let new_short = self.oid.get(..7).unwrap_or(&self.oid);
        let src_short = self.source.get(..7).unwrap_or(&self.source);
        outln!(w, "Picked {src_short} → {new_short} {}", self.subject);
        Ok(())
    }
}

impl MarkdownRender for PickOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_pick_outcome(w, self)?;
        Ok(())
    }
}

/// Cherry-pick `commit` onto the current branch.
pub fn run(commit: &str) -> Result<PickOutcome> {
    let repo = context::discover()?;
    let source_oid = resolve_revision_as_commit(&repo, commit)
        .map_err(|err| map_pick_committish_error(err, commit))?;
    let source = context::read_commit(&repo, &source_oid)?;
    let env = repo.environment();
    let config =
        ConfigSet::load(env, Some(repo.git_dir()), true).context("could not load config")?;
    let committer = context::identity(
        env,
        &config,
        IdentRole::Committer,
        "GIT_COMMITTER_DATE",
        context::wall_clock_now(env),
    )?;
    let outcome = replay_commit(
        &repo,
        &ReplayRequest {
            commit: source_oid,
            direction: ReplayDirection::Pick,
            committer,
            message_override: None,
        },
    )
    .map_err(map_replay_error)?;
    let short = &source_oid.to_hex()[..7];
    match outcome {
        ReplayOutcome::Committed { oid, .. } => Ok(PickOutcome {
            source: source_oid.to_hex(),
            oid: oid.to_hex(),
            subject: context::subject_line(&source.message),
        }),
        ReplayOutcome::Conflicts { mut paths } => {
            paths.sort();
            paths.dedup();
            Err(crate::json_error::operation_conflict("pick", paths))
        }
        ReplayOutcome::Empty => {
            bail!("{short} is empty (its tree matches its parent) — nothing to pick")
        }
        ReplayOutcome::AlreadyApplied => {
            bail!("{short} is already applied on this branch — nothing to pick")
        }
    }
}

fn map_pick_committish_error(err: Error, spec: &str) -> anyhow::Error {
    match err {
        Error::InvalidRef(msg) => anyhow::anyhow!(msg),
        Error::ObjectNotFound(msg) => {
            anyhow::anyhow!("could not resolve commit '{spec}': {msg}")
        }
        other => anyhow::Error::new(other),
    }
}

fn map_replay_error(err: Error) -> anyhow::Error {
    match err {
        Error::DetachedHead => anyhow::anyhow!("HEAD is detached; grit pick needs a branch"),
        Error::UnbornHead => anyhow::anyhow!("no commits yet on this branch to pick onto"),
        Error::MergeCommit { oid } => {
            let s = &oid.to_hex()[..7];
            anyhow::anyhow!(
                "{s} is a merge commit — grit pick only handles regular commits; use `git cherry-pick -m 1 {s}`"
            )
        }
        Error::ReplaySourceAtHead => {
            anyhow::anyhow!("nothing to pick — that commit is already the current HEAD")
        }
        other => anyhow::Error::new(other),
    }
}
