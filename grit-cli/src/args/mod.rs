//! Clap argument definitions for `grit`.

mod argv;

pub use argv::{normalize_log_limit_argv, normalize_tag_verify_argv, parse_cli};


use clap::{Parser, Subcommand};
use clap_complete::Shell;

use crate::branch_list_args;
use crate::clap_value_args;
use crate::diff_args;
use crate::diff_format;
use crate::global_args;
use crate::limit_args;
use crate::merge_strategy_args;
use crate::message_input;
use crate::pathspec_args;
use crate::quiet_args;
use crate::rev_select;

fn parse_init_object_format(value: &str) -> Result<grit_lib::objects::HashAlgo, String> {
    grit_lib::objects::HashAlgo::from_name(value)
        .ok_or_else(|| format!("unknown object format '{value}' (expected sha1 or sha256)"))
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub(crate) enum LogGroupByArg {
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
    pub(crate) version: (),
    /// Emit machine-readable JSON instead of human-readable text.
    #[arg(long, global = true, conflicts_with = "markdown")]
    pub(crate) json: bool,
    /// Emit agent-friendly Markdown instead of human-readable text.
    #[arg(long, global = true, conflicts_with = "json")]
    pub(crate) markdown: bool,
    /// jq-like expression applied to JSON output (requires `--json`).
    ///
    /// Iterator-style filters (for example `.commits[].oid`) always emit a JSON
    /// array; singular filters (for example `.branch`) emit one value, including
    /// `null` when a field is missing.
    #[arg(long, global = true, value_name = "EXPR")]
    pub(crate) filter: Option<String>,
    /// Worker threads for parallel library work (`0` = auto, `1` = serial).
    #[arg(long, global = true, value_name = "N")]
    pub(crate) threads: Option<usize>,
    #[command(flatten)]
    pub(crate) repo: global_args::GlobalRepoArgs,
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

/// Subcommands of `grit remote`.
#[derive(Debug, Subcommand)]
pub(crate) enum RemoteAction {
    /// Add a new remote.
    Add {
        /// Short name for the remote (e.g. origin).
        name: String,
        /// The remote's URL or path.
        url: String,
        /// Fetch from the remote after adding it.
        #[arg(long)]
        fetch: bool,
    },
    /// Set the fetch or push URL for an existing remote.
    SetUrl {
        /// Remote to update.
        name: String,
        /// New URL or path.
        url: String,
        /// Update `pushurl` instead of `url`.
        #[arg(long)]
        push: bool,
        /// Show what would change without writing config.
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove a remote and its remote-tracking refs.
    Remove {
        /// Remote name to remove.
        name: String,
        /// Report changes without writing config or refs.
        #[arg(long)]
        dry_run: bool,
    },
    /// Delete stale remote-tracking refs without fetching.
    Prune {
        /// Configured remote name.
        name: String,
        /// Report refs that would be pruned without deleting them.
        #[arg(long)]
        dry_run: bool,
    },
    /// Set or delete `refs/remotes/<name>/HEAD`.
    SetHead {
        /// Configured remote name.
        name: String,
        /// Short branch name (sets HEAD to `refs/remotes/<name>/<branch>`).
        branch: Option<String>,
        /// Query the remote and set HEAD from its default branch.
        #[arg(long, conflicts_with_all = ["branch", "delete"])]
        auto: bool,
        /// Delete the local HEAD symref.
        #[arg(long, conflicts_with_all = ["branch", "auto"])]
        delete: bool,
        /// Show what would change without updating refs.
        #[arg(long)]
        dry_run: bool,
    },
    /// Summarize a remote (URLs, branches, pull config).
    Show {
        /// Configured remote name.
        name: String,
        /// Use local remote-tracking refs only; do not contact the remote.
        #[arg(long = "no-query")]
        no_query: bool,
        /// Return at most this many remote branches.
        #[arg(short = 'n', long = "limit", value_name = "N")]
        limit: Option<usize>,
    },
    /// Rename an existing remote.
    Rename {
        /// Current remote name.
        old: String,
        /// New remote name.
        new: String,
        /// Show what would change without writing the repository.
        #[arg(long)]
        dry_run: bool,
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
pub(crate) enum ReflogAction {
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
        /// Report what would be pruned without rewriting reflog files.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// List refs that currently have a reflog.
    List {
        /// Return at most this many ref names (sorted).
        #[arg(short = 'n', long = "limit", value_name = "N")]
        limit: Option<usize>,
    },
}

/// Subcommands of `grit gc`.
#[derive(Debug, Subcommand)]
pub(crate) enum GcAction {
    /// Repack reachable objects into one pack.
    Repack {
        /// Write a reachability `.bitmap` sidecar for the new pack.
        #[arg(long)]
        bitmaps: bool,
        /// Recompute every delta instead of reusing the ones already in packs.
        #[arg(short, long)]
        force: bool,
        /// Report what would be packed without writing.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Write a reachable commit-graph with changed-path Bloom filters.
    #[command(name = "commit-graph")]
    CommitGraph {
        /// Report commit count without writing the graph file.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Write or refresh the multi-pack-index.
    Midx {
        /// Report pack coverage without writing the index.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Pack loose refs into `packed-refs` (or compact reftable).
    #[command(name = "pack-refs")]
    PackRefs {
        /// Report refs that would be packed without writing.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Delete unreachable loose objects.
    Prune {
        /// Only remove objects older than this date (Git approxidate syntax).
        #[arg(long = "older-than", value_name = "DATE")]
        older_than: Option<String>,
        /// Report objects that would be deleted without removing them.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Move large blobs to a large-object promisor remote and drop them here.
    Offload {
        /// Promisor remote that receives the blobs (`remote.<name>.url`).
        #[arg(long, value_name = "REMOTE")]
        to: String,
        /// Blobs to move: `none` for every blob, or a size such as `1m` (blobs at
        /// least that big). Defaults to `remote.<REMOTE>.partialCloneFilter`.
        #[arg(long, value_name = "none|SIZE")]
        blobs: Option<String>,
        /// The promisor's git directory, when its URL is not a local path.
        #[arg(long = "lop-dir", value_name = "PATH")]
        lop_dir: Option<std::path::PathBuf>,
        /// Also set `uploadpack.allowFilter` and `uploadpack.allowAnySHA1InWant` in the promisor.
        #[arg(long = "configure-lop")]
        configure_lop: bool,
        /// Report what would move without changing anything.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Drop large blobs a promisor remote can supply again and nothing needed recently.
    #[command(name = "drop-large-blobs")]
    DropLargeBlobs {
        /// Drop blobs at least this big, such as `1m` (default:
        /// `maintenance.drop-large-blobs.minSize`, else the promisor's `blob:limit`, else 1m).
        #[arg(long = "min-size", value_name = "SIZE")]
        min_size: Option<String>,
        /// Keep blobs needed since this date (Git approxidate; `now` keeps none for
        /// recency). Default: `maintenance.drop-large-blobs.expire`, else `2.weeks.ago`.
        #[arg(long = "older-than", value_name = "DATE")]
        older_than: Option<String>,
        /// List the blobs that would be dropped without changing anything.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
}

/// Subcommands of `grit ignore`.
#[derive(Debug, Subcommand)]
pub(crate) enum IgnoreAction {
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
pub(crate) enum StashAction {
    /// Save local changes to the stash and reset the working tree.
    Push {
        /// Stash message.
        #[arg(short = 'm', long = "message", allow_hyphen_values = true)]
        message: Option<String>,
        /// Include untracked files in the stash.
        #[arg(
            short = 'u',
            long = "include-untracked",
            conflicts_with_all = ["hunks", "lines"]
        )]
        include_untracked: bool,
        /// Stash only unstaged hunks from `grit diff --json`.
        #[arg(
            long = "hunks",
            value_name = "ID,...",
            action = clap::ArgAction::Append,
            conflicts_with = "include_untracked"
        )]
        hunks: Vec<String>,
        /// Stash only unstaged line ranges (`path:start-end`).
        #[arg(
            long = "lines",
            value_name = "PATH:START-END",
            action = clap::ArgAction::Append,
            conflicts_with = "include_untracked"
        )]
        lines: Vec<String>,
        /// Preview stash without updating refs, index, or worktree.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Stash only index changes vs `HEAD`.
        #[arg(
            short = 'S',
            long = "staged",
            conflicts_with_all = ["keep_index", "hunks", "lines", "include_untracked"]
        )]
        staged: bool,
        /// Leave staged content in the index and worktree after stashing.
        #[arg(
            short = 'k',
            long = "keep-index",
            conflicts_with_all = ["staged", "hunks", "lines"]
        )]
        keep_index: bool,
        #[command(flatten)]
        pathspecs: crate::pathspec_args::TrailingPathspecArgs,
    },
    /// List stash entries.
    List,
    /// Show the summary or patch for a stash entry.
    Show {
        /// Stash index (`0` is newest) or `stash@{n}`.
        index: Option<String>,
        /// Show a diffstat summary (default).
        #[arg(long = "stat")]
        stat: bool,
        /// Show the full patch instead of a diffstat.
        #[arg(short = 'p', long = "patch")]
        patch: bool,
        /// Include untracked files from the stash U parent.
        #[arg(short = 'u', long = "include-untracked")]
        include_untracked: bool,
    },
    /// Create a branch at the stash base commit and apply the stash with `--index`.
    Branch {
        /// New branch name.
        name: String,
        /// Stash index (`0` is newest) or `stash@{n}`.
        index: Option<String>,
    },
    /// Remove every stash entry.
    Clear {
        /// Delete all stash entries (required).
        #[arg(long = "force")]
        force: bool,
        /// Report how many entries would be cleared without deleting.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Apply a stash entry without removing it.
    Apply {
        /// Stash index (`0` is newest) or `stash@{n}`.
        index: Option<String>,
        /// Restore the stashed index as well as the worktree.
        #[arg(long = "index")]
        restore_index: bool,
    },
    /// Apply a stash entry and remove it when there are no conflicts.
    Pop {
        /// Stash index (`0` is newest) or `stash@{n}`.
        index: Option<String>,
        /// Restore the stashed index as well as the worktree.
        #[arg(long = "index")]
        restore_index: bool,
    },
    /// Remove a stash entry without applying it.
    Drop {
        /// Stash index (`0` is newest) or `stash@{n}`.
        index: Option<String>,
    },
}

/// Subcommands of `grit history`.
#[derive(Debug, Subcommand)]
pub(crate) enum HistoryAction {
    /// Remove a commit and replay descendants onto its parent.
    Drop {
        /// Commit to remove (revision spec).
        commit: String,
        #[command(flatten)]
        shared: crate::commands::history::HistoryRewriteArgs,
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
        shared: crate::commands::history::HistoryRewriteArgs,
    },
    /// Replace a commit message and replay descendants onto the new history.
    Reword {
        /// Commit to reword (revision spec).
        commit: String,
        #[command(flatten)]
        message: message_input::MessageInputArgs,
        #[command(flatten)]
        author: message_input::AuthorArg,
        #[command(flatten)]
        shared: crate::commands::history::HistoryRewriteArgs,
    },
    /// Apply staged changes onto a commit and replay descendants.
    Fixup {
        /// Commit to fix up (revision spec).
        commit: String,
        #[command(flatten)]
        message: message_input::MessageInputArgs,
        /// Unstaged hunk ids to fold (`grit diff --unstaged --json`, comma-separated or repeated).
        #[arg(long = "hunks", value_name = "ID,...", action = clap::ArgAction::Append)]
        hunks: Vec<String>,
        /// Unstaged new-side line ranges to fold (`path:start-end` or `path:line`, comma-separated or repeated).
        #[arg(long = "lines", value_name = "PATH:START-END", action = clap::ArgAction::Append)]
        lines: Vec<String>,
        #[command(flatten)]
        pathspecs: crate::pathspec_args::TrailingPathspecArgs,
        #[command(flatten)]
        shared: crate::commands::history::HistoryRewriteArgs,
    },
    /// Assign staged hunks into recent commits and replay descendants once.
    Absorb {
        /// Oldest commit boundary (exclusive) for candidate commits.
        #[arg(long = "base")]
        base: Option<String>,
        #[command(flatten)]
        shared: crate::commands::history::HistoryRewriteArgs,
    },
    /// Replay local commits onto a new base (same as top-level `grit rebase`).
    Rebase {
        /// Commits to replay are those reachable from `HEAD` but not from this revision (defaults to `@{upstream}`).
        #[arg(value_name = "UPSTREAM")]
        upstream: Option<String>,
        /// Replay onto this commit instead of `UPSTREAM`.
        #[arg(long = "onto", value_name = "REV")]
        onto: Option<String>,
        /// Finish a rebase stopped on conflicts after resolving and staging paths.
        #[arg(long = "continue", conflicts_with_all = ["abort", "skip", "upstream", "onto", "check"])]
        continue_rebase: bool,
        /// Cancel an in-progress rebase and restore the branch to its pre-rebase tip.
        #[arg(long = "abort", conflicts_with_all = ["continue_rebase", "skip", "upstream", "onto", "check"])]
        abort: bool,
        /// Skip the current commit and replay the rest.
        #[arg(long = "skip", conflicts_with_all = ["continue_rebase", "abort", "upstream", "onto", "check"])]
        skip: bool,
        #[command(flatten)]
        rebase_flags: crate::commands::history::RebaseRunFlags,
        #[command(flatten)]
        shared: crate::commands::history::HistoryRewriteArgs,
    },
    /// Peel path-limited changes out of a commit into a new parent and replay descendants.
    Split {
        /// Commit to split (revision spec).
        commit: String,
        #[command(flatten)]
        message: message_input::MessageInputArgs,
        /// Replace the remaining commit's message after the split.
        #[arg(long = "rest-message", allow_hyphen_values = true)]
        rest_message: Option<String>,
        #[command(flatten)]
        paths: crate::pathspec_args::TrailingPathspecArgs,
        #[command(flatten)]
        shared: crate::commands::history::HistoryRewriteArgs,
    },
    /// Undo the last applied history rewrite using the recorded ref plan.
    Undo {
        #[command(flatten)]
        shared: crate::commands::history::HistoryRewriteArgs,
    },
    /// Fold a revision range into the oldest commit and replay descendants.
    Squash {
        /// Revision range (same syntax as `git rev-list`, for example `base..tip`).
        #[arg(required = true)]
        range: Vec<String>,
        #[command(flatten)]
        message: message_input::MessageInputArgs,
        /// Build a combined autosquash-style message from every commit in the range.
        #[arg(long = "combine", conflicts_with_all = ["message", "message_file"])]
        combine: bool,
        #[command(flatten)]
        shared: crate::commands::history::HistoryRewriteArgs,
    },
}

/// Subcommands of `grit notes`.
#[derive(Debug, Subcommand)]
pub(crate) enum NotesAction {
    /// List notes attached to commits.
    List {
        #[command(flatten)]
        limit: limit_args::LimitArgs,
    },
    /// Show the note for a revision.
    Show {
        /// Commit (or other object) to show the note for.
        rev: String,
    },
    /// Add or replace a note on a revision.
    Add {
        /// Revision to attach the note to.
        rev: String,
        #[command(flatten)]
        message: message_input::MessageInputArgs,
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
pub(crate) enum WorktreeAction {
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
        #[arg(short = 'b', long = "branch", value_name = "NAME", conflicts_with_all = ["force_branch", "detach"])]
        branch: Option<String>,
        /// Create or reset a branch to the start point (`-B`).
        #[arg(short = 'B', value_name = "NAME", conflicts_with_all = ["branch", "detach"])]
        force_branch: Option<String>,
        /// Check out in detached HEAD mode.
        #[arg(long = "detach", conflicts_with_all = ["branch", "force_branch"])]
        detach: bool,
        /// Reuse a stale registration when the working tree directory is missing.
        #[arg(short, long)]
        force: bool,
        /// Mark the new worktree locked before checkout finishes.
        #[arg(long = "lock")]
        lock: bool,
        /// Reason stored in the lock file (requires `--lock`).
        #[arg(long = "reason", requires = "lock")]
        reason: Option<String>,
        /// Register the worktree without populating files (HEAD only).
        #[arg(long = "no-checkout")]
        no_checkout: bool,
        /// Set upstream when the start point is a remote-tracking branch.
        #[arg(long = "track")]
        track: bool,
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
    Prune {
        /// List prunable worktrees without removing registrations.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
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
    /// Move a linked worktree to a new directory.
    Move {
        /// Current worktree path.
        worktree: std::path::PathBuf,
        /// New path for the working tree.
        new_path: std::path::PathBuf,
        /// Allow move when the worktree is locked.
        #[arg(short, long)]
        force: bool,
        /// Preview the move without changing disk state.
        #[arg(long)]
        dry_run: bool,
    },
    /// Fix `.git` / `gitdir` links after a manual directory move.
    Repair {
        /// Linked worktree roots to repair (default: all registered linked worktrees).
        #[arg(value_name = "PATH")]
        paths: Vec<std::path::PathBuf>,
        /// Report repairs without writing files.
        #[arg(long)]
        dry_run: bool,
    },
}

/// Subcommands of `grit bundle`.
#[derive(Debug, Subcommand)]
pub(crate) enum BundleAction {
    /// Write a bundle file from revision specs.
    Create {
        /// Output bundle path.
        file: String,
        /// Commits or ref ranges to include (same rules as `grit log`).
        #[arg(value_name = "REV", conflicts_with = "all")]
        revs: Vec<String>,
        /// Include every ref tip in the bundle header (`HEAD`, all branches and tags).
        #[arg(long)]
        all: bool,
        /// Report refs, prerequisites, and object count without writing a file.
        #[arg(long = "dry-run")]
        dry_run: bool,
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
        #[command(flatten)]
        limit: limit_args::LimitArgs,
    },
    /// Ingest a bundle pack into the current repository.
    Unbundle {
        /// Bundle file to read.
        file: String,
        /// Verify and list ref updates without ingesting objects or writing refs.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
}

/// Subcommands of `grit hooks`.
#[derive(Debug, Subcommand)]
pub(crate) enum HooksAction {
    /// List hook scripts in the resolved hooks directory.
    List {
        #[command(flatten)]
        limit: limit_args::LimitArgs,
    },
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
pub(crate) enum AuthAction {
    /// Forget the stored token (sign out) for grithub (GitHub inside a github.com repository), or for a `grit serve` URL.
    Logout {
        /// A host to sign out of instead of the default one.
        #[arg(value_name = "URL")]
        url: Option<String>,
        /// Report which stored credential would be removed without erasing it.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
}

/// Subcommands of `grit slice`.
#[derive(Debug, Subcommand)]
pub(crate) enum SliceAction {
    /// List the slices.
    List,
    /// Make a slice.
    New {
        /// The slice's name.
        name: String,
        /// A path in the slice (repeatable).
        #[arg(short = 'p', long = "path", value_name = "PATH")]
        paths: Vec<String>,
        /// What the slice is for.
        #[arg(short = 'd', long, value_name = "TEXT")]
        description: Option<String>,
    },
    /// Add paths to a slice or remove them.
    Edit {
        /// The slice's name.
        name: String,
        /// A path to add (repeatable).
        #[arg(long, value_name = "PATH")]
        add: Vec<String>,
        /// A path to remove (repeatable).
        #[arg(long, value_name = "PATH")]
        remove: Vec<String>,
        /// A new description.
        #[arg(short = 'd', long, value_name = "TEXT")]
        description: Option<String>,
    },
    /// Delete a slice.
    Delete {
        /// The slice's name.
        name: String,
    },
}

/// Subcommands of `grit share`.
#[derive(Debug, Subcommand)]
pub(crate) enum ShareAction {
    /// Share a branch of your space with people, or make a share link.
    Add {
        /// The branch (pushed to your space first when it isn't there yet).
        branch: String,
        /// User names or email addresses to share it with; each is emailed. An address
        /// without an account is invited to sign up and join.
        #[arg(value_name = "USER|EMAIL")]
        users: Vec<String>,
        /// Make a share link anyone signed in can accept.
        #[arg(long)]
        link: bool,
        /// How many times the link can be accepted: a number, or `unlimited` (default 1).
        #[arg(long, value_name = "N", requires = "link")]
        uses: Option<String>,
        /// How many days the link works, up to 30 (default 7).
        #[arg(long, value_name = "DAYS", requires = "link")]
        expires: Option<u32>,
        /// Share the branch's own history only, on a snapshot of the base branch,
        /// without the rest of the project's history.
        #[arg(long)]
        no_history: bool,
        /// Share only this path of the tree (implies --no-history). Repeatable.
        #[arg(long = "path", value_name = "PATH")]
        paths: Vec<String>,
    },
    /// List your shared branches, and where each member's head is.
    List {
        /// Only this branch.
        branch: Option<String>,
    },
    /// Accept a share link, and fetch the branch (or clone your copy).
    Accept {
        /// The link, `https://<server>/share/<token>`.
        link: String,
    },
    /// Remove people from a shared branch, or end its share when you name nobody.
    Remove {
        /// The shared branch.
        branch: String,
        /// User names to remove, or email addresses whose invitations to withdraw.
        #[arg(value_name = "USER|EMAIL")]
        users: Vec<String>,
    },
    /// Leave a shared branch; your head stays a branch in your space.
    Leave {
        /// The shared branch.
        branch: String,
    },
    /// Put your head and another member's back together after they diverged.
    Resolve {
        /// The shared branch.
        branch: String,
        /// The option to apply, such as `combine` or `keep_both` (default: `combine`,
        /// when nothing would be lost; otherwise the options are listed). For a
        /// branch pushed again after it was integrated: `rebase`, `rename` or
        /// `reshare`.
        option: Option<String>,
        /// For `rename`: the branch's new name.
        name: Option<String>,
        /// The member whose head to resolve with (needed when several diverged).
        #[arg(long, value_name = "USER")]
        with: Option<String>,
    },
    /// Revoke a share link.
    Revoke {
        /// The shared branch.
        branch: String,
        /// The link's id, from `grit share list`.
        link: String,
    },
}

/// The snapshot view `grit share add --no-history --path …` asks for.
pub(crate) fn api_view(paths: Vec<String>) -> grit_lib::remote::grit_server_share::ShareView {
    grit_lib::remote::grit_server_share::ShareView { paths }
}

/// Subcommands of `grit serve`.
#[derive(Debug, Subcommand)]
pub(crate) enum ServeAction {
    /// Add a user. Prompts for their password, or generates a temporary one.
    #[command(name = "add-user")]
    AddUser {
        /// The user's email address, which is also their user name.
        email: String,
        /// Make the user an admin, with read and write access to every repository.
        #[arg(long)]
        admin: bool,
    },
    /// Create an access token for a user, for scripts and CI that cannot sign in in a browser.
    Token {
        /// The user's email address.
        email: String,
    },
    /// Give a user read access to a repository (or write access with `--write`).
    Grant {
        /// The email address of the user to give access to.
        user: String,
        /// The repository name, with or without `.git`.
        repository: String,
        /// Allow pushing as well as fetching.
        #[arg(long)]
        write: bool,
    },
    /// List users, their tokens and their repository access.
    Users,
}

/// Top-level `grit` commands.
///
/// In the binary, each subcommand's arguments are built only when that subcommand is parsed,
/// so startup does not pay for the whole tree. Help output and completions go through
/// [`cli_command::full_cli_command`] so deferred building cannot replace a subcommand's
/// description. Unit tests keep the tree eager: the contract, docs and schema checks walk each
/// command's declared arguments, which `Command::build` would mix with globals inherited from
/// the parent.
#[derive(Debug, Subcommand)]
#[cfg_attr(not(test), command(defer = true))]
pub(crate) enum Command {
    /// Create a new, empty repository.
    Init {
        /// Where to create the repository (defaults to the current directory).
        path: Option<String>,
        /// Create a bare repository (no working tree).
        #[arg(long, conflicts_with = "separate_git_dir")]
        bare: bool,
        /// Ref storage backend for the new repository.
        #[arg(long, value_name = "FORMAT", default_value = "files")]
        ref_format: grit_lib::refs::ref_storage::RefStorageFormat,
        /// Initial branch name (overrides `init.defaultBranch` from config).
        #[arg(short = 'b', long = "initial-branch", value_name = "NAME")]
        initial_branch: Option<String>,
        /// Place the git directory here and link from the work tree via a gitfile.
        #[arg(long = "separate-git-dir", value_name = "DIR", conflicts_with = "bare")]
        separate_git_dir: Option<std::path::PathBuf>,
        /// Object hash algorithm for the new repository (default SHA-1; omitted on reinit preserves format).
        #[arg(
            long = "object-format",
            value_name = "ALGO",
            value_parser = parse_init_object_format
        )]
        object_format: Option<grit_lib::objects::HashAlgo>,
    },
    /// Start work on a repository from a snapshot of its main branch (or some of its paths) instead of a clone.
    Checkout {
        /// The repository on a server that offers snapshot checkouts (`grit serve`).
        url: String,
        /// A slice (named set of paths) on the server to check out; `--path` adds to it.
        slice: Option<String>,
        /// Check out only this path, plus the files in the directories above it (repeatable).
        #[arg(short = 'p', long = "path", value_name = "PATH")]
        paths: Vec<String>,
        /// Directory to check out into (defaults to the repository name).
        #[arg(long, value_name = "DIR")]
        into: Option<std::path::PathBuf>,
    },
    /// Send a snapshot checkout's branches back to the server, grafted onto its real history.
    Checkin {
        /// Branches to check in (default: every branch with commits on top of the snapshot).
        #[arg(value_name = "BRANCH")]
        branches: Vec<String>,
        /// Replace a server branch even where that drops commits it has.
        #[arg(short = 'f', long)]
        force: bool,
    },
    /// Merge a branch into the server's target branch (checking it in first), then refresh this checkout.
    Integrate {
        /// The branch to merge.
        #[arg(value_name = "BRANCH")]
        branch: String,
        /// For a shared branch: integrate without waiting for this member's
        /// changes, which stay in their copy. Repeatable.
        #[arg(long, value_name = "USER")]
        without: Vec<String>,
    },
    /// Push and pull every active branch between this clone and your space on the server (grithub).
    Sync,
    /// Work on a branch together with other users of the server: each has their own head of it.
    Share {
        #[command(subcommand)]
        action: ShareAction,
    },
    /// Named sets of paths to check out together, kept on the server's target branch.
    Slice {
        #[command(subcommand)]
        action: SliceAction,
        /// The repository whose slices to use (default: the checkout you are in).
        #[arg(long, global = true, value_name = "URL")]
        url: Option<String>,
    },
    /// Copy a remote repository into a new directory.
    ///
    /// Submodule gitlinks are checked out as empty directories only; grit does not fetch submodule
    /// repositories or provide a `submodule` command.
    Clone {
        /// The repository to clone (URL or local path).
        url: String,
        /// Directory to clone into (defaults to the repository name).
        dir: Option<String>,
        /// Truncate fetched history to this many commits (`git clone --depth`).
        #[arg(long, value_name = "N", conflicts_with = "shallow")]
        depth: Option<u32>,
        /// Branch or tag to check out instead of the remote default.
        #[arg(short = 'b', long = "branch", value_name = "NAME")]
        branch: Option<String>,
        /// Shallow clone of the fetched ref (`git clone --depth=1`; hidden alias).
        #[arg(long, hide = true, conflicts_with = "depth")]
        shallow: bool,
        /// Fetch only the branch checked out (same as `git clone --single-branch`).
        #[arg(long, overrides_with = "no_single_branch")]
        single_branch: bool,
        /// Fetch all branch tips even when `--shallow` is set (`git clone --no-single-branch`).
        #[arg(long, overrides_with = "single_branch")]
        no_single_branch: bool,
        /// Do not fetch any tags from the remote.
        #[arg(long)]
        no_tags: bool,
        /// Name for the remote (default `origin`).
        #[arg(short = 'o', long, value_name = "NAME")]
        origin: Option<String>,
        /// Partial clone: omit blobs (`none`), blobs at/above a size (`1k`, `512`, …), or
        /// what the server's accepted promisor remotes hold (`auto`).
        #[arg(long, value_name = "none|auto|SIZE")]
        blobs: Option<String>,
        /// Set a config value in the new repository before fetching (`git clone -c`;
        /// repeatable). Unlike the global `-c`, it is kept in the clone's config.
        #[arg(long = "config", value_name = "KEY=VALUE", value_parser = global_args::parse_config_override)]
        config: Vec<grit_lib::config::environment::ConfigOverride>,
        /// Create a bare repository (`git clone --bare`).
        #[arg(long)]
        bare: bool,
        /// Do not check out the default branch (`git clone --no-checkout`).
        #[arg(long = "no-checkout")]
        no_checkout: bool,
        #[command(flatten)]
        quiet: quiet_args::QuietArgs,
    },
    /// List remotes, or add one.
    Remote {
        /// Show fetch and push URLs (like `git remote -v`).
        #[arg(short = 'v', long)]
        verbose: bool,
        #[command(subcommand)]
        action: Option<RemoteAction>,
    },
    /// Show recent commits reachable from HEAD.
    Log {
        #[command(flatten)]
        rev: rev_select::RevSelectArgs,
        #[command(flatten)]
        pathspecs: pathspec_args::TrailingPathspecArgs,
        /// Continue a paged walk from the `TOKEN` a previous page printed (an empty value is ignored).
        #[arg(
            long = "cursor",
            value_name = "TOKEN",
            num_args = 0..=1,
            default_missing_value = ""
        )]
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
        /// Draw a commit graph in human output (implies topological order; Unicode by default).
        #[arg(long)]
        graph: bool,
        /// Write straight to the terminal instead of through the pager.
        #[arg(long = "no-pager")]
        no_pager: bool,
        /// Use ASCII `*|` lane characters in `--graph` output instead of Unicode box-drawing.
        #[arg(long = "graph-ascii")]
        graph_ascii: bool,
        /// Draw `--graph` with Unicode box-drawing (default when `--graph` is set).
        #[arg(long = "graph-unicode", conflicts_with = "graph_ascii")]
        graph_unicode: bool,
    },
    /// List or count commits matching revision ranges.
    Revs {
        #[command(flatten)]
        rev: rev_select::RevSelectArgs,
        #[command(flatten)]
        pathspecs: pathspec_args::TrailingPathspecArgs,
        /// Print only the number of reachable commits.
        #[arg(long)]
        count: bool,
        /// Mark each commit of a symmetric range (`A...B`) as left or right; with `--count`,
        /// print left/right counts instead.
        #[arg(long = "left-right")]
        left_right: bool,
        /// Pair the two sides of a symmetric range (`A...B`) by patch-id and mark
        /// equivalent commits (implies `--left-right`).
        #[arg(long = "cherry-mark", conflicts_with = "count")]
        cherry_mark: bool,
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
        #[arg(short = 'n', long = "max-count", alias = "limit", value_name = "N")]
        max_count: Option<usize>,
        /// Exclude entries older than this date (Git `--since`).
        #[arg(long = "since", value_name = "DATE")]
        since: Option<String>,
        /// Alias for `--since`.
        #[arg(long = "after", value_name = "DATE")]
        after: Option<String>,
        /// Exclude entries newer than this date (Git `--until`).
        #[arg(long = "until", value_name = "DATE")]
        until: Option<String>,
        /// Alias for `--until`.
        #[arg(long = "before", value_name = "DATE")]
        before: Option<String>,
        /// Keep entries whose message contains this substring (case-sensitive).
        #[arg(long = "grep", value_name = "PAT")]
        grep: Option<String>,
        #[command(subcommand)]
        action: Option<ReflogAction>,
    },
    /// Show who last modified each line of a file.
    Blame {
        /// Optional revision before the file (use `--` to disambiguate paths that look like refs).
        #[arg(value_name = "REV")]
        rev_pos: Option<String>,
        /// File to blame (repository-relative).
        #[arg(value_name = "FILE", last = true, allow_hyphen_values = true)]
        file: Option<String>,
        /// Blame the file as committed at this revision. Without a revision the working
        /// tree file is blamed, with uncommitted lines marked.
        #[arg(long)]
        rev: Option<String>,
        /// Limit output with a Git-style line range (`-L` / `--lines`).
        #[arg(
            short = 'L',
            long = "lines",
            value_name = "RANGE",
            allow_hyphen_values = true
        )]
        line_range: Option<String>,
        /// At merge commits, follow only the first parent (Git's `--first-parent`).
        #[arg(long = "first-parent")]
        first_parent: bool,
        /// Ignore this revision when attributing lines (repeatable).
        #[arg(long = "ignore-rev", value_name = "REV")]
        ignore_rev: Vec<String>,
        /// Read revision oids to ignore from a file (repeatable; `""` clears prior entries).
        #[arg(long = "ignore-revs-file", value_name = "FILE")]
        ignore_revs_file: Vec<String>,
        /// With `--json`, emit grouped `hunks[]` instead of per-line `lines[]`.
        #[arg(long)]
        group: bool,
        /// Ignore whitespace when comparing lines (`git blame -w`).
        #[arg(short = 'w', long = "ignore-all-space")]
        ignore_all_space: bool,
        /// Detect lines copied from other files (`git blame -C`; repeat up to three times).
        #[arg(short = 'C', long = "copy-detection", action = clap::ArgAction::Count)]
        copy_detection: u8,
        /// Detect lines moved within the same file (`git blame -M`). Enabled automatically when
        /// `-C` is used; Git treats copy detection as implying move detection.
        #[arg(short = 'M', long = "move-detection")]
        move_detection: bool,
        /// Blame this file's content (`-` for stdin) instead of the working tree, on top of
        /// the revision (default `HEAD`); lines not in the revision show as uncommitted.
        #[arg(long, value_name = "FILE")]
        contents: Option<String>,
    },
    /// Nearest reachable tag, exact commit distance, and abbreviated id for a revision.
    #[command(name = "describe")]
    Describe {
        #[command(flatten)]
        describe: crate::commands::describe::DescribeCliArgs,
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
        #[command(flatten)]
        pathspecs: pathspec_args::TrailingPathspecArgs,
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
        #[command(flatten)]
        pathspecs: pathspec_args::TrailingPathspecArgs,
        #[command(flatten)]
        format: diff_format::DiffFormatArgs,
    },
    /// Show what's changed and where you are (this is the default).
    #[command(alias = "st")]
    Status {
        #[command(flatten)]
        status: crate::commands::status::StatusCliArgs,
    },
    /// List the commits on this branch that aren't on the target branch yet.
    #[command(alias = "sl", visible_alias = "ahead")]
    Shortlog {
        #[command(flatten)]
        shortlog: crate::commands::shortlog::ShortlogCliArgs,
    },
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
        /// Stage only new-side line ranges from `grit diff --json` (`path:start-end`, repeatable).
        #[arg(long = "lines", value_name = "PATH:START-END", action = clap::ArgAction::Append)]
        lines: Vec<String>,
        /// Unified context lines when resolving `--hunks` / `--lines` (match `grit diff -U`; default 3).
        #[arg(short = 'U', long = "unified", value_name = "N")]
        unified_context: Option<usize>,
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
        /// List paths that would be removed without changing the index or work tree.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Treat non-matching pathspecs as success with an empty result.
        #[arg(long = "ignore-unmatch")]
        ignore_unmatch: bool,
    },
    /// Rename or move tracked paths.
    Mv {
        /// Source path(s) and final destination (last argument).
        #[arg(value_name = "PATH")]
        paths: Vec<String>,
        /// Replace existing files at the destination.
        #[arg(short = 'f', long = "force")]
        force: bool,
        /// List moves that would happen without changing the index or work tree.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Remove untracked files from the working tree.
    Clean {
        #[command(flatten)]
        pathspecs: pathspec_args::PathspecArgs,
        /// Actually delete files. Without this flag, only prints what would be removed.
        #[arg(short, long, conflicts_with = "dry_run")]
        force: bool,
        /// Explicit dry run (default when `-f` is not passed).
        #[arg(long = "dry-run", conflicts_with = "force")]
        dry_run: bool,
        /// Remove ignored files as well (like `git clean -x`).
        #[arg(long, conflicts_with = "only_ignored")]
        ignored: bool,
        /// Remove only ignored files (like `git clean -X`).
        #[arg(long = "only-ignored", conflicts_with = "ignored")]
        only_ignored: bool,
        /// Additional exclude patterns; matching paths are not removed.
        #[arg(short = 'e', long = "exclude", value_name = "PATTERN", action = clap::ArgAction::Append)]
        exclude: Vec<String>,
        /// Remove only files, not untracked directories.
        #[arg(long = "files-only")]
        files_only: bool,
    },
    /// Pack reachable objects and remove redundant packs.
    Repack {
        /// Recompute every delta instead of reusing the ones already in packs.
        #[arg(short, long)]
        force: bool,
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
        /// Run only when `gc.auto` or `gc.autoPackLimit` is exceeded (what commands run on their own).
        #[arg(long = "auto")]
        auto: bool,
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
        #[command(flatten)]
        limit: limit_args::LimitArgs,
        #[command(subcommand)]
        action: Option<HooksAction>,
    },
    /// Print stable patch-IDs for commits or revision ranges.
    #[command(name = "patch-id")]
    PatchId {
        /// Commits or ranges to hash; `A..B` and `A...B` expand to one ID per commit
        /// (merge commits are skipped).
        #[arg(value_name = "REV")]
        revs: Vec<String>,
        /// Use legacy unstable patch-ID algorithm (Git 1.9 and older).
        #[arg(long)]
        unstable: bool,
    },
    /// Resolve revision specs to object ids and types.
    Resolve {
        /// Revision specs to resolve (same forms as history commands).
        #[arg(
            value_name = "SPEC",
            required_unless_present = "verify",
            num_args = 1..,
            id = "specs",
            conflicts_with = "verify"
        )]
        specs: Vec<String>,
        /// Print abbreviated object ids.
        #[arg(long)]
        short: bool,
        /// Print the shortest unambiguous ref name for each spec (like `git rev-parse --abbrev-ref`).
        #[arg(long = "abbrev-ref", conflicts_with = "symbolic_full_name")]
        abbrev_ref: bool,
        /// Print the full symbolic ref name for each spec (like `git rev-parse --symbolic-full-name`).
        #[arg(long = "symbolic-full-name", conflicts_with = "abbrev_ref")]
        symbolic_full_name: bool,
        /// Resolve exactly one spec strictly (`git rev-parse --verify`); pass the spec as the flag value.
        #[arg(
            long = "verify",
            value_name = "SPEC",
            num_args = 1,
            id = "verify",
            conflicts_with_all = ["abbrev_ref", "symbolic_full_name", "specs"]
        )]
        verify_spec: Option<String>,
        /// With `--verify`, answer yes/no via exit status only (no stdout/stderr).
        #[arg(short = 'q', long = "quiet", requires = "verify")]
        quiet: bool,
    },
    /// Show blob, tree, commit, or tag contents for a revision spec.
    Cat {
        /// Object or revision spec (`HEAD`, `main:path`, `v1^{commit}`, …).
        spec: String,
    },
    /// Stage every change and record a new commit.
    Commit {
        /// Commit message (you can also pass it with -m).
        #[arg(
            id = "commit_message",
            value_name = "MESSAGE",
            conflicts_with_all = ["message", "message_file"],
            allow_hyphen_values = true,
            value_parser = clap_value_args::FreeTextPositional::new("-m")
        )]
        commit_message: Option<String>,
        #[command(flatten)]
        message_input: message_input::MessageInputArgs,
        #[command(flatten)]
        quiet: quiet_args::QuietArgs,
        #[command(flatten)]
        author: message_input::AuthorArg,
        /// Override the author date (Git date formats, RFC 3339, `@epoch`, …).
        #[arg(long = "date")]
        author_date: Option<String>,
        /// Append a commit trailer (`key=value` or `key: value`); repeat for multiple trailers.
        #[arg(long = "trailer", value_name = "KEY=VALUE", allow_hyphen_values = true)]
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
        /// Apply a unified diff to the index, then commit only that staged content (`git apply --cached` + commit).
        #[arg(
            long = "patch-file",
            value_name = "FILE|-",
            conflicts_with_all = ["hunks", "lines", "all", "staged", "paths", "pathspec_from_file"]
        )]
        patch_file: Option<String>,
        /// Commit only the listed hunk ids from `grit diff --json` (comma-separated, repeatable).
        #[arg(
            long = "hunks",
            value_name = "ID,...",
            action = clap::ArgAction::Append,
            conflicts_with_all = ["patch_file", "all", "staged"]
        )]
        hunks: Vec<String>,
        /// Commit only line ranges from the unstaged diff (same format as `grit add --lines`).
        #[arg(
            long = "lines",
            value_name = "PATH:START-END",
            action = clap::ArgAction::Append,
            conflicts_with_all = ["patch_file", "all", "staged"]
        )]
        lines: Vec<String>,
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
        #[command(flatten)]
        list: branch_list_args::BranchListArgs,
        /// Preview the operation without updating refs, config, or reflogs.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Switch to another branch, remote branch, or detached commit.
    #[command(alias = "co")]
    Switch {
        /// Branch to switch to, revision with `--detach`, or new branch name with `-c`.
        #[arg(value_name = "NAME", required_unless_present = "detach")]
        branch: Option<String>,
        /// With `-c`, the commit to start the branch from (defaults to `HEAD`).
        start_point: Option<String>,
        /// Create the branch first, then switch to it.
        #[arg(short = 'c', long = "create")]
        create: bool,
        /// Configure upstream from the start point when creating with `-c`.
        #[arg(
            short = 't',
            long = "track",
            conflicts_with = "no_track",
            requires = "create",
            conflicts_with_all = ["detach"]
        )]
        track: bool,
        /// Do not configure upstream when creating with `-c`.
        #[arg(
            long = "no-track",
            conflicts_with = "track",
            requires = "create",
            conflicts_with_all = ["detach"]
        )]
        no_track: bool,
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
        #[arg(long, conflicts_with_all = ["hard", "mixed", "keep"])]
        soft: bool,
        /// Reset the index to the target commit; keep the worktree (default).
        #[arg(long, conflicts_with_all = ["hard", "soft", "keep"])]
        mixed: bool,
        /// Reset the index and worktree to match the target commit.
        #[arg(long, conflicts_with_all = ["soft", "mixed", "keep"])]
        hard: bool,
        /// Reset the tip and update differing paths while keeping unrelated local edits.
        #[arg(long, conflicts_with_all = ["soft", "mixed", "hard"])]
        keep: bool,
        /// Preview the reset without updating refs, the index, or the worktree.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Maximum orphaned commits to list in `--json` output.
        #[arg(short = 'n', long = "limit", value_name = "N")]
        orphan_limit: Option<usize>,
    },
    /// Merge another branch, tag or commit into the current branch.
    Merge {
        /// Branch, tag or commit to merge (any revision, e.g. `v1.2`, a commit id, `topic~2`).
        #[arg(
            value_name = "REV",
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
        /// Predict whether the merge would succeed without changing refs, index, or worktree.
        #[arg(long = "check", conflicts_with_all = ["abort", "continue_merge", "dry_run"])]
        check: bool,
        /// Report the planned merge outcome without changing refs, index, or worktree.
        #[arg(long = "dry-run", conflicts_with_all = ["abort", "continue_merge", "check"])]
        dry_run: bool,
        /// Refuse when a fast-forward is not possible (exit code 128).
        #[arg(
            long = "ff-only",
            conflicts_with_all = ["no_ff", "continue_merge", "check", "dry_run"]
        )]
        ff_only: bool,
        /// Record a merge commit even when a fast-forward is possible.
        #[arg(
            long = "no-ff",
            conflicts_with_all = ["ff_only", "continue_merge", "check", "dry_run"]
        )]
        no_ff: bool,
        /// Apply the merge to the index and worktree but do not commit yet.
        #[arg(
            long = "no-commit",
            conflicts_with_all = ["squash", "continue_merge", "check", "dry_run"]
        )]
        no_commit: bool,
        /// Squash merged commits into staged changes and write `SQUASH_MSG`.
        #[arg(
            long = "squash",
            conflicts_with_all = ["no_commit", "continue_merge", "check", "dry_run"]
        )]
        squash: bool,
        /// Merge histories that share no commits (uses the empty tree as merge base).
        #[arg(long = "allow-unrelated-histories")]
        allow_unrelated_histories: bool,
        /// Skip the `pre-merge-commit` and `commit-msg` hooks (`prepare-commit-msg` and `post-merge` still run).
        #[arg(
            long = "no-verify",
            conflicts_with_all = ["abort", "continue_merge", "check", "dry_run"]
        )]
        no_verify: bool,
        #[command(flatten)]
        strategy: merge_strategy_args::MergeStrategyArgs,
        #[command(flatten)]
        message: message_input::MessageInputArgs,
    },
    /// Cherry-pick commits onto the current branch.
    Pick {
        /// Commits or ranges to pick (`A..B`, branch tips, oids). Omit with `--continue`, `--abort`, or `--skip`.
        #[arg(
            required_unless_present_any = ["continue_pick", "abort", "skip"],
            value_name = "COMMIT",
            conflicts_with_all = ["continue_pick", "abort", "skip"]
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
        /// Append `(cherry picked from commit …)` to each new commit message.
        #[arg(short = 'x', long = "record-origin")]
        record_origin: bool,
        /// Apply the pick to the index only; do not create a commit yet.
        #[arg(long = "no-commit")]
        no_commit: bool,
        /// When a pick step would not change the tree: stop (default), keep an empty commit, or drop the step.
        #[arg(long = "empty", value_enum, default_value_t = crate::commands::replay_merge_args::EmptyStepArg::Stop)]
        empty: crate::commands::replay_merge_args::EmptyStepArg,
        #[command(flatten)]
        message: message_input::MessageInputArgs,
        /// Predict whether the pick would succeed without changing refs, the index, or the worktree.
        #[arg(
            long = "check",
            conflicts_with_all = ["continue_pick", "abort", "skip", "dry_run"]
        )]
        check: bool,
        /// List commits that would be picked without changing refs or the worktree.
        #[arg(long = "dry-run", conflicts_with = "check")]
        dry_run: bool,
        #[command(flatten)]
        replay_merge: crate::commands::replay_merge_args::ReplayMergeArgs,
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
        #[arg(long = "empty", value_enum, default_value_t = crate::commands::replay_merge_args::EmptyStepArg::Stop)]
        empty: crate::commands::replay_merge_args::EmptyStepArg,
        #[command(flatten)]
        message: message_input::MessageInputArgs,
        /// Predict whether the revert would succeed without changing refs, the index, or the worktree.
        #[arg(
            long = "check",
            conflicts_with_all = ["continue_revert", "abort", "skip", "dry_run"]
        )]
        check: bool,
        /// List commits that would be reverted without changing refs or the worktree.
        #[arg(long = "dry-run", conflicts_with = "check")]
        dry_run: bool,
        #[command(flatten)]
        replay_merge: crate::commands::replay_merge_args::ReplayMergeArgs,
    },
    /// Rewrite commits without a sequencer (plan, then apply refs atomically).
    History {
        #[command(subcommand)]
        action: HistoryAction,
    },
    /// Save or restore shelved work.
    Stash {
        /// Stash message (bare `grit stash` / `grit stash push`).
        #[arg(
            short = 'm',
            long = "message",
            global = true,
            allow_hyphen_values = true
        )]
        message: Option<String>,
        /// Include untracked files (bare `grit stash` / `grit stash push`).
        #[arg(
            short = 'u',
            long = "include-untracked",
            global = true,
            conflicts_with_all = ["hunks", "lines"]
        )]
        include_untracked: bool,
        /// Stash only unstaged hunks from `grit diff --json` (comma-separated, repeatable).
        #[arg(
            long = "hunks",
            value_name = "ID,...",
            action = clap::ArgAction::Append,
            global = true,
            conflicts_with = "include_untracked"
        )]
        hunks: Vec<String>,
        /// Stash only unstaged line ranges (`path:start-end`, same as `grit add --lines`).
        #[arg(
            long = "lines",
            value_name = "PATH:START-END",
            action = clap::ArgAction::Append,
            global = true,
            conflicts_with = "include_untracked"
        )]
        lines: Vec<String>,
        /// Preview stash without updating refs, index, or worktree.
        #[arg(long = "dry-run", global = true)]
        dry_run: bool,
        /// Stash only index changes vs `HEAD` (unstaged edits on other paths remain).
        #[arg(
            short = 'S',
            long = "staged",
            global = true,
            conflicts_with_all = ["keep_index", "hunks", "lines", "include_untracked"]
        )]
        staged: bool,
        /// After stashing, leave the index and worktree matching staged content.
        #[arg(
            short = 'k',
            long = "keep-index",
            global = true,
            conflicts_with_all = ["staged", "hunks", "lines"]
        )]
        keep_index: bool,
        /// Cap path lists in `--dry-run` preview output.
        #[arg(short = 'n', long = "limit", global = true, value_name = "N")]
        stash_limit: Option<usize>,
        #[command(flatten)]
        pathspecs: crate::pathspec_args::TrailingPathspecArgs,
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
        #[arg(short = 's', long = "source")]
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
        conflict_style: Option<crate::conflict_style::ConflictStyleArg>,
        /// Discard only the listed unstaged hunk ids (`grit diff --unstaged --json`) or unstage staged ids (`--staged`).
        #[arg(long = "hunks", value_name = "ID,...", action = clap::ArgAction::Append)]
        hunks: Vec<String>,
        /// Discard or unstage only new-side line ranges (`path:start-end` from `grit diff --json`).
        #[arg(long = "lines", value_name = "PATH:START-END", action = clap::ArgAction::Append)]
        lines: Vec<String>,
        /// Report paths that would change without writing the worktree or index.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Download refs and objects from a remote.
    Fetch {
        #[command(flatten)]
        fetch: crate::commands::fetch_args::FetchArgs,
    },
    /// Fetch from the remote and integrate it into the current branch.
    ///
    /// In a snapshot checkout, refresh it onto the server's newest snapshot;
    /// `grit pull <branch>` brings `<branch>` onto the server's branch of that
    /// name, as the checkout sees it.
    Pull {
        #[command(flatten)]
        fetch: crate::commands::fetch_args::FetchArgs,
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
    /// Replay local commits onto a new base (atomic ref update).
    Rebase {
        /// Commits to replay are those reachable from `HEAD` but not from this revision (defaults to `@{upstream}`).
        #[arg(value_name = "UPSTREAM")]
        upstream: Option<String>,
        /// Replay onto this commit instead of `UPSTREAM`.
        #[arg(long = "onto", value_name = "REV")]
        onto: Option<String>,
        /// Finish a rebase stopped on conflicts after resolving and staging paths.
        #[arg(long = "continue", conflicts_with_all = ["abort", "skip", "upstream", "onto", "check"])]
        continue_rebase: bool,
        /// Cancel an in-progress rebase and restore the branch to its pre-rebase tip.
        #[arg(long = "abort", conflicts_with_all = ["continue_rebase", "skip", "upstream", "onto", "check"])]
        abort: bool,
        /// Skip the current commit and replay the rest.
        #[arg(long = "skip", conflicts_with_all = ["continue_rebase", "abort", "upstream", "onto", "check"])]
        skip: bool,
        #[command(flatten)]
        rebase_flags: crate::commands::history::RebaseRunFlags,
        #[command(flatten)]
        shared: crate::commands::history::HistoryRewriteArgs,
    },
    /// Publish refs to a remote.
    Push {
        /// Push tags instead of the current branch. Pushes every local tag
        /// under `refs/tags/` to the remote.
        #[arg(short = 't', long = "tags")]
        tags: bool,
        /// Delete the named remote branches or tags instead of pushing.
        #[arg(
            long = "delete",
            conflicts_with_all = ["tags", "all", "mirror"]
        )]
        delete: bool,
        /// Push every local branch (`refs/heads/*:refs/heads/*`).
        #[arg(
            long = "all",
            conflicts_with_all = ["tags", "mirror", "refspecs"]
        )]
        all: bool,
        /// Mirror all refs under `refs/` and delete stale remote refs (implies force).
        #[arg(
            long = "mirror",
            conflicts_with_all = ["tags", "all", "refspecs", "delete"]
        )]
        mirror: bool,
        /// Delete remote branch refs that have no local counterpart in this push.
        #[arg(long = "prune", conflicts_with = "mirror")]
        prune: bool,
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
        /// Skip the `pre-push` hook.
        #[arg(long = "no-verify")]
        no_verify: bool,
        #[command(flatten)]
        quiet: quiet_args::QuietArgs,
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
        /// Preview `add` or `remove` without writing objects or refs.
        #[arg(long = "dry-run", global = true)]
        dry_run: bool,
        #[command(flatten)]
        limit: limit_args::LimitArgs,
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
        /// List tags matching `PATTERN` (repeatable). Omit `PATTERN` to list all tags.
        #[arg(
            short = 'l',
            long = "list",
            value_name = "PATTERN",
            num_args = 0..=1,
            default_missing_value = "",
            action = clap::ArgAction::Append
        )]
        list: Vec<String>,
        /// Sort listed tags by version, date, or name.
        #[arg(long = "sort", value_enum, default_value_t = crate::commands::tag::TagListSort::Version)]
        sort: crate::commands::tag::TagListSort,
        /// Reverse the sort order.
        #[arg(long = "reverse")]
        reverse: bool,
        /// Only tags whose peeled commit contains this revision (`HEAD` when omitted).
        #[arg(long = "contains", value_name = "REV", default_missing_value = "HEAD", num_args = 0..=1)]
        contains: Option<String>,
        /// Only tags pointing at this object (repeatable).
        #[arg(long = "points-at", value_name = "REV")]
        points_at: Vec<String>,
        #[command(flatten)]
        limit: limit_args::LimitArgs,
        /// Delete the named tag instead of creating it.
        #[arg(short = 'd', long = "delete")]
        delete: bool,
        #[command(flatten)]
        message: message_input::MessageInputArgs,
        /// Git habit: `-m` / `-F` already write an annotated tag; ignored when a message is set.
        #[arg(
            short = 'a',
            long = "annotate",
            conflicts_with_all = ["delete", "verify"]
        )]
        annotate: bool,
        /// Deprecated alias for `--message-file`.
        #[arg(
            long = "file",
            value_name = "PATH|-",
            hide = true,
            conflicts_with_all = ["message", "message_file"],
            allow_hyphen_values = true
        )]
        legacy_message_file: Option<String>,
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
                "object",
                "tree_sha256",
                "exclude_submodule"
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
        /// Attach a tree-sha256 content digest to the tag (implies `--sign`).
        #[arg(long = "tree-sha256", conflicts_with = "verify")]
        tree_sha256: bool,
        /// Submodule path globs excluded from the digest (repeatable).
        #[arg(
            long = "exclude-submodule",
            value_name = "GLOB",
            conflicts_with = "verify"
        )]
        exclude_submodule: Vec<String>,
    },
    /// List, add, or remove linked worktrees.
    Worktree {
        #[command(subcommand)]
        action: WorktreeAction,
    },
    /// Sign in to grithub (GitHub inside a github.com repository), a `grit serve` host, or store a username and token for any HTTPS host.
    #[command(args_conflicts_with_subcommands = true, visible_alias = "login")]
    Auth {
        /// The host (or one of its repository URLs) to sign in to; without it, grithub.dev, or GitHub when `origin` is on github.com.
        #[arg(value_name = "URL")]
        url: Option<String>,
        /// Store this access token: with `--username`, for any HTTP(S) host; alone, a `grit serve` or grithub token, checked with the server.
        #[arg(long, value_name = "TOKEN")]
        token: Option<String>,
        /// Store `--token` for this user name on any HTTP(S) host, without signing in.
        #[arg(long, value_name = "USER", requires_all = ["url", "token"])]
        username: Option<String>,
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
        #[arg(long, conflicts_with_all = ["local", "system", "worktree", "file"])]
        global: bool,
        /// Read or write only this repository's config file (`.git/config`).
        #[arg(long, conflicts_with_all = ["global", "system", "worktree", "file"])]
        local: bool,
        /// Read or write the system config file (`/etc/gitconfig` or `GIT_CONFIG_SYSTEM`).
        #[arg(long, conflicts_with_all = ["global", "local", "worktree", "file"])]
        system: bool,
        /// Read or write this worktree's `config.worktree` (requires `extensions.worktreeConfig`).
        #[arg(long, conflicts_with_all = ["global", "local", "system", "file"])]
        worktree: bool,
        /// Read or write only the given config file (no repository required).
        #[arg(
            short = 'f',
            long = "file",
            value_name = "PATH",
            conflicts_with_all = ["global", "local", "system", "worktree"]
        )]
        file: Option<std::path::PathBuf>,
        /// List configuration values (optional `<prefix>` filters keys).
        #[arg(
            short = 'l',
            long = "list",
            conflicts_with_all = [
                "all",
                "add",
                "unset",
                "unset_all",
                "rename_section",
                "rename_section_new",
                "remove_section",
                "dry_run"
            ]
        )]
        list: bool,
        /// Return every value for a multi-valued key.
        #[arg(
            long,
            conflicts_with_all = [
                "list",
                "add",
                "unset",
                "unset_all",
                "rename_section",
                "rename_section_new",
                "remove_section",
                "dry_run"
            ]
        )]
        all: bool,
        /// Append a value without replacing existing entries.
        #[arg(
            long,
            conflicts_with_all = [
                "list",
                "unset",
                "unset_all",
                "all",
                "rename_section",
                "rename_section_new",
                "remove_section"
            ]
        )]
        add: bool,
        /// Remove the key instead of reading or setting it.
        #[arg(
            long,
            conflicts_with_all = [
                "list",
                "add",
                "unset_all",
                "all",
                "rename_section",
                "rename_section_new",
                "remove_section"
            ]
        )]
        unset: bool,
        /// Remove every occurrence of a multi-valued key.
        #[arg(
            long = "unset-all",
            conflicts_with_all = [
                "list",
                "add",
                "unset",
                "all",
                "rename_section",
                "rename_section_new",
                "remove_section"
            ]
        )]
        unset_all: bool,
        /// Current section name for `--rename-section`.
        #[arg(
            long = "rename-section",
            value_name = "OLD",
            requires = "rename_section_new",
            conflicts_with_all = ["list", "all", "add", "unset", "unset_all", "remove_section"]
        )]
        rename_section: Option<String>,
        /// New section name for `--rename-section`.
        #[arg(
            long = "rename-section-new",
            value_name = "NEW",
            requires = "rename_section",
            conflicts_with_all = ["list", "all", "add", "unset", "unset_all", "remove_section"]
        )]
        rename_section_new: Option<String>,
        /// Remove an entire section.
        #[arg(
            long = "remove-section",
            value_name = "SECTION",
            conflicts_with_all = [
                "list",
                "all",
                "add",
                "unset",
                "unset_all",
                "rename_section",
                "rename_section_new"
            ]
        )]
        remove_section: Option<String>,
        /// Coerce values as bool, int, or path (canonical on write).
        #[arg(
            long = "type",
            value_name = "TYPE",
            value_enum,
            conflicts_with_all = [
                "list",
                "unset",
                "unset_all",
                "rename_section",
                "rename_section_new",
                "remove_section"
            ]
        )]
        value_type: Option<crate::commands::config::ConfigTypeArg>,
        /// When reading, use this value if the key is unset.
        #[arg(
            long = "default",
            value_name = "VALUE",
            conflicts_with_all = [
                "list",
                "add",
                "unset",
                "unset_all",
                "rename_section",
                "rename_section_new",
                "remove_section",
                "dry_run"
            ]
        )]
        default_value: Option<String>,
        /// Show what would change without writing the config file.
        #[arg(
            long,
            conflicts_with_all = ["list", "all", "default_value"]
        )]
        dry_run: bool,
        /// The configuration key, e.g. user.name. Optional prefix with `--list`.
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
    /// Serve repositories over HTTP, with a web view where people sign in with email and password.
    Serve {
        #[command(subcommand)]
        action: Option<ServeAction>,
        /// A repository, or a directory of repositories, to serve.
        #[arg(long, global = true, value_name = "PATH", default_value = ".")]
        root: std::path::PathBuf,
        /// The user database (default: `grit-serve.db` in the root, or in its `.git`).
        #[arg(long, global = true, value_name = "FILE")]
        db: Option<std::path::PathBuf>,
        /// Address to listen on.
        #[arg(long, value_name = "HOST:PORT", default_value = "127.0.0.1:8080")]
        bind: String,
        /// Create the user database and its first admin user, then exit. Prompts for a password (or generates one).
        #[arg(long)]
        setup: bool,
        /// Email address of the admin `--setup` creates (asked for when omitted).
        #[arg(long, value_name = "EMAIL", requires = "setup")]
        email: Option<String>,
        /// How often to rebuild a changed repository's clone bundle, like `24h`, `30m` or `2d`.
        #[arg(
            long,
            value_name = "DURATION",
            default_value = "24h",
            value_parser = crate::commands::serve_http::parse_interval
        )]
        bundle_interval: std::time::Duration,
        /// Do not build or offer clone bundles.
        #[arg(long)]
        no_bundles: bool,
        /// Maximum time to wait for complete request headers on Git connections.
        #[arg(
            long,
            value_name = "DURATION",
            default_value = "30s",
            value_parser = crate::commands::serve_http::parse_interval
        )]
        header_timeout: std::time::Duration,
        /// Drop Git requests whose body or upload-pack response stalls this long between chunks.
        #[arg(
            long,
            value_name = "DURATION",
            default_value = "60s",
            value_parser = crate::commands::serve_http::parse_interval
        )]
        idle_timeout: std::time::Duration,
        /// Concurrent Git smart-HTTP requests before new ones get HTTP 503.
        #[arg(long, value_name = "N", default_value = "128")]
        max_git_requests: usize,
        /// After SIGTERM, SIGINT or SIGHUP, keep serving in-flight requests for this long before closing remaining connections (`0s` waits until they finish).
        #[arg(
            long,
            value_name = "DURATION",
            default_value = "30s",
            value_parser = crate::commands::serve_http::parse_interval
        )]
        shutdown_timeout: std::time::Duration,
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
pub(crate) struct ServeArgs {
    /// Answer a single request without advertising refs first (smart HTTP).
    #[arg(long)]
    pub(crate) stateless_rpc: bool,
    /// Print the ref advertisement and exit (smart HTTP discovery).
    #[arg(long, alias = "http-backend-info-refs")]
    pub(crate) advertise_refs: bool,
    /// The repository to serve.
    pub(crate) directory: String,
}
