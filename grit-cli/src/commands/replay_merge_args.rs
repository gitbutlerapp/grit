//! Shared merge-favor and conflict-marker flags for pick and revert.

use std::num::NonZeroU32;

use clap::{Args, ValueEnum};
use grit_lib::diff::merge_file::{ConflictStyle, MergeFavor};
use grit_lib::porcelain::replay_sequence::ReplayEmptyPolicy;

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

/// How to handle a pick / revert step that would not change `HEAD` (`--empty`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum EmptyStepArg {
    /// Stop and leave sequencer state for `--continue` / `--skip`.
    #[default]
    Stop,
    /// Record an empty commit for the step.
    Keep,
    /// Skip the commit and continue with the next one.
    Drop,
}

impl From<EmptyStepArg> for ReplayEmptyPolicy {
    fn from(value: EmptyStepArg) -> Self {
        match value {
            EmptyStepArg::Stop => ReplayEmptyPolicy::Stop,
            EmptyStepArg::Keep => ReplayEmptyPolicy::Keep,
            EmptyStepArg::Drop => ReplayEmptyPolicy::Drop,
        }
    }
}

/// Validate `--mainline <n>` (1-based parent number) for pick / revert.
pub fn mainline_parent(n: Option<u32>) -> anyhow::Result<Option<NonZeroU32>> {
    n.map(|n| {
        NonZeroU32::new(n).ok_or_else(|| {
            anyhow::anyhow!("--mainline requires a positive parent number (1, 2, …)")
        })
    })
    .transpose()
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
