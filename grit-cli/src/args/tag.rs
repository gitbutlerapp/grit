//! Clap arguments for `grit tag`.

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

/// List tags, or create / delete one.
#[derive(Debug, Args)]
pub(crate) struct TagArgs {
    /// Name of the tag to create. Omit to list tags.
    #[arg(value_name = "NAME", conflicts_with = "verify")]
    pub(crate) name: Option<String>,
    /// Revision to tag (default `HEAD`).
    #[arg(value_name = "TARGET", conflicts_with_all = ["object", "verify"])]
    pub(crate) target: Option<String>,
    /// List tags matching `PATTERN` (repeatable). Omit `PATTERN` to list all tags.
    #[arg(
            short = 'l',
            long = "list",
            value_name = "PATTERN",
            num_args = 0..=1,
            default_missing_value = "",
            action = clap::ArgAction::Append
        )]
    pub(crate) list: Vec<String>,
    /// Sort listed tags by version, date, or name.
    #[arg(long = "sort", value_enum, default_value_t = crate::commands::tag::TagListSort::Version)]
    pub(crate) sort: crate::commands::tag::TagListSort,
    /// Reverse the sort order.
    #[arg(long = "reverse")]
    pub(crate) reverse: bool,
    /// Only tags whose peeled commit contains this revision (`HEAD` when omitted).
    #[arg(long = "contains", value_name = "REV", default_missing_value = "HEAD", num_args = 0..=1)]
    pub(crate) contains: Option<String>,
    /// Only tags pointing at this object (repeatable).
    #[arg(long = "points-at", value_name = "REV")]
    pub(crate) points_at: Vec<String>,
    #[command(flatten)]
    pub(crate) limit: limit_args::LimitArgs,
    /// Delete the named tag instead of creating it.
    #[arg(short = 'd', long = "delete")]
    pub(crate) delete: bool,
    #[command(flatten)]
    pub(crate) message: message_input::MessageInputArgs,
    /// Git habit: `-m` / `-F` already write an annotated tag; ignored when a message is set.
    #[arg(
            short = 'a',
            long = "annotate",
            conflicts_with_all = ["delete", "verify"]
        )]
    pub(crate) annotate: bool,
    /// Deprecated alias for `--message-file`.
    #[arg(
            long = "file",
            value_name = "PATH|-",
            hide = true,
            conflicts_with_all = ["message", "message_file"],
            allow_hyphen_values = true
        )]
    pub(crate) legacy_message_file: Option<String>,
    /// Object to tag (revision). Defaults to `HEAD`.
    #[arg(
        long = "object",
        value_name = "REV",
        hide = true,
        conflicts_with = "target"
    )]
    pub(crate) object: Option<String>,
    /// Replace an existing tag with the same name.
    #[arg(short = 'f', long = "force", conflicts_with = "delete")]
    pub(crate) force: bool,
    /// Preview create or delete without changing refs.
    #[arg(long = "dry-run")]
    pub(crate) dry_run: bool,
    /// Sign the tag object (implies annotated). Honors `tag.gpgSign` when omitted.
    #[arg(short = 's', long = "sign", conflicts_with = "no_sign")]
    pub(crate) sign: bool,
    /// Do not sign even when `tag.gpgSign` is true.
    #[arg(long = "no-sign", conflicts_with = "sign")]
    pub(crate) no_sign: bool,
    /// Signing key id or path (overrides `user.signingkey` for this tag).
    #[arg(long = "signing-key", value_name = "KEY")]
    pub(crate) signing_key: Option<String>,
    /// Verify annotated tag signatures and optional `tree-sha256` claims.
    #[arg(
            short = 'v',
            long = "verify",
            conflicts_with_all = [
                "delete",
                "message",
                "message_file",
                "force",
                "dry_run",
                "sign",
                "no_sign",
                "signing_key",
                "object",
                "tree_sha256",
                "exclude_submodule"
            ]
        )]
    pub(crate) verify: bool,
    /// Tag names to verify (one or more, with `--verify`).
    #[arg(
            id = "verify_names",
            value_name = "TAG",
            num_args = 1..,
            requires = "verify",
            last = true
        )]
    pub(crate) verify_names: Vec<String>,
    /// Skip signature verification (still checks `tree-sha256` when present).
    #[arg(long = "no-signature", requires = "verify")]
    pub(crate) no_signature: bool,
    /// Attach a tree-sha256 content digest to the tag (implies `--sign`).
    #[arg(long = "tree-sha256", conflicts_with = "verify")]
    pub(crate) tree_sha256: bool,
    /// Submodule path globs excluded from the digest (repeatable).
    #[arg(
        long = "exclude-submodule",
        value_name = "GLOB",
        conflicts_with = "verify"
    )]
    pub(crate) exclude_submodule: Vec<String>,
}
