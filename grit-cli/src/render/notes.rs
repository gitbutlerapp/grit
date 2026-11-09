//! Outcome types and renderers for `grit notes`.

use std::io::Write;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::{outln, HumanRender, MarkdownRender, Person, RenderContext};

use crate::markdown;
use grit_lib::objects::short_hex;

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
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        dry_run: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        notes_ref_old: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        notes_ref_new: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        note_blob: Option<String>,
    },
    /// Notes were removed from one or more objects.
    Remove {
        notes_ref: String,
        objects: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        dry_run: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        notes_ref_old: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        notes_ref_new: Option<String>,
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
                object,
                replaced,
                dry_run,
                ..
            } => {
                let short = short_hex(object);
                if dry_run == &Some(true) {
                    if *replaced {
                        outln!(w, "Would replace note for {short}");
                    } else {
                        outln!(w, "Would add note for {short}");
                    }
                } else if *replaced {
                    outln!(w, "Replaced note for {short}");
                } else {
                    outln!(w, "Added note for {short}");
                }
            }
            NotesOutcome::Remove {
                objects, dry_run, ..
            } => {
                if objects.is_empty() {
                    if dry_run == &Some(true) {
                        outln!(w, "Would remove no notes.");
                    } else {
                        outln!(w, "No notes removed.");
                    }
                } else {
                    let prefix = if dry_run == &Some(true) {
                        "Would remove note for "
                    } else {
                        "Removed note for "
                    };
                    for object in objects {
                        let short = short_hex(object);
                        outln!(w, "{prefix}{short}");
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
                let short = short_hex(object);
                outln!(w, "Note on `{short}` (`{notes_ref}`):");
                outln!(w);
                markdown::print_fenced_text(w, body)?;
            }
            NotesOutcome::Add {
                notes_ref,
                object,
                replaced,
                dry_run,
                ..
            } => {
                let short = short_hex(object);
                let verb = match (dry_run == &Some(true), *replaced) {
                    (true, true) => "Would replace",
                    (true, false) => "Would add",
                    (false, true) => "Replaced",
                    (false, false) => "Added",
                };
                outln!(w, "{verb} note for `{short}` on `{notes_ref}`.");
            }
            NotesOutcome::Remove {
                notes_ref,
                objects,
                dry_run,
                ..
            } => {
                let dry_run = dry_run == &Some(true);
                if objects.is_empty() {
                    if dry_run {
                        outln!(w, "Would remove no notes on `{notes_ref}`.");
                    } else {
                        outln!(w, "No notes removed on `{notes_ref}`.");
                    }
                } else {
                    let verb = if dry_run { "Would remove" } else { "Removed" };
                    outln!(w, "{verb} notes on `{notes_ref}`:");
                    for object in objects {
                        let short = short_hex(object);
                        outln!(w, "- `{short}`");
                    }
                }
            }
        }
        Ok(())
    }
}
