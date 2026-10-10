//! Shared error types for grit-lib.
//!
//! Library code uses [`enum@Error`] (a `thiserror` enum) so callers can match on
//! specific failure modes. The binary wraps these with `anyhow` for human-
//! readable top-level reporting.

use thiserror::Error;

pub use crate::midx_error::MidxError;

/// Why a Git config numeric value was rejected (`git config int` strict parsing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadNumericSource {
    /// Suffix unit letter is invalid or missing where required.
    InvalidUnit,
    /// Parsed value or scaled product does not fit.
    OutOfRange,
}

impl BadNumericSource {
    fn detail(self) -> &'static str {
        match self {
            Self::InvalidUnit => "invalid unit",
            Self::OutOfRange => "out of range",
        }
    }
}

impl std::fmt::Display for BadNumericSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.detail())
    }
}

/// Configuration parse and validation failures.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigError {
    /// A line in a config file could not be parsed.
    #[error("bad config line {line} in {location}")]
    BadConfigLine { location: String, line: usize },

    /// Same as [`Self::BadConfigLine`] with an explicit file path (Git file diagnostics).
    #[error("bad config line {line} in file '{file}'")]
    BadConfigLineInFile { file: String, line: usize },

    /// A config key requires a value but none was given.
    #[error("missing value for '{key}'")]
    MissingValue { key: String },

    /// `fetch.negotiationalgorithm` and similar keys with missing values.
    #[error("bad config variable '{key}' in file '{file}' at line {line}")]
    BadConfigVariable {
        key: String,
        file: String,
        line: usize,
    },

    /// Integer config value could not be parsed or is out of range.
    #[error("bad numeric config value '{value}' for '{key}': {reason}")]
    BadNumericValue {
        key: String,
        value: String,
        reason: BadNumericSource,
    },

    /// Same as [`Self::BadNumericValue`] with file context.
    #[error("bad numeric config value '{value}' for '{key}' in file {file}: {reason}")]
    BadNumericValueInFile {
        key: String,
        value: String,
        file: String,
        reason: BadNumericSource,
    },

    /// `diff.context` (or similar) failed variable validation.
    #[error("bad config variable '{key}' in file '{file}' at line {line}")]
    BadConfigVariableAtLine {
        key: String,
        file: String,
        line: usize,
    },

    /// Command-line config could not supply a parseable value for a key.
    #[error("unable to parse '{key}' from command-line config")]
    UnableToParseFromCommandLine { key: String },

    /// `includeIf.hasconfig:remote.*.url` cannot pull remote URLs from included files.
    #[error(
        "remote URLs cannot be configured in file directly or indirectly included by includeIf.hasconfig:remote.*.url"
    )]
    RemoteUrlInHasconfigInclude,

    /// `[include]` / `[includeIf]` nesting exceeded the Git-compatible depth limit.
    #[error("exceeded maximum include depth (depth {depth} exceeds limit {limit})")]
    IncludeDepthExceeded { depth: usize, limit: usize },

    /// Generic config error with free-form detail (legacy call sites).
    #[error("{0}")]
    Other(String),

    /// The config file could not be written because a `.lock` file is already present.
    #[error("could not lock config file {path}: File exists")]
    ConfigFileLocked { path: String },

    /// A single-value operation was requested but the key has multiple values.
    #[error("cannot overwrite multiple values with a single value for '{key}'")]
    MultipleValues { key: String },

    /// Inline `--comment` text must not span lines.
    #[error("no multi-line comment allowed")]
    MultilineCommentNotAllowed,
}

impl From<String> for ConfigError {
    fn from(value: String) -> Self {
        Self::Other(value)
    }
}

impl From<&str> for ConfigError {
    fn from(value: &str) -> Self {
        Self::Other(value.to_owned())
    }
}

impl ConfigError {
    /// Git stderr body for this error (without a `fatal:` prefix).
    #[must_use]
    pub fn message_body(&self) -> String {
        self.to_string()
    }
}

/// Reference lock failures while creating or updating a ref.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RefLockError {
    /// A non-empty directory occupies the path where the ref file must be created.
    #[error(
        "cannot lock ref '{refname}': there is a non-empty directory '{path}' blocking reference '{refname}'"
    )]
    DirectoryInTheWay { refname: String, path: String },

    /// Another process holds the `packed-refs.lock` file (or it already exists).
    #[error("Unable to create '{lock_path}': File exists.")]
    PackedRefsLockHeld { lock_path: String },
}

/// Clean/smudge filter and EOL conversion failures.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FilterError {
    /// External clean or smudge filter command failed.
    #[error("{path}: {phase} filter '{driver}' failed")]
    ExternalFilterFailed {
        driver: String,
        path: String,
        phase: FilterPhase,
    },

    /// Filter output did not round-trip (clean/smudge mismatch).
    #[error("{path}: {detail}")]
    NotFilteredProperly { path: String, detail: String },

    /// Invalid `working-tree-encoding` attribute value.
    #[error("true/false are no valid working-tree-encodings")]
    InvalidWorkingTreeEncoding,

    /// Line-ending conversion would modify the worktree against user settings.
    #[error("{detail}")]
    LineEndingWouldChange { detail: String },

    /// Shell filter subprocess exited non-zero.
    #[error("filter command exited with status {status}")]
    Failed { status: i32 },
}

/// Which filter hook failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterPhase {
    /// Index → worktree (smudge).
    Smudge,
    /// Worktree → index (clean).
    Clean,
}

impl std::fmt::Display for FilterPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Smudge => f.write_str("smudge"),
            Self::Clean => f.write_str("clean"),
        }
    }
}

/// Sparse-checkout cone pattern parse warnings (non-fatal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SparseCheckoutWarning {
    UnrecognizedPattern { pattern: String },
    UnrecognizedNegativePattern { pattern: String },
    RepeatedPattern { pattern: String },
    DisablingConePatternMatching,
}

impl SparseCheckoutWarning {
    /// Human-readable body without a `warning:` prefix.
    #[must_use]
    pub fn body(&self) -> String {
        match self {
            Self::UnrecognizedPattern { pattern } => {
                format!("unrecognized pattern: '{pattern}'")
            }
            Self::UnrecognizedNegativePattern { pattern } => {
                format!("unrecognized negative pattern: '{pattern}'")
            }
            Self::RepeatedPattern { pattern } => format!(
                "your sparse-checkout file may have issues: pattern '{pattern}' is repeated"
            ),
            Self::DisablingConePatternMatching => "disabling cone pattern matching".to_owned(),
        }
    }

    /// Git-style `warning: …` line.
    #[must_use]
    pub fn format_line(&self) -> String {
        crate::diagnostics::warning_line(&self.body())
    }
}

/// Unified diff / patch application errors.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApplyError {
    /// Patch bytes are malformed at a known input location.
    #[error("corrupt patch at {input}:{line}")]
    CorruptPatch { input: String, line: usize },

    /// Other apply parse failures.
    #[error("{0}")]
    Other(String),
}

/// Path lookup failures while formatting a diff.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DiffPathError {
    #[error("No such path '{path}' in the diff")]
    NoSuchPathInDiff { path: String },
}

/// Path outside the repository during pathspec or transport resolution.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PathOutsideRepoError {
    #[error("{path}: '{subpath}' is outside repository at '{repo_root}'")]
    OutsideRepository {
        path: String,
        subpath: String,
        repo_root: String,
    },
}

/// The top-level error type for all grit-lib operations.
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

    /// `extensions.refStorage` names an unsupported backend.
    #[error("invalid value for 'extensions.refstorage': '{value}'")]
    InvalidRefStorageFormat { value: String },

    /// A supplied object ID string was not valid hex or the wrong length.
    #[error("invalid object id '{0}'")]
    InvalidObjectId(String),

    /// The requested object does not exist in the object store.
    #[error("object not found: {0}")]
    ObjectNotFound(String),

    /// An object's stored data is corrupt or malformed.
    #[error("corrupt object: {0}")]
    CorruptObject(String),

    /// In-pack delta resolution exceeded the read-side chain depth limit (guards
    /// against cyclic ref-delta chains and unbounded recursion; Git has no read limit).
    #[error("delta chain too deep (limit {limit})")]
    DeltaChainTooDeep { limit: usize },

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

    /// Stage-1/2/3 entries are present; a stage-0 tree cannot be built.
    #[error("index contains unmerged entries")]
    IndexUnmerged,

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
    PackedRefsUnexpectedLine { path: String, line: String },

    /// A general path-related error (invalid UTF-8, out-of-bounds, etc.).
    #[error("path error: {0}")]
    PathError(String),

    /// A tree/index path component is forbidden (`.git`, HFS/NTFS aliases, etc.).
    #[error("invalid path '{0}'")]
    InvalidPath(String),

    /// A configuration file parsing or access error.
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// Reference lock failure while updating refs.
    #[error(transparent)]
    RefLock(#[from] RefLockError),

    /// Reference store transaction or iteration failure.
    #[error(transparent)]
    RefStore(#[from] crate::refs::store::RefStoreError),

    /// Filter or EOL conversion failure.
    #[error(transparent)]
    Filter(#[from] FilterError),

    /// Patch application failure.
    #[error(transparent)]
    Apply(#[from] ApplyError),

    /// Diff path selection failure.
    #[error(transparent)]
    DiffPath(#[from] DiffPathError),

    /// Path resolves outside the repository.
    #[error(transparent)]
    PathOutsideRepo(#[from] PathOutsideRepoError),

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

    /// A bundle fetch or verify found prerequisite commits missing from history.
    #[error("repository lacks these prerequisite commits")]
    BundleMissingPrerequisites,

    /// A pathspec did not match any file known to Git (index or HEAD tree).
    #[error("pathspec '{spec}' did not match any file(s) known to git")]
    PathspecNoMatch { spec: String },

    /// User-facing message that should be printed verbatim (no extra prefix).
    ///
    /// Used for revision errors that must match Git's `fatal:` lines exactly.
    #[error("{0}")]
    Message(String),

    /// The index tree matches the parent commit tree and [`allow_empty`](crate::porcelain::commit::CommitRequest::allow_empty) is false.
    #[error("nothing to commit")]
    NothingToCommit,

    /// [`create_commit`](crate::porcelain::commit::create_commit) requires a branch checkout.
    #[error("HEAD is detached")]
    DetachedHead,

    /// [`replay_commit`](crate::porcelain::replay::replay_commit) requires a branch with at least one commit.
    #[error("no commits yet on this branch")]
    UnbornHead,

    /// [`replay_commit`](crate::porcelain::replay::replay_commit) does not replay merge commits
    /// until callers can select a mainline parent explicitly.
    #[error("merge commit {oid}")]
    MergeCommit { oid: crate::objects::ObjectId },

    /// Cherry-pick source is already checked out as `HEAD`.
    #[error("commit already at HEAD")]
    ReplaySourceAtHead,

    /// Stash commit object has fewer parents or an invalid shape for apply/show.
    #[error("corrupt stash commit: {0}")]
    CorruptStash(&'static str),

    /// `stash@{{n}}` is out of range for the current stash reflog.
    #[error("stash entry stash@{{{n}}} not found")]
    StashNotFound { n: usize },

    /// [`create_stash`](crate::porcelain::stash::create_stash) requires at least one commit.
    #[error("cannot stash without a commit")]
    StashNoInitialCommit,

    /// HEAD does not resolve while applying a stash entry.
    #[error("missing HEAD while applying stash")]
    StashMissingHead,

    /// Applying a stash would overwrite locally modified worktree content.
    #[error(
        "your local changes to the following files would be overwritten by stash apply: {paths}"
    )]
    StashWouldOverwriteLocalChanges { paths: String },

    /// The index could not be written after a successful stash apply.
    #[error("writing index after stash apply: {0}")]
    StashIndexWrite(String),

    /// A symlink blob in a stash commit is not valid UTF-8.
    #[error("symlink target is not UTF-8")]
    StashSymlinkNotUtf8,

    /// Multi-pack-index load or write failure.
    #[error(transparent)]
    Midx(#[from] MidxError),

    /// Revision parsing failed ([`crate::rev_parse_error::RevParseError`]).
    #[error(transparent)]
    RevParse(#[from] crate::rev_parse_error::RevParseError),

    /// Revision walking / `rev-list` option parsing failed ([`crate::rev_list_error::RevListError`]).
    #[error(transparent)]
    RevList(#[from] crate::rev_list_error::RevListError),

    /// A Git hook subprocess failed or could not be started.
    #[error(transparent)]
    Hook(#[from] crate::hooks::HookError),

    /// An [`ObjectStore`](crate::odb::store::ObjectStore) backend does not implement the
    /// requested operation (for example filesystem-only helpers on an in-memory store).
    #[error("object store does not support: {operation}")]
    UnsupportedObjectStore { operation: &'static str },
}

impl Error {
    /// When set, the CLI should prefix stderr with `fatal: ` for Git-compatible commands.
    #[must_use]
    pub fn wants_fatal_prefix(&self) -> bool {
        matches!(
            self,
            Error::Config(_)
                | Error::RefLock(_)
                | Error::Filter(_)
                | Error::PathOutsideRepo(_)
                | Error::DiffPath(_)
        )
    }

    /// Format for Git-style stderr (adds `fatal:` / `error:` when appropriate).
    #[must_use]
    pub fn git_stderr_message(&self) -> String {
        match self {
            Error::Apply(ApplyError::CorruptPatch { .. }) => {
                crate::diagnostics::error_line(&self.to_string())
            }
            Error::Message(s) => s.clone(),
            e if e.wants_fatal_prefix() => crate::diagnostics::fatal_line(&e.to_string()),
            other => other.to_string(),
        }
    }
}

/// Convenience alias for `Result<T, Error>`.
pub type Result<T> = std::result::Result<T, Error>;
