//! Clap arguments for `grit init`.

use super::parse_init_object_format;
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

/// Create a new, empty repository.
#[derive(Debug, Args)]
pub(crate) struct InitArgs {
    /// Where to create the repository (defaults to the current directory).
    pub(crate) path: Option<String>,
    /// Create a bare repository (no working tree).
    #[arg(long, conflicts_with = "separate_git_dir")]
    pub(crate) bare: bool,
    /// Ref storage backend for the new repository.
    #[arg(long, value_name = "FORMAT", default_value = "files")]
    pub(crate) ref_format: grit_lib::refs::ref_storage::RefStorageFormat,
    /// Initial branch name (overrides `init.defaultBranch` from config).
    #[arg(short = 'b', long = "initial-branch", value_name = "NAME")]
    pub(crate) initial_branch: Option<String>,
    /// Place the git directory here and link from the work tree via a gitfile.
    #[arg(long = "separate-git-dir", value_name = "DIR", conflicts_with = "bare")]
    pub(crate) separate_git_dir: Option<std::path::PathBuf>,
    /// Object hash algorithm for the new repository (default SHA-1; omitted on reinit preserves format).
    #[arg(
            long = "object-format",
            value_name = "ALGO",
            value_parser = parse_init_object_format
        )]
    pub(crate) object_format: Option<grit_lib::objects::HashAlgo>,
}
