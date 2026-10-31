//! Repository-scoped tree and index diffs via [`Diff`].
use crate::diff::targets::collect_files_for_target;
use crate::diff::{
    diff_index_to_tree, diff_index_to_worktree, diff_trees, diff_trees_with_renames_and_policy,
    DiffEntry, DiffOptions,
};
use crate::diff::{DiffTarget, FileDiff, UncommittedDiff};
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
    /// Reads tree metadata without loading blobs and skips identical subtree ids.
    /// The walk retains active directory frames alongside the ODB's bounded tree
    /// cache; the returned vector owns metadata for every changed path.
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

    /// Like [`Self::tree_to_tree`], then pair renames using `options.renames` (Git `-M`).
    ///
    /// # Errors
    ///
    /// Propagates ODB read, tree walk, and rename detection errors.
    pub fn tree_to_tree_renames(
        &self,
        old_tree: Option<&ObjectId>,
        new_tree: Option<&ObjectId>,
        options: &DiffOptions,
    ) -> Result<Vec<DiffEntry>> {
        let config = self.repo.config()?;
        diff_trees_with_renames_and_policy(
            self.repo.odb(),
            old_tree,
            new_tree,
            "",
            options.rename_threshold_u32(),
            config.as_ref(),
            &self.repo.thread_policy(),
        )
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
            .ok_or(crate::error::Error::RequiresWorkTree)?;
        diff_index_to_worktree(
            self.repo.odb(),
            index,
            work_tree,
            ignore_submodule_untracked,
            simplify_gitlinks,
        )
    }

    /// Staged, unstaged, and working-tree diffs over the repository's on-disk index.
    ///
    /// # Errors
    ///
    /// Returns index read errors.
    pub fn uncommitted(&self) -> Result<UncommittedDiff<'repo>> {
        let index = self.repo.index_handle().load_preserving_sparse()?;
        Ok(UncommittedDiff::new(self.repo, index))
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
        collect_files_for_target(self.repo, target, pathspecs, opts.clone())
    }
}
