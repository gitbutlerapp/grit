//! `grit diff-files` command.
//!
//! Compares the working tree against the index.  This is the plumbing
//! equivalent of `grit diff` (without `--cached`).

use crate::commands::diff::{
    diff_emit_unified_patch_from_plumbing_argv, parse_diff_files_format_argv,
    resolve_dirstat_options_from_cli, write_dirstat, DiffFilesEmitKind, DiffFilesStatVariant,
};
use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::config::ConfigSet;
use grit_lib::diff::{
    count_changes, detect_copies, empty_blob_oid, format_stat_line,
    normalize_ignore_space_change_line, parse_indent_heuristic_cli_flags, resolve_indent_heuristic,
    rewrite_dissimilarity_index_percent, should_break_rewrite_for_stat, stat_matches,
    submodule_porcelain_flags, unified_diff, zero_oid, DiffEntry, DiffStatus,
};
use grit_lib::repo_path::{RepoPath, RepoPathBuf};
use grit_lib::diffstat::{terminal_columns, write_diffstat_block, DiffstatOptions, FileStatInput};
use grit_lib::index::{
    Index, IndexEntry, MODE_EXECUTABLE, MODE_GITLINK, MODE_REGULAR, MODE_SYMLINK,
};
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::repo::Repository;
use grit_lib::rev_parse::abbreviate_object_id;
#[cfg(unix)]
use libc;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

// ── Public clap interface ────────────────────────────────────────────

/// Arguments for `grit diff-files`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Raw command arguments forwarded by the CLI parser.
    #[arg(value_name = "ARG", num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    pub args: Vec<String>,
}

/// Run `grit diff-files`.
pub fn run(mut args: Args) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    if grit_lib::precompose_config::effective_core_precomposeunicode(Some(&repo.git_dir)) {
        crate::precompose::precompose_plumbing_argv(&mut args.args);
    }
    let mut options = parse_options(&args.args)?;
    let diff_cfg = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let (cli_ind, cli_no) = parse_indent_heuristic_cli_flags(&args.args);
    options.indent_heuristic = resolve_indent_heuristic(&diff_cfg, cli_ind, cli_no);

    let Some(work_tree) = repo.work_tree.clone() else {
        bail!("this operation must be run in a work tree");
    };
    prefix_relative_pathspecs_from_cwd(&work_tree, &mut options.pathspecs);

    let index_path = effective_index_path(&repo)?;
    let index = repo.load_index_at(&index_path).context("loading index")?;
    let index_mtime = index_file_mtime_pair(&index_path);

    let changes = collect_changes(&repo, &index, &work_tree, &options, index_mtime)?;

    let mut diff_entries: Vec<DiffEntry> = changes.iter().map(change_to_diff_entry).collect();

    if options.break_rewrites {
        for entry in &mut diff_entries {
            if entry.status != DiffStatus::Modified {
                continue;
            }
            let old_raw = if entry.old_oid == zero_oid() {
                Vec::new()
            } else {
                match repo.odb.read(&entry.old_oid) {
                    Ok(obj) => obj.data,
                    Err(_) => continue,
                }
            };
            let new_raw = if entry.new_oid == zero_oid() {
                Vec::new()
            } else {
                let path = entry.new_path.as_deref().unwrap_or(entry.path());
                let abs = path.to_fs_path(&work_tree);
                match fs::read(&abs) {
                    Ok(b) => b,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                    Err(_) => continue,
                }
            };
            if should_break_rewrite_for_stat(&old_raw, &new_raw) {
                if let Some(pct) = rewrite_dissimilarity_index_percent(&old_raw, &new_raw) {
                    entry.score = Some(pct);
                }
            }
        }
    }

    if options.reverse {
        diff_entries = diff_entries
            .into_iter()
            .map(reverse_diff_entry_for_diff_files)
            .collect();
    }

    if options.find_copies {
        let threshold = options.find_renames.unwrap_or(50);
        let source_index_entries: Vec<(String, String, ObjectId)> = index
            .entries
            .iter()
            .filter(|e| e.stage() == 0)
            .filter_map(|e| {
                let path = String::from_utf8(e.path.clone()).ok()?;
                if options.ignore_submodules && e.mode == MODE_GITLINK {
                    return None;
                }
                if !matches_pathspec(&path, &options.pathspecs) {
                    return None;
                }
                let mode = format!("{:06o}", canonicalize_mode(e.mode));
                Some((path, mode, e.oid))
            })
            .collect();
        diff_entries = detect_copies(
            &repo.odb,
            None,
            diff_entries,
            threshold,
            options.find_copies_harder,
            &source_index_entries,
        );
    } else if let Some(threshold) = options.find_renames {
        diff_entries = grit_lib::diff::detect_renames(&repo.odb, None, diff_entries, threshold);
    }

    diff_entries =
        filter_diff_files_whitespace_equivalent(diff_entries, &repo, &work_tree, &options)?;

    let emit_patch =
        diff_emit_unified_patch_from_plumbing_argv("diff-files", &env::args().collect::<Vec<_>>());
    let dirstat_cli_active =
        !options.dirstat_cli.dirstat.is_empty() || options.dirstat_cli.dirstat_by_file.is_some();
    let (dirstat_opts, dirstat_warnings) =
        resolve_dirstat_options_from_cli(&options.dirstat_cli, &repo.git_dir, dirstat_cli_active)?;
    for w in &dirstat_warnings {
        eprintln!("warning: {w}");
    }

    let use_emit_queue = !options.emit_queue.is_empty();

    if !options.quiet {
        let mut wrote_any = false;
        let mut need_blank_before_patch = false;

        if use_emit_queue {
            let mut out = std::io::stdout().lock();
            for kind in &options.emit_queue {
                match kind {
                    DiffFilesEmitKind::Raw => {
                        if options.suppress_diff {
                            continue;
                        }
                        for entry in &diff_entries {
                            writeln!(
                                out,
                                "{}",
                                render_raw_diff_entry(
                                    entry,
                                    &repo,
                                    options.abbrev,
                                    options.reverse
                                )?
                            )?;
                        }
                        wrote_any = true;
                        need_blank_before_patch = true;
                    }
                    DiffFilesEmitKind::Stat => {
                        if options.suppress_diff {
                            continue;
                        }
                        if options.stat_variant == DiffFilesStatVariant::CompactSummary {
                            print_compact_summary_from_diff_entries(
                                &diff_entries,
                                &repo,
                                &work_tree,
                            )?;
                        } else {
                            print_stat_from_diff_entries(
                                &diff_entries,
                                &repo,
                                &work_tree,
                                &options,
                            )?;
                        }
                        wrote_any = true;
                        need_blank_before_patch = true;
                    }
                    DiffFilesEmitKind::NumStat => {
                        if options.suppress_diff {
                            continue;
                        }
                        print_numstat_from_diff_entries(
                            &diff_entries,
                            &repo,
                            &work_tree,
                            &options,
                        )?;
                        wrote_any = true;
                        need_blank_before_patch = true;
                    }
                    DiffFilesEmitKind::Shortstat => {
                        if options.suppress_diff {
                            continue;
                        }
                        write_diff_files_shortstat_line(&diff_entries, &repo, &work_tree)?;
                        wrote_any = true;
                        need_blank_before_patch = true;
                    }
                    DiffFilesEmitKind::Dirstat => {
                        if options.suppress_diff {
                            continue;
                        }
                        if let Some(ref ds) = dirstat_opts {
                            write_dirstat(
                                &mut out,
                                ds,
                                &diff_entries,
                                &repo.odb,
                                Some(work_tree.as_path()),
                                options.break_rewrites,
                            )?;
                        }
                        wrote_any = true;
                        need_blank_before_patch = true;
                    }
                    DiffFilesEmitKind::Summary => {
                        print_diff_files_summary(&diff_entries)?;
                        wrote_any = true;
                        need_blank_before_patch = true;
                    }
                }
            }
            drop(out);

            let show_patch = emit_patch && !options.suppress_diff;
            if show_patch {
                let prefix_raw = options.patch_with_raw
                    && !options
                        .emit_queue
                        .iter()
                        .any(|k| *k == DiffFilesEmitKind::Raw);
                let prefix_stat = options.patch_with_stat
                    && options.stat_variant != DiffFilesStatVariant::CompactSummary
                    && !options
                        .emit_queue
                        .iter()
                        .any(|k| *k == DiffFilesEmitKind::Stat);
                if prefix_raw {
                    for entry in &diff_entries {
                        println!(
                            "{}",
                            render_raw_diff_entry(entry, &repo, options.abbrev, options.reverse)?
                        );
                    }
                    wrote_any = true;
                    need_blank_before_patch = true;
                }
                if prefix_stat {
                    print_stat_from_diff_entries(&diff_entries, &repo, &work_tree, &options)?;
                    wrote_any = true;
                    need_blank_before_patch = true;
                }
                if need_blank_before_patch && wrote_any {
                    println!();
                }
                for entry in &diff_entries {
                    print_patch_from_diff_entry(
                        entry,
                        &repo,
                        &work_tree,
                        &options,
                        options.abbrev,
                        options.indent_heuristic,
                        options.no_prefix,
                    )?;
                }
            }
        } else {
            if options
                .emit_queue
                .iter()
                .any(|k| *k == DiffFilesEmitKind::Summary)
            {
                print_diff_files_summary(&diff_entries)?;
                wrote_any = true;
                need_blank_before_patch = true;
            }
            if !options.suppress_diff {
                match options.format {
                    OutputFormat::Raw => {
                        let summary_only = options
                            .emit_queue
                            .iter()
                            .any(|k| *k == DiffFilesEmitKind::Summary);
                        if !(summary_only && !options.explicit_raw) {
                            for entry in &diff_entries {
                                println!(
                                    "{}",
                                    render_raw_diff_entry(
                                        entry,
                                        &repo,
                                        options.abbrev,
                                        options.reverse
                                    )?
                                );
                            }
                        }
                    }
                    OutputFormat::NameOnly => {
                        for entry in &diff_entries {
                            println!("{}", entry.path());
                        }
                    }
                    OutputFormat::NameStatus => {
                        for entry in &diff_entries {
                            match (entry.status, entry.score) {
                                (DiffStatus::Renamed, Some(s)) => {
                                    println!(
                                        "R{s:03}\t{}\t{}",
                                        entry.old_path.as_deref().unwrap_or(RepoPath::from_str("")),
                                        entry.new_path.as_deref().unwrap_or(RepoPath::from_str(""))
                                    );
                                }
                                (DiffStatus::Copied, Some(s)) => {
                                    println!(
                                        "C{s:03}\t{}\t{}",
                                        entry.old_path.as_deref().unwrap_or(RepoPath::from_str("")),
                                        entry.new_path.as_deref().unwrap_or(RepoPath::from_str(""))
                                    );
                                }
                                _ => {
                                    println!("{}\t{}", entry.status.letter(), entry.path());
                                }
                            }
                        }
                    }
                    OutputFormat::Patch => {
                        if options.patch_with_raw {
                            for entry in &diff_entries {
                                println!(
                                    "{}",
                                    render_raw_diff_entry(
                                        entry,
                                        &repo,
                                        options.abbrev,
                                        options.reverse
                                    )?
                                );
                            }
                            wrote_any = true;
                            need_blank_before_patch = true;
                        }
                        if options.patch_with_stat {
                            print_stat_from_diff_entries(
                                &diff_entries,
                                &repo,
                                &work_tree,
                                &options,
                            )?;
                            wrote_any = true;
                            need_blank_before_patch = true;
                        }
                        if need_blank_before_patch && wrote_any {
                            println!();
                        }
                        for entry in &diff_entries {
                            print_patch_from_diff_entry(
                                entry,
                                &repo,
                                &work_tree,
                                &options,
                                options.abbrev,
                                options.indent_heuristic,
                                options.no_prefix,
                            )?;
                        }
                    }
                    OutputFormat::Stat => {
                        print_stat_from_diff_entries(&diff_entries, &repo, &work_tree, &options)?;
                    }
                    OutputFormat::NumStat => {
                        print_numstat_from_diff_entries(
                            &diff_entries,
                            &repo,
                            &work_tree,
                            &options,
                        )?;
                    }
                }
            } else if options.format == OutputFormat::Raw && options.explicit_raw {
                for entry in &diff_entries {
                    println!(
                        "{}",
                        render_raw_diff_entry(entry, &repo, options.abbrev, options.reverse)?
                    );
                }
            }
        }
    }

    if (options.exit_code || options.quiet) && !diff_entries.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

fn quote_c_style_path(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    let mut needs_quotes = false;
    for ch in name.chars() {
        match ch {
            '"' => {
                out.push_str("\\\"");
                needs_quotes = true;
            }
            '\\' => {
                out.push_str("\\\\");
                needs_quotes = true;
            }
            '\t' => {
                out.push_str("\\t");
                needs_quotes = true;
            }
            '\n' => {
                out.push_str("\\n");
                needs_quotes = true;
            }
            '\r' => {
                out.push_str("\\r");
                needs_quotes = true;
            }
            c if c.is_control() => {
                out.push_str(&format!("\\{:03o}", u32::from(c)));
                needs_quotes = true;
            }
            c => out.push(c),
        }
    }
    if needs_quotes {
        format!("\"{out}\"")
    } else {
        out
    }
}

fn format_rename_summary_display(old: &str, new: &str) -> String {
    let pretty = grit_lib::diff::format_rename_path(old, new);
    quote_c_style_path(&pretty)
}

/// Emit `git diff-files --summary` lines (rewrites, deletes, mode changes, renames/copies).
fn print_diff_files_summary(entries: &[DiffEntry]) -> Result<()> {
    for entry in entries {
        match entry.status {
            DiffStatus::Renamed => {
                let old = entry.old_path.as_deref().unwrap_or(RepoPath::from_str(""));
                let new = entry.new_path.as_deref().unwrap_or(RepoPath::from_str(""));
                let display =
                    format_rename_summary_display(&old.to_string_lossy(), &new.to_string_lossy());
                let sim = entry.score.unwrap_or(100);
                println!(" rename {display} ({sim}%)");
            }
            DiffStatus::Copied => {
                let old = entry.old_path.as_deref().unwrap_or(RepoPath::from_str(""));
                let new = entry.new_path.as_deref().unwrap_or(RepoPath::from_str(""));
                let display =
                    format_rename_summary_display(&old.to_string_lossy(), &new.to_string_lossy());
                let sim = entry.score.unwrap_or(100);
                println!(" copy {display} ({sim}%)");
            }
            DiffStatus::Added => {
                println!(
                    " create mode {} {}",
                    entry.new_mode,
                    quote_c_style_path(&entry.path().to_string_lossy())
                );
            }
            DiffStatus::Deleted => {
                println!(
                    " delete mode {} {}",
                    entry.old_mode,
                    quote_c_style_path(&entry.path().to_string_lossy())
                );
            }
            DiffStatus::TypeChanged => {
                println!(
                    " mode change {} => {} {}",
                    entry.old_mode,
                    entry.new_mode,
                    quote_c_style_path(&entry.path().to_string_lossy())
                );
            }
            DiffStatus::Modified => {
                if entry.old_mode != entry.new_mode && entry.old_oid == entry.new_oid {
                    println!(
                        " mode change {} => {} {}",
                        entry.old_mode,
                        entry.new_mode,
                        quote_c_style_path(&entry.path().to_string_lossy())
                    );
                }
                if let Some(pct) = entry.score {
                    println!(" rewrite {} ({pct}%)", quote_c_style_path(&entry.path().to_string_lossy()));
                }
            }
            DiffStatus::Unmerged => {}
        }
    }
    Ok(())
}

// ── Internal types ───────────────────────────────────────────────────

/// Output format for `diff-files`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputFormat {
    /// `:old-mode new-mode old-oid new-oid status\tpath` (default).
    Raw,
    /// Unified patch output.
    Patch,
    /// Diff stat summary.
    Stat,
    /// Numeric stat (NUL-line-terminated counts).
    NumStat,
    /// File names only.
    NameOnly,
    /// Status letter + tab + file name.
    NameStatus,
}

/// Parsed command-line options.
#[derive(Debug, Clone)]
struct Options {
    /// Paths to limit output to; empty means all paths.
    pathspecs: Vec<String>,
    /// Merge stage to diff against (0 = normal, 1–3 = unmerged stage).
    stage: u8,
    /// Suppress all output; exit 1 if any difference.
    quiet: bool,
    /// Exit 1 if differences, regardless of output format.
    exit_code: bool,
    /// Abbreviate OIDs to this many hex digits.
    abbrev: Option<usize>,
    /// Chosen output format.
    format: OutputFormat,
    /// True only when `--raw` was passed (default format is raw but must be suppressed with `--summary`).
    explicit_raw: bool,
    /// Suppress diff output (-s / --no-patch).
    suppress_diff: bool,
    /// Emit `--stat` or `--compact-summary` block (`--compact-summary` wins over `--stat`).
    stat_variant: DiffFilesStatVariant,
    /// Prefix patch with full `--stat` block (`---` separator before hunks).
    patch_with_stat: bool,
    /// Prefix patch with `--raw` lines.
    patch_with_raw: bool,
    /// `git diff-files` output format queue (order-preserving duplicate flags).
    emit_queue: Vec<DiffFilesEmitKind>,
    /// Parsed `--dirstat` / `--dirstat-by-file` / `--cumulative`.
    dirstat_cli: crate::commands::diff::DirstatCliState,
    /// Optional diff-filter specification.
    diff_filter: Option<String>,
    /// Omit submodule entries (gitlinks) from the diff.
    ignore_submodules: bool,
    /// Rename similarity threshold (percent); `None` disables rename detection.
    find_renames: Option<u32>,
    /// Enable copy detection (`-C` / `--find-copies`).
    find_copies: bool,
    /// Consider unmodified index entries as copy sources (`--find-copies-harder`).
    find_copies_harder: bool,
    /// Swap old/new sides (reverse diff).
    reverse: bool,
    /// Detect complete rewrites (`-B` / `--break-rewrites`) for summary/raw dissimilarity.
    break_rewrites: bool,
    indent_heuristic: bool,
    /// `--no-prefix`: omit the `a/`/`b/` path prefixes in patch output.
    no_prefix: bool,
    ignore_all_space: bool,
    ignore_space_change: bool,
    ignore_space_at_eol: bool,
    ignore_blank_lines: bool,
}

/// A single changed file: index side vs working tree.
#[derive(Debug, Clone)]
struct Change {
    /// Relative path.
    path: String,
    /// Single-letter status code (`M`, `D`, `A`, `U`).
    status: char,
    /// Index-side mode (octal).
    old_mode: u32,
    /// Working-tree-side mode (octal), or 0 for deleted.
    new_mode: u32,
    /// Index-side OID.
    old_oid: ObjectId,
    /// Working-tree blob OID (hashed content); zero when unknown or deleted from worktree.
    new_oid: ObjectId,
    /// `git add -N`: emit [`DiffStatus::Added`] with null index OID (t2203).
    intent_to_add: bool,
}

// ── Option parsing ───────────────────────────────────────────────────

fn parse_options(argv: &[String]) -> Result<Options> {
    let fmt = parse_diff_files_format_argv(&env::args().collect::<Vec<_>>());
    let mut pathspecs = Vec::new();
    // 255 = "no -N flag given" (git default: unmerged stage 2, normal merged diff).
    let mut stage: u8 = 255;
    let mut quiet = false;
    let mut exit_code = false;
    let mut abbrev: Option<usize> = None;
    let format = match fmt.format {
        crate::commands::diff::DiffFilesDefaultFormat::Raw => OutputFormat::Raw,
        crate::commands::diff::DiffFilesDefaultFormat::Patch => OutputFormat::Patch,
        crate::commands::diff::DiffFilesDefaultFormat::Stat => OutputFormat::Stat,
        crate::commands::diff::DiffFilesDefaultFormat::NumStat => OutputFormat::NumStat,
        crate::commands::diff::DiffFilesDefaultFormat::NameOnly => OutputFormat::NameOnly,
        crate::commands::diff::DiffFilesDefaultFormat::NameStatus => OutputFormat::NameStatus,
    };
    let explicit_raw = fmt.explicit_raw;
    let suppress_diff = fmt.suppress_diff;
    let stat_variant = fmt.stat_variant;
    let patch_with_raw = fmt.patch_with_raw;
    let patch_with_stat = fmt.patch_with_stat;
    let emit_queue = fmt.emit_queue;
    let dirstat_cli = fmt.dirstat_cli;
    let mut diff_filter: Option<String> = None;
    let mut ignore_submodules = false;
    let mut find_renames: Option<u32> = None;
    let mut find_copies = false;
    let mut find_copies_harder = false;
    let mut c_count = 0u32;
    let mut reverse = false;
    let mut break_rewrites = false;
    let mut ignore_all_space = false;
    let mut ignore_space_change = false;
    let mut ignore_space_at_eol = false;
    let mut ignore_blank_lines = false;
    let mut no_prefix = false;
    let mut end_of_options = false;

    let mut idx = 0usize;
    while idx < argv.len() {
        let arg = &argv[idx];
        if !end_of_options && arg == "--" {
            end_of_options = true;
            idx += 1;
            continue;
        }
        if !end_of_options && arg.starts_with('-') {
            match arg.as_str() {
                "-R" => reverse = true,
                "-B" | "--break-rewrites" => break_rewrites = true,
                _ if arg.starts_with("--break-rewrites=") => break_rewrites = true,
                _ if arg.starts_with("-B") && arg.len() > 2 => {
                    let rest = &arg[2..];
                    let num = rest.strip_suffix('%').unwrap_or(rest);
                    if num.is_empty() || num.chars().all(|c| c.is_ascii_digit()) {
                        break_rewrites = true;
                    } else if !rest.chars().all(|c| c.is_ascii_digit() || c == '%') {
                        bail!("unsupported option: {arg}");
                    } else {
                        break_rewrites = true;
                    }
                }
                "--exit-code" => exit_code = true,
                "-q" | "--quiet" => quiet = true,
                "-0" => stage = 0,
                "-1" => stage = 1,
                "-2" => stage = 2,
                "-3" => stage = 3,
                "--abbrev" => abbrev = Some(7),
                _ if arg.starts_with("--abbrev=") => {
                    let value = arg.trim_start_matches("--abbrev=");
                    let parsed = value
                        .parse::<usize>()
                        .with_context(|| format!("invalid --abbrev value: `{value}`"))?;
                    abbrev = Some(parsed);
                }
                "-w" | "--ignore-all-space" => ignore_all_space = true,
                "-b" | "--ignore-space-change" => ignore_space_change = true,
                "--ignore-space-at-eol" => ignore_space_at_eol = true,
                "--ignore-blank-lines" => ignore_blank_lines = true,
                // Silently accept diff options we don't fully implement yet
                "--no-prefix" => no_prefix = true,
                "--full-index" | "--no-ext-diff" | "--no-abbrev" => {}
                "--indent-heuristic" | "--no-indent-heuristic" => {}
                "-s" | "--no-patch" | "-p" | "--patch" | "-u" | "--raw" | "--stat"
                | "--compact-summary" | "--numstat" | "--shortstat" | "--name-only"
                | "--name-status" | "--summary" | "--patch-with-raw" | "--patch-with-stat"
                | "--dirstat" | "--dirstat-by-file" | "--cumulative" => {}
                s if s.starts_with("--dirstat=") => {}
                s if s.starts_with("--dirstat-by-file=") => {}
                "-M" | "--find-renames" => {
                    find_renames = Some(50);
                }
                "--no-renames" => {
                    find_renames = None;
                }
                _ if arg.starts_with("-M") && arg.len() > 2 => {
                    let val = &arg[2..];
                    let pct = if val.ends_with('%') {
                        val[..val.len() - 1].parse::<u32>().unwrap_or(50)
                    } else {
                        val.parse::<u32>().unwrap_or(50)
                    };
                    find_renames = Some(pct);
                }
                _ if arg.starts_with("--find-renames=") => {
                    let val = &arg["--find-renames=".len()..];
                    let pct = if val.ends_with('%') {
                        val[..val.len() - 1].parse::<u32>().unwrap_or(50)
                    } else {
                        val.parse::<u32>().unwrap_or(50)
                    };
                    find_renames = Some(pct);
                }
                "-C" | "--find-copies" => {
                    c_count += 1;
                    find_copies = true;
                    if c_count >= 2 {
                        find_copies_harder = true;
                    }
                    if find_renames.is_none() {
                        find_renames = Some(50);
                    }
                }
                "--find-copies-harder" => {
                    find_copies = true;
                    find_copies_harder = true;
                    if find_renames.is_none() {
                        find_renames = Some(50);
                    }
                }
                "--diff-filter" => {
                    if idx + 1 < argv.len() {
                        diff_filter = Some(argv[idx + 1].clone());
                        idx += 1;
                    }
                }
                _ if arg.starts_with("--diff-filter=") => {
                    diff_filter = Some(arg.trim_start_matches("--diff-filter=").to_string());
                }
                "--ignore-submodules" => {
                    ignore_submodules = true;
                    if idx + 1 < argv.len() {
                        let n = argv[idx + 1].as_str();
                        if matches!(n, "all" | "dirty" | "untracked" | "none") {
                            idx += 1;
                        }
                    }
                }
                // Global flags passed through that we accept but ignore
                "--literal-pathspecs"
                | "--glob-pathspecs"
                | "--noglob-pathspecs"
                | "--icase-pathspecs" => {}
                _ if arg.starts_with("--max-depth=") => {
                    // max-depth is not implemented for the worktree/index lane;
                    // git only succeeds for -1 (unlimited recursion), which is
                    // equivalent to the default behavior.
                    let val = &arg["--max-depth=".len()..];
                    let parsed = val
                        .parse::<i32>()
                        .with_context(|| format!("invalid --max-depth value: `{val}`"))?;
                    if parsed != -1 {
                        bail!("unsupported option: {arg}");
                    }
                }
                _ if arg.starts_with('-')
                    && !arg.starts_with("--")
                    && arg.len() > 2
                    && arg.as_bytes().get(1).is_some_and(|b| *b != b'-') =>
                {
                    const COMBINABLE: &[u8] = b"spuwqRb";
                    let bytes = arg.as_bytes();
                    let tail = &bytes[1..];
                    if tail.is_empty() || !tail.iter().all(|b| COMBINABLE.contains(b)) {
                        bail!("unsupported option: {arg}");
                    }
                }
                _ if arg.starts_with("-G")
                    || arg.starts_with("-S")
                    || arg.starts_with("-O")
                    || arg.starts_with("--src-prefix=")
                    || arg.starts_with("--dst-prefix=") => {}
                _ => bail!("unsupported option: {arg}"),
            }
            idx += 1;
            continue;
        }
        pathspecs.push(arg.clone());
        idx += 1;
    }

    Ok(Options {
        pathspecs,
        stage,
        quiet,
        exit_code,
        abbrev,
        format,
        explicit_raw,
        suppress_diff,
        stat_variant,
        patch_with_raw,
        patch_with_stat,
        emit_queue,
        dirstat_cli,
        diff_filter,
        ignore_submodules,
        find_renames,
        find_copies,
        find_copies_harder,
        reverse,
        break_rewrites,
        indent_heuristic: false,
        no_prefix,
        ignore_all_space,
        ignore_space_change,
        ignore_space_at_eol,
        ignore_blank_lines,
    })
}

fn diff_files_ws_any(o: &Options) -> bool {
    o.ignore_all_space || o.ignore_space_change || o.ignore_space_at_eol || o.ignore_blank_lines
}

fn diff_files_normalize_content(s: &str, o: &Options) -> String {
    if !diff_files_ws_any(o) {
        return s.to_owned();
    }
    let mut lines: Vec<String> = s
        .lines()
        .map(|line| {
            if o.ignore_all_space {
                line.chars().filter(|c| !c.is_whitespace()).collect()
            } else if o.ignore_space_change {
                normalize_ignore_space_change_line(line)
            } else if o.ignore_space_at_eol {
                line.trim_end().to_owned()
            } else {
                line.to_owned()
            }
        })
        .collect();
    if o.ignore_blank_lines {
        lines.retain(|l| !l.trim().is_empty());
    }
    lines.join("\n")
}

fn diff_files_ws_content_matches_index_wt(
    repo: &Repository,
    idx_oid: &ObjectId,
    wt_bytes: &[u8],
    o: &Options,
) -> Result<bool> {
    let old = if *idx_oid == zero_oid() {
        String::new()
    } else {
        let obj = repo.odb.read(idx_oid)?;
        String::from_utf8_lossy(&obj.data).into_owned()
    };
    let new = String::from_utf8_lossy(wt_bytes).into_owned();
    Ok(diff_files_normalize_content(&old, o) == diff_files_normalize_content(&new, o))
}

fn filter_diff_files_whitespace_equivalent(
    entries: Vec<DiffEntry>,
    repo: &Repository,
    work_tree: &Path,
    o: &Options,
) -> Result<Vec<DiffEntry>> {
    if !diff_files_ws_any(o) {
        return Ok(entries);
    }
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        if e.status != DiffStatus::Modified {
            out.push(e);
            continue;
        }
        if e.old_mode != e.new_mode {
            out.push(e);
            continue;
        }
        let (old, new) = load_patch_contents_for_diff_entry(&e, repo, work_tree)?;
        if diff_files_normalize_content(&old, o) != diff_files_normalize_content(&new, o) {
            out.push(e);
        }
    }
    Ok(out)
}

// ── Core diff logic ──────────────────────────────────────────────────

/// True when the index entry has real cached stat data from the filesystem.
///
/// After `read-tree` or `update-index --cacheinfo`, Git leaves ctime/mtime/dev/ino/size at
/// zero until refresh; `diff-files` must not treat "blob on disk matches index OID" as clean
/// in that state (see Git `ce_match_stat_basic` / `run_diff_files`).
fn index_stat_is_trusted(entry: &IndexEntry) -> bool {
    entry.size != 0
        || entry.mtime_sec != 0
        || entry.mtime_nsec != 0
        || entry.ctime_sec != 0
        || entry.ctime_nsec != 0
        || entry.dev != 0
        || entry.ino != 0
}

/// Build the list of changes between the index and the working tree.
fn collect_changes(
    repo: &Repository,
    index: &Index,
    work_tree: &Path,
    options: &Options,
    index_mtime: Option<(u32, u32)>,
) -> Result<Vec<Change>> {
    // Collect index entries, grouped by path.  For stage==0 we use merged
    // entries (stage 0).  For stage 1–3 we use that specific unmerged stage.
    // Paths that only have higher-stage entries and no stage-0 entry are
    // "unmerged"; we report them as 'U' when stage==0.
    let mut stage0: BTreeMap<String, (u32, ObjectId, &IndexEntry)> = BTreeMap::new();
    let mut unmerged_paths: BTreeSet<String> = BTreeSet::new();
    let mut staged: BTreeMap<String, (u32, ObjectId, bool)> = BTreeMap::new();

    // git's `diff_unmerged_stage` defaults to 2 (theirs/ours = stage 2). Plain
    // `diff-files` therefore behaves like `diff-files -2` for unmerged paths:
    // it emits a U line plus an M/D line against stage 2. `-1/-2/-3` override it.
    // git's `diff_unmerged_stage` defaults to 2; `-0` selects "U only" (no M/D),
    // `-1/-2/-3` select that stage. 255 is our sentinel for "no -N given".
    let unmerged_compare_stage: u8 = if options.stage == 255 {
        2
    } else {
        options.stage
    };

    for entry in &index.entries {
        let Ok(path) = String::from_utf8(entry.path.clone()) else {
            continue;
        };
        if !matches_pathspec(&path, &options.pathspecs) {
            continue;
        }
        let s = entry.stage();
        if s == 0 {
            if options.ignore_submodules && entry.mode == MODE_GITLINK {
                continue;
            }
            stage0.insert(path, (entry.mode, entry.oid, entry));
        } else {
            unmerged_paths.insert(path.clone());
            if s == unmerged_compare_stage {
                let skip_wt_examine = entry.assume_unchanged() || entry.skip_worktree();
                staged.insert(path, (entry.mode, entry.oid, skip_wt_examine));
            }
        }
    }

    let mut changes: BTreeMap<String, Change> = BTreeMap::new();

    if options.stage == 0 || options.stage == 255 {
        // Normal mode: compare stage-0 entries against worktree.
        // Use stat info to skip unchanged files (avoid hashing).
        for (path, (idx_mode, idx_oid, idx_entry)) in &stage0 {
            let abs = work_tree.join(path);
            match read_worktree_info_fast(repo, work_tree, &abs, idx_entry, index_mtime)? {
                WorktreeStatus::Unchanged => { /* skip — stat says identical */ }
                WorktreeStatus::Modified(wt_mode, wt_oid) => {
                    let idx_canonical = canonicalize_mode(*idx_mode);
                    let mut effective_wt_oid = wt_oid;
                    if effective_wt_oid != *idx_oid {
                        let abs = work_tree.join(path);
                        if let Ok(raw) = fs::read(&abs) {
                            let raw_oid =
                                grit_lib::odb::Odb::hash_object_data(ObjectKind::Blob, &raw);
                            if raw_oid == *idx_oid {
                                effective_wt_oid = *idx_oid;
                            }
                        }
                    }
                    let content_matches = effective_wt_oid == *idx_oid && wt_mode == idx_canonical;
                    // Git's `run_diff_files` reports a file as modified whenever the worktree
                    // stat tuple disagrees with the index, even if re-hashing the content would
                    // yield the same OID (it never re-hashes to clear the change). Only suppress
                    // a content-equal entry when its on-disk stat *also* matches the index; a
                    // stat-dirty entry must still be reported as `M` (t7508 read-only repo).
                    // For racy entries (mtime in the racy window relative to the index file's
                    // own mtime), git re-hashes: a content-equal racy entry is suppressed
                    // (t0010 racy-clean), a content-different one is reported.
                    let stat_agrees = fs::symlink_metadata(&abs)
                        .map(|m| stat_matches(idx_entry, &m))
                        .unwrap_or(false);
                    if content_matches
                        && index_stat_is_trusted(idx_entry)
                        && stat_agrees
                        && !idx_entry.intent_to_add()
                    {
                        continue;
                    }
                    if effective_wt_oid != *idx_oid
                        || wt_mode != idx_canonical
                        || idx_entry.intent_to_add()
                    {
                        // Detect type changes (e.g., symlink ↔ regular, regular ↔ submodule)
                        let status = if mode_type(idx_canonical) != mode_type(wt_mode) {
                            'T'
                        } else {
                            'M'
                        };
                        changes.insert(
                            path.clone(),
                            Change {
                                path: path.clone(),
                                status,
                                old_mode: idx_canonical,
                                new_mode: wt_mode,
                                old_oid: *idx_oid,
                                new_oid: effective_wt_oid,
                                intent_to_add: idx_entry.intent_to_add(),
                            },
                        );
                    } else {
                        // Same blob and mode on disk as in the index, but index stat is
                        // uninitialized (e.g. post-`read-tree` checkout without `-u`).
                        let ws_only = diff_files_ws_any(options)
                            && fs::read(&abs).ok().is_some_and(|bytes| {
                                diff_files_ws_content_matches_index_wt(
                                    repo, idx_oid, &bytes, options,
                                )
                                .unwrap_or(false)
                            });
                        if !ws_only {
                            changes.insert(
                                path.clone(),
                                Change {
                                    path: path.clone(),
                                    status: 'M',
                                    old_mode: idx_canonical,
                                    new_mode: wt_mode,
                                    old_oid: *idx_oid,
                                    new_oid: effective_wt_oid,
                                    intent_to_add: false,
                                },
                            );
                        }
                    }
                }
                WorktreeStatus::Missing => {
                    // File missing from working tree.
                    changes.insert(
                        path.clone(),
                        Change {
                            path: path.clone(),
                            status: 'D',
                            old_mode: canonicalize_mode(*idx_mode),
                            new_mode: 0,
                            old_oid: *idx_oid,
                            new_oid: zero_oid(),
                            intent_to_add: idx_entry.intent_to_add(),
                        },
                    );
                }
            }
        }

        // Unmerged paths (no stage-0 entry): emit U + (M/D against the default
        // unmerged stage 2), like git's plain `diff-files`. These go into an
        // ordered Vec (two lines can share a path) and are merged below.
        let mut unmerged_out: Vec<Change> = Vec::new();
        for path in &unmerged_paths {
            if stage0.contains_key(path) {
                continue;
            }
            if !matches_pathspec(path, &options.pathspecs) {
                continue;
            }
            let abs = work_tree.join(path);
            let wt_info = read_worktree_info(repo, &abs)?;
            let wt_mode_for_u = wt_info.as_ref().map_or(0, |(m, _)| *m);
            unmerged_out.push(Change {
                path: path.clone(),
                status: 'U',
                old_mode: 0,
                new_mode: wt_mode_for_u,
                old_oid: zero_oid(),
                new_oid: zero_oid(),
                intent_to_add: false,
            });
            if let Some((idx_mode, idx_oid, skip_wt_examine)) = staged.get(path) {
                if *skip_wt_examine {
                    continue;
                }
                match wt_info {
                    Some((wt_mode, wt_oid)) => {
                        let idx_mode = canonicalize_mode(*idx_mode);
                        if idx_mode == wt_mode && *idx_oid == wt_oid {
                            continue;
                        }
                        unmerged_out.push(Change {
                            path: path.clone(),
                            status: 'M',
                            old_mode: idx_mode,
                            new_mode: wt_mode,
                            old_oid: *idx_oid,
                            new_oid: wt_oid,
                            intent_to_add: false,
                        });
                    }
                    None => {
                        unmerged_out.push(Change {
                            path: path.clone(),
                            status: 'D',
                            old_mode: canonicalize_mode(*idx_mode),
                            new_mode: 0,
                            old_oid: *idx_oid,
                            new_oid: zero_oid(),
                            intent_to_add: false,
                        });
                    }
                }
            }
        }
        // Merge merged-path changes (from `changes`) with unmerged U/M/D lines,
        // keeping overall path order and U-before-M/D per path.
        let mut combined: Vec<Change> = changes.into_values().collect();
        combined.extend(unmerged_out);
        combined.sort_by(|a, b| {
            a.path
                .cmp(&b.path)
                .then_with(|| status_rank(a.status).cmp(&status_rank(b.status)))
        });
        let mut out = combined;
        if let Some(spec) = options.diff_filter.as_deref() {
            out.retain(|change| matches_diff_filter(change.status, spec));
        }
        return Ok(out);
    } else {
        // Stage-specific mode (`diff-files -1/-2/-3`). git emits, for *every*
        // unmerged path, a ":000000 <wt-mode> 0 0 U" line (diff_unmerge), and
        // *additionally* an M/D line when that path has an entry at the requested
        // stage. The two lines share a path, so we build an ordered Vec here
        // instead of the per-path map; U precedes M/D for the same path.
        let mut staged_out: Vec<Change> = Vec::new();
        for path in &unmerged_paths {
            if stage0.contains_key(path) {
                continue;
            }
            if !matches_pathspec(path, &options.pathspecs) {
                continue;
            }
            let abs = work_tree.join(path);
            let wt_info = read_worktree_info(repo, &abs)?;
            let wt_mode_for_u = wt_info.as_ref().map_or(0, |(m, _)| *m);
            // U line for the unmerged path.
            staged_out.push(Change {
                path: path.clone(),
                status: 'U',
                old_mode: 0,
                new_mode: wt_mode_for_u,
                old_oid: zero_oid(),
                new_oid: zero_oid(),
                intent_to_add: false,
            });
            // M/D line against the requested stage, if present.
            if let Some((idx_mode, idx_oid, skip_wt_examine)) = staged.get(path) {
                if *skip_wt_examine {
                    continue;
                }
                match wt_info {
                    Some((wt_mode, wt_oid)) => {
                        let idx_mode = canonicalize_mode(*idx_mode);
                        if idx_mode == wt_mode && *idx_oid == wt_oid {
                            continue;
                        }
                        staged_out.push(Change {
                            path: path.clone(),
                            status: 'M',
                            old_mode: idx_mode,
                            new_mode: wt_mode,
                            old_oid: *idx_oid,
                            new_oid: wt_oid,
                            intent_to_add: false,
                        });
                    }
                    None => {
                        staged_out.push(Change {
                            path: path.clone(),
                            status: 'D',
                            old_mode: canonicalize_mode(*idx_mode),
                            new_mode: 0,
                            old_oid: *idx_oid,
                            new_oid: zero_oid(),
                            intent_to_add: false,
                        });
                    }
                }
            }
        }
        let mut out: Vec<Change> = staged_out;
        if let Some(spec) = options.diff_filter.as_deref() {
            out.retain(|change| matches_diff_filter(change.status, spec));
        }
        Ok(out)
    }
}

/// Ordering rank for combined raw output of an unmerged path: the `U` line is
/// emitted before the per-stage `M`/`D` line (git diff_unmerge then diff_change).
fn status_rank(status: char) -> u8 {
    match status {
        'U' => 0,
        _ => 1,
    }
}

fn change_to_diff_entry(c: &Change) -> DiffEntry {
    if c.intent_to_add && c.status == 'M' {
        let new_mode_str = format!("{:06o}", c.new_mode);
        return DiffEntry {
            status: DiffStatus::Added,
            old_path: None,
            new_path: Some(RepoPathBuf::from_string(c.path.clone())),
            old_mode: "000000".to_owned(),
            new_mode: new_mode_str,
            old_oid: zero_oid(),
            new_oid: c.new_oid,
            score: None,
        };
    }
    let old_mode_str = format!("{:06o}", c.old_mode);
    let new_mode_str = format!("{:06o}", c.new_mode);
    match c.status {
        'D' => DiffEntry {
            status: DiffStatus::Deleted,
            // TODO(byte-paths): Change.path is a lossy String; migrate its source (Phase 2/4).
            old_path: Some(RepoPathBuf::from_string(c.path.clone())),
            new_path: None,
            old_mode: old_mode_str,
            new_mode: new_mode_str,
            old_oid: c.old_oid,
            new_oid: zero_oid(),
            score: None,
        },
        'U' => DiffEntry {
            status: DiffStatus::Unmerged,
            // TODO(byte-paths): Change.path is a lossy String; migrate its source (Phase 2/4).
            old_path: Some(RepoPathBuf::from_string(c.path.clone())),
            new_path: Some(RepoPathBuf::from_string(c.path.clone())),
            old_mode: old_mode_str,
            new_mode: new_mode_str,
            old_oid: c.old_oid,
            new_oid: c.new_oid,
            score: None,
        },
        'T' => DiffEntry {
            status: DiffStatus::TypeChanged,
            // TODO(byte-paths): Change.path is a lossy String; migrate its source (Phase 2/4).
            old_path: Some(RepoPathBuf::from_string(c.path.clone())),
            new_path: Some(RepoPathBuf::from_string(c.path.clone())),
            old_mode: old_mode_str,
            new_mode: new_mode_str,
            old_oid: c.old_oid,
            new_oid: c.new_oid,
            score: None,
        },
        _ => DiffEntry {
            status: DiffStatus::Modified,
            // TODO(byte-paths): Change.path is a lossy String; migrate its source (Phase 2/4).
            old_path: Some(RepoPathBuf::from_string(c.path.clone())),
            new_path: Some(RepoPathBuf::from_string(c.path.clone())),
            old_mode: old_mode_str,
            new_mode: new_mode_str,
            old_oid: c.old_oid,
            new_oid: c.new_oid,
            score: None,
        },
    }
}

/// Swap old/new sides for `diff-files -R` before rename/copy detection.
fn reverse_diff_entry_for_diff_files(mut e: DiffEntry) -> DiffEntry {
    match e.status {
        DiffStatus::Added => {
            e.status = DiffStatus::Deleted;
            e.old_path = e.new_path.take();
            e.new_path = None;
            std::mem::swap(&mut e.old_mode, &mut e.new_mode);
            std::mem::swap(&mut e.old_oid, &mut e.new_oid);
        }
        DiffStatus::Deleted => {
            e.status = DiffStatus::Added;
            e.new_path = e.old_path.take();
            e.old_path = None;
            std::mem::swap(&mut e.old_mode, &mut e.new_mode);
            std::mem::swap(&mut e.old_oid, &mut e.new_oid);
        }
        DiffStatus::Renamed | DiffStatus::Copied => {
            std::mem::swap(&mut e.old_path, &mut e.new_path);
            std::mem::swap(&mut e.old_mode, &mut e.new_mode);
            std::mem::swap(&mut e.old_oid, &mut e.new_oid);
        }
        DiffStatus::Modified | DiffStatus::TypeChanged | DiffStatus::Unmerged => {
            std::mem::swap(&mut e.old_mode, &mut e.new_mode);
            std::mem::swap(&mut e.old_oid, &mut e.new_oid);
        }
    }
    e
}

fn render_raw_diff_entry(
    entry: &DiffEntry,
    repo: &Repository,
    abbrev: Option<usize>,
    reverse: bool,
) -> Result<String> {
    let width = abbrev.unwrap_or(40).clamp(4, 40);
    let old_oid = format_oid_for_raw(entry.old_oid, repo, abbrev, width)?;
    let new_oid = if reverse {
        format_oid_for_raw(entry.new_oid, repo, abbrev, width)?
    } else {
        "0".repeat(width)
    };

    let status_str = match (entry.status, entry.score) {
        (DiffStatus::Renamed, Some(s)) => format!("R{s:03}"),
        (DiffStatus::Copied, Some(s)) => format!("C{s:03}"),
        (DiffStatus::Modified, Some(pct)) => format!("M{pct:03}"),
        _ => entry.status.letter().to_string(),
    };

    let path = match entry.status {
        DiffStatus::Renamed | DiffStatus::Copied => format!(
            "{}\t{}",
            entry.old_path.as_deref().unwrap_or(RepoPath::from_str("")),
            entry.new_path.as_deref().unwrap_or(RepoPath::from_str(""))
        ),
        _ => entry.path().to_string(),
    };

    Ok(format!(
        ":{} {} {} {} {}\t{}",
        entry.old_mode, entry.new_mode, old_oid, new_oid, status_str, path
    ))
}

fn format_oid_for_raw(
    oid: ObjectId,
    repo: &Repository,
    abbrev: Option<usize>,
    width: usize,
) -> Result<String> {
    if oid == zero_oid() {
        return Ok("0".repeat(width));
    }
    match abbrev {
        Some(min_len) => abbreviate_object_id(repo, oid, min_len).map_err(Into::into),
        None => Ok(oid.to_hex()),
    }
}

fn abbrev_oid_for_patch(oid: &ObjectId, abbrev: Option<usize>) -> String {
    let hex = oid.to_hex();
    let len = abbrev.unwrap_or(7).clamp(4, hex.len());
    hex[..len].to_owned()
}

fn print_patch_from_diff_entry(
    entry: &DiffEntry,
    repo: &Repository,
    work_tree: &Path,
    ws_opts: &Options,
    abbrev: Option<usize>,
    indent_heuristic: bool,
    no_prefix: bool,
) -> Result<()> {
    let quote_path_fully = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true)
        .unwrap_or_default()
        .quote_path_fully();
    let (old_content, new_content) = load_patch_contents_for_diff_entry(entry, repo, work_tree)?;
    let old_path = entry
        .old_path
        .as_deref()
        .unwrap_or(entry.new_path.as_deref().unwrap_or(RepoPath::from_str("")));
    let new_path = entry
        .new_path
        .as_deref()
        .unwrap_or(entry.old_path.as_deref().unwrap_or(RepoPath::from_str("")));

    let (src_prefix, dst_prefix) = if no_prefix { ("", "") } else { ("a/", "b/") };
    let old_label = match entry.status {
        DiffStatus::Added => "/dev/null".to_owned(),
        _ => format!("{src_prefix}{old_path}"),
    };
    let new_label = match entry.status {
        DiffStatus::Deleted => "/dev/null".to_owned(),
        _ => format!("{dst_prefix}{new_path}"),
    };

    let display_path = entry.path().to_string_lossy();
    let mut header = format!("diff --git {src_prefix}{old_path} {dst_prefix}{new_path}");
    match entry.status {
        DiffStatus::Deleted => {
            header.push_str(&format!("\ndeleted file mode {}", entry.old_mode));
            header.push_str(&format!(
                "\nindex {}..{}",
                abbrev_oid_for_patch(&entry.old_oid, abbrev),
                abbrev_oid_for_patch(&zero_oid(), abbrev),
            ));
        }
        DiffStatus::Added => {
            header.push_str(&format!("\nnew file mode {}", entry.new_mode));
            let new_for_index = if entry.old_oid == zero_oid() && entry.new_oid == empty_blob_oid()
            {
                empty_blob_oid()
            } else {
                entry.new_oid
            };
            header.push_str(&format!(
                "\nindex {}..{}",
                abbrev_oid_for_patch(&entry.old_oid, abbrev),
                abbrev_oid_for_patch(&new_for_index, abbrev)
            ));
        }
        DiffStatus::Renamed => {
            let sim = entry.score.unwrap_or(100);
            header.push_str(&format!(
                "\nsimilarity index {sim}%\nrename from {old_path}\nrename to {new_path}"
            ));
        }
        DiffStatus::Copied => {
            let sim = entry.score.unwrap_or(100);
            header.push_str(&format!(
                "\nsimilarity index {sim}%\ncopy from {old_path}\ncopy to {new_path}"
            ));
        }
        _ => {
            if entry.old_mode != entry.new_mode {
                header.push_str(&format!(
                    "\nold mode {}\nnew mode {}",
                    entry.old_mode, entry.new_mode
                ));
            }
            // `git diff-files -p` includes an `index old..new <mode>` line for content changes
            // even when the mode is unchanged (required by t4115-apply-symlink symlink diffs).
            header.push_str(&format!(
                "\nindex {}..{} {}",
                abbrev_oid_for_patch(&entry.old_oid, abbrev),
                abbrev_oid_for_patch(&entry.new_oid, abbrev),
                entry.new_mode
            ));
        }
    }

    if (entry.status == DiffStatus::Renamed || entry.status == DiffStatus::Copied)
        && entry.old_oid == entry.new_oid
    {
        println!("{header}");
        return Ok(());
    }

    if old_content == new_content
        && entry.old_mode != entry.new_mode
        && entry.status != DiffStatus::Renamed
        && entry.status != DiffStatus::Copied
    {
        println!("{header}");
    } else if old_content != new_content {
        let ws_equivalent = diff_files_ws_any(ws_opts)
            && diff_files_normalize_content(&old_content, ws_opts)
                == diff_files_normalize_content(&new_content, ws_opts);
        if !ws_equivalent {
            let patch = unified_diff(
                &old_content,
                &new_content,
                &display_path,
                &display_path,
                3,
                indent_heuristic,
                quote_path_fully,
            );
            let body: String = patch.lines().skip(2).map(|l| format!("\n{l}")).collect();
            println!("{header}\n--- {old_label}\n+++ {new_label}{body}");
        } else {
            println!("{header}\n--- {old_label}\n+++ {new_label}");
        }
    } else {
        println!("{header}\n--- {old_label}\n+++ {new_label}");
    }
    Ok(())
}

fn print_stat_from_diff_entries(
    entries: &[DiffEntry],
    repo: &Repository,
    work_tree: &Path,
    ws_opts: &Options,
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let max_len = entries.iter().map(|e| e.path().len()).max().unwrap_or(0);
    let mut total_ins = 0usize;
    let mut total_del = 0usize;
    for entry in entries {
        let (old, new) = load_patch_contents_for_diff_entry(entry, repo, work_tree)?;
        let (old, new) = if diff_files_ws_any(ws_opts) {
            (
                diff_files_normalize_content(&old, ws_opts),
                diff_files_normalize_content(&new, ws_opts),
            )
        } else {
            (old, new)
        };
        let (ins, del) = count_changes(&old, &new);
        total_ins += ins;
        total_del += del;
        println!(
            "{}",
            format_stat_line(&entry.path().to_string_lossy(), ins, del, max_len)
        );
    }
    let files = entries.len();
    let mut summary = format!(
        " {} file{} changed",
        files,
        if files == 1 { "" } else { "s" },
    );
    if total_ins > 0 || (total_ins == 0 && total_del == 0) {
        summary.push_str(&format!(
            ", {} insertion{}(+)",
            total_ins,
            if total_ins == 1 { "" } else { "s" }
        ));
    }
    if total_del > 0 || (total_ins == 0 && total_del == 0) {
        summary.push_str(&format!(
            ", {} deletion{}(-)",
            total_del,
            if total_del == 1 { "" } else { "s" }
        ));
    }
    println!("{summary}");
    Ok(())
}

fn print_numstat_from_diff_entries(
    entries: &[DiffEntry],
    repo: &Repository,
    work_tree: &Path,
    ws_opts: &Options,
) -> Result<()> {
    for entry in entries {
        let (old, new) = load_patch_contents_for_diff_entry(entry, repo, work_tree)?;
        let (old, new) = if diff_files_ws_any(ws_opts) {
            (
                diff_files_normalize_content(&old, ws_opts),
                diff_files_normalize_content(&new, ws_opts),
            )
        } else {
            (old, new)
        };
        let (ins, del) = count_changes(&old, &new);
        println!("{}\t{}\t{}", ins, del, entry.path());
    }
    Ok(())
}

fn append_diff_files_stat_totals(summary: &mut String, total_ins: usize, total_del: usize) {
    if total_ins > 0 {
        summary.push_str(&format!(
            ", {} insertion{}(+)",
            total_ins,
            if total_ins == 1 { "" } else { "s" }
        ));
    }
    if total_del > 0 {
        summary.push_str(&format!(
            ", {} deletion{}(-)",
            total_del,
            if total_del == 1 { "" } else { "s" }
        ));
    }
    if total_ins == 0 && total_del == 0 {
        summary.push_str(", 0 insertions(+), 0 deletions(-)");
    }
}

fn write_diff_files_shortstat_line(
    entries: &[DiffEntry],
    repo: &Repository,
    work_tree: &Path,
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let mut total_ins = 0usize;
    let mut total_del = 0usize;
    for entry in entries {
        let (old, new) = load_patch_contents_for_diff_entry(entry, repo, work_tree)?;
        let (ins, del) = count_changes(&old, &new);
        total_ins += ins;
        total_del += del;
    }
    let files = entries.len();
    let mut line = format!(
        " {} file{} changed",
        files,
        if files == 1 { "" } else { "s" }
    );
    append_diff_files_stat_totals(&mut line, total_ins, total_del);
    println!("{line}");
    Ok(())
}

fn mode_has_executable_bit(mode_str: &str) -> bool {
    u32::from_str_radix(mode_str, 8)
        .map(|m| m & 0o111 != 0)
        .unwrap_or(false)
}

fn compact_summary_path_display(entry: &DiffEntry) -> String {
    let path = entry.path().to_string();
    match entry.status {
        DiffStatus::Added => format!("{path} (new)"),
        DiffStatus::Deleted => format!("{path} (gone)"),
        _ => {
            let old_x = mode_has_executable_bit(&entry.old_mode);
            let new_x = mode_has_executable_bit(&entry.new_mode);
            if new_x != old_x {
                if new_x {
                    format!("{path} (mode +x)")
                } else {
                    format!("{path} (mode -x)")
                }
            } else {
                path
            }
        }
    }
}

fn load_patch_bytes_for_diff_entry(
    entry: &DiffEntry,
    repo: &Repository,
    work_tree: &Path,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let old = if entry.status == DiffStatus::Added || entry.old_oid == zero_oid() {
        Vec::new()
    } else {
        repo.odb
            .read(&entry.old_oid)
            .map(|o| o.data)
            .unwrap_or_default()
    };
    let new = if entry.status == DiffStatus::Deleted {
        Vec::new()
    } else {
        let path = entry.new_path.as_deref().unwrap_or(entry.path());
        let abs = path.to_fs_path(work_tree);
        match fs::read(&abs) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        }
    };
    Ok((old, new))
}

fn print_compact_summary_from_diff_entries(
    entries: &[DiffEntry],
    repo: &Repository,
    work_tree: &Path,
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let mut files: Vec<FileStatInput> = Vec::with_capacity(entries.len());
    let mut total_ins = 0usize;
    let mut total_del = 0usize;
    for entry in entries {
        let (old_b, new_b) = load_patch_bytes_for_diff_entry(entry, repo, work_tree)?;
        let binary =
            grit_lib::merge_file::is_binary(&old_b) || grit_lib::merge_file::is_binary(&new_b);
        let (ins, del) = if binary {
            let deleted = if entry.old_oid == zero_oid() {
                0
            } else {
                old_b.len()
            };
            let added = if entry.new_oid == zero_oid() {
                0
            } else {
                new_b.len()
            };
            (added, deleted)
        } else {
            let old_s = String::from_utf8_lossy(&old_b).into_owned();
            let new_s = String::from_utf8_lossy(&new_b).into_owned();
            count_changes(&old_s, &new_s)
        };
        total_ins += ins;
        total_del += del;
        files.push(FileStatInput {
            path_display: compact_summary_path_display(entry),
            insertions: ins,
            deletions: del,
            is_binary: binary,
            is_unmerged: false,
        });
    }
    let cfg = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), false)
        .unwrap_or_else(|_| grit_lib::config::ConfigSet::new());
    let stat_name_width = cfg
        .get("diff.statNameWidth")
        .and_then(|v| v.parse::<usize>().ok());
    let stat_graph_width = cfg
        .get("diff.statGraphWidth")
        .and_then(|v| v.parse::<usize>().ok());
    let opts = DiffstatOptions {
        total_width: terminal_columns(),
        line_prefix: "",
        width_prefix: "",
        subtract_prefix_from_terminal: false,
        stat_name_width,
        stat_graph_width,
        stat_count: None,
        color_add: "",
        color_del: "",
        color_reset: "",
        graph_bar_slack: 0,
        graph_prefix_budget_slack: 0,
    };
    write_diffstat_block(&mut std::io::stdout().lock(), &files, &opts)?;
    let n = entries.len();
    let mut summary = format!(" {} file{} changed", n, if n == 1 { "" } else { "s" });
    append_diff_files_stat_totals(&mut summary, total_ins, total_del);
    println!("{summary}");
    Ok(())
}

fn load_patch_contents_for_diff_entry(
    entry: &DiffEntry,
    repo: &Repository,
    work_tree: &Path,
) -> Result<(String, String)> {
    let old_content = if entry.status == DiffStatus::Added || entry.old_oid == zero_oid() {
        String::new()
    } else {
        let obj = repo.odb.read(&entry.old_oid)?;
        String::from_utf8(obj.data).unwrap_or_default()
    };

    let new_content = if entry.status == DiffStatus::Deleted {
        String::new()
    } else {
        let path = entry.new_path.as_deref().unwrap_or(entry.path());
        let abs = path.to_fs_path(work_tree);
        match fs::symlink_metadata(&abs) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = fs::read_link(&abs)?;
                #[cfg(unix)]
                {
                    use std::os::unix::ffi::OsStrExt;
                    String::from_utf8_lossy(target.as_os_str().as_bytes()).into_owned()
                }
                #[cfg(not(unix))]
                {
                    target.to_string_lossy().into_owned()
                }
            }
            Ok(_) => match fs::read(&abs) {
                Ok(bytes) => String::from_utf8(bytes).unwrap_or_default(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(e) => return Err(e.into()),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e.into()),
        }
    };

    Ok((old_content, new_content))
}

fn matches_diff_filter(status: char, spec: &str) -> bool {
    if spec.is_empty() {
        return true;
    }
    let status = status.to_ascii_uppercase();
    let mut includes: Vec<char> = Vec::new();
    let mut excludes: Vec<char> = Vec::new();
    for c in spec.chars() {
        if c == '*' {
            continue;
        }
        if c.is_ascii_uppercase() {
            includes.push(c);
        } else if c.is_ascii_lowercase() {
            excludes.push(c.to_ascii_uppercase());
        }
    }
    if !includes.is_empty() && !includes.contains(&status) {
        return false;
    }
    if excludes.contains(&status) {
        return false;
    }
    true
}

// ── Worktree probing ─────────────────────────────────────────────────

/// Result of probing a working-tree file against its index entry.
enum WorktreeStatus {
    /// File is unchanged according to stat info — no need to hash.
    Unchanged,
    /// File exists and may be modified (mode, oid from full hash).
    Modified(u32, ObjectId),
    /// File is missing from the working tree.
    Missing,
}

/// Fast worktree probe: uses stat() data from the index to skip hashing
/// when the file hasn't changed.  Falls back to full read+hash if stat
/// info doesn't match.
fn path_component_is_not_directory(err: &std::io::Error) -> bool {
    if err.kind() == std::io::ErrorKind::NotADirectory {
        return true;
    }
    #[cfg(unix)]
    {
        if err.raw_os_error() == Some(libc::ENOTDIR) {
            return true;
        }
    }
    false
}

fn index_file_mtime_pair(index_path: &Path) -> Option<(u32, u32)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = fs::metadata(index_path).ok()?;
        return Some((meta.mtime() as u32, meta.mtime_nsec() as u32));
    }
    #[cfg(not(unix))]
    {
        let meta = fs::metadata(index_path).ok()?;
        let mtime = meta
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        Some((mtime.as_secs() as u32, mtime.subsec_nanos()))
    }
}

fn index_entry_is_racy(index_entry: &IndexEntry, index_mtime: Option<(u32, u32)>) -> bool {
    let Some((index_mtime_sec, index_mtime_nsec)) = index_mtime else {
        return false;
    };
    if index_mtime_sec == 0 {
        return false;
    }
    index_mtime_sec < index_entry.mtime_sec
        || (index_mtime_sec == index_entry.mtime_sec && index_mtime_nsec <= index_entry.mtime_nsec)
}

fn read_worktree_info_fast(
    repo: &Repository,
    super_worktree: &Path,
    abs_path: &Path,
    index_entry: &IndexEntry,
    index_mtime: Option<(u32, u32)>,
) -> Result<WorktreeStatus> {
    if index_entry.assume_unchanged() || index_entry.skip_worktree() {
        return Ok(WorktreeStatus::Unchanged);
    }

    if path_has_symlink_parent(super_worktree, abs_path) {
        return Ok(WorktreeStatus::Missing);
    }

    let meta = match fs::symlink_metadata(abs_path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if canonicalize_mode(index_entry.mode) == MODE_GITLINK {
                return Ok(WorktreeStatus::Unchanged);
            }
            return Ok(WorktreeStatus::Missing);
        }
        Err(e) if path_component_is_not_directory(&e) => {
            if canonicalize_mode(index_entry.mode) == MODE_GITLINK {
                return Ok(WorktreeStatus::Unchanged);
            }
            return Ok(WorktreeStatus::Missing);
        }
        Err(e) => return Err(e.into()),
    };

    let _ = repo;

    // Intent-to-add: `ie_match_stat` in Git always reports dirty until fully staged.
    // Never treat as unchanged from stat alone (t2203 `git diff-files` vs `git diff`).
    if index_entry.intent_to_add() {
        if meta.file_type().is_symlink() {
            let target = fs::read_link(abs_path)?;
            let oid = Odb::hash_object_data(ObjectKind::Blob, target.as_os_str().as_bytes());
            return Ok(WorktreeStatus::Modified(MODE_SYMLINK, oid));
        }
        if meta.file_type().is_file() {
            let mode = if meta.permissions().mode() & 0o111 != 0 {
                MODE_EXECUTABLE
            } else {
                MODE_REGULAR
            };
            let data = fs::read(abs_path)?;
            let oid = Odb::hash_object_data(ObjectKind::Blob, &data);
            return Ok(WorktreeStatus::Modified(mode, oid));
        }
        if meta.file_type().is_dir() {
            let dot_git = abs_path.join(".git");
            if dot_git.exists() {
                let sub_oid = read_submodule_head(abs_path).unwrap_or(index_entry.oid);
                return Ok(WorktreeStatus::Modified(0o160000, sub_oid));
            }
        }
        return Ok(WorktreeStatus::Missing);
    }

    // Symlinks: when lstat matches the index, skip readlink+hash (matches Git `ce_match_stat`;
    // needed so `diff-files` agrees with `apply` after symlink-only updates — see t4115).
    if canonicalize_mode(index_entry.mode) == MODE_SYMLINK && meta.file_type().is_symlink() {
        let racy = index_entry_is_racy(index_entry, index_mtime);
        if stat_matches(index_entry, &meta) && !racy {
            return Ok(WorktreeStatus::Unchanged);
        }
    }

    // Fast path: if stat info matches the index, file is unchanged.
    // But also check if the index mode differs from the worktree mode
    // (e.g., after git update-index --chmod=+x).
    let racy = index_entry_is_racy(index_entry, index_mtime);
    if meta.file_type().is_file() && stat_matches(index_entry, &meta) && !racy {
        let wt_mode = if meta.permissions().mode() & 0o111 != 0 {
            MODE_EXECUTABLE
        } else {
            MODE_REGULAR
        };
        let idx_mode = canonicalize_mode(index_entry.mode);
        if wt_mode == idx_mode {
            return Ok(WorktreeStatus::Unchanged);
        }
        // Mode differs — report as modified with same OID.
        return Ok(WorktreeStatus::Modified(wt_mode, index_entry.oid));
    }

    if meta.file_type().is_symlink() {
        // Reached only when the symlink's lstat did *not* match the index (the
        // clean-stat case returns `Unchanged` above). Git's `run_diff_files`
        // reports a stat-dirty entry as modified even when re-hashing the target
        // would yield the same OID; it never re-hashes to clear the change. Hand
        // the hashed OID back as `Modified` and let `collect_changes` suppress it
        // only when the on-disk stat is both trusted and agrees with the index
        // (the racy-clean case). Returning `Unchanged` here on a content match
        // would wrongly hide symlinks after a `read-tree` that zeroed the stat
        // (t0000 diff-files for a known cache/work tree state).
        let target = fs::read_link(abs_path)?;
        let oid = Odb::hash_object_data(ObjectKind::Blob, target.as_os_str().as_bytes());
        return Ok(WorktreeStatus::Modified(MODE_SYMLINK, oid));
    }

    if meta.file_type().is_file() {
        let mode = if meta.permissions().mode() & 0o111 != 0 {
            MODE_EXECUTABLE
        } else {
            MODE_REGULAR
        };
        let data = fs::read(abs_path)?;
        let oid = Odb::hash_object_data(ObjectKind::Blob, &data);
        return Ok(WorktreeStatus::Modified(mode, oid));
    }

    // If it's a directory, check if it's a submodule (has .git subdirectory)
    if meta.file_type().is_dir() {
        let dot_git = abs_path.join(".git");
        if dot_git.exists() {
            // Treat as a submodule (mode 160000)
            let sub_oid = read_submodule_head(abs_path).unwrap_or(index_entry.oid);
            if sub_oid == index_entry.oid {
                let path_str = std::str::from_utf8(&index_entry.path).unwrap_or("");
                let flags = submodule_porcelain_flags(super_worktree, path_str, index_entry.oid);
                if flags.modified || flags.untracked {
                    return Ok(WorktreeStatus::Modified(0o160000, zero_oid()));
                }
                return Ok(WorktreeStatus::Unchanged);
            }
            return Ok(WorktreeStatus::Modified(0o160000, sub_oid));
        }
        if canonicalize_mode(index_entry.mode) == MODE_GITLINK {
            let is_empty = fs::read_dir(abs_path)
                .map(|mut d| d.next().is_none())
                .unwrap_or(false);
            if is_empty {
                return Ok(WorktreeStatus::Unchanged);
            }
        }
    }

    Ok(WorktreeStatus::Missing)
}

/// Read the current HEAD commit OID of a submodule at the given path.
fn read_submodule_head(path: &Path) -> Result<ObjectId> {
    let head_path = path.join(".git").join("HEAD");
    let content = std::fs::read_to_string(&head_path)?;
    let content = content.trim();
    if let Some(refname) = content.strip_prefix("ref: ") {
        let ref_file = path.join(".git").join(refname);
        let ref_content = std::fs::read_to_string(&ref_file)?;
        Ok(ref_content.trim().parse()?)
    } else {
        Ok(content.parse()?)
    }
}

/// Read mode and OID for a working-tree file; returns `None` if missing.
///
/// The OID is computed by hashing the file content so we can detect
/// modifications.  The mode is canonicalized to one of the four Git modes.
fn read_worktree_info(repo: &Repository, abs_path: &Path) -> Result<Option<(u32, ObjectId)>> {
    if let Some(wt) = repo.work_tree.as_deref() {
        if path_has_symlink_parent(wt, abs_path) {
            return Ok(None);
        }
    }
    let meta = match fs::symlink_metadata(abs_path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) if path_component_is_not_directory(&e) => return Ok(None),
        Err(e) => return Err(e.into()),
    };

    let _ = repo;

    if meta.file_type().is_symlink() {
        let target = fs::read_link(abs_path)?;
        let oid = Odb::hash_object_data(ObjectKind::Blob, target.as_os_str().as_bytes());
        return Ok(Some((MODE_SYMLINK, oid)));
    }

    if meta.file_type().is_file() {
        let mode = if meta.permissions().mode() & 0o111 != 0 {
            MODE_EXECUTABLE
        } else {
            MODE_REGULAR
        };
        let data = fs::read(abs_path)?;
        let oid = Odb::hash_object_data(ObjectKind::Blob, &data);
        return Ok(Some((mode, oid)));
    }

    Ok(None)
}

fn path_has_symlink_parent(work_tree: &Path, abs_path: &Path) -> bool {
    let Ok(rel) = abs_path.strip_prefix(work_tree) else {
        return false;
    };
    let mut cur = work_tree.to_path_buf();
    let mut comps = rel.components().peekable();
    while let Some(component) = comps.next() {
        if comps.peek().is_none() {
            break;
        }
        cur.push(component.as_os_str());
        if fs::symlink_metadata(&cur)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

/// Canonicalize a raw file mode to one of the four Git modes.
fn canonicalize_mode(raw_mode: u32) -> u32 {
    match raw_mode & 0o170000 {
        0o120000 => MODE_SYMLINK,
        0o160000 => MODE_GITLINK,
        0o100000 => {
            if raw_mode & 0o111 != 0 {
                MODE_EXECUTABLE
            } else {
                MODE_REGULAR
            }
        }
        _ => MODE_REGULAR,
    }
}

/// Return true if `path` matches any of the given pathspecs.
///
/// An empty pathspec list matches everything.
fn matches_pathspec(path: &str, pathspecs: &[String]) -> bool {
    if pathspecs.is_empty() {
        return true;
    }
    grit_lib::pathspec::matches_pathspec_list_with_context(
        path,
        pathspecs,
        grit_lib::pathspec::PathspecMatchContext::default(),
    )
}

fn prefix_relative_pathspecs_from_cwd(work_tree: &Path, pathspecs: &mut [String]) {
    if pathspecs.is_empty() {
        return;
    }
    let Ok(cwd) = env::current_dir() else {
        return;
    };
    let Ok(rel) = cwd.strip_prefix(work_tree) else {
        return;
    };
    if rel.as_os_str().is_empty() {
        return;
    }
    let prefix = rel.to_string_lossy().replace('\\', "/");
    for spec in pathspecs {
        if spec.is_empty() || spec.starts_with('/') || spec.starts_with(":/") {
            continue;
        }
        *spec = if spec == "." {
            prefix.clone()
        } else {
            format!("{prefix}/{spec}")
        };
    }
}

/// Return the file type category for a mode: 0=blob (regular or executable), 2=symlink, 3=submodule, 4=other.
///
/// Git treats `100644` and `100755` as the same object kind for `diff-files` status (`M`, not `T`).
fn mode_type(mode: u32) -> u32 {
    let m = canonicalize_mode(mode);
    match m {
        MODE_REGULAR | MODE_EXECUTABLE => 0,
        MODE_SYMLINK => 2,
        MODE_GITLINK => 3,
        _ => 4,
    }
}

/// Resolve the index file path, honouring `GIT_INDEX_FILE`.
fn effective_index_path(repo: &Repository) -> Result<PathBuf> {
    if let Ok(raw) = std::env::var("GIT_INDEX_FILE") {
        let path = PathBuf::from(raw);
        if path.is_absolute() {
            return Ok(path);
        }
        let cwd = std::env::current_dir().context("resolving GIT_INDEX_FILE")?;
        return Ok(cwd.join(path));
    }
    Ok(repo.index_path())
}
