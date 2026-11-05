//! Warnings when nested repositories are recorded as gitlinks.

use crate::markdown::format_inline_code;
use crate::output::OutputMode;
use crate::ui::{path_display_for_discovered_repo, PathDisplayOptions};

fn format_path_for_display(path: &str, path_opts: Option<&PathDisplayOptions>) -> String {
    let discovered = path_display_for_discovered_repo();
    let opts = path_opts.or(discovered.as_ref());
    opts.map(|o| o.format_path_for_human(path))
        .unwrap_or_else(|| path.to_owned())
}

/// Print warnings for paths newly staged as gitlinks (human and markdown modes).
pub fn emit_embedded_gitlink_warnings(
    mode: OutputMode,
    paths: &[String],
    dry_run: bool,
    path_opts: Option<&PathDisplayOptions>,
) {
    if paths.is_empty() {
        return;
    }
    let records = if dry_run { "would record" } else { "records" };
    for path in paths {
        let shown = format_path_for_display(path, path_opts);
        let hint_shown = format_path_for_display(path, path_opts);
        match mode {
            OutputMode::Human => {
                let hint =
                    format!("Remove {hint_shown}/.git and stage again to vendor the files inside.");
                eprintln!(
                    "warning: {shown} is a nested repository; grit {records} it as a gitlink. Clones get an empty directory, not its files. {hint}"
                );
            }
            OutputMode::Markdown => {
                let path_code = format_inline_code(&shown);
                let hint_code = format_inline_code(&format!("{hint_shown}/.git"));
                eprintln!(
                    "> **Warning:** {path_code} is a nested repository; grit {records} it as a gitlink. Clones get an empty directory, not its files. Remove {hint_code} and stage again to vendor the files inside."
                );
            }
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
