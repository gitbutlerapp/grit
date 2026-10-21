//! `grit commit` — stage every change and record a new commit.

use anyhow::{bail, Context, Result};
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::error::Error;
use grit_lib::objects::parse_commit;
use grit_lib::porcelain::add::stage_for_commit;
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::progress::NullProgress;
use grit_lib::state::{resolve_head, HeadState};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::context::{self, subject_line};
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// Result of `grit commit`.
#[derive(Serialize, JsonSchema)]
pub struct CommitOutcome {
    pub oid: String,
    pub branch: String,
    pub subject: String,
    pub changes: usize,
    pub amended: bool,
}

impl HumanRender for CommitOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(
            w,
            "[{} {}] {}",
            self.branch,
            self.oid.get(..7).unwrap_or(&self.oid),
            self.subject
        );
        outln!(
            w,
            "{} change{} committed",
            self.changes,
            if self.changes == 1 { "" } else { "s" }
        );
        Ok(())
    }
}

impl MarkdownRender for CommitOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_commit_outcome(w, self)?;
        Ok(())
    }
}

pub fn run(message: Option<String>, amend: bool, allow_empty: bool) -> Result<CommitOutcome> {
    let repo = context::discover()?;

    match resolve_head(repo.git_dir())? {
        HeadState::Branch { .. } => {}
        HeadState::Detached { .. } => bail!("HEAD is detached; grit commit needs a branch"),
        HeadState::Invalid => bail!("HEAD is in an unknown state"),
    }

    let staged = stage_for_commit(&repo, &mut NullProgress).map_err(|e| anyhow::anyhow!("{e}"))?;

    if repo.index_handle().load()?.has_unmerged_entries() {
        return Err(map_commit_error(Error::IndexUnmerged));
    }

    let message = match message {
        Some(m) if !m.trim().is_empty() => Some(m),
        None if amend => None,
        _ => bail!("provide a commit message, e.g. grit commit \"what changed\""),
    };
    let message_text = message.as_deref().unwrap_or("");

    let config = repo.config().context("could not load config")?;
    let env = repo.environment();
    let now = crate::context::wall_clock_now(env);
    let committer = context::identity(env, config.as_ref(), IdentRole::Committer, now)?;
    let author = if amend {
        // `create_commit` reuses the tip commit's author when amending; strict author resolution
        // is not required (and would wrongly fail committer-only amend flows).
        String::new()
    } else {
        context::identity(env, config.as_ref(), IdentRole::Author, now)?
    };

    let outcome = create_commit(
        &repo,
        &CommitRequest {
            message: message_text.to_owned(),
            author,
            committer,
            allow_empty,
            sign_override: None,
            amend,
            assume_worktree_synced: true,
            skip_pre_commit_tree_match: true,
            staged_change_count: Some(staged.total()),
        },
        &mut NullProgress,
    )
    .map_err(map_commit_error)?;

    let subject = if let Some(m) = message.as_ref().filter(|m| !m.trim().is_empty()) {
        subject_line(&format!("{}\n", m.trim()))
    } else {
        let obj = repo.odb().read(&outcome.oid).context("read new commit")?;
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
            "cannot commit: unresolved merge conflicts remain — resolve each conflicted path (grit add or grit rm), then try again"
        ),
        Error::AmendUnborn => anyhow::anyhow!("cannot amend: no commits on this branch yet"),
        other => anyhow::anyhow!("{other}"),
    }
}
