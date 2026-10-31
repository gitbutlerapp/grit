//! Warnings when nested repositories are recorded as gitlinks.

use crate::output::OutputMode;

/// Print warnings for paths newly staged as gitlinks (human and markdown modes).
pub fn emit_embedded_gitlink_warnings(mode: OutputMode, paths: &[String], dry_run: bool) {
    if paths.is_empty() {
        return;
    }
    let records = if dry_run { "would record" } else { "records" };
    for path in paths {
        let hint = format!("Remove `{path}/.git` and stage again to vendor the files inside.");
        let body = format!(
            "`{path}` is a nested repository; grit {records} it as a gitlink. Clones get an empty directory, not its files. {hint}"
        );
        match mode {
            OutputMode::Human => eprintln!("warning: {body}"),
            OutputMode::Markdown => eprintln!("> **Warning:** {body}"),
            OutputMode::Json => {}
        }
    }
}

/// Status note for untracked nested repositories (not yet indexed as gitlinks).
pub fn emit_embedded_repository_status_notes(mode: OutputMode, paths: &[String]) {
    if paths.is_empty() {
        return;
    }
    for path in paths {
        let body = format!(
            "`{path}` is a nested repository; staging it would record a gitlink and clones would not contain its files. Remove `{path}.git` to vendor the files, or add a `.gitmodules` entry if you intend a submodule."
        );
        match mode {
            OutputMode::Human => eprintln!("note: {body}"),
            OutputMode::Markdown => eprintln!("> **Note:** {body}"),
            OutputMode::Json => {}
        }
    }
}

/// Clone note for gitlinks that are not registered submodules.
pub fn emit_bare_gitlink_note(mode: OutputMode, paths: &[String]) {
    if paths.is_empty() {
        return;
    }
    let count = paths.len();
    let list = paths.join(", ");
    let noun = if count == 1 { "gitlink" } else { "gitlinks" };
    match mode {
        OutputMode::Human => eprintln!(
            "note: checkout left {count} empty {noun} ({list}) with no `.gitmodules` entry; nested repository files were not copied"
        ),
        OutputMode::Markdown => eprintln!(
            "> **Note:** checkout left {count} empty {noun} (`{list}`) with no `.gitmodules` entry; nested repository files were not copied."
        ),
        OutputMode::Json => {}
    }
}
