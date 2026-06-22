//! Interactive `git add -i` — the top-level command menu (status/update/revert/add untracked/
//! patch/diff), mirroring the C implementation in `git/add-interactive.c`.
//!
//! The patch sub-command delegates to [`crate::commands::add_patch::run_add_patch`]; everything
//! else (computing per-file add/del counts, staging, reverting, listing untracked files) is
//! implemented here on top of the existing diff machinery in `grit-lib`.

use anyhow::{Context, Result};
use grit_lib::config::ConfigSet;
use grit_lib::diff::{
    count_changes_with_algorithm, diff_index_to_tree, diff_index_to_worktree, DiffEntry, DiffStatus,
};
use grit_lib::index::Index;
use grit_lib::objects::ObjectId;
use grit_lib::odb::Odb;
use grit_lib::repo::Repository;
use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};
use std::path::Path;

use crate::commands::add::{resolved_env_index_path, AddConfig};

/// Per-side add/delete counts for one path.
#[derive(Clone, Copy, Default)]
struct AddDel {
    add: u64,
    del: u64,
    seen: bool,
    binary: bool,
    unmerged: bool,
}

/// One file row in the interactive status/menu list.
#[derive(Clone, Default)]
struct FileItem {
    name: String,
    index: AddDel,
    worktree: AddDel,
    /// Length of the unique prefix (for highlighting); unused for output parity in non-color mode.
    prefix_length: usize,
}

/// Which diffs feed the file list, matching `enum modified_files_filter` in `add-interactive.c`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ModifiedFilter {
    /// Both index↔HEAD and worktree↔index (used by `status`).
    NoFilter,
    /// Only worktree↔index (used by `update`, `patch`).
    WorktreeOnly,
    /// Only index↔HEAD (used by `revert`, `diff`).
    IndexOnly,
}

/// Read a blob's bytes from the ODB, returning empty on the null OID.
fn read_blob(odb: &Odb, oid: &ObjectId) -> Vec<u8> {
    if oid.is_zero() {
        return Vec::new();
    }
    match odb.read(oid) {
        Ok(obj) => obj.data,
        Err(_) => Vec::new(),
    }
}

/// Read the new-side bytes. For worktree diffs the new OID is a hash of an on-disk file that is
/// not written to the ODB, so read directly from the worktree; index diffs read the blob.
fn read_new_side(odb: &Odb, entry: &DiffEntry, work_tree: &Path, from_worktree: bool) -> Vec<u8> {
    if entry.status == DiffStatus::Deleted {
        return Vec::new();
    }
    if from_worktree {
        let p = entry.path().to_fs_path(work_tree);
        std::fs::read(&p).unwrap_or_default()
    } else {
        read_blob(odb, &entry.new_oid)
    }
}

/// Count `(added, deleted)` lines for a diff entry the way `compute_diffstat` does.
fn count_add_del(
    odb: &Odb,
    entry: &DiffEntry,
    work_tree: &Path,
    from_worktree: bool,
) -> (u64, u64) {
    let old_raw = read_blob(odb, &entry.old_oid);
    let new_raw = read_new_side(odb, entry, work_tree, from_worktree);
    let old = String::from_utf8_lossy(&old_raw);
    let new = String::from_utf8_lossy(&new_raw);
    let (add, del) = count_changes_with_algorithm(&old, &new, similar::Algorithm::Myers, false);
    (add as u64, del as u64)
}

/// Whether the entry is a gitlink (submodule) on either side; interactive add ignores dirty
/// submodule worktrees the same way `get_modified_files` passes `ignore_dirty_submodules`.
fn is_gitlink(entry: &DiffEntry) -> bool {
    entry.old_mode == "160000" || entry.new_mode == "160000"
}

/// Compute the modified-files list, mirroring `get_modified_files()`:
/// two passes (worktree and index), with the second pass skipping unseen entries when filtered.
fn get_modified_files(
    odb: &Odb,
    index: &Index,
    work_tree: &Path,
    head_tree: Option<&ObjectId>,
    filter: ModifiedFilter,
) -> Result<Vec<FileItem>> {
    let mut map: BTreeMap<String, FileItem> = BTreeMap::new();

    // Pass order matches C: INDEX_ONLY runs index first then worktree; otherwise worktree first.
    let order = if filter == ModifiedFilter::IndexOnly {
        [false, true] // [worktree?, ...] -> index first
    } else {
        [true, false]
    };

    for (i, &from_worktree) in order.iter().enumerate() {
        let skip_unseen = filter != ModifiedFilter::NoFilter && i == 1;

        let entries = if from_worktree {
            // Worktree vs index; ignore dirty-submodule worktrees.
            diff_index_to_worktree(odb, index, work_tree, true, false)?
        } else {
            diff_index_to_tree(odb, index, head_tree, false)?
        };

        for entry in &entries {
            if is_gitlink(entry) {
                continue;
            }
            let name = entry.path().to_string();
            if skip_unseen && !map.contains_key(&name) {
                continue;
            }
            let (add, del) = count_add_del(odb, entry, work_tree, from_worktree);
            let item = map.entry(name.clone()).or_insert_with(|| FileItem {
                name,
                ..FileItem::default()
            });
            let slot = if from_worktree {
                &mut item.worktree
            } else {
                &mut item.index
            };
            slot.seen = true;
            slot.add = add;
            slot.del = del;
            slot.unmerged = entry.status == DiffStatus::Unmerged;
        }
    }

    Ok(map.into_values().collect())
}

/// `render_adddel`: `binary` / `+A/-D` / the no-change placeholder.
fn render_adddel(ad: &AddDel, no_changes: &str) -> String {
    if ad.binary {
        "binary".to_string()
    } else if ad.seen {
        format!("+{}/-{}", ad.add, ad.del)
    } else {
        no_changes.to_string()
    }
}

/// Print the file list with the `%12s %12s %s` layout and `staged unstaged path` header,
/// matching `list()` + `print_file_item()`.
fn print_file_list(out: &mut impl Write, files: &[FileItem], selected: Option<&[bool]>) {
    if files.is_empty() {
        return;
    }
    let header = format!("     {:>12} {:>12} {}", "staged", "unstaged", "path");
    writeln!(out, "{header}").ok();
    for (i, item) in files.iter().enumerate() {
        let sel = selected
            .map(|s| s.get(i).copied().unwrap_or(false))
            .unwrap_or(false);
        let mark = if sel { '*' } else { ' ' };
        let index = render_adddel(&item.index, "unchanged");
        let worktree = render_adddel(&item.worktree, "nothing");
        let _ = item.prefix_length;
        // Git's `print_file_item` uses `%c%2d:` — the selection marker char plus the 1-based
        // index right-aligned to width 2 (`  1:`), not width 3 (t3701 "brackets appear without
        // color"). The header's 5 leading spaces line up with `<mark><2-wide>: ` = 1 + 2 + 2.
        writeln!(
            out,
            "{mark}{:>2}: {:>12} {:>12} {}",
            i + 1,
            index,
            worktree,
            item.name
        )
        .ok();
    }
}

/// Print the file list for `add untracked` (only names, `only_names` mode in C).
fn print_name_list(out: &mut impl Write, files: &[FileItem], selected: &[bool]) {
    let header = format!("     {:>12} {:>12} {}", "staged", "unstaged", "path");
    writeln!(out, "{header}").ok();
    for (i, item) in files.iter().enumerate() {
        let sel = selected.get(i).copied().unwrap_or(false);
        let mark = if sel { '*' } else { ' ' };
        writeln!(out, "{mark}{:>2}: {}", i + 1, item.name).ok();
    }
}

/// Selection mode for [`list_and_choose`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum ChooseFlags {
    /// Multi-select (`update`, `revert`, `add untracked`, `patch`): returns count selected.
    Multi,
    /// Immediate multi-select (`diff`): finishes as soon as something is selected.
    Immediate,
}

/// Parse one input token like `5`, `5-7`, `5-`, `*`, or a unique prefix, returning the inclusive
/// `from` and exclusive `to` 0-based range, or `None` for an unparseable token.
fn parse_range(tok: &str, n: usize, files: &[FileItem]) -> Option<(usize, usize, bool)> {
    let mut t = tok;
    let mut choose = true;
    if let Some(rest) = t.strip_prefix('-') {
        choose = false;
        t = rest;
    }
    if t == "*" {
        return Some((0, n, choose));
    }
    if let Some(first) = t.chars().next() {
        if first.is_ascii_digit() {
            if let Some((a, b)) = t.split_once('-') {
                let from = a.parse::<usize>().ok()?.checked_sub(1)?;
                let to = if b.is_empty() {
                    n
                } else {
                    b.parse::<usize>().ok()?
                };
                return Some((from, to.min(n), choose));
            }
            let from = t.parse::<usize>().ok()?.checked_sub(1)?;
            return Some((from, from + 1, choose));
        }
    }
    // Unique-prefix match by name.
    let matches: Vec<usize> = files
        .iter()
        .enumerate()
        .filter(|(_, f)| f.name.starts_with(t))
        .map(|(i, _)| i)
        .collect();
    if matches.len() == 1 {
        let i = matches[0];
        return Some((i, i + 1, choose));
    }
    // Exact name match wins even if it is a prefix of others.
    if let Some(i) = files.iter().position(|f| f.name == t) {
        return Some((i, i + 1, choose));
    }
    None
}

/// `list_and_choose` for multi-select prompts; returns the boolean selection vector and the count.
fn list_and_choose(
    out: &mut impl Write,
    reader: &mut impl BufRead,
    files: &[FileItem],
    prompt: &str,
    flags: ChooseFlags,
) -> (Vec<bool>, isize) {
    let n = files.len();
    let mut selected = vec![false; n];
    let mut count: isize = 0;

    loop {
        print_file_list(out, files, Some(&selected));
        write!(out, "{prompt}>> ").ok();
        out.flush().ok();

        let mut line = String::new();
        let read = reader.read_line(&mut line).unwrap_or(0);
        if read == 0 {
            // EOF
            writeln!(out).ok();
            return (selected, count);
        }
        let line = line.trim_end_matches(['\n', '\r']);
        if line.is_empty() {
            break;
        }
        if line == "?" {
            print_select_help(out);
            continue;
        }

        for tok in line.split([' ', '\t', '\r', ',']).filter(|t| !t.is_empty()) {
            match parse_range(tok, n, files) {
                Some((from, to, choose)) => {
                    for idx in from..to.min(n) {
                        if selected[idx] != choose {
                            selected[idx] = choose;
                            count += if choose { 1 } else { -1 };
                        }
                    }
                }
                None => {
                    writeln!(out, "Huh ({tok})?").ok();
                }
            }
        }

        if flags == ChooseFlags::Immediate && count != 0 || line == "*" {
            break;
        }
    }

    (selected, count)
}

/// `choose_prompt_help` for the file-selection prompt.
fn print_select_help(out: &mut impl Write) {
    writeln!(out, "Prompt help:").ok();
    writeln!(out, "1          - select a single item").ok();
    writeln!(out, "3-5        - select a range of items").ok();
    writeln!(out, "2-3,6-9    - select multiple ranges").ok();
    writeln!(out, "foo        - select item based on unique prefix").ok();
    writeln!(out, "-...       - unselect specified items").ok();
    writeln!(out, "*          - choose all items").ok();
    writeln!(out, "           - (empty) finish selecting").ok();
}

/// Context bundle passed to each interactive sub-command.
struct AddIContext<'a> {
    repo: &'a Repository,
    index: Index,
    index_path: std::path::PathBuf,
    work_tree: &'a Path,
    head_tree: Option<ObjectId>,
    config: &'a ConfigSet,
    add_cfg: &'a AddConfig,
    pathspec: &'a [String],
}

/// Top-level entry point invoked from `add::run` for `git add -i`.
///
/// # Errors
/// Propagates I/O and ODB errors. Sub-command failures print to stderr and continue the loop.
pub fn run_add_i(
    repo: &Repository,
    index: Index,
    work_tree: &Path,
    config: &ConfigSet,
    add_cfg: &AddConfig,
    pathspec: &[String],
) -> Result<()> {
    let head_tree = resolve_head_tree(repo)?;
    let index_path = resolved_env_index_path(repo);
    let mut ctx = AddIContext {
        repo,
        index,
        index_path,
        work_tree,
        head_tree,
        config,
        add_cfg,
        pathspec,
    };

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let stdin = io::stdin();
    let mut reader = stdin.lock();

    // Initial status, like `run_status` before the loop.
    run_status(&mut ctx, &mut out)?;

    let commands = [
        "status",
        "update",
        "revert",
        "add untracked",
        "patch",
        "diff",
        "quit",
        "help",
    ];

    loop {
        print_command_menu(&mut out, &commands);
        write!(out, "What now> ").ok();
        out.flush().ok();

        let mut line = String::new();
        let read = reader.read_line(&mut line).unwrap_or(0);
        if read == 0 {
            writeln!(out).ok();
            writeln!(out, "Bye.").ok();
            break;
        }
        let line = line.trim_end_matches(['\n', '\r']);
        if line.is_empty() {
            continue;
        }
        if line == "?" {
            print_command_help(&mut out);
            continue;
        }

        let choice = resolve_command(line, &commands);
        match choice {
            Some(idx) => match commands[idx] {
                "status" => run_status(&mut ctx, &mut out)?,
                "update" => run_update(&mut ctx, &mut out, &mut reader)?,
                "revert" => run_revert(&mut ctx, &mut out, &mut reader)?,
                "add untracked" => run_add_untracked(&mut ctx, &mut out, &mut reader)?,
                "patch" => run_patch(&mut ctx, &mut out, &mut reader)?,
                "diff" => run_diff(&mut ctx, &mut out, &mut reader)?,
                "quit" => {
                    writeln!(out, "Bye.").ok();
                    break;
                }
                "help" => run_help(&mut out),
                _ => {}
            },
            None => {
                writeln!(out, "Huh ({line})?").ok();
            }
        }
    }

    Ok(())
}

/// Resolve a typed command name to its index by number or unique prefix.
fn resolve_command(input: &str, commands: &[&str]) -> Option<usize> {
    if let Ok(num) = input.parse::<usize>() {
        if num >= 1 && num <= commands.len() {
            return Some(num - 1);
        }
        return None;
    }
    let matches: Vec<usize> = commands
        .iter()
        .enumerate()
        .filter(|(_, c)| c.starts_with(input))
        .map(|(i, _)| i)
        .collect();
    if matches.len() == 1 {
        return Some(matches[0]);
    }
    commands.iter().position(|c| *c == input)
}

/// Print the `*** Commands ***` menu in the 4-column layout C uses.
fn print_command_menu(out: &mut impl Write, commands: &[&str]) {
    writeln!(out, "*** Commands ***").ok();
    let cols = 4;
    for (i, name) in commands.iter().enumerate() {
        // Highlight the first letter with brackets (C uses the unique prefix; one letter suffices
        // for all of these command names except `add untracked` whose prefix is `a`).
        let label = bracket_prefix(name);
        write!(out, " {:>2}: {label}", i + 1).ok();
        if (i + 1) % cols != 0 {
            write!(out, "\t").ok();
        } else {
            writeln!(out).ok();
        }
    }
    if commands.len() % cols != 0 {
        writeln!(out).ok();
    }
}

/// Wrap the unique single-character prefix in brackets, e.g. `status` -> `[s]tatus`.
fn bracket_prefix(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => format!("[{first}]{}", chars.as_str()),
        None => name.to_string(),
    }
}

/// `command_prompt_help`.
fn print_command_help(out: &mut impl Write) {
    writeln!(out, "Prompt help:").ok();
    writeln!(out, "1          - select a numbered item").ok();
    writeln!(out, "foo        - select item based on unique prefix").ok();
    writeln!(out, "           - (empty) select nothing").ok();
}

/// `run_help`.
fn run_help(out: &mut impl Write) {
    writeln!(out, "status        - show paths with changes").ok();
    writeln!(
        out,
        "update        - add working tree state to the staged set of changes"
    )
    .ok();
    writeln!(
        out,
        "revert        - revert staged set of changes back to the HEAD version"
    )
    .ok();
    writeln!(out, "patch         - pick hunks and update selectively").ok();
    writeln!(out, "diff          - view diff between HEAD and index").ok();
    writeln!(
        out,
        "add untracked - add contents of untracked files to the staged set of changes"
    )
    .ok();
}

/// Resolve the HEAD commit's tree OID, `None` for an unborn branch.
fn resolve_head_tree(repo: &Repository) -> Result<Option<ObjectId>> {
    let head = grit_lib::state::resolve_head(&repo.git_dir)?;
    match head.oid() {
        Some(oid) => {
            let obj = repo.odb.read(oid)?;
            let commit = grit_lib::objects::parse_commit(&obj.data)?;
            Ok(Some(commit.tree))
        }
        None => Ok(None),
    }
}

/// `run_status`: print the full status list then a blank line.
fn run_status(ctx: &mut AddIContext, out: &mut impl Write) -> Result<()> {
    let files = get_modified_files(
        &ctx.repo.odb,
        &ctx.index,
        ctx.work_tree,
        ctx.head_tree.as_ref(),
        ModifiedFilter::NoFilter,
    )?;
    print_file_list(out, &files, None);
    writeln!(out).ok();
    Ok(())
}

/// `run_update`: stage selected worktree paths (including deletions).
fn run_update(
    ctx: &mut AddIContext,
    out: &mut impl Write,
    reader: &mut impl BufRead,
) -> Result<()> {
    let files = get_modified_files(
        &ctx.repo.odb,
        &ctx.index,
        ctx.work_tree,
        ctx.head_tree.as_ref(),
        ModifiedFilter::WorktreeOnly,
    )?;
    if files.is_empty() {
        writeln!(out).ok();
        return Ok(());
    }
    let (selected, count) = list_and_choose(out, reader, &files, "Update ", ChooseFlags::Multi);
    if count <= 0 {
        writeln!(out).ok();
        return Ok(());
    }

    for (i, item) in files.iter().enumerate() {
        if !selected[i] {
            continue;
        }
        let abs = ctx.work_tree.join(&item.name);
        if !abs.exists() {
            ctx.index.remove(item.name.as_bytes());
        } else {
            stage_one(ctx, &item.name, &abs)?;
        }
    }
    ctx.repo.write_index_at(&ctx.index_path, &mut ctx.index)?;

    let word = if count == 1 { "path" } else { "paths" };
    writeln!(out, "updated {count} {word}").ok();
    writeln!(out).ok();
    Ok(())
}

/// Stage one worktree path into the in-memory index using the shared `stage_file` machinery.
fn stage_one(ctx: &mut AddIContext, rel: &str, abs: &Path) -> Result<()> {
    let stage_ctx = crate::commands::add::StageFileContext::for_commit();
    crate::commands::add::stage_file(
        &ctx.repo.odb,
        &mut ctx.index,
        ctx.work_tree,
        rel,
        abs,
        ctx.repo,
        &stage_ctx,
        ctx.add_cfg,
    )
}

/// `run_revert`: reset selected index entries back to their HEAD version.
fn run_revert(
    ctx: &mut AddIContext,
    out: &mut impl Write,
    reader: &mut impl BufRead,
) -> Result<()> {
    let files = get_modified_files(
        &ctx.repo.odb,
        &ctx.index,
        ctx.work_tree,
        ctx.head_tree.as_ref(),
        ModifiedFilter::IndexOnly,
    )?;
    if files.is_empty() {
        writeln!(out).ok();
        return Ok(());
    }
    let (selected, count) = list_and_choose(out, reader, &files, "Revert ", ChooseFlags::Multi);
    if count <= 0 {
        writeln!(out).ok();
        return Ok(());
    }

    // Map HEAD tree paths -> the index entry as it should appear after revert.
    let head_entries = match &ctx.head_tree {
        Some(tree) => crate::commands::reset::tree_to_flat_entries(ctx.repo, tree, "")?,
        None => Vec::new(),
    };
    let mut head_map: BTreeMap<String, grit_lib::index::IndexEntry> = BTreeMap::new();
    for e in head_entries {
        head_map.insert(String::from_utf8_lossy(&e.path).into_owned(), e);
    }

    for (i, item) in files.iter().enumerate() {
        if !selected[i] {
            continue;
        }
        ctx.index.remove(item.name.as_bytes());
        match head_map.get(&item.name) {
            Some(entry) => {
                ctx.index.entries.push(entry.clone());
            }
            None => {
                writeln!(out, "note: {} is untracked now.", item.name).ok();
            }
        }
    }
    ctx.index.entries.sort_by(|a, b| a.path.cmp(&b.path));
    ctx.repo.write_index_at(&ctx.index_path, &mut ctx.index)?;

    let word = if count == 1 { "path" } else { "paths" };
    writeln!(out, "reverted {count} {word}").ok();
    writeln!(out).ok();
    Ok(())
}

/// `run_add_untracked`: stage selected untracked files.
fn run_add_untracked(
    ctx: &mut AddIContext,
    out: &mut impl Write,
    reader: &mut impl BufRead,
) -> Result<()> {
    let untracked = crate::commands::status::collect_untracked_normal_for_status(
        ctx.repo,
        &ctx.index,
        ctx.work_tree,
        if ctx.pathspec.is_empty() {
            None
        } else {
            Some(ctx.pathspec)
        },
    )?;
    let files: Vec<FileItem> = untracked
        .into_iter()
        .map(|name| FileItem {
            name,
            ..FileItem::default()
        })
        .collect();
    if files.is_empty() {
        writeln!(out, "No untracked files.").ok();
        writeln!(out).ok();
        return Ok(());
    }

    let n = files.len();
    let mut selected = vec![false; n];
    let mut count: isize = 0;
    loop {
        print_name_list(out, &files, &selected);
        write!(out, "Add untracked>> ").ok();
        out.flush().ok();
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            writeln!(out).ok();
            break;
        }
        let line = line.trim_end_matches(['\n', '\r']);
        if line.is_empty() {
            break;
        }
        if line == "?" {
            print_select_help(out);
            continue;
        }
        for tok in line.split([' ', '\t', '\r', ',']).filter(|t| !t.is_empty()) {
            match parse_range(tok, n, &files) {
                Some((from, to, choose)) => {
                    for idx in from..to.min(n) {
                        if selected[idx] != choose {
                            selected[idx] = choose;
                            count += if choose { 1 } else { -1 };
                        }
                    }
                }
                None => {
                    writeln!(out, "Huh ({tok})?").ok();
                }
            }
        }
        if line == "*" {
            break;
        }
    }
    if count <= 0 {
        writeln!(out).ok();
        return Ok(());
    }

    for (i, item) in files.iter().enumerate() {
        if !selected[i] {
            continue;
        }
        let abs = ctx.work_tree.join(&item.name);
        stage_one(ctx, &item.name, &abs)?;
    }
    ctx.repo.write_index_at(&ctx.index_path, &mut ctx.index)?;

    let word = if count == 1 { "path" } else { "paths" };
    writeln!(out, "added {count} {word}").ok();
    writeln!(out).ok();
    Ok(())
}

/// `run_patch`: select worktree files and hand them to `add -p`.
fn run_patch(ctx: &mut AddIContext, out: &mut impl Write, reader: &mut impl BufRead) -> Result<()> {
    let files = get_modified_files(
        &ctx.repo.odb,
        &ctx.index,
        ctx.work_tree,
        ctx.head_tree.as_ref(),
        ModifiedFilter::WorktreeOnly,
    )?;
    // Drop unmerged entries (and report), like C.
    let files: Vec<FileItem> = files
        .into_iter()
        .filter(|f| {
            if f.index.unmerged || f.worktree.unmerged {
                writeln!(out, "ignoring unmerged: {}", f.name).ok();
                false
            } else {
                true
            }
        })
        .collect();
    if files.is_empty() {
        writeln!(out, "No changes.").ok();
        return Ok(());
    }
    let (selected, count) =
        list_and_choose(out, reader, &files, "Patch update ", ChooseFlags::Multi);
    if count <= 0 {
        return Ok(());
    }
    let chosen: Vec<String> = files
        .iter()
        .enumerate()
        .filter(|(i, _)| selected[*i])
        .map(|(_, f)| f.name.clone())
        .collect();

    // Make sure add -p sees our current index, then delegate, threading our stdin reader through
    // so buffered input (the hunk decisions) is not lost across the two readers.
    ctx.repo.write_index_at(&ctx.index_path, &mut ctx.index)?;
    let patch_opts = crate::commands::add_patch::PatchOptions {
        context: crate::commands::add::resolve_patch_context(None, ctx.config)?,
        inter_hunk_context: crate::commands::add::resolve_patch_interhunk(None, ctx.config)?,
        auto_advance: true,
    };
    crate::commands::add_patch::run_add_patch_with_reader(
        ctx.repo,
        &chosen,
        ctx.add_cfg,
        &patch_opts,
        Some(reader),
    )?;
    // Refresh our in-memory index after add -p wrote it.
    ctx.index = ctx
        .repo
        .load_index_at(&ctx.index_path)
        .unwrap_or_else(|_| Index::new());
    Ok(())
}

/// `run_diff`: review the cached diff for selected paths.
fn run_diff(ctx: &mut AddIContext, out: &mut impl Write, reader: &mut impl BufRead) -> Result<()> {
    let files = get_modified_files(
        &ctx.repo.odb,
        &ctx.index,
        ctx.work_tree,
        ctx.head_tree.as_ref(),
        ModifiedFilter::IndexOnly,
    )?;
    if files.is_empty() {
        writeln!(out).ok();
        return Ok(());
    }
    let (selected, count) =
        list_and_choose(out, reader, &files, "Review diff ", ChooseFlags::Immediate);
    if count > 0 {
        let chosen: Vec<String> = files
            .iter()
            .enumerate()
            .filter(|(i, _)| selected[*i])
            .map(|(_, f)| f.name.clone())
            .collect();
        out.flush().ok();
        emit_cached_diff(ctx, out, &chosen)?;
    }
    writeln!(out).ok();
    Ok(())
}

/// Emit `git diff -p --cached <tree> -- <paths>` for the selected paths.
fn emit_cached_diff(ctx: &mut AddIContext, out: &mut impl Write, paths: &[String]) -> Result<()> {
    let entries = diff_index_to_tree(&ctx.repo.odb, &ctx.index, ctx.head_tree.as_ref(), false)?;
    let filtered: Vec<DiffEntry> = entries
        .into_iter()
        .filter(|e| paths.iter().any(|p| e.path() == p))
        .collect();
    let mut buf: Vec<u8> = Vec::new();
    crate::commands::diff::write_patch_from_pairs(&mut buf, &filtered, ctx.repo)
        .context("render cached diff for add -i")?;
    out.write_all(&buf).ok();
    Ok(())
}
