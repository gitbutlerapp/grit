//! `grit commit` — record changes to the repository.
//!
//! Creates a new commit object from the current index state, updates HEAD
//! to point to the new commit, and optionally runs hooks.

use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use grit_lib::commit::{format_git_timestamp, parse_date_to_git_timestamp};
use grit_lib::config::ConfigSet;
use grit_lib::diff::{
    diff_index_to_tree, diff_index_to_worktree, diff_trees, status_apply_rename_copy_detection,
    DiffEntry, DiffStatus,
};
use grit_lib::error::Error;
use grit_lib::hooks::{
    run_commit_hook, run_hook, run_reference_transaction_committed_for_head_update, CommitHookEnv,
    HookResult,
};
use grit_lib::index::{Index, MODE_GITLINK, MODE_SYMLINK, MODE_TREE};
use grit_lib::interpret_trailers::{NewTrailerArg, ProcessTrailerOptions};
use grit_lib::mailmap::load_mailmap_table;
use grit_lib::objects::{parse_commit, serialize_commit, CommitData, ObjectId, ObjectKind};
use grit_lib::reflog::read_reflog;
use grit_lib::refs::{append_reflog, list_refs, should_autocreate_reflog, write_ref};
use grit_lib::repo::Repository;
use grit_lib::rev_list::{rev_list, RevListOptions};
use grit_lib::rev_parse::resolve_revision_as_commit;
use grit_lib::shared_repo::refresh_repository_shared_tree;
use grit_lib::state::{detect_in_progress, resolve_head, HeadState};
use regex::RegexBuilder;

use crate::branch_tracking::{format_tracking_info, AheadBehindMode};
use grit_lib::write_tree::{
    build_cache_tree_from_index, write_tree_from_index, write_tree_from_index_subset,
    write_tree_partial_from_index,
};

use crate::ident::{resolve_email, resolve_name, IdentRole};

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use time::OffsetDateTime;

/// Environment variable set by [`preprocess_commit_argv`] with the effective `-v` count after
/// scanning argv (Git-compatible ordering with `--no-verbose`).
pub(crate) const GIT_GRIT_COMMIT_VERBOSE_ENV: &str = "GIT_GRIT_INTERNAL_COMMIT_VERBOSE";

/// Scissors line inserted before verbose diffs in `COMMIT_EDITMSG` (matches Git's `cut_line`).
const GIT_COMMIT_CUT_LINE: &str = "------------------------ >8 ------------------------\n";

/// Arguments for `grit commit`.
#[derive(Debug, ClapArgs)]
#[command(about = "Record changes to the repository")]
pub struct Args {
    /// Use the given message as the commit message.
    #[arg(short = 'm', long = "message")]
    pub message: Vec<String>,

    /// Raw `-m` / `--message` argv values captured before UTF-8 conversion.
    #[arg(skip)]
    pub(crate) raw_messages: Vec<Vec<u8>>,

    /// Take the commit message from the given file.
    #[arg(short = 'F', long = "file")]
    pub file: Option<String>,

    /// Commit all changed tracked files (like `git add -u` first).
    #[arg(short = 'a', long = "all")]
    pub all: bool,

    /// Amend the last commit.
    #[arg(long = "amend")]
    pub amend: bool,

    /// Allow an empty commit (no changes).
    #[arg(long = "allow-empty")]
    pub allow_empty: bool,

    /// Allow an empty commit message.
    #[arg(long = "allow-empty-message")]
    pub allow_empty_message: bool,

    /// Show what would be committed without committing.
    #[arg(long = "dry-run")]
    pub dry_run: bool,

    /// Suppress commit summary output.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Add Signed-off-by trailer.
    #[arg(short = 's', long = "signoff")]
    pub signoff: bool,

    /// Take the commit message from an existing commit.
    #[arg(short = 'C', long = "reuse-message", value_name = "COMMIT")]
    pub reuse_message: Option<String>,

    /// Like -C, but open editor to modify the message.
    #[arg(short = 'c', long = "reedit-message", value_name = "COMMIT")]
    pub reedit_message: Option<String>,

    /// Override the author.
    #[arg(long = "author")]
    pub author: Option<String>,

    /// Raw `--author` argv value captured before UTF-8 conversion.
    #[arg(skip)]
    pub(crate) raw_author: Option<Vec<u8>>,

    /// Override the date.
    #[arg(long = "date")]
    pub date: Option<String>,

    /// Suppress the post-rewrite hook.
    #[arg(long = "no-post-rewrite")]
    pub no_post_rewrite: bool,

    /// Give output in short format (for dry-run).
    #[arg(long = "short")]
    pub short: bool,

    /// Give output in porcelain format (for dry-run).
    #[arg(long = "porcelain")]
    pub porcelain: bool,

    /// Give output in long format (default for dry-run).
    #[arg(long = "long")]
    pub long: bool,

    /// Show ahead/behind in dry-run status (default; matches `status.aheadbehind`).
    #[arg(long = "ahead-behind", overrides_with = "no_ahead_behind")]
    pub ahead_behind: bool,

    /// Omit ahead/behind counts in dry-run status.
    #[arg(long = "no-ahead-behind")]
    pub no_ahead_behind: bool,

    /// Include staged changes when given pathspec (with -i).
    #[arg(short = 'i', long = "include")]
    pub include: bool,

    /// Only commit specified paths (with -o or --only).
    #[arg(short = 'o', long = "only")]
    pub only: bool,

    /// Interactively add changes.
    #[arg(long = "interactive")]
    pub interactive: bool,

    /// Select hunks interactively before committing (same idea as `git add -p`).
    #[arg(short = 'p', long = "patch", hide = true)]
    pub patch: bool,

    /// Lines of context for `--patch` (validated to require `-p`).
    #[arg(long = "unified", short = 'U', allow_hyphen_values = true, hide = true)]
    pub unified: Option<i32>,

    /// Context lines between adjacent `--patch` hunks (validated to require `-p`).
    #[arg(long = "inter-hunk-context", allow_hyphen_values = true, hide = true)]
    pub inter_hunk_context: Option<i32>,

    /// Disable auto-advance in interactive patch mode (validated to require `-p`).
    #[arg(long = "no-auto-advance", hide = true)]
    pub no_auto_advance: bool,

    /// Untracked files mode.
    #[arg(short = 'u', long = "untracked-files", value_name = "MODE", num_args = 0..=1, default_missing_value = "all")]
    pub untracked_files: Option<String>,

    /// Verbose - show diff in commit message template (`-v` / `--verbose`; see argv preprocessing in `main`).
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Override cleanup mode.
    #[arg(long = "cleanup", value_name = "MODE")]
    pub cleanup: Option<String>,

    /// Use specified template file.
    #[arg(short = 't', long = "template", value_name = "FILE")]
    pub template: Option<String>,

    /// Edit the commit message (used with -C).
    #[arg(short = 'e', long = "edit")]
    pub edit: bool,

    /// Suppress editing the commit message.
    #[arg(long = "no-edit")]
    pub no_edit: bool,

    /// Set the commit status (accepted but not used).
    #[arg(long = "status")]
    pub status: bool,

    /// Suppress commit status in editor template.
    #[arg(long = "no-status")]
    pub no_status: bool,

    /// Add a Signed-off-by trailer with specific value.
    #[arg(long = "trailer", value_name = "TOKEN:VALUE")]
    pub trailer: Vec<String>,

    /// Override gpg sign.
    #[arg(short = 'S', long = "gpg-sign", value_name = "KEYID", num_args = 0..=1, default_missing_value = "")]
    pub gpg_sign: Option<String>,

    /// Don't sign the commit.
    #[arg(long = "no-gpg-sign")]
    pub no_gpg_sign: bool,

    /// Don't verify the commit message.
    #[arg(long = "no-verify", short = 'n')]
    pub no_verify: bool,

    /// Fixup commit.
    #[arg(long = "fixup", value_name = "COMMIT")]
    pub fixup: Option<String>,

    /// Squash commit.
    #[arg(long = "squash", value_name = "COMMIT")]
    pub squash: Option<String>,

    /// Reset author.
    #[arg(long = "reset-author")]
    pub reset_author: bool,

    /// Read pathspecs from a file (use `-` for stdin), same rules as `git add`.
    #[arg(long = "pathspec-from-file", value_name = "FILE")]
    pub pathspec_from_file: Option<String>,

    /// NUL-separated entries for `--pathspec-from-file` (C-quoting not allowed).
    #[arg(long = "pathspec-file-nul")]
    pub pathspec_file_nul: bool,

    /// Pathspec — files to include in the commit (stages them first).
    #[arg(trailing_var_arg = true, allow_hyphen_values = false)]
    pub pathspec: Vec<String>,
}

#[cfg(unix)]
fn os_arg_bytes(arg: &std::ffi::OsString) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    arg.as_os_str().as_bytes()
}

#[cfg(unix)]
fn strip_raw_prefix<'a>(arg: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    arg.strip_prefix(prefix).filter(|rest| !rest.is_empty())
}

#[cfg(unix)]
fn hydrate_raw_commit_argv_values(args: &mut Args, raw_rest: &[Vec<u8>]) {
    let mut raw_messages = Vec::new();
    let mut raw_author = None;
    let mut i = 0usize;
    while i < raw_rest.len() {
        let arg = raw_rest[i].as_slice();
        if arg == b"--author" {
            if let Some(value) = raw_rest.get(i + 1) {
                raw_author = Some(value.clone());
            }
            i += 2;
            continue;
        }
        if let Some(value) = strip_raw_prefix(arg, b"--author=") {
            raw_author = Some(value.to_vec());
            i += 1;
            continue;
        }
        if arg == b"-m" || arg == b"--message" {
            if let Some(value) = raw_rest.get(i + 1) {
                raw_messages.push(value.clone());
            }
            i += 2;
            continue;
        }
        if let Some(value) = strip_raw_prefix(arg, b"--message=") {
            raw_messages.push(value.to_vec());
            i += 1;
            continue;
        }
        if arg.starts_with(b"-m") && !arg.starts_with(b"--") && arg.len() > 2 {
            raw_messages.push(arg[2..].to_vec());
            i += 1;
            continue;
        }
        i += 1;
    }
    if args.author.is_some() {
        args.raw_author = raw_author;
    }
    if raw_messages.len() == args.message.len() {
        args.raw_messages = raw_messages;
    }
}

/// Capture raw argv values for commit metadata fields that may use `i18n.commitencoding`.
#[cfg(unix)]
pub(crate) fn hydrate_raw_argv(args: &mut Args) {
    let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let Some(commit_pos) = argv.iter().position(|arg| os_arg_bytes(arg) == b"commit") else {
        return;
    };
    let raw_rest: Vec<Vec<u8>> = argv[commit_pos + 1..]
        .iter()
        .map(|arg| os_arg_bytes(arg).to_vec())
        .collect();
    hydrate_raw_commit_argv_values(args, &raw_rest);
}

/// Capture raw argv values for commit metadata fields that may use `i18n.commitencoding`.
#[cfg(not(unix))]
pub(crate) fn hydrate_raw_argv(_args: &mut Args) {}

fn decode_commit_argv_bytes(commit_encoding: Option<&str>, raw: &[u8]) -> String {
    match commit_encoding {
        Some(enc) if !enc.eq_ignore_ascii_case("utf-8") && !enc.eq_ignore_ascii_case("utf8") => {
            grit_lib::commit_encoding::decode_bytes(Some(enc), raw)
        }
        _ => grit_lib::commit_encoding::decode_bytes(None, raw),
    }
}

fn apply_raw_commit_argv_encoding(args: &mut Args, commit_encoding: Option<&str>) {
    if let (Some(raw), Some(_)) = (args.raw_author.as_deref(), args.author.as_ref()) {
        args.author = Some(decode_commit_argv_bytes(commit_encoding, raw));
    }
    if !args.raw_messages.is_empty() && args.raw_messages.len() == args.message.len() {
        args.message = args
            .raw_messages
            .iter()
            .map(|raw| decode_commit_argv_bytes(commit_encoding, raw))
            .collect();
    }
}

/// Parsed `--fixup` value: plain autosquash vs `amend:` / `reword:` forms.
#[derive(Debug, Clone)]
enum FixupMode {
    /// `fixup! <subject>` one-liner (or `-m` append); uses editor only with `--edit`.
    Fixup,
    /// `amend!` / `reword!` message body built from the target commit.
    AmendStyle { is_reword: bool },
}

#[derive(Debug, Clone)]
struct FixupParsed {
    mode: FixupMode,
    commit_ref: String,
}

/// Split `-m` / `-F` (and `=` / glued forms) out of the trailing pathspec bucket.
///
/// Clap routes all trailing tokens into `pathspec`; Git allows `git commit <path> -m msg`.
fn peel_message_flags_from_pathspec(args: &mut Args) {
    let ps = std::mem::take(&mut args.pathspec);
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < ps.len() {
        let a = ps[i].as_str();
        if a == "-m" || a == "--message" {
            if i + 1 < ps.len() {
                args.message.push(ps[i + 1].clone());
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if let Some(rest) = a.strip_prefix("--message=") {
            if !rest.is_empty() {
                args.message.push(rest.to_owned());
            }
            i += 1;
            continue;
        }
        if a == "-F" || a == "--file" {
            if i + 1 < ps.len() {
                if args.file.is_none() {
                    args.file = Some(ps[i + 1].clone());
                }
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if let Some(rest) = a.strip_prefix("--file=") {
            if !rest.is_empty() && args.file.is_none() {
                args.file = Some(rest.to_owned());
            }
            i += 1;
            continue;
        }
        if a.len() > 2 && a.starts_with("-m") && !a.starts_with("--") {
            args.message.push(a[2..].to_owned());
            i += 1;
            continue;
        }
        if a.len() > 2 && a.starts_with("-F") && !a.starts_with("--") {
            if args.file.is_none() {
                args.file = Some(a[2..].to_owned());
                i += 1;
                continue;
            }
        }
        match a {
            "-e" | "--edit" => {
                args.edit = true;
                i += 1;
                continue;
            }
            "--no-edit" => {
                args.no_edit = true;
                i += 1;
                continue;
            }
            "-a" | "--all" => {
                args.all = true;
                i += 1;
                continue;
            }
            "-i" | "--include" => {
                args.include = true;
                i += 1;
                continue;
            }
            "-o" | "--only" => {
                args.only = true;
                i += 1;
                continue;
            }
            "--allow-empty" => {
                args.allow_empty = true;
                i += 1;
                continue;
            }
            "--allow-empty-message" => {
                args.allow_empty_message = true;
                i += 1;
                continue;
            }
            "--amend" => {
                args.amend = true;
                i += 1;
                continue;
            }
            "--author" => {
                if i + 1 < ps.len() {
                    args.author = Some(ps[i + 1].clone());
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            "--date" => {
                if i + 1 < ps.len() {
                    args.date = Some(ps[i + 1].clone());
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            _ => {}
        }
        if let Some(rest) = a.strip_prefix("--author=") {
            args.author = Some(rest.to_owned());
            i += 1;
            continue;
        }
        if let Some(rest) = a.strip_prefix("--date=") {
            args.date = Some(rest.to_owned());
            i += 1;
            continue;
        }
        out.push(ps[i].clone());
        i += 1;
    }
    args.pathspec = out;
}

/// Git-compatible scan of `commit` argv for `-v` / `--verbose` / `--no-verbose`, sets
/// [`GIT_GRIT_COMMIT_VERBOSE_ENV`], and strips `--no-verbose` so clap does not error on unknown long
/// options.
pub(crate) fn preprocess_commit_for_parse(argv: &[String]) -> Vec<String> {
    let mut effective: Option<u32> = None;
    for arg in argv {
        if arg == "--no-verbose" {
            effective = Some(0);
            continue;
        }
        let inc = match arg.as_str() {
            "-v" | "--verbose" => Some(1u32),
            s if s.starts_with('-')
                && !s.starts_with("--")
                && s.len() > 1
                && s[1..].chars().all(|c| c == 'v') =>
            {
                Some(s.len().saturating_sub(1) as u32)
            }
            _ => None,
        };
        if let Some(n) = inc {
            let base = effective.unwrap_or(0);
            effective = Some(base.saturating_add(n));
        }
    }
    if let Some(v) = effective {
        std::env::set_var(GIT_GRIT_COMMIT_VERBOSE_ENV, v.to_string());
    } else {
        let _ = std::env::remove_var(GIT_GRIT_COMMIT_VERBOSE_ENV);
    }

    argv.iter()
        .filter(|a| a.as_str() != "--no-verbose")
        .cloned()
        .collect()
}

/// Run the `commit` command.
pub fn run(mut args: Args) -> Result<()> {
    peel_message_flags_from_pathspec(&mut args);

    crate::commands::add::validate_patch_context_options(
        args.unified,
        args.inter_hunk_context,
        args.patch,
    )?;
    if args.no_auto_advance && !args.patch {
        bail!(
            "the option '{}' requires '{}'",
            "--no-auto-advance",
            "--interactive/--patch"
        );
    }

    // Tests and some scripts pass `-q` after `-m MSG`; if it lands in the
    // trailing pathspec bucket, strip it so we match Git (quiet is already
    // handled by the top-level flag).
    while args
        .pathspec
        .last()
        .is_some_and(|s| s == "-q" || s == "--quiet")
    {
        args.pathspec.pop();
    }

    if args.pathspec_file_nul && args.pathspec_from_file.is_none() {
        bail!("fatal: the option '--pathspec-file-nul' requires '--pathspec-from-file'");
    }

    if let Some(ref psf) = args.pathspec_from_file {
        if args.interactive || args.patch {
            bail!(
                "fatal: options '--pathspec-from-file' and '--interactive/--patch' cannot be used together"
            );
        }
        if args.all {
            bail!("fatal: options '--pathspec-from-file' and '-a' cannot be used together");
        }
        if !args.pathspec.is_empty() {
            bail!("fatal: '--pathspec-from-file' and pathspec arguments cannot be used together");
        }
        let data = if psf == "-" {
            let mut buf = Vec::new();
            std::io::stdin()
                .read_to_end(&mut buf)
                .context("reading pathspecs from stdin")?;
            buf
        } else {
            fs::read(psf).with_context(|| format!("cannot read pathspec file '{psf}'"))?
        };
        args.pathspec =
            grit_lib::pathspec::parse_pathspecs_from_source(&data, args.pathspec_file_nul)?;
    }

    // Validate conflicting options
    let msg_source_count = [
        !args.message.is_empty(),
        args.file.is_some(),
        args.reuse_message.is_some(),
        args.reedit_message.is_some(),
    ]
    .iter()
    .filter(|&&b| b)
    .count();
    if msg_source_count > 1 {
        bail!("Only one of -m, -F, -C, -c can be used.");
    }

    if args.reset_author && args.author.is_some() {
        bail!("options '--reset-author' and '--author' cannot be used together");
    }

    // -a and explicit pathspec don't mix
    if args.all && !args.pathspec.is_empty() {
        bail!(
            "paths '{}' with -a does not make sense",
            args.pathspec.join(" ")
        );
    }

    // --include and --only don't mix
    if args.include && args.only {
        bail!("fatal: options '-i/--include' and '-o/--only' cannot be used together");
    }

    if args.include && (args.interactive || args.patch) {
        bail!(
            "fatal: options '-i/--include' and '--interactive/-p/--patch' cannot be used together"
        );
    }
    if args.only && (args.interactive || args.patch) {
        bail!("fatal: options '-o/--only' and '--interactive/-p/--patch' cannot be used together");
    }
    if args.all && (args.interactive || args.patch) {
        bail!("fatal: options '-a/--all' and '--interactive/-p/--patch' cannot be used together");
    }

    if args.fixup.is_some() && args.squash.is_some() {
        bail!("fatal: options '--squash' and '--fixup' cannot be used together");
    }

    let fixup_parsed: Option<FixupParsed> = if let Some(ref raw) = args.fixup {
        Some(parse_fixup_argument(raw)?)
    } else {
        None
    };

    if let Some(ref fp) = fixup_parsed {
        match &fp.mode {
            FixupMode::AmendStyle { is_reword: true } => {
                if !args.message.is_empty() {
                    bail!("fatal: options '-m' and '--fixup:reword' cannot be used together");
                }
            }
            FixupMode::AmendStyle { is_reword: false } => {
                if !args.message.is_empty() {
                    bail!("fatal: options '-m' and '--fixup:amend' cannot be used together");
                }
            }
            FixupMode::Fixup => {}
        }
    }

    if fixup_parsed
        .as_ref()
        .is_some_and(|f| matches!(f.mode, FixupMode::AmendStyle { is_reword: true }))
        && (args.all
            || args.include
            || args.only
            || args.interactive
            || args.patch
            || !args.pathspec.is_empty())
    {
        if !args.pathspec.is_empty() {
            let p = &args.pathspec[0];
            bail!("fatal: reword option of '--fixup' and path '{p}' cannot be used together");
        }
        bail!("fatal: reword option of '--fixup' and '--patch/--interactive/--all/--include/--only' cannot be used together");
    }

    if fixup_parsed.is_some() {
        if args.reuse_message.is_some() {
            bail!("fatal: options '-C' and '--fixup' cannot be used together");
        }
        if args.reedit_message.is_some() {
            bail!("fatal: options '-c' and '--fixup' cannot be used together");
        }
        if args.file.is_some() {
            bail!("fatal: options '-F' and '--fixup' cannot be used together");
        }
    }

    let fixup_amend_style = fixup_parsed
        .as_ref()
        .is_some_and(|f| matches!(f.mode, FixupMode::AmendStyle { .. }));
    let fixup_amend_message_only = fixup_parsed
        .as_ref()
        .is_some_and(|f| matches!(f.mode, FixupMode::AmendStyle { is_reword: false }));
    if args.pathspec.is_empty()
        && (args.include
            || (args.only
                && !args.allow_empty
                && (!args.amend || (fixup_parsed.is_some() && !fixup_amend_style))
                && !fixup_amend_message_only))
    {
        bail!("fatal: No paths with --include/--only does not make sense.");
    }

    let repo = Repository::discover(None).context("not a git repository")?;

    let had_cp_head = repo.git_dir.join("CHERRY_PICK_HEAD").exists();
    let had_rv_head = repo.git_dir.join("REVERT_HEAD").exists();
    let seq_todo_path = repo.git_dir.join("sequencer").join("todo");
    let resume_pick_after_cp = had_cp_head && seq_todo_path.exists();
    let resume_revert_after_rv = had_rv_head && seq_todo_path.exists();
    // Git's `sequencer_determine_whence`: a stopped interactive `pick` leaves CHERRY_PICK_HEAD
    // *and* a rebase state dir with REBASE_HEAD == CHERRY_PICK_HEAD. In that case `commit` reports
    // a rebase (not a cherry-pick) for partial-commit / amend errors (t3404 118/119).
    let from_rebase_pick = had_cp_head && commit_is_rebase_pick_whence(&repo.git_dir);

    if grit_lib::precompose_config::effective_core_precomposeunicode(Some(&repo.git_dir)) {
        for ps in &mut args.pathspec {
            *ps = grit_lib::unicode_normalization::precompose_utf8_path(ps).into_owned();
        }
    }

    let head = resolve_head(&repo.git_dir)?;
    let parent_tree_oid = if let Some(head_oid) = head.oid() {
        let obj = repo.odb.read(head_oid)?;
        let commit = grit_lib::objects::parse_commit(&obj.data)?;
        Some(commit.tree)
    } else {
        None
    };

    let work_tree = repo.work_tree.as_deref();

    let reset_author_allowed = args.amend
        || args.reuse_message.is_some()
        || args.reedit_message.is_some()
        || repo.git_dir.join("CHERRY_PICK_HEAD").exists()
        || repo.git_dir.join("REBASE_HEAD").exists();
    if args.reset_author && !reset_author_allowed {
        bail!("--reset-author can be used only with -C, -c or --amend.");
    }

    // If -a, stage all tracked file changes first
    if args.all {
        if let Some(wt) = work_tree {
            auto_stage_tracked(&repo, wt)?;
        }
    }

    // If pathspec given, stage those files. A real commit persists to disk; `--dry-run` only
    // simulates staging in memory so status output matches Git without mutating the index.
    // `commit -p` with pathspec does not pre-stage like a normal pathspec commit; partial trees
    // still use the path list for `write_tree_partial_from_index`.
    let mut pathspec_matched: Option<HashSet<Vec<u8>>> = if args.patch || args.interactive {
        if args.pathspec.is_empty() {
            None
        } else {
            let Some(wt) = work_tree else {
                bail!("pathspec requires a work tree");
            };
            Some(
                commit_patch_pathspec_targets(wt, &args.pathspec)?
                    .into_iter()
                    .map(|s| s.into_bytes())
                    .collect(),
            )
        }
    } else if !args.pathspec.is_empty() && !args.dry_run {
        let Some(wt) = work_tree else {
            bail!("pathspec requires a work tree");
        };
        let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
        let core_filemode = config
            .get_bool("core.filemode")
            .and_then(|r| r.ok())
            .unwrap_or(true);
        let precompose_unicode =
            grit_lib::precompose_config::effective_core_precomposeunicode(Some(&repo.git_dir));
        let sparse_state = crate::commands::add::AddSparseState::load(&repo, &config);
        let add_cfg = crate::commands::add::AddConfig {
            core_filemode,
            precompose_unicode,
            ignore_errors: false,
            conv: grit_lib::crlf::ConversionConfig::from_config(&config),
            attrs: grit_lib::crlf::load_gitattributes(wt),
            config,
            sparse: sparse_state,
            include_sparse: false,
            large_blobs: None,
        };
        Some(crate::commands::add::stage_pathspecs_for_commit(
            &repo,
            wt,
            &args.pathspec,
            &add_cfg,
        )?)
    } else {
        None
    };

    let index_path = resolved_index_path(&repo);

    // For `commit --interactive` (`-i`), the selections are staged into a temporary index and the
    // real on-disk index is left untouched until the commit succeeds (so an aborted commit does not
    // mutate the index — t7501). Carry the staged index here so the post-commit refresh can persist
    // it once the commit is final.
    let mut interactive_staged_index: Option<Index> = None;
    let index = if args.interactive && !args.patch {
        let Some(wt) = work_tree else {
            bail!("this operation must be run in a work tree");
        };
        let staged = run_commit_interactive_mode(&repo, wt, &args)?;
        interactive_staged_index = Some(staged.clone());
        staged
    } else if args.patch {
        let Some(wt) = work_tree else {
            bail!("this operation must be run in a work tree");
        };
        run_commit_patch_mode(&repo, wt, &args, &head, parent_tree_oid.as_ref())?
    } else {
        let mut idx = match repo.load_index_at(&index_path) {
            Ok(i) => i,
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Index::new(),
            Err(e) => return Err(e.into()),
        };

        if !args.dry_run && idx.entries.iter().any(|e| e.stage() != 0) {
            eprintln!("error: Committing is not possible because you have unmerged files.");
            eprintln!("hint: Fix them up in the work tree, and then use 'git add/rm <file>'");
            eprintln!("hint: as appropriate to mark resolution and make a commit.");
            eprintln!("fatal: Exiting because of an unresolved conflict.");
            std::process::exit(128);
        }

        if args.dry_run && !args.pathspec.is_empty() {
            let Some(wt) = work_tree else {
                bail!("pathspec requires a work tree");
            };
            pathspec_matched = Some(apply_pathspec_to_index(
                &repo,
                wt,
                &mut idx,
                &args.pathspec,
            )?);
        }
        idx
    };

    let has_unmerged_entries = index.entries.iter().any(|e| e.stage() != 0);
    if has_unmerged_entries && !args.dry_run {
        eprintln!("error: Committing is not possible because you have unmerged files.");
        eprintln!("hint: Fix them up in the work tree, and then use 'git add/rm <file>'");
        eprintln!("hint: as appropriate to mark resolution and make a commit.");
        eprintln!("fatal: Exiting because of an unresolved conflict.");
        std::process::exit(128);
    }

    // Write tree: pathspec commits record only matched paths, except `--include`,
    // which stages named paths and commits the whole index.
    let tree_oid = match (&pathspec_matched, &parent_tree_oid) {
        (Some(paths), Some(base)) if !paths.is_empty() && !args.include => {
            match write_tree_partial_from_index(&repo.odb, &index, base, paths) {
                Ok(oid) => oid,
                Err(err) => {
                    if is_permission_denied_error(&err) {
                        eprintln!(
                            "error: insufficient permission for adding an object to repository database .git/objects"
                        );
                        eprintln!("error: Error building trees");
                        std::process::exit(128);
                    }
                    return Err(err.into());
                }
            }
        }
        (Some(paths), None) if !paths.is_empty() && !args.include => {
            match write_tree_from_index_subset(&repo.odb, &index, paths) {
                Ok(oid) => oid,
                Err(err) => {
                    if is_permission_denied_error(&err) {
                        eprintln!(
                            "error: insufficient permission for adding an object to repository database .git/objects"
                        );
                        eprintln!("error: Error building trees");
                        std::process::exit(128);
                    }
                    return Err(err.into());
                }
            }
        }
        _ => match write_tree_from_index(&repo.odb, &index, "") {
            Ok(oid) => oid,
            Err(err) => {
                if is_permission_denied_error(&err) {
                    eprintln!(
                        "error: insufficient permission for adding an object to repository database .git/objects"
                    );
                    eprintln!("error: Error building trees");
                    std::process::exit(128);
                }
                return Err(err.into());
            }
        },
    };

    // `git commit --dry-run <pathspec>` prints the commit tree that would be recorded (partial
    // merge) and omits the "Changes not staged" section (t7508).
    let dry_run_pathspec_status = args.dry_run && !args.pathspec.is_empty() && head.oid().is_some();

    let mut parents = Vec::new();
    let old_head_oid = head.oid().cloned();

    if had_cp_head && args.amend {
        if from_rebase_pick {
            eprintln!("fatal: You are in the middle of a rebase -- cannot amend.");
        } else {
            eprintln!("fatal: You are in the middle of a cherry-pick -- cannot amend.");
        }
        std::process::exit(128);
    }
    if had_rv_head && args.amend {
        eprintln!("fatal: You are in the middle of a revert -- cannot amend.");
        std::process::exit(128);
    }

    if had_cp_head && !args.pathspec.is_empty() {
        if from_rebase_pick {
            eprintln!("fatal: cannot do a partial commit during a rebase.");
        } else {
            eprintln!("fatal: cannot do a partial commit during a cherry-pick.");
        }
        std::process::exit(128);
    }
    if had_rv_head && !args.pathspec.is_empty() {
        eprintln!("fatal: cannot do a partial commit during a revert.");
        std::process::exit(128);
    }

    if args.amend {
        // Amend: use the parent(s) of the current HEAD commit
        if let Some(head_oid) = head.oid() {
            let obj = repo.odb.read(head_oid)?;
            let commit = grit_lib::objects::parse_commit(&obj.data)?;
            parents = commit.parents;
        }
    } else {
        let merge_heads = grit_lib::state::read_merge_heads(&repo.git_dir)?;
        if merge_heads.len() > 1 {
            // Octopus / multi-head merge in conflict: Git records parents as `MERGE_HEAD` lines
            // only (sequential internal merges are not parents of the resolution commit; t7603).
            parents.extend(merge_heads);
        } else {
            if let Some(head_oid) = head.oid() {
                parents.push(*head_oid);
            }
            parents.extend(merge_heads);
        }
    }

    let head_tree = match head.oid() {
        Some(oid) => {
            let obj = repo.odb.read(oid)?;
            let c = grit_lib::objects::parse_commit(&obj.data)?;
            Some(c.tree)
        }
        None => None,
    };

    let skip_index_tree_vs_parent = fixup_parsed
        .as_ref()
        .is_some_and(|f| matches!(f.mode, FixupMode::AmendStyle { .. }));

    // `--fixup=reword:` and `--fixup=amend: --only` record a new commit with the same tree as
    // `HEAD` while leaving the index (and staged changes) untouched — matching Git's behavior
    // for autosquash helper commits.
    let mut tree_oid = tree_oid;
    if let Some(ref fp) = fixup_parsed {
        if matches!(fp.mode, FixupMode::AmendStyle { is_reword: true })
            || (matches!(fp.mode, FixupMode::AmendStyle { is_reword: false }) && args.only)
        {
            let Some(t) = head_tree else {
                bail!("nothing to commit");
            };
            tree_oid = t;
        }
    }
    if args.only && args.pathspec.is_empty() && fixup_parsed.is_none() {
        let Some(t) = head_tree else {
            bail!("nothing to commit");
        };
        tree_oid = t;
    }

    // For initial commits with empty tree (only ITA entries), fail
    if !args.allow_empty && parents.is_empty() {
        let empty_tree =
            grit_lib::objects::ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904")
                .unwrap_or(tree_oid);
        if tree_oid == empty_tree {
            bail!("nothing to commit");
        }
    }

    let config = ConfigSet::load(Some(&repo.git_dir), true)?;
    let commit_encoding = config
        .get("i18n.commitEncoding")
        .or_else(|| config.get("i18n.commitencoding"));
    apply_raw_commit_argv_encoding(&mut args, commit_encoding.as_deref());

    let status_base_tree = if args.amend {
        if let Some(parent_oid) = parents.first() {
            let parent_obj = repo.odb.read(parent_oid)?;
            let parent_commit = grit_lib::objects::parse_commit(&parent_obj.data)?;
            Some(parent_commit.tree)
        } else {
            None
        }
    } else {
        head_tree
    };

    let mut staged = if dry_run_pathspec_status {
        diff_trees(&repo.odb, status_base_tree.as_ref(), Some(&tree_oid), "")?
    } else {
        diff_index_to_tree(&repo.odb, &index, status_base_tree.as_ref(), false)?
    };
    let unstaged_raw = if dry_run_pathspec_status {
        Vec::new()
    } else if let Some(wt) = work_tree {
        diff_index_to_worktree(&repo.odb, &index, wt, false, false)?
    } else {
        Vec::new()
    };
    let (rename_threshold, rename_copies) = commit_rename_settings(&config);
    if let Some(th) = rename_threshold {
        staged = status_apply_rename_copy_detection(
            &repo.odb,
            staged,
            th,
            rename_copies,
            status_base_tree.as_ref(),
        )?;
    }
    let mut unstaged = if let Some(th) = rename_threshold {
        status_apply_rename_copy_detection(
            &repo.odb,
            unstaged_raw,
            th,
            rename_copies,
            head_tree.as_ref(),
        )?
    } else {
        unstaged_raw
    };
    let unmerged_full = crate::commands::status::unmerged_paths_and_mask(&index);
    let unmerged_keys: BTreeSet<String> = unmerged_full.keys().cloned().collect();
    staged.retain(|e| !unmerged_keys.contains(e.path()));
    unstaged.retain(|e| !unmerged_keys.contains(e.path()));
    // `-u<mode>` / `--untracked-files`: `no` suppresses the untracked listing entirely (Git prints
    // "Untracked files not listed (use -u option to show untracked files)" instead). The default
    // and `normal`/`all` collect untracked files (t7508 commit -uno --dry-run).
    let untracked_mode = args
        .untracked_files
        .as_deref()
        .map(|s| s.to_ascii_lowercase())
        .or_else(|| {
            config
                .get("status.showUntrackedFiles")
                .map(|v| v.to_ascii_lowercase())
        })
        .unwrap_or_else(|| "normal".to_owned());
    let hide_untracked = matches!(untracked_mode.as_str(), "no" | "false" | "off" | "0");
    let untracked = if hide_untracked {
        Vec::new()
    } else if let Some(wt) = work_tree {
        find_untracked_files(&repo, wt, &index, None)?
    } else {
        Vec::new()
    };

    // --dry-run: show status (including tracking) even when there is nothing to commit (Git behavior).
    if args.dry_run {
        let mut no_ab = args.no_ahead_behind;
        if args.ahead_behind {
            no_ab = false;
        } else if !args.no_ahead_behind {
            if let Some(v) = config.get("status.aheadbehind") {
                if matches!(
                    v.to_ascii_lowercase().as_str(),
                    "false" | "no" | "off" | "0"
                ) {
                    no_ab = true;
                }
            }
        }
        let in_progress = detect_in_progress(&repo.git_dir);
        print_dry_run(
            &repo,
            &config,
            &head,
            &staged,
            &unstaged,
            &untracked,
            &unmerged_full,
            &in_progress,
            pathspec_matched.as_ref(),
            no_ab,
            args.amend,
            &index_path,
            &index,
            hide_untracked,
        )?;
        if has_unmerged_entries {
            std::process::exit(1);
        }
        // Match Git: `commit --dry-run` exits 1 when there is nothing to commit (after printing status).
        // Merge commits are allowed even when the index matches `HEAD^{tree}` (e.g. resolving
        // modify/delete by keeping our version — tree unchanged but we still record the merge).
        if !args.allow_empty
            && !args.amend
            && !skip_index_tree_vs_parent
            && !has_unmerged_entries
            && staged.is_empty()
            && !parents.is_empty()
            && parents.len() == 1
        {
            let parent_obj = repo.odb.read(&parents[0])?;
            let parent_commit = grit_lib::objects::parse_commit(&parent_obj.data)?;
            if parent_commit.tree == tree_oid {
                if work_tree.is_some() {
                    if !unstaged.is_empty() {
                        println!(
                            "no changes added to commit (use \"git add\" and/or \"git commit -a\")"
                        );
                    } else if !untracked.is_empty() {
                        println!(
                            "nothing added to commit but untracked files present (use \"git add\" to track)"
                        );
                    }
                }
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    if !args.allow_empty
        && !args.amend
        && !skip_index_tree_vs_parent
        && staged.is_empty()
        && !parents.is_empty()
        && parents.len() == 1
    {
        let parent_obj = repo.odb.read(&parents[0])?;
        let parent_commit = grit_lib::objects::parse_commit(&parent_obj.data)?;
        if parent_commit.tree == tree_oid {
            if work_tree.is_some() {
                if !unstaged.is_empty() {
                    println!(
                        "no changes added to commit (use \"git add\" and/or \"git commit -a\")"
                    );
                } else if !untracked.is_empty() {
                    println!(
                        "nothing added to commit but untracked files present (use \"git add\" to track)"
                    );
                }
            }
            if had_cp_head {
                eprintln!("hint: try \"git cherry-pick --skip\"");
            } else if repo.git_dir.join("REBASE_HEAD").exists() {
                eprintln!("hint: try \"git rebase --skip\"");
            }
            bail!("nothing to commit, working tree clean");
        }
    }

    let template_path = resolve_commit_template_path(&args, &config)?;
    let use_editor_for_message = commit_uses_editor(&args, fixup_parsed.as_ref());
    if use_editor_for_message {
        validate_explicit_committer_identity(&config)?;
        let _ = resolve_committer(&config, OffsetDateTime::now_utc())?;
    }

    let verbose_level = resolve_commit_verbose_level(&args, &config);
    let commit_cleanup_mode = resolve_commit_cleanup_mode(&args, &config, use_editor_for_message);
    // For editor commits, `prepare-commit-msg` must run on the full template buffer *before*
    // the editor opens (Git `prepare_to_commit`: hook then editor). This closure is invoked at
    // each editor-launch site inside `prepare_commit_message`. Non-editor commits run the hook
    // afterwards (below) on the assembled message instead.
    let prepare_hook = |msg_file: &Path| -> Result<()> {
        run_prepare_commit_msg_hook_on(
            &repo,
            &args,
            index_path.as_path(),
            use_editor_for_message,
            msg_file,
        )
    };
    let msg_result = prepare_commit_message(
        &args,
        &repo,
        &config,
        fixup_parsed.as_ref(),
        template_path.as_deref(),
        use_editor_for_message,
        &head,
        &staged,
        &unstaged,
        verbose_level,
        &prepare_hook,
    )?;
    let mut message = normalize_autosquash_editor_message(
        &args,
        fixup_parsed.as_ref(),
        use_editor_for_message,
        &msg_result.message,
    );
    let mut raw_message = msg_result.raw_bytes;
    let post_editor_editmsg = if use_editor_for_message {
        fs::read(repo.git_dir.join("COMMIT_EDITMSG")).ok()
    } else {
        None
    };
    let template_for_aborted_check = template_path.filter(|_| use_editor_for_message);

    // prepare-commit-msg runs for normal commits (not skipped by `--no-verify`; only pre-commit
    // and commit-msg are). For editor commits the hook already ran on the full template before the
    // editor opened (inside `prepare_commit_message`), matching Git's `prepare_to_commit` order;
    // running it again here would operate on the post-editor, comment-stripped buffer. So only the
    // non-editor path writes COMMIT_EDITMSG and lets the hook edit it in place.
    if !use_editor_for_message {
        let msg_file = repo.git_dir.join("COMMIT_EDITMSG");
        if let Some(ref raw) = raw_message {
            fs::write(&msg_file, raw)?;
        } else {
            fs::write(&msg_file, message.as_bytes())?;
        }
        run_prepare_commit_msg_hook_on(&repo, &args, index_path.as_path(), false, &msg_file)?;
        let new_raw = fs::read(&msg_file)?;
        // Preserve the verbatim bytes when the source message carried raw bytes
        // (a `-F` file or non-UTF-8 content); the commit body must be stored as-is
        // and never transcoded. Only fall back to a plain UTF-8 string when the
        // message had no raw representation to begin with.
        let had_raw = raw_message.is_some();
        match String::from_utf8(new_raw.clone()) {
            Ok(s) if !had_raw => {
                message = s;
                raw_message = None;
            }
            Ok(s) => {
                message = s;
                raw_message = Some(new_raw);
            }
            Err(_) => {
                message = String::from_utf8_lossy(&new_raw).to_string();
                raw_message = Some(new_raw);
            }
        }
    }

    let empty_message = if commit_cleanup_mode == CommitMsgCleanupMode::None {
        message.is_empty()
    } else {
        message.trim().is_empty() || message_is_empty_or_signedoff_only(&message)
    };
    if empty_message && !args.allow_empty_message {
        eprintln!("Aborting commit due to empty commit message.");
        std::process::exit(1);
    }

    if let Some(ref tpl) = template_for_aborted_check {
        let cp = comment_line_prefix_full(&config);
        if commit_cleanup_mode != CommitMsgCleanupMode::None
            && template_untouched(&message, tpl, cp.as_ref(), commit_cleanup_mode)
            && !args.allow_empty_message
        {
            eprintln!("Aborting commit; you did not edit the message.");
            std::process::exit(1);
        }
    }

    if fixup_parsed.as_ref().is_some_and(|f| {
        matches!(f.mode, FixupMode::AmendStyle { .. }) && message.starts_with("amend! ")
    }) && !args.allow_empty_message
    {
        let body = message_after_first_line(&message);
        if body.trim().is_empty() {
            eprintln!("Aborting commit due to empty commit message body.");
            std::process::exit(1);
        }
    }

    let now = OffsetDateTime::now_utc();

    // When amending, preserve original author unless explicitly overridden
    let amend_author = if args.amend
        && !args.reset_author
        && args.author.is_none()
        && args.reuse_message.is_none()
        && args.reedit_message.is_none()
        && args.date.is_none()
    {
        if let Some(head_oid) = head.oid() {
            let obj = repo.odb.read(head_oid)?;
            let commit = grit_lib::objects::parse_commit(&obj.data)?;
            validate_amend_source_author(&commit.author)?;
            Some((commit.author, commit.author_raw))
        } else {
            None
        }
    } else {
        None
    };
    let (author, mut author_raw) = if let Some((preserved_author, preserved_raw)) = amend_author {
        (preserved_author, preserved_raw)
    } else {
        (resolve_author(&args, &config, &repo, now)?, Vec::new())
    };
    let committer = resolve_committer(&config, now)?;

    let author_hook_env = author_env_for_commit_hooks(&author)?;
    let author_env_refs: Vec<(&str, &str)> = author_hook_env
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let hook_editor = if use_editor_for_message {
        None
    } else {
        Some(":")
    };
    let hook_opts = CommitHookEnv {
        index_file: Some(index_path.as_path()),
        git_editor: hook_editor,
        git_prefix: None,
        extra_env: author_env_refs.as_slice(),
    };

    if !args.no_verify {
        let r = run_commit_hook(&repo, "pre-commit", &[], None, &hook_opts)
            .map_err(|e| anyhow::anyhow!(e))?;
        if let HookResult::Failed(code) = r {
            bail!("pre-commit hook exited with status {code}");
        }
    }

    // Append Signed-off-by trailer if --signoff
    if args.signoff {
        let trailer = if let Some(angle_end) = committer.find('>') {
            format!("Signed-off-by: {}", &committer[..=angle_end])
        } else {
            format!("Signed-off-by: {committer}")
        };
        let name_email = if let Some(angle_end) = committer.find('>') {
            committer[..=angle_end].to_string()
        } else {
            committer.clone()
        };
        let already_has_sob = message.lines().any(|l| {
            l.trim_start()
                .strip_prefix("Signed-off-by:")
                .is_some_and(|rest| rest.trim() == name_email)
        });
        if !already_has_sob && !message.contains(&trailer) {
            const SCISSORS: &str = "# ------------------------ >8 ------------------------";
            fn unsigned_conflicts_start(msg: &str) -> Option<usize> {
                if let Some(i) = msg.find("\nConflicts:") {
                    return Some(i + 1);
                }
                if msg.starts_with("Conflicts:") {
                    return Some(0);
                }
                None
            }
            fn insert_trailer_before(msg: &str, pos: usize, trailer: &str) -> String {
                let before = msg[..pos].trim_end_matches(['\n', '\r', ' ', '\t']);
                let after = &msg[pos..];
                format!("{before}\n\n{trailer}\n{after}")
            }

            if message.trim().is_empty() {
                message = format!("\n\n{trailer}\n");
                if raw_message.is_some() {
                    raw_message = Some(message.clone().into_bytes());
                }
            } else if (had_cp_head || had_rv_head) && message.contains(SCISSORS) {
                if let Some(pos) = message.find(SCISSORS) {
                    message = insert_trailer_before(&message, pos, &trailer);
                    if let Some(ref raw) = raw_message {
                        if let Ok(s) = std::str::from_utf8(raw) {
                            if let Some(p) = s.find(SCISSORS) {
                                raw_message =
                                    Some(insert_trailer_before(s, p, &trailer).into_bytes());
                            }
                        }
                    }
                }
            } else if args.amend {
                if let Some(pos) = unsigned_conflicts_start(&message) {
                    message = insert_trailer_before(&message, pos, &trailer);
                    if let Some(ref raw) = raw_message {
                        if let Ok(s) = std::str::from_utf8(raw) {
                            if let Some(p) = unsigned_conflicts_start(s) {
                                raw_message =
                                    Some(insert_trailer_before(s, p, &trailer).into_bytes());
                            }
                        }
                    }
                } else {
                    let mut signed = message.clone();
                    grit_lib::commit_trailers::append_signoff_trailer(
                        &mut signed,
                        &format!("{trailer}\n"),
                        &config,
                    );
                    message = signed;
                    if let Some(ref raw) = raw_message {
                        if let Ok(s) = std::str::from_utf8(raw) {
                            let mut signed = s.to_owned();
                            grit_lib::commit_trailers::append_signoff_trailer(
                                &mut signed,
                                &format!("{trailer}\n"),
                                &config,
                            );
                            raw_message = Some(signed.into_bytes());
                        }
                    }
                }
            } else {
                let mut signed = message.clone();
                grit_lib::commit_trailers::append_signoff_trailer(
                    &mut signed,
                    &format!("{trailer}\n"),
                    &config,
                );
                message = signed;
                if let Some(ref raw) = raw_message {
                    if let Ok(s) = std::str::from_utf8(raw) {
                        let mut signed = s.to_owned();
                        grit_lib::commit_trailers::append_signoff_trailer(
                            &mut signed,
                            &format!("{trailer}\n"),
                            &config,
                        );
                        raw_message = Some(signed.into_bytes());
                    }
                }
            }
        }
    }

    if !args.trailer.is_empty() {
        let trailer_args: Vec<NewTrailerArg> = args
            .trailer
            .iter()
            .map(|text| NewTrailerArg {
                text: text.clone(),
                where_: Default::default(),
                if_exists: Default::default(),
                if_missing: Default::default(),
            })
            .collect();
        let trailer_opts = ProcessTrailerOptions {
            no_divider: true,
            ..Default::default()
        };
        message = grit_lib::interpret_trailers::process_trailers(
            &message,
            &trailer_opts,
            &trailer_args,
            Some(repo.git_dir.as_path()),
        );
        if let Some(ref raw) = raw_message {
            if let Ok(s) = std::str::from_utf8(raw) {
                raw_message = Some(
                    grit_lib::interpret_trailers::process_trailers(
                        s,
                        &trailer_opts,
                        &trailer_args,
                        Some(repo.git_dir.as_path()),
                    )
                    .into_bytes(),
                );
            }
        }
    }

    // commit-msg hook (skipped with `--no-verify` / `-n`).
    if !args.no_verify {
        let msg_file = repo.git_dir.join("COMMIT_EDITMSG");
        let pre_commit_msg_hook_editmsg = post_editor_editmsg
            .clone()
            .or_else(|| fs::read(&msg_file).ok());
        // When finishing a conflicted cherry-pick/revert, git keeps the trailing `# Conflicts:`
        // comment block in COMMIT_EDITMSG even though it is stripped from the committed message
        // — and a `-s` sign-off sits above it (t3507 "commit after failed cherry-pick adds -s at
        // the right place"). The committed `message` is already clean+signed; re-attach the
        // conflicts comment block (read from the still-present MERGE_MSG) for the on-disk editmsg.
        let editmsg_conflicts_suffix = if had_cp_head || had_rv_head {
            conflicts_comment_block(&repo.git_dir, &config)
        } else {
            None
        };
        if let (Some(suffix), None) = (&editmsg_conflicts_suffix, &raw_message) {
            let mut editmsg = message.clone();
            if !editmsg.ends_with('\n') {
                editmsg.push('\n');
            }
            editmsg.push_str(suffix);
            fs::write(&msg_file, &editmsg)?;
        } else if let Some(ref raw) = raw_message {
            fs::write(&msg_file, raw)?;
        } else {
            fs::write(&msg_file, &message)?;
        }
        let msg_path_str = msg_file.to_string_lossy().to_string();
        let r = run_commit_hook(&repo, "commit-msg", &[&msg_path_str], None, &hook_opts)
            .map_err(|e| anyhow::anyhow!(e))?;
        match r {
            HookResult::Failed(code) => {
                bail!("commit-msg hook exited with status {code}");
            }
            HookResult::Success => {
                let new_raw = fs::read(&msg_file)?;
                match String::from_utf8(new_raw.clone()) {
                    Ok(s) => {
                        let cp = comment_line_prefix_full(&config);
                        message = apply_cleanup_message(
                            &s,
                            verbose_level,
                            cp.as_ref(),
                            commit_cleanup_mode,
                        );
                        raw_message = None;
                    }
                    Err(_) => {
                        message = String::from_utf8_lossy(&new_raw).to_string();
                        raw_message = Some(new_raw);
                    }
                }
            }
            HookResult::NotFound => {
                if let Some(previous) = pre_commit_msg_hook_editmsg {
                    let cp = comment_line_prefix_full(&config);
                    let restored = if post_editor_editmsg.is_some()
                        && (commit_cleanup_mode != CommitMsgCleanupMode::None
                            || args.signoff
                            || !args.trailer.is_empty())
                    {
                        replace_editmsg_user_message(&previous, &message, cp.as_ref())
                    } else {
                        previous
                    };
                    fs::write(&msg_file, restored)?;
                }
            }
        }
    }

    message = ensure_trailing_newline(&message);
    if let Some(ref mut raw) = raw_message {
        if !raw.ends_with(b"\n") {
            raw.push(b'\n');
        }
    }

    // Build commit object — set encoding header when i18n.commitEncoding is configured
    // and differs from UTF-8.
    let encoding = match &commit_encoding {
        Some(enc) if !enc.eq_ignore_ascii_case("utf-8") && !enc.eq_ignore_ascii_case("utf8") => {
            Some(enc.clone())
        }
        _ => None,
    };
    let mut committer_raw = Vec::new();
    if let Some(ref enc_label) = encoding {
        // Git stores the identity lines verbatim and never transcodes them; the
        // raw author/committer name bytes are written exactly as configured (in
        // the same charset as i18n.commitEncoding). We only re-encode when the
        // resolved identity actually contains non-ASCII characters — for ASCII
        // (the common case) the UTF-8 string is already the correct byte
        // sequence, and encoding_rs's ISO-2022-JP encoder would otherwise mangle
        // control bytes via HTML entity escaping.
        if author_raw.is_empty() && !author.is_ascii() {
            author_raw = grit_lib::commit_encoding::encode_header_text(enc_label, &author)
                .ok_or_else(|| anyhow::anyhow!("unsupported i18n.commitencoding: {enc_label}"))?;
        }
        if !committer.is_ascii() {
            committer_raw = grit_lib::commit_encoding::encode_header_text(enc_label, &committer)
                .ok_or_else(|| anyhow::anyhow!("unsupported i18n.commitencoding: {enc_label}"))?;
        }
        // The commit message body is stored verbatim when it already has a raw
        // byte representation (e.g. a `-F` file already in this charset). When the
        // message originates from a Unicode source (`-m`, an editor, or a `-s`
        // sign-off trailer built from a non-ASCII committer name), its bytes must
        // match the declared encoding: encode the Unicode body to the charset so a
        // Latin-1 / EUC-JP committer name in the trailer is stored in that charset
        // rather than as mislabeled UTF-8.
        if raw_message.is_none() && !message.is_ascii() {
            let body = message.trim_end_matches('\n');
            if let Some(encoded) = grit_lib::commit_encoding::encode_unicode(enc_label, body) {
                raw_message = Some(encoded);
            }
        }
    }

    // Git refuses a NUL byte anywhere in the commit log message (commit.c:
    // commit_tree_extended). A UTF-16 message file, for example, is rejected here.
    {
        let body_bytes: &[u8] = match raw_message {
            Some(ref r) => r.as_slice(),
            None => message.as_bytes(),
        };
        if body_bytes.contains(&0) {
            bail!("a NUL byte in commit log message not allowed.");
        }
    }

    // When the effective encoding is UTF-8 (no encoding header), Git still
    // validates the assembled message body and warns when it is not strictly
    // valid UTF-8 (see commit.c:verify_utf8). Replicate that warning here.
    if encoding.is_none() {
        let body_bytes: &[u8] = match raw_message {
            Some(ref r) => r.as_slice(),
            None => message.as_bytes(),
        };
        if !grit_lib::commit_encoding::is_strict_utf8(body_bytes) {
            eprintln!("Warning: commit message did not conform to UTF-8.");
            eprintln!("You may want to amend it after fixing the message, or set the config");
            eprintln!("variable i18n.commitEncoding to the encoding your project uses.");
        }
    }
    let commit_data = CommitData {
        tree: tree_oid,
        parents,
        author,
        committer,
        author_raw,
        committer_raw,
        encoding,
        message,
        raw_message,
    };

    let mut commit_bytes = serialize_commit(&commit_data);
    if should_sign_commit(&args, &config) {
        commit_bytes = sign_commit_bytes(
            &config,
            &commit_data.committer,
            args.gpg_sign.as_deref(),
            commit_bytes,
        )?;
    }
    let commit_oid = repo.odb.write(ObjectKind::Commit, &commit_bytes)?;

    // Update HEAD
    let old_oid = head.oid().copied().unwrap_or_else(ObjectId::zero);
    ensure_head_unchanged(&repo.git_dir, &head)?;
    update_head(&repo.git_dir, &head, &commit_oid)?;

    let zero_oid = ObjectId::zero();
    let mut amend_reattached_ref: Option<String> = None;

    // `git commit --amend` with detached HEAD: if exactly one local branch still points at the
    // pre-amend commit, move that branch to the new commit and attach HEAD (matches Git; t3428).
    if args.amend && head.is_detached() && old_oid != zero_oid {
        let mut branches = Vec::new();
        if let Ok(refs) = list_refs(&repo.git_dir, "refs/heads/") {
            for (name, tip) in refs {
                if tip == old_oid {
                    branches.push(name);
                }
            }
        }
        branches.sort();
        if branches.len() == 1 {
            let refname = branches[0].clone();
            let ref_path = repo.git_dir.join(&refname);
            if let Some(parent) = ref_path.parent() {
                fs::create_dir_all(parent)?;
            }
            write_ref(&repo.git_dir, &refname, &commit_oid)?;
            fs::write(repo.git_dir.join("HEAD"), format!("ref: {refname}\n"))?;
            amend_reattached_ref = Some(refname);
        }
    }

    let reflog_msg = if head.is_unborn() {
        format!(
            "commit (initial): {}",
            commit_data.message.lines().next().unwrap_or("")
        )
    } else if args.amend {
        format!(
            "commit (amend): {}",
            commit_data.message.lines().next().unwrap_or("")
        )
    } else if commit_data.parents.len() >= 2 {
        format!(
            "commit (merge): {}",
            commit_data.message.lines().next().unwrap_or("")
        )
    } else {
        format!(
            "commit: {}",
            commit_data.message.lines().next().unwrap_or("")
        )
    };

    match &head {
        HeadState::Branch { refname, .. } => {
            if repo
                .git_dir
                .join("logs")
                .join(refname)
                .metadata()
                .map(|_| true)
                .unwrap_or(false)
                || should_autocreate_reflog(&repo.git_dir, refname)
            {
                append_reflog(
                    &repo.git_dir,
                    refname,
                    &old_oid,
                    &commit_oid,
                    &commit_data.committer,
                    &reflog_msg,
                    false,
                )?;
            }
            // Append the same entry to `logs/HEAD` instead of replacing it with a copy of
            // `logs/refs/heads/<branch>` — mirroring would drop `checkout: moving from …`
            // lines and break `git switch -` / `@{-1}` (t3452-history-split).
            append_reflog(
                &repo.git_dir,
                "HEAD",
                &old_oid,
                &commit_oid,
                &commit_data.committer,
                &reflog_msg,
                false,
            )?;
        }
        _ => {
            append_reflog(
                &repo.git_dir,
                "HEAD",
                &old_oid,
                &commit_oid,
                &commit_data.committer,
                &reflog_msg,
                false,
            )?;
        }
    }
    if let Some(ref refname) = amend_reattached_ref {
        append_reflog(
            &repo.git_dir,
            refname,
            &old_oid,
            &commit_oid,
            &commit_data.committer,
            &reflog_msg,
            false,
        )?;
    }

    let _ = run_reference_transaction_committed_for_head_update(
        &repo,
        &head,
        head.oid().copied(),
        commit_oid,
    );

    let _ = grit_lib::rerere::rerere_post_commit(&repo);
    if std::env::var("GIT_TEST_NO_MAINT_AFTER_COMMIT")
        .ok()
        .as_deref()
        != Some("1")
    {
        let _ = crate::commands::maintenance::run_auto_after_commit(&repo, args.quiet);
    }

    if let HeadState::Branch { refname, .. } = &head {
        let head_ok = read_reflog(&repo.git_dir, "HEAD")
            .ok()
            .and_then(|e| e.last().map(|l| l.new_oid == commit_oid))
            .unwrap_or(false);
        let branch_ok = read_reflog(&repo.git_dir, refname)
            .ok()
            .and_then(|e| e.last().map(|l| l.new_oid == commit_oid))
            .unwrap_or(false);
        let branch_log_wants_entry = repo
            .git_dir
            .join("logs")
            .join(refname)
            .metadata()
            .map(|_| true)
            .unwrap_or(false)
            || should_autocreate_reflog(&repo.git_dir, refname);
        if head_ok && !branch_ok && branch_log_wants_entry {
            append_reflog(
                &repo.git_dir,
                refname,
                &old_oid,
                &commit_oid,
                &commit_data.committer,
                &reflog_msg,
                true,
            )?;
        }
    }
    // A merge that was started with `--autostash --no-commit` records its WIP under
    // `MERGE_AUTOSTASH`; concluding the merge with `git commit` re-applies it
    // (git finish()/apply_autostash_ref). No-op when the ref is absent.
    let _ = crate::commands::stash::apply_autostash_ref(&repo, "MERGE_AUTOSTASH");
    cleanup_merge_state(&repo.git_dir);
    // A plain `git commit` that resolves a cherry-pick/revert conflict only removes
    // CHERRY_PICK_HEAD/REVERT_HEAD (done via cleanup_merge_state). It must NOT advance
    // and replay the remaining sequencer todo: git only continues the sequence on an
    // explicit `cherry-pick --continue` / `revert --continue` (sequencer.c). Auto-resuming
    // here would tear down the sequencer state that a later `--continue` needs.
    //
    // BUT sequencer.c:sequencer_post_commit_cleanup *does* remove the whole sequencer
    // state when the just-committed pick was the final one (`have_finished_the_last_pick`:
    // the todo has at most one line left). That lets the last pick of a sequence be
    // finished with a plain `git commit` (t3507 "successful final commit clears ... state").
    // This applies equally to a revert sequence (REVERT_HEAD), matching
    // sequencer_post_commit_cleanup which sets need_cleanup for either head.
    //
    // grit's cherry-pick and revert keep the sequencer todo in different shapes:
    // cherry-pick leaves the *current* (conflicting) item at the head of the todo
    // (git's on-disk format), so "finished the last pick" means the todo has at
    // most one line left. grit's revert instead drops the current item from the
    // todo and tracks it via REVERT_HEAD, so the todo only ever holds the *remaining*
    // un-attempted reverts; for revert the sequence is finished only when that todo
    // is empty. Using the wrong test here would prematurely tear down the sequencer
    // after committing the resolution of an intermediate revert (t7512.42).
    let finished = if resume_revert_after_rv {
        sequencer_todo_remaining_empty(&repo.git_dir)
    } else {
        sequencer_finished_last_pick(&repo.git_dir)
    };
    if (resume_pick_after_cp || resume_revert_after_rv) && finished {
        let _ = fs::remove_dir_all(repo.git_dir.join("sequencer"));
    }

    // Refresh the index file Git used for this commit (including `GIT_INDEX_FILE`). For
    // `commit --interactive`, the real index was intentionally not touched until now; promote the
    // interactively staged index here, after the commit has succeeded.
    let mut index_refresh = match interactive_staged_index.take() {
        Some(idx) => idx,
        None => match repo.load_index_at(&index_path) {
            Ok(idx) => idx,
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Index::new(),
            Err(e) => return Err(e.into()),
        },
    };
    let cache_tree = build_cache_tree_from_index(&repo.odb, &index_refresh)?;
    index_refresh.set_cache_tree(cache_tree);
    repo.write_index_at(&index_path, &mut index_refresh)?;

    // Run post-commit hook (informational, don't abort on failure)
    let _ = run_hook(&repo, "post-commit", &[], None);

    if args.amend {
        if let Some(old_oid) = old_head_oid {
            let _ = crate::commands::notes::copy_notes_for_rewrite(
                &repo,
                "amend",
                &old_oid,
                &commit_oid,
            );
        }
    }

    // Run post-rewrite hook for --amend (unless --no-post-rewrite)
    if args.amend && !args.no_post_rewrite {
        if let Some(old_oid) = old_head_oid {
            let stdin_data = format!("{} {}\n", old_oid.to_hex(), commit_oid.to_hex());
            let _ = run_hook(
                &repo,
                "post-rewrite",
                &["amend"],
                Some(stdin_data.as_bytes()),
            );
        }
    }

    // Output summary
    if !args.quiet {
        let branch = match &head {
            HeadState::Branch { short_name, .. } => short_name.as_str(),
            HeadState::Detached { .. } => "HEAD detached",
            HeadState::Invalid => "unknown",
        };
        let short_oid = &commit_oid.to_hex()[..7];
        let first_line = commit_data.message.lines().next().unwrap_or("");
        if head.is_unborn() {
            println!("[{branch} (root-commit) {short_oid}] {first_line}");
        } else {
            println!("[{branch} {short_oid}] {first_line}");
        }
        if args.author.is_some() {
            if let Ok((name, email, _)) = split_stored_author_line(&commit_data.author) {
                println!(" Author: {name} <{email}>");
            }
        }
        if args.date.is_some() {
            if let Some(display) = format_commit_summary_date(&commit_data.author) {
                println!(" Date:   {display}");
            }
        }

        // Print diff stat summary line
        let parent_tree = if commit_data.parents.is_empty() {
            None
        } else {
            let parent_obj = repo.odb.read(&commit_data.parents[0])?;
            let parent_commit = grit_lib::objects::parse_commit(&parent_obj.data)?;
            Some(parent_commit.tree)
        };
        if let Ok(diff_entries) =
            grit_lib::diff::diff_trees(&repo.odb, parent_tree.as_ref(), Some(&commit_data.tree), "")
        {
            let zero_oid = ObjectId::zero();
            let mut total_files = 0usize;
            let mut total_ins = 0usize;
            let mut total_del = 0usize;
            for entry in &diff_entries {
                total_files += 1;
                let old_content = if entry.old_oid == zero_oid {
                    String::new()
                } else {
                    repo.odb
                        .read(&entry.old_oid)
                        .map(|o| String::from_utf8_lossy(&o.data).into_owned())
                        .unwrap_or_default()
                };
                let new_content = if entry.new_oid == zero_oid {
                    String::new()
                } else {
                    repo.odb
                        .read(&entry.new_oid)
                        .map(|o| String::from_utf8_lossy(&o.data).into_owned())
                        .unwrap_or_default()
                };
                let (a, d) = grit_lib::diff::count_changes(&old_content, &new_content);
                total_ins += a;
                total_del += d;
            }
            if total_files > 0 {
                let mut summary = format!(
                    " {} file{} changed",
                    total_files,
                    if total_files == 1 { "" } else { "s" }
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
                println!("{summary}");
            }
        }
    }

    let _ = refresh_repository_shared_tree(&repo.git_dir);

    Ok(())
}

/// Print dry-run output (like `git commit --dry-run`).
fn print_dry_run(
    repo: &Repository,
    config: &ConfigSet,
    head: &HeadState,
    staged: &[DiffEntry],
    unstaged: &[DiffEntry],
    untracked: &[String],
    unmerged: &BTreeMap<String, u8>,
    in_progress: &[grit_lib::state::InProgressOperation],
    pathspec_matched: Option<&HashSet<Vec<u8>>>,
    no_ahead_behind: bool,
    amend: bool,
    index_path: &Path,
    loaded_index: &Index,
    hide_untracked: bool,
) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();

    // Per-submodule ignore decisions (annotation / suppression) for gitlink entries, matching
    // `git status` (t7508 'commit --dry-run will show a staged but ignored submodule').
    let gitlink_oid_by_path: HashMap<String, grit_lib::objects::ObjectId> = loaded_index
        .entries
        .iter()
        .filter(|e| e.stage() == 0 && e.mode == MODE_GITLINK)
        .map(|e| (String::from_utf8_lossy(&e.path).into_owned(), e.oid))
        .collect();
    let mut submodule_decisions: HashMap<String, (String, bool, bool)> = HashMap::new();
    let mut any_dirty_submodule_shown = false;
    if let Some(wt) = repo.work_tree.as_deref() {
        for (path, recorded) in &gitlink_oid_by_path {
            let d = crate::commands::status::submodule_display_decision(
                config, wt, None, path, *recorded,
            );
            if d.has_dirty_content {
                any_dirty_submodule_shown = true;
            }
            submodule_decisions.insert(
                path.clone(),
                (d.annotation, d.suppress_unstaged, d.suppress_staged),
            );
        }
    }

    let config_hints = match config.get("advice.statusHints") {
        Some(v) if v == "false" || v == "no" || v == "off" || v == "0" => false,
        _ => true,
    };
    let show_hints = std::env::var("GIT_ADVICE")
        .ok()
        .and_then(|v| match v.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        })
        .unwrap_or(config_hints);

    let orphan_line = if head.oid().is_none() {
        Some("Initial commit")
    } else {
        None
    };
    let ab_mode = if no_ahead_behind {
        AheadBehindMode::Quick
    } else {
        AheadBehindMode::Full
    };
    let header_ends_with_blank = match head {
        HeadState::Branch {
            short_name,
            oid: Some(_),
            ..
        } => !format_tracking_info(repo, short_name, ab_mode, show_hints)?.is_empty(),
        HeadState::Branch { oid: None, .. } => true,
        HeadState::Detached { .. } | HeadState::Invalid => false,
    };
    crate::commands::status::write_status_branch_header(
        &mut out,
        head,
        repo,
        "",
        show_hints,
        no_ahead_behind,
        true,
        orphan_line,
    )?;

    let merge_active = in_progress.contains(&grit_lib::state::InProgressOperation::Merge);
    if merge_active {
        if !unmerged.is_empty() {
            writeln!(out, "You have unmerged paths.")?;
            if show_hints {
                writeln!(out, "  (fix conflicts and run \"git commit\")")?;
                writeln!(out, "  (use \"git merge --abort\" to abort the merge)")?;
            }
        } else {
            writeln!(out, "All conflicts fixed but you are still merging.")?;
            if show_hints {
                writeln!(out, "  (use \"git commit\" to conclude merge)")?;
            }
        }
        writeln!(out)?;
    }
    let mut printed_body_section = merge_active;
    let mut begin_section = |out: &mut std::io::StdoutLock<'_>| -> Result<()> {
        if printed_body_section || !header_ends_with_blank {
            writeln!(out)?;
        }
        printed_body_section = true;
        Ok(())
    };

    let (staged_show, unstaged_show, extra_untracked) = if let Some(matched) = pathspec_matched {
        let mut staged_in = Vec::new();
        let mut staged_out = Vec::new();
        for e in staged {
            let p = e.path().as_bytes();
            if matched.contains(p) {
                staged_in.push(e.clone());
            } else {
                staged_out.push(e.clone());
            }
        }
        let unstaged_paths: HashSet<String> =
            unstaged.iter().map(|e| e.path().to_string()).collect();
        let mut u = unstaged.to_vec();
        let mut extra_ut = Vec::new();
        for e in staged_out {
            if unstaged_paths.contains(e.path()) {
                continue;
            }
            // Git: fully staged paths excluded from the commit are listed like untracked in
            // `--dry-run` output when the worktree matches the index (e.g. new files).
            if e.status == DiffStatus::Added {
                extra_ut.push(e.path().to_string());
            } else {
                u.push(e);
            }
        }
        (staged_in, u, extra_ut)
    } else {
        (staged.to_vec(), unstaged.to_vec(), Vec::new())
    };

    // Apply submodule ignore decisions: drop suppressed gitlinks from staged/unstaged sections.
    let staged_show: Vec<DiffEntry> = staged_show
        .into_iter()
        .filter(|e| {
            submodule_decisions
                .get(e.path())
                .map(|(_, _, suppress_staged)| !suppress_staged)
                .unwrap_or(true)
        })
        .collect();
    let unstaged_show: Vec<DiffEntry> = unstaged_show
        .into_iter()
        .filter(|e| {
            submodule_decisions
                .get(e.path())
                .map(|(_, suppress_unstaged, _)| !suppress_unstaged)
                .unwrap_or(true)
        })
        .collect();

    if !staged_show.is_empty() {
        begin_section(&mut out)?;
        writeln!(out, "Changes to be committed:")?;
        if amend {
            writeln!(
                out,
                "  (use \"git restore --source=HEAD^1 --staged <file>...\" to unstage)"
            )?;
        } else {
            writeln!(out, "  (use \"git restore --staged <file>...\" to unstage)")?;
        }
        for entry in &staged_show {
            let label = status_label_staged(entry.status);
            writeln!(out, "\t{label}:   {}", entry.path())?;
        }
    }

    if !unmerged.is_empty() {
        let include_unstage = show_hints && !merge_active;
        crate::commands::status::print_unmerged_long_section(
            &mut out,
            "",
            show_hints,
            head,
            unmerged,
            include_unstage,
        )?;
    }

    if !unstaged_show.is_empty() {
        begin_section(&mut out)?;
        writeln!(out, "Changes not staged for commit:")?;
        writeln!(
            out,
            "  (use \"git add <file>...\" to update what will be committed)"
        )?;
        writeln!(
            out,
            "  (use \"git restore <file>...\" to discard changes in working directory)"
        )?;
        if any_dirty_submodule_shown {
            writeln!(
                out,
                "  (commit or discard the untracked or modified content in submodules)"
            )?;
        }
        for entry in &unstaged_show {
            let label = status_label_unstaged(entry.status);
            let suffix = submodule_decisions
                .get(entry.path())
                .map(|(annotation, _, _)| annotation.as_str())
                .unwrap_or("");
            writeln!(out, "\t{label}:   {}{suffix}", entry.path())?;
        }
    }

    if let Some(limit) = crate::commands::status::parse_submodule_summary_limit(config) {
        let head_spec = if amend { "HEAD^" } else { "HEAD" };
        let txt = crate::commands::status::run_submodule_summary_text(
            repo,
            index_path,
            limit,
            true,
            Some(head_spec),
        )?;
        let txt = txt.trim_end_matches('\n');
        if !txt.trim().is_empty() {
            begin_section(&mut out)?;
            writeln!(out, "Submodule changes to be committed:")?;
            writeln!(out)?;
            writeln!(out, "{txt}")?;
        }
    }

    let mut all_untracked: Vec<String> = untracked.to_vec();
    all_untracked.extend(extra_untracked.iter().cloned());
    all_untracked.sort();

    if pathspec_matched.is_some() {
        let mut suppressed_roots: BTreeSet<String> = BTreeSet::new();
        if let Some(matched) = pathspec_matched {
            for ie in &loaded_index.entries {
                if ie.stage() != 0 || ie.mode == MODE_TREE || ie.mode == MODE_GITLINK {
                    continue;
                }
                let p = String::from_utf8_lossy(&ie.path).into_owned();
                if matched.contains(p.as_bytes()) {
                    continue;
                }
                let Some(parent) = Path::new(&p)
                    .parent()
                    .and_then(|x| x.to_str())
                    .map(str::to_owned)
                else {
                    continue;
                };
                if parent.is_empty() {
                    continue;
                }
                let prefix = format!("{parent}/");
                let parent_has_matched_path = matched.iter().any(|m| {
                    std::str::from_utf8(m)
                        .map(|ms| ms == parent || ms.starts_with(&prefix))
                        .unwrap_or(false)
                });
                if parent_has_matched_path {
                    continue;
                }
                suppressed_roots.insert(parent);
            }
        }
        for p in &extra_untracked {
            let Some(parent) = Path::new(p)
                .parent()
                .and_then(|x| x.to_str())
                .map(str::to_owned)
            else {
                continue;
            };
            if parent.is_empty() {
                continue;
            }
            suppressed_roots.insert(parent);
        }
        let mut collapsed: Vec<String> = Vec::new();
        let mut used_suppressed: BTreeSet<String> = BTreeSet::new();
        for p in &all_untracked {
            if p.ends_with('/') {
                collapsed.push(p.clone());
                continue;
            }
            let mut under = false;
            for root in &suppressed_roots {
                let prefix = format!("{root}/");
                if p == root || p.starts_with(&prefix) {
                    used_suppressed.insert(root.clone());
                    under = true;
                    break;
                }
            }
            if !under {
                collapsed.push(p.clone());
            }
        }
        for root in used_suppressed {
            collapsed.push(format!("{root}/"));
        }
        collapsed.sort();
        all_untracked = collapsed;
    }

    if hide_untracked {
        // `-uno`: list nothing but note it (Git `wt_longstatus_print`) when there is something
        // to commit. The note follows a blank line separating it from the previous section
        // (t7508 commit -uno --dry-run).
        let committable = !staged_show.is_empty();
        if committable {
            if printed_body_section {
                writeln!(out)?;
            }
            if show_hints {
                writeln!(
                    out,
                    "Untracked files not listed (use -u option to show untracked files)"
                )?;
            } else {
                writeln!(out, "Untracked files not listed")?;
            }
        }
    } else {
        if !all_untracked.is_empty() {
            begin_section(&mut out)?;
            writeln!(out, "Untracked files:")?;
            writeln!(
                out,
                "  (use \"git add <file>...\" to include in what will be committed)"
            )?;
            for path in &all_untracked {
                writeln!(out, "\t{path}")?;
            }
        }
        if printed_body_section && unmerged.is_empty() {
            writeln!(out)?;
        }
    }

    if !unmerged.is_empty() && staged_show.is_empty() {
        writeln!(
            out,
            "no changes added to commit (use \"git add\" and/or \"git commit -a\")"
        )?;
    }

    if staged_show.is_empty()
        && unstaged_show.is_empty()
        && all_untracked.is_empty()
        && unmerged.is_empty()
        && !merge_active
    {
        writeln!(out, "nothing to commit, working tree clean")?;
    }

    Ok(())
}

fn status_label_staged(status: DiffStatus) -> &'static str {
    match status {
        DiffStatus::Added => "new file",
        DiffStatus::Deleted => "deleted",
        DiffStatus::Modified => "modified",
        DiffStatus::Renamed => "renamed",
        DiffStatus::TypeChanged => "typechange",
        _ => "changed",
    }
}

fn status_label_unstaged(status: DiffStatus) -> &'static str {
    match status {
        DiffStatus::Deleted => "deleted",
        DiffStatus::Modified => "modified",
        DiffStatus::TypeChanged => "typechange",
        _ => "changed",
    }
}

/// Find untracked files in the working tree.
///
/// Respects `.gitignore` / exclude rules so `commit --dry-run` matches Git when test output is
/// redirected to a path listed in `.gitignore` (t7506).
fn find_untracked_files(
    repo: &Repository,
    work_tree: &Path,
    index: &Index,
    pathspecs: Option<&[String]>,
) -> Result<Vec<String>> {
    crate::commands::status::collect_untracked_normal_for_status(repo, index, work_tree, pathspecs)
}

/// `git commit --interactive` (`-i`): drive the full interactive-add menu loop (status / update /
/// revert / add untracked / patch / diff) against a *temporary* index seeded from the current
/// index, then commit the tree that index describes (Git's `interactive_add` on a temp index file).
///
/// The selections are made in a copy so the real index/work tree are untouched by the loop. After
/// the loop the temp index is written over the real index path so the normal post-commit refresh
/// records exactly the staged-and-committed state (e.g. `foo.c` ends up ` M` in `git status`).
fn run_commit_interactive_mode(repo: &Repository, work_tree: &Path, args: &Args) -> Result<Index> {
    let real_index_path = resolved_index_path(repo);

    // Seed a temporary index file from the current index so interactive selections never touch the
    // real index until the commit is being built.
    let tmp_index_path = repo
        .git_dir
        .join(format!("next-index-{}", std::process::id()));
    match fs::copy(&real_index_path, &tmp_index_path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // No index yet: start the temp index empty.
            let mut empty = Index::new();
            repo.write_index_at(&tmp_index_path, &mut empty)?;
        }
        Err(e) => return Err(e.into()),
    }

    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let core_filemode = config
        .get_bool("core.filemode")
        .and_then(|r| r.ok())
        .unwrap_or(true);
    let precompose_unicode =
        grit_lib::precompose_config::effective_core_precomposeunicode(Some(&repo.git_dir));
    let sparse_state = crate::commands::add::AddSparseState::load(repo, &config);
    let add_cfg = crate::commands::add::AddConfig {
        core_filemode,
        precompose_unicode,
        ignore_errors: false,
        conv: grit_lib::crlf::ConversionConfig::from_config(&config),
        attrs: grit_lib::crlf::load_gitattributes(work_tree),
        config: config.clone(),
        sparse: sparse_state,
        include_sparse: false,
        large_blobs: None,
    };

    // Point the interactive-add engine at the temp index for the duration of the loop.
    let prev_index_env = std::env::var_os("GIT_INDEX_FILE");
    std::env::set_var("GIT_INDEX_FILE", &tmp_index_path);

    let tmp_index = match repo.load_index_at(&tmp_index_path) {
        Ok(idx) => idx,
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Index::new(),
        Err(e) => {
            restore_index_env(prev_index_env.as_deref());
            return Err(e.into());
        }
    };

    let loop_result = crate::commands::add_interactive::run_add_i(
        repo,
        tmp_index,
        work_tree,
        &config,
        &add_cfg,
        &args.pathspec,
    );

    restore_index_env(prev_index_env.as_deref());

    if let Err(e) = loop_result {
        let _ = fs::remove_file(&tmp_index_path);
        return Err(e);
    }

    // Reload the temp index (now carrying the interactively staged hunks). The commit tree is built
    // from this index, but the *real* index is deliberately left untouched here: a later abort
    // (e.g. empty commit message) must leave the on-disk index unchanged (t7501 "commit
    // --interactive doesn't change index if editor aborts"). The post-commit index refresh persists
    // this staged index only once the commit actually succeeds.
    let staged_index = match repo.load_index_at(&tmp_index_path) {
        Ok(idx) => idx,
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Index::new(),
        Err(e) => {
            let _ = fs::remove_file(&tmp_index_path);
            return Err(e.into());
        }
    };
    let _ = fs::remove_file(&tmp_index_path);

    Ok(staged_index)
}

/// Restore (or clear) `GIT_INDEX_FILE` after a temporary override.
fn restore_index_env(prev: Option<&std::ffi::OsStr>) {
    match prev {
        Some(val) => std::env::set_var("GIT_INDEX_FILE", val),
        None => std::env::remove_var("GIT_INDEX_FILE"),
    }
}

/// Paths named by `commit -p` pathspec arguments (repository-relative), for partial-tree commits.
fn commit_patch_pathspec_targets(work_tree: &Path, pathspecs: &[String]) -> Result<Vec<String>> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| work_tree.to_path_buf());
    let prefix = crate::pathspec::pathdiff(&cwd, work_tree);
    let mut out = Vec::new();

    for spec in pathspecs {
        let resolved = crate::pathspec::resolve_pathspec(spec, work_tree, prefix.as_deref());
        if !grit_lib::pathspec::has_glob_chars(&resolved) {
            let abs_path = work_tree.join(&resolved);
            if fs::symlink_metadata(&abs_path).is_ok() {
                out.push(resolved);
            } else {
                bail!("pathspec '{spec}' did not match any file(s) known to git");
            }
            continue;
        }

        let (dir_prefix, pattern) = if let Some(slash_pos) = resolved.rfind('/') {
            (&resolved[..slash_pos], &resolved[slash_pos + 1..])
        } else {
            ("", resolved.as_str())
        };

        let search_dir = if dir_prefix.is_empty() {
            work_tree.to_path_buf()
        } else {
            work_tree.join(dir_prefix)
        };

        let mut spec_matched = false;
        if let Ok(entries) = fs::read_dir(&search_dir) {
            for entry in entries.flatten() {
                let name_str = entry.file_name().to_string_lossy().to_string();
                if name_str == ".git" {
                    continue;
                }
                if !grit_lib::wildmatch::wildmatch(pattern.as_bytes(), name_str.as_bytes(), 0) {
                    continue;
                }
                let rel = if dir_prefix.is_empty() {
                    name_str.clone()
                } else {
                    format!("{dir_prefix}/{name_str}")
                };
                let abs_path = work_tree.join(&rel);
                if fs::symlink_metadata(&abs_path).is_ok() {
                    out.push(rel);
                    spec_matched = true;
                }
            }
        }
        if pattern.contains('[') && fs::symlink_metadata(search_dir.join(pattern)).is_ok() {
            let rel = if dir_prefix.is_empty() {
                pattern.to_string()
            } else {
                format!("{dir_prefix}/{pattern}")
            };
            if !out.iter().any(|p| p == &rel) {
                out.push(rel);
                spec_matched = true;
            }
        }

        if !spec_matched {
            bail!("pathspec '{spec}' did not match any file(s) known to git");
        }
    }

    Ok(out)
}

/// Interactive `commit -p` / `commit -i`: stage selected hunks (index vs worktree), optionally without
/// writing the index (`--dry-run`).
///
/// Returns the index to use for the remainder of `commit` (in-memory when `dry_run`, otherwise
/// re-read from disk after writing).
fn run_commit_patch_mode(
    repo: &Repository,
    work_tree: &Path,
    args: &Args,
    head: &HeadState,
    parent_tree_oid: Option<&ObjectId>,
) -> Result<Index> {
    use similar::{Algorithm, TextDiff};
    use std::io::BufRead;

    let patch_cfg = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let context = crate::commands::add::resolve_patch_context(args.unified, &patch_cfg)?;
    let inter_hunk_context =
        crate::commands::add::resolve_patch_interhunk(args.inter_hunk_context, &patch_cfg)?;

    let index_path = resolved_index_path(repo);
    let disk_index = match repo.load_index_at(&index_path) {
        Ok(idx) => idx,
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Index::new(),
        Err(e) => return Err(e.into()),
    };

    if disk_index.entries.iter().any(|e| e.stage() != 0) {
        eprintln!("error: Committing is not possible because you have unmerged files.");
        eprintln!("hint: Fix them up in the work tree, and then use 'git add/rm <file>'");
        eprintln!("hint: as appropriate to mark resolution and make a commit.");
        eprintln!("fatal: Exiting because of an unresolved conflict.");
        std::process::exit(128);
    }

    let filter_paths: Vec<String> = if args.pathspec.is_empty() {
        Vec::new()
    } else {
        commit_patch_pathspec_targets(work_tree, &args.pathspec)?
    };

    let mut candidate_paths: Vec<String> = Vec::new();
    for ie in &disk_index.entries {
        if ie.stage() != 0 {
            continue;
        }
        if ie.mode == MODE_SYMLINK || ie.mode == grit_lib::index::MODE_GITLINK {
            continue;
        }
        let path_str = String::from_utf8_lossy(&ie.path).to_string();
        if !crate::commands::checkout::patch_path_filter_matches(&path_str, &filter_paths) {
            continue;
        }
        let abs_path = work_tree.join(&path_str);
        let work_content = if fs::symlink_metadata(&abs_path).is_ok() {
            fs::read(&abs_path).with_context(|| format!("reading {path_str}"))?
        } else {
            Vec::new()
        };
        let obj = repo.odb.read(&ie.oid)?;
        if obj.kind != ObjectKind::Blob {
            continue;
        }
        if work_content == obj.data {
            continue;
        }
        candidate_paths.push(path_str);
    }

    candidate_paths.sort();
    candidate_paths.dedup();

    if candidate_paths.is_empty() {
        // No worktree changes are available for interactive selection. Git's
        // `commit -p` still prints "No changes." for the interactive phase, but
        // if the index already carries staged changes versus the parent tree
        // (e.g. a `git rm` deletion staged before `commit -p`), it commits those
        // staged changes as-is rather than aborting.
        println!("No changes.");
        let config = ConfigSet::load(Some(&repo.git_dir), true)?;
        let staged = diff_index_to_tree(&repo.odb, &disk_index, parent_tree_oid, false)?;
        if !staged.is_empty() {
            return Ok(disk_index);
        }
        let unstaged_raw = diff_index_to_worktree(&repo.odb, &disk_index, work_tree, false, false)?;
        let (rename_threshold, rename_copies) = commit_rename_settings(&config);
        let unstaged = if let Some(th) = rename_threshold {
            status_apply_rename_copy_detection(
                &repo.odb,
                unstaged_raw,
                th,
                rename_copies,
                parent_tree_oid,
            )?
        } else {
            unstaged_raw
        };
        let untracked = find_untracked_files(repo, work_tree, &disk_index, None)?;
        let unmerged_full = crate::commands::status::unmerged_paths_and_mask(&disk_index);
        let in_progress = detect_in_progress(&repo.git_dir);
        print_dry_run(
            repo,
            &config,
            head,
            &staged,
            &unstaged,
            &untracked,
            &unmerged_full,
            &in_progress,
            None,
            false,
            args.amend,
            &index_path,
            &disk_index,
            false,
        )?;
        std::process::exit(1);
    }

    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut out = io::stdout();

    let mut new_index = disk_index.clone();
    let mut path_to_new_blob: HashMap<String, Vec<u8>> = HashMap::new();
    let mut any_hunk_staged = false;
    let mut stdin_eof_after_edit = false;

    for path in candidate_paths {
        let path_bytes = path.as_bytes();
        let Some(ie) = new_index.get(path_bytes, 0).cloned() else {
            continue;
        };
        if ie.mode == MODE_SYMLINK || ie.mode == grit_lib::index::MODE_GITLINK {
            continue;
        }
        let index_obj = repo.odb.read(&ie.oid)?;
        if index_obj.kind != ObjectKind::Blob {
            continue;
        }
        let index_content = index_obj.data;
        let abs_path = work_tree.join(&path);
        let mut cur_work = if fs::symlink_metadata(&abs_path).is_ok() {
            fs::read(&abs_path).with_context(|| format!("reading {path}"))?
        } else {
            Vec::new()
        };

        'file_pass: loop {
            let index_str = String::from_utf8_lossy(&index_content);
            let work_str = String::from_utf8_lossy(&cur_work);
            let text_diff = TextDiff::configure()
                .algorithm(Algorithm::Myers)
                .diff_lines(index_str.as_ref(), work_str.as_ref());
            let ops: Vec<_> = text_diff.ops().to_vec();

            let has_change = ops
                .iter()
                .any(|o| !matches!(o, similar::DiffOp::Equal { .. }));
            if !has_change {
                path_to_new_blob.insert(path.clone(), index_content.clone());
                break 'file_pass;
            }

            let n_ops = ops.len();
            let mut hunk_ranges: Vec<(usize, usize)> = vec![(0, n_ops)];
            let mut accepted = vec![false; hunk_ranges.len()];
            let mut hunk_cursor = 0usize;

            'hunk_loop: loop {
                let n_hunks = hunk_ranges.len();
                if hunk_cursor >= n_hunks {
                    break;
                }

                let display_idx = hunk_cursor + 1;
                let (s, e) = hunk_ranges[hunk_cursor];
                let hunk_only = crate::commands::stash::partial_unified_for_op_range_interhunk(
                    path.as_str(),
                    &index_content,
                    &cur_work,
                    &ops[s..e],
                    context,
                    inter_hunk_context,
                    true,
                );

                writeln!(out, "diff --git a/{path} b/{path}").ok();
                write!(out, "--- a/{path}\n+++ b/{path}\n").ok();
                write!(out, "{hunk_only}").ok();
                write!(
                    out,
                    "({display_idx}/{n_hunks}) Stage this hunk [y,n,q,a,d,s,e,?]? "
                )
                .ok();
                out.flush().ok();

                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    if stdin_eof_after_edit {
                        // Match Git: after `e`, EOF on the next prompt stages the hunk (t7514).
                        line.push('y');
                        stdin_eof_after_edit = false;
                    } else {
                        std::process::exit(1);
                    }
                }
                let answer = line.trim();
                match answer {
                    "y" | "Y" => {
                        accepted[hunk_cursor] = true;
                        any_hunk_staged = true;
                        hunk_cursor += 1;
                    }
                    "n" | "N" => {
                        hunk_cursor += 1;
                    }
                    "a" | "A" => {
                        any_hunk_staged = true;
                        for j in hunk_cursor..n_hunks {
                            accepted[j] = true;
                        }
                        break 'hunk_loop;
                    }
                    "d" | "D" => {
                        break 'hunk_loop;
                    }
                    "q" | "Q" => {
                        std::process::exit(1);
                    }
                    "s" | "S" => {
                        if !crate::commands::stash::split_hunk_at_first_gap(
                            &mut hunk_ranges,
                            hunk_cursor,
                            &ops,
                        ) {
                            continue 'hunk_loop;
                        }
                        let n = hunk_ranges.len();
                        accepted.resize(n, false);
                        if hunk_cursor >= n {
                            hunk_cursor = n.saturating_sub(1);
                        }
                    }
                    "e" | "E" => {
                        if let Ok(edited) = crate::commands::stash::edit_bytes_tempfile(&cur_work) {
                            cur_work = edited;
                            stdin_eof_after_edit = true;
                            continue 'file_pass;
                        }
                    }
                    "?" => {
                        writeln!(
                            out,
                            "y - stage this hunk\n\
                             n - do not stage this hunk\n\
                             q - quit; do not stage this hunk or any of the remaining ones\n\
                             a - stage this hunk and all later hunks in the file\n\
                             d - do not stage this hunk or any of the later hunks in the file\n\
                             s - split the current hunk into smaller hunks\n\
                             e - manually edit the current hunk\n"
                        )
                        .ok();
                        out.flush().ok();
                    }
                    _ => {}
                }
            }

            // Git prints a trailing newline when leaving the file's interactive hunk loop
            // (t3701 "commit falls back to color.ui" compares raw vs color-decoded output).
            writeln!(out).ok();

            // `blend_line_diff_by_hunk_ranges` uses the first arg as "source" when a range is
            // accepted. For `commit -p` the diff is **index → worktree**; answering `y` must stage
            // the worktree side, so invert flags (same relationship as stash's `stash_accepted`).
            let stage_accepted: Vec<bool> = accepted.iter().map(|a| !*a).collect();
            let staged_bytes = crate::commands::checkout::blend_line_diff_by_hunk_ranges(
                &index_content,
                &cur_work,
                &hunk_ranges,
                &stage_accepted,
            );
            path_to_new_blob.insert(path, staged_bytes.into_bytes());
            break 'file_pass;
        }
    }

    if !any_hunk_staged {
        let config = ConfigSet::load(Some(&repo.git_dir), true)?;
        let staged = diff_index_to_tree(&repo.odb, &disk_index, parent_tree_oid, false)?;
        let unstaged_raw = diff_index_to_worktree(&repo.odb, &disk_index, work_tree, false, false)?;
        let (rename_threshold, rename_copies) = commit_rename_settings(&config);
        let unstaged = if let Some(th) = rename_threshold {
            status_apply_rename_copy_detection(
                &repo.odb,
                unstaged_raw,
                th,
                rename_copies,
                parent_tree_oid,
            )?
        } else {
            unstaged_raw
        };
        let untracked = find_untracked_files(repo, work_tree, &disk_index, None)?;
        let unmerged_full = crate::commands::status::unmerged_paths_and_mask(&disk_index);
        let in_progress = detect_in_progress(&repo.git_dir);
        print_dry_run(
            repo,
            &config,
            head,
            &staged,
            &unstaged,
            &untracked,
            &unmerged_full,
            &in_progress,
            None,
            false,
            args.amend,
            &index_path,
            &disk_index,
            false,
        )?;
        std::process::exit(1);
    }

    for (path, bytes) in &path_to_new_blob {
        let path_b = path.as_bytes();
        let Some(entry) = new_index.get_mut(path_b, 0) else {
            continue;
        };
        if bytes.is_empty() {
            entry.oid = repo.odb.write(ObjectKind::Blob, &[])?;
            entry.size = 0;
        } else {
            entry.oid = repo.odb.write(ObjectKind::Blob, bytes)?;
            entry.size = bytes.len() as u32;
        }
    }

    if args.dry_run {
        return Ok(new_index);
    }

    repo.write_index(&mut new_index)?;
    repo.load_index_at(&index_path).map_err(|e| e.into())
}

/// Apply pathspec staging to `index` in memory (no disk write).
///
/// Returns the set of repository-relative paths that were staged (or removed) for this commit.
fn apply_pathspec_to_index(
    repo: &Repository,
    work_tree: &Path,
    index: &mut Index,
    pathspecs: &[String],
) -> Result<HashSet<Vec<u8>>> {
    use std::os::unix::fs::MetadataExt;

    let cwd = std::env::current_dir().unwrap_or_else(|_| work_tree.to_path_buf());
    let prefix = crate::pathspec::pathdiff(&cwd, work_tree);

    let mut matched_paths = HashSet::new();

    let is_known_to_index = |idx: &Index, path: &[u8]| -> bool {
        idx.get(path, 0).is_some()
            || idx.entries.iter().any(|entry| {
                entry.stage() == 0
                    && entry.path.starts_with(path)
                    && entry.path.get(path.len()) == Some(&b'/')
            })
    };

    let reject_skip_worktree = |idx: &Index, path: &[u8]| -> Result<()> {
        if idx.get(path, 0).is_some_and(|e| e.skip_worktree()) {
            bail!("cannot update skip-worktree entry");
        }
        Ok(())
    };

    for spec in pathspecs {
        let resolved = crate::pathspec::resolve_pathspec(spec, work_tree, prefix.as_deref());
        if !grit_lib::pathspec::has_glob_chars(&resolved) {
            reject_skip_worktree(&index, resolved.as_bytes())?;
            if !is_known_to_index(index, resolved.as_bytes()) {
                bail!("pathspec '{spec}' did not match any file(s) known to git");
            }
            let abs_path = work_tree.join(&resolved);
            if let Ok(meta) = fs::symlink_metadata(&abs_path) {
                let data = if meta.file_type().is_symlink() {
                    let target = fs::read_link(&abs_path)?;
                    target.to_string_lossy().into_owned().into_bytes()
                } else {
                    fs::read(&abs_path)?
                };
                let oid = repo.odb.write(ObjectKind::Blob, &data)?;
                let mode = grit_lib::index::normalize_mode(meta.mode());
                let raw_path = resolved.as_bytes().to_vec();
                let entry = grit_lib::index::entry_from_stat(&abs_path, &raw_path, oid, mode)?;
                index.add_or_replace(entry);
                matched_paths.insert(raw_path);
            } else {
                index.remove(resolved.as_bytes());
                matched_paths.insert(resolved.as_bytes().to_vec());
            }
            continue;
        }

        let (dir_prefix, pattern) = if let Some(slash_pos) = resolved.rfind('/') {
            (&resolved[..slash_pos], &resolved[slash_pos + 1..])
        } else {
            ("", resolved.as_str())
        };

        let search_dir = if dir_prefix.is_empty() {
            work_tree.to_path_buf()
        } else {
            work_tree.join(dir_prefix)
        };

        let mut spec_matched = false;
        let mut matched_rels: Vec<String> = Vec::new();
        if let Ok(entries) = fs::read_dir(&search_dir) {
            for entry in entries.flatten() {
                let name_str = entry.file_name().to_string_lossy().to_string();
                if name_str == ".git" {
                    continue;
                }
                if !grit_lib::wildmatch::wildmatch(pattern.as_bytes(), name_str.as_bytes(), 0) {
                    continue;
                }
                let rel = if dir_prefix.is_empty() {
                    name_str.clone()
                } else {
                    format!("{dir_prefix}/{name_str}")
                };
                matched_rels.push(rel);
            }
        }
        if pattern.contains('[') && fs::symlink_metadata(search_dir.join(pattern)).is_ok() {
            let rel = if dir_prefix.is_empty() {
                pattern.to_string()
            } else {
                format!("{dir_prefix}/{pattern}")
            };
            if !matched_rels.contains(&rel) {
                matched_rels.push(rel);
            }
        }

        for rel in matched_rels {
            if index.get(rel.as_bytes(), 0).is_none() {
                continue;
            }
            reject_skip_worktree(&index, rel.as_bytes())?;
            let abs_path = work_tree.join(&rel);
            if let Ok(meta) = fs::symlink_metadata(&abs_path) {
                let data = if meta.file_type().is_symlink() {
                    let target = fs::read_link(&abs_path)?;
                    target.to_string_lossy().into_owned().into_bytes()
                } else {
                    fs::read(&abs_path)?
                };
                let oid = repo.odb.write(ObjectKind::Blob, &data)?;
                let mode = grit_lib::index::normalize_mode(meta.mode());
                let raw_path = rel.as_bytes().to_vec();
                let entry = grit_lib::index::entry_from_stat(&abs_path, &raw_path, oid, mode)?;
                index.add_or_replace(entry);
                spec_matched = true;
                matched_paths.insert(raw_path);
            }
        }

        if !spec_matched {
            bail!("pathspec '{spec}' did not match any file(s) known to git");
        }
    }

    Ok(matched_paths)
}

/// Auto-stage tracked files (for `commit -a`).
fn auto_stage_tracked(repo: &Repository, work_tree: &Path) -> Result<()> {
    let index_path = resolved_index_path(repo);
    let mut index = match repo.load_index_at(&index_path) {
        Ok(idx) => idx,
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    // The index file's own mtime bounds racy-timestamp detection (Git `is_racy_timestamp`):
    // an entry written in the same instant the index was saved cannot be trusted by stat alone.
    let index_mtime = {
        use std::os::unix::fs::MetadataExt;
        fs::symlink_metadata(&index_path)
            .ok()
            .map(|m| (m.mtime() as u32, m.mtime_nsec() as u32))
    };

    // One pass over the (sorted) index instead of a linear scan per path: which paths have
    // conflict stages, and a snapshot of each stage-0 entry for stat/oid comparisons below.
    // Snapshots stay valid because the loop only mutates entries for the path it is visiting.
    let mut path_keys: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
    let mut unmerged_paths: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
    let mut stage0: std::collections::HashMap<Vec<u8>, grit_lib::index::IndexEntry> =
        std::collections::HashMap::new();
    for e in &index.entries {
        path_keys.insert(e.path.clone());
        if e.stage() != 0 {
            unmerged_paths.insert(e.path.clone());
        } else {
            stage0.insert(e.path.clone(), e.clone());
        }
    }

    // Memoize per-directory symlink-parent answers so N files in one directory cost one
    // ancestor walk, not N.
    let mut symlink_parent_cache: std::collections::HashMap<PathBuf, bool> =
        std::collections::HashMap::new();

    let mut changed = false;
    for raw_path in path_keys {
        let path_str = String::from_utf8_lossy(&raw_path).to_string();
        let abs_path = work_tree.join(&path_str);
        if path_has_symlink_parent_cached(work_tree, &abs_path, &mut symlink_parent_cache) {
            index.remove(&raw_path);
            changed = true;
            continue;
        }

        if unmerged_paths.contains(&raw_path) {
            // Merge conflicts list multiple index rows per path. Refresh once from the
            // worktree and collapse to a single stage-0 entry (matches `git commit -a`).
            let idx_mode = index
                .entries
                .iter()
                .find(|e| e.path == raw_path && e.stage() == 0)
                .map(|e| e.mode)
                .or_else(|| {
                    index
                        .entries
                        .iter()
                        .find(|e| e.path == raw_path)
                        .map(|e| e.mode)
                })
                .unwrap_or(0o100644);
            index.remove(&raw_path);
            if fs::symlink_metadata(&abs_path).is_ok() {
                if idx_mode == 0o160000 {
                    if let Some(oid) = grit_lib::diff::read_submodule_head_oid(&abs_path) {
                        use std::os::unix::fs::MetadataExt;
                        let meta = fs::symlink_metadata(&abs_path)?;
                        let entry = grit_lib::index::IndexEntry {
                            ctime_sec: meta.ctime() as u32,
                            ctime_nsec: meta.ctime_nsec() as u32,
                            mtime_sec: meta.mtime() as u32,
                            mtime_nsec: meta.mtime_nsec() as u32,
                            dev: meta.dev() as u32,
                            ino: meta.ino() as u32,
                            mode: 0o160000,
                            uid: meta.uid(),
                            gid: meta.gid(),
                            size: 0,
                            oid,
                            flags: path_str.len().min(0xFFF) as u16,
                            flags_extended: None,
                            path: raw_path.clone(),
                            base_index_pos: 0,
                        };
                        index.add_or_replace(entry);
                        changed = true;
                    }
                } else {
                    use std::os::unix::fs::MetadataExt;
                    let meta = fs::symlink_metadata(&abs_path)?;
                    let data = if meta.file_type().is_symlink() {
                        let target = fs::read_link(&abs_path)?;
                        target.to_string_lossy().into_owned().into_bytes()
                    } else {
                        fs::read(&abs_path)?
                    };
                    let oid = repo.odb.write(ObjectKind::Blob, &data)?;
                    let mode = grit_lib::index::normalize_mode(meta.mode());
                    let entry = grit_lib::index::entry_from_stat(&abs_path, &raw_path, oid, mode)?;
                    index.add_or_replace(entry);
                    changed = true;
                }
            } else {
                changed = true;
            }
            continue;
        }

        let Some(idx_e) = stage0.get(&raw_path) else {
            continue;
        };
        let idx_mode = idx_e.mode;
        let idx_skip_worktree = idx_e.skip_worktree();
        let idx_intent_to_add = idx_e.intent_to_add();
        let idx_oid = idx_e.oid;

        // Use `symlink_metadata`, not `exists()`: `Path::exists` follows symlinks, so
        // dangling symlinks look "missing" and would be dropped from the index (t1006).
        if fs::symlink_metadata(&abs_path).is_ok() {
            // Gitlink (submodule) entries: read the embedded repo's HEAD to
            // get the current commit OID instead of trying to read the
            // directory as a file.
            if idx_mode == 0o160000 {
                // `.git` may be a gitfile (submodule layout); resolve via the same helper as `git add`.
                if let Some(oid) = grit_lib::diff::read_submodule_head_oid(&abs_path) {
                    if index
                        .entries
                        .iter()
                        .find(|e| e.path == *raw_path)
                        .is_some_and(|e| e.oid == oid && e.mode == 0o160000)
                    {
                        continue;
                    }
                    use std::os::unix::fs::MetadataExt;
                    let meta = fs::symlink_metadata(&abs_path)?;
                    let entry = grit_lib::index::IndexEntry {
                        ctime_sec: meta.ctime() as u32,
                        ctime_nsec: meta.ctime_nsec() as u32,
                        mtime_sec: meta.mtime() as u32,
                        mtime_nsec: meta.mtime_nsec() as u32,
                        dev: meta.dev() as u32,
                        ino: meta.ino() as u32,
                        mode: 0o160000,
                        uid: meta.uid(),
                        gid: meta.gid(),
                        size: 0,
                        oid,
                        flags: path_str.len().min(0xFFF) as u16,
                        flags_extended: None,
                        path: raw_path.clone(),
                        base_index_pos: 0,
                    };
                    index.stage_file(entry);
                    changed = true;
                }
                continue;
            }
            use std::os::unix::fs::MetadataExt;
            let meta = fs::symlink_metadata(&abs_path)?;
            if meta.is_dir() && !meta.file_type().is_symlink() {
                if abs_path.join(".git").exists() {
                    if let Some(oid) = grit_lib::diff::read_submodule_head_oid(&abs_path) {
                        let entry = grit_lib::index::IndexEntry {
                            ctime_sec: meta.ctime() as u32,
                            ctime_nsec: meta.ctime_nsec() as u32,
                            mtime_sec: meta.mtime() as u32,
                            mtime_nsec: meta.mtime_nsec() as u32,
                            dev: meta.dev() as u32,
                            ino: meta.ino() as u32,
                            mode: 0o160000,
                            uid: meta.uid(),
                            gid: meta.gid(),
                            size: 0,
                            oid,
                            flags: path_str.len().min(0xFFF) as u16,
                            flags_extended: None,
                            path: raw_path.clone(),
                            base_index_pos: 0,
                        };
                        index.stage_file(entry);
                        changed = true;
                    }
                } else {
                    index.remove(&raw_path);
                    changed = true;
                }
                continue;
            }
            // Fast path (Git `ie_match_stat`): an unchanged stat tuple with an unchanged mode
            // proves the blob is unchanged — unless the entry is racy — so skip both the
            // re-hash and the object-database write for the common "file untouched" case.
            let wt_mode = if meta.file_type().is_symlink() {
                grit_lib::index::MODE_SYMLINK
            } else {
                grit_lib::index::normalize_mode(meta.mode())
            };
            if !idx_intent_to_add
                && wt_mode == idx_mode
                && grit_lib::diff::stat_matches(idx_e, &meta)
                && !grit_lib::diff::entry_is_racy(idx_e, index_mtime)
            {
                continue;
            }
            let data = if meta.file_type().is_symlink() {
                let target = fs::read_link(&abs_path)?;
                target.to_string_lossy().into_owned().into_bytes()
            } else {
                fs::read(&abs_path)?
            };
            let oid = repo.odb.write(ObjectKind::Blob, &data)?;
            // Unmerged paths were handled above, so the only index row for this path is the
            // stage-0 snapshot taken before the loop.
            if !idx_intent_to_add && idx_oid == oid {
                continue;
            }
            let mode = grit_lib::index::normalize_mode(meta.mode());
            let entry = grit_lib::index::entry_from_stat(&abs_path, &raw_path, oid, mode)?;
            // Use `stage_file` so conflict stages (1/2/3) are cleared when `commit -a`
            // re-stages the resolved worktree file (t4038 merge conflict resolution).
            index.stage_file(entry);
            changed = true;
        } else if idx_skip_worktree {
            continue;
        } else {
            index.remove(&raw_path);
            changed = true;
        }
    }

    if changed {
        repo.write_index_at(&index_path, &mut index)?;
    }

    Ok(())
}

/// Whether any parent directory of `abs_path` (below `work_tree`) is a symlink, memoizing
/// per-directory answers in `cache` so staging many files in one directory costs a single
/// ancestor walk instead of one lstat per component per file.
fn path_has_symlink_parent_cached(
    work_tree: &Path,
    abs_path: &Path,
    cache: &mut std::collections::HashMap<PathBuf, bool>,
) -> bool {
    let Some(parent) = abs_path.parent() else {
        return false;
    };
    dir_or_ancestor_is_symlink(work_tree, parent, cache)
}

/// Whether `dir` itself or any of its ancestors below `work_tree` is a symlink (memoized).
fn dir_or_ancestor_is_symlink(
    work_tree: &Path,
    dir: &Path,
    cache: &mut std::collections::HashMap<PathBuf, bool>,
) -> bool {
    if dir == work_tree || !dir.starts_with(work_tree) {
        return false;
    }
    if let Some(&answer) = cache.get(dir) {
        return answer;
    }
    let answer = match dir.parent() {
        Some(parent) if dir_or_ancestor_is_symlink(work_tree, parent, cache) => true,
        _ => fs::symlink_metadata(dir)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false),
    };
    cache.insert(dir.to_path_buf(), answer);
    answer
}

/// Result of building a commit message — may be UTF-8 or raw bytes.
struct MessageResult {
    /// UTF-8 message (always set; lossy if raw_bytes is Some).
    message: String,
    /// Raw bytes when the message is not valid UTF-8.
    raw_bytes: Option<Vec<u8>>,
    /// Message came from `.git/MERGE_MSG` (cherry-pick / revert conflict template).
    from_merge_msg: bool,
}

fn resolved_index_path(repo: &Repository) -> PathBuf {
    if let Ok(raw) = std::env::var("GIT_INDEX_FILE") {
        let p = PathBuf::from(raw);
        if p.is_absolute() {
            p
        } else if let Ok(cwd) = std::env::current_dir() {
            cwd.join(p)
        } else {
            p
        }
    } else {
        repo.index_path()
    }
}

/// Whether a stopped commit corresponds to an interactive-rebase `pick` (Git `FROM_REBASE_PICK`).
///
/// True when an interactive rebase state dir is present and `REBASE_HEAD` equals
/// `CHERRY_PICK_HEAD` (the in-progress pick that halted). Used to choose rebase-specific
/// "cannot do a partial commit" / "cannot amend" error messages.
///
/// # Parameters
/// - `git_dir`: the repository git directory.
fn commit_is_rebase_pick_whence(git_dir: &Path) -> bool {
    let rebase_dir_exists =
        git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists();
    if !rebase_dir_exists {
        return false;
    }
    let read = |name: &str| -> Option<String> {
        fs::read_to_string(git_dir.join(name))
            .ok()
            .map(|s| s.trim().to_owned())
    };
    match (read("REBASE_HEAD"), read("CHERRY_PICK_HEAD")) {
        (Some(rh), Some(ch)) => !rh.is_empty() && rh == ch,
        _ => false,
    }
}

fn parse_fixup_argument(raw: &str) -> Result<FixupParsed> {
    let (prefix, rest) = match raw.split_once(':') {
        Some((a, b)) if !a.is_empty() && a.chars().all(|c| c.is_ascii_alphabetic()) => (a, b),
        _ => {
            return Ok(FixupParsed {
                mode: FixupMode::Fixup,
                commit_ref: raw.to_string(),
            });
        }
    };
    match prefix {
        "amend" => Ok(FixupParsed {
            mode: FixupMode::AmendStyle { is_reword: false },
            commit_ref: rest.to_string(),
        }),
        "reword" => Ok(FixupParsed {
            mode: FixupMode::AmendStyle { is_reword: true },
            commit_ref: rest.to_string(),
        }),
        _ => bail!("unknown option: --fixup={prefix}:{rest}"),
    }
}

fn commit_rename_settings(config: &ConfigSet) -> (Option<u32>, bool) {
    for key in ["status.renames", "diff.renames"] {
        if let Some(val) = config.get(key) {
            let lowered = val.trim().to_ascii_lowercase();
            return match lowered.as_str() {
                "false" | "no" | "off" | "0" => (None, false),
                "true" | "yes" | "on" | "1" | "" => (Some(50), false),
                "copies" | "copy" => (Some(50), true),
                _ => (None, false),
            };
        }
    }
    (Some(50), false)
}

fn commit_uses_editor(args: &Args, fixup: Option<&FixupParsed>) -> bool {
    // Mirror `builtin/commit.c`: first derive a default `use_editor` from the message source,
    // then let an explicit `-e`/`--no-edit` override it (upstream `edit_flag` tri-state).
    let base = commit_uses_editor_default(args, fixup);
    if args.edit {
        return true;
    }
    if args.no_edit {
        return false;
    }
    base
}

/// Default `use_editor` before applying an explicit `-e`/`--no-edit` override.
fn commit_uses_editor_default(args: &Args, fixup: Option<&FixupParsed>) -> bool {
    // `-c`/`-C` (reuse), `-m`, and `-F` all disable the editor by default.
    if args.reuse_message.is_some() && args.reedit_message.is_none() {
        return false;
    }
    if !args.message.is_empty() || args.file.is_some() {
        return false;
    }
    // Note: the presence of MERGE_MSG / SQUASH_MSG does NOT disable the editor. Git
    // (`builtin/commit.c`) only clears `use_editor` for `-m`/`-F`/`-c`/`-C`; a plain
    // `git commit` after a merge (or a manual commit while resolving a rebase conflict)
    // still opens the editor, seeded with MERGE_MSG. Tests pass `EDITOR=:` to make that a
    // no-op. Skipping the editor here would mislabel such commits to prepare-commit-msg
    // hooks as non-interactive (t7505 `merge [pick rebase-b]`).
    if let Some(f) = fixup {
        match f.mode {
            // Plain `--fixup` uses a generated message (no editor) by default.
            FixupMode::Fixup => return false,
            FixupMode::AmendStyle { .. } => return true,
        }
    }
    true
}

fn parse_optional_path_spec(spec: &str) -> (bool, &str) {
    const OPT: &str = ":(optional)";
    if let Some(rest) = spec.strip_prefix(OPT) {
        (true, rest)
    } else {
        (false, spec)
    }
}

fn resolve_commit_template_path(args: &Args, config: &ConfigSet) -> Result<Option<PathBuf>> {
    let cli = args.template.as_deref();
    let cfg_owned = config.get("commit.template");
    let cfg = cfg_owned.as_deref();
    let chosen = cli.or(cfg);
    let Some(raw) = chosen else {
        return Ok(None);
    };
    let (optional, path_str) = parse_optional_path_spec(raw.trim());
    let path = Path::new(path_str);
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(path)
    } else {
        path.to_path_buf()
    };
    if abs.is_file() {
        return Ok(Some(abs));
    }
    if optional {
        return Ok(None);
    }
    bail!("fatal: could not read '{}'", abs.display());
}

fn first_line(message: &str) -> &str {
    message.lines().next().unwrap_or("").trim_end()
}

fn format_fixup_subject(repo: &Repository, prefix: &str, commit_ref: &str) -> Result<String> {
    let oid = resolve_revision_as_commit(repo, commit_ref)?;
    let obj = repo.odb.read(&oid)?;
    let commit = grit_lib::objects::parse_commit(&obj.data)?;
    let subj = first_line(&commit.message);
    Ok(format!("{prefix}! {subj}\n\n"))
}

fn message_body_after_subject(full: &str) -> &str {
    if let Some(pos) = full.find("\n\n") {
        &full[pos + 2..]
    } else {
        ""
    }
}

fn skip_blank_lines(mut s: &str) -> &str {
    while let Some(rest) = s.strip_prefix('\n') {
        s = rest;
    }
    s
}

fn commit_body_for_amend_fixup(repo: &Repository, target_oid: &ObjectId) -> Result<String> {
    let obj = repo.odb.read(target_oid)?;
    let commit = grit_lib::objects::parse_commit(&obj.data)?;
    let subj = first_line(&commit.message);
    // Match `prepare_amend_commit` in Git: if the target subject already begins with
    // `amend!`, format with `%b` only (drop the duplicated subject line from the body).
    let body = if subj.trim_start().starts_with("amend!") {
        message_body_after_subject(&commit.message)
    } else {
        commit.message.as_str()
    };
    Ok(skip_blank_lines(body).to_string())
}

fn message_after_first_line(message: &str) -> &str {
    message.find('\n').map(|i| &message[i + 1..]).unwrap_or("")
}

/// Git inserts a blank line between the autosquash subject and editor-appended body when the
/// template starts with `subject\n\n` (even if cleanup removed the second newline visually).
fn normalize_autosquash_editor_message(
    args: &Args,
    fixup: Option<&FixupParsed>,
    used_editor: bool,
    message: &str,
) -> String {
    if !used_editor
        || args.file.is_some()
        || args.reuse_message.is_some()
        || args.reedit_message.is_some()
    {
        return message.to_string();
    }
    if args.squash.is_none() {
        return message.to_string();
    }
    if fixup.is_some() {
        return message.to_string();
    }
    let Some(first_nl) = message.find('\n') else {
        return message.to_string();
    };
    let first_line = &message[..first_nl];
    let rest = &message[first_nl + 1..];
    let rest_trim = rest.trim_start_matches(['\n', '\r']);
    if rest_trim.is_empty() {
        return message.to_string();
    }
    if rest.starts_with("\n\n") || rest.starts_with("\r\n\r\n") {
        return message.to_string();
    }
    format!("{first_line}\n\n{rest_trim}")
}

fn build_squash_prefix(
    repo: &Repository,
    squash_ref: &str,
    reuse_rev: Option<&str>,
) -> Result<String> {
    if reuse_rev == Some(squash_ref) {
        return Ok("squash! ".to_string());
    }
    format_fixup_subject(repo, "squash", squash_ref)
}

fn read_message_file_raw(file_path: &str) -> Result<Vec<u8>> {
    if file_path == "-" {
        use std::io::Read;
        let mut buf = Vec::new();
        std::io::stdin().read_to_end(&mut buf)?;
        Ok(buf)
    } else {
        fs::read(file_path).with_context(|| format!("could not read log file '{file_path}'"))
    }
}

fn raw_to_message_result(raw: Vec<u8>) -> Result<MessageResult> {
    // Git stores the `-F` file content verbatim and never transcodes it; the
    // raw bytes flow through to the commit object unchanged. We always retain
    // the raw bytes (not only when they are non-UTF-8) so that messages already
    // in a non-UTF-8 `i18n.commitEncoding` (or 7-bit encodings like ISO-2022-JP
    // that happen to be valid UTF-8) are preserved exactly. The lossy string is
    // kept only for cleanup/empty-detection bookkeeping.
    let lossy = String::from_utf8_lossy(&raw).to_string();
    let mut raw_nl = raw;
    if !raw_nl.is_empty() && !raw_nl.ends_with(b"\n") {
        raw_nl.push(b'\n');
    }
    Ok(MessageResult {
        message: ensure_trailing_newline(&lossy),
        raw_bytes: Some(raw_nl),
        from_merge_msg: false,
    })
}

fn build_initial_commit_buffer(
    args: &Args,
    repo: &Repository,
    fixup: Option<&FixupParsed>,
    template_path: Option<&Path>,
) -> Result<String> {
    let mut buf = String::new();

    if fixup.is_none() && !args.message.is_empty() {
        buf.push_str(&args.message.join("\n\n"));
        if !buf.ends_with('\n') {
            buf.push('\n');
        }
        return Ok(buf);
    }

    if let Some(fp) = fixup {
        match &fp.mode {
            FixupMode::Fixup => {
                buf.push_str(&format_fixup_subject(repo, "fixup", &fp.commit_ref)?);
                if !args.message.is_empty() {
                    buf.push_str(&args.message.join("\n\n"));
                }
                if !buf.ends_with('\n') {
                    buf.push('\n');
                }
                return Ok(buf);
            }
            FixupMode::AmendStyle { .. } => {
                buf.push_str(&format_fixup_subject(repo, "amend", &fp.commit_ref)?);
                let oid = resolve_revision_as_commit(repo, &fp.commit_ref)?;
                buf.push_str(&commit_body_for_amend_fixup(repo, &oid)?);
                if !buf.ends_with('\n') {
                    buf.push('\n');
                }
                return Ok(buf);
            }
        }
    }

    if let Some(ref file_path) = args.file {
        let raw = read_message_file_raw(file_path)?;
        let text = String::from_utf8_lossy(&raw);
        buf.push_str(text.as_ref());
        if !buf.ends_with('\n') {
            buf.push('\n');
        }
        return Ok(buf);
    }

    let reuse_rev = args.reuse_message.as_ref().or(args.reedit_message.as_ref());
    if let Some(rev) = reuse_rev {
        let oid = resolve_revision_as_commit(repo, rev)?;
        let obj = repo.odb.read(&oid)?;
        let commit = grit_lib::objects::parse_commit(&obj.data)?;
        let body = skip_blank_lines(message_body_after_subject(&commit.message));
        buf.push_str(body);
        if !buf.is_empty() && !buf.ends_with('\n') {
            buf.push('\n');
        }
        return Ok(buf);
    }

    if let Some(msg) = grit_lib::state::read_merge_msg(&repo.git_dir)? {
        buf.push_str(&msg);
        return Ok(buf);
    }

    if repo.git_dir.join("REBASE_HEAD").exists() {
        if let Ok(msg) = fs::read_to_string(repo.git_dir.join("COMMIT_EDITMSG")) {
            if !msg.is_empty() {
                buf.push_str(&msg);
                return Ok(buf);
            }
        }
    }

    let squash_msg_path = repo.git_dir.join("SQUASH_MSG");
    if let Ok(msg) = fs::read_to_string(&squash_msg_path) {
        if !msg.is_empty() {
            buf.push_str(&msg);
            return Ok(buf);
        }
    }

    if let Some(tpl) = template_path {
        buf.push_str(
            &fs::read_to_string(tpl)
                .with_context(|| format!("fatal: could not read '{}'", tpl.display()))?,
        );
        return Ok(buf);
    }

    if args.amend {
        let head = resolve_head(&repo.git_dir)?;
        if let Some(oid) = head.oid() {
            let obj = repo.odb.read(oid)?;
            let commit = grit_lib::objects::parse_commit(&obj.data)?;
            buf.push_str(&commit.message);
            return Ok(buf);
        }
    }

    Ok(buf)
}

pub(crate) fn launch_commit_editor(repo: &Repository, path: &Path) -> Result<()> {
    let config = ConfigSet::load(Some(&repo.git_dir), true).unwrap_or_default();
    let mut editor = crate::editor::resolve_commit_launch_editor(&config)
        .ok_or_else(|| anyhow::anyhow!("Terminal is dumb, but EDITOR unset"))?;
    if editor.trim() == ":" {
        if let Ok(env_editor) = std::env::var("EDITOR") {
            if !env_editor.trim().is_empty() && env_editor.trim() != ":" {
                editor = env_editor;
            }
        }
    }

    // Git treats `:` as a no-op editor (`launch_specified_editor`).
    if editor.trim() == ":" {
        return Ok(());
    }
    // Match Git: the editor command is run under `sh -c` with the path as `$1` (not `$@`),
    // so `test_set_editor` patterns like `EDITOR='"$FAKE_EDITOR"'` expand and receive the file.
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(path);
    // Run from the work tree so editor scripts that use relative paths (e.g. `fake-input` in
    // t3452-history-split) see the same cwd as `git commit`.
    if let Some(wt) = repo.work_tree.as_ref() {
        cmd.current_dir(wt);
    } else {
        cmd.current_dir(&repo.git_dir);
    }
    let status = cmd
        .status()
        .with_context(|| format!("failed to launch editor '{editor}'"))?;
    if !status.success() {
        bail!("editor exited with non-zero status");
    }
    Ok(())
}

/// Post-editor cleanup matching Git `strbuf_stripspace` with `comment_prefix` (from
/// `core.commentChar` / `core.commentString`, default `#`): skip comment-prefixed lines, trim
/// trailing whitespace per line, collapse runs of empty lines to a single blank between paragraphs,
/// trim leading/trailing blank lines.
pub(crate) fn cleanup_edited_commit_message(message: &str, comment_prefix: &str) -> String {
    fn line_cleanup(line: &str) -> usize {
        let mut len = line.len();
        while len > 0 {
            let c = line.as_bytes()[len - 1];
            if !c.is_ascii_whitespace() {
                break;
            }
            len -= 1;
        }
        len
    }

    let mut out = String::new();
    let mut empties = 0usize;
    let mut i = 0usize;
    while i < message.len() {
        let rest = &message[i..];
        let (line_with_nl, advance) = if let Some(pos) = rest.find('\n') {
            (&rest[..=pos], pos + 1)
        } else {
            (rest, rest.len())
        };
        i += advance;

        if line_with_nl.starts_with(comment_prefix) {
            continue;
        }
        let content_len = line_cleanup(line_with_nl);
        if content_len > 0 {
            if empties > 0 && !out.is_empty() {
                out.push('\n');
            }
            empties = 0;
            out.push_str(&line_with_nl[..content_len]);
            out.push('\n');
        } else {
            empties += 1;
        }
    }
    out
}

fn git_vertical_stripspace(s: &str) -> String {
    let trimmed_start = s.trim_start_matches(['\n', '\r']);
    trimmed_start
        .trim_end_matches(['\n', '\r', ' ', '\t'])
        .to_string()
}

/// Byte-level twin of [`git_vertical_stripspace`]: strip leading `\n`/`\r` and any
/// trailing horizontal/vertical whitespace, operating on raw bytes so that invalid
/// UTF-8 message payloads (Git stores commit bodies verbatim) survive cleanup.
fn git_vertical_stripspace_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut start = 0usize;
    while start < bytes.len() && matches!(bytes[start], b'\n' | b'\r') {
        start += 1;
    }
    let mut end = bytes.len();
    while end > start && matches!(bytes[end - 1], b'\n' | b'\r' | b' ' | b'\t') {
        end -= 1;
    }
    bytes[start..end].to_vec()
}

/// Byte-level twin of [`cleanup_edited_commit_message`]: drop comment lines, trim each
/// line's trailing whitespace, and collapse runs of blank lines. Operates on raw bytes
/// so invalid-UTF-8 payloads are preserved.
fn cleanup_edited_commit_message_bytes(bytes: &[u8], comment_prefix: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut empties = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let line_end = match bytes[i..].iter().position(|&b| b == b'\n') {
            Some(pos) => i + pos + 1,
            None => bytes.len(),
        };
        let line = &bytes[i..line_end];
        i = line_end;

        if !comment_prefix.is_empty() && line.starts_with(comment_prefix) {
            continue;
        }
        let mut content_len = line.len();
        while content_len > 0 && line[content_len - 1].is_ascii_whitespace() {
            content_len -= 1;
        }
        if content_len > 0 {
            if empties > 0 && !out.is_empty() {
                out.push(b'\n');
            }
            empties = 0;
            out.extend_from_slice(&line[..content_len]);
            out.push(b'\n');
        } else {
            empties += 1;
        }
    }
    out
}

/// Apply `mode`'s cleanup to a raw (possibly non-UTF-8) message body and return the
/// bytes Git would store, with a single trailing newline when non-empty. Mirrors
/// [`apply_cleanup_message`] for the non-`Scissors`/non-verbose cases that `-m`/`-F`
/// take. The caller only routes non-UTF-8 payloads here, so scissors/verbose (which
/// require text scanning) never apply.
fn cleanup_message_bytes(
    bytes: &[u8],
    comment_prefix: &[u8],
    mode: CommitMsgCleanupMode,
) -> Vec<u8> {
    let mut cleaned = match mode {
        CommitMsgCleanupMode::None => bytes.to_vec(),
        CommitMsgCleanupMode::All => cleanup_edited_commit_message_bytes(bytes, comment_prefix),
        CommitMsgCleanupMode::Space | CommitMsgCleanupMode::Scissors => {
            git_vertical_stripspace_bytes(bytes)
        }
    };
    if !cleaned.is_empty() && !cleaned.ends_with(b"\n") {
        cleaned.push(b'\n');
    }
    cleaned
}

fn rest_is_empty_signedoff_only(s: &str, start: usize) -> bool {
    const SOB: &str = "Signed-off-by:";
    let rest = s.get(start..).unwrap_or("");
    for line in rest.split_inclusive('\n') {
        let line_no_nl = line.strip_suffix('\n').unwrap_or(line);
        let t = line_no_nl.trim();
        if t.is_empty() {
            continue;
        }
        if t.starts_with(SOB) {
            continue;
        }
        return false;
    }
    true
}

fn replace_editmsg_user_message(previous: &[u8], message: &str, comment_prefix: &str) -> Vec<u8> {
    let Ok(previous_text) = std::str::from_utf8(previous) else {
        return previous.to_vec();
    };
    let suffix_start = previous_text
        .split_inclusive('\n')
        .scan(0usize, |offset, line| {
            let current = *offset;
            *offset += line.len();
            Some((current, line))
        })
        .find_map(|(offset, line)| line.starts_with(comment_prefix).then_some(offset))
        .unwrap_or(previous_text.len());
    let suffix = &previous_text[suffix_start..];
    let mut restored = message.to_string();
    if !restored.is_empty() && !restored.ends_with('\n') {
        restored.push('\n');
    }
    if !suffix.is_empty() && !restored.is_empty() && !restored.ends_with("\n\n") {
        restored.push('\n');
    }
    restored.push_str(suffix);
    restored.into_bytes()
}

fn message_is_empty_or_signedoff_only(message: &str) -> bool {
    let mut saw_signoff = false;
    for line in message.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !trimmed.starts_with("Signed-off-by:") {
            return false;
        }
        saw_signoff = true;
    }
    saw_signoff
}

fn template_untouched(
    message: &str,
    template_path: &Path,
    comment_prefix: &str,
    cleanup_mode: CommitMsgCleanupMode,
) -> bool {
    let Ok(tmpl_raw) = fs::read_to_string(template_path) else {
        return false;
    };
    let tmpl = match cleanup_mode {
        CommitMsgCleanupMode::None => tmpl_raw,
        CommitMsgCleanupMode::All => cleanup_edited_commit_message(&tmpl_raw, comment_prefix),
        CommitMsgCleanupMode::Space | CommitMsgCleanupMode::Scissors => {
            git_vertical_stripspace(&tmpl_raw)
        }
    };
    let msg = match cleanup_mode {
        CommitMsgCleanupMode::None => message.to_string(),
        CommitMsgCleanupMode::All => cleanup_edited_commit_message(message, comment_prefix),
        CommitMsgCleanupMode::Space | CommitMsgCleanupMode::Scissors => {
            git_vertical_stripspace(message)
        }
    };
    let after_prefix = msg.strip_prefix(&tmpl).unwrap_or(msg.as_str());
    rest_is_empty_signedoff_only(msg.as_str(), msg.len().saturating_sub(after_prefix.len()))
}

fn branch_display_name(head: &HeadState) -> String {
    match head {
        HeadState::Branch { short_name, .. } => short_name.clone(),
        HeadState::Detached { .. } => "HEAD detached".to_string(),
        HeadState::Invalid => "unknown".to_string(),
    }
}

/// Full `core.commentChar` / `core.commentString` prefix (Git may use multi-character prefixes).
pub(crate) fn comment_line_prefix_full(config: &ConfigSet) -> Cow<'_, str> {
    let raw = config
        .get("core.commentchar")
        .or_else(|| config.get("core.commentChar"))
        .or_else(|| config.get("core.commentstring"))
        .or_else(|| config.get("core.commentString"));
    let Some(s) = raw else {
        return Cow::Borrowed("#");
    };
    let t = s.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("auto") {
        Cow::Borrowed("#")
    } else {
        Cow::Owned(t.to_string())
    }
}

fn has_auto_comment_char(config: &ConfigSet) -> bool {
    config.entries().iter().any(|entry| {
        entry.key == "core.commentchar"
            && entry
                .value
                .as_deref()
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("auto"))
    })
}

fn auto_comment_prefix_for_message(message: &str) -> Option<String> {
    const CANDIDATES: &[char] = &['#', ';', '@', '!', '$', '%', '^', '&', '|', ':'];
    CANDIDATES
        .iter()
        .copied()
        .find(|candidate| !message.lines().any(|line| line.starts_with(*candidate)))
        .map(|c| c.to_string())
}

fn warn_auto_comment_char_deprecated() {
    eprintln!(
        "warning: Support for 'core.commentChar=auto' is deprecated and will be removed in Git 3.0"
    );
    eprintln!("hint:");
    eprintln!("hint: To use the default comment string (#) please run");
    eprintln!("hint:");
    eprintln!("hint:     git config unset core.commentChar");
    eprintln!("hint:     git config unset --file ~/config-include --all core.commentString");
    eprintln!("hint:     git config unset --file ~/config-include core.commentChar");
    eprintln!("hint:");
    eprintln!("hint: To set a custom comment string please run");
    eprintln!("hint:");
    eprintln!("hint:     git config set --file ~/config-include core.commentChar <comment string>");
    eprintln!("hint:");
    eprintln!("hint: where '<comment string>' is the string you wish to use.");
}

fn comment_line_prefix_for_message(
    config: &ConfigSet,
    message: &str,
    warn: bool,
) -> Result<String> {
    if has_auto_comment_char(config) {
        if warn {
            warn_auto_comment_char_deprecated();
        }
        return auto_comment_prefix_for_message(message)
            .ok_or_else(|| anyhow::anyhow!("fatal: unable to select a comment character"));
    }
    Ok(comment_line_prefix_full(config).into_owned())
}

/// Git `commit.verbose`: bool or integer; `-1` when unset (inherit default 0).
fn config_commit_verbose_raw(config: &ConfigSet) -> i64 {
    let Some(v) = config.get("commit.verbose") else {
        return -1;
    };
    let t = v.trim();
    let lower = t.to_ascii_lowercase();
    if matches!(lower.as_str(), "true" | "yes" | "on" | "1" | "") {
        return 1;
    }
    if matches!(lower.as_str(), "false" | "no" | "off" | "0") {
        return 0;
    }
    t.parse::<i64>().unwrap_or(0)
}

/// Effective verbosity level after argv (`GIT_GRIT_INTERNAL_COMMIT_VERBOSE`) and `commit.verbose`.
fn resolve_commit_verbose_level(args: &Args, config: &ConfigSet) -> i64 {
    let cfg_v = config_commit_verbose_raw(config);
    let scanned = std::env::var(GIT_GRIT_COMMIT_VERBOSE_ENV)
        .ok()
        .and_then(|s| s.parse::<u32>().ok());
    let _ = std::env::remove_var(GIT_GRIT_COMMIT_VERBOSE_ENV);
    let clap_v = u32::from(args.verbose);
    let cli_count = scanned.unwrap_or(clap_v);
    if scanned.is_some() {
        return i64::from(cli_count);
    }
    if clap_v > 0 {
        return i64::from(clap_v);
    }
    if cfg_v >= 0 {
        cfg_v
    } else {
        0
    }
}

/// Git `get_cleanup_mode` / `cleanup_message` modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommitMsgCleanupMode {
    None,
    Space,
    All,
    Scissors,
}

fn resolve_commit_cleanup_mode(
    args: &Args,
    config: &ConfigSet,
    use_editor: bool,
) -> CommitMsgCleanupMode {
    let cleanup_cfg = config.get("commit.cleanup");
    let cleanup = args.cleanup.as_deref().or(cleanup_cfg.as_deref());
    match cleanup.map(|s| s.trim()) {
        None | Some("default") => {
            if use_editor {
                CommitMsgCleanupMode::All
            } else {
                CommitMsgCleanupMode::Space
            }
        }
        Some("verbatim") => CommitMsgCleanupMode::None,
        Some("whitespace") => CommitMsgCleanupMode::Space,
        Some("strip") => CommitMsgCleanupMode::All,
        Some("scissors") => {
            if use_editor {
                CommitMsgCleanupMode::Scissors
            } else {
                CommitMsgCleanupMode::Space
            }
        }
        Some(_) => {
            if use_editor {
                CommitMsgCleanupMode::All
            } else {
                CommitMsgCleanupMode::Space
            }
        }
    }
}

/// Match Git `cleanup_message`: truncate before scissors when `verbose` or `scissors` mode; then
/// `strbuf_stripspace` with comment prefix only for `All`.
fn apply_cleanup_message(
    message: &str,
    verbose_level: i64,
    comment_prefix: &str,
    mode: CommitMsgCleanupMode,
) -> String {
    let truncate = verbose_level > 0 || mode == CommitMsgCleanupMode::Scissors;
    let truncated = if truncate {
        truncate_at_verbose_cutoff(message, comment_prefix)
    } else {
        message.to_string()
    };
    match mode {
        CommitMsgCleanupMode::None => truncated,
        CommitMsgCleanupMode::All => cleanup_edited_commit_message(&truncated, comment_prefix),
        CommitMsgCleanupMode::Space | CommitMsgCleanupMode::Scissors => {
            git_vertical_stripspace(&truncated)
        }
    }
}

/// Locate end of user message before the scissors line (Git `wt_status_locate_end`).
fn wt_status_locate_end(message: &str, comment_prefix: &str) -> usize {
    let cut = GIT_COMMIT_CUT_LINE;
    let lead = format!("{comment_prefix} {cut}");
    if message.starts_with(&lead) {
        return 0;
    }
    let needle = format!("\n{comment_prefix} {cut}");
    if let Some(pos) = message.find(&needle) {
        return pos.saturating_add(1);
    }
    message.len()
}

fn truncate_at_verbose_cutoff(message: &str, comment_prefix: &str) -> String {
    let end = wt_status_locate_end(message, comment_prefix);
    message.get(..end).unwrap_or("").to_string()
}

fn diff_mnemonic_prefix(config: &ConfigSet) -> bool {
    config.get("diff.mnemonicprefix").is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "true" | "yes" | "on" | "1" | ""
        )
    })
}

fn append_commented_line(buf: &mut String, comment_prefix: &str, body: &str) {
    buf.push_str(comment_prefix);
    if !body.is_empty() && !body.starts_with(['\n', '\t']) {
        buf.push(' ');
    }
    buf.push_str(body);
    if !body.ends_with('\n') {
        buf.push('\n');
    }
}

fn append_verbose_cut_line(buf: &mut String, comment_prefix: &str) {
    let explanation =
        "Do not modify or remove the line above.\nEverything below it will be ignored.\n";
    append_commented_line(
        buf,
        comment_prefix,
        GIT_COMMIT_CUT_LINE.trim_end_matches('\n'),
    );
    for line in explanation.split_inclusive('\n') {
        let line = line.strip_suffix('\n').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        append_commented_line(buf, comment_prefix, line);
    }
}

/// Append unified diffs to the commit message template (Git `wt_longstatus_print_verbose`).
fn append_commit_verbose_diffs(
    args: &Args,
    repo: &Repository,
    config: &ConfigSet,
    head: &HeadState,
    staged: &[DiffEntry],
    unstaged: &[DiffEntry],
    verbose_level: i64,
    buf: &mut String,
) -> Result<()> {
    if verbose_level <= 0 {
        return Ok(());
    }
    let Some(wt) = repo.work_tree.as_deref() else {
        return Ok(());
    };
    let committable = !staged.is_empty();
    let worktree_dirty = !unstaged.is_empty();
    let mnemonic = diff_mnemonic_prefix(config);
    let comment = comment_line_prefix_full(config).into_owned();
    let index_file = resolved_index_path(repo);

    append_verbose_cut_line(buf, &comment);

    let (a1, b1) = if verbose_level > 1 && committable {
        ("c/", "i/")
    } else if mnemonic {
        ("c/", "i/")
    } else {
        ("a/", "b/")
    };

    if verbose_level > 1 && committable {
        buf.push('\n');
        append_commented_line(buf, &comment, "Changes to be committed:");
    }

    // Match Git `wt_longstatus_print_verbose`: base tree is `HEAD^` when amending (index vs parent),
    // otherwise `HEAD`. Root amend uses the empty tree.
    let cached_base = if args.amend {
        match head.oid() {
            Some(oid) => {
                let obj = repo.odb.read(oid)?;
                let c = grit_lib::objects::parse_commit(&obj.data)?;
                if c.parents.is_empty() {
                    Cow::Borrowed("4b825dc642cb6eb9a060e54bf8d69288fbee4904")
                } else {
                    Cow::Owned(format!("{}^", oid.to_hex()))
                }
            }
            None => Cow::Borrowed("HEAD"),
        }
    } else {
        Cow::Borrowed("HEAD")
    };

    let out1 = Command::new(crate::grit_exe::grit_executable())
        .current_dir(wt)
        .env("GIT_DIR", &repo.git_dir)
        .env("GIT_INDEX_FILE", &index_file)
        .args([
            "-c",
            "diff.noprefix=false",
            "-c",
            "diff.mnemonicprefix=false",
            "diff",
            "--cached",
            "-p",
            "--color=never",
            "--src-prefix",
            a1,
            "--dst-prefix",
            b1,
            cached_base.as_ref(),
        ])
        .output()
        .context("run grit diff --cached for commit template")?;
    if out1.status.success() {
        let patch = String::from_utf8_lossy(&out1.stdout);
        buf.push_str(&patch);
    }

    if verbose_level > 1 && worktree_dirty {
        buf.push('\n');
        append_commented_line(
            buf,
            &comment,
            "--------------------------------------------------",
        );
        append_commented_line(buf, &comment, "Changes not staged for commit:");
        let out2 = Command::new(crate::grit_exe::grit_executable())
            .current_dir(wt)
            .env("GIT_DIR", &repo.git_dir)
            .env("GIT_INDEX_FILE", &index_file)
            .args([
                "-c",
                "diff.noprefix=false",
                "-c",
                "diff.mnemonicprefix=false",
                "diff",
                "-p",
                "--color=never",
                "--src-prefix",
                "i/",
                "--dst-prefix",
                "w/",
            ])
            .output()
            .context("run grit diff for commit template")?;
        if out2.status.success() {
            let patch = String::from_utf8_lossy(&out2.stdout);
            buf.push_str(&patch);
        }
    }

    Ok(())
}

fn commit_template_includes_status(args: &Args, config: &ConfigSet) -> bool {
    if args.no_status {
        return false;
    }
    if args.status {
        return true;
    }
    config.get("commit.status").map_or(true, |value| {
        !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "false" | "no" | "off" | "0"
        )
    })
}

fn commit_template_status_append(
    args: &Args,
    repo: &Repository,
    head: &HeadState,
    config: &ConfigSet,
    buf: &mut String,
) -> Result<()> {
    let cp = comment_line_prefix_full(config);
    commit_template_status_append_with_prefix(args, repo, head, config, cp.as_ref(), buf)
}

fn commit_template_status_append_with_prefix(
    args: &Args,
    repo: &Repository,
    head: &HeadState,
    config: &ConfigSet,
    p: &str,
    buf: &mut String,
) -> Result<()> {
    buf.push('\n');
    if args.allow_empty_message {
        append_commented_line(
            buf,
            p,
            "Please enter the commit message for your changes. Lines starting",
        );
        append_commented_line(buf, p, &format!("with '{p}' will be ignored."));
    } else {
        append_commented_line(
            buf,
            p,
            "Please enter the commit message for your changes. Lines starting",
        );
        append_commented_line(
            buf,
            p,
            &format!("with '{p}' will be ignored, and an empty message aborts the commit."),
        );
    }
    if args.allow_empty_message {
        buf.push_str(p);
        buf.push('\n');
    }
    let author = resolve_author(args, config, repo, OffsetDateTime::now_utc())?;
    let author_display = author
        .split_once('>')
        .map(|(a, _)| format!("{}>", a.trim()))
        .unwrap_or_else(|| author.clone());
    append_commented_line(buf, p, &format!("Author:    {author_display}"));
    if args.date.is_some() {
        if let Some(display) = format_commit_summary_date(&author) {
            append_commented_line(buf, p, &format!("Date:      {display}"));
        }
    }
    append_commented_line(buf, p, "");
    append_commented_line(buf, p, &format!("On branch {}", branch_display_name(head)));
    append_commented_line(buf, p, "Changes to be committed:");

    if let Some(wt) = repo.work_tree.as_deref() {
        let index_file = resolved_index_path(repo);
        let output = Command::new(crate::grit_exe::grit_executable())
            .current_dir(wt)
            .env("GIT_DIR", &repo.git_dir)
            .env("GIT_INDEX_FILE", &index_file)
            .args(["diff", "--cached", "--name-status"])
            .output();
        if let Ok(out) = output {
            if out.status.success() {
                let text = String::from_utf8_lossy(&out.stdout);
                for line in text.lines() {
                    let line = line.trim_end();
                    if line.is_empty() {
                        continue;
                    }
                    let parts: Vec<&str> = line.split('\t').collect();
                    if parts.is_empty() {
                        continue;
                    }
                    let status = parts[0];
                    let (label, display_path) =
                        if status.starts_with('R') || status.starts_with('C') {
                            if parts.len() >= 3 {
                                let lbl = if status.starts_with('R') {
                                    "renamed"
                                } else {
                                    "copied"
                                };
                                (lbl, format!("{} -> {}", parts[1], parts[2]))
                            } else {
                                continue;
                            }
                        } else {
                            let lbl = match status.chars().next() {
                                Some('A') => "new file",
                                Some('D') => "deleted",
                                Some('M') => "modified",
                                Some('T') => "typechange",
                                _ => "changed",
                            };
                            let path_cell = parts.get(1).copied().unwrap_or("");
                            (lbl, path_cell.to_string())
                        };
                    buf.push_str(p);
                    buf.push('\t');
                    buf.push_str(&format!("{label}:   {display_path}\n"));
                }
                buf.push_str(p);
                buf.push('\n');
                buf.push_str(p);
                buf.push_str(" Untracked files not listed\n");
                return Ok(());
            }
        }
    }

    let index = match repo.load_index_at(&resolved_index_path(repo)) {
        Ok(i) => i,
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Index::new(),
        Err(e) => return Err(e.into()),
    };
    let head_tree = match head.oid() {
        Some(oid) => {
            let obj = repo.odb.read(oid)?;
            let c = grit_lib::objects::parse_commit(&obj.data)?;
            Some(c.tree)
        }
        None => None,
    };
    let staged = diff_index_to_tree(&repo.odb, &index, head_tree.as_ref(), false)?;
    for e in &staged {
        let label = status_label_staged(e.status);
        buf.push_str(p);
        buf.push('\t');
        buf.push_str(&format!("{label}:   {}\n", e.display_path()));
    }
    buf.push_str(p);
    buf.push('\n');
    buf.push_str(p);
    buf.push_str(" Untracked files not listed\n");
    Ok(())
}

fn prepare_commit_message(
    args: &Args,
    repo: &Repository,
    config: &ConfigSet,
    fixup: Option<&FixupParsed>,
    template_path: Option<&Path>,
    use_editor: bool,
    head: &HeadState,
    staged: &[DiffEntry],
    unstaged: &[DiffEntry],
    verbose_level: i64,
    // Runs the `prepare-commit-msg` hook on the editor template file (the full message
    // buffer including status comments) immediately before launching the editor, matching
    // Git's `prepare_to_commit` order: hook first, editor second. For non-editor commits
    // the caller runs the hook itself after the message is assembled.
    run_prepare_hook: &dyn Fn(&Path) -> Result<()>,
) -> Result<MessageResult> {
    let comment_owned = comment_line_prefix_full(config);
    let comment_prefix = comment_owned.as_ref();
    let cleanup_mode = resolve_commit_cleanup_mode(args, config, use_editor);

    if let Some(sq) = args.squash.as_deref() {
        let reuse = args
            .reuse_message
            .as_deref()
            .or(args.reedit_message.as_deref());
        let prefix = build_squash_prefix(repo, sq, reuse)?;
        let mut body = String::new();
        if !args.message.is_empty() {
            body.push_str(&args.message.join("\n\n"));
        } else if let Some(ref fp) = args.file {
            let raw = read_message_file_raw(fp)?;
            body.push_str(&String::from_utf8_lossy(&raw));
        } else if let Some(rev) = reuse {
            let oid = resolve_revision_as_commit(repo, rev)?;
            let obj = repo.odb.read(&oid)?;
            let commit = grit_lib::objects::parse_commit(&obj.data)?;
            if args.reedit_message.is_some() {
                let edit_path = repo.git_dir.join("COMMIT_EDITMSG");
                let mut file_body = prefix.clone();
                file_body.push_str(&commit.message);
                if commit_template_includes_status(args, config) {
                    commit_template_status_append(args, repo, head, config, &mut file_body)?;
                }
                if verbose_level > 0 {
                    append_commit_verbose_diffs(
                        args,
                        repo,
                        config,
                        head,
                        staged,
                        unstaged,
                        verbose_level,
                        &mut file_body,
                    )?;
                }
                fs::write(&edit_path, &file_body)?;
                run_prepare_hook(&edit_path)?;
                launch_commit_editor(repo, &edit_path)?;
                let edited = fs::read_to_string(&edit_path)?;
                let cleaned =
                    apply_cleanup_message(&edited, verbose_level, comment_prefix, cleanup_mode);
                return Ok(MessageResult {
                    message: ensure_trailing_newline(&cleaned),
                    raw_bytes: None,
                    from_merge_msg: false,
                });
            }
            if rev == sq {
                let subj = first_line(&commit.message);
                body.push_str(subj);
            } else {
                // `-C`: reuse the full commit log (including its subject) after the squash prefix.
                body.push_str(&commit.message);
            }
        } else if use_editor {
            let edit_path = repo.git_dir.join("COMMIT_EDITMSG");
            let mut file_body = prefix.clone();
            if file_body.trim().is_empty() {
                file_body.push('\n');
            }
            if commit_template_includes_status(args, config) {
                commit_template_status_append(args, repo, head, config, &mut file_body)?;
            }
            if verbose_level > 0 {
                append_commit_verbose_diffs(
                    args,
                    repo,
                    config,
                    head,
                    staged,
                    unstaged,
                    verbose_level,
                    &mut file_body,
                )?;
            }
            fs::write(&edit_path, &file_body)?;
            run_prepare_hook(&edit_path)?;
            launch_commit_editor(repo, &edit_path)?;
            let edited = fs::read_to_string(&edit_path)?;
            let cleaned =
                apply_cleanup_message(&edited, verbose_level, comment_prefix, cleanup_mode);
            return Ok(MessageResult {
                message: ensure_trailing_newline(&cleaned),
                raw_bytes: None,
                from_merge_msg: false,
            });
        }
        let combined = format!("{prefix}{body}");
        return Ok(MessageResult {
            message: ensure_trailing_newline(&combined),
            raw_bytes: None,
            from_merge_msg: false,
        });
    }

    if !args.message.is_empty() && fixup.map(|f| matches!(f.mode, FixupMode::Fixup)) != Some(true) {
        let msg = args.message.join("\n\n");
        if use_editor {
            let edit_path = repo.git_dir.join("COMMIT_EDITMSG");
            let mut file_body = msg;
            if !file_body.ends_with('\n') {
                file_body.push('\n');
            }
            if commit_template_includes_status(args, config) {
                commit_template_status_append(args, repo, head, config, &mut file_body)?;
            }
            if verbose_level > 0 {
                append_commit_verbose_diffs(
                    args,
                    repo,
                    config,
                    head,
                    staged,
                    unstaged,
                    verbose_level,
                    &mut file_body,
                )?;
            }
            fs::write(&edit_path, &file_body)?;
            run_prepare_hook(&edit_path)?;
            launch_commit_editor(repo, &edit_path)?;
            let edited = fs::read_to_string(&edit_path)?;
            let cleaned =
                apply_cleanup_message(&edited, verbose_level, comment_prefix, cleanup_mode);
            return Ok(MessageResult {
                message: ensure_trailing_newline(&cleaned),
                raw_bytes: None,
                from_merge_msg: false,
            });
        }
        let no_editor_cleanup = resolve_commit_cleanup_mode(args, config, false);
        let cleaned = apply_cleanup_message(&msg, 0, comment_prefix, no_editor_cleanup);
        // Git stores the commit body verbatim and never transcodes it. When the raw
        // `-m` argv bytes are not valid UTF-8 (e.g. an unknown `i18n.commitEncoding`),
        // preserve them by cleaning the raw bytes and routing them through `raw_bytes`.
        let raw_bytes = if args.raw_messages.len() == args.message.len()
            && args
                .raw_messages
                .iter()
                .any(|m| std::str::from_utf8(m).is_err())
        {
            let mut joined: Vec<u8> = Vec::new();
            for (idx, m) in args.raw_messages.iter().enumerate() {
                if idx > 0 {
                    joined.extend_from_slice(b"\n\n");
                }
                joined.extend_from_slice(m);
            }
            Some(cleanup_message_bytes(
                &joined,
                comment_prefix.as_bytes(),
                no_editor_cleanup,
            ))
        } else {
            None
        };
        return Ok(MessageResult {
            message: ensure_trailing_newline(&cleaned),
            raw_bytes,
            from_merge_msg: false,
        });
    }

    if let Some(ref file_path) = args.file {
        let raw = read_message_file_raw(file_path)?;
        let text = String::from_utf8_lossy(&raw).to_string();
        if use_editor {
            let edit_path = repo.git_dir.join("COMMIT_EDITMSG");
            let mut file_body = ensure_trailing_newline(&text);
            if commit_template_includes_status(args, config) {
                commit_template_status_append(args, repo, head, config, &mut file_body)?;
            }
            if verbose_level > 0 {
                append_commit_verbose_diffs(
                    args,
                    repo,
                    config,
                    head,
                    staged,
                    unstaged,
                    verbose_level,
                    &mut file_body,
                )?;
            }
            fs::write(&edit_path, &file_body)?;
            run_prepare_hook(&edit_path)?;
            launch_commit_editor(repo, &edit_path)?;
            let edited = fs::read_to_string(&edit_path)?;
            let cleaned =
                apply_cleanup_message(&edited, verbose_level, comment_prefix, cleanup_mode);
            return Ok(MessageResult {
                message: ensure_trailing_newline(&cleaned),
                raw_bytes: None,
                from_merge_msg: false,
            });
        }
        if cleanup_mode == CommitMsgCleanupMode::None {
            return raw_to_message_result(raw);
        }
        let cleaned = apply_cleanup_message(&text, 0, comment_prefix, cleanup_mode);
        // Preserve verbatim bytes (Git never transcodes the body) when the `-F` file
        // content is not valid UTF-8; apply the same cleanup at the byte level.
        let raw_bytes = if std::str::from_utf8(&raw).is_err() {
            Some(cleanup_message_bytes(
                &raw,
                comment_prefix.as_bytes(),
                cleanup_mode,
            ))
        } else {
            None
        };
        return Ok(MessageResult {
            message: ensure_trailing_newline(&cleaned),
            raw_bytes,
            from_merge_msg: false,
        });
    }

    let reuse_rev = args.reuse_message.as_ref().or(args.reedit_message.as_ref());
    if let Some(rev) = reuse_rev {
        let oid = resolve_revision_as_commit(repo, rev)?;
        let obj = repo.odb.read(&oid)?;
        let commit = grit_lib::objects::parse_commit(&obj.data)?;
        if args.reedit_message.is_some() {
            let edit_path = repo.git_dir.join("COMMIT_EDITMSG");
            let mut file_body = ensure_trailing_newline(&commit.message);
            if commit_template_includes_status(args, config) {
                commit_template_status_append(args, repo, head, config, &mut file_body)?;
            }
            if verbose_level > 0 {
                append_commit_verbose_diffs(
                    args,
                    repo,
                    config,
                    head,
                    staged,
                    unstaged,
                    verbose_level,
                    &mut file_body,
                )?;
            }
            fs::write(&edit_path, &file_body)?;
            run_prepare_hook(&edit_path)?;
            launch_commit_editor(repo, &edit_path)?;
            let edited = fs::read_to_string(&edit_path)?;
            let cleaned =
                apply_cleanup_message(&edited, verbose_level, comment_prefix, cleanup_mode);
            return Ok(MessageResult {
                message: ensure_trailing_newline(&cleaned),
                raw_bytes: None,
                from_merge_msg: false,
            });
        }
        return Ok(MessageResult {
            message: commit.message,
            raw_bytes: None,
            from_merge_msg: false,
        });
    }

    let initial = build_initial_commit_buffer(args, repo, fixup, template_path)?;

    // `git commit --amend --no-edit` (or implied no-edit): reuse HEAD without the editor.
    // `--fixup` overrides this: the message must become `fixup!`/`amend!`/`squash!` of the
    // target's subject even when `--amend` replaces the current commit (t3404 `--update-refs`).
    if args.amend && !use_editor && fixup.is_none() {
        let head_st = resolve_head(&repo.git_dir)?;
        if let Some(oid) = head_st.oid() {
            let obj = repo.odb.read(oid)?;
            let commit = grit_lib::objects::parse_commit(&obj.data)?;
            return Ok(MessageResult {
                message: commit.message,
                raw_bytes: None,
                from_merge_msg: false,
            });
        }
    }

    if args.allow_empty_message
        && initial.trim().is_empty()
        && template_path.is_none()
        && fixup.is_none()
        && args.squash.is_none()
        && !use_editor
    {
        return Ok(MessageResult {
            message: String::new(),
            raw_bytes: None,
            from_merge_msg: false,
        });
    }

    if !use_editor && fixup.is_some() {
        return Ok(MessageResult {
            message: ensure_trailing_newline(&initial),
            raw_bytes: None,
            from_merge_msg: false,
        });
    }

    if use_editor {
        let edit_path = repo.git_dir.join("COMMIT_EDITMSG");
        let mut file_body = initial;
        let selected_comment = comment_line_prefix_for_message(config, &file_body, true)?;
        let comment_prefix = selected_comment.as_str();
        if commit_template_includes_status(args, config) {
            commit_template_status_append_with_prefix(
                args,
                repo,
                head,
                config,
                comment_prefix,
                &mut file_body,
            )?;
        }
        if verbose_level > 0 {
            append_commit_verbose_diffs(
                args,
                repo,
                config,
                head,
                staged,
                unstaged,
                verbose_level,
                &mut file_body,
            )?;
        }
        fs::write(&edit_path, &file_body)?;
        run_prepare_hook(&edit_path)?;
        launch_commit_editor(repo, &edit_path)?;
        let edited = fs::read_to_string(&edit_path)?;
        let cleaned = apply_cleanup_message(&edited, verbose_level, comment_prefix, cleanup_mode);
        return Ok(MessageResult {
            message: ensure_trailing_newline(&cleaned),
            raw_bytes: None,
            from_merge_msg: false,
        });
    }

    if let Some(msg) = grit_lib::state::read_merge_msg(&repo.git_dir)? {
        let msg = if args
            .cleanup
            .as_deref()
            .is_some_and(|m| m.eq_ignore_ascii_case("strip"))
        {
            cleanup_edited_commit_message(&msg, comment_prefix)
        } else {
            msg
        };
        return Ok(MessageResult {
            message: ensure_trailing_newline(&msg),
            raw_bytes: None,
            from_merge_msg: true,
        });
    }

    let squash_msg_path = repo.git_dir.join("SQUASH_MSG");
    if let Ok(msg) = fs::read_to_string(&squash_msg_path) {
        if !msg.is_empty() {
            return Ok(MessageResult {
                message: ensure_trailing_newline(&msg),
                raw_bytes: None,
                from_merge_msg: false,
            });
        }
    }

    if let Some(tpl) = template_path {
        let content = fs::read_to_string(tpl)
            .with_context(|| format!("fatal: could not read '{}'", tpl.display()))?;
        return Ok(MessageResult {
            message: ensure_trailing_newline(&content),
            raw_bytes: None,
            from_merge_msg: false,
        });
    }

    if args.allow_empty_message {
        return Ok(MessageResult {
            message: String::new(),
            raw_bytes: None,
            from_merge_msg: false,
        });
    }

    bail!("no commit message provided (use -m or -F)");
}

/// Parse `git commit --author="Name <email>"` parameter into name and email.
fn parse_force_author_parameter(author: &str) -> Result<(String, String)> {
    let Some(lt) = author.find('<') else {
        bail!("malformed --author parameter");
    };
    let Some(gt) = author.rfind('>') else {
        bail!("malformed --author parameter");
    };
    if gt <= lt {
        bail!("malformed --author parameter");
    }
    // Git trims both ends of the name (`split_ident_line`); leading spaces must not be stored.
    let name = author[..lt].trim();
    let email = author[lt + 1..gt].trim();
    if name.is_empty() {
        bail!("empty ident name (for <author>) not allowed");
    }
    // Git accepts an empty email in `Name <>` (see split_ident_line / t4203 empty-syntax tests).
    if lt > 0 && author.as_bytes()[lt - 1] != b' ' {
        bail!("malformed --author parameter");
    }
    Ok((name.to_string(), email.to_string()))
}

/// Resolve `git commit --author=nick` when `nick` is not `Name <email>` (Git `find_author_by_nickname`).
fn find_author_by_nickname(repo: &Repository, nick: &str) -> Result<String> {
    let mailmap = load_mailmap_table(repo)?;
    let pat = regex::escape(nick);
    let re = RegexBuilder::new(&pat)
        .case_insensitive(true)
        .build()
        .map_err(|e| anyhow::anyhow!("invalid author nickname pattern: {e}"))?;

    let mut tips: Vec<String> = list_refs(&repo.git_dir, "refs/")?
        .into_iter()
        .map(|(_, oid)| oid.to_hex())
        .collect();
    if tips.is_empty() {
        let head = resolve_head(&repo.git_dir)?;
        if let Some(oid) = head.oid() {
            tips.push(oid.to_hex());
        }
    }
    if tips.is_empty() {
        bail!(
            "--author '{}' is not 'Name <email>' and matches no existing author",
            nick
        );
    }

    let opts = RevListOptions {
        all_refs: false,
        first_parent: false,
        ordering: grit_lib::rev_list::OrderingMode::Topo,
        ..RevListOptions::default()
    };
    let result = rev_list(repo, &tips, &[], &opts)
        .map_err(|_| anyhow::anyhow!("revision walk setup failed"))?;

    for oid in result.commits {
        let obj = repo.odb.read(&oid)?;
        let commit = parse_commit(&obj.data)?;
        let (name, email, _) = split_stored_author_line(&commit.author)?;
        let (mn, me) = mailmap.map_user(name, email);
        let mapped_line = format!("{mn} <{me}>");
        if re.is_match(&mapped_line) {
            return Ok(mapped_line);
        }
    }

    bail!(
        "--author '{}' is not 'Name <email>' and matches no existing author",
        nick
    );
}

/// Build `GIT_AUTHOR_*` values for hook subprocesses (matches Git `determine_author_info` / `export_one`).
pub(crate) fn author_env_for_commit_hooks(author_line: &str) -> Result<Vec<(String, String)>> {
    let (name, email, date_tail) = split_stored_author_line(author_line)?;
    let mut out = vec![
        ("GIT_AUTHOR_NAME".to_string(), name),
        ("GIT_AUTHOR_EMAIL".to_string(), email),
    ];
    if let Some(dt) = date_tail.filter(|s| !s.is_empty()) {
        out.push(("GIT_AUTHOR_DATE".to_string(), format!("@{dt}")));
    }
    Ok(out)
}

/// Split a stored author line (`name <email> <epoch> <tz>`) into name, email, and optional date tail.
pub(crate) fn split_stored_author_line(author: &str) -> Result<(String, String, Option<String>)> {
    let Some(lt) = author.find('<') else {
        bail!("malformed author line");
    };
    let Some(gt) = author.rfind('>') else {
        bail!("malformed author line");
    };
    if gt <= lt {
        bail!("malformed author line");
    }
    let name = author[..lt].trim_end();
    let email = author[lt + 1..gt].trim();
    let after_gt = author[gt + 1..].trim_start();
    let date_tail = if after_gt.is_empty() {
        None
    } else {
        Some(after_gt.to_string())
    };
    Ok((name.to_string(), email.to_string(), date_tail))
}

/// Reject empty/malformed author identity when amending (matches Git's strictness for t7509).
fn validate_amend_source_author(author: &str) -> Result<()> {
    let (name, email, date_tail) = split_stored_author_line(author)
        .map_err(|_| anyhow::anyhow!("commit has malformed author line"))?;
    if name.is_empty() {
        bail!("empty ident name (for <author>) not allowed");
    }
    validate_ident_name(&name, "author")?;
    if email.is_empty() {
        bail!("empty ident name (for <author>) not allowed");
    }
    if date_tail.is_none() || date_tail.as_ref().is_some_and(|s| s.is_empty()) {
        bail!("empty ident name (for <author>) not allowed");
    }
    Ok(())
}

fn read_cherry_pick_head_author(repo: &Repository) -> Result<Option<String>> {
    let path = if repo.git_dir.join("CHERRY_PICK_HEAD").exists() {
        repo.git_dir.join("CHERRY_PICK_HEAD")
    } else if repo.git_dir.join("REBASE_HEAD").exists() {
        repo.git_dir.join("REBASE_HEAD")
    } else {
        return Ok(None);
    };
    let content = fs::read_to_string(&path).context("read CHERRY_PICK_HEAD")?;
    let hex = content.trim();
    if hex.is_empty() {
        return Ok(None);
    }
    let oid: ObjectId = hex
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid CHERRY_PICK_HEAD"))?;
    let obj = repo.odb.read(&oid)?;
    let commit = grit_lib::objects::parse_commit(&obj.data)?;
    Ok(Some(commit.author))
}

/// Check if an ident name is valid (not empty and not all special characters).
fn validate_ident_name(name: &str, kind: &str) -> Result<()> {
    let cleaned: String = name
        .chars()
        .filter(|&c| {
            c != '.'
                && c != ','
                && c != ';'
                && c != '<'
                && c != '>'
                && c != '\''
                && c != '"'
                && c != ' '
        })
        .collect();
    if cleaned.is_empty() {
        if name.is_empty() {
            bail!("empty ident name (for <{}>) not allowed", kind);
        } else {
            bail!("invalid ident name: '{}'", name);
        }
    }
    Ok(())
}

fn resolve_author(
    args: &Args,
    config: &ConfigSet,
    repo: &Repository,
    now: OffsetDateTime,
) -> Result<String> {
    if let Some(ref author) = args.author {
        let author = if author.contains('>') {
            author.clone()
        } else {
            find_author_by_nickname(repo, author)?
        };
        let (name, email) = parse_force_author_parameter(&author)?;
        validate_ident_name(&name, "author")?;
        let timestamp = if let Some(ref d) = args.date {
            parse_explicit_author_date(d)?
        } else if args.amend {
            amend_head_author(repo)?
                .and_then(|head_author| split_stored_author_line(&head_author).ok())
                .and_then(|(_, _, date_tail)| date_tail)
                .unwrap_or_else(|| {
                    std::env::var("GIT_AUTHOR_DATE")
                        .ok()
                        .and_then(|d| parse_date_to_git_timestamp(&d))
                        .unwrap_or_else(|| format_git_timestamp(now))
                })
        } else {
            let date_str = std::env::var("GIT_AUTHOR_DATE")
                .ok()
                .filter(|s| !s.trim().is_empty());
            match date_str {
                Some(d) => parse_date_to_git_timestamp(&d).unwrap_or(d),
                None => format_git_timestamp(now),
            }
        };
        return Ok(format!("{name} <{email}> {timestamp}"));
    }

    let reuse_rev = args.reuse_message.as_ref().or(args.reedit_message.as_ref());
    if let Some(rev) = reuse_rev {
        if !args.reset_author {
            let oid = resolve_revision_as_commit(repo, rev)?;
            let obj = repo.odb.read(&oid)?;
            let commit = grit_lib::objects::parse_commit(&obj.data)?;
            if let Some(ref d) = args.date {
                let (name, email, _) = split_stored_author_line(&commit.author)?;
                validate_ident_name(&name, "author")?;
                let timestamp = parse_explicit_author_date(d)?;
                return Ok(format!("{name} <{email}> {timestamp}"));
            }
            return Ok(commit.author);
        }
    }

    if args.amend && !args.reset_author {
        if let Some(head_author) = amend_head_author(repo)? {
            if let Some(ref d) = args.date {
                let (name, email, _) = split_stored_author_line(&head_author)?;
                validate_ident_name(&name, "author")?;
                let timestamp = parse_explicit_author_date(d)?;
                return Ok(format!("{name} <{email}> {timestamp}"));
            }
            return Ok(head_author);
        }
    }

    if !args.reset_author {
        if let Some(cp_author) = read_cherry_pick_head_author(repo)? {
            if let Some(ref d) = args.date {
                let (name, email, _) = split_stored_author_line(&cp_author)?;
                validate_ident_name(&name, "author")?;
                let timestamp = parse_explicit_author_date(d)?;
                return Ok(format!("{name} <{email}> {timestamp}"));
            }
            return Ok(cp_author);
        }
    }

    let name = resolve_name(config, IdentRole::Author)?;

    let email = resolve_email(config, IdentRole::Author)?;

    let date_str = args
        .date
        .as_deref()
        .map(String::from)
        .or_else(|| std::env::var("GIT_AUTHOR_DATE").ok())
        .filter(|s| !s.trim().is_empty());

    let timestamp = match date_str {
        Some(d) => {
            if args.date.is_some() {
                parse_explicit_author_date(&d)?
            } else {
                parse_date_to_git_timestamp(&d).unwrap_or(d)
            }
        }
        None => format_git_timestamp(now),
    };

    Ok(format!("{name} <{email}> {timestamp}"))
}

fn amend_head_author(repo: &Repository) -> Result<Option<String>> {
    let head = resolve_head(&repo.git_dir)?;
    let Some(head_oid) = head.oid() else {
        return Ok(None);
    };
    let obj = repo.odb.read(head_oid)?;
    let commit = grit_lib::objects::parse_commit(&obj.data)?;
    Ok(Some(commit.author))
}

fn parse_explicit_author_date(date: &str) -> Result<String> {
    let trimmed = date.trim();
    if is_epoch_with_tz(trimmed) {
        return Ok(trimmed.to_owned());
    }
    if let Some(ts) = parse_date_to_git_timestamp(trimmed) {
        return Ok(ts);
    }
    if !trimmed.bytes().any(|b| b.is_ascii_digit()) {
        bail!("fatal: invalid date format: {date}");
    }
    let mut err = 0;
    let ts = grit_lib::git_date::approx::approxidate_careful(trimmed, Some(&mut err));
    if err != 0 {
        bail!("fatal: invalid date format: {date}");
    }
    Ok(format!("{ts} +0000"))
}

fn is_epoch_with_tz(raw: &str) -> bool {
    let mut parts = raw.split_whitespace();
    let Some(epoch) = parts.next() else {
        return false;
    };
    let Some(tz) = parts.next() else {
        return false;
    };
    parts.next().is_none()
        && epoch.bytes().all(|b| b.is_ascii_digit())
        && parse_tz_hhmm(tz).is_some()
}

fn format_commit_summary_date(author: &str) -> Option<String> {
    let (_, _, date_tail) = split_stored_author_line(author).ok()?;
    let tail = date_tail?;
    let mut parts = tail.split_whitespace();
    let ts = parts.next()?.parse::<u64>().ok()?;
    let tz_raw = parts.next()?;
    let tz = parse_tz_hhmm(tz_raw)?;
    let mut mode = grit_lib::git_date::show::DateMode::from_type(
        grit_lib::git_date::show::DateModeType::Normal,
    );
    Some(grit_lib::git_date::show::show_date(ts, tz, &mut mode))
}

fn parse_tz_hhmm(raw: &str) -> Option<i32> {
    let bytes = raw.as_bytes();
    if bytes.len() != 5 || !matches!(bytes[0], b'+' | b'-') {
        return None;
    }
    let hours = raw[1..3].parse::<i32>().ok()?;
    let minutes = raw[3..5].parse::<i32>().ok()?;
    let sign = if bytes[0] == b'-' { -1 } else { 1 };
    Some(sign * (hours * 100 + minutes))
}

/// Resolve the committer identity from env and config.
/// Decide whether the commit being created should be GPG-signed.
///
/// Mirrors `git commit`: sign when `-S`/`--gpg-sign` is given, or when
/// `commit.gpgsign` is true; never when `--no-gpg-sign` is given.
fn should_sign_commit(args: &Args, config: &ConfigSet) -> bool {
    if args.no_gpg_sign {
        return false;
    }
    if args.gpg_sign.is_some() {
        return true;
    }
    matches!(config.get_bool("commit.gpgsign"), Some(Ok(true)))
}

/// Sign the serialized commit object and splice in a `gpgsig` header.
///
/// `key_override` is the value of `-S<keyid>` / `--gpg-sign=<keyid>` (empty when
/// `-S` was given without an argument).  The signing key falls back to
/// `user.signingkey` then to the committer identity.
fn sign_commit_bytes(
    config: &ConfigSet,
    committer: &str,
    key_override: Option<&str>,
    commit_bytes: Vec<u8>,
) -> Result<Vec<u8>> {
    let cfg = grit_lib::signing::GpgConfig::from_config(config)?;
    let committer_default = grit_lib::signing::committer_signing_default(committer);
    let signing_key = cfg.resolve_signing_key(key_override, &committer_default);
    let signature = grit_lib::signing::sign_buffer(&cfg, &commit_bytes, &signing_key)?;
    Ok(grit_lib::signing::add_header_signature(
        &commit_bytes,
        &signature,
        grit_lib::signing::GPG_SIG_HEADER_SHA1,
    ))
}

pub(crate) fn resolve_committer(config: &ConfigSet, now: OffsetDateTime) -> Result<String> {
    let name = resolve_name(config, IdentRole::Committer)?;

    let email = resolve_email(config, IdentRole::Committer)?;

    let date_str = std::env::var("GIT_COMMITTER_DATE").ok();

    Ok(grit_lib::commit::assemble_identity(
        &name,
        &email,
        date_str.as_deref(),
        now,
    ))
}

fn validate_explicit_committer_identity(config: &ConfigSet) -> Result<()> {
    let has_name = std::env::var("GIT_COMMITTER_NAME")
        .ok()
        .is_some_and(|value| !value.trim().is_empty())
        || config
            .get("committer.name")
            .or_else(|| config.get("user.name"))
            .is_some_and(|value| !value.trim().is_empty());
    let has_email = std::env::var("GIT_COMMITTER_EMAIL")
        .ok()
        .is_some_and(|value| !value.trim().is_empty())
        || std::env::var("EMAIL")
            .ok()
            .is_some_and(|value| !value.trim().is_empty())
        || config
            .get("committer.email")
            .or_else(|| config.get("user.email"))
            .is_some_and(|value| !value.trim().is_empty());
    if has_name && has_email {
        Ok(())
    } else {
        bail!("unable to auto-detect committer identity")
    }
}

/// First and optional second argument for `prepare-commit-msg` (Git `prepare_to_commit` semantics).
///
/// Mirrors `builtin/commit.c:prepare_to_commit`: `hook_arg1` defaults to `NULL` (returned here
/// as `None`, meaning the hook is invoked with only the message-file path). It becomes
/// `"message"` only when a message was supplied directly via `-m`/`-F`/`--fixup`, `"commit"`
/// for `-c`/`-C` reuse, and `"squash"`/`"merge"`/`"template"`/CHERRY_PICK for the respective
/// sources. The `-m`/`-F`/`--fixup` cases are checked first to match upstream precedence.
fn prepare_commit_msg_hook_args(
    args: &Args,
    git_dir: &Path,
) -> (Option<&'static str>, Option<String>) {
    // `-m`, `-F`/`-F -` (stdin) and `--fixup` all supply the message directly and set arg1.
    if !args.message.is_empty() || args.file.is_some() || args.fixup.is_some() {
        return (Some("message"), None);
    }

    let merge_msg = git_dir.join("MERGE_MSG");
    let squash_msg = git_dir.join("SQUASH_MSG");
    if merge_msg.exists() {
        if squash_msg.exists() {
            return (Some("squash"), None);
        }
        return (Some("merge"), None);
    }
    if squash_msg.exists() {
        return (Some("squash"), None);
    }
    if args.template.is_some() {
        return (Some("template"), None);
    }
    if git_dir.join("CHERRY_PICK_HEAD").exists() {
        return (Some("commit"), Some("CHERRY_PICK_HEAD".to_owned()));
    }
    if let Some(ref r) = args.reuse_message {
        return (Some("commit"), Some(r.clone()));
    }
    if let Some(ref r) = args.reedit_message {
        return (Some("commit"), Some(r.clone()));
    }
    // `--amend` with no other message source reuses HEAD's message: upstream sets
    // `use_message = "HEAD"` (commit.c:1353), so the hook sees arg1="commit", arg2="HEAD".
    if args.amend {
        return (Some("commit"), Some("HEAD".to_owned()));
    }
    // Plain editor commit (no message source): hook_arg1 stays NULL upstream.
    (None, None)
}

/// Run the `prepare-commit-msg` hook on `msg_file`, in place.
///
/// Mirrors `builtin/commit.c:run_commit_hook(use_editor, …, "prepare-commit-msg", …)`. The hook
/// receives the message-file path plus the source arguments from [`prepare_commit_msg_hook_args`].
/// When no editor is used, `GIT_EDITOR=:` is exported so hooks can detect non-interactive commits;
/// `GIT_INDEX_FILE` is always exported. A non-zero hook exit aborts the commit (`bail!`).
fn run_prepare_commit_msg_hook_on(
    repo: &Repository,
    args: &Args,
    index_path: &Path,
    use_editor: bool,
    msg_file: &Path,
) -> Result<()> {
    let msg_path_str = msg_file.to_string_lossy().to_string();
    let (hook_arg1, hook_arg2) = prepare_commit_msg_hook_args(args, &repo.git_dir);
    let mut hook_args: Vec<&str> = vec![msg_path_str.as_str()];
    if let Some(a1) = hook_arg1 {
        hook_args.push(a1);
        if let Some(ref a2) = hook_arg2 {
            hook_args.push(a2.as_str());
        }
    }
    let prepare_hook_env = CommitHookEnv {
        index_file: Some(index_path),
        git_editor: if use_editor { None } else { Some(":") },
        git_prefix: None,
        extra_env: &[],
    };
    let r = run_commit_hook(
        repo,
        "prepare-commit-msg",
        &hook_args,
        None,
        &prepare_hook_env,
    )
    .map_err(|e| anyhow::anyhow!(e))?;
    if let HookResult::Failed(code) = r {
        bail!("prepare-commit-msg hook exited with status {code}");
    }
    Ok(())
}

/// Update HEAD to point to the new commit.
fn update_head(git_dir: &Path, head: &HeadState, commit_oid: &ObjectId) -> Result<()> {
    match head {
        HeadState::Branch { refname, .. } => {
            write_ref(git_dir, refname, commit_oid)?;
        }
        HeadState::Detached { .. } | HeadState::Invalid => {
            // Write directly to HEAD
            fs::write(git_dir.join("HEAD"), format!("{}\n", commit_oid.to_hex()))?;
        }
    }
    Ok(())
}

fn ensure_head_unchanged(git_dir: &Path, expected: &HeadState) -> Result<()> {
    let current = resolve_head(git_dir)?;
    let unchanged = match (expected, &current) {
        (
            HeadState::Branch {
                refname: expected_ref,
                oid: expected_oid,
                ..
            },
            HeadState::Branch {
                refname: current_ref,
                oid: current_oid,
                ..
            },
        ) => expected_ref == current_ref && expected_oid == current_oid,
        (HeadState::Detached { oid: expected_oid }, HeadState::Detached { oid: current_oid }) => {
            expected_oid == current_oid
        }
        (HeadState::Invalid, HeadState::Invalid) => true,
        _ => false,
    };
    if unchanged {
        return Ok(());
    }
    bail!("cannot lock ref 'HEAD': is at a different commit than expected");
}

/// Extract the trailing `# Conflicts:` comment block from `MERGE_MSG` for re-attaching to
/// COMMIT_EDITMSG (git keeps it in the editor file when finishing a conflicted pick/revert).
///
/// Returns the block as `\n# Conflicts:\n#\t<path>...\n` (leading blank line included) using the
/// repo's comment prefix, or `None` when no such block is present.
fn conflicts_comment_block(git_dir: &Path, config: &ConfigSet) -> Option<String> {
    let merge_msg = fs::read_to_string(git_dir.join("MERGE_MSG")).ok()?;
    let cp = comment_line_prefix_full(config);
    let header = format!("{cp} Conflicts:");
    let start = merge_msg.lines().position(|l| l.trim_end() == header)?;
    let mut block = String::from("\n");
    for line in merge_msg.lines().skip(start) {
        block.push_str(line);
        block.push('\n');
    }
    Some(block)
}

/// Whether the sequencer todo has at most one remaining line (git's `have_finished_the_last_pick`).
///
/// Returns `true` when `sequencer/todo` is missing or contains only the final pick line, signalling
/// that a plain `git commit` finishing the conflict resolution also completed the whole sequence.
fn sequencer_finished_last_pick(git_dir: &Path) -> bool {
    let todo_path = git_dir.join("sequencer").join("todo");
    let Ok(content) = fs::read_to_string(&todo_path) else {
        // Missing todo => not in a sequence; nothing to remove (git returns 0 here).
        return false;
    };
    // git checks for a second line: only one line (or a trailing-newline-only remainder) => done.
    match content.find('\n') {
        None => true,
        Some(eol) => content[eol + 1..].is_empty(),
    }
}

/// Whether the revert sequencer todo has no remaining un-attempted reverts.
///
/// grit's revert sequencer records only the commits *after* the current one in
/// `sequencer/todo` (the in-progress revert is tracked via `REVERT_HEAD`). The
/// whole revert sequence is therefore complete only when that todo holds no more
/// `revert` lines. Returns `true` when the todo is missing or contains only blank
/// or comment lines.
fn sequencer_todo_remaining_empty(git_dir: &Path) -> bool {
    let todo_path = git_dir.join("sequencer").join("todo");
    let Ok(content) = fs::read_to_string(&todo_path) else {
        return true;
    };
    !content.lines().any(|line| {
        let t = line.trim();
        !t.is_empty() && !t.starts_with('#')
    })
}

/// Clean up merge-related state files after a successful commit.
fn cleanup_merge_state(git_dir: &Path) {
    let _ = fs::remove_file(git_dir.join("MERGE_HEAD"));
    let _ = fs::remove_file(git_dir.join("MERGE_MSG"));
    let _ = fs::remove_file(git_dir.join("MERGE_MODE"));
    let _ = fs::remove_file(git_dir.join("SQUASH_MSG"));
    let _ = fs::remove_file(git_dir.join("CHERRY_PICK_HEAD"));
    let _ = fs::remove_file(git_dir.join("REVERT_HEAD"));
}

/// Ensure a string ends with a newline.
fn ensure_trailing_newline(s: &str) -> String {
    if s.is_empty() || s.ends_with('\n') {
        s.to_owned()
    } else {
        format!("{s}\n")
    }
}

fn is_permission_denied_error(err: &grit_lib::error::Error) -> bool {
    err.to_string().contains("Permission denied") || err.to_string().contains("permission denied")
}
