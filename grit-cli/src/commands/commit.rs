//! `grit commit` — record a new commit with configurable staging scope.

use anyhow::{bail, Context, Result};
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::error::Error;
use grit_lib::ident::{author_identity_line, parse_author_arg};
use grit_lib::objects::parse_commit;
use grit_lib::porcelain::add::stage_for_commit;
use grit_lib::porcelain::commit::{
    commit_with_scope, default_commit_scope, CommitRequest, CommitScope, HookPolicy,
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
use crate::pathspec_args::TrailingPathspecArgs;

/// JSON scope label for [`CommitOutcome::scope`].
#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommitScopeJson {
    AutoStage,
    Staged,
    Paths,
}

/// Author recorded on the new commit (`--json` / `--markdown`).
#[derive(Serialize, JsonSchema, Clone)]
pub struct CommitAuthorOutcome {
    pub name: String,
    pub email: String,
    pub date: String,
}

/// One path that would be or was included in the commit (`--json`).
#[derive(Serialize, JsonSchema, Clone)]
pub struct CommitFileOutcome {
    pub path: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
}

/// Result of `grit commit`.
#[derive(Serialize, JsonSchema)]
pub struct CommitOutcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oid: Option<String>,
    pub branch: String,
    pub subject: String,
    pub changes: usize,
    pub amended: bool,
    pub dry_run: bool,
    pub scope: CommitScopeJson,
    pub files: Vec<CommitFileOutcome>,
    /// Stored `change-id` header on the new commit, or JSON `null` when absent or dry-run.
    pub change_id: Option<String>,
    pub parents: Vec<String>,
    pub author: CommitAuthorOutcome,
    pub hooks_run: Vec<String>,
}

impl HumanRender for CommitOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.dry_run {
            outln!(
                w,
                "Would commit {} file{}",
                self.files.len(),
                if self.files.len() == 1 { "" } else { "s" }
            );
            const MAX_LIST: usize = 20;
            for file in self.files.iter().take(MAX_LIST) {
                if let Some(ref old) = file.old_path {
                    outln!(w, "  {} {} -> {}", file.status, old, file.path);
                } else {
                    outln!(w, "  {} {}", file.status, file.path);
                }
            }
            if self.files.len() > MAX_LIST {
                outln!(w, "  … and {} more", self.files.len() - MAX_LIST);
            }
            return Ok(());
        }
        outln!(
            w,
            "[{} {}] {}",
            self.branch,
            self.oid.as_deref().unwrap_or("").get(..7).unwrap_or(""),
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

/// Run `grit commit` with the given message, flags, and optional path scope.
#[allow(clippy::too_many_arguments)]
pub fn run(
    message: Option<String>,
    message_file: Option<String>,
    author_override: Option<String>,
    no_verify: bool,
    dry_run: bool,
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

    let message = resolve_commit_message(message, message_file, amend, &scope, &repo)?;
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

    let req = CommitRequest {
        message: message_text.to_owned(),
        author,
        committer,
        allow_empty,
        sign_override: None,
        amend,
        assume_worktree_synced: false,
        skip_pre_commit_tree_match: false,
        staged_change_count: None,
        hooks,
        dry_run,
    };

    let outcome =
        commit_with_scope(&repo, &req, scope, &mut NullProgress).map_err(map_commit_error)?;

    let subject = if dry_run {
        subject_line(&format!("{}\n", message_text.trim()))
    } else if let Some(m) = message.as_ref().filter(|m| !m.trim().is_empty()) {
        subject_line(&format!("{}\n", m.trim()))
    } else {
        let obj = repo.odb().read(&outcome.oid).context("read new commit")?;
        subject_line(&parse_commit(&obj.data).context("parse new commit")?.message)
    };

    let files = outcome
        .files
        .iter()
        .map(|f| CommitFileOutcome {
            path: f.path.clone(),
            status: f.status.letter().to_string(),
            old_path: f.old_path.clone(),
        })
        .collect();

    Ok(CommitOutcome {
        oid: if dry_run {
            None
        } else {
            Some(outcome.oid.to_hex())
        },
        branch: outcome.branch,
        subject,
        changes: outcome.changes,
        amended: amend,
        dry_run,
        scope: scope_json,
        files,
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

fn resolve_commit_message(
    message: Option<String>,
    message_file: Option<String>,
    amend: bool,
    scope: &CommitScope,
    repo: &grit_lib::repo::Repository,
) -> Result<Option<String>> {
    match (message, message_file) {
        (Some(m), None) => Ok(Some(m)),
        (None, Some(path)) => read_message_file(&path).map(Some),
        (None, None) if amend => Ok(None),
        (None, None) => {
            if matches!(scope, CommitScope::AutoStage) {
                stage_for_commit(repo, &mut NullProgress).map_err(|e| anyhow::anyhow!("{e}"))?;
            }
            bail!("provide a commit message, e.g. grit commit \"what changed\"");
        }
        (Some(_), Some(_)) => unreachable!("clap rejects message and message-file together"),
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
        Error::PartialCommitDuringMerge => {
            anyhow::anyhow!("cannot do a partial commit during a merge")
        }
        Error::PathspecNoMatch { spec } => anyhow::anyhow!(
            "pathspec '{spec}' did not match any file(s) known to git"
        ),
        other => anyhow::anyhow!("{other}"),
    }
}
