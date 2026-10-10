//! `grit revert` — undo a single commit with a new inverse commit.

use crate::context;
use crate::output::{HumanRender, MarkdownRender};
use anyhow::{bail, Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::error::Error;
use grit_lib::ident_resolve::IdentRole;
use grit_lib::porcelain::replay::{replay_commit, ReplayDirection, ReplayOutcome, ReplayRequest};
use grit_lib::rev_parse::resolve_revision;
use serde::Serialize;

/// Result of `grit revert`.
#[derive(Serialize)]
pub struct RevertOutcome {
    /// The commit that was reverted (full hex oid).
    pub source: String,
    /// The new revert commit on the current branch (full hex oid).
    pub oid: String,
    /// Subject line of the revert commit.
    pub subject: String,
}

impl HumanRender for RevertOutcome {
    fn render_human(&self) {
        let new_short = self.oid.get(..7).unwrap_or(&self.oid);
        let src_short = self.source.get(..7).unwrap_or(&self.source);
        println!("Reverted {src_short} → {new_short} {}", self.subject);
    }
}

impl MarkdownRender for RevertOutcome {}

/// Revert `commit` on the current branch.
pub fn run(commit: &str) -> Result<RevertOutcome> {
    let repo = context::discover()?;
    let source_oid = resolve_revision(&repo, commit)
        .with_context(|| format!("could not resolve commit '{commit}'"))?;
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
            direction: ReplayDirection::Revert,
            committer,
            message_override: None,
        },
    )
    .map_err(map_replay_error)?;
    let short = &source_oid.to_hex()[..7];
    match outcome {
        ReplayOutcome::Committed { oid, .. } => {
            let reverted = context::read_commit(&repo, &oid)?;
            Ok(RevertOutcome {
                source: source_oid.to_hex(),
                oid: oid.to_hex(),
                subject: context::subject_line(&reverted.message),
            })
        }
        ReplayOutcome::Conflicts { paths } => bail!(
            "revert has conflicts in:\n  {}\n\nNothing was changed. grit can't resolve conflicts yet — run `git revert {short}` to resolve them.",
            paths.join("\n  "),
        ),
        ReplayOutcome::AlreadyApplied => {
            bail!("{short} is already reverted on this branch — nothing to do")
        }
        ReplayOutcome::Empty => {
            bail!("{short} is empty — nothing to revert")
        }
    }
}

fn map_replay_error(err: Error) -> anyhow::Error {
    match err {
        Error::DetachedHead => anyhow::anyhow!("HEAD is detached; grit revert needs a branch"),
        Error::UnbornHead => anyhow::anyhow!("no commits yet on this branch to revert onto"),
        Error::MergeCommit { oid } => {
            let s = &oid.to_hex()[..7];
            anyhow::anyhow!(
                "{s} is a merge commit — grit revert only handles regular commits; use `git revert -m 1 {s}`"
            )
        }
        other => anyhow::Error::new(other),
    }
}
