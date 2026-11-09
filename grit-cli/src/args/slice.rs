//! Clap arguments for `grit slice`.

use clap::{Args, Subcommand};

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

/// Named sets of paths to check out together, kept on the server's target branch.
#[derive(Debug, Args)]
pub(crate) struct SliceArgs {
    #[command(subcommand)]
    pub(crate) action: SliceAction,
    /// The repository whose slices to use (default: the checkout you are in).
    #[arg(long, global = true, value_name = "URL")]
    pub(crate) url: Option<String>,
}
