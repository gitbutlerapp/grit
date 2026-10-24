//! Repository-scoped revision parsing and walking via [`RevWalk`].
use crate::error::Result;
use crate::object_inspect::{ResolveError, ResolvedObject};
use crate::objects::ObjectId;
use crate::repo::Repository;
use crate::rev_list::{rev_list, split_revision_token, RevListOptions, RevListResult};
use crate::rev_parse::resolve_revision;

/// Parse revision specs and walk commit history for one repository.
#[derive(Debug, Copy, Clone)]
pub struct RevWalk<'repo> {
    repo: &'repo Repository,
}

impl<'repo> RevWalk<'repo> {
    /// Bind rev-parse and rev-list to `repo`.
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

    /// Resolve a user revision spec (for example `HEAD`, `main~2`, `v1.0`).
    ///
    /// # Errors
    ///
    /// Propagates rev-parse and ref errors.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> grit_lib::error::Result<()> {
    /// use grit_lib::repo::{InitOptions, Repository};
    /// use grit_lib::porcelain::commit::{create_commit, CommitRequest};
    /// use grit_lib::progress::NullProgress;
    /// let dir = std::env::temp_dir().join("grit-revwalk-parse-doc");
    /// let repo = Repository::init(&dir, &InitOptions::default())?;
    /// let req = CommitRequest {
    ///     message: "doc".into(),
    ///     author: "Doc <doc@example.com>".into(),
    ///     committer: "Doc <doc@example.com>".into(),
    ///     allow_empty: true,
    ///     sign_override: None,
    ///     amend: false,
    ///     assume_worktree_synced: false,
    ///     skip_pre_commit_tree_match: false,
    ///     ..Default::default()
    /// };
    /// let _ = create_commit(&repo, &req, &mut NullProgress)?;
    /// let _ = repo.rev_walk().resolve("refs/heads/main")?;
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub fn resolve(&self, spec: &str) -> Result<ObjectId> {
        resolve_revision(self.repo, spec)
    }

    /// Resolve `spec` to oid, kind, size, and optional ref name ([`ResolvedObject`]).
    ///
    /// # Errors
    ///
    /// See [`Repository::resolve`].
    #[inline]
    pub fn resolve_object(&self, spec: &str) -> std::result::Result<ResolvedObject, ResolveError> {
        self.repo.resolve(spec)
    }

    /// Walk commits matching `spec` (supports `A..B`, `^tip`, and plain tips).
    ///
    /// Uses the same [`RevListOptions`] defaults as [`rev_list`]: commit-graph acceleration
    /// and bounded `-n` early-stop apply with [`RevListOptions::default`].
    ///
    /// # Errors
    ///
    /// Propagates rev-parse and rev-list errors.
    ///
    /// # Examples
    ///
    /// See the library guide page on [revision walking](https://grit-scm.com/docs/library/revwalk/).
    #[inline]
    pub fn walk(&self, spec: &str, options: &RevListOptions) -> Result<RevListResult> {
        let (positive, negative) = split_revision_token(spec);
        rev_list(self.repo, &positive, &negative, options)
    }
}
