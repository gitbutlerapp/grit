//! `grit notes` — list, show, add, and remove Git notes.

use grit_lib::error::NotesError;

use anyhow::{bail, Context, Result};
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::error::Error;
use grit_lib::notes::{resolve_configured_notes_ref, NotesRef};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::repo_handles::{NoteAddOutcome, NoteAddPlan, NotesWriteOptions};
use grit_lib::revwalk::ResolveOptions;

use time::OffsetDateTime;

use crate::context;

pub use crate::render::notes::{NoteEntry, NotesOutcome};

/// Which `grit notes` subcommand to run (parsed in `main`).
pub enum NotesRunAction {
    /// List notes (default when no subcommand is given).
    List,
    /// Show one note body.
    Show {
        /// Revision to show the note for.
        rev: String,
    },
    /// Attach or replace a note.
    Add {
        /// Revision to annotate.
        rev: String,
        /// Note text.
        message: String,
        /// Replace an existing note.
        force: bool,
    },
    /// Remove notes from one or more revisions.
    Remove {
        /// Revisions whose notes should be removed.
        revs: Vec<String>,
    },
}

/// Run `grit notes` with the parsed CLI arguments.
pub fn run(
    notes_ref_flag: Option<String>,
    action: NotesRunAction,
    dry_run: bool,
    limit: Option<usize>,
    now: OffsetDateTime,
) -> Result<NotesOutcome> {
    let repo = context::discover()?;
    let notes_ref = resolve_notes_ref(&repo, notes_ref_flag.as_deref())?;

    match action {
        NotesRunAction::List => list_notes(&repo, &notes_ref, limit),
        NotesRunAction::Show { rev } => show_note(&repo, &notes_ref, &rev),
        NotesRunAction::Add {
            rev,
            message,
            force,
        } => add_note(&repo, &notes_ref, &rev, &message, force, dry_run, now),
        NotesRunAction::Remove { revs } => remove_notes(&repo, &notes_ref, &revs, dry_run, now),
    }
}

fn resolve_notes_ref(repo: &Repository, override_ref: Option<&str>) -> Result<NotesRef> {
    if let Some(name) = override_ref {
        return Ok(NotesRef::parse(name));
    }
    let config = repo.config().snapshot().context("could not load config")?;
    resolve_configured_notes_ref(repo.environment(), config.as_ref()).map_err(map_notes_error)
}

fn list_notes(
    repo: &Repository,
    notes_ref: &NotesRef,
    limit: Option<usize>,
) -> Result<NotesOutcome> {
    let listed = repo
        .notes()
        .list_with_bodies(notes_ref, limit)
        .map_err(map_notes_error)?;
    let notes = listed
        .items
        .into_iter()
        .map(|note| NoteEntry {
            object: note.object.to_hex(),
            blob: note.note_blob.to_hex(),
            body: String::from_utf8(note.body)
                .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned()),
        })
        .collect();
    Ok(NotesOutcome::List {
        notes_ref: notes_ref.to_string(),
        notes,
        truncated: listed.truncated,
    })
}

fn show_note(repo: &Repository, notes_ref: &NotesRef, rev: &str) -> Result<NotesOutcome> {
    let object = resolve_object(repo, rev)?;
    let body = repo
        .notes()
        .read(notes_ref, &object)
        .map_err(map_notes_error)?
        .ok_or_else(|| anyhow::anyhow!("no note found for {}", object.to_hex()))?;
    Ok(NotesOutcome::Show {
        notes_ref: notes_ref.to_string(),
        object: object.to_hex(),
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn add_note(
    repo: &Repository,
    notes_ref: &NotesRef,
    rev: &str,
    message: &str,
    force: bool,
    dry_run: bool,
    now: OffsetDateTime,
) -> Result<NotesOutcome> {
    if message.is_empty() {
        bail!("note message must not be empty (use -m)");
    }
    let object = resolve_object(repo, rev)?;
    let write_opts = notes_write_options(repo, "add", now)?;
    let mut content = message.as_bytes().to_vec();
    if !content.ends_with(b"\n") {
        content.push(b'\n');
    }
    if dry_run {
        let plan = repo
            .notes()
            .plan_add(notes_ref, &object, &content, &write_opts, force)
            .map_err(map_notes_error)?;
        return Ok(add_outcome_from_plan(notes_ref, plan, true));
    }
    let outcome = repo
        .notes()
        .add(notes_ref, &object, &content, &write_opts, force)
        .map_err(map_notes_error)?;
    Ok(NotesOutcome::Add {
        notes_ref: notes_ref.to_string(),
        object: object.to_hex(),
        replaced: outcome == NoteAddOutcome::Replaced,
        dry_run: None,
        notes_ref_old: None,
        notes_ref_new: None,
        note_blob: None,
    })
}

fn add_outcome_from_plan(notes_ref: &NotesRef, plan: NoteAddPlan, dry_run: bool) -> NotesOutcome {
    NotesOutcome::Add {
        notes_ref: notes_ref.to_string(),
        object: plan.object.to_hex(),
        replaced: plan.outcome == NoteAddOutcome::Replaced,
        dry_run: dry_run.then_some(true),
        notes_ref_old: plan.notes_ref_tip_before.map(|o| o.to_hex()),
        notes_ref_new: Some(plan.notes_ref_tip_after.to_hex()),
        note_blob: Some(plan.note_blob.to_hex()),
    }
}

fn remove_notes(
    repo: &Repository,
    notes_ref: &NotesRef,
    revs: &[String],
    dry_run: bool,
    now: OffsetDateTime,
) -> Result<NotesOutcome> {
    if revs.is_empty() {
        bail!("specify at least one revision to remove");
    }
    let mut objects = Vec::with_capacity(revs.len());
    for rev in revs {
        objects.push(resolve_object(repo, rev)?);
    }
    let write_opts = notes_write_options(repo, "remove", now)?;
    if dry_run {
        let plan = repo
            .notes()
            .plan_remove_many(notes_ref, &objects, &write_opts)
            .map_err(map_notes_error)?;
        let changes = !plan.removed.is_empty();
        return Ok(NotesOutcome::Remove {
            notes_ref: notes_ref.to_string(),
            objects: plan.removed.iter().map(ObjectId::to_hex).collect(),
            dry_run: Some(true),
            notes_ref_old: plan
                .notes_ref_tip_before
                .filter(|_| changes)
                .map(|o| o.to_hex()),
            notes_ref_new: plan.notes_ref_tip_after.map(|o| o.to_hex()),
        });
    }
    let removed = repo
        .notes()
        .remove_many(notes_ref, &objects, &write_opts)
        .map_err(map_notes_error)?;
    Ok(NotesOutcome::Remove {
        notes_ref: notes_ref.to_string(),
        objects: removed.into_iter().map(|o| o.to_hex()).collect(),
        dry_run: None,
        notes_ref_old: None,
        notes_ref_new: None,
    })
}

fn resolve_object(repo: &Repository, rev: &str) -> Result<ObjectId> {
    repo.revwalk()
        .resolve(rev, &ResolveOptions::default())
        .map_err(map_notes_error)
}

fn notes_write_options(
    repo: &Repository,
    verb: &str,
    now: OffsetDateTime,
) -> Result<NotesWriteOptions> {
    Ok(NotesWriteOptions {
        message: format!("Notes {verb} by 'grit notes {verb}'"),
        committer: context::signature(repo, IdentRole::Committer, now)?,
        author: Some(context::signature(repo, IdentRole::Author, now)?),
    })
}

fn map_notes_error(err: Error) -> anyhow::Error {
    match err {
        Error::Notes(NotesError::AlreadyExists { object }) => {
            anyhow::anyhow!(
                "note for object {} already exists (use --force)",
                object.to_hex()
            )
        }
        other => anyhow::Error::new(other),
    }
}

/// Dispatch parsed CLI args for `Notes`.
pub fn dispatch(
    args: crate::args::NotesArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;

    use crate::dispatch::{emit_success, CommandEnv};

    use crate::message_input;

    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let NotesArgs {
        notes_ref,
        dry_run,
        limit,
        action,
    } = args;
    {
        use commands::notes::NotesRunAction::*;
        let (run_action, list_limit) = match action {
            None => (List, limit.get()),
            Some(NotesAction::List { limit: sub_limit }) => (List, sub_limit.get().or(limit.get())),
            Some(NotesAction::Show { rev }) => (Show { rev }, None),
            Some(NotesAction::Add {
                rev,
                message,
                force,
            }) => {
                let message = message_input::read_required_normalized_message(&message)?;
                (
                    Add {
                        rev,
                        message,
                        force,
                    },
                    None,
                )
            }
            Some(NotesAction::Remove { revs }) => (Remove { revs }, None),
        };
        emit_success(
            commands::notes::run(notes_ref, run_action, dry_run, list_limit, ctx.now)?,
            opts,
            ctx,
        )
    }
}
