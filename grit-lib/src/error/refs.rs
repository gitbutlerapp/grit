//! [`RefError`]: reference, reflog, and `HEAD` state failures.

use thiserror::Error;

use super::RefLockError;

/// Reference failure: ref names and values, `packed-refs`, ref locks, ref stores (files and
/// reftable), reflog selectors, and the state of `HEAD`.
///
/// Surfaced on the umbrella as [`Error::Ref`](crate::error::Error::Ref). Branch and tag
/// porcelain failures have their own areas ([`Error::Branch`](crate::error::Error::Branch),
/// [`Error::Tag`](crate::error::Error::Tag)).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RefError {
    /// A reference name or value is invalid.
    #[error("invalid ref: {detail}")]
    Invalid {
        /// What is wrong with the ref name or value.
        detail: String,
    },

    /// A ref name failed validation ([`RefName::new`](crate::refs::store::RefName::new),
    /// [`RefNameRef::new`](crate::refs::store::RefNameRef::new) and the other
    /// [`RefName`](crate::refs::store::RefName) constructors), so `?` works on them in
    /// functions returning [`crate::error::Result`].
    #[error("invalid ref name: {0}")]
    InvalidName(#[from] crate::check_ref_format::RefNameError),

    /// A `packed-refs` line Git would reject when reading the ref database.
    #[error("unexpected line in {path}: {line}")]
    PackedRefsUnexpectedLine {
        /// Path to the `packed-refs` file being read.
        path: String,
        /// Offending line contents.
        line: String,
    },

    /// Reference lock failure while updating refs.
    #[error(transparent)]
    Lock(#[from] RefLockError),

    /// Reference store transaction or iteration failure.
    #[error(transparent)]
    Store(crate::refs::store::RefStoreError),

    /// Reftable file, stack, or transaction failure.
    #[error(transparent)]
    Reftable(#[from] crate::reftable::ReftableError),

    /// [`create_commit`](crate::porcelain::commit::create_commit) requires a branch checkout.
    #[error("HEAD is detached")]
    DetachedHead,

    /// `HEAD` is on a branch with no commits yet, but the operation needs a commit
    /// ([`Repository::head_commit`](crate::repo::Repository::head_commit),
    /// [`replay_commit`](crate::porcelain::replay::replay_commit)).
    #[error("no commits yet on this branch")]
    UnbornHead,

    /// `HEAD` could not be resolved to a branch, detached commit, or unborn branch.
    #[error("HEAD is in an unknown state")]
    InvalidHead,

    /// `HEAD` is not a symbolic ref to the branch an update expected it to be on.
    #[error("HEAD does not point at branch '{branch}'")]
    HeadNotOnBranch {
        /// Full name of the branch `HEAD` was expected to point at.
        branch: String,
    },

    /// A detached-`HEAD` update found `HEAD` attached to a branch.
    #[error("HEAD points at a branch, not a detached commit")]
    HeadNotDetached,

    /// A ref update tried to write through a lock it had already committed or dropped.
    #[error("ref lock for '{refname}' was already released")]
    LockReleased {
        /// The ref whose lock was released.
        refname: String,
    },

    /// A ref query sorted by creator date, or filtered by reachability, without an object
    /// database to read commits from.
    #[error("ref query requires an object database")]
    QueryNeedsObjectDatabase,

    /// A ref query's `--contains` / `--merged` / `--no-merged` target does not peel to a commit.
    #[error("object {oid} is not a commit")]
    QueryTargetNotCommit {
        /// The target object.
        oid: crate::objects::ObjectId,
    },

    /// Ref does not exist and has no reflog entries.
    #[error("unknown ref '{refname}'")]
    ReflogUnknownRef {
        /// User-supplied or DWIM ref name.
        refname: String,
    },

    /// A reflog selector is missing its reference or `@{n}` delimiters.
    #[error("expected ref@{{n}} form, got '{spec}'")]
    ReflogInvalidSelector {
        /// Selector after trimming surrounding whitespace.
        spec: String,
    },

    /// A reflog selector index is not a nonnegative integer that fits in `usize`.
    #[error("invalid reflog index in '{spec}'")]
    ReflogInvalidIndex {
        /// Selector after trimming surrounding whitespace.
        spec: String,
        /// Integer parsing failure, including overflow.
        #[source]
        source: std::num::ParseIntError,
    },

    /// A reflog expiry (`gc.reflogExpire`, `--expire`) is neither `never`/`now`/`all` nor a
    /// date.
    #[error("invalid reflog expiry: {value:?}")]
    InvalidReflogExpiry {
        /// The expiry as given.
        value: String,
    },

    /// `ref@{{n}}` is out of range for the current reflog.
    #[error("reflog entry {refname}@{{{index}}} not found")]
    ReflogIndexOutOfRange {
        /// Ref whose reflog was read.
        refname: String,
        /// Requested newest-first index.
        index: usize,
    },
}

impl RefError {
    /// Stable snake_case slug for this failure (the former umbrella variant names).
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Invalid { .. } => "invalid_ref",
            Self::InvalidName(..) => "invalid_ref_name",
            Self::PackedRefsUnexpectedLine { .. } => "packed_refs_unexpected_line",
            Self::Lock(..) => "ref_lock",
            Self::Store(..) => "ref_store",
            Self::Reftable(..) => "reftable",
            Self::DetachedHead => "detached_head",
            Self::UnbornHead => "unborn_head",
            Self::InvalidHead => "invalid_head",
            Self::HeadNotOnBranch { .. } => "head_not_on_branch",
            Self::HeadNotDetached => "head_not_detached",
            Self::LockReleased { .. } => "ref_lock_released",
            Self::QueryNeedsObjectDatabase => "ref_query_needs_object_database",
            Self::QueryTargetNotCommit { .. } => "ref_query_target_not_commit",
            Self::ReflogUnknownRef { .. } => "reflog_unknown_ref",
            Self::ReflogInvalidSelector { .. } => "reflog_invalid_selector",
            Self::ReflogInvalidIndex { .. } => "reflog_invalid_index",
            Self::ReflogIndexOutOfRange { .. } => "reflog_index_out_of_range",
            Self::InvalidReflogExpiry { .. } => "invalid_reflog_expiry",
        }
    }
}
