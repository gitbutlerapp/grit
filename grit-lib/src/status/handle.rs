//! [`Status`] — structured working-tree status for one repository.

use crate::diff::DiffEntry;
use crate::error::Result;
use crate::index::Index;
use crate::pathspec::Pathspec;
use crate::porcelain::status::{
    collect_untracked_and_ignored_inner, status as status_impl,
    status_diff_entry_matches_pathspec_typed, status_options_from_config, IgnoredMode, StatusModel,
    StatusOptions, UntrackedCollectMeta, UntrackedScan,
};
use crate::progress::{CancelToken, ProgressSink};
use crate::repo::Repository;
use crate::status::summary::{assemble_status_summary, StatusSummary, StatusSummaryOptions};
use crate::worktree_rules::WorktreeRules;

/// Whether untracked collection may refresh and read the index UNTR extension.
///
/// Used by [`UntrackedOptions::cache`](UntrackedOptions::cache) for [`Status::untracked`] walks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum UntrackedCachePolicy {
    /// Use UNTR when configured (status default).
    #[default]
    Refresh,
    /// Skip UNTR refresh (staging/`git add` must not pay a full status-style rebuild).
    Skip,
}

/// Options for [`Status::untracked`] and related untracked walks.
///
/// Defaults match a normal `git status` untracked scan (no ignored files, collapsed directories).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct UntrackedOptions {
    /// How ignored paths are reported.
    pub ignored_mode: IgnoredMode,
    /// When true, list every file under untracked directories (`--untracked-files=all`).
    pub show_all: bool,
    /// Limit the walk to paths matching this pathspec.
    pub pathspec: Pathspec,
    /// UNTR cache use for this walk.
    pub cache: UntrackedCachePolicy,
}

impl Default for UntrackedOptions {
    fn default() -> Self {
        Self {
            ignored_mode: IgnoredMode::No,
            show_all: false,
            pathspec: Pathspec::entire_tree(crate::pathspec::PathspecGlobalFlags::default()),
            cache: UntrackedCachePolicy::default(),
        }
    }
}

impl UntrackedOptions {
    /// Sets [`ignored_mode`](Self::ignored_mode).
    #[must_use]
    pub fn with_ignored_mode(mut self, ignored_mode: IgnoredMode) -> Self {
        self.ignored_mode = ignored_mode;
        self
    }

    /// Sets [`show_all`](Self::show_all).
    #[must_use]
    pub fn with_show_all(mut self, show_all: bool) -> Self {
        self.show_all = show_all;
        self
    }

    /// Sets [`pathspec`](Self::pathspec).
    #[must_use]
    pub fn with_pathspec(mut self, pathspec: Pathspec) -> Self {
        self.pathspec = pathspec;
        self
    }

    /// Sets [`cache`](Self::cache).
    #[must_use]
    pub fn with_cache(mut self, cache: UntrackedCachePolicy) -> Self {
        self.cache = cache;
        self
    }
}

/// Compute staged, unstaged, untracked, and ignored paths for one repository.
#[derive(Debug, Copy, Clone)]
pub struct Status<'repo> {
    repo: &'repo Repository,
}

impl<'repo> Status<'repo> {
    /// Bind status operations to `repo`.
    #[inline]
    #[must_use]
    pub fn new(repo: &'repo Repository) -> Self {
        Self { repo }
    }

    /// Underlying repository.
    #[inline]
    #[must_use]
    pub fn repository(&self) -> &'repo Repository {
        self.repo
    }

    /// Full status model from [`StatusOptions`].
    ///
    /// # Errors
    ///
    /// Propagates repository, diff, index, and untracked-walk failures. Returns
    /// [`crate::error::Error::Cancelled`] when `cancel` is set.
    ///
    /// # Examples
    ///
    /// ```
    /// use grit_lib::progress::{CancelToken, NullProgress};
    /// use grit_lib::repo::{InitOptions, Repository};
    /// use grit_lib::status::StatusOptions;
    ///
    /// # fn main() -> grit_lib::error::Result<()> {
    /// let dir = tempfile::tempdir()?;
    /// let (repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
    /// let model = repo.status().run(
    ///     &StatusOptions::default(),
    ///     &mut NullProgress,
    ///     &CancelToken::never(),
    /// )?;
    /// assert!(model.staged.is_empty() && model.unstaged.is_empty());
    /// # Ok(())
    /// # }
    /// ```
    pub fn run(
        &self,
        options: &StatusOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<StatusModel> {
        status_impl(self.repo, options, progress, cancel)
    }

    /// Dashboard summary: branch standing, guidance, and conflict grouping for a [`StatusModel`].
    ///
    /// Call after [`Self::run`] when the CLI or an embedder needs header and integration hints
    /// without rescanning the worktree.
    ///
    /// # Errors
    ///
    /// Propagates [`crate::branch_tracking::BranchStanding::compute`] failures.
    ///
    /// # Examples
    ///
    /// ```
    /// use grit_lib::progress::{CancelToken, NullProgress};
    /// use grit_lib::repo::{InitOptions, Repository};
    /// use grit_lib::status::{StatusOptions, StatusSummaryOptions};
    ///
    /// # fn main() -> grit_lib::error::Result<()> {
    /// let dir = tempfile::tempdir()?;
    /// let (repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
    /// let status = repo.status();
    /// let model = status.run(&StatusOptions::default(), &mut NullProgress, &CancelToken::never())?;
    /// let summary = status.assemble_summary(model, StatusSummaryOptions::default())?;
    /// assert!(summary.conflict_groups.is_empty());
    /// # Ok(())
    /// # }
    /// ```
    pub fn assemble_summary(
        &self,
        model: StatusModel,
        options: StatusSummaryOptions,
    ) -> Result<StatusSummary> {
        assemble_status_summary(self.repo, model, options)
    }

    /// Walk the work tree and collect untracked and ignored paths into sorted lists.
    ///
    /// Honors [`UntrackedOptions`] (ignored mode, `show_all`, pathspec limit, UNTR cache policy).
    ///
    /// # Errors
    ///
    /// Propagates ignore-rule load, index, and walk failures. Returns
    /// [`crate::error::Error::Cancelled`] when `cancel` is set.
    pub fn untracked(
        &self,
        index: &mut Index,
        options: &UntrackedOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<(Vec<String>, Vec<String>)> {
        let _ = progress;
        let (untracked, ignored, _) = self.untracked_with_meta(index, options, None, cancel)?;
        Ok((untracked, ignored))
    }

    /// Like [`Self::untracked`], also returning walk metadata (collapsed clean units, etc.).
    ///
    /// When `rules` is `Some`, ignore and attribute files already loaded for this operation are
    /// reused instead of reading them again.
    ///
    /// # Errors
    ///
    /// Same as [`Self::untracked`].
    pub fn untracked_with_meta(
        &self,
        index: &mut Index,
        options: &UntrackedOptions,
        rules: Option<&WorktreeRules>,
        cancel: &CancelToken,
    ) -> Result<(Vec<String>, Vec<String>, UntrackedCollectMeta)> {
        let work_tree = self
            .repo
            .work_tree()
            .ok_or(crate::repo::RepositoryError::RequiresWorkTree)?;
        let owned_rules;
        let rules = match rules {
            Some(rules) => rules,
            None => {
                owned_rules = WorktreeRules::from_repository(self.repo, index)?;
                &owned_rules
            }
        };
        let specs = if options.pathspec.is_entire_tree() {
            &[] as &[String]
        } else {
            options.pathspec.specs()
        };
        let use_cache = matches!(options.cache, UntrackedCachePolicy::Refresh);
        collect_untracked_and_ignored_inner(
            self.repo,
            index,
            work_tree,
            specs,
            Some(&options.pathspec),
            UntrackedScan {
                ignored_mode: options.ignored_mode,
                show_all: options.show_all,
                sort_paths: true,
                use_untracked_cache: use_cache,
                cache_miss: Default::default(),
                clean_unit_dirs: None,
                listing_cache: None,
            },
            rules,
            Some(cancel.clone()),
        )
    }

    /// Expand collapsed untracked directory markers (`dir/`) into concrete file paths for staging.
    ///
    /// # Errors
    ///
    /// Propagates ignore-rule and walk failures.
    pub fn expand_untracked_for_staging(
        &self,
        index: &mut Index,
        untracked: Vec<String>,
        pathspecs: &[String],
    ) -> Result<Vec<String>> {
        crate::porcelain::status::expand_untracked_for_staging(
            self.repo,
            index,
            self.repo
                .work_tree()
                .ok_or(crate::repo::RepositoryError::RequiresWorkTree)?,
            untracked,
            pathspecs,
        )
    }

    /// Whether a diff entry matches `pathspec` using typed pathspec evaluation (rename-aware).
    ///
    /// # Errors
    ///
    /// Same as [`Pathspec::matches_change`](crate::pathspec::Pathspec::matches_change).
    pub fn diff_entry_matches_pathspec(
        &self,
        entry: &DiffEntry,
        pathspec: &Pathspec,
    ) -> Result<bool> {
        status_diff_entry_matches_pathspec_typed(self.repo, entry, pathspec)
    }

    /// Case-insensitive path groups whose work-tree bytes cannot be committed faithfully.
    ///
    /// # Errors
    ///
    /// Propagates repository, config, and work-tree failures from
    /// [`crate::status::worktree_case_collision_groups`].
    ///
    /// # Examples
    ///
    /// ```
    /// use grit_lib::index::IndexLoadOptions;
    /// use grit_lib::repo::{InitOptions, Repository};
    ///
    /// # fn main() -> grit_lib::error::Result<()> {
    /// let dir = tempfile::tempdir()?;
    /// let (repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
    /// let index = repo.index().load(&IndexLoadOptions::default())?;
    /// assert!(repo.status().worktree_case_collisions(&index)?.is_empty());
    /// # Ok(())
    /// # }
    /// ```
    pub fn worktree_case_collisions(
        &self,
        index: &Index,
    ) -> Result<Vec<crate::status::CaseCollisionGroup>> {
        crate::status::worktree_case_collision_groups(self.repo, index)
    }
}

impl StatusOptions {
    /// Build default status options from repository configuration.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::ConfigError::InvalidUntrackedFilesMode`] when
    /// `status.showUntrackedFiles` is invalid.
    pub fn from_config(config: &crate::config::Config<'_>) -> Result<Self> {
        Ok(status_options_from_config(config.snapshot()?.as_ref())?)
    }
}
