//! Clap arguments for `grit commit`.

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
use clap::Args;

/// Stage every change and record a new commit.
#[derive(Debug, Args)]
pub(crate) struct CommitArgs {
    /// Commit message (you can also pass it with -m).
    #[arg(
            id = "commit_message",
            value_name = "MESSAGE",
            conflicts_with_all = ["message", "message_file"],
            allow_hyphen_values = true,
            value_parser = clap_value_args::FreeTextPositional::new("-m")
        )]
    pub(crate) commit_message: Option<String>,
    #[command(flatten)]
    pub(crate) message_input: message_input::MessageInputArgs,
    #[command(flatten)]
    pub(crate) quiet: quiet_args::QuietArgs,
    #[command(flatten)]
    pub(crate) author: message_input::AuthorArg,
    /// Override the author date (Git date formats, RFC 3339, `@epoch`, …).
    #[arg(long = "date")]
    pub(crate) author_date: Option<String>,
    /// Append a commit trailer (`key=value` or `key: value`); repeat for multiple trailers.
    #[arg(long = "trailer", value_name = "KEY=VALUE", allow_hyphen_values = true)]
    pub(crate) trailers: Vec<String>,
    /// Append a `Signed-off-by` trailer for the committer.
    #[arg(short = 's', long = "signoff")]
    pub(crate) signoff: bool,
    /// Set the author to the committer with a fresh author date.
    #[arg(long = "reset-author")]
    pub(crate) reset_author: bool,
    /// Reuse the log message and author from a commit (`-C` / `--reuse-message`).
    #[arg(short = 'C', long = "reuse-message", value_name = "COMMIT")]
    pub(crate) reuse_message: Option<String>,
    /// Sign the commit with the configured signing key.
    #[arg(short = 'S', long = "gpg-sign")]
    pub(crate) gpg_sign: bool,
    /// Do not sign this commit, even when `commit.gpgsign` is enabled.
    #[arg(long = "no-gpg-sign", conflicts_with = "gpg_sign")]
    pub(crate) no_gpg_sign: bool,
    /// Skip the `pre-commit` and `commit-msg` hooks.
    #[arg(long = "no-verify")]
    pub(crate) no_verify: bool,
    /// Stage every change first, then commit. This is the default when `commit.autoStage` is true.
    #[arg(
            short = 'a',
            long = "all",
            conflicts_with_all = ["staged", "paths", "pathspec_from_file"]
        )]
    pub(crate) all: bool,
    /// Commit only what is already staged (same as `git commit` without `-a`).
    #[arg(
            long = "staged",
            conflicts_with_all = ["all", "paths", "pathspec_from_file"]
        )]
    pub(crate) staged: bool,
    /// Replace the tip commit instead of creating a new one.
    #[arg(long = "amend")]
    pub(crate) amend: bool,
    /// Record a commit even when the tree matches the parent (same as `git commit --allow-empty`).
    #[arg(long = "allow-empty")]
    pub(crate) allow_empty: bool,
    /// Compute the commit without writing objects, updating refs, or running hooks.
    #[arg(long = "dry-run")]
    pub(crate) dry_run: bool,
    /// Apply a unified diff to the index, then commit only that staged content (`git apply --cached` + commit).
    #[arg(
            long = "patch-file",
            value_name = "FILE|-",
            conflicts_with_all = ["hunks", "lines", "all", "staged", "paths", "pathspec_from_file"]
        )]
    pub(crate) patch_file: Option<String>,
    /// Commit only the listed hunk ids from `grit diff --json` (comma-separated, repeatable).
    #[arg(
            long = "hunks",
            value_name = "ID,...",
            action = clap::ArgAction::Append,
            conflicts_with_all = ["patch_file", "all", "staged"]
        )]
    pub(crate) hunks: Vec<String>,
    /// Commit only line ranges from the unstaged diff (same format as `grit add --lines`).
    #[arg(
            long = "lines",
            value_name = "PATH:START-END",
            action = clap::ArgAction::Append,
            conflicts_with_all = ["patch_file", "all", "staged"]
        )]
    pub(crate) lines: Vec<String>,
    #[command(flatten)]
    pub(crate) paths: crate::pathspec_args::TrailingPathspecArgs,
}
