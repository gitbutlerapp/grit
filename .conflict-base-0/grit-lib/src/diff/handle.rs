//! Repository-scoped tree and index diffs via [`Diff`].
use crate::diff::targets::{
    collect_diff_entries_for_patch_target, collect_files_for_target,
    collect_tree_structured_for_target, file_diffs_from_entries,
};
use crate::diff::TreeDiffStructured;
use crate::diff::{diff_index_to_tree, diff_index_to_worktree, diff_trees, DiffEntry, DiffOptions};
use crate::diff::{DiffTarget, FileDiff};
use crate::error::Result;
use crate::index::Index;
use crate::objects::ObjectId;
use crate::repo::Repository;

/// Compare trees, the index, and the working tree for one repository.
#[derive(Debug, Copy, Clone)]
pub struct Diff<'repo> {
    repo: &'repo Repository,
}

impl<'repo> Diff<'repo> {
    /// Bind diff helpers to `repo`.
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

    /// Diff two tree oids (either side may be `None` for an empty tree).
    ///
    /// # Errors
    ///
    /// Propagates ODB read and tree walk errors.
    ///
    /// # Examples
    ///
    /// See the library guide on [diffing](https://grit-scm.com/docs/library/diff/).
    #[inline]
    pub fn tree_to_tree(
        &self,
        old_tree: Option<&ObjectId>,
        new_tree: Option<&ObjectId>,
    ) -> Result<Vec<DiffEntry>> {
        diff_trees(self.repo.odb(), old_tree, new_tree, "")
    }

    /// Diff the index against a tree (typically `HEAD`'s tree).
    ///
    /// # Errors
    ///
    /// Propagates ODB and index diff errors.
    #[inline]
    pub fn index_to_tree(
        &self,
        index: &Index,
        tree_oid: Option<&ObjectId>,
        ignore_submodules: bool,
    ) -> Result<Vec<DiffEntry>> {
        diff_index_to_tree(self.repo.odb(), index, tree_oid, ignore_submodules)
    }

    /// Diff the working tree against the index.
    ///
    /// # Errors
    ///
    /// Returns an error when the repository has no work tree or diff I/O fails.
    #[inline]
    pub fn worktree_to_index(
        &self,
        index: &mut Index,
        ignore_submodule_untracked: bool,
        simplify_gitlinks: bool,
    ) -> Result<Vec<DiffEntry>> {
        let work_tree = self
            .repo
            .work_tree()
            .ok_or_else(|| crate::error::Error::NotARepository("bare repository".into()))?;
        diff_index_to_worktree(
            self.repo.odb(),
            index,
            work_tree,
            ignore_submodule_untracked,
            simplify_gitlinks,
        )
    }

    /// Structured per-file diffs for `target`, honoring `pathspecs` and line-diff `opts`.
    ///
    /// # Errors
    ///
    /// Propagates index, ODB, and worktree diff failures.
    pub fn files(
        &self,
        target: &DiffTarget,
        pathspecs: &[String],
        opts: &DiffOptions,
    ) -> Result<Vec<FileDiff>> {
        collect_files_for_target(self.repo, target, pathspecs, *opts)
    }

    /// Build [`FileDiff`] rows from an existing tree diff entry list (e.g. stash W vs index).
    ///
    /// # Errors
    ///
    /// Propagates object read and structured diff errors.
    pub fn files_from_entries(
        &self,
        entries: Vec<DiffEntry>,
        opts: &DiffOptions,
    ) -> Result<Vec<FileDiff>> {
        file_diffs_from_entries(self.repo, entries, *opts)
    }

    /// Structured diff rows for [`write_patch_structured`] (patch-text output).
    ///
    /// # Errors
    ///
    /// Propagates index, ODB, and worktree diff failures.
    pub fn tree_structured(
        &self,
        target: &DiffTarget,
        pathspecs: &[String],
        opts: &DiffOptions,
    ) -> Result<Vec<TreeDiffStructured>> {
        collect_tree_structured_for_target(self.repo, target, pathspecs, *opts)
    }

    /// Tree diff entries for commit-range [`write_patch`] (skips structured line diffs).
    ///
    /// # Errors
    ///
    /// Propagates diff failures; returns an error for worktree/index patch targets.
    pub fn patch_entries(
        &self,
        target: &DiffTarget,
        pathspecs: &[String],
        opts: &DiffOptions,
    ) -> Result<Vec<DiffEntry>> {
        collect_diff_entries_for_patch_target(self.repo, target, pathspecs, *opts)
    }
}
