//! `grit notes` — add, show, list, remove, append, and merge object notes.
//!
//! Notes are stored as blobs in a tree referenced by `refs/notes/commits`
//! (or a custom namespace via `--ref`).  Each entry in the notes tree is
//! named by the full hex SHA of the annotated object.

#![allow(clippy::if_same_then_else, clippy::redundant_pattern_matching)]
use anyhow::{bail, Context, Result};
use clap::{Args as ClapArgs, Subcommand};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use grit_lib::config::ConfigSet;
use grit_lib::notes::{
    combine_notes_cat_sort_uniq, combine_notes_concatenate, display_note_path, expand_notes_ref,
    note_object_name, notes_merge_git_dir, notes_merge_inner, notes_merge_worktree_path,
    parse_notes_merge_strategy_value, read_notes_tree, upsert_note_entry, write_notes_commit,
    write_notes_commit_with_parents, NotesMergeStrategy, NotesTreeEntry,
};
use grit_lib::objects::{parse_commit, ObjectId, ObjectKind};
use grit_lib::refs::{
    common_dir, delete_ref, read_symbolic_ref, resolve_ref, write_ref, write_symbolic_ref,
};

use crate::commands::worktree_refs;
use grit_lib::repo::Repository;
use grit_lib::rev_parse::resolve_revision;
use grit_lib::state::resolve_head;
use grit_lib::stripspace::{process as stripspace_process, Mode as StripspaceMode};

use std::io::{self, Read, Write};

/// Arguments for `grit notes`.
#[derive(Debug, ClapArgs)]
#[command(about = "Add or inspect object notes")]
pub struct Args {
    /// Use notes ref <ref> instead of refs/notes/commits.
    #[arg(long = "ref", global = true)]
    pub notes_ref: Option<String>,

    #[command(subcommand)]
    pub command: Option<NotesSubcommand>,
}

#[derive(Debug, Subcommand)]
pub enum NotesSubcommand {
    /// List notes.
    List {
        /// Object to list notes for (if omitted, list all notes).
        #[arg()]
        object: Option<String>,
    },
    /// Add a note to an object.
    Add {
        /// Note message.
        #[arg(short = 'm', long = "message", action = clap::ArgAction::Append)]
        message: Vec<String>,

        /// Read note message from file ('-' for stdin).
        #[arg(short = 'F', long = "file", value_name = "FILE", action = clap::ArgAction::Append)]
        file: Vec<std::path::PathBuf>,

        /// Reuse an existing blob object as the note (verbatim blob).
        #[arg(short = 'C', long = "reuse-message", value_name = "OBJECT")]
        reuse_message: Option<String>,

        /// Reuse and edit note from object.
        #[arg(short = 'c', long = "reedit-message", value_name = "OBJECT")]
        reedit_message: Option<String>,

        /// Edit message in editor after composing from -m/-F/-c/-C.
        #[arg(short = 'e', long = "edit")]
        use_editor: bool,

        /// Overwrite an existing note.
        #[arg(short = 'f', long = "force")]
        force: bool,

        /// Allow empty note.
        #[arg(long = "allow-empty")]
        allow_empty: bool,

        /// Paragraph separator between multiple -m/-F parts (Git default: one newline).
        #[arg(long = "separator", num_args = 0..=1, default_missing_value = "\n")]
        separator: Option<String>,

        /// Concatenate -m/-F parts with no separator between paragraphs.
        #[arg(long = "no-separator", conflicts_with = "separator")]
        no_separator: bool,

        #[arg(long = "stripspace", conflicts_with = "no_stripspace")]
        stripspace: bool,

        #[arg(long = "no-stripspace")]
        no_stripspace: bool,

        /// Object to annotate (defaults to HEAD).
        #[arg()]
        object: Option<String>,
    },
    /// Show the note for an object.
    Show {
        /// Object whose note to show (defaults to HEAD).
        #[arg()]
        object: Option<String>,
    },
    /// Remove the note for an object.
    Remove {
        #[arg(long = "ignore-missing")]
        ignore_missing: bool,

        #[arg(long = "stdin")]
        from_stdin: bool,

        #[arg()]
        objects: Vec<String>,
    },
    /// Append to the note for an object.
    Append {
        #[arg(short = 'm', long = "message", action = clap::ArgAction::Append)]
        message: Vec<String>,

        #[arg(short = 'F', long = "file", value_name = "FILE", action = clap::ArgAction::Append)]
        file: Vec<std::path::PathBuf>,

        #[arg(short = 'C', long = "reuse-message", value_name = "OBJECT")]
        reuse_message: Option<String>,

        #[arg(short = 'c', long = "reedit-message", value_name = "OBJECT")]
        reedit_message: Option<String>,

        #[arg(short = 'e', long = "edit")]
        use_editor: bool,

        #[arg(long = "allow-empty")]
        allow_empty: bool,

        #[arg(long = "separator", num_args = 0..=1, default_missing_value = "\n")]
        separator: Option<String>,

        #[arg(long = "no-separator", conflicts_with = "separator")]
        no_separator: bool,

        #[arg(long = "stripspace", conflicts_with = "no_stripspace")]
        stripspace: bool,

        #[arg(long = "no-stripspace")]
        no_stripspace: bool,

        #[arg()]
        object: Option<String>,
    },
    /// Copy the note from one object to another.
    Copy {
        #[arg(short = 'f', long = "force")]
        force: bool,

        #[arg(long = "stdin")]
        from_stdin: bool,

        #[arg(long = "for-rewrite", value_name = "CMD")]
        for_rewrite: Option<String>,

        #[arg(value_name = "OBJECT")]
        objects: Vec<String>,
    },
    /// Edit an existing note (launches editor).
    Edit {
        #[arg(short = 'm', long = "message", action = clap::ArgAction::Append)]
        message: Vec<String>,

        #[arg(short = 'F', long = "file", value_name = "FILE", action = clap::ArgAction::Append)]
        file: Vec<std::path::PathBuf>,

        #[arg(short = 'C', long = "reuse-message", value_name = "OBJECT")]
        reuse_message: Option<String>,

        #[arg(short = 'c', long = "reedit-message", value_name = "OBJECT")]
        reedit_message: Option<String>,

        #[arg(short = 'e', long = "edit")]
        use_editor: bool,

        #[arg(long = "allow-empty")]
        allow_empty: bool,

        #[arg(long = "separator", num_args = 0..=1, default_missing_value = "\n")]
        separator: Option<String>,

        #[arg(long = "no-separator", conflicts_with = "separator")]
        no_separator: bool,

        #[arg(long = "stripspace", conflicts_with = "no_stripspace")]
        stripspace: bool,

        #[arg(long = "no-stripspace")]
        no_stripspace: bool,

        #[arg()]
        object: Option<String>,
    },
    /// Merge notes refs.
    Merge {
        /// Finalize a notes merge after resolving conflicts.
        #[arg(long)]
        commit: bool,

        /// Abort an in-progress notes merge.
        #[arg(long)]
        abort: bool,

        /// More verbose output (repeat for more detail; matches Git).
        #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
        verbose: u8,

        /// Quieter output (repeat for less; matches Git).
        #[arg(short = 'q', long = "quiet", action = clap::ArgAction::Count)]
        quiet: u8,

        /// Merge strategy (manual, ours, theirs, union, cat_sort_uniq).
        #[arg(short = 's', long = "strategy")]
        strategy: Option<String>,

        /// Notes ref to merge from (with `git notes merge <ref>`).
        #[arg()]
        source_ref: Option<String>,
    },
    /// Remove notes for non-existent objects.
    Prune {
        /// Only report what would be done.
        #[arg(short = 'n', long)]
        dry_run: bool,

        /// Report pruned entries.
        #[arg(short, long)]
        verbose: bool,
    },
    /// Print the current notes ref.
    #[command(name = "get-ref")]
    GetRef {
        #[arg(trailing_var_arg = true, hide = true)]
        extra: Vec<String>,
    },
}

/// Active notes ref after `--ref` / `GIT_NOTES_REF` / config (matches C Git: only `--ref` uses `expand_notes_ref`).
fn active_notes_ref(repo: &Repository, cli_override: Option<&str>) -> Result<String> {
    if let Some(r) = cli_override {
        return Ok(expand_notes_ref(r));
    }
    if let Ok(v) = std::env::var("GIT_NOTES_REF") {
        if !v.is_empty() {
            return Ok(v);
        }
    }
    let cfg = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    if let Some(r) = cfg.get("core.notesRef").filter(|s| !s.is_empty()) {
        return Ok(r);
    }
    Ok("refs/notes/commits".to_owned())
}

fn ensure_notes_ref_namespace(notes_ref: &str) -> Result<()> {
    if notes_ref == "/" {
        bail!("refusing to use notes ref '/'");
    }
    if !notes_ref.starts_with("refs/notes/") {
        bail!("refusing to use notes in {notes_ref} (outside of refs/notes/)");
    }
    Ok(())
}

/// Git refuses to add/edit/append/remove/copy when `--ref` / `GIT_NOTES_REF` uses revision syntax
/// (`^{tree}`, `@{1}`, …) rather than a plain ref name under `refs/notes/`.
fn ensure_notes_ref_is_plain_refname(notes_ref: &str) -> Result<()> {
    if notes_ref.contains('^') || notes_ref.contains("@{") || notes_ref.contains(':') {
        bail!("refusing to use notes ref {notes_ref}");
    }
    Ok(())
}

/// Parse argv like the harness (`git notes ...`). Bare `git notes` lists all; `git notes <x>` without
/// a subcommand is an error (matches C Git).
pub fn run_from_argv(rest: &[String]) -> Result<()> {
    let args = crate::parse_cmd_args::<Args>("notes", rest);
    run(args)
}

/// Run the `notes` command.
pub fn run(args: Args) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let notes_ref = active_notes_ref(&repo, args.notes_ref.as_deref())?;
    if notes_ref == "/" {
        bail!("refusing to use notes ref '/'");
    }
    if !matches!(args.command, Some(NotesSubcommand::GetRef { .. })) {
        ensure_notes_ref_namespace(&notes_ref)?;
    }
    let needs_plain_ref = matches!(
        args.command,
        Some(NotesSubcommand::Add { .. })
            | Some(NotesSubcommand::Edit { .. })
            | Some(NotesSubcommand::Append { .. })
            | Some(NotesSubcommand::Remove { .. })
            | Some(NotesSubcommand::Copy { .. })
            | Some(NotesSubcommand::Merge { .. })
            | Some(NotesSubcommand::Prune { .. })
    );
    if needs_plain_ref {
        ensure_notes_ref_is_plain_refname(&notes_ref)?;
    }

    match args.command {
        None => list_all_notes(&repo, &notes_ref),
        Some(NotesSubcommand::List { object: None }) => list_all_notes(&repo, &notes_ref),
        Some(NotesSubcommand::List {
            object: Some(object),
        }) => list_note_for_object(&repo, &notes_ref, &object),
        Some(NotesSubcommand::Add {
            message,
            file,
            reuse_message,
            reedit_message,
            use_editor,
            force,
            allow_empty,
            separator,
            no_separator,
            stripspace,
            no_stripspace,
            object,
        }) => add_note(
            &repo,
            &notes_ref,
            object.as_deref(),
            &message,
            &file,
            reuse_message.as_deref(),
            reedit_message.as_deref(),
            use_editor,
            force,
            allow_empty,
            stripspace,
            no_stripspace,
            if no_separator {
                Some("")
            } else if no_stripspace && separator.is_none() {
                None
            } else if separator.as_deref() == Some("") {
                Some("\n")
            } else {
                Some(separator.as_deref().unwrap_or("\n\n"))
            },
        ),
        Some(NotesSubcommand::Show { object }) => show_note(&repo, &notes_ref, object.as_deref()),
        Some(NotesSubcommand::Remove {
            ignore_missing,
            from_stdin,
            objects,
        }) => remove_notes(&repo, &notes_ref, ignore_missing, from_stdin, &objects),
        Some(NotesSubcommand::Append {
            message,
            file,
            reuse_message,
            reedit_message,
            use_editor,
            allow_empty,
            separator,
            no_separator,
            stripspace,
            no_stripspace,
            object,
        }) => append_or_edit_note(
            &repo,
            &notes_ref,
            object.as_deref(),
            false,
            &message,
            &file,
            reuse_message.as_deref(),
            reedit_message.as_deref(),
            use_editor,
            allow_empty,
            stripspace,
            no_stripspace,
            if no_separator {
                Some("")
            } else if no_stripspace && separator.is_none() {
                None
            } else if separator.as_deref() == Some("") {
                Some("\n")
            } else {
                Some(separator.as_deref().unwrap_or("\n\n"))
            },
        ),
        Some(NotesSubcommand::Edit {
            message,
            file,
            reuse_message,
            reedit_message,
            use_editor,
            allow_empty,
            separator,
            no_separator,
            stripspace,
            no_stripspace,
            object,
        }) => append_or_edit_note(
            &repo,
            &notes_ref,
            object.as_deref(),
            true,
            &message,
            &file,
            reuse_message.as_deref(),
            reedit_message.as_deref(),
            use_editor,
            allow_empty,
            stripspace,
            no_stripspace,
            if no_separator {
                Some("")
            } else if no_stripspace && separator.is_none() {
                None
            } else if separator.as_deref() == Some("") {
                Some("\n")
            } else {
                Some(separator.as_deref().unwrap_or("\n\n"))
            },
        ),
        Some(NotesSubcommand::Copy {
            force,
            from_stdin,
            for_rewrite,
            objects,
        }) => copy_notes(
            &repo,
            &notes_ref,
            force,
            from_stdin,
            for_rewrite.as_deref(),
            &objects,
        ),
        Some(NotesSubcommand::Merge {
            commit,
            abort,
            verbose,
            quiet,
            strategy,
            source_ref,
        }) => merge_notes_dispatch(
            &repo,
            &notes_ref,
            commit,
            abort,
            verbose,
            quiet,
            strategy.as_deref(),
            source_ref.as_deref(),
        ),
        Some(NotesSubcommand::Prune { dry_run, verbose }) => {
            prune_notes(&repo, &notes_ref, dry_run, verbose)
        }
        Some(NotesSubcommand::GetRef { extra }) => {
            if !extra.is_empty() {
                eprintln!("error: too many arguments");
                std::process::exit(129);
            }
            println!("{notes_ref}");
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve an object spec to an ObjectId, defaulting to HEAD.
fn resolve_object(repo: &Repository, spec: Option<&str>) -> Result<ObjectId> {
    match spec {
        Some(s) => resolve_revision(repo, s).with_context(|| format!("cannot resolve '{s}'")),
        None => {
            let head = resolve_head(&repo.git_dir)?;
            match head {
                grit_lib::state::HeadState::Branch { oid: Some(oid), .. } => Ok(oid),
                grit_lib::state::HeadState::Detached { oid } => Ok(oid),
                _ => bail!("HEAD does not point to a valid object"),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Subcommand implementations
// ---------------------------------------------------------------------------

/// List all notes.
fn list_all_notes(repo: &Repository, notes_ref: &str) -> Result<()> {
    let entries = read_notes_tree(repo, notes_ref)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for entry in &entries {
        writeln!(out, "{} {}", entry.oid.to_hex(), display_note_path(entry))?;
    }
    Ok(())
}

/// List the note for a specific object.
fn list_note_for_object(repo: &Repository, notes_ref: &str, object: &str) -> Result<()> {
    let oid = resolve_object(repo, Some(object))?;
    let hex = oid.to_hex();
    let entries = read_notes_tree(repo, notes_ref)?;

    for entry in &entries {
        if note_object_name(&entry.path).as_deref() == Some(hex.as_str()) {
            println!("{}", entry.oid.to_hex());
            return Ok(());
        }
    }

    bail!("No note found for object {hex}");
}

/// Add a note to an object.
/// Resolve the editor to use for notes.
fn resolve_editor(repo: &Repository) -> String {
    if let Ok(e) = std::env::var("GIT_EDITOR") {
        return e;
    }
    if let Ok(config) = ConfigSet::load(Some(&repo.git_dir), true) {
        if let Some(e) = config.get("core.editor") {
            return e;
        }
    }
    if let Ok(e) = std::env::var("VISUAL") {
        return e;
    }
    if let Ok(e) = std::env::var("EDITOR") {
        return e;
    }
    "vi".to_owned()
}

fn append_separator(buf: &mut String, sep: Option<&str>) {
    let Some(s) = sep else {
        return;
    };
    if s.is_empty() {
        if !buf.ends_with('\n') {
            buf.push('\n');
        }
        return;
    }
    if !s.starts_with('\n') && !buf.ends_with('\n') {
        buf.push('\n');
    }
    let separator = if buf.ends_with('\n') && s.starts_with('\n') {
        &s[1..]
    } else {
        s
    };
    if separator.as_bytes().last() == Some(&b'\n') {
        buf.push_str(separator);
    } else {
        buf.push_str(separator);
        buf.push('\n');
    }
}

fn concat_note_fragments(parts: &[String], separator: Option<&str>) -> String {
    let mut out = String::new();
    for p in parts {
        if !out.is_empty() {
            append_separator(&mut out, separator);
        }
        out.push_str(p);
    }
    out
}

fn read_note_file(path: &std::path::PathBuf) -> Result<String> {
    if path.as_os_str() == "-" {
        let mut s = String::new();
        io::stdin().read_to_string(&mut s)?;
        Ok(s)
    } else {
        fs::read_to_string(path).with_context(|| format!("reading '{}'", path.display()))
    }
}

fn load_blob_content(repo: &Repository, spec: &str) -> Result<Vec<u8>> {
    let oid = resolve_revision(repo, spec)
        .with_context(|| format!("failed to resolve '{spec}' as a valid ref."))?;
    let obj = repo
        .odb
        .read(&oid)
        .with_context(|| format!("reading '{spec}'"))?;
    if obj.kind != ObjectKind::Blob {
        bail!("cannot read note data from non-blob object '{spec}'.");
    }
    Ok(obj.data)
}

fn ordered_note_fragments_from_argv(
    repo: &Repository,
    add_newline_to_multiple_messages: bool,
) -> Result<Option<Vec<String>>> {
    enum Fragment {
        Message(String),
        Other(String),
    }

    let argv: Vec<String> = std::env::args().collect();
    let Some(notes_pos) = argv.iter().position(|a| a == "notes") else {
        return Ok(None);
    };
    let mut i = notes_pos + 1;
    while i < argv.len() {
        let arg = &argv[i];
        if matches!(arg.as_str(), "add" | "append" | "edit") {
            i += 1;
            break;
        }
        i += 1;
    }
    let mut out = Vec::new();
    let mut saw_fragment = false;
    while i < argv.len() {
        let arg = &argv[i];
        match arg.as_str() {
            "-m" | "--message" => {
                if let Some(v) = argv.get(i + 1) {
                    out.push(Fragment::Message(v.clone()));
                    saw_fragment = true;
                    i += 2;
                    continue;
                }
            }
            "-F" | "--file" => {
                if let Some(v) = argv.get(i + 1) {
                    out.push(Fragment::Other(read_note_file(&PathBuf::from(v))?));
                    saw_fragment = true;
                    i += 2;
                    continue;
                }
            }
            "-C" | "--reuse-message" | "-c" | "--reedit-message" => {
                if let Some(v) = argv.get(i + 1) {
                    out.push(Fragment::Other(
                        String::from_utf8_lossy(&load_blob_content(repo, v)?).into_owned(),
                    ));
                    saw_fragment = true;
                    i += 2;
                    continue;
                }
            }
            _ => {}
        }
        if let Some(v) = arg.strip_prefix("--message=") {
            out.push(Fragment::Message(v.to_owned()));
            saw_fragment = true;
        } else if let Some(v) = arg.strip_prefix("--file=") {
            out.push(Fragment::Other(read_note_file(&PathBuf::from(v))?));
            saw_fragment = true;
        } else if let Some(v) = arg.strip_prefix("--reuse-message=") {
            out.push(Fragment::Other(
                String::from_utf8_lossy(&load_blob_content(repo, v)?).into_owned(),
            ));
            saw_fragment = true;
        } else if let Some(v) = arg.strip_prefix("--reedit-message=") {
            out.push(Fragment::Other(
                String::from_utf8_lossy(&load_blob_content(repo, v)?).into_owned(),
            ));
            saw_fragment = true;
        } else if arg.starts_with("-m") && arg.len() > 2 {
            out.push(Fragment::Message(arg[2..].to_owned()));
            saw_fragment = true;
        } else if arg.starts_with("-F") && arg.len() > 2 {
            out.push(Fragment::Other(read_note_file(&PathBuf::from(&arg[2..]))?));
            saw_fragment = true;
        } else if arg.starts_with("-C") && arg.len() > 2 {
            out.push(Fragment::Other(
                String::from_utf8_lossy(&load_blob_content(repo, &arg[2..])?).into_owned(),
            ));
            saw_fragment = true;
        } else if arg.starts_with("-c") && arg.len() > 2 {
            out.push(Fragment::Other(
                String::from_utf8_lossy(&load_blob_content(repo, &arg[2..])?).into_owned(),
            ));
            saw_fragment = true;
        }
        i += 1;
    }
    if !saw_fragment {
        return Ok(None);
    }
    let multi = out.len() > 1;
    let mut previous_other = false;
    let last_index = out.len().saturating_sub(1);
    Ok(Some(
        out.into_iter()
            .enumerate()
            .map(|(idx, fragment)| match fragment {
                Fragment::Message(mut value) => {
                    if add_newline_to_multiple_messages && multi {
                        if value.bytes().all(|b| b == b'\n') {
                            if idx != last_index {
                                value.push('\n');
                            }
                        } else if !value.ends_with('\n') {
                            value.push('\n');
                        }
                    }
                    previous_other = false;
                    value
                }
                Fragment::Other(value) => {
                    let value = if add_newline_to_multiple_messages && previous_other {
                        format!("\n{value}")
                    } else {
                        value
                    };
                    previous_other = true;
                    value
                }
            })
            .collect(),
    ))
}

fn last_ordered_note_fragment_is_reuse_message() -> bool {
    let argv: Vec<String> = std::env::args().collect();
    let Some(notes_pos) = argv.iter().position(|a| a == "notes") else {
        return false;
    };
    let mut i = notes_pos + 1;
    while i < argv.len() {
        if matches!(argv[i].as_str(), "add" | "append" | "edit") {
            i += 1;
            break;
        }
        i += 1;
    }
    let mut last_is_reuse = false;
    while i < argv.len() {
        match argv[i].as_str() {
            "-m" | "--message" | "-F" | "--file" | "-c" | "--reedit-message" => {
                last_is_reuse = false;
                i += 2;
                continue;
            }
            "-C" | "--reuse-message" => {
                last_is_reuse = true;
                i += 2;
                continue;
            }
            _ => {}
        }
        if argv[i].starts_with("--message=")
            || argv[i].starts_with("--file=")
            || argv[i].starts_with("--reedit-message=")
            || (argv[i].starts_with("-m") && argv[i].len() > 2)
            || (argv[i].starts_with("-F") && argv[i].len() > 2)
            || (argv[i].starts_with("-c") && argv[i].len() > 2)
        {
            last_is_reuse = false;
        } else if argv[i].starts_with("--reuse-message=")
            || (argv[i].starts_with("-C") && argv[i].len() > 2)
        {
            last_is_reuse = true;
        }
        i += 1;
    }
    last_is_reuse
}

/// Launch the editor on a temporary file and return its contents.
fn launch_editor(repo: &Repository, initial: &str) -> Result<String> {
    let editor = resolve_editor(repo);
    let tmp_dir = repo.git_dir.join("tmp");
    let _ = std::fs::create_dir_all(&tmp_dir);
    let tmp_path = tmp_dir.join("NOTES_EDITMSG");
    std::fs::write(&tmp_path, initial)?;

    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("{} \"$@\"", editor))
        .arg("--")
        .arg(tmp_path.to_string_lossy().as_ref())
        .status()
        .with_context(|| format!("failed to launch editor '{editor}'"))?;

    if !status.success() {
        let _ = std::fs::remove_file(&tmp_path);
        bail!("editor exited with non-zero status");
    }

    let result = std::fs::read_to_string(&tmp_path)?;
    let _ = std::fs::remove_file(&tmp_path);
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
fn add_note(
    repo: &Repository,
    notes_ref: &str,
    object: Option<&str>,
    messages: &[String],
    files: &[std::path::PathBuf],
    reuse_message: Option<&str>,
    reedit_message: Option<&str>,
    use_editor: bool,
    force: bool,
    allow_empty: bool,
    stripspace: bool,
    no_stripspace: bool,
    separator: Option<&str>,
) -> Result<()> {
    let oid = resolve_object(repo, object)?;
    let hex = oid.to_hex();
    let mut entries = read_notes_tree(repo, notes_ref)?;
    let existing_content = entries
        .iter()
        .find(|e| note_object_name(&e.path).as_deref() == Some(hex.as_str()))
        .and_then(|e| repo.odb.read(&e.oid).ok())
        .map(|obj| String::from_utf8_lossy(&obj.data).to_string());
    let parts: Vec<String> = if let Some(ordered) =
        ordered_note_fragments_from_argv(repo, no_stripspace && separator.is_none())?
    {
        ordered
    } else {
        let mut parts = Vec::new();
        for m in messages {
            parts.push(m.clone());
        }
        for f in files {
            parts.push(read_note_file(f)?);
        }
        if let Some(spec) = reuse_message {
            let data = load_blob_content(repo, spec)?;
            parts.push(String::from_utf8_lossy(&data).into_owned());
        }
        if let Some(spec) = reedit_message {
            let data = load_blob_content(repo, spec)?;
            parts.push(String::from_utf8_lossy(&data).into_owned());
        }
        parts
    };
    let has_cli = !parts.is_empty()
        || reuse_message.is_some()
        || reedit_message.is_some()
        || !messages.is_empty();
    if existing_content.is_some() && has_cli && !force {
        bail!(
            "Cannot add notes. Found existing notes for object {}. Use '-f' to overwrite existing notes",
            hex
        );
    }
    let only_minus_c = reuse_message.is_some()
        && messages.is_empty()
        && files.is_empty()
        && reedit_message.is_none()
        && !use_editor;
    let mut combined = concat_note_fragments(&parts, separator);
    if reedit_message.is_some() {
        combined = launch_editor(repo, &combined)?;
    } else if use_editor && has_cli {
        combined = launch_editor(repo, &combined)?;
    } else if !has_cli {
        let initial = existing_content.as_deref().unwrap_or("");
        combined = launch_editor(repo, initial)?;
        if combined.trim().is_empty() && !allow_empty {
            if existing_content.is_some() {
                entries.retain(|e| note_object_name(&e.path).as_deref() != Some(hex.as_str()));
                write_notes_commit(
                    repo,
                    notes_ref,
                    &entries,
                    "Notes removed by 'git notes add'",
                )?;
                eprintln!("Removing note for object {hex}");
                return Ok(());
            }
            return Ok(());
        }
    }
    let should_strip = if no_stripspace {
        false
    } else if stripspace {
        true
    } else {
        !only_minus_c && !last_ordered_note_fragment_is_reuse_message()
    };
    if should_strip {
        combined = String::from_utf8_lossy(&stripspace_process(
            combined.as_bytes(),
            &StripspaceMode::Default,
        ))
        .into_owned();
    }
    let empty = combined.trim().is_empty();
    entries.retain(|e| note_object_name(&e.path).as_deref() != Some(hex.as_str()));
    if empty && !allow_empty {
        if existing_content.is_some() {
            write_notes_commit(
                repo,
                notes_ref,
                &entries,
                "Notes removed by 'git notes add'",
            )?;
        }
        eprintln!("Removing note for object {hex}");
        return Ok(());
    }
    let note_oid = if let Some(reuse) = reuse_message.filter(|_| only_minus_c && !stripspace) {
        resolve_revision(repo, reuse)?
    } else {
        if !combined.ends_with('\n') && !combined.is_empty() {
            combined.push('\n');
        }
        repo.odb.write(ObjectKind::Blob, combined.as_bytes())?
    };
    entries.push(NotesTreeEntry {
        mode: 0o100644,
        path: hex.as_bytes().to_vec(),
        oid: note_oid,
    });
    write_notes_commit(repo, notes_ref, &entries, "Notes added by 'git notes add'")?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn append_or_edit_note(
    repo: &Repository,
    notes_ref: &str,
    object: Option<&str>,
    is_edit: bool,
    messages: &[String],
    files: &[std::path::PathBuf],
    reuse_message: Option<&str>,
    reedit_message: Option<&str>,
    use_editor: bool,
    allow_empty: bool,
    stripspace: bool,
    no_stripspace: bool,
    separator: Option<&str>,
) -> Result<()> {
    if is_edit && (!messages.is_empty() || !files.is_empty() || reuse_message.is_some()) {
        eprintln!(
            "The -m/-F/-c/-C options have been deprecated for the 'edit' subcommand.\n\
Please use 'git notes add -f -m/-F/-c/-C' instead."
        );
    }
    let oid = resolve_object(repo, object)?;
    let hex = oid.to_hex();
    let mut entries = read_notes_tree(repo, notes_ref)?;
    let existing = entries
        .iter()
        .find(|e| note_object_name(&e.path).as_deref() == Some(hex.as_str()))
        .and_then(|e| repo.odb.read(&e.oid).ok())
        .map(|obj| String::from_utf8_lossy(&obj.data).to_string());
    let note_exists = existing.is_some();
    let mut parts: Vec<String> = if let Some(ordered) =
        ordered_note_fragments_from_argv(repo, no_stripspace && separator.is_none())?
    {
        ordered
    } else {
        let mut parts = Vec::new();
        for m in messages {
            parts.push(m.clone());
        }
        for f in files {
            parts.push(read_note_file(f)?);
        }
        if let Some(spec) = reuse_message {
            let data = load_blob_content(repo, spec)?;
            parts.push(String::from_utf8_lossy(&data).into_owned());
        }
        if let Some(spec) = reedit_message {
            let data = load_blob_content(repo, spec)?;
            parts.push(String::from_utf8_lossy(&data).into_owned());
        }
        parts
    };
    if !is_edit
        && messages.is_empty()
        && files.is_empty()
        && reuse_message.is_none()
        && reedit_message.is_none()
        && !use_editor
    {
        if let Ok(m) = std::env::var("MSG") {
            parts.push(m);
        }
    }
    let has_cli = !parts.is_empty() || reuse_message.is_some() || reedit_message.is_some();
    let mut combined = if is_edit {
        concat_note_fragments(&parts, separator)
    } else {
        let mut base = existing.clone().unwrap_or_default();
        let mut frag = concat_note_fragments(&parts, separator);
        if reedit_message.is_some() {
            frag = launch_editor(repo, &frag)?;
        } else if use_editor && has_cli {
            frag = launch_editor(repo, &frag)?;
        }
        if !frag.is_empty() {
            if !base.is_empty() {
                let base_separator = if no_stripspace && separator.is_none() {
                    Some("\n")
                } else {
                    separator
                };
                append_separator(&mut base, base_separator);
            }
            base.push_str(&frag);
        }
        base
    };
    if reedit_message.is_some() && is_edit {
        combined = launch_editor(repo, &combined)?;
    } else if use_editor && has_cli && is_edit {
        combined = launch_editor(repo, &combined)?;
    } else if !is_edit && !has_cli && !use_editor {
        let edited = launch_editor(repo, "")?;
        if edited.trim().is_empty() {
            bail!("Aborting due to empty note");
        }
        combined = edited;
    } else if is_edit && !has_cli && !use_editor && reedit_message.is_none() {
        combined = launch_editor(repo, existing.as_deref().unwrap_or(""))?;
    }
    let only_minus_c = reuse_message.is_some()
        && messages.is_empty()
        && files.is_empty()
        && reedit_message.is_none()
        && !use_editor;
    let should_strip = if no_stripspace {
        false
    } else if stripspace {
        true
    } else {
        !only_minus_c && !last_ordered_note_fragment_is_reuse_message()
    };
    if should_strip {
        combined = String::from_utf8_lossy(&stripspace_process(
            combined.as_bytes(),
            &StripspaceMode::Default,
        ))
        .into_owned();
    }
    let empty = combined.trim().is_empty();
    entries.retain(|e| note_object_name(&e.path).as_deref() != Some(hex.as_str()));
    if empty && !allow_empty {
        if note_exists {
            let msg = if is_edit {
                "Notes removed by 'git notes edit'"
            } else {
                "Notes removed by 'git notes append'"
            };
            write_notes_commit(repo, notes_ref, &entries, msg)?;
            eprintln!("Removing note for object {hex}");
        }
        return Ok(());
    }
    if !combined.ends_with('\n') && !combined.is_empty() {
        combined.push('\n');
    }
    let note_oid = repo.odb.write(ObjectKind::Blob, combined.as_bytes())?;
    entries.push(NotesTreeEntry {
        mode: 0o100644,
        path: hex.as_bytes().to_vec(),
        oid: note_oid,
    });
    let log = if is_edit {
        "Notes added by 'git notes edit'"
    } else {
        "Notes added by 'git notes append'"
    };
    write_notes_commit(repo, notes_ref, &entries, log)?;
    Ok(())
}

/// Show the note for an object.
fn show_note(repo: &Repository, notes_ref: &str, object: Option<&str>) -> Result<()> {
    let oid = resolve_object(repo, object)?;
    let hex = oid.to_hex();
    let entries = read_notes_tree(repo, notes_ref)?;

    for entry in &entries {
        if note_object_name(&entry.path).as_deref() == Some(hex.as_str()) {
            let blob = repo.odb.read(&entry.oid)?;
            if blob.kind != ObjectKind::Blob {
                bail!("note entry is not a blob");
            }
            let stdout = io::stdout();
            let mut out = stdout.lock();
            out.write_all(&blob.data)?;
            return Ok(());
        }
    }

    bail!("No note found for object {hex}");
}

fn remove_notes(
    repo: &Repository,
    notes_ref: &str,
    ignore_missing: bool,
    from_stdin: bool,
    objects: &[String],
) -> Result<()> {
    let mut targets: Vec<String> = objects.to_vec();
    if from_stdin {
        let mut line = String::new();
        while io::stdin().read_line(&mut line)? > 0 {
            let t = line.trim();
            if !t.is_empty() {
                targets.push(t.to_string());
            }
            line.clear();
        }
    }
    if targets.is_empty() && !from_stdin {
        targets.push("HEAD".to_string());
    }
    let entries_before = read_notes_tree(repo, notes_ref)?;
    let count_before = entries_before.len();
    let mut retval = 0i32;
    let mut oids: Vec<ObjectId> = Vec::new();
    for name in &targets {
        match resolve_revision(repo, name) {
            Ok(o) => oids.push(o),
            Err(_) => {
                eprintln!("error: Failed to resolve '{name}' as a valid ref.");
                retval = 1;
            }
        }
    }
    for oid in &oids {
        let hex = oid.to_hex();
        let has = entries_before
            .iter()
            .any(|e| note_object_name(&e.path).as_deref() == Some(hex.as_str()));
        if !has {
            eprintln!("Object {hex} has no note");
            if !ignore_missing {
                retval = 1;
            }
        }
    }
    if retval != 0 {
        std::process::exit(1);
    }
    let mut entries = entries_before;
    for oid in oids {
        let hex = oid.to_hex();
        let len = entries.len();
        entries.retain(|e| note_object_name(&e.path).as_deref() != Some(hex.as_str()));
        if entries.len() != len {
            eprintln!("Removing note for object {hex}");
        }
    }
    if entries.len() != count_before {
        write_notes_commit(
            repo,
            notes_ref,
            &entries,
            "Notes removed by 'git notes remove'",
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum RewriteCombine {
    Overwrite,
    Ignore,
    Concatenate,
    CatSortUniq,
}

fn parse_rewrite_combine(s: &str) -> Option<RewriteCombine> {
    match s.to_ascii_lowercase().as_str() {
        "overwrite" => Some(RewriteCombine::Overwrite),
        "ignore" => Some(RewriteCombine::Ignore),
        "concatenate" => Some(RewriteCombine::Concatenate),
        "cat_sort_uniq" => Some(RewriteCombine::CatSortUniq),
        _ => None,
    }
}

struct RewriteCfg {
    refs: Vec<String>,
    combine: RewriteCombine,
}

fn expand_rewrite_ref(repo: &Repository, pattern: &str) -> Vec<String> {
    if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
        return grit_lib::refs::list_refs_glob(&repo.git_dir, pattern)
            .map(|items| items.into_iter().map(|(name, _)| name).collect())
            .unwrap_or_default();
    }
    vec![pattern.to_string()]
}

fn load_rewrite_cfg(repo: &Repository, cmd: &str) -> Result<Option<RewriteCfg>> {
    let cfg = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let key = format!("notes.rewrite.{cmd}");
    let enabled = cfg
        .get(&key)
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(true);
    let mut combine = RewriteCombine::Concatenate;
    if let Ok(v) = std::env::var("GIT_NOTES_REWRITE_MODE") {
        combine = parse_rewrite_combine(&v)
            .ok_or_else(|| anyhow::anyhow!("Bad GIT_NOTES_REWRITE_MODE value: '{v}'"))?;
    } else if let Some(v) = cfg.get("notes.rewriteMode") {
        combine = parse_rewrite_combine(&v)
            .ok_or_else(|| anyhow::anyhow!("Bad notes.rewriteMode value: '{v}'"))?;
    }
    let mut refs: Vec<String> = Vec::new();
    if let Ok(v) = std::env::var("GIT_NOTES_REWRITE_REF") {
        for p in v.split(':') {
            let s = p.trim();
            if !s.is_empty() {
                refs.extend(expand_rewrite_ref(repo, s));
            }
        }
    } else {
        for p in cfg.get_all("notes.rewriteRef") {
            let s = p.trim();
            if s.starts_with("refs/notes/") {
                refs.extend(expand_rewrite_ref(repo, s));
            }
        }
        if refs.is_empty() {
            if let Some(s) = cfg.get("notes.rewriteRef") {
                let s = s.trim();
                if s.starts_with("refs/notes/") {
                    refs.extend(expand_rewrite_ref(repo, s));
                }
            }
        }
    }
    refs.sort();
    refs.dedup();
    if !enabled || refs.is_empty() {
        return Ok(None);
    }
    Ok(Some(RewriteCfg { refs, combine }))
}

fn apply_rewrite_copy(
    repo: &Repository,
    entries: &mut Vec<NotesTreeEntry>,
    from: &ObjectId,
    to: &ObjectId,
    force: bool,
    combine: RewriteCombine,
) -> Result<()> {
    let from_hex = from.to_hex();
    let to_hex = to.to_hex();
    let from_blob = entries
        .iter()
        .find(|e| note_object_name(&e.path).as_deref() == Some(from_hex.as_str()))
        .map(|e| e.oid);
    let to_blob = entries
        .iter()
        .find(|e| note_object_name(&e.path).as_deref() == Some(to_hex.as_str()))
        .map(|e| e.oid);
    match combine {
        RewriteCombine::Ignore => Ok(()),
        RewriteCombine::Overwrite => {
            let Some(note) = from_blob else {
                return Ok(());
            };
            if to_blob.is_some() && !force {
                bail!("Cannot copy notes. Found existing notes for object {to_hex}. Use '-f' to overwrite existing notes");
            }
            if to_blob.is_some() && force {
                eprintln!("Overwriting existing notes for object {to_hex}");
            }
            entries.retain(|e| note_object_name(&e.path).as_deref() != Some(to_hex.as_str()));
            entries.push(NotesTreeEntry {
                mode: 0o100644,
                path: to_hex.as_bytes().to_vec(),
                oid: note,
            });
            Ok(())
        }
        RewriteCombine::Concatenate => {
            let new_oid = from_blob;
            let cur_oid = to_blob;
            let out = match (cur_oid, new_oid) {
                (None, None) => return Ok(()),
                (None, Some(n)) => n,
                (Some(c), None) => c,
                (Some(c), Some(n)) if c == n => c,
                (Some(c), Some(n)) => combine_notes_concatenate(repo, Some(&c), Some(&n))?,
            };
            entries.retain(|e| note_object_name(&e.path).as_deref() != Some(to_hex.as_str()));
            entries.push(NotesTreeEntry {
                mode: 0o100644,
                path: to_hex.as_bytes().to_vec(),
                oid: out,
            });
            Ok(())
        }
        RewriteCombine::CatSortUniq => match (to_blob, from_blob) {
            (None, None) => Ok(()),
            (Some(_t), None) => Ok(()),
            (None, Some(f)) => {
                entries.retain(|e| note_object_name(&e.path).as_deref() != Some(to_hex.as_str()));
                entries.push(NotesTreeEntry {
                    mode: 0o100644,
                    path: to_hex.as_bytes().to_vec(),
                    oid: f,
                });
                Ok(())
            }
            (Some(t), Some(f)) => {
                let out = combine_notes_cat_sort_uniq(repo, Some(&t), Some(&f))?;
                entries.retain(|e| note_object_name(&e.path).as_deref() != Some(to_hex.as_str()));
                entries.push(NotesTreeEntry {
                    mode: 0o100644,
                    path: to_hex.as_bytes().to_vec(),
                    oid: out,
                });
                Ok(())
            }
        },
    }
}

pub(crate) fn copy_notes_for_rewrite(
    repo: &Repository,
    cmd: &str,
    from_oid: &ObjectId,
    to_oid: &ObjectId,
) -> Result<()> {
    let Some(rcfg) = load_rewrite_cfg(repo, cmd)? else {
        return Ok(());
    };
    for refname in &rcfg.refs {
        let mut entries = read_notes_tree(repo, refname).unwrap_or_default();
        apply_rewrite_copy(repo, &mut entries, from_oid, to_oid, true, rcfg.combine)?;
        write_notes_commit(repo, refname, &entries, "Notes added by 'git notes copy'")?;
    }
    Ok(())
}

fn copy_notes(
    repo: &Repository,
    notes_ref: &str,
    force: bool,
    from_stdin: bool,
    for_rewrite: Option<&str>,
    objects: &[String],
) -> Result<()> {
    if from_stdin || for_rewrite.is_some() {
        if !objects.is_empty() {
            eprintln!("error: too many arguments");
            eprintln!(
                "usage: git notes copy [<options>] <from-object> <to-object>\n   or: git notes copy --stdin [<from-object> <to-object>]..."
            );
            std::process::exit(129);
        }
        if let Some(cmd) = for_rewrite {
            if let Some(rcfg) = load_rewrite_cfg(repo, cmd)? {
                let mut trees: Vec<(String, Vec<NotesTreeEntry>)> = rcfg
                    .refs
                    .iter()
                    .map(|r| (r.clone(), read_notes_tree(repo, r).unwrap_or_default()))
                    .collect();
                let mut line = String::new();
                let mut err = 0i32;
                while io::stdin().read_line(&mut line)? > 0 {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() < 2 {
                        bail!("malformed input line: '{}'.", line.trim_end());
                    }
                    let from_oid = resolve_revision(repo, parts[0]).with_context(|| {
                        format!("failed to resolve '{}' as a valid ref.", parts[0])
                    })?;
                    let to_oid = resolve_revision(repo, parts[1]).with_context(|| {
                        format!("failed to resolve '{}' as a valid ref.", parts[1])
                    })?;
                    for (_refname, ent) in trees.iter_mut() {
                        if apply_rewrite_copy(repo, ent, &from_oid, &to_oid, true, rcfg.combine)
                            .is_err()
                        {
                            eprintln!(
                                "error: failed to copy notes from '{}' to '{}'",
                                parts[0], parts[1]
                            );
                            err = 1;
                        }
                    }
                    line.clear();
                }
                for (refname, ent) in trees {
                    write_notes_commit(repo, &refname, &ent, "Notes added by 'git notes copy'")?;
                }
                if err != 0 {
                    std::process::exit(1);
                }
                return Ok(());
            }
            return Ok(());
        }
        let mut entries = read_notes_tree(repo, notes_ref)?;
        let mut line = String::new();
        let mut err = 0i32;
        while io::stdin().read_line(&mut line)? > 0 {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 2 {
                bail!("malformed input line: '{}'.", line.trim_end());
            }
            let from_oid = resolve_revision(repo, parts[0])
                .with_context(|| format!("failed to resolve '{}' as a valid ref.", parts[0]))?;
            let to_oid = resolve_revision(repo, parts[1])
                .with_context(|| format!("failed to resolve '{}' as a valid ref.", parts[1]))?;
            if let Err(_) = apply_rewrite_copy(
                repo,
                &mut entries,
                &from_oid,
                &to_oid,
                force,
                RewriteCombine::Overwrite,
            ) {
                eprintln!(
                    "error: failed to copy notes from '{}' to '{}'",
                    parts[0], parts[1]
                );
                err = 1;
            }
            line.clear();
        }
        if err != 0 {
            std::process::exit(1);
        }
        write_notes_commit(repo, notes_ref, &entries, "Notes added by 'git notes copy'")?;
        return Ok(());
    }
    let (from, to) = match objects.len() {
        0 => {
            eprintln!("error: too few arguments");
            eprintln!(
                "usage: git notes copy [<options>] <from-object> <to-object>\n   or: git notes copy --stdin [<from-object> <to-object>]..."
            );
            std::process::exit(129);
        }
        1 => (objects[0].as_str(), "HEAD"),
        2 => (objects[0].as_str(), objects[1].as_str()),
        _ => {
            eprintln!("error: too many arguments");
            eprintln!(
                "usage: git notes copy [<options>] <from-object> <to-object>\n   or: git notes copy --stdin [<from-object> <to-object>]..."
            );
            std::process::exit(129);
        }
    };
    let from_oid = resolve_revision(repo, from)
        .with_context(|| format!("failed to resolve '{from}' as a valid ref."))?;
    let to_oid = resolve_revision(repo, to)
        .with_context(|| format!("failed to resolve '{to}' as a valid ref."))?;
    let from_hex = from_oid.to_hex();
    let to_hex = to_oid.to_hex();
    let mut entries = read_notes_tree(repo, notes_ref)?;
    let source_entry = entries
        .iter()
        .find(|e| note_object_name(&e.path).as_deref() == Some(from_hex.as_str()))
        .ok_or_else(|| {
            anyhow::anyhow!("missing notes on source object {from_hex}. Cannot copy.")
        })?;
    let note_blob_oid = source_entry.oid;
    if entries
        .iter()
        .any(|e| note_object_name(&e.path).as_deref() == Some(to_hex.as_str()))
    {
        if !force {
            bail!(
                "Cannot copy notes. Found existing notes for object {}. Use '-f' to overwrite existing notes",
                to_hex
            );
        }
        eprintln!("Overwriting existing notes for object {to_hex}");
        entries.retain(|e| note_object_name(&e.path).as_deref() != Some(to_hex.as_str()));
    }
    entries.push(NotesTreeEntry {
        mode: 0o100644,
        path: to_hex.as_bytes().to_vec(),
        oid: note_blob_oid,
    });
    write_notes_commit(repo, notes_ref, &entries, "Notes added by 'git notes copy'")?;
    Ok(())
}

const NOTES_MERGE_PARTIAL: &str = "NOTES_MERGE_PARTIAL";
const NOTES_MERGE_REF: &str = "NOTES_MERGE_REF";
const NOTES_MERGE_WORKTREE: &str = "NOTES_MERGE_WORKTREE";

/// If another worktree already has `NOTES_MERGE_REF` → `target_ref`, return its working-tree path.
fn find_other_worktree_with_notes_merge_ref(
    repo: &Repository,
    target_ref: &str,
) -> Option<std::path::PathBuf> {
    let current_canon = repo.git_dir.canonicalize().ok()?;
    let common = common_dir(&repo.git_dir).unwrap_or_else(|| repo.git_dir.clone());
    let common_canon = common.canonicalize().unwrap_or(common.clone());

    let mut admins: Vec<PathBuf> = vec![common_canon.clone()];
    let worktrees_dir = common_canon.join("worktrees");
    if let Ok(entries) = fs::read_dir(&worktrees_dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                admins.push(p);
            }
        }
    }

    for admin in admins {
        let admin_canon = admin.canonicalize().unwrap_or(admin);
        if admin_canon == current_canon {
            continue;
        }
        let refpath = admin_canon.join(NOTES_MERGE_REF);
        let Ok(content) = fs::read_to_string(&refpath) else {
            continue;
        };
        let line = content.trim_end_matches('\n');
        let Some(sym) = line.strip_prefix("ref: ") else {
            continue;
        };
        if sym.trim() != target_ref {
            continue;
        }
        let path = if admin_canon == common_canon {
            common_canon
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| common_canon.clone())
        } else {
            PathBuf::from(worktree_path_from_admin(&admin_canon))
        };
        return Some(path);
    }
    None
}

fn worktree_path_from_admin(admin_dir: &std::path::Path) -> String {
    worktree_refs::worktree_path_from_admin(admin_dir)
}
fn parse_notes_merge_strategy_cli(s: &str) -> Result<NotesMergeStrategy> {
    parse_notes_merge_strategy_value(s).ok_or_else(|| anyhow::anyhow!("unknown -s/--strategy: {s}"))
}

fn parse_notes_merge_strategy_config(s: &str) -> Result<NotesMergeStrategy> {
    parse_notes_merge_strategy_value(s).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown notes merge strategy {s}\n\
fatal: unable to parse 'notes.mergeStrategy' from command-line config"
        )
    })
}

fn clean_notes_merge_worktree(worktree: &std::path::Path) -> Result<()> {
    if !worktree.is_dir() {
        return Ok(());
    }
    for e in fs::read_dir(worktree)? {
        let e = e?;
        let t = e.file_type()?;
        if t.is_file() {
            let _ = fs::remove_file(e.path());
        }
    }
    Ok(())
}

fn merge_notes_abort(repo: &Repository) -> Result<()> {
    let merge_git = notes_merge_git_dir(repo);
    let _ = delete_ref(&merge_git, NOTES_MERGE_PARTIAL);
    let _ = delete_ref(&merge_git, NOTES_MERGE_REF);
    clean_notes_merge_worktree(&notes_merge_worktree_path(repo))?;
    Ok(())
}

fn merge_notes_commit_cmd(repo: &Repository) -> Result<()> {
    let merge_git = notes_merge_git_dir(repo);
    let partial_oid = resolve_ref(&merge_git, NOTES_MERGE_PARTIAL)?;
    let target_ref = read_symbolic_ref(&merge_git, NOTES_MERGE_REF)?
        .ok_or_else(|| anyhow::anyhow!("failed to resolve NOTES_MERGE_REF"))?;
    let partial_obj = repo.odb.read(&partial_oid)?;
    if partial_obj.kind != ObjectKind::Commit {
        bail!("could not parse commit from NOTES_MERGE_PARTIAL.");
    }
    let partial_commit = parse_commit(&partial_obj.data)?;
    let worktree = notes_merge_worktree_path(repo);
    let mut entries = read_notes_tree(repo, NOTES_MERGE_PARTIAL)?;
    if worktree.is_dir() {
        for e in fs::read_dir(&worktree)? {
            let e = e?;
            let name = e.file_name();
            let name_str = name.to_string_lossy();
            if name_str == "." || name_str == ".." {
                continue;
            }
            if !e.file_type()?.is_file() {
                continue;
            }
            let Ok(obj) = ObjectId::from_hex(name_str.trim()) else {
                continue;
            };
            let data = fs::read(e.path())?;
            let blob_oid = repo.odb.write(ObjectKind::Blob, &data)?;
            upsert_note_entry(&mut entries, &obj.to_hex(), blob_oid);
        }
    }
    let msg = partial_commit.message.clone();
    if msg.trim().is_empty() {
        bail!("partial notes commit has empty message");
    }
    let current_target = resolve_ref(&repo.git_dir, &target_ref).ok();
    let expected_first_parent = partial_commit.parents.first().copied();
    if let (Some(cur), Some(exp)) = (current_target, expected_first_parent) {
        if cur != exp {
            bail!(
                "The notes ref {} is at {} but NOTES_MERGE_PARTIAL^1 expects {}. \
Finalize the merge from the correct ref or abort.",
                target_ref,
                cur.to_hex(),
                exp.to_hex()
            );
        }
    }
    let new_oid = write_notes_commit_with_parents(
        repo,
        &target_ref,
        &entries,
        &msg,
        &partial_commit.parents,
    )?;
    write_ref(&repo.git_dir, &target_ref, &new_oid)?;
    merge_notes_abort(repo)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn merge_notes_dispatch(
    repo: &Repository,
    notes_ref: &str,
    do_commit: bool,
    do_abort: bool,
    verbose: u8,
    quiet: u8,
    strategy: Option<&str>,
    source_ref: Option<&str>,
) -> Result<()> {
    let do_merge = strategy.is_some() || (!do_commit && !do_abort);
    if (do_merge as u8) + (do_commit as u8) + (do_abort as u8) != 1 {
        bail!("cannot mix --commit, --abort or -s/--strategy");
    }
    if do_merge && source_ref.is_none() {
        bail!("must specify a notes ref to merge");
    }
    if !do_merge && source_ref.is_some() {
        bail!("too many arguments");
    }
    if do_abort {
        return merge_notes_abort(repo);
    }
    if do_commit {
        return merge_notes_commit_cmd(repo);
    }
    let src_raw = source_ref.context("must specify a notes ref to merge")?;
    let remote_ref = if src_raw.starts_with("refs/") {
        src_raw.to_owned()
    } else {
        expand_notes_ref(src_raw)
    };
    let verbosity = i32::from(verbose).saturating_sub(i32::from(quiet));
    if verbosity > 0 {
        eprintln!("Merging notes from {remote_ref} into {notes_ref}");
    }
    let strategy = if let Some(s) = strategy {
        parse_notes_merge_strategy_cli(s)?
    } else {
        let config = ConfigSet::load(Some(&repo.git_dir), true)?;
        let short = notes_ref.strip_prefix("refs/notes/").unwrap_or(notes_ref);
        let key = format!("notes.{short}.mergeStrategy");
        if let Some(v) = config.get(&key) {
            parse_notes_merge_strategy_config(&v)?
        } else if let Some(v) = config.get("notes.mergeStrategy") {
            parse_notes_merge_strategy_config(&v)?
        } else {
            NotesMergeStrategy::Manual
        }
    };
    let merge_result = notes_merge_inner(repo, notes_ref, &remote_ref, strategy)?;
    match merge_result {
        Ok(new_oid) => {
            write_ref(&repo.git_dir, notes_ref, &new_oid)?;
            Ok(())
        }
        Err(partial_oid) => {
            let merge_git = notes_merge_git_dir(repo);
            if let Some(other_path) = find_other_worktree_with_notes_merge_ref(repo, notes_ref) {
                bail!(
                    "a notes merge into {} is already in-progress at {}",
                    notes_ref,
                    other_path.display()
                );
            }
            write_ref(&merge_git, NOTES_MERGE_PARTIAL, &partial_oid)?;
            write_symbolic_ref(&merge_git, NOTES_MERGE_REF, notes_ref)?;
            let wt_display = if let Some(wt) = repo.work_tree.as_ref() {
                match merge_git.strip_prefix(wt) {
                    Ok(rel) if !rel.as_os_str().is_empty() => {
                        format!("{}/NOTES_MERGE_WORKTREE", rel.display())
                    }
                    _ => format!("{}/NOTES_MERGE_WORKTREE", merge_git.display()),
                }
            } else {
                format!("{}/NOTES_MERGE_WORKTREE", merge_git.display())
            };
            bail!(
                "Automatic notes merge failed. Fix conflicts in {} \
and commit the result with 'git notes merge --commit', \
or abort the merge with 'git notes merge --abort'.",
                wt_display
            );
        }
    }
}

/// Prune notes for objects that no longer exist in the object database.
fn prune_notes(repo: &Repository, notes_ref: &str, dry_run: bool, verbose: bool) -> Result<()> {
    let entries = read_notes_tree(repo, notes_ref)?;
    let mut kept = Vec::new();
    let mut pruned_oids: Vec<String> = Vec::new();

    for entry in &entries {
        let name = display_note_path(entry);
        // The note name is the hex OID of the annotated object
        let obj_exists = if let Ok(oid) = ObjectId::from_hex(name.as_ref()) {
            repo.odb.read(&oid).is_ok()
        } else {
            // Non-hex name — keep it
            true
        };

        if obj_exists {
            kept.push(entry.clone());
        } else {
            pruned_oids.push(name.into_owned());
        }
    }

    // Match git: `-n` and/or `-v` print each pruned object's full hex to stdout (see notes.c).
    if verbose || dry_run {
        for oid_hex in &pruned_oids {
            println!("{oid_hex}");
        }
    }

    if !pruned_oids.is_empty() && !dry_run {
        write_notes_commit(repo, notes_ref, &kept, "Notes removed by 'git notes prune'")?;
    }

    Ok(())
}
