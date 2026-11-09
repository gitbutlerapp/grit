//! Clap arguments for `grit upload-pack`.

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
#[derive(Debug, clap::Args)]
pub(crate) struct UploadPackArgs {
    #[command(flatten)]
    pub(crate) inner: super::ServeArgs,
}
