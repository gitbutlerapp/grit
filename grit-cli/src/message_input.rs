//! Shared `-m` / `-F` / `--message-file` and `--author` argument groups for mutating commands.

use std::error::Error as StdError;
use std::fmt;
use std::io::Read;
use std::sync::Once;

use anyhow::{Context, Result};
use clap::Args;
use grit_lib::config::ident::parse_author_arg;
use grit_lib::ident::AuthorContact;
use grit_lib::porcelain::commit::normalize_commit_message;

use crate::error_kind::ErrorKind;
use crate::json_error::{StructuredCliError, StructuredJsonError};

static TAG_FILE_ALIAS_WARN: Once = Once::new();

/// Commit message from `-m` or `-F` / `--message-file` (`-` reads stdin).
#[derive(Debug, Args, Clone, Default)]
pub struct MessageInputArgs {
    /// Commit message text.
    #[arg(
        short = 'm',
        long = "message",
        conflicts_with = "message_file",
        allow_hyphen_values = true
    )]
    pub message: Option<String>,
    /// Read the message from a file (`-` for stdin).
    #[arg(
        short = 'F',
        long = "message-file",
        value_name = "PATH|-",
        conflicts_with = "message",
        allow_hyphen_values = true
    )]
    pub message_file: Option<String>,
}

/// Optional `--author "Name <email>"` override (commit, history reword, …).
#[derive(Debug, Args, Clone, Default)]
pub struct AuthorArg {
    /// Author ident (`Name <email>` or bare email).
    #[arg(long = "author", allow_hyphen_values = true)]
    pub author: Option<String>,
}

/// Usage mistakes for shared CLI flags (exit code 2, JSON `kind: usage`).
#[derive(Debug)]
pub struct MessageInputUsageError;

impl fmt::Display for MessageInputUsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid arguments")
    }
}

impl StdError for MessageInputUsageError {}

/// Whether `err` should map to exit code 2 (`kind: usage`).
pub fn is_message_input_usage_error(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.downcast_ref::<MessageInputUsageError>().is_some())
}

fn usage_error(message: impl Into<String>) -> anyhow::Error {
    let human = message.into();
    anyhow::Error::new(StructuredCliError {
        payload: StructuredJsonError {
            error: human.clone(),
            kind: ErrorKind::Usage,
            conflicts: None,
            commit: None,
            would_overwrite: None,
            oid: None,
            lock: None,
        },
        human,
    })
    .context(MessageInputUsageError)
}

/// Parse `--author` with grit-lib's ident rules; malformed input is a usage error (exit 2).
pub fn parse_author_cli(raw: &str) -> Result<AuthorContact> {
    parse_author_arg(raw).map_err(|e| usage_error(format!("{e}")))
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

/// Read message input and apply [`normalize_commit_message`] (Git `commit -F` stripspace rules).
///
/// # Errors
///
/// Returns a usage error when `-m` and `-F` would both apply (clap prevents this) or when the
/// normalized body would be empty.
pub fn read_normalized_message_input(args: &MessageInputArgs) -> Result<Option<String>> {
    let Some(raw) = read_message_input(args)? else {
        return Ok(None);
    };
    Ok(Some(
        normalize_commit_message(&raw).map_err(|e| usage_error(e.to_string()))?,
    ))
}

/// Like [`read_normalized_message_input`], but requires a non-empty message.
pub fn read_required_normalized_message(args: &MessageInputArgs) -> Result<String> {
    read_normalized_message_input(args)?
        .ok_or_else(|| usage_error("a message is required (-m or -F / --message-file)"))
}

/// Merge hidden `--file` (tag legacy) into [`MessageInputArgs`] and warn once.
pub fn merge_tag_legacy_file(
    mut input: MessageInputArgs,
    legacy_file: Option<String>,
) -> MessageInputArgs {
    if let Some(path) = legacy_file {
        TAG_FILE_ALIAS_WARN.call_once(|| {
            eprintln!("warning: `--file` is deprecated; use `-F` / `--message-file` instead");
        });
        if input.message_file.is_none() {
            input.message_file = Some(path);
        }
    }
    input
}
