//! `grit format-patch` — generate patch files from commits.
//!
//! Produces email-style patch files (with From/Subject/Date headers and a diff)
//! for each commit in a range.  Output goes to individual `.patch` files in the
//! current directory (or `-o <dir>`), or to stdout with `--stdout`.

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use grit_lib::config::{parse_bool, ConfigSet};
use grit_lib::diff::{count_changes, diff_trees, unified_diff_with_prefix, zero_oid, DiffStatus};
use grit_lib::repo_path::{RepoPath, RepoPathBuf};
use grit_lib::diffstat::{
    write_diffstat_block, DiffstatOptions, FileStatInput, FORMAT_PATCH_STAT_WIDTH,
};
use grit_lib::merge_base::merge_bases_first_vs_rest;
use grit_lib::merge_diff::{blob_text_for_diff_with_oid, diff_textconv_active, is_binary_for_diff};
use grit_lib::objects::{parse_commit, CommitData, ObjectId};
use grit_lib::odb::Odb;
use grit_lib::patch_ids::compute_patch_id;
use grit_lib::porcelain::format_patch::{
    apply_rfc_prefix, build_patch_filename, build_patch_subject, commit_author_timestamp,
    encode_email_address, encode_email_address_for_charset, ensure_trailing_slash,
    first_subject_line, flatten_subject, format_date_rfc2822, format_ident, is_valid_from_ident,
    mboxrd_escape, notes_value_to_ref, path_matches_spec, prev_version_label, push_indented,
    rfc2047_charset_label, sanitize_reroll, strip_angles, write_addr_header, write_folded_header,
    write_signature, write_subject_header, write_thread_headers,
};
use grit_lib::repo::Repository;

use grit_lib::rev_list::{
    rev_list, split_revision_token, split_symmetric_diff, OrderingMode, RevListOptions,
};
use grit_lib::rev_parse::{resolve_revision, resolve_revision_for_range_end};
use std::collections::HashSet;
use std::io::{self, Write};
use std::path::PathBuf;

use crate::ident::{read_git_identity_name_env, GitIdentityNameEnv};
use grit_lib::commit_encoding::commit_message_unicode_for_display;

/// Arguments for `grit format-patch`.
#[derive(Debug, ClapArgs)]
#[command(about = "Prepare patches for e-mail submission")]
pub struct Args {
    /// Revision(s), range, or count. Empty means last commit (`-1`).
    /// With `--cherry-pick --right-only`, supports symmetric `A...B` like `git rebase --apply`.
    #[arg(value_name = "REV", num_args = 0..)]
    pub revisions: Vec<String>,

    /// Pathspec limiting which files/commits are shown (after `--`).
    #[arg(value_name = "PATH", last = true)]
    pub pathspec: Vec<String>,

    /// Commit message encoding for the patch (Git compatibility; `t3901` passes this).
    #[arg(long = "encoding", value_name = "ENCODING")]
    pub encoding: Option<String>,

    /// Limit output to commits whose message matches the given pattern (like `git log --grep`).
    #[arg(long = "grep", value_name = "PATTERN")]
    pub grep_patterns: Vec<String>,

    /// Interpret `--grep` patterns as fixed strings.
    #[arg(short = 'F', long = "fixed-strings")]
    pub fixed_strings: bool,

    /// Interpret `--grep` patterns as POSIX basic regular expressions.
    #[arg(short = 'G', long = "basic-regexp")]
    pub basic_regexp: bool,

    /// Interpret `--grep` patterns as POSIX extended regular expressions.
    #[arg(short = 'E', long = "extended-regexp")]
    pub extended_regexp: bool,

    /// Interpret `--grep` patterns as Perl-compatible regular expressions.
    #[arg(short = 'P', long = "perl-regexp")]
    pub perl_regexp: bool,

    /// Match `--grep` patterns case-insensitively.
    #[arg(short = 'i', long = "regexp-ignore-case")]
    pub regexp_ignore_case: bool,

    /// Write output to stdout instead of individual files.
    #[arg(long)]
    pub stdout: bool,

    /// Omit patch-id equivalents on the left side of a symmetric range (used by `rebase --apply`).
    #[arg(long = "cherry-pick")]
    pub cherry_pick: bool,

    /// With `--cherry-pick`, list only commits on the right side of `A...B`.
    #[arg(long = "right-only")]
    pub right_only: bool,

    /// Use default a/b path prefixes (accepted for `rebase --apply` compatibility).
    #[arg(long = "default-prefix", hide = true)]
    pub default_prefix: bool,

    /// Do not detect renames in diffs (accepted for `rebase --apply` compatibility).
    #[arg(long = "no-renames", hide = true)]
    pub no_renames: bool,

    /// Do not emit a cover letter (accepted for `rebase --apply` compatibility).
    #[arg(long = "no-cover-letter", hide = true)]
    pub no_cover_letter: bool,

    /// Pretty-print / mbox encoding (accepted; `mboxrd` is the default output shape).
    #[arg(long = "pretty", value_name = "FORMAT", hide = true)]
    pub pretty: Option<String>,

    /// Order commits topologically (used by `rebase --apply`).
    #[arg(long = "topo-order")]
    pub topo_order: bool,

    /// Omit base-commit trailer (accepted for `rebase --apply` compatibility).
    #[arg(long = "no-base", hide = true)]
    pub no_base: bool,

    /// Add `[PATCH n/m]` numbering to subjects.
    #[arg(short = 'n', long = "numbered")]
    pub numbered: bool,

    /// Suppress `[PATCH n/m]` numbering.
    #[arg(short = 'N', long = "no-numbered")]
    pub no_numbered: bool,

    /// Start numbering patches at <n> instead of 1.
    #[arg(long = "start-number", value_name = "N", default_value_t = 1)]
    pub start_number: usize,

    /// Generate a cover letter as patch 0.
    #[arg(long = "cover-letter")]
    pub cover_letter: bool,

    /// Format all commits from root (instead of since a revision).
    #[arg(long = "root")]
    pub root: bool,

    /// Custom subject prefix (default: "PATCH").
    #[arg(long = "subject-prefix", value_name = "PREFIX")]
    pub subject_prefix: Option<String>,

    /// Threading mode for Message-Id/In-Reply-To/References chaining.
    #[arg(long = "thread", value_name = "STYLE", num_args = 0..=1, default_missing_value = "shallow", require_equals = true)]
    pub thread: Option<String>,

    /// Disable threading.
    #[arg(long = "no-thread")]
    pub no_thread: bool,

    /// Cover-letter signature.
    #[arg(long = "signature", value_name = "SIGNATURE")]
    pub signature: Option<String>,

    /// Read signature from a file.
    #[arg(long = "signature-file", value_name = "FILE")]
    pub signature_file: Option<PathBuf>,

    /// Use the zero (all-zero) commit hash in `From ` lines.
    #[arg(long = "zero-commit")]
    pub zero_commit: bool,

    /// Generate a numstat in addition to the patch (accepted; full patch is still emitted).
    #[arg(long = "numstat")]
    pub numstat: bool,

    /// Generate a shortstat in addition to the patch (accepted; full patch is still emitted).
    #[arg(long = "shortstat")]
    pub shortstat: bool,

    /// Number of context lines in the diff (`-U<n>` / `--unified=<n>`).
    #[arg(long = "unified", value_name = "N")]
    pub unified: Option<usize>,

    /// Generate diffs relative to a subdirectory.
    #[arg(long = "relative", num_args = 0..=1, default_missing_value = "", require_equals = true)]
    pub relative: Option<String>,

    /// Disable --relative.
    #[arg(long = "no-relative")]
    pub no_relative: bool,

    /// Choose how to populate the cover letter from a branch/commit description.
    #[arg(long = "cover-from-description", value_name = "MODE")]
    pub cover_from_description: Option<String>,

    /// Read the cover-letter description from a file.
    #[arg(long = "description-file", value_name = "FILE")]
    pub description_file: Option<PathBuf>,

    /// Force keeping the in-body From: header even when redundant.
    #[arg(long = "force-in-body-from")]
    pub force_in_body_from: bool,

    /// Do not force keeping the in-body From: header.
    #[arg(long = "no-force-in-body-from")]
    pub no_force_in_body_from: bool,

    /// RFC2047-encode email headers (default).
    #[arg(long = "encode-email-headers")]
    pub encode_email_headers: bool,

    /// Do not RFC2047-encode email headers.
    #[arg(long = "no-encode-email-headers")]
    pub no_encode_email_headers: bool,

    /// Use mboxrd escaping in the body.
    #[arg(long = "mboxrd")]
    pub mboxrd: bool,

    /// Disable RFC prefix (`--rfc`).
    #[arg(long = "no-rfc")]
    pub no_rfc: bool,

    /// Rejected: format-patch always shows the full patch.
    #[arg(long = "name-only", hide = true)]
    pub name_only: bool,

    /// Rejected: format-patch always shows the full patch.
    #[arg(long = "name-status", hide = true)]
    pub name_status: bool,

    /// Rejected: format-patch always shows the full patch.
    #[arg(long = "check", hide = true)]
    pub check: bool,

    /// Output directory for patch files.
    #[arg(short = 'o', long = "output-directory", value_name = "DIR")]
    pub output_directory: Option<PathBuf>,

    /// Add base-commit info (the commit the series is based on).
    #[arg(long = "base", value_name = "COMMIT")]
    pub base: Option<String>,

    /// Add Signed-off-by trailer using the committer identity.
    #[arg(short = 's', long = "signoff")]
    pub signoff: bool,

    /// Set the In-Reply-To header (for threading patches).
    #[arg(long = "in-reply-to", value_name = "MESSAGE-ID")]
    pub in_reply_to: Option<String>,

    /// Add Cc header(s) to each patch email.
    #[arg(long = "cc", value_name = "EMAIL")]
    pub cc: Vec<String>,

    /// Add To header(s) to each patch email.
    #[arg(long = "to", value_name = "EMAIL")]
    pub to: Vec<String>,

    /// Create MIME multipart attachment (optional custom boundary).
    #[arg(long = "attach", num_args = 0..=1, default_missing_value = "", require_equals = true)]
    pub attach: Option<String>,

    /// Create MIME inline attachment (optional custom boundary).
    #[arg(long = "inline", num_args = 0..=1, default_missing_value = "", require_equals = true)]
    pub inline: Option<String>,

    /// Keep subject intact (do not strip/add [PATCH] prefix).
    #[arg(short = 'k', long = "keep-subject")]
    pub keep_subject: bool,

    /// Include patches for commits that don't change any files.
    #[arg(long = "always")]
    pub always: bool,

    /// Prepend "RFC" (or a custom string) to the subject prefix. May be repeated; last wins.
    #[arg(
        long = "rfc",
        num_args = 0..=1,
        default_missing_value = "RFC",
        require_equals = true,
        action = clap::ArgAction::Append
    )]
    pub rfc: Vec<String>,

    /// Add extra header.
    #[arg(long = "add-header", value_name = "HEADER")]
    pub add_header: Vec<String>,

    /// Number of context lines in patches.
    #[arg(short = 'U', value_name = "N")]
    pub context_lines: Option<usize>,

    /// Include binary diffs (accepted for compatibility).
    #[arg(long = "binary")]
    pub binary: bool,

    /// Do not run textconv in the patch body (emit `Binary files differ` like plumbing).
    #[arg(long = "no-binary")]
    pub no_binary: bool,

    /// Do not use a/b/ prefix in diff output.
    #[arg(long = "no-prefix")]
    pub no_prefix: bool,

    /// Detect renames.
    #[arg(short = 'M')]
    pub detect_renames: bool,

    /// Use numbered filenames (0001, 0002, ...) instead of subject-based names.
    #[arg(long = "numbered-files")]
    pub numbered_files: bool,

    /// Filename suffix for generated patches (default `.patch`).
    #[arg(long = "suffix")]
    pub suffix: Option<String>,

    /// Include diffstat in patch body (`--stat[=width[,name-width[,count]]]`).
    #[arg(long = "stat", num_args = 0..=1, default_missing_value = "", require_equals = true)]
    pub stat: Option<String>,

    /// Limit files shown in embedded `--stat`.
    #[arg(long = "stat-count")]
    pub stat_count: Option<usize>,

    /// Total width for embedded `--stat`.
    #[arg(long = "stat-width")]
    pub stat_width_cli: Option<usize>,

    /// Graph width cap for embedded `--stat`.
    #[arg(long = "stat-graph-width")]
    pub stat_graph_width: Option<usize>,

    /// Name width cap for embedded `--stat`.
    #[arg(long = "stat-name-width")]
    pub stat_name_width: Option<usize>,

    /// Limit number of patches (e.g., -1 for only the last commit).
    #[arg(short = '1', hide = true)]
    pub last_one: bool,

    /// Populated by the CLI layer from `-N` count shorthands (`format-patch -3`).
    #[arg(long = "grit-format-patch-max-count", hide = true, value_name = "N")]
    pub grit_format_patch_max_count: Option<usize>,
    /// Use the From: header to attribute patches (accepted, partial impl).
    #[arg(long = "from", default_missing_value = "", num_args = 0..=1, require_equals = true)]
    pub from: Option<String>,

    /// Suppress signature.
    #[arg(long = "no-signature")]
    pub no_signature: bool,

    /// Append notes (optionally from a specific notes ref; may be repeated).
    #[arg(
        long = "notes",
        default_missing_value = "",
        num_args = 0..=1,
        require_equals = true,
        action = clap::ArgAction::Append
    )]
    pub notes: Vec<String>,

    /// Suppress notes.
    #[arg(long = "no-notes")]
    pub no_notes: bool,

    /// Ignore if upstream already has the patch.
    #[arg(long = "ignore-if-in-upstream")]
    pub ignore_if_in_upstream: bool,

    /// Reroll count / version prefix (e.g. -v2).
    #[arg(short = 'v', long = "reroll-count", value_name = "N")]
    pub reroll_count: Option<String>,

    /// Include interdiff against a previous version.
    #[arg(long = "interdiff", value_name = "REV")]
    pub interdiff: Option<String>,

    /// Include range-diff against a previous version.
    #[arg(long = "range-diff", value_name = "REV")]
    pub range_diff: Option<String>,

    /// Show patch (accepted for compat, default behavior).
    #[arg(short = 'p', long = "patch")]
    pub patch: bool,

    /// Max filename length for patches.
    #[arg(long = "filename-max-length", value_name = "N")]
    pub filename_max_length: Option<usize>,

    /// Creation factor for range-diff.
    #[arg(long = "creation-factor", value_name = "N")]
    pub creation_factor: Option<usize>,

    /// Output file (instead of per-patch files).
    #[arg(long = "output", value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Suppress Cc: headers.
    #[arg(long = "no-cc")]
    pub no_cc: bool,

    /// Suppress To: headers.
    #[arg(long = "no-to")]
    pub no_to: bool,

    /// Suppress From: header (overrides `format.from`).
    #[arg(long = "no-from")]
    pub no_from: bool,

    /// Do not add `format.headers` from config (still allows `--add-header`).
    #[arg(long = "no-add-header")]
    pub no_add_header: bool,

    /// Progress display (accepted for compat, no-op).
    #[arg(long = "progress")]
    pub progress: bool,

    /// Quiet mode.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Order files according to the given orderfile.
    #[arg(short = 'O', value_name = "orderfile")]
    pub order_file: Option<String>,

    /// Show full object hashes in diff output.
    #[arg(long = "full-index")]
    pub full_index: bool,

    /// Graph mode for mbox output (affects embedded diffstat layout).
    #[arg(long = "graph", hide = true)]
    pub graph: bool,
}

/// How to populate the `From:` mbox header (the `From ` line is always the commit OID).
#[derive(Debug, Clone)]
enum FromHeaderMode {
    /// Use the commit author (Git default when `format.from` is unset / false).
    Author,
    /// `format.from=true` or `--from` without `=` value.
    Committer,
    /// Explicit mailbox from `--from=` or `format.from=<ident>`.
    Custom(String),
    /// `--no-from` or suppressed.
    Omit,
}

/// Threading mode resolved from `--thread` / `format.thread`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThreadMode {
    None,
    Shallow,
    Deep,
}

/// Extra headers/options computed from args, passed into formatting functions.
struct PatchOptions {
    in_reply_to: Option<String>,
    cc: Vec<String>,
    to: Vec<String>,
    extra_headers: Vec<String>,
    from_header: FromHeaderMode,
    signoff: bool,
    attach: Option<String>,
    inline: Option<String>,
    keep_subject: bool,
    base_commit: Option<String>,
    /// prerequisite-patch-id trailers (from `--base`), emitted before base-commit.
    prereq_patch_ids: Vec<String>,
    order_file: Option<String>,
    stat_width: usize,
    stat_name_width: Option<usize>,
    stat_graph_width: Option<usize>,
    stat_count: Option<usize>,
    /// `format-patch --graph`: indent stat like Git's mbox graph mode.
    format_patch_graph: bool,
    /// Unified diff path prefixes (`a/` / `b/` unless `format.noprefix` / `--no-prefix`).
    diff_src_prefix: &'static str,
    diff_dst_prefix: &'static str,
    /// Number of diff context lines (`-U<n>`), default 3.
    context_lines: usize,
    /// Signature trailer (`None` = suppress the `-- \n...` block entirely).
    signature: Option<String>,
    /// RFC2047-encode the Subject/From headers when non-ASCII (default true).
    encode_email_headers: bool,
    /// Force keeping the in-body From: even when redundant (`--force-in-body-from`).
    force_in_body_from: bool,
    /// Apply mboxrd `>From` escaping to the body.
    mboxrd: bool,
    /// `--relative=<dir>`: strip this prefix from diff paths.
    relative: Option<String>,
    /// `--zero-commit`: use an all-zero object name on the `From` line.
    zero_commit: bool,
    /// `-p`/`--patch` given without any stat option: suppress the per-patch diffstat block.
    suppress_stat: bool,
    /// `--stat` requested (human-readable diffstat block).
    want_stat: bool,
    /// `--numstat` requested (machine-readable `ins\tdel\tpath` block).
    want_numstat: bool,
    /// `--shortstat` requested (single summary line).
    want_shortstat: bool,
    /// Ordered `(header, refname)` notes refs to append to each patch body (from `--notes` /
    /// `format.notes`); empty when notes display is disabled.
    notes_refs: Vec<(String, String)>,
    /// Pathspec (from `format-patch -- <path>...`): restricts each patch's diff to these paths.
    pathspec: Vec<String>,
    /// Rename-detection similarity threshold (`Some(50)` by default; `None` with `--no-renames`).
    rename_threshold: Option<u32>,
}

pub fn run(mut args: Args) -> Result<()> {
    let repo = Repository::discover(None).context("not a git repository")?;

    // Load git configuration for format.* keys
    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();

    if let Some(raw) = config.get("format.noprefix") {
        parse_bool(&raw).map_err(|e| {
            let q = |s: &str| format!("'{s}'");
            anyhow::anyhow!(
                "fatal: {e} for {}\
                 \nhint: {} used to accept any value and treat that as {}.\
                 \nhint: Now it only accepts boolean values, like what {} does.",
                q("format.noprefix"),
                q("format.noprefix"),
                q("true"),
                q("diff.noprefix"),
            )
        })?;
    }

    let (diff_src_prefix, diff_dst_prefix) = if args.no_prefix {
        ("", "")
    } else if args.default_prefix {
        ("a/", "b/")
    } else {
        match config.get("format.noprefix") {
            Some(v) => {
                let b = parse_bool(&v).map_err(|e| {
                    let q = |s: &str| format!("'{s}'");
                    anyhow::anyhow!(
                        "fatal: {e} for {}\
                         \nhint: {} used to accept any value and treat that as {}.\
                         \nhint: Now it only accepts boolean values, like what {} does.",
                        q("format.noprefix"),
                        q("format.noprefix"),
                        q("true"),
                        q("diff.noprefix"),
                    )
                })?;
                if b {
                    ("", "")
                } else {
                    ("a/", "b/")
                }
            }
            None => ("a/", "b/"),
        }
    };

    if let Some(ref val) = args.stat {
        if !val.is_empty() {
            let parts: Vec<&str> = val.split(',').collect();
            if let Some(w) = parts.first().and_then(|s| s.parse::<usize>().ok()) {
                if args.stat_width_cli.is_none() {
                    args.stat_width_cli = Some(w);
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
    }

    // Rename detection: format-patch enables it by default (threshold 50), unless `--no-renames`
    // is given or `diff.renames` is disabled in config. `-M` forces it on.
    let rename_threshold: Option<u32> = if args.no_renames {
        None
    } else if args.detect_renames {
        Some(50)
    } else {
        match config.get("diff.renames") {
            Some(val) => match val.to_lowercase().as_str() {
                "false" | "no" | "0" => None,
                "true" | "yes" | "1" | "" | "copies" | "copy" => Some(50),
                _ => None,
            },
            None => Some(50),
        }
    };

    let stat_width = args.stat_width_cli.unwrap_or(FORMAT_PATCH_STAT_WIDTH);
    let stat_name_width = args.stat_name_width;
    let stat_graph_width = args.stat_graph_width;

    // Reject diff-only output modes (format-patch always emits the full patch).
    if args.name_only {
        anyhow::bail!("fatal: --name-only does not make sense");
    }
    if args.name_status {
        anyhow::bail!("fatal: --name-status does not make sense");
    }
    if args.check {
        anyhow::bail!("fatal: --check does not make sense");
    }

    // --subject-prefix/--rfc cannot be combined with -k.
    if args.keep_subject && (args.subject_prefix.is_some() || !args.rfc.is_empty()) {
        anyhow::bail!("fatal: options '--subject-prefix/--rfc' and '-k' cannot be used together");
    }

    let filename_max_length = args.filename_max_length.or_else(|| {
        config
            .get("format.filenamemaxlength")
            .or_else(|| config.get("format.filenameMaxLength"))
            .and_then(|s| s.trim().parse().ok())
    });

    // Output conflict checks: --stdout / --output / --output-directory are mutually exclusive
    // (a configured format.outputDirectory does NOT conflict — only the CLI flag does).
    let output_modes = [
        args.stdout,
        args.output.is_some(),
        args.output_directory.is_some(),
    ]
    .iter()
    .filter(|b| **b)
    .count();
    if output_modes > 1 {
        anyhow::bail!("--stdout, --output, and --output-directory are mutually exclusive");
    }

    // Reroll-count: `-v<x>`/`--reroll-count`. Filename prefix `v<x>-`, subject `[PATCH v<x> ...]`.
    let reroll = args.reroll_count.clone();

    let (positive_specs, exclude_specs): (Vec<&String>, Vec<&String>) =
        args.revisions.iter().partition(|s| !s.starts_with('^'));
    let mut exclude_rest: Vec<String> = exclude_specs
        .iter()
        .map(|s| s.strip_prefix('^').unwrap_or(s.as_str()).to_string())
        .collect();

    let mut rev_tokens: Vec<String> = positive_specs.iter().map(|s| (*s).clone()).collect();
    let max_count_flag = if args.last_one { Some(1) } else { None };
    let max_count_from_argv = strip_leading_neg_count(&mut rev_tokens);
    // A leading `-N` count token placed after `--` (e.g. `format-patch -- -1`) is consumed by clap
    // into the pathspec. When no revisions/count were given on the left, treat it as the commit
    // count rather than a (non-matching) pathspec so `-N` works regardless of `--` placement.
    let mut pathspec_tokens: Vec<String> = args.pathspec.clone();
    let max_count_from_pathspec = if positive_specs.is_empty() && max_count_from_argv.is_none() {
        strip_leading_neg_count(&mut pathspec_tokens)
    } else {
        None
    };
    let max_count = max_count_flag
        .or(args.grit_format_patch_max_count)
        .or(max_count_from_argv)
        .or(max_count_from_pathspec);
    let no_revs = positive_specs.is_empty() && exclude_specs.is_empty();
    // With no revision range and no count, git defaults to `@{upstream}..HEAD`. When the current
    // branch has a configured upstream, format the commits since it; otherwise there is nothing to
    // format (git emits no output, exit 0) -- so leave the commit set empty.
    let mut default_no_upstream = false;
    if no_revs && max_count.is_none() && !args.root && !(args.cherry_pick && args.right_only) {
        match resolve_branch_upstream_oid(&repo, &config) {
            Some(upstream_oid) => {
                rev_tokens.push("HEAD".to_owned());
                exclude_rest.push(upstream_oid.to_hex());
            }
            None => {
                default_no_upstream = true;
            }
        }
    }

    // Pathspec after `--` limits which commits/diffs are shown.
    let pathspec: Vec<String> = pathspec_tokens.clone();

    // Determine the list of commits to format.
    let mut commits = if default_no_upstream {
        // No revision range, no count, and no upstream: nothing to format.
        Vec::new()
    } else if args.cherry_pick && args.right_only {
        let range_spec = rev_tokens
            .first()
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "--cherry-pick --right-only requires a symmetric revision range A...B"
                )
            })?;
        collect_cherry_pick_right_only_commits(&repo, range_spec, &exclude_rest, args.topo_order)?
    } else if args.root {
        if !exclude_rest.is_empty() {
            anyhow::bail!(
                "revision exclusions (^rev) are only supported with --cherry-pick --right-only"
            );
        }
        let revision = rev_tokens
            .first()
            .cloned()
            .unwrap_or_else(|| "-1".to_owned());
        collect_root_commits(&repo, &revision)?
    } else {
        let (mut out, pos_specs, neg_specs) = collect_commits_for_format_patch(
            &repo,
            &rev_tokens,
            &exclude_rest,
            max_count,
            args.topo_order,
        )?;
        if args.ignore_if_in_upstream {
            filter_ignore_if_in_upstream(&repo, &pos_specs, &neg_specs, &mut out)?;
        }
        out
    };

    // Limit to commits that touch the pathspec, and remember which paths to diff.
    if !pathspec.is_empty() {
        commits.retain(|(_oid, commit)| commit_touches_pathspec(&repo, commit, &pathspec));
    }

    // `--grep` limits the series to commits whose message matches (like `git log
    // --grep`), honoring `grep.patternType` / `-F`/`-G`/`-E`/`-P`/`-i`.
    if !args.grep_patterns.is_empty() {
        let grep_res = crate::commands::log::compile_command_grep_regexes(
            &args.grep_patterns,
            args.fixed_strings,
            args.basic_regexp,
            args.extended_regexp,
            args.perl_regexp,
            args.regexp_ignore_case,
            &config,
        )?;
        commits.retain(|(_oid, commit)| grep_res.iter().any(|re| re.is_match(&commit.message)));
    }

    // Resolve threading / signature / cover-from-description / base / in-body-from settings
    // BEFORE the empty-commit short-circuit, because some need a non-empty series anyway.
    let total = commits.len();

    // Cover letter: explicit --cover-letter, format.coverletter=true, or auto when >1 patch.
    let cover_from_cli = args.cover_letter;
    let cover_config = config
        .get("format.coverletter")
        .or_else(|| config.get("format.coverLetter"))
        .map(|v| v.trim().to_ascii_lowercase());
    // --interdiff / --range-diff imply a cover letter (where the inter/range diff is placed)
    // when there is more than one patch in the series. With an explicit --no-cover-letter
    // (or format.coverLetter=no), git errors out rather than silently dropping the diff.
    let has_inter_or_range = args.interdiff.is_some() || args.range_diff.is_some();
    let cover_config_is_no = matches!(cover_config.as_deref(), Some("false" | "no" | "0" | "off"));
    if has_inter_or_range && total > 1 && (args.no_cover_letter || cover_config_is_no) {
        anyhow::bail!("--interdiff and --range-diff require --cover-letter or single patch");
    }
    let want_cover = if args.no_cover_letter {
        false
    } else if cover_from_cli {
        true
    } else if has_inter_or_range && total > 1 {
        true
    } else {
        match cover_config.as_deref() {
            Some("true" | "yes" | "1") => true,
            Some("auto") => total > 1,
            _ => false,
        }
    };

    if commits.is_empty() {
        // No commits: emit nothing (`cover-letter with nothing` expects empty output).
        return Ok(());
    }

    // Subject prefix: format.subjectprefix / --subject-prefix, then RFC handling.
    let mut prefix = args
        .subject_prefix
        .clone()
        .or_else(|| {
            config
                .get("format.subjectprefix")
                .or_else(|| config.get("format.subjectPrefix"))
        })
        .unwrap_or_else(|| "PATCH".to_owned());
    // When `--rfc` is repeated the last occurrence wins (matches git's option parsing). An
    // empty value (`--rfc=`) resets the RFC prefix to nothing, so no prefix is prepended.
    if let Some(rfc) = args.rfc.last() {
        if !args.no_rfc && !rfc.is_empty() {
            prefix = apply_rfc_prefix(&prefix, rfc);
        }
    }
    if let Some(ref v) = reroll {
        // Append the version to the prefix: "PATCH v4".
        if prefix.is_empty() {
            prefix = format!("v{v}");
        } else {
            prefix = format!("{prefix} v{v}");
        }
    }

    // Determine whether to number patches.
    let config_numbered_val = config
        .get("format.numbered")
        .map(|v| v.trim().to_ascii_lowercase());
    let config_numbered = matches!(config_numbered_val.as_deref(), Some("true" | "yes" | "1"));
    let use_numbering = if args.no_numbered {
        false
    } else if args.numbered || want_cover || config_numbered {
        true
    } else {
        total > 1
    };

    let start = args.start_number;
    let display_total = if start != 1 { start + total - 1 } else { total };

    // Resolve --base / format.useAutoBase prerequisite-patch-id trailers.
    let (base_commit, prereq_patch_ids) =
        resolve_base_and_prereqs(&repo, &config, &args, &commits)?;

    // Build merged To/Cc lists from config + command line.
    let mut to_list: Vec<String> = Vec::new();
    let mut cc_list: Vec<String> = Vec::new();
    let mut extra_headers: Vec<String> = Vec::new();

    if !args.no_add_header {
        for h in config.get_all("format.headers") {
            let h = h.trim_end_matches('\n').to_string();
            if h.is_empty() {
                continue;
            }
            if let Some(val) = h.strip_prefix("To:") {
                to_list.push(val.trim().to_string());
            } else if let Some(val) = h.strip_prefix("Cc:") {
                cc_list.push(val.trim().to_string());
            } else {
                extra_headers.push(h);
            }
        }
    }

    if !args.no_to {
        if let Some(to) = config.get("format.to") {
            to_list.push(to);
        }
    }
    if !args.no_cc {
        if let Some(cc) = config.get("format.cc") {
            cc_list.push(cc);
        }
    }

    to_list.extend(args.to.iter().cloned());
    cc_list.extend(args.cc.iter().cloned());
    // `--add-header` may carry To:/Cc: values; git folds those into the merged To/Cc headers
    // rather than emitting them as separate header lines (matching `format.headers` handling).
    for h in &args.add_header {
        let h = h.trim_end_matches('\n');
        if let Some(val) = h.strip_prefix("To:") {
            to_list.push(val.trim().to_string());
        } else if let Some(val) = h.strip_prefix("Cc:") {
            cc_list.push(val.trim().to_string());
        } else {
            extra_headers.push(h.to_string());
        }
    }

    // Validate --from ident (a bare word with no '@' is rejected by git).
    if let Some(ref from_arg) = args.from {
        if !from_arg.is_empty() && !is_valid_from_ident(from_arg) {
            anyhow::bail!("invalid ident line: {from_arg}");
        }
    }

    let from_header_mode = if args.no_from {
        // `--no-from` does not suppress the From header; it resets it to the commit author
        // (overriding any format.from / --from), exactly like `format.from=false`.
        FromHeaderMode::Author
    } else if let Some(ref from_arg) = args.from {
        if from_arg.is_empty() {
            FromHeaderMode::Committer
        } else {
            FromHeaderMode::Custom(from_arg.clone())
        }
    } else {
        match config
            .get("format.from")
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            None => FromHeaderMode::Author,
            Some("true" | "yes" | "1") => FromHeaderMode::Committer,
            Some("false" | "no" | "0") => FromHeaderMode::Author,
            Some(s) => FromHeaderMode::Custom(s.to_owned()),
        }
    };

    // Threading mode.
    let thread_mode = resolve_thread_mode(&config, &args);

    // Signature.
    let git_version = git_version_string();
    let signature = resolve_signature(&config, &args, &git_version)?;

    // encode-email-headers (default true).
    let encode_email_headers = if args.no_encode_email_headers {
        false
    } else if args.encode_email_headers {
        true
    } else {
        config
            .get("format.encodeemailheaders")
            .or_else(|| config.get("format.encodeEmailHeaders"))
            .and_then(|v| parse_bool(&v).ok())
            .unwrap_or(true)
    };

    // force-in-body-from.
    let force_in_body_from = if args.no_force_in_body_from {
        false
    } else if args.force_in_body_from {
        true
    } else {
        config
            .get("format.forceinbodyfrom")
            .or_else(|| config.get("format.forceInBodyFrom"))
            .and_then(|v| parse_bool(&v).ok())
            .unwrap_or(false)
    };

    // mboxrd: --mboxrd, --pretty=mboxrd, or format.mboxrd.
    let mboxrd = args.mboxrd
        || args.pretty.as_deref() == Some("mboxrd")
        || config
            .get("format.mboxrd")
            .and_then(|v| parse_bool(&v).ok())
            .unwrap_or(false);

    // --relative=<dir>.
    let relative = if args.no_relative {
        None
    } else if let Some(ref r) = args.relative {
        if r.is_empty() {
            // bare --relative: relative to cwd within the worktree.
            relative_prefix_from_cwd(&repo)
        } else {
            Some(ensure_trailing_slash(r))
        }
    } else if config
        .get("diff.relative")
        .and_then(|v| parse_bool(&v).ok())
        .unwrap_or(false)
    {
        relative_prefix_from_cwd(&repo)
    } else {
        None
    };

    let context_lines = args
        .unified
        .or(args.context_lines)
        .or_else(|| {
            config
                .get("diff.context")
                .and_then(|v| v.trim().parse::<usize>().ok())
        })
        .unwrap_or(3);

    let opts = PatchOptions {
        in_reply_to: args.in_reply_to.clone(),
        cc: cc_list,
        to: to_list,
        extra_headers,
        from_header: from_header_mode,
        signoff: args.signoff,
        attach: resolve_attach(&args, &config),
        inline: args.inline.clone(),
        keep_subject: args.keep_subject,
        base_commit,
        prereq_patch_ids,
        order_file: args.order_file.clone(),
        stat_width,
        stat_name_width,
        stat_graph_width,
        stat_count: args.stat_count,
        format_patch_graph: args.graph,
        diff_src_prefix,
        diff_dst_prefix,
        context_lines,
        signature,
        encode_email_headers,
        force_in_body_from,
        mboxrd,
        relative,
        zero_commit: args.zero_commit,
        // `-p`/`--patch` suppresses the diffstat (unless an explicit stat form is requested).
        suppress_stat: args.patch && args.stat.is_none() && !args.numstat && !args.shortstat,
        // Human-readable diffstat is shown by default; it is also shown when `--stat` is given
        // explicitly. When only `--numstat`/`--shortstat` are requested (without `--stat`), the
        // human diffstat is NOT shown.
        want_stat: !(args.patch && args.stat.is_none() && !args.numstat && !args.shortstat)
            && (args.stat.is_some() || (!args.numstat && !args.shortstat)),
        want_numstat: args.numstat,
        want_shortstat: args.shortstat,
        notes_refs: resolve_notes_refs(&args, &config),
        pathspec: pathspec.clone(),
        rename_threshold,
    };

    let mut log_output_encoding = config
        .get("i18n.logOutputEncoding")
        .or_else(|| config.get("i18n.logoutputencoding"))
        .unwrap_or_else(|| "UTF-8".to_owned());
    if let Some(enc) = args.encoding.as_deref() {
        let t = enc.trim();
        if !t.is_empty() {
            log_output_encoding = t.to_owned();
        }
    }

    // Output directory: --output-directory / format.outputDirectory (ignored with --stdout/--output).
    let out_dir = if args.stdout || args.output.is_some() {
        std::env::current_dir().context("cannot determine current directory")?
    } else if let Some(ref dir) = args.output_directory {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("cannot create output directory '{}'", dir.display()))?;
        dir.clone()
    } else if let Some(cfg_dir) = config
        .get("format.outputdirectory")
        .or_else(|| config.get("format.outputDirectory"))
    {
        let dir = PathBuf::from(cfg_dir);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create output directory '{}'", dir.display()))?;
        dir
    } else {
        std::env::current_dir().context("cannot determine current directory")?
    };

    // Reroll filename prefix: `v<x>-`.
    let file_prefix = reroll
        .as_deref()
        .map(|v| format!("v{}-", sanitize_reroll(v)))
        .unwrap_or_default();

    // Patch filename suffix (default `.patch`; `--suffix=` overrides, e.g. `.diff`).
    let patch_suffix: &str = args.suffix.as_deref().unwrap_or(".patch");

    // Message-Id chain state for threading.
    let mut thread = ThreadState::new(thread_mode, opts.in_reply_to.clone(), want_cover);

    // Collect output into a buffer (for --output single-file or stdout).
    let mut single_buf = String::new();
    let to_single = args.stdout || args.output.is_some();

    // Cover letter description (subject + blurb).
    let cover_desc = if want_cover {
        Some(resolve_cover_description(&repo, &config, &args, &commits)?)
    } else {
        None
    };

    // Cover-letter (patch 0/N).
    if want_cover {
        let msg_id = thread.next_message_id(&commits, 0);
        let cover_subject = build_cover_subject(
            &prefix,
            use_numbering,
            display_total,
            start,
            cover_desc.as_ref(),
            encode_email_headers,
            &log_output_encoding,
        );
        let cover = format_cover_letter(
            &repo,
            &commits,
            &cover_subject,
            cover_desc.as_ref(),
            &opts,
            &log_output_encoding,
            &msg_id,
            thread.in_reply_to_for(0),
            &thread.references_for(0),
            args.interdiff.as_deref(),
            args.range_diff.as_deref(),
            reroll.as_deref(),
            args.creation_factor,
        )?;
        emit_output(
            &mut single_buf,
            to_single,
            &out_dir,
            &format!("{file_prefix}0000-cover-letter{patch_suffix}"),
            &cover,
            args.quiet,
        )?;
    }

    let is_last_patch = |idx: usize| idx + 1 == total;

    for (idx, (oid, commit)) in commits.iter().enumerate() {
        let patch_num = start + idx;
        let seq = if want_cover { idx + 1 } else { idx };
        let msg_id = thread.next_message_id(&commits, seq);

        let display_msg = commit_message_unicode_for_display(
            commit.encoding.as_deref(),
            &commit.message,
            commit.raw_message.as_deref(),
        );
        let subject_line = flatten_subject(&display_msg);

        let subject = build_patch_subject(
            &prefix,
            opts.keep_subject,
            use_numbering,
            patch_num,
            display_total,
            &subject_line,
        );

        let include_base = is_last_patch(idx);
        // When there is no cover letter, an --interdiff / --range-diff is appended to the
        // single (last) patch, with its body indented by two spaces.
        let solo_extra = if !want_cover && is_last_patch(idx) {
            build_solo_interdiff_block(
                &repo,
                &commits,
                &opts,
                args.interdiff.as_deref(),
                args.range_diff.as_deref(),
                reroll.as_deref(),
                args.creation_factor,
            )?
        } else {
            None
        };
        // Git derives the patch filename from only the FIRST line of the subject (it stops at the
        // first newline), even though the Subject header flattens a multi-line subject onto one line.
        let filename_subject = first_subject_line(&display_msg);
        // `--numbered-files`: the patch file (and its MIME attachment name) is just the
        // sequence number, with no prefix, subject, or suffix (matches Git).
        let filename = if args.numbered_files {
            patch_num.to_string()
        } else {
            build_patch_filename(
                &file_prefix,
                patch_num,
                filename_subject,
                filename_max_length,
                patch_suffix,
            )
        };

        let patch = format_single_patch(
            &repo,
            &repo.odb,
            repo.git_dir.as_path(),
            oid,
            commit,
            &subject,
            &opts,
            include_base,
            &log_output_encoding,
            args.no_binary,
            &msg_id,
            thread.in_reply_to_for(seq),
            &thread.references_for(seq),
            encode_email_headers,
            solo_extra.as_deref(),
            &filename,
        )?;
        emit_output(
            &mut single_buf,
            to_single,
            &out_dir,
            &filename,
            &patch,
            args.quiet,
        )?;
        // Single-file (stdout/--output): one blank line between patches.
        if to_single && idx + 1 < total {
            single_buf.push('\n');
        }
    }

    if args.stdout {
        let mut out = stdout_handle_lock();
        write!(out, "{single_buf}")?;
    } else if let Some(ref outfile) = args.output {
        std::fs::write(outfile, &single_buf)
            .with_context(|| format!("cannot write output file '{}'", outfile.display()))?;
    }

    Ok(())
}

fn stdout_handle_lock() -> io::StdoutLock<'static> {
    io::stdout().lock()
}

/// Write a patch either into the single-file buffer or as a standalone file (printing its path).
fn emit_output(
    single_buf: &mut String,
    to_single: bool,
    out_dir: &std::path::Path,
    filename: &str,
    content: &str,
    quiet: bool,
) -> Result<()> {
    if to_single {
        single_buf.push_str(content);
    } else {
        let path = out_dir.join(filename);
        std::fs::write(&path, content)
            .with_context(|| format!("cannot write patch file '{}'", path.display()))?;
        if !quiet {
            // Print the path relative to the current directory when possible (Git prints the
            // path as the user would refer to it; absolute output dirs stay absolute).
            println!("{}", display_output_path(out_dir, filename));
        }
    }
    Ok(())
}

/// Render the path Git would print: `<out_dir>/<filename>` made relative to cwd when `out_dir`
/// is inside the cwd, else as given.
fn display_output_path(out_dir: &std::path::Path, filename: &str) -> String {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let full = out_dir.join(filename);
    if let Ok(rel) = full.strip_prefix(&cwd) {
        return rel.display().to_string();
    }
    full.display().to_string()
}

/// Commits for `git format-patch --cherry-pick --right-only A...B` (`rebase --apply` path).
fn collect_cherry_pick_right_only_commits(
    repo: &Repository,
    range_spec: &str,
    extra_excludes: &[String],
    topo_order: bool,
) -> Result<Vec<(ObjectId, CommitData)>> {
    let Some((lhs, rhs)) = split_symmetric_diff(range_spec) else {
        anyhow::bail!("expected symmetric revision range A...B, got '{range_spec}'");
    };
    let left_tip = if lhs.is_empty() {
        resolve_revision_for_range_end(repo, "HEAD")?
    } else {
        resolve_revision_for_range_end(repo, &lhs)?
    };
    let right_tip = if rhs.is_empty() {
        resolve_revision_for_range_end(repo, "HEAD")?
    } else {
        resolve_revision_for_range_end(repo, &rhs)?
    };
    let bases = merge_bases_first_vs_rest(repo, left_tip, &[right_tip])?;
    let mut negative: Vec<String> = bases.iter().map(|b| b.to_hex()).collect();
    for e in extra_excludes {
        let oid = resolve_revision_for_range_end(repo, e)?;
        negative.push(oid.to_hex());
    }
    let ordering = if topo_order {
        OrderingMode::Topo
    } else {
        OrderingMode::Default
    };
    let result = rev_list(
        repo,
        &[left_tip.to_hex(), right_tip.to_hex()],
        &negative,
        &RevListOptions {
            cherry_pick: true,
            right_only: true,
            left_right: true,
            symmetric_left: Some(left_tip),
            symmetric_right: Some(right_tip),
            ordering,
            ..Default::default()
        },
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut oids = result.commits;
    oids.reverse();
    let mut commits = Vec::new();
    for oid in oids {
        let obj = repo.odb.read(&oid)?;
        let c = parse_commit(&obj.data)?;
        if c.parents.len() > 1 {
            continue;
        }
        commits.push((oid, c));
    }
    Ok(commits)
}

/// If the first revision token is `-N`, strip it and return `Some(N)` (Git `format-patch -3`).
fn strip_leading_neg_count(tokens: &mut Vec<String>) -> Option<usize> {
    let Some(first) = tokens.first() else {
        return None;
    };
    let rest = first.strip_prefix('-')?;
    let n: usize = rest.parse().ok()?;
    tokens.remove(0);
    Some(n)
}

/// Resolve revision argv like Git `format-patch`: `rev_list` with `max_parents=1`, reversed for
/// patch order. Returns positive/negative spec strings for `--ignore-if-in-upstream`.
fn collect_commits_for_format_patch(
    repo: &Repository,
    rev_tokens: &[String],
    exclude_rest: &[String],
    max_count: Option<usize>,
    topo_order: bool,
) -> Result<(Vec<(ObjectId, CommitData)>, Vec<String>, Vec<String>)> {
    let mut positive: Vec<String> = Vec::new();
    let mut negative: Vec<String> = Vec::new();

    if rev_tokens.is_empty() {
        positive.push("HEAD".to_owned());
    } else {
        for t in rev_tokens {
            let (pos, neg) = split_revision_token(t);
            positive.extend(pos);
            negative.extend(neg);
        }
    }
    // `^rev` exclusions on the command line become negative endpoints.
    negative.extend(exclude_rest.iter().cloned());

    if positive.is_empty() {
        positive.push("HEAD".to_owned());
    }

    // `git format-patch <since>` with a single committish: same as `<since>..HEAD` (see
    // `setup_revisions`). This rewrite only applies when no explicit commit count is given.
    // With an explicit `-N` count (e.g. `git format-patch -1 <commit>`) the committish is the
    // positive endpoint itself — format `<commit>` and its N-1 ancestors — per
    // git-format-patch docs ("If you want to format only <commit> itself ... `-1 <commit>`").
    if max_count.is_none() && negative.is_empty() && positive.len() == 1 {
        let spec = positive[0].trim();
        if spec != "HEAD"
            && !spec.is_empty()
            && grit_lib::rev_parse::split_double_dot_range(spec).is_none()
        {
            let since = positive[0].clone();
            positive[0] = "HEAD".to_owned();
            negative.push(since);
        }
    }

    let ordering = if topo_order {
        OrderingMode::Topo
    } else {
        OrderingMode::Default
    };

    let opts = RevListOptions {
        max_parents: Some(1),
        reverse: true,
        ordering,
        max_count,
        ..Default::default()
    };

    let result = rev_list(repo, &positive, &negative, &opts).map_err(|e| anyhow::anyhow!("{e}"))?;

    let mut commits = Vec::with_capacity(result.commits.len());
    for oid in result.commits {
        let obj = repo.odb.read(&oid).context("reading commit")?;
        let c = parse_commit(&obj.data).context("parsing commit")?;
        commits.push((oid, c));
    }

    Ok((commits, positive, negative))
}

/// Drop commits whose patch-id already appears on the other side of a two-endpoint range.
fn filter_ignore_if_in_upstream(
    repo: &Repository,
    positive: &[String],
    negative: &[String],
    commits: &mut Vec<(ObjectId, CommitData)>,
) -> Result<()> {
    if positive.len() != 1 || negative.len() != 1 {
        // Without a two-endpoint range (e.g. just `HEAD`) there is no upstream to compare
        // patch-ids against, so git simply formats every commit without filtering anything.
        return Ok(());
    }
    // Match `get_patch_ids` in Git: collect patch-ids from commits reachable from the range's
    // left endpoint but not from its right (`main..side` → `main` minus `side`).
    let check_pos = vec![negative[0].clone()];
    let check_neg = vec![positive[0].clone()];
    let ids_result = rev_list(
        repo,
        &check_pos,
        &check_neg,
        &RevListOptions {
            max_parents: Some(1),
            ..Default::default()
        },
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    let mut upstream_patch_ids = HashSet::new();
    for oid in ids_result.commits {
        if let Ok(Some(pid)) = compute_patch_id(&repo.odb, &oid) {
            upstream_patch_ids.insert(pid);
        }
    }

    commits.retain(|(oid, _)| {
        if let Ok(Some(pid)) = compute_patch_id(&repo.odb, oid) {
            !upstream_patch_ids.contains(&pid)
        } else {
            true
        }
    });
    Ok(())
}

/// Collect all commits from root up to the given revision (for --root).
fn collect_root_commits(repo: &Repository, revision: &str) -> Result<Vec<(ObjectId, CommitData)>> {
    // If revision is a negative count, just use that
    if let Some(count_str) = revision.strip_prefix('-') {
        if let Ok(count) = count_str.parse::<usize>() {
            return collect_last_n_commits(repo, count);
        }
    }

    // Resolve the target
    let target_oid = resolve_revision(repo, revision)
        .with_context(|| format!("unknown revision '{revision}'"))?;

    // Walk all the way back to root
    let mut commits = Vec::new();
    let mut current = target_oid;

    loop {
        let obj = repo.odb.read(&current).context("reading commit")?;
        let commit = parse_commit(&obj.data).context("parsing commit")?;
        let parent = commit.parents.first().copied();
        commits.push((current, commit));
        match parent {
            Some(p) => current = p,
            None => break,
        }
    }

    commits.reverse();
    Ok(commits)
}

/// Collect the last N commits from HEAD.
fn collect_last_n_commits(repo: &Repository, count: usize) -> Result<Vec<(ObjectId, CommitData)>> {
    let head_oid = resolve_head_oid(repo)?;
    let mut commits = Vec::new();
    let mut current = head_oid;

    for _ in 0..count {
        let obj = repo.odb.read(&current).context("reading commit")?;
        let commit = parse_commit(&obj.data).context("parsing commit")?;
        let parent = commit.parents.first().copied();
        commits.push((current, commit));
        match parent {
            Some(p) => current = p,
            None => break,
        }
    }

    commits.reverse();
    Ok(commits)
}

/// Collect exactly one commit identified by `revision`.
fn collect_single_commit(repo: &Repository, revision: &str) -> Result<Vec<(ObjectId, CommitData)>> {
    let oid = resolve_revision(repo, revision)
        .with_context(|| format!("unknown revision '{revision}'"))?;
    let obj = repo.odb.read(&oid).context("reading commit")?;
    let commit = parse_commit(&obj.data).context("parsing commit")?;
    Ok(vec![(oid, commit)])
}

fn diffstat_for_patch_entries(
    odb: &Odb,
    entries: &[grit_lib::diff::DiffEntry],
    opts: &PatchOptions,
) -> Result<String> {
    let mut files: Vec<FileStatInput> = Vec::with_capacity(entries.len());
    for entry in entries {
        // Renames/copies are displayed in the diffstat as `old => new` (pretty-printed).
        let path = match entry.status {
            DiffStatus::Renamed | DiffStatus::Copied => {
                let old = entry.old_path.as_deref().map(RepoPath::to_string_lossy).unwrap_or_default();
                let new = entry.new_path.as_deref().map(RepoPath::to_string_lossy).unwrap_or_default();
                grit_lib::diff::format_rename_path(&old, &new)
            }
            _ => entry.path().to_string(),
        };
        let old_raw = read_blob_raw(odb, &entry.old_oid);
        let new_raw = read_blob_raw(odb, &entry.new_oid);
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
                path_display: path,
                insertions: added,
                deletions: deleted,
                is_binary: true,
                is_unmerged: false,
            });
        } else {
            let old_content = String::from_utf8_lossy(&old_raw).into_owned();
            let new_content = String::from_utf8_lossy(&new_raw).into_owned();
            let (ins, del) = count_changes(&old_content, &new_content);
            files.push(FileStatInput {
                path_display: path,
                insertions: ins,
                deletions: del,
                is_binary: false,
                is_unmerged: false,
            });
        }
    }
    let mut out = String::new();

    // `--numstat`: machine-readable `ins\tdel\tpath` lines (binary files use `-\t-\tpath`).
    if opts.want_numstat {
        for (file, entry) in files.iter().zip(entries.iter()) {
            let display = match entry.status {
                DiffStatus::Renamed | DiffStatus::Copied => {
                    let old = entry.old_path.as_deref().map(RepoPath::to_string_lossy).unwrap_or_default();
                    let new = entry.new_path.as_deref().map(RepoPath::to_string_lossy).unwrap_or_default();
                    let pretty = grit_lib::diff::format_rename_path(&old, &new);
                    grit_lib::quote_path::quote_c_style(&pretty, false)
                }
                _ => file.path_display.clone(),
            };
            if file.is_binary {
                out.push_str(&format!("-\t-\t{display}\n"));
            } else {
                out.push_str(&format!(
                    "{}\t{}\t{}\n",
                    file.insertions, file.deletions, display
                ));
            }
        }
    }

    // `--shortstat`: single summary line.
    if opts.want_shortstat && !files.is_empty() {
        let total_ins: usize = files
            .iter()
            .filter(|f| !f.is_binary)
            .map(|f| f.insertions)
            .sum();
        let total_del: usize = files
            .iter()
            .filter(|f| !f.is_binary)
            .map(|f| f.deletions)
            .sum();
        let files_changed = files.len();
        let mut summary = format!(
            " {} file{} changed",
            files_changed,
            if files_changed == 1 { "" } else { "s" }
        );
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
        out.push_str(&summary);
        out.push('\n');
    }

    // Human-readable `--stat` diffstat block.
    if opts.want_stat {
        let line_prefix = if opts.format_patch_graph { "|  " } else { "" };
        let dstat_opts = DiffstatOptions {
            total_width: opts.stat_width,
            line_prefix,
            width_prefix: "",
            subtract_prefix_from_terminal: false,
            stat_name_width: opts.stat_name_width,
            stat_graph_width: opts.stat_graph_width,
            stat_count: opts.stat_count,
            color_add: "",
            color_del: "",
            color_reset: "",
            graph_bar_slack: 0,
            graph_prefix_budget_slack: 0,
        };
        let mut buf = Vec::new();
        write_diffstat_block(&mut buf, &files, &dstat_opts)?;
        out.push_str(&String::from_utf8_lossy(&buf));

        // git format-patch emits `--summary` lines (create/delete mode, mode change,
        // rename/copy) right after the diffstat.
        out.push_str(&format_summary_lines(entries));
    }

    Ok(out)
}

/// `--summary` lines for the diffstat (git's `show_stats`/`show_rename_copy`): file
/// creations, deletions, mode changes, and renames/copies.
fn format_summary_lines(entries: &[grit_lib::diff::DiffEntry]) -> String {
    use grit_lib::diff::DiffStatus;
    let mut out = String::new();
    for entry in entries {
        match entry.status {
            DiffStatus::Added => {
                out.push_str(&format!(
                    " create mode {} {}\n",
                    entry.new_mode,
                    entry.path()
                ));
            }
            DiffStatus::Deleted => {
                out.push_str(&format!(
                    " delete mode {} {}\n",
                    entry.old_mode,
                    entry.path()
                ));
            }
            DiffStatus::Modified | DiffStatus::TypeChanged if entry.old_mode != entry.new_mode => {
                out.push_str(&format!(
                    " mode change {} => {} {}\n",
                    entry.old_mode,
                    entry.new_mode,
                    entry.path()
                ));
            }
            DiffStatus::Renamed => {
                let sim = entry.score.unwrap_or(100);
                out.push_str(&format!(
                    " rename {} => {} ({sim}%)\n",
                    entry.old_path.as_deref().unwrap_or(RepoPath::from_str("")),
                    entry.new_path.as_deref().unwrap_or(RepoPath::from_str(""))
                ));
            }
            DiffStatus::Copied => {
                let sim = entry.score.unwrap_or(100);
                out.push_str(&format!(
                    " copy {} => {} ({sim}%)\n",
                    entry.old_path.as_deref().unwrap_or(RepoPath::from_str("")),
                    entry.new_path.as_deref().unwrap_or(RepoPath::from_str(""))
                ));
            }
            _ => {}
        }
    }
    out
}

fn read_blob_raw(odb: &Odb, oid: &ObjectId) -> Vec<u8> {
    if *oid == zero_oid() {
        return Vec::new();
    }
    odb.read(oid).map(|o| o.data).unwrap_or_default()
}

/// Resolve HEAD to an ObjectId.
fn resolve_head_oid(repo: &Repository) -> Result<ObjectId> {
    let head = grit_lib::state::resolve_head(&repo.git_dir).context("cannot resolve HEAD")?;
    head.oid()
        .copied()
        .ok_or_else(|| anyhow::anyhow!("HEAD is unborn"))
}

/// Generate a cover letter for a patch series.
#[allow(clippy::too_many_arguments)]
fn format_cover_letter(
    repo: &Repository,
    commits: &[(ObjectId, CommitData)],
    subject: &str,
    cover_desc: Option<&CoverDescription>,
    patch_opts: &PatchOptions,
    log_output_encoding: &str,
    message_id: &str,
    in_reply_to: Option<&str>,
    references: &[String],
    interdiff: Option<&str>,
    range_diff: Option<&str>,
    reroll: Option<&str>,
    creation_factor: Option<usize>,
) -> Result<String> {
    let mut out = String::new();

    // Use the last commit's info for From/Date
    let (last_oid, last_commit) = commits
        .last()
        .ok_or_else(|| anyhow::anyhow!("cannot format cover letter for empty commit series"))?;

    let cover_from_oid = if patch_opts.zero_commit {
        "0".repeat(last_oid.to_hex().len())
    } else {
        last_oid.to_hex()
    };
    out.push_str(&format!("From {cover_from_oid} Mon Sep 17 00:00:00 2001\n"));

    // Git's cover letter uses the committer identity (`git_committer_info(0)`) for the
    // `From`/`Date` headers, not the last commit's author. A `--from=<custom>` overrides
    // only the address, keeping the committer date.
    let committer_ident = current_committer_ident_line(repo);
    let charset_label = rfc2047_charset_label(log_output_encoding);
    let use_utf8_log = charset_label.eq_ignore_ascii_case("UTF-8");
    if !matches!(patch_opts.from_header, FromHeaderMode::Omit) {
        let mailbox = match &patch_opts.from_header {
            FromHeaderMode::Custom(s) => {
                format_ident(&grit_lib::commit_encoding::decode_rfc2047_mailbox_from_line(s))
            }
            _ => format_ident(
                &grit_lib::commit_encoding::decode_rfc2047_mailbox_from_line(&committer_ident),
            ),
        };
        write_addr_header(
            &mut out,
            "From",
            &mailbox,
            patch_opts.encode_email_headers,
            &charset_label,
        );
    }

    let date = format_date_rfc2822(&committer_ident);
    out.push_str(&format!("Date: {date}\n"));

    // Subject is pre-built; encode/fold it.
    write_subject_header(
        &mut out,
        subject,
        patch_opts.encode_email_headers,
        &charset_label,
    );

    write_thread_headers(&mut out, message_id, in_reply_to, references);
    write_recipient_headers(&mut out, patch_opts);

    // Body description (blurb).
    let blurb = cover_desc
        .and_then(|d| d.body.clone())
        .unwrap_or_else(|| "*** BLURB HERE ***".to_owned());
    let body_has_non_ascii = blurb.bytes().any(|b| b > 127);
    if !use_utf8_log || (patch_opts.encode_email_headers && body_has_non_ascii) {
        out.push_str("MIME-Version: 1.0\n");
        out.push_str(&format!(
            "Content-Type: text/plain; charset={charset_label}\n"
        ));
        out.push_str("Content-Transfer-Encoding: 8bit\n");
    }
    out.push('\n');
    out.push_str(&blurb);
    out.push('\n');
    out.push('\n');

    // Shortlog (grouped by author, with counts and wrapped onelines).
    out.push_str(&shortlog_block(commits));

    // Diffstat across all commits
    let first_parent_tree = commits.first().and_then(|(_oid, commit)| {
        commit.parents.first().and_then(|parent_oid| {
            repo.odb
                .read(parent_oid)
                .ok()
                .and_then(|obj| parse_commit(&obj.data).ok())
                .map(|c| c.tree)
        })
    });
    let last_tree = &last_commit.tree;

    let mut diff_entries = diff_trees(&repo.odb, first_parent_tree.as_ref(), Some(last_tree), "")
        .context("computing diff for cover letter")?;
    if let Some(threshold) = patch_opts.rename_threshold {
        diff_entries = grit_lib::diff::detect_renames(&repo.odb, None, diff_entries, threshold);
    }
    if !patch_opts.pathspec.is_empty() {
        diff_entries.retain(|e| {
            patch_opts
                .pathspec
                .iter()
                .any(|ps| path_matches_spec(&e.path().to_string_lossy(), ps))
        });
    }
    let diff_entries = apply_relative_filter(diff_entries, patch_opts.relative.as_deref());

    out.push_str(&diffstat_for_patch_entries(
        &repo.odb,
        &diff_entries,
        patch_opts,
    )?);

    // Interdiff / Range-diff blocks. In the cover letter the inter/range diff body is NOT
    // indented (unlike the solo-patch case where it is indented by two spaces), and the
    // signature line follows the diff directly (no trailing blank line).
    let mut had_extra = false;
    if let Some(spec) = interdiff {
        out.push('\n');
        let prev_ver = reroll.and_then(prev_version_label);
        match prev_ver {
            Some(v) => out.push_str(&format!("Interdiff against {v}:\n")),
            None => out.push_str("Interdiff:\n"),
        }
        let body = compute_interdiff(repo, spec, commits, patch_opts)?;
        out.push_str(&body);
        had_extra = true;
    }
    if let Some(spec) = range_diff {
        out.push('\n');
        let prev_ver = reroll.and_then(prev_version_label);
        match prev_ver {
            Some(v) => out.push_str(&format!("Range-diff against {v}:\n")),
            None => out.push_str("Range-diff:\n"),
        }
        let body = compute_range_diff(repo, spec, commits, creation_factor, patch_opts)?;
        out.push_str(&body);
        had_extra = true;
    }
    if !had_extra {
        out.push('\n');
    }

    write_signature(&mut out, patch_opts.signature.as_deref());

    Ok(out)
}

/// Build the shortlog block for a cover letter: group consecutive commits by author, print
/// `  Author (N):` then each oneline indented `    oneline` (wrapping long onelines at ~74 cols).
fn shortlog_block(commits: &[(ObjectId, CommitData)]) -> String {
    let mut out = String::new();
    // Git's shortlog groups by author across the whole series (not just consecutive).
    let mut order: Vec<String> = Vec::new();
    let mut groups: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for (_oid, commit) in commits {
        let display_msg = commit_message_unicode_for_display(
            commit.encoding.as_deref(),
            &commit.message,
            commit.raw_message.as_deref(),
        );
        let oneline = flatten_subject(&display_msg);
        let decoded_author =
            grit_lib::commit_encoding::decode_rfc2047_mailbox_from_line(&commit.author);
        let author_name = if let Some(bracket) = decoded_author.find('<') {
            decoded_author[..bracket].trim().to_string()
        } else {
            decoded_author.clone()
        };
        groups.entry(author_name.clone()).or_default().push(oneline);
        if !order.contains(&author_name) {
            order.push(author_name);
        }
    }
    for author in &order {
        let lines = &groups[author];
        out.push_str(&format!("{author} ({}):\n", lines.len()));
        for line in lines {
            // Wrap onelines like git: indent 4, wrap to ~74 cols width with hanging indent 6.
            out.push_str(&wrap_oneline(line));
        }
        out.push('\n');
    }
    out
}

/// Wrap a shortlog oneline like Git: first line `    <text...>`, continuation `      <text...>`,
/// wrapping at a 72-column overall width.
fn wrap_oneline(text: &str) -> String {
    const WIDTH: usize = 72;
    let words: Vec<&str> = text.split(' ').collect();
    let mut out = String::new();
    let mut col;
    let indent1 = "  ";
    let indent2 = "    ";
    let mut line = String::from(indent1);
    col = indent1.len();
    let mut first_word = true;
    for w in words {
        let add = if first_word { w.len() } else { w.len() + 1 };
        if !first_word && col + add > WIDTH {
            out.push_str(&line);
            out.push('\n');
            line = String::from(indent2);
            col = indent2.len();
            line.push_str(w);
            col += w.len();
        } else {
            if !first_word {
                line.push(' ');
                col += 1;
            }
            line.push_str(w);
            col += w.len();
        }
        first_word = false;
    }
    out.push_str(&line);
    out.push('\n');
    out
}

/// Build the current committer ident line (`Name <email> <epoch> <tz>`) from the
/// `GIT_COMMITTER_*` environment and config, matching Git's `git_committer_info(0)`.
/// Used for the cover letter's `From`/`Date` headers (Git uses the committer, not the
/// last commit's author).
fn current_committer_ident_line(repo: &Repository) -> String {
    let config = grit_lib::config::ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();

    let name = match read_git_identity_name_env("GIT_COMMITTER_NAME") {
        GitIdentityNameEnv::Set(s) if !s.trim().is_empty() => s,
        _ => config
            .get("user.name")
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "Unknown".to_owned()),
    };
    let email = std::env::var("GIT_COMMITTER_EMAIL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| config.get("user.email").filter(|s| !s.trim().is_empty()))
        .unwrap_or_default();

    let date = std::env::var("GIT_COMMITTER_DATE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .and_then(|d| grit_lib::commit::parse_date_to_git_timestamp(&d).or(Some(d)))
        .unwrap_or_else(|| {
            let now = time::OffsetDateTime::now_utc();
            let off = now.offset();
            format!(
                "{} {:+03}{:02}",
                now.unix_timestamp(),
                off.whole_hours(),
                off.minutes_past_hour().unsigned_abs()
            )
        });

    format!("{name} <{email}> {date}")
}

/// Get the signoff identity, preferring GIT_COMMITTER_NAME/EMAIL env vars.
fn get_signoff_identity(committer_ident: &str) -> (String, String) {
    let env_email = std::env::var("GIT_COMMITTER_EMAIL").ok();

    let name = match read_git_identity_name_env("GIT_COMMITTER_NAME") {
        GitIdentityNameEnv::Set(s) if !s.is_empty() => s,
        _ => {
            if let Some(bracket) = committer_ident.find('<') {
                committer_ident[..bracket].trim().to_string()
            } else {
                "Unknown".to_string()
            }
        }
    };
    let email = env_email.unwrap_or_else(|| {
        extract_email(committer_ident)
            .unwrap_or("unknown")
            .to_string()
    });

    (name, email)
}

/// Extract the email portion from an ident string like "Name <email> ts tz".
fn extract_email(ident: &str) -> Option<&str> {
    let start = ident.find('<')?;
    let end = ident.find('>')?;
    Some(&ident[start + 1..end])
}

/// Format a single commit as an email-style patch.
#[allow(clippy::too_many_arguments)]
fn format_single_patch(
    repo: &Repository,
    odb: &Odb,
    git_dir: &std::path::Path,
    oid: &ObjectId,
    commit: &CommitData,
    subject: &str,
    opts: &PatchOptions,
    include_base: bool,
    log_output_encoding: &str,
    no_binary: bool,
    message_id: &str,
    in_reply_to: Option<&str>,
    references: &[String],
    encode_email_headers: bool,
    solo_extra: Option<&str>,
    attachment_filename: &str,
) -> Result<String> {
    let mut out = String::new();
    let charset_label = rfc2047_charset_label(log_output_encoding);
    let use_utf8_log = charset_label.eq_ignore_ascii_case("UTF-8");
    let commit_msg_unicode = commit_message_unicode_for_display(
        commit.encoding.as_deref(),
        &commit.message,
        commit.raw_message.as_deref(),
    );

    // Generate the diff first (needed for MIME attachment)
    let parent_tree = commit.parents.first().map(|parent_oid| {
        odb.read(parent_oid)
            .ok()
            .and_then(|obj| parse_commit(&obj.data).ok())
            .map(|c| c.tree)
    });
    let parent_tree_oid: Option<ObjectId> = parent_tree.flatten();

    let mut diff_entries_raw = diff_trees(odb, parent_tree_oid.as_ref(), Some(&commit.tree), "")
        .context("computing diff")?;
    // Rename detection (format-patch enables it by default; `--no-renames` disables).
    if let Some(threshold) = opts.rename_threshold {
        diff_entries_raw = grit_lib::diff::detect_renames(odb, None, diff_entries_raw, threshold);
    }
    // Restrict each patch's diff to the pathspec (`format-patch -- <path>...`).
    if !opts.pathspec.is_empty() {
        diff_entries_raw.retain(|e| {
            opts.pathspec
                .iter()
                .any(|ps| path_matches_spec(&e.path().to_string_lossy(), ps))
        });
    }
    let diff_entries_raw = apply_relative_filter(diff_entries_raw, opts.relative.as_deref());
    let diff_entries = if let Some(ref order_path) = opts.order_file {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        grit_lib::diff::apply_orderfile_entries(diff_entries_raw, order_path, &cwd)?
    } else {
        diff_entries_raw
    };
    let config = ConfigSet::load(Some(git_dir), true).unwrap_or_default();

    // Build stat + full diff into separate string
    let mut diff_text = String::new();
    if !opts.suppress_stat {
        diff_text.push_str(&diffstat_for_patch_entries(odb, &diff_entries, opts)?);
        diff_text.push('\n');
    }

    for entry in &diff_entries {
        let old_path = entry
            .old_path
            .as_deref()
            .map_or(std::borrow::Cow::Borrowed("/dev/null"), RepoPath::to_string_lossy);
        let new_path = entry
            .new_path
            .as_deref()
            .map_or(std::borrow::Cow::Borrowed("/dev/null"), RepoPath::to_string_lossy);
        write_diff_header_to_string(
            &mut diff_text,
            entry,
            opts.diff_src_prefix,
            opts.diff_dst_prefix,
        );
        // TODO(byte-paths): attribute/textconv lookup is still str-keyed (Phase 3).
        let path_for_attrs_owned = entry.path().to_string_lossy();
        let path_for_attrs: &str = &path_for_attrs_owned;
        let use_textconv = !no_binary;
        let textconv_patch = use_textconv && diff_textconv_active(git_dir, &config, path_for_attrs);
        let old_raw = read_blob_bytes(odb, &entry.old_oid);
        let new_raw = read_blob_bytes(odb, &entry.new_oid);
        if !textconv_patch
            && (is_binary_for_diff(git_dir, path_for_attrs, &old_raw)
                || is_binary_for_diff(git_dir, path_for_attrs, &new_raw))
        {
            if entry.status == DiffStatus::Deleted {
                diff_text.push_str(&format!("Binary files a/{old_path} and /dev/null differ\n"));
            } else if entry.status == DiffStatus::Added {
                diff_text.push_str(&format!("Binary files /dev/null and b/{new_path} differ\n"));
            } else {
                diff_text.push_str(&format!(
                    "Binary files a/{old_path} and b/{new_path} differ\n"
                ));
            }
            continue;
        }
        let old_content = if entry.old_oid == zero_oid() {
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
            read_blob_content(odb, &entry.old_oid)
        };
        let new_content = if entry.new_oid == zero_oid() {
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
            read_blob_content(odb, &entry.new_oid)
        };
        let patch = unified_diff_with_prefix(
            &old_content,
            &new_content,
            &old_path,
            &new_path,
            opts.context_lines,
            0,
            opts.diff_src_prefix,
            opts.diff_dst_prefix,
            true,
            config.quote_path_fully(),
        );
        diff_text.push_str(&patch);
    }

    let patch_start = diff_text.find("diff --git").unwrap_or(diff_text.len());
    let stat_block = diff_text[..patch_start].to_string();
    let patch_only = diff_text[patch_start..].to_string();

    // Git prepends a fixed `------------` prefix to the user-supplied attachment separator to form
    // the MIME boundary (`--<boundary>` then yields `--------------<sep>` delimiter lines).
    let mime_boundary = opts
        .attach
        .as_deref()
        .or(opts.inline.as_deref())
        .filter(|b| !b.is_empty())
        .map(|b| format!("------------{b}"));
    let use_mime = opts.attach.is_some() || opts.inline.is_some();
    // Default MIME boundary is git's version string with the fixed `------------` prefix
    // (builtin/log.c make_cover_letter / mime boundary uses `git_version_string`).
    let boundary = mime_boundary.unwrap_or_else(|| format!("------------{}", git_version_string()));

    // From line
    let from_oid = if opts.zero_commit {
        "0".repeat(oid.to_hex().len())
    } else {
        oid.to_hex()
    };
    out.push_str(&format!("From {from_oid} Mon Sep 17 00:00:00 2001\n"));

    // Determine in-body From: (when the author differs from the From: header mailbox).
    let author_mailbox =
        format_ident(&grit_lib::commit_encoding::decode_rfc2047_mailbox_from_line(&commit.author));
    let mut in_body_from: Option<String> = None;
    if !matches!(opts.from_header, FromHeaderMode::Omit) {
        let header_mailbox = mailbox_for_from_header(commit, &opts.from_header);
        write_addr_header(
            &mut out,
            "From",
            &header_mailbox,
            encode_email_headers,
            &charset_label,
        );
        if header_mailbox != author_mailbox || opts.force_in_body_from {
            in_body_from = Some(author_mailbox.clone());
        }
    }

    // Date: from author timestamp
    let date = format_date_rfc2822(&commit.author);
    out.push_str(&format!("Date: {date}\n"));

    // Subject
    write_subject_header(&mut out, subject, encode_email_headers, &charset_label);

    write_thread_headers(&mut out, message_id, in_reply_to, references);
    write_recipient_headers(&mut out, opts);

    // Commit message body (skip first line which is in Subject). Git strips trailing whitespace
    // from each body line when emitting the mail body (matching `pp_remainder`).
    let body: String = commit_msg_unicode
        .lines()
        .skip(1)
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n");
    let body = body.trim_start_matches('\n');
    // Notes are emitted after the `---` separator (in the diffstat region), NOT inside the commit
    // message body, so they appear after any Signed-off-by trailer. See below.
    let notes_block = crate::commands::log::format_patch_notes_block(repo, oid, &opts.notes_refs);

    // Apply signoff trailer to the body text.
    let signoff_line = if opts.signoff {
        let (name, email) = get_signoff_identity(&commit.committer);
        Some(if use_utf8_log {
            format!("Signed-off-by: {name} <{email}>")
        } else {
            encode_email_address_for_charset(
                &format!("Signed-off-by: {name} <{email}>"),
                &charset_label,
            )
        })
    } else {
        None
    };
    // Git appends the signoff to the *full* pretty-printed mail buffer (subject line plus
    // the blank separator plus the body), so for an empty body the trailing "\n\n" already
    // present after the subject means no extra blank lines are inserted before the trailer.
    // Reconstruct that by passing the raw subject line so footer/blank-line handling matches.
    let raw_subject_line = commit_msg_unicode.lines().next().unwrap_or("");
    let body_with_signoff = apply_signoff(raw_subject_line, body, signoff_line.as_deref(), git_dir);

    // Check if anything (subject already encoded; body, in-body From) contains non-ASCII.
    let in_body_non_ascii = in_body_from
        .as_deref()
        .map(|s| s.bytes().any(|b| b > 127))
        .unwrap_or(false);
    let body_has_non_ascii = body_with_signoff.bytes().any(|b| b > 127) || in_body_non_ascii;
    let needs_mime = use_mime || body_has_non_ascii;

    // MIME headers for --attach / --inline or non-ASCII body
    if needs_mime {
        out.push_str("MIME-Version: 1.0\n");
        if use_mime {
            out.push_str(&format!(
                "Content-Type: multipart/mixed; boundary=\"{}\"\n",
                boundary
            ));
        } else {
            out.push_str(&format!(
                "Content-Type: text/plain; charset={charset_label}\n"
            ));
            out.push_str("Content-Transfer-Encoding: 8bit\n");
        }
    }

    out.push('\n');

    if use_mime {
        // MIME multipart: preamble, description part, then patch as attachment.
        out.push_str("This is a multi-part message in MIME format.\n");
        out.push_str(&format!("--{boundary}\n"));
        out.push_str(&format!(
            "Content-Type: text/plain; charset={charset_label}; format=fixed\n"
        ));
        out.push_str("Content-Transfer-Encoding: 8bit\n");
        out.push('\n');
        if let Some(ref ibf) = in_body_from {
            out.push_str(&format!("From: {ibf}\n\n"));
        }
        if !body_with_signoff.is_empty() {
            // git's MIME text/plain part keeps the blank line that separates the (omitted)
            // subject from the body, so a non-empty body is preceded by a blank line.
            out.push('\n');
            out.push_str(&mboxrd_escape(&body_with_signoff, opts.mboxrd));
            out.push('\n');
        }

        out.push_str("---\n");
        if !notes_block.is_empty() {
            out.push_str(&notes_block);
            out.push('\n');
        }
        out.push_str(&stat_block);
        out.push('\n');

        // Patch attachment part. Git names the part after the patch's filename
        // (`0001-<subject>.patch`).
        out.push_str(&format!("--{boundary}\n"));
        let disposition = if opts.inline.is_some() {
            "inline"
        } else {
            "attachment"
        };
        let filename = attachment_filename;
        out.push_str(&format!(
            "Content-Type: text/x-patch; name=\"{filename}\"\n"
        ));
        out.push_str("Content-Transfer-Encoding: 8bit\n");
        out.push_str(&format!(
            "Content-Disposition: {disposition}; filename=\"{filename}\"\n"
        ));
        out.push('\n');
        out.push_str(&patch_only);
        // The base-commit / prerequisite footer belongs inside the patch attachment part, before
        // the closing MIME boundary (git appends it to the diff body, which is the attachment).
        if include_base {
            if let Some(ref base_hex) = opts.base_commit {
                out.push('\n');
                out.push_str(&format!("base-commit: {base_hex}\n"));
                for pid in &opts.prereq_patch_ids {
                    out.push_str(&format!("prerequisite-patch-id: {pid}\n"));
                }
            }
        }
        // A blank line separates the patch body from the closing boundary, and git emits
        // two trailing blank lines after it.
        out.push('\n');
        out.push_str(&format!("--{boundary}--\n"));
        out.push('\n');
        out.push('\n');
    } else {
        // Standard (non-MIME) patch format
        if let Some(ref ibf) = in_body_from {
            out.push_str(&format!("From: {ibf}\n\n"));
        }
        if !body_with_signoff.is_empty() {
            out.push_str(&mboxrd_escape(&body_with_signoff, opts.mboxrd));
            out.push('\n');
        }

        if opts.want_stat {
            out.push_str("---\n");
        } else {
            // `-p` with no stat, or only `--numstat`/`--shortstat`: git omits the `---` separator,
            // emitting a blank line before the stat/raw diff instead.
            out.push('\n');
        }
        // Notes block (after the `---` separator, before the diffstat/diff), separated by a blank.
        if !notes_block.is_empty() {
            out.push_str(&notes_block);
            out.push('\n');
        }
        out.push_str(&diff_text);
    }

    // prerequisite-patch-id + base-commit info (appended to the last patch in the series). For MIME
    // attachments this footer was already emitted inside the attachment part above.
    if !use_mime && include_base {
        if let Some(ref base_hex) = opts.base_commit {
            out.push('\n');
            out.push_str(&format!("base-commit: {base_hex}\n"));
            for pid in &opts.prereq_patch_ids {
                out.push_str(&format!("prerequisite-patch-id: {pid}\n"));
            }
        }
    }

    // Solo interdiff / range-diff block (no cover letter): placed after the diff (and after
    // any base-commit footer) and before the signature.
    if let Some(extra) = solo_extra {
        out.push_str(extra);
    }

    // MIME (--attach / --inline) patches carry no `-- \n<signature>` trailer; the closing
    // boundary terminates the message instead.
    if !use_mime {
        write_signature(&mut out, opts.signature.as_deref());
    }

    Ok(out)
}

fn read_blob_bytes(odb: &Odb, oid: &ObjectId) -> Vec<u8> {
    if *oid == zero_oid() {
        return Vec::new();
    }
    odb.read(oid).map(|o| o.data).unwrap_or_default()
}

/// Read blob content as UTF-8 string (empty for zero OID).
fn read_blob_content(odb: &Odb, oid: &ObjectId) -> String {
    if *oid == zero_oid() {
        return String::new();
    }
    match odb.read(oid) {
        Ok(obj) => String::from_utf8_lossy(&obj.data).into_owned(),
        Err(_) => String::new(),
    }
}

/// Write diff header to a string.
fn write_diff_header_to_string(
    out: &mut String,
    entry: &grit_lib::diff::DiffEntry,
    src_prefix: &str,
    dst_prefix: &str,
) {
    use grit_lib::diff::DiffStatus;
    use std::fmt::Write;

    let old_path = entry
        .old_path
        .as_deref()
        .unwrap_or(entry.new_path.as_deref().unwrap_or(RepoPath::from_str("")));
    let new_path = entry
        .new_path
        .as_deref()
        .unwrap_or(entry.old_path.as_deref().unwrap_or(RepoPath::from_str("")));

    if src_prefix.is_empty() && dst_prefix.is_empty() {
        let _ = writeln!(out, "diff --git {old_path} {new_path}");
    } else {
        let _ = writeln!(
            out,
            "diff --git {src_prefix}{old_path} {dst_prefix}{new_path}",
            src_prefix = src_prefix,
            dst_prefix = dst_prefix
        );
    }

    match entry.status {
        DiffStatus::Added => {
            let _ = writeln!(out, "new file mode {}", entry.new_mode);
            let old_abbrev = &entry.old_oid.to_hex()[..7];
            let new_abbrev = &entry.new_oid.to_hex()[..7];
            let _ = writeln!(out, "index {old_abbrev}..{new_abbrev}");
        }
        DiffStatus::Deleted => {
            let _ = writeln!(out, "deleted file mode {}", entry.old_mode);
            let old_abbrev = &entry.old_oid.to_hex()[..7];
            let new_abbrev = &entry.new_oid.to_hex()[..7];
            let _ = writeln!(out, "index {old_abbrev}..{new_abbrev}");
        }
        DiffStatus::Modified => {
            if entry.old_mode != entry.new_mode {
                let _ = writeln!(out, "old mode {}", entry.old_mode);
                let _ = writeln!(out, "new mode {}", entry.new_mode);
            }
            let old_abbrev = &entry.old_oid.to_hex()[..7];
            let new_abbrev = &entry.new_oid.to_hex()[..7];
            if entry.old_mode == entry.new_mode {
                let _ = writeln!(out, "index {old_abbrev}..{new_abbrev} {}", entry.old_mode);
            } else {
                let _ = writeln!(out, "index {old_abbrev}..{new_abbrev}");
            }
        }
        DiffStatus::Renamed => {
            let sim = entry.score.unwrap_or(100);
            let _ = writeln!(out, "similarity index {sim}%");
            let _ = writeln!(out, "rename from {old_path}");
            let _ = writeln!(out, "rename to {new_path}");
            if entry.old_oid != entry.new_oid {
                let old_abbrev = &entry.old_oid.to_hex()[..7];
                let new_abbrev = &entry.new_oid.to_hex()[..7];
                let _ = writeln!(out, "index {old_abbrev}..{new_abbrev}");
            }
        }
        DiffStatus::Copied => {
            let sim = entry.score.unwrap_or(100);
            let _ = writeln!(out, "similarity index {sim}%");
            let _ = writeln!(out, "copy from {old_path}");
            let _ = writeln!(out, "copy to {new_path}");
            if entry.old_oid != entry.new_oid {
                let old_abbrev = &entry.old_oid.to_hex()[..7];
                let new_abbrev = &entry.new_oid.to_hex()[..7];
                let _ = writeln!(out, "index {old_abbrev}..{new_abbrev}");
            }
        }
        DiffStatus::TypeChanged => {
            let _ = writeln!(out, "old mode {}", entry.old_mode);
            let _ = writeln!(out, "new mode {}", entry.new_mode);
        }
        DiffStatus::Unmerged => {}
    }
}

/// Build the `From:` mailbox for a patch from `format.from` / `--from`.
fn mailbox_for_from_header(commit: &CommitData, mode: &FromHeaderMode) -> String {
    match mode {
        FromHeaderMode::Omit => String::new(),
        FromHeaderMode::Author => format_ident(
            &grit_lib::commit_encoding::decode_rfc2047_mailbox_from_line(&commit.author),
        ),
        FromHeaderMode::Committer => format_ident(
            &grit_lib::commit_encoding::decode_rfc2047_mailbox_from_line(&commit.committer),
        ),
        FromHeaderMode::Custom(s) => {
            format_ident(&grit_lib::commit_encoding::decode_rfc2047_mailbox_from_line(s))
        }
    }
}

// ---------------------------------------------------------------------------
// Header encoding / folding (ported from git's pretty.c + utf8.c)
// ---------------------------------------------------------------------------

/// Write the To/Cc/extra recipient headers.
fn write_recipient_headers(out: &mut String, opts: &PatchOptions) {
    for h in &opts.extra_headers {
        let h = h.trim_end_matches('\n');
        if !h.is_empty() {
            out.push_str(h);
            out.push('\n');
        }
    }
    if !opts.cc.is_empty() {
        let encoded: Vec<String> = opts.cc.iter().map(|a| encode_email_address(a)).collect();
        write_folded_header(out, "Cc", &encoded);
    }
    if !opts.to.is_empty() {
        let encoded: Vec<String> = opts.to.iter().map(|a| encode_email_address(a)).collect();
        write_folded_header(out, "To", &encoded);
    }
}

// ---------------------------------------------------------------------------
// Subject / prefix / reroll / signature / threading / base / cover helpers
// ---------------------------------------------------------------------------

/// Build the cover-letter Subject value.
fn build_cover_subject(
    prefix: &str,
    use_numbering: bool,
    display_total: usize,
    start: usize,
    cover_desc: Option<&CoverDescription>,
    _encode: bool,
    _enc: &str,
) -> String {
    let subj = cover_desc
        .and_then(|d| d.subject.clone())
        .unwrap_or_else(|| "*** SUBJECT HERE ***".to_owned());
    let num0 = if start != 1 { 0 } else { 0 };
    let _ = num0;
    if use_numbering {
        if prefix.is_empty() {
            format!("[0/{display_total}] {subj}")
        } else {
            format!("[{prefix} 0/{display_total}] {subj}")
        }
    } else if prefix.is_empty() {
        subj
    } else {
        format!("[{prefix}] {subj}")
    }
}

/// The git version string as the test's `signature()` default (matches `git --version` minus
/// the `git version ` prefix).
fn git_version_string() -> String {
    crate::version_string()
}

/// Resolve the signature: `None` suppresses the `-- \n...` block entirely.
fn resolve_signature(config: &ConfigSet, args: &Args, git_version: &str) -> Result<Option<String>> {
    if args.no_signature {
        return Ok(None);
    }
    // --signature-file / --signature take priority over config.
    if let Some(ref sf) = args.signature_file {
        let raw = std::fs::read(sf)
            .with_context(|| format!("cannot read signature file '{}'", sf.display()))?;
        let mut s = String::from_utf8_lossy(&raw).into_owned();
        // Trailing newline is added by write_signature; drop one trailing newline if present.
        if s.ends_with('\n') {
            s.pop();
        }
        return Ok(Some(s));
    }
    if let Some(ref sig) = args.signature {
        if sig.is_empty() {
            return Ok(None);
        }
        return Ok(Some(sig.clone()));
    }
    // config.signaturefile
    if let Some(sf) = config
        .get("format.signaturefile")
        .or_else(|| config.get("format.signatureFile"))
    {
        let raw =
            std::fs::read(&sf).with_context(|| format!("cannot read signature file '{sf}'"))?;
        let mut s = String::from_utf8_lossy(&raw).into_owned();
        if s.ends_with('\n') {
            s.pop();
        }
        return Ok(Some(s));
    }
    // config.signature
    if let Some(sig) = config.get("format.signature") {
        if sig.is_empty() {
            return Ok(None);
        }
        return Ok(Some(sig));
    }
    Ok(Some(git_version.to_owned()))
}

/// Resolve the threading mode from `--thread`/`--no-thread`/`format.thread`.
fn resolve_thread_mode(config: &ConfigSet, args: &Args) -> ThreadMode {
    if args.no_thread {
        return ThreadMode::None;
    }
    if let Some(ref t) = args.thread {
        return match t.as_str() {
            "deep" => ThreadMode::Deep,
            _ => ThreadMode::Shallow,
        };
    }
    // format.thread: true=shallow, deep=deep, else none.
    match config.get("format.thread").as_deref().map(str::trim) {
        Some("deep") => ThreadMode::Deep,
        Some("shallow") => ThreadMode::Shallow,
        Some(v) => {
            if parse_bool(v).unwrap_or(false) {
                ThreadMode::Shallow
            } else {
                ThreadMode::None
            }
        }
        None => ThreadMode::None,
    }
}

/// Message-Id chain state for `--thread`.
struct ThreadState {
    mode: ThreadMode,
    explicit_irt: Option<String>,
    /// Message-Ids generated so far, indexed by sequence number (0-based; cover=0 when present).
    ids: Vec<String>,
    /// Whether a cover letter occupies seq 0 (affects the shallow thread root).
    has_cover: bool,
}

impl ThreadState {
    fn new(mode: ThreadMode, in_reply_to: Option<String>, has_cover: bool) -> Self {
        ThreadState {
            mode,
            explicit_irt: in_reply_to.map(|s| strip_angles(&s).to_string()),
            ids: Vec::new(),
            has_cover,
        }
    }

    /// Generate (and remember) the Message-Id for sequence `seq`.
    fn next_message_id(&mut self, commits: &[(ObjectId, CommitData)], seq: usize) -> String {
        // git only assigns/emits Message-Id headers when threading is active (`--thread` or
        // `format.thread`). Without it, no Message-Id is generated even with `--in-reply-to`.
        if matches!(self.mode, ThreadMode::None) {
            return String::new();
        }
        // The cover letter (seq 0 when present) gets a synthetic id; each patch maps to its commit
        // by `seq - cover_offset`.
        let cover_offset = if self.cover_present() { 1 } else { 0 };
        let id = if self.cover_present() && seq == 0 {
            "cover.git-send-email-grit-0@example.com".to_string()
        } else if let Some((oid, commit)) = commits.get(seq - cover_offset) {
            let ts = commit_author_timestamp(commit);
            format!("{ts}-{}-git-send-email-grit@example.com", oid.to_hex())
        } else {
            format!("cover.git-send-email-grit-{seq}@example.com")
        };
        while self.ids.len() <= seq {
            self.ids.push(String::new());
        }
        self.ids[seq] = id.clone();
        id
    }

    fn cover_present(&self) -> bool {
        // The cover letter, when present, always occupies seq 0.
        self.has_cover
    }

    /// In-Reply-To value for sequence `seq` (the bare id, no angles).
    fn in_reply_to_for(&self, seq: usize) -> Option<&str> {
        match self.mode {
            ThreadMode::None => self.explicit_irt.as_deref(),
            ThreadMode::Shallow => {
                if seq == 0 {
                    self.explicit_irt.as_deref()
                } else if self.has_cover {
                    // Patches reply to the cover letter (seq 0).
                    self.ids.first().map(|s| s.as_str())
                } else if let Some(ref e) = self.explicit_irt {
                    // No cover letter: the in-reply-to anchor is the thread root, so every patch
                    // replies to it directly.
                    Some(e.as_str())
                } else {
                    // No cover and no anchor: patches reply to the first patch.
                    self.ids.first().map(|s| s.as_str())
                }
            }
            ThreadMode::Deep => {
                if seq == 0 {
                    self.explicit_irt.as_deref()
                } else {
                    self.ids.get(seq - 1).map(|s| s.as_str())
                }
            }
        }
    }

    /// References chain for sequence `seq`.
    fn references_for(&self, seq: usize) -> Vec<String> {
        match self.mode {
            ThreadMode::None => self
                .explicit_irt
                .as_deref()
                .map(|s| vec![s.to_string()])
                .unwrap_or_default(),
            ThreadMode::Shallow => {
                let mut refs = Vec::new();
                if let Some(ref e) = self.explicit_irt {
                    refs.push(e.clone());
                }
                // The first patch references only the anchor (its parent). Subsequent patches add
                // the thread root: the cover letter (seq 0) when present, otherwise -- when there is
                // no anchor -- the first patch. With an anchor and no cover, the anchor is the root
                // and is already the sole reference.
                if seq > 0 && (self.has_cover || self.explicit_irt.is_none()) {
                    if let Some(first) = self.ids.first() {
                        refs.push(first.clone());
                    }
                }
                refs
            }
            ThreadMode::Deep => {
                let mut refs = Vec::new();
                if let Some(ref e) = self.explicit_irt {
                    refs.push(e.clone());
                }
                for i in 0..seq {
                    if let Some(id) = self.ids.get(i) {
                        if !id.is_empty() {
                            refs.push(id.clone());
                        }
                    }
                }
                refs
            }
        }
    }
}

/// Compute the `--relative` prefix from the current directory inside the worktree.
fn relative_prefix_from_cwd(repo: &Repository) -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let wt = repo.work_tree.as_ref()?;
    let rel = cwd.strip_prefix(wt).ok()?;
    if rel.as_os_str().is_empty() {
        None
    } else {
        Some(ensure_trailing_slash(&rel.to_string_lossy()))
    }
}

/// Drop diff entries outside the `--relative` prefix and strip the prefix from the rest.
fn apply_relative_filter(
    entries: Vec<grit_lib::diff::DiffEntry>,
    relative: Option<&str>,
) -> Vec<grit_lib::diff::DiffEntry> {
    let Some(prefix) = relative else {
        return entries;
    };
    let mut out = Vec::new();
    for mut e in entries {
        let keep = e.path().starts_with_bytes(prefix.as_bytes());
        if !keep {
            continue;
        }
        if let Some(ref p) = e.old_path {
            if let Some(stripped) = p.as_bytes().strip_prefix(prefix.as_bytes()) {
                e.old_path = Some(RepoPathBuf::from_bytes(stripped.to_vec()));
            }
        }
        if let Some(ref p) = e.new_path {
            if let Some(stripped) = p.as_bytes().strip_prefix(prefix.as_bytes()) {
                e.new_path = Some(RepoPathBuf::from_bytes(stripped.to_vec()));
            }
        }
        out.push(e);
    }
    out
}

/// True when the commit's diff against its first parent touches any path in `pathspec`.
fn commit_touches_pathspec(repo: &Repository, commit: &CommitData, pathspec: &[String]) -> bool {
    let parent_tree = commit.parents.first().and_then(|p| {
        repo.odb
            .read(p)
            .ok()
            .and_then(|obj| parse_commit(&obj.data).ok())
            .map(|c| c.tree)
    });
    let entries = match diff_trees(&repo.odb, parent_tree.as_ref(), Some(&commit.tree), "") {
        Ok(e) => e,
        Err(_) => return true,
    };
    entries
        .iter()
        .any(|e| pathspec.iter().any(|ps| path_matches_spec(&e.path().to_string_lossy(), ps)))
}

/// Cover-letter subject/blurb resolved from description settings.
struct CoverDescription {
    subject: Option<String>,
    body: Option<String>,
}

/// Resolve the cover-letter description (subject + blurb) from --cover-from-description /
/// --description-file / branch.<name>.description / config.
fn resolve_cover_description(
    repo: &Repository,
    config: &ConfigSet,
    args: &Args,
    commits: &[(ObjectId, CommitData)],
) -> Result<CoverDescription> {
    // Determine the mode.
    let mode = if let Some(ref m) = args.cover_from_description {
        m.clone()
    } else {
        config
            .get("format.coverfromdescription")
            .or_else(|| config.get("format.coverFromDescription"))
            .unwrap_or_else(|| "message".to_owned())
    };
    let mode = mode.trim();
    if !matches!(mode, "default" | "none" | "message" | "subject" | "auto") {
        anyhow::bail!("invalid cover-from-description mode '{mode}'");
    }

    // Source description text: --description-file, else branch description.
    let description = if let Some(ref f) = args.description_file {
        let raw = std::fs::read(f)
            .with_context(|| format!("cannot read description file '{}'", f.display()))?;
        Some(String::from_utf8_lossy(&raw).into_owned())
    } else {
        current_branch_description(repo, config)
    };

    let Some(desc) = description else {
        return Ok(CoverDescription {
            subject: None,
            body: None,
        });
    };
    let _ = commits;

    // Split into first paragraph (subject candidate) and remainder.
    let desc = desc.trim_end_matches('\n');
    let mut lines = desc.lines();
    let first = lines.next().unwrap_or("").to_string();
    let rest: String = {
        let collected: Vec<&str> = lines.collect();
        collected.join("\n")
    };
    let rest_trimmed = rest.trim_start_matches('\n').to_string();

    match mode {
        "none" => Ok(CoverDescription {
            subject: None,
            body: None,
        }),
        "subject" => {
            // First paragraph becomes subject; remainder becomes blurb.
            let body = if rest_trimmed.is_empty() {
                None
            } else {
                Some(rest_trimmed)
            };
            Ok(CoverDescription {
                subject: Some(first),
                body,
            })
        }
        "auto" => {
            // Subject from first line only if it is "short" (<= 100 chars and single line).
            let is_subject = first.chars().count() <= 100 && desc.lines().count() >= 1;
            // git: use as subject if the first line is at most 100 columns.
            if is_subject && first.chars().count() <= 100 {
                let body = if rest_trimmed.is_empty() {
                    None
                } else {
                    Some(rest_trimmed)
                };
                Ok(CoverDescription {
                    subject: Some(first),
                    body,
                })
            } else {
                Ok(CoverDescription {
                    subject: None,
                    body: Some(desc.to_string()),
                })
            }
        }
        // "default" / "message": whole description becomes the blurb; subject stays placeholder.
        _ => Ok(CoverDescription {
            subject: None,
            body: Some(desc.to_string()),
        }),
    }
}

/// Read `branch.<current>.description` for the checked-out branch.
fn current_branch_description(repo: &Repository, config: &ConfigSet) -> Option<String> {
    let branch = current_branch_name(repo)?;
    let key = format!("branch.{branch}.description");
    config.get(&key).filter(|s| !s.is_empty())
}

/// Resolve the effective `--attach` separator: an explicit `--attach[=sep]` on the command line
/// wins; otherwise `format.attach` enables attachment when set to a non-empty value (an empty
/// value explicitly disables it, even overriding a value inherited from a broader config scope).
/// `--inline` suppresses attachment entirely.
fn resolve_attach(args: &Args, config: &ConfigSet) -> Option<String> {
    if let Some(a) = args.attach.clone() {
        return Some(a);
    }
    if args.inline.is_some() {
        return None;
    }
    match config.get("format.attach") {
        Some(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

fn current_branch_name(repo: &Repository) -> Option<String> {
    let head = grit_lib::state::resolve_head(&repo.git_dir).ok()?;
    head.branch_name().map(|s| s.to_string())
}

/// Build the ordered `(header, refname)` list of notes to display, honoring `format.notes`
/// (multi-value, `false` clears) followed by command-line `--notes[=ref]` / `--no-notes`
/// processed in their original argv order (`--no-notes` clears all accumulated refs).
fn resolve_notes_refs(args: &Args, config: &ConfigSet) -> Vec<(String, String)> {
    let mut refs: Vec<String> = Vec::new();
    let add = |refs: &mut Vec<String>, r: String| {
        if !refs.contains(&r) {
            refs.push(r);
        }
    };

    // Config: format.notes (multi-value, in order). `false`/`no`/`off`/`0` clears the list.
    for v in config.get_all("format.notes") {
        let t = v.trim();
        if matches!(t, "false" | "no" | "off" | "0") {
            refs.clear();
        } else {
            add(&mut refs, notes_value_to_ref(t));
        }
    }

    // Command line: replay `--notes`/`--no-notes` in argv order so the last of a conflicting pair
    // wins (git treats these as ordered toggles).
    for arg in std::env::args().skip(1) {
        if arg == "--no-notes" {
            refs.clear();
        } else if arg == "--notes" {
            add(&mut refs, "refs/notes/commits".to_string());
        } else if let Some(val) = arg.strip_prefix("--notes=") {
            add(&mut refs, notes_value_to_ref(val));
        }
    }
    if args.no_notes {
        refs.clear();
    } else {
        for note in &args.notes {
            add(&mut refs, notes_value_to_ref(note));
        }
    }

    refs.into_iter()
        .map(|refname| {
            let short = refname.strip_prefix("refs/notes/").unwrap_or(&refname);
            let header = if refname == "refs/notes/commits" {
                "Notes".to_string()
            } else {
                format!("Notes ({short})")
            };
            (header, refname)
        })
        .collect()
}

/// Resolve the upstream (`@{upstream}`) commit of the current branch from `branch.<name>.{remote,merge}`.
/// Returns `None` when HEAD is detached or no upstream is configured / resolvable.
fn resolve_branch_upstream_oid(repo: &Repository, config: &ConfigSet) -> Option<ObjectId> {
    let branch = current_branch_name(repo)?;
    let remote = config.get(&format!("branch.{branch}.remote"));
    let merge_ref = config.get(&format!("branch.{branch}.merge"));
    match (remote.as_deref(), merge_ref.as_deref()) {
        (Some("."), Some(m)) => resolve_revision(repo, m).ok(),
        (Some(_r), Some(m)) => {
            let short = m.strip_prefix("refs/heads/").unwrap_or(m);
            resolve_revision(repo, short)
                .ok()
                .or_else(|| resolve_revision(repo, m).ok())
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// --base / prerequisite-patch-id, interdiff, range-diff, signoff, mboxrd
// ---------------------------------------------------------------------------

/// Resolve `--base` (or `format.useAutoBase`) into (base-commit-hex, prerequisite-patch-ids).
fn resolve_base_and_prereqs(
    repo: &Repository,
    config: &ConfigSet,
    args: &Args,
    commits: &[(ObjectId, CommitData)],
) -> Result<(Option<String>, Vec<String>)> {
    if args.no_base {
        return Ok((None, Vec::new()));
    }

    // Determine the base spec: --base wins; else format.useAutoBase.
    let auto_base = config
        .get("format.useautobase")
        .or_else(|| config.get("format.useAutoBase"));
    let when_able = matches!(
        auto_base.as_deref().map(str::trim),
        Some("whenAble" | "whenable")
    );

    let base_spec: Option<String> = if let Some(ref b) = args.base {
        Some(b.clone())
    } else if let Some(ref ab) = auto_base {
        let t = ab.trim();
        if t == "auto" || when_able || parse_bool(t).unwrap_or(false) {
            Some("auto".to_string())
        } else {
            None
        }
    } else {
        None
    };

    let Some(base_spec) = base_spec else {
        return Ok((None, Vec::new()));
    };

    let (first_oid, first_commit) = match commits.first() {
        Some(c) => c,
        None => return Ok((None, Vec::new())),
    };
    // The first patch's parent is the "tip" the prerequisites lead up to.
    let first_parent = first_commit.parents.first().copied();

    let base_oid = if base_spec == "auto" {
        match compute_auto_base(repo, first_oid, when_able)? {
            Some(o) => o,
            None => return Ok((None, Vec::new())),
        }
    } else {
        resolve_revision(repo, &base_spec)
            .with_context(|| format!("unknown base revision '{base_spec}'"))?
    };

    // Validate the base commit the way Git does in `get_base_commit` (builtin/log.c): the base must
    // be an ancestor of the *pairwise-reduced merge base* of every commit in the patch series, not
    // merely of the first patch's parent. For a criss-cross history (e.g. `Z..X` across a merge)
    // this correctly rejects an inner commit like P or Z while accepting the true fork point.
    let rev0 = reduced_pairwise_merge_base(repo, commits)?;
    if !is_ancestor(repo, &base_oid, &rev0)? {
        anyhow::bail!("base commit should be the ancestor of revision list");
    }
    // Validate: base must not be one of the commits in the revision list.
    if commits.iter().any(|(o, _)| *o == base_oid) {
        anyhow::bail!("base commit shouldn't be in revision list");
    }

    // Prerequisite patch-ids: commits in base..first_parent (oldest first).
    let mut prereqs = Vec::new();
    if let Some(fp) = first_parent {
        let result = rev_list(
            repo,
            &[fp.to_hex()],
            &[base_oid.to_hex()],
            &RevListOptions {
                max_parents: Some(1),
                reverse: true,
                ..Default::default()
            },
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        for oid in result.commits {
            if let Ok(Some(pid)) = compute_patch_id(&repo.odb, &oid) {
                prereqs.push(pid.to_hex());
            }
        }
    }

    Ok((Some(base_oid.to_hex()), prereqs))
}

/// Compute `--base=auto`: the merge-base of HEAD and its configured upstream.
fn compute_auto_base(
    repo: &Repository,
    tip: &ObjectId,
    when_able: bool,
) -> Result<Option<ObjectId>> {
    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let branch = match current_branch_name(repo) {
        Some(b) => b,
        None => {
            if when_able {
                return Ok(None);
            }
            anyhow::bail!("Can't find a base commit; need a branch with an upstream");
        }
    };
    let remote = config.get(&format!("branch.{branch}.remote"));
    let merge_ref = config.get(&format!("branch.{branch}.merge"));
    let upstream_oid = match (remote.as_deref(), merge_ref.as_deref()) {
        (Some("."), Some(m)) => resolve_revision(repo, m).ok(),
        (Some(_r), Some(m)) => {
            let short = m.strip_prefix("refs/heads/").unwrap_or(m);
            resolve_revision(repo, short)
                .ok()
                .or_else(|| resolve_revision(repo, m).ok())
        }
        _ => None,
    };
    let Some(upstream_oid) = upstream_oid else {
        if when_able {
            return Ok(None);
        }
        anyhow::bail!("Can't find a base commit; need a branch with an upstream");
    };
    let bases = merge_bases_first_vs_rest(repo, *tip, &[upstream_oid])?;
    if bases.len() != 1 {
        if when_able {
            return Ok(None);
        }
        anyhow::bail!("base commit shouldn't be in revision list");
    }
    Ok(Some(bases[0]))
}

fn is_ancestor(repo: &Repository, ancestor: &ObjectId, descendant: &ObjectId) -> Result<bool> {
    let bases = merge_bases_first_vs_rest(repo, *ancestor, &[*descendant])?;
    Ok(bases.iter().any(|b| b == ancestor))
}

/// Reduce the patch series to a single representative commit via pairwise merge bases, matching
/// Git's `get_base_commit` (builtin/log.c): repeatedly replace each adjacent pair with their merge
/// base until one commit remains. Each pair must have exactly one merge base, otherwise Git errors
/// with "failed to find exact merge base".
fn reduced_pairwise_merge_base(
    repo: &Repository,
    commits: &[(ObjectId, CommitData)],
) -> Result<ObjectId> {
    let mut rev: Vec<ObjectId> = commits.iter().map(|(o, _)| *o).collect();
    if rev.is_empty() {
        anyhow::bail!("base commit should be the ancestor of revision list");
    }
    while rev.len() > 1 {
        let mut next: Vec<ObjectId> = Vec::with_capacity(rev.len().div_ceil(2));
        let half = rev.len() / 2;
        for i in 0..half {
            let bases = merge_bases_first_vs_rest(repo, rev[2 * i], &[rev[2 * i + 1]])?;
            if bases.len() != 1 {
                anyhow::bail!("failed to find exact merge base");
            }
            next.push(bases[0]);
        }
        if rev.len() % 2 == 1 {
            next.push(rev[rev.len() - 1]);
        }
        rev = next;
    }
    Ok(rev[0])
}

/// Build the solo-patch interdiff / range-diff block appended to a single patch when there
/// is no cover letter. The label is unindented; the diff body is indented by two spaces
/// (matching git's `s/^/  /`). Returns `None` if neither --interdiff nor --range-diff is set.
fn build_solo_interdiff_block(
    repo: &Repository,
    commits: &[(ObjectId, CommitData)],
    opts: &PatchOptions,
    interdiff: Option<&str>,
    range_diff: Option<&str>,
    reroll: Option<&str>,
    creation_factor: Option<usize>,
) -> Result<Option<String>> {
    if interdiff.is_none() && range_diff.is_none() {
        return Ok(None);
    }
    let mut out = String::new();
    let prev_ver = reroll.and_then(prev_version_label);
    if let Some(spec) = interdiff {
        out.push('\n');
        match &prev_ver {
            Some(v) => out.push_str(&format!("Interdiff against {v}:\n")),
            None => out.push_str("Interdiff:\n"),
        }
        let body = compute_interdiff(repo, spec, commits, opts)?;
        push_indented(&mut out, &body);
    }
    if let Some(spec) = range_diff {
        out.push('\n');
        match &prev_ver {
            Some(v) => out.push_str(&format!("Range-diff against {v}:\n")),
            None => out.push_str("Range-diff:\n"),
        }
        // Unlike interdiff, the range-diff body is NOT indented in git's output.
        let body = compute_range_diff(repo, spec, commits, creation_factor, opts)?;
        out.push_str(&body);
    }
    Ok(Some(out))
}

/// Compute the interdiff body for the cover letter (`git diff <spec> <series-tip>`).
fn compute_interdiff(
    repo: &Repository,
    spec: &str,
    commits: &[(ObjectId, CommitData)],
    opts: &PatchOptions,
) -> Result<String> {
    let prev_oid = resolve_revision(repo, spec)
        .with_context(|| format!("unknown interdiff revision '{spec}'"))?;
    let prev_commit = parse_commit(&repo.odb.read(&prev_oid)?.data)?;
    let cur_tree = commits
        .last()
        .map(|(_o, c)| c.tree)
        .ok_or_else(|| anyhow::anyhow!("empty series"))?;
    let entries = diff_trees(&repo.odb, Some(&prev_commit.tree), Some(&cur_tree), "")
        .context("computing interdiff")?;
    let mut out = String::new();
    for entry in &entries {
        let old_path = entry
            .old_path
            .as_deref()
            .map_or(std::borrow::Cow::Borrowed("/dev/null"), RepoPath::to_string_lossy);
        let new_path = entry
            .new_path
            .as_deref()
            .map_or(std::borrow::Cow::Borrowed("/dev/null"), RepoPath::to_string_lossy);
        write_diff_header_to_string(&mut out, entry, opts.diff_src_prefix, opts.diff_dst_prefix);
        let old_content = read_blob_content(&repo.odb, &entry.old_oid);
        let new_content = read_blob_content(&repo.odb, &entry.new_oid);
        let patch = unified_diff_with_prefix(
            &old_content,
            &new_content,
            &old_path,
            &new_path,
            opts.context_lines,
            0,
            opts.diff_src_prefix,
            opts.diff_dst_prefix,
            true,
            false,
        );
        out.push_str(&patch);
    }
    Ok(out)
}

/// Compute the range-diff body for a cover letter / solo patch: compare the previous range
/// (`spec`, e.g. `boop~2..boop~1`) against the current series (`<first parent>..<last commit>`).
fn compute_range_diff(
    repo: &Repository,
    spec: &str,
    commits: &[(ObjectId, CommitData)],
    creation_factor: Option<usize>,
    patch_opts: &PatchOptions,
) -> Result<String> {
    let (first_oid, first_commit) = commits
        .first()
        .ok_or_else(|| anyhow::anyhow!("empty series"))?;
    let (last_oid, _) = commits
        .last()
        .ok_or_else(|| anyhow::anyhow!("empty series"))?;
    // New range: parent-of-first .. last. If the first commit is a root, use the commit itself.
    let new_range = match first_commit.parents.first() {
        Some(parent) => format!("{}..{}", parent.to_hex(), last_oid.to_hex()),
        None => first_oid.to_hex(),
    };
    let notes_refs: Vec<String> = patch_opts
        .notes_refs
        .iter()
        .map(|(_, refname)| {
            if refname == "refs/notes/commits" {
                String::new()
            } else {
                refname.clone()
            }
        })
        .collect();
    crate::commands::range_diff::compute_range_diff_body_with_notes(
        repo,
        spec,
        &new_range,
        creation_factor,
        notes_refs.is_empty(),
        &notes_refs,
    )
}

/// Append a Signed-off-by trailer to the body using git's `append_signoff` semantics.
///
/// Git runs `append_signoff` over the whole pretty-printed mail buffer, i.e. the subject
/// line followed by a blank line and the body. We replicate that by prepending
/// `<subject>\n\n` before invoking the trailer logic and stripping it back off, so that an
/// empty body does not trigger the "completely empty buffer" path (which would insert two
/// extra blank lines before the trailer).
fn apply_signoff(
    subject: &str,
    body: &str,
    signoff_line: Option<&str>,
    git_dir: &std::path::Path,
) -> String {
    let Some(sob) = signoff_line else {
        return body.to_string();
    };
    let config = ConfigSet::load(Some(git_dir), true).unwrap_or_default();
    let prefix = format!("{subject}\n\n");
    let mut msg = format!("{prefix}{body}");
    let sob_with_nl = format!("{sob}\n");
    // format-patch --signoff uses APPEND_SIGNOFF_DEDUP: do not add a duplicate sign-off that is
    // already present anywhere in the trailer block.
    grit_lib::commit_trailers::append_signoff_trailer_with_dedup(
        &mut msg,
        &sob_with_nl,
        &config,
        true,
    );
    // Strip the synthetic subject prefix back off.
    let mut msg = msg.split_off(prefix.len());
    // Drop one trailing newline (caller re-adds one).
    if msg.ends_with('\n') {
        msg.pop();
    }
    msg
}
