//! Clap arguments for `grit auth`.

use super::AuthAction;
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

/// Sign in to grithub (GitHub inside a github.com repository), a `grit serve` host, or store a username and token for any HTTPS host.
#[derive(Debug, Args)]
pub(crate) struct AuthArgs {
    /// The host (or one of its repository URLs) to sign in to; without it, grithub.dev, or GitHub when `origin` is on github.com.
    #[arg(value_name = "URL")]
    pub(crate) url: Option<String>,
    /// Store this access token: with `--username`, for any HTTP(S) host; alone, a `grit serve` or grithub token, checked with the server.
    #[arg(long, value_name = "TOKEN")]
    pub(crate) token: Option<String>,
    /// Store `--token` for this user name on any HTTP(S) host, without signing in.
    #[arg(long, value_name = "USER", requires_all = ["url", "token"])]
    pub(crate) username: Option<String>,
    #[command(subcommand)]
    pub(crate) action: Option<AuthAction>,
}
