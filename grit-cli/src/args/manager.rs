//! Clap arguments for `grit manager`.

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
