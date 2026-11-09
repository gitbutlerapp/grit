//! Clap arguments for `grit checkout`.

use clap::Args;

/// Start work on a repository from a snapshot of its main branch (or some of its paths) instead of a clone.
#[derive(Debug, Args)]
pub(crate) struct CheckoutArgs {
    /// The repository on a server that offers snapshot checkouts (`grit serve`).
    pub(crate) url: String,
    /// A slice (named set of paths) on the server to check out; `--path` adds to it.
    pub(crate) slice: Option<String>,
    /// Check out only this path, plus the files in the directories above it (repeatable).
    #[arg(short = 'p', long = "path", value_name = "PATH")]
    pub(crate) paths: Vec<String>,
    /// Directory to check out into (defaults to the repository name).
    #[arg(long, value_name = "DIR")]
    pub(crate) into: Option<std::path::PathBuf>,
}
