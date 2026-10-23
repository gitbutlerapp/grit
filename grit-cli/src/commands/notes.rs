//! `grit notes` — list, show, add, and remove Git notes.

use std::io::Write;

use anyhow::{bail, Context, Result};
use grit_lib::config::ident_resolve::IdentRole;
use grit_lib::error::Error;
use grit_lib::notes::{resolve_configured_notes_ref, NotesRef};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::repo_handles::{NoteAddOutcome, NotesWriteOptions};
use grit_lib::revwalk::rev_parse::resolve_revision;
use schemars::JsonSchema;
use serde::Serialize;

use crate::context;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use time::OffsetDateTime;

fn is_false(value: &bool) -> bool {
    !*value
}

/// Result of `grit notes`, tagged by `action`.
#[derive(Serialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum NotesOutcome {
    /// All notes under the selected ref.
    List {
        notes_ref: String,
        notes: Vec<NoteEntry>,
        /// True when more notes existed than returned (`-n` / `--limit`).
        #[serde(default, skip_serializing_if = "is_false")]
        #[schemars(default)]
        truncated: bool,
    },
    /// Note bytes for one object.
    Show {
        notes_ref: String,
        object: String,
        body: String,
    },
    /// A note was written.
    Add {
        notes_ref: String,
        object: String,
        replaced: bool,
    },
    /// Notes were removed from one or more objects.
    Remove {
        notes_ref: String,
        objects: Vec<String>,
    },
}

/// One note in a `list` outcome.
#[derive(Serialize, JsonSchema)]
pub struct NoteEntry {
    /// Annotated object id (full hex).
    pub object: String,
    /// Note blob object id (full hex).
    pub blob: String,
    /// Note text (UTF-8 lossy when not valid UTF-8).
    pub body: String,
}

impl HumanRender for NotesOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            NotesOutcome::List { notes, .. } => {
                if notes.is_empty() {
                    outln!(w, "No notes yet.");
                    return Ok(());
                }
                for entry in notes {
                    outln!(w, "{}  {}", entry.object, entry.body.trim_end());
                }
            }
            NotesOutcome::Show { body, .. } => {
                w.write_all(body.as_bytes())?;
                if !body.ends_with('\n') {
                    w.write_all(b"\n")?;
                }
            }
            NotesOutcome::Add {
                object, replaced, ..
            } => {
                let short = object.get(..7).unwrap_or(object.as_str());
                if *replaced {
                    outln!(w, "Replaced note for {short}");
                } else {
                    outln!(w, "Added note for {short}");
                }
            }
            NotesOutcome::Remove { objects, .. } => {
                if objects.is_empty() {
                    outln!(w, "No notes removed.");
                } else {
                    for object in objects {
                        let short = object.get(..7).unwrap_or(object.as_str());
                        outln!(w, "Removed note for {short}");
                    }
                }
            }
        }
        Ok(())
    }
}

impl MarkdownRender for NotesOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            NotesOutcome::List {
                notes_ref, notes, ..
            } => {
                markdown::print_notes_list(w, notes_ref, notes)?;
            }
            NotesOutcome::Show {
                notes_ref,
                object,
                body,
            } => {
                let short = object.get(..7).unwrap_or(object.as_str());
                outln!(w, "Note on `{short}` (`{notes_ref}`):");
                outln!(w);
                outln!(w, "```text");
                w.write_all(body.as_bytes())?;
                if !body.ends_with('\n') {
                    outln!(w);
                }
                outln!(w, "```");
            }
            NotesOutcome::Add {
                notes_ref,
                object,
                replaced,
            } => {
                let short = object.get(..7).unwrap_or(object.as_str());
                let verb = if *replaced { "Replaced" } else { "Added" };
                outln!(w, "{verb} note for `{short}` on `{notes_ref}`.");
            }
            NotesOutcome::Remove { notes_ref, objects } => {
                if objects.is_empty() {
                    outln!(w, "No notes removed on `{notes_ref}`.");
                } else {
                    outln!(w, "Removed notes on `{notes_ref}`:");
                    for object in objects {
                        let short = object.get(..7).unwrap_or(object.as_str());
                        outln!(w, "- `{short}`");
                    }
                }
            }
        }
        Ok(())
    }
}

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
        } => add_note(&repo, &notes_ref, &rev, &message, force, now),
        NotesRunAction::Remove { revs } => remove_notes(&repo, &notes_ref, &revs, now),
    }
}

fn resolve_notes_ref(repo: &Repository, override_ref: Option<&str>) -> Result<NotesRef> {
    if let Some(name) = override_ref {
        return Ok(NotesRef::parse(name));
    }
    let config = repo.config().context("could not load config")?;
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
    let outcome = repo
        .notes()
        .add(notes_ref, &object, &content, &write_opts, force)
        .map_err(map_notes_error)?;
    Ok(NotesOutcome::Add {
        notes_ref: notes_ref.to_string(),
        object: object.to_hex(),
        replaced: outcome == NoteAddOutcome::Replaced,
    })
}

fn remove_notes(
    repo: &Repository,
    notes_ref: &NotesRef,
    revs: &[String],
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
    let removed = repo
        .notes()
        .remove_many(notes_ref, &objects, &write_opts)
        .map_err(map_notes_error)?;
    Ok(NotesOutcome::Remove {
        notes_ref: notes_ref.to_string(),
        objects: removed.into_iter().map(|o| o.to_hex()).collect(),
    })
}

fn resolve_object(repo: &Repository, rev: &str) -> Result<ObjectId> {
    resolve_revision(repo, rev).map_err(map_notes_error)
}

fn notes_write_options(
    repo: &Repository,
    verb: &str,
    now: OffsetDateTime,
) -> Result<NotesWriteOptions> {
    let env = repo.environment();
    let config = repo.config().context("could not load config")?;
    let committer = context::identity(env, config.as_ref(), IdentRole::Committer, now)?;
    let author = context::identity(env, config.as_ref(), IdentRole::Author, now)?;
    let message = format!("Notes {verb} by 'grit notes {verb}'");
    Ok(NotesWriteOptions {
        message,
        committer,
        author: Some(author),
    })
}

fn map_notes_error(err: Error) -> anyhow::Error {
    match err {
        Error::NoteAlreadyExists { object } => {
            anyhow::anyhow!(
                "note for object {} already exists (use --force)",
                object.to_hex()
            )
        }
        other => anyhow::Error::new(other),
    }
}
