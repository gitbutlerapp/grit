//! `grit` — a small, opinionated command line interface backed by `grit-lib`.
//!
//! `grit` deliberately does not mirror Git's UX. It favors a single obvious way
//! to do the common thing, plain-language output, and a status screen that
//! doubles as the home base: running `grit` with no arguments shows you where you
//! are, what's changed, and what to do next.

mod cli_messages;
mod commands;
mod commits_json;
mod conflict_style;
mod context;
mod dates;
mod diagnostics;
mod diff_args;
mod diff_format;
mod diff_render;
mod grit_trace;
mod json_error;
mod json_filter;
#[cfg(test)]
mod json_schema;
mod log_display;
mod markdown;
mod operation_output;
mod output;
mod pathspec_args;
mod progress;
mod ref_name_messages;
mod rev_select;
mod revision_errors;
mod stdio;
mod terminal_progress;
mod ui;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use clap_complete::Shell;

use output::{emit, OutputMode, OutputOptions, RenderContext};

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum LogGroupByArg {
    Author,
}

/// A small, opinionated Git client built on `grit-lib`.
#[derive(Debug, Parser)]
#[command(
    name = "grit",
    version,
    about = "A simple Grit-powered CLI",
    disable_version_flag = true
)]
pub(crate) struct Cli {
    /// Print version.
    #[arg(short = 'v', short_alias = 'V', long, action = clap::ArgAction::Version)]
    version: (),
    /// Emit machine-readable JSON instead of human-readable text.
    #[arg(long, global = true, conflicts_with = "markdown")]
    json: bool,
    /// Emit agent-friendly Markdown instead of human-readable text.
    #[arg(long, global = true, conflicts_with = "json")]
    markdown: bool,
    /// jq-like expression applied to JSON output (requires `--json`).
    ///
    /// Examples: `.branch`, `.commits[].oid`, `{branch, clean}`.
    #[arg(long, global = true, value_name = "EXPR")]
    filter: Option<String>,
    /// Worker threads for parallel library work (`0` = auto, `1` = serial).
    #[arg(long, global = true, value_name = "N")]
    threads: Option<usize>,
    #[command(subcommand)]
    command: Option<Command>,
}

/// Subcommands of `grit remote`.
#[derive(Debug, Subcommand)]
enum RemoteAction {
    /// Add a new remote.
    Add {
        /// Short name for the remote (e.g. origin).
        name: String,
        /// The remote's URL or path.
        url: String,
    },
    /// List references on a remote (like `git ls-remote`).
    Refs {
        /// Configured remote name or a literal URL/path.
        remote_or_url: String,
        /// Only show refs under `refs/heads/`.
        #[arg(long)]
        heads: bool,
        /// Only show refs under `refs/tags/`.
        #[arg(long)]
        tags: bool,
        /// Sort refs by `name` or `version` (`git ls-remote --sort=version:refname`).
        #[arg(long = "sort", value_name = "KEY", default_value = "name")]
        sort: RemoteRefSortArg,
        /// Reverse sort order (`git ls-remote --sort=-…`).
        #[arg(long)]
        reverse: bool,
        /// Return at most this many refs after sorting.
        #[arg(short = 'n', long = "limit", value_name = "N")]
        limit: Option<usize>,
        /// Optional ref prefixes (same rules as `git ls-remote`).
        #[arg(value_name = "PREFIX")]
        prefixes: Vec<String>,
    },
}

/// Sort key for `grit remote refs`.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub(crate) enum RemoteRefSortArg {
    /// Full ref name order.
    Name,
    /// Numeric `version:refname` order.
    Version,
}

/// Subcommands of `grit reflog`.
#[derive(Debug, Subcommand)]
enum ReflogAction {
    /// Remove one or more reflog entries (`ref@{n}`).
    Delete {
        /// Reflog selectors (`main@{1}`, `HEAD@{0}`, …).
        #[arg(required = true, value_name = "REF@{N}")]
        specs: Vec<String>,
    },
    /// Drop reflog entries older than configured or explicit cutoffs.
    Expire {
        /// Process every ref under `refs/`, not only refs that already have log files.
        #[arg(long)]
        all: bool,
        /// Expire reachable entries older than this (Git `--expire` syntax).
        #[arg(long = "expire", value_name = "DATE")]
        expire: Option<String>,
        /// Expire unreachable entries older than this.
        #[arg(long = "expire-unreachable", value_name = "DATE")]
        expire_unreachable: Option<String>,
    },
}

/// Subcommands of `grit gc`.
#[derive(Debug, Subcommand)]
enum GcAction {
    /// Repack reachable objects into one pack.
    Repack {
        /// Write a reachability `.bitmap` sidecar for the new pack.
        #[arg(long)]
        bitmaps: bool,
    },
    /// Write a reachable commit-graph with changed-path Bloom filters.
    #[command(name = "commit-graph")]
    CommitGraph,
    /// Write or refresh the multi-pack-index.
    Midx,
    /// Pack loose refs into `packed-refs` (or compact reftable).
    #[command(name = "pack-refs")]
    PackRefs,
    /// Delete unreachable loose objects.
    Prune {
        /// Only remove objects older than this date (Git approxidate syntax).
        #[arg(long = "older-than", value_name = "DATE")]
        older_than: Option<String>,
    },
}

/// Subcommands of `grit ignore`.
#[derive(Debug, Subcommand)]
enum IgnoreAction {
    /// Print whether paths match exclude rules.
    Check {
        /// Paths to query (repository-relative or from the current directory).
        #[arg(value_name = "PATH")]
        paths: Vec<String>,
        /// Also list paths that are not ignored.
        #[arg(long)]
        all: bool,
    },
}
/// Subcommands of `grit stash`.
#[derive(Debug, Subcommand)]
enum StashAction {
    /// Save local changes to the stash and reset the working tree.
    Push {
        /// Stash message.
        #[arg(short = 'm', long = "message")]
        message: Option<String>,
        /// Include untracked files in the stash.
        #[arg(short = 'u', long = "include-untracked")]
        include_untracked: bool,
    },
    /// List stash entries.
    List,
    /// Show the summary or patch for a stash entry.
    Show {
        /// Stash index (`0` is newest) or `stash@{n}`.
        index: Option<String>,
        /// Show the full patch instead of a diffstat.
        #[arg(short = 'p', long = "patch")]
        patch: bool,
    },
    /// Apply a stash entry without removing it.
    Apply {
        /// Stash index (`0` is newest) or `stash@{n}`.
        index: Option<String>,
    },
    /// Apply a stash entry and remove it when there are no conflicts.
    Pop {
        /// Stash index (`0` is newest) or `stash@{n}`.
        index: Option<String>,
    },
    /// Remove a stash entry without applying it.
    Drop {
        /// Stash index (`0` is newest) or `stash@{n}`.
        index: Option<String>,
    },
}

/// Subcommands of `grit history`.
#[derive(Debug, Subcommand)]
enum HistoryAction {
    /// Remove a commit and replay descendants onto its parent.
    Drop {
        /// Commit to remove (revision spec).
        commit: String,
        #[command(flatten)]
        shared: commands::history::HistoryRewriteArgs,
    },
    /// Reorder a commit on the first-parent chain of `HEAD`.
    Move {
        /// Commit to move (revision spec).
        commit: String,
        /// Place the commit immediately before this revision.
        #[arg(long = "before", conflicts_with = "after")]
        before: Option<String>,
        /// Place the commit immediately after this revision.
        #[arg(long = "after", conflicts_with = "before")]
        after: Option<String>,
        #[command(flatten)]
        shared: commands::history::HistoryRewriteArgs,
    },
    /// Replace a commit message and replay descendants onto the new history.
    Reword {
        /// Commit to reword (revision spec).
        commit: String,
        /// New commit message.
        #[arg(short = 'm', long = "message", conflicts_with = "message_file")]
        message: Option<String>,
        /// Read the new message from a file (`-` for stdin).
        #[arg(
            long = "message-file",
            value_name = "PATH|-",
            conflicts_with = "message"
        )]
        message_file: Option<String>,
        #[command(flatten)]
        shared: commands::history::HistoryRewriteArgs,
    },
    /// Apply staged changes onto a commit and replay descendants.
    Fixup {
        /// Commit to fix up (revision spec).
        commit: String,
        /// Replace the target commit message.
        #[arg(short = 'm', long = "message", conflicts_with = "message_file")]
        message: Option<String>,
        /// Read the replacement message from a file (`-` for stdin).
        #[arg(
            long = "message-file",
            value_name = "PATH|-",
            conflicts_with = "message"
        )]
        message_file: Option<String>,
        /// When a commit would become empty after the fixup.
        #[arg(long = "empty", value_enum, default_value_t = commands::history::EmptyPolicyArg::Drop)]
        empty: commands::history::EmptyPolicyArg,
        #[command(flatten)]
        shared: commands::history::HistoryRewriteArgs,
    },
    /// Assign staged hunks into recent commits and replay descendants once.
    Absorb {
        /// Oldest commit boundary (exclusive) for candidate commits.
        #[arg(long = "base")]
        base: Option<String>,
        #[command(flatten)]
        shared: commands::history::HistoryRewriteArgs,
    },
    /// Fold a revision range into the oldest commit and replay descendants.
    Squash {
        /// Revision range (same syntax as `git rev-list`, for example `base..tip`).
        #[arg(required = true)]
        range: Vec<String>,
        /// Message for the squashed commit (default: keep the oldest commit's message).
        #[arg(short = 'm', long = "message", conflicts_with_all = ["message_file", "combine"])]
        message: Option<String>,
        /// Read the squashed commit message from a file (`-` for stdin).
        #[arg(
            long = "message-file",
            value_name = "PATH|-",
            conflicts_with_all = ["message", "combine"]
        )]
        message_file: Option<String>,
        /// Build a combined autosquash-style message from every commit in the range.
        #[arg(long = "combine", conflicts_with_all = ["message", "message_file"])]
        combine: bool,
        #[command(flatten)]
        shared: commands::history::HistoryRewriteArgs,
    },
}

/// Subcommands of `grit notes`.
#[derive(Debug, Subcommand)]
enum NotesAction {
    /// List notes attached to commits.
    List,
    /// Show the note for a revision.
    Show {
        /// Commit (or other object) to show the note for.
        rev: String,
    },
    /// Add or replace a note on a revision.
    Add {
        /// Revision to attach the note to.
        rev: String,
        /// Note text.
        #[arg(short = 'm', long = "message")]
        message: String,
        /// Replace an existing note.
        #[arg(long)]
        force: bool,
    },
    /// Remove notes from one or more revisions.
    Remove {
        /// Revisions whose notes should be removed.
        #[arg(value_name = "REV")]
        revs: Vec<String>,
    },
}

/// Subcommands of `grit worktree`.
#[derive(Debug, Subcommand)]
enum WorktreeAction {
    /// List registered worktrees (main first, then linked).
    List,
    /// Create a linked worktree at `path`.
    Add {
        /// Directory for the new working tree.
        path: std::path::PathBuf,
        /// Commit or branch to check out (defaults to `HEAD`).
        #[arg(value_name = "REV")]
        start_point: Option<String>,
        /// Create and check out a new branch.
        #[arg(short = 'b', long = "branch", value_name = "NAME")]
        branch: Option<String>,
        /// Check out in detached HEAD mode.
        #[arg(long = "detach")]
        detach: bool,
        /// Reuse a stale registration when the working tree directory is missing.
        #[arg(short, long)]
        force: bool,
    },
    /// Remove a linked worktree.
    Remove {
        /// Path to the worktree to remove.
        path: std::path::PathBuf,
        /// Remove even when the worktree has local changes or is locked.
        #[arg(short, long)]
        force: bool,
    },
    /// Drop registrations for linked worktrees whose directories are gone.
    Prune,
    /// Mark a linked worktree as locked (prevents accidental removal).
    Lock {
        /// Path to the linked worktree.
        path: std::path::PathBuf,
        /// Optional reason stored in the lock file.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Clear the lock on a linked worktree.
    Unlock {
        /// Path to the linked worktree.
        path: std::path::PathBuf,
    },
}

/// Subcommands of `grit bundle`.
#[derive(Debug, Subcommand)]
enum BundleAction {
    /// Write a bundle file from revision specs.
    Create {
        /// Output bundle path.
        file: String,
        /// Commits or ref ranges to include (same rules as `grit log`).
        #[arg(value_name = "REV")]
        revs: Vec<String>,
    },
    /// Check that a bundle file is well-formed.
    Verify {
        /// Bundle file to verify.
        file: String,
    },
    /// List refs recorded in a bundle header.
    List {
        /// Bundle file to read.
        file: String,
    },
    /// Ingest a bundle pack into the current repository.
    Unbundle {
        /// Bundle file to read.
        file: String,
    },
}

/// Subcommands of `grit hooks`.
#[derive(Debug, Subcommand)]
enum HooksAction {
    /// List hook scripts in the resolved hooks directory.
    List,
    /// Run a hook by name (passes `--` arguments through to the script).
    Run {
        /// Hook basename (e.g. `pre-commit`).
        name: String,
        /// Arguments forwarded to the hook after `--`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        hook_args: Vec<String>,
    },
}

/// Subcommands of `grit auth`.
#[derive(Debug, Subcommand)]
enum AuthAction {
    /// Forget the stored GitHub token (sign out).
    Logout,
}

/// Top-level `grit` commands.
#[derive(Debug, Subcommand)]
enum Command {
    /// Create a new, empty repository.
    Init {
        /// Where to create the repository (defaults to the current directory).
        path: Option<String>,
        /// Create a bare repository (no working tree).
        #[arg(long)]
        bare: bool,
        /// Ref storage backend for the new repository.
        #[arg(long, value_name = "FORMAT", default_value = "files")]
        ref_format: grit_lib::refs::ref_storage::RefStorageFormat,
    },
    /// Copy a remote repository into a new directory.
    Clone {
        /// The repository to clone (URL or local path).
        url: String,
        /// Directory to clone into (defaults to the repository name).
        dir: Option<String>,
        /// Fetch only the tip commit on each branch (same as `git clone --depth=1`).
        #[arg(long)]
        shallow: bool,
    },
    /// List remotes, or add one.
    Remote {
        #[command(subcommand)]
        action: Option<RemoteAction>,
    },
    /// Show recent commits reachable from HEAD.
    Log {
        #[command(flatten)]
        rev: rev_select::RevSelectArgs,
        /// Continue a paged walk using the opaque `next` token from a previous page.
        #[arg(long = "cursor", value_name = "TOKEN")]
        cursor: Option<String>,
        #[command(flatten)]
        format: diff_format::DiffFormatArgs,
        /// One line per commit: short oid and subject (refs when `--decorate`).
        #[arg(long)]
        oneline: bool,
        /// Show branch and tag names that point at each commit.
        #[arg(long)]
        decorate: bool,
        /// Aggregate commits (`author` matches `git shortlog -sne` ordering).
        #[arg(long = "group-by", value_enum, value_name = "FIELD")]
        group_by: Option<LogGroupByArg>,
        /// With `--group-by`, omit per-commit lists in `--json` output.
        #[arg(long)]
        count: bool,
        /// Draw an ASCII commit graph in human output (implies topological order).
        #[arg(long)]
        graph: bool,
        /// Use Unicode box-drawing characters in `--graph` output (human only).
        #[arg(long = "graph-unicode")]
        graph_unicode: bool,
    },
    /// List or count commits matching revision ranges.
    Revs {
        #[command(flatten)]
        rev: rev_select::RevSelectArgs,
        /// Print only the number of reachable commits.
        #[arg(long)]
        count: bool,
        /// With `--count`, print left/right counts for a symmetric range (`A...B`).
        #[arg(long = "left-right")]
        left_right: bool,
    },
    /// Find merge bases or test whether one commit is an ancestor of another.
    #[command(name = "merge-base")]
    MergeBase {
        /// First commit or ref.
        a: String,
        /// Second commit or ref.
        b: String,
        /// List every minimal merge base (criss-cross histories).
        #[arg(long)]
        all: bool,
        /// Exit 0 when `a` is an ancestor of `b`, 1 otherwise; prints nothing in human mode.
        #[arg(long = "is-ancestor")]
        is_ancestor: bool,
    },
    /// Show, delete, or expire reflog entries.
    Reflog {
        /// Ref to show (defaults to `HEAD`).
        #[arg(value_name = "REF")]
        reference: Option<String>,
        /// Show at most this many entries (newest first).
        #[arg(long)]
        limit: Option<usize>,
        #[command(subcommand)]
        action: Option<ReflogAction>,
    },
    /// Show who last modified each line of a file.
    Blame {
        /// Path to the file (repository-relative).
        file: String,
        /// Blame at this revision instead of `HEAD`.
        #[arg(long)]
        rev: Option<String>,
        /// Limit to an inclusive 1-based line range (`START,END`).
        #[arg(short = 'L', long = "lines", value_name = "START,END")]
        line_range: Option<String>,
        /// At merge commits, follow only the first parent (Git's `--first-parent`).
        #[arg(long = "first-parent")]
        first_parent: bool,
    },
    /// Show changes as a diff. No argument: uncommitted changes; with a commit:
    /// the change that commit introduced; with two commits (or `A..B`) diff their trees.
    Diff {
        /// Compare two filesystem paths outside any repository (like `git diff --no-index`).
        #[arg(long = "no-index")]
        no_index: bool,
        /// Staged changes only (index vs HEAD).
        #[arg(short = 'S', long = "staged", visible_aliases = ["cached"])]
        staged: bool,
        /// Unstaged changes only (worktree vs index).
        #[arg(long = "unstaged")]
        unstaged: bool,
        /// Compare the worktree to this ref's tree instead of HEAD (no commit arguments).
        #[arg(long = "against", value_name = "REF")]
        against: Option<String>,
        /// Exit with 1 when there are changes (errors exit 2).
        #[arg(long = "exit-code")]
        exit_code: bool,
        /// Print nothing; implies `--exit-code`.
        #[arg(long = "quiet")]
        quiet: bool,
        /// One commit (that commit's patch), two commits (tree diff), or a range `A..B` / `A...B`.
        #[arg(value_name = "COMMIT")]
        commits: Vec<String>,
        /// Limit the diff to these paths (after `--`, like `grit log`).
        #[arg(value_name = "PATH", last = true, allow_hyphen_values = true)]
        paths: Vec<String>,
        #[command(flatten)]
        format: diff_format::DiffFormatArgs,
        #[command(flatten)]
        line_format: diff_args::DiffCompareArgs,
    },
    /// Show information about a commit, tag, or branch (defaults to HEAD).
    Show {
        /// Commits, tags, branches, or `rev:path` specs to show.
        #[arg(value_name = "OBJECT")]
        objects: Vec<String>,
        /// Merge commit diffs: `combined` (default) or `first-parent`.
        #[arg(long = "diff-merges", value_name = "MODE", default_value = "combined")]
        diff_merges: String,
        /// Limit the diff to these paths (after `--`).
        #[arg(value_name = "PATH", last = true)]
        paths: Vec<String>,
        #[command(flatten)]
        format: diff_format::DiffFormatArgs,
    },
    /// Show what's changed and where you are (this is the default).
    #[command(alias = "st")]
    Status {
        #[command(flatten)]
        status: commands::status::StatusCliArgs,
    },
    /// List the commits on this branch that aren't on the target branch yet.
    #[command(alias = "sl")]
    Shortlog,
    /// Stage changes. With no paths, stages everything.
    Add {
        #[command(flatten)]
        pathspecs: pathspec_args::PathspecArgs,
        /// Stage tracked changes only (modified and deleted paths; no untracked files).
        #[arg(short = 'u', long = "update")]
        update: bool,
        /// Report paths that would be staged without writing the index or object database.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Record intent to add (empty index entries) instead of staging file content.
        #[arg(short = 'N', long = "intent-to-add")]
        intent_to_add: bool,
        /// Stage paths that would otherwise be rejected as ignored.
        #[arg(short = 'f', long = "force")]
        force: bool,
        /// Mark staged regular files as executable (`100755`) in the index.
        #[arg(long = "executable", conflicts_with = "non_executable")]
        executable: bool,
        /// Mark staged regular files as non-executable (`100644`) in the index.
        #[arg(long = "non-executable", conflicts_with = "executable")]
        non_executable: bool,
        /// Apply a unified diff to the index (`git apply --cached`). Use `-` for stdin.
        #[arg(long = "patch-file", value_name = "FILE|-")]
        patch_file: Option<String>,
        /// Stage only the listed hunk ids from `grit diff --json` (comma-separated, repeatable).
        #[arg(long = "hunks", value_name = "ID,...", action = clap::ArgAction::Append)]
        hunks: Vec<String>,
    },
    /// Remove tracked paths from the working tree and index.
    Rm {
        #[command(flatten)]
        pathspecs: pathspec_args::PathspecArgs,
        /// Remove from the index only; keep files in the working tree.
        #[arg(long)]
        cached: bool,
        /// Remove even when paths have local modifications.
        #[arg(short = 'f', long = "force")]
        force: bool,
    },
    /// Rename or move tracked paths.
    Mv {
        /// Source path(s) and final destination (last argument).
        #[arg(value_name = "PATH")]
        paths: Vec<String>,
        /// Replace existing files at the destination.
        #[arg(short = 'f', long = "force")]
        force: bool,
    },
    /// Remove untracked files from the working tree.
    Clean {
        #[command(flatten)]
        pathspecs: pathspec_args::PathspecArgs,
        /// Actually delete files. Without this flag, only prints what would be removed.
        #[arg(short, long)]
        force: bool,
        /// Remove ignored files as well (like `git clean -x`).
        #[arg(long)]
        ignored: bool,
    },
    /// Pack reachable objects and remove redundant packs.
    Repack {
        /// Show what would be packed without writing.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Suppress progress summary on stderr.
        #[arg(short, long)]
        quiet: bool,
    },
    /// Pack refs, expire reflogs, repack, prune, and refresh commit-graph.
    Gc {
        /// Show what would run without writing.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Suppress progress summary on stderr.
        #[arg(short, long)]
        quiet: bool,
        #[command(subcommand)]
        action: Option<GcAction>,
    },
    /// Check refs, objects, and pack indexes for problems.
    #[command(alias = "verify")]
    Fsck {
        /// Check connectivity from refs only (skip store-wide object validation).
        #[arg(long = "connectivity-only")]
        connectivity_only: bool,
        /// Report unreachable objects (with `--connectivity-only`, off unless this flag is set).
        #[arg(long)]
        dangling: bool,
        /// Do not list dangling objects (matches Git `--no-dangling`).
        #[arg(long = "no-dangling")]
        no_dangling: bool,
        /// Treat ref warnings as errors.
        #[arg(long)]
        strict: bool,
        /// Include info-level object diagnostics in human output (matches Git `--verbose`).
        #[arg(short, long)]
        verbose: bool,
    },
    /// Query exclude (gitignore) rules.
    Ignore {
        #[command(subcommand)]
        action: IgnoreAction,
    },
    /// List hook scripts or run one by name.
    Hooks {
        #[command(subcommand)]
        action: Option<HooksAction>,
    },
    /// Print stable patch-IDs for one or more commits.
    #[command(name = "patch-id")]
    PatchId {
        /// Commits or revisions to hash (merge commits are skipped).
        #[arg(value_name = "REV")]
        revs: Vec<String>,
        /// Use legacy unstable patch-ID algorithm (Git 1.9 and older).
        #[arg(long)]
        unstable: bool,
    },
    /// Resolve revision specs to object ids and types.
    Resolve {
        /// Revision specs to resolve (same forms as history commands).
        #[arg(value_name = "SPEC", required = true)]
        specs: Vec<String>,
        /// Print abbreviated object ids.
        #[arg(long)]
        short: bool,
    },
    /// Show blob, tree, commit, or tag contents for a revision spec.
    Cat {
        /// Object or revision spec (`HEAD`, `main:path`, `v1^{commit}`, …).
        spec: String,
    },
    /// Stage every change and record a new commit.
    Commit {
        /// Commit message (you can also pass it with -m).
        #[arg(conflicts_with = "message_file")]
        message: Option<String>,
        /// Commit message.
        #[arg(short = 'm', long = "message", conflicts_with_all = ["message", "message_file"])]
        message_flag: Option<String>,
        /// Read the commit message from a file (`-` for stdin).
        #[arg(short = 'F', long = "message-file", conflicts_with_all = ["message", "message_flag"])]
        message_file: Option<String>,
        /// Author for this commit (`Name <email>`); committer is unchanged.
        #[arg(long = "author")]
        author_override: Option<String>,
        /// Override the author date (Git date formats, RFC 3339, `@epoch`, …).
        #[arg(long = "date")]
        author_date: Option<String>,
        /// Append a commit trailer (`key=value` or `key: value`); repeat for multiple trailers.
        #[arg(long = "trailer", value_name = "KEY=VALUE")]
        trailers: Vec<String>,
        /// Append a `Signed-off-by` trailer for the committer.
        #[arg(short = 's', long = "signoff")]
        signoff: bool,
        /// Set the author to the committer with a fresh author date.
        #[arg(long = "reset-author")]
        reset_author: bool,
        /// Reuse the log message and author from a commit (`-C` / `--reuse-message`).
        #[arg(short = 'C', long = "reuse-message", value_name = "COMMIT")]
        reuse_message: Option<String>,
        /// Sign the commit with the configured signing key.
        #[arg(short = 'S', long = "gpg-sign")]
        gpg_sign: bool,
        /// Do not sign this commit, even when `commit.gpgsign` is enabled.
        #[arg(long = "no-gpg-sign", conflicts_with = "gpg_sign")]
        no_gpg_sign: bool,
        /// Skip the `pre-commit` and `commit-msg` hooks.
        #[arg(long = "no-verify")]
        no_verify: bool,
        /// Stage every change first, then commit. This is the default when `commit.autoStage` is true.
        #[arg(
            short = 'a',
            long = "all",
            conflicts_with_all = ["staged", "paths", "pathspec_from_file"]
        )]
        all: bool,
        /// Commit only what is already staged (same as `git commit` without `-a`).
        #[arg(
            long = "staged",
            conflicts_with_all = ["all", "paths", "pathspec_from_file"]
        )]
        staged: bool,
        /// Replace the tip commit instead of creating a new one.
        #[arg(long = "amend")]
        amend: bool,
        /// Record a commit even when the tree matches the parent (same as `git commit --allow-empty`).
        #[arg(long = "allow-empty")]
        allow_empty: bool,
        /// Compute the commit without writing objects, updating refs, or running hooks.
        #[arg(long = "dry-run")]
        dry_run: bool,
        #[command(flatten)]
        paths: crate::pathspec_args::TrailingPathspecArgs,
    },
    /// List branches, or create / delete / rename / configure one.
    Branch {
        /// Branch to create, delete, rename, or configure upstream for. Omit to list.
        name: Option<String>,
        /// Start commit when creating (`grit branch <name> <start>`).
        start: Option<String>,
        /// Overwrite an existing branch when creating or renaming.
        #[arg(short = 'f', long = "force", conflicts_with = "force_rename")]
        force: bool,
        /// Configure upstream from the start point when creating.
        #[arg(short = 't', long = "track", conflicts_with = "no_track")]
        track: bool,
        /// Do not configure upstream when creating.
        #[arg(long = "no-track", conflicts_with = "track")]
        no_track: bool,
        /// Set upstream for the named branch (or the current branch when omitted).
        #[arg(short = 'u', long = "set-upstream-to", value_name = "UPSTREAM")]
        set_upstream_to: Option<String>,
        /// Remove upstream configuration for the named branch (or the current branch).
        #[arg(long = "unset-upstream")]
        unset_upstream: bool,
        /// Rename a branch: `-m <new>` or `-m <old> <new>`.
        #[arg(
            short = 'm',
            long = "move",
            value_name = "NAME",
            num_args = 1..=2,
            conflicts_with = "force_rename"
        )]
        rename: Option<Vec<String>>,
        /// Force rename: `-M <new>` or `-M <old> <new>`.
        #[arg(short = 'M', value_name = "NAME", num_args = 1..=2, conflicts_with = "rename")]
        force_rename: Option<Vec<String>>,
        /// Delete the named branch instead of creating it.
        #[arg(short = 'd', long = "delete")]
        delete: bool,
        /// Delete even when the branch is not fully merged into the current branch.
        #[arg(short = 'D', long = "force-delete")]
        force_delete: bool,
        /// Preview the operation without updating refs, config, or reflogs.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Switch to another branch, remote branch, or detached commit.
    #[command(alias = "checkout", alias = "co")]
    Switch {
        /// Branch to switch to, revision with `--detach`, or new branch name with `-c`.
        #[arg(value_name = "NAME", required_unless_present = "detach")]
        branch: Option<String>,
        /// With `-c`, the commit to start the branch from (defaults to `HEAD`).
        start_point: Option<String>,
        /// Create the branch first, then switch to it.
        #[arg(short = 'c', long = "create")]
        create: bool,
        /// Check out a commit in detached HEAD mode.
        #[arg(long = "detach")]
        detach: bool,
        /// Run overwrite checks without updating refs, the index, or the worktree.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Move the branch tip and optionally reset the index or worktree.
    Reset {
        /// Commit to reset to (defaults to `HEAD`).
        #[arg(value_name = "REV")]
        rev: Option<String>,
        /// Keep the index and worktree; move the branch tip only.
        #[arg(long, conflicts_with_all = ["hard", "mixed"])]
        soft: bool,
        /// Reset the index to the target commit; keep the worktree (default).
        #[arg(long, conflicts_with_all = ["hard", "soft"])]
        mixed: bool,
        /// Reset the index and worktree to match the target commit.
        #[arg(long, conflicts_with_all = ["soft", "mixed"])]
        hard: bool,
    },
    /// Merge another branch into the current one.
    Merge {
        /// Branch to merge in.
        #[arg(
            required_unless_present_any = ["abort", "continue_merge"],
            conflicts_with = "continue_merge"
        )]
        branch: Option<String>,
        /// Cancel an in-progress merge and restore the pre-merge state.
        #[arg(long = "abort", conflicts_with_all = ["continue_merge", "check"])]
        abort: bool,
        /// Finish an in-progress merge after resolving conflicts (or after `--no-commit`).
        #[arg(long = "continue", conflicts_with_all = ["abort", "check"])]
        continue_merge: bool,
        /// Commit message (only with `--continue`; overrides `MERGE_MSG`).
        #[arg(short = 'm', long = "message", requires = "continue_merge")]
        message: Option<String>,
        /// Predict whether the merge would succeed without changing refs, index, or worktree.
        #[arg(long = "check", conflicts_with_all = ["abort", "continue_merge"])]
        check: bool,
    },
    /// Cherry-pick commits onto the current branch.
    Pick {
        /// Commits to pick (any revision spec — full / short oid, branch, HEAD~2, …).
        #[arg(
            required_unless_present_any = ["continue_pick", "abort", "skip"],
            value_name = "COMMIT"
        )]
        commits: Vec<String>,
        /// Parent number when picking a merge commit (1-based, long only).
        #[arg(long = "mainline", value_name = "N")]
        mainline: Option<u32>,
        /// Finish a pick stopped on conflicts or an empty commit.
        #[arg(long = "continue", conflicts_with_all = ["abort", "skip"])]
        continue_pick: bool,
        /// Cancel an in-progress pick and restore the pre-pick state.
        #[arg(long = "abort", conflicts_with_all = ["continue_pick", "skip"])]
        abort: bool,
        /// Skip the current pick step and continue with the next commit.
        #[arg(long = "skip", conflicts_with_all = ["continue_pick", "abort"])]
        skip: bool,
        /// Apply the pick to the index only; do not create a commit yet.
        #[arg(long = "no-commit")]
        no_commit: bool,
        /// When a pick step would not change the tree: stop (default), keep an empty commit, or drop the step.
        #[arg(long = "empty", value_enum, default_value_t = commands::pick::ReplayEmptyPolicyArg::Stop)]
        empty: commands::pick::ReplayEmptyPolicyArg,
        /// Replace the pick message (single commit only).
        #[arg(short = 'm', long = "message", conflicts_with = "message_file")]
        message: Option<String>,
        /// Read the pick message from a file (single commit only).
        #[arg(long = "message-file", conflicts_with = "message")]
        message_file: Option<String>,
        /// List commits that would be picked without changing refs or the worktree.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Undo commits with new inverse commits.
    Revert {
        /// Commits or revision ranges to revert (newest first within each range).
        #[arg(
            required_unless_present_any = ["continue_revert", "abort", "skip"],
            value_name = "COMMIT"
        )]
        commits: Vec<String>,
        /// Parent number when reverting a merge commit (1-based, long only).
        #[arg(long = "mainline", value_name = "N")]
        mainline: Option<u32>,
        /// Finish a revert stopped on conflicts or an empty commit.
        #[arg(long = "continue", conflicts_with_all = ["abort", "skip"])]
        continue_revert: bool,
        /// Cancel an in-progress revert and restore the pre-revert state.
        #[arg(long = "abort", conflicts_with_all = ["continue_revert", "skip"])]
        abort: bool,
        /// Skip the current revert step and continue with the next commit.
        #[arg(long = "skip", conflicts_with_all = ["continue_revert", "abort"])]
        skip: bool,
        /// Apply the revert to the index only; do not create a commit yet.
        #[arg(long = "no-commit")]
        no_commit: bool,
        /// When a revert step would not change the tree: stop (default), keep an empty commit, or drop the step.
        #[arg(long = "empty", value_enum, default_value_t = commands::revert::ReplayEmptyPolicyArg::Stop)]
        empty: commands::revert::ReplayEmptyPolicyArg,
        /// Replace the revert message (single commit only).
        #[arg(short = 'm', long = "message", conflicts_with = "message_file")]
        message: Option<String>,
        /// Read the revert message from a file (single commit only).
        #[arg(long = "message-file", conflicts_with = "message")]
        message_file: Option<String>,
        /// List commits that would be reverted without changing refs or the worktree.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Rewrite commits without a sequencer (plan, then apply refs atomically).
    History {
        #[command(subcommand)]
        action: HistoryAction,
    },
    /// Save or restore shelved work.
    Stash {
        /// Stash message (bare `grit stash` / `grit stash push`).
        #[arg(short = 'm', long = "message", global = true)]
        message: Option<String>,
        /// Include untracked files (bare `grit stash` / `grit stash push`).
        #[arg(short = 'u', long = "include-untracked", global = true)]
        include_untracked: bool,
        #[command(subcommand)]
        action: Option<StashAction>,
    },
    /// Restore working tree and/or index paths.
    Restore {
        #[command(flatten)]
        pathspecs: pathspec_args::PathspecArgs,
        /// Restore the index from HEAD (or `--source`).
        #[arg(short = 'S', long = "staged")]
        staged: bool,
        /// Restore the working tree (default when `--staged` is absent).
        #[arg(short = 'W', long = "worktree")]
        worktree: bool,
        /// Tree or commit to restore from.
        #[arg(long)]
        source: Option<String>,
        /// For unmerged paths, write our side (index stage 2) into the worktree.
        #[arg(long = "ours", conflicts_with_all = ["theirs", "merge", "conflict_style"])]
        ours: bool,
        /// For unmerged paths, write their side (index stage 3) into the worktree.
        #[arg(long = "theirs", conflicts_with_all = ["ours", "merge", "conflict_style"])]
        theirs: bool,
        /// Rebuild conflict markers in the worktree from index stages.
        #[arg(long = "merge", conflicts_with_all = ["ours", "theirs"])]
        merge: bool,
        /// Conflict marker style (implies `--merge`).
        #[arg(long = "conflict", value_enum, conflicts_with_all = ["ours", "theirs"])]
        conflict_style: Option<conflict_style::ConflictStyleArg>,
    },
    /// Download refs and objects from a remote.
    Fetch {
        #[command(flatten)]
        fetch: commands::fetch_args::FetchArgs,
    },
    /// Fetch from the remote and integrate it into the current branch.
    Pull {
        #[command(flatten)]
        fetch: commands::fetch_args::FetchArgs,
        /// Refuse when a merge would be required (exit 1 when diverged).
        #[arg(long = "ff-only", conflicts_with_all = ["no_rebase", "rebase", "check"])]
        ff_only: bool,
        /// Merge instead of rebasing, overriding `pull.rebase`.
        #[arg(long = "no-rebase", conflicts_with_all = ["ff_only", "rebase", "check"])]
        no_rebase: bool,
        /// Rebase local commits onto the fetched upstream, overriding `pull.rebase=false`.
        #[arg(long = "rebase", conflicts_with_all = ["ff_only", "no_rebase", "check"])]
        rebase: bool,
        /// Stash dirty worktree/index changes around the pull (`merge.autoStash` / `rebase.autoStash`).
        #[arg(long = "autostash", conflicts_with = "check")]
        autostash: bool,
        /// Fetch and report whether a fast-forward is possible without changing the branch.
        #[arg(long = "check", conflicts_with_all = ["autostash", "ff_only", "no_rebase", "rebase"])]
        check: bool,
    },
    /// Publish refs to a remote.
    Push {
        /// Push tags instead of the current branch. Pushes every local tag
        /// under `refs/tags/` to the remote.
        #[arg(short = 't', long = "tags")]
        tags: bool,
        /// Delete a remote branch or tag (may be repeated).
        #[arg(long = "delete", value_name = "REF")]
        delete: Vec<String>,
        /// Remote to push to (defaults to the current branch's remote or `origin`).
        #[arg(long, hide = true)]
        remote: Option<String>,
        /// Contact the remote but do not update refs or send objects.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Overwrite remote refs even when the update is not a fast-forward.
        #[arg(short = 'f', long = "force", conflicts_with = "force_with_lease")]
        force: bool,
        /// Push only when the remote ref matches the local remote-tracking ref, or
        /// `=<rev>` when pushing exactly one ref.
        #[arg(
            long = "force-with-lease",
            value_name = "REV",
            num_args = 0..=1,
            require_equals = true,
            conflicts_with = "force"
        )]
        force_with_lease: Option<Option<String>>,
        /// Set `branch.<name>.remote` and `branch.<name>.merge` for each pushed branch.
        #[arg(short = 'u', long = "set-upstream")]
        set_upstream: bool,
        /// All ref updates succeed or none are applied.
        #[arg(long = "atomic")]
        atomic: bool,
        /// Server-side push option (repeatable; `GIT_PUSH_OPTION_*` in hooks).
        #[arg(short = 'o', long = "push-option", value_name = "OPTION")]
        push_option: Vec<String>,
        /// Push annotated tags reachable from the commits being pushed.
        #[arg(long = "follow-tags")]
        follow_tags: bool,
        /// Remote name (when not using `--remote`).
        #[arg(value_name = "REMOTE")]
        remote_name: Option<String>,
        /// Refspec (`src`, `src:dst`, `:dst`, …).
        #[arg(value_name = "REFSPEC")]
        refspecs: Vec<String>,
    },
    /// Create, verify, or list git bundle files.
    Bundle {
        #[command(subcommand)]
        action: BundleAction,
    },
    /// List, show, add, or remove Git notes on commits.
    Notes {
        /// Notes ref (default: `core.notesRef` or `refs/notes/commits`).
        #[arg(long = "ref", value_name = "REF", global = true)]
        notes_ref: Option<String>,
        #[command(subcommand)]
        action: Option<NotesAction>,
    },
    /// List tags, or create / delete one.
    Tag {
        /// Name of the tag to create. Omit to list tags.
        #[arg(value_name = "NAME", conflicts_with = "verify")]
        name: Option<String>,
        /// Revision to tag (default `HEAD`).
        #[arg(value_name = "TARGET", conflicts_with_all = ["object", "verify"])]
        target: Option<String>,
        /// Delete the named tag instead of creating it.
        #[arg(short = 'd', long = "delete")]
        delete: bool,
        /// Annotated tag message (`-m` creates an annotated tag).
        #[arg(short = 'm', long = "message", conflicts_with = "message_file")]
        message: Option<String>,
        /// Read the annotated tag message from a file (`-` reads stdin).
        #[arg(
            short = 'F',
            long = "file",
            value_name = "PATH|-",
            conflicts_with = "message"
        )]
        message_file: Option<String>,
        /// Object to tag (revision). Defaults to `HEAD`.
        #[arg(
            long = "object",
            value_name = "REV",
            hide = true,
            conflicts_with = "target"
        )]
        object: Option<String>,
        /// Replace an existing tag with the same name.
        #[arg(short = 'f', long = "force", conflicts_with = "delete")]
        force: bool,
        /// Preview create or delete without changing refs.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Sign the tag object (implies annotated). Honors `tag.gpgSign` when omitted.
        #[arg(short = 's', long = "sign", conflicts_with = "no_sign")]
        sign: bool,
        /// Do not sign even when `tag.gpgSign` is true.
        #[arg(long = "no-sign", conflicts_with = "sign")]
        no_sign: bool,
        /// Signing key id or path (overrides `user.signingkey` for this tag).
        #[arg(long = "signing-key", value_name = "KEY")]
        signing_key: Option<String>,
        /// Verify annotated tag signatures and optional `tree-sha256` claims.
        #[arg(
            short = 'v',
            long = "verify",
            conflicts_with_all = [
                "delete",
                "message",
                "message_file",
                "force",
                "dry_run",
                "sign",
                "no_sign",
                "signing_key",
                "object"
            ]
        )]
        verify: bool,
        /// Tag names to verify (one or more, with `--verify`).
        #[arg(
            id = "verify_names",
            value_name = "TAG",
            num_args = 1..,
            requires = "verify",
            last = true
        )]
        verify_names: Vec<String>,
        /// Skip signature verification (still checks `tree-sha256` when present).
        #[arg(long = "no-signature", requires = "verify")]
        no_signature: bool,
    },
    /// List, add, or remove linked worktrees.
    Worktree {
        #[command(subcommand)]
        action: WorktreeAction,
    },
    /// Sign in to GitHub (device flow) and store a token for HTTPS push/fetch.
    Auth {
        #[command(subcommand)]
        action: Option<AuthAction>,
    },
    /// Update grit to the latest release (re-runs the install script).
    Update,
    /// Print an agent skill (SKILL.md) that explains how to use grit.
    Skill,
    /// Generate shell completion scripts for bash, zsh, or fish.
    Completions {
        /// Target shell.
        shell: Shell,
    },
    /// Read, set, or list configuration values.
    Config {
        /// Use the global (per-user) config file instead of this repository's.
        #[arg(long)]
        global: bool,
        /// List all configuration values.
        #[arg(short = 'l', long = "list")]
        list: bool,
        /// Remove the key instead of reading or setting it.
        #[arg(long)]
        unset: bool,
        /// The configuration key, e.g. user.name. Omit only with --list.
        key: Option<String>,
        /// The value to set. Omit to read the current value.
        value: Option<String>,
    },
    /// Git credential helper backed by the Windows Credential Manager.
    ///
    /// You don't normally run this yourself — `grit auth` wires it into
    /// `credential.helper` on Windows. It speaks Git's credential protocol.
    Manager {
        /// The credential operation: get, store, or erase.
        operation: String,
    },
    /// Serve a fetch or clone over stdin/stdout (run by ssh and grit-http-server).
    #[command(name = "upload-pack", hide = true)]
    UploadPack {
        #[command(flatten)]
        args: ServeArgs,
    },
    /// Accept a push over stdin/stdout (run by ssh and grit-http-server).
    #[command(name = "receive-pack", hide = true)]
    ReceivePack {
        #[command(flatten)]
        args: ServeArgs,
    },
}

/// Arguments shared by the `upload-pack` and `receive-pack` plumbing commands.
#[derive(Debug, clap::Args)]
struct ServeArgs {
    /// Answer a single request without advertising refs first (smart HTTP).
    #[arg(long)]
    stateless_rpc: bool,
    /// Print the ref advertisement and exit (smart HTTP discovery).
    #[arg(long, alias = "http-backend-info-refs")]
    advertise_refs: bool,
    /// The repository to serve.
    directory: String,
}

/// True when this token is a `tag` subcommand flag that consumes the next argv word.
///
/// Keep this list in sync with value-taking [`Cli`] global options (`--filter`, `--threads`).
fn tag_verify_subarg_takes_value(arg: &str) -> bool {
    matches!(arg, "--filter" | "--threads")
}

/// Route verify tag names to `verify_names` by inserting `--` before bare arguments.
fn normalize_tag_verify_argv(mut args: Vec<String>) -> Vec<String> {
    let Some(tag_idx) = args.iter().position(|a| a == "tag") else {
        return args;
    };
    let sub = &args[tag_idx + 1..];
    if !sub.iter().any(|a| a == "--verify" || a == "-v") {
        return args;
    }
    if sub.contains(&"--".to_owned()) {
        return args;
    }
    let mut flags = Vec::new();
    let mut names = Vec::new();
    let mut i = 0;
    while i < sub.len() {
        let arg = &sub[i];
        if tag_verify_subarg_takes_value(arg) {
            flags.push(arg.clone());
            if i + 1 < sub.len() {
                flags.push(sub[i + 1].clone());
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if arg.starts_with('-') {
            flags.push(arg.clone());
            i += 1;
            continue;
        }
        names.push(arg.clone());
        i += 1;
    }
    if names.is_empty() {
        return args;
    }
    args.truncate(tag_idx + 1);
    args.extend(flags);
    args.push("--".into());
    args.extend(names);
    args
}

fn main() {
    stdio::configure();
    grit_trace::maybe_init_from_env();
    let parse_argv =
        normalize_tag_verify_argv(diff_args::normalize_diff_argv(std::env::args().collect()));
    let cli = match Cli::try_parse_from(parse_argv) {
        Ok(cli) => cli,
        Err(err) if err.use_stderr() => {
            let json = std::env::args().any(|a| a == "--json");
            if json {
                let opts = OutputOptions {
                    mode: OutputMode::Json,
                    filter: extract_filter_flag(),
                };
                let message = clap_json_error_message(&err);
                output::emit_error(&anyhow::anyhow!(message), &opts);
                std::process::exit(err.exit_code());
            }
            err.exit();
        }
        Err(err) => err.exit(),
    };
    let opts = OutputOptions {
        mode: if cli.json {
            OutputMode::Json
        } else if cli.markdown {
            OutputMode::Markdown
        } else {
            OutputMode::Human
        },
        filter: cli.filter.clone(),
    };
    context::set_thread_policy(cli.threads);
    if let Err(err) = opts.validate() {
        if opts.mode == OutputMode::Json {
            output::emit_error(&err, &opts);
        } else {
            eprintln!("error: {err:#}");
        }
        std::process::exit(1);
    }
    if let Err(err) = reject_non_human_plumbing(&cli, &opts) {
        output::emit_error(&err, &opts);
        std::process::exit(1);
    }
    let env = context::environment();
    let render_ctx = RenderContext {
        now: context::wall_clock_now(&env),
    };
    if let Err(err) = dispatch(cli, &opts, &render_ctx) {
        let err = json_error::ensure_structured(err);
        output::emit_error(&err, &opts);
        let code = if err
            .downcast_ref::<commands::bundle::MissingPrerequisites>()
            .is_some()
        {
            commands::bundle::MISSING_PREREQUISITE_EXIT
        } else if err
            .downcast_ref::<grit_lib::remote::clone::CloneError>()
            .is_some_and(grit_lib::remote::clone::CloneError::is_remote_read_failure)
        {
            128
        } else if crate::json_error::structured_json_payload(&err)
            .is_some_and(|p| p.kind == "invalid_pattern")
        {
            crate::json_error::INVALID_COMMIT_FILTER_EXIT
        } else if commands::branch::is_branch_usage_error(&err)
            || crate::json_error::structured_json_payload(&err)
                .is_some_and(|payload| payload.kind == "usage")
        {
            2
        } else {
            1
        };
        std::process::exit(code);
    }
}

/// Format a clap usage error for `--json`: keep missing-argument detail, omit Usage/help.
fn clap_json_error_message(err: &clap::Error) -> String {
    let raw = err.to_string();
    let mut lines = raw.lines();
    let Some(first) = lines.next() else {
        return "invalid arguments".to_owned();
    };
    let first = first
        .strip_prefix("error: ")
        .or_else(|| first.strip_prefix("error:"))
        .unwrap_or(first)
        .trim();
    let mut out = vec![first.to_owned()];
    for line in lines {
        let trimmed = line.trim();
        if trimmed.starts_with("Usage:") || trimmed.starts_with("For more information") {
            break;
        }
        if trimmed.is_empty() {
            if out.len() > 1 {
                break;
            }
            continue;
        }
        out.push(line.to_owned());
    }
    let message = out.join("\n").trim().to_owned();
    if message.is_empty() {
        "invalid arguments".to_owned()
    } else {
        message
    }
}

fn extract_filter_flag() -> Option<String> {
    let mut args = std::env::args();
    while let Some(arg) = args.next() {
        if arg == "--filter" {
            return args.next();
        }
        if let Some(expr) = arg.strip_prefix("--filter=") {
            return Some(expr.to_owned());
        }
    }
    None
}

/// Run the selected subcommand and render its outcome.
///
/// Each command computes a typed, serializable outcome; [`emit`] renders it as
/// human text or a single JSON object. The exceptions are `completions` (a raw
/// completion script on stdout), `manager` (a raw credential-helper protocol on
/// stdin/stdout — no outcome), `upload-pack` and `receive-pack` (the Git wire
/// protocol on stdin/stdout — no outcome), and
/// `push` (which emits its per-ref outcome and then exits non-zero when a ref
/// was rejected).
/// Plumbing commands whose stdout is not a command outcome object.
fn reject_non_human_plumbing(cli: &Cli, opts: &OutputOptions) -> Result<()> {
    if opts.mode == OutputMode::Human {
        return Ok(());
    }
    let Some(cmd) = &cli.command else {
        return Ok(());
    };
    let name = match cmd {
        Command::Completions { .. } => "completions",
        Command::Manager { .. } => "manager",
        Command::UploadPack { .. } => "upload-pack",
        Command::ReceivePack { .. } => "receive-pack",
        _ => return Ok(()),
    };
    bail!("`grit {name}` does not support --json or --markdown");
}

fn dispatch(cli: Cli, opts: &OutputOptions, ctx: &RenderContext) -> Result<()> {
    match cli.command.unwrap_or(Command::Status {
        status: commands::status::StatusCliArgs::default(),
    }) {
        Command::Init {
            path,
            bare,
            ref_format,
        } => emit(&commands::init::run(path, bare, ref_format)?, opts, ctx),
        Command::Clone { url, dir, shallow } => emit(
            &commands::clone::run(&url, dir, shallow, opts.mode)?,
            opts,
            ctx,
        ),
        Command::Remote { action } => match action {
            None => emit(&commands::remote::run_list_or_add(None)?, opts, ctx),
            Some(RemoteAction::Add { name, url }) => emit(
                &commands::remote::run_list_or_add(Some((name, url)))?,
                opts,
                ctx,
            ),
            Some(RemoteAction::Refs {
                remote_or_url,
                heads,
                tags,
                sort,
                reverse,
                limit,
                prefixes,
            }) => {
                let refs_cli = commands::remote::RemoteRefsCliOptions {
                    heads,
                    tags,
                    sort,
                    reverse,
                    limit,
                    prefixes,
                };
                emit(
                    &commands::remote::run_refs(&remote_or_url, &refs_cli, opts.mode)?,
                    opts,
                    ctx,
                )
            }
        },
        Command::Log {
            rev,
            cursor,
            format,
            oneline,
            decorate,
            group_by,
            count,
            graph,
            graph_unicode,
        } => {
            if count && group_by.is_none() {
                bail!("`--count` requires `--group-by`");
            }
            let display = commands::log::LogDisplayOptions {
                oneline,
                decorate,
                group_by: group_by.map(|g| match g {
                    LogGroupByArg::Author => commands::log::LogGroupBy::Author,
                }),
                group_count_only: count,
                graph,
                graph_unicode,
            };
            emit(
                &commands::log::run(rev, cursor, format, display, opts.mode)?,
                opts,
                ctx,
            )
        }
        Command::Revs {
            rev,
            count,
            left_right,
        } => emit(&commands::revs::run(rev, count, left_right)?, opts, ctx),
        Command::MergeBase {
            a,
            b,
            all,
            is_ancestor,
        } => {
            let outcome = commands::merge_base_cmd::run(&a, &b, all, is_ancestor)?;
            let code = commands::merge_base_cmd::exit_code(&outcome);
            emit(&outcome, opts, ctx)?;
            if is_ancestor && code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Command::Reflog {
            reference,
            limit,
            action,
        } => match action {
            None => emit(&commands::reflog::run_show(reference, limit)?, opts, ctx),
            Some(ReflogAction::Delete { specs }) => {
                emit(&commands::reflog::run_delete(specs)?, opts, ctx)
            }
            Some(ReflogAction::Expire {
                all,
                expire,
                expire_unreachable,
            }) => emit(
                &commands::reflog::run_expire(all, expire, expire_unreachable)?,
                opts,
                ctx,
            ),
        },
        Command::Blame {
            file,
            rev,
            line_range,
            first_parent,
        } => emit(
            &commands::blame::run(file, rev, line_range, first_parent, ctx.now)?,
            opts,
            ctx,
        ),
        Command::Diff {
            staged,
            unstaged,
            against,
            no_index,
            exit_code,
            quiet,
            commits,
            paths,
            format,
            line_format,
        } => {
            if format.patch_text && opts.mode != OutputMode::Human {
                bail!("`--patch-text` cannot be used with --json or --markdown");
            }
            let wants_exit_code = exit_code || quiet || no_index;
            let run_opts = commands::diff::DiffRunOptions {
                no_index,
                staged,
                unstaged,
                against,
                commits,
                paths,
                format,
                line_format,
                word_level_inline: matches!(opts.mode, crate::output::OutputMode::Human)
                    && commands::diff::diff_word_level_inline_for_output(),
                quiet,
            };
            let run_result = commands::diff::run(run_opts);
            match run_result {
                Ok(outcome) => {
                    if !quiet && !format.patch_text {
                        emit(&outcome, opts, ctx)?;
                    }
                    if no_index || wants_exit_code {
                        std::process::exit(commands::diff::exit_code(&outcome));
                    }
                    Ok(())
                }
                Err(err) => {
                    output::emit_error(&err, opts);
                    std::process::exit(if no_index || wants_exit_code {
                        commands::diff::DIFF_ERROR_EXIT
                    } else {
                        1
                    });
                }
            }
        }
        Command::Show {
            objects,
            diff_merges,
            paths,
            format,
        } => {
            let diff_merges =
                grit_lib::diff::DiffMerges::parse(&diff_merges).map_err(|e| anyhow::anyhow!(e))?;
            emit(
                &commands::show::run(commands::show::ShowRunOptions {
                    objects,
                    paths,
                    format,
                    word_level_inline: matches!(opts.mode, crate::output::OutputMode::Human)
                        && commands::diff::diff_word_level_inline_for_output(),
                    diff_merges,
                })?,
                opts,
                ctx,
            )
        }
        Command::Status { status } => emit(&commands::status::run(&status)?, opts, ctx),
        Command::Shortlog => emit(&commands::shortlog::run()?, opts, ctx),
        Command::Add {
            pathspecs,
            update,
            dry_run,
            intent_to_add,
            force,
            executable,
            non_executable,
            patch_file,
            hunks,
        } => {
            if let Some(file) = patch_file {
                if !hunks.is_empty() {
                    bail!("--patch-file cannot be used with --hunks");
                }
                if executable || non_executable {
                    bail!("--patch-file cannot be used with --executable or --non-executable");
                }
                if force {
                    bail!("--patch-file cannot be used with --force");
                }
                if update {
                    bail!("--patch-file cannot be used with --update");
                }
                if intent_to_add {
                    bail!("--patch-file cannot be used with --intent-to-add");
                }
                if !pathspecs.paths.is_empty() || pathspecs.pathspec_from_file.is_some() {
                    bail!("--patch-file cannot be used with path arguments");
                }
                emit(&commands::add::run_patch_file(&file, dry_run)?, opts, ctx)
            } else if !hunks.is_empty() {
                if executable || non_executable {
                    bail!("--hunks cannot be used with --executable or --non-executable");
                }
                if force {
                    bail!("--hunks cannot be used with --force");
                }
                if update {
                    bail!("--hunks cannot be used with --update");
                }
                if intent_to_add {
                    bail!("--hunks cannot be used with --intent-to-add");
                }
                match commands::add::run_hunks(&pathspecs, &hunks, dry_run) {
                    Ok(outcome) => emit(&outcome, opts, ctx),
                    Err(err) => {
                        let code = commands::add::hunks_exit_code(&err);
                        if opts.mode == OutputMode::Json {
                            output::emit_error(&err, opts);
                        } else {
                            eprintln!("error: {err:#}");
                        }
                        std::process::exit(code);
                    }
                }
            } else {
                emit(
                    &commands::add::run(
                        &pathspecs,
                        update,
                        dry_run,
                        intent_to_add,
                        force,
                        executable,
                        non_executable,
                    )?,
                    opts,
                    ctx,
                )
            }
        }
        Command::Rm {
            pathspecs,
            cached,
            force,
        } => emit(&commands::rm::run(&pathspecs, cached, force)?, opts, ctx),
        Command::Mv { paths, force } => emit(&commands::mv::run(&paths, force)?, opts, ctx),
        Command::Clean {
            pathspecs,
            force,
            ignored,
        } => emit(
            &commands::clean::run(&pathspecs, force, ignored)?,
            opts,
            ctx,
        ),
        Command::Repack { dry_run, quiet } => {
            emit(&commands::repack::run(dry_run, quiet)?, opts, ctx)
        }
        Command::Gc {
            dry_run,
            quiet,
            action,
        } => {
            let mode = opts.mode;
            let outcome = match action {
                None => commands::gc::run(dry_run, quiet, mode)?,
                Some(GcAction::Repack { bitmaps }) => {
                    commands::gc::run_repack(bitmaps, dry_run, quiet, mode)?
                }
                Some(GcAction::CommitGraph) => commands::gc::run_commit_graph(quiet, mode)?,
                Some(GcAction::Midx) => commands::gc::run_midx(quiet, mode)?,
                Some(GcAction::PackRefs) => commands::gc::run_pack_refs(quiet, mode)?,
                Some(GcAction::Prune { older_than }) => {
                    commands::gc::run_prune(older_than.as_deref(), quiet, mode)?
                }
            };
            emit(&outcome, opts, ctx)
        }
        Command::Fsck {
            connectivity_only,
            dangling,
            no_dangling,
            strict,
            verbose,
        } => {
            let outcome = commands::fsck::run(commands::fsck::FsckRunOptions {
                connectivity_only,
                dangling,
                no_dangling,
                strict,
                verbose,
                mode: opts.mode,
            })?;
            let code = commands::fsck::exit_code(&outcome);
            emit(&outcome, opts, ctx)?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Command::Ignore { action } => match action {
            IgnoreAction::Check { paths, all } => {
                let outcome = commands::ignore::run(&paths, all)?;
                let code = commands::ignore::exit_code(&outcome);
                emit(&outcome, opts, ctx)?;
                if code != 0 {
                    std::process::exit(code);
                }
                Ok(())
            }
        },
        Command::Hooks { action } => match action {
            None | Some(HooksAction::List) => emit(&commands::hooks::run_list()?, opts, ctx),
            Some(HooksAction::Run { name, hook_args }) => {
                let outcome = commands::hooks::run_hook_by_name(&name, &hook_args)?;
                let code = commands::hooks::run_exit_code(&outcome);
                emit(&outcome, opts, ctx)?;
                if code != 0 {
                    std::process::exit(code);
                }
                Ok(())
            }
        },
        Command::PatchId { revs, unstable } => {
            emit(&commands::patch_id::run(&revs, unstable)?, opts, ctx)
        }
        Command::Resolve { specs, short } => {
            emit(&commands::resolve::run(specs, short)?, opts, ctx)
        }
        Command::Cat { spec } => emit(&commands::cat::run(spec)?, opts, ctx),
        Command::Commit {
            message,
            message_flag,
            message_file,
            author_override,
            author_date,
            trailers,
            signoff,
            reset_author,
            reuse_message,
            gpg_sign,
            no_gpg_sign,
            no_verify,
            all,
            staged,
            amend,
            allow_empty,
            dry_run,
            paths,
        } => emit(
            &commands::commit::run(
                message.or(message_flag),
                message_file,
                author_override,
                author_date,
                trailers,
                signoff,
                reset_author,
                reuse_message,
                gpg_sign,
                no_gpg_sign,
                no_verify,
                dry_run,
                amend,
                allow_empty,
                all,
                staged,
                paths,
            )?,
            opts,
            ctx,
        ),
        Command::Branch {
            name,
            start,
            force,
            track,
            no_track,
            set_upstream_to,
            unset_upstream,
            rename,
            force_rename,
            delete,
            force_delete,
            dry_run,
        } => emit(
            &commands::branch::run(commands::branch::BranchRunOptions {
                name,
                start,
                force,
                track,
                no_track,
                set_upstream_to,
                unset_upstream,
                rename,
                force_rename,
                delete,
                force_delete,
                dry_run,
            })?,
            opts,
            ctx,
        ),
        Command::Bundle { action } => match action {
            BundleAction::Create { file, revs } => {
                emit(&commands::bundle::run_create(file.into(), revs)?, opts, ctx)
            }
            BundleAction::Verify { file } => {
                let outcome = commands::bundle::run_verify(file.into())?;
                emit(&outcome, opts, ctx)?;
                if !outcome.ok {
                    std::process::exit(1);
                }
                Ok(())
            }
            BundleAction::List { file } => {
                emit(&commands::bundle::run_list(file.into())?, opts, ctx)
            }
            BundleAction::Unbundle { file } => {
                emit(&commands::bundle::run_unbundle(file.into())?, opts, ctx)
            }
        },
        Command::Notes { notes_ref, action } => {
            use commands::notes::NotesRunAction::*;
            let run_action = match action {
                None | Some(NotesAction::List) => List,
                Some(NotesAction::Show { rev }) => Show { rev },
                Some(NotesAction::Add {
                    rev,
                    message,
                    force,
                }) => Add {
                    rev,
                    message,
                    force,
                },
                Some(NotesAction::Remove { revs }) => Remove { revs },
            };
            emit(
                &commands::notes::run(notes_ref, run_action, ctx.now)?,
                opts,
                ctx,
            )
        }
        Command::Tag {
            name,
            target,
            delete,
            message,
            message_file,
            object,
            force,
            dry_run,
            sign,
            no_sign,
            signing_key,
            verify,
            verify_names,
            no_signature,
        } => {
            let sign_override = if sign {
                Some(true)
            } else if no_sign {
                Some(false)
            } else {
                None
            };
            let outcome = commands::tag::run(commands::tag::TagRunOptions {
                name,
                target,
                object,
                delete,
                message,
                message_file,
                force,
                dry_run,
                sign: sign_override,
                signing_key,
                now: ctx.now,
                verify,
                verify_names,
                no_signature,
            })?;
            emit(&outcome, opts, ctx)?;
            if verify {
                std::process::exit(commands::tag::verify_exit_code(&outcome));
            }
            Ok(())
        }
        Command::Worktree { action } => match action {
            WorktreeAction::List => emit(&commands::worktree::run_list()?, opts, ctx),
            WorktreeAction::Add {
                path,
                start_point,
                branch,
                detach,
                force,
            } => {
                let new_branch = branch.is_some();
                emit(
                    &commands::worktree::run_add(
                        path,
                        start_point,
                        branch,
                        new_branch,
                        detach,
                        force,
                    )?,
                    opts,
                    ctx,
                )
            }
            WorktreeAction::Remove { path, force } => {
                emit(&commands::worktree::run_remove(path, force)?, opts, ctx)
            }
            WorktreeAction::Prune => emit(&commands::worktree::run_prune()?, opts, ctx),
            WorktreeAction::Lock { path, reason } => {
                emit(&commands::worktree::run_lock(path, reason)?, opts, ctx)
            }
            WorktreeAction::Unlock { path } => {
                emit(&commands::worktree::run_unlock(path)?, opts, ctx)
            }
        },
        Command::Switch {
            branch,
            start_point,
            create,
            detach,
            dry_run,
        } => emit(
            &commands::switch::run(branch, start_point, create, detach, dry_run)?,
            opts,
            ctx,
        ),
        Command::Reset {
            rev,
            soft,
            mixed,
            hard,
        } => emit(&commands::reset::run(rev, soft, mixed, hard)?, opts, ctx),
        Command::Merge {
            branch,
            abort,
            continue_merge,
            message,
            check,
        } => {
            let outcome = commands::merge::run(
                branch.as_deref(),
                abort,
                continue_merge,
                message.as_deref(),
                check,
            )?;
            emit(&outcome, opts, ctx)?;
            if outcome.result == commands::merge::MergeResult::Conflicts {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Pick {
            commits,
            mainline,
            continue_pick,
            abort,
            skip,
            no_commit,
            empty,
            message,
            message_file,
            dry_run,
        } => {
            let outcome = commands::pick::run(&commands::pick::PickRunOptions {
                commits,
                mainline,
                continue_pick,
                abort,
                skip,
                no_commit,
                empty,
                message,
                message_file,
                dry_run,
            })?;
            if matches!(
                outcome.result,
                commands::pick::PickResult::Conflicts | commands::pick::PickResult::Empty
            ) {
                emit(&outcome, opts, ctx)?;
                std::process::exit(1);
            }
            emit(&outcome, opts, ctx)
        }
        Command::Revert {
            commits,
            mainline,
            continue_revert,
            abort,
            skip,
            no_commit,
            empty,
            message,
            message_file,
            dry_run,
        } => {
            let outcome = commands::revert::run(&commands::revert::RevertRunOptions {
                commits,
                mainline,
                continue_revert,
                abort,
                skip,
                no_commit,
                empty,
                message,
                message_file,
                dry_run,
            })?;
            if matches!(
                outcome.result,
                commands::revert::RevertResult::Conflicts | commands::revert::RevertResult::Empty
            ) {
                emit(&outcome, opts, ctx)?;
                std::process::exit(1);
            }
            emit(&outcome, opts, ctx)
        }
        Command::History { action } => match action {
            HistoryAction::Drop { commit, shared } => {
                emit(&commands::history::run_drop(&commit, &shared)?, opts, ctx)
            }
            HistoryAction::Move {
                commit,
                before,
                after,
                shared,
            } => emit(
                &commands::history::run_move(&commit, before, after, &shared)?,
                opts,
                ctx,
            ),
            HistoryAction::Reword {
                commit,
                message,
                message_file,
                shared,
            } => emit(
                &commands::history::run_reword(&commit, message, message_file, &shared)?,
                opts,
                ctx,
            ),
            HistoryAction::Fixup {
                commit,
                message,
                message_file,
                empty,
                shared,
            } => emit(
                &commands::history::run_fixup(&commit, message, message_file, empty, &shared)?,
                opts,
                ctx,
            ),
            HistoryAction::Absorb { base, shared } => emit(
                &commands::history::run_absorb(base.as_deref(), &shared)?,
                opts,
                ctx,
            ),
            HistoryAction::Squash {
                range,
                message,
                message_file,
                combine,
                shared,
            } => emit(
                &commands::history::run_squash(&range, message, message_file, combine, &shared)?,
                opts,
                ctx,
            ),
        },
        Command::Stash {
            action,
            message,
            include_untracked,
        } => match action {
            None => emit(
                &commands::stash::run_push(message, include_untracked)?,
                opts,
                ctx,
            ),
            Some(StashAction::Push {
                message: push_message,
                include_untracked: push_untracked,
            }) => emit(
                &commands::stash::run_push(
                    push_message.or(message),
                    include_untracked || push_untracked,
                )?,
                opts,
                ctx,
            ),
            Some(StashAction::List) => emit(&commands::stash::run_list()?, opts, ctx),
            Some(StashAction::Show { index, patch }) => {
                emit(&commands::stash::run_show(index, patch)?, opts, ctx)
            }
            Some(StashAction::Apply { index }) => {
                let outcome = commands::stash::run_apply(index)?;
                emit(&outcome, opts, ctx)?;
                if outcome.conflicts {
                    std::process::exit(1);
                }
                Ok(())
            }
            Some(StashAction::Pop { index }) => {
                let outcome = commands::stash::run_pop(index)?;
                emit(&outcome, opts, ctx)?;
                if outcome.conflicts {
                    std::process::exit(1);
                }
                Ok(())
            }
            Some(StashAction::Drop { index }) => {
                emit(&commands::stash::run_drop(index)?, opts, ctx)
            }
        },
        Command::Restore {
            pathspecs,
            staged,
            worktree,
            source,
            ours,
            theirs,
            merge,
            conflict_style,
        } => {
            use grit_lib::porcelain::restore::ConflictSide;
            let side = if ours {
                Some(ConflictSide::Ours)
            } else if theirs {
                Some(ConflictSide::Theirs)
            } else {
                None
            };
            emit(
                &commands::restore::run(
                    &pathspecs,
                    staged,
                    worktree,
                    source,
                    side,
                    merge,
                    conflict_style,
                )?,
                opts,
                ctx,
            )
        }
        Command::Fetch { fetch } => {
            let outcome = commands::fetch::run(&fetch, opts.mode)?;
            if !(fetch.quiet && opts.mode == OutputMode::Human) {
                match &outcome {
                    commands::fetch::FetchRunOutcome::One(one) => emit(one, opts, ctx)?,
                    commands::fetch::FetchRunOutcome::All(all) => emit(all, opts, ctx)?,
                }
            }
            if outcome.exit_nonzero() {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Pull {
            fetch,
            ff_only,
            no_rebase,
            rebase,
            autostash,
            check,
        } => {
            let args = commands::pull::PullRunArgs {
                fetch,
                ff_only,
                no_rebase,
                rebase,
                autostash,
                check,
            };
            let outcome = commands::pull::run(&args, opts.mode)?;
            if !(args.fetch.quiet && opts.mode == OutputMode::Human) {
                emit(&outcome, opts, ctx)?;
            }
            let code = commands::pull::pull_check_exit_code(&outcome);
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Command::Push {
            tags,
            delete,
            remote,
            dry_run,
            force,
            force_with_lease,
            set_upstream,
            atomic,
            push_option,
            follow_tags,
            remote_name,
            refspecs,
        } => {
            let mut positionals = Vec::new();
            if let Some(r) = remote_name {
                positionals.push(r);
            }
            positionals.extend(refspecs);
            let outcome = commands::push::run(
                tags,
                &delete,
                remote,
                &positionals,
                dry_run,
                force,
                force_with_lease,
                set_upstream,
                atomic,
                &push_option,
                follow_tags,
            )?;
            emit(&outcome, opts, ctx)?;
            // The outcome (per-ref results) is reported in both modes; a rejected
            // push is still a failure, so mirror Git and exit non-zero.
            if outcome.rejected {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Auth { action } => match action {
            None => emit(&commands::auth::run()?, opts, ctx),
            Some(AuthAction::Logout) => emit(&commands::auth::logout()?, opts, ctx),
        },
        Command::Update => emit(&commands::update::run(opts.mode)?, opts, ctx),
        Command::Skill => emit(&commands::skill::run()?, opts, ctx),
        Command::Completions { shell } => commands::completions::run(shell),
        Command::Config {
            global,
            list,
            unset,
            key,
            value,
        } => emit(
            &commands::config::run(global, list, unset, key, value)?,
            opts,
            ctx,
        ),
        // `manager` speaks Git's credential protocol on stdout; it has no JSON form.
        Command::Manager { operation } => commands::manager::run(&operation),
        // The server programs speak the Git wire protocol on stdout.
        Command::UploadPack { args } => commands::serve::run(
            commands::serve::Service::UploadPack,
            &args.directory,
            args.stateless_rpc,
            args.advertise_refs,
        ),
        Command::ReceivePack { args } => commands::serve::run(
            commands::serve::Service::ReceivePack,
            &args.directory,
            args.stateless_rpc,
            args.advertise_refs,
        ),
    }
}

#[cfg(test)]
mod render_outcome_snapshots;

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::PathBuf;

    use clap::CommandFactory;
    use serde_json::Value;

    use super::Cli;

    /// Options every command accepts; they're documented once on the docs
    /// overview page rather than on each command's page.
    const GLOBAL_OPTIONS: &[&str] = &["help", "version", "json", "markdown", "filter", "threads"];

    /// Required `##` sections on every command page, in order. `Markdown output`
    /// may appear after `JSON output` when the command supports `--markdown`.
    const TEMPLATE_HEADINGS: &[&str] = &[
        "Synopsis",
        "Description",
        "Options",
        "Examples",
        "JSON output",
        "See also",
    ];

    /// Plumbing commands whose stdout is a wire protocol or credential stream,
    /// not a JSON outcome object.
    const NO_JSON_COMMANDS: &[&str] = &["completions", "manager", "upload-pack", "receive-pack"];

    /// Subcommands excluded from three-way output snapshot coverage (plumbing + shell completions).
    const OUTPUT_SNAPSHOT_EXEMPT: &[&str] = &[
        "help",
        "completions",
        "manager",
        "upload-pack",
        "receive-pack",
    ];

    /// In-process render snapshots that satisfy coverage for a top-level command.
    const RENDER_SNAPSHOT_FOR_COMMAND: &[(&str, &str)] =
        &[("auth", "auth_render"), ("update", "update_render")];

    fn commands_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../content/docs/commands")
    }

    fn split_front_matter(text: &str) -> (&str, &str) {
        let Some(rest) = text.strip_prefix("---\n") else {
            return ("", text);
        };
        let Some((_, body)) = rest.split_once("\n---\n") else {
            return ("", text);
        };
        ("", body)
    }

    fn h2_headings(body: &str) -> Vec<String> {
        let mut headings = Vec::new();
        let mut in_fenced_block = false;
        for line in body.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("```") {
                in_fenced_block = !in_fenced_block;
                continue;
            }
            if in_fenced_block {
                continue;
            }
            if let Some(heading) = line.strip_prefix("## ") {
                headings.push(heading.trim().to_owned());
            }
        }
        headings
    }

    fn section_body(body: &str, heading: &str) -> Option<String> {
        let marker = format!("## {heading}");
        let start = body.find(&marker)? + marker.len();
        let rest = &body[start..];
        let end = rest.find("\n## ").map(|i| start + i).unwrap_or(body.len());
        Some(body[start..end].trim().to_owned())
    }

    fn fenced_json_blocks(section: &str) -> Vec<String> {
        const OPEN: &str = "```json";
        let mut blocks = Vec::new();
        let mut rest = section;
        while let Some(start) = rest.find(OPEN) {
            let after = &rest[start + OPEN.len()..];
            let Some(end) = after.find("```") else {
                break;
            };
            blocks.push(after[..end].trim().to_owned());
            rest = &after[end + 3..];
        }
        blocks
    }

    fn json_section_is_none(section: &str) -> bool {
        let lower = section.to_ascii_lowercase();
        lower.contains("none.")
            || lower.contains("no json")
            || lower.contains("has no effect")
            || lower.contains("always speaks")
    }

    fn command_supports_markdown(command: &clap::Command) -> bool {
        command
            .get_arguments()
            .any(|arg| arg.get_id() == "markdown" || arg.get_long() == Some("markdown"))
    }

    fn cli_supports_markdown() -> bool {
        Cli::command()
            .get_arguments()
            .any(|arg| arg.get_id() == "markdown" || arg.get_long() == Some("markdown"))
    }

    fn top_level_keys(value: &Value) -> HashSet<String> {
        match value {
            Value::Object(map) => map.keys().cloned().collect(),
            _ => HashSet::new(),
        }
    }

    /// Keys named in the first column of a markdown field table inside the JSON section.
    fn field_table_top_level_keys(section: &str) -> HashSet<String> {
        let mut keys = HashSet::new();
        let mut past_header = false;
        for line in section.lines() {
            let trimmed = line.trim();
            if !trimmed.starts_with('|') {
                continue;
            }
            if trimmed.contains("---") {
                past_header = true;
                continue;
            }
            if !past_header {
                continue;
            }
            let Some(first_cell) = trimmed.split('|').nth(1) else {
                continue;
            };
            let cell = first_cell.trim().trim_matches('`');
            let top = cell.split(['.', '[']).next().unwrap_or(cell).trim();
            if top.is_empty() || top.eq_ignore_ascii_case("field") {
                continue;
            }
            keys.insert(top.to_owned());
        }
        keys
    }

    fn json_section_has_field_table(section: &str) -> bool {
        let has_header = section
            .lines()
            .any(|line| line.contains('|') && line.to_lowercase().contains("field"));
        let has_separator = section
            .lines()
            .any(|line| line.trim().starts_with('|') && line.contains("---"));
        has_header && has_separator && !field_table_top_level_keys(section).is_empty()
    }

    fn page_mentions_markdown(body: &str) -> bool {
        body.contains("## Markdown output") || body.contains("--markdown")
    }

    /// Validate `##` heading order, allowing `Markdown output` only between JSON output and See also.
    fn validate_page_headings(name: &str, headings: &[String]) -> Vec<String> {
        let see_also_idx = TEMPLATE_HEADINGS.len() - 1;
        let mut problems = Vec::new();
        let mut template_idx = 0usize;
        let mut heading_idx = 0usize;
        while heading_idx < headings.len() {
            let heading = &headings[heading_idx];
            if template_idx < TEMPLATE_HEADINGS.len() && heading == &TEMPLATE_HEADINGS[template_idx]
            {
                template_idx += 1;
                heading_idx += 1;
                continue;
            }
            if heading == "Markdown output" {
                if template_idx == see_also_idx {
                    heading_idx += 1;
                    continue;
                }
                if template_idx > see_also_idx {
                    problems.push(format!("{name}.md: unexpected ## Markdown output"));
                } else {
                    problems.push(format!(
                        "{name}.md: ## Markdown output must appear only after ## JSON output and before ## See also"
                    ));
                }
                heading_idx += 1;
                continue;
            }
            if template_idx < TEMPLATE_HEADINGS.len() {
                problems.push(format!(
                    "{name}.md: expected ## {} but found ## {heading}",
                    TEMPLATE_HEADINGS[template_idx]
                ));
            } else {
                problems.push(format!("{name}.md: unexpected ## {heading}"));
            }
            heading_idx += 1;
            template_idx = TEMPLATE_HEADINGS.len();
        }
        if template_idx < TEMPLATE_HEADINGS.len() {
            problems.push(format!(
                "{name}.md: missing ## {}",
                TEMPLATE_HEADINGS[template_idx]
            ));
        }
        problems
    }

    /// Every command page uses the documented section order.
    #[test]
    fn command_pages_follow_template() {
        let dir = commands_dir();
        let mut problems = Vec::new();
        for command in Cli::command().get_subcommands() {
            let name = command.get_name();
            if name == "help" {
                continue;
            }
            let path = dir.join(format!("{name}.md"));
            let Ok(text) = std::fs::read_to_string(&path) else {
                problems.push(format!("missing page {}", path.display()));
                continue;
            };
            let (_, body) = split_front_matter(&text);
            problems.extend(validate_page_headings(name, &h2_headings(body)));
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn command_pages_follow_template_rejects_markdown_after_see_also() {
        let headings = vec![
            "Synopsis".into(),
            "Description".into(),
            "Options".into(),
            "Examples".into(),
            "JSON output".into(),
            "See also".into(),
            "Markdown output".into(),
        ];
        let problems = validate_page_headings("commit", &headings);
        assert!(
            problems
                .iter()
                .any(|p| p.contains("unexpected ## Markdown output")),
            "{problems:?}"
        );
    }

    #[test]
    fn h2_headings_ignores_headings_inside_fenced_blocks() {
        let body = r##"## Synopsis

## Markdown output

Example:

```markdown
## Garbage collection

### Tasks
```

## See also
"##;
        assert_eq!(
            h2_headings(body),
            vec![
                "Synopsis".to_owned(),
                "Markdown output".to_owned(),
                "See also".to_owned(),
            ]
        );
    }

    /// JSON examples in the JSON output section must parse, and document top-level keys.
    #[test]
    fn json_examples_parse() {
        let dir = commands_dir();
        let mut problems = Vec::new();
        for command in Cli::command().get_subcommands() {
            let name = command.get_name();
            if name == "help" {
                continue;
            }
            let path = dir.join(format!("{name}.md"));
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let (_, body) = split_front_matter(&text);
            let Some(section) = section_body(body, "JSON output") else {
                problems.push(format!("{name}.md: no JSON output section"));
                continue;
            };
            let blocks = fenced_json_blocks(&section);
            if NO_JSON_COMMANDS.contains(&name) {
                if !blocks.is_empty() {
                    problems.push(format!(
                        "{name}.md: plumbing command must not include ```json examples"
                    ));
                }
                if !json_section_is_none(&section) {
                    problems.push(format!(
                        "{name}.md: JSON output section must explain that there is no JSON"
                    ));
                }
                continue;
            }
            if blocks.is_empty() {
                problems.push(format!(
                    "{name}.md: JSON output section needs a ```json example"
                ));
                continue;
            }
            if !json_section_has_field_table(&section) {
                problems.push(format!(
                    "{name}.md: JSON output section needs a field table (| Field | … |)"
                ));
                continue;
            }
            let table_keys = field_table_top_level_keys(&section);
            for (i, block) in blocks.iter().enumerate() {
                let parsed: Value = serde_json::from_str(block).unwrap_or_else(|e| {
                    problems.push(format!("{name}.md: JSON example {i} does not parse: {e}"));
                    Value::Null
                });
                if parsed.is_null() {
                    continue;
                }
                for key in top_level_keys(&parsed) {
                    if !table_keys.contains(&key) {
                        problems.push(format!(
                            "{name}.md: JSON example documents `{key}` but the field table does not"
                        ));
                    }
                }
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn json_examples_parse_requires_field_table_not_prose() {
        let section = r#"Pass `--json` for output.

The keys are `initialized`, `path`, `bare`, and `branch`.

```json
{"initialized": true, "path": "/tmp/x", "bare": false, "branch": "main"}
```
"#;
        assert!(!json_section_has_field_table(section));
        let keys = field_table_top_level_keys(section);
        assert!(keys.is_empty());
    }

    /// `--markdown` is documented only when clap exposes a `markdown` flag.
    #[test]
    fn markdown_output_only_when_supported() {
        let dir = commands_dir();
        let global_markdown = cli_supports_markdown();
        let mut problems = Vec::new();
        for command in Cli::command().get_subcommands() {
            let name = command.get_name();
            if name == "help" {
                continue;
            }
            let path = dir.join(format!("{name}.md"));
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let (_, body) = split_front_matter(&text);
            let mentions_markdown = page_mentions_markdown(body);
            let supported = global_markdown || command_supports_markdown(command);
            if mentions_markdown && !supported {
                problems.push(format!(
                    "{name}.md documents --markdown but the command has no markdown flag"
                ));
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn markdown_output_only_when_supported_detects_unbackticked_flag() {
        let body = "## JSON output\n\nPass --markdown for agent output.\n";
        assert!(page_mentions_markdown(body));
    }

    /// Each library area from the project direction document must have a CLI entry point.
    #[test]
    fn every_library_area_has_a_command() {
        const LIBRARY_AREAS: &[(&str, &[&str])] = &[
            ("object database", &["cat"]),
            ("network transport", &["fetch"]),
            ("refs", &["branch"]),
            ("index", &["add"]),
            ("stash", &["stash"]),
            ("reflog", &["reflog"]),
            ("diffing", &["diff"]),
            ("revwalking", &["revs"]),
            ("revparsing", &["resolve"]),
            ("config", &["config"]),
            ("ignore rules", &["ignore", "check"]),
            ("hooks", &["hooks"]),
            ("patch-ids", &["patch-id"]),
            ("notes", &["notes"]),
            ("bundles", &["bundle", "unbundle"]),
            ("fsck", &["fsck"]),
            ("worktrees", &["worktree", "lock"]),
        ];

        fn cli_has_path(command: &clap::Command, path: &[&str]) -> bool {
            let Some((head, tail)) = path.split_first() else {
                return true;
            };
            let Some(sub) = command
                .get_subcommands()
                .find(|subcommand| subcommand.get_name() == *head)
            else {
                return false;
            };
            if tail.is_empty() {
                return true;
            }
            cli_has_path(sub, tail)
        }

        let root = Cli::command();
        let mut problems = Vec::new();
        for (area, path) in LIBRARY_AREAS {
            if !cli_has_path(&root, path) {
                problems.push(format!(
                    "library area `{area}` has no CLI path `{}`",
                    path.join(" ")
                ));
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    /// Every command (including hidden plumbing) has a man page in
    /// `content/docs/commands/`, and that page mentions each of its flags and
    /// subcommands. Positional arguments are named freely in the synopsis. Run `python3 scripts/docs.py` after editing the pages.
    #[test]
    fn every_command_is_documented() {
        let pages = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../content/docs/commands");
        let mut problems = Vec::new();
        for command in Cli::command().get_subcommands() {
            let name = command.get_name();
            if name == "help" {
                continue;
            }
            let path = pages.join(format!("{name}.md"));
            let Ok(page) = std::fs::read_to_string(&path) else {
                problems.push(format!("missing page {}", path.display()));
                continue;
            };
            for arg in command.get_arguments() {
                let id = arg.get_id().as_str();
                if GLOBAL_OPTIONS.contains(&id) {
                    continue;
                }
                let spellings = arg
                    .get_long()
                    .map(|long| format!("--{long}"))
                    .into_iter()
                    .chain(arg.get_short().map(|short| format!("-{short}")));
                for spelling in spellings {
                    if !page.contains(&format!("`{spelling}")) {
                        problems.push(format!("{name}.md does not document `{spelling}`"));
                    }
                }
            }
            for sub in command.get_subcommands() {
                let sub_name = sub.get_name();
                if sub_name != "help" && !page.contains(&format!("`{sub_name}")) {
                    problems.push(format!("{name}.md does not document `{sub_name}`"));
                }
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    fn snapshot_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots")
    }

    fn read_snapshot_payload(stem: &str) -> Option<serde_json::Value> {
        use crate::json_schema::{discover_snapshot_stems, SNAPSHOT_FILE_PREFIX};
        let dir = snapshot_dir();
        if !discover_snapshot_stems(&dir).contains(stem) {
            return None;
        }
        let path = dir.join(format!("{SNAPSHOT_FILE_PREFIX}{stem}.snap"));
        let text = std::fs::read_to_string(&path).ok()?;
        let json_body = text.rsplit_once("\n---\n").map(|(_, body)| body)?;
        serde_json::from_str(json_body).ok()
    }

    fn mode_triple_present(value: &serde_json::Value) -> bool {
        ["human", "json", "markdown"]
            .iter()
            .all(|key| value.get(key).is_some())
    }

    fn snapshot_has_three_modes(stem: &str, payload: &serde_json::Value) -> bool {
        if stem == "auth_render" {
            return payload.get("sign_in").is_some_and(mode_triple_present)
                && payload.get("logout").is_some_and(mode_triple_present);
        }
        mode_triple_present(payload)
    }

    fn command_covered_by_snapshot_stem(stem: &str, command: &str) -> bool {
        use crate::json_schema::snapshot_command_args;
        if RENDER_SNAPSHOT_FOR_COMMAND
            .iter()
            .any(|(cmd, snap)| *cmd == command && *snap == stem)
        {
            return true;
        }
        let Some((_, args)) = snapshot_command_args()
            .iter()
            .find(|(name, _)| *name == stem)
        else {
            return false;
        };
        args.first().is_some_and(|first| *first == command)
    }

    /// Every non-plumbing command has at least one committed three-mode output snapshot.
    #[test]
    fn every_command_has_output_snapshots() {
        use crate::json_schema::discover_snapshot_stems;
        let dir = snapshot_dir();
        let stems = discover_snapshot_stems(&dir);
        let mut problems = Vec::new();
        for stem in &stems {
            let Some(payload) = read_snapshot_payload(stem) else {
                problems.push(format!("snapshot `{stem}` is unreadable"));
                continue;
            };
            if !snapshot_has_three_modes(stem, &payload) {
                problems.push(format!(
                    "snapshot `{stem}` must include human, json, and markdown sections"
                ));
            }
        }
        for command in Cli::command().get_subcommands() {
            let name = command.get_name();
            if OUTPUT_SNAPSHOT_EXEMPT.contains(&name) {
                continue;
            }
            let covered = stems
                .iter()
                .any(|stem| command_covered_by_snapshot_stem(stem, name));
            if !covered {
                problems.push(format!("command `{name}` has no output snapshot"));
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    fn schema_top_level_field_names(schema: &serde_json::Value) -> HashSet<String> {
        schema_field_names_in(schema, schema)
    }

    /// Top-level properties of `node`, following `#/definitions/` refs into `root`.
    fn schema_field_names_in(
        node: &serde_json::Value,
        root: &serde_json::Value,
    ) -> HashSet<String> {
        let definition = |value: &serde_json::Value| {
            value
                .get("$ref")
                .and_then(|r| r.as_str())
                .and_then(|r| r.strip_prefix("#/definitions/"))
                .and_then(|name| root.get("definitions").and_then(|d| d.get(name)))
        };
        if let Some(def) = definition(node) {
            return schema_field_names_in(def, root);
        }
        let mut keys = HashSet::new();
        if let Some(props) = node.get("properties").and_then(|v| v.as_object()) {
            keys.extend(props.keys().cloned());
            return keys;
        }
        for branch_key in ["anyOf", "oneOf"] {
            if let Some(items) = node.get(branch_key).and_then(|v| v.as_array()) {
                for item in items {
                    keys.extend(schema_field_names_in(item, root));
                }
                return keys;
            }
        }
        if let Some(items) = node.get("items") {
            keys.extend(schema_field_names_in(items, root));
        }
        keys
    }

    fn schema_keys_for_command(command: &str) -> HashSet<String> {
        use crate::json_schema::{load_committed_schema, schema_registry};
        let mut keys = HashSet::new();
        for entry in schema_registry() {
            if entry.command == command || entry.command.starts_with(&format!("{command}.")) {
                keys.extend(schema_top_level_field_names(&load_committed_schema(
                    entry.command,
                )));
            }
        }
        keys
    }

    fn page_requires_markdown(command: &clap::Command) -> bool {
        cli_supports_markdown() || command_supports_markdown(command)
    }

    fn markdown_section_has_schema_link(section: &str, command: &str) -> bool {
        section.contains("/schemas/")
            || section.contains("grit-scm.com/schemas")
            || section.contains(&format!("/schemas/{command}/"))
    }

    /// JSON field tables document exactly the top-level schema properties; Markdown sections link schemas.
    #[test]
    fn json_field_tables_match_schemas() {
        let dir = commands_dir();
        let mut problems = Vec::new();
        for command in Cli::command().get_subcommands() {
            let name = command.get_name();
            if name == "help" || NO_JSON_COMMANDS.contains(&name) {
                continue;
            }
            let path = dir.join(format!("{name}.md"));
            let Ok(text) = std::fs::read_to_string(&path) else {
                problems.push(format!("missing page {}", path.display()));
                continue;
            };
            let (_, body) = split_front_matter(&text);
            let Some(json_section) = section_body(body, "JSON output") else {
                problems.push(format!("{name}.md: no JSON output section"));
                continue;
            };
            if !json_section.contains("Schema:") && !json_section.contains("/schemas/") {
                problems.push(format!(
                    "{name}.md: JSON output section must link the v1 schema under /schemas/"
                ));
            }
            let table_keys = field_table_top_level_keys(&json_section);
            let schema_keys = schema_keys_for_command(name);
            if table_keys != schema_keys {
                let missing: Vec<_> = schema_keys.difference(&table_keys).collect();
                let extra: Vec<_> = table_keys.difference(&schema_keys).collect();
                if !missing.is_empty() {
                    problems.push(format!(
                        "{name}.md: field table missing schema keys: {missing:?}"
                    ));
                }
                if !extra.is_empty() {
                    problems.push(format!(
                        "{name}.md: field table documents keys not in schema: {extra:?}"
                    ));
                }
            }
            if page_requires_markdown(command) {
                let Some(md_section) = section_body(body, "Markdown output") else {
                    problems.push(format!("{name}.md: missing ## Markdown output section"));
                    continue;
                };
                if !markdown_section_has_schema_link(&md_section, name) {
                    problems.push(format!(
                        "{name}.md: Markdown output must link the command JSON schema"
                    ));
                }
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn render_snapshot_missing_markdown_mode_is_detected() {
        let payload = serde_json::json!({
            "sign_in": {
                "human": { "exit": 0, "stdout": "", "stderr": "" },
                "json": { "exit": 0, "stdout": "{}", "stderr": "" }
            },
            "logout": {
                "human": { "exit": 0, "stdout": "", "stderr": "" },
                "json": { "exit": 0, "stdout": "{}", "stderr": "" },
                "markdown": { "exit": 0, "stdout": "", "stderr": "" }
            }
        });
        assert!(
            !snapshot_has_three_modes("auth_render", &payload),
            "auth_render must require human, json, and markdown for every scenario"
        );
    }

    #[test]
    fn markdown_schema_link_must_live_in_markdown_section() {
        let json_section = "Schema: [v1](/schemas/pull/v1.json)\n";
        let md_section = "Pass `--markdown` for agent-friendly output.\n";
        assert!(markdown_section_has_schema_link(json_section, "pull"));
        assert!(!markdown_section_has_schema_link(md_section, "pull"));
    }

    #[test]
    fn normalize_tag_verify_argv_two_names_and_globals() {
        let out = super::normalize_tag_verify_argv(vec![
            "grit".into(),
            "tag".into(),
            "--verify".into(),
            "one".into(),
            "two".into(),
            "--no-signature".into(),
            "--json".into(),
        ]);
        assert_eq!(
            out,
            vec![
                "grit",
                "tag",
                "--verify",
                "--no-signature",
                "--json",
                "--",
                "one",
                "two"
            ]
        );
    }

    #[test]
    fn normalize_tag_verify_argv_preserves_filter_and_threads_values() {
        let filter = super::normalize_tag_verify_argv(vec![
            "grit".into(),
            "tag".into(),
            "--verify".into(),
            "missing".into(),
            "--filter".into(),
            ".tags".into(),
            "--json".into(),
        ]);
        assert_eq!(
            filter,
            vec!["grit", "tag", "--verify", "--filter", ".tags", "--json", "--", "missing"]
        );
        let threads = super::normalize_tag_verify_argv(vec![
            "grit".into(),
            "tag".into(),
            "--verify".into(),
            "missing".into(),
            "--threads".into(),
            "1".into(),
            "--json".into(),
        ]);
        assert_eq!(
            threads,
            vec![
                "grit",
                "tag",
                "--verify",
                "--threads",
                "1",
                "--json",
                "--",
                "missing"
            ]
        );
    }
}
