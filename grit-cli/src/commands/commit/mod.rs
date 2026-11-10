//! `grit commit` — record a new commit with configurable staging scope.

use anyhow::{bail, Context, Result};
use grit_lib::commit::{normalize_commit_date_override, parse_trailer_command_line_arg};
use grit_lib::index::IndexError;
use grit_lib::revwalk::{Peel, ResolveOptions};
use grit_lib::worktree::WorktreeError;

use crate::message_input::parse_author_cli;
use grit_lib::commits::{CommitOptions, CommitPatchInput, CommitRequest, CommitScope, HookPolicy};
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::error::Error;
use grit_lib::porcelain::hunk_select::{parse_hunk_id_list, HunkSelectError};
use grit_lib::porcelain::CommitError;
use grit_lib::progress::{NullProgress, ProgressSink};
use grit_lib::state::HeadState;
use std::io::{self, Read};

use crate::context::{self, subject_line};
use crate::error_kind::ErrorKind;
use crate::json_error::{StructuredCliError, StructuredJsonError};
use crate::pathspec_args::TrailingPathspecArgs;

pub use crate::render::commit::*;

fn author_outcome_json(
    author: &grit_lib::porcelain::commit::CommitAuthorInfo,
) -> CommitAuthorOutcome {
    let sig = author.signature();
    let date = sig
        .format_rfc3339()
        .unwrap_or_else(|| grit_lib::commit::format_git_timestamp(sig.time));
    CommitAuthorOutcome {
        name: sig.name,
        email: sig.email,
        date,
    }
}

/// Exit code for `--hunks` failures (unknown, stale, or wrong-side ids).
pub fn hunks_exit_code(err: &anyhow::Error) -> i32 {
    if err.downcast_ref::<HunkSelectError>().is_some()
        || matches!(
            err.downcast_ref::<Error>(),
            Some(
                Error::Commit(grit_lib::porcelain::CommitError::HunkSelect(_))
                    | Error::Diff(grit_lib::error::DiffError::HunkSelection(_))
            )
        )
    {
        2
    } else {
        1
    }
}

fn map_patch_error(err: Error) -> anyhow::Error {
    if matches!(err, Error::LockHeld(..)) {
        return crate::json_error::map_lib_error(err);
    }
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
    refuse_gitbutler_workspace(&repo)?;
    let resolved = paths.resolve(&repo)?;
    let pathspecs = resolved.specs().to_vec();
    let mut selection = parse_hunk_id_list(hunks).map_err(crate::commands::add::map_hunk_error)?;
    selection.ranges = crate::commands::add::parse_line_range_list(lines)?;
    if selection.ids.is_empty() && selection.ranges.is_empty() {
        bail!("--hunks or --lines requires at least one hunk id or line range");
    }
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
    let _interrupt = crate::interrupt::InterruptCancel::install();
    let scope = CommitScope::Patch {
        input: CommitPatchInput::Hunks(selection),
        pathspecs,
    };
    let outcome = repo
        .commits()
        .create(
            &CommitOptions::new(req.clone(), scope),
            &mut NullProgress,
            &crate::interrupt::process_cancel_token(),
        )
        .map_err(map_patch_error)?;
    finish_commit_outcome(req, outcome, CommitScopeJson::Patch, dry_run)
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
    refuse_gitbutler_workspace(&repo)?;
    let patches = crate::patch_selection::parse_patch_file(patch_file)?;
    if patches.is_empty() {
        bail!("patch file contains no applicable hunks");
    }
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
    let _interrupt = crate::interrupt::InterruptCancel::install();
    let scope = CommitScope::Patch {
        input: CommitPatchInput::Patches(patches),
        pathspecs: Vec::new(),
    };
    let outcome = repo
        .commits()
        .create(
            &CommitOptions::new(req.clone(), scope),
            &mut NullProgress,
            &crate::interrupt::process_cancel_token(),
        )
        .map_err(map_patch_error)?;
    finish_commit_outcome(req, outcome, CommitScopeJson::Patch, dry_run)
}

/// Fail when `HEAD` is on a GitButler workspace: GitButler rewrites that branch, so a commit
/// made on it would be lost.
fn refuse_gitbutler_workspace(repo: &grit_lib::repo::Repository) -> Result<()> {
    match grit_lib::gitbutler::GitButlerWorkspace::detect(repo).context("could not read HEAD")? {
        Some(_) => Err(crate::json_error::gitbutler_workspace_commit()),
        None => Ok(()),
    }
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
    progress: &mut dyn ProgressSink,
) -> Result<CommitOutcome> {
    let repo = context::discover()?;
    refuse_gitbutler_workspace(&repo)?;

    match repo.references().head()? {
        HeadState::Branch { .. } | HeadState::Detached { .. } => {}
        HeadState::Invalid => bail!("HEAD is in an unknown state"),
    }

    let scope = resolve_commit_scope(&repo, all, staged, &paths)?;
    let scope_json = scope_json(&scope);
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
    let _interrupt = crate::interrupt::InterruptCancel::install();
    let outcome = repo
        .commits()
        .create(
            &CommitOptions::new(req.clone(), scope.clone()),
            progress,
            &crate::interrupt::process_cancel_token(),
        )
        .map_err(map_commit_error)?;
    finish_commit_outcome(req, outcome, scope_json, dry_run)
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
            repo.revwalk()
                .resolve(rev, &ResolveOptions::default().with_peel(Peel::Commit))
                .with_context(|| format!("could not resolve reuse-message commit '{rev}'"))?,
        )
    } else {
        None
    };

    let message =
        resolve_commit_message(message, message_file, amend, reuse_message_from.is_some())?;
    let has_pending_message = || -> Result<bool> {
        Ok(repo
            .commits()
            .pending_message()
            .context("could not read pending commit message")?
            .is_some_and(|m| !m.trim().is_empty()))
    };
    if message.is_none() && !amend && reuse_message_from.is_none() && !has_pending_message()? {
        return Err(commit_message_required_error());
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
        // Leave unset so grit-lib picks author from CHERRY_PICK_HEAD, config, or identity.
        None
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

    let mut req = CommitRequest::new(message_text);
    req.author = author;
    req.committer = committer;
    req.allow_empty = allow_empty;
    req.sign_override = sign_override;
    req.amend = amend;
    req.hooks = hooks;
    req.author_date = parsed_author_date;
    req.trailers = parsed_trailers;
    req.signoff = signoff;
    req.reset_author = reset_author;
    req.reuse_message_from = reuse_message_from;
    req.dry_run = dry_run;
    Ok(req)
}

fn finish_commit_outcome(
    req: CommitRequest,
    outcome: grit_lib::porcelain::commit::CommitOutcome,
    scope_json: CommitScopeJson,
    dry_run: bool,
) -> Result<CommitOutcome> {
    let amend = req.amend;
    let subject = subject_line(&outcome.message);

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
        embedded_gitlinks: outcome.embedded_gitlinks,
    })
}

/// Resolve the commit message from CLI input. `None` leaves the message to the library: the
/// pending merge or squash message, or the amended or reused commit's.
fn resolve_commit_message(
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
                return Err(empty_commit_message_error());
            }
            Ok(Some(m))
        }
        (None, Some(path)) => {
            let contents = read_message_file(&path)?;
            if contents.trim().is_empty() {
                return Err(empty_commit_message_error());
            }
            Ok(Some(contents))
        }
        (None, None) => Ok(None),
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
    repo.commits()
        .default_scope()
        .map_err(|e| anyhow::anyhow!("{e}"))
}

fn scope_json(scope: &CommitScope) -> CommitScopeJson {
    match scope {
        CommitScope::AutoStage => CommitScopeJson::AutoStage,
        CommitScope::Staged => CommitScopeJson::Staged,
        CommitScope::Paths(_) => CommitScopeJson::Paths,
        CommitScope::Patch { .. } => CommitScopeJson::Patch,
        // The CLI only builds the scopes above.
        _ => CommitScopeJson::Staged,
    }
}

fn commit_message_required_error() -> anyhow::Error {
    let human = "provide a commit message, e.g. grit commit \"what changed\"".to_owned();
    anyhow::Error::new(StructuredCliError {
        payload: StructuredJsonError::new(human.clone(), ErrorKind::CommitMessageRequired),
        human,
    })
}

fn empty_commit_message_error() -> anyhow::Error {
    let human = "empty commit message".to_owned();
    anyhow::Error::new(StructuredCliError {
        payload: StructuredJsonError::new(human.clone(), ErrorKind::EmptyCommitMessage),
        human,
    })
}

fn worktree_case_collision_commit_error(paths: &[String]) -> anyhow::Error {
    let summary =
        "tracked paths collide on this filesystem and cannot be staged or committed".to_owned();
    let hint = "use a case-sensitive volume, or run `grit reset --hard` to match the index to HEAD";
    anyhow::Error::new(StructuredCliError {
        payload: StructuredJsonError {
            paths: Some(paths.to_vec()),
            hint: Some(hint.to_owned()),
            ..StructuredJsonError::new(summary.clone(), ErrorKind::WorktreeCaseCollision)
        },
        human: summary,
    })
}

fn nothing_staged_commit_error(unstaged: usize, untracked: usize) -> anyhow::Error {
    let total = unstaged + untracked;
    let files = if total == 1 { "file" } else { "files" };
    let verb = if total == 1 { "has" } else { "have" };
    let summary =
        format!("nothing staged to commit ({total} {files} {verb} unstaged or untracked changes)");
    let hint = "run `grit add <file>` first, or `grit commit -- <path>`";
    anyhow::Error::new(StructuredCliError {
        payload: StructuredJsonError {
            hint: Some(hint.to_owned()),
            ..StructuredJsonError::new(summary.clone(), ErrorKind::NothingStaged)
        },
        human: summary,
    })
}

fn map_commit_error(err: Error) -> anyhow::Error {
    match err {
        Error::Commit(CommitError::WorktreeCaseCollision { paths }) => {
            worktree_case_collision_commit_error(&paths)
        }
        Error::Commit(CommitError::NothingStagedToCommit {
            unstaged,
            untracked,
        }) => nothing_staged_commit_error(unstaged, untracked),
        Error::Commit(CommitError::NothingToCommit) => anyhow::Error::new(StructuredCliError {
            payload: StructuredJsonError::new(
                "nothing to commit — working tree clean",
                ErrorKind::NothingToCommit,
            ),
            human: "nothing to commit — working tree clean".to_owned(),
        }),
        Error::Index(IndexError::Unmerged { .. }) => {
            anyhow::Error::new(Error::Index(IndexError::unmerged()))
        }
        Error::Commit(CommitError::AmendUnborn) => {
            anyhow::anyhow!("cannot amend: no commits on this branch yet")
        }
        Error::Commit(CommitError::PartialCommitDuringMerge) => {
            anyhow::anyhow!("cannot do a partial commit during a merge")
        }
        Error::Commit(CommitError::PartialCommitDuringReplay) => {
            anyhow::anyhow!("cannot do a partial commit during a cherry-pick or revert")
        }
        pathspec @ Error::Worktree(WorktreeError::PathspecNoMatch { .. }) => {
            anyhow::Error::new(pathspec)
        }
        Error::Worktree(WorktreeError::EmbeddedRepoWithoutCommit { path }) => {
            let human = format!(
                "cannot commit: '{path}' is an embedded git repository with no commit checked out"
            );
            anyhow::Error::new(crate::json_error::StructuredCliError {
                payload: crate::json_error::StructuredJsonError {
                    paths: Some(vec![path]),
                    ..crate::json_error::StructuredJsonError::new(
                        human.clone(),
                        crate::error_kind::ErrorKind::EmbeddedRepoWithoutCommit,
                    )
                },
                human,
            })
        }
        hook @ Error::Hook(_) => anyhow::Error::new(hook),
        locked @ Error::LockHeld(..) => crate::json_error::map_lib_error(locked),
        other => crate::json_error::map_lib_error(other),
    }
}

mod dispatch;
pub use dispatch::dispatch;
