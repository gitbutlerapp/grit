//! Repository-scoped revision parsing and walking via [`RevWalk`].
use crate::error::Result;
use crate::object_inspect::{ResolveError, ResolvedObject};
use crate::objects::ObjectId;
use crate::repo::Repository;
use crate::rev_list::{rev_list, RevListOptions, RevListResult};
use crate::rev_list_error::RevListError;
use crate::rev_parse::resolve_revision;
use crate::revwalk::parse_rev_walk_tokens;
use std::borrow::Cow;

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
    /// use grit_lib::ident::Signature;
    /// use grit_lib::porcelain::commit::{create_commit, CommitRequest};
    /// use grit_lib::progress::NullProgress;
    /// let dir = std::env::temp_dir().join("grit-revwalk-parse-doc");
    /// let repo = Repository::init(&dir, &InitOptions::default())?;
    /// let req = CommitRequest {
    ///     message: "doc".into(),
    ///     author: Some(Signature::new("Doc", "doc@example.com", repo.identity_now())?),
    ///     committer: Some(Signature::new("Doc", "doc@example.com", repo.identity_now())?),
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

    /// Walk commits matching one revision `spec`: a plain tip (`main`, `HEAD~3`), an exclusion
    /// (`^tip`), a range (`A..B`), or a symmetric difference (`A...B`, like `git rev-list A...B`).
    /// Either side of a range may be empty and defaults to `HEAD`.
    ///
    /// This is the low-level entry point: it takes the [`rev_list`] engine's full
    /// [`RevListOptions`]. For several tokens, ref selections (`--all`, `--branches`), or path
    /// limits use [`RevWalk::commits`] / [`RevWalk::rev_list_result`] with
    /// [`RevWalkListOptions`](crate::revwalk::RevWalkListOptions). For `A...B`,
    /// `symmetric_left` / `symmetric_right` are filled from the range unless already set, so
    /// `left_right`, `left_only`, and `right_only` work as in Git.
    ///
    /// Commit-graph acceleration and bounded `-n` early-stop apply with
    /// [`RevListOptions::default`].
    ///
    /// # Errors
    ///
    /// - [`RevListError::UnsupportedWalkSpec`] when `spec` starts with `-` (an option or `--`)
    ///   or holds several whitespace-separated tokens.
    /// - Rev-parse, merge-base, and rev-list errors otherwise.
    ///
    /// # Examples
    ///
    /// See the library guide page on [revision walking](https://grit-scm.com/docs/library/revwalk/).
    pub fn walk(&self, spec: &str, options: &RevListOptions) -> Result<RevListResult> {
        if !is_single_walk_spec(spec) {
            return Err(RevListError::UnsupportedWalkSpec {
                spec: spec.to_owned(),
            }
            .into());
        }
        let parsed = parse_rev_walk_tokens(self.repo, &[spec])?;
        let options = if parsed.symmetric_left.is_some()
            && (options.symmetric_left.is_none() || options.symmetric_right.is_none())
        {
            let mut symmetric = options.clone();
            symmetric.symmetric_left = symmetric.symmetric_left.or(parsed.symmetric_left);
            symmetric.symmetric_right = symmetric.symmetric_right.or(parsed.symmetric_right);
            Cow::Owned(symmetric)
        } else {
            Cow::Borrowed(options)
        };
        rev_list(self.repo, &parsed.positive, &parsed.negative, &options)
    }
}

/// Whether `spec` can be one revision expression rather than an option or a token list.
///
/// Whitespace is legal inside `@{...}` / `^{...}` (reflog dates, message searches) and after a
/// `:` (`:/message`, `rev:path`), so only whitespace outside those forms marks several tokens.
fn is_single_walk_spec(spec: &str) -> bool {
    if spec.starts_with('-') {
        return false;
    }
    let mut brace_depth = 0usize;
    for c in spec.chars() {
        match c {
            ':' if brace_depth == 0 => return true,
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            c if c.is_whitespace() && brace_depth == 0 => return false,
            _ => {}
        }
    }
    true
}
