//! Clap arguments for `grit reset`.

use clap::Args;

/// Move the branch tip and optionally reset the index or worktree.
#[derive(Debug, Args)]
pub(crate) struct ResetArgs {
    /// Commit to reset to (defaults to `HEAD`).
    #[arg(value_name = "REV")]
    pub(crate) rev: Option<String>,
    /// Keep the index and worktree; move the branch tip only.
    #[arg(long, conflicts_with_all = ["hard", "mixed", "keep"])]
    pub(crate) soft: bool,
    /// Reset the index to the target commit; keep the worktree (default).
    #[arg(long, conflicts_with_all = ["hard", "soft", "keep"])]
    pub(crate) mixed: bool,
    /// Reset the index and worktree to match the target commit.
    #[arg(long, conflicts_with_all = ["soft", "mixed", "keep"])]
    pub(crate) hard: bool,
    /// Reset the tip and update differing paths while keeping unrelated local edits.
    #[arg(long, conflicts_with_all = ["soft", "mixed", "hard"])]
    pub(crate) keep: bool,
    /// Preview the reset without updating refs, the index, or the worktree.
    #[arg(long = "dry-run")]
    pub(crate) dry_run: bool,
    /// Maximum orphaned commits to list in `--json` output.
    #[arg(short = 'n', long = "limit", value_name = "N")]
    pub(crate) orphan_limit: Option<usize>,
}
