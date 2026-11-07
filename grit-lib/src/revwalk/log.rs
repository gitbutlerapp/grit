//! Log rows on [`RevWalk`]: ref decorations and commit metadata for `log`, `show` and `revs`.
use crate::error::Result;
use crate::objects::ObjectId;
use crate::porcelain::log::{self, CommitReuse, DecorationMap, DecorationOptions, LoadedCommit};
use crate::revwalk::RevWalk;

impl RevWalk<'_> {
    /// Ref decorations (heads, tags, remotes, the stash, replace refs and `HEAD`) for the
    /// commits they point at, keyed by commit hex.
    ///
    /// Each commit's list is in Git's `log --decorate` order: refs are walked in ascending
    /// name order and each is prepended, so `tag: A1` comes before `other/main` before
    /// `other/HEAD`. Collect once per command and reuse the map for every row.
    ///
    /// # Errors
    ///
    /// Returns ref listing, `HEAD` resolution, and object read errors.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> grit_lib::error::Result<()> {
    /// use grit_lib::ident::Signature;
    /// use grit_lib::porcelain::commit::{create_commit, CommitRequest};
    /// use grit_lib::porcelain::log::DecorationOptions;
    /// use grit_lib::progress::{CancelToken, NullProgress};
    /// use grit_lib::repo::{InitOptions, Repository};
    /// use grit_lib::revwalk::ResolveOptions;
    /// let dir = tempfile::tempdir()?;
    /// let (repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
    /// let mut req = CommitRequest::new("doc");
    /// req.author = Some(Signature::new("Doc", "doc@example.com", repo.identity_now())?);
    /// req.committer = Some(Signature::new("Doc", "doc@example.com", repo.identity_now())?);
    /// req.allow_empty = true;
    /// create_commit(&repo, &req, &mut NullProgress, &CancelToken::never())?;
    /// let revwalk = repo.revwalk();
    /// let head = revwalk.resolve("HEAD", &ResolveOptions::default())?;
    /// let decorations = revwalk.decorations(&DecorationOptions::default())?;
    /// let rows = revwalk.load_commits(&[head], &decorations, &mut ())?;
    /// assert_eq!(rows[0].message.trim(), "doc");
    /// assert!(!rows[0].refs.is_empty());
    /// # Ok(())
    /// # }
    /// ```
    pub fn decorations(&self, options: &DecorationOptions) -> Result<DecorationMap> {
        log::collect_decorations(self.repository(), options)
    }

    /// Commit metadata for `oids`, in order, with the ref labels from `decorations`
    /// ([`Self::decorations`]).
    ///
    /// Commits the caller already holds are taken out of `parsed` (for example
    /// [`RevListResult::commit_data`](crate::revwalk::rev_list::RevListResult::commit_data) or
    /// [`LogWalkPage::commit_data`](crate::revwalk::log_walk::LogWalkPage::commit_data))
    /// instead of read again; pass `&mut ()` to read every commit from the object database.
    ///
    /// # Errors
    ///
    /// Returns ODB and commit-parse errors when an oid is missing or corrupt.
    pub fn load_commits(
        &self,
        oids: &[ObjectId],
        decorations: &DecorationMap,
        parsed: &mut impl CommitReuse,
    ) -> Result<Vec<LoadedCommit>> {
        log::load_commits(self.repository(), oids, decorations, parsed)
    }
}
