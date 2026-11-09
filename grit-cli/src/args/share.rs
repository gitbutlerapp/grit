//! Clap arguments for `grit share`.

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
use clap::{Args, Subcommand};

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


/// Work on a branch together with other users of the server: each has their own head of it.
#[derive(Debug, Args)]
pub(crate) struct ShareArgs {
    #[command(subcommand)]
    pub(crate) action: ShareAction,
}
