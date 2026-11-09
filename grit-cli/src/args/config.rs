//! Clap arguments for `grit config`.

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

/// Read, set, or list configuration values.
#[derive(Debug, Args)]
pub(crate) struct ConfigArgs {
    /// Use the global (per-user) config file instead of this repository's.
    #[arg(long, conflicts_with_all = ["local", "system", "worktree", "file"])]
    pub(crate) global: bool,
    /// Read or write only this repository's config file (`.git/config`).
    #[arg(long, conflicts_with_all = ["global", "system", "worktree", "file"])]
    pub(crate) local: bool,
    /// Read or write the system config file (`/etc/gitconfig` or `GIT_CONFIG_SYSTEM`).
    #[arg(long, conflicts_with_all = ["global", "local", "worktree", "file"])]
    pub(crate) system: bool,
    /// Read or write this worktree's `config.worktree` (requires `extensions.worktreeConfig`).
    #[arg(long, conflicts_with_all = ["global", "local", "system", "file"])]
    pub(crate) worktree: bool,
    /// Read or write only the given config file (no repository required).
    #[arg(
            short = 'f',
            long = "file",
            value_name = "PATH",
            conflicts_with_all = ["global", "local", "system", "worktree"]
        )]
    pub(crate) file: Option<std::path::PathBuf>,
    /// List configuration values (optional `<prefix>` filters keys).
    #[arg(
            short = 'l',
            long = "list",
            conflicts_with_all = [
                "all",
                "add",
                "unset",
                "unset_all",
                "rename_section",
                "rename_section_new",
                "remove_section",
                "dry_run"
            ]
        )]
    pub(crate) list: bool,
    /// Return every value for a multi-valued key.
    #[arg(
            long,
            conflicts_with_all = [
                "list",
                "add",
                "unset",
                "unset_all",
                "rename_section",
                "rename_section_new",
                "remove_section",
                "dry_run"
            ]
        )]
    pub(crate) all: bool,
    /// Append a value without replacing existing entries.
    #[arg(
            long,
            conflicts_with_all = [
                "list",
                "unset",
                "unset_all",
                "all",
                "rename_section",
                "rename_section_new",
                "remove_section"
            ]
        )]
    pub(crate) add: bool,
    /// Remove the key instead of reading or setting it.
    #[arg(
            long,
            conflicts_with_all = [
                "list",
                "add",
                "unset_all",
                "all",
                "rename_section",
                "rename_section_new",
                "remove_section"
            ]
        )]
    pub(crate) unset: bool,
    /// Remove every occurrence of a multi-valued key.
    #[arg(
            long = "unset-all",
            conflicts_with_all = [
                "list",
                "add",
                "unset",
                "all",
                "rename_section",
                "rename_section_new",
                "remove_section"
            ]
        )]
    pub(crate) unset_all: bool,
    /// Current section name for `--rename-section`.
    #[arg(
            long = "rename-section",
            value_name = "OLD",
            requires = "rename_section_new",
            conflicts_with_all = ["list", "all", "add", "unset", "unset_all", "remove_section"]
        )]
    pub(crate) rename_section: Option<String>,
    /// New section name for `--rename-section`.
    #[arg(
            long = "rename-section-new",
            value_name = "NEW",
            requires = "rename_section",
            conflicts_with_all = ["list", "all", "add", "unset", "unset_all", "remove_section"]
        )]
    pub(crate) rename_section_new: Option<String>,
    /// Remove an entire section.
    #[arg(
            long = "remove-section",
            value_name = "SECTION",
            conflicts_with_all = [
                "list",
                "all",
                "add",
                "unset",
                "unset_all",
                "rename_section",
                "rename_section_new"
            ]
        )]
    pub(crate) remove_section: Option<String>,
    /// Coerce values as bool, int, or path (canonical on write).
    #[arg(
            long = "type",
            value_name = "TYPE",
            value_enum,
            conflicts_with_all = [
                "list",
                "unset",
                "unset_all",
                "rename_section",
                "rename_section_new",
                "remove_section"
            ]
        )]
    pub(crate) value_type: Option<crate::commands::config::ConfigTypeArg>,
    /// When reading, use this value if the key is unset.
    #[arg(
            long = "default",
            value_name = "VALUE",
            conflicts_with_all = [
                "list",
                "add",
                "unset",
                "unset_all",
                "rename_section",
                "rename_section_new",
                "remove_section",
                "dry_run"
            ]
        )]
    pub(crate) default_value: Option<String>,
    /// Show what would change without writing the config file.
    #[arg(
            long,
            conflicts_with_all = ["list", "all", "default_value"]
        )]
    pub(crate) dry_run: bool,
    /// The configuration key, e.g. user.name. Optional prefix with `--list`.
    pub(crate) key: Option<String>,
    /// The value to set. Omit to read the current value.
    pub(crate) value: Option<String>,
}
