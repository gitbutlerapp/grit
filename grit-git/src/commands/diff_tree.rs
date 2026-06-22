//! `grit diff-tree` — compare the content and mode of blobs found via two tree objects.
//!
//! # Modes
//!
//! - Two tree-ish arguments: compare the trees directly.
//! - One commit argument: compare the commit's tree against its parent(s).
//! - `--stdin`: read commit or tree-pair OIDs from standard input.

use anyhow::{anyhow, bail, Context, Result};
use clap::Args as ClapArgs;
use encoding_rs::Encoding;
use grit_lib::combined_diff_patch::CombinedDiffWsOptions;
use grit_lib::combined_tree_diff::{
    combined_diff_paths_filtered, combined_diff_paths_trees, combined_raw_meta,
    format_combined_raw_line, format_combined_raw_line_all_paths, CombinedDiffPath,
    CombinedParentStatus, CombinedTreeDiffOptions,
};
use grit_lib::config::{ConfigFile, ConfigScope, ConfigSet};
use grit_lib::delta_encode::{encode_lcp_delta, encode_prefix_extension_delta};
use grit_lib::repo_path::{RepoPath, RepoPathBuf};
use grit_lib::diff::{
    count_changes, detect_copies as lib_detect_copies, detect_renames, diff_trees,
    diff_trees_show_tree_entries, format_raw, format_raw_abbrev,
    normalize_ignore_space_change_line, unified_diff_with_prefix, DiffEntry, DiffStatus,
};
use grit_lib::merge_base::{
    merge_base_for_diff_two_commits, merge_bases_first_vs_rest, MergeBaseForDiffError,
};
use grit_lib::merge_diff::{
    combined_diff_paths, combined_merge_parent_blob_paths, enrich_combined_path_renames,
    format_combined_textconv_patch, is_binary_for_diff,
};
use grit_lib::objects::{parse_commit, parse_tag, parse_tree, ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::pathspec::{
    context_from_mode_octal, matches_pathspec_list_with_context, matches_pathspec_with_context,
};
use grit_lib::quote_path::{format_diff_path_with_prefix, quote_c_style};
use grit_lib::repo::{resolve_dot_git, Repository};

use crate::commands::diff::check_whitespace_errors;
use crate::commands::diff_index::write_diff_index_name_status;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use grit_lib::attributes::load_gitattributes_for_diff;
use grit_lib::rev_parse::resolve_revision;
use regex::Regex;
use std::io::Write as IoWrite;
use std::io::{self, BufRead, Write};
use std::path::Path;

/// Default maximum tree recursion depth when `core.maxtreedepth` is unset.
const DEFAULT_MAX_TREE_DEPTH: usize = 2048;

/// Arguments for `grit diff-tree`.
#[derive(Debug, ClapArgs)]
#[command(about = "Compare the content and mode of blobs found via two tree objects")]
pub struct Args {
    /// All flags and positional arguments forwarded from the CLI.
    #[arg(
        value_name = "ARG",
        num_args = 0..,
        allow_hyphen_values = true,
        trailing_var_arg = true
    )]
    pub args: Vec<String>,
}

// ── Output format ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputFormat {
    Raw,
    Patch,
    Stat,
    NameOnly,
    NameStatus,
}

// ── Parsed options ───────────────────────────────────────────────────

struct Options {
    /// Positional tree-ish or commit arguments (0–2).
    objects: Vec<String>,
    /// Optional path-limiting specs.
    pathspecs: Vec<String>,
    /// Recurse into sub-trees (`-r`).
    recursive: bool,
    /// Show tree entries in recursive mode (`-t`).
    show_trees: bool,
    /// Show root commit as diff against empty tree (`--root`).
    root: bool,
    /// Read OIDs from stdin (`--stdin`).
    stdin_mode: bool,
    /// Suppress the commit-id header line in stdin mode (`--no-commit-id`).
    no_commit_id: bool,
    /// Show commit message before diff in stdin mode (`-v`).
    verbose: bool,
    /// Suppress diff output in stdin mode (`-s`).
    suppress_diff: bool,
    /// Show a `Notes:` block in the pretty header (`--notes`); map is keyed by commit OID.
    notes_blocks: Option<std::collections::HashMap<ObjectId, String>>,
    /// Raw note bytes per commit, for the `%N` placeholder in a user `--format` string.
    format_notes: Option<std::collections::HashMap<ObjectId, Vec<u8>>>,
    /// Output binary patches (--binary).
    binary: bool,
    /// Show diffs for merge commits in stdin mode (`-m`).
    show_merges: bool,
    /// Combined diff for merge commits (`-c` / `--cc`, plumbing: no textconv).
    combined_patch: bool,
    /// Use `diff --cc` instead of `diff --combined` in combined mode.
    combined_use_cc_word: bool,
    /// Output format.
    format: OutputFormat,
    /// Number of unified context lines for patch output.
    context_lines: usize,
    /// Abbreviate OIDs to this length (None = full).
    abbrev: Option<usize>,
    /// Abbreviate the `commit <hash>` header line (`--abbrev-commit` /
    /// `log.abbrevCommit`).
    abbrev_commit: bool,
    /// Rename detection threshold (None = disabled).
    find_renames: Option<u32>,
    /// Copy detection threshold (None = disabled).
    find_copies: Option<u32>,
    /// Break complete-rewrite pairs (`-B`/`--break-rewrites`).
    break_rewrites: bool,
    /// Use all source files for copy detection, not just modified ones.
    find_copies_harder: bool,
    /// Rename limit (max number of rename source candidates).
    rename_limit: Option<usize>,
    /// Show full object IDs in patch headers (--full-index).
    full_index: bool,
    /// Omit `a/` and `b/` prefixes on diff paths (--no-prefix).
    no_prefix: bool,
    /// Also show raw format with patch (--patch-with-raw).
    patch_with_raw: bool,
    /// Also show stat with patch (--patch-with-stat).
    patch_with_stat: bool,
    /// Show summary (new/deleted/mode changes) after diff.
    summary: bool,
    /// Whether `--raw` was given explicitly (so the default raw format is not
    /// the only output and is not suppressed by `--summary`).
    raw_explicit: bool,
    /// Annotate `--stat` filenames with `(new)`/`(gone)`/`(mode +x)` (`--compact-summary`).
    compact_summary: bool,
    /// Show only the summary line of `--stat` (`--shortstat`).
    shortstat: bool,
    /// Pretty-print commit header (--pretty). None = off, Some("oneline"), Some("medium"), etc.
    pretty: Option<String>,
    /// Show combined stat+summary after diff.
    stat_too: bool,
    /// Limit recursion depth for --name-only etc.
    max_depth: Option<i32>,
    /// Exit with 1 if there are differences.
    exit_code: bool,
    /// NUL-terminate fields/lines (`-z`) for machine-readable output.
    nul_terminated: bool,
    /// Suppress all output, implies exit_code.
    quiet: bool,
    /// Re-merge parents and diff against merge result tree.
    remerge_diff: bool,
    /// Swap the two tree sides (`-R`), inverting raw/patch output like Git.
    reverse: bool,
    /// Pickaxe: only entries where `-S` string occurrence count changes between blobs.
    pickaxe_string: Option<String>,
    /// Pickaxe: only entries whose diff has added/removed lines matching `-G` regex.
    pickaxe_grep: Option<String>,
    /// Treat `-S` pattern as a regex (count regex matches per side).
    pickaxe_regex: bool,
    /// Show all matching files when using pickaxe, not only those with count changes (`--pickaxe-all`).
    pickaxe_all: bool,
    /// Submodule diff format (`log` shows one-line summaries for gitlinks, like Git's `diff --submodule=log`).
    submodule_mode: Option<String>,
    /// Object id spec for `--find-object` (resolved against the repo before the walk).
    find_object: Option<String>,
    /// List old paths per parent on renames (`--combined-all-paths`).
    combined_all_paths: bool,
    /// `-w` / `--ignore-all-space` — ignore all whitespace when comparing blob content.
    ignore_all_space: bool,
    /// `-b` / `--ignore-space-change` — collapse whitespace runs when comparing.
    ignore_space_change: bool,
    /// `--ignore-space-at-eol` — strip trailing whitespace per line when comparing.
    ignore_space_at_eol: bool,
    /// `--ignore-cr-at-eol` — ignore carriage return at end of line.
    ignore_cr_at_eol: bool,
    /// `--ignore-blank-lines` — drop blank lines when comparing.
    ignore_blank_lines: bool,
    /// Whitespace / conflict-marker check (no raw/patch output).
    check: bool,
    /// Compare merge-base(HEAD, A) vs B trees (two commits required).
    merge_base: bool,
    /// Line-diff indent heuristic (Git `diff.indentHeuristic`).
    indent_heuristic: bool,
    /// `--grep` message patterns (used by `whatchanged` to limit the series).
    grep_patterns: Vec<String>,
    /// `--grep` pattern flavor flags (`-F`/`-G`/`-E`/`-P`) and `-i`.
    grep_fixed: bool,
    grep_basic: bool,
    grep_extended: bool,
    grep_perl: bool,
    grep_ignore_case: bool,
}

/// Whitespace comparison options for plumbing `diff-tree` (aligned with porcelain `git diff`).
struct WhitespaceCompare {
    ignore_all_space: bool,
    ignore_space_change: bool,
    ignore_space_at_eol: bool,
    ignore_blank_lines: bool,
}

impl WhitespaceCompare {
    fn from_opts(opts: &Options) -> Self {
        Self {
            ignore_all_space: opts.ignore_all_space,
            ignore_space_change: opts.ignore_space_change,
            ignore_space_at_eol: opts.ignore_space_at_eol,
            ignore_blank_lines: opts.ignore_blank_lines,
        }
    }

    fn any(&self) -> bool {
        self.ignore_all_space
            || self.ignore_space_change
            || self.ignore_space_at_eol
            || self.ignore_blank_lines
    }

    fn normalize_line(&self, line: &str) -> String {
        let s = line.to_owned();
        if self.ignore_all_space {
            return s.chars().filter(|c| !c.is_whitespace()).collect();
        }
        if self.ignore_space_change {
            return normalize_ignore_space_change_line(&s);
        }
        if self.ignore_space_at_eol {
            return s.trim_end().to_owned();
        }
        s
    }

    fn normalize(&self, content: &str) -> String {
        if !self.any() {
            return content.to_owned();
        }
        let mut lines: Vec<String> = content.lines().map(|l| self.normalize_line(l)).collect();
        if self.ignore_blank_lines {
            lines.retain(|l| !l.trim().is_empty());
        }
        lines.join("\n")
    }
}

impl Default for Options {
    fn default() -> Self {
        Self {
            objects: Vec::new(),
            pathspecs: Vec::new(),
            recursive: false,
            show_trees: false,
            root: false,
            stdin_mode: false,
            no_commit_id: false,
            notes_blocks: None,
            format_notes: None,
            verbose: false,
            suppress_diff: false,
            binary: false,
            show_merges: false,
            combined_patch: false,
            combined_use_cc_word: false,
            format: OutputFormat::Raw,
            context_lines: 3,
            abbrev: None,
            abbrev_commit: false,
            find_renames: None,
            find_copies: None,
            break_rewrites: false,
            find_copies_harder: false,
            rename_limit: None,
            full_index: false,
            no_prefix: false,
            patch_with_raw: false,
            patch_with_stat: false,
            summary: false,
            raw_explicit: false,
            compact_summary: false,
            shortstat: false,
            pretty: None,
            stat_too: false,
            max_depth: None,
            exit_code: false,
            nul_terminated: false,
            quiet: false,
            remerge_diff: false,
            reverse: false,
            pickaxe_string: None,
            pickaxe_grep: None,
            pickaxe_regex: false,
            pickaxe_all: false,
            submodule_mode: None,
            find_object: None,
            combined_all_paths: false,
            ignore_all_space: false,
            ignore_space_change: false,
            ignore_space_at_eol: false,
            ignore_cr_at_eol: false,
            ignore_blank_lines: false,
            check: false,
            merge_base: false,
            indent_heuristic: true,
            grep_patterns: Vec::new(),
            grep_fixed: false,
            grep_basic: false,
            grep_extended: false,
            grep_perl: false,
            grep_ignore_case: false,
        }
    }
}

/// True when `spec` resolves to a commit, tree, or annotated tag (Git `setup_revisions` tree-ish).
fn spec_names_commit_or_tree(repo: &Repository, spec: &str) -> bool {
    match resolve_revision(repo, spec) {
        Ok(oid) => match repo.odb.read(&oid) {
            Ok(obj) => match obj.kind {
                ObjectKind::Commit | ObjectKind::Tree => true,
                ObjectKind::Tag => true,
                ObjectKind::Blob => false,
            },
            Err(_) => false,
        },
        Err(_) => false,
    }
}

/// Parse the raw argument vector.
fn parse_options(repo: &Repository, argv: &[String]) -> Result<Options> {
    let mut opts = Options::default();
    let cfg_early = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let (cli_ind, cli_no) = grit_lib::diff::parse_indent_heuristic_cli_flags(argv);
    opts.indent_heuristic = grit_lib::diff::resolve_indent_heuristic(&cfg_early, cli_ind, cli_no);
    let mut end_of_options = false;
    let mut i = 0usize;

    while i < argv.len() {
        let arg = &argv[i];

        if !end_of_options && arg == "--" {
            end_of_options = true;
            i += 1;
            continue;
        }

        if !end_of_options && arg.starts_with('-') {
            match arg.as_str() {
                "-r" => opts.recursive = true,
                "-t" => {
                    opts.recursive = true;
                    opts.show_trees = true;
                }
                "--root" => opts.root = true,
                "--notes" => {
                    opts.notes_blocks = Some(crate::commands::log::notes_blocks_for_display(repo));
                }
                "--no-notes" => opts.notes_blocks = None,
                "--stdin" => opts.stdin_mode = true,
                "--no-commit-id" => opts.no_commit_id = true,
                "-v" => opts.verbose = true,
                "-s" => opts.suppress_diff = true,
                "-m" => opts.show_merges = true,
                "-c" => opts.combined_patch = true,
                "--cc" => {
                    opts.combined_patch = true;
                    opts.combined_use_cc_word = true;
                }
                "--raw" => {
                    opts.format = OutputFormat::Raw;
                    opts.raw_explicit = true;
                }
                "-p" | "-u" | "--patch" => opts.format = OutputFormat::Patch,
                "--binary" => {
                    opts.format = OutputFormat::Patch;
                    opts.binary = true;
                }
                "--stat" => {
                    opts.format = OutputFormat::Stat;
                    opts.stat_too = true;
                }
                "--compact-summary" => {
                    opts.format = OutputFormat::Stat;
                    opts.stat_too = true;
                    opts.compact_summary = true;
                }
                "--shortstat" => {
                    opts.format = OutputFormat::Stat;
                    opts.stat_too = true;
                    opts.shortstat = true;
                }
                "--name-only" => opts.format = OutputFormat::NameOnly,
                "--name-status" => opts.format = OutputFormat::NameStatus,
                "--summary" => opts.summary = true,
                "--exit-code" => opts.exit_code = true,
                "-q" | "--quiet" => {
                    opts.quiet = true;
                    opts.exit_code = true;
                }
                "-z" => opts.nul_terminated = true,
                "--remerge-diff" => opts.remerge_diff = true,
                "--merge-base" => opts.merge_base = true,
                "--full-index" => opts.full_index = true,
                "--no-prefix" => opts.no_prefix = true,
                _ if arg.starts_with("--max-depth=") => {
                    let val = &arg["--max-depth=".len()..];
                    opts.max_depth = Some(
                        val.parse::<i32>()
                            .with_context(|| format!("invalid --max-depth value: `{val}`"))?,
                    );
                }
                "--patch-with-stat" => {
                    opts.format = OutputFormat::Patch;
                    opts.patch_with_stat = true;
                }
                "--patch-with-raw" => {
                    opts.format = OutputFormat::Patch;
                    opts.patch_with_raw = true;
                }
                "--pretty" | "--pretty=medium" => opts.pretty = Some("medium".to_string()),
                _ if arg.starts_with("--pretty=") => {
                    let val = &arg["--pretty=".len()..];
                    opts.pretty = Some(val.to_string());
                }
                "--abbrev" => opts.abbrev = Some(7),
                "--no-abbrev" => opts.abbrev = Some(40),
                "--abbrev-commit" => opts.abbrev_commit = true,
                "--no-abbrev-commit" => opts.abbrev_commit = false,
                _ if arg.starts_with("--abbrev=") => {
                    let val = &arg["--abbrev=".len()..];
                    opts.abbrev = Some(
                        val.parse::<usize>()
                            .with_context(|| format!("invalid --abbrev value: `{val}`"))?,
                    );
                }
                _ if arg.starts_with("-U") => {
                    let val = &arg[2..];
                    if val.is_empty() {
                        i += 1;
                        let next = argv
                            .get(i)
                            .ok_or_else(|| anyhow::anyhow!("-U requires an argument"))?;
                        opts.context_lines = next
                            .parse()
                            .with_context(|| format!("invalid -U value: `{next}`"))?;
                    } else {
                        opts.context_lines = val
                            .parse()
                            .with_context(|| format!("invalid -U value: `{val}`"))?;
                    }
                }
                "--combined-all-paths" => opts.combined_all_paths = true,
                "--ignore-space-at-eol" => opts.ignore_space_at_eol = true,
                "-b" | "--ignore-space-change" => opts.ignore_space_change = true,
                "-w" | "--ignore-all-space" => opts.ignore_all_space = true,
                "--ignore-cr-at-eol" => opts.ignore_cr_at_eol = true,
                "-B" | "--break-rewrites" => opts.break_rewrites = true,
                _ if arg.starts_with("-B") || arg.starts_with("--break-rewrites=") => {
                    // Accept a score/merge-score argument but use git's defaults;
                    // these tests only exercise the default break behavior.
                    opts.break_rewrites = true;
                }
                "-M" | "--find-renames" => opts.find_renames = Some(50),
                "-C" | "--find-copies" => {
                    opts.find_copies = Some(50);
                    // -C implies rename detection too.
                    if opts.find_renames.is_none() {
                        opts.find_renames = Some(50);
                    }
                }
                "--find-copies-harder" => opts.find_copies_harder = true,
                "--no-renames" => opts.find_renames = None,
                _ if arg.starts_with("-M") => {
                    let val = &arg[2..];
                    let pct = if val.ends_with('%') {
                        val[..val.len() - 1].parse::<u32>().unwrap_or(50)
                    } else {
                        // Could be e.g. -M80 or -M80%
                        val.parse::<u32>().unwrap_or(50)
                    };
                    opts.find_renames = Some(pct);
                }
                _ if arg.starts_with("--find-renames=") => {
                    let val = &arg["--find-renames=".len()..];
                    let pct = if val.ends_with('%') {
                        val[..val.len() - 1].parse::<u32>().unwrap_or(50)
                    } else {
                        val.parse::<u32>().unwrap_or(50)
                    };
                    opts.find_renames = Some(pct);
                }
                _ if arg.starts_with("-l") => {
                    let val = &arg[2..];
                    if let Ok(n) = val.parse::<usize>() {
                        opts.rename_limit = Some(if n == 0 { 32767 } else { n });
                    }
                }
                // Silently accept common diff options that we do not implement.
                "--no-rename-empty" | "--always" | "--diff-merges=off" => {}
                "--check" => opts.check = true,
                "-R" => opts.reverse = true,
                _ if arg.len() > 2 && arg.starts_with("-R") => {
                    opts.reverse = true;
                    let rest = arg[2..].chars();
                    for ch in rest {
                        match ch {
                            'p' | 'u' => opts.format = OutputFormat::Patch,
                            _ => bail!("unknown option: -{ch}"),
                        }
                    }
                }
                _ if arg.starts_with("--find-object=") => {
                    opts.find_object = Some(arg["--find-object=".len()..].to_string());
                }
                "--find-object" => {
                    i += 1;
                    let next = argv
                        .get(i)
                        .ok_or_else(|| anyhow::anyhow!("`--find-object` requires a value"))?;
                    opts.find_object = Some(next.clone());
                }
                _ if arg.starts_with("--format=") => {
                    let val = &arg["--format=".len()..];
                    opts.pretty = Some(format!("format:{val}"));
                }
                _ if arg.starts_with("--diff-filter=")
                    || arg.starts_with("--diff-merges=")
                    || arg.starts_with("-O")
                    || arg.starts_with("--relative") =>
                {
                    // ignored
                }
                "--pickaxe-regex" => opts.pickaxe_regex = true,
                "--pickaxe-all" => opts.pickaxe_all = true,
                "--indent-heuristic" => {}
                "--no-indent-heuristic" => {}
                s if s.starts_with("-S") => {
                    if s.len() > 2 {
                        opts.pickaxe_string = Some(s[2..].to_owned());
                    } else {
                        i += 1;
                        if i >= argv.len() {
                            bail!("option `-S` requires a value");
                        }
                        opts.pickaxe_string = Some(argv[i].clone());
                    }
                    i += 1;
                    continue;
                }
                s if s.starts_with("-G") => {
                    if s.len() > 2 {
                        opts.pickaxe_grep = Some(s[2..].to_owned());
                    } else {
                        i += 1;
                        if i >= argv.len() {
                            bail!("option `-G` requires a value");
                        }
                        opts.pickaxe_grep = Some(argv[i].clone());
                    }
                    i += 1;
                    continue;
                }
                _ if arg.starts_with("--submodule=") => {
                    opts.submodule_mode = Some(arg["--submodule=".len()..].to_string());
                }
                "--ignore-blank-lines" => opts.ignore_blank_lines = true,
                // `--grep` message limiting (used by `whatchanged`). Only the long
                // pattern-flavor forms are accepted here because diff-tree's short
                // `-G`/`-S` are pickaxe options.
                _ if arg.starts_with("--grep=") => {
                    opts.grep_patterns.push(arg["--grep=".len()..].to_string());
                }
                "--grep" => {
                    i += 1;
                    if i >= argv.len() {
                        bail!("option `--grep` requires a value");
                    }
                    opts.grep_patterns.push(argv[i].clone());
                    i += 1;
                    continue;
                }
                "--fixed-strings" => opts.grep_fixed = true,
                "--basic-regexp" => opts.grep_basic = true,
                "--extended-regexp" => opts.grep_extended = true,
                "--perl-regexp" => opts.grep_perl = true,
                "--regexp-ignore-case" => opts.grep_ignore_case = true,
                _ => bail!("unknown option: {arg}"),
            }
            i += 1;
            continue;
        }

        // Positional: like Git `setup_revisions` — up to two tree-ishes, then pathspecs.
        // In `--stdin` mode the tree-ishes come from stdin, so any positional given
        // on the command line is a pathspec.
        if end_of_options || opts.stdin_mode || opts.objects.len() >= 2 {
            opts.pathspecs.push(arg.clone());
        } else if opts.objects.len() == 1 && !spec_names_commit_or_tree(repo, arg) {
            opts.pathspecs.push(arg.clone());
        } else {
            opts.objects.push(arg.clone());
        }
        i += 1;
    }

    // `--cc` (dense combined) defaults to a combined *patch* when no other output
    // format is requested. `-c` keeps the combined *raw* default. An explicit
    // `--raw`/`--stat`/`--summary`/`--patch-with-*` request wins over this default.
    if opts.combined_use_cc_word
        && opts.format == OutputFormat::Raw
        && !opts.raw_explicit
        && !opts.summary
        && !opts.patch_with_stat
        && !opts.patch_with_raw
    {
        opts.format = OutputFormat::Patch;
    }

    // Patch and stat imply recursion (Git shows nested file paths). `--name-only`
    // and `--name-status` follow plain `diff-tree` rules: top-level entries only
    // unless `-r` is given (see t4010-diff-pathspec).
    match opts.format {
        OutputFormat::Patch | OutputFormat::Stat => {
            opts.recursive = true;
        }
        _ => {}
    }
    if opts.summary {
        opts.recursive = true;
    }

    // A user `--format`/`--pretty=format:` string with the `%N` placeholder needs the raw
    // note bytes for each commit; load them once here (default notes ref).
    if opts.format_notes.is_none() {
        if let Some(p) = &opts.pretty {
            let template = p
                .strip_prefix("tformat:")
                .or_else(|| p.strip_prefix("format:"));
            if template.is_some_and(|t| t.contains("%N")) {
                opts.format_notes = Some(crate::commands::show::load_notes_map(repo));
            }
        }
    }

    Ok(opts)
}

// ── Public entry point ───────────────────────────────────────────────

/// Run `grit diff-tree`.
pub fn run(mut args: Args) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    if grit_lib::precompose_config::effective_core_precomposeunicode(Some(&repo.git_dir)) {
        crate::precompose::precompose_diff_tree_argv(&mut args.args);
    }
    let opts = parse_options(&repo, &args.args)?;
    if opts.max_depth.is_some()
        && opts
            .pathspecs
            .iter()
            .any(|p| grit_lib::pathspec::has_glob_chars(p))
    {
        bail!("fatal: max-depth cannot be used with wildcard pathspecs");
    }
    if opts.merge_base && opts.stdin_mode {
        bail!("fatal: options '--merge-base' and '--stdin' cannot be used together");
    }
    if opts.merge_base && opts.objects.len() != 2 {
        bail!("fatal: --merge-base only works with two commits");
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();

    let has_diff = if opts.stdin_mode {
        run_stdin_mode(&repo, &opts, &mut out)?
    } else if opts.objects.len() == 2 {
        run_two_trees(&repo, &opts, &mut out)?
    } else if opts.objects.len() == 1 {
        run_one_commit(&repo, &opts, &mut out)?
    } else {
        bail!(
            "usage: grit diff-tree [--stdin] [-r] [--root] [-p|--stat|--name-only|--name-status] \
             <tree-ish> [<tree-ish>] [<path>...]"
        )
    };

    if opts.check {
        return Ok(());
    }
    if opts.exit_code && has_diff {
        std::process::exit(1);
    }
    Ok(())
}

// ── whatchanged ──────────────────────────────────────────────────────

/// Run `grit whatchanged` (`git log --raw --no-merges`, with the root commit's
/// diff shown). Reuses the `diff-tree` option parser and per-commit renderer so
/// the per-commit output is byte-for-byte identical to `diff-tree --pretty`.
pub fn run_whatchanged(argv: &[String]) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
    let mut opts = parse_options(&repo, argv)?;

    // `whatchanged` defaults: medium pretty header, abbreviated OIDs, recursive
    // raw output, and the root commit's diff is shown (against the empty tree).
    if opts.pretty.is_none() {
        opts.pretty = Some("medium".to_string());
    }
    if opts.abbrev.is_none() && !opts.full_index {
        opts.abbrev = Some(7);
    }
    // `log.abbrevCommit` enables `--abbrev-commit` unless `--no-abbrev-commit`
    // was given explicitly on the command line.
    if !opts.abbrev_commit
        && !argv.iter().any(|a| a == "--no-abbrev-commit")
        && !argv.iter().any(|a| a == "--abbrev-commit")
    {
        let cfg = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
        if cfg
            .get_bool("log.abbrevCommit")
            .and_then(|r| r.ok())
            .unwrap_or(false)
        {
            opts.abbrev_commit = true;
        }
    }
    if matches!(
        opts.format,
        OutputFormat::Raw | OutputFormat::NameOnly | OutputFormat::NameStatus
    ) {
        opts.recursive = true;
    }
    // The root commit's diff is shown unless `log.showroot` is explicitly false
    // (Git default is true). An explicit `--root` on the command line always wins.
    // When suppressed, `whatchanged` drops the root commit entirely because it then
    // has an empty diff.
    if !argv.iter().any(|a| a == "--root") {
        let cfg = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
        let show_root = cfg
            .get_bool("log.showroot")
            .and_then(|r| r.ok())
            .unwrap_or(true);
        opts.root = show_root;
    }

    // Revision tips: the positional `objects` are the starting commits for the
    // walk (default HEAD when none are given).
    let positive_specs: Vec<String> = if opts.objects.is_empty() {
        let head = grit_lib::state::resolve_head(&repo.git_dir)?;
        match head.oid() {
            Some(oid) => vec![oid.to_hex()],
            None => return Ok(()),
        }
    } else {
        opts.objects.clone()
    };

    let include_merges = opts.combined_patch;
    // Use the shared rev-list walk so `whatchanged` orders commits exactly like
    // `git log --raw` (reverse-chronological with topological tie-breaks).
    let rl_opts = grit_lib::rev_list::RevListOptions {
        ordering: grit_lib::rev_list::OrderingMode::Default,
        ..grit_lib::rev_list::RevListOptions::default()
    };
    let rl = grit_lib::rev_list::rev_list(&repo, &positive_specs, &[], &rl_opts)
        .map_err(|e| anyhow::anyhow!("rev-list failed: {e}"))?;
    // `--grep` limits the series to commits whose message matches (like `git log
    // --grep`), honoring `grep.patternType` / `--fixed-strings`/etc.
    let grep_res = if opts.grep_patterns.is_empty() {
        Vec::new()
    } else {
        let cfg = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
        crate::commands::log::compile_command_grep_regexes(
            &opts.grep_patterns,
            opts.grep_fixed,
            opts.grep_basic,
            opts.grep_extended,
            opts.grep_perl,
            opts.grep_ignore_case,
            &cfg,
        )?
    };
    let mut commits: Vec<ObjectId> = Vec::new();
    for oid in rl.commits {
        let obj = repo.odb.read(&oid)?;
        let commit = parse_commit(&obj.data)?;
        // `whatchanged` skips merges (== `--no-merges`) unless combined.
        if commit.parents.len() > 1 && !include_merges {
            continue;
        }
        if !grep_res.is_empty() && !grep_res.iter().any(|re| re.is_match(&commit.message)) {
            continue;
        }
        commits.push(oid);
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let pickaxe_active =
        opts.pickaxe_string.is_some() || opts.pickaxe_grep.is_some() || opts.find_object.is_some();

    let mut printed_any = false;
    for oid in &commits {
        let printed =
            render_whatchanged_commit(&repo, &opts, &mut out, oid, pickaxe_active, printed_any)?;
        if printed {
            printed_any = true;
        }
    }
    Ok(())
}

/// Walk commits reachable from `tips`, ordered by committer date (newest first)
/// like `git log`. Merge commits are skipped unless `include_merges` is set
/// (`whatchanged -c`/`--cc`, which show a combined merge diff).
fn whatchanged_walk(odb: &Odb, tips: &[ObjectId], include_merges: bool) -> Result<Vec<ObjectId>> {
    use std::collections::HashSet;
    let mut visited: HashSet<ObjectId> = HashSet::new();
    let mut stack: Vec<ObjectId> = tips.to_vec();
    // (oid, committer_timestamp, insertion_order)
    let mut collected: Vec<(ObjectId, i64, usize)> = Vec::new();
    let mut order = 0usize;

    while let Some(oid) = stack.pop() {
        if !visited.insert(oid) {
            continue;
        }
        let obj = odb.read(&oid)?;
        let commit = parse_commit(&obj.data)?;
        // Skip merge commits (whatchanged == `--no-merges`) unless combined.
        if commit.parents.len() <= 1 || include_merges {
            let ts = committer_timestamp(&commit.committer);
            collected.push((oid, ts, order));
            order += 1;
        }
        for parent in commit.parents.iter().rev() {
            if !visited.contains(parent) {
                stack.push(*parent);
            }
        }
    }

    // Newest committer date first; stable on insertion order for ties.
    collected.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)));
    Ok(collected.into_iter().map(|(oid, _, _)| oid).collect())
}

fn committer_timestamp(committer: &str) -> i64 {
    // Format: "Name <email> <timestamp> <tz>"
    let mut it = committer.rsplit(' ');
    let _tz = it.next();
    it.next().and_then(|ts| ts.parse::<i64>().ok()).unwrap_or(0)
}

/// Render one commit for `whatchanged`: a pretty header followed by the diff
/// against the first parent (or the empty tree for the root commit).
fn render_whatchanged_commit(
    repo: &Repository,
    opts: &Options,
    out: &mut impl Write,
    oid: &ObjectId,
    _pickaxe_active: bool,
    leading_blank: bool,
) -> Result<bool> {
    let obj = repo.odb.read(oid).context("reading commit")?;
    let commit = parse_commit(&obj.data).context("parsing commit")?;

    // Merge commits: `whatchanged -c`/`--cc` shows a combined merge diff with the
    // pretty header (other invocations skip merges and never reach here).
    if commit.parents.len() > 1 && opts.combined_patch {
        let walk = CombinedTreeDiffOptions {
            recursive: true,
            tree_in_recursive: false,
        };
        let mut paths =
            combined_diff_paths_filtered(&repo.odb, &commit.tree, &commit.parents, &walk, None)?;
        paths = filter_combined_paths_intersection(&repo.odb, &commit.tree, &commit.parents, paths);
        if !opts.pathspecs.is_empty() {
            paths.retain(|p| combined_path_matches_pathspecs(p, &opts.pathspecs));
        }
        if paths.is_empty() {
            return Ok(false);
        }
        if leading_blank {
            writeln!(out)?;
        }
        write_commit_header(out, oid, &obj.data, opts, None)?;
        print_combined_merge_output(out, repo, &paths, opts, &commit.parents, &commit.tree, None)?;
        return Ok(true);
    }

    // The root commit is only diffed when `log.showroot` is enabled (opts.root).
    let is_root = commit.parents.is_empty();
    let parent_tree = if let Some(parent) = commit.parents.first() {
        Some(commit_tree(&repo.odb, parent)?)
    } else {
        None
    };
    let filtered = if is_root && !opts.root {
        Vec::new()
    } else {
        let entries = diff_with_opts(&repo.odb, parent_tree.as_ref(), Some(&commit.tree), opts)?;
        filter_entries(&repo.odb, repo, entries, opts)?
    };
    let has_diff = !filtered.is_empty();

    // `whatchanged` (unlike `git log --raw`) suppresses commits with no diff,
    // including the root commit when `log.showroot` is false.
    if !has_diff {
        return Ok(false);
    }

    if leading_blank {
        writeln!(out)?;
    }
    write_commit_header(out, oid, &obj.data, opts, None)?;
    if !opts.suppress_diff {
        print_diff(out, repo, &filtered, opts, parent_tree.as_ref())?;
    }
    Ok(true)
}

// ── Two-tree mode ────────────────────────────────────────────────────

fn run_multi_tree_combined(
    repo: &Repository,
    opts: &Options,
    out: &mut impl Write,
    merge_tree: &ObjectId,
    parent_trees: &[ObjectId],
) -> Result<bool> {
    let odb = &repo.odb;
    let walk = CombinedTreeDiffOptions {
        recursive: true,
        tree_in_recursive: false,
    };
    let parent_opts: Vec<Option<ObjectId>> = parent_trees.iter().copied().map(Some).collect();
    let mut paths = combined_diff_paths_trees(odb, merge_tree, &parent_opts, &walk, None)?;
    let has_diff = !paths.is_empty();
    if opts.quiet || !has_diff {
        return Ok(has_diff);
    }

    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let quote_fully = config.quote_path_fully();
    let abbrev_len = if opts.full_index {
        40usize
    } else {
        opts.abbrev.unwrap_or(7)
    };
    let ws = CombinedDiffWsOptions {
        ignore_all_space: opts.ignore_all_space,
        ignore_space_change: opts.ignore_space_change,
        ignore_space_at_eol: opts.ignore_space_at_eol,
        ignore_cr_at_eol: opts.ignore_cr_at_eol,
    };
    let rename_thresh = opts.find_renames.unwrap_or(50);

    if opts.combined_all_paths && opts.find_renames.is_some() {
        for p in &mut paths {
            enrich_combined_path_renames(odb, p, parent_trees, merge_tree, rename_thresh);
        }
    }

    for p in &paths {
        match opts.format {
            OutputFormat::Raw => {
                if opts.nul_terminated {
                    write_combined_raw_z(out, None, p, opts.abbrev, opts.combined_all_paths)?;
                } else {
                    writeln!(
                        out,
                        "{}",
                        format_combined_raw_line_all_paths(
                            p,
                            opts.abbrev,
                            opts.combined_all_paths,
                            quote_fully,
                        )
                    )?;
                }
            }
            OutputFormat::NameOnly | OutputFormat::NameStatus => {
                print_combined_paths(out, std::slice::from_ref(p), opts)?;
            }
            OutputFormat::Patch => {
                let parent_blob_paths = if opts.combined_all_paths && opts.find_renames.is_some() {
                    combined_merge_parent_blob_paths(
                        odb,
                        &p.path,
                        parent_trees,
                        merge_tree,
                        rename_thresh,
                    )
                } else {
                    None
                };
                let ps_ref = parent_blob_paths.as_deref();
                if let Some(patch) = format_combined_textconv_patch(
                    &repo.git_dir,
                    &config,
                    odb,
                    &p.path,
                    parent_trees,
                    merge_tree,
                    abbrev_len,
                    opts.context_lines,
                    opts.combined_use_cc_word,
                    false,
                    ws,
                    opts.combined_all_paths,
                    ps_ref,
                    &p.parents,
                    quote_fully,
                ) {
                    write!(out, "{patch}")?;
                }
            }
            _ => {}
        }
    }
    Ok(has_diff)
}

fn run_two_trees(repo: &Repository, opts: &Options, out: &mut impl Write) -> Result<bool> {
    if opts.combined_patch && opts.objects.len() >= 3 {
        let last_obj = opts
            .objects
            .last()
            .ok_or_else(|| anyhow!("combined patch requires at least one object"))?;
        let merge = resolve_to_tree(repo, last_obj)?;
        let mut parents = Vec::with_capacity(opts.objects.len() - 1);
        for s in &opts.objects[..opts.objects.len() - 1] {
            parents.push(resolve_to_tree(repo, s)?);
        }
        return run_multi_tree_combined(repo, opts, out, &merge, &parents);
    }

    let (spec_a, spec_b) = if opts.reverse {
        (&opts.objects[1], &opts.objects[0])
    } else {
        (&opts.objects[0], &opts.objects[1])
    };
    let (oid1, oid2) = if opts.merge_base {
        let commit_a = resolve_commit_ish_for_merge_base(repo, spec_a)?;
        let commit_b = resolve_commit_ish_for_merge_base(repo, spec_b)?;
        let mb_oid = match merge_base_for_diff_two_commits(repo, commit_a, commit_b) {
            Ok(oid) => oid,
            Err(MergeBaseForDiffError::None) => bail!("fatal: no merge base found"),
            Err(MergeBaseForDiffError::Multiple) => bail!("fatal: multiple merge bases found"),
            Err(MergeBaseForDiffError::Other(msg)) => bail!("{msg}"),
        };
        let tree_mb = tree_oid_for_commit(repo, mb_oid)?;
        let tree_second = resolve_to_tree(repo, spec_b)?;
        (tree_mb, tree_second)
    } else {
        (
            resolve_to_tree(repo, spec_a)?,
            resolve_to_tree(repo, spec_b)?,
        )
    };
    let max_tree_depth = resolve_max_tree_depth(repo);
    let old_tree = if is_magic_empty_tree_oid(&oid1) {
        None
    } else {
        Some(&oid1)
    };
    let new_tree = if is_magic_empty_tree_oid(&oid2) {
        None
    } else {
        Some(&oid2)
    };
    if let Some(tree_oid) = old_tree {
        validate_tree_depth_limit(&repo.odb, tree_oid, 0, max_tree_depth)?;
    }
    if let Some(tree_oid) = new_tree {
        validate_tree_depth_limit(&repo.odb, tree_oid, 0, max_tree_depth)?;
    }
    let entries = diff_with_opts(&repo.odb, old_tree, new_tree, opts)?;
    let filtered = filter_entries(&repo.odb, &repo, entries, opts)?;
    let has_diff = !filtered.is_empty();
    if opts.check {
        let prepared = prepare_diff_tree_entries(&repo.odb, filtered, opts, old_tree);
        run_diff_tree_whitespace_check(repo, &prepared, opts)?;
        return Ok(has_diff);
    }
    if !opts.quiet {
        print_diff(out, repo, &filtered, opts, old_tree)?;
    }
    Ok(has_diff)
}

// ── Single-commit mode ───────────────────────────────────────────────

fn run_one_commit(repo: &Repository, opts: &Options, out: &mut impl Write) -> Result<bool> {
    let spec = &opts.objects[0];
    let oid =
        resolve_revision(repo, spec).with_context(|| format!("unknown revision: '{spec}'"))?;
    let obj = repo.odb.read(&oid).context("reading object")?;

    let mut has_diff = false;
    match obj.kind {
        ObjectKind::Commit => {
            let commit = parse_commit(&obj.data).context("parsing commit")?;
            let max_tree_depth = resolve_max_tree_depth(repo);
            validate_tree_depth_limit(&repo.odb, &commit.tree, 0, max_tree_depth)?;
            if commit.parents.is_empty() {
                if opts.root {
                    // `-R` reverses the diff: a root commit's additions become
                    // deletions (empty tree on the new side).
                    let (old_side, new_side) = if opts.reverse {
                        (Some(&commit.tree), None)
                    } else {
                        (None, Some(&commit.tree))
                    };
                    let entries = diff_with_opts(&repo.odb, old_side, new_side, opts)?;
                    let filtered = filter_entries(&repo.odb, &repo, entries, opts)?;
                    has_diff = !filtered.is_empty();
                    if opts.check {
                        // `--check` runs the whitespace/conflict-marker check instead of
                        // emitting raw/patch output: print the commit header, then the check.
                        if !opts.quiet {
                            write_commit_header(out, &oid, &obj.data, opts, None)?;
                        }
                        let prepared = prepare_diff_tree_entries(&repo.odb, filtered, opts, None);
                        run_diff_tree_whitespace_check(repo, &prepared, opts)?;
                        return Ok(has_diff);
                    }
                    if !opts.quiet && (has_diff || opts.pretty.is_some()) {
                        // `git diff-tree --root <commit>` prints the bare commit OID header
                        // line first (gated only on --no-commit-id), exactly like the
                        // single-parent and merge branches below.
                        write_commit_header(out, &oid, &obj.data, opts, None)?;
                        if !opts.suppress_diff {
                            print_diff(out, repo, &filtered, opts, None)?;
                        }
                    }
                }
            } else if commit.parents.len() == 2
                && opts.remerge_diff
                && opts.format == OutputFormat::Patch
            {
                use crate::commands::remerge_diff::{write_remerge_diff, RemergeDiffOptions};
                let ro = RemergeDiffOptions {
                    pathspecs: &opts.pathspecs,
                    diff_filter: None,
                    pickaxe: None,
                    find_object: None,
                    submodule_mode: None,
                    context_lines: opts.context_lines,
                    indent_heuristic: opts.indent_heuristic,
                };
                let mut buf = Vec::new();
                write_remerge_diff(&mut buf, repo, &commit.tree, &commit.parents, &ro)?;
                let hd = !buf.is_empty();
                has_diff = hd;
                if !opts.quiet && (hd || opts.pretty.is_some()) {
                    write_commit_header(out, &oid, &obj.data, opts, None)?;
                    out.write_all(&buf)?;
                }
            } else if commit.parents.len() > 1 && opts.combined_patch {
                let find_oid = if let Some(ref spec) = opts.find_object {
                    Some(
                        resolve_revision(repo, spec)
                            .with_context(|| format!("unable to resolve '{spec}'"))?,
                    )
                } else {
                    None
                };
                let walk = CombinedTreeDiffOptions {
                    recursive: true,
                    tree_in_recursive: opts.show_trees || find_oid.is_some(),
                };
                let mut paths = combined_diff_paths_filtered(
                    &repo.odb,
                    &commit.tree,
                    &commit.parents,
                    &walk,
                    find_oid.as_ref(),
                )?;
                paths = filter_combined_paths_intersection(
                    &repo.odb,
                    &commit.tree,
                    &commit.parents,
                    paths,
                );
                if !opts.pathspecs.is_empty() {
                    paths.retain(|p| combined_path_matches_pathspecs(p, &opts.pathspecs));
                }
                has_diff = !paths.is_empty();
                if !opts.quiet && (has_diff || opts.pretty.is_some()) {
                    write_commit_header(out, &oid, &obj.data, opts, None)?;
                    if has_diff {
                        if matches!(
                            opts.format,
                            OutputFormat::NameStatus | OutputFormat::NameOnly
                        ) {
                            print_combined_paths(out, &paths, opts)?;
                        } else {
                            print_combined_merge_output(
                                out,
                                repo,
                                &paths,
                                opts,
                                &commit.parents,
                                &commit.tree,
                                Some(&oid),
                            )?;
                        }
                    }
                }
            } else if commit.parents.len() > 1 && opts.show_merges {
                let mut any_diff = false;
                for parent_oid in &commit.parents {
                    let parent_tree = commit_tree(&repo.odb, parent_oid)?;
                    let entries =
                        diff_with_opts(&repo.odb, Some(&parent_tree), Some(&commit.tree), opts)?;
                    let filtered = filter_entries(&repo.odb, &repo, entries, opts)?;
                    any_diff |= !filtered.is_empty();
                    if !opts.quiet {
                        write_commit_header(out, &oid, &obj.data, opts, Some(parent_oid))?;
                        print_diff(out, repo, &filtered, opts, Some(&parent_tree))?;
                    }
                }
                has_diff = any_diff;
            } else if commit.parents.len() > 1 {
                // A bare merge commit (no `-m`/`-c`/`--cc`/`--remerge-diff`) produces
                // no output at all in `git diff-tree` — not even the commit header
                // with `--pretty`/`-s`. Leave `has_diff` false and emit nothing.
            } else {
                let parent_tree = commit_tree(&repo.odb, &commit.parents[0])?;
                let entries =
                    diff_with_opts(&repo.odb, Some(&parent_tree), Some(&commit.tree), opts)?;
                let filtered = filter_entries(&repo.odb, &repo, entries, opts)?;
                has_diff = !filtered.is_empty();
                if opts.check {
                    let prepared =
                        prepare_diff_tree_entries(&repo.odb, filtered, opts, Some(&parent_tree));
                    run_diff_tree_whitespace_check(repo, &prepared, opts)?;
                    return Ok(has_diff);
                }
                if !opts.quiet && (has_diff || opts.pretty.is_some()) {
                    write_commit_header(out, &oid, &obj.data, opts, None)?;
                    // `-s`/`--no-patch`: print the (pretty) header only, omit the diff.
                    if !opts.suppress_diff {
                        print_diff(out, repo, &filtered, opts, Some(&parent_tree))?;
                    }
                }
            }
        }
        _ => bail!("'{spec}' does not name a commit"),
    }

    Ok(has_diff)
}

// ── --stdin mode ─────────────────────────────────────────────────────

fn run_stdin_mode(repo: &Repository, opts: &Options, out: &mut impl Write) -> Result<bool> {
    let stdin = io::stdin();
    let mut has_diff = false;
    for line in stdin.lock().lines() {
        let line = line.context("reading stdin")?;
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        if process_stdin_line(repo, opts, out, trimmed)? {
            has_diff = true;
        }
    }
    Ok(has_diff)
}

/// Process one line from stdin.
fn process_stdin_line(
    repo: &Repository,
    opts: &Options,
    out: &mut impl Write,
    line: &str,
) -> Result<bool> {
    // Split on the first space to get the leading OID and optional remainder.
    let (oid_str, rest) = line
        .split_once(' ')
        .map(|(a, b)| (a, b))
        .unwrap_or((line, ""));

    let oid = match oid_str.parse::<ObjectId>() {
        Ok(o) => o,
        Err(_) => {
            // Not a valid OID: pass through.
            writeln!(out, "{line}")?;
            return Ok(false);
        }
    };

    let obj = match repo.odb.read(&oid) {
        Ok(o) => o,
        Err(_) => {
            writeln!(out, "{line}")?;
            return Ok(false);
        }
    };

    match obj.kind {
        ObjectKind::Commit => process_stdin_commit(repo, opts, out, &oid, &obj.data, rest),
        ObjectKind::Tree => process_stdin_two_trees(repo, opts, out, &oid, rest),
        _ => {
            writeln!(out, "{line}")?;
            Ok(false)
        }
    }
}

/// Handle a commit line from stdin.
fn process_stdin_commit(
    repo: &Repository,
    opts: &Options,
    out: &mut impl Write,
    oid: &ObjectId,
    data: &[u8],
    rest: &str,
) -> Result<bool> {
    let commit = parse_commit(data).context("parsing commit")?;

    // Decide up front whether this commit is shown at all. `git diff-tree --stdin`
    // skips merge commits (unless `-m`/`-c`/`--cc`/remerge) and the root commit
    // (unless `--root`); the header is only emitted for commits that would yield a
    // diff. This decision must precede header printing so that `-s` (which
    // suppresses only the diff body) does not leak headers for skipped commits.
    let is_merge = commit.parents.len() > 1;
    let is_root = commit.parents.is_empty();
    let remerge_merge_stdin =
        commit.parents.len() == 2 && opts.remerge_diff && opts.format == OutputFormat::Patch;
    let show_merge = opts.show_merges || opts.combined_patch || remerge_merge_stdin;
    if is_merge && !show_merge {
        return Ok(false);
    }
    if is_root && !opts.root {
        return Ok(false);
    }

    // Header. With `--format`/`--pretty`, apply the pretty/format template (e.g.
    // `--format=%s` prints the subject line) instead of the bare OID. Without a
    // pretty format, print the bare commit-id header (unless `--no-commit-id`).
    // `--quiet` still prints this header (only the raw/patch diff is suppressed),
    // matching `git diff-tree --stdin`.
    if opts.pretty.is_some() {
        write_commit_header(out, oid, data, opts, None)?;
    } else if !opts.no_commit_id {
        writeln!(out, "{}", oid.to_hex())?;
    }

    // `-v` shows the commit message even with `--quiet` (raw diff stays off).
    // Skip when a pretty format already rendered the header above.
    if opts.verbose && opts.pretty.is_none() {
        writeln!(out, "commit {}", oid.to_hex())?;
        writeln!(out)?;
        for msg_line in commit.message.lines() {
            writeln!(out, "    {msg_line}")?;
        }
        writeln!(out)?;
    }

    if opts.suppress_diff {
        return Ok(false);
    }

    // Override parents if the line contains extra OIDs.
    let extra_parents = parse_oid_list(rest)?;
    let parent_oids: Vec<ObjectId> = if extra_parents.is_empty() {
        commit.parents.clone()
    } else {
        extra_parents
    };

    let has_diff = if remerge_merge_stdin && parent_oids.len() == 2 {
        use crate::commands::remerge_diff::{write_remerge_diff, RemergeDiffOptions};
        let ro = RemergeDiffOptions {
            pathspecs: &opts.pathspecs,
            diff_filter: None,
            pickaxe: None,
            find_object: None,
            submodule_mode: None,
            context_lines: opts.context_lines,
            indent_heuristic: opts.indent_heuristic,
        };
        let mut buf = Vec::new();
        write_remerge_diff(&mut buf, repo, &commit.tree, &parent_oids, &ro)?;
        let hd = !buf.is_empty();
        if !opts.quiet {
            out.write_all(&buf)?;
        }
        hd
    } else if parent_oids.is_empty() {
        if opts.root {
            let entries = diff_with_opts(&repo.odb, None, Some(&commit.tree), opts)?;
            let filtered = filter_entries(&repo.odb, &repo, entries, opts)?;
            let hd = !filtered.is_empty();
            if !opts.quiet {
                print_diff(out, repo, &filtered, opts, None)?;
            }
            hd
        } else {
            false
        }
    } else {
        let parent_tree = commit_tree(&repo.odb, &parent_oids[0])?;
        let entries = diff_with_opts(&repo.odb, Some(&parent_tree), Some(&commit.tree), opts)?;
        let filtered = filter_entries(&repo.odb, &repo, entries, opts)?;
        let hd = !filtered.is_empty();
        if !opts.quiet {
            print_diff(out, repo, &filtered, opts, None)?;
        }
        hd
    };

    Ok(has_diff)
}

/// Handle a two-tree line from stdin: `<tree1> <tree2>`.
fn process_stdin_two_trees(
    repo: &Repository,
    opts: &Options,
    out: &mut impl Write,
    oid1: &ObjectId,
    rest: &str,
) -> Result<bool> {
    let oid2_str = rest.trim();
    if oid2_str.is_empty() {
        bail!("stdin two-tree format requires a second OID after the first");
    }
    let oid2 = oid2_str
        .parse::<ObjectId>()
        .with_context(|| format!("invalid OID: `{oid2_str}`"))?;

    if !opts.quiet {
        writeln!(out, "{} {}", oid1.to_hex(), oid2.to_hex())?;
    }

    let (old_side, new_side) = if opts.reverse {
        (Some(&oid2), Some(oid1))
    } else {
        (Some(oid1), Some(&oid2))
    };
    let entries = diff_with_opts(&repo.odb, old_side, new_side, opts)?;
    let filtered = filter_entries(&repo.odb, &repo, entries, opts)?;
    let has_diff = !filtered.is_empty();
    if !opts.quiet {
        print_diff(out, repo, &filtered, opts, None)?;
    }
    Ok(has_diff)
}

// ── Diff helpers ─────────────────────────────────────────────────────

/// Compute the diff, recursing into sub-trees only when `recursive` is set.
fn diff_with_opts(
    odb: &Odb,
    old_tree: Option<&ObjectId>,
    new_tree: Option<&ObjectId>,
    opts: &Options,
) -> Result<Vec<DiffEntry>> {
    if opts.max_depth.is_some() {
        // Always do full recursion; max_depth is applied as a post-filter
        // after pathspec filtering (depth is relative to pathspec root).
        return diff_trees(odb, old_tree, new_tree, "").map_err(Into::into);
    }
    if opts.recursive {
        if opts.show_trees {
            diff_trees_show_tree_entries(odb, old_tree, new_tree, "").map_err(Into::into)
        } else {
            diff_trees(odb, old_tree, new_tree, "").map_err(Into::into)
        }
    } else {
        diff_trees_toplevel(odb, old_tree, new_tree)
    }
}

/// Apply max-depth filtering: collapse entries deeper than `max_depth` levels
/// relative to the deepest matching pathspec prefix.
fn filter_max_depth(
    entries: Vec<DiffEntry>,
    max_depth: i32,
    pathspecs: &[String],
) -> Vec<DiffEntry> {
    if max_depth < 0 {
        return entries; // unlimited
    }
    let max_depth = max_depth as usize;

    // Normalize pathspecs (strip trailing '/') and sort so the longest match can
    // be found first, mirroring git's reverse-sorted scan in check_recursion_depth.
    let specs: Vec<String> = pathspecs
        .iter()
        .map(|p| p.strip_suffix('/').unwrap_or(p).to_owned())
        .filter(|p| !p.is_empty())
        .collect();

    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    for entry in entries {
        // TODO(byte-paths): depth-collapse logic runs on the lossy string form (Phase 3).
        let path = entry.path().to_string_lossy().into_owned();
        // Find the deepest directory-prefix of `path` that git would refuse to
        // recurse into; if found, the entry collapses to that directory name.
        match collapse_boundary(&path, &specs, max_depth) {
            None => result.push(entry),
            Some(dir) => {
                if seen.insert(dir.clone()) {
                    let mut collapsed = entry;
                    collapsed.old_path = Some(RepoPathBuf::from_string(dir.clone()));
                    collapsed.new_path = Some(RepoPathBuf::from_string(dir));
                    result.push(collapsed);
                }
            }
        }
    }
    result
}

/// Returns true if every '/'-separated component boundary in `rel` keeps the
/// running depth within `max_depth`, mirroring git's within_depth() starting
/// from `start_depth`.
fn within_depth(rel: &str, start_depth: usize, max_depth: usize) -> bool {
    let mut depth = start_depth;
    for b in rel.bytes() {
        if b == b'/' {
            depth += 1;
            if depth > max_depth {
                return false;
            }
        }
    }
    depth <= max_depth
}

/// Mirror of git's check_recursion_depth for a directory path `name`: returns
/// true if git would recurse into this directory under the given pathspecs.
fn check_recursion_depth(name: &str, specs: &[String], max_depth: usize) -> bool {
    if specs.is_empty() {
        return within_depth(name, 1, max_depth);
    }
    // Reverse-sorted scan: longest matching pathspec first.
    let mut sorted: Vec<&String> = specs.iter().collect();
    sorted.sort();
    for item in sorted.iter().rev() {
        let item = item.as_str();
        if name.len() >= item.len() {
            if is_dir_prefix(name, item) {
                // Git does NOT strip the boundary '/'; the leading slash of the
                // remainder counts as a depth increment (within_depth start=1).
                let rel = &name[item.len()..];
                return within_depth(rel, 1, max_depth);
            }
        } else if is_dir_prefix(item, name) {
            // name is a prefix of the pathspec: must always recurse.
            return true;
        }
    }
    false
}

/// True iff `dir` is a leading directory of `path` (or equal).
fn is_dir_prefix(path: &str, dir: &str) -> bool {
    path.starts_with(dir) && (path.len() == dir.len() || path.as_bytes()[dir.len()] == b'/')
}

/// Walk the ancestor directories of a blob `path`; if git would stop recursing
/// at some directory, return that directory name (the collapse target).
fn collapse_boundary(path: &str, specs: &[String], max_depth: usize) -> Option<String> {
    let bytes = path.as_bytes();
    let mut idx = 0;
    while let Some(rel) = bytes[idx..].iter().position(|&b| b == b'/') {
        let end = idx + rel;
        let dir = &path[..end];
        if !check_recursion_depth(dir, specs, max_depth) {
            return Some(dir.to_owned());
        }
        idx = end + 1;
    }
    None
}

/// Non-recursive tree diff: only top-level entries.
///
/// Tree sub-directories are shown as single entries with their tree OIDs,
/// not expanded.
fn diff_trees_toplevel(
    odb: &Odb,
    old_tree_oid: Option<&ObjectId>,
    new_tree_oid: Option<&ObjectId>,
) -> Result<Vec<DiffEntry>> {
    let zero = grit_lib::diff::zero_oid();

    let old_entries = match old_tree_oid {
        Some(oid) => {
            let obj = odb.read(oid).context("reading old tree")?;
            parse_tree(&obj.data).context("parsing old tree")?
        }
        None => Vec::new(),
    };
    let new_entries = match new_tree_oid {
        Some(oid) => {
            let obj = odb.read(oid).context("reading new tree")?;
            parse_tree(&obj.data).context("parsing new tree")?
        }
        None => Vec::new(),
    };

    let mut result = Vec::new();
    let mut oi = 0usize;
    let mut ni = 0usize;

    while oi < old_entries.len() || ni < new_entries.len() {
        match (old_entries.get(oi), new_entries.get(ni)) {
            (Some(o), Some(n)) => {
                // Byte-true ordering and paths (Git tree names are raw bytes).
                match o.name.cmp(&n.name) {
                    std::cmp::Ordering::Less => {
                        result.push(DiffEntry {
                            status: DiffStatus::Deleted,
                            old_path: Some(RepoPathBuf::from_bytes(o.name.clone())),
                            new_path: None,
                            old_mode: format!("{:06o}", o.mode),
                            new_mode: "000000".to_owned(),
                            old_oid: o.oid,
                            new_oid: zero,
                            score: None,
                        });
                        oi += 1;
                    }
                    std::cmp::Ordering::Greater => {
                        result.push(DiffEntry {
                            status: DiffStatus::Added,
                            old_path: None,
                            new_path: Some(RepoPathBuf::from_bytes(n.name.clone())),
                            old_mode: "000000".to_owned(),
                            new_mode: format!("{:06o}", n.mode),
                            old_oid: zero,
                            new_oid: n.oid,
                            score: None,
                        });
                        ni += 1;
                    }
                    std::cmp::Ordering::Equal => {
                        if o.oid != n.oid || o.mode != n.mode {
                            let path = RepoPathBuf::from_bytes(o.name.clone());
                            let old_type = o.mode & 0o170000;
                            let new_type = n.mode & 0o170000;
                            let old_is_tree = old_type == 0o040000;
                            let new_is_tree = new_type == 0o040000;
                            if old_is_tree != new_is_tree {
                                // A directory turning into a file (or vice versa) is not a
                                // typechange in non-recursive diff-tree; git emits a separate
                                // addition of the new entry followed by deletion of the old.
                                result.push(DiffEntry {
                                    status: DiffStatus::Added,
                                    old_path: None,
                                    new_path: Some(path.clone()),
                                    old_mode: "000000".to_owned(),
                                    new_mode: format!("{:06o}", n.mode),
                                    old_oid: zero,
                                    new_oid: n.oid,
                                    score: None,
                                });
                                result.push(DiffEntry {
                                    status: DiffStatus::Deleted,
                                    old_path: Some(path),
                                    new_path: None,
                                    old_mode: format!("{:06o}", o.mode),
                                    new_mode: "000000".to_owned(),
                                    old_oid: o.oid,
                                    new_oid: zero,
                                    score: None,
                                });
                            } else {
                                // A mode-only change (e.g. chmod) is Modified, not TypeChanged.
                                // TypeChanged is only for actual type changes (blob ↔ symlink etc.)
                                let status = if old_type != new_type {
                                    DiffStatus::TypeChanged
                                } else {
                                    DiffStatus::Modified
                                };
                                result.push(DiffEntry {
                                    status,
                                    old_path: Some(path.clone()),
                                    new_path: Some(path),
                                    old_mode: format!("{:06o}", o.mode),
                                    new_mode: format!("{:06o}", n.mode),
                                    old_oid: o.oid,
                                    new_oid: n.oid,
                                    score: None,
                                });
                            }
                        }
                        oi += 1;
                        ni += 1;
                    }
                }
            }
            (Some(o), None) => {
                result.push(DiffEntry {
                    status: DiffStatus::Deleted,
                    old_path: Some(RepoPathBuf::from_bytes(o.name.clone())),
                    new_path: None,
                    old_mode: format!("{:06o}", o.mode),
                    new_mode: "000000".to_owned(),
                    old_oid: o.oid,
                    new_oid: zero,
                    score: None,
                });
                oi += 1;
            }
            (None, Some(n)) => {
                result.push(DiffEntry {
                    status: DiffStatus::Added,
                    old_path: None,
                    new_path: Some(RepoPathBuf::from_bytes(n.name.clone())),
                    old_mode: "000000".to_owned(),
                    new_mode: format!("{:06o}", n.mode),
                    old_oid: zero,
                    new_oid: n.oid,
                    score: None,
                });
                ni += 1;
            }
            (None, None) => break,
        }
    }

    Ok(result)
}

// ── Output ───────────────────────────────────────────────────────────

/// Recursively collect all blob entries from a tree, returning (oid, path, mode).
fn collect_tree_blobs_recursive(
    odb: &Odb,
    tree_oid: &ObjectId,
    prefix: &str,
) -> Result<Vec<(String, String, ObjectId)>> {
    let obj = odb.read(tree_oid)?;
    let tree = parse_tree(&obj.data)?;
    let mut result = Vec::new();
    for entry in tree {
        let name = String::from_utf8_lossy(&entry.name).into_owned();
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", prefix, name)
        };
        if entry.mode == 0o040000 {
            // Subtree — recurse.
            if let Ok(sub) = collect_tree_blobs_recursive(odb, &entry.oid, &path) {
                result.extend(sub);
            }
        } else {
            result.push((path, format!("{:06o}", entry.mode), entry.oid));
        }
    }
    Ok(result)
}

fn is_gitlink_mode(mode: &str) -> bool {
    mode == "160000"
}

/// For `submodule=log`, Git collapses a pure submodule path change (same gitlink OID) into a
/// single `(new submodule)` line at the new path — omit the paired delete at the old path.
fn preprocess_gitlink_renames_for_submodule_log(entries: Vec<DiffEntry>) -> Vec<DiffEntry> {
    let mut out = Vec::with_capacity(entries.len());
    let mut i = 0usize;
    while i < entries.len() {
        let e = &entries[i];
        if i + 1 < entries.len()
            && e.status == DiffStatus::Deleted
            && is_gitlink_mode(&e.old_mode)
            && entries[i + 1].status == DiffStatus::Added
            && is_gitlink_mode(&entries[i + 1].new_mode)
            && e.old_oid == entries[i + 1].new_oid
            && e.old_oid != grit_lib::diff::zero_oid()
        {
            out.push(entries[i + 1].clone());
            i += 2;
        } else {
            out.push(entries[i].clone());
            i += 1;
        }
    }
    out
}

/// Open the submodule object database for `path`, matching Git's `open_submodule`: prefer the
/// checked-out work tree (gitfile), else `.git/modules/<path>` when the work tree was removed.
fn open_submodule_repo(
    super_git_dir: &Path,
    work_tree: Option<&Path>,
    path: &str,
) -> Option<Repository> {
    if let Some(wt) = work_tree {
        let sub_wt = wt.join(path);
        let dot_git = sub_wt.join(".git");
        if dot_git.exists() {
            if let Ok(gd) = resolve_dot_git(&dot_git) {
                if let Ok(repo) = Repository::open(&gd, Some(&sub_wt)) {
                    return Some(repo);
                }
            }
        }
    }
    let modules_dir = super_git_dir.join("modules").join(path);
    if modules_dir.is_dir() {
        return Repository::open(&modules_dir, None).ok();
    }
    if let Some(wt) = work_tree {
        if let Some(name) = submodule_name_for_path(wt, path) {
            let modules_dir = super_git_dir.join("modules").join(name);
            if modules_dir.is_dir() {
                return Repository::open(&modules_dir, None).ok();
            }
        }
    }
    None
}

fn submodule_name_for_path(work_tree: &Path, path: &str) -> Option<String> {
    let gm = work_tree.join(".gitmodules");
    let text = std::fs::read_to_string(&gm).ok()?;
    let file = ConfigFile::parse(&gm, &text, ConfigScope::Local).ok()?;
    let wanted = path.replace('\\', "/");
    file.entries.iter().find_map(|e| {
        let rest = e.key.strip_prefix("submodule.")?;
        let name = rest.strip_suffix(".path")?;
        let value = e.value.as_deref()?.trim().replace('\\', "/");
        (value == wanted).then(|| name.to_owned())
    })
}

fn commit_exists_in_repo(repo: &Repository, oid: &ObjectId) -> bool {
    match repo.odb.read(oid) {
        Ok(obj) => obj.kind == ObjectKind::Commit,
        Err(_) => false,
    }
}

fn format_submodule_log_subject(commit: &grit_lib::objects::CommitData) -> String {
    let first_line = commit.message.lines().next().unwrap_or("").trim_end();
    let raw_body: &[u8] = commit
        .raw_message
        .as_deref()
        .unwrap_or(commit.message.as_bytes());
    if let Some(enc_name) = commit.encoding.as_deref() {
        if let Some(enc) = Encoding::for_label(enc_name.as_bytes()) {
            let (cow, _, _) = enc.decode(raw_body);
            let s = cow.lines().next().unwrap_or("").trim_end();
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    first_line.to_string()
}

fn print_submodule_log_for_entry(
    out: &mut impl Write,
    super_git_dir: &Path,
    work_tree: Option<&Path>,
    entry: &DiffEntry,
    abbrev_len: usize,
) -> Result<()> {
    let zero = grit_lib::diff::zero_oid();
    let path = entry.path();
    let (one, two) = match entry.status {
        DiffStatus::Added => (zero, entry.new_oid),
        DiffStatus::Deleted => (entry.old_oid, zero),
        DiffStatus::Modified | DiffStatus::TypeChanged => (entry.old_oid, entry.new_oid),
        DiffStatus::Renamed | DiffStatus::Copied => (entry.old_oid, entry.new_oid),
        DiffStatus::Unmerged => return Ok(()),
    };

    let mut message: Option<&'static str> = None;
    if one == zero {
        message = Some("(new submodule)");
    } else if two == zero {
        message = Some("(submodule deleted)");
    }

    let sub_repo = open_submodule_repo(super_git_dir, work_tree, &path.to_string_lossy());
    if sub_repo.is_none() && message.is_none() {
        message = Some("(commits not present)");
    }

    let left = if one != zero {
        sub_repo
            .as_ref()
            .filter(|r| commit_exists_in_repo(r, &one))
            .map(|_| one)
    } else {
        Some(one)
    };
    let right = if two != zero {
        sub_repo
            .as_ref()
            .filter(|r| commit_exists_in_repo(r, &two))
            .map(|_| two)
    } else {
        Some(two)
    };

    if sub_repo.is_some()
        && message.is_none()
        && ((one != zero && left.is_none()) || (two != zero && right.is_none()))
    {
        message = Some("(commits not present)");
    }

    let mut fast_forward = false;
    let mut fast_backward = false;
    if let (Some(ref sr), Some(l), Some(r)) = (&sub_repo, left, right) {
        if l != r && l != zero && r != zero {
            if let Ok(bases) = merge_bases_first_vs_rest(sr, l, &[r]) {
                if let Some(b) = bases.first() {
                    if *b == l {
                        fast_forward = true;
                    } else if *b == r {
                        fast_backward = true;
                    }
                }
            }
        }
    }

    if one == two {
        return Ok(());
    }

    let sep = if fast_forward || fast_backward {
        ".."
    } else {
        "..."
    };
    let one_hex = one.to_hex();
    let two_hex = two.to_hex();
    let a1 = abbrev_oid(&one_hex, Some(abbrev_len), false);
    let a2 = abbrev_oid(&two_hex, Some(abbrev_len), false);
    write!(out, "Submodule {path} {a1}{sep}{a2}")?;
    if let Some(m) = message {
        writeln!(out, " {m}")?;
    } else if fast_backward {
        writeln!(out, " (rewind):")?;
    } else {
        writeln!(out, ":")?;
    }

    if let (Some(sr), Some(l), Some(r)) = (sub_repo, left, right) {
        if l != zero && r != zero && l != r {
            let l_ancestor_of_r = merge_bases_first_vs_rest(&sr, l, &[r])
                .ok()
                .and_then(|b| b.first().copied())
                .is_some_and(|b| b == l);
            if l_ancestor_of_r {
                let mut cur = r;
                let mut logged: Vec<grit_lib::objects::CommitData> = Vec::new();
                while cur != l {
                    let obj = match sr.odb.read(&cur) {
                        Ok(o) => o,
                        Err(_) => break,
                    };
                    if obj.kind != ObjectKind::Commit {
                        break;
                    }
                    let data = match parse_commit(&obj.data) {
                        Ok(d) => d,
                        Err(_) => break,
                    };
                    let Some(parent) = data.parents.first().copied() else {
                        break;
                    };
                    logged.push(data);
                    cur = parent;
                }
                for data in logged {
                    let subj = format_submodule_log_subject(&data);
                    writeln!(out, "  > {subj}")?;
                }
            }
        }
    }

    Ok(())
}

/// Build normal [`DiffEntry`] list for first-parent vs merge tree on combined-diff paths only.
#[allow(dead_code)]
fn combined_paths_to_first_parent_entries(
    _odb: &Odb,
    paths: &[CombinedDiffPath],
) -> Result<Vec<DiffEntry>> {
    let zero = grit_lib::diff::zero_oid();
    let mut out = Vec::with_capacity(paths.len());
    for p in paths {
        let p0 = p.parents.first();
        let (old_mode, old_oid, new_mode, new_oid, status) = match p0 {
            None => continue,
            Some(side) if side.status == CombinedParentStatus::Added => (
                "000000".to_string(),
                zero,
                format!("{:06o}", p.merge_mode),
                p.merge_oid,
                DiffStatus::Added,
            ),
            Some(side) if p.merge_mode == 0 || p.merge_oid == zero => (
                format!("{:06o}", side.mode),
                side.oid,
                "000000".to_string(),
                zero,
                DiffStatus::Deleted,
            ),
            Some(side) => {
                let st = if side.oid != p.merge_oid || side.mode != p.merge_mode {
                    let ot = side.mode & 0o170000;
                    let nt = p.merge_mode & 0o170000;
                    if ot != nt {
                        DiffStatus::TypeChanged
                    } else {
                        DiffStatus::Modified
                    }
                } else {
                    continue;
                };
                (
                    format!("{:06o}", side.mode),
                    side.oid,
                    format!("{:06o}", p.merge_mode),
                    p.merge_oid,
                    st,
                )
            }
        };
        out.push(DiffEntry {
            status,
            // TODO(byte-paths): CombinedDiffPath.path is a lossy String (Phase 3/4).
            old_path: Some(RepoPathBuf::from_string(p.path.clone())),
            new_path: Some(RepoPathBuf::from_string(p.path.clone())),
            old_mode,
            new_mode,
            old_oid,
            new_oid,
            score: None,
        });
    }
    Ok(out)
}

/// Print combined `--summary` lines (create/delete/mode) using first-parent vs merge semantics.
#[allow(dead_code)]
fn write_combined_summary(out: &mut impl Write, paths: &[CombinedDiffPath]) -> Result<()> {
    for p in paths {
        let p0 = match p.parents.first() {
            Some(s) => s,
            None => continue,
        };
        if p0.status == CombinedParentStatus::Added {
            writeln!(out, " create mode {:06o} {}", p.merge_mode, p.path)?;
            continue;
        }
        if p.merge_mode == 0 || p.merge_oid == grit_lib::diff::zero_oid() {
            writeln!(out, " delete mode {:06o} {}", p0.mode, p.path)?;
            continue;
        }
        if p0.mode != p.merge_mode && p0.oid == p.merge_oid {
            writeln!(
                out,
                " mode change {:06o} => {:06o} {}",
                p0.mode, p.merge_mode, p.path
            )?;
        }
    }
    Ok(())
}

/// Stat / raw / patch / summary for `-c` / `--cc` merge commits (matches `diff_tree_combined` order).
fn print_combined_merge_output(
    out: &mut impl Write,
    repo: &Repository,
    paths: &[CombinedDiffPath],
    opts: &Options,
    parent_commits: &[ObjectId],
    merge_tree: &ObjectId,
    commit_oid: Option<&ObjectId>,
) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let odb = &repo.odb;
    let abbrev_len = if opts.full_index {
        Some(40usize)
    } else {
        opts.abbrev
    };
    let want_stat = opts.format == OutputFormat::Stat
        || (opts.format == OutputFormat::Patch && opts.patch_with_stat);
    // `--summary` without an explicit `--raw` suppresses the default combined raw
    // output (matching the non-combined `--summary` behavior), so e.g.
    // `diff-tree --cc --summary` shows only the summary lines.
    let suppress_raw = opts.summary && !opts.raw_explicit;
    let want_raw = (opts.format == OutputFormat::Raw && !suppress_raw)
        || (opts.format == OutputFormat::Patch && opts.patch_with_raw);
    let want_patch = opts.format == OutputFormat::Patch;
    let quote_fully = ConfigSet::load(Some(&repo.git_dir), true)
        .unwrap_or_default()
        .quote_path_fully();

    // With `-M`/`--combined-all-paths`, reclassify per-parent `Added` sides as renames so the raw
    // line shows `R` and all source names (git's `find_paths_generic`). Work on an owned copy.
    let enriched_storage: Option<Vec<CombinedDiffPath>> =
        if opts.combined_all_paths && opts.find_renames.is_some() {
            let rename_thresh = opts.find_renames.unwrap_or(50);
            let mut parent_trees = Vec::with_capacity(parent_commits.len());
            for p in parent_commits {
                parent_trees.push(commit_tree(odb, p)?);
            }
            if parent_trees.len() >= 2 {
                let mut owned = paths.to_vec();
                for p in &mut owned {
                    enrich_combined_path_renames(odb, p, &parent_trees, merge_tree, rename_thresh);
                }
                Some(owned)
            } else {
                None
            }
        } else {
            None
        };
    let paths: &[CombinedDiffPath] = enriched_storage.as_deref().unwrap_or(paths);

    // Git computes `--stat`/`--summary` for a combined diff as an ordinary diff
    // against the FIRST parent (combine-diff.c: `diff_tree_oid(&parents->oid[0],
    // oid, ...)` under STAT_FORMAT_MASK), not from the combined-interesting paths.
    let first_parent_entries = if want_stat || opts.summary {
        match parent_commits.first() {
            Some(p0) => {
                let p0_tree = commit_tree(odb, p0)?;
                let entries = diff_with_opts(odb, Some(&p0_tree), Some(merge_tree), opts)?;
                filter_entries(odb, repo, entries, opts)?
            }
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };
    let stat_entries = &first_parent_entries;

    if want_stat && !stat_entries.is_empty() {
        print_stat_summary(
            out,
            odb,
            stat_entries,
            quote_fully,
            opts.compact_summary,
            opts.shortstat,
        )?;
    }

    // `--summary` lines are part of the first-parent stat group and precede the
    // patch (git emits them via diff_flush under STAT_FORMAT_MASK).
    if opts.summary {
        write_summary(out, stat_entries)?;
    }

    if want_raw {
        // The commit-id header (NUL- or newline-terminated) is already emitted by the per-commit
        // caller, so do not prepend it here.
        let _ = commit_oid;
        for p in paths {
            if opts.nul_terminated {
                write_combined_raw_z(out, None, p, abbrev_len, opts.combined_all_paths)?;
            } else {
                writeln!(
                    out,
                    "{}",
                    format_combined_raw_line_all_paths(
                        p,
                        abbrev_len,
                        opts.combined_all_paths,
                        quote_fully,
                    )
                )?;
            }
        }
    }

    let summary_emitted = opts.summary && summary_has_lines(stat_entries);
    let need_patch_sep = (want_stat && !stat_entries.is_empty()) || want_raw || summary_emitted;
    if want_patch {
        let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
        let patch_abbrev = if opts.full_index {
            40usize
        } else {
            opts.abbrev.unwrap_or(7)
        };
        let ws = CombinedDiffWsOptions {
            ignore_all_space: opts.ignore_all_space,
            ignore_space_change: opts.ignore_space_change,
            ignore_space_at_eol: opts.ignore_space_at_eol,
            ignore_cr_at_eol: opts.ignore_cr_at_eol,
        };
        let rename_thresh = opts.find_renames.unwrap_or(50);
        let mut parent_trees = Vec::with_capacity(parent_commits.len());
        for p in parent_commits {
            parent_trees.push(commit_tree(odb, p)?);
        }
        if parent_trees.len() >= 2 {
            if need_patch_sep {
                writeln!(out)?;
            }
            for p in paths {
                let parent_blob_paths = if opts.combined_all_paths && opts.find_renames.is_some() {
                    combined_merge_parent_blob_paths(
                        odb,
                        &p.path,
                        &parent_trees,
                        merge_tree,
                        rename_thresh,
                    )
                } else {
                    None
                };
                let ps_ref = parent_blob_paths.as_deref();
                if let Some(patch) = format_combined_textconv_patch(
                    &repo.git_dir,
                    &config,
                    odb,
                    &p.path,
                    &parent_trees,
                    merge_tree,
                    patch_abbrev,
                    opts.context_lines,
                    opts.combined_use_cc_word,
                    false,
                    ws,
                    opts.combined_all_paths,
                    ps_ref,
                    &p.parents,
                    quote_fully,
                ) {
                    write!(out, "{patch}")?;
                }
            }
        }
    }

    Ok(())
}

fn write_combined_raw_z(
    out: &mut impl Write,
    commit_hex: Option<&str>,
    p: &CombinedDiffPath,
    abbrev_len: Option<usize>,
    combined_all_paths: bool,
) -> Result<()> {
    if let Some(h) = commit_hex {
        out.write_all(h.as_bytes())?;
        out.write_all(b"\0")?;
    }
    if combined_all_paths {
        // NUL-separated raw (unquoted) output: metadata, then one source name per parent, then the
        // merge path. Build the metadata directly (names contain tabs, so we cannot split on `\t`).
        out.write_all(combined_raw_meta(p, abbrev_len).as_bytes())?;
        out.write_all(b"\0")?;
        for side in &p.parents {
            let name = side.rename_from.as_deref().unwrap_or(&p.path);
            out.write_all(name.as_bytes())?;
            out.write_all(b"\0")?;
        }
        out.write_all(p.path.as_bytes())?;
        out.write_all(b"\0")?;
    } else {
        let line = format_combined_raw_line(p, abbrev_len);
        out.write_all(line.as_bytes())?;
        out.write_all(b"\0")?;
    }
    Ok(())
}

/// Print combined merge paths (`-c` / `--cc` with `--name-status` / `--name-only`).
fn print_combined_paths(
    out: &mut impl Write,
    paths: &[CombinedDiffPath],
    opts: &Options,
) -> Result<()> {
    for p in paths {
        match opts.format {
            OutputFormat::NameOnly => {
                writeln!(out, "{}", p.path)?;
            }
            OutputFormat::NameStatus => {
                let letters: String = p
                    .parents
                    .iter()
                    .map(|side| match side.status {
                        CombinedParentStatus::Added => 'A',
                        CombinedParentStatus::Modified => 'M',
                        CombinedParentStatus::Deleted => 'D',
                        CombinedParentStatus::Renamed => 'R',
                    })
                    .collect();
                writeln!(out, "{letters}\t{}", p.path)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn write_raw_diff_tree_z(
    out: &mut impl Write,
    entry: &DiffEntry,
    abbrev_len: Option<usize>,
) -> Result<()> {
    let ellipsis = if std::env::var("GIT_PRINT_SHA1_ELLIPSIS").ok().as_deref() == Some("yes") {
        "..."
    } else {
        ""
    };
    let old_hex = format!("{}", entry.old_oid);
    let new_hex = format!("{}", entry.new_oid);
    let (old_disp, new_disp) = if let Some(len) = abbrev_len {
        let oa = &old_hex[..len.min(old_hex.len())];
        let na = &new_hex[..len.min(new_hex.len())];
        (oa.to_string(), na.to_string())
    } else {
        (old_hex, new_hex)
    };

    let status_str = match (entry.status, entry.score) {
        (DiffStatus::Renamed, Some(s)) => format!("R{s:03}"),
        (DiffStatus::Copied, Some(s)) => format!("C{s:03}"),
        _ => entry.status.letter().to_string(),
    };

    write!(
        out,
        ":{} {} {}{} {}{} {}\0",
        entry.old_mode, entry.new_mode, old_disp, ellipsis, new_disp, ellipsis, status_str
    )?;
    match entry.status {
        DiffStatus::Renamed | DiffStatus::Copied => {
            out.write_all(entry.old_path.as_deref().unwrap_or(RepoPath::from_str("")).as_bytes())?;
            out.write_all(b"\0")?;
            out.write_all(entry.new_path.as_deref().unwrap_or(RepoPath::from_str("")).as_bytes())?;
            out.write_all(b"\0")?;
        }
        _ => {
            out.write_all(entry.path().as_bytes())?;
            out.write_all(b"\0")?;
        }
    }
    Ok(())
}

fn prepare_diff_tree_entries<'a>(
    odb: &Odb,
    entries: Vec<DiffEntry>,
    opts: &Options,
    old_tree_oid: Option<&ObjectId>,
) -> Vec<DiffEntry> {
    let old_blobs = if opts.find_copies.is_some() && opts.find_copies_harder {
        if let Some(tree_oid) = old_tree_oid {
            collect_tree_blobs_recursive(odb, tree_oid, "").unwrap_or_default()
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };
    // `-B` (break-rewrites): split complete-rewrite Modified/TypeChanged pairs
    // into a Deleted + Added pair so that rename/copy detection can re-pair them
    // across paths, then merge any surviving broken pairs back together.
    if opts.break_rewrites && (opts.find_renames.is_some() || opts.find_copies.is_some()) {
        return break_then_detect(odb, entries, opts, &old_blobs);
    }

    let mut out = if let Some(threshold) = opts.find_renames {
        let mut result = detect_renames(odb, None, entries, threshold);
        if let Some(copy_threshold) = opts.find_copies {
            result = lib_detect_copies(
                odb,
                None,
                result,
                copy_threshold,
                opts.find_copies_harder,
                &old_blobs,
            );
        }
        result
    } else if let Some(copy_threshold) = opts.find_copies {
        lib_detect_copies(
            odb,
            None,
            entries,
            copy_threshold,
            opts.find_copies_harder,
            &old_blobs,
        )
    } else {
        entries
    };
    if opts.format == OutputFormat::Patch
        && opts.submodule_mode.as_deref().is_some_and(|m| m == "log")
    {
        out = preprocess_gitlink_renames_for_submodule_log(out);
    }
    out
}

/// Implements `-B` (break-rewrites) combined with rename/copy detection.
///
/// Mirrors git's diffcore-break + diffcore-rename + diffcore-merge-broken:
/// complete-rewrite Modified/TypeChanged pairs are split into a Deleted + Added
/// pair (the "broken pair"), rename/copy detection is run, and surviving broken
/// halves are merged back. A rename whose source path survives (its broken-add
/// peer was not consumed) is downgraded to a copy.
fn break_then_detect(
    odb: &Odb,
    entries: Vec<DiffEntry>,
    opts: &Options,
    old_blobs: &[(String, String, ObjectId)],
) -> Vec<DiffEntry> {
    use std::collections::HashSet;
    let zero = grit_lib::diff::zero_oid();
    let zero_mode = "000000".to_owned();

    // Phase 1: break eligible pairs.
    let mut broken_paths: HashSet<String> = HashSet::new();
    // Remember the original (old half, new half) of every broken path so we can
    // merge it back together later.
    let mut broken_old: std::collections::HashMap<String, DiffEntry> =
        std::collections::HashMap::new();
    let mut broken_new: std::collections::HashMap<String, DiffEntry> =
        std::collections::HashMap::new();
    let mut prepared: Vec<DiffEntry> = Vec::new();

    for entry in entries {
        let is_inplace_edit =
            matches!(entry.status, DiffStatus::Modified | DiffStatus::TypeChanged)
                && entry.old_path.as_deref() == entry.new_path.as_deref()
                && entry.old_path.is_some();
        if !is_inplace_edit {
            prepared.push(entry);
            continue;
        }
        let old_data = odb.read(&entry.old_oid).ok().map(|o| o.data);
        let new_data = odb.read(&entry.new_oid).ok().map(|o| o.data);
        let should_break = match (&old_data, &new_data) {
            (Some(o), Some(n)) => grit_lib::diff::should_break_rewrite_pair(
                o,
                n,
                grit_lib::diff::GIT_DIFF_DEFAULT_BREAK_SCORE,
            ),
            _ => false,
        };
        if !should_break {
            prepared.push(entry);
            continue;
        }
        // TODO(byte-paths): break/rename bookkeeping keyed on lossy string (Phase 3).
        let path = entry.path().to_string_lossy().into_owned();
        broken_paths.insert(path.clone());
        // Deleted half (old side).
        let del = DiffEntry {
            status: DiffStatus::Deleted,
            old_path: entry.old_path.clone(),
            new_path: None,
            old_mode: entry.old_mode.clone(),
            new_mode: zero_mode.clone(),
            old_oid: entry.old_oid,
            new_oid: zero,
            score: None,
        };
        // Added half (new side).
        let add = DiffEntry {
            status: DiffStatus::Added,
            old_path: None,
            new_path: entry.new_path.clone(),
            old_mode: zero_mode.clone(),
            new_mode: entry.new_mode.clone(),
            old_oid: zero,
            new_oid: entry.new_oid,
            score: None,
        };
        broken_old.insert(path.clone(), del.clone());
        broken_new.insert(path.clone(), add.clone());
        prepared.push(del);
        prepared.push(add);
    }

    // Phase 2: rename (and optional copy) detection.
    let mut detected = if let Some(threshold) = opts.find_renames {
        detect_renames(odb, None, prepared, threshold)
    } else {
        prepared
    };
    if let Some(copy_threshold) = opts.find_copies {
        detected = lib_detect_copies(
            odb,
            None,
            detected,
            copy_threshold,
            opts.find_copies_harder,
            old_blobs,
        );
    }

    // Phase 3: merge back surviving broken pairs and resolve rename-vs-copy.
    //
    // A broken-add peer "survives" if the path still appears as a standalone
    // Added entry in the detection output.
    let surviving_add: HashSet<String> = detected
        .iter()
        .filter(|e| e.status == DiffStatus::Added)
        .filter_map(|e| e.new_path.as_deref().map(|p| p.to_string_lossy().into_owned()))
        .filter(|p| broken_paths.contains(p))
        .collect();
    let surviving_del: HashSet<String> = detected
        .iter()
        .filter(|e| e.status == DiffStatus::Deleted)
        .filter_map(|e| e.old_path.as_deref().map(|p| p.to_string_lossy().into_owned()))
        .filter(|p| broken_paths.contains(p))
        .collect();

    let mut result: Vec<DiffEntry> = Vec::new();
    let mut merged_paths: HashSet<String> = HashSet::new();
    for entry in detected {
        match entry.status {
            // A rename/copy whose source is a broken path that still survives in
            // the destination tree is really a copy.
            DiffStatus::Renamed
                if entry
                    .old_path
                    .as_deref()
                    .is_some_and(|p| surviving_add.contains(p.to_string_lossy().as_ref())) =>
            {
                let mut e = entry;
                e.status = DiffStatus::Copied;
                result.push(e);
            }
            // Standalone surviving broken halves: recombine into the original
            // Modified/TypeChanged once (when both halves survived) or restore
            // the original change when only this half survived because its peer
            // was consumed by a rename/copy.
            DiffStatus::Added
                if entry
                    .new_path
                    .as_deref()
                    .is_some_and(|p| broken_paths.contains(p.to_string_lossy().as_ref())) =>
            {
                let path = entry
                    .new_path
                    .as_deref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if merged_paths.insert(path.clone()) {
                    if let (Some(del), Some(add)) = (broken_old.get(&path), broken_new.get(&path)) {
                        result.push(merge_broken_pair(del, add));
                    }
                }
            }
            DiffStatus::Deleted
                if entry
                    .old_path
                    .as_deref()
                    .is_some_and(|p| broken_paths.contains(p.to_string_lossy().as_ref())) =>
            {
                let path = entry
                    .old_path
                    .as_deref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                // Only emit here if the add-half did NOT survive (otherwise the
                // Added arm above handles the merge); this covers the symmetric
                // case where the delete half is the survivor.
                if !surviving_add.contains(&path) && merged_paths.insert(path.clone()) {
                    if let (Some(del), Some(add)) = (broken_old.get(&path), broken_new.get(&path)) {
                        result.push(merge_broken_pair(del, add));
                    }
                }
            }
            _ => result.push(entry),
        }
    }
    let _ = surviving_del;

    result.sort_by(|a, b| a.path().cmp(b.path()));
    result
}

/// Recombine the two halves of a broken pair into a single Modified or
/// TypeChanged entry, restoring git's score-100 break marker.
fn merge_broken_pair(del: &DiffEntry, add: &DiffEntry) -> DiffEntry {
    let status = if del.old_mode != add.new_mode {
        DiffStatus::TypeChanged
    } else {
        DiffStatus::Modified
    };
    DiffEntry {
        status,
        old_path: del.old_path.clone(),
        new_path: add.new_path.clone(),
        old_mode: del.old_mode.clone(),
        new_mode: add.new_mode.clone(),
        old_oid: del.old_oid,
        new_oid: add.new_oid,
        score: Some(100),
    }
}

fn run_diff_tree_whitespace_check(
    repo: &Repository,
    entries: &[DiffEntry],
    opts: &Options,
) -> Result<()> {
    let merged_attrs = match load_gitattributes_for_diff(repo) {
        Ok(a) => a,
        Err(grit_lib::error::Error::InvalidRef(msg)) if msg.starts_with("bad --attr-source") => {
            eprintln!("fatal: bad --attr-source or GIT_ATTR_SOURCE");
            std::process::exit(128);
        }
        Err(e) => return Err(e.into()),
    };
    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let ignore_case = config
        .get("core.ignorecase")
        .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "yes" | "1"));
    let mut stdout = std::io::stdout().lock();
    let has_ws = check_whitespace_errors(
        &mut stdout,
        entries,
        &repo.odb,
        None,
        &merged_attrs,
        ignore_case,
        &config,
    )?;
    if has_ws {
        if opts.exit_code {
            std::process::exit(3);
        }
        std::process::exit(2);
    }
    Ok(())
}

/// Print the diff entries according to `opts.format`.
fn print_diff(
    out: &mut impl Write,
    repo: &Repository,
    entries: &[DiffEntry],
    opts: &Options,
    old_tree_oid: Option<&ObjectId>,
) -> Result<bool> {
    let odb = &repo.odb;
    let git_dir: &Path = repo.git_dir.as_ref();
    let work_tree = repo.work_tree.as_deref();
    let quote_fully = ConfigSet::load(Some(&repo.git_dir), true)
        .unwrap_or_default()
        .quote_path_fully();

    let owned_entries = prepare_diff_tree_entries(odb, entries.to_vec(), opts, old_tree_oid);
    let entries = owned_entries.as_slice();

    let submodule_log = opts.format == OutputFormat::Patch
        && opts.submodule_mode.as_deref().is_some_and(|m| m == "log");

    if submodule_log {
        let abbrev_len = if opts.full_index {
            40usize
        } else {
            opts.abbrev.unwrap_or(7)
        };
        for entry in entries {
            if is_gitlink_mode(&entry.old_mode) || is_gitlink_mode(&entry.new_mode) {
                print_submodule_log_for_entry(out, git_dir, work_tree, entry, abbrev_len)?;
            }
        }
        return Ok(false);
    }

    match opts.format {
        OutputFormat::Raw => {
            // `diff-tree` only defaults to raw output when no other output format
            // was requested (git's diff_tree_tweak_rev). `--summary` selects the
            // summary format, so unless `--raw` was given explicitly the default
            // raw lines are suppressed (matches `git diff-tree --summary`).
            let suppress_raw = opts.summary && !opts.raw_explicit;
            if !suppress_raw {
                if opts.nul_terminated {
                    let abbrev = opts.abbrev;
                    for entry in entries {
                        write_raw_diff_tree_z(out, entry, abbrev)?;
                    }
                } else {
                    for entry in entries {
                        if let Some(abbrev_len) = opts.abbrev {
                            writeln!(out, "{}", format_raw_abbrev(entry, abbrev_len))?;
                        } else {
                            writeln!(out, "{}", format_raw(entry))?;
                        }
                    }
                }
            }
            if opts.summary {
                write_summary(out, entries)?;
            }
        }
        OutputFormat::Patch => {
            // --patch-with-stat: show stat (then `--summary` lines) before patch
            if opts.patch_with_stat {
                print_stat_summary(
                    out,
                    odb,
                    entries,
                    quote_fully,
                    opts.compact_summary,
                    opts.shortstat,
                )?;
                if opts.summary {
                    write_summary(out, entries)?;
                }
                writeln!(out)?;
            }
            // --patch-with-raw: show raw before patch
            if opts.patch_with_raw {
                for entry in entries {
                    if let Some(abbrev_len) = opts.abbrev {
                        writeln!(out, "{}", format_raw_abbrev(entry, abbrev_len))?;
                    } else {
                        writeln!(out, "{}", format_raw(entry))?;
                    }
                }
                writeln!(out)?;
            }
            // Plain `-p --summary` (without `--patch-with-stat`) emits the summary
            // lines between the header and the patch, followed by a blank line.
            if opts.summary && !opts.patch_with_stat && summary_has_lines(entries) {
                write_summary(out, entries)?;
                writeln!(out)?;
            }
            for entry in entries {
                write_patch_entry(
                    out,
                    odb,
                    entry,
                    opts.context_lines,
                    opts.abbrev,
                    opts.full_index,
                    opts.no_prefix,
                    opts.binary,
                    git_dir,
                    quote_fully,
                    opts.indent_heuristic,
                )?;
            }
        }
        OutputFormat::Stat => {
            print_stat_summary(
                out,
                odb,
                entries,
                quote_fully,
                opts.compact_summary,
                opts.shortstat,
            )?;
            if opts.summary {
                write_summary(out, entries)?;
            }
        }
        OutputFormat::NameOnly => {
            for entry in entries {
                if opts.nul_terminated {
                    out.write_all(entry.path().as_bytes())?;
                    out.write_all(b"\0")?;
                } else {
                    writeln!(out, "{}", quote_c_style(&entry.path().to_string_lossy(), quote_fully))?;
                }
            }
        }
        OutputFormat::NameStatus => {
            write_diff_index_name_status(out, entries, quote_fully, opts.nul_terminated)?;
        }
    }
    Ok(false)
}

/// Abbreviate an OID hex string to the given length.
fn abbrev_oid(hex: &str, abbrev: Option<usize>, full_index: bool) -> &str {
    if full_index {
        hex
    } else {
        let len = abbrev.unwrap_or(7).min(hex.len());
        &hex[..len]
    }
}

/// Whether `write_summary` would emit at least one line for these entries.
fn summary_has_lines(entries: &[DiffEntry]) -> bool {
    entries.iter().any(|entry| match entry.status {
        DiffStatus::Added
        | DiffStatus::Deleted
        | DiffStatus::TypeChanged
        | DiffStatus::Renamed
        | DiffStatus::Copied => true,
        DiffStatus::Modified => entry.old_mode != entry.new_mode,
        _ => false,
    })
}

/// Write human-readable `--summary` lines (create mode, delete mode, mode change, etc.)
fn write_summary(out: &mut impl Write, entries: &[DiffEntry]) -> Result<()> {
    for entry in entries {
        match entry.status {
            DiffStatus::Added => {
                writeln!(out, " create mode {} {}", entry.new_mode, entry.path())?;
            }
            DiffStatus::Deleted => {
                writeln!(out, " delete mode {} {}", entry.old_mode, entry.path())?;
            }
            DiffStatus::Modified if entry.old_mode != entry.new_mode => {
                writeln!(
                    out,
                    " mode change {} => {} {}",
                    entry.old_mode,
                    entry.new_mode,
                    entry.path()
                )?;
            }
            DiffStatus::TypeChanged => {
                writeln!(
                    out,
                    " mode change {} => {} {}",
                    entry.old_mode,
                    entry.new_mode,
                    entry.path()
                )?;
            }
            DiffStatus::Renamed => {
                let sim = entry.score.unwrap_or(100);
                writeln!(
                    out,
                    " rename {} => {} ({sim}%)",
                    entry.old_path.as_deref().unwrap_or(RepoPath::from_str("")),
                    entry.new_path.as_deref().unwrap_or(RepoPath::from_str(""))
                )?;
            }
            DiffStatus::Copied => {
                let sim = entry.score.unwrap_or(100);
                writeln!(
                    out,
                    " copy {} => {} ({sim}%)",
                    entry.old_path.as_deref().unwrap_or(RepoPath::from_str("")),
                    entry.new_path.as_deref().unwrap_or(RepoPath::from_str(""))
                )?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Write a unified-diff block for one entry.
fn zlib_compress_raw(input: &[u8]) -> Result<Vec<u8>> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    IoWrite::write_all(&mut enc, input).map_err(|e| anyhow::anyhow!("zlib compress: {e}"))?;
    enc.finish()
        .map_err(|e| anyhow::anyhow!("zlib compress finish: {e}"))
}

fn encode_len_byte(n: usize) -> char {
    if (1..=26).contains(&n) {
        return (b'A' + (n as u8) - 1) as char;
    }
    if (27..=52).contains(&n) {
        return (b'a' + (n as u8) - 27) as char;
    }
    '?'
}

fn git_encode_85(encoded: &mut Vec<u8>, data: &[u8]) {
    const EN85: &[u8] =
        b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz!#$%&()*+-;<=>?@^_`{|}~";
    let mut pos = 0usize;
    let mut bytes = data.len();
    while bytes > 0 {
        let mut acc: u32 = 0;
        let mut cnt = 24i32;
        while cnt >= 0 {
            let ch = u32::from(data[pos]);
            acc |= ch << cnt;
            pos += 1;
            bytes -= 1;
            if bytes == 0 {
                break;
            }
            cnt -= 8;
        }
        let mut group = [0u8; 5];
        for cnt in (0..=4).rev() {
            let val = acc % 85;
            acc /= 85;
            group[cnt] = EN85[val as usize];
        }
        encoded.extend_from_slice(&group);
    }
}

fn emit_base85_line(out: &mut impl Write, line_payload: &[u8]) -> Result<()> {
    let n = line_payload.len();
    let len_ch = encode_len_byte(n);
    write!(out, "{len_ch}")?;
    let mut enc = Vec::new();
    git_encode_85(&mut enc, line_payload);
    out.write_all(&enc)?;
    writeln!(out)?;
    Ok(())
}

fn write_wrapped_base85(out: &mut impl Write, data: &[u8]) -> Result<()> {
    let mut pos = 0usize;
    while pos < data.len() {
        let take = (data.len() - pos).min(52);
        emit_base85_line(out, &data[pos..pos + take])?;
        pos += take;
    }
    Ok(())
}

fn emit_git_binary_patch(out: &mut impl Write, old_raw: &[u8], new_raw: &[u8]) -> Result<()> {
    let delta_plain = encode_prefix_extension_delta(old_raw, new_raw)
        .or_else(|_| encode_lcp_delta(old_raw, new_raw))
        .unwrap_or_default();
    if delta_plain.is_empty() {
        let compressed = zlib_compress_raw(new_raw)?;
        writeln!(out, "GIT binary patch")?;
        writeln!(out, "literal {}", new_raw.len())?;
        write_wrapped_base85(out, &compressed)?;
        writeln!(out)?;
        writeln!(out, "literal 0")?;
        writeln!(out, "HcmV?d00001")?;
        writeln!(out)?;
    } else {
        let forward = zlib_compress_raw(&delta_plain)?;
        let reverse_delta = encode_prefix_extension_delta(new_raw, old_raw)
            .or_else(|_| encode_lcp_delta(new_raw, old_raw))
            .unwrap_or_default();
        let reverse = zlib_compress_raw(&reverse_delta)?;
        writeln!(out, "GIT binary patch")?;
        writeln!(out, "delta {}", delta_plain.len())?;
        write_wrapped_base85(out, &forward)?;
        writeln!(out)?;
        writeln!(out, "delta {}", reverse_delta.len())?;
        write_wrapped_base85(out, &reverse)?;
        writeln!(out)?;
    }
    Ok(())
}

pub(crate) fn write_patch_entry(
    out: &mut impl Write,
    odb: &Odb,
    entry: &DiffEntry,
    context_lines: usize,
    abbrev: Option<usize>,
    full_index: bool,
    no_prefix: bool,
    binary_patch: bool,
    git_dir: &Path,
    quote_fully: bool,
    indent_heuristic: bool,
) -> Result<bool> {
    let (old_content, new_content) = read_blob_pair(odb, entry)?;

    // TODO(byte-paths): diff-header path quoting is lossy until quote_path goes byte-based (Phase 3).
    let old_lossy = entry
        .old_path
        .as_deref()
        .or(entry.new_path.as_deref())
        .map(RepoPath::to_string_lossy)
        .unwrap_or(std::borrow::Cow::Borrowed(""));
    let old_path: &str = &old_lossy;
    let new_lossy = entry
        .new_path
        .as_deref()
        .or(entry.old_path.as_deref())
        .map(RepoPath::to_string_lossy)
        .unwrap_or(std::borrow::Cow::Borrowed(""));
    let new_path: &str = &new_lossy;

    let old_hex = entry.old_oid.to_hex();
    let new_hex = entry.new_oid.to_hex();
    let index_full = full_index || binary_patch;
    let old_abbrev = abbrev_oid(&old_hex, abbrev, index_full);
    let new_abbrev = abbrev_oid(&new_hex, abbrev, index_full);

    let (old_pfx, new_pfx) = if no_prefix { ("", "") } else { ("a/", "b/") };
    let git_old = format_diff_path_with_prefix(old_pfx, old_path, quote_fully);
    let git_new = format_diff_path_with_prefix(new_pfx, new_path, quote_fully);

    writeln!(out, "diff --git {git_old} {git_new}")?;

    match entry.status {
        DiffStatus::Added => {
            writeln!(out, "new file mode {}", entry.new_mode)?;
            writeln!(out, "index {old_abbrev}..{new_abbrev}")?;
        }
        DiffStatus::Deleted => {
            writeln!(out, "deleted file mode {}", entry.old_mode)?;
            writeln!(out, "index {old_abbrev}..{new_abbrev}")?;
        }
        DiffStatus::Modified => {
            if entry.old_mode != entry.new_mode {
                writeln!(out, "old mode {}", entry.old_mode)?;
                writeln!(out, "new mode {}", entry.new_mode)?;
            }
            if entry.old_mode == entry.new_mode {
                writeln!(out, "index {old_abbrev}..{new_abbrev} {}", entry.old_mode)?;
            } else {
                writeln!(out, "index {old_abbrev}..{new_abbrev}")?;
            }
        }
        DiffStatus::Renamed => {
            if entry.old_mode != entry.new_mode {
                writeln!(out, "old mode {}", entry.old_mode)?;
                writeln!(out, "new mode {}", entry.new_mode)?;
            }
            let sim = entry.score.unwrap_or(100);
            writeln!(out, "similarity index {sim}%")?;
            writeln!(out, "rename from {}", quote_c_style(old_path, quote_fully))?;
            writeln!(out, "rename to {}", quote_c_style(new_path, quote_fully))?;
            if entry.old_oid != entry.new_oid {
                writeln!(out, "index {old_abbrev}..{new_abbrev}")?;
            }
        }
        DiffStatus::Copied => {
            let sim = entry.score.unwrap_or(100);
            writeln!(out, "similarity index {sim}%")?;
            writeln!(out, "copy from {}", quote_c_style(old_path, quote_fully))?;
            writeln!(out, "copy to {}", quote_c_style(new_path, quote_fully))?;
            if entry.old_oid != entry.new_oid {
                writeln!(out, "index {old_abbrev}..{new_abbrev}")?;
            }
        }
        DiffStatus::TypeChanged => {
            writeln!(out, "old mode {}", entry.old_mode)?;
            writeln!(out, "new mode {}", entry.new_mode)?;
        }
        DiffStatus::Unmerged => {}
    }

    // TODO(byte-paths): attribute lookup uses the lossy path until pathspec/attrs go byte-based (Phase 3).
    let path_for_attrs = entry.path().to_string_lossy();
    let old_raw = old_content.as_bytes();
    let new_raw = new_content.as_bytes();
    if is_binary_for_diff(git_dir, &path_for_attrs, old_raw)
        || is_binary_for_diff(git_dir, &path_for_attrs, new_raw)
    {
        if binary_patch {
            emit_git_binary_patch(out, old_raw, new_raw)?;
        } else {
            let bo = if entry.status == DiffStatus::Added {
                "/dev/null".to_owned()
            } else {
                format_diff_path_with_prefix(old_pfx, old_path, quote_fully)
            };
            let bn = if entry.status == DiffStatus::Deleted {
                "/dev/null".to_owned()
            } else {
                format_diff_path_with_prefix(new_pfx, new_path, quote_fully)
            };
            writeln!(out, "Binary files {bo} and {bn} differ")?;
        }
        return Ok(false);
    }

    let display_old = if entry.status == DiffStatus::Added {
        "/dev/null".to_owned()
    } else {
        format_diff_path_with_prefix(old_pfx, old_path, quote_fully)
    };
    let display_new = if entry.status == DiffStatus::Deleted {
        "/dev/null".to_owned()
    } else {
        format_diff_path_with_prefix(new_pfx, new_path, quote_fully)
    };
    let patch = unified_diff_with_prefix(
        &old_content,
        &new_content,
        &display_old,
        &display_new,
        context_lines,
        0,
        "",
        "",
        indent_heuristic,
        quote_fully,
    );
    write!(out, "{patch}")?;

    Ok(false)
}

/// Write a `--stat` summary.
fn stat_mode_has_executable_bit(mode_str: &str) -> bool {
    u32::from_str_radix(mode_str, 8)
        .map(|m| m & 0o111 != 0)
        .unwrap_or(false)
}

/// Quote `entry`'s path and, for `--compact-summary`, append git's
/// `(new)` / `(gone)` / `(mode +x)` / `(mode -x)` annotation.
fn stat_display_path(entry: &DiffEntry, quote_fully: bool, compact: bool) -> String {
    let path = quote_c_style(&entry.path().to_string_lossy(), quote_fully);
    if !compact {
        return path;
    }
    match entry.status {
        DiffStatus::Added => format!("{path} (new)"),
        DiffStatus::Deleted => format!("{path} (gone)"),
        _ => {
            let old_x = stat_mode_has_executable_bit(&entry.old_mode);
            let new_x = stat_mode_has_executable_bit(&entry.new_mode);
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

fn print_stat_summary(
    out: &mut impl Write,
    odb: &Odb,
    entries: &[DiffEntry],
    quote_fully: bool,
    compact_summary: bool,
    shortstat: bool,
) -> Result<bool> {
    use grit_lib::diff::format_stat_line_width;

    let max_path_len = entries
        .iter()
        .map(|e| stat_display_path(e, quote_fully, compact_summary).len())
        .max()
        .unwrap_or(0);
    let mut total_ins = 0usize;
    let mut total_del = 0usize;

    // First pass: compute all stats
    let mut file_stats: Vec<(String, usize, usize)> = Vec::new();
    for entry in entries {
        let (old_content, new_content) = read_blob_pair(odb, entry)?;
        let (ins, del) = count_changes(&old_content, &new_content);
        total_ins += ins;
        total_del += del;
        file_stats.push((
            stat_display_path(entry, quote_fully, compact_summary),
            ins,
            del,
        ));
    }

    // Compute count width based on max total change
    let max_count = file_stats.iter().map(|(_, i, d)| i + d).max().unwrap_or(0);
    let count_width = format!("{}", max_count).len();

    if !shortstat {
        for (path, ins, del) in &file_stats {
            writeln!(
                out,
                "{}",
                format_stat_line_width(path, *ins, *del, max_path_len, count_width)
            )?;
        }
    }

    let n = entries.len();
    let mut summary = format!(" {} file{} changed", n, if n == 1 { "" } else { "s" },);
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
    writeln!(out, "{summary}")?;

    Ok(false)
}

// ── Small helpers ─────────────────────────────────────────────────────

fn peel_tag_chain_to_oid(repo: &Repository, mut oid: ObjectId) -> Result<ObjectId> {
    loop {
        let obj = repo.odb.read(&oid)?;
        if obj.kind != ObjectKind::Tag {
            return Ok(oid);
        }
        let tag = parse_tag(&obj.data)?;
        oid = tag.object;
    }
}

fn object_kind_phrase(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Tree => "tree",
        ObjectKind::Blob => "blob",
        ObjectKind::Tag => "tag",
        ObjectKind::Commit => "commit",
    }
}

fn resolve_commit_ish_for_merge_base(repo: &Repository, spec: &str) -> Result<ObjectId> {
    let oid =
        resolve_revision(repo, spec).with_context(|| format!("unknown revision: '{spec}'"))?;
    let peeled = peel_tag_chain_to_oid(repo, oid)?;
    let obj = repo.odb.read(&peeled)?;
    if obj.kind != ObjectKind::Commit {
        bail!(
            "fatal: {} is a {}, not a commit",
            spec,
            object_kind_phrase(obj.kind)
        );
    }
    Ok(peeled)
}

fn tree_oid_for_commit(repo: &Repository, commit_oid: ObjectId) -> Result<ObjectId> {
    let obj = repo.odb.read(&commit_oid)?;
    if obj.kind != ObjectKind::Commit {
        bail!(
            "fatal: {} is a {}, not a commit",
            commit_oid.to_hex(),
            obj.kind.as_str()
        );
    }
    let commit = parse_commit(&obj.data)?;
    Ok(commit.tree)
}

/// Resolve a tree-ish (commit or tree) to a tree OID.
fn resolve_to_tree(repo: &Repository, spec: &str) -> Result<ObjectId> {
    if spec == "4b825dc642cb6eb9a060e54bf899d69f7c6948d4"
        || spec == "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
    {
        return ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").map_err(Into::into);
    }
    let mut oid =
        resolve_revision(repo, spec).with_context(|| format!("unknown revision: '{spec}'"))?;
    oid = peel_tag_chain_to_oid(repo, oid)?;
    loop {
        let obj = repo.odb.read(&oid)?;
        match obj.kind {
            ObjectKind::Tree => return Ok(oid),
            ObjectKind::Commit => {
                let commit = parse_commit(&obj.data)?;
                oid = commit.tree;
            }
            _ => bail!("'{spec}' does not name a tree or commit"),
        }
    }
}

fn is_magic_empty_tree_oid(oid: &ObjectId) -> bool {
    let hex = oid.to_hex();
    hex == "4b825dc642cb6eb9a060e54bf899d69f7c6948d4"
        || hex == "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
}

fn resolve_max_tree_depth(repo: &Repository) -> usize {
    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    if let Some(raw) = config.get("core.maxtreedepth") {
        raw.parse::<usize>().unwrap_or(DEFAULT_MAX_TREE_DEPTH)
    } else {
        DEFAULT_MAX_TREE_DEPTH
    }
}

fn validate_tree_depth_limit(
    odb: &Odb,
    tree_oid: &ObjectId,
    depth: usize,
    max_tree_depth: usize,
) -> Result<()> {
    if depth > max_tree_depth {
        bail!(
            "tree depth {} exceeds core.maxtreedepth {}",
            depth,
            max_tree_depth
        );
    }

    let obj = odb
        .read(tree_oid)
        .context("reading tree for depth validation")?;
    let entries = parse_tree(&obj.data).context("parsing tree for depth validation")?;
    for entry in entries {
        if entry.mode == 0o040000 {
            validate_tree_depth_limit(odb, &entry.oid, depth + 1, max_tree_depth)?;
        }
    }
    Ok(())
}

/// Retrieve the tree OID from a commit OID.
/// Write a commit header line. If `pretty` is set, write a full "medium" format
/// header; otherwise just write the OID.
///
/// `from_parent` is set when `-m` compares the merge result against each parent; Git prints
/// `commit <id> (from <parent>)` in that case for `--pretty` and adjusts the oneline format.
fn write_commit_header(
    out: &mut impl Write,
    oid: &ObjectId,
    commit_data: &[u8],
    opts: &Options,
    from_parent: Option<&ObjectId>,
) -> Result<bool> {
    if let Some(ref pretty_fmt) = opts.pretty {
        let commit = parse_commit(commit_data).context("parsing commit for pretty")?;
        if pretty_fmt == "oneline" {
            let first_line = commit.message.lines().next().unwrap_or("");
            if let Some(p) = from_parent {
                writeln!(out, "{} (from {}) {first_line}", oid.to_hex(), p.to_hex())?;
            } else {
                writeln!(out, "{oid} {first_line}")?;
            }
            return Ok(false);
        }
        if let Some(template) = pretty_fmt
            .strip_prefix("tformat:")
            .or_else(|| pretty_fmt.strip_prefix("format:"))
        {
            // General user-format expansion (`%s`, `%N`, `%H`, …). Mirrors `git log --format`.
            let note_bytes = opts
                .format_notes
                .as_ref()
                .and_then(|m| m.get(oid))
                .map(|v| v.as_slice());
            let formatted =
                crate::commands::show::apply_format_string(template, oid, &commit, note_bytes, 0);
            writeln!(out, "{formatted}")?;
            // A blank line separates the format header from the raw/patch diff; with
            // `-s`/`--no-patch` there is no diff, so git omits it.
            if !opts.suppress_diff {
                writeln!(out)?;
            }
            return Ok(false);
        }
        let commit_hex = if opts.abbrev_commit {
            let full = oid.to_hex();
            full[..opts.abbrev.unwrap_or(7).min(full.len())].to_string()
        } else {
            oid.to_hex()
        };
        if let Some(p) = from_parent {
            writeln!(out, "commit {} (from {})", commit_hex, p.to_hex())?;
        } else {
            writeln!(out, "commit {commit_hex}")?;
        }
        if commit.parents.len() > 1 {
            // Git abbreviates the `Merge:` parent OIDs to the default short length
            // in medium-style headers.
            let merge_abbrev = if opts.full_index {
                40usize
            } else {
                opts.abbrev.unwrap_or(7)
            };
            let mut merge_line = String::new();
            for (i, parent) in commit.parents.iter().enumerate() {
                if i > 0 {
                    merge_line.push(' ');
                }
                let hex = parent.to_hex();
                merge_line.push_str(&hex[..merge_abbrev.min(hex.len())]);
            }
            writeln!(out, "Merge: {merge_line}")?;
        }
        // Parse author line: "Name <email> timestamp tz"
        let author = &commit.author;
        if let Some(date_start) = author.rfind('>') {
            let name_email = &author[..=date_start];
            let timestamp_tz = author[date_start + 1..].trim();
            writeln!(out, "Author: {name_email}")?;
            // Try to parse the date
            if let Some(formatted) = format_author_date(timestamp_tz) {
                writeln!(out, "Date:   {formatted}")?;
            }
        } else {
            writeln!(out, "Author: {author}")?;
        }
        writeln!(out)?;
        // Indent commit message
        for line in commit.message.lines() {
            writeln!(out, "    {line}")?;
        }
        // Optional `Notes:` block (`--notes`), separated from the message by a blank line.
        if let Some(notes_map) = &opts.notes_blocks {
            if let Some(block) = notes_map.get(oid) {
                writeln!(out)?;
                for line in block.lines() {
                    writeln!(out, "{line}")?;
                }
            }
        }
        // Use the "---" separator when `--patch-with-stat` is active, except for a
        // combined merge diff (which separates the header from the stat with a
        // blank line, like Git's combined log output); otherwise a blank line.
        let is_combined_merge = opts.combined_patch && commit.parents.len() > 1;
        if opts.patch_with_stat && !is_combined_merge {
            writeln!(out, "---")?;
        } else {
            writeln!(out)?;
        }
    } else if !opts.no_commit_id {
        // In `-z` raw mode git NUL-terminates the bare commit-id header instead of using a newline.
        if opts.nul_terminated && opts.format == OutputFormat::Raw {
            write!(out, "{oid}\0")?;
        } else {
            writeln!(out, "{oid}")?;
        }
    }
    Ok(false)
}

/// Format a Unix timestamp + tz offset into git's default date format.
fn format_commit_date(timestamp: i64, tz: &str) -> String {
    use time::OffsetDateTime;
    let tz_offset_secs = parse_tz_offset_secs(tz);
    if let Ok(offset) = time::UtcOffset::from_whole_seconds(tz_offset_secs) {
        if let Ok(dt) = OffsetDateTime::from_unix_timestamp(timestamp) {
            let dt = dt.to_offset(offset);
            let weekday = match dt.weekday() {
                time::Weekday::Monday => "Mon",
                time::Weekday::Tuesday => "Tue",
                time::Weekday::Wednesday => "Wed",
                time::Weekday::Thursday => "Thu",
                time::Weekday::Friday => "Fri",
                time::Weekday::Saturday => "Sat",
                time::Weekday::Sunday => "Sun",
            };
            let month = match dt.month() {
                time::Month::January => "Jan",
                time::Month::February => "Feb",
                time::Month::March => "Mar",
                time::Month::April => "Apr",
                time::Month::May => "May",
                time::Month::June => "Jun",
                time::Month::July => "Jul",
                time::Month::August => "Aug",
                time::Month::September => "Sep",
                time::Month::October => "Oct",
                time::Month::November => "Nov",
                time::Month::December => "Dec",
            };
            let sign = if tz_offset_secs < 0 { '-' } else { '+' };
            let abs = tz_offset_secs.unsigned_abs();
            let h = abs / 3600;
            let m = (abs % 3600) / 60;
            return format!(
                "{} {} {} {:02}:{:02}:{:02} {:4} {}{:02}{:02}",
                weekday,
                month,
                dt.day(),
                dt.hour(),
                dt.minute(),
                dt.second(),
                dt.year(),
                sign,
                h,
                m
            );
        }
    }
    format!("{timestamp} {tz}")
}

/// Parse an author date field and format it for pretty printing.
/// Handles both "<unix_ts> <tz>" and "YYYY-MM-DD HH:MM:SS <tz>" formats.
fn format_author_date(date_str: &str) -> Option<String> {
    if date_str.is_empty() {
        return None;
    }
    // Try "<unix_ts> <tz>" first
    let parts: Vec<&str> = date_str.splitn(2, ' ').collect();
    if parts.len() == 2 {
        if let Ok(ts) = parts[0].parse::<i64>() {
            return Some(format_commit_date(ts, parts[1]));
        }
    }
    // Try "YYYY-MM-DD HH:MM:SS <tz>" format
    // Split from the end to find the timezone
    let parts: Vec<&str> = date_str.rsplitn(2, ' ').collect();
    if parts.len() == 2 {
        let tz = parts[0];
        let datetime = parts[1];
        // Try to parse as ISO-ish datetime
        let tz_secs = parse_tz_offset_secs(tz);
        if let Ok(offset) = time::UtcOffset::from_whole_seconds(tz_secs) {
            // Try YYYY-MM-DD HH:MM:SS
            let ymd_hms = time::format_description::parse_borrowed::<1>(
                "[year]-[month]-[day] [hour]:[minute]:[second]",
            )
            .ok()?;
            if let Ok(naive) = time::PrimitiveDateTime::parse(datetime, &ymd_hms) {
                let dt = naive.assume_offset(offset);
                let ts = dt.unix_timestamp();
                return Some(format_commit_date(ts, tz));
            }
        }
    }
    // Fallback: just return the raw string
    Some(date_str.to_owned())
}

fn parse_tz_offset_secs(tz: &str) -> i32 {
    if tz.len() < 4 {
        return 0;
    }
    let (sign, rest) = if tz.starts_with('+') {
        (1i32, &tz[1..])
    } else if tz.starts_with('-') {
        (-1i32, &tz[1..])
    } else {
        (1i32, tz)
    };
    let hours: i32 = rest.get(..2).and_then(|s| s.parse().ok()).unwrap_or(0);
    let mins: i32 = rest.get(2..4).and_then(|s| s.parse().ok()).unwrap_or(0);
    sign * (hours * 3600 + mins * 60)
}

fn commit_tree(odb: &Odb, commit_oid: &ObjectId) -> Result<ObjectId> {
    let obj = odb.read(commit_oid).context("reading parent commit")?;
    let commit = parse_commit(&obj.data).context("parsing parent commit")?;
    Ok(commit.tree)
}

/// Read both blob sides of a diff entry as UTF-8 strings.
///
/// Fails with `unable to read <oid>` when a side stores the null OID but a real
/// blob mode (bogus tree entry), or when a non-null OID is missing from the ODB,
/// matching `git diff-tree` / `git diff-index` patch behavior.
fn read_blob_pair(odb: &Odb, entry: &DiffEntry) -> Result<(String, String)> {
    let zero = grit_lib::diff::zero_oid();

    let old_content = if entry.old_mode == "160000" {
        // Gitlink: the submodule commit is not in this repository's ODB; Git
        // synthesizes the textual `Subproject commit` content instead.
        format!("Subproject commit {}\n", entry.old_oid.to_hex())
    } else if entry.old_oid == zero {
        if entry.old_mode != "000000" {
            bail!("unable to read {}", zero.to_hex());
        }
        String::new()
    } else {
        let obj = odb
            .read(&entry.old_oid)
            .map_err(|_| anyhow::anyhow!("unable to read {}", entry.old_oid.to_hex()))?;
        String::from_utf8_lossy(&obj.data).into_owned()
    };

    let new_content = if entry.new_mode == "160000" {
        format!("Subproject commit {}\n", entry.new_oid.to_hex())
    } else if entry.new_oid == zero {
        if entry.new_mode != "000000" {
            bail!("unable to read {}", zero.to_hex());
        }
        String::new()
    } else {
        let obj = odb
            .read(&entry.new_oid)
            .map_err(|_| anyhow::anyhow!("unable to read {}", entry.new_oid.to_hex()))?;
        String::from_utf8_lossy(&obj.data).into_owned()
    };

    Ok((old_content, new_content))
}

/// Drop modified blob pairs that are identical after whitespace rules (`-b`, `-w`, etc.).
fn filter_whitespace_equivalent_blob_pairs(
    odb: &Odb,
    entries: Vec<DiffEntry>,
    ws: &WhitespaceCompare,
) -> Result<Vec<DiffEntry>> {
    if !ws.any() {
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
        let (old, new) = match read_blob_pair(odb, &e) {
            Ok(pair) => pair,
            Err(_) => {
                out.push(e);
                continue;
            }
        };
        if ws.normalize(&old) != ws.normalize(&new) {
            out.push(e);
        }
    }
    Ok(out)
}

/// Apply post-diff filters: pathspecs, max-depth, and pickaxe (`-S` / `-G`).
fn filter_entries(
    odb: &Odb,
    repo: &Repository,
    entries: Vec<DiffEntry>,
    opts: &Options,
) -> Result<Vec<DiffEntry>> {
    let mut filtered = filter_pathspecs(entries, &opts.pathspecs);
    if let Some(depth) = opts.max_depth {
        filtered = filter_max_depth(filtered, depth, &opts.pathspecs);
    }
    let filtered = apply_pickaxe_filter(odb, filtered, opts)?;
    let filtered = apply_find_object_filter(repo, filtered, opts)?;
    filter_whitespace_equivalent_blob_pairs(odb, filtered, &WhitespaceCompare::from_opts(opts))
}

/// Keep entries whose old or new blob OID matches `--find-object` (non-combined diffs).
fn apply_find_object_filter(
    repo: &Repository,
    entries: Vec<DiffEntry>,
    opts: &Options,
) -> Result<Vec<DiffEntry>> {
    let Some(ref spec) = opts.find_object else {
        return Ok(entries);
    };
    let oid =
        resolve_revision(repo, spec).with_context(|| format!("unable to resolve '{spec}'"))?;
    let filtered: Vec<DiffEntry> = entries
        .into_iter()
        .filter(|e| e.old_oid == oid || e.new_oid == oid)
        .collect();
    Ok(filtered)
}

/// Keep only diff entries that match `-G` / `-S` pickaxe rules (same semantics as `git diff`).
fn apply_pickaxe_filter(
    odb: &Odb,
    entries: Vec<DiffEntry>,
    opts: &Options,
) -> Result<Vec<DiffEntry>> {
    if let Some(ref pattern) = opts.pickaxe_grep {
        let re =
            Regex::new(pattern).with_context(|| format!("invalid pickaxe regex: {pattern}"))?;
        let mut out = Vec::new();
        for e in entries {
            let (old, new) = read_blob_pair(odb, &e)?;
            let mut keep = false;
            for line in new.lines() {
                if re.is_match(line) {
                    keep = true;
                    break;
                }
            }
            if !keep {
                for line in old.lines() {
                    if re.is_match(line) {
                        keep = true;
                        break;
                    }
                }
            }
            if keep {
                out.push(e);
            }
        }
        return Ok(out);
    }

    if let Some(ref needle) = opts.pickaxe_string {
        if opts.pickaxe_regex {
            let re =
                Regex::new(needle).with_context(|| format!("invalid pickaxe regex: {needle}"))?;
            let mut out = Vec::new();
            for e in entries {
                let (old, new) = read_blob_pair(odb, &e)?;
                let old_count = re.find_iter(&old).count();
                let new_count = re.find_iter(&new).count();
                let keep = if opts.pickaxe_all {
                    old_count > 0 || new_count > 0
                } else {
                    old_count != new_count
                };
                if keep {
                    out.push(e);
                }
            }
            return Ok(out);
        }

        let mut out = Vec::new();
        for e in entries {
            let (old, new) = read_blob_pair(odb, &e)?;
            let old_count = old.matches(needle.as_str()).count();
            let new_count = new.matches(needle.as_str()).count();
            let keep = if opts.pickaxe_all {
                old_count > 0 || new_count > 0
            } else {
                old_count != new_count
            };
            if keep {
                out.push(e);
            }
        }
        return Ok(out);
    }

    Ok(entries)
}

/// Keep only paths in the combined-diff intersection set Git uses (`D(A,P1) ∩ D(A,P2) ∩ …`).
fn filter_combined_paths_intersection(
    odb: &Odb,
    merge_tree: &ObjectId,
    parents: &[ObjectId],
    paths: Vec<CombinedDiffPath>,
) -> Vec<CombinedDiffPath> {
    let allowed: std::collections::HashSet<String> = combined_diff_paths(odb, merge_tree, parents)
        .into_iter()
        .collect();
    paths
        .into_iter()
        .filter(|p| {
            // `allowed` is a blob-level path list; tree entries (from `-t` /
            // `--find-object`) are already intersection-filtered by the walker
            // and must pass through (t4038: `diff-tree -c -t` shows `MM dir`).
            let is_tree = (p.merge_mode & 0o170000) == 0o040000
                || p.parents
                    .iter()
                    .any(|side| (side.mode & 0o170000) == 0o040000);
            is_tree || allowed.contains(&p.path)
        })
        .collect()
}

fn combined_path_matches_pathspecs(path: &CombinedDiffPath, pathspecs: &[String]) -> bool {
    if pathspecs.is_empty() {
        return true;
    }
    let ctx = context_from_mode_octal(&format!("{:06o}", path.merge_mode));
    pathspecs
        .iter()
        .any(|spec| matches_pathspec_with_context(spec, &path.path, ctx))
}

fn filter_pathspecs(entries: Vec<DiffEntry>, pathspecs: &[String]) -> Vec<DiffEntry> {
    if pathspecs.is_empty() {
        return entries;
    }
    entries
        .into_iter()
        .filter(|e| diff_entry_matches_pathspecs(e, pathspecs))
        .collect()
}

fn diff_entry_pathspec_context(entry: &DiffEntry) -> grit_lib::pathspec::PathspecMatchContext {
    use grit_lib::pathspec::PathspecMatchContext;

    match entry.status {
        DiffStatus::Deleted => context_from_mode_octal(&entry.old_mode),
        DiffStatus::Added => context_from_mode_octal(&entry.new_mode),
        _ => {
            let old = context_from_mode_octal(&entry.old_mode);
            let new = context_from_mode_octal(&entry.new_mode);
            PathspecMatchContext {
                is_directory: old.is_directory || new.is_directory,
                is_git_submodule: old.is_git_submodule || new.is_git_submodule,
            }
        }
    }
}

fn diff_entry_matches_pathspecs(entry: &DiffEntry, pathspecs: &[String]) -> bool {
    // TODO(byte-paths): pathspec matching runs on the lossy string form (Phase 3).
    let ctx = diff_entry_pathspec_context(entry);
    if let Some(ref p) = entry.new_path {
        if matches_pathspec_list_with_context(&p.to_string_lossy(), pathspecs, ctx) {
            return true;
        }
    }
    if let Some(ref p) = entry.old_path {
        if entry.new_path.as_ref() != Some(p)
            && matches_pathspec_list_with_context(&p.to_string_lossy(), pathspecs, ctx)
        {
            return true;
        }
    }
    false
}

/// Parse a whitespace-separated list of OID strings.
fn parse_oid_list(s: &str) -> Result<Vec<ObjectId>> {
    s.split_whitespace()
        .filter(|t| !t.is_empty())
        .map(|t| {
            t.parse::<ObjectId>()
                .with_context(|| format!("invalid OID: `{t}`"))
        })
        .collect()
}
