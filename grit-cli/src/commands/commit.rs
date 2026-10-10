//! `grit commit` — stage every change and record a new commit.

use anyhow::{bail, Context, Result};
use grit_lib::error::Error;
use grit_lib::ident_resolve::IdentRole;
use grit_lib::objects::parse_commit;
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::progress::NullProgress;
use serde::Serialize;

use crate::commands::add;
use crate::context::{self, subject_line};
use crate::output::{HumanRender, MarkdownRender};

/// Result of `grit commit`.
#[derive(Serialize)]
pub struct CommitOutcome {
    pub oid: String,
    pub branch: String,
    pub subject: String,
    pub changes: usize,
    pub amended: bool,
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

impl MarkdownRender for CommitOutcome {}

pub fn run(message: Option<String>, amend: bool) -> Result<CommitOutcome> {
    let repo = context::discover()?;

    if repo.load_index()?.has_unmerged_entries() {
        return Err(map_commit_error(Error::IndexUnmerged));
    }

    add::stage(&repo, &[])?;

    let message = match message {
        Some(m) if !m.trim().is_empty() => Some(m),
        None if amend => None,
        _ => bail!("provide a commit message, e.g. grit commit \"what changed\""),
    };
    let message_text = message.as_deref().unwrap_or("");

    let config = repo.config().context("could not load config")?;
    let env = repo.environment();
    let now = crate::context::wall_clock_now(env);
    let author = context::identity(
        env,
        config.as_ref(),
        IdentRole::Author,
        "GIT_AUTHOR_DATE",
        now,
    )?;
    let committer = context::identity(
        env,
        config.as_ref(),
        IdentRole::Committer,
        "GIT_COMMITTER_DATE",
        now,
    )?;

    let outcome = create_commit(
        &repo,
        &CommitRequest {
            message: message_text.to_owned(),
            author,
            committer,
            allow_empty: false,
            sign_override: None,
            amend,
        },
        &mut NullProgress,
    )
    .map_err(map_commit_error)?;

    let subject = if let Some(m) = message.as_ref().filter(|m| !m.trim().is_empty()) {
        subject_line(&format!("{}\n", m.trim()))
    } else {
        let obj = repo.odb.read(&outcome.oid).context("read new commit")?;
        subject_line(&parse_commit(&obj.data).context("parse new commit")?.message)
    };

    Ok(CommitOutcome {
        oid: outcome.oid.to_hex(),
        branch: outcome.branch,
        subject,
        changes: outcome.changes,
        amended: amend,
    })
}

fn map_commit_error(err: Error) -> anyhow::Error {
    match err {
        Error::NothingToCommit => anyhow::anyhow!("nothing to commit — working tree clean"),
        Error::DetachedHead => anyhow::anyhow!("HEAD is detached; grit commit needs a branch"),
        Error::IndexUnmerged => anyhow::anyhow!(
            "cannot commit: the index still has unmerged paths — resolve conflicts and stage the result"
        ),
        Error::AmendUnborn => anyhow::anyhow!("cannot amend: no commits on this branch yet"),
        other => anyhow::anyhow!("{other}"),
    }
}
