//! `grit pick` — cherry-pick a single commit onto the current branch.

use crate::context;
use crate::output::{HumanRender, MarkdownRender};
use anyhow::{bail, Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::error::Error;
use grit_lib::ident_resolve::IdentRole;
use grit_lib::porcelain::replay::{replay_commit, ReplayDirection, ReplayOutcome, ReplayRequest};
use grit_lib::rev_parse::resolve_revision;
use serde::Serialize;

/// Result of `grit pick`.
#[derive(Serialize)]
pub struct PickOutcome {
    /// The original commit that was picked (full hex oid).
    pub source: String,
    /// The new commit created on the current branch (full hex oid).
    pub oid: String,
    /// Subject line of the picked commit.
    pub subject: String,
}

impl HumanRender for PickOutcome {
    fn render_human(&self) {
        let new_short = self.oid.get(..7).unwrap_or(&self.oid);
        let src_short = self.source.get(..7).unwrap_or(&self.source);
        println!("Picked {src_short} → {new_short} {}", self.subject);
    }
}

impl MarkdownRender for PickOutcome {}

/// Cherry-pick `commit` onto the current branch.
pub fn run(commit: &str) -> Result<PickOutcome> {
    let repo = context::discover()?;
    let source_oid = resolve_revision(&repo, commit)
        .with_context(|| format!("could not resolve commit '{commit}'"))?;
    let source = context::read_commit(&repo, &source_oid)?;
    let env = repo.environment();
    let config =
        ConfigSet::load(env, Some(&repo.git_dir), true).context("could not load config")?;
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
