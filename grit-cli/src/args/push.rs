//! Clap arguments for `grit push`.

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

/// Publish refs to a remote.
#[derive(Debug, Args)]
pub(crate) struct PushArgs {
    /// Push tags instead of the current branch. Pushes every local tag
    /// under `refs/tags/` to the remote.
    #[arg(short = 't', long = "tags")]
    pub(crate) tags: bool,
    /// Delete the named remote branches or tags instead of pushing.
    #[arg(
            long = "delete",
            conflicts_with_all = ["tags", "all", "mirror"]
        )]
    pub(crate) delete: bool,
    /// Push every local branch (`refs/heads/*:refs/heads/*`).
    #[arg(
            long = "all",
            conflicts_with_all = ["tags", "mirror", "refspecs"]
        )]
    pub(crate) all: bool,
    /// Mirror all refs under `refs/` and delete stale remote refs (implies force).
    #[arg(
            long = "mirror",
            conflicts_with_all = ["tags", "all", "refspecs", "delete"]
        )]
    pub(crate) mirror: bool,
    /// Delete remote branch refs that have no local counterpart in this push.
    #[arg(long = "prune", conflicts_with = "mirror")]
    pub(crate) prune: bool,
    /// Remote to push to (defaults to the current branch's remote or `origin`).
    #[arg(long, hide = true)]
    pub(crate) remote: Option<String>,
    /// Contact the remote but do not update refs or send objects.
    #[arg(long = "dry-run")]
    pub(crate) dry_run: bool,
    /// Overwrite remote refs even when the update is not a fast-forward.
    #[arg(short = 'f', long = "force", conflicts_with = "force_with_lease")]
    pub(crate) force: bool,
    /// Push only when the remote ref matches the local remote-tracking ref, or
    /// `=<rev>` when pushing exactly one ref.
    #[arg(
            long = "force-with-lease",
            value_name = "REV",
            num_args = 0..=1,
            require_equals = true,
            conflicts_with = "force"
        )]
    pub(crate) force_with_lease: Option<Option<String>>,
    /// Set `branch.<name>.remote` and `branch.<name>.merge` for each pushed branch.
    #[arg(short = 'u', long = "set-upstream")]
    pub(crate) set_upstream: bool,
    /// All ref updates succeed or none are applied.
    #[arg(long = "atomic")]
    pub(crate) atomic: bool,
    /// Server-side push option (repeatable; `GIT_PUSH_OPTION_*` in hooks).
    #[arg(short = 'o', long = "push-option", value_name = "OPTION")]
    pub(crate) push_option: Vec<String>,
    /// Push annotated tags reachable from the commits being pushed.
    #[arg(long = "follow-tags")]
    pub(crate) follow_tags: bool,
    /// Skip the `pre-push` hook.
    #[arg(long = "no-verify")]
    pub(crate) no_verify: bool,
    #[command(flatten)]
    pub(crate) quiet: quiet_args::QuietArgs,
    /// Remote name (when not using `--remote`).
    #[arg(value_name = "REMOTE")]
    pub(crate) remote_name: Option<String>,
    /// Refspec (`src`, `src:dst`, `:dst`, …).
    #[arg(value_name = "REFSPEC")]
    pub(crate) refspecs: Vec<String>,
}
