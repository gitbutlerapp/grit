//! Shared merge strategy flags (`-X`, `--conflict`) for merge, pick, revert, and restore.

use grit_lib::diff::merge_file::{ConflictStyle, MergeFavor};

use crate::conflict_style::ConflictStyleArg;

/// `-X ours|theirs` (Git merge strategy option; union is not exposed on the CLI).
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum MergeFavorArg {
    /// Prefer the current branch when both sides changed the same region.
    Ours,
    /// Prefer the branch being merged when both sides changed the same region.
    Theirs,
}

impl From<MergeFavorArg> for MergeFavor {
    fn from(value: MergeFavorArg) -> Self {
        match value {
            MergeFavorArg::Ours => MergeFavor::Ours,
            MergeFavorArg::Theirs => MergeFavor::Theirs,
        }
    }
}

/// Merge strategy options shared across commands that perform three-way merges.
#[derive(Debug, Clone, Default, clap::Args)]
#[group(id = "merge_strategy")]
pub struct MergeStrategyArgs {
    /// Prefer ours or theirs on overlapping edits (`-X ours` / `-X theirs`).
    #[arg(short = 'X', value_name = "ours|theirs", value_enum)]
    pub favor: Option<MergeFavorArg>,
    /// Conflict marker style (`merge`, `diff3`, or `zdiff3`).
    #[arg(long = "conflict", value_enum)]
    pub conflict_style: Option<ConflictStyleArg>,
}

impl MergeStrategyArgs {
    /// Map CLI flags into library merge options.
    #[must_use]
    pub fn into_merge_fields(self) -> (MergeFavor, Option<ConflictStyle>) {
        (
            self.favor.map(Into::into).unwrap_or(MergeFavor::None),
            self.conflict_style.map(Into::into),
        )
    }
}
