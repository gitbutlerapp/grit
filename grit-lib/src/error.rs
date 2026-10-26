//! Shared error types for grit-lib.
//!
//! Library code uses [`enum@Error`] (a `thiserror` enum) so callers can match on
//! specific failure modes. The binary wraps these with `anyhow` for human-
//! readable top-level reporting.
//!
//! Library guide: [errors](https://grit-scm.com/docs/library/embedding)
//!
//! ```rust
//! use grit_lib::error::Error;
//! # fn main() {
//! let _ = Error::Message("example".into());
//! # }
//! ```

use thiserror::Error;

pub use crate::git_dir::GitDirPathOperation;
pub use crate::midx_error::MidxError;
pub use crate::worktree::io_error::WorktreePathOperation;

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

/// Why a tree transition would overwrite existing worktree content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OverwriteReason {
    /// Staged or unstaged edits on paths that differ between trees.
    LocalChanges,
    /// Untracked paths that a checkout would replace.
    Untracked,
}

impl std::fmt::Display for OverwriteReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LocalChanges => f.write_str("local_changes"),
            Self::Untracked => f.write_str("untracked"),
        }
    }
}

/// Why a remote could not be read or written ([`Error::RemoteUnreadable`]).
///
/// Every reason is a failure to talk to the remote at all, as opposed to the remote refusing
/// an update. [`Self::Transport`] is the one usually worth retrying on a flaky network.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RemoteUnreadableReason {
    /// A local path or `file://` URL does not name a Git directory (valid `HEAD`, plus
    /// `objects/` and `refs/` in the common directory).
    RepositoryNotFound,
    /// The connection failed or broke: name resolution, refused or reset connection, TLS, or
    /// timeouts.
    Transport {
        /// Transport-level description of the failure, with URL credentials redacted.
        detail: String,
    },
    /// A smart-HTTP server answered with an error status other than an authentication
    /// challenge (those are [`Error::Auth`]).
    HttpStatus {
        /// HTTP status code (`>= 400`).
        status: u16,
    },
    /// An HTTP response looked successful but was not a Git smart-HTTP ref advertisement
    /// (for example `200 text/html` from a parked or mistyped host).
    NotGitServer {
        /// HTTP status code (often `200`).
        status: u16,
        /// Response `Content-Type` when present (media type only, without parameters).
        content_type: Option<String>,
    },
    /// The transport ran, but the remote never completed the Git protocol handshake (for
    /// example `ssh` exited, or a `git://` peer closed, before any ref advertisement).
    NoHandshake,
}

impl RemoteUnreadableReason {
    /// Stable snake-case name for machine-readable output (`repository_not_found`,
    /// `transport`, `http_status`, `not_git_server`, `no_handshake`).
    #[must_use]
    pub fn slug(&self) -> &'static str {
        match self {
            Self::RepositoryNotFound => "repository_not_found",
            Self::Transport { .. } => "transport",
            Self::HttpStatus { .. } => "http_status",
            Self::NotGitServer { .. } => "not_git_server",
            Self::NoHandshake => "no_handshake",
        }
    }
}

fn remote_unreadable_message(url: Option<&str>, reason: &RemoteUnreadableReason) -> String {
    let target = url.map_or_else(|| "the remote".to_owned(), |u| format!("'{u}'"));
    match reason {
        RemoteUnreadableReason::RepositoryNotFound => {
            format!("could not find repository at {target}")
        }
        RemoteUnreadableReason::Transport { detail } => {
            format!("could not reach {target}: {detail}")
        }
        RemoteUnreadableReason::HttpStatus { status } => match url {
            Some(u) => format!("HTTP {status} from {u}"),
            None => format!("HTTP {status} from the remote"),
        },
        RemoteUnreadableReason::NotGitServer {
            status,
            content_type,
        } => {
            let got = match content_type
                .as_deref()
                .map(|ct| ct.split(';').next().unwrap_or(ct).trim())
                .filter(|ct| !ct.is_empty())
            {
                Some(ct) => format!("{status} {ct}"),
                None => status.to_string(),
            };
            format!("{target} did not answer like a Git server (got {got})")
        }
        RemoteUnreadableReason::NoHandshake => {
            format!("could not read from {target}: no Git protocol response")
        }
    }
}

/// Why an HTTP remote refused access ([`Error::Auth`]).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum AuthFailure {
    /// The server asked for credentials (`401`) and no credential provider is wired.
    #[error("no credential provider is configured")]
    NoCredentialProvider,
    /// The credential provider could not supply credentials (no helper has them, and
    /// prompting is not possible).
    #[error("{detail}")]
    CredentialsUnavailable {
        /// The provider's explanation (for example that no helper has credentials and
        /// prompting is disabled).
        detail: String,
    },
    /// The provider answered without a usable username and password.
    #[error("the credential helper returned no usable username and password")]
    UnusableCredentials,
    /// The URL could not be split into the credential fields helpers expect.
    #[error("the URL cannot be described to a credential helper")]
    UrlNotDescribable,
    /// The server only offers authentication schemes grit does not speak (no `Basic`).
    #[error("unsupported authentication scheme: {}", schemes.join(", "))]
    UnsupportedScheme {
        /// The `WWW-Authenticate` challenges the server sent.
        schemes: Vec<String>,
    },
    /// Credentials were sent and the server rejected them (`401` again).
    #[error("the server rejected the credentials")]
    Rejected,
}

impl AuthFailure {
    /// Whether credentials were sent and refused, as opposed to never being available.
    #[must_use]
    pub fn is_rejected(&self) -> bool {
        matches!(self, Self::Rejected)
    }
}

/// The command a dirty worktree blocked ([`Error::DirtyWorktree`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WorktreeOperation {
    /// Switching branches or checking out a commit.
    Switch,
    /// Picking (cherry-picking) or reverting commits.
    Pick,
    /// Merging.
    Merge,
    /// Rebasing with `--exec`.
    RebaseExec,
    /// Starting a rebase that stops on conflicts (the stop rewrites the index and worktree).
    Rebase,
}

impl std::fmt::Display for WorktreeOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Switch => "switching",
            Self::Pick => "picking",
            Self::Merge => "merging",
            Self::RebaseExec => "rebasing with --exec",
            Self::Rebase => "rebasing",
        })
    }
}

/// Configuration parse and validation failures.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigError {
    /// A line in a config file could not be parsed.
    #[error("bad config line {line} in {location}")]
    BadConfigLine {
        /// Config source label or path where the error was detected.
        location: String,
        /// One-based line number within that source.
        line: usize,
    },

    /// Same as [`Self::BadConfigLine`] with an explicit file path (Git file diagnostics).
    #[error("bad config line {line} in file '{file}'")]
    BadConfigLineInFile {
        /// Path to the config file involved in the error.
        file: String,
        /// One-based line number within that file.
        line: usize,
    },

    /// A config key requires a value but none was given.
    #[error("missing value for '{key}'")]
    MissingValue {
        /// Config key name that failed validation.
        key: String,
    },

    /// `fetch.negotiationalgorithm` and similar keys with missing values.
    #[error("bad config variable '{key}' in file '{file}' at line {line}")]
    BadConfigVariable {
        /// Config key name that failed validation.
        key: String,
        /// Path to the config file involved in the error.
        file: String,
        /// One-based line number within the config source.
        line: usize,
    },

    /// `unset` or a scoped read/write was requested for a key that is not set in that file.
    #[error("key '{key}' is not set")]
    KeyNotSet {
        /// Config key that was not present.
        key: String,
    },

    /// Integer config value could not be parsed or is out of range.
    #[error("bad numeric config value '{value}' for '{key}': {reason}")]
    BadNumericValue {
        /// Config key name that failed validation.
        key: String,
        /// Raw config value that could not be parsed.
        value: String,
        /// Why the value or operation was rejected.
        reason: BadNumericSource,
    },

    /// Same as [`Self::BadNumericValue`] with file context.
    #[error("bad numeric config value '{value}' for '{key}' in file {file}: {reason}")]
    BadNumericValueInFile {
        /// Config key name that failed validation.
        key: String,
        /// Raw config value that could not be parsed.
        value: String,
        /// Path to the config file involved in the error.
        file: String,
        /// Why the value or operation was rejected.
        reason: BadNumericSource,
    },

    /// `diff.context` (or similar) failed variable validation.
    #[error("bad config variable '{key}' in file '{file}' at line {line}")]
    BadConfigVariableAtLine {
        /// Config key name that failed validation.
        key: String,
        /// Path to the config file involved in the error.
        file: String,
        /// One-based line number within the config source.
        line: usize,
    },

    /// Command-line config could not supply a parseable value for a key.
    #[error("unable to parse '{key}' from command-line config")]
    UnableToParseFromCommandLine {
        /// Config key name that failed validation.
        key: String,
    },

    /// `includeIf.hasconfig:remote.*.url` cannot pull remote URLs from included files.
    #[error(
        "remote URLs cannot be configured in file directly or indirectly included by includeIf.hasconfig:remote.*.url"
    )]
    RemoteUrlInHasconfigInclude,

    /// `[include]` / `[includeIf]` nesting exceeded the Git-compatible depth limit.
    #[error("exceeded maximum include depth (depth {depth} exceeds limit {limit})")]
    IncludeDepthExceeded {
        /// Current include nesting depth when the limit was hit.
        depth: usize,
        /// Maximum allowed include nesting depth.
        limit: usize,
    },

    /// Generic config error with free-form detail (legacy call sites).
    #[error("{0}")]
    Other(String),

    /// The config file could not be written because a `.lock` file is already present.
    #[error("could not lock config file {path}: File exists")]
    ConfigFileLocked {
        /// Path to the config file that could not be locked.
        path: String,
    },

    /// A single-value operation was requested but the key has multiple values.
    #[error("cannot overwrite multiple values with a single value for '{key}'")]
    MultipleValues {
        /// Config key name that failed validation.
        key: String,
    },

    /// Per-worktree config was requested but `extensions.worktreeConfig` is off.
    #[error(
        "worktree config is only allowed when extensions.worktreeConfig is enabled in the common config"
    )]
    WorktreeConfigNotEnabled,

    /// `remove-section` or `rename-section` did not find the named section.
    #[error("section '{section}' not found")]
    SectionNotFound {
        /// Section header name (e.g. `remote.origin`).
        section: String,
    },

    /// Inline `--comment` text must not span lines.
    #[error("no multi-line comment allowed")]
    MultilineCommentNotAllowed,

    /// `status.showUntrackedFiles` must be `no`, `normal`, `all`, or a Git boolean synonym.
    #[error(
        "invalid value '{value}' for 'status.showUntrackedFiles' (expected no, normal, all, boolean words, or integer 0 vs nonzero)"
    )]
    InvalidUntrackedFilesMode {
        /// Raw config value that could not be parsed.
        value: String,
    },
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
    DirectoryInTheWay {
        /// Full ref name (for example `refs/heads/main`).
        refname: String,
        /// Directory path blocking creation of the ref file.
        path: String,
    },

    /// Another process holds the `packed-refs.lock` file (or it already exists).
    #[error("Unable to create '{lock_path}': File exists.")]
    PackedRefsLockHeld {
        /// Path to the lock file that could not be created.
        lock_path: String,
    },
}

/// Clean/smudge filter and EOL conversion failures.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FilterError {
    /// External clean or smudge filter command failed.
    #[error("{path}: {phase} filter '{driver}' failed")]
    ExternalFilterFailed {
        /// Filter driver name from `.gitattributes`.
        driver: String,
        /// Repository-relative or filesystem path related to the failure.
        path: String,
        /// Whether the clean or smudge filter hook failed.
        phase: FilterPhase,
    },

    /// A required clean or smudge filter could not be started (for example, no shell to run
    /// it), so the content was neither filtered nor stored unfiltered.
    #[error("{path}: cannot start {phase} filter '{driver}' ({program}): {detail}")]
    ExternalFilterSpawnFailed {
        /// Filter driver name from `.gitattributes`.
        driver: String,
        /// Repository-relative path being filtered.
        path: String,
        /// Whether the clean or smudge filter was being started.
        phase: FilterPhase,
        /// Program grit tried to spawn (`sh` for shell-command filters).
        program: String,
        /// OS error kind from the failed spawn.
        kind: std::io::ErrorKind,
        /// OS error message from the failed spawn.
        detail: String,
    },

    /// Filter output did not round-trip (clean/smudge mismatch).
    #[error("{path}: {detail}")]
    NotFilteredProperly {
        /// Repository-relative or filesystem path related to the failure.
        path: String,
        /// Why clean/smudge output did not round-trip.
        detail: String,
    },

    /// Invalid `working-tree-encoding` attribute value.
    #[error("true/false are no valid working-tree-encodings")]
    InvalidWorkingTreeEncoding,

    /// Line-ending conversion would modify the worktree against user settings.
    #[error("{detail}")]
    LineEndingWouldChange {
        /// Additional human-readable detail for the failure.
        detail: String,
    },

    /// Shell filter subprocess exited non-zero.
    #[error("filter command exited with status {status}")]
    Failed {
        /// Subprocess exit status from the filter command.
        status: i32,
    },
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
    /// A cone pattern line was not recognized.
    UnrecognizedPattern {
        /// Raw pattern text from the sparse-checkout file.
        pattern: String,
    },
    /// A negative cone pattern line was not recognized.
    UnrecognizedNegativePattern {
        /// Raw negative pattern text from the sparse-checkout file.
        pattern: String,
    },
    /// The same cone pattern appears more than once in the sparse-checkout file.
    RepeatedPattern {
        /// Duplicated pattern text.
        pattern: String,
    },
    /// Cone pattern matching was disabled due to invalid sparse-checkout contents.
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
    CorruptPatch {
        /// Patch input label (file name or `stdin`).
        input: String,
        /// One-based line number within the patch input.
        line: usize,
    },

    /// Preimage context could not be located in the blob at any search offset.
    #[error(
        "patch failed: context mismatch at hunk {hunk_index} (expected near line {expected_line})"
    )]
    ContextMismatch {
        /// Zero-based index of the hunk in the input slice.
        hunk_index: usize,
        /// One-based line number from the hunk header (`old_start`).
        expected_line: usize,
    },

    /// A later hunk would modify lines already changed by an earlier hunk.
    #[error("patch failed: overlapping hunks at hunk {hunk_index}")]
    HunksOverlap {
        /// Zero-based index of the hunk that overlaps prior applied changes.
        hunk_index: usize,
    },

    /// The patch removes more lines than exist in the blob (without a valid EOF match).
    #[error("patch failed: unexpected end of file at hunk {hunk_index}")]
    UnexpectedEof {
        /// Zero-based index of the offending hunk.
        hunk_index: usize,
    },

    /// Other apply parse failures.
    #[error("{0}")]
    Other(String),

    /// A text hunk could not be matched or applied.
    #[error("patch failed: {path}:{line}")]
    PatchFailed {
        /// File path from the patch.
        path: String,
        /// One-based line number in the patch input.
        line: usize,
    },

    /// Index or blob apply failed for a path.
    #[error("could not apply patch to {path}: {message}")]
    Failed {
        /// Repository-relative path.
        path: String,
        /// Failure detail.
        message: String,
    },

    /// `--cached` apply requires a stage-0 index entry at the preimage path.
    #[error("'{path}' is not in the index")]
    NotInIndex {
        /// Missing path.
        path: String,
    },

    /// A binary diff marker without `GIT binary patch` payload cannot be applied.
    #[error("binary patch data missing for '{path}'")]
    BinaryDataMissing {
        /// Path from the patch.
        path: String,
    },
}

/// Path lookup failures while formatting a diff.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DiffPathError {
    /// The requested path is not part of the diff being formatted.
    #[error("No such path '{path}' in the diff")]
    NoSuchPathInDiff {
        /// Path that was requested from the diff.
        path: String,
    },
}

/// `--pathspec-from-file` / `--pathspec-file-nul` parse failure.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PathspecFromFileParseError {
    /// A NUL-separated entry is not valid UTF-8.
    #[error("invalid UTF-8 in pathspec-from-file (NUL mode)")]
    InvalidUtf8InNulRecord,
    /// A NUL-terminated entry used C-style quoting (forbidden in NUL mode).
    #[error("pathspec-from-file: line is not NUL terminated: {line}")]
    QuotedLineInNulMode {
        /// Offending entry text.
        line: String,
    },
    /// A line-mode entry had invalid double-quote wrapping.
    #[error("invalid C-style quoting: {line}")]
    InvalidCStyleQuoting {
        /// Raw line from the file.
        line: String,
    },
    /// Backslash escape at end of a quoted line.
    #[error("invalid escape at end of string")]
    InvalidEscapeAtEnd,
    /// Octal escape shorter than three digits.
    #[error("truncated octal escape")]
    TruncatedOctalEscape,
    /// Octal escape bytes are not valid UTF-8.
    #[error("invalid octal bytes")]
    InvalidOctalBytes,
    /// Octal digits do not form a valid byte value.
    #[error("invalid octal escape value")]
    InvalidOctalValue,
    /// Unknown escape after backslash in a quoted line.
    #[error("invalid escape sequence \\{escape}")]
    InvalidEscapeSequence {
        /// Character after the backslash.
        escape: char,
    },
    /// Quoted line decoded to invalid UTF-8.
    #[error("invalid UTF-8 in quoted pathspec")]
    InvalidUtf8InQuotedLine,
}

/// Failure while resolving argv and `--pathspec-from-file` pathspecs against a work tree.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PathspecResolveError {
    /// An argv pathspec was the empty string.
    #[error("invalid path ''")]
    EmptyPathspec,
    /// Resolved path lies outside the repository work tree.
    #[error("{elt}: '{path}' is outside repository at '{work_tree}'")]
    OutsideRepository {
        /// User-facing pathspec token (argv element).
        elt: String,
        /// Resolved absolute path outside the work tree.
        path: String,
        /// Canonical work tree root.
        work_tree: String,
    },
    /// `--pathspec-from-file` bytes could not be parsed.
    #[error(transparent)]
    FromFile(#[from] PathspecFromFileParseError),
    /// Command requires a working tree but the repository is bare.
    #[error("this operation needs a working tree")]
    NeedsWorkTree,
}

/// Why a `:(attr:...)` pathspec clause was rejected.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AttrPathspecError {
    /// The clause names no attribute requirement (`:(attr:)`).
    #[error("attr requirement list is empty")]
    Empty,
    /// More than one `attr:` clause appears in one pathspec.
    #[error("a pathspec may contain at most one attr clause")]
    MultipleAttrClauses,
    /// A requirement names an attribute that is not valid (`-a=b`, `bad name`, `bad/name`).
    #[error("'{0}' is not a usable attribute name")]
    InvalidName(String),
    /// A `\` escape ends the value or precedes whitespace.
    #[error("attr value ends with a dangling escape")]
    DanglingEscape,
    /// A `\` escape precedes something other than `,`.
    #[error("attr values may only escape ','")]
    UnsupportedEscape,
}

/// Path outside the repository during pathspec or transport resolution.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PathOutsideRepoError {
    /// A resolved path escapes the repository work tree or git directory.
    #[error("{path}: '{subpath}' is outside repository at '{repo_root}'")]
    OutsideRepository {
        /// Repository-relative or filesystem path related to the failure.
        path: String,
        /// Path component that lies outside the repository root.
        subpath: String,
        /// Canonical repository root used for the outside-path check.
        repo_root: String,
    },
}

fn maintenance_running_message(holder: &crate::maintenance::GcLockHolder) -> String {
    match (holder.pid, holder.host.as_deref()) {
        (Some(pid), Some(host)) => {
            format!("repository maintenance is already running (process {pid} on {host})")
        }
        (Some(pid), None) => format!("repository maintenance is already running (process {pid})"),
        _ => "repository maintenance is already running".to_owned(),
    }
}

fn restore_unmerged_message(paths: &[String]) -> String {
    match paths {
        [path] => format!("path '{path}' is unmerged"),
        _ => format!(
            "paths are unmerged: {}",
            paths
                .iter()
                .map(|p| format!("'{p}'"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// The top-level error type for all grit-lib operations.
///
/// # Examples
///
/// ```
/// use grit_lib::error::Error;
///
/// let err = Error::Message("repository not found".into());
/// assert!(err.to_string().contains("repository not found"));
/// ```
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
    InvalidRefStorageFormat {
        /// Raw `extensions.refStorage` value from the repository config.
        value: String,
    },

    /// [`crate::repo::Repository::init_with_outcome`] refused to change object format on reinit.
    #[error("cannot change object format in an existing repository")]
    InitObjectFormatChange,

    /// [`crate::repo::Repository::init_with_outcome`] with incompatible `--bare` and separate git dir.
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

    /// An object's stored data is corrupt or malformed.
    #[error("corrupt object: {0}")]
    CorruptObject(String),

    /// A pack index (`.idx`) file's trailing checksum does not match its contents.
    ///
    /// Only reported by reads that verify the index trailer, such as
    /// [`crate::pack::read_pack_index`]; the checksum of the `.pack` the index
    /// describes is checked separately.
    #[error("corrupt object: index checksum mismatch for {}: {mismatch}", .path.display())]
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
    /// process finishes, or delete `path` when no process owns it (a `holder` with
    /// `running: false` means the lock is stale).
    #[error("lock file {} already exists", path.display())]
    LockHeld {
        /// The lock file that could not be created.
        path: std::path::PathBuf,
        /// Owner recorded in the lock's pid sidecar (`core.lockfilePid`), when one was written.
        holder: Option<crate::lockfile::LockHolder>,
    },

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

    /// Explicit [`crate::porcelain::add::stage`] refused paths that still contain conflict markers.
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

    /// Branch create, delete, or upstream configuration failure.
    #[error(transparent)]
    Branch(#[from] crate::porcelain::branch_error::BranchError),

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
    #[error("authentication failed for {url}: {reason}")]
    Auth {
        /// Repository URL with credentials redacted and smart-HTTP endpoint suffixes
        /// (`/info/refs?service=…`, `/git-upload-pack`, `/git-receive-pack`) removed.
        url: String,
        /// What went wrong.
        reason: AuthFailure,
    },

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
        path.display()
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

    /// [`crate::porcelain::commit::CommitScope::Paths`] was used while a merge is in progress.
    #[error("cannot do a partial commit during a merge")]
    PartialCommitDuringMerge,

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

    /// A commit was requested with an empty message body after normalization.
    #[error("empty commit message")]
    EmptyCommitMessage,

    /// [`crate::porcelain::paths::remove_paths`] was called with no pathspecs.
    #[error("you must specify path(s) to remove")]
    RemovePathsRequired,

    /// [`crate::porcelain::paths::move_path`] / [`crate::porcelain::paths::move_paths`] source is not tracked.
    #[error("not under version control, source={src}")]
    MoveSourceNotTracked {
        /// Untracked or missing source path.
        src: String,
    },

    /// Move refused because the destination already exists without `--force`.
    #[error("destination exists, source={src}, destination={destination}")]
    MoveDestinationExists {
        /// Source path in the move request.
        src: String,
        /// Existing destination path.
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
    #[error("cannot move into itself, source={src}, destination={destination}")]
    MoveIntoSelf {
        /// Source path.
        src: String,
        /// Destination equal to the source.
        destination: String,
    },

    /// Move refused because the source has no index entries.
    #[error("nothing to move, source={src}")]
    MoveNothingToMove {
        /// Source path with no index entries.
        src: String,
    },

    /// Multi-source move would write more than one source to the same destination path.
    #[error("multiple sources for the same target, destination={destination}")]
    MoveDuplicateDestination {
        /// Destination path that would receive more than one source.
        destination: String,
    },

    /// A worktree source path is missing or not a file, directory, or symlink.
    #[error("bad source, source={src}, destination={destination}")]
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
    /// Use [`Error::is_remote_unreadable`] to test for it through wrapper errors.
    ///
    /// Not used for cancellation; see [`Error::is_cancelled`].
    #[error("{}", remote_unreadable_message(url.as_deref(), reason))]
    RemoteUnreadable {
        /// Remote URL or resolved local path, with credentials redacted. `None` when the
        /// failing layer only had a connection, not the URL it was opened from.
        url: Option<String>,
        /// What went wrong.
        reason: RemoteUnreadableReason,
    },

    /// Explicit fetch refspec resolution failed (missing or ambiguous source).
    #[error("{0}")]
    FetchRefspec(#[from] crate::fetch_request::FetchRefspecError),

    /// Fetch refused to overwrite a local branch with a non-fast-forward update.
    #[error("non-fast-forward update for {local_ref}")]
    FetchNonFastForward {
        /// Local ref that would have been overwritten.
        local_ref: String,
    },

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

    /// Author or committer on [`crate::porcelain::commit::CommitRequest`] is invalid.
    #[error(transparent)]
    InvalidIdentity(#[from] crate::ident::CommitIdentityError),

    /// A long-running library operation stopped at a cancellation checkpoint.
    ///
    /// Use [`Error::is_cancelled`] to test for it through wrapper errors
    /// ([`crate::remote::RemoteError::is_cancelled`], [`crate::remote::clone::CloneError::is_cancelled`], …).
    #[error("operation cancelled")]
    Cancelled,

    /// No usable author or committer identity is configured
    /// ([`crate::repo::Repository::identity`]).
    #[error(transparent)]
    Identity(#[from] crate::config::ident_resolve::IdentityError),

    /// User-facing message that should be printed verbatim (no extra prefix).
    ///
    /// Used for revision errors that must match Git's `fatal:` lines exactly.
    #[error("{0}")]
    Message(String),

    /// A merge was requested with fast-forward only, but the histories diverged.
    #[error("not possible to fast-forward; refusing to merge")]
    NotFastForward,

    /// The index tree matches the parent commit tree and [`allow_empty`](crate::porcelain::commit::CommitRequest::allow_empty) is false.
    #[error("nothing to commit")]
    NothingToCommit,

    /// No staged changes are available for a [`CommitScope::Staged`](crate::porcelain::commit::CommitScope::Staged)
    /// commit, but the work tree has unstaged or untracked paths.
    #[error("nothing staged to commit")]
    NothingStagedToCommit {
        /// Tracked paths that differ between the index and the work tree.
        unstaged: usize,
        /// Untracked paths under the work tree.
        untracked: usize,
    },

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

    /// [`replay_commit`](crate::porcelain::replay::replay_commit) does not replay merge commits
    /// until callers can select a mainline parent explicitly.
    #[error("merge commit {oid}")]
    MergeCommit {
        /// Object id of the merge commit that cannot be replayed yet.
        oid: crate::objects::ObjectId,
    },

    /// Cherry-pick source is already checked out as `HEAD`.
    #[error("commit already at HEAD")]
    ReplaySourceAtHead,

    /// Sequencer todo lists a command grit does not implement yet.
    #[error("unknown sequencer command '{command}'")]
    SequencerUnknownCommand {
        /// Verb read from `.git/sequencer/todo`.
        command: String,
    },

    /// [`abort_replay_sequence`](crate::porcelain::replay_sequence::abort_replay_sequence) refuses when `HEAD` moved since the replay started.
    #[error("cannot abort replay: HEAD moved since replay started")]
    ReplayAbortHeadMoved,

    /// [`continue_replay_sequence`](crate::porcelain::replay_sequence::continue_replay_sequence) refuses while unmerged index entries remain.
    #[error("cannot continue replay: unmerged index entries remain")]
    ReplayContinueUnmerged,

    /// No cherry-pick or revert sequence is in progress.
    #[error("no cherry-pick or revert in progress")]
    NoReplayInProgress,

    /// The command would move `HEAD` while a paused operation still depends on it
    /// (see [`operation_blocking_switch`](crate::state::operation_blocking_switch)).
    #[error("{} in progress", .0.description())]
    OperationInProgress(crate::state::InProgressOperation),

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

    /// [`delete_branch`](crate::porcelain::branch::delete_branch) refuses to delete the branch
    /// `HEAD` points at.
    #[error("cannot delete '{branch}' — it is the current branch")]
    CannotDeleteCurrentBranch {
        /// Short branch name (e.g. `main`).
        branch: String,
    },

    /// [`delete_branch`](crate::porcelain::branch::delete_branch) without `force` found commits
    /// on the branch that are not reachable from its upstream (or from `HEAD` when it has none).
    #[error("the branch '{branch}' is not fully merged")]
    BranchNotMerged {
        /// Short branch name (e.g. `topic`).
        branch: String,
    },

    /// `refs/tags/<tag>` does not exist.
    #[error("no tag named '{tag}'")]
    TagNotFound {
        /// Short tag name (e.g. `v1.0`).
        tag: String,
    },

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

    /// Ref does not exist and has no reflog entries.
    #[error("unknown ref '{refname}'")]
    ReflogUnknownRef {
        /// User-supplied or DWIM ref name.
        refname: String,
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

    /// [`crate::repo_handles::RepositoryStash::apply`] with `restore_index` could
    /// not merge the current index with the stashed index parent.
    #[error("conflicts in index while applying stash with --index")]
    StashIndexConflict,

    /// Incompatible [`StashPushOptions`](crate::porcelain::stash::StashPushOptions) flags.
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

    /// [`CommitRequest::amend`](crate::porcelain::commit::CommitRequest::amend) was set on an unborn branch.
    #[error("cannot amend: no commits on this branch yet")]
    AmendUnborn,

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
    #[error("unfinished notes merge: conflict files remain in {}", path.display())]
    NotesMergeInProgress {
        /// The notes merge worktree directory.
        path: std::path::PathBuf,
    },

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

    /// History rewrite planning failed ([`crate::history::HistoryError`]).
    #[error(transparent)]
    History(#[from] crate::history::HistoryError),

    /// The operation needs the files-backed `objects/` directory, but the repository's
    /// [`Odb`](crate::odb::Odb) uses a custom [`OdbBuilder::primary`](crate::odb::OdbBuilder::primary)
    /// store. Nothing was written; use a files-backed repository for this operation.
    #[error("{operation} requires the files-backed object store; it is not supported with a custom primary")]
    UnsupportedObjectStore {
        /// Operation that was refused (for example `"fetch"`, `"push"`, `"gc"`).
        operation: &'static str,
    },

    /// [`RepositoryBuilder::open`](crate::repo::RepositoryBuilder::open) was given an
    /// [`OdbBuilder`](crate::odb::OdbBuilder) rooted somewhere other than the repository's
    /// `objects/` directory.
    #[error(
        "object database {} is not the repository's objects directory {}",
        builder.display(),
        repository.display()
    )]
    OdbObjectsDirMismatch {
        /// The builder's objects directory, canonicalized when it exists.
        builder: std::path::PathBuf,
        /// The repository's canonical objects directory.
        repository: std::path::PathBuf,
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

impl Error {
    /// Build [`Error::DirtyWorktree`], sorting and deduplicating `paths`.
    #[must_use]
    pub fn dirty_worktree(operation: WorktreeOperation, mut paths: Vec<String>) -> Self {
        paths.sort();
        paths.dedup();
        Self::DirtyWorktree { operation, paths }
    }

    /// Build [`Error::WouldOverwrite`] for switch and similar tree transitions.
    #[must_use]
    pub fn would_overwrite(reason: OverwriteReason, mut paths: Vec<String>) -> Self {
        paths.sort();
        paths.dedup();
        let message = match reason {
            OverwriteReason::LocalChanges => {
                "you have uncommitted changes — commit them before switching".into()
            }
            OverwriteReason::Untracked => {
                let first = paths.first().map(String::as_str).unwrap_or("<path>");
                format!("untracked file '{first}' would be overwritten — move or remove it first")
            }
        };
        Self::WouldOverwrite {
            reason,
            paths,
            message,
        }
    }

    /// Build [`Error::RemoteUnreadable`] for `url`.
    #[must_use]
    pub fn remote_unreadable(url: impl Into<String>, reason: RemoteUnreadableReason) -> Self {
        Self::RemoteUnreadable {
            url: Some(url.into()),
            reason,
        }
    }

    /// Whether this is [`Error::RemoteUnreadable`]: the remote could not be read or written
    /// at all (as opposed to a rejected update or an empty remote).
    #[must_use]
    pub fn is_remote_unreadable(&self) -> bool {
        matches!(self, Self::RemoteUnreadable { .. })
    }

    /// Whether this is [`Error::Cancelled`] or a nested history failure that was cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        match self {
            Self::Cancelled => true,
            Self::History(e) => e.is_cancelled(),
            _ => false,
        }
    }

    /// Build [`Error::InvalidPathComponent`].
    #[must_use]
    pub fn invalid_path_component(
        path: impl Into<String>,
        reason: crate::path_validate::PathComponentError,
    ) -> Self {
        Self::InvalidPathComponent {
            path: path.into(),
            reason,
        }
    }

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
            Error::WouldOverwrite { message, .. } => message.clone(),
            e if e.wants_fatal_prefix() => crate::diagnostics::fatal_line(&e.to_string()),
            other => other.to_string(),
        }
    }
}

impl From<crate::refs::store::RefStoreError> for Error {
    fn from(err: crate::refs::store::RefStoreError) -> Self {
        match err {
            crate::refs::store::RefStoreError::GitDirPathIo {
                path,
                operation,
                os_error,
            } => Error::GitDirPathIo {
                path,
                operation,
                source: crate::refs::store::RefStoreError::git_dir_io_source(os_error),
            },
            other => Error::RefStore(other),
        }
    }
}

impl From<crate::lockfile::LockError> for Error {
    /// [`LockError::Held`](crate::lockfile::LockError::Held) becomes [`Error::LockHeld`] with the
    /// recorded [`LockHolder`](crate::lockfile::LockHolder), and I/O failures become [`Error::Io`].
    #[cold]
    fn from(err: crate::lockfile::LockError) -> Self {
        use crate::lockfile::{LockError, LockHolder};
        match err {
            LockError::Held { path } => Self::LockHeld {
                holder: LockHolder::read(&path),
                path,
            },
            LockError::Io { source, .. } => Self::Io(source),
        }
    }
}

/// Convenience alias for `Result<T, Error>`.
pub type Result<T> = std::result::Result<T, Error>;
