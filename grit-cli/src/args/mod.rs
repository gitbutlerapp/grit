//! Clap argument definitions for `grit`.

mod argv;

pub use argv::{normalize_log_limit_argv, normalize_tag_verify_argv, parse_cli};

mod add;
mod auth;
mod blame;
mod branch;
mod bundle;
mod cat;
mod checkin;
mod checkout;
mod clean;
mod clone;
mod commit;
mod completions;
mod config;
mod describe;
mod diff;
mod fetch;
mod fsck;
mod gc;
mod history;
mod hooks;
mod ignore;
mod init;
mod integrate;
mod log;
mod manager;
mod merge;
mod merge_base;
mod mv;
mod notes;
mod patch_id;
mod pick;
mod pull;
mod push;
mod rebase;
mod receive_pack;
mod reflog;
mod remote;
mod repack;
mod reset;
mod resolve;
mod restore;
mod revert;
mod revs;
mod rm;
mod serve;
mod share;
mod shortlog;
mod show;
mod skill;
mod slice;
mod stash;
mod status;
mod switch;
mod sync;
mod tag;
mod update;
mod upload_pack;
mod worktree;

pub(crate) use add::AddArgs;
pub(crate) use auth::AuthArgs;
pub(crate) use blame::BlameArgs;
pub(crate) use branch::BranchArgs;
pub(crate) use bundle::BundleArgs;
pub(crate) use cat::CatArgs;
pub(crate) use checkin::CheckinArgs;
pub(crate) use checkout::CheckoutArgs;
pub(crate) use clean::CleanArgs;
pub(crate) use clone::CloneArgs;
pub(crate) use commit::CommitArgs;
pub(crate) use completions::CompletionsArgs;
pub(crate) use config::ConfigArgs;
pub(crate) use describe::DescribeArgs;
pub(crate) use diff::DiffArgs;
pub(crate) use fetch::FetchArgs;
pub(crate) use fsck::FsckArgs;
pub(crate) use gc::GcArgs;
pub(crate) use history::HistoryArgs;
pub(crate) use hooks::HooksArgs;
pub(crate) use ignore::IgnoreArgs;
pub(crate) use init::InitArgs;
pub(crate) use integrate::IntegrateArgs;
pub(crate) use log::LogArgs;
pub(crate) use manager::ManagerArgs;
pub(crate) use merge::MergeArgs;
pub(crate) use merge_base::MergeBaseArgs;
pub(crate) use mv::MvArgs;
pub(crate) use notes::NotesArgs;
pub(crate) use patch_id::PatchIdArgs;
pub(crate) use pick::PickArgs;
pub(crate) use pull::PullArgs;
pub(crate) use push::PushArgs;
pub(crate) use rebase::RebaseArgs;
pub(crate) use receive_pack::ReceivePackArgs;
pub(crate) use reflog::ReflogArgs;
pub(crate) use remote::RemoteArgs;
pub(crate) use repack::RepackArgs;
pub(crate) use reset::ResetArgs;
pub(crate) use resolve::ResolveArgs;
pub(crate) use restore::RestoreArgs;
pub(crate) use revert::RevertArgs;
pub(crate) use revs::RevsArgs;
pub(crate) use rm::RmArgs;
pub(crate) use serve::ServeCommandArgs;
pub(crate) use share::ShareArgs;
pub(crate) use shortlog::ShortlogArgs;
pub(crate) use show::ShowArgs;
pub(crate) use skill::SkillArgs;
pub(crate) use slice::SliceArgs;
pub(crate) use stash::StashArgs;
pub(crate) use status::StatusArgs;
pub(crate) use switch::SwitchArgs;
pub(crate) use sync::SyncArgs;
pub(crate) use tag::TagArgs;
pub(crate) use update::UpdateArgs;
pub(crate) use upload_pack::UploadPackArgs;
pub(crate) use worktree::WorktreeArgs;

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
        #[command(flatten)]
        args: InitArgs,
    },
    /// Start work on a repository from a snapshot of its main branch (or some of its paths) instead of a clone.
    Checkout {
        #[command(flatten)]
        args: CheckoutArgs,
    },
    /// Send a snapshot checkout's branches back to the server, grafted onto its real history.
    Checkin {
        #[command(flatten)]
        args: CheckinArgs,
    },
    /// Merge a branch into the server's target branch (checking it in first), then refresh this checkout.
    Integrate {
        #[command(flatten)]
        args: IntegrateArgs,
    },
    /// Push and pull every active branch between this clone and your space on the server (grithub).
    Sync {
        #[command(flatten)]
        args: SyncArgs,
    },
    /// Work on a branch together with other users of the server: each has their own head of it.
    Share {
        #[command(flatten)]
        args: ShareArgs,
    },
    /// Named sets of paths to check out together, kept on the server's target branch.
    Slice {
        #[command(flatten)]
        args: SliceArgs,
    },
    /// Copy a remote repository into a new directory.
    ///
    /// Submodule gitlinks are checked out as empty directories only; grit does not fetch submodule
    /// repositories or provide a `submodule` command.
    Clone {
        #[command(flatten)]
        args: CloneArgs,
    },
    /// List remotes, or add one.
    Remote {
        #[command(flatten)]
        args: RemoteArgs,
    },
    /// Show recent commits reachable from HEAD.
    Log {
        #[command(flatten)]
        args: LogArgs,
    },
    /// List or count commits matching revision ranges.
    Revs {
        #[command(flatten)]
        args: RevsArgs,
    },
    /// Find merge bases or test whether one commit is an ancestor of another.
    #[command(name = "merge-base")]
    MergeBase {
        #[command(flatten)]
        args: MergeBaseArgs,
    },
    /// Show, delete, or expire reflog entries.
    Reflog {
        #[command(flatten)]
        args: ReflogArgs,
    },
    /// Show who last modified each line of a file.
    Blame {
        #[command(flatten)]
        args: BlameArgs,
    },
    /// Nearest reachable tag, exact commit distance, and abbreviated id for a revision.
    #[command(name = "describe")]
    Describe {
        #[command(flatten)]
        args: DescribeArgs,
    },
    /// Show changes as a diff. No argument: uncommitted changes; with a commit:
    /// the change that commit introduced; with two commits (or `A..B`) diff their trees.
    Diff {
        #[command(flatten)]
        args: DiffArgs,
    },
    /// Show information about a commit, tag, or branch (defaults to HEAD).
    Show {
        #[command(flatten)]
        args: ShowArgs,
    },
    /// Show what's changed and where you are (this is the default).
    #[command(alias = "st")]
    Status {
        #[command(flatten)]
        args: StatusArgs,
    },
    /// List the commits on this branch that aren't on the target branch yet.
    #[command(alias = "sl", visible_alias = "ahead")]
    Shortlog {
        #[command(flatten)]
        args: ShortlogArgs,
    },
    /// Stage changes. With no paths, stages everything.
    Add {
        #[command(flatten)]
        args: AddArgs,
    },
    /// Remove tracked paths from the working tree and index.
    Rm {
        #[command(flatten)]
        args: RmArgs,
    },
    /// Rename or move tracked paths.
    Mv {
        #[command(flatten)]
        args: MvArgs,
    },
    /// Remove untracked files from the working tree.
    Clean {
        #[command(flatten)]
        args: CleanArgs,
    },
    /// Pack reachable objects and remove redundant packs.
    Repack {
        #[command(flatten)]
        args: RepackArgs,
    },
    /// Pack refs, expire reflogs, repack, prune, and refresh commit-graph.
    Gc {
        #[command(flatten)]
        args: GcArgs,
    },
    /// Check refs, objects, and pack indexes for problems.
    #[command(alias = "verify")]
    Fsck {
        #[command(flatten)]
        args: FsckArgs,
    },
    /// Query exclude (gitignore) rules.
    Ignore {
        #[command(flatten)]
        args: IgnoreArgs,
    },
    /// List hook scripts or run one by name.
    Hooks {
        #[command(flatten)]
        args: HooksArgs,
    },
    /// Print stable patch-IDs for commits or revision ranges.
    #[command(name = "patch-id")]
    PatchId {
        #[command(flatten)]
        args: PatchIdArgs,
    },
    /// Resolve revision specs to object ids and types.
    Resolve {
        #[command(flatten)]
        args: ResolveArgs,
    },
    /// Show blob, tree, commit, or tag contents for a revision spec.
    Cat {
        #[command(flatten)]
        args: CatArgs,
    },
    /// Stage every change and record a new commit.
    Commit {
        #[command(flatten)]
        args: CommitArgs,
    },
    /// List branches, or create / delete / rename / configure one.
    Branch {
        #[command(flatten)]
        args: BranchArgs,
    },
    /// Switch to another branch, remote branch, or detached commit.
    #[command(alias = "co")]
    Switch {
        #[command(flatten)]
        args: SwitchArgs,
    },
    /// Move the branch tip and optionally reset the index or worktree.
    Reset {
        #[command(flatten)]
        args: ResetArgs,
    },
    /// Merge another branch, tag or commit into the current branch.
    Merge {
        #[command(flatten)]
        args: MergeArgs,
    },
    /// Cherry-pick commits onto the current branch.
    Pick {
        #[command(flatten)]
        args: PickArgs,
    },
    /// Undo commits with new inverse commits.
    Revert {
        #[command(flatten)]
        args: RevertArgs,
    },
    /// Rewrite commits without a sequencer (plan, then apply refs atomically).
    History {
        #[command(flatten)]
        args: HistoryArgs,
    },
    /// Save or restore shelved work.
    Stash {
        #[command(flatten)]
        args: StashArgs,
    },
    /// Restore working tree and/or index paths.
    Restore {
        #[command(flatten)]
        args: RestoreArgs,
    },
    /// Download refs and objects from a remote.
    Fetch {
        #[command(flatten)]
        args: FetchArgs,
    },
    /// Fetch from the remote and integrate it into the current branch.
    ///
    /// In a snapshot checkout, refresh it onto the server's newest snapshot;
    /// `grit pull <branch>` brings `<branch>` onto the server's branch of that
    /// name, as the checkout sees it.
    Pull {
        #[command(flatten)]
        args: PullArgs,
    },
    /// Replay local commits onto a new base (atomic ref update).
    Rebase {
        #[command(flatten)]
        args: RebaseArgs,
    },
    /// Publish refs to a remote.
    Push {
        #[command(flatten)]
        args: PushArgs,
    },
    /// Create, verify, or list git bundle files.
    Bundle {
        #[command(flatten)]
        args: BundleArgs,
    },
    /// List, show, add, or remove Git notes on commits.
    Notes {
        #[command(flatten)]
        args: NotesArgs,
    },
    /// List tags, or create / delete one.
    Tag {
        #[command(flatten)]
        args: TagArgs,
    },
    /// List, add, or remove linked worktrees.
    Worktree {
        #[command(flatten)]
        args: WorktreeArgs,
    },
    /// Sign in to grithub (GitHub inside a github.com repository), a `grit serve` host, or store a username and token for any HTTPS host.
    #[command(args_conflicts_with_subcommands = true, visible_alias = "login")]
    Auth {
        #[command(flatten)]
        args: AuthArgs,
    },
    /// Update grit to the latest release (re-runs the install script).
    Update {
        #[command(flatten)]
        args: UpdateArgs,
    },
    /// Print an agent skill (SKILL.md) that explains how to use grit.
    Skill {
        #[command(flatten)]
        args: SkillArgs,
    },
    /// Generate shell completion scripts for bash, zsh, or fish.
    Completions {
        #[command(flatten)]
        args: CompletionsArgs,
    },
    /// Read, set, or list configuration values.
    Config {
        #[command(flatten)]
        args: ConfigArgs,
    },
    /// Git credential helper backed by the Windows Credential Manager.
    ///
    /// You don't normally run this yourself — `grit auth` wires it into
    /// `credential.helper` on Windows. It speaks Git's credential protocol.
    Manager {
        #[command(flatten)]
        args: ManagerArgs,
    },
    /// Serve repositories over HTTP, with a web view where people sign in with email and password.
    Serve {
        #[command(flatten)]
        args: ServeCommandArgs,
    },
    /// Serve a fetch or clone over stdin/stdout (run by ssh and grit-http-server).
    #[command(name = "upload-pack", hide = true)]
    UploadPack {
        #[command(flatten)]
        args: UploadPackArgs,
    },
    /// Accept a push over stdin/stdout (run by ssh and grit-http-server).
    #[command(name = "receive-pack", hide = true)]
    ReceivePack {
        #[command(flatten)]
        args: ReceivePackArgs,
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
