//! `grit commit` — record a new commit with configurable staging scope.

use anyhow::{bail, Context, Result};
use grit_lib::commit::{normalize_commit_date_override, parse_trailer_command_line_arg};

use crate::message_input::parse_author_cli;
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::error::Error;
use grit_lib::objects::parse_commit;
use grit_lib::porcelain::commit::{
    commit_with_patch, commit_with_scope, default_commit_scope, pending_commit_message,
    CommitPatchError, CommitPatchInput, CommitRequest, CommitScope, HookPolicy,
};
use grit_lib::porcelain::hunk_select::{parse_hunk_id_list, HunkSelectError};
use grit_lib::progress::NullProgress;
use grit_lib::revwalk::rev_parse::resolve_revision_as_commit;
use grit_lib::state::{resolve_head, HeadState};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::{self, Read, Write};

use crate::context::{self, subject_line};
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, Person, RenderContext};
use crate::pathspec_args::TrailingPathspecArgs;

/// JSON scope label for [`CommitOutcome::scope`].
#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommitScopeJson {
    AutoStage,
    Staged,
    Paths,
    Patch,
}

/// Author recorded on the new commit (`--json` / `--markdown`).
#[derive(Serialize, JsonSchema, Clone)]
pub struct CommitAuthorOutcome {
    pub name: String,
    pub email: String,
    /// Author timestamp in RFC 3339 (same shape as log/show).
    pub date: String,
}

fn author_outcome_json(
    author: &grit_lib::porcelain::commit::CommitAuthorInfo,
) -> CommitAuthorOutcome {
    let line = format!("{} <{}> {}", author.name, author.email, author.date);
    let person = Person::from_identity_line(&line);
    CommitAuthorOutcome {
        name: person.name,
        email: person.email,
        date: person.date,
    }
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
    /// Branch the commit advanced, or JSON `null` on a detached `HEAD`.
    pub branch: Option<String>,
    pub detached: bool,
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

impl CommitOutcome {
    /// The branch name, or `detached HEAD`, for human and Markdown output.
    pub fn branch_label(&self) -> &str {
        self.branch.as_deref().unwrap_or("detached HEAD")
    }
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
            self.branch_label(),
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

/// Exit code for `--hunks` failures (unknown, stale, or wrong-side ids).
pub fn hunks_exit_code(err: &anyhow::Error) -> i32 {
    if err.downcast_ref::<HunkSelectError>().is_some()
        || err
            .downcast_ref::<CommitPatchError>()
            .is_some_and(|e| matches!(e, CommitPatchError::Select(_)))
    {
        2
    } else {
        1
    }
}

fn map_patch_error(err: CommitPatchError) -> anyhow::Error {
    anyhow::Error::new(err)
}

fn map_hunk_parse(err: grit_lib::porcelain::hunk_select::HunkSelectError) -> anyhow::Error {
    anyhow::Error::new(err)
}

/// Run `grit commit --hunks`.
#[allow(clippy::too_many_arguments)]
pub fn run_hunks(
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
    dry_run: bool,
    amend: bool,
    allow_empty: bool,
    paths: &TrailingPathspecArgs,
    hunks: &[String],
    lines: &[String],
) -> Result<CommitOutcome> {
    let repo = context::discover()?;
    let resolved = paths.resolve(&repo)?;
    let (pathspecs, _) = resolved.into_pathspec_lists();
    let mut selection = parse_hunk_id_list(hunks).map_err(map_hunk_parse)?;
    selection.ranges = crate::commands::add::parse_line_range_list(lines)?;
    if selection.ids.is_empty() && selection.ranges.is_empty() {
        bail!("--hunks or --lines requires at least one hunk id or line range");
    }
    let message_for_outcome = message.clone();
    let req = build_commit_request(
        &repo,
        message,
        message_file,
        author_override,
        author_date,
        trailers,
        signoff,
        reset_author,
        reuse_message,
        gpg_sign,
        no_gpg_sign,
        no_verify,
        dry_run,
        amend,
        allow_empty,
    )?;
    let outcome = commit_with_patch(
        &repo,
        &req,
        CommitPatchInput::Hunks(selection),
        &pathspecs,
        &mut NullProgress,
    )
    .map_err(map_patch_error)?;
    finish_commit_outcome(
        &repo,
        req,
        outcome,
        CommitScopeJson::Patch,
        dry_run,
        message_for_outcome,
    )
}

/// Run `grit commit --patch-file`.
#[allow(clippy::too_many_arguments)]
pub fn run_patch_file(
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
    dry_run: bool,
    amend: bool,
    allow_empty: bool,
    patch_file: &str,
) -> Result<CommitOutcome> {
    let repo = context::discover()?;
    let patches = crate::patch_selection::parse_patch_file(patch_file)?;
    if patches.is_empty() {
        bail!("patch file contains no applicable hunks");
    }
    let message_for_outcome = message.clone();
    let req = build_commit_request(
        &repo,
        message,
        message_file,
        author_override,
        author_date,
        trailers,
        signoff,
        reset_author,
        reuse_message,
        gpg_sign,
        no_gpg_sign,
        no_verify,
        dry_run,
        amend,
        allow_empty,
    )?;
    let outcome = commit_with_patch(
        &repo,
        &req,
        CommitPatchInput::Patches(patches),
        &[],
        &mut NullProgress,
    )
    .map_err(map_patch_error)?;
    finish_commit_outcome(
        &repo,
        req,
        outcome,
        CommitScopeJson::Patch,
        dry_run,
        message_for_outcome,
    )
}

/// Run `grit commit` with the given message, flags, and optional path scope.
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
    dry_run: bool,
    amend: bool,
    allow_empty: bool,
    all: bool,
    staged: bool,
    paths: TrailingPathspecArgs,
) -> Result<CommitOutcome> {
    let repo = context::discover()?;

    match resolve_head(repo.git_dir())? {
        HeadState::Branch { .. } | HeadState::Detached { .. } => {}
        HeadState::Invalid => bail!("HEAD is in an unknown state"),
    }

    let scope = resolve_commit_scope(&repo, all, staged, &paths)?;
    let scope_json = scope_json(&scope);
    let message_for_outcome = message.clone();
    let req = build_commit_request(
        &repo,
        message,
        message_file,
        author_override,
        author_date,
        trailers,
        signoff,
        reset_author,
        reuse_message,
        gpg_sign,
        no_gpg_sign,
        no_verify,
        dry_run,
        amend,
        allow_empty,
    )?;
    let outcome =
        commit_with_scope(&repo, &req, scope, &mut NullProgress).map_err(map_commit_error)?;
    finish_commit_outcome(
        &repo,
        req,
        outcome,
        scope_json,
        dry_run,
        message_for_outcome,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_commit_request(
    repo: &grit_lib::repo::Repository,
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
    dry_run: bool,
    amend: bool,
    allow_empty: bool,
) -> Result<CommitRequest> {
    let reuse_message_from = if let Some(rev) = reuse_message.as_deref() {
        Some(
            resolve_revision_as_commit(repo, rev)
                .with_context(|| format!("could not resolve reuse-message commit '{rev}'"))?,
        )
    } else {
        None
    };

    let message = resolve_commit_message(
        repo,
        message,
        message_file,
        amend,
        reuse_message_from.is_some(),
    )?;
    if message.is_none() && !amend && reuse_message_from.is_none() {
        bail!("provide a commit message, e.g. grit commit \"what changed\"");
    }
    let message_text = message.as_deref().unwrap_or("");

    let env = repo.environment();
    let now = crate::context::wall_clock_now(env);
    let committer = Some(context::signature(repo, IdentRole::Committer, now)?);
    let author = if reset_author {
        None
    } else if let Some(arg) = author_override.as_deref() {
        let contact = parse_author_cli(arg)?;
        // `--date` reaches the library as `CommitRequest::author_date`, which replaces this time.
        let env_author_date = author_date
            .is_none()
            .then(|| env.var(IdentRole::Author.env_date_key()))
            .flatten();
        Some(
            contact
                .signature(env_author_date.as_deref(), now)
                .with_context(|| format!("invalid --author '{arg}'"))?,
        )
    } else if amend || reuse_message_from.is_some() {
        None
    } else {
        Some(context::signature(repo, IdentRole::Author, now)?)
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

    Ok(CommitRequest {
        message: message_text.to_owned(),
        author,
        committer,
        allow_empty,
        sign_override,
        amend,
        hooks,
        author_date: parsed_author_date,
        trailers: parsed_trailers,
        signoff,
        reset_author,
        reuse_message_from,
        dry_run,
        ..Default::default()
    })
}

fn finish_commit_outcome(
    repo: &grit_lib::repo::Repository,
    req: CommitRequest,
    outcome: grit_lib::porcelain::commit::CommitOutcome,
    scope_json: CommitScopeJson,
    dry_run: bool,
    message: Option<String>,
) -> Result<CommitOutcome> {
    let amend = req.amend;
    let message_text = req.message.clone();
    let subject = if dry_run && amend && message.as_ref().is_none_or(|m| m.trim().is_empty()) {
        let head = resolve_head(repo.git_dir()).context("resolve HEAD")?;
        if let HeadState::Branch { oid: Some(tip), .. } | HeadState::Detached { oid: tip } = head {
            let obj = repo.odb().read(&tip).context("read HEAD commit")?;
            subject_line(
                &parse_commit(&obj.data)
                    .context("parse HEAD commit")?
                    .message,
            )
        } else {
            subject_line(&format!("{}\n", message_text.trim()))
        }
    } else if dry_run {
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
        detached: outcome.is_detached(),
        branch: outcome.branch,
        subject,
        changes: outcome.changes,
        amended: amend,
        dry_run,
        scope: scope_json,
        files,
        change_id: outcome.change_id.as_ref().map(|id| id.as_str().to_owned()),
        parents: outcome.parents.iter().map(|p| p.to_hex()).collect(),
        author: author_outcome_json(&outcome.author),
        hooks_run: outcome
            .hooks_run
            .iter()
            .map(|h| h.as_str().to_owned())
            .collect(),
    })
}

/// Resolve the commit message from CLI input, pending merge/squash state, or amend reuse.
fn resolve_commit_message(
    repo: &grit_lib::repo::Repository,
    message: Option<String>,
    message_file: Option<String>,
    amend: bool,
    reuse_message: bool,
) -> Result<Option<String>> {
    if amend || reuse_message {
        return match (message, message_file) {
            (Some(m), None) => Ok(if m.trim().is_empty() { None } else { Some(m) }),
            (None, Some(path)) => {
                let contents = read_message_file(&path)?;
                Ok(if contents.trim().is_empty() {
                    None
                } else {
                    Some(contents)
                })
            }
            (None, None) => Ok(None),
            (Some(_), Some(_)) => {
                unreachable!("clap rejects message and message-file together")
            }
        };
    }

    match (message, message_file) {
        (Some(m), None) => {
            if m.trim().is_empty() {
                bail!("empty commit message");
            }
            Ok(Some(m))
        }
        (None, Some(path)) => {
            let contents = read_message_file(&path)?;
            if contents.trim().is_empty() {
                bail!("empty commit message");
            }
            Ok(Some(contents))
        }
        (None, None) => Ok(pending_commit_message(repo)
            .context("could not read pending commit message")?
            .filter(|m| !m.trim().is_empty())),
        (Some(_), Some(_)) => unreachable!("clap rejects message and message-file together"),
    }
}

fn parse_trailer_arg(raw: &str) -> Result<(String, String)> {
    parse_trailer_command_line_arg(raw).map_err(|e| anyhow::anyhow!("{e}"))
}

fn parse_trailer_date(raw: &str) -> Result<String> {
    normalize_commit_date_override(raw)
        .map_err(|e| anyhow::anyhow!("could not parse --date '{raw}': {e}"))
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
        CommitScope::Patch { .. } => CommitScopeJson::Patch,
    }
}

fn map_commit_error(err: Error) -> anyhow::Error {
    match err {
        Error::NothingToCommit => anyhow::anyhow!("nothing to commit — working tree clean"),
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
        hook @ Error::Hook(_) => anyhow::Error::new(hook),
        other => anyhow::anyhow!("{other}"),
    }
}
