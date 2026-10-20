//! Repository-scoped tree and index diffs via [`Diff`].
use crate::config::ConfigSet;
use crate::diff::{
    build_file_diffs, diff_index_to_tree, diff_index_to_worktree, diff_trees, DiffDetail,
    DiffEntry, DiffFilesOutput, DiffOptions,
};
use crate::error::Result;
use crate::index::Index;
use crate::objects::ObjectId;
use crate::parallel::ThreadPolicy;
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

    /// Build per-file diff output for tree or index diff entries.
    ///
    /// # Errors
    ///
    /// Propagates object database and diff errors from [`build_file_diffs`].
    pub fn files(
        &self,
        entries: Vec<DiffEntry>,
        detail: DiffDetail,
        diff_options: DiffOptions,
        config: &ConfigSet,
        thread_policy: &ThreadPolicy,
    ) -> Result<DiffFilesOutput> {
        build_file_diffs(
            self.repo.odb(),
            entries,
            detail,
            diff_options,
            config,
            thread_policy,
        )
    }
}
