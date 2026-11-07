//! Revision resolution on [`RevWalk`]: [`RevWalk::resolve`] with [`ResolveOptions`], peeling,
//! abbreviation and symbolic names.
use crate::error::Result;
use crate::objects::ObjectId;
use crate::rev_parse::{self, TreeishBlobAtPath};
use crate::revwalk::RevWalk;

/// Object kind [`RevWalk::resolve`] peels the named object to.
///
/// The kind also breaks ties when abbreviated hex matches several objects: [`Peel::Commit`]
/// prefers commits, [`Peel::Tree`] tree-ish objects and [`Peel::Blob`] blobs. An explicit
/// `^{kind}` suffix in the spec always wins over the hint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Peel {
    /// The object the spec names, unpeeled. Ambiguous abbreviated hex follows
    /// `core.disambiguate`, or [`ResolvePurpose::RangeEnd`] when set.
    #[default]
    Any,
    /// A commit: tags are peeled, `A..B` resolves to `B` (an empty side means `HEAD`) and
    /// `A...B` to the merge base of `A` and `B`, like Git's commit-ish arguments to `log`
    /// or `reset`.
    Commit,
    /// A tree: a commit yields its root tree and tags are peeled, like Git's tree-ish
    /// arguments (`commit-tree`, `read-tree`).
    Tree,
    /// A blob: tags are peeled; any other kind is an error.
    Blob,
}

/// How a name that matches no ref or object is treated: as a possible path (porcelain) or as
/// a bad revision (plumbing).
///
/// With [`IndexDwim::On`], such a name fails with
/// [`RevParseError::AmbiguousArgument`](super::rev_parse_error::RevParseError::AmbiguousArgument)
/// (Git's "unknown revision or path not in the working tree"), so a caller can fall back to
/// treating it as a pathspec; in a bare repository a tracked path resolves to its index entry
/// like `:path`. Git's plumbing (`rev-parse --verify`) and commands that must tell revisions
/// from pathspecs (`reset <commit> -- <paths>`) use [`IndexDwim::Off`], where the name fails
/// with [`RevParseError::InvalidObjectName`](super::rev_parse_error::RevParseError::InvalidObjectName).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IndexDwim {
    /// Treat an unknown name as a possible path.
    #[default]
    On,
    /// Resolve refs and object names only.
    Off,
}

/// The kind of argument being resolved, which tunes disambiguation like Git's per-command
/// lookups.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ResolvePurpose {
    /// A plain revision argument.
    #[default]
    Revision,
    /// An endpoint of a range (`A..B`) or a `rev-parse --verify` argument: ambiguous
    /// abbreviated hex prefers commits. [`Peel::Commit`] implies this; [`Peel::Tree`] and
    /// [`Peel::Blob`] prefer their own kind instead.
    RangeEnd,
    /// A `checkout` / `switch` target with remote-branch guessing off (Git's `--no-guess`):
    /// a bare `name` does not resolve to the one `refs/remotes/<remote>/name` that matches.
    CheckoutNoGuess,
}

/// How [`RevWalk::resolve`] interprets a revision spec.
///
/// The default resolves any object, reports an unknown name as a possible path, and treats
/// the spec as a plain revision argument.
///
/// # Examples
///
/// ```
/// use grit_lib::revwalk::{IndexDwim, Peel, ResolveOptions, ResolvePurpose};
///
/// // `git rev-parse --verify <spec>`
/// let verify = ResolveOptions::default()
///     .with_index_dwim(IndexDwim::Off)
///     .with_purpose(ResolvePurpose::RangeEnd);
/// // A commit-ish argument such as `git log <rev>`
/// let commit = ResolveOptions::default().with_peel(Peel::Commit);
/// assert_ne!(verify, commit);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ResolveOptions {
    /// Object kind the result is peeled to.
    pub peel: Peel,
    /// Whether a tracked path may resolve to its index entry.
    pub index_dwim: IndexDwim,
    /// The kind of argument being resolved.
    pub purpose: ResolvePurpose,
}

impl ResolveOptions {
    /// Sets [`peel`](Self::peel).
    #[must_use]
    pub fn with_peel(mut self, peel: Peel) -> Self {
        self.peel = peel;
        self
    }

    /// Sets [`index_dwim`](Self::index_dwim).
    #[must_use]
    pub fn with_index_dwim(mut self, index_dwim: IndexDwim) -> Self {
        self.index_dwim = index_dwim;
        self
    }

    /// Sets [`purpose`](Self::purpose).
    #[must_use]
    pub fn with_purpose(mut self, purpose: ResolvePurpose) -> Self {
        self.purpose = purpose;
        self
    }
}

/// How many hex digits [`RevWalk::abbreviate`] starts from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AbbrevLength {
    /// `core.abbrev`, or Git's automatic length for the object count when it is unset or
    /// `auto`.
    #[default]
    Configured,
    /// At least this many digits (clamped to 4 through the full hex length).
    AtLeast(usize),
}

impl<'repo> RevWalk<'repo> {
    /// Resolve a revision `spec` (`HEAD`, `main~2`, `v1.0^{tree}`, `HEAD:path`, `@{-1}`,
    /// `:/message`, an abbreviated object id, ...) to an object id.
    ///
    /// # Parameters
    ///
    /// - `spec` - one revision expression.
    /// - `options` - what the result is peeled to, how an unknown name is reported, and how
    ///   ambiguous abbreviated hex is broken ([`ResolveOptions`]).
    ///
    /// # Errors
    ///
    /// - [`RevParseError::AmbiguousArgument`](super::rev_parse_error::RevParseError::AmbiguousArgument)
    ///   for an unknown name with [`IndexDwim::On`] and
    ///   [`RevParseError::InvalidObjectName`](super::rev_parse_error::RevParseError::InvalidObjectName)
    ///   with [`IndexDwim::Off`];
    ///   [`RevParseError::AmbiguousShortObject`](super::rev_parse_error::RevParseError::AmbiguousShortObject)
    ///   when abbreviated hex matches several objects the options can't choose between, and
    ///   other rev-parse errors.
    /// - [`RefError::Invalid`](crate::error::RefError::Invalid) when [`Peel::Commit`] meets a
    ///   tree or blob, and [`ObjectError::NotFound`](crate::error::ObjectError::NotFound) when
    ///   [`Peel::Tree`] or [`Peel::Blob`] cannot peel the object.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> grit_lib::error::Result<()> {
    /// use grit_lib::objects::ObjectKind;
    /// use grit_lib::refs::store::RefName;
    /// use grit_lib::repo::{InitOptions, Repository};
    /// use grit_lib::revwalk::{Peel, ResolveOptions};
    /// let dir = tempfile::tempdir()?;
    /// let options = InitOptions::default().with_initial_branch("main");
    /// let (repo, _) = Repository::init(dir.path(), &options)?;
    /// let tree = repo.odb().write(ObjectKind::Tree, b"")?;
    /// let body = format!("tree {tree}\nauthor A <a@example.com> 1700000000 +0000\ncommitter A <a@example.com> 1700000000 +0000\n\ndoc\n");
    /// let commit = repo.odb().write(ObjectKind::Commit, body.as_bytes())?;
    /// let main = RefName::try_from("refs/heads/main").expect("valid ref name");
    /// repo.references().update(&main, &commit)?;
    /// let revwalk = repo.revwalk();
    /// let commit = revwalk.resolve("refs/heads/main", &ResolveOptions::default())?;
    /// let tree = revwalk.resolve("HEAD", &ResolveOptions::default().with_peel(Peel::Tree))?;
    /// assert_ne!(commit, tree);
    /// # Ok(())
    /// # }
    /// ```
    pub fn resolve(&self, spec: &str, options: &ResolveOptions) -> Result<ObjectId> {
        rev_parse::resolve_revision_with_options(self.repository(), spec, options)
    }

    /// Peel `oid` through tags to a commit.
    ///
    /// # Errors
    ///
    /// Returns [`RefError::Invalid`](crate::error::RefError::Invalid) when the object peels
    /// to a tree or blob, and object read errors.
    pub fn peel_to_commit(&self, oid: ObjectId) -> Result<ObjectId> {
        rev_parse::peel_to_commit_for_merge_base(self.repository(), oid)
    }

    /// Peel `oid` to a tree: a commit yields its root tree, tags are peeled, a tree is
    /// returned unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`ObjectError::NotFound`](crate::error::ObjectError::NotFound) when the
    /// object peels to a blob, and object read errors.
    pub fn peel_to_tree(&self, oid: ObjectId) -> Result<ObjectId> {
        rev_parse::peel_to_tree(self.repository(), oid)
    }

    /// Resolve `rev:path` to the blob (or symlink) at `path` in the tree of `rev`.
    ///
    /// # Errors
    ///
    /// Fails when `spec` is not `treeish:path`, when the path is missing, or when it names a
    /// tree or gitlink.
    pub fn blob_at_path(&self, spec: &str) -> Result<TreeishBlobAtPath> {
        rev_parse::resolve_treeish_blob_at_path(self.repository(), spec)
    }

    /// Shortest unique abbreviation of `oid`, starting from `length` hex digits.
    ///
    /// An id missing from the object database is cut to the starting length.
    ///
    /// # Errors
    ///
    /// Propagates config read and object database lookup failures.
    pub fn abbreviate(&self, oid: ObjectId, length: AbbrevLength) -> Result<String> {
        let repo = self.repository();
        let min_len = match length {
            AbbrevLength::AtLeast(len) => len,
            AbbrevLength::Configured => {
                let config = repo.config().snapshot()?;
                crate::commit_pretty::resolve_core_abbrev_len(&config, repo.odb())?
            }
        };
        rev_parse::abbreviate_object_id(repo, oid, min_len)
    }

    /// Every object id (loose, packed and in alternates) whose hex starts with `prefix`.
    ///
    /// # Errors
    ///
    /// Propagates object database lookup failures.
    pub fn abbrev_matches(&self, prefix: &str) -> Result<Vec<ObjectId>> {
        rev_parse::list_all_abbrev_matches(self.repository(), prefix)
    }

    /// Full ref name `spec` names (`rev-parse --symbolic-full-name`): `HEAD` gives its
    /// branch, `main` gives `refs/heads/main`, `@{-1}` the previous branch, and
    /// `@{upstream}` / `@{push}` the remote-tracking ref.
    ///
    /// Returns [`None`] when `spec` does not name a ref.
    #[must_use]
    pub fn symbolic_full_name(&self, spec: &str) -> Option<String> {
        rev_parse::symbolic_full_name(self.repository(), spec)
    }

    /// Remote-tracking ref an `@{upstream}`, `@{u}` or `@{push}` spec names
    /// (`main@{u}` gives `refs/remotes/origin/main`).
    ///
    /// # Errors
    ///
    /// Returns [`RevParseError::NoUpstream`](super::rev_parse_error::RevParseError::NoUpstream),
    /// [`RevParseError::UpstreamNotTracked`](super::rev_parse_error::RevParseError::UpstreamNotTracked)
    /// and the `@{push}` variants of
    /// [`RevParseError`](super::rev_parse_error::RevParseError), or
    /// [`RefError::Invalid`](crate::error::RefError::Invalid) when `spec` has no upstream
    /// suffix.
    pub fn upstream_symbolic_name(&self, spec: &str) -> Result<String> {
        rev_parse::resolve_upstream_symbolic_name(self.repository(), spec)
    }
}
