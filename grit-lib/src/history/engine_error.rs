//! Engine-level library failures for [`super::HistoryError`](crate::history::HistoryError).
//!
//! Mirrors [`crate::error::Error`] without the recursive [`crate::error::Error::History`].

use crate::error::{
    maintenance_running_message, remote_unreadable_message, restore_unmerged_message, ApplyError,
    AttrPathspecError, AuthFailure, ConfigError, FilterError, LockHeldError, MidxError,
    NotARepositoryReason, OverwriteReason, PathIoOperation, PathOutsideRepoError,
    PathspecFromFileParseError, RefLockError, RemoteUnreadableReason, SubmoduleError,
    WorktreeOperation,
};
use crate::git_dir::GitDirPathOperation;
use crate::repo::error::FilesOnlyOperation;
use crate::worktree::io_error::WorktreePathOperation;

use super::PlanningError;

fn promisor_fetch_failed_engine_message(
    missing: &crate::objects::ObjectId,
    failures: &[(String, Box<HistoryEngineError>)],
) -> String {
    let causes = failures
        .iter()
        .map(|(remote, err)| format!("{remote}: {err}"))
        .collect::<Vec<_>>()
        .join("; ");
    format!("could not fetch promised object {missing} ({causes})")
}

fn path_io_to_engine(err: crate::error::PathIoError) -> HistoryEngineError {
    let path = err
        .path
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    match err.operation {
        PathIoOperation::GitDir(operation) => HistoryEngineError::GitDirPathIo {
            path,
            operation,
            source: err.source,
        },
        PathIoOperation::Worktree(operation) => HistoryEngineError::WorktreePathIo {
            path,
            operation,
            source: err.source,
        },
    }
}

fn repository_error_to_engine(err: crate::repo::RepositoryError) -> HistoryEngineError {
    use crate::repo::RepositoryError;
    match err {
        RepositoryError::NotARepository(reason) => HistoryEngineError::NotARepository(reason),
        RepositoryError::ForbiddenBareRepository { path } => {
            HistoryEngineError::ForbiddenBareRepository(path)
        }
        RepositoryError::DubiousOwnership { path, .. } => {
            HistoryEngineError::DubiousOwnership(path)
        }
        RepositoryError::UnsupportedRepositoryFormatVersion { version } => {
            HistoryEngineError::UnsupportedRepositoryFormatVersion(version)
        }
        RepositoryError::InvalidRefStorageFormat { value } => {
            HistoryEngineError::InvalidRefStorageFormat { value }
        }
        RepositoryError::InitObjectFormatChange => HistoryEngineError::InitObjectFormatChange,
        RepositoryError::InitBareWithSeparateGitDir => {
            HistoryEngineError::InitBareWithSeparateGitDir
        }
        RepositoryError::RequiresWorkTree => HistoryEngineError::RequiresWorkTree,
        RepositoryError::GitDirPathIo(err) => path_io_to_engine(err),
        RepositoryError::UnsupportedObjectStore { operation } => {
            HistoryEngineError::UnsupportedObjectStore { operation }
        }
        RepositoryError::PreciousObjectsRepository { operation } => {
            HistoryEngineError::PreciousObjectsRepository { operation }
        }
        RepositoryError::ObjectFormatMismatch { repository, store } => {
            HistoryEngineError::ObjectFormatMismatch { repository, store }
        }
        other => HistoryEngineError::UmbrellaMessage(other.to_string()),
    }
}

fn index_error_to_engine(err: crate::index::IndexError) -> HistoryEngineError {
    use crate::index::IndexError;
    match err {
        IndexError::Corrupt { detail, .. } => HistoryEngineError::IndexError(detail),
        IndexError::Unmerged { .. } => HistoryEngineError::IndexUnmerged,
        IndexError::PathPrefixConflict { directory, file } => {
            HistoryEngineError::IndexPathPrefixConflict { directory, file }
        }
        IndexError::FsmonitorExtensionTruncated => {
            HistoryEngineError::IndexFsmonitorExtensionTruncated
        }
        IndexError::FsmonitorExtensionBadVersion(version) => {
            HistoryEngineError::IndexFsmonitorExtensionBadVersion(version)
        }
        IndexError::FsmonitorExtensionEwahInvalid => {
            HistoryEngineError::IndexFsmonitorExtensionEwahInvalid
        }
        IndexError::CacheTreeCorrupt => HistoryEngineError::CacheTreeCorrupt,
        IndexError::LockHeld(err) => HistoryEngineError::LockHeld(err),
        IndexError::PathIo(err) => path_io_to_engine(err),
        other => HistoryEngineError::UmbrellaMessage(other.to_string()),
    }
}

fn remote_error_to_engine(err: crate::remote::RemoteError) -> HistoryEngineError {
    use crate::remote::RemoteError;
    match err {
        RemoteError::Auth { url, reason } => HistoryEngineError::Auth { url, reason },
        RemoteError::CredentialsUnavailable { helpers } => {
            HistoryEngineError::CredentialsUnavailable { helpers }
        }
        RemoteError::PushAtomicUnsupported => HistoryEngineError::PushAtomicUnsupported,
        RemoteError::PushOptionsUnsupported => HistoryEngineError::PushOptionsUnsupported,
        RemoteError::FetchUnshallowNotShallow => HistoryEngineError::FetchUnshallowNotShallow,
        RemoteError::FetchShallowUnsupported => HistoryEngineError::FetchShallowUnsupported,
        RemoteError::FetchDeepenRelativeUnsupported => {
            HistoryEngineError::FetchDeepenRelativeUnsupported
        }
        RemoteError::ReceivedPackTempMissing { path } => {
            HistoryEngineError::ReceivedPackTempMissing { path }
        }
        RemoteError::Unreadable { url, reason } => {
            HistoryEngineError::RemoteUnreadable { url, reason }
        }
        RemoteError::Fatal { message } => HistoryEngineError::RemoteFatal { message },
        RemoteError::LocalUploadPack(err) => HistoryEngineError::LocalUploadPack(err),
        RemoteError::LocalUploadPackPanicked => HistoryEngineError::LocalUploadPackPanicked,
        RemoteError::Resumable(err) => HistoryEngineError::Resumable(err),
        RemoteError::FetchRefspec(err) => HistoryEngineError::FetchRefspec(err),
        RemoteError::FetchIntoCheckedOutBranch { branch } => {
            HistoryEngineError::FetchIntoCheckedOutBranch { branch }
        }
        RemoteError::Cancelled => HistoryEngineError::Cancelled,
        RemoteError::Nested(err) => error_to_engine(*err),
        other => HistoryEngineError::UmbrellaMessage(other.to_string()),
    }
}

fn object_read_failure_to_engine(err: crate::error::ObjectReadFailure) -> HistoryEngineError {
    use crate::error::ObjectReadFailure;
    match err {
        ObjectReadFailure::Io(err) => HistoryEngineError::Io(err),
        ObjectReadFailure::Zlib(err) => HistoryEngineError::Zlib(err),
        ObjectReadFailure::CorruptObject(err) => HistoryEngineError::CorruptObject(err),
        ObjectReadFailure::ObjectStreamTruncated { expected, actual } => {
            HistoryEngineError::ObjectStreamTruncated { expected, actual }
        }
        ObjectReadFailure::UnknownObjectType(err) => HistoryEngineError::UnknownObjectType(err),
        ObjectReadFailure::ObjectHeaderTooLong { oid } => {
            HistoryEngineError::ObjectHeaderTooLong { oid }
        }
        ObjectReadFailure::DeltaChainTooDeep { limit } => {
            HistoryEngineError::DeltaChainTooDeep { limit }
        }
        ObjectReadFailure::InvalidObject { kind, check } => {
            HistoryEngineError::InvalidObject { kind, check }
        }
    }
}

/// Convert a top-level library error into the history engine mirror.
#[allow(deprecated)]
pub(crate) fn error_to_engine(err: crate::error::Error) -> HistoryEngineError {
    match err {
        crate::error::Error::History(h) => *super::error::history_error_to_engine_box(h),
        crate::error::Error::Repository(err) => repository_error_to_engine(err),
        crate::error::Error::InvalidObjectId(v) => HistoryEngineError::InvalidObjectId(v),
        crate::error::Error::HashCollision { oid } => HistoryEngineError::HashCollision { oid },
        crate::error::Error::ObjectNotFound(v) => HistoryEngineError::ObjectNotFound(v),
        crate::error::Error::RevisionNotFound(v) => HistoryEngineError::RevisionNotFound(v),
        crate::error::Error::PromisedObjectMissing(v) => {
            HistoryEngineError::PromisedObjectMissing(v)
        }
        crate::error::Error::CorruptObject(v) => HistoryEngineError::CorruptObject(v),
        crate::error::Error::PackIndexChecksumMismatch { path, mismatch } => {
            HistoryEngineError::PackIndexChecksumMismatch { path, mismatch }
        }
        crate::error::Error::UnresolvedDeltas { count } => {
            HistoryEngineError::UnresolvedDeltas { count }
        }
        crate::error::Error::PackDeltaBaseNotWritten { target, base } => {
            HistoryEngineError::PackDeltaBaseNotWritten { target, base }
        }
        crate::error::Error::InvalidObject { kind, check } => {
            HistoryEngineError::InvalidObject { kind, check }
        }
        crate::error::Error::InvalidTreeEntry(v) => HistoryEngineError::InvalidTreeEntry(v),
        crate::error::Error::InvalidSignature(v) => HistoryEngineError::InvalidSignature(v),
        crate::error::Error::UnexpectedObjectKind {
            oid,
            expected,
            actual,
        } => HistoryEngineError::UnexpectedObjectKind {
            oid,
            expected,
            actual,
        },
        crate::error::Error::CommitGraphParentMissing { commit, parent } => {
            HistoryEngineError::CommitGraphParentMissing { commit, parent }
        }
        crate::error::Error::DeltaChainTooDeep { limit } => {
            HistoryEngineError::DeltaChainTooDeep { limit }
        }
        crate::error::Error::UnknownObjectType(v) => HistoryEngineError::UnknownObjectType(v),
        crate::error::Error::ObjectHeaderTooLong { oid } => {
            HistoryEngineError::ObjectHeaderTooLong { oid }
        }
        crate::error::Error::Io(v) => HistoryEngineError::Io(v),
        crate::error::Error::LockHeld(v) => HistoryEngineError::LockHeld(v),
        crate::error::Error::Zlib(v) => HistoryEngineError::Zlib(v),
        crate::error::Error::LooseHashMismatch { path, real_oid } => {
            HistoryEngineError::LooseHashMismatch { path, real_oid }
        }
        crate::error::Error::LooseObjectWrite { path, source } => {
            HistoryEngineError::LooseObjectWrite { path, source }
        }
        crate::error::Error::WorktreePathIo {
            path,
            operation,
            source,
        } => HistoryEngineError::WorktreePathIo {
            path,
            operation,
            source,
        },
        crate::error::Error::ObjectStreamTruncated { expected, actual } => {
            HistoryEngineError::ObjectStreamTruncated { expected, actual }
        }
        crate::error::Error::WorktreeFileChanged { path } => {
            HistoryEngineError::WorktreeFileChanged { path }
        }
        crate::error::Error::IndexError(v) => HistoryEngineError::IndexError(v),
        crate::error::Error::IndexUnmerged => HistoryEngineError::IndexUnmerged,
        crate::error::Error::MaintenanceRunning { holder } => {
            HistoryEngineError::MaintenanceRunning { holder }
        }
        crate::error::Error::UnmergedConflictMarkers { paths } => {
            HistoryEngineError::UnmergedConflictMarkers { paths }
        }
        crate::error::Error::IndexPathPrefixConflict { directory, file } => {
            HistoryEngineError::IndexPathPrefixConflict { directory, file }
        }
        crate::error::Error::IndexFsmonitorExtensionTruncated => {
            HistoryEngineError::IndexFsmonitorExtensionTruncated
        }
        crate::error::Error::IndexFsmonitorExtensionBadVersion(v) => {
            HistoryEngineError::IndexFsmonitorExtensionBadVersion(v)
        }
        crate::error::Error::IndexFsmonitorExtensionEwahInvalid => {
            HistoryEngineError::IndexFsmonitorExtensionEwahInvalid
        }
        crate::error::Error::CacheTreeCorrupt => HistoryEngineError::CacheTreeCorrupt,
        crate::error::Error::Index(err) => index_error_to_engine(err),
        crate::error::Error::InvalidRef(v) => HistoryEngineError::InvalidRef(v),
        crate::error::Error::PackedRefsUnexpectedLine { path, line } => {
            HistoryEngineError::PackedRefsUnexpectedLine { path, line }
        }
        crate::error::Error::PathError(v) => HistoryEngineError::PathError(v),
        crate::error::Error::PathspecFromFile(v) => HistoryEngineError::PathspecFromFile(v),
        crate::error::Error::InvalidPath(v) => HistoryEngineError::InvalidPath(v),
        crate::error::Error::InvalidPathComponent { path, reason } => {
            HistoryEngineError::InvalidPathComponent { path, reason }
        }
        crate::error::Error::Config(v) => HistoryEngineError::Config(v),
        crate::error::Error::Submodule(v) => HistoryEngineError::Submodule(v),
        crate::error::Error::Branch(v) => HistoryEngineError::Branch(v),
        crate::error::Error::Commit(v) => HistoryEngineError::Commit(v),
        crate::error::Error::Merge(v) => HistoryEngineError::Merge(v),
        crate::error::Error::RefLock(v) => HistoryEngineError::RefLock(v),
        crate::error::Error::RefStore(v) => HistoryEngineError::RefStore(v),
        crate::error::Error::Reftable(v) => HistoryEngineError::Reftable(v),
        crate::error::Error::Filter(v) => HistoryEngineError::Filter(v),
        crate::error::Error::Apply(v) => HistoryEngineError::Apply(v),
        crate::error::Error::PathOutsideRepo(v) => HistoryEngineError::PathOutsideRepo(v),
        crate::error::Error::Remote(err) => remote_error_to_engine(err),
        crate::error::Error::InvalidAttrPathspec { spec, reason } => {
            HistoryEngineError::InvalidAttrPathspec { spec, reason }
        }
        crate::error::Error::PathspecNoMatch { spec } => {
            HistoryEngineError::PathspecNoMatch { spec }
        }
        crate::error::Error::RestoreConflictWithStaged => {
            HistoryEngineError::RestoreConflictWithStaged
        }
        crate::error::Error::RestoreConflictWithSource => {
            HistoryEngineError::RestoreConflictWithSource
        }
        crate::error::Error::RestoreConflictMissingTheirs { path } => {
            HistoryEngineError::RestoreConflictMissingTheirs { path }
        }
        crate::error::Error::RestoreConflictMissingOurs { path } => {
            HistoryEngineError::RestoreConflictMissingOurs { path }
        }
        crate::error::Error::RestoreConflictNotUnmerged { path } => {
            HistoryEngineError::RestoreConflictNotUnmerged { path }
        }
        crate::error::Error::RestorePathUnmerged { paths } => {
            HistoryEngineError::RestorePathUnmerged { paths }
        }
        crate::error::Error::PathspecBeyondSymlink { spec } => {
            HistoryEngineError::PathspecBeyondSymlink { spec }
        }
        crate::error::Error::EmbeddedRepoWithoutCommit { path } => {
            HistoryEngineError::EmbeddedRepoWithoutCommit { path }
        }
        crate::error::Error::PathspecIgnored { paths } => {
            HistoryEngineError::PathspecIgnored { paths }
        }
        crate::error::Error::RemoveBlocked { paths } => HistoryEngineError::RemoveBlocked { paths },
        crate::error::Error::NotRemovingRecursively(v) => {
            HistoryEngineError::NotRemovingRecursively(v)
        }
        crate::error::Error::RemovePathsRequired => HistoryEngineError::RemovePathsRequired,
        crate::error::Error::MoveSourceNotTracked { src } => {
            HistoryEngineError::MoveSourceNotTracked { src }
        }
        crate::error::Error::MoveDestinationExists { src, destination } => {
            HistoryEngineError::MoveDestinationExists { src, destination }
        }
        crate::error::Error::MoveDestinationNotDirectory { destination } => {
            HistoryEngineError::MoveDestinationNotDirectory { destination }
        }
        crate::error::Error::MoveIntoSubdirectory { src, destination } => {
            HistoryEngineError::MoveIntoSubdirectory { src, destination }
        }
        crate::error::Error::MoveIntoSelf { src, destination } => {
            HistoryEngineError::MoveIntoSelf { src, destination }
        }
        crate::error::Error::MoveNothingToMove { src } => {
            HistoryEngineError::MoveNothingToMove { src }
        }
        crate::error::Error::MoveDuplicateDestination { destination } => {
            HistoryEngineError::MoveDuplicateDestination { destination }
        }
        crate::error::Error::MoveBadSource { src, destination } => {
            HistoryEngineError::MoveBadSource { src, destination }
        }
        crate::error::Error::MoveMissingIndexEntry { path } => {
            HistoryEngineError::MoveMissingIndexEntry { path }
        }
        crate::error::Error::WouldOverwrite {
            reason,
            paths,
            message,
        } => HistoryEngineError::WouldOverwrite {
            reason,
            paths,
            message,
        },
        crate::error::Error::DirtyWorktree { operation, paths } => {
            HistoryEngineError::DirtyWorktree { operation, paths }
        }
        crate::error::Error::Cancelled => HistoryEngineError::Cancelled,
        crate::error::Error::Verbatim(v) => HistoryEngineError::UmbrellaMessage(v),
        crate::error::Error::DetachedHead => HistoryEngineError::DetachedHead,
        crate::error::Error::UnbornHead => HistoryEngineError::UnbornHead,
        crate::error::Error::InvalidHead => HistoryEngineError::InvalidHead,
        crate::error::Error::BranchCheckedOutInWorktree {
            branch,
            worktree_path,
        } => HistoryEngineError::BranchCheckedOutInWorktree {
            branch,
            worktree_path,
        },
        crate::error::Error::BranchNotFound { branch } => {
            HistoryEngineError::BranchNotFound { branch }
        }
        crate::error::Error::BranchListNamesDateSort => HistoryEngineError::BranchListNamesDateSort,
        crate::error::Error::Tag(v) => HistoryEngineError::Tag(v),
        crate::error::Error::Worktree(v) => HistoryEngineError::Worktree(v),
        crate::error::Error::ReflogUnknownRef { refname } => {
            HistoryEngineError::ReflogUnknownRef { refname }
        }
        crate::error::Error::ReflogInvalidSelector { spec } => {
            HistoryEngineError::ReflogInvalidSelector { spec }
        }
        crate::error::Error::ReflogInvalidIndex { spec, source } => {
            HistoryEngineError::ReflogInvalidIndex { spec, source }
        }
        crate::error::Error::ReflogIndexOutOfRange { refname, index } => {
            HistoryEngineError::ReflogIndexOutOfRange { refname, index }
        }
        crate::error::Error::BlameWorktreeBareRepository => {
            HistoryEngineError::BlameWorktreeBareRepository
        }
        crate::error::Error::HunkSelection(v) => HistoryEngineError::HunkSelection(v),
        crate::error::Error::NoteAlreadyExists { object } => {
            HistoryEngineError::NoteAlreadyExists { object }
        }
        crate::error::Error::InvalidNotesRef(v) => HistoryEngineError::InvalidNotesRef(v),
        crate::error::Error::NotesRewriteMode(v) => HistoryEngineError::NotesRewriteMode(v),
        crate::error::Error::NotesRefNotCommit { notes_ref, found } => {
            HistoryEngineError::NotesRefNotCommit { notes_ref, found }
        }
        crate::error::Error::NotesObjectKind {
            oid,
            expected,
            found,
        } => HistoryEngineError::NotesObjectKind {
            oid,
            expected,
            found,
        },
        crate::error::Error::NotesMergeBothEmpty { local, remote } => {
            HistoryEngineError::NotesMergeBothEmpty { local, remote }
        }
        crate::error::Error::NotesMergeInProgress { path } => {
            HistoryEngineError::NotesMergeInProgress { path }
        }
        crate::error::Error::Midx(v) => HistoryEngineError::Midx(v),
        crate::error::Error::Offload(v) => HistoryEngineError::Offload(v),
        crate::error::Error::RevParse(v) => HistoryEngineError::RevParse(v),
        crate::error::Error::RevList(v) => HistoryEngineError::RevList(v),
        crate::error::Error::Hook(v) => HistoryEngineError::Hook(v),
        crate::error::Error::NoPromisorRemote { operation } => {
            HistoryEngineError::NoPromisorRemote { operation }
        }
        crate::error::Error::TreeSha256InvalidHex => HistoryEngineError::TreeSha256InvalidHex,
        crate::error::Error::TreeSha256InvalidRevision { found } => {
            HistoryEngineError::TreeSha256InvalidRevision { found }
        }
        crate::error::Error::TreeSha256MissingSubmodules(v) => {
            HistoryEngineError::TreeSha256MissingSubmodules(v)
        }
        crate::error::Error::TreeSha256RequiresSigning => {
            HistoryEngineError::TreeSha256RequiresSigning
        }
        crate::error::Error::TagVerifyNotTag { oid, found } => {
            HistoryEngineError::TagVerifyNotTag { oid, found }
        }
        crate::error::Error::TagVerifyMissingObjectHeader => {
            HistoryEngineError::TagVerifyMissingObjectHeader
        }
        crate::error::Error::TagVerifyDuplicateTreeSha256Header => {
            HistoryEngineError::TagVerifyDuplicateTreeSha256Header
        }
        crate::error::Error::TreeSha256Claim(v) => HistoryEngineError::TreeSha256Claim(v),
        crate::error::Error::BlameIgnoreRevUnknown { rev } => {
            HistoryEngineError::BlameIgnoreRevUnknown { rev }
        }
        crate::error::Error::BlameIgnoreRevsFileOpen { path, source } => {
            HistoryEngineError::BlameIgnoreRevsFileOpen { path, source }
        }
        crate::error::Error::BlameIgnoreRevsFileInvalidRev { path, rev } => {
            HistoryEngineError::BlameIgnoreRevsFileInvalidRev { path, rev }
        }
        crate::error::Error::PromisorFetchFailed { missing, failures } => {
            HistoryEngineError::PromisorFetchFailed {
                missing,
                failures: failures
                    .into_iter()
                    .map(|(name, e)| (name, Box::new(error_to_engine(*e))))
                    .collect(),
            }
        }
        crate::error::Error::CheckoutRollbackIncomplete { source, unrestored } => {
            HistoryEngineError::CheckoutRollbackIncomplete {
                source: Box::new(error_to_engine(*source)),
                unrestored,
            }
        }
        crate::error::Error::CorruptLooseObject { oid, path, source } => {
            HistoryEngineError::CorruptLooseObject {
                oid,
                path,
                source: Box::new(object_read_failure_to_engine(*source)),
            }
        }
        crate::error::Error::CorruptPackedObject {
            oid,
            pack,
            offset,
            source,
        } => HistoryEngineError::CorruptPackedObject {
            oid,
            pack,
            offset,
            source: Box::new(object_read_failure_to_engine(*source)),
        },
        other => HistoryEngineError::UmbrellaMessage(other.to_string()),
    }
}

/// Library failures surfaced through [`HistoryError::Engine`](crate::history::HistoryError::Engine).
///
/// This is the same shape as [`crate::error::Error`] except history is unwrapped to avoid nesting.
#[derive(Debug, thiserror::Error)]
pub enum HistoryEngineError {
    /// A repository could not be found or is structurally invalid.
    #[error("not a git repository (or any of the parent directories): {0}")]
    NotARepository(NotARepositoryReason),

    /// A bare repository was found but access is forbidden by safe.bareRepository.
    #[error("cannot use bare repository '{0}' (safe.bareRepository is 'explicit')")]
    ForbiddenBareRepository(String),

    /// The repository is owned by a different user (safe.directory).
    #[error("detected dubious ownership in repository at '{0}'")]
    DubiousOwnership(String),

    /// Repository format version is not supported by this implementation.
    #[error("unsupported repository format version '{0}'")]
    UnsupportedRepositoryFormatVersion(u32),

    /// `extensions.refStorage` names an unsupported backend.
    #[error("invalid value for 'extensions.refstorage': '{value}'")]
    InvalidRefStorageFormat {
        /// Raw `extensions.refStorage` value from the repository config.
        value: String,
    },

    /// [`crate::repo::Repository::init`] refused to change object format on reinit.
    #[error("cannot change object format in an existing repository")]
    InitObjectFormatChange,

    /// [`crate::repo::Repository::init`] with incompatible `--bare` and separate git dir.
    #[error("cannot combine bare repository with separate git directory")]
    InitBareWithSeparateGitDir,

    /// A supplied object ID string was not valid hex or the wrong length.
    #[error("invalid object id '{0}'")]
    InvalidObjectId(String),

    /// SHA-1 object hashing detected a chosen-prefix collision (sha1dc).
    #[error("SHA-1 hash collision detected for object {oid}")]
    HashCollision {
        /// Plain SHA-1 digest for the colliding input (not safe to use as an object id).
        oid: crate::objects::ObjectId,
    },

    /// The requested object does not exist in the object store.
    #[error("object not found: {0}")]
    ObjectNotFound(String),

    /// A revision expression (branch, tag, object id, `x~n`, `ORIG_HEAD`, …) names nothing in
    /// the repository. Holds the expression as the caller wrote it.
    #[error("unknown revision '{0}'")]
    RevisionNotFound(String),

    /// A partial-clone promisor object is not present locally and no fetcher ran.
    #[error("promised object not available locally: {0}")]
    PromisedObjectMissing(crate::objects::ObjectId),

    /// Promised objects could not be fetched: every promisor remote was asked, and some
    /// failed. [`Self::PromisedObjectMissing`] is used instead when every remote answered
    /// but none had the object.
    #[error("{}", promisor_fetch_failed_engine_message(missing, failures))]
    PromisorFetchFailed {
        /// The first object still missing after every remote was tried.
        missing: crate::objects::ObjectId,
        /// One entry per remote that failed, in the order they were tried: the remote's
        /// name and its first error.
        failures: Vec<(String, Box<HistoryEngineError>)>,
    },

    /// An object's stored data is corrupt or malformed.
    #[error("corrupt object: {0}")]
    CorruptObject(String),

    /// A pack index (`.idx`) file's trailing checksum does not match its contents.
    ///
    /// Only reported by reads that verify the index trailer, such as
    /// [`crate::pack::read_pack_index`]; the checksum of the `.pack` the index
    /// describes is checked separately.
    #[error("corrupt object: index checksum mismatch for {}: {mismatch}", crate::git_path::user_display_path(.path))]
    PackIndexChecksumMismatch {
        /// The `.idx` file that failed verification.
        path: std::path::PathBuf,
        /// Expected and stored trailer digests.
        mismatch: crate::hash::TrailerMismatch,
    },

    /// Indexing or unpacking a pack left deltas whose bases are neither in the pack nor
    /// in the object database: a thin pack without its bases, or a ref-delta cycle.
    #[error("corrupt object: {count} delta(s) could not be resolved")]
    UnresolvedDeltas {
        /// Number of deltas left unresolved.
        count: u64,
    },

    /// A pack being written that is not thin reached a delta before its base: the base
    /// is missing from the pack, or the planned deltas form a cycle. The receiver could
    /// not resolve that delta, so the pack was abandoned.
    #[error("pack delta for {target} comes before its base {base}")]
    PackDeltaBaseNotWritten {
        /// The object that would have been stored as a delta.
        target: crate::objects::ObjectId,
        /// Its delta base, not written ahead of it.
        base: crate::objects::ObjectId,
    },

    /// [`crate::objects::Objects::write`] refused a payload that `git fsck` would reject;
    /// nothing was written.
    #[error("refusing to write invalid {kind} object: {}", .check.report_line())]
    InvalidObject {
        /// Kind the payload was meant to be stored as.
        kind: crate::objects::ObjectKind,
        /// The first fsck rule the payload breaks (`check.id` is Git's fsck message id).
        check: crate::fsck_standalone::FsckError,
    },

    /// [`crate::objects::Objects::write_tree`] refused an entry.
    #[error(transparent)]
    InvalidTreeEntry(#[from] crate::objects::TreeEntryError),

    /// A [`crate::ident::Signature`] cannot be written as an author, committer or tagger line.
    #[error(transparent)]
    InvalidSignature(#[from] crate::ident::SignatureError),

    /// An object exists but has a different kind than the operation requires, such as a
    /// tree or blob where a commit was expected.
    #[error("object {oid} is a {actual}, not a {expected}")]
    UnexpectedObjectKind {
        /// The object that was read.
        oid: crate::objects::ObjectId,
        /// The kind the operation required.
        expected: crate::objects::ObjectKind,
        /// The kind the object actually has.
        actual: crate::objects::ObjectKind,
    },

    /// A commit-graph write was given a commit whose parent is neither among the commits
    /// being written nor in the base graph layers, so its parent edge cannot be encoded.
    #[error("commit-graph cannot encode parent {parent} of commit {commit}: parent not in graph")]
    CommitGraphParentMissing {
        /// Commit whose parent list was being encoded.
        commit: crate::objects::ObjectId,
        /// Parent that is missing from the graph.
        parent: crate::objects::ObjectId,
    },

    /// In-pack delta resolution exceeded the read-side chain depth limit (guards
    /// against cyclic ref-delta chains and unbounded recursion; Git has no read limit).
    #[error("delta chain too deep (limit {limit})")]
    DeltaChainTooDeep {
        /// Maximum delta chain depth allowed while reading packed objects.
        limit: usize,
    },

    /// An unsupported or unknown object type was encountered.
    #[error("unknown object type '{0}'")]
    UnknownObjectType(String),

    /// Loose object header type field exceeds Git's 32-byte limit.
    #[error("header for {oid} too long, exceeds 32 bytes")]
    ObjectHeaderTooLong {
        /// Object id (hex) of the loose object with an oversized header.
        oid: String,
    },

    /// An I/O error from the underlying filesystem.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A lock file (for example `.git/index.lock` or `refs/heads/main.lock`) already exists, so
    /// another process is updating the same file or a crashed one left the lock behind.
    ///
    /// Operations that return this before changing anything leave the repository untouched.
    /// The lock file belongs to whoever created it and is never removed. Retry once the other
    /// process finishes, or delete the lock path when no process owns it (a `holder` with
    /// `running: false` means the lock is stale).
    #[error(transparent)]
    LockHeld(#[from] LockHeldError),

    /// A zlib compression or decompression failure.
    #[error("zlib error: {0}")]
    Zlib(String),

    /// A loose object file is present but its stored bytes could not be decoded.
    #[error("loose object {oid} at {} is unreadable: {source}", crate::git_path::user_display_path(.path))]
    #[non_exhaustive]
    CorruptLooseObject {
        /// Object id implied by the loose object path.
        oid: crate::objects::ObjectId,
        /// Path to the loose object file (often under `.git/objects/`).
        path: std::path::PathBuf,
        /// Underlying zlib, header, or I/O failure.
        #[source]
        source: Box<HistoryEngineError>,
    },

    /// A pack entry names the object but its stored bytes could not be decoded.
    #[error(
        "packed object {oid} in {} (offset {offset}) is unreadable: {source}",
        pack.display()
    )]
    #[non_exhaustive]
    CorruptPackedObject {
        /// Object id from the pack index.
        oid: crate::objects::ObjectId,
        /// Path to the `.pack` file.
        pack: std::path::PathBuf,
        /// Byte offset of the object record in the pack.
        offset: u64,
        /// Underlying zlib, header, or I/O failure.
        #[source]
        source: Box<HistoryEngineError>,
    },

    /// Loose object bytes hash to a different OID than the file path implies (`git fsck` / `read_loose_object`).
    #[error("{real_oid}: hash-path mismatch, found at: {path}")]
    LooseHashMismatch {
        /// Repository-relative or filesystem path to the loose object file.
        path: String,
        /// Hex object id of the unpacked contents.
        real_oid: String,
    },

    /// A prepared loose object could not be written during parallel index staging.
    #[error("could not store {path}: {source}")]
    LooseObjectWrite {
        /// Index-relative path of the blob being stored.
        path: String,
        /// Underlying filesystem error from the object store.
        source: std::io::Error,
    },

    /// Filesystem I/O failed on a repository-relative worktree path (checkout, staging, diff).
    #[error("{}", crate::worktree::io_error::format_worktree_path_io_message(.path, *operation, .source))]
    WorktreePathIo {
        /// Index- or worktree-relative path grit was operating on.
        path: String,
        /// What the operation was trying to do when I/O failed.
        operation: WorktreePathOperation,
        /// Underlying OS error.
        source: std::io::Error,
    },

    /// Filesystem I/O failed on a path under the repository git directory (objects, refs, logs, …).
    #[error("{}", crate::git_dir::io_error::format_git_dir_io_message(.path, *operation, .source))]
    GitDirPathIo {
        /// Display path such as `.git/objects` or `.git/refs/heads/main`.
        path: String,
        /// What the operation was trying to do when I/O failed.
        operation: GitDirPathOperation,
        /// Underlying OS error.
        source: std::io::Error,
    },

    /// A streamed object write or hash ended before the declared size was read.
    #[error("object stream ended after {actual} of {expected} bytes")]
    ObjectStreamTruncated {
        /// Payload size the caller declared.
        expected: u64,
        /// Bytes the stream delivered before end of file.
        actual: u64,
    },

    /// A worktree file hashed earlier in an operation had different content when stored.
    #[error("{path} changed while it was being added")]
    WorktreeFileChanged {
        /// Index-relative path of the file.
        path: String,
    },

    /// The index file is missing, truncated, or has a bad header.
    #[error("index error: {0}")]
    IndexError(String),

    /// Stage-1/2/3 entries are present; a stage-0 tree cannot be built.
    #[error("index contains unmerged entries")]
    IndexUnmerged,

    /// Another maintenance run holds the repository's `gc.pid` lock
    /// ([`crate::maintenance::GcLock`]); nothing was changed.
    #[error("{}", maintenance_running_message(holder))]
    MaintenanceRunning {
        /// Process recorded in `gc.pid`.
        holder: crate::maintenance::GcLockHolder,
    },

    /// Staging ([`crate::porcelain::add::stage`] with explicit paths, or the auto-stage pass of
    /// [`crate::porcelain::commit::commit_worktree`]) refused paths that still contain conflict
    /// markers: either they are unmerged, or their conflict stages were dropped (the index keeps
    /// a resolve-undo record) and at least one marker block is not in the `HEAD` version. A dropped-stage
    /// path stops the whole staging pass before the index is written.
    #[error("unmerged paths still have conflict markers")]
    UnmergedConflictMarkers {
        /// Repository-relative paths that were not staged because markers remain.
        paths: Vec<String>,
    },

    /// A file path is a prefix of another index path at the same tree level.
    #[error(
        "index path prefix conflict between '{}' and '{}'",
        directory.display(),
        file.display()
    )]
    IndexPathPrefixConflict {
        /// Shorter path (treated as the ambiguous directory prefix).
        directory: std::path::PathBuf,
        /// Longer path that extends the prefix with an additional component.
        file: std::path::PathBuf,
    },

    /// The `FSMN` index extension ends before its header, token, or EWAH payload.
    #[error("index fsmonitor extension payload is truncated")]
    IndexFsmonitorExtensionTruncated,

    /// The `FSMN` extension header version is not supported.
    #[error("index fsmonitor extension has unsupported header version {0}")]
    IndexFsmonitorExtensionBadVersion(u32),

    /// The EWAH bitmap embedded in `FSMN` could not be decoded.
    #[error("index fsmonitor extension EWAH bitmap is invalid")]
    IndexFsmonitorExtensionEwahInvalid,

    /// The cache-tree extension references more entries than the index contains. Git emits this
    /// (verbatim, prefixed with `error: `) when a tree with duplicate path entries is read into the
    /// index (`t4058-diff-duplicates`).
    #[error("corrupted cache-tree has entries not present in index")]
    CacheTreeCorrupt,

    /// A reference name or value is invalid.
    #[error("invalid ref: {0}")]
    InvalidRef(String),

    /// A `packed-refs` line Git would reject when reading the ref database.
    #[error("unexpected line in {path}: {line}")]
    PackedRefsUnexpectedLine {
        /// Path to the `packed-refs` file being read.
        path: String,
        /// Offending line contents.
        line: String,
    },

    /// A general path-related error (invalid UTF-8, out-of-bounds, etc.).
    #[error("path error: {0}")]
    PathError(String),

    /// `--pathspec-from-file` parse failure.
    #[error(transparent)]
    PathspecFromFile(#[from] PathspecFromFileParseError),

    /// A tree/index path component is forbidden (`.git`, HFS/NTFS aliases, etc.).
    #[error("invalid path '{0}'")]
    InvalidPath(String),

    /// A repository-relative path failed [`crate::path_validate`] rules while populating the index or checkout.
    #[error("invalid path '{path}': {reason}")]
    InvalidPathComponent {
        /// Full repository-relative path that was rejected.
        path: String,
        /// Which component rule failed.
        reason: crate::path_validate::PathComponentError,
    },

    /// A configuration file parsing or access error.
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// Submodule git dir layout or submodule state failure.
    #[error(transparent)]
    Submodule(#[from] SubmoduleError),

    /// Branch create, delete, or upstream configuration failure.
    #[error(transparent)]
    Branch(#[from] crate::porcelain::branch_error::BranchError),

    /// Commit, identity, signing, and patch-scoped commit failures.
    #[error(transparent)]
    Commit(#[from] crate::porcelain::commit_error::CommitError),

    /// Merge, cherry-pick, revert, and in-progress operation failures.
    #[error(transparent)]
    Merge(#[from] crate::porcelain::merge_error::MergeError),

    /// Reference lock failure while updating refs.
    #[error(transparent)]
    RefLock(#[from] RefLockError),

    /// Reference store transaction or iteration failure.
    #[error(transparent)]
    RefStore(crate::refs::store::RefStoreError),

    /// Reftable file, stack, or transaction failure.
    #[error(transparent)]
    Reftable(#[from] crate::reftable::ReftableError),

    /// Filter or EOL conversion failure.
    #[error(transparent)]
    Filter(#[from] FilterError),

    /// Patch application failure.
    #[error(transparent)]
    Apply(#[from] ApplyError),

    /// Path resolves outside the repository.
    #[error(transparent)]
    PathOutsideRepo(#[from] PathOutsideRepoError),

    /// HTTP authentication failed: the server required credentials (`401`) and
    /// either no credential provider was wired, the provider could not supply a
    /// usable username/password, the server demanded an unsupported auth scheme,
    /// or the supplied credentials were rejected.
    ///
    /// Distinct from [`crate::error::Error::Verbatim`] so embedders can detect an authentication
    /// failure (and e.g. fall back to an interactive/subprocess path) rather than
    /// string-matching, and so the failure surfaces typed instead of hanging.
    #[error("authentication failed for {url}: {reason}")]
    Auth {
        /// Repository URL with credentials redacted and smart-HTTP endpoint suffixes
        /// (`/info/refs?service=…`, `/git-upload-pack`, `/git-receive-pack`) removed.
        url: String,
        /// What went wrong.
        reason: AuthFailure,
    },

    /// A credential provider could not supply a complete username and password:
    /// no helper had them and no askpass program could be asked.
    ///
    /// Returned by [`HelperCredentialProvider`](crate::credentials::HelperCredentialProvider)
    /// instead of prompting on a terminal; transports surface it as
    /// [`AuthFailure::CredentialsUnavailable`].
    #[error("{}", crate::credentials::NON_INTERACTIVE_MESSAGE)]
    CredentialsUnavailable {
        /// The `credential.helper` programs that were asked, in order (empty when none
        /// is configured for the URL).
        helpers: Vec<String>,
    },

    /// An atomic wire push was requested, but the remote did not advertise the
    /// `atomic` capability. No ref-update commands or pack data were sent.
    #[error("the remote does not support atomic pushes")]
    PushAtomicUnsupported,

    /// A push carried `--push-option` values but the remote `git-receive-pack`
    /// did not advertise the `push-options` capability, so the options cannot be
    /// transmitted.
    ///
    /// Distinct from [`crate::error::Error::Verbatim`] so embedders can detect this specific
    /// negotiation failure (and e.g. fall back to a subprocess push) rather than
    /// string-matching. The message matches Git's
    /// `fatal: the receiving end does not support push options`.
    #[error("the receiving end does not support push options")]
    PushOptionsUnsupported,

    /// `git fetch --unshallow` on a repository that is not shallow.
    #[error("repository is not shallow")]
    FetchUnshallowNotShallow,

    /// A deepen/shallow fetch was requested but the remote did not advertise the
    /// `shallow` capability.
    #[error("the remote does not support shallow fetches")]
    FetchShallowUnsupported,

    /// A `--deepen` fetch was requested but the remote did not advertise
    /// `deepen-relative`.
    #[error("the remote does not support deepen-relative")]
    FetchDeepenRelativeUnsupported,

    /// A bundle fetch or verify found prerequisite commits missing from history.
    #[error("repository lacks these prerequisite commits")]
    BundleMissingPrerequisites,

    /// Streaming fetch/clone temp pack disappeared before index-pack (often real-time antivirus).
    #[error(
        "received pack file missing: {}. If this persists, check real-time antivirus or disk cleanup for quarantined pack temp files.",
        crate::git_path::user_display_path(.path)
    )]
    ReceivedPackTempMissing {
        /// Path to the temp pack that was expected on disk.
        path: std::path::PathBuf,
    },

    /// A pathspec's `:(attr:...)` magic is malformed.
    #[error("invalid attr magic in pathspec '{spec}': {reason}")]
    InvalidAttrPathspec {
        /// The offending pathspec as given.
        spec: String,
        /// What is wrong with its attr clause.
        #[source]
        reason: AttrPathspecError,
    },

    /// A pathspec did not match any file known to Git (index or HEAD tree).
    #[error("pathspec '{spec}' did not match any file(s) known to git")]
    PathspecNoMatch {
        /// User-supplied pathspec that matched nothing.
        spec: String,
    },

    /// [`crate::porcelain::restore::restore_paths`] conflict options were combined with `--staged`.
    #[error("conflict restore options cannot be used with --staged")]
    RestoreConflictWithStaged,

    /// [`crate::porcelain::restore::restore_paths`] conflict options were combined with `--source`.
    #[error("conflict restore options cannot be used with --source")]
    RestoreConflictWithSource,

    /// A conflict side was missing from the index for the requested path.
    #[error("path '{path}' does not have their version")]
    RestoreConflictMissingTheirs {
        /// Repository-relative path.
        path: String,
    },

    /// A conflict side was missing from the index for the requested path.
    #[error("path '{path}' does not have our version")]
    RestoreConflictMissingOurs {
        /// Repository-relative path.
        path: String,
    },

    /// Three-way remerge needs stages 1, 2, and 3 in the index.
    #[error("path '{path}' is not unmerged")]
    RestoreConflictNotUnmerged {
        /// Repository-relative path.
        path: String,
    },

    /// [`crate::porcelain::restore::restore_paths`] was asked to restore the worktree from the
    /// index for paths that still have conflict stages; pick a side or rebuild the markers instead.
    ///
    /// Nothing is written when this is returned.
    #[error("{}", restore_unmerged_message(paths))]
    RestorePathUnmerged {
        /// Matched repository-relative paths with conflict stages (sorted, deduplicated).
        paths: Vec<String>,
    },

    /// A literal pathspec names a file under a symlink directory component.
    #[error("pathspec '{spec}' is beyond a symbolic link")]
    PathspecBeyondSymlink {
        /// User-supplied pathspec that crosses a symlink directory.
        spec: String,
    },

    /// An embedded git repository (nested `.git` checkout) has no commit checked out on its
    /// current branch, so it cannot be recorded as a gitlink or as regular files.
    #[error("embedded git repository '{path}' has no commit checked out")]
    EmbeddedRepoWithoutCommit {
        /// Repository-relative path to the nested checkout directory.
        path: String,
    },

    /// Explicit [`crate::porcelain::add::stage`] pathspecs name paths excluded by ignore rules.
    #[error("the following paths are ignored:\n{}", paths.join("\n"))]
    PathspecIgnored {
        /// Repository-relative paths (or user labels) that cannot be staged while ignored.
        paths: Vec<String>,
    },

    /// [`crate::porcelain::paths::remove_paths`] refused paths whose removal would lose
    /// content, because [`crate::porcelain::paths::RemoveOptions::force`] was not set.
    #[error(
        "refusing to remove paths with unsaved content:\n{}",
        paths.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n")
    )]
    RemoveBlocked {
        /// Each refused path with the reason it was refused.
        paths: Vec<crate::porcelain::paths::BlockedRemoval>,
    },

    /// A directory pathspec was used without `recursive` ([`crate::porcelain::paths::remove_paths`]).
    #[error("not removing '{0}' recursively without -r")]
    NotRemovingRecursively(String),

    /// A porcelain path operation requires a checked-out work tree.
    #[error("this operation must be run in a work tree")]
    RequiresWorkTree,

    /// [`crate::porcelain::paths::remove_paths`] was called with no pathspecs.
    #[error("you must specify path(s) to remove")]
    RemovePathsRequired,

    /// [`crate::porcelain::paths::move_path`] / [`crate::porcelain::paths::move_paths`] source is not tracked.
    #[error("'{src}' is not under version control")]
    MoveSourceNotTracked {
        /// Untracked or missing source path.
        src: String,
    },

    /// Move refused because the destination already exists without `--force`.
    #[error("destination '{destination}' already exists")]
    MoveDestinationExists {
        /// Source path in the move request.
        src: String,
        /// Existing destination path.
        destination: String,
    },

    /// Move refused because a trailing slash named a destination that is not a directory.
    #[error("destination '{destination}' is not a directory")]
    MoveDestinationNotDirectory {
        /// Destination path (trailing slash already removed).
        destination: String,
    },

    /// Move refused because the destination lies under the source directory.
    #[error("cannot move '{src}' to a subdirectory of itself, '{destination}'")]
    MoveIntoSubdirectory {
        /// Source directory path.
        src: String,
        /// Invalid destination under the source.
        destination: String,
    },

    /// Move refused because source and destination are the same path.
    #[error("cannot move '{src}' into itself")]
    MoveIntoSelf {
        /// Source path.
        src: String,
        /// Destination equal to the source.
        destination: String,
    },

    /// Move refused because the source has no index entries.
    #[error("nothing to move from '{src}'")]
    MoveNothingToMove {
        /// Source path with no index entries.
        src: String,
    },

    /// Multi-source move would write more than one source to the same destination path.
    #[error("multiple sources would move to '{destination}'")]
    MoveDuplicateDestination {
        /// Destination path that would receive more than one source.
        destination: String,
    },

    /// A worktree source path is missing or not a file, directory, or symlink.
    #[error("cannot move '{src}' to '{destination}': bad source path")]
    MoveBadSource {
        /// Source path.
        src: String,
        /// Planned destination path.
        destination: String,
    },

    /// Internal invariant: a planned rename references a missing index entry.
    #[error("internal error: missing index entry for '{path}'")]
    MoveMissingIndexEntry {
        /// Index path that should exist during a move.
        path: String,
    },

    /// The remote could not be read or written: a missing repository, a failed connection, an
    /// HTTP error status, or no Git handshake.
    ///
    /// Use [`crate::error::Error::is_remote_unreadable`] to test for it through wrapper errors.
    ///
    /// Not used for cancellation; see [`crate::error::Error::is_cancelled`].
    #[error("{}", remote_unreadable_message(url.as_deref(), reason))]
    RemoteUnreadable {
        /// Remote URL or resolved local path, with credentials redacted. `None` when the
        /// failing layer only had a connection, not the URL it was opened from.
        url: Option<String>,
        /// What went wrong.
        reason: RemoteUnreadableReason,
    },

    /// The remote was reached but aborted the conversation with its own error text: an
    /// `ERR <message>` pkt-line (for example upload-pack declining a ref it does not
    /// serve) or a packet on side-band channel 3.
    ///
    /// The text is the server's, not grit's: show it to the user, but do not parse it.
    #[error("remote error: {message}")]
    RemoteFatal {
        /// The server's message, without the `ERR ` prefix or trailing whitespace.
        message: String,
    },

    /// The upload-pack serving a local path or `file://` URL in this process failed
    /// (for example it was asked for an object it does not have). Repository errors
    /// of the served side are returned as themselves instead.
    #[error("local upload-pack: {0}")]
    LocalUploadPack(Box<crate::serve::ServeError>),

    /// The thread serving a local path or `file://` URL panicked.
    #[error("local upload-pack stopped unexpectedly")]
    LocalUploadPackPanicked,

    /// A resumable bundle download or upload failed.
    #[error(transparent)]
    Resumable(#[from] Box<crate::resumable::ResumableError>),

    /// Explicit fetch refspec resolution failed (missing or ambiguous source).
    #[error("{0}")]
    FetchRefspec(#[from] crate::fetch_request::FetchRefspecError),

    /// Fetch refused to update the branch checked out in this or a linked worktree.
    #[error("cannot fetch into checked-out branch '{branch}'")]
    FetchIntoCheckedOutBranch {
        /// Short branch name under `refs/heads/`.
        branch: String,
    },

    /// Checkout or switch would overwrite local or untracked worktree content.
    #[error("{message}")]
    WouldOverwrite {
        /// Whether tracked local edits or untracked paths block the operation.
        reason: OverwriteReason,
        /// Repository-relative paths that would be overwritten (sorted, deduplicated).
        paths: Vec<String>,
        /// Human-readable summary (depends on [`OverwriteReason`]).
        message: String,
    },

    /// Staged or unstaged changes block an operation that needs a clean worktree.
    #[error("you have uncommitted changes — commit them before {operation}")]
    DirtyWorktree {
        /// The operation that was refused.
        operation: WorktreeOperation,
        /// Repository-relative paths with staged or unstaged changes (sorted, deduplicated;
        /// empty when the caller only knew that something changed).
        paths: Vec<String>,
    },

    /// A long-running library operation stopped at a cancellation checkpoint.
    ///
    /// Use [`crate::error::Error::is_cancelled`] to test for it through wrapper errors
    /// ([`crate::remote::RemoteError::is_cancelled`], [`crate::remote::clone::CloneError::is_cancelled`], …).
    #[error("operation cancelled")]
    Cancelled,

    /// A checkout failed or was cancelled before it finished, and putting the work tree back
    /// failed for some paths.
    ///
    /// Refs and the index are unchanged, as after any failed checkout; only `unrestored`
    /// differ from the index. Use [`crate::error::Error::is_cancelled`] to see whether `source` was a
    /// cancellation.
    #[error("{source}; {} work-tree path(s) could not be restored", unrestored.len())]
    CheckoutRollbackIncomplete {
        /// Why the checkout stopped.
        source: Box<HistoryEngineError>,
        /// Repository-relative paths left different from the index (sorted, deduplicated).
        unrestored: Vec<String>,
    },

    /// Planning input was invalid for history rewrite (mirrors [`super::PlanningError`]).
    #[error(transparent)]
    Planning(#[from] PlanningError),

    /// A resumable rebase session failed outside the core rewrite operation.
    #[error(transparent)]
    RebaseSession(Box<super::RebaseSessionError>),

    /// Catch-all text from the umbrella [`crate::error::Error::Verbatim`] (no `Error::History` recursion).
    #[error("{0}")]
    UmbrellaMessage(String),

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

    /// A local branch is checked out as `HEAD` in a linked or main worktree.
    #[error("branch '{branch}' is already checked out in '{worktree_path}'")]
    BranchCheckedOutInWorktree {
        /// Short branch name (e.g. `main`).
        branch: String,
        /// Path to the worktree that has the branch checked out.
        worktree_path: String,
    },

    /// `refs/heads/<branch>` does not exist.
    #[error("no branch named '{branch}'")]
    BranchNotFound {
        /// Short branch name (e.g. `topic`).
        branch: String,
    },

    /// [`list_branches`](crate::branch_tracking::list_branches) with [`BranchDetail::Names`](crate::branch_tracking::BranchDetail::Names) cannot use date sort.
    #[error("branch list with Names detail cannot use date sort (date sort reads commit objects)")]
    BranchListNamesDateSort,

    /// Tag create/delete validation failure ([`crate::porcelain::tag::TagError`]).
    #[error(transparent)]
    Tag(#[from] crate::porcelain::tag::TagError),

    /// Linked worktree add/remove/move/lock/repair failure ([`crate::worktree::WorktreeError`]).
    #[error(transparent)]
    Worktree(#[from] crate::worktree::WorktreeError),

    /// Stash commit object has fewer parents or an invalid shape for apply/show.
    #[error("corrupt stash commit: {0}")]
    CorruptStash(&'static str),

    /// `stash@{{n}}` is out of range for the current stash reflog.
    #[error("stash entry stash@{{{n}}} not found")]
    StashNotFound {
        /// Stash reflog index (`stash@{{n}}`).
        n: usize,
    },

    /// A stash commit id was given to [`crate::repo_handles::RepositoryStash::pop`] but is not on
    /// `refs/stash` (use [`crate::repo_handles::RepositoryStash::apply_commit`] to recover it).
    #[error("stash commit {oid} is not on the stash stack")]
    StashCommitNotOnStack {
        /// Stash W commit object id.
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

    /// `ref@{{n}}` is out of range for the current reflog.
    #[error("reflog entry {refname}@{{{index}}} not found")]
    ReflogIndexOutOfRange {
        /// Ref whose reflog was read.
        refname: String,
        /// Requested newest-first index.
        index: usize,
    },

    /// Pushing or creating a stash ([`crate::repo_handles::RepositoryStash::push`]) requires at least one commit.
    #[error("cannot stash without a commit")]
    StashNoInitialCommit,

    /// Stash create/apply/pop requires a working tree (not a bare repository).
    #[error("cannot stash in a bare repository")]
    StashBareRepository,

    /// [`crate::blame::BlameFinalSource::Worktree`] was requested in a bare repository.
    #[error("cannot blame the working tree of a bare repository")]
    BlameWorktreeBareRepository,

    /// [`crate::repo_handles::RepositoryStash::apply`] with `restore_index` could
    /// not merge the current index with the stashed index parent.
    #[error("conflicts in index while applying stash with --index")]
    StashIndexConflict,

    /// Incompatible [`StashPushOptions`](crate::stash::StashPushOptions) flags.
    #[error("{0}")]
    StashOptionConflict(String),

    /// Partial [`crate::repo_handles::RepositoryStash::push`] hunk selection failed.
    #[error(transparent)]
    HunkSelection(#[from] crate::porcelain::hunk_select::HunkSelectError),

    /// HEAD does not resolve while applying a stash entry.
    #[error("missing HEAD while applying stash")]
    StashMissingHead,

    /// Applying a stash would overwrite locally modified worktree content.
    #[error(
        "your local changes to the following files would be overwritten by stash apply: {paths}"
    )]
    StashWouldOverwriteLocalChanges {
        /// Space-separated list of paths that would be overwritten.
        paths: String,
    },

    /// The index could not be written after a successful stash apply.
    #[error("writing index after stash apply: {0}")]
    StashIndexWrite(String),

    /// A symlink blob in a stash commit is not valid UTF-8.
    #[error("symlink target is not UTF-8")]
    StashSymlinkNotUtf8,

    /// Applying a stash with an untracked-files parent would replace an existing worktree path.
    #[error("stash untracked restore blocked: '{path}' already exists on disk")]
    StashCouldNotRestoreUntracked {
        /// Worktree path that already exists and blocks untracked restore.
        path: String,
    },

    /// [`RepositoryNotes::add`](crate::repo_handles::RepositoryNotes::add) was called without
    /// `force` when a note already exists on the object.
    #[error("note for object {object} already exists")]
    NoteAlreadyExists {
        /// Annotated object that already has a note.
        object: crate::objects::ObjectId,
    },

    /// A configured or environment notes ref is not under `refs/notes/`.
    #[error("notes ref '{0}' must start with 'refs/notes/'")]
    InvalidNotesRef(String),

    /// Unknown `notes.rewriteMode` or `GIT_NOTES_REWRITE_MODE` value.
    #[error("unknown notes rewrite mode '{0}'")]
    NotesRewriteMode(String),

    /// A notes ref resolves to an object that cannot hold notes (notes reads accept a commit or
    /// a tree; notes merges require a commit).
    #[error("notes ref '{notes_ref}' points to a {found}, not a notes commit")]
    NotesRefNotCommit {
        /// Notes ref (or revision) that was read.
        notes_ref: String,
        /// Kind of object the ref resolved to.
        found: crate::objects::ObjectKind,
    },

    /// An object reached while reading a notes tree has the wrong kind (for example a note
    /// entry that is not a blob).
    #[error("notes object {oid} is a {found}, expected a {expected}")]
    NotesObjectKind {
        /// Object that was read.
        oid: crate::objects::ObjectId,
        /// Kind the notes layout requires at this position.
        expected: crate::objects::ObjectKind,
        /// Kind actually stored.
        found: crate::objects::ObjectKind,
    },

    /// [`notes_merge_inner`](crate::notes::notes_merge_inner) was asked to merge two notes refs
    /// that both have no notes commit.
    #[error("cannot merge notes: neither '{local}' nor '{remote}' has any notes")]
    NotesMergeBothEmpty {
        /// Local notes ref (merge destination).
        local: String,
        /// Remote notes ref (merge source).
        remote: String,
    },

    /// A manual notes merge was started while an earlier one still has conflict files in the
    /// notes merge worktree.
    #[error("unfinished notes merge: conflict files remain in {}", crate::git_path::user_display_path(.path))]
    NotesMergeInProgress {
        /// The notes merge worktree directory.
        path: std::path::PathBuf,
    },

    /// Multi-pack-index load or write failure.
    #[error(transparent)]
    Midx(#[from] MidxError),

    /// Offloading blobs to a large-object promisor was refused
    /// ([`crate::maintenance::offload_blobs`]).
    #[error(transparent)]
    Offload(#[from] crate::offload::OffloadError),

    /// Revision parsing failed ([`crate::rev_parse_error::RevParseError`]).
    #[error(transparent)]
    RevParse(#[from] crate::rev_parse_error::RevParseError),

    /// Revision walking / `rev-list` option parsing failed ([`crate::rev_list_error::RevListError`]).
    #[error(transparent)]
    RevList(#[from] crate::rev_list_error::RevListError),

    /// A Git hook subprocess failed or could not be started.
    #[error(transparent)]
    Hook(crate::hooks::HookError),

    /// The operation needs the files-backed `objects/` directory, but the repository's
    /// [`Odb`](crate::odb::Odb) uses a custom primary store
    /// ([`RepositoryOptions::with_object_store`](crate::config::environment::RepositoryOptions::with_object_store)). Nothing was written; use a files-backed repository for this operation.
    #[error("{operation} requires the files-backed object store; it is not supported with a custom primary")]
    UnsupportedObjectStore {
        /// Operation that was refused.
        operation: FilesOnlyOperation,
    },

    /// The operation fetches objects back from a promisor remote, and the repository
    /// has none: no `remote.<name>.promisor = true` and no `extensions.partialClone`.
    /// Nothing was changed.
    #[error("{operation} needs a promisor remote to fetch objects back from")]
    NoPromisorRemote {
        /// Operation that was refused (for example `"drop-large-blobs"`).
        operation: &'static str,
    },

    /// The operation would delete objects, and `extensions.preciousObjects` says this
    /// repository never loses any (it may be shared as an alternate). Nothing was
    /// changed.
    #[error("{operation} deletes objects, which extensions.preciousObjects forbids here")]
    PreciousObjectsRepository {
        /// Operation that was refused (for example `"drop-large-blobs"`).
        operation: &'static str,
    },

    /// [`Repository::open`](crate::repo::Repository::open) was given a custom primary
    /// ([`RepositoryOptions::with_object_store`](crate::config::environment::RepositoryOptions::with_object_store))
    /// or a read source ([`OdbBuilder`](crate::odb::OdbBuilder)) that hashes objects with a different algorithm than the
    /// repository's `extensions.objectformat` (SHA-1 when unset).
    #[error(
        "object store hashes with {} but the repository's object format is {}",
        store.name(),
        repository.name()
    )]
    #[non_exhaustive]
    ObjectFormatMismatch {
        /// The repository's object format from its config.
        repository: crate::objects::HashAlgo,
        /// The first injected store's [`hash_algo`](crate::odb::store::ObjectStore::hash_algo)
        /// that differs from `repository`.
        store: crate::objects::HashAlgo,
    },

    /// Parsing a [`crate::tree_sha256::TreeSha256`] from hex failed (expected 64 hex digits).
    #[error("invalid tree-sha256 hex")]
    TreeSha256InvalidHex,

    /// A revision passed to [`crate::tree_sha256::tree_sha256`] did not peel to a commit or tree.
    #[error("tree-sha256: revision resolves to {found:?}, not a commit or tree")]
    TreeSha256InvalidRevision {
        /// Object kind after peeling annotated tags.
        found: crate::objects::ObjectKind,
    },

    /// One or more submodules required for [`crate::tree_sha256::tree_sha256`] are missing.
    #[error("tree-sha256: missing submodules")]
    TreeSha256MissingSubmodules(Vec<crate::tree_sha256::MissingSubmodule>),

    /// `--tree-sha256` was requested with signing explicitly disabled.
    #[error("tree-sha256 requires a signed tag")]
    TreeSha256RequiresSigning,

    /// [`crate::tree_sha256::verify_tag`] was given an object that is not an annotated tag.
    #[error("verify-tag: object {oid} is a {found:?}, not an annotated tag")]
    TagVerifyNotTag {
        /// Object id passed to verification.
        oid: crate::objects::ObjectId,
        /// Kind stored in the ODB.
        found: crate::objects::ObjectKind,
    },

    /// Annotated tag header block has no `object` line.
    #[error("verify-tag: tag has no object header")]
    TagVerifyMissingObjectHeader,

    /// Annotated tag carries more than one `tree-sha256` header line.
    #[error("verify-tag: tag has more than one tree-sha256 header")]
    TagVerifyDuplicateTreeSha256Header,

    /// Annotated tag tree-sha256 header parsing failed.
    #[error(transparent)]
    TreeSha256Claim(#[from] crate::tree_sha256::TreeSha256ClaimError),

    /// `grit blame --ignore-rev` did not resolve to a commit.
    #[error("cannot find revision {rev} to ignore")]
    BlameIgnoreRevUnknown {
        /// Revision expression from `--ignore-rev`.
        rev: String,
    },

    /// `--ignore-revs-file` could not be read.
    #[error("could not open object name list: {path}")]
    BlameIgnoreRevsFileOpen {
        /// Path argument (before expansion).
        path: String,
        /// Underlying open error.
        #[source]
        source: std::io::Error,
    },

    /// A line in an ignore-revs file is not a full-length object id.
    #[error("invalid object name: {rev}")]
    BlameIgnoreRevsFileInvalidRev {
        /// Path argument for the file.
        path: String,
        /// Offending line text after comment/trim handling.
        rev: String,
    },
}

impl HistoryEngineError {
    pub(super) fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

impl From<crate::error::Error> for HistoryEngineError {
    fn from(err: crate::error::Error) -> Self {
        error_to_engine(err)
    }
}

impl From<HistoryEngineError> for crate::error::Error {
    fn from(err: HistoryEngineError) -> Self {
        Self::History(super::HistoryError::Engine(Box::new(err)))
    }
}
