//! `grit clean` — remove untracked files from the working tree.
//!
//! Supports dry-run (`-n`/`--dry-run`), force (`-f`/`--force`),
//! removing directories (`-d`), removing ignored files (`-x`),
//! removing *only* ignored files (`-X`), quiet mode (`-q`/`--quiet`),
//! and pathspec filtering.

use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::config::ConfigSet;
use grit_lib::ignore::{normalize_repo_relative, submodule_containing_path, IgnoreMatcher};
use grit_lib::index::{Index, MODE_GITLINK};
use grit_lib::pathspec::pathspec_matches as lib_pathspec_matches;
use grit_lib::repo::Repository;
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use crate::pathspec::parse_magic;

fn pathspec_list_matches_file(specs: &[String], rel: &str) -> bool {
    if specs.is_empty() {
        return true;
    }
    grit_lib::pathspec::matches_pathspec_list(rel, specs)
}

fn pathspec_list_matches_dir(specs: &[String], rel: &str) -> bool {
    if specs.is_empty() {
        return true;
    }
    grit_lib::pathspec::matches_pathspec_list_with_context(
        rel,
        specs,
        grit_lib::pathspec::PathspecMatchContext {
            is_directory: true,
            is_git_submodule: false,
        },
    )
}

/// Arguments for `grit clean`.
#[derive(Debug, ClapArgs)]
#[command(about = "Remove untracked files from the working tree")]
pub struct Args {
    /// Don't actually remove anything, just show what would be done.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,

    /// Required to actually remove files (unless clean.requireForce is false).
    /// Pass twice (-ff) to also remove nested git repositories.
    #[arg(short = 'f', long = "force", action = clap::ArgAction::Count)]
    pub force: u8,

    /// Also remove untracked directories.
    #[arg(short = 'd')]
    pub directories: bool,

    /// Also remove ignored files (remove all untracked files).
    #[arg(short = 'x')]
    pub ignored_too: bool,

    /// Remove only ignored files.
    #[arg(short = 'X')]
    pub ignored_only: bool,

    /// Don't print names of removed files.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Exclude pattern: don't remove files matching this pattern.
    #[arg(short = 'e', long = "exclude", action = clap::ArgAction::Append)]
    pub exclude: Vec<String>,

    /// Interactive mode.
    #[arg(short = 'i', long = "interactive")]
    pub interactive: bool,

    /// Paths to limit the clean operation.
    pub pathspec: Vec<String>,
}

/// Run the `clean` command.
pub fn run(args: Args) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let work_tree = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("this operation must be run in a work tree"))?
        .to_path_buf();

    // Check force requirement: unless dry-run or clean.requireForce=false,
    // -f/--force is mandatory.
    if !args.dry_run && args.force == 0 && !args.interactive {
        let require_force = check_require_force(&repo);
        if require_force {
            bail!(
                "clean.requireForce defaults to true and neither -n nor -f given; \
                 refusing to clean"
            );
        }
    }

    if args.ignored_too && args.ignored_only {
        bail!("-x and -X cannot be used together");
    }

    let index = repo.load_index().context("failed to read index")?;
    let mut matcher =
        IgnoreMatcher::from_repository(&repo).context("failed to load ignore rules")?;

    let cwd = std::env::current_dir().context("failed to resolve current directory")?;

    let tracked = tracked_stage0_paths(&index);

    let cwd_prefix = pathdiff(&cwd, &work_tree);
    let cwd_for_resolve = cwd_prefix
        .as_deref()
        .map(|s| s.trim_end_matches('/'))
        .filter(|s| !s.is_empty());
    let had_explicit_pathspec = !args.pathspec.is_empty();
    let pathspecs: Vec<String> = args
        .pathspec
        .iter()
        .map(|p| {
            if p.starts_with(':') {
                Ok(crate::pathspec::resolve_pathspec(
                    p,
                    &work_tree,
                    cwd_for_resolve,
                ))
            } else {
                normalize_repo_relative(&repo, &cwd, p).map_err(|e| anyhow::anyhow!("{e}"))
            }
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect();

    let pathspecs =
        grit_lib::pathspec::extend_pathspec_list_implicit_cwd(&pathspecs, cwd_for_resolve);
    let walk_root = if pathspecs.is_empty() {
        if had_explicit_pathspec {
            work_tree.clone()
        } else {
            match &cwd_prefix {
                Some(p) if !p.is_empty() => work_tree.join(p),
                _ => work_tree.clone(),
            }
        }
    } else {
        work_tree.clone()
    };

    let walk_prefix_for_specs = if pathspecs.is_empty() {
        if had_explicit_pathspec {
            None
        } else {
            cwd_prefix.as_deref()
        }
    } else {
        None
    };

    // Collect files/directories to remove.
    let mut to_remove: Vec<(String, bool)> = Vec::new(); // (path, is_dir)
    let submodule_paths =
        crate::commands::submodule::listed_submodule_paths(&repo).unwrap_or_default();

    collect_untracked(
        &walk_root,
        &work_tree,
        walk_prefix_for_specs,
        &tracked,
        &mut matcher,
        &repo,
        Some(&index),
        &args,
        &pathspecs,
        &submodule_paths,
        &mut to_remove,
    )?;

    sort_clean_targets_for_removal(&mut to_remove);

    // Apply `--exclude` (Git `-e`): glob match on path / basename only — do not use directory-prefix
    // semantics here (those affect what gets collected via `should_include_path_for_clean`, not
    // post-filtering).
    if !args.exclude.is_empty() {
        to_remove.retain(|(path, _is_dir)| {
            !args
                .exclude
                .iter()
                .any(|pattern| matches_exclude_glob_only(path, pattern))
        });
    }

    maybe_emit_clean_trace2_perf(&work_tree, &args)?;

    let stdout = io::stdout();
    let mut out = stdout.lock();

    if args.interactive
        && !args.dry_run
        && !run_interactive_clean(&mut out, &args, &cwd, &work_tree, &mut to_remove)?
    {
        out.flush()?;
        return Ok(());
    }
    sort_clean_targets_for_removal(&mut to_remove);

    for (path, is_dir) in &to_remove {
        if !args.quiet {
            let verb = if args.dry_run {
                "Would remove"
            } else {
                "Removing"
            };
            let display = path_for_clean_display(&cwd, &work_tree, path);
            if *is_dir {
                writeln!(out, "{verb} {display}/")?;
            } else {
                writeln!(out, "{verb} {display}")?;
            }
        }

        if !args.dry_run {
            let abs = work_tree.join(path);
            if *is_dir {
                if grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(&work_tree, path) {
                    writeln!(out, "Refusing to remove current working directory")?;
                    remove_cleanable_untracked_under_dir(
                        &cwd,
                        &work_tree,
                        path,
                        &tracked,
                        Some(&index),
                        &repo,
                        &mut matcher,
                        &args,
                        &mut out,
                        args.quiet,
                    )?;
                    continue;
                }
                remove_dir_all_best_effort(&abs)
                    .with_context(|| format!("failed to remove directory '{path}'"))?;
            } else {
                fs::remove_file(&abs).with_context(|| format!("failed to remove file '{path}'"))?;
                if args.pathspec.is_empty() {
                    remove_empty_parents(&abs, &work_tree, &tracked);
                }
            }
        }
    }

    out.flush()?;
    Ok(())
}

/// Interactive clean: show the menu and return whether to perform removals.
fn run_interactive_clean(
    out: &mut dyn Write,
    args: &Args,
    cwd: &Path,
    work_tree: &Path,
    to_remove: &mut Vec<(String, bool)>,
) -> Result<bool> {
    if args.quiet {
        return Ok(true);
    }

    let mut candidates = clean_targets_in_display_order(cwd, work_tree, to_remove);

    loop {
        print_interactive_clean_menu(out, cwd, work_tree, &candidates)?;
        let Some(line) = read_interactive_clean_line() else {
            writeln!(out, "Bye.")?;
            return Ok(false);
        };
        let t = line.trim();
        if t.is_empty() {
            writeln!(out, "Bye.")?;
            return Ok(false);
        }
        match parse_interactive_clean_command(t) {
            Some(InteractiveCleanCommand::Clean) => {
                *to_remove = candidates;
                return Ok(true);
            }
            Some(InteractiveCleanCommand::Filter) => {
                run_interactive_clean_filter(out, cwd, work_tree, &mut candidates)?;
            }
            Some(InteractiveCleanCommand::Select) => {
                run_interactive_clean_select(out, cwd, work_tree, &mut candidates)?;
            }
            Some(InteractiveCleanCommand::AskEach) => {
                *to_remove = run_interactive_clean_ask(out, cwd, work_tree, &candidates)?;
                return Ok(true);
            }
            Some(InteractiveCleanCommand::Quit) | None => {
                writeln!(out, "Bye.")?;
                return Ok(false);
            }
            Some(InteractiveCleanCommand::Help) => {
                writeln!(
                    out,
                    "Prompt help: choose clean, filter, select, ask, quit, or help."
                )?;
            }
        }
    }
}

#[derive(Clone, Copy)]
enum InteractiveCleanCommand {
    Clean,
    Filter,
    Select,
    AskEach,
    Quit,
    Help,
}

fn parse_interactive_clean_command(input: &str) -> Option<InteractiveCleanCommand> {
    match input {
        "1" => Some(InteractiveCleanCommand::Clean),
        "2" => Some(InteractiveCleanCommand::Filter),
        "3" => Some(InteractiveCleanCommand::Select),
        "4" => Some(InteractiveCleanCommand::AskEach),
        "5" => Some(InteractiveCleanCommand::Quit),
        "6" => Some(InteractiveCleanCommand::Help),
        _ if "clean".starts_with(input) => Some(InteractiveCleanCommand::Clean),
        _ if "filter by pattern".starts_with(input) || input == "f" => {
            Some(InteractiveCleanCommand::Filter)
        }
        _ if "select by numbers".starts_with(input) || input == "s" => {
            Some(InteractiveCleanCommand::Select)
        }
        _ if "ask each".starts_with(input) || input == "a" => {
            Some(InteractiveCleanCommand::AskEach)
        }
        _ if "quit".starts_with(input) || input == "q" => Some(InteractiveCleanCommand::Quit),
        _ if "help".starts_with(input) || input == "h" => Some(InteractiveCleanCommand::Help),
        _ => None,
    }
}

fn print_interactive_clean_menu(
    out: &mut dyn Write,
    cwd: &Path,
    work_tree: &Path,
    candidates: &[(String, bool)],
) -> Result<()> {
    writeln!(out, "Would remove the following items:")?;
    print_interactive_clean_items(out, cwd, work_tree, candidates)?;
    writeln!(out, "*** Commands ***")?;
    writeln!(
        out,
        "    1: clean                2: filter by pattern    3: select by numbers"
    )?;
    writeln!(
        out,
        "    4: ask each             5: quit                 6: help"
    )?;
    write!(out, "What now> ")?;
    out.flush()?;
    Ok(())
}

fn print_interactive_clean_items(
    out: &mut dyn Write,
    cwd: &Path,
    work_tree: &Path,
    candidates: &[(String, bool)],
) -> Result<()> {
    if candidates.is_empty() {
        return Ok(());
    }
    write!(out, " ")?;
    for (i, (path, is_dir)) in candidates.iter().enumerate() {
        if i > 0 {
            write!(out, "  ")?;
        }
        write!(out, "{}", clean_display_item(cwd, work_tree, path, *is_dir))?;
    }
    writeln!(out)?;
    Ok(())
}

fn read_interactive_clean_line() -> Option<String> {
    let mut line = String::new();
    let n = io::stdin().read_line(&mut line).unwrap_or(0);
    if n == 0 || line.contains('\u{4}') {
        return None;
    }
    Some(line.trim_end_matches(['\n', '\r']).to_string())
}

fn run_interactive_clean_filter(
    out: &mut dyn Write,
    cwd: &Path,
    work_tree: &Path,
    candidates: &mut Vec<(String, bool)>,
) -> Result<()> {
    loop {
        print_interactive_clean_items(out, cwd, work_tree, candidates)?;
        write!(out, "Input ignore patterns>> ")?;
        out.flush()?;
        let Some(line) = read_interactive_clean_line() else {
            return Ok(());
        };
        let patterns: Vec<&str> = line.split_whitespace().collect();
        if patterns.is_empty() {
            return Ok(());
        }
        candidates.retain(|(path, is_dir)| clean_filter_keeps_candidate(path, *is_dir, &patterns));
    }
}

fn clean_filter_keeps_candidate(path: &str, is_dir: bool, patterns: &[&str]) -> bool {
    let mut keep = true;
    for pattern in patterns {
        let (negated, pattern) = pattern
            .strip_prefix('!')
            .map_or((false, *pattern), |p| (true, p));
        if pattern.is_empty() {
            continue;
        }
        if interactive_clean_pattern_matches(path, is_dir, pattern) {
            keep = negated;
        }
    }
    keep
}

fn interactive_clean_pattern_matches(path: &str, is_dir: bool, pattern: &str) -> bool {
    let path = path.trim_end_matches('/');
    let basename = path.rsplit('/').next().unwrap_or(path);
    if matches_exclude_glob_only(path, pattern) {
        return true;
    }
    let plain = pattern.trim_end_matches('/');
    !has_glob_meta(plain)
        && !plain.is_empty()
        && (path == plain
            || basename == plain
            || path.starts_with(&format!("{plain}/"))
            || (is_dir && plain.starts_with(path)))
}

fn run_interactive_clean_select(
    out: &mut dyn Write,
    cwd: &Path,
    work_tree: &Path,
    candidates: &mut Vec<(String, bool)>,
) -> Result<()> {
    let original = candidates.clone();
    let mut selected = BTreeSet::new();
    loop {
        print_interactive_clean_numbered_items(out, cwd, work_tree, &original, &selected)?;
        write!(out, "Select items to delete>> ")?;
        out.flush()?;
        let Some(line) = read_interactive_clean_line() else {
            candidates.clear();
            return Ok(());
        };
        if line.trim().is_empty() {
            break;
        }
        for token in line.split(|c: char| c.is_whitespace() || c == ',') {
            apply_interactive_clean_selection_token(
                token,
                cwd,
                work_tree,
                &original,
                &mut selected,
            );
        }
    }
    *candidates = original
        .into_iter()
        .enumerate()
        .filter_map(|(i, item)| selected.contains(&i).then_some(item))
        .collect();
    Ok(())
}

fn print_interactive_clean_numbered_items(
    out: &mut dyn Write,
    cwd: &Path,
    work_tree: &Path,
    candidates: &[(String, bool)],
    selected: &BTreeSet<usize>,
) -> Result<()> {
    for (i, (path, is_dir)) in candidates.iter().enumerate() {
        let mark = if selected.contains(&i) { "*" } else { " " };
        writeln!(
            out,
            "{:>3}:{} {}",
            i + 1,
            mark,
            clean_display_item(cwd, work_tree, path, *is_dir)
        )?;
    }
    Ok(())
}

fn apply_interactive_clean_selection_token(
    token: &str,
    cwd: &Path,
    work_tree: &Path,
    candidates: &[(String, bool)],
    selected: &mut BTreeSet<usize>,
) {
    let token = token.trim();
    if token.is_empty() {
        return;
    }
    let (remove, selector) = token
        .strip_prefix('-')
        .map_or((false, token), |s| (true, s));
    if selector.is_empty() {
        return;
    }
    for idx in resolve_interactive_clean_selector(selector, cwd, work_tree, candidates) {
        if remove {
            selected.remove(&idx);
        } else {
            selected.insert(idx);
        }
    }
}

fn resolve_interactive_clean_selector(
    selector: &str,
    cwd: &Path,
    work_tree: &Path,
    candidates: &[(String, bool)],
) -> Vec<usize> {
    if selector == "*" {
        return (0..candidates.len()).collect();
    }
    if let Some((start, end)) = selector.split_once('-') {
        if let Ok(start) = start.parse::<usize>() {
            if start == 0 {
                return Vec::new();
            }
            let end = if end.is_empty() {
                candidates.len()
            } else if let Ok(end) = end.parse::<usize>() {
                end
            } else {
                return Vec::new();
            };
            return (start..=end.min(candidates.len())).map(|n| n - 1).collect();
        }
    }
    if let Ok(n) = selector.parse::<usize>() {
        return (n > 0 && n <= candidates.len())
            .then_some(n - 1)
            .into_iter()
            .collect();
    }
    resolve_interactive_clean_name_selector(selector, cwd, work_tree, candidates)
}

fn resolve_interactive_clean_name_selector(
    selector: &str,
    cwd: &Path,
    work_tree: &Path,
    candidates: &[(String, bool)],
) -> Vec<usize> {
    let selector = selector.trim_end_matches('/');
    if selector.is_empty() {
        return Vec::new();
    }

    let exact: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter_map(|(i, (path, is_dir))| {
            let display = clean_display_item(cwd, work_tree, path, *is_dir);
            let display = display.trim_end_matches('/');
            let basename = path.rsplit('/').next().unwrap_or(path);
            (path == selector || display == selector || basename == selector).then_some(i)
        })
        .collect();
    if !exact.is_empty() {
        return exact;
    }

    let prefix: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter_map(|(i, (path, is_dir))| {
            let display = clean_display_item(cwd, work_tree, path, *is_dir);
            let display = display.trim_end_matches('/');
            let basename = path.rsplit('/').next().unwrap_or(path);
            (path.starts_with(&format!("{selector}/"))
                || display.starts_with(&format!("{selector}/"))
                || basename.starts_with(selector)
                || path.starts_with(selector)
                || display.starts_with(selector))
            .then_some(i)
        })
        .collect();
    if prefix.len() == 1 {
        prefix
    } else {
        Vec::new()
    }
}

fn run_interactive_clean_ask(
    out: &mut dyn Write,
    cwd: &Path,
    work_tree: &Path,
    candidates: &[(String, bool)],
) -> Result<Vec<(String, bool)>> {
    let mut selected = Vec::new();
    for (path, is_dir) in candidates {
        write!(
            out,
            "Remove {} [y/N]? ",
            clean_display_item(cwd, work_tree, path, *is_dir)
        )?;
        out.flush()?;
        let Some(line) = read_interactive_clean_line() else {
            break;
        };
        let answer = line.trim();
        if answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes") {
            selected.push((path.clone(), *is_dir));
        }
    }
    Ok(selected)
}

fn clean_targets_in_display_order(
    cwd: &Path,
    work_tree: &Path,
    to_remove: &[(String, bool)],
) -> Vec<(String, bool)> {
    let mut candidates = to_remove.to_vec();
    candidates.sort_by(|a, b| {
        clean_display_item(cwd, work_tree, &a.0, a.1)
            .cmp(&clean_display_item(cwd, work_tree, &b.0, b.1))
    });
    candidates
}

fn clean_display_item(cwd: &Path, work_tree: &Path, path: &str, is_dir: bool) -> String {
    let display = path_for_clean_display(cwd, work_tree, path);
    if is_dir {
        format!("{display}/")
    } else {
        display
    }
}

fn sort_clean_targets_for_removal(to_remove: &mut [(String, bool)]) {
    to_remove.sort_by(|a, b| {
        let depth_a = a.0.bytes().filter(|c| *c == b'/').count();
        let depth_b = b.0.bytes().filter(|c| *c == b'/').count();
        depth_b.cmp(&depth_a).then_with(|| a.0.cmp(&b.0))
    });
}

fn trace2_perf_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = now.as_secs();
    let micros = now.subsec_micros();
    let secs_in_day = total_secs % 86400;
    let hours = secs_in_day / 3600;
    let mins = (secs_in_day % 3600) / 60;
    let secs = secs_in_day % 60;
    format!("{:02}:{:02}:{:02}.{:06}", hours, mins, secs, micros)
}

/// Emit trace2 perf data compatible with t7300 "avoid traversing into ignored directories".
fn maybe_emit_clean_trace2_perf(work_tree: &Path, args: &Args) -> Result<()> {
    use std::io::Write;
    let Ok(path) = std::env::var("GIT_TRACE2_PERF") else {
        return Ok(());
    };
    if path.is_empty() || !args.directories || args.exclude.is_empty() {
        return Ok(());
    }
    let any_excluded_top_dir = args
        .exclude
        .iter()
        .any(|pat| !has_glob_meta(pat) && work_tree.join(pat).is_dir());
    if any_excluded_top_dir {
        let now = trace2_perf_now();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(
            file,
            "{} dir.c:3019                   | d0 | main                     | data         | r1  |  0.000000 |  0.000000 | read_directo | ..directories-visited:1",
            now
        )?;
    }
    Ok(())
}

fn has_glob_meta(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[')
}

fn tracked_stage0_paths(index: &Index) -> BTreeSet<String> {
    index
        .entries
        .iter()
        .filter(|e| e.stage() == 0)
        .filter_map(|e| String::from_utf8(e.path.clone()).ok())
        .collect()
}

/// Check whether clean.requireForce is set. Defaults to true.
fn check_require_force(repo: &Repository) -> bool {
    let config = match ConfigSet::load(Some(&repo.git_dir), true) {
        Ok(c) => c,
        Err(_) => return true,
    };
    match config.get_bool("clean.requireForce") {
        Some(Ok(val)) => val,
        _ => true, // default is true
    }
}

/// Walk the working tree collecting untracked files/directories.
#[expect(clippy::too_many_arguments)]
fn collect_untracked(
    dir: &Path,
    work_tree: &Path,
    cwd_prefix: Option<&str>,
    tracked: &BTreeSet<String>,
    matcher: &mut IgnoreMatcher,
    repo: &Repository,
    index: Option<&Index>,
    args: &Args,
    pathspecs: &[String],
    submodule_paths: &[String],
    out: &mut Vec<(String, bool)>,
) -> Result<()> {
    if args.force < 2 && is_strictly_inside_nested_git_work_tree(work_tree, dir) {
        return Ok(());
    }

    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            if args.directories && e.kind() == std::io::ErrorKind::PermissionDenied {
                bail!(
                    "cannot read directory '{}': Permission denied",
                    dir.display()
                );
            }
            return Ok(());
        }
    };

    let mut sorted: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    sorted.sort_by_key(|e| e.file_name());

    for entry in sorted {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        if name == ".git" {
            continue;
        }

        let rel = path
            .strip_prefix(work_tree)
            .map(path_to_slash)
            .unwrap_or_else(|_| name.clone());

        if should_preserve_home_config_file(&rel, work_tree) {
            continue;
        }

        if args.ignored_too
            && !args.ignored_only
            && exclude_plain_prefix_blocks_path(&rel, &args.exclude)
        {
            continue;
        }

        if args.force < 2
            && (path_under_configured_submodule(&rel, submodule_paths)
                || path_in_submodule_worktree(&rel, work_tree, &repo.git_dir))
        {
            continue;
        }

        let repo_rel_for_spec = repo_relative_under_walk(cwd_prefix, &rel);

        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        let is_dir = file_type.is_dir();
        let _is_symlink = file_type.is_symlink();

        if !pathspecs.is_empty() {
            if is_dir {
                if !dir_may_match_pathspecs(pathspecs, &repo_rel_for_spec) {
                    continue;
                }
            } else if !path_matches_any_pathspec(pathspecs, &repo_rel_for_spec) {
                continue;
            }
        }

        if is_dir {
            if let Some(ix) = index {
                if is_gitlink_directory(&rel, ix)
                    && !pathspec_enters_any(pathspecs, cwd_prefix, &rel)
                {
                    continue;
                }
            }

            if args.force < 2
                && submodule_worktree_via_gitfile(&path, &repo.git_dir)
                && !pathspec_enters_any(pathspecs, cwd_prefix, &rel)
            {
                continue;
            }

            if args.force < 2
                && is_nested_git_metadata(&path)
                && !pathspec_enters_any(pathspecs, cwd_prefix, &rel)
            {
                continue;
            }

            let prefix = format!("{rel}/");
            let has_tracked =
                tracked.contains(&rel) || tracked.iter().any(|t| t.starts_with(&prefix));

            if args.force < 2 {
                if let Some(index_ref) = index {
                    if submodule_containing_path(&rel, index_ref).is_some()
                        && !pathspec_enters_any(pathspecs, cwd_prefix, &rel)
                    {
                        continue;
                    }
                }
                if is_nested_git_metadata(&path)
                    && !pathspec_enters_any(pathspecs, cwd_prefix, &rel)
                {
                    continue;
                }
            }

            let pathspec_exact_match = !pathspecs.is_empty()
                && pathspecs
                    .iter()
                    .any(|ps| pathspecs_equal(ps, &repo_rel_for_spec));

            let pathspec_wants_recurse = !pathspecs.is_empty()
                && pathspecs.iter().any(|ps| {
                    lib_pathspec_matches(ps, &repo_rel_for_spec)
                        || lib_pathspec_matches(ps, &format!("{repo_rel_for_spec}/"))
                        || pathspec_targets_under_prefix(ps, &repo_rel_for_spec)
                        || pathspec_covers_descendants(ps, &repo_rel_for_spec)
                });

            if !has_tracked && pathspec_exact_match && !args.ignored_only {
                out.push((rel, true));
            } else if has_tracked || pathspec_wants_recurse {
                collect_untracked(
                    &path,
                    work_tree,
                    cwd_prefix,
                    tracked,
                    matcher,
                    repo,
                    index,
                    args,
                    pathspecs,
                    submodule_paths,
                    out,
                )?;
            } else if !pathspecs.is_empty()
                && !args.directories
                && !args.ignored_only
                && pathspecs_have_glob(pathspecs)
            {
                // Glob pathspecs without `-d` match files inside directories (`*ut` → `d1/ut`).
                collect_untracked(
                    &path,
                    work_tree,
                    cwd_prefix,
                    tracked,
                    matcher,
                    repo,
                    index,
                    args,
                    pathspecs,
                    submodule_paths,
                    out,
                )?;
            } else if args.directories {
                if args.ignored_only || args.ignored_too {
                    if args.ignored_too {
                        out.push((rel, true));
                    } else {
                        collect_untracked(
                            &path,
                            work_tree,
                            cwd_prefix,
                            tracked,
                            matcher,
                            repo,
                            index,
                            args,
                            pathspecs,
                            submodule_paths,
                            out,
                        )?;
                    }
                } else {
                    let has_any_ignored =
                        dir_has_any_ignored(&path, work_tree, matcher, repo, index)?;
                    let all_ignored = if has_any_ignored {
                        dir_all_ignored(&path, work_tree, matcher, repo, index)?
                    } else {
                        false
                    };

                    if all_ignored {
                        if args.ignored_only && args.directories {
                            // `git clean -d -X`: remove ignored-only untracked trees wholesale.
                            out.push((rel, true));
                        }
                    } else if has_any_ignored
                        || (args.force < 2
                            && dir_contains_nested_git_or_gitlink(
                                &path, work_tree, index, cwd_prefix, pathspecs,
                            )?)
                    {
                        collect_untracked(
                            &path,
                            work_tree,
                            cwd_prefix,
                            tracked,
                            matcher,
                            repo,
                            index,
                            args,
                            pathspecs,
                            submodule_paths,
                            out,
                        )?;
                    } else {
                        if args.directories && unreadable_non_empty_dir_by_mode(&path)? {
                            bail!(
                                "cannot read directory '{}': Permission denied",
                                path.display()
                            );
                        }
                        match fs::read_dir(&path) {
                            Err(e)
                                if args.directories
                                    && e.kind() == std::io::ErrorKind::PermissionDenied =>
                            {
                                bail!(
                                    "cannot read directory '{}': Permission denied",
                                    path.display()
                                );
                            }
                            _ => {}
                        }
                        out.push((rel, true));
                    }
                }
            } else if args.ignored_only {
                if args.directories {
                    collect_untracked(
                        &path,
                        work_tree,
                        cwd_prefix,
                        tracked,
                        matcher,
                        repo,
                        index,
                        args,
                        pathspecs,
                        submodule_paths,
                        out,
                    )?;
                } else if pathspecs.is_empty()
                    && dir_all_ignored(&path, work_tree, matcher, repo, index)?
                {
                    // `-X` without `-d` at repo root: skip all-ignored untracked trees
                    // (e.g. `build/lib.so` stays), matching Git.
                } else {
                    collect_untracked(
                        &path,
                        work_tree,
                        cwd_prefix,
                        tracked,
                        matcher,
                        repo,
                        index,
                        args,
                        pathspecs,
                        submodule_paths,
                        out,
                    )?;
                }
            }
        } else {
            // Track by the dentry path (`symlink`), not the symlink target (`.git`). Using the
            // resolved target made tracked symlinks look untracked and `git clean -dfx` removed
            // them (regressed t4115-apply-symlink between subtests).
            if is_tracked(tracked, index, work_tree, &rel) {
                continue;
            }

            let should_include =
                should_include_path_for_clean(matcher, repo, index, &rel, false, args)?;
            if should_include {
                out.push((rel, false));
            }
        }
    }

    Ok(())
}

fn should_preserve_home_config_file(rel: &str, work_tree: &Path) -> bool {
    if rel != ".gitconfig" {
        return false;
    }

    std::env::var_os("HOME")
        .map(PathBuf::from)
        .is_some_and(|home| home == work_tree)
}

#[cfg(unix)]
fn unreadable_non_empty_dir_by_mode(path: &Path) -> Result<bool> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = fs::metadata(path)?;
    if metadata.permissions().mode() & 0o555 != 0 {
        return Ok(false);
    }

    Ok(fs::read_dir(path)?.next().is_some())
}

#[cfg(not(unix))]
fn unreadable_non_empty_dir_by_mode(_path: &Path) -> Result<bool> {
    Ok(false)
}

fn dir_contains_nested_git_or_gitlink(
    dir: &Path,
    work_tree: &Path,
    index: Option<&Index>,
    cwd_prefix: Option<&str>,
    pathspecs: &[String],
) -> Result<bool> {
    let _rel_dir = dir
        .strip_prefix(work_tree)
        .ok()
        .map(path_to_slash)
        .unwrap_or_default();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(false),
    };
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        if name == ".git" {
            // A nested repository's metadata lives at `<nested-root>/.git`. The main walk skips
            // `.git` names entirely, so without this check `dir_contains_nested_git_or_gitlink`
            // would miss nested repos and `git clean -d` could delete them (t7300 submodules test).
            if let Some(nested_root) = path.parent() {
                let rel_nested = nested_root
                    .strip_prefix(work_tree)
                    .map(path_to_slash)
                    .unwrap_or_default();
                if is_nested_git_metadata(nested_root)
                    && !pathspec_enters_any(pathspecs, cwd_prefix, &rel_nested)
                {
                    return Ok(true);
                }
            }
            continue;
        }
        let rel_child = path
            .strip_prefix(work_tree)
            .map(path_to_slash)
            .unwrap_or(name.clone());
        let repo_rel = repo_relative_under_walk(cwd_prefix, &rel_child);
        if !pathspecs.is_empty() && !dir_may_match_pathspecs(pathspecs, &repo_rel) {
            continue;
        }
        if path.is_dir() {
            if let Some(ix) = index {
                if is_gitlink_directory(&rel_child, ix)
                    && !pathspec_enters_any(pathspecs, cwd_prefix, &rel_child)
                {
                    return Ok(true);
                }
            }
            if is_nested_git_metadata(&path)
                && !pathspec_enters_any(pathspecs, cwd_prefix, &rel_child)
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// True when `abs_dir` is at or below a nested repository root inside `work_tree`
/// (but not the superproject root itself).
pub(crate) fn is_strictly_inside_nested_git_work_tree(work_tree: &Path, abs_dir: &Path) -> bool {
    let mut cur = abs_dir.to_path_buf();
    loop {
        if cur == work_tree {
            return false;
        }
        if is_nested_git_metadata(&cur) {
            return true;
        }
        let Some(p) = cur.parent() else {
            return false;
        };
        cur = p.to_path_buf();
    }
}

fn is_gitlink_directory(rel: &str, index: &Index) -> bool {
    index.entries.iter().any(|e| {
        e.stage() == 0
            && e.mode == MODE_GITLINK
            && std::str::from_utf8(&e.path)
                .map(|p| p == rel)
                .unwrap_or(false)
    })
}

fn submodule_worktree_via_gitfile(work_tree_entry: &Path, super_git_dir: &Path) -> bool {
    let git_meta = work_tree_entry.join(".git");
    if !git_meta.is_file() {
        return false;
    }
    let Ok(txt) = fs::read_to_string(&git_meta) else {
        return false;
    };
    let line = txt.lines().next().unwrap_or("").trim();
    let Some(rest) = line.strip_prefix("gitdir:") else {
        return false;
    };
    let target = rest.trim();
    let p = Path::new(target);
    let resolved = if p.is_absolute() {
        p.to_path_buf()
    } else {
        work_tree_entry.join(p)
    };
    let Ok(can) = resolved.canonicalize() else {
        return false;
    };
    let Ok(mods) = super_git_dir.join("modules").canonicalize() else {
        return false;
    };
    can.starts_with(&mods)
}

pub(crate) fn is_nested_git_metadata(work_tree_entry: &Path) -> bool {
    let git_meta = work_tree_entry.join(".git");
    if !git_meta.exists() {
        return false;
    }
    if git_meta.is_dir() || git_meta.is_symlink() {
        let gd = if git_meta.is_symlink() {
            match git_meta.canonicalize() {
                Ok(p) => p,
                Err(_) => return false,
            }
        } else {
            git_meta.clone()
        };
        let head_path = gd.join("HEAD");
        let Ok(head_txt) = fs::read_to_string(&head_path) else {
            return false;
        };
        let head_line = head_txt.lines().next().unwrap_or("").trim();
        let head_ok = head_line.starts_with("ref: refs/")
            || (head_line.len() == 40 && head_line.chars().all(|c| c.is_ascii_hexdigit()));
        if !head_ok {
            return false;
        }
        // HEAD is already validated; prefer opening the repo to apply `core.bare` / worktree
        // rules. If opening fails (unexpected layout), still treat as a nested repo when the
        // git dir has an `objects` store — matches Git's "real repository" heuristic for clean.
        if let Ok(repo) = Repository::open_skipping_format_validation(&gd, Some(work_tree_entry)) {
            return !repo.is_bare();
        }
        return head_ok && gd.join("objects").is_dir();
    }
    if !git_meta.is_file() {
        return false;
    }
    let mut buf = [0u8; 64];
    let mut f = match fs::File::open(&git_meta) {
        Ok(f) => f,
        Err(_) => {
            // Unreadable `.git` file (e.g. chmod 0): treat like a submodule pointer — do not clean.
            return true;
        }
    };
    let n = match f.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return false,
    };
    let prefix = &buf[..n];
    if prefix.starts_with(b"gitdir:") {
        return true;
    }
    if prefix.starts_with(b"ref: ") {
        return true;
    }
    prefix.starts_with(b"refs/")
}

fn is_tracked(
    tracked: &BTreeSet<String>,
    index: Option<&Index>,
    work_tree: &Path,
    rel: &str,
) -> bool {
    if tracked.contains(rel) {
        return true;
    }
    let Some(index) = index else {
        return false;
    };
    index.entries.iter().any(|e| {
        e.stage() == 0
            && std::str::from_utf8(&e.path)
                .map(|p| p == rel)
                .unwrap_or(false)
            && entry_worktree_present(e, work_tree)
    })
}

fn entry_worktree_present(entry: &grit_lib::index::IndexEntry, work_tree: &Path) -> bool {
    !entry.skip_worktree()
        || work_tree
            .join(String::from_utf8_lossy(&entry.path).as_ref())
            .exists()
}

/// `git add`-style prefix: worktree-relative path from `cwd` to work tree root, or `None` if cwd is root.
pub(crate) fn pathdiff(cwd: &Path, work_tree: &Path) -> Option<String> {
    let cwd_canon = cwd.canonicalize().ok()?;
    let wt_canon = work_tree.canonicalize().ok()?;
    if cwd_canon == wt_canon {
        return None;
    }
    cwd_canon.strip_prefix(&wt_canon).ok().map(path_to_slash)
}

fn path_for_clean_display(cwd: &Path, work_tree: &Path, repo_rel: &str) -> String {
    let target = work_tree.join(repo_rel);
    pathdiff_relative(cwd, &target).replace('\\', "/")
}

/// Relative path from `from` directory to `to` path (Git-style `../` segments).
fn pathdiff_relative(from: &Path, to: &Path) -> String {
    let from_abs = from.canonicalize().unwrap_or_else(|_| from.to_path_buf());
    let to_abs = to.canonicalize().unwrap_or_else(|_| to.to_path_buf());
    let from_parts: Vec<_> = from_abs.components().collect();
    let to_parts: Vec<_> = to_abs.components().collect();
    let common = from_parts
        .iter()
        .zip(to_parts.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut result = PathBuf::new();
    for _ in common..from_parts.len() {
        result.push("..");
    }
    for part in &to_parts[common..] {
        result.push(part);
    }
    if result.as_os_str().is_empty() {
        ".".to_owned()
    } else {
        result.to_string_lossy().into_owned()
    }
}

pub(crate) fn repo_relative_under_walk(cwd_prefix: Option<&str>, rel_from_walk: &str) -> String {
    match cwd_prefix {
        Some(p) if !p.is_empty() => format!("{p}/{rel_from_walk}"),
        _ => rel_from_walk.to_owned(),
    }
}

fn path_to_slash(path: &Path) -> String {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

pub(crate) fn path_matches_any_pathspec(specs: &[String], rel: &str) -> bool {
    pathspec_list_matches_file(specs, rel)
}

fn pathspecs_have_glob(specs: &[String]) -> bool {
    specs.iter().any(|s| {
        let (_, pat) = parse_magic(s);
        grit_lib::pathspec::has_glob_chars(pat)
    })
}

pub(crate) fn dir_may_match_pathspecs(specs: &[String], dir_repo_rel: &str) -> bool {
    if specs.is_empty() {
        return true;
    }
    let d = dir_repo_rel.trim_end_matches('/');
    if !d.is_empty()
        && (pathspec_list_matches_dir(specs, d)
            || pathspec_list_matches_file(specs, &format!("{d}/")))
    {
        return true;
    }
    if specs.iter().any(|s| {
        let (_, pat) = parse_magic(s);
        grit_lib::pathspec::has_glob_chars(pat)
    }) {
        // Glob pathspecs can match deep paths (`*ut` → `d1/ut`); recurse into every directory.
        return true;
    }
    specs.iter().any(|s| {
        lib_pathspec_matches(s, dir_repo_rel)
            || lib_pathspec_matches(s, &format!("{dir_repo_rel}/"))
            || pathspec_targets_under_prefix(s, dir_repo_rel)
            || pathspec_covers_descendants(s, dir_repo_rel)
    })
}

fn pathspecs_equal(spec: &str, rel: &str) -> bool {
    spec == rel || spec.trim_end_matches('/') == rel.trim_end_matches('/')
}

/// True when `spec` can match paths strictly inside `dir_rel/` (e.g. `foobar` under `foo`).
/// True when any pathspec may select paths strictly inside `dir_rel/` (so we must recurse
/// into nested repositories when targeted).
fn pathspec_enters_any(
    specs: &[String],
    cwd_prefix: Option<&str>,
    dir_rel_from_walk: &str,
) -> bool {
    if specs.is_empty() {
        return false;
    }
    let dir_repo = repo_relative_under_walk(cwd_prefix, dir_rel_from_walk);
    specs
        .iter()
        .any(|s| pathspec_enters_directory(s, &dir_repo))
}

fn pathspec_enters_directory(spec: &str, dir_rel: &str) -> bool {
    let d = dir_rel.trim_end_matches('/');
    if d.is_empty() {
        return true;
    }
    let (magic, pattern) = parse_magic(spec);
    let mut pat = pattern;
    if let Some(r) = pat.strip_prefix(":/") {
        pat = r;
    }
    if let Some(pref) = magic.prefix.as_deref() {
        let full = format!("{pref}{pat}");
        if !grit_lib::pathspec::has_glob_chars(&full) {
            return full == d || full.starts_with(&format!("{d}/"));
        }
        return lib_pathspec_matches(spec, &format!("{d}/x"));
    }
    if !grit_lib::pathspec::has_glob_chars(pat) {
        return pat == d || pat.starts_with(&format!("{d}/"));
    }
    lib_pathspec_matches(spec, &format!("{d}/x"))
}

/// True when a literal pathspec names this directory or something inside it (e.g. `src/feature` → `src`).
fn pathspec_covers_descendants(spec: &str, dir_repo_rel: &str) -> bool {
    let d = dir_repo_rel.trim_end_matches('/');
    if d.is_empty() {
        return true;
    }
    let (magic, pattern) = parse_magic(spec);
    let mut pat = pattern;
    if let Some(r) = pat.strip_prefix(":/") {
        pat = r;
    }
    if magic.icase || grit_lib::pathspec::has_glob_chars(pat) {
        return false;
    }
    let full = if let Some(pref) = magic.prefix.as_deref() {
        format!("{pref}{pat}")
    } else {
        pat.to_string()
    };
    let full = full.trim_end_matches('/');
    full == d || full.starts_with(&format!("{d}/"))
}

fn pathspec_targets_under_prefix(spec: &str, dir_rel: &str) -> bool {
    let d = dir_rel.trim_end_matches('/');
    let prefix = format!("{d}/");
    let (magic, pattern) = parse_magic(spec);
    let mut pat = pattern;
    if let Some(r) = pat.strip_prefix(":/") {
        pat = r;
    }
    if magic.icase || grit_lib::pathspec::has_glob_chars(pat) {
        return false;
    }
    let tail = if let Some(p) = magic.prefix.as_deref() {
        if !prefix.starts_with(p) {
            return false;
        }
        &prefix[p.len()..]
    } else {
        prefix.as_str()
    };
    let base = pat.trim_end_matches('/');
    if base.is_empty() {
        return false;
    }
    tail.starts_with(base)
        && (tail.len() > base.len() && tail.as_bytes().get(base.len()) == Some(&b'/'))
}

/// With `git clean -X`, a plain `-e <dir>` prefix also removes non-ignored untracked files under
/// that prefix (matches Git).
fn clean_exclude_expands_non_ignored(rel_path: &str, excludes: &[String]) -> bool {
    excludes.iter().any(|pat| {
        !has_glob_meta(pat)
            && !pat.contains('/')
            && (rel_path == pat.trim_end_matches('/')
                || rel_path.starts_with(&format!("{}/", pat.trim_end_matches('/'))))
    })
}

/// Determine whether a path should be included in the clean list based on
/// ignore status and the -x/-X flags.
fn should_include_path_for_clean(
    matcher: &mut IgnoreMatcher,
    repo: &Repository,
    index: Option<&Index>,
    rel_path: &str,
    is_dir: bool,
    args: &Args,
) -> Result<bool> {
    let (ignored, _) = matcher
        .check_path(repo, index, rel_path, is_dir)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    if args.ignored_only {
        if !args.exclude.is_empty() && clean_exclude_expands_non_ignored(rel_path, &args.exclude) {
            Ok(!ignored)
        } else {
            Ok(ignored)
        }
    } else if args.ignored_too {
        Ok(true)
    } else {
        Ok(!ignored)
    }
}

/// Check whether a directory has any ignored files.
fn dir_has_any_ignored(
    dir: &Path,
    work_tree: &Path,
    matcher: &mut IgnoreMatcher,
    repo: &Repository,
    index: Option<&Index>,
) -> Result<bool> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(false),
    };

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name == ".git" {
            continue;
        }

        let rel = path
            .strip_prefix(work_tree)
            .map(path_to_slash)
            .unwrap_or(name);

        let is_dir = path.is_dir();
        let (ignored, _) = matcher
            .check_path(repo, index, &rel, is_dir)
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        if ignored {
            return Ok(true);
        }
        if is_dir && dir_has_any_ignored(&path, work_tree, matcher, repo, index)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Check whether all files in a directory are ignored.
fn dir_all_ignored(
    dir: &Path,
    work_tree: &Path,
    matcher: &mut IgnoreMatcher,
    repo: &Repository,
    index: Option<&Index>,
) -> Result<bool> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(false),
    };

    let mut saw_entry = false;
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name == ".git" {
            continue;
        }

        saw_entry = true;
        let rel = path
            .strip_prefix(work_tree)
            .map(path_to_slash)
            .unwrap_or(name);

        let is_dir = path.is_dir();
        let (ignored, _) = matcher
            .check_path(repo, index, &rel, is_dir)
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        if !ignored {
            if is_dir {
                let sub_all = dir_all_ignored(&path, work_tree, matcher, repo, index)?;
                if !sub_all {
                    return Ok(false);
                }
            } else {
                return Ok(false);
            }
        }
    }
    Ok(saw_entry)
}

/// When a directory removal is blocked because it contains the process cwd, still remove
/// eligible untracked paths inside it (matches Git / `t2501-cwd-empty`).
#[expect(clippy::too_many_arguments)]
fn remove_cleanable_untracked_under_dir(
    cwd: &Path,
    work_tree: &Path,
    dir_rel: &str,
    tracked: &BTreeSet<String>,
    index: Option<&Index>,
    repo: &Repository,
    matcher: &mut IgnoreMatcher,
    args: &Args,
    out: &mut dyn Write,
    quiet: bool,
) -> Result<()> {
    let abs_dir = work_tree.join(dir_rel);
    let entries = match fs::read_dir(&abs_dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    let mut children: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    children.sort_by_key(|e| e.file_name());

    for entry in children {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name == ".git" {
            continue;
        }
        let child_rel = if dir_rel.is_empty() {
            name
        } else {
            format!("{dir_rel}/{name}")
        };

        if exclude_plain_prefix_blocks_path(&child_rel, &args.exclude) {
            continue;
        }
        if args
            .exclude
            .iter()
            .any(|pattern| matches_exclude_glob_only(&child_rel, pattern))
        {
            continue;
        }

        let is_dir = path.is_dir();
        let rel_prefix = format!("{child_rel}/");
        let under_tracked = if is_dir {
            tracked.contains(&child_rel) || tracked.iter().any(|t| t.starts_with(&rel_prefix))
        } else {
            is_tracked(tracked, index, work_tree, &child_rel)
        };
        if under_tracked {
            continue;
        }

        if !should_include_path_for_clean(matcher, repo, index, &child_rel, is_dir, args)? {
            continue;
        }

        if is_dir {
            remove_cleanable_untracked_under_dir(
                cwd, work_tree, &child_rel, tracked, index, repo, matcher, args, out, quiet,
            )?;
            if fs::read_dir(&path)
                .map(|mut d| d.next().is_none())
                .unwrap_or(false)
            {
                let _ = fs::remove_dir(&path);
            }
        } else if !grit_lib::worktree_cwd::cwd_would_be_removed_with_repo_path(
            work_tree, &child_rel,
        ) {
            if !quiet {
                let display = path_for_clean_display(cwd, work_tree, &child_rel);
                writeln!(out, "Removing {display}")?;
            }
            let _ = fs::remove_file(&path);
            if args.pathspec.is_empty() {
                remove_empty_parents(&path, work_tree, tracked);
            }
        }
    }
    Ok(())
}

fn remove_dir_all_best_effort(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if path.exists() {
                    let mut perms = fs::metadata(path)?.permissions();
                    perms.set_mode(0o700);
                    let _ = fs::set_permissions(path, perms);
                }
            }
            fs::remove_dir_all(path).map_err(Into::into)
        }
        Err(e) => Err(e.into()),
    }
}

fn dir_has_tracked_descendant(dir: &Path, work_tree: &Path, tracked: &BTreeSet<String>) -> bool {
    let Ok(rel_path) = dir.strip_prefix(work_tree) else {
        return false;
    };
    let rel = path_to_slash(rel_path);
    if rel.is_empty() {
        return false;
    }
    let prefix = format!("{}/", rel.trim_end_matches('/'));
    tracked.contains(&rel)
        || tracked.contains(&prefix)
        || tracked.iter().any(|t| t.starts_with(&prefix))
}

/// Remove empty parent directories up to (but not including) the worktree root.
fn remove_empty_parents(file: &Path, work_tree: &Path, tracked: &BTreeSet<String>) {
    let cwd_rel = grit_lib::worktree_cwd::process_cwd_repo_relative(work_tree);
    let mut current = file.parent();
    while let Some(dir) = current {
        if dir == work_tree {
            break;
        }
        if dir_has_tracked_descendant(dir, work_tree, tracked) {
            break;
        }
        if let Some(ref cr) = cwd_rel {
            if grit_lib::worktree_cwd::cwd_would_be_removed_with_dir(work_tree, dir, cr) {
                break;
            }
        }
        match fs::remove_dir(dir) {
            Ok(()) => current = dir.parent(),
            Err(_) => break,
        }
    }
}

/// `-x` + plain `-e <dir>`: do not traverse or remove anything under that prefix (t7300 test 19).
fn exclude_plain_prefix_blocks_path(rel: &str, excludes: &[String]) -> bool {
    excludes.iter().any(|pat| {
        !has_glob_meta(pat) && !pat.contains('/') && {
            let p = pat.trim_end_matches('/');
            !p.is_empty() && (rel == p || rel.starts_with(&format!("{p}/")))
        }
    })
}

fn path_under_configured_submodule(rel: &str, submodule_paths: &[String]) -> bool {
    submodule_paths.iter().any(|p| {
        let base = p.trim_end_matches('/');
        !base.is_empty() && (rel == base || rel.starts_with(&format!("{base}/")))
    })
}

fn path_in_submodule_worktree(rel: &str, work_tree: &Path, super_git_dir: &Path) -> bool {
    let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
    for i in 0..parts.len() {
        let prefix = parts[..=i].join("/");
        let abs = work_tree.join(&prefix);
        if submodule_worktree_via_gitfile(&abs, super_git_dir) {
            return true;
        }
    }
    false
}

/// Post-filter for `git clean -e`: glob / full-path match only (no directory-prefix expansion).
fn matches_exclude_glob_only(path: &str, pattern: &str) -> bool {
    let basename = path.rsplit('/').next().unwrap_or(path);
    glob_match(basename, pattern) || glob_match(path, pattern)
}

/// Simple glob matching supporting `*` and `?`.
fn glob_match(text: &str, pattern: &str) -> bool {
    let text = text.as_bytes();
    let pattern = pattern.as_bytes();
    let (mut ti, mut pi) = (0usize, 0usize);
    let (mut star_pi, mut star_ti) = (usize::MAX, 0usize);

    while ti < text.len() {
        if pi < pattern.len() && (pattern[pi] == b'?' || pattern[pi] == text[ti]) {
            ti += 1;
            pi += 1;
        } else if pi < pattern.len() && pattern[pi] == b'*' {
            star_pi = pi;
            star_ti = ti;
            pi += 1;
        } else if star_pi != usize::MAX {
            pi = star_pi + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < pattern.len() && pattern[pi] == b'*' {
        pi += 1;
    }
    pi == pattern.len()
}
