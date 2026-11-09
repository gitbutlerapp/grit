//! Clap arguments for `grit pull`.

use clap::Args;

/// Fetch from the remote and integrate it into the current branch.
///
/// In a snapshot checkout, refresh it onto the server's newest snapshot;
/// `grit pull <branch>` brings `<branch>` onto the server's branch of that
/// name, as the checkout sees it.
#[derive(Debug, Args)]
pub(crate) struct PullArgs {
    #[command(flatten)]
    pub(crate) fetch: crate::commands::fetch_args::FetchArgs,
    /// Refuse when a merge would be required (exit 1 when diverged).
    #[arg(long = "ff-only", conflicts_with_all = ["no_rebase", "rebase", "check"])]
    pub(crate) ff_only: bool,
    /// Merge instead of rebasing, overriding `pull.rebase`.
    #[arg(long = "no-rebase", conflicts_with_all = ["ff_only", "rebase", "check"])]
    pub(crate) no_rebase: bool,
    /// Rebase local commits onto the fetched upstream, overriding `pull.rebase=false`.
    #[arg(long = "rebase", conflicts_with_all = ["ff_only", "no_rebase", "check"])]
    pub(crate) rebase: bool,
    /// Stash dirty worktree/index changes around the pull (`merge.autoStash` / `rebase.autoStash`).
    #[arg(long = "autostash", conflicts_with = "check")]
    pub(crate) autostash: bool,
    /// Fetch and report whether a fast-forward is possible without changing the branch.
    #[arg(long = "check", conflicts_with_all = ["autostash", "ff_only", "no_rebase", "rebase"])]
    pub(crate) check: bool,
}
