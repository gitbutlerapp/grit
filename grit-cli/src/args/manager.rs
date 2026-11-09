//! Clap arguments for `grit manager`.

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

/// Git credential helper backed by the Windows Credential Manager.
///
/// You don't normally run this yourself — `grit auth` wires it into
/// `credential.helper` on Windows. It speaks Git's credential protocol.
#[derive(Debug, Args)]
pub(crate) struct ManagerArgs {
    /// The credential operation: get, store, or erase.
    pub(crate) operation: String,
}
