//! Shared `-X ours|theirs` merge strategy values for replay commands.

use clap::ValueEnum;
use grit_lib::diff::merge_file::MergeFavor;

/// Conflict auto-resolution during history replay (`-X`).
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum MergeStrategyArg {
    /// Favor the upstream / onto side during rebase replay (matches `git rebase -X ours`).
    Ours,
    /// Favor the commit being replayed (matches `git rebase -X theirs`).
    Theirs,
}

impl From<MergeStrategyArg> for MergeFavor {
    fn from(value: MergeStrategyArg) -> Self {
        match value {
            MergeStrategyArg::Ours => MergeFavor::Ours,
            MergeStrategyArg::Theirs => MergeFavor::Theirs,
        }
    }
}
