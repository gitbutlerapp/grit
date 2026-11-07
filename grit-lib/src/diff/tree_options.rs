//! Options for the metadata diffs on [`Diff`](super::Diff): which paths, which pairings.

use crate::parallel::ThreadPolicy;
use crate::pathspec::Pathspec;
use crate::submodule_ignore::SubmoduleIgnoreMode;

use super::core::{DiffPostprocessOptions, GIT_DIFF_DEFAULT_BREAK_SCORE, GIT_DIFF_MAX_SCORE};
use super::options::RenameDetection;

/// Copy detection policy for tree, index and work-tree diffs (Git `-C`).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CopyDetection {
    /// Do not detect copies.
    #[default]
    Off,
    /// Pair added paths with paths changed in the same diff when content is at least this
    /// percent similar (Git `-C<n>`).
    Similarity(u8),
    /// Like [`Self::Similarity`], but every path of the old side is a copy source, changed or
    /// not (Git `-C<n> --find-copies-harder`).
    FindHarder(u8),
}

impl CopyDetection {
    /// Copy similarity threshold in percent, or `None` when [`Self::Off`].
    #[must_use]
    pub fn threshold_percent(self) -> Option<u32> {
        match self {
            Self::Off => None,
            Self::Similarity(p) | Self::FindHarder(p) => Some(u32::from(p)),
        }
    }
}

/// Break-rewrite policy (Git `-B`): split a heavily rewritten modification into a deletion and an
/// addition so rename and copy detection can pair its halves.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BreakRewrites {
    /// Never break modifications.
    #[default]
    Off,
    /// Break a modification whose rewritten share reaches this score, on Git's internal scale
    /// from `0` to [`BreakRewrites::MAX_SCORE`].
    Score(u64),
}

impl BreakRewrites {
    /// Top of the score scale [`Self::Score`] uses (100% rewritten).
    pub const MAX_SCORE: u64 = GIT_DIFF_MAX_SCORE;

    /// Git's default for a bare `-B` (50% of [`Self::MAX_SCORE`]).
    pub const DEFAULT: Self = Self::Score(GIT_DIFF_DEFAULT_BREAK_SCORE);

    /// Break at `percent` of [`Self::MAX_SCORE`] (Git `-B<n>%`); values above 100 saturate.
    #[must_use]
    pub fn percent(percent: u8) -> Self {
        Self::Score(Self::MAX_SCORE * u64::from(percent.min(100)) / 100)
    }

    /// Score on the [`Self::MAX_SCORE`] scale, or `None` when [`Self::Off`].
    #[must_use]
    pub fn score(self) -> Option<u64> {
        match self {
            Self::Off => None,
            Self::Score(score) => Some(score),
        }
    }
}

/// Options shared by the metadata diffs on [`Diff`](super::Diff): [`Diff::tree_to_tree`],
/// [`Diff::commits`], [`Diff::index_to_tree`], [`Diff::index_to_worktree`],
/// [`Diff::tree_to_worktree`] and [`Diff::detect_renames`].
///
/// The default compares every path with no rename, copy or break pairing (like
/// `git diff-tree -r`). Configuration such as `diff.renameLimit` and `core.autocrlf` is read
/// from the repository by the handle.
///
/// [`Diff::tree_to_tree`]: super::Diff::tree_to_tree
/// [`Diff::commits`]: super::Diff::commits
/// [`Diff::index_to_tree`]: super::Diff::index_to_tree
/// [`Diff::index_to_worktree`]: super::Diff::index_to_worktree
/// [`Diff::tree_to_worktree`]: super::Diff::tree_to_worktree
/// [`Diff::detect_renames`]: super::Diff::detect_renames
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct TreeDiffOptions {
    /// Rename pairing (Git `-M`). Defaults to [`RenameDetection::Off`].
    pub renames: RenameDetection,
    /// Copy pairing (Git `-C`). Defaults to [`CopyDetection::Off`].
    pub copies: CopyDetection,
    /// Break-rewrite splitting (Git `-B`). Defaults to [`BreakRewrites::Off`].
    pub break_rewrites: BreakRewrites,
    /// Also report changed subtrees themselves, not only the paths inside them (Git
    /// `diff-tree -t`). Applies to tree and commit diffs.
    pub show_tree_entries: bool,
    /// Path prefix prepended to every reported path (empty for the repository root). Applies to
    /// tree and commit diffs, whose trees are then subtrees at that prefix.
    pub prefix: String,
    /// Worker limits for rename scoring and work-tree stat; `None` uses the repository's
    /// [`thread_policy`](crate::repo::Repository::thread_policy).
    pub threads: Option<ThreadPolicy>,
    /// Report only paths this pathspec selects; `None` reports every path. Tree walks skip
    /// directories the pathspec cannot reach.
    pub pathspec: Option<Pathspec>,
    /// Which submodule (gitlink) changes to report. [`SubmoduleIgnoreMode::All`] omits gitlink
    /// paths entirely.
    pub submodules: SubmoduleIgnoreMode,
    /// Index-to-work-tree only: adopt fresh stat data for unchanged entries in the same scan,
    /// so a later index write makes the next diff cheaper. The outcome reports whether the
    /// in-memory index changed in
    /// [`DiffEntriesResult::index_stat_refreshed`](super::DiffEntriesResult::index_stat_refreshed).
    pub refresh_index_stat: bool,
    /// Index-to-work-tree only: status shortcuts. A tracked file whose size changed is reported
    /// as modified without being read, with a null work-tree `new_oid`, and intent-to-add
    /// entries are not hashed. Leave off when the contents of the changes are loaded next.
    pub for_status: bool,
}

impl Default for TreeDiffOptions {
    fn default() -> Self {
        Self {
            renames: RenameDetection::Off,
            copies: CopyDetection::Off,
            break_rewrites: BreakRewrites::Off,
            show_tree_entries: false,
            prefix: String::new(),
            threads: None,
            pathspec: None,
            submodules: SubmoduleIgnoreMode::None,
            refresh_index_stat: false,
            for_status: false,
        }
    }
}

impl TreeDiffOptions {
    /// Sets [`renames`](Self::renames).
    #[must_use]
    pub fn with_renames(mut self, renames: RenameDetection) -> Self {
        self.renames = renames;
        self
    }

    /// Sets [`copies`](Self::copies).
    #[must_use]
    pub fn with_copies(mut self, copies: CopyDetection) -> Self {
        self.copies = copies;
        self
    }

    /// Sets [`break_rewrites`](Self::break_rewrites).
    #[must_use]
    pub fn with_break_rewrites(mut self, break_rewrites: BreakRewrites) -> Self {
        self.break_rewrites = break_rewrites;
        self
    }

    /// Sets [`show_tree_entries`](Self::show_tree_entries).
    #[must_use]
    pub fn with_show_tree_entries(mut self, show_tree_entries: bool) -> Self {
        self.show_tree_entries = show_tree_entries;
        self
    }

    /// Sets [`prefix`](Self::prefix).
    #[must_use]
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// Sets [`threads`](Self::threads).
    #[must_use]
    pub fn with_threads(mut self, threads: Option<ThreadPolicy>) -> Self {
        self.threads = threads;
        self
    }

    /// Sets [`pathspec`](Self::pathspec).
    #[must_use]
    pub fn with_pathspec(mut self, pathspec: Option<Pathspec>) -> Self {
        self.pathspec = pathspec;
        self
    }

    /// Sets [`submodules`](Self::submodules).
    #[must_use]
    pub fn with_submodules(mut self, submodules: SubmoduleIgnoreMode) -> Self {
        self.submodules = submodules;
        self
    }

    /// Sets [`refresh_index_stat`](Self::refresh_index_stat).
    #[must_use]
    pub fn with_refresh_index_stat(mut self, refresh_index_stat: bool) -> Self {
        self.refresh_index_stat = refresh_index_stat;
        self
    }

    /// Sets [`for_status`](Self::for_status).
    #[must_use]
    pub fn with_for_status(mut self, for_status: bool) -> Self {
        self.for_status = for_status;
        self
    }

    /// Whether any rename, copy or break pairing runs after the path walk.
    pub(crate) fn pairs_entries(&self) -> bool {
        self.renames != RenameDetection::Off
            || self.copies != CopyDetection::Off
            || self.break_rewrites != BreakRewrites::Off
    }

    /// Pathspec strings to match, empty for every path.
    pub(crate) fn pathspec_specs(&self) -> &[String] {
        self.pathspec.as_ref().map_or(&[], Pathspec::specs)
    }

    /// Equivalent post-processing options; `materialize_worktree_oid` is set for
    /// tree-to-work-tree diffs, where `-B` fills the work-tree side's blob id like Git.
    pub(crate) fn postprocess_options(
        &self,
        materialize_worktree_oid: bool,
    ) -> DiffPostprocessOptions {
        DiffPostprocessOptions {
            break_score: self.break_rewrites.score(),
            rename_threshold: self
                .renames
                .threshold_percent()
                .unwrap_or(super::RENAME_DETECTION_DISABLED),
            copy_threshold: self.copies.threshold_percent(),
            find_copies_harder: matches!(self.copies, CopyDetection::FindHarder(_)),
            materialize_break_worktree_new_oid: materialize_worktree_oid,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_pair_nothing() {
        let opts = TreeDiffOptions::default();
        assert_eq!(opts.renames, RenameDetection::Off);
        assert_eq!(opts.copies, CopyDetection::Off);
        assert_eq!(opts.break_rewrites, BreakRewrites::Off);
        assert!(!opts.pairs_entries());
        assert!(opts.pathspec_specs().is_empty());
    }

    #[test]
    fn postprocess_options_map_every_field() {
        let opts = TreeDiffOptions::default()
            .with_renames(RenameDetection::Similarity(40))
            .with_copies(CopyDetection::FindHarder(60))
            .with_break_rewrites(BreakRewrites::DEFAULT);
        assert!(opts.pairs_entries());
        let pp = opts.postprocess_options(true);
        assert_eq!(pp.rename_threshold, 40);
        assert_eq!(pp.copy_threshold, Some(60));
        assert!(pp.find_copies_harder);
        assert_eq!(pp.break_score, Some(GIT_DIFF_DEFAULT_BREAK_SCORE));
        assert!(pp.materialize_break_worktree_new_oid);

        let off = TreeDiffOptions::default().postprocess_options(false);
        assert_eq!(
            off.rename_threshold,
            super::super::RENAME_DETECTION_DISABLED
        );
        assert_eq!(off.copy_threshold, None);
        assert!(!off.find_copies_harder);
    }

    #[test]
    fn break_rewrites_percent_scales_and_saturates() {
        assert_eq!(BreakRewrites::percent(50), BreakRewrites::DEFAULT);
        assert_eq!(
            BreakRewrites::percent(200).score(),
            Some(BreakRewrites::MAX_SCORE)
        );
        assert_eq!(BreakRewrites::Off.score(), None);
        assert_eq!(CopyDetection::Similarity(30).threshold_percent(), Some(30));
        assert_eq!(CopyDetection::Off.threshold_percent(), None);
    }
}
