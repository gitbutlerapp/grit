//! Typed failures from revision parsing ([`crate::rev_parse`]).

use thiserror::Error;

/// Failure modes when resolving revision specs, upstream/push refs, paths, and reflogs.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RevParseError {
    /// `branch.<name>.merge` / remote tracking is not configured.
    #[error("no upstream configured for branch '{branch}'")]
    NoUpstream {
        /// Short branch name shown to the user.
        branch: String,
    },

    /// Upstream is configured but the remote-tracking ref is missing.
    #[error("upstream branch '{merge}' is not stored as a remote-tracking branch")]
    UpstreamNotTracked {
        /// Value from `branch.*.merge` (may include `refs/heads/`).
        merge: String,
    },

    /// No remote is configured for `@{push}` resolution.
    #[error("branch has no configured push remote")]
    NoPushRemote,

    /// Remote has push refspecs but none map this branch.
    #[error("push refspecs for '{remote}' do not include '{branch}'")]
    PushRefspecMissing {
        /// Remote name from config.
        remote: String,
        /// Short branch name.
        branch: String,
    },

    /// `push.default = nothing` and no overriding push refspec applies.
    #[error("push.default is nothing; no push destination")]
    PushDefaultNothing,

    /// `push.default = upstream` but no upstream tracking ref exists.
    #[error("branch '{branch}' has no upstream for push.default upstream")]
    NoUpstreamForPush {
        /// Short branch name.
        branch: String,
    },

    /// `push.default = simple` and upstream differs from the branch’s push ref.
    #[error("push.default simple: upstream and push ref differ")]
    PushDefaultSimpleMismatch,

    /// Default push mode could not find a tracking ref for the branch.
    #[error("no push tracking ref for branch '{branch}'")]
    NoPushTrackingRef {
        /// Short branch name.
        branch: String,
    },

    /// `HEAD` is not on a local branch (detached or unborn).
    #[error("HEAD does not point to a branch")]
    HeadNotBranch,

    /// Named branch does not exist under `refs/heads/`.
    #[error("no such branch: '{branch}'")]
    NoSuchBranch {
        /// Branch name from the spec.
        branch: String,
    },

    /// Could not read `.git/AUTO_MERGE`.
    #[error("failed to read AUTO_MERGE: {detail}")]
    AutoMergeRead {
        /// Underlying I/O or parse detail.
        detail: String,
    },

    /// Spec fragment is not a valid object name after other resolution failed.
    #[error("invalid object name '{name}'")]
    InvalidObjectName {
        /// The unresolved token.
        name: String,
    },

    /// Path in `rev:path` lies outside the work tree.
    #[error("'{path}' is outside repository at '{work_tree}'")]
    OutsideRepository {
        /// Path from the revision spec.
        path: String,
        /// Canonical work-tree path for the message.
        work_tree: String,
    },

    /// Revision string matches neither an object, ref, nor index path.
    #[error("ambiguous revision or path '{spec}'")]
    AmbiguousArgument {
        /// Full user spec.
        spec: String,
    },

    /// Reflog selector used but the reflog has no entries.
    #[error("log for '{ref_display}' is empty")]
    ReflogEmpty {
        /// Human-readable ref label.
        ref_display: String,
    },

    /// Reflog index is out of range.
    #[error("log for '{ref_display}' only has {available} entries")]
    ReflogInsufficientEntries {
        /// Human-readable ref label.
        ref_display: String,
        /// Number of entries available.
        available: usize,
    },

    /// Tree path exists at `HEAD` but not at the requested revision.
    #[error("path '{path}' exists on disk, but not in '{revision}'")]
    PathOnDiskNotInRevision { path: String, revision: String },

    /// Tree path missing; a related path exists under the current prefix.
    #[error("path '{candidate}' exists, but not '{path}'")]
    PathPrefixExists {
        candidate: String,
        path: String,
        /// Optional alternate `rev:path` spelling (no Git-style hint prefix).
        alternate_spec: Option<String>,
    },

    /// Path is absent from the named tree.
    #[error("path '{path}' does not exist in '{revision}'")]
    PathNotInRevision { path: String, revision: String },

    /// Index path lookup failed with no work-tree match.
    #[error("path '{path}' does not exist (neither on disk nor in the index)")]
    PathMissingFromWorktreeAndIndex { path: String },

    /// Index path exists at another stage.
    #[error("path '{path}' is in the index, but not at stage {stage}")]
    PathWrongIndexStage {
        path: String,
        stage: u8,
        /// Optional `:stage:path` spelling to try.
        alternate_spec: Option<String>,
    },

    /// Index path mismatch under a path prefix.
    #[error("path '{candidate}' is in the index, but not '{path}'")]
    PathIndexPrefixMismatch {
        candidate: String,
        path: String,
        alternate_spec: Option<String>,
    },

    /// File exists in the work tree but is not staged.
    #[error("path '{path}' exists on disk, but not in the index")]
    PathOnDiskNotInIndex { path: String },

    /// Path is not present in the index at all.
    #[error("path '{path}' does not exist in the index")]
    PathNotInIndex { path: String },

    /// Internal describe helper failed to compile its regex.
    #[error("internal describe regex")]
    InternalDescribeRegex,
}

/// One candidate line when a short object id is ambiguous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmbiguousObjectHint {
    /// Full or abbreviated hex object id.
    pub oid_hex: String,
    /// `None` when the object bytes are corrupt (`[bad object]` in Git).
    pub kind: Option<&'static str>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    fn assert_rev_parse_variant(result: Result<(), Error>, f: impl FnOnce(&RevParseError) -> bool) {
        match result {
            Err(Error::RevParse(e)) if f(&e) => {}
            other => panic!("expected RevParse variant, got {other:?}"),
        }
    }

    #[test]
    fn variant_no_upstream() {
        let err = Error::from(RevParseError::NoUpstream {
            branch: "main".into(),
        });
        assert!(matches!(
            err,
            Error::RevParse(RevParseError::NoUpstream { .. })
        ));
    }

    #[test]
    fn variant_push_default_nothing() {
        let err = Error::from(RevParseError::PushDefaultNothing);
        assert!(matches!(
            err,
            Error::RevParse(RevParseError::PushDefaultNothing)
        ));
    }

    #[test]
    fn variant_ambiguous_argument() {
        let err = Error::from(RevParseError::AmbiguousArgument { spec: "foo".into() });
        assert!(matches!(
            err,
            Error::RevParse(RevParseError::AmbiguousArgument { .. })
        ));
    }

    #[test]
    fn display_is_neutral() {
        let msg = RevParseError::NoPushRemote.to_string();
        assert!(!msg.contains("fatal:"));
        assert!(!msg.contains("hint:"));
    }

    // Placeholder so `assert_rev_parse_variant` is used when integration tests call it.
    #[test]
    fn assert_helper_compiles() {
        let r: Result<(), Error> = Err(RevParseError::HeadNotBranch.into());
        assert_rev_parse_variant(r, |e| matches!(e, RevParseError::HeadNotBranch));
    }
}
