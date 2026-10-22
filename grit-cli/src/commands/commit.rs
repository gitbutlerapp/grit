//! `grit commit` — record a new commit with configurable staging scope.

use anyhow::{bail, Context, Result};
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::error::Error;
use grit_lib::objects::parse_commit;
use grit_lib::porcelain::add::stage_for_commit;
use grit_lib::porcelain::commit::{
    commit_with_scope, default_commit_scope, pending_commit_message, CommitRequest, CommitScope,
};
use grit_lib::progress::NullProgress;
use grit_lib::state::{resolve_head, HeadState};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::context::{self, subject_line};
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::pathspec_args::TrailingPathspecArgs;

/// JSON scope label for [`CommitOutcome::scope`].
#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommitScopeJson {
    AutoStage,
    Staged,
    Paths,
}

/// Result of `grit commit`.
#[derive(Serialize, JsonSchema)]
pub struct CommitOutcome {
    pub oid: String,
    pub branch: String,
    pub subject: String,
    pub changes: usize,
    pub amended: bool,
    /// Stored `change-id` header on the new commit, or JSON `null` when absent.
    pub change_id: Option<String>,
    pub scope: CommitScopeJson,
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

/// Run `grit commit` with the given message, flags, and optional path scope.
pub fn run(
    message: Option<String>,
    amend: bool,
    allow_empty: bool,
    all: bool,
    staged: bool,
    paths: TrailingPathspecArgs,
) -> Result<CommitOutcome> {
    let repo = context::discover()?;

    match resolve_head(repo.git_dir())? {
        HeadState::Branch { .. } => {}
        HeadState::Detached { .. } => bail!("HEAD is detached; grit commit needs a branch"),
        HeadState::Invalid => bail!("HEAD is in an unknown state"),
    }

    let scope = resolve_commit_scope(&repo, all, staged, &paths)?;
    let scope_json = scope_json(&scope);

    let message = match message {
        Some(m) if !m.trim().is_empty() => Some(m),
        None if amend => None,
        None => pending_commit_message(&repo)
            .context("could not read pending commit message")?
            .filter(|m| !m.trim().is_empty()),
        _ => None,
    };
    let message = match message {
        Some(m) => Some(m),
        None => {
            if matches!(scope, CommitScope::AutoStage) {
                stage_for_commit(&repo, &mut NullProgress).map_err(|e| anyhow::anyhow!("{e}"))?;
            }
            bail!("provide a commit message, e.g. grit commit \"what changed\"");
        }
    };
    let message_text = message.as_deref().unwrap_or("");

    let config = repo.config().context("could not load config")?;
    let env = repo.environment();
    let now = crate::context::wall_clock_now(env);
    let committer = context::identity(env, config.as_ref(), IdentRole::Committer, now)?;
    let author = if amend {
        String::new()
    } else {
        context::identity(env, config.as_ref(), IdentRole::Author, now)?
    };

    let outcome = commit_with_scope(
        &repo,
        &CommitRequest {
            message: message_text.to_owned(),
            author,
            committer,
            allow_empty,
            sign_override: None,
            amend,
            assume_worktree_synced: false,
            skip_pre_commit_tree_match: false,
            staged_change_count: None,
        },
        scope,
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
        change_id: outcome.change_id.as_ref().map(|id| id.as_str().to_owned()),
        scope: scope_json,
    })
}

fn resolve_commit_scope(
    repo: &grit_lib::repo::Repository,
    all: bool,
    staged: bool,
    paths: &TrailingPathspecArgs,
) -> Result<CommitScope> {
    let has_paths = !paths.paths.is_empty() || paths.pathspec_from_file.is_some();
    let flags = usize::from(all) + usize::from(staged) + usize::from(has_paths);
    if flags > 1 {
        bail!("use only one of --all, --staged, or path arguments");
    }
    if all {
        return Ok(CommitScope::AutoStage);
    }
    if staged {
        return Ok(CommitScope::Staged);
    }
    if has_paths {
        paths.require_non_empty()?;
        let resolved = paths.resolve(repo)?;
        return Ok(CommitScope::Paths(resolved));
    }
    default_commit_scope(repo).map_err(|e| anyhow::anyhow!("{e}"))
}

fn scope_json(scope: &CommitScope) -> CommitScopeJson {
    match scope {
        CommitScope::AutoStage => CommitScopeJson::AutoStage,
        CommitScope::Staged => CommitScopeJson::Staged,
        CommitScope::Paths(_) => CommitScopeJson::Paths,
    }
}

fn map_commit_error(err: Error) -> anyhow::Error {
    match err {
        Error::NothingToCommit => anyhow::anyhow!("nothing to commit — working tree clean"),
        Error::DetachedHead => anyhow::anyhow!("HEAD is detached; grit commit needs a branch"),
        Error::IndexUnmerged => anyhow::anyhow!(
            "cannot commit: unresolved merge conflicts remain — resolve each conflicted path (grit add or grit rm), then try again"
        ),
        Error::AmendUnborn => anyhow::anyhow!("cannot amend: no commits on this branch yet"),
        Error::PartialCommitDuringMerge => {
            anyhow::anyhow!("cannot do a partial commit during a merge")
        }
        Error::PathspecNoMatch { spec } => anyhow::anyhow!(
            "pathspec '{spec}' did not match any file(s) known to git"
        ),
        other => anyhow::anyhow!("{other}"),
    }
}
