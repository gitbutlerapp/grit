//! Shared `--conflict` value for commands that rebuild merge conflict markers.

use grit_lib::diff::merge_file::ConflictStyle;

/// CLI spelling of Git's `merge.conflictStyle` values.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum ConflictStyleArg {
    /// Standard two-section conflict markers.
    Merge,
    /// Three-section markers including the base section.
    Diff3,
    /// Zealous diff3 (`zdiff3`).
    #[value(name = "zdiff3")]
    Zdiff3,
}

impl From<ConflictStyleArg> for ConflictStyle {
    fn from(value: ConflictStyleArg) -> Self {
        match value {
            ConflictStyleArg::Merge => ConflictStyle::Merge,
            ConflictStyleArg::Diff3 => ConflictStyle::Diff3,
            ConflictStyleArg::Zdiff3 => ConflictStyle::ZealousDiff3,
        }
    }
}

/// Read `merge.conflictStyle` from repository config (Git default: `merge`).
pub fn conflict_style_from_repo(repo: &grit_lib::repo::Repository) -> ConflictStyle {
    let Ok(config) = repo.config() else {
        return ConflictStyle::Merge;
    };
    let Some(value) = config.get("merge.conflictStyle") else {
        return ConflictStyle::Merge;
    };
    match value.as_str() {
        "diff3" => ConflictStyle::Diff3,
        "zdiff3" => ConflictStyle::ZealousDiff3,
        _ => ConflictStyle::Merge,
    }
}
