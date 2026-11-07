//! Repository-scoped index load/write via [`IndexFile`].
use std::path::Path;

use crate::error::Result;
use crate::index::{Index, IndexParseContext};
use crate::repo::Repository;
use crate::split_index::WriteSplitIndexRequest;

/// How [`IndexFile::load`] treats sparse-directory placeholders.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum SparseIndexLoad {
    /// Replace every placeholder with the blob entries of its tree, so each path is a row.
    #[default]
    Expand,
    /// Keep placeholders as they are (no tree reads). For read-mostly operations such as
    /// `status` and `diff` on cone sparse-index checkouts.
    Preserve,
}

/// Options for [`IndexFile::load`].
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct IndexLoadOptions<'a> {
    /// Index file to read; `None` uses `GIT_INDEX_FILE` or `$GIT_DIR/index`.
    pub path: Option<&'a Path>,
    /// Whether sparse-directory placeholders are expanded.
    pub sparse: SparseIndexLoad,
    /// Thread policy for parallel entry loading; `None` uses the repository's policy.
    pub parse_context: Option<IndexParseContext>,
}

impl<'a> IndexLoadOptions<'a> {
    /// Sets [`path`](Self::path).
    #[must_use]
    pub fn with_path(mut self, path: &'a Path) -> Self {
        self.path = Some(path);
        self
    }

    /// Sets [`sparse`](Self::sparse).
    #[must_use]
    pub fn with_sparse(mut self, sparse: SparseIndexLoad) -> Self {
        self.sparse = sparse;
        self
    }

    /// Sets [`parse_context`](Self::parse_context).
    #[must_use]
    pub fn with_parse_context(mut self, parse_context: IndexParseContext) -> Self {
        self.parse_context = Some(parse_context);
        self
    }
}

/// Options for [`IndexFile::write`].
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct IndexWriteOptions<'a> {
    /// Index file to write; `None` uses `GIT_INDEX_FILE` or `$GIT_DIR/index`.
    pub path: Option<&'a Path>,
    /// Split-index emission (`core.splitIndex` decides when not explicit).
    pub split: WriteSplitIndexRequest,
}

impl<'a> IndexWriteOptions<'a> {
    /// Sets [`path`](Self::path).
    #[must_use]
    pub fn with_path(mut self, path: &'a Path) -> Self {
        self.path = Some(path);
        self
    }

    /// Sets [`split`](Self::split).
    #[must_use]
    pub fn with_split(mut self, split: WriteSplitIndexRequest) -> Self {
        self.split = split;
        self
    }
}

/// Load and persist the Git index for one repository.
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
/// let handle = repo.index();
/// let index = handle.load(&IndexLoadOptions::default())?;
/// assert!(index.entries().is_empty());
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Copy, Clone)]
pub struct IndexFile<'repo> {
    repo: &'repo Repository,
}

impl<'repo> IndexFile<'repo> {
    /// Bind index operations to `repo`.
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

    /// Path to the index file this repository uses by default.
    #[inline]
    #[must_use]
    pub fn path(&self) -> std::path::PathBuf {
        self.repo.index_path()
    }

    /// Resolve the index path honoring `GIT_INDEX_FILE` when set.
    ///
    /// # Errors
    ///
    /// Propagates configuration errors.
    #[inline]
    pub fn path_for_env(&self) -> Result<std::path::PathBuf> {
        self.repo.index_path_for_env()
    }

    /// Load the index, by default from the path selected by `GIT_INDEX_FILE` / the default
    /// `.git/index`, resolving a split index and expanding sparse-directory placeholders.
    ///
    /// A missing index file loads as an empty index.
    ///
    /// # Errors
    ///
    /// Propagates index parse, sparse, and split-index errors.
    pub fn load(&self, options: &IndexLoadOptions<'_>) -> crate::error::Result<Index> {
        let path = match options.path {
            Some(path) => path.to_path_buf(),
            None => self.repo.index_path_for_env()?,
        };
        let parse_ctx = options
            .parse_context
            .unwrap_or_else(|| IndexParseContext::from_policy(self.repo.thread_policy()));
        match options.sparse {
            SparseIndexLoad::Expand => self.repo.load_index_at_with_context(&path, parse_ctx),
            SparseIndexLoad::Preserve => {
                self.repo.load_index_at_preserving_sparse(&path, parse_ctx)
            }
        }
    }

    /// Write `index`, by default to the path selected by `GIT_INDEX_FILE` / the default
    /// `.git/index`.
    ///
    /// Collapses skip-worktree subtrees into sparse-directory placeholders when the sparse
    /// index is enabled, writes a split index when [`IndexWriteOptions::split`] or
    /// `core.splitIndex` asks for one, and runs the `post-index-change` hook.
    ///
    /// # Errors
    ///
    /// Propagates index serialization and I/O errors.
    pub fn write(
        &self,
        index: &mut Index,
        options: &IndexWriteOptions<'_>,
    ) -> crate::error::Result<()> {
        let path = match options.path {
            Some(path) => path.to_path_buf(),
            None => self.repo.index_path_for_env()?,
        };
        self.repo.write_index_at_split(&path, index, options.split)
    }

    /// Mark stage-0 entries of `index` that the sparse-checkout patterns exclude as
    /// skip-worktree, as Git does after building an index from a tree (`read-tree`, branch
    /// checkout, fast-forward merge).
    ///
    /// A no-op when `core.sparseCheckout` is false or `info/sparse-checkout` is missing. The
    /// index is bumped to version 3 when any entry is marked. On-disk directories in the work
    /// tree can match directory-only non-cone patterns.
    ///
    /// Returns cone-parse warnings when `core.sparseCheckoutCone` is set but the file is not a
    /// valid cone definition (matching then falls back to non-cone rules).
    ///
    /// # Errors
    ///
    /// Propagates config load failures.
    pub fn apply_sparse_checkout(&self, index: &mut Index) -> Result<Vec<String>> {
        let config = self.repo.config().snapshot()?;
        Ok(crate::sparse_checkout::apply_sparse_checkout_with_config(
            self.repo.git_dir(),
            self.repo.work_tree(),
            index,
            config.as_ref(),
        ))
    }

    /// Acquire a lock on the index path selected by `GIT_INDEX_FILE` / the default `.git/index`.
    #[inline]
    pub fn begin_update(&self) -> crate::error::Result<crate::index::IndexLock> {
        self.repo.begin_index_update()
    }

    /// Write `index` using a lock from [`Self::begin_update`].
    #[inline]
    pub fn commit_update(
        &self,
        lock: &mut crate::index::IndexLock,
        index: &mut Index,
        updated_workdir: bool,
    ) -> crate::error::Result<()> {
        self.repo.commit_index_update(lock, index, updated_workdir)
    }

    /// Persist the index when the lock can be acquired (Git `repo_update_index_if_able`),
    /// using the path selected by `GIT_INDEX_FILE` when set.
    #[inline]
    pub fn try_write(&self, index: &mut Index) -> crate::error::Result<bool> {
        self.repo.try_write_index(index)
    }

    /// Stage paths matching [`StageOptions`](crate::porcelain::add::StageOptions) into the index.
    ///
    /// # Errors
    ///
    /// Propagates index, worktree, and pathspec failures; returns [`crate::error::Error::Cancelled`]
    /// when `cancel` is set.
    pub fn stage(
        &self,
        options: &crate::porcelain::add::StageOptions,
        progress: &mut dyn crate::progress::ProgressSink,
        cancel: &crate::progress::CancelToken,
    ) -> crate::error::Result<crate::porcelain::add::StageOutcome> {
        crate::porcelain::add::stage(self.repo, options, progress, cancel)
    }

    /// Stage worktree changes for commit auto-staging.
    ///
    /// # Errors
    ///
    /// Same as [`Self::stage`] for the implicit all-paths staging pass.
    pub fn stage_for_commit(
        &self,
        progress: &mut dyn crate::progress::ProgressSink,
        cancel: &crate::progress::CancelToken,
    ) -> crate::error::Result<crate::porcelain::add::StageOutcome> {
        crate::porcelain::add::stage_for_commit(self.repo, progress, cancel)
    }

    /// Stage paths into a caller-owned index without writing it to disk.
    ///
    /// # Errors
    ///
    /// Same as [`Self::stage`], without persisting `index`.
    pub fn stage_on(
        &self,
        index: &mut Index,
        options: &crate::porcelain::add::StageOptions,
        progress: &mut dyn crate::progress::ProgressSink,
        cancel: &crate::progress::CancelToken,
    ) -> crate::error::Result<crate::porcelain::add::StageOutcome> {
        crate::porcelain::add::stage_on_index(self.repo, index, options, progress, cancel)
    }

    /// Stage paths matching `pathspecs` without a full status pass.
    ///
    /// # Errors
    ///
    /// Propagates index refresh and worktree scan failures.
    pub fn stage_worktree_paths(
        &self,
        pathspecs: &[String],
    ) -> crate::error::Result<crate::porcelain::staging::StageWorktreeChangesOutcome> {
        crate::porcelain::staging::stage_worktree_changes(self.repo, pathspecs)
    }

    /// Refresh tracked modifications from the worktree into the index.
    ///
    /// # Errors
    ///
    /// Propagates index load, stat refresh, and worktree scan failures.
    pub fn stage_tracked_modifications(
        &self,
        work_tree: &Path,
    ) -> crate::error::Result<crate::porcelain::stage_tracked::StageTrackedSummary> {
        let thresholds =
            self.staging_thresholds_from_options(&crate::porcelain::add::StageOptions::default());
        crate::porcelain::stage_tracked::stage_tracked_modifications_with_thresholds(
            self.repo, work_tree, thresholds,
        )
    }

    /// Like [`Self::stage_tracked_modifications`], updating a caller-owned index only.
    ///
    /// # Errors
    ///
    /// Same as [`Self::stage_tracked_modifications`].
    pub fn stage_tracked_modifications_on(
        &self,
        work_tree: &Path,
        index_path: &Path,
        index: &mut Index,
        options: &crate::porcelain::add::StageOptions,
    ) -> crate::error::Result<crate::porcelain::stage_tracked::StageTrackedSummary> {
        let thresholds = self.staging_thresholds_from_options(options);
        crate::porcelain::stage_tracked::stage_tracked_modifications_in_index_with_thresholds(
            self.repo, work_tree, index_path, index, thresholds,
        )
    }

    fn staging_thresholds_from_options(
        &self,
        options: &crate::porcelain::add::StageOptions,
    ) -> crate::porcelain::bulk_index::IndexStagingThresholds {
        #[cfg(any(test, feature = "test-support"))]
        {
            options.staging_thresholds.unwrap_or_default()
        }
        #[cfg(not(any(test, feature = "test-support")))]
        {
            options.staging_thresholds.unwrap_or_default()
        }
    }

    /// Stage structured diff hunks by stable id.
    ///
    /// # Errors
    ///
    /// Propagates diff, index, and hunk-resolution failures.
    pub fn stage_hunks(
        &self,
        side: crate::diff::DiffSide,
        selection: &crate::porcelain::hunk_select::HunkSelection,
        pathspecs: &[String],
        diff_options: crate::diff::DiffOptions,
        dry_run: bool,
        cancel: &crate::progress::CancelToken,
    ) -> crate::error::Result<crate::porcelain::hunk_select::StageHunksOutcome> {
        crate::porcelain::hunk_select::stage_hunks(
            self.repo,
            side,
            selection,
            pathspecs,
            diff_options,
            dry_run,
            cancel,
        )
    }

    /// Discard or unstage hunks in `selection`.
    ///
    /// # Errors
    ///
    /// Propagates diff and index update failures.
    pub fn restore_hunks(
        &self,
        selection: &crate::porcelain::hunk_select::HunkSelection,
        pathspecs: &[String],
        restore_staged: bool,
        restore_worktree: bool,
        dry_run: bool,
    ) -> crate::error::Result<crate::porcelain::hunk_select::RestoreHunksOutcome> {
        crate::porcelain::hunk_select::restore_hunks(
            self.repo,
            selection,
            pathspecs,
            restore_staged,
            restore_worktree,
            dry_run,
        )
    }

    /// Diff options for hunk id resolution (same boundaries as `grit diff --json`).
    ///
    /// # Errors
    ///
    /// Propagates config and diff-option construction failures.
    pub fn hunk_diff_options(&self) -> crate::error::Result<crate::diff::DiffOptions> {
        crate::porcelain::hunk_select::diff_options_for_hunk_select(self.repo)
    }

    /// Unstaged file diffs with staged peer hunks on the same path.
    ///
    /// # Errors
    ///
    /// Propagates index load and diff failures.
    pub fn unstaged_file_diffs(
        &self,
        pathspecs: &[String],
    ) -> crate::error::Result<
        Vec<(
            crate::diff::FileDiff,
            Vec<crate::diff::structured::StructuredHunk>,
        )>,
    > {
        crate::porcelain::hunk_select::unstaged_file_diffs(self.repo, pathspecs)
    }

    /// Unstaged hunk ids per path (same rules as `grit diff --json` on `--unstaged`).
    ///
    /// # Errors
    ///
    /// Propagates diff failures for the given pathspecs.
    pub fn list_unstaged_hunk_ids(
        &self,
        pathspecs: &[String],
        diff_options: crate::diff::DiffOptions,
    ) -> crate::error::Result<std::collections::BTreeMap<String, Vec<crate::diff::HunkId>>> {
        crate::porcelain::hunk_select::list_unstaged_hunk_ids(self.repo, pathspecs, diff_options)
    }

    /// Resolve a hunk selection to index-ready patches.
    ///
    /// # Errors
    ///
    /// Propagates diff and selection validation failures.
    pub fn resolve_hunk_selection(
        &self,
        side: crate::diff::DiffSide,
        selection: &crate::porcelain::hunk_select::HunkSelection,
        pathspecs: &[String],
        diff_options: crate::diff::DiffOptions,
    ) -> crate::error::Result<Vec<crate::apply::FilePatch>> {
        crate::porcelain::hunk_select::resolve_selection(
            self.repo,
            side,
            selection,
            pathspecs,
            diff_options,
        )
    }

    /// Whether `resolved` matches a known path in this repository.
    ///
    /// # Errors
    ///
    /// Propagates index and ref resolution failures.
    pub fn pathspec_matches_known(&self, resolved: &str) -> crate::error::Result<bool> {
        crate::porcelain::pathspec_match::pathspec_matches_known_in_repository(self.repo, resolved)
    }

    /// Require that `resolved` matches a known path, else return a pathspec error.
    ///
    /// # Errors
    ///
    /// Returns a pathspec error when the path is unknown.
    pub fn ensure_pathspec_matches_known(
        &self,
        display_spec: &str,
        resolved: &str,
    ) -> crate::error::Result<()> {
        crate::porcelain::pathspec_match::ensure_pathspec_matches_known_in_repository(
            self.repo,
            display_spec,
            resolved,
        )
    }
}
