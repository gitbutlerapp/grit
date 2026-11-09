//! Clap arguments for `grit serve`.

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


/// Serve repositories over HTTP, with a web view where people sign in with email and password.
#[derive(Debug, Args)]
pub(crate) struct ServeCommandArgs {
    #[command(subcommand)]
    pub(crate) action: Option<ServeAction>,
    /// A repository, or a directory of repositories, to serve.
    #[arg(long, global = true, value_name = "PATH", default_value = ".")]
    pub(crate) root: std::path::PathBuf,
    /// The user database (default: `grit-serve.db` in the root, or in its `.git`).
    #[arg(long, global = true, value_name = "FILE")]
    pub(crate) db: Option<std::path::PathBuf>,
    /// Address to listen on.
    #[arg(long, value_name = "HOST:PORT", default_value = "127.0.0.1:8080")]
    pub(crate) bind: String,
    /// Create the user database and its first admin user, then exit. Prompts for a password (or generates one).
    #[arg(long)]
    pub(crate) setup: bool,
    /// Email address of the admin `--setup` creates (asked for when omitted).
    #[arg(long, value_name = "EMAIL", requires = "setup")]
    pub(crate) email: Option<String>,
    /// How often to rebuild a changed repository's clone bundle, like `24h`, `30m` or `2d`.
    #[arg(
            long,
            value_name = "DURATION",
            default_value = "24h",
            value_parser = crate::commands::serve_http::parse_interval
        )]
    pub(crate) bundle_interval: std::time::Duration,
    /// Do not build or offer clone bundles.
    #[arg(long)]
    pub(crate) no_bundles: bool,
    /// Maximum time to wait for complete request headers on Git connections.
    #[arg(
            long,
            value_name = "DURATION",
            default_value = "30s",
            value_parser = crate::commands::serve_http::parse_interval
        )]
    pub(crate) header_timeout: std::time::Duration,
    /// Drop Git requests whose body or upload-pack response stalls this long between chunks.
    #[arg(
            long,
            value_name = "DURATION",
            default_value = "60s",
            value_parser = crate::commands::serve_http::parse_interval
        )]
    pub(crate) idle_timeout: std::time::Duration,
    /// Concurrent Git smart-HTTP requests before new ones get HTTP 503.
    #[arg(long, value_name = "N", default_value = "128")]
    pub(crate) max_git_requests: usize,
    /// After SIGTERM, SIGINT or SIGHUP, keep serving in-flight requests for this long before closing remaining connections (`0s` waits until they finish).
    #[arg(
            long,
            value_name = "DURATION",
            default_value = "30s",
            value_parser = crate::commands::serve_http::parse_interval
        )]
    pub(crate) shutdown_timeout: std::time::Duration,
}
