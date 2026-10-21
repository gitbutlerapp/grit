//! `grit commit` — stage every change and record a new commit.

use anyhow::{bail, Context, Result};
use grit_lib::config::ident::{author_identity_line, parse_author_arg};
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::error::Error;
use grit_lib::objects::parse_commit;
use grit_lib::porcelain::add::stage_for_commit;
use grit_lib::porcelain::commit::{
    create_commit, normalize_commit_message, CommitRequest, HookPolicy,
};
use grit_lib::progress::NullProgress;
use grit_lib::state::{resolve_head, HeadState};
use grit_lib::HookName;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::{self, Read, Write};

use crate::context::{self, subject_line};
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// Author recorded on the new commit (`--json` / `--markdown`).
#[derive(Serialize, JsonSchema, Clone)]
pub struct CommitAuthorOutcome {
    pub name: String,
    pub email: String,
    pub date: String,
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
    pub parents: Vec<String>,
    pub author: CommitAuthorOutcome,
    pub hooks_run: Vec<String>,
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
        if !self.hooks_run.is_empty() {
            outln!(w, "hooks: {}", self.hooks_run.join(", "));
        }
        Ok(())
    }
}

impl MarkdownRender for CommitOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_commit_outcome(w, self)?;
        Ok(())
    }
}

/// Stage the worktree and create a commit with the given message and options.
pub fn run(
    message: Option<String>,
    message_file: Option<String>,
    author_override: Option<String>,
    no_verify: bool,
    amend: bool,
    allow_empty: bool,
) -> Result<CommitOutcome> {
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

    let message = resolve_commit_message(message, message_file, amend)?;
    let message_text = message.as_deref().unwrap_or("");

    let config = repo.config().context("could not load config")?;
    let env = repo.environment();
    let now = crate::context::wall_clock_now(env);
    let committer = context::identity(env, config.as_ref(), IdentRole::Committer, now)?;
    let author = if let Some(arg) = author_override {
        let contact = parse_author_arg(&arg).map_err(|e| anyhow::anyhow!("{e}"))?;
        let date_override = env.var(IdentRole::Author.env_date_key());
        author_identity_line(&contact, date_override.as_deref(), now)
    } else if amend {
        String::new()
    } else {
        context::identity(env, config.as_ref(), IdentRole::Author, now)?
    };

    let hooks = if no_verify {
        HookPolicy::SkipVerify
    } else {
        HookPolicy::Run
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
            hooks,
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
        change_id: outcome.change_id.as_ref().map(|id| id.as_str().to_owned()),
        parents: outcome.parents.iter().map(|p| p.to_hex()).collect(),
        author: CommitAuthorOutcome {
            name: outcome.author.name,
            email: outcome.author.email,
            date: outcome.author.date,
        },
        hooks_run: outcome
            .hooks_run
            .iter()
            .map(|h| hook_name_json(*h))
            .collect(),
    })
}

fn hook_name_json(hook: HookName) -> String {
    hook.as_str().to_owned()
}

const COMMIT_MESSAGE_REQUIRED: &str = "provide a commit message, e.g. grit commit \"what changed\"";

fn resolve_commit_message(
    message: Option<String>,
    message_file: Option<String>,
    amend: bool,
) -> Result<Option<String>> {
    match (message, message_file) {
        (Some(m), None) => normalize_explicit_commit_message(&m),
        (None, Some(path)) => {
            let raw = read_message_file(&path)?;
            normalize_explicit_commit_message(&raw)
        }
        (None, None) if amend => Ok(None),
        (None, None) => bail!(COMMIT_MESSAGE_REQUIRED),
        (Some(_), Some(_)) => unreachable!("clap rejects message and message-file together"),
    }
}

fn normalize_explicit_commit_message(raw: &str) -> Result<Option<String>> {
    match normalize_commit_message(raw) {
        Ok(normalized) => Ok(Some(normalized)),
        Err(Error::Message(_)) => bail!(COMMIT_MESSAGE_REQUIRED),
        Err(err) => Err(anyhow::anyhow!("{err}")),
    }
}

fn read_message_file(path: &str) -> Result<String> {
    if path == "-" {
        let mut buf = String::new();
        io::stdin()
            .read_to_string(&mut buf)
            .context("could not read message from stdin")?;
        return Ok(buf);
    }
    std::fs::read_to_string(path).with_context(|| format!("could not read message file '{path}'"))
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
