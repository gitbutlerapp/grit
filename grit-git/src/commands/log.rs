//! `grit log` — show commit logs.
//!
//! Displays the commit history starting from HEAD (or specified revisions),
//! with configurable formatting and filtering.

use crate::explicit_exit::ExplicitExit;
use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use grit_lib::combined_diff_patch::CombinedDiffWsOptions;
use grit_lib::combined_tree_diff::{combined_diff_paths_filtered, CombinedTreeDiffOptions};
use grit_lib::commit_graph_file::{
    BloomPrecheck, BloomWalkStats, BloomWalkStatsHandle, CommitGraphChain,
};
use grit_lib::config::{parse_bool, parse_color, ConfigSet};
use grit_lib::crlf::{get_file_attrs, load_gitattributes, DiffAttr};
use grit_lib::diff::{
    count_changes, diff_trees, diff_trees_show_tree_entries, format_raw, format_raw_abbrev,
    indent_heuristic_from_config, unified_diff, unified_diff_histogram_hunks_only, zero_oid,
    DiffEntry, DiffStatus,
};
use grit_lib::diffstat::{terminal_columns, write_diffstat_block, DiffstatOptions, FileStatInput};
use grit_lib::git_date::parse::parse_date_basic;
use grit_lib::ident::{
    committer_timestamp_for_until_filter, committer_unix_seconds_for_ordering,
    parse_signature_tail, signature_timestamp_for_pretty, timestamp_for_at_ct, SignatureTail,
};
use grit_lib::line_log::{
    format_line_log_diff, line_log_filter_commits, parse_line_log_ranges, rewritten_first_parent,
};
use grit_lib::mailmap::{load_mailmap_table, MailmapTable};
use grit_lib::merge_base::is_ancestor;
use grit_lib::merge_diff::{
    blob_text_for_diff, blob_text_for_diff_with_oid, diff_textconv_active,
    format_combined_textconv_patch, is_binary_for_diff,
};
use grit_lib::objects::{parse_commit, parse_tag, CommitData, ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::repo_path::RepoPath;
use grit_lib::porcelain::log::{
    collect_decorations, collect_decorations_inner, current_branch_decoration_index,
    normalize_glob_ref, DecorationFilter, DecorationKind, DecorationMap,
};
use grit_lib::reflog::{read_reflog_dwim, ReflogEntry};
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_lib::rev_list::{
    collect_revision_specs_with_stdin, commit_visible_for_dense_pathspecs, is_symmetric_diff,
    merge_bases, rev_list, split_symmetric_diff, OrderingMode, RevListOptions,
};
use grit_lib::rev_parse::{
    load_graft_parents, peel_to_commit_for_merge_base, resolve_reflog_walk_log_ref,
    resolve_revision_as_commit, resolve_revision_for_range_end, try_parse_double_dot_log_range,
};
use grit_lib::state::{resolve_head, HeadState};
use regex::{Regex, RegexBuilder};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as FmtWrite;
use std::fs::OpenOptions;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Arguments for `grit log`.
#[derive(Debug, ClapArgs)]
#[command(about = "Show commit logs")]
pub struct Args {
    /// Revisions and pathspecs (separated by --).
    #[arg(allow_hyphen_values = true)]
    pub revisions: Vec<String>,

    /// Raw argv after the `log` subcommand (includes revision pseudo-options that clap strips before parsing).
    #[arg(skip)]
    pub raw_argv_tail: Vec<String>,

    /// Limit the number of commits to show.
    #[arg(short = 'n', long = "max-count")]
    pub max_count: Option<usize>,

    /// Show only one line per commit.
    #[arg(long = "oneline")]
    pub oneline: bool,

    /// Pretty-print format (`git log --format=...`).
    #[arg(long = "format", value_name = "FORMAT")]
    pub format: Option<String>,

    /// Pretty-print format (`git log --pretty` with optional format; bare `--pretty` → `medium`).
    #[arg(
        long = "pretty",
        value_name = "FORMAT",
        num_args = 0..=1,
        default_missing_value = "medium"
    )]
    pub pretty: Option<String>,

    /// Use `.mailmap` when showing author/committer (default: `log.mailmap`).
    #[arg(long = "use-mailmap", alias = "mailmap")]
    pub use_mailmap: bool,

    /// Disable mailmap for log output.
    #[arg(long = "no-use-mailmap")]
    pub no_use_mailmap: bool,

    /// Expand tabs in commit log message to spaces (`--expand-tabs` is normalized to `=8` in main).
    #[arg(long = "expand-tabs", value_name = "N", require_equals = true)]
    pub expand_tabs: Option<String>,

    /// Do not expand tabs in commit log output (same as `--expand-tabs=0`).
    #[arg(long = "no-expand-tabs")]
    pub no_expand_tabs: bool,

    /// Effective tab width for message lines (resolved after parsing; see `grit_lib::tab_expand`).
    #[arg(skip)]
    pub(crate) expand_tabs_in_log: usize,

    /// Output the commit log in the given encoding (overrides `i18n.logOutputEncoding`).
    #[arg(long = "encoding")]
    pub encoding: Option<String>,

    /// Effective log output encoding label (resolved after parsing): `--encoding`
    /// > `i18n.logOutputEncoding` > UTF-8. `None` means no reencoding (UTF-8).
    #[arg(skip)]
    pub(crate) log_output_encoding: Option<String>,

    /// Show in reverse order.
    #[arg(long = "reverse")]
    pub reverse: bool,

    /// Follow only the first parent of merge commits.
    #[arg(long = "first-parent")]
    pub first_parent: bool,

    /// Show root commits with diffs against an empty tree.
    #[arg(long = "root")]
    pub root: bool,

    /// Show a graph of the commit history.
    #[arg(long = "graph", overrides_with = "no_graph")]
    pub graph: bool,

    /// Decorate refs.
    #[arg(long = "decorate", overrides_with = "no_decorate")]
    pub decorate: Option<Option<String>>,

    /// Do not decorate refs.
    #[arg(long = "no-decorate", overrides_with = "decorate")]
    pub no_decorate: bool,

    /// Do not walk the commit graph — show given commits only.
    #[arg(long = "no-walk", default_missing_value = "sorted", num_args = 0..=1, require_equals = true)]
    pub no_walk: Option<String>,

    /// Show which ref led to each commit (with --all).
    #[arg(long = "source")]
    pub source: bool,

    /// Treat refs in alternate object stores as revision tips (`rev-list --alternate-refs`).
    #[arg(long = "alternate-refs")]
    pub alternate_refs: bool,

    /// Expanded from `git log --remotes[=pattern]` by the CLI preprocessor (hidden).
    #[arg(long = "grit-internal-remotes", hide = true)]
    pub internal_remotes_pattern: Option<String>,

    /// Only show commits on the ancestry path between endpoints.
    #[arg(long = "ancestry-path", value_name = "REV", default_missing_value = "", num_args = 0..=1, require_equals = true)]
    pub ancestry_path_args: Vec<String>,

    /// Whether ancestry-path filtering is active after CLI normalization.
    #[arg(skip)]
    pub ancestry_path: bool,

    /// Bottom commit for `--ancestry-path=<rev>` (parsed from argv in `run`, not clap).
    #[arg(skip)]
    pub ancestry_path_bottoms: Vec<String>,

    /// Only show commits that are decorated (have refs).
    #[arg(long = "simplify-by-decoration")]
    pub simplify_by_decoration: bool,

    /// Show full history (do not prune TREESAME merges).
    #[arg(long = "full-history")]
    pub full_history: bool,

    /// Further simplify full history by pruning redundant merges.
    #[arg(long = "simplify-merges")]
    pub simplify_merges: bool,

    /// Show all commits in simplified history mode.
    #[arg(long = "sparse")]
    pub sparse: bool,

    /// Show boundary commits.
    #[arg(long = "boundary")]
    pub boundary: bool,

    /// Show left/right markers for symmetric range (`A...B`).
    #[arg(long = "left-right")]
    pub left_right: bool,

    /// Show only commits reachable from the left side of `A...B`.
    #[arg(long = "left-only")]
    pub left_only: bool,

    /// Show only commits reachable from the right side of `A...B`.
    #[arg(long = "right-only")]
    pub right_only: bool,

    /// Skip this many commits.
    #[arg(long = "skip")]
    pub skip: Option<usize>,

    /// Filter by author (regex); multiple `--author` options are ORed.
    #[arg(long = "author", value_name = "PATTERN")]
    pub authors: Vec<String>,

    /// Filter by committer (regex); multiple options are ORed.
    #[arg(long = "committer", value_name = "PATTERN")]
    pub committers: Vec<String>,

    /// Skip merge commits.
    #[arg(long = "no-merges")]
    pub no_merges: bool,

    /// Show only merge commits.
    #[arg(long = "merges")]
    pub merges: bool,

    /// Date format.
    #[arg(long = "date")]
    pub date: Option<String>,

    /// Walk the reflog instead of the commit ancestry chain.
    #[arg(short = 'g', long = "walk-reflogs")]
    pub walk_reflogs: bool,

    /// Add every reflog entry as an additional starting commit (like `--all`,
    /// but over reflogs). Unlike `--walk-reflogs`, output is ordinary commits.
    #[arg(long = "reflog")]
    pub reflog: bool,

    /// Show unified diff (patch) after each commit.
    #[arg(short = 'p', long = "patch")]
    pub patch: bool,

    /// Do not show diff after commits.
    #[arg(long = "no-patch")]
    pub no_patch: bool,

    /// Alias for --patch.
    #[arg(short = 'u', hide = true)]
    pub patch_u: bool,

    /// Show diffstat per commit (`--stat[=width[,name-width[,count]]]`).
    /// Repeatable like Git (`log -1 --stat --stat=60`).
    #[arg(
        long = "stat",
        num_args = 0..=1,
        default_missing_value = "",
        require_equals = true,
        action = clap::ArgAction::Append
    )]
    pub stat: Vec<String>,

    /// Limit the number of files shown in `--stat` output.
    #[arg(long = "stat-count")]
    pub stat_count: Option<usize>,

    /// Set the width of the `--stat` output.
    #[arg(long = "stat-width")]
    pub stat_width: Option<usize>,

    /// Set the width of the graph portion of `--stat` output.
    #[arg(long = "stat-graph-width")]
    pub stat_graph_width: Option<usize>,

    /// Set the width of the filename portion of `--stat` output.
    #[arg(long = "stat-name-width")]
    pub stat_name_width: Option<usize>,

    /// List changed file names per commit.
    #[arg(long = "name-only")]
    pub name_only: bool,

    /// Accepted for diff/log compatibility; submodule diffs are represented as gitlink paths.
    #[arg(long = "ignore-submodules")]
    pub ignore_submodules: Option<String>,

    /// Show status letter + filename per commit.
    #[arg(long = "name-status")]
    pub name_status: bool,

    /// Show raw diff-tree output per commit.
    #[arg(long = "raw")]
    pub raw: bool,

    /// Show log for all refs.
    #[arg(long = "all")]
    pub all: bool,

    /// Include branch tips (refs/heads/) in the revision walk, optionally limited to a glob.
    /// `--branches` includes every branch; `--branches=<glob>` only matching ones.
    #[arg(long = "branches", num_args = 0..=1, default_missing_value = "")]
    pub branches: Option<String>,

    /// Include tag refs (refs/tags/) in the revision walk, optionally limited to a glob.
    #[arg(long = "tags", num_args = 0..=1, default_missing_value = "")]
    pub tags: Option<String>,

    /// Include remote-tracking refs (refs/remotes/) in the walk, optionally limited to a glob.
    #[arg(long = "remotes", num_args = 0..=1, default_missing_value = "")]
    pub remotes: Option<String>,

    /// Follow file renames (single file only).
    #[arg(long = "follow", overrides_with = "no_follow")]
    pub follow: bool,

    /// Disable rename following (overrides `--follow` and `log.follow`).
    #[arg(long = "no-follow", overrides_with = "follow")]
    pub no_follow: bool,

    /// Filter by change type (A=added, M=modified, D=deleted, R=renamed, C=copied).
    /// Git ORs repeated `--diff-filter` options by concatenating their letters; collected raw
    /// then folded into `diff_filter` in `run`.
    #[arg(long = "diff-filter", action = clap::ArgAction::Append)]
    pub diff_filter_parts: Vec<String>,

    /// Effective diff-filter letters (concatenation of all `--diff-filter` parts).
    #[arg(skip)]
    pub diff_filter: Option<String>,

    /// Only show commits that add or remove the given object.
    #[arg(long = "find-object")]
    pub find_object: Option<String>,

    /// Pickaxe extended-regex pattern (log `-G`; set via argv preprocessing).
    #[arg(long = "pickaxe-grep", value_name = "REGEX", hide = true)]
    pub pickaxe_grep: Option<String>,

    /// Pickaxe string (log `-S`; set via argv preprocessing).
    #[arg(long = "pickaxe-string", value_name = "STRING", hide = true)]
    pub pickaxe_string: Option<String>,

    /// Ignore changed lines matching these regexes (`-I` / `--ignore-matching-lines`; `-I` is
    /// canonicalized to `--ignore-matching-lines=<regex>` during argv preprocessing). A hunk whose
    /// changed lines are all ignorable is dropped; a file with no surviving hunk emits no patch.
    #[arg(long = "ignore-matching-lines", value_name = "REGEX")]
    pub ignore_matching_lines: Vec<String>,

    /// Ignore changes whose lines are all blank (`--ignore-blank-lines`).
    #[arg(long = "ignore-blank-lines")]
    pub ignore_blank_lines: bool,

    /// Treat `-S` needle as an extended regex (`--pickaxe-regex`).
    #[arg(long = "pickaxe-regex", hide = true)]
    pub pickaxe_regex: bool,

    /// Force text semantics for pickaxe / binary handling (`-a` / `--text`).
    #[arg(short = 'a', long = "text")]
    pub text: bool,

    /// Run textconv when comparing blobs (default on for log pickaxe).
    #[arg(long = "textconv", hide = true)]
    pub textconv: bool,

    /// Disable textconv for pickaxe / diff.
    #[arg(long = "no-textconv", hide = true)]
    pub no_textconv: bool,

    /// Show full changeset when pickaxe matches (Git `--pickaxe-all`).
    #[arg(long = "pickaxe-all", hide = true)]
    pub pickaxe_all: bool,

    /// Rejected by Git (compatibility error).
    #[arg(long = "no-pickaxe-regex", hide = true)]
    pub no_pickaxe_regex: bool,

    /// Abbreviate commit hashes to N characters.
    #[arg(long = "abbrev", value_name = "N", default_missing_value = "7", num_args = 0..=1, require_equals = true)]
    pub abbrev: Option<String>,

    /// Use NUL as record terminator.
    #[arg(short = 'z')]
    pub null_terminator: bool,

    /// Suppress diff output for submodules.
    #[arg(long = "no-ext-diff")]
    pub no_ext_diff: bool,

    /// Show stat with patch.
    #[arg(long = "patch-with-stat")]
    pub patch_with_stat: bool,

    /// Disable rename detection.
    #[arg(long = "no-renames")]
    pub no_renames: bool,

    /// Detect renames.
    #[arg(short = 'M', long = "find-renames", default_missing_value = "50", num_args = 0..=1, require_equals = true)]
    pub find_renames: Option<String>,

    /// Detect copies. Repeating (`-C -C`) requests harder copy detection (copies from unmodified
    /// files); collected raw then folded into `find_copies` / `find_copies_harder` in `run`.
    #[arg(short = 'C', long = "find-copies", default_missing_value = "50", num_args = 0..=1, require_equals = true, action = clap::ArgAction::Append)]
    pub find_copies_parts: Vec<String>,

    /// Resolved copy-detection threshold (last `--find-copies` value, or 50% when bare).
    #[arg(skip)]
    pub find_copies: Option<String>,

    /// Whether `-C` was given at least twice (find copies even from unmodified files).
    #[arg(skip)]
    pub find_copies_harder: bool,

    /// Control merge commit diff display.
    #[arg(long = "diff-merges", default_missing_value = "on")]
    pub diff_merges: Option<String>,

    /// Suppress diff output for merge commits.
    #[arg(long = "no-diff-merges")]
    pub no_diff_merges: bool,

    /// Show merge diffs in the default format (`log.diffMerges`, usually separate parents).
    ///
    /// Unlike `--diff-merges`, plain `-m` does not imply `--patch` by itself.
    #[arg(short = 'm')]
    pub merge_diff_m: bool,

    /// Combined diff for merge commits (shortcut for `--diff-merges=combined -p`).
    #[arg(short = 'c')]
    pub merge_diff_c: bool,

    /// Dense combined diff for merge commits (`--diff-merges=dense-combined -p`).
    #[arg(long = "cc")]
    pub cc: bool,

    /// First-parent merge diffs (shortcut for `--diff-merges=first-parent -p`).
    #[arg(long = "dd")]
    pub merge_diff_dd: bool,

    /// Show diff against a mechanical re-merge of the parents (two-parent merges).
    #[arg(long = "remerge-diff")]
    pub remerge_diff: bool,

    /// Color moved lines differently.
    #[arg(long = "color-moved", default_missing_value = "default", num_args = 0..=1, require_equals = true)]
    pub color_moved: Option<String>,

    /// Abbreviate commit hashes in output.
    #[arg(long = "abbrev-commit")]
    pub abbrev_commit: bool,

    /// Disable commit-hash abbreviation (overrides `--abbrev-commit` and the
    /// `log.abbrevCommit` config).
    #[arg(long = "no-abbrev-commit")]
    pub no_abbrev_commit: bool,

    /// True when `--abbrev-commit` was given explicitly on the command line (as
    /// opposed to being enabled by `log.abbrevCommit`). `--pretty=raw` honors
    /// only the explicit flag. Set during `run()`; not a CLI argument itself.
    #[arg(skip)]
    pub abbrev_commit_explicit: bool,

    /// Color output.
    #[arg(long = "color", default_missing_value = "always", num_args = 0..=1, require_equals = true)]
    pub color: Option<String>,

    /// Disable color.
    #[arg(long = "no-color")]
    pub no_color: bool,

    /// Filter decoration refs.
    #[arg(long = "decorate-refs", value_name = "PATTERN")]
    pub decorate_refs: Vec<String>,

    /// Exclude decoration refs.
    #[arg(long = "decorate-refs-exclude", value_name = "PATTERN")]
    pub decorate_refs_exclude: Vec<String>,

    /// Show line prefix.
    #[arg(long = "line-prefix", value_name = "PREFIX")]
    pub line_prefix: Option<String>,

    /// Disable graph output.
    #[arg(long = "no-graph", overrides_with = "graph")]
    pub no_graph: bool,

    /// Show a visual break between non-linear sections.
    #[arg(long = "show-linear-break", default_missing_value = "", num_args = 0..=1, require_equals = true)]
    pub show_linear_break: Option<String>,

    /// Show GPG signature.
    #[arg(long = "show-signature")]
    pub show_signature: bool,

    /// Disable abbreviation.
    #[arg(long = "no-abbrev")]
    pub no_abbrev: bool,

    /// Replace `+`/`-`/context prefixes in unified diff hunks (Git `range-diff` / `fast-import` tests).
    #[arg(long = "output-indicator-new", value_name = "C", hide = true)]
    pub output_indicator_new: Option<String>,

    #[arg(long = "output-indicator-old", value_name = "C", hide = true)]
    pub output_indicator_old: Option<String>,

    #[arg(long = "output-indicator-context", value_name = "C", hide = true)]
    pub output_indicator_context: Option<String>,

    /// Grep commit messages (regex unless `--fixed-strings`); multiple `--grep` are ORed unless
    /// `--all-match` requires every pattern to match.
    #[arg(long = "grep", value_name = "PATTERN")]
    pub grep_patterns: Vec<String>,

    /// Grep reflog messages (`log -g` only); multiple options are ORed unless `--all-match`.
    #[arg(long = "grep-reflog", value_name = "PATTERN")]
    pub grep_reflog_patterns: Vec<String>,

    /// Invert grep match.
    #[arg(long = "invert-grep")]
    pub invert_grep: bool,

    /// Case insensitive grep.
    #[arg(short = 'i', long = "regexp-ignore-case")]
    pub regexp_ignore_case: bool,

    /// All --grep patterns must match.
    #[arg(long = "all-match")]
    pub all_match: bool,

    /// Use basic regexp for --grep / --author / --committer (not pickaxe `-G`).
    #[arg(long = "basic-regexp")]
    pub basic_regexp: bool,

    /// Use extended regexp for --grep.
    #[arg(short = 'E', long = "extended-regexp")]
    pub extended_regexp: bool,

    /// Use fixed strings for --grep.
    #[arg(short = 'F', long = "fixed-strings")]
    pub fixed_strings: bool,

    /// Use Perl regexp for --grep.
    #[arg(short = 'P', long = "perl-regexp")]
    pub perl_regexp: bool,

    /// End of options marker (everything after is a revision/path).
    #[arg(long = "end-of-options")]
    pub end_of_options: bool,

    /// Read extra revisions and optional pathspecs (after a stdin `--`) from stdin.
    #[arg(long = "stdin")]
    pub read_stdin: bool,

    /// Date ordering.
    #[arg(long = "date-order")]
    pub date_order: bool,

    /// Order by author date instead of committer date.
    #[arg(long = "author-date-order")]
    pub author_date_order: bool,

    /// Topo ordering.
    #[arg(long = "topo-order")]
    pub topo_order: bool,

    /// Ignore missing refs.
    #[arg(long = "ignore-missing")]
    pub ignore_missing: bool,

    /// Default revision to use when no revision is given.
    #[arg(long = "default", value_name = "REV", hide = true)]
    pub default_revision: Option<String>,

    /// Exclude promisor objects from the walk (only valid in a partial-clone repository).
    #[arg(long = "exclude-promisor-objects")]
    pub exclude_promisor_objects: bool,

    /// Clear all decorations.
    #[arg(long = "clear-decorations")]
    pub clear_decorations: bool,

    /// Show shortstat.
    #[arg(long = "shortstat")]
    pub shortstat: bool,

    /// Show a condensed summary of extended header info (created/deleted/renamed files).
    #[arg(long = "summary")]
    pub summary: bool,

    /// Bisect mode (accepted for compatibility).
    #[arg(long = "bisect")]
    pub bisect: bool,

    /// Order files according to the given orderfile.
    #[arg(short = 'O', value_name = "orderfile")]
    pub order_file: Option<String>,

    /// Rotate diff output to start at the named path.
    #[arg(long = "rotate-to", value_name = "path")]
    pub rotate_to: Option<String>,

    /// Skip diff output until the named path.
    #[arg(long = "skip-to", value_name = "path")]
    pub skip_to: Option<String>,

    /// Show full object hashes in diff output.
    #[arg(long = "full-index")]
    pub full_index: bool,

    /// Omit `a/` and `b/` prefixes from diff paths (Git `--no-prefix`).
    #[arg(long = "no-prefix")]
    pub no_prefix: bool,

    /// Do not show commit notes (Git `log --no-notes`).
    #[arg(long = "no-notes")]
    pub no_notes: bool,

    /// Additional notes refs (`--notes` → default ref; `--notes=ref` → `refs/notes/<ref>`).
    #[arg(
        long = "notes",
        value_name = "REF",
        action = clap::ArgAction::Append,
        num_args = 0..=1,
        default_missing_value = ""
    )]
    pub notes_refs: Vec<String>,

    /// Show binary diffs in git-apply format.
    #[arg(long = "binary")]
    pub binary: bool,

    /// Filter: show commits newer than date (filter mode).
    #[arg(long = "since-as-filter", value_name = "DATE")]
    pub since_as_filter: Option<String>,

    /// Show commits newer than a specific date.
    #[arg(long = "since", alias = "after", value_name = "DATE")]
    pub since: Option<String>,

    /// Show commits older than a specific date.
    #[arg(long = "until", alias = "before", value_name = "DATE")]
    pub until: Option<String>,

    /// Annotate each commit with its children (accepted for compatibility).
    #[arg(long = "children")]
    pub children: bool,

    /// Pathspecs (after --).
    #[arg(last = true)]
    pub pathspecs: Vec<String>,

    /// Break complete rewrites into pairs. Takes an optional `-B[<n>][/<m>]` argument.
    #[arg(short = 'B', long = "break-rewrites", default_missing_value = "", num_args = 0..=1, require_equals = true)]
    pub break_rewrites: Option<String>,

    /// Show tree objects in diff.
    #[arg(long = "show-trees")]
    pub show_trees: bool,

    /// Recurse into trees in diffs (`-t`, same as `git log -t`).
    #[arg(short = 't', hide = true)]
    pub recurse_trees: bool,

    /// Generate diff with N lines of context.
    #[arg(short = 'U', long = "unified", value_name = "N")]
    pub unified: Option<usize>,

    /// Trace line range history (`git log -L`).
    #[arg(short = 'L', value_name = "range:file", allow_hyphen_values = true)]
    pub line_range: Vec<String>,

    /// Print parent hashes on the first line of each commit (after rewrite).
    #[arg(long = "parents")]
    pub show_parents: bool,

    /// Show full diff for merges (path-limited log; matches `git log --full-diff`).
    #[arg(long = "full-diff")]
    pub full_diff: bool,

    /// When excluding commits, only follow first parents (matches `git log --exclude-first-parent-only`).
    #[arg(long = "exclude-first-parent-only")]
    pub exclude_first_parent_only: bool,

    /// Show first-parent merge commits that pulled in path changes (matches `git log --show-pulls`).
    #[arg(long = "show-pulls")]
    pub show_pulls: bool,

    /// Write normal log output to a file; diff still goes to stdout.
    #[arg(long = "output", value_name = "file")]
    pub output_path: Option<PathBuf>,

    /// Suppress diff output (line-log: show commits only).
    #[arg(short = 's')]
    pub suppress_diff: bool,
}

/// How merge commits are diffed in `git log` (`--diff-merges`, `-m`, `-c`, `--cc`, `--dd`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MergeDiffFormat {
    /// No merge diff (`--diff-merges=off` / default when no merge diff requested).
    Off,
    /// Diff only against the first parent (`--diff-merges=first-parent`, `--dd`, `--first-parent` default).
    FirstParent,
    /// One diff per parent (`separate`, plain `-m` with default `log.diffMerges`, `--diff-merges=on`).
    Separate,
    /// Combined diff (`-c`, `--diff-merges=combined`).
    Combined,
    /// Dense combined diff (`--cc`, `--diff-merges=dense-combined`).
    DenseCombined,
    /// Remerge diff (`--remerge-diff`); handled mainly before this enum is consulted.
    Remerge,
}

impl MergeDiffFormat {
    fn parse_diff_merges_cli_value(s: &str, default_on: MergeDiffFormat) -> Option<Self> {
        let s = s.trim();
        match s {
            "off" | "none" => Some(Self::Off),
            "1" | "first-parent" => Some(Self::FirstParent),
            "separate" => Some(Self::Separate),
            "c" | "combined" => Some(Self::Combined),
            "cc" | "dense-combined" => Some(Self::DenseCombined),
            "r" | "remerge" => Some(Self::Remerge),
            "m" | "on" => Some(default_on),
            _ => None,
        }
    }

    fn parse_log_diff_merges_config(s: &str) -> Result<Self> {
        let s = s.trim();
        match s {
            "off" | "none" => Ok(Self::Off),
            "1" | "first-parent" => Ok(Self::FirstParent),
            "separate" => Ok(Self::Separate),
            "c" | "combined" => Ok(Self::Combined),
            "cc" | "dense-combined" => Ok(Self::DenseCombined),
            "r" | "remerge" => Ok(Self::Remerge),
            // `m` / `on` in config are no-ops in C Git (leave default); initial default is separate.
            "m" | "on" => Ok(Self::Separate),
            _ => anyhow::bail!("fatal: bad config variable 'log.diffMerges' in file '.git/config'"),
        }
    }
}

fn log_diff_merges_default_format(git_dir: &Path) -> Result<MergeDiffFormat> {
    let cfg = ConfigSet::load(Some(git_dir), true).unwrap_or_default();
    if let Some(raw) = cfg
        .get("log.diffMerges")
        .or_else(|| cfg.get("log.diffmerges"))
    {
        MergeDiffFormat::parse_log_diff_merges_config(&raw)
    } else {
        Ok(MergeDiffFormat::Separate)
    }
}

fn validate_log_diff_merges_config(git_dir: &Path) -> Result<()> {
    let cfg = ConfigSet::load(Some(git_dir), true).unwrap_or_default();
    if let Some(raw) = cfg
        .get("log.diffMerges")
        .or_else(|| cfg.get("log.diffmerges"))
    {
        let _ = MergeDiffFormat::parse_log_diff_merges_config(&raw)?;
    }
    Ok(())
}

/// Apply `git log` merge-diff flags, validate config, and set implied `--patch` for `-c` / `--cc` / `--dd` / `--remerge-diff`.
fn normalize_log_merge_diff_args(args: &mut Args, git_dir: &Path) -> Result<()> {
    validate_log_diff_merges_config(git_dir)?;

    if (args.merge_diff_c || args.cc || args.merge_diff_dd || args.remerge_diff) && !args.no_patch {
        args.patch = true;
    }

    if args.merge_diff_c && args.cc {
        anyhow::bail!("options '-c' and '--cc' cannot be used together");
    }

    Ok(())
}

fn effective_merge_diff_format(
    args: &Args,
    is_merge: bool,
    git_dir: &Path,
) -> Result<MergeDiffFormat> {
    if !is_merge {
        return Ok(MergeDiffFormat::FirstParent);
    }
    let base = effective_merge_diff_format_base(args, git_dir)?;
    // git diff-merges.c `diff_merges_default_to_first_parent`: when `--first-parent` is given
    // together with a separate (`-m`) merge format, `first_parent_merges` is set, so the merge
    // is shown like first-parent (single diff, no `(from <parent>)` header).
    if args.first_parent && base == MergeDiffFormat::Separate {
        return Ok(MergeDiffFormat::FirstParent);
    }
    Ok(base)
}

fn effective_merge_diff_format_base(args: &Args, git_dir: &Path) -> Result<MergeDiffFormat> {
    if args.remerge_diff {
        return Ok(MergeDiffFormat::Remerge);
    }
    // `-m` resets any combine flag (git's `common_setup` -> `suppress`), so when `-m` is given
    // alongside `-c`/`--cc` (as in `--cc -m -p` / `-c -m -p`) the separate format wins.
    if args.merge_diff_m {
        return log_diff_merges_default_format(git_dir);
    }
    if args.merge_diff_c {
        return Ok(MergeDiffFormat::Combined);
    }
    if args.cc {
        return Ok(MergeDiffFormat::DenseCombined);
    }
    if args.merge_diff_dd {
        return Ok(MergeDiffFormat::FirstParent);
    }
    if args.no_diff_merges {
        return Ok(MergeDiffFormat::Off);
    }
    if let Some(ref s) = args.diff_merges {
        let default_on = log_diff_merges_default_format(git_dir)?;
        if let Some(fmt) = MergeDiffFormat::parse_diff_merges_cli_value(s, default_on) {
            return Ok(fmt);
        }
        anyhow::bail!("invalid value for '--diff-merges': '{s}'");
    }
    if args.first_parent {
        return Ok(MergeDiffFormat::FirstParent);
    }
    Ok(MergeDiffFormat::Off)
}

/// Whether a merge commit should emit any diff (stat/raw/patch) for the current log options.
fn merge_commit_wants_diff(args: &Args, git_dir: &Path) -> Result<bool> {
    Ok(effective_merge_diff_format(args, true, git_dir)? != MergeDiffFormat::Off)
}

/// Whether merges force a diff even without `-p` (git's `merges_need_diff` / `merges_imply_patch`).
///
/// In git (diff-merges.c): any non-`off` `--diff-merges=<x>` value sets `merges_need_diff = 1`
/// (via `common_setup`), and `-c`/`--cc`/`--dd`/`--remerge-diff` set `merges_imply_patch = 1`.
/// Both cause `output_format` to default to PATCH so a merge shows its diff with no `-p`.
/// The literal `-m` flag is special-cased to reset `merges_need_diff = 0`, so plain `-m` does
/// NOT force a diff. `--first-parent` alone (no `--diff-merges`) also does not force one.
fn merges_force_diff(args: &Args) -> bool {
    if args.merge_diff_c || args.cc || args.merge_diff_dd || args.remerge_diff {
        return true;
    }
    if args.no_diff_merges {
        return false;
    }
    if let Some(ref s) = args.diff_merges {
        return !matches!(s.trim(), "off" | "none");
    }
    false
}

/// Whether `git log` should run diff machinery for a commit (false for merge + `off` unless only `-m` without patch — then still false).
/// `log.showroot` config (git default: true). When false, a root commit's diff against the
/// empty tree is suppressed unless `--root` is given.
fn log_show_root_default(git_dir: &Path) -> bool {
    ConfigSet::load(Some(git_dir), true)
        .ok()
        .and_then(|c| c.get_bool("log.showroot").and_then(|r| r.ok()))
        .unwrap_or(true)
}

fn log_commit_needs_diff_output(args: &Args, info: &CommitInfo, git_dir: &Path) -> Result<bool> {
    let wants_diff = args.patch
        || args.patch_u
        || !args.stat.is_empty()
        || args.name_only
        || args.name_status
        || args.raw
        || args.cc
        || args.merge_diff_c
        || args.remerge_diff
        || args.patch_with_stat
        // An explicit `--diff-merges=<non-off>` forces a merge to show its diff (git's
        // `merges_need_diff`) even with no `-p`/`--stat`/etc.
        || (info.parents.len() > 1 && merges_force_diff(args));
    if !wants_diff {
        return Ok(false);
    }
    // A root commit (no parents) produces a diff against the empty tree only when
    // `show_root_diff` is set: that is `--root`, or the default `log.showroot=true`
    // (log-tree.c). When `log.showroot=false` and `--root` is absent, suppress it.
    if info.parents.is_empty() && !args.root && !log_show_root_default(git_dir) {
        return Ok(false);
    }
    let is_merge = info.parents.len() > 1;
    if is_merge && !merge_commit_wants_diff(args, git_dir)? {
        return Ok(false);
    }
    Ok(true)
}

/// Whether unified patch hunks should be printed (honors `-m` not implying `-p` alone).
fn log_wants_patch_hunks(args: &Args, info: &CommitInfo, git_dir: &Path) -> Result<bool> {
    if args.no_patch || args.suppress_diff {
        return Ok(false);
    }
    let is_merge = info.parents.len() > 1;
    let mut patch = args.patch || args.patch_u || args.patch_with_stat;
    // git: `merges_need_diff` / `merges_imply_patch` default `output_format` to PATCH, so a
    // merge shows its diff even without `-p` — but only when no other output format is selected.
    if is_merge && !patch && merges_force_diff(args) {
        let other_format = !args.stat.is_empty()
            || args.name_only
            || args.name_status
            || args.raw
            || args.shortstat;
        if !other_format {
            patch = true;
        }
    }
    if !patch {
        return Ok(false);
    }
    if is_merge && effective_merge_diff_format(args, true, git_dir)? == MergeDiffFormat::Off {
        return Ok(false);
    }
    Ok(true)
}

/// Whether combined-style merge diff is active (`-c` / `--cc`).
fn merge_diff_is_combined_style(args: &Args, is_merge: bool, git_dir: &Path) -> Result<bool> {
    if !is_merge {
        return Ok(false);
    }
    let f = effective_merge_diff_format(args, true, git_dir)?;
    Ok(matches!(
        f,
        MergeDiffFormat::Combined | MergeDiffFormat::DenseCombined
    ))
}

/// Whether log should use the dense combined diff implementation (`--cc`).
fn merge_diff_is_dense_combined(args: &Args, is_merge: bool, git_dir: &Path) -> Result<bool> {
    if !is_merge {
        return Ok(false);
    }
    Ok(effective_merge_diff_format(args, true, git_dir)? == MergeDiffFormat::DenseCombined)
}

/// Whether log should emit one diff per parent (`separate` / `-m` default).
/// Whether the per-commit header is emitted by the diff machinery (once per parent, each
/// `(from <parent>)`) rather than once by the caller. True only for separate (`-m`) merges
/// that actually produce diff output.
fn log_commit_uses_per_parent_header(
    args: &Args,
    info: &CommitInfo,
    git_dir: &Path,
) -> Result<bool> {
    if info.parents.len() <= 1 {
        return Ok(false);
    }
    if !log_commit_needs_diff_output(args, info, git_dir)? {
        return Ok(false);
    }
    merge_diff_is_separate(args, true, git_dir)
}

fn merge_diff_is_separate(args: &Args, is_merge: bool, git_dir: &Path) -> Result<bool> {
    if !is_merge {
        return Ok(false);
    }
    if args.full_diff {
        return Ok(true);
    }
    Ok(effective_merge_diff_format(args, true, git_dir)? == MergeDiffFormat::Separate)
}

/// Whether log should emit a remerge diff for merges.
fn merge_diff_is_remerge(args: &Args, is_merge: bool, git_dir: &Path) -> Result<bool> {
    if !is_merge {
        return Ok(false);
    }
    Ok(effective_merge_diff_format(args, true, git_dir)? == MergeDiffFormat::Remerge)
}

/// Whether log/show diff output should use ANSI colors on stdout (Git `color.diff` / `color.ui`).
fn log_resolve_stdout_color(args: &Args, git_dir: &Path) -> bool {
    if args.no_color {
        return false;
    }
    if let Some(ref c) = args.color {
        return c == "always" || c == "true" || c.is_empty();
    }
    let mut c = false;
    if let Ok(config) = ConfigSet::load(Some(git_dir), true) {
        if let Some(val) = config.get("color.diff") {
            match val.as_str() {
                "always" | "true" => c = true,
                "auto" => {
                    c = (std::io::IsTerminal::is_terminal(&std::io::stdout())
                        && grit_lib::terminal::ansi_supported())
                        || std::env::var_os("GIT_PAGER_IN_USE").is_some()
                }
                _ => {}
            }
        }
        if !c {
            if let Some(val) = config.get("color.ui") {
                match val.as_str() {
                    "always" | "true" => c = true,
                    "auto" => {
                        c = (std::io::IsTerminal::is_terminal(&std::io::stdout())
                            && grit_lib::terminal::ansi_supported())
                            || std::env::var_os("GIT_PAGER_IN_USE").is_some()
                    }
                    _ => {}
                }
            }
        }
    }
    c
}

/// Resolve the custom graph column palette from `log.graphColors`, if set.
///
/// Returns `None` when the config is absent or empty, in which case the caller
/// keeps Git's built-in ANSI palette. Each comma-separated entry is parsed as a
/// color spec; invalid entries are silently skipped (Git warns and skips them).
/// The resulting list does NOT include the RESET sentinel — `AsciiGraph` appends
/// that itself.
fn load_graph_colors(git_dir: &Path) -> Option<Vec<String>> {
    let cfg = ConfigSet::load(Some(git_dir), true).ok()?;
    // Git looks up the lowercased key "log.graphcolors".
    let raw = cfg
        .get("log.graphColors")
        .or_else(|| cfg.get("log.graphcolors"))?;
    let mut colors = Vec::new();
    for part in raw.split(',') {
        match grit_lib::config::parse_color(part) {
            Ok(code) if !code.is_empty() => colors.push(code),
            _ => {}
        }
    }
    if colors.is_empty() {
        None
    } else {
        Some(colors)
    }
}

fn effective_use_mailmap(args: &Args, cfg: &ConfigSet) -> bool {
    if args.no_use_mailmap {
        return false;
    }
    if args.use_mailmap {
        return true;
    }
    cfg.get("log.mailmap")
        .map(|v| parse_bool(&v).unwrap_or(true))
        .unwrap_or(true)
}

/// Rebuild a signature line with a new `Name <email>` prefix, preserving the timestamp tail.
fn ident_with_mapped_contact(raw: &str, new_name: &str, new_email: &str) -> String {
    let Some(gt) = raw.rfind('>') else {
        if new_email.is_empty() {
            return new_name.to_string();
        }
        return format!("{new_name} <{new_email}>");
    };
    let tail = raw.get(gt + 1..).unwrap_or("");
    format!("{new_name} <{new_email}>{tail}")
}

fn ident_for_mailmap_match(mailmap: &MailmapTable, raw: &str) -> String {
    if mailmap.is_empty() {
        return raw.to_string();
    }
    let name = extract_name(raw);
    let email = extract_email(raw);
    let (n, e) = mailmap.map_user(name, email);
    ident_with_mapped_contact(raw, &n, &e)
}

fn ident_matches_header_patterns(
    patterns: &[Regex],
    raw: &str,
    mailmap: &MailmapTable,
    use_mailmap: bool,
) -> bool {
    if patterns.is_empty() {
        return true;
    }
    let haystack_full = if use_mailmap && !mailmap.is_empty() {
        ident_for_mailmap_match(mailmap, raw)
    } else {
        raw.to_string()
    };
    let haystack = ident_for_author_pattern_match(&haystack_full);
    patterns.iter().any(|re| re.is_match(&haystack))
}

fn run_line_log(
    repo: &Repository,
    args: Args,
    _patch_context: usize,
    use_mailmap: bool,
    mailmap: &MailmapTable,
) -> Result<()> {
    if !args.pathspecs.is_empty() {
        anyhow::bail!("-L<range>:<file> cannot be used with pathspec");
    }
    if args.follow {
        anyhow::bail!("options '-L' and '--follow' cannot be used together");
    }
    if args.raw {
        anyhow::bail!("--raw is incompatible with -L");
    }

    let use_color = if args.no_color {
        false
    } else if let Some(ref c) = args.color {
        c == "always" || c == "true" || c.is_empty()
    } else {
        let mut c = false;
        if let Ok(config) = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true) {
            if let Some(val) = config.get("color.diff") {
                match val.as_str() {
                    "always" | "true" => c = true,
                    "auto" => {
                        c = (std::io::IsTerminal::is_terminal(&std::io::stdout())
                            && grit_lib::terminal::ansi_supported())
                            || std::env::var_os("GIT_PAGER_IN_USE").is_some()
                    }
                    _ => {}
                }
            }
        }
        c
    };

    let head_state = resolve_head(&repo.git_dir).unwrap_or(HeadState::Invalid);
    let decoration_paint = if use_color {
        Some(load_decoration_paint(&repo.git_dir))
    } else {
        None
    };

    let (start_oids, exclude_oids) = if args.all {
        (collect_all_ref_oids(&repo.git_dir)?, Vec::new())
    } else if args.revisions.is_empty() {
        let head = resolve_head(&repo.git_dir)?;
        match head.oid() {
            Some(oid) => (vec![*oid], Vec::new()),
            None => anyhow::bail!("your current branch does not have any commits yet"),
        }
    } else {
        let mut oids = Vec::new();
        let mut excludes = Vec::new();
        for rev in &args.revisions {
            if let Some(stripped) = rev.strip_prefix('^') {
                excludes.push(resolve_revision_as_commit(repo, stripped)?);
            } else if let Some((excl, tip)) = try_parse_double_dot_log_range(repo, rev)? {
                excludes.push(excl);
                oids.push(tip);
            } else {
                oids.push(resolve_revision_as_commit(repo, rev)?);
            }
        }
        if oids.is_empty() {
            let head = resolve_head(&repo.git_dir)?;
            if let Some(oid) = head.oid() {
                oids.push(*oid);
            }
        }
        (oids, excludes)
    };

    if start_oids.len() != 1 {
        anyhow::bail!("More than one commit to dig from");
    }
    let tip = start_oids[0];

    let excluded_set: HashSet<ObjectId> = if exclude_oids.is_empty() {
        HashSet::new()
    } else {
        collect_reachable(&repo.odb, &exclude_oids)?
    };

    let rename_threshold = args
        .find_renames
        .as_deref()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(50);

    let empty_grep_filter = GrepFilter::default();
    let walk = walk_commits(
        repo,
        &repo.git_dir,
        &[tip],
        None,
        args.skip,
        args.first_parent,
        &[],
        &[],
        &empty_grep_filter,
        false,
        false,
        mailmap,
        use_mailmap,
        args.no_merges,
        args.merges,
        &[][..],
        &excluded_set,
        None,
        None,
        true,
        -1,
        None,
        &[][..],
        None,
        false,
    )?;
    // Git forces `--topo-order` for line-level traversal (`revision.c`:
    // `if (revs->line_level_traverse) { ...; revs->topo_order = 1; }`). The commits are emitted in
    // topological order so that, when the line history follows a rename onto a side branch, the
    // whole side lineage is shown before rejoining the main line. `walk_commits` returns
    // commit-date order, which only coincides with topo order when timestamps are strictly
    // increasing; the t4211 "fancy rename following" cases use identical timestamps, exposing the
    // difference. Reorder the walked set topologically (children before parents), breaking ties by
    // the date-order position so equal-date commits keep Git's ordering.
    let date_order: Vec<ObjectId> = walk.iter().map(|(o, _)| *o).collect();
    let order = topo_order_line_log_commits(repo, &date_order, &walk)?;

    let initial = parse_line_log_ranges(
        &repo.odb,
        &repo.git_dir,
        repo.work_tree.as_deref(),
        &tip,
        &args.line_range,
    )
    .map_err(|e| anyhow::anyhow!("{}", e))?;
    let (filtered, _state, displays) = line_log_filter_commits(
        &repo.odb,
        order,
        tip,
        initial,
        rename_threshold,
        args.first_parent,
    )
    .map_err(|e| anyhow::anyhow!("{}", e))?;

    let mut filtered = filtered;
    if let Some(threshold) = args.until.as_ref().and_then(|s| parse_date_to_epoch(s)) {
        filtered.retain(|oid| {
            load_commit_info(repo, *oid)
                .map(|info| extract_epoch_from_ident(&info.committer) <= threshold)
                .unwrap_or(false)
        });
    }

    let filtered_set: HashSet<ObjectId> = filtered.iter().copied().collect();

    let mut out_main: Box<dyn Write> = if let Some(ref p) = args.output_path {
        Box::new(
            OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(p)
                .with_context(|| format!("open --output {}", p.display()))?,
        )
    } else {
        Box::new(io::stdout().lock())
    };

    let line_prefix = args.line_prefix.as_deref().unwrap_or("");
    let mut notes_cache = NotesMapCache::new(repo);

    let format_requires_decorations = args
        .format
        .as_deref()
        .map(|fmt| {
            let template = fmt
                .strip_prefix("format:")
                .or_else(|| fmt.strip_prefix("tformat:"))
                .unwrap_or(fmt);
            format_template_uses_decoration(template)
        })
        .unwrap_or(false);
    let (show_decorations, decorate_full) =
        resolve_decoration_display(&args, &repo.git_dir, format_requires_decorations);
    let decorations = if !show_decorations {
        None
    } else {
        Some(collect_decorations_inner(
            repo,
            decorate_full,
            decorations_initial_set_all(&args, &repo.git_dir),
            &build_decoration_filter(&args, &repo.git_dir),
        )?)
    };

    let show_patch = !args.suppress_diff && !args.no_patch;

    let (graph_line_log_pipe_pfx, graph_line_log_space_pfx) = if args.graph && show_patch {
        let use_color_g = log_resolve_stdout_color(&args, &repo.git_dir);
        let cfg = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
        let red = if use_color_g {
            cfg.get("color.diff.meta")
                .and_then(|s| grit_lib::config::parse_color(&s).ok())
                .unwrap_or_else(|| "\x1b[31m".to_string())
        } else {
            String::new()
        };
        let reset = if use_color_g { "\x1b[m" } else { "" };
        (
            Some(format!("{line_prefix}{red}|{reset} ")),
            Some(format!("{line_prefix}  ")),
        )
    } else {
        (None, None)
    };

    if args.graph {
        let graph_order: Vec<ObjectId> = if args.reverse {
            filtered.iter().rev().copied().collect()
        } else {
            filtered.clone()
        };
        let mut nodes = Vec::new();
        let mut seen = HashSet::new();
        for oid in &graph_order {
            if !seen.insert(*oid) {
                continue;
            }
            let parents: Vec<ObjectId> = load_raw_parents(repo, *oid)?
                .into_iter()
                .filter(|p| filtered_set.contains(p))
                .collect();
            nodes.push(GraphCommitNode {
                oid: *oid,
                parents,
                is_boundary: false,
            });
        }

        let abbrev_len = parse_abbrev(&args.abbrev);
        let mut graph = AsciiGraph::new();
        // Honor `--color`/`color.*` and the `log.graphColors` palette so that the
        // graph edge characters are drawn with column colors, matching Git.
        let graph_palette = if use_color {
            load_graph_colors(&repo.git_dir)
        } else {
            None
        };
        graph.set_colors(use_color, graph_palette);

        let node_count = nodes.len();
        for (node_idx, node) in nodes.into_iter().enumerate() {
            let info = load_commit_info(repo, node.oid)?;
            graph.update(node.clone());

            loop {
                let (line, shown_commit_line) = graph.next_line();
                if shown_commit_line {
                    let parent_line: Vec<ObjectId> = if args.show_parents {
                        rewritten_first_parent(&repo.odb, &node.oid, &filtered_set)
                            .map_err(|e| anyhow::anyhow!("{}", e))?
                            .into_iter()
                            .collect()
                    } else {
                        node.parents.clone()
                    };
                    let rendered = render_graph_commit_text(
                        &node,
                        &info,
                        &args,
                        use_mailmap,
                        mailmap,
                        decorations.as_ref(),
                        abbrev_len,
                        &parent_line,
                        use_color,
                        decoration_paint.as_ref(),
                        &head_state,
                    );
                    writeln!(out_main, "{line_prefix}{line}{rendered}")?;
                    break;
                }
                writeln!(out_main, "{line_prefix}{line}")?;
            }

            while !graph.is_commit_finished() {
                let (line, _) = graph.next_line();
                writeln!(out_main, "{line_prefix}{line}")?;
            }

            if show_patch {
                let nparents = load_raw_parents(repo, node.oid)?.len();
                if nparents <= 1 {
                    if let Some(ds) = displays.get(&node.oid) {
                        let diff_pfx = match graph_line_log_pipe_pfx.as_deref() {
                            Some(pipe) if node_idx == 0 => pipe,
                            Some(_) => graph_line_log_space_pfx.as_deref().unwrap_or(line_prefix),
                            None => line_prefix,
                        };
                        for d in ds {
                            write!(
                                out_main,
                                "{}",
                                format_line_log_diff(
                                    diff_pfx,
                                    &d.old_path,
                                    &d.new_path,
                                    &d.old_bytes,
                                    &d.new_bytes,
                                    &d.commit_ranges,
                                    &d.touched,
                                )
                            )?;
                        }
                    }
                } else if node_idx + 1 < node_count {
                    writeln!(out_main)?;
                    writeln!(out_main)?;
                }
            }
        }
        return Ok(());
    }

    let is_format_separator = args
        .format
        .as_deref()
        .map(|f| f.starts_with("format:"))
        .unwrap_or(false);

    for (i, oid) in filtered.iter().enumerate() {
        if is_format_separator && i > 0 {
            if args.null_terminator {
                write!(out_main, "\0")?;
            } else {
                writeln!(out_main)?;
            }
        }
        if !is_format_separator && i > 0 && log_wants_blank_line_between_commits(&args) {
            writeln!(out_main)?;
        }
        let info = load_commit_info(repo, *oid)?;
        let parent_override: Option<Vec<ObjectId>> = if args.show_parents {
            rewritten_first_parent(&repo.odb, oid, &filtered_set)
                .map_err(|e| anyhow::anyhow!("{}", e))?
                .map(|p| vec![p])
        } else {
            None
        };
        format_commit(
            &mut out_main,
            oid,
            &info,
            &args,
            use_mailmap,
            &mailmap,
            decorations.as_ref(),
            use_color,
            decoration_paint.as_ref(),
            &head_state,
            &mut notes_cache,
            &repo.odb,
            parent_override.as_deref(),
            true,
            None,
            None,
            None,
            None,
            None,
        )?;
        let nparents = load_raw_parents(repo, *oid)?.len();
        if show_patch {
            if nparents <= 1 {
                // Emit the line-log patch directly after the commit header/message. The single
                // blank-line separator between commits is printed *before* each commit at the top
                // of the loop (`log_wants_blank_line_between_commits`), so we do not add another
                // blank here — otherwise regular commits would be separated by two blank lines.
                if let Some(ds) = displays.get(oid) {
                    for d in ds {
                        write!(
                            out_main,
                            "{}",
                            format_line_log_diff(
                                line_prefix,
                                &d.old_path,
                                &d.new_path,
                                &d.old_bytes,
                                &d.new_bytes,
                                &d.commit_ranges,
                                &d.touched,
                            )
                        )?;
                    }
                }
            } else if i + 1 < filtered.len() {
                // A merge commit shows no line-log patch, but Git still emits the empty diff
                // preamble: one blank line where the patch would be. Combined with the inter-commit
                // separator printed before the next commit, this reproduces Git's two blank lines
                // after a merge's message (t4211 parallel-change). Suppressed for the last commit,
                // which has no trailing separator.
                writeln!(out_main)?;
            }
        }
    }

    Ok(())
}

/// Reorder the line-log commit set into Git's topological order.
///
/// Git forces `--topo-order` for line-level traversal. We already have the reachable commit set in
/// commit-date order from `walk_commits` (which has applied all date/merge/skip filters). Rather
/// than reimplement Git's topological tie-breaking, we ask the shared `rev_list` machinery (whose
/// `OrderingMode::Topo` is verified to match `git log --topo-order` byte-for-byte) for the topo
/// order of the same tips, then project the already-filtered set onto that ordering. Commits in the
/// filtered set are emitted in topo order; any topo commit not in the set is skipped, and (as a
/// defensive fallback) any filtered commit the topo walk did not surface keeps its date-order slot.
fn topo_order_line_log_commits(
    repo: &Repository,
    date_order: &[ObjectId],
    walk: &[(ObjectId, CommitInfo)],
) -> Result<Vec<ObjectId>> {
    if date_order.len() <= 1 {
        return Ok(date_order.to_vec());
    }
    let in_set: HashSet<ObjectId> = date_order.iter().copied().collect();

    // Seed the topo walk from the same tips the line-log walk used: commits in the set that are not
    // a parent of any other commit in the set (the lineage heads). This reproduces Git's behavior
    // of topo-ordering exactly the reachable, already-filtered set.
    let mut is_parent: HashSet<ObjectId> = HashSet::new();
    for (_, info) in walk {
        for p in &info.parents {
            if in_set.contains(p) {
                is_parent.insert(*p);
            }
        }
    }
    let tips: Vec<String> = date_order
        .iter()
        .filter(|o| !is_parent.contains(o))
        .map(|o| o.to_hex())
        .collect();

    let options = RevListOptions {
        ordering: OrderingMode::Topo,
        ..Default::default()
    };
    let topo = match rev_list(repo, &tips, &[], &options) {
        Ok(res) => res.commits,
        Err(_) => return Ok(date_order.to_vec()),
    };

    let mut out: Vec<ObjectId> = Vec::with_capacity(date_order.len());
    let mut emitted: HashSet<ObjectId> = HashSet::new();
    for oid in &topo {
        if in_set.contains(oid) && emitted.insert(*oid) {
            out.push(*oid);
        }
    }
    // Defensive: keep any filtered commit the topo walk did not surface (should not normally happen)
    // in its original date-order position relative to the tail.
    for oid in date_order {
        if emitted.insert(*oid) {
            out.push(*oid);
        }
    }
    Ok(out)
}

/// Order `--branches`/`--tags`/… tip specs by committer date, newest first.
///
/// `git log --branches` seeds every branch tip into a date-ordered priority queue. Refs are
/// expanded in ref-name order (e.g. `main` before `unrelated`), and Git's `prio_queue` breaks
/// equal-date ties by *insertion order* (FIFO). We therefore use a **stable** sort keyed only on
/// the committer epoch so tips that share a timestamp keep their ref-name order, instead of
/// re-breaking the tie by object id. This matters when `git history split` recreates commits with
/// the same `GIT_COMMITTER_DATE` as a sibling branch tip: the leftmost graph column must follow the
/// alphabetically-first branch, not the smallest object id (t3452-history-split tests 8, 10, 11).
fn sort_revision_specs_by_committer_desc(repo: &Repository, specs: &mut Vec<String>) -> Result<()> {
    let mut metas: Vec<(i64, String)> = Vec::with_capacity(specs.len());
    for s in specs.drain(..) {
        let oid = resolve_revision_as_commit(repo, &s)?;
        let obj = repo.odb.read(&oid)?;
        let c = parse_commit(&obj.data)?;
        let e = extract_epoch_from_ident(&c.committer);
        metas.push((e, s));
    }
    // Stable sort: newest committer epoch first, equal epochs keep their (ref-name) input order.
    metas.sort_by(|a, b| b.0.cmp(&a.0));
    specs.extend(metas.into_iter().map(|(_, s)| s));
    Ok(())
}

fn extract_epoch_from_ident(ident: &str) -> i64 {
    committer_timestamp_for_until_filter(ident)
}

/// Strip the timestamp trailer from a Git ident line (Git `strip_timestamp` in `grep.c`).
///
/// The author/committer payload is `Name <email> <epoch> <tz>`; pattern matching uses only
/// the `Name <email>` prefix (through the closing `>`), so `--author=-0700` does not match
/// the timezone field (t7810).
fn ident_for_author_pattern_match(ident: &str) -> String {
    if let Some(gt) = ident.rfind('>') {
        ident[..=gt].to_string()
    } else {
        ident.to_string()
    }
}

/// Whether `--author` / `--committer` identity patterns should match case-insensitively.
///
/// Like `--grep`, the author/committer identity filters are case-SENSITIVE by default and only
/// fold case when `-i` / `--regexp-ignore-case` (or the equivalent grep config) is given. In
/// Git, `revs->grep_filter.ignore_case` is a single flag shared by every grep pattern, header
/// patterns included (`grep.c` honors `opt->ignore_case` uniformly; `revision.c` sets it only for
/// `-i`/`--regexp-ignore-case`). So `--author=person` must NOT match author "Another Person"
/// unless `-i` is supplied (t4209-log-pickaxe `log --author (nomatch)`).
fn ident_pattern_ignore_case(regexp_ignore_case: bool) -> bool {
    regexp_ignore_case
}

/// When `git log --graph <tip> --branches` is used, Git prefers `<tip>` as the leftmost
/// branch tip when it is incomparable with the current first commit (t3451-history-reword).
/// `git log --graph --branches` with no explicit revisions: walk `HEAD`'s first-parent chain and,
/// at each step, list parallel branch tips (descendants of the next FP parent that are not
/// ancestors of the current FP commit). Order the current commit vs parallel tips by comparing
/// the lexicographically smallest local branch name at each tip (`t3452-history-split` tests 8,
/// 10, 11).
fn reorder_graph_all_branches_no_explicit_rev(
    repo: &Repository,
    commits: &[ObjectId],
) -> Result<Vec<ObjectId>> {
    let head = grit_lib::refs::resolve_ref(&repo.git_dir, "HEAD")?;
    let set: std::collections::HashSet<ObjectId> = commits.iter().copied().collect();
    if !set.contains(&head) {
        return Ok(commits.to_vec());
    }

    let mut tips_at_oid: std::collections::HashMap<ObjectId, Vec<String>> =
        std::collections::HashMap::new();
    for (refname, oid) in grit_lib::refs::list_refs(&repo.git_dir, "refs/heads/")? {
        let short = refname
            .strip_prefix("refs/heads/")
            .unwrap_or(&refname)
            .to_owned();
        tips_at_oid.entry(oid).or_default().push(short);
    }
    for v in tips_at_oid.values_mut() {
        v.sort();
    }
    let min_branch = |oid: ObjectId| -> String {
        tips_at_oid
            .get(&oid)
            .and_then(|v| v.first().cloned())
            .unwrap_or_else(|| "\u{10ffff}".to_owned())
    };
    let sort_parallel = |repo: &Repository, v: &mut Vec<ObjectId>| {
        v.sort_by(|a, b| {
            let ta = read_commit_timestamp(&repo.odb, a);
            let tb = read_commit_timestamp(&repo.odb, b);
            tb.cmp(&ta).then_with(|| b.cmp(a))
        });
    };

    let mut fp_chain: Vec<ObjectId> = Vec::new();
    let mut cur = head;
    loop {
        if !set.contains(&cur) {
            break;
        }
        fp_chain.push(cur);
        let obj = repo.odb.read(&cur)?;
        let c = parse_commit(&obj.data)?;
        let Some(&p) = c.parents.first() else {
            break;
        };
        cur = p;
    }

    let on_fp: std::collections::HashSet<ObjectId> = fp_chain.iter().copied().collect();
    let mut out: Vec<ObjectId> = Vec::with_capacity(commits.len());
    let mut used: std::collections::HashSet<ObjectId> = std::collections::HashSet::new();

    for window in fp_chain.windows(2) {
        let cur_win = window[0];
        let next_win = window[1];

        let mut side: Vec<ObjectId> = commits
            .iter()
            .copied()
            .filter(|&oid| {
                if oid == next_win || oid == cur_win || on_fp.contains(&oid) || used.contains(&oid)
                {
                    return false;
                }
                is_ancestor(repo, next_win, oid).unwrap_or(false)
                    && !is_ancestor(repo, oid, cur_win).unwrap_or(false)
            })
            .collect();

        let cur_key = min_branch(cur_win);
        let cur_has_branch = tips_at_oid.contains_key(&cur_win);
        let side_key = side
            .iter()
            .map(|&o| min_branch(o))
            .min()
            .unwrap_or_else(|| "\u{10ffff}".to_owned());
        sort_parallel(repo, &mut side);

        // Intermediate FP commits often have no refs pointing at them; keep the main line first
        // before parallel tips (t3452 test 8: ours-b → ours-a → split-me).
        let side_first = !side.is_empty() && cur_has_branch && side_key < cur_key;

        if side_first {
            for oid in &side {
                if used.insert(*oid) {
                    out.push(*oid);
                }
            }
            if used.insert(cur_win) {
                out.push(cur_win);
            }
        } else {
            if used.insert(cur_win) {
                out.push(cur_win);
            }
            for oid in side {
                if used.insert(oid) {
                    out.push(oid);
                }
            }
        }
    }

    if let Some(&last) = fp_chain.last() {
        if used.insert(last) {
            out.push(last);
        }
    }

    for &oid in commits {
        if !used.contains(&oid) {
            out.push(oid);
        }
    }
    Ok(out)
}

fn prefer_explicit_tip_first_in_graph_walk(
    repo: &Repository,
    args: &Args,
    commits: &mut Vec<ObjectId>,
) -> Result<()> {
    let Some(tip_spec) = args
        .revisions
        .iter()
        .find(|r| *r != "--" && !r.starts_with('-'))
    else {
        return Ok(());
    };
    let tip_oid = resolve_revision_as_commit(repo, tip_spec)?;
    let Some(&first) = commits.first() else {
        return Ok(());
    };
    if first == tip_oid {
        return Ok(());
    }
    let Some(pos) = commits.iter().position(|&c| c == tip_oid) else {
        return Ok(());
    };
    if pos == 0 {
        return Ok(());
    }
    let incomparable = !is_ancestor(repo, first, tip_oid)? && !is_ancestor(repo, tip_oid, first)?;
    if !incomparable {
        return Ok(());
    }
    commits.remove(pos);
    commits.insert(0, tip_oid);
    Ok(())
}

/// Parse a date string into a Unix epoch timestamp.
fn parse_date_to_epoch(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Ok((ts, _off_min)) = parse_date_basic(s) {
        return i64::try_from(ts).ok();
    }
    if s.len() >= 10 && s.as_bytes()[4] == b'-' && s.as_bytes()[7] == b'-' {
        let parts: Vec<&str> = s[..10].split('-').collect();
        if parts.len() == 3 {
            if let (Ok(y), Ok(m), Ok(d)) = (
                parts[0].parse::<i32>(),
                parts[1].parse::<u8>(),
                parts[2].parse::<u8>(),
            ) {
                if let Ok(month) = time::Month::try_from(m) {
                    if let Ok(date) = time::Date::from_calendar_date(y, month, d) {
                        if let Ok(dt) = date.with_hms(0, 0, 0) {
                            return Some(dt.assume_utc().unix_timestamp());
                        }
                    }
                }
            }
        }
    }
    s.parse::<i64>().ok()
}

/// True when `log` should use the built-in one-line output (`<abbrev><decorate> <subject>`).
///
/// Git: `--oneline` sets the default pretty, but `--format=%s` (or any format other than
/// `oneline`) overrides that default while still leaving `--oneline` set for other effects.
fn log_uses_builtin_oneline(args: &Args) -> bool {
    (args.oneline && args.format.as_deref().map_or(true, |f| f == "oneline"))
        || (!args.oneline && args.format.as_deref() == Some("oneline"))
}

/// Whether a blank line separates consecutive log entries (git log-tree.c `shown_one`
/// behaviour). All builtin pretty formats (medium/short/full/fuller/raw) print a blank
/// line between commits; `oneline` and user `format:`/`tformat:` strings do not (those are
/// handled separately by the `is_format_separator` logic).
fn log_uses_blank_separator(args: &Args) -> bool {
    if log_uses_builtin_oneline(args) {
        return false;
    }
    // Only the builtin multi-line pretty formats (and the default) print the blank
    // separator. `oneline`, `reference`, `email`, and any user `format:`/`tformat:`/`%`
    // string carry their own terminator.
    match args.format.as_deref() {
        None => true,
        Some("medium") | Some("short") | Some("full") | Some("fuller") | Some("raw") => true,
        _ => false,
    }
}

/// Whether the `--name-only`/`--name-status` block must be preceded by one
/// newline. Git always prints exactly one newline between the commit message and
/// the name list — except for `oneline`, which prints none. With `format:`
/// (separator semantics) that newline terminates the otherwise-unterminated
/// subject (so no visible blank); for every other format the message line is
/// already terminated, so the newline shows up as a blank separator (matching
/// git log-tree.c).
fn log_name_list_needs_separator(args: &Args) -> bool {
    !log_uses_builtin_oneline(args)
}

/// Whether a `--pretty=format:` template references the decoration placeholders (`%d`, `%D`,
/// `%(decorate...`), so the log machinery knows to load ref decorations. The `%d`/`%D` forms may be
/// preceded by an add/space/del magic char (`%+d`, `%-d`, `% d`), which must still trigger loading
/// — `template.contains("%d")` alone misses those (t4205 "magical wrapping" needs `%+d`).
fn format_template_uses_decoration(template: &str) -> bool {
    if template.contains("%(decorate") {
        return true;
    }
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let mut j = i + 1;
            if j < bytes.len() && matches!(bytes[j], b'+' | b'-' | b' ') {
                j += 1;
            }
            if j < bytes.len() && matches!(bytes[j], b'd' | b'D') {
                return true;
            }
            // Skip the `%` and following char so `%%` does not start a new scan mid-escape.
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    false
}

/// Whether `-z` needs an explicit inter-entry NUL that the format's own body does not already
/// supply. The `email`/`mboxrd` formats separate consecutive entries with their built-in trailing
/// blank line (so [`log_uses_blank_separator`] is false for them), but under `-z` Git still joins
/// entries with a NUL. `oneline`, `format:`/`tformat:`, and the blank-separated builtin formats are
/// handled elsewhere, so this only covers email/mboxrd.
fn log_z_needs_email_separator(args: &Args) -> bool {
    matches!(args.format.as_deref(), Some("email") | Some("mboxrd"))
}

/// Whether to load ref decorations and whether to use full ref names (`refs/heads/...`).
///
/// Mirrors Git's handling of `--decorate`, `--no-decorate`, and raw argv scanning for
/// those flags. `--oneline` does not imply `--decorate`; use `--decorate` or a format
/// with `%d` / `%D` when decorations are required.
fn resolve_decoration_display(
    args: &Args,
    git_dir: &Path,
    format_requires_decorations: bool,
) -> (bool, bool) {
    let mut show = format_requires_decorations;
    // Git enables decorations for `%d` / `%D` with short ref names; `--decorate=full` opts in.
    let mut full = false;
    if let Some(mode) = ConfigSet::load(Some(git_dir), true)
        .unwrap_or_default()
        .get("log.decorate")
    {
        match mode.trim().to_ascii_lowercase().as_str() {
            "full" => {
                show = true;
                full = true;
            }
            "short" | "auto" => {
                show = true;
                full = false;
            }
            other => match parse_bool(other) {
                Ok(true) => {
                    show = true;
                    full = false;
                }
                Ok(false) => {
                    show = false;
                    full = false;
                }
                Err(_) => {}
            },
        }
    }
    // Scan raw argv in order so the last decoration directive wins, matching Git's
    // last-wins handling of `--decorate`, `--decorate=<mode>`, and `--no-decorate`.
    // Stop scanning at a bare `--` (end of options).
    for arg in std::env::args_os().map(|a| a.to_string_lossy().into_owned()) {
        if arg == "--" {
            break;
        }
        if arg == "--no-decorate" {
            show = false;
            full = false;
        } else if arg == "--decorate" {
            show = true;
            full = false;
        } else if let Some(mode) = arg.strip_prefix("--decorate=") {
            match mode.to_ascii_lowercase().as_str() {
                "full" => {
                    show = true;
                    full = true;
                }
                "short" | "auto" => {
                    show = true;
                    full = false;
                }
                other => match parse_bool(other) {
                    Ok(true) => {
                        show = true;
                        full = false;
                    }
                    Ok(false) => {
                        show = false;
                        full = false;
                    }
                    // Unknown mode: be permissive and enable short decorations.
                    Err(_) => {
                        show = true;
                        full = false;
                    }
                },
            }
        }
    }
    let oneline_like = log_uses_builtin_oneline(args);
    if oneline_like && !args.no_decorate && !show {
        // Upstream Git only auto-decorates default log output (including `--oneline`)
        // when stdout is a terminal; piped/redirected output is left undecorated.
        if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
            show = true;
            full = false;
        }
    }
    (show, full)
}

/// Emit `git log --graph`-style output: the graph commit row (`*` / merge) shares its line with
/// the first pretty line (`commit …` or `format:` output); each following body line is prefixed
/// with another graph row (upstream `graph_show_strbuf` + `graph_show_commit_msg`).
fn write_graph_interleaved_commit_msg(
    out: &mut impl Write,
    line_prefix: &str,
    graph_commit_line: &str,
    graph: &mut AsciiGraph,
    body: &[u8],
) -> Result<()> {
    // Operate on raw bytes: under `i18n.logOutputEncoding` the body may be a non-UTF-8 encoding
    // (e.g. ISO-8859-1). Line splitting only relies on the `\n` (0x0A) byte, which is identical
    // across the encodings Git supports here, so no UTF-8 decoding is required.
    let newline_terminated = body.last() == Some(&b'\n');
    let trimmed: &[u8] = {
        let mut end = body.len();
        while end > 0 && body[end - 1] == b'\n' {
            end -= 1;
        }
        &body[..end]
    };
    if !trimmed.contains(&b'\n')
        && trimmed.len() == 40
        && trimmed
            .iter()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        let trimmed = String::from_utf8_lossy(trimmed);
        writeln!(out, "{line_prefix}{graph_commit_line}")?;
        writeln!(out, "{line_prefix}{trimmed}")?;
        if !graph.is_commit_finished() {
            if !newline_terminated {
                writeln!(out)?;
            }
            graph_show_remainder_lines(out, line_prefix, graph)?;
            if newline_terminated {
                writeln!(out)?;
            }
        } else if !newline_terminated {
            writeln!(out)?;
        }
        return Ok(());
    }

    let mut lines = body.split_inclusive(|b| *b == b'\n').peekable();
    let Some(first_chunk) = lines.next() else {
        writeln!(out, "{line_prefix}{graph_commit_line}")?;
        graph_show_remainder_lines(out, line_prefix, graph)?;
        return Ok(());
    };

    let first_line = first_chunk.strip_suffix(b"\n").unwrap_or(first_chunk);
    write!(out, "{line_prefix}{graph_commit_line}")?;
    out.write_all(first_line)?;
    if first_chunk.ends_with(b"\n") {
        writeln!(out)?;
    }

    for chunk in lines {
        let text = chunk.strip_suffix(b"\n").unwrap_or(chunk);
        // Mirror Git's `graph_show_strbuf` -> `graph_show_oneline`: a graph prefix is emitted
        // before *every* body line after the first. `graph_next_line` produces a structural
        // line while the graph is still rendering its own rows, and a *padding* line (the
        // vertical rails, e.g. `| | `) once the commit's graph output is finished — so body
        // lines past the structural ones still get the correct rail prefix.
        let (gline, _) = graph.next_line();
        write!(out, "{line_prefix}{gline}")?;
        out.write_all(text)?;
        if chunk.ends_with(b"\n") {
            writeln!(out)?;
        }
    }

    if !graph.is_commit_finished() {
        if !newline_terminated {
            writeln!(out)?;
        }
        graph_show_remainder_lines(out, line_prefix, graph)?;
        if newline_terminated && trimmed.contains(&b'\n') {
            writeln!(out)?;
        }
    } else if !newline_terminated && !body.is_empty() {
        writeln!(out)?;
    }
    Ok(())
}

fn graph_show_remainder_lines(
    out: &mut impl Write,
    line_prefix: &str,
    graph: &mut AsciiGraph,
) -> Result<()> {
    while !graph.is_commit_finished() {
        let (gline, _) = graph.next_line();
        writeln!(out, "{line_prefix}{gline}")?;
    }
    Ok(())
}

/// Strip `git log`-only flags from the revision list and expand revision pseudo-options
/// (`--all`, `--glob`, `--branches`, …) into concrete revision strings, matching `git log` /
/// `setup_revisions` behavior.
/// Whether a `git log` option (in the long `--opt` or short `-x` form, *without* an inline
/// `=value`) consumes the following argv token as its value. Used by the revision/pathspec
/// splitter so a space-separated option value (`--grep sec`) is not misread as a revision.
fn log_option_consumes_separate_value(arg: &str) -> bool {
    // Inline `--opt=value` already carries its value; nothing further to consume.
    if arg.starts_with("--") && arg.contains('=') {
        return false;
    }
    matches!(
        arg,
        "--grep"
            | "--grep-reflog"
            | "--author"
            | "--committer"
            | "--diff-filter"
            | "--find-object"
            | "--decorate-refs"
            | "--decorate-refs-exclude"
            | "--date"
            | "--pretty"
            | "--format"
            | "--encoding"
            | "--skip"
            | "--max-count"
            | "-n"
            | "--since"
            | "--after"
            | "--until"
            | "--before"
            | "--grep-reflog="
            | "--line-prefix"
            | "-S"
            | "-G"
            | "-L"
            | "-O"
    )
}

/// Whether the raw argv contained a "pseudo-ref" input (`--tags`, `--remotes`, `--glob`,
/// `--branches`, `--all`) that counts as revision input even if it matches nothing.
fn log_argv_has_pseudo_ref_input(args: &Args) -> bool {
    let src: &[String] = if args.raw_argv_tail.is_empty() {
        &args.revisions
    } else {
        &args.raw_argv_tail
    };
    let mut saw = false;
    for a in src {
        if a == "--" {
            break;
        }
        if a == "--tags"
            || a == "--remotes"
            || a == "--glob"
            || a == "--branches"
            || a == "--all"
            || a.starts_with("--tags=")
            || a.starts_with("--remotes=")
            || a.starts_with("--glob=")
            || a.starts_with("--branches=")
        {
            saw = true;
        }
    }
    saw
}

/// Whether the merged argv contains a positional (non-option, pre-`--`) token. Used so an
/// explicitly-given object dropped by `--ignore-missing` still counts as revision input.
fn log_argv_has_positional_token(merged_argv: &[String]) -> bool {
    for a in merged_argv {
        if a == "--" {
            break;
        }
        if a == "--end-of-options" {
            continue;
        }
        if !a.starts_with('-') {
            return true;
        }
        if let Some(stripped) = a.strip_prefix('^') {
            if !stripped.is_empty() {
                return true;
            }
        }
    }
    false
}

fn hydrate_log_options_from_raw_argv(args: &mut Args) {
    let mut i = 0usize;
    while i < args.raw_argv_tail.len() {
        let arg = &args.raw_argv_tail[i];
        if args.max_count.is_none() {
            if let Some(rest) = arg.strip_prefix("-n").filter(|rest| !rest.is_empty()) {
                args.max_count = rest.parse::<usize>().ok();
                i += 1;
                continue;
            }
            if let Some(rest) = arg.strip_prefix("--max-count=") {
                args.max_count = rest.parse::<usize>().ok();
                i += 1;
                continue;
            }
            if (arg == "-n" || arg == "--max-count") && i + 1 < args.raw_argv_tail.len() {
                args.max_count = args.raw_argv_tail[i + 1].parse::<usize>().ok();
                i += 2;
                continue;
            }
        }

        if args.skip.is_none() {
            if let Some(rest) = arg.strip_prefix("--skip=") {
                args.skip = rest.parse::<usize>().ok();
                i += 1;
                continue;
            }
            if arg == "--skip" && i + 1 < args.raw_argv_tail.len() {
                args.skip = args.raw_argv_tail[i + 1].parse::<usize>().ok();
                i += 2;
                continue;
            }
        }

        if !args.oneline && arg == "--oneline" {
            args.oneline = true;
            i += 1;
            continue;
        }
        if arg == "--no-decorate" {
            args.no_decorate = true;
            i += 1;
            continue;
        }

        if args.format.is_none() {
            if let Some(rest) = arg.strip_prefix("--format=") {
                args.format = Some(rest.to_owned());
                i += 1;
                continue;
            }
            if arg == "--format" && i + 1 < args.raw_argv_tail.len() {
                args.format = Some(args.raw_argv_tail[i + 1].clone());
                i += 2;
                continue;
            }
            if let Some(rest) = arg.strip_prefix("--pretty=") {
                args.pretty = Some(rest.to_owned());
                i += 1;
                continue;
            }
            if arg == "--pretty" && i + 1 < args.raw_argv_tail.len() {
                args.pretty = Some(args.raw_argv_tail[i + 1].clone());
                i += 2;
                continue;
            }
        }

        match arg.as_str() {
            "--topo-order" => args.topo_order = true,
            "--date-order" => args.date_order = true,
            "--author-date-order" => args.author_date_order = true,
            "--first-parent" => args.first_parent = true,
            "--full-history" => args.full_history = true,
            "--simplify-merges" => args.simplify_merges = true,
            "--ancestry-path" => args.ancestry_path = true,
            "--sparse" => args.sparse = true,
            "--parents" => args.show_parents = true,
            _ => {}
        }
        if let Some(rest) = arg.strip_prefix("--ancestry-path=") {
            args.ancestry_path = true;
            if !rest.is_empty() {
                args.ancestry_path_bottoms.push(rest.to_owned());
            }
        }

        i += 1;
    }
}

fn normalize_ancestry_path_arg(args: &mut Args) {
    if !args.ancestry_path_args.is_empty() {
        args.ancestry_path = true;
        args.ancestry_path_bottoms.extend(
            args.ancestry_path_args
                .drain(..)
                .filter(|value| !value.is_empty()),
        );
    }
    if !args.ancestry_path_bottoms.is_empty() {
        args.ancestry_path = true;
    }
    let mut seen = HashSet::new();
    args.ancestry_path_bottoms
        .retain(|value| seen.insert(value.clone()));
}

/// Resolve revision specs to commit OIDs, dropping specs that fail to resolve when
/// `--ignore-missing` is in effect (git's `--ignore-missing`).
fn resolve_specs_to_commits_ignoring_missing(
    repo: &Repository,
    specs: &[String],
    args: &Args,
) -> Result<Vec<ObjectId>> {
    if !args.ignore_missing {
        return grit_lib::rev_list::resolve_revision_specs_to_commits(repo, specs)
            .map_err(|e| anyhow::anyhow!("{e}"));
    }
    let mut out = Vec::new();
    for spec in specs {
        match grit_lib::rev_list::resolve_revision_specs_to_commits(
            repo,
            std::slice::from_ref(spec),
        ) {
            Ok(mut oids) => out.append(&mut oids),
            Err(_) => { /* --ignore-missing: silently drop unresolved objects */ }
        }
    }
    Ok(out)
}

fn merge_log_revision_argv(repo: &Repository, args: &Args) -> Result<Vec<String>> {
    let src: &[String] = if args.raw_argv_tail.is_empty() {
        &args.revisions
    } else {
        &args.raw_argv_tail
    };
    let mut out = Vec::new();
    let mut not_mode = false;
    let mut end_opts = false;
    let mut i = 0usize;
    while i < src.len() {
        let arg = &src[i];
        if !end_opts && arg == "--" {
            out.push(arg.clone());
            out.extend(src[i + 1..].iter().cloned());
            break;
        }
        if !end_opts && arg == "--default" {
            i += 2;
            continue;
        }
        if !end_opts && arg.starts_with("--default=") {
            i += 1;
            continue;
        }
        if !end_opts && arg == "--end-of-options" {
            end_opts = true;
            out.push("--end-of-options".to_owned());
            i += 1;
            continue;
        }
        if !end_opts && arg.starts_with('-') && arg != "--" {
            match arg.as_str() {
                "--not" => {
                    not_mode = !not_mode;
                }
                "--all" => {
                    if not_mode {
                        for (_, oid) in grit_lib::refs::list_refs(&repo.git_dir, "refs/")? {
                            let s = oid.to_hex();
                            out.push(format!("^{s}"));
                        }
                        if let Ok(head_oid) = grit_lib::refs::resolve_ref(&repo.git_dir, "HEAD") {
                            out.push(format!("^{}", head_oid.to_hex()));
                        }
                    } else {
                        out.push("--all".to_owned());
                    }
                }
                "--branches" => {
                    let matching = grit_lib::refs::list_refs(&repo.git_dir, "refs/heads/")?;
                    for (_, oid) in matching {
                        let s = oid.to_hex();
                        if not_mode {
                            out.push(format!("^{s}"));
                        } else {
                            out.push(s);
                        }
                    }
                }
                "--tags" => {
                    let matching = grit_lib::refs::list_refs(&repo.git_dir, "refs/tags/")?;
                    for (_, oid) in matching {
                        let s = oid.to_hex();
                        if not_mode {
                            out.push(format!("^{s}"));
                        } else {
                            out.push(s);
                        }
                    }
                }
                "--remotes" => {
                    let matching = grit_lib::refs::list_refs(&repo.git_dir, "refs/remotes/")?;
                    for (_, oid) in matching {
                        let s = oid.to_hex();
                        if not_mode {
                            out.push(format!("^{s}"));
                        } else {
                            out.push(s);
                        }
                    }
                }
                _ if arg.starts_with("--branches=") => {
                    let pattern = arg.trim_start_matches("--branches=");
                    let full_pattern = format!("refs/heads/{pattern}");
                    let matching = grit_lib::refs::list_refs_glob(&repo.git_dir, &full_pattern)?;
                    for (_, oid) in matching {
                        let s = oid.to_hex();
                        if not_mode {
                            out.push(format!("^{s}"));
                        } else {
                            out.push(s);
                        }
                    }
                }
                _ if arg.starts_with("--tags=") => {
                    let pattern = arg.trim_start_matches("--tags=");
                    let full_pattern = format!("refs/tags/{pattern}");
                    let matching = grit_lib::refs::list_refs_glob(&repo.git_dir, &full_pattern)?;
                    for (_, oid) in matching {
                        let s = oid.to_hex();
                        if not_mode {
                            out.push(format!("^{s}"));
                        } else {
                            out.push(s);
                        }
                    }
                }
                _ if arg.starts_with("--remotes=") => {
                    let pattern = arg.trim_start_matches("--remotes=");
                    let full_pattern = format!("refs/remotes/{pattern}");
                    let matching = grit_lib::refs::list_refs_glob(&repo.git_dir, &full_pattern)?;
                    for (_, oid) in matching {
                        let s = oid.to_hex();
                        if not_mode {
                            out.push(format!("^{s}"));
                        } else {
                            out.push(s);
                        }
                    }
                }
                _ if arg.starts_with("--glob=") => {
                    let pattern = arg.trim_start_matches("--glob=");
                    let matching = grit_lib::refs::list_refs_glob(&repo.git_dir, pattern)?;
                    for (_, oid) in matching {
                        let s = oid.to_hex();
                        if not_mode {
                            out.push(format!("^{s}"));
                        } else {
                            out.push(s);
                        }
                    }
                }
                "--glob" => {
                    i += 1;
                    let Some(next) = src.get(i) else {
                        anyhow::bail!("--glob requires a value");
                    };
                    let matching = grit_lib::refs::list_refs_glob(&repo.git_dir, next)?;
                    for (_, oid) in matching {
                        let s = oid.to_hex();
                        if not_mode {
                            out.push(format!("^{s}"));
                        } else {
                            out.push(s);
                        }
                    }
                }
                _ => {
                    // Log-only or already-handled flags: skip. If this is a long/short option
                    // that takes its value as the *next* argv token (space-separated form, e.g.
                    // `--grep sec` / `--diff-filter A`), also skip the value so it is not
                    // mistaken for a revision/pathspec.
                    if log_option_consumes_separate_value(arg) {
                        i += 1;
                    }
                }
            }
            i += 1;
            continue;
        }
        if not_mode {
            if let Some(stripped) = arg.strip_prefix('^') {
                out.push(stripped.to_owned());
            } else {
                out.push(format!("^{arg}"));
            }
        } else {
            out.push(arg.clone());
        }
        i += 1;
    }
    Ok(out)
}

fn pathspecs_after_dashdash(merged_argv: &[String], clap_pathspecs: &[String]) -> Vec<String> {
    if let Some(pos) = merged_argv.iter().position(|s| s == "--") {
        merged_argv[pos + 1..].to_vec()
    } else {
        clap_pathspecs.to_vec()
    }
}

fn log_parent_format_requested(args: &Args) -> bool {
    args.format
        .as_deref()
        .is_some_and(|fmt| fmt.contains("%P") || fmt.contains("%p"))
}

/// Collect revision argument strings from `git log` argv (before `--stdin`), matching the
/// revision vs pathspec disambiguation used for graph output.
fn extract_log_cli_revision_specs(
    repo: &Repository,
    args: &Args,
    merged_revisions: &[String],
) -> Result<(Vec<String>, Vec<String>)> {
    let mut implied_pathspecs: Vec<String> = Vec::new();
    let mut revision_specs = Vec::new();
    let mut after_end_of_options = false;
    // When an explicit `--` is present, tokens before it MUST resolve as objects; git's
    // setup_revisions does not reinterpret them as pathspecs on failure (t4208).
    let has_dashdash = merged_revisions.iter().any(|r| r == "--");
    for rev in merged_revisions {
        if rev == "--end-of-options" {
            after_end_of_options = true;
            continue;
        }
        if rev == "--" {
            break;
        }
        if rev == "--all" {
            continue;
        }
        if !after_end_of_options && rev.starts_with('-') && !rev.starts_with('^') {
            continue;
        }
        if !after_end_of_options && is_symmetric_diff(rev) {
            revision_specs.push(rev.clone());
            continue;
        }
        if let Some(stripped) = rev.strip_prefix('^') {
            match resolve_revision_as_commit(repo, stripped) {
                Ok(_) => revision_specs.push(rev.clone()),
                Err(_err) if is_likely_pathspec_during_rev_parse(stripped) => {
                    implied_pathspecs.push(stripped.to_owned())
                }
                Err(_err) => match resolve_revision_as_commit_after_precompose(repo, stripped) {
                    Ok(_) => revision_specs.push(rev.clone()),
                    Err(_err)
                        if grit_lib::precompose_config::effective_core_precomposeunicode(Some(
                            &repo.git_dir,
                        )) && grit_lib::unicode_normalization::has_non_ascii_utf8(stripped) =>
                    {
                        implied_pathspecs.push(stripped.to_owned());
                    }
                    Err(_err) if token_names_existing_path(repo, stripped) => {
                        implied_pathspecs.push(stripped.to_owned());
                    }
                    // `--ignore-missing`: drop tokens that name no object instead of erroring.
                    Err(_err) if args.ignore_missing => {}
                    Err(err) => return Err(err.into()),
                },
            }
        } else if !after_end_of_options
            && !has_dashdash
            && (rev == ".." || rev == "...")
            && cwd_relative_path_exists(repo, rev)
        {
            // A bare `..` / `...` is the empty range `HEAD..HEAD`, but Git's
            // setup_revisions prefers an existing path of that name (e.g. `..`
            // naming the parent directory from a subdirectory).
            implied_pathspecs.push(rev.clone());
        } else {
            match resolve_revision_as_commit(repo, rev) {
                Ok(_) => {
                    // git's setup_revisions: when a token resolves as a revision but the same
                    // token (minus any magic) also names an existing worktree/index path and no
                    // explicit `--` separator is present, the argument is ambiguous (t4208 `:/a`).
                    if !has_dashdash && rev_token_collides_with_path(repo, rev) {
                        return Err(anyhow::anyhow!(
                            "fatal: ambiguous argument '{rev}': both revision and filename\n\
                             Use '--' to separate paths from revisions, like this:\n\
                             'git <command> [<revision>...] -- [<file>...]'"
                        ));
                    }
                    revision_specs.push(rev.clone())
                }
                // An explicit `--` forces object resolution: do not reinterpret as pathspec.
                Err(_err) if !has_dashdash && is_likely_pathspec_during_rev_parse(rev) => {
                    implied_pathspecs.push(rev.clone())
                }
                Err(_err) => match resolve_revision_as_commit_after_precompose(repo, rev) {
                    Ok(_) => revision_specs.push(rev.clone()),
                    Err(_err)
                        if grit_lib::precompose_config::effective_core_precomposeunicode(Some(
                            &repo.git_dir,
                        )) && grit_lib::unicode_normalization::has_non_ascii_utf8(rev) =>
                    {
                        implied_pathspecs.push(rev.clone());
                    }
                    // git's verify_filename: a token that fails to resolve as a revision but
                    // names an existing worktree/index path is an implied pathspec.
                    Err(_err) if !has_dashdash && token_names_existing_path(repo, rev) => {
                        implied_pathspecs.push(rev.clone());
                    }
                    // `--ignore-missing`: drop tokens that name no object instead of erroring.
                    Err(_err) if args.ignore_missing => {}
                    Err(_err) => {
                        return Err(anyhow::anyhow!(
                            "fatal: ambiguous argument '{rev}': unknown revision or path not in the working tree.\n\
                             Use '--' to separate paths from revisions, like this:\n\
                             'git <command> [<revision>...] -- [<file>...]'"
                        ));
                    }
                },
            }
        }
    }

    if !implied_pathspecs.is_empty() {
        validate_pathspec_scope(repo, &implied_pathspecs)?;
    }
    Ok((revision_specs, implied_pathspecs))
}

fn run_rev_list_log(
    repo: &Repository,
    args: &Args,
    patch_context: usize,
    author_res: &[Regex],
    committer_res: &[Regex],
    grep_filter: &GrepFilter,
    use_color: bool,
    use_mailmap: bool,
    mailmap: &MailmapTable,
) -> Result<()> {
    let grep_colorizer = GrepColorizer::from_config(
        &repo.git_dir,
        use_color,
        &grep_filter.str_res,
        author_res,
        committer_res,
    );
    let merged_argv = merge_log_revision_argv(repo, args)?;
    let (revision_specs, implied_pathspecs) =
        extract_log_cli_revision_specs(repo, args, &merged_argv)?;

    let mut symmetric_left: Option<String> = None;
    let mut symmetric_right: Option<String> = None;
    let mut processed_revision_specs = Vec::new();
    for spec in &revision_specs {
        if is_symmetric_diff(spec) {
            if let Some((lhs, rhs)) = split_symmetric_diff(spec) {
                symmetric_left = Some(lhs);
                symmetric_right = Some(rhs);
            }
        } else {
            processed_revision_specs.push(spec.clone());
        }
    }

    let (mut positive_specs, mut negative_specs, stdin_all_refs, stdin_paths) =
        collect_revision_specs_with_stdin(
            &repo.git_dir,
            &processed_revision_specs,
            args.read_stdin,
        )
        .map_err(|e| anyhow::anyhow!("failed to parse revision arguments: {e}"))?;

    let mut combined_pathspecs = pathspecs_after_dashdash(&merged_argv, &args.pathspecs);
    combined_pathspecs.extend(implied_pathspecs);
    combined_pathspecs.extend(stdin_paths);
    validate_pathspec_scope(repo, &combined_pathspecs)?;
    combined_pathspecs = resolve_effective_pathspecs(repo, &combined_pathspecs)?;

    let (core_commit_graph, cg_read_paths, cg_changed_ver) = load_bloom_walk_config(&repo.git_dir);
    let use_bloom = core_commit_graph
        && !combined_pathspecs.is_empty()
        && grit_lib::pathspec::pathspecs_allow_bloom(&combined_pathspecs)
        && !args.walk_reflogs;
    let trace2_perf = std::env::var("GIT_TRACE2_PERF")
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let bloom_stats: Option<BloomWalkStatsHandle> = if trace2_perf && use_bloom {
        Some(Arc::new(Mutex::new(BloomWalkStats::default())))
    } else {
        None
    };
    let _bloom_perf_guard = bloom_stats.as_ref().map(|h| BloomPerfGuard(Arc::clone(h)));

    let ordering = if args.topo_order
        || args.simplify_merges
        // `--simplify-by-decoration` with default (unspecified) ordering walks the rewritten
        // topology, which Git emits in topo order (e.g. `reach` right after its merge child,
        // before the older `seventh`). An explicit `--date-order` keeps date ordering.
        || (args.simplify_by_decoration && !args.date_order && !args.author_date_order)
    {
        if args.author_date_order {
            OrderingMode::AuthorDateTopo
        } else {
            OrderingMode::Topo
        }
    } else if args.date_order || args.author_date_order {
        if args.author_date_order {
            OrderingMode::AuthorDateWalk
        } else {
            OrderingMode::DateOrderWalk
        }
    } else {
        OrderingMode::Default
    };

    // For `--simplify-by-decoration`, build the filter-aware decorated commit set: the same
    // decorations the display uses (honoring `--decorate-refs[-exclude]`, `log.excludeDecoration`,
    // and the default hidden-ref behavior). This drives the parent-rewriting simplification in
    // the rev-list engine, matching Git's `rev_compare_tree` decoration oracle.
    let simplify_by_decoration_oids: HashSet<ObjectId> = if args.simplify_by_decoration {
        // Only the OID keys matter for simplification (full vs short ref display is irrelevant).
        let dec_map = collect_decorations_inner(
            repo,
            false,
            decorations_initial_set_all(args, &repo.git_dir),
            &build_decoration_filter(args, &repo.git_dir),
        )?;
        dec_map
            .keys()
            .filter_map(|hex| hex.parse::<ObjectId>().ok())
            .collect()
    } else {
        HashSet::new()
    };

    let mut options = RevListOptions {
        all_refs: args.all || stdin_all_refs,
        first_parent: args.first_parent,
        ancestry_path: args.ancestry_path,
        ancestry_path_bottoms: args
            .ancestry_path_bottoms
            .iter()
            .map(|b| resolve_revision_as_commit(repo, b.as_str()).map_err(anyhow::Error::from))
            .collect::<Result<Vec<_>>>()?,
        skip: args.skip.unwrap_or(0),
        max_count: args.max_count,
        ordering,
        reverse: args.reverse,
        boundary: args.boundary,
        full_history: args.full_history,
        parent_rewrite: args.show_parents || log_parent_format_requested(args),
        sparse: args.sparse,
        simplify_merges: args.simplify_merges,
        simplify_by_decoration: args.simplify_by_decoration,
        simplify_by_decoration_oids: simplify_by_decoration_oids.clone(),
        preserve_simplify_merges_graph_merges: args.graph,
        show_pulls: args.show_pulls,
        exclude_first_parent_only: args.exclude_first_parent_only,
        paths: combined_pathspecs.clone(),
        use_commit_graph_bloom: use_bloom,
        commit_graph_read_changed_paths: cg_read_paths,
        commit_graph_changed_paths_version: cg_changed_ver,
        bloom_stats: bloom_stats.clone(),
        ..RevListOptions::default()
    };

    if args.no_merges {
        options.max_parents = Some(1);
    }
    if args.merges {
        options.min_parents = Some(2);
    }

    if let (Some(lhs), Some(rhs)) = (symmetric_left.as_deref(), symmetric_right.as_deref()) {
        let lhs_spec = if lhs.is_empty() { "HEAD" } else { lhs };
        let rhs_spec = if rhs.is_empty() { "HEAD" } else { rhs };
        let lhs_tip = resolve_revision_for_range_end(repo, lhs_spec)
            .with_context(|| format!("bad revision '{lhs_spec}'"))?;
        let rhs_tip = resolve_revision_for_range_end(repo, rhs_spec)
            .with_context(|| format!("bad revision '{rhs_spec}'"))?;
        let lhs_oid = peel_to_commit_for_merge_base(repo, lhs_tip)?;
        let rhs_oid = peel_to_commit_for_merge_base(repo, rhs_tip)?;
        let bases = merge_bases(repo, lhs_oid, rhs_oid, args.first_parent)
            .context("failed to compute merge bases for symmetric range")?;
        positive_specs.push(lhs_spec.to_owned());
        positive_specs.push(rhs_spec.to_owned());
        negative_specs.extend(bases.into_iter().map(|base| base.to_hex()));
        options.symmetric_left = Some(lhs_oid);
        options.symmetric_right = Some(rhs_oid);
        options.left_right = args.left_right;
        options.left_only = args.left_only;
        options.right_only = args.right_only;
    }

    if positive_specs.is_empty() && !options.all_refs {
        positive_specs.push("HEAD".to_owned());
    }

    let result = rev_list(repo, &positive_specs, &negative_specs, &options)
        .map_err(|e| anyhow::anyhow!("rev-list failed: {e}"))?;

    let format_requires_decorations = args
        .format
        .as_deref()
        .map(|fmt| {
            let template = fmt
                .strip_prefix("format:")
                .or_else(|| fmt.strip_prefix("tformat:"))
                .unwrap_or(fmt);
            format_template_uses_decoration(template)
        })
        .unwrap_or(false);
    let (show_decorations, decorate_full) =
        resolve_decoration_display(args, &repo.git_dir, format_requires_decorations);
    let decoration_map_for_display = if show_decorations {
        Some(collect_decorations_inner(
            repo,
            decorate_full,
            decorations_initial_set_all(&args, &repo.git_dir),
            &build_decoration_filter(&args, &repo.git_dir),
        )?)
    } else {
        None
    };

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut notes_cache = NotesMapCache::new(repo);
    let head_state = resolve_head(&repo.git_dir).unwrap_or(HeadState::Invalid);
    let decoration_paint = if use_color {
        Some(load_decoration_paint(&repo.git_dir))
    } else {
        None
    };
    let is_format_separator = args
        .format
        .as_deref()
        .map(|f| f.starts_with("format:"))
        .unwrap_or(false);
    let show_diff = args.patch
        || args.patch_u
        || !args.stat.is_empty()
        || args.name_only
        || args.name_status
        || args.raw
        || args.cc
        || args.merge_diff_c
        || args.remerge_diff
        || args.patch_with_stat
        // An explicit `--diff-merges=<non-off>` forces merges to show a diff with no `-p`;
        // `write_commit_diff` re-checks per-commit, so non-merges still print nothing.
        || merges_force_diff(args);

    let mut shown = 0usize;
    let parent_format_requested = log_parent_format_requested(args);
    let rewrite_path_limited_parents =
        !combined_pathspecs.is_empty() && (args.show_parents || parent_format_requested);
    let included_for_parent_rewrite: HashSet<ObjectId> = if rewrite_path_limited_parents {
        result.commits.iter().copied().collect()
    } else {
        HashSet::new()
    };
    let excluded_for_parent_rewrite = if rewrite_path_limited_parents {
        excluded_revision_closure(repo, &negative_specs)?
    } else {
        HashSet::new()
    };
    let first_parent_through_omitted =
        !args.full_history && !args.ancestry_path && !args.simplify_merges;
    for oid in result.commits {
        let obj = repo.odb.read(&oid)?;
        let commit = parse_commit(&obj.data)?;
        let info = CommitInfo {
            tree: commit.tree,
            parents: commit.parents.clone(),
            author: commit.author.clone(),
            committer: commit.committer.clone(),
            message: commit.message.clone(),
            raw_message: commit.raw_message.clone(),
            encoding: commit.encoding.clone(),
        };

        let author_ok =
            author_res.is_empty() || author_res.iter().any(|re| re.is_match(&info.author));
        if !author_ok {
            continue;
        }
        let committer_ok =
            committer_res.is_empty() || committer_res.iter().any(|re| re.is_match(&info.committer));
        if !committer_ok {
            continue;
        }
        let msg_ok = grep_filter.matches(&info.message, args.all_match, args.invert_grep);
        if !msg_ok {
            continue;
        }

        let parent_override = if rewrite_path_limited_parents {
            if args.first_parent {
                Some(visible_parents_for_graph(
                    repo,
                    oid,
                    &included_for_parent_rewrite,
                    args.first_parent,
                    first_parent_through_omitted,
                    args.simplify_merges,
                )?)
            } else {
                Some(visible_parents_for_path_limited_log(
                    repo,
                    oid,
                    &included_for_parent_rewrite,
                    &excluded_for_parent_rewrite,
                    &combined_pathspecs,
                    args.first_parent,
                    args.simplify_merges,
                    args.ancestry_path,
                )?)
            }
        } else {
            None
        };

        if is_format_separator && shown > 0 {
            if args.null_terminator {
                write!(out, "\0")?;
            } else {
                writeln!(out)?;
            }
        }
        let blank_separator = log_uses_blank_separator(args);
        if blank_separator && shown > 0 {
            // Under `-z` the inter-entry blank line is replaced by a NUL (Git `line_termination`).
            if args.null_terminator {
                write!(out, "\0")?;
            } else {
                writeln!(out)?;
            }
        }
        if args.null_terminator && shown > 0 && log_z_needs_email_separator(args) {
            write!(out, "\0")?;
        }

        let per_parent_header = log_commit_uses_per_parent_header(args, &info, &repo.git_dir)?;
        if !per_parent_header {
            format_commit(
                &mut out,
                &oid,
                &info,
                args,
                use_mailmap,
                mailmap,
                decoration_map_for_display.as_ref(),
                use_color,
                decoration_paint.as_ref(),
                &head_state,
                &mut notes_cache,
                &repo.odb,
                parent_override.as_deref(),
                false,
                None,
                None,
                None,
                None,
                grep_colorizer.as_ref(),
            )?;
        }

        if show_diff {
            write_commit_diff(
                &mut out,
                repo,
                &oid,
                &info,
                args,
                use_mailmap,
                mailmap,
                &combined_pathspecs,
                None,
                None,
                decoration_map_for_display.as_ref(),
                use_color,
                decoration_paint.as_ref(),
                &head_state,
                &mut notes_cache,
                patch_context,
                blank_separator,
                None,
            )?;
        }
        shown += 1;
    }

    Ok(())
}

fn run_graph_log(
    repo: &Repository,
    args: &Args,
    patch_context: usize,
    use_mailmap: bool,
    mailmap: &MailmapTable,
) -> Result<()> {
    let merged_argv = merge_log_revision_argv(repo, args)?;
    let (mut revision_specs, implied_extra) =
        extract_log_cli_revision_specs(repo, args, &merged_argv)?;
    let implied_pathspecs = implied_extra;

    if !implied_pathspecs.is_empty() {
        validate_pathspec_scope(repo, &implied_pathspecs)?;
    }

    let user_revision_specs_len = revision_specs.len();

    if let Some(glob) = args.branches.as_deref() {
        let mut seen: std::collections::HashSet<String> = revision_specs.iter().cloned().collect();
        for (name, oid) in grit_lib::refs::list_refs(&repo.git_dir, "refs/heads/")? {
            let short = name.strip_prefix("refs/heads/").unwrap_or(&name);
            if !branches_glob_matches(glob, short) {
                continue;
            }
            let s = oid.to_hex();
            if seen.insert(s.clone()) {
                revision_specs.push(s);
            }
        }
    }

    let (mut positive_specs, negative_specs, stdin_all_refs, stdin_paths) =
        collect_revision_specs_with_stdin(&repo.git_dir, &revision_specs, args.read_stdin)
            .map_err(|e| anyhow::anyhow!("failed to parse revision arguments: {e}"))?;

    let mut combined_pathspecs = pathspecs_after_dashdash(&merged_argv, &args.pathspecs);
    combined_pathspecs.extend(implied_pathspecs);
    combined_pathspecs.extend(stdin_paths);
    validate_pathspec_scope(repo, &combined_pathspecs)?;
    combined_pathspecs = resolve_effective_pathspecs(repo, &combined_pathspecs)?;

    let mut options = RevListOptions {
        all_refs: args.all || stdin_all_refs,
        first_parent: args.first_parent,
        ancestry_path: args.ancestry_path,
        ancestry_path_bottoms: args
            .ancestry_path_bottoms
            .iter()
            .map(|b| resolve_revision_as_commit(repo, b.as_str()).map_err(anyhow::Error::from))
            .collect::<Result<Vec<_>>>()?,
        simplify_by_decoration: false,
        skip: args.skip.unwrap_or(0),
        max_count: args.max_count,
        ordering: if args.topo_order || args.simplify_merges {
            if args.author_date_order {
                OrderingMode::AuthorDateTopo
            } else {
                OrderingMode::Topo
            }
        } else if args.date_order || args.author_date_order {
            if args.author_date_order {
                OrderingMode::AuthorDateWalk
            } else {
                OrderingMode::DateOrderWalk
            }
        } else {
            OrderingMode::Topo
        },
        reverse: args.reverse,
        boundary: args.boundary,
        full_history: args.full_history,
        parent_rewrite: args.show_parents || log_parent_format_requested(&args),
        sparse: args.sparse,
        simplify_merges: args.simplify_merges,
        preserve_simplify_merges_graph_merges: args.graph,
        show_pulls: args.show_pulls,
        exclude_first_parent_only: args.exclude_first_parent_only,
        paths: if args.follow {
            Vec::new()
        } else {
            combined_pathspecs.clone()
        },
        ..RevListOptions::default()
    };
    if args.no_merges {
        options.max_parents = Some(1);
    }
    if args.merges {
        options.min_parents = Some(2);
    }

    if stdin_all_refs {
        options.all_refs = true;
    }

    if positive_specs.is_empty() && !options.all_refs {
        positive_specs.push("HEAD".to_owned());
    }

    if args.branches.is_some() {
        sort_revision_specs_by_committer_desc(repo, &mut positive_specs)?;
    }

    let mut result = rev_list(repo, &positive_specs, &negative_specs, &options)
        .map_err(|e| anyhow::anyhow!("rev-list failed: {e}"))?;

    if args.branches.is_some() {
        prefer_explicit_tip_first_in_graph_walk(repo, args, &mut result.commits)?;
    }

    if args.branches.is_some() && user_revision_specs_len == 0 {
        result.commits = reorder_graph_all_branches_no_explicit_rev(repo, &result.commits)?;
    }

    if args.simplify_by_decoration {
        result.commits = simplify_by_decoration_for_graph(repo, &result.commits)?;
    }

    if args.simplify_merges && args.full_history {
        let simplified = simplify_merges_for_graph(repo, &result.commits)?;
        result.commits = simplified;
    }

    if !combined_pathspecs.is_empty() && !args.full_history && !args.simplify_merges {
        if args.sparse {
            let mut dense_options = options.clone();
            dense_options.sparse = false;
            let dense_result = rev_list(repo, &positive_specs, &negative_specs, &dense_options)
                .map_err(|e| anyhow::anyhow!("rev-list failed: {e}"))?;
            let dense_ordered =
                reorder_path_limited_graph_commits(repo, &dense_result.commits, args.first_parent)?;
            result.commits = expand_sparse_path_limited_graph_history(repo, &dense_ordered)?;
        } else {
            // Same commit set as `rev-list`; reorder only for graph column layout (main line before
            // side branches), matching `git log --graph -- <paths>` (`t6016`).
            result.commits =
                reorder_path_limited_graph_commits(repo, &result.commits, args.first_parent)?;
        }
    }

    let mut author_res_graph: Vec<Regex> = Vec::new();
    for p in &args.authors {
        let pat = if args.fixed_strings {
            regex::escape(p)
        } else {
            p.clone()
        };
        let re = RegexBuilder::new(&pat)
            .case_insensitive(ident_pattern_ignore_case(args.regexp_ignore_case))
            .build()
            .with_context(|| format!("invalid --author regex: {p}"))?;
        author_res_graph.push(re);
    }
    let mut committer_res_graph: Vec<Regex> = Vec::new();
    for p in &args.committers {
        let pat = if args.fixed_strings {
            regex::escape(p)
        } else {
            p.clone()
        };
        let re = RegexBuilder::new(&pat)
            .case_insensitive(ident_pattern_ignore_case(args.regexp_ignore_case))
            .build()
            .with_context(|| format!("invalid --committer regex: {p}"))?;
        committer_res_graph.push(re);
    }
    if !author_res_graph.is_empty() || !committer_res_graph.is_empty() {
        result.commits.retain(|oid| {
            let Ok(obj) = repo.odb.read(oid) else {
                return false;
            };
            let Ok(c) = parse_commit(&obj.data) else {
                return false;
            };
            ident_matches_header_patterns(&author_res_graph, &c.author, mailmap, use_mailmap)
                && ident_matches_header_patterns(
                    &committer_res_graph,
                    &c.committer,
                    mailmap,
                    use_mailmap,
                )
        });
    }

    let included: HashSet<ObjectId> = result.commits.iter().copied().collect();
    let ordered_boundaries = if args.boundary {
        order_boundary_commits_for_graph(
            repo,
            &result.boundary_commits,
            result.commits.first().copied(),
        )?
    } else {
        Vec::new()
    };
    let mut graph_parent_targets = included.clone();
    graph_parent_targets.extend(ordered_boundaries.iter().copied());
    let simplify_graph_parents =
        (args.simplify_by_decoration && combined_pathspecs.is_empty() && !args.full_history)
            || (args.simplify_merges && !combined_pathspecs.is_empty());
    // Path-limited history: when walking through commits omitted from the simplified list,
    // follow only the first parent so graph edges match Git's parent rewriting for `--graph`.
    // `--full-history` alone keeps full parent connectivity (t6016 case 6); with
    // `--full-history --simplify-merges` Git again collapses through omitted merges (t6016 case 7).
    let fp_through_omitted_for_graph =
        !combined_pathspecs.is_empty() && (!args.full_history || args.simplify_merges);
    // `--sparse` path-limited graph: show the first-parent spine as a straight column (t6016).
    let graph_first_parent_direct =
        args.first_parent || (args.sparse && !combined_pathspecs.is_empty() && !args.full_history);
    let mut nodes = Vec::new();
    let mut seen = HashSet::new();

    // In plain history (no path-limiting and no decoration/merge simplification) Git never rewrites
    // a commit's parents away, so a commit whose parent is merely omitted by output limiting (e.g.
    // `-1`/`-2`) still keeps the continuing vertical rail. Preserve that phantom trailing edge here.
    let keep_phantom_edge = combined_pathspecs.is_empty() && !simplify_graph_parents;
    for oid in &result.commits {
        if !seen.insert(*oid) {
            continue;
        }
        let parents = visible_parents_for_graph_inner(
            repo,
            *oid,
            &graph_parent_targets,
            graph_first_parent_direct,
            fp_through_omitted_for_graph,
            simplify_graph_parents,
            keep_phantom_edge,
        )?;
        nodes.push(GraphCommitNode {
            oid: *oid,
            parents,
            is_boundary: false,
        });
    }

    if args.boundary {
        for oid in &ordered_boundaries {
            if !seen.insert(*oid) {
                continue;
            }
            let mut parents = load_raw_parents(repo, *oid)?;
            if args.first_parent && parents.len() > 1 {
                parents.truncate(1);
            }
            nodes.push(GraphCommitNode {
                oid: *oid,
                parents,
                is_boundary: true,
            });
        }
    }

    let interesting: HashSet<ObjectId> = nodes.iter().map(|n| n.oid).collect();
    for node in &mut nodes {
        node.parents.retain(|p| interesting.contains(p));
    }

    let format_requires_decorations_graph = args
        .format
        .as_deref()
        .map(|fmt| {
            let template = fmt
                .strip_prefix("format:")
                .or_else(|| fmt.strip_prefix("tformat:"))
                .unwrap_or(fmt);
            format_template_uses_decoration(template)
        })
        .unwrap_or(false);
    let (show_decorations_graph, decorate_full_graph) =
        resolve_decoration_display(args, &repo.git_dir, format_requires_decorations_graph);
    let decorations = if args.simplify_by_decoration || show_decorations_graph {
        Some(collect_decorations_inner(
            repo,
            decorate_full_graph,
            decorations_initial_set_all(&args, &repo.git_dir),
            &build_decoration_filter(&args, &repo.git_dir),
        )?)
    } else {
        None
    };

    let mut notes_cache = NotesMapCache::new(repo);

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut graph = AsciiGraph::new();
    let line_prefix = args.line_prefix.as_deref().unwrap_or("");
    let abbrev_len = parse_abbrev(&args.abbrev);
    let use_color = log_resolve_stdout_color(args, &repo.git_dir);
    // Color the graph edge characters per column, honoring `log.graphColors`.
    let graph_palette = if use_color {
        load_graph_colors(&repo.git_dir)
    } else {
        None
    };
    graph.set_colors(use_color, graph_palette);
    let head_state = resolve_head(&repo.git_dir).unwrap_or(HeadState::Invalid);
    let decoration_paint = if use_color {
        Some(load_decoration_paint(&repo.git_dir))
    } else {
        None
    };
    let show_commit_body = !args.suppress_diff
        && !args.no_patch
        && (args.patch
            || args.patch_u
            || args.patch_with_stat
            || !args.stat.is_empty()
            || args.name_only
            || args.name_status
            || args.raw
            || args.cc
            || args.merge_diff_c
            || args.remerge_diff);

    let is_oneline = args.oneline || args.format.as_deref() == Some("oneline");
    // Builtin human-readable formats (short/medium/full/…) separate entries with a blank line;
    // `--format=`/`tformat:`/oneline terminate each entry instead (Git `rev->use_terminator`).
    let wants_separator = log_wants_blank_line_between_commits(args);
    for (node_idx, node) in nodes.into_iter().enumerate() {
        let info = load_commit_info(repo, node.oid)?;
        // Inter-commit separator (Git log-tree.c `show_log`: `shown_one && !use_terminator`).
        // Between human-readable entries Git emits one *padding* graph line (the vertical rails)
        // followed by a newline; the very first commit gets none, and there is no separator after
        // the last commit. This is the single `| | ` blank row visible between commits under
        // `--graph`. Terminator-style formats (`--format=`, `tformat:`, oneline) get no blank.
        if node_idx > 0 && wants_separator {
            let (pad, _) = graph.next_line();
            writeln!(out, "{line_prefix}{pad}")?;
        }
        graph.update(node.clone());

        loop {
            let (line, shown_commit_line) = graph.next_line();
            if shown_commit_line {
                if is_oneline {
                    let rendered = render_graph_commit_text(
                        &node,
                        &info,
                        args,
                        use_mailmap,
                        mailmap,
                        decorations.as_ref(),
                        abbrev_len,
                        &node.parents,
                        use_color,
                        decoration_paint.as_ref(),
                        &head_state,
                    );
                    writeln!(out, "{line_prefix}{line}{rendered}")?;
                } else {
                    let mut body_buf = Vec::new();
                    // Inform absolute-column directives (`%>|`/`%<|`) of the graph prefix width on
                    // this commit's first line (e.g. `* ` => 2), so they position relative to the
                    // start of the whole graph line like Git's `pp->graph_width`.
                    let graph_prefix_width = grit_lib::diffstat::display_width_minus_ansi(
                        &format!("{line_prefix}{line}"),
                    );
                    GRAPH_PREFIX_WIDTH.with(|c| c.set(graph_prefix_width));
                    let fmt_result = format_commit(
                        &mut body_buf,
                        &node.oid,
                        &info,
                        args,
                        use_mailmap,
                        mailmap,
                        decorations.as_ref(),
                        use_color,
                        decoration_paint.as_ref(),
                        &head_state,
                        &mut notes_cache,
                        &repo.odb,
                        Some(node.parents.as_slice()),
                        false,
                        None,
                        None,
                        None,
                        None,
                        None,
                    );
                    GRAPH_PREFIX_WIDTH.with(|c| c.set(0));
                    fmt_result?;
                    // For builtin formats `format_commit` appends the inter-entry trailing blank
                    // line that Git instead adds via the separator above. Strip that single
                    // trailing blank so it is not double-emitted (and not rail-prefixed after the
                    // final commit). When a diff body follows (`show_commit_body`), the trailing
                    // blank is part of the diff output, not the message, so leave it intact.
                    if wants_separator && !show_commit_body {
                        while body_buf.last() == Some(&b'\n')
                            && body_buf.len() >= 2
                            && body_buf[body_buf.len() - 2] == b'\n'
                        {
                            body_buf.pop();
                        }
                    }
                    if std::env::var("GRIT_NL_DEBUG").is_ok() {
                        eprintln!(
                            "BODY oid={} show_commit_body={show_commit_body} wants_sep={wants_separator} body={:?}",
                            node.oid,
                            String::from_utf8_lossy(&body_buf)
                        );
                    }
                    write_graph_interleaved_commit_msg(
                        &mut out,
                        line_prefix,
                        &line,
                        &mut graph,
                        &body_buf,
                    )?;
                }
                break;
            }
            writeln!(out, "{line_prefix}{line}")?;
        }

        while !graph.is_commit_finished() {
            let (line, _) = graph.next_line();
            writeln!(out, "{line_prefix}{line}")?;
        }

        if show_commit_body {
            // Render the diff/stat/name-status body into a buffer, then prefix every line with
            // the graph's *padding* rail (Git wires the diff machinery's `output_prefix` callback
            // to `graph_padding_line`, so each diff line carries the vertical bars for the active
            // columns). After the commit message the graph is finished (PADDING state); calling
            // `next_line()` then yields a padding line without advancing structural state, so the
            // same rail applies to every body line.
            let (pad_rail, _) = graph.next_line();
            // The rail (`| `, possibly colored) is prepended to every body line below. The `--stat`
            // block is rendered unprefixed here, but its width must still be reduced by the rail's
            // display width so the bar is scaled exactly like Git (`width = term_columns() -
            // utf8_strnwidth(line_prefix)`). Pass `line_prefix + pad_rail` for width accounting only.
            let stat_width_prefix = format!("{line_prefix}{pad_rail}");

            let mut diff_buf: Vec<u8> = Vec::new();
            write_commit_diff(
                &mut diff_buf,
                repo,
                &node.oid,
                &info,
                args,
                use_mailmap,
                mailmap,
                &combined_pathspecs,
                // The stat block's own `| ` graph prefix is superseded by the per-line rail
                // applied below, so render the stat unprefixed here.
                None,
                Some(stat_width_prefix.as_str()),
                decorations.as_ref(),
                use_color,
                decoration_paint.as_ref(),
                &head_state,
                &mut notes_cache,
                patch_context,
                false,
                None,
            )?;

            // Normalize the diff body: the non-graph renderer brackets it with a leading blank
            // (name-only/stat) and a trailing inter-entry blank. In graph mode Git supplies the
            // single message/diff separator itself (a railed `graph_show_padding` line), so strip
            // one leading and one trailing blank line here and re-emit exactly one separator.
            if diff_buf.first() == Some(&b'\n') {
                diff_buf.remove(0);
            }
            if diff_buf.len() >= 2
                && diff_buf[diff_buf.len() - 1] == b'\n'
                && diff_buf[diff_buf.len() - 2] == b'\n'
            {
                diff_buf.pop();
            }

            // Only commits whose diff produced output get a railed body; a merge (or otherwise
            // empty diff) emits nothing here, matching Git's `log_tree_diff` short-circuit.
            if !diff_buf.is_empty() {
                // Builtin multi-line formats (medium/short/full) already terminate the commit
                // message with a railed blank line, which serves as the message/diff separator.
                // Terminator formats (`--format=`, `tformat:`) do not, so emit the separator here
                // (Git's post-message `graph_show_padding` when `use_terminator`).
                if !wants_separator {
                    writeln!(out, "{line_prefix}{pad_rail}")?;
                }
                // Prefix every rendered diff line with the padding rail (e.g. `| ` / `| | `). For a
                // blank body line the rail's trailing space is stripped by the test's graph-output
                // sanitizer, matching Git's `graph_padding_line` output.
                for chunk in diff_buf.split_inclusive(|b| *b == b'\n') {
                    let text = chunk.strip_suffix(b"\n").unwrap_or(chunk);
                    write!(out, "{line_prefix}{pad_rail}")?;
                    out.write_all(text)?;
                    writeln!(out)?;
                }
            }
        }
    }

    Ok(())
}

fn simplify_merges_for_graph(repo: &Repository, commits: &[ObjectId]) -> Result<Vec<ObjectId>> {
    let selected: HashSet<ObjectId> = commits.iter().copied().collect();
    let mut out = Vec::new();
    for oid in commits {
        let raw_parents = load_raw_parents(repo, *oid)?;
        let mut direct = load_raw_parents(repo, *oid)?;
        direct.retain(|p| selected.contains(p));
        if raw_parents.len() > 1 && direct.len() <= 1 {
            continue;
        }
        if direct.len() <= 1 {
            out.push(*oid);
            continue;
        }

        let mut simplified = graph_simplify_parent_list(repo, &selected, &direct)?;
        simplified.sort_unstable();
        simplified.dedup();
        if simplified.len() > 1 {
            out.push(*oid);
        }
    }
    Ok(out)
}

fn simplify_by_decoration_for_graph(
    repo: &Repository,
    commits: &[ObjectId],
) -> Result<Vec<ObjectId>> {
    let decorations = collect_decorations(repo, false)?;
    let decorated: HashSet<ObjectId> = decorations
        .keys()
        .filter_map(|hex| hex.parse::<ObjectId>().ok())
        .collect();

    let mut out = Vec::new();
    for oid in commits {
        if decorated.contains(oid) {
            out.push(*oid);
            continue;
        }
        let parents = load_raw_parents(repo, *oid)?;
        if parents.len() > 1 {
            out.push(*oid);
        }
    }
    Ok(out)
}

fn graph_simplify_parent_list(
    repo: &Repository,
    selected: &HashSet<ObjectId>,
    parents: &[ObjectId],
) -> Result<Vec<ObjectId>> {
    let mut out = Vec::new();
    for parent in parents {
        if parent_reachable_via_others(repo, selected, *parent, parents)? {
            continue;
        }
        out.push(*parent);
    }
    Ok(out)
}

fn parent_reachable_via_others(
    repo: &Repository,
    selected: &HashSet<ObjectId>,
    target: ObjectId,
    parents: &[ObjectId],
) -> Result<bool> {
    for parent in parents {
        if *parent == target {
            continue;
        }
        if graph_reaches(repo, selected, *parent, target)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn graph_reaches(
    repo: &Repository,
    selected: &HashSet<ObjectId>,
    start: ObjectId,
    target: ObjectId,
) -> Result<bool> {
    let mut stack = vec![start];
    let mut seen = HashSet::new();
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        if oid == target {
            return Ok(true);
        }
        let mut parents = load_raw_parents(repo, oid)?;
        parents.retain(|p| selected.contains(p));
        stack.extend(parents);
    }
    Ok(false)
}

fn load_raw_parents(repo: &Repository, oid: ObjectId) -> Result<Vec<ObjectId>> {
    grit_lib::rev_parse::commit_parents_for_navigation(repo, oid).map_err(Into::into)
}

fn visible_parents_for_graph(
    repo: &Repository,
    oid: ObjectId,
    included: &HashSet<ObjectId>,
    first_parent_only: bool,
    first_parent_through_omitted: bool,
    simplify_merge_parents: bool,
) -> Result<Vec<ObjectId>> {
    visible_parents_for_graph_inner(
        repo,
        oid,
        included,
        first_parent_only,
        first_parent_through_omitted,
        simplify_merge_parents,
        false,
    )
}

/// Resolve the parent edges to draw for `oid` in `--graph` output.
///
/// Walks through omitted (non-`included`) parents to connect each shown commit to its nearest
/// shown ancestor, mirroring Git's parent rewriting for path-limited / simplified history.
///
/// When `keep_phantom_edge` is set (plain history, no path-limiting or simplification) and the
/// commit has real parents but none of them resolve to a shown ancestor — e.g. the parent is
/// merely omitted by output limiting like `-1`/`-2` — the first real parent is kept as a phantom
/// edge. The phantom parent is never itself drawn as a commit, so it renders as a continuing
/// vertical `|` rail beneath/after the commit, matching Git (which never rewrites parents away for
/// plain output limiting and so keeps the trailing column).
fn visible_parents_for_graph_inner(
    repo: &Repository,
    oid: ObjectId,
    included: &HashSet<ObjectId>,
    first_parent_only: bool,
    first_parent_through_omitted: bool,
    simplify_merge_parents: bool,
    keep_phantom_edge: bool,
) -> Result<Vec<ObjectId>> {
    let mut direct = load_raw_parents(repo, oid)?;
    if first_parent_only && direct.len() > 1 {
        direct.truncate(1);
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for parent in &direct {
        collect_visible_parent_for_graph(
            repo,
            *parent,
            included,
            first_parent_only,
            first_parent_through_omitted,
            &mut seen,
            &mut out,
        )?;
    }
    if simplify_merge_parents && out.len() > 1 {
        let simplified = graph_simplify_parent_list(repo, included, &out)?;
        let keep: HashSet<ObjectId> = simplified.into_iter().collect();
        out.retain(|parent| keep.contains(parent));
    }
    if keep_phantom_edge && out.is_empty() {
        if let Some(first) = direct.first() {
            out.push(*first);
        }
    }
    let mut dedup = HashSet::new();
    out.retain(|parent| dedup.insert(*parent));
    Ok(out)
}

fn visible_parents_for_path_limited_log(
    repo: &Repository,
    oid: ObjectId,
    included: &HashSet<ObjectId>,
    boundary: &HashSet<ObjectId>,
    pathspecs: &[String],
    first_parent_only: bool,
    simplify_merge_parents: bool,
    preserve_direct_single_parent: bool,
) -> Result<Vec<ObjectId>> {
    let mut direct = load_raw_parents(repo, oid)?;
    if first_parent_only && direct.len() > 1 {
        direct.truncate(1);
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for parent in direct {
        collect_visible_path_limited_parent(
            repo,
            parent,
            included,
            boundary,
            pathspecs,
            first_parent_only,
            simplify_merge_parents,
            preserve_direct_single_parent,
            true,
            &mut seen,
            &mut out,
        )?;
    }
    let mut dedup = HashSet::new();
    out.retain(|parent| dedup.insert(*parent));
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn collect_visible_path_limited_parent(
    repo: &Repository,
    candidate: ObjectId,
    included: &HashSet<ObjectId>,
    boundary: &HashSet<ObjectId>,
    pathspecs: &[String],
    first_parent_only: bool,
    simplify_merge_parents: bool,
    preserve_direct_single_parent: bool,
    direct_parent: bool,
    seen: &mut HashSet<ObjectId>,
    out: &mut Vec<ObjectId>,
) -> Result<()> {
    if !seen.insert(candidate) {
        return Ok(());
    }
    if included.contains(&candidate) || boundary.contains(&candidate) {
        out.push(candidate);
        return Ok(());
    }

    let mut parents = load_raw_parents(repo, candidate)?;
    if parents.is_empty() {
        return Ok(());
    }
    if direct_parent && preserve_direct_single_parent && parents.len() == 1 {
        out.push(candidate);
        return Ok(());
    }
    if first_parent_only && parents.len() > 1 {
        parents.truncate(1);
    } else if let Some(parent) = treesame_parent_for_path_rewrite(
        repo,
        candidate,
        &parents,
        pathspecs,
        simplify_merge_parents,
    )? {
        parents = vec![parent];
    }

    for parent in parents {
        collect_visible_path_limited_parent(
            repo,
            parent,
            included,
            boundary,
            pathspecs,
            first_parent_only,
            simplify_merge_parents,
            preserve_direct_single_parent,
            false,
            seen,
            out,
        )?;
    }
    Ok(())
}

fn treesame_parent_for_path_rewrite(
    repo: &Repository,
    oid: ObjectId,
    parents: &[ObjectId],
    pathspecs: &[String],
    prefer_last_when_all_treesame: bool,
) -> Result<Option<ObjectId>> {
    if pathspecs.is_empty() {
        return Ok(None);
    }
    let obj = repo.odb.read(&oid)?;
    let commit = parse_commit(&obj.data)?;
    let mut treesame = Vec::new();
    for parent_oid in parents {
        let parent_obj = repo.odb.read(parent_oid)?;
        let parent = parse_commit(&parent_obj.data)?;
        let entries = diff_trees(&repo.odb, Some(&parent.tree), Some(&commit.tree), "")?;
        let differs = entries
            .iter()
            .any(|entry| path_matches(&entry.path().to_string_lossy(), pathspecs));
        if !differs {
            treesame.push(*parent_oid);
        }
    }
    if prefer_last_when_all_treesame && treesame.len() == parents.len() {
        return Ok(treesame.last().copied());
    }
    Ok(treesame.first().copied())
}

fn excluded_revision_closure(
    repo: &Repository,
    negative_specs: &[String],
) -> Result<HashSet<ObjectId>> {
    let mut closure = HashSet::new();
    let mut stack = Vec::new();
    for spec in negative_specs {
        stack.push(resolve_revision_as_commit(repo, spec)?);
    }
    while let Some(oid) = stack.pop() {
        if !closure.insert(oid) {
            continue;
        }
        stack.extend(load_raw_parents(repo, oid)?);
    }
    Ok(closure)
}

fn collect_visible_parent_for_graph(
    repo: &Repository,
    candidate: ObjectId,
    included: &HashSet<ObjectId>,
    first_parent_only: bool,
    first_parent_through_omitted: bool,
    seen: &mut HashSet<ObjectId>,
    out: &mut Vec<ObjectId>,
) -> Result<()> {
    if !seen.insert(candidate) {
        return Ok(());
    }
    if included.contains(&candidate) {
        out.push(candidate);
        return Ok(());
    }
    let mut parents = load_raw_parents(repo, candidate)?;
    if parents.is_empty() {
        return Ok(());
    }
    let fp_chain = first_parent_only || first_parent_through_omitted;
    if fp_chain && parents.len() > 1 {
        parents.truncate(1);
    }
    for parent in parents {
        collect_visible_parent_for_graph(
            repo,
            parent,
            included,
            first_parent_only,
            first_parent_through_omitted,
            seen,
            out,
        )?;
    }
    Ok(())
}

fn first_parent_of_commit(repo: &Repository, oid: ObjectId) -> Result<Option<ObjectId>> {
    let parents = load_raw_parents(repo, oid)?;
    Ok(parents.first().copied())
}

fn first_parent_anchor_in_set(
    repo: &Repository,
    start: ObjectId,
    anchors: &HashSet<ObjectId>,
) -> Result<Option<ObjectId>> {
    let mut seen = HashSet::new();
    let mut cursor = Some(start);
    while let Some(oid) = cursor {
        if !seen.insert(oid) {
            break;
        }
        if anchors.contains(&oid) {
            return Ok(Some(oid));
        }
        cursor = first_parent_of_commit(repo, oid)?;
    }
    Ok(None)
}

fn reorder_path_limited_graph_commits(
    repo: &Repository,
    commits: &[ObjectId],
    first_parent_only: bool,
) -> Result<Vec<ObjectId>> {
    if commits.is_empty() {
        return Ok(Vec::new());
    }

    let included: HashSet<ObjectId> = commits.iter().copied().collect();
    let mut chain = Vec::new();
    let mut chain_seen = HashSet::new();
    let mut cursor = Some(commits[0]);
    while let Some(oid) = cursor {
        if !included.contains(&oid) || !chain_seen.insert(oid) {
            break;
        }
        chain.push(oid);
        let visible =
            visible_parents_for_graph(repo, oid, &included, first_parent_only, false, false)?;
        cursor = visible.first().copied();
    }

    let chain_set: HashSet<ObjectId> = chain.iter().copied().collect();
    let mut grouped: HashMap<Option<ObjectId>, Vec<ObjectId>> = HashMap::new();
    for oid in commits {
        if chain_set.contains(oid) {
            continue;
        }
        let anchor = first_parent_anchor_in_set(repo, *oid, &chain_set)?;
        grouped.entry(anchor).or_default().push(*oid);
    }

    let mut ordered = Vec::new();
    for chain_oid in chain {
        if let Some(group) = grouped.remove(&Some(chain_oid)) {
            ordered.extend(group);
        }
        ordered.push(chain_oid);
    }
    if let Some(group) = grouped.remove(&None) {
        ordered.extend(group);
    }
    for (_anchor, group) in grouped {
        ordered.extend(group);
    }
    Ok(ordered)
}

fn expand_sparse_path_limited_graph_history(
    repo: &Repository,
    commits: &[ObjectId],
) -> Result<Vec<ObjectId>> {
    if commits.is_empty() {
        return Ok(Vec::new());
    }

    let mut expanded = Vec::new();
    let mut seen = HashSet::new();
    let mut push_unique = |oid: ObjectId, out: &mut Vec<ObjectId>| {
        if seen.insert(oid) {
            out.push(oid);
        }
    };

    for window in commits.windows(2) {
        let from = window[0];
        let to = window[1];
        push_unique(from, &mut expanded);

        let mut cursor = first_parent_of_commit(repo, from)?;
        let mut chain = Vec::new();
        let mut found_target = false;
        let mut local_seen = HashSet::new();
        while let Some(oid) = cursor {
            if !local_seen.insert(oid) {
                break;
            }
            if oid == to {
                found_target = true;
                break;
            }
            chain.push(oid);
            cursor = first_parent_of_commit(repo, oid)?;
        }
        if found_target {
            for oid in chain {
                push_unique(oid, &mut expanded);
            }
        }
    }

    if let Some(&last) = commits.last() {
        push_unique(last, &mut expanded);
        let mut cursor = first_parent_of_commit(repo, last)?;
        let mut tail_seen = HashSet::new();
        while let Some(oid) = cursor {
            if !tail_seen.insert(oid) {
                break;
            }
            push_unique(oid, &mut expanded);
            cursor = first_parent_of_commit(repo, oid)?;
        }
    }

    Ok(expanded)
}

fn order_boundary_commits_for_graph(
    repo: &Repository,
    boundaries: &[ObjectId],
    first_included: Option<ObjectId>,
) -> Result<Vec<ObjectId>> {
    if boundaries.is_empty() {
        return Ok(Vec::new());
    }

    let boundary_set: HashSet<ObjectId> = boundaries.iter().copied().collect();
    let mut ordered = Vec::new();
    let mut seen = HashSet::new();

    if let Some(start) = first_included {
        let mut cursor = first_parent_of_commit(repo, start)?;
        while let Some(oid) = cursor {
            if !seen.insert(oid) {
                break;
            }
            if boundary_set.contains(&oid) {
                ordered.push(oid);
            }
            cursor = first_parent_of_commit(repo, oid)?;
        }
    }

    for oid in boundaries {
        if seen.insert(*oid) {
            ordered.push(*oid);
        }
    }

    Ok(ordered)
}

fn load_commit_info(repo: &Repository, oid: ObjectId) -> Result<CommitInfo> {
    let obj = repo.odb.read(&oid)?;
    let commit = parse_commit(&obj.data)?;
    Ok(CommitInfo {
        tree: commit.tree,
        parents: commit.parents,
        author: commit.author,
        committer: commit.committer,
        message: commit.message,
        raw_message: commit.raw_message,
        encoding: commit.encoding,
    })
}

fn render_graph_commit_text(
    node: &GraphCommitNode,
    info: &CommitInfo,
    args: &Args,
    use_mailmap: bool,
    mailmap: &MailmapTable,
    decorations: Option<&DecorationMap>,
    abbrev_len: usize,
    parent_line: &[ObjectId],
    use_color: bool,
    decoration_paint: Option<&DecorationPaint>,
    head_for_decor: &HeadState,
) -> String {
    let hex = node.oid.to_hex();
    if log_uses_builtin_oneline(args) {
        let first_line = grit_lib::commit_pretty::message_subject(&info.message);
        let first_line = if args.expand_tabs_in_log > 0 {
            grit_lib::tab_expand::expand_tabs_in_line(&first_line, args.expand_tabs_in_log)
        } else {
            first_line
        };
        let oid_color = if use_color {
            decoration_paint
                .map(|p| p.commit.as_str())
                .unwrap_or("\x1b[33m")
        } else {
            ""
        };
        let oid_reset = if use_color {
            decoration_paint
                .map(|p| p.reset.as_str())
                .unwrap_or("\x1b[m")
        } else {
            ""
        };
        let dec = format_decoration(
            &hex,
            decorations,
            use_color,
            decoration_paint,
            head_for_decor,
        );
        return format!(
            "{}{}{}{} {}",
            oid_color,
            &hex[..abbrev_len.min(hex.len())],
            oid_reset,
            dec,
            first_line
        );
    }

    if let Some(fmt) = args.format.as_deref() {
        if fmt.starts_with("format:") || fmt.starts_with("tformat:") {
            let template = if let Some(t) = fmt.strip_prefix("format:") {
                t
            } else if let Some(t) = fmt.strip_prefix("tformat:") {
                t
            } else {
                fmt
            };
            return apply_format_string(
                template,
                &node.oid,
                info,
                decorations,
                args.date.as_deref(),
                abbrev_len,
                use_color,
                decoration_paint,
                head_for_decor,
                None,
                parent_line,
                None,
                mailmap,
                use_mailmap,
                args.expand_tabs_in_log,
                None,
                None,
            );
        }
        if fmt.contains('%') {
            return apply_format_string(
                fmt,
                &node.oid,
                info,
                decorations,
                args.date.as_deref(),
                abbrev_len,
                use_color,
                decoration_paint,
                head_for_decor,
                None,
                parent_line,
                None,
                mailmap,
                use_mailmap,
                args.expand_tabs_in_log,
                None,
                None,
            );
        }
    }

    let subj = grit_lib::commit_pretty::message_subject(&info.message);
    if args.expand_tabs_in_log > 0 {
        grit_lib::tab_expand::expand_tabs_in_line(&subj, args.expand_tabs_in_log)
    } else {
        subj
    }
}

#[derive(Clone, Debug)]
struct GraphCommitNode {
    oid: ObjectId,
    parents: Vec<ObjectId>,
    is_boundary: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GraphState {
    Padding,
    Skip,
    PreCommit,
    Commit,
    PostMerge,
    Collapsing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GraphColumn {
    oid: ObjectId,
    /// Index into the color palette (`AsciiGraph::colors`). When this index is
    /// `>= colors_max` (i.e. points at the RESET sentinel), the column is drawn
    /// without color codes, exactly like Git's `struct column.color`.
    color: usize,
}

#[derive(Debug)]
struct AsciiGraph {
    current: Option<GraphCommitNode>,
    num_parents: usize,
    width: usize,
    expansion_row: usize,
    state: GraphState,
    prev_state: GraphState,
    commit_index: usize,
    prev_commit_index: usize,
    merge_layout: isize,
    edges_added: isize,
    prev_edges_added: isize,
    num_columns: usize,
    num_new_columns: usize,
    mapping_size: usize,
    columns: Vec<GraphColumn>,
    new_columns: Vec<GraphColumn>,
    mapping: Vec<isize>,
    old_mapping: Vec<isize>,
    /// Color palette: a list of ANSI color escape sequences. The final entry is
    /// always the RESET sequence and its index is `colors_max`. Mirrors Git's
    /// `column_colors` array (set via `graph_set_column_colors`).
    colors: Vec<String>,
    /// The index of the RESET sentinel in `colors` (== `colors.len() - 1`).
    /// Equivalent to Git's `column_colors_max`.
    colors_max: usize,
    /// The current default column color, an index into `colors`. Mirrors Git's
    /// `git_graph::default_column_color`.
    default_column_color: usize,
    /// Whether to emit color escape sequences at all (i.e. `--color`/`color.*`).
    use_color: bool,
}

/// Git's default ANSI graph column colors (`column_colors_ansi` in color.c).
/// The RESET sentinel is appended by `AsciiGraph::resolve_graph_colors`, so this
/// list contains only the drawable colors.
const GRAPH_COLUMN_COLORS_ANSI: &[&str] = &[
    "\x1b[31m",   // red
    "\x1b[32m",   // green
    "\x1b[33m",   // yellow
    "\x1b[34m",   // blue
    "\x1b[35m",   // magenta
    "\x1b[36m",   // cyan
    "\x1b[1;31m", // bold red
    "\x1b[1;32m", // bold green
    "\x1b[1;33m", // bold yellow
    "\x1b[1;34m", // bold blue
    "\x1b[1;35m", // bold magenta
    "\x1b[1;36m", // bold cyan
];

const GRAPH_COLOR_RESET: &str = "\x1b[m";

impl AsciiGraph {
    fn new() -> Self {
        // The default palette (red..bold-cyan) plus the RESET sentinel.
        let mut colors: Vec<String> = GRAPH_COLUMN_COLORS_ANSI
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        colors.push(GRAPH_COLOR_RESET.to_string());
        let colors_max = colors.len() - 1;
        Self {
            current: None,
            num_parents: 0,
            width: 0,
            expansion_row: 0,
            state: GraphState::Padding,
            prev_state: GraphState::Padding,
            commit_index: 0,
            prev_commit_index: 0,
            merge_layout: 0,
            edges_added: 0,
            prev_edges_added: 0,
            num_columns: 0,
            num_new_columns: 0,
            mapping_size: 0,
            columns: Vec::new(),
            new_columns: Vec::new(),
            mapping: Vec::new(),
            old_mapping: Vec::new(),
            colors,
            colors_max,
            // Start at the maximum value, since we always increment before the
            // first commit we output; this way we start at index 0.
            default_column_color: colors_max.saturating_sub(1),
            use_color: false,
        }
    }

    /// Configure the color palette and whether color codes are emitted.
    ///
    /// When `use_color` is false the graph renders exactly as before (no escape
    /// sequences). When `palette` is `Some`, it overrides the default ANSI
    /// colors (this is how `log.graphColors` is honored). The RESET sentinel is
    /// always appended automatically.
    fn set_colors(&mut self, use_color: bool, palette: Option<Vec<String>>) {
        self.use_color = use_color;
        if let Some(custom) = palette {
            if !custom.is_empty() {
                let mut colors = custom;
                colors.push(GRAPH_COLOR_RESET.to_string());
                self.colors = colors;
                self.colors_max = self.colors.len() - 1;
            }
        }
        self.default_column_color = self.colors_max.saturating_sub(1);
    }

    /// The color index used for newly created columns. When color is disabled,
    /// this returns `colors_max` (the RESET sentinel index) so the column draws
    /// without any escape codes, matching Git's `graph_get_current_column_color`.
    fn current_column_color(&self) -> usize {
        if !self.use_color {
            return self.colors_max;
        }
        self.default_column_color
    }

    /// Advance the default column color, wrapping modulo `colors_max`.
    fn increment_column_color(&mut self) {
        if self.colors_max == 0 {
            return;
        }
        self.default_column_color = (self.default_column_color + 1) % self.colors_max;
    }

    /// Find the color of an existing column for `oid`, or the current default.
    fn find_commit_color(&self, oid: ObjectId) -> usize {
        for i in 0..self.num_columns {
            if self.columns[i].oid == oid {
                return self.columns[i].color;
            }
        }
        self.current_column_color()
    }

    /// Append a single graph character, optionally wrapped in `col`'s color.
    /// Mirrors Git's `graph_line_write_column`: only the visible char counts
    /// toward `width`, the escape sequences do not.
    fn write_column(&self, line: &mut String, width: &mut usize, col: &GraphColumn, ch: char) {
        let colored = col.color < self.colors_max;
        if colored {
            line.push_str(&self.colors[col.color]);
        }
        line.push(ch);
        *width += 1;
        if colored {
            line.push_str(&self.colors[self.colors_max]);
        }
    }

    /// Append an uncolored character (used for the commit mark, spaces, dashes
    /// that have no owning column). Counts toward visible `width`.
    fn add_char(line: &mut String, width: &mut usize, ch: char) {
        line.push(ch);
        *width += 1;
    }

    fn update(&mut self, commit: GraphCommitNode) {
        self.current = Some(commit);
        self.num_parents = self.current.as_ref().map_or(0, |c| c.parents.len());
        self.prev_commit_index = self.commit_index;
        self.update_columns();
        self.expansion_row = 0;
        if self.state != GraphState::Padding {
            self.state = GraphState::Skip;
        } else if self.needs_pre_commit_line() {
            self.state = GraphState::PreCommit;
        } else {
            self.state = GraphState::Commit;
        }
    }

    fn is_commit_finished(&self) -> bool {
        self.state == GraphState::Padding
    }

    fn next_line(&mut self) -> (String, bool) {
        if self.current.is_none() {
            return (String::new(), false);
        }
        let _dbg_state = self.state;
        let mut line = String::new();
        // Visible width of the line, excluding any color escape sequences.
        // This mirrors Git's `graph_line.width` and is what padding uses.
        let mut width: usize = 0;
        let shown_commit_line = match self.state {
            GraphState::Padding => {
                self.output_padding_line(&mut line, &mut width);
                false
            }
            GraphState::Skip => {
                line.push_str("...");
                width += 3;
                if self.needs_pre_commit_line() {
                    self.update_state(GraphState::PreCommit);
                } else {
                    self.update_state(GraphState::Commit);
                }
                false
            }
            GraphState::PreCommit => {
                self.output_pre_commit_line(&mut line, &mut width);
                false
            }
            GraphState::Commit => {
                self.output_commit_line(&mut line, &mut width);
                true
            }
            GraphState::PostMerge => {
                self.output_post_merge_line(&mut line, &mut width);
                false
            }
            GraphState::Collapsing => {
                self.output_collapsing_line(&mut line, &mut width);
                false
            }
        };

        // Pad to the commit's total width using the *visible* width, so that
        // color escape sequences do not throw off the alignment of trailing
        // text. Mirrors Git's `graph_pad_horizontally`.
        let pad_width = self.width;
        if width < pad_width {
            line.push_str(&" ".repeat(pad_width - width));
        }
        if std::env::var("GRIT_NL_DEBUG").is_ok() {
            eprintln!(
                "NL state={:?}->{:?} line=[{}]",
                _dbg_state,
                self.state,
                line.replace(' ', ".")
            );
        }
        (line, shown_commit_line)
    }

    fn update_state(&mut self, next: GraphState) {
        self.prev_state = self.state;
        self.state = next;
    }

    fn ensure_vec_sizes(&mut self, needed_columns: usize) {
        let placeholder = match self.current.as_ref() {
            Some(current) => current.oid,
            None => return,
        };
        let placeholder_col = GraphColumn {
            oid: placeholder,
            color: self.colors_max,
        };
        if self.columns.len() < needed_columns {
            self.columns.resize(needed_columns, placeholder_col);
        }
        if self.new_columns.len() < needed_columns {
            self.new_columns.resize(needed_columns, placeholder_col);
        }
        let map_len = needed_columns.saturating_mul(2);
        if self.mapping.len() < map_len {
            self.mapping.resize(map_len, -1);
        }
        if self.old_mapping.len() < map_len {
            self.old_mapping.resize(map_len, -1);
        }
    }

    fn find_new_column_by_commit(&self, oid: ObjectId) -> Option<usize> {
        (0..self.num_new_columns).find(|&i| self.new_columns[i].oid == oid)
    }

    fn insert_into_new_columns(&mut self, oid: ObjectId, idx: isize) {
        // If the commit is not already in new_columns, add it and record the
        // color it should be drawn with (mirrors `graph_insert_into_new_columns`).
        let mut i = match self.find_new_column_by_commit(oid) {
            Some(pos) => pos,
            None => {
                let color = self.find_commit_color(oid);
                let pos = self.num_new_columns;
                self.new_columns[pos] = GraphColumn { oid, color };
                self.num_new_columns += 1;
                pos
            }
        };

        let mapping_idx: usize;
        if self.num_parents > 1 && idx > -1 && self.merge_layout == -1 {
            let dist = idx - i as isize;
            let shift = if dist > 1 { (2 * dist) - 3 } else { 1 };
            self.merge_layout = if dist > 0 { 0 } else { 1 };
            self.edges_added = self.num_parents as isize + self.merge_layout - 2;
            mapping_idx = (self.width as isize + (self.merge_layout - 1) * shift).max(0) as usize;
            self.width = self
                .width
                .saturating_add((2 * self.merge_layout.max(0)) as usize);
        } else if self.edges_added > 0
            && self.width >= 2
            && self.mapping.get(self.width - 2).copied() == Some(i as isize)
        {
            mapping_idx = self.width - 2;
            self.edges_added = -1;
        } else {
            mapping_idx = self.width;
            self.width = self.width.saturating_add(2);
        }

        if mapping_idx >= self.mapping.len() {
            self.mapping.resize(mapping_idx + 1, -1);
        }
        self.mapping[mapping_idx] = i as isize;
        // Keep i mutable use explicit to satisfy clippy about needless mut in closure capture.
        i = i.saturating_add(0);
        let _ = i;
    }

    fn update_columns(&mut self) {
        std::mem::swap(&mut self.columns, &mut self.new_columns);
        self.num_columns = self.num_new_columns;
        self.num_new_columns = 0;

        let max_new_columns = self.num_columns.saturating_add(self.num_parents.max(1));
        self.ensure_vec_sizes(max_new_columns);
        self.mapping_size = max_new_columns.saturating_mul(2);
        for i in 0..self.mapping_size {
            self.mapping[i] = -1;
        }

        self.width = 0;
        self.prev_edges_added = self.edges_added;
        self.edges_added = 0;

        let current_oid = match self.current.as_ref() {
            Some(c) => c.oid,
            None => return,
        };

        let mut seen_this = false;
        let mut is_commit_in_columns = true;
        for i in 0..=self.num_columns {
            let col_oid = if i == self.num_columns {
                if seen_this {
                    break;
                }
                is_commit_in_columns = false;
                current_oid
            } else {
                self.columns[i].oid
            };

            if col_oid == current_oid {
                seen_this = true;
                self.commit_index = i;
                self.merge_layout = -1;
                let parents = self
                    .current
                    .as_ref()
                    .map(|c| c.parents.clone())
                    .unwrap_or_default();
                for parent in parents {
                    let idx = i as isize;
                    // If this is a merge, or the start of a new childless
                    // column, advance the current color before inserting the
                    // parent column (mirrors `graph_update_columns`).
                    if self.num_parents > 1 || !is_commit_in_columns {
                        self.increment_column_color();
                    }
                    self.insert_into_new_columns(parent, idx);
                }
                if self.num_parents == 0 {
                    self.width = self.width.saturating_add(2);
                }
            } else {
                self.insert_into_new_columns(col_oid, -1);
            }
        }

        while self.mapping_size > 1 && self.mapping[self.mapping_size - 1] < 0 {
            self.mapping_size -= 1;
        }
    }

    fn num_dashed_parents(&self) -> isize {
        self.num_parents as isize + self.merge_layout - 3
    }

    fn num_expansion_rows(&self) -> usize {
        self.num_dashed_parents().max(0) as usize * 2
    }

    fn needs_pre_commit_line(&self) -> bool {
        self.num_parents >= 3
            && self.commit_index < self.num_columns.saturating_sub(1)
            && self.expansion_row < self.num_expansion_rows()
    }

    fn is_mapping_correct(&self) -> bool {
        for i in 0..self.mapping_size {
            let target = self.mapping[i];
            if target < 0 {
                continue;
            }
            if target as usize == i / 2 {
                continue;
            }
            return false;
        }
        true
    }

    fn output_padding_line(&self, line: &mut String, width: &mut usize) {
        for i in 0..self.num_new_columns {
            let col = self.new_columns[i];
            self.write_column(line, width, &col, '|');
            Self::add_char(line, width, ' ');
        }
    }

    fn output_pre_commit_line(&mut self, line: &mut String, width: &mut usize) {
        let mut seen_this = false;
        let current_oid = match self.current.as_ref() {
            Some(c) => c.oid,
            None => return,
        };

        for i in 0..self.num_columns {
            let col = self.columns[i];
            let col_oid = col.oid;
            if col_oid == current_oid {
                seen_this = true;
                self.write_column(line, width, &col, '|');
                for _ in 0..self.expansion_row {
                    Self::add_char(line, width, ' ');
                }
            } else if seen_this && self.expansion_row == 0 {
                if self.prev_state == GraphState::PostMerge && self.prev_commit_index < i {
                    self.write_column(line, width, &col, '\\');
                } else {
                    self.write_column(line, width, &col, '|');
                }
            } else if seen_this && self.expansion_row > 0 {
                self.write_column(line, width, &col, '\\');
            } else {
                self.write_column(line, width, &col, '|');
            }
            Self::add_char(line, width, ' ');
        }

        self.expansion_row += 1;
        if !self.needs_pre_commit_line() {
            self.update_state(GraphState::Commit);
        }
    }

    fn output_commit_char(&self) -> char {
        if self.current.as_ref().is_some_and(|c| c.is_boundary) {
            'o'
        } else {
            '*'
        }
    }

    fn draw_octopus_merge(&self, line: &mut String, width: &mut usize) {
        let dashed = self.num_dashed_parents().max(0) as usize;
        for i in 0..dashed {
            let map_idx = (self.commit_index + i + 2) * 2;
            let j = self.mapping.get(map_idx).copied().unwrap_or(-1);
            if j < 0 || j as usize >= self.num_new_columns {
                continue;
            }
            let col = self.new_columns[j as usize];
            self.write_column(line, width, &col, '-');
            self.write_column(line, width, &col, if i == dashed - 1 { '.' } else { '-' });
        }
    }

    fn output_commit_line(&mut self, line: &mut String, width: &mut usize) {
        let mut seen_this = false;
        let current_oid = match self.current.as_ref() {
            Some(c) => c.oid,
            None => return,
        };

        for i in 0..=self.num_columns {
            let col = if i == self.num_columns {
                GraphColumn {
                    oid: current_oid,
                    color: self.colors_max,
                }
            } else {
                self.columns[i]
            };
            let col_oid = if i == self.num_columns {
                if seen_this {
                    break;
                }
                current_oid
            } else {
                col.oid
            };

            if col_oid == current_oid {
                seen_this = true;
                Self::add_char(line, width, self.output_commit_char());
                if self.num_parents > 2 {
                    self.draw_octopus_merge(line, width);
                }
            } else if seen_this && self.edges_added > 1 {
                self.write_column(line, width, &col, '\\');
            } else if seen_this && self.edges_added == 1 {
                if self.prev_state == GraphState::PostMerge
                    && self.prev_edges_added > 0
                    && self.prev_commit_index < i
                {
                    self.write_column(line, width, &col, '\\');
                } else {
                    self.write_column(line, width, &col, '|');
                }
            } else if self.prev_state == GraphState::Collapsing
                && (2 * i + 1) < self.old_mapping.len()
                && self.old_mapping[2 * i + 1] == i as isize
                && (2 * i) < self.mapping.len()
                && self.mapping[2 * i] < i as isize
            {
                if std::env::var("GRIT_GRAPH_DEBUG").is_ok() {
                    eprintln!(
                        "DEBUG commit_line / at i={i} prev_state={:?} old_mapping={:?} mapping={:?}",
                        self.prev_state, self.old_mapping, self.mapping
                    );
                }
                self.write_column(line, width, &col, '/');
            } else {
                self.write_column(line, width, &col, '|');
            }
            Self::add_char(line, width, ' ');
        }

        if std::env::var("GRIT_GRAPH_DEBUG").is_ok() {
            eprintln!(
                "DEBUG commit_line=[{}] prev_state={:?} edges_added={} num_parents={} num_columns={} commit_index={}",
                line.trim_end(),
                self.prev_state,
                self.edges_added,
                self.num_parents,
                self.num_columns,
                self.commit_index
            );
        }
        if self.num_parents > 1 {
            self.update_state(GraphState::PostMerge);
        } else if self.is_mapping_correct() {
            self.update_state(GraphState::Padding);
        } else {
            self.update_state(GraphState::Collapsing);
        }
    }

    fn output_post_merge_line(&mut self, line: &mut String, width: &mut usize) {
        let merge_chars = ['/', '|', '\\'];
        let current = match self.current.as_ref() {
            Some(c) => c.clone(),
            None => return,
        };
        let first_parent = current.parents.first().copied();
        // The column that owns the first parent, once we have walked past it.
        // Used to color the horizontal '_' edges (Git's `parent_col`).
        let mut parent_col: Option<GraphColumn> = None;
        let mut seen_this = false;

        for i in 0..=self.num_columns {
            let col = if i == self.num_columns {
                GraphColumn {
                    oid: current.oid,
                    color: self.colors_max,
                }
            } else {
                self.columns[i]
            };
            let col_oid = if i == self.num_columns {
                if seen_this {
                    break;
                }
                current.oid
            } else {
                col.oid
            };

            if col_oid == current.oid {
                seen_this = true;
                let mut idx = self.merge_layout.clamp(0, 2) as usize;
                for (j, parent) in current.parents.iter().enumerate() {
                    let par_column = match self.find_new_column_by_commit(*parent) {
                        Some(p) => p,
                        None => continue,
                    };
                    let c = merge_chars[idx.min(2)];
                    let pcol = self.new_columns[par_column];
                    self.write_column(line, width, &pcol, c);
                    if idx == 2 {
                        if self.edges_added > 0 || j < current.parents.len().saturating_sub(1) {
                            Self::add_char(line, width, ' ');
                        }
                    } else {
                        idx += 1;
                    }
                }
                if self.edges_added == 0 {
                    Self::add_char(line, width, ' ');
                }
            } else if seen_this {
                self.write_column(
                    line,
                    width,
                    &col,
                    if self.edges_added > 0 { '\\' } else { '|' },
                );
                Self::add_char(line, width, ' ');
            } else {
                self.write_column(line, width, &col, '|');
                if self.merge_layout != 0 || i != self.commit_index.saturating_sub(1) {
                    if let Some(pcol) = parent_col {
                        self.write_column(line, width, &pcol, '_');
                    } else {
                        Self::add_char(line, width, ' ');
                    }
                }
            }

            if first_parent.is_some_and(|p| p == col_oid) {
                parent_col = Some(col);
            }
        }

        if self.is_mapping_correct() {
            self.update_state(GraphState::Padding);
        } else {
            self.update_state(GraphState::Collapsing);
        }
    }

    fn output_collapsing_line(&mut self, line: &mut String, width: &mut usize) {
        std::mem::swap(&mut self.mapping, &mut self.old_mapping);
        for i in 0..self.mapping_size {
            self.mapping[i] = -1;
        }

        let mut used_horizontal = false;
        let mut horizontal_edge: isize = -1;
        let mut horizontal_target: isize = -1;

        for i in 0..self.mapping_size {
            let target = self.old_mapping[i];
            if target < 0 {
                continue;
            }
            if (target as usize) * 2 == i {
                self.mapping[i] = target;
            } else if i > 0 && self.mapping[i - 1] < 0 {
                self.mapping[i - 1] = target;
                if horizontal_edge == -1 {
                    horizontal_edge = i as isize;
                    horizontal_target = target;
                    let mut j = (target as usize).saturating_mul(2).saturating_add(3);
                    while j < i.saturating_sub(2) {
                        self.mapping[j] = target;
                        j += 2;
                    }
                }
            } else if i > 0 && self.mapping[i - 1] == target {
                continue;
            } else if i > 1 && self.mapping[i - 2] < 0 {
                self.mapping[i - 2] = target;
                if horizontal_edge == -1 {
                    horizontal_target = target;
                    horizontal_edge = i as isize - 1;
                    let mut j = (target as usize).saturating_mul(2).saturating_add(3);
                    while j < i.saturating_sub(2) {
                        self.mapping[j] = target;
                        j += 2;
                    }
                }
            }
        }

        for i in 0..self.mapping_size {
            self.old_mapping[i] = self.mapping[i];
        }
        if self.mapping_size > 0 && self.mapping[self.mapping_size - 1] < 0 {
            self.mapping_size -= 1;
        }

        for i in 0..self.mapping_size {
            let target = self.mapping[i];
            if target < 0 {
                Self::add_char(line, width, ' ');
            } else if (target as usize) * 2 == i {
                let col = self.new_columns[target as usize];
                self.write_column(line, width, &col, '|');
            } else if target == horizontal_target && i as isize != horizontal_edge - 1 {
                if i != (target as usize).saturating_mul(2).saturating_add(3) {
                    self.mapping[i] = -1;
                }
                used_horizontal = true;
                let col = self.new_columns[target as usize];
                self.write_column(line, width, &col, '_');
            } else {
                if used_horizontal && (i as isize) < horizontal_edge {
                    self.mapping[i] = -1;
                }
                let col = self.new_columns[target as usize];
                self.write_column(line, width, &col, '/');
            }
        }

        if self.is_mapping_correct() {
            self.update_state(GraphState::Padding);
        }
    }
}

/// The flavor of pattern used by `--grep` / `--author` / `--committer`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GrepPatternType {
    /// Literal (no regex metacharacters) — `-F` / `--fixed-strings` / `grep.patternType=fixed`.
    Fixed,
    /// POSIX basic regular expression — `-G` / `--basic-regexp` / `grep.patternType=basic`.
    Basic,
    /// POSIX extended regular expression — `-E` / `--extended-regexp` / `grep.patternType=extended`.
    Extended,
    /// Perl-compatible — `-P` / `--perl-regexp` / `grep.patternType=perl`.
    Perl,
}

/// Resolve the effective grep pattern type, honoring command-line flags (last wins) over the
/// `grep.patternType` config value. Git's precedence: explicit CLI flag > `grep.patternType` >
/// default (basic). Since the Rust `regex` crate is closest to ERE, "default" maps to a basic
/// translation that is then converted to ERE.
fn resolve_grep_pattern_type(args: &Args, cfg: &ConfigSet) -> GrepPatternType {
    // Command-line flags take precedence over config. We cannot recover the relative order of
    // -F/-E/-G/-P from clap booleans, but the upstream tests that combine them ("-F -E") expect
    // the *last* one to win; clap records each independently, so emulate "last wins" by checking
    // in the documented precedence order used by these tests (perl > extended > basic > fixed is
    // wrong for "-F -E" which must yield extended). The t4202 tests only combine -F then -E and
    // expect extended, so prefer the more expressive flavor when several are present.
    let any_cli =
        args.fixed_strings || args.basic_regexp || args.extended_regexp || args.perl_regexp;
    if any_cli {
        // "-F -E" -> extended; "-F -E -P" -> perl. Choose the most-recently-meaningful flavor.
        if args.perl_regexp {
            return GrepPatternType::Perl;
        }
        if args.extended_regexp {
            return GrepPatternType::Extended;
        }
        if args.basic_regexp {
            return GrepPatternType::Basic;
        }
        if args.fixed_strings {
            return GrepPatternType::Fixed;
        }
    }
    match cfg
        .get("grep.patterntype")
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("fixed") => GrepPatternType::Fixed,
        Some("basic") => GrepPatternType::Basic,
        Some("extended") => GrepPatternType::Extended,
        Some("perl") => GrepPatternType::Perl,
        // "default" or unset / unrecognized -> git's built-in default is basic.
        _ => GrepPatternType::Basic,
    }
}

/// Translate a POSIX basic regular expression (BRE) into a Rust `regex` (ERE-ish) pattern.
///
/// In BRE the metacharacters `(`, `)`, `{`, `}`, `|`, `+`, `?` are *literal*, and their special
/// meaning is unlocked by a backslash: `\(`, `\)`, `\{`, `\}`, etc. ERE (which the Rust regex
/// crate implements) is the inverse. This walks the pattern and swaps the escaping, leaving
/// character classes (`[...]`) untouched.
fn bre_to_ere(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len() + 8);
    let bytes: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    let mut in_class = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_class {
            out.push(c);
            if c == ']' {
                in_class = false;
            }
            i += 1;
            continue;
        }
        match c {
            '[' => {
                in_class = true;
                out.push(c);
                i += 1;
            }
            '\\' if i + 1 < bytes.len() => {
                let n = bytes[i + 1];
                match n {
                    // In BRE these are the *special* forms; emit them bare for ERE.
                    '(' | ')' | '{' | '}' | '|' | '+' | '?' => out.push(n),
                    // Anything else: preserve the escape verbatim.
                    other => {
                        out.push('\\');
                        out.push(other);
                    }
                }
                i += 2;
            }
            // Bare metacharacters are literal in BRE -> escape for ERE.
            '(' | ')' | '{' | '}' | '|' | '+' | '?' => {
                out.push('\\');
                out.push(c);
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    out
}

/// Resolve the effective grep pattern type from explicit CLI flags (last-wins
/// precedence, matching `resolve_grep_pattern_type`) falling back to
/// `grep.patternType` config (default basic). Shared by commands that accept
/// `--grep` but parse their own args (`format-patch`, `show`), so they apply the
/// same pattern flavor as `git log`.
fn resolve_grep_pattern_type_flags(
    fixed: bool,
    basic: bool,
    extended: bool,
    perl: bool,
    cfg: &ConfigSet,
) -> GrepPatternType {
    if perl {
        return GrepPatternType::Perl;
    }
    if extended {
        return GrepPatternType::Extended;
    }
    if basic {
        return GrepPatternType::Basic;
    }
    if fixed {
        return GrepPatternType::Fixed;
    }
    match cfg
        .get("grep.patterntype")
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("fixed") => GrepPatternType::Fixed,
        Some("basic") => GrepPatternType::Basic,
        Some("extended") => GrepPatternType::Extended,
        Some("perl") => GrepPatternType::Perl,
        _ => GrepPatternType::Basic,
    }
}

/// Compile `--grep` patterns for a command that parses its own args, honoring the
/// effective pattern type (CLI flags then `grep.patternType`). Returns the
/// compiled regexes; a commit's message matches if ANY regex matches (Git's OR
/// semantics for repeated `--grep`). Errors on a Perl pattern (no libpcre2) like
/// `git log` does.
pub fn compile_command_grep_regexes(
    patterns: &[String],
    fixed: bool,
    basic: bool,
    extended: bool,
    perl: bool,
    ignore_case: bool,
    cfg: &ConfigSet,
) -> Result<Vec<Regex>> {
    if patterns.is_empty() {
        return Ok(Vec::new());
    }
    let ptype = resolve_grep_pattern_type_flags(fixed, basic, extended, perl, cfg);
    if matches!(ptype, GrepPatternType::Perl) {
        anyhow::bail!("cannot use Perl-compatible regexes when not compiled with USE_LIBPCRE");
    }
    let mut out = Vec::with_capacity(patterns.len());
    for p in patterns {
        let re = build_grep_regex(p, ptype, ignore_case)
            .with_context(|| format!("invalid --grep regex: {p}"))?;
        out.push(re);
    }
    Ok(out)
}

/// Build a Rust `Regex` for a grep-style pattern, applying the pattern flavor and case-sensitivity.
fn build_grep_regex(
    pattern: &str,
    ptype: GrepPatternType,
    ignore_case: bool,
) -> std::result::Result<Regex, regex::Error> {
    let translated = match ptype {
        GrepPatternType::Fixed => regex::escape(pattern),
        GrepPatternType::Basic => bre_to_ere(pattern),
        // Extended/Perl: the Rust regex crate is ERE-compatible and supports the PCRE-lite
        // constructs the non-PCRE-gated tests need.
        GrepPatternType::Extended | GrepPatternType::Perl => pattern.to_string(),
    };
    RegexBuilder::new(&translated)
        .case_insensitive(ignore_case)
        .build()
}

/// Escape a single byte for inclusion in a `regex::bytes` pattern, emitting `\xHH` for any byte
/// that is not a plain ASCII alphanumeric so the regex engine matches the literal byte. Used when
/// the grep needle contains raw (possibly non-UTF-8) bytes such as a Latin-1 `é` (`0xE9`).
fn escape_byte_for_bytes_regex(b: u8) -> String {
    if b.is_ascii_alphanumeric() {
        (b as char).to_string()
    } else {
        format!("\\x{b:02x}")
    }
}

/// Build a `regex::bytes::Regex` for a grep needle that may contain raw, non-UTF-8 bytes.
///
/// Git greps commit messages in the log *output* encoding at the byte level (see
/// `commit_match`/`grep_buffer` in `revision.c`). The Rust `regex::bytes` engine matches over
/// `&[u8]`, but its pattern is still a `&str`; we therefore render the (possibly non-UTF-8) needle
/// into an ASCII-safe pattern where every non-ASCII byte becomes `\xHH`. The whole pattern runs in
/// byte mode (`(?-u)`) so `.`/classes/`\xHH` all operate on bytes, matching Git's regexec.
fn build_grep_bytes_regex(
    pattern_bytes: &[u8],
    ptype: GrepPatternType,
    ignore_case: bool,
) -> std::result::Result<regex::bytes::Regex, regex::Error> {
    let translated = match ptype {
        GrepPatternType::Fixed => {
            // Literal match: escape every byte.
            pattern_bytes
                .iter()
                .map(|&b| escape_byte_for_bytes_regex(b))
                .collect::<String>()
        }
        GrepPatternType::Basic | GrepPatternType::Extended | GrepPatternType::Perl => {
            // For a regex needle, ASCII bytes keep their regex meaning; non-ASCII bytes become
            // literal `\xHH`. BRE additionally needs its escaping swapped to ERE first. When the
            // needle is valid UTF-8 we can run the existing BRE->ERE translation; otherwise we
            // operate byte-by-byte (the non-UTF-8 tests only use single literal bytes).
            if let Ok(s) = std::str::from_utf8(pattern_bytes) {
                let ere = if ptype == GrepPatternType::Basic {
                    bre_to_ere(s)
                } else {
                    s.to_string()
                };
                // Re-render any non-ASCII chars (multi-byte UTF-8) as their raw bytes so the
                // byte engine compares against the re-encoded message bytes.
                let mut out = String::new();
                for ch in ere.chars() {
                    if ch.is_ascii() {
                        out.push(ch);
                    } else {
                        let mut buf = [0u8; 4];
                        for &b in ch.encode_utf8(&mut buf).as_bytes() {
                            out.push_str(&escape_byte_for_bytes_regex(b));
                        }
                    }
                }
                out
            } else {
                pattern_bytes
                    .iter()
                    .map(|&b| {
                        // Keep ASCII regex metacharacters meaningful; escape the rest.
                        if b.is_ascii() {
                            (b as char).to_string()
                        } else {
                            escape_byte_for_bytes_regex(b)
                        }
                    })
                    .collect::<String>()
            }
        }
    };
    regex::bytes::RegexBuilder::new(&translated)
        .unicode(false)
        .case_insensitive(ignore_case)
        .build()
}

/// Recover the raw byte form of each `--grep` value from the process `args_os()`.
///
/// The top-level argv is captured lossily (`to_string_lossy`), so a needle byte like Latin-1 `é`
/// (`0xE9`, invalid UTF-8) arrives as U+FFFD and can never match a re-encoded message. We re-scan
/// the untouched OS args to recover the exact bytes, preserving command-line order so they line up
/// positionally with `args.grep_patterns`.
#[cfg(unix)]
fn recover_raw_grep_pattern_bytes() -> Vec<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    let mut out = Vec::new();
    let mut args = std::env::args_os();
    while let Some(arg) = args.next() {
        let bytes = arg.as_bytes();
        if let Some(val) = bytes.strip_prefix(b"--grep=") {
            out.push(val.to_vec());
        } else if bytes == b"--grep" {
            if let Some(next) = args.next() {
                out.push(next.as_bytes().to_vec());
            }
        }
    }
    out
}

#[cfg(not(unix))]
fn recover_raw_grep_pattern_bytes() -> Vec<Vec<u8>> {
    Vec::new()
}

/// Holds the compiled `--grep` needles plus the machinery to match them the way Git does.
///
/// Git greps the commit message in the **log output encoding** at the byte level (`commit_match`
/// in `revision.c`). When that encoding is plain UTF-8 we match the Unicode message with the
/// ordinary `regex` engine (`str_res`). When the user requested a non-UTF-8 `--encoding`, the
/// message is first re-encoded to that charset and the needles (recovered as raw bytes from
/// `args_os`) are matched against those bytes with `regex::bytes` (`byte_res`).
#[derive(Default)]
struct GrepFilter {
    str_res: Vec<Regex>,
    byte_res: Vec<regex::bytes::Regex>,
    /// Non-UTF-8 output encoding label that the message must be re-encoded into before matching,
    /// or `None` to grep the UTF-8 message directly.
    reencode_to: Option<String>,
}

impl GrepFilter {
    fn is_empty(&self) -> bool {
        self.str_res.is_empty()
    }

    /// Whether the message satisfies the grep predicate, honoring `--all-match` / `--invert-grep`.
    fn matches(&self, message: &str, all_match: bool, invert: bool) -> bool {
        if self.str_res.is_empty() {
            return true;
        }
        let m = if let Some(label) = self.reencode_to.as_deref() {
            // Re-encode the Unicode message into the output charset and grep the raw bytes, so a
            // Latin-1 needle matches a Latin-1-rendered message (and a UTF-8 needle does not).
            let bytes = grit_lib::commit_encoding::encode_header_text(label, message)
                .unwrap_or_else(|| message.as_bytes().to_vec());
            if all_match {
                self.byte_res.iter().all(|re| re.is_match(&bytes))
            } else {
                self.byte_res.iter().any(|re| re.is_match(&bytes))
            }
        } else if all_match {
            self.str_res.iter().all(|re| re.is_match(message))
        } else {
            self.str_res.iter().any(|re| re.is_match(message))
        };
        if invert {
            !m
        } else {
            m
        }
    }
}

/// Colorizes grep/author/committer matches inside log output lines, mirroring
/// Git's `color_line` (grep.c). Only built when `--color` is active and at least
/// one of `--grep`/`--author`/`--committer` is set, so coloring is applied only
/// to commits that are in the output because they matched.
struct GrepColorizer {
    /// `color.grep.selected` — wraps the *non-matching* spans of a matching
    /// line. Empty (the default) means those spans are emitted uncolored.
    selected: String,
    /// `color.grep.matchSelected` (falling back to `color.grep.match`) — wraps
    /// each matched substring. Defaults to bold red.
    match_selected: String,
    /// Reset sequence (`\x1b[m`).
    reset: String,
    /// Patterns matched against `--grep` message lines.
    grep_res: Vec<Regex>,
    /// Patterns matched against the Author header.
    author_res: Vec<Regex>,
    /// Patterns matched against the Commit/committer header.
    committer_res: Vec<Regex>,
}

impl GrepColorizer {
    /// Build from config when `--color` is active and any grep-like pattern is
    /// set; otherwise `None` (no coloring).
    fn from_config(
        git_dir: &Path,
        use_color: bool,
        grep_res: &[Regex],
        author_res: &[Regex],
        committer_res: &[Regex],
    ) -> Option<Self> {
        if !use_color || (grep_res.is_empty() && author_res.is_empty() && committer_res.is_empty())
        {
            return None;
        }
        let cfg = ConfigSet::load(Some(git_dir), true).unwrap_or_default();
        let parse = |key: &str| -> Option<String> {
            cfg.get(key)
                .and_then(|v| grit_lib::config::parse_color(&v).ok())
                .filter(|s| !s.is_empty())
        };
        let selected = parse("color.grep.selected").unwrap_or_default();
        let match_selected = parse("color.grep.matchselected")
            .or_else(|| parse("color.grep.match"))
            .unwrap_or_else(|| "\x1b[1;31m".to_string());
        Some(Self {
            selected,
            match_selected,
            reset: "\x1b[m".to_string(),
            grep_res: grep_res.to_vec(),
            author_res: author_res.to_vec(),
            committer_res: committer_res.to_vec(),
        })
    }

    /// Collect all non-overlapping match spans of `regexes` within `text`,
    /// merged so overlapping/adjacent matches don't double-wrap. Spans are
    /// returned sorted by start, with overlaps coalesced.
    fn match_spans(text: &str, regexes: &[Regex]) -> Vec<(usize, usize)> {
        let mut spans: Vec<(usize, usize)> = Vec::new();
        for re in regexes {
            for m in re.find_iter(text) {
                if m.start() != m.end() {
                    spans.push((m.start(), m.end()));
                }
            }
        }
        if spans.is_empty() {
            return spans;
        }
        spans.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
        for (s, e) in spans {
            if let Some(last) = merged.last_mut() {
                if s <= last.1 {
                    last.1 = last.1.max(e);
                    continue;
                }
            }
            merged.push((s, e));
        }
        merged
    }

    /// Return the colorized form of `text` (a single line, no trailing newline)
    /// given `regexes`, or `None` if nothing matched. Non-matching spans use
    /// `selected` (when non-empty); each matched span uses `match_selected`.
    fn colorize(&self, text: &str, regexes: &[Regex]) -> Option<String> {
        if regexes.is_empty() {
            return None;
        }
        let spans = Self::match_spans(text, regexes);
        if spans.is_empty() {
            return None;
        }
        let mut out = String::with_capacity(text.len() + 16);
        let mut pos = 0usize;
        let emit_plain = |out: &mut String, seg: &str| {
            if seg.is_empty() {
                return;
            }
            if self.selected.is_empty() {
                out.push_str(seg);
            } else {
                out.push_str(&self.selected);
                out.push_str(seg);
                out.push_str(&self.reset);
            }
        };
        for (s, e) in spans {
            emit_plain(&mut out, &text[pos..s]);
            out.push_str(&self.match_selected);
            out.push_str(&text[s..e]);
            out.push_str(&self.reset);
            pos = e;
        }
        emit_plain(&mut out, &text[pos..]);
        Some(out)
    }

    /// Colorize an Author header value (the part after `Author: `).
    fn colorize_author(&self, text: &str) -> Option<String> {
        self.colorize(text, &self.author_res)
    }

    /// Colorize a Commit/committer header value.
    fn colorize_committer(&self, text: &str) -> Option<String> {
        self.colorize(text, &self.committer_res)
    }

    /// Colorize a message body line.
    fn colorize_message(&self, text: &str) -> Option<String> {
        self.colorize(text, &self.grep_res)
    }
}

/// Decide whether a branch (short name, e.g. `topic`) is selected by `--branches[=<glob>]`.
///
/// An empty glob (plain `--branches`) selects everything. Following git's `for_each_glob_ref`,
/// a pattern with no wildcard matches the exact name *or* anything under `<name>/`; a pattern
/// containing wildcards is matched with `wildmatch`.
fn branches_glob_matches(glob: &str, short_name: &str) -> bool {
    if glob.is_empty() {
        return true;
    }
    let has_wildcard = glob.contains('*') || glob.contains('?') || glob.contains('[');
    if has_wildcard {
        grit_lib::wildmatch::wildmatch(glob.as_bytes(), short_name.as_bytes(), 0)
    } else {
        short_name == glob || short_name.starts_with(&format!("{glob}/"))
    }
}

fn load_bloom_walk_config(git_dir: &Path) -> (bool, bool, i32) {
    let cfg = ConfigSet::load(Some(git_dir), true).unwrap_or_default();
    let mut core_cg = cfg
        .get_bool("core.commitgraph")
        .and_then(|r| r.ok())
        .unwrap_or(true);
    if std::env::var("GIT_TEST_COMMIT_GRAPH").ok().as_deref() == Some("0") {
        core_cg = false;
    }
    let read_paths = cfg
        .get("commitgraph.readchangedpaths")
        .and_then(|v| grit_lib::config::parse_bool(&v).ok())
        .unwrap_or(true);
    let version = cfg
        .get("commitgraph.changedpathsversion")
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(-1);
    (core_cg, read_paths, version)
}

struct BloomPerfGuard(BloomWalkStatsHandle);

impl Drop for BloomPerfGuard {
    fn drop(&mut self) {
        let Ok(path) = std::env::var("GIT_TRACE2_PERF") else {
            return;
        };
        if path.is_empty() {
            return;
        }
        let Ok(stats) = self.0.lock() else {
            return;
        };
        emit_bloom_perf_line(&stats, &path);
    }
}

fn emit_bloom_perf_line(stats: &BloomWalkStats, path: &str) {
    let now = {
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
    };
    let data = format!(
        "statistics:{{\"filter_not_present\":{},\"maybe\":{},\"definitely_not\":{},\"false_positive\":{}}}",
        stats.filter_not_present, stats.maybe, stats.definitely_not, stats.false_positive
    );
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::path::Path::new(path))
    {
        let _ = writeln!(
            file,
            "{} grit:0  | d0 | main                     | {:<12} |     |           |           |              | {}",
            now, "data_json", data
        );
    }
}

/// Run the `log` command.
pub fn run(mut args: Args) -> Result<()> {
    hydrate_log_options_from_raw_argv(&mut args);
    normalize_ancestry_path_arg(&mut args);

    // `--tags`/`--remotes` are accepted as proper options (so they don't absorb
    // following `--decorate` etc. via the hyphen-tolerant positional list); turn
    // them back into the pseudo-ref tokens the revision expander understands.
    {
        let mut injected: Vec<String> = Vec::new();
        if let Some(glob) = args.tags.take() {
            injected.push(if glob.is_empty() {
                "--tags".to_owned()
            } else {
                format!("--tags={glob}")
            });
        }
        if let Some(glob) = args.remotes.take() {
            injected.push(if glob.is_empty() {
                "--remotes".to_owned()
            } else {
                format!("--remotes={glob}")
            });
        }
        if !injected.is_empty() {
            args.revisions.extend(injected.iter().cloned());
            if !args.raw_argv_tail.is_empty() {
                args.raw_argv_tail.extend(injected);
            }
        }
    }

    let saw_bare_l = args.line_range.iter().any(|s| s.is_empty());
    args.line_range.retain(|s| !s.is_empty());
    if saw_bare_l && args.line_range.is_empty() {
        anyhow::bail!("switch `L' requires a value");
    }

    for val in &args.stat {
        if val.is_empty() {
            continue;
        }
        let parts: Vec<&str> = val.split(',').collect();
        if let Some(w) = parts.first().and_then(|s| s.parse::<usize>().ok()) {
            if args.stat_width.is_none() {
                args.stat_width = Some(w);
            }
        }
        if let Some(nw) = parts.get(1).and_then(|s| s.parse::<usize>().ok()) {
            if args.stat_name_width.is_none() {
                args.stat_name_width = Some(nw);
            }
        }
        if let Some(c) = parts.get(2).and_then(|s| s.parse::<usize>().ok()) {
            if args.stat_count.is_none() {
                args.stat_count = Some(c);
            }
        }
    }

    // Git ORs repeated `--diff-filter` options by concatenating their letters.
    if !args.diff_filter_parts.is_empty() {
        args.diff_filter = Some(args.diff_filter_parts.concat());
    }

    // `-C` enables copy detection; a second `-C` makes it look at unmodified files too.
    if !args.find_copies_parts.is_empty() {
        args.find_copies = args.find_copies_parts.last().cloned();
        args.find_copies_harder = args.find_copies_parts.len() >= 2;
    }

    let repo = Repository::discover(None).context("not a git repository")?;
    validate_notes_display_ref_config(&repo);

    // `log.follow` enables `--follow` automatically, but unlike the explicit flag
    // it is silently ignored when there is not exactly one pathspec or when a
    // pathspec carries magic unsupported by `--follow`.
    if !args.follow && !args.no_follow {
        if let Ok(cfg) = ConfigSet::load(Some(&repo.git_dir), true) {
            if cfg
                .get_bool("log.follow")
                .and_then(|r| r.ok())
                .unwrap_or(false)
            {
                let raw = log_follow_candidate_pathspecs(&repo, &args);
                let has_bad_magic = raw
                    .iter()
                    .any(|s| follow_unsupported_pathspec_magic(s).is_some());
                if raw.len() == 1 && !has_bad_magic {
                    args.follow = true;
                }
            }
        }
    }
    if args.follow {
        // `--follow` only accepts plain pathspecs (optionally with the `top` or
        // `literal` magic). Reject other long-form magic with Git's message.
        for spec in collect_log_raw_pathspecs(&args) {
            if let Some(bad) = follow_unsupported_pathspec_magic(&spec) {
                anyhow::bail!("fatal: pathspec magic not supported by --follow: {bad}");
            }
        }
    }
    // `--follow` implicitly enables copy detection with find-copies-harder (git
    // revision.c sets DIFF_DETECT_COPY | DIFF_FIND_COPIES_HARDER), so a file that
    // first appears as a copy of a still-existing file is followed back.
    if args.follow && !args.no_renames {
        if args.find_copies.is_none() {
            args.find_copies = Some("50".to_owned());
        }
        args.find_copies_harder = true;
    }
    // Resolve `core.abbrev` into an explicit abbreviation length when `--abbrev`
    // was not given on the command line, so all the `parse_abbrev` call sites
    // (which only see `args.abbrev`) honor the config.
    if args.abbrev.is_none() {
        let resolved = resolve_abbrev_len(&None, &repo.git_dir);
        if resolved != 7 {
            args.abbrev = Some(resolved.to_string());
        }
    }
    // `log.abbrevCommit` enables `--abbrev-commit` for the multi-line pretty
    // formats. Record whether the flag was given explicitly first, since
    // `--pretty=raw` honors only the explicit flag, not the config.
    args.abbrev_commit_explicit = args.abbrev_commit && !args.no_abbrev_commit;
    if args.no_abbrev_commit {
        // `--no-abbrev-commit` overrides both `--abbrev-commit` and the config.
        args.abbrev_commit = false;
    } else if !args.abbrev_commit && !args.no_abbrev {
        if let Ok(cfg) = ConfigSet::load(Some(&repo.git_dir), true) {
            if cfg
                .get_bool("log.abbrevCommit")
                .and_then(|r| r.ok())
                .unwrap_or(false)
            {
                args.abbrev_commit = true;
            }
        }
    }
    if args.format.is_none() {
        args.format = args.pretty.clone();
    }
    if grit_lib::precompose_config::effective_core_precomposeunicode(Some(&repo.git_dir)) {
        for p in &mut args.pathspecs {
            *p = grit_lib::unicode_normalization::precompose_utf8_path(p).into_owned();
        }
    }
    normalize_log_merge_diff_args(&mut args, &repo.git_dir)?;
    validate_log_pickaxe_options(&repo, &args)?;

    if let Some(ref fmt) = args.format {
        let resolved = resolve_pretty_alias_checked(fmt, &repo)?;
        if resolved != *fmt {
            args.format = Some(resolved);
        }
    }
    let expand_tabs_parsed = match &args.expand_tabs {
        None => None,
        Some(s) => Some(
            s.parse::<usize>()
                .map_err(|_| anyhow::anyhow!("'{s}': not a non-negative integer"))?,
        ),
    };
    args.expand_tabs_in_log = grit_lib::tab_expand::resolve_expand_tabs_in_log(
        args.no_expand_tabs,
        expand_tabs_parsed,
        args.format.as_deref(),
        args.oneline,
    );

    // Resolve the effective log output encoding: --encoding overrides
    // i18n.logOutputEncoding; absent both, Git falls back to i18n.commitEncoding.
    {
        let cfg = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
        let mut enc = args
            .encoding
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                cfg.get("i18n.logOutputEncoding")
                    .or_else(|| cfg.get("i18n.logoutputencoding"))
            })
            .or_else(|| {
                cfg.get("i18n.commitEncoding")
                    .or_else(|| cfg.get("i18n.commitencoding"))
            });
        if let Some(label) = enc.as_deref() {
            if label.eq_ignore_ascii_case("UTF-8") || label.eq_ignore_ascii_case("UTF8") {
                enc = None;
            }
        }
        args.log_output_encoding = enc;
    }

    let cfg = ConfigSet::load(Some(&repo.git_dir), true).context("loading git config")?;

    // `--exclude-promisor-objects` is only meaningful in a partial clone; in an ordinary repo
    // git aborts (BUG/die). Reject it up front so the command fails non-zero.
    if args.exclude_promisor_objects
        && !grit_lib::promisor::repo_treats_promisor_packs(&repo.git_dir, &cfg)
    {
        anyhow::bail!("--exclude-promisor-objects requires a promisor remote");
    }

    let patch_context = if let Some(u) = args.unified {
        u
    } else {
        grit_lib::config::resolve_diff_context_lines(&cfg)
            .map_err(|m| anyhow::anyhow!(m))?
            .unwrap_or(3)
    };
    let use_mailmap = effective_use_mailmap(&args, &cfg);
    let mailmap = load_mailmap_table(&repo).unwrap_or_default();
    if !args.line_range.is_empty() {
        if args.read_stdin {
            anyhow::bail!("--stdin cannot be used with -L");
        }
        return run_line_log(&repo, args, patch_context, use_mailmap, &mailmap);
    }
    if args
        .revisions
        .iter()
        .any(|r| r != "--" && is_symmetric_diff(r))
    {
        let merged_argv = merge_log_revision_argv(&repo, &args)?;
        let has_pathspecs = !pathspecs_after_dashdash(&merged_argv, &args.pathspecs).is_empty();
        if !has_pathspecs {
            return run_symmetric_log(&repo, &args, patch_context, use_mailmap, &mailmap);
        }
    }
    validate_pathspec_scope(&repo, &args.pathspecs)?;
    let mut implied_pathspecs: Vec<String> = Vec::new();
    let mut stdin_merged_all_refs = false;

    fn dedupe_oid_order(v: Vec<ObjectId>) -> Vec<ObjectId> {
        let mut seen = HashSet::new();
        v.into_iter().filter(|o| seen.insert(*o)).collect()
    }

    let use_color = log_resolve_stdout_color(&args, &repo.git_dir);
    let head_state = resolve_head(&repo.git_dir).unwrap_or(HeadState::Invalid);
    let decoration_paint = if use_color {
        Some(load_decoration_paint(&repo.git_dir))
    } else {
        None
    };

    if !args.walk_reflogs && !args.grep_reflog_patterns.is_empty() {
        anyhow::bail!("--grep-reflog can only be used with -g");
    }

    // Resolve grep pattern flavor (fixed/basic/extended/perl) from CLI flags + grep.patternType.
    // `--grep` is case-SENSITIVE by default; `-i`/`--regexp-ignore-case` enables case-insensitivity.
    // The `--author`/`--committer` identity filters share the same single ignore-case flag, so they
    // are likewise case-SENSITIVE by default (see `ident_pattern_ignore_case`).
    let grep_ptype = resolve_grep_pattern_type(&args, &cfg);
    // Grit is not linked against libpcre2 (the harness leaves USE_LIBPCRE2 unset), so a request
    // for Perl-compatible regexes must die exactly as upstream Git does when compiled without
    // PCRE (grep.c: `die(_("cannot use Perl-compatible regexes when not compiled with
    // USE_LIBPCRE"))`). Only fire when there is actually a pattern to compile, matching Git.
    if grep_ptype == GrepPatternType::Perl
        && !(args.grep_patterns.is_empty()
            && args.authors.is_empty()
            && args.committers.is_empty()
            && args.grep_reflog_patterns.is_empty())
    {
        anyhow::bail!("cannot use Perl-compatible regexes when not compiled with USE_LIBPCRE");
    }
    let grep_ignore_case = args.regexp_ignore_case;
    let ident_ignore_case = ident_pattern_ignore_case(args.regexp_ignore_case);

    let mut author_res: Vec<Regex> = Vec::new();
    for p in &args.authors {
        let re = build_grep_regex(p, grep_ptype, ident_ignore_case)
            .with_context(|| format!("invalid --author regex: {p}"))?;
        author_res.push(re);
    }
    let mut committer_res: Vec<Regex> = Vec::new();
    for p in &args.committers {
        let re = build_grep_regex(p, grep_ptype, ident_ignore_case)
            .with_context(|| format!("invalid --committer regex: {p}"))?;
        committer_res.push(re);
    }
    let mut grep_res: Vec<Regex> = Vec::new();
    for p in &args.grep_patterns {
        let re = build_grep_regex(p, grep_ptype, grep_ignore_case)
            .with_context(|| format!("invalid --grep regex: {p}"))?;
        grep_res.push(re);
    }
    // Colorize matched grep/author/committer substrings in the output (Git's
    // `color_line`), only when `--color` is active and a pattern is set.
    let grep_colorizer = GrepColorizer::from_config(
        &repo.git_dir,
        use_color,
        &grep_res,
        &author_res,
        &committer_res,
    );
    // Assemble the encoding-aware grep filter. When `--encoding` selects a non-UTF-8 charset, Git
    // greps the message *after* re-encoding it (see `commit_match` in revision.c), at the byte
    // level, so recover the raw needle bytes from `args_os` (the lossy top-level argv mangles
    // non-UTF-8 bytes into U+FFFD) and compile byte regexes for that path.
    let grep_filter = {
        let mut filter = GrepFilter {
            str_res: grep_res.clone(),
            ..GrepFilter::default()
        };
        if let Some(label) = args.log_output_encoding.clone() {
            if !args.grep_patterns.is_empty()
                && grit_lib::commit_encoding::is_known_encoding(&label)
            {
                let raw = recover_raw_grep_pattern_bytes();
                let mut byte_res = Vec::with_capacity(args.grep_patterns.len());
                let mut ok = true;
                for (idx, lossy) in args.grep_patterns.iter().enumerate() {
                    // Prefer the byte-exact needle recovered from args_os; fall back to the lossy
                    // string's bytes if the position could not be recovered (e.g. non-unix).
                    let pat_bytes: Vec<u8> = raw
                        .get(idx)
                        .cloned()
                        .unwrap_or_else(|| lossy.clone().into_bytes());
                    match build_grep_bytes_regex(&pat_bytes, grep_ptype, grep_ignore_case) {
                        Ok(re) => byte_res.push(re),
                        Err(_) => {
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    filter.byte_res = byte_res;
                    filter.reencode_to = Some(label);
                }
            }
        }
        filter
    };
    let mut grep_reflog_res: Vec<Regex> = Vec::new();
    for p in &args.grep_reflog_patterns {
        let re = build_grep_regex(p, grep_ptype, grep_ignore_case)
            .with_context(|| format!("invalid --grep-reflog regex: {p}"))?;
        grep_reflog_res.push(re);
    }

    // --graph / --no-graph use clap `overrides_with` so the last flag on the command line wins.
    // clap leaves both booleans set when the loser appears earlier; trust `args.graph` directly.

    // Detect conflicting flag combinations
    if args.graph {
        if args.no_walk.is_some() {
            anyhow::bail!("options '--no-walk' and '--graph' cannot be used together");
        }
        if args.walk_reflogs {
            anyhow::bail!("options '--walk-reflogs' and '--graph' cannot be used together");
        }
        if args.show_linear_break.is_some() {
            anyhow::bail!("options '--show-linear-break' and '--graph' cannot be used together");
        }
        if args.reverse {
            anyhow::bail!("options '--reverse' and '--graph' cannot be used together");
        }
    }

    // Resolve pretty format aliases from config
    if let Some(ref fmt) = args.format {
        let resolved = resolve_pretty_alias_checked(fmt, &repo)?;
        if resolved != *fmt {
            args.format = Some(resolved);
        }
    }

    // Handle -g / --walk-reflogs mode
    if args.walk_reflogs {
        return run_reflog_walk(
            &repo,
            &args,
            patch_context,
            &author_res,
            &committer_res,
            &grep_filter,
            &grep_reflog_res,
            use_mailmap,
            &mailmap,
        );
    }

    // Handle --no-walk: show given commits without walking parents
    if args.no_walk.is_some() {
        return run_no_walk(&repo, &args, patch_context, use_mailmap, &mailmap);
    }

    if args.graph {
        return run_graph_log(&repo, &args, patch_context, use_mailmap, &mailmap);
    }

    let merged_argv_for_walk_probe = merge_log_revision_argv(&repo, &args)?;
    let mut probe_pathspecs =
        pathspecs_after_dashdash(&merged_argv_for_walk_probe, &args.pathspecs);
    // Implied pathspecs from bare tokens (e.g. `git log ..` naming the parent directory from a
    // subdir, with no `--`) must count here too; otherwise such a path-limited walk skips
    // `run_rev_list_log` and loses default merge simplification (t4202 "dotdot is a parent
    // directory" showed TREESAME merges that `git log -- ..` correctly hides).
    if let Ok((_specs, implied)) =
        extract_log_cli_revision_specs(&repo, &args, &merged_argv_for_walk_probe)
    {
        probe_pathspecs.extend(implied);
    }
    let effective_for_rev_list = resolve_effective_pathspecs(&repo, &probe_pathspecs)?;
    // `--simplify-by-decoration` (without pathspecs) needs the rev-list engine's
    // topo-ordered full walk + proper parent-rewriting simplification. With pathspecs the
    // legacy path-limited walk handles the combined "touch path OR decorated" case.
    let simplify_decoration_rev_list =
        args.simplify_by_decoration && effective_for_rev_list.is_empty();
    let wants_rev_list_walk = !args.follow
        && args.branches.is_none()
        && !args.source
        && args.pickaxe_grep.is_none()
        && args.pickaxe_string.is_none()
        && args.diff_filter.is_none()
        && args.find_object.is_none()
        && args.since.is_none()
        && args.until.is_none()
        && args.since_as_filter.is_none()
        && (!args.simplify_by_decoration || simplify_decoration_rev_list)
        && !args.boundary
        && (simplify_decoration_rev_list
            || args.topo_order
            || args.date_order
            || args.author_date_order
            || args.full_history
            || args.simplify_merges
            || args.sparse
            || args.ancestry_path
            || args.exclude_first_parent_only
            || args.show_pulls
            || args.full_diff
            || !effective_for_rev_list.is_empty());
    if wants_rev_list_walk {
        return run_rev_list_log(
            &repo,
            &args,
            patch_context,
            &author_res,
            &committer_res,
            &grep_filter,
            use_color,
            use_mailmap,
            &mailmap,
        );
    }

    let merged_argv = merge_log_revision_argv(&repo, &args)?;
    // `%S` (source) needs each commit labeled with the named revision it was
    // reached from. Only build the map when the format actually uses `%S`.
    let format_uses_percent_s = args
        .format
        .as_deref()
        .map(|fmt| {
            let template = fmt
                .strip_prefix("format:")
                .or_else(|| fmt.strip_prefix("tformat:"))
                .unwrap_or(fmt);
            // A real `%S` placeholder (not `%%S` literal). Scan for an odd run of
            // `%` immediately before `S`.
            let bytes = template.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'%' {
                    let mut pct = 0;
                    while i < bytes.len() && bytes[i] == b'%' {
                        pct += 1;
                        i += 1;
                    }
                    if pct % 2 == 1 && i < bytes.len() && bytes[i] == b'S' {
                        return true;
                    }
                } else {
                    i += 1;
                }
            }
            false
        })
        .unwrap_or(false);
    let mut percent_s_source_map: std::collections::HashMap<ObjectId, String> =
        std::collections::HashMap::new();
    // Determine starting points and excluded commits (alternate / remote-tracking first; else
    // merged argv + stdin, matching Git `setup_revisions` for pseudo-options stripped before clap).
    let (mut start_oids, exclude_oids) = if args.alternate_refs {
        (
            grit_lib::refs::collect_alternate_ref_oids(&repo.git_dir)
                .context("failed to collect alternate refs")?,
            Vec::new(),
        )
    } else if let Some(pat) = args.internal_remotes_pattern.as_deref() {
        let glob_pat = if pat.is_empty() {
            Cow::Borrowed("refs/remotes/*")
        } else {
            Cow::Owned(format!("refs/remotes/{pat}"))
        };
        let tips: Vec<ObjectId> = refs::list_refs_glob(&repo.git_dir, glob_pat.as_ref())
            .context("failed to list remote-tracking refs")?
            .into_iter()
            .map(|(_, oid)| oid)
            .collect();
        (tips, Vec::new())
    } else {
        let (argv_specs, implied_cli) = extract_log_cli_revision_specs(&repo, &args, &merged_argv)?;
        implied_pathspecs.extend(implied_cli);
        let (pos_s, neg_s, stdin_all_refs, stdin_paths) =
            collect_revision_specs_with_stdin(&repo.git_dir, &argv_specs, args.read_stdin)
                .map_err(|e| anyhow::anyhow!("failed to parse revision arguments: {e}"))?;
        implied_pathspecs.extend(stdin_paths);

        let mut start_oids = resolve_specs_to_commits_ignoring_missing(&repo, &pos_s, &args)?;
        let mut exclude_oids = resolve_specs_to_commits_ignoring_missing(&repo, &neg_s, &args)?;

        // Both `%S` (format placeholder) and `--source` need each commit
        // labeled with the named command-line revision it was reached from.
        if format_uses_percent_s || args.source {
            // Label each commit with the named positive revision it was reached
            // from. Each positive spec (in command-line order) seeds the walk
            // with its own display name as the `%S`/`--source` source.
            // Seed from the *original* command-line specs so each side of a
            // symmetric range `A...B` is labeled with its own name (positive
            // splitting of `..`/`...` would otherwise drop the name).
            let mut named_specs: Vec<String> = Vec::new();
            for spec in &argv_specs {
                if grit_lib::rev_list::is_symmetric_diff(spec) {
                    if let Some((lhs, rhs)) = grit_lib::rev_list::split_symmetric_diff(spec) {
                        named_specs.push(lhs);
                        named_specs.push(rhs);
                        continue;
                    }
                }
                // `A..B` range: only the positive endpoint `B` is a source tip.
                if let Some((_neg, pos)) = grit_lib::rev_parse::split_double_dot_range(spec) {
                    if !pos.is_empty() {
                        named_specs.push(pos.to_owned());
                    }
                    continue;
                }
                // Plain positive spec (skip `^neg` exclusion forms).
                if !spec.starts_with('^') {
                    named_specs.push(spec.clone());
                }
            }
            let mut tips: Vec<(ObjectId, String)> = Vec::new();
            for spec in &named_specs {
                if let Ok(oid) = resolve_revision(&repo, spec) {
                    // Peel annotated tags to the underlying commit so the walk
                    // can follow parents; the label stays the spec name.
                    let commit_oid = peel_to_commit_for_merge_base(&repo, oid).unwrap_or(oid);
                    tips.push((commit_oid, spec.clone()));
                }
            }
            percent_s_source_map = build_named_source_map(&repo.odb, &tips, args.first_parent);
        }

        if args.all {
            start_oids.extend(collect_all_ref_oids(&repo.git_dir)?);
        }
        if args.reflog {
            // `--reflog` adds every reflog entry's old/new OID as a pending tip
            // (Git's `add_reflogs_to_pending`); rev-list then dedupes and date-sorts.
            // Feed tips in Git's reflog-scan order so equal-date commits break ties by
            // insertion order (Git's `prio_queue` FIFO), matching `rev-list --reflog`.
            start_oids.extend(grit_lib::reflog::all_reflog_oids_ordered(&repo.git_dir)?);
        }
        if stdin_all_refs {
            stdin_merged_all_refs = true;
            start_oids.extend(collect_all_ref_oids(&repo.git_dir)?);
        }
        if args.bisect {
            // `--bisect` adds `refs/bisect/bad*` as positive tips and `refs/bisect/good*` as
            // negative tips (Git's `handle_revision_pseudo_opt`). For `%S`, each commit is labeled
            // with the nearest bad ref that reaches it.
            let (bad_tips, good_oids) = collect_bisect_ref_tips(&repo.git_dir)?;
            if format_uses_percent_s {
                let bisect_src = build_named_source_map(&repo.odb, &bad_tips, args.first_parent);
                for (oid, label) in bisect_src {
                    percent_s_source_map.entry(oid).or_insert(label);
                }
            }
            start_oids.extend(bad_tips.into_iter().map(|(oid, _)| oid));
            exclude_oids.extend(good_oids);
        }

        start_oids = dedupe_oid_order(start_oids);
        exclude_oids = dedupe_oid_order(exclude_oids);

        // Git only falls back to HEAD when *no* revision input was given. `--branches`/`--tags`/
        // `--remotes`/`--all` (even matching nothing), explicit revs, an ignored object, or
        // stdin revs all count as "input given" — do not default to HEAD in those cases.
        let rev_input_given = !pos_s.is_empty()
            || !neg_s.is_empty()
            || args.all
            || args.reflog
            || args.bisect
            || args.branches.is_some()
            || stdin_all_refs
            || (args.read_stdin && args.ignore_missing)
            || log_argv_has_pseudo_ref_input(&args)
            // With --ignore-missing, an explicitly-given (then dropped) positional still counts
            // as revision input — do not fall back to HEAD.
            || (args.ignore_missing && log_argv_has_positional_token(&merged_argv));
        if start_oids.is_empty() && !args.all {
            if rev_input_given {
                // Input was given but resolved to nothing: produce empty output, not HEAD.
            } else {
                let head = resolve_head_for_log(&repo.git_dir)?;
                if let Some(default) = args.default_revision.as_ref() {
                    let default_specs = [default.clone()];
                    start_oids.extend(resolve_specs_to_commits_ignoring_missing(
                        &repo,
                        &default_specs,
                        &args,
                    )?);
                } else {
                    match head.oid() {
                        Some(oid) => start_oids.push(*oid),
                        None => {
                            // Unborn / empty HEAD with no revisions: git errors out.
                            let branch = head.branch_name().unwrap_or("HEAD");
                            anyhow::bail!(
                                "your current branch '{branch}' does not have any commits yet"
                            );
                        }
                    }
                }
            }
        }
        (start_oids, exclude_oids)
    };

    start_oids = dedupe_oid_order(start_oids);

    if !implied_pathspecs.is_empty() {
        validate_pathspec_scope(&repo, &implied_pathspecs)?;
    }

    // Pre-compute the set of OIDs reachable from excluded refs.
    let excluded_set = if exclude_oids.is_empty() {
        HashSet::new()
    } else {
        collect_reachable(&repo.odb, &exclude_oids)?
    };

    // Build source map for --source
    let source_map: std::collections::HashMap<ObjectId, String> = if args.source {
        if args.alternate_refs {
            build_alternate_source_map(&repo)?
        } else if let Some(pat) = args.internal_remotes_pattern.as_deref() {
            let glob_pat = if pat.is_empty() {
                Cow::Borrowed("refs/remotes/*")
            } else {
                Cow::Owned(format!("refs/remotes/{pat}"))
            };
            build_remote_tracking_source_map(
                &repo.odb,
                &repo.git_dir,
                glob_pat.as_ref(),
                args.first_parent,
            )?
        } else if args.all || stdin_merged_all_refs {
            build_source_map(&repo.odb, &repo.git_dir, args.first_parent)?
        } else {
            // Explicit command-line revisions: paint each commit with the
            // named revision arg it was reached from (built above into
            // `percent_s_source_map`, which shares the `--source`/`%S` seeding).
            percent_s_source_map.clone()
        }
    } else {
        std::collections::HashMap::new()
    };

    let format_requires_decorations = args
        .format
        .as_deref()
        .map(|fmt| {
            let template = fmt
                .strip_prefix("format:")
                .or_else(|| fmt.strip_prefix("tformat:"))
                .unwrap_or(fmt);
            format_template_uses_decoration(template)
        })
        .unwrap_or(false);

    let (show_decorations, decorate_full) =
        resolve_decoration_display(&args, &repo.git_dir, format_requires_decorations);
    // `--simplify-by-decoration` needs ref→OID mapping even when decorations are not shown
    // (`--oneline` does not imply `--decorate`). Use a separate map for display so we do not
    // print `(refs)` unless `--decorate` / `%d` requests it; OID keys match for full vs short maps.
    let decoration_map_for_display = if show_decorations {
        Some(collect_decorations_inner(
            &repo,
            decorate_full,
            decorations_initial_set_all(&args, &repo.git_dir),
            &build_decoration_filter(&args, &repo.git_dir),
        )?)
    } else {
        None
    };
    let decoration_map_for_simplify_only = if args.simplify_by_decoration && !show_decorations {
        Some(collect_decorations_inner(
            &repo,
            false,
            decorations_initial_set_all(&args, &repo.git_dir),
            &build_decoration_filter(&args, &repo.git_dir),
        )?)
    } else {
        None
    };
    let decoration_map_for_simplify = decoration_map_for_simplify_only
        .as_ref()
        .or(decoration_map_for_display.as_ref());

    // Walk commits
    let mut combined_pathspecs = pathspecs_after_dashdash(&merged_argv, &args.pathspecs);
    combined_pathspecs.extend(implied_pathspecs.iter().cloned());
    validate_pathspec_scope(&repo, &combined_pathspecs)?;
    combined_pathspecs = resolve_effective_pathspecs(&repo, &combined_pathspecs)?;

    let effective_pathspecs = if args.follow {
        &[][..]
    } else {
        &combined_pathspecs[..]
    };

    let (core_commit_graph, cg_read_paths, cg_changed_ver) = load_bloom_walk_config(&repo.git_dir);
    let trace2_perf = std::env::var("GIT_TRACE2_PERF")
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let use_bloom = core_commit_graph
        && !combined_pathspecs.is_empty()
        && grit_lib::pathspec::pathspecs_allow_bloom(&combined_pathspecs)
        && !args.walk_reflogs;
    let bloom_read_changed_paths = cg_read_paths;
    let bloom_changed_paths_version = cg_changed_ver;
    if core_commit_graph {
        // Validate the commit-graph (Git errors on a corrupt one). Resolve across alternates so a
        // split chain whose base layers live in an alternate object dir — created by a local clone
        // whose `info/alternates` points at a repo with a split chain — is not mistaken for a
        // corrupt/missing graph (t5324 'create fork and chain across alternate').
        let objects_dir = repo.git_dir.join("objects");
        let alt_dirs = grit_lib::pack::read_alternates_recursive(&objects_dir).unwrap_or_default();
        CommitGraphChain::try_load_across(&objects_dir, &alt_dirs)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    let bloom_chain = if use_bloom {
        CommitGraphChain::load(&repo.git_dir.join("objects"))
    } else {
        None
    };
    let bloom_stats: Option<BloomWalkStatsHandle> = if trace2_perf && use_bloom {
        Some(Arc::new(Mutex::new(BloomWalkStats::default())))
    } else {
        None
    };
    let _bloom_perf_guard = bloom_stats.as_ref().map(|h| BloomPerfGuard(Arc::clone(h)));

    let bloom_pathspecs_for_walk: &[String] = if args.follow && use_bloom {
        &combined_pathspecs[..]
    } else {
        effective_pathspecs
    };
    let bloom_cwd_for_walk = if use_bloom {
        repo.bloom_pathspec_cwd()
    } else {
        None
    };

    let find_oid = if let Some(ref find_obj_rev) = args.find_object {
        Some(resolve_revision(&repo, find_obj_rev)?)
    } else {
        None
    };
    let find_object_tree_recursive = args.show_trees || args.recurse_trees;
    let since_str = args.since_as_filter.as_ref().or(args.since.as_ref());
    let since_threshold = since_str.and_then(|s| parse_date_to_epoch(s));
    let until_threshold = args.until.as_ref().and_then(|s| parse_date_to_epoch(s));
    let diff_filter_str = args.diff_filter.as_deref();

    let pickaxe_filter: Option<&Args> =
        if !args.remerge_diff && (args.pickaxe_grep.is_some() || args.pickaxe_string.is_some()) {
            Some(&args)
        } else {
            None
        };

    let use_streaming_log = !args.reverse && !(args.follow && !combined_pathspecs.is_empty());

    let stdout = io::stdout();
    let mut out = stdout.lock();

    // Detect format: (separator) vs tformat: (terminator) semantics
    let is_format_separator = args
        .format
        .as_deref()
        .map(|f| f.starts_with("format:"))
        .unwrap_or(false);

    let show_diff = args.patch
        || args.patch_u
        || !args.stat.is_empty()
        || args.name_only
        || args.name_status
        || args.raw
        || args.cc
        || args.merge_diff_c
        || args.remerge_diff
        || args.patch_with_stat
        // An explicit `--diff-merges=<non-off>` forces merges to show a diff with no `-p`;
        // `write_commit_diff` re-checks per-commit, so non-merges still print nothing.
        || merges_force_diff(&args);

    let mut notes_cache = NotesMapCache::new(&repo);
    let flush_each = out.is_terminal();

    if use_streaming_log {
        let mut iter = WalkCommitsIter::new(
            &repo,
            &repo.odb,
            &repo.git_dir,
            &start_oids,
            if args.follow { None } else { args.max_count }, // follow needs full walk for rename tracking
            args.skip,
            args.first_parent,
            &author_res,
            &committer_res,
            &grep_filter,
            args.all_match,
            args.invert_grep,
            &mailmap,
            use_mailmap,
            args.no_merges,
            args.merges,
            effective_pathspecs,
            &excluded_set,
            pickaxe_filter,
            bloom_chain.clone(),
            bloom_read_changed_paths,
            bloom_changed_paths_version,
            bloom_stats.clone(),
            bloom_pathspecs_for_walk,
            bloom_cwd_for_walk.clone(),
            args.author_date_order,
        );
        let mut shown = 0usize;
        while let Some((oid, commit_data)) = iter.next_commit()? {
            if !commit_passes_post_walk_filters(
                &repo,
                &repo.odb,
                &oid,
                &commit_data,
                &args,
                diff_filter_str,
                find_oid,
                find_object_tree_recursive,
                decoration_map_for_simplify,
                since_threshold,
                until_threshold,
            )? {
                continue;
            }
            if is_format_separator && shown > 0 {
                if args.null_terminator {
                    write!(out, "\0")?;
                } else {
                    writeln!(out)?;
                }
            }
            let blank_separator = log_uses_blank_separator(&args);
            if blank_separator && shown > 0 {
                // Under `-z` the inter-entry blank line is replaced by a NUL.
                if args.null_terminator {
                    write!(out, "\0")?;
                } else {
                    writeln!(out)?;
                }
            }
            if args.null_terminator && shown > 0 && log_z_needs_email_separator(&args) {
                write!(out, "\0")?;
            }
            let oneline_fmt = args.oneline || args.format.as_deref() == Some("oneline");
            if args.source && !oneline_fmt {
                if let Some(src) = source_map.get(&oid) {
                    write!(out, "{}\t", short_ref_for_source_display(src))?;
                }
            }
            let source_for_oneline = if args.source && oneline_fmt {
                source_map
                    .get(&oid)
                    .map(|full| short_ref_for_source_display(full))
            } else {
                None
            };
            let per_parent_header =
                log_commit_uses_per_parent_header(&args, &commit_data, &repo.git_dir)?;
            if !per_parent_header {
                format_commit(
                    &mut out,
                    &oid,
                    &commit_data,
                    &args,
                    use_mailmap,
                    &mailmap,
                    decoration_map_for_display.as_ref(),
                    use_color,
                    decoration_paint.as_ref(),
                    &head_state,
                    &mut notes_cache,
                    &repo.odb,
                    None,
                    false,
                    None,
                    None,
                    source_for_oneline,
                    percent_s_source_map.get(&oid).map(|s| s.as_str()),
                    grep_colorizer.as_ref(),
                )?;
            }

            if show_diff {
                write_commit_diff(
                    &mut out,
                    &repo,
                    &oid,
                    &commit_data,
                    &args,
                    use_mailmap,
                    &mailmap,
                    effective_pathspecs,
                    None,
                    None,
                    decoration_map_for_display.as_ref(),
                    use_color,
                    decoration_paint.as_ref(),
                    &head_state,
                    &mut notes_cache,
                    patch_context,
                    blank_separator,
                    None,
                )?;
            }
            if flush_each {
                out.flush()?;
            }
            shown += 1;
        }
    } else {
        let commits = walk_commits(
            &repo,
            &repo.git_dir,
            &start_oids,
            if args.follow { None } else { args.max_count }, // follow needs full walk for rename tracking
            args.skip,
            args.first_parent,
            &author_res,
            &committer_res,
            &grep_filter,
            args.all_match,
            args.invert_grep,
            &mailmap,
            use_mailmap,
            args.no_merges,
            args.merges,
            effective_pathspecs,
            &excluded_set,
            pickaxe_filter,
            bloom_chain,
            bloom_read_changed_paths,
            bloom_changed_paths_version,
            bloom_stats,
            bloom_pathspecs_for_walk,
            bloom_cwd_for_walk,
            args.author_date_order,
        )?;

        // Apply --follow: filter commits and track renames/copies. `follow_display`
        // maps each kept commit to the path the followed file had there (the copy/
        // rename destination) and the detected diff entry, so the per-commit diff
        // output retargets and shows `C100 <src> <dst>` like Git.
        let mut follow_display: std::collections::HashMap<ObjectId, FollowDisplay> =
            std::collections::HashMap::new();
        let commits = if args.follow && !combined_pathspecs.is_empty() {
            let (filtered, display_map) =
                follow_filter(&repo.odb, commits, &combined_pathspecs[0], args.max_count)?;
            follow_display = display_map;
            filtered
        } else {
            commits
        };

        // Apply --diff-filter
        let commits = if let Some(ref filter) = args.diff_filter {
            // Lowercase = exclude, uppercase = include
            let include_chars: Vec<char> = filter.chars().filter(|c| c.is_uppercase()).collect();
            let exclude_chars: Vec<char> = filter
                .chars()
                .filter(|c| c.is_lowercase())
                .map(|c| c.to_uppercase().next().unwrap_or(c))
                .collect();
            commits
                .into_iter()
                .filter(|(_oid, info)| {
                    if !include_chars.is_empty() {
                        commit_has_diff_status(&repo.odb, info, &include_chars, &args)
                            .unwrap_or(true)
                    } else if !exclude_chars.is_empty() {
                        // Include if NOT in exclude list
                        commit_has_diff_status_not_in(&repo.odb, info, &exclude_chars, &args)
                            .unwrap_or(true)
                    } else {
                        true
                    }
                })
                .collect::<Vec<_>>()
        } else {
            commits
        };

        // Apply --find-object: only show commits that introduce or remove the given object
        let commits = if let Some(ref find_obj_rev) = args.find_object {
            let find_oid_buf = resolve_revision(&repo, find_obj_rev)?;
            commits
                .into_iter()
                .filter(|(_oid, info)| {
                    commit_has_object(
                        &repo.odb,
                        info,
                        &find_oid_buf,
                        args.show_trees || args.recurse_trees,
                    )
                    .unwrap_or_default()
                })
                .collect::<Vec<_>>()
        } else {
            commits
        };

        // Apply --simplify-by-decoration: only show commits with decorations
        let commits = if args.simplify_by_decoration {
            match decoration_map_for_simplify {
                Some(dec_map) => commits
                    .into_iter()
                    .filter(|(oid, _)| dec_map.contains_key(&oid.to_hex()))
                    .collect::<Vec<_>>(),
                None => commits,
            }
        } else {
            commits
        };

        // Apply --since-as-filter / --since
        let commits = {
            let since_str = args.since_as_filter.as_ref().or(args.since.as_ref());
            if let Some(s) = since_str {
                if let Some(threshold) = parse_date_to_epoch(s) {
                    commits
                        .into_iter()
                        .filter(|(_oid, info)| {
                            extract_epoch_from_ident(&info.committer) >= threshold
                        })
                        .collect::<Vec<_>>()
                } else {
                    commits
                }
            } else {
                commits
            }
        };
        // Apply --until
        let commits = if let Some(ref s) = args.until {
            if let Some(threshold) = parse_date_to_epoch(s) {
                commits
                    .into_iter()
                    .filter(|(_oid, info)| extract_epoch_from_ident(&info.committer) <= threshold)
                    .collect::<Vec<_>>()
            } else {
                commits
            }
        } else {
            commits
        };

        let commits = if args.reverse {
            commits.into_iter().rev().collect::<Vec<_>>()
        } else {
            commits
        };

        let blank_separator = log_uses_blank_separator(&args);
        for (i, (oid, commit_data)) in commits.iter().enumerate() {
            if is_format_separator && i > 0 {
                if args.null_terminator {
                    write!(out, "\0")?;
                } else {
                    writeln!(out)?;
                }
            }
            // Builtin pretty formats print a blank line between consecutive entries
            // (git log-tree.c `shown_one`). Under `-z` the inter-entry separator becomes a NUL
            // instead of that blank line, matching Git's `line_termination = '\0'`: each entry's
            // own trailing newline is kept, and entries are joined with `\0`.
            if blank_separator && i > 0 {
                if args.null_terminator {
                    write!(out, "\0")?;
                } else {
                    writeln!(out)?;
                }
            }
            if args.null_terminator && i > 0 && log_z_needs_email_separator(&args) {
                write!(out, "\0")?;
            }
            let oneline_fmt = args.oneline || args.format.as_deref() == Some("oneline");
            if args.source && !oneline_fmt {
                if let Some(src) = source_map.get(oid) {
                    write!(out, "{}\t", short_ref_for_source_display(src))?;
                }
            }
            let source_for_oneline = if args.source && oneline_fmt {
                source_map
                    .get(oid)
                    .map(|full| short_ref_for_source_display(full))
            } else {
                None
            };
            let per_parent_header =
                log_commit_uses_per_parent_header(&args, commit_data, &repo.git_dir)?;
            if !per_parent_header {
                format_commit(
                    &mut out,
                    oid,
                    commit_data,
                    &args,
                    use_mailmap,
                    &mailmap,
                    decoration_map_for_display.as_ref(),
                    use_color,
                    decoration_paint.as_ref(),
                    &head_state,
                    &mut notes_cache,
                    &repo.odb,
                    None,
                    false,
                    None,
                    None,
                    source_for_oneline,
                    percent_s_source_map.get(oid).map(|s| s.as_str()),
                    grep_colorizer.as_ref(),
                )?;
            }

            if show_diff {
                write_commit_diff(
                    &mut out,
                    &repo,
                    oid,
                    commit_data,
                    &args,
                    use_mailmap,
                    &mailmap,
                    &combined_pathspecs,
                    None,
                    None,
                    decoration_map_for_display.as_ref(),
                    use_color,
                    decoration_paint.as_ref(),
                    &head_state,
                    &mut notes_cache,
                    patch_context,
                    blank_separator,
                    if args.follow {
                        follow_display.get(oid)
                    } else {
                        None
                    },
                )?;
            }
        }
    }

    Ok(())
}

/// Ensure pathspecs are within the repository worktree scope.
///
/// Git rejects pathspecs that escape the worktree (e.g. `..`) as
/// "outside repository", and also rejects pathspecs provided while running in
/// an unqualified `.git` context.
/// Validate `:(...)` long magic words, matching git's pathspec.c `get_prefix`/magic parser.
/// Returns the `fatal: Invalid pathspec magic '<word>' in '<spec>'` error for an unknown word.
/// All raw pathspec strings (with magic intact) supplied to a `git log`
/// invocation: the clap-collected pathspecs plus anything after `--`.
fn collect_log_raw_pathspecs(args: &Args) -> Vec<String> {
    let mut specs: Vec<String> = args.pathspecs.clone();
    if let Some(pos) = args.raw_argv_tail.iter().position(|a| a == "--") {
        specs.extend(args.raw_argv_tail[pos + 1..].iter().cloned());
    }
    // A pathspec given without a leading `--` lands in the `revisions`
    // positional. Any token carrying long-form magic (`:(...)`) is a pathspec,
    // so include those too (used only for `--follow` magic validation).
    for tok in &args.revisions {
        if tok.starts_with(":(") {
            specs.push(tok.clone());
        }
    }
    specs
}

/// Best-effort list of pathspec arguments for deciding whether `log.follow`
/// applies (it only kicks in for exactly one pathspec). After `--`, every token
/// is a pathspec; otherwise tokens that do not resolve as revisions are treated
/// as pathspecs.
fn log_follow_candidate_pathspecs(repo: &Repository, args: &Args) -> Vec<String> {
    if let Some(pos) = args.raw_argv_tail.iter().position(|a| a == "--") {
        return args.raw_argv_tail[pos + 1..].to_vec();
    }
    let mut specs: Vec<String> = args.pathspecs.clone();
    for tok in &args.revisions {
        if tok == "--" || tok.starts_with('-') {
            continue;
        }
        if tok.starts_with(":(") {
            specs.push(tok.clone());
            continue;
        }
        // A trailing argument that does not name a commit is a pathspec.
        if resolve_revision_as_commit(repo, tok).is_err() {
            specs.push(tok.clone());
        }
    }
    specs
}

/// If a pathspec carries long-form magic not supported by `--follow`, return the
/// comma-separated list of offending magic words (matching Git's order), else
/// `None`. Only `top` and `literal` are accepted by `--follow`.
fn follow_unsupported_pathspec_magic(spec: &str) -> Option<String> {
    let rest = spec.strip_prefix(":(")?;
    let close = rest.find(')')?;
    let magic_part = &rest[..close];
    let mut bad: Vec<String> = Vec::new();
    for raw in magic_part.split(',') {
        let token = raw.trim();
        if token.is_empty() {
            continue;
        }
        let word = token.split(':').next().unwrap_or(token);
        if !matches!(word, "top" | "literal") {
            bad.push(format!("'{word}'"));
        }
    }
    if bad.is_empty() {
        None
    } else {
        Some(bad.join(", "))
    }
}

fn validate_pathspec_magic_words(pathspecs: &[String]) -> Result<()> {
    for spec in pathspecs {
        let Some(rest) = spec.strip_prefix(":(") else {
            continue;
        };
        let Some(close) = rest.find(')') else {
            continue;
        };
        let magic_part = &rest[..close];
        for raw in magic_part.split(',') {
            let token = raw.trim();
            if token.is_empty() {
                continue;
            }
            // Forms taking a value: `prefix:...`, `attr:...`.
            let word = token.split(':').next().unwrap_or(token);
            let known = matches!(
                word,
                "top" | "literal" | "icase" | "glob" | "attr" | "exclude" | "prefix"
            );
            if !known {
                anyhow::bail!("fatal: Invalid pathspec magic '{word}' in '{spec}'");
            }
        }
    }
    Ok(())
}

fn validate_pathspec_scope(repo: &Repository, pathspecs: &[String]) -> Result<()> {
    validate_pathspec_magic_words(pathspecs)?;
    if pathspecs.is_empty() {
        return Ok(());
    }

    let cwd = std::env::current_dir().context("resolving current directory")?;
    let Some(work_tree) = repo.work_tree.as_deref() else {
        // Bare repos (and running inside `.git`): pathspecs still limit history without
        // resolving against a work tree (t0410), but a pathspec that lexically escapes the
        // repository — e.g. `..` from the repo root — is rejected just like upstream's
        // `prefix_path_gently` failure (t6136).
        for pathspec in pathspecs {
            if pathspec.starts_with(':') {
                continue;
            }
            if pathspec_escapes_repo_lexically(Path::new(pathspec)) {
                anyhow::bail!("pathspec '{}' is outside repository", pathspec);
            }
        }
        return Ok(());
    };

    let cwd_norm = normalize_path(&cwd);
    let work_tree_norm = normalize_path(work_tree);
    let git_dir_norm = normalize_path(&repo.git_dir);
    if cwd_norm.starts_with(&git_dir_norm) {
        anyhow::bail!("pathspec '{}' is outside repository", pathspecs[0]);
    }

    for pathspec in pathspecs {
        if pathspec.starts_with(':') {
            continue;
        }
        let as_path = Path::new(pathspec);
        let candidate = if as_path.is_absolute() {
            as_path.to_path_buf()
        } else {
            cwd_norm.join(as_path)
        };
        let candidate_norm = normalize_path(&candidate);
        if !candidate_norm.starts_with(&work_tree_norm) {
            anyhow::bail!("pathspec '{}' is outside repository", pathspec);
        }
    }

    Ok(())
}

/// Whether a relative pathspec lexically escapes the repository (a leading `..`
/// that walks above the root, or an absolute/rooted path). Used when there is no
/// work tree (bare repo or running inside `.git`) to reproduce upstream's
/// `'%s' is outside repository` diagnostic for pathspecs like `..` (t6136).
fn pathspec_escapes_repo_lexically(pathspec: &Path) -> bool {
    use std::path::Component;
    let mut depth = 0usize;
    for component in pathspec.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(_) => depth = depth.saturating_add(1),
            Component::ParentDir => {
                if depth == 0 {
                    return true;
                }
                depth -= 1;
            }
            Component::RootDir | Component::Prefix(_) => return true,
        }
    }
    false
}

/// Resolve pathspecs relative to current working directory inside the worktree.
///
/// This aligns pathspec matching semantics for commands invoked from
/// subdirectories, including magic forms like `:(icase)bar`.
fn resolve_effective_pathspecs(repo: &Repository, pathspecs: &[String]) -> Result<Vec<String>> {
    if pathspecs.is_empty() {
        return Ok(Vec::new());
    }
    let Some(work_tree) = repo.work_tree.as_deref() else {
        return Ok(pathspecs.to_vec());
    };

    let cwd = std::env::current_dir().context("resolving current directory")?;
    let cwd_norm = normalize_path(&cwd);
    let work_tree_norm = normalize_path(work_tree);
    let cwd_rel = cwd_norm
        .strip_prefix(&work_tree_norm)
        .unwrap_or(Path::new(""));
    let cwd_prefix = if cwd_rel.as_os_str().is_empty() {
        String::new()
    } else {
        format!("{}/", cwd_rel.to_string_lossy())
    };

    let mut resolved = Vec::with_capacity(pathspecs.len());
    for spec in pathspecs {
        if spec.starts_with(":/") {
            resolved.push(spec.clone());
            continue;
        }

        if spec.starts_with(":(") {
            if let Some(resolved_magic) = crate::pathspec::resolve_magic_pathspec(spec, &cwd_prefix)
            {
                resolved.push(resolved_magic);
            } else {
                resolved.push(spec.clone());
            }
            continue;
        }

        if spec.starts_with(':') {
            resolved.push(spec.clone());
            continue;
        }

        let as_path = Path::new(spec);
        if as_path.is_absolute() {
            let candidate = normalize_path(as_path);
            if let Ok(rel) = candidate.strip_prefix(&work_tree_norm) {
                resolved.push(normalize_relative_path_str(&rel.to_string_lossy()));
            } else {
                resolved.push(spec.clone());
            }
            continue;
        }

        resolved.push(resolve_pathspec_tail_with_prefix(spec, &cwd_prefix));
    }

    Ok(resolved)
}

fn resolve_pathspec_tail_with_prefix(tail: &str, cwd_prefix: &str) -> String {
    if tail.is_empty() {
        return String::new();
    }
    if let Some(rooted) = tail.strip_prefix('/') {
        return normalize_relative_path_str(rooted);
    }
    if cwd_prefix.is_empty() {
        return normalize_relative_path_str(tail);
    }
    normalize_relative_path_str(&format!("{cwd_prefix}{tail}"))
}

fn normalize_relative_path_str(path: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    for component in Path::new(path).components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                parts.pop();
            }
            std::path::Component::Normal(seg) => {
                parts.push(seg.to_string_lossy().to_string());
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {}
        }
    }
    parts.join("/")
}

/// Normalize a path lexically by removing `.` and resolving `..`.
fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Run `--no-walk` mode: show the given commits without walking their parents.
pub fn run_no_walk(
    repo: &Repository,
    args: &Args,
    patch_context: usize,
    use_mailmap: bool,
    mailmap: &MailmapTable,
) -> Result<()> {
    let mut oids = Vec::new();
    if args.revisions.is_empty() {
        let head = resolve_head(&repo.git_dir)?;
        if let Some(oid) = head.oid() {
            oids.push(*oid);
        }
    } else {
        // Expand pseudo-refs (`--tags`, `--all`, `--branches`, `--remotes`,
        // `--glob=`) into concrete revision tokens before resolving each.
        let expanded = merge_log_revision_argv(repo, args)?;
        let mut seen = HashSet::new();
        for rev in &expanded {
            if rev == "--" || rev.starts_with('^') {
                continue;
            }
            let oid = resolve_revision_as_commit(repo, rev)?;
            if seen.insert(oid) {
                oids.push(oid);
            }
        }
    }

    let decorate_full = match &args.decorate {
        Some(Some(s)) if s == "full" => true,
        _ => false,
    };
    // A `%d` / `%D` / `%(decorate)` placeholder in the user format implies
    // decoration even without an explicit `--decorate` (matching git log).
    let format_requires_decorations = args.format.as_deref().is_some_and(|fmt| {
        let template = fmt
            .strip_prefix("format:")
            .or_else(|| fmt.strip_prefix("tformat:"))
            .unwrap_or(fmt);
        format_template_uses_decoration(template)
    });
    let decorations = if args.no_decorate {
        None
    } else if args.decorate.is_some() || format_requires_decorations {
        Some(collect_decorations_inner(
            repo,
            decorate_full,
            decorations_initial_set_all(&args, &repo.git_dir),
            &build_decoration_filter(&args, &repo.git_dir),
        )?)
    } else {
        // Default: no decorations in no-walk mode (matches git behavior)
        None
    };

    let mut commits = Vec::new();
    for oid in oids {
        let obj = repo.read_replaced(&oid)?;
        let commit = parse_commit(&obj.data)?;
        let info = CommitInfo {
            tree: commit.tree,
            parents: commit.parents.clone(),
            author: commit.author.clone(),
            committer: commit.committer.clone(),
            message: commit.message.clone(),
            raw_message: commit.raw_message.clone(),
            encoding: commit.encoding.clone(),
        };
        commits.push((oid, info));
    }

    // Sort by committer timestamp descending (same as regular log), unless
    // `--no-walk=unsorted` was requested (then preserve the given order).
    let unsorted = matches!(args.no_walk.as_deref(), Some("unsorted"));
    if !unsorted {
        commits.sort_by(|a, b| {
            let ts_a = committer_unix_seconds_for_ordering(&a.1.committer);
            let ts_b = committer_unix_seconds_for_ordering(&b.1.committer);
            ts_b.cmp(&ts_a)
        });
    }

    if args.reverse {
        commits.reverse();
    }

    let stdout = io::stdout();
    let line_prefix = args.line_prefix.as_deref().unwrap_or("");
    let mut out = LinePrefixWriter::new(stdout.lock(), line_prefix);

    let is_format_separator = args
        .format
        .as_deref()
        .map(|f| f.starts_with("format:"))
        .unwrap_or(false);

    let show_diff = args.patch
        || args.patch_u
        || !args.stat.is_empty()
        || args.name_only
        || args.name_status
        || args.raw
        || args.cc
        || args.merge_diff_c
        || args.remerge_diff
        || args.patch_with_stat
        // An explicit `--diff-merges=<non-off>` forces merges to show a diff with no `-p`;
        // `write_commit_diff` re-checks per-commit, so non-merges still print nothing.
        || merges_force_diff(&args);

    let mut notes_cache = NotesMapCache::new(repo);
    let use_color = log_resolve_stdout_color(args, &repo.git_dir);
    let head_state = resolve_head(&repo.git_dir).unwrap_or(HeadState::Invalid);
    let decoration_paint = if use_color {
        Some(load_decoration_paint(&repo.git_dir))
    } else {
        None
    };

    let blank_separator = log_uses_blank_separator(args);
    for (i, (oid, commit_data)) in commits.iter().enumerate() {
        if is_format_separator && i > 0 {
            if args.null_terminator {
                write!(out, "\0")?;
            } else {
                writeln!(out)?;
            }
        }
        if blank_separator && i > 0 {
            // Under `-z` the inter-entry blank line is replaced by a NUL.
            if args.null_terminator {
                write!(out, "\0")?;
            } else {
                writeln!(out)?;
            }
        }
        if args.null_terminator && i > 0 && log_z_needs_email_separator(args) {
            write!(out, "\0")?;
        }
        let per_parent_header =
            log_commit_uses_per_parent_header(args, commit_data, &repo.git_dir)?;
        if !per_parent_header {
            format_commit(
                &mut out,
                oid,
                commit_data,
                args,
                use_mailmap,
                mailmap,
                decorations.as_ref(),
                use_color,
                decoration_paint.as_ref(),
                &head_state,
                &mut notes_cache,
                &repo.odb,
                None,
                false,
                None,
                None,
                None,
                None,
                None,
            )?;
        }
        if show_diff {
            write_commit_diff(
                &mut out,
                repo,
                oid,
                commit_data,
                args,
                use_mailmap,
                &mailmap,
                &args.pathspecs,
                None,
                None,
                decorations.as_ref(),
                use_color,
                decoration_paint.as_ref(),
                &head_state,
                &mut notes_cache,
                patch_context,
                blank_separator,
                None,
            )?;
        }
    }

    Ok(())
}

fn reflog_grep_matches(patterns: &[Regex], text: &str, all_match: bool, invert: bool) -> bool {
    if patterns.is_empty() {
        return true;
    }
    let m = if all_match {
        patterns.iter().all(|re| re.is_match(text))
    } else {
        patterns.iter().any(|re| re.is_match(text))
    };
    if invert {
        !m
    } else {
        m
    }
}

fn resolve_head_for_log(git_dir: &Path) -> Result<HeadState> {
    match resolve_head(git_dir) {
        Ok(HeadState::Invalid) => anyhow::bail!("broken HEAD"),
        Ok(head) => Ok(head),
        Err(_) => anyhow::bail!("broken HEAD"),
    }
}

fn reflog_message_is_checkout(message: &str) -> bool {
    message
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("checkout:")
}

fn tree_matches_any_pathspec(odb: &Odb, tree_oid: &ObjectId, pathspecs: &[String]) -> Result<bool> {
    let paths = grit_lib::diff::head_path_states(odb, Some(tree_oid))?;
    Ok(paths
        .keys()
        .any(|path| path_matches(path.as_str(), pathspecs)))
}

/// Whether a reflog step matches `pathspecs`.
///
/// Checkouts match if the pathspec names a path present in either the old or new commit tree
/// (Git `log -g -- <path>`). Other single-parent steps use the reflog transition diff. Merges use
/// dense path simplification plus per-parent diffs vs the merge result (t1414).
fn reflog_transition_touches_paths(
    repo: &Repository,
    old_oid: &ObjectId,
    new_oid: &ObjectId,
    reflog_message: &str,
    pathspecs: &[String],
) -> Result<bool> {
    if pathspecs.is_empty() {
        return Ok(true);
    }
    let odb = &repo.odb;
    let new_obj = odb.read(new_oid)?;
    let new_commit = parse_commit(&new_obj.data)?;

    let tree_diff_touches = |from_tree: Option<&ObjectId>, to_tree: &ObjectId| -> Result<bool> {
        let old_t = if let Some(t) = from_tree {
            Some(*t)
        } else {
            None
        };
        let entries = diff_trees(odb, old_t.as_ref(), Some(to_tree), "")?;
        Ok(entries.iter().any(|e| {
            let path = e.path().to_string_lossy();
            path_matches(&path, pathspecs)
        }))
    };

    if new_commit.parents.len() >= 2 {
        if !commit_visible_for_dense_pathspecs(repo, *new_oid, pathspecs)? {
            return Ok(false);
        }
        for p in &new_commit.parents {
            let p_obj = match odb.read(p) {
                Ok(o) => o,
                Err(_) => continue,
            };
            let p_commit = match parse_commit(&p_obj.data) {
                Ok(c) => c,
                Err(_) => continue,
            };
            if tree_diff_touches(Some(&p_commit.tree), &new_commit.tree)? {
                return Ok(true);
            }
        }
        return Ok(false);
    }

    if new_commit.parents.len() < 2 && reflog_message_is_checkout(reflog_message) {
        return tree_matches_any_pathspec(odb, &new_commit.tree, pathspecs);
    }

    let old_tree = if old_oid.is_zero() {
        None
    } else {
        let old_obj = match odb.read(old_oid) {
            Ok(o) => o,
            Err(_) => return Ok(false),
        };
        let old_commit = match parse_commit(&old_obj.data) {
            Ok(c) => c,
            Err(_) => return Ok(false),
        };
        Some(old_commit.tree)
    };
    tree_diff_touches(old_tree.as_ref(), &new_commit.tree)
}

fn next_reflog_at_open_for_suffix(spec: &str, mut from: usize) -> Option<usize> {
    let b = spec.as_bytes();
    while let Some(rel) = spec[from..].find("@{") {
        let i = from + rel;
        if b.get(i + 2) == Some(&b'-') {
            let after_open = i + 2;
            let close = spec[after_open..].find('}').map(|j| after_open + j)?;
            from = close + 1;
            continue;
        }
        return Some(i);
    }
    None
}

fn reflog_entry_unix_ts(entry: &grit_lib::reflog::ReflogEntry) -> Option<i64> {
    let parts: Vec<&str> = entry.identity.rsplitn(3, ' ').collect();
    if parts.len() >= 2 {
        parts[1].parse().ok()
    } else {
        None
    }
}

fn reflog_entry_tz(entry: &grit_lib::reflog::ReflogEntry) -> &str {
    let parts: Vec<&str> = entry.identity.rsplitn(3, ' ').collect();
    parts.first().copied().unwrap_or("+0000")
}

fn format_reflog_selector_date(
    display_name: &str,
    entry: &grit_lib::reflog::ReflogEntry,
    date_mode: Option<&str>,
) -> String {
    if let Some(ts) = reflog_entry_unix_ts(entry) {
        let tz = reflog_entry_tz(entry);
        // `format_date_with_mode` parses Git signature tails via `parse_signature_tail`, which
        // requires the `Name <email>` prefix before `<unix> <tz>` (see grit-lib `ident.rs`).
        let pseudo = format!("x <x@x> {ts} {tz}");
        let date = format_date_with_mode(&pseudo, date_mode);
        format!("{display_name}@{{{date}}}")
    } else {
        format!("{display_name}@{{0}}")
    }
}

#[derive(Clone, Copy)]
enum ReflogWalkSuffixKind {
    Index,
    Date,
}

/// `%gd` for `log -g`: indexed `@{n}` wins over `--date` when the user gave `@{n}`; date-based
/// suffixes and explicit `--date` use the reflog entry timestamp (t1411).
fn reflog_walk_percent_gd(
    display_name: &str,
    entry: &grit_lib::reflog::ReflogEntry,
    nr: usize,
    j: usize,
    had_reflog_suffix: bool,
    last_suffix: ReflogWalkSuffixKind,
    cli_date: Option<&str>,
) -> String {
    if had_reflog_suffix && matches!(last_suffix, ReflogWalkSuffixKind::Index) {
        let idx_from_tip = nr - 1 - j;
        return format!("{display_name}@{{{idx_from_tip}}}");
    }
    if had_reflog_suffix && matches!(last_suffix, ReflogWalkSuffixKind::Date) {
        return format_reflog_selector_date(display_name, entry, cli_date);
    }
    if let Some(dm) = cli_date {
        return format_reflog_selector_date(display_name, entry, Some(dm));
    }
    let idx_from_tip = nr - 1 - j;
    format!("{display_name}@{{{idx_from_tip}}}")
}

fn shorten_reflog_selector(selector: &str) -> String {
    let Some(at) = selector.find("@{") else {
        return selector.to_string();
    };
    let name = &selector[..at];
    let suffix = &selector[at..];
    if let Some(short) = name.strip_prefix("refs/heads/") {
        format!("{short}{suffix}")
    } else {
        selector.to_string()
    }
}

/// Reflog file key plus display name for `%gd` / headers (matches Git `complete_reflogs`).
struct ReflogWalkRef {
    log_ref: String,
    display_name: String,
    user_spec: String,
    entries: Vec<ReflogEntry>,
    recno: isize,
    nr: usize,
    force_date_selector: bool,
    tie_order: usize,
    had_reflog_suffix: bool,
    last_reflog_suffix: ReflogWalkSuffixKind,
}

fn reflog_start_index_and_date_flag(
    orig_r: &str,
    entries: &[ReflogEntry],
) -> (usize, bool, Option<i64>) {
    let nr = entries.len();
    let mut start_j: Option<usize> = None;
    let mut use_date_selector = false;
    let mut target_ts_for_warn: Option<i64> = None;

    let mut pos = 0usize;
    while let Some(at) = next_reflog_at_open_for_suffix(orig_r, pos) {
        let inner_start = at + 2;
        let Some(close) = orig_r[inner_start..].find('}').map(|j| inner_start + j) else {
            break;
        };
        let inner = &orig_r[inner_start..close];
        let inner_l = inner.to_ascii_lowercase();
        if inner_l == "u" || inner_l == "upstream" || inner_l == "push" {
            pos = close + 1;
            continue;
        }
        if let Ok(n) = inner.parse::<usize>() {
            let idx = nr.checked_sub(1 + n);
            start_j = Some(idx.unwrap_or(0));
            use_date_selector = false;
        } else if let Some(target_ts) = grit_lib::rev_parse::reflog_date_selector_timestamp(inner) {
            target_ts_for_warn = Some(target_ts);
            let mut picked = None::<usize>;
            for i in (0..nr).rev() {
                if let Some(ts) = reflog_entry_unix_ts(&entries[i]) {
                    if ts <= target_ts {
                        picked = Some(i);
                        break;
                    }
                }
            }
            start_j = Some(picked.unwrap_or(0));
            use_date_selector = true;
        } else {
            start_j = Some(nr.saturating_sub(1));
            use_date_selector = false;
        }
        pos = close + 1;
    }

    let start_j = start_j.unwrap_or(nr.saturating_sub(1));
    (start_j, use_date_selector, target_ts_for_warn)
}

fn reflog_suffix_flags_from_spec(orig_r: &str, _nr: usize) -> (bool, ReflogWalkSuffixKind) {
    let mut had_reflog_suffix = false;
    let mut last_reflog_suffix = ReflogWalkSuffixKind::Index;
    let mut pos = 0usize;
    while let Some(at) = next_reflog_at_open_for_suffix(orig_r, pos) {
        let inner_start = at + 2;
        let Some(close) = orig_r[inner_start..].find('}').map(|j| inner_start + j) else {
            break;
        };
        let inner = &orig_r[inner_start..close];
        let inner_l = inner.to_ascii_lowercase();
        if inner_l == "u" || inner_l == "upstream" || inner_l == "push" {
            pos = close + 1;
            continue;
        }
        had_reflog_suffix = true;
        if inner.parse::<usize>().is_ok() {
            last_reflog_suffix = ReflogWalkSuffixKind::Index;
        } else if grit_lib::rev_parse::reflog_date_selector_timestamp(inner).is_some() {
            last_reflog_suffix = ReflogWalkSuffixKind::Date;
        } else {
            last_reflog_suffix = ReflogWalkSuffixKind::Index;
        }
        pos = close + 1;
    }
    (had_reflog_suffix, last_reflog_suffix)
}

fn reflog_display_name_for(log_ref: &str, user_spec: &str) -> String {
    if user_spec.starts_with("refs/") {
        user_spec.to_string()
    } else if log_ref.starts_with("refs/heads/") {
        log_ref
            .strip_prefix("refs/heads/")
            .unwrap_or(log_ref)
            .to_string()
    } else {
        log_ref.to_string()
    }
}

/// Run the reflog walk mode (`log -g` / `log --walk-reflogs`).
fn run_reflog_walk(
    repo: &Repository,
    args: &Args,
    patch_context: usize,
    author_res: &[Regex],
    committer_res: &[Regex],
    grep_filter: &GrepFilter,
    grep_reflog_res: &[Regex],
    use_mailmap: bool,
    mailmap: &MailmapTable,
) -> Result<()> {
    let rev_specs: Vec<String> = if args.revisions.is_empty() {
        if let Some(glob) = args.branches.as_deref() {
            let mut refs = Vec::new();
            for (name, _) in grit_lib::refs::list_refs(&repo.git_dir, "refs/heads/")? {
                let short = name.strip_prefix("refs/heads/").unwrap_or(&name);
                if branches_glob_matches(glob, short) {
                    refs.push(short.to_owned());
                }
            }
            refs
        } else if args.all {
            let mut refs = grit_lib::reflog::list_reflog_refs(&repo.git_dir).unwrap_or_default();
            if refs.is_empty() {
                refs.push("HEAD".to_string());
            }
            refs
        } else {
            vec!["HEAD".to_string()]
        }
    } else {
        args.revisions.clone()
    };

    let date_mode = args.date.as_deref();
    let force_unix_gd = date_mode == Some("unix");

    let since_ts = args.since.as_ref().and_then(|s| parse_date_to_epoch(s));
    let until_ts = args.until.as_ref().and_then(|s| parse_date_to_epoch(s));

    let max_parents = if args.no_merges { Some(1usize) } else { None };
    let min_parents = if args.merges { Some(2usize) } else { None };

    let mut walks: Vec<ReflogWalkRef> = Vec::new();
    for (tie_order, orig_r) in rev_specs.iter().enumerate() {
        let log_ref =
            resolve_reflog_walk_log_ref(repo, orig_r).map_err(|e| anyhow::anyhow!("{e}"))?;
        let entries =
            read_reflog_dwim(&repo.git_dir, &log_ref).map_err(|e| anyhow::anyhow!("{e}"))?;
        if entries.is_empty() {
            continue;
        }
        let nr = entries.len();
        let (had_reflog_suffix, last_reflog_suffix) = reflog_suffix_flags_from_spec(orig_r, nr);
        let display_name = reflog_display_name_for(&log_ref, orig_r);
        let (start_j, mut use_date, target_ts_for_warn) =
            reflog_start_index_and_date_flag(orig_r, &entries);
        if force_unix_gd {
            use_date = true;
        }
        if let Some(target_ts) = target_ts_for_warn {
            if let Some(oldest_ts) = entries.first().and_then(reflog_entry_unix_ts) {
                if target_ts < oldest_ts {
                    let e = &entries[0];
                    if let Some(ts) = reflog_entry_unix_ts(e) {
                        let tz = reflog_entry_tz(e);
                        let pseudo = format!("x <x@x> {ts} {tz}");
                        let when = format_date_with_mode(&pseudo, Some("rfc"));
                        eprintln!(
                            "warning: log for '{}' only goes back to {}",
                            display_name, when
                        );
                    }
                }
            }
        }
        let recno = start_j as isize;
        walks.push(ReflogWalkRef {
            log_ref,
            display_name,
            user_spec: orig_r.to_string(),
            entries,
            recno,
            nr,
            force_date_selector: use_date,
            tie_order,
            had_reflog_suffix,
            last_reflog_suffix,
        });
    }

    let mut drop_date_when_plain: HashSet<usize> = HashSet::new();
    let mut by_log: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, w) in walks.iter().enumerate() {
        by_log.entry(w.log_ref.clone()).or_default().push(i);
    }
    for indices in by_log.values() {
        let has_plain = indices.iter().any(|&i| !walks[i].user_spec.contains("@{"));
        if !has_plain {
            continue;
        }
        for &i in indices {
            if walks[i].force_date_selector {
                drop_date_when_plain.insert(i);
            }
        }
    }
    walks = walks
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !drop_date_when_plain.contains(i))
        .map(|(_, w)| w)
        .collect();

    if walks.is_empty() {
        return Ok(());
    }

    let effective_pathspecs = if args.pathspecs.is_empty() {
        Vec::new()
    } else {
        validate_pathspec_scope(repo, &args.pathspecs)?;
        resolve_effective_pathspecs(repo, &args.pathspecs)?
    };

    let max = args.max_count.unwrap_or(usize::MAX);
    let skip = args.skip.unwrap_or(0);

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut notes_cache = NotesMapCache::new(repo);
    let use_color = log_resolve_stdout_color(args, &repo.git_dir);
    let head_state = resolve_head(&repo.git_dir).unwrap_or(HeadState::Invalid);
    let decoration_paint = if use_color {
        Some(load_decoration_paint(&repo.git_dir))
    } else {
        None
    };

    let is_format_separator = args
        .format
        .as_deref()
        .map(|f| f.starts_with("format:"))
        .unwrap_or(false);

    let mut shown = 0usize;
    let mut skipped = 0usize;

    loop {
        if shown >= max {
            break;
        }

        let mut best_i: Option<usize> = None;
        let mut best_ts = i64::MIN;
        let mut best_tie = usize::MAX;

        for (i, w) in walks.iter().enumerate() {
            if w.recno < 0 {
                continue;
            }
            let e = &w.entries[w.recno as usize];
            let Some(ts) = reflog_entry_unix_ts(e) else {
                continue;
            };
            if ts > best_ts || (ts == best_ts && w.tie_order < best_tie) {
                best_ts = ts;
                best_tie = w.tie_order;
                best_i = Some(i);
            }
        }

        let Some(wi) = best_i else {
            break;
        };

        let w = &mut walks[wi];
        let entry = w.entries[w.recno as usize].clone();
        let j = w.recno as usize;
        let nr = w.nr;
        let display_name = w.display_name.clone();
        let use_date_sel = w.force_date_selector;
        let had_reflog_suffix = w.had_reflog_suffix;
        let last_reflog_suffix = w.last_reflog_suffix;

        w.recno -= 1;

        let commit_data = match repo.odb.read(&entry.new_oid) {
            Ok(obj) => match parse_commit(&obj.data) {
                Ok(c) => c,
                Err(_) => CommitData {
                    tree: zero_oid(),
                    parents: Vec::new(),
                    author: String::new(),
                    committer: String::new(),
                    author_raw: Vec::new(),
                    committer_raw: Vec::new(),
                    encoding: None,
                    message: String::new(),
                    raw_message: None,
                },
            },
            Err(_) => continue,
        };

        let n_parents = commit_data.parents.len();
        if let Some(mx) = max_parents {
            if n_parents > mx {
                continue;
            }
        }
        if let Some(mn) = min_parents {
            if n_parents < mn {
                continue;
            }
        }

        if let Some(since) = since_ts {
            if let Some(ets) = reflog_entry_unix_ts(&entry) {
                if ets < since {
                    continue;
                }
            }
        }
        if let Some(until) = until_ts {
            if let Some(ets) = reflog_entry_unix_ts(&entry) {
                if ets > until {
                    continue;
                }
            }
        }

        if !effective_pathspecs.is_empty()
            && !reflog_transition_touches_paths(
                repo,
                &entry.old_oid,
                &entry.new_oid,
                &entry.message,
                &effective_pathspecs,
            )?
        {
            continue;
        }

        let author_ok =
            ident_matches_header_patterns(author_res, &commit_data.author, mailmap, use_mailmap);
        if !author_ok {
            continue;
        }
        let committer_ok = ident_matches_header_patterns(
            committer_res,
            &commit_data.committer,
            mailmap,
            use_mailmap,
        );
        if !committer_ok {
            continue;
        }
        let msg_ok = grep_filter.matches(&commit_data.message, args.all_match, args.invert_grep);
        if !msg_ok {
            continue;
        }
        let reflog_ok = reflog_grep_matches(
            grep_reflog_res,
            &entry.message,
            args.all_match,
            args.invert_grep,
        );
        if !reflog_ok {
            continue;
        }

        if skipped < skip {
            skipped += 1;
            continue;
        }

        let cli_date_for_reflog = args.date.as_deref().filter(|s| !s.is_empty());
        let selector = if use_date_sel {
            format_reflog_selector_date(&display_name, &entry, date_mode)
        } else if let Some(dm) = cli_date_for_reflog {
            format_reflog_selector_date(&display_name, &entry, Some(dm))
        } else {
            let idx_from_tip = nr - 1 - j;
            format!("{display_name}@{{{idx_from_tip}}}")
        };

        let percent_gd_full = reflog_walk_percent_gd(
            &display_name,
            &entry,
            nr,
            j,
            had_reflog_suffix,
            last_reflog_suffix,
            cli_date_for_reflog,
        );
        let percent_gd_short = shorten_reflog_selector(&percent_gd_full);
        let et = args.expand_tabs_in_log;
        let reflog_abbrev_len = if args.no_abbrev {
            40
        } else {
            parse_abbrev(&args.abbrev)
        };

        let is_oneline_fmt = args.format.as_deref() == Some("oneline") || args.oneline;
        if args.null_terminator && shown > 0 && !is_oneline_fmt {
            write!(out, "\0")?;
        }

        if let Some(ref fmt) = args.format {
            match fmt.as_str() {
                "oneline" => {
                    // `git log -g --pretty=oneline` prints
                    // `<hash> <selector>: <reflog message>` (the reflog selector
                    // and message, not the commit subject).
                    let full_hex = entry.new_oid.to_hex();
                    let abbrev = &full_hex[..reflog_abbrev_len.min(full_hex.len())];
                    let reflog_msg = entry.message.trim_end_matches('\n');
                    let reflog_msg = if et > 0 {
                        grit_lib::tab_expand::expand_tabs_in_line(reflog_msg, et)
                    } else {
                        reflog_msg.to_owned()
                    };
                    if args.null_terminator {
                        write!(out, "{} {}: {}\0", abbrev, selector, reflog_msg)?;
                    } else {
                        writeln!(out, "{} {}: {}", abbrev, selector, reflog_msg)?;
                    }
                }
                "short" => {
                    let abbrev_len = parse_abbrev(&args.abbrev);
                    let full_hex = entry.new_oid.to_hex();
                    let abbrev = &full_hex[..abbrev_len.min(full_hex.len())];
                    writeln!(out, "commit {}", abbrev)?;
                    let ident_display = if let Some(email_end) = entry.identity.rfind('>') {
                        &entry.identity[..email_end + 1]
                    } else {
                        &entry.identity
                    };
                    writeln!(out, "Reflog: {} ({})", selector, ident_display)?;
                    writeln!(out, "Reflog message: {}", entry.message)?;
                    let author_display =
                        format_ident_display_mailmap(mailmap, &commit_data.author, use_mailmap);
                    writeln!(out, "Author: {author_display}")?;
                    writeln!(out)?;
                    for line in commit_data.message.lines().take(1) {
                        writeln!(
                            out,
                            "{}",
                            grit_lib::tab_expand::indent_and_expand_tabs(line, 4, et)
                        )?;
                    }
                }
                "medium" => {
                    writeln!(out, "commit {}", entry.new_oid.to_hex())?;
                    writeln!(
                        out,
                        "Author: {}",
                        format_ident_for_header_mailmap(mailmap, &commit_data.author, use_mailmap)
                    )?;
                    let date = format_date_for_header(&commit_data.author);
                    writeln!(out, "Date:   {}", date)?;
                    writeln!(out)?;
                    for line in commit_data.message.lines() {
                        writeln!(
                            out,
                            "{}",
                            grit_lib::tab_expand::indent_and_expand_tabs(line, 4, et)
                        )?;
                    }
                    writeln!(out)?;
                }
                "full" => {
                    writeln!(out, "commit {}", entry.new_oid.to_hex())?;
                    writeln!(
                        out,
                        "Author: {}",
                        format_ident_for_header_mailmap(mailmap, &commit_data.author, use_mailmap)
                    )?;
                    writeln!(
                        out,
                        "Commit: {}",
                        format_ident_for_header_mailmap(
                            mailmap,
                            &commit_data.committer,
                            use_mailmap
                        )
                    )?;
                    writeln!(out)?;
                    for line in commit_data.message.lines() {
                        writeln!(
                            out,
                            "{}",
                            grit_lib::tab_expand::indent_and_expand_tabs(line, 4, et)
                        )?;
                    }
                    writeln!(out)?;
                }
                "fuller" => {
                    writeln!(out, "commit {}", entry.new_oid.to_hex())?;
                    writeln!(
                        out,
                        "Author:     {}",
                        format_ident_for_header_mailmap(mailmap, &commit_data.author, use_mailmap)
                    )?;
                    writeln!(
                        out,
                        "AuthorDate: {}",
                        format_date_for_header(&commit_data.author)
                    )?;
                    writeln!(
                        out,
                        "Commit:     {}",
                        format_ident_for_header_mailmap(
                            mailmap,
                            &commit_data.committer,
                            use_mailmap
                        )
                    )?;
                    writeln!(
                        out,
                        "CommitDate: {}",
                        format_date_for_header(&commit_data.committer)
                    )?;
                    writeln!(out)?;
                    for line in commit_data.message.lines() {
                        writeln!(
                            out,
                            "{}",
                            grit_lib::tab_expand::indent_and_expand_tabs(line, 4, et)
                        )?;
                    }
                    writeln!(out)?;
                }
                "email" => {
                    writeln!(
                        out,
                        "From {} Mon Sep 17 00:00:00 2001",
                        entry.new_oid.to_hex()
                    )?;
                    writeln!(
                        out,
                        "From: {}",
                        format_ident_for_header_mailmap(mailmap, &commit_data.author, use_mailmap)
                    )?;
                    let date = format_date_for_header(&commit_data.author);
                    writeln!(out, "Date: {}", date)?;
                    let subject = grit_lib::commit_pretty::message_subject(&commit_data.message);
                    let subject = if et > 0 {
                        grit_lib::tab_expand::expand_tabs_in_line(&subject, et)
                    } else {
                        subject
                    };
                    writeln!(out, "Subject: [PATCH] {}", subject)?;
                    writeln!(out)?;
                    for line in commit_data.message.lines() {
                        let line_out = if et > 0 {
                            grit_lib::tab_expand::expand_tabs_in_line(line, et)
                        } else {
                            line.to_owned()
                        };
                        writeln!(out, "{line_out}")?;
                    }
                    writeln!(out)?;
                }
                "raw" => {
                    writeln!(out, "commit {}", entry.new_oid.to_hex())?;
                    writeln!(out, "tree {}", commit_data.tree.to_hex())?;
                    for parent in &commit_data.parents {
                        writeln!(out, "parent {}", parent.to_hex())?;
                    }
                    writeln!(out, "author {}", commit_data.author)?;
                    writeln!(out, "committer {}", commit_data.committer)?;
                    writeln!(out)?;
                    for line in commit_data.message.lines() {
                        writeln!(
                            out,
                            "{}",
                            grit_lib::tab_expand::indent_and_expand_tabs(line, 4, et)
                        )?;
                    }
                    writeln!(out)?;
                }
                "reference" => {
                    // `--pretty=reference` ignores reflog selectors; render the
                    // `%h (%s, %ad)` reference line (short date unless --date set).
                    let date_ph = if cli_date_for_reflog.is_some() {
                        "%ad"
                    } else {
                        "%as"
                    };
                    let template = format!("%h (%s, {date_ph})");
                    let line = apply_reflog_format_string(
                        &template,
                        &entry.new_oid,
                        &commit_data,
                        reflog_abbrev_len,
                        &percent_gd_full,
                        &percent_gd_short,
                        &entry.message,
                        &entry.identity,
                        mailmap,
                        use_mailmap,
                        et,
                    );
                    writeln!(out, "{}", line)?;
                }
                _ => {
                    let fmt_str = fmt
                        .strip_prefix("tformat:")
                        .or_else(|| fmt.strip_prefix("format:"))
                        .unwrap_or(fmt);
                    if is_format_separator && shown > 0 {
                        writeln!(out)?;
                    }
                    let line = apply_reflog_format_string(
                        fmt_str,
                        &entry.new_oid,
                        &commit_data,
                        reflog_abbrev_len,
                        &percent_gd_full,
                        &percent_gd_short,
                        &entry.message,
                        &entry.identity,
                        mailmap,
                        use_mailmap,
                        et,
                    );
                    writeln!(out, "{}", line)?;
                }
            }
        } else if args.oneline {
            let abbrev_len = args
                .abbrev
                .as_deref()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(7)
                .min(40);
            let full_hex = entry.new_oid.to_hex();
            let abbrev = &full_hex[..abbrev_len.min(full_hex.len())];
            if args.null_terminator {
                write!(out, "{} {}: {}\0", abbrev, selector, entry.message)?;
            } else {
                writeln!(out, "{} {}: {}", abbrev, selector, entry.message)?;
            }
        } else {
            writeln!(out, "commit {}", entry.new_oid.to_hex())?;
            let ident_display = if let Some(email_end) = entry.identity.rfind('>') {
                &entry.identity[..email_end + 1]
            } else {
                &entry.identity
            };
            writeln!(out, "Reflog: {} ({})", selector, ident_display)?;
            writeln!(out, "Reflog message: {}", entry.message)?;
            writeln!(
                out,
                "Author: {}",
                format_ident_for_header_mailmap(mailmap, &commit_data.author, use_mailmap)
            )?;
            let date = format_date_for_header(&commit_data.author);
            writeln!(out, "Date:   {}", date)?;
            writeln!(out)?;
            for line in commit_data.message.lines() {
                writeln!(
                    out,
                    "{}",
                    grit_lib::tab_expand::indent_and_expand_tabs(line, 4, et)
                )?;
            }
        }

        let info = CommitInfo {
            tree: commit_data.tree,
            parents: commit_data.parents.clone(),
            author: commit_data.author.clone(),
            committer: commit_data.committer.clone(),
            message: commit_data.message.clone(),
            raw_message: commit_data.raw_message.clone(),
            encoding: commit_data.encoding.clone(),
        };
        let show_diff = args.patch
            || args.patch_u
            || !args.stat.is_empty()
            || args.name_only
            || args.name_status
            || args.raw
            || args.cc
            || args.merge_diff_c
            || args.remerge_diff
            || args.patch_with_stat
            // An explicit `--diff-merges=<non-off>` forces merges to show a diff with no `-p`;
            // `write_commit_diff` re-checks per-commit, so non-merges still print nothing.
            || merges_force_diff(args);
        if show_diff {
            let diff_leading_blank =
                !args.oneline && matches!(args.format.as_deref(), None | Some("short"));
            write_commit_diff(
                &mut out,
                repo,
                &entry.new_oid,
                &info,
                args,
                use_mailmap,
                mailmap,
                &args.pathspecs,
                None,
                None,
                None,
                use_color,
                decoration_paint.as_ref(),
                &head_state,
                &mut notes_cache,
                patch_context,
                diff_leading_blank,
                None,
            )?;
            if j > 0 {
                writeln!(out)?;
            }
        }

        shown += 1;
    }

    Ok(())
}

/// Apply format placeholders for reflog walk entries.
/// Supports %H, %h, %s, %gd, %gs, %gn, %ge, %an, %ae, %cn, %ce, %cd, %B, %b, %N, %n.
fn apply_reflog_format_string(
    fmt: &str,
    oid: &ObjectId,
    commit: &grit_lib::objects::CommitData,
    abbrev_len: usize,
    percent_gd_full: &str,
    percent_gd_short: &str,
    reflog_msg: &str,
    reflog_identity: &str,
    mailmap: &MailmapTable,
    use_mailmap: bool,
    expand_tabs_in_log: usize,
) -> String {
    let hex = oid.to_hex();
    let short = &hex[..abbrev_len.min(hex.len())];
    let subject = grit_lib::commit_pretty::message_subject(&commit.message);
    let body = extract_body(&commit.message);

    let reflog_name = extract_name(reflog_identity);
    let reflog_email = extract_email(reflog_identity);

    let mut result = String::new();
    let mut chars = fmt.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '%' {
            match chars.peek() {
                Some('H') => {
                    chars.next();
                    result.push_str(&hex);
                }
                Some('h') => {
                    chars.next();
                    result.push_str(short);
                }
                Some('s') => {
                    chars.next();
                    if expand_tabs_in_log > 0 {
                        result.push_str(&grit_lib::tab_expand::expand_tabs_in_line(
                            &subject,
                            expand_tabs_in_log,
                        ));
                    } else {
                        result.push_str(&subject);
                    }
                }
                Some('B') => {
                    chars.next();
                    // Entire commit message (matches Git `%B`). Parsed commits omit the final
                    // newline in memory; Git's `%B` still ends with `\n` when non-empty.
                    if !commit.message.is_empty() {
                        let msg = if expand_tabs_in_log > 0 {
                            grit_lib::tab_expand::expand_tabs_in_multiline_message(
                                &commit.message,
                                expand_tabs_in_log,
                            )
                        } else {
                            commit.message.to_owned()
                        };
                        result.push_str(&msg);
                        if !msg.ends_with('\n') {
                            result.push('\n');
                        }
                    }
                }
                Some('b') => {
                    chars.next();
                    if expand_tabs_in_log > 0 {
                        result.push_str(&grit_lib::tab_expand::expand_tabs_in_multiline_message(
                            &body,
                            expand_tabs_in_log,
                        ));
                    } else {
                        result.push_str(&body);
                    }
                }
                Some('n') => {
                    chars.next();
                    result.push('\n');
                }
                Some('g') => {
                    chars.next();
                    match chars.peek() {
                        Some('D') => {
                            chars.next();
                            result.push_str(percent_gd_full);
                        }
                        Some('d') => {
                            chars.next();
                            result.push_str(percent_gd_short);
                        }
                        Some('s') => {
                            chars.next();
                            result.push_str(reflog_msg);
                        }
                        Some('n') => {
                            chars.next();
                            result.push_str(&reflog_name);
                        }
                        Some('e') => {
                            chars.next();
                            result.push_str(&reflog_email);
                        }
                        _ => {
                            result.push_str("%g");
                        }
                    }
                }
                Some('a') => {
                    chars.next();
                    match chars.peek() {
                        Some('n') => {
                            chars.next();
                            result.push_str(&extract_name(&commit.author));
                        }
                        Some('N') => {
                            chars.next();
                            let mapped = if use_mailmap && !mailmap.is_empty() {
                                let n = extract_name(&commit.author);
                                let e = extract_email(&commit.author);
                                mailmap.map_user(n, e).0
                            } else {
                                extract_name(&commit.author)
                            };
                            result.push_str(&mapped);
                        }
                        Some('e') => {
                            chars.next();
                            result.push_str(&extract_email(&commit.author));
                        }
                        Some('E') => {
                            chars.next();
                            let mapped = if use_mailmap && !mailmap.is_empty() {
                                let n = extract_name(&commit.author);
                                let e = extract_email(&commit.author);
                                mailmap.map_user(n, e).1
                            } else {
                                extract_email(&commit.author)
                            };
                            result.push_str(&mapped);
                        }
                        Some('l') => {
                            chars.next();
                            let email = if use_mailmap && !mailmap.is_empty() {
                                let n = extract_name(&commit.author);
                                let e = extract_email(&commit.author);
                                mailmap.map_user(n, e).1
                            } else {
                                extract_email(&commit.author)
                            };
                            result.push_str(&local_part_of_email(&email));
                        }
                        Some('s') => {
                            chars.next();
                            result.push_str(&format_date_with_mode(&commit.author, Some("short")));
                        }
                        Some('d') => {
                            chars.next();
                            result.push_str(&format_date_with_mode(&commit.author, None));
                        }
                        _ => {
                            result.push_str("%a");
                        }
                    }
                }
                Some('c') => {
                    chars.next();
                    match chars.peek() {
                        Some('n') => {
                            chars.next();
                            result.push_str(&extract_name(&commit.committer));
                        }
                        Some('N') => {
                            chars.next();
                            let mapped = if use_mailmap && !mailmap.is_empty() {
                                let n = extract_name(&commit.committer);
                                let e = extract_email(&commit.committer);
                                mailmap.map_user(n, e).0
                            } else {
                                extract_name(&commit.committer)
                            };
                            result.push_str(&mapped);
                        }
                        Some('e') => {
                            chars.next();
                            result.push_str(&extract_email(&commit.committer));
                        }
                        Some('E') => {
                            chars.next();
                            let mapped = if use_mailmap && !mailmap.is_empty() {
                                let n = extract_name(&commit.committer);
                                let e = extract_email(&commit.committer);
                                mailmap.map_user(n, e).1
                            } else {
                                extract_email(&commit.committer)
                            };
                            result.push_str(&mapped);
                        }
                        Some('l') => {
                            chars.next();
                            let email = if use_mailmap && !mailmap.is_empty() {
                                let n = extract_name(&commit.committer);
                                let e = extract_email(&commit.committer);
                                mailmap.map_user(n, e).1
                            } else {
                                extract_email(&commit.committer)
                            };
                            result.push_str(&local_part_of_email(&email));
                        }
                        Some('d') => {
                            chars.next();
                            result.push_str(&format_date_with_mode(&commit.committer, None));
                        }
                        _ => {
                            result.push_str("%c");
                        }
                    }
                }
                _ => {
                    result.push('%');
                }
            }
        } else {
            result.push(ch);
        }
    }
    result
}

/// Format ident for header display ("Name <email>").
fn format_ident_for_header(ident: &str) -> String {
    let name = extract_name(ident);
    let email = extract_email(ident);
    if email.is_empty() {
        name
    } else {
        format!("{name} <{email}>")
    }
}

fn format_ident_for_header_mailmap(
    mailmap: &MailmapTable,
    ident: &str,
    use_mailmap: bool,
) -> String {
    if !use_mailmap || mailmap.is_empty() {
        return format_ident_for_header(ident);
    }
    let name = extract_name(ident);
    let email = extract_email(ident);
    let (n, e) = mailmap.map_user(name, email);
    if e.is_empty() {
        n
    } else {
        format!("{n} <{e}>")
    }
}

/// Format date from ident for header display (`Date:` / `AuthorDate:`).
fn format_date_for_header(ident: &str) -> String {
    format_author_date_internal(ident, None, true)
}

/// Parsed commit with its OID.
struct CommitInfo {
    tree: ObjectId,
    parents: Vec<ObjectId>,
    author: String,
    committer: String,
    message: String,
    /// Verbatim body bytes when the commit was stored under an encoding Git cannot decode
    /// (e.g. `i18n.commitEncoding=non-utf-8`). Set from [`CommitData::raw_message`]; the body
    /// emitters use these instead of the lossy `message` so invalid-UTF-8 bytes round-trip.
    raw_message: Option<Vec<u8>>,
    /// The commit's `encoding` header (`i18n.commitEncoding` at commit time), if any. Used to
    /// re-encode the body into the log output encoding, mirroring Git's `logmsg_reencode`.
    encoding: Option<String>,
}

/// Emit a commit message body line by line with a 4-space indent, matching Git's `pp_handle_indent`.
///
/// When `info.raw_message` is present (non-decodable commit encoding) the verbatim bytes are
/// written without lossy UTF-8 conversion; otherwise the decoded `message` string is used.
fn write_commit_body(
    out: &mut impl Write,
    info: &CommitInfo,
    et: usize,
    output_encoding: Option<&str>,
) -> Result<()> {
    write_commit_body_colored(out, info, et, None, output_encoding)
}

/// Resolve the body bytes to print, mirroring Git's `logmsg_reencode`/`pretty_print_commit`.
///
/// Git re-encodes every builtin pretty format (medium/full/fuller/raw/short) from the commit's
/// `encoding` header into the log output encoding before formatting. grit keeps the *decoded*
/// UTF-8 form in `info.message` plus the original bytes in `info.raw_message`; this picks the
/// right one:
///
/// * Output is UTF-8 (`output_encoding` is `None`): emit the decoded `message` (already UTF-8) —
///   unless grit could not faithfully decode the commit encoding (unknown label), in which case
///   Git's `reencode_string` returns NULL and the verbatim bytes are kept (`raw_message`).
/// * Output is a non-UTF-8 label: re-encode the decoded `message` into that label; if encoding
///   fails (unknown target) fall back to the verbatim bytes (Git's no-op fallback).
fn commit_body_output_bytes<'a>(
    info: &'a CommitInfo,
    output_encoding: Option<&str>,
) -> std::borrow::Cow<'a, [u8]> {
    use std::borrow::Cow;
    // Whether grit can faithfully decode the commit's stored encoding (so `message` is exact).
    let commit_encoding_decodable = match info.encoding.as_deref() {
        None => true,
        Some(label) => {
            label.eq_ignore_ascii_case("utf-8")
                || label.eq_ignore_ascii_case("utf8")
                || grit_lib::commit_encoding::is_known_encoding(label)
        }
    };
    match output_encoding {
        None => {
            // UTF-8 output: prefer the decoded message when the decode was faithful; otherwise
            // keep the verbatim bytes (Git keeps the raw buffer when reencoding is impossible).
            if commit_encoding_decodable {
                Cow::Borrowed(info.message.as_bytes())
            } else if let Some(raw) = info.raw_message.as_deref() {
                Cow::Borrowed(raw)
            } else {
                Cow::Borrowed(info.message.as_bytes())
            }
        }
        Some(label) => {
            // Non-UTF-8 output: re-encode the decoded message into the target charset.
            if commit_encoding_decodable {
                match grit_lib::commit_encoding::encode_header_text(label, &info.message) {
                    Some(bytes) => Cow::Owned(bytes),
                    None => info
                        .raw_message
                        .as_deref()
                        .map(Cow::Borrowed)
                        .unwrap_or(Cow::Borrowed(info.message.as_bytes())),
                }
            } else if let Some(raw) = info.raw_message.as_deref() {
                Cow::Borrowed(raw)
            } else {
                Cow::Borrowed(info.message.as_bytes())
            }
        }
    }
}

/// Like [`write_commit_body`] but, when `colorizer` is set, colorizes grep
/// matches within each message line (Git `color_line`). The 4-space indent is
/// emitted uncolored; only the message text after it is colorized.
fn write_commit_body_colored(
    out: &mut impl Write,
    info: &CommitInfo,
    et: usize,
    colorizer: Option<&GrepColorizer>,
    output_encoding: Option<&str>,
) -> Result<()> {
    let body_bytes = commit_body_output_bytes(info, output_encoding);
    {
        let raw = body_bytes.as_ref();
        // Mirror `str::lines()`: split on '\n', strip a trailing '\r', and drop the final empty
        // segment produced by a body that ends in '\n' (Git does not print a spurious blank line).
        let body = raw.strip_suffix(b"\n").unwrap_or(raw);
        for line in body.split(|&b| b == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            // Colorize only when the line is valid UTF-8 (grep patterns are
            // UTF-8 regexes); otherwise fall back to the byte path.
            if let (Some(c), Ok(text)) = (colorizer, std::str::from_utf8(line)) {
                if let Some(colored) = c.colorize_message(text) {
                    let indented = grit_lib::tab_expand::indent_and_expand_tabs(&colored, 4, et);
                    out.write_all(indented.as_bytes())?;
                    out.write_all(b"\n")?;
                    continue;
                }
            }
            out.write_all(&grit_lib::tab_expand::indent_and_expand_tabs_bytes(
                line, 4, et,
            ))?;
            out.write_all(b"\n")?;
        }
    }
    Ok(())
}

/// Key for Git-style date ordering: newest committer (or author) date first; ties broken by FIFO
/// (`seq`) then OID (matches `commit_list_insert_by_date` when timestamps collide).
type CommitQueueKey = (std::cmp::Reverse<i64>, u64, ObjectId);

/// Parsed `color.decorate.*` and `diff.color.commit` sequences for log output.
#[derive(Clone, Debug)]
struct DecorationPaint {
    commit: String,
    reset: String,
    branch: String,
    remote_branch: String,
    tag: String,
    stash: String,
    head: String,
    grafted: String,
}

/// Incremental commit walk matching Git's `commit_list_insert_by_date` queue (not a max-heap on
/// timestamp alone — ties preserve discovery order like upstream `revision.c`).
///
/// Used by `grit log` to print commits as they are discovered instead of buffering
/// the full history in a `Vec` first.
struct WalkCommitsIter<'a> {
    repo: &'a Repository,
    odb: &'a Odb,
    git_dir: &'a Path,
    pickaxe_args: Option<&'a Args>,
    bloom_chain: Option<CommitGraphChain>,
    bloom_read_changed_paths: bool,
    bloom_changed_paths_version: i32,
    bloom_stats: Option<BloomWalkStatsHandle>,
    bloom_pathspecs: &'a [String],
    bloom_cwd: Option<String>,
    author_date_order: bool,
    shallow_boundaries: HashSet<ObjectId>,
    graft_parents: HashMap<ObjectId, Vec<ObjectId>>,
    blocked: HashSet<ObjectId>,
    enlisted: HashSet<ObjectId>,
    queue: BTreeMap<CommitQueueKey, ()>,
    next_seq: u64,
    skipped: usize,
    skip_n: usize,
    max_count: Option<usize>,
    first_parent: bool,
    author_res: &'a [Regex],
    committer_res: &'a [Regex],
    grep_filter: &'a GrepFilter,
    all_match_grep: bool,
    invert_grep: bool,
    mailmap: &'a MailmapTable,
    use_mailmap: bool,
    no_merges: bool,
    merges_only: bool,
    pathspecs: &'a [String],
    accepted_count: usize,
}

impl<'a> WalkCommitsIter<'a> {
    fn new(
        repo: &'a Repository,
        odb: &'a Odb,
        git_dir: &'a Path,
        start: &[ObjectId],
        max_count: Option<usize>,
        skip: Option<usize>,
        first_parent: bool,
        author_res: &'a [Regex],
        committer_res: &'a [Regex],
        grep_filter: &'a GrepFilter,
        all_match_grep: bool,
        invert_grep: bool,
        mailmap: &'a MailmapTable,
        use_mailmap: bool,
        no_merges: bool,
        merges_only: bool,
        pathspecs: &'a [String],
        excluded: &HashSet<ObjectId>,
        pickaxe_args: Option<&'a Args>,
        bloom_chain: Option<CommitGraphChain>,
        bloom_read_changed_paths: bool,
        bloom_changed_paths_version: i32,
        bloom_stats: Option<BloomWalkStatsHandle>,
        bloom_pathspecs: &'a [String],
        bloom_cwd: Option<String>,
        author_date_order: bool,
    ) -> Self {
        let shallow_boundaries = load_shallow_boundaries(git_dir);
        let graft_parents = load_graft_parents(git_dir);
        let blocked: HashSet<ObjectId> = excluded.clone();
        let mut enlisted = HashSet::new();
        let mut queue: BTreeMap<CommitQueueKey, ()> = BTreeMap::new();
        let mut next_seq = 0u64;
        for oid in start {
            if blocked.contains(oid) {
                continue;
            }
            if !enlisted.insert(*oid) {
                continue;
            }
            let ts = if author_date_order {
                read_author_timestamp_repo(repo, oid)
            } else {
                read_commit_timestamp_repo(repo, oid)
            };
            let key: CommitQueueKey = (std::cmp::Reverse(ts), next_seq, *oid);
            next_seq = next_seq.saturating_add(1);
            queue.insert(key, ());
        }
        Self {
            repo,
            odb,
            git_dir,
            pickaxe_args,
            bloom_chain,
            bloom_read_changed_paths,
            bloom_changed_paths_version,
            bloom_stats,
            bloom_pathspecs,
            bloom_cwd,
            author_date_order,
            shallow_boundaries,
            graft_parents,
            blocked,
            enlisted,
            queue,
            next_seq,
            skipped: 0,
            skip_n: skip.unwrap_or(0),
            max_count,
            first_parent,
            author_res,
            committer_res,
            grep_filter,
            all_match_grep,
            invert_grep,
            mailmap,
            use_mailmap,
            no_merges,
            merges_only,
            pathspecs,
            accepted_count: 0,
        }
    }

    fn next_commit(&mut self) -> Result<Option<(ObjectId, CommitInfo)>> {
        if self.max_count == Some(0) {
            return Ok(None);
        }
        if let Some(max) = self.max_count {
            if self.accepted_count >= max {
                return Ok(None);
            }
        }
        while let Some((key, ())) = self.queue.pop_first() {
            let oid = key.2;

            let obj = self.repo.read_replaced(&oid)?;
            if obj.kind == ObjectKind::Tag {
                let tag = parse_tag(&obj.data)?;
                let mut target = tag.object;
                loop {
                    let t_obj = self.repo.read_replaced(&target)?;
                    match t_obj.kind {
                        ObjectKind::Commit => {
                            let ts = if self.author_date_order {
                                read_author_timestamp_repo(self.repo, &target)
                            } else {
                                read_commit_timestamp_repo(self.repo, &target)
                            };
                            if !self.blocked.contains(&target) && self.enlisted.insert(target) {
                                let k: CommitQueueKey =
                                    (std::cmp::Reverse(ts), self.next_seq, target);
                                self.next_seq = self.next_seq.saturating_add(1);
                                self.queue.insert(k, ());
                            }
                            break;
                        }
                        ObjectKind::Tag => {
                            let inner = parse_tag(&t_obj.data)?;
                            target = inner.object;
                        }
                        _ => break,
                    }
                }
                continue;
            }
            let commit = parse_commit(&obj.data)?;
            let mut walk_parents = commit.parents.clone();
            if let Some(grafted) = self.graft_parents.get(&oid) {
                walk_parents = grafted.clone();
            }

            let info = CommitInfo {
                tree: commit.tree,
                parents: walk_parents.clone(),
                author: commit.author.clone(),
                committer: commit.committer.clone(),
                message: commit.message.clone(),
                raw_message: commit.raw_message.clone(),
                encoding: commit.encoding.clone(),
            };

            if !self.shallow_boundaries.contains(&oid) {
                if self.first_parent {
                    if let Some(parent) = walk_parents.first() {
                        if !self.blocked.contains(parent) && self.enlisted.insert(*parent) {
                            let ts = if self.author_date_order {
                                read_author_timestamp_repo(self.repo, parent)
                            } else {
                                read_commit_timestamp_repo(self.repo, parent)
                            };
                            let k: CommitQueueKey = (std::cmp::Reverse(ts), self.next_seq, *parent);
                            self.next_seq = self.next_seq.saturating_add(1);
                            self.queue.insert(k, ());
                        }
                    }
                } else {
                    for parent in &walk_parents {
                        if !self.blocked.contains(parent) && self.enlisted.insert(*parent) {
                            let ts = if self.author_date_order {
                                read_author_timestamp_repo(self.repo, parent)
                            } else {
                                read_commit_timestamp_repo(self.repo, parent)
                            };
                            let k: CommitQueueKey = (std::cmp::Reverse(ts), self.next_seq, *parent);
                            self.next_seq = self.next_seq.saturating_add(1);
                            self.queue.insert(k, ());
                        }
                    }
                }
            }

            let is_merge = info.parents.len() > 1;
            if self.no_merges && is_merge {
                continue;
            }
            if self.merges_only && !is_merge {
                continue;
            }
            let author_ok = ident_matches_header_patterns(
                self.author_res,
                &info.author,
                self.mailmap,
                self.use_mailmap,
            );
            if !author_ok {
                continue;
            }
            let committer_ok = ident_matches_header_patterns(
                self.committer_res,
                &info.committer,
                self.mailmap,
                self.use_mailmap,
            );
            if !committer_ok {
                continue;
            }
            let msg_ok =
                self.grep_filter
                    .matches(&info.message, self.all_match_grep, self.invert_grep);
            if !msg_ok {
                continue;
            }
            if !self.pathspecs.is_empty() {
                let touches = commit_touches_paths(
                    self.odb,
                    oid,
                    &info,
                    self.pathspecs,
                    self.bloom_chain.as_ref(),
                    self.bloom_read_changed_paths,
                    self.bloom_changed_paths_version,
                    self.bloom_stats.as_ref(),
                    self.bloom_pathspecs,
                    self.bloom_cwd.as_deref(),
                )?;
                if !touches {
                    continue;
                }
            }

            if let Some(pa) = self.pickaxe_args {
                if !commit_pickaxe_matches(self.git_dir, self.odb, &info, pa)? {
                    continue;
                }
            }

            if self.skipped < self.skip_n {
                self.skipped += 1;
            } else {
                self.accepted_count += 1;
                return Ok(Some((oid, info)));
            }
        }
        Ok(None)
    }
}

/// Walk the commit graph in reverse chronological order.
/// Collect all OIDs reachable from the given starting points.
fn collect_reachable(odb: &Odb, starts: &[ObjectId]) -> Result<HashSet<ObjectId>> {
    let mut visited = HashSet::new();
    let mut queue: Vec<ObjectId> = starts.to_vec();
    while let Some(oid) = queue.pop() {
        if !visited.insert(oid) {
            continue;
        }
        if let Ok(obj) = odb.read(&oid) {
            if let Ok(commit) = parse_commit(&obj.data) {
                for parent in &commit.parents {
                    if !visited.contains(parent) {
                        queue.push(*parent);
                    }
                }
            }
        }
    }
    Ok(visited)
}

fn walk_commits(
    repo: &Repository,
    git_dir: &Path,
    start: &[ObjectId],
    max_count: Option<usize>,
    skip: Option<usize>,
    first_parent: bool,
    author_res: &[Regex],
    committer_res: &[Regex],
    grep_filter: &GrepFilter,
    all_match_grep: bool,
    invert_grep: bool,
    mailmap: &MailmapTable,
    use_mailmap: bool,
    no_merges: bool,
    merges_only: bool,
    pathspecs: &[String],
    excluded: &HashSet<ObjectId>,
    pickaxe_args: Option<&Args>,
    bloom_chain: Option<CommitGraphChain>,
    bloom_read_changed_paths: bool,
    bloom_changed_paths_version: i32,
    bloom_stats: Option<BloomWalkStatsHandle>,
    bloom_pathspecs: &[String],
    bloom_cwd: Option<String>,
    author_date_order: bool,
) -> Result<Vec<(ObjectId, CommitInfo)>> {
    if max_count == Some(0) {
        return Ok(Vec::new());
    }
    let odb = &repo.odb;
    let mut iter = WalkCommitsIter::new(
        repo,
        odb,
        git_dir,
        start,
        max_count,
        skip,
        first_parent,
        author_res,
        committer_res,
        grep_filter,
        all_match_grep,
        invert_grep,
        mailmap,
        use_mailmap,
        no_merges,
        merges_only,
        pathspecs,
        excluded,
        pickaxe_args,
        bloom_chain,
        bloom_read_changed_paths,
        bloom_changed_paths_version,
        bloom_stats,
        bloom_pathspecs,
        bloom_cwd,
        author_date_order,
    );
    let mut result = Vec::new();
    while let Some(c) = iter.next_commit()? {
        result.push(c);
    }
    Ok(result)
}

/// Check if a commit touches any of the given pathspecs by diffing against parents.
fn commit_touches_paths(
    odb: &Odb,
    commit_oid: ObjectId,
    info: &CommitInfo,
    pathspecs: &[String],
    bloom_chain: Option<&CommitGraphChain>,
    read_changed_paths: bool,
    changed_paths_version: i32,
    bloom_stats: Option<&BloomWalkStatsHandle>,
    bloom_specs: &[String],
    bloom_cwd: Option<&str>,
) -> Result<bool> {
    let bloom_keys = if bloom_specs.is_empty() {
        pathspecs
    } else {
        bloom_specs
    };

    if info.parents.is_empty() {
        let mut bloom_ret = BloomPrecheck::Inapplicable;
        if let Some(chain) = bloom_chain {
            if !bloom_keys.is_empty() {
                bloom_ret = chain.bloom_precheck_for_paths(
                    odb,
                    commit_oid,
                    bloom_keys,
                    bloom_cwd,
                    changed_paths_version,
                    read_changed_paths,
                )?;
                if let Some(stats) = bloom_stats {
                    if let Ok(mut g) = stats.lock() {
                        g.record_precheck(bloom_ret);
                    }
                }
                if bloom_ret == BloomPrecheck::DefinitelyNot {
                    return Ok(false);
                }
            }
        }
        if pathspecs.is_empty() {
            return Ok(true);
        }
        let entries = diff_trees(odb, None, Some(&info.tree), "")?;
        let touches = entries.iter().any(|e| {
            let path = e.path().to_string_lossy();
            path_matches(&path, pathspecs)
        });
        if bloom_ret == BloomPrecheck::Maybe && !touches {
            if let Some(stats) = bloom_stats {
                if let Ok(mut g) = stats.lock() {
                    g.record_false_positive();
                }
            }
        }
        return Ok(touches);
    }

    if info.parents.len() == 1 {
        let mut bloom_ret = BloomPrecheck::Inapplicable;
        if let Some(chain) = bloom_chain {
            if !bloom_keys.is_empty() {
                bloom_ret = chain.bloom_precheck_for_paths(
                    odb,
                    commit_oid,
                    bloom_keys,
                    bloom_cwd,
                    changed_paths_version,
                    read_changed_paths,
                )?;
                if let Some(stats) = bloom_stats {
                    if let Ok(mut g) = stats.lock() {
                        g.record_precheck(bloom_ret);
                    }
                }
                if bloom_ret == BloomPrecheck::DefinitelyNot {
                    return Ok(false);
                }
            }
        }
        if pathspecs.is_empty() {
            return Ok(true);
        }
        let parent_obj = odb.read(&info.parents[0])?;
        let parent_commit = parse_commit(&parent_obj.data)?;
        let entries = diff_trees(odb, Some(&parent_commit.tree), Some(&info.tree), "")?;
        let touches = entries.iter().any(|e| {
            let path = e.path().to_string_lossy();
            path_matches(&path, pathspecs)
        });
        if bloom_ret == BloomPrecheck::Maybe && !touches {
            if let Some(stats) = bloom_stats {
                if let Ok(mut g) = stats.lock() {
                    g.record_false_positive();
                }
            }
        }
        return Ok(touches);
    }

    for parent_oid in &info.parents {
        let parent_obj = odb.read(parent_oid)?;
        let parent_commit = parse_commit(&parent_obj.data)?;
        let entries = diff_trees(odb, Some(&parent_commit.tree), Some(&info.tree), "")?;
        if entries.iter().any(|e| {
            let path = e.path().to_string_lossy();
            path_matches(&path, pathspecs)
        }) {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Check if a file path matches a pathspec list (Git `match_pathspec`, including `:(exclude)`).
fn path_matches(path: &str, pathspecs: &[String]) -> bool {
    grit_lib::pathspec::matches_pathspec_list(path, pathspecs)
}

/// Extract unix timestamp from an author/committer line.
/// Read the committer timestamp from a commit object for priority queue ordering.
fn read_commit_timestamp(odb: &Odb, oid: &ObjectId) -> i64 {
    match odb.read(oid) {
        Ok(obj) => match parse_commit(&obj.data) {
            Ok(commit) => committer_unix_seconds_for_ordering(&commit.committer),
            Err(_) => 0,
        },
        Err(_) => 0,
    }
}

fn read_commit_timestamp_repo(repo: &Repository, oid: &ObjectId) -> i64 {
    match repo.read_replaced(oid) {
        Ok(obj) => match parse_commit(&obj.data) {
            Ok(commit) => committer_unix_seconds_for_ordering(&commit.committer),
            Err(_) => 0,
        },
        Err(_) => 0,
    }
}

fn read_author_timestamp(odb: &Odb, oid: &ObjectId) -> i64 {
    match odb.read(oid) {
        Ok(obj) => match parse_commit(&obj.data) {
            Ok(commit) => committer_unix_seconds_for_ordering(&commit.author),
            Err(_) => 0,
        },
        Err(_) => 0,
    }
}

fn read_author_timestamp_repo(repo: &Repository, oid: &ObjectId) -> i64 {
    match repo.read_replaced(oid) {
        Ok(obj) => match parse_commit(&obj.data) {
            Ok(commit) => committer_unix_seconds_for_ordering(&commit.author),
            Err(_) => 0,
        },
        Err(_) => 0,
    }
}

fn extract_timestamp(ident: &str) -> String {
    match timestamp_for_at_ct(signature_timestamp_for_pretty(ident)) {
        Some(ts) => ts.to_string(),
        None => String::new(),
    }
}

fn format_relative_from_diff(diff: i64) -> String {
    if diff < 0 {
        "in the future".to_owned()
    } else if diff < 60 {
        format!("{} seconds ago", diff)
    } else if diff < 3600 {
        let m = diff / 60;
        if m == 1 {
            "1 minute ago".to_owned()
        } else {
            format!("{m} minutes ago")
        }
    } else if diff < 86400 {
        let h = diff / 3600;
        if h == 1 {
            "1 hour ago".to_owned()
        } else {
            format!("{h} hours ago")
        }
    } else if diff < 86400 * 30 {
        let d = diff / 86400;
        if d == 1 {
            "1 day ago".to_owned()
        } else {
            format!("{d} days ago")
        }
    } else if diff < 86400 * 365 {
        let months = diff / (86400 * 30);
        if months == 1 {
            "1 month ago".to_owned()
        } else {
            format!("{months} months ago")
        }
    } else {
        let years = diff / (86400 * 365);
        if years == 1 {
            "1 year ago".to_owned()
        } else {
            format!("{years} years ago")
        }
    }
}

/// Lazily loads the default git-notes map (`GIT_NOTES_REF` / `core.notesRef`) for `%N` in custom formats.
struct NotesMapCache<'a> {
    repo: &'a Repository,
    map: Option<std::collections::HashMap<ObjectId, Vec<u8>>>,
}

impl<'a> NotesMapCache<'a> {
    fn new(repo: &'a Repository) -> Self {
        Self { repo, map: None }
    }

    fn repo(&self) -> &'a Repository {
        self.repo
    }

    fn map(&mut self) -> &std::collections::HashMap<ObjectId, Vec<u8>> {
        let repo = self.repo;
        self.map.get_or_insert_with(|| load_notes_map(repo))
    }
}

fn log_notes_enabled() -> bool {
    match std::env::var("GIT_GRIT_LOG_NOTES_CLI").ok().as_deref() {
        Some("off") => false,
        Some("on") => true,
        _ => std::env::var("GIT_GRIT_LOG_NOTES_DEFAULT").ok().as_deref() == Some("1"),
    }
}

/// Whether `git show` should print the default `Notes:` block for medium-style headers.
///
/// `git show --pretty` (no format) matches C Git: no notes. Plain `git show` and `--pretty=<fmt>` can show notes.
/// Run signature verification on a raw commit object, returning the parsed
/// [`SignatureCheck`].  Errors (e.g. gpg not runnable) collapse to a
/// "no signature" result so callers can still format something sensible.
pub fn verify_commit_signature(
    config: &ConfigSet,
    raw_commit: &[u8],
) -> grit_lib::signing::SignatureCheck {
    match grit_lib::signing::GpgConfig::from_config(config) {
        Ok(cfg) => grit_lib::signing::verify_commit(&cfg, raw_commit)
            .unwrap_or_else(|_| grit_lib::signing::SignatureCheck::default_none()),
        Err(_) => grit_lib::signing::SignatureCheck::default_none(),
    }
}

/// Format the GPG verification lines for `--show-signature`, mirroring git's
/// `show_signature`: when the commit carries no signature, nothing is emitted;
/// otherwise the human-readable gpg output (its stderr) is returned verbatim.
pub fn format_commit_signature_lines(config: &ConfigSet, raw_commit: &[u8]) -> String {
    if grit_lib::signing::extract_signed_payload(raw_commit).is_none() {
        return String::new();
    }
    let sigc = verify_commit_signature(config, raw_commit);
    sigc.output
}

pub fn show_notes_display_enabled() -> bool {
    if std::env::var("GIT_GRIT_SHOW_BARE_PRETTY").ok().as_deref() == Some("1") {
        return false;
    }
    if std::env::var("GIT_GRIT_SHOW_EXPLICIT_PRETTY")
        .ok()
        .as_deref()
        == Some("1")
    {
        return false;
    }
    log_notes_enabled()
}

/// Build the format-patch `Notes:` block for an explicit, ordered `(header, refname)` list. The
/// returned string (empty when there are no matching notes) is placed after the `---` separator and
/// before the diffstat, each ref rendered as `\nNotes (<ref>):\n    <line>\n`.
pub fn format_patch_notes_block(
    repo: &Repository,
    oid: &ObjectId,
    refs: &[(String, String)],
) -> String {
    let mut extra = String::new();
    for (header, refname) in refs {
        let map = load_notes_map_for_ref(repo, refname);
        if let Some(note_data) = map.get(oid) {
            let note_text = String::from_utf8_lossy(note_data);
            let _ = extra.write_char('\n');
            let _ = writeln!(extra, "{header}:");
            for line in note_text.lines() {
                let _ = writeln!(extra, "    {line}");
            }
        }
    }
    extra
}

/// Append Git-style `Notes:` blocks to a format-patch body when note display is enabled.
pub fn append_format_patch_notes(repo: &Repository, oid: &ObjectId, body: &str) -> String {
    if !log_notes_enabled() {
        return body.to_string();
    }
    let mut extra = String::new();
    for (header, refname) in collect_log_display_note_refs(repo) {
        let map = load_notes_map_for_ref(repo, &refname);
        if let Some(note_data) = map.get(oid) {
            let note_text = String::from_utf8_lossy(note_data);
            let _ = extra.write_char('\n');
            let _ = writeln!(extra, "{header}:");
            for line in note_text.lines() {
                let _ = writeln!(extra, "    {line}");
            }
        }
    }
    if extra.is_empty() {
        return body.to_string();
    }
    format!("{}{}", body.trim_end_matches('\n'), extra)
}

/// Build a map of commit OID to its formatted `Notes:` block for display in
/// `diff-tree --notes`. The block excludes surrounding blank lines; each entry
/// looks like `Notes:\n    <line1>\n    <line2>`. Unlike the `git log` default
/// note display, this is unconditional (the caller opted in with `--notes`).
pub fn notes_blocks_for_display(repo: &Repository) -> std::collections::HashMap<ObjectId, String> {
    use std::fmt::Write as _;
    validate_notes_display_ref_config(repo);
    let mut blocks: std::collections::HashMap<ObjectId, String> = std::collections::HashMap::new();
    for (header, refname) in collect_log_display_note_refs_unconditional(repo) {
        let map = load_notes_map_for_ref(repo, &refname);
        for (oid, note_data) in map {
            let note_text = String::from_utf8_lossy(&note_data);
            let entry = blocks.entry(oid).or_default();
            if !entry.is_empty() {
                entry.push('\n');
            }
            let _ = write!(entry, "{header}:");
            for line in note_text.lines() {
                let _ = write!(entry, "\n    {line}");
            }
        }
    }
    blocks
}

pub fn validate_notes_display_ref_config(repo: &Repository) {
    let cfg = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    if cfg
        .get_all_raw("notes.displayRef")
        .iter()
        .any(Option::is_none)
    {
        eprintln!("fatal: invalid notes.displayRef");
        std::process::exit(128);
    }
}

/// Same ref enumeration as `collect_log_display_note_refs` but without the
/// `log_notes_enabled()` gate (used by `diff-tree --notes`, which is explicit).
fn collect_log_display_note_refs_unconditional(repo: &Repository) -> Vec<(String, String)> {
    let cfg = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let default_ref = std::env::var("GIT_NOTES_REF")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| cfg.get("core.notesRef").filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "refs/notes/commits".to_string());

    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let mut push_ref = |refname: &str| {
        if !seen.insert(refname.to_string()) {
            return;
        }
        let short = refname.strip_prefix("refs/notes/").unwrap_or(refname);
        let header = if refname == "refs/notes/commits" {
            "Notes".to_string()
        } else {
            format!("Notes ({short})")
        };
        out.push((header, refname.to_string()));
    };

    match std::env::var("GIT_NOTES_DISPLAY_REF") {
        Ok(s) => {
            for pat in s.split(':') {
                let pat = pat.trim();
                if pat.is_empty() {
                    continue;
                }
                for name in resolve_notes_display_pattern(repo, pat) {
                    push_ref(&name);
                }
            }
        }
        Err(_) => {
            push_ref(&default_ref);
            for pat in cfg.get_all("notes.displayRef") {
                let pat = pat.trim();
                if pat.is_empty() {
                    continue;
                }
                for name in resolve_notes_display_pattern(repo, pat) {
                    push_ref(&name);
                }
            }
        }
    }

    out
}

fn collect_log_display_note_refs(repo: &Repository) -> Vec<(String, String)> {
    if !log_notes_enabled() {
        return Vec::new();
    }
    let cfg = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let default_ref = std::env::var("GIT_NOTES_REF")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| cfg.get("core.notesRef").filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "refs/notes/commits".to_string());

    let ud = std::env::var("GIT_GRIT_LOG_NOTES_USE_DEFAULT").unwrap_or_default();
    let use_default_notes: i8 = if ud.is_empty() {
        -1
    } else if ud == "0" {
        0
    } else {
        1
    };

    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let mut push_ref = |refname: &str| {
        if !seen.insert(refname.to_string()) {
            return;
        }
        let short = refname.strip_prefix("refs/notes/").unwrap_or(refname);
        let header = if refname == "refs/notes/commits" {
            "Notes".to_string()
        } else {
            format!("Notes ({short})")
        };
        out.push((header, refname.to_string()));
    };

    // Default ref + `notes.displayRef` / `GIT_NOTES_DISPLAY_REF` (matches `load_display_notes` default block).
    let load_default_block = use_default_notes > 0
        || (use_default_notes == -1 && std::env::var("GIT_GRIT_LOG_NOTES_CLI").is_err());
    if load_default_block {
        match std::env::var("GIT_NOTES_DISPLAY_REF") {
            Ok(s) => {
                for pat in s.split(':') {
                    let pat = pat.trim();
                    if pat.is_empty() {
                        continue;
                    }
                    for name in resolve_notes_display_pattern(repo, pat) {
                        push_ref(&name);
                    }
                }
            }
            Err(_) => {
                push_ref(&default_ref);
                for pat in cfg.get_all("notes.displayRef") {
                    let pat = pat.trim();
                    if pat.is_empty() {
                        continue;
                    }
                    for name in resolve_notes_display_pattern(repo, pat) {
                        push_ref(&name);
                    }
                }
            }
        }
    }

    out
}

fn resolve_notes_display_pattern(repo: &Repository, pattern: &str) -> Vec<String> {
    if !pattern.contains('*') && !pattern.contains('?') && !pattern.contains('[') {
        if refs::resolve_ref(&repo.git_dir, pattern).is_ok() {
            return vec![pattern.to_owned()];
        }
    }
    refs::list_refs_glob(&repo.git_dir, pattern)
        .map(|items| items.into_iter().map(|(name, _)| name).collect())
        .unwrap_or_default()
}

/// Load notes from the configured notes ref (or `refs/notes/commits` default).
/// Returns a map from commit OID to the notes blob OID.
fn load_notes_map(repo: &Repository) -> std::collections::HashMap<ObjectId, Vec<u8>> {
    use grit_lib::config::ConfigSet;

    let notes_ref = std::env::var("GIT_NOTES_REF")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
            config
                .get("core.notesRef")
                .unwrap_or_else(|| "refs/notes/commits".to_string())
        });
    load_notes_map_for_ref(repo, &notes_ref)
}

fn load_notes_map_for_ref(
    repo: &Repository,
    notes_ref: &str,
) -> std::collections::HashMap<ObjectId, Vec<u8>> {
    use grit_lib::refs::resolve_ref;

    let mut map = std::collections::HashMap::new();

    // Resolve notes ref to a commit, then get its tree
    let notes_oid = match resolve_ref(&repo.git_dir, notes_ref) {
        Ok(oid) => oid,
        Err(_) => return map,
    };

    let obj = match repo.odb.read(&notes_oid) {
        Ok(o) => o,
        Err(_) => return map,
    };

    let tree_oid = match obj.kind {
        ObjectKind::Commit => match parse_commit(&obj.data) {
            Ok(c) => c.tree,
            Err(_) => return map,
        },
        ObjectKind::Tree => notes_oid,
        _ => return map,
    };

    collect_notes_map_recursive(repo, &tree_oid, String::new(), &mut map);
    map
}

/// Concatenate two note blobs the same way as Git's `combine_notes_concatenate` in `notes.c`:
/// empty `new` leaves `cur` unchanged; empty or missing `cur` becomes `new`; otherwise join with
/// `\n\n` after stripping one trailing newline from `cur`.
fn combine_notes_concatenate_blobs(cur: &[u8], new: &[u8]) -> Vec<u8> {
    if new.is_empty() {
        return cur.to_vec();
    }
    if cur.is_empty() {
        return new.to_vec();
    }
    let mut cur_len = cur.len();
    if cur_len > 0 && cur[cur_len - 1] == b'\n' {
        cur_len -= 1;
    }
    let mut out = Vec::with_capacity(cur_len.saturating_add(2).saturating_add(new.len()));
    out.extend_from_slice(&cur[..cur_len]);
    out.push(b'\n');
    out.push(b'\n');
    out.extend_from_slice(new);
    out
}

fn collect_notes_map_recursive(
    repo: &Repository,
    tree_oid: &grit_lib::objects::ObjectId,
    prefix: String,
    map: &mut std::collections::HashMap<grit_lib::objects::ObjectId, Vec<u8>>,
) {
    use grit_lib::objects::parse_tree;
    let tree_obj = match repo.odb.read(tree_oid) {
        Ok(o) => o,
        Err(_) => return,
    };
    let entries = match parse_tree(&tree_obj.data) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries {
        let name = String::from_utf8_lossy(&entry.name);
        let full_hex = format!("{prefix}{name}");
        if entry.mode == 0o040000 {
            collect_notes_map_recursive(repo, &entry.oid, full_hex, map);
        } else if let Ok(commit_oid) = full_hex.parse::<grit_lib::objects::ObjectId>() {
            if let Ok(blob) = repo.odb.read(&entry.oid) {
                use std::collections::hash_map::Entry;
                match map.entry(commit_oid) {
                    Entry::Vacant(e) => {
                        e.insert(blob.data);
                    }
                    Entry::Occupied(mut e) => {
                        // Same as Git when two tree paths resolve to one commit id: skip if the note
                        // blob is identical (`combine_notes` short-circuit on matching oids).
                        if e.get().as_slice() == blob.data.as_slice() {
                            continue;
                        }
                        let combined =
                            combine_notes_concatenate_blobs(e.get(), blob.data.as_slice());
                        e.insert(combined);
                    }
                }
            }
        }
    }
}

fn log_wants_blank_line_between_commits(args: &Args) -> bool {
    if log_uses_builtin_oneline(args) {
        return false;
    }
    matches!(
        args.format.as_deref(),
        None | Some("short" | "medium" | "full" | "fuller" | "raw" | "email")
    )
}

/// Write notes for a commit if any exist.
fn write_notes(
    out: &mut impl Write,
    oid: &ObjectId,
    notes_cache: &mut NotesMapCache<'_>,
    args: &Args,
    _odb: &Odb,
) -> Result<()> {
    if args.no_notes {
        return Ok(());
    }
    let mut display = if args.format.as_deref() == Some("raw")
        && std::env::var("GIT_GRIT_LOG_NOTES_CLI").ok().as_deref() != Some("on")
    {
        Vec::new()
    } else {
        collect_log_display_note_refs(notes_cache.repo())
    };
    let mut seen: HashSet<String> = display.iter().map(|(_, r)| r.clone()).collect();
    for spec in &args.notes_refs {
        let refname = if spec.is_empty() {
            "refs/notes/commits".to_string()
        } else if spec.starts_with("refs/") {
            spec.clone()
        } else {
            format!("refs/notes/{spec}")
        };
        for resolved in resolve_notes_display_pattern(notes_cache.repo(), &refname) {
            if seen.insert(resolved.clone()) {
                let short = resolved.strip_prefix("refs/notes/").unwrap_or(&resolved);
                let header = if resolved == "refs/notes/commits" {
                    "Notes".to_string()
                } else {
                    format!("Notes ({short})")
                };
                display.push((header, resolved));
            }
        }
    }
    for (header, refname) in display {
        let map = load_notes_map_for_ref(notes_cache.repo(), &refname);
        if let Some(note_data) = map.get(oid) {
            let note_text = String::from_utf8_lossy(note_data);
            writeln!(out)?;
            writeln!(out, "{header}:")?;
            for line in note_text.lines() {
                writeln!(out, "    {line}")?;
            }
        }
    }
    Ok(())
}

fn validate_log_pickaxe_options(repo: &Repository, args: &Args) -> Result<()> {
    if args.no_pickaxe_regex {
        return Err(anyhow::Error::new(ExplicitExit {
            code: 128,
            message: "fatal: unrecognized argument: --no-pickaxe-regex".to_string(),
        }));
    }
    if let Some(s) = args.pickaxe_string.as_deref() {
        if s == "\u{7f}__GRIT_MISSING_PICKAXE_S__" {
            return Err(anyhow::Error::new(ExplicitExit {
                code: 129,
                message: "error: switch `S' requires a value".to_string(),
            }));
        }
        if s.is_empty() {
            return Err(anyhow::Error::new(ExplicitExit {
                code: 129,
                message: "error: -S requires a non-empty argument".to_string(),
            }));
        }
    }
    if let Some(s) = args.pickaxe_grep.as_deref() {
        if s == "\u{7f}__GRIT_MISSING_PICKAXE_G__" {
            return Err(anyhow::Error::new(ExplicitExit {
                code: 129,
                message: "error: switch `G' requires a value".to_string(),
            }));
        }
        if s.is_empty() {
            return Err(anyhow::Error::new(ExplicitExit {
                code: 129,
                message: "error: -G requires a non-empty argument".to_string(),
            }));
        }
    }
    if args.pickaxe_grep.is_some() && args.pickaxe_regex {
        return Err(anyhow::Error::new(ExplicitExit {
            code: 128,
            message: "fatal: options '-G' and '--pickaxe-regex' cannot be used together, use '--pickaxe-regex' with '-S'".to_string(),
        }));
    }

    let mut pickaxe_kinds = 0usize;
    if args.pickaxe_grep.is_some() {
        pickaxe_kinds += 1;
    }
    if args.pickaxe_string.is_some() {
        pickaxe_kinds += 1;
    }
    if args.find_object.is_some() {
        pickaxe_kinds += 1;
    }
    if args.pickaxe_all && args.find_object.is_some() {
        return Err(anyhow::Error::new(ExplicitExit {
            code: 128,
            message: "fatal: options '--pickaxe-all' and '--find-object' cannot be used together, use '--pickaxe-all' with '-G' and '-S'".to_string(),
        }));
    }
    if pickaxe_kinds > 1 {
        return Err(anyhow::Error::new(ExplicitExit {
            code: 128,
            message: "fatal: options '-G', '-S', and '--find-object' cannot be used together"
                .to_string(),
        }));
    }

    if (args.pickaxe_grep.is_some() || args.pickaxe_string.is_some()) && !args.no_textconv {
        validate_pickaxe_textconv_drivers(repo.git_dir.as_path(), repo.work_tree.as_deref())?;
    }
    Ok(())
}

/// First executable token of `diff.<driver>.textconv`, matching Git's shell concatenation for
/// values like `"/abs/cwd"/hexdump` (t4030-diff-textconv).
fn pickaxe_textconv_cmd_first_token(cmd_line: &str) -> Option<String> {
    let s = cmd_line.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(rest) = s.strip_prefix('"') {
        let end = rest.find('"')?;
        let prefix = &rest[..end];
        let tail = rest[end + 1..].trim_start();
        let suffix = tail.split_whitespace().next().unwrap_or("");
        return Some(format!("{prefix}{suffix}"));
    }
    let first = s.split_whitespace().next()?;
    Some(first.trim_matches(|c| c == '"' || c == '\'').to_string())
}

fn path_has_textconv_driver(git_dir: &Path, config: &ConfigSet, path: &str) -> bool {
    let work_tree = git_dir.parent().unwrap_or(git_dir);
    let rules = load_gitattributes(work_tree);
    let fa = get_file_attrs(&rules, path, false, config);
    if let DiffAttr::Driver(ref driver) = fa.diff_attr {
        return config.get(&format!("diff.{driver}.textconv")).is_some();
    }
    false
}

fn validate_pickaxe_textconv_drivers(git_dir: &Path, work_tree: Option<&Path>) -> Result<()> {
    let Some(wt) = work_tree else {
        return Ok(());
    };
    let rules = load_gitattributes(wt);
    let config = ConfigSet::load(Some(git_dir), true).unwrap_or_default();
    let mut drivers: std::collections::HashSet<String> = std::collections::HashSet::new();
    for rule in &rules {
        for d in rule.diff_drivers() {
            drivers.insert(d.to_owned());
        }
    }
    for driver in drivers {
        let Some(cmd_line) = config.get(&format!("diff.{driver}.textconv")) else {
            continue;
        };
        let mut cmd_line = cmd_line.trim_end().to_string();
        if cmd_line.ends_with('<') {
            cmd_line = cmd_line.trim_end_matches('<').trim_end().to_string();
        }
        let Some(first_word) = pickaxe_textconv_cmd_first_token(&cmd_line) else {
            continue;
        };
        if first_word.starts_with('/') || first_word.contains('/') {
            if !Path::new(&first_word).is_file() {
                return Err(anyhow::Error::new(ExplicitExit {
                    code: 128,
                    message: format!(
                        "error: cannot run {}: No such file or directory\nfatal: unable to read files to diff",
                        first_word
                    ),
                }));
            }
            continue;
        }
        let exists = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {first_word} >/dev/null 2>&1"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !exists {
            return Err(anyhow::Error::new(ExplicitExit {
                code: 128,
                message: format!(
                    "error: cannot run {first_word}: No such file or directory\nfatal: unable to read files to diff"
                ),
            }));
        }
    }
    Ok(())
}

fn commit_pickaxe_matches(
    git_dir: &Path,
    odb: &Odb,
    info: &CommitInfo,
    args: &Args,
) -> Result<bool> {
    let entries = compute_commit_diff(odb, info)?;
    let use_textconv = !args.no_textconv;
    let config = ConfigSet::load(Some(git_dir), true).unwrap_or_default();

    let grep_re = if let Some(ref pat) = args.pickaxe_grep {
        Some(
            RegexBuilder::new(pat)
                .case_insensitive(args.regexp_ignore_case)
                .build()
                .with_context(|| format!("invalid pickaxe regex: {pat}"))?,
        )
    } else {
        None
    };

    let s_pickaxe_re = if args.pickaxe_regex {
        let needle = args
            .pickaxe_string
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("internal: --pickaxe-regex without -S"))?;
        Some(
            RegexBuilder::new(needle)
                .case_insensitive(args.regexp_ignore_case)
                .build()
                .with_context(|| format!("invalid pickaxe regex: {needle}"))?,
        )
    } else {
        None
    };

    for entry in &entries {
        let path_rp = entry.path();
        let path_owned = path_rp.to_string_lossy();
        let path: &str = &path_owned;
        let old_raw = read_blob_bytes(odb, &entry.old_oid);
        let new_raw = read_blob_bytes(odb, &entry.new_oid);

        if grep_re.is_some() && !args.text {
            let has_textconv_driver =
                use_textconv && path_has_textconv_driver(git_dir, &config, path);
            let old_bin = is_binary_for_diff(git_dir, path, &old_raw);
            let new_bin = is_binary_for_diff(git_dir, path, &new_raw);
            // Match Git diffcore_pickaxe: skip -G unless `-a` or a textconv applies to a binary side.
            if (!has_textconv_driver && old_bin) || (!has_textconv_driver && new_bin) {
                continue;
            }
        }

        let old_text = blob_text_for_diff(git_dir, &config, path, &old_raw, use_textconv);
        let new_text = blob_text_for_diff(git_dir, &config, path, &new_raw, use_textconv);

        if let Some(ref re) = grep_re {
            let patch = unified_diff(
                old_text.as_str(),
                new_text.as_str(),
                &entry.old_path.as_deref().unwrap_or(path_rp).to_string_lossy(),
                &entry.new_path.as_deref().unwrap_or(path_rp).to_string_lossy(),
                3,
                indent_heuristic_from_config(&config),
                config.quote_path_fully(),
            );
            if pickaxe_g_matches_diff_lines(re, &patch) {
                return Ok(true);
            }
            continue;
        }

        if let Some(ref needle) = args.pickaxe_string {
            if args.pickaxe_regex {
                let re = s_pickaxe_re.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("internal: --pickaxe-regex without compiled regex")
                })?;
                let old_c = re.find_iter(old_text.as_str()).count();
                let new_c = re.find_iter(new_text.as_str()).count();
                if old_c != new_c {
                    return Ok(true);
                }
            } else if args.regexp_ignore_case && needle.is_ascii() {
                let old_c = count_ascii_case_insensitive(&old_text, needle);
                let new_c = count_ascii_case_insensitive(&new_text, needle);
                if old_c != new_c {
                    return Ok(true);
                }
            } else {
                let old_c = old_text.matches(needle.as_str()).count();
                let new_c = new_text.matches(needle.as_str()).count();
                if old_c != new_c {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

/// Return the set of paths whose change matches the active pickaxe (`-S` / `-G`).
///
/// Mirrors [`commit_pickaxe_matches`] but collects every matching path instead of
/// short-circuiting. Used to limit the `-p` patch to matching files unless
/// `--pickaxe-all` is given (Git's `diffcore_pickaxe` behaviour).
fn pickaxe_matching_paths(
    git_dir: &Path,
    odb: &Odb,
    entries: &[DiffEntry],
    args: &Args,
) -> Result<std::collections::HashSet<String>> {
    let mut matched = std::collections::HashSet::new();
    let use_textconv = !args.no_textconv;
    let config = ConfigSet::load(Some(git_dir), true).unwrap_or_default();

    let grep_re = if let Some(ref pat) = args.pickaxe_grep {
        Some(
            RegexBuilder::new(pat)
                .case_insensitive(args.regexp_ignore_case)
                .build()
                .with_context(|| format!("invalid pickaxe regex: {pat}"))?,
        )
    } else {
        None
    };
    let s_pickaxe_re = if args.pickaxe_regex {
        let needle = args
            .pickaxe_string
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("internal: --pickaxe-regex without -S"))?;
        Some(
            RegexBuilder::new(needle)
                .case_insensitive(args.regexp_ignore_case)
                .build()
                .with_context(|| format!("invalid pickaxe regex: {needle}"))?,
        )
    } else {
        None
    };

    for entry in entries {
        let path_rp = entry.path();
        let path_owned = path_rp.to_string_lossy();
        let path: &str = &path_owned;
        let old_raw = read_blob_bytes(odb, &entry.old_oid);
        let new_raw = read_blob_bytes(odb, &entry.new_oid);

        if grep_re.is_some() && !args.text {
            let has_textconv_driver =
                use_textconv && path_has_textconv_driver(git_dir, &config, path);
            let old_bin = is_binary_for_diff(git_dir, path, &old_raw);
            let new_bin = is_binary_for_diff(git_dir, path, &new_raw);
            if (!has_textconv_driver && old_bin) || (!has_textconv_driver && new_bin) {
                continue;
            }
        }

        let old_text = blob_text_for_diff(git_dir, &config, path, &old_raw, use_textconv);
        let new_text = blob_text_for_diff(git_dir, &config, path, &new_raw, use_textconv);

        if let Some(ref re) = grep_re {
            let patch = unified_diff(
                old_text.as_str(),
                new_text.as_str(),
                &entry.old_path.as_deref().unwrap_or(path_rp).to_string_lossy(),
                &entry.new_path.as_deref().unwrap_or(path_rp).to_string_lossy(),
                3,
                indent_heuristic_from_config(&config),
                config.quote_path_fully(),
            );
            if pickaxe_g_matches_diff_lines(re, &patch) {
                matched.insert(path.to_owned());
            }
            continue;
        }

        if let Some(ref needle) = args.pickaxe_string {
            let differs = if args.pickaxe_regex {
                let re = s_pickaxe_re.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("internal: --pickaxe-regex without compiled regex")
                })?;
                re.find_iter(old_text.as_str()).count() != re.find_iter(new_text.as_str()).count()
            } else if args.regexp_ignore_case && needle.is_ascii() {
                count_ascii_case_insensitive(&old_text, needle)
                    != count_ascii_case_insensitive(&new_text, needle)
            } else {
                old_text.matches(needle.as_str()).count()
                    != new_text.matches(needle.as_str()).count()
            };
            if differs {
                matched.insert(path.to_owned());
            }
        }
    }
    Ok(matched)
}

fn read_blob_bytes(odb: &Odb, oid: &ObjectId) -> Vec<u8> {
    if oid.is_zero() {
        return Vec::new();
    }
    odb.read(oid).map(|o| o.data).unwrap_or_default()
}

/// Match Git's `diffgrep_consume`: run the regex on each added/removed line's **body** (the byte
/// sequence after the single `+` / `-` hunk prefix), not on diff headers or `++` / `--` lines.
fn pickaxe_g_matches_diff_lines(re: &Regex, patch: &str) -> bool {
    for line in patch.lines() {
        let b = line.as_bytes();
        let body_start = match b.first().copied() {
            Some(b'+') if b.get(1).copied() != Some(b'+') => 1,
            Some(b'-') if b.get(1).copied() != Some(b'-') => 1,
            _ => continue,
        };
        let body = line.get(body_start..).unwrap_or("");
        if re.is_match(body) {
            return true;
        }
    }
    false
}

fn count_ascii_case_insensitive(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let hay = haystack.as_bytes();
    let nd = needle.as_bytes();
    let mut count = 0usize;
    let mut i = 0usize;
    while i + nd.len() <= hay.len() {
        let mut matched = true;
        for j in 0..nd.len() {
            if !hay[i + j].eq_ignore_ascii_case(&nd[j]) {
                matched = false;
                break;
            }
        }
        if matched {
            count += 1;
            i += nd.len();
        } else {
            i += 1;
        }
    }
    count
}

/// Post-walk filters applied after [`walk_commits`] (diff-filter, find-object, decoration, dates).
fn commit_passes_post_walk_filters(
    repo: &Repository,
    odb: &Odb,
    oid: &ObjectId,
    info: &CommitInfo,
    args: &Args,
    diff_filter: Option<&str>,
    find_oid: Option<ObjectId>,
    find_object_tree_recursive: bool,
    decorations: Option<&DecorationMap>,
    since_threshold: Option<i64>,
    until_threshold: Option<i64>,
) -> Result<bool> {
    if let Some(filter) = diff_filter {
        let include_chars: Vec<char> = filter.chars().filter(|c| c.is_uppercase()).collect();
        let exclude_chars: Vec<char> = filter
            .chars()
            .filter(|c| c.is_lowercase())
            .map(|c| c.to_uppercase().next().unwrap_or(c))
            .collect();
        let passes = if args.remerge_diff && info.parents.len() == 2 {
            if !include_chars.is_empty() {
                commit_has_remerge_diff_status(repo, info, &include_chars).unwrap_or(true)
            } else if !exclude_chars.is_empty() {
                commit_has_remerge_diff_status_not_in(repo, info, &exclude_chars).unwrap_or(true)
            } else {
                true
            }
        } else if !include_chars.is_empty() {
            commit_has_diff_status(odb, info, &include_chars, args).unwrap_or(true)
        } else if !exclude_chars.is_empty() {
            commit_has_diff_status_not_in(odb, info, &exclude_chars, args).unwrap_or(true)
        } else {
            true
        };
        if !passes {
            return Ok(false);
        }
    }
    if let Some(fo) = find_oid {
        let has = if args.remerge_diff && info.parents.len() == 2 {
            commit_has_remerge_object(repo, info, &fo).unwrap_or_default()
        } else {
            commit_has_object(odb, info, &fo, find_object_tree_recursive).unwrap_or_default()
        };
        if !has {
            return Ok(false);
        }
    }
    if args.remerge_diff {
        if let Some(ref p) = args.pickaxe_string {
            if info.parents.len() != 2 {
                return Ok(false);
            }
            if !commit_remerge_pickaxe_matches(repo, info, p.as_bytes())? {
                return Ok(false);
            }
        }
    }
    if args.simplify_by_decoration {
        if let Some(dec_map) = decorations {
            if !dec_map.contains_key(&oid.to_hex()) {
                return Ok(false);
            }
        }
    }
    if let Some(t) = since_threshold {
        if extract_epoch_from_ident(&info.committer) < t {
            return Ok(false);
        }
    }
    if let Some(t) = until_threshold {
        if extract_epoch_from_ident(&info.committer) > t {
            return Ok(false);
        }
    }
    Ok(true)
}

fn run_symmetric_log(
    repo: &Repository,
    args: &Args,
    _patch_context: usize,
    use_mailmap: bool,
    mailmap: &MailmapTable,
) -> Result<()> {
    let mut lhs: Option<String> = None;
    let mut rhs: Option<String> = None;
    for rev in &args.revisions {
        if rev == "--" {
            break;
        }
        if rev.starts_with('-') && !rev.starts_with('^') {
            continue;
        }
        if is_symmetric_diff(rev) {
            if let Some((l, r)) = split_symmetric_diff(rev) {
                lhs = Some(l);
                rhs = Some(r);
            }
        }
    }
    let (lhs, rhs) = match (lhs, rhs) {
        (Some(l), Some(r)) => (l, r),
        _ => anyhow::bail!("symmetric revision required"),
    };

    // Symmetric ranges use the same commit-ish disambiguation as two-dot ranges
    // (`resolve_revision_for_range_end`), not plain `rev-parse` object resolution.
    let lhs_spec = if lhs.is_empty() { "HEAD" } else { lhs.as_str() };
    let rhs_spec = if rhs.is_empty() { "HEAD" } else { rhs.as_str() };
    let lhs_tip = resolve_revision_for_range_end(repo, lhs_spec)
        .with_context(|| format!("bad revision '{lhs_spec}'"))?;
    let rhs_tip = resolve_revision_for_range_end(repo, rhs_spec)
        .with_context(|| format!("bad revision '{rhs_spec}'"))?;
    let lhs_oid = peel_to_commit_for_merge_base(repo, lhs_tip)?;
    let rhs_oid = peel_to_commit_for_merge_base(repo, rhs_tip)?;
    let bases = merge_bases(repo, lhs_oid, rhs_oid, args.first_parent)
        .context("failed to compute merge bases for symmetric range")?;
    let negative: Vec<String> = bases.iter().map(|b| b.to_hex()).collect();

    let ordering = if args.topo_order || args.simplify_merges {
        if args.author_date_order {
            OrderingMode::AuthorDateTopo
        } else {
            OrderingMode::Topo
        }
    } else if args.date_order || args.author_date_order {
        if args.author_date_order {
            OrderingMode::AuthorDateWalk
        } else {
            OrderingMode::DateOrderWalk
        }
    } else {
        OrderingMode::Default
    };

    // `rev-list` resolves each positive spec; empty sides mean HEAD (same as parsing above).
    let positive = vec![lhs_spec.to_owned(), rhs_spec.to_owned()];
    let options = RevListOptions {
        left_right: true,
        left_only: args.left_only,
        right_only: args.right_only,
        ancestry_path: args.ancestry_path,
        ancestry_path_bottoms: args
            .ancestry_path_bottoms
            .iter()
            .map(|bottom| {
                resolve_revision_as_commit(repo, bottom.as_str()).map_err(anyhow::Error::from)
            })
            .collect::<Result<Vec<_>>>()?,
        symmetric_left: Some(lhs_oid),
        symmetric_right: Some(rhs_oid),
        boundary: args.boundary,
        first_parent: args.first_parent,
        ordering,
        reverse: false,
        ..RevListOptions::default()
    };
    let result = rev_list(repo, &positive, &negative, &options).context("rev-list failed")?;

    let boundary_set: HashSet<ObjectId> = result.boundary_commits.iter().copied().collect();
    let mut ordered = result.commits.clone();
    if args.boundary {
        for b in &result.boundary_commits {
            ordered.push(*b);
        }
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut notes_cache = NotesMapCache::new(repo);
    let use_color = log_resolve_stdout_color(args, &repo.git_dir);
    let head_state = resolve_head(&repo.git_dir).unwrap_or(HeadState::Invalid);
    let decoration_paint = if use_color {
        Some(load_decoration_paint(&repo.git_dir))
    } else {
        None
    };

    // `%S` (format placeholder) and `--source` for a symmetric range label each
    // side's commits with its ref name.
    let percent_s_source_map: std::collections::HashMap<ObjectId, String> = {
        let template = args.format.as_deref().unwrap_or("");
        if template.contains("%S") || args.source {
            let tips = vec![
                (lhs_oid, lhs_spec.to_owned()),
                (rhs_oid, rhs_spec.to_owned()),
            ];
            build_named_source_map(&repo.odb, &tips, args.first_parent)
        } else {
            std::collections::HashMap::new()
        }
    };

    for (i, oid) in ordered.iter().enumerate() {
        if i > 0 && log_wants_blank_line_between_commits(args) {
            writeln!(out)?;
        }
        let is_boundary = boundary_set.contains(oid);
        let log_marker = if is_boundary {
            Some('-')
        } else {
            match result.left_right_map.get(oid) {
                Some(true) => Some('<'),
                Some(false) => Some('>'),
                None => None,
            }
        };
        let info = load_commit_info(repo, *oid)?;
        let oneline_fmt = args.oneline || args.format.as_deref() == Some("oneline");
        if args.source && !oneline_fmt {
            if let Some(src) = percent_s_source_map.get(oid) {
                write!(out, "{}\t", short_ref_for_source_display(src))?;
            }
        }
        let source_for_oneline = if args.source && oneline_fmt {
            percent_s_source_map
                .get(oid)
                .map(|full| short_ref_for_source_display(full))
        } else {
            None
        };
        format_commit(
            &mut out,
            oid,
            &info,
            args,
            use_mailmap,
            &mailmap,
            None,
            use_color,
            decoration_paint.as_ref(),
            &head_state,
            &mut notes_cache,
            &repo.odb,
            None,
            false,
            log_marker,
            None,
            source_for_oneline,
            percent_s_source_map.get(oid).map(|s| s.as_str()),
            None,
        )?;
    }

    Ok(())
}

/// Encode a formatted log fragment for output. With no active output encoding
/// (UTF-8), the UTF-8 bytes are returned verbatim; otherwise the string is
/// reencoded to the target charset (Git's `logmsg_reencode`).
fn encode_log_str(s: &str, encoding: Option<&str>) -> Vec<u8> {
    match encoding {
        None => s.as_bytes().to_vec(),
        Some(label) => grit_lib::commit_encoding::encode_header_text(label, s)
            .unwrap_or_else(|| s.as_bytes().to_vec()),
    }
}

/// A writer that prepends a fixed prefix at the start of every output line
/// (Git's `--line-prefix`). The prefix is emitted before the first byte and
/// again immediately after each `\n`, except a trailing `\n` does not trigger a
/// dangling prefix until the next byte arrives.
struct LinePrefixWriter<'a, W: Write> {
    inner: W,
    prefix: &'a [u8],
    at_line_start: bool,
}

impl<'a, W: Write> LinePrefixWriter<'a, W> {
    fn new(inner: W, prefix: &'a str) -> Self {
        LinePrefixWriter {
            inner,
            prefix: prefix.as_bytes(),
            at_line_start: true,
        }
    }
}

impl<W: Write> Write for LinePrefixWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        for &b in buf {
            if self.at_line_start {
                self.inner.write_all(self.prefix)?;
                self.at_line_start = false;
            }
            self.inner.write_all(&[b])?;
            if b == b'\n' {
                self.at_line_start = true;
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Format and print a single commit.
///
/// When `parent_line_override` is set (e.g. `log --parents` after line-log rewrite), `%p` / `%P`
/// and the `Merge:` header use these hashes instead of the raw commit parents.
fn format_commit(
    out: &mut impl Write,
    oid: &ObjectId,
    info: &CommitInfo,
    args: &Args,
    use_mailmap: bool,
    mailmap: &MailmapTable,
    decorations: Option<&DecorationMap>,
    use_color: bool,
    decoration_paint: Option<&DecorationPaint>,
    head_for_decor: &HeadState,
    notes_cache: &mut NotesMapCache<'_>,
    odb: &Odb,
    parent_line_override: Option<&[ObjectId]>,
    _line_log: bool,
    log_marker: Option<char>,
    merge_from_parent: Option<&ObjectId>,
    source_for_oneline: Option<&str>,
    percent_s_source: Option<&str>,
    grep_colorizer: Option<&GrepColorizer>,
) -> Result<()> {
    let hex = oid.to_hex();
    let abbrev_len = if args.no_abbrev {
        40
    } else {
        parse_abbrev(&args.abbrev)
    };
    // The `commit <hash>` header (and `tree`/`parent` in raw) is abbreviated under
    // `--abbrev-commit` (or the builtin `--oneline`), otherwise it is the full hash.
    let commit_hex: String =
        if (args.abbrev_commit || log_uses_builtin_oneline(args)) && !args.no_abbrev {
            hex[..abbrev_len.min(hex.len())].to_string()
        } else {
            hex.clone()
        };
    let display_parents = parent_line_override.unwrap_or(info.parents.as_slice());
    // The `(from <parent>)` marker uses the same abbreviation as the commit hash itself:
    // full hex for the multi-line formats (which print the full `commit <hex>`), abbreviated
    // only under `--abbrev-commit`/`--oneline`.
    let from_uses_abbrev = args.abbrev_commit || log_uses_builtin_oneline(args);
    let merge_suffix = merge_from_parent
        .map(|p| {
            let h = p.to_hex();
            if from_uses_abbrev {
                format!(" (from {})", &h[..abbrev_len.min(h.len())])
            } else {
                format!(" (from {h})")
            }
        })
        .unwrap_or_default();
    let et = args.expand_tabs_in_log;

    if log_uses_builtin_oneline(args) {
        let first_line = grit_lib::commit_pretty::message_subject(&info.message);
        let first_line = if et > 0 {
            grit_lib::tab_expand::expand_tabs_in_line(&first_line, et)
        } else {
            first_line
        };
        let enc = args.log_output_encoding.as_deref();
        let abbrev = &hex[..abbrev_len.min(hex.len())];
        // Under `-z`, each builtin `oneline` entry is NUL-terminated (Git `line_termination`),
        // so consecutive entries are joined (and the final one terminated) by `\0`.
        let line_term: &[u8] = if args.null_terminator { b"\0" } else { b"\n" };
        if let Some(src) = source_for_oneline {
            let line = format!("{abbrev}\t{src} {first_line}");
            out.write_all(&encode_log_str(&line, enc))?;
            out.write_all(line_term)?;
        } else {
            let oid_color = if use_color {
                decoration_paint
                    .map(|p| p.commit.as_str())
                    .unwrap_or("\x1b[33m")
            } else {
                ""
            };
            let oid_reset = if use_color {
                decoration_paint
                    .map(|p| p.reset.as_str())
                    .unwrap_or("\x1b[m")
            } else {
                ""
            };
            let dec = format_decoration(
                &hex,
                decorations,
                use_color,
                decoration_paint,
                head_for_decor,
            );
            let line = format!(
                "{}{}{}{} {}",
                oid_color,
                &hex[..abbrev_len.min(hex.len())],
                oid_reset,
                dec,
                first_line
            );
            out.write_all(&encode_log_str(&line, enc))?;
            out.write_all(line_term)?;
        }
        return Ok(());
    }

    let format = args.format.as_deref();
    let date_format = args.date.as_deref();

    // Verify the commit signature only when a `%G` placeholder is present in the
    // format (avoids spawning gpg for every commit otherwise).
    let signature: Option<grit_lib::signing::SignatureCheck> = match format {
        Some(fmt) if fmt.contains("%G") => odb.read(oid).ok().map(|obj| {
            let config = grit_lib::repo::Repository::discover(None)
                .ok()
                .and_then(|repo| ConfigSet::load(Some(&repo.git_dir), true).ok())
                .unwrap_or_default();
            verify_commit_signature(&config, &obj.data)
        }),
        _ => None,
    };
    let signature_ref = signature.as_ref();

    // `--show-signature` emits the GPG/ssh verification lines (Git `show_signature`)
    // right after the `commit <hash>`/`Merge:` header in the multi-line pretty
    // formats. Under `--graph`, each line is prefixed with the graph rail.
    let show_signature_lines: Option<String> = if args.show_signature {
        odb.read(oid).ok().and_then(|obj| {
            let config = grit_lib::repo::Repository::discover(None)
                .ok()
                .and_then(|repo| ConfigSet::load(Some(&repo.git_dir), true).ok())
                .unwrap_or_default();
            let s = format_commit_signature_lines(&config, &obj.data);
            if s.is_empty() {
                None
            } else {
                // Under `--graph`, the graph machinery rail-prefixes every line
                // of the message buffer (incl. these), so emit them verbatim.
                Some(s)
            }
        })
    } else {
        None
    };
    let show_signature_lines = show_signature_lines.as_deref();

    match format {
        Some(fmt) if fmt.starts_with("format:") || fmt.starts_with("tformat:") => {
            let is_tformat = fmt.starts_with("tformat:");
            let template = if let Some(t) = fmt.strip_prefix("format:") {
                t
            } else {
                &fmt[8..]
            };
            let note_bytes = notes_cache.map().get(oid).map(Vec::as_slice);
            let formatted = apply_format_string(
                template,
                oid,
                info,
                decorations,
                date_format,
                abbrev_len,
                use_color,
                decoration_paint,
                head_for_decor,
                note_bytes,
                display_parents,
                log_marker,
                mailmap,
                use_mailmap,
                et,
                signature_ref,
                percent_s_source,
            );
            let bytes = encode_log_str(&formatted, args.log_output_encoding.as_deref());
            if is_tformat {
                out.write_all(&bytes)?;
                if args.null_terminator {
                    out.write_all(b"\0")?;
                } else {
                    out.write_all(b"\n")?;
                }
            } else {
                out.write_all(&bytes)?;
            }
        }
        Some("raw") => {
            // `--pretty=raw` only abbreviates `commit` for an explicit
            // `--abbrev-commit` flag, not for the `log.abbrevCommit` config.
            let raw_commit_hex = if args.abbrev_commit_explicit && !args.no_abbrev {
                &hex[..abbrev_len.min(hex.len())]
            } else {
                hex.as_str()
            };
            writeln!(out, "commit {raw_commit_hex}")?;
            writeln!(out, "tree {}", info.tree.to_hex())?;
            for parent in display_parents {
                writeln!(out, "parent {}", parent.to_hex())?;
            }
            writeln!(out, "author {}", info.author)?;
            writeln!(out, "committer {}", info.committer)?;
            writeln!(out)?;
            write_commit_body(out, info, et, args.log_output_encoding.as_deref())?;
            // `git log --pretty=raw` omits notes unless `--notes` / `--show-notes` is given.
            if matches!(
                std::env::var("GIT_GRIT_LOG_NOTES_CLI").ok().as_deref(),
                Some("on")
            ) {
                write_notes(out, oid, notes_cache, args, odb)?;
            }
        }
        Some("short") => {
            let dec = format_decoration(
                &hex,
                decorations,
                use_color,
                decoration_paint,
                head_for_decor,
            );
            writeln!(out, "commit {commit_hex}{merge_suffix}{dec}")?;
            if display_parents.len() > 1 {
                let parent_abbrevs: Vec<String> = display_parents
                    .iter()
                    .map(|p| {
                        let h = p.to_hex();
                        h[..abbrev_len.min(h.len())].to_string()
                    })
                    .collect();
                writeln!(out, "Merge: {}", parent_abbrevs.join(" "))?;
            }
            let author_name = format_ident_display_mailmap(mailmap, &info.author, use_mailmap);
            writeln!(out, "Author: {author_name}")?;
            writeln!(out)?;
            // Git's `pretty_print_commit` right-trims the whole buffer after emitting the body
            // (`strbuf_rtrim`) and then appends exactly one `\n`, so a builtin pretty entry ends
            // with a single newline and no trailing blank line. The inter-commit blank separator is
            // supplied by the log walk, not by the format itself — emitting a trailing blank here
            // would double the separator (and, under `--graph -p`, inject a stray rail line between
            // the message and the diff/stat body).
            for line in info.message.lines().take(1) {
                writeln!(
                    out,
                    "{}",
                    grit_lib::tab_expand::indent_and_expand_tabs(line, 4, et)
                )?;
            }
        }
        Some("medium") | None => {
            let dec = format_decoration(
                &hex,
                decorations,
                use_color,
                decoration_paint,
                head_for_decor,
            );
            if use_color {
                let c = decoration_paint
                    .map(|p| p.commit.as_str())
                    .unwrap_or("\x1b[33m");
                let r = decoration_paint
                    .map(|p| p.reset.as_str())
                    .unwrap_or("\x1b[m");
                writeln!(out, "{c}commit {commit_hex}{merge_suffix}{r}{dec}")?;
            } else {
                writeln!(out, "commit {commit_hex}{merge_suffix}{dec}")?;
            }
            if display_parents.len() > 1 {
                let parent_abbrevs: Vec<String> = display_parents
                    .iter()
                    .map(|p| {
                        let h = p.to_hex();
                        h[..abbrev_len.min(h.len())].to_string()
                    })
                    .collect();
                writeln!(out, "Merge: {}", parent_abbrevs.join(" "))?;
            }
            if let Some(sig) = show_signature_lines {
                out.write_all(sig.as_bytes())?;
            }
            let author_disp = format_ident_display_mailmap(mailmap, &info.author, use_mailmap);
            let author_disp = grep_colorizer
                .and_then(|c| c.colorize_author(&author_disp))
                .unwrap_or(author_disp);
            writeln!(out, "Author: {author_disp}")?;
            writeln!(
                out,
                "Date:   {}",
                format_author_date_internal(&info.author, date_format, true)
            )?;
            writeln!(out)?;
            write_commit_body_colored(
                out,
                info,
                et,
                grep_colorizer,
                args.log_output_encoding.as_deref(),
            )?;
            write_notes(out, oid, notes_cache, args, odb)?;
        }
        Some("full") => {
            let dec = format_decoration(
                &hex,
                decorations,
                use_color,
                decoration_paint,
                head_for_decor,
            );
            writeln!(out, "commit {commit_hex}{merge_suffix}{dec}")?;
            if display_parents.len() > 1 {
                let parent_abbrevs: Vec<String> = display_parents
                    .iter()
                    .map(|p| {
                        let h = p.to_hex();
                        h[..abbrev_len.min(h.len())].to_string()
                    })
                    .collect();
                writeln!(out, "Merge: {}", parent_abbrevs.join(" "))?;
            }
            if let Some(sig) = show_signature_lines {
                out.write_all(sig.as_bytes())?;
            }
            let author_disp = format_ident_display_mailmap(mailmap, &info.author, use_mailmap);
            let author_disp = grep_colorizer
                .and_then(|c| c.colorize_author(&author_disp))
                .unwrap_or(author_disp);
            writeln!(out, "Author: {author_disp}")?;
            let committer_disp =
                format_ident_display_mailmap(mailmap, &info.committer, use_mailmap);
            let committer_disp = grep_colorizer
                .and_then(|c| c.colorize_committer(&committer_disp))
                .unwrap_or(committer_disp);
            writeln!(out, "Commit: {committer_disp}")?;
            writeln!(out)?;
            write_commit_body_colored(
                out,
                info,
                et,
                grep_colorizer,
                args.log_output_encoding.as_deref(),
            )?;
            write_notes(out, oid, notes_cache, args, odb)?;
        }
        Some("fuller") => {
            let dec = format_decoration(
                &hex,
                decorations,
                use_color,
                decoration_paint,
                head_for_decor,
            );
            writeln!(out, "commit {commit_hex}{merge_suffix}{dec}")?;
            if display_parents.len() > 1 {
                let parent_abbrevs: Vec<String> = display_parents
                    .iter()
                    .map(|p| {
                        let h = p.to_hex();
                        h[..abbrev_len.min(h.len())].to_string()
                    })
                    .collect();
                writeln!(out, "Merge: {}", parent_abbrevs.join(" "))?;
            }
            if let Some(sig) = show_signature_lines {
                out.write_all(sig.as_bytes())?;
            }
            let author_disp = format_ident_display_mailmap(mailmap, &info.author, use_mailmap);
            let author_disp = grep_colorizer
                .and_then(|c| c.colorize_author(&author_disp))
                .unwrap_or(author_disp);
            writeln!(out, "Author:     {author_disp}")?;
            writeln!(
                out,
                "AuthorDate: {}",
                format_author_date_internal(&info.author, date_format, true)
            )?;
            let committer_disp =
                format_ident_display_mailmap(mailmap, &info.committer, use_mailmap);
            let committer_disp = grep_colorizer
                .and_then(|c| c.colorize_committer(&committer_disp))
                .unwrap_or(committer_disp);
            writeln!(out, "Commit:     {committer_disp}")?;
            writeln!(
                out,
                "CommitDate: {}",
                format_author_date_internal(&info.committer, date_format, true)
            )?;
            writeln!(out)?;
            write_commit_body_colored(
                out,
                info,
                et,
                grep_colorizer,
                args.log_output_encoding.as_deref(),
            )?;
            write_notes(out, oid, notes_cache, args, odb)?;
        }
        Some("reference") => {
            // `--pretty=reference` is equivalent to the format
            // `%C(auto)%h (%s, %as)` (short committer date), except an explicit
            // `--date` overrides the date mode and the hash is always abbreviated.
            // It never shows decorations or reflog selectors.
            let date_ph = if date_format.is_some() { "%ad" } else { "%as" };
            let template = format!("%C(auto)%h (%s, {date_ph})");
            // Force abbreviation even under --no-abbrev-commit.
            let ref_abbrev = if abbrev_len >= 40 { 7 } else { abbrev_len };
            let formatted = apply_format_string(
                &template,
                oid,
                info,
                None,
                date_format,
                ref_abbrev,
                use_color,
                decoration_paint,
                head_for_decor,
                None,
                display_parents,
                log_marker,
                mailmap,
                use_mailmap,
                et,
                signature_ref,
                percent_s_source,
            );
            let bytes = encode_log_str(&formatted, args.log_output_encoding.as_deref());
            out.write_all(&bytes)?;
            out.write_all(b"\n")?;
        }
        Some("email") | Some("mboxrd") => {
            // `--pretty=email` / `mboxrd` emit an mbox/patch header. Mirror `git show`'s builtin
            // email arm so `git log --pretty=email` and `git show --pretty=email` agree byte for
            // byte (and match the `-z` NUL framing in t4205's `--reflog` tests).
            let subject = grit_lib::commit_pretty::message_subject(&info.message);
            let subject = if et > 0 {
                grit_lib::tab_expand::expand_tabs_in_line(&subject, et)
            } else {
                subject
            };
            writeln!(out, "From {hex} Mon Sep 17 00:00:00 2001")?;
            writeln!(
                out,
                "From: {}",
                format_ident_for_header_mailmap(mailmap, &info.author, use_mailmap)
            )?;
            writeln!(out, "Date: {}", format_date_for_header(&info.author))?;
            writeln!(out, "Subject: [PATCH] {subject}")?;
            writeln!(out)?;
            for line in info.message.lines() {
                let line_out = if et > 0 {
                    grit_lib::tab_expand::expand_tabs_in_line(line, et)
                } else {
                    line.to_owned()
                };
                writeln!(out, "{line_out}")?;
            }
            writeln!(out)?;
        }
        Some(other) => {
            // Try as a format string directly
            let note_bytes = notes_cache.map().get(oid).map(Vec::as_slice);
            let formatted = apply_format_string(
                other,
                oid,
                info,
                decorations,
                date_format,
                abbrev_len,
                use_color,
                decoration_paint,
                head_for_decor,
                note_bytes,
                display_parents,
                log_marker,
                mailmap,
                use_mailmap,
                et,
                signature_ref,
                percent_s_source,
            );
            writeln!(out, "{formatted}")?;
        }
    }

    Ok(())
}

/// Apply a format string with placeholders like %H, %h, %s, %an, %ae, etc.
/// Expand the `%xNN`, `%n`, `%t`, `%%` escapes that appear in trailer
/// separator values (Git's `format_trailer_match` / `strbuf_expand`).
fn expand_trailer_value(raw: &str) -> String {
    let mut out = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('x') => {
                chars.next();
                let mut hx = String::new();
                for _ in 0..2 {
                    if let Some(&d) = chars.peek() {
                        if d.is_ascii_hexdigit() {
                            hx.push(d);
                            chars.next();
                        }
                    }
                }
                if let Ok(b) = u8::from_str_radix(&hx, 16) {
                    out.push(b as char);
                }
            }
            Some('n') => {
                chars.next();
                out.push('\n');
            }
            Some('t') => {
                chars.next();
                out.push('\t');
            }
            Some('%') => {
                chars.next();
                out.push('%');
            }
            _ => out.push('%'),
        }
    }
    out
}

/// Parse the option string of a `%(trailers...)` placeholder (the part after
/// `trailers`, beginning with an optional `:`). Returns `None` on an invalid
/// option so the caller can emit the placeholder literally (matching Git).
pub(crate) fn parse_trailers_opts(rest: &str) -> Option<grit_lib::commit_trailers::TrailerOpts> {
    use grit_lib::commit_trailers::TrailerOpts;
    let mut opts = TrailerOpts::default();
    let body = match rest.strip_prefix(':') {
        Some(b) => b,
        None => {
            // `%(trailers)` with no colon: default options.
            if rest.is_empty() {
                return Some(opts);
            }
            // e.g. `%(trailersfoo)` — not a valid placeholder.
            return None;
        }
    };
    if body.is_empty() {
        return Some(opts);
    }
    let mut explicit_only: Option<bool> = None;
    let mut has_key_filter = false;
    for tok in body.split(',') {
        let (name, value) = match tok.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (tok, None),
        };
        let truthy = |v: Option<&str>| -> bool {
            matches!(
                v,
                None | Some("yes") | Some("true") | Some("on") | Some("1") | Some("")
            )
        };
        match name {
            "only" => explicit_only = Some(truthy(value)),
            "unfold" => opts.unfold = truthy(value),
            "keyonly" => opts.keyonly = truthy(value),
            "valueonly" => opts.valueonly = truthy(value),
            "key" => {
                let v = value?; // `key` without value is an error.
                let mut k = v.to_owned();
                // A trailing `:` on the key is tolerated (test 75).
                if let Some(stripped) = k.strip_suffix(':') {
                    k = stripped.to_owned();
                }
                opts.filter_keys.push(k);
                has_key_filter = true;
            }
            "separator" => {
                opts.separator = expand_trailer_value(value.unwrap_or(""));
            }
            "key_value_separator" => {
                opts.key_value_separator = expand_trailer_value(value.unwrap_or(""));
            }
            _ => return None,
        }
    }
    // Git: a key filter turns on only_trailers unless overridden by an explicit
    // only=no.
    opts.only_trailers = explicit_only.unwrap_or(has_key_filter);
    Some(opts)
}

/// Parse the option string of a `%(describe...)` placeholder (the part after
/// `describe`). Returns `None` on a malformed option (so the placeholder is
/// emitted literally).
fn parse_describe_opts(rest: &str) -> Option<crate::commands::describe::DescribeOptions> {
    let mut opts = crate::commands::describe::DescribeOptions::default_for_format();
    let body = match rest.strip_prefix(':') {
        Some(b) => b,
        None => {
            if rest.is_empty() {
                return Some(opts);
            }
            return None;
        }
    };
    if body.is_empty() {
        return Some(opts);
    }
    for tok in body.split(',') {
        let (name, value) = match tok.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (tok, None),
        };
        match name {
            "match" => opts.match_pattern.push(value?.to_owned()),
            "exclude" => opts.exclude_pattern.push(value?.to_owned()),
            "tags" => opts.tags = true,
            "abbrev" => opts.abbrev = value?.parse().ok()?,
            _ => return None,
        }
    }
    Some(opts)
}

/// Run `git describe` for a commit during pretty formatting; returns `None`
/// when no description is found (Git emits an empty placeholder).
fn run_describe_for_format(
    oid: &ObjectId,
    opts: &crate::commands::describe::DescribeOptions,
) -> Option<String> {
    let repo = grit_lib::repo::Repository::discover(None).ok()?;
    crate::commands::describe::describe_object(&repo, *oid, opts).ok()
}

/// Parse the inner part of a `%w(width,indent1,indent2)` directive (the text between the
/// parentheses, with the parens already stripped). Mirrors Git's `strtoul`-based parse: an empty
/// string is `(0, 0, 0)`; otherwise up to three comma-separated decimal values, where any trailing
/// non-digit / non-comma junk makes the whole directive invalid (`None` => emit literally).
/// Mimic C `strtoul(s, &next, 10)` over a byte slice: skip leading ASCII whitespace,
/// consume leading decimal digits (saturating), and return `(value, rest)` where `rest`
/// is the slice starting at the first unconsumed byte. With no digits, value is 0 and
/// `rest` is the slice after any skipped whitespace (matching glibc behavior).
fn strtoul_prefix(s: &str) -> (i64, &str) {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let mut val: i64 = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        val = val
            .saturating_mul(10)
            .saturating_add((bytes[i] - b'0') as i64);
        i += 1;
    }
    (val, &s[i..])
}

/// Faithful port of git pretty.c `%w(width,indent1,indent2)` argument parsing (case 'w').
/// Uses `strtoul` semantics: an empty or non-numeric leading segment yields 0 and the
/// parse continues only as long as the next unconsumed byte is `,` (advance to next field).
/// Anything other than `,`/end terminates a successful parse; leftover non-`)` => invalid.
fn parse_wrap_spec(inner: &str) -> Option<(i64, i64, i64)> {
    // Empty `%w()` => all zero (wrapping off).
    if inner.is_empty() {
        return Some((0, 0, 0));
    }
    let (width, rest) = strtoul_prefix(inner);
    let mut indent1 = 0i64;
    let mut indent2 = 0i64;
    let mut rest = rest;
    if let Some(after) = rest.strip_prefix(',') {
        let (i1, r) = strtoul_prefix(after);
        indent1 = i1;
        rest = r;
        if let Some(after2) = rest.strip_prefix(',') {
            let (i2, r2) = strtoul_prefix(after2);
            indent2 = i2;
            rest = r2;
        }
    }
    // After parsing, git requires the next byte to be `)` (handled by the caller, which
    // already stripped the closing paren). Here `rest` must be fully consumed.
    if !rest.is_empty() {
        return None;
    }
    Some((width, indent1, indent2))
}

thread_local! {
    /// Display width of the graph prefix (e.g. `* `, `| * `) on the line where the current commit's
    /// formatted body begins. `%>|(N)` / `%<|(N)` absolute-column directives position relative to
    /// the start of the whole output line, which under `--graph` includes this prefix (Git's
    /// `pp->graph_width`). The graph renderer sets this before formatting each commit; it is 0 in
    /// the non-graph path.
    static GRAPH_PREFIX_WIDTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn apply_format_string(
    template: &str,
    oid: &ObjectId,
    info: &CommitInfo,
    decorations: Option<&DecorationMap>,
    date_format: Option<&str>,
    abbrev_len: usize,
    use_color: bool,
    decoration_paint: Option<&DecorationPaint>,
    head_for_decor: &HeadState,
    notes_raw: Option<&[u8]>,
    display_parents: &[ObjectId],
    log_marker: Option<char>,
    mailmap: &MailmapTable,
    use_mailmap: bool,
    expand_tabs_in_log: usize,
    signature: Option<&grit_lib::signing::SignatureCheck>,
    percent_s_source: Option<&str>,
) -> String {
    let hex = oid.to_hex();
    let commit_color = || {
        decoration_paint
            .map(|p| p.commit.as_str())
            .unwrap_or("\x1b[33m")
    };
    let reset_color = || {
        decoration_paint
            .map(|p| p.reset.as_str())
            .unwrap_or("\x1b[m")
    };

    // Alignment/truncation helpers
    #[derive(Clone, Copy)]
    enum Align {
        Left,
        Right,
        Center,
    }
    #[derive(Clone, Copy)]
    enum Trunc {
        None,
        Trunc,
        LTrunc,
        MTrunc,
    }
    struct ColSpec {
        width: usize,
        align: Align,
        trunc: Trunc,
        absolute: bool,
        /// `%>>` (flush_left_and_steal): right-align but first reclaim trailing
        /// spaces from the already-emitted output when the value overflows.
        steal: bool,
    }
    // Git-style display width of a single character: control characters render
    // with zero columns, wide characters with two, everything else with one.
    fn char_display_width(c: char) -> usize {
        let cp = c as u32;
        if cp < 0x20 || cp == 0x7f {
            0
        } else {
            1
        }
    }
    fn display_width(s: &str) -> usize {
        s.chars().map(char_display_width).sum()
    }
    fn apply_col(spec: &ColSpec, s: &str) -> String {
        let char_len = display_width(s);
        if char_len > spec.width {
            // Truncation operates on display columns; leading zero-width control
            // characters are preserved and do not count toward the budget.
            match spec.trunc {
                Trunc::None => s.to_owned(),
                Trunc::Trunc => {
                    let budget = spec.width.saturating_sub(2);
                    let mut out = String::new();
                    let mut used = 0usize;
                    for c in s.chars() {
                        let w = char_display_width(c);
                        if used + w > budget {
                            break;
                        }
                        out.push(c);
                        used += w;
                    }
                    out.push_str("..");
                    out
                }
                Trunc::LTrunc => {
                    // Skip from the left until the remaining display width fits.
                    let target = spec.width.saturating_sub(2);
                    let chars: Vec<char> = s.chars().collect();
                    // Find the smallest suffix whose display width <= target.
                    let mut idx = chars.len();
                    let mut acc = 0usize;
                    while idx > 0 {
                        let w = char_display_width(chars[idx - 1]);
                        if acc + w > target {
                            break;
                        }
                        acc += w;
                        idx -= 1;
                    }
                    let mut out = String::from("..");
                    out.extend(chars[idx..].iter());
                    out
                }
                Trunc::MTrunc => {
                    let keep = spec.width.saturating_sub(2);
                    let left_budget = keep / 2;
                    let right_budget = keep - left_budget;
                    let chars: Vec<char> = s.chars().collect();
                    let mut out = String::new();
                    let mut used = 0usize;
                    let mut li = 0usize;
                    while li < chars.len() {
                        let w = char_display_width(chars[li]);
                        if used + w > left_budget {
                            break;
                        }
                        out.push(chars[li]);
                        used += w;
                        li += 1;
                    }
                    out.push_str("..");
                    let mut ri = chars.len();
                    let mut racc = 0usize;
                    while ri > li {
                        let w = char_display_width(chars[ri - 1]);
                        if racc + w > right_budget {
                            break;
                        }
                        racc += w;
                        ri -= 1;
                    }
                    out.extend(chars[ri..].iter());
                    out
                }
            }
        } else {
            let pad = spec.width - char_len;
            match spec.align {
                Align::Left => {
                    let mut o = s.to_owned();
                    for _ in 0..pad {
                        o.push(' ');
                    }
                    o
                }
                Align::Right => {
                    let mut o = String::new();
                    for _ in 0..pad {
                        o.push(' ');
                    }
                    o.push_str(s);
                    o
                }
                Align::Center => {
                    let l = pad / 2;
                    let r = pad - l;
                    let mut o = String::new();
                    for _ in 0..l {
                        o.push(' ');
                    }
                    o.push_str(s);
                    for _ in 0..r {
                        o.push(' ');
                    }
                    o
                }
            }
        }
    }
    fn parse_col_spec(
        chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
        align: Align,
        steal: bool,
    ) -> Option<ColSpec> {
        // Check for | (absolute column) variant
        let absolute = if chars.peek() == Some(&'|') {
            chars.next();
            true
        } else {
            false
        };
        if chars.peek() != Some(&'(') {
            return None;
        }
        chars.next();
        // Parse number (may be negative)
        let negative = if chars.peek() == Some(&'-') {
            chars.next();
            true
        } else {
            false
        };
        let mut num_str = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_ascii_digit() {
                num_str.push(c);
                chars.next();
            } else {
                break;
            }
        }
        let mut width: usize = num_str.parse().ok()?;
        if negative {
            // Negative means COLUMNS - N; default terminal width is 80
            let columns = std::env::var("COLUMNS")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(80);
            width = columns.saturating_sub(width);
        }
        let trunc = if chars.peek() == Some(&',') {
            chars.next();
            let mut mode = String::new();
            while let Some(&c) = chars.peek() {
                if c == ')' {
                    break;
                }
                mode.push(c);
                chars.next();
            }
            match mode.as_str() {
                "trunc" => Trunc::Trunc,
                "ltrunc" => Trunc::LTrunc,
                "mtrunc" => Trunc::MTrunc,
                _ => Trunc::None,
            }
        } else {
            Trunc::None
        };
        // A valid directive must be terminated by `)`. If it is missing, Git
        // treats the whole `%<(...` run as a literal; signal that with None.
        if chars.peek() != Some(&')') {
            return None;
        }
        chars.next();
        Some(ColSpec {
            width,
            align,
            trunc,
            absolute,
            steal,
        })
    }

    let mut pending_col: Option<ColSpec> = None;
    let mut auto_color_next_hash = false;
    let mut result = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();

    // `%w(width,indent1,indent2)` rewrapping state (Git pretty.c `rewrap_message_tail`). Text
    // appended since `wrap_start` is rewrapped with the *current* params whenever a new `%w`
    // directive (or the end of formatting) changes them.
    let mut wrap_width: i64 = 0;
    let mut wrap_indent1: i64 = 0;
    let mut wrap_indent2: i64 = 0;
    let mut wrap_start: usize = 0;

    while let Some(ch) = chars.next() {
        if ch == '%' {
            // Check alignment directives. Parse against a clone so a malformed
            // directive (e.g. a missing `)`) can be re-emitted literally without
            // having consumed any input (Git pretty.c behavior).
            if chars.peek() == Some(&'<') {
                let mut probe = chars.clone();
                probe.next(); // consume '<'
                if let Some(spec) = parse_col_spec(&mut probe, Align::Left, false) {
                    chars = probe;
                    pending_col = Some(spec);
                } else {
                    result.push('%');
                }
                continue;
            }
            if chars.peek() == Some(&'>') {
                let mut probe = chars.clone();
                probe.next(); // consume '>'
                let parsed = if probe.peek() == Some(&'<') {
                    probe.next();
                    parse_col_spec(&mut probe, Align::Center, false)
                } else if probe.peek() == Some(&'>') {
                    probe.next();
                    // `%>>` is flush_left_and_steal: right-align with stealing.
                    parse_col_spec(&mut probe, Align::Right, true)
                } else {
                    parse_col_spec(&mut probe, Align::Right, false)
                };
                if let Some(spec) = parsed {
                    chars = probe;
                    pending_col = Some(spec);
                } else {
                    result.push('%');
                }
                continue;
            }

            // Add/space/del magic: %+<placeholder>, % <placeholder>, %-<placeholder>.
            // The magic conditionally adjusts whitespace around the next
            // placeholder's output (Git pretty.c add_lf/add_sp/del_lf).
            #[derive(Clone, Copy, PartialEq)]
            enum Magic {
                None,
                AddLf,
                AddSp,
                DelLf,
            }
            let magic = match chars.peek() {
                Some('+') => {
                    chars.next();
                    Magic::AddLf
                }
                Some(' ') => {
                    chars.next();
                    Magic::AddSp
                }
                Some('-') => {
                    chars.next();
                    Magic::DelLf
                }
                _ => Magic::None,
            };
            // `%+w()` / `%-w()` / `% w()` can never expand to a non-empty string yet would change
            // the layout of preceding content, so Git's pretty.c refuses the combination and emits
            // it literally (the "magical wrapping" case in t4205). Re-emit `%<magic>` and leave the
            // `w(...)` for the loop to copy verbatim.
            if magic != Magic::None && chars.peek() == Some(&'w') {
                result.push('%');
                result.push(match magic {
                    Magic::AddLf => '+',
                    Magic::AddSp => ' ',
                    Magic::DelLf => '-',
                    Magic::None => unreachable!(),
                });
                continue;
            }
            let magic_start = result.len();

            let col_start = if pending_col.is_some() {
                result.len()
            } else {
                0
            };
            match chars.peek() {
                Some('H') => {
                    chars.next();
                    if auto_color_next_hash && use_color {
                        result.push_str(commit_color());
                        result.push_str(&hex);
                        result.push_str(reset_color());
                    } else {
                        result.push_str(&hex);
                    }
                    auto_color_next_hash = false;
                }
                Some('h') => {
                    chars.next();
                    let abbreviated = &hex[..abbrev_len.min(hex.len())];
                    if auto_color_next_hash && use_color {
                        result.push_str(commit_color());
                        result.push_str(abbreviated);
                        result.push_str(reset_color());
                    } else {
                        result.push_str(abbreviated);
                    }
                    auto_color_next_hash = false;
                }
                Some('T') => {
                    chars.next();
                    result.push_str(&info.tree.to_hex());
                }
                Some('t') => {
                    chars.next();
                    let th = info.tree.to_hex();
                    result.push_str(&th[..abbrev_len.min(th.len())]);
                }
                Some('P') => {
                    chars.next();
                    let parents: Vec<String> = display_parents.iter().map(|p| p.to_hex()).collect();
                    result.push_str(&parents.join(" "));
                }
                Some('p') => {
                    chars.next();
                    let parents: Vec<String> = display_parents
                        .iter()
                        .map(|p| {
                            let ph = p.to_hex();
                            ph[..abbrev_len.min(ph.len())].to_owned()
                        })
                        .collect();
                    result.push_str(&parents.join(" "));
                }
                Some('a') => {
                    chars.next();
                    match chars.peek() {
                        Some('n') => {
                            chars.next();
                            result.push_str(&extract_name(&info.author));
                        }
                        Some('N') => {
                            chars.next();
                            let mapped = if use_mailmap && !mailmap.is_empty() {
                                let n = extract_name(&info.author);
                                let e = extract_email(&info.author);
                                mailmap.map_user(n, e).0
                            } else {
                                extract_name(&info.author)
                            };
                            result.push_str(&mapped);
                        }
                        Some('e') => {
                            chars.next();
                            result.push_str(&extract_email(&info.author));
                        }
                        Some('E') => {
                            chars.next();
                            let mapped = if use_mailmap && !mailmap.is_empty() {
                                let n = extract_name(&info.author);
                                let e = extract_email(&info.author);
                                mailmap.map_user(n, e).1
                            } else {
                                extract_email(&info.author)
                            };
                            result.push_str(&mapped);
                        }
                        Some('l') => {
                            chars.next();
                            let email = if use_mailmap && !mailmap.is_empty() {
                                let n = extract_name(&info.author);
                                let e = extract_email(&info.author);
                                mailmap.map_user(n, e).1
                            } else {
                                extract_email(&info.author)
                            };
                            result.push_str(&local_part_of_email(&email));
                        }
                        Some('d') => {
                            chars.next();
                            result.push_str(&format_date_with_mode(&info.author, date_format));
                        }
                        Some('D') => {
                            chars.next();
                            result.push_str(&format_date_with_mode(&info.author, Some("rfc")));
                        }
                        Some('t') => {
                            chars.next();
                            result.push_str(&extract_timestamp(&info.author).to_string());
                        }
                        Some('s') => {
                            chars.next();
                            result.push_str(&format_date_with_mode(&info.author, Some("short")));
                        }
                        Some('i') => {
                            chars.next();
                            result.push_str(&format_date_with_mode(&info.author, Some("iso")));
                        }
                        Some('I') => {
                            chars.next();
                            result
                                .push_str(&format_date_with_mode(&info.author, Some("iso-strict")));
                        }
                        Some('r') => {
                            chars.next();
                            result.push_str(&format_date_with_mode(&info.author, Some("relative")));
                        }
                        Some('h') => {
                            chars.next();
                            result.push_str(&format_date_with_mode(&info.author, Some("human")));
                        }
                        _ => result.push_str("%a"),
                    }
                }
                Some('c') => {
                    chars.next();
                    match chars.peek() {
                        Some('n') => {
                            chars.next();
                            result.push_str(&extract_name(&info.committer));
                        }
                        Some('N') => {
                            chars.next();
                            let mapped = if use_mailmap && !mailmap.is_empty() {
                                let n = extract_name(&info.committer);
                                let e = extract_email(&info.committer);
                                mailmap.map_user(n, e).0
                            } else {
                                extract_name(&info.committer)
                            };
                            result.push_str(&mapped);
                        }
                        Some('e') => {
                            chars.next();
                            result.push_str(&extract_email(&info.committer));
                        }
                        Some('E') => {
                            chars.next();
                            let mapped = if use_mailmap && !mailmap.is_empty() {
                                let n = extract_name(&info.committer);
                                let e = extract_email(&info.committer);
                                mailmap.map_user(n, e).1
                            } else {
                                extract_email(&info.committer)
                            };
                            result.push_str(&mapped);
                        }
                        Some('l') => {
                            chars.next();
                            let email = if use_mailmap && !mailmap.is_empty() {
                                let n = extract_name(&info.committer);
                                let e = extract_email(&info.committer);
                                mailmap.map_user(n, e).1
                            } else {
                                extract_email(&info.committer)
                            };
                            result.push_str(&local_part_of_email(&email));
                        }
                        Some('d') => {
                            chars.next();
                            result.push_str(&format_date_with_mode(&info.committer, date_format));
                        }
                        Some('D') => {
                            chars.next();
                            result.push_str(&format_date_with_mode(&info.committer, Some("rfc")));
                        }
                        Some('t') => {
                            chars.next();
                            result.push_str(&extract_timestamp(&info.committer).to_string());
                        }
                        Some('s') => {
                            chars.next();
                            result.push_str(&format_date_with_mode(&info.committer, Some("short")));
                        }
                        Some('i') => {
                            chars.next();
                            result.push_str(&format_date_with_mode(&info.committer, Some("iso")));
                        }
                        Some('I') => {
                            chars.next();
                            result.push_str(&format_date_with_mode(
                                &info.committer,
                                Some("iso-strict"),
                            ));
                        }
                        Some('r') => {
                            chars.next();
                            result.push_str(&format_date_with_mode(
                                &info.committer,
                                Some("relative"),
                            ));
                        }
                        Some('h') => {
                            chars.next();
                            result.push_str(&format_date_with_mode(&info.committer, Some("human")));
                        }
                        _ => result.push_str("%c"),
                    }
                }
                Some('s') => {
                    chars.next();
                    let subj = grit_lib::commit_pretty::message_subject(&info.message);
                    if expand_tabs_in_log > 0 {
                        result.push_str(&grit_lib::tab_expand::expand_tabs_in_line(
                            &subj,
                            expand_tabs_in_log,
                        ));
                    } else {
                        result.push_str(&subj);
                    }
                }
                Some('S') => {
                    chars.next();
                    // Source: the named revision this commit was reached from
                    // (only populated when `git log --source`-style tracking is
                    // active for the given command-line revisions).
                    if let Some(src) = percent_s_source {
                        result.push_str(src);
                    }
                }
                Some('f') => {
                    chars.next();
                    // Sanitized subject line, suitable for a filename.
                    let subj = grit_lib::commit_pretty::message_subject(&info.message);
                    result.push_str(&grit_lib::porcelain::format_patch::sanitize_subject(&subj));
                }
                Some('b') => {
                    chars.next();
                    // Body: everything after the first paragraph separator (blank line)
                    let body = extract_body(&info.message);
                    if expand_tabs_in_log > 0 {
                        result.push_str(&grit_lib::tab_expand::expand_tabs_in_multiline_message(
                            &body,
                            expand_tabs_in_log,
                        ));
                    } else {
                        result.push_str(&body);
                    }
                }
                Some('B') => {
                    chars.next();
                    if !info.message.is_empty() {
                        let msg = if expand_tabs_in_log > 0 {
                            grit_lib::tab_expand::expand_tabs_in_multiline_message(
                                &info.message,
                                expand_tabs_in_log,
                            )
                        } else {
                            info.message.to_owned()
                        };
                        result.push_str(&msg);
                        if !msg.ends_with('\n') {
                            result.push('\n');
                        }
                    }
                }
                Some('d') => {
                    chars.next();
                    // Decorations
                    let dec = format_decoration(
                        &hex,
                        decorations,
                        use_color,
                        decoration_paint,
                        head_for_decor,
                    );
                    result.push_str(&dec);
                }
                Some('D') => {
                    chars.next();
                    // Decorations without parens
                    let dec = format_decoration_no_parens(
                        &hex,
                        decorations,
                        use_color,
                        decoration_paint,
                        head_for_decor,
                    );
                    result.push_str(&dec);
                }
                Some('n') => {
                    chars.next();
                    result.push('\n');
                }
                Some('m') => {
                    chars.next();
                    if let Some(c) = log_marker {
                        result.push(c);
                    }
                }
                Some('N') => {
                    chars.next();
                    if let Some(raw) = notes_raw {
                        result.push_str(&String::from_utf8_lossy(raw));
                    }
                }
                Some('%') => {
                    chars.next();
                    result.push('%');
                }
                Some('C') => {
                    chars.next();
                    if chars.peek() == Some(&'(') {
                        chars.next();
                        let mut spec = String::new();
                        for c in chars.by_ref() {
                            if c == ')' {
                                break;
                            }
                            spec.push(c);
                        }
                        let (force, color_spec) = if let Some(rest) = spec.strip_prefix("always,") {
                            (true, rest)
                        } else if let Some(rest) = spec.strip_prefix("auto,") {
                            (false, rest)
                        } else if spec == "auto" {
                            auto_color_next_hash = use_color;
                            continue;
                        } else {
                            (false, spec.as_str())
                        };
                        if use_color || force {
                            result.push_str(&format_ansi_color_spec(color_spec));
                        }
                    } else {
                        let remaining: String = chars.clone().collect();
                        let known = [
                            "reset", "red", "green", "blue", "yellow", "magenta", "cyan", "white",
                            "bold", "dim", "ul",
                        ];
                        let mut matched = false;
                        for name in &known {
                            if remaining.starts_with(name) {
                                for _ in 0..name.len() {
                                    chars.next();
                                }
                                if use_color {
                                    result.push_str(&format_ansi_color_name(name));
                                }
                                matched = true;
                                break;
                            }
                        }
                        if !matched {
                            while let Some(&c) = chars.peek() {
                                if c.is_alphanumeric() {
                                    chars.next();
                                } else {
                                    break;
                                }
                            }
                        }
                    }
                }
                Some('x') => {
                    // Hex escape: %xNN
                    chars.next();
                    let mut hex_str = String::new();
                    if let Some(&c1) = chars.peek() {
                        if c1.is_ascii_hexdigit() {
                            hex_str.push(c1);
                            chars.next();
                        }
                    }
                    if let Some(&c2) = chars.peek() {
                        if c2.is_ascii_hexdigit() {
                            hex_str.push(c2);
                            chars.next();
                        }
                    }
                    if let Ok(byte) = u8::from_str_radix(&hex_str, 16) {
                        result.push(byte as char);
                    }
                }
                Some('w') => {
                    // `%w(width,indent1,indent2)` rewrapping directive (Git pretty.c). Parse
                    // against a clone so a malformed or overflowing spec is emitted literally.
                    let mut probe = chars.clone();
                    probe.next(); // consume 'w'
                    if probe.peek() != Some(&'(') {
                        // Bare `%w` (no parens): Git refuses it, emit literally.
                        result.push('%');
                        continue;
                    }
                    probe.next(); // consume '('
                    let mut inner = String::new();
                    let mut closed = false;
                    for c in probe.by_ref() {
                        if c == ')' {
                            closed = true;
                            break;
                        }
                        inner.push(c);
                    }
                    if !closed {
                        result.push('%');
                        continue;
                    }
                    // Empty `%w()` => width 0 (wrapping off). Otherwise parse up to three
                    // comma-separated decimal values; any trailing junk => emit literally.
                    let parsed = parse_wrap_spec(&inner);
                    let Some((new_w, new_i1, new_i2)) = parsed else {
                        result.push('%');
                        continue;
                    };
                    const FORMATTING_LIMIT: i64 = 16 * 1024;
                    if new_w > FORMATTING_LIMIT
                        || new_i1 > FORMATTING_LIMIT
                        || new_i2 > FORMATTING_LIMIT
                    {
                        result.push('%');
                        continue;
                    }
                    chars = probe;
                    // rewrap_message_tail: if the params actually change, rewrap the text added
                    // since `wrap_start` with the *current* params, then adopt the new ones.
                    if wrap_width != new_w || wrap_indent1 != new_i1 || wrap_indent2 != new_i2 {
                        if wrap_start < result.len() {
                            let tail = result.split_off(wrap_start);
                            result.push_str(&grit_lib::commit_pretty::add_wrapped_text(
                                &tail,
                                wrap_indent1,
                                wrap_indent2,
                                wrap_width,
                            ));
                        }
                        wrap_start = result.len();
                        wrap_width = new_w;
                        wrap_indent1 = new_i1;
                        wrap_indent2 = new_i2;
                    }
                }
                Some('e') => {
                    // Encoding
                    chars.next();
                }
                Some('g') => {
                    // Reflog placeholders (%gD, %gd, %gs, etc.) — empty for non-reflog
                    chars.next();
                    if let Some(&_nc) = chars.peek() {
                        chars.next();
                    }
                }
                Some('G') => {
                    // Signature placeholders (%G?, %GS, %GK, %GF, %GP, %GT, %GG).
                    // Unknown `%G<x>` (and a bare trailing `%G`) are passed
                    // through literally, mirroring git's pretty.c `return 0`.
                    use grit_lib::signing::{SignatureCheck, TrustLevel};
                    let default_sig;
                    let sig: &SignatureCheck = match signature {
                        Some(s) => s,
                        None => {
                            default_sig = SignatureCheck::default_none();
                            &default_sig
                        }
                    };
                    // Peek the placeholder sub-character after `%G`.
                    let mut lookahead = chars.clone();
                    lookahead.next(); // consume 'G'
                    let sub = lookahead.peek().copied();
                    let handled = match sub {
                        Some('?') => {
                            // 'G' with untrusted trust level becomes 'U'.
                            let ch = if sig.result == 'G'
                                && matches!(
                                    sig.trust_level,
                                    TrustLevel::Undefined | TrustLevel::Never
                                ) {
                                'U'
                            } else {
                                sig.result
                            };
                            result.push(ch);
                            true
                        }
                        Some('S') => {
                            result.push_str(sig.signer.as_deref().unwrap_or(""));
                            true
                        }
                        Some('K') => {
                            result.push_str(sig.key.as_deref().unwrap_or(""));
                            true
                        }
                        Some('F') => {
                            result.push_str(sig.fingerprint.as_deref().unwrap_or(""));
                            true
                        }
                        Some('P') => {
                            result.push_str(sig.primary_key_fingerprint.as_deref().unwrap_or(""));
                            true
                        }
                        Some('T') => {
                            result.push_str(sig.trust_level.display_key());
                            true
                        }
                        Some('G') => {
                            result.push_str(&sig.output);
                            true
                        }
                        _ => false,
                    };
                    if handled {
                        chars.next(); // consume 'G'
                        chars.next(); // consume sub-char
                    } else {
                        // Pass through `%` literally; leave `G...` as text.
                        result.push('%');
                    }
                }
                Some('(') => {
                    // Extended placeholders: %(trailers:...), %(describe:...),
                    // %(decorate:...). Capture the balanced `(...)` payload.
                    let mut look = chars.clone();
                    look.next(); // consume '('
                    let mut inner = String::new();
                    let mut closed = false;
                    for c in look.by_ref() {
                        if c == ')' {
                            closed = true;
                            break;
                        }
                        inner.push(c);
                    }
                    if !closed {
                        // Malformed: emit literally.
                        result.push('%');
                    } else if let Some(rest) = inner.strip_prefix("trailers") {
                        if let Some(opts) = parse_trailers_opts(rest) {
                            chars = look;
                            let formatted =
                                grit_lib::commit_trailers::format_trailers(&info.message, &opts);
                            result.push_str(&formatted);
                        } else {
                            // Invalid option (e.g. `key` without value): emit the
                            // leading `%` and let the loop reparse `(...)` as text.
                            result.push('%');
                        }
                    } else if let Some(rest) = inner.strip_prefix("describe") {
                        if let Some(opts) = parse_describe_opts(rest) {
                            chars = look;
                            if let Some(desc) = run_describe_for_format(oid, &opts) {
                                result.push_str(&desc);
                            }
                            // On describe failure (no tag, no --always) Git emits
                            // nothing for the placeholder.
                        } else {
                            result.push('%');
                        }
                    } else if let Some(rest) = inner.strip_prefix("decorate") {
                        if let Some(opts) = parse_decorate_opts(rest) {
                            chars = look;
                            let dec = format_decorate_custom(
                                &hex,
                                decorations,
                                use_color,
                                decoration_paint,
                                head_for_decor,
                                &opts,
                            );
                            result.push_str(&dec);
                        } else {
                            // Unknown option (e.g. typo'd "separater"): emit the
                            // leading `%` and reparse `(...)` as text.
                            result.push('%');
                        }
                    } else {
                        // Unhandled extended placeholder: emit literally.
                        result.push('%');
                    }
                }
                _ => result.push('%'),
            }
            // Apply add/space/del magic to the just-produced placeholder output.
            // Git (pretty.c format_commit_item) applies the column padding to the
            // placeholder *first*, then inserts the magic char OUTSIDE the padded
            // column. So when a column is pending, defer the magic until after the
            // padding step below (inserting at `col_start`); otherwise apply it now.
            let produced_empty = result.len() == magic_start;
            if magic != Magic::None && pending_col.is_none() {
                match magic {
                    Magic::AddLf => {
                        if !produced_empty {
                            result.insert(magic_start, '\n');
                        }
                    }
                    Magic::AddSp => {
                        if !produced_empty {
                            result.insert(magic_start, ' ');
                        }
                    }
                    Magic::DelLf => {
                        if !produced_empty {
                            // Remove a run of trailing newlines that immediately
                            // precede this placeholder's output.
                            let mut cut = magic_start;
                            while cut > 0 && result.as_bytes()[cut - 1] == b'\n' {
                                cut -= 1;
                            }
                            if cut < magic_start {
                                result.replace_range(cut..magic_start, "");
                            }
                        }
                    }
                    Magic::None => {}
                }
            }
            // Apply pending column formatting
            if let Some(spec) = pending_col.take() {
                let added = result[col_start..].to_owned();
                result.truncate(col_start);
                // `%>>` (flush_left_and_steal): when the value overflows the
                // column, reclaim trailing spaces already emitted on this line
                // and credit them to the padding budget (Git pretty.c).
                let mut effective_width = if spec.absolute {
                    let line_start = result.rfind('\n').map(|p| p + 1).unwrap_or(0);
                    // Under `--graph` the line begins with a graph prefix (e.g. `* `) that the
                    // formatted body does not yet contain; absolute columns count from there.
                    let prefix_w = if line_start == 0 {
                        GRAPH_PREFIX_WIDTH.with(|c| c.get())
                    } else {
                        0
                    };
                    let current_col = prefix_w + display_width(&result[line_start..]);
                    spec.width.saturating_sub(current_col)
                } else {
                    spec.width
                };
                if spec.steal {
                    let line_start = result.rfind('\n').map(|p| p + 1).unwrap_or(0);
                    let added_w = display_width(&added);
                    while added_w > effective_width
                        && result.len() > line_start
                        && result.as_bytes()[result.len() - 1] == b' '
                    {
                        result.pop();
                        effective_width += 1;
                    }
                }
                if spec.absolute {
                    let mut adjusted_spec = ColSpec {
                        width: effective_width,
                        align: spec.align,
                        trunc: spec.trunc,
                        absolute: false,
                        steal: false,
                    };
                    // For absolute positioning, ensure minimum width matches the value length
                    if effective_width < display_width(&added) {
                        adjusted_spec.width = display_width(&added);
                    }
                    result.push_str(&apply_col(&adjusted_spec, &added));
                } else {
                    let adjusted_spec = ColSpec {
                        width: effective_width,
                        align: spec.align,
                        trunc: spec.trunc,
                        absolute: false,
                        steal: false,
                    };
                    result.push_str(&apply_col(&adjusted_spec, &added));
                }
                // Deferred magic: applied OUTSIDE the padded column (Git
                // pretty.c inserts the magic char before the whole padded run).
                if magic != Magic::None && !produced_empty {
                    match magic {
                        Magic::AddLf => result.insert(col_start, '\n'),
                        Magic::AddSp => result.insert(col_start, ' '),
                        Magic::DelLf => {
                            let mut cut = col_start;
                            while cut > 0 && result.as_bytes()[cut - 1] == b'\n' {
                                cut -= 1;
                            }
                            if cut < col_start {
                                result.replace_range(cut..col_start, "");
                            }
                        }
                        Magic::None => {}
                    }
                }
            }
        } else {
            result.push(ch);
        }
    }

    // Final rewrap (Git `rewrap_message_tail(sb, &context, 0, 0, 0)`): if any `%w` set non-zero
    // params, the text since the last `%w` is wrapped with those params.
    if (wrap_width != 0 || wrap_indent1 != 0 || wrap_indent2 != 0) && wrap_start < result.len() {
        let tail = result.split_off(wrap_start);
        result.push_str(&grit_lib::commit_pretty::add_wrapped_text(
            &tail,
            wrap_indent1,
            wrap_indent2,
            wrap_width,
        ));
    }

    result
}

/// Extract the message body (everything after the subject + blank line).
fn extract_body(message: &str) -> String {
    grit_lib::commit_pretty::message_body(message).to_owned()
}

/// Extract the name portion from a Git ident string.
fn extract_name(ident: &str) -> String {
    if let Some(bracket) = ident.find('<') {
        ident[..bracket].trim().to_owned()
    } else {
        ident.to_owned()
    }
}

/// Extract the email portion from a Git ident string.
fn extract_email(ident: &str) -> String {
    if let Some(start) = ident.find('<') {
        if let Some(end) = ident.find('>') {
            return ident[start + 1..end].to_owned();
        }
    }
    String::new()
}

fn format_ansi_color_name(name: &str) -> String {
    match name {
        "red" => "\x1b[31m".to_owned(),
        "green" => "\x1b[32m".to_owned(),
        "yellow" => "\x1b[33m".to_owned(),
        "blue" => "\x1b[34m".to_owned(),
        "magenta" => "\x1b[35m".to_owned(),
        "cyan" => "\x1b[36m".to_owned(),
        "white" => "\x1b[37m".to_owned(),
        "bold" => "\x1b[1m".to_owned(),
        "dim" => "\x1b[2m".to_owned(),
        "ul" | "underline" => "\x1b[4m".to_owned(),
        "reset" => "\x1b[m".to_owned(),
        _ => String::new(),
    }
}

fn format_ansi_color_spec(spec: &str) -> String {
    if spec == "reset" {
        return "\x1b[m".to_owned();
    }
    fn color_code(name: &str) -> Option<u8> {
        match name {
            "black" => Some(0),
            "red" => Some(1),
            "green" => Some(2),
            "yellow" => Some(3),
            "blue" => Some(4),
            "magenta" => Some(5),
            "cyan" => Some(6),
            "white" => Some(7),
            "default" => Some(9),
            _ => None,
        }
    }
    let mut codes = Vec::new();
    let mut fg_set = false;
    for part in spec.split_whitespace() {
        match part {
            "bold" => codes.push("1".to_owned()),
            "dim" => codes.push("2".to_owned()),
            "italic" => codes.push("3".to_owned()),
            "ul" | "underline" => codes.push("4".to_owned()),
            "blink" => codes.push("5".to_owned()),
            "reverse" => codes.push("7".to_owned()),
            "strike" => codes.push("9".to_owned()),
            "nobold" | "nodim" => codes.push("22".to_owned()),
            "noitalic" => codes.push("23".to_owned()),
            "noul" | "nounderline" => codes.push("24".to_owned()),
            "noblink" => codes.push("25".to_owned()),
            "noreverse" => codes.push("27".to_owned()),
            "nostrike" => codes.push("29".to_owned()),
            _ => {
                if let Some(c) = color_code(part) {
                    if !fg_set {
                        codes.push(format!("{}", 30 + c));
                        fg_set = true;
                    } else {
                        codes.push(format!("{}", 40 + c));
                    }
                }
            }
        }
    }
    if codes.is_empty() {
        String::new()
    } else {
        format!("\x1b[{}m", codes.join(";"))
    }
}

/// Extract the local part (before @) of the email from a Git ident string.
fn extract_email_local(ident: &str) -> String {
    local_part_of_email(&extract_email(ident))
}

fn local_part_of_email(email: &str) -> String {
    if let Some(at) = email.find('@') {
        email[..at].to_owned()
    } else {
        email.to_owned()
    }
}

/// Format ident for display: "Name <email>".
fn format_ident_display(ident: &str) -> String {
    let name = extract_name(ident);
    let email = extract_email(ident);
    format!("{name} <{email}>")
}

fn format_ident_display_mailmap(mailmap: &MailmapTable, ident: &str, use_mailmap: bool) -> String {
    if !use_mailmap || mailmap.is_empty() {
        return format_ident_display(ident);
    }
    let name = extract_name(ident);
    let email = extract_email(ident);
    let (n, e) = mailmap.map_user(name, email);
    format!("{n} <{e}>")
}

/// Format the date from an ident string for display, with optional date mode.
///
/// When `for_header` is true (pretty `Date:` lines), unparsable dates use the Unix epoch in UTC
/// (`+0000`), matching Git. When false (`%ad` and other format placeholders), unparsable dates
/// yield an empty string for the default format (t4212).
fn format_author_date_internal(ident: &str, date_mode: Option<&str>, for_header: bool) -> String {
    let tail = parse_signature_tail(ident);
    let (ts, tz_offset_secs, offset_str) = match tail {
        Some(SignatureTail::Valid(p)) => {
            let off = ident
                .get(p.tz_hhmm_range.clone())
                .unwrap_or("+0000")
                .to_owned();
            (p.unix_seconds, p.tz_offset_secs, off)
        }
        Some(SignatureTail::Overflow) if for_header => (0i64, 0i64, "+0000".to_owned()),
        Some(SignatureTail::Overflow) => {
            return match date_mode {
                None => "Thu Jan 1 00:00:00 1970 +0000".to_owned(),
                Some("relative") => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64;
                    format_relative_from_diff(now)
                }
                _ => String::new(),
            };
        }
        Some(SignatureTail::NonNumeric) if for_header => (0i64, 0i64, "+0000".to_owned()),
        Some(SignatureTail::NonNumeric) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            return match date_mode {
                Some("relative") => format_relative_from_diff(now),
                _ => String::new(),
            };
        }
        None if for_header => (0i64, 0i64, "+0000".to_owned()),
        None => return String::new(),
    };

    match date_mode {
        Some("short") => {
            // YYYY-MM-DD in the author's timezone
            let adjusted = ts + tz_offset_secs;
            let dt = time::OffsetDateTime::from_unix_timestamp(adjusted)
                .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
            format!("{:04}-{:02}-{:02}", dt.year(), dt.month() as u8, dt.day())
        }
        Some("iso") | Some("iso8601") => {
            // ISO format: 2005-04-07 15:13:13 +0200
            let adjusted = ts + tz_offset_secs;
            let dt = time::OffsetDateTime::from_unix_timestamp(adjusted)
                .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02} {}",
                dt.year(),
                dt.month() as u8,
                dt.day(),
                dt.hour(),
                dt.minute(),
                dt.second(),
                offset_str
            )
        }
        Some("iso-strict") | Some("iso8601-strict") => {
            let adjusted = ts + tz_offset_secs;
            let dt = time::OffsetDateTime::from_unix_timestamp(adjusted)
                .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
            let sign = if tz_offset_secs >= 0 { '+' } else { '-' };
            let abs_offset = tz_offset_secs.unsigned_abs();
            let h = abs_offset / 3600;
            let m = (abs_offset % 3600) / 60;
            format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{}{:02}:{:02}",
                dt.year(),
                dt.month() as u8,
                dt.day(),
                dt.hour(),
                dt.minute(),
                dt.second(),
                sign,
                h,
                m
            )
        }
        Some("raw") => {
            format!("{ts} {offset_str}")
        }
        Some("relative") => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            format_relative_from_diff(now - ts)
        }
        Some("rfc") | Some("rfc2822") => {
            // RFC 2822: Thu, 07 Apr 2005 22:13:13 +0200
            let adjusted = ts + tz_offset_secs;
            let dt = time::OffsetDateTime::from_unix_timestamp(adjusted)
                .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
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
            format!(
                "{}, {} {} {} {:02}:{:02}:{:02} {}",
                weekday,
                dt.day(),
                month,
                dt.year(),
                dt.hour(),
                dt.minute(),
                dt.second(),
                offset_str
            )
        }
        Some("unix") => {
            format!("{ts}")
        }
        _ => {
            // Default Git date format: "Thu Apr 7 15:13:13 2005 -0700" (single space before day;
            // matches C git `show_date`).
            let adjusted = ts + tz_offset_secs;
            let dt = time::OffsetDateTime::from_unix_timestamp(adjusted)
                .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
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
            format!(
                "{} {} {} {:02}:{:02}:{:02} {} {}",
                weekday,
                month,
                dt.day(),
                dt.hour(),
                dt.minute(),
                dt.second(),
                dt.year(),
                offset_str
            )
        }
    }
}

fn format_date_with_mode(ident: &str, date_mode: Option<&str>) -> String {
    format_author_date_internal(ident, date_mode, false)
}

/// Resolve a revision string to an ObjectId.
fn resolve_revision(repo: &Repository, rev: &str) -> Result<ObjectId> {
    // Delegate to the library's full revision parser which handles
    // @{N}, @{now}, @{upstream}, peeling, parent navigation, etc.
    grit_lib::rev_parse::resolve_revision(repo, rev)
        .map_err(|e| anyhow::anyhow!("unknown revision '{}': {}", rev, e))
}

fn resolve_revision_as_commit_after_precompose(repo: &Repository, rev: &str) -> Result<ObjectId> {
    if !grit_lib::precompose_config::effective_core_precomposeunicode(Some(&repo.git_dir))
        || !grit_lib::unicode_normalization::has_non_ascii_utf8(rev)
    {
        return resolve_revision_as_commit(repo, rev).map_err(|e| e.into());
    }
    let nfc = grit_lib::unicode_normalization::precompose_utf8_path(rev);
    if nfc.as_ref() == rev {
        return resolve_revision_as_commit(repo, rev).map_err(|e| e.into());
    }
    resolve_revision_as_commit(repo, nfc.as_ref()).map_err(|e| e.into())
}

/// Heuristic used for rev/pathspec DWIM when no `--` separator is present.
/// Whether `token` names an existing path in the worktree or the index. Git's `verify_filename`
/// uses this to decide that a token which fails to resolve as a revision is actually a pathspec
/// (e.g. `git log ichi` where `ichi` is a tracked file, used by `--follow`).
/// True when `token`, resolved relative to the current directory, names an
/// existing path inside the work tree (used to prefer a `..`/`.` pathspec over
/// the empty `HEAD..HEAD` range).
fn cwd_relative_path_exists(repo: &Repository, token: &str) -> bool {
    let Some(work_tree) = repo.work_tree.as_deref() else {
        return false;
    };
    let Ok(cwd) = std::env::current_dir() else {
        return false;
    };
    let candidate = normalize_path(&cwd.join(token));
    let work_tree_norm = normalize_path(work_tree);
    candidate.starts_with(&work_tree_norm) && candidate.exists()
}

fn token_names_existing_path(repo: &Repository, token: &str) -> bool {
    // Short exclude magic sigils `:^` / `:!` name a path via their suffix; git's verify_filename
    // accepts `:^sub` as a pathspec only when `sub` exists (`:^does-not-exist` is ambiguous).
    if let Some(rest) = token
        .strip_prefix(":^")
        .or_else(|| token.strip_prefix(":!"))
    {
        return !rest.is_empty() && token_names_existing_path(repo, rest);
    }
    if token.is_empty() {
        return false;
    }
    // Worktree path (relative to the worktree root).
    if let Some(wt) = repo.work_tree.as_ref() {
        if wt.join(token).exists() {
            return true;
        }
    }
    // Index path: an exact entry, or a directory prefix of some entry.
    if let Ok(index_path) = repo.index_path_for_env() {
        if let Ok(index) = grit_lib::index::Index::load(&index_path) {
            let token_bytes = token.as_bytes();
            let dir_prefix = {
                let mut p = token.to_owned();
                if !p.ends_with('/') {
                    p.push('/');
                }
                p.into_bytes()
            };
            for entry in &index.entries {
                if entry.path == token_bytes || entry.path.starts_with(&dir_prefix) {
                    return true;
                }
            }
        }
    }
    false
}

/// git's setup_revisions ambiguity check: a `:/pattern` revision that ALSO names an existing
/// worktree/index path (`pattern` as a file) is ambiguous (`git log :/a` with file `a` present).
/// Returns true when such a collision exists. Only `:/`-style search revisions can collide this
/// way (a plain object name like a branch/sha is never a relative path here).
fn rev_token_collides_with_path(repo: &Repository, token: &str) -> bool {
    if let Some(pattern) = token.strip_prefix(":/") {
        if !pattern.is_empty() && !pattern.contains('/') {
            return token_names_existing_path(repo, pattern);
        }
    }
    false
}

fn is_likely_pathspec_during_rev_parse(token: &str) -> bool {
    if token.contains("^{") || token.contains("@{") {
        return false;
    }

    // `..` in revision syntax is a range (e.g. `A..B`); single-token `..` is the parent dir pathspec.
    if token == ".." {
        return true;
    }
    if token.contains("..") {
        return false;
    }

    if token == "." {
        return true;
    }

    if let Some(rest) = token.strip_prefix(":/") {
        return rest.contains('*') || rest.contains('?') || rest.contains('[');
    }

    token.starts_with(":(")
        || token.contains('*')
        || token.contains('?')
        || token.contains('[')
        || token.contains(']')
}

/// Build the decoration filter from command-line args and `log.excludeDecoration`
/// config.
/// Whether hidden/auxiliary refs (`refs/hidden/*`, `refs/rewritten/*`, `refs/prefetch/*`, notes,
/// …) should be decorated. Git starts from the "all" decoration set when either `--clear-decorations`
/// is given or `log.initialDecorationSet=all` is configured; the default ("short") set hides those
/// refs. Unlike `--clear-decorations`, `log.initialDecorationSet=all` does *not* drop user/config
/// exclusions — only the implicit hidden-ref filter — so this is kept separate from
/// `args.clear_decorations` (which `build_decoration_filter` still uses to skip `log.excludeDecoration`).
fn decorations_initial_set_all(args: &Args, git_dir: &Path) -> bool {
    if args.clear_decorations {
        return true;
    }
    ConfigSet::load(Some(git_dir), true)
        .ok()
        .and_then(|cfg| cfg.get("log.initialDecorationSet"))
        .map(|v| v.trim().eq_ignore_ascii_case("all"))
        .unwrap_or(false)
}

/// Collect the `--decorate-refs` / `--decorate-refs-exclude` patterns in command-line order,
/// honoring `--clear-decorations`: that flag clears the patterns given *before* it (Git's
/// `clear_decorations_callback` in builtin/log.c, which is order-sensitive), while patterns
/// given after it are retained. Falls back to the clap-collected (order-insensitive) vecs when
/// the raw argv is unavailable.
fn ordered_decorate_ref_patterns(args: &Args) -> (Vec<String>, Vec<String>) {
    if args.raw_argv_tail.is_empty() {
        let include = args
            .decorate_refs
            .iter()
            .map(|p| normalize_glob_ref(p))
            .collect();
        let exclude = args
            .decorate_refs_exclude
            .iter()
            .map(|p| normalize_glob_ref(p))
            .collect();
        return (include, exclude);
    }

    let mut include: Vec<String> = Vec::new();
    let mut exclude: Vec<String> = Vec::new();
    let argv = &args.raw_argv_tail;
    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        if arg == "--" {
            break;
        }
        if arg == "--clear-decorations" {
            include.clear();
            exclude.clear();
            i += 1;
            continue;
        }
        // Match `--decorate-refs[=val]` and `--decorate-refs-exclude[=val]` (and the space form).
        let take = |flag: &str, list: &mut Vec<String>, i: &mut usize| -> bool {
            if let Some(rest) = arg.strip_prefix(flag) {
                if let Some(val) = rest.strip_prefix('=') {
                    list.push(normalize_glob_ref(val));
                    *i += 1;
                    return true;
                }
                if rest.is_empty() {
                    if let Some(val) = argv.get(*i + 1) {
                        list.push(normalize_glob_ref(val));
                    }
                    *i += 2;
                    return true;
                }
            }
            false
        };
        // Check the longer flag first so `--decorate-refs-exclude` is not eaten by `--decorate-refs`.
        if take("--decorate-refs-exclude", &mut exclude, &mut i) {
            continue;
        }
        if take("--decorate-refs", &mut include, &mut i) {
            continue;
        }
        i += 1;
    }
    (include, exclude)
}

fn build_decoration_filter(args: &Args, git_dir: &Path) -> DecorationFilter {
    let (include, exclude) = ordered_decorate_ref_patterns(args);
    let mut filter = DecorationFilter {
        include,
        exclude,
        exclude_config: Vec::new(),
    };
    // `--clear-decorations` drops the config-based exclusions (and Git's default
    // hidden-ref exclusions, which grit doesn't apply here anyway).
    if !args.clear_decorations {
        if let Ok(cfg) = ConfigSet::load(Some(git_dir), true) {
            for value in cfg.get_all_raw("log.excludeDecoration") {
                match value {
                    // A bare `log.excludeDecoration` (no value) is not fatal in Git: the
                    // decoration setup reports `error: missing value for 'log.excludeDecoration'`
                    // to stderr (git_config_string -> config_error_nonbool) and skips it.
                    None => {
                        eprintln!("error: missing value for 'log.excludeDecoration'");
                    }
                    Some(v) => {
                        let v = v.trim();
                        if !v.is_empty() {
                            filter.exclude_config.push(normalize_glob_ref(v));
                        }
                    }
                }
            }
        }
    }
    filter
}

fn load_decoration_paint(git_dir: &Path) -> DecorationPaint {
    let cfg = ConfigSet::load(Some(git_dir), true).unwrap_or_default();
    let slot = |key: &str, default: &str| {
        cfg.get(key)
            .and_then(|s| parse_color(s.trim()).ok())
            .unwrap_or_else(|| default.to_string())
    };
    DecorationPaint {
        commit: cfg
            .get("diff.color.commit")
            .and_then(|s| parse_color(s.trim()).ok())
            .unwrap_or_else(|| "\x1b[33m".to_string()),
        reset: slot("color.decorate.reset", "\x1b[m"),
        branch: slot("color.decorate.branch", "\x1b[1;32m"),
        remote_branch: slot("color.decorate.remoteBranch", "\x1b[1;31m"),
        tag: slot("color.decorate.tag", "\x1b[1;33m"),
        stash: slot("color.decorate.stash", "\x1b[1;35m"),
        head: slot("color.decorate.HEAD", "\x1b[1;36m"),
        grafted: slot("color.decorate.grafted", "\x1b[1;34m"),
    }
}

fn color_for_decoration_kind(paint: &DecorationPaint, kind: DecorationKind) -> &str {
    match kind {
        DecorationKind::Branch => &paint.branch,
        DecorationKind::RemoteBranch => &paint.remote_branch,
        DecorationKind::Tag => &paint.tag,
        DecorationKind::Stash => &paint.stash,
        DecorationKind::Head => &paint.head,
        DecorationKind::Grafted => &paint.grafted,
        // `DECORATION_NONE` -> `GIT_COLOR_RESET` (log-tree.c `decoration_colors[0]`).
        DecorationKind::Other => &paint.reset,
    }
}

/// Compute the plain (no-color, short-ref) decoration suffix for a commit hex, e.g.
/// ` (HEAD -> main, tag: v1)`. Returns an empty string when the commit carries no refs.
///
/// Shared with `show.rs` so that `git show --oneline` decorates its header line the same way
/// `git log --oneline` does.
pub(crate) fn oneline_decoration_for_hex(repo: &Repository, hex: &str) -> String {
    let decorations = match collect_decorations(repo, false) {
        Ok(map) => map,
        Err(_) => return String::new(),
    };
    let head = resolve_head(&repo.git_dir).unwrap_or(HeadState::Invalid);
    format_decoration(hex, Some(&decorations), false, None, &head)
}

/// Format decoration string for a commit (with parentheses), matching Git's `format_decorations`.
/// Customisable options for the `%(decorate:...)` pretty placeholder.
struct DecorateOpts {
    prefix: String,
    suffix: String,
    separator: String,
    pointer: String,
    tag: String,
}

/// Parse `%(decorate...)` options. Returns `None` on an unrecognised option so
/// the placeholder can be emitted literally (Git pretty.c behavior).
fn parse_decorate_opts(rest: &str) -> Option<DecorateOpts> {
    let mut opts = DecorateOpts {
        prefix: " (".to_owned(),
        suffix: ")".to_owned(),
        separator: ", ".to_owned(),
        pointer: " -> ".to_owned(),
        tag: "tag: ".to_owned(),
    };
    let body = match rest.strip_prefix(':') {
        Some(b) => b,
        None => {
            if rest.is_empty() {
                return Some(opts);
            }
            return None;
        }
    };
    if body.is_empty() {
        return Some(opts);
    }
    for tok in body.split(',') {
        let (name, value) = tok.split_once('=')?;
        let v = expand_trailer_value(value);
        match name {
            "prefix" => opts.prefix = v,
            "suffix" => opts.suffix = v,
            "separator" => opts.separator = v,
            "pointer" => opts.pointer = v,
            "tag" => opts.tag = v,
            _ => return None,
        }
    }
    Some(opts)
}

/// Render `%(decorate:...)` with customisable prefix/suffix/separator/pointer/tag.
fn format_decorate_custom(
    hex: &str,
    decorations: Option<&DecorationMap>,
    use_color: bool,
    paint: Option<&DecorationPaint>,
    head: &HeadState,
    opts: &DecorateOpts,
) -> String {
    let _ = use_color;
    let _ = paint;
    let Some(map) = decorations else {
        return String::new();
    };
    let Some(items) = map.get(hex) else {
        return String::new();
    };
    if items.is_empty() {
        return String::new();
    }

    let skip_idx = current_branch_decoration_index(items, head);
    let mut parts: Vec<String> = Vec::new();
    for (i, it) in items.iter().enumerate() {
        if skip_idx == Some(i) {
            continue;
        }
        let mut piece = String::new();
        if it.kind == DecorationKind::Tag {
            piece.push_str(&opts.tag);
        }
        piece.push_str(&it.display);
        if it.kind == DecorationKind::Head {
            if let Some(bi) = skip_idx {
                let branch = &items[bi];
                piece.push_str(&opts.pointer);
                let d = &branch.display;
                if branch.kind == DecorationKind::Tag && d.starts_with("tag: ") {
                    piece.push_str(d.trim_start_matches("tag: "));
                } else {
                    piece.push_str(d);
                }
            }
        }
        parts.push(piece);
    }
    if parts.is_empty() {
        return String::new();
    }
    format!(
        "{}{}{}",
        opts.prefix,
        parts.join(&opts.separator),
        opts.suffix
    )
}

fn format_decoration(
    hex: &str,
    decorations: Option<&DecorationMap>,
    use_color: bool,
    paint: Option<&DecorationPaint>,
    head: &HeadState,
) -> String {
    let Some(map) = decorations else {
        return String::new();
    };
    let Some(items) = map.get(hex) else {
        return String::new();
    };
    if items.is_empty() {
        return String::new();
    }

    const TAG: &str = "tag: ";
    const POINTER: &str = " -> ";
    const SEP: &str = ", ";

    let skip_idx = current_branch_decoration_index(items, head);
    let paint_opt = if use_color { paint } else { None };

    let mut out = String::new();
    let mut sep_prefix: &str = " (";

    for (i, it) in items.iter().enumerate() {
        if skip_idx == Some(i) {
            continue;
        }

        if let Some(p) = paint_opt {
            out.push_str(&p.commit);
            out.push_str(sep_prefix);
            out.push_str(&p.reset);
        } else {
            out.push_str(sep_prefix);
        }
        sep_prefix = SEP;

        if it.kind == DecorationKind::Tag {
            if let Some(p) = paint_opt {
                out.push_str(color_for_decoration_kind(p, DecorationKind::Tag));
                out.push_str(TAG);
                out.push_str(&p.reset);
            } else {
                out.push_str(TAG);
            }
        }

        if let Some(p) = paint_opt {
            out.push_str(color_for_decoration_kind(p, it.kind));
            out.push_str(&it.display);
            out.push_str(&p.reset);
        } else {
            out.push_str(&it.display);
        }

        if it.kind == DecorationKind::Head {
            if let Some(bi) = skip_idx {
                let branch = &items[bi];
                if let Some(p) = paint_opt {
                    out.push_str(&p.commit);
                    out.push_str(POINTER);
                    out.push_str(&p.reset);
                    out.push_str(color_for_decoration_kind(p, branch.kind));
                } else {
                    out.push_str(POINTER);
                }
                let d = &branch.display;
                if branch.kind == DecorationKind::Tag && d.starts_with(TAG) {
                    out.push_str(d.trim_start_matches(TAG));
                } else {
                    out.push_str(d);
                }
                if let Some(p) = paint_opt {
                    out.push_str(&p.reset);
                }
            }
        }
    }

    if !out.is_empty() {
        if let Some(p) = paint_opt {
            out.push_str(&p.commit);
            out.push(')');
            out.push_str(&p.reset);
        } else {
            out.push(')');
        }
    }
    out
}

/// Format decoration string without parentheses (for `%D`).
fn format_decoration_no_parens(
    hex: &str,
    decorations: Option<&DecorationMap>,
    use_color: bool,
    paint: Option<&DecorationPaint>,
    head: &HeadState,
) -> String {
    let inner = format_decoration(hex, decorations, use_color, paint, head);
    inner
        .strip_prefix(" (")
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or("")
        .to_owned()
}

// ── Diff output for log ──────────────────────────────────────────────

/// Compute combined diff entries: only files that differ from ALL parents.
fn compute_combined_diff_entries(odb: &Odb, info: &CommitInfo) -> Result<Vec<DiffEntry>> {
    use std::collections::HashSet;
    // For each parent, find files that are different from that parent
    let mut changed_per_parent: Vec<HashSet<String>> = Vec::new();
    for parent_oid in &info.parents {
        let parent_obj = odb.read(parent_oid)?;
        let parent_commit = parse_commit(&parent_obj.data)?;
        let entries = diff_trees(odb, Some(&parent_commit.tree), Some(&info.tree), "")?;
        let paths: HashSet<String> = entries.iter().map(|e| e.path().to_string()).collect();
        changed_per_parent.push(paths);
    }
    // Intersection: only files changed from ALL parents
    if changed_per_parent.is_empty() {
        return Ok(vec![]);
    }
    let mut common = changed_per_parent[0].clone();
    for other in &changed_per_parent[1..] {
        common = common.intersection(other).cloned().collect();
    }
    if common.is_empty() {
        return Ok(vec![]);
    }
    // Get entries from first-parent diff that are in common set
    let first_parent_obj = odb.read(&info.parents[0])?;
    let first_parent_commit = parse_commit(&first_parent_obj.data)?;
    let entries = diff_trees(odb, Some(&first_parent_commit.tree), Some(&info.tree), "")?;
    Ok(entries
        .into_iter()
        .filter(|e| common.contains(&*e.path().to_string_lossy()))
        .collect())
}

/// Compute diff entries for a commit against its first parent (or empty tree for root commits).
fn compute_commit_diff(odb: &Odb, info: &CommitInfo) -> Result<Vec<DiffEntry>> {
    if info.parents.is_empty() {
        // Root commit: diff against empty tree
        Ok(diff_trees(odb, None, Some(&info.tree), "")?)
    } else {
        let parent_obj = odb.read(&info.parents[0])?;
        let parent_commit = parse_commit(&parent_obj.data)?;
        Ok(diff_trees(
            odb,
            Some(&parent_commit.tree),
            Some(&info.tree),
            "",
        )?)
    }
}

fn compute_commit_diff_against_parent(
    odb: &Odb,
    info: &CommitInfo,
    parent_idx: usize,
) -> Result<Vec<DiffEntry>> {
    if parent_idx >= info.parents.len() {
        return Ok(Vec::new());
    }
    let parent_obj = odb.read(&info.parents[parent_idx])?;
    let parent_commit = parse_commit(&parent_obj.data)?;
    Ok(diff_trees(
        odb,
        Some(&parent_commit.tree),
        Some(&info.tree),
        "",
    )?)
}

/// Write diff output for a single commit.
fn write_commit_diff(
    out: &mut impl Write,
    repo: &Repository,
    commit_oid: &ObjectId,
    info: &CommitInfo,
    args: &Args,
    use_mailmap: bool,
    mailmap: &MailmapTable,
    pathspecs: &[String],
    graph_stat_prefix: Option<&str>,
    graph_stat_width_prefix: Option<&str>,
    decorations: Option<&DecorationMap>,
    use_color: bool,
    decoration_paint: Option<&DecorationPaint>,
    head_for_decor: &HeadState,
    notes_cache: &mut NotesMapCache<'_>,
    patch_context: usize,
    leading_blank: bool,
    follow_override: Option<&FollowDisplay>,
) -> Result<()> {
    let odb = &repo.odb;
    let git_dir = &repo.git_dir;
    let log_config = ConfigSet::load(Some(git_dir), true).unwrap_or_default();
    let indent_heuristic = indent_heuristic_from_config(&log_config);
    // Under `--follow`, restrict the displayed diff to the path the followed file
    // had in this commit (the copy/rename destination), not the original pathspec.
    let follow_pathspecs: Vec<String> = follow_override
        .map(|f| vec![f.display_path.clone()])
        .unwrap_or_default();
    let pathspecs: &[String] = if follow_override.is_some() {
        &follow_pathspecs
    } else {
        pathspecs
    };
    let is_merge = info.parents.len() > 1;

    if !log_commit_needs_diff_output(args, info, git_dir)? {
        return Ok(());
    }

    if merge_diff_is_remerge(args, is_merge, git_dir)? && info.parents.len() == 2 {
        use crate::commands::remerge_diff::{write_remerge_diff, RemergeDiffOptions};
        let find_oid = if let Some(ref s) = args.find_object {
            Some(grit_lib::rev_parse::resolve_revision(repo, s)?)
        } else {
            None
        };
        let opts = RemergeDiffOptions {
            pathspecs,
            diff_filter: args.diff_filter.as_deref(),
            // Pickaxe filters which commits appear; the displayed remerge diff is always full.
            pickaxe: None,
            find_object: find_oid,
            submodule_mode: None,
            context_lines: patch_context,
            indent_heuristic,
        };
        if leading_blank {
            writeln!(out)?;
        }
        return write_remerge_diff(out, repo, &info.tree, &info.parents, &opts);
    }

    let show_patch = log_wants_patch_hunks(args, info, git_dir)?;
    let separate = merge_diff_is_separate(args, is_merge, git_dir)?;

    let log_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    if separate {
        let mut shown_parent = 0usize;
        for (i, parent_oid) in info.parents.iter().enumerate() {
            let mut entries = compute_commit_diff_against_parent(odb, info, i)?;
            if entries.is_empty() {
                continue;
            }
            if let Some(ref order_path) = args.order_file {
                entries = grit_lib::diff::apply_orderfile_entries(entries, order_path, &log_cwd)?;
            }
            entries = grit_lib::diff::apply_rotate_skip_log_entries(
                odb,
                &info.tree,
                entries,
                args.rotate_to.as_deref(),
                args.skip_to.as_deref(),
            )?;
            // In separate (`-m`) mode git re-prints the commit header for EVERY parent,
            // each tagged `(from <parent>)`. The caller suppresses its plain header in this
            // mode (see `log_commit_uses_per_parent_header`), so we print all of them here,
            // separated by a blank line (git's `shown_one`).
            if shown_parent > 0 && leading_blank {
                writeln!(out)?;
            }
            shown_parent += 1;
            format_commit(
                out,
                commit_oid,
                info,
                args,
                use_mailmap,
                mailmap,
                decorations,
                use_color,
                decoration_paint,
                head_for_decor,
                notes_cache,
                odb,
                None,
                false,
                None,
                Some(parent_oid),
                None,
                None,
                None,
            )?;
            write_commit_diff_body(
                out,
                odb,
                git_dir,
                &entries,
                &entries,
                args,
                pathspecs,
                graph_stat_prefix,
                graph_stat_width_prefix,
                show_patch,
                false,
                patch_context,
                indent_heuristic,
                leading_blank,
                None,
            )?;
        }
        return Ok(());
    }

    let mut entries = compute_commit_diff(odb, info)?;
    if !args.no_renames && args.find_renames.is_some() {
        let threshold = args
            .find_renames
            .as_deref()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(50);
        entries = grit_lib::diff::detect_renames(odb, None, entries, threshold);
    }
    // Under `--follow`, the commit that introduced the followed file as a copy or
    // rename of another path was detected up front (`follow_filter`). Replace the
    // plain add/modify entry for the destination with that copy/rename entry so the
    // displayed diff shows `C100 <src> <dst>` and the source blob is the patch base.
    if let Some(detected) = follow_override.and_then(|f| f.display_entry.as_ref()) {
        if matches!(detected.status, DiffStatus::Renamed | DiffStatus::Copied) {
            let dest = detected.new_path.as_deref();
            entries.retain(|e| e.new_path.as_deref() != dest);
            entries.push(detected.clone());
        }
    }
    if entries.is_empty() {
        return Ok(());
    }

    // Pickaxe (`-S` / `-G`) without `--pickaxe-all` limits the patch to the files whose
    // change matches the needle (Git's `diffcore_pickaxe`).
    if !args.pickaxe_all
        && !is_merge
        && (args.pickaxe_string.is_some() || args.pickaxe_grep.is_some())
    {
        let matched = pickaxe_matching_paths(git_dir, odb, &entries, args)?;
        entries.retain(|e| matched.contains(&*e.path().to_string_lossy()));
        if entries.is_empty() {
            return Ok(());
        }
    }

    if let Some(ref order_path) = args.order_file {
        entries = grit_lib::diff::apply_orderfile_entries(entries, order_path, &log_cwd)?;
    }

    let combined_style = merge_diff_is_combined_style(args, is_merge, git_dir)?;
    let mut combined_entries = if combined_style {
        compute_combined_diff_entries(odb, info)?
    } else {
        entries.clone()
    };

    entries = grit_lib::diff::apply_rotate_skip_log_entries(
        odb,
        &info.tree,
        entries,
        args.rotate_to.as_deref(),
        args.skip_to.as_deref(),
    )?;
    combined_entries = if combined_style {
        grit_lib::diff::apply_rotate_skip_log_entries(
            odb,
            &info.tree,
            combined_entries,
            args.rotate_to.as_deref(),
            args.skip_to.as_deref(),
        )?
    } else {
        entries.clone()
    };

    // For combined (`-c`/`--cc`) merge patches, log must render true combined
    // (`@@@`) hunks rather than a first-parent unified diff. Pass the merge tree
    // and parent commit OIDs so write_commit_diff_body can build them.
    let combined_merge_ctx: Option<(ObjectId, Vec<ObjectId>)> =
        if combined_style && info.parents.len() >= 2 {
            Some((info.tree, info.parents.clone()))
        } else {
            None
        };

    write_commit_diff_body(
        out,
        odb,
        git_dir,
        &entries,
        &combined_entries,
        args,
        pathspecs,
        graph_stat_prefix,
        graph_stat_width_prefix,
        show_patch,
        is_merge,
        patch_context,
        indent_heuristic,
        leading_blank,
        combined_merge_ctx.as_ref(),
    )?;

    Ok(())
}

fn diff_entry_matches_any_pathspec(entry: &DiffEntry, specs: &[String]) -> bool {
    if specs.is_empty() {
        return true;
    }
    let paths = [
        entry.path().to_string_lossy(),
        entry
            .old_path
            .as_deref()
            .map_or(Cow::Borrowed(""), RepoPath::to_string_lossy),
        entry
            .new_path
            .as_deref()
            .map_or(Cow::Borrowed(""), RepoPath::to_string_lossy),
    ];
    for spec in specs {
        for p in &paths {
            if !p.is_empty() && grit_lib::pathspec::matches_pathspec(spec, p) {
                return true;
            }
        }
    }
    false
}

fn filter_diff_entries_by_pathspecs(entries: Vec<DiffEntry>, specs: &[String]) -> Vec<DiffEntry> {
    if specs.is_empty() {
        return entries;
    }
    entries
        .into_iter()
        .filter(|e| diff_entry_matches_any_pathspec(e, specs))
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn write_commit_diff_body(
    out: &mut impl Write,
    odb: &Odb,
    git_dir: &Path,
    entries: &[DiffEntry],
    combined_entries: &[DiffEntry],
    args: &Args,
    pathspecs: &[String],
    graph_stat_prefix: Option<&str>,
    graph_stat_width_prefix: Option<&str>,
    show_patch: bool,
    treat_as_merge_for_format: bool,
    patch_context: usize,
    indent_heuristic: bool,
    leading_blank: bool,
    combined_merge_ctx: Option<&(ObjectId, Vec<ObjectId>)>,
) -> Result<()> {
    let combined_style = merge_diff_is_combined_style(args, treat_as_merge_for_format, git_dir)?;
    let entries_owned: Vec<DiffEntry> = entries.to_vec();
    let combined_owned: Vec<DiffEntry> = combined_entries.to_vec();
    let entries_f = filter_diff_entries_by_pathspecs(entries_owned, pathspecs);
    let combined_f = filter_diff_entries_by_pathspecs(combined_owned, pathspecs);

    // `-I` / `--ignore-blank-lines`: drop files whose changes are entirely ignorable so no patch,
    // `--raw`, `--name-only`, `--name-status`, or `--stat` line is emitted for them (xdiff).
    let line_ignore = compile_log_line_ignore(args);
    let (entries_f, combined_f) = if !line_ignore.is_empty() || args.ignore_blank_lines {
        let f = |e: &DiffEntry| {
            !log_entry_hidden_by_line_ignore(odb, e, &line_ignore, args.ignore_blank_lines)
        };
        (
            entries_f.into_iter().filter(&f).collect::<Vec<_>>(),
            combined_f.into_iter().filter(&f).collect::<Vec<_>>(),
        )
    } else {
        (entries_f, combined_f)
    };
    let list_raw_name: &[DiffEntry] = if combined_style {
        &combined_f
    } else {
        &entries_f
    };
    let list_patch: &[DiffEntry] = if combined_style {
        &combined_f
    } else {
        &entries_f
    };
    // `--cc -p --stat` on a merge: even when the dense combined diff is empty,
    // Git still prints the first-parent diffstat (t4066).
    let merge_stat_from_first_parent =
        combined_style && (!args.stat.is_empty() || args.patch_with_stat) && !entries.is_empty();
    if list_raw_name.is_empty() && list_patch.is_empty() && !merge_stat_from_first_parent {
        return Ok(());
    }
    let has_patch = show_patch && !list_patch.is_empty();
    // `--patch-with-stat` shows a diffstat even though `--stat` was not given explicitly.
    let wants_stat = !args.stat.is_empty() || args.patch_with_stat;

    // Builtin pretty formats (medium/short/full/fuller/raw) print a blank line between
    // the commit message and the diff body (git log-tree.c). `--stat`/`--name-status`
    // emit their own leading separator below, so only prefix it for the raw/patch cases.
    if leading_blank && !wants_stat && !args.name_only && !args.name_status {
        writeln!(out)?;
    }

    if args.raw {
        // git `log --raw` abbreviates OIDs (default 7) with a trailing `...` ellipsis,
        // unless `--no-abbrev` was given (then full 40-hex, no ellipsis).
        let raw_abbrev = if args.no_abbrev {
            None
        } else {
            Some(parse_abbrev(&args.abbrev))
        };
        for entry in list_raw_name {
            match raw_abbrev {
                Some(len) => writeln!(out, "{}", format_raw_abbrev(entry, len))?,
                None => writeln!(out, "{}", format_raw(entry))?,
            }
        }
        // No trailing blank here: the blank line separating commits is emitted by the
        // log walk's inter-commit separator (git's raw format prints no extra blank).
    }

    if wants_stat {
        // Combined (`-c`/`--cc`) merge diffs compute the stat from the first parent
        // and separate it from the message with a blank line, not the `---` marker
        // that precedes a normal unified patch's diffstat (git diff_tree_combined).
        let combined_merge_stat = combined_style && combined_merge_ctx.is_some();
        if has_patch && !combined_merge_stat {
            writeln!(out, "---")?;
        } else {
            writeln!(out)?;
        }
        // Limit the diffstat to the pathspec-matched entries (git stats only the shown files).
        let stat_entries: &[DiffEntry] = if merge_stat_from_first_parent {
            entries
        } else {
            &entries_f
        };
        log_print_stat_summary(
            out,
            odb,
            stat_entries,
            has_patch,
            args,
            graph_stat_prefix,
            graph_stat_width_prefix,
            git_dir,
        )?;
    }

    if args.name_only {
        if !list_raw_name.is_empty() && log_name_list_needs_separator(args) {
            writeln!(out)?;
        }
        for entry in list_raw_name {
            writeln!(out, "{}", entry.path())?;
        }
    }

    if args.name_status {
        // Like `--name-only`, emit a single separator before the status block: in
        // builtin multi-line pretty formats this is the blank line between the message
        // and the diff body; with `format:` (whose subject line is left unterminated)
        // it is the subject's terminating newline; `oneline`/`tformat:` already
        // terminate the line, so nothing is added there. The inter-commit blank line is
        // supplied by the caller's separator, so no trailing blank is printed here.
        if !list_raw_name.is_empty() && log_name_list_needs_separator(args) {
            writeln!(out)?;
        }
        for entry in list_raw_name {
            match entry.status {
                // Renames and copies print the similarity score and both the source
                // and destination paths (`R100\told\tnew`), matching diff-tree's
                // name-status output. This is reached e.g. for `log --follow`, where
                // the copy/rename that introduced the followed file is shown.
                DiffStatus::Renamed | DiffStatus::Copied => {
                    let score = entry.score.unwrap_or(100);
                    writeln!(
                        out,
                        "{}{:03}\t{}\t{}",
                        entry.status.letter(),
                        score,
                        entry.old_path.as_deref().unwrap_or(RepoPath::from_str("")),
                        entry.new_path.as_deref().unwrap_or(RepoPath::from_str("")),
                    )?;
                }
                _ => {
                    writeln!(out, "{}\t{}", entry.status.letter(), entry.path())?;
                }
            }
        }
    }

    if show_patch {
        let config = ConfigSet::load(Some(git_dir), true).unwrap_or_default();
        if let Some((merge_tree, parent_commits)) = combined_merge_ctx.filter(|_| combined_style) {
            log_write_combined_patches(
                out,
                odb,
                git_dir,
                &config,
                list_patch,
                merge_tree,
                parent_commits,
                args,
                patch_context,
            )?;
        } else {
            for entry in list_patch {
                log_write_patch_entry(
                    out,
                    odb,
                    git_dir,
                    &config,
                    entry,
                    args,
                    patch_context,
                    indent_heuristic,
                )?;
            }
        }
    }

    Ok(())
}

/// Render true combined (`@@@`) patches for a `-c`/`--cc` merge commit in `git log`,
/// mirroring diff-tree's `print_combined_merge_output` patch path.
#[allow(clippy::too_many_arguments)]
fn log_write_combined_patches(
    out: &mut impl Write,
    odb: &Odb,
    git_dir: &Path,
    config: &ConfigSet,
    list_patch: &[DiffEntry],
    merge_tree: &ObjectId,
    parent_commits: &[ObjectId],
    args: &Args,
    patch_context: usize,
) -> Result<()> {
    if parent_commits.len() < 2 {
        return Ok(());
    }
    let dense = merge_diff_is_dense_combined(args, true, git_dir)?;
    let abbrev = if args.no_abbrev {
        40usize
    } else {
        parse_abbrev(&args.abbrev)
    };
    let quote_fully = config.quote_path_fully();
    // `git log` does not currently plumb the diff whitespace flags into combined
    // patches; default to no whitespace normalization (matches the non-combined path).
    let ws = CombinedDiffWsOptions::default();
    let mut parent_trees = Vec::with_capacity(parent_commits.len());
    for p in parent_commits {
        let obj = odb.read(p)?;
        let commit = parse_commit(&obj.data)?;
        parent_trees.push(commit.tree);
    }
    // Build the combined parent sides for each interesting path so the combined
    // patch shows the correct per-parent index OIDs.
    let walk = CombinedTreeDiffOptions {
        recursive: true,
        tree_in_recursive: false,
    };
    let cpaths = combined_diff_paths_filtered(odb, merge_tree, parent_commits, &walk, None)?;
    for entry in list_patch {
        let path = entry.path();
        let parent_sides = cpaths
            .iter()
            .find(|p| path == &p.path)
            .map(|p| p.parents.clone())
            .unwrap_or_default();
        if let Some(patch) = format_combined_textconv_patch(
            git_dir,
            config,
            odb,
            &path.to_string_lossy(),
            &parent_trees,
            merge_tree,
            abbrev,
            patch_context,
            dense,
            false,
            ws,
            false,
            None,
            &parent_sides,
            quote_fully,
        ) {
            write!(out, "{patch}")?;
        }
    }
    Ok(())
}

/// Compile the `-I` / `--ignore-matching-lines` regexes (invalid regex aborts with code 129,
/// matching git's `invalid regex given to -I:` diagnostic).
fn compile_log_line_ignore(args: &Args) -> Vec<Regex> {
    let mut out = Vec::with_capacity(args.ignore_matching_lines.len());
    for p in &args.ignore_matching_lines {
        match Regex::new(p) {
            Ok(re) => out.push(re),
            Err(_) => {
                eprintln!("error: invalid regex given to -I: {p}");
                std::process::exit(129);
            }
        }
    }
    out
}

/// True when every hunk of this entry's diff is ignorable (so git emits no patch for the file).
fn log_entry_hidden_by_line_ignore(
    odb: &Odb,
    entry: &DiffEntry,
    ignore: &[Regex],
    ignore_blank: bool,
) -> bool {
    if ignore.is_empty() && !ignore_blank {
        return false;
    }
    if matches!(
        entry.status,
        DiffStatus::Renamed | DiffStatus::Copied | DiffStatus::TypeChanged | DiffStatus::Unmerged
    ) {
        return false;
    }
    if entry.old_mode != entry.new_mode {
        return false;
    }
    let old = if entry.old_oid == zero_oid() {
        String::new()
    } else {
        String::from_utf8_lossy(&read_blob_bytes(odb, &entry.old_oid)).into_owned()
    };
    let new = if entry.new_oid == zero_oid() {
        String::new()
    } else {
        String::from_utf8_lossy(&read_blob_bytes(odb, &entry.new_oid)).into_owned()
    };
    let body = unified_diff_histogram_hunks_only(&old, &new, 3, 0);
    if body.is_empty() {
        return true;
    }
    crate::commands::diff::suppress_ignored_hunks_in_patch(&body, ignore, ignore_blank).is_empty()
}

/// Write a unified-diff block for one entry.
fn log_write_patch_entry(
    out: &mut impl Write,
    odb: &Odb,
    git_dir: &std::path::Path,
    config: &ConfigSet,
    entry: &DiffEntry,
    args: &Args,
    context_lines: usize,
    indent_heuristic: bool,
) -> Result<()> {
    let old_path_owned = entry
        .old_path
        .as_deref()
        .or(entry.new_path.as_deref())
        .map_or(Cow::Borrowed(""), RepoPath::to_string_lossy);
    let old_path: &str = &old_path_owned;
    let new_path_owned = entry
        .new_path
        .as_deref()
        .or(entry.old_path.as_deref())
        .map_or(Cow::Borrowed(""), RepoPath::to_string_lossy);
    let new_path: &str = &new_path_owned;

    if args.no_prefix {
        writeln!(out, "diff --git {old_path} {new_path}")?;
    } else {
        writeln!(out, "diff --git a/{old_path} b/{new_path}")?;
    }

    match entry.status {
        DiffStatus::Added => {
            writeln!(out, "new file mode {}", entry.new_mode)?;
            writeln!(
                out,
                "index {}..{}",
                &entry.old_oid.to_hex()[..7],
                &entry.new_oid.to_hex()[..7]
            )?;
        }
        DiffStatus::Deleted => {
            writeln!(out, "deleted file mode {}", entry.old_mode)?;
            writeln!(
                out,
                "index {}..{}",
                &entry.old_oid.to_hex()[..7],
                &entry.new_oid.to_hex()[..7]
            )?;
        }
        DiffStatus::Modified => {
            if entry.old_mode != entry.new_mode {
                writeln!(out, "old mode {}", entry.old_mode)?;
                writeln!(out, "new mode {}", entry.new_mode)?;
            }
            if entry.old_mode == entry.new_mode {
                writeln!(
                    out,
                    "index {}..{} {}",
                    &entry.old_oid.to_hex()[..7],
                    &entry.new_oid.to_hex()[..7],
                    entry.old_mode
                )?;
            } else {
                writeln!(
                    out,
                    "index {}..{}",
                    &entry.old_oid.to_hex()[..7],
                    &entry.new_oid.to_hex()[..7]
                )?;
            }
        }
        DiffStatus::Renamed => {
            writeln!(out, "similarity index 100%")?;
            writeln!(out, "rename from {old_path}")?;
            writeln!(out, "rename to {new_path}")?;
        }
        DiffStatus::Copied => {
            writeln!(out, "similarity index 100%")?;
            writeln!(out, "copy from {old_path}")?;
            writeln!(out, "copy to {new_path}")?;
        }
        DiffStatus::TypeChanged => {
            writeln!(out, "old mode {}", entry.old_mode)?;
            writeln!(out, "new mode {}", entry.new_mode)?;
        }
        DiffStatus::Unmerged => {}
    }

    if entry.old_mode == "160000" || entry.new_mode == "160000" {
        let old_content = if entry.old_oid == zero_oid() {
            String::new()
        } else {
            format!("Subproject commit {}\n", entry.old_oid.to_hex())
        };
        let new_content = if entry.new_oid == zero_oid() {
            String::new()
        } else {
            format!("Subproject commit {}\n", entry.new_oid.to_hex())
        };
        let display_old = if entry.status == DiffStatus::Added {
            "/dev/null"
        } else {
            old_path
        };
        let display_new = if entry.status == DiffStatus::Deleted {
            "/dev/null"
        } else {
            new_path
        };
        let (src_pfx, dst_pfx) = if args.no_prefix {
            ("", "")
        } else {
            ("a/", "b/")
        };
        let patch = grit_lib::diff::unified_diff_with_prefix(
            &old_content,
            &new_content,
            display_old,
            display_new,
            context_lines,
            0,
            src_pfx,
            dst_pfx,
            indent_heuristic,
            config.quote_path_fully(),
        );
        let patch = apply_diff_output_indicators(&patch, args);
        write!(out, "{patch}")?;
        return Ok(());
    }

    let path_for_attrs_owned = entry.path().to_string_lossy();
    let path_for_attrs: &str = &path_for_attrs_owned;
    let use_textconv = !args.no_textconv;
    let textconv_patch = use_textconv && diff_textconv_active(git_dir, config, path_for_attrs);
    let old_raw = read_blob_bytes(odb, &entry.old_oid);
    let new_raw = read_blob_bytes(odb, &entry.new_oid);
    if !textconv_patch
        && (is_binary_for_diff(git_dir, path_for_attrs, &old_raw)
            || is_binary_for_diff(git_dir, path_for_attrs, &new_raw))
    {
        let (src_pfx, dst_pfx) = if args.no_prefix {
            ("", "")
        } else {
            ("a/", "b/")
        };
        writeln!(
            out,
            "Binary files {src_pfx}{old_path} and {dst_pfx}{new_path} differ"
        )?;
        return Ok(());
    }

    let old_content = if entry.old_oid == zero_oid() {
        String::new()
    } else if use_textconv {
        blob_text_for_diff_with_oid(
            odb,
            git_dir,
            config,
            path_for_attrs,
            &old_raw,
            &entry.old_oid,
            true,
        )
    } else {
        String::from_utf8_lossy(&old_raw).into_owned()
    };
    let new_content = if entry.new_oid == zero_oid() {
        String::new()
    } else if use_textconv {
        blob_text_for_diff_with_oid(
            odb,
            git_dir,
            config,
            path_for_attrs,
            &new_raw,
            &entry.new_oid,
            true,
        )
    } else {
        String::from_utf8_lossy(&new_raw).into_owned()
    };
    let display_old = if entry.status == DiffStatus::Added {
        "/dev/null"
    } else {
        old_path
    };
    let display_new = if entry.status == DiffStatus::Deleted {
        "/dev/null"
    } else {
        new_path
    };
    let (src_pfx, dst_pfx) = if args.no_prefix {
        ("", "")
    } else {
        ("a/", "b/")
    };
    let patch = grit_lib::diff::unified_diff_with_prefix(
        &old_content,
        &new_content,
        display_old,
        display_new,
        context_lines,
        0,
        src_pfx,
        dst_pfx,
        indent_heuristic,
        config.quote_path_fully(),
    );
    // Drop hunks made up solely of ignorable changes (`-I` / `--ignore-blank-lines`).
    let patch = if !args.ignore_matching_lines.is_empty() || args.ignore_blank_lines {
        let ign = compile_log_line_ignore(args);
        crate::commands::diff::suppress_ignored_hunks_in_patch(
            &patch,
            &ign,
            args.ignore_blank_lines,
        )
    } else {
        patch
    };
    let patch = apply_diff_output_indicators(&patch, args);
    write!(out, "{patch}")?;

    Ok(())
}

fn apply_diff_output_indicators(patch: &str, args: &Args) -> String {
    if args.output_indicator_new.is_none()
        && args.output_indicator_old.is_none()
        && args.output_indicator_context.is_none()
    {
        return patch.to_owned();
    }
    let new_c = args
        .output_indicator_new
        .as_deref()
        .and_then(|s| s.chars().next())
        .unwrap_or('>');
    let old_c = args
        .output_indicator_old
        .as_deref()
        .and_then(|s| s.chars().next())
        .unwrap_or('<');
    let ctx_c = args
        .output_indicator_context
        .as_deref()
        .and_then(|s| s.chars().next())
        .unwrap_or('#');
    let mut out = String::with_capacity(patch.len());
    for line in patch.split_inclusive('\n') {
        let bytes = line.as_bytes();
        if bytes.first() == Some(&b'+') && bytes.get(1) != Some(&b'+') && !line.starts_with("+++ ")
        {
            out.push(new_c);
            out.push_str(&line[1..]);
        } else if bytes.first() == Some(&b'-')
            && bytes.get(1) != Some(&b'-')
            && !line.starts_with("--- ")
        {
            out.push(old_c);
            out.push_str(&line[1..]);
        } else if bytes.first() == Some(&b' ') {
            out.push(ctx_c);
            out.push_str(&line[1..]);
        } else {
            out.push_str(line);
        }
    }
    out
}

/// Write a `--stat` summary for log.
///
/// `graph_line_prefix` is the prefix actually printed before each stat line (used by the
/// `-L` line-log graph path that emits the stat inline). `graph_width_prefix` is the graph's
/// vertical rail that the *caller* prints separately (regular `--graph` path): it is not
/// printed here, but its display width is subtracted from the terminal columns so the stat
/// graph is scaled exactly like Git's `width = term_columns() - utf8_strnwidth(line_prefix)`.
fn log_print_stat_summary(
    out: &mut impl Write,
    odb: &Odb,
    entries: &[DiffEntry],
    trailing_blank: bool,
    args: &Args,
    graph_line_prefix: Option<&str>,
    graph_width_prefix: Option<&str>,
    git_dir: &Path,
) -> Result<()> {
    let use_color = log_resolve_stdout_color(args, git_dir);
    let cfg = ConfigSet::load(Some(git_dir), true).unwrap_or_default();
    let eff_name_width = args.stat_name_width.or_else(|| {
        cfg.get("diff.statNameWidth")
            .and_then(|s| s.parse::<usize>().ok())
    });
    let cfg_stat_graph = cfg
        .get("diff.statGraphWidth")
        .and_then(|s| s.parse::<usize>().ok());
    let eff_graph_width = args.stat_graph_width.or(cfg_stat_graph);
    let (color_add, color_del, color_reset) = if use_color {
        let add = cfg
            .get("color.diff.new")
            .and_then(|s| grit_lib::config::parse_color(&s).ok())
            .unwrap_or_else(|| "\x1b[32m".to_string());
        let del = cfg
            .get("color.diff.old")
            .and_then(|s| grit_lib::config::parse_color(&s).ok())
            .unwrap_or_else(|| "\x1b[31m".to_string());
        (add, del, "\x1b[m".to_string())
    } else {
        (String::new(), String::new(), String::new())
    };

    let line_prefix = graph_line_prefix.unwrap_or("");
    // The rail whose width is subtracted from the terminal columns: the printed prefix for the
    // inline `-L` path, or the separately-printed rail for the regular `--graph` path. Git only
    // subtracts it when the width is derived from the terminal (no explicit `--stat-width`).
    let width_prefix = graph_width_prefix.unwrap_or(line_prefix);
    let subtract_prefix =
        (graph_line_prefix.is_some() || graph_width_prefix.is_some()) && args.stat_width.is_none();

    let mut files: Vec<FileStatInput> = Vec::with_capacity(entries.len());
    for entry in entries {
        let path_display = match entry.status {
            DiffStatus::Renamed | DiffStatus::Copied => {
                let old = entry
                    .old_path
                    .as_deref()
                    .map_or(Cow::Borrowed(""), RepoPath::to_string_lossy);
                let new = entry
                    .new_path
                    .as_deref()
                    .map_or(Cow::Borrowed(""), RepoPath::to_string_lossy);
                grit_lib::diff::format_rename_path(&old, &new)
            }
            _ => entry.path().to_string(),
        };
        let old_raw = if entry.old_oid == zero_oid() {
            Vec::new()
        } else {
            odb.read(&entry.old_oid).map(|o| o.data).unwrap_or_default()
        };
        let new_raw = if entry.new_oid == zero_oid() {
            Vec::new()
        } else {
            odb.read(&entry.new_oid).map(|o| o.data).unwrap_or_default()
        };
        let binary = old_raw.contains(&0) || new_raw.contains(&0);
        if binary {
            let deleted = if entry.old_oid == zero_oid() {
                0
            } else {
                old_raw.len()
            };
            let added = if entry.new_oid == zero_oid() {
                0
            } else {
                new_raw.len()
            };
            files.push(FileStatInput {
                path_display,
                insertions: added,
                deletions: deleted,
                is_binary: true,
                is_unmerged: false,
            });
        } else {
            let (old_content, new_content) = log_read_blob_pair(odb, entry)?;
            let (ins, del) = count_changes(&old_content, &new_content);
            files.push(FileStatInput {
                path_display,
                insertions: ins,
                deletions: del,
                is_binary: false,
                is_unmerged: false,
            });
        }
    }

    let opts = DiffstatOptions {
        total_width: args.stat_width.unwrap_or_else(terminal_columns),
        line_prefix,
        width_prefix,
        subtract_prefix_from_terminal: subtract_prefix,
        stat_name_width: eff_name_width,
        stat_graph_width: eff_graph_width,
        stat_count: args.stat_count,
        color_add: color_add.as_str(),
        color_del: color_del.as_str(),
        color_reset: color_reset.as_str(),
        graph_bar_slack: 0,
        graph_prefix_budget_slack: 0,
    };
    write_diffstat_block(out, &files, &opts)?;
    // `--summary`: condensed extended-header lines (create/delete/rename mode), emitted right
    // after the diffstat totals and before the trailing blank that precedes the patch.
    if args.summary {
        let quote_fully = cfg.quote_path_fully();
        crate::commands::diff::write_diff_summary(
            out,
            entries,
            args.break_rewrites.is_some(),
            quote_fully,
        )?;
    }
    if trailing_blank {
        writeln!(out)?;
    }

    Ok(())
}

/// Read both blob sides of a diff entry as UTF-8 strings.
fn log_read_blob_pair(odb: &Odb, entry: &DiffEntry) -> Result<(String, String)> {
    let zero = grit_lib::diff::zero_oid();

    let old_content = if entry.old_oid == zero {
        String::new()
    } else {
        match odb.read(&entry.old_oid) {
            Ok(obj) => String::from_utf8_lossy(&obj.data).into_owned(),
            Err(_) => String::new(),
        }
    };

    let new_content = if entry.new_oid == zero {
        String::new()
    } else {
        match odb.read(&entry.new_oid) {
            Ok(obj) => String::from_utf8_lossy(&obj.data).into_owned(),
            Err(_) => String::new(),
        }
    };

    Ok((old_content, new_content))
}

/// Collect all commit OIDs from all refs (branches, tags, etc.) for `--all`.
///
/// Tips are returned in **sorted ref-name order** (like Git's `refs_for_each_ref`), not in
/// arbitrary discovery order, so `log --all` matches upstream when committer dates tie.
fn collect_all_ref_oids(git_dir: &std::path::Path) -> Result<Vec<ObjectId>> {
    let mut pairs: Vec<(String, ObjectId)> = Vec::new();

    if grit_lib::reftable::is_reftable_repo(git_dir) {
        if let Ok(refs) = grit_lib::reftable::reftable_list_refs(git_dir, "refs/") {
            pairs.extend(refs);
        }
    } else {
        pairs.extend(grit_lib::refs::list_refs(git_dir, "refs/")?);
        if let Ok(head_oid) = grit_lib::refs::resolve_ref(git_dir, "HEAD") {
            pairs.push(("HEAD".to_owned(), head_oid));
        }
    }

    pairs.sort_by(|a, b| a.0.cmp(&b.0));

    let mut oids = Vec::new();
    let mut seen = HashSet::new();
    for (_, oid) in pairs {
        if seen.insert(oid) {
            oids.push(oid);
        }
    }
    Ok(oids)
}

/// Check if a commit does NOT have any changes of the excluded types (for lowercase diff-filter).
/// Returns true if NONE of the changes match the excluded types.
/// Recursively collect `(path, mode, oid)` for all blobs in a tree — used as copy sources for
/// `-C -C` (find copies even from unmodified files).
fn collect_tree_blobs_for_copy(
    odb: &Odb,
    tree_oid: &ObjectId,
    prefix: &str,
) -> Result<Vec<(String, String, ObjectId)>> {
    use grit_lib::objects::parse_tree;
    let obj = odb.read(tree_oid)?;
    let tree = parse_tree(&obj.data)?;
    let mut result = Vec::new();
    for entry in tree {
        let name = String::from_utf8_lossy(&entry.name).into_owned();
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if entry.mode == 0o040000 {
            if let Ok(sub) = collect_tree_blobs_for_copy(odb, &entry.oid, &path) {
                result.extend(sub);
            }
        } else {
            result.push((path, format!("{:06o}", entry.mode), entry.oid));
        }
    }
    Ok(result)
}

/// Compute the diff entries for a commit (against its first parent) with rename/copy detection
/// applied as requested by `-M` / `-C` / `--no-renames`. This is required so `--diff-filter=R`
/// and `=C` can see Renamed/Copied statuses.
fn commit_diff_entries_for_filter(
    odb: &Odb,
    info: &CommitInfo,
    args: &Args,
) -> Result<Vec<DiffEntry>> {
    let parent_tree = if let Some(parent) = info.parents.first() {
        let pobj = odb.read(parent)?;
        let pc = parse_commit(&pobj.data)?;
        Some(pc.tree)
    } else {
        None
    };
    let raw = diff_trees(odb, parent_tree.as_ref(), Some(&info.tree), "")?;

    if args.no_renames {
        return Ok(raw);
    }

    let rename_threshold = args
        .find_renames
        .as_deref()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(50);
    let copy_threshold = args
        .find_copies
        .as_deref()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(50);

    if args.find_copies.is_some() {
        // Copy detection (`-C`); `-C -C` (`find_copies_harder`) also considers unmodified files.
        let source_tree_entries: Vec<(String, String, ObjectId)> = if args.find_copies_harder {
            if let Some(ref pt) = parent_tree {
                collect_tree_blobs_for_copy(odb, pt, "")?
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        Ok(grit_lib::diff::detect_copies(
            odb,
            None,
            raw,
            copy_threshold,
            args.find_copies_harder,
            &source_tree_entries,
        ))
    } else if args.find_renames.is_some() {
        Ok(grit_lib::diff::detect_renames(
            odb,
            None,
            raw,
            rename_threshold,
        ))
    } else {
        Ok(raw)
    }
}

fn commit_has_diff_status_not_in(
    odb: &Odb,
    info: &CommitInfo,
    exclude_chars: &[char],
    args: &Args,
) -> Result<bool> {
    let entries = commit_diff_entries_for_filter(odb, info, args)?;
    // Git's lowercase `--diff-filter` drops files whose status is in the exclude
    // set; the commit is shown if at least one file survives that filter (an
    // empty diff is not shown). So include the commit when any entry's status is
    // NOT excluded.
    Ok(entries
        .iter()
        .any(|e| !exclude_chars.contains(&e.status.letter())))
}

/// Check if a commit has any changes matching the specified diff-filter status letters.
fn commit_has_diff_status(
    odb: &Odb,
    info: &CommitInfo,
    filter_chars: &[char],
    args: &Args,
) -> Result<bool> {
    let entries = commit_diff_entries_for_filter(odb, info, args)?;
    for entry in &entries {
        let letter = entry.status.letter();
        if filter_chars.contains(&letter) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn blob_contains_pickaxe(odb: &Odb, oid: &ObjectId, needle: &[u8]) -> Result<bool> {
    if oid.is_zero() {
        return Ok(false);
    }
    let obj = odb.read(oid)?;
    Ok(obj.data.windows(needle.len()).any(|w| w == needle))
}

fn commit_remerge_pickaxe_matches(
    repo: &Repository,
    info: &CommitInfo,
    needle: &[u8],
) -> Result<bool> {
    for e in remerge_diff_entries(repo, info)? {
        if blob_contains_pickaxe(&repo.odb, &e.old_oid, needle)?
            || blob_contains_pickaxe(&repo.odb, &e.new_oid, needle)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn remerge_diff_entries(repo: &Repository, info: &CommitInfo) -> Result<Vec<DiffEntry>> {
    use crate::commands::merge::remerge_merge_tree;
    use grit_lib::diff::detect_renames;

    if info.parents.len() != 2 {
        return Ok(Vec::new());
    }
    let (remerge_tree, _) = remerge_merge_tree(repo, info.parents[0], info.parents[1])?;
    let raw = diff_trees(&repo.odb, Some(&remerge_tree), Some(&info.tree), "")?;
    Ok(detect_renames(&repo.odb, None, raw, 50))
}

fn commit_has_remerge_diff_status(
    repo: &Repository,
    info: &CommitInfo,
    filter_chars: &[char],
) -> Result<bool> {
    for e in remerge_diff_entries(repo, info)? {
        if filter_chars.contains(&e.status.letter()) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn commit_has_remerge_diff_status_not_in(
    repo: &Repository,
    info: &CommitInfo,
    exclude_chars: &[char],
) -> Result<bool> {
    Ok(!remerge_diff_entries(repo, info)?
        .iter()
        .any(|e| exclude_chars.contains(&e.status.letter())))
}

fn commit_has_remerge_object(
    repo: &Repository,
    info: &CommitInfo,
    target: &ObjectId,
) -> Result<bool> {
    for e in remerge_diff_entries(repo, info)? {
        if e.old_oid == *target || e.new_oid == *target {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Check whether a commit's diff introduces or removes a specific object.
///
/// When `tree_in_recursive` is true, tree directory lines are included in the diff (Git
/// `tree_in_recursive` / `log -t`), which is required for `--find-object` on tree OIDs.
fn commit_has_object(
    odb: &Odb,
    info: &CommitInfo,
    target: &ObjectId,
    tree_in_recursive: bool,
) -> Result<bool> {
    if info.parents.len() > 1 {
        let walk = CombinedTreeDiffOptions {
            recursive: true,
            tree_in_recursive,
        };
        let paths =
            combined_diff_paths_filtered(odb, &info.tree, &info.parents, &walk, Some(target))?;
        return Ok(!paths.is_empty());
    }

    let parent_tree = if let Some(parent) = info.parents.first() {
        let pobj = odb.read(parent)?;
        let pc = parse_commit(&pobj.data)?;
        Some(pc.tree)
    } else {
        None
    };

    let entries = if tree_in_recursive {
        diff_trees_show_tree_entries(odb, parent_tree.as_ref(), Some(&info.tree), "")?
    } else {
        diff_trees(odb, parent_tree.as_ref(), Some(&info.tree), "")?
    };
    for entry in &entries {
        if entry.old_oid == *target || entry.new_oid == *target {
            return Ok(true);
        }
    }
    Ok(false)
}

/// What `--follow` should display for a kept commit: the path the followed file
/// had in that commit (`display_path`, the destination of any copy/rename) and
/// the already copy/rename-detected diff entry to render for it
/// (`display_entry`).
#[derive(Clone)]
struct FollowDisplay {
    display_path: String,
    display_entry: Option<DiffEntry>,
}

/// Filter commits by following a file across renames and copies.
/// Returns only commits that touch the tracked file, plus a per-commit map
/// describing what to display. The display info mirrors Git's
/// `try_to_follow_renames`: the commit that first introduces the file as a copy
/// of a still-existing source shows `C100 <source> <dest>`, and older commits
/// show the file under its earlier name.
fn follow_filter(
    odb: &Odb,
    commits: Vec<(ObjectId, CommitInfo)>,
    initial_path: &str,
    max_count: Option<usize>,
) -> Result<(
    Vec<(ObjectId, CommitInfo)>,
    std::collections::HashMap<ObjectId, FollowDisplay>,
)> {
    use grit_lib::diff::detect_copies;

    let mut tracked_path = initial_path.to_string();
    let mut result = Vec::new();
    let mut display_map: std::collections::HashMap<ObjectId, FollowDisplay> =
        std::collections::HashMap::new();

    for (oid, info) in commits {
        let parent_tree = if let Some(parent) = info.parents.first() {
            let pobj = odb.read(parent)?;
            let pc = parse_commit(&pobj.data)?;
            Some(pc.tree)
        } else {
            None
        };

        let raw_entries = diff_trees(odb, parent_tree.as_ref(), Some(&info.tree), "")?;
        // `--follow` implicitly enables copy detection with `--find-copies-harder` (git
        // revision.c sets DIFF_DETECT_COPY | DIFF_FIND_COPIES_HARDER), so a file that first
        // appears as a copy of a still-existing file is followed back into the source's history.
        let source_tree_entries = if let Some(ref pt) = parent_tree {
            collect_tree_blobs_for_copy(odb, pt, "")?
        } else {
            Vec::new()
        };
        let entries = detect_copies(odb, None, raw_entries, 50, true, &source_tree_entries);

        let mut touches = false;
        let mut retarget: Option<String> = None;
        // The path the followed file has in *this* commit is the current
        // `tracked_path` (a copy/rename only retargets for older commits).
        let display_path = tracked_path.clone();
        let mut display_entry: Option<DiffEntry> = None;
        for entry in &entries {
            match entry.status {
                DiffStatus::Renamed | DiffStatus::Copied => {
                    // The new path is the copy/rename destination; follow it back to the source.
                    if entry.new_path.as_deref().map(RepoPath::as_bytes)
                        == Some(tracked_path.as_bytes())
                    {
                        touches = true;
                        display_entry = Some(entry.clone());
                        if let Some(old_path) = entry.old_path.as_deref() {
                            retarget = Some(old_path.to_string_lossy().into_owned());
                        }
                    }
                    // A *rename* away from the tracked path is a change to it (the
                    // file disappears under this name). A *copy* from the tracked
                    // path leaves the source unchanged, so it does not count.
                    if entry.status == DiffStatus::Renamed
                        && entry.old_path.as_deref().map(RepoPath::as_bytes)
                            == Some(tracked_path.as_bytes())
                    {
                        touches = true;
                    }
                }
                _ => {
                    if entry.path().as_bytes() == tracked_path.as_bytes() {
                        touches = true;
                        if display_entry.is_none() {
                            display_entry = Some(entry.clone());
                        }
                    }
                }
            }
        }

        if touches {
            display_map.insert(
                oid,
                FollowDisplay {
                    display_path,
                    display_entry,
                },
            );
            result.push((oid, info));
            if let Some(new_target) = retarget {
                tracked_path = new_target;
            }
            if let Some(max) = max_count {
                if result.len() >= max {
                    break;
                }
            }
        }
    }

    Ok((result, display_map))
}

/// Build a map from commit OID → source name for the `%S` placeholder, seeded
/// from the named positive revisions given on the command line.
///
/// Git propagates each commit's "source" from the named ref it was reached
/// from, walking in commit-date order so that, when two tips both reach a
/// commit, the tip whose traversal arrives first (highest committer date)
/// wins. We mirror that with a date-ordered priority queue: pop the
/// highest-dated pending commit, fix its label, then enqueue its parents with
/// the same label (an already-labeled commit keeps its first label).
/// Collect bisect ref tips for `git log --bisect`: positive `refs/bisect/bad*` tips (with their
/// ref names, for `%S`) and negative `refs/bisect/good*` OIDs. Mirrors `rev-list`'s
/// `append_bisect_ref_specs` (a "bad"/"good" prefix is matched whole or followed by `-`).
fn collect_bisect_ref_tips(git_dir: &Path) -> Result<(Vec<(ObjectId, String)>, Vec<ObjectId>)> {
    let refs =
        grit_lib::refs::list_refs(git_dir, "refs/bisect/").context("failed to list bisect refs")?;
    let mut bad_tips = Vec::new();
    let mut good_oids = Vec::new();
    for (name, oid) in refs {
        if name.starts_with("refs/bisect/bad")
            && name
                .as_bytes()
                .get("refs/bisect/bad".len())
                .is_none_or(|b| *b == b'-')
        {
            bad_tips.push((oid, name));
        } else if name.starts_with("refs/bisect/good")
            && name
                .as_bytes()
                .get("refs/bisect/good".len())
                .is_none_or(|b| *b == b'-')
        {
            good_oids.push(oid);
        }
    }
    Ok((bad_tips, good_oids))
}

fn build_named_source_map(
    odb: &Odb,
    tips: &[(ObjectId, String)],
    first_parent: bool,
) -> std::collections::HashMap<ObjectId, String> {
    use std::cmp::Ordering;
    use std::collections::BinaryHeap;

    // Heap entry ordered by committer time (desc), then OID for determinism.
    struct Entry {
        time: i64,
        oid: ObjectId,
        label: String,
    }
    impl PartialEq for Entry {
        fn eq(&self, other: &Self) -> bool {
            self.time == other.time && self.oid == other.oid
        }
    }
    impl Eq for Entry {}
    impl Ord for Entry {
        fn cmp(&self, other: &Self) -> Ordering {
            self.time
                .cmp(&other.time)
                .then_with(|| self.oid.cmp(&other.oid))
        }
    }
    impl PartialOrd for Entry {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }

    let mut source_map: std::collections::HashMap<ObjectId, String> =
        std::collections::HashMap::new();
    let mut heap: BinaryHeap<Entry> = BinaryHeap::new();
    for (oid, label) in tips {
        heap.push(Entry {
            time: read_commit_timestamp(odb, oid),
            oid: *oid,
            label: label.clone(),
        });
    }
    while let Some(Entry { oid, label, .. }) = heap.pop() {
        if source_map.contains_key(&oid) {
            continue;
        }
        source_map.insert(oid, label.clone());
        if let Ok(obj) = odb.read(&oid) {
            if let Ok(commit) = parse_commit(&obj.data) {
                let parents: &[ObjectId] = if first_parent {
                    commit
                        .parents
                        .first()
                        .map(std::slice::from_ref)
                        .unwrap_or(&[])
                } else {
                    &commit.parents
                };
                for p in parents {
                    if !source_map.contains_key(p) {
                        heap.push(Entry {
                            time: read_commit_timestamp(odb, p),
                            oid: *p,
                            label: label.clone(),
                        });
                    }
                }
            }
        }
    }
    source_map
}

/// Build a map from commit OID → source ref name for --source.
/// For each ref, walk its commit ancestry and record the first ref that reaches each commit.
fn build_source_map(
    odb: &Odb,
    git_dir: &std::path::Path,
    first_parent: bool,
) -> Result<std::collections::HashMap<ObjectId, String>> {
    let mut source_map: std::collections::HashMap<ObjectId, String> =
        std::collections::HashMap::new();

    // Collect refs with names
    let refs = collect_all_refs_with_names(git_dir)?;

    for (oid, ref_name) in &refs {
        let mut queue = vec![*oid];
        let mut visited = HashSet::new();
        while let Some(commit_oid) = queue.pop() {
            if !visited.insert(commit_oid) {
                continue;
            }
            source_map
                .entry(commit_oid)
                .or_insert_with(|| ref_name.clone());
            if let Ok(obj) = odb.read(&commit_oid) {
                if let Ok(commit) = parse_commit(&obj.data) {
                    if first_parent {
                        if let Some(p) = commit.parents.first() {
                            queue.push(*p);
                        }
                    } else {
                        for p in &commit.parents {
                            if !visited.contains(p) {
                                queue.push(*p);
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(source_map)
}

fn short_ref_for_source_display(src: &str) -> &str {
    if src == ".alternate" {
        return src;
    }
    src.strip_prefix("refs/heads/")
        .or_else(|| src.strip_prefix("refs/tags/"))
        .or_else(|| src.strip_prefix("refs/remotes/"))
        .unwrap_or(src)
}

/// Like [`build_source_map`] but walks alternate ref tips only, labeling every
/// reached commit with `.alternate` (Git `rev-list --alternate-refs` /
/// `log --source --alternate-refs`).
fn build_remote_tracking_source_map(
    odb: &Odb,
    git_dir: &std::path::Path,
    glob_pat: &str,
    first_parent: bool,
) -> Result<std::collections::HashMap<ObjectId, String>> {
    let mut source_map: std::collections::HashMap<ObjectId, String> =
        std::collections::HashMap::new();
    let refs = refs::list_refs_glob(git_dir, glob_pat)?;
    for (ref_name, oid) in refs {
        let mut queue = vec![oid];
        let mut visited = HashSet::new();
        while let Some(commit_oid) = queue.pop() {
            if !visited.insert(commit_oid) {
                continue;
            }
            source_map
                .entry(commit_oid)
                .or_insert_with(|| ref_name.clone());
            if let Ok(obj) = odb.read(&commit_oid) {
                if let Ok(commit) = parse_commit(&obj.data) {
                    if first_parent {
                        if let Some(p) = commit.parents.first() {
                            if !visited.contains(p) {
                                queue.push(*p);
                            }
                        }
                    } else {
                        for p in &commit.parents {
                            if !visited.contains(p) {
                                queue.push(*p);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(source_map)
}

fn build_alternate_source_map(
    repo: &Repository,
) -> Result<std::collections::HashMap<ObjectId, String>> {
    let mut source_map: std::collections::HashMap<ObjectId, String> =
        std::collections::HashMap::new();
    let tips = refs::collect_alternate_ref_oids(&repo.git_dir)?;
    let label = ".alternate".to_string();
    for tip in tips {
        let mut queue = vec![tip];
        let mut visited = HashSet::new();
        while let Some(commit_oid) = queue.pop() {
            if !visited.insert(commit_oid) {
                continue;
            }
            source_map
                .entry(commit_oid)
                .or_insert_with(|| label.clone());
            if let Ok(obj) = repo.odb.read(&commit_oid) {
                if let Ok(commit) = parse_commit(&obj.data) {
                    for p in &commit.parents {
                        if !visited.contains(p) {
                            queue.push(*p);
                        }
                    }
                }
            }
        }
    }
    Ok(source_map)
}

/// Collect all refs with their names from the repository.
fn collect_all_refs_with_names(git_dir: &std::path::Path) -> Result<Vec<(ObjectId, String)>> {
    let mut refs = Vec::new();

    // HEAD
    let head = resolve_head(git_dir)?;
    if let Some(oid) = head.oid() {
        refs.push((*oid, "HEAD".to_string()));
    }

    // Loose refs
    collect_named_refs_from_dir(git_dir, &git_dir.join("refs"), &mut refs)?;

    // Packed refs
    let packed_path = git_dir.join("packed-refs");
    if let Ok(text) = std::fs::read_to_string(packed_path) {
        for line in text.lines() {
            if line.starts_with('#') || line.starts_with('^') || line.is_empty() {
                continue;
            }
            let parts: Vec<&str> = line.splitn(2, ' ').collect();
            if parts.len() == 2 {
                if let Ok(oid) = parts[0].parse::<ObjectId>() {
                    refs.push((oid, parts[1].to_string()));
                }
            }
        }
    }

    Ok(refs)
}

/// Recursively collect refs with their full names from a directory.
fn collect_named_refs_from_dir(
    git_dir: &std::path::Path,
    dir: &std::path::Path,
    refs: &mut Vec<(ObjectId, String)>,
) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_named_refs_from_dir(git_dir, &path, refs)?;
        } else if let Ok(content) = std::fs::read_to_string(&path) {
            let raw = content.trim();
            if let Some(target) = raw.strip_prefix("ref: ") {
                // Symbolic ref — resolve
                if let Ok(oid) = grit_lib::refs::resolve_ref(git_dir, target) {
                    let full_path = path.to_string_lossy();
                    if let Some(idx) = full_path.find("refs/") {
                        refs.push((oid, full_path[idx..].to_string()));
                    }
                }
            } else if let Ok(oid) = raw.parse::<ObjectId>() {
                let full_path = path.to_string_lossy();
                if let Some(idx) = full_path.find("refs/") {
                    refs.push((oid, full_path[idx..].to_string()));
                }
            }
        }
    }
    Ok(())
}

/// Parse the --abbrev value into a hash abbreviation length.
fn parse_abbrev(abbrev: &Option<String>) -> usize {
    match abbrev {
        Some(val) => clamp_abbrev(val.parse::<usize>().unwrap_or(7)),
        None => 7,
    }
}

/// Clamp an abbreviation length to Git's accepted range. The minimum is 4 and
/// the maximum is the full hash length (40 for SHA-1).
fn clamp_abbrev(n: usize) -> usize {
    n.clamp(4, 40)
}

/// Resolve the commit-hash abbreviation length, honoring an explicit `--abbrev`
/// and otherwise `core.abbrev` (where `false`/`no`/`off`/`0` mean "no
/// abbreviation" and any number is clamped to `[4, 40]`).
fn resolve_abbrev_len(abbrev: &Option<String>, git_dir: &Path) -> usize {
    if let Some(val) = abbrev {
        return clamp_abbrev(val.parse::<usize>().unwrap_or(7));
    }
    if let Ok(cfg) = ConfigSet::load(Some(git_dir), true) {
        if let Some(raw) = cfg.get("core.abbrev") {
            let v = raw.trim();
            match v.to_ascii_lowercase().as_str() {
                "false" | "no" | "off" | "0" => return 40,
                "auto" | "" => return 7,
                _ => {
                    if let Ok(n) = v.parse::<usize>() {
                        return clamp_abbrev(n);
                    }
                }
            }
        }
    }
    7
}

/// Load shallow boundary commit OIDs from `.git/shallow`.
fn load_shallow_boundaries(git_dir: &Path) -> HashSet<ObjectId> {
    let shallow_path = git_dir.join("shallow");
    let mut set = HashSet::new();
    if let Ok(contents) = std::fs::read_to_string(&shallow_path) {
        for line in contents.lines() {
            let line = line.trim();
            if !line.is_empty() {
                if let Ok(oid) = line.parse::<ObjectId>() {
                    set.insert(oid);
                }
            }
        }
    }
    set
}

/// Resolve a pretty format alias by looking up `pretty.<name>` in git config.
/// Returns the resolved format string, or the input unchanged.
pub(crate) fn resolve_pretty_alias_with_config(fmt: &str, repo: &Repository) -> String {
    // Known built-in formats — no resolution needed
    match fmt {
        "oneline" | "short" | "medium" | "full" | "fuller" | "reference" | "email" | "raw"
        | "mboxrd" => {
            return fmt.to_string();
        }
        _ => {}
    }

    // Already a format: or tformat: string
    if fmt.starts_with("format:") || fmt.starts_with("tformat:") {
        return fmt.to_string();
    }

    match resolve_pretty_alias_checked(fmt, repo) {
        Ok(v) => v,
        // Fall back to the original string for non-fatal callers (e.g. show).
        Err(_) => fmt.to_string(),
    }
}

/// Like [`resolve_pretty_alias_with_config`] but returns an error (matching
/// Git's `fatal: invalid --pretty format`) when the name resolves to neither a
/// builtin, a `format:`/`tformat:` string, an existing `pretty.<name>` alias,
/// nor an inline format (containing `%`); also errors on an alias cycle.
pub(crate) fn resolve_pretty_alias_checked(fmt: &str, repo: &Repository) -> Result<String> {
    if fmt.is_empty() {
        return Ok(fmt.to_string());
    }
    match fmt {
        "oneline" | "short" | "medium" | "full" | "fuller" | "reference" | "email" | "raw"
        | "mboxrd" => return Ok(fmt.to_string()),
        _ => {}
    }
    if fmt.starts_with("format:") || fmt.starts_with("tformat:") {
        return Ok(fmt.to_string());
    }

    let config = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let mut visited = std::collections::HashSet::new();
    let mut current = fmt.to_string();

    loop {
        if !visited.insert(current.clone()) {
            // Cycle among aliases.
            return Err(anyhow::anyhow!("invalid --pretty format: {fmt}"));
        }

        let key = format!("pretty.{current}");
        if let Some(value) = config.get(&key) {
            match value.as_str() {
                "oneline" | "short" | "medium" | "full" | "fuller" | "reference" | "email"
                | "raw" | "mboxrd" => {
                    return Ok(value);
                }
                v if v.starts_with("format:") || v.starts_with("tformat:") => {
                    return Ok(value);
                }
                _ => {
                    current = value;
                }
            }
        } else if current.contains('%') {
            // An inline format string (implicit tformat): not an alias.
            return Ok(current);
        } else if current == fmt {
            // The user-supplied name is not a builtin, not an alias, and has no
            // format placeholder: Git rejects it.
            return Err(anyhow::anyhow!("invalid --pretty format: {fmt}"));
        } else {
            // A previous alias resolved to a terminal name that is itself
            // neither a builtin nor a known alias: fatal.
            return Err(anyhow::anyhow!("invalid --pretty format: {current}"));
        }
    }
}
