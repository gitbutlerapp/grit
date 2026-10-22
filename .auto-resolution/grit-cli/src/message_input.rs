//! Shared `-m` / `--message-file` argument group for mutating commands.

use anyhow::{Context, Result};
use clap::Args;
use std::io::Read;

/// Commit message from `-m` or `--message-file` (`-` reads stdin).
#[derive(Debug, Args, Clone, Default)]
pub struct MessageInputArgs {
    /// Commit message text.
    #[arg(short = 'm', long = "message", conflicts_with = "message_file")]
    pub message: Option<String>,
    /// Read the message from a file (`-` for stdin).
    #[arg(
        long = "message-file",
        value_name = "PATH|-",
        conflicts_with = "message"
    )]
    pub message_file: Option<String>,
}

/// Resolve a message from CLI flags; returns `None` when neither flag was passed.
pub fn read_message_input(args: &MessageInputArgs) -> Result<Option<String>> {
    match (&args.message, &args.message_file) {
        (Some(message), _) => Ok(Some(message.clone())),
        (None, None) => Ok(None),
        (None, Some(path)) if path == "-" => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("could not read message from stdin")?;
            Ok(Some(buf))
        }
        (None, Some(path)) => std::fs::read_to_string(path)
            .map(Some)
            .with_context(|| format!("could not read message file '{path}'")),
    }
}
