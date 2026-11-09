//! Clap arguments for `grit checkin`.

use clap::Args;

/// Send a snapshot checkout's branches back to the server, grafted onto its real history.
#[derive(Debug, Args)]
pub(crate) struct CheckinArgs {
    /// Branches to check in (default: every branch with commits on top of the snapshot).
    #[arg(value_name = "BRANCH")]
    pub(crate) branches: Vec<String>,
    /// Replace a server branch even where that drops commits it has.
    #[arg(short = 'f', long)]
    pub(crate) force: bool,
}
