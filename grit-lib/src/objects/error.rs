//! Typed failures for object storage, packs, MIDX, commit-graph, and fsck.

use std::path::PathBuf;

use thiserror::Error;

use crate::error::{Error, LockHeldError, PathIoError};
use crate::fsck_standalone::FsckError;
use crate::hash::TrailerMismatch;
use crate::ident::SignatureError;
use crate::maintenance::GcLockHolder;
use crate::midx_error::MidxError;
use crate::objects::{ObjectId, ObjectKind};
use crate::offload::OffloadError;
use crate::tree_sha256::{MissingSubmodule, TreeSha256ClaimError};

/// Result type for object-store public APIs.
pub type ObjectResult<T> = Result<T, ObjectError>;

/// Why stored object bytes failed a structural check (pack verify, loose read, …).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CorruptObjectReason {
    /// Free-form detail when no finer classification exists yet.
    Detail {
        /// Human-readable corruption detail (Grit-owned wording).
        message: String,
    },
}

impl CorruptObjectReason {
    /// Build a detail reason from formatted text.
    #[must_use]
    pub fn detail(message: impl Into<String>) -> Self {
        Self::Detail {
            message: message.into(),
        }
    }
}

/// Object database, pack, MIDX, commit-graph, and fsck failures.
///
/// Wrapped by [`Error::Object`](crate::error::Error::Object) at the repository boundary.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ObjectError {
    /// A supplied object ID string was not valid hex or the wrong length.
    #[error("invalid object id '{input}'")]
    InvalidObjectId {
        /// Raw id text from the caller.
        input: String,
    },

    /// SHA-1 object hashing detected a chosen-prefix collision (sha1dc).
    #[error("SHA-1 hash collision detected for object {oid}")]
    HashCollision {
        /// Plain SHA-1 digest for the colliding input (not safe to use as an object id).
        oid: ObjectId,
    },

    /// The requested object does not exist in the object store.
    #[error("object not found: {spec}")]
    ObjectNotFound {
        /// Parsed object id when `spec` is a full object id.
        oid: Option<ObjectId>,
        /// Lookup label as the caller supplied it.
        spec: String,
    },

    /// A partial-clone promisor object is not present locally and no fetcher ran.
    #[error("promised object not available locally: {0}")]
    PromisedObjectMissing(ObjectId),

    /// Promised objects could not be fetched from every promisor remote.
    #[error("{}", promisor_fetch_failed_message(.missing, .failures))]
    PromisorFetchFailed {
        /// The first object still missing after every remote was tried.
        missing: ObjectId,
        /// One entry per remote that failed, in try order.
        failures: Vec<(String, Box<ObjectError>)>,
    },

    /// An object's stored data is corrupt or malformed.
    #[error("corrupt object: {}", corrupt_object_display(.oid, .reason))]
    CorruptObject {
        /// Object id when known.
        oid: Option<ObjectId>,
        /// Why the bytes are unusable.
        reason: CorruptObjectReason,
    },

    /// A pack index (`.idx`) file's trailing checksum does not match its contents.
    #[error(
        "corrupt object: index checksum mismatch for {}: {mismatch}",
        crate::git_path::user_display_path(.path)
    )]
    PackIndexChecksumMismatch {
        /// The `.idx` file that failed verification.
        path: PathBuf,
        /// Expected and stored trailer digests.
        mismatch: TrailerMismatch,
    },

    /// Indexing or unpacking a pack left deltas whose bases are neither in the pack nor in the ODB.
    #[error("corrupt object: {count} delta(s) could not be resolved")]
    UnresolvedDeltas {
        /// Number of deltas left unresolved.
        count: u64,
    },

    /// A non-thin pack reached a delta before its base was written.
    #[error("pack delta for {target} comes before its base {base}")]
    PackDeltaBaseNotWritten {
        /// The object that would have been stored as a delta.
        target: ObjectId,
        /// Its delta base, not written ahead of it.
        base: ObjectId,
    },

    /// [`super::Objects::write`] refused a payload that `git fsck` would reject.
    #[error("refusing to write invalid {kind} object: {}", .check.report_line())]
    InvalidObject {
        /// Kind the payload was meant to be stored as.
        kind: ObjectKind,
        /// The first fsck rule the payload breaks.
        check: FsckError,
    },

    /// [`super::Objects::write_tree`] refused an entry.
    #[error(transparent)]
    InvalidTreeEntry(#[from] super::TreeEntryError),

    /// A signature cannot be written as an author, committer or tagger line.
    #[error(transparent)]
    InvalidSignature(#[from] SignatureError),

    /// An object exists but has a different kind than the operation requires.
    #[error("object {oid} is a {actual}, not a {expected}")]
    UnexpectedObjectKind {
        /// The object that was read.
        oid: ObjectId,
        /// The kind the operation required.
        expected: ObjectKind,
        /// The kind the object actually has.
        actual: ObjectKind,
    },

    /// A commit-graph write was given a commit whose parent is missing from the graph layers.
    #[error("commit-graph cannot encode parent {parent} of commit {commit}: parent not in graph")]
    CommitGraphParentMissing {
        /// Commit whose parent list was being encoded.
        commit: ObjectId,
        /// Parent that is missing from the graph.
        parent: ObjectId,
    },

    /// In-pack delta resolution exceeded the read-side chain depth limit.
    #[error("delta chain too deep (limit {limit})")]
    DeltaChainTooDeep {
        /// Maximum delta chain depth allowed while reading packed objects.
        limit: usize,
    },

    /// An unsupported or unknown object type was encountered.
    #[error("unknown object type '{ty}'")]
    UnknownObjectType {
        /// Raw type name or byte.
        ty: String,
    },

    /// Loose object header type field exceeds Git's 32-byte limit.
    #[error("header for {oid} too long, exceeds 32 bytes")]
    ObjectHeaderTooLong {
        /// Object id (hex) of the loose object with an oversized header.
        oid: String,
    },

    /// A zlib compression or decompression failure.
    #[error("zlib error: {detail}")]
    Zlib {
        /// Underlying zlib failure description.
        detail: String,
    },

    /// A loose object file is present but its stored bytes could not be decoded.
    #[error(
        "loose object {oid} at {} is unreadable: {source}",
        crate::git_path::user_display_path(.path)
    )]
    CorruptLooseObject {
        /// Object id implied by the loose object path.
        oid: ObjectId,
        /// Path to the loose object file.
        path: PathBuf,
        /// Underlying zlib, header, or I/O failure.
        #[source]
        source: Box<ObjectError>,
    },

    /// A pack entry names the object but its stored bytes could not be decoded.
    #[error(
        "packed object {oid} in {} (offset {offset}) is unreadable: {source}",
        pack.display()
    )]
    CorruptPackedObject {
        /// Object id from the pack index.
        oid: ObjectId,
        /// Path to the `.pack` file.
        pack: PathBuf,
        /// Byte offset of the object record in the pack.
        offset: u64,
        /// Underlying zlib, header, or I/O failure.
        #[source]
        source: Box<ObjectError>,
    },

    /// Loose object bytes hash to a different OID than the file path implies.
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

    /// A streamed object write or hash ended before the declared size was read.
    #[error("object stream ended after {actual} of {expected} bytes")]
    ObjectStreamTruncated {
        /// Payload size the caller declared.
        expected: u64,
        /// Bytes the stream delivered before end of file.
        actual: u64,
    },

    /// Another maintenance run holds the repository's `gc.pid` lock.
    #[error("{}", maintenance_running_message(.holder))]
    MaintenanceRunning {
        /// Process recorded in `gc.pid`.
        holder: GcLockHolder,
    },

    /// Multi-pack-index load or write failure.
    #[error(transparent)]
    Midx(#[from] MidxError),

    /// Offloading blobs to a large-object promisor was refused.
    #[error(transparent)]
    Offload(#[from] OffloadError),

    /// The operation fetches objects back from a promisor remote, and the repository has none.
    #[error("{operation} needs a promisor remote to fetch objects back from")]
    NoPromisorRemote {
        /// Operation that was refused (for example `"drop-large-blobs"`).
        operation: &'static str,
    },

    /// The operation needs the files-backed `objects/` directory, but the ODB uses a custom primary.
    #[error("{operation} requires the files-backed object store; it is not supported with a custom primary")]
    UnsupportedObjectStore {
        /// Operation that was refused.
        operation: &'static str,
    },

    /// The operation would delete objects, and `extensions.preciousObjects` forbids it.
    #[error("{operation} deletes objects, which extensions.preciousObjects forbids here")]
    PreciousObjectsRepository {
        /// Operation that was refused.
        operation: &'static str,
    },

    /// Custom primary or read source hashes with a different algorithm than the repository format.
    #[error(
        "object store hashes with {} but the repository's object format is {}",
        store.name(),
        repository.name()
    )]
    ObjectFormatMismatch {
        /// The repository's object format from its config.
        repository: crate::objects::HashAlgo,
        /// The injected store's hash algorithm.
        store: crate::objects::HashAlgo,
    },

    /// [`RepositoryBuilder::open`](crate::repo::RepositoryBuilder::open) ODB path mismatch.
    #[error(
        "object database {} is not the repository's objects directory {}",
        builder.display(),
        repository.display()
    )]
    OdbObjectsDirMismatch {
        /// The builder's objects directory.
        builder: PathBuf,
        /// The repository's canonical objects directory.
        repository: PathBuf,
    },

    /// Parsing a [`crate::tree_sha256::TreeSha256`] from hex failed.
    #[error("invalid tree-sha256 hex")]
    TreeSha256InvalidHex,

    /// A revision passed to tree-sha256 did not peel to a commit or tree.
    #[error("tree-sha256: revision resolves to {found:?}, not a commit or tree")]
    TreeSha256InvalidRevision {
        /// Object kind after peeling annotated tags.
        found: ObjectKind,
    },

    /// Submodules required for tree-sha256 are missing.
    #[error("tree-sha256: missing submodules")]
    TreeSha256MissingSubmodules(Vec<MissingSubmodule>),

    /// `--tree-sha256` was requested with signing explicitly disabled.
    #[error("tree-sha256 requires a signed tag")]
    TreeSha256RequiresSigning,

    /// [`crate::tree_sha256::verify_tag`] was given an object that is not an annotated tag.
    #[error("verify-tag: object {oid} is a {found:?}, not an annotated tag")]
    TagVerifyNotTag {
        /// Object id passed to verification.
        oid: ObjectId,
        /// Kind stored in the ODB.
        found: ObjectKind,
    },

    /// Annotated tag header block has no `object` line.
    #[error("verify-tag: tag has no object header")]
    TagVerifyMissingObjectHeader,

    /// Annotated tag carries more than one `tree-sha256` header line.
    #[error("verify-tag: tag has more than one tree-sha256 header")]
    TagVerifyDuplicateTreeSha256Header,

    /// Annotated tag tree-sha256 header parsing failed.
    #[error(transparent)]
    TreeSha256Claim(#[from] TreeSha256ClaimError),

    /// Path or git-dir I/O during object store operations.
    #[error("{}", path_io.message())]
    PathIo {
        /// Structured path I/O failure.
        path_io: PathIoError,
    },

    /// Lock file blocked an object store mutation.
    #[error(transparent)]
    LockHeld(#[from] LockHeldError),

    /// Custom [`crate::odb::store::ObjectStore`] backend failure.
    #[error("object store backend error")]
    Backend {
        /// Backend-specific error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// Cross-area failure wrapped while object code awaits step 12 collapse.
    #[error(transparent)]
    Nested(#[from] Box<Error>),
}

fn corrupt_object_display(oid: &Option<ObjectId>, reason: &CorruptObjectReason) -> String {
    match reason {
        CorruptObjectReason::Detail { message } => match oid {
            Some(id) => format!("{} ({id})", message),
            None => message.clone(),
        },
    }
}

fn promisor_fetch_failed_message(
    missing: &ObjectId,
    failures: &[(String, Box<ObjectError>)],
) -> String {
    let mut msg = format!("could not fetch promised object {missing}");
    for (remote, err) in failures {
        msg.push_str(&format!("; remote '{remote}': {err}"));
    }
    msg
}

fn maintenance_running_message(holder: &GcLockHolder) -> String {
    crate::error::maintenance_running_message(holder)
}

impl ObjectError {
    /// Map a repository-level [`Error`] into [`ObjectError`] when crossing into the ODB.
    #[must_use]
    pub fn from_umbrella(err: Error) -> Self {
        match err {
            Error::Object(inner) => inner,
            Error::LockHeld(lock) => Self::LockHeld(lock),
            Error::GitDirPathIo {
                path,
                operation,
                source,
            } => Self::PathIo {
                path_io: PathIoError::git_dir(path, operation, source),
            },
            other => Self::Nested(Box::new(other)),
        }
    }

    /// Build [`Self::ObjectNotFound`] from a lookup label (parses full object ids when possible).
    #[must_use]
    pub fn object_not_found(spec: impl Into<String>) -> Self {
        let spec = spec.into();
        let oid = ObjectId::from_hex(spec.trim()).ok();
        Self::ObjectNotFound { oid, spec }
    }

    /// Build [`Self::CorruptObject`] with a detail reason.
    #[must_use]
    pub fn corrupt_object(oid: Option<ObjectId>, message: impl Into<String>) -> Self {
        Self::CorruptObject {
            oid,
            reason: CorruptObjectReason::detail(message),
        }
    }

    /// Stable snake_case name for scripts (matches legacy [`Error::kind_name`] slugs).
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::InvalidObjectId { .. } => "invalid_object_id",
            Self::HashCollision { .. } => "hash_collision",
            Self::ObjectNotFound { .. } => "object_not_found",
            Self::PromisedObjectMissing(_) => "promised_object_missing",
            Self::PromisorFetchFailed { .. } => "promisor_fetch_failed",
            Self::CorruptObject { .. } => "corrupt_object",
            Self::PackIndexChecksumMismatch { .. } => "pack_index_checksum_mismatch",
            Self::UnresolvedDeltas { .. } => "unresolved_deltas",
            Self::PackDeltaBaseNotWritten { .. } => "pack_delta_base_not_written",
            Self::InvalidObject { .. } => "invalid_object",
            Self::InvalidTreeEntry(_) => "invalid_tree_entry",
            Self::InvalidSignature(_) => "invalid_signature",
            Self::UnexpectedObjectKind { .. } => "unexpected_object_kind",
            Self::CommitGraphParentMissing { .. } => "commit_graph_parent_missing",
            Self::DeltaChainTooDeep { .. } => "delta_chain_too_deep",
            Self::UnknownObjectType { .. } => "unknown_object_type",
            Self::ObjectHeaderTooLong { .. } => "object_header_too_long",
            Self::Zlib { .. } => "zlib",
            Self::CorruptLooseObject { .. } => "corrupt_loose_object",
            Self::CorruptPackedObject { .. } => "corrupt_packed_object",
            Self::LooseHashMismatch { .. } => "loose_hash_mismatch",
            Self::LooseObjectWrite { .. } => "loose_object_write",
            Self::ObjectStreamTruncated { .. } => "object_stream_truncated",
            Self::MaintenanceRunning { .. } => "maintenance_running",
            Self::Midx(_) => "midx",
            Self::Offload(_) => "offload",
            Self::NoPromisorRemote { .. } => "no_promisor_remote",
            Self::UnsupportedObjectStore { .. } => "unsupported_object_store",
            Self::PreciousObjectsRepository { .. } => "precious_objects_repository",
            Self::ObjectFormatMismatch { .. } => "object_format_mismatch",
            Self::OdbObjectsDirMismatch { .. } => "odb_objects_dir_mismatch",
            Self::TreeSha256InvalidHex => "tree_sha256_invalid_hex",
            Self::TreeSha256InvalidRevision { .. } => "tree_sha256_invalid_revision",
            Self::TreeSha256MissingSubmodules(_) => "tree_sha256_missing_submodules",
            Self::TreeSha256RequiresSigning => "tree_sha256_requires_signing",
            Self::TagVerifyNotTag { .. } => "tag_verify_not_tag",
            Self::TagVerifyMissingObjectHeader => "tag_verify_missing_object_header",
            Self::TagVerifyDuplicateTreeSha256Header => "tag_verify_duplicate_tree_sha256_header",
            Self::TreeSha256Claim(_) => "tree_sha256_claim",
            Self::PathIo { .. } => "git_dir_path_io",
            Self::LockHeld(_) => "lock_held",
            Self::Backend { .. } => "object_backend",
            Self::Nested(inner) => inner.kind_name(),
        }
    }

    /// Whether an object read failed because a stored copy is damaged.
    #[must_use]
    pub fn is_damaged_object(&self) -> bool {
        matches!(
            self,
            Self::Zlib { .. }
                | Self::CorruptObject { .. }
                | Self::CorruptLooseObject { .. }
                | Self::CorruptPackedObject { .. }
        )
    }

    /// Low-level read failure that [`Self::wrap_packed_read_failure`] may wrap.
    #[must_use]
    pub fn is_object_read_wrap_source(&self) -> bool {
        if self.is_damaged_object() {
            return true;
        }
        matches!(
            self,
            Self::ObjectHeaderTooLong { .. }
                | Self::ObjectStreamTruncated { .. }
                | Self::DeltaChainTooDeep { .. }
                | Self::UnresolvedDeltas { .. }
        )
    }

    /// Attach loose-object path context to low-level read failures.
    #[must_use]
    pub fn wrap_loose_read_failure(oid: ObjectId, path: PathBuf, err: ObjectError) -> ObjectError {
        if matches!(err, Self::CorruptLooseObject { .. }) {
            return err;
        }
        if err.is_damaged_object() {
            Self::CorruptLooseObject {
                oid,
                path,
                source: Box::new(err),
            }
        } else {
            err
        }
    }

    /// Attach pack location context to low-level read failures.
    #[must_use]
    pub fn wrap_packed_read_failure(
        oid: ObjectId,
        pack: PathBuf,
        offset: u64,
        err: ObjectError,
    ) -> ObjectError {
        if matches!(err, Self::CorruptPackedObject { .. }) {
            return err;
        }
        if err.is_damaged_object() {
            Self::CorruptPackedObject {
                oid,
                pack,
                offset,
                source: Box::new(err),
            }
        } else {
            err
        }
    }
}

impl From<Error> for ObjectError {
    fn from(err: Error) -> Self {
        Self::from_umbrella(err)
    }
}

impl From<PathIoError> for ObjectError {
    fn from(path_io: PathIoError) -> Self {
        Self::PathIo { path_io }
    }
}

impl From<ObjectError> for Error {
    fn from(err: ObjectError) -> Self {
        match err {
            ObjectError::PathIo { path_io } => path_io.into(),
            ObjectError::LockHeld(lock) => lock.into(),
            ObjectError::Nested(inner) => *inner,
            other => Self::Object(other),
        }
    }
}

impl From<crate::objects::TreeEntryError> for Error {
    fn from(err: crate::objects::TreeEntryError) -> Self {
        Error::Object(ObjectError::InvalidTreeEntry(err))
    }
}

impl From<SignatureError> for Error {
    fn from(err: SignatureError) -> Self {
        Error::Object(ObjectError::InvalidSignature(err))
    }
}

impl From<MidxError> for Error {
    fn from(err: MidxError) -> Self {
        Error::Object(ObjectError::Midx(err))
    }
}

impl From<OffloadError> for Error {
    fn from(err: OffloadError) -> Self {
        Error::Object(ObjectError::Offload(err))
    }
}

impl From<TreeSha256ClaimError> for Error {
    fn from(err: TreeSha256ClaimError) -> Self {
        Error::Object(ObjectError::TreeSha256Claim(err))
    }
}
