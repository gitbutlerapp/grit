//! Merge bases, ancestry and reachability counts on [`RevWalk`].
use std::collections::HashSet;

use crate::error::Result;
use crate::merge_base::{
    self, ReachableCountFilter, ReachableWalkCommitterCutoffs, ReachableWalkLimit,
};
use crate::objects::ObjectId;
use crate::revwalk::RevWalk;

/// Which merge bases [`RevWalk::merge_bases`] computes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MergeBaseMode {
    /// `git merge-base A B...`: the best common ancestor of the first commit and a
    /// hypothetical merge of the others. At most one id.
    #[default]
    Best,
    /// `git merge-base --all`: every best common ancestor, newest committer date first for
    /// two commits.
    All,
    /// `git merge-base --octopus`: the best ancestors common to every commit.
    Octopus,
}

/// Options for [`RevWalk::reachable_excluding`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReachableWalkOptions {
    /// How the walk bounds its output.
    pub limit: ReachableWalkLimit,
    /// Committer-date cutoffs (`--since` / `--until`).
    pub cutoffs: ReachableWalkCommitterCutoffs,
}

impl ReachableWalkOptions {
    /// Sets [`limit`](Self::limit).
    #[must_use]
    pub fn with_limit(mut self, limit: ReachableWalkLimit) -> Self {
        self.limit = limit;
        self
    }

    /// Sets [`cutoffs`](Self::cutoffs).
    #[must_use]
    pub fn with_cutoffs(mut self, cutoffs: ReachableWalkCommitterCutoffs) -> Self {
        self.cutoffs = cutoffs;
        self
    }
}

/// Options for [`RevWalk::ahead_behind`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct AheadBehindOptions {
    /// How many of the newest ahead commits to return in [`AheadBehind::newest_ahead`].
    pub retain_newest_ahead: usize,
    /// How many of the newest behind commits to return in [`AheadBehind::newest_behind`].
    pub retain_newest_behind: usize,
}

impl AheadBehindOptions {
    /// Sets [`retain_newest_ahead`](Self::retain_newest_ahead).
    #[must_use]
    pub fn with_retain_newest_ahead(mut self, count: usize) -> Self {
        self.retain_newest_ahead = count;
        self
    }

    /// Sets [`retain_newest_behind`](Self::retain_newest_behind).
    #[must_use]
    pub fn with_retain_newest_behind(mut self, count: usize) -> Self {
        self.retain_newest_behind = count;
        self
    }
}

/// How far a commit is ahead of and behind another ([`RevWalk::ahead_behind`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct AheadBehind {
    /// Commits reachable from the head but not from the target.
    pub ahead: usize,
    /// Commits reachable from the target but not from the head.
    pub behind: usize,
    /// The newest ahead commits, newest first, up to
    /// [`AheadBehindOptions::retain_newest_ahead`].
    pub newest_ahead: Vec<ObjectId>,
    /// The newest behind commits, newest first, up to
    /// [`AheadBehindOptions::retain_newest_behind`].
    pub newest_behind: Vec<ObjectId>,
}

impl<'repo> RevWalk<'repo> {
    /// Merge bases of `commits` (`git merge-base`, `--all` or `--octopus` by `mode`).
    ///
    /// Returns an empty list for no commits or unrelated histories, and the commit itself
    /// for one commit.
    ///
    /// # Errors
    ///
    /// Returns object read and commit parse errors from the walk.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> grit_lib::error::Result<()> {
    /// use grit_lib::objects::ObjectKind;
    /// use grit_lib::refs::store::RefName;
    /// use grit_lib::repo::{InitOptions, Repository};
    /// use grit_lib::revwalk::{MergeBaseMode, ResolveOptions};
    /// let dir = tempfile::tempdir()?;
    /// let options = InitOptions::default().with_initial_branch("main");
    /// let (repo, _) = Repository::init(dir.path(), &options)?;
    /// let tree = repo.odb().write(ObjectKind::Tree, b"")?;
    /// let body = format!("tree {tree}\nauthor A <a@example.com> 1700000000 +0000\ncommitter A <a@example.com> 1700000000 +0000\n\ndoc\n");
    /// let commit = repo.odb().write(ObjectKind::Commit, body.as_bytes())?;
    /// let main = RefName::try_from("refs/heads/main").expect("valid ref name");
    /// repo.references().update(&main, &commit)?;
    /// let revwalk = repo.revwalk();
    /// let head = revwalk.resolve("HEAD", &ResolveOptions::default())?;
    /// assert_eq!(revwalk.merge_bases(&[head, head], MergeBaseMode::Best)?, vec![head]);
    /// assert!(revwalk.is_ancestor(head, head)?);
    /// # Ok(())
    /// # }
    /// ```
    pub fn merge_bases(&self, commits: &[ObjectId], mode: MergeBaseMode) -> Result<Vec<ObjectId>> {
        let repo = self.repository();
        match mode {
            MergeBaseMode::Best => match commits.split_first() {
                Some((&first, rest)) => merge_base::merge_bases_first_vs_rest(repo, first, rest),
                None => Ok(Vec::new()),
            },
            MergeBaseMode::All => merge_base::merge_bases_all(repo, commits),
            MergeBaseMode::Octopus => merge_base::merge_bases_octopus(repo, commits),
        }
    }

    /// The single merge base of `a` and `b`, as `git diff --merge-base` needs it.
    ///
    /// # Errors
    ///
    /// Returns [`RevisionError::NoMergeBase`](crate::error::RevisionError::NoMergeBase) for
    /// unrelated histories,
    /// [`RevisionError::MultipleMergeBases`](crate::error::RevisionError::MultipleMergeBases) when
    /// there is more than one base, and object read errors from the walk.
    pub fn merge_base_for_diff(&self, a: ObjectId, b: ObjectId) -> Result<ObjectId> {
        merge_base::merge_base_for_diff_two_commits(self.repository(), a, b)
    }

    /// Whether `ancestor` is reachable from `descendant` (`git merge-base --is-ancestor`).
    /// A commit is its own ancestor.
    ///
    /// # Errors
    ///
    /// Returns object read and commit parse errors from the walk.
    pub fn is_ancestor(&self, ancestor: ObjectId, descendant: ObjectId) -> Result<bool> {
        merge_base::is_ancestor(self.repository(), ancestor, descendant)
    }

    /// The commits of `commits` that no other one of them can reach
    /// (`git merge-base --independent`), in input order.
    ///
    /// # Errors
    ///
    /// Returns object read and commit parse errors from the walk.
    pub fn independent(&self, commits: &[ObjectId]) -> Result<Vec<ObjectId>> {
        merge_base::independent_commits(self.repository(), commits)
    }

    /// Every commit reachable from `tip`, including `tip`.
    ///
    /// # Errors
    ///
    /// Returns object read and commit parse errors from the walk.
    pub fn ancestor_closure(&self, tip: ObjectId) -> Result<HashSet<ObjectId>> {
        merge_base::ancestor_closure(self.repository(), tip)
    }

    /// Fork point of `head` from the upstream named `upstream_spec`, as
    /// `git merge-base --fork-point` computes it: the merge base of `head` and every commit
    /// the upstream's reflog has held, when that base is itself one of those commits.
    ///
    /// Returns [`None`] when there is no such commit (Git prints nothing and exits 1), for
    /// example when the upstream was rewritten so that its old tips share several merge
    /// bases with `head`.
    ///
    /// # Parameters
    ///
    /// - `upstream_spec` - the upstream as the user named it (`origin/main`, `main@{u}`);
    ///   picks the reflog to read.
    /// - `upstream_tip` - the commit `upstream_spec` resolves to, used when the reflog is
    ///   empty.
    /// - `head` - the commit whose fork point is wanted.
    ///
    /// # Errors
    ///
    /// Returns `@{upstream}` resolution, reflog, object read and commit parse errors.
    pub fn fork_point(
        &self,
        upstream_spec: &str,
        upstream_tip: ObjectId,
        head: ObjectId,
    ) -> Result<Option<ObjectId>> {
        let repo = self.repository();
        let reflog_ref = if crate::rev_parse::upstream_suffix_info(upstream_spec).is_some() {
            crate::rev_parse::resolve_upstream_symbolic_name(repo, upstream_spec)?
        } else {
            merge_base::resolve_fork_point_reflog_ref(repo, upstream_spec)
        };
        merge_base::try_reflog_fork_point(repo, &reflog_ref, upstream_tip, head)
    }

    /// Commit counts `head` is ahead of and behind `target`, optionally with the newest
    /// commits on each side.
    ///
    /// # Errors
    ///
    /// Returns object read and commit parse errors from the walk.
    pub fn ahead_behind(
        &self,
        head: ObjectId,
        target: ObjectId,
        options: &AheadBehindOptions,
    ) -> Result<AheadBehind> {
        let (ahead, behind, newest_ahead, newest_behind) =
            merge_base::ahead_behind_of_target_commits(
                self.repository(),
                head,
                target,
                options.retain_newest_ahead,
                options.retain_newest_behind,
            )?;
        Ok(AheadBehind {
            ahead,
            behind,
            newest_ahead,
            newest_behind,
        })
    }

    /// Walk the commits reachable from `tips` that are not ancestors of any `hide` commit,
    /// newest committer date first (`git rev-list tips ^hide`).
    ///
    /// Returns `(emitted, commits)`: with [`ReachableWalkLimit::CountAllRetainNewest`],
    /// `emitted` counts the whole set while `commits` holds only the newest ones.
    ///
    /// # Errors
    ///
    /// Returns object read and commit parse errors from the walk.
    pub fn reachable_excluding(
        &self,
        tips: &[ObjectId],
        hide: &[ObjectId],
        options: &ReachableWalkOptions,
    ) -> Result<(usize, Vec<ObjectId>)> {
        merge_base::walk_commits_reachable_excluding_ancestors_of_cutoffs(
            self.repository(),
            tips,
            hide,
            options.limit,
            options.cutoffs,
        )
    }

    /// Count the commits reachable from `tips` that are not ancestors of any `hide` commit
    /// (`git rev-list --count tips ^hide`), without collecting them.
    ///
    /// Uses pack bitmaps and the commit-graph when they apply.
    ///
    /// # Errors
    ///
    /// Returns object read and commit parse errors from the walk.
    pub fn count_reachable_excluding(
        &self,
        tips: &[ObjectId],
        hide: &[ObjectId],
        filter: ReachableCountFilter,
    ) -> Result<usize> {
        merge_base::count_commits_reachable_excluding_ancestors_of(
            self.repository(),
            tips,
            hide,
            filter,
        )
    }
}
