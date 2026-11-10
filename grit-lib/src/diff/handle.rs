//! Repository-scoped tree, index and work-tree diffs via [`Diff`].

#![allow(deprecated)]

use std::path::Path;

use crate::diff::targets::{collect_files_for_target, limit_target_entries, stored_target_entries};
use crate::diff::{
    detect_renames_with_policy, diff_index_to_tree, diff_index_to_worktree_with_options,
    diff_tree_to_worktree_with_git_dir_and_rules_extras, diff_trees_for_pathspecs_at,
    index_mtime_for_diff, postprocess_diff_entries, postprocess_diff_entries_from_sources,
    tree_oid_of_commit, DetectRenamesOptions, DiffDetail, DiffEntriesResult, DiffEntry,
    DiffFilesOutput, DiffIndexToWorktreeOptions, DiffMerges, DiffOptions, DiffPostprocessWorktree,
    DiffStat, DiffTreeToWorktreeExtras, ListedFileDiff, TreeDiffOptions, TreeDiffStructured,
};
use crate::diff::{
    DiffArguments, DiffRequest, DiffTarget, FileChange, FileDiff, HunkDiffBundle, UncommittedDiff,
};
use crate::error::Result;
use crate::index::Index;
use crate::objects::{FileMode, ObjectId};
use crate::parallel::ThreadPolicy;
use crate::pathspec::{Pathspec, PathspecMatcher};
use crate::repo::Repository;
use crate::repo::RepositoryError;
use crate::submodule_ignore::SubmoduleIgnoreMode;

/// Compare trees, the index, and the working tree for one repository.
///
/// Obtain it from [`Repository::diff`]. The metadata diffs ([`Self::tree_to_tree`],
/// [`Self::commits`], [`Self::index_to_tree`], [`Self::index_to_worktree`],
/// [`Self::tree_to_worktree`]) list changed paths without loading contents and take one
/// [`TreeDiffOptions`]; the content methods ([`Self::files_for_entries`], [`Self::stat`],
/// [`Self::structured`], [`Self::for_each_structured`], …) turn those entries into hunks and
/// line counts under a [`DiffOptions`]. Configuration is read from the repository.
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
    /// Reads tree metadata without loading blobs and skips identical subtree ids; with a
    /// pathspec, directories it cannot reach are not read. Rename, copy and break pairing
    /// follow `options` (none by default). The walk retains active directory frames alongside
    /// the ODB's bounded tree cache; the result owns metadata for every changed path.
    ///
    /// # Parameters
    ///
    /// - `old_tree` - old side, or `None` for the empty tree.
    /// - `new_tree` - new side, or `None` for the empty tree.
    /// - `options` - pathspec, pairing, [`prefix`](TreeDiffOptions::prefix) and
    ///   [`show_tree_entries`](TreeDiffOptions::show_tree_entries).
    ///
    /// # Errors
    ///
    /// Propagates ODB read, tree walk, pathspec and rename detection errors.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use grit_lib::diff::{DiffStatus, RenameDetection, TreeDiffOptions};
    /// use grit_lib::objects::{serialize_tree, ObjectKind, TreeEntry};
    /// use grit_lib::repo::{InitOptions, Repository};
    /// # fn main() -> grit_lib::error::Result<()> {
    /// let dir = tempfile::tempdir()?;
    /// let (repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
    /// let blob = repo.odb().write(ObjectKind::Blob, b"one\ntwo\nthree\n")?;
    /// let tree = |name: &str| {
    ///     let entry = TreeEntry {
    ///         mode: 0o100644,
    ///         name: name.as_bytes().to_vec(),
    ///         oid: blob,
    ///     };
    ///     repo.odb().write(ObjectKind::Tree, &serialize_tree(&[entry]))
    /// };
    /// let (old, new) = (tree("a.txt")?, tree("b.txt")?);
    ///
    /// let plain = repo.diff().tree_to_tree(Some(&old), Some(&new), &TreeDiffOptions::default())?;
    /// assert_eq!(plain.entries.len(), 2); // a.txt deleted, b.txt added
    ///
    /// let options = TreeDiffOptions::default().with_renames(RenameDetection::Similarity(50));
    /// let renamed = repo.diff().tree_to_tree(Some(&old), Some(&new), &options)?;
    /// assert_eq!(renamed.entries.len(), 1);
    /// assert_eq!(renamed.entries[0].status, DiffStatus::Renamed);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// See also the library guide on [diffing](https://grit-scm.com/docs/library/diff/).
    pub fn tree_to_tree(
        &self,
        old_tree: Option<&ObjectId>,
        new_tree: Option<&ObjectId>,
        options: &TreeDiffOptions,
    ) -> Result<DiffEntriesResult> {
        let specs = options.pathspec_specs();
        let matcher = self.matcher(options)?;
        let mut entries = diff_trees_for_pathspecs_at(
            self.repo.odb(),
            old_tree,
            new_tree,
            &options.prefix,
            options.show_tree_entries,
            specs,
            matcher.as_ref(),
        )?;
        retain_submodule_entries(&mut entries, options.submodules);
        self.pair_with_tree_sources(entries, old_tree, None, None, options)
    }

    /// Diff the trees recorded at two commits (`base` is the old side, `tip` the new side).
    ///
    /// Same as [`Self::tree_to_tree`] on the commits' trees.
    ///
    /// # Errors
    ///
    /// Returns [`RefError::Invalid`](crate::error::RefError::Invalid) when either id is not a
    /// commit, plus the errors of [`Self::tree_to_tree`].
    pub fn commits(
        &self,
        base: &ObjectId,
        tip: &ObjectId,
        options: &TreeDiffOptions,
    ) -> Result<DiffEntriesResult> {
        let odb = self.repo.odb();
        let base_tree = tree_oid_of_commit(odb, base)?;
        let tip_tree = tree_oid_of_commit(odb, tip)?;
        self.tree_to_tree(Some(&base_tree), Some(&tip_tree), options)
    }

    /// Diff the index against a tree (typically `HEAD`'s tree, Git `diff --cached`).
    ///
    /// Uses the index's cache tree to skip unchanged directories when it is valid.
    /// [`SubmoduleIgnoreMode::All`] omits gitlink paths.
    ///
    /// # Parameters
    ///
    /// - `index` - the index to compare (the new side).
    /// - `tree` - the old side, or `None` for the empty tree.
    /// - `options` - pathspec, pairing and submodule policy.
    ///
    /// # Errors
    ///
    /// Propagates ODB, index diff, pathspec and rename detection errors.
    pub fn index_to_tree(
        &self,
        index: &Index,
        tree: Option<&ObjectId>,
        options: &TreeDiffOptions,
    ) -> Result<DiffEntriesResult> {
        let ignore_submodules = options.submodules == SubmoduleIgnoreMode::All;
        let mut entries = diff_index_to_tree(self.repo.odb(), index, tree, ignore_submodules)?;
        self.retain_pathspec(&mut entries, options)?;
        self.pair_with_tree_sources(entries, tree, None, None, options)
    }

    /// Diff the working tree against the index (Git `diff-files`).
    ///
    /// Entries whose stat data matches the index are not read; a changed file's work-tree
    /// side carries a null `new_oid` unless its content had to be hashed to prove it changed.
    /// With a pathspec only the paths it selects are scanned. Submodule checkouts are
    /// compared per [`TreeDiffOptions::submodules`]. With
    /// [`TreeDiffOptions::refresh_index_stat`] unchanged entries adopt fresh stat data in
    /// `index`; nothing is written to disk.
    ///
    /// # Errors
    ///
    /// Returns [`RepositoryError::RequiresWorkTree`] in a bare repository, and work-tree I/O,
    /// ODB, pathspec and rename detection errors.
    pub fn index_to_worktree(
        &self,
        index: &mut Index,
        options: &TreeDiffOptions,
    ) -> Result<DiffEntriesResult> {
        let work_tree = self.work_tree()?;
        let config = self.repo.config().snapshot()?;
        let restrict_to_paths = crate::diff::changes::pathspec_restrict_paths_for_scan(
            self.repo,
            index,
            options.pathspec_specs(),
            None,
        )?;
        let submodules = options.submodules;
        let scan = DiffIndexToWorktreeOptions {
            index_mtime: index_mtime_for_diff(index, None),
            ignore_submodule_untracked: submodules != SubmoduleIgnoreMode::None,
            simplify_gitlinks: matches!(
                submodules,
                SubmoduleIgnoreMode::Dirty | SubmoduleIgnoreMode::All
            ),
            repository_git_dir: Some(self.repo.git_dir().to_path_buf()),
            refresh_index_stat_in_pass: options.refresh_index_stat,
            config: Some(config),
            thread_policy: Some(self.thread_policy(options)),
            for_status: options.for_status,
            refresh_fsmonitor: restrict_to_paths.is_none(),
            command_runner: Some(self.repo.command_runner()),
            restrict_to_paths,
            ..DiffIndexToWorktreeOptions::default()
        };
        let (mut entries, index_stat_refreshed) =
            diff_index_to_worktree_with_options(self.repo.odb(), index, work_tree, scan)?;
        self.retain_pathspec(&mut entries, options)?;
        retain_submodule_entries(&mut entries, submodules);
        let mut result = if options.copies == crate::diff::CopyDetection::Off {
            self.pair_with_tree_sources(entries, None, Some(work_tree), None, options)?
        } else {
            let sources: Vec<(String, FileMode, ObjectId)> = index
                .entries
                .iter()
                .filter(|e| e.stage() == 0 && !e.is_sparse_directory_placeholder())
                .map(|e| {
                    (
                        String::from_utf8_lossy(&e.path).into_owned(),
                        FileMode::new(e.mode),
                        e.oid,
                    )
                })
                .collect();
            self.pair_with_sources(entries, &sources, Some(work_tree), None, false, options)?
        };
        result.index_stat_refreshed = index_stat_refreshed;
        Ok(result)
    }

    /// Diff the working tree against a tree, through `index` (Git `diff <tree>`).
    ///
    /// When the index's cache tree matches `tree`, this is the cheaper index-to-work-tree
    /// scan; otherwise the tree is walked against the work tree. Work-tree bytes go through
    /// the repository's attributes and clean filters. Break-rewrite pairing fills the
    /// work-tree side's blob id like Git. [`SubmoduleIgnoreMode::All`] omits gitlink paths;
    /// the other modes compare a submodule's checked-out commit.
    ///
    /// # Parameters
    ///
    /// - `tree` - the old side, or `None` for the empty tree.
    /// - `index` - the index tracking the work tree.
    /// - `options` - pathspec, pairing and submodule policy.
    ///
    /// # Errors
    ///
    /// Returns [`RepositoryError::RequiresWorkTree`] in a bare repository, and work-tree I/O,
    /// ODB, pathspec and rename detection errors.
    pub fn tree_to_worktree(
        &self,
        tree: Option<&ObjectId>,
        index: &Index,
        options: &TreeDiffOptions,
    ) -> Result<DiffEntriesResult> {
        let work_tree = self.work_tree()?;
        let config = self.repo.config().snapshot()?;
        let restrict_to_paths = crate::diff::changes::pathspec_restrict_paths_for_scan(
            self.repo,
            index,
            options.pathspec_specs(),
            tree,
        )?;
        let mut entries = diff_tree_to_worktree_with_git_dir_and_rules_extras(
            self.repo.odb(),
            tree,
            work_tree,
            self.repo.git_dir(),
            index,
            Some(config.as_ref()),
            None,
            DiffTreeToWorktreeExtras {
                restrict_to_paths,
                ..DiffTreeToWorktreeExtras::default()
            },
        )?;
        self.retain_pathspec(&mut entries, options)?;
        retain_submodule_entries(&mut entries, options.submodules);
        let worktree = DiffPostprocessWorktree { index };
        self.pair_with_tree_sources(entries, tree, Some(work_tree), Some(&worktree), options)
    }

    /// Apply the rename, copy and break pairing of `options` to entries listed elsewhere.
    ///
    /// Old sides are read from the object database; a new side with a null `new_oid` is read
    /// from the work tree. The pathspec, prefix and submodule fields of `options` are not used.
    ///
    /// # Parameters
    ///
    /// - `entries` - changed paths to pair, e.g. a filtered [`Self::tree_to_tree`] result.
    /// - `source_tree` - with
    ///   [`CopyDetection::FindHarder`](crate::diff::CopyDetection::FindHarder), every blob in
    ///   this tree is a copy source as well; `None` limits sources to `entries`.
    /// - `options` - rename, copy and break pairing and thread limits.
    ///
    /// # Errors
    ///
    /// Propagates object and work-tree reads while scoring pairs.
    pub fn detect_renames(
        &self,
        entries: Vec<DiffEntry>,
        source_tree: Option<&ObjectId>,
        options: &TreeDiffOptions,
    ) -> Result<DiffEntriesResult> {
        self.pair_with_tree_sources(entries, source_tree, self.repo.work_tree(), None, options)
    }

    /// Staged, unstaged, and working-tree diffs over the repository's on-disk index.
    ///
    /// # Errors
    ///
    /// Returns index read errors.
    pub fn uncommitted(&self) -> Result<UncommittedDiff<'repo>> {
        let index = self.repo.load_index_preserving_sparse()?;
        Ok(UncommittedDiff::new(self.repo, index))
    }

    /// Changed paths for `target`, limited to `pathspecs` and to the statuses in
    /// `options.filter` (`--diff-filter`), sorted by path. No contents are loaded.
    ///
    /// Commit and tree targets diff stored trees without reading the index. Targets that read
    /// the index or working tree load it once here; when their contents are needed next, call
    /// [`UncommittedDiff::entries`] on [`Self::uncommitted`] instead and load them with
    /// [`UncommittedDiff::load_worktree_changes`] or [`Self::object_changes`] on the same
    /// handle.
    ///
    /// # Parameters
    ///
    /// - `target` - what to compare, e.g. from [`Self::resolve_request`].
    /// - `pathspec` - repository-relative pathspecs; an entire-tree pathspec means every path.
    /// - `options` - only `renames` and `filter` are used.
    ///
    /// # Errors
    ///
    /// Returns object read, rename detection, and pathspec errors, plus
    /// [`RepositoryError::RequiresWorkTree`] for a working-tree target in a bare repository.
    pub fn entries(
        &self,
        target: &DiffTarget,
        pathspec: &Pathspec,
        options: &DiffOptions,
    ) -> Result<DiffEntriesResult> {
        let specs = pathspec.specs();
        match stored_target_entries(self.repo, target, specs, options)? {
            Some(result) => limit_target_entries(self.repo, result, specs, options),
            None => self.uncommitted()?.entries(target, pathspec, options),
        }
    }

    /// Structured per-file diffs for `target`, honoring `pathspecs` and line-diff `opts`.
    ///
    /// # Errors
    ///
    /// Propagates index, ODB, and worktree diff failures.
    pub fn files(
        &self,
        target: &DiffTarget,
        pathspec: &Pathspec,
        opts: &DiffOptions,
    ) -> Result<Vec<FileDiff>> {
        collect_files_for_target(self.repo, target, pathspec.specs(), opts.clone())
    }

    /// Patch-style rows from [`FileChange`] values with `.gitattributes` funcname context.
    ///
    /// # Errors
    ///
    /// Propagates attribute and config errors.
    pub fn file_diffs_from_changes(
        &self,
        changes: &[FileChange],
        opts: DiffOptions,
    ) -> Result<Vec<FileDiff>> {
        crate::diff::targets::file_diffs_from_changes(self.repo, changes, opts)
    }

    /// Patch-style rows from tree diff entries with `.gitattributes` funcname context.
    ///
    /// # Errors
    ///
    /// Propagates object database and attribute errors.
    pub fn file_diffs_from_entries(
        &self,
        entries: Vec<DiffEntry>,
        opts: DiffOptions,
    ) -> Result<Vec<FileDiff>> {
        crate::diff::targets::file_diffs_from_entries(self.repo, entries, opts)
    }

    /// Per-file diffs at `detail` for entries from a metadata diff.
    ///
    /// Applies [`DiffOptions::renames`] to the entries first (Git's default `-M50`); pass
    /// [`RenameDetection::Off`](crate::diff::RenameDetection::Off) when the entries are already
    /// paired. [`DiffDetail::NameOnly`] and [`DiffDetail::NameStatus`] load no contents;
    /// [`DiffDetail::Stat`] counts lines and [`DiffDetail::Patch`] builds hunks, in parallel
    /// when the repository's thread policy allows.
    ///
    /// # Errors
    ///
    /// Propagates object database and parallel diff errors.
    pub fn files_for_entries(
        &self,
        entries: Vec<DiffEntry>,
        detail: DiffDetail,
        options: &DiffOptions,
    ) -> Result<DiffFilesOutput> {
        let config = self.repo.config().snapshot()?;
        crate::diff::files::build_file_diffs(
            self.repo.odb(),
            entries,
            detail,
            options.clone(),
            config.as_ref(),
            &self.repo.thread_policy(),
        )
    }

    /// Patch rows for `entries` one file at a time, without retaining every hunk list.
    ///
    /// Like [`Self::files_for_entries`] at [`DiffDetail::Patch`], but `emit` receives each
    /// file in path order and peak memory stays near one file's diff.
    ///
    /// # Errors
    ///
    /// Propagates object database, parallel diff and `emit` errors.
    pub fn for_each_file(
        &self,
        entries: Vec<DiffEntry>,
        options: &DiffOptions,
        emit: impl FnMut(ListedFileDiff) -> Result<()>,
    ) -> Result<()> {
        let config = self.repo.config().snapshot()?;
        crate::diff::files::build_file_diffs_streaming(
            self.repo.odb(),
            entries,
            options.clone(),
            config.as_ref(),
            &self.repo.thread_policy(),
            emit,
        )
    }

    /// Diffstat (per-file insertions and deletions) for `entries`.
    ///
    /// Same rename handling as [`Self::files_for_entries`] at [`DiffDetail::Stat`].
    ///
    /// # Errors
    ///
    /// Propagates object database and parallel diff errors.
    pub fn stat(&self, entries: Vec<DiffEntry>, options: &DiffOptions) -> Result<DiffStat> {
        let output = self.files_for_entries(entries, DiffDetail::Stat, options)?;
        Ok(DiffStat::from_files(&output.files))
    }

    /// Structured hunks for every entry, in input order, computed in parallel when the
    /// repository's thread policy allows. Entries are used as given (no rename pairing).
    ///
    /// # Errors
    ///
    /// Propagates object database and parallel diff errors.
    pub fn structured(
        &self,
        entries: Vec<DiffEntry>,
        options: &DiffOptions,
    ) -> Result<Vec<TreeDiffStructured>> {
        let config = self.repo.config().snapshot()?;
        crate::diff::tree_content::tree_diff_structured_parallel(
            self.repo.odb(),
            entries,
            config.as_ref(),
            &self.repo.thread_policy(),
            options.clone(),
        )
    }

    /// Like [`Self::structured`], but hands each file to `visit` in input order instead of
    /// collecting them, so only a bounded window of results is held at once.
    ///
    /// # Errors
    ///
    /// Propagates object database, parallel diff and `visit` errors.
    pub fn for_each_structured(
        &self,
        entries: &[DiffEntry],
        options: &DiffOptions,
        visit: impl FnMut(TreeDiffStructured) -> Result<()>,
    ) -> Result<()> {
        let config = self.repo.config().snapshot()?;
        crate::diff::tree_content::for_each_tree_diff_structured(
            self.repo.odb(),
            entries,
            options.clone(),
            config.as_ref(),
            &self.repo.thread_policy(),
            visit,
        )
    }

    /// Per-file diffs for one commit against its parents (Git `show` / `log -p` for a commit).
    ///
    /// A root commit is compared with the empty tree. A merge (two or more effective parents,
    /// after grafts and shallow boundaries) is diffed per `merges`.
    ///
    /// # Parameters
    ///
    /// - `commit` - the commit to show.
    /// - `merges` - how merge commits are diffed.
    /// - `detail` - how much to compute per changed file.
    /// - `options` - line-diff and rename settings.
    /// - `pathspec` - paths to keep; an entire-tree pathspec keeps every path.
    ///
    /// # Errors
    ///
    /// Propagates object read, tree diff, pathspec and parallel diff errors.
    pub fn commit_files(
        &self,
        commit: &ObjectId,
        merges: DiffMerges,
        detail: DiffDetail,
        options: &DiffOptions,
        pathspec: &Pathspec,
    ) -> Result<DiffFilesOutput> {
        crate::diff::merge_commit::commit_diff_files(
            self.repo,
            commit,
            merges,
            detail,
            options,
            pathspec,
            &self.repo.thread_policy(),
        )
    }

    /// Load old and new contents for entries that compare stored objects (trees, commits or
    /// the index against a tree), in parallel when the thread policy allows.
    ///
    /// Work-tree entries need [`UncommittedDiff::load_worktree_changes`] instead.
    ///
    /// # Errors
    ///
    /// Propagates object read errors.
    pub fn object_changes(&self, entries: &[DiffEntry]) -> Result<Vec<FileChange>> {
        crate::diff::changes::load_object_changes(self.repo, entries)
    }

    /// Unstaged file diffs for `pathspec` plus the hunks other paths share with them, the
    /// input for hunk-level staging by [`HunkId`](crate::diff::HunkId).
    ///
    /// # Errors
    ///
    /// Returns [`RepositoryError::RequiresWorkTree`] in a bare repository, and index, ODB and
    /// work-tree diff errors.
    pub fn hunk_bundle(
        &self,
        pathspec: &Pathspec,
        options: &DiffOptions,
    ) -> Result<HunkDiffBundle> {
        crate::diff::targets::hunk_diff_bundle(self.repo, pathspec, options.clone())
    }

    /// Turn a user's diff request (staged/unstaged flags, revisions and paths) into a
    /// [`DiffTarget`] and resolved pathspecs.
    ///
    /// # Parameters
    ///
    /// - `cwd_prefix` - work-tree-relative directory the paths are relative to.
    /// - `request` - what the user asked for.
    ///
    /// # Errors
    ///
    /// Returns [`DiffError::Arguments`](crate::error::DiffError::Arguments) with a
    /// [`ResolveDiffArgumentsError`](crate::diff::ResolveDiffArgumentsError) for conflicting flags, unknown or non-commit revisions,
    /// bad pathspecs, and work-tree targets in a bare repository, plus revision errors such as
    /// [`RevisionError::NoMergeBase`](crate::error::RevisionError::NoMergeBase) for ranges.
    pub fn resolve_request(
        &self,
        cwd_prefix: Option<&str>,
        request: &DiffRequest,
    ) -> Result<DiffArguments> {
        crate::diff::targets::resolve_diff_request(self.repo, cwd_prefix, request)
    }

    /// Resolve pathspecs that limit a commit diff (`show`, `log -p`), also in a bare
    /// repository, where they are relative to the repository root.
    ///
    /// # Errors
    ///
    /// Returns [`DiffError::Arguments`](crate::error::DiffError::Arguments) with a
    /// [`ResolveDiffArgumentsError`](crate::diff::ResolveDiffArgumentsError) for pathspecs outside the repository or with invalid
    /// magic.
    pub fn resolve_pathspec(
        &self,
        cwd_prefix: Option<&str>,
        raw_paths: &[String],
    ) -> Result<Pathspec> {
        crate::diff::targets::resolve_commit_diff_pathspecs(self.repo, cwd_prefix, raw_paths)
    }

    fn work_tree(&self) -> Result<&'repo Path> {
        Ok(self
            .repo
            .work_tree()
            .ok_or(RepositoryError::RequiresWorkTree)?)
    }

    fn thread_policy(&self, options: &TreeDiffOptions) -> ThreadPolicy {
        options.threads.unwrap_or_else(|| self.repo.thread_policy())
    }

    fn matcher<'s>(&self, options: &'s TreeDiffOptions) -> Result<Option<PathspecMatcher<'s>>> {
        let Some(pathspec) = options.pathspec.as_ref() else {
            return Ok(None);
        };
        if pathspec.specs().is_empty() {
            return Ok(None);
        }
        Ok(Some(self.repo.pathspec_matcher_with_globals(
            pathspec.specs(),
            pathspec.globals(),
        )?))
    }

    fn retain_pathspec(
        &self,
        entries: &mut Vec<DiffEntry>,
        options: &TreeDiffOptions,
    ) -> Result<()> {
        if let Some(matcher) = self.matcher(options)? {
            entries.retain(|entry| matcher.matches_change(entry.path(), entry.old_path.as_deref()));
        }
        Ok(())
    }

    /// Pair `entries` per `options`, with the blobs of `source_tree` as copy sources.
    fn pair_with_tree_sources(
        &self,
        entries: Vec<DiffEntry>,
        source_tree: Option<&ObjectId>,
        work_root: Option<&Path>,
        worktree: Option<&DiffPostprocessWorktree<'_>>,
        options: &TreeDiffOptions,
    ) -> Result<DiffEntriesResult> {
        if !needs_postprocess(&entries, options) {
            return self.pair_with_sources(entries, &[], work_root, worktree, false, options);
        }
        let config = self.repo.config().snapshot()?;
        let mut notice = None;
        let source_tree = source_tree.filter(|_| options.copies != crate::diff::CopyDetection::Off);
        let entries = postprocess_diff_entries(
            self.repo.odb(),
            entries,
            source_tree,
            work_root,
            worktree,
            config.as_ref(),
            &self.thread_policy(options),
            &options.postprocess_options(worktree.is_some()),
            Some(&mut notice),
        )?;
        Ok(DiffEntriesResult {
            entries,
            inexact_rename_skipped: notice,
            ..DiffEntriesResult::default()
        })
    }

    /// Pair `entries` per `options`, with `sources` as the extra copy sources.
    fn pair_with_sources(
        &self,
        entries: Vec<DiffEntry>,
        sources: &[(String, FileMode, ObjectId)],
        work_root: Option<&Path>,
        worktree: Option<&DiffPostprocessWorktree<'_>>,
        materialize_worktree_oid: bool,
        options: &TreeDiffOptions,
    ) -> Result<DiffEntriesResult> {
        if entries.is_empty() || !options.pairs_entries() {
            return Ok(DiffEntriesResult::new(entries));
        }
        let config = self.repo.config().snapshot()?;
        let thread_policy = self.thread_policy(options);
        if !needs_postprocess(&entries, options) {
            let Some(threshold) = options.renames.threshold_percent() else {
                return Ok(DiffEntriesResult::new(entries));
            };
            let out = detect_renames_with_policy(
                self.repo.odb(),
                work_root,
                entries,
                threshold,
                config.as_ref(),
                &thread_policy,
                None,
                DetectRenamesOptions::default(),
            )?;
            return Ok(DiffEntriesResult {
                entries: out.entries,
                inexact_rename_skipped: out.inexact_skipped,
                ..DiffEntriesResult::default()
            });
        }
        let mut notice = None;
        let entries = postprocess_diff_entries_from_sources(
            self.repo.odb(),
            entries,
            sources,
            work_root,
            worktree,
            config.as_ref(),
            &thread_policy,
            &options.postprocess_options(materialize_worktree_oid),
            Some(&mut notice),
        )?;
        Ok(DiffEntriesResult {
            entries,
            inexact_rename_skipped: notice,
            ..DiffEntriesResult::default()
        })
    }
}

/// Copy and break pairing go through the post-processing pipeline; renames alone pair
/// directly, so `-M100` (exact renames only) keeps its meaning.
fn needs_postprocess(entries: &[DiffEntry], options: &TreeDiffOptions) -> bool {
    !entries.is_empty()
        && (options.copies != crate::diff::CopyDetection::Off
            || options.break_rewrites != crate::diff::BreakRewrites::Off)
}

/// Drop changes whose every present side is a gitlink when submodules are ignored entirely.
fn retain_submodule_entries(entries: &mut Vec<DiffEntry>, submodules: SubmoduleIgnoreMode) {
    if submodules != SubmoduleIgnoreMode::All {
        return;
    }
    entries.retain(|entry| {
        let gitlink_or_absent = |mode: FileMode| mode.is_gitlink() || mode == FileMode::ABSENT;
        !(gitlink_or_absent(entry.old_mode) && gitlink_or_absent(entry.new_mode))
    });
}

impl<'repo> Diff<'repo> {
    /// Attribute each line of a worktree path to a commit (blame).
    ///
    /// # Errors
    ///
    /// Propagates blame resolution, object read, and worktree failures documented on
    /// [`crate::blame::blame_file`].
    pub fn blame(
        &self,
        path: &str,
        options: &crate::blame::BlameOptions,
    ) -> Result<crate::blame::BlameResult> {
        crate::blame::blame_file(self.repo, path, options)
    }

    /// Resolve a Git-style `-L` range for blame on `path` at a fixed start commit.
    pub fn resolve_blame_line_range(
        &self,
        start: ObjectId,
        path: &str,
        spec: &str,
    ) -> std::result::Result<std::ops::RangeInclusive<usize>, crate::line_range::LineRangeError>
    {
        crate::blame::resolve_blame_line_range(self.repo, start, path, spec)
    }

    /// Resolve a Git-style `-L` range using full [`crate::blame::BlameOptions`].
    pub fn resolve_blame_line_range_with_options(
        &self,
        path: &str,
        spec: &str,
        options: &crate::blame::BlameOptions,
    ) -> std::result::Result<std::ops::RangeInclusive<usize>, crate::line_range::LineRangeError>
    {
        crate::blame::resolve_blame_line_range_with_options(self.repo, path, spec, options)
    }
}
