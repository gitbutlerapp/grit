//! `grit show` — show various types of objects.
//!
//! For commits, displays the commit header (like `git log -1`) followed by the
//! diff introduced by that commit.  For tags, shows the tag object then the
//! tagged commit.  For trees, lists top-level names (Git `ls-tree --name-only`
//! style).  For blobs, prints the raw blob content.
//!
//! Like Git, `show` defaults to `--no-walk` but switches to a `rev-list` walk
//! when revision ranges, exclusions, `-n` / `--max-count`, or `--merge` are used.

use crate::commands::log::show_notes_display_enabled;
use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::combined_diff_patch::CombinedDiffWsOptions;
use grit_lib::combined_tree_diff::{combined_diff_paths_filtered, CombinedTreeDiffOptions};
use grit_lib::config::ConfigSet;
use grit_lib::diff::{
    anchored_unified_diff, detect_copies, detect_renames, diff_trees,
    parse_indent_heuristic_cli_flags, resolve_indent_heuristic, unified_diff, zero_oid, DiffEntry,
    DiffStatus,
};
use grit_lib::repo_path::RepoPath;
use grit_lib::diffstat::{terminal_columns, write_diffstat_block, DiffstatOptions, FileStatInput};
use grit_lib::merge_base::merge_bases_first_vs_rest;
use grit_lib::merge_diff::{
    blob_oid_at_path, blob_text_for_diff, blob_text_for_diff_with_oid, diff_textconv_active,
    format_combined_binary, format_combined_textconv_patch, format_parent_patch,
    is_binary_for_diff, read_blob_at_path,
};
use grit_lib::objects::{parse_commit, parse_tag, parse_tree, ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::refs::{list_refs, resolve_ref};
use grit_lib::repo::Repository;

use crate::commands::promisor_hydrate::{prefetch_promisor_for_diff_entries, PromisorDiffPrefetch};
use grit_lib::rev_list::{
    collect_revision_specs_with_stdin, merge_bases, rev_list, OrderingMode, RevListOptions,
};
use grit_lib::rev_parse::{
    peel_to_tree, resolve_revision, resolve_revision_without_index_dwim, split_double_dot_range,
    split_treeish_colon,
};
use std::collections::{BTreeSet, HashMap};
use std::io::{self, Write};
use std::path::Path;

/// Arguments for `grit show`.
#[derive(Debug, ClapArgs)]
#[command(
    about = "Show various types of objects (commits, trees, blobs, tags)",
    allow_negative_numbers = true
)]
pub struct Args {
    /// Limit the number of commits shown when walking history (`-2` = two commits).
    #[arg(short = 'n', long = "max-count", value_name = "N")]
    pub max_count: Option<usize>,

    /// Draw a text-based representation of the commit history (incompatible with `show`'s default `--no-walk`).
    #[arg(long = "graph")]
    pub graph: bool,

    /// Show diffs relevant to resolving a merge with unmerged index entries (uses `MERGE_HEAD` and related pseudorefs).
    #[arg(long = "merge")]
    pub show_merge: bool,

    /// Object(s) to show (commit, tree, blob, tag, or revision range). Defaults to HEAD.
    #[arg(num_args = 0.., allow_hyphen_values = true, trailing_var_arg = true)]
    pub objects: Vec<String>,

    /// Show only one line per commit (short hash + subject).
    #[arg(long = "oneline")]
    pub oneline: bool,

    /// Pretty-print format (`git show --format=...`).
    #[arg(long = "format", value_name = "FORMAT")]
    pub format: Option<String>,

    /// Pretty-print format (`git show --pretty` defaults to `medium` like C Git).
    #[arg(
        long = "pretty",
        value_name = "FORMAT",
        num_args = 0..=1,
        default_missing_value = "medium"
    )]
    pub pretty: Option<String>,

    /// Re-code commit messages to the given encoding, or `none` for raw bytes.
    #[arg(long = "encoding", value_name = "ENCODING")]
    pub encoding: Option<String>,

    /// Expand tabs in commit log message to spaces (`--expand-tabs` is `--expand-tabs=8`).
    #[arg(long = "expand-tabs", value_name = "N", require_equals = true)]
    pub expand_tabs: Option<String>,

    /// Do not expand tabs in commit log output (same as `--expand-tabs=0`).
    #[arg(long = "no-expand-tabs")]
    pub no_expand_tabs: bool,

    /// Suppress diff output (show only the commit header).
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Suppress diff output (alias for --quiet / -q).
    #[arg(short = 's', long = "no-patch")]
    pub no_patch: bool,

    /// Number of unified context lines for diff output.
    #[arg(short = 'U', long = "unified", value_name = "N")]
    pub unified: Option<usize>,

    /// Anchored diff: keep the specified text as context.
    #[arg(long = "anchored")]
    pub anchored: Vec<String>,

    /// Use the patience diff algorithm.
    #[arg(long = "patience")]
    pub patience: bool,

    /// Show a diffstat summary after the commit header (`--stat[=width[,name-width[,count]]]`).
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

    /// Show raw diff-tree output format.
    #[arg(long = "raw")]
    pub raw: bool,

    /// Verify and display the signature of the commit (`--show-signature`).
    #[arg(long = "show-signature", overrides_with = "no_show_signature")]
    pub show_signature: bool,

    /// Do not display the signature even if `log.showSignature` is set.
    #[arg(long = "no-show-signature", overrides_with = "show_signature")]
    pub no_show_signature: bool,

    /// Show only names of changed files.
    #[arg(long = "name-only")]
    pub name_only: bool,

    /// Show names and status of changed files.
    #[arg(long = "name-status")]
    pub name_status: bool,

    /// Show a summary of extended header information (renames, mode changes).
    #[arg(long = "summary")]
    pub summary: bool,

    /// Show the patch (diff) output together with the diffstat.
    #[arg(long = "patch-with-stat")]
    pub patch_with_stat: bool,

    /// Show the patch (diff) output together with the raw output.
    #[arg(long = "patch-with-raw")]
    pub patch_with_raw: bool,

    /// Generate a patch.
    #[arg(short = 'p', long = "patch")]
    pub patch: bool,

    /// Show abbreviated OIDs.
    #[arg(long = "abbrev", value_name = "N", default_missing_value = "7", num_args = 0..=1, require_equals = true)]
    pub abbrev: Option<String>,

    /// Show full OIDs.
    #[arg(long = "no-abbrev")]
    pub no_abbrev: bool,

    /// Detect renames.
    #[arg(short = 'M', long = "find-renames", value_name = "N", default_missing_value = "50", num_args = 0..=1)]
    pub find_renames: Option<String>,

    /// Detect copies (use twice for harder).
    #[arg(short = 'C', long = "find-copies", value_name = "N", default_missing_value = "50", num_args = 0..=1, action = clap::ArgAction::Append)]
    pub find_copies: Vec<String>,

    /// Show the full diff (for merge commits).
    #[arg(short = 'm')]
    pub diff_merges: bool,

    /// For merge commits, diff against the first parent only (like `git log --first-parent`).
    #[arg(long = "first-parent")]
    pub first_parent: bool,

    /// Dense combined diff for merge commits (`diff --combined`).
    #[arg(short = 'c')]
    pub combined: bool,

    /// Dense combined diff for merge commits (`diff --cc`).
    #[arg(long = "cc")]
    pub combined_cc: bool,

    /// Date format for display.
    #[arg(long = "date")]
    pub date: Option<String>,

    /// Don't show external diff helper.
    #[arg(long = "no-ext-diff")]
    pub no_ext_diff: bool,

    /// Show notes.
    #[arg(long = "notes", num_args = 0..=1, default_missing_value = "", require_equals = true)]
    pub notes: Option<String>,

    /// Full diff index hashes.
    #[arg(long = "full-index")]
    pub full_index: bool,

    /// Colorize the output.
    #[arg(long = "color", value_name = "WHEN", default_missing_value = "always", num_args = 0..=1)]
    pub color: Option<String>,

    /// Disable color.
    #[arg(long = "no-color")]
    pub no_color: bool,

    /// Show short stat summary.
    #[arg(long = "shortstat")]
    pub shortstat: bool,

    /// Disable textconv.
    #[arg(long = "no-textconv")]
    pub no_textconv: bool,

    /// Run `diff.<driver>.textconv` for `rev:path` blob output (Git default is raw blob).
    #[arg(long = "textconv", hide = true)]
    pub textconv: bool,

    /// Ignore whitespace at end of line in combined diffs.
    #[arg(long = "ignore-space-at-eol")]
    pub ignore_space_at_eol: bool,

    /// Ignore changes in amount of whitespace in combined diffs.
    #[arg(short = 'b', long = "ignore-space-change")]
    pub ignore_space_change: bool,

    /// Ignore all whitespace in combined diffs.
    #[arg(short = 'w', long = "ignore-all-space")]
    pub ignore_all_space: bool,

    /// Ignore carriage return at end of line in combined diffs.
    #[arg(long = "ignore-cr-at-eol")]
    pub ignore_cr_at_eol: bool,

    /// Show binary diff in git binary format.
    #[arg(long = "binary")]
    pub binary: bool,

    /// Show numstat summary.
    #[arg(long = "numstat")]
    pub numstat: bool,

    /// Enable indent heuristic (plumbing compatibility; also parsed from argv).
    #[arg(long = "indent-heuristic", hide = true)]
    pub indent_heuristic: bool,

    /// Disable indent heuristic.
    #[arg(long = "no-indent-heuristic", hide = true)]
    pub no_indent_heuristic: bool,

    /// Show diff against a mechanical re-merge of the parents (merge commits).
    #[arg(long = "remerge-diff")]
    pub remerge_diff: bool,

    /// Limit diff to certain change types (same letters as `git log`).
    #[arg(long = "diff-filter", value_name = "FILTER")]
    pub diff_filter: Option<String>,

    /// Submodule diff format (`log` suppresses remerge-diff body in tests).
    #[arg(long = "submodule", value_name = "MODE")]
    pub submodule: Option<String>,

    /// Only include commits whose remerge diff touches this string (pickaxe).
    #[arg(short = 'S', value_name = "STRING", allow_hyphen_values = true)]
    pub pickaxe: Option<String>,

    /// Only include commits whose remerge diff touches this object.
    #[arg(long = "find-object", value_name = "OBJECT")]
    pub find_object: Option<String>,

    /// All refs (honoured with pickaxe / find-object filtering).
    #[arg(long = "all")]
    pub all: bool,
}

/// Run the `show` command.
pub fn run(mut args: Args) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;
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
    maybe_warn_deprecated_grafts(&repo)?;

    let diff_cfg = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let (argv_ind, argv_no) = parse_indent_heuristic_cli_flags(&args.objects);
    let indent_heuristic = resolve_indent_heuristic(
        &diff_cfg,
        args.indent_heuristic || argv_ind,
        args.no_indent_heuristic || argv_no,
    );
    if args.format.is_none() {
        args.format = args.pretty.clone();
    }

    // `git show` accepts `--pretty`/`--format` (and `-s`) *after* a revision
    // (e.g. `git show -s HEAD --pretty=short`). Clap's `trailing_var_arg`
    // captures everything after the first positional into `objects`, so pull
    // any such formatting options back out and apply them, mirroring git's
    // `setup_revisions` interleaving of options and revisions.
    // `--grep`/pattern-flavor flags interspersed in the trailing args (`git show`
    // shares `git log`'s revision walker) are pulled out here and applied as a
    // message filter on the shown commits, honoring `grep.patternType`.
    let mut grep_patterns: Vec<String> = Vec::new();
    let mut grep_fixed = false;
    let mut grep_basic = false;
    let mut grep_extended = false;
    let mut grep_perl = false;
    let mut grep_ignore_case = false;
    {
        let mut kept: Vec<String> = Vec::with_capacity(args.objects.len());
        let mut iter = args.objects.iter().peekable();
        while let Some(tok) = iter.next() {
            if let Some(v) = tok
                .strip_prefix("--pretty=")
                .or_else(|| tok.strip_prefix("--format="))
            {
                args.format = Some(v.to_owned());
            } else if tok == "--pretty" || tok == "--format" {
                if let Some(v) = iter.peek() {
                    args.format = Some((*v).clone());
                    iter.next();
                } else {
                    // Bare `--pretty` defaults to medium (git's behaviour).
                    args.format = Some("medium".to_owned());
                }
            } else if tok == "-s" || tok == "--no-patch" {
                args.no_patch = true;
            } else if let Some(v) = tok.strip_prefix("--encoding=") {
                args.encoding = Some(v.to_owned());
            } else if tok == "--encoding" {
                if let Some(v) = iter.peek() {
                    args.encoding = Some((*v).clone());
                    iter.next();
                }
            } else if let Some(v) = tok.strip_prefix("--grep=") {
                grep_patterns.push(v.to_owned());
            } else if tok == "--grep" {
                if let Some(v) = iter.peek() {
                    grep_patterns.push((*v).clone());
                    iter.next();
                }
            } else if tok == "--fixed-strings" {
                grep_fixed = true;
            } else if tok == "--basic-regexp" {
                // NB: do not alias `-G` here — in `git show` `-G` is the pickaxe
                // (`-G<regex>`), so only the long form selects basic-regexp grep.
                grep_basic = true;
            } else if tok == "--extended-regexp" {
                grep_extended = true;
            } else if tok == "--perl-regexp" {
                grep_perl = true;
            } else if tok == "--regexp-ignore-case" {
                grep_ignore_case = true;
            } else {
                kept.push(tok.clone());
            }
        }
        args.objects = kept;
    }
    let grep_res = if grep_patterns.is_empty() {
        Vec::new()
    } else {
        let cfg = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
        crate::commands::log::compile_command_grep_regexes(
            &grep_patterns,
            grep_fixed,
            grep_basic,
            grep_extended,
            grep_perl,
            grep_ignore_case,
            &cfg,
        )?
    };

    // `--root` forces a root commit's diff against the empty tree even when
    // `log.showroot=false`. It is not a real object, so strip it from the list.
    let want_root = args.objects.iter().any(|s| s == "--root");
    let mut raw_objects: Vec<String> = args
        .objects
        .iter()
        .filter(|s| s.as_str() != "--root")
        .cloned()
        .collect();
    args.objects = raw_objects.clone();
    while let Some(first) = raw_objects.first() {
        if first.len() > 1
            && first.starts_with('-')
            && first[1..].chars().all(|c| c.is_ascii_digit())
        {
            let n: usize = first[1..]
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid -n value: {first}"))?;
            args.max_count = Some(n);
            raw_objects.remove(0);
        } else {
            break;
        }
    }

    let (mut rev_strings_owned, pathspecs): (Vec<String>, Vec<String>) = if raw_objects.is_empty() {
        (vec!["HEAD".to_string()], Vec::new())
    } else if let Some(i) = raw_objects.iter().position(|s| s == "--") {
        let left: Vec<String> = raw_objects[..i].to_vec();
        let right: Vec<String> = raw_objects[i + 1..].to_vec();
        if left.is_empty() {
            (vec!["HEAD".to_string()], right)
        } else {
            (left, right)
        }
    } else {
        let mut split_at = 0usize;
        for s in &raw_objects {
            let looks_like_rev_spec = s.starts_with('^')
                || s.starts_with(':')
                || split_double_dot_range(s).is_some()
                || (s.contains("...") && !s.contains("...."))
                || split_treeish_colon(s).is_some_and(|(b, a)| !b.is_empty() && !a.is_empty());
            // Do not use index DWIM alone: a tracked filename like `numbers` must be a pathspec
            // (`git show rev -- numbers`), not mis-parsed as an extra revision (t4069.15).
            if looks_like_rev_spec || resolve_revision_without_index_dwim(&repo, s).is_ok() {
                split_at += 1;
            } else {
                break;
            }
        }
        if split_at == 0 {
            (vec!["HEAD".to_string()], raw_objects.clone())
        } else {
            (
                raw_objects[..split_at].to_vec(),
                raw_objects[split_at..].to_vec(),
            )
        }
    };

    if rev_strings_owned.is_empty() {
        rev_strings_owned.push("HEAD".to_string());
    }

    if args.graph {
        bail!("fatal: options '--no-walk' and '--graph' cannot be used together");
    }

    let rev_strings: Vec<&str> = rev_strings_owned.iter().map(|s| s.as_str()).collect();
    // `--name-only` / `--name-status` with a user `--format` string terminate each entry
    // themselves (tformat semantics), so git emits no blank line between successive commits.
    let user_format_name_listing = (args.name_only || args.name_status)
        && args.format.as_deref().is_some_and(|f| {
            !matches!(
                f,
                "medium" | "short" | "full" | "fuller" | "reference" | "oneline" | "raw" | "email"
            )
        });
    // `--oneline` (with the diff suppressed) prints one line per commit and Git
    // emits no blank line between them, like `--format=%s`.
    let oneline_compact = (args.oneline || args.format.as_deref() == Some("oneline"))
        && (args.quiet || args.no_patch);
    let custom_format_compact = args.format.as_deref().is_some_and(|f| {
        f.starts_with("format:")
            || f.starts_with("tformat:")
            || !matches!(
                f,
                "medium" | "short" | "full" | "fuller" | "reference" | "oneline" | "raw" | "email"
            )
    });
    let compact_multi_subject =
        custom_format_compact || user_format_name_listing || oneline_compact;

    let notes_map = load_notes_map(&repo);

    let stdout = io::stdout();
    let mut out = stdout.lock();

    let remerge_scan =
        args.remerge_diff && (args.pickaxe.is_some() || args.find_object.is_some() || args.all);

    if remerge_scan {
        use crate::commands::remerge_diff::{
            remerge_diff_matches_pickaxe_or_find, RemergeDiffOptions,
        };

        let find_oid = if let Some(ref s) = args.find_object {
            Some(resolve_revision(&repo, s).with_context(|| format!("unknown revision: '{s}'"))?)
        } else {
            None
        };

        let opts = RemergeDiffOptions {
            pathspecs: &pathspecs,
            diff_filter: args.diff_filter.as_deref(),
            pickaxe: args.pickaxe.as_deref(),
            find_object: find_oid,
            submodule_mode: args.submodule.as_deref(),
            context_lines: args.unified.unwrap_or(3),
            indent_heuristic,
        };

        let mut candidates: BTreeSet<ObjectId> = BTreeSet::new();

        if args.all {
            let gd = &repo.git_dir;
            if let Ok(oid) = resolve_ref(gd, "HEAD") {
                candidates.insert(oid);
            }
            for prefix in ["refs/heads/", "refs/tags/", "refs/remotes/"] {
                if let Ok(refs) = list_refs(gd, prefix) {
                    for (_name, oid) in refs {
                        candidates.insert(oid);
                    }
                }
            }
        } else {
            for spec in &rev_strings {
                let oid = resolve_revision(&repo, spec)
                    .with_context(|| format!("unknown revision or path: '{spec}'"))?;
                candidates.insert(oid);
            }
        }

        let mut matched: Vec<ObjectId> = Vec::new();
        for oid in candidates {
            let obj = match repo.odb.read(&oid) {
                Ok(o) => o,
                Err(_) => continue,
            };
            if obj.kind != ObjectKind::Commit {
                continue;
            }
            let commit = parse_commit(&obj.data).context("parsing commit")?;
            if remerge_diff_matches_pickaxe_or_find(&repo, &commit.tree, &commit.parents, &opts)? {
                matched.push(oid);
            }
        }

        if matched.is_empty() {
            return Ok(());
        }

        let emit_opts = RemergeDiffOptions {
            pathspecs: &pathspecs,
            diff_filter: args.diff_filter.as_deref(),
            pickaxe: None,
            find_object: None,
            submodule_mode: args.submodule.as_deref(),
            context_lines: args.unified.unwrap_or(3),
            indent_heuristic,
        };

        let mut remerge_shown = false;
        for oid in matched {
            let obj = repo.odb.read(&oid).context("reading object")?;
            if remerge_shown && !compact_multi_subject {
                writeln!(out)?;
            }
            show_commit(
                &mut out,
                &repo,
                &oid,
                &obj.data,
                &args,
                &notes_map,
                &pathspecs,
                Some(&emit_opts),
                None,
                indent_heuristic,
                want_root,
            )?;
            remerge_shown = true;
        }
        return Ok(());
    }

    let wants_walk = args.show_merge
        || args.max_count.is_some()
        || rev_strings.iter().any(|s| {
            s.starts_with('^')
                || split_double_dot_range(s).is_some()
                || (s.contains("...") && !s.contains("...."))
        });

    if wants_walk {
        let (positive_specs, negative_specs, _, _) =
            collect_revision_specs_with_stdin(&repo.git_dir, &rev_strings_owned, false)
                .map_err(|e| anyhow::anyhow!("failed to parse revision arguments: {e}"))?;

        let mut negative_specs = negative_specs;
        if args.show_merge {
            let (merge_pos, merge_neg, parent_oid) =
                build_specs_for_show_merge(&repo).context("show --merge")?;
            negative_specs.extend(merge_neg);
            // Git prepends merge walk tips before user arguments.
            let mut combined = merge_pos;
            combined.extend(positive_specs);
            let options = RevListOptions {
                max_count: args.max_count,
                ordering: OrderingMode::Topo,
                ..RevListOptions::default()
            };
            let result = rev_list(&repo, &combined, &negative_specs, &options)
                .map_err(|e| anyhow::anyhow!("rev-list failed: {e}"))?;
            let mut shown = false;
            // Git prints the current branch (HEAD) first, then the merge parent — reverse topo order.
            for oid in result.commits.into_iter().rev() {
                let obj = repo.odb.read(&oid).context("reading object")?;
                if obj.kind != ObjectKind::Commit {
                    continue;
                }
                if shown && !compact_multi_subject {
                    writeln!(out)?;
                }
                show_commit(
                    &mut out,
                    &repo,
                    &oid,
                    &obj.data,
                    &args,
                    &notes_map,
                    &pathspecs,
                    None,
                    Some(parent_oid),
                    indent_heuristic,
                    want_root,
                )?;
                shown = true;
            }
            return Ok(());
        }

        let symmetric = rev_strings
            .iter()
            .find(|s| s.contains("...") && !s.contains("....") && !s.starts_with('^'));
        if let Some(sym_tok) = symmetric {
            if let Some((l, r)) = sym_tok.split_once("...") {
                let lhs = if l.is_empty() { "HEAD" } else { l };
                let rhs = if r.is_empty() { "HEAD" } else { r };
                let lhs_oid = resolve_revision(&repo, lhs)
                    .with_context(|| format!("bad revision '{lhs}'"))?;
                let rhs_oid = resolve_revision(&repo, rhs)
                    .with_context(|| format!("bad revision '{rhs}'"))?;
                let bases = merge_bases(&repo, lhs_oid, rhs_oid, false)
                    .context("failed to compute merge bases for symmetric range")?;
                let mut neg = negative_specs;
                neg.extend(bases.iter().map(|b| b.to_hex()));
                let pos: Vec<String> = rev_strings_owned
                    .iter()
                    .filter(|s| *s != sym_tok)
                    .cloned()
                    .chain([lhs.to_string(), rhs.to_string()])
                    .collect();
                let options = RevListOptions {
                    max_count: args.max_count,
                    left_right: true,
                    symmetric_left: Some(lhs_oid),
                    symmetric_right: Some(rhs_oid),
                    ordering: OrderingMode::Topo,
                    ..RevListOptions::default()
                };
                let result = rev_list(&repo, &pos, &neg, &options)
                    .map_err(|e| anyhow::anyhow!("rev-list failed: {e}"))?;
                let mut shown = false;
                for oid in result.commits {
                    let obj = repo.odb.read(&oid).context("reading object")?;
                    if obj.kind != ObjectKind::Commit {
                        continue;
                    }
                    if shown && !compact_multi_subject {
                        writeln!(out)?;
                    }
                    show_commit(
                        &mut out,
                        &repo,
                        &oid,
                        &obj.data,
                        &args,
                        &notes_map,
                        &pathspecs,
                        None,
                        None,
                        indent_heuristic,
                        want_root,
                    )?;
                    shown = true;
                }
                return Ok(());
            }
        }

        let options = RevListOptions {
            max_count: args.max_count,
            ordering: OrderingMode::Topo,
            ..RevListOptions::default()
        };
        let result = rev_list(&repo, &positive_specs, &negative_specs, &options)
            .map_err(|e| anyhow::anyhow!("rev-list failed: {e}"))?;
        let mut shown = false;
        for oid in result.commits {
            let obj = repo.odb.read(&oid).context("reading object")?;
            if obj.kind != ObjectKind::Commit {
                continue;
            }
            if shown && !compact_multi_subject {
                writeln!(out)?;
            }
            show_commit(
                &mut out,
                &repo,
                &oid,
                &obj.data,
                &args,
                &notes_map,
                &pathspecs,
                None,
                None,
                indent_heuristic,
                want_root,
            )?;
            shown = true;
        }
        return Ok(());
    }

    let mut shown = false;
    for spec in &rev_strings {
        let oid = resolve_revision(&repo, spec)
            .with_context(|| format!("unknown revision or path: '{spec}'"))?;

        let obj = repo.odb.read(&oid).context("reading object")?;

        // `--grep` limits `git show` to commits whose message matches.
        if !grep_res.is_empty() && obj.kind == ObjectKind::Commit {
            let commit = parse_commit(&obj.data).context("parsing commit")?;
            if !grep_res.iter().any(|re| re.is_match(&commit.message)) {
                continue;
            }
        }

        if shown && !compact_multi_subject {
            writeln!(out)?;
        }

        match obj.kind {
            ObjectKind::Commit => {
                show_commit(
                    &mut out,
                    &repo,
                    &oid,
                    &obj.data,
                    &args,
                    &notes_map,
                    &pathspecs,
                    None,
                    None,
                    indent_heuristic,
                    want_root,
                )?;
            }
            ObjectKind::Tag => {
                show_tag(
                    &mut out,
                    &repo,
                    spec,
                    &obj.data,
                    &args,
                    &notes_map,
                    indent_heuristic,
                )?;
            }
            ObjectKind::Tree => {
                show_tree_named(&mut out, &repo, spec, oid)?;
            }
            ObjectKind::Blob => {
                if args.textconv && !args.no_textconv {
                    if let Some((_rev, path_after)) = split_treeish_colon(spec) {
                        if !path_after.is_empty() {
                            let config =
                                ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
                            let text = blob_text_for_diff_with_oid(
                                &repo.odb,
                                repo.git_dir.as_path(),
                                &config,
                                path_after,
                                &obj.data,
                                &oid,
                                true,
                            );
                            out.write_all(text.as_bytes())?;
                            continue;
                        }
                    }
                }
                out.write_all(&obj.data)?;
            }
        }
        shown = true;
    }

    Ok(())
}

/// Build `rev-list` positive/negative specs for `git show --merge` (merge in progress).
///
/// Returns `(positive, negative, merge_parent_oid)` where `merge_parent_oid` is the commit
/// named by the active pseudoref (`MERGE_HEAD`, etc.) for parent-specific diffs.
fn build_specs_for_show_merge(repo: &Repository) -> Result<(Vec<String>, Vec<String>, ObjectId)> {
    let head_oid = resolve_revision(repo, "HEAD").context("show --merge without HEAD?")?;
    let (other_name, other_oid) = lookup_other_head_for_show_merge(&repo.git_dir)?;
    let bases = merge_bases_first_vs_rest(repo, head_oid, &[other_oid])
        .context("merge bases for show --merge")?;
    let negative: Vec<String> = bases.iter().map(|b| b.to_hex()).collect();
    Ok((vec!["HEAD".to_string(), other_name], negative, other_oid))
}

fn lookup_other_head_for_show_merge(git_dir: &Path) -> Result<(String, ObjectId)> {
    const NAMES: &[&str] = &[
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "REBASE_HEAD",
    ];
    for name in NAMES {
        let path = git_dir.join(name);
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let line = raw.lines().next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let oid: ObjectId = line
            .parse()
            .map_err(|_| anyhow::anyhow!("{name}: invalid object id"))?;
        return Ok(((*name).to_string(), oid));
    }
    bail!(
        "fatal: --merge requires one of the pseudorefs MERGE_HEAD, CHERRY_PICK_HEAD, REVERT_HEAD or REBASE_HEAD"
    );
}

/// Show a tree as Git does for `git show <treeish>`: `tree <name>` then name-only listing.
fn show_tree_named(
    out: &mut impl Write,
    repo: &Repository,
    display_name: &str,
    tree_oid: ObjectId,
) -> Result<()> {
    let label = if display_name.is_empty() {
        tree_oid.to_hex()
    } else {
        display_name.to_string()
    };
    writeln!(out, "tree {label}")?;
    writeln!(out)?;
    let obj = repo.odb.read(&tree_oid).context("reading tree object")?;
    let entries = parse_tree(&obj.data).context("parsing tree")?;
    let mut names: Vec<String> = entries
        .iter()
        .filter_map(|e| {
            let name = String::from_utf8_lossy(&e.name).into_owned();
            if name.is_empty() {
                None
            } else {
                Some(if e.mode == 0o040000 {
                    format!("{name}/")
                } else {
                    name
                })
            }
        })
        .collect();
    names.sort();
    for n in names {
        writeln!(out, "{n}")?;
    }
    Ok(())
}

fn maybe_warn_deprecated_grafts(repo: &Repository) -> Result<()> {
    let graft_file = repo.git_dir.join("info/grafts");
    let contents = match std::fs::read_to_string(&graft_file) {
        Ok(contents) => contents,
        Err(_) => return Ok(()),
    };
    if contents.lines().all(|line| {
        let trimmed = line.trim();
        trimmed.is_empty() || trimmed.starts_with('#')
    }) {
        return Ok(());
    }

    let config = ConfigSet::load(Some(&repo.git_dir), true)?;
    let show_warning = config
        .get("advice.graftFileDeprecated")
        .map(|raw| {
            !matches!(
                raw.to_ascii_lowercase().as_str(),
                "false" | "no" | "off" | "0"
            )
        })
        .unwrap_or(true);
    if show_warning {
        eprintln!(
            "warning: grafts are deprecated; use 'git replace --convert-graft-file' to migrate."
        );
    }
    Ok(())
}

/// Write `git show --pretty=format:...` output without doubling a trailing newline when the
/// template already ends with one (e.g. `%B` includes a final `\n`).
fn write_formatted_line(out: &mut impl Write, formatted: &str) -> Result<()> {
    if formatted.ends_with('\n') {
        write!(out, "{formatted}")?;
    } else {
        writeln!(out, "{formatted}")?;
    }
    Ok(())
}

enum ShowOutputEncoding {
    Utf8,
    Reencode(String),
    Raw,
}

fn resolve_show_output_encoding(config: &ConfigSet, args: &Args) -> ShowOutputEncoding {
    let explicit = args
        .encoding
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if explicit.is_some_and(|label| label.eq_ignore_ascii_case("none")) {
        return ShowOutputEncoding::Raw;
    }
    let label = explicit
        .map(str::to_owned)
        .or_else(|| {
            config
                .get("i18n.logOutputEncoding")
                .or_else(|| config.get("i18n.logoutputencoding"))
        })
        .or_else(|| {
            config
                .get("i18n.commitEncoding")
                .or_else(|| config.get("i18n.commitencoding"))
        });
    match label {
        Some(label)
            if !(label.eq_ignore_ascii_case("utf-8") || label.eq_ignore_ascii_case("utf8")) =>
        {
            ShowOutputEncoding::Reencode(label)
        }
        _ => ShowOutputEncoding::Utf8,
    }
}

fn write_medium_message_lines(
    out: &mut impl Write,
    commit: &grit_lib::objects::CommitData,
    expand_tabs_in_log: usize,
    output_encoding: &ShowOutputEncoding,
) -> Result<()> {
    if matches!(output_encoding, ShowOutputEncoding::Raw) {
        let raw = commit
            .raw_message
            .as_deref()
            .unwrap_or_else(|| commit.message.as_bytes());
        return write_indented_raw_message(out, raw);
    }

    for line in commit.message.lines() {
        let line = grit_lib::tab_expand::indent_and_expand_tabs(line, 4, expand_tabs_in_log);
        match output_encoding {
            ShowOutputEncoding::Utf8 | ShowOutputEncoding::Raw => {
                writeln!(out, "{line}")?;
            }
            ShowOutputEncoding::Reencode(label) => {
                out.write_all(b"    ")?;
                let unindented = line.strip_prefix("    ").unwrap_or(&line);
                let bytes = grit_lib::commit_encoding::encode_header_text(label, unindented)
                    .unwrap_or_else(|| unindented.as_bytes().to_vec());
                out.write_all(&bytes)?;
                out.write_all(b"\n")?;
            }
        }
    }
    Ok(())
}

fn write_indented_raw_message(out: &mut impl Write, raw: &[u8]) -> Result<()> {
    let mut lines = raw.split(|b| *b == b'\n').peekable();
    while let Some(line) = lines.next() {
        if line.is_empty() && lines.peek().is_none() {
            break;
        }
        out.write_all(b"    ")?;
        out.write_all(line)?;
        out.write_all(b"\n")?;
    }
    Ok(())
}

/// Git porcelain (`show`, `log -p`) splits a blob↔symlink type change into a delete hunk plus an add
/// hunk so textconv applies only to the deleted regular file (t4030-diff-textconv).
fn expand_typechange_entries_for_porcelain(entries: Vec<DiffEntry>) -> Vec<DiffEntry> {
    let mut out = Vec::with_capacity(entries.len() + 4);
    for e in entries {
        if e.status == DiffStatus::TypeChanged {
            // Byte-true: clone the original path bytes for both synthetic halves.
            let path = e.path().to_owned();
            out.push(DiffEntry {
                status: DiffStatus::Deleted,
                old_path: Some(path.clone()),
                new_path: None,
                old_mode: e.old_mode.clone(),
                new_mode: "000000".to_owned(),
                old_oid: e.old_oid,
                new_oid: zero_oid(),
                score: None,
            });
            out.push(DiffEntry {
                status: DiffStatus::Added,
                old_path: None,
                new_path: Some(path),
                old_mode: "000000".to_owned(),
                new_mode: e.new_mode.clone(),
                old_oid: zero_oid(),
                new_oid: e.new_oid,
                score: None,
            });
        } else {
            out.push(e);
        }
    }
    out
}

/// Emit `git show -m` for a merge commit: one full medium-format entry per parent, each header
/// tagged `(from <parent>)` and followed by that parent's diff (matches `git log -m -p`).
#[allow(clippy::too_many_arguments)]
fn show_commit_separate_merge(
    out: &mut impl Write,
    repo: &Repository,
    oid: &ObjectId,
    commit: &grit_lib::objects::CommitData,
    args: &Args,
    config: &ConfigSet,
    expand_tabs_in_log: usize,
    _indent_heuristic: bool,
    signature_lines: Option<&str>,
) -> Result<()> {
    let odb = &repo.odb;
    let git_dir = &repo.git_dir;
    let hex = oid.to_hex();
    let abbrev_len = if args.no_abbrev {
        40usize
    } else {
        args.abbrev
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(7)
    };
    let context = args.unified.unwrap_or(3);
    let use_textconv = !args.no_textconv;
    let merge_abbrevs: Vec<String> = commit
        .parents
        .iter()
        .map(|p| p.to_hex()[..7].to_string())
        .collect();

    let mut shown = 0usize;
    for parent_oid in &commit.parents {
        let parent_obj = odb.read(parent_oid).context("reading merge parent")?;
        let parent_commit = parse_commit(&parent_obj.data).context("parsing merge parent")?;
        let entries = diff_trees(odb, Some(&parent_commit.tree), Some(&commit.tree), "")
            .context("computing merge parent diff")?;
        if entries.is_empty() {
            continue;
        }
        if shown > 0 {
            writeln!(out)?;
        }
        shown += 1;

        // Medium header, repeated for each parent with the `(from <parent>)` annotation.
        writeln!(out, "commit {hex} (from {})", parent_oid.to_hex())?;
        writeln!(out, "Merge: {}", merge_abbrevs.join(" "))?;
        if let Some(sig) = signature_lines {
            out.write_all(sig.as_bytes())?;
        }
        writeln!(out, "Author: {}", format_ident_display(&commit.author))?;
        writeln!(out, "Date:   {}", format_date(&commit.author))?;
        writeln!(out)?;
        for line in commit.message.lines() {
            writeln!(
                out,
                "{}",
                grit_lib::tab_expand::indent_and_expand_tabs(line, 4, expand_tabs_in_log)
            )?;
        }
        writeln!(out)?;

        for entry in &entries {
            if let Some(patch) = format_parent_patch(
                git_dir,
                config,
                odb,
                &entry.path().to_string_lossy(),
                &parent_commit.tree,
                &commit.tree,
                abbrev_len,
                context,
                use_textconv,
            ) {
                write!(out, "{patch}")?;
            }
        }
    }
    Ok(())
}

/// Show a commit object: header + diff.
fn show_commit(
    out: &mut impl Write,
    repo: &Repository,
    oid: &ObjectId,
    data: &[u8],
    args: &Args,
    notes_map: &HashMap<ObjectId, Vec<u8>>,
    pathspecs: &[String],
    remerge_emit_opts: Option<&crate::commands::remerge_diff::RemergeDiffOptions<'_>>,
    merge_from_parent: Option<ObjectId>,
    indent_heuristic: bool,
    want_root: bool,
) -> Result<()> {
    let odb = &repo.odb;
    let commit = parse_commit(data).context("parsing commit")?;
    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let output_encoding = resolve_show_output_encoding(&config, args);
    let hex = oid.to_hex();

    // `--show-signature` (or `log.showSignature`) emits the GPG verification
    // lines immediately after the `commit <hash>` header line.
    let want_signature = if args.no_show_signature {
        false
    } else {
        args.show_signature || matches!(config.get_bool("log.showsignature"), Some(Ok(true)))
    };
    let signature_lines: Option<String> = if want_signature {
        Some(crate::commands::log::format_commit_signature_lines(
            &config, data,
        ))
    } else {
        None
    };

    let mut resolved_format = args.format.clone();
    if let Some(ref f) = resolved_format {
        let r = crate::commands::log::resolve_pretty_alias_with_config(f, repo);
        if r != *f {
            resolved_format = Some(r);
        }
    }
    let expand_tabs_parsed = match &args.expand_tabs {
        None => None,
        Some(s) => Some(
            s.parse::<usize>()
                .map_err(|_| anyhow::anyhow!("'{s}': not a non-negative integer"))?,
        ),
    };
    let expand_tabs_in_log = grit_lib::tab_expand::resolve_expand_tabs_in_log(
        args.no_expand_tabs,
        expand_tabs_parsed,
        resolved_format.as_deref(),
        args.oneline,
    );

    if args.oneline || resolved_format.as_deref() == Some("oneline") {
        // Mirror `git log --oneline`, which auto-decorates the header line with the refs pointing
        // at the commit only when writing to a terminal; piped/redirected output is undecorated.
        let decoration = if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
            crate::commands::log::oneline_decoration_for_hex(repo, &hex)
        } else {
            String::new()
        };
        let first_line = grit_lib::commit_pretty::message_subject(&commit.message);
        let first_line = if expand_tabs_in_log > 0 {
            grit_lib::tab_expand::expand_tabs_in_line(&first_line, expand_tabs_in_log)
        } else {
            first_line
        };
        if args.remerge_diff && !(args.quiet || args.no_patch) && commit.parents.len() == 2 {
            use crate::commands::remerge_diff::{write_remerge_diff, RemergeDiffOptions};
            let mut remerge_buf = Vec::new();
            match remerge_emit_opts {
                Some(o) => {
                    write_remerge_diff(&mut remerge_buf, repo, &commit.tree, &commit.parents, o)?
                }
                None => {
                    let find_oid = if let Some(ref s) = args.find_object {
                        Some(
                            resolve_revision(repo, s)
                                .with_context(|| format!("unknown revision: '{s}'"))?,
                        )
                    } else {
                        None
                    };
                    let o = RemergeDiffOptions {
                        pathspecs,
                        diff_filter: args.diff_filter.as_deref(),
                        pickaxe: args.pickaxe.as_deref(),
                        find_object: find_oid,
                        submodule_mode: args.submodule.as_deref(),
                        context_lines: args.unified.unwrap_or(3),
                        indent_heuristic,
                    };
                    write_remerge_diff(&mut remerge_buf, repo, &commit.tree, &commit.parents, &o)?;
                }
            }
            let suppress_commit_line = remerge_buf.is_empty()
                && (args.diff_filter.is_some()
                    || args.pickaxe.is_some()
                    || args.find_object.is_some());
            if suppress_commit_line {
                return Ok(());
            }
            writeln!(out, "{}{} {}", &hex[..7], decoration, first_line)?;
            out.write_all(&remerge_buf)?;
            // Pathspecs limit remerge-diff only; do not also emit the default parent diff.
            if !pathspecs.is_empty() {
                return Ok(());
            }
            return Ok(());
        }
        writeln!(out, "{}{} {}", &hex[..7], decoration, first_line)?;
        return Ok(());
    }

    // A root commit with no diff to show (no `--root` and `log.showroot=false`) prints only
    // the header/message — without the trailing blank that normally separates message and diff.
    let root_diff_shown = !args.quiet
        && !args.no_patch
        && (!commit.parents.is_empty()
            || want_root
            || config
                .get_bool("log.showroot")
                .and_then(|r| r.ok())
                .unwrap_or(true));

    // Git's `log-tree.c` rule: when a verbose header is followed by BOTH a diffstat
    // and a patch (`--patch-with-stat`), it emits a `---` line (no extra blank)
    // between the message and the stat, instead of the usual blank line.
    let header_stat_patch_dashes = {
        let will_show_raw = args.patch_with_raw || (args.raw && !args.numstat);
        let will_show_stat =
            args.patch_with_stat || (!args.stat.is_empty() && !args.numstat && !will_show_raw);
        let will_show_patch = !args.quiet
            && !args.no_patch
            && (args.patch
                || args.binary
                || args.patch_with_raw
                || args.patch_with_stat
                || (!args.raw
                    && args.stat.is_empty()
                    && !args.shortstat
                    && !args.summary
                    && !args.numstat
                    && !args.name_only
                    && !args.name_status));
        will_show_stat && will_show_patch && !will_show_raw && !args.numstat
    };

    // `git show -m` on a merge: emit one full entry per parent (medium header repeated with
    // `(from <parent>)`, followed by that parent's diff), matching `git log -m -p`.
    let medium_format = matches!(resolved_format.as_deref(), None | Some("medium"));
    let separate_merge_patch = commit.parents.len() > 1
        && args.diff_merges
        && !args.combined
        && !args.combined_cc
        && !args.first_parent
        && !args.remerge_diff
        && medium_format
        && !args.quiet
        && !args.no_patch
        && !args.name_only
        && !args.name_status
        && !args.raw
        && !args.numstat
        && args.stat.is_empty()
        && !args.shortstat
        && !args.summary
        && !args.patch_with_stat
        && !args.patch_with_raw;
    if separate_merge_patch {
        return show_commit_separate_merge(
            out,
            repo,
            oid,
            &commit,
            args,
            &config,
            expand_tabs_in_log,
            indent_heuristic,
            signature_lines.as_deref(),
        );
    }

    let format = resolved_format.as_deref();
    // User `--format=<string>` (incl. `format:` / `tformat:`) emits no built-in trailing blank;
    // git inserts one blank line between that header and the following diff body.
    let user_format_header = matches!(
        format,
        Some(f) if f.starts_with("format:")
            || f.starts_with("tformat:")
            || !matches!(f, "medium" | "short" | "full" | "fuller" | "reference" | "oneline" | "raw" | "email")
    );
    match format {
        Some(fmt) if fmt.starts_with("format:") || fmt.starts_with("tformat:") => {
            let _template = fmt
                .strip_prefix("format:")
                .or_else(|| fmt.strip_prefix("tformat:"))
                .unwrap_or(fmt);

            let is_tformat = fmt.starts_with("tformat:");
            let template = if let Some(t) = fmt.strip_prefix("format:") {
                t
            } else {
                &fmt[8..]
            };
            let note_bytes = notes_map.get(oid).map(|v| v.as_slice());
            let formatted =
                apply_format_string(template, oid, &commit, note_bytes, expand_tabs_in_log);
            // `--pretty=format:` separates entries with a newline but does NOT terminate the
            // last one, so an empty expansion (e.g. `%b` for a body-less commit with `-s`)
            // prints nothing — unlike `tformat:`, which always appends a terminator newline
            // (`git show -s --pretty=format:%b` is empty; `tformat:%b` is a lone newline).
            if !is_tformat && formatted.is_empty() {
                // emit nothing
            } else {
                write_formatted_line(out, &formatted)?;
            }
        }
        Some("short") => {
            writeln!(out, "commit {hex}")?;
            if let Some(sig) = &signature_lines {
                out.write_all(sig.as_bytes())?;
            }
            writeln!(out, "Author: {}", format_ident_display(&commit.author))?;
            writeln!(out)?;
            for line in commit.message.lines().take(1) {
                writeln!(
                    out,
                    "{}",
                    grit_lib::tab_expand::indent_and_expand_tabs(line, 4, expand_tabs_in_log)
                )?;
            }
            writeln!(out)?;
        }
        Some("full") => {
            writeln!(out, "commit {hex}")?;
            writeln!(out, "Author: {}", format_ident_display(&commit.author))?;
            writeln!(out, "Commit: {}", format_ident_display(&commit.committer))?;
            writeln!(out)?;
            for line in commit.message.lines() {
                writeln!(
                    out,
                    "{}",
                    grit_lib::tab_expand::indent_and_expand_tabs(line, 4, expand_tabs_in_log)
                )?;
            }
            // The blank line separating the message from the diff is only emitted when a diff
            // follows (Git's `log-tree.c`). Under `-s`/`--no-patch` the entry ends at the message,
            // matching `git log --pretty=full` (one trailing newline).
            if root_diff_shown {
                writeln!(out)?;
            }
        }
        Some("fuller") => {
            writeln!(out, "commit {hex}")?;
            writeln!(out, "Author:     {}", format_ident_display(&commit.author))?;
            writeln!(out, "AuthorDate: {}", format_date(&commit.author))?;
            writeln!(
                out,
                "Commit:     {}",
                format_ident_display(&commit.committer)
            )?;
            writeln!(out, "CommitDate: {}", format_date(&commit.committer))?;
            writeln!(out)?;
            for line in commit.message.lines() {
                writeln!(
                    out,
                    "{}",
                    grit_lib::tab_expand::indent_and_expand_tabs(line, 4, expand_tabs_in_log)
                )?;
            }
            if root_diff_shown {
                writeln!(out)?;
            }
        }
        Some("reference") => {
            let subject = grit_lib::commit_pretty::message_subject(&commit.message);
            let line =
                grit_lib::commit_pretty::format_reference_line(oid, &subject, &commit.committer, 7);
            writeln!(out, "{line}")?;
        }
        Some("medium") | None => {
            // Medium format (default)
            writeln!(out, "commit {hex}")?;
            if commit.parents.len() > 1 {
                let abbrevs: Vec<String> = commit
                    .parents
                    .iter()
                    .map(|p| p.to_hex()[..7].to_string())
                    .collect();
                writeln!(out, "Merge: {}", abbrevs.join(" "))?;
            }
            if let Some(sig) = &signature_lines {
                out.write_all(sig.as_bytes())?;
            }
            writeln!(out, "Author: {}", format_ident_display(&commit.author))?;
            writeln!(out, "Date:   {}", format_date(&commit.author))?;
            writeln!(out)?;
            write_medium_message_lines(out, &commit, expand_tabs_in_log, &output_encoding)?;
            if show_notes_display_enabled() {
                if let Some(note_data) = notes_map.get(oid) {
                    let note_text = String::from_utf8_lossy(note_data);
                    writeln!(out)?;
                    writeln!(out, "Notes:")?;
                    for line in note_text.lines() {
                        writeln!(out, "    {line}")?;
                    }
                } else if root_diff_shown {
                    if header_stat_patch_dashes {
                        writeln!(out, "---")?;
                    } else {
                        writeln!(out)?;
                    }
                }
            } else if root_diff_shown {
                if header_stat_patch_dashes {
                    writeln!(out, "---")?;
                } else {
                    writeln!(out)?;
                }
            }
        }
        Some("email") => {
            writeln!(out, "From {} Mon Sep 17 00:00:00 2001", hex)?;
            writeln!(out, "From: {}", format_ident_display(&commit.author))?;
            writeln!(out, "Date: {}", format_date(&commit.author))?;
            let subject = grit_lib::commit_pretty::message_subject(&commit.message);
            let subject = if expand_tabs_in_log > 0 {
                grit_lib::tab_expand::expand_tabs_in_line(&subject, expand_tabs_in_log)
            } else {
                subject
            };
            writeln!(out, "Subject: [PATCH] {}", subject)?;
            writeln!(out)?;
            for line in commit.message.lines() {
                let line_out = if expand_tabs_in_log > 0 {
                    grit_lib::tab_expand::expand_tabs_in_line(line, expand_tabs_in_log)
                } else {
                    line.to_owned()
                };
                writeln!(out, "{line_out}")?;
            }
            writeln!(out)?;
        }
        Some("raw") => {
            writeln!(out, "commit {hex}")?;
            writeln!(out, "tree {}", commit.tree.to_hex())?;
            for parent in &commit.parents {
                writeln!(out, "parent {}", parent.to_hex())?;
            }
            writeln!(out, "author {}", commit.author)?;
            writeln!(out, "committer {}", commit.committer)?;
            writeln!(out)?;
            for line in commit.message.lines() {
                writeln!(
                    out,
                    "{}",
                    grit_lib::tab_expand::indent_and_expand_tabs(line, 4, expand_tabs_in_log)
                )?;
            }
            if root_diff_shown {
                writeln!(out)?;
            }
        }
        Some(other) if other.starts_with("format:") || other.starts_with("tformat:") => {
            // Already handled above — unreachable
        }
        Some(other) => {
            let note_bytes = notes_map.get(oid).map(|v| v.as_slice());
            let formatted =
                apply_format_string(other, oid, &commit, note_bytes, expand_tabs_in_log);
            writeln!(out, "{formatted}")?;
        }
    }

    if args.quiet || args.no_patch {
        return Ok(());
    }

    if args.remerge_diff && commit.parents.len() == 2 {
        use crate::commands::remerge_diff::{write_remerge_diff, RemergeDiffOptions};
        match remerge_emit_opts {
            Some(o) => write_remerge_diff(out, repo, &commit.tree, &commit.parents, o)?,
            None => {
                let find_oid = if let Some(ref s) = args.find_object {
                    Some(
                        resolve_revision(repo, s)
                            .with_context(|| format!("unknown revision: '{s}'"))?,
                    )
                } else {
                    None
                };
                let o = RemergeDiffOptions {
                    pathspecs,
                    diff_filter: args.diff_filter.as_deref(),
                    pickaxe: args.pickaxe.as_deref(),
                    find_object: find_oid,
                    submodule_mode: args.submodule.as_deref(),
                    context_lines: args.unified.unwrap_or(3),
                    indent_heuristic,
                };
                write_remerge_diff(out, repo, &commit.tree, &commit.parents, &o)?;
            }
        }
        return Ok(());
    }

    let abbrev_len = if args.no_abbrev {
        40usize
    } else {
        args.abbrev
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(7)
    };

    let context = args.unified.unwrap_or(3);

    let (_old_tree_oid, diff_entries) = if let Some(merge_parent) = merge_from_parent {
        if commit.parents.contains(&merge_parent) {
            let idx = repo
                .load_index()
                .context("failed to read index for show --merge")?;
            let mut unmerged: BTreeSet<String> = BTreeSet::new();
            for e in &idx.entries {
                if e.stage() != 0 {
                    if let Ok(p) = String::from_utf8(e.path.clone()) {
                        unmerged.insert(p);
                    }
                }
            }
            let parent_obj = odb.read(&merge_parent).context("reading merge parent")?;
            let parent_commit = parse_commit(&parent_obj.data).context("parsing merge parent")?;
            let old_t = parent_commit.tree;
            let mut entries =
                diff_trees(odb, Some(&old_t), Some(&commit.tree), "").context("computing diff")?;
            entries.retain(|e| unmerged.contains(&*e.path().to_string_lossy()));
            (Some(old_t), entries)
        } else {
            let new_tree = Some(&commit.tree);
            let old_tree = commit.parents.first().map(|parent_oid| {
                odb.read(parent_oid)
                    .ok()
                    .and_then(|obj| parse_commit(&obj.data).ok())
                    .map(|c| c.tree)
            });
            let old_tree_oid = old_tree.flatten();
            let raw_entries =
                diff_trees(odb, old_tree_oid.as_ref(), new_tree, "").context("computing diff")?;
            if args.find_renames.is_some() || !args.find_copies.is_empty() {
                prefetch_promisor_for_diff_entries(
                    repo,
                    &raw_entries,
                    None,
                    PromisorDiffPrefetch {
                        rename_detection: true,
                        break_rewrites: false,
                        needs_blob_content: false,
                    },
                );
            }
            let diff_entries =
                apply_rename_copy_detection(odb, raw_entries, args, old_tree_oid.as_ref());
            (
                old_tree_oid,
                expand_typechange_entries_for_porcelain(diff_entries),
            )
        }
    } else {
        let new_tree = Some(&commit.tree);
        let old_tree = commit.parents.first().map(|parent_oid| {
            odb.read(parent_oid)
                .ok()
                .and_then(|obj| parse_commit(&obj.data).ok())
                .map(|c| c.tree)
        });
        let old_tree_oid = old_tree.flatten();
        let raw_entries =
            diff_trees(odb, old_tree_oid.as_ref(), new_tree, "").context("computing diff")?;
        if args.find_renames.is_some() || !args.find_copies.is_empty() {
            prefetch_promisor_for_diff_entries(
                repo,
                &raw_entries,
                None,
                PromisorDiffPrefetch {
                    rename_detection: true,
                    break_rewrites: false,
                    needs_blob_content: false,
                },
            );
        }
        let diff_entries =
            apply_rename_copy_detection(odb, raw_entries, args, old_tree_oid.as_ref());
        (
            old_tree_oid,
            expand_typechange_entries_for_porcelain(diff_entries),
        )
    };

    // A root commit's diff against the empty tree is shown only when `--root` is given or
    // `log.showroot` is true (git default). When `log.showroot=false` and `--root` is absent,
    // `git show <root>` prints just the header/message.
    let show_root = want_root
        || config
            .get_bool("log.showroot")
            .and_then(|r| r.ok())
            .unwrap_or(true);
    let diff_entries = if commit.parents.is_empty() && !show_root {
        Vec::new()
    } else {
        diff_entries
    };

    // Limit the displayed diff to the given pathspecs (`git show <rev> -- <path>...`).
    let diff_entries = if pathspecs.is_empty() {
        diff_entries
    } else {
        diff_entries
            .into_iter()
            .filter(|e| {
                let new_p = e.new_path.as_deref().unwrap_or(RepoPath::from_str(""));
                let old_p = e.old_path.as_deref().unwrap_or(RepoPath::from_str(""));
                // TODO(byte-paths): pathspec matching is &str-based until Phase 3.
                (!new_p.is_empty()
                    && grit_lib::pathspec::path_allowed_by_pathspec_list(
                        pathspecs,
                        &new_p.to_string_lossy(),
                    ))
                    || (!old_p.is_empty()
                        && grit_lib::pathspec::path_allowed_by_pathspec_list(
                            pathspecs,
                            &old_p.to_string_lossy(),
                        ))
            })
            .collect()
    };

    let is_merge = commit.parents.len() > 1;
    // `--first-parent` forces a first-parent (single) diff for merges, suppressing the
    // default dense-combined merge diff (git's `diff_merges_default_to_first_parent`).
    let default_merge_patch =
        is_merge && !args.diff_merges && !args.combined && !args.combined_cc && !args.first_parent;
    let use_combined_format =
        (args.combined || args.combined_cc || default_merge_patch) && !args.first_parent;
    let combined_use_cc_word = args.combined_cc || default_merge_patch;

    // Separate a user `--format` header from the following name-only / name-status list with a
    // blank line (git's behavior). Patch/stat sections emit their own leading separator below.
    if user_format_header && (args.name_only || args.name_status) && !diff_entries.is_empty() {
        writeln!(out)?;
    }

    // --name-only: just print file names
    if args.name_only {
        for entry in &diff_entries {
            let path = entry
                .new_path
                .as_deref()
                .or(entry.old_path.as_deref())
                .unwrap_or(RepoPath::from_str(""));
            writeln!(out, "{path}")?;
        }
        return Ok(());
    }

    // --name-status: print status letter and file name
    if args.name_status {
        for entry in &diff_entries {
            let path = entry
                .new_path
                .as_deref()
                .or(entry.old_path.as_deref())
                .unwrap_or(RepoPath::from_str(""));
            let status = match entry.status {
                grit_lib::diff::DiffStatus::Added => 'A',
                grit_lib::diff::DiffStatus::Deleted => 'D',
                grit_lib::diff::DiffStatus::Modified => 'M',
                grit_lib::diff::DiffStatus::Renamed => 'R',
                grit_lib::diff::DiffStatus::Copied => 'C',
                grit_lib::diff::DiffStatus::TypeChanged => 'T',
                grit_lib::diff::DiffStatus::Unmerged => 'U',
            };
            writeln!(out, "{status}\t{path}")?;
        }
        return Ok(());
    }

    // Determine what sections to show. Summary formats suppress the default
    // patch unless an option explicitly re-enables it.
    let show_raw = args.patch_with_raw || (args.raw && !args.numstat);
    let show_numstat = args.numstat;
    let show_stat = args.patch_with_stat || (!args.stat.is_empty() && !show_numstat && !show_raw);
    let show_patch = !args.quiet
        && !args.no_patch
        && (args.patch
            || args.binary
            || args.patch_with_raw
            || args.patch_with_stat
            || (!args.raw
                && args.stat.is_empty()
                && !args.shortstat
                && !args.summary
                && !args.numstat
                && !args.name_only
                && !args.name_status));

    prefetch_promisor_for_diff_entries(
        repo,
        &diff_entries,
        None,
        PromisorDiffPrefetch {
            rename_detection: false,
            break_rewrites: false,
            needs_blob_content: show_patch || show_numstat || show_stat || args.shortstat,
        },
    );

    // --raw: raw diff-tree output format
    if show_raw {
        for entry in &diff_entries {
            let old_path = entry
                .old_path
                .as_deref()
                .or(entry.new_path.as_deref())
                .unwrap_or(RepoPath::from_str(""));
            let new_path = entry
                .new_path
                .as_deref()
                .or(entry.old_path.as_deref())
                .unwrap_or(RepoPath::from_str(""));
            let status_char = match entry.status {
                grit_lib::diff::DiffStatus::Added => 'A',
                grit_lib::diff::DiffStatus::Deleted => 'D',
                grit_lib::diff::DiffStatus::Modified => 'M',
                grit_lib::diff::DiffStatus::Renamed => 'R',
                grit_lib::diff::DiffStatus::Copied => 'C',
                grit_lib::diff::DiffStatus::TypeChanged => 'T',
                grit_lib::diff::DiffStatus::Unmerged => 'U',
            };
            let status_str = match entry.status {
                grit_lib::diff::DiffStatus::Renamed | grit_lib::diff::DiffStatus::Copied => {
                    let score = entry.score.unwrap_or(0);
                    format!("{status_char}{score:03}")
                }
                _ => format!("{status_char}"),
            };
            let paths = match entry.status {
                grit_lib::diff::DiffStatus::Renamed | grit_lib::diff::DiffStatus::Copied => {
                    format!("{old_path}\t{new_path}")
                }
                _ => new_path.to_string(),
            };
            // Abbreviated OIDs get a trailing `...` when GIT_PRINT_SHA1_ELLIPSIS=yes.
            let ellipsis =
                if std::env::var("GIT_PRINT_SHA1_ELLIPSIS").ok().as_deref() == Some("yes") {
                    "..."
                } else {
                    ""
                };
            writeln!(
                out,
                ":{} {} {}{ellipsis} {}{ellipsis} {status_str}\t{paths}",
                entry.old_mode,
                entry.new_mode,
                &entry.old_oid.to_hex()[..7],
                &entry.new_oid.to_hex()[..7],
            )?;
        }
    }

    // --numstat
    if show_numstat {
        for entry in &diff_entries {
            write_numstat_line(out, odb, entry)?;
        }
    }

    // Blank line separator before patch when raw or numstat was shown
    if (show_raw || show_numstat) && show_patch {
        writeln!(out)?;
    }

    // --stat: show diffstat summary
    if show_stat && !show_raw && !show_numstat {
        write_diffstat(
            out,
            odb,
            &diff_entries,
            args.stat_width,
            args.stat_name_width,
            args.stat_graph_width,
            args.stat_count,
            &config,
        )?;
        // `--summary` emits create/delete mode, mode-change, and rename/copy lines after the
        // diffstat. `--patch-with-stat` alone does NOT imply `--summary`.
        if args.summary {
            write_show_summary_lines(out, &diff_entries)?;
        }
        if !show_patch {
            return Ok(());
        }
        // A blank line separates the stat/summary block from the following patch.
        writeln!(out)?;
    }

    if !show_patch {
        return Ok(());
    }

    let use_textconv = !args.no_textconv;
    let git_dir = &repo.git_dir;

    if is_merge && (args.diff_merges || use_combined_format) {
        if args.format.as_deref() == Some("%s") {
            writeln!(out)?;
        }
        let parent_trees: Vec<ObjectId> = commit
            .parents
            .iter()
            .filter_map(|p| {
                odb.read(p)
                    .ok()
                    .and_then(|obj| parse_commit(&obj.data).ok())
                    .map(|c| c.tree)
            })
            .collect();

        if args.diff_merges {
            let subject_isolated = args.format.as_deref() == Some("%s");
            let subject = grit_lib::commit_pretty::message_subject(&commit.message);
            for (pi, ptree) in parent_trees.iter().enumerate() {
                for entry in &diff_entries {
                    if let Some(patch) = format_parent_patch(
                        git_dir,
                        &config,
                        odb,
                        &entry.path().to_string_lossy(),
                        ptree,
                        &commit.tree,
                        abbrev_len,
                        context,
                        use_textconv,
                    ) {
                        write!(out, "{patch}")?;
                    }
                }
                if subject_isolated && pi + 1 < parent_trees.len() {
                    writeln!(out, "{subject}")?;
                    writeln!(out)?;
                }
            }
            return Ok(());
        }

        if use_combined_format && parent_trees.len() >= 2 {
            let walk = CombinedTreeDiffOptions {
                recursive: true,
                tree_in_recursive: false,
            };
            let paths =
                combined_diff_paths_filtered(odb, &commit.tree, &commit.parents, &walk, None)
                    .unwrap_or_default();
            let quote_fully = config.quote_path_fully();
            let ws = CombinedDiffWsOptions {
                ignore_all_space: args.ignore_all_space,
                ignore_space_change: args.ignore_space_change,
                ignore_space_at_eol: args.ignore_space_at_eol,
                ignore_cr_at_eol: args.ignore_cr_at_eol,
            };
            for p in paths {
                let mut any_blob = false;
                let mut binary = false;
                for (i, _side) in p.parents.iter().enumerate() {
                    if let Some(b) = read_blob_at_path(odb, &parent_trees[i], &p.path) {
                        any_blob = true;
                        if is_binary_for_diff(git_dir, &p.path, &b) {
                            binary = true;
                        }
                    }
                }
                if let Some(nr) = read_blob_at_path(odb, &commit.tree, &p.path) {
                    any_blob = true;
                    if is_binary_for_diff(git_dir, &p.path, &nr) {
                        binary = true;
                    }
                }
                if !any_blob {
                    continue;
                }
                if binary {
                    let mut po = Vec::with_capacity(p.parents.len());
                    for (i, _side) in p.parents.iter().enumerate() {
                        po.push(
                            blob_oid_at_path(odb, &parent_trees[i], &p.path)
                                .unwrap_or_else(zero_oid),
                        );
                    }
                    let roid =
                        blob_oid_at_path(odb, &commit.tree, &p.path).unwrap_or_else(zero_oid);
                    write!(
                        out,
                        "{}",
                        format_combined_binary(
                            &p.path,
                            &po,
                            &roid,
                            abbrev_len,
                            combined_use_cc_word
                        )
                    )?;
                } else if let Some(patch) = format_combined_textconv_patch(
                    git_dir,
                    &config,
                    odb,
                    &p.path,
                    &parent_trees,
                    &commit.tree,
                    abbrev_len,
                    context,
                    combined_use_cc_word,
                    use_textconv,
                    ws,
                    false,
                    None,
                    &p.parents,
                    quote_fully,
                ) {
                    write!(out, "{patch}")?;
                }
            }
            return Ok(());
        }
    }

    let quote_path_fully = config.quote_path_fully();

    // Default: full unified diff (first parent or root)
    for entry in &diff_entries {
        // TODO(byte-paths): patch headers are &str-based until quote_path/Phase 3.
        let old_path_buf = entry
            .old_path
            .as_deref()
            .map_or_else(|| "/dev/null".to_string(), |p| p.to_string_lossy().into_owned());
        let new_path_buf = entry
            .new_path
            .as_deref()
            .map_or_else(|| "/dev/null".to_string(), |p| p.to_string_lossy().into_owned());
        let old_path: &str = &old_path_buf;
        let new_path: &str = &new_path_buf;

        // Print the diff header
        write_diff_header(out, entry)?;

        // Skip diff content for rename/copy with 100% similarity
        if (entry.status == grit_lib::diff::DiffStatus::Renamed
            || entry.status == grit_lib::diff::DiffStatus::Copied)
            && entry.old_oid == entry.new_oid
        {
            continue;
        }

        let old_raw = if entry.old_oid == grit_lib::diff::zero_oid() {
            Vec::new()
        } else {
            odb.read(&entry.old_oid)
                .map(|obj| obj.data)
                .unwrap_or_default()
        };
        let new_raw = if entry.new_oid == grit_lib::diff::zero_oid() {
            Vec::new()
        } else {
            odb.read(&entry.new_oid)
                .map(|obj| obj.data)
                .unwrap_or_default()
        };

        let path_for_attrs_buf = entry.path().to_string_lossy();
        let path_for_attrs: &str = &path_for_attrs_buf;
        let textconv_patch = use_textconv && diff_textconv_active(git_dir, &config, path_for_attrs);
        if !textconv_patch
            && (is_binary_for_diff(git_dir, path_for_attrs, &old_raw)
                || is_binary_for_diff(git_dir, path_for_attrs, &new_raw))
        {
            writeln!(out, "Binary files a/{new_path} and b/{new_path} differ")?;
            continue;
        }

        let old_content = if entry.old_oid == grit_lib::diff::zero_oid() {
            String::new()
        } else if use_textconv {
            blob_text_for_diff_with_oid(
                odb,
                git_dir,
                &config,
                path_for_attrs,
                &old_raw,
                &entry.old_oid,
                true,
            )
        } else {
            blob_text_for_diff(git_dir, &config, path_for_attrs, &old_raw, false)
        };
        let new_content = if entry.new_oid == grit_lib::diff::zero_oid() {
            String::new()
        } else if use_textconv {
            blob_text_for_diff_with_oid(
                odb,
                git_dir,
                &config,
                path_for_attrs,
                &new_raw,
                &entry.new_oid,
                true,
            )
        } else {
            blob_text_for_diff(git_dir, &config, path_for_attrs, &new_raw, false)
        };

        let patch = if !args.anchored.is_empty() {
            let line_algo = if args.patience {
                similar::Algorithm::Patience
            } else {
                similar::Algorithm::Myers
            };
            anchored_unified_diff(
                &old_content,
                &new_content,
                old_path,
                new_path,
                context,
                &args.anchored,
                line_algo,
                false,
                indent_heuristic,
                quote_path_fully,
            )
        } else {
            unified_diff(
                &old_content,
                &new_content,
                old_path,
                new_path,
                context,
                indent_heuristic,
                quote_path_fully,
            )
        };
        write!(out, "{patch}")?;
    }

    Ok(())
}

/// Write git's `--summary` lines (create/delete mode, mode change, rename/copy) for the
/// given diff entries.
fn write_show_summary_lines(
    out: &mut impl Write,
    entries: &[grit_lib::diff::DiffEntry],
) -> Result<()> {
    use grit_lib::diff::DiffStatus;
    for entry in entries {
        match entry.status {
            DiffStatus::Added => {
                writeln!(out, " create mode {} {}", entry.new_mode, entry.path())?;
            }
            DiffStatus::Deleted => {
                writeln!(out, " delete mode {} {}", entry.old_mode, entry.path())?;
            }
            DiffStatus::Modified | DiffStatus::TypeChanged if entry.old_mode != entry.new_mode => {
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

/// Write a diffstat summary for the given diff entries.
/// Write a single numstat line for an entry.
fn write_numstat_line(
    out: &mut impl Write,
    odb: &Odb,
    entry: &grit_lib::diff::DiffEntry,
) -> Result<()> {
    let old_content = if entry.old_oid == grit_lib::diff::zero_oid() {
        String::new()
    } else {
        odb.read(&entry.old_oid)
            .map(|o| String::from_utf8_lossy(&o.data).into_owned())
            .unwrap_or_default()
    };
    let new_content = if entry.new_oid == grit_lib::diff::zero_oid() {
        String::new()
    } else {
        odb.read(&entry.new_oid)
            .map(|o| String::from_utf8_lossy(&o.data).into_owned())
            .unwrap_or_default()
    };

    let is_binary = old_content.bytes().any(|b| b == 0) || new_content.bytes().any(|b| b == 0);
    let path_str = format_rename_path(entry);

    if is_binary {
        writeln!(out, "-\t-\t{path_str}")?;
    } else {
        let (ins, del) = grit_lib::diff::count_changes(&old_content, &new_content);
        writeln!(out, "{ins}\t{del}\t{path_str}")?;
    }
    Ok(())
}

/// Format path for numstat/stat display (with rename arrow notation).
fn format_rename_path(entry: &grit_lib::diff::DiffEntry) -> String {
    let old_path = entry.old_path.as_deref().unwrap_or(RepoPath::from_str(""));
    let new_path = entry.new_path.as_deref().unwrap_or(RepoPath::from_str(""));
    match entry.status {
        grit_lib::diff::DiffStatus::Renamed | grit_lib::diff::DiffStatus::Copied => {
            // Use compact rename format: common_prefix/{old => new}/common_suffix
            grit_lib::diff::format_rename_path(
                &old_path.to_string_lossy(),
                &new_path.to_string_lossy(),
            )
        }
        _ => new_path.to_string(),
    }
}

fn write_diffstat(
    out: &mut impl Write,
    odb: &Odb,
    entries: &[grit_lib::diff::DiffEntry],
    stat_width: Option<usize>,
    stat_name_width: Option<usize>,
    stat_graph_width: Option<usize>,
    stat_count: Option<usize>,
    config: &ConfigSet,
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }

    let eff_name_width = stat_name_width.or_else(|| {
        config
            .get("diff.statNameWidth")
            .and_then(|v| v.parse::<usize>().ok())
    });
    let eff_graph_width = stat_graph_width.or_else(|| {
        config
            .get("diff.statGraphWidth")
            .and_then(|v| v.parse::<usize>().ok())
    });

    let mut files: Vec<FileStatInput> = Vec::with_capacity(entries.len());
    for entry in entries {
        let path_display = format_rename_path(entry);
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
            let old_content = String::from_utf8_lossy(&old_raw).into_owned();
            let new_content = String::from_utf8_lossy(&new_raw).into_owned();
            let (ins, del) = grit_lib::diff::count_changes(&old_content, &new_content);
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
        total_width: stat_width.unwrap_or_else(terminal_columns),
        line_prefix: "",
        width_prefix: "",
        subtract_prefix_from_terminal: false,
        stat_name_width: eff_name_width,
        stat_graph_width: eff_graph_width,
        stat_count,
        color_add: "",
        color_del: "",
        color_reset: "",
        graph_bar_slack: 0,
        graph_prefix_budget_slack: 0,
    };
    write_diffstat_block(out, &files, &opts)?;
    Ok(())
}

/// Write a `diff --git a/path b/path` header plus index/mode lines.
fn write_diff_header(out: &mut impl Write, entry: &grit_lib::diff::DiffEntry) -> Result<()> {
    write_diff_header_with_remerge(out, entry, None, true)
}

/// Same as [`write_diff_header`] but inserts an optional `remerge CONFLICT` line after `diff --git`.
///
/// When `include_index_lines` is `false`, only the `diff --git` line (and optional remerge line) are
/// written — matching `git show --remerge-diff --diff-filter=U` output.
pub(crate) fn write_diff_header_with_remerge(
    out: &mut impl Write,
    entry: &grit_lib::diff::DiffEntry,
    remerge_line: Option<&str>,
    include_index_lines: bool,
) -> Result<()> {
    use grit_lib::diff::DiffStatus;

    let old_path = entry
        .old_path
        .as_deref()
        .unwrap_or(entry.new_path.as_deref().unwrap_or(RepoPath::from_str("")));
    let new_path = entry
        .new_path
        .as_deref()
        .unwrap_or(entry.old_path.as_deref().unwrap_or(RepoPath::from_str("")));

    writeln!(out, "diff --git a/{old_path} b/{new_path}")?;
    if let Some(line) = remerge_line {
        writeln!(out, "{line}")?;
    }

    if !include_index_lines {
        return Ok(());
    }

    match entry.status {
        DiffStatus::Added => {
            writeln!(out, "new file mode {}", entry.new_mode)?;
            let old_abbrev = &entry.old_oid.to_hex()[..7];
            let new_abbrev = &entry.new_oid.to_hex()[..7];
            writeln!(out, "index {old_abbrev}..{new_abbrev}")?;
        }
        DiffStatus::Deleted => {
            writeln!(out, "deleted file mode {}", entry.old_mode)?;
            let old_abbrev = &entry.old_oid.to_hex()[..7];
            let new_abbrev = &entry.new_oid.to_hex()[..7];
            writeln!(out, "index {old_abbrev}..{new_abbrev}")?;
        }
        DiffStatus::Modified => {
            if entry.old_mode != entry.new_mode {
                writeln!(out, "old mode {}", entry.old_mode)?;
                writeln!(out, "new mode {}", entry.new_mode)?;
            }
            let old_abbrev = &entry.old_oid.to_hex()[..7];
            let new_abbrev = &entry.new_oid.to_hex()[..7];
            if entry.old_mode == entry.new_mode {
                writeln!(out, "index {old_abbrev}..{new_abbrev} {}", entry.old_mode)?;
            } else {
                writeln!(out, "index {old_abbrev}..{new_abbrev}")?;
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

    Ok(())
}

/// Show a tag object: tag header, then the tagged object.
fn show_tag(
    out: &mut impl Write,
    repo: &Repository,
    display_name: &str,
    data: &[u8],
    args: &Args,
    notes_map: &HashMap<ObjectId, Vec<u8>>,
    indent_heuristic: bool,
) -> Result<()> {
    let odb = &repo.odb;
    let tag = parse_tag(data).context("parsing tag")?;

    let tag_line_name = if display_name.is_empty() {
        tag.tag.as_str()
    } else {
        display_name
    };
    writeln!(out, "tag {tag_line_name}")?;
    if let Some(ref tagger) = tag.tagger {
        writeln!(out, "Tagger: {}", format_ident_display(tagger))?;
        writeln!(out, "Date:   {}", format_date(tagger))?;
    }
    writeln!(out)?;
    for line in tag.message.lines() {
        writeln!(out, "{line}")?;
    }
    if !tag.message.is_empty() {
        writeln!(out)?;
    }

    // Recursively show the tagged object
    let tagged_obj = odb.read(&tag.object).context("reading tagged object")?;
    match tagged_obj.kind {
        ObjectKind::Commit => {
            show_commit(
                out,
                repo,
                &tag.object,
                &tagged_obj.data,
                args,
                notes_map,
                &[],
                None,
                None,
                indent_heuristic,
                false,
            )?;
        }
        ObjectKind::Tag => {
            show_tag(
                out,
                repo,
                "",
                &tagged_obj.data,
                args,
                notes_map,
                indent_heuristic,
            )?;
        }
        ObjectKind::Tree => {
            let tree_oid = peel_to_tree(repo, tag.object).context("peeling tag to tree")?;
            show_tree_named(out, repo, "", tree_oid)?;
        }
        ObjectKind::Blob => {
            out.write_all(&tagged_obj.data)?;
        }
    }

    Ok(())
}

/// Inline commit info for format string expansion (mirrors log.rs CommitInfo usage).
struct CommitInfo<'a> {
    tree: ObjectId,
    parents: &'a [ObjectId],
    author: &'a str,
    committer: &'a str,
    message: &'a str,
}

/// Apply a format string with placeholders like %H, %h, %s, %an, %ae, etc.
///
/// Used by `rebase` todo generation to honor `rebase.instructionFormat`.
pub(crate) fn format_commit_placeholder(
    template: &str,
    oid: &ObjectId,
    commit: &grit_lib::objects::CommitData,
) -> String {
    apply_format_string(template, oid, commit, None, 0)
}

pub(crate) fn apply_format_string(
    template: &str,
    oid: &ObjectId,
    commit: &grit_lib::objects::CommitData,
    notes_raw: Option<&[u8]>,
    expand_tabs_in_log: usize,
) -> String {
    let info = CommitInfo {
        tree: commit.tree,
        parents: &commit.parents,
        author: &commit.author,
        committer: &commit.committer,
        message: &commit.message,
    };
    let hex = oid.to_hex();
    let mut result = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '%' {
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
            let magic_start = result.len();

            match chars.peek() {
                Some('H') => {
                    chars.next();
                    result.push_str(&hex);
                }
                Some('h') => {
                    chars.next();
                    result.push_str(&hex[..7.min(hex.len())]);
                }
                Some('T') => {
                    chars.next();
                    result.push_str(&info.tree.to_hex());
                }
                Some('t') => {
                    chars.next();
                    result.push_str(&info.tree.to_hex()[..7]);
                }
                Some('P') => {
                    chars.next();
                    let parents: Vec<String> = info.parents.iter().map(|p| p.to_hex()).collect();
                    result.push_str(&parents.join(" "));
                }
                Some('p') => {
                    chars.next();
                    let parents: Vec<String> = info
                        .parents
                        .iter()
                        .map(|p| p.to_hex()[..7].to_owned())
                        .collect();
                    result.push_str(&parents.join(" "));
                }
                Some('a') => {
                    chars.next();
                    match chars.peek() {
                        Some('n') => {
                            chars.next();
                            result.push_str(&extract_name(info.author));
                        }
                        Some('e') => {
                            chars.next();
                            result.push_str(&extract_email(info.author));
                        }
                        Some('d') => {
                            chars.next();
                            result.push_str(&format_date(info.author));
                        }
                        Some('i') => {
                            chars.next();
                            result.push_str(&format_date_iso(info.author));
                        }
                        Some('r') => {
                            chars.next();
                            result.push_str(&format_date_relative(info.author));
                        }
                        _ => result.push_str("%a"),
                    }
                }
                Some('c') => {
                    chars.next();
                    match chars.peek() {
                        Some('n') => {
                            chars.next();
                            result.push_str(&extract_name(info.committer));
                        }
                        Some('e') => {
                            chars.next();
                            result.push_str(&extract_email(info.committer));
                        }
                        Some('d') => {
                            chars.next();
                            result.push_str(&format_date(info.committer));
                        }
                        Some('i') => {
                            chars.next();
                            result.push_str(&format_date_iso(info.committer));
                        }
                        Some('r') => {
                            chars.next();
                            result.push_str(&format_date_relative(info.committer));
                        }
                        _ => result.push_str("%c"),
                    }
                }
                Some('s') => {
                    chars.next();
                    let subj = grit_lib::commit_pretty::message_subject(info.message);
                    if expand_tabs_in_log > 0 {
                        result.push_str(&grit_lib::tab_expand::expand_tabs_in_line(
                            &subj,
                            expand_tabs_in_log,
                        ));
                    } else {
                        result.push_str(&subj);
                    }
                }
                Some('B') => {
                    chars.next();
                    if !info.message.is_empty() {
                        let msg = if expand_tabs_in_log > 0 {
                            grit_lib::tab_expand::expand_tabs_in_multiline_message(
                                info.message,
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
                Some('b') => {
                    chars.next();
                    let raw_body = grit_lib::commit_pretty::message_body(info.message);
                    let body = if expand_tabs_in_log > 0 {
                        grit_lib::tab_expand::expand_tabs_in_multiline_message(
                            raw_body,
                            expand_tabs_in_log,
                        )
                    } else {
                        raw_body.to_owned()
                    };
                    if !body.is_empty() {
                        result.push_str(&body);
                        if !body.ends_with('\n') {
                            result.push('\n');
                        }
                    }
                }
                Some('n') => {
                    chars.next();
                    result.push('\n');
                }
                Some('N') => {
                    chars.next();
                    if let Some(raw) = notes_raw {
                        result.push_str(&String::from_utf8_lossy(raw));
                    }
                }
                Some('D') => {
                    chars.next();
                    // %D: decorations without parentheses — we leave it empty
                    // since we don't have a ref database context here.
                }
                Some('d') => {
                    chars.next();
                    // %d: decorations with parentheses — we leave it empty.
                }
                Some('%') => {
                    chars.next();
                    result.push('%');
                }
                Some('(') => {
                    // Extended placeholder, e.g. `%(trailers:...)`. Capture the balanced
                    // `(...)` payload and delegate trailer formatting to the shared engine.
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
                        result.push('%');
                    } else if let Some(rest) = inner.strip_prefix("trailers") {
                        if let Some(opts) = crate::commands::log::parse_trailers_opts(rest) {
                            chars = look;
                            let formatted =
                                grit_lib::commit_trailers::format_trailers(info.message, &opts);
                            result.push_str(&formatted);
                        } else {
                            // Invalid option: emit the leading `%` and reparse `(...)` as text.
                            result.push('%');
                        }
                    } else {
                        // Unhandled extended placeholder: emit literally.
                        result.push('%');
                    }
                }
                _ => result.push('%'),
            }
            if magic != Magic::None {
                let produced_empty = result.len() == magic_start;
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
                        if produced_empty {
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
        } else {
            result.push(ch);
        }
    }

    result
}

/// Extract the name portion from a Git ident string (e.g. "Name <email> ts offset").
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

/// Format ident for display: "Name <email>".
fn format_ident_display(ident: &str) -> String {
    let name = extract_name(ident);
    let email = extract_email(ident);
    format!("{name} <{email}>")
}

/// Format the date portion of a Git ident string in ISO 8601 format (%ci / %ai).
fn format_date_iso(ident: &str) -> String {
    let parts: Vec<&str> = ident.rsplitn(3, ' ').collect();
    if parts.len() >= 2 {
        let ts_str = parts[1];
        let offset_str = parts[0];
        if let Ok(ts) = ts_str.parse::<i64>() {
            // Parse the offset to apply to the timestamp.
            let offset_secs = parse_offset_seconds(offset_str);
            let dt = time::OffsetDateTime::from_unix_timestamp(ts + offset_secs as i64)
                .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
            let format = time::format_description::parse_borrowed::<1>(
                "[year]-[month]-[day] [hour]:[minute]:[second]",
            );
            if let Ok(fmt) = format {
                if let Ok(formatted) = dt.format(&fmt) {
                    // Git outputs: 2001-09-09 01:46:40 +0000
                    return format!("{formatted} {offset_str}");
                }
            }
        }
        format!("{ts_str} {offset_str}")
    } else {
        ident.to_owned()
    }
}

/// Parse a Git timezone offset string like "+0200" or "-0530" into seconds.
fn parse_offset_seconds(offset: &str) -> i32 {
    if offset.len() < 5 {
        return 0;
    }
    let sign = if offset.starts_with('-') { -1 } else { 1 };
    let hours: i32 = offset[1..3].parse().unwrap_or(0);
    let minutes: i32 = offset[3..5].parse().unwrap_or(0);
    sign * (hours * 3600 + minutes * 60)
}

/// Format the date portion of a Git ident string as a relative date (%cr / %ar).
fn format_date_relative(ident: &str) -> String {
    let parts: Vec<&str> = ident.rsplitn(3, ' ').collect();
    if parts.len() >= 2 {
        let ts_str = parts[1];
        if let Ok(ts) = ts_str.parse::<i64>() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            let diff = now - ts;
            if diff < 0 {
                return "in the future".to_string();
            }
            let diff = diff as u64;
            if diff < 60 {
                return format!("{diff} seconds ago");
            }
            let minutes = diff / 60;
            if minutes < 60 {
                return format!("{minutes} minutes ago");
            }
            let hours = minutes / 60;
            if hours < 24 {
                return format!("{hours} hours ago");
            }
            let days = hours / 24;
            if days < 14 {
                return format!("{days} days ago");
            }
            let weeks = days / 7;
            if weeks < 8 {
                return format!("{weeks} weeks ago");
            }
            let months = days / 30;
            if months < 12 {
                return format!("{months} months ago");
            }
            let years = days / 365;
            return format!("{years} years ago");
        }
    }
    ident.to_owned()
}

/// Parse a timezone offset string like "+0200" or "-0500" into seconds.
fn parse_tz_offset(offset: &str) -> i64 {
    let bytes = offset.as_bytes();
    if bytes.len() < 5 {
        return 0;
    }
    let sign = if bytes[0] == b'-' { -1i64 } else { 1i64 };
    let hours: i64 = offset[1..3].parse().unwrap_or(0);
    let minutes: i64 = offset[3..5].parse().unwrap_or(0);
    sign * (hours * 3600 + minutes * 60)
}

/// Format the date portion of a Git ident string for human display.
/// Default Git date format: "Thu Apr  7 15:13:13 2005 -0700"
fn format_date(ident: &str) -> String {
    let parts: Vec<&str> = ident.rsplitn(3, ' ').collect();
    if parts.len() < 2 {
        return ident.to_owned();
    }
    let ts_str = parts[1];
    let offset_str = parts[0];
    let ts = match ts_str.parse::<i64>() {
        Ok(v) => v,
        Err(_) => return format!("{ts_str} {offset_str}"),
    };

    let tz_offset_secs = parse_tz_offset(offset_str);
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
    // Git's default ("medium") date format uses an UNPADDED day: date.c emits `"%.3s %d "`
    // (e.g. `Thu Apr 7 ...`), not the space-padded `%e` form. (t7600 squash messages,
    // generated from `git show -s`, compare byte-for-byte against this.)
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

/// Apply rename and/or copy detection to diff entries based on CLI flags.
fn apply_rename_copy_detection(
    odb: &Odb,
    entries: Vec<DiffEntry>,
    args: &Args,
    old_tree_oid: Option<&ObjectId>,
) -> Vec<DiffEntry> {
    let has_copies = !args.find_copies.is_empty();
    let has_renames = args.find_renames.is_some();

    if has_copies {
        let threshold = args
            .find_copies
            .last()
            .and_then(|v| v.parse::<u32>().ok())
            .or_else(|| {
                args.find_renames
                    .as_ref()
                    .and_then(|v| v.parse::<u32>().ok())
            })
            .unwrap_or(50);
        let find_copies_harder = args.find_copies.len() > 1;

        // Build source tree entries for copy detection.
        let source_tree_entries = if let Some(tree_oid) = old_tree_oid {
            collect_tree_entries_for_copies(odb, tree_oid)
        } else {
            vec![]
        };

        detect_copies(
            odb,
            None,
            entries,
            threshold,
            find_copies_harder,
            &source_tree_entries,
        )
    } else if has_renames {
        let threshold = args
            .find_renames
            .as_ref()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(50);
        detect_renames(odb, None, entries, threshold)
    } else {
        entries
    }
}

/// Collect all tree entries as (path, mode_str, oid) for copy detection.
fn collect_tree_entries_for_copies(
    odb: &Odb,
    tree_oid: &ObjectId,
) -> Vec<(String, String, ObjectId)> {
    let mut result = Vec::new();
    collect_tree_entries_recursive(odb, tree_oid, "", &mut result);
    result
}

fn collect_tree_entries_recursive(
    odb: &Odb,
    tree_oid: &ObjectId,
    prefix: &str,
    result: &mut Vec<(String, String, ObjectId)>,
) {
    let obj = match odb.read(tree_oid) {
        Ok(obj) => obj,
        Err(_) => return,
    };
    let tree = match parse_tree(&obj.data) {
        Ok(tree) => tree,
        Err(_) => return,
    };
    for entry in &tree {
        let name_str = String::from_utf8_lossy(&entry.name);
        let path = if prefix.is_empty() {
            name_str.into_owned()
        } else {
            format!("{prefix}/{name_str}")
        };
        if entry.mode == 0o040000 {
            collect_tree_entries_recursive(odb, &entry.oid, &path, result);
        } else {
            result.push((path, format!("{:06o}", entry.mode), entry.oid));
        }
    }
}

/// Load notes from the configured notes ref (or `refs/notes/commits` default).
pub(crate) fn load_notes_map(repo: &Repository) -> HashMap<ObjectId, Vec<u8>> {
    use grit_lib::config::ConfigSet;
    use grit_lib::refs::resolve_ref;

    let mut map = HashMap::new();

    let notes_ref = std::env::var("GIT_NOTES_REF")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
            config
                .get("core.notesRef")
                .unwrap_or_else(|| "refs/notes/commits".to_string())
        });

    let notes_oid = match resolve_ref(&repo.git_dir, &notes_ref) {
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

    collect_notes_recursive(repo, &tree_oid, String::new(), &mut map);
    map
}

fn collect_notes_recursive(
    repo: &Repository,
    tree_oid: &ObjectId,
    prefix: String,
    map: &mut HashMap<ObjectId, Vec<u8>>,
) {
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
            collect_notes_recursive(repo, &entry.oid, full_hex, map);
        } else if let Ok(commit_oid) = full_hex.parse::<ObjectId>() {
            if let Ok(blob) = repo.odb.read(&entry.oid) {
                map.insert(commit_oid, blob.data);
            }
        }
    }
}
