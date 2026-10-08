//! `grit commit` — stage every change and record a new commit.

use anyhow::{bail, Context, Result};
use grit_lib::commit::now_from_environment;
use grit_lib::error::Error;
use grit_lib::ident_resolve::IdentRole;
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::progress::NullProgress;
use serde::Serialize;

use crate::commands::add;
use crate::context::{self, subject_line};
use crate::output::HumanRender;

/// Result of `grit commit`.
#[derive(Serialize)]
pub struct CommitOutcome {
    pub oid: String,
    pub branch: String,
    pub subject: String,
    pub changes: usize,
}

impl HumanRender for CommitOutcome {
    fn render_human(&self) {
        println!(
            "[{} {}] {}",
            self.branch,
            self.oid.get(..7).unwrap_or(&self.oid),
            self.subject
        );
        println!(
            "{} change{} committed",
            self.changes,
            if self.changes == 1 { "" } else { "s" }
        );
    }
}

pub fn run(message: Option<String>) -> Result<CommitOutcome> {
    let repo = context::discover()?;

    add::stage(&repo, &[])?;

    let message = match message {
        Some(m) if !m.trim().is_empty() => m,
        _ => bail!("provide a commit message, e.g. grit commit \"what changed\""),
    };

    let config = repo.config().context("could not load config")?;
    let env = repo.environment();
    let now = now_from_environment(env);
    let author = context::identity(env, config.as_ref(), IdentRole::Author, "GIT_AUTHOR_DATE", now)?;
    let committer = context::identity(
        env,
        config.as_ref(),
        IdentRole::Committer,
        "GIT_COMMITTER_DATE",
        now,
    )?;

    let subject = subject_line(&format!("{}\n", message.trim()));

    let outcome = create_commit(
        &repo,
        &CommitRequest {
            message,
            author,
            committer,
            allow_empty: false,
        },
        &mut NullProgress,
    )
    .map_err(map_commit_error)?;

    Ok(CommitOutcome {
        oid: outcome.oid.to_hex(),
        branch: outcome.branch,
        subject,
        changes: outcome.changes,
    })
}

fn map_commit_error(err: Error) -> anyhow::Error {
    match err {
        Error::NothingToCommit => anyhow::anyhow!("nothing to commit — working tree clean"),
        Error::DetachedHead => anyhow::anyhow!("HEAD is detached; grit commit needs a branch"),
        other => anyhow::anyhow!("{other}"),
    }
}
