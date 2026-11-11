//! [`ObjectError`]: object database, pack, and promisor failures.

use thiserror::Error;

use super::{ErrorSummary, MidxError, ObjectReadFailure};

/// Object database failure: loose objects, packs, indexes, commit-graph, multi-pack-index,
/// and promisor (partial clone) objects.
///
/// Surfaced on the umbrella as [`Error::Object`](crate::error::Error::Object).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ObjectError {
    /// A supplied object ID string was not valid hex or the wrong length.
    #[error("invalid object id '{input}'")]
    InvalidId {
        /// The text that was parsed as an object id.
        input: String,
    },

    /// SHA-1 object hashing detected a chosen-prefix collision (sha1dc).
    #[error("SHA-1 hash collision detected for object {oid}")]
    HashCollision {
        /// Plain SHA-1 digest for the colliding input (not safe to use as an object id).
        oid: crate::objects::ObjectId,
    },

    /// The object `oid` is not in the object database (loose objects, packs, alternates).
    ///
    /// Returned only for a concrete object id: a read, peel or walk needed `oid` and the store
    /// does not have it. A revision expression that names nothing (an unknown branch, an
    /// abbreviated id that matches no object, `:/<text>` with no match) is
    /// [`RevisionError::NotFound`] instead; the parser also reports
    /// [`RevParseError::InvalidObjectName`] for an unresolvable `<rev>` in `<rev>:<path>` and
    /// [`RevParseError::RevisionOutOfRange`] for `~`/`^` navigation past a root commit.
    /// A partial-clone object a promisor remote should supply is [`Self::PromisedMissing`].
    ///
    /// [`RevParseError::InvalidObjectName`]: crate::revwalk::rev_parse_error::RevParseError::InvalidObjectName
    /// [`RevParseError::RevisionOutOfRange`]: crate::revwalk::rev_parse_error::RevParseError::RevisionOutOfRange
    /// [`RevisionError::NotFound`]: crate::error::RevisionError::NotFound
    #[error("object not found: {oid}")]
    NotFound {
        /// The missing object.
        oid: crate::objects::ObjectId,
    },

    /// Writing a tree from the index needs the object an index entry points at, and the object
    /// database does not have it (missing objects were not allowed for this write).
    ///
    /// Carries the entry so a report can name the path; a bare missing object is
    /// [`Self::NotFound`].
    #[error("object not found: {:o} {oid} at {path}", .mode.bits())]
    MissingTreeMember {
        /// Repository-relative path of the index entry (UTF-8 lossy).
        path: String,
        /// Mode the entry would have in the tree.
        mode: crate::objects::FileMode,
        /// The missing object.
        oid: crate::objects::ObjectId,
    },

    /// A partial-clone promisor object is not present locally and no fetcher ran.
    #[error("promised object not available locally: {oid}")]
    PromisedMissing {
        /// The missing object.
        oid: crate::objects::ObjectId,
    },

    /// Promised objects could not be fetched: every promisor remote was asked, and some
    /// failed. [`Self::PromisedMissing`] is used instead when every remote answered
    /// but none had the object.
    #[error("{}", promisor_fetch_failed_message(missing, failures))]
    PromisorFetchFailed {
        /// The first object still missing after every remote was tried.
        missing: crate::objects::ObjectId,
        /// One entry per remote that failed, in the order they were tried.
        failures: Vec<PromisorFetchFailure>,
    },

    /// A promisor fetch triggered further promisor fetches more than `limit` levels deep.
    #[error("too many nested promisor lazy fetches (limit {limit})")]
    PromisorFetchTooDeep {
        /// Maximum nesting of lazy fetches.
        limit: u32,
    },

    /// An object's stored data is corrupt or malformed.
    #[error("corrupt object: {detail}")]
    Corrupt {
        /// What is wrong with the object bytes.
        detail: String,
    },

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
    Invalid {
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
    UnexpectedKind {
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
    #[error("unknown object type '{name}'")]
    UnknownType {
        /// The type token as read.
        name: String,
    },

    /// Loose object header type field exceeds Git's 32-byte limit.
    #[error("header for {oid} too long, exceeds 32 bytes")]
    HeaderTooLong {
        /// Object id (hex) of the loose object with an oversized header.
        oid: String,
    },

    /// A zlib compression or decompression failure.
    #[error("zlib error: {detail}")]
    Zlib {
        /// The zlib library's description of the failure.
        detail: String,
    },

    /// A loose object file is present but its stored bytes could not be decoded.
    #[error("loose object {oid} at {} is unreadable: {source}", crate::git_path::user_display_path(.path))]
    #[non_exhaustive]
    CorruptLoose {
        /// Object id implied by the loose object path.
        oid: crate::objects::ObjectId,
        /// Path to the loose object file (often under `.git/objects/`).
        path: std::path::PathBuf,
        /// Underlying zlib, header, or I/O failure.
        #[source]
        source: Box<ObjectReadFailure>,
    },

    /// A pack entry names the object but its stored bytes could not be decoded.
    #[error(
        "packed object {oid} in {} (offset {offset}) is unreadable: {source}",
        pack.display()
    )]
    #[non_exhaustive]
    CorruptPacked {
        /// Object id from the pack index.
        oid: crate::objects::ObjectId,
        /// Path to the `.pack` file.
        pack: std::path::PathBuf,
        /// Byte offset of the object record in the pack.
        offset: u64,
        /// Underlying zlib, header, or I/O failure.
        #[source]
        source: Box<ObjectReadFailure>,
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

    /// A streamed object write or hash ended before the declared size was read.
    #[error("object stream ended after {actual} of {expected} bytes")]
    StreamTruncated {
        /// Payload size the caller declared.
        expected: u64,
        /// Bytes the stream delivered before end of file.
        actual: u64,
    },

    /// A bitmap reachability walk could not run or could not read the bitmap.
    #[error(transparent)]
    BitmapWalk(#[from] crate::bitmap_walk::BitmapWalkError),

    /// Multi-pack-index load or write failure.
    #[error(transparent)]
    Midx(#[from] MidxError),

    /// Offloading blobs to a large-object promisor was refused
    /// ([`Maintenance::offload_blobs`](crate::maintenance::Maintenance::offload_blobs)).
    #[error(transparent)]
    Offload(#[from] crate::offload::OffloadError),

    /// The operation fetches objects back from a promisor remote, and the repository
    /// has none: no `remote.<name>.promisor = true` and no `extensions.partialClone`.
    /// Nothing was changed.
    #[error("{operation} needs a promisor remote to fetch objects back from")]
    NoPromisorRemote {
        /// Operation that was refused (for example `"drop-large-blobs"`).
        operation: &'static str,
    },
}

/// One promisor remote's failure inside [`ObjectError::PromisorFetchFailed`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PromisorFetchFailure {
    /// Name of the promisor remote that was asked.
    pub remote: String,
    /// Why fetching from that remote failed.
    pub error: ErrorSummary,
}

impl PromisorFetchFailure {
    /// Record that fetching from `remote` failed with `error`.
    #[must_use]
    pub fn new(remote: impl Into<String>, error: &super::Error) -> Self {
        Self {
            remote: remote.into(),
            error: ErrorSummary::from(error),
        }
    }
}

fn promisor_fetch_failed_message(
    missing: &crate::objects::ObjectId,
    failures: &[PromisorFetchFailure],
) -> String {
    let causes = failures
        .iter()
        .map(|f| format!("{}: {}", f.remote, f.error))
        .collect::<Vec<_>>()
        .join("; ");
    format!("could not fetch promised object {missing} ({causes})")
}

impl ObjectError {
    /// Stable snake_case slug for this failure (the former umbrella variant names, so
    /// scripts that branch on [`Error::kind_name`](crate::error::Error::kind_name) keep working).
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::InvalidId { .. } => "invalid_object_id",
            Self::HashCollision { .. } => "hash_collision",
            Self::NotFound { .. } => "object_not_found",
            Self::MissingTreeMember { .. } => "missing_tree_member",
            Self::PromisedMissing { .. } => "promised_object_missing",
            Self::PromisorFetchFailed { .. } => "promisor_fetch_failed",
            Self::PromisorFetchTooDeep { .. } => "promisor_fetch_too_deep",
            Self::Corrupt { .. } => "corrupt_object",
            Self::PackIndexChecksumMismatch { .. } => "pack_index_checksum_mismatch",
            Self::UnresolvedDeltas { .. } => "unresolved_deltas",
            Self::PackDeltaBaseNotWritten { .. } => "pack_delta_base_not_written",
            Self::Invalid { .. } => "invalid_object",
            Self::InvalidTreeEntry(..) => "invalid_tree_entry",
            Self::InvalidSignature(..) => "invalid_signature",
            Self::UnexpectedKind { .. } => "unexpected_object_kind",
            Self::CommitGraphParentMissing { .. } => "commit_graph_parent_missing",
            Self::DeltaChainTooDeep { .. } => "delta_chain_too_deep",
            Self::UnknownType { .. } => "unknown_object_type",
            Self::HeaderTooLong { .. } => "object_header_too_long",
            Self::Zlib { .. } => "zlib",
            Self::CorruptLoose { .. } => "corrupt_loose_object",
            Self::CorruptPacked { .. } => "corrupt_packed_object",
            Self::LooseHashMismatch { .. } => "loose_hash_mismatch",
            Self::LooseObjectWrite { .. } => "loose_object_write",
            Self::StreamTruncated { .. } => "object_stream_truncated",
            Self::Midx(..) => "midx",
            Self::BitmapWalk(..) => "bitmap_walk",
            Self::Offload(..) => "offload",
            Self::NoPromisorRemote { .. } => "no_promisor_remote",
        }
    }
}
