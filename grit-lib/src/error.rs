//! Shared error types for the Gust library.
//!
//! Library code uses [`Error`] (a `thiserror` enum) so callers can match on
//! specific failure modes. The binary wraps these with `anyhow` for human-
//! readable top-level reporting.

use thiserror::Error;

/// The top-level error type for all Gust library operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// A repository could not be found or is structurally invalid.
    #[error("not a git repository (or any of the parent directories): {0}")]
    NotARepository(String),

    /// A bare repository was found but access is forbidden by safe.bareRepository.
    #[error("cannot use bare repository '{0}' (safe.bareRepository is 'explicit')")]
    ForbiddenBareRepository(String),

    /// The repository is owned by a different user (safe.directory).
    #[error("detected dubious ownership in repository at '{0}'")]
    DubiousOwnership(String),

    /// Repository format version is not supported by this implementation.
    #[error("unsupported repository format version '{0}'")]
    UnsupportedRepositoryFormatVersion(u32),

    /// Repository declares an unsupported extension.
    #[error("unknown repository extension '{0}'")]
    UnsupportedRepositoryExtension(String),

    /// A supplied object ID string was not valid hex or the wrong length.
    #[error("invalid object id '{0}'")]
    InvalidObjectId(String),

    /// The requested object does not exist in the object store.
    #[error("object not found: {0}")]
    ObjectNotFound(String),

    /// An object's stored data is corrupt or malformed.
    #[error("corrupt object: {0}")]
    CorruptObject(String),

    /// An unsupported or unknown object type was encountered.
    #[error("unknown object type '{0}'")]
    UnknownObjectType(String),

    /// Loose object header type field exceeds Git's 32-byte limit.
    #[error("header for {oid} too long, exceeds 32 bytes")]
    ObjectHeaderTooLong { oid: String },

    /// An I/O error from the underlying filesystem.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A zlib compression or decompression failure.
    #[error("zlib error: {0}")]
    Zlib(String),

    /// Loose object bytes hash to a different OID than the file path implies (`git fsck` / `read_loose_object`).
    #[error("{real_oid}: hash-path mismatch, found at: {path}")]
    LooseHashMismatch {
        /// Repository-relative or filesystem path to the loose object file.
        path: String,
        /// Hex object id of the unpacked contents.
        real_oid: String,
    },

    /// The index file is missing, truncated, or has a bad header.
    #[error("index error: {0}")]
    IndexError(String),

    /// The cache-tree extension references more entries than the index contains. Git emits this
    /// (verbatim, prefixed with `error: `) when a tree with duplicate path entries is read into the
    /// index (`t4058-diff-duplicates`).
    #[error("corrupted cache-tree has entries not present in index")]
    CacheTreeCorrupt,

    /// A reference name or value is invalid.
    #[error("invalid ref: {0}")]
    InvalidRef(String),

    /// A general path-related error (invalid UTF-8, out-of-bounds, etc.).
    #[error("path error: {0}")]
    PathError(String),

    /// A tree/index path component is forbidden (`.git`, HFS/NTFS aliases, etc.).
    #[error("invalid path '{0}'")]
    InvalidPath(String),

    /// A configuration file parsing or access error.
    #[error("config error: {0}")]
    ConfigError(String),

    /// A commit/tag signing or signature-verification error.
    #[error("{0}")]
    Signing(String),

    /// HTTP authentication failed: the server required credentials (`401`) and
    /// either no credential provider was wired, the provider could not supply a
    /// usable username/password, the server demanded an unsupported auth scheme,
    /// or the supplied credentials were rejected.
    ///
    /// Distinct from [`Error::Message`] so embedders can detect an authentication
    /// failure (and e.g. fall back to an interactive/subprocess path) rather than
    /// string-matching, and so the failure surfaces typed instead of hanging.
    #[error("authentication failed: {0}")]
    Auth(String),

    /// A push carried `--push-option` values but the remote `git-receive-pack`
    /// did not advertise the `push-options` capability, so the options cannot be
    /// transmitted.
    ///
    /// Distinct from [`Error::Message`] so embedders can detect this specific
    /// negotiation failure (and e.g. fall back to a subprocess push) rather than
    /// string-matching. The message matches Git's
    /// `fatal: the receiving end does not support push options`.
    #[error("the receiving end does not support push options")]
    PushOptionsUnsupported,

    /// User-facing message that should be printed verbatim (no extra prefix).
    ///
    /// Used for revision errors that must match Git's `fatal:` lines exactly.
    #[error("{0}")]
    Message(String),
}

/// Convenience alias for `Result<T, Error>`.
pub type Result<T> = std::result::Result<T, Error>;
