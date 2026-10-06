//! `grit grep` — search tracked files for a pattern.

use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use regex::{Regex, RegexBuilder};
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;

use crate::commands::grep_expr::{
    collect_atom_indices, line_matches_expr, match_expr_eval, CompiledGrep, GrepExpr,
};
use crate::commands::grep_pattern::PatternToken;
use grit_lib::attributes::load_gitattributes_for_diff;
use grit_lib::attributes::quote_path_for_check_attr;
use grit_lib::config::ConfigSet;
use grit_lib::index::{MODE_GITLINK, MODE_TREE};
use grit_lib::merge_diff::{blob_text_for_diff, blob_text_for_diff_with_oid, diff_textconv_active};
use grit_lib::objects::{parse_commit, parse_tree, ObjectId, ObjectKind};
use grit_lib::promisor::repo_treats_promisor_packs;
use grit_lib::refs::resolve_ref;
use grit_lib::repo::Repository;
use grit_lib::rev_parse::{discover_optional, resolve_revision, show_prefix, split_treeish_colon};
use grit_lib::sparse_checkout::clear_skip_worktree_from_present_files;
use grit_lib::userdiff::{matcher_for_path_parsed, FuncnameMatcher};
use grit_lib::wildmatch::wildmatch;

use crate::explicit_exit::ExplicitExit;
use crate::pathspec::resolve_pathspec;

/// Quoted path for stderr / binary messages: cwd-relative like match lines (t7810 subdir paths).
fn grep_output_path(repo: &Repository, path_prefix: &str, path_str: &str, args: &Args) -> String {
    let rel = worktree_display_rel(repo, path_prefix, path_str, args);
    path_for_output(&rel, args)
}

/// Arguments for `grit grep`.
///
/// Git uses `-h` for `--no-filename`, not for short help (see `main` dispatch). Clap's default
/// `-h`/`--help` would steal `-h` and break `git grep -h` / t0014-alias.
#[derive(Debug, ClapArgs)]
#[command(disable_help_flag = true)]
pub struct Args {
    /// Show line numbers.
    #[arg(short = 'n', long = "line-number")]
    pub line_number: bool,

    /// Suppress line numbers (overrides -n).
    #[arg(long = "no-line-number", hide = true)]
    pub no_line_number: bool,

    /// Show count of matching lines per file.
    #[arg(short = 'c', long = "count")]
    pub count: bool,

    /// Suppress filename prefix on output.
    #[arg(long = "no-filename")]
    pub no_filename: bool,

    /// Force filename prefix on output.
    #[arg(short = 'H', long = "with-filename")]
    pub with_filename: bool,

    /// Show only filenames with matches.
    #[arg(short = 'l', long = "files-with-matches")]
    pub files_with_matches: bool,

    /// Show only filenames without matches.
    #[arg(short = 'L', long = "files-without-match")]
    pub files_without_match: bool,

    /// Case insensitive matching.
    #[arg(short = 'i', long = "ignore-case")]
    pub ignore_case: bool,

    /// Match whole words only.
    #[arg(short = 'w', long = "word-regexp")]
    pub word_regexp: bool,

    /// Process binary files as if they were text.
    #[arg(short = 'a', long = "text")]
    pub text_mode: bool,

    /// Don't match patterns in binary files.
    #[arg(short = 'I')]
    pub ignore_binary: bool,

    /// Invert match (show non-matching lines).
    #[arg(short = 'v', long = "invert-match")]
    pub invert_match: bool,

    /// Use extended regular expressions.
    #[arg(short = 'E', long = "extended-regexp")]
    pub extended_regexp: bool,

    /// Use Perl-compatible regular expressions.
    #[arg(short = 'P', long = "perl-regexp")]
    pub perl_regexp: bool,

    /// Use fixed strings (literal matching, no regex).
    #[arg(short = 'F', long = "fixed-strings")]
    pub fixed_strings: bool,

    /// Use basic regular expressions (default).
    #[arg(short = 'G', long = "basic-regexp")]
    pub basic_regexp: bool,

    /// Search blobs registered in the index file instead of the work tree.
    #[arg(long = "cached")]
    pub cached: bool,

    /// Require all patterns to match (line-level AND).
    #[arg(long = "all-match")]
    pub all_match: bool,

    /// Show the whole function as context.
    #[arg(short = 'W', long = "function-context")]
    pub function_context: bool,

    /// Show the preceding function line.
    #[arg(short = 'p', long = "show-function")]
    pub show_function: bool,

    /// Limit matches per file.
    #[arg(
        short = 'm',
        long = "max-count",
        value_name = "NUM",
        allow_negative_numbers = true
    )]
    pub max_count: Option<i64>,

    /// Suppress output; exit with status 0 on match.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Number of threads to use (accepted but ignored).
    #[arg(long = "threads", value_name = "N")]
    pub threads: Option<usize>,

    /// Show column number of first match.
    #[arg(long = "column")]
    pub column: bool,

    /// Show context lines after match.
    #[arg(
        short = 'A',
        long = "after-context",
        value_name = "NUM",
        allow_hyphen_values = true
    )]
    pub after_context: Option<usize>,

    /// Show context lines before match.
    #[arg(
        short = 'B',
        long = "before-context",
        value_name = "NUM",
        allow_hyphen_values = true
    )]
    pub before_context: Option<usize>,

    /// Show context lines before and after match.
    #[arg(
        short = 'C',
        long = "context",
        value_name = "NUM",
        allow_hyphen_values = true
    )]
    pub context: Option<usize>,

    /// Only print the matched parts of a matching line.
    #[arg(short = 'o', long = "only-matching")]
    pub only_matching: bool,

    /// Descend at most <depth> levels of directories.
    #[arg(
        long = "max-depth",
        value_name = "DEPTH",
        allow_negative_numbers = true
    )]
    pub max_depth: Option<i64>,

    /// Recurse into subdirectories (default, same as --max-depth=-1).
    #[arg(long = "recursive", short = 'r')]
    pub recursive: bool,

    /// Do not recurse into subdirectories (same as --max-depth=0).
    #[arg(long = "no-recursive")]
    pub no_recursive: bool,

    /// Show the full path of the file relative to the top-level directory.
    #[arg(long = "full-name")]
    pub full_name: bool,

    /// Use NUL as filename delimiter in output.
    #[arg(short = 'z', long = "null")]
    pub null_following_name: bool,

    /// Print an empty line between matches from different files.
    #[arg(long = "break")]
    pub file_break: bool,

    /// Show the filename above matches from that file instead of prefixing each line.
    #[arg(long = "heading")]
    pub heading: bool,

    /// Use color in output: always, never, auto.
    #[arg(
        long = "color",
        value_name = "WHEN",
        default_value = "never",
        default_missing_value = "always",
        num_args = 0..=1,
        require_equals = true
    )]
    pub color: String,

    /// Effective grep color slots.
    #[arg(skip)]
    pub colors: GrepColors,

    /// Recurse into submodules.
    #[arg(long = "recurse-submodules")]
    pub recurse_submodules: bool,

    /// Do not recurse into submodules (overrides config).
    #[arg(long = "no-recurse-submodules")]
    pub no_recurse_submodules: bool,

    /// Search also in untracked files.
    #[arg(long = "untracked")]
    pub untracked: bool,

    /// Search files not managed by Git (implies --untracked).
    #[arg(long = "no-index")]
    pub no_index: bool,

    /// Honor standard ignore files in filesystem searches.
    #[arg(long = "exclude-standard")]
    pub exclude_standard: bool,

    /// Do not honor standard ignore files in filesystem searches.
    #[arg(long = "no-exclude-standard")]
    pub no_exclude_standard: bool,

    /// Use `diff.<driver>.textconv` when `diff=<driver>` applies (search converted bytes).
    #[arg(long = "textconv")]
    pub textconv: bool,

    /// Do not use textconv filter.
    #[arg(long = "no-textconv")]
    pub no_textconv: bool,

    /// Positional arguments: [pattern] [<tree>] [-- pathspec...]
    #[arg(trailing_var_arg = true)]
    pub positional: Vec<String>,

    /// Set by `main` after stripping `-O` / `--open-files-in-pager` (Git does not consume the next argv as the pager unless it is glued: `-Opager`).
    #[arg(skip)]
    pub open_in_pager: bool,

    /// Explicit pager from `-Opager` or `--open-files-in-pager=pager`. When `None` with `open_in_pager`, resolve like Git (GIT_PAGER / config / PAGER).
    #[arg(skip)]
    pub open_pager_cmd: Option<String>,
}

impl Args {
    fn before_ctx(&self) -> usize {
        self.context.or(self.before_context).unwrap_or(0)
    }
    fn after_ctx(&self) -> usize {
        self.context.or(self.after_context).unwrap_or(0)
    }
    fn use_color(&self) -> bool {
        self.color == "always"
    }
    fn has_context(&self) -> bool {
        self.before_ctx() > 0 || self.after_ctx() > 0
    }
    fn show_line_number(&self) -> bool {
        // effective_line_number is set in run() to account for config
        self.line_number && !self.no_line_number
    }
    fn show_filename(&self) -> bool {
        if self.no_filename {
            return false;
        }
        true // default: show filename
    }
    /// Effective max depth: None means unlimited, Some(n) means limit to depth n.
    fn effective_max_depth(&self) -> Option<usize> {
        if self.no_recursive {
            return Some(0);
        }
        match self.max_depth {
            Some(d) if d < 0 => None, // -1 means unlimited
            Some(d) => Some(d as usize),
            None => None, // default: unlimited
        }
    }

    fn sep_byte(&self) -> u8 {
        if self.null_following_name {
            0
        } else {
            b':'
        }
    }
}

/// Strip `-O` / `--open-files-in-pager` from argv before clap parsing.
///
/// Git only takes the pager from the same argv cell as `-O` (`-Opager`). A separate token after
/// lone `-O` is not the pager (it is usually the pattern).
pub fn preprocess_open_in_pager_argv(argv: Vec<String>) -> (Vec<String>, bool, Option<String>) {
    let mut open_in_pager = false;
    let mut open_pager_cmd: Option<String> = None;
    let mut out = Vec::with_capacity(argv.len());
    for a in argv {
        if a == "-O" || a == "--open-files-in-pager" {
            open_in_pager = true;
            continue;
        }
        if let Some(v) = a.strip_prefix("--open-files-in-pager=") {
            open_in_pager = true;
            if !v.is_empty() {
                open_pager_cmd = Some(v.to_string());
            }
            continue;
        }
        if a.len() >= 2 {
            let b = a.as_bytes();
            if b[0] == b'-' && b[1] == b'O' {
                open_in_pager = true;
                if a.len() > 2 {
                    open_pager_cmd = Some(a[2..].to_string());
                }
                continue;
            }
        }
        out.push(a);
    }
    (out, open_in_pager, open_pager_cmd)
}

/// Entry point after `main` peels pattern-expression tokens from argv.
pub fn run_with_pattern_tokens(pattern_tokens: Vec<PatternToken>, args: Args) -> Result<()> {
    run_inner(pattern_tokens, args)
}

/// Run `grit grep` (no pre-parsed pattern tokens).
pub fn run(args: Args) -> Result<()> {
    run_inner(Vec::new(), args)
}

fn run_inner(pattern_tokens: Vec<PatternToken>, mut args: Args) -> Result<()> {
    let has_attr_src = std::env::var("GIT_ATTR_SOURCE")
        .ok()
        .filter(|s| !s.is_empty())
        .is_some();
    if has_attr_src {
        let repo_opt = discover_optional(None)?;
        if repo_opt.is_none() {
            bail!("fatal: cannot use --attr-source or GIT_ATTR_SOURCE without repo");
        }
    }

    let mut repo: Option<Repository> = None;
    let mut config = ConfigSet::load(None, true).ok();
    if args.no_index {
        if args.cached {
            bail!("fatal: --cached cannot be used with --no-index");
        }
        if let Ok(Some(r)) = discover_optional(None) {
            config = ConfigSet::load(Some(&r.git_dir), true).ok();
        }
    } else {
        match Repository::discover(None) {
            Ok(r) => {
                config = ConfigSet::load(Some(&r.git_dir), true).ok();
                repo = Some(r);
            }
            Err(err) => {
                let fallback = config
                    .as_ref()
                    .and_then(|c| {
                        c.get("grep.fallbacktonoindex")
                            .or_else(|| c.get("grep.fallbackToNoIndex"))
                    })
                    .is_some_and(|v| v == "true" || v == "1" || v == "yes");
                if fallback {
                    args.no_index = true;
                } else {
                    return Err(err).context("not a git repository");
                }
            }
        }
    }
    args.colors = GrepColors::from_config(config.as_ref());

    // Apply grep config settings
    {
        if let Some(ref c) = config {
            // grep.linenumber: if user didn't explicitly pass -n or --no-line-number
            if !args.line_number && !args.no_line_number {
                if let Some(val) = c.get("grep.linenumber") {
                    args.line_number = val == "true" || val == "1" || val == "yes";
                }
            }
            // grep.patternType / grep.extendedRegexp: affect regex mode
            // Only apply config if user didn't explicitly pass -E, -F, -P, or -G
            let user_set_type =
                args.extended_regexp || args.fixed_strings || args.perl_regexp || args.basic_regexp;
            if !user_set_type {
                let mut pattern_type_set = false;
                if let Some(pt) = c
                    .get("grep.patterntype")
                    .or_else(|| c.get("grep.patternType"))
                {
                    match pt.to_lowercase().as_str() {
                        "extended" => {
                            args.extended_regexp = true;
                            pattern_type_set = true;
                        }
                        "fixed" => {
                            args.fixed_strings = true;
                            pattern_type_set = true;
                        }
                        "perl" => {
                            args.perl_regexp = true;
                            pattern_type_set = true;
                        }
                        "basic" => {
                            pattern_type_set = true; /* BRE is default */
                        }
                        "default" => { /* fall through to grep.extendedRegexp */ }
                        _ => {}
                    }
                }
                // grep.extendedRegexp is only consulted if grep.patternType is unset or "default"
                if !pattern_type_set {
                    if let Some(val) = c
                        .get("grep.extendedregexp")
                        .or_else(|| c.get("grep.extendedRegexp"))
                    {
                        if val == "true" || val == "1" || val == "yes" {
                            args.extended_regexp = true;
                        }
                    }
                }
            }
            // Check grep.threads config
            if let Some(val) = c.get("grep.threads") {
                if val != "0" && val != "1" {
                    eprintln!("warning: no threads support, ignoring grep.threads");
                }
            }
            // submodule.recurse config: enable --recurse-submodules if not explicitly set
            if !args.recurse_submodules && !args.no_recurse_submodules {
                if let Some(val) = c.get("submodule.recurse") {
                    if val == "true" || val == "1" || val == "yes" {
                        args.recurse_submodules = true;
                    }
                }
            }
        }
    }

    // --no-recurse-submodules overrides config
    if args.no_recurse_submodules {
        args.recurse_submodules = false;
    }

    // --no-index: ignore --recurse-submodules silently
    if args.no_index {
        args.recurse_submodules = false;
    }

    // Incompatibility checks
    if args.recurse_submodules && args.untracked {
        bail!("option --untracked not supported with --recurse-submodules");
    }

    // Warn about unsupported threading
    if let Some(n) = args.threads {
        if n > 0 {
            eprintln!("warning: no threads support, ignoring --threads");
        }
    }

    // Positional: [pattern] [tree-ish] [-- pathspec...] — pattern only when no peeled `-e`/`-f`.
    let has_peeled_patterns = !pattern_tokens.is_empty();
    let repo_ref = repo.as_ref();
    let (first_pos_pattern, tree_ish, mut pathspecs) =
        parse_positional(&args, repo_ref, has_peeled_patterns)?;
    let user_supplied_pathspecs = !pathspecs.is_empty();
    let cwd = std::env::current_dir().context("cannot get current directory")?;
    if args.no_index {
        validate_no_index_pathspecs(&cwd, &pathspecs)?;
    }
    if let Some(r) = repo_ref {
        pathspecs = pathspecs_relative_to_cwd(r, &pathspecs);
    }
    // Git `parse_pathspec` with `PATHSPEC_PREFER_CWD`: no explicit paths => limit to cwd (t7811).
    // Use `.` so `resolve_pathspec` maps it to the current prefix only once (not `subdir/subdir`).
    if pathspecs.is_empty()
        && !args.no_index
        && !args.cached
        && repo_ref.and_then(|r| r.work_tree.as_ref()).is_some()
    {
        if let Some(r) = repo_ref {
            let pfx = show_prefix(r, &cwd);
            if !pfx.is_empty() {
                pathspecs.push(".".to_string());
            }
        }
    }
    let pathspec_prefix = repo_ref.and_then(|r| {
        r.work_tree
            .as_ref()
            .map(|_| show_prefix(r, &cwd))
            .filter(|p| !p.is_empty())
            .map(|mut p| {
                p.pop();
                p
            })
    });
    // `pathspecs_relative_to_cwd` already rebased user pathspecs to repo-relative paths; do not
    // apply `show_prefix` again in `resolve_pathspec` (would double-prefix under subdirs, t7810 -f).
    let resolve_prefix = if user_supplied_pathspecs {
        None
    } else {
        pathspec_prefix.as_deref()
    };
    pathspecs = if let Some(wt) = repo_ref.and_then(|r| r.work_tree.as_ref()) {
        pathspecs
            .into_iter()
            .map(|p| resolve_pathspec(&p, wt, resolve_prefix))
            .collect()
    } else {
        pathspecs
    };
    let implicit_cwd = repo_ref.and_then(|r| {
        let wt = r.work_tree.as_ref()?;
        let cwd = std::env::current_dir().ok()?;
        crate::pathspec::pathdiff(&cwd, wt)
    });
    pathspecs = grit_lib::pathspec::extend_pathspec_list_implicit_cwd(
        &pathspecs,
        implicit_cwd
            .as_deref()
            .map(|s| s.trim_end_matches('/'))
            .filter(|s| !s.is_empty()),
    );

    let single_rev_path = repo_ref.and_then(|r| {
        (!args.no_index && !args.cached && tree_ish.is_none() && pathspecs.len() == 1)
            .then(|| pathspecs[0].as_str())
            .and_then(split_treeish_colon)
            .filter(|(rev, path)| !rev.is_empty() && !path.is_empty())
            .and_then(|(rev, path)| {
                let spec = format!("{rev}:{path}");
                let oid = resolve_revision(r, &spec)
                    .or_else(|_| resolve_ref(&r.git_dir, &spec))
                    .or_else(|_| resolve_ref(&r.git_dir, &format!("refs/heads/{spec}")))
                    .ok()?;
                let obj = r.odb.read(&oid).ok()?;
                (obj.kind == ObjectKind::Blob).then(|| (rev.to_string(), path.to_string()))
            })
    });

    let mut pattern_tokens = pattern_tokens;
    if pattern_tokens.is_empty() {
        if let Some(p) = first_pos_pattern {
            for part in p.split('\n') {
                if !part.is_empty() {
                    pattern_tokens.push(PatternToken::Atom(part.to_string()));
                }
            }
        }
    }

    if pattern_tokens.is_empty() {
        bail!("no pattern given");
    }

    if args.open_in_pager && args.cached {
        bail!("--open-files-in-pager only works on the worktree");
    }
    if args.open_in_pager && tree_ish.is_some() {
        bail!("--open-files-in-pager only works on the worktree");
    }

    let (expr, atom_strings) = crate::commands::grep_expr::parse_pattern_tokens(&pattern_tokens)?;
    let compiled = build_compiled_grep(expr, &atom_strings, &args)?;
    if args.invert_match {
        args.only_matching = false;
    }

    for pat in &atom_strings {
        if !args.perl_regexp && pat.as_bytes().contains(&0) {
            bail!(
                "fatal: given pattern contains NULL byte (via -f <file>). This is only supported with -P under PCRE v2"
            );
        }
    }

    let mut open_paths: Option<Vec<String>> = if args.open_in_pager {
        Some(Vec::new())
    } else {
        None
    };

    if open_paths.is_some() {
        args.color = "never".to_string();
        args.files_with_matches = true;
    }

    let stdout = io::stdout();
    let mut out_handle = stdout.lock();
    let mut sink = io::sink();
    let out: &mut dyn Write = if args.quiet {
        &mut sink
    } else {
        &mut out_handle
    };
    // Tracks whether we need a "--" separator before the next context group
    let mut need_sep = false;

    let found_any = if args.no_index {
        let start_dir = std::env::current_dir().context("cannot get current directory")?;
        grep_filesystem(
            &start_dir,
            "",
            &compiled,
            &args,
            &pathspecs,
            &mut need_sep,
            out,
            &mut open_paths,
        )?
    } else {
        let repo = repo.as_ref().context("not a git repository")?;
        if let Some((rev, file_path)) = single_rev_path {
            let diff_attrs = if let Some(ref wt) = repo.work_tree {
                let wt_attrs = load_diff_attrs(wt);
                if wt_attrs.is_empty() {
                    load_diff_attrs_from_index(repo)
                } else {
                    wt_attrs
                }
            } else {
                load_diff_attrs_from_index(repo)
            };
            let rev_path_label = format!("{rev}:{file_path}");
            grep_one_blob_at_revision(
                repo,
                &rev,
                &file_path,
                &rev_path_label,
                &compiled,
                &args,
                &diff_attrs,
                &mut need_sep,
                out,
                &mut open_paths,
            )?
        } else if let Some(tree_spec) = &tree_ish {
            let oid = resolve_revision(repo, tree_spec)
                .or_else(|_| resolve_ref(&repo.git_dir, tree_spec))
                .or_else(|_| resolve_ref(&repo.git_dir, &format!("refs/heads/{tree_spec}")))
                .with_context(|| format!("not a valid revision: '{tree_spec}'"))?;

            let obj = read_object_for_grep(repo, &oid)?;
            let tree_oid = if obj.kind == ObjectKind::Commit {
                let commit = parse_commit(&obj.data)?;
                commit.tree
            } else if obj.kind == ObjectKind::Tree {
                oid
            } else {
                bail!("'{}' is not a tree-ish", tree_spec);
            };

            let tree_obj = read_object_for_grep(repo, &tree_oid)?;
            let diff_attrs = if let Some(ref wt) = repo.work_tree {
                let wt_attrs = load_diff_attrs(wt);
                if wt_attrs.is_empty() {
                    load_diff_attrs_from_index(repo)
                } else {
                    wt_attrs
                }
            } else {
                load_diff_attrs_from_index(repo)
            };
            grep_tree(
                repo,
                &tree_obj.data,
                &tree_oid,
                "",
                0,
                &compiled,
                &args,
                &pathspecs,
                Some(tree_spec),
                &mut need_sep,
                out,
                &diff_attrs,
                &mut open_paths,
            )?
        } else if args.cached {
            grep_cached(
                repo,
                "",
                &compiled,
                &args,
                &pathspecs,
                &mut need_sep,
                out,
                &mut open_paths,
            )?
        } else {
            grep_worktree(
                repo,
                "",
                &compiled,
                &args,
                &pathspecs,
                &mut need_sep,
                out,
                &mut open_paths,
            )?
        }
    };

    if found_any {
        if let Some(paths) = open_paths {
            let pager_cmd = match &args.open_pager_cmd {
                Some(s) if !s.is_empty() => s.clone(),
                _ => resolve_git_pager(&config),
            };
            // Git runs the pager with the same cwd as `git grep` (t7811 `run from subdir`);
            // file arguments are paths relative to that cwd, same as normal `-l` output.
            let pager_cwd = std::env::current_dir().context("cannot get current directory")?;
            run_open_in_pager(
                pager_cwd.as_path(),
                &pager_cmd,
                &atom_strings,
                args.ignore_case,
                &paths,
            )?;
        }
        Ok(())
    } else {
        std::process::exit(1);
    }
}

/// Resolve pager like `git var GIT_PAGER`: env, then `core.pager`, then `PAGER`, then `cat`.
fn resolve_git_pager(config: &Option<ConfigSet>) -> String {
    std::env::var("GIT_PAGER")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            config
                .as_ref()
                .and_then(|c| c.get("core.pager"))
                .filter(|s| !s.is_empty())
        })
        .or_else(|| std::env::var("PAGER").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "cat".to_owned())
}

/// True if the pager command must be run via `sh -c` (Git's `prepare_shell_cmd` heuristic).
fn pager_needs_shell(pager_cmd: &str) -> bool {
    const META: &[u8] = b"|&;<>()$`\\\"' \t\n*?[#~=%";
    pager_cmd.as_bytes().iter().any(|b| META.contains(b))
}

/// Strip leading directory from pager argv0 when Git would (path ending in `/xxxx`).
fn pager_executable_for_argv0(pager_cmd: &str) -> String {
    let b = pager_cmd.as_bytes();
    let len = b.len();
    if len > 4 && (b[len - 5] == b'/' || b[len - 5] == b'\\') {
        pager_cmd[len - 4..].to_owned()
    } else {
        pager_cmd.to_owned()
    }
}

/// Run the pager with collected file paths, matching Git's `run_pager` + `prepare_shell_cmd`.
fn run_open_in_pager(
    work_dir: &Path,
    pager_cmd: &str,
    patterns: &[String],
    ignore_case: bool,
    files: &[String],
) -> Result<()> {
    let exec0 = pager_executable_for_argv0(pager_cmd);
    let base = Path::new(&exec0)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&exec0);

    let mut extra: Vec<String> = Vec::new();
    if patterns.len() == 1 {
        let pat = &patterns[0];
        if base == "less" || base == "vi" {
            if ignore_case && base == "less" {
                extra.push("-I".to_string());
            }
            let star = if base == "less" { "*" } else { "" };
            extra.push(format!("+/{star}{pat}"));
        }
    }

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let status = if pager_needs_shell(pager_cmd) {
        let script = if files.is_empty() {
            pager_cmd.to_string()
        } else {
            format!("{pager_cmd} \"$@\"")
        };
        // Match Git's `prepare_shell_cmd` (git/run-command.c): with extra argv, use
        // `sh -c 'cmd "$@"' cmd ...` so `$0` is the pager and files are in `$@`.
        let mut cmd = Command::new(&shell);
        cmd.current_dir(work_dir).arg("-c").arg(&script);
        if !files.is_empty() {
            cmd.arg(pager_cmd);
        }
        cmd.args(extra.iter()).args(files.iter()).status()
    } else {
        Command::new(pager_cmd)
            .current_dir(work_dir)
            .args(extra.iter())
            .args(files.iter())
            .status()
    }
    .context("failed to run pager for --open-files-in-pager")?;

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

/// `git grep` with a single `rev:path` pathspec (e.g. `HEAD:a`): search one blob from that revision.
#[allow(clippy::too_many_arguments)]
fn grep_one_blob_at_revision(
    repo: &Repository,
    rev: &str,
    file_path: &str,
    rev_path_label: &str,
    compiled: &CompiledGrep,
    args: &Args,
    diff_attrs: &[DiffAttrRule],
    need_sep: &mut bool,
    out: &mut (impl Write + ?Sized),
    open_paths: &mut Option<Vec<String>>,
) -> Result<bool> {
    let spec = rev_path_label.to_string();
    let blob_oid = resolve_revision(repo, &spec)
        .or_else(|_| resolve_ref(&repo.git_dir, &spec))
        .or_else(|_| resolve_ref(&repo.git_dir, &format!("refs/heads/{spec}")))
        .with_context(|| format!("not a valid object: '{spec}'"))?;
    let obj = repo
        .odb
        .read(&blob_oid)
        .with_context(|| format!("object not found: {blob_oid}"))?;
    if obj.kind != ObjectKind::Blob {
        bail!("'{spec}' is not a blob");
    }
    let binary_override = check_binary_override(diff_attrs, file_path);
    let is_binary = grep_is_binary(repo, file_path, &obj.data, args, binary_override);
    if is_binary && args.ignore_binary {
        return Ok(false);
    }
    let rel = cwd_strip_repo_rel(repo, file_path, args);
    if is_binary && !args.text_mode {
        if args.count || args.files_with_matches || args.files_without_match || args.quiet {
            let content = blob_as_grep_text(
                repo,
                file_path,
                &obj.data,
                Some(&blob_oid),
                args,
                binary_override,
            );
            return grep_content(
                &rel,
                None,
                Some(rev),
                &content,
                compiled,
                args,
                None,
                need_sep,
                out,
                open_paths,
            );
        }
        let content = blob_as_grep_text(
            repo,
            file_path,
            &obj.data,
            Some(&blob_oid),
            args,
            binary_override,
        );
        let has_match = compiled
            .atoms
            .iter()
            .any(|r| r.as_ref().is_some_and(|re| re.is_match(&content)));
        if has_match {
            if !args.quiet {
                let quoted = path_for_output(file_path, args);
                let display = format!("{rev}:{quoted}");
                writeln!(out, "Binary file {} matches", display)?;
            }
            return Ok(true);
        }
        return Ok(false);
    }
    let content = blob_as_grep_text(
        repo,
        file_path,
        &obj.data,
        Some(&blob_oid),
        args,
        binary_override,
    );
    grep_content(
        &rel,
        None,
        Some(rev),
        &content,
        compiled,
        args,
        None,
        need_sep,
        out,
        open_paths,
    )
}

/// Grep the index (--cached mode), optionally recursing into submodules.
/// `path_prefix` is prepended to filenames for submodule display (e.g. "submodule/").
#[allow(clippy::too_many_arguments)]
fn grep_cached(
    repo: &Repository,
    path_prefix: &str,
    compiled: &CompiledGrep,
    args: &Args,
    pathspecs: &[String],
    need_sep: &mut bool,
    out: &mut (impl Write + ?Sized),
    open_paths: &mut Option<Vec<String>>,
) -> Result<bool> {
    let index = repo.load_index().context("loading index")?;
    // Load diff attrs from index (for --cached, use index attrs) or worktree
    let diff_attrs = if let Some(ref wt) = repo.work_tree {
        // Try worktree first, fallback to index
        let wt_attrs = load_diff_attrs(wt);
        if wt_attrs.is_empty() {
            load_diff_attrs_from_index(repo)
        } else {
            wt_attrs
        }
    } else {
        load_diff_attrs_from_index(repo)
    };
    let mut seen_stage0_paths = std::collections::HashSet::new();
    let mut found_any = false;

    for entry in &index.entries {
        let path_str = String::from_utf8_lossy(&entry.path).to_string();
        let full_path = if path_prefix.is_empty() {
            path_str.clone()
        } else {
            format!("{path_prefix}{path_str}")
        };
        let output_path = grep_output_path(repo, path_prefix, &path_str, args);

        // Pathspec filtering is relative to the superproject
        let is_submodule = entry.mode == MODE_GITLINK;
        let ps_ctx = grit_lib::pathspec::PathspecMatchContext {
            is_directory: false,
            is_git_submodule: is_submodule,
        };
        let pathspec_matches_path =
            pathspecs.is_empty() || any_pathspec_matches(&full_path, pathspecs, ps_ctx);
        let pathspec_matches_descendant =
            is_submodule && pathspec_may_match_descendant(&full_path, pathspecs);
        if !pathspec_matches_path && !pathspec_matches_descendant {
            continue;
        }

        // `git grep -L --cached` lists tracked paths without matches; skip intent-to-add (t7810).
        if args.cached && args.files_without_match && entry.intent_to_add() {
            continue;
        }

        // Submodule entry (gitlink)
        if is_submodule {
            if entry.stage() != 0 {
                continue;
            }
            if !seen_stage0_paths.insert(path_str.clone()) {
                continue;
            }
            if args.recurse_submodules {
                if let Some(work_tree) = &repo.work_tree {
                    let sub_path = work_tree.join(&path_str);
                    if let Ok(sub_repo) = open_submodule_repo(&sub_path) {
                        let child_pathspecs: &[String] = if pathspec_matches_path {
                            &[]
                        } else {
                            pathspecs
                        };
                        if grep_cached(
                            &sub_repo,
                            &format!("{full_path}/"),
                            compiled,
                            args,
                            child_pathspecs,
                            need_sep,
                            out,
                            open_paths,
                        )? {
                            found_any = true;
                        }
                    }
                }
            }
            continue;
        }

        if let Some(max_depth) = args.effective_max_depth() {
            if !path_allowed_at_max_depth(&full_path, pathspecs, max_depth) {
                continue;
            }
        }

        if entry.stage() != 0 && entry.mode != MODE_GITLINK && entry.mode != MODE_TREE {
            let obj = match read_object_for_grep(repo, &entry.oid) {
                Ok(o) => o,
                Err(_) => continue,
            };
            let binary_override = check_binary_override(&diff_attrs, &path_str);
            let is_binary = grep_is_binary(repo, &path_str, &obj.data, args, binary_override);
            if is_binary && args.ignore_binary {
                continue;
            }
            if is_binary && !args.text_mode {
                if args.count || args.files_with_matches || args.files_without_match || args.quiet {
                    let content = blob_as_grep_text(
                        repo,
                        &path_str,
                        &obj.data,
                        Some(&entry.oid),
                        args,
                        binary_override,
                    );
                    let rel = worktree_display_rel(repo, path_prefix, &path_str, args);
                    let func_matcher = funcname_matcher_for_path(repo, &path_str);
                    if grep_content(
                        &rel,
                        None,
                        None,
                        &content,
                        compiled,
                        args,
                        func_matcher.as_ref(),
                        need_sep,
                        out,
                        open_paths,
                    )? {
                        found_any = true;
                    }
                } else {
                    let content = blob_as_grep_text(
                        repo,
                        &path_str,
                        &obj.data,
                        Some(&entry.oid),
                        args,
                        binary_override,
                    );
                    let has_match = compiled
                        .atoms
                        .iter()
                        .any(|r| r.as_ref().is_some_and(|re| re.is_match(&content)));
                    if has_match {
                        writeln!(out, "Binary file {} matches", output_path)?;
                        found_any = true;
                    }
                }
            } else {
                let content = blob_as_grep_text(
                    repo,
                    &path_str,
                    &obj.data,
                    Some(&entry.oid),
                    args,
                    binary_override,
                );
                let rel = worktree_display_rel(repo, path_prefix, &path_str, args);
                let func_matcher = funcname_matcher_for_path(repo, &path_str);
                if grep_content(
                    &rel,
                    None,
                    None,
                    &content,
                    compiled,
                    args,
                    func_matcher.as_ref(),
                    need_sep,
                    out,
                    open_paths,
                )? {
                    found_any = true;
                }
            }
            continue;
        }

        if entry.stage() != 0 {
            continue;
        }
        if !seen_stage0_paths.insert(path_str.clone()) {
            continue;
        }

        let obj = match read_object_for_grep(repo, &entry.oid) {
            Ok(o) => o,
            Err(_) => continue,
        };
        let binary_override = check_binary_override(&diff_attrs, &path_str);
        let is_binary = grep_is_binary(repo, &path_str, &obj.data, args, binary_override);
        if is_binary && args.ignore_binary {
            continue;
        }
        if is_binary && !args.text_mode {
            if args.count || args.files_with_matches || args.files_without_match || args.quiet {
                let content = blob_as_grep_text(
                    repo,
                    &path_str,
                    &obj.data,
                    Some(&entry.oid),
                    args,
                    binary_override,
                );
                let rel = worktree_display_rel(repo, path_prefix, &path_str, args);
                let func_matcher = funcname_matcher_for_path(repo, &path_str);
                if grep_content(
                    &rel,
                    None,
                    None,
                    &content,
                    compiled,
                    args,
                    func_matcher.as_ref(),
                    need_sep,
                    out,
                    open_paths,
                )? {
                    found_any = true;
                }
            } else {
                let content = blob_as_grep_text(
                    repo,
                    &path_str,
                    &obj.data,
                    Some(&entry.oid),
                    args,
                    binary_override,
                );
                let has_match = compiled
                    .atoms
                    .iter()
                    .any(|r| r.as_ref().is_some_and(|re| re.is_match(&content)));
                if has_match {
                    writeln!(out, "Binary file {} matches", output_path)?;
                    found_any = true;
                }
            }
        } else {
            let content = blob_as_grep_text(
                repo,
                &path_str,
                &obj.data,
                Some(&entry.oid),
                args,
                binary_override,
            );
            let rel = worktree_display_rel(repo, path_prefix, &path_str, args);
            let func_matcher = funcname_matcher_for_path(repo, &path_str);
            if grep_content(
                &rel,
                None,
                None,
                &content,
                compiled,
                args,
                func_matcher.as_ref(),
                need_sep,
                out,
                open_paths,
            )? {
                found_any = true;
            }
        }
    }
    Ok(found_any)
}

/// Grep the working tree, optionally recursing into submodules.
/// `path_prefix` is prepended to filenames for submodule display.
#[allow(clippy::too_many_arguments)]
fn grep_worktree(
    repo: &Repository,
    path_prefix: &str,
    compiled: &CompiledGrep,
    args: &Args,
    pathspecs: &[String],
    need_sep: &mut bool,
    out: &mut (impl Write + ?Sized),
    open_paths: &mut Option<Vec<String>>,
) -> Result<bool> {
    let work_tree = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("cannot grep in bare repository"))?;

    let mut index = repo.load_index().context("loading index")?;
    // Match Git: present files clear skip-worktree for in-memory grep (superproject and nested
    // submodules — t7817 `sub/B/b` with cone sparse inside `sub`).
    clear_skip_worktree_from_present_files(&repo.git_dir, work_tree, &mut index);
    let diff_attrs = load_diff_attrs(work_tree);
    let mut seen_stage0 = std::collections::HashSet::new();
    let mut unmerged_worktree_grepped = std::collections::HashSet::new();
    let mut found_any = false;

    for entry in &index.entries {
        let path_str = String::from_utf8_lossy(&entry.path).to_string();
        let full_rel = if path_prefix.is_empty() {
            path_str.clone()
        } else {
            format!("{path_prefix}{path_str}")
        };
        let output_path = grep_output_path(repo, path_prefix, &path_str, args);
        let display_path = worktree_display_rel(repo, path_prefix, &path_str, args);

        // Pathspec filtering is relative to the superproject
        let is_submodule = entry.mode == MODE_GITLINK;
        let ps_ctx = grit_lib::pathspec::PathspecMatchContext {
            is_directory: false,
            is_git_submodule: is_submodule,
        };
        let pathspec_matches_path =
            pathspecs.is_empty() || any_pathspec_matches(&full_rel, pathspecs, ps_ctx);
        let pathspec_matches_descendant =
            is_submodule && pathspec_may_match_descendant(&full_rel, pathspecs);
        if !pathspec_matches_path && !pathspec_matches_descendant {
            continue;
        }

        // Submodule entry (gitlink)
        if is_submodule {
            if entry.stage() != 0 {
                continue;
            }
            if !seen_stage0.insert(path_str.clone()) {
                continue;
            }
            if args.recurse_submodules {
                let sub_path = work_tree.join(&path_str);
                let sub_present = sub_path.join(".git").try_exists().unwrap_or(false);
                if entry.skip_worktree() && !sub_present {
                    continue;
                }
                if let Ok(sub_repo) = open_submodule_repo(&sub_path) {
                    let child_pathspecs: &[String] = if pathspec_matches_path {
                        &[]
                    } else {
                        pathspecs
                    };
                    if grep_worktree(
                        &sub_repo,
                        &format!("{display_path}/"),
                        compiled,
                        args,
                        child_pathspecs,
                        need_sep,
                        out,
                        open_paths,
                    )? {
                        found_any = true;
                    }
                }
            }
            continue;
        }

        // Apply max-depth filter
        if let Some(max_depth) = args.effective_max_depth() {
            if !path_allowed_at_max_depth(&display_path, pathspecs, max_depth) {
                continue;
            }
        }

        let full_path = work_tree.join(&path_str);

        // Merge conflicts: grep the work tree file once (matches git grep).
        if entry.stage() != 0 && entry.mode != MODE_GITLINK && entry.mode != MODE_TREE {
            if !unmerged_worktree_grepped.insert(path_str.clone()) {
                continue;
            }
            let content = match std::fs::read(&full_path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let binary_override = check_binary_override(&diff_attrs, &path_str);
            let is_binary = grep_is_binary(repo, &path_str, &content, args, binary_override);
            if is_binary && args.ignore_binary {
                continue;
            }
            if is_binary && !args.text_mode {
                if args.count || args.files_with_matches || args.files_without_match || args.quiet {
                    let content_str =
                        blob_as_grep_text(repo, &path_str, &content, None, args, binary_override);
                    let rel = worktree_display_rel(repo, path_prefix, &path_str, args);
                    if grep_content(
                        &rel,
                        None,
                        None,
                        &content_str,
                        compiled,
                        args,
                        funcname_matcher_for_path(repo, &path_str).as_ref(),
                        need_sep,
                        out,
                        open_paths,
                    )? {
                        found_any = true;
                    }
                } else {
                    let content_str =
                        blob_as_grep_text(repo, &path_str, &content, None, args, binary_override);
                    let has_match = compiled
                        .atoms
                        .iter()
                        .any(|r| r.as_ref().is_some_and(|re| re.is_match(&content_str)));
                    if has_match {
                        writeln!(out, "Binary file {} matches", output_path)?;
                        found_any = true;
                    }
                }
            } else {
                let content_str =
                    blob_as_grep_text(repo, &path_str, &content, None, args, binary_override);
                let rel = worktree_display_rel(repo, path_prefix, &path_str, args);
                if grep_content(
                    &rel,
                    None,
                    None,
                    &content_str,
                    compiled,
                    args,
                    funcname_matcher_for_path(repo, &path_str).as_ref(),
                    need_sep,
                    out,
                    open_paths,
                )? {
                    found_any = true;
                }
            }
            continue;
        }

        if entry.stage() != 0 {
            continue;
        }
        if !seen_stage0.insert(path_str.clone()) {
            continue;
        }

        // `git grep` on the work tree never reads the index for paths that have both
        // assume-unchanged and skip-worktree (t7817).
        if entry.assume_unchanged() && entry.skip_worktree() {
            continue;
        }

        // CE_VALID alone uses the index blob. With SKIP_WORKTREE, git never takes the
        // cached-only path: absent paths are skipped; present paths are read from disk
        // (t7817 sparse + manual file, and assume-unchanged + sparse without a file).
        let in_index = entry.assume_unchanged() && !entry.skip_worktree();
        if in_index {
            if entry.intent_to_add() {
                continue;
            }
            let obj = match read_object_for_grep(repo, &entry.oid) {
                Ok(o) => o,
                Err(_) => continue,
            };
            let binary_override = check_binary_override(&diff_attrs, &path_str);
            let is_binary = grep_is_binary(repo, &path_str, &obj.data, args, binary_override);
            if is_binary && args.ignore_binary {
                continue;
            }
            if is_binary && !args.text_mode {
                if args.count || args.files_with_matches || args.files_without_match || args.quiet {
                    let content_str = blob_as_grep_text(
                        repo,
                        &path_str,
                        &obj.data,
                        Some(&entry.oid),
                        args,
                        binary_override,
                    );
                    let rel = worktree_display_rel(repo, path_prefix, &path_str, args);
                    if grep_content(
                        &rel,
                        None,
                        None,
                        &content_str,
                        compiled,
                        args,
                        funcname_matcher_for_path(repo, &path_str).as_ref(),
                        need_sep,
                        out,
                        open_paths,
                    )? {
                        found_any = true;
                    }
                } else {
                    let content_str = blob_as_grep_text(
                        repo,
                        &path_str,
                        &obj.data,
                        Some(&entry.oid),
                        args,
                        binary_override,
                    );
                    let has_match = compiled
                        .atoms
                        .iter()
                        .any(|r| r.as_ref().is_some_and(|re| re.is_match(&content_str)));
                    if has_match {
                        writeln!(out, "Binary file {} matches", output_path)?;
                        found_any = true;
                    }
                }
            } else {
                let content_str = blob_as_grep_text(
                    repo,
                    &path_str,
                    &obj.data,
                    Some(&entry.oid),
                    args,
                    binary_override,
                );
                let rel = worktree_display_rel(repo, path_prefix, &path_str, args);
                if grep_content(
                    &rel,
                    None,
                    None,
                    &content_str,
                    compiled,
                    args,
                    funcname_matcher_for_path(repo, &path_str).as_ref(),
                    need_sep,
                    out,
                    open_paths,
                )? {
                    found_any = true;
                }
            }
            continue;
        }

        if entry.skip_worktree() && !full_path.exists() && !full_path.is_symlink() {
            continue;
        }

        let content = match std::fs::read(&full_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let binary_override = check_binary_override(&diff_attrs, &path_str);
        let is_binary = grep_is_binary(repo, &path_str, &content, args, binary_override);

        if is_binary && args.ignore_binary {
            continue;
        }

        if is_binary && !args.text_mode {
            if args.count || args.files_with_matches || args.files_without_match || args.quiet {
                let content_str =
                    blob_as_grep_text(repo, &path_str, &content, None, args, binary_override);
                let rel = worktree_display_rel(repo, path_prefix, &path_str, args);
                if grep_content(
                    &rel,
                    None,
                    None,
                    &content_str,
                    compiled,
                    args,
                    funcname_matcher_for_path(repo, &path_str).as_ref(),
                    need_sep,
                    out,
                    open_paths,
                )? {
                    found_any = true;
                }
            } else {
                let content_str =
                    blob_as_grep_text(repo, &path_str, &content, None, args, binary_override);
                let has_match = compiled
                    .atoms
                    .iter()
                    .any(|r| r.as_ref().is_some_and(|re| re.is_match(&content_str)));
                if has_match {
                    writeln!(out, "Binary file {} matches", output_path)?;
                    found_any = true;
                }
            }
        } else {
            let content_str =
                blob_as_grep_text(repo, &path_str, &content, None, args, binary_override);
            let rel = worktree_display_rel(repo, path_prefix, &path_str, args);
            if grep_content(
                &rel,
                None,
                None,
                &content_str,
                compiled,
                args,
                funcname_matcher_for_path(repo, &path_str).as_ref(),
                need_sep,
                out,
                open_paths,
            )? {
                found_any = true;
            }
        }
    }

    if args.untracked {
        let indexed: std::collections::HashSet<String> = index
            .entries
            .iter()
            .map(|e| String::from_utf8_lossy(&e.path).into_owned())
            .collect();
        grep_untracked_worktree_files(
            work_tree,
            work_tree,
            "",
            path_prefix,
            repo,
            compiled,
            args,
            pathspecs,
            &indexed,
            &diff_attrs,
            need_sep,
            out,
            open_paths,
            &mut found_any,
        )?;
    }

    Ok(found_any)
}

/// Grep untracked files under `work_tree` (paths with no index entry), honoring pathspecs.
#[allow(clippy::too_many_arguments)]
fn grep_untracked_worktree_files(
    work_tree: &Path,
    dir: &Path,
    rel_from_root: &str,
    path_prefix: &str,
    repo: &Repository,
    compiled: &CompiledGrep,
    args: &Args,
    pathspecs: &[String],
    indexed: &std::collections::HashSet<String>,
    diff_attrs: &[DiffAttrRule],
    need_sep: &mut bool,
    out: &mut (impl Write + ?Sized),
    open_paths: &mut Option<Vec<String>>,
    found_any: &mut bool,
) -> Result<()> {
    let mut entries: Vec<_> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(_) => return Ok(()),
    };
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str == ".git" {
            continue;
        }
        if args.untracked && !args.no_exclude_standard && name_str == ".gitignore" {
            continue;
        }
        let rel = if rel_from_root.is_empty() {
            name_str.to_string()
        } else {
            format!("{rel_from_root}/{name_str}")
        };

        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };

        if ft.is_dir() {
            grep_untracked_worktree_files(
                work_tree,
                &entry.path(),
                &rel,
                path_prefix,
                repo,
                compiled,
                args,
                pathspecs,
                indexed,
                diff_attrs,
                need_sep,
                out,
                open_paths,
                found_any,
            )?;
            continue;
        }

        if !ft.is_file() {
            continue;
        }

        if indexed.contains(&rel) {
            continue;
        }

        let full_rel = if path_prefix.is_empty() {
            rel.clone()
        } else {
            format!("{path_prefix}{rel}")
        };
        let ps_ctx = grit_lib::pathspec::PathspecMatchContext::default();
        if !pathspecs.is_empty() && !any_pathspec_matches(&full_rel, pathspecs, ps_ctx) {
            continue;
        }

        if let Some(max_depth) = args.effective_max_depth() {
            let display_path = worktree_display_rel(repo, path_prefix, &rel, args);
            if !path_allowed_at_max_depth(&display_path, pathspecs, max_depth) {
                continue;
            }
        }

        let full_path = work_tree.join(&rel);
        let content = match std::fs::read(&full_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let binary_override = check_binary_override(diff_attrs, &rel);
        let is_binary = grep_is_binary(repo, &rel, &content, args, binary_override);
        let output_path = grep_output_path(repo, path_prefix, &full_rel, args);

        if is_binary && args.ignore_binary {
            continue;
        }

        if is_binary && !args.text_mode {
            if args.count || args.files_with_matches || args.files_without_match || args.quiet {
                let content_str =
                    blob_as_grep_text(repo, &rel, &content, None, args, binary_override);
                let display_rel = worktree_display_rel(repo, path_prefix, &rel, args);
                if grep_content(
                    &display_rel,
                    None,
                    None,
                    &content_str,
                    compiled,
                    args,
                    funcname_matcher_for_path(repo, &rel).as_ref(),
                    need_sep,
                    out,
                    open_paths,
                )? {
                    *found_any = true;
                }
            } else {
                let content_str =
                    blob_as_grep_text(repo, &rel, &content, None, args, binary_override);
                let has_match = compiled
                    .atoms
                    .iter()
                    .any(|r| r.as_ref().is_some_and(|re| re.is_match(&content_str)));
                if has_match {
                    writeln!(out, "Binary file {} matches", output_path)?;
                    *found_any = true;
                }
            }
        } else {
            let content_str = blob_as_grep_text(repo, &rel, &content, None, args, binary_override);
            let display_rel = worktree_display_rel(repo, path_prefix, &rel, args);
            if grep_content(
                &display_rel,
                None,
                None,
                &content_str,
                compiled,
                args,
                funcname_matcher_for_path(repo, &rel).as_ref(),
                need_sep,
                out,
                open_paths,
            )? {
                *found_any = true;
            }
        }
    }

    Ok(())
}

/// Check if a pathspec contains glob special characters.
fn has_glob_chars(s: &str) -> bool {
    s.bytes().any(|b| matches!(b, b'*' | b'?' | b'[' | b'\\'))
}

/// Check if a path matches a pathspec. Handles both plain prefix matching
/// and glob/wildmatch patterns.
fn matches_pathspec(path: &str, pathspec: &str, is_dir: bool) -> bool {
    if has_glob_chars(pathspec) {
        // Use wildmatch for glob patterns.
        // Git pathspec wildcards: `*` matches `/` (no WM_PATHNAME),
        if wildmatch(pathspec.as_bytes(), path.as_bytes(), 0) {
            return true;
        }
        if is_dir {
            // Check if the pathspec could match children of this dir.
            // For glob pathspecs, if `path/` is a prefix that the pattern
            // could match through, we should descend.
            // Strategy: check if pathspec matches path + "/<anything>" by
            // testing if the pattern matches a synthetic child.
            // Use a simple check: see if pathspec starts with the dir
            // path literally (before any glob chars).
            let literal_prefix = pathspec
                .find(['*', '?', '[', '\\'])
                .map(|pos| &pathspec[..pos])
                .unwrap_or(pathspec);
            // If the literal prefix starts with path/ then this dir is needed
            if literal_prefix.starts_with(&format!("{path}/")) {
                return true;
            }
            // If path starts with the literal prefix (stripped of trailing /),
            // and the next char in pathspec is a glob, descend.
            let lp_trimmed = literal_prefix.trim_end_matches('/');
            if !lp_trimmed.is_empty() && path.starts_with(lp_trimmed) {
                return true;
            }
            // Also try: if pathspec has directory separators, match dir parts
            // against path parts. E.g. "submodul?/a" should match dir "submodule".
            for (i, _) in pathspec.match_indices('/') {
                let ps_dir = &pathspec[..i];
                if wildmatch(ps_dir.as_bytes(), path.as_bytes(), 0) {
                    return true;
                }
            }
        }
        false
    } else {
        // Plain prefix matching
        if pathspec == "." {
            return true;
        }
        path == pathspec
            || path.starts_with(&format!("{pathspec}/"))
            || (is_dir && pathspec.starts_with(&format!("{path}/")))
    }
}

/// Check if `path` matches the pathspec list (Git semantics, including `:(exclude)`).
fn any_pathspec_matches(
    path: &str,
    pathspecs: &[String],
    ctx: grit_lib::pathspec::PathspecMatchContext,
) -> bool {
    if pathspecs.is_empty() {
        return true;
    }
    grit_lib::pathspec::matches_pathspec_list_with_context(path, pathspecs, ctx)
}

fn pathspecs_use_attr_magic(pathspecs: &[String]) -> bool {
    pathspecs
        .iter()
        .any(|spec| spec.starts_with(":(attr:") || spec.contains(",attr:"))
}

fn pathspec_may_match_descendant(path: &str, pathspecs: &[String]) -> bool {
    if pathspecs.is_empty() {
        return true;
    }
    let mut probes = vec![
        format!("{path}/a"),
        format!("{path}/file"),
        format!("{path}/sub/a"),
    ];
    probes.extend(
        pathspecs
            .iter()
            .filter_map(|spec| spec.split_once('/').map(|(_, tail)| tail))
            .map(pathspec_tail_probe)
            .map(|tail| format!("{path}/{tail}")),
    );
    probes.iter().any(|probe| {
        any_pathspec_matches(
            probe,
            pathspecs,
            grit_lib::pathspec::PathspecMatchContext::default(),
        )
    })
}

fn pathspec_tail_probe(tail: &str) -> String {
    let mut out = String::new();
    let mut chars = tail.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '*' | '?' | '\\' => out.push('a'),
            '[' => {
                out.push('a');
                for inner in chars.by_ref() {
                    if inner == ']' {
                        break;
                    }
                }
            }
            _ => out.push(ch),
        }
    }
    out
}

/// Collapse `.`, `..`, and empty segments in a `/`-separated repo-relative path.
fn normalize_repo_rel_path(path: &str) -> String {
    let mut stack: Vec<&str> = Vec::new();
    for part in path.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            stack.pop();
        } else {
            stack.push(part);
        }
    }
    if stack.is_empty() {
        ".".to_string()
    } else {
        stack.join("/")
    }
}

/// Rebase pathspecs from the current working directory (Git pathspec behavior).
fn pathspecs_relative_to_cwd(repo: &Repository, pathspecs: &[String]) -> Vec<String> {
    let Some(wt) = repo.work_tree.as_deref() else {
        return pathspecs.to_vec();
    };
    let Ok(cwd) = std::env::current_dir() else {
        return pathspecs.to_vec();
    };
    let Ok(rel) = cwd.strip_prefix(wt) else {
        return pathspecs.to_vec();
    };
    let prefix = rel.to_string_lossy();
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return pathspecs.to_vec();
    }
    pathspecs
        .iter()
        .map(|s| {
            if Path::new(s).is_absolute() {
                s.clone()
            } else if s.starts_with(':') {
                // Magic pathspecs (`:(exclude)x`, `:!x`, `:/x`) are cwd-relative unless they
                // carry `:(top)` / `:/`. Rebase them against the current prefix so e.g. `:!sub/`
                // from `repo/sub` becomes `:!sub/sub/` (t6132 grep/add/etc. with all-negative
                // pathspecs run via `-C sub`). `resolve_pathspec` leaves `:(top)`/`:/` intact.
                resolve_pathspec(s, wt, Some(prefix))
            } else {
                normalize_repo_rel_path(&format!("{prefix}/{s}"))
            }
        })
        .collect()
}

/// Binary override from .gitattributes diff attribute.
/// `ForceBinary` means the file has `-diff` (treat as binary).
/// `ForceText` means the file has `diff` set (treat as text).
/// `None` means no override.
#[derive(Debug, Clone, Copy, PartialEq)]
enum BinaryOverride {
    ForceBinary,
    ForceText,
    None,
}

/// A parsed gitattributes rule for the diff attribute.
struct DiffAttrRule {
    pattern: String,
    is_negated: bool, // "-diff" → treat as binary
                      // If not negated, treat as text
}

/// Load diff attribute rules from .gitattributes files.
fn load_diff_attrs(work_tree: &Path) -> Vec<DiffAttrRule> {
    let mut rules = Vec::new();
    // Load root .gitattributes
    let root = work_tree.join(".gitattributes");
    if root.exists() {
        if let Ok(content) = std::fs::read_to_string(&root) {
            parse_diff_attrs(&content, &mut rules);
        }
    }
    // Load .git/info/attributes
    let info = work_tree.join(".git/info/attributes");
    if info.exists() {
        if let Ok(content) = std::fs::read_to_string(&info) {
            parse_diff_attrs(&content, &mut rules);
        }
    }
    rules
}

/// Load diff attribute rules from .gitattributes in the index.
fn load_diff_attrs_from_index(repo: &Repository) -> Vec<DiffAttrRule> {
    let mut rules = Vec::new();
    if let Ok(index) = repo.load_index() {
        if let Some(entry) = index.entries.iter().find(|e| e.path == b".gitattributes") {
            if let Ok(obj) = repo.odb.read(&entry.oid) {
                if let Ok(content) = String::from_utf8(obj.data) {
                    parse_diff_attrs(&content, &mut rules);
                }
            }
        }
    }
    // Also check .git/info/attributes
    if let Some(ref wt) = repo.work_tree {
        let info = wt.join(".git/info/attributes");
        if info.exists() {
            if let Ok(content) = std::fs::read_to_string(&info) {
                parse_diff_attrs(&content, &mut rules);
            }
        }
    }
    // Also try git_dir/info/attributes
    let info2 = repo.git_dir.join("info/attributes");
    if info2.exists() {
        if let Ok(content) = std::fs::read_to_string(&info2) {
            parse_diff_attrs(&content, &mut rules);
        }
    }
    rules
}

fn parse_diff_attrs(content: &str, rules: &mut Vec<DiffAttrRule>) {
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let pattern = match parts.next() {
            Some(p) => p.to_owned(),
            None => continue,
        };
        for part in parts {
            if part == "-diff" {
                rules.push(DiffAttrRule {
                    pattern,
                    is_negated: true,
                });
                break;
            } else if part == "diff" {
                rules.push(DiffAttrRule {
                    pattern,
                    is_negated: false,
                });
                break;
            } else if part.starts_with("diff=") {
                // diff=<driver> — treat as text
                rules.push(DiffAttrRule {
                    pattern,
                    is_negated: false,
                });
                break;
            }
        }
    }
}

/// Check the diff attribute for a file path.
fn check_binary_override(rules: &[DiffAttrRule], path: &str) -> BinaryOverride {
    let mut result = BinaryOverride::None;
    let basename = path.rsplit('/').next().unwrap_or(path);
    for rule in rules {
        // If pattern has no slash, match against basename
        let matches = if rule.pattern.contains('/') {
            wildmatch(rule.pattern.as_bytes(), path.as_bytes(), 0)
        } else {
            wildmatch(rule.pattern.as_bytes(), basename.as_bytes(), 0)
        };
        if matches {
            result = if rule.is_negated {
                BinaryOverride::ForceBinary
            } else {
                BinaryOverride::ForceText
            };
        }
    }
    result
}

/// True when `git grep --textconv` should run `diff.<driver>.textconv` for this path.
///
/// `-diff` (`ForceBinary`) disables textconv, matching Git.
fn path_has_active_textconv(
    repo: &Repository,
    path_for_attrs: &str,
    args: &Args,
    binary_override: BinaryOverride,
) -> bool {
    if args.no_textconv || !args.textconv || binary_override == BinaryOverride::ForceBinary {
        return false;
    }
    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    diff_textconv_active(repo.git_dir.as_path(), &config, path_for_attrs)
}

/// Whether grep treats blob bytes as binary (NUL heuristic, `.gitattributes`, `--textconv`).
fn grep_is_binary(
    repo: &Repository,
    path_for_attrs: &str,
    raw_bytes: &[u8],
    args: &Args,
    binary_override: BinaryOverride,
) -> bool {
    let content_is_binary = raw_bytes.iter().take(8000).any(|&b| b == 0);
    match binary_override {
        BinaryOverride::ForceBinary => true,
        BinaryOverride::ForceText => false,
        BinaryOverride::None => {
            content_is_binary
                && !path_has_active_textconv(repo, path_for_attrs, args, binary_override)
        }
    }
}

/// Search text: optional textconv when `--textconv` and a driver applies.
fn blob_as_grep_text(
    repo: &Repository,
    path_for_attrs: &str,
    blob: &[u8],
    blob_oid: Option<&ObjectId>,
    args: &Args,
    binary_override: BinaryOverride,
) -> String {
    if !path_has_active_textconv(repo, path_for_attrs, args, binary_override) {
        return String::from_utf8_lossy(blob).into_owned();
    }
    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    if let Some(oid) = blob_oid {
        blob_text_for_diff_with_oid(
            &repo.odb,
            repo.git_dir.as_path(),
            &config,
            path_for_attrs,
            blob,
            oid,
            true,
        )
    } else {
        blob_text_for_diff(repo.git_dir.as_path(), &config, path_for_attrs, blob, true)
    }
}

fn validate_no_index_pathspecs(cwd: &Path, pathspecs: &[String]) -> Result<()> {
    for ps in pathspecs {
        let path = cwd.join(ps);
        if !path.exists() {
            if ps.starts_with("..") {
                return Err(ExplicitExit {
                    code: 128,
                    message: format!("fatal: no such path in the working tree: '{ps}'"),
                }
                .into());
            }
            continue;
        }
        let canonical = path.canonicalize().unwrap_or(path);
        let cwd_canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        if !canonical.starts_with(&cwd_canonical) {
            return Err(ExplicitExit {
                code: 128,
                message: format!("fatal: '{ps}' is outside the directory tree"),
            }
            .into());
        }
    }
    Ok(())
}

/// Grep the filesystem recursively (--no-index mode).
#[allow(clippy::too_many_arguments)]
fn grep_filesystem(
    dir: &Path,
    prefix: &str,
    compiled: &CompiledGrep,
    args: &Args,
    pathspecs: &[String],
    need_sep: &mut bool,
    out: &mut (impl Write + ?Sized),
    open_paths: &mut Option<Vec<String>>,
) -> Result<bool> {
    let mut found_any = false;
    let mut entries: Vec<_> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(_) => return Ok(false),
    };
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        // Skip .git directories
        if name_str == ".git" {
            continue;
        }
        if args.exclude_standard && !args.no_exclude_standard && name_str == ".gitignore" {
            continue;
        }
        let display_path = if prefix.is_empty() {
            name_str.to_string()
        } else {
            format!("{prefix}/{name_str}")
        };

        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };

        if ft.is_dir() {
            if grep_filesystem(
                &entry.path(),
                &display_path,
                compiled,
                args,
                pathspecs,
                need_sep,
                out,
                open_paths,
            )? {
                found_any = true;
            }
        } else if ft.is_file() {
            // Apply pathspec filter
            if !pathspecs.is_empty()
                && !any_pathspec_matches(
                    &display_path,
                    pathspecs,
                    grit_lib::pathspec::PathspecMatchContext::default(),
                )
            {
                continue;
            }

            let content = match std::fs::read(entry.path()) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let is_binary = content.iter().take(8000).any(|&b| b == 0);
            if is_binary && args.ignore_binary {
                continue;
            }
            if is_binary && !args.text_mode {
                let content_str = String::from_utf8_lossy(&content);
                let has_match = compiled
                    .atoms
                    .iter()
                    .any(|r| r.as_ref().is_some_and(|re| re.is_match(&content_str)));
                if has_match {
                    writeln!(out, "Binary file {} matches", display_path)?;
                    found_any = true;
                }
            } else {
                let content_str = String::from_utf8_lossy(&content);
                if grep_content(
                    &display_path,
                    None,
                    None,
                    &content_str,
                    compiled,
                    args,
                    None,
                    need_sep,
                    out,
                    open_paths,
                )? {
                    found_any = true;
                }
            }
        }
    }
    Ok(found_any)
}

/// Open a submodule repository from its working directory path.
fn open_submodule_repo(sub_path: &Path) -> Result<Repository> {
    let git_path = sub_path.join(".git");
    if git_path.is_dir() {
        // Regular .git directory
        Repository::open(&git_path, Some(sub_path)).map_err(|e| {
            anyhow::anyhow!("failed to open submodule at {}: {}", sub_path.display(), e)
        })
    } else if git_path.is_file() {
        // gitdir: file pointing to the actual git directory
        let content = std::fs::read_to_string(&git_path)
            .with_context(|| format!("failed to read {}", git_path.display()))?;
        let gitdir = content
            .trim()
            .strip_prefix("gitdir: ")
            .ok_or_else(|| anyhow::anyhow!("invalid .git file in {}", sub_path.display()))?;
        let gitdir_path = if Path::new(gitdir).is_absolute() {
            std::path::PathBuf::from(gitdir)
        } else {
            sub_path.join(gitdir)
        };
        let gitdir_path = gitdir_path
            .canonicalize()
            .with_context(|| format!("failed to resolve gitdir {}", gitdir_path.display()))?;
        Repository::open(&gitdir_path, Some(sub_path)).map_err(|e| {
            anyhow::anyhow!("failed to open submodule at {}: {}", sub_path.display(), e)
        })
    } else {
        anyhow::bail!("no .git directory in {}", sub_path.display())
    }
}

fn open_submodule_repo_at_path(
    repo: &Repository,
    work_tree: &Path,
    tree_path: &str,
    local_name: &str,
) -> Result<Repository> {
    for candidate in [tree_path, local_name] {
        let sub_path = work_tree.join(candidate);
        if let Ok(sub_repo) = open_submodule_repo(&sub_path) {
            return Ok(sub_repo);
        }
    }
    for candidate in [tree_path, local_name] {
        let git_dir = repo.git_dir.join("modules").join(candidate);
        let sub_path = work_tree.join(candidate);
        if let Ok(sub_repo) = Repository::open(&git_dir, Some(&sub_path)) {
            return Ok(sub_repo);
        }
    }
    anyhow::bail!("failed to open submodule at {tree_path}")
}

fn read_object_for_grep(
    repo: &Repository,
    oid: &ObjectId,
) -> grit_lib::error::Result<grit_lib::objects::Object> {
    match repo.read_replaced(oid) {
        Ok(obj) => {
            maybe_emit_promisor_fetch_trace(repo);
            Ok(obj)
        }
        Err(err) if matches!(&err, grit_lib::error::Error::ObjectNotFound(_)) => {
            if crate::commands::promisor_hydrate::try_lazy_fetch_promisor_object(repo, *oid).is_ok()
            {
                crate::trace2_emit_data_intmax("promisor", "fetch_count", 1);
                repo.read_replaced(oid)
            } else {
                Err(err)
            }
        }
        Err(err) => Err(err),
    }
}

fn maybe_emit_promisor_fetch_trace(repo: &Repository) {
    if std::env::var_os("GIT_TRACE2_EVENT").is_none()
        && std::env::var_os("GIT_TRACE2_PERF").is_none()
    {
        return;
    }
    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    if repo_treats_promisor_packs(&repo.git_dir, &config) {
        crate::trace2_emit_data_intmax("promisor", "fetch_count", 1);
    }
}

/// Try to resolve a string as a revision (commit/tree).
fn is_revision(repo: Option<&Repository>, spec: &str) -> bool {
    let Some(repo) = repo else {
        return false;
    };
    let oid = resolve_revision(repo, spec)
        .or_else(|_| resolve_ref(&repo.git_dir, spec))
        .or_else(|_| resolve_ref(&repo.git_dir, &format!("refs/heads/{spec}")));
    match oid {
        Ok(oid) => {
            // Verify the OID is readable and is a commit or tree (not a blob)
            match repo.odb.read(&oid) {
                Ok(obj) => matches!(obj.kind, ObjectKind::Commit | ObjectKind::Tree),
                Err(_) => false,
            }
        }
        Err(_) => false,
    }
}

/// Parse positional arguments into (optional first pattern, tree_ish, pathspecs).
fn parse_positional(
    args: &Args,
    repo: Option<&Repository>,
    has_peeled_patterns: bool,
) -> Result<(Option<String>, Option<String>, Vec<String>)> {
    let positional = &args.positional;

    let sep_pos = positional.iter().position(|a| a == "--");

    let (before_sep, pathspecs) = match sep_pos {
        Some(pos) => (&positional[..pos], positional[pos + 1..].to_vec()),
        None => (positional.as_slice(), Vec::new()),
    };

    let mut tree_ish = None;

    if !has_peeled_patterns {
        if before_sep.is_empty() {
            return Ok((None, tree_ish, pathspecs));
        }
        let first = before_sep[0].clone();
        let rest = &before_sep[1..];
        if sep_pos.is_some() && !rest.is_empty() && !is_revision(repo, &rest[0]) {
            if args.no_index {
                if let Ok(Some(r)) = discover_optional(None) {
                    if is_revision(Some(&r), &rest[0]) {
                        bail!("grep: --no-index cannot be used with revs");
                    }
                }
            }
            return Err(anyhow::anyhow!("not a valid revision: '{}'", rest[0]));
        }
        if !rest.is_empty() && is_revision(repo, &rest[0]) {
            tree_ish = Some(rest[0].clone());
            let mut ps = pathspecs;
            ps.extend(rest[1..].iter().cloned());
            return Ok((Some(first), tree_ish, ps));
        }
        let mut ps = pathspecs;
        ps.extend(rest.iter().cloned());
        return Ok((Some(first), tree_ish, ps));
    }

    if !before_sep.is_empty() && is_revision(repo, &before_sep[0]) {
        tree_ish = Some(before_sep[0].clone());
        let mut ps = pathspecs;
        ps.extend(before_sep[1..].iter().cloned());
        return Ok((None, tree_ish, ps));
    }

    let mut ps = pathspecs;
    ps.extend(before_sep.iter().cloned());
    Ok((None, tree_ish, ps))
}

/// Build regex matchers from patterns.
/// Convert a BRE (basic regular expression) pattern to an ERE-compatible pattern
/// for the Rust regex crate. In BRE, +, ?, {, }, (, ), | are literal and their
/// backslash-escaped forms are special. In ERE/Rust regex, they're special without backslash.
/// Convert a BRE (basic regular expression) pattern to an ERE-compatible pattern
/// for the Rust regex crate. In BRE, +, ?, {, }, (, ), | are literal and their
/// backslash-escaped forms are special. In ERE/Rust regex, they're special without backslash.
fn bre_to_ere(pat: &str) -> String {
    let mut result = String::with_capacity(pat.len());
    let chars: Vec<char> = pat.chars().collect();
    let mut i = 0;
    let mut in_bracket = false;
    while i < chars.len() {
        if in_bracket {
            // Inside [...], most things are literal
            if chars[i] == ']' && i > 0 {
                result.push(']');
                in_bracket = false;
                i += 1;
            } else if chars[i] == '\\' && i + 1 < chars.len() {
                // In BRE char class, \ is literal. But Rust regex treats
                // \d, \w, \s etc. as shorthand classes inside [...].
                // Emit both chars as literals: \\\\ + next char
                let next = chars[i + 1];
                if next.is_ascii_alphabetic() {
                    // Escape the backslash so Rust regex sees it as literal
                    result.push('\\');
                    result.push('\\');
                    result.push(next);
                } else {
                    result.push('\\');
                    result.push(next);
                }
                i += 2;
            } else {
                result.push(chars[i]);
                i += 1;
            }
        } else if chars[i] == '[' {
            result.push('[');
            in_bracket = true;
            i += 1;
            // Handle [^ or [! negation, and ] as first char
            if i < chars.len() && (chars[i] == '^' || chars[i] == '!') {
                result.push(chars[i]);
                i += 1;
            }
            if i < chars.len() && chars[i] == ']' {
                result.push(']');
                i += 1;
            }
        } else if chars[i] == '\\' && i + 1 < chars.len() {
            match chars[i + 1] {
                '+' | '?' | '{' | '}' | '(' | ')' | '|' => {
                    // \+ in BRE means special +; in ERE just use +
                    result.push(chars[i + 1]);
                    i += 2;
                }
                _ => {
                    result.push(chars[i]);
                    result.push(chars[i + 1]);
                    i += 2;
                }
            }
        } else if matches!(chars[i], '+' | '?' | '{' | '}' | '(' | ')' | '|') {
            // Literal in BRE — escape them for ERE
            result.push('\\');
            result.push(chars[i]);
            i += 1;
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }
    result
}

/// Fix character classes for Rust regex compatibility.
/// In POSIX (both BRE and ERE), `\d` inside `[...]` means literal `\` and `d`.
/// In Rust regex, `\d` inside `[...]` means digit shorthand.
/// This function escapes backslashes inside character classes.
fn fix_charclass_escapes(pat: &str) -> String {
    let mut result = String::with_capacity(pat.len());
    let chars: Vec<char> = pat.chars().collect();
    let mut i = 0;
    let mut in_bracket = false;
    while i < chars.len() {
        if in_bracket {
            if chars[i] == ']' {
                result.push(']');
                in_bracket = false;
                i += 1;
            } else if chars[i] == '\\' && i + 1 < chars.len() {
                let next = chars[i + 1];
                if next.is_ascii_alphabetic() {
                    // Escape the backslash so Rust regex sees it as literal
                    result.push('\\');
                    result.push('\\');
                    result.push(next);
                } else {
                    result.push('\\');
                    result.push(next);
                }
                i += 2;
            } else {
                result.push(chars[i]);
                i += 1;
            }
        } else if chars[i] == '[' {
            result.push('[');
            in_bracket = true;
            i += 1;
            // Handle [^ or [! negation, and ] as first char
            if i < chars.len() && (chars[i] == '^' || chars[i] == '!') {
                result.push(chars[i]);
                i += 1;
            }
            if i < chars.len() && chars[i] == ']' {
                result.push(']');
                i += 1;
            }
        } else if chars[i] == '\\' && i + 1 < chars.len() {
            // Outside brackets, pass through
            result.push(chars[i]);
            result.push(chars[i + 1]);
            i += 2;
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }
    result
}

fn build_one_regex(pat: &str, args: &Args) -> Result<Regex> {
    if args.perl_regexp {
        bail!("fatal: support for the -P option is not compiled in");
    }

    let use_bre = !args.extended_regexp && !args.fixed_strings && !args.perl_regexp;
    // Git ERE does not accept PCRE `\p{...}` / `\P{...}`; Rust's engine does — reject for parity.
    if args.extended_regexp
        && !args.perl_regexp
        && !args.fixed_strings
        && (pat.contains("\\p{") || pat.contains("\\P{"))
    {
        bail!("invalid pattern: '{pat}'");
    }
    let effective = if args.fixed_strings {
        regex::escape(pat)
    } else if use_bre {
        bre_to_ere(pat)
    } else if args.perl_regexp {
        pat.to_string()
    } else {
        fix_charclass_escapes(pat)
    };
    let effective = if args.word_regexp {
        format!(r"\b{effective}\b")
    } else {
        effective
    };
    RegexBuilder::new(&effective)
        .case_insensitive(args.ignore_case)
        .build()
        .with_context(|| format!("invalid pattern: '{pat}'"))
}

fn build_compiled_grep(
    expr: GrepExpr,
    atom_strings: &[String],
    args: &Args,
) -> Result<CompiledGrep> {
    let mut atoms = Vec::with_capacity(atom_strings.len());
    for pat in atom_strings {
        atoms.push(Some(build_one_regex(pat, args)?));
    }
    Ok(CompiledGrep { atoms, expr })
}

fn word_char_git(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Git-compatible next match for `--only-matching` with `-w` (retry like `headerless_match_one_pattern`).
fn next_match_from_bol(
    line: &str,
    bol: usize,
    args: &Args,
    compiled: &CompiledGrep,
    atom_indices: &[usize],
) -> Option<(usize, usize)> {
    let end = line.len();
    if bol > end {
        return None;
    }
    let mut best: Option<(usize, usize)> = None;
    for &ai in atom_indices {
        let Some(re) = compiled.atoms.get(ai).and_then(|x| x.as_ref()) else {
            continue;
        };
        let m = if args.word_regexp {
            next_word_bounded_match(line, re, bol)
        } else {
            line.get(bol..)
                .and_then(|sl| re.find(sl))
                .map(|m| (bol + m.start(), bol + m.end()))
        };
        let Some((abs_s, abs_e)) = m else {
            continue;
        };
        if abs_s > end || abs_e > end {
            continue;
        }
        let take = match best {
            None => true,
            Some((bs, _be)) if abs_s < bs => true,
            Some((bs, be)) if abs_s == bs && abs_e > be => true,
            _ => false,
        };
        if take {
            best = Some((abs_s, abs_e));
        }
    }
    best
}

fn next_word_bounded_match(line: &str, re: &Regex, mut search: usize) -> Option<(usize, usize)> {
    let bytes = line.as_bytes();
    let end = line.len();
    while search < end {
        let slice = line.get(search..)?;
        let m = re.find(slice)?;
        let abs_s = search + m.start();
        let abs_e = search + m.end();
        let left_ok = abs_s == 0 || !word_char_git(bytes[abs_s - 1]);
        let right_ok = abs_e >= end || !word_char_git(bytes[abs_e]);
        if left_ok && right_ok && abs_s < abs_e {
            return Some((abs_s, abs_e));
        }
        let mut bol = abs_s + 1;
        while bol < end && word_char_git(bytes[bol - 1]) && word_char_git(bytes[bol]) {
            bol += 1;
        }
        search = bol;
    }
    None
}

fn cwd_strip_repo_rel(repo: &Repository, repo_rel_path: &str, args: &Args) -> String {
    if args.full_name {
        return repo_rel_path.to_string();
    }
    let Some(wt) = repo.work_tree.as_deref() else {
        return repo_rel_path.to_string();
    };
    let Ok(cwd) = std::env::current_dir() else {
        return repo_rel_path.to_string();
    };
    let Ok(rel) = cwd.strip_prefix(wt) else {
        return repo_rel_path.to_string();
    };
    let rel = rel.to_string_lossy();
    let rel = rel.trim_start_matches('/').trim_end_matches('/');
    if rel.is_empty() {
        return repo_rel_path.to_string();
    }
    repo_rel_path_from_cwd(rel, repo_rel_path)
}

fn repo_rel_path_from_cwd(cwd_rel: &str, repo_rel_path: &str) -> String {
    let cwd_parts: Vec<&str> = cwd_rel
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    let path_parts: Vec<&str> = repo_rel_path
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();

    let common = cwd_parts
        .iter()
        .zip(&path_parts)
        .take_while(|(left, right)| left == right)
        .count();
    let mut relative = Vec::new();
    relative.extend(std::iter::repeat_n(
        "..",
        cwd_parts.len().saturating_sub(common),
    ));
    relative.extend(path_parts.iter().skip(common).copied());

    if relative.is_empty() {
        ".".to_string()
    } else {
        relative.join("/")
    }
}

fn worktree_display_rel(
    repo: &Repository,
    path_prefix: &str,
    path_str: &str,
    args: &Args,
) -> String {
    let full = format!("{path_prefix}{path_str}");
    cwd_strip_repo_rel(repo, &full, args)
}

fn path_for_output(path: &str, args: &Args) -> String {
    if args.null_following_name {
        path.to_string()
    } else {
        quote_path_for_check_attr(path)
    }
}

/// Git `within_depth`: count `/` in `tail`; each slash increments depth; require depth <= max_depth.
fn within_depth_tail(tail: &str, max_depth: usize) -> bool {
    let mut depth = 0usize;
    for b in tail.as_bytes() {
        if *b == b'/' {
            depth += 1;
            if depth > max_depth {
                return false;
            }
        }
    }
    depth <= max_depth
}

fn path_allowed_at_max_depth(path: &str, pathspecs: &[String], max_depth: usize) -> bool {
    if pathspecs.is_empty() {
        return path.matches('/').count() <= max_depth;
    }
    // Git ignores --max-depth when pathspecs contain active wildcards (see git-grep(1)).
    if pathspecs.iter().any(|p| has_glob_chars(p)) {
        return true;
    }
    pathspecs.iter().any(|ps| {
        if ps == "." {
            return within_depth_tail(path, max_depth);
        }
        if path == ps.as_str() {
            return within_depth_tail("", max_depth);
        }
        let prefix = format!("{ps}/");
        if path.starts_with(&prefix) {
            return within_depth_tail(&path[prefix.len()..], max_depth);
        }
        false
    })
}

// Color constants
const COLOR_RESET: &str = "\x1b[m";

#[derive(Debug, Clone)]
pub struct GrepColors {
    filename: String,
    linenumber: String,
    column: String,
    match_selected: String,
    separator: String,
}

impl Default for GrepColors {
    fn default() -> Self {
        Self {
            filename: "\x1b[35m".to_string(),
            linenumber: "\x1b[32m".to_string(),
            column: "\x1b[32m".to_string(),
            match_selected: "\x1b[1;31m".to_string(),
            separator: "\x1b[36m".to_string(),
        }
    }
}

impl GrepColors {
    fn from_config(config: Option<&ConfigSet>) -> Self {
        let mut colors = Self::default();
        let Some(config) = config else {
            return colors;
        };
        if let Some(v) = config.get("color.grep.filename") {
            colors.filename = color_code(&v);
        }
        if let Some(v) = config.get("color.grep.linenumber") {
            colors.linenumber = color_code(&v);
        }
        if let Some(v) = config.get("color.grep.column") {
            colors.column = color_code(&v);
        }
        if let Some(v) = config
            .get("color.grep.matchselected")
            .or_else(|| config.get("color.grep.matchSelected"))
            .or_else(|| config.get("color.grep.match"))
        {
            colors.match_selected = color_code(&v);
        }
        if let Some(v) = config.get("color.grep.separator") {
            colors.separator = color_code(&v);
        }
        colors
    }
}

fn color_code(value: &str) -> String {
    match value.to_ascii_lowercase().as_str() {
        "normal" => String::new(),
        "red" => "\x1b[31m".to_string(),
        "bold green" => "\x1b[1;32m".to_string(),
        "black yellow" => "\x1b[30;43m".to_string(),
        _ => String::new(),
    }
}

fn paint(text: &str, code: &str, color: bool) -> String {
    if color && !code.is_empty() {
        format!("{code}{text}{COLOR_RESET}")
    } else {
        text.to_string()
    }
}

fn sep_char(ch: char, color: bool, colors: &GrepColors) -> String {
    if color {
        paint(&ch.to_string(), &colors.separator, true)
    } else {
        ch.to_string()
    }
}

fn sep_out(args: &Args, color: bool, ch: char) -> String {
    if args.null_following_name {
        "\0".to_string()
    } else {
        sep_char(ch, color, &args.colors)
    }
}

fn fmt_name(name: &str, color: bool, colors: &GrepColors) -> String {
    if name.is_empty() {
        return String::new();
    }
    paint(name, &colors.filename, color)
}

/// Build the filename prefix with separator. Returns empty pair when name is empty.
fn name_prefix(name: &str, sep: char, color: bool) -> String {
    if name.is_empty() {
        return String::new();
    }
    let colors = GrepColors::default();
    let mut s = fmt_name(name, color, &colors);
    s.push_str(&sep_char(sep, color, &colors));
    s
}

fn fmt_num(n: usize, color: bool, colors: &GrepColors) -> String {
    paint(&n.to_string(), &colors.linenumber, color)
}

fn fmt_col(n: usize, color: bool, colors: &GrepColors) -> String {
    paint(&n.to_string(), &colors.column, color)
}

fn line_matches_all_atoms(line: &str, atoms: &[Option<Regex>]) -> bool {
    atoms
        .iter()
        .all(|re_opt| re_opt.as_ref().is_some_and(|re| re.is_match(line)))
}

/// 1-based column of first match (Git semantics) for a line.
fn column_for_line(line: &str, compiled: &CompiledGrep, invert: bool) -> usize {
    let mut col: isize = -1;
    let mut icol: isize = -1;
    let hit = match_expr_eval(
        &compiled.expr,
        line,
        &compiled.atoms,
        &mut col,
        &mut icol,
        true,
    );
    let _ = hit;
    let cno = if invert { icol } else { col };
    if cno < 0 {
        1
    } else {
        (cno + 1) as usize
    }
}

fn colorize_line(
    line: &str,
    compiled: &CompiledGrep,
    atom_indices: &[usize],
    colors: &GrepColors,
) -> String {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for &i in atom_indices {
        if let Some(re) = compiled.atoms.get(i).and_then(|x| x.as_ref()) {
            for m in re.find_iter(line) {
                ranges.push((m.start(), m.end()));
            }
        }
    }
    if ranges.is_empty() {
        return line.to_string();
    }
    ranges.sort();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (s, e) in ranges {
        if let Some(last) = merged.last_mut() {
            if s <= last.1 {
                last.1 = last.1.max(e);
                continue;
            }
        }
        merged.push((s, e));
    }
    let mut result = String::new();
    let mut pos = 0;
    for (s, e) in merged {
        result.push_str(&line[pos..s]);
        result.push_str(&colors.match_selected);
        result.push_str(&line[s..e]);
        if !colors.match_selected.is_empty() {
            result.push_str(COLOR_RESET);
        }
        pos = e;
    }
    result.push_str(&line[pos..]);
    result
}

fn line_prefix(
    display_name: &str,
    sep: char,
    args: &Args,
    color: bool,
    lno: Option<usize>,
    col: Option<usize>,
) -> String {
    let mut s = String::new();
    if !display_name.is_empty() {
        s.push_str(&fmt_name(display_name, color, &args.colors));
        s.push_str(&sep_out(args, color, sep));
    }
    if let Some(n) = lno {
        s.push_str(&fmt_num(n, color, &args.colors));
        s.push_str(&sep_out(args, color, sep));
    }
    if let Some(c) = col {
        s.push_str(&fmt_col(c, color, &args.colors));
        s.push_str(&sep_out(args, color, sep));
    }
    s
}

fn funcname_matcher_for_path(repo: &Repository, rel_path: &str) -> Option<FuncnameMatcher> {
    let config = ConfigSet::load(Some(&repo.git_dir), true).ok()?;
    let attrs = load_gitattributes_for_diff(repo).ok()?;
    matcher_for_path_parsed(&config, &attrs.rules, &attrs.macros, rel_path, false)
        .ok()
        .flatten()
}

fn fallback_function_line(line: &str) -> bool {
    let trimmed = line.trim_end();
    if trimmed.is_empty() {
        return false;
    }
    let first = trimmed.as_bytes()[0];
    first.is_ascii_alphabetic() || first == b'_' || first == b'$'
}

fn function_line_matches(line: &str, matcher: Option<&FuncnameMatcher>) -> bool {
    if let Some(matcher) = matcher {
        return matcher.match_line(line).is_some();
    }
    fallback_function_line(line)
}

fn preceding_function_line(
    lines: &[&str],
    match_idx: usize,
    matcher: Option<&FuncnameMatcher>,
) -> Option<usize> {
    let mut i = match_idx;
    while i > 0 {
        i -= 1;
        if function_line_matches(lines[i], matcher) {
            return Some(i);
        }
    }
    None
}

fn function_context_range(
    lines: &[&str],
    match_idx: usize,
    matcher: Option<&FuncnameMatcher>,
) -> (usize, usize) {
    if lines.is_empty() {
        return (0, 0);
    }

    let start_func = if function_line_matches(lines[match_idx], matcher) {
        Some(match_idx)
    } else {
        preceding_function_line(lines, match_idx, matcher)
    };

    if let Some(func_idx) = start_func {
        let start = {
            let mut s = func_idx;
            while s > 0 {
                let prev = lines[s - 1];
                if prev.trim().is_empty() || function_line_matches(prev, matcher) {
                    break;
                }
                s -= 1;
            }
            s
        };
        let mut end = match_idx.max(func_idx);
        let mut i = func_idx + 1;
        while i < lines.len() {
            if function_line_matches(lines[i], matcher) {
                break;
            }
            if lines[i].trim().is_empty() {
                break;
            }
            end = i;
            i += 1;
        }
        (start, end)
    } else {
        let mut start = match_idx;
        while start > 0 && !lines[start - 1].trim().is_empty() {
            start -= 1;
        }
        let mut end = match_idx;
        while end + 1 < lines.len() && !lines[end + 1].trim().is_empty() {
            end += 1;
        }
        (start, end)
    }
}

/// Search content of a single file. Returns true if any match found.
/// `relative_path` is the path shown in grep output (cwd-relative when not `--full-name`).
/// `pager_open_path`, when set, overrides the path collected for `--open-files-in-pager` (normally
/// the same as `relative_path`). Pass `None` so the pager receives `relative_path` (cwd-relative
/// like Git's `-l` output).
/// `rev_label` is e.g. `Some("HEAD")` for object-store grep (`HEAD:path` in output).
#[allow(clippy::too_many_arguments)]
fn grep_content(
    relative_path: &str,
    pager_open_path: Option<&str>,
    rev_label: Option<&str>,
    content: &str,
    compiled: &CompiledGrep,
    args: &Args,
    func_matcher: Option<&FuncnameMatcher>,
    need_sep: &mut bool,
    out: &mut (impl Write + ?Sized),
    open_paths: &mut Option<Vec<String>>,
) -> Result<bool> {
    let color = args.use_color();
    let quoted_path = path_for_output(relative_path, args);
    let full_display = match rev_label {
        Some(r) => format!("{r}:{quoted_path}"),
        None => quoted_path,
    };
    let show_name = args.show_filename();
    let display_name = if show_name {
        full_display.clone()
    } else {
        String::new()
    };

    let mut atom_indices_all: Vec<usize> = Vec::new();
    collect_atom_indices(&compiled.expr, &mut atom_indices_all);

    let lines: Vec<&str> = content.lines().collect();
    let nlines = lines.len();
    let before = args.before_ctx();
    let after = args.after_ctx();
    let use_context = args.has_context() || args.show_function || args.function_context;
    let col_mode = args.column;

    let all_atoms_on_line = args.all_match && compiled.atoms.len() > 1;
    let mut match_indices: Vec<usize> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let mut hit = if all_atoms_on_line {
            line_matches_all_atoms(line, &compiled.atoms)
        } else {
            line_matches_expr(&compiled.expr, line, &compiled.atoms, col_mode)
        };
        if args.invert_match {
            hit = !hit;
        }
        if hit {
            match_indices.push(i);
        }
    }

    if let Some(max) = args.max_count {
        if max >= 0 {
            match_indices.truncate(max as usize);
        }
    }

    let has_match = !match_indices.is_empty();
    let match_count = match_indices.len() as u64;

    let display_name = if args.heading && has_match && show_name {
        String::new()
    } else {
        display_name
    };

    if args.files_with_matches {
        if has_match {
            if let Some(paths) = open_paths {
                // Pager argv must be plain paths; `fmt_name` adds ANSI (t7811 color.grep.* + -O).
                let open_base = pager_open_path.unwrap_or(relative_path);
                let open_quoted = path_for_output(open_base, args);
                let open_full = match rev_label {
                    Some(r) => format!("{r}:{open_quoted}"),
                    None => open_quoted,
                };
                paths.push(open_full);
            } else {
                writeln!(out, "{}", fmt_name(&display_name, color, &args.colors))?;
            }
        }
        return Ok(has_match);
    }

    if args.files_without_match {
        if !has_match {
            writeln!(out, "{}", fmt_name(&display_name, color, &args.colors))?;
            return Ok(true);
        }
        return Ok(false);
    }

    if args.count {
        if match_count > 0 {
            let prefix = line_prefix(&display_name, ':', args, color, None, None);
            write!(out, "{prefix}{match_count}")?;
            writeln!(out)?;
        }
        return Ok(has_match);
    }

    if !has_match {
        return Ok(false);
    }

    if args.file_break && *need_sep {
        writeln!(out)?;
        *need_sep = false;
    }

    if args.heading && show_name {
        writeln!(out, "{}", fmt_name(&full_display, color, &args.colors))?;
    }

    if args.only_matching {
        for &idx in &match_indices {
            let line = lines[idx];
            let mut bol = 0usize;
            let mut cno = 0usize;
            let mut first = true;
            while let Some((abs_s, abs_e)) =
                next_match_from_bol(line, bol, args, compiled, &atom_indices_all)
            {
                if first {
                    cno = abs_s + 1;
                    first = false;
                }
                let matched_text = line.get(abs_s..abs_e).unwrap_or("");
                let prefix = if args.column {
                    line_prefix(&display_name, ':', args, color, Some(idx + 1), Some(cno))
                } else {
                    line_prefix(&display_name, ':', args, color, Some(idx + 1), None)
                };
                if color {
                    writeln!(
                        out,
                        "{}{}",
                        prefix,
                        paint(matched_text, &args.colors.match_selected, true)
                    )?;
                } else {
                    writeln!(out, "{prefix}{matched_text}")?;
                }
                cno += abs_e - bol;
                bol = abs_e;
            }
        }
        *need_sep = true;
        return Ok(true);
    }

    if use_context {
        let mut groups: Vec<(usize, usize)> = Vec::new();
        for &idx in &match_indices {
            let (mut start, end) = if args.function_context {
                function_context_range(&lines, idx, func_matcher)
            } else {
                let start = idx.saturating_sub(before);
                let end = if nlines == 0 {
                    0
                } else {
                    (idx + after).min(nlines - 1)
                };
                (start, end)
            };
            if args.show_function {
                if let Some(func_idx) = preceding_function_line(&lines, idx, func_matcher) {
                    start = start.min(func_idx);
                }
            };
            if let Some(last) = groups.last_mut() {
                if start <= last.1 + 1 {
                    last.1 = last.1.max(end);
                    continue;
                }
            }
            groups.push((start, end));
        }

        let match_set: std::collections::HashSet<usize> = match_indices.iter().copied().collect();

        for &(start, end) in &groups {
            if *need_sep {
                writeln!(out, "{}", paint("--", &args.colors.separator, color))?;
            }
            *need_sep = true;

            for i in start..=end {
                let is_match_line = match_set.contains(&i);
                let is_function_line = (args.show_function || args.function_context)
                    && match_indices.iter().any(|&idx| {
                        (i < idx && preceding_function_line(&lines, idx, func_matcher) == Some(i))
                            || (args.function_context
                                && i == idx
                                && function_line_matches(lines[i], func_matcher))
                    });
                if args.show_function
                    && !args.function_context
                    && !args.has_context()
                    && !is_match_line
                    && !is_function_line
                {
                    continue;
                }
                let sep = if is_match_line {
                    ':'
                } else if is_function_line {
                    '='
                } else {
                    '-'
                };
                let col = if args.column && is_match_line {
                    Some(column_for_line(lines[i], compiled, args.invert_match))
                } else {
                    None
                };
                let prefix = line_prefix(
                    &display_name,
                    sep,
                    args,
                    color,
                    args.show_line_number().then_some(i + 1),
                    col,
                );
                if color && is_match_line {
                    writeln!(
                        out,
                        "{}{}",
                        prefix,
                        colorize_line(lines[i], compiled, &atom_indices_all, &args.colors)
                    )?;
                } else {
                    writeln!(out, "{}{}", prefix, lines[i])?;
                }
            }
        }
    } else {
        for &idx in &match_indices {
            let line = lines[idx];
            let col = args
                .column
                .then_some(column_for_line(line, compiled, args.invert_match));
            let prefix = line_prefix(
                &display_name,
                ':',
                args,
                color,
                args.show_line_number().then_some(idx + 1),
                col,
            );
            if color {
                writeln!(
                    out,
                    "{}{}",
                    prefix,
                    colorize_line(line, compiled, &atom_indices_all, &args.colors)
                )?;
            } else {
                writeln!(out, "{prefix}{line}")?;
            }
        }
    }

    *need_sep = true;
    Ok(true)
}

/// Recursively search a tree object.
#[allow(clippy::too_many_arguments)]
fn grep_tree(
    repo: &Repository,
    tree_data: &[u8],
    root_tree_oid: &ObjectId,
    prefix: &str,
    _depth: usize,
    compiled: &CompiledGrep,
    args: &Args,
    pathspecs: &[String],
    tree_name: Option<&str>,
    need_sep: &mut bool,
    out: &mut (impl Write + ?Sized),
    diff_attrs: &[DiffAttrRule],
    open_paths: &mut Option<Vec<String>>,
) -> Result<bool> {
    let entries = parse_tree(tree_data)?;
    let mut found = false;
    let attr_pathspecs = pathspecs_use_attr_magic(pathspecs);

    for entry in &entries {
        let name = String::from_utf8_lossy(&entry.name);
        let full_name = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };

        let is_tree = entry.mode == 0o040000;
        let is_gitlink = entry.mode == 0o160000;

        // Apply pathspec filter
        let ps_ctx = grit_lib::pathspec::PathspecMatchContext {
            is_directory: is_tree,
            is_git_submodule: is_gitlink,
        };
        let pathspec_matches_path = if pathspecs.is_empty() {
            true
        } else if attr_pathspecs {
            let attr_rules = grit_lib::crlf::load_gitattributes_for_tree_path(
                &repo.odb,
                root_tree_oid,
                &full_name,
            );
            grit_lib::pathspec::matches_pathspec_list_for_object(
                &full_name,
                entry.mode,
                &attr_rules,
                pathspecs,
            )
        } else {
            any_pathspec_matches(&full_name, pathspecs, ps_ctx)
        };
        let pathspec_matches_descendant = (is_tree || is_gitlink)
            && (attr_pathspecs || pathspec_may_match_descendant(&full_name, pathspecs));
        if !pathspec_matches_path && !pathspec_matches_descendant {
            continue;
        }

        // Submodule (gitlink) in tree: recurse if --recurse-submodules
        if is_gitlink {
            if args.recurse_submodules {
                if let Some(work_tree) = &repo.work_tree {
                    if let Ok(sub_repo) =
                        open_submodule_repo_at_path(repo, work_tree, &full_name, &name)
                    {
                        let child_pathspecs: &[String] = if pathspec_matches_path {
                            &[]
                        } else {
                            pathspecs
                        };
                        // The entry.oid is the commit SHA of the submodule
                        let sub_obj = match read_object_for_grep(&sub_repo, &entry.oid) {
                            Ok(o) => o,
                            Err(_) => continue,
                        };
                        let sub_tree_oid = if sub_obj.kind == ObjectKind::Commit {
                            match parse_commit(&sub_obj.data) {
                                Ok(c) => c.tree,
                                Err(_) => continue,
                            }
                        } else {
                            continue;
                        };
                        let sub_tree_obj = match read_object_for_grep(&sub_repo, &sub_tree_oid) {
                            Ok(o) => o,
                            Err(_) => continue,
                        };
                        // Load diff attrs for the submodule
                        let sub_diff_attrs = if let Some(ref swt) = sub_repo.work_tree {
                            load_diff_attrs(swt)
                        } else {
                            vec![]
                        };
                        if grep_tree(
                            &sub_repo,
                            &sub_tree_obj.data,
                            &sub_tree_oid,
                            &full_name,
                            0,
                            compiled,
                            args,
                            child_pathspecs,
                            tree_name,
                            need_sep,
                            out,
                            &sub_diff_attrs,
                            open_paths,
                        )? {
                            found = true;
                        }
                    }
                }
            }
            continue;
        }

        if is_tree {
            let sub_obj = read_object_for_grep(repo, &entry.oid)?;
            if grep_tree(
                repo,
                &sub_obj.data,
                root_tree_oid,
                &full_name,
                _depth + 1,
                compiled,
                args,
                pathspecs,
                tree_name,
                need_sep,
                out,
                diff_attrs,
                open_paths,
            )? {
                found = true;
            }
        } else {
            if let Some(max_depth) = args.effective_max_depth() {
                if !path_allowed_at_max_depth(&full_name, pathspecs, max_depth) {
                    continue;
                }
            }

            let obj = match read_object_for_grep(repo, &entry.oid) {
                Ok(o) => o,
                Err(_) => continue,
            };

            let binary_override = check_binary_override(diff_attrs, &full_name);
            let is_binary = grep_is_binary(repo, &full_name, &obj.data, args, binary_override);

            if is_binary && args.ignore_binary {
                continue;
            }

            if is_binary && !args.text_mode {
                let content = blob_as_grep_text(
                    repo,
                    &full_name,
                    &obj.data,
                    Some(&entry.oid),
                    args,
                    binary_override,
                );
                let has_match = compiled
                    .atoms
                    .iter()
                    .any(|r| r.as_ref().is_some_and(|re| re.is_match(&content)));
                if has_match {
                    if !args.quiet {
                        let display = match tree_name {
                            Some(t) => format!("{t}:{full_name}"),
                            None => full_name.clone(),
                        };
                        writeln!(out, "Binary file {} matches", display)?;
                    }
                    found = true;
                }
            } else {
                let content = blob_as_grep_text(
                    repo,
                    &full_name,
                    &obj.data,
                    Some(&entry.oid),
                    args,
                    binary_override,
                );
                let rel = cwd_strip_repo_rel(repo, &full_name, args);
                if grep_content(
                    &rel,
                    None,
                    tree_name,
                    &content,
                    compiled,
                    args,
                    funcname_matcher_for_path(repo, &full_name).as_ref(),
                    need_sep,
                    out,
                    open_paths,
                )? {
                    found = true;
                }
            }
        }
    }

    Ok(found)
}
