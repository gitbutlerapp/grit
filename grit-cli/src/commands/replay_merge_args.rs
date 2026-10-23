//! Shared merge-favor and conflict-marker flags for pick and revert.

use clap::Args;
use grit_lib::diff::merge_file::{ConflictStyle, MergeFavor};

use crate::commands::merge_strategy::MergeStrategyArg;
use crate::conflict_style::ConflictStyleArg;

/// Auto-resolve replay conflicts and choose conflict marker style (pick / revert).
#[derive(Debug, Clone, Default, Args)]
pub struct ReplayMergeArgs {
    /// Auto-resolve replay conflicts: `ours` keeps the current branch side, `theirs` keeps the replayed change.
    #[arg(short = 'X', value_enum, value_name = "OURS|THEIRS")]
    pub strategy: Option<MergeStrategyArg>,
    /// Conflict marker style when a step would stop with conflicts.
    #[arg(long = "conflict", value_enum)]
    pub conflict_style: Option<ConflictStyleArg>,
}

impl ReplayMergeArgs {
    /// Merge-favor policy for replay tree merges.
    pub fn favor(&self) -> MergeFavor {
        self.strategy.map(MergeFavor::from).unwrap_or_default()
    }

    /// Conflict marker style for replay tree merges.
    pub fn conflict_style(&self, repo: &grit_lib::repo::Repository) -> ConflictStyle {
        self.conflict_style
            .map(ConflictStyle::from)
            .unwrap_or_else(|| crate::conflict_style::conflict_style_from_repo(repo))
    }
}
