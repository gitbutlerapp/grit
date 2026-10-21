//! `grit commit` — stage every change and record a new commit.

use anyhow::{bail, Context, Result};
use grit_lib::commit::parse_date_to_git_timestamp;
use grit_lib::config::ident::{author_identity_line, parse_author_arg};
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::error::Error;
use grit_lib::objects::parse_commit;
use grit_lib::porcelain::add::stage_for_commit;
use grit_lib::porcelain::commit::{create_commit, CommitRequest, HookPolicy};
use grit_lib::progress::NullProgress;
use grit_lib::revwalk::rev_parse::resolve_revision_as_commit;
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
#[allow(clippy::too_many_arguments)]
pub fn run(
    message: Option<String>,
    message_file: Option<String>,
    author_override: Option<String>,
    author_date: Option<String>,
    trailers: Vec<String>,
    signoff: bool,
    reset_author: bool,
    reuse_message: Option<String>,
    gpg_sign: bool,
    no_gpg_sign: bool,
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

    let reuse_message_from = if let Some(rev) = reuse_message.as_deref() {
        Some(
            resolve_revision_as_commit(&repo, rev)
                .with_context(|| format!("could not resolve reuse-message commit '{rev}'"))?,
        )
    } else {
        None
    };

    let message =
        resolve_commit_message(message, message_file, amend, reuse_message_from.is_some())?;
    let message_text = message.as_deref().unwrap_or("");

    let config = repo.config().context("could not load config")?;
    let env = repo.environment();
    let now = crate::context::wall_clock_now(env);
    let committer = context::identity(env, config.as_ref(), IdentRole::Committer, now)?;
    let author = if reset_author {
        String::new()
    } else if let Some(arg) = author_override.as_deref() {
        let contact = parse_author_arg(arg).map_err(|e| anyhow::anyhow!("{e}"))?;
        let env_author_date = env.var(IdentRole::Author.env_date_key());
        let date_override = author_date.as_deref().or(env_author_date.as_deref());
        author_identity_line(&contact, date_override, now)
    } else if amend || reuse_message_from.is_some() {
        String::new()
    } else {
        context::identity(env, config.as_ref(), IdentRole::Author, now)?
    };

    let parsed_author_date = author_date.as_deref().map(parse_trailer_date).transpose()?;
    let parsed_trailers = trailers
        .iter()
        .map(|t| parse_trailer_arg(t))
        .collect::<Result<Vec<_>>>()?;

    let sign_override = if gpg_sign {
        Some(true)
    } else if no_gpg_sign {
        Some(false)
    } else {
        None
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
            sign_override,
            amend,
            assume_worktree_synced: true,
            skip_pre_commit_tree_match: true,
            staged_change_count: Some(staged.total()),
            hooks,
            author_date: parsed_author_date,
            trailers: parsed_trailers,
            signoff,
            reset_author,
            reuse_message_from,
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

fn resolve_commit_message(
    message: Option<String>,
    message_file: Option<String>,
    amend: bool,
    reuse_message: bool,
) -> Result<Option<String>> {
    match (message, message_file) {
        (Some(m), None) => Ok(Some(m)),
        (None, Some(path)) => read_message_file(&path).map(Some),
        (None, None) if amend || reuse_message => Ok(None),
        (None, None) => bail!("provide a commit message, e.g. grit commit \"what changed\""),
        (Some(_), Some(_)) => unreachable!("clap rejects message and message-file together"),
    }
}

fn parse_trailer_arg(raw: &str) -> Result<(String, String)> {
    let raw = raw.trim();
    if let Some((key, value)) = raw.split_once('=') {
        return Ok((key.trim().to_owned(), value.trim().to_owned()));
    }
    if let Some((key, value)) = raw.split_once(':') {
        return Ok((key.trim().to_owned(), value.trim().to_owned()));
    }
    bail!("trailer must be key=value or key: value (got '{raw}')");
}

fn parse_trailer_date(raw: &str) -> Result<String> {
    parse_date_to_git_timestamp(raw.trim())
        .ok_or_else(|| anyhow::anyhow!("could not parse --date '{raw}'"))
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
